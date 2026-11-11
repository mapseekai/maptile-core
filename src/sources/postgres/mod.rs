//! PostgreSQL/PostGIS tile sources.
//!
//! Phase 2b-1 ships:
//! - [`PostgresPool`] — a `deadpool-postgres` wrapper with PostGIS version probing
//!   and optional rustls TLS.
//! - [`PostgresFunctionSource`] — a [`crate::sources::TileSource`] that calls a
//!   user-defined SQL function returning MVT bytea.
//!
//! Table-driven sources land in 2b-2; out-db raster in 2b-3.

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
