//! DuckDB-backed tile source.
//!
//! The user supplies a SQL template that returns one row with one
//! column of type `BLOB` containing pre-encoded tile bytes (typically
//! MVT, but the source is format-agnostic). The template can reference
//! `{z}`, `{x}`, `{y}` as the tile coordinates and
//! `{xmin}`, `{ymin}`, `{xmax}`, `{ymax}` as Web-Mercator metres of the
//! tile extent. Examples:
//!
//! ```sql
//! -- DuckDB Spatial extension provides ST_AsMVT.
//! SELECT ST_AsMVT(geom, 'roads', 4096, 'geom') AS tile
//! FROM ST_Read('roads.parquet')
//! WHERE ST_Intersects(
//!   geom,
//!   ST_GeomFromText(
//!     'POLYGON((${xmin} ${ymin}, ${xmax} ${ymin}, ${xmax} ${ymax}, ${xmin} ${ymax}, ${xmin} ${ymin}))'
//!   )
//! )
//! ```
//!
//! Each query leases the connection asynchronously before starting blocking work.
//! The deadline includes connection acquisition.

use super::duckdb_query::{FinishQuery, QueryControl};
use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::tile_math::tile_envelope_4326;
use crate::sources::{
    TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use bytes::Bytes;
use duckdb::types::ValueRef;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_QUERY_ROWS: usize = 100_000;
const MAX_GEOMETRY_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_TILE_BYTES: usize = crate::transport::MAX_DECODED_TILE_BYTES;

#[derive(Debug, Clone)]
pub struct DuckDbSourceConfig {
    pub id: String,
    /// File path or `":memory:"` for an in-memory database.
    pub database_path: String,
    /// Query template; see module docs for placeholder syntax.
    pub query_template: String,
    /// What [`TileFormat`] the BLOB column carries (typically Mvt).
    pub format: TileFormat,
    /// Compression of the BLOB bytes as returned by SQL. Most users will
    /// produce uncompressed MVT and let the HTTP layer gzip on the wire,
    /// so the default is `Uncompressed`.
    pub encoding: TileEncoding,
    pub minzoom: u8,
    pub maxzoom: u8,
    pub bounds: Option<[f64; 4]>,
    pub name: Option<String>,
    pub attribution: Option<String>,
    pub description: Option<String>,
    /// Live-geometry mode: when set, the SQL template is expected to
    /// return rows with `(geometry, ...properties)` columns where the
    /// geometry column is BLOB-typed WGS84 WKB. Each row is folded into
    /// a [`crate::wkb_mvt::WkbFeature`] and the tile is encoded via
    /// [`crate::wkb_mvt::encode_features_to_mvt`]. The column name here
    /// must match the SQL alias (default `"geom"`).
    pub geometry_column: Option<String>,
    /// MVT layer name to use when `geometry_column` is set. Defaults to
    /// `"data"`.
    pub layer_name: String,
    /// Per-source cache zoom policy. `None` ≡ cache every zoom.
    pub cache_zoom: Option<crate::cache::CacheZoomRange>,
}

impl Default for DuckDbSourceConfig {
    fn default() -> Self {
        Self {
            id: "duckdb".into(),
            database_path: ":memory:".into(),
            query_template: String::new(),
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
            minzoom: 0,
            maxzoom: 22,
            bounds: None,
            name: None,
            attribution: None,
            description: None,
            geometry_column: None,
            layer_name: "data".into(),
            cache_zoom: None,
        }
    }
}

#[derive(Clone)]
pub struct DuckDbSource {
    id: String,
    tj: tilejson::TileJSON,
    info: TileInfo,
    minzoom: u8,
    maxzoom: u8,
    query_template: String,
    conn: Arc<Mutex<duckdb::Connection>>,
    /// `Some(geometry_column_name)` puts the source in live-geometry mode.
    geometry_column: Option<String>,
    layer_name: String,
    cache_zoom: crate::cache::CacheZoomRange,
    content_version: ContentVersion,
}

impl std::fmt::Debug for DuckDbSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuckDbSource")
            .field("id", &self.id)
            .field("info", &self.info)
            .finish()
    }
}

