//! Versioned, read-only MBTiles tile source.

/// Re-exports of Martin's `mbtiles` crate that expose archive lifecycle
/// operations (copy, patch, validate, metadata, zoom-window updates) so a
/// downstream `maptile-server` admin endpoint can stay on a single
/// `maptile-core` dependency rather than reaching into `mbtiles` directly.
///
/// Tile reads themselves go through [`MbtilesSource`].
pub mod tools {
    pub use mbtiles::{
        // validation surface
        AGG_TILES_HASH,
        AGG_TILES_HASH_AFTER_APPLY,
        AGG_TILES_HASH_BEFORE_APPLY,
        AggHashType,
        // copier
        CopyDuplicateMode,
        CopyType,
        IntegrityCheckType,
        MbtType,
        MbtTypeCli,
        // archive handles
        Mbtiles,
        MbtilesCopier,
        MbtilesPool,
        // metadata + temp construction
        Metadata,
        PatchType,
        PatchTypeCli,
        // updater
        UpdateZoomType,
        anonymous_mbtiles,
        // patcher (bindiff)
        apply_patch,
        calc_agg_tiles_hash,
        // misc helpers
        invert_y_value,
        temp_named_mbtiles,
    };
}

use super::file_stamp::FileStamp;
use crate::cache::ContentVersion;
use crate::control::{CatalogBuildLease, SourceReadLease};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::{
    ArchiveSourceType, inspect_archive, set_raster_tile_size,
};
use crate::sources::{
    SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
    validate_source_request,
};
use crate::transport::{
    CATALOG_BUILD_LEASE, MAX_DECODED_TILE_BYTES, SOURCE_READ_LEASE, decode_async, run_blocking,
};
use async_trait::async_trait;
use bytes::Bytes;
use mbtiles::sqlx::{
    Connection, Sqlite, SqliteConnection, SqlitePool, pool::PoolConnection,
    sqlite::SqliteConnectOptions,
};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tilejson::TileJSON;

#[derive(Debug, Clone)]
pub struct MbtilesConfig {
    pub id: String,
    pub path: PathBuf,
    /// Override the advertised format without re-encoding tile bytes.
    pub serve_as: Option<TileFormat>,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MbtilesSource {
    pool: SqlitePool,
    tj: TileJSON,
    info: TileInfo,
    default_encoding: TileEncoding,
    archive: Arc<ArchiveSnapshot>,
    content_version: ContentVersion,
}

#[derive(Debug)]
struct ArchiveSnapshot {
    id: String,
    alias: PathBuf,
    main: PathBuf,
    wal: PathBuf,
    stamp: ArchiveStamp,
}

#[derive(Debug, PartialEq, Eq)]
struct ArchiveStamp {
    main: FileStamp,
    wal: Option<FileStamp>,
}

struct ConnectionLease {
    completion_unconfirmed: Cell<bool>,
    read: Option<Arc<SourceReadLease>>,
    build: Option<Arc<CatalogBuildLease>>,
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        if self.completion_unconfirmed.get() {
            if let Some(read) = &self.read {
                read.mark_unconfirmed();
            }
            if let Some(build) = &self.build {
                build.mark_unconfirmed();
            }
        }
    }
}

tokio::task_local! {
    static CHECKOUT_LEASE: ConnectionLease;
}

// Keep the connection out of the pool until the last archive check passes.
struct UncheckedConnection {
    pending: Option<(PoolConnection<Sqlite>, ConnectionLease)>,
    cleanup: &'static tokio::runtime::Handle,
}

impl UncheckedConnection {
    fn get_mut(&mut self) -> &mut SqliteConnection {
        let (connection, _) = self
            .pending
            .as_mut()
            .expect("unchecked connection is present");
        connection
    }

    fn release(mut self) {
        drop(self.pending.take());
    }
}

impl Drop for UncheckedConnection {
    fn drop(&mut self) {
        if let Some((connection, lease)) = self.pending.take() {
            lease.completion_unconfirmed.set(true);
            // SQLx's pooled connection Drop needs a live Tokio context.
            self.cleanup.spawn(async move {
                if connection.close().await.is_ok() {
                    lease.completion_unconfirmed.set(false);
                }
            });
        }
    }
}

impl ArchiveSnapshot {
    async fn check(self: &Arc<Self>) -> MaptileResult<()> {
        let archive = self.clone();
        run_blocking(move || {
            if matches_archive(&archive.alias, &archive.main, &archive.wal, &archive.stamp) {
                Ok(())
            } else {
                Err(MaptileError::VersionChanged(archive.id.clone()))
            }
        })
        .await
    }
}

