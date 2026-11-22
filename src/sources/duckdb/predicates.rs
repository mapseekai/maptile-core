use std::collections::BTreeMap;

use duckdb::types::Value;

use super::{geometry::valid_bounds, identifier};
use crate::error::{MaptileError, MaptileResult};
use crate::filter::{self, Cql2Expr};
use crate::sources::{TileCoord, TileQuery};

pub(super) struct TilePredicates {
    pub sql: String,
    pub params: Vec<(String, Value)>,
}

impl TilePredicates {
    pub fn build(
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
        fields: &BTreeMap<String, String>,
        projected: &str,
    ) -> MaptileResult<Self> {
        let mut predicates = Self {
            sql: String::new(),
            params: vec![
                ("z".into(), Value::Int(i32::from(xyz.z))),
                ("x".into(), Value::BigInt(i64::from(xyz.x))),
                ("y".into(), Value::BigInt(i64::from(xyz.y))),
            ],
        };
        let Some(query) = query else {
            return Ok(predicates);
        };
        if query.url_query.is_some()
            || query.style.is_some()
            || query.style_hash.is_some()
            || query.pixel_selection.is_some()
        {
            return Err(MaptileError::Unsupported(
                "DuckDB tile request options".into(),
            ));
        }
        if let Some(bounds) = query.bbox {
            if !valid_bounds(bounds) {
                return Err(MaptileError::InvalidRequest(
                    "expected ordered WGS84 bbox".into(),
                ));
            }
            let [w, s, e, n] = bounds.map(|v| predicates.bind(Value::Double(v)));
            predicates.sql = format!(
                " AND coalesce(ST_Intersects({projected}, ST_Transform(
                    ST_MakeEnvelope({w}::DOUBLE, {s}::DOUBLE, {e}::DOUBLE, {n}::DOUBLE),
                    'OGC:CRS84', 'EPSG:3857', always_xy := true)), false)"
            );
        }
        if let Some(expr) = query.filter {
            filter::validate(expr, &fields.keys().map(String::as_str).collect())?;
            let sql = predicates.expression(expr, &mut 1024)?;
            predicates.sql.push_str(&format!(" AND ({sql})"));
        }
        Ok(predicates)
    }

    fn bind(&mut self, value: Value) -> String {
        let name = format!("filter{}", self.params.len());
        let sql = format!("${name}");
        self.params.push((name, value));
        sql
    }

    fn expression(&mut self, expr: &Cql2Expr, budget: &mut usize) -> MaptileResult<String> {
        *budget = budget
            .checked_sub(1)
            .ok_or(MaptileError::CapacityExhausted)?;
        Ok(match expr {
            Cql2Expr::Property { property } => identifier(property)?,
            Cql2Expr::Literal(value) => self.bind(Value::Text(value.clone())),
            Cql2Expr::Float(value) if value.is_finite() => self.bind(Value::Double(*value)),
            Cql2Expr::Bool(value) => self.bind(Value::Boolean(*value)),
            Cql2Expr::Null => self.bind(Value::Null),
            Cql2Expr::Operation { op, args } => {
                let op = op.to_ascii_lowercase();
                match (op.as_str(), args.as_slice()) {
                    ("not" | "isnull", [expr]) => {
                        let expr = self.expression(expr, budget)?;
                        if op == "not" {
                            format!("(NOT {expr})")
                        } else {
                            format!("({expr} IS NULL)")
                        }
                    }
                    ("and" | "or", args) if args.len() >= 2 => {
                        let parts = args
                            .iter()
                            .map(|arg| self.expression(arg, budget))
                            .collect::<MaptileResult<Vec<_>>>()?;
                        format!(
                            "({})",
                            parts.join(if op == "and" { " AND " } else { " OR " })
                        )
                    }
                    ("in", [left, right]) => {
                        let Cql2Expr::Array(values) = right.as_ref() else {
                            return Err(unsupported());
                        };
                        if values.is_empty() {
                            return Ok("false".into());
                        }
                        let left = self.expression(left, budget)?;
                        let values = values
                            .iter()
                            .map(|expr| self.expression(expr, budget))
                            .collect::<MaptileResult<Vec<_>>>()?;
                        format!("({left} IN ({}))", values.join(", "))
                    }
                    (op, [left, right]) => {
                        let operator = match op {
                            "=" | "eq" => "=",
                            "<>" | "!=" | "ne" => "<>",
                            "<" | "lt" => "<",
                            "<=" | "le" | "lte" => "<=",
                            ">" | "gt" => ">",
                            ">=" | "ge" | "gte" => ">=",
                            "like" => "LIKE",
                            _ => return Err(unsupported()),
                        };
                        let left = self.expression(left, budget)?;
                        let right = self.expression(right, budget)?;
                        format!("({left} {operator} {right})")
                    }
                    _ => return Err(unsupported()),
                }
            }
            _ => return Err(unsupported()),
        })
    }
}

fn unsupported() -> MaptileError {
    MaptileError::Unsupported("DuckDB CQL2 expression".into())
}
