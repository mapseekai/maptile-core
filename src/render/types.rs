//! Render-pipeline types — format, request, error.
//!
//! These compile unconditionally so that callers can declare and parse
//! requests without pulling in the native renderer dependency. The actual renderer
//! (`crate::render::native`) is feature-gated on `render`.

use std::str::FromStr;

pub(crate) const TILE_SIZE: u32 = 512;

pub(crate) fn mercator_y(lat: f64) -> f64 {
    lat.clamp(-85.051_128_779_806_6, 85.051_128_779_806_6)
        .to_radians()
        .tan()
        .asinh()
}

/// Maximum allowed image dimension (width or height) in pixels — guards
/// against memory-exhaustion DoS via oversized static map requests.
pub const MAX_IMAGE_DIMENSION: u32 = 4096;

/// Maximum allowed pixel ratio / scale (i.e. `@4x`).
pub const MAX_SCALE: u8 = 4;

/// Output image format for rendered tiles and static maps.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderFormat {
    Png,
    Jpeg,
    Webp,
}

impl RenderFormat {
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }
}

impl FromStr for RenderFormat {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "png" => Ok(Self::Png),
            "jpg" | "jpeg" => Ok(Self::Jpeg),
            "webp" => Ok(Self::Webp),
            _ => Err(()),
        }
    }
}

/// Camera viewport — what the renderer should look at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderCamera {
    pub lon: f64,
    pub lat: f64,
    pub zoom: f64,
    pub bearing: f64,
    pub pitch: f64,
}

impl RenderCamera {
    #[must_use]
    pub fn new(lon: f64, lat: f64, zoom: f64) -> Self {
        Self {
            lon,
            lat,
            zoom,
            bearing: 0.0,
            pitch: 0.0,
        }
    }

    #[must_use]
    pub fn with_bearing(mut self, bearing: f64) -> Self {
        self.bearing = bearing;
        self
    }

    #[must_use]
    pub fn with_pitch(mut self, pitch: f64) -> Self {
        self.pitch = pitch;
        self
    }
}

/// One render unit of work — a style + viewport + output configuration.
#[derive(Debug, Clone)]
pub struct RenderRequest {
    /// Full MapLibre style JSON (already URL-rewritten via
    /// [`crate::styles::rewrite_urls`] if needed).
    pub style_json: String,
    /// Use a fresh thread-affine native context for this job. Authenticated
    /// per-render assets must not retain sprite/glyph state across callers.
    /// This flag is set by the trusted application layer, not decoded from JSON.
    pub isolate_native_state: bool,
    pub camera: RenderCamera,
    pub width: u32,
    pub height: u32,
    /// Pixel ratio: 1 = standard, 2 = retina (`@2x`), up to [`MAX_SCALE`].
    pub scale: u8,
    pub format: RenderFormat,
    /// Overlays painted on top of the rendered image before encoding.
    /// `None` (or an empty Vec) means "no overlays". Only available when
    /// the `render` feature is on, since the items reference image-crate
    /// types from [`crate::render::overlay`].
    #[cfg(feature = "render")]
    pub overlay_items: Option<Vec<crate::render::overlay::OverlayItem>>,
}

impl RenderRequest {
    /// Build a request for raster tile (z, x, y) at the given scale and format.
    /// Tile size is fixed at 512px (the de-facto MapLibre default).
    #[must_use]
    pub fn for_tile(
        style_json: impl Into<String>,
        z: u8,
        x: u32,
        y: u32,
        scale: u8,
        format: RenderFormat,
    ) -> Self {
        // Tile centre (not top-left corner): (x + 0.5, y + 0.5).
        let n = 2_f64.powi(z as i32);
        let lon = ((x as f64) + 0.5) / n * 360.0 - 180.0;
        let lat_rad = ((1.0 - 2.0 * ((y as f64) + 0.5) / n) * std::f64::consts::PI)
            .sinh()
            .atan();
        let lat = lat_rad.to_degrees();
        Self {
            style_json: style_json.into(),
            isolate_native_state: false,
            camera: RenderCamera::new(lon, lat, z as f64),
            width: TILE_SIZE,
            height: TILE_SIZE,
            scale,
            format,
            #[cfg(feature = "render")]
            overlay_items: None,
        }
    }

    /// Validate dimensions and scale; called once before handing off to the
    /// renderer pool. Returns the request unchanged on success.
    pub fn validate(self) -> Result<Self, RenderRequestError> {
        if self.width == 0 || self.height == 0 {
            return Err(RenderRequestError::ZeroDimension);
        }
        if self.scale == 0 || self.scale > MAX_SCALE {
            return Err(RenderRequestError::InvalidScale {
                scale: self.scale,
                max: MAX_SCALE,
            });
        }
        let width = self.width.saturating_mul(u32::from(self.scale));
        let height = self.height.saturating_mul(u32::from(self.scale));
        if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
            return Err(RenderRequestError::TooLarge {
                width,
                height,
                max: MAX_IMAGE_DIMENSION,
            });
        }
        if !self.camera.lon.is_finite()
            || !self.camera.lat.is_finite()
            || !self.camera.zoom.is_finite()
            || !self.camera.bearing.is_finite()
            || !self.camera.pitch.is_finite()
        {
            return Err(RenderRequestError::NonFiniteCamera);
        }
        Ok(self)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Clone)]
