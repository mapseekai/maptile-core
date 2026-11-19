//! Encode a [`RasterImage`] (1, 3, or 4 bands) to PNG, JPEG, or WebP.
//!
//! Sister to [`crate::raster::decode`]. Single-band rasters encode as
//! 8-bit grayscale; 3-band rasters encode as RGB; 4-band as RGBA. The
//! `mask` field is folded into the alpha channel for PNG/WebP output:
//! 1- and 3-band rasters with any masked-out pixel encode as gray+alpha
//! and RGBA, so NoData stays transparent. JPEG output drops the alpha
//! channel and the mask (JPEG has no transparency). All encoders reject a mask
//! whose `(height, width)` does not match the data, including all-valid masks.
//!
//! The `*_async` wrappers run on this crate's shared bounded blocking
//! budget, see [`crate::transport::configure_blocking_concurrency`].

use crate::error::{MaptileError, MaptileResult};
use crate::raster::convert::{clamp_to_u8, rgba_from_raster};
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

/// Async wrapper for [`to_png`] that runs the CPU-bound encode on this
/// crate's bounded blocking budget.
///
/// Waits for a slot of the budget configured by
/// [`crate::transport::configure_blocking_concurrency`]. Cancelling the
/// future while it waits takes no slot; once the encode has started the
/// slot stays held until the encode finishes. Do not block on these
/// wrappers from inside other work running under the same budget.
pub async fn to_png_async(img: RasterImage) -> MaptileResult<Bytes> {
    run_encode(move || to_png(&img)).await
}

/// Async wrapper for [`to_jpeg`]; see [`to_png_async`] for budget rules.
pub async fn to_jpeg_async(img: RasterImage, quality: u8) -> MaptileResult<Bytes> {
    run_encode(move || to_jpeg(&img, quality)).await
}

/// Async wrapper for [`to_webp`]; see [`to_png_async`] for budget rules.
pub async fn to_webp_async(img: RasterImage) -> MaptileResult<Bytes> {
    run_encode(move || to_webp(&img)).await
}

/// Run an encode under the shared blocking budget. Encoders only return
/// `Render`; a `Backend` error comes from the task itself (panic or
/// runtime shutdown) and keeps the historical `encode task` message.
async fn run_encode(
    work: impl FnOnce() -> MaptileResult<Bytes> + Send + 'static,
) -> MaptileResult<Bytes> {
    crate::transport::run_blocking(work)
        .await
        .map_err(|e| match e {
            MaptileError::Backend(source) => MaptileError::Render(format!("encode task: {source}")),
            other => other,
        })
}

