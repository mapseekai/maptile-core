//! Canonical hash primitives shared between the in-process tile cache and
//! the cold cache. Producing all variant hashes from this module is the
//! load-bearing invariant that prevents cold-cache poisoning.

/// Bump to invalidate every cache entry across the system. Embedded into
/// every hash input below and every cold-cache object metadata header.
pub const CACHE_SCHEMA_VERSION: u16 = 3;

/// Full 256-bit canonical style identity.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct StyleHash([u8; 32]);

impl StyleHash {
    /// Reserved value for requests with no raster style (for example vector tiles).
    pub const NONE: StyleHash = StyleHash([0u8; 32]);

    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(value: &str) -> Result<Self, hex::FromHexError> {
        let bytes: [u8; 32] = hex::decode(value)?
            .try_into()
            .map_err(|_| hex::FromHexError::InvalidStringLength)?;
        Ok(Self(bytes))
    }
}

impl std::fmt::Debug for StyleHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StyleHash({})", self.to_hex())
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct FilterHash([u8; 16]);

impl FilterHash {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// Full 256-bit source content fingerprint.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct ContentVersion([u8; 32]);

impl ContentVersion {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Digest an arbitrary byte stream.
    #[must_use]
    pub fn from_input(input: &[u8]) -> Self {
        let h = blake3::hash(input);
        Self(*h.as_bytes())
    }

    /// Isolate the caches of independently opened sources with unknown data revisions.
    pub(crate) fn new_generation() -> Self {
        Self::from_input(uuid::Uuid::new_v4().as_bytes())
    }

    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// Build a canonical byte stream from EffectiveStyle inputs.
///
/// Stream order (frozen at v1):
///   \[CACHE_SCHEMA_VERSION u16 LE\]
///   \[tile_size u32 LE\]
///   \[scale u32 LE\]
///   \[format_tag u8: 0=png 1=webp 2=jpg\]
///   \[renderer_version u32 LE\]
///   \[style_json_canonical_bytes len-prefixed (u64 LE) + bytes\]
///
/// The caller is responsible for producing `style_json_canonical_bytes`
/// via `serde_json::to_vec` on the *normalized* (key-sorted) style struct.
#[must_use]
pub fn canonical_style_hash_from_parts(
    tile_size: u32,
    scale: u32,
    format_tag: u8,
    renderer_version: u32,
    style_json_canonical: &[u8],
) -> StyleHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&CACHE_SCHEMA_VERSION.to_le_bytes());
    hasher.update(&tile_size.to_le_bytes());
    hasher.update(&scale.to_le_bytes());
    hasher.update(&[format_tag]);
    hasher.update(&renderer_version.to_le_bytes());
    let len: u64 = style_json_canonical.len() as u64;
    hasher.update(&len.to_le_bytes());
    hasher.update(style_json_canonical);
    let h = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(h.as_bytes());
    StyleHash(out)
}

/// Canonical identity for every input that can change raster bytes.
///
/// `selection` is the canonical mosaic value. Distinct effective algorithms
/// never share an identity. Only requests with neither raster style nor mosaic
/// selection use the `NONE` sentinel.
#[must_use]
pub fn canonical_render_hash(
    style: Option<&crate::raster::style::EffectiveStyle>,
    selection: Option<crate::sources::MosaicPixelSelection>,
    tile_size: u32,
    scale: u32,
    format_tag: u8,
) -> StyleHash {
    if selection.is_none() && style.is_none() {
        return StyleHash::NONE;
    }

    let canonical = style.map_or_else(Vec::new, |s| s.canonical_render_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&CACHE_SCHEMA_VERSION.to_le_bytes());
    hasher.update(&tile_size.to_le_bytes());
    hasher.update(&scale.to_le_bytes());
    hasher.update(&[format_tag]);
    hasher.update(&crate::raster::RENDERER_VERSION.to_le_bytes());
    hasher.update(&[selection.map_or(0, crate::sources::MosaicPixelSelection::cache_tag)]);
    let len = canonical.len() as u64;
    hasher.update(&len.to_le_bytes());
    hasher.update(&canonical);
    let hash = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_bytes());
    StyleHash(out)
}

