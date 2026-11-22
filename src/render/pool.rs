//! Fixed worker pool for thread-affine MapLibre Native renderers.
//!
//! Each named OS thread constructs, uses, resets, and drops its own renderer
//! state. Callers submit work through one shared bounded queue, which provides
//! backpressure while allowing idle workers to pick up any request.

use crate::error::{MaptileError, MaptileResult};
use crate::render::native::MapLibreRenderer;
use crate::render::types::{RenderRequest, RenderedImage};
use std::any::Any;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(debug_assertions)]
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};

/// Pool configuration.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Number of durable render threads. Must be positive.
    pub max_concurrent: usize,
    /// Initial canvas size for renderers created by a worker.
    pub default_size: (u32, u32),
    /// Bounded queue slots allocated per worker. Zero is rejected.
    pub queue_depth_per_worker: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            default_size: (512, 512),
            queue_depth_per_worker: 4,
        }
    }
}

struct ValidatedPoolConfig {
    worker_count: usize,
    queue_capacity: usize,
}

impl PoolConfig {
    fn validated(&self) -> MaptileResult<ValidatedPoolConfig> {
        if self.queue_depth_per_worker == 0 {
            return Err(MaptileError::Config(
                "renderer queue_depth_per_worker must be greater than zero".into(),
            ));
        }
        let worker_count = self.max_concurrent;
        if worker_count == 0 {
            return Err(MaptileError::Config(
                "renderer max_concurrent must be positive".into(),
            ));
        }
        let queue_capacity = worker_count
            .checked_mul(self.queue_depth_per_worker)
            .ok_or_else(|| MaptileError::Config("renderer queue capacity overflow".into()))?;
        Ok(ValidatedPoolConfig {
            worker_count,
            queue_capacity,
        })
    }
}

trait RenderBackend: 'static {
    type Output: Send + 'static;

    fn render(&mut self, request: &RenderRequest) -> MaptileResult<Self::Output>;
    fn reset(&mut self);
}

struct RenderJob<O> {
    request: RenderRequest,
    response: tokio::sync::oneshot::Sender<MaptileResult<O>>,
    keepalive: Option<RenderKeepalive>,
}

pub type RenderKeepalive = Box<dyn Any + Send + 'static>;

enum WorkerMessage<O> {
    Render(RenderJob<O>),
    Shutdown,
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

fn worker_loop<B: RenderBackend>(
    receiver: flume::Receiver<WorkerMessage<B::Output>>,
    mut backend: B,
) {
    while let Ok(message) = receiver.recv() {
        match message {
            WorkerMessage::Render(job) => {
                if job.response.is_closed() {
                    continue;
                }
                let _keepalive = job.keepalive;
                let result = catch_unwind(AssertUnwindSafe(|| backend.render(&job.request)));
                match result {
                    Ok(result) => {
                        let _ = job.response.send(result);
                    }
                    Err(render_panic) => {
                        tracing::error!(
                            panic = panic_message(render_panic.as_ref()),
                            "render backend panicked"
                        );
                        let reset = catch_unwind(AssertUnwindSafe(|| backend.reset()));
                        let message = match reset {
                            Ok(()) => "render worker panicked and reset native state",
                            Err(reset_panic) => {
                                tracing::error!(
                                    panic = panic_message(reset_panic.as_ref()),
                                    "render backend reset also panicked"
                                );
                                "render worker panicked; native-state reset also panicked"
                            }
                        };
                        let _ = job.response.send(Err(MaptileError::Render(message.into())));
                    }
                }
            }
            WorkerMessage::Shutdown => break,
        }
    }
}

struct WorkerGroup<O: Send + 'static> {
    sender: Option<flume::Sender<WorkerMessage<O>>>,
    handles: Vec<JoinHandle<()>>,
}

impl<O: Send + 'static> WorkerGroup<O> {
    fn sender(&self) -> &flume::Sender<WorkerMessage<O>> {
        self.sender
            .as_ref()
            .expect("worker sender exists until pool shutdown")
    }

    fn shutdown_and_join(&mut self) {
        if let Some(sender) = self.sender.take() {
            for _ in 0..self.handles.len() {
                let _ = sender.send(WorkerMessage::Shutdown);
            }
            drop(sender);
        }
        // This synchronous join preserves same-thread native destruction. It
        // can still wait indefinitely if an in-flight native call never
        // returns; there is no safe detach/cancel workaround for that case.
        for handle in self.handles.drain(..) {
            if let Err(worker_panic) = handle.join() {
                tracing::error!(
                    panic = panic_message(worker_panic.as_ref()),
                    "renderer worker thread terminated with a panic"
                );
            }
        }
    }
}

