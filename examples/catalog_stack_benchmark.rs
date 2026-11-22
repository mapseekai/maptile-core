//! Compare cold and warm TileStack reads over real HTTP PMTiles ranges.

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode, header},
    routing::get,
};
use bytes::Bytes;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use maptile_core::{
    cache::TileCache,
    control::CatalogService,
    service::{ReadTarget, TileOutcome, TileStack},
    sources::{
        TileCoord, TileEncoding, TileFormat,
        pmtiles::{HttpPmtilesSource, PmtilesConfig},
    },
};
use pmtiles::{Compression, PmTilesWriter, TileType};
use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::Instant,
};

const ZOOM: u8 = 3;
const WIDTH: u32 = 256;
const ETAG: &str = "\"catalog-stack-benchmark-v1\"";

struct Archive {
    bytes: Bytes,
    reads: AtomicUsize,
    body_bytes: AtomicUsize,
}

impl Archive {
    fn counters(&self) -> (usize, usize) {
        (self.reads.load(Relaxed), self.body_bytes.load(Relaxed))
    }
}

fn fixture(coords: &[TileCoord]) -> Bytes {
    let mut file = tempfile::tempfile().unwrap();
    let mut writer = PmTilesWriter::new(TileType::Png)
        .tile_compression(Compression::None)
        .metadata(r#"{"name":"Catalog tile stack benchmark"}"#)
        .create(&mut file)
        .unwrap();
    for &coord in coords {
        let image = RgbaImage::from_fn(WIDTH, WIDTH, |x, y| {
            Rgba([
                (coord.x * 32 + x / 8) as u8,
                (coord.y * 32 + y / 8) as u8,
                (x + y) as u8,
                255,
            ])
        });
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let xyz = pmtiles::TileCoord::new(coord.z, coord.x, coord.y).unwrap();
        writer.add_tile(xyz, png.get_ref()).unwrap();
    }
    writer.finalize().unwrap();
    use std::io::{Read, Seek};
    file.rewind().unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes.into()
}

async fn range(
    State(archive): State<Arc<Archive>>,
    headers: HeaderMap,
) -> Result<Response<Body>, StatusCode> {
    if archive.reads.load(Relaxed) > 0
        && headers
            .get(header::IF_MATCH)
            .is_none_or(|etag| etag != ETAG)
    {
        return Err(StatusCode::PRECONDITION_FAILED);
    }
    let invalid = StatusCode::RANGE_NOT_SATISFIABLE;
    let (start, end) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|range| range.split_once('-'))
        .ok_or(invalid)?;
    let start: usize = start.parse().map_err(|_| invalid)?;
    let end: usize = end.parse().map_err(|_| invalid)?;
    let len = archive.bytes.len();
    let end = end.min(len - 1);
    if start > end {
        return Err(invalid);
    }
    let body = archive.bytes.slice(start..=end);
    archive.reads.fetch_add(1, Relaxed);
    archive.body_bytes.fetch_add(body.len(), Relaxed);
    Ok(Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::ETAG, ETAG)
        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap())
}

async fn sweep(stack: &TileStack, coords: &[TileCoord], latencies: &mut Vec<u128>) -> usize {
    let target = ReadTarget::new("benchmark", "benchmark");
    let mut failures = 0;
    for &coord in coords {
        let start = Instant::now();
        let read = stack.read(target.at(coord)).await;
        latencies.push(start.elapsed().as_nanos());
        let Ok(TileOutcome::Present(tile)) = read else {
            eprintln!("{coord:?} returned {read:?}");
            failures += 1;
            continue;
        };
        assert_eq!(tile.info.format, TileFormat::Png);
        assert_eq!(tile.info.encoding, TileEncoding::Uncompressed);
        let png = image::load_from_memory_with_format(&tile.data, ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(png.dimensions(), (WIDTH, WIDTH));
        assert_eq!(
            png.get_pixel(8, 16).0,
            [(coord.x * 32 + 1) as u8, (coord.y * 32 + 2) as u8, 24, 255]
        );
        assert_eq!(
            png.get_pixel(255, 255).0,
            [
                (coord.x * 32 + 31) as u8,
                (coord.y * 32 + 31) as u8,
                254,
                255
            ]
        );
    }
    failures
}

#[tokio::main(worker_threads = 2)]
async fn main() {
    let mode = std::env::args().nth(1);
    let warm = match mode.as_deref() {
        Some("cold") => false,
        Some("warm") => true,
        _ => panic!("usage: catalog_stack_benchmark cold|warm"),
    };
    let coords: Vec<_> = (0..1 << ZOOM)
        .flat_map(|y| (0..1 << ZOOM).map(move |x| TileCoord { z: ZOOM, x, y }))
        .collect();
    let archive = Arc::new(Archive {
        bytes: fixture(&coords),
        reads: AtomicUsize::new(0),
        body_bytes: AtomicUsize::new(0),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/archive", get(range))
        .with_state(archive.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let source = HttpPmtilesSource::open(PmtilesConfig {
        id: "benchmark".into(),
        path: format!("http://{addr}/archive"),
        serve_as: None,
        description: None,
    })
    .await
    .unwrap();
    let engine = Arc::new(CatalogService::new(TileCache::new(16, None, None).unwrap()));
    let mut update = engine.catalog().update();
    update.put(Arc::new(source)).unwrap();
    engine.catalog().publish(update).unwrap();
    let stack = TileStack::builder(engine).build();

    let mut latencies = Vec::with_capacity(coords.len());
    if warm {
        assert_eq!(sweep(&stack, &coords, &mut latencies).await, 0);
        latencies.clear();
    }
    let (reads_before, bytes_before) = archive.counters();
    let failures = sweep(&stack, &coords, &mut latencies).await;
    let (reads_after, bytes_after) = archive.counters();
    let reads = reads_after - reads_before;
    let body_bytes = bytes_after - bytes_before;
    latencies.sort_unstable();
    let p95_ns = latencies[(latencies.len() * 95).div_ceil(100) - 1];
    println!(
        "mode={} archive_bytes={} requests={} failures={failures} range_reads={reads} range_body_bytes={body_bytes} p95_ns={p95_ns}",
        mode.unwrap(),
        archive.bytes.len(),
        coords.len()
    );
    assert_eq!(failures, 0);
    if warm {
        assert_eq!((reads, body_bytes), (0, 0));
    } else {
        assert!(reads >= coords.len() && body_bytes > 0);
    }
    server.abort();
}
