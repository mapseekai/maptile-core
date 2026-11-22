//! HTTP remote PMTiles source.

use super::{
    PmtilesConfig, archive_content_version, archive_tile_etag, pmt_compression_to_encoding,
    pmt_tile_type_to_format,
    reader::{IO_TIMEOUT, Reader},
};
use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::io::http_range::{
    has_identity_content_encoding, range_http_client_builder, strong_http_etag as strong_etag,
};
use crate::sources::archive_inspection::ArchiveSourceType;
use crate::sources::{SourceKind, TileCoord, TileInfo, TileQuery, TileResponse, TileSource};
use async_trait::async_trait;
use bytes::BytesMut;
use pmtiles::reqwest::{self, StatusCode, header};
use pmtiles::{AsyncBackend, BackendResponse, PmtError, PmtResult};
use std::sync::{Arc, OnceLock};
use tilejson::TileJSON;

struct HttpBackend {
    client: reqwest::Client,
    url: String,
    identity: OnceLock<(String, u64, String)>,
}

impl AsyncBackend for HttpBackend {
    async fn read(&self, offset: usize, length: usize) -> PmtResult<BackendResponse> {
        let end = offset
            .checked_add(length)
            .and_then(|end| end.checked_sub(1))
            .ok_or_else(|| {
                PmtError::Reading(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid PMTiles range",
                ))
            })?;
        let mut request = self
            .client
            .get(&self.url)
            .header(header::RANGE, format!("bytes={offset}-{end}"));
        if let Some((etag, _, _)) = self.identity.get() {
            request = request.header(header::IF_MATCH, etag);
        }
        let response = request.send().await.map_err(reqwest::Error::without_url)?;
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Err(PmtError::SourceModified);
        }
        if self.identity.get().is_some() && response.status() == StatusCode::NOT_FOUND {
            return Err(PmtError::SourceModified);
        }
        let mut response = response
            .error_for_status()
            .map_err(reqwest::Error::without_url)?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(PmtError::RangeRequestsUnsupported);
        }
        if !has_identity_content_encoding(response.headers()) {
            return Err(PmtError::RangeRequestsUnsupported);
        }
        let (expected, total) = content_range(response.headers(), offset, end)?;
        let etag = strong_etag(response.headers())
            .ok_or_else(|| {
                if self.identity.get().is_some() {
                    return PmtError::SourceModified;
                }
                PmtError::Reading(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "PMTiles requires a strong ETag",
                ))
            })?
            .to_owned();
        let identity = (etag, total, response.url().as_str().to_owned());
        if let Some(expected) = self.identity.get() {
            if expected != &identity {
                return Err(PmtError::SourceModified);
            }
        } else if self.identity.set(identity.clone()).is_err()
            && self.identity.get() != Some(&identity)
        {
            return Err(PmtError::SourceModified);
        }
        let resource = blake3::hash(identity.2.as_bytes());
        let version = format!("etag:{total}:{resource}:{}", identity.0);
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
        let client = range_http_client_builder()
            .user_agent(concat!("maptile-core/", env!("CARGO_PKG_VERSION")))
            .timeout(IO_TIMEOUT)
            .build()
            .map_err(MaptileError::backend)?;
        let reader = Reader::open(HttpBackend {
            client,
            url: cfg.path.clone(),
            identity: OnceLock::new(),
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

    #[test]
    fn weak_missing_and_malformed_etags_are_rejected() {
        let mut headers = HeaderMap::new();
        assert_eq!(strong_etag(&headers), None);
        for tag in ["W/\"weak\"", "*", "unquoted", "\"bad\"tag\""] {
            headers.insert(header::ETAG, tag.parse().unwrap());
            assert_eq!(strong_etag(&headers), None);
        }
        headers.insert(header::ETAG, "\"strong\"".parse().unwrap());
        assert_eq!(strong_etag(&headers), Some("\"strong\""));
    }

    #[tokio::test]
    async fn pins_etag_and_rejects_equal_length_replacement() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = socket.into_split();
                let mut reader = BufReader::new(reader);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                if index == 1 {
                    assert!(headers.contains("if-match: \"one\""));
                }
                let etag = if index == 0 { "one" } else { "two" };
                let body = if index == 0 { "abcd" } else { "wxyz" };
                writer.write_all(format!("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/4\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
            }
        });
        let backend = HttpBackend {
            client: range_http_client_builder().build().unwrap(),
            url: format!("http://{addr}/tiles"),
            identity: OnceLock::new(),
        };
        assert_eq!(backend.read(0, 4).await.unwrap().bytes, "abcd");
        assert!(matches!(
            backend.read(0, 4).await,
            Err(PmtError::SourceModified)
        ));
        server.await.unwrap();
    }
}
