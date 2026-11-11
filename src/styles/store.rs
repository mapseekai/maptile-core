//! Style-JSON store + URL rewriting.
//!
//! Keeps raw `serde_json::Value` rather than a typed style schema — the
//! MapLibre style spec evolves frequently and downstream consumers
//! (MapLibre GL JS, MapLibre Native) expect the JSON unchanged except
//! for the URL rewrites done here.

use crate::error::{MaptileError, MaptileResult};
use serde_json::Value;
use std::path::PathBuf;

/// One loaded map style.
#[derive(Debug, Clone)]
pub struct Style {
    pub id: String,
    pub name: String,
    pub value: Value,
    /// On-disk path to `style.json` (used when the matching sprite/glyph
    /// directories are co-located).
    pub source_path: Option<PathBuf>,
}

#[async_trait::async_trait]
pub trait StyleStore: Send + Sync + 'static {
    async fn get(&self, id: &str) -> MaptileResult<Style>;
    async fn list(&self) -> MaptileResult<Vec<String>>;
}

/// Filesystem-backed store. The constructor takes an explicit
/// `id -> style.json path` mapping (built either from explicit config or
/// from [`crate::discovery::scan`]).
pub struct LocalStyleStore {
    entries: std::collections::HashMap<String, PathBuf>,
}

impl LocalStyleStore {
    pub fn new<I, S>(entries: I) -> MaptileResult<Self>
    where
        I: IntoIterator<Item = (S, PathBuf)>,
        S: Into<String>,
    {
        let mut map = std::collections::HashMap::new();
        for (id, path) in entries {
            let id: String = id.into();
            if !path.exists() {
                return Err(MaptileError::Style(format!(
                    "style {id} path missing: {}",
                    path.display()
                )));
            }
            map.insert(id, path);
        }
        Ok(Self { entries: map })
    }
}

#[async_trait::async_trait]
impl StyleStore for LocalStyleStore {
    async fn get(&self, id: &str) -> MaptileResult<Style> {
        let path = self
            .entries
            .get(id)
            .ok_or_else(|| MaptileError::Style(format!("unknown style: {id}")))?
            .clone();

        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| MaptileError::Style(format!("read style {id}: {e}")))?;
        let value: Value = serde_json::from_slice(&raw)
            .map_err(|e| MaptileError::Style(format!("parse style {id}: {e}")))?;
        let name = value
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| id.to_string());
        Ok(Style {
            id: id.to_string(),
            name,
            value,
            source_path: Some(path),
        })
    }

    async fn list(&self) -> MaptileResult<Vec<String>> {
        let mut ids: Vec<String> = self.entries.keys().cloned().collect();
        ids.sort();
        Ok(ids)
    }
}

/// Rewrite relative URLs (paths starting with `/`) inside a style JSON to
/// absolute URLs prefixed with `base_url`. Targets the standard MapLibre
/// fields: `sources[*].url`, `sources[*].tiles[*]`, `glyphs`, `sprite`.
///
/// `query_string` (if non-empty) is appended to every rewritten URL — useful
/// for forwarding API keys (`?key=...`).
pub fn rewrite_urls(style: &Value, base_url: &str, query_string: &str) -> Value {
    let mut style = style.clone();
    let qs = query_string.to_string();
    let base_url = base_url.trim_end_matches('/').to_string();

    let rewrite = move |url: &str| -> Option<String> {
        if url.starts_with('/') {
            Some(format!("{base_url}{url}{qs}"))
        } else {
            None
        }
    };

    if let Some(sources) = style.get_mut("sources").and_then(Value::as_object_mut) {
        for (_id, source) in sources.iter_mut() {
            let Some(obj) = source.as_object_mut() else {
                continue;
            };
            if let Some(Value::String(url)) = obj.get_mut("url")
                && let Some(rewritten) = rewrite(url)
            {
                *url = rewritten;
            }
            if let Some(Value::Array(tiles)) = obj.get_mut("tiles") {
                for t in tiles {
                    if let Value::String(s) = t
                        && let Some(rewritten) = rewrite(s)
                    {
                        *s = rewritten;
                    }
                }
            }
        }
    }

    if let Some(Value::String(glyphs)) = style.get_mut("glyphs")
        && let Some(rewritten) = rewrite(glyphs)
    {
        *glyphs = rewritten;
    }
    if let Some(Value::String(sprite)) = style.get_mut("sprite")
        && let Some(rewritten) = rewrite(sprite)
    {
        *sprite = rewritten;
    }

    style
}

