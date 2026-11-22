//! Style-JSON store + URL rewriting.
//!
//! Keeps raw `serde_json::Value` rather than a typed style schema — the
//! MapLibre style spec evolves frequently and downstream consumers
//! (MapLibre GL JS, MapLibre Native) expect the JSON unchanged except
//! for the URL rewrites done here.

use crate::control::CatalogSnapshot;
use crate::error::{MaptileError, MaptileResult};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

const MAX_STYLE_BYTES: usize = 16 * 1024 * 1024;

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
///
/// Unknown IDs return [`MaptileError::StyleNotFound`]. Filesystem and JSON
/// failures return [`MaptileError::StyleFile`] with the ID, path and original
/// [`MaptileError::Io`] or [`MaptileError::Json`] cause. Only regular files up to
/// 16 MiB are accepted; larger files return a [`MaptileError::PayloadTooLarge`]
/// cause with the same style context.
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
            std::fs::metadata(&path)
                .map_err(MaptileError::from)
                .and_then(|metadata| validate_style_file(&metadata))
                .map_err(|error| style_file_error(&id, &path, error))?;
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
            .ok_or_else(|| MaptileError::StyleNotFound(id.to_string()))?
            .clone();

        let raw = read_regular_file(&path)
            .await
            .map_err(|error| style_file_error(id, &path, error))?;
        let value: Value =
            serde_json::from_slice(&raw).map_err(|error| style_file_error(id, &path, error))?;
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

fn style_file_error(id: &str, path: &Path, source: impl Into<MaptileError>) -> MaptileError {
    MaptileError::StyleFile {
        id: id.to_owned(),
        path: path.to_owned(),
        source: std::sync::Arc::new(source.into()),
    }
}

fn validate_style_file(metadata: &std::fs::Metadata) -> MaptileResult<()> {
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "style path must refer to a regular file",
        )
        .into());
    }
    if metadata.len() > MAX_STYLE_BYTES as u64 {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_STYLE_BYTES,
        });
    }
    Ok(())
}

async fn read_regular_file(path: &Path) -> MaptileResult<Vec<u8>> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    // A path replaced with a FIFO must not block open before we can inspect its handle.
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let mut file = options.open(path).await?;
    validate_style_file(&file.metadata().await?)?;
    read_style_bytes(&mut file).await
}

async fn read_style_bytes(file: &mut tokio::fs::File) -> MaptileResult<Vec<u8>> {
    let mut bytes = Vec::new();
    // The file may grow after its metadata was checked.
    file.take(MAX_STYLE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_STYLE_BYTES {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_STYLE_BYTES,
        });
    }
    Ok(bytes)
}

