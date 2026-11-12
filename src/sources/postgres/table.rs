//! PostgreSQL table tile source — emits MVT bytes by running an inline
//! `SELECT ST_AsMVT(...)` per tile, with optional CQL2 filter dispatch.
//!
//! On open: discovers the table's geometry column, SRID, properties, and
//! bounds, builds the cached SQL templates (full inline + base + bbox-search
//! fragments), and pre-prepares the unfiltered query on every pooled
//! connection. No DDL is issued on the renderer database.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use tilejson::{TileJSON, tilejson};
use tokio_postgres::types::{ToSql, Type};

use crate::cache::{CacheZoomRange, TileCache, TileCacheKey};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    MAX_TILE_BATCH_BYTES, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource, validate_tile_batch,
};

use super::pool::PostgresPool;

#[derive(Debug, Clone)]
pub struct PostgresTableConfig {
    pub id: String,
    pub schema: String,
    pub table: String,
    pub description: Option<String>,
    pub bounds: Option<[f64; 4]>,

    pub geometry_column: Option<String>,
    pub id_column: Option<String>,
    pub properties: Option<Vec<String>>,

    pub cache_zoom: Option<CacheZoomRange>,
}

impl PostgresTableConfig {
    pub fn new(id: impl Into<String>, schema: impl Into<String>, table: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            schema: schema.into(),
            table: table.into(),
            description: None,
            bounds: None,
            geometry_column: None,
            id_column: None,
            properties: None,
            cache_zoom: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub schema: String,
    pub table: String,
    pub geometry_column: String,
    pub srid: i32,
    pub geometry_type: String,
    pub id_column: Option<String>,
    pub properties: Vec<String>,
    pub bounds: Option<[f64; 4]>,
    pub has_spatial_index: bool,
    /// Minimum zoom at which the dataset spans at least
    /// `MIN_ZOOM_PIXELS` px — derived from `bounds` at open time.
    /// `None` when bounds are unknown. Below this zoom `get_tile`
    /// short-circuits to an empty tile without a DB round-trip.
    pub derived_min_zoom: Option<u8>,
}

pub struct PostgresTableSource {
    cfg: PostgresTableConfig,
    info: TableInfo,
    pool: Arc<PostgresPool>,
    tj: TileJSON,
    /// Inline `SELECT ST_AsMVT(...)` SQL used for the fast path: no filter,
    /// no url_query. Mirrors martin / tileserver-rs's table source — avoids
    /// the plpgsql wrapper for the common case so PG can reuse the prepared
    /// statement plan without extra function-invocation overhead.
    inline_query: String,
    batch_query: String,
    batch_base_select_no_where: String,
    batch_bbox_search_expr: String,
    /// Inner SELECT (no WHERE). Re-used by the filter path in T8 to splice
    /// CQL2 + bbox conditions before closing the subquery.
    base_select_no_where: String,
    /// SRID-aware bbox-search fragment, computed once at open time.
    bbox_search_expr: String,
    /// Columns the CQL2 filter is allowed to reference. Empty by default
    /// — `SourceFactory` injects this from `attributes_schema.keys()`.
    allowed_columns: Arc<std::collections::HashSet<String>>,
    info_meta: TileInfo,
    cache: Option<Arc<TileCache>>,
    definition_version: crate::cache::ContentVersion,
    content_version: crate::cache::ContentVersion,
}

impl std::fmt::Debug for PostgresTableSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresTableSource")
            .field("id", &self.cfg.id)
            .field("schema", &self.info.schema)
            .field("table", &self.info.table)
            .finish_non_exhaustive()
    }
}

