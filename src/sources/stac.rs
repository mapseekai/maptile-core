//! STAC discovery with native Raster Style rendering through async-geotiff.

use crate::error::{MaptileError, MaptileResult};
use crate::raster::style::EffectiveStyle;
use crate::sources::{
    TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
    /// Cap on how many items the search returns per tile (defaults to 50).
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Per-source cache zoom policy. `None` ≡ cache every zoom.
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

pub struct StacSource {
    id: String,
    cfg: StacSourceConfig,
    tj: tilejson::TileJSON,
    /// HTTP client — held in an Arc so clone_source is cheap. We reuse
    /// the reqwest version that `pmtiles` re-exports to avoid pulling
    /// two incompatible reqwest versions into the build.
    http: Arc<pmtiles::reqwest::Client>,
    block_cache: Arc<async_geotiff::io::BlockCache>,
}

impl std::fmt::Debug for StacSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StacSource")
            .field("id", &self.id)
            .field("api_url", &self.cfg.api_url)
            .field("collections", &self.cfg.collections)
            .finish()
    }
}

impl StacSource {
    pub fn open(cfg: StacSourceConfig) -> MaptileResult<Self> {
        if cfg.api_url.is_empty() {
            return Err(MaptileError::Config(
                "stac.api_url must be a non-empty URL".into(),
            ));
        }
        if cfg.collections.is_empty() {
            return Err(MaptileError::Config(
                "stac.collections must list at least one collection".into(),
            ));
        }
        let http = pmtiles::reqwest::Client::builder()
            .user_agent(concat!("maptile-core/", env!("CARGO_PKG_VERSION")))
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

        Ok(Self {
            id: cfg.id.clone(),
            cfg,
            tj,
            http: Arc::new(http),
            block_cache: Arc::new(async_geotiff::io::BlockCache::new(Default::default())),
        })
    }

    /// List the COG assets covering one tile. Public so callers can
    /// inspect what would be rendered (e.g. for catalog UIs).
    pub async fn list_assets_for_tile(&self, xyz: TileCoord) -> MaptileResult<Vec<StacAsset>> {
        let bbox = tile_to_wgs84_bbox(xyz.z, xyz.x, xyz.y);
        let url = build_search_url(&self.cfg.api_url);

        let mut payload = serde_json::json!({
            "bbox": bbox,
            "collections": self.cfg.collections,
            "limit": self.cfg.limit,
        });
        if let Some(dt) = &self.cfg.datetime {
            payload["datetime"] = serde_json::Value::String(dt.clone());
        }

        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&payload).map_err(MaptileError::from)?)
            .send()
            .await
            .map_err(MaptileError::backend)?;
        if !resp.status().is_success() {
            return Err(MaptileError::BackendMsg(format!(
                "stac search returned HTTP {}",
                resp.status()
            )));
        }
        let raw = resp.bytes().await.map_err(MaptileError::backend)?;
        let body: serde_json::Value =
            serde_json::from_slice(&raw).map_err(MaptileError::backend)?;
        Ok(extract_assets_from_item_collection(
            &body,
            &self.cfg.asset_role,
        ))
    }
}

#[async_trait::async_trait]
impl TileSource for StacSource {
    fn id(&self) -> &str {
        &self.id
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
        // Single source of truth: read from the stored config.
        self.cfg.cache_zoom.unwrap_or_default()
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(StacSource {
            id: self.id.clone(),
            cfg: self.cfg.clone(),
            tj: self.tj.clone(),
            http: self.http.clone(),
            block_cache: self.block_cache.clone(),
        })
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let style = query
            .and_then(|q| q.style)
            .ok_or_else(|| {
                MaptileError::InvalidRequest("canonical raster style is required".into())
            })?
            .clone();
        let assets = self.list_assets_for_tile(xyz).await?;
        let tile = render_assets(&assets, xyz, &style, self.block_cache.clone()).await?;
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

// A tile search is not a complete collection: upstream rejects statistical
// styles until the caller binds a fixed dataset or mosaic collection.
struct TileAssets(Vec<async_geotiff::mosaic::AssetRef>);

#[async_trait::async_trait]
impl async_geotiff::mosaic::MosaicSource for TileAssets {
    async fn assets_for_tile(
        &self,
        _: async_geotiff::tms::TileCoord,
    ) -> async_geotiff::error::AgResult<Vec<async_geotiff::mosaic::AssetRef>> {
        Ok(self.0.clone())
    }
}

async fn render_assets(
    assets: &[StacAsset],
    xyz: TileCoord,
    style: &EffectiveStyle,
    block_cache: Arc<async_geotiff::io::BlockCache>,
) -> MaptileResult<bytes::Bytes> {
    use async_geotiff::{
        bounds::Bbox,
        mosaic::{AssetRef, MosaicTiler},
        tms::{TileCoord as AgTileCoord, TileMatrixSet},
    };
    let tile_style = crate::raster::agtiff_style::to_tile_style(style)?;
    let assets = TileAssets(
        assets
            .iter()
            .map(|asset| {
                let b = asset.bbox.unwrap_or([-180., -90., 180., 90.]);
                AssetRef {
                    url: asset.href.clone(),
                    source_id: asset.href.clone(),
                    bbox: Bbox::new(b[0], b[1], b[2], b[3]),
                }
            })
            .collect(),
    );
    MosaicTiler::builder(assets)
        .block_cache(block_cache)
        .build()
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
        .map_err(crate::raster::agtiff_style::render_error)?
        .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))
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
    let features = match body.get("features").and_then(|v| v.as_array()) {
        Some(f) => f,
        None => return Vec::new(),
    };

    let mut out = Vec::with_capacity(features.len());
    for item in features {
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
            if arr.len() >= 4 {
                Some([
                    arr[0].as_f64()?,
                    arr[1].as_f64()?,
                    arr[2].as_f64()?,
                    arr[3].as_f64()?,
                ])
            } else {
                None
            }
        });

        let assets = match item.get("assets").and_then(|v| v.as_object()) {
            Some(a) => a,
            None => continue,
        };

        // Prefer asset whose roles contains `asset_role`, else fall back
        // to the first COG-typed asset.
        let chosen = assets
            .iter()
            .find(|(_, v)| {
                v.get("roles")
                    .and_then(|r| r.as_array())
                    .map(|arr| arr.iter().any(|x| x.as_str() == Some(asset_role)))
                    .unwrap_or(false)
            })
            .or_else(|| {
                assets.iter().find(|(_, v)| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(is_cog_mime_type)
                        .unwrap_or(false)
                })
            });

        if let Some((_key, val)) = chosen {
            let href = val.get("href").and_then(|v| v.as_str()).unwrap_or("");
            if href.is_empty() {
                continue;
            }
            let mime_type = val.get("type").and_then(|v| v.as_str()).map(str::to_string);
            out.push(StacAsset {
                id: id.clone(),
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
