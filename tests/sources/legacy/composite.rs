//! Integration tests for fixed and live MVT composites.

use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::cache::{ContentVersion, StyleHash, TileCache};
use maptile_core::error::{MaptileError, MaptileResult};
use maptile_core::sources::manager::SourceManager;
use maptile_core::sources::mvt::{self, proto};
use maptile_core::sources::{
    CompositeChild, CompositeSource, LazyCompositeSource, MosaicPixelSelection, TileCoord,
    TileEncoding, TileEtagResponse, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tilejson::{TileJSON, VectorLayer, tilejson};

fn mvt_fixture(layer_names: &[&str]) -> (Bytes, TileJSON) {
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
    (bytes, tj)
}

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
        let (bytes, tj) = mvt_fixture(layer_names);
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

#[derive(Clone)]
struct SequentialMvt {
    id: String,
    tj: TileJSON,
    bytes: Bytes,
    format: TileFormat,
    calls: Arc<AtomicUsize>,
    on_tile_info: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl SequentialMvt {
    fn new(id: &str, layer_names: &[&str]) -> Arc<Self> {
        let (bytes, tj) = mvt_fixture(layer_names);
        Arc::new(Self {
            id: id.into(),
            tj,
            bytes,
            format: TileFormat::Mvt,
            calls: Arc::new(AtomicUsize::new(0)),
            on_tile_info: None,
        })
    }
}

#[async_trait]
impl TileSource for SequentialMvt {
    fn id(&self) -> &str {
        &self.id
    }
    fn content_version(&self) -> ContentVersion {
        ContentVersion::from_input(&self.bytes)
    }
    fn supports_filter(&self) -> bool {
        true
    }
    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }
    fn tile_info(&self) -> TileInfo {
        if let Some(on_tile_info) = &self.on_tile_info {
            on_tile_info();
        }
        TileInfo {
            format: self.format,
            encoding: TileEncoding::Uncompressed,
        }
    }
    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }
    async fn get_tile(
        &self,
        _xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        Ok(TileResponse {
            data: self.bytes.clone(),
            info: self.tile_info(),
            last_modified: None,
        })
    }
}

fn manager_with(sources: Vec<Arc<dyn TileSource>>) -> Arc<SourceManager> {
    let mgr =
        Arc::new(SourceManager::new().with_cache(Arc::new(TileCache::new(1, None, None).unwrap())));
    for s in sources {
        mgr.register(s);
    }
    mgr
}

fn layer_names(resp: &TileResponse) -> Vec<String> {
    let raw = maptile_core::transport::decode(&resp.data, resp.info.encoding).unwrap();
    let tile = mvt::decode(&raw).unwrap();
    tile.layers.iter().map(|l| l.name.clone()).collect()
}

#[tokio::test]
async fn indirect_cycles_fail_all_read_paths_and_recovery_is_possible() {
    let manager = manager_with(vec![]);
    let group = Arc::new(LazyCompositeSource::open(
        "a".into(),
        vec!["b".into()],
        Arc::downgrade(&manager),
    ));
    manager.register(group.clone());
    manager.register(Arc::new(LazyCompositeSource::open(
        "b".into(),
        vec!["a".into()],
        Arc::downgrade(&manager),
    )));
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    assert!(matches!(
        group.get_tile(xyz, None).await,
        Err(MaptileError::Config(_))
    ));
    assert!(matches!(
        group.read_tile_batch(&[xyz], None).await,
        Err(MaptileError::Config(_))
    ));
    assert!(matches!(
        group.get_tile_with_etag(xyz, None, None).await,
        Err(MaptileError::Config(_))
    ));
    manager.register(SequentialMvt::new("b", &["recovered"]));
    assert_eq!(
        layer_names(&group.get_tile(xyz, None).await.unwrap()),
        ["recovered"]
    );
}

#[tokio::test]
async fn shared_nested_groups_are_valid_across_concurrent_branches() {
    let manager = manager_with(vec![SequentialMvt::new("leaf", &["roads"])]);
    for (id, members) in [
        ("shared", vec!["leaf"]),
        ("left", vec!["shared"]),
        ("right", vec!["shared"]),
        ("root", vec!["left", "right"]),
    ] {
        manager.register(Arc::new(LazyCompositeSource::open(
            id.into(),
            members.into_iter().map(str::to_owned).collect(),
            Arc::downgrade(&manager),
        )));
    }
    let root = manager.require("root").unwrap();
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    let (first, second) = tokio::join!(
        root.get_tile(xyz, None),
        root.get_tile_with_etag(xyz, None, None)
    );
    assert!(!first.unwrap().data.is_empty());
    assert!(second.is_ok());
    assert!(
        !root.read_tile_batch(&[xyz], None).await.unwrap()[0]
            .data
            .is_empty()
    );
}

