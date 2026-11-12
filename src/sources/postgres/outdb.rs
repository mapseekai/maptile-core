//! PostgreSQL Out-of-Database raster source implementation.
//!
//! This source queries PostgreSQL to get file paths to VRT/COG files,
//! then uses GDAL to render tiles from those files.

use async_trait::async_trait;
use bytes::Bytes;
use gdal::spatial_ref::SpatialRef;
use gdal::{Dataset, DriverManager};
use image::RgbaImage;
use std::sync::Arc;
use tilejson::{TileJSON, tilejson};
use tokio_postgres::types::Type;

use super::PostgresPool;
use super::outdb_types::PostgresOutDbRasterConfig;
use crate::cache::ContentVersion;
use crate::error::{MaptileError, MaptileResult};
use crate::raster::style::EffectiveStyle;
use crate::raster::style::pipeline;
use crate::sources::{
    TileCoord, TileEncoding, TileFormat, TileInfo, TileQuery, TileResponse, TileSource,
};

#[derive(Debug, Clone)]
struct RasterPathResult {
    filepath: String,
}

type DatasetEntry = crate::sources::dataset_cache::CachedDataset;

const WEB_MERCATOR_EXTENT: f64 = 20_037_508.342_789_244;

fn backend_err(msg: impl Into<String>) -> MaptileError {
    MaptileError::BackendMsg(msg.into())
}

#[derive(Clone)]
pub struct PostgresOutDbRasterSource {
    pool: Arc<PostgresPool>,
    id: String,
    tj: TileJSON,
    minzoom: u8,
    maxzoom: u8,
    schema: String,
    function: String,
    sql_query: String,
    cache_zoom: crate::cache::CacheZoomRange,
    content_version: ContentVersion,
}

impl std::fmt::Debug for PostgresOutDbRasterSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresOutDbRasterSource")
            .field("id", &self.id)
            .field("schema", &self.schema)
            .field("function", &self.function)
            .finish()
    }
}

impl PostgresOutDbRasterSource {
    pub async fn new(
        pool: Arc<PostgresPool>,
        config: &PostgresOutDbRasterConfig,
    ) -> MaptileResult<Self> {
        let conn = pool.get().await?;

        let function_name = config.function.as_ref().unwrap_or(&config.id);

        Self::verify_function(&conn, &config.schema, function_name).await?;

        let sql_query = format!(
            "SELECT * FROM \"{}\".\"{}\"($1::integer, $2::integer, $3::integer, $4::geometry, $5::jsonb)",
            config.schema, function_name
        );

        let mut tj: TileJSON = tilejson! { tiles: vec![] };
        if let Some(d) = config.description.as_ref() {
            tj.description = Some(d.clone());
        }
        if let Some(b) = config.bounds {
            tj.bounds = Some(tilejson::Bounds::new(b[0], b[1], b[2], b[3]));
        }
        tj.minzoom = Some(config.minzoom);
        tj.maxzoom = Some(config.maxzoom);

        tracing::info!(
            "Loaded PostgreSQL out-db raster source '{}': {}.{} (zoom {}-{})",
            config.id,
            config.schema,
            function_name,
            config.minzoom,
            config.maxzoom,
        );

        Ok(Self {
            pool,
            id: config.id.clone(),
            tj,
            minzoom: config.minzoom,
            maxzoom: config.maxzoom,
            schema: config.schema.clone(),
            function: function_name.clone(),
            sql_query,
            cache_zoom: config.cache_zoom.unwrap_or_default(),
            content_version: ContentVersion::new_generation(),
        })
    }

    #[must_use]
    pub fn tile_query(&self) -> &str {
        &self.sql_query
    }

    async fn verify_function(
        conn: &deadpool_postgres::Object,
        schema: &str,
        function: &str,
    ) -> MaptileResult<()> {
        let query = r#"
            SELECT 1
            FROM pg_catalog.pg_proc p
            JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
            WHERE n.nspname = $1
              AND p.proname = $2
            LIMIT 1
        "#;

        conn.query_opt(query, &[&schema, &function])
            .await
            .map_err(|e| backend_err(format!("Failed to query function info: {}", e)))?
            .ok_or_else(|| {
                MaptileError::Config(format!("Function {}.{} not found", schema, function))
            })?;

        Ok(())
    }

