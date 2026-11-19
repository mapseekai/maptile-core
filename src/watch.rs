//! Filesystem watcher for tile-source directories.
//!
//! Wraps the `notify` crate so callers can subscribe to a tokio mpsc
//! receiver of [`WatchEvent`]s without dealing with notify's threaded
//! callback model. Pair with
//! [`crate::sources::manager::SourceManager::reload_from_discovery`]
//! to mutate the registry on change.
//!
//! Sandbox note: notify uses kqueue on macOS, inotify on Linux, and
//! ReadDirectoryChangesW on Windows. The watcher's internal thread and
//! one debounce thread are kept alive for the lifetime of the
//! [`SourceWatcher`] handle and stop when it is dropped.

use crate::error::{MaptileError, MaptileResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Default per-path debounce window used by [`SourceWatcher::new`].
const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);

/// Capacity of the bounded watch event channel. Sized to absorb typical
/// rsync / `git checkout` event bursts without growing memory unbounded;
/// overflow is coalesced into a root-directory rescan notification.
/// The pending-path map uses the same bound.
const WATCH_QUEUE_CAP: usize = 1024;

/// One filesystem change event surfaced through [`SourceWatcher::subscribe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A new file appeared (Created/CreatedAndModified collapsed).
    Added(PathBuf),
    /// A file or directory changed. After buffer overflow, `Modified(root)`
    /// requests a full rescan once the subscriber can receive it.
    Modified(PathBuf),
    /// A file was removed or renamed away.
    Removed(PathBuf),
}

impl WatchEvent {
    fn path(&self) -> &Path {
        match self {
            Self::Added(path) | Self::Modified(path) | Self::Removed(path) => path,
        }
    }
}

/// One bounded set of per-path windows, shared with a single flush thread.
struct Debouncer {
    window: Duration,
    root: PathBuf,
    tx: mpsc::Sender<WatchEvent>,
    state: Mutex<DebounceState>,
    wake: Condvar,
}

#[derive(Default)]
struct DebounceState {
    paths: HashMap<PathBuf, PathWindow>,
    rescan: bool,
    shutdown: bool,
}

struct PathWindow {
    opened: Instant,
    pending: Vec<WatchEvent>,
}