impl ArchiveStamp {
    fn read(path: &Path, wal_path: &Path) -> std::io::Result<Self> {
        let main = FileStamp::read(path)?;
        let wal = match FileStamp::read(wal_path) {
            // A reader may create an empty WAL while opening a checkpointed archive.
            Ok(stamp) if stamp.len() > 32 => Some(stamp),
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Self { main, wal })
    }
}

impl MbtilesSource {
    pub async fn open(mut cfg: MbtilesConfig) -> MaptileResult<Self> {
        // Keep the opening alias for replacement checks, independent of later chdir.
        cfg.path = std::path::absolute(&cfg.path)?;
        crate::transport::finish_discovery_open(Self::open_inner(cfg)).await
    }

    async fn open_inner(cfg: MbtilesConfig) -> MaptileResult<Self> {
        let archive_path = std::fs::canonicalize(&cfg.path)?;
        let mut wal_path = archive_path.as_os_str().to_os_string();
        wal_path.push("-wal");
        let wal_path = PathBuf::from(wal_path);
        let stamp = ArchiveStamp::read(&archive_path, &wal_path)?;

        let options = SqliteConnectOptions::new()
            .filename(&archive_path)
            .read_only(true)
            .thread_name(|id| {
                // SQLx calls this when establishing a worker, after any pool admission wait.
                let _ = CHECKOUT_LEASE.try_with(|lease| lease.completion_unconfirmed.set(true));
                format!("sqlx-sqlite-worker-{id}")
            });
        let archive = mbtiles::Mbtiles::new(&archive_path).map_err(MaptileError::backend)?;
        let mut conn = SqliteConnection::connect_with(&options)
            .await
            .map_err(MaptileError::backend)?;
        let inspected = async {
            let mut tj = archive
                .get_metadata(&mut conn)
                .await
                .map_err(MaptileError::backend)?
                .tilejson;
            if let Some(d) = cfg.description.as_ref() {
                tj.description = Some(d.clone());
            }
            let declared_format = tj
                .other
                .get("format")
                .and_then(|value| value.as_str())
                .and_then(parse_format_str)
                .ok_or_else(|| {
                    MaptileError::InvalidMetadata(
                        "MBTiles requires a supported format metadata value".into(),
                    )
                })?;
            // Brotli has no recognizable header and requires an explicit declaration.
            let default_encoding = if tj
                .other
                .get("compression")
                .and_then(|v| v.as_str())
                .and_then(TileEncoding::parse)
                == Some(TileEncoding::Brotli)
            {
                TileEncoding::Brotli
            } else {
                TileInfo::from(declared_format).encoding
            };
            let sample: Option<Vec<u8>> = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                mbtiles::sqlx::query_scalar(
                    "SELECT tile_data FROM tiles
                     WHERE typeof(tile_data) = 'blob' AND length(tile_data) BETWEEN 1 AND ?
                     LIMIT 1",
                )
                .bind(MAX_DECODED_TILE_BYTES as i64)
                .fetch_optional(&mut conn),
            )
            .await
            .map_err(|_| MaptileError::Deadline)?
            .map_err(MaptileError::backend)?;
            let sample = Bytes::from(sample.unwrap_or_default());
            let native_info = TileInfo {
                format: declared_format,
                encoding: TileEncoding::detect(&sample).unwrap_or(default_encoding),
            };
            set_raster_tile_size(&mut tj, &sample, native_info)?;
            Ok::<_, MaptileError>((
                inspect_archive(ArchiveSourceType::Mbtiles, &tj, native_info)?,
                default_encoding,
            ))
        }
        .await;
        if let Err(error) = conn.close().await {
            let _ = CATALOG_BUILD_LEASE.try_with(|lease| lease.mark_unconfirmed());
            return Err(MaptileError::backend(error));
        }
        let (inspection, default_encoding) = inspected?;

        let public_format = cfg.serve_as.unwrap_or(inspection.format);
        let info = TileInfo {
            format: public_format,
            encoding: inspection.encoding,
        };
        if !matches_archive(&cfg.path, &archive_path, &wal_path, &stamp) {
            return Err(MaptileError::VersionChanged(cfg.id));
        }
        let content_version =
            ContentVersion::from_input(format!("{cfg:?}:{archive_path:?}:{stamp:?}").as_bytes());

