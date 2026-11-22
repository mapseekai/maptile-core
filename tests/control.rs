use bytes::Bytes;
use geozero::mvt::{Message, Tile, tile::Layer};
use maptile_core::{
    cache::{ContentVersion, StyleHash, TileCache},
    control::{
        CatalogService, CompositeDeclaration, DiscoveredDataset, DuplicateLayer, Inventory,
        MemberFailurePolicy, SourceDescriptor, TileDriver,
    },
    error::{MaptileError, MaptileResult},
    service::{ReadTarget, TileLayer, TileOutcome, TileService, TileStack, ValidatedTileRequest},
    sources::{
        MAX_TILE_BATCH_COORDS, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery,
        TileResponse,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct TestDriver {
    descriptor: SourceDescriptor,
    tiles: [Option<Bytes>; 2],
    error: Option<MaptileError>,
    reads: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl TileDriver for TestDriver {
    fn descriptor(&self) -> SourceDescriptor {
        self.descriptor.clone()
    }
    async fn read(
        &self,
        coord: TileCoord,
        _: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        Ok(self.tiles[coord.x as usize]
            .clone()
            .map(|data| TileResponse {
                data,
                info: self.descriptor.info,
                last_modified: None,
            }))
    }
}
fn tile(name: &str, extent: u32) -> Bytes {
    let tile = Tile {
        layers: vec![Layer {
            version: 2,
            name: name.into(),
            features: vec![],
            keys: vec![],
            values: vec![],
            extent: Some(extent),
        }],
    };
    Bytes::from(tile.encode_to_vec())
}
fn driver(
    id: &str,
    names: &[&str],
    tiles: [Option<Bytes>; 2],
    error: Option<MaptileError>,
    reads: Arc<AtomicUsize>,
) -> Arc<dyn TileDriver> {
    Arc::new(TestDriver {
        descriptor: strict_descriptor(id, names),
        tiles,
        error,
        reads,
    })
}
fn strict_descriptor(id: &str, names: &[&str]) -> SourceDescriptor {
    let info = TileInfo {
        format: TileFormat::Mvt,
        encoding: TileEncoding::Uncompressed,
    };
    let mut descriptor = SourceDescriptor::new(id, info, ContentVersion::from_input(id.as_bytes()));
    descriptor.capabilities.strict_mvt = true;
    descriptor.capabilities.consistent_export = true;
    descriptor.tilejson.vector_layers = Some(
        names
            .iter()
            .map(|name| tilejson::VectorLayer::new((*name).into(), Default::default()))
            .collect(),
    );
    descriptor
}
fn decl(
    id: &str,
    members: &[&str],
    duplicate_layer: DuplicateLayer,
    failure_policy: MemberFailurePolicy,
) -> CompositeDeclaration {
    CompositeDeclaration {
        id: id.into(),
        members: members.iter().map(|s| Arc::from(*s)).collect(),
        lazy: false,
        duplicate_layer,
        failure_policy,
    }
}
async fn stack_read(stack: &TileStack, id: &str, x: u32) -> MaptileResult<TileOutcome> {
    stack
        .read(ReadTarget::new(id, "scope").at(TileCoord { z: 1, x, y: 0 }))
        .await
}

#[tokio::test]
async fn public_stack_composite_uses_actual_layers_and_keeps_partial_out_of_cache() {
    let service = Arc::new(CatalogService::new(TileCache::new(16, None, None).unwrap()));
    let mut update = service.catalog().update();
    let reads_a = Arc::new(AtomicUsize::new(0));
    let reads_b = Arc::new(AtomicUsize::new(0));
    update
        .put(driver(
            "a",
            &["roads", "water"],
            [Some(tile("roads", 11)), Some(tile("water", 33))],
            None,
            reads_a.clone(),
        ))
        .unwrap();
    update
        .put(driver(
            "b",
            &["roads"],
            [Some(tile("roads", 22)), Some(tile("roads", 22))],
            None,
            reads_b,
        ))
        .unwrap();
    update
        .put(driver(
            "bad",
            &["bad"],
            [None, None],
            Some(MaptileError::SourceUnavailable("bad".into())),
            Arc::new(AtomicUsize::new(0)),
        ))
        .unwrap();
    update
        .composite(decl(
            "merged",
            &["a", "b"],
            DuplicateLayer::KeepFirst,
            MemberFailurePolicy::FailFast,
        ))
        .unwrap();
    update
        .composite(decl(
            "partial",
            &["a", "bad"],
            DuplicateLayer::Error,
            MemberFailurePolicy::SkipTransient,
        ))
        .unwrap();
    service.catalog().publish(update).unwrap();
    let stack = TileStack::builder(service.clone()).build();
    let query = TileQuery {
        style_hash: Some(StyleHash::from_bytes([3; 32])),
        ..Default::default()
    };
    let styled = ReadTarget {
        query: Some(&query),
        ..ReadTarget::new("merged", "scope")
    };
    assert!(matches!(
        stack
            .read_batch(styled, &[TileCoord { z: 1, x: 0, y: 0 }])
            .await,
        Err(MaptileError::Unsupported(_))
    ));
    assert_eq!(reads_a.load(Ordering::SeqCst), 0);
    for (x, expected) in [(0, 11), (1, 22)] {
        let TileOutcome::Present(tile) = stack_read(&stack, "merged", x).await.unwrap() else {
            panic!("expected present")
        };
        let tile = Tile::decode(tile.data).unwrap();
        assert_eq!(
            tile.layers
                .iter()
                .find(|l| l.name == "roads")
                .unwrap()
                .extent,
            Some(expected)
        );
    }
    for _ in 0..2 {
        assert!(matches!(
            stack_read(&stack, "partial", 0).await.unwrap(),
            TileOutcome::Partial { tile: Some(_), .. }
        ));
    }
    assert_eq!(reads_a.load(Ordering::SeqCst), 4);
    let generation = service.catalog().snapshot().generation();
    let error = stack_read(&stack, "a,b", 0).await.unwrap_err();
    assert!(matches!(error, MaptileError::Config(_)));
    let mut update = service.catalog().update();
    update
        .put(driver(
            "c",
            &["lake"],
            [Some(tile("lake", 44)), None],
            None,
            Arc::default(),
        ))
        .unwrap();
    service.catalog().publish(update).unwrap();
    let temporary = stack_read(&stack, "a,c", 0).await.unwrap();
    let layers = Tile::decode(temporary.tile().unwrap().data.clone())
        .unwrap()
        .layers;
    assert_eq!(
        layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
        ["roads", "lake"]
    );
    assert!(service.catalog().snapshot().describe("a,c").is_none());
    assert_eq!(service.catalog().snapshot().generation(), generation + 1);
}

#[tokio::test]
async fn temporary_composite_aliases_share_cache_fills_and_export_cursors() {
    struct CheckIdentity;
    #[async_trait::async_trait]
    impl TileLayer for CheckIdentity {
        async fn read(
            &self,
            request: &ValidatedTileRequest<'_>,
            next: TileService<'_>,
        ) -> MaptileResult<TileOutcome> {
            let id = request.descriptor().id.as_ref();
            assert!(matches!(id, "a,b" | "b,a"));
            assert_eq!(request.request().target.source, id);
            next.read().await
        }
    }

    let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut update = service.catalog().update();
    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));
    for (id, name, reads) in [("a", "roads", a.clone()), ("b", "water", b.clone())] {
        update
            .put(driver(
                id,
                &[name],
                [Some(tile(name, 4096)), None],
                None,
                reads,
            ))
            .unwrap();
    }
    service.catalog().publish(update).unwrap();
    let stack = TileStack::builder(service).layer(CheckIdentity).build();
    let xyz = TileCoord { z: 1, x: 0, y: 0 };
    let canonical = ReadTarget::new("a,b", "scope");
    let spaced = ReadTarget::new("  a ,\tb  ", "scope");
    let (first, alias) = tokio::join!(stack.read(canonical.at(xyz)), stack.read(spaced.at(xyz)));
    assert_eq!(
        first.unwrap().tile().unwrap().data,
        alias.unwrap().tile().unwrap().data
    );

    let coords = vec![xyz; MAX_TILE_BATCH_COORDS + 1];
    let (first_page, cursor) = stack.read_export_page(spaced, &coords, None).await.unwrap();
    assert_eq!(first_page.consumed, MAX_TILE_BATCH_COORDS);
    let cursor = cursor.unwrap();
    let (second_page, end) = stack
        .read_export_page(canonical, &coords, Some(&cursor))
        .await
        .unwrap();
    assert_eq!(second_page.consumed, 1);
    assert!(end.is_none());
    assert_eq!(a.load(Ordering::SeqCst), 1);
    assert_eq!(b.load(Ordering::SeqCst), 1);

    let reverse = ReadTarget::new("b , a", "scope");
    assert!(matches!(
        stack
            .read_export_page(reverse, &coords, Some(&cursor))
            .await,
        Err(MaptileError::VersionChanged(_))
    ));
    let reversed = stack.read(reverse.at(xyz)).await.unwrap();
    let decoded = Tile::decode(reversed.tile().unwrap().data.clone()).unwrap();
    assert_eq!(
        decoded
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect::<Vec<_>>(),
        ["water", "roads"]
    );
    assert_eq!(a.load(Ordering::SeqCst), 2);
    assert_eq!(b.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn discovery_commits_only_authoritative_changes_and_preserves_id_ownership() {
    let service = CatalogService::new(TileCache::new(1, None, None).unwrap());
    let catalog = service.catalog();
    let opens = AtomicUsize::new(0);
    let open = |item: &DiscoveredDataset| {
        opens.fetch_add(1, Ordering::Relaxed);
        std::future::ready(Ok(driver(
            &item.id,
            &["roads"],
            [None, None],
            None,
            Arc::default(),
        )))
    };
    let item = DiscoveredDataset {
        id: "roads".into(),
        dataset: "b/roads.pmtiles".into(),
        revision: ContentVersion::from_input(b"v1"),
    };
    let list = std::slice::from_ref(&item);
    let first = catalog
        .reconcile(
            catalog.snapshot().generation(),
            "files",
            Inventory::Complete(list),
            open,
        )
        .await
        .unwrap();
    let before = catalog.snapshot();
    let revision = before.describe("roads").unwrap().revision;
    for inventory in [
        Inventory::Partial(&[]),
        Inventory::Unchanged,
        Inventory::Failed,
        Inventory::Complete(list),
    ] {
        assert_eq!(
            catalog
                .reconcile(catalog.snapshot().generation(), "files", inventory, open)
                .await
                .unwrap(),
            first
        );
    }
    assert_eq!(opens.load(Ordering::Relaxed), 1);
    let changed = DiscoveredDataset {
        revision: ContentVersion::from_input(b"v2"),
        ..item.clone()
    };
    catalog
        .reconcile(
            catalog.snapshot().generation(),
            "files",
            Inventory::Partial(&[changed]),
            open,
        )
        .await
        .unwrap();
    assert_ne!(
        catalog.snapshot().describe("roads").unwrap().revision,
        revision
    );
    assert!(matches!(
        catalog
            .reconcile(first, "files", Inventory::Complete(&[]), open)
            .await,
        Err(MaptileError::VersionChanged(_))
    ));
    let generation = catalog.snapshot().generation();
    assert!(
        catalog
            .reconcile(
                catalog.snapshot().generation(),
                "other",
                Inventory::Complete(list),
                open
            )
            .await
            .is_err()
    );
    assert_eq!(catalog.snapshot().generation(), generation);
    catalog
        .reconcile(
            catalog.snapshot().generation(),
            "files",
            Inventory::Complete(&[]),
            open,
        )
        .await
        .unwrap();
    assert!(catalog.snapshot().describe("roads").is_none());
    let conflict = DiscoveredDataset {
        dataset: "a/roads.pmtiles".into(),
        ..item.clone()
    };
    assert!(
        catalog
            .reconcile(
                catalog.snapshot().generation(),
                "files",
                Inventory::Complete(&[conflict]),
                open
            )
            .await
            .is_err()
    );
    catalog
        .reconcile(
            catalog.snapshot().generation(),
            "files",
            Inventory::Complete(list),
            open,
        )
        .await
        .unwrap();
    assert_eq!(
        catalog.snapshot().describe("roads").unwrap().revision,
        revision
    );
    let two = [
        DiscoveredDataset {
            id: "other".into(),
            ..item.clone()
        },
        DiscoveredDataset {
            id: "bad".into(),
            ..item
        },
    ];
    let failed = catalog
        .reconcile(
            catalog.snapshot().generation(),
            "files",
            Inventory::Complete(&two),
            |item| {
                std::future::ready(if item.id.as_ref() == "bad" {
                    Err(MaptileError::Config("invalid source".into()))
                } else {
                    Ok(driver(
                        &item.id,
                        &["water"],
                        [None, None],
                        None,
                        Arc::default(),
                    ))
                })
            },
        )
        .await;
    assert!(failed.is_err());
    let snapshot = catalog.snapshot();
    assert!(snapshot.describe("roads").is_some());
    assert!(snapshot.describe("other").is_none());
}

struct OpeningDriver {
    id: &'static str,
    stops: AtomicUsize,
    drains: AtomicUsize,
    fail_first_drain: bool,
    finished: Option<std::sync::mpsc::Sender<()>>,
    pause_descriptor: std::sync::Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
}

impl OpeningDriver {
    fn new(id: &'static str) -> Self {
        Self {
            id,
            stops: 0.into(),
            drains: 0.into(),
            fail_first_drain: false,
            finished: None,
            pause_descriptor: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl TileDriver for OpeningDriver {
    fn descriptor(&self) -> SourceDescriptor {
        if let Some((entered, release)) = self.pause_descriptor.lock().unwrap().take() {
            entered.send(()).unwrap();
            release
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }
        SourceDescriptor::new(
            self.id,
            TileInfo::new(TileFormat::Mvt, TileEncoding::Uncompressed),
            ContentVersion::from_input(b"v1"),
        )
    }
    async fn read(
        &self,
        _: TileCoord,
        _: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        Ok(None)
    }
    fn stop_admission(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
    async fn drain(&self) -> MaptileResult<()> {
        if self.drains.fetch_add(1, Ordering::SeqCst) == 0 && self.fail_first_drain {
            return Err(MaptileError::SourceUnavailable(self.id.into()));
        }
        if let Some(finished) = &self.finished {
            let _ = finished.send(());
        }
        Ok(())
    }
}

fn discovered(id: &str) -> DiscoveredDataset {
    DiscoveredDataset {
        id: id.into(),
        dataset: id.into(),
        revision: ContentVersion::from_input(b"v1"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_a_driver_whose_descriptor_was_in_progress() {
    let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let driver = Arc::new(OpeningDriver::new("roads"));
    let (entered, at_descriptor) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    *driver.pause_descriptor.lock().unwrap() = Some((entered, wait));
    let build = {
        let service = service.clone();
        let driver = driver.clone();
        tokio::spawn(async move {
            service
                .catalog()
                .reconcile(
                    0,
                    "files",
                    Inventory::Complete(&[discovered("roads")]),
                    |_| std::future::ready(Ok(driver.clone() as Arc<dyn TileDriver>)),
                )
                .await
        })
    };
    at_descriptor.await.unwrap();
    let mut shutdown = Box::pin(
        service
            .catalog()
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(5)),
    );
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    release.send(()).unwrap();
    assert!(matches!(build.await.unwrap(), Err(MaptileError::Cancelled)));
    shutdown.await.unwrap();
    assert_eq!(driver.drains.load(Ordering::SeqCst), 1);
    assert!(driver.stops.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn rejected_inventory_drains_unpublished_drivers_and_preserves_shared_ones() {
    let service = CatalogService::new(TileCache::new(1, None, None).unwrap());
    let catalog = service.catalog();
    let first = Arc::new(OpeningDriver::new("first"));
    let wrong = Arc::new(OpeningDriver::new("returned"));
    let inventory = [discovered("first"), discovered("expected")];
    let result = catalog
        .reconcile(0, "files", Inventory::Complete(&inventory), |item| {
            let driver = if item.id.as_ref() == "first" {
                &first
            } else {
                &wrong
            };
            std::future::ready(Ok(driver.clone() as Arc<dyn TileDriver>))
        })
        .await;
    assert!(matches!(result, Err(MaptileError::Config(_))));
    assert_eq!(catalog.snapshot().generation(), 0);
    for driver in [&first, &wrong] {
        assert_eq!(driver.stops.load(Ordering::SeqCst), 1);
        assert_eq!(driver.drains.load(Ordering::SeqCst), 1);
    }
    let live = Arc::new(OpeningDriver::new("live"));
    let mut update = catalog.update();
    update.put(live.clone()).unwrap();
    let generation = catalog.publish(update).unwrap();
    let result = catalog
        .reconcile(
            generation,
            "files",
            Inventory::Complete(&[discovered("other")]),
            |_| std::future::ready(Ok(live.clone() as Arc<dyn TileDriver>)),
        )
        .await;
    assert!(matches!(result, Err(MaptileError::Config(_))));
    assert!(catalog.snapshot().describe("live").is_some());
    assert_eq!(live.stops.load(Ordering::SeqCst), 0);
    assert_eq!(live.drains.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shutdown_retries_an_unpublished_driver_that_failed_to_drain() {
    let service = CatalogService::new(TileCache::new(1, None, None).unwrap());
    let driver = Arc::new(OpeningDriver {
        fail_first_drain: true,
        ..OpeningDriver::new("wrong")
    });
    let result = service
        .catalog()
        .reconcile(
            0,
            "files",
            Inventory::Complete(&[discovered("expected")]),
            |_| std::future::ready(Ok(driver.clone() as Arc<dyn TileDriver>)),
        )
        .await;
    assert!(matches!(result, Err(MaptileError::SourceUnavailable(_))));
    assert_eq!(driver.drains.load(Ordering::SeqCst), 1);
    service
        .catalog()
        .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(driver.drains.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn stale_failure_cannot_admit_a_second_probe() {
    use maptile_core::cache::CacheZoomRange;
    use tokio::sync::Notify;
    struct RecoveryDriver {
        calls: AtomicUsize,
        late: Notify,
        probe: Notify,
    }
    #[async_trait::async_trait]
    impl TileDriver for RecoveryDriver {
        fn descriptor(&self) -> SourceDescriptor {
            let mut d = SourceDescriptor::new(
                "health",
                TileInfo::new(TileFormat::Mvt, TileEncoding::Uncompressed),
                ContentVersion::from_input(b"v"),
            );
            d.cache_zoom = CacheZoomRange::disabled();
            d
        }
        async fn read(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<TileResponse>> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => self.late.notified().await,
                1 => (),
                2 => {
                    self.probe.notified().await;
                    return Ok(None);
                }
                _ => return Ok(None),
            }
            Err(MaptileError::SourceUnavailable("transient".into()))
        }
    }
    let driver = Arc::new(RecoveryDriver {
        calls: 0.into(),
        late: Notify::new(),
        probe: Notify::new(),
    });
    let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut edit = service.catalog().update();
    edit.put(driver.clone()).unwrap();
    service.catalog().publish(edit).unwrap();
    let stack = TileStack::builder(service).build();
    let mut late = Box::pin(stack_read(&stack, "health", 0));
    assert!(futures::poll!(late.as_mut()).is_pending());
    assert!(matches!(
        stack_read(&stack, "health", 0).await,
        Err(MaptileError::SourceUnavailable(_))
    ));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    let mut probe = Box::pin(stack_read(&stack, "health", 0));
    assert!(futures::poll!(probe.as_mut()).is_pending());
    driver.late.notify_one();
    assert!(late.await.is_err());
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    let other = stack_read(&stack, "health", 0).await;
    assert!(
        matches!(other, Err(MaptileError::SourceUnavailable(_))),
        "{other:?}"
    );
    assert_eq!(driver.calls.load(Ordering::SeqCst), 3);
    driver.probe.notify_one();
    assert!(probe.await.is_ok());
}

#[tokio::test]
async fn nested_fail_fast_holds_leaf_capacity_until_abandoned_blocking_work_finishes() {
    use maptile_core::{control::CompositeLimits, transport};
    use std::{
        sync::{Mutex, atomic::AtomicBool, mpsc},
        task::{Context, Wake, Waker},
        time::Duration,
    };
    use tokio::sync::Notify;

    struct HeldGzip {
        data: Bytes,
        entered: Arc<Notify>,
        release: mpsc::Receiver<()>,
    }
    impl AsRef<[u8]> for HeldGzip {
        fn as_ref(&self) -> &[u8] {
            &self.data
        }
    }
    impl Drop for HeldGzip {
        fn drop(&mut self) {
            self.entered.notify_one();
            let _ = self.release.recv_timeout(Duration::from_secs(10));
        }
    }

    enum LeafRead {
        Gzip(Mutex<Option<Bytes>>),
        Timeout(Arc<Notify>),
        Sibling {
            started: Arc<AtomicUsize>,
            resume: tokio_util::sync::CancellationToken,
            data: Bytes,
        },
    }
    struct ControlledLeaf(SourceDescriptor, LeafRead);
    #[async_trait::async_trait]
    impl TileDriver for ControlledLeaf {
        fn descriptor(&self) -> SourceDescriptor {
            self.0.clone()
        }
        async fn read(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<TileResponse>> {
            let data = match &self.1 {
                LeafRead::Gzip(data) => data.lock().unwrap().take().unwrap(),
                LeafRead::Timeout(fail) => {
                    fail.notified().await;
                    return Err(MaptileError::Deadline);
                }
                LeafRead::Sibling {
                    started,
                    resume,
                    data,
                } => {
                    started.fetch_add(1, Ordering::SeqCst);
                    resume.cancelled().await;
                    data.clone()
                }
            };
            Ok(Some(TileResponse {
                data,
                info: self.0.info,
                last_modified: None,
            }))
        }
    }
    struct Woken(AtomicBool);
    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let valid = transport::encode_as(&tile("gzip", 4096), TileEncoding::Gzip).unwrap();
    for input in [valid.clone(), valid.slice(..valid.len() - 1)] {
        let limits = CompositeLimits {
            max_concurrency: 2,
            ..CompositeLimits::default()
        };
        let engine = Arc::new(
            CatalogService::with_limits(TileCache::new(1, None, None).unwrap(), limits).unwrap(),
        );
        let mut update = engine.catalog().update();
        let entered = Arc::new(Notify::new());
        let (release, released) = mpsc::channel();
        let descriptor = |id| strict_descriptor(id, &[id]);
        let mut gzip = descriptor("gzip");
        gzip.info.encoding = TileEncoding::Gzip;
        let bytes = Bytes::from_owner(HeldGzip {
            data: input,
            entered: entered.clone(),
            release: released,
        });
        update
            .put(Arc::new(ControlledLeaf(
                gzip,
                LeafRead::Gzip(Mutex::new(Some(bytes))),
            )))
            .unwrap();
        let fail = Arc::new(Notify::new());
        update
            .put(Arc::new(ControlledLeaf(
                descriptor("timeout"),
                LeafRead::Timeout(fail.clone()),
            )))
            .unwrap();
        let siblings = Arc::new(AtomicUsize::new(0));
        let resume = tokio_util::sync::CancellationToken::new();
        for name in ["roads", "water"] {
            let read = LeafRead::Sibling {
                started: siblings.clone(),
                resume: resume.clone(),
                data: tile(name, 4096),
            };
            update
                .put(Arc::new(ControlledLeaf(descriptor(name), read)))
                .unwrap();
        }
        update
            .composite(decl(
                "inner",
                &["gzip", "timeout"],
                DuplicateLayer::Error,
                MemberFailurePolicy::FailFast,
            ))
            .unwrap();
        update
            .composite(decl(
                "outer",
                &["inner", "roads", "water"],
                DuplicateLayer::Error,
                MemberFailurePolicy::SkipTransient,
            ))
            .unwrap();
        engine.catalog().publish(update).unwrap();
        let stack = TileStack::builder(engine).build();
        let mut request = Box::pin(stack_read(&stack, "outer", 0));
        tokio::select! {
            result = request.as_mut() => panic!("request finished before decoding: {result:?}"),
            () = entered.notified() => {},
            () = tokio::time::sleep(Duration::from_secs(10)) => panic!("decoder did not start"),
        }
        fail.notify_one();

        // Poll all leaves made ready by the timeout while the abandoned decoder still holds its input.
        let woken = Arc::new(Woken(true.into()));
        let waker = Waker::from(woken.clone());
        while woken.0.swap(false, Ordering::SeqCst) {
            assert!(
                request
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }
        let started_before_release = siblings.load(Ordering::SeqCst);
        release.send(()).unwrap();
        resume.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(10), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(started_before_release, 1);
        assert_eq!(siblings.load(Ordering::SeqCst), 2);
        let TileOutcome::Partial {
            tile: Some(tile),
            failures,
        } = outcome
        else {
            panic!("expected partial tile")
        };
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].source.as_ref(), "inner");
        assert!(matches!(failures[0].error, MaptileError::Deadline));
        let layers = Tile::decode(tile.data).unwrap().layers;
        assert_eq!(
            layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            ["roads", "water"]
        );
    }
}

#[tokio::test]
async fn later_fatal_member_cancels_earlier_pending_member() {
    struct Pending(Arc<dyn TileDriver>);
    #[async_trait::async_trait]
    impl TileDriver for Pending {
        fn descriptor(&self) -> SourceDescriptor {
            self.0.descriptor()
        }
        async fn read(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<TileResponse>> {
            std::future::pending().await
        }
    }
    let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut edit = service.catalog().update();
    edit.put(Arc::new(Pending(driver(
        "early",
        &["a"],
        [None, None],
        None,
        Arc::default(),
    ))))
    .unwrap();
    edit.put(driver(
        "late",
        &["b"],
        [None, None],
        Some(MaptileError::InvalidRequest("fatal".into())),
        Arc::default(),
    ))
    .unwrap();
    edit.composite(decl(
        "both",
        &["early", "late"],
        DuplicateLayer::Error,
        MemberFailurePolicy::FailFast,
    ))
    .unwrap();
    service.catalog().publish(edit).unwrap();
    let stack = TileStack::builder(service).build();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        stack_read(&stack, "both", 0),
    )
    .await;
    assert!(
        matches!(response, Ok(Err(MaptileError::InvalidRequest(_)))),
        "{response:?}"
    );
}

#[tokio::test]
async fn shutdown_cannot_finish_while_factory_is_opening() {
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Notify;
    struct Opening(Arc<AtomicBool>);
    impl Drop for Opening {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let active = Arc::new(AtomicBool::new(false));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let building = {
        let (service, active, started, release) = (
            service.clone(),
            active.clone(),
            started.clone(),
            release.clone(),
        );
        tokio::spawn(async move {
            let item = DiscoveredDataset {
                id: "roads".into(),
                dataset: "file.pmtiles".into(),
                revision: ContentVersion::from_input(b"1"),
            };
            service
                .catalog()
                .reconcile(0, "files", Inventory::Complete(&[item]), |_| {
                    let (active, started, release) =
                        (active.clone(), started.clone(), release.clone());
                    async move {
                        active.store(true, Ordering::SeqCst);
                        let _opening = Opening(active);
                        started.notify_one();
                        release.notified().await;
                        Ok(driver(
                            "roads",
                            &["road"],
                            [None, None],
                            None,
                            Arc::default(),
                        ))
                    }
                })
                .await
        })
    };
    started.notified().await;
    let mut shutdown = Box::pin(
        service
            .catalog()
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(4)),
    );
    let result = futures::poll!(shutdown.as_mut());
    let finished_while_active = result.is_ready() && active.load(Ordering::SeqCst);
    release.notify_one();
    if result.is_pending() {
        shutdown.await.unwrap();
    }
    let _ = building.await.unwrap();
    assert!(
        !finished_while_active,
        "shutdown completed while factory still active"
    );
}

#[tokio::test]
async fn cancelling_one_of_two_builds_keeps_shared_handle_until_last_rejects() {
    let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let common = Arc::new(OpeningDriver::new("shared"));
    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let (service, common, gate) = (service.clone(), common.clone(), gate.clone());
        jobs.push(tokio::spawn(async move {
            service
                .catalog()
                .reconcile(
                    0,
                    "files",
                    Inventory::Complete(&[discovered("shared"), discovered("hold")]),
                    |item| {
                        let (driver, gate, hold) =
                            (common.clone(), gate.clone(), item.id.as_ref() == "hold");
                        async move {
                            if hold {
                                gate.wait().await;
                                std::future::pending::<()>().await;
                            }
                            Ok(driver as Arc<dyn TileDriver>)
                        }
                    },
                )
                .await
        }));
    }
    gate.wait().await;
    let second = jobs.pop().unwrap();
    let first = jobs.pop().unwrap();
    first.abort();
    let _ = first.await;
    assert_eq!(common.stops.load(Ordering::SeqCst), 0);
    second.abort();
    let _ = second.await;
    service
        .catalog()
        .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(common.drains.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn catalog_revocation_stops_cached_leaf_and_composite() {
    let service = Arc::new(CatalogService::new(TileCache::new(16, None, None).unwrap()));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut update = service.catalog().update();
    update
        .put(driver(
            "roads",
            &["roads"],
            [Some(tile("roads", 4096)), None],
            None,
            reads.clone(),
        ))
        .unwrap();
    update
        .composite(decl(
            "city",
            &["roads"],
            DuplicateLayer::Error,
            MemberFailurePolicy::FailFast,
        ))
        .unwrap();
    service.catalog().publish(update).unwrap();
    let stack = TileStack::builder(service.clone()).build();
    for id in ["roads", "city"] {
        assert!(stack_read(&stack, id, 0).await.unwrap().tile().is_some());
    }
    let before = reads.load(Ordering::SeqCst);
    for id in ["roads", "city"] {
        assert!(stack_read(&stack, id, 0).await.unwrap().tile().is_some());
    }
    assert_eq!(reads.load(Ordering::SeqCst), before);
    let observed = service.catalog().snapshot();
    service
        .catalog()
        .revoke(
            &observed,
            "roads",
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
    for id in ["roads", "city"] {
        assert!(matches!(
            stack_read(&stack, id, 0).await,
            Err(MaptileError::Cancelled)
        ));
    }
    assert_eq!(reads.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn manual_publication_drains_retired_and_rejected_drivers() {
    let service = CatalogService::new(TileCache::new(1, None, None).unwrap());
    let catalog = service.catalog();
    let retired = Arc::new(OpeningDriver::new("retired"));
    let rejected = Arc::new(OpeningDriver::new("rejected"));
    let mut first = catalog.update();
    first.put(retired.clone()).unwrap();
    catalog.publish(first).unwrap();
    let mut stale = catalog.update();
    stale.put(rejected.clone()).unwrap();
    let mut remove = catalog.update();
    remove.remove("retired");
    catalog.publish(remove).unwrap();
    assert!(matches!(
        catalog.publish(stale),
        Err(MaptileError::VersionChanged(_))
    ));
    catalog
        .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(2))
        .await
        .unwrap();
    for source in [retired, rejected] {
        assert_eq!(source.stops.load(Ordering::SeqCst), 1);
        assert_eq!(source.drains.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_manual_source_admission() {
    let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let mut driver = OpeningDriver::new("manual");
    driver.pause_descriptor = std::sync::Mutex::new(Some((entered_tx, release_rx)));
    let driver = Arc::new(driver);
    let mut edit = service.catalog().update();
    let submitted = driver.clone();
    let put = tokio::task::spawn_blocking(move || edit.put(submitted));
    entered.await.unwrap();
    let mut shutdown = Box::pin(
        service
            .catalog()
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
    );
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    release.send(()).unwrap();
    assert!(matches!(put.await.unwrap(), Err(MaptileError::Cancelled)));
    shutdown.await.unwrap();
    assert_eq!(driver.drains.load(Ordering::SeqCst), 1);
}

#[test]
fn ordinary_retirement_drains_without_an_application_runtime() {
    let service = CatalogService::new(TileCache::new(1, None, None).unwrap());
    let catalog = service.catalog();
    let (finished, drained) = std::sync::mpsc::channel();
    let mut driver = OpeningDriver::new("old");
    driver.finished = Some(finished);
    let mut first = catalog.update();
    first.put(Arc::new(driver)).unwrap();
    catalog.publish(first).unwrap();
    let mut remove = catalog.update();
    remove.remove("old");
    catalog.publish(remove).unwrap();
    drained
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
}
