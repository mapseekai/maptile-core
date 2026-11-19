//! Shared helpers for postgres integration tests.
//!
//! Tests opt in by reading `DATABASE_URL`; when unset they print a notice
//! and return early so `cargo test` is a no-op without a live database.

// Shared helpers; some items may be unused until specific tests pull them in.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the DSN if `DATABASE_URL` is set; otherwise prints a skip notice
/// and returns `None`. Callers should `let dsn = match pg_dsn() { Some(d) => d, None => return };`.
pub fn pg_dsn() -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(d) => Some(d),
        Err(_) => {
            eprintln!("DATABASE_URL unset — skipping postgres test");
            None
        }
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generates a unique schema name like `maptile_test_1730000000000_42`.
pub fn unique_schema() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("maptile_test_{nanos}_{n}")
}

/// RAII helper: drops the schema (and everything in it) when it goes out of scope.
/// Failures during drop are logged to stderr but never panic, so they don't mask
/// the actual test failure.
pub struct SchemaGuard {
    pub name: String,
    dsn: String,
}

impl SchemaGuard {
    pub async fn create(dsn: &str) -> Self {
        Self::create_named(dsn, unique_schema()).await
    }

    pub async fn create_named(dsn: &str, name: String) -> Self {
        let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
            .await
            .expect("connect for schema setup");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        client
            .batch_execute(&format!(
                "CREATE SCHEMA {}",
                pg_escape::quote_identifier(&name)
            ))
            .await
            .expect("create schema");
        Self {
            name,
            dsn: dsn.to_string(),
        }
    }
}

impl Drop for SchemaGuard {
    fn drop(&mut self) {
        let dsn = self.dsn.clone();
        let name = self.name.clone();
        // Spawn a blocking drop so we don't require an executor here.
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("SchemaGuard drop: runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                match tokio_postgres::connect(&dsn, tokio_postgres::NoTls).await {
                    Ok((client, conn)) => {
                        tokio::spawn(async move {
                            let _ = conn.await;
                        });
                        if let Err(e) = client
                            .batch_execute(&format!(
                                "DROP SCHEMA {} CASCADE",
                                pg_escape::quote_identifier(&name)
                            ))
                            .await
                        {
                            eprintln!("SchemaGuard drop: {e}");
                        }
                    }
                    Err(e) => eprintln!("SchemaGuard drop connect: {e}"),
                }
            });
        });
    }
}

pub async fn assert_invalid_tile_requests(
    source: &dyn maptile_core::sources::TileSource,
    style: Option<&maptile_core::raster::style::EffectiveStyle>,
) {
    use maptile_core::{
        error::MaptileError,
        filter::parse_cql2,
        sources::{TileCoord, TileQuery},
    };
    let valid = TileCoord { z: 0, x: 0, y: 0 };
    let filter = parse_cql2("1 = 1").unwrap();
    let filtered = TileQuery {
        filter: Some(&filter),
        style,
        ..Default::default()
    };
    let bounded = TileQuery {
        bbox: Some([-180.0, -90.0, 180.0, 90.0]),
        style,
        ..Default::default()
    };
    for coord in [
        TileCoord { z: 0, x: 1, y: 0 },
        TileCoord { z: 1, x: 10, y: 10 },
        TileCoord {
            z: 30,
            x: 1 << 30,
            y: 0,
        },
        TileCoord {
            z: 30,
            x: 0,
            y: 1 << 30,
        },
        TileCoord {
            z: 31,
            x: 1 << 31,
            y: 0,
        },
        TileCoord {
            z: 255,
            x: u32::MAX,
            y: u32::MAX,
        },
    ] {
        for query in [None, Some(&filtered), Some(&bounded)] {
            let error = source.get_tile(coord, query).await.unwrap_err();
            assert!(
                matches!(error, MaptileError::InvalidRequest(_)),
                "{coord:?}: {error:?}"
            );
            let error = source
                .read_tile_batch(&[valid, coord], query)
                .await
                .unwrap_err();
            assert!(
                matches!(error, MaptileError::InvalidRequest(_)),
                "batch {coord:?}: {error:?}"
            );
        }
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for index in 0..4 {
            let mut bbox = [-180.0, -90.0, 180.0, 90.0];
            bbox[index] = value;
            let query = TileQuery {
                bbox: Some(bbox),
                style,
                ..Default::default()
            };
            let error = source.get_tile(valid, Some(&query)).await.unwrap_err();
            assert!(
                matches!(error, MaptileError::InvalidRequest(_)),
                "{bbox:?}: {error:?}"
            );
            for coords in [&[valid][..], &[][..]] {
                let error = source
                    .read_tile_batch(coords, Some(&query))
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, MaptileError::InvalidRequest(_)),
                    "batch {bbox:?}: {error:?}"
                );
            }
        }
    }
}
