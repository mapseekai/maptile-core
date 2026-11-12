//! Read the repository's synthetic PMTiles fixture through a shared tile cache.

use maptile_core::prelude::*;
use maptile_core::sources::pmtiles::{LocalPmtilesSource, PmtilesConfig};
use std::{path::Path, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> MaptileResult<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.pmtiles");
    let cache = Arc::new(TileCache::new(64, Some(Duration::from_secs(3600)), None)?);
    let manager = SourceManager::new().with_cache(cache);
    let source = LocalPmtilesSource::open(PmtilesConfig {
        id: "sample".into(),
        path: path.to_string_lossy().into_owned(),
        serve_as: None,
        description: Some("Synthetic tile bytes for the getting-started example".into()),
    })
    .await?;

    manager.register(Arc::new(source));
    let result = manager
        .get_tile("sample", TileCoord { z: 0, x: 0, y: 0 }, None)
        .await;
    manager.close_source("sample").await?;
    let tile = result?;
    println!("sample 0/0/0: {} bytes", tile.data.len());
    Ok(())
}
