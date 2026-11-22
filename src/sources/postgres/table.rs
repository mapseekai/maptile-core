//! PostgreSQL table source for dynamic MVT generation.
//!
//! [`PostgresTableSource::open`] discovers geometry, SRID, properties, and bounds,
//! then builds SQL templates for individual tiles and batches. Tile requests
//! prepare the fixed SQL on the borrowed connection with `prepare_typed_cached`,
//! allowing subsequent requests on that connection to reuse the statement.
//!
//! CQL2 attribute filters use a caller-supplied column allowlist, and geographic
//! bounding boxes add request-specific predicates. Filtered queries prepare their
//! SQL for that request. With a durable edit revision, [`crate::control::CatalogService`]
//! can cache tiles and coalesce concurrent misses for that revision.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use tilejson::TileJSON;
use tokio_postgres::types::{ToSql, Type};

use crate::cache::CacheZoomRange;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::{
    MAX_TILE_BATCH_BYTES, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse,
    TileSource, request::validate_source_request,
};
use crate::transport::MAX_DECODED_TILE_BYTES;

mod metadata;
mod tile_sql;

use super::pool::PostgresPool;
use metadata::{build_tilejson_with_vector_layers, discover_property_types, discover_table};
use tile_sql::{BATCH_COORDS, SINGLE_COORDS, TileSql, compose_batch_sql};

const DEFAULT_EXTENT: u32 = 4096;
const BUFFER: u32 = 64;
const MAX_ZOOM: u8 = 22;

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

#[derive(Clone)]
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
