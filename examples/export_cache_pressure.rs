//! Exercise the online tile cache during a version-pinned export.
//! Pass `control` for online reads alone or `export` to run both workloads.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use maptile_core::{
    cache::{ContentVersion, TileCache},
    control::{CatalogService, SourceDescriptor, TileDriver},
    error::MaptileResult,
    service::{BatchPage, ReadTarget, TileOutcome, TileStack},
    sources::{TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, mvt},
};
use tokio::{
    sync::Barrier,
    task::JoinSet,
    time::{MissedTickBehavior, interval, sleep},
};

const TARGET: &str = "pressure";
const ZOOM: u8 = 14;
const HOT: usize = 16;
const WARM: usize = 96;
const ONLINE: usize = 1536;
const EXPORT: usize = 8192;
const WORKERS: usize = 8;
const PAGE: usize = 32;
const DELAY: Duration = Duration::from_millis(2);
const PACING: Duration = Duration::from_millis(1);
const MVT: TileInfo = TileInfo {
    format: TileFormat::Mvt,
    encoding: TileEncoding::Uncompressed,
};

fn coord(x: usize, y: u32) -> TileCoord {
    TileCoord {
        z: ZOOM,
        x: x as u32,
        y,
    }
}

fn target() -> ReadTarget<'static> {
    ReadTarget::new(TARGET, "public")
}

struct Fixture {
    data: Bytes,
    fingerprint: blake3::Hash,
}

impl Fixture {
    fn new(coord: TileCoord) -> Self {
        let tile = mvt::proto::Tile {
            layers: vec![mvt::proto::Layer {
                version: 2,
                name: "point".to_owned(),
                features: vec![mvt::proto::Feature {
                    id: None,
                    tags: vec![0, 0],
                    r#type: Some(mvt::proto::GeomType::Point as i32),
                    geometry: vec![9, coord.x % 256 * 2, coord.y * 2],
                }],
                keys: vec!["coord".to_owned()],
                values: vec![mvt::proto::Value {
                    string_value: Some(format!(
                        "{:02}/{:05}/{:05}{}",
                        coord.z,
                        coord.x,
                        coord.y,
                        "a".repeat(8100)
                    )),
                    ..Default::default()
                }],
                extent: Some(4096),
            }],
        };
        let data = mvt::encode(&tile);
        assert_eq!(mvt::decode(&data).unwrap(), tile);
        let fingerprint = blake3::hash(&data);
        Self { data, fingerprint }
    }
}

struct Backend {
    requests: Vec<Fixture>,
    export: Vec<Fixture>,
    calls: Vec<AtomicUsize>,
    export_calls: AtomicUsize,
    delayed: AtomicBool,
}

impl Backend {
    fn new() -> Self {
        Self {
            requests: (0..HOT + WARM).map(|x| Fixture::new(coord(x, 0))).collect(),
            export: (0..EXPORT).map(|x| Fixture::new(coord(x, 1))).collect(),
            calls: (0..HOT + WARM).map(|_| AtomicUsize::new(0)).collect(),
            export_calls: AtomicUsize::new(0),
            delayed: AtomicBool::new(false),
        }
    }

    fn fixture(&self, coord: TileCoord) -> &Fixture {
        assert_eq!(coord.z, ZOOM);
        match coord.y {
            0 => &self.requests[coord.x as usize],
            1 => &self.export[coord.x as usize],
            _ => panic!("unexpected fixture coordinate {coord:?}"),
        }
    }

    fn check(&self, coord: TileCoord, outcome: TileOutcome) {
        let TileOutcome::Present(tile) = outcome else {
            panic!("{coord:?} returned {outcome:?}");
        };
        assert_eq!(tile.info, MVT);
        assert_eq!(
            blake3::hash(&tile.data),
            self.fixture(coord).fingerprint,
            "{coord:?}"
        );
    }
}

