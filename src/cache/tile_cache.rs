//! In-process tile cache for all source types.
//!
//! Byte-weighted, TTL-evicting cache backed by [`moka`]. All source types
//! (PMTiles, MBTiles, Postgres, COG, ...) share this cache by storing
//! [`crate::sources::TileResponse`] values keyed by [`TileCacheKey`].

use moka::future::Cache;
#[cfg(test)]
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use super::{ContentVersion, StyleHash};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::TileResponse;

/// Cache key: uniquely identifies a tile across all sources.
///
/// `style_hash` stores the full canonical render hash, partitioning raster
/// sources by effective style and effective mosaic selection.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TileCacheKey {
    pub source_id: Arc<str>,
    pub content_version: ContentVersion,
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub style_hash: StyleHash,
}

impl TileCacheKey {
    /// Construct a hot-cache key from the full authoritative render identity.
    #[must_use]
    pub fn new(
        source_id: Arc<str>,
        content_version: ContentVersion,
        z: u8,
        x: u32,
        y: u32,
        style: StyleHash,
    ) -> Self {
        Self {
            source_id,
            content_version,
            z,
            x,
            y,
            style_hash: style,
        }
    }
}

/// Byte-weighted, TTL-evicting tile cache.
#[derive(Clone)]
pub struct TileCache {
    cache: Cache<TileCacheKey, TileResponse>,
}

impl TileCache {
    /// Create a new cache capped at `max_size_mb` megabytes with optional
    /// TTL (`expiry`) and TTI (`idle_timeout`) eviction policies. `None`
    /// on either disables that policy.
    ///
    /// `support_invalidation_closures` is enabled so [`Self::invalidate_source`]
    /// can scan keys.
    ///
    /// # Errors
    ///
    /// Returns [`MaptileError::Config`] if `max_size_mb * 1024 * 1024`
    /// overflows `u64` — sanity-check that prevents pathological config
    /// values from silently wrapping to a tiny cache.
    pub fn new(
        max_size_mb: u64,
        expiry: Option<Duration>,
        idle_timeout: Option<Duration>,
    ) -> MaptileResult<Self> {
        let max_bytes = max_size_mb.checked_mul(1024 * 1024).ok_or_else(|| {
            MaptileError::Config(format!(
                "tile cache max_size_mb={max_size_mb} overflows u64 bytes"
            ))
        })?;
        let mut builder = Cache::builder()
            .name("tile_cache")
            .max_capacity(max_bytes)
            .weigher(|k: &TileCacheKey, v: &TileResponse| -> u32 {
                // Account for key/value storage, source id, and cache bookkeeping
                // even when a tile has an empty payload.
                v.data
                    .len()
                    .saturating_add(k.source_id.len())
                    .saturating_add(std::mem::size_of::<TileCacheKey>())
                    .saturating_add(std::mem::size_of::<TileResponse>())
                    .saturating_add(128)
                    .try_into()
                    .unwrap_or(u32::MAX)
            })
            .support_invalidation_closures();
        if let Some(ttl) = expiry {
            builder = builder.time_to_live(ttl);
        }
        if let Some(tti) = idle_timeout {
            builder = builder.time_to_idle(tti);
        }
        Ok(Self {
            cache: builder.build(),
        })
    }

    /// Look up a tile. Returns `None` on miss or after TTL expiry.
    pub async fn get(&self, key: &TileCacheKey) -> Option<TileResponse> {
        self.cache.get(key).await
    }

    /// Insert a tile. Eviction happens asynchronously in the background.
    pub async fn insert(&self, key: TileCacheKey, mut value: TileResponse) {
        value.data = bytes::Bytes::copy_from_slice(&value.data);
        self.cache.insert(key, value).await;
    }

