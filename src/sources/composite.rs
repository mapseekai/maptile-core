//! Multi-source MVT tile composition (Martin's "composite source").
//!
//! Combines several upstream `TileSource`s into a single virtual source: at
//! request time, fetch each child's tile concurrently, decode the MVT layers,
//! merge them (optionally prefixing layer names per-child to avoid collisions)
//! and re-encode the result.
//!
//! Constraints:
//! - All children must serve `TileFormat::Mvt`. Mixing PMTiles + MBTiles +
//!   Postgres functions is fine — the format is what matters.
//! - Children's wire encoding (gzip/none/zstd/...) is handled transparently
//!   via [`crate::transport::decode`]. The composite re-emits the bytes
//!   uncompressed by default; the caller (or HTTP layer) can re-compress.
//! - `TileQuery` is forwarded to every child that advertises support; others
//!   ignore it. ETags are derived from the children, so query-bearing requests
//!   bypass the standard cache via [`crate::sources::manager::SourceManager`].

use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource, mvt,
    read_tile_batch_with,
};
use crate::transport;
use bytes::Bytes;
use std::sync::Arc;
use std::time::SystemTime;
use tilejson::{TileJSON, VectorLayer};

/// One child of a [`CompositeSource`].
#[derive(Clone)]
pub struct CompositeChild {
    pub source: Arc<dyn TileSource>,
    /// Optional prefix prepended to each of this child's layer names. Useful
    /// when multiple children expose the same logical layer name (e.g. two
    /// PMTiles archives both having a `water` layer).
    pub layer_prefix: String,
}

impl CompositeChild {
    #[must_use]
    pub fn new(source: Arc<dyn TileSource>) -> Self {
        Self {
            source,
            layer_prefix: String::new(),
        }
    }

    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.layer_prefix = prefix.into();
        self
    }
}

/// A virtual source that fans tile requests out to several MVT children and
/// merges the resulting layers.
pub struct CompositeSource {
    id: String,
    children: Vec<CompositeChild>,
    tilejson: TileJSON,
}

impl CompositeSource {
    /// Build a composite. Returns `MaptileError::Config` if the children list
    /// is empty or any child does not produce MVT.
    pub fn new(id: impl Into<String>, children: Vec<CompositeChild>) -> MaptileResult<Self> {
        if children.is_empty() {
            return Err(MaptileError::Config(
                "composite source needs at least one child".into(),
            ));
        }
        for c in &children {
            if c.source.tile_info().format != TileFormat::Mvt {
                return Err(MaptileError::Config(format!(
                    "composite source: child {:?} format must be Mvt",
                    c.source.id()
                )));
            }
        }
        let id = id.into();
        let tilejson = build_tilejson(&id, &children);
        Ok(Self {
            id,
            children,
            tilejson,
        })
    }

    /// Read-only access to the children, in order.
    pub fn children(&self) -> &[CompositeChild] {
        &self.children
    }
}

#[async_trait::async_trait]
impl TileSource for CompositeSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn content_version(&self) -> crate::cache::ContentVersion {
        let mut hash = blake3::Hasher::new();
        for child in &self.children {
            let id = child.source.id().as_bytes();
            hash.update(&(id.len() as u64).to_le_bytes());
            hash.update(id);
            hash.update(child.source.content_version().to_hex().as_bytes());
            hash.update(&(child.layer_prefix.len() as u64).to_le_bytes());
            hash.update(child.layer_prefix.as_bytes());
        }
        crate::cache::ContentVersion::from_bytes(*hash.finalize().as_bytes())
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tilejson
    }

    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        }
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(CompositeSource {
            id: self.id.clone(),
            children: self.children.clone(),
            tilejson: self.tilejson.clone(),
        })
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        // Fetch all children concurrently — composite latency is the slowest
        // child, not the sum.
        let mut futs = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let q = if child.source.supports_url_query() || child.source.supports_filter() {
                query
            } else {
                None
            };
            futs.push(child.source.get_tile(xyz, q));
        }

        let results = futures_util::future::join_all(futs).await;

        self.merge_tiles(xyz, results)
    }

    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        read_tile_batch_with(coords, self.tile_info(), |coord| async move {
            let mut results = Vec::with_capacity(self.children.len());
            for child in &self.children {
                let q = if child.source.supports_url_query() || child.source.supports_filter() {
                    query
                } else {
                    None
                };
                let result = match child.source.read_tile_batch(&[coord], q).await {
                    Ok(mut tiles) if tiles.len() == 1 => Ok(tiles.remove(0)),
                    Ok(_) => Err(MaptileError::InvalidMetadata(
                        "composite child returned an invalid batch length".into(),
                    )),
                    Err(error) => Err(error),
                };
                results.push(result);
            }
            self.merge_tiles(coord, results)
        })
        .await
    }

    fn tile_etag(&self, response: &TileResponse) -> String {
        composite_etag(&self.children, response)
    }

    fn supports_url_query(&self) -> bool {
        self.children.iter().any(|c| c.source.supports_url_query())
    }

    fn supports_filter(&self) -> bool {
        self.children.iter().any(|c| c.source.supports_filter())
    }
}

