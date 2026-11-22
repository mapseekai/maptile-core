//! Ported from `tileserver-rs` (`crates/tileserver-rs/src/render/overlay.rs`)
//! under the MIT license; see `third_party/tileserver-rs/LICENSE`.

use super::{GeoPoint, MarkerOverlay, PathOverlay};
use crate::render::types::{RenderRequest, mercator_y};
use image::{Rgba, RgbaImage};

fn geo_to_pixel(
    point: &GeoPoint,
    center_lon: f64,
    center_lat: f64,
    zoom: f64,
    width: u32,
    height: u32,
    scale: f32,
) -> (f32, f32) {
    let tile_size = f64::from(RenderRequest::TILE_SIZE) * f64::from(scale);
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

#[cfg(test)]
mod tests {
    use super::clip_line;

    #[test]
    fn clips_long_segments_to_the_viewport_before_rasterization() {
        let bounds = [-2.0, -2.0, 514.0, 514.0];
        let line = clip_line([-30_000_000.0, 256.0, 30_000_000.0, 256.0], bounds).unwrap();
        assert_eq!(line, [-2.0, 256.0, 514.0, 256.0]);
        assert!(clip_line([-30_000_000.0, -8.0, 30_000_000.0, -8.0], bounds).is_none());
    }
}
