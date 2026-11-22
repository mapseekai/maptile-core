//! Configuration types for [`PostgresOutDbRasterSource`].
//!
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresOutDbRasterConfig {
    pub id: String,
    #[serde(default = "default_schema")]
    pub schema: String,
    pub function: Option<String>,
    pub name: Option<String>,
    pub attribution: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub minzoom: u8,
    #[serde(default = "default_maxzoom")]
    pub maxzoom: u8,
    pub bounds: Option<[f64; 4]>,
    /// OutDB always bypasses the tile cache because its assets are resolved per request.
    /// Omit this field or set it to [`crate::cache::CacheZoomRange::disabled()`].
    /// Legacy ranges still deserialize. Source construction rejects ranges that enable caching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_zoom: Option<crate::cache::CacheZoomRange>,
}

fn default_schema() -> String {
    "public".to_string()
}

fn default_maxzoom() -> u8 {
    22
}

#[cfg(test)]
mod cache_zoom_tests {
    use super::*;
    use crate::cache::CacheZoomRange;

    #[test]
    fn config_deserializes_cache_zoom() {
        let json = r#"{
            "id": "test_outdb",
            "cache_zoom": { "minzoom": 4, "maxzoom": 14 }
        }"#;
        let cfg: PostgresOutDbRasterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.cache_zoom, Some(CacheZoomRange::new(Some(4), Some(14))));
    }

    #[test]
    fn config_without_cache_zoom_defaults_to_none() {
        let json = r#"{ "id": "test_outdb" }"#;
        let cfg: PostgresOutDbRasterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.cache_zoom, None);
    }

    #[test]
    fn config_round_trips_explicitly_disabled_cache_zoom() {
        let json = serde_json::json!({
            "id": "test_outdb",
            "cache_zoom": {"minzoom": 255, "maxzoom": 0}
        });
        let cfg: PostgresOutDbRasterConfig = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(cfg.cache_zoom, Some(CacheZoomRange::disabled()));
        assert_eq!(
            serde_json::to_value(cfg).unwrap()["cache_zoom"],
            json["cache_zoom"]
        );
    }
}
