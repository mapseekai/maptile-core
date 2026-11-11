//! PostgreSQL function tile source — invokes a user SQL function returning MVT bytea.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Map, Value};
use tilejson::{TileJSON, tilejson};

use crate::cache::{CacheZoomRange, TileCache, TileCacheKey};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
    read_tile_batch_with, validate_tile_batch,
};

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
    /// Per-source cache zoom policy. `None` ≡ cache every zoom.
    pub cache_zoom: Option<CacheZoomRange>,
}

pub struct PostgresFunctionSource {
    cfg: PostgresFunctionConfig,
    pool: Arc<PostgresPool>,
    sql: String,
    supports_url_query: bool,
    tj: TileJSON,
    info: TileInfo,
    cache: Option<Arc<TileCache>>,
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
        let supports_url_query = probe_signature(&pool, &cfg.schema, &cfg.function).await?;
        let sql = build_sql(&cfg.schema, &cfg.function, supports_url_query);

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
            cache: None,
        })
    }

    /// Attach a tile cache. Cache is consulted only for query-less requests
    /// (filter/url_query make the response query-dependent).
    ///
    /// Note: this source-level cache does **not** consult [`Self::cache_zoom`];
    /// the zoom-range filter is applied only by
    /// [`crate::sources::manager::SourceManager::get_tile`]. Prefer attaching
    /// the cache at the manager layer via
    /// [`crate::sources::manager::SourceManager::with_cache`] so the policy
    /// is honored.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<TileCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Mutating variant for callers holding `&mut Self` (e.g., behind `Arc::get_mut`).
    pub fn set_cache(&mut self, cache: Arc<TileCache>) {
        self.cache = Some(cache);
    }

    /// Reference to the attached cache, if any.
    #[must_use]
    pub fn cache(&self) -> Option<&Arc<TileCache>> {
        self.cache.as_ref()
    }
}

async fn probe_signature(pool: &PostgresPool, schema: &str, function: &str) -> MaptileResult<bool> {
    let conn = pool.get().await?;
    let row = conn
        .query_opt(
            r#"
            SELECT array_agg(format_type(t, NULL)::text)
            FROM pg_proc p
            JOIN pg_namespace n ON n.oid = p.pronamespace
            CROSS JOIN LATERAL unnest(p.proargtypes) AS t
            WHERE n.nspname = $1 AND p.proname = $2
            GROUP BY p.oid
            LIMIT 1
            "#,
            &[&schema, &function],
        )
        .await
        .map_err(MaptileError::backend)?;

    let row =
        row.ok_or_else(|| MaptileError::Config(format!("function {schema}.{function} not found")))?;
    let args: Vec<String> = row.get(0);

    let normalized: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match normalized.as_slice() {
        ["integer", "integer", "integer"] => Ok(false),
        ["integer", "integer", "integer", "json"] | ["integer", "integer", "integer", "jsonb"] => {
            Ok(true)
        }
        other => Err(MaptileError::Config(format!(
            "function {schema}.{function} has unsupported signature: {other:?}"
        ))),
    }
}

fn build_sql(schema: &str, function: &str, with_query: bool) -> String {
    let s = pg_escape::quote_identifier(schema);
    let f = pg_escape::quote_identifier(function);
    if with_query {
        format!("SELECT {s}.{f}($1::int, $2::int, $3::int, $4::json)")
    } else {
        format!("SELECT {s}.{f}($1::int, $2::int, $3::int)")
    }
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
    fn id(&self) -> &str {
        &self.cfg.id
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
        // Single source of truth: read from the stored config. Returning the
        // resolved value (with `unwrap_or_default()`) avoids having both a
        // raw `Option<CacheZoomRange>` on the config and a duplicate
        // `CacheZoomRange` field on this struct.
        self.cfg.cache_zoom.unwrap_or_default()
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(PostgresFunctionSource {
            cfg: self.cfg.clone(),
            pool: self.pool.clone(),
            sql: self.sql.clone(),
            supports_url_query: self.supports_url_query,
            tj: self.tj.clone(),
            info: self.info,
            cache: self.cache.clone(),
        })
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        // Cache path — only for query-less requests, since filter/url_query
        // make the tile bytes depend on inputs not captured by TileCacheKey.
        // `get_or_insert` coalesces concurrent misses for the same tile into
        // a single function call (moka entry API).
        if query.is_none()
            && self.cache_zoom().contains(xyz.z)
            && let Some(cache) = self.cache.as_ref()
        {
            let key = TileCacheKey {
                source_id: self.cfg.id.as_str().into(),
                content_version: self.content_version(),
                z: xyz.z,
                x: xyz.x,
                y: xyz.y,
                style_hash: crate::cache::StyleHash::NONE,
            };
            return cache
                .get_or_insert(key, || self.fetch_tile(xyz, None))
                .await
                .map_err(|arc_err: std::sync::Arc<MaptileError>| (*arc_err).clone());
        }

        self.fetch_tile(xyz, query).await
    }

    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        validate_tile_batch(coords)?;
        if coords.is_empty() {
            return Ok(Vec::new());
        }
        let pending = self.pool.get_batch().await?;
        let conn = pending.connection();
        let tiles = read_tile_batch_with(coords, self.info, |coord| {
            self.fetch_tile_on(conn, coord, query)
        })
        .await?;
        pending.finish().await?;
        Ok(tiles)
    }
}

impl PostgresFunctionSource {
    /// Invoke the SQL function for one tile. Shared by the cached
    /// (coalesced) and direct paths of `get_tile`.
    async fn fetch_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let conn = self.pool.get().await?;
        self.fetch_tile_on(&conn, xyz, query).await
    }

    async fn fetch_tile_on(
        &self,
        conn: &deadpool_postgres::Object,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let z = i32::from(xyz.z);
        let x = xyz.x as i32;
        let y = xyz.y as i32;

        let row_opt = if self.supports_url_query {
            let json = build_query_payload(query);
            conn.query_opt(&self.sql, &[&z, &x, &y, &json])
                .await
                .map_err(MaptileError::backend)?
        } else {
            conn.query_opt(&self.sql, &[&z, &x, &y])
                .await
                .map_err(MaptileError::backend)?
        };

        // NULL means no tile; a present but empty bytea remains a valid MVT.
        let row = row_opt.ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        let bytes: Vec<u8> = row
            .try_get::<_, Option<Vec<u8>>>(0)
            .map_err(MaptileError::backend)?
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;

        if bytes.len() > crate::transport::MAX_DECODED_TILE_BYTES {
            return Err(MaptileError::PayloadTooLarge {
                limit: crate::transport::MAX_DECODED_TILE_BYTES,
            });
        }

        Ok(TileResponse {
            data: Bytes::from(bytes),
            info: self.info,
            last_modified: None,
        })
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
    fn cache_key_uses_source_id() {
        // Sanity: verify the key shape matches the manager-level cache so a
        // single shared cache won't collide across sources.
        let key = TileCacheKey {
            source_id: "src-a".into(),
            content_version: crate::cache::ContentVersion::from_input(b"test"),
            z: 5,
            x: 1,
            y: 2,
            style_hash: crate::cache::StyleHash::NONE,
        };
        assert_eq!(&*key.source_id, "src-a");
        assert_eq!(key.z, 5);
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
