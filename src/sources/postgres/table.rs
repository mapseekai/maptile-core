//! PostgreSQL table source for dynamic MVT generation.
//!
//! [`PostgresTableSource::open`] discovers geometry, SRID, properties, and bounds,
//! then builds SQL templates for individual tiles and batches. Tile requests
//! prepare the fixed SQL on the borrowed connection with `prepare_typed_cached`,
//! allowing subsequent requests on that connection to reuse the statement.
//!
//! CQL2 attribute filters use a caller-supplied column allowlist, and geographic
//! bounding boxes add request-specific predicates. Filtered queries prepare their
//! SQL for that request. Tile caching and concurrent miss coalescing belong to
//! [`crate::sources::manager::SourceManager`].

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use tilejson::{TileJSON, tilejson};
use tokio_postgres::types::{ToSql, Type};

use crate::cache::CacheZoomRange;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    MAX_TILE_BATCH_BYTES, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource, validate_source_request,
};
use crate::transport::MAX_DECODED_TILE_BYTES;

use super::pool::PostgresPool;

const DEFAULT_EXTENT: u32 = 4096;
const BUFFER: u32 = 64;
const MAX_ZOOM: u8 = 22;
const MAX_TABLE_CANDIDATES: usize = 100_000;
const MAX_TABLE_INPUT_BYTES: usize = 4 * MAX_DECODED_TILE_BYTES;
const SINGLE_COORDS: [&str; 3] = ["$1::integer", "$2::integer", "$3::integer"];
const BATCH_COORDS: [&str; 3] = ["batch_coords.z", "batch_coords.x", "batch_coords.y"];

#[derive(Debug, Clone)]
pub struct PostgresTableConfig {
    pub id: String,
    pub schema: String,
    pub table: String,
    pub description: Option<String>,
    pub bounds: Option<[f64; 4]>,
    /// Minimum served zoom. By default the source derives it from its bounds.
    pub minzoom: Option<u8>,
    /// Positive MVT grid extent. Defaults to 4096.
    pub extent: u32,

    pub geometry_column: Option<String>,
    pub id_column: Option<String>,
    pub properties: Option<Vec<String>>,

    /// Cache policy when a durable edit revision is supplied.
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
            minzoom: None,
            extent: DEFAULT_EXTENT,
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
    /// `None` when bounds are unknown. The source's explicit minzoom can override this.
    pub derived_min_zoom: Option<u8>,
    pub extent: u32,
}

pub struct PostgresTableSource {
    cfg: PostgresTableConfig,
    info: TableInfo,
    pool: Arc<PostgresPool>,
    tj: TileJSON,
    /// Inline `SELECT ST_AsMVT(...)` SQL for ordinary tile requests.
    /// The query structure follows Martin and tileserver-rs table sources;
    /// each pooled connection prepares and reuses the statement on demand.
    inline_query: String,
    batch_query: String,
    single_sql: TileSql,
    batch_sql: TileSql,
    /// Columns that CQL2 filters may reference. Callers populate this set
    /// through [`Self::with_allowed_columns`] before sharing the source.
    allowed_columns: Arc<std::collections::HashSet<String>>,
    info_meta: TileInfo,
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
        super::pool::validate_source_window(cfg.minzoom, Some(MAX_ZOOM), cfg.bounds)?;
        if !(1..=i32::MAX as u32 - 2 * BUFFER).contains(&cfg.extent) {
            return Err(MaptileError::Config("invalid PostgreSQL MVT extent".into()));
        }
        let info = discover_table(&pool, &cfg).await?;
        let columns = discover_property_types(&pool, &info).await?;

        let tj = build_tilejson_with_vector_layers(
            &cfg,
            cfg.bounds.or(info.bounds),
            cfg.minzoom.or(info.derived_min_zoom),
            &columns,
        );

