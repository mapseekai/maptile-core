//! On-demand, bounded GeoParquet queries over local files or source-scoped
//! range readers. DuckDB sees no remote credentials or user URL query strings.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use duckdb::{Connection, InterruptHandle, named_params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use super::TileCoord;
use super::geoparquet::GeoParquetSourceConfig;
use super::geoparquet_remote::RemoteFile;
use crate::error::{MaptileError, MaptileResult};

/// Per-source execution budget, shared by all clones. DuckDB's memory limit is
/// shared by the connections (not multiplied by pool_size); it is not an RSS cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeoParquetQueryOptions {
    pub pool_size: usize,
    pub threads: usize,
    pub memory_limit_mb: usize,
    pub timeout_ms: u64,
    pub max_features: usize,
    pub max_tile_bytes: usize,
}

impl Default for GeoParquetQueryOptions {
    fn default() -> Self {
        Self {
            pool_size: 2,
            threads: 2,
            memory_limit_mb: 256,
            timeout_ms: 10_000,
            max_features: 100_000,
            max_tile_bytes: 8 * 1024 * 1024,
        }
    }
}

impl GeoParquetQueryOptions {
    pub fn validate(&self) -> MaptileResult<()> {
        if !(1..=32).contains(&self.pool_size)
            || !(1..=64).contains(&self.threads)
            || !(32..=65_536).contains(&self.memory_limit_mb)
            || !(1..=300_000).contains(&self.timeout_ms)
            || !(1..=10_000_000).contains(&self.max_features)
            || !(1..=256 * 1024 * 1024).contains(&self.max_tile_bytes)
        {
            return Err(config("invalid GeoParquet execution budget"));
        }
        Ok(())
    }
}

pub(super) struct GeometrySource {
    pool: Arc<QueryPool>,
    sql: Arc<str>,
    local: Option<(PathBuf, FileStamp)>,
    options: GeoParquetQueryOptions,
    pub bounds: Option<[f64; 4]>,
    pub fields: BTreeMap<String, String>,
}

impl GeometrySource {
    pub fn open(cfg: &GeoParquetSourceConfig) -> MaptileResult<Self> {
        let path = single_file_path(&cfg.path)?;
        let stamp = FileStamp::read(&path)?;
        let location = path
            .to_str()
            .ok_or_else(|| config("path must be UTF-8"))?
            .to_owned();
        Self::open_input(cfg, &location, Some((path, stamp)), None)
    }

    pub fn open_remote(
        cfg: &GeoParquetSourceConfig,
        remote: Arc<RemoteFile>,
    ) -> MaptileResult<Self> {
        Self::open_input(cfg, &remote.url.clone(), None, Some(remote))
    }

