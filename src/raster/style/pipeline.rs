//! Render GDAL pixels with the same Raster Style engine used by COG sources.

use async_geotiff::{
    bounds::{Bbox, GeoTransform},
    crs::Crs,
    pixel::{ImageData, PixelData},
};
use gdal::Dataset;
use ndarray::{Array2, Array3};

use super::EffectiveStyle;
use crate::error::{MaptileError, MaptileResult};

pub fn render(
    dataset: &Dataset,
    tile_size: u32,
    style: &EffectiveStyle,
    bounds_3857: [f64; 4],
) -> MaptileResult<Vec<u8>> {
    // The GDAL warp has already consumed source NoData into NaN. Reapplying a
    // numeric sentinel after interpolation could discard a valid interpolated value.
    let mut document = style.raster_style().as_value().clone();
    document
        .as_object_mut()
        .expect("validated document")
        .remove("nodata");
    let tile_style = async_geotiff::style::TileStyle::from_json(&document.to_string())
        .map_err(|e| MaptileError::InvalidStyle(e.to_string()))?;
    let compiled = async_geotiff::style::compile(&tile_style)
        .map_err(|e| MaptileError::InvalidStyle(e.to_string()))?;
    if style
        .raster_style()
        .as_value()
        .get("raster:mosaic")
        .is_some()
    {
        return Err(MaptileError::InvalidStyle(
            "mosaic styles require the async-geotiff mosaic source".into(),
        ));
    }
    let size = tile_size as usize;
    let mut pixels = Array3::zeros((dataset.raster_count(), size, size));
    for i in 1..=dataset.raster_count() {
        let band = dataset.rasterband(i).map_err(MaptileError::backend)?;
        if band.color_interpretation() == gdal::raster::ColorInterpretation::AlphaBand {
            return Err(MaptileError::InvalidStyle(
                "source Alpha bands are not supported by async-geotiff".into(),
            ));
        }
        let buffer = band
            .read_as::<f64>((0, 0), dataset.raster_size(), (size, size), None)
            .map_err(MaptileError::backend)?;
        let nodata = band.no_data_value();
        for (target, value) in pixels
            .index_axis_mut(ndarray::Axis(0), i - 1)
            .iter_mut()
            .zip(buffer.data())
        {
            *target = if nodata == Some(*value) {
                f64::NAN
            } else {
                *value
            };
        }
    }
    let transform = dataset.geo_transform().map_err(MaptileError::backend)?;
    let right = transform[0] + transform[1] * size as f64;
    let bottom = transform[3] + transform[5] * size as f64;
    let mask = Array2::from_shape_fn((size, size), |(y, x)| {
        let cx = transform[0] + (x as f64 + 0.5) * transform[1];
        let cy = transform[3] + (y as f64 + 0.5) * transform[5];
        cx >= bounds_3857[0] && cx <= bounds_3857[2] && cy >= bounds_3857[1] && cy <= bounds_3857[3]
    });
    let image = ImageData {
        pixels: PixelData::F64(pixels),
        mask: Some(mask),
        nodata: None,
        bbox: Bbox::new(transform[0], bottom, right, transform[3]),
        crs: Crs::epsg(3857),
        transform: GeoTransform::from_gdal(&transform),
    };
    async_geotiff::render::render(&image, &compiled)
        .map(|bytes| bytes.to_vec())
        .map_err(crate::raster::agtiff_style::render_error)
}