impl DuckDbSource {
    /// Open the DuckDB database and prepare the source. The constructor
    /// is async because callers typically build it during startup
    /// alongside async-only sources.
    pub async fn open(cfg: DuckDbSourceConfig) -> MaptileResult<Self> {
        let path = cfg.database_path.clone();
        let conn = crate::transport::run_blocking(move || {
            let config = duckdb::Config::default()
                .max_memory("256MiB")
                .and_then(|config| config.threads(2))
                .map_err(MaptileError::backend)?;
            if path == ":memory:" {
                duckdb::Connection::open_in_memory_with_flags(config)
            } else {
                duckdb::Connection::open_with_flags(&path, config)
            }
            .map_err(MaptileError::backend)
        })
        .await?;

        let mut tj = tilejson::tilejson! { tiles: vec![] };
        tj.minzoom = Some(cfg.minzoom);
        tj.maxzoom = Some(cfg.maxzoom);
        tj.name = cfg.name.clone();
        tj.attribution = cfg.attribution.clone();
        tj.description = cfg.description.clone();
        if let Some(b) = cfg.bounds {
            tj.bounds = Some(tilejson::Bounds::new(b[0], b[1], b[2], b[3]));
        }

        // Live-geometry mode always emits MVT, regardless of cfg.format.
        let info = if cfg.geometry_column.is_some() {
            TileInfo {
                format: TileFormat::Mvt,
                encoding: TileEncoding::Uncompressed,
            }
        } else {
            TileInfo {
                format: cfg.format,
                encoding: cfg.encoding,
            }
        };

        let cache_zoom = cfg.cache_zoom.unwrap_or_default();

        Ok(Self {
            id: cfg.id,
            tj,
            info,
            minzoom: cfg.minzoom,
            maxzoom: cfg.maxzoom,
            query_template: cfg.query_template,
            conn: Arc::new(Mutex::new(conn)),
            geometry_column: cfg.geometry_column,
            layer_name: cfg.layer_name,
            cache_zoom,
            content_version: ContentVersion::new_generation(),
        })
    }

    async fn run_query(
        &self,
        query: impl FnOnce(&duckdb::Connection, &QueryControl) -> MaptileResult<Bytes> + Send + 'static,
    ) -> MaptileResult<Bytes> {
        let deadline = tokio::time::Instant::now() + QUERY_TIMEOUT;
        let conn = tokio::time::timeout_at(deadline, self.conn.clone().lock_owned())
            .await
            .map_err(|_| query_timeout())?;
        let running = QueryControl::new(&conn);
        let _cancellation = running.watch(deadline);
        let task = tokio::task::spawn_blocking(move || {
            let conn = conn;
            let _completion = FinishQuery(running.clone());
            if running.cancelled() {
                return Err(query_timeout());
            }
            query(&conn, &running)
        });
        tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| query_timeout())?
            .map_err(MaptileError::backend)?
    }
}

fn query_timeout() -> MaptileError {
    MaptileError::SourceUnavailable("DuckDB query deadline exceeded".into())
}

