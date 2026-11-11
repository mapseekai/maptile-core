//! A source-scoped, range-only bridge from DuckDB httpfs to the caller's HTTP
//! client or object store. Credentials and remote URL queries never enter SQL.

use std::io;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Response, StatusCode, header};
use axum::routing::any;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use object_store::{GetOptions, GetRange, ObjectMeta, ObjectStore, ObjectStoreExt};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::error::{MaptileError, MaptileResult};

type ByteStream = BoxStream<'static, Result<Bytes, io::Error>>;

enum Backend {
    Http {
        client: reqwest::Client,
        url: reqwest::Url,
    },
    Store {
        store: Arc<dyn ObjectStore>,
        key: object_store::path::Path,
    },
}

struct Metadata {
    size: u64,
    etag: Option<String>,
    modified: Option<String>,
    object: Option<ObjectMeta>,
    identity: String,
}

struct Reader {
    backend: Backend,
    meta: Metadata,
    timeout: Duration,
}

pub(super) struct RemoteFile {
    pub url: String,
    pub identity: String,
    shutdown: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl RemoteFile {
    pub async fn http(
        url: &str,
        client: reqwest::Client,
        timeout: Duration,
    ) -> MaptileResult<Self> {
        if url.trim() != url || url.chars().any(char::is_control) {
            return Err(config("invalid HTTP URL"));
        }
        let url = reqwest::Url::parse(url).map_err(|_| config("invalid HTTP URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.path().chars().any(|c| matches!(c, '*' | '[' | ']'))
        {
            return Err(config(
                "expected one HTTP(S) URL without userinfo, fragment or glob",
            ));
        }
        // A small GET probe works with GET-only presigned URLs as well as
        // servers that do not implement HEAD. Never accept a full-body 200.
        let deadline = tokio::time::Instant::now() + timeout;
        let response = tokio::time::timeout_at(
            deadline,
            client
                .get(url.clone())
                .header(header::ACCEPT_ENCODING, "identity")
                .header(header::RANGE, "bytes=0-7")
                .send(),
        )
        .await
        .map_err(|_| backend("remote probe timed out"))?
        .map_err(|e| backend(format!("remote probe: {}", e.without_url())))?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(backend(format!(
                "remote probe returned {}; a valid 206 is required (redirects are not followed)",
                response.status()
            )));
        }
        let size = text_header(response.headers(), header::CONTENT_RANGE)
            .and_then(|value| value.strip_prefix("bytes 0-7/")?.parse::<u64>().ok())
            .filter(|size| *size >= 12)
            .ok_or_else(|| {
                backend("remote probe requires a valid Content-Range and object size")
            })?;
        if response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err(backend("remote probe must use identity encoding"));
        }
        let etag = text_header(response.headers(), header::ETAG).filter(|v| !v.starts_with("W/"));
        let modified = text_header(response.headers(), header::LAST_MODIFIED);
        let mut body = response.bytes_stream();
        let mut remaining = 8;
        while let Some(chunk) = tokio::time::timeout_at(deadline, body.next())
            .await
            .map_err(|_| backend("remote probe body timed out"))?
        {
            let chunk = chunk.map_err(|_| backend("remote probe body failed"))?;
            if chunk.len() > remaining {
                return Err(backend("remote probe body exceeds its range"));
            }
            remaining -= chunk.len();
        }
        if remaining != 0 {
            return Err(backend("remote probe body is truncated"));
        }
        let identity = format!("{url}:{size}:{etag:?}:{modified:?}");
        Self::start(Reader {
            backend: Backend::Http { client, url },
            meta: Metadata {
                size,
                etag,
                modified,
                object: None,
                identity,
            },
            timeout,
        })
        .await
    }

    pub async fn object(
        store: Arc<dyn ObjectStore>,
        key: &str,
        timeout: Duration,
    ) -> MaptileResult<Self> {
        if key.is_empty() || key.ends_with('/') {
            return Err(config("expected one object key"));
        }
        let key = object_store::path::Path::parse(key).map_err(|_| config("invalid object key"))?;
        let meta = tokio::time::timeout(timeout, store.head(&key))
            .await
            .map_err(|_| backend("object HEAD timed out"))?
            .map_err(|_| backend("object HEAD failed"))?;
        if meta.size < 12 {
            return Err(backend("remote object is too small for Parquet"));
        }
        let identity = format!(
            "{store}:{key}:{}:{:?}:{:?}:{}",
            meta.size, meta.version, meta.e_tag, meta.last_modified
        );
        Self::start(Reader {
            backend: Backend::Store { store, key },
            meta: Metadata {
                size: meta.size,
                etag: meta.e_tag.clone(),
                modified: None,
                object: Some(meta),
                identity,
            },
            timeout,
        })
        .await
    }

    async fn start(reader: Reader) -> MaptileResult<Self> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(MaptileError::backend)?;
        let address = listener.local_addr().map_err(MaptileError::backend)?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let path = format!("/{token}.parquet");
        let identity = blake3::hash(reader.meta.identity.as_bytes())
            .to_hex()
            .to_string();
        let router = Router::new()
            .route(&path, any(serve_range))
            .with_state(Arc::new(reader));
        let (shutdown, mut receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    while !*receiver.borrow_and_update() {
                        if receiver.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await;
        });
        Ok(Self {
            url: format!("http://{address}{path}"),
            identity,
            shutdown,
            task: Mutex::new(Some(task)),
        })
    }

