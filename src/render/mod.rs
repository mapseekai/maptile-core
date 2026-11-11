//! Server-side raster rendering and WMTS metadata.
//!
//! The module is split so that **request-shape** code (formats, camera
//! params, static-map URL parsing, WMTS XML) compiles unconditionally,
//! while **render execution** (the thread-affine `maplibre_native` adapter,
//! image encoding, the renderer pool) is gated behind the `render`
//! cargo feature.
//!
//! Typical adapter flow:
//!
//! 1. Parse the request (path or query params) into a [`RenderRequest`]
//!    via [`RenderRequest::for_tile`] or
//!    [`static_map::build_request`].
//! 2. Hand the request to a process-wide `pool::RendererPool` (with the `render` feature enabled).
//! 3. Serve the resulting [`RenderedImage`] with the MIME type from
//!    [`RenderFormat::content_type`].

#[cfg(all(feature = "render-opengl", not(target_os = "linux")))]
compile_error!("the render-opengl feature is supported only on Linux");

pub mod static_map;
pub mod types;
pub mod wmts;

pub use static_map::{StaticTarget, StaticTargetError};
pub use types::{
    MAX_IMAGE_DIMENSION, MAX_SCALE, RenderCamera, RenderFormat, RenderRequest, RenderRequestError,
    RenderedImage,
};
pub use wmts::{WmtsLayer, WmtsService, build_capabilities};

#[cfg(feature = "render")]
pub mod encode;
#[cfg(feature = "render")]
pub mod native;
#[cfg(feature = "render")]
pub mod overlay;
#[cfg(feature = "render")]
pub mod pool;

#[cfg(feature = "render")]
pub use overlay::{
    GeoPoint, MarkerOverlay, OverlayItem, PathOverlay, calculate_bounds, decode_polyline,
    draw_overlays, expand_overlays, parse_geojson, parse_marker, parse_path,
};

#[cfg(feature = "render")]
pub use encode::encode_rgba;
#[cfg(feature = "render")]
pub use maplibre_native::file_source as maplibre_file_source;
#[cfg(feature = "render")]
pub use native::MapLibreRenderer;
#[cfg(feature = "render")]
pub use pool::{PoolConfig, RendererPool};