/// GDAL's convenience wrapper fixes bilinear; choose the document's warp kernel.
pub(crate) fn reproject(
    source: &Dataset,
    target: &Dataset,
    style: &EffectiveStyle,
) -> MaptileResult<()> {
    use gdal_sys::GDALResampleAlg::*;
    let kernel = match style
        .raster_style()
        .as_value()
        .pointer("/resampling/reproject")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("nearest")
    {
        "nearest" => GRA_NearestNeighbour,
        "bilinear" => GRA_Bilinear,
        "cubic" => GRA_Cubic,
        "cubic_spline" => GRA_CubicSpline,
        "lanczos" => GRA_Lanczos,
        "average" => GRA_Average,
        value => {
            return Err(MaptileError::InvalidStyle(format!(
                "unsupported reprojection kernel: {value}"
            )));
        }
    };
    let override_nodata = style
        .raster_style()
        .as_value()
        .get("nodata")
        .and_then(|v| v.as_f64().or_else(|| (v == "nan").then_some(f64::NAN)));
    let mut source_nodata = (1..=source.raster_count())
        .map(|i| {
            Ok(override_nodata
                .or(source
                    .rasterband(i)
                    .map_err(MaptileError::backend)?
                    .no_data_value())
                .unwrap_or(f64::NAN))
        })
        .collect::<MaptileResult<Vec<_>>>()?;
    let mut target_nodata = vec![f64::NAN; source.raster_count()];
    for index in 1..=target.raster_count() {
        let mut band = target.rasterband(index).map_err(MaptileError::backend)?;
        band.set_no_data_value(Some(f64::NAN))
            .map_err(MaptileError::backend)?;
        band.fill(f64::NAN, None).map_err(MaptileError::backend)?;
    }
    // SAFETY: both borrowed datasets keep their GDAL handles alive throughout
    // this synchronous call. The options allocation is checked before dereference;
    // default band mapping and both stable NoData buffers use source.raster_count().
    // Clear the borrowed buffer pointers before GDALDestroyWarpOptions so GDAL
    // only frees its own allocations, never the Rust-owned vectors.
    let result = unsafe {
        let options = gdal_sys::GDALCreateWarpOptions();
        if options.is_null() {
            return Err(MaptileError::BackendMsg(
                "cannot allocate GDAL warp options".into(),
            ));
        }
        gdal_sys::GDALWarpInitDefaultBandMapping(options, source.raster_count() as i32);
        (*options).padfSrcNoDataReal = source_nodata.as_mut_ptr();
        (*options).padfDstNoDataReal = target_nodata.as_mut_ptr();
        let result = gdal_sys::GDALReprojectImage(
            source.c_dataset(),
            std::ptr::null(),
            target.c_dataset(),
            std::ptr::null(),
            kernel,
            0.0,
            0.0,
            None,
            std::ptr::null_mut(),
            options,
        );
        (*options).padfSrcNoDataReal = std::ptr::null_mut();
        (*options).padfDstNoDataReal = std::ptr::null_mut();
        gdal_sys::GDALDestroyWarpOptions(options);
        result
    };
    if result != gdal_sys::CPLErr::CE_None {
        return Err(MaptileError::BackendMsg(
            "GDAL raster reprojection failed".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdal_pixels_use_native_opacity_nodata_and_output_format() {
        let driver = gdal::DriverManager::get_driver_by_name("MEM").unwrap();
        let mut dataset = driver
            .create_with_band_type::<f64, _>("", 64, 64, 1)
            .unwrap();
        dataset
            .set_geo_transform(&[0., 1., 0., 0., 0., -1.])
            .unwrap();
        let mut values = vec![128.; 64 * 64];
        values[0] = -9999.;
        dataset
            .rasterband(1)
            .unwrap()
            .write(
                (0, 0),
                (64, 64),
                &mut gdal::raster::Buffer::new((64, 64), values),
            )
            .unwrap();
        let style = EffectiveStyle::from_raster_style(
            super::super::RasterStyle::from_value(serde_json::json!({
                "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"], "version": "3.0", "renderer": {"type": "gray", "bidx": [1]},
                "nodata": -9999, "opacity": 0.5,
                "raster:tile": {"format": "webp", "lossless": true, "tilesize": 64}
            }))
            .unwrap(),
        )
        .unwrap();
        let crs = gdal::spatial_ref::SpatialRef::from_epsg(3857).unwrap();
        dataset.set_spatial_ref(&crs).unwrap();
        let mut warped = driver
            .create_with_band_type::<f64, _>("", 64, 64, 1)
            .unwrap();
        warped.set_spatial_ref(&crs).unwrap();
        warped
            .set_geo_transform(&dataset.geo_transform().unwrap())
            .unwrap();
        reproject(&dataset, &warped, &style).unwrap();
        let bytes = render(&warped, 64, &style, [0., -64., 64., 0.]).unwrap();
        assert_eq!(&bytes[8..12], b"WEBP");
        let pixels = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(pixels.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(pixels.get_pixel(1, 0).0, [128, 128, 128, 128]);
    }
}
