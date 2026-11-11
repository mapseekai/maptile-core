//! Encode a [`RasterImage`] (1, 3, or 4 bands) to PNG, JPEG, or WebP.
//!
//! Sister to [`crate::raster::decode`]. Single-band rasters encode as
//! 8-bit grayscale; 3-band rasters encode as RGB; 4-band as RGBA. The
//! `mask` field is folded into the alpha channel for PNG/WebP output.
//! JPEG output drops the alpha channel (JPEG has no transparency).

use crate::error::{MaptileError, MaptileResult};
use crate::raster::convert::clamp_to_u8;
use crate::raster::image::RasterImage;
use bytes::Bytes;
use std::io::Cursor;

/// Encode to PNG.
///
/// **Blocking**: image encode is CPU-bound. From an async context use
/// [`to_png_async`] (or wrap a call in `tokio::task::spawn_blocking`)
/// so the runtime is not stalled.
pub fn to_png(img: &RasterImage) -> MaptileResult<Bytes> {
    let dyn_img = raster_to_dynamic(img, /*drop_alpha*/ false)?;
    let mut buf = Vec::new();
    dyn_img
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| MaptileError::Render(format!("encode png: {e}")))?;
    Ok(Bytes::from(buf))
}

/// Encode to JPEG. `quality` must be in `1..=100`. JPEG has no alpha so
/// the output is always 3-band RGB.
///
/// **Blocking**: see [`to_png`] note.
pub fn to_jpeg(img: &RasterImage, quality: u8) -> MaptileResult<Bytes> {
    if !(1..=100).contains(&quality) {
        return Err(MaptileError::Render(format!(
            "jpeg quality must be in 1..=100, got {quality}"
        )));
    }
    let dyn_img = raster_to_dynamic(img, /*drop_alpha*/ true)?;
    let rgb = dyn_img.to_rgb8();
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    rgb.write_with_encoder(encoder)
        .map_err(|e| MaptileError::Render(format!("encode jpeg: {e}")))?;
    Ok(Bytes::from(buf))
}

/// Encode to WebP (lossless via the `image` crate).
///
/// **Blocking**: see [`to_png`] note.
pub fn to_webp(img: &RasterImage) -> MaptileResult<Bytes> {
    let dyn_img = raster_to_dynamic(img, /*drop_alpha*/ false)?;
    let mut buf = Vec::new();
    dyn_img
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::WebP)
        .map_err(|e| MaptileError::Render(format!("encode webp: {e}")))?;
    Ok(Bytes::from(buf))
}

/// Async wrapper for [`to_png`] that runs the CPU-bound encode on a
/// `tokio::task::spawn_blocking` thread.
pub async fn to_png_async(img: RasterImage) -> MaptileResult<Bytes> {
    tokio::task::spawn_blocking(move || to_png(&img))
        .await
        .map_err(|e| MaptileError::Render(format!("encode task: {e}")))?
}

/// Async wrapper for [`to_jpeg`].
pub async fn to_jpeg_async(img: RasterImage, quality: u8) -> MaptileResult<Bytes> {
    tokio::task::spawn_blocking(move || to_jpeg(&img, quality))
        .await
        .map_err(|e| MaptileError::Render(format!("encode task: {e}")))?
}

/// Async wrapper for [`to_webp`].
pub async fn to_webp_async(img: RasterImage) -> MaptileResult<Bytes> {
    tokio::task::spawn_blocking(move || to_webp(&img))
        .await
        .map_err(|e| MaptileError::Render(format!("encode task: {e}")))?
}

