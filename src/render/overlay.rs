//! Overlay drawing for static map images.
//!
//! Ported from `tileserver-rs` (`crates/tileserver-rs/src/render/overlay.rs`)
//! under the MIT license; see `third_party/tileserver-rs/LICENSE`.
//! Supports drawing paths (polylines), markers, and GeoJSON
//! `FeatureCollection`s on top of an already-rendered RGBA image.
//!
//! Public surface:
//! - [`OverlayItem`] — abstract item (path / marker / geojson) used by
//!   adapters that don't want to call `parse_path` / `parse_marker`
//!   directly.
//! - [`PathOverlay`] / [`MarkerOverlay`] — typed primitives.
//! - [`parse_path`] / [`parse_marker`] — Mapbox-style query syntax.
//! - [`parse_geojson`] — GeoJSON `Feature`/`FeatureCollection` →
//!   `(paths, markers)` pair using `simplestyle-spec` properties.
//! - [`draw_overlays`] — paints paths + markers onto a `RgbaImage`.
//! - [`calculate_bounds`] — auto-fit camera helper.

use image::Rgba;

mod draw;
mod geojson;
pub use draw::draw_overlays;
pub use geojson::parse_geojson;

/// A point in geographic coordinates (`lon` / `lat`, GeoJSON order).
#[derive(Debug, Clone, Copy)]
pub struct GeoPoint {
    pub lon: f64,
    pub lat: f64,
}

/// A path overlay to draw on the map.
#[derive(Debug, Clone)]
pub struct PathOverlay {
    pub points: Vec<GeoPoint>,
    pub stroke_color: Rgba<u8>,
    pub stroke_width: f32,
    pub fill_color: Option<Rgba<u8>>,
}

/// A marker overlay to draw on the map.
#[derive(Debug, Clone)]
pub struct MarkerOverlay {
    pub position: GeoPoint,
    pub color: Rgba<u8>,
    pub size: f32,
}

/// Untyped overlay item — useful for adapters that bundle different
/// kinds of overlays in the same request payload.
#[derive(Debug, Clone)]
pub enum OverlayItem {
    Path(PathOverlay),
    Marker(MarkerOverlay),
    /// Inline GeoJSON `Feature` / `FeatureCollection` — expanded into
    /// path/marker primitives by [`expand_overlays`] before drawing.
    GeoJson(String),
}

/// Decode a Google Encoded Polyline string into a vector of GeoPoints.
///
/// The Google Polyline Algorithm encodes coordinates as a series of ASCII
/// characters. See:
/// <https://developers.google.com/maps/documentation/utilities/polylinealgorithm>.
///
/// Note: Google's format is `(lat, lon)` but we return `GeoPoint` with
/// `(lon, lat)` for GeoJSON compatibility. Invalid, truncated or overflowing
/// input returns the complete points decoded before the invalid pair.
#[must_use]
pub fn decode_polyline(encoded: &str) -> Vec<GeoPoint> {
    // Pre-size: each (dlat, dlng) pair uses ≈ 6–8 bytes of input. Floor
    // division gives a safe lower bound that avoids the first realloc
    // for typical inputs without over-allocating for short ones.
    let mut points = Vec::with_capacity(encoded.len() / 8);
    let mut bytes = encoded.bytes();
    let mut lat = 0i64;
    let mut lng = 0i64;
    while let (Some(dlat), Some(dlng)) = (
        decode_polyline_value(&mut bytes),
        decode_polyline_value(&mut bytes),
    ) {
        let (Some(next_lat), Some(next_lng)) = (lat.checked_add(dlat), lng.checked_add(dlng))
        else {
            break;
        };
        lat = next_lat;
        lng = next_lng;
        points.push(GeoPoint {
            lon: lng as f64 / 1e5,
            lat: lat as f64 / 1e5,
        });
    }
    points
}

fn decode_polyline_value(bytes: &mut impl Iterator<Item = u8>) -> Option<i64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(5) {
        let byte = bytes.next()?;
        if !(63..=126).contains(&byte) {
            return None;
        }
        let chunk = u64::from(byte - 63);
        let payload = chunk & 0x1f;
        if payload > (u64::MAX >> shift) {
            return None;
        }
        value |= payload << shift;
        if chunk < 0x20 {
            return Some((value >> 1) as i64 ^ -((value & 1) as i64));
        }
    }
    None
}

