#![cfg(feature = "agtiff")]
//! Smoke tests for the async-geotiff-backed MosaicSource.
//!
//! Construction tests use inert URLs; reducer behavior uses tiled GeoTIFFs
//! generated in a temporary directory and injected through async-geotiff's
//! object-store backend. This keeps the full render path deterministic and
//! socket-free.

use std::sync::Arc;

use async_geotiff::io::{BlockCache, CacheConfig};
#[cfg(feature = "raster")]
use gdal::DriverManager;
#[cfg(feature = "raster")]
use gdal::raster::{Buffer, RasterCreationOptions};
#[cfg(feature = "raster")]
use gdal::spatial_ref::SpatialRef;
#[cfg(feature = "raster")]
use maptile_core::raster::style::{EffectiveStyle, RasterStyle};
use maptile_core::sources::{
    MosaicMember, MosaicPixelSelection, MosaicSource, MosaicSourceConfig, TileSource,
};
use maptile_core::sources::{TileCoord, TileQuery};
use object_store::local::LocalFileSystem;
#[cfg(feature = "raster")]
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

#[cfg(feature = "raster")]
struct AlgorithmFixture {
    _dir: TempDir,
    source: MosaicSource,
    style: EffectiveStyle,
}

#[cfg(feature = "raster")]
impl AlgorithmFixture {
    async fn new() -> Self {
        Self::from_values(&[&[10], &[90], &[20]]).await
    }

