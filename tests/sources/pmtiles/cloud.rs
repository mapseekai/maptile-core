#![cfg(feature = "cloud")]

use maptile_core::sources::pmtiles::{CloudPmtilesSource, PmtilesConfig, is_cloud_url};
use maptile_core::sources::{TileCoord, TileFormat, TileSource};

const FIXTURE_PATH: &str = "tests/fixtures/sample.pmtiles";

#[test]
fn is_cloud_url_classifies_correctly() {
    assert!(is_cloud_url("s3://bucket/key"));
    assert!(is_cloud_url("s3a://bucket/key"));
    assert!(is_cloud_url("az://account/container/key"));
    assert!(is_cloud_url("gs://bucket/key"));
    assert!(!is_cloud_url("http://host/path"));
    assert!(!is_cloud_url("/local/path"));
}

#[tokio::test]
async fn open_and_get_tile_via_localfs_object_store() {
    use object_store::local::LocalFileSystem;
    use std::sync::Arc;

    assert!(
        std::path::Path::new(FIXTURE_PATH).exists(),
        "Run pmtiles_local first to generate {FIXTURE_PATH}"
    );

    let store = Arc::new(LocalFileSystem::new()) as Arc<dyn object_store::ObjectStore>;
    let abs = std::fs::canonicalize(FIXTURE_PATH).unwrap();
    let cfg = PmtilesConfig {
        id: "c".into(),
        path: format!("file://{}", abs.display()),
        serve_as: None,
        description: None,
    };
    let src = CloudPmtilesSource::open_with_store(cfg, store)
        .await
        .expect("open");
    assert_eq!(src.id(), "c");
    assert_eq!(src.tile_info().format, TileFormat::Mvt);
    let resp = src
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(!resp.data.is_empty());
}
