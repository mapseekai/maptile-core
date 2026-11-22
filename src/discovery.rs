//! Filesystem auto-discovery of tile sources and styles.
//!
//! This is the `maptile-core` equivalent of tileserver-rs' `autodetect.rs`,
//! restricted to producing a structured report. Mapping the report to a
//! concrete configuration type (TOML, RPC payload, ...) is the caller's job
//! and lives in `maptile-server` or any other adapter.
//!
//! Layout conventions follow the MapLibre/Mapbox de-facto standard:
//!
//! ```text
//! data/
//! ├── *.pmtiles                    -> DiscoveredSource::PmTiles
//! ├── *.mbtiles                    -> DiscoveredSource::MbTiles
//! ├── *.tif | *.tiff | *.cog       -> DiscoveredSource::Cog (raster feature)
//! └── styles/<id>/style.json       -> DiscoveredStyle
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{MaptileError, MaptileResult};

/// One concrete tile-data file discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSource {
    /// Suggested source id (file stem, with collisions disambiguated by the caller).
    pub id: String,
    pub kind: DiscoveredSourceKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveredSourceKind {
    PmTiles,
    MbTiles,
    Cog,
    Parquet,
    FlatGeobuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredStyle {
    pub id: String,
    /// Absolute path to the `style.json` file.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryReport {
    pub root: PathBuf,
    pub sources: Vec<DiscoveredSource>,
    pub styles: Vec<DiscoveredStyle>,
}

/// Walk `root` recursively and report every recognised artefact.
///
/// Symlinks are followed; hidden directories (leading `.`) are skipped. The
/// returned report is sorted in deterministic order so that callers can diff
/// it across runs.
///
/// **Blocking**: this performs synchronous filesystem I/O via `walkdir`.
/// In an async context (e.g. server reload paths) call [`scan_async`]
/// instead so the runtime is not stalled on large directory trees.
pub fn scan(root: impl AsRef<Path>) -> MaptileResult<DiscoveryReport> {
    scan_with_ids(root, &HashMap::new())
}

/// Assign configured IDs to paths relative to the discovery root. An ambiguous
/// automatic ID rejects the whole inventory, including when an earlier path sorts first.
pub fn scan_with_ids(
    root: impl AsRef<Path>,
    ids: &HashMap<PathBuf, String>,
) -> MaptileResult<DiscoveryReport> {
    let root = root.as_ref();
    if !root.exists() {
        return Err(MaptileError::Config(format!(
            "discovery root does not exist: {}",
            root.display()
        )));
    }

    let mut sources: Vec<DiscoveredSource> = Vec::new();
    let mut styles: Vec<DiscoveredStyle> = Vec::new();

    let walker = walkdir::WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.path()));

    for entry in walker {
        let entry = entry.map_err(|e| {
            MaptileError::Config(format!("walkdir failed under {}: {e}", root.display()))
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };

        // Style: must literally be named style.json.
        if name == "style.json" {
            let id = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("style")
                .to_string();
            styles.push(DiscoveredStyle {
                id,
                path: path.to_path_buf(),
            });
            continue;
        }

        // Tile sources: detected by extension.
        if let Some(kind) = detect_source_kind(name) {
            let id = ids
                .get(path.strip_prefix(root).unwrap_or(path))
                .cloned()
                .unwrap_or_else(|| stem(name));
            sources.push(DiscoveredSource {
                id,
                kind,
                path: path.to_path_buf(),
            });
            continue;
        }
    }

    sources.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.path.cmp(&b.path)));
    for pair in sources.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(MaptileError::Config(format!(
                "source id {:?} is claimed by {} and {}",
                pair[0].id,
                pair[0].path.display(),
                pair[1].path.display()
            )));
        }
    }
    styles.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(DiscoveryReport {
        root: root.to_path_buf(),
        sources,
        styles,
    })
}

/// Async wrapper over [`scan`] that runs the blocking directory walk on a
/// `tokio::task::spawn_blocking` thread so it does not stall the runtime
/// on large source trees.
pub async fn scan_async(root: impl Into<PathBuf>) -> MaptileResult<DiscoveryReport> {
    let root = root.into();
    tokio::task::spawn_blocking(move || scan(&root))
        .await
        .map_err(|e| MaptileError::Config(format!("discovery scan_async task failed: {e}")))?
}

fn detect_source_kind(name: &str) -> Option<DiscoveredSourceKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".pmtiles") {
        Some(DiscoveredSourceKind::PmTiles)
    } else if lower.ends_with(".mbtiles") {
        Some(DiscoveredSourceKind::MbTiles)
    } else if lower.ends_with(".parquet") || lower.ends_with(".geoparquet") {
        Some(DiscoveredSourceKind::Parquet)
    } else if lower.ends_with(".fgb") {
        Some(DiscoveredSourceKind::FlatGeobuf)
    } else if lower.ends_with(".cog") || lower.ends_with(".tif") || lower.ends_with(".tiff") {
        Some(DiscoveredSourceKind::Cog)
    } else {
        None
    }
}

fn stem(name: &str) -> String {
    // file_stem is OsStr-aware but we already constrained `name` to &str.
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s != "." && s != ".." && s.starts_with('.'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn detects_pmtiles_and_mbtiles() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("planet.pmtiles"), b"\x50\x4d");
        write(&tmp.path().join("Sub/area.MBTiles"), b"sql");
        let r = scan(tmp.path()).unwrap();
        assert_eq!(r.sources.len(), 2);
        let kinds: Vec<_> = r.sources.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&DiscoveredSourceKind::PmTiles));
        assert!(kinds.contains(&DiscoveredSourceKind::MbTiles));
    }

    #[test]
    fn detects_styles_with_parent_dir_id() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("styles/bright/style.json"), b"{}");
        let r = scan(tmp.path()).unwrap();
        assert_eq!(r.styles.len(), 1);
        assert_eq!(r.styles[0].id, "bright");
    }

    #[test]
    fn missing_root_is_config_error() {
        let err = scan("/definitely/not/a/real/path/xyz").unwrap_err();
        assert!(matches!(err, MaptileError::Config(_)));
    }

    #[test]
    fn adding_an_earlier_path_cannot_rebind_an_existing_id() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("b/world.pmtiles"), b"x");
        assert_eq!(scan(tmp.path()).unwrap().sources[0].id, "world");
        write(&tmp.path().join("a/world.pmtiles"), b"x");
        let error = scan(tmp.path()).unwrap_err().to_string();
        assert!(error.contains("a/world.pmtiles") && error.contains("b/world.pmtiles"));
        let ids = HashMap::from([(PathBuf::from("a/world.pmtiles"), "world-a".into())]);
        let result = scan_with_ids(tmp.path(), &ids).unwrap();
        assert_eq!(result.sources[0].path, tmp.path().join("b/world.pmtiles"));
        assert_eq!(result.sources[0].id, "world");
    }

    #[test]
    fn no_collision_means_no_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a/foo.pmtiles"), b"x");
        write(&tmp.path().join("a/bar.pmtiles"), b"x");
        let r = scan(tmp.path()).unwrap();
        let ids: BTreeSet<&str> = r.sources.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains("foo"));
        assert!(ids.contains("bar"));
        assert!(!ids.iter().any(|id| id.contains("__")));
    }
}
