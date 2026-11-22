//! Each source owns its pool. Request tickets outlive abandoned join handles.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

use deadpool::{
    Runtime,
    managed::{Manager, Metrics, Pool, PoolError, QueueMode, RecycleError, Timeouts},
};
use duckdb::Connection;
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

use super::{
    duckdb_query::{FinishQuery, QueryControl},
    geoparquet_geometry::{GeoParquetQueryOptions, literal, load_spatial, spatial_database},
    geoparquet_remote::{DuckDbSecret, RemoteLocation},
};
use crate::error::{MaptileError, MaptileResult};

pub(super) type FileLease = Arc<dyn Send + Sync>;

/// Snapshot of admission and usage for one source's DuckDB pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuckDbPoolStatus {
    pub accepting_requests: bool,
    /// Includes queued, creating, recycling, and interrupted requests.
    pub active_requests: usize,
    pub max_connections: usize,
    /// Managed connections reported by deadpool. In-progress creates are
    /// already counted in active_requests.
    pub connections: usize,
    pub idle_connections: usize,
    pub available_work_slots: usize,
}

pub(super) struct QueryPool {
    pool: Pool<SpatialManager>,
    admission: Arc<Admission>,
    pub(super) slots: Arc<Semaphore>,
    max_connections: usize,
}

struct Admission {
    state: Mutex<State>,
    idle: Notify,
}

#[derive(Default)]
struct State {
    closed: bool,
    resources_released: bool,
    next: u64,
    active: HashMap<u64, Arc<QueryControl>>,
    file_lease: Option<FileLease>,
    snapshot_file: Option<Arc<tempfile::TempPath>>,
}

impl Admission {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct Request {
    admission: Arc<Admission>,
    id: u64,
    control: Arc<QueryControl>,
    deadline: Instant,
    permit: Option<OwnedSemaphorePermit>,
    file_lease: Option<FileLease>,
    snapshot_file: Option<Arc<tempfile::TempPath>>,
    extra_lease: Option<FileLease>,
    secret: Option<Arc<DuckDbSecret>>,
    _catalog_lease: Option<Arc<crate::control::SourceReadLease>>,
    _build_lease: Option<Arc<crate::control::CatalogBuildLease>>,
}

impl Request {
    fn check(&self) -> MaptileResult<()> {
        if Instant::now() >= self.deadline {
            return Err(MaptileError::Deadline);
        }
        if self.control.cancelled() {
            return Err(MaptileError::Cancelled);
        }
        if let Some(secret) = &self.secret {
            secret.check()?;
        }
        Ok(())
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        drop(FinishQuery(self.control.clone()));
        drop(self.file_lease.take());
        drop(self.snapshot_file.take());
        drop(self.extra_lease.take());
        drop(self.permit.take());
        self.admission.lock().active.remove(&self.id);
        self.admission.idle.notify_waiters();
    }
}

tokio::task_local! {
    static REQUEST: Arc<Request>;
}

#[derive(Clone)]
pub(super) struct SpatialManager {
    options: GeoParquetQueryOptions,
    path: Arc<str>,
    remote: Option<Arc<RemoteLocation>>,
}

pub(super) struct DuckDbConnection {
    conn: Option<Connection>,
    tainted: Arc<AtomicBool>,
    secret: Option<Arc<DuckDbSecret>>,
}

impl SpatialManager {
    pub fn new(
        options: GeoParquetQueryOptions,
        path: &str,
        remote: Option<Arc<RemoteLocation>>,
    ) -> Self {
        Self {
            options,
            path: path.into(),
            remote,
        }
    }

    pub fn open(
        &self,
        control: &Arc<QueryControl>,
        secret: Option<&DuckDbSecret>,
    ) -> MaptileResult<Connection> {
        let conn = spatial_database(&self.options)?;
        let attachment = control.bind(conn.interrupt_handle(), None);
        if control.cancelled() {
            return Err(MaptileError::Cancelled);
        }
        load_spatial(&conn)?;
        if self.remote.is_some() {
            RemoteLocation::configure(&conn, self.options.timeout_ms, secret)?;
        }
        conn.execute_batch(&format!(
            "SET allowed_paths = [{}]; SET enable_external_access = false;",
            literal(&self.path)
        ))
        .map_err(MaptileError::backend)?;
        drop(attachment);
        Ok(conn)
    }
}

impl Manager for SpatialManager {
    type Type = DuckDbConnection;
    type Error = MaptileError;

