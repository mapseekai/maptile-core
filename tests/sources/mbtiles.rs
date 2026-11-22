mod mbtiles_tools_smoke {
    //! Smoke test: the MBTiles tooling surface (Mbtiles, Metadata, MbtilesCopier,
    //! validation::*, UpdateZoomType, ...) is reachable through maptile-core's
    //! `sources::mbtiles::tools` module and `prelude::mbtiles_tools` shortcut.
    //! Compile-only / type-resolution check.

    use maptile_core::prelude::mbtiles_tools as mt;
    use maptile_core::sources::mbtiles::tools::{
        AggHashType, IntegrityCheckType, MbtType, Mbtiles, MbtilesCopier, MbtilesPool, Metadata,
        PatchType, UpdateZoomType, invert_y_value,
    };

    #[test]
    fn mbtiles_tools_surface_resolves() {
        fn _accepts<T>() {}
        _accepts::<Mbtiles>();
        _accepts::<MbtilesPool>();
        _accepts::<Metadata>();
        _accepts::<MbtilesCopier>();
        _accepts::<UpdateZoomType>();
        _accepts::<AggHashType>();
        _accepts::<IntegrityCheckType>();
        _accepts::<MbtType>();
        _accepts::<PatchType>();

        // Prelude shortcut resolves to the same module.
        _accepts::<mt::Mbtiles>();
        _accepts::<mt::MbtilesCopier>();
        _accepts::<mt::Metadata>();
    }

    #[test]
    fn invert_y_value_round_trip_known_examples() {
        assert_eq!(invert_y_value(0, 0), 0);
        assert_eq!(invert_y_value(1, 0), 1);
        assert_eq!(invert_y_value(2, 1), 2);
        assert_eq!(invert_y_value(2, 0), 3);
    }
}

mod mbtiles {
    use maptile_core::error::MaptileError;
    use maptile_core::sources::mbtiles::{MbtilesConfig, MbtilesSource};
    use maptile_core::sources::{TileCoord, TileEncoding, TileFormat, TileSource};
    use std::path::PathBuf;

