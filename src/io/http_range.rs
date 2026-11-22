//! Transport checks shared by byte-range readers.

#[cfg(any(feature = "pmtiles", feature = "agtiff"))]
pub(crate) fn strong_http_etag(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    use reqwest::header::ETAG;
    let mut values = headers.get_all(ETAG).iter();
    let etag = values.next()?.to_str().ok()?;
    (values.next().is_none()
        && etag.len() >= 2
        && etag.starts_with('"')
        && etag.ends_with('"')
        && etag[1..etag.len() - 1]
            .bytes()
            .all(|byte| byte >= 0x21 && byte != b'"' && byte != 0x7f))
    .then_some(etag)
}

pub(crate) fn has_identity_content_encoding(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .all(|value| {
            value
                .as_bytes()
                .split(|&byte| byte == b',')
                .all(|coding| coding.trim_ascii().eq_ignore_ascii_case(b"identity"))
        })
}

pub(crate) fn range_http_client_builder() -> reqwest::ClientBuilder {
    use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    // Keep Content-Encoding visible when downstream crates enable reqwest's codecs.
    reqwest::Client::builder()
        .default_headers(headers)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
}