impl CompositeSource {
    fn merge_tiles(
        &self,
        xyz: TileCoord,
        results: Vec<MaptileResult<TileResponse>>,
    ) -> MaptileResult<TileResponse> {
        let mut tiles: Vec<mvt::proto::Tile> = Vec::with_capacity(self.children.len());
        let mut prefixes: Vec<&str> = Vec::with_capacity(self.children.len());
        let mut last_modified: Option<SystemTime> = None;

        for (child, res) in self.children.iter().zip(results) {
            match res {
                Ok(resp) => {
                    last_modified = max_opt(last_modified, resp.last_modified);
                    let raw = transport::decode(&resp.data, resp.info.encoding)?;
                    if raw.is_empty() {
                        continue;
                    }
                    let tile = mvt::decode(&raw)?;
                    tiles.push(tile);
                    prefixes.push(child.layer_prefix.as_str());
                }
                Err(MaptileError::TileNotFound(..)) => continue,
                Err(e) => return Err(e),
            }
        }

        if tiles.is_empty() {
            return Err(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y));
        }

        let merged = mvt::merge(tiles, &prefixes);
        let bytes: Bytes = mvt::encode(&merged);

        Ok(TileResponse {
            data: bytes,
            info: self.tile_info(),
            last_modified,
        })
    }
}

fn max_opt(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (None, b) => b,
        (a, None) => a,
    }
}

fn composite_etag(children: &[CompositeChild], resp: &TileResponse) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(b"composite\0");
    for c in children {
        h.update(c.source.id().as_bytes());
        h.update(b"\0");
        h.update(c.layer_prefix.as_bytes());
        h.update(b"\0");
    }
    h.update(b"|");
    h.update(&resp.data);
    let digest = h.finalize();
    let mut s = String::with_capacity(2 + digest.len() * 2);
    s.push('"');
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s.push('"');
    s
}

/// Build the merged TileJSON: union of vector_layers (with prefixes applied),
/// `minzoom = max(children)`, `maxzoom = min(children)`, bounds union.
fn build_tilejson(id: &str, children: &[CompositeChild]) -> TileJSON {
    let mut tj = tilejson::tilejson! { tiles: vec![] };
    tj.name = Some(format!("composite:{id}"));

    let mut min_z: Option<u8> = None;
    let mut max_z: Option<u8> = None;
    let mut bounds: Option<tilejson::Bounds> = None;
    let mut all_layers: Vec<VectorLayer> = Vec::new();

    for child in children {
        let child_tj = child.source.tilejson();
        if let Some(z) = child_tj.minzoom {
            min_z = Some(min_z.map_or(z, |cur| cur.max(z)));
        }
        if let Some(z) = child_tj.maxzoom {
            max_z = Some(max_z.map_or(z, |cur| cur.min(z)));
        }
        if let Some(b) = child_tj.bounds {
            bounds = Some(bounds.map_or(b, |cur| union_bounds(cur, b)));
        }

        if let Some(vls) = &child_tj.vector_layers {
            for vl in vls {
                let mut new = vl.clone();
                if !child.layer_prefix.is_empty() {
                    new.id = format!("{}{}", child.layer_prefix, vl.id);
                }
                all_layers.push(new);
            }
        }
    }

    tj.minzoom = min_z;
    tj.maxzoom = max_z;
    tj.bounds = bounds;
    if !all_layers.is_empty() {
        tj.vector_layers = Some(all_layers);
    }
    tj
}

