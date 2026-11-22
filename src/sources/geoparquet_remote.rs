//! Validated locations and session-local secrets for DuckDB HTTPFS.

use super::geoparquet_geometry::literal;
use crate::error::{MaptileError, MaptileResult};
use duckdb::Connection;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

pub(super) const HTTPFS_UNAVAILABLE: &str =
    "cannot load DuckDB httpfs; prepare the matching signed extension with geoparquet_extensions";
pub(super) const SPATIAL_UNAVAILABLE: &str =
    "cannot load DuckDB spatial; prepare the matching signed extension with geoparquet_extensions";

/// Explicit S3-compatible storage settings for one GeoParquet session.
/// Credentials stay in DuckDB's in-memory secret store; ambient credentials
/// and persistent secrets are not used. Empty keys select anonymous access.
#[derive(Clone)]
pub struct GeoParquetS3Options {
    pub region: String,
    /// Host and optional port, without a scheme or path, e.g. `localhost:9000`.
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub use_ssl: bool,
    /// Use `endpoint/bucket/key` instead of `bucket.endpoint/key`.
    pub path_style: bool,
}

impl Default for GeoParquetS3Options {
    fn default() -> Self {
        Self {
            region: "us-east-1".into(),
            endpoint: None,
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: None,
            use_ssl: true,
            path_style: false,
        }
    }
}

impl std::fmt::Debug for GeoParquetS3Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeoParquetS3Options")
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("use_ssl", &self.use_ssl)
            .field("path_style", &self.path_style)
            .finish_non_exhaustive()
    }
}

/// Credentials and a validity window for one S3 authorization scope.
#[derive(Clone, Debug)]
pub struct GeoParquetS3Credentials {
    pub options: GeoParquetS3Options,
    pub expires_at: SystemTime,
}

/// Supply S3 credentials for this source. Each source coalesces refreshes
/// for its object URL, and never uses expired credentials after a failed refresh.
#[async_trait::async_trait]
pub trait GeoParquetCredentialProvider: Send + Sync + 'static {
    async fn credentials(&self) -> MaptileResult<GeoParquetS3Credentials>;
}

#[derive(PartialEq, Eq)]
pub(super) struct DuckDbSecret {
    sql: String,
    expires_at: Option<SystemTime>,
}

impl DuckDbSecret {
    pub fn check(&self) -> MaptileResult<()> {
        if self
            .expires_at
            .is_some_and(|time| time <= SystemTime::now())
        {
            return Err(MaptileError::SourceUnavailable(
                "S3 credentials expired".into(),
            ));
        }
        Ok(())
    }

    pub fn install(&self, conn: &Connection) -> MaptileResult<()> {
        self.check()?;
        conn.execute_batch(&self.sql)
            .map_err(|_| config("cannot configure remote authentication"))
    }
}

pub(super) struct RemoteLocation {
    pub url: String,
    credentials: Mutex<Credentials>,
    refresh: tokio::sync::Mutex<()>,
}

enum Credentials {
    Fixed(Option<Arc<DuckDbSecret>>),
    Provider {
        provider: Arc<dyn GeoParquetCredentialProvider>,
        cached: Option<Arc<DuckDbSecret>>,
        scope: Option<S3Scope>,
    },
    Closed,
}

#[derive(PartialEq, Eq)]
struct S3Scope {
    region: String,
    endpoint: Option<String>,
    use_ssl: bool,
    path_style: bool,
}

