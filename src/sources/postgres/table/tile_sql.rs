use super::{BUFFER, TableInfo};
use crate::sources::MAX_TILE_BATCH_BYTES;
use crate::transport::MAX_DECODED_TILE_BYTES;

const MAX_TABLE_CANDIDATES: usize = 100_000;
const MAX_TABLE_INPUT_BYTES: usize = 4 * MAX_DECODED_TILE_BYTES;
pub(super) const SINGLE_COORDS: [&str; 3] = ["$1::integer", "$2::integer", "$3::integer"];
pub(super) const BATCH_COORDS: [&str; 3] = ["batch_coords.z", "batch_coords.x", "batch_coords.y"];

pub(super) fn compose_batch_sql(tile: &str) -> String {
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
pub(super) struct TileSql {
    candidates: String,
    bbox_search: String,
    output: String,
}

impl TileSql {
    pub(super) fn new(
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

    pub(super) fn build(
        &self,
        info: &TableInfo,
        filter: &str,
        user_bbox: Option<[f64; 4]>,
    ) -> String {
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
mod tests;
