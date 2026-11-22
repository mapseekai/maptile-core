//! Multi-COG mosaic raster tile source via async-geotiff's `MosaicTiler`.
//!
//! `MosaicSource` opens N Cloud-Optimized GeoTIFF members via async-geotiff's
//! native mosaic pipeline: `FixedMosaicSource` filters intersecting members by
//! per-tile WGS84 extent while preserving the full member list for global
//! statistics; `MosaicTiler` handles per-asset CogReader cache, warp, pixel
//! selection, and render entirely in pure Rust (no GDAL).
//!
//! The shared `BlockCache` is passed in from the server so multiple mosaic
//! sources (and individual COG sources) share the same decoded-block pool,
//! matching the behaviour of `AsyncCogSource`.

use std::{collections::HashSet, sync::Arc};

use async_geotiff::bounds::Bbox;
use async_geotiff::error::{AgError, AgResult};
use async_geotiff::io::BlockCache;
use async_geotiff::mosaic::{
    AssetRef, Backend, DiscoveredAssets, DiscoveryOptions, MosaicSource as AgMosaicSource,
    MosaicTiler,
};
use async_geotiff::tms::{TileCoord as AgTileCoord, TileMatrixSet};
use async_trait::async_trait;
use bytes::Bytes;
use object_store::ObjectStore;
use tilejson::{TileJSON, tilejson};

use crate::error::{MaptileError, MaptileResult};
use crate::raster::agtiff_style::to_mosaic_tile_style;
use crate::raster::style::RasterOutputFormat;
use crate::sources::{
    MosaicPixelSelection, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource, effective_pixel_selection,
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
    if !bounds.iter().all(|v| v.is_finite()) || bounds[1] > bounds[3] {
        return Err(MaptileError::Config(format!(
            "invalid mosaic bounds {bounds:?}"
        )));
    }
    let mut bounds = match srid {
        Some(4326) | None => bounds,
        Some(srid) if srid > 0 => project_bounds_to_wgs84(bounds, srid)?,
        Some(_) => bounds,
    };
    if !bounds.iter().all(|v| v.is_finite())
        || bounds[1] < -90.0
        || bounds[3] > 90.0
        || bounds[1] > bounds[3]
    {
        return Err(MaptileError::Config(format!(
            "invalid mosaic WGS84 bounds {bounds:?}"
        )));
    }
    if bounds[0] <= bounds[2] {
        (bounds[0], bounds[2]) = normalize_longitude_span(bounds[0], bounds[2]);
    }
    // TileJSON's single ordered bbox must enclose both sides of the date line.
    if bounds[0] > bounds[2] {
        bounds[0] = -180.0;
        bounds[2] = 180.0;
    }
    Ok(bounds)
}

fn project_bounds_to_wgs84(bounds: [f64; 4], srid: i32) -> MaptileResult<[f64; 4]> {
    let [xmin, ymin, xmax, ymax] = bounds;
    if xmin > xmax {
        return Err(MaptileError::Config(format!(
            "invalid mosaic bounds {bounds:?}"
        )));
    }
    let transform = proj::Proj::new_known_crs(&format!("EPSG:{srid}"), "EPSG:4326", None)
        .map_err(|e| backend_err(format!("Failed to create coordinate transform: {e}")))?;
    transform
        .transform_bounds(xmin, ymin, xmax, ymax, 21)
        .map_err(|e| backend_err(format!("Failed to transform mosaic bounds: {e}")))
}

const WORLD_BBOX: Bbox = Bbox {
    xmin: -180.0,
    ymin: -90.0,
    xmax: 180.0,
    ymax: 90.0,
};

/// Unknown extents stay eligible; `srid` describes only the explicit union bounds.
fn normalize_member_bbox([west, south, east, north]: [f64; 4]) -> Bbox {
    if ![west, south, east, north].iter().all(|v| v.is_finite())
        || south >= north
        || south < -90.0
        || north > 90.0
        || west == east
    {
        return WORLD_BBOX;
    }
    if (-180.0..=180.0).contains(&west) && (-180.0..=180.0).contains(&east) {
        // Preserve exact edges, including RFC 7946 west > east date-line crossings.
        return if west - east == 360.0 {
            WORLD_BBOX
        } else {
            Bbox::new(west, south, east, north)
        };
    }
    if west >= east {
        return WORLD_BBOX;
    }
    let (west, east) = normalize_longitude_span(west, east);
    Bbox::new(west, south, east, north)
}

