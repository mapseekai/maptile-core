//! Unified error type for maptile-core.
//!
//! `MaptileError` is `#[non_exhaustive]` so adding variants is not a
//! breaking change for downstream `match` arms (which must therefore
//! include a `_` arm). It is also `Clone` — the cache layer needs
//! `Clone` to fan a single source-error out to multiple concurrent
//! cache-miss waiters without losing the structured variant. Variants
//! that wrap non-`Clone` payloads use `Arc<…>`.

use std::sync::Arc;

/// Protocol-independent classification for retry, admission and adapter decisions.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    NotFound,
    InvalidInput,
    Unsupported,
    VersionChanged,
    ResourceLimit,
    Saturated,
    Deadline,
    Cancelled,
    Unavailable,
    Backend,
}

#[derive(thiserror::Error, Debug, Clone)]
#[non_exhaustive]
pub enum MaptileError {
    #[error("source not found: {0}")]
    SourceNotFound(String),

    #[error("style not found: {0}")]
    StyleNotFound(String),

    /// A local style file failure with its original structured cause.
    #[error("style {id} at {}: {source}", path.display())]
    StyleFile {
        id: String,
        path: std::path::PathBuf,
        #[source]
        source: Arc<MaptileError>,
    },

    #[error("source temporarily unavailable: {0}")]
    SourceUnavailable(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("unsupported operation: {0}")]
    Unsupported(String),

    #[error("source version changed: {0}")]
    VersionChanged(String),

    #[error("tile read deadline elapsed")]
    Deadline,

    #[error("tile read cancelled")]
    Cancelled,

    #[error("tile payload exceeds {limit} bytes")]
    PayloadTooLarge { limit: usize },

    #[error("processing capacity exhausted")]
    CapacityExhausted,

    #[error("invalid metadata: {0}")]
    InvalidMetadata(String),

    #[error("tile not found: z={0} x={1} y={2}")]
    TileNotFound(u8, u32, u32),

    /// Wraps backend-specific errors (postgres, pmtiles, mbtiles, GDAL,
    /// duckdb, …) without pulling them into the public enum. `Arc` so
    /// `MaptileError` stays `Clone`.
    #[error("backend: {0}")]
    Backend(#[source] Arc<dyn std::error::Error + Send + Sync>),

    /// Backend failure where only a formatted message is available
    /// (GDAL string errors, task-join failures, …). Prefer
    /// [`MaptileError::backend`] when the structured error is at hand.
    #[error("backend: {0}")]
    BackendMsg(String),

    #[error("filter: {0}")]
    Filter(#[from] crate::filter::FilterError),

    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[source] Arc<std::io::Error>),

    /// JSON (de)serialization failure crossing a public boundary.
    #[error("json: {0}")]
    Json(#[source] Arc<serde_json::Error>),

    #[error("render: {0}")]
    Render(String),

    #[error("style: {0}")]
    Style(String),

    #[error("resource: {0}")]
    Resource(String),

    #[error("mosaic: {0}")]
    Mosaic(String),

    #[error("transcode: {0}")]
    Transcode(String),

    #[error("invalid raster style: {0}")]
    InvalidStyle(String),
}

impl MaptileError {
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::SourceNotFound(_) | Self::StyleNotFound(_) | Self::TileNotFound(..) => {
                ErrorKind::NotFound
            }
            Self::StyleFile { source, .. } => source.kind(),
            Self::SourceUnavailable(_) => ErrorKind::Unavailable,
            Self::Unsupported(_) => ErrorKind::Unsupported,
            Self::VersionChanged(_) => ErrorKind::VersionChanged,
            Self::Deadline => ErrorKind::Deadline,
            Self::Cancelled => ErrorKind::Cancelled,
            Self::CapacityExhausted => ErrorKind::Saturated,
            Self::PayloadTooLarge { .. } | Self::Resource(_) => ErrorKind::ResourceLimit,
            Self::Config(_) | Self::InvalidRequest(_) | Self::Filter(_) | Self::InvalidStyle(_) => {
                ErrorKind::InvalidInput
            }
            Self::Io(error) => match error.kind() {
                std::io::ErrorKind::TimedOut => ErrorKind::Deadline,
                std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::NetworkUnreachable => ErrorKind::Unavailable,
                _ => ErrorKind::Backend,
            },
            Self::Backend(_)
            | Self::BackendMsg(_)
            | Self::InvalidMetadata(_)
            | Self::Json(_)
            | Self::Render(_)
            | Self::Style(_)
            | Self::Mosaic(_)
            | Self::Transcode(_) => ErrorKind::Backend,
        }
    }

    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind(),
            ErrorKind::Unavailable | ErrorKind::Saturated | ErrorKind::Deadline
        )
    }

    /// Wrap a backend error (postgres, pmtiles, GDAL, duckdb, …)
    /// preserving it as the structured `source` of [`MaptileError::Backend`].
    pub fn backend(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend(Arc::new(e))
    }
}

impl From<raster_style::Error> for MaptileError {
    fn from(e: raster_style::Error) -> Self {
        MaptileError::InvalidStyle(e.to_string())
    }
}

impl From<std::io::Error> for MaptileError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(Arc::new(e))
    }
}

impl From<serde_json::Error> for MaptileError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(Arc::new(e))
    }
}

pub type MaptileResult<T> = Result<T, MaptileError>;
