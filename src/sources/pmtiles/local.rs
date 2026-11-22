//! Local file PMTiles source using owned file reads.
//!
//! Publish archives under immutable versioned paths or replace them by atomic
//! rename. In-place writes may corrupt archive consistency, but owned reads
//! cannot expose invalidated mmap pages to callers.

use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::ArchiveSourceType;
use crate::sources::file_stamp::FileStamp;
use crate::sources::pmtiles::{
    PmtilesConfig, archive_content_version, archive_tile_etag, pmt_compression_to_encoding,
    pmt_tile_type_to_format, reader::Reader,
};
use crate::sources::{SourceKind, TileCoord, TileInfo, TileQuery, TileResponse, TileSource};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;
use tilejson::TileJSON;

struct FileBackend {
    file: Arc<std::fs::File>,
    stamp: FileIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device_inode: (u64, u64),
}
impl FileIdentity {
    fn read(file: &std::fs::File) -> std::io::Result<Self> {
        let meta = file.metadata()?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            device_inode: (meta.dev(), meta.ino()),
        })
    }
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
        let file = self.file.clone();
        let stamp = self.stamp.clone();
        let bytes = crate::transport::run_blocking(move || {
            if FileIdentity::read(&file)? != stamp {
                return Err(MaptileError::VersionChanged("local PMTiles changed".into()));
            }
            let mut data = vec![0; length];
            let mut read = 0;
            while read < length {
                #[cfg(unix)]
                let count = {
                    use std::os::unix::fs::FileExt;
                    file.read_at(&mut data[read..], (offset + read) as u64)?
                };
                #[cfg(windows)]
                let count = {
                    use std::os::windows::fs::FileExt;
                    file.seek_read(&mut data[read..], (offset + read) as u64)?
                };
                if count == 0 {
                    break;
                }
                read += count;
            }
            if FileIdentity::read(&file)? != stamp {
                return Err(MaptileError::VersionChanged("local PMTiles changed".into()));
            }
            data.truncate(read);
            Ok(bytes::Bytes::from(data))
        })
        .await
        .map_err(|error| match error {
            MaptileError::VersionChanged(_) => pmtiles::PmtError::SourceModified,
            other => pmtiles::PmtError::Reading(std::io::Error::other(other)),
        })?;
        Ok(pmtiles::BackendResponse::new(bytes))
    }
}

pub struct LocalPmtilesSource {
    cfg: PmtilesConfig,
    reader: Arc<Reader<FileBackend>>,
    tj: TileJSON,
    info: TileInfo,
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
        let path = cfg.path.clone();
        let (file, stamp, validator) = crate::transport::run_blocking(move || {
            let file = Arc::new(std::fs::File::open(path)?);
            let stamp = FileIdentity::read(&file)?;
            let validator = FileStamp::from_file(&file)
                .ok()
                .map(|stamp| format!("{stamp:?}"));
            Ok((file, stamp, validator))
        })
        .await?;
        let last_modified = Some(stamp.modified);
        let content_version = archive_content_version(&cfg.path, validator.as_deref());
        let reader = Reader::open(FileBackend { file, stamp }).await?;

        let header = &reader.header;
        let native_format = pmt_tile_type_to_format(header.tile_type)?;
        let encoding = pmt_compression_to_encoding(header.tile_compression)?;

        let info = TileInfo {
            format: cfg.serve_as.unwrap_or(native_format),
            encoding,
        };
        let tj = super::read_tilejson(
            &reader,
            cfg.description.as_deref(),
            TileInfo {
                format: native_format,
                encoding,
            },
        )
        .await?;

        Ok(Self {
            cfg,
            reader: Arc::new(reader),
            tj,
            info,
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
            last_modified: self.last_modified,
            content_version: self.content_version,
        })
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        crate::sources::validate_source_request(self, &[xyz], query)?;
        let coord = super::checked_coord(xyz)?;
        let bytes = self
            .reader
            .get_tile(coord)
            .await?
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
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(
                    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
                )
                .unwrap();
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
        let reopened = LocalPmtilesSource::open(cfg.clone()).await.unwrap();
        assert_eq!(first.content_version(), reopened.content_version());
        drop(reopened);
        let replacement = dir.path().join("replacement.pmtiles");
        write_archive(&replacement, b"after!");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            std::fs::metadata(&replacement).unwrap().len()
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            std::fs::metadata(&replacement).unwrap().modified().unwrap(),
        );
        std::fs::rename(replacement, path).unwrap();
        let second = LocalPmtilesSource::open(cfg).await.unwrap();
        assert_eq!(first.tilejson().bounds, second.tilejson().bounds);
        assert_ne!(first.content_version(), second.content_version());
        assert_eq!(
            first.content_version(),
            first.clone_source().content_version()
        );
        let xyz = TileCoord { z: 0, x: 0, y: 0 };
        assert!(matches!(
            first.get_tile(TileCoord { z: 0, x: 1, y: 0 }, None).await,
            Err(MaptileError::InvalidRequest(_))
        ));
        assert!(matches!(
            first.get_tile(TileCoord { z: 31, x: 0, y: 0 }, None).await,
            Err(MaptileError::InvalidRequest(_))
        ));
        let old = first.get_tile(xyz, None).await.unwrap();
        let new = second.get_tile(xyz, None).await.unwrap();
        assert_eq!(old.data.as_ref(), b"before");
        assert_eq!(new.data.as_ref(), b"after!");
        assert_ne!(first.tile_etag(&old), second.tile_etag(&new));
        assert_ne!(first.tile_etag(&old), first.tile_etag(&new));
    }

    #[tokio::test]
    async fn truncation_does_not_invalidate_returned_tile_bytes() {
        let tempfile = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(tempfile.path(), b"archive payload")
            .await
            .unwrap();
        let file = Arc::new(std::fs::File::open(tempfile.path()).unwrap());
        let backend = FileBackend {
            stamp: FileIdentity::read(&file).unwrap(),
            file,
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
            backend.read_exact(0, 15).await,
            Err(pmtiles::PmtError::SourceModified)
        ));
        assert!(
            backend
                .read(0, crate::transport::MAX_DECODED_TILE_BYTES + 1)
                .await
                .is_err()
        );
    }
}
