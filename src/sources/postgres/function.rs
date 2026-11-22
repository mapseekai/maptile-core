//! PostgreSQL function source for MVT returned by application-defined SQL.
//!
//! Functions receive integer `(z, x, y)` coordinates and may also accept a
//! JSON or JSONB parameter object. [`PostgresFunctionSource`] discovers the
//! signature at open time and forwards tile requests through [`PostgresPool`].
//! The source reads mutable data and bypasses the tile cache.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Map, Value};
use tilejson::{TileJSON, tilejson};

use crate::cache::{CacheZoomRange, ContentVersion};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    MAX_TILE_BATCH_BYTES, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource, request::validate_source_request,
};
use crate::transport::MAX_DECODED_TILE_BYTES;

use super::PostgresPool;

#[derive(Debug, Clone)]
pub struct PostgresFunctionConfig {
    pub id: String,
    pub schema: String,
    pub function: String,
    pub description: Option<String>,
    pub minzoom: Option<u8>,
    pub maxzoom: Option<u8>,
    pub bounds: Option<[f64; 4]>,
    /// Legacy cache policy. This source reads mutable data and bypasses the tile cache.
    pub cache_zoom: Option<CacheZoomRange>,
}

#[derive(Clone)]
pub struct PostgresFunctionSource {
    cfg: PostgresFunctionConfig,
    pool: Arc<PostgresPool>,
    sql: String,
    supports_url_query: bool,
    tj: TileJSON,
    info: TileInfo,
    content_version: ContentVersion,
}

impl std::fmt::Debug for PostgresFunctionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresFunctionSource")
            .field("id", &self.cfg.id)
            .field("schema", &self.cfg.schema)
            .field("function", &self.cfg.function)
            .field("supports_url_query", &self.supports_url_query)
            .finish_non_exhaustive()
    }
}

impl PostgresFunctionSource {
    pub async fn open(pool: Arc<PostgresPool>, cfg: PostgresFunctionConfig) -> MaptileResult<Self> {
        super::pool::validate_source_window(cfg.minzoom, cfg.maxzoom, cfg.bounds)?;
        let query_type = probe_signature(&pool, &cfg.schema, &cfg.function).await?;
        let supports_url_query = query_type.is_some();
        let sql = build_sql(&cfg.schema, &cfg.function, query_type);

        let mut tj: TileJSON = tilejson! { tiles: vec![] };
        if let Some(d) = cfg.description.as_ref() {
            tj.description = Some(d.clone());
        }
        if let Some(b) = cfg.bounds {
            tj.bounds = Some(tilejson::Bounds::new(b[0], b[1], b[2], b[3]));
        }
        if let Some(z) = cfg.minzoom {
            tj.minzoom = Some(z);
        }
        if let Some(z) = cfg.maxzoom {
            tj.maxzoom = Some(z);
        }

        let info = TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        };

        Ok(Self {
            cfg,
            pool,
            sql,
            supports_url_query,
            tj,
            info,
            content_version: ContentVersion::new_generation(),
        })
    }

    fn within_zoom_range(&self, zoom: u8) -> bool {
        zoom >= self.cfg.minzoom.unwrap_or(0) && self.cfg.maxzoom.is_none_or(|max| zoom <= max)
    }
}

async fn probe_signature(
    pool: &Arc<PostgresPool>,
    schema: &str,
    function: &str,
) -> MaptileResult<Option<&'static str>> {
    let pending = pool.get_query().await?;
    let row = pending
        .connection()
        .query_opt(
            r#"
            SELECT format_type(p.proargtypes[3], NULL)
            FROM pg_catalog.pg_proc p
            JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
            WHERE n.nspname = $1 AND p.proname = $2
              AND p.prokind = 'f' AND NOT p.proretset AND p.prorettype = 'bytea'::regtype
              AND p.proargtypes[0] = 'integer'::regtype
              AND p.proargtypes[1] = 'integer'::regtype
              AND p.proargtypes[2] = 'integer'::regtype
              AND (p.pronargs = 3 OR (p.pronargs = 4 AND
                   p.proargtypes[3] IN ('json'::regtype, 'jsonb'::regtype)))
            ORDER BY p.pronargs DESC, p.proargtypes[3] DESC NULLS LAST
            LIMIT 1
            "#,
            &[&schema, &function],
        )
        .await
        .map_err(MaptileError::backend)?;
    pending.finish().await?;

    let row = row.ok_or_else(|| MaptileError::Config(format!(
        "no bytea function {schema}.{function} accepts (integer, integer, integer [, json/jsonb])"
    )))?;
    Ok(row
        .get::<_, Option<&str>>(0)
        .map(|arg| if arg == "jsonb" { "jsonb" } else { "json" }))
}

