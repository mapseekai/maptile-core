//! Read an immutable HTTP/S3 GeoParquet object or capture an HTTP revision by ETag.

use std::collections::BTreeMap;

use maptile_core::sources::{
    GeoParquetS3Options, GeoParquetSource, GeoParquetSourceConfig, TileCoord, TileSource,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--help") {
        println!("geoparquet_remote [--immutable] <https://...|s3://bucket/key> [geometry-column]");
        println!("Use --immutable only for a versioned address confirmed to be immutable.");
        println!(
            "For HTTP without --immutable, provide a strong GEOPARQUET_ETAG, including its quotes."
        );
        println!("S3: AWS_REGION, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN");
        println!("Custom S3: S3_ENDPOINT (host:port), S3_USE_SSL=false, S3_PATH_STYLE=true");
        return Ok(());
    }
    let immutable = args.first().is_some_and(|arg| arg == "--immutable");
    let args = if immutable { &args[1..] } else { &args[..] };
    let url = args.first().ok_or("expected an HTTP(S) URL or S3 URI")?;
    let config = GeoParquetSourceConfig {
        id: "remote".into(),
        geometry_column: Some(args.get(1).cloned().unwrap_or_else(|| "geometry".into())),
        ..Default::default()
    };
    let source = if url.starts_with("s3://") {
        if !immutable {
            return Err("S3 requires a versioned immutable object and --immutable".into());
        }
        GeoParquetSource::open_immutable_s3(
            config,
            url,
            GeoParquetS3Options {
                region: std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into()),
                endpoint: std::env::var("S3_ENDPOINT").ok(),
                access_key_id: std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
                secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
                session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
                use_ssl: std::env::var("S3_USE_SSL").as_deref() != Ok("false"),
                path_style: std::env::var("S3_PATH_STYLE").as_deref() == Ok("true"),
            },
        )
        .await?
    } else if immutable {
        GeoParquetSource::open_immutable_http(config, url).await?
    } else {
        let etag = std::env::var("GEOPARQUET_ETAG")
            .map_err(|_| "set a strong GEOPARQUET_ETAG or pass --immutable")?;
        GeoParquetSource::open_http_at_etag(config, url, &etag, BTreeMap::new()).await?
    };
    let tile = source.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None).await;
    source.close().await?;
    println!("{} bytes of MVT", tile?.data.len());
    Ok(())
}