fn table_definition_version(
    pool: &PostgresPool,
    info: &TableInfo,
    query: &str,
) -> crate::cache::ContentVersion {
    let mut hash = blake3::Hasher::new();
    hash.update(pool.database_identity.as_bytes());
    for field in [&info.schema, &info.table] {
        hash.update(&(field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hash.update(&(query.len() as u64).to_le_bytes());
    hash.update(query.as_bytes());
    hash.update(&[u8::from(info.bounds.is_some())]);
    if let Some(bounds) = info.bounds {
        for value in bounds {
            hash.update(&value.to_bits().to_le_bytes());
        }
    }
    hash.update(&[info.derived_min_zoom.unwrap_or(u8::MAX)]);
    crate::cache::ContentVersion::from_bytes(*hash.finalize().as_bytes())
}

impl PostgresTableSource {
    pub async fn open(pool: Arc<PostgresPool>, cfg: PostgresTableConfig) -> MaptileResult<Self> {
        let info = discover_table(&pool, &cfg).await?;
        let columns = discover_property_types(&pool, &info).await?;

        let tj = build_tilejson_with_vector_layers(
            &cfg,
            cfg.bounds.or(info.bounds),
            info.derived_min_zoom,
            &columns,
        );

        let inline_query = build_inline_tile_query(&info, &columns, pool.supports_tile_margin());
        let batch_query = build_batch_tile_query(&info, &columns, pool.supports_tile_margin());
        let batch_coords = ["batch_coords.z", "batch_coords.x", "batch_coords.y"];
        let batch_base_select_no_where =
            build_base_select_with_coordinates(&info, &columns, batch_coords);
        let batch_bbox_search_expr =
            build_bbox_search_with_coordinates(&info, pool.supports_tile_margin(), batch_coords);
        let base_select_no_where = build_base_select_no_where(&info, &columns);
        let bbox_search_expr = build_bbox_search(&info, pool.supports_tile_margin());
        let info_meta = TileInfo {
            format: TileFormat::Mvt,
            encoding: TileEncoding::Uncompressed,
        };
        let definition_version = table_definition_version(&pool, &info, &inline_query);

        Ok(Self {
            cfg,
            info,
            pool,
            tj,
            inline_query,
            batch_query,
            batch_base_select_no_where,
            batch_bbox_search_expr,
            base_select_no_where,
            bbox_search_expr,
            allowed_columns: Arc::new(std::collections::HashSet::new()),
            info_meta,
            cache: None,
            definition_version,
            content_version: crate::cache::ContentVersion::new_generation(),
        })
    }

    /// Inline filter dispatch (spec D5/D6). Validates the CQL2 expression
    /// against `self.allowed_columns`, renders to a PG WHERE fragment via
    /// cql2-rs, splices it (and the optional `?bbox=` envelope) into the
    /// per-request SQL, then uses non-cached prepare so changing values do not grow the
    /// connection statement cache. Fixed tile SQL still uses cached prepare.
    ///
    /// No source-level cache lookup here: the cache key is keyed on
    /// `(source_id, z, x, y)` only, so filtered responses would alias
    /// each other.
    async fn run_filtered(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let filter_sql = self.filter_sql(query)?;
        let user_bbox = query.and_then(|q| q.bbox);

        let sql = compose_filtered_sql(
            &self.base_select_no_where,
            &self.bbox_search_expr,
            &self.info,
            &filter_sql,
            user_bbox,
        );

        let conn = self.pool.get().await?;
        let stmt = conn
            .prepare_typed(&sql, &[Type::INT4, Type::INT4, Type::INT4])
            .await
            .map_err(MaptileError::backend)?;
        let z = i32::from(xyz.z);
        let x = xyz.x as i32;
        let y = xyz.y as i32;
        let row_opt = conn
            .query_opt(&stmt, &[&z, &x, &y])
            .await
            .map_err(MaptileError::backend)?;
        let bytes: Vec<u8> = row_opt
            .and_then(|row| row.get::<_, Option<Vec<u8>>>(0))
            .unwrap_or_default();
        Ok(TileResponse {
            data: Bytes::from(bytes),
            info: self.info_meta,
            last_modified: None,
        })
    }

    fn filter_sql(&self, query: Option<&TileQuery<'_>>) -> MaptileResult<String> {
        let sql = match query.and_then(|q| q.filter) {
            Some(expr) => {
                let allowed: std::collections::HashSet<&str> =
                    self.allowed_columns.iter().map(String::as_str).collect();
                // FilterError flows through MaptileError::Filter (via #[from])
                // so the boundary mapping in interfaces/rpc/error.rs surfaces
                // it as 4xx, not 500.
                crate::filter::validate(expr, &allowed)?;
                crate::filter::to_sql_where(expr)?
            }
            None => String::new(),
        };
        Ok(sql)
    }

    /// Run the unfiltered inline query and gzip the result. Compress
    /// before caching: nearly every HTTP client sends
    /// `Accept-Encoding: gzip`, so caching the gzipped bytes lets
    /// tile_service pass them through unchanged on every cache hit
    /// instead of re-compressing the same MVT for the lifetime of the
    /// entry. Empty results stay empty (avoid emitting a 20-byte gzip
    /// stream for nothing).
    async fn fetch_and_encode(&self, xyz: TileCoord) -> MaptileResult<TileResponse> {
        let conn = self.pool.get().await?;
        let stmt = conn
            .prepare_typed_cached(&self.inline_query, &[Type::INT4; 3])
            .await
            .map_err(MaptileError::backend)?;
        let z = i32::from(xyz.z);
        let x = xyz.x as i32;
        let y = xyz.y as i32;
        let row_opt = conn
            .query_opt(&stmt, &[&z, &x, &y])
            .await
            .map_err(MaptileError::backend)?;

        let bytes: Vec<u8> = row_opt
            .and_then(|row| row.get::<_, Option<Vec<u8>>>(0))
            .unwrap_or_default();

        drop(conn);

        let (data, encoding) = if bytes.is_empty() {
            (Bytes::new(), TileEncoding::Uncompressed)
        } else {
            let gz =
                crate::transport::encode_as_async(Bytes::from(bytes), TileEncoding::Gzip).await?;
            (gz, TileEncoding::Gzip)
        };

        Ok(TileResponse {
            data,
            info: TileInfo {
                format: TileFormat::Mvt,
                encoding,
            },
            last_modified: None,
        })
    }

    /// Inject the column whitelist used by the CQL2 filter validator.
    /// `SourceFactory` calls this with `attributes_schema.keys()` from
    /// the source config (D4); callers that don't set a whitelist will
    /// have every filter rejected with `unknown column` errors.
    #[must_use]
    pub fn with_allowed_columns(mut self, allowed: Arc<std::collections::HashSet<String>>) -> Self {
        self.allowed_columns = allowed;
        self
    }

    pub fn table_info(&self) -> &TableInfo {
        &self.info
    }

    /// Partition caches by the durable version committed with feature/schema edits.
    #[must_use]
    pub fn with_edit_revision(mut self, revision: Option<String>) -> Self {
        if let Some(value) = &revision {
            self.tj
                .other
                .insert("mapseek_revision".into(), serde_json::json!(value));
            let mut hash = blake3::Hasher::new();
            hash.update(self.definition_version.as_bytes());
            hash.update(value.as_bytes());
            self.content_version =
                crate::cache::ContentVersion::from_bytes(*hash.finalize().as_bytes());
        } else {
            self.tj.other.remove("mapseek_revision");
            self.content_version = crate::cache::ContentVersion::new_generation();
        }
        self
    }

    /// Attach a tile cache before sharing this table source.
    /// Query-less requests within [`Self::cache_zoom`] use this cache. Manager
    /// cache fills forward an explicit query and bypass the source-level cache
    /// to avoid recursively waiting on the same shared cache key.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<crate::cache::TileCache>) -> Self {
        self.cache = Some(cache);
        self
    }
}

#[async_trait]
impl TileSource for PostgresTableSource {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        self.info_meta
    }

    fn supports_url_query(&self) -> bool {
        false
    }

    fn supports_filter(&self) -> bool {
        true
    }

    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        self.cfg.cache_zoom.unwrap_or_default()
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(PostgresTableSource {
            cfg: self.cfg.clone(),
            info: self.info.clone(),
            pool: self.pool.clone(),
            tj: self.tj.clone(),
            inline_query: self.inline_query.clone(),
            batch_query: self.batch_query.clone(),
            batch_base_select_no_where: self.batch_base_select_no_where.clone(),
            batch_bbox_search_expr: self.batch_bbox_search_expr.clone(),
            base_select_no_where: self.base_select_no_where.clone(),
            bbox_search_expr: self.bbox_search_expr.clone(),
            allowed_columns: self.allowed_columns.clone(),
            info_meta: self.info_meta,
            cache: self.cache.clone(),
            definition_version: self.definition_version,
            content_version: self.content_version,
        })
    }

    /// The physical table, rendered projection and durable edit revision
    /// identify cached bytes without querying PostgreSQL on a cache hit.
    fn content_version(&self) -> crate::cache::ContentVersion {
        self.content_version
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        // Dispatch (spec D5/D6):
        //   - filter or bbox set    → inline filter SQL via cql2-rs
        //   - otherwise (incl. plain url_query) → cached prepared
        //     unfiltered inline_query
        // url_query is no longer routed to the plpgsql function path —
        // it stays in tile_service for normalization into a CQL2 filter.
        let filter_present = query.and_then(|q| q.filter).is_some();
        let bbox_present = query.and_then(|q| q.bbox).is_some();
        if filter_present || bbox_present {
            return self.run_filtered(xyz, query).await;
        }

        // Min-zoom short-circuit: below the derived minzoom the whole
        // dataset clips to sub-pixel — skip the DB round-trip. Clients
        // that honor `TileJSON.minzoom` never request these zooms; this
        // covers the ones that don't.
        if self.info.derived_min_zoom.is_some_and(|mz| xyz.z < mz) {
            return Ok(TileResponse {
                data: Bytes::new(),
                info: self.info_meta,
                last_modified: None,
            });
        }

        // Bbox short-circuit: if the dataset bounds and the requested tile
        // do not intersect, skip the DB round-trip entirely. At low zoom
        // this is by far the biggest performance win for spatially-small
        // datasets — the client requests every tile in view but only 1-2
        // tiles actually overlap the data.
        if let Some(bounds) = self.info.bounds.as_ref()
            && !tile_intersects_bounds(xyz, bounds)
        {
            return Ok(TileResponse {
                data: bytes::Bytes::new(),
                info: self.info_meta,
                last_modified: None,
            });
        }

        // Cache path — only safe for query-less requests. `get_or_insert`
        // coalesces concurrent misses for the same tile into a single PG
        // query (moka entry API), so a fresh viewport's duplicate requests
        // don't fan out into identical queries.
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
                .get_or_insert(key, || self.fetch_and_encode(xyz))
                .await
                .map_err(|arc_err: std::sync::Arc<MaptileError>| (*arc_err).clone());
        }
        tracing::debug!(
            source = %self.cfg.id,
            z = xyz.z, x = xyz.x, y = xyz.y,
            "source-level tile cache bypassed"
        );
        self.fetch_and_encode(xyz).await
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
        let filtered = query.is_some_and(|q| q.filter.is_some() || q.bbox.is_some());
        let dynamic_sql = if filtered {
            let filter = self.filter_sql(query)?;
            let filter = if filter.is_empty() {
                "batch_coords.enabled".into()
            } else {
                format!("batch_coords.enabled AND ({filter})")
            };
            let tile = compose_filtered_sql(
                &self.batch_base_select_no_where,
                &self.batch_bbox_search_expr,
                &self.info,
                &filter,
                query.and_then(|q| q.bbox),
            );
            Some(compose_batch_sql(&tile))
        } else {
            None
        };
        let enabled: Vec<_> = coords
            .iter()
            .map(|&coord| {
                filtered
                    || !(self.info.derived_min_zoom.is_some_and(|min| coord.z < min)
                        || self
                            .info
                            .bounds
                            .is_some_and(|bounds| !tile_intersects_bounds(coord, &bounds)))
            })
            .collect();
        if !enabled.contains(&true) {
            return Ok(coords
                .iter()
                .map(|_| TileResponse {
                    data: Bytes::new(),
                    info: self.info_meta,
                    last_modified: None,
                })
                .collect());
        }
        let zs: Vec<_> = coords.iter().map(|c| i32::from(c.z)).collect();
        let xs: Vec<_> = coords.iter().map(|c| c.x as i32).collect();
        let ys: Vec<_> = coords.iter().map(|c| c.y as i32).collect();
        let pending = self.pool.get_batch().await?;
        let conn = pending.connection();
        let types = [
            Type::INT4_ARRAY,
            Type::INT4_ARRAY,
            Type::INT4_ARRAY,
            Type::BOOL_ARRAY,
        ];
        let statement = if let Some(sql) = &dynamic_sql {
            conn.prepare_typed(sql, &types).await
        } else {
            conn.prepare_typed_cached(&self.batch_query, &types).await
        }
        .map_err(MaptileError::backend)?;
        let tiles = {
            let params: [&(dyn ToSql + Sync); 4] = [&zs, &xs, &ys, &enabled];
            let rows = conn
                .query_raw(&statement, params)
                .await
                .map_err(MaptileError::backend)?;
            futures_util::pin_mut!(rows);
            let mut tiles = Vec::with_capacity(coords.len());
            let mut bytes = 0;
            while let Some(row) = rows.try_next().await.map_err(MaptileError::backend)? {
                let ordinal: i64 = row.try_get(0).map_err(MaptileError::backend)?;
                if usize::try_from(ordinal).ok() != Some(tiles.len() + 1) {
                    return Err(MaptileError::InvalidMetadata(
                        "tile batch returned coordinates out of order".into(),
                    ));
                }
                let data: Option<Vec<u8>> = row.try_get(1).map_err(MaptileError::backend)?;
                let data = crate::transport::decode_async(
                    Bytes::from(data.unwrap_or_default()),
                    TileEncoding::Uncompressed,
                )
                .await?;
                bytes += data.len();
                tiles.push(TileResponse {
                    data,
                    info: self.info_meta,
                    last_modified: None,
                });
                if bytes >= MAX_TILE_BATCH_BYTES {
                    break;
                }
            }
            if tiles.is_empty() || tiles.len() > coords.len() {
                return Err(MaptileError::InvalidMetadata(
                    "tile batch returned an invalid coordinate count".into(),
                ));
            }
            tiles
        };
        pending.finish().await?;
        Ok(tiles)
    }
}

