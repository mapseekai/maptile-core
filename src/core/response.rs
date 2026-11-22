//! Tile payloads, conditional responses and results of reading a source or composite.
use super::{SourceId, tile_types::TileInfo};
use crate::error::MaptileError;

#[derive(Debug, Clone)]
pub struct TileResponse {
    pub data: bytes::Bytes,
    pub info: TileInfo,
    pub last_modified: Option<std::time::SystemTime>,
}

#[derive(Debug)]
pub enum TileEtagResponse {
    Fresh { resp: TileResponse, etag: String },
    NotModified,
}

#[derive(Debug, Clone)]
pub struct MemberFailure {
    pub source: SourceId,
    pub error: MaptileError,
}

#[derive(Debug, Clone)]
pub enum TileOutcome {
    NoTile,
    Present(TileResponse),
    Partial {
        tile: Option<TileResponse>,
        failures: Vec<MemberFailure>,
    },
}

impl TileOutcome {
    pub fn tile(&self) -> Option<&TileResponse> {
        match self {
            Self::NoTile => None,
            Self::Present(tile) => Some(tile),
            Self::Partial { tile, .. } => tile.as_ref(),
        }
    }
    pub fn is_partial(&self) -> bool {
        matches!(self, Self::Partial { .. })
    }
}