#[tokio::test]
async fn replacement_can_reach_an_older_instance_with_the_same_id() {
    let manager = manager_with(vec![SequentialMvt::new("leaf", &["roads"])]);
    manager.register(Arc::new(LazyCompositeSource::open(
        "a".into(),
        vec!["leaf".into()],
        Arc::downgrade(&manager),
    )));
    manager.register_composite_from_ids("b", &["a"]).unwrap();
    manager.register(Arc::new(LazyCompositeSource::open(
        "a".into(),
        vec!["b".into()],
        Arc::downgrade(&manager),
    )));
    let root = manager.require("a").unwrap();
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    assert_eq!(
        layer_names(&root.get_tile(xyz, None).await.unwrap()),
        ["roads"]
    );
    assert!(root.get_tile_with_etag(xyz, None, None).await.is_ok());
    assert_eq!(
        layer_names(&root.read_tile_batch(&[xyz], None).await.unwrap()[0]),
        ["roads"]
    );
    manager.register(Arc::new(LazyCompositeSource::open(
        "leaf".into(),
        vec!["a".into()],
        Arc::downgrade(&manager),
    )));
    assert!(matches!(
        root.get_tile(xyz, None).await,
        Err(MaptileError::Config(_))
    ));
}

#[tokio::test]
async fn excessive_nesting_returns_an_error_before_stack_exhaustion() {
    let manager = manager_with(vec![SequentialMvt::new("leaf", &["roads"])]);
    let mut child = "leaf".to_owned();
    for n in 0..65 {
        let id = format!("group_{n}");
        manager.register(Arc::new(LazyCompositeSource::open(
            id.clone(),
            vec![child],
            Arc::downgrade(&manager),
        )));
        child = id;
    }
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    let at_limit = manager.require("group_63").unwrap();
    assert!(at_limit.get_tile(xyz, None).await.is_ok());
    assert!(at_limit.get_tile_with_etag(xyz, None, None).await.is_ok());
    assert!(at_limit.read_tile_batch(&[xyz], None).await.is_ok());
    assert!(matches!(
        manager.require(&child).unwrap().get_tile(xyz, None).await,
        Err(MaptileError::Config(_))
    ));
}

