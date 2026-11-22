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
fn zero_workers_and_zero_queue_depth_are_rejected() {
    let invalid_workers = PoolConfig {
        max_concurrent: 0,
        default_size: (512, 512),
        queue_depth_per_worker: 2,
    }
    .validated();
    assert!(matches!(invalid_workers, Err(MaptileError::Config(_))));

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
    let error = submit_with_timeout(
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
    assert!(matches!(error, MaptileError::CapacityExhausted));
}

#[tokio::test]
async fn timed_submission_to_closed_queue_is_render_error() {
    let (sender, receiver) = flume::bounded::<WorkerMessage<()>>(1);
    drop(receiver);
    let (response, _) = tokio::sync::oneshot::channel();
    let error = submit_with_timeout(
        &sender,
        RenderJob {
            request: request(),
            response,
            keepalive: None,
        },
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, MaptileError::Render(_)));
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
    submit_with_timeout(
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
