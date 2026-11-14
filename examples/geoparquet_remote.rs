//! Direct HTTP/S3 GeoParquet reads through DuckDB HTTPFS.

use maptile_core::sources::{
    GeoParquetS3Options, GeoParquetSource, GeoParquetSourceConfig, TileCoord, TileSource,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--help") {
        println!("geoparquet_remote <https://...|s3://bucket/key> [geometry-column]");
        println!("S3: AWS_REGION, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN");
        println!("Custom S3: S3_ENDPOINT (host:port), S3_USE_SSL=false, S3_PATH_STYLE=true");
        return Ok(());
    }
    let url = args.first().ok_or("expected an HTTP(S) URL or S3 URI")?;
    let config = GeoParquetSourceConfig {
        id: "remote".into(),
        geometry_column: Some(args.get(1).cloned().unwrap_or_else(|| "geometry".into())),
        ..Default::default()
    };
    let source = if url.starts_with("s3://") {
        GeoParquetSource::open_s3(
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
    } else {
        GeoParquetSource::open_http(config, url).await?
    };
    let tile = source.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None).await;
    source.close().await?;
    println!("{} bytes of MVT", tile?.data.len());
    Ok(())
}
