//! Bridge between [`RasterImage`] and raw RGBA8 byte buffers — the wire
//! format that [`crate::render::encode::encode_rgba`] consumes.

use crate::error::MaptileResult;
use crate::raster::image::RasterImage;

/// Flatten a [`RasterImage`] into a `width * height * 4` RGBA byte buffer.
///
/// - 1-band rasters are broadcast: `R = G = B = value`,
/// - 3-band rasters use channels 0..3 for RGB and synthesise alpha = 255,
/// - 4-band rasters use the raster's RGBA channels directly.
///
/// In all cases, pixels with `mask == false` get `alpha = 0` so they can
/// be drawn transparently regardless of their stored value. Values are
/// clamped to `0..=255` and NaN is treated as 0.
#[must_use]
pub fn rgba_from_raster(img: &RasterImage) -> Vec<u8> {
    let (b, h, w) = img.shape();
    let pixel_count = h * w;
    let mut out = vec![0u8; pixel_count * 4];

    // Fast path: ndarray Array3<f32> built via `Array3::zeros((b, h, w))` is
    // C-contiguous, so we can iterate via `.as_slice()` and skip per-pixel
    // tuple-index bounds checks. Fall through to the generic indexed path
    // if the array is not standard layout (e.g. constructed via a non-
    // contiguous view).
    let data_slice = img.data.as_slice();
    let mask_slice = img.mask.as_slice();
    if let (Some(data), Some(mask)) = (data_slice, mask_slice) {
        debug_assert_eq!(data.len(), b * pixel_count);
        debug_assert_eq!(mask.len(), pixel_count);
        // Pull each band slice (length pixel_count) out once.
        let band0 = &data[..pixel_count];
        match b {
            1 => fill_rgba_1band(&mut out, band0, mask),
            3 => fill_rgba_3band(
                &mut out,
                band0,
                &data[pixel_count..2 * pixel_count],
                &data[2 * pixel_count..3 * pixel_count],
                mask,
            ),
            n if n >= 4 => fill_rgba_4band(
                &mut out,
                band0,
                &data[pixel_count..2 * pixel_count],
                &data[2 * pixel_count..3 * pixel_count],
                &data[3 * pixel_count..4 * pixel_count],
                mask,
            ),
            _ => {} // already zeroed
        }
        return out;
    }

    // Slow fallback for non-standard ndarray layouts.
    let mut o = 0;
    for y in 0..h {
        for x in 0..w {
            let valid = img.mask[(y, x)];
            let (r, g, bl, a) = match b {
                1 => {
                    let v = clamp_to_u8(img.data[(0, y, x)]);
                    (v, v, v, if valid { 255 } else { 0 })
                }
                3 => {
                    let r = clamp_to_u8(img.data[(0, y, x)]);
                    let g = clamp_to_u8(img.data[(1, y, x)]);
                    let bl = clamp_to_u8(img.data[(2, y, x)]);
                    (r, g, bl, if valid { 255 } else { 0 })
                }
                n if n >= 4 => {
                    let r = clamp_to_u8(img.data[(0, y, x)]);
                    let g = clamp_to_u8(img.data[(1, y, x)]);
                    let bl = clamp_to_u8(img.data[(2, y, x)]);
                    let a = if valid {
                        clamp_to_u8(img.data[(3, y, x)])
                    } else {
                        0
                    };
                    (r, g, bl, a)
                }
                _ => (0, 0, 0, 0),
            };
            out[o..o + 4].copy_from_slice(&[r, g, bl, a]);
            o += 4;
        }
    }
    out
}

#[inline]
fn fill_rgba_1band(out: &mut [u8], band: &[f32], mask: &[bool]) {
    for ((px, &v), &valid) in out.as_chunks_mut::<4>().0.iter_mut().zip(band).zip(mask) {
        let g = clamp_to_u8(v);
        px[0] = g;
        px[1] = g;
        px[2] = g;
        px[3] = if valid { 255 } else { 0 };
    }
}

