#![cfg(feature = "render")]

//! Host-native integration coverage for the MapLibre renderer and worker pool.
//!
//! Run this test binary with `--test-threads=1`: MapLibre Native owns global
//! graphics/run-loop state. Pool concurrency is exercised explicitly below.

use std::collections::HashSet;
use std::io::Cursor;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::future::join_all;
use image::{DynamicImage, GenericImageView, ImageFormat};
use maptile_core::render::{
    GeoPoint, MapLibreRenderer, MarkerOverlay, OverlayItem, PoolConfig, RenderFormat,
    RenderRequest, RendererPool,
    maplibre_file_source::{
        FileSourceType, ResourceKind, ResourceRequest, Response, TokioFileSource,
        register_tokio_file_source,
    },
};
use maptile_core::wkb_mvt::{WkbFeature, encode_features_to_mvt};

const STYLE: &str = include_str!("../../testdata/render-style.json");
const MAX_CONCURRENT: usize = 2;
const DIRECT_RENDER_CHILD: &str = "MAPTILE_DIRECT_RENDER_LIFETIME_CHILD";
const MANAGED_FIXTURE_HOST: &str = "https://managed-fixtures.mapseek.test";
const MANAGED_RENDER_STYLE: &str = r##"{
  "version": 8,
  "sources": {
    "managed_raster": {
      "type": "raster",
      "tiles": ["https://managed-fixtures.mapseek.test/raster/{z}/{x}/{y}.png"],
      "tileSize": 256,
      "minzoom": 0,
      "maxzoom": 0
    },
    "managed_vector": {
      "type": "vector",
      "tiles": ["https://managed-fixtures.mapseek.test/vector/{z}/{x}/{y}.pbf"],
      "minzoom": 0,
      "maxzoom": 0
    }
  },
  "layers": [
    {"id": "background", "type": "background", "paint": {"background-color": "#102030"}},
    {"id": "raster", "type": "raster", "source": "managed_raster", "paint": {"raster-opacity": 0.72}},
    {
      "id": "managed-point",
      "type": "circle",
      "source": "managed_vector",
      "source-layer": "managed_points",
      "paint": {"circle-radius": 24, "circle-color": "#0055ff", "circle-opacity": 1.0}
    }
  ]
}"##;

static MANAGED_FIXTURE_SOURCE: Once = Once::new();

struct ManagedFixtureSource;

impl TokioFileSource for ManagedFixtureSource {
    fn can_request(&self, request: &ResourceRequest) -> bool {
        matches!(request.kind, ResourceKind::Tile) && request.url.starts_with(MANAGED_FIXTURE_HOST)
    }

    async fn request(&self, request: ResourceRequest) -> Response {
        tokio::task::yield_now().await;
        if request.url.contains("/vector/") {
            Response::data(managed_vector_tile().to_vec())
        } else if request.url.contains("/raster/") {
            Response::data(managed_raster_tile())
        } else {
            Response::no_content()
        }
    }
}

fn pool(max_concurrent: usize) -> RendererPool {
    RendererPool::new(PoolConfig {
        max_concurrent,
        default_size: (64, 48),
        queue_depth_per_worker: 4,
    })
    .expect("native renderer pool should initialize")
}

fn request(
    style_json: impl Into<String>,
    width: u32,
    height: u32,
    scale: u8,
    format: RenderFormat,
) -> RenderRequest {
    let mut request = RenderRequest::for_tile(style_json, 0, 0, 0, scale, format);
    request.width = width;
    request.height = height;
    request
}

fn decode(bytes: &[u8]) -> DynamicImage {
    image::load_from_memory(bytes).expect("renderer output should decode")
}

fn register_managed_fixture_source() {
    MANAGED_FIXTURE_SOURCE.call_once(|| {
        register_tokio_file_source(FileSourceType::Network, ManagedFixtureSource);
    });
}

fn managed_vector_tile() -> Bytes {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "kind".to_string(),
        serde_json::Value::String("managed".into()),
    );
    let mut wkb = Vec::with_capacity(21);
    wkb.push(1);
    wkb.extend_from_slice(&1u32.to_le_bytes());
    wkb.extend_from_slice(&0.0f64.to_le_bytes());
    wkb.extend_from_slice(&0.0f64.to_le_bytes());
    encode_features_to_mvt(
        "managed_points",
        [WkbFeature {
            id: Some(1),
            wkb: Bytes::from(wkb),
            properties,
        }],
        (-180.0, -85.051_128_78, 180.0, 85.051_128_78),
        4096,
    )
    .expect("managed vector fixture should encode")
}

