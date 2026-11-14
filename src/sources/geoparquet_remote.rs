//! Validated locations and session-local secrets for DuckDB HTTPFS.

use super::geoparquet_geometry::literal;
use crate::error::{MaptileError, MaptileResult};
use duckdb::Connection;
use std::collections::BTreeMap;

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

pub(super) struct RemoteLocation {
    pub url: String,
    secret_sql: String,
}

impl RemoteLocation {
    pub fn http(url: &str, headers: BTreeMap<String, String>) -> MaptileResult<Self> {
        let url = validate_url(url, false)?;
        for (name, value) in &headers {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
                || value.chars().any(char::is_control)
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "range"
                        | "host"
                        | "content-length"
                        | "accept-encoding"
                        | "if-match"
                        | "if-unmodified-since"
                )
            {
                return Err(config("invalid or reserved HTTP header"));
            }
        }
        let headers = headers
            .iter()
            .map(|(k, v)| format!("{}: {}", literal(k), literal(v)))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Self {
            secret_sql: if headers.is_empty() {
                String::new()
            } else {
                format!(
                    "CREATE SECRET maptile_http (TYPE http, SCOPE {}, EXTRA_HTTP_HEADERS MAP {{{headers}}})",
                    literal(&url)
                )
            },
            url,
        })
    }

    pub fn s3(url: &str, options: GeoParquetS3Options) -> MaptileResult<Self> {
        let url = validate_url(url, true)?;
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
            format!("SCOPE {}", literal(&url)),
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
        Ok(Self {
            url,
            secret_sql: format!("CREATE SECRET maptile_s3 ({})", settings.join(", ")),
        })
    }

    pub fn configure(&self, conn: &Connection, timeout_ms: u64) -> MaptileResult<()> {
        conn.execute_batch("LOAD httpfs").map_err(|_| MaptileError::BackendMsg(
            "cannot load DuckDB httpfs; prepare the matching signed extension with geoparquet_extensions".into()
        ))?;
        configure_httpfs_connection(conn, timeout_ms)?;
        if !self.secret_sql.is_empty() {
            conn.execute_batch(&self.secret_sql)
                .map_err(|_| config("cannot configure remote authentication"))?;
        }
        Ok(())
    }
}

pub(super) fn configure_httpfs_connection(conn: &Connection, timeout_ms: u64) -> MaptileResult<()> {
    // HTTPFS settings are connection-local and are not inherited by try_clone.
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
        let remote = RemoteLocation::s3("s3://bucket/data.parquet", options).unwrap();
        assert!(
            remote
                .secret_sql
                .contains("SCOPE 's3://bucket/data.parquet'")
        );
        assert!(remote.secret_sql.contains("SECRET 'secret''quote'"));
        assert!(!remote.secret_sql.contains("PERSISTENT"));
        let error = remote_error(MaptileError::BackendMsg("url?signature=secret".into()));
        assert!(!format!("{error:?}").contains("signature"));
    }
}
