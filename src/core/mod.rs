//! Tile values shared by catalog, cache, sources and the public read stack.
pub(crate) mod response;
pub mod tile_math;
pub(crate) mod tile_types;

pub type SourceId = std::sync::Arc<str>;

/// Logical tile edge shared by render requests and their WMTS matrix set.
pub(crate) const RENDER_TILE_SIZE: u32 = 512;
