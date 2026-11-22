//! Measure strict MVT composition at one or eight concurrent reads.
//! Arguments select request count, features per member and `raw`, `gzip` or `both`.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant, SystemTime},
};

use bytes::Bytes;
use maptile_core::{
    cache::{CacheZoomRange, ContentVersion, TileCache},
    control::{
        CatalogService, CompositeDeclaration, DuplicateLayer, MemberFailurePolicy,
        SourceDescriptor, TileDriver,
    },
    error::MaptileResult,
    service::{ReadTarget, TileOutcome, TileStack},
    sources::{
        TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
        mvt::{self, proto},
    },
    transport,
};
use tokio::{sync::Barrier, task::JoinSet};

const WIDE: usize = 8;

struct Source {
    descriptor: SourceDescriptor,
    data: Bytes,
    reads: Vec<AtomicUsize>,
    last_modified: SystemTime,
}

#[async_trait::async_trait]
impl TileDriver for Source {
    fn descriptor(&self) -> SourceDescriptor {
        self.descriptor.clone()
    }

    async fn read(
        &self,
        coord: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Option<TileResponse>> {
        assert_eq!(coord.z, 10);
        assert_eq!(coord.y, 0);
        self.reads[coord.x as usize].fetch_add(1, Relaxed);
        Ok(Some(TileResponse {
            data: self.data.clone(),
            info: self.descriptor.info,
            last_modified: Some(self.last_modified),
        }))
    }
}

fn make_layer(member: usize, feature_count: usize) -> proto::Layer {
    let mut values: Vec<_> = ["primary", "local", "service", "path"]
        .into_iter()
        .map(|class| proto::Value {
            string_value: Some(class.into()),
            ..Default::default()
        })
        .collect();
    let mut features = Vec::with_capacity(feature_count);
    for i in 0..feature_count {
        let seed =
            (i as u32).wrapping_mul(2_654_435_761) ^ (member as u32).wrapping_mul(1_013_904_223);
        let x = (seed >> 12) % 4000;
        let y = (seed.rotate_left(13) >> 11) % 4000;
        let steps = 2 + (seed >> 27) % 7;
        let mut geometry = vec![9, 2 * x, 2 * y, (steps << 3) | 2];
        for step in 0..steps {
            geometry.extend([
                2 + 2 * (seed.rotate_left(step) % 6),
                2 + 2 * (seed >> step) % 10,
            ]);
        }
        values.push(proto::Value {
            uint_value: Some((i as u64 + 1) % 131),
            ..Default::default()
        });
        features.push(proto::Feature {
            id: Some(i as u64 + 1),
            tags: vec![0, seed % 4, 1, i as u32 + 4],
            r#type: Some(proto::GeomType::Linestring as i32),
            geometry,
        });
    }
    proto::Layer {
        version: 2,
        name: format!("roads_{member}"),
        features,
        keys: vec!["class".into(), "speed".into()],
        values,
        extent: Some(4096),
    }
}

fn fixture(
    members: usize,
    feature_count: usize,
    coordinate_count: usize,
    encoding: TileEncoding,
) -> (TileStack, Vec<Arc<Source>>, proto::Tile) {
    let engine = Arc::new(CatalogService::new(TileCache::new(16, None, None).unwrap()));
    let mut update = engine.catalog().update();
    let mut sources = Vec::new();
    let mut layers = Vec::new();
    for member in 0..members {
        let layer = make_layer(member, feature_count);
        let id: Arc<str> = format!("fixture{member}").into();
        let raw = mvt::encode(&proto::Tile {
            layers: vec![layer.clone()],
        });
        let data = transport::encode_as(&raw, encoding).unwrap();
        let mut descriptor = SourceDescriptor::new(
            id,
            TileInfo {
                format: TileFormat::Mvt,
                encoding,
            },
            ContentVersion::from_input(&data),
        );
        descriptor.cache_zoom = CacheZoomRange::disabled();
        descriptor.tilejson.vector_layers = Some(vec![tilejson::VectorLayer::new(
            layer.name.clone(),
            BTreeMap::from([
                ("class".into(), "String".into()),
                ("speed".into(), "Number".into()),
            ]),
        )]);
        let source = Arc::new(Source {
            descriptor,
            data,
            reads: (0..coordinate_count).map(|_| AtomicUsize::new(0)).collect(),
            last_modified: SystemTime::UNIX_EPOCH + Duration::from_secs((member + 1) as u64),
        });
        update.put_trusted_mvt(source.clone()).unwrap();
        sources.push(source);
        layers.push(layer);
    }
    update
        .composite(CompositeDeclaration {
            id: "probe".into(),
            members: sources
                .iter()
                .map(|source| source.descriptor.id.clone())
                .collect(),
            lazy: false,
            duplicate_layer: DuplicateLayer::Error,
            failure_policy: MemberFailurePolicy::FailFast,
        })
        .unwrap();
    engine.catalog().publish(update).unwrap();
    (
        TileStack::builder(engine).build(),
        sources,
        proto::Tile { layers },
    )
}

async fn read(stack: &TileStack, x: usize) -> MaptileResult<TileOutcome> {
    stack
        .read(ReadTarget::new("probe", "benchmark").at(TileCoord {
            z: 10,
            x: x as u32,
            y: 0,
        }))
        .await
}

fn verify(
    result: MaptileResult<TileOutcome>,
    expected: &Bytes,
    last_modified: SystemTime,
) -> TileResponse {
    let Ok(TileOutcome::Present(tile)) = result else {
        panic!("expected present, got {result:?}");
    };
    assert_eq!(tile.info.format, TileFormat::Mvt);
    assert_eq!(tile.info.encoding, TileEncoding::Uncompressed);
    assert_eq!(tile.last_modified, Some(last_modified));
    assert!(tile.data == *expected, "composed bytes differ");
    tile
}

async fn run(
    encoding_name: &str,
    encoding: TileEncoding,
    members: usize,
    parallel: usize,
    requests: usize,
    feature_count: usize,
) {
    let warmup = 2 * parallel;
    let (stack, sources, expected) = fixture(members, feature_count, warmup + requests, encoding);
    let golden = mvt::encode(&expected);
    let last_modified = sources.last().unwrap().last_modified;
    let mut warm = JoinSet::new();
    for x in 0..warmup {
        let stack = stack.clone();
        warm.spawn(async move { read(&stack, x).await });
    }
    while let Some(result) = warm.join_next().await {
        verify(result.unwrap(), &golden, last_modified);
    }
    let start = Arc::new(Barrier::new(parallel + 1));
    let mut tasks = JoinSet::new();
    for worker in 0..parallel {
        let stack = stack.clone();
        let start = start.clone();
        tasks.spawn(async move {
            let mut results = Vec::with_capacity(requests.div_ceil(parallel));
            start.wait().await;
            for x in (warmup + worker..warmup + requests).step_by(parallel) {
                let requested = Instant::now();
                let result = read(&stack, x).await;
                results.push((requested.elapsed().as_nanos(), result));
            }
            results
        });
    }
    let started = Instant::now();
    start.wait().await;
    let mut results = Vec::with_capacity(requests);
    while let Some(group) = tasks.join_next().await {
        results.extend(group.unwrap());
    }
    let elapsed = started.elapsed().as_nanos();
    assert_eq!(results.len(), requests);
    let mut latencies = Vec::with_capacity(requests);
    for (ordinal, (latency, result)) in results.into_iter().enumerate() {
        let tile = verify(result, &golden, last_modified);
        if ordinal % requests.div_ceil(parallel) == 0 {
            assert!(
                mvt::decode(&tile.data).unwrap() == expected,
                "decoded layers differ"
            );
        }
        latencies.push(latency);
    }
    let read_count: usize = sources
        .iter()
        .map(|source| {
            assert!(source.reads.iter().all(|count| count.load(Relaxed) == 1));
            source.reads[warmup..]
                .iter()
                .map(|count| count.load(Relaxed))
                .sum::<usize>()
        })
        .sum();
    latencies.sort_unstable();
    let percentile = |p: usize| latencies[(requests * p).div_ceil(100) - 1];
    let input_bytes: usize = sources.iter().map(|s| s.data.len()).sum();
    println!(
        "{encoding_name},{members},{parallel},{feature_count},{requests},{elapsed},{},{},{read_count},{},{},0",
        percentile(50),
        percentile(95),
        input_bytes * requests,
        golden.len() * requests,
    );
}

#[tokio::main(worker_threads = 4)]
async fn main() {
    let mut args = std::env::args().skip(1);
    let requests = args.next().map_or(96, |s| s.parse::<usize>().unwrap());
    let feature_count = args.next().map_or(2400, |s| s.parse::<usize>().unwrap());
    let encoding = args.next().unwrap_or_else(|| "both".into());
    assert!(requests >= WIDE && requests + 2 * WIDE <= 1024);
    assert!(feature_count > 0 && matches!(encoding.as_str(), "raw" | "gzip" | "both"));
    println!(
        "encoding,members,parallel,features_per_member,requests,wall_ns,p50_ns,p95_ns,reads,encoded_bytes,output_bytes,failures"
    );
    for (name, codec) in [
        ("raw", TileEncoding::Uncompressed),
        ("gzip", TileEncoding::Gzip),
    ] {
        if encoding == "both" || encoding == name {
            for members in [1, 4, 8] {
                for parallel in [1, WIDE] {
                    run(name, codec, members, parallel, requests, feature_count).await;
                }
            }
        }
    }
}
