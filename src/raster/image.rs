//! `RasterImage` value type — float pixel data + boolean mask + nodata + bounds.
//!
//! Layered above raw GDAL buffers so band-math, mosaic, encode, decode all
//! consume the same struct. Pure-data type with no GDAL or `image`-crate
//! dependency in its constructors apart from [`RasterImage::from_rgba8`]; the
//! `from_gdal_buffer` constructor lives in the `cog` / `outdb` adapter
//! refactor (Phase 15).

use crate::error::{MaptileError, MaptileResult};
use ndarray::{Array2, Array3};

/// Pixel data, validity mask, and georeferencing for a raster tile.
///
/// - `data` is `[bands, height, width]` of `f32`. Channels are 0-indexed.
/// - `mask` is `[height, width]` of bool — `true` is a valid pixel,
///   `false` is nodata. Encoders translate this into an alpha channel.
/// - `nodata` is the sentinel that produced the mask (kept for round-trips).
/// - `bounds` is `(west, south, east, north)` in WGS84 if known.
/// - `crs` is an EPSG code (e.g. `"EPSG:3857"`) or PROJ string for the
///   source CRS, or `None` if unknown.
#[derive(Debug, Clone)]
pub struct RasterImage {
    pub data: Array3<f32>,
    pub mask: Array2<bool>,
    pub nodata: Option<f32>,
    pub bounds: Option<(f64, f64, f64, f64)>,
    pub crs: Option<String>,
}

impl RasterImage {
    /// `(bands, height, width)`.
    #[must_use]
    pub fn shape(&self) -> (usize, usize, usize) {
        let (b, h, w) = self.data.dim();
        (b, h, w)
    }

    #[must_use]
    pub fn bands(&self) -> usize {
        self.data.dim().0
    }

    #[must_use]
    pub fn height(&self) -> usize {
        self.data.dim().1
    }

    #[must_use]
    pub fn width(&self) -> usize {
        self.data.dim().2
    }

    /// Build a multi-band `RasterImage` from a slice of GDAL band buffers,
    /// converting each pixel to `f32`. Every buffer must share the same
    /// `(width, height)` shape (in `(cols, rows)` GDAL order). When
    /// `nodata` is set, pixels equal to the sentinel are masked out.
    ///
    /// This is the entry point for `cog` / `outdb` / `stac` consumers to
    /// adapt raw `gdal::raster::Buffer<T>` reads into the unified pipeline.
    pub fn from_gdal_buffers(
        bands: &[gdal::raster::Buffer<f64>],
        nodata: Option<f64>,
        bounds: Option<(f64, f64, f64, f64)>,
        crs: Option<String>,
    ) -> MaptileResult<Self> {
        let Some(first) = bands.first() else {
            return Err(MaptileError::Mosaic(
                "from_gdal_buffers requires at least one band".into(),
            ));
        };
        let (w_cols, h_rows) = first.shape();
        for (idx, band) in bands.iter().enumerate() {
            if band.shape() != first.shape() {
                return Err(MaptileError::Mosaic(format!(
                    "band {idx} shape {:?} does not match band 0 shape {:?}",
                    band.shape(),
                    first.shape()
                )));
            }
        }
        let mut data = Array3::<f32>::zeros((bands.len(), h_rows, w_cols));
        let mut mask = Array2::<bool>::from_elem((h_rows, w_cols), true);
        for (b, band) in bands.iter().enumerate() {
            let raw = band.data();
            for y in 0..h_rows {
                for x in 0..w_cols {
                    let v = raw[y * w_cols + x];
                    data[(b, y, x)] = v as f32;
                    if let Some(nd) = nodata {
                        if v == nd || v.is_nan() {
                            mask[(y, x)] = false;
                        }
                    } else if v.is_nan() {
                        mask[(y, x)] = false;
                    }
                }
            }
        }
        Ok(Self {
            data,
            mask,
            nodata: nodata.map(|v| v as f32),
            bounds,
            crs,
        })
    }

    /// Build a 4-band RGBA `RasterImage` from a flat byte buffer of size
    /// `width * height * 4`. Pixel values are converted to `f32` in the
    /// `0.0..=255.0` range; the validity mask is initialised to all-true
    /// (encoders that need transparency should set `mask` accordingly).
    pub fn from_rgba8(rgba: &[u8], width: u32, height: u32) -> MaptileResult<Self> {
        let expected = rgba_len(width, height)?;
        if rgba.len() != expected {
            return Err(MaptileError::Render(format!(
                "rgba buffer is {} bytes; expected {expected} for {width}x{height}",
                rgba.len()
            )));
        }
        let h = height as usize;
        let w = width as usize;
        let mut data = Array3::<f32>::zeros((4, h, w));
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                data[(0, y, x)] = f32::from(rgba[i]);
                data[(1, y, x)] = f32::from(rgba[i + 1]);
                data[(2, y, x)] = f32::from(rgba[i + 2]);
                data[(3, y, x)] = f32::from(rgba[i + 3]);
            }
        }
        let mask = Array2::<bool>::from_elem((h, w), true);
        Ok(Self {
            data,
            mask,
            nodata: None,
            bounds: None,
            crs: None,
        })
    }
}

pub(super) const MAX_IMAGE_WORKING_BYTES: usize = 256 * 1024 * 1024;