    async fn from_values(values: &[&[u8]]) -> Self {
        let dir = tempfile::tempdir().expect("temp fixture directory");
        for (index, value) in values.iter().enumerate() {
            write_constant_geotiff(
                dir.path(),
                &format!("value-{index}.tif"),
                value,
                [-180.0, -85.051_128_78, 180.0, 85.051_128_78],
                4326,
            );
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

#[cfg(feature = "raster")]
fn write_constant_geotiff(
    dir: &std::path::Path,
    name: &str,
    values: &[u8],
    [west, south, east, north]: [f64; 4],
    srid: u32,
) {
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
    dataset
        .set_geo_transform(&[
            west,
            (east - west) / 256.0,
            0.0,
            north,
            0.0,
            (south - north) / 256.0,
        ])
        .expect("set fixture geotransform");
    dataset
        .set_spatial_ref(&SpatialRef::from_epsg(srid).expect("fixture CRS"))
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

#[tokio::test]
async fn immutable_http_revision_preserves_member_order() {
    let src_a = MosaicSource::open_immutable(two_member_cfg(None), make_block_cache())
        .await
        .unwrap();
    let src_a2 = MosaicSource::open_immutable(two_member_cfg(None), make_block_cache())
        .await
        .unwrap();
    assert_eq!(src_a.content_version(), src_a2.content_version());

    let mut reordered = two_member_cfg(None);
    reordered.members.reverse();
    let src_b = MosaicSource::open_immutable(reordered, make_block_cache())
        .await
        .unwrap();
    assert_ne!(src_a.content_version(), src_b.content_version());
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

#[cfg(feature = "raster")]
#[tokio::test]
async fn large_mosaic_filters_before_cap_and_ignores_distant_broken_assets() {
    let dir = tempfile::tempdir().unwrap();
    let local = [0.0, 0.0, 22.5, 21.943_045_533_438_177];
    let distant = [-120.0, -40.0, -100.0, -20.0];
    // The only intersecting TIFF is projected; member extents remain WGS84.
    write_constant_geotiff(
        dir.path(),
        "local.tif",
        &[73],
        [0.0, 0.0, 2_504_688.542_848_655_5, 2_504_688.542_848_655_5],
        3857,
    );
    for i in 0..64 {
        write_constant_geotiff(
            dir.path(),
            &format!("distant-{i}.tif"),
            &[90],
            distant,
            4326,
        );
    }
    let store = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
    let make_config = |count: usize| {
        let mut members = vec![MosaicMember {
            url: "local.tif".into(),
            source_id: "local".into(),
            bbox_wgs84: local,
        }];
        members.extend((0..count).map(|i| MosaicMember {
            url: format!("distant-{i}.tif"),
            source_id: format!("distant-{i}"),
            bbox_wgs84: distant,
        }));
        MosaicSourceConfig {
            id: "large".into(),
            members,
            pixel_selection: MosaicPixelSelection::Mean,
            bounds: Some([
                -20_037_508.342_789_244,
                -20_037_508.342_789_244,
                20_037_508.342_789_244,
                20_037_508.342_789_244,
            ]),
            srid: Some(3857),
            store: Some(store.clone()),
        }
    };
    let style = AlgorithmFixture::from_values(&[&[73]]).await.style;
    let query = TileQuery {
        style: Some(&style),
        ..Default::default()
    };
    let coord = TileCoord { z: 4, x: 8, y: 7 };
    let single = MosaicSource::open(make_config(0), make_block_cache())
        .await
        .unwrap();
    let expected = single.get_tile(coord, Some(&query)).await.unwrap().data;
    let rgba = image::load_from_memory(&expected).unwrap().to_rgba8();
    assert_eq!(rgba.get_pixel(128, 128).0, [73, 73, 73, 255]);

    let large = MosaicSource::open(make_config(64), make_block_cache())
        .await
        .unwrap();
    assert_eq!(
        large.get_tile(coord, Some(&query)).await.unwrap().data,
        expected
    );
    // A fresh source/cache proves the distant bad file is never opened.
    std::fs::write(dir.path().join("distant-0.tif"), b"broken TIFF").unwrap();
    let mut cfg = make_config(64);
    cfg.members.push(MosaicMember {
        url: "missing.tif".into(),
        source_id: "missing".into(),
        bbox_wgs84: distant,
    });
    let source = MosaicSource::open(cfg, make_block_cache()).await.unwrap();
    assert_eq!(
        source.get_tile(coord, Some(&query)).await.unwrap().data,
        expected
    );

    // Truly overlapping candidates are still capped, even with a first reducer.
    let mut cfg = make_config(64);
    for member in &mut cfg.members {
        member.bbox_wgs84 = local;
    }
    let source = MosaicSource::open(cfg, make_block_cache()).await.unwrap();
    let error = source.get_tile(coord, Some(&query)).await.unwrap_err();
    assert!(error.to_string().contains("exceeding limit 64"), "{error}");
}

#[cfg(feature = "raster")]
#[tokio::test]
async fn missing_member_bbox_remains_eligible_and_expands_union() {
    let fixture = AlgorithmFixture::from_values(&[&[73]]).await;
    let source = MosaicSource::open(
        MosaicSourceConfig {
            id: "unknown-extent".into(),
            members: vec![MosaicMember {
                url: "value-0.tif".into(),
                source_id: "unknown".into(),
                bbox_wgs84: [0.0; 4],
            }],
            pixel_selection: MosaicPixelSelection::First,
            bounds: None,
            srid: Some(3857),
            store: Some(Arc::new(
                LocalFileSystem::new_with_prefix(fixture._dir.path()).unwrap(),
            )),
        },
        make_block_cache(),
    )
    .await
    .unwrap();
    let query = TileQuery {
        style: Some(&fixture.style),
        ..Default::default()
    };
    let coord = TileCoord { z: 0, x: 0, y: 0 };
    assert_eq!(
        source.get_tile(coord, Some(&query)).await.unwrap().data,
        fixture
            .source
            .get_tile(coord, Some(&query))
            .await
            .unwrap()
            .data
    );
    let b = source.tilejson().bounds.unwrap();
    assert_eq!(
        [b.left, b.bottom, b.right, b.top],
        [-180.0, -90.0, 180.0, 90.0]
    );
}

#[tokio::test]
async fn member_extents_partition_content_versions() {
    let a = MosaicSource::open_immutable(two_member_cfg(None), make_block_cache())
        .await
        .unwrap();
    let mut cfg = two_member_cfg(None);
    cfg.members[0].bbox_wgs84 = [-20.0, -20.0, -10.0, -10.0];
    let b = MosaicSource::open_immutable(cfg, make_block_cache())
        .await
        .unwrap();
    assert_ne!(a.content_version(), b.content_version());
}

#[cfg(feature = "raster")]
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

#[cfg(feature = "raster")]
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

#[cfg(feature = "raster")]
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

#[cfg(feature = "raster")]
#[tokio::test]
async fn near_full_member_longitude_span_renders_the_same_real_tile() {
    let dir = tempfile::tempdir().unwrap();
    write_constant_geotiff(
        dir.path(),
        "local.tif",
        &[73],
        [0.0, 0.0, 90.0, 66.51326044311186],
        4326,
    );
    let store = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
    let make_config = |east| MosaicSourceConfig {
        id: "near-full".into(),
        members: vec![MosaicMember {
            url: "local.tif".into(),
            source_id: "local".into(),
            bbox_wgs84: [-190.0, -85.0, east, 85.0],
        }],
        pixel_selection: MosaicPixelSelection::First,
        bounds: None,
        srid: None,
        store: Some(store.clone()),
    };
    let style = AlgorithmFixture::from_values(&[&[73]]).await.style;
    let query = TileQuery {
        style: Some(&style),
        ..Default::default()
    };
    let coord = TileCoord { z: 2, x: 2, y: 1 };
    let control = MosaicSource::open(make_config(170.0), make_block_cache())
        .await
        .unwrap();
    let actual = MosaicSource::open(make_config(169.999_999_999_999_94), make_block_cache())
        .await
        .unwrap();
    let expected = control.get_tile(coord, Some(&query)).await.unwrap().data;
    assert_eq!(
        image::load_from_memory(&expected)
            .unwrap()
            .to_rgba8()
            .get_pixel(128, 128)
            .0,
        [73, 73, 73, 255]
    );
    assert_eq!(
        actual.get_tile(coord, Some(&query)).await.unwrap().data,
        expected
    );
}

#[tokio::test]
async fn explicit_wgs84_bounds_reject_out_of_domain_latitudes() {
    for srid in [None, Some(4326), Some(0), Some(-1), Some(i32::MIN)] {
        for bounds in [
            [10.0, 100.0, 20.0, 120.0],
            [-10.0, -200.0, 10.0, 200.0],
            [0.0, (-90.0_f64).next_down(), 1.0, 0.0],
            [0.0, 0.0, 1.0, 90.0_f64.next_up()],
        ] {
            let result = MosaicSource::open(
                two_member_cfg_with_srid(Some(bounds), srid),
                make_block_cache(),
            )
            .await;
            match result {
                Err(maptile_core::error::MaptileError::Config(_)) => {}
                Ok(source) => panic!(
                    "invalid WGS84 accepted for {srid:?}: {}",
                    serde_json::to_string(source.tilejson()).unwrap()
                ),
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
    }
    assert!(matches!(
        MosaicSource::open(
            two_member_cfg_with_srid(Some([10.0, 100.0, 20.0, 120.0]), Some(4269)),
            make_block_cache()
        )
        .await,
        Err(maptile_core::error::MaptileError::Config(_))
    ));
}

#[tokio::test]
async fn explicit_bounds_publish_ordered_tilejson_coordinates_and_center() {
    let cases = [
        ([-180.0, -90.0, 180.0, 90.0], [-180.0, -90.0, 180.0, 90.0]),
        ([10.0, 90.0, 10.0, 90.0], [10.0, 90.0, 10.0, 90.0]),
        ([180.0, -90.0, 180.0, -90.0], [180.0, -90.0, 180.0, -90.0]),
        ([190.0, -10.0, 200.0, 10.0], [-170.0, -10.0, -160.0, 10.0]),
        ([-200.0, -10.0, -190.0, 10.0], [160.0, -10.0, 170.0, 10.0]),
        ([170.0, -10.0, 190.0, 10.0], [-180.0, -10.0, 180.0, 10.0]),
        ([170.0, -10.0, -170.0, 10.0], [-180.0, -10.0, 180.0, 10.0]),
        ([-190.0, -10.0, -180.0, 10.0], [170.0, -10.0, 180.0, 10.0]),
        ([180.0, -10.0, 190.0, 10.0], [-180.0, -10.0, -170.0, 10.0]),
        ([540.0, 10.0, 540.0, 10.0], [-180.0, 10.0, -180.0, 10.0]),
        ([190.0, -10.0, 550.0, 10.0], [-180.0, -10.0, 180.0, 10.0]),
        (
            [f64::MAX / 2.0, 1.0, f64::MAX, 2.0],
            [-180.0, 1.0, 180.0, 2.0],
        ),
    ];
    for srid in [None, Some(4326), Some(0), Some(-1)] {
        for (input, expected) in cases {
            let source = MosaicSource::open(
                two_member_cfg_with_srid(Some(input), srid),
                make_block_cache(),
            )
            .await
            .unwrap();
            let tj = source.tilejson();
            assert_eq!(tj.tilejson, "3.0.0");
            let b = tj.bounds.unwrap();
            assert_eq!(
                [b.left, b.bottom, b.right, b.top],
                expected,
                "{srid:?} {input:?}"
            );
            let c = tj.center.unwrap();
            assert!(c.longitude.is_finite() && (b.left..=b.right).contains(&c.longitude));
            assert!(c.latitude.is_finite() && (b.bottom..=b.top).contains(&c.latitude));
        }
    }
    let projected = MosaicSource::open(
        two_member_cfg_with_srid(Some([10.0, 100.0, 20.0, 120.0]), Some(3857)),
        make_block_cache(),
    )
    .await
    .unwrap();
    let b = projected.tilejson().bounds.unwrap();
    assert!((0.0..1.0).contains(&b.top));
    assert!((0.0..1.0).contains(&b.right));
}

#[cfg(feature = "raster")]
#[tokio::test]
async fn unwrapped_dateline_members_match_their_real_geotiff_extents() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
    let p = 180.0_f64;
    let style = AlgorithmFixture::from_values(&[&[73]]).await.style;
    let query = TileQuery {
        style: Some(&style),
        ..Default::default()
    };
    for (name, geometry, unwrapped) in [
        (
            "east-ulp",
            [p.next_down(), -10.0, p, 10.0],
            [(-p).next_down(), -10.0, -p, 10.0],
        ),
        (
            "west-ulp",
            [-p, -10.0, (-p).next_up(), 10.0],
            [p, -10.0, p.next_up(), 10.0],
        ),
        (
            "east-wide",
            [170.0, -10.0, 180.0, 10.0],
            [-190.0, -10.0, -180.0, 10.0],
        ),
        (
            "west-wide",
            [-180.0, -10.0, -170.0, 10.0],
            [180.0, -10.0, 190.0, 10.0],
        ),
    ] {
        let filename = format!("{name}.tif");
        write_constant_geotiff(dir.path(), &filename, &[73], geometry, 4326);
        let config = |bbox_wgs84| MosaicSourceConfig {
            id: name.into(),
            members: vec![MosaicMember {
                url: filename.clone(),
                source_id: name.into(),
                bbox_wgs84,
            }],
            pixel_selection: MosaicPixelSelection::First,
            bounds: None,
            srid: None,
            store: Some(store.clone()),
        };
        let actual = MosaicSource::open(config(unwrapped), make_block_cache())
            .await
            .unwrap();
        let control = MosaicSource::open(config(geometry), make_block_cache())
            .await
            .unwrap();
        assert_eq!(
            actual.tilejson().bounds,
            control.tilejson().bounds,
            "{name}"
        );
        assert_ne!(
            actual.content_version(),
            control.content_version(),
            "separate mosaic opens over an opaque store have distinct revisions for {name}"
        );
        for x in [0, 3] {
            let coord = TileCoord { z: 2, x, y: 1 };
            let actual = actual.get_tile(coord, Some(&query)).await;
            let expected = control.get_tile(coord, Some(&query)).await;
            if (name == "east-wide" && x == 3) || (name == "west-wide" && x == 0) {
                let rgba = image::load_from_memory(&actual.as_ref().unwrap().data)
                    .unwrap()
                    .to_rgba8();
                assert!(rgba.pixels().any(|p| p.0 == [73, 73, 73, 255]));
            }
            match (actual, expected) {
                (Ok(actual), Ok(expected)) => {
                    assert_eq!(actual.data, expected.data, "{name} {coord:?}")
                }
                (
                    Err(maptile_core::error::MaptileError::TileNotFound(..)),
                    Err(maptile_core::error::MaptileError::TileNotFound(..)),
                ) => {}
                (actual, expected) => {
                    panic!("{name} {coord:?}: actual={actual:?}, control={expected:?}")
                }
            }
        }
    }
}

#[cfg(feature = "raster")]
#[tokio::test]
async fn world_edge_candidates_attempt_object_reads() {
    let fixture = AlgorithmFixture::from_values(&[&[73]]).await;
    let query = TileQuery {
        style: Some(&fixture.style),
        ..Default::default()
    };
    for (x, bounds) in [
        (0, [-180.0, -10.0, (-180.0_f64).next_up(), 10.0]),
        (3, [180.0_f64.next_down(), -10.0, 180.0, 10.0]),
    ] {
        let source = MosaicSource::open(
            MosaicSourceConfig {
                id: format!("world-edge-{x}"),
                members: vec![MosaicMember {
                    url: "sliver.tif".into(),
                    source_id: "sliver".into(),
                    bbox_wgs84: bounds,
                }],
                pixel_selection: MosaicPixelSelection::First,
                bounds: None,
                srid: None,
                store: Some(Arc::new(object_store::memory::InMemory::new())),
            },
            make_block_cache(),
        )
        .await
        .unwrap();
        let error = source
            .get_tile(TileCoord { z: 2, x, y: 1 }, Some(&query))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, maptile_core::error::MaptileError::Backend(source) if source.to_string().contains("open asset sliver.tif")),
            "{x}: {error}"
        );
    }
}

#[path = "../common/raster.rs"]
mod raster_fixture;

#[tokio::test]
async fn mosaic_preserves_order_and_fills_nodata_without_gdal() {
    let dir = tempfile::tempdir().unwrap();
    raster_fixture::write_world_tiff(&dir.path().join("a.tif"), |pixel| {
        if pixel % 256 < 128 { 0 } else { 10 }
    });
    raster_fixture::write_world_tiff(&dir.path().join("b.tif"), |_| 90);
    let store = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
    let style = raster_fixture::gray_png_style();
    let query = TileQuery {
        style: Some(&style),
        ..Default::default()
    };

    for (names, west, east) in [
        (vec!["a.tif", "b.tif"], [90, 90, 90, 255], [10, 10, 10, 255]),
        (vec!["b.tif", "a.tif"], [90, 90, 90, 255], [90, 90, 90, 255]),
        (vec!["a.tif"], [0, 0, 0, 0], [10, 10, 10, 255]),
    ] {
        let members = names
            .into_iter()
            .map(|name| MosaicMember {
                url: name.into(),
                source_id: name.into(),
                bbox_wgs84: [-180.0, -85.0, 180.0, 85.0],
            })
            .collect();
        let source = MosaicSource::open(
            MosaicSourceConfig {
                id: "ordered".into(),
                members,
                store: Some(store.clone()),
                ..two_member_cfg(None)
            },
            make_block_cache(),
        )
        .await
        .unwrap();
        let tile = source
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&query))
            .await
            .unwrap();
        let rgba = image::load_from_memory(&tile.data).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(64, 128).0, west);
        assert_eq!(rgba.get_pixel(192, 128).0, east);
    }
}

#[tokio::test]
async fn export_holds_versioned_mosaic_assets_across_catalog_replacement() {
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        error::MaptileError,
        service::{ReadTarget, TileOutcome, TileStack},
    };

