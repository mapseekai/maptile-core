use std::cell::Cell;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, black_box, criterion_group, criterion_main};
use futures::future::join_all;
use maptile_core::render::native::{benchmark_lifecycle_counts, reset_benchmark_lifecycle_counts};
use maptile_core::render::{PoolConfig, RenderFormat, RenderRequest, RenderedImage, RendererPool};
use tokio::runtime::Runtime;

const STYLE: &str = include_str!("../testdata/render-style.json");
const CONCURRENT_WORKERS: usize = 2;
const CONCURRENT_JOBS: usize = 4;

fn pool(max_concurrent: usize) -> RendererPool {
    RendererPool::new(PoolConfig {
        max_concurrent,
        default_size: (64, 48),
        queue_depth_per_worker: CONCURRENT_JOBS,
    })
    .expect("native renderer pool should initialize")
}

fn request(width: u32, height: u32) -> RenderRequest {
    let mut request = RenderRequest::for_tile(STYLE, 0, 0, 0, 1, RenderFormat::Png);
    request.width = width;
    request.height = height;
    request
}

fn render(runtime: &Runtime, pool: &RendererPool, request: RenderRequest) -> RenderedImage {
    runtime
        .block_on(pool.render(request))
        .expect("native render should succeed")
}

fn report_lifecycle_counts(group_name: &str) {
    let counts = benchmark_lifecycle_counts();
    eprintln!(
        "{group_name} lifecycle counts: renderer_constructions={}, successful_style_loads={}",
        counts.renderer_constructions, counts.successful_style_loads
    );
}

fn measure_renderer_cold_renders(
    runtime: &Runtime,
    pool: &RendererPool,
    iterations: u64,
) -> Duration {
    let mut elapsed = Duration::ZERO;
    for _ in 0..iterations {
        // The measured call must reach the production rotation threshold.
        // One success already exists from either the group prewarm or the
        // preceding measured rotation; 30 more make the next call cold.
        for _ in 0..30 {
            black_box(render(runtime, pool, request(64, 48)).bytes.len());
        }

        let cold_request = request(64, 48);
        let before = benchmark_lifecycle_counts();
        let started = Instant::now();
        black_box(render(runtime, pool, cold_request).bytes.len());
        elapsed += started.elapsed();
        let after = benchmark_lifecycle_counts();

        assert_eq!(
            after.renderer_constructions,
            before.renderer_constructions + 1
        );
        assert_eq!(
            after.successful_style_loads,
            before.successful_style_loads + 1
        );
    }
    elapsed
}

fn cold_first_render(criterion: &mut Criterion, runtime: &Runtime) {
    // Groups execute sequentially and all jobs have completed before reset.
    reset_benchmark_lifecycle_counts();
    let pool = pool(1);
    black_box(render(runtime, &pool, request(64, 48)).bytes.len());

    let mut group = criterion.benchmark_group("renderer_cold_rotation");
    group.throughput(Throughput::Elements(1));
    group.bench_function("64x48", |bencher| {
        bencher.iter_custom(|iterations| measure_renderer_cold_renders(runtime, &pool, iterations));
    });
    group.finish();
    report_lifecycle_counts("renderer_cold_rotation");
}

fn warm_same_style(criterion: &mut Criterion, runtime: &Runtime) {
    reset_benchmark_lifecycle_counts();
    let pool = pool(1);
    let request = request(64, 48);
    black_box(render(runtime, &pool, request.clone()));

    let mut group = criterion.benchmark_group("warm_same_style");
    group.throughput(Throughput::Elements(1));
    group.bench_function("64x48", |bencher| {
        bencher.iter_batched(
            || request.clone(),
            |request| black_box(render(runtime, &pool, request).bytes.len()),
            BatchSize::SmallInput,
        );
    });
    group.finish();
    report_lifecycle_counts("warm_same_style");
}

fn warm_mixed_size(criterion: &mut Criterion, runtime: &Runtime) {
    reset_benchmark_lifecycle_counts();
    let pool = pool(1);
    let requests = [request(64, 48), request(96, 64)];
    for request in &requests {
        black_box(render(runtime, &pool, request.clone()));
    }
    let next = Cell::new(0_usize);

    let mut group = criterion.benchmark_group("warm_mixed_size");
    group.throughput(Throughput::Elements(1));
    group.bench_function("64x48_and_96x64", |bencher| {
        bencher.iter_batched(
            || {
                let index = next.get();
                next.set((index + 1) % requests.len());
                requests[index].clone()
            },
            |request| black_box(render(runtime, &pool, request).bytes.len()),
            BatchSize::SmallInput,
        );
    });
    group.finish();
    report_lifecycle_counts("warm_mixed_size");
}

fn concurrent_pool(criterion: &mut Criterion, runtime: &Runtime) {
    reset_benchmark_lifecycle_counts();
    let pool = pool(CONCURRENT_WORKERS);
    let requests: Vec<_> = (0..CONCURRENT_JOBS)
        .map(|index| request(64 + index as u32 * 8, 48))
        .collect();
    let warmup = requests.iter().cloned().map(|request| pool.render(request));
    for result in runtime.block_on(join_all(warmup)) {
        black_box(result.expect("concurrent warm-up render should succeed"));
    }

    let mut group = criterion.benchmark_group("concurrent_pool");
    group.throughput(Throughput::Elements(CONCURRENT_JOBS as u64));
    group.bench_function("2_workers_4_jobs", |bencher| {
        bencher.iter_batched(
            || requests.clone(),
            |requests| {
                let jobs = requests.into_iter().map(|request| pool.render(request));
                let rendered = runtime.block_on(join_all(jobs));
                for result in rendered {
                    black_box(result.expect("concurrent render should succeed"));
                }
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
    report_lifecycle_counts("concurrent_pool");
}

fn render_native(criterion: &mut Criterion) {
    let runtime = Runtime::new().expect("Tokio runtime should initialize");
    cold_first_render(criterion, &runtime);
    warm_same_style(criterion, &runtime);
    warm_mixed_size(criterion, &runtime);
    concurrent_pool(criterion, &runtime);
}

criterion_group!(benches, render_native);
criterion_main!(benches);
