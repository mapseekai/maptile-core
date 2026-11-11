//! Thread-affine MapLibre Native adapter for server-side rendering.
//!
//! A renderer must be constructed and used on the same thread because the
//! upstream native run loop is thread-affine. `maplibre_native::ImageRenderer`
//! enforces that contract by being neither `Send` nor `Sync`.

use std::num::NonZeroU32;
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(feature = "render-bench")]
use std::sync::atomic::{AtomicU64, Ordering};

use maplibre_native::{CameraUpdate, ImageRendererBuilder, LatLng, Size, Static};

use crate::error::{MaptileError, MaptileResult};
use crate::render::types::{RenderRequest, RenderedImage};

#[cfg(feature = "render-bench")]
static RENDERER_CONSTRUCTIONS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "render-bench")]
static SUCCESSFUL_STYLE_LOADS: AtomicU64 = AtomicU64::new(0);

/// Benchmark-only lifecycle observations for the native renderer adapter.
#[cfg(feature = "render-bench")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BenchmarkLifecycleCounts {
    pub renderer_constructions: u64,
    pub successful_style_loads: u64,
}

/// Snapshot benchmark-only renderer lifecycle counters.
#[cfg(feature = "render-bench")]
#[doc(hidden)]
#[must_use]
pub fn benchmark_lifecycle_counts() -> BenchmarkLifecycleCounts {
    BenchmarkLifecycleCounts {
        renderer_constructions: RENDERER_CONSTRUCTIONS.load(Ordering::Relaxed),
        successful_style_loads: SUCCESSFUL_STYLE_LOADS.load(Ordering::Relaxed),
    }
}

/// Reset benchmark-only lifecycle counters between sequential benchmark groups.
///
/// The benchmark harness must ensure no renderer worker is active while this
/// function runs; reset is not an atomic snapshot across both counters.
#[cfg(feature = "render-bench")]
#[doc(hidden)]
pub fn reset_benchmark_lifecycle_counts() {
    RENDERER_CONSTRUCTIONS.store(0, Ordering::Relaxed);
    SUCCESSFUL_STYLE_LOADS.store(0, Ordering::Relaxed);
}

#[cfg(feature = "render-bench")]
fn record_renderer_construction() {
    RENDERER_CONSTRUCTIONS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "render-bench")]
fn record_successful_style_load() {
    SUCCESSFUL_STYLE_LOADS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn postprocess_image(
    image: &image::RgbaImage,
    request: &RenderRequest,
) -> MaptileResult<RenderedImage> {
    let width = image.width();
    let height = image.height();
    let bytes = if let Some(items) = request.overlay_items.as_deref()
        && !items.is_empty()
    {
        let mut canvas = image.clone();
        let (paths, markers) = crate::render::overlay::expand_overlays(items);
        crate::render::overlay::draw_overlays(
            &mut canvas,
            &paths,
            &markers,
            request.camera.lon,
            request.camera.lat,
            request.camera.zoom,
            f32::from(request.scale),
        );
        crate::render::encode::encode_image(&canvas, request.format)?
    } else {
        crate::render::encode::encode_image(image, request.format)?
    };
    Ok(RenderedImage {
        bytes,
        format: request.format,
        width,
        height,
    })
}

/// One thread-affine MapLibre Native renderer instance.
pub struct MapLibreRenderer {
    renderer: Option<maplibre_native::ImageRenderer<Static>>,
    pixel_ratio: f32,
    current_size: (u32, u32),
    loaded_style_hash: Option<blake3::Hash>,
    native_render_calls: usize,
}

/// `maplibre_native` 0.8.6 Static renderers block on their 32nd render on
/// macOS Metal. Rotate after 31 successful native calls on the owning thread.
const MAX_NATIVE_RENDER_CALLS: usize = 31;

fn prepare_renderer_slot<T, E>(
    renderer: &mut Option<T>,
    native_render_calls: &mut usize,
    build: impl FnOnce() -> Result<T, E>,
) -> Result<(), E> {
    if *native_render_calls >= MAX_NATIVE_RENDER_CALLS {
        // Drop the old native state before constructing its replacement.
        drop(renderer.take());
        *native_render_calls = 0;
    }
    if renderer.is_none() {
        *renderer = Some(build()?);
    }
    Ok(())
}

fn finish_native_render(
    native_render_calls: &mut usize,
    actual_len: usize,
    width: u32,
    height: u32,
) -> MaptileResult<()> {
    *native_render_calls += 1;

    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| MaptileError::Render("rendered image dimensions overflow".into()))?;
    if actual_len != expected_len {
        return Err(MaptileError::Render(format!(
            "renderer returned {actual_len} RGBA bytes, expected {expected_len} for {width}x{height}",
        )));
    }

    Ok(())
}

