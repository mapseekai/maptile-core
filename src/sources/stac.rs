//! STAC discovery with native Raster Style rendering through async-geotiff.

use crate::error::{MaptileError, MaptileResult};
use crate::raster::style::EffectiveStyle;
use crate::sources::{
    MosaicPixelSelection, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource, effective_pixel_selection,
};
use async_geotiff::{
    bounds::Bbox,
    error::{AgError, AgResult},
    mosaic::{AssetRef, MosaicConfig, MosaicSource, MosaicTiler},
    tms::{TileCoord as AgTileCoord, TileMatrixSet},
};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

const MAX_SEARCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHE_TTL: Duration = Duration::from_secs(1000 * 365 * 24 * 60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacSourceConfig {
    pub id: String,
    /// Root STAC API URL (e.g. `https://earth-search.aws.element84.com/v1`).
    pub api_url: String,
    pub collections: Vec<String>,
    /// Optional STAC `datetime` filter (RFC3339 instant, range, or open
    /// interval). Forwarded verbatim.
    #[serde(default)]
    pub datetime: Option<String>,
    /// Asset key (or `roles` entry) to use when an item has multiple
    /// COGs. Defaults to `"data"`.
    #[serde(default = "default_asset_role")]
    pub asset_role: String,
    #[serde(default)]
    pub minzoom: u8,
    #[serde(default = "default_maxzoom")]
    pub maxzoom: u8,
    pub bounds: Option<[f64; 4]>,
    pub name: Option<String>,
    pub attribution: Option<String>,
    pub description: Option<String>,
    /// Cap on how many items the search returns per tile (defaults to 50, maximum 64).
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Zoom range for the optional search-result cache. `None` permits all zooms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_zoom: Option<crate::cache::CacheZoomRange>,
}

fn default_asset_role() -> String {
    "data".into()
}
fn default_maxzoom() -> u8 {
    18
}
fn default_limit() -> u32 {
    50
}

/// One COG asset discovered via STAC search.
#[derive(Debug, Clone, PartialEq)]
pub struct StacAsset {
    pub id: String,
    pub href: String,
    pub mime_type: Option<String>,
    /// `eo:cloud_cover` percentage on the parent Item (0..=100), if set.
    pub cloud_cover: Option<f64>,
    /// WGS84 bbox of the item: `[west, south, east, north]`.
    pub bbox: Option<[f64; 4]>,
}

#[derive(Clone)]
pub struct StacSource {
    search: StacSearch,
    tj: tilejson::TileJSON,
    tiler: MosaicTiler<StacSearch>,
    content_version: crate::cache::ContentVersion,
}

#[derive(Clone)]
struct StacSearch {
    cfg: Arc<StacSourceConfig>,
    http: reqwest::Client,
    limit: usize,
    results: Option<Cache<TileCoord, Arc<SearchResult>>>,
}

struct SearchResult {
    assets: Vec<StacAsset>,
    generation: String,
}

impl std::fmt::Debug for StacSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StacSource")
            .field("id", &self.search.cfg.id)
            .field("collections", &self.search.cfg.collections)
            .finish()
    }
}

impl StacSource {
    /// Search the STAC API on every read and bypass rendered-tile caching.
    pub fn open(cfg: StacSourceConfig) -> MaptileResult<Self> {
        Self::with_search_cache(cfg, None)
    }

    /// Reuse ordered search results and their opened readers for up to `ttl`.
    /// A new search opens new snapshots. Rendered tiles bypass the tile cache.
    pub fn open_with_search_cache(
        cfg: StacSourceConfig,
        capacity: u64,
        ttl: Duration,
    ) -> MaptileResult<Self> {
        if capacity == 0 || ttl.is_zero() || ttl > MAX_CACHE_TTL {
            return Err(MaptileError::Config(
                "STAC search cache needs a positive capacity and a TTL of up to 1000 years".into(),
            ));
        }
        Self::with_search_cache(
            cfg,
            Some(
                Cache::builder()
                    .max_capacity(capacity)
                    .time_to_live(ttl)
                    .build(),
            ),
        )
    }

