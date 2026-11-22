use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use tokio::{sync::RwLock, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{DiscoveredDataset, SourceDescriptor, TileDriver};
use crate::{
    cache::ContentVersion,
    core::response::TileOutcome,
    error::{ErrorKind, MaptileError, MaptileResult},
    sources::{
        RasterPointQuery, RasterPointResponse, TileCoord, TileFormat, TileQuery, TileResponse,
        raster_coverage::{CoverageQuery, CoverageResponse},
    },
};

#[derive(Clone, Copy)]
enum HealthStatus {
    Ready,
    Failed(Instant),
    Probing,
}

struct Health {
    epoch: u64,
    status: HealthStatus,
}

pub(crate) struct SourceSlot {
    descriptor: SourceDescriptor,
    pub(super) instance: Arc<DriverInstance>,
}

pub(super) struct DriverInstance {
    pub(super) state: Arc<DriverState>,
    pub(super) drain_on_drop: bool,
}

impl std::ops::Deref for DriverInstance {
    type Target = DriverState;
    fn deref(&self) -> &DriverState {
        &self.state
    }
}

pub(super) struct DriverState {
    stop_once: std::sync::Once,
    pub(super) descriptor: SourceDescriptor,
    driver: Mutex<Option<Arc<dyn TileDriver>>>,
    health: Mutex<Health>,
    pub(super) unconfirmed: std::sync::atomic::AtomicBool,
    pub(super) revoked: CancellationToken,
    in_use: Arc<RwLock<()>>,
}

pub(crate) struct SourceReadLease {
    _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    _instance: Arc<DriverInstance>,
}

#[cfg(any(feature = "postgres", feature = "agtiff", feature = "mbtiles"))]
impl SourceReadLease {
    pub(crate) fn mark_unconfirmed(&self) {
        self._instance
            .unconfirmed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

struct Recovery<'a> {
    state: &'a Mutex<Health>,
    epoch: u64,
    probe: bool,
}
impl Drop for Recovery<'_> {
    fn drop(&mut self) {
        if self.probe {
            let mut health = self.state.lock().unwrap();
            if health.epoch == self.epoch {
                health.status = HealthStatus::Failed(Instant::now());
                health.epoch = health.epoch.wrapping_add(1);
            }
        }
    }
}

impl SourceSlot {
    pub(super) fn registration(
        instance: Arc<DriverInstance>,
        trusted_mvt: bool,
        dataset: Option<&DiscoveredDataset>,
    ) -> MaptileResult<Self> {
        let mut descriptor = instance.descriptor.clone();
        if trusted_mvt {
            if descriptor.info.format != TileFormat::Mvt
                || descriptor
                    .tilejson
                    .vector_layers
                    .as_ref()
                    .is_none_or(Vec::is_empty)
            {
                return Err(MaptileError::Config(format!(
                    "{} must declare its MVT layers",
                    descriptor.id
                )));
            }
            descriptor.capabilities.strict_mvt = true;
        }
        let mut hash = blake3::Hasher::new();
        hash.update(b"maptile-source-xyz-v1\0");
        hash.update(descriptor.revision.as_bytes());
        hash.update(&[descriptor.info.format as u8, descriptor.info.encoding as u8]);
        if let Some(dataset) = dataset {
            hash.update(dataset.revision.as_bytes());
            hash.update(&(dataset.dataset.len() as u64).to_le_bytes());
            hash.update(dataset.dataset.as_bytes());
        }
        if let Some(layers) = &descriptor.tilejson.vector_layers {
            for layer in layers {
                hash.update(&(layer.id.len() as u64).to_le_bytes());
                hash.update(layer.id.as_bytes());
            }
        }
        descriptor.revision = ContentVersion::from_bytes(*hash.finalize().as_bytes());
        Ok(Self {
            descriptor,
            instance,
        })
    }

    pub(crate) fn descriptor(&self) -> &SourceDescriptor {
        &self.descriptor
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.instance.revoked.is_cancelled()
    }

    pub(super) async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileOutcome> {
        self.run(self.instance.driver()?.read(coord, query))
            .await
            .map(|tile| tile.map_or(TileOutcome::NoTile, TileOutcome::Present))
    }

    pub(crate) async fn read_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<Vec<Option<TileResponse>>>> {
        self.run(self.instance.driver()?.read_batch(coords, query))
            .await
    }

    pub(crate) async fn point(
        &self,
        query: &RasterPointQuery,
    ) -> MaptileResult<RasterPointResponse> {
        self.run(self.instance.driver()?.point(query)).await
    }
    pub(crate) async fn coverage(
        &self,
        query: Option<&CoverageQuery>,
    ) -> MaptileResult<CoverageResponse> {
        self.run(self.instance.driver()?.coverage(query)).await
    }

    async fn run<T>(&self, operation: impl Future<Output = MaptileResult<T>>) -> MaptileResult<T> {
        let lease = Arc::new(SourceReadLease {
            _guard: self.instance.in_use.clone().read_owned().await,
            _instance: self.instance.clone(),
        });
        if self.instance.revoked.is_cancelled() {
            return Err(MaptileError::Cancelled);
        }
        let mut recovery = {
            let mut health = self.instance.health.lock().unwrap();
            let probe = match health.status {
                HealthStatus::Ready => false,
                HealthStatus::Failed(at) if at.elapsed() >= Duration::from_secs(1) => {
                    health.status = HealthStatus::Probing;
                    health.epoch = health.epoch.wrapping_add(1);
                    true
                }
                _ => {
                    return Err(MaptileError::SourceUnavailable(
                        self.descriptor.id.to_string(),
                    ));
                }
            };
            Recovery {
                state: &self.instance.health,
                epoch: health.epoch,
                probe,
            }
        };
        let result = tokio::select! {
            biased;
            () = self.instance.revoked.cancelled() => return Err(MaptileError::Cancelled),
            result = crate::transport::SOURCE_READ_LEASE.scope(lease, operation) => result,
        };
        let mut health = self.instance.health.lock().unwrap();
        if health.epoch == recovery.epoch {
            let unavailable = result
                .as_ref()
                .is_err_and(|e| e.kind() == ErrorKind::Unavailable);
            if unavailable || recovery.probe {
                health.status = if unavailable {
                    HealthStatus::Failed(Instant::now())
                } else {
                    HealthStatus::Ready
                };
                health.epoch = health.epoch.wrapping_add(1);
            }
        }
        recovery.probe = false;
        result
    }
}

impl DriverInstance {
    fn new(driver: Arc<dyn TileDriver>, descriptor: SourceDescriptor) -> Self {
        Self {
            drain_on_drop: true,
            state: Arc::new(DriverState {
                stop_once: std::sync::Once::new(),
                driver: Mutex::new(Some(driver)),
                descriptor,
                health: Mutex::new(Health {
                    epoch: 0,
                    status: HealthStatus::Ready,
                }),
                unconfirmed: false.into(),
                revoked: CancellationToken::new(),
                in_use: Arc::default(),
            }),
        }
    }
}

impl DriverState {
    pub(super) fn driver(&self) -> MaptileResult<Arc<dyn TileDriver>> {
        self.driver
            .lock()
            .unwrap()
            .clone()
            .ok_or(MaptileError::Cancelled)
    }

    pub(super) fn stop(&self) {
        self.stop_once.call_once(|| {
            self.revoked.cancel();
            if let Ok(driver) = self.driver() {
                driver.stop_admission();
            }
        });
    }

    pub(super) async fn drain(&self) -> MaptileResult<()> {
        let _exclusive = self.in_use.write().await;
        if let Ok(driver) = self.driver() {
            driver.drain().await?;
        }
        if self.unconfirmed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(MaptileError::SourceUnavailable(format!(
                "downstream completion unconfirmed for {}",
                self.descriptor.id
            )));
        }
        self.driver.lock().unwrap().take();
        Ok(())
    }
}

