//! Open and execute bounded geometry queries through DuckDB Spatial.

mod bounds;
mod resolver;
#[cfg(test)]
mod tests;
mod tile_sql;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use duckdb::ToSql;

pub use self::bounds::BoundsMode;
use self::bounds::scan_bounds;
pub(super) use self::bounds::valid_bounds;
use self::resolver::{InputKind, resolve_input};
use self::tile_sql::{SpatialQuery, build_sql};
use super::config;
use super::geoparquet::GeoParquetSourceConfig;
use super::pool::{DuckDbPoolStatus, QueryPool, SpatialManager};
use super::query::{FinishQuery, GeoParquetQueryOptions, QueryControl};
use super::remote::{DuckDbSecret, RemoteLocation, remote_error};
use crate::error::{MaptileError, MaptileResult};
use crate::io::file_stamp::FileStamp;
use crate::sources::{TileCoord, TileQuery};

pub(super) struct GeometrySource {
    pool: Arc<QueryPool>,
    query: SpatialQuery,
    local: Option<(PathBuf, FileStamp)>,
    remote: bool,
    options: GeoParquetQueryOptions,
    pub bounds: Option<[f64; 4]>,
    pub fields: BTreeMap<String, String>,
}

impl GeometrySource {
    pub fn open(cfg: &GeoParquetSourceConfig) -> MaptileResult<Self> {
        Self::open_local(cfg, InputKind::GeoParquet)
    }

    pub fn open_flatgeobuf(cfg: &GeoParquetSourceConfig) -> MaptileResult<Self> {
        Self::open_local(cfg, InputKind::FlatGeobuf)
    }

    fn open_local(cfg: &GeoParquetSourceConfig, input: InputKind) -> MaptileResult<Self> {
        cfg.query_options.validate()?;
        let path = single_file_path(&cfg.path)?;
        let stamp = FileStamp::read(&path)?;
        let location = path
            .to_str()
            .ok_or_else(|| config("path must be UTF-8"))?
            .to_owned();
        let running = QueryControl::pending();
        let _finished = FinishQuery(running.clone());
        let mut source = Self::open_input(
            cfg,
            &location,
            Some((path, stamp)),
            None,
            &running,
            None,
            input,
        )?;
        // Local open is synchronous. Use the same interrupt controller from
        // a scoped watchdog for the optional display-bounds scan.
        if source.bounds.is_none() && cfg.bounds_mode != BoundsMode::Skip {
            let manager = SpatialManager::new(cfg.query_options.clone(), &location, None);
            let (done, finished) = std::sync::mpsc::channel();
            source.bounds = std::thread::scope(|threads| {
                let watchdog = running.clone();
                threads.spawn(move || {
                    if matches!(
                        finished.recv_timeout(Duration::from_millis(cfg.query_options.timeout_ms)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ) {
                        loop {
                            watchdog.cancel();
                            if !matches!(
                                finished.recv_timeout(Duration::from_millis(10)),
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                            ) {
                                break;
                            }
                        }
                    }
                });
                let bounds = (|| {
                    let conn = manager.open(&running, None).ok()?;
                    let _binding = running.bind(conn.interrupt_handle(), None);
                    scan_bounds(&conn, &source.query.bounds_sql, cfg.bounds_mode)
                })();
                let _ = done.send(());
                if running.cancelled() { None } else { bounds }
            });
            check_stamp(&source.local)?;
        }
        Ok(source)
    }

    pub fn open_remote(
        cfg: &GeoParquetSourceConfig,
        remote: Arc<RemoteLocation>,
        running: &Arc<QueryControl>,
        secret: Option<&DuckDbSecret>,
    ) -> MaptileResult<Self> {
        let url = remote.url.clone();
        let result = Self::open_input(
            cfg,
            &url,
            None,
            Some(remote),
            running,
            secret,
            InputKind::GeoParquet,
        );
        if running.cancelled() {
            Err(MaptileError::Cancelled)
        } else {
            result.map_err(remote_error)
        }
    }

    pub(super) fn open_snapshot(
        cfg: &GeoParquetSourceConfig,
        snapshot: tempfile::TempPath,
        control: &Arc<QueryControl>,
    ) -> MaptileResult<Self> {
        let path = single_file_path(&snapshot)?;
        let stamp = FileStamp::read(&path)?;
        let location = path.to_str().ok_or_else(|| config("path must be UTF-8"))?;
        let source = Self::open_input(
            cfg,
            location,
            Some((path.clone(), stamp)),
            None,
            control,
            None,
            InputKind::GeoParquet,
        )?;
        source.pool.keep_snapshot_file(snapshot);
        Ok(source)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_input(
        cfg: &GeoParquetSourceConfig,
        location: &str,
        local: Option<(PathBuf, FileStamp)>,
        remote: Option<Arc<RemoteLocation>>,
        running: &Arc<QueryControl>,
        secret: Option<&DuckDbSecret>,
        input: InputKind,
    ) -> MaptileResult<Self> {
        if cfg.extent == 0 || cfg.extent > 65_536 || cfg.buffer > cfg.extent {
            return Err(config("extent must be 1..=65536 and buffer <= extent"));
        }
        if cfg.layer_name.trim().is_empty() || cfg.layer_name.contains('\0') {
            return Err(config("layer_name must be nonempty and contain no NUL"));
        }
        if running.cancelled() {
            return Err(MaptileError::Cancelled);
        }
        let manager = SpatialManager::new(cfg.query_options.clone(), location, remote.clone());
        let conn = manager.open(running, secret)?;
        let attachment = running.bind(conn.interrupt_handle(), None);
        if running.cancelled() {
            return Err(MaptileError::Cancelled);
        }

        let resolved = resolve_input(&conn, cfg, location, input)?;
        let query = build_sql(cfg, &resolved, input);
        conn.prepare(&query.sql).map_err(MaptileError::backend)?;
        drop(attachment);
        check_stamp(&local)?;
        Ok(Self {
            pool: Arc::new(QueryPool::new(manager)?),
            query,
            local,
            remote: remote.is_some(),
            options: cfg.query_options.clone(),
            bounds: resolved.bounds,
            fields: resolved.fields,
        })
    }

    pub fn with_file_lease(&self, lease: Arc<dyn Send + Sync>) {
        self.pool.with_file_lease(lease);
    }

    pub fn pool_status(&self) -> DuckDbPoolStatus {
        self.pool.status()
    }

    pub fn start_close(&self) {
        self.pool.start_close();
    }

    pub async fn close(&self) -> MaptileResult<()> {
        self.pool.close().await;
        Ok(())
    }

    pub async fn display_bounds(&mut self, mode: BoundsMode, deadline: tokio::time::Instant) {
        if self.bounds.is_none() && mode != BoundsMode::Skip {
            let sql = self.query.bounds_sql.clone();
            self.bounds = self
                .pool
                .run(deadline, None, move |conn| {
                    Ok(scan_bounds(conn, &sql, mode))
                })
                .await
                .ok()
                .flatten();
        }
    }

    #[cfg(test)]
    async fn tile(
        &self,
        xyz: TileCoord,
        lease: Option<Arc<dyn Send + Sync>>,
    ) -> MaptileResult<bytes::Bytes> {
        self.read_tile(xyz, lease, None).await
    }

    pub async fn read_tile(
        &self,
        xyz: TileCoord,
        file_lease: Option<Arc<dyn Send + Sync>>,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<bytes::Bytes> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.options.timeout_ms);
        let bound = self.query.bind_tile(xyz, query, &self.fields)?;
        let local = self.local.clone();
        let options = self.options.clone();
        let result = self
            .pool
            .run(deadline, file_lease, move |conn| {
                check_stamp(&local)?;
                let params = bound
                    .params
                    .iter()
                    .map(|(name, value)| (name.as_str(), value as &dyn ToSql))
                    .collect::<Vec<_>>();
                let result = conn
                    .prepare_cached(&bound.sql)
                    .and_then(|mut s| {
                        s.query_row(params.as_slice(), |r| {
                            Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, i64>(1)?))
                        })
                    })
                    .map_err(MaptileError::backend);
                check_stamp(&local)?;
                let (data, outcome) = result?;
                if outcome > options.max_features as i64 {
                    return Err(MaptileError::Resource(
                        "tile exceeds configured feature count".into(),
                    ));
                }
                if outcome == -1 {
                    return Err(MaptileError::PayloadTooLarge {
                        limit: options.max_tile_bytes,
                    });
                }
                if outcome == -2 {
                    return Err(MaptileError::Resource(
                        "tile exceeds query input byte budget".into(),
                    ));
                }
                Ok(bytes::Bytes::from(data.unwrap_or_default()))
            })
            .await;
        if self.remote {
            result.map_err(remote_error)
        } else {
            result
        }
    }
}

