use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use object_store::{
    ClientOptions, CopyOptions, Error, GetOptions, GetResult, GetResultPayload, ListResult,
    MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions, PutPayload,
    PutResult,
    client::{
        HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse, HttpService,
    },
    http::HttpBuilder,
    path::Path,
};

use super::agtiff_request::ACTIVE_RASTER_REQUEST;
use crate::error::{MaptileError, MaptileResult};
use crate::transport::CATALOG_BUILD_LEASE;

const LOCAL_READ_CHUNK_BYTES: u64 = 256 * 1024;

/// async-geotiff checks each range internally, but erases transport errors before returning them.
/// Observe the same responses to preserve a lost revision as a domain error.
pub(super) struct RemoteSnapshot {
    store: Arc<dyn ObjectStore>,
    opened: OnceLock<ObjectMeta>,
    lost: Arc<AtomicBool>,
    http: bool,
    local: bool,
}

impl RemoteSnapshot {
    pub(super) fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            store,
            opened: OnceLock::new(),
            lost: Arc::default(),
            http: false,
            local: false,
        }
    }

    pub(super) fn local(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            local: true,
            ..Self::new(store)
        }
    }

    pub(super) async fn http(url: &str) -> MaptileResult<(Self, String)> {
        use reqwest::{StatusCode, header};
        let invalid = || {
            MaptileError::Config("HTTP COG requires a single-byte range and a strong ETag".into())
        };
        let url = url.to_owned();
        let client = crate::transport::run_blocking(|| {
            super::range_http_client_builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|_| MaptileError::Config("cannot build HTTP COG client".into()))
        })
        .await?;
        let response = client
            .get(url)
            .header(header::RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|_| invalid())?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(invalid());
        }
        let etag = super::strong_http_etag(response.headers())
            .ok_or_else(invalid)?
            .to_owned();
        if !super::has_identity_content_encoding(response.headers()) {
            return Err(MaptileError::Config("encoded HTTP COG range".into()));
        }
        let url = response.url().clone();
        let identity = crate::cache::ContentVersion::from_input(url.as_str().as_bytes()).to_hex();
        let lost = Arc::<AtomicBool>::default();
        let failed = lost.clone();
        let expected = etag;
        let if_match = expected.parse().map_err(|_| invalid())?;
        let store = crate::transport::run_blocking(move || {
            let mut headers = header::HeaderMap::new();
            headers.insert(header::IF_MATCH, if_match);
            let client = super::range_http_client_builder()
                .default_headers(headers)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| invalid())?;
            HttpBuilder::new()
                .with_url(url)
                .with_http_connector(FixedClient {
                    client,
                    etag: expected,
                    lost: failed,
                })
                .build()
                .map_err(|_| invalid())
        })
        .await?;
        Ok((
            Self {
                store: Arc::new(store),
                opened: OnceLock::new(),
                lost,
                http: true,
                local: false,
            },
            identity,
        ))
    }

    pub(super) fn check(&self, id: &str) -> MaptileResult<()> {
        if self.lost.load(Ordering::Acquire) {
            Err(MaptileError::VersionChanged(id.into()))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug)]
struct FixedClient {
    client: reqwest::Client,
    etag: String,
    lost: Arc<AtomicBool>,
}

impl HttpConnector for FixedClient {
    fn connect(&self, _: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(self.clone()))
    }
}

#[async_trait::async_trait]
impl HttpService for FixedClient {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let response = self.client.call(request).await?;
        let status = response.status();
        if status.is_redirection()
            || matches!(status.as_u16(), 404 | 410 | 412)
            || (status.is_success()
                && super::strong_http_etag(response.headers()) != Some(self.etag.as_str()))
        {
            self.lost.store(true, Ordering::Release);
            return Err(HttpError::new(
                HttpErrorKind::Decode,
                std::io::Error::other("COG revision no longer available"),
            ));
        }
        if status.is_success() && !super::has_identity_content_encoding(response.headers()) {
            return Err(HttpError::new(
                HttpErrorKind::Decode,
                std::io::Error::other("encoded HTTP COG range"),
            ));
        }
        Ok(response)
    }
}

impl fmt::Debug for RemoteSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("COG object snapshot")
    }
}

impl fmt::Display for RemoteSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

fn read_error(message: &'static str) -> Error {
    Error::Generic {
        store: "COG",
        source: message.into(),
    }
}

