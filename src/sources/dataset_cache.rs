//! Bounded cache of GDAL dataset handles, partitioned by scope and revision.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gdal::Dataset;
use moka::future::Cache;
use tokio::sync::Mutex;

use super::file_stamp::FileStamp;
use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};

const DEFAULT_TTL_SECONDS: u64 = 600;
const DEFAULT_CAPACITY: u64 = 100;

pub type CachedDataset = Arc<Mutex<Dataset>>;

#[derive(Clone, PartialEq, Eq, Hash)]
struct DatasetKey {
    scope: ContentVersion,
    path: PathBuf,
    revision: ContentVersion,
}

#[derive(Clone)]
pub struct DatasetCache {
    inner: Cache<DatasetKey, CachedDataset>,
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

    /// Cache regular local TIFFs by file identity under the process scope.
    /// Keep any auxiliary files immutable. Virtual and VRT datasets are opened
    /// directly unless a specific immutable revision is supplied.
    pub async fn get_or_open(&self, path: &str) -> MaptileResult<CachedDataset> {
        self.get_or_open_scoped(path, ContentVersion::from_input(b"local"))
            .await
    }

    /// As for `get_or_open`, with an opaque caller-supplied authorization scope.
    pub async fn get_or_open_scoped(
        &self,
        path: &str,
        scope: ContentVersion,
    ) -> MaptileResult<CachedDataset> {
        let path = PathBuf::from(path);
        let Some((resolved, stamp)) = local_file_identity(path.clone()).await? else {
            return open(path, None).await;
        };
        let revision = ContentVersion::from_input(format!("{stamp:?}").as_bytes());
        self.inner
            .try_get_with(
                DatasetKey {
                    scope,
                    path: resolved.clone(),
                    revision,
                },
                open(resolved, Some(stamp)),
            )
            .await
            .map_err(|error| (*error).clone())
    }

    /// Cache an explicitly versioned dataset, including virtual paths. The
    /// path and all referenced files must identify immutable content for
    /// the supplied revision, including after the handle is evicted.
    pub async fn get_or_open_versioned(
        &self,
        path: &str,
        scope: ContentVersion,
        revision: ContentVersion,
    ) -> MaptileResult<CachedDataset> {
        let path = PathBuf::from(path);
        self.inner
            .try_get_with(
                DatasetKey {
                    scope,
                    path: path.clone(),
                    revision,
                },
                open(path, None),
            )
            .await
            .map_err(|error| (*error).clone())
    }

    pub async fn invalidate_versioned(
        &self,
        path: &str,
        scope: ContentVersion,
        revision: ContentVersion,
    ) {
        self.inner
            .invalidate(&DatasetKey {
                scope,
                path: path.into(),
                revision,
            })
            .await;
    }

    /// Evict the current process-scoped generation of a local TIFF.
    pub async fn invalidate(&self, path: &str) {
        if let Ok(Some((path, stamp))) = local_file_identity(path.into()).await {
            self.inner
                .invalidate(&DatasetKey {
                    scope: ContentVersion::from_input(b"local"),
                    path,
                    revision: ContentVersion::from_input(format!("{stamp:?}").as_bytes()),
                })
                .await;
        }
    }

    #[must_use]
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }
}

async fn local_file_identity(path: PathBuf) -> MaptileResult<Option<(PathBuf, FileStamp)>> {
    crate::transport::run_blocking(move || {
        // `/vsi*` is a GDAL virtual path even if a local file has a matching name.
        if path.to_string_lossy().starts_with("/vsi") {
            return Ok(None);
        }
        let Ok(resolved) = path.canonicalize() else {
            return Ok(None);
        };
        if !resolved
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff"))
        {
            return Ok(None);
        }
        let meta = std::fs::metadata(&resolved)?;
        if !meta.is_file() {
            return Ok(None);
        }
        let stamp = FileStamp::read(&resolved)?;
        Ok(Some((resolved, stamp)))
    })
    .await
}

async fn open(path: PathBuf, stamp: Option<FileStamp>) -> MaptileResult<CachedDataset> {
    crate::transport::run_blocking(move || {
        let dataset = Dataset::open(&path).map_err(MaptileError::backend)?;
        if let Some(stamp) = stamp
            && FileStamp::read(&path).ok().as_ref() != Some(&stamp)
        {
            return Err(MaptileError::VersionChanged(
                "GDAL dataset changed while opening".into(),
            ));
        }
        Ok(Arc::new(Mutex::new(dataset)))
    })
    .await
}

static GLOBAL: OnceLock<DatasetCache> = OnceLock::new();

/// Access the default 100-entry, 10-minute cache.
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
    async fn cache_partitions_scopes_and_reopens_replaced_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.tif");
        let replacement = dir.path().join("replacement.tif");
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        driver
            .create_with_band_type::<u8, _>(&path, 4, 4, 1)
            .unwrap();
        driver
            .create_with_band_type::<u8, _>(&replacement, 8, 8, 1)
            .unwrap();
        let cache = DatasetCache::new(10, Duration::from_secs(60));
        let path_str = path.to_str().unwrap();
        let one = ContentVersion::from_input(b"one");
        let two = ContentVersion::from_input(b"two");
        let first = cache.get_or_open_scoped(path_str, one).await.unwrap();
        let other_scope = cache.get_or_open_scoped(path_str, two).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &other_scope));
        std::fs::rename(replacement, &path).unwrap();
        let second = cache.get_or_open_scoped(path_str, one).await.unwrap();
        assert_eq!(first.lock().await.raster_size(), (4, 4));
        assert_eq!(second.lock().await.raster_size(), (8, 8));
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