fn build_sql(schema: &str, function: &str, query_type: Option<&'static str>) -> String {
    let s = pg_escape::quote_identifier(schema);
    let f = pg_escape::quote_identifier(function);
    let arg = query_type.map_or(String::new(), |t| format!(", $4::{t}"));
    // OFFSET 0 prevents inlining so a stable function is evaluated only once.
    format!(
        "SELECT octet_length(data), CASE WHEN octet_length(data) <= {MAX_DECODED_TILE_BYTES} THEN data END
         FROM (SELECT {s}.{f}($1::int, $2::int, $3::int{arg}) AS data OFFSET 0) result"
    )
}

fn url_query_to_json(q: &crate::filter::UrlQuery) -> Value {
    let mut m = Map::new();
    for (k, v) in q.iter() {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

fn build_query_payload(q: Option<&TileQuery<'_>>) -> Value {
    let mut payload = match q.and_then(|q| q.url_query) {
        Some(u) => match url_query_to_json(u) {
            Value::Object(m) => m,
            _ => Map::new(),
        },
        None => Map::new(),
    };
    if let Some(bbox) = q.and_then(|q| q.bbox) {
        // Match the URL query JSON contract: values are strings. Typed bbox
        // callers get the same canonical comma-separated WGS84 envelope.
        payload.insert(
            "bbox".to_owned(),
            Value::String(
                bbox.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
    }
    if let Some(filter) = q.and_then(|q| q.filter) {
        let mut filter_obj = Map::new();
        filter_obj.insert(
            "expr".to_string(),
            serde_json::to_value(filter).unwrap_or(Value::Null),
        );
        payload.insert("filter".to_string(), Value::Object(filter_obj));
    }
    Value::Object(payload)
}

#[async_trait]
impl TileSource for PostgresFunctionSource {
    fn source_kind(&self) -> crate::sources::SourceKind {
        crate::sources::SourceKind::PostgresFunction
    }
    fn supports_bbox(&self) -> bool {
        self.supports_url_query
    }
    fn native_batch(&self) -> bool {
        true
    }

    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn content_version(&self) -> ContentVersion {
        self.content_version
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        self.info
    }

    fn supports_url_query(&self) -> bool {
        self.supports_url_query
    }

    fn supports_filter(&self) -> bool {
        // 4-arg variant routes filter through query_params.filter.expr;
        // 3-arg variant has no place to put it.
        self.supports_url_query
    }

    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        CacheZoomRange::disabled()
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        validate_source_request(self, &[xyz], query)?;
        let missing = || MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y);
        if !self.within_zoom_range(xyz.z) {
            return Err(missing());
        }
        let pending = self.pool.get_query().await?;
        let row = self.fetch_tile_on(pending.connection(), xyz, query).await?;
        pending.finish().await?;
        self.tile_response(&row)?.ok_or_else(missing)
    }

    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        let rows = self.read_tile_batch_slots(coords, query).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                row.unwrap_or_else(|| TileResponse {
                    data: Bytes::new(),
                    info: self.info,
                    last_modified: None,
                })
            })
            .collect())
    }

    async fn read_tile_batch_slots(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<Option<TileResponse>>> {
        validate_source_request(self, coords, query)?;
        if coords.iter().all(|coord| !self.within_zoom_range(coord.z)) {
            return Ok(vec![None; coords.len()]);
        }
        let pending = self.pool.get_batch().await?;
        let mut tiles = Vec::with_capacity(coords.len());
        let mut bytes = 0;
        for &coord in coords {
            let tile = if self.within_zoom_range(coord.z) {
                let row = self
                    .fetch_tile_on(pending.connection(), coord, query)
                    .await?;
                self.tile_response(&row)?
            } else {
                None
            };
            bytes += tile.as_ref().map_or(0, |tile| tile.data.len());
            tiles.push(tile);
            if bytes >= MAX_TILE_BATCH_BYTES {
                break;
            }
        }
        pending.finish().await?;
        Ok(tiles)
    }
}

