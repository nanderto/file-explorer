//! Debounced filesystem watching (ARCHITECTURE.md §6, `watcher.rs`).
//!
//! One process-global `notify` watcher with per-root registrations backs
//! [`crate::vfs::RealVfs::watch`]; `FakeVfs` routes injected events through the
//! same [`debounce`] pump. Raw events accumulate until `Spawner::timer(latency)`
//! elapses (fake time in tests), then flush as one coalesced
//! `Vec<PathEvent>` batch — a paste-storm becomes a single patch. Dropping the
//! returned [`WatchGuard`] unregisters the watch.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use notify::Watcher as _;

use crate::exec::Spawner;

/// One filesystem change, as delivered to listing consumers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathEvent {
    pub path: Arc<Path>,
    pub kind: PathEventKind,
}

/// What happened to [`PathEvent::path`]. `Rescan` means events were dropped or
/// unreliable and the consumer must reload the directory in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathEventKind {
    Created,
    Changed,
    Removed,
    Rescan,
}

/// RAII registration handle: dropping it unregisters the watch (and, for the
/// real watcher, unwatches the root once no registration needs it).
pub struct WatchGuard {
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl WatchGuard {
    pub fn new(on_drop: impl FnOnce() + Send + 'static) -> Self {
        Self {
            on_drop: Some(Box::new(on_drop)),
        }
    }

    /// A guard that unregisters nothing (used when registration failed and the
    /// stream is already terminated).
    pub fn noop() -> Self {
        Self { on_drop: None }
    }
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

/// Pump raw events into debounced, coalesced batches on `spawner`.
///
/// The pump waits for a first event, sleeps `latency` on [`Spawner::timer`]
/// (fake time in tests), drains everything that arrived meanwhile, and emits
/// one batch. The stream ends when every raw sender is dropped
/// (i.e. the watch was unregistered).
pub(crate) fn debounce(
    spawner: &Arc<dyn Spawner>,
    raw: async_channel::Receiver<PathEvent>,
    latency: Duration,
) -> BoxStream<'static, Vec<PathEvent>> {
    let (tx, rx) = async_channel::unbounded();
    let timer_spawner = spawner.clone();
    spawner.spawn(Box::pin(async move {
        while let Ok(first) = raw.recv().await {
            let mut batch = vec![first];
            timer_spawner.timer(latency).await;
            while let Ok(event) = raw.try_recv() {
                batch.push(event);
            }
            if tx.send(coalesce(batch)).await.is_err() {
                break;
            }
        }
    }));
    rx.boxed()
}

/// Collapse a raw batch: any `Rescan` swallows the batch (the consumer reloads
/// anyway); otherwise exact duplicates are dropped, keeping first-seen order.
fn coalesce(batch: Vec<PathEvent>) -> Vec<PathEvent> {
    if let Some(rescan) = batch
        .iter()
        .find(|e| e.kind == PathEventKind::Rescan)
        .cloned()
    {
        return vec![rescan];
    }
    let mut seen = HashSet::new();
    batch
        .into_iter()
        .filter(|event| seen.insert(event.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Real watcher: one process-global `notify` watcher, registrations per root.
// ---------------------------------------------------------------------------

struct Registration {
    root: PathBuf,
    tx: async_channel::Sender<PathEvent>,
}

#[derive(Default)]
struct Registry {
    subs: HashMap<u64, Registration>,
    next_id: u64,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

fn notify_watcher() -> &'static Mutex<Option<notify::RecommendedWatcher>> {
    static WATCHER: OnceLock<Mutex<Option<notify::RecommendedWatcher>>> = OnceLock::new();
    WATCHER.get_or_init(|| Mutex::new(None))
}

/// Translate one `notify` event into our path events.
fn map_notify_event(event: &notify::Event) -> Vec<PathEvent> {
    use notify::EventKind;
    use notify::event::{ModifyKind, RenameMode};

    if event.need_rescan() {
        let path: Arc<Path> = event
            .paths
            .first()
            .map(|p| Arc::from(p.as_path()))
            .unwrap_or_else(|| Arc::from(Path::new("")));
        return vec![PathEvent {
            path,
            kind: PathEventKind::Rescan,
        }];
    }

    let kinds: Vec<PathEventKind> = match &event.kind {
        EventKind::Create(_) => vec![PathEventKind::Created; event.paths.len()],
        EventKind::Remove(_) => vec![PathEventKind::Removed; event.paths.len()],
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            vec![PathEventKind::Removed; event.paths.len()]
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            vec![PathEventKind::Created; event.paths.len()]
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() == 2 => {
            vec![PathEventKind::Removed, PathEventKind::Created]
        }
        EventKind::Modify(_) | EventKind::Access(_) | EventKind::Any | EventKind::Other => {
            vec![PathEventKind::Changed; event.paths.len()]
        }
    };
    event
        .paths
        .iter()
        .zip(kinds)
        .map(|(path, kind)| PathEvent {
            path: Arc::from(path.as_path()),
            kind,
        })
        .collect()
}

fn route_notify_event(result: Result<notify::Event, notify::Error>) {
    let events = match &result {
        Ok(event) => map_notify_event(event),
        // A watcher error means we can no longer trust incremental events.
        Err(_) => vec![PathEvent {
            path: Arc::from(Path::new("")),
            kind: PathEventKind::Rescan,
        }],
    };
    let registry = registry().lock().unwrap();
    for event in events {
        for sub in registry.subs.values() {
            let matches = event.kind == PathEventKind::Rescan || event.path.starts_with(&sub.root);
            if matches {
                let _ = sub.tx.try_send(event.clone());
            }
        }
    }
}

/// Watch `path` (non-recursively) via the process-global `notify` watcher.
///
/// Best-effort: if the OS watch cannot be established, the returned stream is
/// already terminated and the guard is a no-op — the listing simply goes
/// unwatched (matching `Vfs::watch`'s infallible signature).
pub(crate) fn watch_real(
    spawner: &Arc<dyn Spawner>,
    path: &Path,
    latency: Duration,
) -> (BoxStream<'static, Vec<PathEvent>>, WatchGuard) {
    let mut watcher_slot = notify_watcher().lock().unwrap();
    if watcher_slot.is_none() {
        match notify::recommended_watcher(route_notify_event) {
            Ok(watcher) => *watcher_slot = Some(watcher),
            Err(_) => return (futures::stream::empty().boxed(), WatchGuard::noop()),
        }
    }
    let watcher = watcher_slot.as_mut().expect("watcher initialized above");
    if watcher
        .watch(path, notify::RecursiveMode::NonRecursive)
        .is_err()
    {
        return (futures::stream::empty().boxed(), WatchGuard::noop());
    }
    drop(watcher_slot);

    let (tx, rx) = async_channel::unbounded();
    let id = {
        let mut registry = registry().lock().unwrap();
        let id = registry.next_id;
        registry.next_id += 1;
        registry.subs.insert(
            id,
            Registration {
                root: path.to_path_buf(),
                tx,
            },
        );
        id
    };

    let root = path.to_path_buf();
    let guard = WatchGuard::new(move || {
        let mut registry = registry().lock().unwrap();
        registry.subs.remove(&id);
        let root_still_needed = registry.subs.values().any(|sub| sub.root == root);
        drop(registry);
        if !root_still_needed && let Some(watcher) = notify_watcher().lock().unwrap().as_mut() {
            let _ = watcher.unwatch(&root);
        }
    });

    (debounce(spawner, rx, latency), guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::TestSpawner;
    use crate::vfs::{FakeVfs, Vfs};
    use futures::executor::block_on;
    use serde_json::json;

    fn event(path: &str, kind: PathEventKind) -> PathEvent {
        PathEvent {
            path: Arc::from(Path::new(path)),
            kind,
        }
    }

    #[test]
    fn coalesce_drops_exact_duplicates_keeping_order() {
        let batch = vec![
            event("/d/a", PathEventKind::Created),
            event("/d/b", PathEventKind::Changed),
            event("/d/a", PathEventKind::Created),
            event("/d/a", PathEventKind::Changed),
        ];
        let coalesced = coalesce(batch);
        assert_eq!(
            coalesced,
            vec![
                event("/d/a", PathEventKind::Created),
                event("/d/b", PathEventKind::Changed),
                event("/d/a", PathEventKind::Changed),
            ]
        );
    }

    #[test]
    fn coalesce_collapses_batch_to_single_rescan() {
        let batch = vec![
            event("/d/a", PathEventKind::Created),
            event("/d", PathEventKind::Rescan),
            event("/d/b", PathEventKind::Removed),
        ];
        assert_eq!(coalesce(batch), vec![event("/d", PathEventKind::Rescan)]);
    }

    #[test]
    fn debounce_batches_paused_then_flushed_events_on_fake_time() {
        let test_spawner = Arc::new(TestSpawner::new());
        let spawner: Arc<dyn Spawner> = test_spawner.clone();
        let vfs = FakeVfs::new(spawner);
        vfs.insert_tree("/dir", json!({ "existing.txt": "x" }));

        let (mut stream, _guard) = vfs.watch(Path::new("/dir"), Duration::from_millis(100));

        vfs.pause_events();
        vfs.insert_file("/dir/a.txt", 1);
        vfs.insert_file("/dir/b.txt", 2);
        vfs.insert_file("/dir/c.txt", 3);
        vfs.flush_events();

        test_spawner.advance(Duration::from_millis(100));

        let batch = block_on(stream.next()).expect("one debounced batch");
        let paths: Vec<_> = batch.iter().map(|e| e.path.clone()).collect();
        assert_eq!(batch.len(), 3, "paste-storm coalesced into one batch");
        assert!(paths.contains(&Arc::from(Path::new("/dir/a.txt"))));
        assert!(paths.contains(&Arc::from(Path::new("/dir/b.txt"))));
        assert!(paths.contains(&Arc::from(Path::new("/dir/c.txt"))));
        assert!(batch.iter().all(|e| e.kind == PathEventKind::Created));
    }

    #[test]
    fn later_events_form_a_second_batch() {
        let test_spawner = Arc::new(TestSpawner::new());
        let spawner: Arc<dyn Spawner> = test_spawner.clone();
        let vfs = FakeVfs::new(spawner);
        vfs.insert_tree("/dir", json!({}));

        let (mut stream, _guard) = vfs.watch(Path::new("/dir"), Duration::from_millis(50));

        vfs.insert_file("/dir/first.txt", 1);
        test_spawner.advance(Duration::from_millis(50));
        let first = block_on(stream.next()).expect("first batch");
        assert_eq!(first.len(), 1);

        vfs.remove_path("/dir/first.txt");
        test_spawner.advance(Duration::from_millis(50));
        let second = block_on(stream.next()).expect("second batch");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].kind, PathEventKind::Removed);
    }

    #[test]
    fn dropping_the_guard_unregisters_and_ends_the_stream() {
        let test_spawner = Arc::new(TestSpawner::new());
        let spawner: Arc<dyn Spawner> = test_spawner.clone();
        let vfs = FakeVfs::new(spawner);
        vfs.insert_tree("/dir", json!({}));

        let (mut stream, guard) = vfs.watch(Path::new("/dir"), Duration::from_millis(10));
        assert_eq!(vfs.watcher_count(), 1);

        drop(guard);
        assert_eq!(vfs.watcher_count(), 0, "guard drop unregisters");

        // Events after unregistration reach nobody, and the stream terminates.
        vfs.insert_file("/dir/late.txt", 1);
        assert_eq!(block_on(stream.next()), None);
    }
}
