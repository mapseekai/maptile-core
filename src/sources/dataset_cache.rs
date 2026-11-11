//! Process-global cache of open GDAL [`Dataset`] handles.
//!
//! # Why this exists
//!
//! Every call to `Dataset::open(path)` costs 10-50ms on a remote COG
//! because GDAL has to open the HTTP connection, fetch the TIFF header,
//! parse the IFDs, and populate the dataset-level metadata. The STAC
//! mosaic path currently opens one `Dataset` per asset per tile request,
//! so a 5-asset mosaic re-pays this cost on every tile served.
//!
//! Once opened, a `Dataset` can be safely reused across tile requests
//! (all state per-read is stack-local), so caching it eliminates that
//! latency on subsequent reads.
//!
//! # Design
//!
//! - Keyed on `path` (either a local filesystem path or a GDAL virtual
//!   path like `/vsicurl/https://...`).
//! - Values are `Arc<Mutex<Dataset>>`; the `Mutex` serializes access to
//!   GDAL datasets, which are not generally `Send + Sync`.
//! - Moka `future::Cache` handles TTL-based eviction (10 min default)
//!   and LRU size-based eviction (100 entries default).
//! - On miss, `Dataset::open` runs inside `tokio::task::spawn_blocking`
//!   because GDAL I/O is blocking.
//!
//! # Safety
//!
//! `gdal::Dataset` is `Send` but not `Sync`. We only access it
//! behind an async-aware `Mutex`, so cross-task
//! reuse is sound. The `Arc` is cheap to clone; callers pass the clone
//! into `tokio::task::spawn_blocking` and re-lock there.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gdal::Dataset;
use moka::future::Cache;
use tokio::sync::Mutex;

use crate::error::{MaptileError, MaptileResult};

const DEFAULT_TTL_SECONDS: u64 = 600;
const DEFAULT_CAPACITY: u64 = 100;

pub type CachedDataset = Arc<Mutex<Dataset>>;

fn backend_err(msg: String) -> MaptileError {
    MaptileError::BackendMsg(msg)
}

pub struct DatasetCache {
    inner: Cache<String, CachedDataset>,
}

impl DatasetCache {
    #[must_use]
    pub fn new(capacity: u64, ttl: Duration) -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(capacity)
                .time_to_live(ttl)
                .build(),
        }
    }

    /// Fetch a cached [`Dataset`] for `path`, opening it on cache miss.
    ///
    /// The returned `Arc<Mutex<Dataset>>` can be cloned freely; the
    /// underlying `Dataset` is shared across all callers.
    ///
    /// # Errors
    ///
    /// Returns a backend error if `Dataset::open` fails or the blocking
    /// task panics.
    pub async fn get_or_open(&self, path: &str) -> MaptileResult<CachedDataset> {
        self.inner
            .try_get_with(path.to_owned(), async {
                let path_owned = path.to_owned();
                let dataset = crate::transport::run_blocking(move || {
                    Dataset::open(Path::new(&path_owned))
                        .map_err(|e| backend_err(format!("failed to open dataset: {e}")))
                })
                .await?;
                Ok::<_, MaptileError>(Arc::new(Mutex::new(dataset)))
            })
            .await
            .map_err(|error| (*error).clone())
    }

    pub async fn invalidate(&self, path: &str) {
        self.inner.invalidate(path).await;
    }

    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

static GLOBAL: OnceLock<DatasetCache> = OnceLock::new();

/// Access the process-global [`DatasetCache`].
///
/// Lazily initialises with 100-entry capacity and 10-minute TTL. Future
/// work: allow config-driven tuning via `config.toml`.
pub fn global() -> &'static DatasetCache {
    GLOBAL.get_or_init(|| {
        DatasetCache::new(DEFAULT_CAPACITY, Duration::from_secs(DEFAULT_TTL_SECONDS))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn miss_on_nonexistent_path_returns_error_and_does_not_cache() {
        let cache = DatasetCache::new(10, Duration::from_secs(60));
        let path = "/nonexistent/path/will/fail/to/open.tif";
        let first = cache.get_or_open(path).await;
        assert!(first.is_err(), "first open must fail");
        assert_eq!(
            cache.entry_count(),
            0,
            "failed opens must not populate the cache"
        );
        let second = cache.get_or_open(path).await;
        assert!(second.is_err(), "second open must also fail");
        assert_eq!(cache.entry_count(), 0);
    }

    #[tokio::test]
    async fn miss_then_hit_returns_same_arc_when_gdal_can_open() {
        // Generate a tiny in-memory GeoTIFF via GDAL so the test runs
        // deterministically without a bundled fixture binary. Skips if
        // the MEM-to-GTiff driver path isn't available at runtime.
        use gdal::DriverManager;
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let path = tempdir.path().join("tiny.tif");
        let Some(driver) = DriverManager::get_driver_by_name("GTiff").ok() else {
            eprintln!("skip: GTiff driver unavailable");
            return;
        };
        let created = driver
            .create_with_band_type::<u8, _>(&path, 4, 4, 1)
            .expect("create tiny tiff");
        drop(created);

        let cache = DatasetCache::new(10, Duration::from_secs(60));
        let p = path.to_string_lossy().to_string();
        let (first, second) = tokio::join!(cache.get_or_open(&p), cache.get_or_open(&p));
        let first = first.expect("first open");
        let second = second.expect("concurrent open");
        assert!(Arc::ptr_eq(&first, &second), "same Arc on hit");
        cache.inner.run_pending_tasks().await;
        assert_eq!(cache.entry_count(), 1);
    }

    #[tokio::test]
    async fn invalidate_drops_entry() {
        use gdal::DriverManager;
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("tiny.tif");
        let Some(driver) = DriverManager::get_driver_by_name("GTiff").ok() else {
            return;
        };
        let _ = driver
            .create_with_band_type::<u8, _>(&path, 4, 4, 1)
            .expect("create tiny tiff");

        let cache = DatasetCache::new(10, Duration::from_secs(60));
        let p = path.to_string_lossy().to_string();
        cache.get_or_open(&p).await.expect("cache populated");
        cache.inner.run_pending_tasks().await;
        assert_eq!(cache.entry_count(), 1);
        cache.invalidate(&p).await;
        cache.inner.run_pending_tasks().await;
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn global_is_stable_across_calls() {
        let a = global();
        let b = global();
        assert!(std::ptr::eq(a, b));
    }
}
