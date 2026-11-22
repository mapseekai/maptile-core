//! Cloud object storage PMTiles source (S3/Azure/GCS) via `object_store`.

use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::ArchiveSourceType;
use crate::sources::pmtiles::{
    PmtilesConfig, pmt_compression_to_encoding, pmt_tile_type_to_format, reader::Reader,
};
use crate::sources::{SourceKind, TileCoord, TileInfo, TileQuery, TileResponse, TileSource};
use async_trait::async_trait;
use bytes::BytesMut;
use futures_util::TryStreamExt;
use object_store::{GetOptions, GetRange, ObjectStore, path::Path};
use pmtiles::{AsyncBackend, BackendResponse, PmtError, PmtResult};
use std::sync::{Arc, OnceLock};
use tilejson::TileJSON;

struct CloudBackend {
    store: Arc<dyn ObjectStore>,
    path: Path,
    identity: OnceLock<(ObjectIdentity, u64)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ObjectIdentity {
    Version(String),
    Etag(String),
}

impl AsyncBackend for CloudBackend {
    async fn read(&self, offset: usize, length: usize) -> PmtResult<BackendResponse> {
        let start = offset as u64;
        let end = offset.checked_add(length).ok_or_else(|| {
            PmtError::Reading(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid PMTiles range",
            ))
        })? as u64;
        if length == 0 {
            return Err(PmtError::Reading(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty PMTiles range",
            )));
        }
        let (version, if_match) = match self.identity.get().map(|(identity, _)| identity) {
            Some(ObjectIdentity::Version(version)) => (Some(version.clone()), None),
            Some(ObjectIdentity::Etag(etag)) => (None, Some(etag.clone())),
            None => (None, None),
        };
        let response = self
            .store
            .get_opts(
                &self.path,
                GetOptions {
                    range: Some(GetRange::Bounded(start..end)),
                    version,
                    if_match,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| match error {
                object_store::Error::Precondition { .. } if self.identity.get().is_some() => {
                    PmtError::SourceModified
                }
                object_store::Error::NotFound { .. } if self.identity.get().is_some() => {
                    PmtError::SourceModified
                }
                other => PmtError::from(other),
            })?;
        let size = response.meta.size;
        let identity = response
            .meta
            .version
            .as_ref()
            .filter(|version| !version.is_empty() && version.as_str() != "null")
            .map(|version| ObjectIdentity::Version(version.clone()))
            .or_else(|| {
                response
                    .meta
                    .e_tag
                    .as_ref()
                    .filter(|etag| {
                        !etag.is_empty() && etag.as_str() != "*" && !etag.starts_with("W/")
                    })
                    .map(|etag| ObjectIdentity::Etag(etag.clone()))
            })
            .ok_or_else(|| {
                if self.identity.get().is_some() {
                    return PmtError::SourceModified;
                }
                PmtError::Reading(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "PMTiles requires an object version or strong ETag",
                ))
            })?;
        let identity = (identity, size);
        if let Some(expected) = self.identity.get() {
            if expected != &identity {
                return Err(PmtError::SourceModified);
            }
        } else if self.identity.set(identity.clone()).is_err()
            && self.identity.get() != Some(&identity)
        {
            return Err(PmtError::SourceModified);
        }
        if response.range != (start..end.min(size)) || start >= size {
            return Err(PmtError::RangeRequestsUnsupported);
        }
        let version = match &identity.0 {
            ObjectIdentity::Version(value) => format!("version:{size}:{value}"),
            ObjectIdentity::Etag(value) => format!("etag:{size}:{value}"),
        };
        let expected = (response.range.end - start) as usize;
        let mut body = response.into_stream();
        let mut bytes = BytesMut::new();
        while let Some(chunk) = body.try_next().await? {
            if chunk.len() > expected - bytes.len() {
                return Err(PmtError::ResponseBodyTooLong(
                    bytes.len().saturating_add(chunk.len()),
                    expected,
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != expected {
            return Err(PmtError::UnexpectedNumberOfBytesReturned(
                expected,
                bytes.len(),
            ));
        }
        Ok(BackendResponse {
            bytes: bytes.freeze(),
            data_version_string: Some(version),
        })
    }
}

/// Cloud object storage PMTiles source backed by any `object_store` implementation.
#[derive(Clone)]
pub struct CloudPmtilesSource {
    cfg: PmtilesConfig,
    reader: Arc<Reader<CloudBackend>>,
    tj: TileJSON,
    info: TileInfo,
    content_version: ContentVersion,
}

/// URL schemes that indicate a cloud object storage path.
const CLOUD_SCHEMES: &[&str] = &["s3://", "s3a://", "az://", "gs://"];

/// Returns `true` if `path` looks like a cloud object storage URL.
#[must_use]
pub fn is_cloud_url(path: &str) -> bool {
    CLOUD_SCHEMES.iter().any(|s| path.starts_with(s))
}

impl CloudPmtilesSource {
    /// Construct a source from a pre-built `ObjectStore`.
    ///
    /// Recommended for tests (use `LocalFileSystem`) and for callers that want
    /// full control over credentials and endpoint configuration.
    ///
    /// The `cfg.path` must be a URL whose path component is understood by the
    /// provided store (e.g. `file:///abs/path.pmtiles` for `LocalFileSystem`).
    pub async fn open_with_store(
        cfg: PmtilesConfig,
        store: Arc<dyn object_store::ObjectStore>,
    ) -> MaptileResult<Self> {
        let url: url::Url = cfg
            .path
            .parse()
            .map_err(|e: url::ParseError| MaptileError::backend(e))?;

        let object_path =
            object_store::path::Path::from_url_path(url.path()).map_err(MaptileError::backend)?;

        let reader = Reader::open(CloudBackend {
            store,
            path: object_path,
            identity: OnceLock::new(),
        })
        .await?;
        // An injected store can resolve the same URL and ETag in another account or endpoint.
        let content_version = ContentVersion::new_generation();

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
            content_version,
        })
    }
}

#[async_trait]
impl TileSource for CloudPmtilesSource {
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

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        crate::sources::request::validate_source_request(self, &[xyz], query)?;
        let coord = super::checked_coord(xyz)?;
        let bytes = self
            .reader
            .get_tile(coord)
            .await?
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        Ok(TileResponse {
            data: bytes,
            info: self.info,
            last_modified: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt;
    #[tokio::test]
    async fn equal_length_replacement_fails_conditional_range() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let path = Path::from("archive.pmtiles");
        store
            .put(&path, bytes::Bytes::from_static(b"one!").into())
            .await
            .unwrap();
        let backend = CloudBackend {
            store: store.clone(),
            path: path.clone(),
            identity: OnceLock::new(),
        };
        assert_eq!(backend.read(0, 4).await.unwrap().bytes, "one!");
        store
            .put(&path, bytes::Bytes::from_static(b"two!").into())
            .await
            .unwrap();
        assert!(matches!(
            backend.read(0, 4).await,
            Err(PmtError::SourceModified)
        ));
    }
}
