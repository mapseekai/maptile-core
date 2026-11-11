//! End-to-end wiring: a fixture directory laid out per the
//! discovery convention is consumed by [`discovery::scan`] and the resulting
//! report drives [`LocalStyleStore`].

use maptile_core::discovery::{self, DiscoveredSourceKind};
use maptile_core::styles::{self, LocalStyleStore, StyleStore};
use std::fs;
use std::path::Path;

fn write(p: &Path, b: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
}

#[tokio::test]
async fn discovery_layout_drives_all_stores() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Tile sources
    write(&root.join("planet.pmtiles"), b"PMHEADER");

    // One style
    write(
        &root.join("styles/bright/style.json"),
        br#"{
          "version": 8,
          "name": "Bright",
          "glyphs": "/fonts/{fontstack}/{range}.pbf",
          "sprite": "/styles/bright/sprite",
          "sources": {
            "osm": { "type": "vector", "url": "/data/planet.json" }
          },
          "layers": []
        }"#,
    );

    let report = discovery::scan(root).expect("scan ok");

    assert_eq!(report.sources.len(), 1);
    assert_eq!(report.sources[0].kind, DiscoveredSourceKind::PmTiles);
    assert_eq!(report.styles.len(), 1);
    assert_eq!(report.styles[0].id, "bright");

    // Style store driven by discovery output.
    let style_store = LocalStyleStore::new::<_, &str>(
        report
            .styles
            .iter()
            .map(|s| (s.id.as_str(), s.path.clone())),
    )
    .unwrap();
    let bright = style_store.get("bright").await.unwrap();
    assert_eq!(bright.name, "Bright");

    let rewritten = styles::rewrite_urls(&bright.value, "https://maptile.local", "");
    assert_eq!(
        rewritten["sources"]["osm"]["url"],
        serde_json::json!("https://maptile.local/data/planet.json")
    );
    assert_eq!(
        rewritten["sprite"],
        serde_json::json!("https://maptile.local/styles/bright/sprite")
    );

    // Catalog snapshot uses the same store.
    let infos = styles::snapshot(&style_store, Some("https://maptile.local"), "")
        .await
        .unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].id, "bright");
    assert_eq!(
        infos[0].url.as_deref(),
        Some("https://maptile.local/styles/bright/style.json")
    );
}
