use std::sync::atomic::AtomicBool;
use std::{collections::HashSet, sync::Arc};

use super::{SourceCatalog, validate_id};
use crate::{
    cache::ContentVersion,
    control::{
        SourceId, TileDriver,
        lifecycle::{DriverInstance, DriverRegistry, spawn_cleanup, track_driver},
    },
    error::{MaptileError, MaptileResult},
};

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

#[derive(Clone)]
pub(super) struct Binding {
    pub(super) namespace: SourceId,
    pub(super) dataset: SourceId,
    pub(super) revision: ContentVersion,
}

#[derive(Default)]
pub(crate) struct CatalogBuildState {
    pub(super) unconfirmed: AtomicBool,
}
pub(crate) type CatalogBuildLease = tokio::sync::OwnedRwLockReadGuard<CatalogBuildState>;

impl CatalogBuildState {
    #[cfg(any(feature = "postgres", feature = "mbtiles", test))]
    pub(crate) fn mark_unconfirmed(&self) {
        self.unconfirmed
            .store(true, std::sync::atomic::Ordering::Release);
    }
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

impl SourceCatalog {
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
}