fn check_stamp(local: &Option<(PathBuf, FileStamp)>) -> MaptileResult<()> {
    if let Some((path, stamp)) = local
        && !matches!(FileStamp::read(path), Ok(current) if current == *stamp)
    {
        return Err(MaptileError::VersionChanged("local geometry file".into()));
    }
    Ok(())
}

pub(super) fn validate_single_file_path(path: &Path) -> MaptileResult<()> {
    let text = path.to_str().ok_or_else(|| config("path must be UTF-8"))?;
    if text
        .chars()
        .any(|c| matches!(c, '*' | '?' | '[' | ']' | '\0'))
        || text.contains("://")
    {
        return Err(config(
            "GeoParquet requires a local single file, not a URL or glob",
        ));
    }
    Ok(())
}

pub(super) fn single_file_path(path: &Path) -> MaptileResult<PathBuf> {
    validate_single_file_path(path)?;
    let path = normalized_local_path(path.canonicalize()?)?;
    if !path.is_file() {
        return Err(config("GeoParquet input must be a regular file"));
    }
    Ok(path)
}

// Rust's Windows canonicalizer adds a verbatim prefix; expose ordinary absolute
// paths so the same lexical validation can be used when reopening an import.
pub(super) fn normalized_local_path(path: PathBuf) -> MaptileResult<PathBuf> {
    #[cfg(windows)]
    let path = {
        let text = path.to_str().ok_or_else(|| config("path must be UTF-8"))?;
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{rest}"))
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            PathBuf::from(rest)
        } else {
            path
        }
    };
    validate_single_file_path(&path)?;
    Ok(path)
}

pub(super) fn unused_name(prefix: &str, columns: &BTreeMap<String, String>) -> String {
    let mut name = prefix.to_owned();
    while columns.keys().any(|c| c.eq_ignore_ascii_case(&name)) {
        name.push('_');
    }
    name
}
