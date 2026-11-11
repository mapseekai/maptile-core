//! Cloud-Optimized GeoTIFF (COG) raster tile source via async-geotiff
//! (pure Rust, no GDAL). Replaces the GDAL-backed `cog.rs` for single-COG
//! dynamic tiling. Full adoption of async-geotiff's `Tiler::tile -> WebP`
//! pipeline (lossless WebP, requested via `TileStyle::format`); the per-request
//! style is translated from maptile's resolved `EffectiveStyle` via
//! [`crate::raster::agtiff_style::to_tile_style`].
//!
//! See docs/architecture/data-serving.md.

use std::sync::Arc;

use async_geotiff::crs::Crs;
use async_geotiff::io::{BlockCache, CacheConfig, CogReader, ReaderConfig};
use async_geotiff::tiler::Tiler;
use async_geotiff::tms::{TileCoord as AgTileCoord, TileMatrixSet};
use bytes::Bytes;
use object_store::{ObjectStore, path::Path as OsPath};
use tilejson::{TileJSON, tilejson};

use crate::error::{MaptileError, MaptileResult};
use crate::raster::agtiff_style::to_tile_style;
use crate::sources::{
    RasterPointQuery, RasterPointResponse, TileCoord, TileEncoding, TileFormat, TileInfo,
    TileQuery, TileResponse, TileSource,
};

fn backend_err(msg: String) -> MaptileError {
    MaptileError::BackendMsg(msg)
}

fn out_tile_format(f: crate::raster::style::RasterOutputFormat) -> TileFormat {
    use crate::raster::style::RasterOutputFormat as F;
    match f {
        F::Png => TileFormat::Png,
        F::Webp => TileFormat::Webp,
        F::Jpeg => TileFormat::Jpeg,
    }
}

fn reader_config(source_id: &str, block_cache: &Arc<BlockCache>) -> ReaderConfig {
    let mut config = ReaderConfig::default();
    config.source_id = source_id.to_string();
    config.block_cache = Arc::clone(block_cache);
    // Speculatively prefetch the surrounding ring of blocks. The library
    // default is 0; async-geotiff's own `examples/serve.rs` sets 1 because
    // interactive pan/zoom makes the neighbour ring exactly what the next
    // tiles need — without it every viewport tile cold-reads its blocks from
    // object storage (MinIO) serially. See async-geotiff-rs-note.md.
    config.prefetch_ring = 1;
    config
}

/// Where the COG bytes live. Built by the server from the stored URI.
#[derive(Clone)]
pub enum CogObjectSource {
    /// Public HTTP(S) COG. `url` is the full https URL.
    Http(String),
    /// Object store (e.g. MinIO) reusing an already-built store handle.
    /// `key` is the object path within that store.
    ObjectStore {
        store: Arc<dyn ObjectStore>,
        key: String,
    },
    /// Local filesystem absolute path.
    Local(String),
}

#[derive(Clone)]
pub struct AsyncCogSourceConfig {
    pub id: String,
    pub source: CogObjectSource,
    pub name: Option<String>,
    pub description: Option<String>,
    pub attribution: Option<String>,
    pub minzoom: Option<u8>,
    pub maxzoom: Option<u8>,
    pub bounds: Option<[f64; 4]>,
    pub cache_zoom: Option<crate::cache::CacheZoomRange>,
    /// Stable content fingerprint input (the source URI). Embedded into the
    /// cold-cache `cv=` so swapping the underlying file invalidates tiles.
    pub content_id: String,
    /// Process-shared decoded-block cache. `Some` ⇒ this COG's `CogReader`
    /// draws from the injected pool, so the same `source_id` served standalone
    /// and inside a mosaic reuse one decode budget (see async-geotiff-rs-note
    /// §11/§14). `None` ⇒ a private per-source cache (legacy / test default).
    pub block_cache: Option<Arc<BlockCache>>,
}

#[derive(Clone)]
pub struct AsyncCogSource {
    id: String,
    tj: TileJSON,
    reader: Arc<CogReader>,
    tiler: Arc<Tiler>,
    cache_zoom: crate::cache::CacheZoomRange,
    content_id: String,
}

impl AsyncCogSource {
    pub async fn open(cfg: AsyncCogSourceConfig) -> MaptileResult<Self> {
        if cfg.content_id.is_empty() {
            return Err(MaptileError::Config(
                "COG content identity is required".into(),
            ));
        }
        let source_id = cfg.content_id.clone();
        let block_cache = cfg
            .block_cache
            .clone()
            .unwrap_or_else(|| Arc::new(BlockCache::new(CacheConfig::default())));

        let reader = match cfg.source {
            CogObjectSource::Http(url) => CogReader::builder()
                .reader_config(reader_config(&source_id, &block_cache))
                .open_http(&url)
                .await
                .map_err(|e| MaptileError::Config(format!("open http COG: {e}")))?,
            CogObjectSource::ObjectStore { store, key } => CogReader::builder()
                .reader_config(reader_config(&source_id, &block_cache))
                .open_object_store(store, &key)
                .await
                .map_err(|e| MaptileError::Config(format!("open object_store COG: {e}")))?,
            CogObjectSource::Local(p) => {
                let store: Arc<dyn ObjectStore> =
                    Arc::new(object_store::local::LocalFileSystem::new());
                let path = OsPath::from_filesystem_path(&p)
                    .map_err(|e| MaptileError::Config(format!("bad local path {p}: {e}")))?;
                CogReader::builder()
                    .reader_config(reader_config(&source_id, &block_cache))
                    .open_object_store(store, path.as_ref())
                    .await
                    .map_err(|e| MaptileError::Config(format!("open local COG: {e}")))?
            }
        };
        let reader = Arc::new(reader);
        let tiler = Arc::new(Tiler::new(reader.clone()));

        let mut bounds = match cfg.bounds {
            Some(b) => b,
            None => tiler
                .wgs84_bounds()
                .map_err(|e| backend_err(format!("wgs84 bounds: {e}")))?,
        };

        // CRS transforms can overshoot geographic limits by a few floating-point ULPs.
        for (value, limit) in bounds.iter_mut().zip([180.0, 90.0, 180.0, 90.0]) {
            if value.is_finite() && value.abs() <= limit + 1e-9 {
                *value = value.clamp(-limit, limit);
            }
        }

        let mut tj: TileJSON = tilejson! { tiles: vec![] };
        tj.name = cfg.name.clone();
        tj.description = cfg.description.clone();
        tj.attribution = cfg.attribution.clone();
        tj.bounds = Some(tilejson::Bounds::new(
            bounds[0], bounds[1], bounds[2], bounds[3],
        ));
        tj.center = Some(tilejson::Center::new(
            (bounds[0] + bounds[2]) / 2.0,
            (bounds[1] + bounds[3]) / 2.0,
            10,
        ));
        tj.minzoom = Some(cfg.minzoom.unwrap_or(0));
        tj.maxzoom = Some(cfg.maxzoom.unwrap_or(22));
        tj.other
            .insert("format".into(), serde_json::Value::String("webp".into()));

        Ok(Self {
            id: cfg.id,
            tj,
            reader,
            tiler,
            cache_zoom: cfg.cache_zoom.unwrap_or_default(),
            content_id: cfg.content_id,
        })
    }
}