    fn with_search_cache(
        cfg: StacSourceConfig,
        results: Option<Cache<TileCoord, Arc<SearchResult>>>,
    ) -> MaptileResult<Self> {
        let valid_api_url = reqwest::Url::parse(&cfg.api_url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host());
        if !valid_api_url {
            return Err(MaptileError::Config(
                "stac.api_url must be an HTTP(S) URL".into(),
            ));
        }
        if cfg.collections.is_empty() {
            return Err(MaptileError::Config(
                "stac.collections must list at least one collection".into(),
            ));
        }
        if cfg.limit == 0 || cfg.minzoom > cfg.maxzoom || cfg.maxzoom > super::tile_math::MAX_ZOOM {
            return Err(MaptileError::Config(
                "invalid STAC item limit or zoom range".into(),
            ));
        }
        let mosaic_config = MosaicConfig::default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("maptile-core/", env!("CARGO_PKG_VERSION")))
            .timeout(mosaic_config.request_timeout)
            .build()
            .map_err(MaptileError::backend)?;

        let mut tj = tilejson::tilejson! { tiles: vec![] };
        tj.minzoom = Some(cfg.minzoom);
        tj.maxzoom = Some(cfg.maxzoom);
        tj.name = cfg.name.clone();
        tj.attribution = cfg.attribution.clone();
        tj.description = cfg.description.clone();
        if let Some(b) = cfg.bounds {
            tj.bounds = Some(tilejson::Bounds::new(b[0], b[1], b[2], b[3]));
        }

        let search = StacSearch {
            limit: (cfg.limit as usize).min(mosaic_config.max_assets_per_tile),
            cfg: Arc::new(cfg),
            http,
            results,
        };
        let tiler = MosaicTiler::builder(search.clone())
            .config(mosaic_config)
            .build();
        Ok(Self {
            search,
            tj,
            tiler,
            content_version: crate::cache::ContentVersion::new_generation(),
        })
    }

    /// List the COG assets covering one tile. Public so callers can
    /// inspect what would be rendered (e.g. for catalog UIs).
    pub async fn list_assets_for_tile(&self, xyz: TileCoord) -> MaptileResult<Vec<StacAsset>> {
        Ok(self.search.list(xyz).await?.assets.clone())
    }
}

impl StacSearch {
    async fn list(&self, xyz: TileCoord) -> MaptileResult<Arc<SearchResult>> {
        super::tile_math::validate_tile_request(&[xyz], None)?;
        if let Some(cache) = &self.results
            && self.cfg.cache_zoom.unwrap_or_default().contains(xyz.z)
        {
            cache
                .try_get_with(xyz, self.search(xyz))
                .await
                .map_err(|error| (*error).clone())
        } else {
            self.search(xyz).await
        }
    }

    async fn search(&self, xyz: TileCoord) -> MaptileResult<Arc<SearchResult>> {
        let bbox = tile_to_wgs84_bbox(xyz.z, xyz.x, xyz.y);
        let url = build_search_url(&self.cfg.api_url);

        let mut payload = serde_json::json!({
            "bbox": bbox,
            "collections": self.cfg.collections,
            "limit": self.limit,
        });
        if let Some(dt) = &self.cfg.datetime {
            payload["datetime"] = serde_json::Value::String(dt.clone());
        }

        let mut resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&payload).map_err(MaptileError::from)?)
            .send()
            .await
            .map_err(|error| MaptileError::backend(error.without_url()))?;
        if !resp.status().is_success() {
            return Err(MaptileError::BackendMsg(format!(
                "stac search returned HTTP {}",
                resp.status()
            )));
        }
        let mut raw = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|error| MaptileError::backend(error.without_url()))?
        {
            if chunk.len() > MAX_SEARCH_BYTES - raw.len() {
                return Err(MaptileError::PayloadTooLarge {
                    limit: MAX_SEARCH_BYTES,
                });
            }
            raw.extend_from_slice(&chunk);
        }
        let role = self.cfg.clone();
        let limit = self.limit;
        crate::transport::run_blocking(move || {
            let body: serde_json::Value =
                serde_json::from_slice(&raw).map_err(MaptileError::backend)?;
            let items = body
                .get("features")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    MaptileError::BackendMsg("STAC search response lacks a features array".into())
                })?;
            if items.len() > limit {
                return Err(MaptileError::Resource(
                    "STAC search response exceeds the requested item limit".into(),
                ));
            }
            Ok(Arc::new(SearchResult {
                assets: extract_assets(&body, &role.asset_role, limit),
                generation: uuid::Uuid::new_v4().to_string(),
            }))
        })
        .await
    }
}

#[async_trait::async_trait]
impl TileSource for StacSource {
    fn id(&self) -> &str {
        &self.search.cfg.id
    }
    fn source_kind(&self) -> super::SourceKind {
        super::SourceKind::Stac
    }

