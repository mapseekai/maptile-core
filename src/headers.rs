//! HTTP-friendly header value constants and helpers.
//!
//! `maptile-core` does not depend on any HTTP framework, but it owns the
//! recommended cache policy used by every adapter that fronts the registry
//! (axum, volo, ...). Adapters can either use these constants verbatim or
//! parse the structured [`CachePolicy`] for finer control.

use std::time::Duration;

/// Default Cache-Control value for tile responses: 1-day fresh window with a
/// 7-day stale-while-revalidate. Mirrors `tileserver-rs` and the de-facto
/// industry practice (Mapbox, MapTiler, Protomaps).
pub const TILE_CACHE_CONTROL: &str = "public, max-age=86400, stale-while-revalidate=604800";

/// Default Cache-Control for static style/sprite/font assets: 1 hour fresh,
/// 1 day stale-while-revalidate.
pub const STATIC_ASSET_CACHE_CONTROL: &str = "public, max-age=3600, stale-while-revalidate=86400";

/// `no-store` policy for endpoints that must never be cached (admin, ping).
pub const NO_STORE: &str = "no-store";

/// Structured form of the recommended caching directives. Use [`CachePolicy::to_header_value`]
/// to render to a string, or [`CachePolicy::tile`] / [`CachePolicy::static_asset`] for the
/// canonical defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    pub public: bool,
    pub max_age: Duration,
    pub stale_while_revalidate: Option<Duration>,
}

impl CachePolicy {
    #[must_use]
    pub const fn tile() -> Self {
        Self {
            public: true,
            max_age: Duration::from_secs(86_400),
            stale_while_revalidate: Some(Duration::from_secs(604_800)),
        }
    }

    #[must_use]
    pub const fn static_asset() -> Self {
        Self {
            public: true,
            max_age: Duration::from_secs(3_600),
            stale_while_revalidate: Some(Duration::from_secs(86_400)),
        }
    }

    #[must_use]
    pub const fn no_store() -> Self {
        Self {
            public: false,
            max_age: Duration::from_secs(0),
            stale_while_revalidate: None,
        }
    }

    #[must_use]
    pub fn to_header_value(self) -> String {
        if self.max_age.is_zero() && self.stale_while_revalidate.is_none() {
            return NO_STORE.to_string();
        }
        let mut out = String::with_capacity(64);
        out.push_str(if self.public { "public" } else { "private" });
        out.push_str(", max-age=");
        out.push_str(&self.max_age.as_secs().to_string());
        if let Some(swr) = self.stale_while_revalidate {
            out.push_str(", stale-while-revalidate=");
            out.push_str(&swr.as_secs().to_string());
        }
        out
    }
}

/// Map a [`crate::sources::TileFormat`] to its canonical `Content-Type` value.
#[must_use]
pub fn content_type_for(format: crate::sources::TileFormat) -> &'static str {
    use crate::sources::TileFormat as F;
    match format {
        F::Mvt => "application/vnd.mapbox-vector-tile",
        F::Mlt => "application/vnd.maplibre-tile",
        F::Png => "image/png",
        F::Jpeg => "image/jpeg",
        F::Webp => "image/webp",
        F::Avif => "image/avif",
        F::Jxl => "image/jxl",
        F::Json => "application/json",
        F::Gif => "image/gif",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_default_renders_to_constant() {
        assert_eq!(CachePolicy::tile().to_header_value(), TILE_CACHE_CONTROL);
    }

    #[test]
    fn static_asset_default_renders_to_constant() {
        assert_eq!(
            CachePolicy::static_asset().to_header_value(),
            STATIC_ASSET_CACHE_CONTROL
        );
    }

    #[test]
    fn no_store_renders_correctly() {
        assert_eq!(CachePolicy::no_store().to_header_value(), NO_STORE);
    }

    #[test]
    fn private_renders_with_private_token() {
        let p = CachePolicy {
            public: false,
            max_age: Duration::from_secs(60),
            stale_while_revalidate: None,
        };
        assert_eq!(p.to_header_value(), "private, max-age=60");
    }

    #[test]
    fn content_type_mvt() {
        assert_eq!(
            content_type_for(crate::sources::TileFormat::Mvt),
            "application/vnd.mapbox-vector-tile"
        );
    }
}