/// Rewrite relative URLs (paths starting with `/`) inside a style JSON to
/// absolute URLs prefixed with `base_url`. Targets the standard MapLibre
/// fields: `sources[*].url`, `sources[*].tiles[*]`, `glyphs`, `sprite`.
///
/// A `query_string` starting with `?` or `&` is merged into each URL's
/// query before its fragment, without escaping or rewriting placeholders.
/// Supply already encoded parameter values (`#` in a value must be `%23`).
/// Other suffixes retain the legacy behavior of appending verbatim.
pub fn rewrite_urls(style: &Value, base_url: &str, query_string: &str) -> Value {
    let mut style = style.clone();
    let base_url = base_url.trim_end_matches('/').to_string();

    let rewrite = move |url: &str| -> Option<String> {
        if !url.starts_with('/') {
            return None;
        }
        let Some(query) = query_string
            .strip_prefix('?')
            .or_else(|| query_string.strip_prefix('&'))
        else {
            return Some(format!("{base_url}{url}{query_string}"));
        };
        if query.is_empty() {
            return Some(format!("{base_url}{url}"));
        }
        let (resource, fragment) = url.split_at(url.find('#').unwrap_or(url.len()));
        let separator = if !resource.contains('?') {
            "?"
        } else if resource.ends_with(['?', '&']) {
            ""
        } else {
            "&"
        };
        Some(format!("{base_url}{resource}{separator}{query}{fragment}"))
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

/// Inline tile URLs so the native renderer can skip TileJSON requests for
/// registered `/data/<id>` and `/data/<id>.json` references. Resolve the format
/// and metadata of every source and composite against the same catalog snapshot.
/// Relative glyph and sprite paths are prefixed with `base_url` without
/// forwarding the query string because the renderer supplies its own auth.
///
/// Only URLs with a matching registered data ID are rewritten. Unregistered
/// IDs in `/data/` URLs emit a warning.
pub fn rewrite_style_for_native(style: &Value, base_url: &str, catalog: &CatalogSnapshot) -> Value {
    let mut style = style.clone();
    let base_url = base_url.trim_end_matches('/').to_string();

    if let Some(sources_obj) = style.get_mut("sources").and_then(Value::as_object_mut) {
        for (sid, sval) in sources_obj.iter_mut() {
            rewrite_source_for_native(sid, sval, &base_url, catalog);
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
    catalog: &CatalogSnapshot,
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

    let Some(descriptor) = catalog.describe(id) else {
        tracing::warn!(
            "style source `{source_id}` references `{url}`; data source `{id}` not registered"
        );
        return;
    };
    let format = descriptor.info.format;
    let tj = &descriptor.tilejson;

    let extension = tile_format_extension(format);
    let tile_url = format!("{base_url}/data/{id}/{{z}}/{{x}}/{{y}}.{extension}");

    obj.remove("url");
    obj.insert("tiles".to_string(), serde_json::json!([tile_url]));
    if matches!(format, crate::sources::TileFormat::Mlt) {
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
    async fn style_size_limit_accepts_boundary_and_preserves_context_when_exceeded() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("style.json");
        let mut bytes = vec![b' '; MAX_STYLE_BYTES];
        bytes[..2].copy_from_slice(b"{}");
        write(&path, &bytes);
        let store = LocalStyleStore::new([("bounded", path.clone())]).unwrap();
        assert_eq!(store.get("bounded").await.unwrap().value, json!({}));

        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b" ")
            .unwrap();
        for error in [
            LocalStyleStore::new([("bounded", path.clone())])
                .err()
                .unwrap(),
            store.get("bounded").await.unwrap_err(),
            crate::styles::snapshot(&store, None, "").await.unwrap_err(),
        ] {
            assert!(matches!(
                error,
                MaptileError::StyleFile { id, path: failed_path, source }
                    if id == "bounded" && failed_path == path
                        && matches!(source.as_ref(), MaptileError::PayloadTooLarge { limit } if *limit == MAX_STYLE_BYTES)
            ));
        }
    }

    #[tokio::test]
    async fn growth_after_metadata_validation_is_bounded() {
        use tokio::io::AsyncSeekExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("growing.json");
        write(&path, b"{}");
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        validate_style_file(&file.metadata().await.unwrap()).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(4 * MAX_STYLE_BYTES as u64)
            .unwrap();
        assert!(matches!(
            read_style_bytes(&mut file).await,
            Err(MaptileError::PayloadTooLarge { limit }) if limit == MAX_STYLE_BYTES
        ));
        assert_eq!(
            file.stream_position().await.unwrap(),
            MAX_STYLE_BYTES as u64 + 1
        );
    }

    #[tokio::test]
    async fn unknown_id_is_style_not_found() {
        let store = LocalStyleStore::new::<Vec<(&str, PathBuf)>, &str>(vec![]).unwrap();
        let res = store.get("nope").await;
        assert!(matches!(res, Err(MaptileError::StyleNotFound(id)) if id == "nope"));
    }

    #[test]
    fn new_preserves_missing_file_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result = LocalStyleStore::new([("missing", tmp.path().join("missing.json"))]);
        assert!(matches!(
            result,
            Err(MaptileError::StyleFile { id, path, source })
                if id == "missing" && path == tmp.path().join("missing.json")
                    && matches!(source.as_ref(), MaptileError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
        ));
    }

    #[tokio::test]
    async fn get_preserves_file_error_after_registration() {
        use std::error::Error;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("style.json");
        write(&path, br#"{"version":8}"#);
        let store = LocalStyleStore::new([("style", path.clone())]).unwrap();
        fs::remove_file(&path).unwrap();

        let error = store.get("style").await.unwrap_err();
        let MaptileError::StyleFile {
            id,
            path: failed_path,
            source,
        } = &error
        else {
            panic!("{error:?}")
        };
        assert_eq!(id, "style");
        assert_eq!(failed_path, &path);
        assert!(
            matches!(source.as_ref(), MaptileError::Io(cause) if cause.kind() == std::io::ErrorKind::NotFound)
        );
        assert_eq!(
            source
                .source()
                .unwrap()
                .downcast_ref::<std::sync::Arc<std::io::Error>>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn get_preserves_json_error() {
        use std::error::Error;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("style.json");
        write(&path, b"{\n  invalid\n}");
        let store = LocalStyleStore::new([("style", path.clone())]).unwrap();

        let error = store.get("style").await.unwrap_err();
        let MaptileError::StyleFile {
            id,
            path: failed_path,
            source,
        } = &error
        else {
            panic!("{error:?}")
        };
        assert_eq!(id, "style");
        assert_eq!(failed_path, &path);
        assert!(matches!(source.as_ref(), MaptileError::Json(cause) if cause.is_syntax()));
        let cause = source
            .source()
            .unwrap()
            .downcast_ref::<std::sync::Arc<serde_json::Error>>()
            .unwrap();
        assert_eq!(cause.line(), 2);
        assert!(cause.column() > 0);
    }

    #[tokio::test]
    async fn style_snapshot_error_keeps_style_context_and_structured_cause() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken-style.json");
        write(&path, b"{invalid");
        let store = LocalStyleStore::new([("broken-id", path.clone())]).unwrap();
        let error = crate::styles::snapshot(&store, None, "").await.unwrap_err();
        assert!(error.to_string().contains("broken-id"));
        assert!(error.to_string().contains(path.to_str().unwrap()));
        assert!(
            matches!(error, MaptileError::StyleFile { source, .. } if matches!(source.as_ref(), MaptileError::Json(_)))
        );
    }

    #[tokio::test]
    async fn directories_are_rejected_at_construction_and_after_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            matches!(LocalStyleStore::new([("dir", tmp.path().to_owned())]), Err(MaptileError::StyleFile { source, .. }) if matches!(source.as_ref(), MaptileError::Io(error) if error.kind() == std::io::ErrorKind::InvalidInput))
        );
        let path = tmp.path().join("style.json");
        write(&path, b"{}");
        let store = LocalStyleStore::new([("style", path.clone())]).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            store.get("style").await,
            Err(MaptileError::StyleFile { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_replacement_is_rejected_without_a_writer_and_symlinks_still_work() {
        use std::os::unix::ffi::OsStrExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("style.json");
        write(&path, b"{}");
        let link = tmp.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let store = LocalStyleStore::new([("style", link.clone())]).unwrap();
        assert!(runtime.block_on(store.get("style")).is_ok());
        fs::remove_file(&path).unwrap();
        let fifo_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_path is a valid NUL-terminated path in this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert!(LocalStyleStore::new([("fifo", path)]).is_err());
        let result = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), store.get("style")).await
        });
        // A regression must fail the test instead of hanging runtime shutdown on a blocked FIFO.
        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        let result = result.unwrap();
        assert!(
            matches!(result, Err(MaptileError::StyleFile { source, .. }) if matches!(source.as_ref(), MaptileError::Io(error) if error.kind() == std::io::ErrorKind::InvalidInput))
        );
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

    #[test]
    fn rewrite_merges_each_query_before_fragment_without_escaping_templates() {
        let style = json!({
            "sources": {
                "local": { "url": "/data/a.json?v=1#meta", "tiles": [
                    "/tiles/{z}/{x}/{y}.pbf#tile", "/tiles/b?", "/tiles/c?v=1&", "/tiles/d"
                ] },
                "remote": { "url": "https://remote/data?x=1#remote" }
            },
            "glyphs": "/fonts/{fontstack}/{range}.pbf?font=x#glyphs",
            "sprite": "/sprite#?fragment"
        });
        for suffix in ["?key=a%23b", "&key=a%23b"] {
            let out = rewrite_urls(&style, "https://example.com/", suffix);
            assert_eq!(
                out["sources"]["local"]["url"],
                "https://example.com/data/a.json?v=1&key=a%23b#meta"
            );
            assert_eq!(
                out["sources"]["local"]["tiles"],
                json!([
                    "https://example.com/tiles/{z}/{x}/{y}.pbf?key=a%23b#tile",
                    "https://example.com/tiles/b?key=a%23b",
                    "https://example.com/tiles/c?v=1&key=a%23b",
                    "https://example.com/tiles/d?key=a%23b"
                ])
            );
            assert_eq!(out["sources"]["remote"], style["sources"]["remote"]);
            assert_eq!(
                out["glyphs"],
                "https://example.com/fonts/{fontstack}/{range}.pbf?font=x&key=a%23b#glyphs"
            );
            assert_eq!(
                out["sprite"],
                "https://example.com/sprite?key=a%23b#?fragment"
            );
        }
    }

    #[test]
    fn rewrite_preserves_non_query_suffixes_and_empty_queries() {
        for suffix in ["", "?", "&", ".json", "#preview"] {
            let expected_suffix = if matches!(suffix, "?" | "&") {
                ""
            } else {
                suffix
            };
            let out = rewrite_urls(&json!({"sprite": "/sprite"}), "https://example.com", suffix);
            assert_eq!(
                out["sprite"],
                format!("https://example.com/sprite{expected_suffix}")
            );
        }
    }

    // === rewrite_style_for_native ===

    use crate::cache::ContentVersion;
    use crate::control::{
        CompositeDeclaration, DuplicateLayer, MemberFailurePolicy, SourceCatalog, SourceDescriptor,
        TileDriver,
    };
    use crate::sources::{TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse};
    use std::sync::Arc;

    struct FakeSource(SourceDescriptor);

    impl FakeSource {
        fn new(id: &str, format: TileFormat) -> Self {
            let mut descriptor = SourceDescriptor::new(
                id,
                TileInfo {
                    format,
                    encoding: TileEncoding::Uncompressed,
                },
                ContentVersion::new_generation(),
            );
            descriptor.tilejson.minzoom = Some(0);
            descriptor.tilejson.maxzoom = Some(14);
            descriptor.tilejson.attribution = Some("© test".into());
            descriptor.tilejson.bounds = Some(tilejson::Bounds::new(-1.0, -2.0, 3.0, 4.0));
            if format == TileFormat::Mvt {
                descriptor.tilejson.vector_layers = Some(vec![tilejson::VectorLayer::new(
                    id.into(),
                    Default::default(),
                )]);
            }
            Self(descriptor)
        }
    }

    #[async_trait::async_trait]
    impl TileDriver for FakeSource {
        fn descriptor(&self) -> SourceDescriptor {
            self.0.clone()
        }

        async fn read(
            &self,
            _: TileCoord,
            _: Option<&TileQuery<'_>>,
        ) -> MaptileResult<Option<TileResponse>> {
            unreachable!("native style rewriting only reads metadata");
        }
    }

    fn snapshot_with_source(id: &str, format: TileFormat) -> Arc<CatalogSnapshot> {
        let catalog = SourceCatalog::default();
        let mut update = catalog.update();
        update.put(Arc::new(FakeSource::new(id, format))).unwrap();
        catalog.publish(update).unwrap();
        catalog.snapshot()
    }

    #[test]
    fn rewrite_for_native_inlines_data_source_url_to_tiles_template() {
        let snapshot = snapshot_with_source("protomaps", TileFormat::Mvt);

        let style = json!({
            "sources": { "src1": { "type": "vector", "url": "/data/protomaps.json" } },
            "glyphs": "/fonts/{fontstack}/{range}.pbf",
            "sprite": "/sprites/default"
        });
        let out = rewrite_style_for_native(&style, "https://maptile.local", &snapshot);

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
        let snapshot = snapshot_with_source("protomaps", TileFormat::Mlt);

        let style = json!({
            "sources": { "src1": { "type": "vector", "url": "/data/protomaps.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://maptile.local", &snapshot);
        let src = &out["sources"]["src1"];
        assert_eq!(src["encoding"], json!("mlt"));
        assert_eq!(
            src["tiles"][0],
            json!("https://maptile.local/data/protomaps/{z}/{x}/{y}.pbf")
        );
    }

    #[test]
    fn rewrite_for_native_handles_url_without_json_suffix() {
        let snapshot = snapshot_with_source("protomaps", TileFormat::Mvt);
        let style = json!({
            "sources": { "s1": { "type": "vector", "url": "/data/protomaps" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &snapshot);
        assert_eq!(
            out["sources"]["s1"]["tiles"][0],
            json!("https://m.local/data/protomaps/{z}/{x}/{y}.pbf")
        );
    }

    #[test]
    fn rewrite_for_native_handles_absolute_url() {
        let snapshot = snapshot_with_source("protomaps", TileFormat::Mvt);
        let style = json!({
            "sources": {
                "s1": { "type": "vector", "url": "http://upstream/data/protomaps.json" }
            }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &snapshot);
        assert_eq!(
            out["sources"]["s1"]["tiles"][0],
            json!("https://m.local/data/protomaps/{z}/{x}/{y}.pbf")
        );
    }

    #[test]
    fn rewrite_for_native_leaves_unknown_data_source_alone() {
        let snapshot = CatalogSnapshot::default();
        let style = json!({
            "sources": { "s1": { "type": "vector", "url": "/data/protomaps.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &snapshot);
        // Source untouched (still has url, no tiles).
        assert_eq!(out["sources"]["s1"]["url"], json!("/data/protomaps.json"));
        assert!(out["sources"]["s1"].get("tiles").is_none());
    }

    #[test]
    fn rewrite_for_native_skips_non_data_source_url() {
        let snapshot = CatalogSnapshot::default();
        let style = json!({
            "sources": { "s1": { "type": "vector", "url": "https://other.example/x.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &snapshot);
        // External tilejson URLs are left alone.
        assert_eq!(
            out["sources"]["s1"]["url"],
            json!("https://other.example/x.json")
        );
    }

    #[test]
    fn rewrite_for_native_uses_png_extension_for_raster_source() {
        let snapshot = snapshot_with_source("hillshade", TileFormat::Png);
        let style = json!({
            "sources": { "s1": { "type": "raster", "url": "/data/hillshade.json" } }
        });
        let out = rewrite_style_for_native(&style, "https://m.local", &snapshot);
        assert_eq!(
            out["sources"]["s1"]["tiles"][0],
            json!("https://m.local/data/hillshade/{z}/{x}/{y}.png")
        );
    }

    #[test]
    fn rewrite_for_native_reads_sources_and_composites_from_the_selected_snapshot() {
        let catalog = SourceCatalog::default();
        let mut update = catalog.update();
        for id in ["roads", "water"] {
            update
                .put_trusted_mvt(Arc::new(FakeSource::new(id, TileFormat::Mvt)))
                .unwrap();
        }
        update
            .composite(CompositeDeclaration {
                id: "basemap".into(),
                members: vec!["roads".into(), "water".into()],
                lazy: false,
                duplicate_layer: DuplicateLayer::Error,
                failure_policy: MemberFailurePolicy::FailFast,
            })
            .unwrap();
        let generation = catalog.publish(update).unwrap();
        let snapshot = catalog.snapshot();

        let mut expanded = FakeSource::new("roads", TileFormat::Mvt);
        expanded.0.tilejson.bounds = Some(tilejson::Bounds::new(-9.0, -8.0, 1.0, 2.0));
        expanded.0.tilejson.maxzoom = Some(16);
        let mut update = catalog.update();
        update.put_trusted_mvt(Arc::new(expanded)).unwrap();
        assert_eq!(catalog.publish(update).unwrap(), generation + 1);

        let style = json!({
            "sources": {
                "roads": { "type": "vector", "url": "/data/roads.json" },
                "map": { "type": "vector", "url": "/data/basemap.json" }
            },
            "layers": [{
                "id": "water", "type": "fill", "source": "map", "source-layer": "water"
            }]
        });
        let base_url = "https://maptile.local";
        let before = rewrite_style_for_native(&style, base_url, &snapshot);
        let after = rewrite_style_for_native(&style, base_url, &catalog.snapshot());
        for source in ["roads", "map"] {
            assert_eq!(
                before["sources"][source]["bounds"],
                json!([-1.0, -2.0, 3.0, 4.0])
            );
            assert_eq!(before["sources"][source]["maxzoom"], json!(14));
            assert_eq!(after["sources"][source]["maxzoom"], json!(16));
        }
        assert_eq!(
            after["sources"]["roads"]["bounds"],
            json!([-9.0, -8.0, 1.0, 2.0])
        );
        assert_eq!(
            after["sources"]["map"]["bounds"],
            json!([-9.0, -8.0, 3.0, 4.0])
        );
        assert_eq!(
            after["sources"]["map"]["tiles"],
            json!(["https://maptile.local/data/basemap/{z}/{x}/{y}.pbf"])
        );
        assert_eq!(after["layers"], style["layers"]);
        let composite = snapshot.describe("basemap").unwrap();
        let layer_ids: Vec<_> = composite
            .tilejson
            .vector_layers
            .as_ref()
            .unwrap()
            .iter()
            .map(|layer| layer.id.as_str())
            .collect();
        assert_eq!(layer_ids, ["roads", "water"]);
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
