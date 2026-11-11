//! HTTP remote PMTiles source.

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
use std::sync::Arc;
use std::time::Duration;
use tilejson::TileJSON;

/// HTTP PMTiles reader with a bounded `MokaCache` directory cache.
type HttpReader = pmtiles::AsyncPmTilesReader<pmtiles::HttpBackend, pmtiles::MokaCache>;

pub struct HttpPmtilesSource {
    cfg: PmtilesConfig,
    // `pmtiles::AsyncPmTilesReader::get_tile` takes `&self` and the reader
    // is opened once at construction; `Arc` is sufficient and avoids
    // serialising tile reads through a `RwLock` held across `.await`.
    reader: Arc<HttpReader>,
    tj: TileJSON,
    info: TileInfo,
    encoding: TileEncoding,
    content_version: ContentVersion,
}

impl std::fmt::Debug for HttpPmtilesSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpPmtilesSource")
            .field("id", &self.cfg.id)
            .field("url", &self.cfg.path)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl HttpPmtilesSource {
    pub async fn open(cfg: PmtilesConfig) -> MaptileResult<Self> {
        // Use the reqwest version re-exported by pmtiles to avoid version mismatch.
        let client = pmtiles::reqwest::Client::builder()
            .user_agent(concat!("maptile-core/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(MaptileError::backend)?;

        // HEAD is best-effort: origins and signed URLs may only support GET.
        // Bound the extra request and isolate this generation when validators
        // cannot be obtained, rather than trusting unchanged header bounds.
        let metadata = client
            .head(&cfg.path)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()
            .filter(|response| response.status().is_success())
            .map(|response| response.headers().clone());
        let content_version = http_content_version(&cfg.path, metadata.as_ref());

        let cache = directory_cache();
        let reader: HttpReader =
            pmtiles::AsyncPmTilesReader::new_with_cached_url(cache, client, cfg.path.as_str())
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

fn http_content_version(
    path: &str,
    headers: Option<&pmtiles::reqwest::header::HeaderMap>,
) -> ContentVersion {
    use pmtiles::reqwest::header::{CONTENT_LENGTH, ETAG, LAST_MODIFIED};
    let length = headers
        .and_then(|headers| headers.get(CONTENT_LENGTH))
        .and_then(|length| length.to_str().ok())
        .and_then(|length| length.parse::<u64>().ok());
    let etag = headers
        .and_then(|headers| headers.get(ETAG))
        .and_then(|etag| etag.to_str().ok())
        .filter(|etag| !etag.is_empty() && !etag.starts_with("W/"));
    let validator = etag.map(|etag| format!("etag:{etag}")).or_else(|| {
        headers
            .and_then(|headers| headers.get(LAST_MODIFIED))
            .and_then(|modified| modified.to_str().ok())
            .filter(|modified| !modified.is_empty())
            .map(|modified| format!("modified:{modified}"))
    });
    archive_content_version(path, validator.as_deref(), length)
}

#[async_trait]
impl TileSource for HttpPmtilesSource {
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
        Box::new(HttpPmtilesSource {
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
            last_modified: None,
        })
    }

    fn tile_etag(&self, response: &TileResponse) -> String {
        archive_tile_etag(self.content_version, response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmtiles::reqwest::header::{CONTENT_LENGTH, ETAG, HeaderMap, LAST_MODIFIED};

    #[test]
    fn http_identity_uses_validators_and_isolates_unverifiable_opens() {
        let path = "https://example.com/tiles.pmtiles";
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, "100".parse().unwrap());
        headers.insert(ETAG, "\"first\"".parse().unwrap());
        let first = http_content_version(path, Some(&headers));
        assert_eq!(first, http_content_version(path, Some(&headers)));
        headers.insert(ETAG, "\"second\"".parse().unwrap());
        assert_ne!(first, http_content_version(path, Some(&headers)));
        headers.insert(ETAG, "W/\"second\"".parse().unwrap());
        assert_ne!(
            http_content_version(path, Some(&headers)),
            http_content_version(path, Some(&headers))
        );
        headers.insert(
            LAST_MODIFIED,
            "Wed, 30 Sep 2026 00:00:00 GMT".parse().unwrap(),
        );
        assert_eq!(
            http_content_version(path, Some(&headers)),
            http_content_version(path, Some(&headers))
        );
        assert_ne!(
            http_content_version(path, None),
            http_content_version(path, None)
        );
    }
}
