#![cfg(all(feature = "agtiff", feature = "raster"))]
//! Smoke tests for the async-geotiff-backed MosaicSource.
//!
//! Construction tests use inert URLs; reducer behavior uses tiled GeoTIFFs
//! generated in a temporary directory and injected through async-geotiff's
//! object-store backend. This keeps the full render path deterministic and
//! socket-free.

use std::sync::Arc;

use async_geotiff::io::{BlockCache, CacheConfig};
use gdal::DriverManager;
use gdal::raster::{Buffer, RasterCreationOptions};
use gdal::spatial_ref::SpatialRef;
use maptile_core::raster::style::{EffectiveStyle, RasterStyle};
use maptile_core::sources::{
    MosaicMember, MosaicPixelSelection, MosaicSource, MosaicSourceConfig, TileCoord, TileQuery,
    TileSource,
};
use object_store::local::LocalFileSystem;
use tempfile::TempDir;

fn make_block_cache() -> Arc<BlockCache> {
    Arc::new(BlockCache::new(CacheConfig::default()))
}

fn two_member_cfg(bounds: Option<[f64; 4]>) -> MosaicSourceConfig {
    MosaicSourceConfig {
        id: "mosaic-smoke".into(),
        members: vec![
            MosaicMember {
                url: "https://example.invalid/a.tif".into(),
                source_id: "cog.aaa".into(),
                bbox_wgs84: [-10.0, -10.0, 0.0, 0.0],
            },
            MosaicMember {
                url: "https://example.invalid/b.tif".into(),
                source_id: "cog.bbb".into(),
                bbox_wgs84: [0.0, 0.0, 10.0, 10.0],
            },
        ],
        pixel_selection: MosaicPixelSelection::First,
        bounds,
        srid: None,
        store: None,
    }
}

fn two_member_cfg_with_srid(bounds: Option<[f64; 4]>, srid: Option<i32>) -> MosaicSourceConfig {
    MosaicSourceConfig {
        srid,
        ..two_member_cfg(bounds)
    }
}

struct AlgorithmFixture {
    _dir: TempDir,
    source: MosaicSource,
    style: EffectiveStyle,
}

impl AlgorithmFixture {
    async fn new() -> Self {
        Self::from_values(&[&[10], &[90], &[20]]).await
    }

    async fn from_values(values: &[&[u8]]) -> Self {
        let dir = tempfile::tempdir().expect("temp fixture directory");
        for (index, value) in values.iter().enumerate() {
            write_constant_geotiff(dir.path(), &format!("value-{index}.tif"), value);
        }

        let members = values
            .iter()
            .enumerate()
            .map(|(index, _)| MosaicMember {
                url: format!("value-{index}.tif"),
                source_id: format!("fixture-{index}"),
                bbox_wgs84: [-180.0, -85.051_128_78, 180.0, 85.051_128_78],
            })
            .collect();
        let store = Arc::new(
            LocalFileSystem::new_with_prefix(dir.path()).expect("local object-store fixture"),
        );
        let source = MosaicSource::open(
            MosaicSourceConfig {
                id: "algorithm-fixture".into(),
                members,
                pixel_selection: MosaicPixelSelection::First,
                bounds: None,
                srid: None,
                store: Some(store),
            },
            make_block_cache(),
        )
        .await
        .expect("open algorithm fixture");
        let style = EffectiveStyle::from_raster_style(
            RasterStyle::from_value(serde_json::json!({
                "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/mosaic/v1.0.0/schema.json", "https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "gray", "bidx": [1]},
                "raster:tile": {"format": "png", "tilesize": 256},
                "raster:mosaic": {"pixel_selection": "first", "stage": "after_channels"}
            }))
            .unwrap(),
        )
        .unwrap();

        Self {
            _dir: dir,
            source,
            style,
        }
    }

    async fn rendered_center_value(&self, selection: MosaicPixelSelection) -> u8 {
        let mut document = self.style.raster_style().as_value().clone();
        document["raster:mosaic"]["pixel_selection"] = serde_json::json!(match selection {
            MosaicPixelSelection::First => "first",
            MosaicPixelSelection::Highest => "highest",
            MosaicPixelSelection::Lowest => "lowest",
            MosaicPixelSelection::Mean => "mean",
            MosaicPixelSelection::Median => "median",
        });
        self.render_document(document).await[0]
    }

    async fn render_document(&self, document: serde_json::Value) -> [u8; 4] {
        let style =
            EffectiveStyle::from_raster_style(RasterStyle::from_value(document).unwrap()).unwrap();
        let query = TileQuery {
            style: Some(&style),
            ..Default::default()
        };
        let response = self
            .source
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&query))
            .await
            .expect("render algorithm fixture");
        let rgba = image::load_from_memory(&response.data)
            .expect("decode rendered PNG")
            .to_rgba8();
        rgba.get_pixel(rgba.width() / 2, rgba.height() / 2).0
    }
}

