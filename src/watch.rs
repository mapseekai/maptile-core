//! Filesystem watcher for tile-source directories.
//!
//! Wraps the `notify` crate so callers can subscribe to a tokio mpsc
//! receiver of [`WatchEvent`]s without dealing with notify's threaded
//! callback model. Pair with
//! [`crate::sources::manager::SourceManager::reload_from_discovery`]
//! to mutate the registry on change.
//!
//! Sandbox note: notify uses kqueue on macOS, inotify on Linux, and
//! ReadDirectoryChangesW on Windows. The watcher's internal thread is
//! kept alive for the lifetime of the [`SourceWatcher`] handle.

use crate::error::{MaptileError, MaptileResult};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Capacity of the bounded watch event channel. Sized to absorb typical
/// rsync / `git checkout` event bursts without growing memory unbounded;
/// excess events are dropped with a warn log so subscribers never block
/// the notify thread.
const WATCH_QUEUE_CAP: usize = 1024;

/// One filesystem change event surfaced through [`SourceWatcher::subscribe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A new file appeared (Created/CreatedAndModified collapsed).
    Added(PathBuf),
    /// An existing file was modified in place (data change, not metadata).
    Modified(PathBuf),
    /// A file was removed or renamed away.
    Removed(PathBuf),
}

/// Filesystem watcher. Drop the handle to stop watching.
pub struct SourceWatcher {
    _watcher: Box<dyn notify::Watcher + Send + 'static>,
    /// Sender held alive on the watcher so subscribe() can clone receivers
    /// from a buffered fanout.
    _tx: mpsc::Sender<WatchEvent>,
    rx: Arc<Mutex<Option<mpsc::Receiver<WatchEvent>>>>,
    root: PathBuf,
}

impl SourceWatcher {
    /// Begin watching `root` recursively. Events are coalesced by `notify`'s
    /// platform-native debouncer.
    pub fn new(root: impl AsRef<Path>) -> MaptileResult<Self> {
        Self::with_debounce(root, Duration::from_millis(300))
    }

    /// Begin watching `root` with a custom debounce window. The window is
    /// applied by absorbing duplicate events for the same path within
    /// `debounce` of each other.
    pub fn with_debounce(root: impl AsRef<Path>, debounce: Duration) -> MaptileResult<Self> {
        use notify::{RecursiveMode, Watcher};
        let root = root.as_ref().to_path_buf();
        let (tx, rx) = mpsc::channel::<WatchEvent>(WATCH_QUEUE_CAP);
        let tx_for_watcher = tx.clone();
        let last_emit: Arc<Mutex<std::collections::HashMap<PathBuf, std::time::Instant>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        // Prune entries older than `prune_threshold` whenever the
        // dedup map crosses this size. Keeps `last_emit` bounded for
        // long-running daemons watching directories with churn.
        let prune_threshold = debounce.saturating_mul(8);
        const PRUNE_TRIGGER: usize = 1024;

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let evt = match res {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "notify watcher reported error; event dropped");
                    return;
                }
            };
            for path in &evt.paths {
                let kind = match evt.kind {
                    notify::EventKind::Create(_) => Some(WatchEvent::Added(path.clone())),
                    notify::EventKind::Remove(_) => Some(WatchEvent::Removed(path.clone())),
                    notify::EventKind::Modify(_) => Some(WatchEvent::Modified(path.clone())),
                    _ => None,
                };
                let Some(emit) = kind else { continue };
                // Debounce: drop duplicate events for the same path within window.
                let now = std::time::Instant::now();
                // PoisonError::into_inner — keep the watcher running even
                // if a previous notify thread panicked while holding the lock.
                let mut map = last_emit.lock().unwrap_or_else(|p| {
                    tracing::warn!("notify debounce mutex poisoned, recovering");
                    p.into_inner()
                });
                if let Some(prev) = map.get(path)
                    && now.duration_since(*prev) < debounce
                {
                    continue;
                }
                if map.len() >= PRUNE_TRIGGER {
                    map.retain(|_, t| now.duration_since(*t) < prune_threshold);
                }
                map.insert(path.clone(), now);
                drop(map);
                match tx_for_watcher.try_send(emit) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(
                            queue_cap = WATCH_QUEUE_CAP,
                            "watch event dropped: subscriber backpressure"
                        );
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        tracing::debug!("watch subscriber dropped; stopping notify forwarder");
                    }
                }
            }
        })
        .map_err(|e| MaptileError::Config(format!("notify watcher init: {e}")))?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| MaptileError::Config(format!("notify watch {}: {e}", root.display())))?;

        Ok(Self {
            _watcher: Box::new(watcher),
            _tx: tx,
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
}
