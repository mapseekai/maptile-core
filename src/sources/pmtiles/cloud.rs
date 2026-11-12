//! Cloud object storage PMTiles source (S3/Azure/GCS) via `object_store`.

use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::ArchiveSourceType;
use crate::sources::pmtiles::{
    PmtilesConfig, archive_content_version, archive_tile_etag, pmt_compression_to_encoding,
    pmt_tile_type_to_format, reader::Reader,
};
use crate::sources::{SourceKind, TileCoord, TileInfo, TileQuery, TileResponse, TileSource};
use async_trait::async_trait;
use bytes::BytesMut;
use futures_util::TryStreamExt;
use object_store::{GetOptions, GetRange, ObjectStore, path::Path};
use pmtiles::{AsyncBackend, BackendResponse, PmtError, PmtResult};
use std::sync::Arc;
use tilejson::TileJSON;

struct CloudBackend {
    store: Arc<dyn ObjectStore>,
    path: Path,
}

impl AsyncBackend for CloudBackend {
    async fn read(&self, offset: usize, length: usize) -> PmtResult<BackendResponse> {
        let start = offset as u64;
        let end = (offset + length) as u64;
        let response = self
            .store
            .get_opts(
                &self.path,
                GetOptions {
                    range: Some(GetRange::Bounded(start..end)),
                    ..Default::default()
                },
            )
            .await?;
        let size = response.meta.size;
        if response.range != (start..end.min(size)) || start >= size {
            return Err(PmtError::RangeRequestsUnsupported);
        }
        let version = response
            .meta
            .version
            .as_ref()
            .filter(|version| !version.is_empty() && version.as_str() != "null")
            .map(|version| format!("version:{size}:{version}"))
            .or_else(|| {
                response
                    .meta
                    .e_tag
                    .as_ref()
                    .filter(|etag| !etag.is_empty() && !etag.starts_with("W/"))
                    .map(|etag| format!("etag:{size}:{etag}"))
            })
            .unwrap_or_else(|| {
                format!(
                    "modified:{size}:{}",
                    response.meta.last_modified.to_rfc3339()
                )
            });
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
        })
        .await?;
        let stable = reader
            .version()
            .filter(|version| !version.starts_with("modified:"));
        let content_version = archive_content_version(&cfg.path, stable);

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

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
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
            .await?
            .ok_or(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y))?;
        Ok(TileResponse {
            data: bytes,
            info: self.info,
            last_modified: None,
        })
    }

    fn tile_etag(&self, response: &TileResponse) -> String {
        archive_tile_etag(self.content_version, response)
    }
}
