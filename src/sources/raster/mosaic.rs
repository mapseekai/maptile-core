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

mod footprint;

use async_geotiff::io::BlockCache;
use async_geotiff::mosaic::{AssetRef, Backend, MosaicTiler};
use async_geotiff::tms::{TileCoord as AgTileCoord, TileMatrixSet};
use bytes::Bytes;
use object_store::ObjectStore;
use std::sync::Arc;
use tilejson::{TileJSON, tilejson};

use self::footprint::{
    FixedMosaicSource, bounds_for_tilejson, normalize_member_bbox, union_bounds,
};
use super::request;
use crate::error::{MaptileError, MaptileResult};
use crate::raster::agtiff_style::to_mosaic_tile_style;
use crate::raster::style::RasterOutputFormat;
use crate::sources::{
    MosaicPixelSelection, SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery,
    TileResponse, TileSource, effective_pixel_selection, validate_source_request,
};

fn out_tile_format(f: RasterOutputFormat) -> TileFormat {
    match f {
        RasterOutputFormat::Png => TileFormat::Png,
        RasterOutputFormat::Webp => TileFormat::Webp,
        RasterOutputFormat::Jpeg => TileFormat::Jpeg,
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
            None => union_bounds(&assets),
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

        let fixed = FixedMosaicSource::new(assets);
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

    fn source_kind(&self) -> SourceKind {
        SourceKind::Mosaic
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

    #[tracing::instrument(target = "maptile_core::sources::mosaic_source", name = "mosaic.get_tile", skip_all, fields(z = %xyz.z, x = %xyz.x, y = %xyz.y))]
    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        validate_source_request(self, &[xyz], query)?;
        let style = query.and_then(|q| q.style).ok_or_else(|| {
            MaptileError::InvalidRequest("canonical raster style is required".into())
        })?;

        let tile_style = to_mosaic_tile_style(
            style,
            effective_pixel_selection(query, self.pixel_selection),
        )?;
        let out_format = out_tile_format(style.format);

        let tile: Bytes = request::render(|limiter| async {
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
mod tests {
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
}
