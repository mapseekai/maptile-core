//! Decode encoded image bytes (PNG, JPEG, WebP) into a [`RasterImage`].
//!
//! Used by ingest tooling and by sources that consume already-encoded
//! tiles (STAC overlays, pre-rendered raster archives, ...). For raw
//! GDAL buffers see the `from_gdal_buffer` constructor on `RasterImage`.

use crate::error::{MaptileError, MaptileResult};
use crate::raster::image::{MAX_IMAGE_WORKING_BYTES, RasterImage, rgba_len};
use image::ImageDecoder;

/// Decode `bytes` into a [`RasterImage`] by sniffing the format from the
/// magic bytes. Falls back to `MaptileError::Render` if the format cannot
/// be guessed or decoded.
///
/// **Blocking**: image decode is CPU-bound. From an async context use
/// [`from_bytes_async`] (or wrap a call in `tokio::task::spawn_blocking`)
/// so the runtime is not stalled.
pub fn from_bytes(bytes: &[u8]) -> MaptileResult<RasterImage> {
    let format = image::guess_format(bytes)
        .map_err(|e| MaptileError::Render(format!("guess image format: {e}")))?;
    from_bytes_format(bytes, format)
}

/// Decode `bytes` interpreting them as `format` regardless of magic bytes.
///
/// **Blocking**: image decode is CPU-bound. See [`from_bytes`] note.
pub fn from_bytes_format(bytes: &[u8], format: image::ImageFormat) -> MaptileResult<RasterImage> {
    let decode_error = |e| MaptileError::Render(format!("decode {format:?}: {e}"));
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_WORKING_BYTES as u64);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(decode_error)?;
    let (w, h) = decoder.dimensions();
    let rgba_bytes = rgba_len(w, h)?;
    if decoder.total_bytes() > (MAX_IMAGE_WORKING_BYTES - rgba_bytes) as u64 {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_IMAGE_WORKING_BYTES,
        });
    }
    let rgba = image::DynamicImage::from_decoder(decoder)
        .map_err(decode_error)?
        .into_rgba8();
    RasterImage::from_rgba8(rgba.as_raw(), w, h)
}

/// Async wrapper for [`from_bytes`] that runs the CPU-bound image
/// decode on a bounded blocking worker.
pub async fn from_bytes_async(bytes: bytes::Bytes) -> MaptileResult<RasterImage> {
    crate::transport::run_blocking(move || from_bytes(&bytes)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbaImage};
    use std::io::Cursor;

    fn encode_png(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_raw(w, h, rgba.to_vec()).unwrap();
        let dyn_img = image::DynamicImage::ImageRgba8(img);
        let mut buf = Vec::new();
        dyn_img
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn from_bytes_decodes_png_with_correct_pixel_values() {
        // 4×4 solid red.
        let mut rgba = Vec::new();
        for _ in 0..16 {
            rgba.extend_from_slice(&[255, 0, 0, 255]);
        }
        let png = encode_png(&rgba, 4, 4);
        let img = from_bytes(&png).unwrap();
        assert_eq!(img.shape(), (4, 4, 4));
        for y in 0..4 {
            for x in 0..4 {
                assert!((img.data[(0, y, x)] - 255.0).abs() < 1e-3);
                assert!((img.data[(1, y, x)] - 0.0).abs() < 1e-3);
                assert!((img.data[(2, y, x)] - 0.0).abs() < 1e-3);
                assert!((img.data[(3, y, x)] - 255.0).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn from_bytes_invalid_returns_render_error() {
        let res = from_bytes(b"this is not an image");
        assert!(matches!(res, Err(MaptileError::Render(_))));
    }

    #[test]
    fn rgba_budget_accounts_for_float_planes_before_conversion() {
        assert!(matches!(
            RasterImage::from_rgba8(&[], 8192, 8192),
            Err(MaptileError::PayloadTooLarge { .. })
        ));
        assert_eq!(rgba_len(512, 512).unwrap(), 512 * 512 * 4);
    }

    #[test]
    fn from_bytes_format_decodes_explicit_png() {
        let mut rgba = Vec::new();
        for _ in 0..4 {
            rgba.extend_from_slice(&[10, 20, 30, 255]);
        }
        let png = encode_png(&rgba, 2, 2);
        let img = from_bytes_format(&png, ImageFormat::Png).unwrap();
        assert_eq!(img.shape(), (4, 2, 2));
    }
}
