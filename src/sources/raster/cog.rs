//! Cloud-Optimized GeoTIFF (COG) tiling through async-geotiff.
//!
//! [`AsyncCogSource`] reads local files, HTTP(S) URLs, or injected object stores
//! and renders Web Mercator tiles. Each request supplies a resolved Raster Style
//! through [`TileQuery::style`], including its PNG, WebP, or JPEG output format.
//! [`crate::raster::agtiff_style::to_tile_style`] adapts that style for the tiler.
//!
//! [`AsyncCogSourceConfig`] accepts a shared decoded-block cache and a content
//! identity supplied by the application. Each open pins an object snapshot
//! and assigns a distinct rendered-tile cache revision.
//! Direct point and coverage reads share the render admission budget and keep
//! their request permits until the underlying reader work finishes.
//! A TIFF or resource error from a raw point or coverage read can leave native
//! work without a join handle. The catalog reports unconfirmed drain for that generation.

use std::{path::PathBuf, sync::Arc};

use async_geotiff::crs::Crs;
use async_geotiff::io::{BlockCache, CacheConfig, CogReader, ReaderConfig, SnapshotPolicy};
use async_geotiff::tiler::Tiler;
use async_geotiff::tms::{TileCoord as AgTileCoord, TileMatrixSet};
use bytes::Bytes;
use object_store::{ObjectStore, path::Path as OsPath, prefix::PrefixStore};
use tilejson::{TileJSON, tilejson};

use super::{
    coverage,
    request::{self, ReaderJobs},
    snapshot::RemoteSnapshot,
};
use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::io::file_stamp::FileStamp;
use crate::raster::agtiff_style::to_tile_style;
use crate::sources::{
    RasterPointQuery, RasterPointResponse, SourceKind, TileCoord, TileEncoding, TileFormat,
    TileInfo, TileQuery, TileResponse, TileSource, request::validate_source_request,
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
    config.snapshot_policy = SnapshotPolicy::RequireValidator;
    config
}

/// COG location or object-store handle supplied by the application.
#[derive(Clone)]
pub enum CogObjectSource {
    /// Public HTTP(S) COG. `url` is the full https URL.
    Http(String),
    /// Object store (e.g. MinIO) reusing an already-built store handle.
    /// The supplied store owns cancellation and cleanup of work it detaches from the read future.
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
    /// Nonempty identity of this dataset version, such as a versioned URI or
    /// content fingerprint. It scopes decoded-block sharing along with the
    /// object location and its verified validator.
    pub content_id: String,
    /// Shared decoded-block cache for a common memory budget across readers.
    /// Readers with the same content identity can reuse decoded blocks.
    /// `None` creates a private cache with the async-geotiff default capacity.
    pub block_cache: Option<Arc<BlockCache>>,
}

#[derive(Clone)]
pub struct AsyncCogSource {
    id: String,
    tj: TileJSON,
    reader: Arc<CogReader>,
    jobs: Arc<ReaderJobs>,
    tiler: Arc<Tiler>,
    cache_zoom: crate::cache::CacheZoomRange,
    content_version: ContentVersion,
    snapshot: CogSnapshot,
}

#[derive(Clone)]
enum CogSnapshot {
    Local(Arc<LocalSnapshot>),
    Remote(Arc<RemoteSnapshot>),
}

impl CogSnapshot {
    async fn check(&self, id: &str) -> MaptileResult<()> {
        match self {
            Self::Local(local) => local.check(id).await,
            Self::Remote(remote) => remote.check(id),
        }
    }
}

struct LocalSnapshot {
    path: PathBuf,
    stamp: FileStamp,
    store: Arc<RemoteSnapshot>,
}

impl LocalSnapshot {
    async fn check(self: &Arc<Self>, id: &str) -> MaptileResult<()> {
        let local = self.clone();
        let unchanged =
            crate::transport::run_blocking(move || match FileStamp::read(&local.path) {
                Ok(stamp) => Ok(stamp == local.stamp),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            })
            .await?;
        if !unchanged {
            return Err(MaptileError::VersionChanged(id.into()));
        }
        self.store.check(id)
    }
}

impl AsyncCogSource {
    #[cfg(test)]
    pub(super) fn reader(&self) -> &Arc<CogReader> {
        &self.reader
    }

