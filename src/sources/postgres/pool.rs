//! PostgreSQL connection pool with PostGIS version probing and optional rustls TLS.

use deadpool_postgres::{
    Manager, ManagerConfig, Object, Pool, QueueMode, RecyclingMethod, Runtime, Timeouts,
};
use futures_util::{StreamExt, future::join_all};
use semver::Version;
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_postgres::config::SslMode;
use tokio_postgres::{CancelToken, Client, Config, NoTls};

use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};

use super::{MINIMUM_POSTGIS_VERSION, MINIMUM_POSTGRES_VERSION, ST_TILE_ENVELOPE_MARGIN_VERSION};

static BATCH_ADMISSION: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));

/// Pool tunables. Defaults aim at small server workloads — tweak per deployment.
#[derive(Clone, Debug)]
pub struct PoolSettings {
    pub max_size: usize,
    pub wait_timeout_ms: u64,
    pub create_timeout_ms: u64,
    pub recycle_timeout_ms: u64,
    pub pre_warm: bool,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            max_size: 16,
            wait_timeout_ms: 5_000,
            create_timeout_ms: 5_000,
            recycle_timeout_ms: 5_000,
            pre_warm: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PostgresPool {
    pool: Pool,
    postgres_version: Version,
    postgis_version: Version,
    supports_tile_margin: bool,
    ssl_mode: SslMode,
    pub(super) database_identity: ContentVersion,
}

impl PostgresPool {
    pub async fn connect(dsn: &str, settings: PoolSettings) -> MaptileResult<Self> {
        let mut pg_config =
            Config::from_str(dsn).map_err(|e| MaptileError::Config(format!("invalid DSN: {e}")))?;
        // JIT compilation costs more than it saves for short tile queries, including on replacement connections.
        pg_config.options(format!(
            "{} -c jit=off",
            pg_config.get_options().unwrap_or_default()
        ));

        let pool = build_pool(pg_config.clone(), &settings)?;

        // Probe versions from a connection acquired from the pool — also
        // validates connectivity at startup time.
        let conn = pool.get().await.map_err(MaptileError::backend)?;

        let postgres_version = probe_postgres_version(&conn).await?;
        if postgres_version < MINIMUM_POSTGRES_VERSION {
            return Err(MaptileError::Config(format!(
                "PostgreSQL {postgres_version} < required {MINIMUM_POSTGRES_VERSION}"
            )));
        }

        let postgis_version = probe_postgis_version(&conn).await?;
        if postgis_version < MINIMUM_POSTGIS_VERSION {
            return Err(MaptileError::Config(format!(
                "PostGIS {postgis_version} < required {MINIMUM_POSTGIS_VERSION}"
            )));
        }

        let supports_tile_margin = postgis_version >= ST_TILE_ENVELOPE_MARGIN_VERSION;
        let origin = (
            pg_config.get_hosts(),
            pg_config.get_hostaddrs(),
            pg_config.get_ports(),
            pg_config.get_user(),
            pg_config.get_dbname(),
            pg_config.get_options(),
        );
        let database_identity = ContentVersion::from_input(format!("{origin:?}").as_bytes());
        drop(conn);

        if settings.pre_warm {
            // Retain distinct connections until startup prewarming finishes; no request traffic exists yet.
            let connections = join_all((0..settings.max_size).map(|_| pool.get())).await;
            for result in &connections {
                if let Err(error) = result {
                    tracing::warn!("startup connection prewarm failed: {error}");
                }
            }
        }

        Ok(Self {
            pool,
            postgres_version,
            postgis_version,
            supports_tile_margin,
            ssl_mode: pg_config.get_ssl_mode(),
            database_identity,
        })
    }

    pub async fn get(&self) -> MaptileResult<Object> {
        self.pool.get().await.map_err(MaptileError::backend)
    }

    pub(crate) async fn get_batch(self: &Arc<Self>) -> MaptileResult<BatchConnection> {
        let permit = Arc::clone(&BATCH_ADMISSION)
            .acquire_owned()
            .await
            .map_err(|_| {
                MaptileError::BackendMsg(
                    "archive SQL admission closed after unconfirmed backend completion; verify PostgreSQL and restart this process".into(),
                )
            })?;
        Ok(BatchConnection {
            connection: Some(self.get().await?),
            pool: Arc::clone(self),
            permit: Some(BatchPermit {
                permit,
                ready: false,
            }),
        })
    }

