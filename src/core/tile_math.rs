//! Helpers for slippy-tile coordinate math safe for the full u8 zoom range.

// Adapted from MapLibre Martin's martin-tile-utils 0.7.8 (MIT OR Apache-2.0).
// See third_party/martin for both licenses.
// Portions originate in mbtileserver-rs by Kaveh Karimi (MIT OR Apache-2.0).

use crate::error::MaptileError;
use std::f64::consts::PI;

const EARTH_CIRCUMFERENCE: f64 = 40_075_016.685_578_5;
const EARTH_RADIUS: f64 = EARTH_CIRCUMFERENCE / 2.0 / PI;
/// Maximum web-mercator zoom level we support before tile counts no
/// longer fit in `u32`. (`2^30` ≈ 1 billion tiles per axis.)
pub const MAX_ZOOM: u8 = 30;

/// `2^z` as `u32`, returning `Err` for zooms that would overflow.
#[inline]
pub fn tiles_per_axis(z: u8) -> Result<u32, MaptileError> {
    if z > MAX_ZOOM {
        Err(MaptileError::Config(format!(
            "zoom level {z} exceeds MAX_ZOOM ({MAX_ZOOM})"
        )))
    } else {
        Ok(1u32 << z)
    }
}

/// Convert longitude and latitude to a tile (x,y) coordinates for a given zoom
#[must_use]
#[expect(clippy::cast_possible_truncation)]
#[expect(clippy::cast_sign_loss)]
pub fn tile_index(lng: f64, lat: f64, zoom: u8) -> (u32, u32) {
    let tile_size = EARTH_CIRCUMFERENCE / f64::from(1_u32 << zoom);
    let (x, y) = wgs84_to_webmercator(lng, lat);
    let col = ((EARTH_CIRCUMFERENCE.mul_add(0.5, x).abs() / tile_size) as u32).min((1 << zoom) - 1);
    let row =
        ((EARTH_CIRCUMFERENCE.mul_add(0.5, -y).abs() / tile_size) as u32).min((1 << zoom) - 1);
    (col, row)
}

/// Convert min/max XYZ tile coordinates to a bounding box values.
///
/// The result is `[min_lng, min_lat, max_lng, max_lat]`
///
/// # Panics
/// Panics if `zoom` is greater than [`MAX_ZOOM`].
#[must_use]
pub fn xyz_to_bbox(zoom: u8, min_x: u32, min_y: u32, max_x: u32, max_y: u32) -> [f64; 4] {
    assert!(zoom <= MAX_ZOOM, "zoom {zoom} must be <= {MAX_ZOOM}");

    let tile_length = EARTH_CIRCUMFERENCE / f64::from(1_u32 << zoom);

    let left_down_bbox = tile_bbox(min_x, max_y, tile_length);
    let right_top_bbox = tile_bbox(max_x, min_y, tile_length);

    let (min_lng, min_lat) = webmercator_to_wgs84(left_down_bbox[0], left_down_bbox[1]);
    let (max_lng, max_lat) = webmercator_to_wgs84(right_top_bbox[2], right_top_bbox[3]);
    [min_lng, min_lat, max_lng, max_lat]
}

#[expect(clippy::cast_lossless)]
#[must_use]
fn tile_bbox(x: u32, y: u32, tile_length: f64) -> [f64; 4] {
    let min_x = (x as f64).mul_add(tile_length, EARTH_CIRCUMFERENCE * -0.5);
    let max_y = (y as f64).mul_add(-tile_length, EARTH_CIRCUMFERENCE * 0.5);

    [min_x, max_y - tile_length, min_x + tile_length, max_y]
}

/// Convert bounding box to a tile box `(min_x, min_y, max_x, max_y)` for a given zoom
#[must_use]
pub fn bbox_to_xyz(left: f64, bottom: f64, right: f64, top: f64, zoom: u8) -> (u32, u32, u32, u32) {
    let (min_col, min_row) = tile_index(left, top, zoom);
    let (max_col, max_row) = tile_index(right, bottom, zoom);
    (min_col, min_row, max_col, max_row)
}