/// File-extension that the tile-serving HTTP layer uses for a given
/// `TileFormat`, matching the convention `tileserver-rs` and
/// `mapbox-gl-js`/`MapLibre GL JS` clients expect on `/data/<id>/{z}/{x}/{y}.<ext>`.
///
/// This deliberately diverges from `martin_tile_utils::Format::metadata_format_value`
/// in two places: `Mlt` is delivered through the `.pbf` path (the encoding
/// difference is signalled via `source.encoding = "mlt"`), and `Jpeg` uses
/// `jpg` rather than `jpeg`.
fn tile_format_extension(fmt: crate::sources::TileFormat) -> &'static str {
    use crate::sources::TileFormat;
    match fmt {
        TileFormat::Mvt | TileFormat::Mlt => "pbf",
        TileFormat::Png => "png",
        TileFormat::Jpeg => "jpg",
        TileFormat::Webp => "webp",
        TileFormat::Json => "json",
        TileFormat::Gif => "gif",
        TileFormat::Avif => "avif",
        TileFormat::Jxl => "jxl",
    }
}

/// Rewrite a style document so the native renderer can resolve
/// every source without HTTP round-trips. For each source whose `url` points
/// at our `/data/<id>(.json)?` endpoint, replace the `url` with an inline
/// `tiles` array built from the source's `tile_info().format`. Glyph and
/// sprite paths are converted to absolute URLs with `base_url` (similar to
/// [`rewrite_urls`] but without query-string forwarding — mbgl supplies its
/// own auth).
///
/// Sources that do not match our `/data/...` pattern, or whose data id is
/// not registered with `sources`, are left untouched (a warning is logged).
pub fn rewrite_style_for_native(
    style: &Value,
    base_url: &str,
    sources: &crate::sources::manager::SourceManager,
) -> Value {
    let mut style = style.clone();
    let base_url = base_url.trim_end_matches('/').to_string();

    if let Some(sources_obj) = style.get_mut("sources").and_then(Value::as_object_mut) {
        for (sid, sval) in sources_obj.iter_mut() {
            rewrite_source_for_native(sid, sval, &base_url, sources);
        }
    }

    if let Some(Value::String(g)) = style.get_mut("glyphs")
        && g.starts_with('/')
    {
        *g = format!("{base_url}{g}");
    }
    if let Some(Value::String(s)) = style.get_mut("sprite")
        && s.starts_with('/')
    {
        *s = format!("{base_url}{s}");
    }

    style
}