        tracing::info!(
            "MbtilesSource '{}' opened: native={:?} encoding={:?} serve_as={:?}",
            cfg.id,
            inspection.format,
            inspection.encoding,
            cfg.serve_as
        );

        Ok(Self {
            pool: SqlitePool::connect_lazy_with(options),
            tj: inspection.tilejson,
            info,
            default_encoding,
            archive: Arc::new(ArchiveSnapshot {
                id: cfg.id,
                alias: cfg.path,
                main: archive_path,
                wal: wal_path,
                stamp,
            }),
            content_version,
        })
    }
}

fn matches_archive(alias: &Path, archive: &Path, wal: &Path, stamp: &ArchiveStamp) -> bool {
    std::fs::canonicalize(alias).is_ok_and(|path| path == archive)
        && ArchiveStamp::read(archive, wal).is_ok_and(|current| current == *stamp)
}

fn parse_format_str(s: &str) -> Option<TileFormat> {
    match s.to_ascii_lowercase().as_str() {
        "png" => Some(TileFormat::Png),
        "jpg" | "jpeg" => Some(TileFormat::Jpeg),
        "webp" => Some(TileFormat::Webp),
        "pbf" | "mvt" => Some(TileFormat::Mvt),
        "mlt" => Some(TileFormat::Mlt),
        _ => None,
    }
}

#[async_trait]
impl TileSource for MbtilesSource {
    fn id(&self) -> &str {
        &self.archive.id
    }

    fn source_kind(&self) -> SourceKind {
        SourceKind::NativeArchive(ArchiveSourceType::Mbtiles)
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        self.info
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }

    fn content_version(&self) -> ContentVersion {
        self.content_version
    }

    fn start_close(&self) {
        drop(self.pool.close());
    }