fn write_constant_geotiff(dir: &std::path::Path, name: &str, values: &[u8]) {
    let path = dir.join(name);
    let driver = DriverManager::get_driver_by_name("GTiff").expect("GTiff driver");
    let options = RasterCreationOptions::from_iter([
        "TILED=YES",
        "BLOCKXSIZE=256",
        "BLOCKYSIZE=256",
        "COMPRESS=DEFLATE",
    ]);
    let mut dataset = driver
        .create_with_band_type_with_options::<u8, _>(&path, 256, 256, values.len(), &options)
        .expect("create tiled GeoTIFF");
    let north = 85.051_128_78;
    dataset
        .set_geo_transform(&[-180.0, 360.0 / 256.0, 0.0, north, 0.0, -2.0 * north / 256.0])
        .expect("set fixture geotransform");
    dataset
        .set_spatial_ref(&SpatialRef::from_epsg(4326).expect("EPSG:4326"))
        .expect("set fixture CRS");
    for (index, value) in values.iter().enumerate() {
        let mut pixels = Buffer::new((256, 256), vec![*value; 256 * 256]);
        dataset
            .rasterband(index + 1)
            .unwrap()
            .write((0, 0), (256, 256), &mut pixels)
            .unwrap();
    }
    dataset.flush_cache().expect("flush fixture GeoTIFF");
}

// --- Test 1: empty members → Err ---

#[tokio::test]
async fn empty_members_returns_err() {
    let cfg = MosaicSourceConfig {
        id: "empty".into(),
        members: vec![],
        pixel_selection: MosaicPixelSelection::First,
        bounds: None,
        srid: None,
        store: None,
    };
    let result = MosaicSource::open(cfg, make_block_cache()).await;
    assert!(
        result.is_err(),
        "open with zero members must return Err; got Ok"
    );
}

// --- Test 2: construction succeeds; TileJSON bounds = union of member bboxes ---

#[tokio::test]
async fn construction_succeeds_and_bounds_are_union() {
    let src = MosaicSource::open(
        two_member_cfg_with_srid(None, Some(3857)),
        make_block_cache(),
    )
    .await
    .expect("open with two members must succeed");

    // Union of [-10,-10,0,0] ∪ [0,0,10,10] = [-10,-10,10,10]
    let tj = src.tilejson();
    let b = tj.bounds.expect("TileJSON must have bounds");
    assert!((b.left - (-10.0)).abs() < 1e-9, "west={}", b.left);
    assert!((b.bottom - (-10.0)).abs() < 1e-9, "south={}", b.bottom);
    assert!((b.right - 10.0).abs() < 1e-9, "east={}", b.right);
    assert!((b.top - 10.0).abs() < 1e-9, "north={}", b.top);
}

// --- Test 3: explicit bounds override member-union ---

#[tokio::test]
async fn explicit_bounds_override_member_union() {
    let explicit = [-180.0, -90.0, 180.0, 90.0];
    let src = MosaicSource::open(two_member_cfg(Some(explicit)), make_block_cache())
        .await
        .expect("open must succeed");

    let b = src.tilejson().bounds.expect("TileJSON must have bounds");
    assert!((b.left - (-180.0)).abs() < 1e-9);
    assert!((b.top - 90.0).abs() < 1e-9);
}

// --- Test 4: explicit non-WGS84 bounds are projected to WGS84 for TileJSON ---

#[tokio::test]
async fn explicit_projected_bounds_are_wgs84_in_tilejson() {
    let web_mercator = [
        -1113194.9079327357,
        -1118889.9748579594,
        1113194.9079327357,
        1118889.9748579594,
    ];
    let src = MosaicSource::open(
        two_member_cfg_with_srid(Some(web_mercator), Some(3857)),
        make_block_cache(),
    )
    .await
    .expect("open must succeed");

    let tj = src.tilejson();
    let b = tj.bounds.expect("TileJSON must have bounds");
    assert!((b.left - (-10.0)).abs() < 1e-6, "west={}", b.left);
    assert!((b.bottom - (-10.0)).abs() < 1e-6, "south={}", b.bottom);
    assert!((b.right - 10.0).abs() < 1e-6, "east={}", b.right);
    assert!((b.top - 10.0).abs() < 1e-6, "north={}", b.top);
    let center = tj.center.expect("TileJSON must have center");
    assert!(
        center.longitude.abs() < 1e-6,
        "center lon={}",
        center.longitude
    );
    assert!(
        center.latitude.abs() < 1e-6,
        "center lat={}",
        center.latitude
    );
}

// --- Test 5: content_version differs for different source_id sets ---

