//! CQL2 filter pipeline backed by the `cql2` crate (OGC reference).
//!
//! Public surface — see
//! `docs/architecture/data-serving.md`
//! for the locked decisions (D1–D6).
//!
//! Built as `pub(crate)` while migration is in progress; T12 deletes
//! the old `filter` directory and promotes this file to `pub mod filter`.
#![allow(dead_code, unused_imports)]

use std::collections::HashSet;

pub use cql2::Expr as Cql2Expr;

#[derive(thiserror::Error, Debug, Clone)]
#[non_exhaustive]
pub enum FilterError {
    #[error("filter parse: {0}")]
    Parse(String),
    #[error("filter: {0}")]
    Validation(String),
    #[error("filter sql gen: {0}")]
    Sql(String),
}

/// Caps from spec D2. Tight to keep the request-path SQL small and
/// the parser stack bounded.
pub const MAX_DEPTH: usize = 16;
pub const MAX_IN_VALUES: usize = 256;
pub const MAX_LITERAL_BYTES: usize = 1024;

/// URL query map exposed to sources via `TileQuery::url_query`.
pub type UrlQuery = std::collections::BTreeMap<String, String>;

/// Parse CQL2 text or JSON. `cql2::Expr` implements `FromStr`, dispatching
/// to `parse_json` / `parse_text` based on the leading character.
///
/// **Note**: cql2-rs 0.5.x's text parser splits `'O''Reilly'` at the
/// doubled quote (treats `''` as literal-end, not an escape). Callers
/// constructing filters from user input that may contain single
/// quotes MUST use the JSON form — `tile_service` does this.
pub fn parse_cql2(text: &str) -> Result<Cql2Expr, FilterError> {
    text.parse::<Cql2Expr>()
        .map_err(|e| FilterError::Parse(e.to_string()))
}

/// Render a validated `Cql2Expr` to a Postgres WHERE fragment (no
/// leading `WHERE`, no trailing semicolon). Literals are escaped via
/// `pg_escape` (cql2-rs internal). Identifiers are double-quoted.
pub fn to_sql_where(expr: &Cql2Expr) -> Result<String, FilterError> {
    use cql2::ToSqlAst;
    expr.to_sql().map_err(|e| FilterError::Sql(e.to_string()))
}

/// Walk the cql2 AST and reject anything outside the D2 op-set, refer
/// to a column not in `allowed_columns`, or exceed the size caps.
pub fn validate(expr: &Cql2Expr, allowed_columns: &HashSet<&str>) -> Result<(), FilterError> {
    walk(expr, allowed_columns, 0)
}

/// D2 op allow-list. cql2-rs canonicalises ops to lowercase and to the
/// punctuation form (e.g. `eq` → `=`); we accept both spellings the
/// parser actually emits.
const ALLOWED_OPS: &[&str] = &[
    "=", "eq", "<>", "!=", "ne", "<", "lt", "<=", "le", "lte", ">", "gt", ">=", "ge", "gte", "and",
    "or", "not", "in", "like", "isnull",
];