impl RemoteLocation {
    fn new(url: String, credentials: Credentials) -> Self {
        Self {
            url,
            credentials: Mutex::new(credentials),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    fn credential_state(&self) -> MutexGuard<'_, Credentials> {
        self.credentials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn close(&self) {
        let previous = std::mem::replace(&mut *self.credential_state(), Credentials::Closed);
        drop(previous);
    }

    pub fn http(url: &str, headers: BTreeMap<String, String>) -> MaptileResult<Self> {
        let url = validate_url(url, false)?;
        validate_http_headers(&headers)?;
        let headers = headers
            .iter()
            .map(|(k, v)| format!("{}: {}", literal(k), literal(v)))
            .collect::<Vec<_>>()
            .join(", ");
        let secret = (!headers.is_empty()).then(|| Arc::new(DuckDbSecret {
            sql: format!(
                "CREATE OR REPLACE SECRET maptile_http (TYPE http, SCOPE {}, EXTRA_HTTP_HEADERS MAP {{{headers}}})",
                literal(&url)
            ),
            expires_at: None,
        }));
        Ok(Self::new(url, Credentials::Fixed(secret)))
    }

    pub fn s3(url: &str, options: GeoParquetS3Options) -> MaptileResult<Self> {
        let url = validate_url(url, true)?;
        let secret = Some(Arc::new(Self::s3_secret(&url, options, None)?));
        Ok(Self::new(url, Credentials::Fixed(secret)))
    }

    pub fn s3_with_provider(
        url: &str,
        provider: Arc<dyn GeoParquetCredentialProvider>,
    ) -> MaptileResult<Self> {
        Ok(Self::new(
            validate_url(url, true)?,
            Credentials::Provider {
                provider,
                cached: None,
                scope: None,
            },
        ))
    }

    pub async fn credentials(&self) -> MaptileResult<Option<Arc<DuckDbSecret>>> {
        let _refresh = self.refresh.lock().await;
        let provider = match &mut *self.credential_state() {
            Credentials::Fixed(secret) => return Ok(secret.clone()),
            Credentials::Closed => return Err(MaptileError::Cancelled),
            Credentials::Provider {
                provider, cached, ..
            } => {
                if let Some(secret) = cached
                    && secret.check().is_ok()
                {
                    return Ok(Some(secret.clone()));
                }
                *cached = None;
                provider.clone()
            }
        };
        let credentials = provider
            .credentials()
            .await
            .map_err(|_| MaptileError::SourceUnavailable("S3 credential refresh failed".into()))?;
        let grant_scope = S3Scope {
            region: credentials.options.region.clone(),
            endpoint: credentials.options.endpoint.clone(),
            use_ssl: credentials.options.use_ssl,
            path_style: credentials.options.path_style,
        };
        let secret = Arc::new(Self::s3_secret(
            &self.url,
            credentials.options,
            Some(credentials.expires_at),
        )?);
        secret.check()?;
        if let Credentials::Provider { cached, scope, .. } = &mut *self.credential_state() {
            if scope.as_ref().is_some_and(|scope| *scope != grant_scope) {
                return Err(MaptileError::VersionChanged(
                    "S3 authorization scope".into(),
                ));
            }
            *scope = Some(grant_scope);
            *cached = Some(secret.clone());
            Ok(Some(secret))
        } else {
            Err(MaptileError::Cancelled)
        }
    }

    fn s3_secret(
        url: &str,
        options: GeoParquetS3Options,
        expires_at: Option<SystemTime>,
    ) -> MaptileResult<DuckDbSecret> {
        if options.region.is_empty()
            || options.access_key_id.is_empty() != options.secret_access_key.is_empty()
            || (options.session_token.is_some() && options.access_key_id.is_empty())
            || [
                &options.region,
                &options.access_key_id,
                &options.secret_access_key,
            ]
            .into_iter()
            .chain(options.session_token.as_ref())
            .any(|v| v.chars().any(char::is_control))
        {
            return Err(config("invalid S3 credentials or region"));
        }
        if let Some(endpoint) = &options.endpoint {
            let parsed = url::Url::parse(&format!("https://{endpoint}"))
                .map_err(|_| config("invalid S3 endpoint"))?;
            if endpoint.is_empty()
                || endpoint.trim() != endpoint
                || endpoint.chars().any(char::is_control)
                || endpoint.contains(['/', '\\', '?', '#', '@'])
                || parsed.host_str().is_none()
            {
                return Err(config(
                    "S3 endpoint must contain only a host and optional port",
                ));
            }
        }
        let mut settings = vec![
            "TYPE s3".to_owned(),
            "PROVIDER config".to_owned(),
            format!("SCOPE {}", literal(url)),
            format!("REGION {}", literal(&options.region)),
            format!("KEY_ID {}", literal(&options.access_key_id)),
            format!("SECRET {}", literal(&options.secret_access_key)),
            format!("USE_SSL {}", options.use_ssl),
            format!(
                "URL_STYLE '{}'",
                if options.path_style { "path" } else { "vhost" }
            ),
            "URL_COMPATIBILITY_MODE true".to_owned(),
        ];
        if let Some(endpoint) = &options.endpoint {
            settings.push(format!("ENDPOINT {}", literal(endpoint)));
        }
        if let Some(token) = &options.session_token {
            settings.push(format!("SESSION_TOKEN {}", literal(token)));
        }
        Ok(DuckDbSecret {
            sql: format!(
                "CREATE OR REPLACE SECRET maptile_s3 ({})",
                settings.join(", ")
            ),
            expires_at,
        })
    }

    pub fn configure(
        conn: &Connection,
        timeout_ms: u64,
        secret: Option<&DuckDbSecret>,
    ) -> MaptileResult<()> {
        conn.execute_batch("LOAD httpfs")
            .map_err(|_| MaptileError::BackendMsg(HTTPFS_UNAVAILABLE.into()))?;
        configure_httpfs_connection(conn, timeout_ms)?;
        if let Some(secret) = secret {
            secret.install(conn)?;
        }
        Ok(())
    }
}

fn validate_http_headers(headers: &BTreeMap<String, String>) -> MaptileResult<()> {
    for (name, value) in headers {
        if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || value.chars().any(char::is_control)
            || matches!(
                name.to_ascii_lowercase().as_str(),
                "range"
                    | "host"
                    | "content-length"
                    | "accept-encoding"
                    | "if-match"
                    | "if-range"
                    | "if-unmodified-since"
                    | "connection"
                    | "transfer-encoding"
                    | "proxy-authorization"
                    | "proxy-connection"
            )
        {
            return Err(config("invalid or reserved HTTP header"));
        }
    }
    Ok(())
}

// HTTPFS does not expose a response's ETag for comparison to the requested
// revision. Capture verified ranges once so future SQL reads use the same bytes.
pub(super) async fn capture_http_at_etag(
    url: &str,
    etag: &str,
    headers: &BTreeMap<String, String>,
    limit: usize,
) -> MaptileResult<tempfile::TempPath> {
    use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
    use std::io::Write;

    const RANGE_BYTES: usize = 1024 * 1024;
    let url = reqwest::Url::parse(&validate_url(url, false)?)
        .map_err(|_| config("invalid remote URL"))?;
    if !etag
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .is_some_and(|opaque| opaque.bytes().all(|c| c >= b'!' && c != b'"' && c != 0x7f))
    {
        return Err(config("expected a strong ETag in double quotes"));
    }
    validate_http_headers(headers)?;
    let mut request_headers = HeaderMap::new();
    for (name, value) in headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| config("invalid HTTP header"))?;
        let mut value = HeaderValue::from_str(value).map_err(|_| config("invalid HTTP header"))?;
        value.set_sensitive(true);
        if request_headers.insert(name, value).is_some() {
            return Err(config("duplicate HTTP header"));
        }
    }
    request_headers.insert(
        header::IF_MATCH,
        HeaderValue::from_str(etag).map_err(|_| config("invalid ETag"))?,
    );
    request_headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    let client = reqwest::Client::builder()
        .default_headers(request_headers)
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| config("cannot configure HTTP snapshot client"))?;
    let mut temporary = crate::transport::run_blocking(|| {
        Ok(tempfile::Builder::new().suffix(".parquet").tempfile()?)
    })
    .await?;
    let mut offset = 0usize;
    let mut size: Option<usize> = None;
    loop {
        let end = offset
            .saturating_add(RANGE_BYTES - 1)
            .min(size.unwrap_or(limit).saturating_sub(1));
        let mut response = client
            .get(url.clone())
            .header(header::RANGE, format!("bytes={offset}-{end}"))
            .send()
            .await
            .map_err(snapshot_transport_error)?;
        let status = response.status();
        if matches!(status.as_u16(), 404 | 410 | 412) {
            return Err(changed_snapshot());
        }
        if matches!(status.as_u16(), 502 | 503) {
            return Err(MaptileError::SourceUnavailable(
                "GeoParquet snapshot endpoint unavailable".into(),
            ));
        }
        if matches!(status.as_u16(), 408 | 504) {
            return Err(MaptileError::Deadline);
        }
        if status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(MaptileError::BackendMsg(format!(
                "GeoParquet snapshot requires a 206 response, received {status}"
            )));
        }
        let headers = response.headers();
        if response.url() != &url
            || headers.get_all(header::ETAG).iter().count() != 1
            || headers.get(header::ETAG).map(HeaderValue::as_bytes) != Some(etag.as_bytes())
        {
            return Err(changed_snapshot());
        }
        if headers
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(MaptileError::InvalidMetadata(
                "encoded GeoParquet snapshot range".into(),
            ));
        }
        let invalid_range =
            || MaptileError::InvalidMetadata("invalid GeoParquet snapshot range".into());
        let range = headers
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes "))
            .and_then(|v| v.split_once('/'))
            .ok_or_else(invalid_range)?;
        let total: usize = range.1.parse().map_err(|_| invalid_range())?;
        if size.is_some_and(|prior| prior != total) {
            return Err(changed_snapshot());
        }
        if total > limit {
            return Err(MaptileError::PayloadTooLarge { limit });
        }
        if total <= offset || headers.get_all(header::CONTENT_RANGE).iter().count() != 1 {
            return Err(invalid_range());
        }
        let last = end.min(total - 1);
        if range.0 != format!("{offset}-{last}") {
            return Err(invalid_range());
        }
        let next = last + 1;
        while let Some(chunk) = response.chunk().await.map_err(snapshot_transport_error)? {
            if chunk.len() > next - offset {
                return Err(invalid_range());
            }
            offset += chunk.len();
            temporary = crate::transport::run_blocking(move || {
                temporary.write_all(&chunk)?;
                Ok(temporary)
            })
            .await?;
        }
        if offset != next {
            return Err(invalid_range());
        }
        if offset == total {
            break;
        }
        size = Some(total);
    }
    Ok(temporary.into_temp_path())
}

