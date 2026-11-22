use duckdb::Connection;
use serde_json::Value;

pub(super) const RADIUS: f64 = 6_378_137.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BoundsMode {
    #[default]
    Skip,
    Quick,
    Calc,
}

pub(super) fn scan_bounds(conn: &Connection, from: &str, mode: BoundsMode) -> Option<[f64; 4]> {
    let scan = |limit| {
        let sql = format!(
            "SELECT ST_XMin(b), ST_YMin(b), ST_XMax(b), ST_YMax(b)
             FROM (SELECT ST_Extent_Agg(ST_Transform(geom, 'EPSG:3857', 'OGC:CRS84', always_xy := true)) AS b
                   FROM ({from}{limit}))"
        );
        let bounds = conn
            .query_row(&sql, [], |r| {
                Ok([r.get::<_, f64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?])
            })
            .ok()?;
        display_bounds_wgs84(bounds)
    };
    match mode {
        BoundsMode::Skip => None,
        BoundsMode::Quick => scan(" LIMIT 4096").or_else(|| scan("")),
        BoundsMode::Calc => scan(""),
    }
}

pub(super) fn metadata_bounds(column: Option<&Value>, srid: i32) -> Option<[f64; 4]> {
    let bbox = column?.get("bbox")?.as_array()?;
    let indices = match bbox.len() {
        4 => [0, 1, 2, 3],
        6 => [0, 1, 3, 4],
        _ => return None,
    };
    let mut bounds = [0.0; 4];
    for (out, i) in bounds.iter_mut().zip(indices) {
        *out = bbox[i].as_f64()?;
    }
    if srid == 3857 {
        bounds = [
            bounds[0] / RADIUS * 180.0 / std::f64::consts::PI,
            inverse_y(bounds[1]),
            bounds[2] / RADIUS * 180.0 / std::f64::consts::PI,
            inverse_y(bounds[3]),
        ];
    } else if srid != 4326 {
        return None;
    }
    display_bounds_wgs84(bounds)
}

fn display_bounds_wgs84(mut bounds: [f64; 4]) -> Option<[f64; 4]> {
    if !valid_bounds(bounds) {
        return None;
    }
    for (min, max, limit) in [(0, 2, 180.0), (1, 3, 90.0)] {
        if bounds[min] == bounds[max] {
            bounds[min] = (bounds[min] - 1e-6).max(-limit);
            bounds[max] = (bounds[max] + 1e-6).min(limit);
        }
    }
    Some(bounds)
}

pub(in crate::sources::duckdb) fn valid_bounds(b: [f64; 4]) -> bool {
    b.iter().all(|n| n.is_finite())
        && b[0] >= -180.0
        && b[2] <= 180.0
        && b[1] >= -90.0
        && b[3] <= 90.0
        && b[0] <= b[2]
        && b[1] <= b[3]
}

fn inverse_y(y: f64) -> f64 {
    (y / RADIUS).sinh().atan().to_degrees()
}