    fn open_input(
        cfg: &GeoParquetSourceConfig,
        location: &str,
        local: Option<(PathBuf, FileStamp)>,
        remote: Option<Arc<RemoteFile>>,
    ) -> MaptileResult<Self> {
        cfg.query_options.validate()?;
        if cfg.extent == 0 || cfg.extent > 65_536 || cfg.buffer > cfg.extent {
            return Err(config("extent must be 1..=65536 and buffer <= extent"));
        }
        if cfg.layer_name.trim().is_empty() || cfg.layer_name.contains('\0') {
            return Err(config("layer_name must be nonempty and contain no NUL"));
        }
        let geometry = cfg
            .geometry_column
            .as_deref()
            .ok_or_else(|| config("remote sources require geometry_column"))?;
        identifier(geometry)?;
        let db_config = duckdb::Config::default()
            .enable_autoload_extension(false)
            .and_then(|c| c.with("allow_persistent_secrets", "false"))
            .and_then(|c| c.threads(cfg.query_options.threads as i64))
            .and_then(|c| c.max_memory(&format!("{}MB", cfg.query_options.memory_limit_mb)))
            .and_then(|c| c.with("max_temp_directory_size", "0B"))
            .map_err(MaptileError::backend)?;
        let conn =
            Connection::open_in_memory_with_flags(db_config).map_err(MaptileError::backend)?;
        conn.execute_batch("LOAD parquet; LOAD spatial").map_err(|e| MaptileError::BackendMsg(format!(
            "cannot load DuckDB spatial extension; preinstall the signed extension matching the embedded DuckDB version: {e}"
        )))?;
        if remote.is_some() {
            conn.execute_batch("LOAD httpfs").map_err(|e| MaptileError::BackendMsg(format!(
                "cannot load DuckDB httpfs extension; run the explicit geoparquet_extensions preparation command: {e}"
            )))?;
            conn.execute_batch(&format!("SET http_proxy = ''; SET http_retries = 0; SET http_timeout = {}; SET http_keep_alive = false; SET force_download = false;", cfg.query_options.timeout_ms.div_ceil(1000)))
                .map_err(MaptileError::backend)?;
        }
        let path_sql = literal(location);
        conn.execute_batch(&format!(
            "SET allowed_paths = [{path_sql}]; SET enable_external_access = false;"
        ))
        .map_err(MaptileError::backend)?;
        // These operations read footer/schema metadata, not feature rows. The
        // same path works for local files and bounded remote range requests.
        let geo = conn
            .prepare(&format!(
                "SELECT value FROM parquet_kv_metadata({path_sql}) WHERE key = 'geo'::BLOB"
            ))
            .and_then(|mut statement| {
                let mut rows = statement.query([])?;
                rows.next()?.map(|row| row.get::<_, Vec<u8>>(0)).transpose()
            })
            .map_err(MaptileError::backend)?
            .map(|value| serde_json::from_slice::<Value>(&value))
            .transpose()
            .map_err(|e| config(format!("invalid geo metadata: {e}")))?;
        let column = geo
            .as_ref()
            .map(|geo| {
                geo.get("columns")
                    .and_then(|c| c.get(geometry))
                    .ok_or_else(|| config("geometry column is not declared in geo metadata"))
            })
            .transpose()?;
        if let Some(column) = column {
            if column.get("encoding").and_then(Value::as_str) != Some("WKB") {
                return Err(config("only GeoParquet WKB encoding is supported"));
            }
            if column
                .get("edges")
                .and_then(Value::as_str)
                .is_some_and(|v| v != "planar")
            {
                return Err(config("spherical GeoParquet edges are not supported"));
            }
        }
        let srid = resolve_srid(cfg.srid, column)?;
        let from = format!("read_parquet({path_sql})");
        let covering = if matches!(srid, 4326 | 3857) {
            column.and_then(|c| covering_paths(c, &conn, &from))
        } else {
            None
        };
        let bounds = cfg.bounds.or_else(|| metadata_bounds(column, srid));
        let columns: BTreeMap<String, String> = conn
            .prepare(&format!("DESCRIBE SELECT * FROM {from}"))
            .and_then(|mut s| {
                s.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()
            })
            .map_err(MaptileError::backend)?;
        let geom = identifier(geometry)?;
        let geom = match columns.get(geometry).map(String::as_str) {
            Some("BLOB") => format!("ST_GeomFromWKB({geom})"),
            Some(t) if t.starts_with("GEOMETRY") => geom,
            _ => return Err(config("geometry column must be WKB or GEOMETRY")),
        };
        // Explicit always_xy is required: GeoParquet uses x/y, even when EPSG
        // defines latitude-first axes. Never encode degrees against an MVT grid.
        let projected =
            format!("ST_Transform({geom}, 'EPSG:{srid}', 'EPSG:3857', always_xy := true)");
        // Validate the CRS even for empty datasets, without scanning data.
        conn.query_row(&format!(
            "SELECT ST_AsText(ST_Transform(ST_Point(0,0), 'EPSG:{srid}', 'EPSG:3857', always_xy := true))"
        ), [], |r| r.get::<_, String>(0)).map_err(|e| config(format!("unsupported CRS: {e}")))?;
        let selected: HashSet<&str> = cfg
            .properties
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_default();
        if cfg.properties.is_some() && selected.iter().any(|p| !columns.contains_key(*p)) {
            return Err(config("requested property column is absent"));
        }
        let mut fields = BTreeMap::new();
        let mut properties = Vec::new();
        for (name, ty) in &columns {
            if name == geometry
                || cfg.id_column.as_ref() == Some(name)
                || (cfg.properties.is_some() && !selected.contains(name.as_str()))
            {
                continue;
            }
            if let Some((cast, field)) = property_type(ty) {
                let quoted = identifier(name)?;
                properties.push(format!("{quoted}::{cast} AS {quoted}"));
                fields.insert(name.clone(), field.to_owned());
            } else if cfg.properties.is_some() {
                return Err(config(format!(
                    "property {name:?} has no MVT representation"
                )));
            } else {
                tracing::debug!(column = %name, data_type = %ty, "GeoParquet property omitted from MVT");
            }
        }
        let geom_alias = unused_name("__maptile_geom", &columns);
        let id_alias = unused_name("__maptile_id", &columns);
        let id_arg = if let Some(id) = &cfg.id_column {
            let ty = columns
                .get(id)
                .ok_or_else(|| config("id column is absent"))?;
            if !matches!(
                ty.as_str(),
                "TINYINT"
                    | "SMALLINT"
                    | "INTEGER"
                    | "BIGINT"
                    | "UTINYINT"
                    | "USMALLINT"
                    | "UINTEGER"
                    | "UBIGINT"
            ) {
                return Err(config("id column must be an integer"));
            }
            let quoted = identifier(id)?;
            // Negative IDs have no MVT representation. Avoid wrapping them.
            properties.push(format!(
                "CASE WHEN {quoted} >= 0 THEN {quoted}::BIGINT END AS {id_alias}"
            ));
            format!(", {}", literal(&id_alias))
        } else {
            String::new()
        };
        let sql = build_sql(
            cfg,
            &from,
            &projected,
            &geom_alias,
            &id_arg,
            &properties,
            covering.as_ref(),
            srid,
        );
        conn.prepare(&sql).map_err(MaptileError::backend)?;
        let mut connections = Vec::with_capacity(cfg.query_options.pool_size);
        for _ in 1..cfg.query_options.pool_size {
            connections.push(conn.try_clone().map_err(MaptileError::backend)?);
        }
        connections.push(conn);
        check_stamp(&local)?;
        Ok(Self {
            pool: Arc::new(QueryPool {
                state: Mutex::new(PoolState {
                    connections,
                    running: Vec::new(),
                    active: 0,
                    closed: false,
                }),
                slots: Arc::new(Semaphore::new(cfg.query_options.pool_size)),
                idle: Notify::new(),
                remote,
            }),
            sql: sql.into(),
            local,
            options: cfg.query_options.clone(),
            bounds,
            fields,
        })
    }

    pub async fn tile(
        &self,
        xyz: TileCoord,
        file_lease: Option<Arc<dyn Send + Sync>>,
    ) -> MaptileResult<bytes::Bytes> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.options.timeout_ms);
        let permit = tokio::time::timeout_at(deadline, self.pool.slots.clone().acquire_owned())
            .await
            .map_err(|_| timeout())?
            .map_err(|_| config("GeoParquet query pool closed"))?;
        let notify = Arc::new(Notify::new());
        let (lease, running) = {
            let mut state = self
                .pool
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return Err(config("source is closed"));
            }
            let conn = state
                .connections
                .pop()
                .ok_or_else(|| config("GeoParquet query pool invariant"))?;
            let running = Arc::new(Mutex::new(QueryState {
                interrupt: conn.interrupt_handle(),
                cancelled: false,
                done: false,
                wake: notify.clone(),
            }));
            state.running.retain(|s| s.strong_count() > 0);
            state.running.push(Arc::downgrade(&running));
            state.active += 1;
            (
                ConnectionLease {
                    conn: Some(conn),
                    pool: self.pool.clone(),
                    _permit: permit,
                },
                running,
            )
        };
        let cancellation = CancelQuery {
            state: running.clone(),
            notify: notify.clone(),
        };
        let watchdog_state = running.clone();
        tokio::spawn(async move {
            tokio::select! { () = tokio::time::sleep_until(deadline) => {}, () = notify.notified() => {} }
            loop {
                {
                    let mut state = watchdog_state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.done {
                        return;
                    }
                    state.cancelled = true;
                    state.interrupt.interrupt();
                }
                // Repeated interruption also covers cancellation before the
                // blocking task starts executing SQL. The permit stays leased
                // until the interrupted operation actually exits.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let sql = self.sql.clone();
        let local = self.local.clone();
        let options = self.options.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _file_lease = file_lease;
            let _completion = FinishQuery(running.clone());
            // Drop order matters: finish/disarm before the connection is reused.
            let result = (|| {
                if running
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cancelled
                {
                    return Err(timeout());
                }
                check_stamp(&local)?;
                let conn = lease.conn.as_ref().expect("leased connection");
                let (data, count) = conn.prepare_cached(&sql)
                    .and_then(|mut s| s.query_row(named_params! { "z": i32::from(xyz.z), "x": i64::from(xyz.x), "y": i64::from(xyz.y) }, |r| Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, i64>(1)?))))
                    .map_err(MaptileError::backend)?;
                if count as usize > options.max_features {
                    return Err(config(
                        "GeoParquet tile exceeds max_features; no truncated tile was returned",
                    ));
                }
                let data = data.unwrap_or_default();
                if data.len() > options.max_tile_bytes {
                    return Err(MaptileError::PayloadTooLarge {
                        limit: options.max_tile_bytes,
                    });
                }
                check_stamp(&local)?;
                Ok(bytes::Bytes::from(data))
            })();
            drop(_completion);
            drop(_file_lease);
            drop(lease);
            result
        });
        let result = tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| timeout())?
            .map_err(|e| MaptileError::BackendMsg(format!("GeoParquet query task: {e}")))?;
        drop(cancellation);
        result
    }
}