fn changed_snapshot() -> MaptileError {
    MaptileError::VersionChanged("remote GeoParquet object".into())
}

fn snapshot_transport_error(error: reqwest::Error) -> MaptileError {
    if error.is_timeout() {
        MaptileError::Deadline
    } else {
        MaptileError::SourceUnavailable("GeoParquet snapshot transfer failed".into())
    }
}

pub(super) fn configure_httpfs_connection(conn: &Connection, timeout_ms: u64) -> MaptileResult<()> {
    conn.execute_batch(&format!(
        "SET http_proxy = ''; SET http_retries = 0; SET http_timeout = {}; SET force_download = false; SET force_download_threshold = 0; SET auto_fallback_to_full_download = false; SET unsafe_disable_etag_checks = false; SET enable_http_metadata_cache = true; SET enable_external_file_cache = false; SET enable_http_logging = false; SET enable_server_cert_verification = true; SET enable_curl_server_cert_verification = true;",
        timeout_ms.div_ceil(1000)
    )).map_err(|_| config("cannot configure DuckDB HTTPFS"))
}

fn validate_url(value: &str, s3: bool) -> MaptileResult<String> {
    let url = url::Url::parse(value).map_err(|_| config("invalid remote URL"))?;
    if value.trim() != value
        || value.chars().any(char::is_control)
        || value.contains('\\')
        || if s3 {
            url.scheme() != "s3"
        } else {
            !matches!(url.scheme(), "http" | "https")
        }
        || url.host_str().is_none()
        || !value[url.scheme().len()..].starts_with("://")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path().is_empty()
        || url.path().ends_with('/')
        || url.path().contains(['*', '[', ']'])
        || (s3 && (url.query().is_some() || url.port().is_some()))
    {
        return Err(config(
            "expected one remote file without userinfo, backslash, fragment or glob",
        ));
    }
    // HTTPFS requires a lowercase scheme; signed paths and queries stay intact.
    let mut normalized = value.to_owned();
    normalized[..url.scheme().len()].make_ascii_lowercase();
    Ok(normalized)
}