fn normalize_longitude_span(west: f64, east: f64) -> (f64, f64) {
    if (-180.0..=180.0).contains(&west) && (-180.0..=180.0).contains(&east) {
        return (west, east);
    }
    let width = east - west;
    if width >= 360.0 {
        return (-180.0, 180.0);
    }
    // Reduce before shifting; adding 180 first can round a date-line sliver onto the other side.
    let west = west % 360.0;
    let west = if west < -180.0 {
        west + 360.0
    } else if west >= 180.0 {
        west - 360.0
    } else {
        west
    };
    let east = west + width;
    if east > 180.0 {
        let east = east - 360.0;
        // A nearly full turn can round past its west edge; keep the latitude band eligible.
        if east >= west {
            return (-180.0, 180.0);
        }
        (west, east)
    } else {
        (west, east)
    }
}

fn bbox_intersects_tile(asset: Bbox, tile: Bbox) -> bool {
    if asset.ymin >= tile.ymax || asset.ymax <= tile.ymin {
        return false;
    }
    if asset.xmin <= asset.xmax {
        asset.xmin < tile.xmax && asset.xmax > tile.xmin
    } else {
        // Antimeridian-crossing member covers [xmin, 180] U [-180, xmax].
        tile.xmax > asset.xmin || tile.xmin < asset.xmax
    }
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
    MosaicPixelSelection::from_name(s).ok_or_else(|| {
        MaptileError::Config(format!(
            "invalid mosaic pixel_selection {s:?}; expected first/highest/lowest/mean/median"
        ))
    })
}

// ---------------------------------------------------------------------------
// Public config types
// ---------------------------------------------------------------------------

/// One member COG inside the mosaic.
pub struct MosaicMember {
    /// HTTP(S) URL or object key that async-geotiff
    /// `CogReader` can open (e.g. `https://minio.example.com/bucket/file.tif`).
    pub url: String,
    /// Version identity of the object at `url`. Use a new identity for
    /// replacement bytes.
    pub source_id: String,
    /// WGS84 bounding box `[west, south, east, north]`, independent of `srid`.
    /// West > east denotes a date-line crossing; unwrapped longitudes are normalized.
    /// Nonfinite or degenerate bounds (including `[0; 4]`) stay eligible for every
    /// tile and count toward the candidate cap; supply accurate bounds to prune them.
    pub bbox_wgs84: [f64; 4],
}

pub struct MosaicSourceConfig {
    pub id: String,
    pub members: Vec<MosaicMember>,
    pub pixel_selection: MosaicPixelSelection,
    /// Explicit union bbox in `srid` (WGS84 by default).
    /// `None` → union of member `bbox_wgs84`. Components must be finite and south <= north.
    /// WGS84 latitudes must be in [-90, 90], including after projection.
    /// Unwrapped longitudes are accepted; WGS84 west > east is a valid date-line crossing.
    /// TileJSON uses an ordered bbox in [-180, 180], enclosing both sides of a crossing.
    pub bounds: Option<[f64; 4]>,
    /// CRS of the explicit `bounds`, when supplied; absent/nonpositive SRIDs mean WGS84.
    pub srid: Option<i32>,
    /// Object-store backend shared by all members. `Some` ⇒ each member's
    /// `url` is a *decoded object key* within this single store (single
    /// server, single bucket — async-geotiff `Backend::Store`). `None` ⇒ each
    /// member's `url` is a full HTTP(S) URL read over the unauthenticated HTTP
    /// backend (`Backend::Http`).
    /// A supplied store owns cancellation and cleanup of work it detaches from the read future.
    pub store: Option<Arc<dyn ObjectStore>>,
}

// ---------------------------------------------------------------------------
// FixedMosaicSource — implements async-geotiff's MosaicSource trait
// ---------------------------------------------------------------------------

/// Static member collection: filters candidates to members whose WGS84 bbox
/// intersects the requested tile, while retaining the full member list for
/// global `statistics_assets`.
#[derive(Clone)]
struct FixedMosaicSource {
    assets: Arc<[AssetRef]>,
}

