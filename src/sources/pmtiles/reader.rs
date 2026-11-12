//! Bounded PMTiles v3 reads for the tile sources.

use super::pmt_compression_to_encoding;
use crate::error::{MaptileError, MaptileResult};
use crate::transport::{MAX_DECODED_TILE_BYTES, decode_with_limit, run_blocking};
use bytes::{Buf, Bytes};
use pmtiles::{AsyncBackend, Header, TileId};
use std::mem::{size_of, size_of_val};
use std::sync::Arc;
use std::time::Duration;

const HEADER_BYTES: usize = 127;
const INITIAL_BYTES: usize = 16_384;
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
const MAX_LEAF_DEPTH: usize = 6;
const MAX_SAMPLE_READS: usize = 1024;
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Range {
    offset: u64,
    length: u64,
}

impl Range {
    fn read(header: &mut Bytes) -> Self {
        Self {
            offset: header.get_u64_le(),
            length: header.get_u64_le(),
        }
    }

    fn checked(self, limit: usize) -> MaptileResult<(usize, usize)> {
        if self.length > limit as u64 {
            return Err(MaptileError::PayloadTooLarge { limit });
        }
        let offset = usize::try_from(self.offset).map_err(|_| invalid_range())?;
        let length = self.length as usize;
        offset.checked_add(length).ok_or_else(invalid_range)?;
        Ok((offset, length))
    }

    fn entry(self, offset: u64, length: u32) -> MaptileResult<Self> {
        let length = u64::from(length);
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.length)
        {
            return Err(invalid_range());
        }
        Ok(Self {
            offset: self.offset.checked_add(offset).ok_or_else(invalid_range)?,
            length,
        })
    }
}

fn invalid_range() -> MaptileError {
    MaptileError::InvalidMetadata("PMTiles range is outside its archive section".into())
}

#[derive(Clone, Copy, Default)]
struct Entry {
    id: u64,
    offset: u64,
    length: u32,
    run: u32,
}

fn parse_directory(mut input: &[u8]) -> MaptileResult<Arc<[Entry]>> {
    fn varint(input: &mut &[u8]) -> MaptileResult<u64> {
        prost::encoding::decode_varint(input).map_err(MaptileError::backend)
    }
    let invalid = || MaptileError::InvalidMetadata("invalid PMTiles directory".into());
    let count = usize::try_from(varint(&mut input)?).map_err(|_| invalid())?;
    // Four varints per entry, and the expanded entries must fit the same budget.
    if count > input.len() / 4 || count > MAX_INDEX_BYTES / size_of::<Entry>() {
        return Err(invalid());
    }
    let mut entries = vec![Entry::default(); count];
    let mut id = 0u64;
    for (index, entry) in entries.iter_mut().enumerate() {
        let delta = varint(&mut input)?;
        if index != 0 && delta == 0 {
            return Err(invalid());
        }
        id = id.checked_add(delta).ok_or_else(invalid)?;
        entry.id = id;
    }
    for entry in &mut entries {
        entry.run = u32::try_from(varint(&mut input)?).map_err(|_| invalid())?;
        if entry.run != 0 {
            let last = entry
                .id
                .checked_add(u64::from(entry.run - 1))
                .ok_or_else(invalid)?;
            TileId::new(last).map_err(|_| invalid())?;
        }
    }
    for entry in &mut entries {
        entry.length = u32::try_from(varint(&mut input)?).map_err(|_| invalid())?;
    }
    let mut next_offset = None;
    for entry in &mut entries {
        entry.offset = match varint(&mut input)? {
            0 => next_offset.ok_or_else(invalid)?,
            value => value - 1,
        };
        next_offset = Some(
            entry
                .offset
                .checked_add(u64::from(entry.length))
                .ok_or_else(invalid)?,
        );
    }
    Ok(entries.into())
}

pub(super) struct Reader<B> {
    backend: B,
    version: Option<String>,
    pub(super) header: Header,
    metadata: Range,
    leaves: Range,
    tiles: Range,
    root: Arc<[Entry]>,
    directories: moka::future::Cache<Range, Arc<[Entry]>>,
}

