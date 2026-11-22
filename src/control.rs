//! Versioned source catalogs and transactional publication.

mod composite;
pub(crate) use composite::ReadBudget;
use composite::{CompiledComposite, compile_graph};
pub use composite::{CompositeDeclaration, CompositeLimits, DuplicateLayer, MemberFailurePolicy};

use crate::cache::{CacheZoomRange, ContentVersion, TileCache};
use crate::error::{ErrorKind, MaptileError, MaptileResult};
use crate::service::TileOutcome;
use crate::sources::{
    MosaicPixelSelection, RasterPointQuery, RasterPointResponse, SourceKind, TileCoord, TileInfo,
    TileQuery, TileResponse, TileSource,
    raster_coverage::{CoverageQuery, CoverageResponse},
};
use arc_swap::ArcSwap;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub type SourceId = Arc<str>;

/// Run a synchronous source constructor on a bounded blocking worker.
/// During [`SourceCatalog::reconcile`], shutdown waits for the worker and for
/// disposal of a result abandoned by its caller. A constructor that starts
/// additional background work must retain it until the factory or driver drains.
///
/// ```no_run
/// use maptile_core::control::{DiscoveredDataset, Inventory, SourceCatalog, TileDriver, open_blocking};
/// use maptile_core::error::MaptileResult;
/// use std::sync::Arc;
/// # fn open_archive(_path: String) -> MaptileResult<Arc<dyn TileDriver>> { unimplemented!() }
/// # async fn refresh(catalog: &SourceCatalog, generation: u64, items: &[DiscoveredDataset]) -> MaptileResult<u64> {
/// catalog.reconcile(generation, "files", Inventory::Complete(items), |item| {
///     let path = item.dataset.to_string();
///     open_blocking(move || open_archive(path))
/// }).await
/// # }
/// ```
pub async fn open_blocking<T: Send + 'static>(
    open: impl FnOnce() -> MaptileResult<T> + Send + 'static,
) -> MaptileResult<T> {
    crate::transport::run_blocking(open).await
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceCapabilities {
    pub filter: bool,
    pub bbox: bool,
    pub url_query: bool,
    pub pixel_selection: Option<MosaicPixelSelection>,
    pub strict_mvt: bool,
    pub consistent_export: bool,
    pub batch: bool,
    pub raster_point: bool,
    pub raster_coverage: bool,
}

/// Every entry uses the Web Mercator XYZ grid. TileJSON contains source metadata only.
#[derive(Clone, Debug)]
pub struct SourceDescriptor {
    pub id: SourceId,
    pub kind: SourceKind,
    pub tilejson: tilejson::TileJSON,
    pub info: TileInfo,
    pub revision: ContentVersion,
    pub cache_zoom: CacheZoomRange,
    pub capabilities: SourceCapabilities,
}

impl SourceDescriptor {
    pub fn new(id: impl Into<SourceId>, info: TileInfo, revision: ContentVersion) -> Self {
        Self {
            id: id.into(),
            kind: SourceKind::Other,
            tilejson: tilejson::tilejson! { tiles: vec![] },
            info,
            revision,
            cache_zoom: CacheZoomRange::default(),
            capabilities: SourceCapabilities::default(),
        }
    }

    pub(crate) fn validate_query(&self, query: Option<&TileQuery<'_>>) -> MaptileResult<()> {
        self.capabilities.validate_query(&self.id, query)
    }
}

impl SourceCapabilities {
    pub(crate) fn from_source<S: TileSource + ?Sized>(source: &S) -> Self {
        Self {
            filter: source.supports_filter(),
            bbox: source.supports_bbox(),
            url_query: source.supports_url_query(),
            pixel_selection: source.mosaic_pixel_selection(),
            strict_mvt: source.strict_mvt(),
            consistent_export: source.consistent_export(),
            batch: source.native_batch(),
            raster_point: source.supports_raster_point(),
            raster_coverage: source.supports_raster_coverage(),
        }
    }
    pub(crate) fn validate_query(
        self,
        id: &str,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<()> {
        let Some(query) = query else { return Ok(()) };
        let c = self;
        let missing = if query.filter.is_some() && !c.filter {
            "CQL2"
        } else if query.bbox.is_some() && !c.bbox {
            "bbox"
        } else if query.url_query.is_some() && !c.url_query {
            "custom parameters"
        } else if query.pixel_selection.is_some() && c.pixel_selection.is_none() {
            "pixel selection"
        } else {
            return Ok(());
        };
        Err(MaptileError::Unsupported(format!(
            "{id} cannot apply {missing}"
        )))
    }
}

/// A third-party driver returns None for an absent coordinate and Some for a present tile,
/// including a zero-byte vector tile. The opened revision must match its descriptor.
#[async_trait::async_trait]
pub trait TileDriver: Send + Sync + 'static {
    fn descriptor(&self) -> SourceDescriptor;
    async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>>;
    /// Optional accelerated prefix with one position per input coordinate.
    async fn read_batch(
        &self,
        _coords: &[TileCoord],
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<Vec<Option<TileResponse>>>> {
        Ok(None)
    }
    async fn point(&self, _query: &RasterPointQuery) -> MaptileResult<RasterPointResponse> {
        Err(MaptileError::Unsupported("raster point query".into()))
    }
    async fn coverage(&self, _query: Option<&CoverageQuery>) -> MaptileResult<CoverageResponse> {
        Err(MaptileError::Unsupported("raster coverage".into()))
    }
    /// Signal shutdown synchronously. Implementations may receive this signal more than once.
    fn stop_admission(&self) {}
    /// Wait for source-owned work, including detached backend operations, to release its resources.
    async fn drain(&self) -> MaptileResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl<S: TileSource + ?Sized> TileDriver for S {
    fn descriptor(&self) -> SourceDescriptor {
        let mut revision = self.content_version();
        // Earlier third-party sources used the bare id as their default revision.
        if revision == ContentVersion::from_input(self.id().as_bytes()) {
            revision = ContentVersion::new_generation();
        }
        SourceDescriptor {
            id: self.id().into(),
            kind: self.source_kind(),
            tilejson: self.tilejson().clone(),
            info: self.tile_info(),
            revision,
            cache_zoom: self.cache_zoom(),
            capabilities: SourceCapabilities::from_source(self),
        }
    }
    async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        match self.get_tile(coord, query).await {
            Ok(tile) => Ok(Some(tile)),
            Err(MaptileError::TileNotFound(..)) => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn stop_admission(&self) {
        TileSource::start_close(self);
    }
    async fn point(&self, query: &RasterPointQuery) -> MaptileResult<RasterPointResponse> {
        self.query_raster_point(query).await
    }
    async fn coverage(&self, query: Option<&CoverageQuery>) -> MaptileResult<CoverageResponse> {
        self.raster_coverage(query).await
    }
    async fn read_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<Vec<Option<TileResponse>>>> {
        if !self.native_batch() {
            return Ok(None);
        }
        self.read_tile_batch_slots(coords, query).await.map(Some)
    }
    async fn drain(&self) -> MaptileResult<()> {
        TileSource::close(self).await
    }
}

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
    instance: Arc<DriverInstance>,
}

struct DriverInstance {
    state: Arc<DriverState>,
    drain_on_drop: bool,
}

impl std::ops::Deref for DriverInstance {
    type Target = DriverState;
    fn deref(&self) -> &DriverState {
        &self.state
    }
}

struct DriverState {
    stop_once: std::sync::Once,
    descriptor: SourceDescriptor,
    driver: Mutex<Option<Arc<dyn TileDriver>>>,
    health: Mutex<Health>,
    unconfirmed: std::sync::atomic::AtomicBool,
    revoked: CancellationToken,
    in_use: Arc<RwLock<()>>,
}

pub(crate) struct SourceReadLease {
    _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    _instance: Arc<DriverInstance>,
}

#[derive(Default)]
pub(crate) struct CatalogBuildState {
    unconfirmed: std::sync::atomic::AtomicBool,
}
pub(crate) type CatalogBuildLease = tokio::sync::OwnedRwLockReadGuard<CatalogBuildState>;

impl CatalogBuildState {
    #[cfg(any(feature = "postgres", feature = "mbtiles", test))]
    pub(crate) fn mark_unconfirmed(&self) {
        self.unconfirmed
            .store(true, std::sync::atomic::Ordering::Release);
    }
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
    #[cfg(test)]
    fn new(driver: Arc<dyn TileDriver>) -> MaptileResult<Self> {
        let descriptor = driver.descriptor();
        validate_id(&descriptor.id)?;
        let instance = Arc::new(DriverInstance::new(driver, descriptor));
        Self::registration(instance, false, None)
    }

    fn registration(
        instance: Arc<DriverInstance>,
        trusted_mvt: bool,
        dataset: Option<&DiscoveredDataset>,
    ) -> MaptileResult<Self> {
        let mut descriptor = instance.descriptor.clone();
        if trusted_mvt {
            if descriptor.info.format != crate::sources::TileFormat::Mvt
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

    pub(crate) async fn read(
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
    fn driver(&self) -> MaptileResult<Arc<dyn TileDriver>> {
        self.driver
            .lock()
            .unwrap()
            .clone()
            .ok_or(MaptileError::Cancelled)
    }

    fn stop(&self) {
        self.stop_once.call_once(|| {
            self.revoked.cancel();
            if let Ok(driver) = self.driver() {
                driver.stop_admission();
            }
        });
    }

    async fn drain(&self) -> MaptileResult<()> {
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

fn spawn_cleanup(work: impl Future<Output = ()> + Send + 'static) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(work);
    } else {
        static FALLBACK: std::sync::OnceLock<std::io::Result<tokio::runtime::Runtime>> =
            std::sync::OnceLock::new();
        if let Ok(runtime) = FALLBACK.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("maptile-drain")
                .enable_all()
                .build()
        }) {
            runtime.spawn(work);
        }
    }
}

struct RegisteredDriver {
    owner: Weak<DriverInstance>,
    // Shutdown can join a retirement after the last snapshot or staging update disappears.
    state: Arc<DriverState>,
}

type DriverRegistry = Arc<Mutex<HashMap<usize, RegisteredDriver>>>;

fn track_driver(
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

struct DiscoveryBuild {
    slots: DriverRegistry,
    opened: Vec<Arc<DriverInstance>>,
    lease: Option<Arc<CatalogBuildLease>>,
}

impl DiscoveryBuild {
    fn track(&mut self, driver: Arc<dyn TileDriver>) -> MaptileResult<Arc<DriverInstance>> {
        let descriptor = driver.descriptor();
        let instance = track_driver(&mut self.slots.lock().unwrap(), driver, descriptor)?;
        self.opened.push(instance.clone());
        Ok(instance)
    }

    fn discard(&mut self) -> Option<impl Future<Output = MaptileResult<()>> + Send + 'static> {
        let closing: Vec<_> = self
            .opened
            .drain(..)
            .filter_map(Arc::into_inner)
            .map(|mut instance| {
                instance.drain_on_drop = false;
                instance.stop();
                instance.state.clone()
            })
            .collect();
        if closing.is_empty() {
            return None;
        }
        let lease = self.lease.take();
        Some(async move {
            let _lease = lease;
            futures_util::future::join_all(closing.iter().map(|state| state.drain()))
                .await
                .into_iter()
                .collect()
        })
    }
}

impl Drop for DiscoveryBuild {
    fn drop(&mut self) {
        if let Some(cleanup) = self.discard() {
            spawn_cleanup(async move {
                let _ = cleanup.await;
            });
        }
    }
}

fn validate_id(id: &str) -> MaptileResult<()> {
    if id.trim().is_empty() || id.contains(',') {
        return Err(MaptileError::Config(format!("invalid catalog id {id:?}")));
    }
    Ok(())
}

pub(crate) enum CatalogEntry {
    Source(Arc<SourceSlot>),
    Composite(Arc<CompiledComposite>),
}

impl CatalogEntry {
    pub(crate) fn descriptor(&self) -> &SourceDescriptor {
        match self {
            Self::Source(s) => s.descriptor(),
            Self::Composite(c) => c.descriptor(),
        }
    }

    pub(crate) fn leaves(&self) -> &[Arc<SourceSlot>] {
        match self {
            Self::Source(s) => std::slice::from_ref(s),
            Self::Composite(c) => c.leaves(),
        }
    }

    pub(crate) async fn read(
        &self,
        coord: TileCoord,
        query: Option<&TileQuery<'_>>,
        limits: CompositeLimits,
    ) -> MaptileResult<TileOutcome> {
        match self {
            Self::Source(s) => s.read(coord, query).await,
            Self::Composite(c) => c.read(coord, query, &ReadBudget::new(limits)).await,
        }
    }
}

#[derive(Default)]
pub struct CatalogSnapshot {
    owner: Arc<()>,
    stopping: CancellationToken,
    generation: u64,
    entries: HashMap<SourceId, Arc<CatalogEntry>>,
    sources: HashMap<SourceId, Arc<SourceSlot>>,
    declarations: Vec<CompositeDeclaration>,
    bindings: HashMap<SourceId, Binding>,
}

impl CatalogSnapshot {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn describe(&self, id: &str) -> Option<&SourceDescriptor> {
        self.entries.get(id).map(|e| e.descriptor())
    }
    pub fn descriptors(&self) -> impl Iterator<Item = &SourceDescriptor> {
        self.entries.values().map(|e| e.descriptor())
    }
    pub(crate) fn entry(
        &self,
        id: &str,
        limits: CompositeLimits,
    ) -> MaptileResult<Arc<CatalogEntry>> {
        if self.stopping.is_cancelled() {
            return Err(MaptileError::Cancelled);
        }
        if let Some(entry) = self.entries.get(id) {
            return Ok(entry.clone());
        }
        if !id.contains(',') {
            return Err(MaptileError::SourceNotFound(id.into()));
        }
        let mut members = Vec::new();
        for member in id.split(',').map(str::trim) {
            if member.is_empty() || members.len() >= limits.max_leaves {
                return Err(MaptileError::InvalidRequest(
                    "invalid temporary composite membership".into(),
                ));
            }
            if !self.entries.contains_key(member) {
                return Err(MaptileError::SourceNotFound(member.into()));
            }
            members.push(Arc::<str>::from(member));
        }
        let mut needed = Vec::new();
        let mut visited = HashSet::new();
        let mut pending = members.clone();
        while let Some(member) = pending.pop() {
            if !visited.insert(member.clone()) {
                continue;
            }
            if let Some(decl) = self.declarations.iter().find(|d| d.id == member) {
                pending.extend(decl.members.iter().cloned());
                needed.push(decl.clone());
            }
        }
        needed.push(CompositeDeclaration {
            id: id.into(),
            members,
            lazy: false,
            duplicate_layer: DuplicateLayer::Error,
            failure_policy: MemberFailurePolicy::FailFast,
        });
        let mut compiled = compile_graph(&self.sources, &needed, limits)?;
        Ok(Arc::new(CatalogEntry::Composite(
            compiled.remove(id).expect("compiled requested composite"),
        )))
    }
}

#[derive(Clone)]
struct Binding {
    namespace: SourceId,
    dataset: SourceId,
    revision: ContentVersion,
}

pub struct CatalogUpdate {
    base: Arc<CatalogSnapshot>,
    sources: HashMap<SourceId, Arc<SourceSlot>>,
    declarations: Vec<CompositeDeclaration>,
    bindings: HashMap<SourceId, Binding>,
    slots: DriverRegistry,
    builds: Arc<RwLock<CatalogBuildState>>,
}

impl CatalogUpdate {
    /// After admission, the catalog owns cleanup even if publication fails. A stopped catalog rejects new ownership.
    pub fn put(&mut self, driver: Arc<dyn TileDriver>) -> MaptileResult<()> {
        self.put_driver(driver, false)
    }

    /// The publisher guarantees MVT 2.1 conformance and complete declared layer names at every coordinate.
    /// Composition still checks the actual layer names of every tile.
    pub fn put_trusted_mvt(&mut self, driver: Arc<dyn TileDriver>) -> MaptileResult<()> {
        self.put_driver(driver, true)
    }

    fn put_driver(&mut self, driver: Arc<dyn TileDriver>, trusted_mvt: bool) -> MaptileResult<()> {
        let _building = self
            .builds
            .clone()
            .try_read_owned()
            .map_err(|_| MaptileError::Cancelled)?;
        if self.base.stopping.is_cancelled() {
            return Err(MaptileError::Cancelled);
        }
        let descriptor = driver.descriptor();
        let instance = track_driver(&mut self.slots.lock().unwrap(), driver, descriptor)?;
        validate_id(&instance.descriptor.id)?;
        if self.bindings.contains_key(&instance.descriptor.id) {
            return Err(MaptileError::Config(format!(
                "{} belongs to a discovery namespace",
                instance.descriptor.id
            )));
        }
        self.register(instance, trusted_mvt, None)
    }

    fn register(
        &mut self,
        instance: Arc<DriverInstance>,
        trusted_mvt: bool,
        dataset: Option<&DiscoveredDataset>,
    ) -> MaptileResult<()> {
        if self.base.stopping.is_cancelled() || instance.revoked.is_cancelled() {
            return Err(MaptileError::Cancelled);
        }
        if dataset.is_some_and(|item| item.id != instance.descriptor.id) {
            return Err(MaptileError::Config(format!(
                "discovery factory returned unexpected source {}",
                instance.descriptor.id
            )));
        }
        let slot = SourceSlot::registration(instance, trusted_mvt, dataset)?;
        self.sources
            .insert(slot.descriptor.id.clone(), Arc::new(slot));
        Ok(())
    }

    pub fn remove(&mut self, id: &str) {
        self.sources.remove(id);
        self.declarations.retain(|d| d.id.as_ref() != id);
        self.bindings.remove(id);
    }

    pub fn composite(&mut self, declaration: CompositeDeclaration) -> MaptileResult<()> {
        validate_id(&declaration.id)?;
        if self.bindings.contains_key(&declaration.id) {
            return Err(MaptileError::Config(format!(
                "{} belongs to a discovery namespace",
                declaration.id
            )));
        }
        self.declarations.retain(|d| d.id != declaration.id);
        self.declarations.push(declaration);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredDataset {
    pub id: SourceId,
    pub dataset: SourceId,
    pub revision: ContentVersion,
}

pub enum Inventory<'a> {
    Complete(&'a [DiscoveredDataset]),
    Partial(&'a [DiscoveredDataset]),
    Failed,
    Unchanged,
}

pub struct SourceCatalog {
    current: ArcSwap<CatalogSnapshot>,
    builds: Arc<RwLock<CatalogBuildState>>,
    slots: DriverRegistry,
    limits: CompositeLimits,
}

impl Default for SourceCatalog {
    fn default() -> Self {
        Self::new(CompositeLimits::default())
    }
}

impl SourceCatalog {
    fn new(limits: CompositeLimits) -> Self {
        Self {
            current: ArcSwap::from_pointee(CatalogSnapshot::default()),
            builds: Arc::default(),
            slots: Arc::default(),
            limits,
        }
    }
    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.current.load_full()
    }
    pub fn update(&self) -> CatalogUpdate {
        let base = self.snapshot();
        self.slots.lock().unwrap().retain(|_, slot| {
            slot.state.driver().is_ok()
                || slot
                    .state
                    .unconfirmed
                    .load(std::sync::atomic::Ordering::Acquire)
        });
        CatalogUpdate {
            sources: base.sources.clone(),
            declarations: base.declarations.clone(),
            bindings: base.bindings.clone(),
            base,
            slots: self.slots.clone(),
            builds: self.builds.clone(),
        }
    }
    pub fn publish(&self, update: CatalogUpdate) -> MaptileResult<u64> {
        let composites = compile_graph(&update.sources, &update.declarations, self.limits)?;
        let mut entries: HashMap<_, _> = update
            .sources
            .iter()
            .map(|(id, slot)| (id.clone(), Arc::new(CatalogEntry::Source(slot.clone()))))
            .collect();
        entries.extend(
            composites
                .into_iter()
                .map(|(id, c)| (id, Arc::new(CatalogEntry::Composite(c)))),
        );
        let generation = update
            .base
            .generation
            .checked_add(1)
            .ok_or_else(|| MaptileError::Config("catalog generation exhausted".into()))?;
        let next = Arc::new(CatalogSnapshot {
            owner: update.base.owner.clone(),
            stopping: update.base.stopping.clone(),
            generation,
            entries,
            sources: update.sources,
            declarations: update.declarations,
            bindings: update.bindings,
        });
        let registry = self.slots.lock().unwrap();
        if self.current.load().stopping.is_cancelled() {
            return Err(MaptileError::Cancelled);
        }
        let previous = self.current.compare_and_swap(&update.base, next.clone());
        drop(registry);
        drop(next);
        if !Arc::ptr_eq(&previous, &update.base) {
            return Err(MaptileError::VersionChanged("catalog publication".into()));
        }
        Ok(generation)
    }

    pub async fn reconcile<F, Fut>(
        &self,
        expected_generation: u64,
        namespace: &str,
        inventory: Inventory<'_>,
        mut open: F,
    ) -> MaptileResult<u64>
    where
        F: FnMut(&DiscoveredDataset) -> Fut,
        Fut: Future<Output = MaptileResult<Arc<dyn TileDriver>>>,
    {
        let lease = Arc::new(self.builds.clone().read_owned().await);
        let mut build = DiscoveryBuild {
            slots: self.slots.clone(),
            opened: Vec::new(),
            lease: Some(lease.clone()),
        };
        let result = crate::transport::CATALOG_BUILD_LEASE
            .scope(
                lease,
                self.apply_inventory(
                    expected_generation,
                    namespace,
                    inventory,
                    &mut open,
                    &mut build,
                ),
            )
            .await;
        if result.is_ok() {
            build.opened.clear();
        } else if let Some(cleanup) = build.discard() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime
                    .spawn(cleanup)
                    .await
                    .map_err(MaptileError::backend)??;
            } else {
                cleanup.await?;
            }
        }
        result
    }

    async fn apply_inventory<F, Fut>(
        &self,
        expected_generation: u64,
        namespace: &str,
        inventory: Inventory<'_>,
        open: &mut F,
        build: &mut DiscoveryBuild,
    ) -> MaptileResult<u64>
    where
        F: FnMut(&DiscoveredDataset) -> Fut,
        Fut: Future<Output = MaptileResult<Arc<dyn TileDriver>>>,
    {
        let mut update = self.update();
        if update.base.stopping.is_cancelled() {
            return Err(MaptileError::Cancelled);
        }
        if update.base.generation != expected_generation {
            return Err(MaptileError::VersionChanged("discovery inventory".into()));
        }
        let complete = matches!(inventory, Inventory::Complete(_));
        let items = match inventory {
            Inventory::Complete(items) | Inventory::Partial(items) => items,
            Inventory::Failed | Inventory::Unchanged => return Ok(update.base.generation),
        };
        let mut seen = HashSet::new();
        for item in items {
            validate_id(&item.id)?;
            if !seen.insert(item.id.clone())
                || match update.bindings.get(&item.id) {
                    Some(b) => b.namespace.as_ref() != namespace || b.dataset != item.dataset,
                    None => {
                        update.sources.contains_key(&item.id)
                            || update.base.entries.contains_key(&item.id)
                    }
                }
            {
                return Err(MaptileError::Config(format!(
                    "conflicting dataset binding for {}",
                    item.id
                )));
            }
        }
        let mut changed = false;
        for item in items {
            if update.sources.contains_key(&item.id)
                && update
                    .bindings
                    .get(&item.id)
                    .is_some_and(|b| b.revision == item.revision)
            {
                continue;
            }
            let instance = build.track(open(item).await?)?;
            update.register(instance, false, Some(item))?;
            update.bindings.insert(
                item.id.clone(),
                Binding {
                    namespace: namespace.into(),
                    dataset: item.dataset.clone(),
                    revision: item.revision,
                },
            );
            changed = true;
        }
        if complete {
            let removed: Vec<_> = update
                .bindings
                .iter()
                .filter(|(id, b)| {
                    b.namespace.as_ref() == namespace
                        && update.sources.contains_key(*id)
                        && !seen.contains(*id)
                })
                .map(|(id, _)| id.clone())
                .collect();
            changed |= !removed.is_empty();
            for id in removed {
                update.sources.remove(&id);
            }
        }
        if changed {
            self.publish(update)
        } else if !Arc::ptr_eq(&self.current.load(), &update.base) {
            Err(MaptileError::VersionChanged(
                "catalog reconciliation".into(),
            ))
        } else {
            Ok(update.base.generation)
        }
    }

    /// Revoke the exact source instance observed in a snapshot, including pending reads.
    pub async fn revoke(
        &self,
        observed: &CatalogSnapshot,
        id: &str,
        deadline: Instant,
    ) -> MaptileResult<()> {
        if !Arc::ptr_eq(&observed.owner, &self.current.load().owner) {
            return Err(MaptileError::InvalidRequest(
                "snapshot belongs to another catalog".into(),
            ));
        }
        let slot = observed
            .sources
            .get(id)
            .ok_or_else(|| MaptileError::SourceNotFound(id.into()))?;
        slot.instance.stop();
        tokio::time::timeout_at(deadline, slot.instance.drain())
            .await
            .map_err(|_| MaptileError::Deadline)?
    }

    /// Stop publication and reads, then drain all opened generations, including retired ones still leased.
    /// Calling again after a deadline resumes the drain.
    pub async fn shutdown(&self, deadline: Instant) -> MaptileResult<()> {
        {
            let slots = self.slots.lock().unwrap();
            self.current.load().stopping.cancel();
            let states: Vec<_> = slots.values().map(|entry| entry.state.clone()).collect();
            drop(slots);
            for state in states {
                state.stop();
            }
        }
        let builds = tokio::time::timeout_at(deadline, self.builds.write())
            .await
            .map_err(|_| MaptileError::Deadline)?;
        let states: Vec<_> = self
            .slots
            .lock()
            .unwrap()
            .values()
            .map(|entry| entry.state.clone())
            .collect();
        let results = tokio::time::timeout_at(
            deadline,
            futures_util::future::join_all(states.iter().map(|state| state.drain())),
        )
        .await
        .map_err(|_| MaptileError::Deadline)?;
        results.into_iter().collect::<MaptileResult<()>>()?;
        self.slots.lock().unwrap().clear();
        if builds
            .unconfirmed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(MaptileError::SourceUnavailable(
                "downstream completion unconfirmed during source discovery".into(),
            ));
        }
        Ok(())
    }
}

impl CatalogEntry {
    pub(crate) async fn revoked(&self) {
        use futures_util::{StreamExt, stream::FuturesUnordered};
        let mut tokens: FuturesUnordered<_> = self
            .leaves()
            .iter()
            .map(|slot| slot.instance.revoked.cancelled())
            .collect();
        if tokens.is_empty() {
            std::future::pending::<()>().await;
        }
        tokens.next().await;
    }
}

pub struct CatalogService {
    pub(crate) flights: crate::service::Flights,
    catalog: SourceCatalog,
    pub(crate) cache: TileCache,
}

impl CatalogService {
    pub fn new(cache: TileCache) -> Self {
        Self {
            catalog: SourceCatalog::default(),
            cache,
            flights: Default::default(),
        }
    }
    pub fn with_limits(cache: TileCache, limits: CompositeLimits) -> MaptileResult<Self> {
        if limits.max_concurrency == 0
            || limits.max_concurrency > tokio::sync::Semaphore::MAX_PERMITS
            || limits.max_leaves == 0
            || limits.max_depth == 0
            || limits.max_depth > 64
            || limits.max_decoded_bytes == 0
            || limits.max_compressed_bytes == 0
            || limits.max_work_bytes == 0
        {
            return Err(MaptileError::Config("invalid composite limits".into()));
        }
        Ok(Self {
            catalog: SourceCatalog::new(limits),
            cache,
            flights: Default::default(),
        })
    }
    pub fn catalog(&self) -> &SourceCatalog {
        &self.catalog
    }
    pub(crate) fn limits(&self) -> CompositeLimits {
        self.catalog.limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{ReadTarget, TileStack};
    use crate::sources::{TileEncoding, TileFormat};
    use std::sync::mpsc;
    use tokio::sync::oneshot;

    type WorkSignals = (oneshot::Sender<()>, mpsc::Receiver<()>);
    struct Blocked(Mutex<Option<WorkSignals>>);

    #[tokio::test]
    async fn shutdown_remembers_unconfirmed_discovery_work() {
        let catalog = SourceCatalog::default();
        let items = [DiscoveredDataset {
            id: "remote".into(),
            dataset: "places".into(),
            revision: ContentVersion::from_input(b"v1"),
        }];
        let result = catalog
            .reconcile(0, "db", Inventory::Complete(&items), |_| async {
                crate::transport::CATALOG_BUILD_LEASE.with(|lease| lease.mark_unconfirmed());
                Err(MaptileError::Cancelled)
            })
            .await;
        assert!(matches!(result, Err(MaptileError::Cancelled)));
        for _ in 0..2 {
            assert!(matches!(
                catalog
                    .shutdown(Instant::now() + Duration::from_secs(1))
                    .await,
                Err(MaptileError::SourceUnavailable(_))
            ));
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_cancelled_factory_file_work() {
        let catalog = SourceCatalog::default();
        let items = [DiscoveredDataset {
            id: "local".into(),
            dataset: "archive".into(),
            revision: ContentVersion::from_input(b"v1"),
        }];
        let (started, ready) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        let mut signals = Some((started, wait));
        let mut build =
            Box::pin(
                catalog.reconcile(0, "files", Inventory::Complete(&items), |_| {
                    let (started, wait) = signals.take().unwrap();
                    open_blocking(move || {
                        started.send(()).unwrap();
                        wait.recv_timeout(Duration::from_secs(5)).unwrap();
                        Err(MaptileError::Cancelled)
                    })
                }),
            );
        tokio::select! {
            result = build.as_mut() => panic!("factory finished early: {result:?}"),
            ready = ready => ready.unwrap(),
        }
        drop(build);
        let mut shutdown = Box::pin(catalog.shutdown(Instant::now() + Duration::from_secs(5)));
        let first = futures::poll!(shutdown.as_mut());
        release.send(()).unwrap();
        assert!(
            first.is_pending(),
            "shutdown completed during file work: {first:?}"
        );
        shutdown.await.unwrap();
    }

    #[async_trait::async_trait]
    impl TileDriver for Blocked {
        fn descriptor(&self) -> SourceDescriptor {
            SourceDescriptor::new(
                "retired",
                TileInfo::new(TileFormat::Mvt, TileEncoding::Uncompressed),
                ContentVersion::from_input(b"v1"),
            )
        }
        async fn read(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<TileResponse>> {
            let (began, wait) = self.0.lock().unwrap().take().unwrap();
            crate::transport::run_blocking(move || {
                let _ = began.send(());
                let _ = wait.recv_timeout(Duration::from_secs(5));
                Ok(None)
            })
            .await
        }
    }
    #[tokio::test]
    async fn shutdown_waits_for_cancelled_job_on_retired_source() {
        let (began, started) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
        let mut update = engine.catalog().update();
        update
            .put(Arc::new(Blocked(Mutex::new(Some((began, wait))))))
            .unwrap();
        engine.catalog().publish(update).unwrap();
        let stack = TileStack::builder(engine.clone()).build();
        let mut read = Box::pin(
            stack.read(ReadTarget::new("retired", "visible").at(TileCoord { z: 0, x: 0, y: 0 })),
        );
        tokio::select! { r = read.as_mut() => panic!("driver returned early {r:?}"), started = started => started.unwrap(), }
        let mut remove = engine.catalog().update();
        remove.remove("retired");
        engine.catalog().publish(remove).unwrap();
        drop(read);
        let mut drain = Box::pin(
            engine
                .catalog()
                .shutdown(Instant::now() + Duration::from_secs(5)),
        );
        let first = futures::poll!(drain.as_mut());
        release.send(()).unwrap();
        assert!(
            first.is_pending(),
            "shutdown reported {first:?} while tracked file work was still blocked"
        );
        drain.await.unwrap();
    }
}
