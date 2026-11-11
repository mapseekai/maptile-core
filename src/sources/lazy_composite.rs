//! Lazily-resolved composite MVT source backing `vector_group` kinds.
//!
//! Holds member *source ids* instead of member sources: every `get_tile`
//! resolves the currently-registered members from the live [`SourceManager`]
//! and delegates to a short-lived [`CompositeSource`]. Members missing from
//! the registry are skipped (mosaic "any member available" semantics); a
//! member re-registered later is picked up on the next request with no group
//! re-registration.
//!
//! The advertised TileJSON is snapshotted once at `open` from the members
//! present at that moment. On the creation path members are READY (hence
//! registered) before the group exists, so the snapshot is complete; on cold
//! start the server's preload builds vector_group sources last.

use std::sync::Weak;

use tilejson::{TileJSON, tilejson};

use crate::error::{MaptileError, MaptileResult};
use crate::sources::composite::{CompositeChild, CompositeSource};
use crate::sources::manager::SourceManager;
use crate::sources::{
    TileCoord, TileEncoding, TileEtagResponse, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource, validate_tile_batch,
};

pub struct LazyCompositeSource {
    id: String,
    member_ids: Vec<String>,
    manager: Weak<SourceManager>,
    tj: TileJSON,
}

impl LazyCompositeSource {
    #[must_use]
    pub fn open(id: String, member_ids: Vec<String>, manager: Weak<SourceManager>) -> Self {
        let tj = match snapshot(&id, &member_ids, &manager) {
            Ok(c) => c.tilejson().clone(),
            Err(_) => tilejson! { tiles: vec![] },
        };
        Self {
            id,
            member_ids,
            manager,
            tj,
        }
    }
}

/// Resolve present members and build a short-lived [`CompositeSource`].
/// Returns `Err` when zero members are currently registered (or the
/// manager has been dropped).
fn snapshot(
    id: &str,
    member_ids: &[String],
    manager: &Weak<SourceManager>,
) -> MaptileResult<CompositeSource> {
    let mgr = manager
        .upgrade()
        .ok_or_else(|| MaptileError::Config("source manager dropped".into()))?;
    let children: Vec<CompositeChild> = member_ids
        .iter()
        .filter(|mid| mid.as_str() != id) // self-reference guard
        .filter_map(|mid| mgr.get(mid))
        .map(CompositeChild::new)
        .collect();
    if children.is_empty() && member_ids.iter().any(|id| mgr.is_unavailable(id)) {
        return Err(MaptileError::SourceUnavailable(id.into()));
    }
    CompositeSource::new(id, children)
}