impl FixedMosaicSource {
    fn candidates(&self, coord: AgTileCoord) -> AgResult<impl Iterator<Item = &AssetRef>> {
        let tms = async_geotiff::tms::Tms::cached(coord.tms)?;
        let tile = tms.tile_bbox(coord.z, coord.x, coord.y)?;
        let mut corners = [(tile.xmin, tile.ymin), (tile.xmax, tile.ymax)];
        // Both supported TMSs are axis-aligned and monotonic in WGS84.
        async_geotiff::warp::projection::convert_batch(
            &tms.crs(),
            &async_geotiff::crs::Crs::epsg(4326),
            &mut corners,
        )?;
        let mut tile_bbox = Bbox::new(corners[0].0, corners[0].1, corners[1].0, corners[1].1);
        let columns = match coord.tms {
            TileMatrixSet::WebMercatorQuad => 1_u64 << coord.z,
            TileMatrixSet::WorldCrs84Quad => 2_u64 << coord.z,
        };
        // Both global matrices end at exact longitudes; floating TMS bounds can fall short.
        if coord.x == 0 {
            tile_bbox.xmin = -180.0;
        }
        if u64::from(coord.x) + 1 == columns {
            tile_bbox.xmax = 180.0;
        }
        Ok(self
            .assets
            .iter()
            .filter(move |asset| bbox_intersects_tile(asset.bbox, tile_bbox)))
    }
}

#[async_trait]
impl AgMosaicSource for FixedMosaicSource {
    async fn statistics_assets(&self) -> AgResult<Vec<AssetRef>> {
        Ok(self.assets.to_vec())
    }

    async fn statistics_assets_with_options(
        &self,
        options: &DiscoveryOptions,
    ) -> AgResult<DiscoveredAssets> {
        discover(self.assets.iter(), options)
    }

    async fn assets_for_tile(&self, coord: AgTileCoord) -> AgResult<Vec<AssetRef>> {
        Ok(self.candidates(coord)?.cloned().collect())
    }

    async fn assets_for_tile_with_options(
        &self,
        coord: AgTileCoord,
        options: &DiscoveryOptions,
    ) -> AgResult<DiscoveredAssets> {
        discover(self.candidates(coord)?, options)
    }
}

fn discover<'a>(
    assets: impl Iterator<Item = &'a AssetRef>,
    options: &DiscoveryOptions,
) -> AgResult<DiscoveredAssets> {
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    for asset in assets {
        if seen.contains(asset.url.as_str()) {
            continue;
        }
        if options.max_assets > 0 && selected.len() == options.max_assets {
            return Err(AgError::Mosaic(format!(
                "tile matches at least {} assets, exceeding limit {}",
                selected.len() + 1,
                options.max_assets,
            )));
        }
        bytes = bytes
            .saturating_add(size_of::<AssetRef>())
            .saturating_add(asset.url.len())
            .saturating_add(asset.source_id.len());
        if bytes > options.max_asset_bytes {
            return Err(AgError::ResourceLimit(
                "discovered asset metadata exceeds byte limit".into(),
            ));
        }
        seen.insert(asset.url.as_str());
        selected.push(asset);
    }
    let _transient = options.work_budget.try_reserve(bytes as u64)?;
    DiscoveredAssets::from_assets(selected.into_iter().cloned().collect(), options)
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
    content_version: crate::cache::ContentVersion,
    immutable: bool,
}

impl MosaicSource {
    /// Open a fixed member list for tile reads using the given block cache.
    pub async fn open(
        cfg: MosaicSourceConfig,
        block_cache: Arc<BlockCache>,
    ) -> MaptileResult<Self> {
        Self::open_members(cfg, block_cache, false).await
    }

    /// Open a fixed member list that supports consistent exports.
    ///
    /// Every asset address must keep serving its declared version for the
    /// lifetime of the source and all its export sessions. Use version-qualified
    /// URLs or immutable object keys. Readers open lazily and can reopen evicted
    /// assets. Each open requires a validator from the backend.
    pub async fn open_immutable(
        cfg: MosaicSourceConfig,
        block_cache: Arc<BlockCache>,
    ) -> MaptileResult<Self> {
        Self::open_members(cfg, block_cache, true).await
    }

