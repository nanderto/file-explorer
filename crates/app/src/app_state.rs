//! App-wide filesystem context (ARCHITECTURE.md §2 `FsContext`, §5 threading).
//!
//! [`FsContext`] is the gpui global holding the only door to the disk
//! (`Arc<dyn Vfs>`) plus the [`Spawner`] seam fs-core was constructed with.
//! [`GpuiSpawner`] is the ~20-line adapter that implements fs-core's `Spawner`
//! on top of `gpui::BackgroundExecutor`, so all fs-core debounce/delay logic
//! runs on gpui's (test-controllable) clock and every blocking call runs on
//! the background thread pool — the UI thread never touches the disk.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fs_core::exec::UnblockClosure;
use fs_core::{RealVfs, Spawner, Vfs};
use futures::future::BoxFuture;
use gpui::{App, BackgroundExecutor, Global};

/// Opens a file in its default application. Routed through [`FsContext`] like
/// the Vfs so views never call the OS directly and tests can record requests.
pub trait Opener: Send + Sync {
    fn open(&self, path: &Path);
}

/// M1 stub [`Opener`]: logs the request. The real implementation is trivial
/// with the `open` crate and joins the `Platform` trait work (M2).
pub struct LoggingOpener;

impl Opener for LoggingOpener {
    fn open(&self, path: &Path) {
        eprintln!("open: {}", path.display());
    }
}

/// Global filesystem context. M1 carries the Vfs, Spawner, and the opener
/// stub; the job queue, undo stack, clipboard, and `JobsModel` handle join at
/// M3 (additive).
pub struct FsContext {
    pub vfs: Arc<dyn Vfs>,
    pub spawner: Arc<dyn Spawner>,
    pub opener: Arc<dyn Opener>,
}

impl Global for FsContext {}

impl FsContext {
    pub fn global(cx: &App) -> &FsContext {
        cx.global::<FsContext>()
    }
}

/// Install the real [`FsContext`] (RealVfs over the app's background
/// executor). Called once at boot by `main` and the visual test runner.
pub fn init(cx: &mut App) {
    let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
    let vfs: Arc<dyn Vfs> = Arc::new(RealVfs::new(spawner.clone()));
    cx.set_global(FsContext {
        vfs,
        spawner,
        opener: Arc::new(LoggingOpener),
    });
}

/// fs-core [`Spawner`] implemented on `gpui::BackgroundExecutor`
/// (ARCHITECTURE.md §5.2). Under `#[gpui::test]` the executor is gpui's
/// deterministic `TestDispatcher`, so timers obey `advance_clock`.
pub struct GpuiSpawner {
    executor: BackgroundExecutor,
}

impl GpuiSpawner {
    pub fn new(executor: BackgroundExecutor) -> Self {
        Self { executor }
    }
}

impl Spawner for GpuiSpawner {
    fn spawn(&self, fut: BoxFuture<'static, ()>) {
        self.executor.spawn(fut).detach();
    }

    fn timer(&self, dur: Duration) -> BoxFuture<'static, ()> {
        Box::pin(self.executor.timer(dur))
    }

    fn unblock_raw(&self, f: UnblockClosure) -> BoxFuture<'static, Box<dyn std::any::Any + Send>> {
        Box::pin(self.executor.spawn(async move { f() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_core::SpawnerExt as _;
    use gpui::TestAppContext;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[gpui::test]
    async fn unblock_round_trips_values_off_thread(cx: &mut TestAppContext) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor.clone()));
        assert_eq!(spawner.unblock(|| 21 * 2).await, 42);
        assert_eq!(
            spawner.unblock(|| String::from("computed")).await,
            "computed"
        );
    }

    #[gpui::test]
    async fn spawn_runs_futures_and_timer_uses_the_test_clock(cx: &mut TestAppContext) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor.clone()));

        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let timer = spawner.timer(Duration::from_millis(100));
        spawner.spawn(Box::pin(async move {
            timer.await;
            flag.store(true, Ordering::SeqCst);
        }));

        cx.background_executor.run_until_parked();
        assert!(
            !fired.load(Ordering::SeqCst),
            "timer must not fire before the clock advances"
        );

        cx.background_executor
            .advance_clock(Duration::from_millis(100));
        cx.background_executor.run_until_parked();
        assert!(fired.load(Ordering::SeqCst), "timer fires once time passes");
    }
}
