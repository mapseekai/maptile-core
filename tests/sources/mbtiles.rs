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
    use maptile_core::sources::{
        TileCoord, TileEncoding, TileEtagResponse, TileFormat, TileSource,
    };
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
    async fn get_tile_with_etag_round_trip() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        let first = src
            .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, None)
            .await
            .unwrap();
        let etag = match first {
            TileEtagResponse::Fresh { etag, .. } => etag,
            _ => panic!("expected Fresh"),
        };
        let second = src
            .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, Some(&etag))
            .await
            .unwrap();
        assert!(matches!(second, TileEtagResponse::NotModified));
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
    async fn clone_source_works() {
        let fixture = fixture();
        let cfg = MbtilesConfig {
            id: "s".into(),
            path: fixture.path().to_path_buf(),
            serve_as: None,
            description: None,
        };
        let src = MbtilesSource::open(cfg).await.unwrap();
        let cloned = src.clone_source();
        assert_eq!(cloned.id(), "s");
        cloned
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
    }

    // Each test owns a complete PNG archive; no shared on-disk fixture or
    // create-if-missing race can hide a truncated payload from a previous run.
    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_archive_cannot_fill_the_new_generation_from_the_old_pool() {
        use maptile_core::cache::TileCache;
        use maptile_core::sources::manager::SourceManager;
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
        let manager =
            SourceManager::new().with_cache(Arc::new(TileCache::new(8, None, None).unwrap()));
        manager.register(old.clone());
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let before = manager.get_tile("stable", coord, None).await.unwrap().data;
        let replacement = fixture_with_color([255, 0, 0]);
        replacement.persist(first.path()).unwrap();
        assert_eq!(old.content_version(), version);
        let uncached = TileCoord { z: 1, x: 0, y: 1 };
        assert!(matches!(
            manager.get_tile("stable", uncached, None).await,
            Err(MaptileError::SourceUnavailable(_))
        ));
        let new = Arc::new(MbtilesSource::open(cfg).await.unwrap());
        assert_ne!(new.content_version(), version);
        let expected = new.get_tile(coord, None).await.unwrap().data;
        assert_ne!(before, expected);
        manager.register(new);
        assert_eq!(
            manager.get_tile("stable", coord, None).await.unwrap().data,
            expected
        );
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
            Err(MaptileError::SourceUnavailable(_))
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