    let dir = tempfile::tempdir().unwrap();
    for (key, value) in [("v1-a.tif", 10), ("v1-b.tif", 90), ("v2-a.tif", 20)] {
        raster_fixture::write_world_tiff(&dir.path().join(key), |_| value);
    }
    let store = Arc::new(LocalFileSystem::new_with_prefix(dir.path()).unwrap());
    let cfg = |keys: &[&str]| MosaicSourceConfig {
        id: "ordered".into(),
        members: keys
            .iter()
            .map(|&key| MosaicMember {
                url: key.into(),
                source_id: key.into(),
                bbox_wgs84: [-180.0, -85.0, 180.0, 85.0],
            })
            .collect(),
        store: Some(store.clone()),
        ..two_member_cfg(None)
    };
    let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let publish = |source: MosaicSource| {
        let mut update = engine.catalog().update();
        update.put(Arc::new(source)).unwrap();
        engine.catalog().publish(update).unwrap();
    };
    let stack = TileStack::builder(engine.clone()).build();
    let style = raster_fixture::gray_png_style();
    let query = TileQuery {
        style: Some(&style),
        ..Default::default()
    };
    let target = ReadTarget {
        query: Some(&query),
        ..ReadTarget::new("ordered", "public")
    };

    publish(
        MosaicSource::open(cfg(&["v1-a.tif", "v1-b.tif"]), make_block_cache())
            .await
            .unwrap(),
    );
    assert!(matches!(
        stack.begin_export(target),
        Err(MaptileError::Unsupported(_))
    ));
    publish(
        MosaicSource::open_immutable(cfg(&["v1-a.tif", "v1-b.tif"]), make_block_cache())
            .await
            .unwrap(),
    );
    let old = stack.begin_export(target).unwrap();
    let west = TileCoord { z: 1, x: 0, y: 0 };
    let east = TileCoord { x: 1, ..west };
    let center_pixel = |outcome: &TileOutcome| {
        image::load_from_memory(&outcome.tile().unwrap().data)
            .unwrap()
            .to_rgba8()
            .get_pixel(128, 128)
            .0
    };
    assert_eq!(
        center_pixel(&old.read_page(&[west]).await.unwrap().tiles[0]),
        [10, 10, 10, 255]
    );

    publish(
        MosaicSource::open_immutable(cfg(&["v2-a.tif"]), make_block_cache())
            .await
            .unwrap(),
    );
    let current = stack.begin_export(target).unwrap();
    assert_ne!(old.revision(), current.revision());
    assert_eq!(
        center_pixel(&current.read_page(&[east]).await.unwrap().tiles[0]),
        [20, 20, 20, 255]
    );
    assert_eq!(
        center_pixel(&old.read_page(&[east]).await.unwrap().tiles[0]),
        [10, 10, 10, 255]
    );
}
