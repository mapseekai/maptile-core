//! Raster data sources and coverage types.

#[cfg(feature = "agtiff")]
pub(super) mod cog;
pub mod coverage;
#[cfg(feature = "raster")]
pub mod dataset_cache;
#[cfg(feature = "agtiff")]
pub mod mosaic;
#[cfg(feature = "agtiff")]
mod request;
#[cfg(feature = "agtiff")]
mod snapshot;
#[cfg(feature = "stac")]
pub mod stac;
