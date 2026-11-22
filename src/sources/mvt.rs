//! Protobuf encoding and decoding for uncompressed Mapbox Vector Tiles.

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
}
