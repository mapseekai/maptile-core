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
    CATALOG_BUILD_LEASE, MAX_DECODED_TILE_BYTES, SOURCE_READ_LEASE, decode_async,
};
use async_trait::async_trait;
use bytes::Bytes;
use mbtiles::sqlx::{Connection, SqliteConnection, SqlitePool, sqlite::SqliteConnectOptions};
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
    cfg: MbtilesConfig,
    pool: SqlitePool,
    tj: TileJSON,
    info: TileInfo,
    default_encoding: TileEncoding,
    archive_path: PathBuf,
    wal_path: PathBuf,
    stamp: ArchiveStamp,
    content_version: ContentVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArchiveStamp {
    main: FileStamp,
    wal: Option<FileStamp>,
}

struct CheckoutLease {
    worker_started: Cell<bool>,
    read: Option<Arc<SourceReadLease>>,
    build: Option<Arc<CatalogBuildLease>>,
}

impl Drop for CheckoutLease {
    fn drop(&mut self) {
        if self.worker_started.get() {
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
    static CHECKOUT_LEASE: CheckoutLease;
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
                let _ = CHECKOUT_LEASE.try_with(|lease| lease.worker_started.set(true));
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
            cfg,
            pool: SqlitePool::connect_lazy_with(options),
            tj: inspection.tilejson,
            info,
            default_encoding,
            archive_path,
            wal_path,
            stamp,
            content_version,
        })
    }

    fn check_file(&self) -> MaptileResult<()> {
        if matches_archive(
            &self.cfg.path,
            &self.archive_path,
            &self.wal_path,
            &self.stamp,
        ) {
            Ok(())
        } else {
            Err(MaptileError::VersionChanged(self.cfg.id.clone()))
        }
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
        &self.cfg.id
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
        self.check_file()?;
        let pool = self.pool.clone();
        let checkout = CheckoutLease {
            worker_started: Cell::new(false),
            read: SOURCE_READ_LEASE.try_with(Arc::clone).ok(),
            build: CATALOG_BUILD_LEASE.try_with(Arc::clone).ok(),
        };
        // The SQLite worker may start before the pool can return its connection.
        let mut connection = tokio::spawn(CHECKOUT_LEASE.scope(checkout, async move {
            let result = pool.acquire().await;
            if !matches!(result, Err(mbtiles::sqlx::Error::PoolTimedOut)) {
                CHECKOUT_LEASE.with(|lease| lease.worker_started.set(false));
            }
            result
        }))
        .await
        .map_err(MaptileError::backend)?;
        if let Err(error) = self.check_file() {
            if let Ok(connection) = &mut connection {
                connection.close_on_drop();
            }
            return Err(error);
        }
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
        .fetch_optional(&mut *connection)
        .await;
        if let Err(error) = self.check_file() {
            connection.close_on_drop();
            return Err(error);
        }
        drop(connection);
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
        Ok(TileResponse {
            data,
            info: self.info.encoding(encoding),
            last_modified: Some(
                self.stamp
                    .wal
                    .as_ref()
                    .map_or(self.stamp.main.modified, |wal| {
                        self.stamp.main.modified.max(wal.modified)
                    }),
            ),
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
