//! Pass the complete Raster Style document to async-geotiff's native renderer.

use async_geotiff::style::TileStyle;

use crate::error::{MaptileError, MaptileResult};
use crate::raster::style::EffectiveStyle;

pub fn to_tile_style(style: &EffectiveStyle) -> MaptileResult<TileStyle> {
    TileStyle::from_raster_style(style.raster_style())
        .map_err(|error| MaptileError::InvalidStyle(error.to_string()))
}

pub(crate) fn render_error(error: async_geotiff::error::AgError) -> MaptileError {
    use async_geotiff::error::AgError;
    match error {
        AgError::Query { .. } | AgError::UnknownColormap(_) | AgError::ColorFormula(_) => {
            MaptileError::InvalidStyle(error.to_string())
        }
        _ => MaptileError::backend(error),
    }
}
