mod task14_probe {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use maptile_core::error::MaptileError;
    use maptile_core::sources::archive_inspection::{
        ArchiveSourceType, TileFamily, inspect_archive,
    };
    use maptile_core::sources::mbtiles::{MbtilesConfig, MbtilesSource};
    use maptile_core::sources::pmtiles::{LocalPmtilesSource, PmtilesConfig};
    use maptile_core::sources::{TileCoord, TileEncoding, TileFormat, TileSource};
    use pmtiles::{Compression as PmCompression, PmTilesWriter, TileCoord as PmCoord, TileType};
    use rusqlite::params;
    use serde::Serialize;
    use std::fs::File;
    use std::io::Write as _;
    use tempfile::NamedTempFile;

    #[derive(Serialize)]
    struct ProbeCase {
        name: &'static str,
        source_type: &'static str,
        family: &'static str,
        tile_format: &'static str,
        tile_encoding: &'static str,
        tilejson_json: String,
        tile_bytes: Vec<u8>,
        content_type: &'static str,
        extension: &'static str,
        error_type: &'static str,
    }

    fn mbtiles(
        format: &str,
        tile: &[u8],
        vector_layers: Option<&str>,
        omit: Option<&str>,
    ) -> NamedTempFile {
        let file = NamedTempFile::new().expect("temporary MBTiles");
        let conn = rusqlite::Connection::open(file.path()).expect("open MBTiles");
        conn.execute_batch(
        "CREATE TABLE metadata (name TEXT, value TEXT);
         CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB,
           PRIMARY KEY(zoom_level, tile_column, tile_row));",
    )
    .unwrap();
        for (name, value) in [
            ("name", "task14"),
            ("format", format),
            ("tile_size", "256"),
            ("bounds", "-10,-5,10,5"),
            ("center", "0,0,0"),
            ("minzoom", "0"),
            ("maxzoom", "0"),
        ] {
            if omit != Some(name) {
                conn.execute("INSERT INTO metadata VALUES (?1, ?2)", params![name, value])
                    .unwrap();
            }
        }
        if let Some(layers) = vector_layers {
            conn.execute(
                "INSERT INTO metadata VALUES ('json', ?1)",
                [format!(r#"{{"vector_layers":{layers}}}"#)],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO tiles VALUES (0,0,0,?1)", [tile])
            .unwrap();
        drop(conn);
        file
    }

    async fn inspect_mbtiles(
        name: &'static str,
        format: &str,
        tile: &[u8],
        layers: Option<&str>,
    ) -> ProbeCase {
        let file = mbtiles(format, tile, layers, None);
        let source = MbtilesSource::open(MbtilesConfig {
            id: name.into(),
            path: file.path().into(),
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();
        probe(name, "mbtiles", &source, ArchiveSourceType::Mbtiles).await
    }

    async fn probe(
        name: &'static str,
        source_type: &'static str,
        source: &dyn TileSource,
        archive_type: ArchiveSourceType,
    ) -> ProbeCase {
        let inspection =
            inspect_archive(archive_type, source.tilejson(), source.tile_info()).unwrap();
        let response = source
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        let (family, tile_format, content_type, extension) =
            match (inspection.family, inspection.format) {
                (TileFamily::Vector, TileFormat::Mvt) => {
                    ("vector", "mvt", "application/vnd.mapbox-vector-tile", "pbf")
                }
                (TileFamily::Raster, TileFormat::Png) => ("raster", "png", "image/png", "png"),
                (TileFamily::Raster, TileFormat::Jpeg) => ("raster", "jpeg", "image/jpeg", "jpg"),
                (TileFamily::Raster, TileFormat::Webp) => ("raster", "webp", "image/webp", "webp"),
                other => panic!("unsupported probe classification: {other:?}"),
            };
        let tile_encoding = match inspection.encoding {
            TileEncoding::Gzip => "gzip",
            _ => "identity",
        };
        ProbeCase {
            name,
            source_type,
            family,
            tile_format,
            tile_encoding,
            tilejson_json: serde_json::to_string(&inspection.tilejson).unwrap(),
            tile_bytes: response.data.to_vec(),
            content_type,
            extension,
            error_type: "",
        }
    }

    fn raster_pmtiles() -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        let output = File::create(file.path()).unwrap();
        let mut writer = PmTilesWriter::new(TileType::Png)
            .tile_compression(PmCompression::None)
            .internal_compression(PmCompression::None)
            .metadata(r#"{"name":"task14","tile_size":256}"#)
            .create(output)
            .unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(256, 256)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        writer
            .add_tile(PmCoord::new(0, 0, 0).unwrap(), png.get_ref())
            .unwrap();
        writer.finalize().unwrap();
        file
    }

    #[tokio::test]
    async fn emits_shared_archive_inspection_and_native_byte_probe() {
        let Ok(output) = std::env::var("MAPSEEK_TASK14_PROBE_OUTPUT") else {
            return;
        };
        let png = inspect_mbtiles("png-mbtiles", "png", b"png-native-bytes", None).await;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"mvt-native-bytes").unwrap();
        let gzip = encoder.finish().unwrap();
        let mvt = inspect_mbtiles(
            "gzip-mvt-mbtiles",
            "pbf",
            &gzip,
            Some(r#"[{"id":"roads","fields":{}}]"#),
        )
        .await;
        let pm_file = raster_pmtiles();
        if let Ok(path) = std::env::var("MAPSEEK_LIVE_PMTILES_OUTPUT") {
            std::fs::copy(pm_file.path(), path).unwrap();
        }
        if let Ok(path) = std::env::var("MAPSEEK_LIVE_VECTOR_PMTILES_OUTPUT") {
            let mut writer = PmTilesWriter::new(TileType::Mvt)
                .tile_compression(PmCompression::None)
                .internal_compression(PmCompression::None)
                .metadata(r#"{"name":"live-vector","vector_layers":[{"id":"roads","fields":{}}]}"#)
                .create(File::create(path).unwrap())
                .unwrap();
            // A valid empty MVT layer named roads, extent 4096, version 2.
            writer
                .add_tile(
                    PmCoord::new(0, 0, 0).unwrap(),
                    b"\x1a\x0c\x0a\x05roads\x28\x80\x20\x78\x02",
                )
                .unwrap();
            writer.finalize().unwrap();
        }
        let pm_source = LocalPmtilesSource::open(PmtilesConfig {
            id: "raster-pmtiles".into(),
            path: pm_file.path().to_string_lossy().into_owned(),
            serve_as: None,
            description: None,
        })
        .await
        .unwrap();
        let pm = probe(
            "raster-pmtiles",
            "pmtiles",
            &pm_source,
            ArchiveSourceType::Pmtiles,
        )
        .await;

        let missing_file = mbtiles("png", b"missing", None, Some("bounds"));
        let missing = MbtilesSource::open(MbtilesConfig {
            id: "missing-required-metadata".into(),
            path: missing_file.path().into(),
            serve_as: None,
            description: None,
        })
        .await
        .expect_err("missing bounds must fail strict inspection");
        let error_type = match missing {
            MaptileError::InvalidMetadata(_) => "InvalidMetadata",
            other => panic!("missing metadata returned wrong error variant: {other:?}"),
        };
        let failed = ProbeCase {
            name: "missing-required-metadata",
            source_type: "mbtiles",
            family: "",
            tile_format: "",
            tile_encoding: "",
            tilejson_json: String::new(),
            tile_bytes: Vec::new(),
            content_type: "",
            extension: "png",
            error_type,
        };
        assert_eq!(failed.error_type, "InvalidMetadata");
        std::fs::write(
            output,
            serde_json::to_vec_pretty(&[png, mvt, pm, failed]).unwrap(),
        )
        .unwrap();
    }
}

mod archive_inspection {
    use maptile_core::sources::archive_inspection::{ArchiveSourceType, inspect_archive};
    use maptile_core::sources::{TileEncoding, TileFormat, TileInfo};
    use serde_json::{Value, json};

    fn metadata(vector: bool) -> tilejson::TileJSON {
        let mut value = json!({
            "tilejson": "3.0.0", "name": "roads", "tiles": ["https://archive.invalid/{z}/{x}/{y}"],
            "scheme": "tms", "bounds": [1.0, 2.0, 3.0, 4.0], "center": [2.0, 3.0, 7],
            "minzoom": 0, "maxzoom": 14, "description": "Known description",
            "attribution": "Known attribution", "generator": "tippecanoe",
            "private_metadata": {"path": "/not-for-public-output"}, "format": "untrusted"
        });
        if vector {
            value["vector_layers"] = json!([{
                "id": "roads", "fields": {"class": "String"}, "minzoom": 0, "maxzoom": 14,
                "description": "Known layer", "generator": "custom", "private_metadata": {"x": 1}
            }]);
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn archive_normalization_matches_gateway_contract() {
        let mut fixtures: Vec<Value> = Vec::new();
        for source in [ArchiveSourceType::Mbtiles, ArchiveSourceType::Pmtiles] {
            for (format, expected) in [
                (TileFormat::Mvt, None),
                (TileFormat::Png, Some("png")),
                (TileFormat::Jpeg, Some("jpeg")),
                (TileFormat::Webp, Some("webp")),
            ] {
                let vector = expected.is_none();
                let input = metadata(vector);
                let inspected = inspect_archive(
                    source,
                    &input,
                    TileInfo {
                        format,
                        encoding: TileEncoding::Uncompressed,
                    },
                )
                .unwrap();
                let output = serde_json::to_value(&inspected.tilejson).unwrap();
                assert!(output.get("generator").is_none());
                assert!(output.get("private_metadata").is_none());
                assert_eq!(output["tiles"], json!([]));
                assert_eq!(output["scheme"], "xyz");
                assert_eq!(output["bounds"], json!([1.0, 2.0, 3.0, 4.0]));
                assert_eq!(output["center"], json!([2.0, 3.0, 7]));
                assert_eq!(output["description"], "Known description");
                assert_eq!(output["attribution"], "Known attribution");
                if let Some(expected) = expected {
                    assert_eq!(output["format"], expected);
                    assert!(output.get("vector_layers").is_none());
                } else {
                    assert!(output.get("format").is_none());
                    let layer = &output["vector_layers"][0];
                    assert_eq!(layer["id"], "roads");
                    assert_eq!(layer["fields"], json!({"class": "String"}));
                    assert_eq!(layer["description"], "Known layer");
                    assert!(layer.get("generator").is_none());
                    assert!(layer.get("private_metadata").is_none());
                }
                // Normalization must not modify the caller's original metadata.
                assert_eq!(input.other["generator"], "tippecanoe");
                assert_eq!(input.scheme.as_deref(), Some("tms"));
                let repeated = inspect_archive(
                    source,
                    &inspected.tilejson,
                    TileInfo {
                        format,
                        encoding: TileEncoding::Uncompressed,
                    },
                )
                .unwrap();
                assert_eq!(inspected.tilejson, repeated.tilejson);
                fixtures.push(json!({
                "source_type": match source { ArchiveSourceType::Mbtiles => "mbtiles", ArchiveSourceType::Pmtiles => "pmtiles" },
                "format": expected.unwrap_or("mvt"), "tilejson": output
            }));
            }
        }
        if let Ok(path) = std::env::var("MAPSEEK_ARCHIVE_CONTRACT_OUTPUT") {
            std::fs::write(path, serde_json::to_vec_pretty(&fixtures).unwrap()).unwrap();
        }
    }

    #[test]
    fn archive_normalization_keeps_required_metadata_validation() {
        let input = metadata(true);
        let info = TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        };
        let mut missing_bounds = input.clone();
        missing_bounds.bounds = None;
        assert!(inspect_archive(ArchiveSourceType::Mbtiles, &missing_bounds, info).is_err());
        let mut missing_center = input.clone();
        missing_center.center = None;
        assert!(inspect_archive(ArchiveSourceType::Mbtiles, &missing_center, info).is_ok());
        let mut invalid_zooms = input.clone();
        invalid_zooms.minzoom = Some(20);
        assert!(inspect_archive(ArchiveSourceType::Pmtiles, &invalid_zooms, info).is_err());
        let mut missing_layers = input;
        missing_layers.vector_layers = None;
        assert!(inspect_archive(ArchiveSourceType::Pmtiles, &missing_layers, info).is_err());
    }
}