impl GeometrySource {
    pub fn start_close(&self) {
        self.pool.start_close();
    }
    pub async fn close(&self) -> MaptileResult<()> {
        self.pool.start_close();
        loop {
            let idle = self.pool.idle.notified();
            if self
                .pool
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active
                == 0
            {
                break;
            }
            idle.await;
        }
        if let Some(remote) = &self.pool.remote {
            remote.close().await?;
        }
        Ok(())
    }
}
struct PoolState {
    connections: Vec<Connection>,
    running: Vec<Weak<Mutex<QueryState>>>,
    active: usize,
    closed: bool,
}
struct QueryPool {
    state: Mutex<PoolState>,
    slots: Arc<Semaphore>,
    idle: Notify,
    remote: Option<Arc<RemoteFile>>,
}
impl QueryPool {
    fn start_close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        self.slots.close();
        for running in state.running.iter().filter_map(Weak::upgrade) {
            let mut running = running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !running.done {
                running.cancelled = true;
                running.interrupt.interrupt();
                running.wake.notify_one();
            }
        }
        state.connections.clear();
        if let Some(remote) = &self.remote {
            remote.start_close();
        }
    }
}
struct ConnectionLease {
    conn: Option<Connection>,
    pool: Arc<QueryPool>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for ConnectionLease {
    fn drop(&mut self) {
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(conn) = self.conn.take()
            && !state.closed
        {
            state.connections.push(conn);
        }
        // Otherwise the connection is destroyed before active reaches zero.
        state.active -= 1;
        drop(state);
        self.pool.idle.notify_waiters();
    }
}

struct QueryState {
    interrupt: Arc<InterruptHandle>,
    cancelled: bool,
    done: bool,
    wake: Arc<Notify>,
}
struct FinishQuery(Arc<Mutex<QueryState>>);
impl Drop for FinishQuery {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .done = true;
    }
}
struct CancelQuery {
    state: Arc<Mutex<QueryState>>,
    notify: Arc<Notify>,
}
impl Drop for CancelQuery {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.done {
            state.cancelled = true;
            state.interrupt.interrupt();
        }
        self.notify.notify_one();
    }
}

#[derive(Clone, PartialEq)]
struct FileStamp {
    len: u64,
    modified: SystemTime,
}
impl FileStamp {
    fn read(path: &Path) -> MaptileResult<Self> {
        let meta = std::fs::metadata(path).map_err(MaptileError::backend)?;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified().map_err(MaptileError::backend)?,
        })
    }
}

fn check_stamp(local: &Option<(PathBuf, FileStamp)>) -> MaptileResult<()> {
    if let Some((path, stamp)) = local
        && FileStamp::read(path)? != *stamp
    {
        return Err(config("GeoParquet file changed; reopen the source"));
    }
    Ok(())
}

fn single_file_path(path: &Path) -> MaptileResult<PathBuf> {
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
    let path = path.canonicalize().map_err(MaptileError::backend)?;
    if !path.is_file() {
        return Err(config("GeoParquet input must be a regular file"));
    }
    Ok(path)
}

fn resolve_srid(explicit: Option<i32>, column: Option<&Value>) -> MaptileResult<i32> {
    if let Some(srid) = explicit {
        return if srid > 0 {
            Ok(srid)
        } else {
            Err(config("srid must be positive"))
        };
    }
    let Some(crs) = column.and_then(|c| c.get("crs")) else {
        // GeoParquet's absent CRS is OGC:CRS84. Legacy explicitly selected
        // raw-WKB Parquet keeps the original WGS84 default as well.
        return Ok(4326);
    };
    if crs.is_null() {
        return Err(config("GeoParquet CRS is explicitly unknown; set srid"));
    }
    let id = crs
        .get("id")
        .ok_or_else(|| config("CRS has no authority; set srid"))?;
    let authority = id
        .get("authority")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = id
        .get("code")
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    if authority.eq_ignore_ascii_case("OGC") && code.eq_ignore_ascii_case("CRS84") {
        return Ok(4326);
    }
    if authority.eq_ignore_ascii_case("EPSG")
        && let Ok(srid) = code.parse::<i32>()
        && srid > 0
    {
        return Ok(srid);
    }
    Err(config(
        "unsupported GeoParquet CRS authority; set an EPSG srid",
    ))
}

fn metadata_bounds(column: Option<&Value>, srid: i32) -> Option<[f64; 4]> {
    let bbox = column?.get("bbox")?.as_array()?;
    let indices = match bbox.len() {
        4 => [0, 1, 2, 3],
        6 => [0, 1, 3, 4],
        _ => return None,
    };
    let mut bounds = [0.0; 4];
    for (out, i) in bounds.iter_mut().zip(indices) {
        *out = bbox[i].as_f64()?;
    }
    if srid == 3857 {
        bounds = [
            bounds[0] / RADIUS * 180.0 / std::f64::consts::PI,
            inverse_y(bounds[1]),
            bounds[2] / RADIUS * 180.0 / std::f64::consts::PI,
            inverse_y(bounds[3]),
        ];
    } else if srid != 4326 {
        return None;
    }
    valid_bounds(bounds).then_some(bounds)
}

pub(super) fn valid_bounds(b: [f64; 4]) -> bool {
    b.iter().all(|n| n.is_finite())
        && b[0] >= -180.0
        && b[2] <= 180.0
        && b[1] >= -90.0
        && b[3] <= 90.0
        && b[0] <= b[2]
        && b[1] <= b[3]
}

fn covering_paths(column: &Value, conn: &Connection, from: &str) -> Option<[String; 4]> {
    let bbox = column.get("covering")?.get("bbox")?;
    let mut paths = Vec::new();
    for axis in ["xmin", "ymin", "xmax", "ymax"] {
        let parts = bbox.get(axis)?.as_array()?;
        if parts.is_empty() || parts.len() > 16 {
            return None;
        }
        let path = parts
            .iter()
            .map(|part| identifier(part.as_str()?).ok())
            .collect::<Option<Vec<_>>>()?
            .join(".");
        let ty: String = conn
            .query_row(&format!("DESCRIBE SELECT {path} FROM {from}"), [], |row| {
                row.get(1)
            })
            .ok()?;
        if !matches!(property_type(&ty), Some((_, "Number"))) {
            return None;
        }
        paths.push(path);
    }
    paths.try_into().ok()
}

