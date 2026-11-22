//! Integration test: register a [`CompositeSource`] in [`SourceManager`] and
//! resolve a tile end-to-end through the public API.

use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::cache::StyleHash;
use maptile_core::error::{MaptileError, MaptileResult};
use maptile_core::sources::manager::SourceManager;
use maptile_core::sources::mvt::{self, proto};
use maptile_core::sources::{
    CompositeChild, CompositeSource, LazyCompositeSource, TileCoord, TileEncoding, TileFormat,
    TileInfo, TileQuery, TileResponse, TileSource,
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
    expected_style_hash: Option<StyleHash>,
    tile_reads: Arc<AtomicUsize>,
    batch_reads: Arc<AtomicUsize>,
    on_batch: Option<Arc<dyn Fn() + Send + Sync>>,
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
            expected_style_hash: None,
            tile_reads: Arc::new(AtomicUsize::new(0)),
            batch_reads: Arc::new(AtomicUsize::new(0)),
            on_batch: None,
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
            expected_style_hash: self.expected_style_hash,
            tile_reads: self.tile_reads.clone(),
            batch_reads: self.batch_reads.clone(),
            on_batch: self.on_batch.clone(),
        })
    }
    fn supports_raster_style(&self) -> bool {
        self.expected_style_hash.is_some()
    }
    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        self.batch_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(on_batch) = &self.on_batch {
            on_batch();
        }
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
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        self.tile_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(hash) = self.expected_style_hash {
            assert_eq!(query.and_then(|q| q.style_hash), Some(hash));
        }
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