#[async_trait::async_trait]
impl TileSource for DuckDbSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn tilejson(&self) -> &tilejson::TileJSON {
        &self.tj
    }
    fn tile_info(&self) -> TileInfo {
        self.info
    }
    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        self.cache_zoom
    }
    fn content_version(&self) -> ContentVersion {
        self.content_version
    }
    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        if xyz.z < self.minzoom || xyz.z > self.maxzoom {
            return Err(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y));
        }

        let bbox = tile_to_web_mercator_bbox(xyz.z, xyz.x, xyz.y);
        let sql = substitute_template(&self.query_template, xyz.z, xyz.x, xyz.y, &bbox);

        if let Some(geom_col) = self.geometry_column.clone() {
            return self.live_get_tile(xyz, sql, geom_col).await;
        }

        let sql = limit_query(&sql, 1);
        let blob = self
            .run_query(move |conn, _| {
                let mut stmt = conn.prepare(&sql).map_err(MaptileError::backend)?;
                let mut rows = stmt.query([]).map_err(MaptileError::backend)?;
                let row = rows
                    .next()
                    .map_err(MaptileError::backend)?
                    .ok_or_else(|| MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
                let value = row.get_ref(0).map_err(MaptileError::backend)?;
                let bytes = value
                    .as_blob()
                    .map_err(|error| MaptileError::BackendMsg(error.to_string()))?;
                if bytes.len() > MAX_TILE_BYTES {
                    return Err(MaptileError::PayloadTooLarge {
                        limit: MAX_TILE_BYTES,
                    });
                }
                Ok(Bytes::copy_from_slice(bytes))
            })
            .await?;

        if blob.is_empty() {
            return Err(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y));
        }

        Ok(TileResponse {
            data: blob,
            info: self.info,
            last_modified: None,
        })
    }
}

impl DuckDbSource {
    async fn live_get_tile(
        &self,
        xyz: TileCoord,
        sql: String,
        geometry_column: String,
    ) -> MaptileResult<TileResponse> {
        let layer_name = self.layer_name.clone();
        let info = self.info;
        let env = tile_envelope_4326(xyz.z, xyz.x, xyz.y);

        let sql = limit_query(&sql, MAX_QUERY_ROWS + 1);
        let mvt = self
            .run_query(move |conn, control| {
                let mut stmt = conn.prepare(&sql).map_err(MaptileError::backend)?;

                // duckdb-rs 1.10505 exposes result-column metadata only after
                // execution. Asking the unexecuted statement panics, including
                // for valid live-geometry queries.
                let mut rows = stmt.query([]).map_err(MaptileError::backend)?;
                let col_names = rows
                    .as_ref()
                    .expect("fresh query retains its statement")
                    .column_names();
                let geom_idx = col_names
                    .iter()
                    .position(|n| n == &geometry_column)
                    .ok_or_else(|| {
                        MaptileError::Config(format!(
                            "duckdb live-geometry: column {geometry_column:?} not in result set"
                        ))
                    })?;

                let mut features: Vec<crate::wkb_mvt::WkbFeature> = Vec::new();
                let mut remaining = MAX_GEOMETRY_INPUT_BYTES;
                let mut row_count = 0usize;
                while let Some(row) = rows.next().map_err(MaptileError::backend)? {
                    if control.cancelled() {
                        return Err(query_timeout());
                    }
                    row_count += 1;
                    if row_count > MAX_QUERY_ROWS {
                        return Err(MaptileError::Resource(format!(
                            "DuckDB query exceeds {MAX_QUERY_ROWS} rows"
                        )));
                    }
                    let geometry = row.get_ref(geom_idx).map_err(MaptileError::backend)?;
                    let wkb = match geometry {
                        ValueRef::Blob(bytes) | ValueRef::Geometry(bytes) => bytes,
                        _ => continue,
                    };
                    if wkb.is_empty() {
                        continue;
                    }

                    charge(&mut remaining, 128usize.saturating_add(wkb.len()))?;
                    let mut props = serde_json::Map::new();
                    for (i, name) in col_names.iter().enumerate() {
                        if i == geom_idx {
                            continue;
                        }
                        let value = row.get_ref(i).map_err(MaptileError::backend)?;
                        charge(&mut remaining, name.len().saturating_add(128))?;
                        if property_copy_bound(value) > remaining {
                            return Err(query_input_limit());
                        }
                        let value = duckdb_value_to_json(&value.to_owned());
                        if let Some(text) = value.as_str() {
                            charge(&mut remaining, text.len())?;
                        }
                        props.insert(name.clone(), value);
                    }

                    features.push(crate::wkb_mvt::WkbFeature {
                        id: None,
                        wkb: Bytes::copy_from_slice(wkb),
                        properties: props,
                    });
                }

                drop(rows);
                drop(stmt);
                let tile =
                    crate::wkb_mvt::encode_features_to_mvt(&layer_name, features, env, 4096)?;
                if tile.len() > MAX_TILE_BYTES {
                    return Err(MaptileError::PayloadTooLarge {
                        limit: MAX_TILE_BYTES,
                    });
                }
                Ok(tile)
            })
            .await?;

        Ok(TileResponse {
            data: mvt,
            info,
            last_modified: None,
        })
    }
}

