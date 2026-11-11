use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::error::MaptileResult;
use maptile_core::sources::manager::SourceManager;
use maptile_core::sources::{TileCoord, TileQuery, TileResponse, TileSource};
use maptile_core::sources::{TileEncoding, TileFormat, TileInfo};
use std::sync::Arc;
use tilejson::{TileJSON, tilejson};

struct Stub {
    id: String,
    tj: TileJSON,
}

impl Stub {
    fn arc(id: &str) -> Arc<dyn TileSource> {
        Arc::new(Stub {
            id: id.to_string(),
            tj: tilejson! { tiles: vec![] },
        })
    }
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
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        Ok(TileResponse {
            data: Bytes::new(),
            info: self.tile_info(),
            last_modified: None,
        })
    }
}

#[test]
fn register_and_get() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    let got = m.get("a").expect("present");
    assert_eq!(got.id(), "a");
}

#[test]
fn get_missing_returns_none() {
    let m = SourceManager::new();
    assert!(m.get("nope").is_none());
}

#[test]
fn remove_returns_evicted_value() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    let removed = m.remove("a").expect("present");
    assert_eq!(removed.id(), "a");
    assert!(m.get("a").is_none());
}

#[test]
fn list_ids_returns_all_registered() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    m.register(Stub::arc("b"));
    let mut ids = m.list_ids();
    ids.sort();
    assert_eq!(ids, vec!["a", "b"]);
}

#[test]
fn snapshot_tilejson_has_one_per_source() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    m.register(Stub::arc("b"));
    assert_eq!(m.snapshot_tilejson().len(), 2);
}

#[test]
fn register_overwrites_same_id() {
    let m = SourceManager::new();
    m.register(Stub::arc("a"));
    m.register(Stub::arc("a"));
    assert_eq!(m.list_ids().len(), 1);
}

#[tokio::test]
async fn concurrent_register_and_get_is_safe() {
    let m = Arc::new(SourceManager::new());
    let mut handles = vec![];
    for i in 0..50u32 {
        let m2 = m.clone();
        handles.push(tokio::spawn(async move {
            m2.register(Stub::arc(&format!("s{i}")));
        }));
        let m3 = m.clone();
        handles.push(tokio::spawn(async move {
            let _ = m3.get(&format!("s{i}"));
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert_eq!(m.list_ids().len(), 50);
}

mod manager_interop {
    use maptile_core::sources::manager::SourceManager;
    use maptile_core::sources::mbtiles::{MbtilesConfig, MbtilesSource};
    use maptile_core::sources::pmtiles::{LocalPmtilesSource, PmtilesConfig};
    use maptile_core::sources::{TileCoord, TileFormat};
    use std::sync::Arc;

    #[tokio::test]
    async fn co_registered_sources_serve_independently() {
        let fixtures = tempfile::tempdir().unwrap();
        let mbtiles_path = fixtures.path().join("sample.mbtiles");
        let pmtiles_path = fixtures.path().join("sample.pmtiles");
        let connection = rusqlite::Connection::open(&mbtiles_path).unwrap();
        connection.execute_batch(
        "CREATE TABLE metadata (name TEXT, value TEXT);
         CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB);
         INSERT INTO metadata VALUES ('name','sample'),('format','png'),('minzoom','0'),('maxzoom','0'),('bounds','-180,-85,180,85'),('tile_size','256');
"
    ).unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(256, 256)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        connection
            .execute(
                "INSERT INTO tiles VALUES (0,0,0,?1)",
                rusqlite::params![png.into_inner()],
            )
            .unwrap();
        drop(connection);
        let mut writer = pmtiles::PmTilesWriter::new(pmtiles::TileType::Mvt)
            .tile_compression(pmtiles::Compression::None)
            .internal_compression(pmtiles::Compression::None)
            .metadata(r#"{"name":"sample","vector_layers":[{"id":"layer","fields":{}}]}"#)
            .create(std::fs::File::create(&pmtiles_path).unwrap())
            .unwrap();
        writer
            .add_tile(pmtiles::TileCoord::new(0, 0, 0).unwrap(), b"test-tile-data")
            .unwrap();
        writer.finalize().unwrap();

        let mb = MbtilesSource::open(MbtilesConfig {
            id: "m".into(),
            path: mbtiles_path,
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();

        let pm = LocalPmtilesSource::open(PmtilesConfig {
            id: "p".into(),
            path: pmtiles_path.to_string_lossy().into_owned(),
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();

        let mgr = SourceManager::new();
        mgr.register(Arc::new(mb));
        mgr.register(Arc::new(pm));

        let mut ids = mgr.list_ids();
        ids.sort();
        assert_eq!(ids, vec!["m", "p"]);

        let mb_handle = mgr.get("m").unwrap();
        assert_eq!(mb_handle.tile_info().format, TileFormat::Png);
        let mb_resp = mb_handle
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert!(!mb_resp.data.is_empty());

        let pm_handle = mgr.get("p").unwrap();
        assert_eq!(pm_handle.tile_info().format, TileFormat::Mvt);
        let pm_resp = pm_handle
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert!(!pm_resp.data.is_empty());

        assert_eq!(mgr.snapshot_tilejson().len(), 2);
    }
}