#[non_exhaustive]
pub enum RenderRequestError {
    #[error("image dimension must be greater than zero")]
    ZeroDimension,
    #[error("image {width}x{height} exceeds maximum dimension {max}")]
    TooLarge { width: u32, height: u32, max: u32 },
    #[error("scale {scale} is out of range (1..={max})")]
    InvalidScale { scale: u8, max: u8 },
    #[error("camera contains non-finite values")]
    NonFiniteCamera,
}

impl From<RenderRequestError> for crate::error::MaptileError {
    fn from(value: RenderRequestError) -> Self {
        crate::error::MaptileError::Render(value.to_string())
    }
}

/// Result of a successful render — encoded image bytes plus metadata.
#[derive(Debug, Clone)]
pub struct RenderedImage {
    pub bytes: bytes::Bytes,
    pub format: RenderFormat,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_format_parses_case_insensitively() {
        assert_eq!("PNG".parse::<RenderFormat>().unwrap(), RenderFormat::Png);
        assert_eq!("Jpg".parse::<RenderFormat>().unwrap(), RenderFormat::Jpeg);
        assert!("gif".parse::<RenderFormat>().is_err());
    }

    #[test]
    fn for_tile_centers_camera_correctly_at_zoom_zero() {
        let r = RenderRequest::for_tile("{}", 0, 0, 0, 1, RenderFormat::Png);
        // z=0 single tile is the whole world; centre is (0, 0).
        assert!(r.camera.lon.abs() < 1e-9);
        assert!(r.camera.lat.abs() < 1e-9);
        assert_eq!(r.camera.zoom, 0.0);
        assert_eq!(r.width, 512);
        assert_eq!(r.height, 512);
    }

    #[test]
    fn for_tile_z2_x3_y1_lands_in_correct_quadrant() {
        // z=2, x=3 -> right-most tile in eastern hemisphere
        // y=1 -> upper half (north)
        let r = RenderRequest::for_tile("{}", 2, 3, 1, 1, RenderFormat::Png);
        assert!(r.camera.lon > 90.0 && r.camera.lon < 180.0);
        assert!(r.camera.lat > 0.0);
    }

    #[test]
    fn validate_rejects_zero_dim() {
        let mut r = RenderRequest::for_tile("{}", 0, 0, 0, 1, RenderFormat::Png);
        r.width = 0;
        assert_eq!(r.validate().unwrap_err(), RenderRequestError::ZeroDimension);
    }

    #[test]
    fn validate_rejects_too_large() {
        let mut r = RenderRequest::for_tile("{}", 0, 0, 0, 1, RenderFormat::Png);
        r.width = MAX_IMAGE_DIMENSION + 1;
        assert!(matches!(
            r.validate().unwrap_err(),
            RenderRequestError::TooLarge { .. }
        ));
    }

    #[test]
    fn validate_rejects_invalid_scale() {
        let mut r = RenderRequest::for_tile("{}", 0, 0, 0, 1, RenderFormat::Png);
        r.scale = 0;
        assert!(matches!(
            r.clone().validate().unwrap_err(),
            RenderRequestError::InvalidScale { .. }
        ));
        r.scale = MAX_SCALE + 1;
        assert!(matches!(
            r.validate().unwrap_err(),
            RenderRequestError::InvalidScale { .. }
        ));
    }

    #[test]
    fn request_rejects_dimensions_and_scale_outside_supported_bounds() {
        let base = RenderRequest::for_tile("{}", 0, 0, 0, 1, RenderFormat::Png);

        let mut zero_width = base.clone();
        zero_width.width = 0;
        assert!(zero_width.validate().is_err());

        let mut oversized = base.clone();
        oversized.height = 4097;
        assert!(oversized.validate().is_err());

        let mut invalid_scale = base;
        invalid_scale.scale = 5;
        assert!(invalid_scale.validate().is_err());
    }

    #[test]
    fn render_format_extension_is_lowercase() {
        assert_eq!(RenderFormat::Png.extension(), "png");
        assert_eq!(RenderFormat::Jpeg.extension(), "jpg");
    }

    #[test]
    fn render_format_parses_webp() {
        assert_eq!("webp".parse::<RenderFormat>().unwrap(), RenderFormat::Webp);
        assert_eq!("WEBP".parse::<RenderFormat>().unwrap(), RenderFormat::Webp);
    }

    #[test]
    fn webp_extension_and_content_type() {
        assert_eq!(RenderFormat::Webp.extension(), "webp");
        assert_eq!(RenderFormat::Webp.content_type(), "image/webp");
    }
}