impl Debouncer {
    fn new(window: Duration, root: PathBuf, tx: mpsc::Sender<WatchEvent>) -> Self {
        Self {
            window,
            root,
            tx,
            state: Mutex::new(DebounceState::default()),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DebounceState> {
        self.state.lock().unwrap_or_else(|p| {
            tracing::warn!("notify debounce mutex poisoned, recovering");
            p.into_inner()
        })
    }

    fn record(&self, event: WatchEvent, now: Instant) {
        let mut state = self.lock();
        if state.shutdown {
            return;
        }
        if self.window.is_zero() {
            state.rescan |= self.forward(event);
        } else if let Some(window) = state.paths.get_mut(event.path()) {
            push_pending(&mut window.pending, event);
            return;
        } else if state.paths.len() < WATCH_QUEUE_CAP {
            state.paths.insert(
                event.path().to_path_buf(),
                PathWindow {
                    opened: now,
                    pending: Vec::new(),
                },
            );
            state.rescan |= self.forward(event);
        } else {
            state.rescan = true;
        }
        self.wake.notify_one();
    }

    /// Flush expired windows and retry an overflow notification until a
    /// subscriber can receive it. All sends share the lock to retain order.
    fn flush_due(&self, state: &mut DebounceState, now: Instant) -> Option<Duration> {
        let mut next: Option<Duration> = None;
        state.paths.retain(|_, window| {
            let remaining = self
                .window
                .saturating_sub(now.duration_since(window.opened));
            if !state.shutdown && !remaining.is_zero() {
                next = Some(next.map_or(remaining, |t| t.min(remaining)));
                return true;
            }
            for event in window.pending.drain(..) {
                state.rescan |= self.forward(event);
            }
            false
        });
        if state.rescan {
            state.rescan = self.forward(WatchEvent::Modified(self.root.clone()));
            if state.rescan {
                let retry = Duration::from_millis(10);
                next = Some(next.map_or(retry, |t| t.min(retry)));
            }
        }
        next
    }

    fn run(&self) {
        let mut state = self.lock();
        loop {
            let next = self.flush_due(&mut state, Instant::now());
            if state.shutdown {
                return;
            }
            state = match next {
                None => self.wake.wait(state).unwrap_or_else(|p| p.into_inner()),
                Some(wait) => {
                    self.wake
                        // Bound the OS wait without changing custom debounce durations.
                        .wait_timeout(state, wait.min(Duration::from_secs(1)))
                        .unwrap_or_else(|p| p.into_inner())
                        .0
                }
            };
        }
    }

    fn shutdown(&self) {
        self.lock().shutdown = true;
        self.wake.notify_one();
    }

    /// True means the subscriber needs a rescan after backpressure clears.
    fn forward(&self, event: WatchEvent) -> bool {
        matches!(
            self.tx.try_send(event),
            Err(mpsc::error::TrySendError::Full(_))
        )
    }
}

/// Keep one pending event per kind, ordered by latest occurrence.
fn push_pending(pending: &mut Vec<WatchEvent>, event: WatchEvent) {
    let kind = std::mem::discriminant(&event);
    pending.retain(|e| std::mem::discriminant(e) != kind);
    pending.push(event);
}

/// Filesystem watcher. Drop the handle to stop watching.
pub struct SourceWatcher {
    watcher: Option<Box<dyn notify::Watcher + Send + 'static>>,
    debouncer: Arc<Debouncer>,
    flusher: Option<std::thread::JoinHandle<()>>,
    rx: Arc<Mutex<Option<mpsc::Receiver<WatchEvent>>>>,
    root: PathBuf,
}

impl SourceWatcher {
    /// Begin watching `root` recursively with the default 300 ms
    /// per-path debounce window. See [`Self::with_debounce`].
    pub fn new(root: impl AsRef<Path>) -> MaptileResult<Self> {
        Self::with_debounce(root, DEFAULT_DEBOUNCE)
    }

    /// Begin watching `root` with a custom per-path debounce window.
    ///
    /// The first event for a path is delivered immediately. Further events
    /// for that path inside the window are coalesced and delivered when the
    /// window closes: each distinct kind observed is delivered once, ordered
    /// by its latest occurrence, so the last event for a path reflects the
    /// latest observed change. A create followed by a delete therefore still
    /// ends with [`WatchEvent::Removed`], and a parent directory in its
    /// window still gets a trailing [`WatchEvent::Modified`].
    ///
    /// The channel and pending-path map are bounded. On overflow, individual
    /// events are coalesced into a [`WatchEvent::Modified`] for `root`, retried
    /// until the subscriber has space; consumers must rescan the directory.
    /// Dropping the watcher stops both threads and flushes pending events
    /// only if channel capacity remains; it never waits for the subscriber.
    pub fn with_debounce(root: impl AsRef<Path>, debounce: Duration) -> MaptileResult<Self> {
        use notify::{RecursiveMode, Watcher};
        let root = root.as_ref().to_path_buf();
        let (tx, rx) = mpsc::channel::<WatchEvent>(WATCH_QUEUE_CAP);
        let debouncer = Arc::new(Debouncer::new(debounce, root.clone(), tx));
        let callback_debouncer = Arc::downgrade(&debouncer);

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Some(callback_debouncer) = callback_debouncer.upgrade() else {
                return;
            };
            let evt = match res {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "notify watcher reported error; event dropped");
                    return;
                }
            };
            let now = Instant::now();
            for path in &evt.paths {
                let event = match evt.kind {
                    notify::EventKind::Create(_) => WatchEvent::Added(path.clone()),
                    notify::EventKind::Remove(_) => WatchEvent::Removed(path.clone()),
                    notify::EventKind::Modify(_) => WatchEvent::Modified(path.clone()),
                    _ => continue,
                };
                callback_debouncer.record(event, now);
            }
        })
        .map_err(|e| MaptileError::Config(format!("notify watcher init: {e}")))?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| MaptileError::Config(format!("notify watch {}: {e}", root.display())))?;

        let flush_debouncer = Arc::clone(&debouncer);
        let flusher = std::thread::Builder::new()
            .name("maptile-watch-debounce".into())
            .spawn(move || flush_debouncer.run())
            .map_err(|e| MaptileError::Config(format!("watch debounce thread: {e}")))?;

        Ok(Self {
            watcher: Some(Box::new(watcher)),
            debouncer,
            flusher: Some(flusher),
            rx: Arc::new(Mutex::new(Some(rx))),
            root,
        })
    }

    /// Take ownership of the underlying receiver. May only be called once
    /// per [`SourceWatcher`]; subsequent calls return `None`.
    pub fn subscribe(&self) -> Option<mpsc::Receiver<WatchEvent>> {
        self.rx
            .lock()
            .unwrap_or_else(|p| {
                tracing::warn!("watcher receiver mutex poisoned, recovering");
                p.into_inner()
            })
            .take()
    }

    /// Path being watched.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for SourceWatcher {
    fn drop(&mut self) {
        // Request notify shutdown before stopping the flush thread.
        self.watcher.take();
        self.debouncer.shutdown();
        if let Some(flusher) = self.flusher.take()
            && flusher.join().is_err()
        {
            tracing::warn!("watch debounce thread panicked");
        }
    }
}