    async fn create(&self) -> MaptileResult<DuckDbConnection> {
        let request = REQUEST.with(Arc::clone);
        request.check()?;
        let manager = self.clone();
        let (connection, _request) = tokio::task::spawn_blocking(move || {
            request.check()?;
            let conn = manager.open(&request.control, request.secret.as_deref())?;
            request.check()?;
            // Retain the ticket in the result until deadpool accepts the
            // connection or a timed-out join drops it.
            MaptileResult::Ok((
                DuckDbConnection {
                    conn: Some(conn),
                    tainted: Arc::new(AtomicBool::new(false)),
                    secret: request.secret.clone(),
                },
                request,
            ))
        })
        .await
        .map_err(MaptileError::backend)??;
        Ok(connection)
    }

    async fn recycle(
        &self,
        object: &mut DuckDbConnection,
        _: &Metrics,
    ) -> Result<(), RecycleError<MaptileError>> {
        let request = REQUEST.with(Arc::clone);
        request.check().map_err(RecycleError::Backend)?;
        if object.tainted.load(Ordering::Acquire) {
            return Err(RecycleError::message("interrupted DuckDB connection"));
        }
        let conn = object.conn.take().expect("checked out connection");
        let tainted = object.tainted.clone();
        let replaced = object.secret != request.secret;
        let job = (conn, request);
        let (conn, secret, _request) = tokio::task::spawn_blocking(move || {
            let job = job;
            let (conn, request) = &job;
            let binding = request.control.bind(conn.interrupt_handle(), Some(tainted));
            request.check()?;
            if replaced && let Some(secret) = &request.secret {
                secret.install(conn)?;
            }
            conn.execute_batch("SELECT 1")
                .map_err(MaptileError::backend)?;
            request.check()?;
            drop(binding);
            let (conn, request) = job;
            Ok((conn, request.secret.clone(), request))
        })
        .await
        .map_err(MaptileError::backend)
        .and_then(|result| result)
        .map_err(RecycleError::Backend)?;
        object.conn = Some(conn);
        object.secret = secret;
        Ok(())
    }
}

impl QueryPool {
    pub(super) fn new(manager: SpatialManager) -> MaptileResult<Self> {
        let size = manager.options.pool_size;
        let pool = Pool::builder(manager)
            .max_size(size)
            .queue_mode(QueueMode::Lifo)
            .runtime(Runtime::Tokio1)
            .build()
            .map_err(MaptileError::backend)?;
        Ok(Self {
            pool,
            admission: Arc::new(Admission {
                state: Mutex::new(State::default()),
                idle: Notify::new(),
            }),
            slots: Arc::new(Semaphore::new(size)),
            max_connections: size,
        })
    }

    pub fn with_file_lease(&self, lease: FileLease) {
        let previous = {
            let mut state = self.admission.lock();
            if state.closed {
                return;
            }
            state.file_lease.replace(lease)
        };
        drop(previous);
    }

    pub(super) fn keep_snapshot_file(&self, path: tempfile::TempPath) {
        self.admission.lock().snapshot_file = Some(Arc::new(path));
    }

    pub fn start_close(&self) {
        let leases = {
            let mut state = self.admission.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            self.slots.close();
            for query in state.active.values() {
                query.cancel();
            }
            (state.file_lease.take(), state.snapshot_file.take())
        };
        self.pool.close();
        if let Some(remote) = &self.pool.manager().remote {
            remote.close();
        }
        drop(leases);
        self.admission.lock().resources_released = true;
        self.admission.idle.notify_waiters();
    }

    pub async fn close(&self) {
        self.start_close();
        loop {
            let idle = self.admission.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            {
                let state = self.admission.lock();
                if state.resources_released && state.active.is_empty() {
                    return;
                }
            }
            idle.await;
        }
    }