#[async_trait]
impl TileDriver for Backend {
    fn descriptor(&self) -> SourceDescriptor {
        let mut descriptor = SourceDescriptor::new(TARGET, MVT, ContentVersion::from_input(b"v1"));
        descriptor.capabilities.consistent_export = true;
        descriptor
    }

    async fn read(
        &self,
        coord: TileCoord,
        _: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        if coord.y == 0 {
            self.calls[coord.x as usize].fetch_add(1, Relaxed);
        } else {
            self.export_calls.fetch_add(1, Relaxed);
        }
        if self.delayed.load(Relaxed) {
            sleep(DELAY).await;
        }
        Ok(Some(TileResponse {
            data: self.fixture(coord).data.clone(),
            info: MVT,
            last_modified: None,
        }))
    }
}

#[derive(Default)]
struct Samples {
    latencies: Vec<u128>,
    backend_calls: usize,
}

impl Samples {
    fn add(&mut self, elapsed_ns: u128, backend_calls: usize) {
        self.latencies.push(elapsed_ns);
        self.backend_calls += backend_calls;
    }

    fn summary(mut self) -> (usize, usize, u128) {
        self.latencies.sort_unstable();
        let n = self.latencies.len();
        (
            n,
            self.backend_calls,
            self.latencies[(n * 95).div_ceil(100) - 1],
        )
    }
}

async fn single(stack: &TileStack, backend: &Backend, x: usize) -> (u128, usize) {
    let coord = coord(x, 0);
    let calls = &backend.calls[x];
    let before = calls.load(Relaxed);
    let now = Instant::now();
    let result = stack.read(target().at(coord)).await.unwrap();
    let elapsed = now.elapsed().as_nanos();
    backend.check(coord, result);
    let missed = calls.load(Relaxed) - before;
    assert!(missed <= 1);
    (elapsed, missed)
}

fn check_page(backend: &Backend, coords: &[TileCoord], page: BatchPage) {
    assert_eq!(page.consumed, coords.len());
    assert_eq!(page.tiles.len(), coords.len());
    for (&coord, outcome) in coords.iter().zip(page.tiles) {
        backend.check(coord, outcome);
    }
}