    #[tokio::test]
    async fn open_succeeds_on_valid_file() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.expect("open");
        assert_eq!(src.id(), "s");
    }

    #[tokio::test]
    async fn shutdown_waits_for_cancelled_archive_inspection() {
        use maptile_core::{
            cache::{ContentVersion, TileCache},
            control::{CatalogService, DiscoveredDataset, Inventory, TileDriver},
        };
        use std::sync::Arc;
        use tokio::time::{Duration, Instant};

        let fixture = fixture();
        let writer = rusqlite::Connection::open(fixture.path()).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let service = CatalogService::new(TileCache::new(4, None, None).unwrap());
        let cfg = MbtilesConfig {
            id: "locked".into(),
            path: fixture.path().into(),
            serve_as: None,
            description: None,
        };
        let inventory = [DiscoveredDataset {
            id: "locked".into(),
            dataset: "locked.mbtiles".into(),
            revision: ContentVersion::from_input(b"test"),
        }];
        let mut reconcile = Box::pin(service.catalog().reconcile(
            0,
            "files",
            Inventory::Complete(&inventory),
            |_| {
                let cfg = cfg.clone();
                async move { Ok(Arc::new(MbtilesSource::open(cfg).await?) as Arc<dyn TileDriver>) }
            },
        ));
        assert!(futures_util::poll!(reconcile.as_mut()).is_pending());
        drop(reconcile);
        assert!(matches!(
            service
                .catalog()
                .shutdown(Instant::now() + Duration::from_millis(100))
                .await,
            Err(MaptileError::Deadline)
        ));
        writer.execute_batch("ROLLBACK").unwrap();
        service
            .catalog()
            .shutdown(Instant::now() + Duration::from_secs(3))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn open_fails_on_missing_file() {
        let cfg = MbtilesConfig {
            id: "x".into(),
            path: PathBuf::from("tests/fixtures/does-not-exist.mbtiles"),
            serve_as: None,
            description: None,
        };
        let err = MbtilesSource::open(cfg).await.unwrap_err();
        assert!(matches!(err, MaptileError::Io(_)));
    }

    #[tokio::test]
    async fn tilejson_reflects_metadata() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        let tj = src.tilejson();
        assert_eq!(tj.minzoom, Some(0));
        assert_eq!(tj.maxzoom, Some(2));
        assert_eq!(tj.attribution.as_deref(), Some("test"));
    }

    #[tokio::test]
    async fn tile_info_reflects_format() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        let info = src.tile_info();
        assert_eq!(info.format, TileFormat::Png);
        assert_eq!(info.encoding, TileEncoding::Internal);
    }

    #[tokio::test]
    async fn get_tile_returns_bytes_for_existing_coord() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        let resp = src
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert!(!resp.data.is_empty());
        assert_eq!(resp.info.format, TileFormat::Png);
    }

    #[tokio::test]
    async fn png_archive_rejects_raster_style_before_layers_and_batch_reads() {
        use maptile_core::{
            cache::{StyleHash, TileCache},
            control::CatalogService,
            error::MaptileResult,
            raster::style::EffectiveStyle,
            service::{
                ReadTarget, TileLayer, TileOutcome, TileService, TileStack, ValidatedTileRequest,
            },
            sources::TileQuery,
        };
        use std::sync::Arc;

        struct UnreachableLayer;
        #[async_trait::async_trait]
        impl TileLayer for UnreachableLayer {
            async fn read(
                &self,
                _: &ValidatedTileRequest<'_>,
                _: TileService<'_>,
            ) -> MaptileResult<TileOutcome> {
                panic!("style admission must precede layers")
            }
        }

        let fixture = fixture();
        let source = Arc::new(
            MbtilesSource::open(MbtilesConfig {
                id: "archive".into(),
                path: fixture.path().to_path_buf(),
                serve_as: None,
                description: None,
            })
            .await
            .unwrap(),
        );
        assert_eq!(source.tile_info().format, TileFormat::Png);
        let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
        let mut update = service.catalog().update();
        update.put(source.clone()).unwrap();
        service.catalog().publish(update).unwrap();
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let target = ReadTarget::new("archive", "public");
        let stack = TileStack::builder(service.clone()).build();
        let original = stack.read(target.at(coord)).await.unwrap();
        let layered = TileStack::builder(service).layer(UnreachableLayer).build();
        let document = raster_style::Style::from_json(
            r#"{"version":"3.0","renderer":{"type":"gray","bidx":[1]}}"#,
        )
        .unwrap();
        let style = EffectiveStyle::from_raster_style(document).unwrap();

        for query in [
            TileQuery {
                style: Some(&style),
                ..Default::default()
            },
            TileQuery {
                style_hash: Some(StyleHash::from_bytes([7; 32])),
                ..Default::default()
            },
        ] {
            let styled = ReadTarget {
                query: Some(&query),
                ..target
            };
            let errors = [
                layered.read(styled.at(coord)).await.unwrap_err(),
                stack.read_batch(styled, &[coord]).await.unwrap_err(),
                source.get_tile(coord, Some(&query)).await.unwrap_err(),
                source
                    .read_tile_batch(&[coord], Some(&query))
                    .await
                    .unwrap_err(),
            ];
            assert!(
                errors
                    .into_iter()
                    .all(|error| matches!(error, MaptileError::Unsupported(_)))
            );
        }
        let warm = stack.read(target.at(coord)).await.unwrap();
        assert_eq!(warm.tile().unwrap().data, original.tile().unwrap().data);
    }

    #[tokio::test]
    async fn get_tile_returns_not_found_for_missing_coord() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        let err = src
            .get_tile(TileCoord { z: 5, x: 9, y: 9 }, None)
            .await
            .unwrap_err();
        assert!(matches!(err, MaptileError::TileNotFound(5, 9, 9)));
    }

    #[tokio::test]
    async fn serve_as_overrides_format_only() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: Some(TileFormat::Webp),
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        assert_eq!(src.tile_info().format, TileFormat::Webp);
        let resp = src
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(resp.info.format, TileFormat::Webp);
        // Bytes are unchanged: still the on-disk PNG signature
        assert_eq!(&resp.data[..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[tokio::test]
    async fn republishing_the_same_archive_handle_keeps_its_pool_readable() {
        use maptile_core::{
            cache::TileCache,
            control::CatalogService,
            service::{ReadTarget, TileStack},
        };
        use std::sync::Arc;

        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let expected = src.clone().get_tile(coord, None).await.unwrap().data;
        let handle = Arc::new(src);
        let service = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
        let stack = TileStack::builder(service.clone()).build();
        for scope in ["before", "after"] {
            let mut update = service.catalog().update();
            update.put(handle.clone()).unwrap();
            service.catalog().publish(update).unwrap();
            let outcome = stack
                .read(ReadTarget::new("s", scope).at(coord))
                .await
                .unwrap();
            assert_eq!(outcome.tile().unwrap().data, expected);
        }
    }

    // Each test owns a complete PNG archive; no shared on-disk fixture or
    // create-if-missing race can hide a truncated payload from a previous run.
    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_archive_cannot_fill_the_new_generation_from_the_old_pool() {
        use maptile_core::cache::TileCache;
        use maptile_core::control::CatalogService;
        use maptile_core::service::{ReadTarget, TileStack};
        use std::sync::Arc;

        let first = fixture();
        let cfg = MbtilesConfig {
            id: "stable".into(),
            path: first.path().to_owned(),
            serve_as: None,
            description: None,
        };
        let old = Arc::new(MbtilesSource::open(cfg.clone()).await.unwrap());
        let version = old.content_version();
        let service = Arc::new(CatalogService::new(TileCache::new(8, None, None).unwrap()));
        let mut update = service.catalog().update();
        update.put(old.clone()).unwrap();
        service.catalog().publish(update).unwrap();
        let stack = TileStack::builder(service.clone()).build();
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let target = ReadTarget::new("stable", "public");
        let before = stack
            .read(target.at(coord))
            .await
            .unwrap()
            .tile()
            .unwrap()
            .data
            .clone();
        let replacement = fixture_with_color([255, 0, 0]);
        replacement.persist(first.path()).unwrap();
        assert_eq!(old.content_version(), version);
        let uncached = TileCoord { z: 1, x: 0, y: 1 };
        assert!(matches!(
            stack.read(target.at(uncached)).await,
            Err(MaptileError::VersionChanged(_))
        ));
        let new = Arc::new(MbtilesSource::open(cfg).await.unwrap());
        assert_ne!(new.content_version(), version);
        let expected = new.get_tile(coord, None).await.unwrap().data;
        assert_ne!(before, expected);
        let mut update = service.catalog().update();
        update.put(new).unwrap();
        service.catalog().publish(update).unwrap();
        assert_eq!(
            stack
                .read(target.at(coord))
                .await
                .unwrap()
                .tile()
                .unwrap()
                .data
                .clone(),
            expected
        );
    }

    #[tokio::test]
    async fn relative_path_survives_chdir_in_subprocess() {
        const CHILD: &str = "MAPTILE_MBTILES_CWD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            fixture()
                .persist(dir.path().join("relative.mbtiles"))
                .unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mbtiles::relative_path_survives_chdir_in_subprocess",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let writer = rusqlite::Connection::open("relative.mbtiles").unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        let source = MbtilesSource::open(MbtilesConfig {
            id: "relative".into(),
            path: "relative.mbtiles".into(),
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let expected = source.get_tile(coord, None).await.unwrap().data;
        let other_dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(other_dir.path()).unwrap();
        assert_eq!(source.get_tile(coord, None).await.unwrap().data, expected);
        assert_eq!(
            source.clone().get_tile(coord, None).await.unwrap().data,
            expected
        );
        writer
            .execute_batch("UPDATE tiles SET tile_data = x'00' WHERE zoom_level = 0")
            .unwrap();
        assert!(matches!(
            source.get_tile(coord, None).await,
            Err(MaptileError::VersionChanged(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retargeting_opening_symlink_requires_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let first = fixture();
        let second = fixture_with_color([255, 0, 0]);
        let alias = dir.path().join("alias.mbtiles");
        std::os::unix::fs::symlink(first.path(), &alias).unwrap();
        let cfg = MbtilesConfig {
            id: "alias".into(),
            path: alias.clone(),
            serve_as: None,
            description: None,
        };
        let old = MbtilesSource::open(cfg.clone()).await.unwrap();
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let before = old.get_tile(coord, None).await.unwrap().data;
        let replacement = dir.path().join("replacement.mbtiles");
        std::os::unix::fs::symlink(second.path(), &replacement).unwrap();
        std::fs::rename(replacement, alias).unwrap();
        assert!(matches!(
            old.get_tile(coord, None).await,
            Err(MaptileError::VersionChanged(_))
        ));
        let new = MbtilesSource::open(cfg).await.unwrap();
        assert_ne!(new.get_tile(coord, None).await.unwrap().data, before);
    }

    #[tokio::test]
    async fn wal_commit_changes_archive_identity() {
        let file = fixture();
        let writer = rusqlite::Connection::open(file.path()).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        let cfg = MbtilesConfig {
            id: "wal".into(),
            path: file.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let before = MbtilesSource::open(cfg.clone()).await.unwrap();
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let replacement = fixture_with_color([128, 0, 0]);
        let data: Vec<u8> = rusqlite::Connection::open(replacement.path())
            .unwrap()
            .query_row(
                "SELECT tile_data FROM tiles WHERE zoom_level=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        writer
            .execute("UPDATE tiles SET tile_data=?1 WHERE zoom_level=0", [&data])
            .unwrap();
        assert!(matches!(
            before.get_tile(coord, None).await,
            Err(MaptileError::VersionChanged(_))
        ));
        let after = MbtilesSource::open(cfg).await.unwrap();
        assert_ne!(before.content_version(), after.content_version());
        assert_eq!(
            after.get_tile(coord, None).await.unwrap().data.as_ref(),
            data.as_slice()
        );
    }

    fn fixture() -> tempfile::NamedTempFile {
        fixture_with_color([0, 0, 0])
    }

    fn fixture_with_color(color: [u8; 3]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temporary MBTiles fixture");
        let conn = rusqlite::Connection::open(file.path()).expect("open fixture");
        conn.execute_batch(
            "CREATE TABLE metadata (name TEXT, value TEXT);
             CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB,
                                 PRIMARY KEY(zoom_level, tile_column, tile_row));
             INSERT INTO metadata VALUES('name','sample');
             INSERT INTO metadata VALUES('format','png');
             INSERT INTO metadata VALUES('tile_size','256');
             INSERT INTO metadata VALUES('minzoom','0');
             INSERT INTO metadata VALUES('maxzoom','2');
             INSERT INTO metadata VALUES('bounds','-180,-85,180,85');
             INSERT INTO metadata VALUES('attribution','test');",
        ).expect("create fixture schema");
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(256, 256, image::Rgb(color)))
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode complete tile");
        for (zoom, x, y) in [(0, 0, 0), (1, 0, 0), (2, 1, 1)] {
            conn.execute(
                "INSERT INTO tiles VALUES (?1,?2,?3,?4)",
                rusqlite::params![zoom, x, y, png.get_ref()],
            )
            .expect("insert tile");
        }
        drop(conn);
        file
    }

    mod archive_inspection {
        use super::*;
        use maptile_core::sources::archive_inspection::{
            ArchiveSourceType, TileFamily, inspect_archive,
        };
        use rusqlite::params;
        use std::io::Write as _;
        use tempfile::NamedTempFile;

        const REQUIRED_METADATA: &[(&str, &str)] = &[
            ("name", "strict fixture"),
            ("format", "png"),
            ("tile_size", "256"),
            ("bounds", "-10,-5,10,5"),
            ("center", "0,0,0"),
            ("minzoom", "0"),
            ("maxzoom", "2"),
        ];

        fn fixture(
            metadata: &[(&str, &str)],
            tile_data: &[u8],
            vector_layers: Option<&str>,
        ) -> NamedTempFile {
            let file = NamedTempFile::new().expect("temporary MBTiles file");
            let conn = rusqlite::Connection::open(file.path()).expect("open fixture database");
            conn.execute_batch(
                "CREATE TABLE metadata (name TEXT, value TEXT);
             CREATE TABLE tiles (
                 zoom_level INTEGER,
                 tile_column INTEGER,
                 tile_row INTEGER,
                 tile_data BLOB,
                 PRIMARY KEY(zoom_level, tile_column, tile_row)
             );",
            )
            .expect("create MBTiles schema");
            for (name, value) in metadata {
                conn.execute(
                    "INSERT INTO metadata(name, value) VALUES (?1, ?2)",
                    params![name, value],
                )
                .expect("insert metadata");
            }
            if let Some(vector_layers) = vector_layers {
                conn.execute(
                    "INSERT INTO metadata(name, value) VALUES ('json', ?1)",
                    [format!(r#"{{"vector_layers":{vector_layers}}}"#)],
                )
                .expect("insert vector layer metadata");
            }
            conn.execute("INSERT INTO tiles VALUES (0, 0, 0, ?1)", [tile_data])
                .expect("insert tile");
            drop(conn);
            file
        }

        async fn open_fixture(file: &NamedTempFile) -> Result<MbtilesSource, MaptileError> {
            MbtilesSource::open(MbtilesConfig {
                id: "strict".into(),
                path: file.path().to_path_buf(),
                serve_as: None,
                description: None,
            })
            .await
        }

        fn metadata_with(format: &str) -> Vec<(&'static str, &str)> {
            REQUIRED_METADATA
                .iter()
                .copied()
                .map(|(key, value)| {
                    if key == "format" {
                        (key, format)
                    } else {
                        (key, value)
                    }
                })
                .collect()
        }

        async fn assert_raster_format(format: &str, expected: TileFormat) {
            let metadata = metadata_with(format);
            let file = fixture(&metadata, b"non-empty raster bytes", None);
            let source = open_fixture(&file).await.expect("strict raster archive");
            let inspection = inspect_archive(
                ArchiveSourceType::Mbtiles,
                source.tilejson(),
                source.tile_info(),
            )
            .unwrap();
            assert_eq!(inspection.family, TileFamily::Raster);
            assert_eq!(source.tile_info().format, expected);
            assert_eq!(source.tilejson().tiles, Vec::<String>::new());
            assert_eq!(source.tilejson().scheme.as_deref(), Some("xyz"));
            assert_eq!(
                source
                    .tilejson()
                    .other
                    .get("format")
                    .and_then(|v| v.as_str()),
                Some(format)
            );
        }

        #[tokio::test]
        async fn mbtiles_strict_png_metadata() {
            assert_raster_format("png", TileFormat::Png).await;
        }

        #[tokio::test]
        async fn mbtiles_strict_jpeg_metadata() {
            assert_raster_format("jpeg", TileFormat::Jpeg).await;
        }

        #[tokio::test]
        async fn mbtiles_strict_webp_metadata() {
            assert_raster_format("webp", TileFormat::Webp).await;
        }

        #[tokio::test]
        async fn mbtiles_gzip_mvt_uses_declared_format_and_detected_encoding() {
            let metadata = metadata_with("pbf");
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(b"vector tile payload").unwrap();
            let gzip_tile = encoder.finish().unwrap();
            let layers = r#"[{"id":"roads","fields":{}}]"#;
            let file = fixture(&metadata, &gzip_tile, Some(layers));
            let source = open_fixture(&file).await.expect("strict vector archive");
            assert_eq!(source.tile_info().format, TileFormat::Mvt);
            assert_eq!(source.tile_info().encoding, TileEncoding::Gzip);
            assert!(!source.tilejson().other.contains_key("format"));
        }

        #[tokio::test]
        async fn mixed_encodings_and_empty_tiles_survive_catalog_cache_hits() {
            use maptile_core::{
                cache::TileCache,
                control::CatalogService,
                service::{ReadTarget, TileOutcome, TileStack},
            };
            use std::sync::Arc;

            let mvt = b"\x1a\x0c\x0a\x05roads\x28\x80\x20\x78\x02";
            let gzip = maptile_core::transport::encode_as(mvt, TileEncoding::Gzip).unwrap();
            let file = fixture(
                &metadata_with("pbf"),
                &gzip,
                Some(r#"[{"id":"roads","fields":{}}]"#),
            );
            {
                let conn = rusqlite::Connection::open(file.path()).unwrap();
                conn.execute("INSERT INTO tiles VALUES (1, 0, 1, ?1)", [&mvt[..]])
                    .unwrap();
                conn.execute("INSERT INTO tiles VALUES (1, 0, 0, X'')", [])
                    .unwrap();
            }
            let source = Arc::new(open_fixture(&file).await.unwrap());
            let engine = Arc::new(CatalogService::new(TileCache::new(32, None, None).unwrap()));
            let mut update = engine.catalog().update();
            update.put(source.clone()).unwrap();
            engine.catalog().publish(update).unwrap();
            let stack = TileStack::builder(engine).build();
            let target = ReadTarget::new("strict", "public");

            for (coord, encoding, bytes) in [
                (
                    TileCoord { z: 0, x: 0, y: 0 },
                    TileEncoding::Gzip,
                    gzip.as_ref(),
                ),
                (
                    TileCoord { z: 1, x: 0, y: 0 },
                    TileEncoding::Uncompressed,
                    &mvt[..],
                ),
                (
                    TileCoord { z: 1, x: 0, y: 1 },
                    TileEncoding::Uncompressed,
                    &[][..],
                ),
            ] {
                let direct = source.get_tile(coord, None).await.unwrap();
                assert_eq!(direct.data, bytes);
                assert_eq!(direct.info.encoding, encoding);
                for _ in 0..2 {
                    let outcome = stack.read(target.at(coord)).await.unwrap();
                    let cached = outcome.tile().unwrap();
                    assert_eq!(cached.data, direct.data);
                    assert_eq!(cached.info, direct.info);
                }
            }
            assert!(matches!(
                stack
                    .read(target.at(TileCoord { z: 1, x: 1, y: 1 }))
                    .await
                    .unwrap(),
                TileOutcome::NoTile
            ));
        }

        #[tokio::test]
        async fn decoded_and_stored_payloads_obey_the_tile_limit() {
            use maptile_core::transport::MAX_DECODED_TILE_BYTES;
            use std::io::Read;

            let file = fixture(
                &metadata_with("pbf"),
                &[],
                Some(r#"[{"id":"roads","fields":{}}]"#),
            );
            {
                let conn = rusqlite::Connection::open(file.path()).unwrap();
                let length = MAX_DECODED_TILE_BYTES + 1;
                conn.execute("INSERT INTO tiles VALUES (1, 0, 1, zeroblob(?1))", [length])
                    .unwrap();
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                std::io::copy(&mut std::io::repeat(0).take(length as u64), &mut encoder).unwrap();
                conn.execute(
                    "INSERT INTO tiles VALUES (1, 0, 0, ?1)",
                    [encoder.finish().unwrap()],
                )
                .unwrap();
                conn.execute("INSERT INTO tiles VALUES (1, 1, 0, 'not a BLOB')", [])
                    .unwrap();
            }
            let source = open_fixture(&file).await.unwrap();
            assert!(
                source
                    .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
                    .await
                    .unwrap()
                    .data
                    .is_empty()
            );
            for y in [0, 1] {
                let error = source
                    .get_tile(TileCoord { z: 1, x: 0, y }, None)
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    MaptileError::PayloadTooLarge {
                        limit: MAX_DECODED_TILE_BYTES
                    }
                ));
            }
            assert!(matches!(
                source.get_tile(TileCoord { z: 1, x: 1, y: 1 }, None).await,
                Err(MaptileError::InvalidMetadata(_))
            ));
        }

        #[tokio::test]
        async fn mbtiles_missing_required_identity_and_bounds_are_rejected() {
            for missing in ["name", "format", "bounds"] {
                let metadata: Vec<_> = REQUIRED_METADATA
                    .iter()
                    .copied()
                    .filter(|(key, _)| *key != missing)
                    .collect();
                let file = fixture(&metadata, b"tile", None);
                let error = open_fixture(&file)
                    .await
                    .expect_err("missing required metadata must fail");
                assert!(
                    matches!(error, MaptileError::InvalidMetadata(_)),
                    "{missing}: {error:?}"
                );
            }
        }

        #[tokio::test]
        async fn mbtiles_missing_center_is_accepted() {
            let metadata: Vec<_> = REQUIRED_METADATA
                .iter()
                .copied()
                .filter(|(key, _)| *key != "center")
                .collect();
            let file = fixture(&metadata, b"tile", None);
            let source = open_fixture(&file)
                .await
                .expect("center is optional metadata");
            assert!(source.tilejson().center.is_none());
        }

        #[tokio::test]
        async fn mbtiles_invalid_bounds_and_center_are_rejected() {
            for (key, invalid) in [
                ("bounds", "10,-5,-10,5"),
                ("bounds", "-10,-5,-10,5"),
                ("bounds", "-10,5,10,5"),
                ("bounds", "-181,-5,10,5"),
                ("bounds", "NaN,-5,10,5"),
                ("center", "11,0,0"),
                ("center", "NaN,0,0"),
            ] {
                let metadata: Vec<_> = REQUIRED_METADATA
                    .iter()
                    .copied()
                    .map(|(candidate, value)| {
                        if candidate == key {
                            (candidate, invalid)
                        } else {
                            (candidate, value)
                        }
                    })
                    .collect();
                let file = fixture(&metadata, b"tile", None);
                let error = open_fixture(&file)
                    .await
                    .expect_err("invalid spatial metadata must fail");
                assert!(
                    matches!(error, MaptileError::InvalidMetadata(_)),
                    "{key}={invalid}: {error:?}"
                );
            }
        }

        #[tokio::test]
        async fn mbtiles_zoom_metadata_is_normalized_to_actual_tile_range() {
            let metadata = metadata_with("png");
            let file = fixture(&metadata, b"tile", None);
            let source = open_fixture(&file).await.expect("valid fixture");
            assert_eq!(source.tilejson().minzoom, Some(0));
            assert_eq!(source.tilejson().maxzoom, Some(0));
            assert_eq!(source.tilejson().center.map(|center| center.zoom), Some(0));
        }

        #[tokio::test]
        async fn mbtiles_vector_requires_nonempty_vector_layers() {
            let metadata = metadata_with("mvt");
            for layers in [
                None,
                Some("[]"),
                Some(r#"[{"id":"","fields":{}}]"#),
                Some(r#"[{"id":"roads"}]"#),
                Some(r#"[{"id":"roads","fields":{}},{"id":" roads ","fields":{}}]"#),
            ] {
                let file = fixture(&metadata, b"vector tile", layers);
                let error = open_fixture(&file)
                    .await
                    .expect_err("invalid vector_layers must fail");
                assert!(
                    matches!(error, MaptileError::InvalidMetadata(_)),
                    "{error:?}"
                );
            }
        }

        #[tokio::test]
        async fn mbtiles_declared_png_does_not_validate_tile_magic() {
            let file = fixture(REQUIRED_METADATA, b"definitely not png", None);
            let source = open_fixture(&file)
                .await
                .expect("metadata is authoritative");
            assert_eq!(source.tile_info().format, TileFormat::Png);
        }
        #[tokio::test]
        async fn sparse_archive_resolves_dimensions_from_first_nonempty_row() {
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(512, 512)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            let metadata: Vec<_> = REQUIRED_METADATA
                .iter()
                .copied()
                .filter(|(key, _)| !matches!(*key, "tile_size" | "center"))
                .collect();
            let file = fixture(&metadata, png.get_ref(), None);
            let conn = rusqlite::Connection::open(file.path()).unwrap();
            conn.execute(
                "UPDATE tiles SET zoom_level=18, tile_column=123456, tile_row=98765",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO tiles VALUES(0, 0, 0, X'')", [])
                .unwrap();
            drop(conn);
            let source = open_fixture(&file).await.unwrap();
            assert_eq!(source.tilejson().other["tile_size"], 512);
        }
    }
}
