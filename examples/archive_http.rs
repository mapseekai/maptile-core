//! Loopback HTTP adapter showing representation selection outside the canonical cache.
mod common;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use maptile_core::{error::ErrorKind, prelude::*};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
struct App {
    stack: TileStack,
    catalog: Arc<CatalogService>,
    public_base: String,
}

fn public_base(value: &str) -> Result<String, &'static str> {
    let uri: Uri = value.parse().map_err(|_| "invalid public base URL")?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.path() != "/"
        || uri.query().is_some()
    {
        return Err("public base URL must be an HTTP(S) origin");
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn router(app: App) -> Router {
    Router::new()
        .route("/tiles/{z}/{x}/{y}", get(read))
        .route("/tilejson.json", get(tilejson))
        .route("/wmts.xml", get(wmts))
        .with_state(app)
}

async fn tilejson(State(app): State<App>) -> Response {
    let snapshot = app.catalog.catalog().snapshot();
    let Some(descriptor) = snapshot.describe("sample") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut metadata = descriptor.tilejson.clone();
    if matches!(descriptor.info.format, TileFormat::Mvt | TileFormat::Mlt)
        && metadata.vector_layers.is_none()
    {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    metadata.tilejson = "3.0.0".into();
    metadata.tiles = vec![format!("{}/tiles/{{z}}/{{x}}/{{y}}", app.public_base)];
    match serde_json::to_vec(&metadata) {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn wmts(State(app): State<App>) -> Response {
    let snapshot = app.catalog.catalog().snapshot();
    let Some(descriptor) = snapshot.describe("sample") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let metadata = &descriptor.tilejson;
    let service = WmtsService {
        title: "Sample archive".into(),
        abstract_: None,
        service_url: format!("{}/wmts.xml", app.public_base),
    };
    let layer = WmtsLayer {
        identifier: descriptor.id.to_string(),
        title: metadata
            .name
            .clone()
            .unwrap_or_else(|| descriptor.id.to_string()),
        abstract_: metadata.description.clone(),
        tile_url_template: format!(
            "{}/tiles/{{TileMatrix}}/{{TileCol}}/{{TileRow}}",
            app.public_base
        ),
        format: descriptor.info.format.content_type().into(),
        bounding_box: metadata.bounds.map(|b| [b.left, b.bottom, b.right, b.top]),
        min_zoom: metadata.minzoom.unwrap_or(0),
        max_zoom: metadata.maxzoom.unwrap_or(22).min(22),
    };
    (
        [(header::CONTENT_TYPE, "application/xml")],
        build_wmts_capabilities(&service, &[layer]),
    )
        .into_response()
}

fn selected_encoding(headers: &HeaderMap) -> Option<TileEncoding> {
    let mut gzip = None;
    let mut identity = None;
    let mut wildcard = None;
    for header in headers.get_all(header::ACCEPT_ENCODING) {
        for item in header.to_str().ok()?.split(',') {
            let mut parts = item.trim().split(';');
            let name = parts.next()?.trim();
            let mut q = 1.0f32;
            for part in parts {
                if let Some((parameter, value)) = part.trim().split_once('=')
                    && parameter.trim().eq_ignore_ascii_case("q")
                {
                    q = value
                        .trim()
                        .parse()
                        .ok()
                        .filter(|q| (0.0..=1.0).contains(q))
                        .unwrap_or(0.0);
                }
            }
            if name.eq_ignore_ascii_case("gzip") {
                gzip = Some(q);
            } else if name.eq_ignore_ascii_case("identity") {
                identity = Some(q);
            } else if name == "*" {
                wildcard = Some(q);
            }
        }
    }
    let gzip = gzip.or(wildcard).unwrap_or(0.0);
    let identity = identity.unwrap_or(if wildcard == Some(0.0) { 0.0 } else { 1.0 });
    if gzip > 0.0 && gzip >= identity {
        Some(TileEncoding::Gzip)
    } else if identity > 0.0 {
        Some(TileEncoding::Uncompressed)
    } else {
        None
    }
}

fn accepts(headers: &HeaderMap, content_type: &str) -> bool {
    if !headers.contains_key(header::ACCEPT) {
        return true;
    }
    let (kind, subtype) = content_type.split_once('/').unwrap();
    let mut best: Option<(u8, f32)> = None;
    for value in headers.get_all(header::ACCEPT) {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for item in value.split(',') {
            let mut parts = item.trim().split(';');
            let Some((accepted_kind, accepted_subtype)) =
                parts.next().and_then(|p| p.trim().split_once('/'))
            else {
                continue;
            };
            let specificity = if accepted_kind == "*" && accepted_subtype == "*" {
                0
            } else if accepted_kind.eq_ignore_ascii_case(kind) && accepted_subtype == "*" {
                1
            } else if accepted_kind.eq_ignore_ascii_case(kind)
                && accepted_subtype.eq_ignore_ascii_case(subtype)
            {
                2
            } else {
                continue;
            };
            let quality = match parts.next().map(str::trim) {
                None => 1.0,
                Some(parameter) => parameter
                    .split_once('=')
                    .filter(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                    .and_then(|(_, value)| value.trim().parse::<f32>().ok())
                    .filter(|q| (0.0..=1.0).contains(q))
                    .unwrap_or(0.0),
            };
            if best
                .is_none_or(|(rank, q)| specificity > rank || (specificity == rank && quality > q))
            {
                best = Some((specificity, quality));
            }
        }
    }
    best.is_some_and(|(_, q)| q > 0.0)
}

async fn read(
    State(app): State<App>,
    Path((z, x, y)): Path<(u8, u32, u32)>,
    headers: HeaderMap,
) -> Response {
    let Some(encoding) = selected_encoding(&headers) else {
        return (
            StatusCode::NOT_ACCEPTABLE,
            [(header::VARY, "Accept, Accept-Encoding")],
        )
            .into_response();
    };
    let outcome = match app
        .stack
        .read(ReadTarget::new("sample", "public").at(TileCoord { z, x, y }))
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            return match e.kind() {
                ErrorKind::NotFound => StatusCode::NOT_FOUND,
                ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
                ErrorKind::Unsupported => StatusCode::UNPROCESSABLE_ENTITY,
                ErrorKind::ResourceLimit => StatusCode::PAYLOAD_TOO_LARGE,
                ErrorKind::Deadline => StatusCode::GATEWAY_TIMEOUT,
                ErrorKind::VersionChanged => StatusCode::CONFLICT,
                ErrorKind::Unavailable | ErrorKind::Saturated => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            }
            .into_response();
        }
    };
    let partial = outcome.is_partial();
    let mut response = Response::builder().header(header::VARY, "Accept, Accept-Encoding");
    if partial {
        response = response.header(header::CACHE_CONTROL, "no-store");
    }
    let Some(tile) = outcome.tile() else {
        return response
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap();
    };
    if !accepts(&headers, tile.info.format.content_type()) {
        return response
            .status(StatusCode::NOT_ACCEPTABLE)
            .body(Body::empty())
            .unwrap();
    }
    let raw =
        match maptile_core::transport::decode_async(tile.data.clone(), tile.info.encoding).await {
            Ok(bytes) => bytes,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    let bytes = match maptile_core::transport::encode_as_async(raw, encoding).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    response = response.header(header::CONTENT_TYPE, tile.info.format.content_type());
    if let Some(coding) = encoding.compression() {
        response = response.header(header::CONTENT_ENCODING, coding);
    }
    if !partial {
        let mut hash = blake3::Hasher::new();
        hash.update(tile.info.format.content_type().as_bytes());
        hash.update(&[encoding as u8]);
        hash.update(&bytes);
        let tag = format!("\"{}\"", hash.finalize().to_hex());
        response = response.header(header::ETAG, &tag);
        if headers
            .get_all(header::IF_NONE_MATCH)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|s| s.split(','))
            .map(str::trim)
            .any(|s| s == "*" || s.strip_prefix("W/").unwrap_or(s) == tag)
        {
            return response
                .status(StatusCode::NOT_MODIFIED)
                .body(Body::empty())
                .unwrap();
        }
    }
    response.body(Body::from(bytes)).unwrap()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let base = args
        .next()
        .ok_or("usage: archive_http <public-origin> [archive.pmtiles]")?;
    let base = public_base(base.to_str().ok_or("invalid public base URL")?)?;
    let path = args.next().map_or_else(common::fixture, PathBuf::from);
    if args.next().is_some() {
        return Err("usage: archive_http <public-origin> [archive.pmtiles]".into());
    }
    let catalog = common::catalog(&path).await?;
    let app = App {
        stack: TileStack::builder(catalog.clone()).build(),
        catalog,
        public_base: base,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("Listening on http://{}/tiles/0/0/0", listener.local_addr()?);
    axum::serve(listener, router(app)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use std::sync::Mutex;
    use tower::ServiceExt;
    struct Record(&'static str, Arc<Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl TileLayer for Record {
        async fn read(
            &self,
            _: &ValidatedTileRequest<'_>,
            next: TileService<'_>,
        ) -> MaptileResult<TileOutcome> {
            self.1.lock().unwrap().push(format!("{} in", self.0));
            let result = next.read().await;
            self.1.lock().unwrap().push(format!("{} out", self.0));
            result
        }
    }

    struct Partial(bool);
    #[async_trait::async_trait]
    impl TileLayer for Partial {
        async fn read(
            &self,
            _: &ValidatedTileRequest<'_>,
            next: TileService<'_>,
        ) -> MaptileResult<TileOutcome> {
            let tile = if self.0 {
                next.read().await?.tile().cloned()
            } else {
                None
            };
            Ok(TileOutcome::Partial {
                tile,
                failures: vec![MemberFailure {
                    source: "missing".into(),
                    error: MaptileError::SourceUnavailable("missing".into()),
                }],
            })
        }
    }

    async fn app(layers: bool) -> (Router, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let catalog = common::catalog(&common::fixture()).await.unwrap();
        let builder = TileStack::builder(catalog.clone());
        let stack = if layers {
            builder
                .layer(Record("first", calls.clone()))
                .layer(Record("second", calls.clone()))
                .build()
        } else {
            builder.build()
        };
        (
            router(App {
                stack,
                catalog,
                public_base: public_base("https://tiles.example.com").unwrap(),
            }),
            calls,
        )
    }

    #[tokio::test]
    async fn conditional_and_ordinary_requests_share_complete_archive_bytes() {
        for layered in [false, true] {
            let (app, calls) = app(layered).await;
            let req = |encoding: &str, tag: &str| {
                Request::builder()
                    .uri("/tiles/0/0/0")
                    .header(header::ACCEPT_ENCODING, encoding)
                    .header(header::IF_NONE_MATCH, tag)
                    .body(Body::empty())
                    .unwrap()
            };
            let plain = app.clone().oneshot(req("identity", "")).await.unwrap();
            let tag = plain.headers()[header::ETAG].to_str().unwrap().to_owned();
            assert_eq!(
                to_bytes(plain.into_body(), 1024).await.unwrap(),
                &b"test-tile-data"[..]
            );
            let compressed = app.clone().oneshot(req("gzip", &tag)).await.unwrap();
            assert_eq!(compressed.status(), StatusCode::OK);
            assert_ne!(compressed.headers()[header::ETAG], tag);
            assert_eq!(compressed.headers()[header::CONTENT_ENCODING], "gzip");
            assert_eq!(
                compressed.headers()[header::VARY],
                "Accept, Accept-Encoding"
            );
            let gz = to_bytes(compressed.into_body(), 1024).await.unwrap();
            assert_eq!(
                decode_payload(&gz, TileEncoding::Gzip).unwrap(),
                &b"test-tile-data"[..]
            );
            let matched = app.clone().oneshot(req("identity", &tag)).await.unwrap();
            assert_eq!(matched.status(), StatusCode::NOT_MODIFIED);
            assert!(
                to_bytes(matched.into_body(), 1024)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let again = app.oneshot(req("identity", "")).await.unwrap();
            assert_eq!(
                to_bytes(again.into_body(), 1024).await.unwrap(),
                &b"test-tile-data"[..]
            );
            if layered {
                assert_eq!(
                    *calls.lock().unwrap(),
                    ["first in", "second in", "second out", "first out"].repeat(4)
                );
            }
        }
    }

    #[tokio::test]
    async fn partial_results_never_return_etags_or_304() {
        for present in [false, true] {
            let catalog = common::catalog(&common::fixture()).await.unwrap();
            let stack = TileStack::builder(catalog.clone())
                .layer(Partial(present))
                .build();
            let app = router(App {
                stack,
                catalog,
                public_base: public_base("https://tiles.example.com").unwrap(),
            });
            for tag in ["", "*"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri("/tiles/0/0/0")
                            .header(header::IF_NONE_MATCH, tag)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    if present {
                        StatusCode::OK
                    } else {
                        StatusCode::NO_CONTENT
                    }
                );
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                assert!(!response.headers().contains_key(header::ETAG));
                let body = to_bytes(response.into_body(), 1024).await.unwrap();
                assert_eq!(
                    body.as_ref(),
                    if present {
                        &b"test-tile-data"[..]
                    } else {
                        &b""[..]
                    }
                );
            }
        }
    }

    #[tokio::test]
    async fn metadata_uses_configured_public_origin() {
        let (app, _) = app(false).await;
        let request = |path| {
            Request::builder()
                .uri(path)
                .header(header::HOST, "attacker.example")
                .body(Body::empty())
                .unwrap()
        };
        let response = app
            .clone()
            .oneshot(request("/tilejson.json"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(json["tilejson"], "3.0.0");
        assert_eq!(
            json["tiles"][0],
            "https://tiles.example.com/tiles/{z}/{x}/{y}"
        );
        assert_eq!(json["vector_layers"][0]["id"], "sample");
        assert!(json["vector_layers"][0]["fields"].is_object());
        let response = app.clone().oneshot(request("/wmts.xml")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/xml");
        let xml = String::from_utf8(
            to_bytes(response.into_body(), 65536)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(xml.contains("https://tiles.example.com/tiles/{TileMatrix}/{TileCol}/{TileRow}"));
        assert!(xml.contains("https://tiles.example.com/wmts.xml"));
        assert!(!xml.contains("attacker.example"));
        assert_eq!(
            app.oneshot(request("/tiles/0/0/0")).await.unwrap().status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn accept_matches_the_selected_content_type() {
        let (app, _) = app(false).await;
        for (accept, status) in [
            ("application/*", StatusCode::OK),
            ("image/png", StatusCode::NOT_ACCEPTABLE),
            (
                "*/*;q=1, application/x-protobuf;q=0",
                StatusCode::NOT_ACCEPTABLE,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/tiles/0/0/0")
                        .header(header::ACCEPT, accept)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::VARY], "Accept, Accept-Encoding");
        }
    }

    #[test]
    fn public_origin_must_be_absolute_and_have_no_path() {
        for invalid in [
            "/relative",
            "ftp://example.com",
            "http://example.com/path",
            "https://example.com/?q=x",
            "https://user@example.com",
        ] {
            assert!(public_base(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            public_base("https://tiles.example.com/").unwrap(),
            "https://tiles.example.com"
        );
    }
}
