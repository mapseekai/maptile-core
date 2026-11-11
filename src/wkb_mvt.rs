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
//! 1. Validate the full WKB structure without allocating from input counts,
//!    trying PostGIS EWKB first, then ISO WKB.
//! 2. Decode with the validated dialect into `MvtWriter`.
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

/// Maximum accepted WKB blob size. Counts must also fit the payload before
/// reaching geozero, whose MVT writer reserves capacity before reading it.
const MAX_WKB_BYTES: usize = 16 * 1024 * 1024;
/// Bound both the validator's and geozero's recursive geometry traversal.
const MAX_WKB_DEPTH: usize = 64;

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
/// Rejects malformed WKB, blobs over 16 MiB, and nesting beyond 64 geometries.
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
/// the supplied (reusable) `MvtWriter`. Only the dialect whose complete
/// structure passed validation can reach the allocating writer.
///
/// We call `feature_begin` / `geometry_begin` ourselves to reset the
/// writer's per-feature state (`last_x`, `last_y`, `line_state`, and the
/// in-progress `feature` accumulator).
fn encode_one_geometry(writer: &mut MvtWriter, wkb: &[u8]) -> MaptileResult<EncodedGeom> {
    let format = validate_wkb(wkb)?;
    writer
        .feature_begin(0)
        .map_err(|e| MaptileError::Transcode(format!("mvt feature_begin: {e}")))?;
    writer
        .geometry_begin()
        .map_err(|e| MaptileError::Transcode(format!("mvt geometry_begin: {e}")))?;

    format
        .process(wkb, writer)
        .map_err(|e| MaptileError::Transcode(format!("wkb decode: {e}")))?;
    let f = writer.geometry();
    Ok(EncodedGeom {
        r#type: f.r#type,
        geometry: f.geometry.clone(),
    })
}

struct EncodedGeom {
    r#type: Option<i32>,
    geometry: Vec<u32>,
}

/// Compute the 2D bounding box of a WKB geometry without decoding it
/// further. Returns `(minx, miny, maxx, maxy)` or `None` if the WKB is
/// malformed, uses an unsupported dialect, contains no coordinates, exceeds
/// 16 MiB, or is nested beyond 64 geometries.
///
/// Cheap enough to call per row of a parquet scan. Used by the
/// `geoparquet` live-geometry path to filter rows by tile envelope
/// before encoding.
#[must_use]
pub fn wkb_bbox(wkb: &[u8]) -> Option<(f64, f64, f64, f64)> {
    let format = validate_wkb(wkb).ok()?;
    let mut p = geozero::bounds::BoundsProcessor::new();
    format.process(wkb, &mut p).ok()?;
    let b = p.bounds()?;
    Some((b.min_x(), b.min_y(), b.max_x(), b.max_y()))
}

#[derive(Clone, Copy)]
enum WkbFormat {
    Ewkb,
    Iso,
}

impl WkbFormat {
    fn process<P: geozero::GeomProcessor>(
        self,
        wkb: &[u8],
        processor: &mut P,
    ) -> geozero::error::Result<()> {
        match self {
            Self::Ewkb => Ewkb(wkb).process_geom(processor),
            Self::Iso => Wkb(wkb).process_geom(processor),
        }
    }
}

/// Structural preflight, not a second coordinate decoder. Skip coordinates
/// only after proving their byte count fits; never reserve from a WKB count.
/// Validate child types too: geozero's multi-geometry readers assume them.
fn validate_wkb(wkb: &[u8]) -> MaptileResult<WkbFormat> {
    if wkb.len() > MAX_WKB_BYTES {
        return Err(MaptileError::Transcode(format!(
            "wkb validation: blob exceeds {MAX_WKB_BYTES}-byte cap ({} bytes)",
            wkb.len()
        )));
    }
    let validate = |format| {
        let mut reader = WkbValidator {
            remaining: wkb,
            format,
        };
        reader.geometry(1, &[], false)?;
        if !reader.remaining.is_empty() {
            return Err("trailing bytes after geometry");
        }
        Ok(())
    };
    let ewkb_err = match validate(WkbFormat::Ewkb) {
        Ok(()) => return Ok(WkbFormat::Ewkb),
        Err(e) => e,
    };
    validate(WkbFormat::Iso)
        .map(|()| WkbFormat::Iso)
        .map_err(|e| MaptileError::Transcode(format!("wkb validation: ewkb={ewkb_err}; wkb={e}")))
}

struct WkbValidator<'a> {
    remaining: &'a [u8],
    format: WkbFormat,
}