    /// Cache-coordinated read-or-compute. Multiple concurrent waiters
    /// for the same key share a single `compute` invocation
    /// (moka entry API — no thundering herd).
    ///
    /// On compute error, all waiters receive `Arc<E>` cloning the same
    /// error reference. Caller is responsible for converting `Arc<E>`
    /// to its own error type if needed.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            source.id = %key.source_id,
            tile.z = key.z,
            tile.x = key.x,
            tile.y = key.y,
        )
    )]
    pub async fn get_or_insert<F, Fut, E>(
        &self,
        key: TileCacheKey,
        compute: F,
    ) -> Result<TileResponse, Arc<E>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<TileResponse, E>> + Send,
        E: Send + Sync + 'static,
    {
        #[cfg(feature = "metrics")]
        let zoom = key.z;

        let entry = self
            .cache
            .entry(key)
            .or_try_insert_with(async move {
                let mut tile = compute().await?;
                // A small slice may retain an entire archive or oversized Vec.
                // Cache allocations must match the visible payload we weigh.
                tile.data = bytes::Bytes::copy_from_slice(&tile.data);
                Ok::<_, E>(tile)
            })
            .await?;

        #[cfg(feature = "metrics")]
        {
            let result_label = if entry.is_fresh() { "miss" } else { "hit" };
            let zoom_label = crate::metrics::ZOOM_LABELS
                .get(zoom as usize)
                .copied()
                .unwrap_or("other");
            crate::metrics::TILE_CACHE_REQUESTS_TOTAL
                .with_label_values(&["tile", result_label, zoom_label])
                .inc();
        }

        Ok(entry.into_value())
    }

    /// Invalidate all entries. Eviction is eventually consistent.
    pub fn invalidate_all(&self) {
        self.cache.invalidate_all();
    }

    /// Invalidate all entries belonging to a single source.
    ///
    /// Eviction is eventually consistent — call [`Self::run_pending_tasks`]
    /// in tests to flush. Requires the moka builder's
    /// `support_invalidation_closures` flag (enabled in [`Self::new`]).
    pub fn invalidate_source(&self, source_id: &str) {
        let owned = source_id.to_string();
        // Invariant: support_invalidation_closures was enabled at
        // construction, so this call cannot fail.
        self.cache
            .invalidate_entries_if(move |key, _| key.source_id.as_ref() == owned)
            .expect("invalidate_entries_if predicate cannot fail");
        tracing::info!(
            source.id = %source_id,
            "invalidated tile cache for source"
        );
    }

    /// Run pending maintenance (flush invalidations, evictions). Useful
    /// in tests; no-op-ish in production where moka runs background tasks.
    pub async fn run_pending_tasks(&self) {
        self.cache.run_pending_tasks().await;
    }

    /// Current number of cached entries (approximate).
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Current weighted size in bytes (approximate).
    #[must_use]
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }
}