    async fn close(&self) -> MaptileResult<()> {
        self.pool.close().await;
        Ok(())
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        validate_source_request(self, &[xyz], query)?;
        self.archive.check().await?;
        let cleanup = crate::control::cleanup_runtime()?;
        let pool = self.pool.clone();
        let lease = ConnectionLease {
            completion_unconfirmed: Cell::new(false),
            read: SOURCE_READ_LEASE.try_with(Arc::clone).ok(),
            build: CATALOG_BUILD_LEASE.try_with(Arc::clone).ok(),
        };
        // The SQLite worker may start before the pool can return its connection.
        let checkout = CHECKOUT_LEASE.scope(lease, async move { pool.acquire().await });
        let connection = tokio::spawn(async move {
            let mut checkout = std::pin::pin!(checkout);
            let result = checkout.as_mut().await;
            let lease = checkout.take_value().expect("checkout lease is present");
            if !matches!(result, Err(mbtiles::sqlx::Error::PoolTimedOut)) {
                lease.completion_unconfirmed.set(false);
            }
            result.map(|connection| UncheckedConnection {
                pending: Some((connection, lease)),
                cleanup,
            })
        })
        .await
        .map_err(MaptileError::backend)?;
        self.archive.check().await?;
        let mut connection = connection.map_err(|error| match error {
            mbtiles::sqlx::Error::PoolClosed => MaptileError::Cancelled,
            mbtiles::sqlx::Error::PoolTimedOut => MaptileError::CapacityExhausted,
            error => MaptileError::backend(error),
        })?;
        let row = mbtiles::sqlx::query_as::<_, (String, Option<i64>, Option<Vec<u8>>)>(
            "SELECT typeof(tile_data),
                    CASE WHEN typeof(tile_data) = 'blob' THEN length(tile_data) END,
                    CASE WHEN typeof(tile_data) = 'blob' AND length(tile_data) <= ? THEN tile_data END
             FROM tiles WHERE zoom_level = ? AND tile_column = ? AND tile_row = ?",
        )
        .bind(MAX_DECODED_TILE_BYTES as i64)
        .bind(i64::from(xyz.z))
        .bind(i64::from(xyz.x))
        .bind(i64::from(mbtiles::invert_y_value(xyz.z, xyz.y)))
        .fetch_optional(connection.get_mut())
        .await;
        self.archive.check().await?;
        connection.release();
        let missing = MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y);
        let Some((kind, size, data)) = row.map_err(MaptileError::backend)? else {
            return Err(missing);
        };
        if kind != "blob" && kind != "null" {
            return Err(MaptileError::InvalidMetadata(
                "MBTiles tile_data is not a BLOB".into(),
            ));
        }
        if size.is_some_and(|size| size > MAX_DECODED_TILE_BYTES as i64) {
            return Err(MaptileError::PayloadTooLarge {
                limit: MAX_DECODED_TILE_BYTES,
            });
        }
        let data = Bytes::from(data.ok_or(missing)?);
        let encoding = if data.is_empty() {
            TileEncoding::Uncompressed
        } else {
            TileEncoding::detect(&data).unwrap_or(self.default_encoding)
        };
        if encoding.is_encoded() {
            decode_async(data.clone(), encoding).await?;
        }
        let stamp = &self.archive.stamp;
        Ok(TileResponse {
            data,
            info: self.info.encoding(encoding),
            last_modified: Some(stamp.wal.as_ref().map_or(stamp.main.modified, |wal| {
                stamp.main.modified.max(wal.modified)
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::TileCache;
    use crate::control::CatalogService;
    use crate::service::{ReadTarget, TileStack};
    use mbtiles::sqlx::sqlite::SqlitePoolOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use tokio::time::{Duration, Instant, timeout};

    async fn open_fixture() -> (tempfile::NamedTempFile, MbtilesSource) {
        let archive = tempfile::NamedTempFile::new().unwrap();
        rusqlite::Connection::open(archive.path())
            .unwrap()
            .execute_batch(
                r#"
                CREATE TABLE metadata (name TEXT, value TEXT);
                INSERT INTO metadata VALUES
                    ('name','checkout'), ('format','pbf'), ('bounds','-180,-85,180,85'),
                    ('json','{"vector_layers":[{"id":"roads","fields":{}}]}');
                CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB);
                INSERT INTO tiles VALUES (0,0,0,X'');
                "#,
            )
            .unwrap();
        let source = MbtilesSource::open(MbtilesConfig {
            id: "sqlite".into(),
            path: archive.path().into(),
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();
        (archive, source)
    }

    fn publish(source: MbtilesSource) -> (Arc<CatalogService>, TileStack) {
        let service = Arc::new(CatalogService::new(TileCache::new(4, None, None).unwrap()));
        let mut update = service.catalog().update();
        update.put(Arc::new(source)).unwrap();
        service.catalog().publish(update).unwrap();
        let stack = TileStack::builder(service.clone()).build();
        (service, stack)
    }

    #[test]
    fn changed_archive_check_waits_for_blocking_budget() {
        const CHILD: &str = "MAPTILE_MBTILES_BLOCKING_BUDGET_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sources::mbtiles::tests::changed_archive_check_waits_for_blocking_budget",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        crate::transport::configure_blocking_concurrency(1).unwrap();
        let budget_owner = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (archive, source) = open_fixture().await;
                archive.close().unwrap();
                let (entered, started) = tokio::sync::oneshot::channel();
                let (release, wait) = std::sync::mpsc::channel();
                let job = budget_owner.spawn(run_blocking(move || {
                    entered.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok(())
                }));
                timeout(Duration::from_secs(3), started)
                    .await
                    .unwrap()
                    .unwrap();

                let read = source.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None);
                tokio::pin!(read);
                assert!(futures_util::poll!(read.as_mut()).is_pending());
                // The request must wait for admission while this runtime's blocking worker is free.
                tokio::task::spawn_blocking(|| ()).await.unwrap();
                assert!(futures_util::poll!(read.as_mut()).is_pending());
                release.send(()).unwrap();
                assert!(matches!(
                    timeout(Duration::from_secs(3), read).await.unwrap(),
                    Err(MaptileError::VersionChanged(id)) if id == "sqlite"
                ));
                job.await.unwrap().unwrap();
            });
    }

    fn pause_first_checkout(
        source: &mut MbtilesSource,
    ) -> (Arc<Notify>, Arc<Notify>, Arc<AtomicUsize>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let opens = Arc::new(AtomicUsize::new(0));
        source.pool = SqlitePoolOptions::new()
            .max_connections(1)
            .after_connect({
                let (entered, release, opens) = (entered.clone(), release.clone(), opens.clone());
                move |_, _| {
                    let first = opens.fetch_add(1, Ordering::Relaxed) == 0;
                    let (entered, release) = (entered.clone(), release.clone());
                    Box::pin(async move {
                        if first {
                            entered.notify_one();
                            release.notified().await;
                        }
                        Ok(())
                    })
                }
            })
            .connect_lazy_with((*source.pool.connect_options()).clone());
        (entered, release, opens)
    }

    #[tokio::test]
    async fn cancelled_checkout_discards_unchecked_connection() {
        let (_archive, mut source) = open_fixture().await;
        let (entered, release, opens) = pause_first_checkout(&mut source);
        let pool = source.pool.clone();
        let read =
            tokio::spawn(
                async move { source.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None).await },
            );
        timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        read.abort();
        assert!(read.await.unwrap_err().is_cancelled());
        release.notify_one();
        let connection = timeout(Duration::from_secs(3), pool.acquire())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(opens.load(Ordering::Relaxed), 2);
        drop(connection);
        pool.close().await;
    }

    #[test]
    fn dropping_a_pending_archive_check_outside_tokio_closes_the_connection() {
        use std::task::{Context, Wake, Waker};

        struct CheckoutReady(Notify);
        impl Wake for CheckoutReady {
            fn wake(self: Arc<Self>) {
                self.0.notify_one();
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (_archive, mut source) = runtime.block_on(open_fixture());
        let (entered, release, opens) =
            runtime.block_on(async { pause_first_checkout(&mut source) });
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let mut read = source.get_tile(coord, None);
        runtime.block_on(async {
            timeout(Duration::from_secs(3), async {
                tokio::select! {
                    () = entered.notified() => {},
                    result = read.as_mut() => panic!("read finished during checkout: {result:?}"),
                }
            })
            .await
            .unwrap();
        });

        let (started, waiting) = tokio::sync::oneshot::channel();
        let (unblock, blocked) = std::sync::mpsc::channel();
        runtime.spawn_blocking(move || {
            started.send(()).unwrap();
            let _ = blocked.recv();
        });
        runtime.block_on(async {
            timeout(Duration::from_secs(3), waiting)
                .await
                .unwrap()
                .unwrap()
        });

        let ready = Arc::new(CheckoutReady(Notify::new()));
        let waker = Waker::from(ready.clone());
        let mut context = Context::from_waker(&waker);
        {
            let _runtime = runtime.enter();
            assert!(read.as_mut().poll(&mut context).is_pending());
        }
        release.notify_one();
        runtime.block_on(async {
            timeout(Duration::from_secs(3), ready.0.notified())
                .await
                .unwrap()
        });
        {
            let _runtime = runtime.enter();
            assert!(read.as_mut().poll(&mut context).is_pending());
        }
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(read);
        unblock.send(()).unwrap();
        drop(runtime);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let tile = timeout(Duration::from_secs(3), source.get_tile(coord, None))
                .await
                .unwrap()
                .unwrap();
            assert!(tile.data.is_empty());
            assert_eq!(opens.load(Ordering::Relaxed), 2);
            source.close().await.unwrap();
        });
    }

    #[tokio::test]
    async fn shutdown_waits_for_cancelled_first_checkout() {
        let (_archive, mut source) = open_fixture().await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        source.pool = SqlitePoolOptions::new()
            .after_connect({
                let (entered, release) = (entered.clone(), release.clone());
                move |_, _| {
                    let (entered, release) = (entered.clone(), release.clone());
                    Box::pin(async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                }
            })
            .connect_lazy_with((*source.pool.connect_options()).clone());
        let pool = source.pool.clone();
        let (service, stack) = publish(source);
        let read = tokio::spawn(async move {
            stack
                .read(ReadTarget::new("sqlite", "test").at(TileCoord { z: 0, x: 0, y: 0 }))
                .await
        });
        timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            service
                .catalog()
                .shutdown(Instant::now() + Duration::from_millis(100))
                .await,
            Err(MaptileError::Deadline)
        ));
        assert!(matches!(
            timeout(Duration::from_secs(3), read)
                .await
                .unwrap()
                .unwrap(),
            Err(MaptileError::Cancelled)
        ));
        release.notify_one();
        service
            .catalog()
            .shutdown(Instant::now() + Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(pool.size(), 0);
    }

    #[tokio::test]
    async fn pool_wait_timeout_does_not_prevent_shutdown() {
        let (_archive, mut source) = open_fixture().await;
        source.pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(250))
            .connect_lazy_with((*source.pool.connect_options()).clone());
        let pool = source.pool.clone();
        let held = pool.acquire().await.unwrap();
        let (service, stack) = publish(source);
        let request = ReadTarget::new("sqlite", "test").at(TileCoord { z: 0, x: 0, y: 0 });
        assert!(matches!(
            stack.read(request).await,
            Err(MaptileError::CapacityExhausted)
        ));
        drop(held);
        service
            .catalog()
            .shutdown(Instant::now() + Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(pool.size(), 0);
    }
}