/// Compute precision of a zoom level, i.e. how many decimal digits of the longitude and latitude are relevant
#[must_use]
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn get_zoom_precision(zoom: u8) -> usize {
    assert!(zoom <= MAX_ZOOM, "zoom {zoom} must be <= {MAX_ZOOM}");
    let lng_delta = webmercator_to_wgs84(EARTH_CIRCUMFERENCE / f64::from(1_u32 << zoom), 0.0).0;
    let log = lng_delta.log10() - 0.5;
    if log > 0.0 { 0 } else { -log.ceil() as usize }
}

/// transform [`WebMercator`](https://epsg.io/3857) to [WGS84](https://epsg.io/4326)
// from https://github.com/Esri/arcgis-osm-editor/blob/e4b9905c264aa22f8eeb657efd52b12cdebea69a/src/OSMWeb10_1/Utils/WebMercator.cs
#[must_use]
pub fn webmercator_to_wgs84(x: f64, y: f64) -> (f64, f64) {
    let lng = (x / EARTH_RADIUS).to_degrees();
    let lat = f64::atan(f64::sinh(y / EARTH_RADIUS)).to_degrees();
    (lng, lat)
}

/// transform [WGS84](https://epsg.io/4326) to [`WebMercator`](https://epsg.io/3857)
// from https://github.com/Esri/arcgis-osm-editor/blob/e4b9905c264aa22f8eeb657efd52b12cdebea69a/src/OSMWeb10_1/Utils/WebMercator.cs
#[must_use]
pub fn wgs84_to_webmercator(lon: f64, lat: f64) -> (f64, f64) {
    let x = lon.to_radians() * EARTH_RADIUS;

    let y_sin = lat.to_radians().sin();
    let y = EARTH_RADIUS / 2.0 * ((1.0 + y_sin) / (1.0 - y_sin)).ln();

    (x, y)
}

/// `(west, south, east, north)` WGS84 bbox of a slippy tile.
#[cfg(test)]
pub(crate) fn tile_envelope_4326(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
    let n = 2_f64.powi(z as i32);
    let west = (x as f64) / n * 360.0 - 180.0;
    let east = ((x as f64) + 1.0) / n * 360.0 - 180.0;
    let north = ((1.0 - 2.0 * (y as f64) / n) * std::f64::consts::PI)
        .sinh()
        .atan()
        .to_degrees();
    let south = ((1.0 - 2.0 * ((y as f64) + 1.0) / n) * std::f64::consts::PI)
        .sinh()
        .atan()
        .to_degrees();
    (west, south, east, north)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn z0_is_one() {
        assert_eq!(tiles_per_axis(0).unwrap(), 1);
    }

    #[test]
    fn z30_fits_u32() {
        assert_eq!(tiles_per_axis(30).unwrap(), 1u32 << 30);
    }

    #[test]
    fn z31_errors() {
        assert!(tiles_per_axis(31).is_err());
    }

    #[test]
    fn z255_errors() {
        assert!(tiles_per_axis(255).is_err());
    }

    #[test]
    fn envelopes_preserve_world_edges_and_adjacent_boundaries() {
        for z in [0, 1, 18, MAX_ZOOM] {
            let last = tiles_per_axis(z).unwrap() - 1;
            let northwest = tile_envelope_4326(z, 0, 0);
            let southeast = tile_envelope_4326(z, last, last);
            assert_eq!(northwest.0, -180.0);
            assert_eq!(southeast.2, 180.0);
            assert!((northwest.3 - 85.051_128_779_806_6).abs() < 1e-12);
            assert!((southeast.1 + 85.051_128_779_806_6).abs() < 1e-12);
            assert!(northwest.0 < northwest.2 && northwest.1 < northwest.3);
            assert!(southeast.0 < southeast.2 && southeast.1 < southeast.3);
            if z > 0 {
                assert_eq!(northwest.2, tile_envelope_4326(z, 1, 0).0);
                assert_eq!(northwest.1, tile_envelope_4326(z, 0, 1).3);
            }
        }
    }
}
