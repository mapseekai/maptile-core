//! PostgreSQL connection pool with PostGIS version probing and optional rustls TLS.

use deadpool_postgres::{
    Manager, ManagerConfig, Object, Pool, PoolError, QueueMode, RecyclingMethod, Runtime, Status,
    TimeoutType, Timeouts,
};
use futures_util::{StreamExt, future::join_all};
use semver::Version;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_postgres::config::SslMode;
use tokio_postgres::{CancelToken, Client, Config, NoTls};

use crate::cache::ContentVersion;
use crate::control::{CatalogBuildLease, SourceReadLease};
use crate::error::{MaptileError, MaptileResult};
use crate::transport::{CATALOG_BUILD_LEASE, SOURCE_READ_LEASE};

use super::{MINIMUM_POSTGIS_VERSION, MINIMUM_POSTGRES_VERSION, ST_TILE_ENVELOPE_MARGIN_VERSION};

/// Pool tunables. Sessions default to 4 MiB `work_mem` and 30 s
/// `statement_timeout`. Override either through the DSN's `options`.
#[derive(Clone, Debug)]
pub struct PoolSettings {
    pub max_size: usize,
    /// Maximum active batch operations, including cancellation cleanup, per pool.
    /// Clones share the budget; separately connected pools have independent budgets.
    pub batch_concurrency: usize,
    pub wait_timeout_ms: u64,
    pub create_timeout_ms: u64,
    pub recycle_timeout_ms: u64,
    pub pre_warm: bool,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            max_size: 16,
            batch_concurrency: 2,
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
    batch_admission: Arc<Semaphore>,
    postgres_version: Version,
    postgis_version: Version,
    supports_tile_margin: bool,
    // Connections and cancellation share the trust snapshot loaded at pool creation.
    tls_config: Option<rustls::ClientConfig>,
    pub(super) database_identity: ContentVersion,
}

impl PostgresPool {
    pub async fn connect(dsn: &str, settings: PoolSettings) -> MaptileResult<Self> {
        if !(1..=Semaphore::MAX_PERMITS).contains(&settings.max_size) {
            return Err(MaptileError::Config(
                "PostgreSQL max_size must be a valid positive pool size".into(),
            ));
        }
        if !(1..=Semaphore::MAX_PERMITS).contains(&settings.batch_concurrency) {
            return Err(MaptileError::Config(format!(
                "batch_concurrency must be between 1 and {}",
                Semaphore::MAX_PERMITS
            )));
        }
        if [
            settings.wait_timeout_ms,
            settings.create_timeout_ms,
            settings.recycle_timeout_ms,
        ]
        .contains(&0)
        {
            return Err(MaptileError::Config(
                "PostgreSQL pool timeouts must be positive".into(),
            ));
        }
        let mut pg_config =
            Config::from_str(dsn).map_err(|e| MaptileError::Config(format!("invalid DSN: {e}")))?;
        // Startup options apply to every connection. DSN settings can tune the resource defaults.
        pg_config.options(format!(
            "-c work_mem=4MB -c statement_timeout=30s {} -c jit=off",
            pg_config.get_options().unwrap_or_default()
        ));
        crate::transport::finish_discovery_open(Self::connect_pool(pg_config, settings)).await
    }