fn config(message: &str) -> MaptileError {
    MaptileError::Config(format!("GeoParquet: {message}"))
}

/// DuckDB diagnostics can contain signed URLs, SQL and remote response bodies.
pub(super) fn remote_error(error: MaptileError) -> MaptileError {
    match error {
        MaptileError::BackendMsg(ref message)
            if matches!(message.as_str(), HTTPFS_UNAVAILABLE | SPATIAL_UNAVAILABLE) =>
        {
            error
        }
        MaptileError::Backend(ref cause)
            if [
                "HTTP 412",
                "HTTP response code 412",
                "Precondition Failed",
                "ETag mismatch",
                "ETag has changed",
                "ETag verification failed",
            ]
            .iter()
            .any(|reason| cause.to_string().contains(reason)) =>
        {
            MaptileError::VersionChanged("remote GeoParquet object".into())
        }
        MaptileError::Backend(_) | MaptileError::BackendMsg(_) => MaptileError::BackendMsg(
            "GeoParquet remote read failed; check the endpoint, credentials, file and query budget"
                .into(),
        ),
        error => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_drops_a_credential_provider_after_unlocking_its_state() {
        use std::sync::{
            OnceLock, Weak,
            atomic::{AtomicBool, Ordering},
        };

        #[derive(Default)]
        struct Provider {
            location: OnceLock<Weak<RemoteLocation>>,
            released: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl GeoParquetCredentialProvider for Provider {
            async fn credentials(&self) -> MaptileResult<GeoParquetS3Credentials> {
                unreachable!("the source is closed before any query")
            }
        }
        impl Drop for Provider {
            fn drop(&mut self) {
                let remote = self.location.get().unwrap().upgrade().unwrap();
                let state = remote
                    .credentials
                    .try_lock()
                    .expect("credential state lock released");
                self.released
                    .store(matches!(*state, Credentials::Closed), Ordering::SeqCst);
            }
        }

        let provider = Arc::new(Provider::default());
        let released = provider.released.clone();
        let remote = Arc::new(
            RemoteLocation::s3_with_provider("s3://bucket/file.parquet", provider.clone()).unwrap(),
        );
        provider.location.set(Arc::downgrade(&remote)).unwrap();
        drop(provider);
        remote.close();
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn remote_locations_reject_non_file_inputs_and_unsafe_headers() {
        for url in [
            "file:///tmp/a",
            "http://user:password@host/a",
            "http://host/a#fragment",
            "http://host/*.parquet",
            "http://host/",
            " http://host/a",
            "http:host/a",
            "http:/host/a",
            "http://127.0.0.1:8000\\@127.0.0.1:9000/data.parquet",
        ] {
            assert!(RemoteLocation::http(url, BTreeMap::new()).is_err(), "{url}");
        }
        for url in [
            "https://bucket/a",
            "s3://bucket/*.parquet",
            "s3://bucket/a?versionId=x",
            "s3://bucket/",
            "s3://bucket/a\\b.parquet",
        ] {
            assert!(
                RemoteLocation::s3(url, GeoParquetS3Options::default()).is_err(),
                "{url}"
            );
        }
        for endpoint in ["host\\path", "host:8000\\@other:9000", "host/path", "host "] {
            let options = GeoParquetS3Options {
                endpoint: Some(endpoint.into()),
                ..Default::default()
            };
            assert!(RemoteLocation::s3("s3://bucket/data.parquet", options).is_err());
        }
        for (name, value) in [
            ("Authorization", "token\r\nInjected: yes"),
            ("Range", "bytes=0-1"),
            ("Bad:Name", "value"),
        ] {
            assert!(
                RemoteLocation::http(
                    "https://host/a",
                    BTreeMap::from([(name.into(), value.into())])
                )
                .is_err()
            );
        }
    }

    #[test]
    fn url_scheme_is_normalized_without_rewriting_signed_bytes() {
        let remote = RemoteLocation::http(
            "HTTP://host/A%2Fb.parquet?signature=A%2Fb+%2f",
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(remote.url, "http://host/A%2Fb.parquet?signature=A%2Fb+%2f");
        let remote = RemoteLocation::s3("S3://bucket/File.parquet", Default::default()).unwrap();
        assert_eq!(remote.url, "s3://bucket/File.parquet");
    }

    #[test]
    fn only_exact_trusted_diagnostics_survive_redaction() {
        use std::error::Error;
        for message in [HTTPFS_UNAVAILABLE, SPATIAL_UNAVAILABLE] {
            let error = remote_error(MaptileError::BackendMsg(message.into()));
            assert!(error.to_string().contains(message));
            for error in [
                MaptileError::BackendMsg(format!("{message}; url?signature=private-secret")),
                MaptileError::backend(std::io::Error::other(
                    "SQL url?signature=private-secret Authorization: token response-body",
                )),
            ] {
                let error = remote_error(error);
                assert!(!format!("{error} {error:?}").contains("private-secret"));
                assert!(error.source().is_none());
            }
        }
    }

    #[test]
    fn missing_httpfs_retains_installation_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let options = duckdb::Config::default()
            .enable_autoload_extension(false)
            .unwrap()
            .with("autoinstall_known_extensions", "false")
            .unwrap()
            .with("extension_directory", dir.path().to_str().unwrap())
            .unwrap();
        let connection = Connection::open_in_memory_with_flags(options).unwrap();
        let error = remote_error(RemoteLocation::configure(&connection, 1000, None).unwrap_err());
        assert!(
            matches!(error, MaptileError::BackendMsg(message) if message == HTTPFS_UNAVAILABLE)
        );
    }

    #[tokio::test]
    async fn concurrent_s3_borrows_share_refresh_and_reject_a_changed_endpoint() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        #[derive(Default)]
        struct Provider {
            calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl GeoParquetCredentialProvider for Provider {
            async fn credentials(&self) -> MaptileResult<GeoParquetS3Credentials> {
                let refresh = self.calls.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
                Ok(GeoParquetS3Credentials {
                    options: GeoParquetS3Options {
                        endpoint: Some(
                            if refresh == 0 {
                                "old.invalid"
                            } else {
                                "new.invalid"
                            }
                            .into(),
                        ),
                        access_key_id: "dummy".into(),
                        secret_access_key: "unused".into(),
                        ..Default::default()
                    },
                    expires_at: SystemTime::now() + Duration::from_secs(5),
                })
            }
        }

        let provider = Arc::new(Provider::default());
        let remote =
            RemoteLocation::s3_with_provider("s3://bucket/data.parquet", provider.clone()).unwrap();
        let grants = futures_util::future::join_all((0..8).map(|_| remote.credentials())).await;
        assert!(grants.iter().all(|grant| grant.is_ok()));
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        if let Credentials::Provider { cached, .. } = &mut *remote.credential_state() {
            *cached = None;
        }
        assert!(matches!(
            remote.credentials().await,
            Err(MaptileError::VersionChanged(_))
        ));
        remote.close();
        assert!(matches!(
            remote.credentials().await,
            Err(MaptileError::Cancelled)
        ));
    }

    #[test]
    fn restricted_session_can_replace_a_secret_without_losing_it_on_error() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("LOAD httpfs; CREATE SECRET dummy (TYPE S3, KEY_ID 'dummy-old', SECRET 'unused', SCOPE 's3://example.invalid/a.parquet'); SET allowed_paths=['s3://example.invalid/a.parquet']; SET enable_external_access=false;").unwrap();
        connection.execute_batch("CREATE OR REPLACE SECRET dummy (TYPE S3, KEY_ID 'dummy-new', SECRET 'unused', SCOPE 's3://example.invalid/a.parquet')").unwrap();
        assert!(
            connection
                .execute_batch(
                    "CREATE OR REPLACE SECRET dummy (TYPE S3, INVALID_PARAMETER 'unused')"
                )
                .is_err()
        );
        let value: String = connection
            .query_row(
                "SELECT secret_string FROM duckdb_secrets() WHERE name='dummy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(value.contains("key_id=dummy-new"));
    }

    #[test]
    fn secrets_are_scoped_escaped_and_not_debugged() {
        let options = GeoParquetS3Options {
            access_key_id: "private-key".into(),
            secret_access_key: "secret'quote".into(),
            session_token: Some("session-token".into()),
            ..Default::default()
        };
        let debug = format!("{options:?}");
        for value in ["private-key", "secret'quote", "session-token"] {
            assert!(!debug.contains(value));
        }
        let sql = RemoteLocation::s3_secret("s3://bucket/data.parquet", options, None)
            .unwrap()
            .sql;
        assert!(sql.contains("SCOPE 's3://bucket/data.parquet'"));
        assert!(sql.contains("SECRET 'secret''quote'"));
        assert!(!sql.contains("PERSISTENT"));
        let error = remote_error(MaptileError::BackendMsg("url?signature=secret".into()));
        assert!(!format!("{error:?}").contains("signature"));
    }
}