    fn tile_to_web_mercator_bbox(z: u8, x: u32, y: u32) -> (f64, f64, f64, f64) {
        let n = 2.0_f64.powi(z as i32);
        let tile_size = (2.0 * WEB_MERCATOR_EXTENT) / n;

        let min_x = -WEB_MERCATOR_EXTENT + (x as f64 * tile_size);
        let max_x = min_x + tile_size;
        let max_y = WEB_MERCATOR_EXTENT - (y as f64 * tile_size);
        let min_y = max_y - tile_size;

        (min_x, min_y, max_x, max_y)
    }

    fn bbox_to_wkt(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> String {
        format!(
            "POLYGON(({} {}, {} {}, {} {}, {} {}, {} {}))",
            min_x, min_y, max_x, min_y, max_x, max_y, min_x, max_y, min_x, min_y
        )
    }

    /// Render a tile by dispatching to the shared raster styling pipeline.
    ///
    /// The complete style comes from the SDK document.
    #[allow(clippy::too_many_arguments)]
    pub async fn get_tile_with_params(
        &self,
        z: u8,
        x: u32,
        y: u32,
        query_params: Option<serde_json::Value>,
        style: &EffectiveStyle,
    ) -> MaptileResult<Option<TileResponse>> {
        crate::raster::agtiff_style::to_tile_style(style)?;
        let max_tile = crate::sources::tile_math::tiles_per_axis(z)?;
        if x >= max_tile || y >= max_tile {
            return Err(MaptileError::TileNotFound(z, x, y));
        }

        if z < self.minzoom || z > self.maxzoom {
            return Ok(None);
        }

        let (min_x, min_y, max_x, max_y) = Self::tile_to_web_mercator_bbox(z, x, y);
        let bbox_wkt = Self::bbox_to_wkt(min_x, min_y, max_x, max_y);
        let params = query_params.unwrap_or(serde_json::json!({}));

        let conn = self.pool.get().await?;

        let param_types: &[Type] = &[Type::INT4, Type::INT4, Type::INT4, Type::TEXT, Type::JSONB];

        let prep_query = conn
            .prepare_typed_cached(&self.sql_query, param_types)
            .await
            .map_err(|e| {
                backend_err(format!(
                    "Failed to prepare query for {}.{}: {}",
                    self.schema, self.function, e
                ))
            })?;

        let rows = conn
            .query(
                &prep_query,
                &[&(z as i32), &(x as i32), &(y as i32), &bbox_wkt, &params],
            )
            .await
            .map_err(|e| {
                backend_err(format!(
                    "Failed to execute query for {}.{} at z={}, x={}, y={}: {}",
                    self.schema, self.function, z, x, y, e
                ))
            })?;

        drop(conn);

        if rows.is_empty() {
            return Ok(None);
        }

        let results: Vec<RasterPathResult> = rows
            .iter()
            .filter_map(|row| {
                let filepath = row.try_get::<_, String>("filepath").ok()?;
                Some(RasterPathResult { filepath })
            })
            .collect();

        if results.is_empty() {
            return Ok(None);
        }

        let base_style = style.clone();
        let tile_size = style.tile_size;
        // Resolve all datasets up front (async, may open OFF-lock via
        // spawn_blocking). The blocking renderer below only takes the
        // already-resolved Arcs and locks them with `blocking_lock`.
        let mut entries: Vec<DatasetEntry> = Vec::with_capacity(results.len());
        for result in &results {
            let ds = crate::sources::dataset_cache::global()
                .get_or_open(&result.filepath)
                .await?;
            entries.push(ds);
        }

        Self::render_tile_blocking(entries, min_x, min_y, max_x, max_y, tile_size, base_style).await
    }

    /// Render every selected GDAL dataset with the native style engine and
    /// composite the resulting images in query order.
    #[allow(clippy::too_many_arguments)]
    async fn render_tile_blocking(
        entries: Vec<DatasetEntry>,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
        tile_size: u32,
        base_style: EffectiveStyle,
    ) -> MaptileResult<Option<TileResponse>> {
        crate::transport::run_blocking(move || {
            let output_format = match base_style.format {
                crate::raster::style::RasterOutputFormat::Png => TileFormat::Png,
                crate::raster::style::RasterOutputFormat::Webp => TileFormat::Webp,
                crate::raster::style::RasterOutputFormat::Jpeg => TileFormat::Jpeg,
            };
            let tile_bounds = [min_x, min_y, max_x, max_y];
            let mut composite: Option<RgbaImage> = None;

            for dataset in &entries {
                // `blocking_lock` is sound here: we are inside
                // `tokio::task::spawn_blocking`, NOT on the async runtime.
                let dataset_guard = dataset.blocking_lock();

                let warped = warp_to_tile_mem(
                    &dataset_guard,
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                    tile_size,
                    &base_style,
                )?;

                // Tile bounds == source bounds here, so the edge-mask
                // fast-path triggers (no per-pixel work).
                let encoded = pipeline::render(&warped, tile_size, &base_style, tile_bounds)?;
                drop(dataset_guard);

                // Decode the selected image format to RGBA so we can composite. For the
                // common single-asset case we short-circuit below and
                // return the encoded tile directly without decoding.
                if entries.len() == 1 {
                    return Ok(Some(TileResponse {
                        data: Bytes::from(encoded),
                        info: TileInfo {
                            format: output_format,
                            encoding: TileEncoding::Internal,
                        },
                        last_modified: None,
                    }));
                }

                let img = image::load_from_memory(&encoded)
                    .map_err(|e| backend_err(format!("decode rendered tile: {e}")))?
                    .to_rgba8();

                match &mut composite {
                    Some(c) => {
                        for (x, y, pixel) in img.enumerate_pixels() {
                            if pixel[3] > 0 {
                                c.put_pixel(x, y, *pixel);
                            }
                        }
                    }
                    None => composite = Some(img),
                }
            }

            let Some(composite) = composite else {
                return Ok(None);
            };
            let (w, h) = composite.dimensions();
            let raster = crate::raster::image::RasterImage::from_rgba8(composite.as_raw(), w, h)?;
            let encoded = encode_composite(&raster, &base_style)?;
            Ok(Some(TileResponse {
                data: encoded,
                info: TileInfo {
                    format: output_format,
                    encoding: TileEncoding::Internal,
                },
                last_modified: None,
            }))
        })
        .await
    }
}

fn encode_composite(
    raster: &crate::raster::image::RasterImage,
    style: &EffectiveStyle,
) -> MaptileResult<Bytes> {
    match style.format {
        crate::raster::style::RasterOutputFormat::Png => crate::raster::encode::to_png(raster),
        crate::raster::style::RasterOutputFormat::Webp => crate::raster::encode::to_webp(raster),
        crate::raster::style::RasterOutputFormat::Jpeg => crate::raster::encode::to_jpeg(
            raster,
            style
                .raster_style()
                .as_value()
                .pointer("/raster:tile/quality")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(85) as u8,
        ),
    }
}

/// Warp the source `dataset` into a new in-memory dataset sized
/// `tile_size × tile_size` in EPSG:3857 covering `[min_x, min_y, max_x,
/// max_y]`. Band layout is preserved (each band stays `f64`-typed); the
/// raster-style pipeline handles all per-pixel work downstream.
fn warp_to_tile_mem(
    dataset: &Dataset,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    tile_size: u32,
    style: &EffectiveStyle,
) -> MaptileResult<Dataset> {
    let web_mercator = SpatialRef::from_epsg(3857)
        .map_err(|e| backend_err(format!("Failed to create EPSG:3857: {}", e)))?;

    let mem_driver = DriverManager::get_driver_by_name("MEM")
        .map_err(|e| backend_err(format!("Failed to get MEM driver: {}", e)))?;

    let band_count = dataset.raster_count().max(1);

    let mut warped = mem_driver
        .create_with_band_type::<f64, _>("", tile_size as usize, tile_size as usize, band_count)
        .map_err(|e| backend_err(format!("Failed to create output: {}", e)))?;

    let pixel_size_x = (max_x - min_x) / tile_size as f64;
    let pixel_size_y = (max_y - min_y) / tile_size as f64;
    let geo_transform = [min_x, pixel_size_x, 0.0, max_y, 0.0, -pixel_size_y];

    warped
        .set_geo_transform(&geo_transform)
        .map_err(|e| backend_err(format!("Failed to set geotransform: {}", e)))?;
    warped
        .set_spatial_ref(&web_mercator)
        .map_err(|e| backend_err(format!("Failed to set SRS: {}", e)))?;

    for index in 1..=band_count {
        let mut band = warped.rasterband(index).map_err(MaptileError::backend)?;
        band.set_color_interpretation(
            dataset
                .rasterband(index)
                .map_err(MaptileError::backend)?
                .color_interpretation(),
        )
        .map_err(MaptileError::backend)?;
    }
    pipeline::reproject(dataset, &warped, style)?;

    Ok(warped)
}

#[async_trait]
impl TileSource for PostgresOutDbRasterSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn content_version(&self) -> ContentVersion {
        self.content_version
    }

