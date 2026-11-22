//! Tile source contracts and concrete archive, database, vector and raster backends.

pub mod archive_inspection;
mod driver;
#[cfg(feature = "duckdb")]
mod duckdb;
#[cfg(feature = "mbtiles")]
pub mod mbtiles;
pub mod mvt;
#[cfg(feature = "parquet-archive")]
pub mod parquet_archive;
#[cfg(feature = "pmtiles")]
pub mod pmtiles;
#[cfg(feature = "postgres")]
pub mod postgres;
mod raster;
mod request;

pub use crate::core::{
    response::TileResponse,
    tile_math,
    tile_types::{Encoding as TileEncoding, Format as TileFormat, TileCoord, TileInfo},
};
pub use driver::{SourceKind, TileSource};
pub(crate) use request::validate_tile_request;
pub use request::{
    MAX_TILE_BATCH_BYTES, MAX_TILE_BATCH_COORDS, MosaicPixelSelection, RasterPointQuery,
    RasterPointResponse, TileQuery, effective_pixel_selection,
};

#[cfg(feature = "agtiff")]
pub use mosaic_source::{MosaicMember, MosaicSource, MosaicSourceConfig, pixel_selection_from_str};
#[cfg(feature = "agtiff")]
pub use raster::cog::{AsyncCogSource, AsyncCogSourceConfig, CogObjectSource};
pub use raster::coverage as raster_coverage;
#[cfg(feature = "raster")]
pub use raster::dataset_cache;
#[cfg(feature = "agtiff")]
pub use raster::mosaic as mosaic_source;
#[cfg(feature = "stac")]
pub use raster::stac;
#[cfg(feature = "stac")]
pub use stac::{
    StacAsset, StacSource, StacSourceConfig, build_search_url, extract_assets_from_item_collection,
    is_cog_mime_type, tile_to_wgs84_bbox,
};

#[cfg(feature = "duckdb")]
pub use duckdb::{geoparquet, local_vector};
#[cfg(feature = "duckdb")]
pub use geoparquet::{
    BoundsMode, DuckDbPoolStatus, GeoParquetCredentialProvider, GeoParquetQueryOptions,
    GeoParquetS3Credentials, GeoParquetS3Options, GeoParquetSource, GeoParquetSourceConfig,
};
#[cfg(feature = "duckdb")]
pub use local_vector::{
    FlatGeobufSource, FlatGeobufSourceConfig, ImportedGeoParquet, VectorImportOptions,
    import_to_geoparquet,
};
#[cfg(feature = "parquet-archive")]
pub use parquet_archive::{
    ParquetArchiveColumns, ParquetArchiveSource, ParquetArchiveSourceConfig,
};

/// Decoded-block cache shared by COG readers.
/// Applications can inject one cache into COG and mosaic configurations to
/// allocate a common decoded-block memory budget.
#[cfg(feature = "agtiff")]
pub use async_geotiff::io::BlockCache;

/// Build a fresh process-shared [`BlockCache`] with the library default
/// capacity. Hold the returned `Arc` for the lifetime of the process and
/// clone it into every source config that accepts one.
#[cfg(feature = "agtiff")]
#[must_use]
pub fn new_shared_block_cache() -> std::sync::Arc<BlockCache> {
    std::sync::Arc::new(BlockCache::new(async_geotiff::io::CacheConfig::default()))
}
