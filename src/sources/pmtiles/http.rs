//! HTTP remote PMTiles source.

use super::{
    PmtilesConfig, archive_content_version, archive_tile_etag, pmt_compression_to_encoding,
    pmt_tile_type_to_format,
    reader::{IO_TIMEOUT, Reader},
};
use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::sources::archive_inspection::ArchiveSourceType;
use crate::sources::{SourceKind, TileCoord, TileInfo, TileQuery, TileResponse, TileSource};
use async_trait::async_trait;
use bytes::BytesMut;
use pmtiles::reqwest::{self, StatusCode, header};
use pmtiles::{AsyncBackend, BackendResponse, PmtError, PmtResult};
use std::sync::Arc;
use tilejson::TileJSON;

struct HttpBackend {
    client: reqwest::Client,
    url: String,
}

impl AsyncBackend for HttpBackend {
    async fn read(&self, offset: usize, length: usize) -> PmtResult<BackendResponse> {
        let end = offset + length - 1;
        let mut response = self
            .client
            .get(&self.url)
            .header(header::RANGE, format!("bytes={offset}-{end}"))
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(PmtError::RangeRequestsUnsupported);
        }
        let (expected, total) = content_range(response.headers(), offset, end)?;
        let version = response
            .headers()
            .get(header::ETAG)
            .and_then(|etag| etag.to_str().ok())
            .filter(|etag| etag.starts_with('"') && etag.ends_with('"'))
            .map(|etag| format!("etag:{total}:{etag}"))
            .or_else(|| {
                response
                    .headers()
                    .get(header::LAST_MODIFIED)
                    .and_then(|value| value.to_str().ok())
                    .map(|value| format!("modified:{total}:{value}"))
            })
            .unwrap_or_else(|| format!("length:{total}"));
        let mut bytes = BytesMut::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
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

fn content_range(
    headers: &header::HeaderMap,
    offset: usize,
    end: usize,
) -> PmtResult<(usize, u64)> {
    let invalid = || {
        PmtError::Reading(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid PMTiles Content-Range",
        ))
    };
    let value = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes "))
        .ok_or_else(invalid)?;
    let (range, total) = value.split_once('/').ok_or_else(invalid)?;
    let total: u64 = total.parse().map_err(|_| invalid())?;
    let (first, last) = range.split_once('-').ok_or_else(invalid)?;
    let first: usize = first.parse().map_err(|_| invalid())?;
    let last: usize = last.parse().map_err(|_| invalid())?;
    if first != offset
        || last < first
        || last as u64 != (end as u64).min(total.saturating_sub(1))
        || total == 0
    {
        return Err(invalid());
    }
    Ok((last - first + 1, total))
}

#[derive(Clone)]
pub struct HttpPmtilesSource {
    cfg: PmtilesConfig,
    reader: Arc<Reader<HttpBackend>>,
    tj: TileJSON,
    info: TileInfo,
    content_version: ContentVersion,
}

impl std::fmt::Debug for HttpPmtilesSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpPmtilesSource")
            .field("id", &self.cfg.id)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl HttpPmtilesSource {
    pub async fn open(cfg: PmtilesConfig) -> MaptileResult<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("maptile-core/", env!("CARGO_PKG_VERSION")))
            .timeout(IO_TIMEOUT)
            .build()
            .map_err(MaptileError::backend)?;
        let reader = Reader::open(HttpBackend {
            client,
            url: cfg.path.clone(),
        })
        .await?;
        let stable = reader
            .version()
            .filter(|version| version.starts_with("etag:"));
        let content_version = archive_content_version(&cfg.path, stable);
        let native_info = TileInfo {
            format: pmt_tile_type_to_format(reader.header.tile_type)?,
            encoding: pmt_compression_to_encoding(reader.header.tile_compression)?,
        };
        let tj = super::read_tilejson(&reader, cfg.description.as_deref(), native_info).await?;
        Ok(Self {
            info: TileInfo {
                format: cfg.serve_as.unwrap_or(native_info.format),
                ..native_info
            },
            cfg,
            reader: Arc::new(reader),
            tj,
            content_version,
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use pmtiles::reqwest::header::{CONTENT_RANGE, HeaderMap};

    #[test]
    fn range_can_end_early_at_eof() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, "bytes 0-199/200".parse().unwrap());
        assert_eq!(content_range(&headers, 0, 16383).unwrap(), (200, 200));
        assert!(content_range(&headers, 1, 16383).is_err());
    }
}