impl<O: Send + 'static> Drop for WorkerGroup<O> {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

fn spawn_worker_group<B, F>(
    worker_count: usize,
    queue_capacity: usize,
    backend_factory: F,
) -> MaptileResult<WorkerGroup<B::Output>>
where
    B: RenderBackend,
    F: Fn(usize) -> B + Send + Sync + 'static,
{
    let (sender, receiver) = flume::bounded(queue_capacity);
    let backend_factory = Arc::new(backend_factory);
    let mut handles = Vec::with_capacity(worker_count);

    for index in 0..worker_count {
        let receiver = receiver.clone();
        let backend_factory = backend_factory.clone();
        match thread::Builder::new()
            .name(format!("maptile-render-{index}"))
            .spawn(move || worker_loop(receiver, backend_factory(index)))
        {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                let mut group = WorkerGroup {
                    sender: Some(sender),
                    handles,
                };
                group.shutdown_and_join();
                return Err(MaptileError::Render(format!(
                    "failed to spawn render worker {index}: {error}"
                )));
            }
        }
    }

    Ok(WorkerGroup {
        sender: Some(sender),
        handles,
    })
}

struct WorkerState {
    default_size: (u32, u32),
    renderers: HashMap<u8, MapLibreRenderer>,
}

impl RenderBackend for WorkerState {
    type Output = RenderedImage;

    fn render(&mut self, request: &RenderRequest) -> MaptileResult<Self::Output> {
        if request.isolate_native_state {
            // A scoped asset render owns its glyph/sprite atlas and resource
            // callbacks until completion. Drop them on this same worker thread.
            // Pixel caching is handled above the renderer, after live permission checks.
            let mut renderer =
                MapLibreRenderer::new(request.width, request.height, f32::from(request.scale))?;
            return renderer.render(request);
        }
        if !self.renderers.contains_key(&request.scale) {
            let renderer = MapLibreRenderer::new(
                self.default_size.0,
                self.default_size.1,
                f32::from(request.scale),
            )?;
            self.renderers.insert(request.scale, renderer);
        }
        self.renderers
            .get_mut(&request.scale)
            .expect("renderer was inserted for the requested scale")
            .render(request)
    }

    fn reset(&mut self) {
        self.renderers.clear();
    }
}

