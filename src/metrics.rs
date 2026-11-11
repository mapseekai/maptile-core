//! Prometheus metrics primitives. Behind cargo feature `metrics`.
//!
//! Exposes two `IntCounterVec`s following martin-core's metric topology
//! but in the `maptile_*` namespace:
//!
//! - [`CACHE_REQUESTS_TOTAL`]: lookups for caches WITHOUT a zoom
//!   dimension (sprite, font, style — placeholder for future).
//! - [`TILE_CACHE_REQUESTS_TOTAL`]: lookups for caches keyed by tile
//!   coordinates, with a `zoom` label. Used by [`crate::cache::TileCache::get_or_insert`].
//!
//! All metrics are registered against the default Prometheus registry
//! via `register_int_counter_vec!` on first access (`LazyLock`).
//! Adapter crates can scrape them via `prometheus::default_registry()`.

use std::sync::LazyLock;

use prometheus::{IntCounterVec, register_int_counter_vec};

/// Cache lookups WITHOUT zoom dimension.
///
/// Labels:
/// - `cache`: cache type (e.g. "sprite", "font", "style")
/// - `result`: "hit" | "miss"
pub static CACHE_REQUESTS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "maptile_cache_requests_total",
        "maptile-core cache lookups, labeled by cache type and hit/miss result",
        &["cache", "result"]
    )
    .expect("static cache metric definition is valid")
});

/// Cache lookups WITH zoom dimension.
///
/// Labels:
/// - `cache`: cache type (e.g. "tile", "pmtiles_directory")
/// - `result`: "hit" | "miss"
/// - `zoom`: pre-rendered string from [`ZOOM_LABELS`]
pub static TILE_CACHE_REQUESTS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "maptile_tile_cache_requests_total",
        "maptile-core tile-coordinate cache lookups, labeled by cache type, hit/miss result, and zoom",
        &["cache", "result", "zoom"]
    )
    .expect("static tile cache metric definition is valid")
});

/// Pre-rendered zoom labels indexed by zoom level (0..=30).
/// `with_label_values` requires `&str`, so keeping these as static
/// strings avoids a `format!` per lookup.
pub const ZOOM_LABELS: [&str; 31] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    "17", "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28", "29", "30",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_without_panic() {
        // Force LazyLock initialization. If `register_int_counter_vec!`
        // failed (typo, label conflict), the .expect() would panic here.
        let _ = &*CACHE_REQUESTS_TOTAL;
        let _ = &*TILE_CACHE_REQUESTS_TOTAL;
    }

    #[test]
    fn zoom_labels_indexable_up_to_30() {
        assert_eq!(ZOOM_LABELS[0], "0");
        assert_eq!(ZOOM_LABELS[14], "14");
        assert_eq!(ZOOM_LABELS[30], "30");
        assert_eq!(ZOOM_LABELS.len(), 31);
    }
}
