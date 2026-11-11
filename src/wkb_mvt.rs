//! WKB → MVT geometry encoder.
//!
//! Pure-data helper that turns a stream of WKB-encoded features into a
//! Mapbox Vector Tile (MVT) layer. No DB deps; intended to be reused by
//! sources that yield raw WKB blobs rather than a pre-encoded MVT BLOB.
//! GeoParquet dynamic tiles use DuckDB Spatial to project and clip geometry.
//!
//! Backed by `geozero` 0.15. Supports the full OGC SFA WKB type set plus
//! EWKB (PostGIS dialect with optional SRID prefix). Z/M coordinates are
//! silently dropped during the 2D MVT projection. GeometryCollection is
//! supported through geozero's WKB reader.
//!
//! Per-feature flow:
//!
//! 1. Try `Ewkb(bytes).process_geom(&mut writer)` — accepts PostGIS EWKB
//!    (with optional SRID prefix and Z/M dimensions).
//! 2. On failure, retry with `Wkb(bytes).process_geom(...)` (plain OGC).
//! 3. Lift the resulting `tile::Feature.geometry` (the MVT command-int
//!    stream) into our [`proto::Feature`] together with our own id and
//!    dedup'd tag indices.
//!
//! `geozero::mvt::MvtWriter::new` does the projection + Y-axis flip, so
//! the closure-based projector from the pre-geozero implementation is
//! gone.

use crate::error::{MaptileError, MaptileResult};
use crate::sources::mvt::proto;
use bytes::Bytes;
use geozero::FeatureProcessor;
use geozero::GeozeroGeometry;
use geozero::mvt::MvtWriter;
use geozero::wkb::{Ewkb, Wkb};

/// Maximum accepted WKB blob size. Replaces the deleted depth /
/// safe-capacity guards from the hand-rolled parser: we trust geozero to
/// bail on a truncated buffer, but still reject pathologically large
/// inputs at the API boundary so a single malicious feature can't trigger
/// multi-GB allocations downstream.
const MAX_WKB_BYTES: usize = 16 * 1024 * 1024;

/// One feature carried into the encoder.
#[derive(Debug, Clone)]
pub struct WkbFeature {
    /// Optional `feature.id` (MVT spec §4.2 — must fit in u64).
    pub id: Option<u64>,
    /// WKB bytes — accepted dialects: ISO WKB (any endianness) and PostGIS
    /// EWKB (with optional SRID prefix). Z/M dimensions are tolerated and
    /// dropped to 2D for MVT output.
    pub wkb: Bytes,
    /// Property bag — JSON values are folded into the layer's tag table.
    pub properties: serde_json::Map<String, serde_json::Value>,
}

