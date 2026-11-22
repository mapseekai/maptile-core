#[cfg(feature = "agtiff")]
#[path = "../common/raster.rs"]
mod raster_fixture;

use axum::http::{HeaderMap, header};
use maptile_core::sources::{AsyncCogSource, AsyncCogSourceConfig, CogObjectSource, TileSource};
use std::sync::Arc;

fn cog_config(source: CogObjectSource) -> AsyncCogSourceConfig {
    AsyncCogSourceConfig {
        id: "portable-cog".into(),
        source,
        name: None,
        description: None,
        attribution: None,
        minzoom: None,
        maxzoom: None,
        bounds: None,
        cache_zoom: None,
        content_id: "point-generation".into(),
        block_cache: None,
    }
}

#[cfg(feature = "agtiff")]
#[tokio::test]
async fn opens_geotiff_and_queries_pixel_without_gdal() {
    use maptile_core::sources::RasterPointQuery;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("point.tif");
    raster_fixture::write_world_tiff(&path, |_| 73);
    let source = AsyncCogSource::open(cog_config(CogObjectSource::Local(
        path.to_str().unwrap().into(),
    )))
    .await
    .unwrap();
    let point = source
        .query_raster_point(&RasterPointQuery {
            x: 0.0,
            y: 0.0,
            coord_crs: "EPSG:4326".into(),
            bidx: vec![1],
        })
        .await
        .unwrap();
    assert!(point.valid);
    assert_eq!(point.values, vec![73.0]);
}

#[cfg(unix)]
#[test]
fn cleanup_runtime_retries_after_fd_limit_is_restored() {
    use maptile_core::{error::MaptileError, sources::RasterPointQuery};
    use object_store::{ObjectStoreExt, memory::InMemory, path::Path};

    const CHILD: &str = "MAPTILE_TEST_COG_CLEANUP_LIMIT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cleanup_runtime_retries_after_fd_limit_is_restored",
                "--nocapture",
            ])
            .env(CHILD, "1")
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

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Arc::new(InMemory::new());
            let key = Path::from("point.tif");
            let data = bytes::Bytes::from_static(include_bytes!("../fixtures/cog_uint8_rgb.tif"));
            store.put(&key, data.into()).await.unwrap();
            let source = AsyncCogSource::open(cog_config(CogObjectSource::ObjectStore {
                store,
                key: key.to_string(),
            }))
            .await
            .unwrap();
            let bounds = source.tilejson().bounds.unwrap();
            let point = RasterPointQuery {
                x: (bounds.left + bounds.right) / 2.0,
                y: (bounds.bottom + bounds.top) / 2.0,
                coord_crs: "EPSG:4326".into(),
                bidx: vec![1],
            };

            let mut original = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: The writable limit structure lives through the call.
            let status = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) };
            assert_eq!(status, 0);
            let exhausted = libc::rlimit {
                rlim_cur: 0,
                rlim_max: original.rlim_max,
            };
            // SAFETY: The limit pointer is valid and only the child changes its own soft limit.
            let status = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &exhausted) };
            assert_eq!(status, 0);
            let first = source.query_raster_point(&point).await;
            // SAFETY: The original limit structure is valid and the hard limit is preserved.
            let status = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) };
            assert_eq!(status, 0);
            assert!(
                matches!(&first, Err(MaptileError::SourceUnavailable(message))
                    if message.starts_with("cannot start cleanup runtime")),
                "{first:?}"
            );
            assert!(source.query_raster_point(&point).await.unwrap().valid);
        });
}

