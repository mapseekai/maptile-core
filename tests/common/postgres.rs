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
        let name = unique_schema();
        let (client, conn) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
            .await
            .expect("connect for schema setup");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        client
            .batch_execute(&format!("CREATE SCHEMA \"{name}\""))
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
                            .batch_execute(&format!("DROP SCHEMA \"{name}\" CASCADE"))
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
