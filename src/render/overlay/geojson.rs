//! Ported from `tileserver-rs` (`crates/tileserver-rs/src/render/overlay.rs`)
//! under the MIT license; see `third_party/tileserver-rs/LICENSE`.

use super::{GeoPoint, MarkerOverlay, PathOverlay, parse_hex_color};
use image::Rgba;
use serde_json::Value;

/// Parse a GeoJSON `Geometry`, `Feature`, or `FeatureCollection` into
/// path and marker overlays.
///
/// Styling is read from `Feature.properties` per the simplestyle spec:
/// - `stroke` — hex stroke color (default `0000ff`)
/// - `stroke-width` — pixel width (default `3.0`)
/// - `fill` — hex fill color for polygons (optional)
/// - `marker-color` — hex marker color (default `ff0000`)
/// - `marker-size` — `small`/`medium`/`large` enum or pixel value
///
/// Invalid JSON is silently logged at WARN and yields empty results.
#[must_use]
pub fn parse_geojson(geojson_str: &str) -> (Vec<PathOverlay>, Vec<MarkerOverlay>) {
    let mut paths = Vec::new();
    let mut markers = Vec::new();

    let value = match serde_json::from_str::<Value>(geojson_str) {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(target: "maptile_core::render::overlay", "Failed to parse GeoJSON overlay string");
            return (paths, markers);
        }
    };

    process_geojson_value(&value, &mut paths, &mut markers);
    (paths, markers)
}

fn process_geojson_value(
    value: &Value,
    paths: &mut Vec<PathOverlay>,
    markers: &mut Vec<MarkerOverlay>,
) {
    match value.get("type").and_then(Value::as_str) {
        Some("FeatureCollection") => {
            if let Some(features) = value.get("features").and_then(Value::as_array) {
                for feat in features {
                    process_geojson_value(feat, paths, markers);
                }
            }
        }
        Some("Feature") => {
            let props = value.get("properties");
            let stroke_color = props
                .and_then(|p| p.get("stroke"))
                .and_then(Value::as_str)
                .and_then(parse_hex_color)
                .unwrap_or(Rgba([0, 0, 255, 255]));
            let stroke_width = props
                .and_then(|p| p.get("stroke-width"))
                .and_then(Value::as_f64)
                .unwrap_or(3.0) as f32;
            let fill_color = props
                .and_then(|p| p.get("fill"))
                .and_then(Value::as_str)
                .and_then(parse_hex_color);
            let marker_color = props
                .and_then(|p| p.get("marker-color"))
                .and_then(Value::as_str)
                .and_then(parse_hex_color)
                .unwrap_or(Rgba([255, 0, 0, 255]));
            let marker_size = props
                .and_then(|p| p.get("marker-size"))
                .map(parse_marker_size)
                .unwrap_or(12.0);

            if let Some(geom) = value.get("geometry") {
                process_geometry(
                    geom,
                    stroke_color,
                    stroke_width,
                    fill_color,
                    marker_color,
                    marker_size,
                    paths,
                    markers,
                );
            }
        }
        Some(
            "Point" | "MultiPoint" | "LineString" | "MultiLineString" | "Polygon" | "MultiPolygon",
        ) => {
            process_geometry(
                value,
                Rgba([0, 0, 255, 255]),
                3.0,
                None,
                Rgba([255, 0, 0, 255]),
                12.0,
                paths,
                markers,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn process_geometry(
    geom: &Value,
    stroke_color: Rgba<u8>,
    stroke_width: f32,
    fill_color: Option<Rgba<u8>>,
    marker_color: Rgba<u8>,
    marker_size: f32,
    paths: &mut Vec<PathOverlay>,
    markers: &mut Vec<MarkerOverlay>,
) {
    match geom.get("type").and_then(Value::as_str) {
        Some("Point") => {
            if let Some(pos) = geojson_point(geom) {
                markers.push(MarkerOverlay {
                    position: pos,
                    color: marker_color,
                    size: marker_size,
                });
            }
        }
        Some("MultiPoint") => {
            if let Some(coords) = geom.get("coordinates").and_then(Value::as_array) {
                for coord in coords {
                    if let Some(arr) = coord.as_array()
                        && let (Some(lon), Some(lat)) = (
                            arr.first().and_then(Value::as_f64),
                            arr.get(1).and_then(Value::as_f64),
                        )
                    {
                        markers.push(MarkerOverlay {
                            position: GeoPoint { lon, lat },
                            color: marker_color,
                            size: marker_size,
                        });
                    }
                }
            }
        }
        Some("LineString") => {
            let pts = geojson_coord_array(geom, "coordinates");
            if pts.len() >= 2 {
                paths.push(PathOverlay {
                    points: pts,
                    stroke_color,
                    stroke_width,
                    fill_color: None,
                });
            }
        }
        Some("MultiLineString") => {
            if let Some(lines) = geom.get("coordinates").and_then(Value::as_array) {
                for line in lines {
                    if let Some(arr) = line.as_array() {
                        let pts = coords_to_geopoints(arr);
                        if pts.len() >= 2 {
                            paths.push(PathOverlay {
                                points: pts,
                                stroke_color,
                                stroke_width,
                                fill_color: None,
                            });
                        }
                    }
                }
            }
        }
        Some("Polygon") => {
            if let Some(rings) = geom.get("coordinates").and_then(Value::as_array)
                && let Some(outer) = rings.first()
                && let Some(arr) = outer.as_array()
            {
                let pts = coords_to_geopoints(arr);
                if pts.len() >= 3 {
                    paths.push(PathOverlay {
                        points: pts,
                        stroke_color,
                        stroke_width,
                        fill_color,
                    });
                }
            }
        }
        Some("MultiPolygon") => {
            if let Some(polys) = geom.get("coordinates").and_then(Value::as_array) {
                for poly in polys {
                    if let Some(rings) = poly.as_array()
                        && let Some(outer) = rings.first()
                        && let Some(arr) = outer.as_array()
                    {
                        let pts = coords_to_geopoints(arr);
                        if pts.len() >= 3 {
                            paths.push(PathOverlay {
                                points: pts,
                                stroke_color,
                                stroke_width,
                                fill_color,
                            });
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn parse_marker_size(value: &Value) -> f32 {
    if let Some(s) = value.as_str() {
        match s {
            "small" => 8.0,
            "medium" => 12.0,
            "large" => 18.0,
            _ => 12.0,
        }
    } else {
        value.as_f64().unwrap_or(12.0) as f32
    }
}

fn geojson_point(geom: &Value) -> Option<GeoPoint> {
    let arr = geom.get("coordinates")?.as_array()?;
    let lon = arr.first()?.as_f64()?;
    let lat = arr.get(1)?.as_f64()?;
    Some(GeoPoint { lon, lat })
}

fn geojson_coord_array(geom: &Value, field: &str) -> Vec<GeoPoint> {
    geom.get(field)
        .and_then(Value::as_array)
        .map(|a| coords_to_geopoints(a))
        .unwrap_or_default()
}

fn coords_to_geopoints(arr: &[Value]) -> Vec<GeoPoint> {
    arr.iter()
        .filter_map(|c| {
            let pair = c.as_array()?;
            let lon = pair.first()?.as_f64()?;
            let lat = pair.get(1)?.as_f64()?;
            Some(GeoPoint { lon, lat })
        })
        .collect()
}