/// Stable digest over a canonical filter representation.
///
/// Stream:
///   \[CACHE_SCHEMA_VERSION u16 LE\]
///   \[canonical_filter_bytes len-prefixed (u64 LE) + bytes\]
///
/// `canonical_filter` must be produced by the caller from a normalized
/// AST (e.g. cql2::Expr serialized in canonical form). Two structurally
/// equivalent filters must produce identical bytes.
#[must_use]
pub fn canonical_filter_hash(canonical_filter: &[u8]) -> FilterHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&CACHE_SCHEMA_VERSION.to_le_bytes());
    let len: u64 = canonical_filter.len() as u64;
    hasher.update(&len.to_le_bytes());
    hasher.update(canonical_filter);
    let h = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.as_bytes()[..16]);
    FilterHash(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_style_document_survives_and_all_fields_affect_cache_identity() {
        use crate::raster::style::{EffectiveStyle, RasterStyle};
        let document = serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/mosaic/v1.0.0/schema.json", "https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version":"3.0","renderer":{"type":"gray","expression":"b1/b2"},"opacity":0.5,"raster:tile":{"format":"webp","tilesize":512},"raster:mosaic":{"pixel_selection":"highest","stage":"before_channels","rank_channel":1}});
        let style =
            EffectiveStyle::from_raster_style(RasterStyle::from_value(document.clone()).unwrap())
                .unwrap();
        assert_eq!(
            style.raster_style(),
            &RasterStyle::from_value(document.clone()).unwrap()
        );
        assert_eq!(style.tile_size, 512);
        assert_eq!(style.format, crate::raster::style::RasterOutputFormat::Webp);
        let mut changed = document;
        changed["opacity"] = serde_json::json!(0.6);
        let other =
            EffectiveStyle::from_raster_style(RasterStyle::from_value(changed).unwrap()).unwrap();
        assert_ne!(style.style_hash(), other.style_hash());
        assert_ne!(
            style.canonical_style_hash(256, 1, 0),
            other.canonical_style_hash(256, 1, 0)
        );
    }

    fn complete_style(document: serde_json::Value) -> crate::raster::style::EffectiveStyle {
        crate::raster::style::EffectiveStyle::from_raster_style(
            raster_style::Style::from_value(document).unwrap(),
        )
        .unwrap()
    }
    use crate::sources::MosaicPixelSelection;

    #[test]
    fn style_none_is_all_zero() {
        assert_eq!(StyleHash::NONE.as_bytes(), &[0u8; 32]);
        assert_eq!(StyleHash::NONE.to_hex(), "0".repeat(64));
    }

    #[test]
    fn content_version_is_deterministic() {
        let a = ContentVersion::from_input(b"hello");
        let b = ContentVersion::from_input(b"hello");
        let c = ContentVersion::from_input(b"world");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.to_hex().len(), 64);
    }

    #[test]
    fn style_hash_changes_with_any_input() {
        let base = canonical_style_hash_from_parts(256, 1, 0, 1, b"{\"a\":1}");
        assert_ne!(
            base,
            canonical_style_hash_from_parts(512, 1, 0, 1, b"{\"a\":1}"),
            "tile_size"
        );
        assert_ne!(
            base,
            canonical_style_hash_from_parts(256, 2, 0, 1, b"{\"a\":1}"),
            "scale"
        );
        assert_ne!(
            base,
            canonical_style_hash_from_parts(256, 1, 1, 1, b"{\"a\":1}"),
            "format"
        );
        assert_ne!(
            base,
            canonical_style_hash_from_parts(256, 1, 0, 2, b"{\"a\":1}"),
            "renderer"
        );
        assert_ne!(
            base,
            canonical_style_hash_from_parts(256, 1, 0, 1, b"{\"a\":2}"),
            "style json"
        );
    }

    #[test]
    fn style_hash_is_stable_across_calls() {
        let a = canonical_style_hash_from_parts(256, 1, 0, 1, b"{\"x\":42}");
        let b = canonical_style_hash_from_parts(256, 1, 0, 1, b"{\"x\":42}");
        assert_eq!(a, b);
    }

    #[test]
    fn empty_style_json_is_not_style_none() {
        // StyleHash::NONE (all zeros) represents an absent raster style.
        // An empty canonical style JSON is a *real* (but empty) style. The
        // two must never collide — otherwise the sentinel is unsound.
        let h = canonical_style_hash_from_parts(256, 1, 0, 1, b"");
        assert_ne!(h, StyleHash::NONE);
    }

    #[test]
    fn render_hash_changes_with_mosaic_selection() {
        let hashes: std::collections::HashSet<_> = [
            MosaicPixelSelection::First,
            MosaicPixelSelection::Highest,
            MosaicPixelSelection::Lowest,
            MosaicPixelSelection::Mean,
            MosaicPixelSelection::Median,
        ]
        .into_iter()
        .map(|selection| canonical_render_hash(None, Some(selection), 256, 1, 1))
        .collect();
        assert_eq!(hashes.len(), 5);
    }

    #[test]
    fn render_hash_includes_complete_effective_style() {
        let viridis = complete_style(
            serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "pseudocolor", "bidx": vec![1], "colormap_name": "viridis"}, "raster:tile": {"format": "webp", "tilesize": 512, "lossless": true}, "resampling": {"read": "bilinear", "reproject": "bilinear"}}),
        );
        let plasma = complete_style(
            serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "pseudocolor", "bidx": vec![1], "colormap_name": "plasma"}, "raster:tile": {"format": "webp", "tilesize": 512, "lossless": true}, "resampling": {"read": "bilinear", "reproject": "bilinear"}}),
        );

        assert_ne!(
            canonical_render_hash(
                Some(&viridis),
                Some(MosaicPixelSelection::First),
                viridis.tile_size,
                1,
                1,
            ),
            canonical_render_hash(
                Some(&plasma),
                Some(MosaicPixelSelection::First),
                plasma.tile_size,
                1,
                1,
            )
        );
    }

    #[test]
    fn render_hash_includes_document_and_external_tile_size() {
        let style = |tile_size| {
            complete_style(
                serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "pseudocolor", "bidx": vec![1], "colormap_name": "plasma"}, "raster:tile": {"format": "webp", "tilesize": tile_size, "lossless": true}, "resampling": {"read": "bilinear", "reproject": "bilinear"}}),
            )
        };
        let style_256 = style(256);
        let style_512 = style(512);

        let external_256 = canonical_render_hash(
            Some(&style_256),
            Some(MosaicPixelSelection::Mean),
            256,
            1,
            1,
        );
        assert_ne!(
            external_256,
            canonical_render_hash(
                Some(&style_512),
                Some(MosaicPixelSelection::Mean),
                256,
                1,
                1,
            ),
            "the full document participates in the render hash"
        );
        assert_ne!(
            external_256,
            canonical_render_hash(
                Some(&style_256),
                Some(MosaicPixelSelection::Mean),
                512,
                1,
                1,
            ),
            "changing the authoritative external tile_size must change the hash"
        );
    }

    #[test]
    fn render_style_stream_preserves_complete_document() {
        let style = complete_style(
            serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "pseudocolor", "bidx": vec![1], "colormap_name": "viridis"}, "raster:tile": {"format": "webp", "tilesize": 256, "lossless": true}, "resampling": {"read": "bilinear", "reproject": "bilinear"}}),
        );
        assert_eq!(style.canonical_render_bytes(), style.canonical_bytes());
    }

    #[test]
    fn filter_hash_differs_by_input() {
        let a = canonical_filter_hash(b"a=1");
        let b = canonical_filter_hash(b"a=2");
        assert_ne!(a, b);
    }

    #[test]
    fn filter_hash_stable() {
        let a = canonical_filter_hash(b"a=1");
        let b = canonical_filter_hash(b"a=1");
        assert_eq!(a, b);
    }

    #[test]
    fn filter_hash_empty_input_distinct_from_default() {
        // Sanity: empty bytes still produce a stable, non-trivial hash
        // (distinct from FilterHash::from_bytes([0; 16]) for safety).
        let h = canonical_filter_hash(b"");
        assert_ne!(h, FilterHash::from_bytes([0u8; 16]));
    }
}
