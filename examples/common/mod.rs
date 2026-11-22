use maptile_core::{
    cache::TileCache,
    control::CatalogService,
    error::MaptileResult,
    sources::pmtiles::{LocalPmtilesSource, PmtilesConfig},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.pmtiles")
}

pub async fn catalog(path: &Path) -> MaptileResult<Arc<CatalogService>> {
    let source = LocalPmtilesSource::open(PmtilesConfig {
        id: "sample".into(),
        path: path.to_string_lossy().into_owned(),
        serve_as: None,
        description: None,
    })
    .await?;
    let service = Arc::new(CatalogService::new(TileCache::new(16, None, None)?));
    let mut update = service.catalog().update();
    update.put(Arc::new(source))?;
    service.catalog().publish(update)?;
    Ok(service)
}
