use duckdb::{Connection, InterruptHandle};
use std::sync::{Arc, Mutex};
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
    interrupt: Arc<InterruptHandle>,
    status: Mutex<Status>,
    wake: Notify,
}

impl QueryControl {
    pub(super) fn new(connection: &Connection) -> Arc<Self> {
        Arc::new(Self {
            interrupt: connection.interrupt_handle(),
            status: Mutex::new(Status::Running),
            wake: Notify::new(),
        })
    }

    pub(super) fn cancelled(&self) -> bool {
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            == Status::Cancelled
    }

    fn interrupt(&self) -> bool {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *status == Status::Done {
            return false;
        }
        *status = Status::Cancelled;
        self.interrupt.interrupt();
        true
    }

    pub(super) fn cancel(&self) {
        self.interrupt();
        self.wake.notify_one();
    }

    pub(super) fn watch(self: &Arc<Self>, deadline: Instant) -> CancelQuery {
        let query = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {},
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

pub(super) struct CancelQuery(Arc<QueryControl>);

impl Drop for CancelQuery {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) struct FinishQuery(pub(super) Arc<QueryControl>);

impl Drop for FinishQuery {
    fn drop(&mut self) {
        *self
            .0
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Status::Done;
        self.0.wake.notify_one();
    }
}
