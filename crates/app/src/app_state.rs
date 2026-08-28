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
use fs_core::{FileClipboard, JobQueue, Platform, RealVfs, Spawner, UndoStack, Vfs};
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, BackgroundExecutor, Entity, Global};

use crate::jobs_model::JobsModel;

/// The undo/redo stack, shared between the [`JobsModel`] pump (which pushes
/// completed-op inverses) and the workspace's `Undo`/`Redo` handlers. An
/// async mutex: `UndoStack::undo/redo` await Vfs metadata while validating
/// fingerprints, and a blocking lock held across that await would deadlock
/// the single-threaded foreground executor.
pub type SharedUndoStack = Arc<futures::lock::Mutex<UndoStack>>;

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

/// An [`Opener`] that records what it was asked to open, for tests that assert
/// *which* entries a command handed over (e.g. `OpenSelected` over a
/// multi-selection). The log is shared, so the test keeps its own handle.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingOpener(pub Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>);

#[cfg(test)]
impl Opener for RecordingOpener {
    fn open(&self, path: &Path) {
        self.0.lock().unwrap().push(path.to_path_buf());
    }
}

/// Global filesystem context. M1 carries the Vfs, Spawner, and the opener
/// stub; M2 adds the [`Platform`] handle (volumes + eject); M3 adds the
/// [`JobQueue`], the shared [`UndoStack`], and the [`JobsModel`] handle
/// (ARCHITECTURE.md §2 — views observe the model; nothing else touches the
/// queue's event channel).
pub struct FsContext {
    pub vfs: Arc<dyn Vfs>,
    pub spawner: Arc<dyn Spawner>,
    pub opener: Arc<dyn Opener>,
    pub platform: Arc<dyn Platform>,
    pub queue: Arc<JobQueue>,
    pub undo: SharedUndoStack,
    pub jobs: Entity<JobsModel>,
    /// The cut/copy file clipboard (ARCHITECTURE.md §2/§6): a plain struct —
    /// cut membership drives render dimming; paste turns it into a `FileOp`.
    pub clipboard: FileClipboard,
}

impl Global for FsContext {}

impl FsContext {
    pub fn global(cx: &App) -> &FsContext {
        cx.global::<FsContext>()
    }

    /// Mutable access for clipboard writes (Cut/Copy/Paste). Views that
    /// mutate it `cx.notify()` themselves — the global carries no observers.
    pub fn global_mut(cx: &mut App) -> &mut FsContext {
        cx.global_mut::<FsContext>()
    }
}

/// Build the M3 job spine (queue → JobsModel → undo stack) around the given
/// seams and set the [`FsContext`] global. The single place the spine is
/// wired, shared by boot ([`init`]), the visual test runner, and tests.
pub fn install(
    cx: &mut App,
    vfs: Arc<dyn Vfs>,
    spawner: Arc<dyn Spawner>,
    opener: Arc<dyn Opener>,
    platform: Arc<dyn Platform>,
) -> Entity<JobsModel> {
    // M6b: `with_platform`, not `new` — `FileOp::SetTags` and `FileOp::Chown`
    // need the OS services behind the `Platform` seam, and a queue built
    // without one refuses them.
    let queue = JobQueue::with_platform(vfs.clone(), platform.clone(), spawner.clone());
    let undo: SharedUndoStack = Arc::new(futures::lock::Mutex::new(UndoStack::new()));
    let jobs = cx.new(|cx| {
        JobsModel::new(
            queue.clone(),
            vfs.clone(),
            undo.clone(),
            spawner.clone(),
            cx,
        )
    });
    cx.set_global(FsContext {
        vfs,
        spawner,
        opener,
        platform,
        queue,
        undo,
        jobs: jobs.clone(),
        clipboard: FileClipboard::default(),
    });
    jobs
}

/// Install the real [`FsContext`] (RealVfs over the app's background
/// executor). Called once at boot by `main`.
pub fn init(cx: &mut App) {
    let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
    let vfs: Arc<dyn Vfs> = Arc::new(RealVfs::new(spawner.clone()));
    #[cfg(target_os = "macos")]
    let platform: Arc<dyn Platform> = Arc::new(fs_core::MacPlatform::new(spawner.clone()));
    #[cfg(not(target_os = "macos"))]
    let platform: Arc<dyn Platform> = Arc::new(fs_core::StubPlatform::new());
    install(cx, vfs, spawner, Arc::new(LoggingOpener), platform);
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