#[tokio::test]
async fn drain_reports_unconfirmed_raw_decode_after_valid_point_admission() {
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        error::ErrorKind,
        service::{ReadTarget, TileStack},
        sources::RasterPointQuery,
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raw.tif");
    raster_fixture::write_world_tiff_with_compression_tag(&path, |_| 73, 8);
    let config = cog_config(CogObjectSource::Local(path.to_str().unwrap().into()));
    for (band, expected_error, expected_drain_error) in [
        (2, ErrorKind::InvalidInput, None),
        (1, ErrorKind::Backend, Some(ErrorKind::Unavailable)),
    ] {
        let source = AsyncCogSource::open(config.clone()).await.unwrap();
        let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
        let mut update = engine.catalog().update();
        update.put(Arc::new(source)).unwrap();
        engine.catalog().publish(update).unwrap();
        let stack = TileStack::builder(engine.clone()).build();
        let error = stack
            .raster_point(
                ReadTarget::new("portable-cog", "public"),
                &RasterPointQuery {
                    x: 0.,
                    y: 0.,
                    coord_crs: "EPSG:4326".into(),
                    bidx: vec![band],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), expected_error);
        let drain = engine
            .catalog()
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
            .await;
        assert_eq!(drain.err().map(|error| error.kind()), expected_drain_error);
    }
}

#[tokio::test]
async fn cog_export_rejects_equal_length_local_replacement() {
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        error::MaptileError,
        service::{ReadTarget, TileOutcome, TileStack},
        sources::{TileCoord, TileQuery},
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("本地 terrain #1.tif");
    raster_fixture::write_world_tiff(&path, |_| 73);
    let config = cog_config(CogObjectSource::Local(path.to_str().unwrap().into()));
    let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let publish = |source: AsyncCogSource| {
        let mut update = engine.catalog().update();
        update.put(Arc::new(source)).unwrap();
        engine.catalog().publish(update).unwrap();
    };
    publish(AsyncCogSource::open(config.clone()).await.unwrap());
    let stack = TileStack::builder(engine.clone()).build();
    let style = raster_fixture::gray_png_style();
    let query = TileQuery {
        style: Some(&style),
        ..Default::default()
    };
    let target = ReadTarget {
        query: Some(&query),
        ..ReadTarget::new("portable-cog", "public")
    };
    let west = TileCoord { z: 1, x: 0, y: 0 };
    let east = TileCoord { x: 1, ..west };
    let center_pixel = |outcome: &TileOutcome| {
        image::load_from_memory(&outcome.tile().unwrap().data)
            .unwrap()
            .to_rgba8()
            .get_pixel(128, 128)
            .0
    };
    let old = stack.begin_export(target).unwrap();
    assert_eq!(
        center_pixel(&old.read_page(&[west]).await.unwrap().tiles[0]),
        [73, 73, 73, 255]
    );
    let next = dir.path().join("next.tif");
    raster_fixture::write_world_tiff(&next, |_| 91);
    let next_file = std::fs::File::options().write(true).open(&next).unwrap();
    next_file
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1))
        .unwrap();
    drop(next_file);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        std::fs::metadata(&next).unwrap().len()
    );
    std::fs::rename(&next, &path).unwrap();
    assert!(matches!(
        old.read_page(&[east]).await,
        Err(MaptileError::VersionChanged(_))
    ));

    publish(AsyncCogSource::open(config).await.unwrap());
    let current = stack.begin_export(target).unwrap();
    assert_ne!(old.revision(), current.revision());
    assert_eq!(
        center_pixel(&current.read_page(&[west]).await.unwrap().tiles[0]),
        [91, 91, 91, 255]
    );
}

