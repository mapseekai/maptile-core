// async-geotiff's bounded statistics futures exceed the default query depth.
#![cfg_attr(feature = "agtiff", recursion_limit = "256")]

//! Composable tile sources, dynamic tiling, caching, and map rendering.
//!
//! [`sources`] provides a common [`sources::TileSource`] interface for tile
//! archives, PostgreSQL, and feature-enabled vector and raster backends.
//! Applications construct sources from their configuration, register them with
//! [`sources::manager::SourceManager`], and request tiles by source ID.
//!
//! [`cache`], [`filter`], [`styles`], and [`transport`] provide tile caching,
//! CQL2 predicates, style loading, and payload encoding. Optional Cargo features
//! add GeoParquet, local vector imports, COG/STAC tiling, and native map rendering.
//! Use [`prelude`] for common types and [`render`] for rendering requests and
//! WMTS metadata.

pub mod cache;
pub mod discovery;
pub mod error;
pub mod filter;
pub mod headers;
#[cfg(feature = "metrics")]
pub mod metrics;
pub mod prelude;
pub mod raster;
pub mod render;
pub mod sources;
pub mod styles;
pub mod transcode;
pub mod transport;
pub mod watch;
pub mod wkb_mvt;
