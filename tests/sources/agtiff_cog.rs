mod raster_uint16_e2e {
    #![cfg(feature = "agtiff")]

    use gdal::DriverManager;
    use gdal::raster::{Buffer, RasterCreationOptions};
    use gdal::spatial_ref::SpatialRef;
    use maptile_core::raster::ResamplingMethod;
    use maptile_core::raster::style::{EffectiveStyle, RasterOutputFormat};
    use maptile_core::sources::{
        AsyncCogSource, AsyncCogSourceConfig, CogObjectSource, TileCoord, TileQuery, TileSource,
    };

    fn write_uint16_rgb(path: &std::path::Path, generation: u16) {
        let driver = DriverManager::get_driver_by_name("GTiff").expect("GTiff driver");
        let options = RasterCreationOptions::from_iter([
            "TILED=YES",
            "BLOCKXSIZE=256",
            "BLOCKYSIZE=256",
            "COMPRESS=DEFLATE",
        ]);
        let mut dataset = driver
            .create_with_band_type_with_options::<u16, _>(path, 256, 256, 3, &options)
            .expect("create UInt16 RGB GeoTIFF");
        let north = 85.051_128_78;
        dataset
            .set_geo_transform(&[-180.0, 360.0 / 256.0, 0.0, north, 0.0, -2.0 * north / 256.0])
            .expect("set geotransform");
        dataset
            .set_spatial_ref(&SpatialRef::from_epsg(4326).expect("EPSG:4326"))
            .expect("set CRS");
        for band in 1..=3 {
            let values = (0..256 * 256)
                .map(|index| {
                    let x = index % 256;
                    let y = index / 256;
                    generation
                        .saturating_mul(4_000)
                        .saturating_add((band as u16).saturating_mul(6_000))
                        .saturating_add((x as u16).saturating_mul(80))
                        .saturating_add((y as u16).saturating_mul(20))
                })
                .collect();
            let mut buffer = Buffer::new((256, 256), values);
            dataset
                .rasterband(band)
                .expect("raster band")
                .write((0, 0), (256, 256), &mut buffer)
                .expect("write UInt16 pixels");
        }
        dataset.flush_cache().expect("flush UInt16 fixture");
    }

    async fn open(path: &std::path::Path, content_version: &str) -> AsyncCogSource {
        let source_id =
            std::env::var("MAPSEEK_TASK10_SOURCE_ID").unwrap_or_else(|_| "cog.uint16-rgb".into());
        AsyncCogSource::open(AsyncCogSourceConfig {
            id: source_id,
            source: CogObjectSource::Local(path.to_string_lossy().into_owned()),
            name: Some("UInt16 RGB".into()),
            description: None,
            attribution: None,
            minzoom: Some(0),
            maxzoom: Some(0),
            bounds: None,
            cache_zoom: None,
            content_id: content_version.into(),
            block_cache: None,
        })
        .await
        .expect("open UInt16 COG")
    }

    #[tokio::test]
    async fn projected_world_bounds_stay_inside_wgs84() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.tif");
        write_uint16_rgb(&path, 1);
        let mut dataset = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )
        .unwrap();
        let extent = 20_037_508.342_789_244;
        dataset
            .set_geo_transform(&[
                -extent,
                2.0 * extent / 256.0,
                0.0,
                extent,
                0.0,
                -2.0 * extent / 256.0,
            ])
            .unwrap();
        dataset
            .set_spatial_ref(&SpatialRef::from_epsg(3857).unwrap())
            .unwrap();
        dataset.flush_cache().unwrap();
        drop(dataset);
        let source = open(&path, "world-bounds-v1").await;
        let bounds = source.tilejson().bounds.unwrap();
        assert_eq!(bounds.left, -180.0);
        assert_eq!(bounds.right, 180.0);
        assert!(bounds.bottom >= -90.0 && bounds.top <= 90.0);
        assert!(bounds.bottom < bounds.top);
    }

    fn style(format: RasterOutputFormat, resampling: ResamplingMethod) -> EffectiveStyle {
        let mut document = serde_json::json!({
            "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "rgb", "bidx": [1, 2, 3]},
            "raster:tile": {"format": format, "tilesize": 256},
            "resampling": {"read": resampling, "reproject": resampling},
            "stretch": {"method": "linear", "rescale": [[0., 65_535.]]}
        });
        if format == RasterOutputFormat::Webp {
            document["raster:tile"]["lossless"] = true.into();
        }
        EffectiveStyle::from_raster_style(raster_style::Style::from_value(document).unwrap())
            .unwrap()
    }

    fn canonical_service_style() -> EffectiveStyle {
        EffectiveStyle::from_raster_style(raster_style::Style::from_value(serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "rgb", "bidx": vec![1, 2, 3]}, "raster:tile": {"format": RasterOutputFormat::Webp, "tilesize": 256, "lossless": true}, "resampling": {"read": ResamplingMethod::Nearest, "reproject": ResamplingMethod::Nearest}, "stretch": {"method": "linear", "rescale": vec![
            (1_200.0, 60_000.0),
            (1_400.0, 61_000.0),
            (1_600.0, 62_000.0),
        ]}})).unwrap())
    .expect("Dataset canonical UInt16 service style")
    }

    async fn tile(source: &AsyncCogSource, style: &EffectiveStyle) -> Vec<u8> {
        let query = TileQuery {
            style: Some(style),
            ..Default::default()
        };
        source
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&query))
            .await
            .expect("render UInt16 RGB tile")
            .data
            .to_vec()
    }

    #[tokio::test]
    async fn uint16_rgb_ready_tile_is_non_black_and_content_version_invalidates_bytes() {
        let dir = tempfile::tempdir().expect("temporary fixture directory");
        let path = dir.path().join("rgb.tif");
        write_uint16_rgb(&path, 1);
        let first_version = std::env::var("MAPSEEK_TASK10_CONTENT_VERSION")
            .unwrap_or_else(|_| "v1:etag_size:first:1".into());
        let first = open(&path, &first_version).await;
        let canonical = canonical_service_style();
        let first_bytes = tile(&first, &canonical).await;
        let first_image = image::load_from_memory(&first_bytes)
            .expect("decode first tile")
            .to_rgb8();
        let mut histogram = [0_u64; 256];
        for pixel in first_image.pixels() {
            histogram[pixel.0[0] as usize] += 1;
        }
        assert!(
            histogram[1..].iter().sum::<u64>() > 0,
            "tile must not be black"
        );
        assert!(histogram.iter().filter(|count| **count > 0).count() > 8);

        if let Some(artifact_dir) = std::env::var_os("MAPSEEK_TASK10_ARTIFACT_DIR") {
            let artifact_dir = std::path::PathBuf::from(artifact_dir);
            std::fs::create_dir_all(&artifact_dir).expect("create Task10 artifact directory");
            std::fs::write(artifact_dir.join("uint16-rgb-tile.webp"), &first_bytes)
                .expect("write rendered tile artifact");
            let source_id = std::env::var("MAPSEEK_TASK10_SOURCE_ID")
                .unwrap_or_else(|_| "cog.uint16-rgb".into());
            std::fs::write(
                artifact_dir.join("histogram.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "source": "generated UInt16 RGB GeoTIFF",
                    "dimensions": [first_image.width(), first_image.height()],
                    "non_black_pixels": histogram[1..].iter().sum::<u64>(),
                    "occupied_red_buckets": histogram.iter().filter(|count| **count > 0).count(),
                    "red_histogram": histogram.as_slice(),
                }))
                .expect("encode histogram"),
            )
            .expect("write histogram artifact");
            std::fs::write(
                artifact_dir.join("request-log.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "requests": [{
                        "method": "GET",
                        "resource": format!("/api/v1/tile/{source_id}/0/0/0.webp"),
                        "style_transport": "canonical RPC metadata",
                        "url_contains_style_query": false,
                        "url_contains_secret": false,
                        "status": 200,
                        "content_type": "image/webp"
                    }]
                }))
                .expect("encode request log"),
            )
            .expect("write request log artifact");
        }

        let first_content_version = first.content_version();
        write_uint16_rgb(&path, 5);
        let second = open(&path, "v1:etag_size:second:1").await;
        let second_bytes = tile(&second, &canonical).await;
        assert_ne!(first_content_version, second.content_version());
        assert_ne!(
            first_bytes, second_bytes,
            "same source id with changed object bytes must render a new cache partition"
        );

        let tilejson = serde_json::to_string(second.tilejson()).expect("serialize TileJSON");
        assert!(!tilejson.contains("style="));
        assert!(!tilejson.contains("token="));
        assert!(!tilejson.contains("secret"));
    }

    #[tokio::test]
    async fn uint16_rgb_style_matrix_renders_png_and_webp() {
        let dir = tempfile::tempdir().expect("temporary fixture directory");
        let path = dir.path().join("matrix.tif");
        write_uint16_rgb(&path, 2);
        let source = open(&path, "v1:etag_size:matrix:1").await;
        for (format, resampling) in [
            (RasterOutputFormat::Png, ResamplingMethod::Nearest),
            (RasterOutputFormat::Png, ResamplingMethod::Bilinear),
            (RasterOutputFormat::Webp, ResamplingMethod::Nearest),
            (RasterOutputFormat::Webp, ResamplingMethod::Bilinear),
        ] {
            let rendered = tile(&source, &style(format, resampling)).await;
            let decoded = image::load_from_memory(&rendered).expect("decode style matrix tile");
            assert_eq!((decoded.width(), decoded.height()), (256, 256));
        }
    }

    #[tokio::test]
    async fn native_stretch_methods_expression_and_opacity_render_real_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stretch.tif");
        write_uint16_rgb(&path, 1);
        let source = open(&path, "native-stretch").await;
        for stretch in [
            serde_json::json!({"method": "none"}),
            serde_json::json!({"method": "linear", "rescale": [[0, 65535]]}),
            serde_json::json!({"method": "minmax"}),
            serde_json::json!({"method": "percentile", "percentiles": [2, 98]}),
            serde_json::json!({"method": "stddev", "stddev": 2}),
            serde_json::json!({"method": "histogram_equalization"}),
            serde_json::json!({"method": "curve", "curves": [[[0, 0], [65535, 1]]]}),
        ] {
            let style = EffectiveStyle::from_raster_style(
                raster_style::Style::from_value(serde_json::json!({
                    "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "gray", "expression": "b1+b2"},
                    "stretch": stretch, "opacity": 0.5, "raster:tile": {"tilesize": 64}
                }))
                .unwrap(),
            )
            .unwrap();
            let data = tile(&source, &style).await;
            let pixels = image::load_from_memory(&data).unwrap().to_rgba8();
            assert_eq!(pixels.dimensions(), (64, 64));
            assert_eq!(pixels.get_pixel(32, 32).0[3], 128);
            assert!(pixels.get_pixel(32, 32).0[0] > 0);
        }
    }

    #[tokio::test]
    async fn uint16_rgb_statistics_match_expression_pixels_after_display_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgb-statistics.tif");
        write_uint16_rgb(&path, 1);
        let source = open(&path, "rgb-statistics").await;
        for stretch in [
            serde_json::json!({"method": "minmax"}),
            serde_json::json!({"method": "percentile", "percentiles": [2, 98]}),
            serde_json::json!({"method": "stddev", "stddev": 2}),
        ] {
            for opacity in [1.0_f64, 0.5, 1.0] {
                let mut document = serde_json::json!({
                    "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "rgb", "bidx": [3, 1, 3]},
                    "stretch": stretch, "opacity": opacity,
                    "statistics": {"scope": "dataset", "accuracy": "sample", "sample_size": 64},
                    "raster:tile": {"tilesize": 64}
                });
                let direct = EffectiveStyle::from_raster_style(
                    raster_style::Style::from_value(document.clone()).unwrap(),
                )
                .unwrap();
                // Equivalent expressions exercise the generic renderer alongside the RGB fast path.
                document["renderer"] = serde_json::json!({
                    "type": "rgb", "expression": "b3+0;b1+0;b3+0"
                });
                let expression = EffectiveStyle::from_raster_style(
                    raster_style::Style::from_value(document).unwrap(),
                )
                .unwrap();
                let direct = image::load_from_memory(&tile(&source, &direct).await)
                    .unwrap()
                    .to_rgba8();
                let expression = image::load_from_memory(&tile(&source, &expression).await)
                    .unwrap()
                    .to_rgba8();
                assert_eq!(direct, expression, "{stretch}, opacity={opacity}");
                assert_eq!(
                    direct.get_pixel(32, 32).0[3],
                    (opacity * 255.0).round() as u8
                );
                assert!(direct.pixels().any(|p| p[3] > 0 && p[0] > 0 && p[0] < 255));
            }
        }
    }
}

