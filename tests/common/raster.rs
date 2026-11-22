use std::path::Path;

use maptile_core::raster::style::{EffectiveStyle, RasterStyle};
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

pub(crate) fn write_world_tiff(path: &Path, value: impl Fn(usize) -> u8) {
    write_world_tiff_with_compression_tag(path, value, 1);
}

pub(crate) fn write_world_tiff_with_compression_tag(
    path: &Path,
    value: impl Fn(usize) -> u8,
    compression: u16,
) {
    let file = std::fs::File::create(path).unwrap();
    let mut tiff = TiffEncoder::new(file).unwrap();
    let mut ifd = tiff.image_directory().unwrap();
    let pixels: Vec<_> = (0..256 * 256).map(value).collect();
    let offset = u32::try_from(ifd.write_data(pixels.as_slice()).unwrap()).unwrap();
    for (tag, value) in [
        (Tag::ImageWidth, 256u32),
        (Tag::ImageLength, 256),
        (Tag::TileWidth, 256),
        (Tag::TileLength, 256),
        (Tag::TileOffsets, offset),
        (Tag::TileByteCounts, 256 * 256),
    ] {
        ifd.write_tag(tag, value).unwrap();
    }
    for (tag, value) in [
        (Tag::BitsPerSample, 8u16),
        (Tag::Compression, compression),
        (Tag::PhotometricInterpretation, 1),
        (Tag::SamplesPerPixel, 1),
        (Tag::PlanarConfiguration, 1),
    ] {
        ifd.write_tag(tag, value).unwrap();
    }
    ifd.write_tag(
        Tag::ModelPixelScaleTag,
        &[360.0 / 256.0, 170.0 / 256.0, 0.0][..],
    )
    .unwrap();
    ifd.write_tag(
        Tag::ModelTiepointTag,
        &[0.0, 0.0, 0.0, -180.0, 85.0, 0.0][..],
    )
    .unwrap();
    ifd.write_tag(
        Tag::GeoKeyDirectoryTag,
        &[
            1u16, 1, 0, 3, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 4326,
        ][..],
    )
    .unwrap();
    ifd.write_tag(Tag::GdalNodata, "0").unwrap();
    ifd.finish().unwrap();
}

pub(crate) fn gray_png_style() -> EffectiveStyle {
    EffectiveStyle::from_raster_style(
        RasterStyle::from_value(serde_json::json!({
            "extensions": [raster_style::TILE_SCHEMA_URL],
            "version": "3.0",
            "renderer": {"type": "gray", "bidx": [1]},
            "raster:tile": {"format": "png", "tilesize": 256}
        }))
        .unwrap(),
    )
    .unwrap()
}