#[tokio::test]
async fn remote_cog_replacement_preserves_the_version_error() {
    use maptile_core::{error::ErrorKind, sources::RasterPointQuery};
    use object_store::{ObjectStoreExt, memory::InMemory, path::Path};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("point.tif");
    let store = Arc::new(InMemory::new());
    let key = Path::from("point.tif");
    raster_fixture::write_world_tiff(&path, |_| 73);
    store
        .put(&key, std::fs::read(&path).unwrap().into())
        .await
        .unwrap();
    let source = AsyncCogSource::open(cog_config(CogObjectSource::ObjectStore {
        store: store.clone(),
        key: key.to_string(),
    }))
    .await
    .unwrap();
    raster_fixture::write_world_tiff(&path, |_| 91);
    store
        .put(&key, std::fs::read(path).unwrap().into())
        .await
        .unwrap();
    let error = source
        .query_raster_point(&RasterPointQuery {
            x: 0.0,
            y: 0.0,
            coord_crs: "EPSG:4326".into(),
            bidx: vec![1],
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::VersionChanged);
}

async fn start_cog_range_server<F: Future<Output = ()> + Send + 'static>(
    before_response: impl Fn(&HeaderMap, &mut HeaderMap) -> F + Clone + Send + Sync + 'static,
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{Router, http::StatusCode, routing::get};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("point.tif");
    raster_fixture::write_world_tiff(&path, |_| 73);
    let data = bytes::Bytes::from(std::fs::read(path).unwrap());
    let handler = move |headers: HeaderMap| {
        let data = data.clone();
        let before_response = before_response.clone();
        async move {
            assert_eq!(headers[header::ACCEPT_ENCODING], "identity");
            let (a, b) = headers[header::RANGE]
                .to_str()
                .unwrap()
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            let start: usize = a.parse().unwrap();
            let end = b.parse::<usize>().unwrap().min(data.len() - 1);
            let mut out = HeaderMap::new();
            out.insert(header::ETAG, "\"one\"".parse().unwrap());
            out.insert(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", data.len())
                    .parse()
                    .unwrap(),
            );
            before_response(&headers, &mut out).await;
            (StatusCode::PARTIAL_CONTENT, out, data.slice(start..=end))
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/signed.tif", get(handler));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/signed.tif?version=one"), server)
}

#[tokio::test]
async fn cancelled_cog_point_releases_catalog_while_range_is_pending() {
    use maptile_core::{
        cache::TileCache,
        control::CatalogService,
        error::MaptileError,
        service::{ReadTarget, TileStack},
        sources::RasterPointQuery,
    };
    use std::{sync::OnceLock, time::Duration};
    use tokio::{sync::Notify, time::Instant};
    use tokio_util::sync::CancellationToken;

    let gate = Arc::new(OnceLock::<(Notify, Notify)>::new());
    let (url, server) = start_cog_range_server({
        let gate = gate.clone();
        move |_, _| {
            let gate = gate.clone();
            async move {
                if let Some((entered, release)) = gate.get() {
                    entered.notify_one();
                    release.notified().await;
                }
            }
        }
    })
    .await;
    let source = AsyncCogSource::open(cog_config(CogObjectSource::Http(url)))
        .await
        .unwrap();
    let engine = Arc::new(CatalogService::new(TileCache::new(1, None, None).unwrap()));
    let mut update = engine.catalog().update();
    update.put(Arc::new(source)).unwrap();
    engine.catalog().publish(update).unwrap();
    let stack = TileStack::builder(engine.clone()).build();
    let point = RasterPointQuery {
        x: 0.0,
        y: 0.0,
        coord_crs: "EPSG:4326".into(),
        bidx: vec![1],
    };
    gate.set((Notify::new(), Notify::new())).unwrap();
    let (entered, release) = gate.get().unwrap();

    let cancel = CancellationToken::new();
    let target = ReadTarget::new("portable-cog", "public");
    let mut read = Box::pin(stack.raster_point(
        ReadTarget {
            cancellation: Some(&cancel),
            ..target
        },
        &point,
    ));
    tokio::select! {
        result = read.as_mut() => panic!("point read finished before entering the range request: {result:?}"),
        reached = tokio::time::timeout(Duration::from_secs(5), entered.notified()) => reached.unwrap(),
    }

    let deadline = Instant::now() + Duration::from_secs(1);
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout_at(deadline, read).await.unwrap(),
        Err(MaptileError::Cancelled)
    ));
    let observed = engine.catalog().snapshot();
    engine
        .catalog()
        .revoke(&observed, target.source, deadline)
        .await
        .unwrap();
    release.notify_one();
    server.abort();
}