/// Convert a `RasterImage` into a `DynamicImage` suitable for one of the
/// `image` crate encoders.
///
/// - 1 band → 8-bit grayscale (`ImageLuma8`), or `ImageLumaA8` when some
///   pixel is masked out,
/// - 3 bands → 8-bit RGB (`ImageRgb8`), or `ImageRgba8` when some pixel is
///   masked out,
/// - 4 bands → 8-bit RGBA (`ImageRgba8`).
///
/// Masked-out pixels get alpha 0 via [`rgba_from_raster`]. When
/// `drop_alpha` is true (JPEG) the alpha channel and mask are omitted.
///
/// Returns `MaptileError::Render` on unsupported band counts or mismatched mask shape.
fn raster_to_dynamic(img: &RasterImage, drop_alpha: bool) -> MaptileResult<image::DynamicImage> {
    let (b, h, w) = img.shape();
    if img.mask.dim() != (h, w) {
        return Err(MaptileError::Render(format!(
            "raster mask shape {:?} does not match data shape ({h}, {w})",
            img.mask.dim()
        )));
    }
    let h_u32 = h as u32;
    let w_u32 = w as u32;
    if !drop_alpha && matches!(b, 1 | 3 | 4) && (b == 4 || img.mask.iter().any(|valid| !valid)) {
        let rgba = rgba_from_raster(img);
        if b == 1 {
            let gray_alpha = rgba
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|px| [px[0], px[3]])
                .collect();
            let gray =
                image::GrayAlphaImage::from_raw(w_u32, h_u32, gray_alpha).ok_or_else(|| {
                    MaptileError::Render("GrayAlphaImage::from_raw rejected the buffer".into())
                })?;
            return Ok(image::DynamicImage::ImageLumaA8(gray));
        }
        let rgba = image::RgbaImage::from_raw(w_u32, h_u32, rgba).ok_or_else(|| {
            MaptileError::Render("RgbaImage::from_raw rejected the buffer".into())
        })?;
        return Ok(image::DynamicImage::ImageRgba8(rgba));
    }
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
        // 3 bands, or 4 bands with the alpha channel dropped.
        3 | 4 => {
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
        n => Err(MaptileError::Render(format!(
            "unsupported band count {n}; expected 1, 3, or 4"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::decode;
    use ndarray::{Array2, Array3, ShapeBuilder};

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

    #[tokio::test]
    async fn unsupported_bands_are_render_errors_in_all_encoders() {
        for bands in [0, 2, 5] {
            for (h, w) in [(2, 2), (0, 2), (2, 0)] {
                for nonstandard in [false, true] {
                    for valid in [true, false] {
                        let mask = if nonstandard {
                            Array2::from_elem((h, w).f(), valid)
                        } else {
                            Array2::from_elem((h, w), valid)
                        };
                        let img = RasterImage {
                            data: Array3::zeros((bands, h, w)),
                            mask,
                            nodata: None,
                            bounds: None,
                            crs: None,
                        };
                        for result in [
                            to_png(&img),
                            to_jpeg(&img, 90),
                            to_webp(&img),
                            to_png_async(img.clone()).await,
                            to_jpeg_async(img.clone(), 90).await,
                            to_webp_async(img).await,
                        ] {
                            assert!(
                                matches!(result, Err(MaptileError::Render(ref message)) if message.contains("unsupported band count")),
                                "bands={bands} size={h}x{w} nonstandard={nonstandard} valid={valid}: {result:?}"
                            );
                        }
                    }
                }
            }
        }
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

    #[test]
    fn png_and_webp_round_trips_preserve_masks_for_all_supported_bands() {
        for bands in [1, 3, 4] {
            let mut img = RasterImage {
                data: Array3::from_shape_fn((bands, 1, 3), |(b, _, x)| (40 + b * 30 + x) as f32),
                mask: Array2::from_shape_vec((1, 3), vec![true, false, true]).unwrap(),
                nodata: None,
                bounds: None,
                crs: None,
            };
            for encode in [to_png, to_webp] {
                let round = image::load_from_memory(&encode(&img).unwrap())
                    .unwrap()
                    .to_rgba8();
                let expected = rgba_from_raster(&img);
                for (actual, expected) in round.pixels().zip(expected.as_chunks::<4>().0) {
                    assert_eq!(actual[3], expected[3]);
                    if expected[3] != 0 {
                        assert_eq!(&actual.0[..3], &expected[..3]);
                    }
                }
            }
            let jpeg_with_mask = to_jpeg(&img, 90).unwrap();
            img.mask.fill(true);
            assert_eq!(jpeg_with_mask, to_jpeg(&img, 90).unwrap());
            let png = image::load_from_memory(&to_png(&img).unwrap()).unwrap();
            assert_eq!(
                png.color(),
                match bands {
                    1 => image::ColorType::L8,
                    3 => image::ColorType::Rgb8,
                    _ => image::ColorType::Rgba8,
                }
            );
        }
    }

    #[tokio::test]
    async fn encoders_validate_mask_shape_for_every_layout_and_band_count() {
        for bands in [1, 3, 4] {
            for shape in [(1, 3), (3, 3), (3, 2), (2, 3)] {
                for nonstandard in [false, true] {
                    for all_valid in [false, true] {
                        let mut data = Array3::from_shape_fn((bands, 2, 3), |(b, y, x)| {
                            (40 + b * 30 + y * 3 + x) as f32
                        });
                        let mut mask = Array2::from_elem(shape, true);
                        mask[(0, 0)] = all_valid;
                        if nonstandard {
                            data = data
                                .reversed_axes()
                                .as_standard_layout()
                                .to_owned()
                                .reversed_axes();
                            mask = mask
                                .reversed_axes()
                                .as_standard_layout()
                                .to_owned()
                                .reversed_axes();
                            assert!(data.as_slice().is_none());
                        }
                        let img = RasterImage {
                            data,
                            mask,
                            nodata: None,
                            bounds: None,
                            crs: None,
                        };
                        let results = [
                            to_png(&img),
                            to_webp(&img),
                            to_jpeg(&img, 90),
                            to_png_async(img.clone()).await,
                            to_webp_async(img.clone()).await,
                            to_jpeg_async(img.clone(), 90).await,
                        ];
                        if shape != (2, 3) {
                            for result in results {
                                assert!(
                                    matches!(result, Err(MaptileError::Render(ref message)) if message.contains("mask shape")),
                                    "bands={bands} shape={shape:?} nonstandard={nonstandard} all_valid={all_valid}: {result:?}"
                                );
                            }
                        } else {
                            for result in &results {
                                assert!(result.is_ok(), "{result:?}");
                            }
                            for (sync, asynchronous) in results[..3].iter().zip(&results[3..]) {
                                assert_eq!(sync.as_ref().unwrap(), asynchronous.as_ref().unwrap());
                            }
                            let expected = rgba_from_raster(&img);
                            for bytes in &results[..2] {
                                let decoded = image::load_from_memory(bytes.as_ref().unwrap())
                                    .unwrap()
                                    .to_rgba8();
                                for (actual, expected) in
                                    decoded.pixels().zip(expected.as_chunks::<4>().0)
                                {
                                    assert_eq!(actual[3], expected[3]);
                                    if expected[3] != 0 {
                                        assert_eq!(&actual.0[..3], &expected[..3]);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn async_encoders_share_budget_and_keep_it_until_cancelled_work_finishes() {
        const CHILD: &str = "MAPTILE_ENCODER_BUDGET_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "raster::encode::tests::async_encoders_share_budget_and_keep_it_until_cancelled_work_finishes", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        crate::transport::configure_blocking_concurrency(1).unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (started, entered) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let first = tokio::spawn(run_encode(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
                Ok(Bytes::new())
            }));
            entered.await.unwrap();
            first.abort();
            assert!(first.await.unwrap_err().is_cancelled());
            let img = RasterImage::from_rgba8(&solid_rgba(2, 2, [12, 34, 56, 255]), 2, 2).unwrap();
            let mut png = Box::pin(to_png_async(img.clone()));
            let mut jpeg = Box::pin(to_jpeg_async(img.clone(), 80));
            let mut webp = Box::pin(to_webp_async(img.clone()));
            let mut transport = Box::pin(crate::transport::run_blocking(|| Ok(42)));
            assert!(futures_util::poll!(&mut png).is_pending());
            assert!(futures_util::poll!(&mut jpeg).is_pending());
            assert!(futures_util::poll!(&mut webp).is_pending());
            assert!(futures_util::poll!(&mut transport).is_pending());
            // Cancelling a waiter must neither take nor release the running permit.
            drop(png);
            assert!(futures_util::poll!(&mut transport).is_pending());
            release.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                assert_eq!(jpeg.await.unwrap(), to_jpeg(&img, 80).unwrap());
                assert_eq!(webp.await.unwrap(), to_webp(&img).unwrap());
                assert_eq!(transport.await.unwrap(), 42);
                assert_eq!(to_png_async(img.clone()).await.unwrap(), to_png(&img).unwrap());
                assert_eq!(to_jpeg_async(img.clone(), 0).await.unwrap_err().to_string(), to_jpeg(&img, 0).unwrap_err().to_string());
                let failure = run_encode(|| panic!("test encoder panic")).await.unwrap_err();
                assert!(matches!(failure, MaptileError::Render(message) if message.contains("encode task")));
                assert!(to_png_async(img).await.is_ok());
            }).await.unwrap();
        });
    }
}
