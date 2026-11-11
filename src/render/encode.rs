//! Encode raw RGBA pixels into the supported [`RenderFormat`] outputs
//! using the `image` crate.
//!
//! Only available with the `render` feature on. Supports PNG, JPEG, and WebP.

use crate::error::{MaptileError, MaptileResult};
use crate::render::types::RenderFormat;
use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder};

pub(crate) fn encode_image(image: &image::RgbaImage, format: RenderFormat) -> MaptileResult<Bytes> {
    let mut out = Vec::new();
    let result = match format {
        RenderFormat::Png => PngEncoder::new(&mut out).write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        ),
        RenderFormat::Jpeg => {
            let mut rgb = Vec::with_capacity(image.as_raw().len() / 4 * 3);
            for pixel in image.pixels() {
                rgb.extend_from_slice(&pixel.0[..3]);
            }
            JpegEncoder::new(&mut out).write_image(
                &rgb,
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
        }
        RenderFormat::Webp => WebPEncoder::new_lossless(&mut out).write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        ),
    };
    result.map_err(|error| MaptileError::Render(format!("encode {format:?}: {error}")))?;
    Ok(Bytes::from(out))
}

/// Encode raw RGBA8 pixels (`width * height * 4` bytes) to the requested
/// format.
pub fn encode_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    format: RenderFormat,
) -> MaptileResult<Bytes> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| {
            MaptileError::Render(format!("rgba dimensions {width}x{height} overflow usize"))
        })?;
    if rgba.len() != expected {
        return Err(MaptileError::Render(format!(
            "rgba buffer is {} bytes; expected {expected} for {width}x{height}",
            rgba.len()
        )));
    }

    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or_else(|| {
        MaptileError::Render("image::RgbaImage::from_raw rejected the buffer".into())
    })?;
    encode_image(&image, format)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_red(width: u32, height: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            v.extend_from_slice(&[255, 0, 0, 255]);
        }
        v
    }

    #[test]
    fn png_starts_with_magic_bytes() {
        let rgba = solid_red(4, 4);
        let bytes = encode_rgba(&rgba, 4, 4, RenderFormat::Png).unwrap();
        // PNG signature: 89 50 4E 47 0D 0A 1A 0A
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
        );
    }

    #[test]
    fn jpeg_starts_with_soi_marker() {
        let rgba = solid_red(8, 8);
        let bytes = encode_rgba(&rgba, 8, 8, RenderFormat::Jpeg).unwrap();
        // JPEG SOI marker: FF D8
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
    }

    #[test]
    fn jpeg_matches_legacy_dynamic_image_encoding() {
        let image = image::RgbaImage::from_fn(8, 8, |x, y| {
            image::Rgba([(x * 31) as u8, (y * 29) as u8, ((x + y) * 17) as u8, 255])
        });
        let actual = encode_image(&image, RenderFormat::Jpeg).unwrap();

        let mut expected = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut expected, image::ImageFormat::Jpeg)
            .unwrap();

        assert_eq!(actual.as_ref(), expected.into_inner());
    }

    #[test]
    fn buffer_size_mismatch_is_render_error() {
        let res = encode_rgba(&[0u8; 10], 4, 4, RenderFormat::Png);
        assert!(matches!(res, Err(MaptileError::Render(_))));
    }

    #[test]
    fn webp_starts_with_riff_magic() {
        let rgba = solid_red(8, 8);
        let bytes = encode_rgba(&rgba, 8, 8, RenderFormat::Webp).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
    }

    #[test]
    fn rgba_encoder_preserves_dimensions_for_every_format() {
        let rgba = vec![255_u8, 0, 0, 255, 0, 255, 0, 255];
        for format in [RenderFormat::Png, RenderFormat::Jpeg, RenderFormat::Webp] {
            let bytes = encode_rgba(&rgba, 2, 1, format).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 1));
        }
    }

    #[test]
    fn encode_image_borrows_rgba_storage_for_all_formats() {
        let image = image::RgbaImage::from_pixel(3, 2, image::Rgba([12, 34, 56, 255]));
        for format in [RenderFormat::Png, RenderFormat::Jpeg, RenderFormat::Webp] {
            let bytes = encode_image(&image, format).unwrap();
            assert_eq!(image.get_pixel(0, 0).0, [12, 34, 56, 255]);
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (3, 2));
        }
    }
}