/// Convert a `RasterImage` into a `DynamicImage` suitable for one of the
/// `image` crate encoders.
///
/// - 1 band → 8-bit grayscale (`ImageLuma8`),
/// - 3 bands → 8-bit RGB (`ImageRgb8`),
/// - 4 bands → 8-bit RGBA (`ImageRgba8`); when `drop_alpha` is true, the
///   alpha channel is omitted.
///
/// Returns `MaptileError::Render` on unsupported band counts.
fn raster_to_dynamic(img: &RasterImage, drop_alpha: bool) -> MaptileResult<image::DynamicImage> {
    let (b, h, w) = img.shape();
    let h_u32 = h as u32;
    let w_u32 = w as u32;
    match b {
        1 => {
            let mut buf = Vec::with_capacity(h * w);
            for y in 0..h {
                for x in 0..w {
                    buf.push(clamp_to_u8(img.data[(0, y, x)]));
                }
            }
            let gray = image::GrayImage::from_raw(w_u32, h_u32, buf).ok_or_else(|| {
                MaptileError::Render("GrayImage::from_raw rejected the buffer".into())
            })?;
            Ok(image::DynamicImage::ImageLuma8(gray))
        }
        3 => {
            let mut buf = Vec::with_capacity(h * w * 3);
            for y in 0..h {
                for x in 0..w {
                    for c in 0..3 {
                        buf.push(clamp_to_u8(img.data[(c, y, x)]));
                    }
                }
            }
            let rgb = image::RgbImage::from_raw(w_u32, h_u32, buf).ok_or_else(|| {
                MaptileError::Render("RgbImage::from_raw rejected the buffer".into())
            })?;
            Ok(image::DynamicImage::ImageRgb8(rgb))
        }
        4 => {
            if drop_alpha {
                let mut buf = Vec::with_capacity(h * w * 3);
                for y in 0..h {
                    for x in 0..w {
                        for c in 0..3 {
                            buf.push(clamp_to_u8(img.data[(c, y, x)]));
                        }
                    }
                }
                let rgb = image::RgbImage::from_raw(w_u32, h_u32, buf).ok_or_else(|| {
                    MaptileError::Render("RgbImage::from_raw rejected the buffer".into())
                })?;
                Ok(image::DynamicImage::ImageRgb8(rgb))
            } else {
                let mut buf = Vec::with_capacity(h * w * 4);
                for y in 0..h {
                    for x in 0..w {
                        // Channels 0..3 are RGBA; mask overrides alpha when invalid.
                        let r = clamp_to_u8(img.data[(0, y, x)]);
                        let g = clamp_to_u8(img.data[(1, y, x)]);
                        let bl = clamp_to_u8(img.data[(2, y, x)]);
                        let a = if img.mask[(y, x)] {
                            clamp_to_u8(img.data[(3, y, x)])
                        } else {
                            0
                        };
                        buf.extend_from_slice(&[r, g, bl, a]);
                    }
                }
                let rgba = image::RgbaImage::from_raw(w_u32, h_u32, buf).ok_or_else(|| {
                    MaptileError::Render("RgbaImage::from_raw rejected the buffer".into())
                })?;
                Ok(image::DynamicImage::ImageRgba8(rgba))
            }
        }
        n => Err(MaptileError::Render(format!(
            "unsupported band count {n}; expected 1, 3, or 4"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::decode;
    use ndarray::{Array2, Array3};

    #[test]
    fn png_and_raw_rgba_preserve_clamping_and_masked_alpha() {
        let values = [
            f32::NAN,
            f32::NEG_INFINITY,
            -1.0,
            0.0,
            254.9,
            255.0,
            256.0,
            f32::INFINITY,
        ];
        let expected = [0, 0, 0, 0, 254, 255, 255, 255];
        let data = Array3::from_shape_fn((4, 1, values.len()), |(_, _, x)| values[x]);
        let mut mask = Array2::from_elem((1, values.len()), true);
        mask[(0, 5)] = false;
        let img = RasterImage {
            data,
            mask,
            nodata: None,
            bounds: None,
            crs: None,
        };
        let raw = crate::raster::convert::rgba_from_raster(&img);
        let png = image::load_from_memory(&to_png(&img).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(png.as_raw(), &raw);
        for (x, pixel) in raw.as_chunks::<4>().0.iter().enumerate() {
            assert_eq!(
                pixel,
                &[
                    expected[x],
                    expected[x],
                    expected[x],
                    if x == 5 { 0 } else { expected[x] }
                ]
            );
        }
    }

    fn solid_rgba(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            v.extend_from_slice(&rgba);
        }
        v
    }

    #[test]
    fn rgba_round_trip_through_png_preserves_pixels() {
        let img = RasterImage::from_rgba8(&solid_rgba(4, 4, [10, 20, 30, 255]), 4, 4).unwrap();
        let bytes = to_png(&img).unwrap();
        // PNG signature.
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
        );
        let round = decode::from_bytes(&bytes).unwrap();
        for y in 0..4 {
            for x in 0..4 {
                assert!((round.data[(0, y, x)] - 10.0).abs() < 1.0);
                assert!((round.data[(1, y, x)] - 20.0).abs() < 1.0);
                assert!((round.data[(2, y, x)] - 30.0).abs() < 1.0);
                assert!((round.data[(3, y, x)] - 255.0).abs() < 1.0);
            }
        }
    }

    #[test]
    fn to_jpeg_rejects_quality_out_of_range() {
        let img = RasterImage::from_rgba8(&solid_rgba(2, 2, [0; 4]), 2, 2).unwrap();
        assert!(matches!(to_jpeg(&img, 0), Err(MaptileError::Render(_))));
        assert!(matches!(to_jpeg(&img, 101), Err(MaptileError::Render(_))));
    }

    #[test]
    fn to_jpeg_starts_with_soi_marker() {
        let img = RasterImage::from_rgba8(&solid_rgba(8, 8, [255, 0, 0, 255]), 8, 8).unwrap();
        let bytes = to_jpeg(&img, 80).unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
    }

    #[test]
    fn to_webp_starts_with_riff_magic() {
        let img = RasterImage::from_rgba8(&solid_rgba(8, 8, [255, 0, 0, 255]), 8, 8).unwrap();
        let bytes = to_webp(&img).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
    }

    #[test]
    fn single_band_encodes_as_grayscale_png() {
        // 4×4 single-band raster, values 0..15 ramp.
        let mut data = Array3::<f32>::zeros((1, 4, 4));
        for y in 0..4 {
            for x in 0..4 {
                data[(0, y, x)] = ((y * 4 + x) * 16) as f32;
            }
        }
        let img = RasterImage {
            data,
            mask: Array2::<bool>::from_elem((4, 4), true),
            nodata: None,
            bounds: None,
            crs: None,
        };
        let bytes = to_png(&img).unwrap();
        let format = image::guess_format(&bytes).unwrap();
        assert_eq!(format, image::ImageFormat::Png);
    }

    #[test]
    fn five_bands_is_render_error() {
        let img = RasterImage {
            data: Array3::<f32>::zeros((5, 2, 2)),
            mask: Array2::<bool>::from_elem((2, 2), true),
            nodata: None,
            bounds: None,
            crs: None,
        };
        assert!(matches!(to_png(&img), Err(MaptileError::Render(_))));
    }

    #[test]
    fn mask_false_overrides_alpha_to_zero() {
        let mut img =
            RasterImage::from_rgba8(&solid_rgba(2, 2, [100, 100, 100, 255]), 2, 2).unwrap();
        img.mask[(0, 0)] = false; // top-left pixel becomes fully transparent.
        let bytes = to_png(&img).unwrap();
        let round = decode::from_bytes(&bytes).unwrap();
        // Alpha at (0, 0) should be ~0; at (1, 1) should be ~255.
        assert!(round.data[(3, 0, 0)] < 1.0);
        assert!((round.data[(3, 1, 1)] - 255.0).abs() < 1.0);
    }
}
