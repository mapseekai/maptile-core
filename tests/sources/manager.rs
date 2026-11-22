use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::error::{MaptileError, MaptileResult};
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

#[test]
fn shared_cache_entries_are_validated_before_legacy_reads() {
    use maptile_core::{
        cache::{TileCache, TileCacheKey, hashing::canonical_render_hash},
        sources::TileEtagResponse,
        transport::encode_as,
    };
    use std::time::Duration;
    use tokio::sync::oneshot;

    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let cache = Arc::new(TileCache::new(1, None, None).unwrap());
            let manager = SourceManager::new().with_cache(cache.clone());
            let source = Stub::arc("archive");
            let coord = TileCoord { z: 0, x: 0, y: 0 };
            let key = TileCacheKey::new(
                source.id().into(),
                source.content_version(),
                coord.z,
                coord.x,
                coord.y,
                canonical_render_hash(None, None, 256, 1, 1),
            );
            manager.register(source);
            let mut tile = TileResponse {
                data: Bytes::from_static(b"invalid compressed bytes"),
                info: TileInfo::new(TileFormat::Mvt, TileEncoding::Gzip),
                last_modified: None,
            };
            cache.insert(key.clone(), tile.clone()).await;
            assert!(matches!(
                manager.get_tile("archive", coord, None).await,
                Err(MaptileError::Transcode(_))
            ));
            assert!(matches!(
                manager
                    .get_tile_with_etag("archive", coord, None, None)
                    .await,
                Err(MaptileError::Transcode(_))
            ));
            tile.data = encode_as(b"valid cache write", TileEncoding::Gzip).unwrap();
            cache.insert(key, tile.clone()).await;
            assert_eq!(
                manager.get_tile("archive", coord, None).await.unwrap().data,
                tile.data
            );

            let (started, ready) = oneshot::channel();
            let (release, done) = oneshot::channel();
            let blocked = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                let _ = done.blocking_recv();
            });
            ready.await.unwrap();
            let warm = tokio::time::timeout(
                Duration::from_secs(2),
                manager.get_tile_with_etag("archive", coord, None, None),
            )
            .await;
            release.send(()).unwrap();
            blocked.await.unwrap();
            let TileEtagResponse::Fresh { resp, .. } =
                warm.expect("warm read started a second decode").unwrap()
            else {
                panic!("expected the validated compressed tile");
            };
            assert_eq!(resp.data, tile.data);
            assert_eq!(resp.info, tile.info);
        });
}

#[test]
fn conditional_failure_only_retires_the_observed_instance() {
    let manager = SourceManager::new();
    let old = Stub::arc("a");
    let replacement = Stub::arc("a");
    assert_eq!(old.content_version(), replacement.content_version());
    manager.register(old.clone());
    manager.register(replacement.clone());

    assert!(!manager.mark_unavailable_if_current(&old));
    assert!(Arc::ptr_eq(&manager.require("a").unwrap(), &replacement));
    assert!(manager.mark_unavailable_if_current(&replacement));
    assert!(!manager.mark_unavailable_if_current(&replacement));
    assert!(matches!(
        manager.require("a"),
        Err(MaptileError::SourceUnavailable(_))
    ));
    manager.remove("a");
    assert!(!manager.mark_unavailable_if_current(&old));
    assert!(!manager.mark_unavailable_if_current(&replacement));
    assert!(!manager.mark_unavailable_if_current(&Stub::arc("missing")));
    assert!(matches!(
        manager.require("a"),
        Err(MaptileError::SourceNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn foreground_recovery_invokes_factory_only_for_an_eligible_unavailable_id() {
    let manager = SourceManager::new();
    let original = Stub::arc("a");
    manager.register(original.clone());
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let factory = || {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(Ok(Stub::arc("a")))
    };

    let ready = manager.recover_unavailable("a", factory).await.unwrap();
    assert!(Arc::ptr_eq(&ready, &original));
    assert!(matches!(
        manager.recover_unavailable("missing", factory).await,
        Err(MaptileError::SourceNotFound(_))
    ));
    assert!(manager.mark_unavailable_if_current(&original));
    assert!(matches!(
        manager.recover_unavailable("a", factory).await,
        Err(MaptileError::SourceUnavailable(_))
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let recovered = manager.recover_unavailable("a", factory).await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(&manager.require("a").unwrap(), &recovered));
    assert!(!manager.mark_unavailable_if_current(&original));
    assert!(!manager.is_unavailable("a"));
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

#[cfg(all(feature = "mbtiles", feature = "pmtiles"))]
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