impl MapLibreRenderer {
    /// Build a renderer pre-sized to `width × height` at the given
    /// `pixel_ratio` (1.0 for standard, 2.0 for `@2x`, etc.).
    pub fn new(width: u32, height: u32, pixel_ratio: f32) -> MaptileResult<Self> {
        let width = NonZeroU32::new(width)
            .ok_or_else(|| MaptileError::Render("renderer width must be non-zero".into()))?;
        let height = NonZeroU32::new(height)
            .ok_or_else(|| MaptileError::Render("renderer height must be non-zero".into()))?;
        let renderer = Self::build_renderer(width, height, pixel_ratio, "construction")?;

        Ok(Self {
            renderer: Some(renderer),
            pixel_ratio,
            current_size: (width.get(), height.get()),
            loaded_style_hash: None,
            native_render_calls: 0,
        })
    }

    fn build_renderer(
        width: NonZeroU32,
        height: NonZeroU32,
        pixel_ratio: f32,
        operation: &str,
    ) -> MaptileResult<maplibre_native::ImageRenderer<Static>> {
        let renderer = catch_unwind(AssertUnwindSafe(|| {
            ImageRendererBuilder::new()
                .with_size(width, height)
                .with_pixel_ratio(pixel_ratio)
                .build_static_renderer()
        }))
        .map_err(|_| {
            MaptileError::Render(format!(
                "native renderer {operation} panicked; renderer remains empty and can be retried"
            ))
        })?;
        #[cfg(feature = "render-bench")]
        record_renderer_construction();
        Ok(renderer)
    }

    fn prepare_renderer(&mut self) -> MaptileResult<()> {
        if self.native_render_calls >= MAX_NATIVE_RENDER_CALLS {
            self.loaded_style_hash = None;
        }
        let width = NonZeroU32::new(self.current_size.0)
            .expect("MapLibreRenderer current width is always non-zero");
        let height = NonZeroU32::new(self.current_size.1)
            .expect("MapLibreRenderer current height is always non-zero");
        let pixel_ratio = self.pixel_ratio;
        prepare_renderer_slot(&mut self.renderer, &mut self.native_render_calls, || {
            Self::build_renderer(width, height, pixel_ratio, "reconstruction")
        })
    }

    fn renderer_mut(&mut self) -> &mut maplibre_native::ImageRenderer<Static> {
        self.renderer
            .as_mut()
            .expect("prepare_renderer installs a renderer before native work")
    }

    fn ensure_style(&mut self, style_json: &str) -> MaptileResult<()> {
        let hash = blake3::hash(style_json.as_bytes());
        if self.loaded_style_hash == Some(hash) {
            return Ok(());
        }

        self.loaded_style_hash = None;
        self.renderer_mut()
            .load_style_from_json_str(style_json)
            .wait()
            .map_err(|error| MaptileError::Render(format!("style load failed: {error}")))?;
        #[cfg(feature = "render-bench")]
        record_successful_style_load();
        self.loaded_style_hash = Some(hash);
        Ok(())
    }

    pub(crate) fn render_image(
        &mut self,
        request: &RenderRequest,
    ) -> MaptileResult<maplibre_native::Image> {
        self.prepare_renderer()?;
        self.ensure_style(&request.style_json)?;

        let size = (request.width, request.height);
        if size != self.current_size {
            self.renderer_mut().set_map_size(Size {
                width: request.width,
                height: request.height,
            });
            self.current_size = size;
        }

        let camera = CameraUpdate::new()
            .center(LatLng {
                lat: request.camera.lat,
                lng: request.camera.lon,
            })
            .zoom(request.camera.zoom)
            .bearing(request.camera.bearing)
            .pitch(request.camera.pitch);
        let image = self
            .renderer_mut()
            .render_static(&camera)
            .map_err(|error| MaptileError::Render(format!("static render failed: {error}")))?;

        let width = image.as_image().width();
        let height = image.as_image().height();
        let actual_len = image.as_image().as_raw().len();
        finish_native_render(&mut self.native_render_calls, actual_len, width, height)?;

        Ok(image)
    }

    /// Render the given request synchronously. **Blocking** — use it only on
    /// the renderer's owning thread.
    pub fn render_rgba(&mut self, request: &RenderRequest) -> MaptileResult<(Vec<u8>, u32, u32)> {
        let image = self.render_image(request)?;
        let width = image.as_image().width();
        let height = image.as_image().height();
        Ok((image.as_image().as_raw().clone(), width, height))
    }

    /// Render and encode in one shot. If `request.overlay_items` is non-empty,
    /// the items are painted on a mutable copy before encoding.
    pub fn render(&mut self, request: &RenderRequest) -> MaptileResult<RenderedImage> {
        let image = self.render_image(request)?;
        postprocess_image(image.as_image(), request)
    }