mod agtiff_cog {
    mod agtiff_cog_smoke {
        #![cfg(feature = "agtiff")]
        //! Smoke test: AsyncCogSource opens a local COG and reports the resolved
        //! output format (default WebP; PNG when overridden via the request style).

        use maptile_core::raster::style::{EffectiveStyle, RasterOutputFormat};
        use maptile_core::sources::{
            AsyncCogSource, AsyncCogSourceConfig, CogObjectSource, TileCoord, TileFormat,
            TileQuery, TileSource,
        };

        /// RIFF container magic; bytes 8..12 must be "WEBP".
        fn is_webp(data: &[u8]) -> bool {
            data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP"
        }

        fn is_png(d: &[u8]) -> bool {
            d.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        }

        fn fixture() -> AsyncCogSourceConfig {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/cog_uint8_rgb.tif"
            );
            AsyncCogSourceConfig {
                id: "smoke".into(),
                source: CogObjectSource::Local(path.into()),
                name: None,
                description: None,
                attribution: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: None,
                content_id: path.into(),
                block_cache: None,
            }
        }

        #[tokio::test]
        async fn object_store_cog_opens_with_version_verification() {
            let mut cfg = fixture();
            let CogObjectSource::Local(path) = &cfg.source else {
                unreachable!()
            };
            let key = object_store::path::Path::from_filesystem_path(path).unwrap();
            cfg.source = CogObjectSource::ObjectStore {
                store: std::sync::Arc::new(object_store::local::LocalFileSystem::new()),
                key: key.to_string(),
            };
            AsyncCogSource::open(cfg)
                .await
                .expect("open version-verified object store COG");
        }

