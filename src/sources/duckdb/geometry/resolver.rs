use std::collections::{BTreeMap, HashSet};

use duckdb::Connection;
use serde_json::Value;

use super::super::geoparquet::GeoParquetSourceConfig;
use super::super::local_vector::vector_metadata;
use super::super::{config, identifier, literal};
use super::bounds::metadata_bounds;
use super::unused_name;
use crate::error::{MaptileError, MaptileResult};

#[derive(Clone, Copy)]
pub(super) enum InputKind {
    GeoParquet,
    FlatGeobuf,
}

pub(super) struct ResolvedGeometry {
    pub(super) from: String,
    pub(super) projected: String,
    pub(super) geom_alias: String,
    pub(super) id_arg: String,
    pub(super) properties: Vec<String>,
    pub(super) fields: BTreeMap<String, String>,
    pub(super) covering: Option<[String; 4]>,
    pub(super) srid: i32,
    pub(super) bounds: Option<[f64; 4]>,
}

pub(super) fn resolve_input(
    conn: &Connection,
    cfg: &GeoParquetSourceConfig,
    location: &str,
    input: InputKind,
) -> MaptileResult<ResolvedGeometry> {
    let path_sql = literal(location);
    // These operations read footer/schema metadata, not feature rows. The
    // same path works for local files and bounded remote range requests.
    let geo = match input {
        InputKind::GeoParquet => conn
            .prepare(&format!(
                "SELECT value FROM parquet_kv_metadata({path_sql}) WHERE key = 'geo'::BLOB"
            ))
            .and_then(|mut statement| {
                let mut rows = statement.query([])?;
                rows.next()?.map(|row| row.get::<_, Vec<u8>>(0)).transpose()
            })
            .map_err(MaptileError::backend)?
            .map(|value| serde_json::from_slice::<Value>(&value))
            .transpose()
            .map_err(|e| config(format!("invalid geo metadata: {e}")))?,
        InputKind::FlatGeobuf => None,
    };
    let (geometry, vector_srid, from) = match input {
        InputKind::GeoParquet => (
            cfg.geometry_column
                .clone()
                .or_else(|| {
                    geo.as_ref()?
                        .get("primary_column")?
                        .as_str()
                        .map(str::to_owned)
                })
                .ok_or_else(|| config("no primary geometry column; set geometry_column"))?,
            None,
            format!("read_parquet({path_sql})"),
        ),
        InputKind::FlatGeobuf => {
            let metadata = vector_metadata(conn, location, cfg.srid)?;
            (
                metadata.geometry,
                Some(metadata.srid),
                format!("ST_Read({path_sql}, allowed_drivers := ['FlatGeobuf'])"),
            )
        }
    };
    let geometry = geometry.as_str();
    let column = geo
        .as_ref()
        .map(|geo| {
            geo.get("columns")
                .and_then(|c| c.get(geometry))
                .ok_or_else(|| config("geometry column is not declared in geo metadata"))
        })
        .transpose()?;
    if let Some(column) = column {
        if column.get("encoding").and_then(Value::as_str) != Some("WKB") {
            return Err(config("only GeoParquet WKB encoding is supported"));
        }
        if column
            .get("edges")
            .and_then(Value::as_str)
            .is_some_and(|v| v != "planar")
        {
            return Err(config("spherical GeoParquet edges are not supported"));
        }
    }
    let srid = resolve_srid(vector_srid.or(cfg.srid), column)?;
    let covering = if cfg.trust_covering && matches!(srid, 4326 | 3857) {
        column.and_then(|c| covering_paths(c, conn, &from))
    } else {
        None
    };
    let bounds = cfg.bounds.or_else(|| metadata_bounds(column, srid));
    let columns: BTreeMap<String, String> = conn
        .prepare(&format!("DESCRIBE SELECT * FROM {from}"))
        .and_then(|mut s| {
            s.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()
        })
        .map_err(MaptileError::backend)?;
    let geom = identifier(geometry)?;
    let geom = match columns.get(geometry).map(String::as_str) {
        Some("BLOB") => format!("ST_GeomFromWKB({geom})"),
        Some(t) if t.starts_with("GEOMETRY") => geom,
        _ => return Err(config("geometry column must be WKB or GEOMETRY")),
    };
    // Explicit always_xy is required: GeoParquet uses x/y, even when EPSG
    // defines latitude-first axes. Never encode degrees against an MVT grid.
    let projected = format!("ST_Transform({geom}, 'EPSG:{srid}', 'EPSG:3857', always_xy := true)");
    // Validate the CRS even for empty datasets, without scanning data.
    conn.query_row(&format!(
        "SELECT ST_AsText(ST_Transform(ST_Point(0,0), 'EPSG:{srid}', 'EPSG:3857', always_xy := true))"
    ), [], |r| r.get::<_, String>(0)).map_err(|e| config(format!("unsupported CRS: {e}")))?;
    let selected: HashSet<&str> = cfg
        .properties
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();
    if cfg.properties.is_some() && selected.iter().any(|p| !columns.contains_key(*p)) {
        return Err(config("requested property column is absent"));
    }
    let mut fields = BTreeMap::new();
    let mut properties = Vec::new();
    for (name, ty) in &columns {
        if name == geometry
            || cfg.id_column.as_ref() == Some(name)
            || (cfg.properties.is_some() && !selected.contains(name.as_str()))
        {
            continue;
        }
        if let Some((cast, field)) = property_type(ty) {
            let quoted = identifier(name)?;
            properties.push(format!("{quoted}::{cast} AS {quoted}"));
            fields.insert(name.clone(), field.to_owned());
        } else if cfg.properties.is_some() {
            return Err(config(format!(
                "property {name:?} has no MVT representation"
            )));
        } else {
            tracing::debug!(target: "maptile_core::sources::geoparquet_geometry", column = %name, data_type = %ty, "GeoParquet property omitted from MVT");
        }
    }
    let geom_alias = unused_name("__maptile_geom", &columns);
    let id_alias = unused_name("__maptile_id", &columns);
    let id_arg = if let Some(id) = &cfg.id_column {
        let ty = columns
            .get(id)
            .ok_or_else(|| config("id column is absent"))?;
        if !matches!(
            ty.as_str(),
            "TINYINT"
                | "SMALLINT"
                | "INTEGER"
                | "BIGINT"
                | "UTINYINT"
                | "USMALLINT"
                | "UINTEGER"
                | "UBIGINT"
        ) {
            return Err(config("id column must be an integer"));
        }
        let quoted = identifier(id)?;
        // Negative IDs have no MVT representation. Avoid wrapping them.
        properties.push(format!(
            "CASE WHEN {quoted} >= 0 THEN TRY_CAST({quoted} AS BIGINT) END AS {id_alias}"
        ));
        format!(", {}", literal(&id_alias))
    } else {
        String::new()
    };

    Ok(ResolvedGeometry {
        from,
        projected,
        geom_alias,
        id_arg,
        properties,
        fields,
        covering,
        srid,
        bounds,
    })
}