#[async_trait::async_trait]
impl TileSource for AsyncCogSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Webp,
            encoding: TileEncoding::Internal,
        }
    }

    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        self.cache_zoom
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }

    /// Content version derived from the source URI so swapping the
    /// pointed-at file renders prior cached tiles unreachable.
    fn content_version(&self) -> crate::cache::ContentVersion {
        let mut buf = self.content_id.as_bytes().to_vec();
        buf.push(0);
        crate::cache::ContentVersion::from_input(&buf)
    }

    #[tracing::instrument(name = "cog.get_tile", skip_all, fields(z = %xyz.z, x = %xyz.x, y = %xyz.y))]
    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let style = query.and_then(|q| q.style).ok_or_else(|| {
            MaptileError::InvalidRequest("canonical raster style is required".into())
        })?;
        // Validate the renderer's supported capabilities before reading pixels.
        let tile_style = to_tile_style(style)?;
        let out_format = out_tile_format(style.format);

        // `Tiler::tile` returns `Ok(None)` for tiles fully outside the dataset
        // or the projection's valid domain (async-geotiff v0.0.7). Surface that
        // as `TileNotFound`; the server maps it to HTTP 204 (no tile) rather
        // than synthesising a blank one. See async-geotiff-rs-note.md.
        let tile: Bytes = self
            .tiler
            .tile(
                AgTileCoord {
                    tms: TileMatrixSet::WebMercatorQuad,
                    z: xyz.z,
                    x: xyz.x,
                    y: xyz.y,
                },
                &tile_style,
            )
            .await
            .map_err(crate::raster::agtiff_style::render_error)?
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;

        Ok(TileResponse {
            data: tile,
            info: TileInfo {
                format: out_format,
                encoding: TileEncoding::Internal,
            },
            last_modified: None,
        })
    }

    fn supports_raster_coverage(&self) -> bool {
        super::raster_coverage::supports_coverage(&self.reader)
    }

    #[cfg(feature = "raster")]
    async fn raster_coverage(
        &self,
        query: Option<&super::raster_coverage::CoverageQuery>,
    ) -> MaptileResult<super::raster_coverage::CoverageResponse> {
        super::raster_coverage::read_coverage(self.reader.clone(), query).await
    }

    fn supports_raster_point(&self) -> bool {
        true
    }

    async fn query_raster_point(
        &self,
        query: &RasterPointQuery,
    ) -> MaptileResult<RasterPointResponse> {
        if !query.x.is_finite() || !query.y.is_finite() {
            return Err(MaptileError::InvalidStyle(
                "raster point x/y must be finite".into(),
            ));
        }
        if query.bidx.contains(&0) {
            return Err(MaptileError::InvalidStyle(
                "raster point bidx values must be 1-based".into(),
            ));
        }

        let coord_crs = parse_coord_crs(&query.coord_crs)?;
        let point = self
            .reader
            .point(query.x, query.y, &coord_crs, &query.bidx)
            .await
            .map_err(|e| MaptileError::InvalidStyle(format!("raster point query failed: {e}")))?;

        match point {
            Some(value) => Ok(RasterPointResponse {
                x: query.x,
                y: query.y,
                coord_crs: coord_crs.to_string(),
                bidx: query.bidx.clone(),
                values: value.values,
                valid: value.valid,
                outside: false,
            }),
            None => Ok(RasterPointResponse {
                x: query.x,
                y: query.y,
                coord_crs: coord_crs.to_string(),
                bidx: query.bidx.clone(),
                values: Vec::new(),
                valid: false,
                outside: true,
            }),
        }
    }
}

fn parse_coord_crs(raw: &str) -> MaptileResult<Crs> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Crs::epsg(4326));
    }
    let code = trimmed
        .strip_prefix("EPSG:")
        .or_else(|| trimmed.strip_prefix("epsg:"))
        .unwrap_or(trimmed);
    let epsg = code.parse::<u32>().map_err(|_| {
        MaptileError::InvalidStyle(format!(
            "coord_crs must be EPSG:<code> or a numeric EPSG code, got '{raw}'"
        ))
    })?;
    Ok(Crs::epsg(epsg))
}
