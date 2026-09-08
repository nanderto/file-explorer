//! [`BackgroundWatchGuard`] — shared by every `Vfs::watch` caller (the pane's
//! live directory, the theme folder's hot reload).

use fs_core::watcher::WatchGuard;
use gpui::BackgroundExecutor;

/// A [`WatchGuard`] whose *unregistration* is kept off the UI thread.
///
/// Registering a watch is not the only blocking, disk-touching half of
/// `Vfs::watch`: dropping the guard calls the backend's `unwatch`, which on
/// macOS stops and joins an FSEvents run-loop thread and canonicalizes the
/// path again. So the guard is never dropped in place — dropping this wrapper
/// hands it to the background executor (§5: the UI thread never touches the
/// disk).
pub(crate) struct BackgroundWatchGuard {
    guard: Option<WatchGuard>,
    executor: BackgroundExecutor,
}

impl BackgroundWatchGuard {
    pub(crate) fn new(guard: WatchGuard, executor: BackgroundExecutor) -> Self {
        Self {
            guard: Some(guard),
            executor,
        }
    }
}

impl Drop for BackgroundWatchGuard {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            self.executor
                .spawn(async move {
                    drop(guard);
                })
                .detach();
        }
    }
}
