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
//! Concurrency: the wrapped `duckdb::Connection` is `!Send`, so each
//! request acquires the [`tokio::sync::Mutex`] and executes the query
//! on a `spawn_blocking` task.

use crate::error::{MaptileError, MaptileResult};
use crate::sources::tile_math::tile_envelope_4326;
use crate::sources::{
    TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::Mutex;

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
        let conn = tokio::task::spawn_blocking(move || {
            if path == ":memory:" {
                duckdb::Connection::open_in_memory()
            } else {
                duckdb::Connection::open(&path)
            }
        })
        .await
        .map_err(|e| MaptileError::BackendMsg(e.to_string()))?
        .map_err(MaptileError::backend)?;

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
        })
    }
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
    fn clone_source(&self) -> Box<dyn TileSource> {
        // Connection cannot be cheaply cloned; we share it via Arc<Mutex>.
        Box::new(DuckDbSource {
            id: self.id.clone(),
            tj: self.tj.clone(),
            info: self.info,
            minzoom: self.minzoom,
            maxzoom: self.maxzoom,
            query_template: self.query_template.clone(),
            conn: self.conn.clone(),
            geometry_column: self.geometry_column.clone(),
            layer_name: self.layer_name.clone(),
            cache_zoom: self.cache_zoom,
        })
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

        let conn = self.conn.clone();
        let blob: Vec<u8> = tokio::task::spawn_blocking(move || -> MaptileResult<Vec<u8>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(&sql).map_err(MaptileError::backend)?;
            let mut rows = stmt.query([]).map_err(MaptileError::backend)?;
            let row = rows
                .next()
                .map_err(MaptileError::backend)?
                .ok_or_else(|| MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
            let bytes: Vec<u8> = row.get::<_, Vec<u8>>(0).map_err(MaptileError::backend)?;
            Ok(bytes)
        })
        .await
        .map_err(|e| MaptileError::BackendMsg(e.to_string()))??;

        if blob.is_empty() {
            return Err(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y));
        }

        Ok(TileResponse {
            data: Bytes::from(blob),
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
        let conn = self.conn.clone();
        let layer_name = self.layer_name.clone();
        let info = self.info;
        let env = tile_envelope_4326(xyz.z, xyz.x, xyz.y);

        let mvt: Bytes = tokio::task::spawn_blocking(move || -> MaptileResult<Bytes> {
            let conn = conn.blocking_lock();
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
            while let Some(row) = rows.next().map_err(MaptileError::backend)? {
                let wkb: Vec<u8> = match row.get::<_, Vec<u8>>(geom_idx) {
                    Ok(v) => v,
                    Err(_) => continue, // null geometry → skip
                };
                if wkb.is_empty() {
                    continue;
                }

                let mut props = serde_json::Map::with_capacity(col_names.len() - 1);
                for (i, name) in col_names.iter().enumerate() {
                    if i == geom_idx {
                        continue;
                    }
                    let v: duckdb::types::Value = row
                        .get::<_, duckdb::types::Value>(i)
                        .map_err(MaptileError::backend)?;
                    props.insert(name.clone(), duckdb_value_to_json(&v));
                }

                features.push(crate::wkb_mvt::WkbFeature {
                    id: None,
                    wkb: Bytes::from(wkb),
                    properties: props,
                });
            }

            crate::wkb_mvt::encode_features_to_mvt(&layer_name, features, env, 4096)
        })
        .await
        .map_err(|e| MaptileError::BackendMsg(e.to_string()))??;

        Ok(TileResponse {
            data: mvt,
            info,
            last_modified: None,
        })
    }
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
