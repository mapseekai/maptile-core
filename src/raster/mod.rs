//! Shared Raster Style documents and raster image codecs.

/// Bump when the raster renderer's output for the same input can change.
/// Embedded into `EffectiveStyle::canonical_style_hash` so cold-cache keys
/// for old-renderer outputs do not survive a renderer upgrade.
///
/// v6: async-geotiff 0.13 corrects reprojection, resampling and mask semantics.
pub const RENDERER_VERSION: u32 = 6;

pub mod style;
pub mod types;

#[cfg(feature = "agtiff")]
pub mod agtiff_style;

#[cfg(feature = "raster")]
pub mod convert;
#[cfg(feature = "raster")]
pub mod decode;
#[cfg(feature = "raster")]
pub mod encode;
#[cfg(feature = "raster")]
pub mod image;

pub use types::ResamplingMethod;

#[cfg(feature = "raster")]
pub use image::RasterImage;
