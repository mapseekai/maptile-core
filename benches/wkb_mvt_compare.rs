//! Benchmark for the geozero-backed WKB→MVT encoder. Two workloads:
//!
//! - 1000 polygons × 50 vertices each
//! - 1000 points
//!
//! These benchmarks were originally cross-impl comparisons (legacy
//! hand-rolled vs. geozero) under the migration's Phase B.4 acceptance
//! gate (≤ 1.2× regression). After the legacy implementation was
//! deleted, the bench is retained as a single-path baseline so future
//! regressions remain detectable.

use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use maptile_core::wkb_mvt::{WkbFeature, encode_features_to_mvt};
use std::hint::black_box;

const ENV: (f64, f64, f64, f64) = (-180.0, -90.0, 180.0, 90.0);
const EXTENT: u32 = 4096;

fn wkb_point(x: f64, y: f64) -> Bytes {
    let mut v = Vec::with_capacity(1 + 4 + 16);
    v.push(0x01);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(&y.to_le_bytes());
    Bytes::from(v)
}

fn wkb_polygon_n(vertices: usize, cx: f64, cy: f64, radius: f64) -> Bytes {
    use std::f64::consts::TAU;
    let mut ring: Vec<(f64, f64)> = (0..vertices)
        .map(|i| {
            let theta = (i as f64) / (vertices as f64) * TAU;
            (cx + radius * theta.cos(), cy + radius * theta.sin())
        })
        .collect();
    ring.push(ring[0]);

    let mut v = Vec::new();
    v.push(0x01);
    v.extend_from_slice(&3u32.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&(ring.len() as u32).to_le_bytes());
    for (x, y) in ring {
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
    }
    Bytes::from(v)
}

fn build_point_features() -> Vec<WkbFeature> {
    (0..1000)
        .map(|i| {
            let lng = -179.0 + (i as f64) * 0.35;
            let lat = -89.0 + (i as f64 * 0.17) % 178.0;
            WkbFeature {
                id: None,
                wkb: wkb_point(lng, lat),
                properties: serde_json::Map::new(),
            }
        })
        .collect()
}

fn build_polygon_features() -> Vec<WkbFeature> {
    (0..1000)
        .map(|i| {
            let cx = -170.0 + (i as f64) * 0.3;
            let cy = -80.0 + ((i as f64) * 0.23) % 160.0;
            WkbFeature {
                id: None,
                wkb: wkb_polygon_n(50, cx, cy, 1.5),
                properties: serde_json::Map::new(),
            }
        })
        .collect()
}

fn bench_points(c: &mut Criterion) {
    let feats = build_point_features();
    c.bench_function("wkb_mvt_points_1000", |b| {
        b.iter(|| {
            let out = encode_features_to_mvt("L", feats.iter().cloned(), ENV, EXTENT).unwrap();
            black_box(out);
        });
    });
}

fn bench_polygons(c: &mut Criterion) {
    let feats = build_polygon_features();
    c.bench_function("wkb_mvt_polygons_1000x50", |b| {
        b.iter(|| {
            let out = encode_features_to_mvt("L", feats.iter().cloned(), ENV, EXTENT).unwrap();
            black_box(out);
        });
    });
}

criterion_group!(benches, bench_points, bench_polygons);
criterion_main!(benches);
