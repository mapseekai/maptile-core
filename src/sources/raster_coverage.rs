//! Bounded native COG coverage export, independent of rendering styles.
use crate::error::{MaptileError, MaptileResult};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const MAX_COVERAGE_PIXELS: u64 = 4_194_304;
pub const MAX_COVERAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CoverageQuery {
    pub bounds: [f64; 4],
    pub crs: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CoverageMetadata {
    pub crs: String,
    pub bounds: [f64; 4],
    pub width: u32,
    pub height: u32,
    pub bands: usize,
    pub sample_type: String,
    pub nodata: Option<String>,
}

pub struct CoverageResponse {
    pub metadata: CoverageMetadata,
    pub data: Bytes,
}

fn invalid(message: &str) -> MaptileError {
    MaptileError::InvalidRequest(message.into())
}

impl CoverageQuery {
    pub fn validate(&self) -> MaptileResult<()> {
        if self.width == 0
            || self.height == 0
            || u64::from(self.width) * u64::from(self.height) > MAX_COVERAGE_PIXELS
            || self.bounds.iter().any(|v| !v.is_finite())
            || !(self.bounds[2] - self.bounds[0]).is_finite()
            || !(self.bounds[3] - self.bounds[1]).is_finite()
            || self.bounds[0] >= self.bounds[2]
            || self.bounds[1] >= self.bounds[3]
        {
            return Err(invalid("invalid coverage bounds or output dimensions"));
        }
        Ok(())
    }
}

#[cfg(feature = "agtiff")]
pub(crate) fn supports_coverage(reader: &async_geotiff::io::CogReader) -> bool {
    let Ok((width, height)) = reader.level_dimensions(0) else {
        return false;
    };
    let tr = reader.transform();
    let origin = tr.forward(0., 0.);
    let right = tr.forward(f64::from(width), 0.);
    let bottom = tr.forward(0., f64::from(height));
    cfg!(feature = "raster")
        && width > 0
        && height > 0
        && [origin.0, origin.1, right.0, right.1, bottom.0, bottom.1]
            .iter()
            .all(|v| v.is_finite())
        && right.1 == origin.1
        && bottom.0 == origin.0
        && right.0 > origin.0
        && bottom.1 < origin.1
}

#[cfg(feature = "raster")]
pub(crate) async fn read_coverage(
    reader: std::sync::Arc<async_geotiff::io::CogReader>,
    query: Option<&CoverageQuery>,
) -> MaptileResult<CoverageResponse> {
    use async_geotiff::{bounds::Bbox, pixel::PixelData};
    use std::sync::{Arc, OnceLock};
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let permit = SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| MaptileError::CapacityExhausted)?;
    let (width, height) = reader.level_dimensions(0).map_err(MaptileError::backend)?;
    let tr = reader.transform();
    let origin = tr.forward(0., 0.);
    let right = tr.forward(f64::from(width), 0.);
    let bottom = tr.forward(0., f64::from(height));
    if right.1 != origin.1 || bottom.0 != origin.0 || right.0 <= origin.0 || bottom.1 >= origin.1 {
        return Err(invalid("coverage requires a north-up native grid"));
    }
    let bounds = [origin.0, bottom.1, right.0, origin.1];
    let crs = reader.src_crs().as_str().to_string();
    // CogReader validates every declared block against max_decoded_block_bytes
    // at open and reserves its shared WorkBudget before decoding even a 1x1 window.
    let sample = reader
        .read_window(0, 0, 0, 1, 1)
        .await
        .map_err(MaptileError::backend)?;
    let (sample_type, sample_bytes) = match &sample.pixels {
        PixelData::U8(_) => ("UInt8", 1),
        PixelData::U16(_) => ("UInt16", 2),
        PixelData::I16(_) => ("Int16", 2),
        PixelData::U32(_) => ("UInt32", 4),
        PixelData::I32(_) => ("Int32", 4),
        PixelData::F32(_) => ("Float32", 4),
        PixelData::F64(_) => ("Float64", 8),
    };
    let metadata = CoverageMetadata {
        crs,
        bounds,
        width,
        height,
        bands: sample.pixels.bands(),
        sample_type: sample_type.into(),
        nodata: sample.nodata.map(|v| v.to_string()),
    };
    let Some(query) = query else {
        return Ok(CoverageResponse {
            metadata,
            data: Bytes::new(),
        });
    };
    query.validate()?;
    if query.crs != metadata.crs {
        return Err(invalid("only the native coverage CRS is supported"));
    }
    let intersection = [
        query.bounds[0].max(bounds[0]),
        query.bounds[1].max(bounds[1]),
        query.bounds[2].min(bounds[2]),
        query.bounds[3].min(bounds[3]),
    ];
    if intersection[0] >= intersection[2] || intersection[1] >= intersection[3] {
        return Err(invalid(
            "coverage bounds do not intersect the native extent",
        ));
    }
    let (x, y, w, h) = tr
        .window_for_bbox(&Bbox::new(
            intersection[0],
            intersection[1],
            intersection[2],
            intersection[3],
        ))
        .ok_or_else(|| MaptileError::InvalidMetadata("coverage grid is not invertible".into()))?;
    // Inverse-transform rounding can extend an edge by one pixel even after
    // clipping the geographic bounds. CogReader requires an in-bounds window.
    let x_end = x.saturating_add_unsigned(w).clamp(0, i64::from(width));
    let y_end = y.saturating_add_unsigned(h).clamp(0, i64::from(height));
    let x = x.clamp(0, i64::from(width));
    let y = y.clamp(0, i64::from(height));
    let (w, h) = ((x_end - x) as u64, (y_end - y) as u64);
    if w == 0 || h == 0 {
        return Err(invalid("coverage bounds do not intersect native pixels"));
    }
    let bytes_per_pixel = metadata
        .bands
        .checked_mul(sample_bytes)
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| MaptileError::InvalidMetadata("coverage size overflow".into()))?;
    if w.saturating_mul(h) > MAX_COVERAGE_PIXELS
        || w.saturating_mul(h).saturating_mul(bytes_per_pixel as u64)
            > (MAX_COVERAGE_BYTES / 2) as u64
        || u64::from(query.width) * u64::from(query.height) * bytes_per_pixel as u64
            > (MAX_COVERAGE_BYTES / 2) as u64
    {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_COVERAGE_BYTES,
        });
    }
    let pixels = reader
        .read_window(0, x, y, w, h)
        .await
        .map_err(MaptileError::backend)?;
    let query = query.clone();
    let scales = reader
        .band_scale_offset()
        .iter()
        .map(|v| (v.scale, v.offset))
        .collect::<Vec<_>>();
    let data = crate::transport::run_blocking(move || {
        let _permit = permit;
        encode_geotiff(&pixels, &query, &scales)
    })
    .await?;
    Ok(CoverageResponse { metadata, data })
}

