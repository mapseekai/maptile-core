//! Revision checks for files and HTTP byte ranges.

#[cfg(any(
    feature = "mbtiles",
    feature = "pmtiles",
    feature = "duckdb",
    feature = "agtiff",
    feature = "parquet-archive"
))]
pub(crate) mod file_stamp;
#[cfg(any(feature = "pmtiles", feature = "agtiff", feature = "duckdb"))]
pub(crate) mod http_range;