fn managed_raster_tile() -> Vec<u8> {
    let mut rgba = image::RgbaImage::new(256, 256);
    for (x, y, pixel) in rgba.enumerate_pixels_mut() {
        *pixel = if x < 128 && y < 128 {
            image::Rgba([250, 204, 21, 255])
        } else if x >= 128 && y < 128 {
            image::Rgba([34, 197, 94, 255])
        } else if x < 128 {
            image::Rgba([239, 68, 68, 255])
        } else {
            image::Rgba([14, 165, 233, 255])
        };
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut out, ImageFormat::Png)
        .expect("managed raster fixture should encode as PNG");
    out.into_inner()
}

fn assert_visible_png(bytes: &[u8], dimensions: (u32, u32)) -> image::RgbaImage {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(image::guess_format(bytes).unwrap(), ImageFormat::Png);
    let decoded = decode(bytes).to_rgba8();
    assert_eq!(decoded.dimensions(), dimensions);
    let colors: HashSet<_> = decoded.pixels().map(|pixel| pixel.0).collect();
    assert!(
        colors.len() >= 2,
        "rendered PNG should contain visible non-uniform RGBA pixels"
    );
    decoded
}

#[test]
fn direct_renderer_completes_more_than_one_native_renderer_lifetime() {
    if std::env::var_os(DIRECT_RENDER_CHILD).is_some() {
        let request = request(STYLE, 64, 48, 1, RenderFormat::Png);
        let mut renderer = MapLibreRenderer::new(64, 48, 1.0).unwrap();
        for invocation in 1..=40 {
            let (rgba, width, height) = renderer.render_rgba(&request).unwrap();
            assert_eq!((width, height), (64, 48), "invocation {invocation}");
            assert_eq!(rgba.len(), 64 * 48 * 4, "invocation {invocation}");
        }
        return;
    }

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "direct_renderer_completes_more_than_one_native_renderer_lifetime",
            "--nocapture",
        ])
        .env(DIRECT_RENDER_CHILD, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("direct-render regression child should start");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "direct-render regression child failed: {status}"
            );
            break;
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .expect("hung direct-render child should be killable");
            let _ = child.wait();
            panic!("direct renderer did not complete 40 calls within 30 seconds");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn renders_non_uniform_pixels_at_requested_dimensions_and_repeats_png_exactly() {
    let pool = pool(1);
    let request = request(STYLE, 64, 48, 1, RenderFormat::Png);

    let first = pool.render(request.clone()).await.unwrap();
    assert_eq!((first.width, first.height), (64, 48));
    let _decoded = assert_visible_png(&first.bytes, (64, 48));

    let second = pool.render(request).await.unwrap();
    assert_eq!(first.bytes, second.bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renders_managed_vector_and_raster_fixture_pixels_as_png_tile() {
    register_managed_fixture_source();
    let pool = pool(1);
    let rendered = pool
        .render(request(
            MANAGED_RENDER_STYLE,
            256,
            256,
            1,
            RenderFormat::Png,
        ))
        .await
        .unwrap();

    assert_eq!(rendered.format, RenderFormat::Png);
    assert_eq!((rendered.width, rendered.height), (256, 256));
    let rgba = assert_visible_png(&rendered.bytes, (256, 256));
    let center = rgba.get_pixel(128, 128).0;
    let corner = rgba.get_pixel(8, 8).0;
    assert_ne!(
        center, corner,
        "managed vector point should visibly alter the managed raster fixture"
    );
    assert!(
        center[2] > center[0] && center[2] > center[1],
        "managed vector point should contribute visible blue pixels at tile center"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_worker_completes_more_than_one_native_renderer_lifetime() {
    let pool = pool(1);
    let request = request(STYLE, 64, 48, 1, RenderFormat::Png);

    for invocation in 1..=40 {
        let result =
            tokio::time::timeout(Duration::from_secs(2), pool.render(request.clone())).await;
        let rendered = match result {
            Ok(result) => result.unwrap(),
            Err(_) => {
                std::mem::forget(pool);
                panic!("native render invocation {invocation} exceeded two seconds");
            }
        };
        assert_eq!((rendered.width, rendered.height), (64, 48));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_pool_reuses_dynamic_sizes_scales_and_styles() {
    let pool = pool(1);
    let alternate_style = STYLE.replace("#102030", "#304050");
    let first_style = pool
        .render(request(STYLE, 48, 40, 1, RenderFormat::Png))
        .await
        .unwrap();
    let second_style = pool
        .render(request(&alternate_style, 48, 40, 1, RenderFormat::Png))
        .await
        .unwrap();
    assert_ne!(first_style.bytes, second_style.bytes);

    for (index, scale) in (1..=4).enumerate() {
        let (width, height) = if index % 2 == 0 { (32, 32) } else { (96, 48) };
        let style = if index % 2 == 0 {
            STYLE.to_owned()
        } else {
            alternate_style.clone()
        };
        let rendered = pool
            .render(request(style, width, height, scale, RenderFormat::Png))
            .await
            .unwrap();

        let physical_size = (width * u32::from(scale), height * u32::from(scale));
        assert_eq!((rendered.width, rendered.height), physical_size);
        assert_eq!(decode(&rendered.bytes).dimensions(), physical_size);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_completes_twice_its_worker_count_concurrently() {
    let pool = pool(MAX_CONCURRENT);
    let jobs = (0..2 * MAX_CONCURRENT).map(|index| {
        let pool = &pool;
        async move {
            pool.render(request(STYLE, 40 + index as u32, 36, 1, RenderFormat::Png))
                .await
        }
    });

    let results = join_all(jobs).await;
    assert_eq!(results.len(), 2 * MAX_CONCURRENT);
    for (index, result) in results.into_iter().enumerate() {
        let rendered = result.unwrap();
        assert_eq!((rendered.width, rendered.height), (40 + index as u32, 36));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_png_jpeg_and_webp_metadata_without_lossy_byte_assumptions() {
    let pool = pool(1);
    for (format, expected) in [
        (RenderFormat::Png, ImageFormat::Png),
        (RenderFormat::Jpeg, ImageFormat::Jpeg),
        (RenderFormat::Webp, ImageFormat::WebP),
    ] {
        let rendered = pool
            .render(request(STYLE, 52, 44, 1, format))
            .await
            .unwrap();
        assert_eq!(rendered.format, format);
        assert_eq!((rendered.width, rendered.height), (52, 44));
        assert_eq!(image::guess_format(&rendered.bytes).unwrap(), expected);
        if format == RenderFormat::Png {
            let _decoded = assert_visible_png(&rendered.bytes, (52, 44));
        } else {
            assert_eq!(decode(&rendered.bytes).dimensions(), (52, 44));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overlay_changes_output() {
    let pool = pool(1);
    let base = request(STYLE, 64, 48, 1, RenderFormat::Png);
    let without_overlay = pool.render(base.clone()).await.unwrap();

    let mut with_overlay_request = base.clone();
    with_overlay_request.overlay_items = Some(vec![OverlayItem::Marker(MarkerOverlay {
        position: GeoPoint { lon: 0.0, lat: 0.0 },
        color: image::Rgba([0, 255, 0, 255]),
        size: 28.0,
    })]);
    let with_overlay = pool.render(with_overlay_request).await.unwrap();
    assert_ne!(without_overlay.bytes, with_overlay.bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_style_does_not_poison_worker() {
    let pool = pool(1);
    let base = request(STYLE, 64, 48, 1, RenderFormat::Png);
    let baseline = pool.render(base.clone()).await.unwrap();

    let invalid = pool
        .render(request("{not valid json", 64, 48, 1, RenderFormat::Png))
        .await
        .unwrap_err();
    assert!(
        invalid.to_string().contains("style load failed"),
        "unexpected invalid-style error: {invalid}"
    );

    let recovered = pool.render(base).await.unwrap();
    assert_eq!(recovered.bytes, baseline.bytes);
    assert_eq!((baseline.width, baseline.height), (64, 48));
    assert_eq!((recovered.width, recovered.height), (64, 48));
    let _decoded = assert_visible_png(&recovered.bytes, (64, 48));
}