#[inline]
fn fill_rgba_3band(out: &mut [u8], r: &[f32], g: &[f32], b: &[f32], mask: &[bool]) {
    for (((px, &rv), &gv), (&bv, &valid)) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(r)
        .zip(g)
        .zip(b.iter().zip(mask))
    {
        px[0] = clamp_to_u8(rv);
        px[1] = clamp_to_u8(gv);
        px[2] = clamp_to_u8(bv);
        px[3] = if valid { 255 } else { 0 };
    }
}

#[inline]
fn fill_rgba_4band(out: &mut [u8], r: &[f32], g: &[f32], b: &[f32], a: &[f32], mask: &[bool]) {
    for (((px, &rv), &gv), ((&bv, &av), &valid)) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(r)
        .zip(g)
        .zip(b.iter().zip(a).zip(mask))
    {
        px[0] = clamp_to_u8(rv);
        px[1] = clamp_to_u8(gv);
        px[2] = clamp_to_u8(bv);
        px[3] = if valid { clamp_to_u8(av) } else { 0 };
    }
}

/// Build a 4-band RGBA `RasterImage` from a flat byte buffer (delegates to
/// [`RasterImage::from_rgba8`]).
pub fn raster_from_rgba(rgba: &[u8], width: u32, height: u32) -> MaptileResult<RasterImage> {
    RasterImage::from_rgba8(rgba, width, height)
}

#[inline]
pub(super) fn clamp_to_u8(v: f32) -> u8 {
    if v.is_nan() || v <= 0.0 {
        0
    } else if v >= 255.0 {
        255
    } else {
        v as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::{Array2, Array3};

    fn make_single_band(value: f32, w: usize, h: usize) -> RasterImage {
        let mut data = Array3::<f32>::zeros((1, h, w));
        for y in 0..h {
            for x in 0..w {
                data[(0, y, x)] = value;
            }
        }
        RasterImage {
            data,
            mask: Array2::<bool>::from_elem((h, w), true),
            nodata: None,
            bounds: None,
            crs: None,
        }
    }

    #[test]
    fn rgba_from_single_band_broadcasts_to_grayscale() {
        let img = make_single_band(42.0, 2, 2);
        let rgba = rgba_from_raster(&img);
        assert_eq!(rgba.len(), 16);
        for px in rgba.as_chunks::<4>().0 {
            assert_eq!(px[0], 42);
            assert_eq!(px[1], 42);
            assert_eq!(px[2], 42);
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn rgba_mask_false_yields_alpha_zero() {
        let mut img = make_single_band(100.0, 2, 2);
        img.mask[(0, 0)] = false;
        let rgba = rgba_from_raster(&img);
        // Pixel (0,0) → alpha 0, others → alpha 255.
        assert_eq!(rgba[3], 0);
        assert_eq!(rgba[7], 255);
        assert_eq!(rgba[11], 255);
        assert_eq!(rgba[15], 255);
    }

    #[test]
    fn rgba_round_trips_through_raster_from_rgba() {
        let original = vec![
            10, 20, 30, 255, 40, 50, 60, 200, 70, 80, 90, 128, 1, 2, 3, 4,
        ];
        let img = raster_from_rgba(&original, 2, 2).unwrap();
        let back = rgba_from_raster(&img);
        assert_eq!(back, original);
    }

    #[test]
    fn rgba_clamps_out_of_range_values() {
        let mut img = make_single_band(0.0, 2, 1);
        img.data[(0, 0, 0)] = -50.0;
        img.data[(0, 0, 1)] = 500.0;
        let rgba = rgba_from_raster(&img);
        assert_eq!(rgba[0], 0);
        assert_eq!(rgba[4], 255);
    }

    #[test]
    fn rgba_handles_nan_as_zero() {
        let mut img = make_single_band(0.0, 1, 1);
        img.data[(0, 0, 0)] = f32::NAN;
        let rgba = rgba_from_raster(&img);
        assert_eq!(rgba[0], 0);
    }
}