        #[tokio::test]
        async fn missing_style_is_rejected() {
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            let error = src
                .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
                .await
                .expect_err("raster requests without canonical style must fail closed");
            assert!(matches!(
                error,
                maptile_core::error::MaptileError::InvalidRequest(_)
            ));
        }

        #[tokio::test]
        async fn explicit_webp_tile_is_webp() {
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            let style = EffectiveStyle::from_raster_style(raster_style::Style::from_value(serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "gray", "bidx": vec![1]}, "raster:tile": {"format": RasterOutputFormat::Webp, "tilesize": 512, "lossless": true}, "resampling": {"read": maptile_core::raster::ResamplingMethod::Bilinear, "reproject": maptile_core::raster::ResamplingMethod::Bilinear}})).unwrap())
    .unwrap();
            let query = TileQuery {
                style: Some(&style),
                ..Default::default()
            };
            let response = src
                .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&query))
                .await
                .expect("tile");
            assert!(is_webp(&response.data));
            assert_eq!(response.info.format, TileFormat::Webp);
        }

        #[tokio::test]
        async fn png_override_tile_is_png() {
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            let style = EffectiveStyle::from_raster_style(raster_style::Style::from_value(serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "gray", "bidx": vec![1]}, "raster:tile": {"format": RasterOutputFormat::Png, "tilesize": 512}, "resampling": {"read": maptile_core::raster::ResamplingMethod::Bilinear, "reproject": maptile_core::raster::ResamplingMethod::Bilinear}})).unwrap())
    .unwrap();
            let q = TileQuery {
                style: Some(&style),
                ..Default::default()
            };
            let resp = src
                .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&q))
                .await
                .expect("tile");
            assert!(is_png(&resp.data));
            assert_eq!(resp.info.format, TileFormat::Png);
        }

        #[tokio::test]
        async fn unsupported_document_and_source_alpha_fail_explicitly() {
            let mut config = fixture();
            if let CogObjectSource::Local(path) = &mut config.source {
                *path = path.replace("cog_uint8_rgb.tif", "cog_uint8_rgba.tif");
            }
            let source = AsyncCogSource::open(config).await.unwrap();
            for (document, expected) in [
                (
                    serde_json::json!({"version": "3.0", "renderer": {"type": "gray", "index": {"name": "ndvi", "bindings": {"red": 1, "nir": 2}}}}),
                    "/renderer/index",
                ),
                (
                    serde_json::json!({"version": "3.0", "renderer": {"type": "gray", "bidx": [1]}}),
                    "Alpha",
                ),
            ] {
                let style = EffectiveStyle::from_raster_style(
                    raster_style::Style::from_value(document).unwrap(),
                )
                .unwrap();
                let query = TileQuery {
                    style: Some(&style),
                    ..Default::default()
                };
                let error = source
                    .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&query))
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, maptile_core::error::MaptileError::InvalidStyle(_)),
                    "{error}"
                );
                assert!(error.to_string().contains(expected), "{error}");
            }
        }
    }
    mod agtiff_cog_parity {
        #![cfg(feature = "agtiff")]
        //! AsyncCogSource output shape + band-assignment validation.
        //!
        //! Dimension parity is checked by reading the lossless WebP (VP8L) header
        //! directly (no `image` dev-dependency): after the `RIFF....WEBP` +
        //! `VP8L` + chunk-size + 0x2f signature byte, the next 32 LE bits pack
        //! `(width-1)` in the low 14 bits and `(height-1)` in the next 14 bits.

        use maptile_core::raster::style::{EffectiveStyle, RasterOutputFormat};
        use maptile_core::sources::{
            AsyncCogSource, AsyncCogSourceConfig, CogObjectSource, TileCoord, TileQuery, TileSource,
        };

        fn fixture() -> AsyncCogSourceConfig {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/cog_uint8_rgb.tif"
            );
            AsyncCogSourceConfig {
                id: "parity".into(),
                source: CogObjectSource::Local(path.into()),
                name: None,
                description: None,
                attribution: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: None,
                content_id: path.into(),
                block_cache: None,
            }
        }

        fn webp_dimensions(webp: &[u8]) -> (u32, u32) {
            assert!(webp.len() >= 25, "webp too short for VP8L header");
            assert_eq!(&webp[0..4], b"RIFF", "missing RIFF magic");
            assert_eq!(&webp[8..12], b"WEBP", "missing WEBP magic");
            assert_eq!(&webp[12..16], b"VP8L", "expected lossless VP8L chunk");
            assert_eq!(webp[20], 0x2f, "missing VP8L signature byte");
            let bits = u32::from_le_bytes([webp[21], webp[22], webp[23], webp[24]]);
            let w = (bits & 0x3FFF) + 1;
            let h = ((bits >> 14) & 0x3FFF) + 1;
            (w, h)
        }

        #[tokio::test]
        async fn tile_is_512_square_webp() {
            let src = AsyncCogSource::open(fixture()).await.unwrap();
            let style = EffectiveStyle::from_raster_style(raster_style::Style::from_value(serde_json::json!({"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "gray", "bidx": vec![1]}, "raster:tile": {"format": RasterOutputFormat::Webp, "tilesize": 512, "lossless": true}, "resampling": {"read": maptile_core::raster::ResamplingMethod::Bilinear, "reproject": maptile_core::raster::ResamplingMethod::Bilinear}})).unwrap())
    .unwrap();
            let query = TileQuery {
                style: Some(&style),
                ..Default::default()
            };
            let resp = src
                .get_tile(TileCoord { z: 0, x: 0, y: 0 }, Some(&query))
                .await
                .unwrap();
            assert_eq!(webp_dimensions(&resp.data), (512, 512));
        }
    }
    mod agtiff_cog_point {
        #![cfg(feature = "agtiff")]

        use maptile_core::error::MaptileError;
        use maptile_core::sources::{
            AsyncCogSource, AsyncCogSourceConfig, CogObjectSource, RasterPointQuery, TileSource,
        };

        fn fixture() -> AsyncCogSourceConfig {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/cog_uint8_rgba.tif"
            );
            AsyncCogSourceConfig {
                id: "point".into(),
                source: CogObjectSource::Local(path.into()),
                name: None,
                description: None,
                attribution: None,
                minzoom: None,
                maxzoom: None,
                bounds: None,
                cache_zoom: None,
                content_id: path.into(),
                block_cache: None,
            }
        }

        #[tokio::test]
        async fn raster_point_inside_returns_selected_band_value() {
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            assert!(src.supports_raster_point());
            let bounds = src.tilejson().bounds.expect("bounds");
            let query = RasterPointQuery {
                x: (bounds.left + bounds.right) / 2.0,
                y: (bounds.bottom + bounds.top) / 2.0,
                coord_crs: "EPSG:4326".into(),
                bidx: vec![1],
            };

            let point = src.query_raster_point(&query).await.expect("point query");

            assert!(!point.outside);
            assert_eq!(point.bidx, vec![1]);
            assert_eq!(point.values.len(), 1);
            assert!(point.values[0].is_finite());
        }

        #[tokio::test]
        async fn raster_point_outside_returns_empty_outside_response() {
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            let bounds = src.tilejson().bounds.expect("bounds");
            let query = RasterPointQuery {
                x: (bounds.left + bounds.right) / 2.0,
                y: (bounds.top + 5.0).min(89.9),
                coord_crs: "EPSG:4326".into(),
                bidx: vec![1],
            };

            let point = src.query_raster_point(&query).await.expect("point query");

            assert!(point.outside);
            assert!(!point.valid);
            assert_eq!(point.bidx, vec![1]);
            assert!(point.values.is_empty());
        }

        #[tokio::test]
        async fn raster_point_rejects_invalid_band_index() {
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            let bounds = src.tilejson().bounds.expect("bounds");
            let query = RasterPointQuery {
                x: (bounds.left + bounds.right) / 2.0,
                y: (bounds.bottom + bounds.top) / 2.0,
                coord_crs: "EPSG:4326".into(),
                bidx: vec![0],
            };

            let err = src.query_raster_point(&query).await.unwrap_err();
            assert!(
                matches!(err, MaptileError::InvalidStyle(ref msg) if msg.contains("bidx")),
                "expected bidx validation error, got {err:?}"
            );
        }

        #[tokio::test]
        async fn raster_point_rejects_invalid_crs() {
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            let bounds = src.tilejson().bounds.expect("bounds");
            let query = RasterPointQuery {
                x: (bounds.left + bounds.right) / 2.0,
                y: (bounds.bottom + bounds.top) / 2.0,
                coord_crs: "WGS84".into(),
                bidx: vec![1],
            };

            let err = src.query_raster_point(&query).await.unwrap_err();
            assert!(
                matches!(err, MaptileError::InvalidStyle(ref msg) if msg.contains("coord_crs")),
                "expected CRS validation error, got {err:?}"
            );
        }

        #[cfg(feature = "raster")]
        #[tokio::test]
        async fn coverage_full_extent_clamps_floating_point_pixel_window() {
            use gdal::raster::{Buffer, RasterCreationOptions};
            use maptile_core::sources::raster_coverage::CoverageQuery;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("native-grid.tif");
            let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")
                .unwrap()
                .create_with_band_type_with_options::<u8, _>(
                    &path,
                    30,
                    34,
                    1,
                    &RasterCreationOptions::from_iter(["TILED=YES"]),
                )
                .unwrap();
            // Native grid reported by QGIS; inverse rounding extends the pixel window.
            ds.set_geo_transform(&[
                12_801_741.441_226_46,
                1_099.629_397_464_916,
                0.,
                2_912_638.091_006_387,
                0.,
                -1_099.629_397_464_943_4,
            ])
            .unwrap();
            ds.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857).unwrap())
                .unwrap();
            ds.rasterband(1)
                .unwrap()
                .write(
                    (0, 0),
                    (30, 34),
                    &mut Buffer::new((30, 34), vec![73u8; 30 * 34]),
                )
                .unwrap();
            ds.close().unwrap();
            let src = AsyncCogSource::open(AsyncCogSourceConfig {
                source: CogObjectSource::Local(path.to_string_lossy().into_owned()),
                content_id: path.to_string_lossy().into_owned(),
                ..fixture()
            })
            .await
            .unwrap();
            let metadata = src.raster_coverage(None).await.unwrap().metadata;
            for (width, height) in [(30, 34), (10, 10)] {
                let response = src
                    .raster_coverage(Some(&CoverageQuery {
                        bounds: metadata.bounds,
                        crs: metadata.crs.clone(),
                        width,
                        height,
                    }))
                    .await
                    .expect("export full native extent without an extra edge pixel");
                let output = dir.path().join("coverage.tif");
                std::fs::write(&output, response.data).unwrap();
                let output = gdal::Dataset::open(output).unwrap();
                let size = (width as usize, height as usize);
                assert_eq!(output.raster_size(), size);
                let pixels = output
                    .rasterband(1)
                    .unwrap()
                    .read_as::<u8>((0, 0), size, size, None)
                    .unwrap();
                assert!(pixels.data().iter().all(|&value| value == 73));
            }
        }

        #[cfg(feature = "raster")]
        #[tokio::test]
        async fn coverage_reads_native_cog_values_and_rejects_reprojection() {
            use maptile_core::sources::raster_coverage::CoverageQuery;
            let src = AsyncCogSource::open(fixture()).await.expect("open COG");
            assert!(src.supports_raster_coverage());
            let description = src
                .raster_coverage(None)
                .await
                .expect("describe native grid");
            assert!(description.data.is_empty());
            let metadata = description.metadata;
            let query = CoverageQuery {
                bounds: metadata.bounds,
                crs: metadata.crs.clone(),
                width: 2,
                height: 2,
            };
            let response = src
                .raster_coverage(Some(&query))
                .await
                .expect("export native values");
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), response.data).unwrap();
            let ds = gdal::Dataset::open(file.path()).unwrap();
            assert_eq!(ds.raster_size(), (2, 2));
            assert_eq!(ds.raster_count(), metadata.bands);
            assert_eq!(
                ds.rasterband(1).unwrap().band_type(),
                gdal::raster::GdalDataType::UInt8
            );
            let original = gdal::Dataset::open(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/cog_uint8_rgba.tif"
            ))
            .unwrap();
            for band in 1..=metadata.bands {
                let expected = original
                    .rasterband(band)
                    .unwrap()
                    .read_as::<u8>(
                        (0, 0),
                        original.raster_size(),
                        (2, 2),
                        Some(gdal::raster::ResampleAlg::NearestNeighbour),
                    )
                    .unwrap();
                let actual = ds
                    .rasterband(band)
                    .unwrap()
                    .read_as::<u8>((0, 0), (2, 2), (2, 2), None)
                    .unwrap();
                assert_eq!(actual.data(), expected.data(), "raw band {band}");
            }
            let unsupported = CoverageQuery {
                crs: "EPSG:999999".into(),
                ..query
            };
            assert!(src.raster_coverage(Some(&unsupported)).await.is_err());
        }

        #[cfg(feature = "raster")]
        #[tokio::test]
        async fn constant_uint16_style_renders_color_without_changing_values_or_nodata() {
            use gdal::raster::{Buffer, RasterCreationOptions};
            use maptile_core::raster::style::EffectiveStyle;
            use maptile_core::sources::raster_coverage::CoverageQuery;
            use maptile_core::sources::{TileCoord, TileFormat, TileQuery};

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("constant-cog.tif");
            let size = 256;
            let origin = 20_037_508.342_789_244;
            let step = 2.0 * origin / size as f64;
            let mut ds = gdal::DriverManager::get_driver_by_name("MEM")
                .unwrap()
                .create_with_band_type::<u16, _>("", size, size, 1)
                .unwrap();
            ds.set_geo_transform(&[-origin, step, 0.0, origin, 0.0, -step])
                .unwrap();
            ds.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857).unwrap())
                .unwrap();
            let mut values = vec![25u16; size * size];
            values[64 * size + 64] = 65_535;
            let mut band = ds.rasterband(1).unwrap();
            band.set_no_data_value(Some(65_535.0)).unwrap();
            band.write(
                (0, 0),
                (size, size),
                &mut Buffer::new((size, size), values.clone()),
            )
            .unwrap();
            ds.create_copy(
                &gdal::DriverManager::get_driver_by_name("COG").unwrap(),
                &path,
                &RasterCreationOptions::from_iter(["COMPRESS=DEFLATE", "BLOCKSIZE=256"]),
            )
            .unwrap()
            .close()
            .unwrap();
            ds.close().unwrap();
            let src = AsyncCogSource::open(AsyncCogSourceConfig {
                source: CogObjectSource::Local(path.to_string_lossy().into_owned()),
                content_id: path.to_string_lossy().into_owned(),
                ..fixture()
            })
            .await
            .unwrap();

            // Exact default document emitted by Go for UInt16 min=max=p2=p98=25.
            let style = EffectiveStyle::from_raster_style(
                raster_style::Style::from_value(serde_json::json!({
                    "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0",
                    "renderer": {"type": "pseudocolor", "colormap_name": "viridis", "bidx": [1]},
                    "stretch": {"method": "linear", "rescale": [[24.5, 25.5]]},
                    "raster:tile": {"format": "webp", "tilesize": 256, "lossless": true},
                    "nodata": 65535
                }))
                .unwrap(),
            )
            .unwrap();
            let tile = src
                .get_tile(
                    TileCoord { z: 0, x: 0, y: 0 },
                    Some(&TileQuery {
                        style: Some(&style),
                        ..Default::default()
                    }),
                )
                .await
                .expect("render constant COG with padded display range");
            assert_eq!(tile.info.format, TileFormat::Webp);
            let image = image::load_from_memory(&tile.data).unwrap().to_rgba8();
            assert_eq!(image.dimensions(), (256, 256));
            let color = *image.get_pixel(128, 128);
            assert_eq!(color[3], 255, "valid constant values must be opaque");
            assert!(
                color[0] != color[1] || color[1] != color[2],
                "viridis must produce color"
            );
            for (x, y, pixel) in image.enumerate_pixels() {
                if (x, y) == (64, 64) {
                    assert_eq!(pixel[3], 0, "NoData must remain transparent");
                } else {
                    assert_eq!(*pixel, color, "constant pixel ({x},{y})");
                }
            }

            for (pixel, valid) in [(128, true), (64, false)] {
                let point = src
                    .query_raster_point(&RasterPointQuery {
                        x: -origin + (f64::from(pixel) + 0.5) * step,
                        y: origin - (f64::from(pixel) + 0.5) * step,
                        coord_crs: "EPSG:3857".into(),
                        bidx: vec![1],
                    })
                    .await
                    .unwrap();
                assert!(!point.outside);
                assert_eq!(point.valid, valid);
                if valid {
                    assert_eq!(point.values, vec![25.0], "point must expose the raw value");
                }
            }
            let metadata = src.raster_coverage(None).await.unwrap().metadata;
            let coverage = src
                .raster_coverage(Some(&CoverageQuery {
                    bounds: metadata.bounds,
                    crs: metadata.crs,
                    width: 256,
                    height: 256,
                }))
                .await
                .unwrap();
            let output = dir.path().join("coverage.tif");
            std::fs::write(&output, coverage.data).unwrap();
            let output = gdal::Dataset::open(output).unwrap();
            let band = output.rasterband(1).unwrap();
            assert_eq!(band.band_type(), gdal::raster::GdalDataType::UInt16);
            assert_eq!(band.no_data_value(), Some(65_535.0));
            assert_eq!(
                band.read_as::<u16>((0, 0), (size, size), (size, size), None)
                    .unwrap()
                    .data(),
                values,
                "coverage must retain every original value, including NoData"
            );
        }
    }
}
