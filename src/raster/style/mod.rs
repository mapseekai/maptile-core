//! Raster Style 3.0 documents, shared by all raster sources.

#[cfg(feature = "raster")]
pub(crate) mod pipeline;
pub mod resolve;

pub use raster_style::Style as RasterStyle;
pub use resolve::EffectiveStyle;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RasterOutputFormat {
    Png,
    Webp,
    Jpeg,
}