    fn tilejson(&self) -> &tilejson::TileJSON {
        &self.tj
    }
    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Png,
            encoding: TileEncoding::Internal,
        }
    }
    fn mosaic_pixel_selection(&self) -> Option<crate::sources::MosaicPixelSelection> {
        Some(crate::sources::MosaicPixelSelection::First)
    }

    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        crate::cache::CacheZoomRange::disabled()
    }

    fn content_version(&self) -> crate::cache::ContentVersion {
        self.content_version
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }

    fn supports_raster_style(&self) -> bool {
        true
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        super::validate_source_request(self, &[xyz], query)?;
        let style: &EffectiveStyle = query.and_then(|q| q.style).ok_or_else(|| {
            MaptileError::InvalidRequest("canonical raster style is required".into())
        })?;
        let selection = effective_pixel_selection(query, MosaicPixelSelection::First);
        let tile_style = crate::raster::agtiff_style::to_mosaic_tile_style(style, selection)?;
        let tile = super::agtiff_request::render(|limiter| async {
            self.tiler
                .clone()
                .with_request_limiter(limiter)
                .tile(
                    AgTileCoord {
                        tms: TileMatrixSet::WebMercatorQuad,
                        z: xyz.z,
                        x: xyz.x,
                        y: xyz.y,
                    },
                    &tile_style,
                )
                .await
        })
        .await?
        .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        Ok(TileResponse {
            data: tile,
            info: TileInfo {
                format: match style.format {
                    crate::raster::style::RasterOutputFormat::Png => TileFormat::Png,
                    crate::raster::style::RasterOutputFormat::Webp => TileFormat::Webp,
                    crate::raster::style::RasterOutputFormat::Jpeg => TileFormat::Jpeg,
                },
                encoding: TileEncoding::Internal,
            },
            last_modified: None,
        })
    }
}

