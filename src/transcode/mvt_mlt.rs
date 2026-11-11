//! MVT ↔ MLT bidirectional transcoding.
//!
//! Adapted from `tileserver-rs/crates/tileserver-rs/src/transcode.rs`
//! under the MIT license; see `third_party/tileserver-rs/LICENSE`.
//! The MVT protobuf schema is the one in
//! [`crate::sources::mvt::proto`]; we do not redefine it here.

use crate::error::{MaptileError, MaptileResult};
use crate::sources::mvt::proto as mvt_proto;
use bytes::Bytes;
use prost::Message;

// ---------------------------------------------------------------------------
// MVT → MLT
// ---------------------------------------------------------------------------

/// Encode an uncompressed MVT tile to MLT bytes.
pub fn mvt_to_mlt(mvt_bytes: &[u8]) -> MaptileResult<Bytes> {
    let layers = mlt_core::mvt::mvt_to_tile_layers(mvt_bytes)
        .map_err(|e| MaptileError::Transcode(format!("mvt parse: {e}")))?;
    let mut output = Vec::with_capacity(mvt_bytes.len());
    for layer in layers {
        let encoded = layer
            .encode(mlt_core::encoder::EncoderConfig::default())
            .map_err(|e| MaptileError::Transcode(format!("mlt layer encode: {e}")))?;
        output.extend_from_slice(&encoded);
    }
    Ok(Bytes::from(output))
}

// ---------------------------------------------------------------------------
// MLT → MVT
// ---------------------------------------------------------------------------

/// Decode an MLT tile to MVT (protobuf) bytes.
pub fn mlt_to_mvt(mlt_bytes: &[u8]) -> MaptileResult<Bytes> {
    let mut parser = mlt_core::Parser::default();
    let layers = parser
        .parse_layers(mlt_bytes)
        .map_err(|e| MaptileError::Transcode(format!("mlt parse: {e}")))?;

    let mut dec = mlt_core::Decoder::default();
    let parsed: Vec<_> = layers
        .into_iter()
        .map(|layer| {
            layer
                .decode_all(&mut dec)
                .map_err(|e| MaptileError::Transcode(format!("mlt decode layer: {e}")))
        })
        .collect::<MaptileResult<_>>()?;

    let fc = mlt_core::geojson::FeatureCollection::from_layers(parsed)
        .map_err(|e| MaptileError::Transcode(format!("mlt -> features: {e}")))?;

    let mvt_tile = feature_collection_to_mvt(&fc);
    Ok(Bytes::from(mvt_tile.encode_to_vec()))
}

