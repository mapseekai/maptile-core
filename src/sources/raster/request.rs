use std::{
    future::Future,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_geotiff::{
    error::{AgError, AgResult},
    request::{DEFAULT_MAX_IN_FLIGHT_REQUESTS, RequestLimiter},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::task::AbortOnDropHandle;

use crate::control::{SourceReadLease, cleanup_runtime};
use crate::error::{MaptileError, MaptileResult};
use crate::transport::SOURCE_READ_LEASE;

static REQUESTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

tokio::task_local! {
    pub(super) static ACTIVE_RASTER_REQUEST: Arc<ActiveRasterRequest>;
}

pub(super) struct ActiveRasterRequest {
    _permit: OwnedSemaphorePermit,
    lease: Option<Arc<SourceReadLease>>,
    raw_state: Option<AtomicU8>,
}

const READING_OBJECT: u8 = 1;
const RECEIVED_BYTES: u8 = 2;
const UNJOINED_DECODE: u8 = 4;

impl ActiveRasterRequest {
    fn admit(raw: bool) -> MaptileResult<Arc<Self>> {
        let permit = REQUESTS
            .get_or_init(|| Arc::new(Semaphore::new(DEFAULT_MAX_IN_FLIGHT_REQUESTS)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| MaptileError::CapacityExhausted)?;
        Ok(Arc::new(Self {
            _permit: permit,
            lease: SOURCE_READ_LEASE.try_with(Arc::clone).ok(),
            raw_state: raw.then(|| AtomicU8::new(0)),
        }))
    }

    pub(super) fn object_read() -> Option<Arc<Self>> {
        let active = ACTIVE_RASTER_REQUEST.try_with(Arc::clone).ok()?;
        let previous = active
            .raw_state
            .as_ref()?
            .fetch_or(READING_OBJECT, Ordering::Relaxed);
        (previous & RECEIVED_BYTES == 0).then_some(active)
    }

    pub(super) fn received_bytes(&self) {
        if let Some(state) = &self.raw_state {
            state.fetch_or(RECEIVED_BYTES, Ordering::Relaxed);
        }
    }

    fn raw_state(&self) -> u8 {
        self.raw_state
            .as_ref()
            .map_or(0, |state| state.load(Ordering::Relaxed))
    }

    fn waiting_for_first_bytes(&self) -> bool {
        self.raw_state() & (READING_OBJECT | RECEIVED_BYTES) == READING_OBJECT
    }
}

pub(super) async fn render<T, F>(work: impl FnOnce(RequestLimiter) -> F) -> MaptileResult<T>
where
    F: Future<Output = AgResult<T>>,
{
    let limiter = RequestLimiter::new(1);
    let request = NativeRequest::new(NativeWork::Render(limiter.clone()))?;
    ACTIVE_RASTER_REQUEST
        .scope(request.active.clone(), work(limiter))
        .await
        .map_err(crate::raster::agtiff_style::render_error)
}

pub(super) struct ReaderJobs {
    cpu: async_geotiff::CpuLimiter,
    capacity: usize,
    draining: AtomicUsize,
}

impl ReaderJobs {
    pub(super) fn new(cpu: async_geotiff::CpuLimiter) -> Arc<Self> {
        Arc::new(Self {
            capacity: cpu.available_permits(),
            cpu,
            draining: AtomicUsize::new(0),
        })
    }

    pub(super) fn admit(&self) -> MaptileResult<()> {
        if self.draining.load(Ordering::Acquire) == 0 {
            Ok(())
        } else {
            Err(MaptileError::CapacityExhausted)
        }
    }
}

pub(super) async fn reader_call<T: Send + 'static>(
    jobs: Arc<ReaderJobs>,
    work: impl Future<Output = MaptileResult<T>> + Send + 'static,
) -> MaptileResult<T> {
    jobs.admit()?;
    NativeRequest::new(NativeWork::Reader(jobs))?
        .read(work)
        .await
}

pub(super) fn mark_reader_error(error: AgError) -> AgError {
    if matches!(error, AgError::Tiff(_) | AgError::ResourceLimit(_)) {
        // A failed raw window can abort sibling decode futures without joining their work.
        let _ = ACTIVE_RASTER_REQUEST.try_with(|active| {
            if active.waiting_for_first_bytes() {
                return;
            }
            if let Some(state) = &active.raw_state {
                state.fetch_or(UNJOINED_DECODE, Ordering::Relaxed);
            }
            if let Some(lease) = &active.lease {
                lease.mark_unconfirmed();
            }
        });
    }
    error
}

enum NativeWork {
    Render(RequestLimiter),
    Reader(Arc<ReaderJobs>),
}

impl NativeWork {
    fn busy(&self) -> bool {
        match self {
            Self::Render(limiter) => limiter.available_permits() == 0,
            // Raw reads expose only the CPU pool of the opened reader.
            Self::Reader(jobs) => jobs.cpu.available_permits() < jobs.capacity,
        }
    }
}

struct NativeRequest {
    active: Arc<ActiveRasterRequest>,
    cleanup: tokio::runtime::Handle,
    work: Option<NativeWork>,
}

impl NativeRequest {
    fn new(work: NativeWork) -> MaptileResult<Self> {
        Ok(Self {
            active: ActiveRasterRequest::admit(matches!(work, NativeWork::Reader(_)))?,
            cleanup: cleanup_runtime()?.clone(),
            work: Some(work),
        })
    }

    async fn read<T: Send + 'static>(
        mut self,
        work: impl Future<Output = MaptileResult<T>> + Send + 'static,
    ) -> MaptileResult<T> {
        let scheduler = self.cleanup.clone();
        let native_work = self.work.take();
        // A single scheduler worker orders dropping the raw future's JoinSet with any
        // decoder child that could still submit a blocking job and acquire a CPU permit.
        AbortOnDropHandle::new(scheduler.spawn(async move {
            let mut request = self;
            request.work = native_work;
            let read = ACTIVE_RASTER_REQUEST.scope(request.active.clone(), work);
            let result = match &request.active.lease {
                Some(lease) => SOURCE_READ_LEASE.scope(lease.clone(), read).await,
                None => read.await,
            };
            if request.active.raw_state() & UNJOINED_DECODE == 0 {
                request.work = None;
            }
            drop(request);
            result
        }))
        .await
        .map_err(MaptileError::backend)?
    }
}

impl Drop for NativeRequest {
    fn drop(&mut self) {
        // Before the first object bytes arrive this raw call cannot have submitted a block decode.
        if self.active.waiting_for_first_bytes() {
            return;
        }
        let Some(work) = self.work.take().filter(NativeWork::busy) else {
            return;
        };
        if let NativeWork::Reader(jobs) = &work {
            // New work on this reader must not indefinitely postpone observing its CPU pool idle.
            jobs.draining.fetch_add(1, Ordering::AcqRel);
        }
        let active = self.active.clone();
        self.cleanup.spawn(async move {
            let _active = active;
            // async-geotiff 0.13 exposes permit counts but no completion notification.
            while work.busy() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if let NativeWork::Reader(jobs) = work {
                jobs.draining.fetch_sub(1, Ordering::Release);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cache::{CacheZoomRange, TileCache},
        control::CatalogService,
        raster::style::{EffectiveStyle, RasterStyle},
        service::{ReadTarget, TileStack},
        sources::{AsyncCogSource, AsyncCogSourceConfig, CogObjectSource, TileCoord, TileQuery},
    };
    use bytes::Bytes;
    use object_store::{ObjectStoreExt, memory::InMemory, path::Path};

    fn single_worker_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
    }

    async fn cog_fixture() -> AsyncCogSource {
        let store = Arc::new(InMemory::new());
        let key = Path::from("tile.tif");
        let data = Bytes::from_static(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cog_uint8_rgb.tif"
        )));
        store.put(&key, data.into()).await.unwrap();
        AsyncCogSource::open(AsyncCogSourceConfig {
            id: "native-cog".into(),
            source: CogObjectSource::ObjectStore {
                store,
                key: key.to_string(),
            },
            name: None,
            description: None,
            attribution: None,
            minzoom: None,
            maxzoom: None,
            bounds: None,
            cache_zoom: Some(CacheZoomRange::disabled()),
            content_id: "native-drain-test".into(),
            block_cache: None,
        })
        .await
        .unwrap()
    }

    async fn catalog_fixture() -> (Arc<CatalogService>, TileStack, async_geotiff::CpuLimiter) {
        let source = cog_fixture().await;
        let cpu = source.reader().cpu_limiter();
        let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
        let mut update = engine.catalog().update();
        update.put(Arc::new(source)).unwrap();
        engine.catalog().publish(update).unwrap();
        let stack = TileStack::builder(engine.clone()).build();
        (engine, stack, cpu)
    }

    async fn read_origin(
        stack: &TileStack,
        style: &EffectiveStyle,
    ) -> MaptileResult<crate::service::TileOutcome> {
        let query = TileQuery {
            style: Some(style),
            ..Default::default()
        };
        let target = ReadTarget {
            query: Some(&query),
            ..ReadTarget::new("native-cog", "public")
        };
        stack.read(target.at(TileCoord { z: 0, x: 0, y: 0 })).await
    }

    async fn block_worker() -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        started.await.unwrap();
        (release, blocker)
    }

    fn rgb_style() -> EffectiveStyle {
        EffectiveStyle::from_raster_style(
            RasterStyle::from_value(serde_json::json!({
                "extensions": [raster_style::TILE_SCHEMA_URL],
                "version": "3.0",
                "renderer": {"type": "gray", "bidx": [1]},
                "raster:tile": {"format": "png", "tilesize": 256}
            }))
            .unwrap(),
        )
        .unwrap()
    }

    async fn submit_native_read(
        read: std::pin::Pin<&mut (impl Future + ?Sized)>,
        cpu: &async_geotiff::CpuLimiter,
    ) {
        let capacity = cpu.available_permits();
        let submitted = async {
            while cpu.available_permits() == capacity {
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            _ = read => panic!("read finished before submitting native work"),
            result = tokio::time::timeout(Duration::from_secs(5), submitted) => result.unwrap(),
        }
    }

    #[test]
    fn revoked_cog_read_holds_the_lease_until_native_work_stops() {
        single_worker_runtime().block_on(async {
            let (engine, stack, cpu) = catalog_fixture().await;
            let style = rgb_style();
            let (release, blocker) = block_worker().await;
            let mut read = Box::pin(read_origin(&stack, &style));
            submit_native_read(read.as_mut(), &cpu).await;
            let snapshot = engine.catalog().snapshot();
            let mut revoke = Box::pin(engine.catalog().revoke(
                &snapshot,
                "native-cog",
                tokio::time::Instant::now() + Duration::from_secs(5),
            ));
            assert!(futures_util::poll!(revoke.as_mut()).is_pending());
            assert!(matches!(read.await, Err(MaptileError::Cancelled)));
            let polled = futures_util::poll!(revoke.as_mut());
            release.send(()).unwrap();
            assert!(
                polled.is_pending(),
                "revocation returned {polled:?} while native work was queued"
            );
            blocker.await.unwrap();
            revoke.await.unwrap();
        });
    }

    #[test]
    fn pending_cog_future_can_be_dropped_outside_its_runtime() {
        let runtime = single_worker_runtime();
        let (engine, stack, cpu) = runtime.block_on(catalog_fixture());
        let style = rgb_style();
        let (release, blocker) = runtime.block_on(block_worker());
        let mut read = Box::pin(read_origin(&stack, &style));
        runtime.block_on(submit_native_read(read.as_mut(), &cpu));
        drop(read);
        runtime.shutdown_background();

        single_worker_runtime().block_on(async {
            let snapshot = engine.catalog().snapshot();
            let mut revoke = Box::pin(engine.catalog().revoke(
                &snapshot,
                "native-cog",
                tokio::time::Instant::now() + Duration::from_secs(5),
            ));
            assert!(futures_util::poll!(revoke.as_mut()).is_pending());
            release.send(()).unwrap();
            blocker.await.unwrap();
            revoke.await.unwrap();
        });
    }

    #[test]
    fn direct_reader_holds_admission_after_its_caller_runtime_exits() {
        let caller = single_worker_runtime();
        let source = caller.block_on(cog_fixture());
        let reader = source.reader().clone();
        let cpu = reader.cpu_limiter();
        let jobs = ReaderJobs::new(cpu.clone());
        let scheduler = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (release, blocker) = scheduler.block_on(block_worker());
        let mut request = NativeRequest::new(NativeWork::Reader(jobs.clone())).unwrap();
        request.cleanup = scheduler.handle().clone();
        let active = Arc::downgrade(&request.active);
        let mut read = Box::pin(request.read(async move {
            reader
                .point(0., 0., &async_geotiff::crs::Crs::epsg(3857), &[1])
                .await
                .map_err(crate::raster::agtiff_style::render_error)
        }));
        caller.block_on(submit_native_read(read.as_mut(), &cpu));
        let failed_io = reader_call(jobs.clone(), async {
            let _waiting = ActiveRasterRequest::object_read().unwrap();
            let error = mark_reader_error(AgError::Tiff("range failed".into()));
            Err::<(), _>(crate::raster::agtiff_style::render_error(error))
        });
        assert!(caller.block_on(failed_io).is_err());
        jobs.admit()
            .expect("first range error has no decode to drain");
        drop(read);
        caller.shutdown_background();
        scheduler.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while jobs.admit().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(active.upgrade().is_some());
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while active.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            jobs.admit().unwrap();
        });
    }

    #[test]
    fn direct_cog_queries_share_render_admission() {
        const CHILD: &str = "MAPTILE_TEST_DIRECT_COG_ADMISSION";
        if std::env::var_os(CHILD).is_none() {
            let (_, module) = module_path!().split_once("::").unwrap();
            let test_name = format!("{module}::direct_cog_queries_share_render_admission");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--format=pretty", "--color=never"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let passed = format!("test {test_name} ... ok");
            assert!(
                output.status.success() && stdout.lines().any(|line| line == passed),
                "isolated test {test_name} did not pass ({}):
{stdout}
{}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        use crate::sources::{RasterPointQuery, TileSource};
        single_worker_runtime().block_on(async {
            let source = cog_fixture().await;
            let bounds = source.tilejson().bounds.unwrap();
            let point = RasterPointQuery {
                x: (bounds.left + bounds.right) / 2.0,
                y: (bounds.bottom + bounds.top) / 2.0,
                coord_crs: "EPSG:4326".into(),
                bidx: vec![1],
            };
            let permits = (0..DEFAULT_MAX_IN_FLIGHT_REQUESTS)
                .map(|_| ActiveRasterRequest::admit(false).unwrap())
                .collect::<Vec<_>>();
            assert!(matches!(
                source.query_raster_point(&point).await,
                Err(MaptileError::CapacityExhausted)
            ));
            #[cfg(feature = "raster")]
            assert!(matches!(
                source.raster_coverage(None).await,
                Err(MaptileError::CapacityExhausted)
            ));
            drop(permits);
            assert!(source.query_raster_point(&point).await.unwrap().valid);
        });
    }
}
