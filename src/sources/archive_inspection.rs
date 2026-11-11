//! Strict archive metadata validation and public TileJSON normalization.

use crate::error::{MaptileError, MaptileResult};
use crate::sources::{TileFormat, TileInfo};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveSourceType {
    Mbtiles,
    Pmtiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileFamily {
    Vector,
    Raster,
}

#[derive(Debug, Clone)]
pub struct ArchiveInspection {
    pub family: TileFamily,
    pub format: TileFormat,
    pub encoding: crate::sources::TileEncoding,
    pub tilejson: tilejson::TileJSON,
}

pub fn inspect_archive(
    source_type: ArchiveSourceType,
    tilejson: &tilejson::TileJSON,
    info: TileInfo,
) -> MaptileResult<ArchiveInspection> {
    let family = match info.format {
        TileFormat::Mvt | TileFormat::Mlt => TileFamily::Vector,
        TileFormat::Png | TileFormat::Jpeg | TileFormat::Webp => TileFamily::Raster,
        unsupported => {
            return Err(MaptileError::InvalidMetadata(format!(
                "unsupported archive format: {unsupported:?}"
            )));
        }
    };
    validate_required_metadata(source_type, tilejson, family)?;
    Ok(ArchiveInspection {
        family,
        format: info.format,
        encoding: info.encoding,
        tilejson: normalize_public_tilejson(tilejson, family, info.format)?,
    })
}

// Canonicalize common archive extensions once; no per-request probing.
pub(crate) fn declared_tile_size(tj: &tilejson::TileJSON) -> MaptileResult<Option<u32>> {
    let mut size = None;
    for key in ["tile_size", "tileSize", "tilesize"] {
        if let Some(value) = tj.other.get(key) {
            let parsed = value
                .as_u64()
                .or_else(|| value.as_str()?.parse().ok())
                .filter(|value| (1..=2048).contains(value))
                .ok_or_else(|| MaptileError::InvalidMetadata("invalid archive tile_size".into()))?
                as u32;
            if size.is_some_and(|size| size != parsed) {
                return Err(MaptileError::InvalidMetadata(
                    "conflicting archive tile sizes".into(),
                ));
            }
            size = Some(parsed);
        }
    }
    Ok(size)
}

pub(crate) fn set_raster_tile_size(
    tj: &mut tilejson::TileJSON,
    bytes: &bytes::Bytes,
    info: TileInfo,
) -> MaptileResult<()> {
    if matches!(info.format, TileFormat::Mvt | TileFormat::Mlt) {
        return Ok(());
    }
    if let Some(size) = declared_tile_size(tj)? {
        tj.other.insert("tile_size".into(), size.into());
        return Ok(());
    }
    let bytes = crate::transport::decode(bytes, info.encoding)?;
    let expected = match info.format {
        TileFormat::Png => image::ImageFormat::Png,
        TileFormat::Jpeg => image::ImageFormat::Jpeg,
        TileFormat::Webp => image::ImageFormat::WebP,
        _ => {
            return Err(MaptileError::InvalidMetadata(
                "unsupported raster archive format".into(),
            ));
        }
    };
    let actual = image::guess_format(&bytes).map_err(MaptileError::backend)?;
    if actual != expected {
        return Err(MaptileError::InvalidMetadata(
            "archive tile format disagrees with metadata".into(),
        ));
    }
    let (width, height) = image::ImageReader::with_format(std::io::Cursor::new(bytes), actual)
        .into_dimensions()
        .map_err(MaptileError::backend)?;
    if width == 0 || width != height || width > 2048 {
        return Err(MaptileError::InvalidMetadata(
            "archive tile dimensions disagree or are unsupported".into(),
        ));
    }
    tj.other.insert("tile_size".into(), width.into());
    Ok(())
}

fn validate_required_metadata(
    source_type: ArchiveSourceType,
    tilejson: &tilejson::TileJSON,
    family: TileFamily,
) -> MaptileResult<()> {
    let invalid =
        |message: &str| MaptileError::InvalidMetadata(format!("{source_type:?} archive {message}"));

    if tilejson
        .name
        .as_deref()
        .is_none_or(|name| name.trim().is_empty())
    {
        return Err(invalid("requires a non-empty name"));
    }

    let bounds = tilejson.bounds.ok_or_else(|| invalid("requires bounds"))?;
    let coordinates = [bounds.left, bounds.bottom, bounds.right, bounds.top];
    if !coordinates.into_iter().all(f64::is_finite)
        || bounds.left < -180.0
        || bounds.right > 180.0
        || bounds.bottom < -90.0
        || bounds.top > 90.0
        || bounds.left >= bounds.right
        || bounds.bottom >= bounds.top
    {
        return Err(invalid("has invalid bounds"));
    }

    let minzoom = tilejson
        .minzoom
        .ok_or_else(|| invalid("requires minzoom"))?;
    let maxzoom = tilejson
        .maxzoom
        .ok_or_else(|| invalid("requires maxzoom"))?;
    if minzoom > maxzoom || maxzoom > 30 {
        return Err(invalid("has invalid zoom range"));
    }

    if let Some(center) = tilejson.center
        && (!center.longitude.is_finite()
            || !center.latitude.is_finite()
            || center.longitude < bounds.left
            || center.longitude > bounds.right
            || center.latitude < bounds.bottom
            || center.latitude > bounds.top
            || center.zoom < minzoom
            || center.zoom > maxzoom)
    {
        return Err(invalid("has invalid center"));
    }

    match (family, tilejson.vector_layers.as_ref()) {
        (TileFamily::Vector, Some(layers)) if !layers.is_empty() => {
            let mut layer_ids = HashSet::with_capacity(layers.len());
            for layer in layers {
                let layer_id = layer.id.trim();
                if layer_id.is_empty() || !layer_ids.insert(layer_id) {
                    return Err(invalid("has invalid or duplicate vector layer ids"));
                }
                if layer
                    .minzoom
                    .is_some_and(|zoom| zoom < minzoom || zoom > maxzoom)
                    || layer
                        .maxzoom
                        .is_some_and(|zoom| zoom < minzoom || zoom > maxzoom)
                    || matches!((layer.minzoom, layer.maxzoom), (Some(min), Some(max)) if min > max)
                {
                    return Err(invalid("has an invalid vector layer zoom range"));
                }
            }
        }
        (TileFamily::Vector, _) => {
            return Err(invalid(
                "requires non-empty vector_layers with non-empty ids and fields maps",
            ));
        }
        (TileFamily::Raster, None) => {}
        (TileFamily::Raster, Some(_)) => {
            return Err(invalid("must not declare vector_layers"));
        }
    }

    Ok(())
}

fn normalize_public_tilejson(
    tilejson: &tilejson::TileJSON,
    family: TileFamily,
    format: TileFormat,
) -> MaptileResult<tilejson::TileJSON> {
    let mut normalized = tilejson.clone();
    normalized.other.clear();
    if family == TileFamily::Raster {
        if let Some(size) = declared_tile_size(tilejson)? {
            normalized.other.insert("tile_size".into(), size.into());
        }
    } else if format == TileFormat::Mlt {
        normalized.other.insert("encoding".into(), "mlt".into());
    }
    if let Some(layers) = normalized.vector_layers.as_mut() {
        for layer in layers {
            layer.other.clear();
        }
    }
    let mut value = serde_json::to_value(normalized)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| MaptileError::InvalidMetadata("TileJSON must be an object".into()))?;
    object.insert("tiles".into(), serde_json::Value::Array(Vec::new()));
    object.insert("scheme".into(), serde_json::Value::String("xyz".into()));
    match family {
        TileFamily::Vector => {
            object.remove("format");
        }
        TileFamily::Raster => {
            let format = match format {
                TileFormat::Png => "png",
                TileFormat::Jpeg => "jpeg",
                TileFormat::Webp => "webp",
                _ => unreachable!("family was derived from the same format"),
            };
            object.insert("format".into(), serde_json::Value::String(format.into()));
        }
    }
    serde_json::from_value(value).map_err(MaptileError::from)
}

