use maptile_core::error::MaptileError;
use maptile_core::sources::pmtiles::{LocalPmtilesSource, PmtilesConfig};
use maptile_core::sources::{TileCoord, TileEtagResponse, TileFormat, TileSource};

const FIXTURE_PATH: &str = "tests/fixtures/sample.pmtiles";

/// Generate the fixture if it doesn't exist.
async fn ensure_fixture() {
    let path = std::path::Path::new(FIXTURE_PATH);
    if path.exists() {
        return;
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_minimal_pmtiles_fixture(path).await;
}

async fn write_minimal_pmtiles_fixture(path: &std::path::Path) {
    use pmtiles::{Compression, PmTilesWriter, TileCoord as PmtCoord, TileType};
    use std::fs::File;

    let file = File::create(path).expect("create fixture file");
    // Use Mvt tile type with no tile compression so tiles are stored as raw bytes.
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .tile_compression(Compression::None)
        .internal_compression(Compression::None)
        .metadata(r#"{"name":"test","vector_layers":[{"id":"sample","fields":{}}]}"#)
        .create(file)
        .expect("create PMTiles writer");

    // Add a single tile at z=0, x=0, y=0 with non-empty content.
    let coord = PmtCoord::new(0, 0, 0).unwrap();
    writer.add_tile(coord, b"test-tile-data").expect("add tile");

    writer.finalize().expect("finalize PMTiles archive");
}

#[tokio::test]
async fn open_succeeds_on_valid_file() {
    ensure_fixture().await;
    let cfg = PmtilesConfig {
        id: "s".into(),
        path: FIXTURE_PATH.into(),
        serve_as: None,
        description: None,
    };
    let src = LocalPmtilesSource::open(cfg).await.expect("open");
    assert_eq!(src.id(), "s");
}

#[tokio::test]
async fn compact_archive_smaller_than_initial_range_serves_tiles() {
    let file = tempfile::NamedTempFile::new().unwrap();
    write_minimal_pmtiles_fixture(file.path()).await;
    let mut archive = std::fs::read(file.path()).unwrap();
    let field = |bytes: &[u8], at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let root_end = (field(&archive, 8) + field(&archive, 16)) as usize;
    let metadata_offset = field(&archive, 24) as usize;
    let padding = (metadata_offset - root_end) as u64;
    for offset_field in [24, 40, 56] {
        let offset = field(&archive, offset_field);
        if offset >= metadata_offset as u64 {
            archive[offset_field..offset_field + 8]
                .copy_from_slice(&(offset - padding).to_le_bytes());
        }
    }
    archive.drain(root_end..metadata_offset);
    assert!(archive.len() < 16_384);
    std::fs::write(file.path(), archive).unwrap();
    let source = LocalPmtilesSource::open(PmtilesConfig {
        id: "compact".into(),
        path: file.path().to_string_lossy().into(),
        serve_as: None,
        description: None,
    })
    .await
    .unwrap();
    assert_eq!(source.tilejson().name.as_deref(), Some("test"));
    let tile = source
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert_eq!(tile.data.as_ref(), b"test-tile-data");
}

#[tokio::test]
async fn tiles_are_found_through_leaf_directories() {
    use pmtiles::{Compression, PmTilesWriter, TileId, TileType};
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .tile_compression(Compression::None)
        .internal_compression(Compression::None)
        .metadata(r#"{"name":"leaves","vector_layers":[{"id":"leaves","fields":{}}]}"#)
        .create(file.reopen().unwrap())
        .unwrap();
    for id in 0u64..5_000 {
        writer
            .add_tile(TileId::new(id).unwrap().into(), &id.to_le_bytes())
            .unwrap();
    }
    writer.finalize().unwrap();
    let bytes = std::fs::read(file.path()).unwrap();
    assert_ne!(u64::from_le_bytes(bytes[48..56].try_into().unwrap()), 0);

    let source = LocalPmtilesSource::open(PmtilesConfig {
        id: "leaves".into(),
        path: file.path().to_string_lossy().into_owned(),
        serve_as: None,
        description: None,
    })
    .await
    .unwrap();
    let expected = 4_999u64;
    let coord: pmtiles::TileCoord = TileId::new(expected).unwrap().into();
    let (z, x, y) = (coord.z(), coord.x(), coord.y());
    let tile = source.get_tile(TileCoord { z, x, y }, None).await.unwrap();
    assert_eq!(tile.data.as_ref(), expected.to_le_bytes());
}

#[tokio::test]
async fn open_fails_on_missing_file() {
    let cfg = PmtilesConfig {
        id: "x".into(),
        path: "tests/fixtures/missing.pmtiles".into(),
        serve_as: None,
        description: None,
    };
    let err = LocalPmtilesSource::open(cfg).await.unwrap_err();
    assert!(matches!(
        err,
        MaptileError::Io(_) | MaptileError::Backend(_)
    ));
}

#[tokio::test]
async fn tile_info_reflects_format() {
    ensure_fixture().await;
    let cfg = PmtilesConfig {
        id: "s".into(),
        path: FIXTURE_PATH.into(),
        serve_as: None,
        description: None,
    };
    let src = LocalPmtilesSource::open(cfg).await.unwrap();
    assert_eq!(src.tile_info().format, TileFormat::Mvt);
}

#[tokio::test]
async fn get_tile_returns_bytes_for_existing_coord() {
    ensure_fixture().await;
    let cfg = PmtilesConfig {
        id: "s".into(),
        path: FIXTURE_PATH.into(),
        serve_as: None,
        description: None,
    };
    let src = LocalPmtilesSource::open(cfg).await.unwrap();
    let resp = src
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(!resp.data.is_empty());
}

#[tokio::test]
async fn get_tile_returns_not_found_for_missing_coord() {
    ensure_fixture().await;
    let cfg = PmtilesConfig {
        id: "s".into(),
        path: FIXTURE_PATH.into(),
        serve_as: None,
        description: None,
    };
    let src = LocalPmtilesSource::open(cfg).await.unwrap();
    let err = src
        .get_tile(TileCoord { z: 5, x: 9, y: 9 }, None)
        .await
        .unwrap_err();
    assert!(matches!(err, MaptileError::TileNotFound(5, 9, 9)));
}

#[tokio::test]
async fn get_tile_with_etag_round_trip() {
    ensure_fixture().await;
    let cfg = PmtilesConfig {
        id: "s".into(),
        path: FIXTURE_PATH.into(),
        serve_as: None,
        description: None,
    };
    let src = LocalPmtilesSource::open(cfg).await.unwrap();
    let first = src
        .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, None)
        .await
        .unwrap();
    let etag = match first {
        TileEtagResponse::Fresh { etag, .. } => etag,
        _ => panic!("expected Fresh response"),
    };
    let second = src
        .get_tile_with_etag(TileCoord { z: 0, x: 0, y: 0 }, None, Some(&etag))
        .await
        .unwrap();
    assert!(matches!(second, TileEtagResponse::NotModified));
}

#[tokio::test]
async fn serve_as_overrides_format() {
    ensure_fixture().await;
    let cfg = PmtilesConfig {
        id: "s".into(),
        path: FIXTURE_PATH.into(),
        serve_as: Some(TileFormat::Webp),
        description: None,
    };
    let src = LocalPmtilesSource::open(cfg).await.unwrap();
    assert_eq!(src.tile_info().format, TileFormat::Webp);
}

#[tokio::test]
async fn clone_source_works() {
    ensure_fixture().await;
    let cfg = PmtilesConfig {
        id: "s".into(),
        path: FIXTURE_PATH.into(),
        serve_as: None,
        description: None,
    };
    let src = LocalPmtilesSource::open(cfg).await.unwrap();
    let cloned = src.clone_source();
    assert_eq!(cloned.id(), "s");
}

mod archive_inspection {
    use super::*;
    use maptile_core::sources::archive_inspection::{
        ArchiveSourceType, TileFamily, inspect_archive,
    };
    use pmtiles::{Compression, PmTilesWriter, TileCoord as PmtCoord, TileType};
    use std::fs::File;
    use tempfile::NamedTempFile;

    fn fixture(tile_type: TileType) -> NamedTempFile {
        let file = NamedTempFile::new().expect("temporary PMTiles file");
        let output = File::create(file.path()).expect("create fixture file");
        let metadata = match tile_type {
            TileType::Mvt => r#"{"name":"test","vector_layers":[{"id":"roads","fields":{}}]}"#,
            _ => r#"{"name":"test","tile_size":256}"#,
        };
        let mut writer = PmTilesWriter::new(tile_type)
            .tile_compression(Compression::None)
            .internal_compression(Compression::None)
            .metadata(metadata)
            .create(output)
            .expect("create PMTiles writer");
        writer
            .add_tile(PmtCoord::new(0, 0, 0).unwrap(), b"tile")
            .expect("add tile");
        writer.finalize().expect("finalize PMTiles archive");
        file
    }

    async fn open_fixture(file: &NamedTempFile) -> Result<LocalPmtilesSource, MaptileError> {
        LocalPmtilesSource::open(PmtilesConfig {
            id: "strict".into(),
            path: file.path().to_string_lossy().into_owned(),
            serve_as: None,
            description: None,
        })
        .await
    }

    #[tokio::test]
    async fn pmtiles_supported_headers_classify_exactly() {
        for (tile_type, family, format) in [
            (TileType::Mvt, TileFamily::Vector, TileFormat::Mvt),
            (TileType::Png, TileFamily::Raster, TileFormat::Png),
            (TileType::Jpeg, TileFamily::Raster, TileFormat::Jpeg),
            (TileType::Webp, TileFamily::Raster, TileFormat::Webp),
        ] {
            let file = fixture(tile_type);
            let source = open_fixture(&file).await.expect("supported PMTiles header");
            let inspection = inspect_archive(
                ArchiveSourceType::Pmtiles,
                source.tilejson(),
                source.tile_info(),
            )
            .unwrap();
            assert_eq!(source.tile_info().format, format);
            assert_eq!(inspection.family, family);
            assert_eq!(source.tilejson().tiles, Vec::<String>::new());
            assert_eq!(source.tilejson().scheme.as_deref(), Some("xyz"));
        }
    }

    #[tokio::test]
    async fn pmtiles_unknown_tile_type_is_rejected() {
        for tile_type in [TileType::Unknown, TileType::Avif] {
            let file = fixture(tile_type);
            let error = open_fixture(&file)
                .await
                .expect_err("unsupported PMTiles header must fail");
            assert!(
                matches!(error, MaptileError::InvalidMetadata(_)),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn pmtiles_unknown_tile_compression_is_rejected() {
        let file = NamedTempFile::new().expect("temporary PMTiles file");
        let output = File::create(file.path()).expect("create fixture file");
        let writer = PmTilesWriter::new(TileType::Mvt)
            .tile_compression(Compression::Unknown)
            .internal_compression(Compression::None)
            .metadata(r#"{"name":"test","vector_layers":[{"id":"roads","fields":{}}]}"#)
            .create(output)
            .expect("create PMTiles writer");
        writer.finalize().expect("finalize PMTiles archive");

        let error = open_fixture(&file)
            .await
            .expect_err("unknown PMTiles tile compression must fail");
        assert!(
            matches!(error, MaptileError::InvalidMetadata(_)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn sparse_high_zoom_archive_resolves_size_from_occupied_directory_entry() {
    use pmtiles::{Compression, PmTilesWriter, TileCoord as PmtCoord, TileType};
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(512, 512)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let mut writer = PmTilesWriter::new(TileType::Png)
        .tile_compression(Compression::None)
        .internal_compression(Compression::None)
        .metadata(r#"{"name":"sparse"}"#)
        .create(std::fs::File::create(file.path()).unwrap())
        .unwrap();
    writer
        .add_tile(PmtCoord::new(18, 123456, 98765).unwrap(), png.get_ref())
        .unwrap();
    writer.finalize().unwrap();
    let source = LocalPmtilesSource::open(PmtilesConfig {
        id: "sparse".into(),
        path: file.path().to_string_lossy().into(),
        serve_as: None,
        description: None,
    })
    .await
    .unwrap();
    assert_eq!(source.tilejson().other["tile_size"], 512);
}