pub(super) fn resolve_srid(explicit: Option<i32>, column: Option<&Value>) -> MaptileResult<i32> {
    if let Some(srid) = explicit {
        return if srid > 0 {
            Ok(srid)
        } else {
            Err(config("srid must be positive"))
        };
    }
    let Some(crs) = column.and_then(|c| c.get("crs")) else {
        // GeoParquet's absent CRS is OGC:CRS84. Legacy explicitly selected
        // raw-WKB Parquet keeps the original WGS84 default as well.
        return Ok(4326);
    };
    if crs.is_null() {
        return Err(config("GeoParquet CRS is explicitly unknown; set srid"));
    }
    let id = crs
        .get("id")
        .ok_or_else(|| config("CRS has no authority; set srid"))?;
    let authority = id
        .get("authority")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = id
        .get("code")
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    if authority.eq_ignore_ascii_case("OGC") && code.eq_ignore_ascii_case("CRS84") {
        return Ok(4326);
    }
    if authority.eq_ignore_ascii_case("EPSG")
        && let Ok(srid) = code.parse::<i32>()
        && srid > 0
    {
        return Ok(srid);
    }
    Err(config(
        "unsupported GeoParquet CRS authority; set an EPSG srid",
    ))
}

fn covering_paths(column: &Value, conn: &Connection, from: &str) -> Option<[String; 4]> {
    let bbox = column.get("covering")?.get("bbox")?;
    let mut paths = Vec::new();
    for axis in ["xmin", "ymin", "xmax", "ymax"] {
        let parts = bbox.get(axis)?.as_array()?;
        if parts.is_empty() || parts.len() > 16 {
            return None;
        }
        let path = parts
            .iter()
            .map(|part| identifier(part.as_str()?).ok())
            .collect::<Option<Vec<_>>>()?
            .join(".");
        let ty: String = conn
            .query_row(&format!("DESCRIBE SELECT {path} FROM {from}"), [], |row| {
                row.get(1)
            })
            .ok()?;
        if !matches!(property_type(&ty), Some((_, "Number"))) {
            return None;
        }
        paths.push(path);
    }
    paths.try_into().ok()
}

fn property_type(ty: &str) -> Option<(&'static str, &'static str)> {
    match ty {
        "BOOLEAN" => Some(("BOOLEAN", "Boolean")),
        "TINYINT" | "SMALLINT" | "INTEGER" | "UTINYINT" | "USMALLINT" => {
            Some(("INTEGER", "Number"))
        }
        "BIGINT" | "UINTEGER" => Some(("BIGINT", "Number")),
        "FLOAT" | "DOUBLE" => Some(("DOUBLE", "Number")),
        "VARCHAR"
        | "UBIGINT"
        | "HUGEINT"
        | "UHUGEINT"
        | "DATE"
        | "TIME"
        | "TIME WITH TIME ZONE"
        | "TIMESTAMP"
        | "TIMESTAMP WITH TIME ZONE"
        | "TIMESTAMP_S"
        | "TIMESTAMP_MS"
        | "TIMESTAMP_NS"
        | "INTERVAL"
        | "UUID"
        | "JSON" => Some(("VARCHAR", "String")),
        t if t.starts_with("DECIMAL(") => Some(("DOUBLE", "Number")),
        t if t.starts_with("ENUM(") => Some(("VARCHAR", "String")),
        _ => None,
    }
}