    pub(crate) async fn cancel_query(&self, token: CancelToken) -> MaptileResult<()> {
        match self.ssl_mode {
            SslMode::Disable | SslMode::Prefer => token.cancel_query(NoTls).await,
            SslMode::Require => token.cancel_query(build_rustls_connector(false)?).await,
            _ => token.cancel_query(build_rustls_connector(true)?).await,
        }
        .map_err(MaptileError::backend)
    }

    pub fn postgres_version(&self) -> &Version {
        &self.postgres_version
    }

    pub fn postgis_version(&self) -> &Version {
        &self.postgis_version
    }

    pub fn supports_tile_margin(&self) -> bool {
        self.supports_tile_margin
    }
}

pub(crate) struct BatchConnection {
    connection: Option<Object>,
    pool: Arc<PostgresPool>,
    permit: Option<BatchPermit>,
}
impl BatchConnection {
    pub(crate) fn connection(&self) -> &Object {
        self.connection.as_ref().expect("batch connection is held")
    }

    pub(crate) async fn finish(mut self) -> MaptileResult<()> {
        wait_for_ready(self.connection())
            .await
            .map_err(MaptileError::backend)?;
        drop(self.connection.take());
        self.permit.take().expect("batch permit is held").release();
        Ok(())
    }
}
impl Drop for BatchConnection {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        // A delayed cancel must never hit a later query on a reused backend.
        let connection = Object::take(connection);
        let permit = self.permit.take().expect("batch permit is held");
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = Arc::clone(&self.pool);
        runtime.spawn(async move {
            // Each cleanup retains its SQL slot, so at most two can wait in the background.
            let drain = wait_for_ready(&connection);
            tokio::pin!(drain);
            let result = tokio::select! {
                result = &mut drain => result,
                result = tokio::time::timeout(
                    Duration::from_secs(5), pool.cancel_query(connection.cancel_token())
                ) => {
                    if !matches!(result, Ok(Ok(()))) {
                        tracing::warn!("archive batch cancellation failed; waiting for backend completion");
                    }
                    drain.await
                }
            };
            if result.is_ok() {
                permit.release();
            }
            // Failure or task/runtime interruption closes admission through BatchPermit::drop.
        });
    }
}

// Dropping a socket does not prove the remote SQL stopped. Unknown completion stops
// archive admission for this process; ordinary online pool access remains available.
struct BatchPermit {
    permit: OwnedSemaphorePermit,
    ready: bool,
}
impl BatchPermit {
    fn release(mut self) {
        self.ready = true;
    }
}
impl Drop for BatchPermit {
    fn drop(&mut self) {
        if !self.ready {
            self.permit.semaphore().close();
            tracing::error!(
                "archive SQL admission closed: backend completion unconfirmed; verify PostgreSQL and restart this process"
            );
        }
    }
}

async fn wait_for_ready(connection: &Client) -> Result<(), tokio_postgres::Error> {
    let responses = connection.simple_query_raw("").await?;
    let mut responses = std::pin::pin!(responses);
    while let Some(result) = responses.next().await {
        if let Err(error) = result
            && error.as_db_error().is_none()
        {
            return Err(error);
        }
        // A late cancel may produce ErrorResponse; keep reading this stream to ReadyForQuery.
    }
    Ok(())
}

fn build_pool(pg_config: Config, settings: &PoolSettings) -> MaptileResult<Pool> {
    let mgr_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };

    let mgr = match pg_config.get_ssl_mode() {
        SslMode::Disable | SslMode::Prefer => Manager::from_config(pg_config, NoTls, mgr_config),
        SslMode::Require => {
            let connector = build_rustls_connector(false)?;
            Manager::from_config(pg_config, connector, mgr_config)
        }
        // VerifyCa / VerifyFull and any future variants both want a real cert
        // chain; rustls-native-certs supplies the platform store.
        _ => {
            let connector = build_rustls_connector(true)?;
            Manager::from_config(pg_config, connector, mgr_config)
        }
    };

    let timeouts = Timeouts {
        wait: Some(Duration::from_millis(settings.wait_timeout_ms)),
        create: Some(Duration::from_millis(settings.create_timeout_ms)),
        recycle: Some(Duration::from_millis(settings.recycle_timeout_ms)),
    };

    Pool::builder(mgr)
        .max_size(settings.max_size)
        // Reuse hot connections and their prepared statements before touching idle cold ones.
        .queue_mode(QueueMode::Lifo)
        .timeouts(timeouts)
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(|e| MaptileError::Config(format!("pool build: {e}")))
}