impl PostgresFunctionSource {
    async fn fetch_tile_on(
        &self,
        conn: &deadpool_postgres::Object,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<tokio_postgres::Row> {
        let z = i32::from(xyz.z);
        let x = xyz.x as i32;
        let y = xyz.y as i32;
        let stmt = conn
            .prepare_cached(&self.sql)
            .await
            .map_err(MaptileError::backend)?;

        if self.supports_url_query {
            let json = build_query_payload(query);
            conn.query_one(&stmt, &[&z, &x, &y, &json]).await
        } else {
            conn.query_one(&stmt, &[&z, &x, &y]).await
        }
        .map_err(MaptileError::backend)
    }

    fn tile_response(&self, row: &tokio_postgres::Row) -> MaptileResult<Option<TileResponse>> {
        let size: Option<i32> = row.try_get(0).map_err(MaptileError::backend)?;
        let Some(size) = size else {
            return Ok(None);
        };
        if size as usize > MAX_DECODED_TILE_BYTES {
            return Err(MaptileError::PayloadTooLarge {
                limit: MAX_DECODED_TILE_BYTES,
            });
        }
        let bytes: Vec<u8> = row.try_get(1).map_err(MaptileError::backend)?;
        Ok(Some(TileResponse {
            data: Bytes::from(bytes),
            info: self.info,
            last_modified: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn typed_bbox_is_forwarded_to_function_query_payload() {
        let query = TileQuery {
            bbox: Some([-10.0, -5.0, 10.0, 5.0]),
            ..Default::default()
        };
        assert_eq!(build_query_payload(Some(&query))["bbox"], "-10,-5,10,5");
    }

    #[test]
    fn payload_contains_only_url_query_when_no_filter() {
        let mut q = BTreeMap::new();
        q.insert("a".into(), "1".into());
        let tq = TileQuery {
            url_query: Some(&q),
            filter: None,
            bbox: None,
            style: None,
            style_hash: None,
            pixel_selection: None,
        };
        let v = build_query_payload(Some(&tq));
        assert_eq!(v["a"], serde_json::json!("1"));
        assert!(v.get("filter").is_none());
    }

    #[test]
    fn payload_includes_filter_expr_when_present() {
        // cql2::Expr round-trips through serde; the inner shape is whatever
        // cql2-rs produces (we only assert presence).
        let expr: crate::filter::Cql2Expr = "name IS NULL".parse().unwrap();
        let tq = TileQuery {
            url_query: None,
            filter: Some(&expr),
            bbox: None,
            style: None,
            style_hash: None,
            pixel_selection: None,
        };
        let v = build_query_payload(Some(&tq));
        assert!(v["filter"]["expr"].is_object());
    }

    #[test]
    fn payload_merges_url_query_and_filter() {
        let mut q = BTreeMap::new();
        q.insert("limit".into(), "100".into());
        let expr: crate::filter::Cql2Expr = "x IS NULL".parse().unwrap();
        let tq = TileQuery {
            url_query: Some(&q),
            filter: Some(&expr),
            bbox: None,
            style: None,
            style_hash: None,
            pixel_selection: None,
        };
        let v = build_query_payload(Some(&tq));
        assert_eq!(v["limit"], serde_json::json!("100"));
        assert!(v["filter"]["expr"].is_object());
    }
}