// === Discovery helpers ===

async fn discover_table(
    pool: &PostgresPool,
    cfg: &PostgresTableConfig,
) -> MaptileResult<TableInfo> {
    let conn = pool.get().await?;

    let geometry_column = match cfg.geometry_column.clone() {
        Some(c) => c,
        None => find_geometry_column(&conn, &cfg.schema, &cfg.table).await?,
    };

    let (srid, geometry_type) =
        get_geometry_info(&conn, &cfg.schema, &cfg.table, &geometry_column).await?;

    let properties = match cfg.properties.clone() {
        Some(p) => p,
        None => discover_properties(&conn, &cfg.schema, &cfg.table, &geometry_column).await?,
    };

    let has_spatial_index = check_spatial_index(&conn, &cfg.schema, &cfg.table).await?;
    if !has_spatial_index {
        tracing::warn!(
            "table {}.{} has no spatial index on '{}'; tile queries will be slow",
            cfg.schema,
            cfg.table,
            geometry_column
        );
    }

    let bounds = match cfg.bounds {
        Some(b) => Some(b),
        None => estimate_bounds(&conn, &cfg.schema, &cfg.table, &geometry_column).await,
    };

    Ok(TableInfo {
        schema: cfg.schema.clone(),
        table: cfg.table.clone(),
        geometry_column,
        srid,
        geometry_type,
        id_column: cfg.id_column.clone(),
        properties,
        bounds,
        has_spatial_index,
        derived_min_zoom: bounds.as_ref().map(derive_min_zoom),
    })
}

