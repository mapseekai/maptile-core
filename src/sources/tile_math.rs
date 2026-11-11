//! Helpers for slippy-tile coordinate math safe for the full u8 zoom range.

use crate::error::MaptileError;

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

/// `(west, south, east, north)` WGS84 bbox of a slippy tile.
#[cfg(any(test, feature = "duckdb", feature = "geoparquet"))]
pub(super) fn tile_envelope_4326(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
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