    pub async fn open(cfg: AsyncCogSourceConfig) -> MaptileResult<Self> {
        if cfg.content_id.is_empty() {
            return Err(MaptileError::Config(
                "COG content identity is required".into(),
            ));
        }
        let backend_identity = match &cfg.source {
            CogObjectSource::Http(url) => (0, url.as_str(), None),
            CogObjectSource::ObjectStore { key, .. } => (
                1,
                key.as_str(),
                Some(ContentVersion::new_generation().to_hex()),
            ),
            CogObjectSource::Local(path) => (2, path.as_str(), None),
        };
        let source_id =
            ContentVersion::from_input(&serde_json::to_vec(&(&cfg.content_id, backend_identity))?)
                .to_hex();
        let block_cache = cfg
            .block_cache
            .clone()
            .unwrap_or_else(|| Arc::new(BlockCache::new(CacheConfig::default())));

        let (reader, snapshot) = match cfg.source {
            CogObjectSource::Http(url) => {
                let (remote, resolved_id) = RemoteSnapshot::http(&url).await?;
                let snapshot = Arc::new(remote);
                let cache_id = format!("{source_id}:{resolved_id}");
                let reader = CogReader::builder()
                    .reader_config(reader_config(&cache_id, &block_cache))
                    .open_object_store(snapshot.clone(), "")
                    .await;
                snapshot.check(&cfg.id)?;
                (
                    reader.map_err(|_| MaptileError::Config("open HTTP COG failed".into()))?,
                    CogSnapshot::Remote(snapshot),
                )
            }
            CogObjectSource::ObjectStore { store, key } => {
                let snapshot = Arc::new(RemoteSnapshot::new(store));
                let reader = CogReader::builder()
                    .reader_config(reader_config(&source_id, &block_cache))
                    .open_object_store(snapshot.clone(), &key)
                    .await;
                snapshot.check(&cfg.id)?;
                (
                    reader
                        .map_err(|e| MaptileError::Config(format!("open object_store COG: {e}")))?,
                    CogSnapshot::Remote(snapshot),
                )
            }
            CogObjectSource::Local(p) => {
                let snapshot = crate::transport::run_blocking(move || {
                    let path = std::fs::canonicalize(p)?;
                    let key = OsPath::from_absolute_path(&path).map_err(MaptileError::backend)?;
                    let store = PrefixStore::new(object_store::local::LocalFileSystem::new(), key);
                    let store = Arc::new(RemoteSnapshot::local(Arc::new(store)));
                    let stamp = FileStamp::read(&path)?;
                    Ok(Arc::new(LocalSnapshot { path, stamp, store }))
                })
                .await?;
                let local_id = ContentVersion::from_input(
                    format!("{source_id}:{:?}", snapshot.stamp).as_bytes(),
                )
                .to_hex();
                let result = CogReader::builder()
                    .reader_config(reader_config(&local_id, &block_cache))
                    .open_object_store(snapshot.store.clone(), "")
                    .await;
                snapshot.check(&cfg.id).await?;
                (
                    result.map_err(|e| MaptileError::Config(format!("open local COG: {e}")))?,
                    CogSnapshot::Local(snapshot),
                )
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
            jobs: ReaderJobs::new(reader.cpu_limiter()),
            reader,
            tiler,
            cache_zoom: cfg.cache_zoom.unwrap_or_default(),
            content_version: ContentVersion::new_generation(),
            snapshot,
        })
    }

    async fn read_snapshot<T>(
        &self,
        read: impl Future<Output = MaptileResult<T>>,
    ) -> MaptileResult<T> {
        self.snapshot.check(&self.id).await?;
        let result = read.await;
        self.snapshot.check(&self.id).await?;
        result
    }

    async fn read_raw<T: Send + 'static>(
        &self,
        work: impl Future<Output = MaptileResult<T>> + Send + 'static,
    ) -> MaptileResult<T> {
        self.read_snapshot(request::reader_call(self.jobs.clone(), work))
            .await
    }
}

#[async_trait::async_trait]
impl TileSource for AsyncCogSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn source_kind(&self) -> SourceKind {
        SourceKind::Cog
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

    fn content_version(&self) -> crate::cache::ContentVersion {
        self.content_version
    }

    fn supports_raster_style(&self) -> bool {
        true
    }

    #[tracing::instrument(target = "maptile_core::sources::agtiff_cog", name = "cog.get_tile", skip_all, fields(z = %xyz.z, x = %xyz.x, y = %xyz.y))]
    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        validate_source_request(self, &[xyz], query)?;
        let style = query.and_then(|q| q.style).ok_or_else(|| {
            MaptileError::InvalidRequest("canonical raster style is required".into())
        })?;
        // Validate the renderer's supported capabilities before reading pixels.
        let tile_style = to_tile_style(style)?;
        let out_format = out_tile_format(style.format);
        self.jobs.admit()?;

        // Map an empty tiler result to TileNotFound so the application can
        // represent a coordinate outside the dataset or projection domain.
        let tile: Bytes = self
            .read_snapshot(request::render(|limiter| async {
                self.tiler
                    .as_ref()
                    .clone()
                    .with_request_limiter(limiter)
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
            }))
            .await?
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
        coverage::supports_coverage(&self.reader)
    }

    #[cfg(feature = "raster")]
    async fn raster_coverage(
        &self,
        query: Option<&coverage::CoverageQuery>,
    ) -> MaptileResult<coverage::CoverageResponse> {
        let reader = self.reader.clone();
        let query = query.cloned();
        self.read_raw(async move { coverage::read_coverage(reader, query.as_ref()).await })
            .await
    }

    fn consistent_export(&self) -> bool {
        true
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
        let (x, y) = (query.x, query.y);
        let bidx = query.bidx.clone();
        let reader = self.reader.clone();
        let point_crs = coord_crs.clone();
        let point = self
            .read_raw(async move {
                reader
                    .point(x, y, &point_crs, &bidx)
                    .await
                    .map_err(request::mark_reader_error)
                    .map_err(crate::raster::agtiff_style::render_error)
            })
            .await?;

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
