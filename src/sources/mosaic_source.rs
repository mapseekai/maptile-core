//! Multi-COG mosaic raster tile source via async-geotiff's `MosaicTiler`.
//!
//! `MosaicSource` opens N Cloud-Optimized GeoTIFF members via async-geotiff's
//! native mosaic pipeline: `FixedMosaicSource` returns the full member list
//! for every tile; `MosaicTiler` handles per-asset CogReader cache, warp,
//! pixel selection, and render entirely in pure Rust (no GDAL).
//!
//! The shared `BlockCache` is passed in from the server so multiple mosaic
//! sources (and individual COG sources) share the same decoded-block pool,
//! matching the behaviour of `AsyncCogSource`.

use std::sync::Arc;

use async_geotiff::bounds::Bbox;
use async_geotiff::io::BlockCache;
use async_geotiff::mosaic::{AssetRef, Backend, MosaicSource as AgMosaicSource, MosaicTiler};
use async_geotiff::tms::{TileCoord as AgTileCoord, TileMatrixSet};
use async_trait::async_trait;
use bytes::Bytes;
#[cfg(feature = "raster")]
use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
use object_store::ObjectStore;
use tilejson::{TileJSON, tilejson};

use crate::error::{MaptileError, MaptileResult};
use crate::raster::agtiff_style::to_tile_style;
use crate::raster::style::RasterOutputFormat;
use crate::sources::{
    MosaicPixelSelection, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource,
};

// ---------------------------------------------------------------------------
// Private helpers (mirrors agtiff_cog.rs — cannot import private fns)
// ---------------------------------------------------------------------------

fn backend_err(msg: String) -> MaptileError {
    MaptileError::BackendMsg(msg)
}

fn out_tile_format(f: RasterOutputFormat) -> TileFormat {
    match f {
        RasterOutputFormat::Png => TileFormat::Png,
        RasterOutputFormat::Webp => TileFormat::Webp,
        RasterOutputFormat::Jpeg => TileFormat::Jpeg,
    }
}

fn bounds_for_tilejson(bounds: [f64; 4], srid: Option<i32>) -> MaptileResult<[f64; 4]> {
    match srid {
        Some(4326) | None => Ok(bounds),
        Some(srid) if srid > 0 => project_bounds_to_wgs84(bounds, srid),
        Some(_) => Ok(bounds),
    }
}

#[cfg(feature = "raster")]
fn project_bounds_to_wgs84(bounds: [f64; 4], srid: i32) -> MaptileResult<[f64; 4]> {
    let mut src_srs = SpatialRef::from_epsg(srid as u32)
        .map_err(|e| backend_err(format!("Failed to create EPSG:{srid}: {e}")))?;
    src_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let mut dst_srs = SpatialRef::from_epsg(4326)
        .map_err(|e| backend_err(format!("Failed to create EPSG:4326: {e}")))?;
    dst_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let coord_transform = CoordTransform::new(&src_srs, &dst_srs)
        .map_err(|e| backend_err(format!("Failed to create coordinate transform: {e}")))?;

    let mut xs = [bounds[0], bounds[2], bounds[0], bounds[2]];
    let mut ys = [bounds[1], bounds[1], bounds[3], bounds[3]];
    let mut zs = [0.0; 4];
    coord_transform
        .transform_coords(&mut xs, &mut ys, &mut zs)
        .map_err(|e| backend_err(format!("Failed to transform mosaic bounds: {e}")))?;

    Ok([
        xs.iter().copied().fold(f64::INFINITY, f64::min),
        ys.iter().copied().fold(f64::INFINITY, f64::min),
        xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    ])
}

#[cfg(not(feature = "raster"))]
fn project_bounds_to_wgs84(bounds: [f64; 4], _srid: i32) -> MaptileResult<[f64; 4]> {
    Ok(bounds)
}