fn build_rustls_connector(
    _strict_hostname: bool,
) -> MaptileResult<tokio_postgres_rustls::MakeRustlsConnect> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if native.certs.is_empty() && !native.errors.is_empty() {
        return Err(MaptileError::Config(format!(
            "load native CA bundle: {:?}",
            native.errors
        )));
    }
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(cfg))
}

async fn probe_postgres_version(conn: &Object) -> MaptileResult<Version> {
    let row = conn
        .query_one("SHOW server_version", &[])
        .await
        .map_err(MaptileError::backend)?;
    let raw: String = row.get(0);
    parse_loose_version(&raw)
        .ok_or_else(|| MaptileError::Config(format!("unparseable server_version: {raw}")))
}

async fn probe_postgis_version(conn: &Object) -> MaptileResult<Version> {
    let row = conn
        .query_opt(
            "SELECT extversion FROM pg_extension WHERE extname = 'postgis'",
            &[],
        )
        .await
        .map_err(MaptileError::backend)?
        .ok_or_else(|| MaptileError::Config("PostGIS extension not installed".to_string()))?;
    let raw: String = row.get(0);
    parse_loose_version(&raw)
        .ok_or_else(|| MaptileError::Config(format!("unparseable postgis version: {raw}")))
}

/// Accept '11', '11.5', '11.5.0', '14.2 (Debian 14.2-1.pgdg110+1)' etc.
fn parse_loose_version(raw: &str) -> Option<Version> {
    let head: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let parts: Vec<u64> = head.split('.').filter_map(|s| s.parse().ok()).collect();
    match parts.as_slice() {
        [a] => Some(Version::new(*a, 0, 0)),
        [a, b] => Some(Version::new(*a, *b, 0)),
        [a, b, c, ..] => Some(Version::new(*a, *b, *c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{BatchPermit, parse_loose_version};
    use semver::Version;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn confirmed_batch_permit_restores_capacity() {
        let admission = Arc::new(Semaphore::new(2));
        let permit = BatchPermit {
            permit: admission.clone().acquire_owned().await.unwrap(),
            ready: false,
        };
        permit.release();
        assert!(!admission.is_closed());
        assert_eq!(admission.available_permits(), 2);
    }

    #[test]
    fn runtime_shutdown_closes_unconfirmed_batch_admission() {
        let admission = Arc::new(Semaphore::new(2));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let permit = BatchPermit {
            permit: admission.clone().try_acquire_owned().unwrap(),
            ready: false,
        };
        runtime.spawn(async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        });
        drop(runtime);
        assert!(admission.is_closed());
        assert!(admission.try_acquire().is_err());
    }

    #[tokio::test]
    async fn aborted_cleanup_closes_unconfirmed_batch_admission() {
        let admission = Arc::new(Semaphore::new(2));
        let permit = BatchPermit {
            permit: admission.clone().acquire_owned().await.unwrap(),
            ready: false,
        };
        let cleanup = tokio::spawn(async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        });
        cleanup.abort();
        let _ = cleanup.await;
        assert!(admission.is_closed());
        assert!(admission.try_acquire().is_err());
    }

    #[test]
    fn parses_pg_version_strings() {
        assert_eq!(parse_loose_version("11"), Some(Version::new(11, 0, 0)));
        assert_eq!(parse_loose_version("14.2"), Some(Version::new(14, 2, 0)));
        assert_eq!(
            parse_loose_version("16.1 (Debian 16.1-1.pgdg110+1)"),
            Some(Version::new(16, 1, 0))
        );
        assert_eq!(parse_loose_version("3.4.0"), Some(Version::new(3, 4, 0)));
        assert_eq!(parse_loose_version("garbage"), None);
    }
}