#[async_trait::async_trait]
impl MosaicSource for StacSearch {
    async fn assets_for_tile(&self, xyz: AgTileCoord) -> AgResult<Vec<AssetRef>> {
        let assets = self
            .list(TileCoord {
                z: xyz.z,
                x: xyz.x,
                y: xyz.y,
            })
            .await
            .map_err(|error| match error {
                MaptileError::PayloadTooLarge { .. } | MaptileError::Resource(_) => {
                    AgError::ResourceLimit(error.to_string())
                }
                _ => AgError::Mosaic(error.to_string()),
            })?;
        // ReaderPool keys snapshots by (href, source_id). Scope each reader to
        // its search result because a catalog checksum alone cannot pin a URL.
        Ok(assets
            .assets
            .iter()
            .map(|asset| {
                let b = asset.bbox.unwrap_or([-180., -90., 180., 90.]);
                AssetRef {
                    url: asset.href.clone(),
                    source_id: assets.generation.clone(),
                    bbox: Bbox::new(b[0], b[1], b[2], b[3]),
                }
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the canonical STAC search endpoint URL: `<api_url>/search`.
#[must_use]
pub fn build_search_url(api_url: &str) -> String {
    let trimmed = api_url.trim_end_matches('/');
    format!("{trimmed}/search")
}

/// Web Mercator → WGS84 bbox of a tile. Matches the convention in
/// [`crate::render::types::RenderRequest::for_tile`] but returns
/// `[west, south, east, north]` for STAC search.
#[must_use]
pub fn tile_to_wgs84_bbox(z: u8, x: u32, y: u32) -> [f64; 4] {
    let n = 2_f64.powi(z as i32);
    let west = (x as f64) / n * 360.0 - 180.0;
    let east = ((x as f64) + 1.0) / n * 360.0 - 180.0;
    let north_rad = ((1.0 - 2.0 * (y as f64) / n) * std::f64::consts::PI)
        .sinh()
        .atan();
    let south_rad = ((1.0 - 2.0 * ((y as f64) + 1.0) / n) * std::f64::consts::PI)
        .sinh()
        .atan();
    [west, south_rad.to_degrees(), east, north_rad.to_degrees()]
}

/// Extract COG assets from a STAC ItemCollection JSON.
#[must_use]
pub fn extract_assets_from_item_collection(
    body: &serde_json::Value,
    asset_role: &str,
) -> Vec<StacAsset> {
    extract_assets(body, asset_role, usize::MAX)
}

fn extract_assets(body: &serde_json::Value, asset_role: &str, limit: usize) -> Vec<StacAsset> {
    let features = match body.get("features").and_then(|v| v.as_array()) {
        Some(f) => f,
        None => return Vec::new(),
    };

    let mut out = Vec::with_capacity(features.len().min(limit));
    for item in features.iter().take(limit) {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let cloud_cover = item
            .get("properties")
            .and_then(|p| p.get("eo:cloud_cover"))
            .and_then(|v| v.as_f64());
        let bbox = item.get("bbox").and_then(|v| v.as_array()).and_then(|arr| {
            let offset = match arr.len() {
                4 => 2,
                6 => 3,
                _ => return None,
            };
            Some([
                arr[0].as_f64()?,
                arr[1].as_f64()?,
                arr[offset].as_f64()?,
                arr[offset + 1].as_f64()?,
            ])
        });

        let assets = match item.get("assets").and_then(|v| v.as_object()) {
            Some(a) => a,
            None => continue,
        };

        let chosen = assets
            .get(asset_role)
            .or_else(|| {
                assets.values().find(|v| {
                    v.get("roles")
                        .and_then(|r| r.as_array())
                        .is_some_and(|roles| roles.iter().any(|x| x.as_str() == Some(asset_role)))
                })
            })
            .or_else(|| {
                assets.values().find(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(is_cog_mime_type)
                        .unwrap_or(false)
                })
            });

        if let Some(val) = chosen {
            let href = val.get("href").and_then(|v| v.as_str()).unwrap_or("");
            if href.is_empty() {
                continue;
            }
            let mime_type = val.get("type").and_then(|v| v.as_str()).map(str::to_string);
            out.push(StacAsset {
                id,
                href: href.to_string(),
                mime_type,
                cloud_cover,
                bbox,
            });
        }
    }
    out
}

/// Heuristic: COG / GeoTIFF MIME types we know how to render.
#[must_use]
pub fn is_cog_mime_type(mime: &str) -> bool {
    matches!(
        mime,
        "image/tiff; application=geotiff"
            | "image/tiff; application=geotiff; profile=cloud-optimized"
            | "image/tiff"
            | "image/geotiff"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_search_url_strips_trailing_slash() {
        assert_eq!(
            build_search_url("https://example.com/stac/"),
            "https://example.com/stac/search"
        );
        assert_eq!(
            build_search_url("https://example.com/stac"),
            "https://example.com/stac/search"
        );
    }

    #[test]
    fn tile_z0_covers_world() {
        let b = tile_to_wgs84_bbox(0, 0, 0);
        assert!((b[0] - (-180.0)).abs() < 1e-6);
        assert!((b[2] - 180.0).abs() < 1e-6);
        assert!(b[3] > 80.0 && b[3] < 90.0); // ~85.05
        assert!(b[1] > -90.0 && b[1] < -80.0);
    }

    #[test]
    fn cog_mime_recognised() {
        assert!(is_cog_mime_type("image/tiff; application=geotiff"));
        assert!(is_cog_mime_type(
            "image/tiff; application=geotiff; profile=cloud-optimized"
        ));
        assert!(!is_cog_mime_type("image/png"));
    }

    #[test]
    fn extract_assets_picks_role_match() {
        let json = serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "id": "scene-1",
                "bbox": [-1.0, -1.0, 1.0, 1.0],
                "properties": {"eo:cloud_cover": 12.5},
                "assets": {
                    "thumbnail": {"href": "https://x/t.png", "type": "image/png", "roles": ["thumbnail"]},
                    "data": {"href": "https://x/d.tif", "type": "image/tiff; application=geotiff", "roles": ["data"]}
                }
            }]
        });
        let out = extract_assets_from_item_collection(&json, "data");
        assert_eq!(out.len(), 1);
        let a = &out[0];
        assert_eq!(a.id, "scene-1");
        assert_eq!(a.href, "https://x/d.tif");
        assert_eq!(a.cloud_cover, Some(12.5));
        assert_eq!(a.bbox, Some([-1.0, -1.0, 1.0, 1.0]));
    }

    #[test]
    fn extract_assets_falls_back_to_cog_mime_when_no_role_match() {
        let json = serde_json::json!({
            "features": [{
                "id": "x",
                "assets": {
                    "img": {"href": "https://x/d.tif", "type": "image/tiff; application=geotiff"}
                }
            }]
        });
        let out = extract_assets_from_item_collection(&json, "data");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].href, "https://x/d.tif");
    }

    #[test]
    fn empty_features_yields_empty_assets() {
        let json = serde_json::json!({"features": []});
        assert!(extract_assets_from_item_collection(&json, "data").is_empty());
    }

    #[test]
    fn discovery_prefers_an_exact_asset_key_and_limits_items() {
        let collection = serde_json::json!({"features": [
            {"id":"first", "bbox":[-1, -2, 0, 3, 4, 500], "assets": {
                "a": {"href":"https://example.test/a.tif", "type":"image/tiff"},
                "red": {"href":"https://example.test/red.tif", "type":"image/tiff", "file:checksum":"test-checksum"}
            }},
            {"id":"second", "assets": {"red":{"href":"https://example.test/second.tif"}}}
        ]});
        let matches = extract_assets(&collection, "red", 1);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].href, "https://example.test/red.tif");
        assert_eq!(matches[0].bbox, Some([-1.0, -2.0, 3.0, 4.0]));
    }

    #[test]
    fn open_validates_required_fields() {
        let bad = StacSourceConfig {
            id: "x".into(),
            api_url: String::new(),
            collections: vec![],
            datetime: None,
            asset_role: default_asset_role(),
            minzoom: 0,
            maxzoom: 18,
            bounds: None,
            name: None,
            attribution: None,
            description: None,
            limit: 50,
            cache_zoom: None,
        };
        assert!(matches!(
            StacSource::open(bad),
            Err(MaptileError::Config(_))
        ));
    }

    #[tokio::test]
    async fn search_refreshes_by_default_and_allows_bounded_reuse() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let handler = {
            let reads = reads.clone();
            move || async move {
                let version = reads.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({
                    "features": [{
                        "id": "scene",
                        "assets": {"data": {
                            "href": format!("https://example.invalid/{version}.tif"),
                            "roles": ["data"]
                        }}
                    }, {
                        "id": "stable-address",
                        "assets": {"data": {
                            "href": "https://example.invalid/stable.tif",
                            "file:checksum": "same-checksum"
                        }}
                    }]
                })
                .to_string()
            }
        };
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/search", axum::routing::post(handler)),
            )
            .await
            .unwrap();
        });

        let cfg: StacSourceConfig = serde_json::from_value(serde_json::json!({
            "id": "fresh",
            "api_url": format!("http://{addr}"),
            "collections": ["scenes"]
        }))
        .unwrap();
        let fresh = StacSource::open(cfg.clone()).unwrap();
        let cached = StacSource::open_with_search_cache(cfg, 2, Duration::from_secs(60)).unwrap();
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let first = fresh.list_assets_for_tile(coord).await.unwrap();
        let second = fresh.list_assets_for_tile(coord).await.unwrap();
        assert_ne!(first, second);

        let ag_coord = AgTileCoord {
            tms: TileMatrixSet::WebMercatorQuad,
            z: coord.z,
            x: coord.x,
            y: coord.y,
        };
        let first = fresh.search.assets_for_tile(ag_coord).await.unwrap();
        let second = fresh.search.assets_for_tile(ag_coord).await.unwrap();
        assert_eq!(first[1].url, second[1].url);
        assert_ne!(first[1].source_id, second[1].source_id);
        let first = cached.search.assets_for_tile(ag_coord).await.unwrap();
        let second = cached.search.assets_for_tile(ag_coord).await.unwrap();
        assert_eq!(first[1].source_id, second[1].source_id);
        assert!(fresh.cache_zoom().is_disabled());
        assert!(cached.cache_zoom().is_disabled());
        assert!(!fresh.consistent_export());
        assert_eq!(reads.load(Ordering::SeqCst), 5);
        server.abort();
    }

    #[test]
    fn config_deserializes_cache_zoom() {
        use crate::cache::CacheZoomRange;
        let json = r#"{
            "id": "test_stac",
            "api_url": "https://example.com/stac",
            "collections": ["coll-a"],
            "cache_zoom": { "minzoom": 4, "maxzoom": 14 }
        }"#;
        let cfg: StacSourceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.cache_zoom, Some(CacheZoomRange::new(Some(4), Some(14))));
    }

    #[test]
    fn config_without_cache_zoom_defaults_to_none() {
        let json = r#"{
            "id": "test_stac",
            "api_url": "https://example.com/stac",
            "collections": ["coll-a"]
        }"#;
        let cfg: StacSourceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.cache_zoom, None);
    }
}
