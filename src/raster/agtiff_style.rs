//! Pass the complete Raster Style document to async-geotiff's native renderer.

use async_geotiff::style::TileStyle;

use crate::error::{MaptileError, MaptileResult};
use crate::raster::style::EffectiveStyle;

pub fn to_tile_style(style: &EffectiveStyle) -> MaptileResult<TileStyle> {
    TileStyle::from_raster_style(style.raster_style())
        .map_err(|error| MaptileError::InvalidStyle(error.to_string()))
}

pub(crate) fn to_mosaic_tile_style(
    style: &EffectiveStyle,
    selection: crate::sources::MosaicPixelSelection,
) -> MaptileResult<TileStyle> {
    let value = style.raster_style().as_value();
    if value
        .pointer("/raster:mosaic/pixel_selection")
        .and_then(serde_json::Value::as_str)
        == Some(selection.as_str())
    {
        return to_tile_style(style);
    }
    let mut value = value.clone();
    if value.get("raster:mosaic").is_none() {
        let mut extensions = value["extensions"].as_array().cloned().unwrap_or_default();
        let mosaic_extension = serde_json::json!(raster_style::MOSAIC_SCHEMA_URL);
        if !extensions.contains(&mosaic_extension) {
            extensions.push(mosaic_extension);
        }
        value["extensions"] = extensions.into();
        value["raster:mosaic"] = serde_json::json!({"stage": "after_channels"});
    }
    value["raster:mosaic"]["pixel_selection"] = selection.as_str().into();
    use crate::sources::MosaicPixelSelection::{Highest, Lowest};
    if !matches!(selection, Highest | Lowest)
        && let Some(mosaic) = value["raster:mosaic"].as_object_mut()
    {
        mosaic.remove("rank_channel");
    }
    let document = raster_style::Style::from_value(value)
        .map_err(|error| MaptileError::InvalidStyle(error.to_string()))?;
    TileStyle::from_raster_style(&document)
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
