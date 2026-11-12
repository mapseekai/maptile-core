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
    /// Number of durable render threads. Zero is normalized to one.
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
        let worker_count = self.max_concurrent.max(1);
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

    pub async fn render(&self, request: RenderRequest) -> MaptileResult<RenderedImage> {
        self.try_render_with_timeout(request, std::time::Duration::from_millis(100))
            .await
    }

    pub async fn try_render_with_timeout(
        &self,
        request: RenderRequest,
        admission_timeout: std::time::Duration,
    ) -> MaptileResult<RenderedImage> {
        self.try_render_with_timeout_and_keepalive(request, admission_timeout, None)
            .await
    }

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
    .map_err(|_| MaptileError::Render("render queue admission timed out".into()))?
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
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    #[derive(Clone)]
    struct TestBlock {
        entered: mpsc::Sender<()>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    struct TestBlockControl {
        entered: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
    }

    struct TestReleaseGuard {
        release: mpsc::Sender<()>,
    }

    impl TestBlockControl {
        fn wait_entered(&self) {
            self.entered
                .recv_timeout(Duration::from_secs(1))
                .expect("blocking test backend should enter");
        }

        fn release(&self) {
            let _ = self.release.send(());
        }

        fn release_on_drop(&self) -> TestReleaseGuard {
            TestReleaseGuard {
                release: self.release.clone(),
            }
        }
    }

    impl Drop for TestReleaseGuard {
        fn drop(&mut self) {
            let _ = self.release.send(());
        }
    }

    fn test_block_pair() -> (TestBlock, TestBlockControl) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        (
            TestBlock {
                entered: entered_tx,
                release: Arc::new(Mutex::new(release_rx)),
            },
            TestBlockControl {
                entered: entered_rx,
                release: release_tx,
            },
        )
    }

    struct FakeBackend {
        calls: Arc<AtomicUsize>,
        resets: Arc<AtomicUsize>,
        panic_once: Arc<AtomicBool>,
        reset_panic_once: Arc<AtomicBool>,
        block: Option<TestBlock>,
    }

    impl RenderBackend for FakeBackend {
        type Output = usize;

        fn render(&mut self, _request: &RenderRequest) -> MaptileResult<Self::Output> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(block) = self.block.take() {
                let _ = block.entered.send(());
                block
                    .release
                    .lock()
                    .expect("test release receiver should not be poisoned")
                    .recv_timeout(Duration::from_secs(2))
                    .expect("test backend should be released");
            }
            if self.panic_once.swap(false, Ordering::SeqCst) {
                panic!("fake backend panic");
            }
            Ok(self.calls.load(Ordering::SeqCst))
        }

        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::SeqCst);
            if self.reset_panic_once.swap(false, Ordering::SeqCst) {
                panic!("fake backend reset panic");
            }
        }
    }

    fn request() -> RenderRequest {
        RenderRequest::for_tile("{}", 0, 0, 0, 1, crate::render::types::RenderFormat::Png)
    }

    fn fake_backend(
        calls: Arc<AtomicUsize>,
        resets: Arc<AtomicUsize>,
        panic_once: Arc<AtomicBool>,
    ) -> FakeBackend {
        FakeBackend {
            calls,
            resets,
            panic_once,
            reset_panic_once: Arc::new(AtomicBool::new(false)),
            block: None,
        }
    }

    #[test]
    fn queue_capacity_scales_with_worker_count() {
        let cfg = PoolConfig {
            max_concurrent: 3,
            default_size: (512, 512),
            queue_depth_per_worker: 7,
        };

        assert_eq!(cfg.validated().unwrap().queue_capacity, 21);
    }

    #[test]
    fn zero_workers_clamp_to_one_and_zero_queue_depth_is_rejected() {
        let clamped = PoolConfig {
            max_concurrent: 0,
            default_size: (512, 512),
            queue_depth_per_worker: 2,
        }
        .validated()
        .unwrap();
        assert_eq!(clamped.worker_count, 1);
        assert_eq!(clamped.queue_capacity, 2);

        let invalid = PoolConfig {
            max_concurrent: 1,
            default_size: (512, 512),
            queue_depth_per_worker: 0,
        }
        .validated();
        assert!(matches!(invalid, Err(MaptileError::Config(_))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_queue_applies_backpressure_until_a_message_is_received() {
        let (block, control) = test_block_pair();
        struct BlockFirstBackend {
            first: bool,
            block: TestBlock,
        }
        impl RenderBackend for BlockFirstBackend {
            type Output = ();
            fn render(&mut self, _request: &RenderRequest) -> MaptileResult<()> {
                if self.first {
                    self.first = false;
                    let _ = self.block.entered.send(());
                    self.block
                        .release
                        .lock()
                        .expect("test release receiver should not be poisoned")
                        .recv_timeout(Duration::from_secs(2))
                        .expect("test backend should be released");
                }
                Ok(())
            }
            fn reset(&mut self) {}
        }
        let group = spawn_worker_group(1, 1, {
            let block = block.clone();
            move |_| BlockFirstBackend {
                first: true,
                block: block.clone(),
            }
        })
        .unwrap();
        let _release = control.release_on_drop();

        let (first_tx, _first_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send_async(WorkerMessage::Render(RenderJob {
                request: request(),
                response: first_tx,
                keepalive: None,
            }))
            .await
            .unwrap();
        control.wait_entered();

        let (second_tx, _second_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send_async(WorkerMessage::Render(RenderJob {
                request: request(),
                response: second_tx,
                keepalive: None,
            }))
            .await
            .unwrap();

        let (third_tx, _third_rx) = tokio::sync::oneshot::channel();
        let third_send = group.sender().send_async(WorkerMessage::Render(RenderJob {
            request: request(),
            response: third_tx,
            keepalive: None,
        }));
        tokio::pin!(third_send);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut third_send)
                .await
                .is_err()
        );

        control.release();
        third_send.await.unwrap();
    }

    #[test]
    fn already_cancelled_response_is_skipped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        let panic_once = Arc::new(AtomicBool::new(false));
        let mut group = spawn_worker_group(1, 1, {
            let (calls, resets, panic_once) = (calls.clone(), resets.clone(), panic_once.clone());
            move |_| fake_backend(calls.clone(), resets.clone(), panic_once.clone())
        })
        .unwrap();
        let (response, cancelled) = tokio::sync::oneshot::channel();
        drop(cancelled);

        group
            .sender()
            .send(WorkerMessage::Render(RenderJob {
                request: request(),
                response,
                keepalive: None,
            }))
            .unwrap();
        group.shutdown_and_join();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn in_flight_cancellation_finishes_backend_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        let (block, control) = test_block_pair();
        let mut group = spawn_worker_group(1, 1, {
            let (calls, resets, block) = (calls.clone(), resets.clone(), block.clone());
            move |_| FakeBackend {
                calls: calls.clone(),
                resets: resets.clone(),
                panic_once: Arc::new(AtomicBool::new(false)),
                reset_panic_once: Arc::new(AtomicBool::new(false)),
                block: Some(block.clone()),
            }
        })
        .unwrap();
        let _release = control.release_on_drop();
        let (response, cancelled) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send(WorkerMessage::Render(RenderJob {
                request: request(),
                response,
                keepalive: None,
            }))
            .unwrap();

        control.wait_entered();
        drop(cancelled);
        control.release();
        group.shutdown_and_join();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panic_reports_error_resets_backend_and_next_job_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        let panic_once = Arc::new(AtomicBool::new(true));
        let group = spawn_worker_group(1, 2, {
            let (calls, resets, panic_once) = (calls.clone(), resets.clone(), panic_once.clone());
            move |_| fake_backend(calls.clone(), resets.clone(), panic_once.clone())
        })
        .unwrap();

        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send(WorkerMessage::Render(RenderJob {
                request: request(),
                response: first_tx,
                keepalive: None,
            }))
            .unwrap();
        let first = first_rx.blocking_recv().unwrap();
        assert!(
            matches!(first, Err(MaptileError::Render(message)) if message == "render worker panicked and reset native state")
        );
        assert_eq!(resets.load(Ordering::SeqCst), 1);

        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send(WorkerMessage::Render(RenderJob {
                request: request(),
                response: second_tx,
                keepalive: None,
            }))
            .unwrap();
        assert_eq!(second_rx.blocking_recv().unwrap().unwrap(), 2);
    }

    #[test]
    fn reset_panic_is_reported_and_worker_processes_the_next_job() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        let panic_once = Arc::new(AtomicBool::new(true));
        let reset_panic_once = Arc::new(AtomicBool::new(true));
        let group = spawn_worker_group(1, 2, {
            let (calls, resets, panic_once, reset_panic_once) = (
                calls.clone(),
                resets.clone(),
                panic_once.clone(),
                reset_panic_once.clone(),
            );
            move |_| FakeBackend {
                calls: calls.clone(),
                resets: resets.clone(),
                panic_once: panic_once.clone(),
                reset_panic_once: reset_panic_once.clone(),
                block: None,
            }
        })
        .unwrap();

        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send(WorkerMessage::Render(RenderJob {
                request: request(),
                response: first_tx,
                keepalive: None,
            }))
            .unwrap();
        let first = first_rx.blocking_recv().unwrap();
        assert!(
            matches!(first, Err(MaptileError::Render(message)) if message == "render worker panicked; native-state reset also panicked")
        );
        assert_eq!(resets.load(Ordering::SeqCst), 1);

        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send(WorkerMessage::Render(RenderJob {
                request: request(),
                response: second_tx,
                keepalive: None,
            }))
            .unwrap();
        assert_eq!(second_rx.blocking_recv().unwrap().unwrap(), 2);
    }

    #[test]
    fn final_group_drop_joins_every_worker() {
        let dropped = Arc::new(AtomicUsize::new(0));
        struct DropBackend(Arc<AtomicUsize>);
        impl RenderBackend for DropBackend {
            type Output = ();
            fn render(&mut self, _request: &RenderRequest) -> MaptileResult<()> {
                Ok(())
            }
            fn reset(&mut self) {}
        }
        impl Drop for DropBackend {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let group = spawn_worker_group(3, 3, {
            let dropped = dropped.clone();
            move |_| DropBackend(dropped.clone())
        })
        .unwrap();
        drop(group);

        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn pool_config_default_is_sane() {
        let cfg = PoolConfig::default();
        assert!(cfg.max_concurrent >= 1);
        assert_eq!(cfg.queue_depth_per_worker, 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timed_submission_returns_overload_when_queue_remains_full() {
        let (block, control) = test_block_pair();
        let group = spawn_worker_group(1, 1, {
            let block = block.clone();
            move |_| FakeBackend {
                calls: Arc::new(AtomicUsize::new(0)),
                resets: Arc::new(AtomicUsize::new(0)),
                panic_once: Arc::new(AtomicBool::new(false)),
                reset_panic_once: Arc::new(AtomicBool::new(false)),
                block: Some(block.clone()),
            }
        })
        .unwrap();
        let _release = control.release_on_drop();

        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send_async(WorkerMessage::Render(RenderJob {
                request: request(),
                response: first_tx,
                keepalive: None,
            }))
            .await
            .unwrap();
        control.wait_entered();

        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send_async(WorkerMessage::Render(RenderJob {
                request: request(),
                response: second_tx,
                keepalive: None,
            }))
            .await
            .unwrap();

        let (third_tx, _third_rx) = tokio::sync::oneshot::channel();
        let error = submit_for_test_with_timeout(
            group.sender(),
            RenderJob {
                request: request(),
                response: third_tx,
                keepalive: None,
            },
            Duration::from_millis(25),
        )
        .await
        .unwrap_err();

        control.release();
        drop((first_rx, second_rx));
        assert!(
            matches!(error, MaptileError::Render(message) if message.contains("admission timed out"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_response_after_admission_skips_before_worker_pickup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (block, control) = test_block_pair();
        let mut group = spawn_worker_group(1, 2, {
            let (calls, block) = (calls.clone(), block.clone());
            move |_| FakeBackend {
                calls: calls.clone(),
                resets: Arc::new(AtomicUsize::new(0)),
                panic_once: Arc::new(AtomicBool::new(false)),
                reset_panic_once: Arc::new(AtomicBool::new(false)),
                block: Some(block.clone()),
            }
        })
        .unwrap();
        let _release = control.release_on_drop();

        let (first_tx, _first_rx) = tokio::sync::oneshot::channel();
        group
            .sender()
            .send_async(WorkerMessage::Render(RenderJob {
                request: request(),
                response: first_tx,
                keepalive: None,
            }))
            .await
            .unwrap();
        control.wait_entered();

        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        submit_for_test_with_timeout(
            group.sender(),
            RenderJob {
                request: request(),
                response: second_tx,
                keepalive: None,
            },
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        drop(second_rx);

        control.release();
        group.shutdown_and_join();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