/// Smallest tile span (in CSS px at 256px/tile) a dataset must cover
/// before its tiles are worth generating. Below the derived zoom the
/// whole dataset clips to under this many pixels, so PG would scan the
/// table only for ST_AsMVTGeom to discard everything.
const MIN_ZOOM_PIXELS: f64 = 4.0;

/// Derive the minimum useful zoom from WGS84 bounds: the smallest `z`
/// where `max(Δlon, Δlat)` maps to at least `MIN_ZOOM_PIXELS` px
/// (world = 256·2^z px). Surfaced via `TileJSON.minzoom` and enforced
/// as a `get_tile` short-circuit for clients that ignore minzoom.
fn derive_min_zoom(bounds: &[f64; 4]) -> u8 {
    let span = (bounds[2] - bounds[0])
        .abs()
        .max((bounds[3] - bounds[1]).abs());
    if !span.is_finite() || span <= 0.0 {
        return 0;
    }
    let z = (MIN_ZOOM_PIXELS * 360.0 / (256.0 * span)).log2().ceil();
    if z.is_nan() {
        0
    } else {
        z.clamp(0.0, 22.0) as u8
    }
}

async fn find_geometry_column(
    conn: &deadpool_postgres::Object,
    schema: &str,
    table: &str,
) -> MaptileResult<String> {
    let rows = conn
        .query(
            "SELECT f_geometry_column::text FROM geometry_columns
             WHERE f_table_schema = $1 AND f_table_name = $2",
            &[&schema, &table],
        )
        .await
        .map_err(MaptileError::backend)?;

    match rows.len() {
        0 => Err(MaptileError::Config(format!(
            "table {schema}.{table} has no PostGIS geometry column"
        ))),
        1 => Ok(rows[0].get(0)),
        n => {
            let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
            Err(MaptileError::Config(format!(
                "table {schema}.{table} has {n} geometry columns ({names:?}); \
                 specify one in PostgresTableConfig.geometry_column"
            )))
        }
    }
}

async fn get_geometry_info(
    conn: &deadpool_postgres::Object,
    schema: &str,
    table: &str,
    geom_col: &str,
) -> MaptileResult<(i32, String)> {
    let row = conn
        .query_opt(
            "SELECT srid, type::text FROM geometry_columns
             WHERE f_table_schema = $1 AND f_table_name = $2 AND f_geometry_column = $3",
            &[&schema, &table, &geom_col],
        )
        .await
        .map_err(MaptileError::backend)?
        .ok_or_else(|| {
            MaptileError::Config(format!(
                "geometry_columns missing entry for {schema}.{table}.{geom_col}"
            ))
        })?;
    Ok((row.get(0), row.get(1)))
}

async fn discover_properties(
    conn: &deadpool_postgres::Object,
    schema: &str,
    table: &str,
    geom_col: &str,
) -> MaptileResult<Vec<String>> {
    let rows = conn
        .query(
            "SELECT column_name::text FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2 AND column_name != $3
               AND data_type IN ('integer','bigint','smallint','real','double precision',
                                 'numeric','text','character varying','character',
                                 'boolean','json','jsonb','date',
                                 'timestamp without time zone','timestamp with time zone',
                                 'uuid')
             ORDER BY ordinal_position",
            &[&schema, &table, &geom_col],
        )
        .await
        .map_err(MaptileError::backend)?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

/// Map a PostgreSQL `data_type` (from `information_schema.columns`) to the
/// TileJSON `vector_layers[].fields` value type. Per the TileJSON 3.0 spec
/// only `String`, `Number`, and `Boolean` are valid; anything else
/// degrades gracefully to `String`.
fn pg_type_to_tilejson_field(pg_type: &str) -> &'static str {
    match pg_type.to_ascii_lowercase().as_str() {
        "integer" | "bigint" | "smallint" | "real" | "double precision" | "numeric" => "Number",
        "boolean" => "Boolean",
        _ => "String",
    }
}

/// Build the `TileJSON` advertised by a `PostgresTableSource`. The single
/// `vector_layers[]` entry is named after the source `id` and carries one
/// `fields` entry per discovered column, with each PG `data_type` mapped to
/// the TileJSON field-type vocabulary by [`pg_type_to_tilejson_field`].
fn build_tilejson_with_vector_layers(
    cfg: &PostgresTableConfig,
    bounds: Option<[f64; 4]>,
    min_zoom: Option<u8>,
    columns: &[(String, String)],
) -> TileJSON {
    let mut tj = tilejson! { tiles: vec![] };
    if let Some(d) = cfg.description.as_ref() {
        tj.description = Some(d.clone());
    }
    if let Some(b) = bounds {
        tj.bounds = Some(tilejson::Bounds::new(b[0], b[1], b[2], b[3]));
    }
    // The RPC TileInfo contract requires an explicit zoom range. Keep the
    // bounds-derived minimum when available, and publish the serving defaults
    // otherwise so a valid source is not rejected during conversion.
    tj.minzoom = Some(min_zoom.unwrap_or(0));
    tj.maxzoom = Some(22);
    let fields: std::collections::BTreeMap<String, String> = columns
        .iter()
        .map(|(name, ty)| (name.clone(), pg_type_to_tilejson_field(ty).to_string()))
        .collect();
    // Layer id MUST match what the CQL2 SQL function emits as the MVT
    // layer name — `generate_cql2_tile_function_with_columns` uses
    // `ST_AsMVT(tile, '<table_name>', ...)`. If we used `cfg.id` here
    // (e.g. "vector.<table>"), the MVT bytes would be tagged "<table>"
    // and a MapLibre client matching by tilejson layer id would render
    // an empty map.
    let mut layer = tilejson::VectorLayer::new(cfg.table.clone(), fields);
    layer.description = cfg.description.clone();
    layer.minzoom = tj.minzoom;
    layer.maxzoom = tj.maxzoom;
    tj.vector_layers = Some(vec![layer]);
    tj
}

