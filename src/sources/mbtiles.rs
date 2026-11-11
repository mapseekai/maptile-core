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

use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::{
    ArchiveSourceType, inspect_archive, set_raster_tile_size,
};
use crate::sources::{
    SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
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
    last_modified: Option<SystemTime>,
}

impl MbtilesSource {
    pub async fn open(cfg: MbtilesConfig) -> MaptileResult<Self> {
        if !cfg.path.exists() {
            return Err(MaptileError::Io(std::sync::Arc::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("MBTiles file not found: {}", cfg.path.display()),
            ))));
        }
        let last_modified = std::fs::metadata(&cfg.path)
            .ok()
            .and_then(|m| m.modified().ok());

        let pool = Arc::new(
            mbtiles::MbtilesPool::open_readonly(&cfg.path)
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
        let archive = mbtiles::Mbtiles::new(&cfg.path).map_err(MaptileError::backend)?;
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
            last_modified,
        })
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
            last_modified: self.last_modified,
        })
    }

    /// Content version derived from `(path, mtime, size)` so the cache
    /// busts automatically when the underlying `.mbtiles` file is
    /// replaced in place under a stable source `id`.  Best-effort: if
    /// `metadata()` fails (e.g. file deleted), mtime/size fall back to
    /// `0` and we still get a deterministic fingerprint for the path.
    fn content_version(&self) -> crate::cache::ContentVersion {
        let path = &self.cfg.path;
        let path_str = path.to_string_lossy();
        let (mtime, size) = std::fs::metadata(path)
            .ok()
            .map(|m| {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (mtime, m.len())
            })
            .unwrap_or((0, 0));
        let mut buf = Vec::with_capacity(path_str.len() + 17);
        buf.extend_from_slice(path_str.as_bytes());
        buf.push(0);
        buf.extend_from_slice(&mtime.to_le_bytes());
        buf.extend_from_slice(&size.to_le_bytes());
        crate::cache::ContentVersion::from_input(&buf)
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        // MbtilesPool::get_tile handles TMS↔XYZ y-flip internally.
        let bytes = self
            .pool
            .get_tile(xyz.z, xyz.x, xyz.y)
            .await
            .map_err(MaptileError::backend)?
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        Ok(TileResponse {
            data: Bytes::from(bytes),
            info: self.info,
            last_modified: self.last_modified,
        })
    }
}
