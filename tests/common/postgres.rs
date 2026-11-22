//! Shared helpers for postgres integration tests.
//!
//! Tests opt in by reading `DATABASE_URL`; when unset they print a notice
//! and return early so `cargo test` is a no-op without a live database.

// Shared helpers; some items may be unused until specific tests pull them in.
#![allow(dead_code)]

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio_postgres::{Client, Config, Transaction, config::Host};

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

pub const CANCELLATION_WATCHDOG: Duration = Duration::from_secs(5);

pub struct CancellationGate<'a> {
    monitor: &'a Client,
    pub lock_key: i32,
    pub dsn: String,
    proxy: JoinHandle<()>,
    pending: mpsc::Receiver<(i32, oneshot::Sender<()>)>,
}

impl<'a> CancellationGate<'a> {
    pub async fn start(monitor: &'a Client, dsn: &str) -> Self {
        let session = monitor
            .query_one(
                "SELECT pg_backend_pid(), session_user::text, current_database()",
                &[],
            )
            .await
            .unwrap();
        let upstream: Config = dsn.parse().unwrap();
        let host = match upstream.get_hostaddrs().first() {
            Some(ip) => Host::Tcp(ip.to_string()),
            None => upstream.get_hosts().first().unwrap().clone(),
        };
        let port = upstream.get_ports().first().copied().unwrap_or(5432);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let quote = |value: &str| format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"));
        let password = std::str::from_utf8(upstream.get_password().unwrap_or_default()).unwrap();
        let dsn = format!(
            "host=127.0.0.1 port={proxy_port} sslmode=disable user={} password={} dbname={} options={}",
            quote(session.get(1)),
            quote(password),
            quote(session.get(2)),
            quote(upstream.get_options().unwrap_or_default()),
        );
        let (requests, pending) = mpsc::channel(4);
        let proxy = tokio::spawn(async move {
            let mut peers = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.unwrap();
                        peers.spawn(relay_postgres(client, host.clone(), port, requests.clone()));
                    }
                    Some(_) = peers.join_next() => {}
                }
            }
        });
        Self {
            monitor,
            lock_key: session.get(0),
            dsn,
            proxy,
            pending,
        }
    }

    pub async fn hold<'connection>(
        &self,
        holder: &'connection mut Client,
    ) -> Transaction<'connection> {
        let locks = holder.transaction().await.unwrap();
        locks
            .query_one("SELECT pg_advisory_xact_lock($1, 1)", &[&self.lock_key])
            .await
            .unwrap();
        locks
    }

    pub async fn wait_for_query(&self) -> i32 {
        tokio::time::timeout(CANCELLATION_WATCHDOG, async {
            loop {
                let waiter = self.monitor.query_opt(
                    "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND classid = $1::int::oid
                     AND objid = 1 AND objsubid = 2 AND NOT granted",
                    &[&self.lock_key],
                ).await.unwrap();
                if let Some(row) = waiter {
                    return row.get(0);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend never waited on the advisory lock")
    }

    pub async fn pause_cancellation(&mut self, pid: i32) -> oneshot::Sender<()> {
        let (cancelled_pid, release) =
            tokio::time::timeout(CANCELLATION_WATCHDOG, self.pending.recv())
                .await
                .expect("no cancellation request arrived")
                .unwrap();
        assert_eq!(cancelled_pid, pid);
        assert_eq!(self.wait_for_query().await, pid);
        release
    }

    pub async fn assert_inactive(&self, pid: i32) {
        let active: bool = self
            .monitor
            .query_one(
                "SELECT EXISTS (SELECT FROM pg_stat_activity WHERE pid = $1 AND state = 'active')",
                &[&pid],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            !active,
            "PostgreSQL backend {pid} was still active after drain"
        );
    }
}

impl Drop for CancellationGate<'_> {
    fn drop(&mut self) {
        self.proxy.abort();
    }
}

async fn relay_postgres(
    mut client: TcpStream,
    host: Host,
    port: u16,
    requests: mpsc::Sender<(i32, oneshot::Sender<()>)>,
) -> io::Result<()> {
    const CANCEL_REQUEST: [u8; 8] = [0, 0, 0, 16, 4, 210, 22, 46];
    let mut startup = [0; 16];
    client.read_exact(&mut startup[..8]).await?;
    let startup = if startup[..8] == CANCEL_REQUEST {
        client.read_exact(&mut startup[8..]).await?;
        let pid = i32::from_be_bytes(startup[8..12].try_into().unwrap());
        let (release, allowed) = oneshot::channel();
        if requests.send((pid, release)).await.is_err() || allowed.await.is_err() {
            return Ok(());
        }
        &startup[..]
    } else {
        &startup[..8]
    };
    match host {
        Host::Tcp(host) => {
            relay_stream(client, TcpStream::connect((host, port)).await?, startup).await
        }
        #[cfg(unix)]
        Host::Unix(dir) => {
            let server =
                tokio::net::UnixStream::connect(dir.join(format!(".s.PGSQL.{port}"))).await?;
            relay_stream(client, server, startup).await
        }
    }
}

async fn relay_stream(
    mut client: TcpStream,
    mut server: impl AsyncRead + AsyncWrite + Unpin,
    startup: &[u8],
) -> io::Result<()> {
    server.write_all(startup).await?;
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
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