#[cfg(feature = "raster")]
fn encode_geotiff(
    image: &async_geotiff::pixel::ImageData,
    query: &CoverageQuery,
    scales: &[(f64, f64)],
) -> MaptileResult<Bytes> {
    use async_geotiff::pixel::PixelData;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = format!(
        "/vsimem/mapseek-coverage-{}-{}.tif",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let _cleanup = gdal::vsi::MemFileRef::new(std::path::Path::new(&path));
    let inverse = image
        .transform
        .inverse()
        .ok_or_else(|| MaptileError::InvalidMetadata("coverage grid is not invertible".into()))?;
    let width = query.width as usize;
    let height = query.height as usize;
    let dx = (query.bounds[2] - query.bounds[0]) / f64::from(query.width);
    let dy = (query.bounds[3] - query.bounds[1]) / f64::from(query.height);
    let mut indices = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let (col, row) = inverse.forward(
                query.bounds[0] + (x as f64 + 0.5) * dx,
                query.bounds[3] - (y as f64 + 0.5) * dy,
            );
            if !col.is_finite()
                || !row.is_finite()
                || col < 0.
                || row < 0.
                || col >= image.pixels.width() as f64
                || row >= image.pixels.height() as f64
            {
                indices.push(None);
            } else {
                indices.push(Some((row.floor() as usize, col.floor() as usize)));
            }
        }
    }
    macro_rules! write {
        ($pixels:expr, $kind:ty, $integer:expr) => {{
            let fill = image.nodata.unwrap_or(0.) as $kind;
            if let Some(nodata) = image.nodata {
                if ($integer && (!nodata.is_finite() || fill as f64 != nodata))
                    || (!$integer && nodata.is_finite() && !(fill as f64).is_finite())
                {
                    return Err(MaptileError::InvalidMetadata(
                        "nodata is not representable by the native sample type".into(),
                    ));
                }
            }
            write_bands(&path, $pixels, image, query, scales, &indices, fill)?
        }};
    }
    match &image.pixels {
        PixelData::U8(v) => write!(v, u8, true),
        PixelData::U16(v) => write!(v, u16, true),
        PixelData::I16(v) => write!(v, i16, true),
        PixelData::U32(v) => write!(v, u32, true),
        PixelData::I32(v) => write!(v, i32, true),
        PixelData::F32(v) => write!(v, f32, false),
        PixelData::F64(v) => write!(v, f64, false),
    }
    gdal::vsi::call_on_mem_file_bytes(&path, |bytes| {
        if bytes.len() > MAX_COVERAGE_BYTES {
            Err(MaptileError::PayloadTooLarge {
                limit: MAX_COVERAGE_BYTES,
            })
        } else {
            Ok(Bytes::copy_from_slice(bytes))
        }
    })
    .map_err(MaptileError::backend)?
}

