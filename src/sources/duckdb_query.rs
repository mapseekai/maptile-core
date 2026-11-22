#[cfg(test)]
use duckdb::Connection;
use duckdb::InterruptHandle;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::{sync::Notify, time::Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Cancelled,
    Done,
}

/// Interruption is serialized with completion before a connection is reused.
pub(super) struct QueryControl {
    state: Mutex<QueryState>,
    wake: Notify,
    cancelled: Notify,
}

struct QueryState {
    status: Status,
    interrupt: Option<Interrupt>,

    #[cfg(test)]
    pause_interrupt: Option<InterruptPause>,
}

struct Interrupt {
    handle: Arc<InterruptHandle>,
    tainted: Option<Arc<AtomicBool>>,
}

impl Interrupt {
    fn cancel(&self) {
        if let Some(tainted) = &self.tainted {
            tainted.store(true, Ordering::Release);
        }
        self.handle.interrupt();
    }
}

#[cfg(test)]
struct InterruptPause {
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl QueryControl {
    pub(super) fn pending() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueryState {
                status: Status::Running,
                interrupt: None,
                #[cfg(test)]
                pause_interrupt: None,
            }),
            wake: Notify::new(),
            cancelled: Notify::new(),
        })
    }

    #[cfg(test)]
    pub(super) fn attach(self: &Arc<Self>, connection: &Connection) -> AttachedQuery {
        self.bind(connection.interrupt_handle(), None)
    }

    pub(super) fn bind(
        self: &Arc<Self>,
        interrupt: Arc<InterruptHandle>,
        tainted: Option<Arc<AtomicBool>>,
    ) -> AttachedQuery {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interrupt = Interrupt {
            handle: interrupt,
            tainted,
        };
        debug_assert!(state.interrupt.is_none());
        if state.status == Status::Cancelled {
            interrupt.cancel();
        }
        state.interrupt = Some(interrupt);
        AttachedQuery {
            query: self.clone(),
        }
    }

    pub(super) async fn wait_cancelled(&self) {
        let notified = self.cancelled.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.cancelled() {
            notified.await;
        }
    }

    #[cfg(test)]
    pub(super) fn assert_detached(&self) {
        assert!(self.state.lock().unwrap().interrupt.is_none());
    }

    pub(super) fn cancelled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status
            == Status::Cancelled
    }

    fn interrupt(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.status == Status::Done {
            return false;
        }
        state.status = Status::Cancelled;
        #[cfg(test)]
        let pause = state.pause_interrupt.take();
        #[cfg(test)]
        if let Some(pause) = &pause {
            pause.entered.send(()).unwrap();
            pause.resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if let Some(interrupt) = &state.interrupt {
            interrupt.cancel();
        }
        #[cfg(test)]
        if let Some(pause) = pause {
            pause
                .finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        true
    }

    pub(super) fn cancel(&self) {
        self.interrupt();
        self.cancelled.notify_waiters();
        self.wake.notify_one();
    }

    pub(super) fn watch(self: &Arc<Self>, deadline: Instant) -> CancelQuery {
        let query = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => query.cancel(),
                () = query.wake.notified() => {},
            }
            // DuckDB resets an interrupt sent before a query starts.
            while query.interrupt() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        CancelQuery(self.clone())
    }
}

/// Disarm this borrow generation before returning or destroying its connection.
pub(super) struct AttachedQuery {
    query: Arc<QueryControl>,
}

impl Drop for AttachedQuery {
    fn drop(&mut self) {
        self.query
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .interrupt
            .take();
    }
}

pub(super) struct CancelQuery(Arc<QueryControl>);

impl Drop for CancelQuery {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) struct FinishQuery(pub(super) Arc<QueryControl>);

impl Drop for FinishQuery {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.interrupt = None;
        state.status = Status::Done;
        drop(state);
        self.0.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CheckedConnection {
        connection: Connection,
        running: Arc<QueryControl>,
        dropped: Arc<AtomicBool>,
    }

