use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::cache::TileCache;
use maptile_core::error::{MaptileError, MaptileResult};
use maptile_core::sources::manager::SourceManager;
use maptile_core::sources::{
    MosaicPixelSelection, TileCoord, TileEncoding, TileEtagResponse, TileFormat, TileInfo,
    TileQuery, TileResponse, TileSource,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tilejson::{TileJSON, tilejson};

struct Stub {
    id: String,
    tj: TileJSON,
}

impl Stub {
    fn arc(id: &str) -> Arc<dyn TileSource> {
        Arc::new(Stub {
            id: id.to_string(),
            tj: tilejson! { tiles: vec![] },
        })
    }
}

#[async_trait]
impl TileSource for Stub {
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
        Box::new(Stub {
            id: self.id.clone(),
            tj: self.tj.clone(),
        })
    }
    async fn get_tile(
        &self,
        _xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        Ok(TileResponse {
            data: Bytes::new(),
            info: self.tile_info(),
            last_modified: None,
        })
    }
}

#[test]
fn register_and_get() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    let got = m.get("a").expect("present");
    assert_eq!(got.id(), "a");
}

#[tokio::test]
async fn manager_reads_and_replacements_reach_the_public_tracing_target() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    // Other manager tests can register these tracing callsites concurrently.
    const CHILD: &str = "MAPTILE_MANAGER_TRACE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let name = "manager_reads_and_replacements_reach_the_public_tracing_target";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--format=pretty", "--color=never"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let passed = format!("test {name} ... ok");
        assert!(
            output.status.success() && stdout.lines().any(|line| line == passed),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    #[derive(Default)]
    struct ManagerTrace {
        read: AtomicBool,
        replacement: AtomicBool,
    }

    impl Subscriber for ManagerTrace {
        fn enabled(&self, meta: &Metadata<'_>) -> bool {
            meta.target() == "maptile_core::sources::manager"
        }
        fn new_span(&self, attrs: &Attributes<'_>) -> Id {
            if attrs.metadata().name() == "get_tile" {
                self.read.store(true, Ordering::Relaxed);
            }
            Id::from_u64(1)
        }
        fn event(&self, event: &Event<'_>) {
            if event.metadata().fields().field("source_id").is_some() {
                self.replacement.store(true, Ordering::Relaxed);
            }
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    let trace = Arc::new(ManagerTrace::default());
    tracing::subscriber::set_global_default(trace.clone()).unwrap();
    let manager = SourceManager::new();
    manager.register(Stub::arc("a"));
    manager.register(Stub::arc("a"));
    manager
        .get_tile("a", TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(trace.read.load(Ordering::Relaxed), "missing get_tile span");
    assert!(
        trace.replacement.load(Ordering::Relaxed),
        "missing replacement event"
    );
}

#[test]
fn get_missing_returns_none() {
    let m = SourceManager::new();
    assert!(m.get("nope").is_none());
}

#[test]
fn remove_returns_evicted_value() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    let removed = m.remove("a").expect("present");
    assert_eq!(removed.id(), "a");
    assert!(m.get("a").is_none());
}

#[test]
fn list_ids_returns_all_registered() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    m.register(Stub::arc("b"));
    let mut ids = m.list_ids();
    ids.sort();
    assert_eq!(ids, vec!["a", "b"]);
}

#[test]
fn snapshot_tilejson_has_one_per_source() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    m.register(Stub::arc("b"));
    assert_eq!(m.snapshot_tilejson().len(), 2);
}

#[test]
fn register_overwrites_same_id() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    m.register(Stub::arc("a"));
    assert_eq!(m.list_ids().len(), 1);
}

#[test]
fn shared_cache_entries_are_validated_before_legacy_reads() {
    use maptile_core::{
        cache::{TileCache, TileCacheKey, hashing::canonical_render_hash},
        sources::TileEtagResponse,
        transport::encode_as,
    };
    use std::time::Duration;
    use tokio::sync::oneshot;

    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let cache = Arc::new(TileCache::new(1, None, None).unwrap());
            let manager = SourceManager::new().with_cache(cache.clone());
            let source = Stub::arc("archive");
            let coord = TileCoord { z: 0, x: 0, y: 0 };
            let key = TileCacheKey::new(
                source.id().into(),
                source.content_version(),
                coord.z,
                coord.x,
                coord.y,
                canonical_render_hash(None, None, 256, 1, 1),
            );
            manager.register(source);
            let mut tile = TileResponse {
                data: Bytes::from_static(b"invalid compressed bytes"),
                info: TileInfo::new(TileFormat::Mvt, TileEncoding::Gzip),
                last_modified: None,
            };
            cache.insert(key.clone(), tile.clone()).await;
            assert!(matches!(
                manager.get_tile("archive", coord, None).await,
                Err(MaptileError::Transcode(_))
            ));
            assert!(matches!(
                manager
                    .get_tile_with_etag("archive", coord, None, None)
                    .await,
                Err(MaptileError::Transcode(_))
            ));
            tile.data = encode_as(b"valid cache write", TileEncoding::Gzip).unwrap();
            cache.insert(key, tile.clone()).await;
            assert_eq!(
                manager.get_tile("archive", coord, None).await.unwrap().data,
                tile.data
            );

            let (started, ready) = oneshot::channel();
            let (release, done) = oneshot::channel();
            let blocked = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                let _ = done.blocking_recv();
            });
            ready.await.unwrap();
            let warm = tokio::time::timeout(
                Duration::from_secs(2),
                manager.get_tile_with_etag("archive", coord, None, None),
            )
            .await;
            release.send(()).unwrap();
            blocked.await.unwrap();
            let TileEtagResponse::Fresh { resp, .. } =
                warm.expect("warm read started a second decode").unwrap()
            else {
                panic!("expected the validated compressed tile");
            };
            assert_eq!(resp.data, tile.data);
            assert_eq!(resp.info, tile.info);
        });
}