#[cfg(feature = "raster")]
fn write_bands<T: gdal::raster::GdalType + Copy>(
    path: &str,
    pixels: &ndarray::Array3<T>,
    image: &async_geotiff::pixel::ImageData,
    query: &CoverageQuery,
    scales: &[(f64, f64)],
    indices: &[Option<(usize, usize)>],
    fill: T,
) -> MaptileResult<()> {
    use gdal::raster::Buffer;
    let size = (query.width as usize, query.height as usize);
    let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")
        .map_err(MaptileError::backend)?
        .create_with_band_type::<T, _>(path, size.0, size.1, pixels.shape()[0])
        .map_err(MaptileError::backend)?;
    let dx = (query.bounds[2] - query.bounds[0]) / f64::from(query.width);
    let dy = (query.bounds[3] - query.bounds[1]) / f64::from(query.height);
    ds.set_geo_transform(&[query.bounds[0], dx, 0., query.bounds[3], 0., -dy])
        .map_err(MaptileError::backend)?;
    ds.set_spatial_ref(
        &gdal::spatial_ref::SpatialRef::from_definition(&query.crs)
            .map_err(MaptileError::backend)?,
    )
    .map_err(MaptileError::backend)?;
    for b in 0..pixels.shape()[0] {
        let mut band = ds.rasterband(b + 1).map_err(MaptileError::backend)?;
        let values = indices
            .iter()
            .map(|index| index.map_or(fill, |(y, x)| pixels[[b, y, x]]))
            .collect();
        band.write((0, 0), size, &mut Buffer::new(size, values))
            .map_err(MaptileError::backend)?;
        band.set_no_data_value(image.nodata)
            .map_err(MaptileError::backend)?;
        if let Some(&(scale, offset)) = scales.get(b) {
            band.set_scale(scale).map_err(MaptileError::backend)?;
            band.set_offset(offset).map_err(MaptileError::backend)?;
        }
    }
    // Retain GDAL's per-band nodata masks when the source has no explicit mask.
    // Filling outside samples with nodata then preserves each band's validity.
    if image.mask.is_some() || (image.nodata.is_none() && indices.iter().any(Option::is_none)) {
        // GeoTIFF internal masks keep validity in the single returned artifact.
        let previous_mask_option =
            gdal::config::get_thread_local_config_option("GDAL_TIFF_INTERNAL_MASK", "")
                .map_err(MaptileError::backend)?;
        gdal::config::set_thread_local_config_option("GDAL_TIFF_INTERNAL_MASK", "YES")
            .map_err(MaptileError::backend)?;
        let result = ds
            .rasterband(1)
            .and_then(|mut band| band.create_mask_band(true));
        if previous_mask_option.is_empty() {
            let _ = gdal::config::clear_thread_local_config_option("GDAL_TIFF_INTERNAL_MASK");
        } else {
            let _ = gdal::config::set_thread_local_config_option(
                "GDAL_TIFF_INTERNAL_MASK",
                &previous_mask_option,
            );
        }
        result.map_err(MaptileError::backend)?;
        let source_band = ds.rasterband(1).map_err(MaptileError::backend)?;
        let mut band = source_band
            .open_mask_band()
            .map_err(MaptileError::backend)?;
        let values = indices
            .iter()
            .map(|index| {
                if index.is_some_and(|(y, x)| image.mask.as_ref().is_none_or(|mask| mask[[y, x]])) {
                    255u8
                } else {
                    0
                }
            })
            .collect();
        band.write((0, 0), size, &mut Buffer::new(size, values))
            .map_err(MaptileError::backend)?;
    }
    ds.flush_cache().map_err(MaptileError::backend)?;
    ds.close().map_err(MaptileError::backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_coverage_is_a_client_error_not_a_configuration_failure() {
        let q = CoverageQuery {
            bounds: [0., 0., 1., 1.],
            crs: "EPSG:4326".into(),
            width: 0,
            height: 1,
        };
        assert!(matches!(q.validate(), Err(MaptileError::InvalidRequest(_))));
    }

    #[test]
    fn coverage_limits_reject_nonfinite_and_oversized_grids() {
        let mut q = CoverageQuery {
            bounds: [0., 0., 1., 1.],
            crs: "EPSG:4326".into(),
            width: 256,
            height: 256,
        };
        assert!(q.validate().is_ok());
        q.width = u32::MAX;
        assert!(q.validate().is_err());
        q.width = 256;
        q.bounds[0] = f64::NAN;
        assert!(q.validate().is_err());
    }
    #[cfg(feature = "raster")]
    #[test]
    fn geotiff_preserves_uint16_values_nodata_mask_and_georeferencing() {
        use async_geotiff::{
            bounds::{Bbox, GeoTransform},
            crs::Crs,
            pixel::{ImageData, PixelData},
        };
        let mut image = ImageData {
            pixels: PixelData::U16(
                ndarray::Array3::from_shape_vec((1, 2, 2), vec![1000, 2000, 3000, 65535]).unwrap(),
            ),
            mask: Some(
                ndarray::Array2::from_shape_vec((2, 2), vec![true, true, true, false]).unwrap(),
            ),
            bbox: Bbox::new(10., 20., 12., 22.),
            crs: Crs::epsg(4326),
            transform: GeoTransform::from_gdal(&[10., 1., 0., 22., 0., -1.]),
            nodata: Some(65535.),
        };
        let q = CoverageQuery {
            bounds: [10., 20., 12., 22.],
            crs: "EPSG:4326".into(),
            width: 2,
            height: 2,
        };
        let bytes = encode_geotiff(&image, &q, &[(0.5, 2.)]).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &bytes).unwrap();
        let ds = gdal::Dataset::open(file.path()).unwrap();
        assert_eq!(ds.raster_size(), (2, 2));
        assert_eq!(ds.raster_count(), 1);
        assert_eq!(ds.geo_transform().unwrap(), [10., 1., 0., 22., 0., -1.]);
        assert_eq!(ds.spatial_ref().unwrap().auth_code().unwrap(), 4326);
        let band = ds.rasterband(1).unwrap();
        assert_eq!(band.band_type(), gdal::raster::GdalDataType::UInt16);
        assert_eq!(band.no_data_value(), Some(65535.));
        assert_eq!(band.scale(), Some(0.5));
        assert_eq!(band.offset(), Some(2.));
        assert_eq!(
            band.read_as::<u16>((0, 0), (2, 2), (2, 2), None)
                .unwrap()
                .data(),
            &[1000, 2000, 3000, 65535]
        );
        assert_eq!(
            band.open_mask_band()
                .unwrap()
                .read_as::<u8>((0, 0), (2, 2), (2, 2), None)
                .unwrap()
                .data(),
            &[255, 255, 255, 0]
        );
        // Requested grid is retained outside the source; invalid samples carry nodata and mask=0.
        let partial = CoverageQuery {
            bounds: [9., 20., 12., 22.],
            width: 3,
            height: 2,
            crs: q.crs.clone(),
        };
        let bytes = encode_geotiff(&image, &partial, &[]).unwrap();
        let partial_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(partial_file.path(), bytes).unwrap();
        let partial_ds = gdal::Dataset::open(partial_file.path()).unwrap();
        assert_eq!(
            partial_ds.geo_transform().unwrap(),
            [9., 1., 0., 22., 0., -1.]
        );
        let partial_band = partial_ds.rasterband(1).unwrap();
        assert_eq!(
            partial_band
                .read_as::<u16>((0, 0), (3, 2), (3, 2), None)
                .unwrap()
                .data(),
            &[65535, 1000, 2000, 65535, 3000, 65535]
        );
        assert_eq!(
            partial_band
                .open_mask_band()
                .unwrap()
                .read_as::<u8>((0, 0), (3, 2), (3, 2), None)
                .unwrap()
                .data(),
            &[0, 255, 255, 0, 255, 0]
        );
        image.mask = None;
        let bytes = encode_geotiff(&image, &partial, &[]).unwrap();
        let nodata_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(nodata_file.path(), bytes).unwrap();
        let nodata_ds = gdal::Dataset::open(nodata_file.path()).unwrap();
        assert_eq!(
            nodata_ds
                .rasterband(1)
                .unwrap()
                .open_mask_band()
                .unwrap()
                .read_as::<u8>((0, 0), (3, 2), (3, 2), None)
                .unwrap()
                .data(),
            &[0, 255, 255, 0, 255, 0]
        );
        let q = CoverageQuery {
            width: 1,
            height: 1,
            ..q
        };
        let bytes = encode_geotiff(&image, &q, &[]).unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        let ds = gdal::Dataset::open(file.path()).unwrap();
        assert_eq!(
            ds.rasterband(1)
                .unwrap()
                .read_as::<u16>((0, 0), (1, 1), (1, 1), None)
                .unwrap()
                .data(),
            &[65535]
        );
        image.pixels = PixelData::F32(
            ndarray::Array3::from_shape_vec((1, 2, 2), vec![1., f32::NAN, 3., 4.]).unwrap(),
        );
        image.nodata = Some(f64::NAN);
        let bytes = encode_geotiff(&image, &partial, &[]).unwrap();
        let nan_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(nan_file.path(), bytes).unwrap();
        let nan_ds = gdal::Dataset::open(nan_file.path()).unwrap();
        assert_eq!(
            nan_ds
                .rasterband(1)
                .unwrap()
                .open_mask_band()
                .unwrap()
                .read_as::<u8>((0, 0), (3, 2), (3, 2), None)
                .unwrap()
                .data(),
            &[0, 255, 0, 0, 255, 255]
        );
    }
}
