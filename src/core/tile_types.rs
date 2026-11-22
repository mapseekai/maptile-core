//! Tile coordinates, formats and encodings shared by all backends.

// Adapted from MapLibre Martin's martin-tile-utils 0.7.8 (MIT OR Apache-2.0).
// See third_party/martin for both licenses.
// Portions originate in mbtileserver-rs by Kaveh Karimi (MIT OR Apache-2.0).

use std::fmt::{Display, Formatter};
use strum::EnumIter;

use super::tile_math::MAX_ZOOM;

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct TileCoord {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl Display for TileCoord {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            write!(f, "{}/{}/{}", self.z, self.x, self.y)
        } else {
            write!(f, "{},{},{}", self.z, self.x, self.y)
        }
    }
}

impl TileCoord {
    /// Checks provided coordinates for validity
    /// before constructing [`TileCoord`] instance.
    ///
    /// Check [`Self::new_unchecked`] if you are sure that your inputs are possible.
    #[must_use]
    pub fn new_checked(z: u8, x: u32, y: u32) -> Option<Self> {
        Self::is_possible_on_zoom_level(z, x, y).then_some(Self { z, x, y })
    }

    /// Constructs [`TileCoord`] instance from arguments without checking that the tiles can exist.
    ///
    /// Check [`Self::new_checked`] if you are unsure if your inputs are possible.
    #[must_use]
    pub const fn new_unchecked(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }

    /// Checks that zoom `z` is plausibly small and `x`/`y` is possible on said zoom level
    #[must_use]
    pub const fn is_possible_on_zoom_level(z: u8, x: u32, y: u32) -> bool {
        if z > MAX_ZOOM {
            return false;
        }

        let side_len = 1_u32 << z;
        x < side_len && y < side_len
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, EnumIter)]
pub enum Format {
    Gif,
    Jpeg,
    Json,
    Mvt,
    Mlt,
    Png,
    Webp,
    Avif,
    Jxl,
}

impl Format {
    /// All image formats.
    pub const IMAGE_FORMATS: &[Self] = &[
        Self::Gif,
        Self::Jpeg,
        Self::Png,
        Self::Webp,
        Self::Avif,
        Self::Jxl,
    ];

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.to_ascii_lowercase().as_str() {
            "gif" => Self::Gif,
            "jpg" | "jpeg" => Self::Jpeg,
            "json" => Self::Json,
            "pbf" | "mvt" => Self::Mvt,
            "mlt" => Self::Mlt,
            "png" => Self::Png,
            "webp" => Self::Webp,
            "avif" => Self::Avif,
            "jxl" => Self::Jxl,
            _ => None?,
        })
    }

    /// Get the `format` value as it should be stored in the `MBTiles` metadata table
    #[must_use]
    pub const fn metadata_format_value(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Jpeg => "jpeg",
            Self::Json => "json",
            // QGIS uses `pbf` instead of `mvt` for some reason
            Self::Mvt => "pbf",
            Self::Mlt => "mlt",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }

    #[must_use]
    pub const fn content_type(&self) -> &str {
        match *self {
            Self::Gif => "image/gif",
            Self::Jpeg => "image/jpeg",
            Self::Json => "application/json",
            Self::Mvt => "application/x-protobuf",
            Self::Mlt => "application/vnd.maplibre-tile",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
            Self::Jxl => "image/jxl",
        }
    }

    /// Parse a content type string back to a `Format`.
    #[must_use]
    pub fn from_content_type(supertype: &str, subtype: &str) -> Option<Self> {
        Some(match (supertype, subtype) {
            ("image", "gif") => Self::Gif,
            ("image", "jpeg" | "jpg") => Self::Jpeg,
            ("application", "json") => Self::Json,
            ("application", "x-protobuf" | "vnd.mapbox-vector-tile") => Self::Mvt,
            ("application", "vnd.maplibre-vector-tile" | "vnd.maplibre-tile") => Self::Mlt,
            ("image", "png") => Self::Png,
            ("image", "webp") => Self::Webp,
            ("image", "avif") => Self::Avif,
            ("image", "jxl") => Self::Jxl,
            _ => None?,
        })
    }

    #[must_use]
    pub const fn is_detectable(self) -> bool {
        match self {
            Self::Png
            | Self::Jpeg
            | Self::Gif
            | Self::Webp
            | Self::Avif
            | Self::Jxl
            | Self::Json
            | Self::Mlt => true,
            Self::Mvt => false,
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match *self {
            Self::Gif => "gif",
            Self::Jpeg => "jpeg",
            Self::Json => "json",
            Self::Mvt => "mvt",
            Self::Mlt => "mlt",
            Self::Png => "png",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        })
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Encoding {
    /// Data is not compressed, but it can be
    Uncompressed = 0b0000_0000,
    /// Some formats like JPEG and PNG are already compressed
    Internal = 0b0000_0001,
    Gzip = 0b0000_0010,
    Zlib = 0b0000_0100,
    Brotli = 0b0000_1000,
    Zstd = 0b0001_0000,
}

impl Encoding {
    /// Parse the encoding from common names if they match
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.to_ascii_lowercase().as_str() {
            "none" | "identity" => Self::Uncompressed,
            "gzip" => Self::Gzip,
            "deflate" | "zlib" => Self::Zlib,
            "br" | "brotli" => Self::Brotli,
            "zstd" => Self::Zstd,
            _ => None?,
        })
    }

    /// Returns `None` for [`Encoding::Uncompressed`] and [`Encoding::Internal`]:
    /// absence of the `compression` key in the metadata table means no external encoding.
    #[must_use]
    pub const fn compression(self) -> Option<&'static str> {
        match self {
            Self::Uncompressed | Self::Internal => None,
            Self::Gzip => Some("gzip"),
            Self::Zlib => Some("deflate"),
            Self::Brotli => Some("br"),
            Self::Zstd => Some("zstd"),
        }
    }

    #[must_use]
    pub const fn is_encoded(self) -> bool {
        match self {
            Self::Uncompressed | Self::Internal => false,
            Self::Gzip | Self::Zlib | Self::Brotli | Self::Zstd => true,
        }
    }

    /// The compression the leading bytes of `value` announce, without decompressing anything
    #[must_use]
    pub fn detect(value: &[u8]) -> Option<Self> {
        match value {
            [0x1f, 0x8b, ..] => Some(Self::Gzip),
            [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(Self::Zstd),
            [cmf, flg, ..] if is_zlib_header(*cmf, *flg) => Some(Self::Zlib),
            _ => None,
        }
    }
}