/// Compute the WGS84 lon/lat bounds of the (z,x,y) tile and check if it
/// overlaps the dataset's geographic bounds. Used by `get_tile` to skip
/// DB round-trips for tiles that can't possibly contain data — the
/// largest single-request optimization for spatially-small datasets.
fn tile_intersects_bounds(xyz: TileCoord, bounds: &[f64; 4]) -> bool {
    let tile_bbox = martin_tile_utils::xyz_to_bbox(xyz.z, xyz.x, xyz.y, xyz.x, xyz.y);
    // tile_bbox is [west, south, east, north]; bounds same order.
    let [t_w, t_s, t_e, t_n] = tile_bbox;
    let [b_w, b_s, b_e, b_n] = *bounds;
    !(t_e < b_w || t_w > b_e || t_n < b_s || t_s > b_n)
}

/// Build the inline `SELECT ST_AsMVT(...)` query used for unfiltered tile
/// requests. Template mirrors **martin** (`martin/src/config/file/tiles/
/// postgres/resolver/query_tables.rs`, `table_to_query`) and **tileserver-rs**
/// (`crates/tileserver-rs/src/sources/postgres/table.rs`, `build_tile_query`)
/// byte-for-byte where the configurable bits are constants in our build.
///
/// Key invariants kept identical to martin:
/// - `ST_AsMVTGeom` bounds = **unbuffered** `ST_TileEnvelope($1,$2,$3)`;
///   buffer is the 4th arg, not the bounds.
/// - SRID-aware bbox_search: 4 branches for buffer=0, 3857+margin, 4326
///   (ST_Expand to dodge the antimeridian wrap), and the generic fallback.
/// - `id_column` is passed as the 5th `feature_id_name` arg of `ST_AsMVT`.
///
/// Departure from martin: no `ST_CurveToLine` wrapper. See
/// `build_base_select_no_where` for the reasoning.
///
/// Constants we don't yet expose as TableInfo fields: `extent=4096`,
/// `buffer=64`, `clip_geom=true`, no `LIMIT`. Add config plumbing later
/// if a workload needs to tune them.
///
/// Returns a SQL string parameterized on `(z,x,y)` as `$1,$2,$3` INT4.
/// Pair with `prepare_typed_cached(&[INT4, INT4, INT4])` for plan reuse.
fn build_inline_tile_query(
    info: &TableInfo,
    columns: &[(String, String)],
    supports_tile_margin: bool,
) -> String {
    let base = build_base_select_no_where(info, columns);
    let bbox = build_bbox_search(info, supports_tile_margin);
    let geom = pg_escape::quote_identifier(&info.geometry_column);
    format!("{base}\n  WHERE\n    {geom} && {bbox}\n) AS tile")
}

fn build_batch_tile_query(
    info: &TableInfo,
    columns: &[(String, String)],
    supports_tile_margin: bool,
) -> String {
    let coords = ["batch_coords.z", "batch_coords.x", "batch_coords.y"];
    let base = build_base_select_with_coordinates(info, columns, coords);
    let bbox = build_bbox_search_with_coordinates(info, supports_tile_margin, coords);
    let tile = compose_filtered_sql(&base, &bbox, info, "batch_coords.enabled", None);
    compose_batch_sql(&tile)
}

fn compose_batch_sql(tile: &str) -> String {
    // Dropping a row stream does not stop PostgreSQL from rendering its suffix.
    format!(
        "WITH RECURSIVE batch_coords AS MATERIALIZED (\n\
         SELECT * FROM unnest($1::integer[], $2::integer[], $3::integer[], $4::boolean[]) \
         WITH ORDINALITY AS coords(z,x,y,enabled,ordinality)),\n\
         batch_tiles AS (\n\
         SELECT batch_coords.ordinality, result.mvt, \
         COALESCE(octet_length(result.mvt), 0)::bigint AS total_bytes \
         FROM batch_coords CROSS JOIN LATERAL ({tile}) AS result(mvt) \
         WHERE batch_coords.ordinality = 1\n\
         UNION ALL\n\
         SELECT batch_coords.ordinality, result.mvt, \
         previous.total_bytes + COALESCE(octet_length(result.mvt), 0) \
         FROM batch_tiles AS previous JOIN batch_coords \
         ON batch_coords.ordinality = previous.ordinality + 1 \
         CROSS JOIN LATERAL ({tile}) AS result(mvt) \
         WHERE previous.total_bytes < {MAX_TILE_BATCH_BYTES})\n\
         SELECT batch_tiles.ordinality, batch_tiles.mvt FROM batch_tiles \
         ORDER BY batch_tiles.ordinality"
    )
}

/// Inner SELECT including FROM but no WHERE. Re-used by the filter
/// path to splice extra conditions before closing the subquery.
fn build_base_select_no_where(info: &TableInfo, columns: &[(String, String)]) -> String {
    build_base_select_with_coordinates(info, columns, ["$1::integer", "$2::integer", "$3::integer"])
}