    impl CheckedConnection {
        fn new(running: &Arc<QueryControl>) -> Self {
            Self {
                connection: Connection::open_in_memory().unwrap(),
                running: running.clone(),
                dropped: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl Drop for CheckedConnection {
        fn drop(&mut self) {
            self.running.assert_detached();
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn error_and_unwind_detach_before_connection_drops_even_before_finish() {
        for panic in [false, true] {
            let running = QueryControl::pending();
            let _completion = FinishQuery(running.clone());
            let connection = CheckedConnection::new(&running);
            let dropped = connection.dropped.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let connection = connection;
                let _attachment = running.attach(&connection.connection);
                running.cancel();
                if panic {
                    panic!("exercise unwind after attaching");
                }
                connection.connection.execute_batch("invalid SQL")
            }));
            if panic {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().is_err());
            }
            assert!(dropped.load(Ordering::SeqCst));
            assert!(running.cancelled());
            running.assert_detached();
            // The outer FinishQuery is deliberately still alive.
            running.cancel();
        }
    }

    #[test]
    fn attachment_drop_waits_for_in_flight_interrupt() {
        let running = QueryControl::pending();
        let _completion = FinishQuery(running.clone());
        let finished = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let (drop_tx, drop_rx) = std::sync::mpsc::channel();
        let (dropping_tx, dropping) = std::sync::mpsc::channel();
        let (dropped_tx, dropped) = std::sync::mpsc::channel();
        let owner = std::thread::spawn({
            let running = running.clone();
            let finished = finished.clone();
            move || {
                let connection = CheckedConnection::new(&running);
                let attachment = running.attach(&connection.connection);
                ready_tx.send(()).unwrap();
                drop_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                dropping_tx.send(()).unwrap();
                drop(attachment);
                assert!(finished.load(Ordering::SeqCst));
                drop(connection);
                dropped_tx.send(()).unwrap();
            }
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (resume_tx, resume) = std::sync::mpsc::channel();
        running.state.lock().unwrap().pause_interrupt = Some(InterruptPause {
            entered: entered_tx,
            resume,
            finished: finished.clone(),
        });
        let interrupter = std::thread::spawn({
            let running = running.clone();
            move || running.cancel()
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        drop_tx.send(()).unwrap();
        dropping.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(running.state.try_lock().is_err());
        assert!(!finished.load(Ordering::SeqCst));
        assert!(matches!(
            dropped.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        resume_tx.send(()).unwrap();
        interrupter.join().unwrap();
        dropped.recv_timeout(Duration::from_secs(5)).unwrap();
        owner.join().unwrap();
        running.assert_detached();
    }

    #[test]
    fn unstarted_blocking_closure_drops_connection_without_publishing_interrupt() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
            .block_on(async {
                let (entered, ready) = tokio::sync::oneshot::channel();
                let (release, wait) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    entered.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                });
                ready.await.unwrap();
                let running = QueryControl::pending();
                let cancellation = running.watch(Instant::now() + Duration::from_secs(10));
                let connection = CheckedConnection::new(&running);
                let dropped = connection.dropped.clone();
                let task = tokio::task::spawn_blocking({
                    let running = running.clone();
                    move || {
                        let connection = connection;
                        let _completion = FinishQuery(running.clone());
                        let _attachment = running.attach(&connection.connection);
                        panic!("aborted queued closure must not start");
                    }
                });
                drop(cancellation);
                running.assert_detached();
                task.abort();
                release.send(()).unwrap();
                blocker.await.unwrap();
                assert!(task.await.unwrap_err().is_cancelled());
                assert!(dropped.load(Ordering::SeqCst));
                running.assert_detached();
                drop(FinishQuery(running));
            });
    }

    #[tokio::test]
    async fn pending_cancellation_survives_attach_and_completion_disarms_it() {
        let running = QueryControl::pending();
        let cancellation = running.watch(Instant::now() + Duration::from_secs(10));
        drop(cancellation);
        assert!(running.cancelled());
        let connection = Connection::open_in_memory().unwrap();
        let _attachment = running.attach(&connection);
        assert!(running.cancelled());
        drop(FinishQuery(running.clone()));
        assert!(!running.interrupt());
        let value: i32 = connection
            .query_row("SELECT 42", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_after_attach_interrupts_query_and_releases_worker() {
        let running = QueryControl::pending();
        let cancellation = running.watch(Instant::now() + Duration::from_secs(10));
        let (entered, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking({
            let running = running.clone();
            move || {
                let _completion = FinishQuery(running.clone());
                let connection = Connection::open_in_memory().unwrap();
                let _attachment = running.attach(&connection);
                entered.send(()).unwrap();
                connection.query_row(
                    "SELECT sum(i * j) FROM range(1000000000) a(i), range(1000000000) b(j)",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            }
        });
        ready.await.unwrap();
        drop(cancellation);
        assert!(running.cancelled());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), worker)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(!running.interrupt());
    }
}