impl Drop for DriverInstance {
    fn drop(&mut self) {
        if self.drain_on_drop {
            self.state.stop();
            if self.state.driver().is_ok() {
                let state = self.state.clone();
                spawn_cleanup(async move {
                    let _ = state.drain().await;
                });
            }
        }
    }
}

pub(super) fn spawn_cleanup(work: impl Future<Output = ()> + Send + 'static) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(work);
    } else if let Ok(runtime) = cleanup_runtime() {
        runtime.spawn(work);
    }
}

pub(crate) fn cleanup_runtime() -> MaptileResult<&'static tokio::runtime::Handle> {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();
    static INIT: Mutex<()> = Mutex::new(());
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let _init = INIT.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let handle = start_cleanup_runtime(std::thread::Builder::new().name("maptile-drain".into()))
        .map_err(|error| {
            MaptileError::SourceUnavailable(format!("cannot start cleanup runtime: {error}"))
        })?;
    Ok(RUNTIME.get_or_init(|| handle))
}

fn start_cleanup_runtime(thread: std::thread::Builder) -> std::io::Result<tokio::runtime::Handle> {
    let (started, ready) = std::sync::mpsc::sync_channel(1);
    // A fallible OS thread starts the single scheduler that orders raw COG cancellation and decode.
    thread.spawn(move || {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => {
                if started.send(Ok(runtime.handle().clone())).is_ok() {
                    runtime.block_on(std::future::pending::<()>());
                }
            }
            Err(error) => {
                let _ = started.send(Err(error));
            }
        }
    })?;
    ready.recv().map_err(std::io::Error::other)?
}

pub(super) struct RegisteredDriver {
    owner: Weak<DriverInstance>,
    // Shutdown can join a retirement after the last snapshot or staging update disappears.
    pub(super) state: Arc<DriverState>,
}

pub(super) type DriverRegistry = Arc<Mutex<HashMap<usize, RegisteredDriver>>>;

pub(super) fn track_driver(
    slots: &mut HashMap<usize, RegisteredDriver>,
    driver: Arc<dyn TileDriver>,
    descriptor: SourceDescriptor,
) -> MaptileResult<Arc<DriverInstance>> {
    let key = Arc::as_ptr(&driver) as *const () as usize;
    if let Some(existing) = slots.get(&key)
        && existing
            .state
            .driver()
            .is_ok_and(|active| Arc::ptr_eq(&active, &driver))
    {
        return existing.owner.upgrade().ok_or(MaptileError::Cancelled);
    }
    let instance = Arc::new(DriverInstance::new(driver, descriptor));
    slots.insert(
        key,
        RegisteredDriver {
            owner: Arc::downgrade(&instance),
            state: instance.state.clone(),
        },
    );
    Ok(instance)
}

#[cfg(test)]
mod tests {
    #[cfg(all(unix, target_pointer_width = "64"))]
    #[test]
    fn cleanup_worker_creation_failure_returns_an_error() {
        let thread = std::thread::Builder::new().stack_size(1 << 60);
        assert!(super::start_cleanup_runtime(thread).is_err());
    }
}
