//! Minimal Mapbox Vector Tile (MVT) protobuf schema, used for layer-level
//! merging in [`crate::sources::composite`].
//!
//! This is intentionally **incomplete**: we only model the message structure
//! we need to read (decode), rename, and re-encode whole layers. Feature
//! contents (`tags`, `geometry`) are kept as `Vec<u32>` — prost serialises
//! them back unchanged, so we never need to decode the geometry command
//! stream or the per-feature property tables.
//!
//! Wire format reference: <https://github.com/mapbox/vector-tile-spec/blob/master/2.1/vector_tile.proto>

use crate::error::{MaptileError, MaptileResult};
use bytes::Bytes;
// Re-exported so we use the same prost version that geozero's generated
// types depend on (geozero 0.15 → prost 0.14). The workspace also has
// prost 0.13 around for unrelated crates; mixing them on these messages
// would fail at the trait-bound level.
use geozero::mvt::Message;

/// MVT protobuf types — re-exported from `geozero::mvt::vector_tile` so
/// that wire-level structs are kept in lockstep with the geozero
/// adapter used by [`crate::wkb_mvt`]. The shape is identical to the
/// previously hand-defined messages: same field names, same prost
/// tags, same wire encoding.
pub mod proto {
    pub use geozero::mvt::Tile;
    pub use geozero::mvt::tile::{Feature, GeomType, Layer, Value};
}

/// Decode an MVT tile (uncompressed bytes) to its layer list.
pub fn decode(data: &[u8]) -> MaptileResult<proto::Tile> {
    proto::Tile::decode(data).map_err(|e| MaptileError::Transcode(format!("mvt decode: {e}")))
}

/// Encode an MVT tile back to bytes.
pub fn encode(tile: &proto::Tile) -> Bytes {
    let mut out = Vec::with_capacity(tile.encoded_len());
    // prost::Message::encode never fails for a Vec sink.
    tile.encode(&mut out)
        .expect("encode into Vec is infallible");
    Bytes::from(out)
}

/// Consume `tiles` and merge them into a single MVT, optionally prefixing each
/// tile's layer names with `prefixes[i]` (use empty string to skip prefixing).
///
/// On a layer-name collision the **later** tile in `tiles` wins (caller
/// orders sources by visual priority).
pub fn merge(tiles: Vec<proto::Tile>, prefixes: &[&str]) -> proto::Tile {
    debug_assert_eq!(
        tiles.len(),
        prefixes.len(),
        "merge: tiles and prefixes length mismatch"
    );

    let mut by_name: indexmap::IndexMap<String, proto::Layer> = indexmap::IndexMap::new();
    for (tile, prefix) in tiles.into_iter().zip(prefixes.iter()) {
        for mut layer in tile.layers {
            let name = if prefix.is_empty() {
                layer.name.clone()
            } else {
                format!("{prefix}{}", layer.name)
            };
            layer.name = name.clone();
            by_name.insert(name, layer);
        }
    }
    proto::Tile {
        layers: by_name.into_values().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(name: &str) -> proto::Layer {
        proto::Layer {
            version: 2,
            name: name.into(),
            features: Vec::new(),
            keys: Vec::new(),
            values: Vec::new(),
            extent: Some(4096),
        }
    }

    #[test]
    fn decode_encode_roundtrip() {
        let tile = proto::Tile {
            layers: vec![layer("water"), layer("roads")],
        };
        let bytes = encode(&tile);
        let back = decode(&bytes).unwrap();
        assert_eq!(back, tile);
    }

    #[test]
    fn merge_unions_unique_layers() {
        let a = proto::Tile {
            layers: vec![layer("water")],
        };
        let b = proto::Tile {
            layers: vec![layer("roads")],
        };
        let m = merge(vec![a, b], &["", ""]);
        let names: Vec<_> = m.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["water", "roads"]);
    }

    #[test]
    fn merge_later_layer_wins_on_name_collision() {
        let a = proto::Tile {
            layers: vec![proto::Layer {
                extent: Some(256),
                ..layer("places")
            }],
        };
        let b = proto::Tile {
            layers: vec![proto::Layer {
                extent: Some(4096),
                ..layer("places")
            }],
        };
        let m = merge(vec![a, b], &["", ""]);
        assert_eq!(m.layers.len(), 1);
        assert_eq!(m.layers[0].extent, Some(4096));
    }

    #[test]
    fn merge_with_prefix_avoids_collision() {
        let a = proto::Tile {
            layers: vec![layer("places")],
        };
        let b = proto::Tile {
            layers: vec![layer("places")],
        };
        let m = merge(vec![a, b], &["base_", "overlay_"]);
        let names: Vec<_> = m.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["base_places", "overlay_places"]);
    }

    #[test]
    fn merge_decode_roundtrips_through_bytes() {
        let a = proto::Tile {
            layers: vec![layer("water")],
        };
        let b = proto::Tile {
            layers: vec![layer("roads")],
        };
        let merged = merge(vec![a, b], &["", ""]);
        let bytes = encode(&merged);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.layers.len(), 2);
    }
}