#[tokio::test]
async fn merges_layers_of_present_members() {
    let a = SequentialMvt::new("vector.a", &["water"]);
    let b = SequentialMvt::new("vector.b", &["roads"]);
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
    let a = SequentialMvt::new("vector.a", &["water"]);
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
async fn configuration_and_lifecycle_errors_are_not_missing_tiles() {
    for dropped_manager in [false, true] {
        let mut source = SequentialMvt::new("member", &["water"]);
        if !dropped_manager {
            Arc::get_mut(&mut source).unwrap().format = TileFormat::Png;
        }
        let manager = manager_with(vec![source]);
        let group = LazyCompositeSource::open(
            "group".into(),
            vec!["member".into()],
            Arc::downgrade(&manager),
        );
        if dropped_manager {
            drop(manager);
        }
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        let expected = if dropped_manager {
            "manager dropped"
        } else {
            "must be Mvt"
        };
        for error in [
            group.get_tile(xyz, None).await.unwrap_err(),
            group.get_tile_with_etag(xyz, None, None).await.unwrap_err(),
            group.read_tile_batch(&[xyz], None).await.unwrap_err(),
        ] {
            assert!(
                matches!(error, MaptileError::Config(ref message) if message.contains(expected)),
                "expected configuration/lifecycle error, got {error:?}"
            );
        }
    }
}

#[tokio::test]
async fn manager_rejects_mosaic_selection_for_live_group_etag_reads() {
    let manager = manager_with(vec![SequentialMvt::new("member", &["water"])]);
    manager.register(Arc::new(LazyCompositeSource::open(
        "group".into(),
        vec!["member".into()],
        Arc::downgrade(&manager),
    )));
    let query = TileQuery {
        pixel_selection: Some(MosaicPixelSelection::Median),
        ..Default::default()
    };
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    assert!(matches!(
        manager.get_tile("group", xyz, Some(&query)).await,
        Err(MaptileError::Unsupported(_))
    ));
    assert!(matches!(
        manager
            .get_tile_with_etag("group", xyz, Some(&query), None)
            .await,
        Err(MaptileError::Unsupported(_))
    ));
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
    mgr.register(SequentialMvt::new("vector.a", &["water"]));

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
    mgr.register(SequentialMvt::new("member", &["water"]));
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
    mgr.register(SequentialMvt::new("member", &["water"]));
    let batch = group.read_tile_batch(&coords, None).await.unwrap();
    assert_eq!(batch.len(), 2);
    assert!(batch.iter().all(|tile| layer_names(tile) == ["water"]));
}

#[tokio::test]
async fn live_members_share_manager_cache_but_queries_and_batches_bypass_it() {
    let member = SequentialMvt::new("member", &["roads"]);
    let mgr = manager_with(vec![member.clone()]);
    let group = Arc::new(LazyCompositeSource::open(
        "group".into(),
        vec!["member".into()],
        Arc::downgrade(&mgr),
    ));
    mgr.register(group.clone());
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    for _ in 0..2 {
        mgr.get_tile("member", xyz, None).await.unwrap();
        mgr.get_tile("group", xyz, None).await.unwrap();
    }
    mgr.get_tile("group", xyz, Some(&TileQuery::default()))
        .await
        .unwrap();
    let TileEtagResponse::Fresh { etag, .. } = mgr
        .get_tile_with_etag("group", xyz, None, None)
        .await
        .unwrap()
    else {
        panic!("expected fresh group response");
    };
    assert!(matches!(
        mgr.get_tile_with_etag("group", xyz, None, Some(&etag))
            .await
            .unwrap(),
        TileEtagResponse::NotModified
    ));
    assert_eq!(member.calls.load(Ordering::SeqCst), 1);

    let filter = maptile_core::filter::parse_cql2("id=1").unwrap();
    let query = TileQuery {
        filter: Some(&filter),
        ..TileQuery::default()
    };
    for _ in 0..2 {
        mgr.get_tile("group", xyz, Some(&query)).await.unwrap();
    }
    assert_eq!(member.calls.load(Ordering::SeqCst), 3);
    group.read_tile_batch(&[xyz], None).await.unwrap();
    assert_eq!(member.calls.load(Ordering::SeqCst), 4);

    let cold = TileCoord { z: 1, x: 0, y: 0 };
    let (group, direct) = tokio::join!(
        mgr.get_tile("group", cold, None),
        mgr.get_tile("member", cold, None),
    );
    group.unwrap();
    direct.unwrap();
    assert_eq!(member.calls.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn cached_member_reads_keep_the_resolved_snapshot_after_replacement() {
    let mgr = manager_with(vec![]);
    let weak_mgr = Arc::downgrade(&mgr);
    let new = SequentialMvt::new("member", &["roads"]);
    let mut old = SequentialMvt::new("member", &["water"]);
    let pending = Arc::new(Mutex::new(None::<Arc<dyn TileSource>>));
    let on_tile_info_pending = pending.clone();
    Arc::get_mut(&mut old).unwrap().on_tile_info = Some(Arc::new(move || {
        let replacement = on_tile_info_pending.lock().unwrap().take();
        if let Some(replacement) = replacement {
            weak_mgr.upgrade().unwrap().register(replacement);
        }
    }));
    mgr.register(old.clone());
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    mgr.get_tile("member", xyz, None).await.unwrap();
    let group =
        LazyCompositeSource::open("group".into(), vec!["member".into()], Arc::downgrade(&mgr));

    // The group's format check runs after it resolves its member and before it
    // looks up the tile in the cache. Replace the member at that point.
    *pending.lock().unwrap() = Some(new.clone());
    let resp = group.get_tile(xyz, None).await.unwrap();
    assert_eq!(layer_names(&resp), ["water"]);
    assert_eq!(old.calls.load(Ordering::SeqCst), 1);
    assert_eq!(new.calls.load(Ordering::SeqCst), 0);
    assert!(Arc::ptr_eq(
        &mgr.require("member").unwrap(),
        &(new.clone() as Arc<dyn TileSource>),
    ));

    assert_eq!(
        layer_names(&group.get_tile(xyz, None).await.unwrap()),
        ["roads"]
    );
    assert_eq!(new.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn manager_preserves_live_members_and_composite_etag_with_hot_cache() {
    let cache = Arc::new(TileCache::new(1, None, None).unwrap());
    let mgr = Arc::new(SourceManager::new().with_cache(cache));
    mgr.register(SequentialMvt::new("a", &["water"]));
    let group = Arc::new(LazyCompositeSource::open(
        "group".into(),
        vec!["a".into()],
        Arc::downgrade(&mgr),
    ));
    mgr.register(group.clone());
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    let expected_source = CompositeSource::new(
        "group",
        vec![CompositeChild::new(mgr.require("a").unwrap())],
    )
    .unwrap();
    let expected = expected_source
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
    mgr.register(SequentialMvt::new("a", &["roads"]));
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