fn rewrite_source_for_native(
    source_id: &str,
    source_config: &mut Value,
    base_url: &str,
    sources: &crate::sources::manager::SourceManager,
) {
    let Some(obj) = source_config.as_object_mut() else {
        return;
    };
    let url: String = match obj.get("url") {
        Some(Value::String(u)) => u.clone(),
        _ => return,
    };

    // Match `/data/<id>(.json)?`, optionally with leading scheme+host.
    let data_id: Option<&str> = if let Some(rest) = url.strip_prefix("/data/") {
        Some(rest.strip_suffix(".json").unwrap_or(rest))
    } else if url.contains("/data/") {
        url.rsplit("/data/")
            .next()
            .map(|s| s.strip_suffix(".json").unwrap_or(s))
    } else {
        None
    };
    let Some(id) = data_id.filter(|s| !s.is_empty()) else {
        return;
    };

    let Some(src) = sources.get(id) else {
        tracing::warn!(
            "style source `{source_id}` references `{url}`; data source `{id}` not registered"
        );
        return;
    };
    let info = src.tile_info();
    let tj = src.tilejson();

    let extension = tile_format_extension(info.format);
    let tile_url = format!("{base_url}/data/{id}/{{z}}/{{x}}/{{y}}.{extension}");

    obj.remove("url");
    obj.insert("tiles".to_string(), serde_json::json!([tile_url]));
    if matches!(info.format, crate::sources::TileFormat::Mlt) {
        obj.insert("encoding".to_string(), serde_json::json!("mlt"));
    }

    if !obj.contains_key("minzoom")
        && let Some(z) = tj.minzoom
    {
        obj.insert("minzoom".into(), serde_json::json!(z));
    }
    if !obj.contains_key("maxzoom")
        && let Some(z) = tj.maxzoom
    {
        obj.insert("maxzoom".into(), serde_json::json!(z));
    }
    if !obj.contains_key("bounds")
        && let Some(b) = tj.bounds.as_ref()
    {
        obj.insert(
            "bounds".into(),
            serde_json::json!([b.left, b.bottom, b.right, b.top]),
        );
    }
    if !obj.contains_key("attribution")
        && let Some(a) = tj.attribution.as_ref()
    {
        obj.insert("attribution".into(), serde_json::json!(a));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write(p: &std::path::Path, b: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b).unwrap();
    }

    #[tokio::test]
    async fn get_returns_parsed_style_with_name_from_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bright/style.json");
        write(
            &path,
            br#"{"version":8,"name":"OSM Bright","sources":{},"layers":[]}"#,
        );
        let store = LocalStyleStore::new([("bright", path.clone())]).unwrap();
        let style = store.get("bright").await.unwrap();
        assert_eq!(style.id, "bright");
        assert_eq!(style.name, "OSM Bright");
        assert_eq!(style.source_path.as_ref().unwrap(), &path);
    }

    #[tokio::test]
    async fn get_falls_back_to_id_when_name_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dark/style.json");
        write(&path, br#"{"version":8,"sources":{},"layers":[]}"#);
        let store = LocalStyleStore::new([("dark", path)]).unwrap();
        let style = store.get("dark").await.unwrap();
        assert_eq!(style.name, "dark");
    }

    #[tokio::test]
    async fn unknown_id_is_style_error() {
        let store = LocalStyleStore::new::<Vec<(&str, PathBuf)>, &str>(vec![]).unwrap();
        let res = store.get("nope").await;
        assert!(matches!(res, Err(MaptileError::Style(_))));
    }

    #[test]
    fn rewrite_relative_glyphs_and_sprite() {
        let style = json!({
            "glyphs": "/fonts/{fontstack}/{range}.pbf",
            "sprite": "/styles/bright/sprite",
            "sources": {
                "osm": { "type": "vector", "url": "/data/osm.json" },
                "extra": { "type": "raster", "tiles": ["/data/extra/{z}/{x}/{y}.png"] }
            }
        });
        let out = rewrite_urls(&style, "https://example.com", "?key=abc");
        assert_eq!(
            out["glyphs"],
            json!("https://example.com/fonts/{fontstack}/{range}.pbf?key=abc")
        );
        assert_eq!(
            out["sprite"],
            json!("https://example.com/styles/bright/sprite?key=abc")
        );
        assert_eq!(
            out["sources"]["osm"]["url"],
            json!("https://example.com/data/osm.json?key=abc")
        );
        assert_eq!(
            out["sources"]["extra"]["tiles"][0],
            json!("https://example.com/data/extra/{z}/{x}/{y}.png?key=abc")
        );
    }

    #[test]
    fn rewrite_skips_absolute_urls() {
        let style = json!({
            "glyphs": "https://other.example/fonts/{fontstack}/{range}.pbf",
            "sources": {
                "remote": { "type": "vector", "url": "https://cdn.example/tiles.json" }
            }
        });
        let out = rewrite_urls(&style, "https://maptile.local", "");
        // Untouched.
        assert_eq!(out["glyphs"], style["glyphs"]);
        assert_eq!(
            out["sources"]["remote"]["url"],
            style["sources"]["remote"]["url"]
        );
    }

    #[test]
    fn rewrite_strips_trailing_slash_in_base_url() {
        let style = json!({ "sprite": "/sprite" });
        let out = rewrite_urls(&style, "https://example.com/", "");
        assert_eq!(out["sprite"], json!("https://example.com/sprite"));
    }

    // === rewrite_style_for_native ===

    use crate::error::MaptileResult;
    use crate::sources::manager::SourceManager;
    use crate::sources::{
        TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
    };
    use std::sync::Arc;

    /// Minimal `TileSource` stub used by the rewrite_style_for_native tests.
    struct FakeSource {
        id: &'static str,
        tj: tilejson::TileJSON,
        info: TileInfo,
    }

    impl FakeSource {
        fn new(id: &'static str, format: TileFormat) -> Arc<Self> {
            let mut tj = tilejson::tilejson! { tiles: vec![] };
            tj.minzoom = Some(0);
            tj.maxzoom = Some(14);
            tj.attribution = Some("© test".to_string());
            tj.bounds = Some(tilejson::Bounds::new(-1.0, -2.0, 3.0, 4.0));
            Arc::new(FakeSource {
                id,
                tj,
                info: TileInfo {
                    format,
                    encoding: TileEncoding::Uncompressed,
                },
            })
        }
    }

    #[async_trait::async_trait]
    impl TileSource for FakeSource {
        fn id(&self) -> &str {
            self.id
        }
        fn tilejson(&self) -> &tilejson::TileJSON {
            &self.tj
        }
        fn tile_info(&self) -> TileInfo {
            self.info
        }
        fn clone_source(&self) -> Box<dyn TileSource> {
            Box::new(FakeSource {
                id: self.id,
                tj: self.tj.clone(),
                info: self.info,
            })
        }
        async fn get_tile(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<TileResponse> {
            unreachable!("test fixture; tile reads not exercised");
        }
    }

    #[test]
    fn rewrite_for_native_inlines_data_source_url_to_tiles_template() {
        let mgr = SourceManager::new();
        mgr.register(FakeSource::new("protomaps", TileFormat::Mvt));

        let style = json!({
            "sources": { "src1": { "type": "vector", "url": "/data/protomaps.json" } },
            "glyphs": "/fonts/{fontstack}/{range}.pbf",
            "sprite": "/sprites/default"
        });
        let out = rewrite_style_for_native(&style, "https://maptile.local", &mgr);

        let src = &out["sources"]["src1"];
        assert!(src.get("url").is_none(), "url should be removed");
        assert_eq!(
            src["tiles"][0],
            json!("https://maptile.local/data/protomaps/{z}/{x}/{y}.pbf")
        );
        assert_eq!(src["minzoom"], json!(0));
        assert_eq!(src["maxzoom"], json!(14));
        assert_eq!(src["bounds"], json!([-1.0, -2.0, 3.0, 4.0]));
        assert_eq!(src["attribution"], json!("© test"));
        assert_eq!(
            out["glyphs"],
            json!("https://maptile.local/fonts/{fontstack}/{range}.pbf")
        );
        assert_eq!(
            out["sprite"],
            json!("https://maptile.local/sprites/default")
        );
    }

    #[test]
    fn rewrite_for_native_marks_mlt_encoding() {
        let mgr = SourceManager::new();
        mgr.register(FakeSource::new("protomaps", TileFormat::Mlt));

        let style = json!({
            "sources": { "src1": { "type": "vector", "url": "/data/protomaps.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://maptile.local", &mgr);
        let src = &out["sources"]["src1"];
        assert_eq!(src["encoding"], json!("mlt"));
        assert_eq!(
            src["tiles"][0],
            json!("https://maptile.local/data/protomaps/{z}/{x}/{y}.pbf")
        );
    }

    #[test]
    fn rewrite_for_native_handles_url_without_json_suffix() {
        let mgr = SourceManager::new();
        mgr.register(FakeSource::new("protomaps", TileFormat::Mvt));
        let style = json!({
            "sources": { "s1": { "type": "vector", "url": "/data/protomaps" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &mgr);
        assert_eq!(
            out["sources"]["s1"]["tiles"][0],
            json!("https://m.local/data/protomaps/{z}/{x}/{y}.pbf")
        );
    }

    #[test]
    fn rewrite_for_native_handles_absolute_url() {
        let mgr = SourceManager::new();
        mgr.register(FakeSource::new("protomaps", TileFormat::Mvt));
        let style = json!({
            "sources": {
                "s1": { "type": "vector", "url": "http://upstream/data/protomaps.json" }
            }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &mgr);
        assert_eq!(
            out["sources"]["s1"]["tiles"][0],
            json!("https://m.local/data/protomaps/{z}/{x}/{y}.pbf")
        );
    }

    #[test]
    fn rewrite_for_native_leaves_unknown_data_source_alone() {
        let mgr = SourceManager::new();
        // Note: we do NOT register `protomaps`.
        let style = json!({
            "sources": { "s1": { "type": "vector", "url": "/data/protomaps.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &mgr);
        // Source untouched (still has url, no tiles).
        assert_eq!(out["sources"]["s1"]["url"], json!("/data/protomaps.json"));
        assert!(out["sources"]["s1"].get("tiles").is_none());
    }

    #[test]
    fn rewrite_for_native_skips_non_data_source_url() {
        let mgr = SourceManager::new();
        let style = json!({
            "sources": { "s1": { "type": "vector", "url": "https://other.example/x.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &mgr);
        // External tilejson URLs are left alone.
        assert_eq!(
            out["sources"]["s1"]["url"],
            json!("https://other.example/x.json")
        );
    }

    #[test]
    fn rewrite_for_native_uses_png_extension_for_raster_source() {
        let mgr = SourceManager::new();
        mgr.register(FakeSource::new("hillshade", TileFormat::Png));
        let style = json!({
            "sources": { "s1": { "type": "raster", "url": "/data/hillshade.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &mgr);
        assert_eq!(
            out["sources"]["s1"]["tiles"][0],
            json!("https://m.local/data/hillshade/{z}/{x}/{y}.png")
        );
    }

    #[test]
    fn tile_format_extension_covers_known_formats() {
        assert_eq!(tile_format_extension(TileFormat::Mvt), "pbf");
        assert_eq!(tile_format_extension(TileFormat::Mlt), "pbf");
        assert_eq!(tile_format_extension(TileFormat::Png), "png");
        assert_eq!(tile_format_extension(TileFormat::Jpeg), "jpg");
        assert_eq!(tile_format_extension(TileFormat::Webp), "webp");
        assert_eq!(tile_format_extension(TileFormat::Json), "json");
        assert_eq!(tile_format_extension(TileFormat::Gif), "gif");
        assert_eq!(tile_format_extension(TileFormat::Avif), "avif");
    }
}
