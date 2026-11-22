use std::sync::Arc;

use super::super::geoparquet::GeoParquetSourceConfig;
use super::super::literal;
use super::bounds::RADIUS;
use super::resolver::{InputKind, ResolvedGeometry};

pub(super) struct SpatialQuery {
    pub(super) sql: Arc<str>,
    pub(super) bounds_sql: Arc<str>,
    pub(super) filter_at: usize,
    pub(super) projected: Arc<str>,
}

pub(super) fn build_sql(
    cfg: &GeoParquetSourceConfig,
    resolved: &ResolvedGeometry,
    input: InputKind,
) -> SpatialQuery {
    let ResolvedGeometry {
        from,
        projected: geom,
        geom_alias,
        id_arg,
        properties,
        fields,
        covering,
        ..
    } = resolved;
    let srid = resolved.srid;
    let flatgeobuf = matches!(input, InputKind::FlatGeobuf);

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
            // PROJ can wrap the buffered corners at the antimeridian. Analytic
            // inverse bounds preserve world-edge and crossing geometries.
            [
                format!("greatest(-180.0, ST_XMin({buffered}) / {RADIUS} * 180 / pi())"),
                format!("degrees(atan(sinh(ST_YMin({buffered}) / {RADIUS})))"),
                format!("least(180.0, ST_XMax({buffered}) / {RADIUS} * 180 / pi())"),
                format!("degrees(atan(sinh(ST_YMax({buffered}) / {RADIUS})))"),
            ]
        };
        let overlap = [(0, "<=", 2), (1, "<=", 3), (2, ">=", 0), (3, ">=", 1)]
            .map(|(i, op, j)| format!("{} {op} {}", paths[i], bounds[j]))
            .join(" AND ");
        // World-edge tiles can intersect pieces across the wrap. Use
        // exact projected geometry whenever the buffered bounds reach it.
        filters.push(if srid == 4326 {
            format!("(ST_XMin({buffered}) <= -{RADIUS} * pi() OR ST_XMax({buffered}) >= {RADIUS} * pi() OR ({overlap}))")
        } else {
            overlap
        });
    }
    let intersects = format!("ST_Intersects({geom}, {buffered})");
    // shortcut: Spatial 1.5.6 corrupts empty FGB index batches and prunes projected chords; scan until a verified extension upgrade.
    filters.push(if flatgeobuf {
        format!("coalesce({intersects}, false)")
    } else {
        intersects
    });
    let properties = if properties.is_empty() {
        String::new()
    } else {
        format!(", {}", properties.join(", "))
    };
    let fields = fields
        .keys()
        .map(|name| {
            format!(
                " + coalesce(octet_length(encode(\"{}\"::VARCHAR)), 0)",
                name.replace('"', "\"\"")
            )
        })
        .collect::<String>();
    let input_limit =
        cfg.query_options.memory_limit_mb * 1024 * 1024 / cfg.query_options.pool_size / 2;
    let limit = cfg.query_options.max_features;
    let blob_limit = cfg.query_options.max_tile_bytes;
    let head = format!(
        "WITH tile AS MATERIALIZED (
           SELECT ST_AsMVTGeom({geom}, ST_Extent({tile}), {}::BIGINT, {}::BIGINT, true)
                  AS {geom_alias}{properties}
           FROM {from} WHERE {}",
        cfg.extent,
        cfg.buffer,
        filters.join(" AND ")
    );
    let filter_at = head.len();
    let sql = format!(
        "{head} LIMIT {}
         ), stats AS (
           SELECT count(*) AS feature_count,
                  coalesce(sum(32 + coalesce(octet_length(ST_AsWKB({geom_alias})), 0){fields}), 0) AS input_bytes
           FROM tile
         ), encoded AS MATERIALIZED (
           SELECT ST_AsMVT(tile, {}, {}, '{geom_alias}'{id_arg}) AS data
           FROM tile
           WHERE (SELECT feature_count <= {limit} AND input_bytes <= {input_limit} FROM stats)
         )
         SELECT CASE WHEN octet_length(data) <= {blob_limit} THEN data END,
                CASE WHEN feature_count > {limit} THEN feature_count
                     WHEN input_bytes > {input_limit} THEN -2
                     WHEN octet_length(data) > {blob_limit} THEN -1
                     ELSE feature_count END
         FROM stats, encoded",
        limit + 1,
        literal(&cfg.layer_name),
        cfg.extent,
    );
    SpatialQuery {
        sql: sql.into(),
        bounds_sql: format!("SELECT {geom} AS geom FROM {from}").into(),
        filter_at,
        projected: geom.as_str().into(),
    }
}