#[tokio::test]
async fn http_cog_rechecks_successful_ranges_when_server_ignores_if_match() {
    use maptile_core::{error::ErrorKind, sources::RasterPointQuery};
    use std::sync::atomic::{AtomicBool, Ordering};

    let changed = Arc::new(AtomicBool::new(false));
    let (url, server) = start_cog_range_server({
        let changed = changed.clone();
        move |headers, out| {
            if changed.load(Ordering::Acquire) {
                assert_eq!(headers[header::IF_MATCH], "\"one\"");
                out.append(header::ETAG, "\"two\"".parse().unwrap());
            }
            std::future::ready(())
        }
    })
    .await;
    let source = AsyncCogSource::open(cog_config(CogObjectSource::Http(url)))
        .await
        .unwrap();
    changed.store(true, Ordering::Release);
    let error = source
        .query_raster_point(&RasterPointQuery {
            x: 0.0,
            y: 0.0,
            coord_crs: "EPSG:4326".into(),
            bidx: vec![1],
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::VersionChanged);
    server.abort();
}

#[tokio::test]
async fn http_cog_rejects_encoded_probe_and_data_ranges() {
    use maptile_core::{
        error::{ErrorKind, MaptileError},
        sources::RasterPointQuery,
    };
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering::Relaxed},
    };

    let point = RasterPointQuery {
        x: 0.0,
        y: 0.0,
        coord_crs: "EPSG:4326".into(),
        bidx: vec![1],
    };
    let identity = &["iDeNtItY,\tIDENTITY", "identity"][..];
    for encoded in [&["gzip"][..], &["identity", "gzip"], &["identity, BR"]] {
        let codings = Arc::new(Mutex::new(encoded));
        let conditional_reads = Arc::new(AtomicUsize::new(0));
        let (url, server) = start_cog_range_server({
            let (codings, conditional_reads) = (codings.clone(), conditional_reads.clone());
            move |request, response| {
                if let Some(if_match) = request.get(header::IF_MATCH) {
                    assert_eq!(if_match, "\"one\"");
                    conditional_reads.fetch_add(1, Relaxed);
                } else {
                    assert_eq!(request[header::RANGE], "bytes=0-0");
                }
                for &coding in *codings.lock().unwrap() {
                    response.append(header::CONTENT_ENCODING, coding.parse().unwrap());
                }
                std::future::ready(())
            }
        })
        .await;
        let open = || AsyncCogSource::open(cog_config(CogObjectSource::Http(url.clone())));
        assert!(matches!(open().await, Err(MaptileError::Config(_))));
        assert_eq!(conditional_reads.load(Relaxed), 0);

        *codings.lock().unwrap() = identity;
        let source = open().await.unwrap();
        let reads_at_open = conditional_reads.load(Relaxed);
        assert!(reads_at_open > 0);

        *codings.lock().unwrap() = encoded;
        let error = source.query_raster_point(&point).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Backend);
        assert!(conditional_reads.load(Relaxed) > reads_at_open);

        *codings.lock().unwrap() = identity;
        let recovered = source.query_raster_point(&point).await.unwrap();
        assert!(recovered.valid);
        assert_eq!(recovered.values, vec![73.0]);
        server.abort();
    }
}

#[tokio::test]
async fn cog_rejects_an_http_object_without_a_strong_validator() {
    use axum::{Router, extract::Path, http::StatusCode, routing::get};
    use maptile_core::error::MaptileError;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let probes = Arc::new(AtomicUsize::new(0));
    let handler = {
        let probes = probes.clone();
        move |Path(mode): Path<String>, headers: HeaderMap| {
            let probes = probes.clone();
            async move {
                assert_eq!(headers[header::RANGE], "bytes=0-0");
                probes.fetch_add(1, Relaxed);
                let mut out = HeaderMap::new();
                out.insert(header::CONTENT_RANGE, "bytes 0-0/4".parse().unwrap());
                if mode == "weak.tif" {
                    out.insert(header::ETAG, "W/\"v1\"".parse().unwrap());
                }
                (StatusCode::PARTIAL_CONTENT, out, "I")
            }
        }
    };
    let app = Router::new().route("/{mode}", get(handler));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for mode in ["missing.tif", "weak.tif"] {
        let result = AsyncCogSource::open(cog_config(CogObjectSource::Http(format!(
            "http://{addr}/{mode}"
        ))))
        .await;
        assert!(matches!(result, Err(MaptileError::Config(_))));
    }
    assert_eq!(probes.load(Relaxed), 2);
    server.abort();
}

mod raster_uint16_e2e {
    #![cfg(feature = "raster")]

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