pub struct RendererPool {
    workers: WorkerGroup<RenderedImage>,
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub struct RenderQueueAdmissionForTest {
    _receiver: tokio::sync::oneshot::Receiver<MaptileResult<RenderedImage>>,
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub struct BlockingRenderControlForTest {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub struct BlockingRenderReleaseForTest {
    release: mpsc::Sender<()>,
}

#[cfg(debug_assertions)]
impl BlockingRenderControlForTest {
    pub fn wait_entered(&self, timeout: std::time::Duration) -> MaptileResult<()> {
        self.entered.recv_timeout(timeout).map_err(|error| {
            MaptileError::Render(format!("blocking test renderer did not enter: {error}"))
        })
    }

    pub fn release(&self) {
        let _ = self.release.send(());
    }

    pub fn release_on_drop(&self) -> BlockingRenderReleaseForTest {
        BlockingRenderReleaseForTest {
            release: self.release.clone(),
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for BlockingRenderReleaseForTest {
    fn drop(&mut self) {
        let _ = self.release.send(());
    }
}

impl RendererPool {
    pub fn new(cfg: PoolConfig) -> MaptileResult<Self> {
        let validated = cfg.validated()?;
        let default_size = cfg.default_size;
        let workers = spawn_worker_group(
            validated.worker_count,
            validated.queue_capacity,
            move |_| WorkerState {
                default_size,
                renderers: HashMap::new(),
            },
        )?;
        Ok(Self { workers })
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn new_blocking_for_test(
        cfg: PoolConfig,
    ) -> MaptileResult<(Self, BlockingRenderControlForTest)> {
        struct BlockingBackend {
            first: Arc<AtomicBool>,
            entered: mpsc::Sender<()>,
            release: Arc<Mutex<mpsc::Receiver<()>>>,
        }

        impl RenderBackend for BlockingBackend {
            type Output = RenderedImage;

            fn render(&mut self, _request: &RenderRequest) -> MaptileResult<Self::Output> {
                if self.first.swap(false, Ordering::SeqCst) {
                    let _ = self.entered.send(());
                    self.release
                        .lock()
                        .expect("blocking test release receiver should not be poisoned")
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .map_err(|error| {
                            MaptileError::Render(format!(
                                "blocking test renderer release timed out: {error}"
                            ))
                        })?;
                }
                Err(MaptileError::Render(
                    "blocking test renderer released".into(),
                ))
            }

            fn reset(&mut self) {}
        }

        let validated = cfg.validated()?;
        let first = Arc::new(AtomicBool::new(true));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let workers = spawn_worker_group(validated.worker_count, validated.queue_capacity, {
            let first = first.clone();
            let entered = entered_tx.clone();
            let release = release_rx.clone();
            move |_| BlockingBackend {
                first: first.clone(),
                entered: entered.clone(),
                release: release.clone(),
            }
        })?;
        Ok((
            Self { workers },
            BlockingRenderControlForTest {
                entered: entered_rx,
                release: release_tx,
            },
        ))
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub async fn enqueue_for_test(&self) -> MaptileResult<RenderQueueAdmissionForTest> {
        let (response, receiver) = tokio::sync::oneshot::channel();
        self.workers
            .sender()
            .send_async(WorkerMessage::Render(RenderJob {
                request: RenderRequest::for_tile(
                    "{}",
                    0,
                    0,
                    0,
                    1,
                    crate::render::types::RenderFormat::Png,
                ),
                response,
                keepalive: None,
            }))
            .await
            .map_err(|_| {
                MaptileError::Render("render queue closed before test submission".into())
            })?;
        Ok(RenderQueueAdmissionForTest {
            _receiver: receiver,
        })
    }

    /// Render with a 100 ms queue admission timeout.
    /// Returns [`MaptileError::CapacityExhausted`] if admission times out.
    pub async fn render(&self, request: RenderRequest) -> MaptileResult<RenderedImage> {
        self.try_render_with_timeout(request, std::time::Duration::from_millis(100))
            .await
    }

    /// Apply a caller deadline across admission and execution. A running native call
    /// keeps its worker and resources until it returns.
    pub async fn render_until(
        &self,
        request: RenderRequest,
        admission_timeout: std::time::Duration,
        deadline: tokio::time::Instant,
    ) -> MaptileResult<RenderedImage> {
        if deadline <= tokio::time::Instant::now() {
            return Err(MaptileError::Deadline);
        }
        tokio::time::timeout_at(
            deadline,
            self.try_render_with_timeout(request, admission_timeout),
        )
        .await
        .map_err(|_| MaptileError::Deadline)?
    }

    /// Returns [`MaptileError::CapacityExhausted`] if the queue cannot accept
    /// the request within `admission_timeout`. This does not time out rendering.
    pub async fn try_render_with_timeout(
        &self,
        request: RenderRequest,
        admission_timeout: std::time::Duration,
    ) -> MaptileResult<RenderedImage> {
        self.try_render_with_timeout_and_keepalive(request, admission_timeout, None)
            .await
    }

    /// Like [`Self::try_render_with_timeout`], retaining `keepalive` through execution.
    pub async fn try_render_with_timeout_and_keepalive(
        &self,
        request: RenderRequest,
        admission_timeout: std::time::Duration,
        keepalive: Option<RenderKeepalive>,
    ) -> MaptileResult<RenderedImage> {
        let request = request.validate().map_err(MaptileError::from)?;
        let (response, receiver) = tokio::sync::oneshot::channel();
        submit_with_timeout(
            self.workers.sender(),
            RenderJob {
                request,
                response,
                keepalive,
            },
            admission_timeout,
        )
        .await?;
        receiver
            .await
            .map_err(|_| MaptileError::Render("render worker lost the response".into()))?
    }
}

async fn submit_with_timeout<O: Send + 'static>(
    sender: &flume::Sender<WorkerMessage<O>>,
    job: RenderJob<O>,
    admission_timeout: std::time::Duration,
) -> MaptileResult<()> {
    tokio::time::timeout(
        admission_timeout,
        sender.send_async(WorkerMessage::Render(job)),
    )
    .await
    .map_err(|_| MaptileError::CapacityExhausted)?
    .map_err(|_| MaptileError::Render("render queue closed before submission".into()))
}

#[cfg(test)]
async fn submit_for_test_with_timeout<O: Send + 'static>(
    sender: &flume::Sender<WorkerMessage<O>>,
    job: RenderJob<O>,
    admission_timeout: std::time::Duration,
) -> MaptileResult<()> {
    submit_with_timeout(sender, job, admission_timeout).await
}

#[cfg(test)]
mod tests;