#[tokio::main(worker_threads = 4)]
async fn main() {
    let mode = std::env::args().nth(1);
    let export = match mode.as_deref() {
        Some("control" | "contract") => false,
        Some("export") => true,
        _ => panic!("usage: export_cache_pressure control|export|contract"),
    };
    let backend = Arc::new(Backend::new());
    let cache = TileCache::new(1, None, None).unwrap();
    let engine = Arc::new(CatalogService::new(cache.clone()));
    let mut update = engine.catalog().update();
    update.put(backend.clone()).unwrap();
    engine.catalog().publish(update).unwrap();
    let stack = TileStack::builder(engine).build();

    if mode.as_deref() == Some("contract") {
        let coords = [coord(0, 1)];
        let session = stack.begin_export(target()).unwrap();
        let mut calls = Vec::new();
        for _ in 0..2 {
            check_page(&backend, &coords, session.read_page(&coords).await.unwrap());
            calls.push(backend.export_calls.load(Relaxed));
        }
        backend.check(coords[0], stack.read(target().at(coords[0])).await.unwrap());
        calls.push(backend.export_calls.load(Relaxed));
        check_page(&backend, &coords, session.read_page(&coords).await.unwrap());
        calls.push(backend.export_calls.load(Relaxed));
        println!("{calls:?}");
        return;
    }

    for x in 0..HOT + WARM {
        assert_eq!(single(&stack, &backend, x).await.1, 1);
    }
    cache.run_pending_tasks().await;
    assert_eq!(cache.entry_count(), (HOT + WARM) as u64);
    let pre_weighted = cache.weighted_size();
    assert!(pre_weighted <= 1024 * 1024);
    for x in 0..HOT + WARM {
        assert_eq!(single(&stack, &backend, x).await.1, 0);
    }
    for _ in 0..16 {
        for x in 0..HOT {
            assert_eq!(single(&stack, &backend, x).await.1, 0);
        }
    }
    cache.run_pending_tasks().await;
    backend.delayed.store(true, Relaxed);

    let mut export_jobs = JoinSet::new();
    let go = Arc::new(Barrier::new(if export { WORKERS + 1 } else { 1 }));
    if export {
        for worker in 0..WORKERS {
            let coords: Vec<_> = (worker..EXPORT)
                .step_by(WORKERS)
                .map(|x| coord(x, 1))
                .collect();
            let session = stack.begin_export(target()).unwrap();
            let go = go.clone();
            let backend = backend.clone();
            export_jobs.spawn(async move {
                go.wait().await;
                let start = Instant::now();
                let mut exported = 0;
                for coords in coords.chunks(PAGE) {
                    let page = session.read_page(coords).await.unwrap();
                    check_page(&backend, coords, page);
                    exported += coords.len();
                }
                (exported, start.elapsed().as_micros())
            });
        }
    }
    go.wait().await;
    let start = Instant::now();
    let mut frequent = Samples::default();
    let mut occasional = Samples::default();
    let mut ticks = interval(PACING);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Burst);
    for request in 0..ONLINE {
        ticks.tick().await;
        let occasional_read = request % 4 == 0;
        let (x, samples) = if occasional_read {
            (HOT + (request / 4) % WARM, &mut occasional)
        } else {
            ((request - request.div_ceil(4)) % HOT, &mut frequent)
        };
        let (ns, calls) = single(&stack, &backend, x).await;
        samples.add(ns, calls);
    }
    let online_us = start.elapsed().as_micros();
    // Export workers hit disjoint coordinates. This counts source work initiated
    // while online requests were being served, independently of their timings.
    let during_online = backend.export_calls.load(Relaxed);
    if export {
        assert!((EXPORT / 4..EXPORT).contains(&during_online));
    }
    let mut exported = 0;
    let mut export_us = 0;
    while let Some(joined) = export_jobs.join_next().await {
        let (count, elapsed) = joined.unwrap();
        exported += count;
        export_us = export_us.max(elapsed);
    }
    let expected = if export { EXPORT } else { 0 };
    let export_calls = backend.export_calls.load(Relaxed);
    assert_eq!(exported, expected);
    assert_eq!(export_calls, expected);

    cache.run_pending_tasks().await;
    let after_entries = cache.entry_count();
    let after_weighted = cache.weighted_size();
    assert!(after_weighted <= 1024 * 1024);
    let mut recover_hot = Samples::default();
    let mut recover_warm = Samples::default();
    for x in 0..HOT + WARM {
        let samples = if x < HOT {
            &mut recover_hot
        } else {
            &mut recover_warm
        };
        let (ns, calls) = single(&stack, &backend, x).await;
        samples.add(ns, calls);
    }
    let mut values = vec![
        exported as u128,
        export_calls as u128,
        during_online as u128,
        export_us,
        online_us,
    ];
    for samples in [frequent, occasional, recover_hot, recover_warm] {
        let (n, calls, p95) = samples.summary();
        values.extend([n as u128, calls as u128, p95]);
    }
    values.extend([
        0,
        pre_weighted as u128,
        after_entries as u128,
        after_weighted as u128,
    ]);
    let csv = values
        .iter()
        .map(u128::to_string)
        .collect::<Vec<_>>()
        .join(",");
    println!(concat!(
        "mode,export_completed,export_backend_calls,export_reads_started_by_online_end,",
        "export_us,online_us,frequent_n,frequent_backend_calls,frequent_p95_ns,",
        "occasional_n,occasional_backend_calls,occasional_p95_ns,recovery_frequent_n,",
        "recovery_frequent_backend_calls,recovery_frequent_p95_ns,recovery_occasional_n,",
        "recovery_occasional_backend_calls,recovery_occasional_p95_ns,failures,",
        "pre_weighted_bytes,post_entries,post_weighted_bytes"
    ));
    println!("{},{csv}", mode.unwrap());
}