impl<B: AsyncBackend + Send + Sync> Reader<B> {
    pub(super) fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub(super) async fn open(backend: B) -> MaptileResult<Self> {
        let initial = tokio::time::timeout(IO_TIMEOUT, backend.read(0, INITIAL_BYTES))
            .await
            .map_err(MaptileError::backend)?
            .map_err(MaptileError::backend)?;
        if initial.bytes.len() < HEADER_BYTES {
            return Err(invalid_range());
        }
        let mut fields = initial.bytes.slice(..HEADER_BYTES);
        let header = Header::try_from_bytes(fields.clone()).map_err(MaptileError::backend)?;
        if header.spec_version() != 3 {
            return Err(MaptileError::InvalidMetadata(
                "unsupported PMTiles version".into(),
            ));
        }
        fields.advance(8);
        let (offset, length) = Range::read(&mut fields).checked(INITIAL_BYTES)?;
        if offset < HEADER_BYTES || offset + length > initial.bytes.len().min(INITIAL_BYTES) {
            return Err(invalid_range());
        }
        let metadata = Range::read(&mut fields);
        let leaves = Range::read(&mut fields);
        let tiles = Range::read(&mut fields);
        let root = initial.bytes.slice(offset..offset + length);
        let encoding = pmt_compression_to_encoding(header.internal_compression())?;
        let root = run_blocking(move || {
            parse_directory(&decode_with_limit(&root, encoding, MAX_INDEX_BYTES)?)
        })
        .await?;
        Ok(Self {
            backend,
            version: initial.data_version_string,
            header,
            metadata,
            leaves,
            tiles,
            root,
            directories: moka::future::Cache::builder()
                .max_capacity(16 * 1024 * 1024)
                .weigher(|_: &Range, entries: &Arc<[Entry]>| {
                    (128 + size_of_val(entries.as_ref())) as u32
                })
                .build(),
        })
    }

    async fn read(&self, range: Range, limit: usize) -> MaptileResult<Bytes> {
        let (offset, length) = range.checked(limit)?;
        if length == 0 {
            return Ok(Bytes::new());
        }
        let response = self
            .backend
            .read_exact(offset, length)
            .await
            .map_err(MaptileError::backend)?;
        if self.version.is_some() && self.version != response.data_version_string {
            return Err(MaptileError::backend(pmtiles::PmtError::SourceModified));
        }
        Ok(response.bytes)
    }

    async fn leaf(&self, entry: Entry) -> MaptileResult<Arc<[Entry]>> {
        let range = self.leaves.entry(entry.offset, entry.length)?;
        self.directories
            .try_get_with(range, async {
                let raw = self.read(range, MAX_INDEX_BYTES).await?;
                let encoding = pmt_compression_to_encoding(self.header.internal_compression())?;
                run_blocking(move || {
                    parse_directory(&decode_with_limit(&raw, encoding, MAX_INDEX_BYTES)?)
                })
                .await
            })
            .await
            .map_err(|error| (*error).clone())
    }

    async fn tile(&self, entry: Entry) -> MaptileResult<Bytes> {
        self.read(
            self.tiles.entry(entry.offset, entry.length)?,
            MAX_DECODED_TILE_BYTES,
        )
        .await
    }

    pub(super) async fn get_tile(&self, id: impl Into<TileId>) -> MaptileResult<Option<Bytes>> {
        let id = id.into().value();
        tokio::time::timeout(IO_TIMEOUT, async {
            let mut directory = self.root.clone();
            for depth in 0..=MAX_LEAF_DEPTH {
                let end = directory.partition_point(|entry| entry.id <= id);
                let Some(entry) = end.checked_sub(1).map(|i| directory[i]) else {
                    return Ok(None);
                };
                if entry.run != 0 {
                    return if id - entry.id < u64::from(entry.run) {
                        self.tile(entry).await.map(Some)
                    } else {
                        Ok(None)
                    };
                }
                if depth == MAX_LEAF_DEPTH {
                    break;
                }
                directory = self.leaf(entry).await?;
            }
            Err(MaptileError::InvalidMetadata(
                "PMTiles leaf directory depth exceeded".into(),
            ))
        })
        .await
        .map_err(MaptileError::backend)?
    }

    pub(super) async fn sample_tile(&self) -> MaptileResult<Bytes> {
        let mut stack = vec![(self.root.clone(), 0)];
        for _ in 0..MAX_SAMPLE_READS {
            let Some((directory, next)) = stack.last_mut() else {
                break;
            };
            let Some(entry) = directory.get(*next).copied() else {
                stack.pop();
                continue;
            };
            *next += 1;
            if entry.run != 0 {
                let bytes = self.tile(entry).await?;
                if !bytes.is_empty() {
                    return Ok(bytes);
                }
            } else if stack.len() <= MAX_LEAF_DEPTH {
                stack.push((self.leaf(entry).await?, 0));
            }
        }
        Err(MaptileError::InvalidMetadata(
            "archive has no non-empty tile within directory inspection limit".into(),
        ))
    }

    pub(super) async fn metadata(
        &self,
    ) -> MaptileResult<serde_json::Map<String, serde_json::Value>> {
        let raw = tokio::time::timeout(IO_TIMEOUT, self.read(self.metadata, MAX_INDEX_BYTES))
            .await
            .map_err(MaptileError::backend)??;
        let encoding = pmt_compression_to_encoding(self.header.internal_compression())?;
        run_blocking(move || {
            let decoded = decode_with_limit(&raw, encoding, MAX_INDEX_BYTES)?;
            serde_json::from_slice(&decoded).map_err(MaptileError::from)
        })
        .await
    }
}