/// Encode an iterator of features into a single-layer MVT tile.
///
/// `tile_envelope_4326` is the WGS84 bounding box `(minx, miny, maxx, maxy)`
/// of the tile being produced. WKB coordinates are assumed to be in WGS84
/// degrees and are projected linearly into the `0..extent` tile-pixel
/// space (Y axis flipped so origin is top-left). For Web-Mercator
/// projection preserving fidelity, callers should pre-warp WKB into the
/// tile's local space before invoking this helper.
///
/// Returns the encoded tile bytes (uncompressed).
pub fn encode_features_to_mvt<I>(
    layer_name: &str,
    features: I,
    tile_envelope_4326: (f64, f64, f64, f64),
    extent: u32,
) -> MaptileResult<Bytes>
where
    I: IntoIterator<Item = WkbFeature>,
{
    let (minx, miny, maxx, maxy) = tile_envelope_4326;
    if (maxx - minx).abs() < f64::EPSILON || (maxy - miny).abs() < f64::EPSILON {
        return Err(MaptileError::Transcode(
            "tile envelope has zero width or height".into(),
        ));
    }
    if extent == 0 {
        return Err(MaptileError::Transcode("extent must be > 0".into()));
    }

    let mut layer = proto::Layer {
        version: 2,
        name: layer_name.to_string(),
        features: Vec::new(),
        keys: Vec::new(),
        values: Vec::new(),
        extent: Some(extent),
    };

    let mut key_index: indexmap::IndexMap<String, u32> = indexmap::IndexMap::new();
    let mut value_index: indexmap::IndexMap<TagValueKey, u32> = indexmap::IndexMap::new();

    // Reuse a single MvtWriter across all features to amortise the cost
    // of constructing the internal TagsBuilder + DupIndexer maps. We
    // never use geozero's own tag table — only its geometry command
    // stream — but we still pay an allocation per `MvtWriter::new`, so
    // reusing the writer is significantly cheaper for high-feature tiles.
    let mut writer = MvtWriter::new(extent, minx, miny, maxx, maxy)
        .map_err(|e| MaptileError::Transcode(format!("mvt writer: {e}")))?;

    for feature in features {
        if feature.wkb.len() > MAX_WKB_BYTES {
            return Err(MaptileError::Transcode(format!(
                "wkb: blob exceeds {MAX_WKB_BYTES}-byte cap ({} bytes)",
                feature.wkb.len()
            )));
        }

        let geom_feat = encode_one_geometry(&mut writer, &feature.wkb)?;

        // Build tag table — keep our typed dedup so we don't pay for the
        // `format!("{value:?}")` allocation that geozero's TagsBuilder
        // would impose for non-scalar JSON values.
        let mut tags = Vec::with_capacity(feature.properties.len() * 2);
        for (k, v) in &feature.properties {
            let key_idx = match key_index.get(k) {
                Some(&i) => i,
                None => {
                    let i = layer.keys.len() as u32;
                    layer.keys.push(k.clone());
                    key_index.insert(k.clone(), i);
                    i
                }
            };
            let key = TagValueKey::from_json(v);
            let value_idx = match value_index.get(&key) {
                Some(&i) => i,
                None => {
                    let i = layer.values.len() as u32;
                    layer.values.push(key.to_proto_value());
                    value_index.insert(key, i);
                    i
                }
            };
            tags.push(key_idx);
            tags.push(value_idx);
        }

        layer.features.push(proto::Feature {
            id: feature.id,
            tags,
            r#type: geom_feat.r#type,
            geometry: geom_feat.geometry,
        });
    }

    let tile = proto::Tile {
        layers: vec![layer],
    };
    Ok(crate::sources::mvt::encode(&tile))
}

/// Encode a single WKB geometry into an MVT command stream by driving
/// the supplied (reusable) `MvtWriter`. Tries EWKB first — it accepts
/// the same byte sequence as plain WKB when no SRID flag is present —
/// and falls back to ISO WKB on any error.
///
/// We call `feature_begin` / `geometry_begin` ourselves to reset the
/// writer's per-feature state (`last_x`, `last_y`, `line_state`, and the
/// in-progress `feature` accumulator).
fn encode_one_geometry(writer: &mut MvtWriter, wkb: &[u8]) -> MaptileResult<EncodedGeom> {
    writer
        .feature_begin(0)
        .map_err(|e| MaptileError::Transcode(format!("mvt feature_begin: {e}")))?;
    writer
        .geometry_begin()
        .map_err(|e| MaptileError::Transcode(format!("mvt geometry_begin: {e}")))?;

    let ewkb_err = match Ewkb(wkb).process_geom(writer) {
        Ok(()) => {
            let f = writer.geometry();
            return Ok(EncodedGeom {
                r#type: f.r#type,
                geometry: f.geometry.clone(),
            });
        }
        Err(e) => e,
    };

    // Reset state and retry as plain WKB.
    writer
        .feature_begin(0)
        .map_err(|e| MaptileError::Transcode(format!("mvt feature_begin: {e}")))?;
    writer
        .geometry_begin()
        .map_err(|e| MaptileError::Transcode(format!("mvt geometry_begin: {e}")))?;
    match Wkb(wkb).process_geom(writer) {
        Ok(()) => {
            let f = writer.geometry();
            Ok(EncodedGeom {
                r#type: f.r#type,
                geometry: f.geometry.clone(),
            })
        }
        Err(e2) => Err(MaptileError::Transcode(format!(
            "wkb decode: ewkb={ewkb_err}; wkb={e2}"
        ))),
    }
}

