//! Public API test for source descriptors and style metadata.

use async_trait::async_trait;
use maptile_core::cache::ContentVersion;
use maptile_core::control::{SourceCatalog, SourceDescriptor, TileDriver};
use maptile_core::error::MaptileResult;
use maptile_core::sources::{
    TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
};
use maptile_core::styles::{LocalStyleStore, snapshot};
use std::fs;
use std::sync::Arc;

struct Stub(&'static str);

#[async_trait]
impl TileDriver for Stub {
    fn descriptor(&self) -> SourceDescriptor {
        let mut descriptor = SourceDescriptor::new(
            self.0,
            TileInfo {
                format: TileFormat::Mvt,
                encoding: TileEncoding::Uncompressed,
            },
            ContentVersion::from_input(self.0.as_bytes()),
        );
        descriptor.tilejson.name = Some(self.0.into());
        descriptor
    }

    async fn read(
        &self,
        _: TileCoord,
        _: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        unreachable!("catalog listing only reads metadata");
    }
}

#[tokio::test]
async fn source_descriptors_and_style_snapshot_preserve_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let style_path = tmp.path().join("style.json");
    fs::write(&style_path, br#"{"version":8,"name":"Bright"}"#).unwrap();
    let style_store = LocalStyleStore::new([("bright", style_path)]).unwrap();

    let source_catalog = SourceCatalog::default();
    let mut update = source_catalog.update();
    update.put(Arc::new(Stub("planet"))).unwrap();
    update.put(Arc::new(Stub("labels"))).unwrap();
    source_catalog.publish(update).unwrap();
    let sources = source_catalog.snapshot();
    let mut metadata: Vec<_> = sources
        .descriptors()
        .map(|source| {
            (
                source.id.as_ref(),
                source.tilejson.name.as_deref(),
                source.info.format,
            )
        })
        .collect();
    metadata.sort_by_key(|(id, _, _)| *id);
    assert_eq!(
        metadata,
        [
            ("labels", Some("labels"), TileFormat::Mvt),
            ("planet", Some("planet"), TileFormat::Mvt),
        ]
    );

    let styles = snapshot(&style_store, Some("https://map.example"), "")
        .await
        .unwrap();
    assert_eq!(styles.len(), 1);
    assert_eq!(styles[0].id, "bright");
    assert_eq!(styles[0].name, "Bright");
    assert_eq!(
        styles[0].url.as_deref(),
        Some("https://map.example/styles/bright/style.json")
    );
}
