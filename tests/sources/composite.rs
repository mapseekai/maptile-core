//! Integration test: register a [`CompositeSource`] in [`SourceManager`] and
//! resolve a tile end-to-end through the public API.

use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::error::MaptileResult;
use maptile_core::sources::manager::SourceManager;
use maptile_core::sources::mvt::{self, proto};
use maptile_core::sources::{
    CompositeChild, CompositeSource, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery,
    TileResponse, TileSource,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tilejson::{TileJSON, VectorLayer, tilejson};

struct StaticMvt {
    id: String,
    tj: TileJSON,
    bytes: Bytes,
    batch_reads: Arc<AtomicUsize>,
}

impl StaticMvt {
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
            batch_reads: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl TileSource for StaticMvt {
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
        Box::new(StaticMvt {
            id: self.id.clone(),
            tj: self.tj.clone(),
            bytes: self.bytes.clone(),
            batch_reads: self.batch_reads.clone(),
        })
    }
    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        self.batch_reads.fetch_add(1, Ordering::SeqCst);
        Ok(coords
            .iter()
            .map(|_| TileResponse {
                data: self.bytes.clone(),
                info: self.tile_info(),
                last_modified: None,
            })
            .collect())
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

#[tokio::test]
async fn composite_via_source_manager_returns_merged_layers() {
    let mgr = SourceManager::new();
    let a = StaticMvt::new("a", &["water"]);
    let b = StaticMvt::new("b", &["roads"]);
    mgr.register(a.clone());
    mgr.register(b.clone());

    let composite = Arc::new(
        CompositeSource::new(
            "world",
            vec![CompositeChild::new(a), CompositeChild::new(b)],
        )
        .unwrap(),
    );
    mgr.register(composite);

    let resp = mgr
        .get_tile("world", TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    let tile = mvt::decode(&resp.data).unwrap();
    let names: Vec<_> = tile.layers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, vec!["water", "roads"]);
}

#[tokio::test]
async fn composite_tilejson_is_visible_in_manager_snapshot() {
    let mgr = SourceManager::new();
    let a = StaticMvt::new("a", &["water"]);
    let composite = Arc::new(CompositeSource::new("world", vec![CompositeChild::new(a)]).unwrap());
    mgr.register(composite);

    let snapshots = mgr.snapshot_tilejson();
    assert_eq!(snapshots.len(), 1);
    let tj = &snapshots[0];
    assert_eq!(tj.name.as_deref(), Some("composite:world"));
    assert_eq!(tj.vector_layers.as_ref().unwrap().len(), 1);
}

#[tokio::test]
async fn batch_delegates_to_children_through_static_and_lazy_composites() {
    use maptile_core::sources::LazyCompositeSource;
    let manager = Arc::new(SourceManager::new());
    let a = StaticMvt::new("a", &["water"]);
    let b = StaticMvt::new("b", &["roads"]);
    manager.register(a.clone());
    manager.register(b.clone());
    let composite = CompositeSource::new(
        "static",
        vec![
            CompositeChild::new(a.clone()).with_prefix("a_"),
            CompositeChild::new(b.clone()).with_prefix("b_"),
        ],
    )
    .unwrap();
    let coords = [TileCoord { z: 0, x: 0, y: 0 }; 2];
    let single = composite.get_tile(coords[0], None).await.unwrap();
    let batch = composite.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().all(|tile| tile.data == single.data));
    assert_eq!(a.batch_reads.load(Ordering::SeqCst), 2);
    assert_eq!(b.batch_reads.load(Ordering::SeqCst), 2);
    let lazy = LazyCompositeSource::open(
        "group".into(),
        vec!["a".into(), "b".into()],
        Arc::downgrade(&manager),
    );
    let batch = lazy.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(a.batch_reads.load(Ordering::SeqCst), 4);
    assert_eq!(b.batch_reads.load(Ordering::SeqCst), 4);
    for tile in batch {
        assert_eq!(
            mvt::decode(&tile.data)
                .unwrap()
                .layers
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            ["water", "roads"]
        );
    }
}
