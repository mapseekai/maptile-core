//! Local file PMTiles source using owned file reads.
//!
//! Publish archives under immutable versioned paths or replace them by atomic
//! rename. In-place writes may corrupt archive consistency, but owned reads
//! cannot expose invalidated mmap pages to callers.

use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::ArchiveSourceType;
use crate::sources::pmtiles::{
    PmtilesConfig, archive_content_version, archive_tile_etag, directory_cache,
    pmt_compression_to_encoding, pmt_tile_type_to_format,
};
use crate::sources::{
    SourceKind, TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};
use async_trait::async_trait;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tilejson::TileJSON;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

type LocalReader = pmtiles::AsyncPmTilesReader<FileBackend, pmtiles::MokaCache>;

struct FileBackend {
    file: Mutex<tokio::fs::File>,
}

impl pmtiles::AsyncBackend for FileBackend {
    async fn read(
        &self,
        offset: usize,
        length: usize,
    ) -> pmtiles::PmtResult<pmtiles::BackendResponse> {
        if length > crate::transport::MAX_DECODED_TILE_BYTES || offset.checked_add(length).is_none()
        {
            return Err(pmtiles::PmtError::Reading(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PMTiles read exceeds tile byte limit",
            )));
        }
        // ponytail: file seek/read is serialized per source; use positional reads
        // if this is a measured bottleneck. CPU work
        // and responses hold no lock and own their bytes, even after truncation.
        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(offset as u64))
            .await
            .map_err(pmtiles::PmtError::Reading)?;
        let mut data = Vec::new();
        (&mut *file)
            .take(length as u64)
            .read_to_end(&mut data)
            .await
            .map_err(pmtiles::PmtError::Reading)?;
        if data.len() != length {
            return Err(pmtiles::PmtError::Reading(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "PMTiles read ended before the requested range",
            )));
        }
        Ok(pmtiles::BackendResponse::new(bytes::Bytes::from(data)))
    }
}

pub struct LocalPmtilesSource {
    cfg: PmtilesConfig,
    // `pmtiles::AsyncPmTilesReader::get_tile` takes `&self` and the reader
    // is opened once at construction; an `Arc` is enough and avoids
    // serialising tile reads through a `RwLock` held across `.await`.
    reader: Arc<LocalReader>,
    tj: TileJSON,
    info: TileInfo,
    native_format: TileFormat,
    encoding: TileEncoding,
    last_modified: Option<SystemTime>,
    content_version: ContentVersion,
}

impl std::fmt::Debug for LocalPmtilesSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalPmtilesSource")
            .field("id", &self.cfg.id)
            .field("path", &self.cfg.path)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl LocalPmtilesSource {
    pub async fn open(cfg: PmtilesConfig) -> MaptileResult<Self> {
        let file = tokio::fs::File::open(&cfg.path).await?;
        // Read metadata from the same open handle used by the reader, so an
        // atomic path replacement cannot identify the wrong archive.
        let metadata = file.metadata().await.ok();
        let last_modified = metadata.as_ref().and_then(|m| m.modified().ok());
        let validator = last_modified
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|modified| format!("modified:{}", modified.as_nanos()));
        let content_version = archive_content_version(
            &cfg.path,
            validator.as_deref(),
            metadata.as_ref().map(std::fs::Metadata::len),
        );
        let reader: LocalReader = pmtiles::AsyncPmTilesReader::try_from_cached_source(
            FileBackend {
                file: Mutex::new(file),
            },
            directory_cache(),
        )
        .await
        .map_err(MaptileError::backend)?;

        let header = reader.get_header();
        let native_format = pmt_tile_type_to_format(header.tile_type)?;
        let encoding = pmt_compression_to_encoding(header.tile_compression)?;

        let info = TileInfo {
            format: cfg.serve_as.unwrap_or(native_format),
            encoding,
        };
        let reader = Arc::new(reader);
        let tj = super::read_tilejson(
            reader.clone(),
            cfg.description.as_deref(),
            TileInfo {
                format: native_format,
                encoding,
            },
        )
        .await?;

        Ok(Self {
            cfg,
            reader,
            tj,
            info,
            native_format,
            encoding,
            last_modified,
            content_version,
        })
    }
}