#[test]
fn conditional_failure_only_retires_the_observed_instance() {
    let manager = SourceManager::new();
    let old = Stub::arc("a");
    let replacement = Stub::arc("a");
    assert_eq!(old.content_version(), replacement.content_version());
    manager.register(old.clone());
    manager.register(replacement.clone());

    assert!(!manager.mark_unavailable_if_current(&old));
    assert!(Arc::ptr_eq(&manager.require("a").unwrap(), &replacement));
    assert!(manager.mark_unavailable_if_current(&replacement));
    assert!(!manager.mark_unavailable_if_current(&replacement));
    assert!(matches!(
        manager.require("a"),
        Err(MaptileError::SourceUnavailable(_))
    ));
    manager.remove("a");
    assert!(!manager.mark_unavailable_if_current(&old));
    assert!(!manager.mark_unavailable_if_current(&replacement));
    assert!(!manager.mark_unavailable_if_current(&Stub::arc("missing")));
    assert!(matches!(
        manager.require("a"),
        Err(MaptileError::SourceNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn foreground_recovery_invokes_factory_only_for_an_eligible_unavailable_id() {
    let manager = SourceManager::new();
    let original = Stub::arc("a");
    manager.register(original.clone());
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let factory = || {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(Ok(Stub::arc("a")))
    };

    let ready = manager.recover_unavailable("a", factory).await.unwrap();
    assert!(Arc::ptr_eq(&ready, &original));
    assert!(matches!(
        manager.recover_unavailable("missing", factory).await,
        Err(MaptileError::SourceNotFound(_))
    ));
    assert!(manager.mark_unavailable_if_current(&original));
    assert!(matches!(
        manager.recover_unavailable("a", factory).await,
        Err(MaptileError::SourceUnavailable(_))
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let recovered = manager.recover_unavailable("a", factory).await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(&manager.require("a").unwrap(), &recovered));
    assert!(!manager.mark_unavailable_if_current(&original));
    assert!(!manager.is_unavailable("a"));
}

#[tokio::test]
async fn concurrent_register_and_get_is_safe() {
    let m = Arc::new(SourceManager::new());
    let mut handles = vec![];
    for i in 0..50u32 {
        let m2 = m.clone();
        handles.push(tokio::spawn(async move {
            m2.register(Stub::arc(&format!("s{i}")));
        }));
        let m3 = m.clone();
        handles.push(tokio::spawn(async move {
            let _ = m3.get(&format!("s{i}"));
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(m.list_ids().len(), 50);
}

fn complete_style(document: serde_json::Value) -> maptile_core::raster::style::EffectiveStyle {
    maptile_core::raster::style::EffectiveStyle::from_raster_style(
        raster_style::Style::from_value(document).unwrap(),
    )
    .unwrap()
}

struct CountingSource {
    id: String,
    tj: tilejson::TileJSON,
    calls: Arc<AtomicUsize>,
    raster_style: bool,
    mosaic_selection: Option<MosaicPixelSelection>,
    seen_selection: Arc<Mutex<Vec<Option<MosaicPixelSelection>>>>,
    on_close: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[async_trait]
impl TileSource for CountingSource {
    fn supports_bbox(&self) -> bool {
        true
    }
    fn supports_filter(&self) -> bool {
        true
    }
    fn supports_raster_style(&self) -> bool {
        self.raster_style
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn tilejson(&self) -> &tilejson::TileJSON {
        &self.tj
    }
    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        }
    }
    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(CountingSource {
            id: self.id.clone(),
            tj: self.tj.clone(),
            calls: self.calls.clone(),
            raster_style: self.raster_style,
            mosaic_selection: self.mosaic_selection,
            seen_selection: self.seen_selection.clone(),
            on_close: self.on_close.clone(),
        })
    }
    fn start_close(&self) {
        if let Some(on_close) = &self.on_close {
            on_close();
        }
    }
    fn mosaic_pixel_selection(&self) -> Option<MosaicPixelSelection> {
        self.mosaic_selection
    }
    async fn get_tile(
        &self,
        _xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_selection
            .lock()
            .unwrap()
            .push(query.and_then(|q| q.pixel_selection));
        Ok(TileResponse {
            data: Bytes::from_static(b"\x00\x01\x02"),
            info: TileInfo {
                format: TileFormat::Mvt,
                encoding: TileEncoding::Uncompressed,
            },
            last_modified: None,
        })
    }
}

fn counting_source(id: &str) -> (Arc<CountingSource>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let src = Arc::new(CountingSource {
        id: id.into(),
        tj: tilejson! { tiles: vec![] },
        calls: calls.clone(),
        raster_style: false,
        mosaic_selection: None,
        seen_selection: Arc::new(Mutex::new(Vec::new())),
        on_close: None,
    });
    (src, calls)
}

#[test]
fn unavailable_source_state_clears_only_after_registration_or_authoritative_removal() {
    let manager = SourceManager::new();
    let (source, _) = counting_source("recovering");
    manager.register(source.clone());
    manager.mark_unavailable("recovering");
    assert!(manager.get("recovering").is_none());
    assert!(manager.list_ids().is_empty());
    assert!(manager.snapshot_tilejson().is_empty());
    assert!(matches!(
        manager.require("recovering"),
        Err(MaptileError::SourceUnavailable(_))
    ));
    manager.register(source);
    assert!(manager.require("recovering").is_ok());
    manager.mark_unavailable("recovering");
    manager.remove("recovering");
    assert!(matches!(
        manager.require("recovering"),
        Err(MaptileError::SourceNotFound(_))
    ));
}

#[test]
fn registration_cannot_erase_a_concurrent_unavailable_state() {
    use std::sync::mpsc;

    let manager = Arc::new(SourceManager::new());
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    let (mut old, _) = counting_source("same");
    Arc::get_mut(&mut old).unwrap().on_close = Some(Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
    }));
    manager.register(old);

    let writer = {
        let manager = manager.clone();
        std::thread::spawn(move || manager.register(counting_source("same").0))
    };
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let failure = {
        let manager = manager.clone();
        std::thread::spawn(move || {
            manager.mark_unavailable("same");
            done_tx.send(()).unwrap();
        })
    };
    let completed = done_rx.recv_timeout(Duration::from_secs(2));
    resume_tx.send(()).unwrap();
    writer.join().unwrap();
    failure.join().unwrap();
    assert!(
        completed.is_ok(),
        "close callbacks must not hold a registry lock"
    );
    assert!(matches!(
        manager.require("same"),
        Err(MaptileError::SourceUnavailable(_))
    ));
    assert!(manager.is_unavailable("same"));
    assert!(manager.list_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn repeated_failure_marks_preserve_backoff_and_close_the_source_once() {
    let manager = SourceManager::new();
    let (source, closed) = closing_source("retry");
    let source: Arc<dyn TileSource> = source;
    manager.register(source.clone());
    assert!(manager.mark_unavailable_if_current(&source));
    tokio::time::advance(Duration::from_millis(750)).await;

    manager.mark_unavailable("retry");
    assert!(!manager.mark_unavailable_if_current(&source));
    assert!(!manager.unavailable_retry_due("retry"));
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(250)).await;
    assert!(manager.unavailable_retry_due("retry"));
}

#[test]
fn conditional_retirement_runs_reentrant_close_after_unlocking() {
    let manager = Arc::new(SourceManager::new());
    let replacement: Arc<dyn TileSource> = counting_source("same").0;
    let (mut source, _) = counting_source("same");
    let weak_manager = Arc::downgrade(&manager);
    let on_close_replacement = replacement.clone();
    Arc::get_mut(&mut source).unwrap().on_close = Some(Arc::new(move || {
        let manager = weak_manager.upgrade().unwrap();
        assert!(manager.is_unavailable("same"));
        manager.register(on_close_replacement.clone());
    }));
    let source: Arc<dyn TileSource> = source;
    manager.register(source.clone());

    assert!(manager.mark_unavailable_if_current(&source));
    assert!(Arc::ptr_eq(&manager.require("same").unwrap(), &replacement));
    assert!(!manager.mark_unavailable_if_current(&source));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_recovery_admits_one_factory_and_allows_other_ids_to_progress() {
    use tokio::sync::{Barrier, Notify, mpsc};

    let manager = Arc::new(SourceManager::new());
    let ids = ["busy", "other"];
    for id in ids {
        manager.mark_unavailable(id);
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(ids.into_iter().all(|id| manager.unavailable_retry_due(id)));
    let start = Arc::new(Barrier::new(17));
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let manager = manager.clone();
        let start = start.clone();
        let release = release.clone();
        let calls = calls.clone();
        let entered_tx = entered_tx.clone();
        let finished_tx = finished_tx.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            let result = manager
                .recover_unavailable("busy", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert!(manager.is_unavailable("busy"));
                    entered_tx.send(()).unwrap();
                    async move {
                        release.notified().await;
                        Ok(counting_source("busy").0 as Arc<dyn TileSource>)
                    }
                })
                .await;
            assert!(finished_tx.send(result).is_ok());
        }));
    }
    start.wait().await;
    entered_rx.recv().await.unwrap();
    for _ in 0..15 {
        let result = tokio::time::timeout(Duration::from_secs(1), finished_rx.recv())
            .await
            .expect("non-owner requests must return before the factory finishes")
            .unwrap();
        assert!(matches!(result, Err(MaptileError::SourceUnavailable(_))));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    manager.mark_unavailable("busy");
    assert!(!manager.unavailable_retry_due("busy"));
    assert!(matches!(
        manager
            .recover_unavailable("busy", || {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Ok(counting_source("busy").0 as Arc<dyn TileSource>))
            })
            .await,
        Err(MaptileError::SourceUnavailable(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let other = manager
        .recover_unavailable("other", || async {
            Ok(counting_source("other").0 as Arc<dyn TileSource>)
        })
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&manager.require("other").unwrap(), &other));
    manager.register(counting_source("third").0);
    assert!(manager.remove("third").is_some());

    release.notify_one();
    let recovered = finished_rx.recv().await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&manager.require("busy").unwrap(), &recovered));
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn failed_cancelled_and_panicking_recoveries_release_ownership_with_fresh_backoff() {
    for failure in ["error", "cancel", "factory_panic", "future_panic"] {
        let manager = Arc::new(SourceManager::new());
        manager.mark_unavailable("retry");
        tokio::time::advance(Duration::from_secs(1)).await;
        match failure {
            "error" => {
                assert!(matches!(
                    manager
                        .recover_unavailable("retry", || async {
                            Err(MaptileError::InvalidMetadata("broken archive".into()))
                        })
                        .await,
                    Err(MaptileError::InvalidMetadata(_))
                ));
            }
            "cancel" => {
                let mut recovery = Box::pin(manager.recover_unavailable("retry", || {
                    std::future::pending::<MaptileResult<Arc<dyn TileSource>>>()
                }));
                assert!(futures_util::poll!(&mut recovery).is_pending());
                tokio::time::advance(Duration::from_secs(3)).await;
                drop(recovery);
            }
            _ => {
                let task_manager = manager.clone();
                let task = tokio::spawn(async move {
                    if failure == "factory_panic" {
                        task_manager
                            .recover_unavailable(
                                "retry",
                                || -> std::future::Ready<MaptileResult<Arc<dyn TileSource>>> {
                                    panic!("synchronous factory panic");
                                },
                            )
                            .await
                    } else {
                        task_manager
                            .recover_unavailable("retry", || async {
                                panic!("factory future panic");
                            })
                            .await
                    }
                });
                assert!(task.await.is_err_and(|error| error.is_panic()));
            }
        }
        assert!(manager.is_unavailable("retry"), "{failure}");
        assert!(!manager.unavailable_retry_due("retry"), "{failure}");
        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(matches!(
            manager
                .recover_unavailable("retry", || async {
                    Ok(counting_source("retry").0 as Arc<dyn TileSource>)
                })
                .await,
            Err(MaptileError::SourceUnavailable(_))
        ));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(manager.unavailable_retry_due("retry"), "{failure}");
        let recovered = manager
            .recover_unavailable("retry", || async {
                Ok(counting_source("retry").0 as Arc<dyn TileSource>)
            })
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&manager.require("retry").unwrap(), &recovered));
    }
}

#[derive(Clone)]
struct ResponseSource {
    inner: Arc<CountingSource>,
    data: Option<Bytes>,
    cache_zoom: maptile_core::cache::CacheZoomRange,
}

#[async_trait]
impl TileSource for ResponseSource {
    fn supports_bbox(&self) -> bool {
        true
    }
    fn supports_filter(&self) -> bool {
        true
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn tilejson(&self) -> &tilejson::TileJSON {
        self.inner.tilejson()
    }
    fn tile_info(&self) -> TileInfo {
        self.inner.tile_info()
    }
    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }
    fn tile_etag(&self, _: &TileResponse) -> String {
        "\"authoritative\"".into()
    }
    fn cache_zoom(&self) -> maptile_core::cache::CacheZoomRange {
        self.cache_zoom
    }
    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        assert!(
            query.is_none() || query.is_some_and(TileQuery::bypasses_cache),
            "query-less reads must not receive a synthetic cache-control query"
        );
        let mut tile = self.inner.get_tile(xyz, query).await?;
        if let Some(data) = &self.data {
            tile.data = data.clone();
        }
        Ok(tile)
    }
}

#[tokio::test]
async fn manager_coalesces_etag_reads_without_a_cache_control_query() {
    let cache = Arc::new(TileCache::new(1, None, None).unwrap());
    let mgr = SourceManager::new().with_cache(cache.clone());
    let (inner, calls) = counting_source("nested");
    mgr.register(Arc::new(ResponseSource {
        inner,
        data: None,
        cache_zoom: maptile_core::cache::CacheZoomRange::default(),
    }));
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    tokio::time::timeout(Duration::from_secs(1), async {
        let requests = (0..16).map(|_| mgr.get_tile_with_etag("nested", xyz, None, None));
        for response in futures_util::future::join_all(requests).await {
            assert!(matches!(response.unwrap(), TileEtagResponse::Fresh { etag, .. } if etag == "\"authoritative\""));
        }
        assert!(matches!(mgr.get_tile_with_etag("nested", xyz, None,
            Some("\"authoritative\"")).await.unwrap(), TileEtagResponse::NotModified));
    }).await.expect("manager cache fill must complete");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let filter = maptile_core::filter::parse_cql2("population > 0").unwrap();
    for query in [
        TileQuery {
            bbox: Some([-1., -1., 1., 1.]),
            ..Default::default()
        },
        TileQuery {
            filter: Some(&filter),
            ..Default::default()
        },
    ] {
        for _ in 0..2 {
            mgr.get_tile_with_etag("nested", xyz, Some(&query), None)
                .await
                .unwrap();
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "bbox/filter must bypass the manager cache"
    );
}

#[tokio::test]
async fn oversized_tiles_are_rejected_before_hot_cache_insert() {
    let cache = Arc::new(TileCache::new(64, None, None).unwrap());
    let mgr = SourceManager::new().with_cache(cache.clone());
    let (inner, calls) = counting_source("huge");
    mgr.register(Arc::new(ResponseSource {
        inner,
        data: Some(Bytes::from(vec![
            0;
            maptile_core::transport::MAX_DECODED_TILE_BYTES
                + 1
        ])),
        cache_zoom: maptile_core::cache::CacheZoomRange::default(),
    }));
    for _ in 0..2 {
        assert!(
            mgr.get_tile_with_etag("huge", TileCoord { z: 0, x: 0, y: 0 }, None, None)
                .await
                .is_err()
        );
    }
    cache.run_pending_tasks().await;
    assert_eq!(cache.entry_count(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cache_disabled_sources_still_reject_oversized_tiles() {
    let cache = Arc::new(TileCache::new(64, None, None).unwrap());
    let mgr = SourceManager::new().with_cache(cache.clone());
    let (inner, _) = counting_source("huge-live");
    mgr.register(Arc::new(ResponseSource {
        inner,
        data: Some(Bytes::from(vec![
            0;
            maptile_core::transport::MAX_DECODED_TILE_BYTES
                + 1
        ])),
        cache_zoom: maptile_core::cache::CacheZoomRange::disabled(),
    }));
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    assert!(matches!(
        mgr.get_tile_with_etag("huge-live", xyz, None, None).await,
        Err(MaptileError::PayloadTooLarge { .. })
    ));
    assert!(matches!(
        mgr.get_tile("huge-live", xyz, None).await,
        Err(MaptileError::PayloadTooLarge { .. })
    ));
    cache.run_pending_tasks().await;
    assert_eq!(cache.entry_count(), 0);
}

struct CountingMosaicFixture {
    source: Arc<CountingSource>,
    calls: Arc<AtomicUsize>,
    seen_selection: Arc<Mutex<Vec<Option<MosaicPixelSelection>>>>,
}

fn counting_mosaic_source(id: &str, fallback: MosaicPixelSelection) -> CountingMosaicFixture {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen_selection = Arc::new(Mutex::new(Vec::new()));
    let src = Arc::new(CountingSource {
        id: id.into(),
        tj: tilejson! { tiles: vec![] },
        calls: calls.clone(),
        raster_style: true,
        mosaic_selection: Some(fallback),
        seen_selection: seen_selection.clone(),
        on_close: None,
    });
    CountingMosaicFixture {
        source: src,
        calls,
        seen_selection,
    }
}

#[tokio::test]
async fn get_tile_without_cache_calls_source_every_time() {
    let mgr = SourceManager::new();
    let (src, calls) = counting_source("a");
    mgr.register(src);
    for _ in 0..3 {
        mgr.get_tile("a", TileCoord { z: 1, x: 0, y: 0 }, None)
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn get_tile_with_cache_serves_from_cache_on_repeat() {
    let mgr = SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    ));
    let (src, calls) = counting_source("a");
    mgr.register(src);
    for _ in 0..3 {
        mgr.get_tile("a", TileCoord { z: 1, x: 0, y: 0 }, None)
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn get_tile_with_non_style_query_bypasses_cache() {
    let mgr = SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    ));
    let (src, calls) = counting_source("a");
    mgr.register(src);
    let query = TileQuery {
        bbox: Some([-1.0, -1.0, 1.0, 1.0]),
        ..TileQuery::default()
    };
    for _ in 0..3 {
        mgr.get_tile("a", TileCoord { z: 1, x: 0, y: 0 }, Some(&query))
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn get_tile_with_empty_query_still_caches() {
    let mgr = SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    ));
    let (src, calls) = counting_source("a");
    mgr.register(src);
    let query = TileQuery::default();
    for _ in 0..3 {
        mgr.get_tile("a", TileCoord { z: 1, x: 0, y: 0 }, Some(&query))
            .await
            .unwrap();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "empty TileQuery carries no bypass inputs and must hit the cache"
    );
}

#[tokio::test]
async fn different_mosaic_selection_partitions_tile_cache() {
    let mgr = SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    ));
    let CountingMosaicFixture {
        source,
        calls,
        seen_selection,
    } = counting_mosaic_source("a", MosaicPixelSelection::First);
    mgr.register(source);

    for pixel_selection in [MosaicPixelSelection::First, MosaicPixelSelection::Median] {
        let query = TileQuery {
            pixel_selection: Some(pixel_selection),
            ..TileQuery::default()
        };
        mgr.get_tile("a", TileCoord { z: 1, x: 0, y: 0 }, Some(&query))
            .await
            .unwrap();
    }

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *seen_selection.lock().unwrap(),
        vec![
            Some(MosaicPixelSelection::First),
            Some(MosaicPixelSelection::Median)
        ],
        "cache fills must preserve the effective selection"
    );
}

#[tokio::test]
async fn explicit_fallback_selection_shares_implicit_cache_key() {
    let mgr = SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    ));
    let CountingMosaicFixture {
        source,
        calls,
        seen_selection,
    } = counting_mosaic_source("a", MosaicPixelSelection::First);
    mgr.register(source);
    let xyz = TileCoord { z: 1, x: 0, y: 0 };

    mgr.get_tile("a", xyz, None).await.unwrap();
    let explicit = TileQuery {
        pixel_selection: Some(MosaicPixelSelection::First),
        ..TileQuery::default()
    };
    mgr.get_tile("a", xyz, Some(&explicit)).await.unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *seen_selection.lock().unwrap(),
        vec![Some(MosaicPixelSelection::First)]
    );
}