    pub fn status(&self) -> DuckDbPoolStatus {
        let admission = self.admission.lock();
        let status = self.pool.status();
        DuckDbPoolStatus {
            accepting_requests: !admission.closed,
            active_requests: admission.active.len(),
            max_connections: self.max_connections,
            connections: status.size,
            idle_connections: status.available,
            available_work_slots: if admission.closed {
                0
            } else {
                self.slots.available_permits()
            },
        }
    }

    pub async fn run<T, F>(
        &self,
        deadline: Instant,
        file_lease: Option<FileLease>,
        execute: F,
    ) -> MaptileResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> MaptileResult<T> + Send + 'static,
    {
        let control = QueryControl::pending();
        let mut request = {
            let mut state = self.admission.lock();
            if state.closed {
                return Err(MaptileError::Cancelled);
            }
            let id = state.next;
            state.next += 1;
            state.active.insert(id, control.clone());
            Request {
                admission: self.admission.clone(),
                id,
                control: control.clone(),
                deadline,
                permit: None,
                file_lease: state.file_lease.clone(),
                snapshot_file: state.snapshot_file.clone(),
                extra_lease: file_lease,
                secret: None,
                _catalog_lease: crate::transport::SOURCE_READ_LEASE
                    .try_with(Arc::clone)
                    .ok(),
                _build_lease: crate::transport::CATALOG_BUILD_LEASE
                    .try_with(Arc::clone)
                    .ok(),
            }
        };
        let _watch = control.watch(deadline);
        let bounded = async {
            request.check()?;
            let permit = self
                .slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| MaptileError::Cancelled)?;
            request.permit = Some(permit);
            request.check()?;
            if let Some(remote) = &self.pool.manager().remote {
                request.secret = remote.credentials().await?;
            }
            request.check()?;
            MaptileResult::Ok(())
        };
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => return Err(MaptileError::Deadline),
            () = control.wait_cancelled() => return Err(MaptileError::Cancelled),
            result = bounded => result?,
        }
        let request = Arc::new(request);
        let pool = self.pool.clone();
        let task = tokio::spawn(async move {
            // Deadpool may abandon create or recycle at the phase timeout.
            // Their workers retain the request and slot until they really exit.
            let budget = Some(deadline.saturating_duration_since(Instant::now()));
            let timeouts = Timeouts {
                wait: budget,
                create: budget,
                recycle: budget,
            };
            let object = REQUEST
                .scope(request.clone(), pool.timeout_get(&timeouts))
                .await;
            request.check()?;
            let object = object.map_err(|error| match error {
                PoolError::Timeout(_) => MaptileError::Deadline,
                PoolError::Closed => MaptileError::Cancelled,
                PoolError::Backend(err) => err,
                _ => MaptileError::BackendMsg("DuckDB pool could not lend a connection".into()),
            })?;
            let binding = request.control.bind(
                object
                    .conn
                    .as_ref()
                    .expect("checked out connection")
                    .interrupt_handle(),
                Some(object.tainted.clone()),
            );
            // Tuple drop order also disarms and returns the connection
            // when a blocking job is discarded before it starts.
            let job = (binding, object, request);
            tokio::task::spawn_blocking(move || {
                let job = job;
                let (_, object, request) = &job;
                let conn = object.conn.as_ref().expect("checked out connection");
                let result = request.check().and_then(|()| execute(conn));
                request.check().and(result)
            })
            .await
            .map_err(MaptileError::backend)?
        });
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => Err(MaptileError::Deadline),
            () = control.wait_cancelled() => Err(MaptileError::Cancelled),
            result = task => result.map_err(MaptileError::backend)?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn pool() -> Arc<QueryPool> {
        Arc::new(
            QueryPool::new(SpatialManager::new(
                GeoParquetQueryOptions {
                    pool_size: 1,
                    ..Default::default()
                },
                "/maptile-pool-test",
                None,
            ))
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn file_lease_cleanup_can_inspect_the_pool_and_close_waits_for_it() {
        struct Lease {
            pool: std::sync::Weak<QueryPool>,
            entered: Option<oneshot::Sender<bool>>,
            resume: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
        }

        impl Drop for Lease {
            fn drop(&mut self) {
                let accepting = self.pool.upgrade().unwrap().status().accepting_requests;
                let _ = self.entered.take().unwrap().send(accepting);
                if let Some(resume) = &self.resume {
                    let _ = resume.lock().unwrap().recv_timeout(Duration::from_secs(10));
                }
            }
        }

        let pool = pool();
        let (first, replaced) = oneshot::channel();
        pool.with_file_lease(Arc::new(Lease {
            pool: Arc::downgrade(&pool),
            entered: Some(first),
            resume: None,
        }));
        let (second, retiring) = oneshot::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let pool = pool.clone();
            move || {
                pool.with_file_lease(Arc::new(Lease {
                    pool: Arc::downgrade(&pool),
                    entered: Some(second),
                    resume: Some(Mutex::new(resume)),
                }));
                pool.start_close();
            }
        });

        let timeout = Duration::from_secs(5);
        assert!(
            tokio::time::timeout(timeout, replaced)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(
            !tokio::time::timeout(timeout, retiring)
                .await
                .unwrap()
                .unwrap()
        );
        let closing = pool.close();
        tokio::pin!(closing);
        assert!(futures_util::poll!(&mut closing).is_pending());
        release.send(()).unwrap();
        tokio::time::timeout(timeout, closing).await.unwrap();
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn cancellation_dirties_the_borrow_and_next_query_gets_a_fresh_connection() {
        let pool = pool();
        let deadline = Instant::now() + Duration::from_secs(10);
        let (entered, outside) = oneshot::channel();
        let (resume, release) = std::sync::mpsc::channel();
        let running = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.run(deadline, None, move |conn| {
                    conn.execute_batch("CREATE TEMP TABLE sentinel AS SELECT 1 AS value")
                        .map_err(MaptileError::backend)?;
                    entered.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .await
            }
        });
        outside.await.unwrap();
        // The cancelled query still owns the only blocking slot and connection.
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());
        let status = pool.status();
        assert_eq!((status.active_requests, status.connections), (1, 1));
        resume.send(()).unwrap();
        pool.run(deadline, None, |conn| {
            let count = conn
                .query_row(
                    "SELECT count(*) FROM duckdb_tables() WHERE table_name='sentinel'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(MaptileError::backend)?;
            assert_eq!(count, 0);
            Ok(())
        })
        .await
        .unwrap();
        pool.close().await;
        let status = pool.status();
        assert_eq!((status.active_requests, status.connections), (0, 0));
    }

    #[test]
    fn closing_keeps_a_create_ticket_and_file_lease_until_the_worker_exits() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                let pool = pool();
                let (blocked, ready) = oneshot::channel();
                let (resume, released) = std::sync::mpsc::channel();
                let worker = tokio::task::spawn_blocking(move || {
                    blocked.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(10)).unwrap();
                });
                ready.await.unwrap();
                let lease = Arc::new(());
                let retained = Arc::downgrade(&lease);
                pool.with_file_lease(lease);
                let request = tokio::spawn({
                    let pool = pool.clone();
                    async move {
                        pool.run(Instant::now() + Duration::from_secs(10), None, |_| {
                            panic!("closed query must never execute")
                        })
                        .await as MaptileResult<()>
                    }
                });
                tokio::time::timeout(Duration::from_secs(2), async {
                    while pool.pool.status().waiting == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let mut close = Box::pin(pool.close());
                assert!(futures_util::poll!(&mut close).is_pending());
                let status = pool.status();
                assert_eq!((status.active_requests, status.connections), (1, 0));
                assert!(retained.upgrade().is_some());
                assert!(matches!(
                    request.await.unwrap(),
                    Err(MaptileError::Cancelled)
                ));
                resume.send(()).unwrap();
                worker.await.unwrap();
                tokio::time::timeout(Duration::from_secs(2), close)
                    .await
                    .unwrap();
                assert!(retained.upgrade().is_none());
                let status = pool.status();
                assert_eq!((status.active_requests, status.connections), (0, 0));
            });
    }
}