struct EncodedGeom {
    r#type: Option<i32>,
    geometry: Vec<u32>,
}

/// Compute the 2D bounding box of a WKB geometry without decoding it
/// further. Returns `(minx, miny, maxx, maxy)` or `None` if the WKB is
/// truncated, uses an unsupported dialect, or contains no coordinates.
///
/// Cheap enough to call per row of a parquet scan. Used by the
/// `geoparquet` live-geometry path to filter rows by tile envelope
/// before encoding.
#[must_use]
pub fn wkb_bbox(wkb: &[u8]) -> Option<(f64, f64, f64, f64)> {
    if wkb.len() > MAX_WKB_BYTES {
        return None;
    }
    let mut p = geozero::bounds::BoundsProcessor::new();
    if Ewkb(wkb).process_geom(&mut p).is_err() {
        // Reset and retry as plain WKB.
        let mut p2 = geozero::bounds::BoundsProcessor::new();
        if Wkb(wkb).process_geom(&mut p2).is_err() {
            return None;
        }
        let b = p2.bounds()?;
        return Some((b.min_x(), b.min_y(), b.max_x(), b.max_y()));
    }
    let b = p.bounds()?;
    Some((b.min_x(), b.min_y(), b.max_x(), b.max_y()))
}

// ---------------------------------------------------------------------------
// Tag-value dedup helper.
//
// We use our own `IndexMap<TagValueKey, u32>` rather than geozero's
// `TagsBuilder` so non-scalar JSON values (arrays / objects) hash on a
// stable string representation without round-tripping through Debug.
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Hash)]
enum TagValueKey {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    /// Stores `f64::to_bits` so NaNs hash consistently with their bit
    /// pattern (good enough for tag-table dedup).
    Double(u64),
    String(String),
}

