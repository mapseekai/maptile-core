use std::sync::Arc;
use tilejson::{TileJSON, tilejson};

use super::{MAX_ZOOM, PostgresTableConfig, TableInfo};
use crate::error::{MaptileError, MaptileResult};
use crate::sources::postgres::pool::PostgresPool;

pub(super) async fn discover_table(
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
            target: "maptile_core::sources::postgres::table",
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
pub(super) fn build_tilejson_with_vector_layers(
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

pub(super) async fn discover_property_types(
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
mod tests;