    fn tilejson(&self) -> &TileJSON {
        &self.tj
    }

    fn tile_info(&self) -> TileInfo {
        TileInfo {
            format: TileFormat::Png,
            encoding: TileEncoding::Internal,
        }
    }

    fn cache_zoom(&self) -> crate::cache::CacheZoomRange {
        self.cache_zoom
    }

    fn clone_source(&self) -> Box<dyn TileSource> {
        Box::new(self.clone())
    }

    async fn get_tile(
        &self,
        xyz: TileCoord,
        query: Option<&TileQuery<'_>>,
    ) -> MaptileResult<TileResponse> {
        let style = query.and_then(|q| q.style).ok_or_else(|| {
            MaptileError::InvalidRequest("canonical raster style is required".into())
        })?;
        match self
            .get_tile_with_params(xyz.z, xyz.x, xyz.y, None, style)
            .await?
        {
            Some(tile) => Ok(tile),
            None => Err(MaptileError::TileNotFound(xyz.z, xyz.x, xyz.y)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_jpeg_is_encoded_as_jpeg() {
        let document = raster_style::Style::from_json(
            r#"{"extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version":"3.0","renderer":{"type":"gray","bidx":[1]},"raster:tile":{"format":"jpeg","quality":90}}"#
        ).unwrap();
        let style = EffectiveStyle::from_raster_style(document).unwrap();
        let raster =
            crate::raster::image::RasterImage::from_rgba8(&[255, 0, 0, 255], 1, 1).unwrap();
        let encoded = encode_composite(&raster, &style).unwrap();
        assert_eq!(
            image::guess_format(&encoded).unwrap(),
            image::ImageFormat::Jpeg
        );
    }

    #[test]
    fn test_tile_to_web_mercator_bbox_z0() {
        let (min_x, min_y, max_x, max_y) =
            PostgresOutDbRasterSource::tile_to_web_mercator_bbox(0, 0, 0);
        assert!((min_x - (-WEB_MERCATOR_EXTENT)).abs() < 1.0);
        assert!((max_x - WEB_MERCATOR_EXTENT).abs() < 1.0);
        assert!((min_y - (-WEB_MERCATOR_EXTENT)).abs() < 1.0);
        assert!((max_y - WEB_MERCATOR_EXTENT).abs() < 1.0);
    }

    #[test]
    fn test_tile_to_web_mercator_bbox_z1() {
        let (min_x, min_y, max_x, _max_y) =
            PostgresOutDbRasterSource::tile_to_web_mercator_bbox(1, 0, 0);
        let tile_width = max_x - min_x;
        let expected_width = WEB_MERCATOR_EXTENT;
        assert!((tile_width - expected_width).abs() < 1.0);
        assert!((min_y - 0.0).abs() < 1.0);
    }

    #[test]
    fn test_tile_to_web_mercator_bbox_z10() {
        let (min_x, min_y, max_x, max_y) =
            PostgresOutDbRasterSource::tile_to_web_mercator_bbox(10, 512, 512);
        let tile_size = max_x - min_x;
        let expected_size = (2.0 * WEB_MERCATOR_EXTENT) / 1024.0;
        assert!((tile_size - expected_size).abs() < 1.0);
        assert!(max_y > min_y);
    }

    #[test]
    fn test_tile_bbox_symmetry() {
        let (min_x, min_y, max_x, max_y) =
            PostgresOutDbRasterSource::tile_to_web_mercator_bbox(2, 1, 1);
        let tile_width = max_x - min_x;
        let tile_height = max_y - min_y;
        assert!((tile_width - tile_height).abs() < 0.001);
    }

    #[test]
    fn test_bbox_to_wkt_format() {
        let wkt = PostgresOutDbRasterSource::bbox_to_wkt(0.0, 0.0, 1.0, 1.0);
        assert!(wkt.starts_with("POLYGON(("));
        assert!(wkt.ends_with("))"));
        assert!(wkt.contains("0 0"));
        assert!(wkt.contains("1 0"));
        assert!(wkt.contains("1 1"));
        assert!(wkt.contains("0 1"));
    }

    #[test]
    fn test_bbox_to_wkt_negative_coords() {
        let wkt = PostgresOutDbRasterSource::bbox_to_wkt(-180.0, -90.0, 180.0, 90.0);
        assert!(wkt.contains("-180 -90"));
        assert!(wkt.contains("180 90"));
    }

    #[test]
    fn test_bbox_to_wkt_web_mercator_extent() {
        let wkt = PostgresOutDbRasterSource::bbox_to_wkt(
            -WEB_MERCATOR_EXTENT,
            -WEB_MERCATOR_EXTENT,
            WEB_MERCATOR_EXTENT,
            WEB_MERCATOR_EXTENT,
        );
        assert!(wkt.starts_with("POLYGON(("));
        assert!(wkt.contains("-20037508"));
        assert!(wkt.contains("20037508"));
    }

    #[test]
    fn test_tile_bbox_coverage_z0() {
        let (min_x, _min_y, max_x, _max_y) =
            PostgresOutDbRasterSource::tile_to_web_mercator_bbox(0, 0, 0);
        let total_width = 2.0 * WEB_MERCATOR_EXTENT;
        let bbox_width = max_x - min_x;
        assert!((bbox_width - total_width).abs() < 1.0);
    }

    #[test]
    fn test_tile_bbox_adjacent_tiles() {
        let (_, _, max_x_0, _) = PostgresOutDbRasterSource::tile_to_web_mercator_bbox(2, 0, 0);
        let (min_x_1, _, _, _) = PostgresOutDbRasterSource::tile_to_web_mercator_bbox(2, 1, 0);
        assert!((max_x_0 - min_x_1).abs() < 0.001);
    }

    #[test]
    fn test_tile_bbox_valid_range() {
        for z in 0u8..5 {
            let max_tile = crate::sources::tile_math::tiles_per_axis(z).unwrap();
            for x in 0..max_tile.min(4) {
                for y in 0..max_tile.min(4) {
                    let (min_x, min_y, max_x, max_y) =
                        PostgresOutDbRasterSource::tile_to_web_mercator_bbox(z, x, y);
                    assert!(min_x < max_x, "min_x should be less than max_x");
                    assert!(min_y < max_y, "min_y should be less than max_y");
                    assert!(min_x >= -WEB_MERCATOR_EXTENT - 1.0);
                    assert!(max_x <= WEB_MERCATOR_EXTENT + 1.0);
                }
            }
        }
    }
}