#[cfg(test)]
fn encode_polyline(points: &[GeoPoint]) -> String {
    let mut encoded = String::new();
    let mut prev_lat = 0i64;
    let mut prev_lng = 0i64;
    for point in points {
        let lat = (point.lat * 1e5).round() as i64;
        let lng = (point.lon * 1e5).round() as i64;
        encode_value(lat - prev_lat, &mut encoded);
        encode_value(lng - prev_lng, &mut encoded);
        prev_lat = lat;
        prev_lng = lng;
    }
    encoded
}

#[cfg(test)]
fn encode_value(mut value: i64, output: &mut String) {
    value = if value < 0 { !(value << 1) } else { value << 1 };
    while value >= 0x20 {
        output.push(((value & 0x1f) as u8 + 63 + 0x20) as char);
        value >>= 5;
    }
    output.push((value as u8 + 63) as char);
}

fn is_encoded_polyline(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.contains(',') {
        for part in s.split('|') {
            let mut it = part.split(',');
            if let (Some(a), Some(b)) = (it.next(), it.next())
                && a.parse::<f64>().is_ok()
                && b.parse::<f64>().is_ok()
            {
                return false;
            }
        }
    }
    s.chars().all(|c| {
        let code = c as u32;
        (63..=126).contains(&code)
    })
}

fn parse_coord_pair(s: &str, latlng: bool) -> Option<GeoPoint> {
    let mut it = s.split(',');
    let a: f64 = it.next()?.parse().ok()?;
    let b: f64 = it.next()?.parse().ok()?;
    if latlng {
        Some(GeoPoint { lon: b, lat: a })
    } else {
        Some(GeoPoint { lon: a, lat: b })
    }
}

/// Parse a path string into a [`PathOverlay`].
///
/// Format: `path-{strokeWidth}+{strokeColor}-{fillColor}({coordinates})`.
/// Example: `path-5+f00-88f(0,0|10,10|20,0)`. Encoded polyline:
/// `path-5+f00(encodedPolylineString)`. Or just `enc:_p~iF~ps|U_ulLnnqC`.
#[must_use]
pub fn parse_path(path_str: &str, latlng: bool) -> Option<PathOverlay> {
    let mut stroke_width = 3.0f32;
    let mut stroke_color = Rgba([0, 0, 255, 255]);
    let mut fill_color: Option<Rgba<u8>> = None;
    let mut points = Vec::new();

    let path_str = path_str.trim();

    if let Some(encoded) = path_str.strip_prefix("enc:") {
        points = decode_polyline(encoded);
    } else if let Some(rest) = path_str.strip_prefix("path-") {
        if let Some((style_part, coords)) = rest.split_once('(') {
            let coords_part = coords.strip_suffix(')')?;

            let mut parts = style_part.split('+');
            if let Some(first) = parts.next() {
                stroke_width = first.parse().unwrap_or(3.0);
            }
            if let Some(color_part) = parts.next() {
                let mut color_parts = color_part.split('-');
                if let Some(stroke_hex) = color_parts.next() {
                    stroke_color = parse_hex_color(stroke_hex).unwrap_or(stroke_color);
                }
                if let Some(fill_hex) = color_parts.next() {
                    fill_color = parse_hex_color(fill_hex);
                }
            }

            if is_encoded_polyline(coords_part) {
                points = decode_polyline(coords_part);
            } else {
                for coord in coords_part.split('|') {
                    if let Some(p) = parse_coord_pair(coord, latlng) {
                        points.push(p);
                    }
                }
            }
        }
    } else if is_encoded_polyline(path_str) {
        points = decode_polyline(path_str);
    } else {
        for coord in path_str.split('|') {
            if let Some(p) = parse_coord_pair(coord, latlng) {
                points.push(p);
            }
        }
    }

    if points.len() >= 2 {
        Some(PathOverlay {
            points,
            stroke_color,
            stroke_width,
            fill_color,
        })
    } else {
        None
    }
}