        let single_sql = TileSql::new(&info, &columns, pool.supports_tile_margin(), SINGLE_COORDS);
        let batch_sql = TileSql::new(&info, &columns, pool.supports_tile_margin(), BATCH_COORDS);
        let inline_query = single_sql.build(&info, "", None);
        let batch_query = compose_batch_sql(&batch_sql.build(&info, "batch_coords.enabled", None));
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
            single_sql,
            batch_sql,
            allowed_columns: Arc::new(std::collections::HashSet::new()),
            info_meta,
            definition_version,
            content_version: crate::cache::ContentVersion::new_generation(),
        })
    }

    fn filter_sql(&self, query: Option<&TileQuery<'_>>) -> MaptileResult<String> {
        let sql = match query.and_then(|q| q.filter) {
            Some(expr) => {
                let allowed: std::collections::HashSet<&str> =
                    self.allowed_columns.iter().map(String::as_str).collect();
                // Preserve the structured FilterError so the calling
                // application can classify the request failure.
                crate::filter::validate(expr, &allowed)?;
                crate::filter::to_sql_where(expr)?
            }
            None => String::new(),
        };
        Ok(sql)
    }

    fn empty_tile(&self) -> TileResponse {
        TileResponse {
            data: Bytes::new(),
            info: self.info_meta,
            last_modified: None,
        }
    }

    fn zoom_enabled(&self, zoom: u8) -> bool {
        self.tj.minzoom.is_none_or(|min| zoom >= min)
    }

    /// Set the columns that CQL2 attribute filters may reference.
    /// Populate the allowlist with the database columns the application exposes
    /// for filtering. Each property in a filter must occur in this set.
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
}

fn tile_response(
    row: &tokio_postgres::Row,
    size_column: usize,
    info: TileInfo,
) -> MaptileResult<TileResponse> {
    let size: Option<i32> = row.try_get(size_column).map_err(MaptileError::backend)?;
    let size = size.ok_or_else(|| {
        MaptileError::Resource(
            "PostGIS tile exceeds the candidate count or input byte limit".into(),
        )
    })?;
    if size as usize > MAX_DECODED_TILE_BYTES {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_DECODED_TILE_BYTES,
        });
    }
    let data: Vec<u8> = row
        .try_get(size_column + 1)
        .map_err(MaptileError::backend)?;
    Ok(TileResponse {
        data: Bytes::from(data),
        info,
        last_modified: None,
    })
}

#[async_trait]
impl TileSource for PostgresTableSource {
    fn source_kind(&self) -> crate::sources::SourceKind {
        crate::sources::SourceKind::PostgresTable
    }
    fn supports_bbox(&self) -> bool {
        true
    }
    fn strict_mvt(&self) -> bool {
        true
    }
    fn native_batch(&self) -> bool {
        true
    }

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