    async fn connect_pool(pg_config: Config, settings: PoolSettings) -> MaptileResult<Self> {
        let tls_config = match pg_config.get_ssl_mode() {
            SslMode::Disable => None,
            _ => Some(build_rustls_config()?),
        };
        let pool = build_pool(pg_config.clone(), &settings, tls_config.as_ref())?;

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
            batch_admission: Arc::new(Semaphore::new(settings.batch_concurrency)),
            postgres_version,
            postgis_version,
            supports_tile_margin,
            tls_config,
            database_identity,
        })
    }

    pub async fn get(&self) -> MaptileResult<Object> {
        self.pool.get().await.map_err(|error| match error {
            PoolError::Timeout(TimeoutType::Wait) => MaptileError::CapacityExhausted,
            error => MaptileError::backend(error),
        })
    }

    /// Current PostgreSQL connection pool occupancy.
    pub fn connection_status(&self) -> Status {
        self.pool.status()
    }

    /// Available batch slots. `None` means backend completion could not be confirmed and admission closed.
    pub fn batch_slots_available(&self) -> Option<usize> {
        (!self.batch_admission.is_closed()).then(|| self.batch_admission.available_permits())
    }

    pub(super) async fn get_query(self: &Arc<Self>) -> MaptileResult<QueryConnection> {
        self.query_connection(None).await
    }

    pub(super) async fn get_batch(self: &Arc<Self>) -> MaptileResult<QueryConnection> {
        let permit = Arc::clone(&self.batch_admission)
            .acquire_owned()
            .await
            .map_err(|_| {
                MaptileError::BackendMsg(
                    "archive SQL admission closed after unconfirmed backend completion; verify PostgreSQL before recreating this pool".into(),
                )
            })?;
        self.query_connection(Some(permit)).await
    }

    async fn query_connection(
        self: &Arc<Self>,
        permit: Option<OwnedSemaphorePermit>,
    ) -> MaptileResult<QueryConnection> {
        Ok(QueryConnection {
            pool: Arc::clone(self),
            pending: Some(QueryResources {
                connection: Some(self.get().await?),
                batch: permit.map(|permit| BatchPermit {
                    permit,
                    ready: false,
                }),
                lease: SOURCE_READ_LEASE.try_with(Arc::clone).ok(),
                build: CATALOG_BUILD_LEASE.try_with(Arc::clone).ok(),
            }),
        })
    }

    async fn cancel_query(&self, token: CancelToken) -> MaptileResult<()> {
        match &self.tls_config {
            None => token.cancel_query(NoTls).await,
            Some(config) => {
                token
                    .cancel_query(tokio_postgres_rustls::MakeRustlsConnect::new(
                        config.clone(),
                    ))
                    .await
            }
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

pub(super) fn validate_source_window(
    minzoom: Option<u8>,
    maxzoom: Option<u8>,
    bounds: Option<[f64; 4]>,
) -> MaptileResult<()> {
    let minzoom = minzoom.unwrap_or(0);
    let maxzoom = maxzoom.unwrap_or(crate::sources::tile_math::MAX_ZOOM);
    if minzoom > maxzoom || maxzoom > crate::sources::tile_math::MAX_ZOOM {
        return Err(MaptileError::Config(
            "invalid PostgreSQL source zoom range".into(),
        ));
    }
    if bounds.is_some_and(|bounds| !bounds.into_iter().all(f64::is_finite)) {
        return Err(MaptileError::Config(
            "PostgreSQL bounds must be finite".into(),
        ));
    }
    Ok(())
}

pub(super) struct QueryConnection {
    pool: Arc<PostgresPool>,
    pending: Option<QueryResources>,
}
impl QueryConnection {
    pub(super) fn connection(&self) -> &Object {
        self.pending
            .as_ref()
            .expect("query is in progress")
            .connection()
    }

    pub(super) async fn finish(mut self) -> MaptileResult<()> {
        let pending = self.pending.as_ref().expect("query is in progress");
        if pending.batch.is_some() || pending.build.is_some() {
            wait_for_ready(pending.connection())
                .await
                .map_err(MaptileError::backend)?;
        }
        self.pending.take().unwrap().release(false);
        Ok(())
    }
}
impl Drop for QueryConnection {
    fn drop(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = Arc::clone(&self.pool);
        runtime.spawn(async move {
            let result = {
                let connection = pending.connection();
                let drain = wait_for_ready(connection);
                tokio::pin!(drain);
                tokio::select! {
                    result = &mut drain => result,
                    result = tokio::time::timeout(
                        Duration::from_secs(5), pool.cancel_query(connection.cancel_token())
                    ) => {
                        if !matches!(result, Ok(Ok(()))) {
                            tracing::warn!("PostgreSQL cancellation failed; waiting for backend completion");
                        }
                        drain.await
                    }
                }
            };
            if result.is_ok() {
                pending.release(true);
            }
        });
    }
}

struct QueryResources {
    connection: Option<Object>,
    batch: Option<BatchPermit>,
    lease: Option<Arc<SourceReadLease>>,
    build: Option<Arc<CatalogBuildLease>>,
}

impl QueryResources {
    fn connection(&self) -> &Object {
        self.connection.as_ref().expect("query connection is held")
    }

    fn release(mut self, interrupted: bool) {
        let connection = self.connection.take().expect("query connection is held");
        if interrupted {
            // A delayed cancel must never reach a later query on this backend.
            drop(Object::take(connection));
        } else {
            drop(connection);
        }
        if let Some(batch) = self.batch.take() {
            batch.release();
        }
    }
}

impl Drop for QueryResources {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            drop(Object::take(connection));
            if let Some(lease) = &self.lease {
                lease.mark_unconfirmed();
            }
            if let Some(build) = &self.build {
                build.mark_unconfirmed();
            }
        }
    }
}

// Dropping a socket does not prove the remote SQL stopped. Unknown completion stops
// archive admission for this pool; ordinary online pool access remains available.
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
                "archive SQL admission closed: backend completion unconfirmed; verify PostgreSQL before recreating this pool"
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

fn build_pool(
    pg_config: Config,
    settings: &PoolSettings,
    tls_config: Option<&rustls::ClientConfig>,
) -> MaptileResult<Pool> {
    let mgr_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };

    let mgr = match tls_config {
        None => Manager::from_config(pg_config, NoTls, mgr_config),
        Some(config) => Manager::from_config(
            pg_config,
            tokio_postgres_rustls::MakeRustlsConnect::new(config.clone()),
            mgr_config,
        ),
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

fn build_rustls_config() -> MaptileResult<rustls::ClientConfig> {
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
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| MaptileError::Config(format!("TLS protocol versions: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(cfg)
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
    use super::{BatchPermit, PoolSettings, PostgresPool, parse_loose_version};
    use semver::Version;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn invalid_pool_sizes_and_timeouts_are_rejected_before_connecting() {
        for (settings, expected) in [
            (
                PoolSettings {
                    max_size: 0,
                    ..Default::default()
                },
                "max_size",
            ),
            (
                PoolSettings {
                    max_size: Semaphore::MAX_PERMITS + 1,
                    ..Default::default()
                },
                "max_size",
            ),
            (
                PoolSettings {
                    wait_timeout_ms: 0,
                    ..Default::default()
                },
                "timeouts",
            ),
            (
                PoolSettings {
                    create_timeout_ms: 0,
                    ..Default::default()
                },
                "timeouts",
            ),
            (
                PoolSettings {
                    recycle_timeout_ms: 0,
                    ..Default::default()
                },
                "timeouts",
            ),
        ] {
            let error = PostgresPool::connect("invalid dsn", settings)
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                crate::error::MaptileError::Config(message) if message.contains(expected)
            ));
        }
    }

    #[tokio::test]
    async fn invalid_batch_concurrency_is_rejected_before_connecting() {
        for batch_concurrency in [0, Semaphore::MAX_PERMITS + 1] {
            let error = PostgresPool::connect(
                "invalid dsn",
                PoolSettings {
                    batch_concurrency,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                crate::error::MaptileError::Config(message) if message.starts_with("batch_concurrency")
            ));
        }
    }

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