impl TagValueKey {
    fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => TagValueKey::Null,
            serde_json::Value::Bool(b) => TagValueKey::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    TagValueKey::Int(i)
                } else if let Some(u) = n.as_u64() {
                    TagValueKey::UInt(u)
                } else if let Some(f) = n.as_f64() {
                    TagValueKey::Double(f.to_bits())
                } else {
                    TagValueKey::Null
                }
            }
            serde_json::Value::String(s) => TagValueKey::String(s.clone()),
            other => TagValueKey::String(other.to_string()),
        }
    }

    fn to_proto_value(&self) -> proto::Value {
        let mut out = proto::Value::default();
        match self {
            TagValueKey::Null => {}
            TagValueKey::Bool(b) => out.bool_value = Some(*b),
            TagValueKey::Int(i) => out.sint_value = Some(*i),
            TagValueKey::UInt(u) => out.uint_value = Some(*u),
            TagValueKey::Double(bits) => out.double_value = Some(f64::from_bits(*bits)),
            TagValueKey::String(s) => out.string_value = Some(s.clone()),
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::mvt;

    /// Build little-endian WKB for a 2D Point.
    fn wkb_point(x: f64, y: f64) -> Bytes {
        let mut v = Vec::with_capacity(1 + 4 + 16);
        v.push(0x01);
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        Bytes::from(v)
    }

    #[test]
    fn empty_iter_produces_layer_with_no_features() {
        let bytes = encode_features_to_mvt(
            "empty",
            std::iter::empty::<WkbFeature>(),
            (-180.0, -85.0, 180.0, 85.0),
            4096,
        )
        .unwrap();
        let tile = mvt::decode(&bytes).unwrap();
        assert_eq!(tile.layers.len(), 1);
        assert_eq!(tile.layers[0].name, "empty");
        assert!(tile.layers[0].features.is_empty());
    }

    #[test]
    fn single_point_produces_one_feature_with_id() {
        let f = WkbFeature {
            id: Some(7),
            wkb: wkb_point(0.0, 0.0),
            properties: serde_json::Map::new(),
        };
        let bytes = encode_features_to_mvt(
            "pts",
            std::iter::once(f),
            (-180.0, -90.0, 180.0, 90.0),
            4096,
        )
        .unwrap();
        let tile = mvt::decode(&bytes).unwrap();
        let layer = &tile.layers[0];
        assert_eq!(layer.features.len(), 1);
        let feat = &layer.features[0];
        assert_eq!(feat.id, Some(7));
        assert_eq!(feat.r#type, Some(mvt::proto::GeomType::Point as i32));
        assert_eq!(feat.geometry.len(), 3);
        assert_eq!(feat.geometry[0], 9); // MoveTo(1)
    }

    #[test]
    fn properties_become_tag_table() {
        let mut props = serde_json::Map::new();
        props.insert("name".into(), serde_json::json!("river"));
        props.insert("length".into(), serde_json::json!(42));
        let f = WkbFeature {
            id: Some(1),
            wkb: wkb_point(0.0, 0.0),
            properties: props,
        };
        let bytes =
            encode_features_to_mvt("x", std::iter::once(f), (-180.0, -90.0, 180.0, 90.0), 4096)
                .unwrap();
        let tile = mvt::decode(&bytes).unwrap();
        let layer = &tile.layers[0];
        assert!(layer.keys.contains(&"name".to_string()));
        assert!(layer.keys.contains(&"length".to_string()));
        assert_eq!(layer.features[0].tags.len(), 4);
    }

    #[test]
    fn duplicate_property_values_share_tag_slot() {
        let mut p1 = serde_json::Map::new();
        p1.insert("kind".into(), serde_json::json!("road"));
        let mut p2 = serde_json::Map::new();
        p2.insert("kind".into(), serde_json::json!("road"));
        let bytes = encode_features_to_mvt(
            "x",
            vec![
                WkbFeature {
                    id: None,
                    wkb: wkb_point(0.0, 0.0),
                    properties: p1,
                },
                WkbFeature {
                    id: None,
                    wkb: wkb_point(0.0, 0.0),
                    properties: p2,
                },
            ],
            (-180.0, -90.0, 180.0, 90.0),
            4096,
        )
        .unwrap();
        let tile = mvt::decode(&bytes).unwrap();
        assert_eq!(tile.layers[0].keys.len(), 1);
        assert_eq!(tile.layers[0].values.len(), 1);
    }

    #[test]
    fn zero_envelope_is_transcode_error() {
        let f = WkbFeature {
            id: None,
            wkb: wkb_point(0.0, 0.0),
            properties: serde_json::Map::new(),
        };
        let res = encode_features_to_mvt("x", std::iter::once(f), (0.0, 0.0, 0.0, 0.0), 4096);
        assert!(matches!(res, Err(MaptileError::Transcode(_))));
    }

    #[test]
    fn truncated_wkb_is_transcode_error() {
        let f = WkbFeature {
            id: None,
            wkb: Bytes::from(vec![0x01, 0x01, 0x00]),
            properties: serde_json::Map::new(),
        };
        let res =
            encode_features_to_mvt("x", std::iter::once(f), (-180.0, -90.0, 180.0, 90.0), 4096);
        assert!(matches!(res, Err(MaptileError::Transcode(_))));
    }

    #[test]
    fn big_endian_wkb_is_parsed() {
        let mut v = Vec::with_capacity(1 + 4 + 16);
        v.push(0x00); // big endian
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1.0_f64.to_be_bytes());
        v.extend_from_slice(&2.0_f64.to_be_bytes());
        let f = WkbFeature {
            id: None,
            wkb: Bytes::from(v),
            properties: serde_json::Map::new(),
        };
        let bytes =
            encode_features_to_mvt("x", std::iter::once(f), (0.0, 0.0, 10.0, 10.0), 4096).unwrap();
        let tile = mvt::decode(&bytes).unwrap();
        assert_eq!(tile.layers[0].features.len(), 1);
    }

    #[test]
    fn wkb_bbox_for_point() {
        let bbox = wkb_bbox(&wkb_point(3.0, 4.0)).unwrap();
        assert!((bbox.0 - 3.0).abs() < 1e-9);
        assert!((bbox.1 - 4.0).abs() < 1e-9);
        assert!((bbox.2 - 3.0).abs() < 1e-9);
        assert!((bbox.3 - 4.0).abs() < 1e-9);
    }
}
