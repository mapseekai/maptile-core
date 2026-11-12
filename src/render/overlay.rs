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

use image::{Rgba, RgbaImage};
use serde_json::Value;

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
/// `(lon, lat)` for GeoJSON compatibility.
#[must_use]
pub fn decode_polyline(encoded: &str) -> Vec<GeoPoint> {
    // Pre-size: each (dlat, dlng) pair uses ≈ 6–8 bytes of input. Floor
    // division gives a safe lower bound that avoids the first realloc
    // for typical inputs without over-allocating for short ones.
    let mut points = Vec::with_capacity(encoded.len() / 8);
    let mut index = 0;
    let mut lat = 0i64;
    let mut lng = 0i64;

    let bytes = encoded.as_bytes();

    while index < bytes.len() {
        let mut shift = 0;
        let mut result = 0i64;
        loop {
            if index >= bytes.len() {
                return points;
            }
            let b = (bytes[index] as i64) - 63;
            index += 1;
            result |= (b & 0x1f) << shift;
            shift += 5;
            if b < 0x20 {
                break;
            }
        }
        let dlat = if (result & 1) != 0 {
            !(result >> 1)
        } else {
            result >> 1
        };
        lat += dlat;

        shift = 0;
        result = 0;
        loop {
            if index >= bytes.len() {
                return points;
            }
            let b = (bytes[index] as i64) - 63;
            index += 1;
            result |= (b & 0x1f) << shift;
            shift += 5;
            if b < 0x20 {
                break;
            }
        }
        let dlng = if (result & 1) != 0 {
            !(result >> 1)
        } else {
            result >> 1
        };
        lng += dlng;

        points.push(GeoPoint {
            lon: lng as f64 / 1e5,
            lat: lat as f64 / 1e5,
        });
    }

    points
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
        if let Some(paren_idx) = rest.find('(') {
            let style_part = &rest[..paren_idx];
            let coords_part = &rest[paren_idx + 1..rest.len() - 1];

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

    if marker_str.starts_with("pin-") {
        if let Some(paren_idx) = marker_str.find('(') {
            let style_part = &marker_str[4..paren_idx];
            let coords_part = &marker_str[paren_idx + 1..marker_str.len() - 1];

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

fn geo_to_pixel(
    point: &GeoPoint,
    center_lon: f64,
    center_lat: f64,
    zoom: f64,
    width: u32,
    height: u32,
    scale: f32,
) -> (f32, f32) {
    use super::types::{TILE_SIZE, mercator_y};

    let tile_size = f64::from(TILE_SIZE) * f64::from(scale);
    let scale_factor = tile_size * 2.0_f64.powf(zoom) / 360.0;

    let dx = (point.lon - center_lon) * scale_factor;

    let center_y = mercator_y(center_lat);
    let point_y = mercator_y(point.lat);
    let dy = -(point_y - center_y) * scale_factor * 180.0 / std::f64::consts::PI;

    let px = (width as f64 / 2.0 + dx) as f32;
    let py = (height as f64 / 2.0 + dy) as f32;

    (px, py)
}

/// Draw paths and markers on a previously-rendered RGBA image.
pub fn draw_overlays(
    image: &mut RgbaImage,
    paths: &[PathOverlay],
    markers: &[MarkerOverlay],
    center_lon: f64,
    center_lat: f64,
    zoom: f64,
    scale: f32,
) {
    let width = image.width();
    let height = image.height();

    for path in paths {
        draw_path(
            image, path, center_lon, center_lat, zoom, width, height, scale,
        );
    }

    for marker in markers {
        draw_marker(
            image, marker, center_lon, center_lat, zoom, width, height, scale,
        );
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

#[allow(clippy::too_many_arguments)]
fn draw_path(
    image: &mut RgbaImage,
    path: &PathOverlay,
    center_lon: f64,
    center_lat: f64,
    zoom: f64,
    width: u32,
    height: u32,
    scale: f32,
) {
    if path.points.len() < 2 {
        return;
    }

    let pixels: Vec<(f32, f32)> = path
        .points
        .iter()
        .map(|p| geo_to_pixel(p, center_lon, center_lat, zoom, width, height, scale))
        .collect();

    if let Some(fill) = path.fill_color
        && pixels.iter().all(|(x, y)| x.is_finite() && y.is_finite())
    {
        fill_polygon(image, &pixels, fill);
    }

    if !path.stroke_width.is_finite() || path.stroke_width <= 0.0 {
        return;
    }
    let stroke_width = path.stroke_width.min(32.0) * scale;
    for pair in pixels.windows(2) {
        draw_line(
            image,
            pair[0].0,
            pair[0].1,
            pair[1].0,
            pair[1].1,
            path.stroke_color,
            stroke_width,
        );
    }
}

fn draw_line(
    image: &mut RgbaImage,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    color: Rgba<u8>,
    thickness: f32,
) {
    let half_thick = thickness / 2.0;
    let margin = f64::from(half_thick.ceil()) + 1.0;
    let Some([x0, y0, x1, y1]) = clip_line(
        [x0, y0, x1, y1],
        [
            -margin,
            -margin,
            f64::from(image.width()) + margin,
            f64::from(image.height()) + margin,
        ],
    ) else {
        return;
    };
    let dx = x1 - x0;
    let dy = y1 - y0;
    let length = (dx * dx + dy * dy).sqrt();
    if length < 0.5 {
        return;
    }

    let steps = length.ceil() as i32;

    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let cx = x0 + dx * t;
        let cy = y0 + dy * t;

        for ox in (-half_thick.ceil() as i32)..=(half_thick.ceil() as i32) {
            for oy in (-half_thick.ceil() as i32)..=(half_thick.ceil() as i32) {
                let dist = ((ox * ox + oy * oy) as f32).sqrt();
                if dist <= half_thick {
                    let px = (cx + ox as f32) as i32;
                    let py = (cy + oy as f32) as i32;
                    if px >= 0 && py >= 0 && px < image.width() as i32 && py < image.height() as i32
                    {
                        blend_pixel(image, px as u32, py as u32, color);
                    }
                }
            }
        }
    }
}

fn clip_line(line: [f32; 4], bounds: [f64; 4]) -> Option<[f32; 4]> {
    if !line.iter().all(|coordinate| coordinate.is_finite()) {
        return None;
    }
    let [x0, y0, x1, y1] = line.map(f64::from);
    let [left, top, right, bottom] = bounds;
    let (dx, dy) = (x1 - x0, y1 - y0);
    let (mut start, mut end) = (0.0_f64, 1.0_f64);
    for (direction, distance) in [
        (-dx, x0 - left),
        (dx, right - x0),
        (-dy, y0 - top),
        (dy, bottom - y0),
    ] {
        if direction == 0.0 {
            if distance < 0.0 {
                return None;
            }
        } else if direction < 0.0 {
            start = start.max(distance / direction);
        } else {
            end = end.min(distance / direction);
        }
    }
    (start <= end).then_some([
        (x0 + start * dx) as f32,
        (y0 + start * dy) as f32,
        (x0 + end * dx) as f32,
        (y0 + end * dy) as f32,
    ])
}

fn fill_polygon(image: &mut RgbaImage, pixels: &[(f32, f32)], color: Rgba<u8>) {
    if pixels.len() < 3 {
        return;
    }

    let mut poly = pixels.to_vec();
    let first = poly[0];
    let Some(&last) = poly.last() else { return };
    if (first.0 - last.0).abs() > 0.5 || (first.1 - last.1).abs() > 0.5 {
        poly.push(first);
    }

    let min_y = poly.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
    let max_y = poly.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
    let img_h = image.height() as i32;
    let img_w = image.width() as i32;
    let scan_min = (min_y.floor() as i32).max(0);
    let scan_max = (max_y.ceil() as i32).min(img_h - 1);

    let n = poly.len();
    for y in scan_min..=scan_max {
        let yf = y as f32 + 0.5;
        let mut xs: Vec<f32> = Vec::new();

        for i in 0..n - 1 {
            let (x0, y0) = poly[i];
            let (x1, y1) = poly[i + 1];
            if (y1 - y0).abs() < f32::EPSILON {
                continue;
            }
            let y_lo = y0.min(y1);
            let y_hi = y0.max(y1);
            if yf < y_lo || yf >= y_hi {
                continue;
            }
            let t = (yf - y0) / (y1 - y0);
            xs.push(x0 + t * (x1 - x0));
        }

        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        for chunk in xs.chunks(2) {
            if chunk.len() < 2 {
                break;
            }
            let x_start = (chunk[0].floor() as i32).max(0);
            let x_end = (chunk[1].ceil() as i32).min(img_w - 1);
            for x in x_start..=x_end {
                blend_pixel(image, x as u32, y as u32, color);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_marker(
    image: &mut RgbaImage,
    marker: &MarkerOverlay,
    center_lon: f64,
    center_lat: f64,
    zoom: f64,
    width: u32,
    height: u32,
    scale: f32,
) {
    let (px, py) = geo_to_pixel(
        &marker.position,
        center_lon,
        center_lat,
        zoom,
        width,
        height,
        scale,
    );

    if !px.is_finite() || !py.is_finite() || !marker.size.is_finite() || marker.size <= 0.0 {
        return;
    }
    let size = marker.size.min(128.0) * scale;
    if px < -size || px > width as f32 + size || py < -size || py > height as f32 + size {
        return;
    }
    let half_size = size / 2.0;

    let circle_radius = half_size * 0.6;
    let circle_cy = py - size * 0.3;

    for ox in (-circle_radius.ceil() as i32)..=(circle_radius.ceil() as i32) {
        for oy in (-circle_radius.ceil() as i32)..=(circle_radius.ceil() as i32) {
            let dist = ((ox * ox + oy * oy) as f32).sqrt();
            if dist <= circle_radius {
                let mx = (px + ox as f32) as i32;
                let my = (circle_cy + oy as f32) as i32;
                if mx >= 0 && my >= 0 && mx < image.width() as i32 && my < image.height() as i32 {
                    blend_pixel(image, mx as u32, my as u32, marker.color);
                }
            }
        }
    }

    let point_y = py;
    let triangle_height = size * 0.4;
    let triangle_width = circle_radius * 0.8;

    for y_offset in 0..=(triangle_height as i32) {
        let progress = y_offset as f32 / triangle_height;
        let width_at_y = triangle_width * (1.0 - progress);

        for x_offset in (-width_at_y.ceil() as i32)..=(width_at_y.ceil() as i32) {
            if (x_offset as f32).abs() <= width_at_y {
                let mx = (px + x_offset as f32) as i32;
                let my = (circle_cy + circle_radius + y_offset as f32) as i32;
                if mx >= 0
                    && my >= 0
                    && mx < image.width() as i32
                    && my < image.height() as i32
                    && my <= point_y as i32
                {
                    blend_pixel(image, mx as u32, my as u32, marker.color);
                }
            }
        }
    }

    let inner_radius = circle_radius * 0.4;
    let white = Rgba([255, 255, 255, 255]);
    for ox in (-inner_radius.ceil() as i32)..=(inner_radius.ceil() as i32) {
        for oy in (-inner_radius.ceil() as i32)..=(inner_radius.ceil() as i32) {
            let dist = ((ox * ox + oy * oy) as f32).sqrt();
            if dist <= inner_radius {
                let mx = (px + ox as f32) as i32;
                let my = (circle_cy + oy as f32) as i32;
                if mx >= 0 && my >= 0 && mx < image.width() as i32 && my < image.height() as i32 {
                    blend_pixel(image, mx as u32, my as u32, white);
                }
            }
        }
    }
}

fn blend_pixel(image: &mut RgbaImage, x: u32, y: u32, color: Rgba<u8>) {
    let existing = image.get_pixel(x, y);
    let alpha = color.0[3] as f32 / 255.0;
    let inv_alpha = 1.0 - alpha;
    let r = (color.0[0] as f32 * alpha + existing.0[0] as f32 * inv_alpha) as u8;
    let g = (color.0[1] as f32 * alpha + existing.0[1] as f32 * inv_alpha) as u8;
    let b = (color.0[2] as f32 * alpha + existing.0[2] as f32 * inv_alpha) as u8;
    let a = ((color.0[3] as f32 + existing.0[3] as f32 * inv_alpha).min(255.0)) as u8;
    image.put_pixel(x, y, Rgba([r, g, b, a]));
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
            tracing::warn!("Failed to parse GeoJSON overlay string");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_long_segments_to_the_viewport_before_rasterization() {
        let bounds = [-2.0, -2.0, 514.0, 514.0];
        let line = clip_line([-30_000_000.0, 256.0, 30_000_000.0, 256.0], bounds).unwrap();
        assert_eq!(line, [-2.0, 256.0, 514.0, 256.0]);
        assert!(clip_line([-30_000_000.0, -8.0, 30_000_000.0, -8.0], bounds).is_none());
    }

    #[test]
    fn parse_hex_color_3_digit() {
        assert_eq!(parse_hex_color("f00"), Some(Rgba([255, 0, 0, 255])));
        assert_eq!(parse_hex_color("abc"), Some(Rgba([170, 187, 204, 255])));
    }

    #[test]
    fn parse_hex_color_6_and_8_digit() {
        assert_eq!(parse_hex_color("ff0000"), Some(Rgba([255, 0, 0, 255])));
        assert_eq!(parse_hex_color("ff000080"), Some(Rgba([255, 0, 0, 128])));
        assert_eq!(parse_hex_color("#aabbcc"), Some(Rgba([170, 187, 204, 255])));
    }

    #[test]
    fn parse_hex_color_invalid_lengths() {
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("ffffff0"), None);
    }

    #[test]
    fn polyline_round_trip_wikipedia_example() {
        // Wikipedia's worked example: 38.5,-120.2 / 40.7,-120.95 / 43.252,-126.453.
        let pts = decode_polyline("_p~iF~ps|U_ulLnnqC_mqNvxq`@");
        assert_eq!(pts.len(), 3);
        assert!((pts[0].lat - 38.5).abs() < 1e-5);
        assert!((pts[0].lon - (-120.2)).abs() < 1e-5);
        assert!((pts[2].lat - 43.252).abs() < 1e-5);
        assert!((pts[2].lon - (-126.453)).abs() < 1e-5);

        // Round-trip through the encoder.
        let re = encode_polyline(&pts);
        assert_eq!(re, "_p~iF~ps|U_ulLnnqC_mqNvxq`@");
    }

    #[test]
    fn parse_path_basic_query() {
        let p = parse_path("path-5+f00(-122.4,37.8|-122.5,37.9)", false).unwrap();
        assert_eq!(p.points.len(), 2);
        assert_eq!(p.stroke_width, 5.0);
        assert_eq!(p.stroke_color, Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn parse_path_with_fill_color() {
        let p = parse_path("path-3+00f-ff0(-10,20|30,40|50,60)", false).unwrap();
        assert_eq!(p.points.len(), 3);
        assert_eq!(p.fill_color, Some(Rgba([255, 255, 0, 255])));
    }

    #[test]
    fn parse_path_single_point_returns_none() {
        assert!(parse_path("path-5+f00(-122.4,37.8)", false).is_none());
        assert!(parse_path("0,0", false).is_none());
    }

    #[test]
    fn parse_marker_pin_sizes() {
        assert_eq!(parse_marker("pin-s+fff(0,0)").unwrap().size, 20.0);
        assert_eq!(parse_marker("pin-m+fff(0,0)").unwrap().size, 28.0);
        assert_eq!(parse_marker("pin-l+fff(0,0)").unwrap().size, 36.0);
        assert_eq!(parse_marker("pin-x+fff(0,0)").unwrap().size, 24.0);
    }

    #[test]
    fn parse_marker_simple_lonlat() {
        let m = parse_marker("-122.4,37.8").unwrap();
        assert!((m.position.lon - (-122.4)).abs() < 1e-6);
        assert!((m.position.lat - 37.8).abs() < 1e-6);
        assert_eq!(m.color, Rgba([255, 0, 0, 255]));
    }

    #[test]
    fn parse_marker_invalid_returns_none() {
        assert!(parse_marker("").is_none());
        assert!(parse_marker("pin-s+f00()").is_none());
        assert!(parse_marker("invalid").is_none());
    }

    #[test]
    fn calculate_bounds_combines_paths_and_markers() {
        let paths = vec![PathOverlay {
            points: vec![
                GeoPoint { lon: 0.0, lat: 0.0 },
                GeoPoint {
                    lon: 10.0,
                    lat: 5.0,
                },
            ],
            stroke_color: Rgba([0, 0, 255, 255]),
            stroke_width: 3.0,
            fill_color: None,
        }];
        let markers = vec![MarkerOverlay {
            position: GeoPoint {
                lon: -1.0,
                lat: 6.0,
            },
            color: Rgba([255, 0, 0, 255]),
            size: 24.0,
        }];
        let (min_lon, min_lat, max_lon, max_lat) = calculate_bounds(&paths, &markers).unwrap();
        assert!((min_lon - (-1.0)).abs() < 1e-9);
        assert!((min_lat - 0.0).abs() < 1e-9);
        assert!((max_lon - 10.0).abs() < 1e-9);
        assert!((max_lat - 6.0).abs() < 1e-9);
    }

    #[test]
    fn calculate_bounds_empty_returns_none() {
        assert!(calculate_bounds(&[], &[]).is_none());
    }

    #[test]
    fn parse_geojson_feature_collection_lines_and_points() {
        let json = r##"{
            "type": "FeatureCollection",
            "features": [
                {"type": "Feature", "properties": {"stroke":"#ff0000","stroke-width":2.0},
                 "geometry": {"type": "LineString", "coordinates": [[0,0],[1,1]]}},
                {"type": "Feature", "properties": {"marker-color":"#00ff00","marker-size":"large"},
                 "geometry": {"type": "Point", "coordinates": [5,5]}}
            ]
        }"##;
        let (paths, markers) = parse_geojson(json);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].stroke_color, Rgba([255, 0, 0, 255]));
        assert_eq!(paths[0].stroke_width, 2.0);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].color, Rgba([0, 255, 0, 255]));
        assert_eq!(markers[0].size, 18.0);
    }

    #[test]
    fn parse_geojson_polygon_outer_ring_only() {
        let json = r#"{"type": "Polygon", "coordinates": [
            [[0,0],[10,0],[10,10],[0,10],[0,0]],
            [[2,2],[8,2],[8,8],[2,8],[2,2]]
        ]}"#;
        let (paths, _) = parse_geojson(json);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].points.len(), 5);
    }

    #[test]
    fn parse_geojson_invalid_json_returns_empty() {
        let (paths, markers) = parse_geojson("not json");
        assert!(paths.is_empty());
        assert!(markers.is_empty());
    }

    #[test]
    fn draw_overlays_paints_path_pixels() {
        // Render a 2-pixel-wide red line from (-1, 0) to (1, 0) on a
        // 10x10 white canvas centered on (0, 0) at zoom 0. The path
        // crosses the image horizontally near y=5.
        let mut img = RgbaImage::from_pixel(10, 10, Rgba([255, 255, 255, 255]));
        let path = PathOverlay {
            points: vec![
                GeoPoint {
                    lon: -1.0,
                    lat: 0.0,
                },
                GeoPoint { lon: 1.0, lat: 0.0 },
            ],
            stroke_color: Rgba([255, 0, 0, 255]),
            stroke_width: 2.0,
            fill_color: None,
        };
        // Zoom high enough that the lon span fills the canvas.
        draw_overlays(&mut img, &[path], &[], 0.0, 0.0, 1.5, 1.0);
        let mut red = 0;
        for px in img.pixels() {
            if px.0[0] == 255 && px.0[1] == 0 && px.0[2] == 0 {
                red += 1;
            }
        }
        assert!(red > 0, "expected at least one red pixel from the stroke");
    }

    #[test]
    fn draw_overlays_makes_transparent_canvas_visible() {
        let mut image = RgbaImage::from_pixel(32, 32, Rgba([0, 0, 0, 0]));
        let marker = MarkerOverlay {
            position: GeoPoint { lon: 0.0, lat: 0.0 },
            color: Rgba([255, 0, 0, 255]),
            size: 12.0,
        };

        draw_overlays(&mut image, &[], &[marker], 0.0, 0.0, 0.0, 1.0);

        assert!(image.pixels().any(|pixel| pixel.0[3] != 0));
    }

    #[test]
    fn expand_overlays_routes_each_kind() {
        let items = vec![
            OverlayItem::Path(PathOverlay {
                points: vec![
                    GeoPoint { lon: 0.0, lat: 0.0 },
                    GeoPoint { lon: 1.0, lat: 1.0 },
                ],
                stroke_color: Rgba([0, 0, 255, 255]),
                stroke_width: 1.0,
                fill_color: None,
            }),
            OverlayItem::Marker(MarkerOverlay {
                position: GeoPoint { lon: 2.0, lat: 2.0 },
                color: Rgba([255, 0, 0, 255]),
                size: 24.0,
            }),
            OverlayItem::GeoJson(
                r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[3,3]}}"#.into(),
            ),
        ];
        let (paths, markers) = expand_overlays(&items);
        assert_eq!(paths.len(), 1);
        assert_eq!(markers.len(), 2);
    }
}
