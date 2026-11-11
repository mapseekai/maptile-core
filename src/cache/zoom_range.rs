//! Zoom-level bounds for tile caching.
//!
//! Mirrors `martin_core::CacheZoomRange` so configurations stay
//! compatible. Used by [`crate::sources::TileSource::cache_zoom`] to
//! tell [`crate::sources::manager::SourceManager`] which zoom levels
//! are worth caching for that source.

use serde::{Deserialize, Serialize};

/// Zoom-level bounds for tile caching.
///
/// `None` on either bound means "unbounded on that side". Default
/// (both `None`) caches every zoom level.
///
/// # Examples
///
/// ```
/// use maptile_core::prelude::CacheZoomRange;
///
/// // Cache only mid-zooms.
/// let r = CacheZoomRange::new(Some(2), Some(14));
/// assert!(!r.contains(1));
/// assert!(r.contains(5));
/// assert!(!r.contains(15));
///
/// // Default = cache everything.
/// assert!(CacheZoomRange::default().contains(0));
/// assert!(CacheZoomRange::default().contains(u8::MAX));
///
/// // Sentinel that never matches — for explicitly disabling caching
/// // while still carrying a `CacheZoomRange` value through config.
/// assert!(!CacheZoomRange::disabled().contains(5));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheZoomRange {
    /// Inclusive minimum zoom. Tiles below this bypass the cache.
    /// Default: no lower bound.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    minzoom: Option<u8>,
    /// Inclusive maximum zoom. Tiles above this bypass the cache.
    /// Default: no upper bound.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    maxzoom: Option<u8>,
}

impl CacheZoomRange {
    /// Construct a range with optional bounds.
    #[must_use]
    pub fn new(minzoom: Option<u8>, maxzoom: Option<u8>) -> Self {
        Self { minzoom, maxzoom }
    }

    /// A range that never matches (`minzoom > maxzoom`). Used to
    /// explicitly disable caching for a source while still carrying
    /// a `CacheZoomRange` value through configuration.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            minzoom: Some(u8::MAX),
            maxzoom: Some(0),
        }
    }

    /// Whether neither bound is set (default-constructed value, i.e.
    /// every zoom level passes [`Self::contains`]).
    #[must_use]
    pub fn is_unbounded(self) -> bool {
        self.minzoom.is_none() && self.maxzoom.is_none()
    }

    /// Renamed in 0.3.0 — use [`Self::is_unbounded`]. The previous name was
    /// confusing because it returned `true` for the default-everything
    /// range (the opposite of what an empty/no-match predicate suggests).
    #[must_use]
    #[deprecated(since = "0.3.0", note = "renamed to `is_unbounded` for clarity")]
    pub fn is_empty(self) -> bool {
        self.is_unbounded()
    }

    /// Whether this range is the [`Self::disabled`] sentinel —
    /// `minzoom > maxzoom`, so [`Self::contains`] always returns `false`.
    #[must_use]
    pub fn is_disabled(self) -> bool {
        matches!((self.minzoom, self.maxzoom), (Some(min), Some(max)) if min > max)
    }

    /// Whether `zoom` falls within the configured bounds.
    /// Missing bounds are treated as unbounded.
    #[must_use]
    pub fn contains(self, zoom: u8) -> bool {
        self.minzoom.is_none_or(|m| zoom >= m) && self.maxzoom.is_none_or(|m| zoom <= m)
    }

    /// Fill any `None` field of `self` with the corresponding field
    /// from `other`. Pre-set fields are kept as-is.
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        Self {
            minzoom: self.minzoom.or(other.minzoom),
            maxzoom: self.maxzoom.or(other.maxzoom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_never_contains() {
        let disabled = CacheZoomRange::disabled();
        assert!(!disabled.contains(0));
        assert!(!disabled.contains(10));
        assert!(!disabled.contains(u8::MAX));
    }

    #[test]
    fn disabled_is_not_unbounded() {
        assert!(!CacheZoomRange::disabled().is_unbounded());
    }

    #[test]
    fn is_disabled_distinguishes_sentinel_from_default() {
        assert!(CacheZoomRange::disabled().is_disabled());
        assert!(!CacheZoomRange::default().is_disabled());
        assert!(!CacheZoomRange::new(Some(2), Some(10)).is_disabled());
        // Inverted bounds explicitly constructed should also count as disabled.
        assert!(CacheZoomRange::new(Some(10), Some(2)).is_disabled());
        // One-sided ranges are not "disabled" — they match by ignoring the
        // missing side.
        assert!(!CacheZoomRange::new(Some(10), None).is_disabled());
        assert!(!CacheZoomRange::new(None, Some(2)).is_disabled());
    }

    #[test]
    fn disabled_not_overridden_by_or() {
        let disabled = CacheZoomRange::disabled();
        let defaults = CacheZoomRange::new(Some(0), Some(20));
        let merged = disabled.or(defaults);
        assert!(!merged.contains(0));
        assert!(!merged.contains(10));
    }

    #[test]
    fn default_contains_all() {
        let range = CacheZoomRange::default();
        assert!(range.contains(0));
        assert!(range.contains(u8::MAX));
    }

    #[test]
    fn bounded_range() {
        let range = CacheZoomRange::new(Some(2), Some(10));
        assert!(!range.contains(1));
        assert!(range.contains(2));
        assert!(range.contains(10));
        assert!(!range.contains(11));
    }

    #[test]
    fn or_fills_only_missing_fields() {
        let lhs = CacheZoomRange::new(Some(3), None);
        let rhs = CacheZoomRange::new(Some(0), Some(20));
        let merged = lhs.or(rhs);
        assert!(!merged.contains(2));
        assert!(merged.contains(3));
        assert!(merged.contains(20));
        assert!(!merged.contains(21));
    }

    #[test]
    fn serde_roundtrip_default_is_empty_object() {
        let range = CacheZoomRange::default();
        let s = serde_json::to_string(&range).unwrap();
        assert_eq!(s, "{}", "default omits both fields via skip_serializing_if");
        let back: CacheZoomRange = serde_json::from_str(&s).unwrap();
        assert_eq!(back, range);
    }

    #[test]
    fn serde_roundtrip_with_bounds() {
        let range = CacheZoomRange::new(Some(2), Some(14));
        let s = serde_json::to_string(&range).unwrap();
        // field order is struct-defined: minzoom first
        assert_eq!(s, r#"{"minzoom":2,"maxzoom":14}"#);
        let back: CacheZoomRange = serde_json::from_str(&s).unwrap();
        assert_eq!(back, range);
    }
}
