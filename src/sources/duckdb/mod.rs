//! DuckDB Spatial sources and their shared connection and query support.

mod geometry;
pub mod geoparquet;
pub mod local_vector;
mod pool;
mod predicates;
mod query;
mod remote;

use crate::error::{MaptileError, MaptileResult};

fn identifier(name: &str) -> MaptileResult<String> {
    if name.is_empty() || name.contains('\0') {
        return Err(config("column names must be nonempty and contain no NUL"));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn config(message: impl Into<String>) -> MaptileError {
    MaptileError::Config(format!("geoparquet: {}", message.into()))
}