/// Parse a marker string into a [`MarkerOverlay`].
///
/// Format: `pin-{size}-{label}+{color}({lon},{lat})`. Or simple
/// `{lon},{lat}`.
#[must_use]
pub fn parse_marker(marker_str: &str) -> Option<MarkerOverlay> {
    let marker_str = marker_str.trim();

    let mut color = Rgba([255, 0, 0, 255]);
    let mut size = 24.0f32;

    if let Some(rest) = marker_str.strip_prefix("pin-") {
        if let Some((style_part, coords)) = rest.split_once('(') {
            let coords_part = coords.strip_suffix(')')?;

            let mut parts = style_part.split('+');
            if let Some(size_label_part) = parts.next() {
                let size_label = size_label_part.split('-').next().unwrap_or("");
                size = match size_label {
                    "s" => 20.0,
                    "m" => 28.0,
                    "l" => 36.0,
                    _ => 24.0,
                };
            }
            if let Some(color_part) = parts.next() {
                color = parse_hex_color(color_part).unwrap_or(color);
            }

            let mut xy = coords_part.split(',');
            if let (Some(x), Some(y)) = (xy.next(), xy.next())
                && let (Ok(lon), Ok(lat)) = (x.parse(), y.parse())
            {
                return Some(MarkerOverlay {
                    position: GeoPoint { lon, lat },
                    color,
                    size,
                });
            }
        }
    } else {
        let mut xy = marker_str.split(',');
        if let (Some(x), Some(y)) = (xy.next(), xy.next())
            && let (Ok(lon), Ok(lat)) = (x.parse(), y.parse())
        {
            return Some(MarkerOverlay {
                position: GeoPoint { lon, lat },
                color,
                size,
            });
        }
    }

    None
}

/// Parse a hex color string (3, 4, 6, or 8 digits) with optional `#` prefix.
fn parse_hex_color(hex: &str) -> Option<Rgba<u8>> {
    let hex = hex.trim_start_matches('#');
    if !hex.is_ascii() {
        return None;
    }
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some(Rgba([r, g, b, 255]))
        }
        4 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            let a = u8::from_str_radix(&hex[3..4], 16).ok()? * 17;
            Some(Rgba([r, g, b, a]))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Rgba([r, g, b, 255]))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Rgba([r, g, b, a]))
        }
        _ => None,
    }
}

/// Expand a heterogeneous `[OverlayItem]` slice into separate path and
/// marker buckets ready for [`draw_overlays`].
#[must_use]
pub fn expand_overlays(items: &[OverlayItem]) -> (Vec<PathOverlay>, Vec<MarkerOverlay>) {
    let mut paths = Vec::new();
    let mut markers = Vec::new();
    for item in items {
        match item {
            OverlayItem::Path(p) => paths.push(p.clone()),
            OverlayItem::Marker(m) => markers.push(m.clone()),
            OverlayItem::GeoJson(s) => {
                let (gp, gm) = parse_geojson(s);
                paths.extend(gp);
                markers.extend(gm);
            }
        }
    }
    (paths, markers)
}

/// Calculate `(min_lon, min_lat, max_lon, max_lat)` from any combination
/// of paths and markers. Returns `None` if both slices are empty.
#[must_use]
pub fn calculate_bounds(
    paths: &[PathOverlay],
    markers: &[MarkerOverlay],
) -> Option<(f64, f64, f64, f64)> {
    let mut min_lon = f64::MAX;
    let mut min_lat = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut max_lat = f64::MIN;
    let mut has_points = false;

    for path in paths {
        for point in &path.points {
            min_lon = min_lon.min(point.lon);
            min_lat = min_lat.min(point.lat);
            max_lon = max_lon.max(point.lon);
            max_lat = max_lat.max(point.lat);
            has_points = true;
        }
    }
    for marker in markers {
        min_lon = min_lon.min(marker.position.lon);
        min_lat = min_lat.min(marker.position.lat);
        max_lon = max_lon.max(marker.position.lon);
        max_lat = max_lat.max(marker.position.lat);
        has_points = true;
    }

    if has_points {
        Some((min_lon, min_lat, max_lon, max_lat))
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