pub(super) fn rgba_len(width: u32, height: u32) -> MaptileResult<usize> {
    let pixels = u64::from(width) * u64::from(height);
    // RGBA8 input, four f32 planes, and one validity byte per pixel.
    if pixels > (MAX_IMAGE_WORKING_BYTES / 21) as u64 {
        return Err(MaptileError::PayloadTooLarge {
            limit: MAX_IMAGE_WORKING_BYTES,
        });
    }
    Ok(pixels as usize * 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgba(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            v.extend_from_slice(&rgba);
        }
        v
    }

    #[test]
    fn shape_returns_bands_height_width() {
        let img = RasterImage::from_rgba8(&solid_rgba(4, 3, [10, 20, 30, 255]), 4, 3).unwrap();
        assert_eq!(img.shape(), (4, 3, 4));
        assert_eq!(img.bands(), 4);
        assert_eq!(img.height(), 3);
        assert_eq!(img.width(), 4);
    }

    #[test]
    fn from_rgba8_rejects_bad_byte_length() {
        let err = RasterImage::from_rgba8(&[0u8; 10], 4, 4).unwrap_err();
        assert!(matches!(err, MaptileError::Render(_)));
    }

    #[test]
    fn from_rgba8_loads_per_channel_values() {
        let img = RasterImage::from_rgba8(&solid_rgba(2, 2, [10, 20, 30, 200]), 2, 2).unwrap();
        for y in 0..2 {
            for x in 0..2 {
                assert!((img.data[(0, y, x)] - 10.0).abs() < 1e-3);
                assert!((img.data[(1, y, x)] - 20.0).abs() < 1e-3);
                assert!((img.data[(2, y, x)] - 30.0).abs() < 1e-3);
                assert!((img.data[(3, y, x)] - 200.0).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn from_rgba8_initialises_mask_to_all_true() {
        let img = RasterImage::from_rgba8(&solid_rgba(2, 2, [0; 4]), 2, 2).unwrap();
        for y in 0..2 {
            for x in 0..2 {
                assert!(img.mask[(y, x)]);
            }
        }
    }

    #[test]
    fn bounds_and_crs_survive_clone() {
        let mut img = RasterImage::from_rgba8(&solid_rgba(1, 1, [0; 4]), 1, 1).unwrap();
        img.bounds = Some((-10.0, -5.0, 10.0, 5.0));
        img.crs = Some("EPSG:3857".to_string());
        img.nodata = Some(-9999.0);
        let cloned = img.clone();
        assert_eq!(cloned.bounds, Some((-10.0, -5.0, 10.0, 5.0)));
        assert_eq!(cloned.crs.as_deref(), Some("EPSG:3857"));
        assert_eq!(cloned.nodata, Some(-9999.0));
    }

    #[test]
    fn from_gdal_buffers_stacks_bands_and_masks_nodata() {
        // Two synthetic bands shaped (cols=3, rows=2). Band 0 has a -9999
        // sentinel at pixel (row=0, col=1).
        let band0 =
            gdal::raster::Buffer::<f64>::new((3, 2), vec![1.0, -9999.0, 3.0, 4.0, 5.0, 6.0]);
        let band1 =
            gdal::raster::Buffer::<f64>::new((3, 2), vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
        let img = RasterImage::from_gdal_buffers(
            &[band0, band1],
            Some(-9999.0),
            Some((-1.0, -2.0, 3.0, 4.0)),
            Some("EPSG:3857".into()),
        )
        .unwrap();
        assert_eq!(img.shape(), (2, 2, 3)); // (bands, height, width)
        assert!((img.data[(0, 0, 0)] - 1.0).abs() < 1e-3);
        assert!(img.data[(0, 0, 1)] < 0.0); // sentinel still in data
        assert!(!img.mask[(0, 1)]); // but masked out
        assert!(img.mask[(0, 0)]);
        assert_eq!(img.bounds, Some((-1.0, -2.0, 3.0, 4.0)));
        assert_eq!(img.crs.as_deref(), Some("EPSG:3857"));
        assert_eq!(img.nodata, Some(-9999.0));
    }

    #[test]
    fn from_gdal_buffers_rejects_band_shape_mismatch() {
        let a = gdal::raster::Buffer::<f64>::new((2, 2), vec![0.0; 4]);
        let b = gdal::raster::Buffer::<f64>::new((3, 2), vec![0.0; 6]);
        let err = RasterImage::from_gdal_buffers(&[a, b], None, None, None).unwrap_err();
        assert!(matches!(err, MaptileError::Mosaic(_)));
    }

    #[test]
    fn from_gdal_buffers_empty_input_is_error() {
        let err = RasterImage::from_gdal_buffers(&[], None, None, None).unwrap_err();
        assert!(matches!(err, MaptileError::Mosaic(_)));
    }

    #[test]
    fn from_gdal_buffers_treats_nan_as_invalid_when_no_nodata() {
        let band = gdal::raster::Buffer::<f64>::new((2, 1), vec![1.0, f64::NAN]);
        let img = RasterImage::from_gdal_buffers(&[band], None, None, None).unwrap();
        assert!(img.mask[(0, 0)]);
        assert!(!img.mask[(0, 1)]);
    }

    #[test]
    fn zero_dims_compile_but_shape_zero() {
        // Defensive — caller-built from raw data, not from_rgba8 — make sure
        // shape() is well-defined for empty arrays.
        let img = RasterImage {
            data: Array3::<f32>::zeros((0, 0, 0)),
            mask: Array2::<bool>::from_elem((0, 0), false),
            nodata: None,
            bounds: None,
            crs: None,
        };
        assert_eq!(img.shape(), (0, 0, 0));
    }
}
