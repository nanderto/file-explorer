//! The executor seam — fs-core's only threading dependency (ARCHITECTURE.md §5).
//!
//! fs-core never sees gpui; it is handed a [`Spawner`] at construction. The app
//! crate implements `Spawner` for `gpui::BackgroundExecutor`; tests use
//! [`TestSpawner`], a simple thread-based implementation with a controllable
//! fake clock so all debounce/delay logic runs on fake time.

use std::any::Any;
use std::time::Duration;

use futures::future::BoxFuture;

#[cfg(any(test, feature = "test-support"))]
use futures::{FutureExt as _, channel::oneshot};
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Condvar, Mutex};

/// The boxed closure form used by the object-safe [`Spawner::unblock_raw`].
/// Use [`SpawnerExt::unblock`] instead of calling this directly.
pub type UnblockClosure = Box<dyn FnOnce() -> Box<dyn Any + Send> + Send>;

/// Executor abstraction fs-core code is written against.
///
/// * [`spawn`](Self::spawn) — fire-and-forget background futures (watcher
///   pumps, job lanes).
/// * [`timer`](Self::timer) — **all** debounce/delay logic goes through this,
///   so headless tests run on controllable fake time.
/// * [`unblock_raw`](Self::unblock_raw) — object-safe core for
///   [`SpawnerExt::unblock`]: runs a blocking closure off-thread and returns
///   its value.
pub trait Spawner: Send + Sync + 'static {
    /// Run a future to completion in the background.
    fn spawn(&self, fut: BoxFuture<'static, ()>);

    /// A future that resolves after `dur` (fake time under [`TestSpawner`]).
    fn timer(&self, dur: Duration) -> BoxFuture<'static, ()>;

    /// Object-safe core for `unblock()`; use [`SpawnerExt::unblock`], not this.
    fn unblock_raw(&self, f: UnblockClosure) -> BoxFuture<'static, Box<dyn Any + Send>>;
}

/// Typed convenience over [`Spawner::unblock_raw`].
pub trait SpawnerExt: Spawner {
    /// Run a blocking closure (`std::fs`, objc2, libc) off-thread and get its
    /// value back.
    fn unblock<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> BoxFuture<'static, T> {
        let fut = self.unblock_raw(Box::new(move || Box::new(f()) as Box<dyn Any + Send>));
        Box::pin(async move { *fut.await.downcast::<T>().expect("unblock type") })
    }
}

impl<S: Spawner + ?Sized> SpawnerExt for S {}

/// Thread-based [`Spawner`] with a controllable fake clock, for tests.
///
/// `spawn` and `unblock_raw` run on plain OS threads; `timer` registers with
/// the fake clock and only resolves when [`advance`](Self::advance) moves fake
/// time past its deadline. To avoid racing a background task that has not yet
/// registered its timer, `advance` briefly waits for at least one pending
/// timer before advancing.
#[cfg(any(test, feature = "test-support"))]
pub struct TestSpawner {
    clock: Arc<FakeClock>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestSpawner {
    pub fn new() -> Self {
        Self {
            clock: Arc::new(FakeClock::default()),
        }
    }

    /// Advance fake time by `by`, firing every timer whose deadline is reached.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// Current fake time (starts at zero).
    pub fn now(&self) -> Duration {
        self.clock.now()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for TestSpawner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Spawner for TestSpawner {
    fn spawn(&self, fut: BoxFuture<'static, ()>) {
        std::thread::Builder::new()
            .name("test-spawner".into())
            .spawn(move || futures::executor::block_on(fut))
            .expect("spawn test-spawner thread");
    }

    fn timer(&self, dur: Duration) -> BoxFuture<'static, ()> {
        if dur.is_zero() {
            return futures::future::ready(()).boxed();
        }
        let rx = self.clock.register(dur);
        Box::pin(async move {
            // A dropped clock resolves the timer immediately — tests are over.
            let _ = rx.await;
        })
    }

    fn unblock_raw(&self, f: UnblockClosure) -> BoxFuture<'static, Box<dyn Any + Send>> {
        let (tx, rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("test-unblock".into())
            .spawn(move || {
                let _ = tx.send(f());
            })
            .expect("spawn test-unblock thread");
        Box::pin(async move { rx.await.expect("unblock closure panicked") })
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct FakeClock {
    state: Mutex<ClockState>,
    timer_registered: Condvar,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct ClockState {
    now: Duration,
    pending: Vec<(Duration, oneshot::Sender<()>)>,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeClock {
    fn now(&self) -> Duration {
        self.state.lock().unwrap().now
    }

    fn register(&self, dur: Duration) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().unwrap();
        let deadline = state.now + dur;
        state.pending.push((deadline, tx));
        self.timer_registered.notify_all();
        rx
    }

    fn advance(&self, by: Duration) {
        let mut state = self.state.lock().unwrap();
        if state.pending.is_empty() {
            // Give in-flight background tasks a moment to register their
            // timers, so tests don't race the thread they just spawned.
            let (guard, _) = self
                .timer_registered
                .wait_timeout_while(state, Duration::from_secs(2), |s| s.pending.is_empty())
                .unwrap();
            state = guard;
        }
        state.now += by;
        let now = state.now;
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut state.pending)
            .into_iter()
            .partition(|(deadline, _)| *deadline <= now);
        state.pending = rest;
        drop(state);
        for (_, tx) in due {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    fn is_ready(fut: &mut BoxFuture<'static, ()>) -> bool {
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        matches!(fut.as_mut().poll(&mut cx), Poll::Ready(()))
    }

    #[test]
    fn unblock_round_trips_values() {
        let spawner = TestSpawner::new();
        let value =
            futures::executor::block_on(spawner.unblock(|| String::from("computed off-thread")));
        assert_eq!(value, "computed off-thread");

        let sum = futures::executor::block_on(spawner.unblock(|| (1..=10).sum::<u64>()));
        assert_eq!(sum, 55);
    }

    #[test]
    fn fake_clock_fires_timers_in_deadline_order() {
        let spawner = TestSpawner::new();
        let mut t5 = spawner.timer(Duration::from_millis(5));
        let mut t10 = spawner.timer(Duration::from_millis(10));

        assert!(!is_ready(&mut t5));
        assert!(!is_ready(&mut t10));

        spawner.advance(Duration::from_millis(5));
        assert!(is_ready(&mut t5), "5ms timer fires at t=5ms");
        assert!(!is_ready(&mut t10), "10ms timer must not fire at t=5ms");

        spawner.advance(Duration::from_millis(5));
        assert!(is_ready(&mut t10), "10ms timer fires at t=10ms");
        assert_eq!(spawner.now(), Duration::from_millis(10));
    }

    #[test]
    fn zero_duration_timer_is_immediately_ready() {
        let spawner = TestSpawner::new();
        let mut t = spawner.timer(Duration::ZERO);
        assert!(is_ready(&mut t));
    }

    #[test]
    fn spawn_runs_future_to_completion() {
        let spawner = TestSpawner::new();
        let (tx, rx) = oneshot::channel();
        spawner.spawn(Box::pin(async move {
            let _ = tx.send(42u32);
        }));
        assert_eq!(futures::executor::block_on(rx), Ok(42));
    }
}