#[async_trait::async_trait]
impl TileSource for LazyCompositeSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        }
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(Self {
            id: self.id.clone(),
            member_ids: self.member_ids.clone(),
            manager: self.manager.clone(),
            tj: self.tj.clone(),
        })
    }

    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        // Member registrations may change between key construction, rendering,
        // and ETag calculation. Preserve one snapshot per request instead of
        // caching this live group; member sources can still cache their tiles.
        crate::cache::CacheZoomRange::disabled()
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let composite =
            snapshot(&self.id, &self.member_ids, &self.manager).map_err(|error| match error {
                MaptileError::SourceUnavailable(_) => error,
                _ => MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y),
            })?;
        composite.get_tile(xyz, query).await
    }

    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        validate_tile_batch(coords)?;
        if coords.is_empty() {
            return Ok(Vec::new());
        }
        let composite = match snapshot(&self.id, &self.member_ids, &self.manager) {
            Ok(composite) => composite,
            Err(error @ MaptileError::SourceUnavailable(_)) => return Err(error),
            Err(_) => {
                return Ok(coords
                    .iter()
                    .map(|_| TileResponse {
                        data: bytes::Bytes::new(),
                        info: self.tile_info(),
                        last_modified: None,
                    })
                    .collect());
            }
        };
        composite.read_tile_batch(coords, query).await
    }

    async fn get_tile_with_etag(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
        if_none_match: Option<&str>,
    ) -> MaptileResult<TileEtagResponse> {
        let composite =
            snapshot(&self.id, &self.member_ids, &self.manager).map_err(|error| match error {
                MaptileError::SourceUnavailable(_) => error,
                _ => MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y),
            })?;
        composite
            .get_tile_with_etag(xyz, query, if_none_match)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::mvt::{self, proto};
    use std::sync::Arc;
    use tilejson::{TileJSON, VectorLayer, tilejson};

    // --- minimal MVT fake (mirrors composite.rs's test fake) ---

    struct MvtFake {
        id: String,
        tj: TileJSON,
        bytes: bytes::Bytes,
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
            })
        }
    }

    #[async_trait::async_trait]
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
                encoding: TileEncoding::Uncompressed,
            }
        }
        fn clone_source(&self) -> Box<dyn TileSource> {
            Box::new(MvtFake {
                id: self.id.clone(),
                tj: self.tj.clone(),
                bytes: self.bytes.clone(),
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

    // --- helpers ---

    fn manager_with(sources: Vec<Arc<dyn TileSource>>) -> Arc<SourceManager> {
        let mgr = Arc::new(SourceManager::new());
        for s in sources {
            mgr.register(s);
        }
        mgr
    }

    fn layer_names(resp: &TileResponse) -> Vec<String> {
        let raw = crate::transport::decode(&resp.data, resp.info.encoding).unwrap();
        let tile = mvt::decode(&raw).unwrap();
        tile.layers.iter().map(|l| l.name.clone()).collect()
    }

    // --- tests ---

    #[tokio::test]
    async fn merges_layers_of_present_members() {
        let a = MvtFake::new("vector.a", &["water"]);
        let b = MvtFake::new("vector.b", &["roads"]);
        let mgr = manager_with(vec![a, b]);

        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into(), "vector.b".into()],
            Arc::downgrade(&mgr),
        );
        let resp = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(layer_names(&resp), vec!["water", "roads"]);
    }

    #[tokio::test]
    async fn missing_member_is_skipped() {
        let a = MvtFake::new("vector.a", &["water"]);
        // vector.b is not registered
        let mgr = manager_with(vec![a]);

        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into(), "vector.b".into()],
            Arc::downgrade(&mgr),
        );
        let resp = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(layer_names(&resp), vec!["water"]);
    }

    #[tokio::test]
    async fn all_members_missing_is_tile_not_found() {
        let mgr = manager_with(vec![]);

        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into(), "vector.b".into()],
            Arc::downgrade(&mgr),
        );
        let err = group
            .get_tile(TileCoord { z: 1, x: 0, y: 0 }, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MaptileError::TileNotFound(1, 0, 0)),
            "expected TileNotFound(1,0,0), got: {err:?}"
        );
    }

    #[tokio::test]
    async fn member_registered_after_open_is_picked_up() {
        // Open with an empty manager — snapshot is empty TileJSON.
        let mgr = Arc::new(SourceManager::new());
        let group = LazyCompositeSource::open(
            "group.g".into(),
            vec!["vector.a".into()],
            Arc::downgrade(&mgr),
        );

        // Before registration: TileNotFound.
        let err = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap_err();
        assert!(matches!(err, MaptileError::TileNotFound(..)));

        // Register the member AFTER open — the lazy resolve picks it up.
        mgr.register(MvtFake::new("vector.a", &["water"]));

        let resp = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(layer_names(&resp), vec!["water"]);
    }

    #[tokio::test]
    async fn unavailable_member_preserves_failure_until_recovered_or_deleted() {
        let mgr = Arc::new(SourceManager::new());
        let group =
            LazyCompositeSource::open("group".into(), vec!["member".into()], Arc::downgrade(&mgr));
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        mgr.mark_unavailable("member");
        assert!(matches!(
            group.get_tile(xyz, None).await,
            Err(MaptileError::SourceUnavailable(..))
        ));
        assert!(matches!(
            group.get_tile_with_etag(xyz, None, None).await,
            Err(MaptileError::SourceUnavailable(..))
        ));
        assert!(matches!(
            group.read_tile_batch(&[xyz], None).await,
            Err(MaptileError::SourceUnavailable(..))
        ));
        mgr.register(MvtFake::new("member", &["water"]));
        assert_eq!(
            layer_names(&group.get_tile(xyz, None).await.unwrap()),
            vec!["water"]
        );
        mgr.mark_unavailable("member");
        mgr.remove("member");
        assert!(matches!(
            group.get_tile(xyz, None).await,
            Err(MaptileError::TileNotFound(..))
        ));
    }

    #[tokio::test]
    async fn batch_keeps_missing_members_empty_and_observes_later_registration() {
        let mgr = Arc::new(SourceManager::new());
        let group =
            LazyCompositeSource::open("group".into(), vec!["member".into()], Arc::downgrade(&mgr));
        let coords = [TileCoord { z: 0, x: 0, y: 0 }; 2];
        let empty = group.read_tile_batch(&coords, None).await.unwrap();
        assert_eq!(empty.len(), 2);
        assert!(empty.iter().all(|tile| tile.data.is_empty()));
        mgr.register(MvtFake::new("member", &["water"]));
        let batch = group.read_tile_batch(&coords, None).await.unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch.iter().all(|tile| layer_names(tile) == ["water"]));
    }

    #[tokio::test]
    async fn manager_preserves_live_members_and_composite_etag_with_hot_cache() {
        let cache = Arc::new(crate::cache::TileCache::new(1, None, None).unwrap());
        let mgr = Arc::new(SourceManager::new().with_cache(cache));
        mgr.register(MvtFake::new("a", &["water"]));
        let group = Arc::new(LazyCompositeSource::open(
            "group".into(),
            vec!["a".into()],
            Arc::downgrade(&mgr),
        ));
        mgr.register(group.clone());
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        let expected = snapshot("group", &["a".into()], &Arc::downgrade(&mgr))
            .unwrap()
            .get_tile_with_etag(xyz, None, None)
            .await
            .unwrap();
        let expected_etag = match expected {
            TileEtagResponse::Fresh { etag, .. } => etag,
            _ => unreachable!(),
        };
        let actual = mgr
            .get_tile_with_etag("group", xyz, None, None)
            .await
            .unwrap();
        match actual {
            TileEtagResponse::Fresh { resp, etag } => {
                assert_eq!(layer_names(&resp), vec!["water"]);
                assert_eq!(etag, expected_etag);
            }
            _ => unreachable!(),
        }
        mgr.register(MvtFake::new("a", &["roads"]));
        let actual = mgr
            .get_tile_with_etag("group", xyz, None, Some(&expected_etag))
            .await
            .unwrap();
        match actual {
            TileEtagResponse::Fresh { resp, etag } => {
                assert_eq!(layer_names(&resp), vec!["roads"]);
                assert_ne!(etag, expected_etag);
            }
            _ => panic!("new live members must not return a cached validator"),
        }
    }

    #[tokio::test]
    async fn self_reference_is_skipped() {
        // A group listing its own id as a member must not recurse.
        let mgr = Arc::new(SourceManager::new());
        let group = Arc::new(LazyCompositeSource::open(
            "group.g".into(),
            vec!["group.g".into()],
            Arc::downgrade(&mgr),
        ));
        // Register the group itself in the manager (worst case).
        mgr.register(group.clone() as Arc<dyn TileSource>);

        // get_tile must yield TileNotFound (0 non-self members resolved),
        // not recurse or panic.
        let err = group
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MaptileError::TileNotFound(..)),
            "expected TileNotFound for self-reference, got: {err:?}"
        );
    }
}