fn limit_query(sql: &str, limit: usize) -> String {
    let query = sql.replace('\'', "''");
    format!("SELECT * FROM query('{query}') LIMIT {limit}")
}

fn query_input_limit() -> MaptileError {
    MaptileError::PayloadTooLarge {
        limit: MAX_GEOMETRY_INPUT_BYTES,
    }
}

fn charge(remaining: &mut usize, bytes: usize) -> MaptileResult<()> {
    *remaining = remaining.checked_sub(bytes).ok_or_else(query_input_limit)?;
    Ok(())
}

fn property_copy_bound(value: ValueRef<'_>) -> usize {
    use duckdb::arrow::array::Array;
    use duckdb::types::{EnumType, ListType};
    let nested = match value {
        ValueRef::Text(text) => return text.len(),
        ValueRef::Blob(bytes) | ValueRef::Geometry(bytes) => return bytes.len().saturating_mul(6),
        ValueRef::List(ListType::Regular(array), _) => array.get_array_memory_size(),
        ValueRef::List(ListType::Large(array), _) => array.get_array_memory_size(),
        ValueRef::Array(array, _) => array.get_array_memory_size(),
        ValueRef::Struct(array, _) => array.get_array_memory_size(),
        ValueRef::Map(array, _) => array.get_array_memory_size(),
        ValueRef::Union(array, _) => array.get_array_memory_size(),
        ValueRef::Enum(EnumType::UInt8(array), _) => array.get_array_memory_size(),
        ValueRef::Enum(EnumType::UInt16(array), _) => array.get_array_memory_size(),
        ValueRef::Enum(EnumType::UInt32(array), _) => array.get_array_memory_size(),
        _ => return 0,
    };
    nested.saturating_mul(8)
}

