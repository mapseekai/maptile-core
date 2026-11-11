//! Integration tests for `HttpPmtilesSource`.
//!
//! **Range-request fallback**: wiremock 0.6 returns 200 for every request and
//! does not interpret `Range:` headers, but the pmtiles 0.19 HTTP backend
//! *requires* a 206 Partial Content response and returns
//! `PmtError::RangeRequestsUnsupported` otherwise.  We therefore hand-roll a
//! tiny `tokio::net::TcpListener` server that honours `Range:` headers and
//! responds with 206, keeping the rest of the test infrastructure in-process.

use maptile_core::sources::pmtiles::{HttpPmtilesSource, PmtilesConfig};
use maptile_core::sources::{TileCoord, TileFormat, TileSource};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const FIXTURE_PATH: &str = "tests/fixtures/sample.pmtiles";

/// Start a minimal HTTP/1.1 server that serves a single static byte slice.
/// Every `GET` request with a `Range: bytes=X-Y` header receives a `206`
/// response with the requested slice.  Requests without a Range header get
/// the full body with status `200`.  The server shuts down when `shutdown_tx`
/// is dropped.
async fn start_range_server(data: Arc<Vec<u8>>) -> (u16, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                res = listener.accept() => {
                    let (stream, _) = res.unwrap();
                    let data = data.clone();
                    tokio::spawn(handle_connection(stream, data));
                }
            }
        }
    });

    (port, shutdown_tx)
}

async fn handle_connection(stream: tokio::net::TcpStream, data: Arc<Vec<u8>>) {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);

    // Read request headers until blank line.
    let mut range_header: Option<String> = None;
    loop {
        let mut line = String::new();
        if buf_reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("range:") {
            range_header = Some(trimmed["range:".len()..].trim().to_string());
        }
    }

    // Parse optional `bytes=X-Y`.
    let total = data.len();
    let (status, slice): (&str, &[u8]) = if let Some(ref r) = range_header {
        // Strip "bytes=" prefix.
        let r = r.trim_start_matches("bytes=");
        if let Some((start_s, end_s)) = r.split_once('-') {
            let start: usize = start_s.parse().unwrap_or(0);
            let end: usize = end_s.parse().unwrap_or(total.saturating_sub(1));
            let end = end.min(total.saturating_sub(1));
            ("206 Partial Content", &data[start..=end])
        } else {
            ("200 OK", &data[..])
        }
    } else {
        ("200 OK", &data[..])
    };

    let content_len = slice.len();
    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {content_len}\r\n\
         Accept-Ranges: bytes\r\n\
         Connection: close\r\n\
         \r\n"
    );
    let _ = writer.write_all(header.as_bytes()).await;
    let _ = writer.write_all(slice).await;
}

#[tokio::test]
async fn open_and_get_tile_over_http() {
    assert!(
        std::path::Path::new(FIXTURE_PATH).exists(),
        "Run pmtiles_local first to generate {FIXTURE_PATH}"
    );
    let bytes = tokio::fs::read(FIXTURE_PATH).await.unwrap();
    let data = Arc::new(bytes);

    let (port, _shutdown) = start_range_server(data).await;

    let cfg = PmtilesConfig {
        id: "h".into(),
        path: format!("http://127.0.0.1:{port}/sample.pmtiles"),
        serve_as: None,
        description: None,
    };

    let src = HttpPmtilesSource::open(cfg).await.expect("open");
    assert_eq!(src.id(), "h");
    assert_eq!(src.tile_info().format, TileFormat::Mvt);

    let resp = src
        .get_tile(TileCoord { z: 0, x: 0, y: 0 }, None)
        .await
        .unwrap();
    assert!(!resp.data.is_empty(), "tile data should be non-empty");
}
