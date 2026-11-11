use std::hash::{Hash, Hasher};

use ahash::AHasher;

/// An immutable SDK document plus the output metadata used by tile caches.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveStyle {
    raster_style: raster_style::Style,
    raster_json: String,
    pub tile_size: u32,
    pub format: super::RasterOutputFormat,
    style_hash: u64,
}

impl EffectiveStyle {
    pub fn from_raster_style(document: raster_style::Style) -> Result<Self, raster_style::Error> {
        document.require_extension_support(&[
            raster_style::TILE_SCHEMA_URL,
            raster_style::MOSAIC_SCHEMA_URL,
        ])?;
        let json = document.to_json()?;
        let value = document.as_value();
        let format = match value
            .pointer("/raster:tile/format")
            .and_then(serde_json::Value::as_str)
        {
            Some("jpeg") => super::RasterOutputFormat::Jpeg,
            Some("webp") => super::RasterOutputFormat::Webp,
            _ => super::RasterOutputFormat::Png,
        };
        let mut hasher = AHasher::default();
        json.hash(&mut hasher);
        Ok(Self {
            tile_size: value
                .pointer("/raster:tile/tilesize")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(256.0) as u32,
            format,
            style_hash: hasher.finish().max(1),
            raster_json: json,
            raster_style: document,
        })
    }

    pub fn raster_style(&self) -> &raster_style::Style {
        &self.raster_style
    }

    #[must_use]
    pub fn style_hash(&self) -> u64 {
        self.style_hash
    }

    /// Cold-cache canonical 128-bit style hash.
    ///
    /// Distinct from the in-process [`Self::style_hash`] (AHash-u64): this
    /// version is BLAKE3-128 over a stable
    /// canonical byte stream that ALSO includes per-request rendering
    /// inputs (`tile_size`, `scale`, output format). The cold cache key
    /// requires those because the same `EffectiveStyle` rendered as PNG
    /// vs WebP at different scales must NOT collide.
    ///
    #[must_use]
    pub fn canonical_style_hash(
        &self,
        tile_size: u32,
        scale: u32,
        format_tag: u8,
    ) -> crate::cache::StyleHash {
        crate::cache::hashing::canonical_style_hash_from_parts(
            tile_size,
            scale,
            format_tag,
            crate::raster::RENDERER_VERSION,
            &self.canonical_bytes(),
        )
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"mapseek:raster-style:3.0\0".to_vec();
        bytes.extend_from_slice(self.raster_json.as_bytes());
        bytes
    }

    pub(crate) fn canonical_render_bytes(&self) -> Vec<u8> {
        self.canonical_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_rescale_ranges_before_rendering() {
        let mut value = serde_json::json!({
            "version": "3.0", "renderer": {"type": "gray", "bidx": [1]},
            "stretch": {"method": "linear", "rescale": [[0, 255]]}
        });
        let document = raster_style::Style::from_value(value.clone()).unwrap();
        EffectiveStyle::from_raster_style(document).unwrap();
        for ranges in [
            serde_json::json!([]),
            serde_json::json!([[10, 0]]),
            serde_json::json!([[1, 1]]),
            serde_json::json!([[0, f64::INFINITY]]),
            serde_json::json!([[f64::NAN, 1]]),
        ] {
            value["stretch"]["rescale"] = ranges;
            assert!(raster_style::Style::from_value(value.clone()).is_err());
        }
    }

    #[test]
    fn rejects_invalid_renderer_colors_without_a_transparent_fallback() {
        let mut value = serde_json::json!({
            "version": "3.0", "renderer": {"type": "single_color", "bidx": [1], "color": "#008333"}
        });
        let document = raster_style::Style::from_value(value.clone()).unwrap();
        EffectiveStyle::from_raster_style(document).unwrap();
        for color in ["not-a-color", "#12345", "#zzzzzz"] {
            value["renderer"]["color"] = serde_json::json!(color);
            assert!(raster_style::Style::from_value(value.clone()).is_err());
        }
    }

    #[test]
    fn rejects_incomplete_extensions_and_reads_official_tile_metadata() {
        let unknown = raster_style::Style::from_json(r#"{"version":"3.0","renderer":{"type":"gray","bidx":[1]},"extensions":["https://example.test/unknown/v1/schema.json"],"custom:value":1}"#).unwrap();
        assert!(EffectiveStyle::from_raster_style(unknown).is_err());
        let document = raster_style::Style::from_value(serde_json::json!({
            "version": "3.0", "extensions": [raster_style::TILE_SCHEMA_URL],
            "renderer": {"type": "gray", "bidx": [1]},
            "raster:tile": {"format": "webp", "lossless": true, "tilesize": 512}
        }))
        .unwrap();
        let style = EffectiveStyle::from_raster_style(document).unwrap();
        assert_eq!(style.tile_size, 512);
        assert_eq!(style.format, super::super::RasterOutputFormat::Webp);
    }
}
