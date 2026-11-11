//! Cloud object storage PMTiles source (S3/Azure/GCS) via `object_store`.

use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::ArchiveSourceType;
use crate::sources::pmtiles::{
    PmtilesConfig, archive_content_version, archive_tile_etag, directory_cache,
    pmt_compression_to_encoding, pmt_tile_type_to_format,
};
use crate::sources::{
    SourceKind, TileCoord, TileEncoding, TileInfo, TileQuery, TileResponse, TileSource,
};
use async_trait::async_trait;
use bytes::Bytes;
use object_store::ObjectStoreExt;
use std::sync::Arc;
use std::time::Duration;
use tilejson::TileJSON;

/// Type alias for a cloud-backed PMTiles reader with a directory cache.
type CloudReader = pmtiles::AsyncPmTilesReader<pmtiles::ObjectStoreBackend, pmtiles::MokaCache>;

/// Cloud object storage PMTiles source backed by any `object_store` implementation.
pub struct CloudPmtilesSource {
    cfg: PmtilesConfig,
    // `pmtiles::AsyncPmTilesReader::get_tile` takes `&self` and the reader
    // is opened once at construction; an `Arc` is enough and avoids
    // serialising tile reads through a `RwLock` held across `.await`.
    reader: Arc<CloudReader>,
    tj: TileJSON,
    info: TileInfo,
    encoding: TileEncoding,
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

        let metadata = tokio::time::timeout(Duration::from_secs(5), store.head(&object_path))
            .await
            .ok()
            .and_then(Result::ok);
        let validator = metadata.as_ref().map(|metadata| {
            if let Some(version) = metadata.version.as_ref().filter(|v| !v.is_empty()) {
                format!("version:{version}")
            } else if let Some(etag) = metadata.e_tag.as_ref().filter(|etag| !etag.is_empty()) {
                format!("etag:{etag}")
            } else {
                format!("modified:{}", metadata.last_modified.to_rfc3339())
            }
        });
        let content_version = archive_content_version(
            &cfg.path,
            validator.as_deref(),
            metadata.as_ref().map(|metadata| metadata.size),
        );

        // ObjectStoreBackend::new takes Box<dyn ObjectStore>, not Arc.
        let backend = pmtiles::ObjectStoreBackend::new(Box::from(store), object_path);
        let cache = directory_cache();
        let reader = pmtiles::AsyncPmTilesReader::try_from_cached_source(backend, cache)
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
            encoding,
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
        Box::new(CloudPmtilesSource {
            cfg: self.cfg.clone(),
            reader: self.reader.clone(),
            tj: self.tj.clone(),
            info: self.info,
            encoding: self.encoding,
            content_version: self.content_version,
        })
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        _query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let coord = pmtiles::TileCoord::new(xyz.z, xyz.x, xyz.y).map_err(MaptileError::backend)?;
        let bytes: Bytes = self
            .reader
            .get_tile(coord)
            .await
            .map_err(MaptileError::backend)?
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