fn build_base_select_with_coordinates(
    info: &TableInfo,
    columns: &[(String, String)],
    [z, x, y]: [&str; 3],
) -> String {
    const EXTENT: u32 = 4096;
    const BUFFER: u32 = 64;
    let geom = pg_escape::quote_identifier(&info.geometry_column);
    let schema = pg_escape::quote_identifier(&info.schema);
    let table = pg_escape::quote_identifier(&info.table);
    let layer_name = pg_escape::quote_literal(&info.table);

    let properties = if columns.is_empty() {
        String::new()
    } else {
        columns
            .iter()
            .map(|(c, _)| format!(", {}", pg_escape::quote_identifier(c)))
            .collect::<Vec<_>>()
            .join("")
    };

    let (id_name, id_field) = match info.id_column.as_deref() {
        Some(c) => {
            let ident = pg_escape::quote_identifier(c);
            let literal = pg_escape::quote_literal(c);
            (
                format!(", {literal}"),
                format!(", {ident}::bigint AS {ident}"),
            )
        }
        None => (String::new(), String::new()),
    };

    // No `ST_CurveToLine` wrapper: it's a no-op for the standard PostGIS
    // geometry types (Point/Line/Polygon/Multi*, with or without Z/M)
    // that this tile pipeline ever sees. martin emits the call defensively
    // for the rare CircularString / CompoundCurve / CurvePolygon cases —
    // we do not, since dropping it shaves a per-row function dispatch
    // (~5–10 % at high zoom). If a Curve* source ever shows up, validate
    // it at discovery (geometry_columns.type) and re-add the wrapper there.
    // Keep the physical table name from shadowing correlated batch coordinates.
    format!(
        "\
SELECT
  ST_AsMVT(tile, {layer_name}, {EXTENT}, 'geom'{id_name})
FROM (
  SELECT
    ST_AsMVTGeom(
        ST_Transform({geom}::geometry, 3857),
        ST_TileEnvelope({z}, {x}, {y}),
        {EXTENT}, {BUFFER}, true
    ) AS geom{id_field}{properties}
  FROM
    {schema}.{table} AS source_features",
    )
}

/// SRID-aware bbox-search expression; 4 branches matching martin
/// (query_tables.rs:206-218) byte-for-byte.
fn build_bbox_search(info: &TableInfo, supports_tile_margin: bool) -> String {
    build_bbox_search_with_coordinates(
        info,
        supports_tile_margin,
        ["$1::integer", "$2::integer", "$3::integer"],
    )
}

fn build_bbox_search_with_coordinates(
    info: &TableInfo,
    supports_tile_margin: bool,
    [z, x, y]: [&str; 3],
) -> String {
    const BUFFER: u32 = 64;
    const EXTENT: u32 = 4096;
    const MARGIN: f64 = BUFFER as f64 / EXTENT as f64;
    const EARTH_CIRCUMFERENCE_DEGREES: f64 = 360.0;
    let srid = info.srid;
    if BUFFER == 0 {
        format!("ST_Transform(ST_TileEnvelope({z}, {x}, {y}), {srid})")
    } else if supports_tile_margin && srid == 3857 {
        format!(
            "ST_Transform(ST_TileEnvelope({z}, {x}, {y}, \
             margin => {MARGIN}), {srid})"
        )
    } else if srid == 4326 {
        format!(
            "ST_Expand(ST_Transform(ST_TileEnvelope({z}, {x}, {y}), {srid}), \
             ({MARGIN} * {EARTH_CIRCUMFERENCE_DEGREES}) / 2^{z})"
        )
    } else {
        format!("ST_Transform(ST_TileEnvelope({z}, {x}, {y}), {srid})")
    }
}

/// Compose the per-request inline SQL for a filtered get_tile call.
/// `filter_sql` may be empty when only `?bbox=` is set; `user_bbox`
/// may be `None` when only `?filter=` is set.
fn compose_filtered_sql(
    base_select_no_where: &str,
    bbox_search: &str,
    info: &TableInfo,
    filter_sql: &str,
    user_bbox: Option<[f64; 4]>,
) -> String {
    let mut conds: Vec<String> = Vec::with_capacity(3);
    let geom_col = pg_escape::quote_identifier(&info.geometry_column);
    conds.push(format!("{geom_col} && {bbox_search}"));
    if let Some([w, s, e, n]) = user_bbox {
        // Test the geographic bounding box after the indexed tile predicate.
        // Projecting only the envelope corners can lose matches in curved CRSs.
        let geometry = if info.srid == 4326 {
            geom_col
        } else {
            format!("ST_Transform({geom_col}::geometry, 4326)").into()
        };
        conds.push(format!(
            "{geometry} && ST_MakeEnvelope({w}, {s}, {e}, {n}, 4326)"
        ));
    }
    if !filter_sql.trim().is_empty() {
        conds.push(format!("({filter_sql})"));
    }
    let where_clause = conds.join("\n    AND ");
    format!("{base_select_no_where}\n  WHERE\n    {where_clause}\n) AS tile")
}

#[cfg(test)]
mod inline_filter_tests {
    use super::*;

    fn fixture_info() -> TableInfo {
        TableInfo {
            schema: "vector".into(),
            table: "test".into(),
            geometry_column: "geom".into(),
            geometry_type: "POINT".into(),
            srid: 4326,
            id_column: None,
            properties: Vec::new(),
            bounds: None,
            has_spatial_index: true,
            derived_min_zoom: None,
        }
    }

    #[test]
    fn unfiltered_split_matches_legacy_one_shot_form() {
        let info = fixture_info();
        let sql = build_inline_tile_query(&info, &[], true);
        assert!(sql.contains("ST_AsMVT"));
        assert!(sql.contains(" && "));
        assert!(sql.ends_with(") AS tile"));
    }

    #[test]
    fn batch_sql_uses_ordered_arrays_without_rewriting_quoted_identifiers() {
        let mut info = fixture_info();
        info.geometry_column = "$1::integer".into();
        let sql = build_batch_tile_query(&info, &[("$2::integer".into(), "text".into())], true);
        assert!(sql.contains("unnest($1::integer[], $2::integer[], $3::integer[], $4::boolean[])"));
        assert!(sql.contains("WITH ORDINALITY"));
        assert!(sql.contains("ST_TileEnvelope(batch_coords.z, batch_coords.x, batch_coords.y)"));
        assert!(sql.contains("\"$1::integer\"::geometry"));
        assert!(sql.contains(", \"$2::integer\""));
        assert!(sql.ends_with("ORDER BY batch_tiles.ordinality"));
    }

    #[test]
    fn compose_filtered_includes_filter_and_bbox() {
        let info = fixture_info();
        let base = build_base_select_no_where(&info, &[]);
        let bbox = build_bbox_search(&info, true);
        let sql = compose_filtered_sql(
            &base,
            &bbox,
            &info,
            "(\"class\" = 'highway')",
            Some([10.0, 20.0, 30.0, 40.0]),
        );
        assert!(sql.contains("ST_AsMVT"));
        assert!(sql.contains("\"class\" = 'highway'"));
        assert!(sql.contains("ST_MakeEnvelope(10, 20, 30, 40, 4326)"));
        assert!(sql.ends_with(") AS tile"));
    }

    #[test]
    fn compose_with_only_filter_omits_user_bbox_clause() {
        let info = fixture_info();
        let base = build_base_select_no_where(&info, &[]);
        let bbox = build_bbox_search(&info, true);
        let sql = compose_filtered_sql(&base, &bbox, &info, "(\"class\" = 'highway')", None);
        assert!(sql.contains("\"class\" = 'highway'"));
        assert!(!sql.contains("ST_MakeEnvelope"));
        assert!(sql.contains(" && "));
        assert!(sql.ends_with(") AS tile"));
    }

    #[test]
    fn compose_with_only_bbox_omits_filter_clause() {
        let info = fixture_info();
        let base = build_base_select_no_where(&info, &[]);
        let bbox = build_bbox_search(&info, true);
        let sql = compose_filtered_sql(&base, &bbox, &info, "", Some([10.0, 20.0, 30.0, 40.0]));
        assert!(sql.contains("ST_MakeEnvelope"));
        // No "()" empty filter group.
        assert!(!sql.contains("AND ()"));
    }

    #[test]
    fn inline_sql_quotes_postgres_identifiers_and_literals() {
        let mut info = fixture_info();
        info.schema = r#"tenant"1"#.into();
        info.table = "select".into();
        info.geometry_column = r#"geom"col"#.into();
        info.id_column = Some(r#"id"col"#.into());
        let columns = vec![(r#"name"col"#.to_string(), "text".to_string())];

        let sql = build_inline_tile_query(&info, &columns, true);

        assert!(sql.contains(r#""tenant""1"."select""#));
        assert!(sql.contains(r#"ST_Transform("geom""col"::geometry, 3857)"#));
        assert!(sql.contains(r#""id""col"::bigint AS "id""col""#));
        assert!(sql.contains(r#""name""col""#));
        assert!(sql.contains(r#"ST_AsMVT(tile, 'select', 4096, 'geom', 'id"col')"#));
    }

    #[test]
    fn filtered_sql_quotes_geometry_identifier() {
        let base = "SELECT 1 FROM \"tenant\".\"table\"";
        let mut info = fixture_info();
        info.geometry_column = r#"geom"col"#.into();
        let sql = compose_filtered_sql(base, "bbox", &info, "", Some([10.0, 20.0, 30.0, 40.0]));

        assert!(sql.contains(r#""geom""col" && bbox"#));
        assert!(sql.contains(r#""geom""col" && ST_MakeEnvelope(10, 20, 30, 40, 4326)"#));
    }
}

async fn discover_property_types(
    pool: &PostgresPool,
    info: &TableInfo,
) -> MaptileResult<Vec<(String, String)>> {
    if info.properties.is_empty() {
        return Ok(Vec::new());
    }
    let conn = pool.get().await?;
    let rows = conn
        .query(
            "SELECT column_name::text, data_type::text FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2 AND column_name = ANY($3)
             ORDER BY ordinal_position",
            &[&info.schema, &info.table, &info.properties],
        )
        .await
        .map_err(MaptileError::backend)?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

async fn check_spatial_index(
    conn: &deadpool_postgres::Object,
    schema: &str,
    table: &str,
) -> MaptileResult<bool> {
    let row = conn
        .query_one(
            "SELECT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_index AS idx
                JOIN pg_catalog.pg_class AS tbl ON tbl.oid = idx.indrelid
                JOIN pg_catalog.pg_namespace AS ns ON ns.oid = tbl.relnamespace
                JOIN pg_catalog.pg_class AS ic ON ic.oid = idx.indexrelid
                JOIN pg_catalog.pg_am AS am ON am.oid = ic.relam
                WHERE ns.nspname = $1 AND tbl.relname = $2 AND am.amname = 'gist'
                  AND idx.indisvalid AND idx.indisready
             )",
            &[&schema, &table],
        )
        .await
        .map_err(MaptileError::backend)?;
    Ok(row.get(0))
}

async fn estimate_bounds(
    conn: &deadpool_postgres::Object,
    schema: &str,
    table: &str,
    geom_col: &str,
) -> Option<[f64; 4]> {
    let s = pg_escape::quote_identifier(schema);
    let t = pg_escape::quote_identifier(table);
    let g = pg_escape::quote_identifier(geom_col);
    let sql = format!(
        "SELECT ST_XMin(b), ST_YMin(b), ST_XMax(b), ST_YMax(b)
         FROM (SELECT ST_Extent(ST_Transform({g}, 4326))::geometry AS b FROM {s}.{t}) e
         WHERE b IS NOT NULL"
    );
    let row = conn.query_opt(&sql, &[]).await.ok().flatten()?;
    Some([row.get(0), row.get(1), row.get(2), row.get(3)])
}

#[cfg(test)]
mod field_type_tests {
    use super::pg_type_to_tilejson_field;

    #[test]
    fn integer_family_maps_to_number() {
        assert_eq!(pg_type_to_tilejson_field("integer"), "Number");
        assert_eq!(pg_type_to_tilejson_field("bigint"), "Number");
        assert_eq!(pg_type_to_tilejson_field("smallint"), "Number");
        assert_eq!(pg_type_to_tilejson_field("real"), "Number");
        assert_eq!(pg_type_to_tilejson_field("double precision"), "Number");
        assert_eq!(pg_type_to_tilejson_field("numeric"), "Number");
    }

    #[test]
    fn text_family_maps_to_string() {
        assert_eq!(pg_type_to_tilejson_field("text"), "String");
        assert_eq!(pg_type_to_tilejson_field("character varying"), "String");
        assert_eq!(pg_type_to_tilejson_field("character"), "String");
        assert_eq!(pg_type_to_tilejson_field("uuid"), "String");
    }

    #[test]
    fn boolean_maps_to_boolean() {
        assert_eq!(pg_type_to_tilejson_field("boolean"), "Boolean");
    }

    #[test]
    fn json_family_maps_to_string_per_tilejson_3_0() {
        // TileJSON 3.0 only allows String/Number/Boolean — JSON-typed columns
        // round-trip through MVT as opaque strings.
        assert_eq!(pg_type_to_tilejson_field("json"), "String");
        assert_eq!(pg_type_to_tilejson_field("jsonb"), "String");
    }

    #[test]
    fn unknown_falls_back_to_string() {
        assert_eq!(pg_type_to_tilejson_field("inet"), "String");
        assert_eq!(pg_type_to_tilejson_field("bytea"), "String");
        assert_eq!(pg_type_to_tilejson_field("date"), "String");
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(pg_type_to_tilejson_field("INTEGER"), "Number");
        assert_eq!(pg_type_to_tilejson_field("Boolean"), "Boolean");
    }
}

#[cfg(test)]
mod min_zoom_tests {
    use super::*;

    #[test]
    fn global_bounds_derive_zoom_zero() {
        assert_eq!(derive_min_zoom(&[-180.0, -90.0, 180.0, 90.0]), 0);
    }

    #[test]
    fn small_extent_derives_positive_zoom() {
        // 0.1° span: 4·360/(256·0.1) = 56.25 → ceil(log2) = 6.
        let z = derive_min_zoom(&[116.0, 39.0, 116.1, 39.1]);
        assert_eq!(z, 6);
        // At the derived zoom the dataset spans ≥ MIN_ZOOM_PIXELS px.
        let px = 0.1 / 360.0 * 256.0 * f64::from(1u32 << z);
        assert!(px >= MIN_ZOOM_PIXELS);
        // One zoom lower it would be sub-threshold.
        let px_below = px / 2.0;
        assert!(px_below < MIN_ZOOM_PIXELS);
    }

    #[test]
    fn degenerate_bounds_derive_zero() {
        assert_eq!(derive_min_zoom(&[10.0, 20.0, 10.0, 20.0]), 0);
        // All-NaN bounds: span is NaN → defensive zero.
        assert_eq!(derive_min_zoom(&[f64::NAN; 4]), 0);
        // One NaN axis: f64::max ignores it, the finite axis wins.
        assert_eq!(derive_min_zoom(&[f64::NAN, 0.0, 1.0, 1.0]), 3);
    }

    #[test]
    fn tiny_extent_clamps_to_22() {
        assert_eq!(derive_min_zoom(&[0.0, 0.0, 1e-12, 1e-12]), 22);
    }

    #[test]
    fn tilejson_minzoom_set_from_derived_zoom() {
        let cfg = PostgresTableConfig::new("pts", "public", "pts");
        let bounds = [116.0, 39.0, 116.1, 39.1];
        let tj = build_tilejson_with_vector_layers(
            &cfg,
            Some(bounds),
            Some(derive_min_zoom(&bounds)),
            &[],
        );
        assert_eq!(tj.minzoom, Some(6));
        assert_eq!(tj.vector_layers.as_ref().unwrap()[0].minzoom, Some(6));
    }

    #[test]
    fn tilejson_zoom_range_is_explicit_when_minimum_is_zero_or_unknown() {
        let cfg = PostgresTableConfig::new("pts", "public", "pts");
        let tj = build_tilejson_with_vector_layers(&cfg, None, Some(0), &[]);
        assert_eq!(tj.minzoom, Some(0));
        assert_eq!(tj.maxzoom, Some(22));
        let tj = build_tilejson_with_vector_layers(&cfg, None, None, &[]);
        assert_eq!(tj.minzoom, Some(0));
        assert_eq!(tj.maxzoom, Some(22));
    }
}

#[cfg(test)]
mod vector_layers_tests {
    use super::*;

    #[test]
    fn vector_layers_uses_source_id_and_typed_fields() {
        let cfg = PostgresTableConfig::new("roads", "public", "roads");
        let columns = vec![
            ("name".to_string(), "text".to_string()),
            ("highway".to_string(), "character varying".to_string()),
            ("lanes".to_string(), "integer".to_string()),
            ("oneway".to_string(), "boolean".to_string()),
        ];
        let tj = build_tilejson_with_vector_layers(&cfg, None, None, &columns);
        let layers = tj.vector_layers.as_ref().expect("vector_layers populated");
        assert_eq!(layers.len(), 1);
        let l = &layers[0];
        assert_eq!(l.id, "roads");
        assert_eq!(l.fields.get("name").map(String::as_str), Some("String"));
        assert_eq!(l.fields.get("highway").map(String::as_str), Some("String"));
        assert_eq!(l.fields.get("lanes").map(String::as_str), Some("Number"));
        assert_eq!(l.fields.get("oneway").map(String::as_str), Some("Boolean"));
    }

    #[test]
    fn empty_columns_still_produce_one_layer() {
        let cfg = PostgresTableConfig::new("points", "public", "points");
        let tj = build_tilejson_with_vector_layers(&cfg, None, None, &[]);
        let layers = tj.vector_layers.as_ref().unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].id, "points");
        assert!(layers[0].fields.is_empty());
    }

    #[test]
    fn description_propagates_to_layer() {
        let mut cfg = PostgresTableConfig::new("pts", "public", "pts");
        cfg.description = Some("hello".into());
        let tj = build_tilejson_with_vector_layers(&cfg, None, None, &[]);
        assert_eq!(tj.description.as_deref(), Some("hello"));
        assert_eq!(
            tj.vector_layers.as_ref().unwrap()[0].description.as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn bounds_are_set_when_provided() {
        let cfg = PostgresTableConfig::new("x", "public", "x");
        let tj = build_tilejson_with_vector_layers(&cfg, Some([-10.0, -5.0, 10.0, 5.0]), None, &[]);
        let b = tj.bounds.unwrap();
        assert!((b.left - -10.0).abs() < 1e-9);
        assert!((b.bottom - -5.0).abs() < 1e-9);
        assert!((b.right - 10.0).abs() < 1e-9);
        assert!((b.top - 5.0).abs() < 1e-9);
    }
}