const RADIUS: f64 = 6_378_137.0;
fn inverse_y(y: f64) -> f64 {
    (y / RADIUS).sinh().atan().to_degrees()
}

// Keep SQL inputs borrowed and explicit rather than adding a builder state.
#[allow(clippy::too_many_arguments)]
fn build_sql(
    cfg: &GeoParquetSourceConfig,
    from: &str,
    geom: &str,
    geom_alias: &str,
    id_arg: &str,
    properties: &[String],
    covering: Option<&[String; 4]>,
    srid: i32,
) -> String {
    // Keep all tile expressions constant at plan time. A CROSS JOIN to a
    // one-row bounds CTE can disable Parquet predicate pushdown.
    let tile = "ST_TileEnvelope($z::INTEGER, $x::INTEGER, $y::INTEGER)";
    let buffered = format!(
        "ST_Expand({tile}, {}::DOUBLE * {}::DOUBLE / power(2, $z::INTEGER))",
        f64::from(cfg.buffer) / f64::from(cfg.extent),
        2.0 * std::f64::consts::PI * RADIUS
    );
    let mut filters = Vec::new();
    if let Some(paths) = covering {
        let bounds = if srid == 3857 {
            [
                format!("ST_XMin({buffered})"),
                format!("ST_YMin({buffered})"),
                format!("ST_XMax({buffered})"),
                format!("ST_YMax({buffered})"),
            ]
        } else {
            // Do not transform the polygon's corners through PROJ here: its
            // antimeridian wrapping can invert an edge tile's buffered bbox.
            [
                format!("greatest(-180.0, ST_XMin({buffered}) / {RADIUS} * 180 / pi())"),
                format!("degrees(atan(sinh(ST_YMin({buffered}) / {RADIUS})))"),
                format!("least(180.0, ST_XMax({buffered}) / {RADIUS} * 180 / pi())"),
                format!("degrees(atan(sinh(ST_YMax({buffered}) / {RADIUS})))"),
            ]
        };
        for (i, op, j) in [(0, "<=", 2), (1, "<=", 3), (2, ">=", 0), (3, ">=", 1)] {
            filters.push(format!("{} {op} {}", paths[i], bounds[j]));
        }
    }
    filters.push(format!("ST_Intersects({geom}, {buffered})"));
    let properties = if properties.is_empty() {
        String::new()
    } else {
        format!(", {}", properties.join(", "))
    };
    format!(
        "SELECT ST_AsMVT(tile, {}, {}, '{}'{id_arg}), count(*) FROM (SELECT ST_AsMVTGeom({geom}, ST_Extent({tile}), {}::BIGINT, {}::BIGINT, true) AS {geom_alias}{properties} FROM {from} WHERE {} LIMIT {}) AS tile",
        literal(&cfg.layer_name),
        cfg.extent,
        geom_alias,
        cfg.extent,
        cfg.buffer,
        filters.join(" AND "),
        cfg.query_options.max_features + 1
    )
}