fn union_bounds(a: tilejson::Bounds, b: tilejson::Bounds) -> tilejson::Bounds {
    tilejson::Bounds::new(
        a.left.min(b.left),
        a.bottom.min(b.bottom),
        a.right.max(b.right),
        a.top.max(b.top),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::TileEtagResponse;
    use crate::sources::mvt::proto;
    use async_trait::async_trait;
    use tilejson::tilejson;

    struct MvtFake {
        id: String,
        tj: TileJSON,
        bytes: Bytes,
        encoding: TileEncoding,
    }

    impl MvtFake {
        fn new(id: &str, layer_names: &[&str]) -> Arc<Self> {
            let layers: Vec<proto::Layer> = layer_names
                .iter()
                .map(|n| proto::Layer {
                    version: 2,
                    name: (*n).into(),
                    features: vec![],
                    keys: vec![],
                    values: vec![],
                    extent: Some(4096),
                })
                .collect();
            let bytes = mvt::encode(&proto::Tile { layers });
            let mut tj = tilejson! { tiles: vec![] };
            tj.minzoom = Some(0);
            tj.maxzoom = Some(14);
            tj.vector_layers = Some(
                layer_names
                    .iter()
                    .map(|n| VectorLayer::new((*n).into(), Default::default()))
                    .collect(),
            );
            Arc::new(Self {
                id: id.into(),
                tj,
                bytes,
                encoding: TileEncoding::Uncompressed,
            })
        }
    }

    #[async_trait]
    impl TileSource for MvtFake {
        fn id(&self) -> &str {
            &self.id
        }
        fn tilejson(&self) -> &TileJSON {
            &self.tj
        }
        fn tile_info(&self) -> TileInfo {
            TileInfo {
                format: TileFormat::Mvt,
                encoding: self.encoding,
            }
        }
        fn clone_source(&self) -> Box<dyn TileSource> {
            Box::new(MvtFake {
                id: self.id.clone(),
                tj: self.tj.clone(),
                bytes: self.bytes.clone(),
                encoding: self.encoding,
            })
        }
        async fn get_tile(
            &self,
            _xyz: TileCoord,
            _query: Option<&TileQuery<'_>>,
        ) -> MaptileResult<TileResponse> {
            Ok(TileResponse {
                data: self.bytes.clone(),
                info: self.tile_info(),
                last_modified: None,
            })
        }
    }

    #[test]
    fn content_version_includes_child_membership_and_prefix() {
        let a = MvtFake::new("a", &["water"]);
        let b = MvtFake::new("b", &["roads"]);
        let plain = CompositeSource::new("same", vec![CompositeChild::new(a.clone())]).unwrap();
        let prefixed =
            CompositeSource::new("same", vec![CompositeChild::new(a).with_prefix("a_")]).unwrap();
        let different = CompositeSource::new("same", vec![CompositeChild::new(b)]).unwrap();
        assert_ne!(plain.content_version(), prefixed.content_version());
        assert_ne!(plain.content_version(), different.content_version());
    }

    #[tokio::test]
    async fn empty_children_rejected() {
        let res = CompositeSource::new("c", vec![]);
        assert!(matches!(res, Err(MaptileError::Config(_))));
    }

    #[tokio::test]
    async fn merges_layers_from_two_children() {
        let a = MvtFake::new("a", &["water"]);
        let b = MvtFake::new("b", &["roads"]);
        let c = CompositeSource::new("c", vec![CompositeChild::new(a), CompositeChild::new(b)])
            .unwrap();
        let resp = c
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        let raw = transport::decode(&resp.data, resp.info.encoding).unwrap();
        let tile = mvt::decode(&raw).unwrap();
        let names: Vec<_> = tile.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["water", "roads"]);
    }

    #[tokio::test]
    async fn collision_disambiguated_by_prefix() {
        let a = MvtFake::new("a", &["places"]);
        let b = MvtFake::new("b", &["places"]);
        let c = CompositeSource::new(
            "c",
            vec![
                CompositeChild::new(a).with_prefix("a_"),
                CompositeChild::new(b).with_prefix("b_"),
            ],
        )
        .unwrap();
        let resp = c
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        let raw = transport::decode(&resp.data, resp.info.encoding).unwrap();
        let tile = mvt::decode(&raw).unwrap();
        let names: Vec<_> = tile.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["a_places", "b_places"]);
    }

    #[tokio::test]
    async fn tilejson_min_max_zoom_intersect_and_union() {
        let a_src = MvtFake::new("a", &["x"]);
        // override zoom range
        let mut a_tj = a_src.tj.clone();
        a_tj.minzoom = Some(2);
        a_tj.maxzoom = Some(10);
        let a = Arc::new(MvtFake {
            id: "a".into(),
            tj: a_tj,
            bytes: a_src.bytes.clone(),
            encoding: TileEncoding::Uncompressed,
        });
        let b_src = MvtFake::new("b", &["y"]);
        let mut b_tj = b_src.tj.clone();
        b_tj.minzoom = Some(5);
        b_tj.maxzoom = Some(14);
        let b = Arc::new(MvtFake {
            id: "b".into(),
            tj: b_tj,
            bytes: b_src.bytes.clone(),
            encoding: TileEncoding::Uncompressed,
        });
        let c = CompositeSource::new("c", vec![CompositeChild::new(a), CompositeChild::new(b)])
            .unwrap();
        let tj = c.tilejson();
        assert_eq!(tj.minzoom, Some(5)); // max of children
        assert_eq!(tj.maxzoom, Some(10)); // min of children
        assert_eq!(
            tj.vector_layers.as_ref().unwrap().len(),
            2 // x + y
        );
    }

    #[tokio::test]
    async fn rejects_non_mvt_child() {
        struct PngFake;
        #[async_trait]
        impl TileSource for PngFake {
            fn id(&self) -> &str {
                "p"
            }
            fn tilejson(&self) -> &TileJSON {
                static TJ: std::sync::OnceLock<TileJSON> = std::sync::OnceLock::new();
                TJ.get_or_init(|| tilejson! { tiles: vec![] })
            }
            fn tile_info(&self) -> TileInfo {
                TileInfo {
                    format: TileFormat::Png,
                    encoding: TileEncoding::Internal,
                }
            }
            fn clone_source(&self) -> Box<dyn TileSource> {
                Box::new(PngFake)
            }
            async fn get_tile(
                &self,
                _xyz: TileCoord,
                _query: Option<&TileQuery<'_>>,
            ) -> MaptileResult<TileResponse> {
                unreachable!()
            }
        }
        let res = CompositeSource::new("c", vec![CompositeChild::new(Arc::new(PngFake))]);
        assert!(matches!(res, Err(MaptileError::Config(_))));
    }

    #[tokio::test]
    async fn handles_gzipped_child_via_transport() {
        let layer = proto::Layer {
            version: 2,
            name: "water".into(),
            features: vec![],
            keys: vec![],
            values: vec![],
            extent: Some(4096),
        };
        let raw = mvt::encode(&proto::Tile {
            layers: vec![layer],
        });
        let gz = transport::encode_as(&raw, TileEncoding::Gzip).unwrap();

        let mut tj = tilejson! { tiles: vec![] };
        tj.minzoom = Some(0);
        tj.maxzoom = Some(14);
        let gz_src = Arc::new(MvtFake {
            id: "gz".into(),
            tj,
            bytes: gz,
            encoding: TileEncoding::Gzip,
        });
        let c = CompositeSource::new("c", vec![CompositeChild::new(gz_src)]).unwrap();
        let resp = c
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        // Composite emits uncompressed bytes by default.
        assert_eq!(resp.info.encoding, TileEncoding::Uncompressed);
        let tile = mvt::decode(&resp.data).unwrap();
        assert_eq!(tile.layers.len(), 1);
    }

    #[tokio::test]
    async fn etag_changes_when_underlying_bytes_change() {
        let a = MvtFake::new("a", &["water"]);
        let c1 = CompositeSource::new("c", vec![CompositeChild::new(a.clone())]).unwrap();
        let r1 = c1
            .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, None)
            .await
            .unwrap();
        let etag1 = match r1 {
            TileEtagResponse::Fresh { etag, .. } => etag,
            _ => panic!("expected Fresh"),
        };
        let r2 = c1
            .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, Some(&etag1))
            .await
            .unwrap();
        assert!(matches!(r2, TileEtagResponse::NotModified));
    }
}
