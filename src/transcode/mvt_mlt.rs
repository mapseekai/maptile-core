//! MVT ↔ MLT bidirectional transcoding.
//!
//! Originally adapted from `tileserver-rs/crates/tileserver-rs/src/transcode.rs`
//! under the MIT license.
//! See `third_party/tileserver-rs/LICENSE`.

use crate::error::{MaptileError, MaptileResult};
use crate::transport::MAX_DECODED_TILE_BYTES;
use bytes::Bytes;
use geo_types::{Coord, Geometry, LineString, Polygon};
use mlt_core::fast_mvt::{MvtReaderRef, MvtTileBuilder, MvtValue, MvtValueRef};
use mlt_core::{LendingIterator, ParsedLayer, PropValueRef};
use prost::encoding::{DecodeContext, WireType, decode_key, decode_varint, skip_field};
use std::collections::{HashMap, HashSet};

const CONVERSION_WORK_BYTES: usize = 64 * 1024 * 1024;

fn check_tile_size(size: usize) -> MaptileResult<()> {
    if size > MAX_DECODED_TILE_BYTES {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_DECODED_TILE_BYTES,
        });
    }
    Ok(())
}

fn charge(remaining: &mut usize, cost: usize) -> MaptileResult<()> {
    *remaining = remaining.checked_sub(cost).ok_or_else(|| {
        MaptileError::Resource("vector tile exceeds the transcoding work budget".into())
    })?;
    Ok(())
}

fn transcode_error(error: impl std::fmt::Display) -> MaptileError {
    MaptileError::Transcode(error.to_string())
}

fn take_frame<'a>(input: &mut &'a [u8]) -> MaptileResult<&'a [u8]> {
    let length = decode_varint(input).map_err(transcode_error)?;
    let length = usize::try_from(length).map_err(transcode_error)?;
    let Some((frame, rest)) = input.split_at_checked(length) else {
        return Err(transcode_error("truncated vector tile frame"));
    };
    *input = rest;
    Ok(frame)
}

fn protobuf_fields(
    mut input: &[u8],
    mut field: impl FnMut(u32, WireType, &[u8]) -> MaptileResult<()>,
) -> MaptileResult<()> {
    while !input.is_empty() {
        let (tag, wire) = decode_key(&mut input).map_err(transcode_error)?;
        if wire == WireType::LengthDelimited {
            field(tag, wire, take_frame(&mut input)?)?;
        } else {
            skip_field(wire, tag, &mut input, DecodeContext::default()).map_err(transcode_error)?;
            field(tag, wire, &[])?;
        }
    }
    Ok(())
}

fn check_mvt_wire(bytes: &[u8], mut remaining: usize) -> MaptileResult<()> {
    // The borrowed protobuf reader still allocates repeated objects and packed integers.
    charge(&mut remaining, bytes.len().saturating_mul(4))?;
    use WireType::{LengthDelimited, Varint};
    protobuf_fields(bytes, |tag, wire, layer| {
        if (tag, wire) != (3, LengthDelimited) {
            return Err(unsupported("MVT tile field"));
        }
        charge(&mut remaining, 256)?;
        protobuf_fields(layer, |tag, wire, data| match (tag, wire) {
            (1, LengthDelimited) | (5 | 15, Varint) => Ok(()),
            (2, LengthDelimited) => {
                charge(&mut remaining, 256)?;
                protobuf_fields(data, |tag, wire, _| match (tag, wire) {
                    (1 | 3, Varint) | (2 | 4, Varint | LengthDelimited) => Ok(()),
                    _ => Err(unsupported("MVT feature field")),
                })
            }
            (3, LengthDelimited) => charge(&mut remaining, 32),
            (4, LengthDelimited) => {
                charge(&mut remaining, 192)?;
                let mut fields = 0;
                protobuf_fields(data, |tag, wire, _| {
                    fields += 1;
                    match (tag, wire) {
                        (1, LengthDelimited)
                        | (2, WireType::ThirtyTwoBit)
                        | (3, WireType::SixtyFourBit)
                        | (4..=7, Varint)
                            if fields == 1 =>
                        {
                            Ok(())
                        }
                        _ => Err(unsupported("MVT property value")),
                    }
                })?;
                if fields != 1 {
                    return Err(unsupported("empty MVT property value"));
                }
                Ok(())
            }
            _ => Err(unsupported("MVT layer field")),
        })
    })
}