impl<'a> WkbValidator<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], &'static str> {
        let (bytes, rest) = self
            .remaining
            .split_at_checked(size)
            .ok_or("truncated geometry")?;
        self.remaining = rest;
        Ok(bytes)
    }

    fn u32(&mut self, little_endian: bool) -> Result<u32, &'static str> {
        let bytes = self.take(4)?.try_into().expect("checked four-byte slice");
        Ok(if little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn count(&mut self, little_endian: bool, min_bytes: usize) -> Result<usize, &'static str> {
        let count = self.u32(little_endian)? as usize;
        // Division avoids overflow even on a 32-bit target.
        if count > self.remaining.len() / min_bytes {
            return Err("count exceeds remaining payload");
        }
        Ok(count)
    }

    fn coordinates(&mut self, little_endian: bool, stride: usize) -> Result<usize, &'static str> {
        let count = self.count(little_endian, stride)?;
        self.take(count * stride)?;
        Ok(count)
    }

    fn geometry(
        &mut self,
        depth: usize,
        allowed_types: &[u32],
        untagged_line: bool,
    ) -> Result<(), &'static str> {
        if depth > MAX_WKB_DEPTH {
            return Err("geometry nesting exceeds 64 levels");
        }
        let little_endian = match self.take(1)?[0] {
            0 => false,
            1 => true,
            _ => return Err("invalid byte order"),
        };
        let type_id = self.u32(little_endian)?;
        let (kind, dimensions) = match self.format {
            WkbFormat::Ewkb => {
                if type_id & 0x2000_0000 != 0 {
                    self.take(4)?; // Optional SRID, in this geometry's byte order.
                }
                (
                    type_id & 0x1fff_ffff,
                    2 + usize::from(type_id & 0x8000_0000 != 0)
                        + usize::from(type_id & 0x4000_0000 != 0),
                )
            }
            WkbFormat::Iso => {
                let dimensions = match type_id / 1000 {
                    0 => 2,
                    1 | 2 => 3,
                    3 => 4,
                    _ => return Err("invalid ISO geometry type"),
                };
                (type_id % 1000, dimensions)
            }
        };
        if !allowed_types.is_empty() && !allowed_types.contains(&kind) {
            return Err("invalid child geometry type");
        }
        let stride = dimensions * 8;
        let child_types: &[u32] = match kind {
            1 => return self.take(stride).map(|_| ()),
            2 | 8 => {
                let count = self.coordinates(little_endian, stride)?;
                if kind == 2 && untagged_line && count == 0 {
                    return Err("untagged line has no coordinates");
                }
                return Ok(());
            }
            3 | 17 => {
                let rings = self.count(little_endian, 4)?;
                for _ in 0..rings {
                    if self.coordinates(little_endian, stride)? == 0 {
                        // geozero's ring reservation subtracts one from this count.
                        return Err("polygon ring has no coordinates");
                    }
                }
                return Ok(());
            }
            4 => &[1],
            5 => &[2],
            6 | 15 => &[3],
            7 => &[],
            9 => &[2, 8],
            10 | 11 => &[2, 8, 9],
            12 => &[3, 10],
            16 => &[17],
            _ => return Err("unsupported geometry type"),
        };
        // A child needs at least its header and count (9 bytes); Points
        // instead need a header and at least two f64 coordinates (21 bytes).
        let count = self.count(little_endian, if kind == 4 { 21 } else { 9 })?;
        for _ in 0..count {
            self.geometry(depth + 1, child_types, matches!(kind, 9..=11))?;
        }
        Ok(())
    }
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

    #[test]
    fn invalid_count_is_rejected_before_touching_the_writer() {
        let mut writer = MvtWriter::new(4096, -180.0, -90.0, 180.0, 90.0).unwrap();
        encode_one_geometry(&mut writer, &wkb_point(3.0, 4.0)).unwrap();
        let before = writer.geometry().clone();
        for kind in [2u32, 4] {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(&u32::MAX.to_le_bytes());
            assert!(matches!(
                encode_one_geometry(&mut writer, &bytes),
                Err(MaptileError::Transcode(message)) if message.starts_with("wkb validation:")
            ));
            // Validation runs before even feature_begin, let alone a reserve.
            assert_eq!(writer.geometry(), &before);
        }
    }

    #[test]
    fn nesting_limit_accepts_64_levels_and_rejects_65() {
        let mut bytes = Vec::new();
        for _ in 1..MAX_WKB_DEPTH {
            bytes.push(1);
            bytes.extend_from_slice(&7u32.to_le_bytes());
            bytes.extend_from_slice(&1u32.to_le_bytes());
        }
        bytes.extend_from_slice(&wkb_point(3.0, 4.0));
        let mut writer = MvtWriter::new(4096, -180.0, -90.0, 180.0, 90.0).unwrap();
        assert!(encode_one_geometry(&mut writer, &bytes).is_ok());
        assert_eq!(wkb_bbox(&bytes), Some((3.0, 4.0, 3.0, 4.0)));

        let mut deeper = bytes[..9].to_vec();
        deeper.extend_from_slice(&bytes);
        assert!(encode_one_geometry(&mut writer, &deeper).is_err());
        assert!(wkb_bbox(&deeper).is_none());
    }
}