impl std::fmt::Debug for TileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileCache")
            .field("entry_count", &self.cache.entry_count())
            .field("weighted_size_bytes", &self.cache.weighted_size())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{TileEncoding, TileFormat, TileInfo};
    use bytes::Bytes;

    #[test]
    fn content_version_changes_hot_cache_identity() {
        let first = TileCacheKey::new(
            Arc::from("src"),
            ContentVersion::from_input(b"version-a"),
            5,
            6,
            7,
            StyleHash::NONE,
        );
        let second = TileCacheKey::new(
            Arc::from("src"),
            ContentVersion::from_input(b"version-b"),
            5,
            6,
            7,
            StyleHash::NONE,
        );
        assert_ne!(first, second);
    }

    fn make_tile(size: usize) -> TileResponse {
        TileResponse {
            data: Bytes::from(vec![0u8; size]),
            info: TileInfo {
                format: TileFormat::Mvt,
                encoding: TileEncoding::Uncompressed,
            },
            last_modified: None,
        }
    }

    #[tokio::test]
    async fn test_cache_insert_and_get() {
        let cache = TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap();
        let key = TileCacheKey {
            source_id: "test".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 14,
            x: 8580,
            y: 5737,
            style_hash: super::StyleHash::NONE,
        };
        cache.insert(key.clone(), make_tile(1024)).await;
        let result = cache.get(&key).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().data.len(), 1024);
    }

    #[tokio::test]
    async fn cache_copies_small_slices_out_of_large_allocations() {
        let cache = TileCache::new(1, None, None).unwrap();
        let backing = bytes::Bytes::from(vec![1; 4 * 1024 * 1024]);
        let mut tile = make_tile(0);
        tile.data = backing.slice(0..2);
        let key = TileCacheKey::new(
            "slice".into(),
            ContentVersion::from_input(b"test"),
            0,
            0,
            0,
            StyleHash::NONE,
        );
        cache.insert(key.clone(), tile.clone()).await;
        let cached = cache.get(&key).await.unwrap();
        assert_eq!(cached.data, tile.data);
        assert_ne!(cached.data.as_ptr(), backing.as_ptr());
        cache.invalidate_all();
        cache.run_pending_tasks().await;
        let cached = cache
            .get_or_insert(key, || async { Ok::<_, MaptileError>(tile) })
            .await
            .unwrap();
        assert_ne!(cached.data.as_ptr(), backing.as_ptr());
    }

    #[tokio::test]
    async fn empty_tiles_consume_capacity_and_are_evicted() {
        let cache = TileCache::new(1, None, None).unwrap();
        for x in 0..10_000 {
            let key = TileCacheKey::new(
                "empty".into(),
                ContentVersion::from_input(b"test"),
                14,
                x,
                0,
                StyleHash::NONE,
            );
            cache.insert(key, make_tile(0)).await;
        }
        cache.run_pending_tasks().await;
        assert!(cache.weighted_size() > 0);
        assert!(cache.weighted_size() <= 1024 * 1024);
        assert!(cache.entry_count() < 10_000);
    }

    #[tokio::test]
    async fn test_cache_miss() {
        let cache = TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap();
        let key = TileCacheKey {
            source_id: "test".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 14,
            x: 8580,
            y: 5737,
            style_hash: super::StyleHash::NONE,
        };
        assert!(cache.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn test_cache_weighted_size() {
        let cache = TileCache::new(10, Some(Duration::from_secs(3600)), None).unwrap();
        for i in 0..5 {
            let key = TileCacheKey {
                source_id: "test".into(),
                content_version: ContentVersion::from_input(b"test"),
                z: 14,
                x: i,
                y: 0,
                style_hash: super::StyleHash::NONE,
            };
            cache.insert(key, make_tile(1000)).await;
        }
        cache.run_pending_tasks().await;
        assert!(cache.weighted_size() >= 4000);
    }

    #[tokio::test]
    async fn test_cache_different_sources_do_not_collide() {
        let cache = TileCache::new(10, Some(Duration::from_secs(3600)), None).unwrap();
        let k1 = TileCacheKey {
            source_id: "source-a".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 1,
            x: 0,
            y: 0,
            style_hash: super::StyleHash::NONE,
        };
        let k2 = TileCacheKey {
            source_id: "source-b".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 1,
            x: 0,
            y: 0,
            style_hash: super::StyleHash::NONE,
        };
        cache.insert(k1.clone(), make_tile(100)).await;
        assert!(
            cache.get(&k2).await.is_none(),
            "different source must not collide"
        );
    }

    #[tokio::test]
    async fn test_cache_invalidate_all() {
        let cache = TileCache::new(10, Some(Duration::from_secs(3600)), None).unwrap();
        let key = TileCacheKey {
            source_id: "src".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 0,
            x: 0,
            y: 0,
            style_hash: super::StyleHash::NONE,
        };
        cache.insert(key.clone(), make_tile(512)).await;
        cache.run_pending_tasks().await;
        assert!(cache.get(&key).await.is_some());

        cache.invalidate_all();
        cache.run_pending_tasks().await;
        assert!(
            cache.get(&key).await.is_none(),
            "entry should be gone after invalidate_all"
        );
    }

    #[tokio::test]
    async fn test_cache_entry_count() {
        let cache = TileCache::new(10, Some(Duration::from_secs(3600)), None).unwrap();
        for i in 0..3 {
            let key = TileCacheKey {
                source_id: "src".into(),
                content_version: ContentVersion::from_input(b"test"),
                z: 0,
                x: i,
                y: 0,
                style_hash: super::StyleHash::NONE,
            };
            cache.insert(key, make_tile(100)).await;
        }
        cache.run_pending_tasks().await;
        assert_eq!(cache.entry_count(), 3);
    }

    #[test]
    fn test_cache_key_equality() {
        let k1 = TileCacheKey {
            source_id: "a".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 1,
            x: 2,
            y: 3,
            style_hash: super::StyleHash::NONE,
        };
        let k2 = TileCacheKey {
            source_id: "a".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 1,
            x: 2,
            y: 3,
            style_hash: super::StyleHash::NONE,
        };
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_cache_key_inequality_z() {
        let k1 = TileCacheKey {
            source_id: "a".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 1,
            x: 2,
            y: 3,
            style_hash: super::StyleHash::NONE,
        };
        let k2 = TileCacheKey {
            source_id: "a".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 2,
            x: 2,
            y: 3,
            style_hash: super::StyleHash::NONE,
        };
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_cache_key_hash_consistency() {
        use std::collections::hash_map::DefaultHasher;

        let k1 = TileCacheKey {
            source_id: "src".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 5,
            x: 10,
            y: 20,
            style_hash: super::StyleHash::NONE,
        };
        let k2 = TileCacheKey {
            source_id: "src".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 5,
            x: 10,
            y: 20,
            style_hash: super::StyleHash::NONE,
        };
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        k1.hash(&mut h1);
        k2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[tokio::test]
    async fn test_invalidate_source_clears_only_matching() {
        let cache = TileCache::new(10, Some(Duration::from_secs(3600)), None).unwrap();
        let k_a = TileCacheKey {
            source_id: "alpha".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 1,
            x: 0,
            y: 0,
            style_hash: super::StyleHash::NONE,
        };
        let k_b = TileCacheKey {
            source_id: "beta".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 1,
            x: 0,
            y: 0,
            style_hash: super::StyleHash::NONE,
        };
        cache.insert(k_a.clone(), make_tile(100)).await;
        cache.insert(k_b.clone(), make_tile(100)).await;
        cache.run_pending_tasks().await;
        assert_eq!(cache.entry_count(), 2);

        cache.invalidate_source("alpha");
        cache.run_pending_tasks().await;

        assert!(cache.get(&k_a).await.is_none(), "alpha must be evicted");
        assert!(cache.get(&k_b).await.is_some(), "beta must remain");
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn test_cache_debug_format() {
        let cache = TileCache::new(1, Some(Duration::from_secs(60)), None).unwrap();
        let debug = format!("{:?}", cache);
        assert!(debug.contains("TileCache"));
        assert!(debug.contains("entry_count"));
        assert!(debug.contains("weighted_size_bytes"));
    }

    #[tokio::test]
    async fn test_cache_new_is_empty() {
        let cache = TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap();
        assert_eq!(cache.entry_count(), 0);
        assert_eq!(cache.weighted_size(), 0);
    }

    #[tokio::test]
    async fn test_cache_overwrite_same_key() {
        let cache = TileCache::new(10, Some(Duration::from_secs(3600)), None).unwrap();
        let key = TileCacheKey {
            source_id: "src".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 0,
            x: 0,
            y: 0,
            style_hash: super::StyleHash::NONE,
        };
        cache.insert(key.clone(), make_tile(100)).await;
        cache.insert(key.clone(), make_tile(200)).await;
        let result = cache.get(&key).await.unwrap();
        assert_eq!(result.data.len(), 200);
    }

    #[tokio::test]
    async fn test_get_or_insert_dedups_inflight_misses() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = TileCache::new(10, Some(Duration::from_secs(3600)), None).unwrap();
        let key = TileCacheKey {
            source_id: "src".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 5,
            x: 10,
            y: 20,
            style_hash: super::StyleHash::NONE,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = Arc::new(cache);

        // Fire 16 concurrent get_or_insert calls competing on the same key.
        let mut handles = Vec::with_capacity(16);
        for _ in 0..16 {
            let cache = cache.clone();
            let key = key.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_insert(key, || async {
                        // Simulate a slow source so concurrent waiters pile up.
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        Ok::<_, std::io::Error>(make_tile(42))
                    })
                    .await
            }));
        }
        let results: Vec<_> = futures::future::join_all(handles).await;
        for r in &results {
            let resp = r.as_ref().unwrap().as_ref().unwrap();
            assert_eq!(resp.data.len(), 42);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "compute closure must run exactly once for 16 concurrent waiters"
        );
    }

    #[tokio::test]
    async fn test_get_or_insert_propagates_compute_error() {
        let cache = TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap();
        let key = TileCacheKey {
            source_id: "src".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 0,
            x: 0,
            y: 0,
            style_hash: super::StyleHash::NONE,
        };
        let result: Result<TileResponse, _> = cache
            .get_or_insert(key, || async {
                Err::<TileResponse, _>(std::io::Error::other("boom"))
            })
            .await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn tile_cache_key_new_preserves_full_style_hash() {
        use crate::cache::StyleHash;
        let first = StyleHash::from_bytes([
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let second = StyleHash::from_bytes([
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let first_key = TileCacheKey::new(
            Arc::from("src"),
            ContentVersion::from_input(b"test"),
            5,
            6,
            7,
            first,
        );
        let second_key = TileCacheKey::new(
            Arc::from("src"),
            ContentVersion::from_input(b"test"),
            5,
            6,
            7,
            second,
        );
        assert_eq!(first_key.style_hash, first);
        assert_eq!(second_key.style_hash, second);
        assert_ne!(first_key, second_key);
    }

    #[cfg(feature = "metrics")]
    #[tokio::test]
    async fn test_metrics_increment_on_miss_then_hit() {
        use prometheus::Encoder;

        let cache = TileCache::new(1, Some(Duration::from_secs(3600)), None).unwrap();
        let key = TileCacheKey {
            source_id: "metrics-test".into(),
            content_version: ContentVersion::from_input(b"test"),
            z: 5,
            x: 1,
            y: 1,
            style_hash: super::StyleHash::NONE,
        };

        // Miss
        let _ = cache
            .get_or_insert(key.clone(), || async {
                Ok::<_, std::io::Error>(make_tile(10))
            })
            .await
            .unwrap();
        // Hit
        let _ = cache
            .get_or_insert(key.clone(), || async {
                Ok::<_, std::io::Error>(make_tile(10))
            })
            .await
            .unwrap();

        // Gather and check that both labels appear with non-zero count.
        let mut buf = Vec::new();
        prometheus::TextEncoder::new()
            .encode(&prometheus::default_registry().gather(), &mut buf)
            .unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains(
                "maptile_tile_cache_requests_total{cache=\"tile\",result=\"miss\",zoom=\"5\"}"
            ),
            "miss counter not found in:\n{text}"
        );
        assert!(
            text.contains(
                "maptile_tile_cache_requests_total{cache=\"tile\",result=\"hit\",zoom=\"5\"}"
            ),
            "hit counter not found in:\n{text}"
        );
    }
}