#[async_trait]
impl TileSource for LocalPmtilesSource {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn source_kind(&self) -> SourceKind {
        SourceKind::NativeArchive(ArchiveSourceType::Pmtiles)
    }

    fn content_version(&self) -> ContentVersion {
        self.content_version
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        self.info
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(LocalPmtilesSource {
            cfg: self.cfg.clone(),
            reader: self.reader.clone(),
            tj: self.tj.clone(),
            info: self.info,
            native_format: self.native_format,
            encoding: self.encoding,
            last_modified: self.last_modified,
            content_version: self.content_version,
        })
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let coord = pmtiles::TileCoord::new(xyz.z, xyz.x, xyz.y)
            .map_err(|_| MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        let bytes = self
            .reader
            .get_tile(coord)
            .await
            .map_err(MaptileError::backend)?
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        Ok(TileResponse {
            data: bytes,
            info: self.info,
            last_modified: self.last_modified,
        })
    }

    fn tile_etag(&self, response: &TileResponse) -> String {
        archive_tile_etag(self.content_version, response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmtiles::AsyncBackend;

    #[tokio::test]
    async fn atomic_archive_replacement_changes_version_without_changing_header_bounds() {
        use pmtiles::{Compression, PmTilesWriter, TileType};
        fn write_archive(path: &std::path::Path, payload: &[u8]) {
            let mut writer = PmTilesWriter::new(TileType::Mvt)
                .tile_compression(Compression::None)
                .internal_compression(Compression::None)
                .metadata(r#"{"name":"atomic-test","vector_layers":[{"id":"layer","fields":{}}]}"#)
                .create(std::fs::File::create(path).unwrap())
                .unwrap();
            writer
                .add_tile(pmtiles::TileCoord::new(0, 0, 0).unwrap(), payload)
                .unwrap();
            writer.finalize().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiles.pmtiles");
        write_archive(&path, b"before");
        let cfg = PmtilesConfig {
            id: "stable-id".into(),
            path: path.to_string_lossy().into_owned(),
            serve_as: None,
            description: None,
        };
        let first = LocalPmtilesSource::open(cfg.clone()).await.unwrap();
        let replacement = dir.path().join("replacement.pmtiles");
        write_archive(&replacement, b"after update");
        std::fs::rename(replacement, path).unwrap();
        let second = LocalPmtilesSource::open(cfg).await.unwrap();
        assert_eq!(first.tilejson().bounds, second.tilejson().bounds);
        assert_ne!(first.content_version(), second.content_version());
        assert_eq!(
            first.content_version(),
            first.clone_source().content_version()
        );
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        let old = first.get_tile(xyz, None).await.unwrap();
        let new = second.get_tile(xyz, None).await.unwrap();
        assert_eq!(old.data.as_ref(), b"before");
        assert_eq!(new.data.as_ref(), b"after update");
        assert_ne!(first.tile_etag(&old), second.tile_etag(&new));
        assert_ne!(first.tile_etag(&old), first.tile_etag(&new));
    }

    #[tokio::test]
    async fn truncation_does_not_invalidate_returned_tile_bytes() {
        let tempfile = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(tempfile.path(), b"archive payload")
            .await
            .unwrap();
        let backend = FileBackend {
            file: Mutex::new(tokio::fs::File::open(tempfile.path()).await.unwrap()),
        };
        let response = backend.read_exact(0, 15).await.unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(tempfile.path())
            .unwrap()
            .set_len(0)
            .unwrap();
        assert_eq!(response.bytes.as_ref(), b"archive payload");
        assert!(matches!(
            backend.read(0, 15).await,
            Err(pmtiles::PmtError::Reading(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        assert!(
            backend
                .read(0, crate::transport::MAX_DECODED_TILE_BYTES + 1)
                .await
                .is_err()
        );
    }
}