    pub fn start_close(&self) {
        let _ = self.shutdown.send(true);
    }

    pub async fn close(&self) -> MaptileResult<()> {
        self.start_close();
        // Serialize concurrent closes, so every successful waiter observes that
        // the listener, accepted connections and backend clients have exited.
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await;
            task.take();
            result.map_err(|e| backend(format!("remote reader shutdown: {e}")))?;
        }
        Ok(())
    }
}

impl Drop for RemoteFile {
    fn drop(&mut self) {
        self.start_close();
    }
}

impl Reader {
    async fn range(&self, range: Range<u64>) -> MaptileResult<ByteStream> {
        match &self.backend {
            Backend::Http { client, url } => {
                let mut request = client
                    .get(url.clone())
                    .header(header::ACCEPT_ENCODING, "identity")
                    .header(
                        header::RANGE,
                        format!("bytes={}-{}", range.start, range.end - 1),
                    );
                if let Some(etag) = &self.meta.etag {
                    request = request.header(header::IF_MATCH, etag);
                } else if let Some(modified) = &self.meta.modified {
                    request = request.header(header::IF_UNMODIFIED_SINCE, modified);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| backend(format!("remote range: {}", e.without_url())))?;
                if response.status() != StatusCode::PARTIAL_CONTENT {
                    return Err(backend(format!(
                        "remote range returned {}; a valid 206 is required",
                        response.status()
                    )));
                }
                let expected =
                    format!("bytes {}-{}/{}", range.start, range.end - 1, self.meta.size);
                if text_header(response.headers(), header::CONTENT_RANGE).as_deref()
                    != Some(&expected)
                    || response
                        .headers()
                        .get(header::CONTENT_ENCODING)
                        .is_some_and(|v| v != "identity")
                    || self.meta.etag.as_ref().is_some_and(|v| {
                        text_header(response.headers(), header::ETAG).as_ref() != Some(v)
                    })
                    || (self.meta.etag.is_none()
                        && self.meta.modified.as_ref().is_some_and(|v| {
                            text_header(response.headers(), header::LAST_MODIFIED).as_ref()
                                != Some(v)
                        }))
                {
                    return Err(backend(
                        "remote range or object validator changed; reopen the source",
                    ));
                }
                Ok(response
                    .bytes_stream()
                    .map(|r| r.map_err(|_| io::Error::other("remote body failed")))
                    .boxed())
            }
            Backend::Store { store, key } => {
                let old = self.meta.object.as_ref().expect("object metadata");
                let result = store
                    .get_opts(
                        key,
                        GetOptions {
                            range: Some(GetRange::Bounded(range.clone())),
                            version: old.version.clone(),
                            if_match: old.e_tag.clone(),
                            if_unmodified_since: Some(old.last_modified),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|_| {
                        backend("object range failed or object changed; reopen the source")
                    })?;
                if result.range != range
                    || result.meta.size != old.size
                    || result.meta.e_tag != old.e_tag
                    || result.meta.version != old.version
                    || result.meta.last_modified != old.last_modified
                {
                    return Err(backend(
                        "object range or validator changed; reopen the source",
                    ));
                }
                Ok(result
                    .into_stream()
                    .map(|r| r.map_err(|_| io::Error::other("object body failed")))
                    .boxed())
            }
        }
    }
}

async fn serve_range(
    State(reader): State<Arc<Reader>>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    let mut response = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONNECTION, "close")
        .header(header::CONTENT_TYPE, "application/octet-stream");
    if method == Method::HEAD {
        return response
            .header(header::CONTENT_LENGTH, reader.meta.size)
            .body(Body::empty())
            .expect("fixed headers");
    }
    if method != Method::GET {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some(range) = headers
        .get(header::RANGE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| parse_range(s, reader.meta.size))
    else {
        // Never silently let httpfs fall back to an upfront full download.
        return empty(StatusCode::RANGE_NOT_SATISFIABLE);
    };
    let deadline = tokio::time::Instant::now() + reader.timeout;
    let stream = match tokio::time::timeout_at(deadline, reader.range(range.clone())).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            tracing::debug!(%error, "GeoParquet remote range failed");
            return empty(StatusCode::BAD_GATEWAY);
        }
        Err(_) => {
            tracing::debug!("GeoParquet remote range timed out");
            return empty(StatusCode::BAD_GATEWAY);
        }
    };
    let remaining = range.end - range.start;
    response = response
        .status(StatusCode::PARTIAL_CONTENT)
        .header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                range.start,
                range.end - 1,
                reader.meta.size
            ),
        )
        .header(header::CONTENT_LENGTH, remaining);
    let stream = futures_util::stream::unfold(
        (stream, remaining, false),
        move |(mut stream, mut left, done)| async move {
            if done {
                return None;
            }
            let next = tokio::time::timeout_at(deadline, stream.next()).await;
            let item = match next {
                Ok(Some(Ok(bytes))) if bytes.len() as u64 <= left => {
                    left -= bytes.len() as u64;
                    Ok(bytes)
                }
                Ok(None) if left == 0 => return None,
                _ => Err(io::Error::other(
                    "remote range truncated, oversized or timed out",
                )),
            };
            let done = item.is_err();
            Some((item, (stream, left, done)))
        },
    );
    response
        .body(Body::from_stream(stream))
        .expect("fixed headers")
}

