//! In-process registry of `TileSource` instances.
//!
//! DashMap is used over `RwLock<HashMap>` because the workload is
//! write-rare / read-very-frequent: every tile request takes a `get`,
//! while registers only happen at startup or on rare config changes.
//! DashMap's sharded locks scale better under that mix.

use crate::cache::{TileCache, TileCacheKey};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    TileCoord, TileEtagResponse, TileQuery, TileResponse, TileSource, effective_pixel_selection,
};
use std::sync::Arc;

enum SourceState {
    Ready(Arc<dyn TileSource>),
    Unavailable {
        failed_at: tokio::time::Instant,
        recovery: Option<Arc<()>>,
    },
}

#[derive(Default)]
pub struct SourceManager {
    sources: dashmap::DashMap<String, SourceState>,
    cache: Option<Arc<TileCache>>,
}

struct PendingSources<'a> {
    sources: Vec<Arc<dyn TileSource>>,
    manager: &'a SourceManager,
}

impl Drop for PendingSources<'_> {
    fn drop(&mut self) {
        // A failed or cancelled factory must not leave unpublished sources running.
        for source in &self.sources {
            if !self
                .manager
                .get(source.id())
                .is_some_and(|registered| Arc::ptr_eq(&registered, source))
            {
                source.start_close();
            }
        }
    }
}

struct RecoveryGuard<'a> {
    manager: &'a SourceManager,
    id: &'a str,
    token: Arc<()>,
    pending: PendingSources<'a>,
}

impl Drop for RecoveryGuard<'_> {
    fn drop(&mut self) {
        if let Some(mut entry) = self.manager.sources.get_mut(self.id)
            && let SourceState::Unavailable {
                failed_at,
                recovery,
            } = entry.value_mut()
            && recovery
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            *failed_at = tokio::time::Instant::now();
            *recovery = None;
        }
    }
}

