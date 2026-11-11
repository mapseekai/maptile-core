//! Read a world tile directly from FGB, or import a local vector file first.
//! Run geoparquet_extensions --install explicitly before this example.
use maptile_core::sources::{
    FlatGeobufSource, FlatGeobufSourceConfig, GeoParquetSource, GeoParquetSourceConfig, TileCoord,
    TileSource, VectorImportFormat, VectorImportOptions, import_to_geoparquet,
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let usage =
        "usage: local_vector fgb INPUT.fgb | local_vector geojson|shapefile INPUT OUTPUT.parquet";
    let source: Box<dyn TileSource> = match args.as_slice() {
        [format, input] if format == "fgb" => {
            let path = PathBuf::from(input);
            Box::new(
                tokio::task::spawn_blocking(move || {
                    FlatGeobufSource::open(FlatGeobufSourceConfig {
                        path,
                        ..Default::default()
                    })
                })
                .await??,
            )
        }
        [format, input, output] if matches!(format.as_str(), "geojson" | "shapefile") => {
            let format = if format == "geojson" {
                VectorImportFormat::GeoJson
            } else {
                VectorImportFormat::ShapefileZip
            };
            let imported =
                import_to_geoparquet(input, format, output, VectorImportOptions::default()).await?;
            Box::new(
                tokio::task::spawn_blocking(move || {
                    GeoParquetSource::open(GeoParquetSourceConfig {
                        path: imported.path,
                        geometry_column: Some(imported.geometry_column),
                        ..Default::default()
                    })
                })
                .await??,
            )
        }
        _ => return Err(usage.into()),
    };
    let result = source.get_tile(TileCoord { z: 0, x: 0, y: 0 }, None).await;
    source.close().await?;
    println!("{} bytes of MVT", result?.data.len());
    Ok(())
}