pub(super) fn parse_range(value: &str, size: u64) -> Option<Range<u64>> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if size == 0
        || !start.bytes().all(|c| c.is_ascii_digit())
        || !end.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    if start.is_empty() {
        let length: u64 = end.parse().ok()?;
        return (length > 0).then_some(size.saturating_sub(length)..size);
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        size
    } else {
        end.parse::<u64>().ok()?.checked_add(1)?.min(size)
    };
    (start < end && start < size).then_some(start..end)
}
fn text_header(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}
fn empty(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONNECTION, "close")
        .body(Body::empty())
        .expect("fixed headers")
}
fn backend(message: impl Into<String>) -> MaptileError {
    MaptileError::BackendMsg(format!("GeoParquet: {}", message.into()))
}
fn config(message: &str) -> MaptileError {
    MaptileError::Config(format!("GeoParquet: {message}"))
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn range_parser_accepts_bounded_open_ended_and_suffix_ranges() {
        for (header, expected) in [
            ("bytes=0-0", 0..1),
            ("bytes=0-99", 0..100),
            ("bytes=90-200", 90..100),
            ("bytes=90-", 90..100),
            ("bytes=-10", 90..100),
            ("bytes=-200", 0..100),
        ] {
            assert_eq!(parse_range(header, 100), Some(expected), "{header}");
        }
    }

    #[test]
    fn range_parser_rejects_invalid_multiple_empty_and_overflowing_ranges() {
        for header in [
            "",
            "bytes=",
            "bytes=-",
            "bytes=-0",
            "bytes=100-",
            "bytes=20-10",
            "bytes=0-1,5-9",
            "bytes=+1-9",
            "bytes=0-+9",
            "bytes= 0-9",
            "bytes=0-9 ",
            "items=0-9",
            "bytes=0-18446744073709551615",
            "bytes=18446744073709551616-",
        ] {
            assert_eq!(parse_range(header, 100), None, "{header}");
        }
        assert_eq!(parse_range("bytes=-1", 0), None);
    }
}