    /// Pixel ratio this renderer was constructed with.
    #[must_use]
    pub fn pixel_ratio(&self) -> f32 {
        self.pixel_ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::types::RenderFormat;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn postprocess_image_borrows_pixels_without_overlays() {
        let image = image::RgbaImage::from_pixel(3, 2, image::Rgba([12, 34, 56, 255]));
        let mut request = RenderRequest::for_tile("{}", 0, 0, 0, 1, RenderFormat::Png);
        request.width = image.width();
        request.height = image.height();

        let rendered = postprocess_image(&image, &request).unwrap();

        assert_eq!(image.get_pixel(0, 0).0, [12, 34, 56, 255]);
        assert_eq!((rendered.width, rendered.height), (3, 2));
        let decoded = image::load_from_memory(&rendered.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (3, 2));
    }

    #[test]
    fn renderer_slot_retries_after_failed_reconstruction() {
        let first_calls = Rc::new(Cell::new(MAX_NATIVE_RENDER_CALLS));
        let mut renderer = Some(first_calls);
        let mut native_render_calls = MAX_NATIVE_RENDER_CALLS;

        let failed = prepare_renderer_slot(&mut renderer, &mut native_render_calls, || {
            Err::<Rc<Cell<usize>>, _>("reconstruction failed")
        });
        assert_eq!(failed, Err("reconstruction failed"));
        assert!(renderer.is_none());
        assert_eq!(native_render_calls, 0);

        let replacement = Rc::new(Cell::new(0));
        prepare_renderer_slot(&mut renderer, &mut native_render_calls, || {
            Ok::<_, &str>(replacement.clone())
        })
        .unwrap();
        assert!(Rc::ptr_eq(renderer.as_ref().unwrap(), &replacement));
    }

    #[test]
    fn renderer_slot_rotates_before_any_instance_receives_call_32() {
        let mut renderer = None;
        let mut native_render_calls = 0;
        let mut instances = Vec::new();

        for _ in 0..40 {
            prepare_renderer_slot(&mut renderer, &mut native_render_calls, || {
                let calls = Rc::new(Cell::new(0));
                instances.push(calls.clone());
                Ok::<_, ()>(calls)
            })
            .unwrap();
            let calls = renderer.as_ref().unwrap();
            calls.set(calls.get() + 1);
            finish_native_render(&mut native_render_calls, 4, 1, 1).unwrap();
        }

        let calls: Vec<_> = instances.iter().map(|calls| calls.get()).collect();
        assert_eq!(calls, [31, 9]);
        assert!(calls.iter().all(|calls| *calls <= MAX_NATIVE_RENDER_CALLS));
    }

    #[test]
    fn native_successes_consume_rotation_budget_even_when_validation_fails() {
        let mut renderer = None;
        let mut native_render_calls = 0;
        let mut instances = Vec::new();

        for _ in 0..MAX_NATIVE_RENDER_CALLS {
            prepare_renderer_slot(&mut renderer, &mut native_render_calls, || {
                let calls = Rc::new(Cell::new(0));
                instances.push(calls.clone());
                Ok::<_, ()>(calls)
            })
            .unwrap();
            let calls = renderer.as_ref().unwrap();
            calls.set(calls.get() + 1);

            let error = finish_native_render(&mut native_render_calls, 3, 1, 1).unwrap_err();
            assert!(error.to_string().contains("3 RGBA bytes, expected 4"));
        }

        assert_eq!(native_render_calls, MAX_NATIVE_RENDER_CALLS);

        prepare_renderer_slot(&mut renderer, &mut native_render_calls, || {
            let calls = Rc::new(Cell::new(0));
            instances.push(calls.clone());
            Ok::<_, ()>(calls)
        })
        .unwrap();

        let calls: Vec<_> = instances.iter().map(|calls| calls.get()).collect();
        assert_eq!(calls, [MAX_NATIVE_RENDER_CALLS, 0]);
        assert_eq!(native_render_calls, 0);
    }

    #[cfg(feature = "render-bench")]
    #[test]
    fn benchmark_lifecycle_counters_reset_to_zero() {
        reset_benchmark_lifecycle_counts();
        record_renderer_construction();
        record_successful_style_load();

        assert_eq!(
            benchmark_lifecycle_counts(),
            BenchmarkLifecycleCounts {
                renderer_constructions: 1,
                successful_style_loads: 1,
            }
        );

        reset_benchmark_lifecycle_counts();

        assert_eq!(
            benchmark_lifecycle_counts(),
            BenchmarkLifecycleCounts {
                renderer_constructions: 0,
                successful_style_loads: 0,
            }
        );
    }
}