fn property_type(ty: &str) -> Option<(&'static str, &'static str)> {
    match ty {
        "BOOLEAN" => Some(("BOOLEAN", "Boolean")),
        "TINYINT" | "SMALLINT" | "INTEGER" | "UTINYINT" | "USMALLINT" => {
            Some(("INTEGER", "Number"))
        }
        "BIGINT" | "UINTEGER" => Some(("BIGINT", "Number")),
        "FLOAT" | "DOUBLE" => Some(("DOUBLE", "Number")),
        "VARCHAR"
        | "UBIGINT"
        | "HUGEINT"
        | "UHUGEINT"
        | "DATE"
        | "TIME"
        | "TIME WITH TIME ZONE"
        | "TIMESTAMP"
        | "TIMESTAMP WITH TIME ZONE"
        | "TIMESTAMP_S"
        | "TIMESTAMP_MS"
        | "TIMESTAMP_NS"
        | "INTERVAL"
        | "UUID"
        | "JSON" => Some(("VARCHAR", "String")),
        t if t.starts_with("DECIMAL(") => Some(("DOUBLE", "Number")),
        t if t.starts_with("ENUM(") => Some(("VARCHAR", "String")),
        _ => None,
    }
}
fn unused_name(prefix: &str, columns: &BTreeMap<String, String>) -> String {
    let mut name = prefix.to_owned();
    while columns.keys().any(|c| c.eq_ignore_ascii_case(&name)) {
        name.push('_');
    }
    name
}
fn identifier(name: &str) -> MaptileResult<String> {
    if name.is_empty() || name.contains('\0') {
        return Err(config("column names must be nonempty and contain no NUL"));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn config(message: impl Into<String>) -> MaptileError {
    MaptileError::Config(format!("geoparquet: {}", message.into()))
}
fn timeout() -> MaptileError {
    MaptileError::BackendMsg("GeoParquet query exceeded its deadline or was cancelled".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::mvt;
    use crate::sources::{GeoParquetSource, TileSource};
    use arrow_array::{
        ArrayRef, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray, StructArray,
        UInt64Array,
    };
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;

    const TILE: TileCoord = TileCoord { z: 2, x: 2, y: 1 };

    #[tokio::test]
    async fn genuine_geoparquet_projects_filters_and_preserves_properties_and_ids() {
        let fixture = fixture(
            vec![point(10.0, 60.0), point(-100.0, -50.0), None],
            None,
            true,
        );
        let source = GeoParquetSource::open(fixture.config()).unwrap();
        let bytes = source.get_tile(TILE, None).await.unwrap().data;
        let tile = mvt::decode(&bytes).unwrap();
        let layer = &tile.layers[0];
        assert_eq!(layer.name, "places");
        assert_eq!(layer.extent, Some(4096));
        assert_eq!(layer.features.len(), 1);
        let feature = &layer.features[0];
        assert_eq!(feature.id, Some(1));
        assert_eq!(feature.geometry[0], 9);
        let (x, y) = (unzig(feature.geometry[1]), unzig(feature.geometry[2]));
        assert!((x - 455).abs() <= 1);
        assert!(
            (y - 662).abs() <= 1,
            "latitude 60 must be projected, not linearly scaled: {y}"
        );
        let props = properties(layer, feature);
        assert_eq!(props["name"].string_value.as_deref(), Some("row-0"));
        assert_eq!(
            props["unsigned"].string_value.as_deref(),
            Some("18446744073709551615")
        );
        assert_eq!(
            source.tilejson().vector_layers.as_ref().unwrap()[0].fields["unsigned"],
            "String"
        );
        assert!(!layer.keys.iter().any(|k| k == "bbox" || k == "id"));
    }

    #[tokio::test]
    async fn mercator_source_matches_wgs84_and_unknown_crs_requires_override() {
        let projected = fixture(
            vec![point(
                10f64.to_radians() * RADIUS,
                (std::f64::consts::FRAC_PI_4 + 60f64.to_radians() / 2.0)
                    .tan()
                    .ln()
                    * RADIUS,
            )],
            Some(Value::Null),
            true,
        );
        assert!(GeoParquetSource::open(projected.config()).is_err());
        let mut cfg = projected.config();
        cfg.srid = Some(3857);
        let source = GeoParquetSource::open(cfg).unwrap();
        let tile = mvt::decode(&source.get_tile(TILE, None).await.unwrap().data).unwrap();
        let feature = &tile.layers[0].features[0];
        assert!((unzig(feature.geometry[1]) - 455).abs() <= 1);
        assert!((unzig(feature.geometry[2]) - 662).abs() <= 1);
        assert_eq!(
            resolve_srid(
                None,
                Some(&serde_json::json!({"crs":{"id":{"authority":"EPSG","code":3857}}}))
            )
            .unwrap(),
            3857
        );
        assert!(resolve_srid(Some(-1), None).is_err());
        assert!(
            resolve_srid(
                None,
                Some(&serde_json::json!({"crs":{"id":{"authority":"UNKNOWN","code":42}}}))
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn buffer_edge_world_edge_and_crossing_shapes_are_not_pruned() {
        let fixture = fixture(
            vec![
                point(-0.5, 60.0),
                line(&[(-10.0, 55.0), (40.0, 65.0)]),
                polygon(),
            ],
            None,
            true,
        );
        let source = GeoParquetSource::open(fixture.config()).unwrap();
        let tile = mvt::decode(&source.get_tile(TILE, None).await.unwrap().data).unwrap();
        let layer = &tile.layers[0];
        assert_eq!(
            layer.features.len(),
            3,
            "overlap, not containment, must drive bbox pruning"
        );
        assert!(
            unzig(layer.features[0].geometry[1]) < 0,
            "buffer point retained outside nominal tile"
        );
        let polygon = layer.features.iter().find(|f| f.r#type == Some(3)).unwrap();
        assert_eq!(
            polygon.geometry.iter().filter(|v| **v == 15).count(),
            2,
            "outer ring and hole"
        );
        for feature in &layer.features {
            for (x, y) in coordinates(&feature.geometry) {
                assert!((-64..=4160).contains(&x));
                assert!((-64..=4160).contains(&y));
            }
        }
        let edge = fixture_points(&[(-179.5, 60.0), (179.5, 60.0)], true);
        let source = GeoParquetSource::open(edge.config()).unwrap();
        for x in [0, 3] {
            let tile = mvt::decode(
                &source
                    .get_tile(TileCoord { z: 2, x, y: 1 }, None)
                    .await
                    .unwrap()
                    .data,
            )
            .unwrap();
            assert_eq!(
                tile.layers[0].features.len(),
                1,
                "world edge buffer must not invert the envelope"
            );
        }
    }

    #[tokio::test]
    async fn empty_files_null_geometry_and_invalid_coordinates_are_explicit() {
        for data in [vec![], vec![None]] {
            let fixture = fixture(data, None, true);
            let src = GeoParquetSource::open(fixture.config()).unwrap();
            let bytes = src.get_tile(TILE, None).await.unwrap().data;
            assert!(
                bytes.is_empty()
                    || mvt::decode(&bytes)
                        .unwrap()
                        .layers
                        .iter()
                        .all(|l| l.features.is_empty())
            );
            for xyz in [
                TileCoord { z: 31, x: 0, y: 0 },
                TileCoord { z: 2, x: 4, y: 0 },
                TileCoord { z: 2, x: 0, y: 4 },
            ] {
                assert!(matches!(
                    src.get_tile(xyz, None).await,
                    Err(MaptileError::TileNotFound(..))
                ));
            }
        }
    }

    #[tokio::test]
    async fn identifiers_paths_properties_and_limits_are_safe() {
        let f = fixture_points(&[(10.0, 60.0), (20.0, 60.0)], true);
        let mut cfg = f.config();
        cfg.layer_name = "layer'); DROP TABLE anything; --".into();
        cfg.properties = Some(vec!["name".into()]);
        let src = GeoParquetSource::open(cfg).unwrap();
        let tile = mvt::decode(&src.get_tile(TILE, None).await.unwrap().data).unwrap();
        assert_eq!(tile.layers[0].name, "layer'); DROP TABLE anything; --");
        assert_eq!(tile.layers[0].keys, vec!["name"]);
        let mut cfg = f.config();
        cfg.query_options.max_features = 1;
        assert!(
            GeoParquetSource::open(cfg)
                .unwrap()
                .get_tile(TILE, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("max_features")
        );
        let mut cfg = f.config();
        cfg.query_options.max_tile_bytes = 1;
        assert!(
            GeoParquetSource::open(cfg)
                .unwrap()
                .get_tile(TILE, None)
                .await
                .is_err()
        );
        for bad in [
            "/tmp/*.parquet",
            "/tmp/x?.parquet",
            "https://example.com/data.parquet",
        ] {
            assert!(single_file_path(Path::new(bad)).is_err());
        }
        assert_eq!(identifier("a\"; --").unwrap(), "\"a\"\"; --\"");
        for mutate in [0, 1, 2, 3] {
            let mut cfg = f.config();
            match mutate {
                0 => cfg.srid = Some(999_999),
                1 => cfg.id_column = Some("name".into()),
                2 => cfg.properties = Some(vec!["not there".into()]),
                _ => cfg.geometry_column = Some("wrong\"; SELECT".into()),
            };
            assert!(GeoParquetSource::open(cfg).is_err());
        }
    }

    #[tokio::test]
    async fn pruning_reaches_parquet_statistics_and_does_not_change_tiles() {
        let f = fixture_points(&[(10.0, 60.0), (20.0, 60.0)], true);
        let with = GeometrySource::open(&f.config()).unwrap();
        let without_fixture = fixture_points(&[(10.0, 60.0), (20.0, 60.0)], false);
        let without = GeometrySource::open(&without_fixture.config()).unwrap();
        let tile = with.tile(TILE, None).await.unwrap();
        assert_eq!(tile, without.tile(TILE, None).await.unwrap());
        let explain = |source: &GeometrySource| {
            let state = source.pool.state.lock().unwrap();
            let connections = &state.connections;
            connections[0]
                .prepare(&format!("EXPLAIN {}", source.sql))
                .unwrap()
                .query_row(named_params! {"z":2,"x":0,"y":3}, |r| r.get::<_, String>(1))
                .unwrap()
        };
        let optimized = explain(&with);
        let control = explain(&without);
        assert!(
            optimized.contains("EMPTY_RESULT"),
            "no statistics pruning: {optimized}"
        );
        assert!(
            control.contains("READ_PARQUET"),
            "control must scan: {control}"
        );
    }

    #[tokio::test]
    async fn changed_file_is_not_served_with_stale_metadata() {
        let f = fixture_points(&[(10.0, 60.0)], true);
        let src = GeoParquetSource::open(f.config()).unwrap();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.path)
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        assert!(
            src.get_tile(TILE, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeouts_and_cancellation_keep_pool_bounded_and_reusable() {
        let f = fixture_points(&[(10.0, 60.0)], true);
        let mut cfg = f.config();
        cfg.query_options.pool_size = 1;
        cfg.query_options.timeout_ms = 50;
        let mut source = GeometrySource::open(&cfg).unwrap();
        let normal = source.sql.clone();
        source.sql="SELECT NULL::BLOB, sum(i)::BIGINT FROM range(1000000000) t(i) WHERE i % ($z::INTEGER + 2) = $x::INTEGER AND i > $y::INTEGER".into();
        let source = Arc::new(source);
        assert!(source.tile(TILE, None).await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if source.pool.slots.available_permits() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let cloned = source.clone();
        let task = tokio::spawn(async move { cloned.tile(TILE, None).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if source.pool.slots.available_permits() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut source = Arc::try_unwrap(source).ok().unwrap();
        source.sql = normal;
        source.options.timeout_ms = 1_000;
        for _ in 0..5 {
            assert!(!source.tile(TILE, None).await.unwrap().is_empty());
        }
        let permit = source.pool.slots.clone().acquire_owned().await.unwrap();
        source.options.timeout_ms = 20;
        assert!(source.tile(TILE, None).await.is_err());
        assert_eq!(source.pool.slots.available_permits(), 0);
        drop(permit);
        assert_eq!(source.pool.slots.available_permits(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clones_share_pool_and_file_lease_until_last_reader() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Lease(Arc<AtomicUsize>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let f = fixture_points(&[(10.0, 60.0)], true);
        let dropped = Arc::new(AtomicUsize::new(0));
        let src = GeoParquetSource::open(f.config())
            .unwrap()
            .with_file_lease(Lease(dropped.clone()));
        let cloned = src.clone_source();
        drop(src);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert!(!cloned.get_tile(TILE, None).await.unwrap().data.is_empty());
        drop(cloned);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn query_connections_cannot_read_another_file() {
        let allowed = fixture_points(&[(10.0, 60.0)], true);
        let other = fixture_points(&[(20.0, 60.0)], true);
        let source = GeometrySource::open(&allowed.config()).unwrap();
        let state = source.pool.state.lock().unwrap();
        let connections = &state.connections;
        for conn in connections.iter() {
            let err = conn
                .query_row(
                    &format!(
                        "SELECT count(*) FROM read_parquet({})",
                        literal(other.path.to_str().unwrap())
                    ),
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("Permission") || message.contains("disabled"),
                "unexpected failure: {message}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_query_retains_then_releases_its_file_lease() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Lease(Arc<AtomicUsize>);
        impl Drop for Lease {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let fixture = fixture_points(&[(10.0, 60.0)], true);
        let mut source = GeometrySource::open(&fixture.config()).unwrap();
        source.sql = "SELECT NULL::BLOB, sum(i)::BIGINT FROM range(1000000000) t(i) WHERE i % ($z::INTEGER + 2) = $x::INTEGER AND i > $y::INTEGER".into();
        let pool = source.pool.clone();
        let dropped = Arc::new(AtomicUsize::new(0));
        let lease = Arc::new(Lease(dropped.clone()));
        let task = tokio::spawn(async move { source.tile(TILE, Some(lease)).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.slots.available_permits() == 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while dropped.load(Ordering::SeqCst) == 0 || pool.slots.available_permits() != 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[ignore = "explicit small performance probe, not a production benchmark"]
    async fn repeated_tile_probe() {
        let f = fixture_points(
            &(0..100_000)
                .map(|i| (10.0 + f64::from(i % 100) / 100.0, 60.0))
                .collect::<Vec<_>>(),
            true,
        );
        let start = std::time::Instant::now();
        let source = GeoParquetSource::open(f.config()).unwrap();
        let opening = start.elapsed();
        let start = std::time::Instant::now();
        for _ in 0..20 {
            source
                .get_tile(
                    TileCoord {
                        z: 12,
                        x: 2161,
                        y: 1189,
                    },
                    None,
                )
                .await
                .unwrap();
        }
        eprintln!(
            "100k features, open={opening:?}, 20 uncached tiles={:?}",
            start.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_range_open_tile_and_close_do_not_materialize_the_object() {
        let points: Vec<_> = (0..100_000)
            .map(|i| {
                if i == 0 {
                    (10.0, 60.0)
                } else {
                    (-100.0, -50.0)
                }
            })
            .collect();
        let fixture = fixture_points(&points, true);
        let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let remote = Arc::new(
            RemoteFile::http(
                &format!("{}?signature=opaque", http.url),
                client,
                Duration::from_secs(3),
            )
            .await
            .unwrap(),
        );
        let local_url = remote.url.clone();
        let cfg = fixture.config();
        let source = tokio::task::spawn_blocking(move || GeometrySource::open_remote(&cfg, remote))
            .await
            .unwrap()
            .unwrap();
        let opening = http
            .stats
            .transferred
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            opening < http.stats.bytes.len() as u64,
            "opening transferred {opening} of {} bytes",
            http.stats.bytes.len()
        );
        let data = source.tile(TILE, None).await.unwrap();
        let tile = mvt::decode(&data).unwrap();
        assert_eq!(tile.layers[0].features.len(), 1);
        assert_eq!(tile.layers[0].features[0].id, Some(1));
        assert!(
            http.stats
                .ranges
                .lock()
                .unwrap()
                .iter()
                .all(|r| r.end - r.start < http.stats.bytes.len() as u64)
        );
        let transfer = http
            .stats
            .transferred
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            transfer < http.stats.bytes.len() as u64,
            "open and tile transferred {transfer} bytes, total {}",
            http.stats.bytes.len()
        );
        eprintln!(
            "HTTP Range bytes: open={opening}, open+tile={transfer}, object={}",
            http.stats.bytes.len()
        );
        source.close().await.unwrap();
        source.close().await.unwrap();
        assert!(source.pool.state.lock().unwrap().connections.is_empty());
        assert!(source.tile(TILE, None).await.is_err());
        let url = url::Url::parse(&local_url).unwrap();
        assert!(
            tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap()))
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn object_reader_is_released_on_explicit_close_even_while_source_clones_exist() {
        use object_store::{ObjectStoreExt, memory::InMemory, path::Path as ObjectPath};
        let fixture = fixture_points(&[(10.0, 60.0)], true);
        let store = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from("data.parquet"),
                std::fs::read(&fixture.path).unwrap().into(),
            )
            .await
            .unwrap();
        let weak = Arc::downgrade(&store);
        let source =
            GeoParquetSource::open_object_store(fixture.config(), store.clone(), "data.parquet")
                .await
                .unwrap();
        let clone = source.clone();
        drop(store);
        assert!(weak.upgrade().is_some());
        assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
        source.close().await.unwrap();
        assert!(
            weak.upgrade().is_none(),
            "the source must release its object-store client during close"
        );
        assert!(clone.get_tile(TILE, None).await.is_err());
        clone.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_refuses_redirects_ignored_ranges_and_changed_objects() {
        use std::sync::atomic::Ordering;
        let fixture = fixture_points(&[(10.0, 60.0)], true);
        let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
        http.stats.mode.store(1, Ordering::SeqCst);
        assert!(
            GeoParquetSource::open_http(fixture.config(), &http.url)
                .await
                .is_err()
        );
        assert_eq!(
            http.stats.requests.load(Ordering::SeqCst),
            1,
            "must not follow redirect"
        );
        http.stats.mode.store(2, Ordering::SeqCst);
        assert!(
            GeoParquetSource::open_http(fixture.config(), &http.url)
                .await
                .is_err(),
            "200 must not become a whole-file fallback"
        );
        http.stats.mode.store(0, Ordering::SeqCst);
        let source = GeoParquetSource::open_http(fixture.config(), &http.url)
            .await
            .unwrap();
        http.stats.mode.store(3, Ordering::SeqCst);
        assert!(
            source.get_tile(TILE, None).await.is_err(),
            "ETag changes must not mix file versions"
        );
        source.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_get_only_urls_open_without_head_support() {
        let fixture = fixture_points(&[(10.0, 60.0)], true);
        let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
        http.stats
            .mode
            .store(4, std::sync::atomic::Ordering::SeqCst);
        let source = GeoParquetSource::open_http(
            fixture.config(),
            &format!("{}?signature=get-only", http.url),
        )
        .await
        .unwrap();
        assert!(!source.get_tile(TILE, None).await.unwrap().data.is_empty());
        source.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_interrupts_active_queries_and_rejects_waiters() {
        let fixture = fixture_points(&[(10.0, 60.0)], true);
        let mut source = GeometrySource::open(&fixture.config()).unwrap();
        source.sql = "SELECT NULL::BLOB, sum(i)::BIGINT FROM range(1000000000) t(i) WHERE i % ($z::INTEGER + 2) = $x::INTEGER AND i > $y::INTEGER".into();
        let source = Arc::new(source);
        let query_source = source.clone();
        let query = tokio::spawn(async move { query_source.tile(TILE, None).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while source.pool.state.lock().unwrap().active == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), source.close())
            .await
            .unwrap()
            .unwrap();
        assert!(query.await.unwrap().is_err());
        assert!(source.pool.state.lock().unwrap().connections.is_empty());
        assert_eq!(source.pool.state.lock().unwrap().active, 0);
        assert!(source.tile(TILE, None).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_close_waits_for_active_ranges_and_can_be_cancelled_and_retried() {
        use std::sync::atomic::Ordering;
        let fixture = fixture_points(&[(10.0, 60.0)], true);
        let http = TestHttp::start(std::fs::read(&fixture.path).unwrap()).await;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let remote = RemoteFile::http(&http.url, client, Duration::from_millis(150))
            .await
            .unwrap();
        http.stats.ranges.lock().unwrap().clear();
        http.stats.mode.store(5, Ordering::SeqCst);
        let local_url = remote.url.clone();
        let request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(local_url)
                .header("Range", "bytes=0-7")
                .send()
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while http.stats.ranges.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), remote.close())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(1), remote.close())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request.await.unwrap().unwrap().status(),
            reqwest::StatusCode::BAD_GATEWAY
        );
        let url = url::Url::parse(&remote.url).unwrap();
        assert!(
            tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap()))
                .await
                .is_err()
        );
        remote.close().await.unwrap();
    }

    struct HttpStats {
        bytes: bytes::Bytes,
        ranges: Mutex<Vec<std::ops::Range<u64>>>,
        transferred: std::sync::atomic::AtomicU64,
        requests: std::sync::atomic::AtomicUsize,
        mode: std::sync::atomic::AtomicU8,
    }
    struct TestHttp {
        url: String,
        stats: Arc<HttpStats>,
        task: tokio::task::JoinHandle<()>,
    }
    impl TestHttp {
        async fn start(bytes: Vec<u8>) -> Self {
            let stats = Arc::new(HttpStats {
                bytes: bytes.into(),
                ranges: Mutex::new(Vec::new()),
                transferred: 0.into(),
                requests: 0.into(),
                mode: 0.into(),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/data.parquet", listener.local_addr().unwrap());
            let app = axum::Router::new()
                .route("/data.parquet", axum::routing::any(test_http_request))
                .with_state(stats.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self { url, stats, task }
        }
    }
    impl Drop for TestHttp {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn test_http_request(
        axum::extract::State(stats): axum::extract::State<Arc<HttpStats>>,
        method: axum::http::Method,
        headers: axum::http::HeaderMap,
    ) -> axum::http::Response<axum::body::Body> {
        use axum::body::Body;
        use axum::http::{Response, StatusCode, header};
        use std::sync::atomic::Ordering;
        stats.requests.fetch_add(1, Ordering::SeqCst);
        let mode = stats.mode.load(Ordering::SeqCst);
        if mode == 1 {
            return Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, "http://127.0.0.1:1/forbidden")
                .body(Body::empty())
                .unwrap();
        }
        if mode == 4 && method == axum::http::Method::HEAD {
            return Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(Body::empty())
                .unwrap();
        }
        let response =
            Response::builder().header(header::ETAG, if mode == 3 { "\"v2\"" } else { "\"v1\"" });
        if method == axum::http::Method::HEAD {
            return response
                .header(header::CONTENT_LENGTH, stats.bytes.len())
                .body(Body::empty())
                .unwrap();
        }
        let range = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| super::super::geoparquet_remote::parse_range(v, stats.bytes.len() as u64))
            .expect("every data GET must carry a valid Range");
        stats.ranges.lock().unwrap().push(range.clone());
        if mode == 5 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if mode == 2 {
            return response.body(Body::from(stats.bytes.clone())).unwrap();
        }
        stats
            .transferred
            .fetch_add(range.end - range.start, Ordering::SeqCst);
        response
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                header::CONTENT_RANGE,
                format!(
                    "bytes {}-{}/{}",
                    range.start,
                    range.end - 1,
                    stats.bytes.len()
                ),
            )
            .header(header::CONTENT_LENGTH, range.end - range.start)
            .body(Body::from(
                stats.bytes.slice(range.start as usize..range.end as usize),
            ))
            .unwrap()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }
    impl Fixture {
        fn config(&self) -> GeoParquetSourceConfig {
            GeoParquetSourceConfig {
                path: self.path.clone(),
                geometry_column: Some("geometry".into()),
                id_column: Some("id".into()),
                layer_name: "places".into(),
                ..Default::default()
            }
        }
    }
    fn fixture_points(points: &[(f64, f64)], covering: bool) -> Fixture {
        fixture(
            points.iter().map(|(x, y)| point(*x, *y)).collect(),
            None,
            covering,
        )
    }
    fn fixture(rows: Vec<Option<Vec<u8>>>, crs: Option<Value>, covering: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("geoparquet's file.parquet");
        let bboxes: Vec<_> = rows
            .iter()
            .map(|w| w.as_ref().and_then(|w| crate::wkb_mvt::wkb_bbox(w)))
            .collect();
        let bbox_fields: Vec<_> = ["xmin", "ymin", "xmax", "ymax"]
            .into_iter()
            .map(|n| Field::new(n, DataType::Float64, true))
            .collect();
        let bbox = StructArray::from(
            bbox_fields
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let values: Vec<Option<f64>> = bboxes
                        .iter()
                        .map(|b| b.map(|b| [b.0, b.1, b.2, b.3][i]))
                        .collect();
                    (
                        Arc::new(f.clone()),
                        Arc::new(Float64Array::from(values)) as ArrayRef,
                    )
                })
                .collect::<Vec<_>>(),
        );
        let schema = Arc::new(Schema::new(vec![
            Field::new("geometry", DataType::Binary, true),
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("unsigned", DataType::UInt64, false),
            Field::new("bbox", DataType::Struct(bbox_fields.into()), false),
        ]));
        let count = rows.len();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(BinaryArray::from_iter(rows.iter().map(|r| r.as_deref()))),
                Arc::new(Int64Array::from_iter_values(1..=count as i64)),
                Arc::new(StringArray::from_iter_values(
                    (0..count).map(|i| format!("row-{i}")),
                )),
                Arc::new(UInt64Array::from(vec![u64::MAX; count])),
                Arc::new(bbox),
            ],
        )
        .unwrap();
        let mut column =
            serde_json::json!({"encoding":"WKB","geometry_types":["Point","LineString","Polygon"]});
        if let Some(crs) = crs {
            column["crs"] = crs;
        }
        if covering {
            column["covering"] = serde_json::json!({"bbox":{"xmin":["bbox","xmin"],"ymin":["bbox","ymin"],"xmax":["bbox","xmax"],"ymax":["bbox","ymax"]}});
        }
        let geo=serde_json::json!({"version":"1.1.0","primary_column":"geometry","columns":{"geometry":column}}).to_string();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(2048))
            .set_key_value_metadata(Some(vec![KeyValue::new("geo".into(), geo)]))
            .build();
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, Some(props))
                .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        Fixture { _dir: dir, path }
    }
    fn point(x: f64, y: f64) -> Option<Vec<u8>> {
        let mut w = vec![1];
        w.extend_from_slice(&1u32.to_le_bytes());
        w.extend_from_slice(&x.to_le_bytes());
        w.extend_from_slice(&y.to_le_bytes());
        Some(w)
    }
    fn line(points: &[(f64, f64)]) -> Option<Vec<u8>> {
        let mut w = vec![1];
        w.extend_from_slice(&2u32.to_le_bytes());
        w.extend_from_slice(&(points.len() as u32).to_le_bytes());
        for (x, y) in points {
            w.extend_from_slice(&x.to_le_bytes());
            w.extend_from_slice(&y.to_le_bytes());
        }
        Some(w)
    }
    fn polygon() -> Option<Vec<u8>> {
        let mut w = vec![1];
        w.extend_from_slice(&3u32.to_le_bytes());
        w.extend_from_slice(&2u32.to_le_bytes());
        for ring in [
            [
                (-10f64, 50f64),
                (40., 50.),
                (40., 70.),
                (-10., 70.),
                (-10., 50.),
            ],
            [(10., 55.), (10., 60.), (20., 60.), (20., 55.), (10., 55.)],
        ] {
            w.extend_from_slice(&5u32.to_le_bytes());
            for (x, y) in ring {
                w.extend_from_slice(&x.to_le_bytes());
                w.extend_from_slice(&y.to_le_bytes());
            }
        }
        Some(w)
    }
    fn unzig(v: u32) -> i32 {
        ((v >> 1) as i32) ^ (-((v & 1) as i32))
    }
    fn coordinates(commands: &[u32]) -> Vec<(i32, i32)> {
        let (mut i, mut x, mut y) = (0, 0, 0);
        let mut out = Vec::new();
        while i < commands.len() {
            let cmd = commands[i] & 7;
            let count = commands[i] >> 3;
            i += 1;
            if cmd == 7 {
                continue;
            }
            assert!(cmd == 1 || cmd == 2);
            for _ in 0..count {
                x += unzig(commands[i]);
                y += unzig(commands[i + 1]);
                i += 2;
                out.push((x, y));
            }
        }
        out
    }
    fn properties<'a>(
        layer: &'a mvt::proto::Layer,
        feature: &mvt::proto::Feature,
    ) -> BTreeMap<&'a str, &'a mvt::proto::Value> {
        feature
            .tags
            .chunks(2)
            .map(|t| {
                (
                    layer.keys[t[0] as usize].as_str(),
                    &layer.values[t[1] as usize],
                )
            })
            .collect()
    }
}