#[tokio::test]
async fn non_mosaic_rejects_selection_before_source_or_cache() {
    let mgr = SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    ));
    let (src, calls) = counting_source("single");
    mgr.register(src);
    let query = TileQuery {
        pixel_selection: Some(MosaicPixelSelection::Median),
        ..TileQuery::default()
    };

    let err = mgr
        .get_tile("single", TileCoord { z: 1, x: 0, y: 0 }, Some(&query))
        .await
        .unwrap_err();

    assert!(matches!(err, MaptileError::Unsupported(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cache_disabled_etag_reads_reject_selection_before_reading_or_matching() {
    let cache = Arc::new(TileCache::new(1, None, None).unwrap());
    let manager = SourceManager::new().with_cache(cache.clone());
    let (inner, calls) = counting_source("live");
    manager.register(Arc::new(ResponseSource {
        inner,
        data: None,
        cache_zoom: maptile_core::cache::CacheZoomRange::disabled(),
    }));
    let xyz = TileCoord { z: 0, x: 0, y: 0 };
    let query = TileQuery {
        pixel_selection: Some(MosaicPixelSelection::Median),
        ..Default::default()
    };
    assert!(matches!(
        manager.get_tile("live", xyz, Some(&query)).await,
        Err(MaptileError::Unsupported(_))
    ));
    for etag in [None, Some("\"authoritative\"")] {
        assert!(matches!(
            manager
                .get_tile_with_etag("live", xyz, Some(&query), etag)
                .await,
            Err(MaptileError::Unsupported(_))
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        manager
            .get_tile_with_etag("live", xyz, None, None)
            .await
            .unwrap(),
        TileEtagResponse::Fresh { .. }
    ));
    assert!(matches!(
        manager
            .get_tile_with_etag("live", xyz, None, Some("\"authoritative\""))
            .await
            .unwrap(),
        TileEtagResponse::NotModified
    ));
}

#[tokio::test]
async fn styled_query_partitions_cache_by_style_hash() {
    let mgr = SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    ));
    let (mut src, calls) = counting_source("a");
    Arc::get_mut(&mut src).unwrap().raster_style = true;
    mgr.register(src);

    let style_a = complete_style(
        serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "pseudocolor", "bidx": vec![1], "colormap_name": "viridis"}, "raster:tile": {"format": "webp", "tilesize": 512, "lossless": true}, "resampling": {"read": "bilinear", "reproject": "bilinear"}}),
    );
    let style_b = complete_style(
        serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "pseudocolor", "bidx": vec![1], "colormap_name": "plasma"}, "raster:tile": {"format": "webp", "tilesize": 512, "lossless": true}, "resampling": {"read": "bilinear", "reproject": "bilinear"}}),
    );
    assert_ne!(style_a.style_hash(), style_b.style_hash());

    // Same style twice: 1 source call (cached on the second hit).
    let q_a = TileQuery {
        style: Some(&style_a),
        ..TileQuery::default()
    };
    for _ in 0..2 {
        mgr.get_tile("a", TileCoord { z: 1, x: 0, y: 0 }, Some(&q_a))
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Different style for the same tile: distinct cache key, +1 call.
    let q_b = TileQuery {
        style: Some(&style_b),
        ..TileQuery::default()
    };
    mgr.get_tile("a", TileCoord { z: 1, x: 0, y: 0 }, Some(&q_b))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn get_tile_unknown_source_errors() {
    let mgr = SourceManager::new();
    let err = mgr
        .get_tile("nope", TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap_err();
    assert!(matches!(err, MaptileError::SourceNotFound(_)));
}

#[test]
fn register_composite_from_ids_builds_and_registers() {
    let mgr = SourceManager::new();
    let (a, _) = counting_source("a");
    let (b, _) = counting_source("b");
    mgr.register(a);
    mgr.register(b);

    mgr.register_composite_from_ids("ab", &["a", "b"]).unwrap();
    let composite = mgr.get("ab").expect("composite registered");
    assert_eq!(composite.id(), "ab");
    // Composite sources advertise Mvt.
    assert_eq!(
        composite.tile_info().format,
        maptile_core::sources::TileFormat::Mvt
    );
}

#[test]
fn register_composite_from_ids_unknown_child_is_source_not_found() {
    let mgr = SourceManager::new();
    let (a, _) = counting_source("a");
    mgr.register(a);
    let err = mgr
        .register_composite_from_ids("ab", &["a", "ghost"])
        .unwrap_err();
    assert!(matches!(err, MaptileError::SourceNotFound(_)));
}

#[test]
fn register_composite_from_ids_empty_list_is_config_error() {
    let mgr = SourceManager::new();
    let err = mgr.register_composite_from_ids("ab", &[]).unwrap_err();
    assert!(matches!(err, MaptileError::Config(_)));
}

#[tokio::test]
async fn reload_from_discovery_adds_and_removes_sources() {
    use maptile_core::discovery::{DiscoveredSource, DiscoveredSourceKind, DiscoveryReport};
    use std::path::PathBuf;

    let mgr = SourceManager::new();
    let (existing, _) = counting_source("keeper");
    let (going, _) = counting_source("going");
    mgr.register(existing);
    mgr.register(going);

    // Target report: keeper survives, going disappears, newcomer added.
    let report = DiscoveryReport {
        root: PathBuf::from("/tmp/fake-root"),
        sources: vec![
            DiscoveredSource {
                id: "keeper".into(),
                kind: DiscoveredSourceKind::PmTiles,
                path: PathBuf::from("/tmp/fake-root/keeper.pmtiles"),
            },
            DiscoveredSource {
                id: "newcomer".into(),
                kind: DiscoveredSourceKind::PmTiles,
                path: PathBuf::from("/tmp/fake-root/newcomer.pmtiles"),
            },
        ],
        styles: vec![],
    };

    let delta = mgr
        .reload_from_discovery(&report, |s| {
            let id = s.id.clone();
            async move {
                let (src, _) = counting_source(id.as_str());
                Ok(src as Arc<dyn TileSource>)
            }
        })
        .await
        .unwrap();
    assert_eq!(delta.added, vec!["newcomer".to_string()]);
    assert_eq!(delta.removed, vec!["going".to_string()]);
    assert!(mgr.get("keeper").is_some());
    assert!(mgr.get("newcomer").is_some());
    assert!(mgr.get("going").is_none());
}

fn closing_source(id: &str) -> (Arc<CountingSource>, Arc<AtomicUsize>) {
    let (mut source, _) = counting_source(id);
    let closed = Arc::new(AtomicUsize::new(0));
    let counter = closed.clone();
    Arc::get_mut(&mut source).unwrap().on_close = Some(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    (source, closed)
}

fn discovery_report(ids: &[&str]) -> maptile_core::discovery::DiscoveryReport {
    maptile_core::discovery::DiscoveryReport {
        sources: ids
            .iter()
            .map(|id| maptile_core::discovery::DiscoveredSource {
                id: (*id).into(),
                kind: maptile_core::discovery::DiscoveredSourceKind::PmTiles,
                path: format!("/tmp/{id}.pmtiles").into(),
            })
            .collect(),
        ..Default::default()
    }
}

#[tokio::test(start_paused = true)]
async fn wrong_recovery_id_closes_only_unregistered_outputs_without_holding_locks() {
    for registered_alias in [false, true] {
        let manager = Arc::new(SourceManager::new());
        manager.mark_unavailable("retry");
        tokio::time::advance(Duration::from_secs(1)).await;
        let (mut output, closed) = closing_source("other");
        let source = Arc::get_mut(&mut output).unwrap();
        let close = source.on_close.take().unwrap();
        let weak_manager = Arc::downgrade(&manager);
        source.on_close = Some(Arc::new(move || {
            close();
            let manager = weak_manager.upgrade().unwrap();
            assert!(manager.is_unavailable("retry"));
            manager.mark_unavailable("retry");
        }));
        let output: Arc<dyn TileSource> = output;
        if registered_alias {
            manager.register(output.clone());
        }

        assert!(matches!(
            manager
                .recover_unavailable("retry", || async { Ok(output.clone()) })
                .await,
            Err(MaptileError::Config(_))
        ));
        assert_eq!(
            closed.load(Ordering::SeqCst),
            usize::from(!registered_alias)
        );
        if registered_alias {
            assert!(Arc::ptr_eq(&manager.require("other").unwrap(), &output));
        }
        assert!(!manager.unavailable_retry_due("retry"));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(manager.unavailable_retry_due("retry"));
    }
}

#[tokio::test(start_paused = true)]
async fn obsolete_recovery_results_resolve_authoritative_registration_removal_and_reload() {
    use tokio::sync::oneshot;

    for change in ["register", "remove", "reload", "reload_remove"] {
        for completion in ["success", "error", "wrong_id"] {
            let manager = SourceManager::new();
            manager.mark_unavailable("retry");
            tokio::time::advance(Duration::from_secs(1)).await;
            let (finish_tx, finish_rx) = oneshot::channel();
            let mut recovery = Box::pin(
                manager.recover_unavailable("retry", || async { finish_rx.await.unwrap() }),
            );
            assert!(futures_util::poll!(&mut recovery).is_pending());
            let (current, current_closed) = closing_source("retry");
            let current: Arc<dyn TileSource> = current;
            match change {
                "register" => manager.register(current.clone()),
                "remove" => {
                    manager.remove("retry");
                }
                _ => {
                    let ids = if change == "reload" {
                        vec!["retry"]
                    } else {
                        vec![]
                    };
                    manager
                        .reload_from_discovery(&discovery_report(&ids), |_| {
                            std::future::ready(Ok(current.clone()))
                        })
                        .await
                        .unwrap();
                }
            }
            let id = if completion == "wrong_id" {
                "other"
            } else {
                "retry"
            };
            let (output, output_closed) = closing_source(id);
            let result = if completion == "error" {
                Err(MaptileError::InvalidMetadata("obsolete failure".into()))
            } else {
                Ok(output as Arc<dyn TileSource>)
            };
            assert!(finish_tx.send(result).is_ok());
            let result = recovery.await;
            if matches!(change, "register" | "reload") {
                assert!(Arc::ptr_eq(&result.unwrap(), &current));
                assert!(Arc::ptr_eq(&manager.require("retry").unwrap(), &current));
            } else {
                assert!(matches!(result, Err(MaptileError::SourceNotFound(_))));
                assert!(manager.get("retry").is_none());
                assert!(!manager.is_unavailable("retry"));
            }
            assert_eq!(
                output_closed.load(Ordering::SeqCst),
                usize::from(completion != "error")
            );
            assert_eq!(current_closed.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn obsolete_success_keeps_the_same_arc_registered_by_another_writer_open() {
    use tokio::sync::oneshot;

    let manager = SourceManager::new();
    manager.mark_unavailable("retry");
    tokio::time::advance(Duration::from_secs(1)).await;
    let (finish_tx, finish_rx) = oneshot::channel();
    let mut recovery =
        Box::pin(manager.recover_unavailable("retry", || async { finish_rx.await.unwrap() }));
    assert!(futures_util::poll!(&mut recovery).is_pending());
    let (output, closed) = closing_source("retry");
    let output: Arc<dyn TileSource> = output;
    manager.register(output.clone());
    assert!(finish_tx.send(Ok(output.clone())).is_ok());

    assert!(Arc::ptr_eq(&recovery.await.unwrap(), &output));
    assert!(Arc::ptr_eq(&manager.require("retry").unwrap(), &output));
    assert_eq!(closed.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn obsolete_completion_or_cancellation_cannot_release_a_new_recovery() {
    use tokio::sync::oneshot;

    for replace in [false, true] {
        for completion in ["success", "error", "cancel"] {
            let manager = SourceManager::new();
            manager.mark_unavailable("retry");
            tokio::time::advance(Duration::from_secs(1)).await;
            let (old_tx, old_rx) = oneshot::channel();
            let mut old =
                Box::pin(manager.recover_unavailable("retry", || async { old_rx.await.unwrap() }));
            assert!(futures_util::poll!(&mut old).is_pending());
            if replace {
                manager.register(counting_source("retry").0);
            } else {
                manager.remove("retry");
            }
            manager.mark_unavailable("retry");
            tokio::time::advance(Duration::from_secs(1)).await;
            let (new_tx, new_rx) = oneshot::channel();
            let mut new =
                Box::pin(manager.recover_unavailable("retry", || async { new_rx.await.unwrap() }));
            assert!(futures_util::poll!(&mut new).is_pending());
            let (output, closed) = closing_source("retry");
            if completion == "cancel" {
                drop(old);
            } else {
                let result = if completion == "error" {
                    Err(MaptileError::InvalidMetadata("old failure".into()))
                } else {
                    Ok(output as Arc<dyn TileSource>)
                };
                assert!(old_tx.send(result).is_ok());
                assert!(matches!(old.await, Err(MaptileError::SourceUnavailable(_))));
            }
            assert_eq!(
                closed.load(Ordering::SeqCst),
                usize::from(completion == "success")
            );
            assert!(!manager.unavailable_retry_due("retry"));
            tokio::time::advance(Duration::from_secs(2)).await;
            assert!(matches!(
                manager
                    .recover_unavailable("retry", || async {
                        Ok(counting_source("retry").0 as Arc<dyn TileSource>)
                    })
                    .await,
                Err(MaptileError::SourceUnavailable(_))
            ));
            let recovered: Arc<dyn TileSource> = counting_source("retry").0;
            assert!(new_tx.send(Ok(recovered.clone())).is_ok());
            assert!(Arc::ptr_eq(&new.await.unwrap(), &recovered));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn obsolete_guard_drop_does_not_reset_a_new_failure_deadline() {
    let manager = SourceManager::new();
    manager.mark_unavailable("retry");
    tokio::time::advance(Duration::from_secs(1)).await;
    let mut old = Box::pin(manager.recover_unavailable("retry", || {
        std::future::pending::<MaptileResult<Arc<dyn TileSource>>>()
    }));
    assert!(futures_util::poll!(&mut old).is_pending());
    manager.remove("retry");
    manager.mark_unavailable("retry");
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        manager
            .recover_unavailable("retry", || async {
                Err(MaptileError::InvalidMetadata("new failure".into()))
            })
            .await,
        Err(MaptileError::InvalidMetadata(_))
    ));
    tokio::time::advance(Duration::from_millis(500)).await;
    drop(old);
    tokio::time::advance(Duration::from_millis(500)).await;
    assert!(manager.unavailable_retry_due("retry"));
}

#[tokio::test]
async fn reload_removes_absent_unavailable_sources_and_rebuilds_present_ones() {
    let manager = SourceManager::new();
    manager.mark_unavailable("gone");
    manager.mark_unavailable("retry");
    let (recovered, _) = counting_source("retry");

    let delta = manager
        .reload_from_discovery(&discovery_report(&["retry"]), |source| {
            assert_eq!(source.id, "retry");
            let recovered = recovered.clone();
            async move { Ok(recovered as Arc<dyn TileSource>) }
        })
        .await
        .unwrap();

    assert_eq!(delta.added, ["retry"]);
    assert_eq!(delta.removed, ["gone"]);
    assert!(matches!(
        manager.require("gone"),
        Err(MaptileError::SourceNotFound(id)) if id == "gone"
    ));
    assert!(!manager.is_unavailable("gone"));
    assert!(!manager.unavailable_retry_due("gone"));
    assert!(!manager.is_unavailable("retry"));
    let recovered: Arc<dyn TileSource> = recovered;
    assert!(Arc::ptr_eq(&manager.require("retry").unwrap(), &recovered));
}

#[tokio::test]
async fn duplicate_discovered_ids_fail_before_factory_calls_or_registry_changes() {
    for duplicate in ["new", "ready", "unavailable"] {
        let manager = SourceManager::new();
        let (old, closed) = closing_source("ready");
        manager.register(old.clone());
        manager.mark_unavailable("unavailable");
        manager.mark_unavailable("obsolete");
        let mut report = discovery_report(&["fresh", duplicate, duplicate]);
        report.sources.last_mut().unwrap().path = "/tmp/another-file.pmtiles".into();
        let factory_calls = AtomicUsize::new(0);
        let result = manager
            .reload_from_discovery(&report, |_| {
                factory_calls.fetch_add(1, Ordering::SeqCst);
                async { Err(MaptileError::Config("factory must not be called".into())) }
            })
            .await;
        assert!(matches!(
            result,
            Err(MaptileError::Config(message))
                if message == format!("duplicate discovered source id: {duplicate}")
        ));
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
        let old: Arc<dyn TileSource> = old;
        assert!(Arc::ptr_eq(&manager.require("ready").unwrap(), &old));
        assert_eq!(closed.load(Ordering::SeqCst), 0);
        assert_eq!(manager.list_ids(), ["ready"]);
        assert!(manager.is_unavailable("unavailable"));
        assert!(manager.is_unavailable("obsolete"));
    }
}

#[tokio::test]
async fn mismatched_factory_id_keeps_registry_and_closes_every_unpublished_source() {
    let manager = SourceManager::new();
    let (old, old_closed) = closing_source("wrong");
    manager.register(old.clone());
    manager.mark_unavailable("requested");
    let (good, good_closed) = closing_source("good");
    let (wrong, wrong_closed) = closing_source("wrong");
    let result = manager
        .reload_from_discovery(&discovery_report(&["good", "requested"]), |source| {
            let built = if source.id == "good" {
                good.clone()
            } else {
                wrong.clone()
            };
            async move { Ok(built as Arc<dyn TileSource>) }
        })
        .await;
    assert!(matches!(result, Err(MaptileError::Config(_))));
    let old: Arc<dyn TileSource> = old;
    assert!(Arc::ptr_eq(&manager.require("wrong").unwrap(), &old));
    assert!(manager.is_unavailable("requested"));
    assert!(manager.get("good").is_none());
    assert_eq!(old_closed.load(Ordering::SeqCst), 0);
    assert_eq!(good_closed.load(Ordering::SeqCst), 1);
    assert_eq!(wrong_closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mismatched_factory_id_does_not_close_a_registered_instance() {
    let manager = SourceManager::new();
    let (old, old_closed) = closing_source("old");
    manager.register(old.clone());
    let (good, good_closed) = closing_source("good");
    let result = manager
        .reload_from_discovery(&discovery_report(&["good", "requested"]), |source| {
            let built = if source.id == "good" {
                good.clone()
            } else {
                old.clone()
            };
            async move { Ok(built as Arc<dyn TileSource>) }
        })
        .await;
    assert!(matches!(result, Err(MaptileError::Config(_))));
    let old: Arc<dyn TileSource> = old;
    assert!(Arc::ptr_eq(&manager.require("old").unwrap(), &old));
    assert_eq!(old_closed.load(Ordering::SeqCst), 0);
    assert_eq!(good_closed.load(Ordering::SeqCst), 1);
    assert!(manager.get("good").is_none());
    assert!(manager.get("requested").is_none());
}

#[tokio::test]
async fn failed_reload_keeps_registry_and_closes_unpublished_sources() {
    let manager = SourceManager::new();
    let (old, old_closed) = closing_source("obsolete");
    manager.register(old);
    manager.mark_unavailable("good");
    manager.mark_unavailable("gone");
    let (good, good_closed) = closing_source("good");
    let error = manager
        .reload_from_discovery(&discovery_report(&["good", "bad"]), |source| {
            let good = good.clone();
            let fail = source.id == "bad";
            async move {
                if fail {
                    Err(MaptileError::InvalidMetadata("broken archive".into()))
                } else {
                    Ok(good as Arc<dyn TileSource>)
                }
            }
        })
        .await
        .unwrap_err();
    assert!(matches!(error, MaptileError::InvalidMetadata(_)));
    assert_eq!(manager.list_ids(), ["obsolete"]);
    assert!(manager.is_unavailable("good"));
    assert!(manager.is_unavailable("gone"));
    assert_eq!(old_closed.load(Ordering::SeqCst), 0);
    assert_eq!(good_closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_reload_closes_unpublished_sources_without_applying_changes() {
    let manager = SourceManager::new();
    let (old, old_closed) = closing_source("obsolete");
    manager.register(old);
    manager.mark_unavailable("gone");
    let (good, good_closed) = closing_source("good");
    let report = discovery_report(&["good", "pending"]);
    {
        let mut reload = Box::pin(manager.reload_from_discovery(&report, |source| {
            let good = good.clone();
            let pending = source.id == "pending";
            async move {
                if pending {
                    std::future::pending::<()>().await;
                }
                Ok(good as Arc<dyn TileSource>)
            }
        }));
        assert!(futures_util::poll!(&mut reload).is_pending());
        assert_eq!(manager.list_ids(), ["obsolete"]);
    }
    assert_eq!(manager.list_ids(), ["obsolete"]);
    assert!(manager.is_unavailable("gone"));
    assert_eq!(old_closed.load(Ordering::SeqCst), 0);
    assert_eq!(good_closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn get_tile_skips_cache_outside_zoom_range() {
    use maptile_core::cache::CacheZoomRange;

    // CountingSource that overrides cache_zoom() to z 2..=10
    struct ZoomGated {
        inner: CountingSource,
        range: CacheZoomRange,
    }
    #[async_trait]
    impl TileSource for ZoomGated {
        fn id(&self) -> &str {
            self.inner.id()
        }
        fn tilejson(&self) -> &tilejson::TileJSON {
            self.inner.tilejson()
        }
        fn tile_info(&self) -> TileInfo {
            self.inner.tile_info()
        }
        fn clone_source(&self) -> Box<dyn TileSource> {
            self.inner.clone_source()
        }
        async fn get_tile(
            &self,
            xyz: TileCoord,
            query: Option<&TileQuery<'_>>,
        ) -> MaptileResult<TileResponse> {
            self.inner.get_tile(xyz, query).await
        }
        fn cache_zoom(&self) -> CacheZoomRange {
            self.range
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let inner = CountingSource {
        id: "z".into(),
        tj: tilejson::tilejson! { tiles: vec![] },
        calls: calls.clone(),
        raster_style: false,
        mosaic_selection: None,
        seen_selection: Arc::new(Mutex::new(Vec::new())),
        on_close: None,
    };
    let gated = ZoomGated {
        inner,
        range: CacheZoomRange::new(Some(2), Some(10)),
    };
    let cache = Arc::new(TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap());
    let mgr = SourceManager::new().with_cache(cache.clone());
    mgr.register(Arc::new(gated));

    // z=1 is outside the range — should hit the source every time
    // and never write to the cache.
    for _ in 0..3 {
        mgr.get_tile("z", TileCoord { z: 1, x: 0, y: 0 }, None)
            .await
            .unwrap();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "out-of-range zoom must not be cached"
    );
    assert_eq!(
        cache.entry_count(),
        0,
        "cache must remain empty for out-of-range zoom"
    );

    // z=5 is in range — first call hits source, next two come from cache.
    for _ in 0..3 {
        mgr.get_tile("z", TileCoord { z: 5, x: 0, y: 0 }, None)
            .await
            .unwrap();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "in-range zoom should populate the cache after first miss \
         (3 requests, only 1 source call ⇒ 2 cache hits)"
    );

    // z=11 is above the upper bound — must bypass.
    for _ in 0..2 {
        mgr.get_tile("z", TileCoord { z: 11, x: 0, y: 0 }, None)
            .await
            .unwrap();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        6,
        "above-range zoom must bypass cache \
         (2 requests ⇒ 2 fresh source calls)"
    );
}

#[tokio::test]
async fn get_tile_dedups_concurrent_cache_miss() {
    use std::time::Duration;

    let mgr = Arc::new(SourceManager::new().with_cache(Arc::new(
        TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
    )));
    let (src, calls) = counting_source("dedup");
    // Wrap CountingSource so its get_tile is artificially slow.
    struct SlowSource {
        inner: Arc<CountingSource>,
    }
    #[async_trait]
    impl TileSource for SlowSource {
        fn id(&self) -> &str {
            self.inner.id()
        }
        fn tilejson(&self) -> &tilejson::TileJSON {
            self.inner.tilejson()
        }
        fn tile_info(&self) -> TileInfo {
            self.inner.tile_info()
        }
        fn clone_source(&self) -> Box<dyn TileSource> {
            self.inner.clone_source()
        }
        async fn get_tile(
            &self,
            xyz: TileCoord,
            query: Option<&TileQuery<'_>>,
        ) -> MaptileResult<TileResponse> {
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.inner.get_tile(xyz, query).await
        }
    }
    mgr.register(Arc::new(SlowSource { inner: src }));

    let mut handles = Vec::with_capacity(16);
    for _ in 0..16 {
        let mgr = mgr.clone();
        handles.push(tokio::spawn(async move {
            mgr.get_tile("dedup", TileCoord { z: 5, x: 0, y: 0 }, None)
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "16 concurrent cache-miss requests must dedupe to a single source call"
    );
}

#[tokio::test]
async fn snapshot_catalog_with_no_stores_only_lists_sources() {
    let mgr = SourceManager::new();
    let (a, _) = counting_source("zeta");
    let (b, _) = counting_source("alpha");
    mgr.register(a);
    mgr.register(b);

    let cat = mgr.snapshot_catalog(None, None).await.unwrap();
    assert_eq!(cat.sources.len(), 2);
    // Sources are sorted by id for stable diff.
    assert_eq!(cat.sources[0].id, "alpha");
    assert_eq!(cat.sources[1].id, "zeta");
    assert!(cat.styles.is_empty());
}

#[cfg(all(feature = "mbtiles", feature = "pmtiles"))]
mod manager_interop {
    use maptile_core::sources::manager::SourceManager;
    use maptile_core::sources::mbtiles::{MbtilesConfig, MbtilesSource};
    use maptile_core::sources::pmtiles::{LocalPmtilesSource, PmtilesConfig};
    use maptile_core::sources::{TileCoord, TileFormat};
    use std::sync::Arc;

    #[tokio::test]
    async fn co_registered_sources_serve_independently() {
        let fixtures = tempfile::tempdir().unwrap();
        let mbtiles_path = fixtures.path().join("sample.mbtiles");
        let pmtiles_path = fixtures.path().join("sample.pmtiles");
        let connection = rusqlite::Connection::open(&mbtiles_path).unwrap();
        connection.execute_batch(
        "CREATE TABLE metadata (name TEXT, value TEXT);
         CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB);
         INSERT INTO metadata VALUES ('name','sample'),('format','png'),('minzoom','0'),('maxzoom','0'),('bounds','-180,-85,180,85'),('tile_size','256');
"
    ).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(256, 256)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        connection
            .execute(
                "INSERT INTO tiles VALUES (0,0,0,?1)",
                rusqlite::params![png.into_inner()],
            )
            .unwrap();
        drop(connection);
        let mut writer = pmtiles::PmTilesWriter::new(pmtiles::TileType::Mvt)
            .tile_compression(pmtiles::Compression::None)
            .internal_compression(pmtiles::Compression::None)
            .metadata(r#"{"name":"sample","vector_layers":[{"id":"layer","fields":{}}]}"#)
            .create(std::fs::File::create(&pmtiles_path).unwrap())
            .unwrap();
        writer
            .add_tile(pmtiles::TileCoord::new(0, 0, 0).unwrap(), b"test-tile-data")
            .unwrap();
        writer.finalize().unwrap();

        let mb = MbtilesSource::open(MbtilesConfig {
            id: "m".into(),
            path: mbtiles_path,
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();

        let pm = LocalPmtilesSource::open(PmtilesConfig {
            id: "p".into(),
            path: pmtiles_path.to_string_lossy().into_owned(),
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();

        let mgr = SourceManager::new();
        mgr.register(Arc::new(mb));
        mgr.register(Arc::new(pm));

        let mut ids = mgr.list_ids();
        ids.sort();
        assert_eq!(ids, vec!["m", "p"]);

        let mb_handle = mgr.get("m").unwrap();
        assert_eq!(mb_handle.tile_info().format, TileFormat::Png);
        let mb_resp = mb_handle
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert!(!mb_resp.data.is_empty());

        let pm_handle = mgr.get("p").unwrap();
        assert_eq!(pm_handle.tile_info().format, TileFormat::Mvt);
        let pm_resp = pm_handle
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert!(!pm_resp.data.is_empty());

        assert_eq!(mgr.snapshot_tilejson().len(), 2);
    }
}
