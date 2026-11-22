use std::sync::Arc;

use super::{
    ReadTarget, TileStack, bounded,
    engine::{check_output, render_identity},
};
use crate::{
    cache::ContentVersion,
    control::{CatalogEntry, CatalogSnapshot},
    core::response::TileOutcome,
    error::{MaptileError, MaptileResult},
    sources::{MAX_TILE_BATCH_BYTES, MAX_TILE_BATCH_COORDS, TileCoord, validate_tile_request},
};

impl TileStack {
    pub async fn read_batch(
        &self,
        target: ReadTarget<'_>,
        coords: &[TileCoord],
    ) -> MaptileResult<BatchPage> {
        self.session(target, false)?.read_page(coords).await
    }

    /// Pin the entry and every transitive source across pages. Mutable drivers must opt in.
    pub fn begin_export<'a>(&self, target: ReadTarget<'a>) -> MaptileResult<ExportSession<'a>> {
        self.session(target, true)
    }

    /// Resume a caller-owned, stable coordinate sequence after a process or request boundary.
    /// The caller authenticates every request and retains the same layer configuration and order.
    pub async fn read_export_page(
        &self,
        target: ReadTarget<'_>,
        coords: &[TileCoord],
        cursor: Option<&ExportCursor>,
    ) -> MaptileResult<(BatchPage, Option<ExportCursor>)> {
        let session = self.begin_export(target)?;
        let descriptor = session.entry.descriptor();
        let (_, render) = render_identity(descriptor, target.query);
        let mut hash = blake3::Hasher::new();
        hash.update(descriptor.revision.as_bytes());
        hash.update(render.as_bytes());
        serde_json::to_writer(
            &mut hash,
            &(
                descriptor.id.as_ref(),
                target.scope,
                target.query.and_then(|q| q.filter),
                target.query.and_then(|q| q.url_query),
                target.query.and_then(|q| q.bbox),
            ),
        )
        .map_err(MaptileError::backend)?;
        let revision = *hash.finalize().as_bytes();
        if cursor.is_some_and(|c| c.revision != revision) {
            return Err(MaptileError::VersionChanged(descriptor.id.to_string()));
        }
        let start = cursor.map_or(0, |c| c.position);
        let rest = coords.get(start..).ok_or_else(|| {
            MaptileError::InvalidRequest("export cursor exceeds coordinate sequence".into())
        })?;
        let page = session
            .read_page(&rest[..rest.len().min(MAX_TILE_BATCH_COORDS)])
            .await?;
        let position = start + page.consumed;
        Ok((
            page,
            (position < coords.len()).then_some(ExportCursor { revision, position }),
        ))
    }

    fn session<'a>(
        &self,
        target: ReadTarget<'a>,
        consistent: bool,
    ) -> MaptileResult<ExportSession<'a>> {
        validate_tile_request(&[], target.query)?;
        let snapshot = self.engine.catalog().snapshot();
        let entry = snapshot.entry(target.source, self.engine.limits())?;
        let descriptor = entry.descriptor();
        descriptor.validate_query(target.query)?;
        if consistent && !descriptor.capabilities.consistent_export {
            return Err(MaptileError::Unsupported(format!(
                "{} cannot pin its data for export",
                descriptor.id
            )));
        }
        Ok(ExportSession {
            stack: self.clone(),
            target,
            snapshot,
            entry,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BatchPage {
    pub tiles: Vec<TileOutcome>,
    pub consumed: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExportCursor {
    revision: [u8; 32],
    position: usize,
}

impl ExportCursor {
    pub fn position(&self) -> usize {
        self.position
    }
}

pub struct ExportSession<'a> {
    stack: TileStack,
    target: ReadTarget<'a>,
    snapshot: Arc<CatalogSnapshot>,
    entry: Arc<CatalogEntry>,
}

impl ExportSession<'_> {
    pub fn revision(&self) -> ContentVersion {
        self.entry.descriptor().revision
    }
    pub fn generation(&self) -> u64 {
        self.snapshot.generation()
    }
    pub async fn read_page(&self, coords: &[TileCoord]) -> MaptileResult<BatchPage> {
        bounded(self.target, &self.entry, self.read_page_inner(coords)).await
    }

    async fn read_page_inner(&self, coords: &[TileCoord]) -> MaptileResult<BatchPage> {
        validate_tile_request(coords, self.target.query)?;
        if self.stack.layers.is_empty()
            && let CatalogEntry::Source(slot) = self.entry.as_ref()
            && slot.descriptor().capabilities.batch
            && let Some(rows) = slot.read_batch(coords, self.target.query).await?
        {
            if rows.len() > coords.len() || (!coords.is_empty() && rows.is_empty()) {
                return Err(MaptileError::InvalidMetadata(
                    "invalid accelerated batch length".into(),
                ));
            }
            let mut tiles = Vec::with_capacity(rows.len());
            let mut bytes = 0;
            for row in rows {
                let outcome = row.map_or(TileOutcome::NoTile, TileOutcome::Present);
                check_output(&outcome).await?;
                bytes += outcome.tile().map_or(0, |t| t.data.len());
                tiles.push(outcome);
                if bytes >= MAX_TILE_BATCH_BYTES {
                    break;
                }
            }
            return Ok(BatchPage {
                consumed: tiles.len(),
                tiles,
            });
        }
        let mut tiles = Vec::with_capacity(coords.len());
        let mut bytes = 0;
        for &coord in coords {
            let outcome = self
                .stack
                .dispatch(&self.snapshot, &self.entry, self.target.at(coord))
                .await?;
            bytes += outcome.tile().map_or(0, |t| t.data.len());
            tiles.push(outcome);
            if bytes >= MAX_TILE_BATCH_BYTES {
                break;
            }
        }
        Ok(BatchPage {
            consumed: tiles.len(),
            tiles,
        })
    }
}