    async fn open_members(
        cfg: MosaicSourceConfig,
        block_cache: Arc<BlockCache>,
        immutable: bool,
    ) -> MaptileResult<Self> {
        if cfg.members.is_empty() {
            return Err(MaptileError::Config(
                "MosaicSource requires at least one member".into(),
            ));
        }

        if cfg
            .members
            .iter()
            .any(|member| member.url.is_empty() || member.source_id.is_empty())
        {
            return Err(MaptileError::Config(
                "mosaic members require an address and version identity".into(),
            ));
        }

        let assets: Vec<AssetRef> = cfg
            .members
            .into_iter()
            .map(|m| AssetRef {
                url: m.url,
                source_id: m.source_id,
                bbox: normalize_member_bbox(m.bbox_wgs84),
            })
            .collect();
        let tilejson_bounds = match cfg.bounds {
            Some(bounds) => {
                crate::transport::run_blocking(move || bounds_for_tilejson(bounds, cfg.srid))
                    .await?
            }
            None => assets.iter().fold(
                [
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ],
                |acc, asset| {
                    let b = asset.bbox;
                    let (west, east) = if b.xmin <= b.xmax {
                        (b.xmin, b.xmax)
                    } else {
                        (-180.0, 180.0)
                    };
                    [
                        acc[0].min(west),
                        acc[1].min(b.ymin),
                        acc[2].max(east),
                        acc[3].max(b.ymax),
                    ]
                },
            ),
        };

        let content_version = if immutable && cfg.store.is_none() {
            let manifest = format!(
                "{:?}",
                (
                    cfg.pixel_selection,
                    tilejson_bounds,
                    assets
                        .iter()
                        .map(|asset| (&asset.url, &asset.source_id, asset.bbox))
                        .collect::<Vec<_>>()
                )
            );
            crate::cache::ContentVersion::from_input(manifest.as_bytes())
        } else {
            crate::cache::ContentVersion::new_generation()
        };

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

        let fixed = FixedMosaicSource {
            assets: assets.into(),
        };
        // object_store mode (production: members are MinIO keys in one bucket,
        // `Backend::Store`) vs. HTTP mode (public COG URLs / tests, the default
        // `Backend::Http`). Both use the injected decoded-block memory budget.
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
            content_version,
            immutable,
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

    fn source_kind(&self) -> super::SourceKind {
        super::SourceKind::Mosaic
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
        self.content_version
    }

    fn consistent_export(&self) -> bool {
        self.immutable
    }

    fn supports_raster_style(&self) -> bool {
        true
    }

    #[tracing::instrument(name = "mosaic.get_tile", skip_all, fields(z = %xyz.z, x = %xyz.x, y = %xyz.y))]
    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        super::validate_source_request(self, &[xyz], query)?;
        let style = query.and_then(|q| q.style).ok_or_else(|| {
            MaptileError::InvalidRequest("canonical raster style is required".into())
        })?;

        let tile_style = to_mosaic_tile_style(
            style,
            effective_pixel_selection(query, self.pixel_selection),
        )?;
        let out_format = out_tile_format(style.format);

        let tile: Bytes = super::agtiff_request::render(|limiter| async {
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
        })
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

    #[tokio::test]
    async fn candidate_filtering_handles_dateline_missing_bbox_and_adjacent_edges() {
        let members = [
            // Normal member in Q1
            ("q1", [10.0, 10.0, 20.0, 20.0]),
            // Adjacent member touching tile (4, 8, 7) [0, 0, 22.5, 21.94] only at origin (0, 0)
            ("touching_corner", [-10.0, -10.0, 0.0, 0.0]),
            // RFC 7946 antimeridian-crossing member [170, -20, -170, -10]
            ("dateline_rfc", [170.0, -20.0, -170.0, -10.0]),
            // Unwrapped GDAL antimeridian-crossing member [175, 5, 185, 15]
            ("dateline_unwrapped", [175.0, 5.0, 185.0, 15.0]),
            // Missing/degenerate bbox [0, 0, 0, 0] -> conservatively retained for all tiles
            ("missing_bbox", [0.0, 0.0, 0.0, 0.0]),
        ];
        let assets = members
            .into_iter()
            .map(|(id, raw)| AssetRef {
                url: format!("{id}.tif"),
                source_id: id.into(),
                bbox: normalize_member_bbox(raw),
            })
            .collect();
        let fixed = FixedMosaicSource { assets };

        // Global statistics_assets retains all 5 members.
        assert_eq!(fixed.statistics_assets().await.unwrap().len(), 5);

        // Tile (4, 8, 7) covers [0, 0, 22.5, 21.943]: matches "q1" and "missing_bbox",
        // excludes "touching_corner" (zero-area corner touch) and both dateline members.
        let q1_candidates = fixed
            .assets_for_tile(AgTileCoord {
                tms: TileMatrixSet::WebMercatorQuad,
                z: 4,
                x: 8,
                y: 7,
            })
            .await
            .unwrap();
        let q1_ids: Vec<&str> = q1_candidates.iter().map(|a| a.source_id.as_str()).collect();
        assert_eq!(q1_ids, ["q1", "missing_bbox"]);

        // Easternmost tile (4, 15, 8) covers [157.5, -21.943, 180.0, 0.0]:
        // matches "dateline_rfc" and "missing_bbox".
        let east_candidates = fixed
            .assets_for_tile(AgTileCoord {
                tms: TileMatrixSet::WebMercatorQuad,
                z: 4,
                x: 15,
                y: 8,
            })
            .await
            .unwrap();
        let east_ids: Vec<&str> = east_candidates
            .iter()
            .map(|a| a.source_id.as_str())
            .collect();
        assert_eq!(east_ids, ["dateline_rfc", "missing_bbox"]);

        // Westernmost tile (4, 0, 7) covers [-180.0, 0.0, -157.5, 21.943]:
        // matches "dateline_unwrapped" ([175..185] wraps to [-180..-175]) and "missing_bbox".
        let west_candidates = fixed
            .assets_for_tile(AgTileCoord {
                tms: TileMatrixSet::WebMercatorQuad,
                z: 4,
                x: 0,
                y: 7,
            })
            .await
            .unwrap();
        let west_ids: Vec<&str> = west_candidates
            .iter()
            .map(|a| a.source_id.as_str())
            .collect();
        assert_eq!(west_ids, ["dateline_unwrapped", "missing_bbox"]);

        let geographic = fixed
            .assets_for_tile(AgTileCoord {
                tms: TileMatrixSet::WorldCrs84Quad,
                z: 0,
                x: 0,
                y: 0,
            })
            .await
            .unwrap();
        assert_eq!(
            geographic
                .iter()
                .map(|a| a.source_id.as_str())
                .collect::<Vec<_>>(),
            [
                "touching_corner",
                "dateline_rfc",
                "dateline_unwrapped",
                "missing_bbox"
            ]
        );
        assert!(
            fixed
                .assets_for_tile(AgTileCoord {
                    tms: TileMatrixSet::WorldCrs84Quad,
                    z: 0,
                    x: 2,
                    y: 0
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn open_validates_explicit_bounds_before_projection() {
        let config = |bounds, srid| MosaicSourceConfig {
            id: "bounds-contract".into(),
            members: vec![MosaicMember {
                url: "https://example.invalid/t.tif".into(),
                source_id: "t".into(),
                bbox_wgs84: [0.0; 4],
            }],
            pixel_selection: MosaicPixelSelection::First,
            bounds: Some(bounds),
            srid,
            store: None,
        };
        let cache = || Arc::new(BlockCache::new(async_geotiff::io::CacheConfig::default()));
        for srid in [None, Some(4326), Some(-1), Some(0), Some(3857)] {
            for bounds in [
                [f64::NAN, 0.0, 1.0, 1.0],
                [0.0, 0.0, f64::INFINITY, 1.0],
                [0.0, f64::NEG_INFINITY, 1.0, 1.0],
                [0.0, 2.0, 1.0, 1.0],
            ] {
                assert!(
                    matches!(
                        MosaicSource::open(config(bounds, srid), cache()).await,
                        Err(MaptileError::Config(_))
                    ),
                    "{srid:?} {bounds:?}"
                );
            }
        }
        for srid in [None, Some(4326), Some(-1), Some(0)] {
            let source = MosaicSource::open(config([170.0, -10.0, -170.0, 10.0], srid), cache())
                .await
                .unwrap();
            let bounds = source.tilejson().bounds.unwrap();
            assert_eq!(
                [bounds.left, bounds.bottom, bounds.right, bounds.top],
                [-180.0, -10.0, 180.0, 10.0]
            );
        }
        let projected =
            MosaicSource::open(config([0.0, 0.0, 1000.0, 1000.0], Some(3857)), cache()).await;
        assert!(projected.unwrap().tilejson().bounds.unwrap().right < 1.0);
    }

    #[test]
    fn near_full_longitude_span_keeps_intersecting_tiles() {
        let east = 169.999_999_999_999_94_f64;
        assert!(east - (-190.0) < 360.0);
        let bbox = normalize_member_bbox([-190.0, -85.0, east, 85.0]);
        assert!(
            bbox_intersects_tile(bbox, Bbox::new(0.0, 0.0, 90.0, 66.0)),
            "{bbox:?}"
        );
        assert_eq!(bbox, Bbox::new(-180.0, -85.0, 180.0, 85.0));
        // The first-round example already rounds its width to 360 before wrapping.
        assert_eq!(169.999_999_999_999_98_f64 - (-190.0), 360.0);
    }

    #[test]
    fn invalid_extents_are_conservative_and_wrapped_union_is_ordered() {
        for raw in [
            [f64::NAN, 0.0, 1.0, 1.0],
            [0.0; 4],
            [0.0, -100.0, 1.0, 1.0],
            [0.0, 2.0, 1.0, 1.0],
        ] {
            assert_eq!(normalize_member_bbox(raw), WORLD_BBOX);
        }
        assert_eq!(
            normalize_member_bbox([-190.0, -10.0, -170.0, 10.0]),
            Bbox::new(170.0, -10.0, -170.0, 10.0)
        );
        assert_eq!(
            normalize_member_bbox([-180.0, -10.0, 180.0, 10.0]),
            Bbox::new(-180.0, -10.0, 180.0, 10.0)
        );
        assert_eq!(
            normalize_member_bbox([1e-16, 0.0, 2e-16, 1.0]),
            Bbox::new(1e-16, 0.0, 2e-16, 1.0)
        );
        assert_eq!(
            bounds_for_tilejson([170.0, -10.0, -170.0, 10.0], None).unwrap(),
            [-180.0, -10.0, 180.0, 10.0]
        );
    }

    #[test]
    fn dateline_ulps_preserve_the_represented_longitude_interval() {
        let p = 180.0_f64;
        for (raw, expected) in [
            (
                [p.next_down(), p.next_up()],
                [p.next_down(), (-p).next_up()],
            ),
            (
                [(-p).next_down(), (-p).next_up()],
                [p.next_down(), (-p).next_up()],
            ),
            ([(-p).next_down(), -p], [p.next_down(), p]),
            ([p, p.next_up()], [-p, (-p).next_up()]),
            (
                [p.next_down().next_down(), p.next_up().next_up()],
                [p.next_down().next_down(), (-p).next_up().next_up()],
            ),
            (
                [540.0_f64.next_down(), 540.0_f64.next_up()],
                [179.999_999_999_999_9, -179.999_999_999_999_9],
            ),
        ] {
            let b = normalize_member_bbox([raw[0], -10.0, raw[1], 10.0]);
            assert_eq!([b.xmin, b.xmax], expected, "raw={raw:?}");
            assert_eq!([b.ymin, b.ymax], [-10.0, 10.0]);
            assert!(!bbox_intersects_tile(b, Bbox::new(-1.0, -1.0, 1.0, 1.0)));
        }
    }

    #[tokio::test]
    async fn dateline_equivalent_members_have_the_same_actual_tile_candidates() {
        let p = 180.0_f64;
        let raw = [
            [(-p).next_down(), -10.0, -p, 10.0],
            [p.next_down(), -10.0, p.next_up(), 10.0],
            [p, -10.0, p.next_up(), 10.0],
            [170.0, -10.0, 190.0, 10.0],
            [0.0; 4],
        ];
        let canonical = [
            [p.next_down(), -10.0, p, 10.0],
            [p.next_down(), -10.0, (-p).next_up(), 10.0],
            [-p, -10.0, (-p).next_up(), 10.0],
            [170.0, -10.0, -170.0, 10.0],
            [0.0; 4],
        ];
        let fixed = |bounds: [[f64; 4]; 5]| FixedMosaicSource {
            assets: bounds
                .into_iter()
                .enumerate()
                .map(|(i, b)| AssetRef {
                    url: format!("{i}.tif"),
                    source_id: i.to_string(),
                    bbox: normalize_member_bbox(b),
                })
                .collect(),
        };
        let actual = fixed(raw);
        let expected = fixed(canonical);
        assert_eq!(actual.statistics_assets().await.unwrap().len(), 5);
        for (tms, z, y, xs) in [
            (TileMatrixSet::WorldCrs84Quad, 0, 0, [0, 1]),
            (TileMatrixSet::WebMercatorQuad, 2, 1, [0, 3]),
            (
                TileMatrixSet::WebMercatorQuad,
                22,
                2_097_151,
                [0, 4_194_303],
            ),
        ] {
            for x in xs {
                let coord = AgTileCoord { tms, z, x, y };
                let ids = |v: Vec<AssetRef>| v.into_iter().map(|a| a.source_id).collect::<Vec<_>>();
                assert_eq!(
                    ids(actual.assets_for_tile(coord).await.unwrap()),
                    ids(expected.assets_for_tile(coord).await.unwrap()),
                    "{coord:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn world_edges_keep_one_ulp_members_and_validate_coordinates() {
        let west = Bbox::new(-180.0, -10.0, (-180.0_f64).next_up(), 10.0);
        let east = Bbox::new(180.0_f64.next_down(), -10.0, 180.0, 10.0);
        let fixed = FixedMosaicSource {
            assets: [("west", west), ("east", east)]
                .into_iter()
                .map(|(id, bbox)| AssetRef {
                    url: format!("{id}.tif"),
                    source_id: id.into(),
                    bbox,
                })
                .collect(),
        };
        for (tms, root_columns, max_zoom) in [
            (TileMatrixSet::WebMercatorQuad, 1_u32, 24),
            (TileMatrixSet::WorldCrs84Quad, 2, 23),
        ] {
            for z in [0, 2, 22, max_zoom] {
                let columns = root_columns << z;
                let y = (1_u32 << z) / 2;
                for x in [0, columns - 1] {
                    let coord = AgTileCoord { tms, z, x, y };
                    let ids: Vec<_> = fixed
                        .assets_for_tile(coord)
                        .await
                        .unwrap()
                        .into_iter()
                        .map(|a| a.source_id)
                        .collect();
                    let expected = if columns == 1 {
                        vec!["west", "east"]
                    } else if x == 0 {
                        vec!["west"]
                    } else {
                        vec!["east"]
                    };
                    assert_eq!(ids, expected, "{coord:?}");
                }
            }
            for (z, x, y) in [
                (2, root_columns << 2, 0),
                (2, 0, 4),
                (max_zoom + 1, 0, 0),
                (u8::MAX, 0, 0),
            ] {
                let coord = AgTileCoord { tms, z, x, y };
                assert!(
                    matches!(
                        fixed.assets_for_tile(coord).await,
                        Err(async_geotiff::error::AgError::TileOutOfBounds { .. })
                    ),
                    "{coord:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn world_edges_preserve_interior_columns_and_strict_tangency() {
        for (tms, columns) in [
            (TileMatrixSet::WebMercatorQuad, 4),
            (TileMatrixSet::WorldCrs84Quad, 8),
        ] {
            let tm = async_geotiff::tms::Tms::cached(tms).unwrap();
            for x in [0, 1, columns - 2, columns - 1] {
                let coord = AgTileCoord { tms, z: 2, x, y: 1 };
                let b = tm.tile_bbox(coord.z, x, coord.y).unwrap();
                let mut corners = [(b.xmin, b.ymin), (b.xmax, b.ymax)];
                async_geotiff::warp::projection::convert_batch(
                    &tm.crs(),
                    &async_geotiff::crs::Crs::epsg(4326),
                    &mut corners,
                )
                .unwrap();
                let (west, south) = corners[0];
                let (east, north) = corners[1];
                let mid_x = (west + east) / 2.0;
                let mid_y = (south + north) / 2.0;
                let left_touch = if x == 0 {
                    [179.0, 180.0]
                } else {
                    [west - 1.0, west]
                };
                let right_touch = if x == columns - 1 {
                    [-180.0, -179.0]
                } else {
                    [east, east + 1.0]
                };
                let members = [
                    (
                        "inside",
                        [mid_x - 0.5, mid_y - 0.5, mid_x + 0.5, mid_y + 0.5],
                    ),
                    (
                        "touch-west",
                        [left_touch[0], mid_y - 0.5, left_touch[1], mid_y + 0.5],
                    ),
                    (
                        "touch-east",
                        [right_touch[0], mid_y - 0.5, right_touch[1], mid_y + 0.5],
                    ),
                    (
                        "touch-south",
                        [mid_x - 0.5, south - 1.0, mid_x + 0.5, south],
                    ),
                    (
                        "touch-north",
                        [mid_x - 0.5, north, mid_x + 0.5, north + 1.0],
                    ),
                    ("world-west", [-180.0, -10.0, (-180.0_f64).next_up(), 10.0]),
                    ("world-east", [180.0_f64.next_down(), -10.0, 180.0, 10.0]),
                ];
                let fixed = FixedMosaicSource {
                    assets: members
                        .into_iter()
                        .map(|(id, b)| AssetRef {
                            url: format!("{id}.tif"),
                            source_id: id.into(),
                            bbox: normalize_member_bbox(b),
                        })
                        .collect(),
                };
                let ids: Vec<_> = fixed
                    .assets_for_tile(coord)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|a| a.source_id)
                    .collect();
                let mut expected = vec!["inside"];
                if x == 0 {
                    expected.push("world-west");
                }
                if x == columns - 1 {
                    expected.push("world-east");
                }
                assert_eq!(ids, expected, "{coord:?}");
            }
        }
    }

    #[tokio::test]
    async fn world_edges_preserve_order_duplicates_and_unique_asset_cap() {
        use async_geotiff::mosaic::DiscoveryOptions;

        let west = Bbox::new(-180.0, -10.0, (-180.0_f64).next_up(), 10.0);
        let east = Bbox::new(180.0_f64.next_down(), -10.0, 180.0, 10.0);
        for (tms, columns) in [
            (TileMatrixSet::WebMercatorQuad, 4),
            (TileMatrixSet::WorldCrs84Quad, 8),
        ] {
            for (x, bbox, far_bbox) in [(0, west, east), (columns - 1, east, west)] {
                let coord = AgTileCoord { tms, z: 2, x, y: 1 };
                let mut assets = vec![AssetRef {
                    url: "0.tif".into(),
                    source_id: "far".into(),
                    bbox: far_bbox,
                }];
                for i in 0..64 {
                    assets.push(AssetRef {
                        url: format!("{i}.tif"),
                        source_id: i.to_string(),
                        bbox,
                    });
                }
                assets.insert(
                    2,
                    AssetRef {
                        url: "0.tif".into(),
                        source_id: "duplicate".into(),
                        bbox,
                    },
                );
                let fixed = FixedMosaicSource {
                    assets: assets.into(),
                };
                let mut expected = (0..64).map(|i| i.to_string()).collect::<Vec<_>>();
                expected.insert(1, "duplicate".into());
                let raw = fixed.assets_for_tile(coord).await.unwrap();
                assert_eq!(
                    raw.iter().map(|a| a.source_id.clone()).collect::<Vec<_>>(),
                    expected,
                    "{coord:?}"
                );
                let all = fixed.statistics_assets().await.unwrap();
                assert_eq!(all.len(), 66);
                assert_eq!(all[0].source_id, "far");
                assert_eq!(all[2].source_id, "duplicate");
                let options = DiscoveryOptions::default();
                let selected = fixed
                    .assets_for_tile_with_options(coord, &options)
                    .await
                    .unwrap();
                assert_eq!(
                    selected
                        .assets()
                        .iter()
                        .map(|a| a.source_id.clone())
                        .collect::<Vec<_>>(),
                    (0..64).map(|i| i.to_string()).collect::<Vec<_>>()
                );
                let small_budget = DiscoveryOptions {
                    max_asset_bytes: 16,
                    ..options.clone()
                };
                assert!(matches!(
                    fixed
                        .assets_for_tile_with_options(coord, &small_budget)
                        .await,
                    Err(AgError::ResourceLimit(_))
                ));
                let mut assets = fixed.assets.to_vec();
                assets.push(AssetRef {
                    url: "64.tif".into(),
                    source_id: "64".into(),
                    bbox,
                });
                let fixed = FixedMosaicSource {
                    assets: assets.into(),
                };
                let result = fixed.assets_for_tile_with_options(coord, &options).await;
                assert!(
                    matches!(result, Err(async_geotiff::error::AgError::Mosaic(message)) if message.contains("65 assets, exceeding limit 64")),
                    "{coord:?}"
                );
            }
        }
    }
}
