//! Versioned source catalogs and transactional publication.

mod catalog;
mod composition;
mod driver;
mod lifecycle;

pub use crate::core::SourceId;
pub use crate::service::engine::CatalogService;
pub(crate) use catalog::{CatalogBuildLease, CatalogEntry};
pub use catalog::{CatalogSnapshot, CatalogUpdate, DiscoveredDataset, Inventory, SourceCatalog};
pub use composition::{CompositeDeclaration, CompositeLimits, DuplicateLayer, MemberFailurePolicy};
pub use driver::{SourceCapabilities, SourceDescriptor, TileDriver, open_blocking};
pub(crate) use lifecycle::SourceReadLease;
#[cfg(any(feature = "agtiff", feature = "mbtiles"))]
pub(crate) use lifecycle::cleanup_runtime;
