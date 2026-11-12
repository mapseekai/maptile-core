//! PostgreSQL/PostGIS sources for dynamic vector and raster tiles.
//!
//! - [`PostgresPool`] manages database connections, version probing, and rustls TLS.
//! - [`PostgresTableSource`] generates MVT from a spatial table and supports
//!   CQL2 attribute filters and geographic bounding boxes.
//! - [`PostgresFunctionSource`] calls a user-defined SQL function returning MVT
//!   `bytea`, with optional JSON or JSONB request parameters.
//! - With the `raster` feature, `PostgresOutDbRasterSource` reads out-db raster
//!   references and generates image tiles through GDAL.
//!
//! Configure connections with [`PoolSettings`] and share the pool across sources.
//! Each source implements [`crate::sources::TileSource`] for registration with
//! [`crate::sources::manager::SourceManager`].

mod function;
mod pool;
mod table;

#[cfg(feature = "raster")]
mod outdb;
#[cfg(feature = "raster")]
mod outdb_types;

pub use function::{PostgresFunctionConfig, PostgresFunctionSource};
pub use pool::{PoolSettings, PostgresPool};
pub use table::{PostgresTableConfig, PostgresTableSource, TableInfo};

#[cfg(feature = "raster")]
pub use outdb::PostgresOutDbRasterSource;
#[cfg(feature = "raster")]
pub use outdb_types::PostgresOutDbRasterConfig;

use semver::Version;

/// Minimum PostgreSQL version required (11.0.0).
pub const MINIMUM_POSTGRES_VERSION: Version = Version::new(11, 0, 0);

/// Minimum PostGIS version required (3.0.0) for ST_TileEnvelope support.
pub const MINIMUM_POSTGIS_VERSION: Version = Version::new(3, 0, 0);

/// PostGIS version that supports the `margin` parameter in ST_TileEnvelope (3.1.0).
pub const ST_TILE_ENVELOPE_MARGIN_VERSION: Version = Version::new(3, 1, 0);
