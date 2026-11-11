// async-geotiff's bounded statistics futures exceed the default query depth.
#![cfg_attr(feature = "agtiff", recursion_limit = "256")]

//! `maptile-core` — pure tile-serving library: source abstractions, registry,
//! style/render/raster/resource modules, and a CQL2 filter pipeline.
//!
//! No configuration parsing lives here; callers construct concrete sources
//! and inject them via [`sources::manager::SourceManager::register`].

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
