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

#[derive(Default)]
pub struct SourceManager {
    sources: dashmap::DashMap<String, Arc<dyn TileSource>>,
    unavailable: dashmap::DashMap<String, tokio::time::Instant>,
    cache: Option<Arc<TileCache>>,
}

impl SourceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a shared tile cache. Returns `self` for chaining.
    /// Filter, bbox and URL inputs bypass the cache; style and mosaic selection
    /// partition the cache key. Empty queries remain cacheable.
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
        if self.sources.contains_key(&id) {
            tracing::debug!(
                source_id = %id,
                "SourceManager::register replacing an existing source with the same id"
            );
        }
        let current = src.clone();
        if let Some(previous) = self.sources.insert(id.clone(), src)
            && !Arc::ptr_eq(&previous, &current)
        {
            previous.start_close();
        }
        self.unavailable.remove(&id);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn TileSource>> {
        self.sources.get(id).map(|r| r.value().clone())
    }

    /// Keep a failed active source distinct from a source that does not exist.
    pub fn mark_unavailable(&self, id: &str) {
        self.unavailable
            .insert(id.to_string(), tokio::time::Instant::now());
        if let Some((_, source)) = self.sources.remove(id) {
            source.start_close();
        }
    }

    pub fn is_unavailable(&self, id: &str) -> bool {
        self.unavailable.contains_key(id)
    }

    /// Foreground recovery shares a short failure backoff; authoritative events
    /// and explicit fresh registration do not use this throttle.
    pub fn unavailable_retry_due(&self, id: &str) -> bool {
        self.unavailable
            .get(id)
            .is_some_and(|failed| failed.elapsed() >= std::time::Duration::from_secs(1))
    }

    /// Resolve the current source, preserving a retryable loading failure.
    pub fn require(&self, id: &str) -> MaptileResult<Arc<dyn TileSource>> {
        self.get(id).ok_or_else(|| {
            if self.is_unavailable(id) {
                MaptileError::SourceUnavailable(id.into())
            } else {
                MaptileError::SourceNotFound(id.into())
            }
        })
    }

    pub fn remove(&self, id: &str) -> Option<Arc<dyn TileSource>> {
        self.unavailable.remove(id);
        self.sources.remove(id).map(|(_, source)| {
            source.start_close();
            source
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
        self.sources.iter().map(|r| r.key().clone()).collect()
    }

    /// Reload the registry to match the contents of a `DiscoveryReport`.
    ///
    /// `factory` is invoked for every newly-discovered source id; sources
    /// already registered are left in place (no replacement). Sources that
    /// disappeared from the report are removed. The returned
    /// [`crate::watch::ReloadDelta`] lists the ids that were added /
    /// removed in this call.
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

        let current: BTreeSet<String> = self.list_ids().into_iter().collect();
        let target: BTreeSet<String> = report.sources.iter().map(|s| s.id.clone()).collect();

        let mut delta = crate::watch::ReloadDelta::default();

        for src in &report.sources {
            if !current.contains(&src.id) {
                let built = factory(src).await?;
                self.register(built);
                delta.added.push(src.id.clone());
            }
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

        self.get_tile_from_source(id, source, xyz, query).await
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
            // Dynamic sources retain their own one-snapshot validator semantics.
            let response = source.get_tile_with_etag(xyz, query, if_none_match).await?;
            if let TileEtagResponse::Fresh { resp, .. } = &response {
                check_tile_size(resp)?;
            }
            return Ok(response);
        }
        let resp = self
            .get_tile_from_source(id, source.clone(), xyz, query)
            .await?;
        let etag = source.tile_etag(&resp);
        if Some(etag.as_str()) == if_none_match {
            Ok(TileEtagResponse::NotModified)
        } else {
            Ok(TileEtagResponse::Fresh { resp, etag })
        }
    }

    async fn get_tile_from_source(
        &self,
        id: &str,
        source: Arc<dyn TileSource>,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let requested_selection = query.and_then(|q| q.pixel_selection);
        let selection_fallback = source.mosaic_pixel_selection();
        if requested_selection.is_some() && selection_fallback.is_none() {
            return Err(MaptileError::InvalidRequest(format!(
                "source {id:?} does not support mosaic pixel_selection"
            )));
        }
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
                .get_or_insert(key, move || async move {
                    // Some(empty) marks a manager-owned cache fill. PostgreSQL
                    // source caches only run for query=None, avoiding a recursive
                    // get_or_insert of this exact same key in the shared cache.
                    let inner = TileQuery {
                        style: cached_style,
                        style_hash: query.and_then(|q| q.style_hash),
                        pixel_selection: cached_selection,
                        ..TileQuery::default()
                    };
                    let tile = source_for_compute.get_tile(xyz, Some(&inner)).await?;
                    check_tile_size(&tile)?;
                    Ok::<_, MaptileError>(tile)
                })
                .await
                .map_err(|arc_err: std::sync::Arc<MaptileError>| (*arc_err).clone());
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
            .map(|r| r.value().tilejson().clone())
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
            .map(|r| {
                let src = r.value();
                CatalogSource {
                    id: src.id().to_string(),
                    info: src.tile_info(),
                    tilejson: src.tilejson().clone(),
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::TileCache;
    use crate::sources::{MosaicPixelSelection, TileEncoding, TileFormat, TileInfo};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tilejson::tilejson;

    fn complete_style(document: serde_json::Value) -> crate::raster::style::EffectiveStyle {
        crate::raster::style::EffectiveStyle::from_raster_style(
            raster_style::Style::from_value(document).unwrap(),
        )
        .unwrap()
    }

    struct CountingSource {
        id: String,
        tj: tilejson::TileJSON,
        calls: Arc<AtomicUsize>,
        mosaic_selection: Option<MosaicPixelSelection>,
        seen_selection: Arc<Mutex<Vec<Option<MosaicPixelSelection>>>>,
    }

    #[async_trait]
    impl TileSource for CountingSource {
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
                mosaic_selection: self.mosaic_selection,
                seen_selection: self.seen_selection.clone(),
            })
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
            mosaic_selection: None,
            seen_selection: Arc::new(Mutex::new(Vec::new())),
        });
        (src, calls)
    }

    #[test]
    fn unavailable_source_state_clears_only_after_registration_or_authoritative_removal() {
        let manager = SourceManager::new();
        let (source, _) = counting_source("recovering");
        manager.register(source.clone());
        manager.mark_unavailable("recovering");
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

    #[derive(Clone)]
    struct SourceCached {
        inner: Arc<CountingSource>,
        cache: Arc<TileCache>,
        data: Option<Bytes>,
        cache_zoom: crate::cache::CacheZoomRange,
    }

    #[async_trait]
    impl TileSource for SourceCached {
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
        fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
            self.cache_zoom
        }
        async fn get_tile(
            &self,
            xyz: TileCoord,
            query: Option<&TileQuery<'_>>,
        ) -> MaptileResult<TileResponse> {
            let compute = || async {
                let mut tile = self.inner.get_tile(xyz, query).await?;
                if let Some(data) = &self.data {
                    tile.data = data.clone();
                }
                Ok::<_, MaptileError>(tile)
            };
            if query.is_none() && self.cache_zoom.contains(xyz.z) {
                let key = TileCacheKey::new(
                    self.id().into(),
                    self.content_version(),
                    xyz.z,
                    xyz.x,
                    xyz.y,
                    crate::cache::StyleHash::NONE,
                );
                return self
                    .cache
                    .get_or_insert(key, compute)
                    .await
                    .map_err(|e| (*e).clone());
            }
            compute().await
        }
    }

    #[tokio::test]
    async fn etag_cache_coalesces_and_avoids_nested_source_cache_deadlock() {
        let cache = Arc::new(TileCache::new(1, None, None).unwrap());
        let mgr = SourceManager::new().with_cache(cache.clone());
        let (inner, calls) = counting_source("nested");
        mgr.register(Arc::new(SourceCached {
            inner,
            cache,
            data: None,
            cache_zoom: crate::cache::CacheZoomRange::default(),
        }));
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        tokio::time::timeout(Duration::from_secs(1), async {
            let requests = (0..16).map(|_| mgr.get_tile_with_etag("nested", xyz, None, None));
            for response in futures_util::future::join_all(requests).await {
                assert!(matches!(response.unwrap(), TileEtagResponse::Fresh { etag, .. } if etag == "\"authoritative\""));
            }
            assert!(matches!(mgr.get_tile_with_etag("nested", xyz, None,
                Some("\"authoritative\"")).await.unwrap(), TileEtagResponse::NotModified));
        }).await.expect("shared cache must not recursively await its own fill");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let filter = crate::filter::parse_cql2("population > 0").unwrap();
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
            "bbox/filter must bypass both cache levels"
        );
    }

    #[tokio::test]
    async fn oversized_tiles_are_rejected_before_hot_cache_insert() {
        let cache = Arc::new(TileCache::new(64, None, None).unwrap());
        let mgr = SourceManager::new().with_cache(cache.clone());
        let (inner, calls) = counting_source("huge");
        mgr.register(Arc::new(SourceCached {
            inner,
            cache: cache.clone(),
            data: Some(Bytes::from(vec![
                0;
                crate::transport::MAX_DECODED_TILE_BYTES
                    + 1
            ])),
            cache_zoom: crate::cache::CacheZoomRange::default(),
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
        mgr.register(Arc::new(SourceCached {
            inner,
            cache: cache.clone(),
            data: Some(Bytes::from(vec![
                0;
                crate::transport::MAX_DECODED_TILE_BYTES
                    + 1
            ])),
            cache_zoom: crate::cache::CacheZoomRange::disabled(),
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
            mosaic_selection: Some(fallback),
            seen_selection: seen_selection.clone(),
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

        assert!(matches!(err, MaptileError::InvalidRequest(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn styled_query_partitions_cache_by_style_hash() {
        let mgr = SourceManager::new().with_cache(Arc::new(
            TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap(),
        ));
        let (src, calls) = counting_source("a");
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
            crate::sources::TileFormat::Mvt
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
        use crate::discovery::{DiscoveredSource, DiscoveredSourceKind, DiscoveryReport};
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

    #[tokio::test]
    async fn get_tile_skips_cache_outside_zoom_range() {
        use crate::cache::CacheZoomRange;

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
            mosaic_selection: None,
            seen_selection: Arc::new(Mutex::new(Vec::new())),
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
}