/// Parse the persisted registration value using the same canonical vocabulary
/// as requests. This is intentionally separate from the request-boundary
/// parser so configuration failures remain `Config` errors.
///
/// # Errors
///
/// Returns [`MaptileError::Config`] unless `s` is one of the five canonical
/// lower-case values.
pub fn pixel_selection_from_str(s: &str) -> MaptileResult<MosaicPixelSelection> {
    match s {
        "first" => Ok(MosaicPixelSelection::First),
        "highest" => Ok(MosaicPixelSelection::Highest),
        "lowest" => Ok(MosaicPixelSelection::Lowest),
        "mean" => Ok(MosaicPixelSelection::Mean),
        "median" => Ok(MosaicPixelSelection::Median),
        _ => Err(MaptileError::Config(format!(
            "invalid mosaic pixel_selection {s:?}; expected first/highest/lowest/mean/median"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Public config types
// ---------------------------------------------------------------------------

/// One member COG inside the mosaic.
pub struct MosaicMember {
    /// HTTP(S) URL or object_store-resolvable URL that async-geotiff
    /// `CogReader` can open (e.g. `https://minio.example.com/bucket/file.tif`).
    pub url: String,
    /// Stable cache identity (e.g. `"cog.<cuid>"`).  Decoded blocks keyed
    /// by this value are shared with the member's standalone `AsyncCogSource`
    /// when both use the same `BlockCache` instance.
    pub source_id: String,
    /// WGS84 bounding box `[west, south, east, north]`.
    pub bbox_wgs84: [f64; 4],
}

pub struct MosaicSourceConfig {
    pub id: String,
    pub members: Vec<MosaicMember>,
    pub pixel_selection: MosaicPixelSelection,
    /// Union WGS84 bbox.  `None` → union of member `bbox_wgs84`.
    pub bounds: Option<[f64; 4]>,
    /// SRID hint (informational).
    pub srid: Option<i32>,
    /// Object-store backend shared by all members. `Some` ⇒ each member's
    /// `url` is a *decoded object key* within this single store (single
    /// server, single bucket — async-geotiff `Backend::Store`). `None` ⇒ each
    /// member's `url` is a full HTTP(S) URL read over the unauthenticated HTTP
    /// backend (`Backend::Http`).
    pub store: Option<Arc<dyn ObjectStore>>,
}

// ---------------------------------------------------------------------------
// FixedMosaicSource — implements async-geotiff's MosaicSource trait
// ---------------------------------------------------------------------------

/// A static asset list: returns ALL members for every tile request.
/// `MosaicTiler` handles per-tile extent filtering internally — members whose
/// bbox doesn't overlap the requested tile are simply read and discarded or
/// produce empty warped images.
struct FixedMosaicSource {
    assets: Vec<AssetRef>,
}

#[async_trait]
impl AgMosaicSource for FixedMosaicSource {
    async fn statistics_assets(&self) -> async_geotiff::error::AgResult<Vec<AssetRef>> {
        Ok(self.assets.clone())
    }

    async fn assets_for_tile(
        &self,
        _coord: AgTileCoord,
    ) -> async_geotiff::error::AgResult<Vec<AssetRef>> {
        Ok(self.assets.clone())
    }
}

// ---------------------------------------------------------------------------
// MosaicSource
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MosaicSource {
    id: String,
    tj: TileJSON,
    /// Wrapped in `Arc` so `clone_source()` is cheap and `Clone` is trivially
    /// derived even though `MosaicTiler` may not itself be `Clone`.
    tiler: Arc<MosaicTiler<FixedMosaicSource>>,
    pixel_selection: MosaicPixelSelection,
    cache_zoom: crate::cache::CacheZoomRange,
    /// Stable content fingerprint: `\0`-joined member `source_id`s.
    content_id: String,
}

impl MosaicSource {
    /// Build a `MosaicSource` from config, sharing the given `BlockCache` with
    /// all other sources in the process.
    pub async fn open(
        cfg: MosaicSourceConfig,
        block_cache: Arc<BlockCache>,
    ) -> MaptileResult<Self> {
        if cfg.members.is_empty() {
            return Err(MaptileError::Config(
                "MosaicSource requires at least one member".into(),
            ));
        }

        // Union bounds: prefer explicit persisted bounds, else fold member
        // WGS84 bboxes. Persisted bounds may be in the source CRS (`cfg.srid`);
        // TileJSON and async-geotiff AssetRef bboxes require WGS84.
        let has_explicit_bounds = cfg.bounds.is_some();
        let stored_bounds = cfg.bounds.unwrap_or_else(|| {
            cfg.members.iter().fold(
                [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ],
                |acc, m| {
                    [
                        acc[0].min(m.bbox_wgs84[0]),
                        acc[1].min(m.bbox_wgs84[1]),
                        acc[2].max(m.bbox_wgs84[2]),
                        acc[3].max(m.bbox_wgs84[3]),
                    ]
                },
            )
        });
        let tilejson_bounds = bounds_for_tilejson(stored_bounds, cfg.srid)?;

        // Build AssetRef vec.
        let assets: Vec<AssetRef> = cfg
            .members
            .iter()
            .map(|m| AssetRef {
                url: m.url.clone(),
                source_id: m.source_id.clone(),
                // Bbox::new(xmin, ymin, xmax, ymax) — matches WGS84 [w,s,e,n]
                bbox: {
                    let b = if has_explicit_bounds {
                        tilejson_bounds
                    } else {
                        m.bbox_wgs84
                    };
                    Bbox::new(b[0], b[1], b[2], b[3])
                },
            })
            .collect();

        // Stable content id: join member source_ids with NUL separator.
        let content_id: String = cfg
            .members
            .iter()
            .map(|m| m.source_id.as_str())
            .collect::<Vec<_>>()
            .join("\0");

        // Build TileJSON (mirrors AsyncCogSource).
        let mut tj: TileJSON = tilejson! { tiles: vec![] };
        tj.bounds = Some(tilejson::Bounds::new(
            tilejson_bounds[0],
            tilejson_bounds[1],
            tilejson_bounds[2],
            tilejson_bounds[3],
        ));
        tj.center = Some(tilejson::Center::new(
            (tilejson_bounds[0] + tilejson_bounds[2]) / 2.0,
            (tilejson_bounds[1] + tilejson_bounds[3]) / 2.0,
            10,
        ));
        tj.minzoom = Some(0);
        tj.maxzoom = Some(22);
        tj.other
            .insert("format".into(), serde_json::Value::String("webp".into()));

        let fixed = FixedMosaicSource { assets };
        // object_store mode (production: members are MinIO keys in one bucket,
        // `Backend::Store`) vs. HTTP mode (public COG URLs / tests, the default
        // `Backend::Http`). Both share the injected `BlockCache` so a member
        // served standalone reuses its decoded blocks.
        let builder = MosaicTiler::builder(fixed).block_cache(block_cache);
        let tiler = match cfg.store {
            Some(store) => builder.backend(Backend::Store(store)).build(),
            None => builder.build(),
        };

        Ok(Self {
            id: cfg.id,
            tj,
            tiler: Arc::new(tiler),
            pixel_selection: cfg.pixel_selection,
            cache_zoom: crate::cache::CacheZoomRange::default(),
            content_id,
        })
    }
}

// ---------------------------------------------------------------------------
// TileSource impl
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl TileSource for MosaicSource {
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

    fn mosaic_pixel_selection(&self) -> Option<MosaicPixelSelection> {
        Some(self.pixel_selection)
    }

    fn content_version(&self) -> crate::cache::ContentVersion {
        crate::cache::ContentVersion::from_input(self.content_id.as_bytes())
    }

    #[tracing::instrument(name = "mosaic.get_tile", skip_all, fields(z = %xyz.z, x = %xyz.x, y = %xyz.y))]
    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let style = query.and_then(|q| q.style).ok_or_else(|| {
            MaptileError::InvalidRequest("canonical raster style is required".into())
        })?;

        let tile_style = to_tile_style(style)?;
        let out_format = out_tile_format(style.format);

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

    // query_raster_point: use the trait default ("source does not support
    // raster point queries").  A future revision could iterate members and
    // forward to the first whose WGS84 bbox covers the coordinate.
}

#[cfg(test)]
mod pixel_selection_tests {
    use super::*;

    #[test]
    fn persisted_selection_accepts_only_canonical_values() {
        for value in ["first", "highest", "lowest", "mean", "median"] {
            assert!(pixel_selection_from_str(value).is_ok(), "{value}");
        }
        for value in ["max", "min", "average", "last", "", " First"] {
            assert!(pixel_selection_from_str(value).is_err(), "{value}");
        }
    }
}