fn unsupported(content: &str) -> MaptileError {
    MaptileError::Unsupported(format!(
        "cannot preserve {content} during MVT/MLT conversion"
    ))
}

fn check_mvt_work(bytes: &[u8], mut remaining: usize) -> MaptileResult<()> {
    let reader = MvtReaderRef::new(bytes).map_err(transcode_error)?;
    let mut layers = HashSet::new();
    for layer in reader.layers() {
        if layer.version() != 2 || !layers.insert(layer.name()) {
            return Err(unsupported("MVT layer version or duplicate layer name"));
        }
        let features = layer.feature_count();
        if features == 0 {
            return Err(unsupported("empty MVT layer"));
        }
        charge(
            &mut remaining,
            256usize.saturating_add(features.saturating_mul(128)),
        )?;
        let mut columns = HashMap::new();
        let mut feature_keys = HashSet::new();
        for feature in layer.features() {
            feature_keys.clear();
            charge(
                &mut remaining,
                feature.geometry_commands().len().saturating_mul(16),
            )?;
            for property in feature.properties() {
                let (key, value) = property.map_err(transcode_error)?;
                if !feature_keys.insert(key) {
                    return Err(unsupported("duplicate MVT feature property"));
                }
                let column_type = std::mem::discriminant(&match value {
                    MvtValueRef::Int(n) => MvtValueRef::SInt(n),
                    MvtValueRef::Null => return Err(unsupported("null MVT property value")),
                    other => other,
                });
                let string_bytes = match value {
                    MvtValueRef::String(text) => text.len(),
                    _ => 0,
                };
                charge(&mut remaining, 32usize.saturating_add(string_bytes))?;
                if let Some(previous) = columns.insert(key, column_type) {
                    if previous != column_type {
                        return Err(unsupported("mixed MVT property types in one column"));
                    }
                } else {
                    // mlt-core allocates a typed property slot even for an absent value.
                    charge(
                        &mut remaining,
                        key.len().saturating_add(
                            features.saturating_mul(size_of::<mlt_core::PropValue>()),
                        ),
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Encode an uncompressed MVT tile to MLT bytes.
pub fn mvt_to_mlt(mvt_bytes: &[u8]) -> MaptileResult<Bytes> {
    check_tile_size(mvt_bytes.len())?;
    check_mvt_wire(mvt_bytes, CONVERSION_WORK_BYTES)?;
    check_mvt_work(mvt_bytes, CONVERSION_WORK_BYTES)?;
    let layers = mlt_core::mvt::mvt_to_tile_layers(mvt_bytes).map_err(transcode_error)?;
    let mut output = Vec::new();
    let config = mlt_core::encoder::EncoderConfig::default()
        .with_spatial_morton_sort(false)
        .with_spatial_hilbert_sort(false)
        .with_id_sort(false);
    for layer in layers {
        let encoded = layer.encode(config).map_err(transcode_error)?;
        check_tile_size(output.len().saturating_add(encoded.len()))?;
        output.extend_from_slice(&encoded);
    }
    Ok(Bytes::from(output))
}

/// Decode an MLT tile to MVT (protobuf) bytes.
pub fn mlt_to_mvt(mlt_bytes: &[u8]) -> MaptileResult<Bytes> {
    check_tile_size(mlt_bytes.len())?;
    let mut parser = mlt_core::Parser::default();
    let mut decoder = mlt_core::Decoder::default();
    let mut remaining = CONVERSION_WORK_BYTES;
    let mut tile = MvtTileBuilder::new();
    let mut input = mlt_bytes;
    let mut layer_names = HashSet::new();
    while !input.is_empty() {
        let start = input;
        let frame = take_frame(&mut input)?;
        let tag = *frame
            .first()
            .ok_or_else(|| transcode_error("empty MLT frame"))?;
        charge(&mut remaining, 128)?;
        if tag != 1 {
            return Err(unsupported("MLT frame version"));
        }
        let layer = parser
            .parse_layers(&start[..start.len() - input.len()])
            .map_err(transcode_error)?
            .pop()
            .ok_or_else(|| transcode_error("missing MLT layer"))?;
        let ParsedLayer::Tag01(layer) = layer.decode_all(&mut decoder).map_err(transcode_error)?
        else {
            return Err(unsupported("MLT layer type"));
        };
        if !layer_names.insert(layer.name().to_owned()) {
            return Err(unsupported("duplicate MLT layer name"));
        }
        let count = layer.feature_count();
        let mut names = Vec::new();
        let mut names_seen = HashSet::new();
        for name in layer.iterate_prop_names() {
            let name = name.to_string();
            charge(
                &mut remaining,
                name.len().saturating_add(size_of::<String>()),
            )?;
            if !names_seen.insert(name.clone()) {
                return Err(unsupported("duplicate MLT property name"));
            }
            names.push(name);
        }
        charge(
            &mut remaining,
            count.saturating_mul(128usize.saturating_add(names.len().saturating_mul(8))),
        )?;
        let mut output = tile
            .layer_with_capacity(layer.name(), count)
            .map_err(transcode_error)?;
        output.extent(layer.extent().into());
        let mut features = layer.iter_features();
        while let Some(feature) = features.next() {
            let feature = feature.map_err(transcode_error)?;
            let vertices = check_geometry(feature.geometry())?;
            charge(&mut remaining, vertices.saturating_mul(24))?;
            let mut builder = output
                .feature(feature.geometry())
                .map_err(transcode_error)?;
            builder.id(feature.id());
            for (key, value) in names.iter().zip(feature.iter_all_properties()) {
                let Some(value) = value else { continue };
                let text_len = match value {
                    PropValueRef::Str(text) => text.len(),
                    _ => 0,
                };
                charge(
                    &mut remaining,
                    64usize.saturating_add(key.len()).saturating_add(text_len),
                )?;
                builder
                    .tag(key, mvt_value(value))
                    .map_err(transcode_error)?;
            }
            output = builder.end();
        }
        tile = output.end();
    }
    check_tile_size(tile.encoded_len())?;
    Ok(Bytes::from(tile.encode()))
}

fn check_geometry(geometry: &Geometry<i32>) -> MaptileResult<usize> {
    let mut cursor = Coord { x: 0i32, y: 0i32 };
    let mut count = 0usize;
    let mut visit = |point: Coord<i32>| {
        if point.x.checked_sub(cursor.x).is_none() || point.y.checked_sub(cursor.y).is_none() {
            return Err(transcode_error(
                "geometry has an unrepresentable MVT coordinate delta",
            ));
        }
        cursor = point;
        count += 1;
        Ok(())
    };
    match geometry {
        Geometry::Point(point) => visit(point.0)?,
        Geometry::MultiPoint(points) => points
            .0
            .iter()
            .map(|point| point.0)
            .try_for_each(&mut visit)?,
        Geometry::LineString(line) => line.0.iter().copied().try_for_each(&mut visit)?,
        Geometry::MultiLineString(lines) => lines
            .0
            .iter()
            .flat_map(|line| &line.0)
            .copied()
            .try_for_each(&mut visit)?,
        Geometry::Polygon(polygon) => check_polygon(polygon, &mut visit)?,
        Geometry::MultiPolygon(polygons) => {
            for polygon in &polygons.0 {
                check_polygon(polygon, &mut visit)?;
            }
        }
        _ => return Err(transcode_error("geometry has no MVT representation")),
    }
    Ok(count)
}

fn check_polygon(
    polygon: &Polygon<i32>,
    visit: &mut impl FnMut(Coord<i32>) -> MaptileResult<()>,
) -> MaptileResult<()> {
    for (index, LineString(points)) in std::iter::once(polygon.exterior())
        .chain(polygon.interiors())
        .enumerate()
    {
        let points = if points.len() >= 2 && points.first() == points.last() {
            &points[..points.len() - 1]
        } else {
            points.as_slice()
        };
        let mut area = 0i64;
        for (a, b) in points.iter().zip(points.iter().cycle().skip(1)) {
            let cross = i64::from(a.x) * i64::from(b.y) - i64::from(a.y) * i64::from(b.x);
            area = area
                .checked_add(cross)
                .ok_or_else(|| transcode_error("polygon area exceeds the MVT encoder range"))?;
        }
        // Match fast-mvt's winding normalization when checking cursor deltas.
        if area != 0 && (area > 0) != (index == 0) {
            points.iter().rev().copied().try_for_each(&mut *visit)?;
        } else {
            points.iter().copied().try_for_each(&mut *visit)?;
        }
    }
    Ok(())
}

fn mvt_value(value: PropValueRef<'_>) -> MvtValue {
    match value {
        PropValueRef::Bool(value) => MvtValue::Bool(value),
        PropValueRef::I8(value) => MvtValue::SInt(value.into()),
        PropValueRef::I32(value) => MvtValue::SInt(value.into()),
        PropValueRef::I64(value) => MvtValue::SInt(value),
        PropValueRef::U8(value) => MvtValue::UInt(value.into()),
        PropValueRef::U32(value) => MvtValue::UInt(value.into()),
        PropValueRef::U64(value) => MvtValue::UInt(value),
        PropValueRef::F32(value) => MvtValue::Float(value),
        PropValueRef::F64(value) => MvtValue::Double(value),
        PropValueRef::Str(value) => MvtValue::String(value.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{LineString, MultiLineString};

    #[test]
    fn validates_consecutive_mvt_deltas_instead_of_the_total_extent() {
        let bridged = Geometry::LineString(LineString::from(vec![(i32::MIN, 0), (-1, 0), (1, 0)]));
        assert_eq!(check_geometry(&bridged).unwrap(), 3);
        let unrepresentable = Geometry::LineString(LineString::from(vec![(i32::MIN, 0), (1, 0)]));
        assert!(matches!(
            check_geometry(&unrepresentable),
            Err(MaptileError::Transcode(_))
        ));
    }

    #[test]
    fn unknown_frames_and_properties_with_mixed_types_are_rejected() {
        let error = mlt_to_mvt(&[1, 2]).unwrap_err();
        assert!(matches!(error, MaptileError::Unsupported(_)));

        let geometry = Geometry::Point(geo_types::Point::new(1, 1));
        let layer = MvtTileBuilder::new().layer("roads").unwrap();
        let mut first = layer.feature(&geometry).unwrap();
        first.tag("value", MvtValue::SInt(-7)).unwrap();
        let mut second = first.end().feature(&geometry).unwrap();
        second.tag("value", MvtValue::UInt(u64::MAX)).unwrap();
        let err = mvt_to_mlt(&second.end().encode()).unwrap_err();
        assert!(matches!(err, MaptileError::Unsupported(_)));
    }

    #[test]
    fn rejects_empty_layers_and_preserves_empty_tiles() {
        assert!(mlt_to_mvt(&mvt_to_mlt(&[]).unwrap()).unwrap().is_empty());
        let mvt = MvtTileBuilder::new().layer("empty").unwrap().encode();
        assert!(matches!(
            mvt_to_mlt(&mvt),
            Err(MaptileError::Unsupported(_))
        ));
    }

    #[test]
    fn multipart_geometry_and_typed_properties_round_trip() {
        let geometry = Geometry::MultiLineString(MultiLineString(vec![
            LineString::from(vec![(10, 20), (12, 20)]),
            LineString::from(vec![(30, 40), (32, 42)]),
        ]));
        let mut layer = MvtTileBuilder::new().layer("roads").unwrap();
        layer.extent(std::num::NonZeroU32::new(1024).unwrap());
        let mut feature = layer.feature(&geometry).unwrap();
        feature.id(Some(7));
        feature
            .tag("_road", MvtValue::String("north".into()))
            .unwrap();
        feature.tag("speed", MvtValue::UInt(45)).unwrap();
        let original = feature.end().encode();
        let mlt = mvt_to_mlt(&original).unwrap();
        let encoded = mlt_to_mvt(&mlt).unwrap();
        let reader = MvtReaderRef::new(&encoded).unwrap();
        let layer = reader.layers().next().unwrap();
        assert_eq!(layer.extent(), 1024);
        let feature = layer.features().next().unwrap();
        assert_eq!(feature.id(), Some(7));
        assert_eq!(feature.geometry().unwrap(), geometry);
        assert_eq!(
            feature.properties().collect::<Result<Vec<_>, _>>().unwrap(),
            vec![
                ("_road", MvtValueRef::String("north")),
                ("speed", MvtValueRef::UInt(45)),
            ]
        );
        assert!(matches!(
            check_mvt_work(&original, 256),
            Err(MaptileError::Resource(_))
        ));
        assert!(matches!(
            check_mvt_wire(&original, 256),
            Err(MaptileError::Resource(_))
        ));
    }
}
