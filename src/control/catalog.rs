mod inventory;
#[cfg(test)]
mod tests;

pub(crate) use inventory::CatalogBuildLease;
use inventory::{Binding, CatalogBuildState};
pub use inventory::{DiscoveredDataset, Inventory};

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use arc_swap::ArcSwap;
use tokio::{sync::RwLock, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{
    SourceDescriptor, SourceId, TileDriver,
    composition::{
        CompiledComposite, CompositeDeclaration, CompositeLimits, DuplicateLayer,
        MemberFailurePolicy, compile_graph,
    },
    lifecycle::{DriverInstance, DriverRegistry, SourceSlot, track_driver},
};
use crate::{
    core::response::TileOutcome,
    error::{MaptileError, MaptileResult},
    sources::{TileCoord, TileQuery},
};

pub(super) fn validate_id(id: &str) -> MaptileResult<()> {
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
            Self::Composite(c) => c.read(coord, query, limits).await,
        }
    }

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
        let id: SourceId = members.join(",").into();
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
            id: id.clone(),
            members,
            lazy: false,
            duplicate_layer: DuplicateLayer::Error,
            failure_policy: MemberFailurePolicy::FailFast,
        });
        let mut compiled = compile_graph(&self.sources, &needed, limits)?;
        Ok(Arc::new(CatalogEntry::Composite(
            compiled.remove(&id).expect("compiled requested composite"),
        )))
    }
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
            .insert(slot.descriptor().id.clone(), Arc::new(slot));
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
    pub(crate) fn new(limits: CompositeLimits) -> Self {
        Self {
            current: ArcSwap::from_pointee(CatalogSnapshot::default()),
            builds: Arc::default(),
            slots: Arc::default(),
            limits,
        }
    }
    pub(crate) fn limits(&self) -> CompositeLimits {
        self.limits
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
