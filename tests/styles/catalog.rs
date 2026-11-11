//! End-to-end test: build a discovery layout, register sources + stores
//! in [`SourceManager`], and snapshot a single [`Catalog`] payload.

use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::error::MaptileResult;
use maptile_core::prelude::*;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use tilejson::tilejson;

fn write(p: &Path, b: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
}

/// Trivial in-memory MVT source for the catalog test.
struct Stub {
    id: String,
    tj: TileJSON,
}

#[async_trait]
impl TileSource for Stub {
    fn id(&self) -> &str {
        &self.id
    }
    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }
    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        }
    }
    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(Stub {
            id: self.id.clone(),
            tj: self.tj.clone(),
        })
    }
    async fn get_tile(
        &self,
        _xyz: TileCoord,
        _q: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        Ok(TileResponse {
            data: Bytes::from_static(b""),
            info: self.tile_info(),
            last_modified: None,
        })
    }
}

#[tokio::test]
async fn catalog_aggregates_sources_styles() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    write(
        &root.join("styles/bright/style.json"),
        br#"{"version":8,"name":"Bright"}"#,
    );
    // Source manager with two stub sources.
    let mgr = SourceManager::new();
    mgr.register(Arc::new(Stub {
        id: "planet".into(),
        tj: tilejson! { tiles: vec![] },
    }));
    mgr.register(Arc::new(Stub {
        id: "labels".into(),
        tj: tilejson! { tiles: vec![] },
    }));

    let style_store =
        LocalStyleStore::new::<_, &str>(vec![("bright", root.join("styles/bright/style.json"))])
            .unwrap();
    let cat = mgr
        .snapshot_catalog(Some(&style_store), Some("https://map.example"))
        .await
        .unwrap();

    // Sources sorted by id.
    let ids: Vec<&str> = cat.sources.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["labels", "planet"]);

    // One style with a populated url.
    assert_eq!(cat.styles.len(), 1);
    assert_eq!(cat.styles[0].id, "bright");
    assert_eq!(cat.styles[0].name, "Bright");
    assert_eq!(
        cat.styles[0].url.as_deref(),
        Some("https://map.example/styles/bright/style.json")
    );
}