#[cfg(test)]
mod generation_regression_tests {
    use super::*;
    #[test]
    fn tile_size_metadata_is_canonical_and_validated() {
        let mut tj = tilejson::tilejson! { tiles: vec![] };
        tj.other.insert("tilesize".into(), "512".into());
        assert_eq!(declared_tile_size(&tj).unwrap(), Some(512));
        tj.other.insert("tile_size".into(), 256.into());
        assert!(declared_tile_size(&tj).is_err());
        for value in [
            serde_json::json!(0),
            serde_json::json!(4096),
            serde_json::json!(512.5),
            serde_json::json!("invalid"),
        ] {
            tj.other.clear();
            tj.other.insert("tile_size".into(), value);
            assert!(declared_tile_size(&tj).is_err());
        }
    }

    #[test]
    fn conflicting_extensions_cannot_delete_required_header_fields() {
        let mut tj = tilejson::tilejson! { tiles: vec![] };
        tj.name = Some("generated".into());
        tj.bounds = Some(tilejson::Bounds::new(100.0, 20.0, 101.0, 21.0));
        tj.minzoom = Some(3);
        tj.maxzoom = Some(3);
        for key in ["tilejson", "bounds", "minzoom", "maxzoom"] {
            tj.other.insert(key.into(), serde_json::Value::Null);
        }
        let result = inspect_archive(
            ArchiveSourceType::Pmtiles,
            &tj,
            TileInfo {
                format: TileFormat::Png,
                encoding: crate::sources::TileEncoding::Uncompressed,
            },
        )
        .unwrap();
        assert_eq!(result.tilejson.bounds, tj.bounds);
        assert_eq!(result.tilejson.minzoom, Some(3));
        assert!(result.tilejson.other.contains_key("format"));
    }
}