#[tokio::test]
async fn legacy_composites_validate_style_against_their_resolved_children() {
    let manager = Arc::new(SourceManager::new());
    let hash = StyleHash::from_bytes([19; 32]);
    let query = Some(&TileQuery {
        style_hash: Some(hash),
        ..Default::default()
    });
    let coord = TileCoord { z: 0, x: 0, y: 0 };
    let mut styled = StaticMvt::new("styled", &["water"]);
    Arc::get_mut(&mut styled).unwrap().expected_style_hash = Some(hash);
    let fixed = CompositeSource::new("fixed", vec![CompositeChild::new(styled.clone())]).unwrap();
    let live = Arc::new(LazyCompositeSource::open(
        "live".into(),
        vec!["styled".into(), "raw".into()],
        Arc::downgrade(&manager),
    ));
    manager.register(styled);
    manager.register(live.clone());
    let fixed_tile = fixed.get_tile(coord, query).await.unwrap();
    let live_tile = manager.get_tile("live", coord, query).await.unwrap();
    assert_eq!(live_tile.data, fixed_tile.data);
    assert_eq!(
        live.read_tile_batch(&[coord], query).await.unwrap()[0].data,
        fixed_tile.data
    );
    assert!(live.read_tile_batch(&[], query).await.unwrap().is_empty());

    let raw = StaticMvt::new("raw", &["roads"]);
    manager.register(raw.clone());
    assert!(matches!(
        manager.get_tile("live", coord, query).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        live.get_tile_with_etag(coord, query, None).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        live.read_tile_batch(&[coord], query).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        live.read_tile_batch(&[], query).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert_eq!(raw.batch_reads.load(Ordering::SeqCst), 0);

    manager.remove("raw");
    manager.remove("styled");
    assert!(matches!(
        live.get_tile(coord, None).await,
        Err(MaptileError::TileNotFound(..))
    ));
    assert!(matches!(
        live.get_tile(coord, query).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        live.get_tile_with_etag(coord, query, None).await,
        Err(MaptileError::Unsupported(_))
    ));
    for coords in [&[][..], &[coord]] {
        assert!(matches!(
            live.read_tile_batch(coords, query).await,
            Err(MaptileError::Unsupported(_))
        ));
    }
}

#[tokio::test]
async fn nested_live_style_members_are_pinned_before_any_child_reads() {
    let manager = Arc::new(SourceManager::new());
    let hash = StyleHash::from_bytes([3; 32]);
    let query = Some(&TileQuery {
        style_hash: Some(hash),
        ..Default::default()
    });
    let coord = TileCoord { z: 0, x: 0, y: 0 };
    let raw = StaticMvt::new("replaceable", &["raw"]);
    let mut original = StaticMvt::new("replaceable", &["original"]);
    Arc::get_mut(&mut original).unwrap().expected_style_hash = Some(hash);
    manager.register(original.clone());
    manager.register(Arc::new(LazyCompositeSource::open(
        "nested".into(),
        vec!["replaceable".into()],
        Arc::downgrade(&manager),
    )));

    let mut first = StaticMvt::new("first", &["first"]);
    let first_source = Arc::get_mut(&mut first).unwrap();
    first_source.expected_style_hash = Some(hash);
    let nested_manager = Arc::downgrade(&manager);
    let replacement = raw.clone();
    first_source.on_batch = Some(Arc::new(move || {
        nested_manager
            .upgrade()
            .unwrap()
            .register(replacement.clone());
    }));
    manager.register(first.clone());
    let live = Arc::new(LazyCompositeSource::open(
        "live".into(),
        vec!["first".into(), "nested".into()],
        Arc::downgrade(&manager),
    ));
    manager.register(live.clone());

    let page = live.read_tile_batch(&[coord], query).await.unwrap();
    let mvt = mvt::decode(&page[0].data).unwrap();
    assert_eq!(
        mvt.layers
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "original"]
    );
    assert!(Arc::ptr_eq(
        &manager.require("replaceable").unwrap(),
        &(raw.clone() as Arc<dyn TileSource>)
    ));
    assert_eq!(original.batch_reads.load(Ordering::SeqCst), 1);
    assert_eq!(raw.batch_reads.load(Ordering::SeqCst), 0);

    assert!(matches!(
        manager.get_tile("live", coord, query).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        live.get_tile_with_etag(coord, query, None).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        live.read_tile_batch(&[coord], query).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert_eq!(first.tile_reads.load(Ordering::SeqCst), 0);
    assert_eq!(first.batch_reads.load(Ordering::SeqCst), 1);
    manager.remove("replaceable");
    assert_eq!(
        manager.get_tile("live", coord, query).await.unwrap().data,
        first.bytes
    );
}

#[tokio::test]
async fn catalog_rejects_style_on_live_legacy_sources_before_a_short_circuit_layer() {
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        service::{
            ReadTarget, TileLayer, TileOutcome, TileService, TileStack, ValidatedTileRequest,
        },
    };

    struct ShortCircuit(Arc<AtomicUsize>);
    #[async_trait]
    impl TileLayer for ShortCircuit {
        async fn read(
            &self,
            _: &ValidatedTileRequest<'_>,
            _: TileService<'_>,
        ) -> MaptileResult<TileOutcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(TileOutcome::NoTile)
        }
    }

    let manager = Arc::new(SourceManager::new());
    manager.register(StaticMvt::new("member", &["raw"]));
    let live = Arc::new(LazyCompositeSource::open(
        "live".into(),
        vec!["member".into()],
        Arc::downgrade(&manager),
    ));
    let fixed = CompositeSource::new("fixed", vec![CompositeChild::new(live.clone())]).unwrap();
    let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut update = engine.catalog().update();
    update.put(live.clone()).unwrap();
    update.put(Arc::new(fixed)).unwrap();
    engine.catalog().publish(update).unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let stack = TileStack::builder(engine)
        .layer(ShortCircuit(reads.clone()))
        .build();
    let hash = StyleHash::from_bytes([5; 32]);
    let query = TileQuery {
        style_hash: Some(hash),
        ..Default::default()
    };
    let coord = TileCoord { z: 0, x: 0, y: 0 };
    for id in ["live", "fixed"] {
        let target = ReadTarget {
            query: Some(&query),
            ..ReadTarget::new(id, "test")
        };
        assert!(matches!(
            stack.read(target.at(coord)).await,
            Err(MaptileError::Unsupported(_))
        ));
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let mut styled = StaticMvt::new("member", &["styled"]);
    Arc::get_mut(&mut styled).unwrap().expected_style_hash = Some(hash);
    manager.register(styled);
    assert!(live.get_tile(coord, Some(&query)).await.is_ok());
    let target = ReadTarget {
        query: Some(&query),
        ..ReadTarget::new("live", "test")
    };
    assert!(matches!(
        stack.read(target.at(coord)).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        stack.read(ReadTarget::new("live", "test").at(coord)).await,
        Ok(TileOutcome::NoTile)
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}