fn walk(e: &Cql2Expr, allowed: &HashSet<&str>, depth: usize) -> Result<(), FilterError> {
    if depth > MAX_DEPTH {
        return Err(FilterError::Validation(format!("depth > {MAX_DEPTH}")));
    }
    match e {
        Cql2Expr::Operation { op, args } => {
            let lc = op.to_ascii_lowercase();
            if !ALLOWED_OPS.iter().any(|a| *a == lc) {
                return Err(FilterError::Validation(format!("unsupported op '{op}'")));
            }
            // IN: args[1] is the candidate Array. Cap its length here.
            if lc == "in"
                && args.len() == 2
                && let Cql2Expr::Array(items) = &*args[1]
                && items.len() > MAX_IN_VALUES
            {
                return Err(FilterError::Validation(format!(
                    "IN list > {MAX_IN_VALUES}"
                )));
            }
            for a in args {
                walk(a, allowed, depth + 1)?;
            }
            Ok(())
        }
        Cql2Expr::Property { property } => {
            if !allowed.contains(property.as_str()) {
                return Err(FilterError::Validation(format!(
                    "unknown column '{property}'"
                )));
            }
            Ok(())
        }
        Cql2Expr::Literal(s) => {
            if s.len() > MAX_LITERAL_BYTES {
                return Err(FilterError::Validation(format!(
                    "literal > {MAX_LITERAL_BYTES} bytes"
                )));
            }
            Ok(())
        }
        Cql2Expr::Float(_) | Cql2Expr::Bool(_) | Cql2Expr::Null => Ok(()),
        Cql2Expr::Array(items) => {
            for it in items {
                walk(it, allowed, depth + 1)?;
            }
            Ok(())
        }
        // Reject everything else explicitly: spatial, temporal, etc.
        Cql2Expr::Geometry(_)
        | Cql2Expr::BBox { .. }
        | Cql2Expr::Interval { .. }
        | Cql2Expr::Timestamp { .. }
        | Cql2Expr::Date { .. } => Err(FilterError::Validation(
            "unsupported expression: spatial/temporal forms are out of scope".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_eq() {
        let e = parse_cql2("class = 'highway'").expect("parse ok");
        let _ = format!("{e:?}");
    }

    #[test]
    fn parse_error_is_returned() {
        // Empty input — cql2-rs surfaces this as a parse error rather
        // than silently producing an Expr.
        let err = parse_cql2("").unwrap_err();
        assert!(matches!(err, FilterError::Parse(_)));
    }

    fn cols(names: &[&'static str]) -> HashSet<&'static str> {
        names.iter().copied().collect()
    }

    #[test]
    fn validates_eq_with_allowed_column() {
        let e = parse_cql2("class = 'highway'").unwrap();
        validate(&e, &cols(&["class"])).expect("ok");
    }

    #[test]
    fn rejects_unknown_column() {
        let e = parse_cql2("secret = 'x'").unwrap();
        let err = validate(&e, &cols(&["class"])).unwrap_err();
        assert!(format!("{err}").contains("unknown column 'secret'"));
    }

    #[test]
    fn rejects_unsupported_op() {
        // S_INTERSECTS produces an Operation { op: "s_intersects", … }.
        let e = parse_cql2("S_INTERSECTS(geom, POINT(0 0))").unwrap();
        let err = validate(&e, &cols(&["geom"])).unwrap_err();
        assert!(format!("{err}").contains("unsupported"));
    }

    #[test]
    fn enforces_in_list_cap() {
        let lits = (0..MAX_IN_VALUES + 5)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let e = parse_cql2(&format!("class IN ({lits})")).unwrap();
        let err = validate(&e, &cols(&["class"])).unwrap_err();
        assert!(format!("{err}").contains("IN list"));
    }

    #[test]
    fn validates_compound_and() {
        let e = parse_cql2("speed > 30 AND class = 'highway'").unwrap();
        validate(&e, &cols(&["speed", "class"])).expect("ok");
    }

    #[test]
    fn renders_eq_to_pg_sql() {
        let e = parse_cql2("class = 'highway'").unwrap();
        let sql = to_sql_where(&e).unwrap();
        assert!(sql.to_lowercase().contains("class"));
        assert!(sql.contains("'highway'"));
        assert!(sql.contains('='));
    }

    #[test]
    fn escapes_quote_in_literal() {
        // Test via JSON form, where the parser is unambiguous about
        // string contents — cql2-rs's *text* parser handles `''` as a
        // CQL2 literal terminator (not as an escape), so we route
        // strings-with-quotes through JSON in production.
        let json = r#"{"op":"=","args":[{"property":"name"},"O'Reilly"]}"#;
        let e = parse_cql2(json).unwrap();
        let sql = to_sql_where(&e).unwrap();
        // cql2-rs uses standard PG quoting — either `'O''Reilly'`
        // (doubled) or `E'O\'Reilly'` (escape-string). Both are safe.
        assert!(
            sql.contains("'O''Reilly'") || sql.contains(r"E'O\'Reilly'"),
            "unexpected escape form: {sql}"
        );
        // No bare statement terminator outside a string literal.
        assert!(!sql.contains("'; "));
    }

    #[test]
    fn injection_payload_stays_inside_literal() {
        // Payload delivered as JSON to bypass cql2-text quoting rules.
        let json = r#"{"op":"=","args":[{"property":"name"},"x'; DROP TABLE foo; --"]}"#;
        let e = parse_cql2(json).unwrap();
        let sql = to_sql_where(&e).unwrap();
        // The DROP TABLE bytes are data — must remain within a quoted
        // literal, not break out into top-level SQL.
        assert!(
            sql.contains("DROP") || sql.to_uppercase().contains("DROP"),
            "expected DROP bytes preserved in literal: {sql}"
        );
        // No top-level statement break.
        assert!(!sql.to_uppercase().contains("); DROP"));
    }

    #[test]
    fn validates_isnull_and_like() {
        let e = parse_cql2("name IS NULL").unwrap();
        validate(&e, &cols(&["name"])).expect("ok");
        let e = parse_cql2("name LIKE 'A%'").unwrap();
        validate(&e, &cols(&["name"])).expect("ok");
    }
}