#[async_trait::async_trait]
impl ObjectStore for RemoteSnapshot {
    async fn get_opts(
        &self,
        path: &Path,
        mut options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if self.local {
            // LocalFileSystem moves GetOptions into the blocking open and stat job.
            if let Ok(active) = ACTIVE_RASTER_REQUEST.try_with(Arc::clone) {
                options.extensions.insert(active);
            }
            if let Ok(build) = CATALOG_BUILD_LEASE.try_with(Arc::clone) {
                options.extensions.insert(build);
            }
        }
        let mut raw_read = if options.head {
            None
        } else {
            super::agtiff_request::ActiveRasterRequest::object_read()
        };
        let conditional = options.if_match.is_some() || options.version.is_some();
        let mut result = self.store.get_opts(path, options).await.map_err(|error| {
            if (conditional || self.opened.get().is_some() || self.http)
                && matches!(error, Error::Precondition { .. } | Error::NotFound { .. })
            {
                self.lost.store(true, Ordering::Release);
            }
            if self.http {
                read_error("HTTP range request failed")
            } else {
                error
            }
        })?;
        if result
            .meta
            .version
            .as_deref()
            .is_some_and(|id| id.trim().is_empty() || id == "null")
        {
            result.meta.version = None;
        }
        let expected = self.opened.get_or_init(|| result.meta.clone());
        if expected.size != result.meta.size
            || expected.e_tag != result.meta.e_tag
            || expected.version != result.meta.version
        {
            self.lost.store(true, Ordering::Release);
            return Err(read_error("COG object changed"));
        }
        if let GetResultPayload::File(file, _) = result.payload {
            let start = Some(result.range.start);
            let remaining = result.range.end.saturating_sub(result.range.start);
            result.payload = GetResultPayload::Stream(
                stream::try_unfold(
                    (file, start, remaining),
                    |(mut file, start, remaining)| async move {
                        if remaining == 0 {
                            return Ok(None);
                        }
                        let active = ACTIVE_RASTER_REQUEST.try_with(Arc::clone).ok();
                        crate::transport::run_blocking(move || {
                            let _active = active;
                            if let Some(start) = start {
                                file.seek(SeekFrom::Start(start))?;
                            }
                            let mut chunk = vec![0; remaining.min(LOCAL_READ_CHUNK_BYTES) as usize];
                            file.read_exact(&mut chunk)?;
                            let remaining = remaining - chunk.len() as u64;
                            Ok(Some((Bytes::from(chunk), (file, None, remaining))))
                        })
                        .await
                        .map_err(|error| Error::Generic {
                            store: "COG",
                            source: Box::new(error),
                        })
                    },
                )
                .boxed(),
            );
        }
        if raw_read.is_some()
            && let GetResultPayload::Stream(body) = result.payload
        {
            result.payload = GetResultPayload::Stream(
                body.inspect_ok(move |bytes| {
                    if !bytes.is_empty()
                        && let Some(active) = raw_read.take()
                    {
                        active.received_bytes();
                    }
                })
                .boxed(),
            );
        }
        Ok(result)
    }

    async fn put_opts(
        &self,
        _: &Path,
        _: PutPayload,
        _: PutOptions,
    ) -> object_store::Result<PutResult> {
        Err(read_error("read-only COG"))
    }
    async fn put_multipart_opts(
        &self,
        _: &Path,
        _: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Err(read_error("read-only COG"))
    }
    fn delete_stream(
        &self,
        _: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        stream::once(async { Err(read_error("read-only COG")) }).boxed()
    }
    fn list(&self, _: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        stream::once(async { Err(read_error("COG listing is unsupported")) }).boxed()
    }
    async fn list_with_delimiter(&self, _: Option<&Path>) -> object_store::Result<ListResult> {
        Err(read_error("COG listing is unsupported"))
    }
    async fn copy_opts(&self, _: &Path, _: &Path, _: CopyOptions) -> object_store::Result<()> {
        Err(read_error("read-only COG"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{GetRange, local::LocalFileSystem};

    #[tokio::test]
    async fn local_range_stream_preserves_offsets_and_reports_short_reads() {
        let dir = tempfile::tempdir().unwrap();
        let name = "source.tif";
        let path = dir.path().join(name);
        let data: Vec<u8> = (0..3 * LOCAL_READ_CHUNK_BYTES)
            .map(|n| (n % 251) as u8)
            .collect();
        std::fs::write(&path, &data).unwrap();
        let store = RemoteSnapshot::local(Arc::new(
            LocalFileSystem::new_with_prefix(dir.path()).unwrap(),
        ));
        let range = 1_043..1_823 + 2 * LOCAL_READ_CHUNK_BYTES;
        let key = Path::from(name);
        let options = GetOptions {
            range: Some(GetRange::Bounded(range.clone())),
            ..Default::default()
        };
        let result = store.get_opts(&key, options.clone()).await.unwrap();
        assert_eq!(result.range, range);
        assert_eq!(
            result.bytes().await.unwrap(),
            &data[range.start as usize..range.end as usize]
        );

        let mut chunks = store.get_opts(&key, options).await.unwrap().into_stream();
        let first = chunks.next().await.unwrap().unwrap();
        assert_eq!(first.len() as u64, LOCAL_READ_CHUNK_BYTES);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(range.start + LOCAL_READ_CHUNK_BYTES + 3)
            .unwrap();
        let error = chunks.next().await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            Error::Generic { source, .. }
                if matches!(source.downcast_ref::<MaptileError>(), Some(MaptileError::Io(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof)
        ));
    }
}