impl std::fmt::Debug for SourceWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceWatcher")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// Result of a discovery-driven reload. Exposed via
/// [`crate::sources::manager::SourceManager::reload_from_discovery`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_emits_event_when_file_added() {
        let tmp = tempfile::tempdir().unwrap();
        let watcher = SourceWatcher::new(tmp.path()).unwrap();
        let mut rx = watcher.subscribe().expect("first subscribe should succeed");

        // Give the watcher a moment to register the recursive watch.
        tokio::time::sleep(Duration::from_millis(100)).await;

        std::fs::write(tmp.path().join("new.pmtiles"), b"x").unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("watcher event timeout")
            .expect("watcher channel closed");

        match evt {
            WatchEvent::Added(p) | WatchEvent::Modified(p) => {
                assert!(p.ends_with("new.pmtiles"), "got path {p:?}");
            }
            other => panic!("expected Added/Modified, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_subscribe_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let watcher = SourceWatcher::new(tmp.path()).unwrap();
        assert!(watcher.subscribe().is_some());
        assert!(watcher.subscribe().is_none());
    }

    #[test]
    fn root_returns_watched_path() {
        let tmp = tempfile::tempdir().unwrap();
        let watcher = SourceWatcher::new(tmp.path()).unwrap();
        assert_eq!(watcher.root(), tmp.path());
    }

    /// Collect events until `deadline`, recording when each arrived.
    async fn collect_until(
        rx: &mut mpsc::Receiver<WatchEvent>,
        deadline: tokio::time::Instant,
    ) -> Vec<(tokio::time::Instant, WatchEvent)> {
        let mut events = Vec::new();
        while let Ok(Some(evt)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            events.push((tokio::time::Instant::now(), evt));
        }
        events
    }

    fn last_for<'a>(
        events: &'a [(tokio::time::Instant, WatchEvent)],
        path: &Path,
    ) -> Option<&'a WatchEvent> {
        events
            .iter()
            .rev()
            .map(|(_, e)| e)
            .find(|e| e.path() == path)
    }

    // Regression for MC-05: with the default window, a file created and
    // removed while its parent directory is cooling down must still end
    // with `Removed`, and the parent must get a trailing `Modified`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_window_reports_final_remove_during_parent_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let file_a = root.join("a.pmtiles");
        let file_b = root.join("b.pmtiles");
        std::fs::write(&file_a, b"init-a").unwrap();

        let watcher = SourceWatcher::new(&root).unwrap();
        let mut rx = watcher.subscribe().unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        while rx.try_recv().is_ok() {}

        // Opens the root's window.
        std::fs::remove_file(&file_a).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        std::fs::write(&file_b, b"temp-b").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        std::fs::remove_file(&file_b).unwrap();
        let removed_at = tokio::time::Instant::now();

        let events = collect_until(&mut rx, removed_at + Duration::from_millis(1500)).await;
        assert!(!file_b.exists());
        assert_eq!(
            last_for(&events, &file_b),
            Some(&WatchEvent::Removed(file_b.clone())),
            "events: {events:?}"
        );
        // kqueue also reports directory writes; other backends need not.
        #[cfg(target_os = "macos")]
        assert!(
            events
                .iter()
                .any(|(at, e)| *at > removed_at && *e == WatchEvent::Modified(root.clone())),
            "parent directory needs a trailing Modified after b was removed: {events:?}"
        );
    }

    // Regression for MC-05: an in-place rewrite inside the default window
    // must still be reported after the rewrite happened.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_window_reports_in_place_update_after_create() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let file_c = root.join("c.pmtiles");

        let watcher = SourceWatcher::new(&root).unwrap();
        let mut rx = watcher.subscribe().unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        std::fs::write(&file_c, b"version-1").unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first event timeout")
            .expect("channel closed");
        assert!(first.path().starts_with(&root), "got {first:?}");

        tokio::time::sleep(Duration::from_millis(30)).await;
        std::fs::write(&file_c, b"version-2-updated-content").unwrap();
        let rewritten_at = tokio::time::Instant::now();

        let events = collect_until(&mut rx, rewritten_at + Duration::from_millis(1500)).await;
        assert!(
            events
                .iter()
                .any(|(at, e)| *at > rewritten_at && *e == WatchEvent::Modified(file_c.clone())),
            "rewrite inside the window must surface as Modified: {events:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_watcher_stops_threads_and_closes_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let watcher = SourceWatcher::new(tmp.path()).unwrap();
        let mut rx = watcher.subscribe().unwrap();
        let debouncer = Arc::downgrade(&watcher.debouncer);
        drop(watcher);
        let closed = tokio::time::timeout(Duration::from_secs(3), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "channel stayed open after drop");
        assert!(debouncer.upgrade().is_none(), "debounce state leaked");
    }

    #[test]
    fn zero_window_delivers_without_coalescing() {
        let (tx, mut rx) = mpsc::channel(16);
        let root = PathBuf::from("/watch");
        let debouncer = Debouncer::new(Duration::ZERO, root.clone(), tx);
        let now = Instant::now();
        for _ in 0..3 {
            debouncer.record(WatchEvent::Modified(root.clone()), now);
        }
        assert!(debouncer.lock().paths.is_empty());
        for _ in 0..3 {
            assert_eq!(rx.try_recv().unwrap(), WatchEvent::Modified(root.clone()));
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn trailing_flush_keeps_each_kind_in_latest_order() {
        let (tx, mut rx) = mpsc::channel(16);
        let debouncer = Debouncer::new(Duration::from_millis(50), PathBuf::from("/watch"), tx);
        let path = PathBuf::from("/watch/a.pmtiles");
        let start = Instant::now();
        for (offset, event) in [
            (0, WatchEvent::Added(path.clone())),
            (1, WatchEvent::Removed(path.clone())),
            (2, WatchEvent::Modified(path.clone())),
            (3, WatchEvent::Added(path.clone())),
            (4, WatchEvent::Removed(path.clone())),
        ] {
            debouncer.record(event, start + Duration::from_millis(offset));
        }
        assert_eq!(rx.try_recv().unwrap(), WatchEvent::Added(path.clone()));
        assert!(rx.try_recv().is_err(), "events inside the window are held");

        let mut state = debouncer.lock();
        let next = debouncer.flush_due(&mut state, start + Duration::from_millis(10));
        assert_eq!(next, Some(Duration::from_millis(40)));
        assert!(rx.try_recv().is_err());
        let closed_at = start + Duration::from_millis(60);
        let next = debouncer.flush_due(&mut state, closed_at);
        assert_eq!(next, None);
        let flushed: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            flushed,
            [
                WatchEvent::Modified(path.clone()),
                WatchEvent::Added(path.clone()),
                WatchEvent::Removed(path.clone()),
            ]
        );
        assert!(state.paths.is_empty());
    }

    #[test]
    fn shutdown_forwards_pending_events() {
        let (tx, mut rx) = mpsc::channel(16);
        let debouncer = Arc::new(Debouncer::new(
            Duration::from_secs(60),
            PathBuf::from("/watch"),
            tx,
        ));
        let path = PathBuf::from("/watch/b.pmtiles");
        let now = Instant::now();
        debouncer.record(WatchEvent::Added(path.clone()), now);
        debouncer.record(WatchEvent::Removed(path.clone()), now);
        let runner = Arc::clone(&debouncer);
        let flusher = std::thread::spawn(move || runner.run());
        debouncer.shutdown();
        flusher.join().unwrap();
        assert_eq!(rx.try_recv().unwrap(), WatchEvent::Added(path.clone()));
        assert_eq!(rx.try_recv().unwrap(), WatchEvent::Removed(path));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backpressure_eventually_requests_rescan_and_drop_does_not_wait() {
        let (tx, mut rx) = mpsc::channel(1);
        let root = PathBuf::from("/watch");
        let debouncer = Arc::new(Debouncer::new(Duration::ZERO, root.clone(), tx));
        let now = Instant::now();
        debouncer.record(WatchEvent::Added(root.join("a")), now);
        debouncer.record(WatchEvent::Removed(root.join("a")), now);
        {
            let mut state = debouncer.lock();
            debouncer.flush_due(&mut state, now);
            assert!(state.rescan);
        }
        let runner = debouncer.clone();
        let flusher = std::thread::spawn(move || runner.run());
        assert!(matches!(rx.recv().await, Some(WatchEvent::Added(_))));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap(),
            Some(WatchEvent::Modified(root.clone())),
        );
        // Leave a full channel at shutdown: joining must not depend on recv().
        debouncer.record(WatchEvent::Added(root.join("b")), Instant::now());
        debouncer.record(WatchEvent::Removed(root.join("b")), Instant::now());
        debouncer.shutdown();
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || flusher.join().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[test]
    fn distinct_path_bursts_are_bounded_and_custom_windows_do_not_overflow() {
        let (tx, mut rx) = mpsc::channel(1);
        let root = PathBuf::from("/watch");
        let debouncer = Debouncer::new(Duration::MAX, root.clone(), tx);
        let now = Instant::now();
        for i in 0..WATCH_QUEUE_CAP * 4 {
            let path = root.join(i.to_string());
            debouncer.record(WatchEvent::Added(path.clone()), now);
            debouncer.record(WatchEvent::Removed(path), now);
        }
        let mut state = debouncer.lock();
        assert_eq!(state.paths.len(), WATCH_QUEUE_CAP);
        assert!(state.paths.values().all(|p| p.pending.len() <= 3));
        assert!(state.rescan);
        rx.try_recv().unwrap();
        assert_eq!(debouncer.flush_due(&mut state, now), Some(Duration::MAX));
        assert_eq!(rx.try_recv().unwrap(), WatchEvent::Modified(root));
        state.shutdown = true;
        debouncer.flush_due(&mut state, now);
        assert!(state.paths.is_empty());
    }
}
