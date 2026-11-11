//! Curated re-exports for the most common types and traits.
//!
//! ```no_run
//! use maptile_core::prelude::*;
//! # async fn _ex() -> MaptileResult<()> {
//! let mgr = SourceManager::new();
//! let _ids = mgr.list_ids();
//! # Ok(()) }
//! ```
//!
//! Items only available behind a cargo feature appear here only when that
//! feature is on. Reach for the full module path (e.g.
//! `crate::sources::pmtiles::LocalPmtilesSource`) if you need something
//! that the prelude doesn't surface.

pub use crate::cache::{CacheZoomRange, TileCache, TileCacheKey};
pub use crate::discovery::{
    DiscoveredSource, DiscoveredSourceKind, DiscoveredStyle, DiscoveryReport,
    scan as scan_discovery, scan_async as scan_discovery_async,
};
pub use crate::error::{MaptileError, MaptileResult};
pub use crate::filter::{Cql2Expr, FilterError, UrlQuery};
pub use crate::headers::{
    CachePolicy, NO_STORE, STATIC_ASSET_CACHE_CONTROL, TILE_CACHE_CONTROL, content_type_for,
};
pub use crate::raster::ResamplingMethod;
pub use crate::sources::manager::{Catalog, SourceManager};
/// MBTiles archive lifecycle ops (copy, patch, validate, metadata,
/// zoom-window updates) — re-exports of Martin's `mbtiles` crate.
/// Use as `maptile_core::prelude::mbtiles_tools::Mbtiles`.
pub use crate::sources::mbtiles::tools as mbtiles_tools;
pub use crate::sources::pmtiles::{PmTilesStreamWriter, PmTilesWriter};
pub use crate::sources::tile_math::{MAX_ZOOM as MAX_TILE_ZOOM, tiles_per_axis};
pub use crate::sources::{
    CompositeChild, CompositeSource, TileCoord, TileEncoding, TileEtagResponse, TileFormat,
    TileInfo, TileQuery, TileResponse, TileSource,
};
pub use crate::styles::{
    LocalStyleStore, Style, StyleInfo, StyleStore, rewrite_style_for_native, rewrite_urls,
    snapshot as snapshot_styles,
};
pub use crate::transport::{decode as decode_payload, encode_as as encode_payload, sniff_encoding};

pub use bytes::Bytes;
pub use tilejson::TileJSON;

pub use crate::sources::parse_composite_id_list;
pub use crate::transcode::transcode_with_gzip;
pub use crate::watch::{ReloadDelta, SourceWatcher, WatchEvent};
pub use crate::wkb_mvt::{WkbFeature, encode_features_to_mvt};
/// Web-mercator helpers re-exported from `martin-tile-utils` so callers
/// don't need a direct dependency for tile↔geographic coordinate work.
pub use martin_tile_utils::{
    bbox_to_xyz, get_zoom_precision, tile_index, webmercator_to_wgs84, wgs84_to_webmercator,
    xyz_to_bbox,
};

#[cfg(feature = "raster")]
pub use crate::raster::RasterImage;

#[cfg(feature = "render")]
pub use crate::render::{
    GeoPoint, MapLibreRenderer, MarkerOverlay, OverlayItem, PathOverlay, PoolConfig, RenderCamera,
    RenderFormat, RenderRequest, RenderRequestError, RenderedImage, RendererPool, StaticTarget,
    WmtsLayer, WmtsService, build_capabilities as build_wmts_capabilities,
    calculate_bounds as calculate_overlay_bounds, decode_polyline, draw_overlays, expand_overlays,
    parse_geojson, parse_marker, parse_path,
};

#[cfg(not(feature = "render"))]
pub use crate::render::{
    RenderCamera, RenderFormat, RenderRequest, RenderRequestError, RenderedImage, StaticTarget,
    WmtsLayer, WmtsService, build_capabilities as build_wmts_capabilities,
};

#[cfg(feature = "duckdb")]
pub use crate::sources::{DuckDbSource, DuckDbSourceConfig};

#[cfg(feature = "geoparquet")]
pub use crate::sources::{GeoParquetColumns, GeoParquetSource, GeoParquetSourceConfig};

#[cfg(feature = "local-vector")]
pub use crate::sources::{
    FlatGeobufSource, FlatGeobufSourceConfig, ImportedGeoParquet, VectorImportFormat,
    VectorImportOptions, import_to_geoparquet,
};

#[cfg(feature = "stac")]
pub use crate::sources::{
    StacAsset, StacSource, StacSourceConfig, build_search_url, extract_assets_from_item_collection,
    is_cog_mime_type, tile_to_wgs84_bbox,
};
