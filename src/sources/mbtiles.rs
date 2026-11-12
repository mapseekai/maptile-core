//! MBTiles tile source backed by the `mbtiles` crate (martin's pooled
//! SQLite reader). The upstream crate handles schema parsing, the TMS↔XYZ
//! y-flip, and connection pooling; this adapter only translates between
//! its types and maptile-core's `TileSource` contract.

/// Re-exports of Martin's `mbtiles` crate that expose archive lifecycle
/// operations (copy, patch, validate, metadata, zoom-window updates) so a
/// downstream `maptile-server` admin endpoint can stay on a single
/// `maptile-core` dependency rather than reaching into `mbtiles` directly.
///
/// Tile reads themselves go through [`MbtilesSource`] (which wraps
/// [`mbtiles::MbtilesPool`]); this module exposes the sister APIs.
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
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::{
    ArchiveSourceType, inspect_archive, set_raster_tile_size,
};
use crate::sources::{
    SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use async_trait::async_trait;
use bytes::Bytes;
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

#[derive(Debug)]
pub struct MbtilesSource {
    cfg: MbtilesConfig,
    pool: Arc<mbtiles::MbtilesPool>,
    tj: TileJSON,
    info: TileInfo,
    native_format: TileFormat,
    encoding: TileEncoding,
    wal_path: PathBuf,
    stamp: ArchiveStamp,
    content_version: ContentVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArchiveStamp {
    main: FileStamp,
    wal: Option<FileStamp>,
}

impl ArchiveStamp {
    fn read(path: &Path, wal_path: &Path) -> std::io::Result<Self> {
        let main = FileStamp::read(path)?;
        let wal = match std::fs::metadata(wal_path) {
            // A reader may create an empty WAL while opening a checkpointed archive.
            Ok(metadata) if metadata.len() > 32 => Some(FileStamp::from_metadata(&metadata)?),
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Self { main, wal })
    }
}

impl MbtilesSource {
    pub async fn open(cfg: MbtilesConfig) -> MaptileResult<Self> {
        let archive_path = std::fs::canonicalize(&cfg.path)?;
        let mut wal_path = archive_path.as_os_str().to_os_string();
        wal_path.push("-wal");
        let wal_path = PathBuf::from(wal_path);
        let stamp = ArchiveStamp::read(&archive_path, &wal_path)?;

        let pool = Arc::new(
            mbtiles::MbtilesPool::open_readonly(&archive_path)
                .await
                .map_err(MaptileError::backend)?,
        );

        // Pull TileJSON metadata.
        let metadata = pool.get_metadata().await.map_err(MaptileError::backend)?;

        let mut tj: TileJSON = metadata.tilejson.clone();
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

        // The declaration classifies the archive. Tile bytes are sampled only
        // to determine transport encoding, never to override that declaration.
        let archive = mbtiles::Mbtiles::new(&archive_path).map_err(MaptileError::backend)?;
        let mut conn = archive
            .open_readonly()
            .await
            .map_err(MaptileError::backend)?;
        // Query stored rows directly, independent of the declared minzoom and bounds.
        // Bound both SQLite work and the payload retained by this inspection.
        let sample: Option<Vec<u8>> = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            mbtiles::sqlx::query_scalar(
                "SELECT substr(tile_data, 1, 16777217) FROM tiles WHERE length(tile_data) > 0 LIMIT 1"
            ).fetch_optional(&mut conn),
        ).await.map_err(MaptileError::backend)?.map_err(MaptileError::backend)?;
        let sample = bytes::Bytes::from(sample.ok_or_else(|| {
            MaptileError::InvalidMetadata("MBTiles requires at least one non-empty tile".into())
        })?);
        let encoding = TileInfo::detect(&sample).encoding;
        set_raster_tile_size(
            &mut tj,
            &sample,
            TileInfo {
                format: declared_format,
                encoding,
            },
        )?;
        drop(conn);

        let native_format = declared_format;
        let public_format = cfg.serve_as.unwrap_or(declared_format);
        let info = TileInfo {
            format: public_format,
            encoding,
        };
        let inspection = inspect_archive(
            ArchiveSourceType::Mbtiles,
            &tj,
            TileInfo {
                format: native_format,
                encoding,
            },
        )?;
        let tj = inspection.tilejson;
        if ArchiveStamp::read(&cfg.path, &wal_path)? != stamp {
            return Err(MaptileError::SourceUnavailable(cfg.id));
        }
        let content_version = ContentVersion::from_input(format!("{cfg:?}:{stamp:?}").as_bytes());

        tracing::info!(
            "MbtilesSource '{}' opened: native={:?} encoding={:?} serve_as={:?}",
            cfg.id,
            native_format,
            encoding,
            cfg.serve_as
        );

        Ok(Self {
            cfg,
            pool,
            tj,
            info,
            native_format,
            encoding,
            wal_path,
            stamp,
            content_version,
        })
    }

    fn check_file(&self) -> MaptileResult<()> {
        if ArchiveStamp::read(&self.cfg.path, &self.wal_path).is_ok_and(|stamp| stamp == self.stamp)
        {
            Ok(())
        } else {
            Err(MaptileError::SourceUnavailable(self.cfg.id.clone()))
        }
    }
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
        Box::new(MbtilesSource {
            cfg: self.cfg.clone(),
            pool: self.pool.clone(),
            tj: self.tj.clone(),
            info: self.info,
            native_format: self.native_format,
            encoding: self.encoding,
            wal_path: self.wal_path.clone(),
            stamp: self.stamp.clone(),
            content_version: self.content_version,
        })
    }

    fn content_version(&self) -> ContentVersion {
        self.content_version
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        self.check_file()?;
        // A pooled connection may be opened concurrently with replacement.
        let bytes = self.pool.get_tile(xyz.z, xyz.x, xyz.y).await;
        self.check_file()?;
        let bytes = bytes
            .map_err(MaptileError::backend)?
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        Ok(TileResponse {
            data: Bytes::from(bytes),
            info: self.info,
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