    fn cache_zoom(&self) -> CacheZoomRange {
        if self.tj.other.contains_key("mapseek_revision") {
            self.cfg.cache_zoom.unwrap_or_default()
        } else {
            CacheZoomRange::disabled()
        }
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(PostgresTableSource {
            cfg: self.cfg.clone(),
            info: self.info.clone(),
            pool: self.pool.clone(),
            tj: self.tj.clone(),
            inline_query: self.inline_query.clone(),
            batch_query: self.batch_query.clone(),
            single_sql: self.single_sql.clone(),
            batch_sql: self.batch_sql.clone(),
            allowed_columns: self.allowed_columns.clone(),
            info_meta: self.info_meta,
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
        validate_source_request(self, &[xyz], query)?;
        let filter = self.filter_sql(query)?;
        if !self.zoom_enabled(xyz.z) {
            return Ok(self.empty_tile());
        }

        let bbox = query.and_then(|q| q.bbox);
        let pending = self.pool.get_query().await?;
        let conn = pending.connection();
        let types = [Type::INT4; 3];
        let stmt = if filter.is_empty() && bbox.is_none() {
            conn.prepare_typed_cached(&self.inline_query, &types).await
        } else {
            let sql = self.single_sql.build(&self.info, &filter, bbox);
            conn.prepare_typed(&sql, &types).await
        }
        .map_err(MaptileError::backend)?;
        let row = conn
            .query_one(
                &stmt,
                &[&i32::from(xyz.z), &(xyz.x as i32), &(xyz.y as i32)],
            )
            .await
            .map_err(MaptileError::backend)?;
        pending.finish().await?;
        tile_response(&row, 0, self.info_meta)
    }

    async fn read_tile_batch(
        &self,
        coords: &[TileCoord],
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<Vec<TileResponse>> {
        validate_source_request(self, coords, query)?;
        let filter = self.filter_sql(query)?;
        let enabled: Vec<_> = coords
            .iter()
            .map(|coord| self.zoom_enabled(coord.z))
            .collect();
        if !enabled.contains(&true) {
            return Ok(vec![self.empty_tile(); coords.len()]);
        }
        let bbox = query.and_then(|q| q.bbox);
        let dynamic_sql = if filter.is_empty() && bbox.is_none() {
            None
        } else {
            let filter = if filter.is_empty() {
                "batch_coords.enabled".into()
            } else {
                format!("batch_coords.enabled AND ({filter})")
            };
            let tile = self.batch_sql.build(&self.info, &filter, bbox);
            Some(compose_batch_sql(&tile))
        };
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
                let tile = tile_response(&row, 1, self.info_meta)?;
                bytes += tile.data.len();
                tiles.push(tile);
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
    pool: &Arc<PostgresPool>,
    cfg: &PostgresTableConfig,
) -> MaptileResult<TableInfo> {
    let pending = pool.get_query().await?;
    let conn = pending.connection();

    let geometry_column = match cfg.geometry_column.clone() {
        Some(c) => c,
        None => find_geometry_column(conn, &cfg.schema, &cfg.table).await?,
    };

    let (srid, geometry_type) =
        get_geometry_info(conn, &cfg.schema, &cfg.table, &geometry_column).await?;
    if srid <= 0 {
        return Err(MaptileError::Config(format!(
            "table {}.{} must declare a positive SRID",
            cfg.schema, cfg.table,
        )));
    }
    if cfg.id_column.as_ref() == Some(&geometry_column) {
        return Err(MaptileError::Config(
            "the PostGIS geometry and feature ID columns must differ".into(),
        ));
    }

    let properties = match cfg.properties.clone() {
        Some(p) => p,
        None => discover_properties(conn, &cfg.schema, &cfg.table, &geometry_column).await?,
    };

    let has_spatial_index = check_spatial_index(conn, &cfg.schema, &cfg.table).await?;
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
        None => estimate_bounds(conn, &cfg.schema, &cfg.table, &geometry_column).await,
    };
    pending.finish().await?;

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
        extent: cfg.extent,
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
        z.clamp(0.0, f64::from(MAX_ZOOM)) as u8
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
    // Publish an explicit zoom range for catalog consumers, using the
    // bounds-derived minimum when available.
    tj.minzoom = Some(min_zoom.unwrap_or(0));
    tj.maxzoom = Some(MAX_ZOOM);
    let fields: std::collections::BTreeMap<String, String> = columns
        .iter()
        .map(|(name, ty)| (name.clone(), pg_type_to_tilejson_field(ty).to_string()))
        .collect();
    // Match the layer name passed to ST_AsMVT by both query builders.
    // Clients use this TileJSON layer ID to select the encoded MVT layer.
    let mut layer = tilejson::VectorLayer::new(cfg.table.clone(), fields);
    layer.description = cfg.description.clone();
    layer.minzoom = tj.minzoom;
    layer.maxzoom = tj.maxzoom;
    tj.vector_layers = Some(vec![layer]);
    tj
}

fn compose_batch_sql(tile: &str) -> String {
    // Dropping a row stream does not stop PostgreSQL from rendering its suffix.
    format!(
        "WITH RECURSIVE batch_coords AS (
         SELECT * FROM unnest($1::integer[], $2::integer[], $3::integer[], $4::boolean[])
         WITH ORDINALITY AS coords(z,x,y,enabled,ordinality)),
         batch_tiles AS (
         SELECT batch_coords.ordinality, result.size, result.mvt,
         COALESCE(result.size, {MAX_TILE_BATCH_BYTES})::bigint AS total_bytes
         FROM batch_coords CROSS JOIN LATERAL ({tile}) AS result
         WHERE batch_coords.ordinality = 1
         UNION ALL
         SELECT batch_coords.ordinality, result.size, result.mvt,
         previous.total_bytes + COALESCE(result.size, {MAX_TILE_BATCH_BYTES})
         FROM batch_tiles AS previous JOIN batch_coords
         ON batch_coords.ordinality = previous.ordinality + 1
         CROSS JOIN LATERAL ({tile}) AS result
         WHERE previous.total_bytes < {MAX_TILE_BATCH_BYTES})
         SELECT batch_tiles.ordinality, batch_tiles.size, batch_tiles.mvt FROM batch_tiles
         ORDER BY batch_tiles.ordinality"
    )
}

#[derive(Clone)]
struct TileSql {
    candidates: String,
    bbox_search: String,
    output: String,
}

impl TileSql {
    fn new(
        info: &TableInfo,
        columns: &[(String, String)],
        supports_tile_margin: bool,
        coords: [&str; 3],
    ) -> Self {
        let [z, x, y] = coords;
        let geom = pg_escape::quote_identifier(&info.geometry_column);
        let geom_name = pg_escape::quote_literal(&info.geometry_column);
        let schema = pg_escape::quote_identifier(&info.schema);
        let table = pg_escape::quote_identifier(&info.table);
        let layer = pg_escape::quote_literal(&info.table);
        let extent = info.extent;

        let mut projected = String::new();
        let mut input_bytes = format!("COALESCE(ST_MemSize({geom}), 0)::bigint");
        let id_name = info.id_column.as_deref().map_or_else(String::new, |name| {
            let ident = pg_escape::quote_identifier(name);
            projected.push_str(&format!(", {ident}::bigint AS {ident}"));
            input_bytes.push_str(" + 8");
            format!(", {}", pg_escape::quote_literal(name))
        });
        for (column, data_type) in columns {
            let ident = pg_escape::quote_identifier(column);
            projected.push_str(&format!(", {ident}"));
            let size = match data_type.as_str() {
                "text" | "character varying" | "character" | "json" | "jsonb" => {
                    format!("octet_length({ident}::text)")
                }
                _ => format!("pg_column_size({ident})"),
            };
            input_bytes.push_str(&format!(" + COALESCE({size}, 0)"));
        }

        let candidates =
            format!("SELECT {geom}{projected} FROM {schema}.{table} AS source_features");
        // The CASE gates aggregation on preflight. OFFSET 0 keeps its result
        // from being inlined and aggregated again for each use of data.
        let output = format!(
            "SELECT octet_length(data) AS size,
                    CASE WHEN octet_length(data) <= {MAX_DECODED_TILE_BYTES} THEN data END AS mvt
             FROM (
                 SELECT CASE WHEN NOT EXISTS (
                     SELECT 1 FROM candidates
                     HAVING count(*) > {MAX_TABLE_CANDIDATES}
                         OR sum({input_bytes}) > {MAX_TABLE_INPUT_BYTES}
                 ) THEN (
                     SELECT COALESCE(ST_AsMVT(tile.*, {layer}, {extent}, {geom_name}{id_name}), ''::bytea)
                     FROM (
                         SELECT ST_AsMVTGeom(
                             ST_Transform({geom}::geometry, 3857),
                             ST_TileEnvelope({z}, {x}, {y}),
                             {extent}, {BUFFER}, true
                         ) AS {geom}{projected} FROM candidates
                     ) AS tile
                 ) END AS data OFFSET 0
             ) AS payload"
        );

        Self {
            candidates,
            bbox_search: build_bbox_search(info, supports_tile_margin, coords),
            output,
        }
    }

    fn build(&self, info: &TableInfo, filter: &str, user_bbox: Option<[f64; 4]>) -> String {
        let geom = pg_escape::quote_identifier(&info.geometry_column);
        let mut predicates = format!("{geom} && {}", self.bbox_search);
        if let Some([w, s, e, n]) = user_bbox {
            // Keep the WGS84 envelope in its own CRS after indexed tile pruning.
            let geometry = if info.srid == 4326 {
                geom
            } else {
                format!("ST_Transform({geom}::geometry, 4326)").into()
            };
            predicates.push_str(&format!(
                " AND {geometry} && ST_MakeEnvelope({w}, {s}, {e}, {n}, 4326)"
            ));
        }
        if !filter.is_empty() {
            predicates.push_str(&format!(" AND ({filter})"));
        }
        format!(
            "WITH candidates AS ({} WHERE {predicates} LIMIT {})
             {}",
            self.candidates,
            MAX_TABLE_CANDIDATES + 1,
            self.output,
        )
    }
}

fn build_bbox_search(info: &TableInfo, supports_tile_margin: bool, [z, x, y]: [&str; 3]) -> String {
    let margin = f64::from(BUFFER) / f64::from(info.extent);
    let srid = info.srid;
    if supports_tile_margin && srid == 3857 {
        format!("ST_TileEnvelope({z}, {x}, {y}, margin => {margin})")
    } else if srid == 4326 {
        // Expanding after transforming avoids wrap-around at the antimeridian.
        format!(
            "ST_Expand(ST_Transform(ST_TileEnvelope({z}, {x}, {y}), {srid}),
             ({margin} * 360.0) / 2^{z})"
        )
    } else {
        format!(
            "ST_Transform(ST_Expand(ST_TileEnvelope({z}, {x}, {y}),
             ({margin} * 40075016.68557849) / 2^{z}), {srid})"
        )
    }
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
            extent: DEFAULT_EXTENT,
        }
    }

    #[test]
    fn unfiltered_sql_includes_spatial_predicate() {
        let info = fixture_info();
        let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(&info, "", None);
        assert!(sql.contains("ST_AsMVT"));
        assert!(sql.contains(" && "));
    }

    #[test]
    fn batch_sql_uses_ordered_arrays_without_rewriting_quoted_identifiers() {
        let mut info = fixture_info();
        info.geometry_column = "$1::integer".into();
        let template = TileSql::new(
            &info,
            &[("$2::integer".into(), "text".into())],
            true,
            BATCH_COORDS,
        );
        let sql = compose_batch_sql(&template.build(&info, "batch_coords.enabled", None));
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
        let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(
            &info,
            "(\"class\" = 'highway')",
            Some([10.0, 20.0, 30.0, 40.0]),
        );
        assert!(sql.contains("ST_AsMVT"));
        assert!(sql.contains("\"class\" = 'highway'"));
        assert!(sql.contains("ST_MakeEnvelope(10, 20, 30, 40, 4326)"));
    }

    #[test]
    fn compose_with_only_filter_omits_user_bbox_clause() {
        let info = fixture_info();
        let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(
            &info,
            "(\"class\" = 'highway')",
            None,
        );
        assert!(sql.contains("\"class\" = 'highway'"));
        assert!(!sql.contains("ST_MakeEnvelope"));
        assert!(sql.contains(" && "));
    }

    #[test]
    fn compose_with_only_bbox_omits_filter_clause() {
        let info = fixture_info();
        let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(
            &info,
            "",
            Some([10.0, 20.0, 30.0, 40.0]),
        );
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

        let sql = TileSql::new(&info, &columns, true, SINGLE_COORDS).build(&info, "", None);

        assert!(sql.contains(r#""tenant""1"."select""#));
        assert!(sql.contains(r#"ST_Transform("geom""col"::geometry, 3857)"#));
        assert!(sql.contains(r#""id""col"::bigint AS "id""col""#));
        assert!(sql.contains(r#""name""col""#));
        assert!(sql.contains(r#"ST_AsMVT(tile.*, 'select', 4096, 'geom"col', 'id"col')"#));
    }

    #[test]
    fn filtered_sql_quotes_geometry_identifier() {
        let mut info = fixture_info();
        info.geometry_column = r#"geom"col"#.into();
        let sql = TileSql::new(&info, &[], true, SINGLE_COORDS).build(
            &info,
            "",
            Some([10.0, 20.0, 30.0, 40.0]),
        );

        assert!(sql.contains(r#""geom""col" && ST_Expand"#));
        assert!(sql.contains(r#""geom""col" && ST_MakeEnvelope(10, 20, 30, 40, 4326)"#));
    }
}

async fn discover_property_types(
    pool: &Arc<PostgresPool>,
    info: &TableInfo,
) -> MaptileResult<Vec<(String, String)>> {
    if info.properties.is_empty() {
        return Ok(Vec::new());
    }
    let pending = pool.get_query().await?;
    let rows = pending
        .connection()
        .query(
            "SELECT column_name::text, data_type::text FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2 AND column_name = ANY($3)
             ORDER BY ordinal_position",
            &[&info.schema, &info.table, &info.properties],
        )
        .await
        .map_err(MaptileError::backend)?;
    pending.finish().await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let name: String = row.get(0);
            (Some(&name) != info.id_column.as_ref() && name != info.geometry_column)
                .then(|| (name, row.get(1)))
        })
        .collect())
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