/// Whether `cmf` and `flg` form a zlib header per RFC 1950 §2.2: deflate with a window of at most
/// 32K, and a `FCHECK` that makes the pair a multiple of 31, which admits every `FLEVEL`.
fn is_zlib_header(cmf: u8, flg: u8) -> bool {
    cmf & 0x0f == 8 && cmf >> 4 <= 7 && (u16::from(cmf) * 256 + u16::from(flg)) % 31 == 0
}

/// Describes a tile payload as a `(format, encoding)` pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileInfo {
    /// The underlying tile data format, e.g. MVT, PNG or JPEG.
    pub format: Format,
    /// External compression applied to the payload bytes.
    pub encoding: Encoding,
}

impl TileInfo {
    #[must_use]
    pub const fn new(format: Format, encoding: Encoding) -> Self {
        Self { format, encoding }
    }

    /// Try to figure out the format and encoding of the raw tile data
    #[must_use]
    pub fn detect(value: &[u8]) -> Self {
        match Encoding::detect(value) {
            Some(encoding) => {
                let format =
                    crate::transport::decode(&bytes::Bytes::copy_from_slice(value), encoding)
                        .map_or(Format::Mvt, |decoded| {
                            Self::detect_vectorish_format(&decoded)
                        });
                Self::new(format, encoding)
            }
            _ => match Self::detect_raster_formats(value) {
                Some(raster_format) => Self::new(raster_format, Encoding::Internal),
                None => Self::detect_vectorish_format(value).into(),
            },
        }
    }

    /// Fast-path detection without decompression
    #[must_use]
    fn detect_raster_formats(value: &[u8]) -> Option<Format> {
        match value {
            v if v.starts_with(b"\x89\x50\x4E\x47\x0D\x0A\x1A\x0A") => Some(Format::Png),
            v if v.starts_with(b"\x47\x49\x46\x38\x39\x61") => Some(Format::Gif),
            v if v.starts_with(b"\xFF\xD8\xFF") => Some(Format::Jpeg),
            v if v.starts_with(b"\xFF\x0A") => Some(Format::Jxl),
            v if v.starts_with(b"\x00\x00\x00\x0C\x4A\x58\x4C\x20\x0D\x0A\x87\x0A") => {
                Some(Format::Jxl)
            }
            v if v.starts_with(b"RIFF") && v.len() > 8 && v[8..].starts_with(b"WEBP") => {
                Some(Format::Webp)
            }
            _ => None,
        }
    }

    /// Detect the format of vector (or json) data after decompression
    #[must_use]
    fn detect_vectorish_format(value: &[u8]) -> Format {
        match value {
            v if is_mlt(v) => Format::Mlt,
            v if is_valid_json(v) => Format::Json,
            // If we can't detect the format, we assume MVT.
            // Reasoning:
            //- it's the most common format and
            //- we don't have a detector for it
            _ => Format::Mvt,
        }
    }

    #[must_use]
    pub const fn encoding(self, encoding: Encoding) -> Self {
        Self { encoding, ..self }
    }
}

impl From<Format> for TileInfo {
    fn from(format: Format) -> Self {
        Self::new(
            format,
            match format {
                Format::Mlt
                | Format::Png
                | Format::Jpeg
                | Format::Webp
                | Format::Gif
                | Format::Avif
                | Format::Jxl => Encoding::Internal,
                Format::Mvt | Format::Json => Encoding::Uncompressed,
            },
        )
    }
}

impl Display for TileInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.format.content_type())?;
        if let Some(encoding) = self.encoding.compression() {
            write!(f, "; encoding={encoding}")?;
        } else if self.encoding != Encoding::Uncompressed {
            f.write_str("; uncompressed")?;
        }
        Ok(())
    }
}

fn is_mlt(mut tile: &[u8]) -> bool {
    if tile.is_empty() {
        return false;
    }
    while !tile.is_empty() {
        let Ok(size) = prost::encoding::decode_varint(&mut tile) else {
            return false;
        };
        let Ok(size) = usize::try_from(size) else {
            return false;
        };
        if size == 0 || tile.first() != Some(&1) {
            return false;
        }
        let Some(rest) = tile.get(size..) else {
            return false;
        };
        tile = rest;
    }
    true
}

/// Detects if the given tile is a valid JSON tile.
///
/// The check for a dictionary is used to speed up the validation process.
fn is_valid_json(tile: &[u8]) -> bool {
    tile.starts_with(b"{")
        && tile.ends_with(b"}")
        && serde_json::from_slice::<serde::de::IgnoredAny>(tile).is_ok()
}