#[tokio::test]
async fn content_version_is_stable_and_source_id_driven() {
    let src_a = MosaicSource::open(two_member_cfg(None), make_block_cache())
        .await
        .unwrap();

    let cfg_b = MosaicSourceConfig {
        id: "mosaic-b".into(),
        members: vec![MosaicMember {
            url: "https://example.invalid/c.tif".into(),
            source_id: "cog.ccc".into(),
            bbox_wgs84: [1.0, 1.0, 2.0, 2.0],
        }],
        pixel_selection: MosaicPixelSelection::First,
        bounds: None,
        srid: None,
        store: None,
    };
    let src_b = MosaicSource::open(cfg_b, make_block_cache()).await.unwrap();

    // Same id → same content_version
    let src_a2 = MosaicSource::open(two_member_cfg(None), make_block_cache())
        .await
        .unwrap();
    assert_eq!(
        src_a.content_version().to_hex(),
        src_a2.content_version().to_hex(),
        "same source_ids must yield identical content_version"
    );

    // Different member → different content_version
    assert_ne!(
        src_a.content_version().to_hex(),
        src_b.content_version().to_hex(),
        "different source_ids must yield different content_version"
    );
}

// --- Test 6: tile_info reports Webp by default ---

#[tokio::test]
async fn tile_info_defaults_to_webp() {
    use maptile_core::sources::TileFormat;
    let src = MosaicSource::open(two_member_cfg(None), make_block_cache())
        .await
        .unwrap();
    assert_eq!(src.tile_info().format, TileFormat::Webp);
}

#[tokio::test]
async fn request_selection_executes_all_async_geotiff_reducers() {
    let fixture = AlgorithmFixture::new().await;
    let mut actual = Vec::new();
    for selection in [
        MosaicPixelSelection::First,
        MosaicPixelSelection::Highest,
        MosaicPixelSelection::Lowest,
        MosaicPixelSelection::Mean,
        MosaicPixelSelection::Median,
    ] {
        actual.push(fixture.rendered_center_value(selection).await);
    }

    assert_eq!(actual, [10, 90, 10, 40, 20]);
}

#[tokio::test]
async fn request_selection_overrides_style_and_partitions_the_cache() {
    use maptile_core::cache::{StyleHash, TileCache};
    use maptile_core::sources::manager::SourceManager;
    let fixture = AlgorithmFixture::new().await;
    let mut ranked = fixture.style.raster_style().as_value().clone();
    ranked["raster:mosaic"]["pixel_selection"] = "highest".into();
    ranked["raster:mosaic"]["rank_channel"] = 1.into();
    let style =
        EffectiveStyle::from_raster_style(RasterStyle::from_value(ranked).unwrap()).unwrap();
    let manager = SourceManager::new().with_cache(Arc::new(TileCache::new(1, None, None).unwrap()));
    let id = fixture.source.id().to_string();
    manager.register(Arc::new(fixture.source));
    let coord = TileCoord { z: 0, x: 0, y: 0 };
    for (selection, value) in [
        (None, 90),
        (Some(MosaicPixelSelection::First), 10),
        (None, 90),
    ] {
        let query = TileQuery {
            style: Some(&style),
            style_hash: Some(StyleHash::from_bytes([3; 32])),
            pixel_selection: selection,
            ..Default::default()
        };
        let tile = manager.get_tile(&id, coord, Some(&query)).await.unwrap();
        let rgba = image::load_from_memory(&tile.data).unwrap().to_rgba8();
        assert_eq!(
            rgba.get_pixel(rgba.width() / 2, rgba.height() / 2).0[0],
            value
        );
    }
}

#[tokio::test]
async fn native_mosaic_stage_rank_and_statistics_reach_the_renderer() {
    let fixture = AlgorithmFixture::from_values(&[&[100, 25], &[10, 5]]).await;
    let mut document = serde_json::json!({
        "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/mosaic/v1.0.0/schema.json", "https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "gray", "expression": "b1/b2"},
        "stretch": {"method": "linear", "rescale": [[0, 5]]},
        "raster:tile": {"format": "png", "tilesize": 64},
        "raster:mosaic": {"pixel_selection": "mean", "stage": "before_channels"}
    });
    assert_eq!(
        fixture.render_document(document.clone()).await,
        [187, 187, 187, 255]
    );
    document["raster:mosaic"]["stage"] = "after_channels".into();
    assert_eq!(
        fixture.render_document(document).await,
        [153, 153, 153, 255]
    );

    let fixture = AlgorithmFixture::from_values(&[&[100, 20, 30], &[10, 200, 40]]).await;
    let ranked = serde_json::json!({
        "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/mosaic/v1.0.0/schema.json", "https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "rgb", "bidx": [1, 2, 3]},
        "opacity": 0.5, "raster:tile": {"tilesize": 64},
        "raster:mosaic": {"pixel_selection": "highest", "stage": "after_channels", "rank_channel": 2}
    });
    assert_eq!(fixture.render_document(ranked).await, [10, 200, 40, 128]);

    let mut statistical = fixture.style.raster_style().as_value().clone();
    statistical["stretch"] = serde_json::json!({"method": "minmax"});
    assert_eq!(
        fixture.render_document(statistical).await,
        [128, 128, 128, 255]
    );
}