impl SourceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a shared tile cache. Returns `self` for chaining.
    /// Filter, bbox and URL inputs bypass the cache; style and mosaic selection
    /// partition the cache key. Empty queries remain cacheable.
    /// Sources must not re-enter this cache for the same key during a miss.
    /// Custom sources that own a tile cache should disable their manager cache policy.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<TileCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Reference to the attached cache, if any (used by admin flush endpoints).
    #[must_use]
    pub fn cache(&self) -> Option<&Arc<TileCache>> {
        self.cache.as_ref()
    }

    /// Insert (or replace) a source by its `id()`.
    ///
    /// Replacing an already-registered source is expected during local command
    /// registration followed by async event fan-out, so keep it at debug level.
    pub fn register(&self, src: Arc<dyn TileSource>) {
        let id = src.id().to_string();
        if let Some(SourceState::Ready(previous)) = self
            .sources
            .insert(id.clone(), SourceState::Ready(src.clone()))
        {
            tracing::debug!(
                target: "maptile_core::sources::manager",
                source_id = %id,
                "SourceManager::register replacing an existing source with the same id"
            );
            if !Arc::ptr_eq(&previous, &src) {
                previous.start_close();
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn TileSource>> {
        self.sources.get(id).and_then(|r| match r.value() {
            SourceState::Ready(source) => Some(source.clone()),
            SourceState::Unavailable { .. } => None,
        })
    }

    /// Administratively mark an id unavailable, creating the record if absent.
    /// Repeated marks preserve its backoff and any active recovery. Use
    /// [`Self::mark_unavailable_if_current`] for failures from an observed source.
    pub fn mark_unavailable(&self, id: &str) {
        let previous = {
            let mut entry =
                self.sources
                    .entry(id.to_string())
                    .or_insert_with(|| SourceState::Unavailable {
                        failed_at: tokio::time::Instant::now(),
                        recovery: None,
                    });
            if matches!(entry.value(), SourceState::Unavailable { .. }) {
                return;
            }
            std::mem::replace(
                entry.value_mut(),
                SourceState::Unavailable {
                    failed_at: tokio::time::Instant::now(),
                    recovery: None,
                },
            )
        };
        if let SourceState::Ready(source) = previous {
            source.start_close();
        }
    }

    /// Retire only the exact registered instance observed by a failed request.
    /// Returns false for stale, absent or already unavailable sources without
    /// changing the registry. Equal content versions do not imply identity.
    pub fn mark_unavailable_if_current(&self, expected: &Arc<dyn TileSource>) -> bool {
        let previous = if let Some(mut entry) = self.sources.get_mut(expected.id())
            && matches!(entry.value(), SourceState::Ready(source) if Arc::ptr_eq(source, expected))
        {
            Some(std::mem::replace(
                entry.value_mut(),
                SourceState::Unavailable {
                    failed_at: tokio::time::Instant::now(),
                    recovery: None,
                },
            ))
        } else {
            None
        };
        if let Some(SourceState::Ready(source)) = previous {
            source.start_close();
            true
        } else {
            false
        }
    }

    pub fn is_unavailable(&self, id: &str) -> bool {
        self.sources
            .get(id)
            .is_some_and(|r| matches!(r.value(), SourceState::Unavailable { .. }))
    }

    /// Advisory eligibility after a one-second backoff, false during recovery.
    /// Only [`Self::recover_unavailable`] reserves a factory invocation.
    /// Authoritative discovery and registration do not use this throttle.
    pub fn unavailable_retry_due(&self, id: &str) -> bool {
        self.sources.get(id).is_some_and(|r| match r.value() {
            SourceState::Unavailable {
                failed_at,
                recovery,
            } => recovery.is_none() && failed_at.elapsed() >= std::time::Duration::from_secs(1),
            SourceState::Ready(_) => false,
        })
    }

    /// Recover an eligible unavailable id with one lazily invoked factory.
    /// Ready ids return their current source; absent ids return `SourceNotFound`.
    /// Backoff and active recovery return `SourceUnavailable` without invoking
    /// or waiting for the factory. Failure, cancellation and unwinding start a
    /// fresh one-second backoff. The returned source must retain the requested id.
    ///
    /// Registration, removal and discovery supersede this attempt. An obsolete
    /// result is discarded and the current [`Self::require`] result is returned.
    /// Unpublished outputs start closing unless the same Arc remains registered.
    /// The returned Arc is a snapshot; subsequent registration may replace it.
    pub async fn recover_unavailable<F, Fut>(
        &self,
        id: &str,
        factory: F,
    ) -> MaptileResult<Arc<dyn TileSource>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = MaptileResult<Arc<dyn TileSource>>>,
    {
        let token = {
            let mut entry = self
                .sources
                .get_mut(id)
                .ok_or_else(|| MaptileError::SourceNotFound(id.into()))?;
            match entry.value_mut() {
                SourceState::Ready(source) => return Ok(source.clone()),
                SourceState::Unavailable {
                    failed_at,
                    recovery,
                } => {
                    if recovery.is_some() || failed_at.elapsed() < std::time::Duration::from_secs(1)
                    {
                        return Err(MaptileError::SourceUnavailable(id.into()));
                    }
                    let token = Arc::new(());
                    *recovery = Some(token.clone());
                    token
                }
            }
        };
        let mut guard = RecoveryGuard {
            manager: self,
            id,
            token,
            pending: PendingSources {
                sources: Vec::new(),
                manager: self,
            },
        };
        let result = match factory().await {
            Ok(source) => {
                guard.pending.sources.push(source.clone());
                if source.id() == id {
                    Ok(source)
                } else {
                    Err(MaptileError::Config(format!(
                        "source factory returned a different id for {id}"
                    )))
                }
            }
            Err(error) => Err(error),
        };
        let owned = if let Some(mut entry) = self.sources.get_mut(id)
            && matches!(entry.value(), SourceState::Unavailable { recovery: Some(token), .. }
                if Arc::ptr_eq(token, &guard.token))
        {
            *entry = match &result {
                Ok(source) => SourceState::Ready(source.clone()),
                Err(_) => SourceState::Unavailable {
                    failed_at: tokio::time::Instant::now(),
                    recovery: None,
                },
            };
            true
        } else {
            false
        };
        if owned && result.is_ok() {
            guard.pending.sources.clear();
        }
        drop(guard);
        if owned { result } else { self.require(id) }
    }

    /// Resolve the current source, preserving a retryable loading failure.
    pub fn require(&self, id: &str) -> MaptileResult<Arc<dyn TileSource>> {
        match self.sources.get(id).as_deref() {
            Some(SourceState::Ready(source)) => Ok(source.clone()),
            Some(SourceState::Unavailable { .. }) => {
                Err(MaptileError::SourceUnavailable(id.into()))
            }
            None => Err(MaptileError::SourceNotFound(id.into())),
        }
    }

    pub fn remove(&self, id: &str) -> Option<Arc<dyn TileSource>> {
        self.sources.remove(id).and_then(|(_, state)| match state {
            SourceState::Ready(source) => {
                source.start_close();
                Some(source)
            }
            SourceState::Unavailable { .. } => None,
        })
    }

    /// Remove a source and wait for its session-owned resources to close.
    pub async fn close_source(&self, id: &str) -> MaptileResult<()> {
        let source = self.remove(id);
        if let Some(cache) = &self.cache {
            cache.invalidate_source(id);
        }
        if let Some(source) = source {
            source.close().await?;
        }
        Ok(())
    }

    pub fn list_ids(&self) -> Vec<String> {
        self.sources
            .iter()
            .filter(|r| matches!(r.value(), SourceState::Ready(_)))
            .map(|r| r.key().clone())
            .collect()
    }

    /// Reload the registry to match the contents of a `DiscoveryReport`.
    ///
    /// `factory` is invoked for every newly-discovered or unavailable source id;
    /// ready sources are left in place (no replacement). Entries that disappeared
    /// from the report are removed, including unavailable records. The returned
    /// [`crate::watch::ReloadDelta`] lists the ids that were added /
    /// removed in this call.
    /// Duplicate discovered ids return `Config` before any factory is called
    /// or registry entries are changed, even when the duplicate id is ready.
    /// Each factory result must retain the discovered id; a mismatch returns
    /// `Config` and starts closing unpublished sources built by this call.
    /// Factory results that are still registered as the same `Arc` stay open.
    /// All new sources are built before applying changes. A failed or cancelled
    /// build starts closing unpublished sources and leaves this call's changes
    /// unapplied. The apply phase is not a transaction across concurrent writers.
    ///
    /// Pair with [`crate::watch::SourceWatcher`] to drive a reload loop:
    /// re-run [`crate::discovery::scan`] on each `WatchEvent`, then call
    /// this helper.
    pub async fn reload_from_discovery<F, Fut>(
        &self,
        report: &crate::discovery::DiscoveryReport,
        factory: F,
    ) -> MaptileResult<crate::watch::ReloadDelta>
    where
        F: Fn(&crate::discovery::DiscoveredSource) -> Fut,
        Fut: std::future::Future<Output = MaptileResult<Arc<dyn TileSource>>>,
    {
        use std::collections::BTreeSet;

        let mut target = BTreeSet::new();
        for source in &report.sources {
            if !target.insert(source.id.clone()) {
                return Err(MaptileError::Config(format!(
                    "duplicate discovered source id: {}",
                    source.id
                )));
            }
        }
        let mut current = BTreeSet::new();
        let mut ready = BTreeSet::new();
        for entry in &self.sources {
            current.insert(entry.key().clone());
            if matches!(entry.value(), SourceState::Ready(_)) {
                ready.insert(entry.key().clone());
            }
        }
        let mut pending = PendingSources {
            sources: Vec::new(),
            manager: self,
        };
        for src in &report.sources {
            if !ready.contains(&src.id) {
                let built = factory(src).await?;
                let matches_id = built.id() == src.id;
                pending.sources.push(built);
                if !matches_id {
                    return Err(MaptileError::Config(format!(
                        "source factory returned a different id for {}",
                        src.id
                    )));
                }
            }
        }
        let mut delta = crate::watch::ReloadDelta::default();
        for source in pending.sources.drain(..) {
            delta.added.push(source.id().to_string());
            self.register(source);
        }
        for id in current.difference(&target) {
            self.remove(id);
            delta.removed.push(id.clone());
        }
        delta.added.sort();
        delta.removed.sort();
        Ok(delta)
    }

    /// Build a [`crate::sources::CompositeSource`] from already-registered
    /// child ids and register it under `composite_id`. Mirrors Martin's
    /// implicit `/foo,bar/{z}/{x}/{y}.pbf` URL syntax: a server adapter
    /// can split the request path on commas, then call this helper to
    /// register a composite on demand. Returns `SourceNotFound` if any id
    /// is unknown, or `Config` if the child set is empty / not all MVT.
    pub fn register_composite_from_ids(
        &self,
        composite_id: impl Into<String>,
        child_ids: &[&str],
    ) -> MaptileResult<()> {
        let mut children: Vec<crate::sources::CompositeChild> = Vec::with_capacity(child_ids.len());
        for id in child_ids {
            let src = self
                .get(id)
                .ok_or_else(|| MaptileError::SourceNotFound((*id).to_string()))?;
            children.push(crate::sources::CompositeChild::new(src));
        }
        let composite = crate::sources::CompositeSource::new(composite_id, children)?;
        self.register(Arc::new(composite));
        Ok(())
    }

    /// Resolve `id` and fetch the tile, consulting the shared cache for
    /// non-query requests. Errors `SourceNotFound` if `id` is unknown.
    ///
    /// Cache-miss requests for the same (source, z, x, y) are coalesced:
    /// all concurrent waiters share a single source call via moka's entry
    /// API (no thundering herd). Cache-miss errors are cloned out of the
    /// shared `Arc<MaptileError>` and surfaced unchanged — the structured
    /// variant is preserved (now that `MaptileError: Clone`).
    #[tracing::instrument(
        target = "maptile_core::sources::manager",
        level = "debug",
        skip_all,
        fields(source.id = %id, tile.z = xyz.z, tile.x = xyz.x, tile.y = xyz.y)
    )]
    pub async fn get_tile(
        &self,
        id: &str,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let source = self.require(id)?;

        self.get_tile_from_source(source, xyz, query).await
    }

    /// Fetch through the same hot cache while preserving source validators.
    pub async fn get_tile_with_etag(
        &self,
        id: &str,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
        if_none_match: Option<&str>,
    ) -> MaptileResult<TileEtagResponse> {
        let source = self.require(id)?;
        if source.cache_zoom().is_disabled() {
            crate::sources::validate_source_request(source.as_ref(), &[xyz], query)?;
            // Dynamic sources retain their own one-snapshot validator semantics.
            let response = source.get_tile_with_etag(xyz, query, if_none_match).await?;
            if let TileEtagResponse::Fresh { resp, .. } = &response {
                check_tile_size(resp)?;
            }
            return Ok(response);
        }
        let resp = self
            .get_tile_from_source(source.clone(), xyz, query)
            .await?;
        let etag = source.tile_etag(&resp);
        if Some(etag.as_str()) == if_none_match {
            Ok(TileEtagResponse::NotModified)
        } else {
            Ok(TileEtagResponse::Fresh { resp, etag })
        }
    }

    pub(super) async fn get_tile_from_source(
        &self,
        source: Arc<dyn TileSource>,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let id = source.id();
        let selection_fallback = source.mosaic_pixel_selection();
        crate::sources::validate_source_request(source.as_ref(), &[xyz], query)?;
        let effective_selection =
            selection_fallback.map(|fallback| effective_pixel_selection(query, fallback));

        let render_style = query.and_then(|q| q.style);

        // A request is cacheable when all three hold:
        // 1. We have a cache attached.
        // 2. The query (if any) carries no non-style inputs — `url_query`,
        //    `filter`, and `bbox` change the underlying data and therefore
        //    bypass the cache. `style` joins the cache key instead.
        // 3. The source's cache_zoom() policy includes this zoom.
        let bypass = query.is_some_and(TileQuery::bypasses_cache);
        if let Some(cache) = self.cache.as_ref()
            && !bypass
            && source.cache_zoom().contains(xyz.z)
        {
            let (tile_size, format_tag) = render_style.map_or((256, 1), |style| {
                let format_tag = match style.format {
                    crate::raster::style::RasterOutputFormat::Png => 0,
                    crate::raster::style::RasterOutputFormat::Webp => 1,
                    crate::raster::style::RasterOutputFormat::Jpeg => 2,
                };
                (style.tile_size, format_tag)
            });
            let style_hash = match (query.and_then(|q| q.style_hash), effective_selection) {
                (Some(hash), Some(selection)) => {
                    let mut hasher = blake3::Hasher::new();
                    hasher.update(b"mosaic-selection\0");
                    hasher.update(hash.as_bytes());
                    hasher.update(&[selection.cache_tag()]);
                    crate::cache::StyleHash::from_bytes(*hasher.finalize().as_bytes())
                }
                (Some(hash), None) => hash,
                (None, selection) => crate::cache::hashing::canonical_render_hash(
                    render_style,
                    selection,
                    tile_size,
                    1,
                    format_tag,
                ),
            };
            let key = TileCacheKey {
                source_id: id.into(),
                content_version: source.content_version(),
                z: xyz.z,
                x: xyz.x,
                y: xyz.y,
                style_hash,
            };
            let source_for_compute = source.clone();
            // The cached fill must use the same effective style as the key,
            // so forward only the style portion of `query` (bypass inputs
            // have been excluded by the check above).
            let cached_style = query.and_then(|q| q.style);
            let cached_selection = effective_selection;
            return cache
                .get_or_insert_validated(key, async move {
                    let inner = TileQuery {
                        style: cached_style,
                        style_hash: query.and_then(|q| q.style_hash),
                        pixel_selection: cached_selection,
                        ..TileQuery::default()
                    };
                    let inner = (query.is_some() || cached_selection.is_some()).then_some(&inner);
                    source_for_compute.get_tile(xyz, inner).await
                })
                .await;
        }

        let tile = source.get_tile(xyz, query).await?;
        check_tile_size(&tile)?;
        Ok(tile)
    }

    /// Snapshot every registered source's TileJSON. Used by `/catalog` style
    /// endpoints. Returned as owned `TileJSON` values to avoid holding
    /// DashMap shard locks across the call.
    pub fn snapshot_tilejson(&self) -> Vec<tilejson::TileJSON> {
        self.sources
            .iter()
            .filter_map(|r| match r.value() {
                SourceState::Ready(source) => Some(source.tilejson().clone()),
                SourceState::Unavailable { .. } => None,
            })
            .collect()
    }

    /// Build a single `index.json`-style catalog snapshot covering data
    /// sources and styles. Each non-source store is
    /// optional — pass `None` for the slots the caller does not serve.
    ///
    /// `base_url` (when supplied) is forwarded to
    /// [`crate::styles::snapshot`] to populate each [`crate::styles::StyleInfo::url`].
    pub async fn snapshot_catalog(
        &self,
        styles: Option<&dyn crate::styles::StyleStore>,
        base_url: Option<&str>,
    ) -> MaptileResult<Catalog> {
        let mut sources: Vec<CatalogSource> = self
            .sources
            .iter()
            .filter_map(|r| match r.value() {
                SourceState::Ready(src) => Some(CatalogSource {
                    id: src.id().to_string(),
                    info: src.tile_info(),
                    tilejson: src.tilejson().clone(),
                }),
                SourceState::Unavailable { .. } => None,
            })
            .collect();
        sources.sort_by(|a, b| a.id.cmp(&b.id));

        let style_infos = match styles {
            Some(store) => crate::styles::snapshot(store, base_url, "").await?,
            None => Vec::new(),
        };
        Ok(Catalog {
            sources,
            styles: style_infos,
        })
    }
}

fn check_tile_size(tile: &TileResponse) -> MaptileResult<()> {
    if tile.data.len() > crate::transport::MAX_DECODED_TILE_BYTES {
        return Err(MaptileError::PayloadTooLarge {
            limit: crate::transport::MAX_DECODED_TILE_BYTES,
        });
    }
    Ok(())
}

/// One source entry inside a [`Catalog`] snapshot.
#[derive(Debug, Clone)]
pub struct CatalogSource {
    pub id: String,
    pub info: crate::sources::TileInfo,
    pub tilejson: tilejson::TileJSON,
}

/// Aggregate snapshot returned by [`SourceManager::snapshot_catalog`].
/// Maps directly to the `/index.json` payload served by typical adapter
/// crates (axum / volo).
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub sources: Vec<CatalogSource>,
    pub styles: Vec<crate::styles::StyleInfo>,
}