fn feature_collection_to_mvt(fc: &mlt_core::geojson::FeatureCollection) -> mvt_proto::Tile {
    use std::collections::HashMap;

    let mut mvt_layers: Vec<mvt_proto::Layer> = Vec::with_capacity(8);
    let mut layer_map: HashMap<&str, Vec<&mlt_core::geojson::Feature>> = HashMap::with_capacity(8);
    for feature in &fc.features {
        let layer_name = feature
            .properties
            .get("_layer")
            .and_then(|v| v.as_str())
            .unwrap_or("default");
        layer_map.entry(layer_name).or_default().push(feature);
    }

    for (layer_name, features) in &layer_map {
        let extent = features
            .first()
            .and_then(|f| f.properties.get("_extent"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(4096);

        let mut keys: Vec<String> = Vec::with_capacity(32);
        let mut values: Vec<mvt_proto::Value> = Vec::with_capacity(features.len() * 4);
        let mut key_index: HashMap<String, u32> = HashMap::with_capacity(32);
        let mut value_index: HashMap<String, u32> = HashMap::with_capacity(features.len() * 4);
        let mut mvt_features: Vec<mvt_proto::Feature> = Vec::with_capacity(features.len());

        for feature in features {
            let (geom_type, geometry) = encode_geometry_to_mvt(&feature.geometry);

            let mut tags = Vec::with_capacity(feature.properties.len() * 2);
            for (key, val) in &feature.properties {
                if key.starts_with('_') {
                    continue;
                }
                let key_idx = *key_index.entry(key.clone()).or_insert_with(|| {
                    let idx = keys.len() as u32;
                    keys.push(key.clone());
                    idx
                });
                let val_str = val.to_string();
                let val_idx = *value_index.entry(val_str.clone()).or_insert_with(|| {
                    let idx = values.len() as u32;
                    values.push(json_value_to_mvt(val));
                    idx
                });
                tags.push(key_idx);
                tags.push(val_idx);
            }

            mvt_features.push(mvt_proto::Feature {
                id: feature.id,
                tags,
                r#type: Some(geom_type as i32),
                geometry,
            });
        }

        mvt_layers.push(mvt_proto::Layer {
            version: 2,
            name: layer_name.to_string(),
            features: mvt_features,
            keys,
            values,
            extent: Some(extent),
        });
    }

    mvt_proto::Tile { layers: mvt_layers }
}

// ---------------------------------------------------------------------------
// Geometry encoding
// ---------------------------------------------------------------------------

fn encode_geometry_to_mvt(geometry: &geo_types::Geometry<i32>) -> (mvt_proto::GeomType, Vec<u32>) {
    use geo_types::Geometry;

    match geometry {
        Geometry::Point(point) => {
            let commands = encode_point(point.x(), point.y());
            (mvt_proto::GeomType::Point, commands)
        }
        Geometry::MultiPoint(mp) => {
            let points = mp.0.as_slice();
            let mut commands = Vec::with_capacity(points.len() * 3);
            commands.push(command_integer(1, points.len() as u32));
            let mut cx = 0i32;
            let mut cy = 0i32;
            for point in points {
                let dx = point.x() - cx;
                let dy = point.y() - cy;
                commands.push(zigzag_encode(dx));
                commands.push(zigzag_encode(dy));
                cx = point.x();
                cy = point.y();
            }
            (mvt_proto::GeomType::Point, commands)
        }
        Geometry::LineString(ls) => {
            let commands = encode_linestring(&ls.0, false);
            (mvt_proto::GeomType::Linestring, commands)
        }
        Geometry::MultiLineString(mls) => {
            let mut commands = Vec::with_capacity(mls.0.len() * 8);
            for line in &mls.0 {
                commands.extend(encode_linestring(&line.0, false));
            }
            (mvt_proto::GeomType::Linestring, commands)
        }
        Geometry::Polygon(poly) => {
            let mut commands = Vec::with_capacity(poly.exterior().0.len() * 2 + 4);
            commands.extend(encode_linestring(&poly.exterior().0, true));
            for ring in poly.interiors() {
                commands.extend(encode_linestring(&ring.0, true));
            }
            (mvt_proto::GeomType::Polygon, commands)
        }
        Geometry::MultiPolygon(mp) => {
            let mut commands = Vec::with_capacity(mp.0.len() * 16);
            for polygon in &mp.0 {
                commands.extend(encode_linestring(&polygon.exterior().0, true));
                for ring in polygon.interiors() {
                    commands.extend(encode_linestring(&ring.0, true));
                }
            }
            (mvt_proto::GeomType::Polygon, commands)
        }
        _ => (mvt_proto::GeomType::Unknown, Vec::new()),
    }
}

fn encode_point(x: i32, y: i32) -> Vec<u32> {
    vec![command_integer(1, 1), zigzag_encode(x), zigzag_encode(y)]
}

fn encode_linestring(coords: &[geo_types::Coord<i32>], close_path: bool) -> Vec<u32> {
    if coords.is_empty() {
        return Vec::new();
    }

    let mut commands = Vec::with_capacity(coords.len() * 2 + 4);
    let mut cx = 0i32;
    let mut cy = 0i32;

    commands.push(command_integer(1, 1));
    let dx = coords[0].x - cx;
    let dy = coords[0].y - cy;
    commands.push(zigzag_encode(dx));
    commands.push(zigzag_encode(dy));
    cx = coords[0].x;
    cy = coords[0].y;

    let line_count = if close_path {
        coords.len().saturating_sub(2)
    } else {
        coords.len() - 1
    };

    if line_count > 0 {
        commands.push(command_integer(2, line_count as u32));
        let end = if close_path {
            coords.len() - 1
        } else {
            coords.len()
        };
        for coord in &coords[1..end] {
            let dx = coord.x - cx;
            let dy = coord.y - cy;
            commands.push(zigzag_encode(dx));
            commands.push(zigzag_encode(dy));
            cx = coord.x;
            cy = coord.y;
        }
    }

    if close_path {
        commands.push(command_integer(7, 1));
    }
    commands
}

#[inline]
fn command_integer(id: u32, count: u32) -> u32 {
    (id & 0x7) | (count << 3)
}

#[inline]
fn zigzag_encode(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

fn json_value_to_mvt(val: &serde_json::Value) -> mvt_proto::Value {
    match val {
        serde_json::Value::String(s) => mvt_proto::Value {
            string_value: Some(s.clone()),
            ..Default::default()
        },
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                mvt_proto::Value {
                    int_value: Some(i),
                    ..Default::default()
                }
            } else if let Some(u) = n.as_u64() {
                mvt_proto::Value {
                    uint_value: Some(u),
                    ..Default::default()
                }
            } else if let Some(f) = n.as_f64() {
                mvt_proto::Value {
                    double_value: Some(f),
                    ..Default::default()
                }
            } else {
                mvt_proto::Value::default()
            }
        }
        serde_json::Value::Bool(b) => mvt_proto::Value {
            bool_value: Some(*b),
            ..Default::default()
        },
        _ => mvt_proto::Value::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_mvt_layer(name: &str) -> mvt_proto::Layer {
        mvt_proto::Layer {
            version: 2,
            name: name.into(),
            features: vec![],
            keys: vec![],
            values: vec![],
            extent: Some(4096),
        }
    }

    fn point_feature(x: i32, y: i32, props: &[(&str, &str)]) -> mvt_proto::Feature {
        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<mvt_proto::Value> = Vec::new();
        let mut tags: Vec<u32> = Vec::new();
        for (i, (k, v)) in props.iter().enumerate() {
            keys.push((*k).into());
            values.push(mvt_proto::Value {
                string_value: Some((*v).into()),
                ..Default::default()
            });
            tags.push(i as u32);
            tags.push(i as u32);
        }
        let _ = (keys, values);
        mvt_proto::Feature {
            id: Some(1),
            tags,
            r#type: Some(mvt_proto::GeomType::Point as i32),
            geometry: vec![command_integer(1, 1), zigzag_encode(x), zigzag_encode(y)],
        }
    }

    #[test]
    fn zigzag_known_pairs() {
        assert_eq!(zigzag_encode(0), 0);
        assert_eq!(zigzag_encode(-1), 1);
        assert_eq!(zigzag_encode(1), 2);
        assert_eq!(zigzag_encode(-2), 3);
    }

    #[test]
    fn command_integer_packs_id_and_count() {
        // MoveTo, count=1 -> (1 & 7) | (1 << 3) = 9
        assert_eq!(command_integer(1, 1), 9);
        // ClosePath, count=1 -> (7 & 7) | (1 << 3) = 15
        assert_eq!(command_integer(7, 1), 15);
        // LineTo, count=3 -> (2 & 7) | (3 << 3) = 26
        assert_eq!(command_integer(2, 3), 26);
    }

    #[test]
    fn single_point_feature_roundtrips_geometry() {
        let mut layer = empty_mvt_layer("places");
        layer.features = vec![point_feature(10, 20, &[])];
        let tile = mvt_proto::Tile {
            layers: vec![layer],
        };
        let bytes = tile.encode_to_vec();
        let mlt = mvt_to_mlt(&bytes).expect("mvt -> mlt");
        let mvt_back = mlt_to_mvt(&mlt).expect("mlt -> mvt");
        let parsed = mvt_proto::Tile::decode(&mvt_back[..]).unwrap();
        assert_eq!(parsed.layers.len(), 1);
        let layer = &parsed.layers[0];
        assert_eq!(layer.name, "places");
        assert_eq!(layer.features.len(), 1);
        let feat = &layer.features[0];
        assert_eq!(feat.r#type, Some(mvt_proto::GeomType::Point as i32));
        // Geometry: MoveTo(1) + dx, dy where (dx, dy) zigzag-encode (10, 20).
        assert_eq!(feat.geometry[0], command_integer(1, 1));
        assert_eq!(feat.geometry[1], zigzag_encode(10));
        assert_eq!(feat.geometry[2], zigzag_encode(20));
    }
}