fn duckdb_value_to_json(v: &duckdb::types::Value) -> serde_json::Value {
    use duckdb::types::Value as V;
    use serde_json::Value as J;
    match v {
        V::Null => J::Null,
        V::Boolean(b) => J::Bool(*b),
        V::TinyInt(n) => J::from(*n as i64),
        V::SmallInt(n) => J::from(*n as i64),
        V::Int(n) => J::from(*n as i64),
        V::BigInt(n) => J::from(*n),
        V::UTinyInt(n) => J::from(*n as u64),
        V::USmallInt(n) => J::from(*n as u64),
        V::UInt(n) => J::from(*n as u64),
        V::UBigInt(n) => J::from(*n),
        V::Float(f) => serde_json::Number::from_f64(*f as f64)
            .map(J::Number)
            .unwrap_or(J::Null),
        V::Double(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        V::Text(s) => J::String(s.clone()),
        // Fall-through: stringify everything else.
        other => J::String(format!("{other:?}")),
    }
}

const WEB_MERCATOR_EXTENT: f64 = 20_037_508.342_789_244;

fn tile_to_web_mercator_bbox(z: u8, x: u32, y: u32) -> [f64; 4] {
    let n = 2_f64.powi(z as i32);
    let tile_size = (WEB_MERCATOR_EXTENT * 2.0) / n;
    let xmin = -WEB_MERCATOR_EXTENT + tile_size * (x as f64);
    let xmax = xmin + tile_size;
    let ymax = WEB_MERCATOR_EXTENT - tile_size * (y as f64);
    let ymin = ymax - tile_size;
    [xmin, ymin, xmax, ymax]
}

fn substitute_template(template: &str, z: u8, x: u32, y: u32, bbox: &[f64; 4]) -> String {
    template
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
        .replace("{xmin}", &format!("{:.10}", bbox[0]))
        .replace("{ymin}", &format!("{:.10}", bbox[1]))
        .replace("{xmax}", &format!("{:.10}", bbox[2]))
        .replace("{ymax}", &format!("{:.10}", bbox[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_z0_covers_world() {
        let b = tile_to_web_mercator_bbox(0, 0, 0);
        assert!((b[0] - (-WEB_MERCATOR_EXTENT)).abs() < 1e-6);
        assert!((b[2] - WEB_MERCATOR_EXTENT).abs() < 1e-6);
    }

    #[tokio::test]
    async fn preencoded_query_accepts_quoted_literals_and_trailing_comments() {
        let source = DuckDbSource::open(DuckDbSourceConfig {
            query_template: "SELECT unhex('AABB') AS tile; -- retain valid DuckDB syntax".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let response = source
            .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
            .await
            .unwrap();
        assert_eq!(response.data.as_ref(), [0xaa, 0xbb]);
        assert_eq!(
            source.content_version(),
            source.clone_source().content_version()
        );
    }

    #[test]
    fn template_substitutes_all_placeholders() {
        let s = substitute_template(
            "z={z} x={x} y={y} bb=[{xmin},{ymin},{xmax},{ymax}]",
            5,
            6,
            7,
            &[1.0, 2.0, 3.0, 4.0],
        );
        assert!(s.starts_with("z=5 x=6 y=7"));
        assert!(s.contains("bb=[1.0000000000,2.0000000000,3.0000000000,4.0000000000]"));
    }

    #[test]
    fn tile_envelope_4326_z0_covers_world() {
        let (w, s, e, n) = tile_envelope_4326(0, 0, 0);
        assert!((w - (-180.0)).abs() < 1e-9);
        assert!((e - 180.0).abs() < 1e-9);
        assert!(n > 80.0 && n < 90.0);
        assert!(s > -90.0 && s < -80.0);
    }

    /// Build a minimal little-endian WKB Point at `(lon, lat)`.
    fn wkb_point(lon: f64, lat: f64) -> Vec<u8> {
        let mut out = Vec::with_capacity(21);
        out.push(0x01);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&lon.to_le_bytes());
        out.extend_from_slice(&lat.to_le_bytes());
        out
    }

    #[tokio::test]
    async fn live_geometry_round_trips_through_duckdb_and_mvt() {
        // SQL VALUES query that returns 3 rows with (geom, id, label).
        // The encoded WKB is hex-pasted via to_hex / unhex round-trip.
        let p1 = wkb_point(10.0, 20.0);
        let p2 = wkb_point(10.1, 20.1);
        let p3 = wkb_point(-100.0, -50.0);
        let to_hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };

        let sql = format!(
            "SELECT * FROM (VALUES \
             (unhex('{}'), 1, 'near-a'), \
             (unhex('{}'), 2, 'near-b'), \
             (unhex('{}'), 3, 'far')) t(geom, id, label)",
            to_hex(&p1),
            to_hex(&p2),
            to_hex(&p3),
        );

        let cfg = DuckDbSourceConfig {
            query_template: sql,
            geometry_column: Some("geom".into()),
            layer_name: "places".into(),
            ..Default::default()
        };

        let src = DuckDbSource::open(cfg).await.expect("duckdb open");
        assert_eq!(src.tile_info().format, TileFormat::Mvt);

        // Tile (z=4, x=8, y=7) covers ~(0..22.5, 0..21) which includes
        // the two near-points but not (-100,-50).
        let resp = TileSource::get_tile(&src, TileCoord { z: 4, x: 8, y: 7 }, None)
            .await
            .expect("get_tile live");
        assert!(resp.data.len() > 10, "MVT should be non-empty");
        // First byte of an MVT tile is the field-3 length-delimited tag.
        assert_eq!(resp.data[0], 0x1a);
    }
}
