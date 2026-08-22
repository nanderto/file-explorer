//! OS services behind a trait (ARCHITECTURE.md §6 `platform/`) — volumes and
//! eject for M2. Strictly OS integration, *not* file I/O (that is [`crate::Vfs`]).
//!
//! The trait grows additively in later milestones (tags, thumbnails, open,
//! reveal — M4/M5/M6). [`MacPlatform`](macos::MacPlatform) is the real
//! implementation behind `cfg(target_os = "macos")`; [`StubPlatform`] is the
//! portable implementation used for Windows/Linux development **and** for
//! deterministic tests and visual scenarios on every platform.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;

use crate::exec::Spawner;
use crate::watcher::WatchGuard;

#[cfg(target_os = "macos")]
mod macos;
mod stub;

#[cfg(target_os = "macos")]
pub use macos::MacPlatform;
pub use stub::StubPlatform;

/// Stable identity of a mounted volume. M2 keys it by mount path — unique per
/// mounted volume and exactly what both eject and navigation need.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VolumeId(pub Arc<str>);

impl VolumeId {
    pub fn from_path(path: &Path) -> Self {
        Self(path.to_string_lossy().into_owned().into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One mounted volume, as shown in the sidebar's Devices section.
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeInfo {
    pub volume_id: VolumeId,
    pub name: String,
    /// Mount point (root of the volume).
    pub path: PathBuf,
    /// Total capacity in bytes.
    pub total: u64,
    /// Free bytes.
    pub free: u64,
    /// Whether the volume can be ejected (shows the eject affordance).
    pub ejectable: bool,
}

/// OS services seam. Every implementation is safe to call from any thread;
/// the macOS implementation runs all blocking Objective-C / process calls
/// through [`crate::SpawnerExt::unblock`].
#[async_trait]
pub trait Platform: Send + Sync {
    /// The currently mounted volumes.
    async fn volumes(&self) -> Result<Vec<VolumeInfo>>;

    /// Unmount and eject the volume. Fails if the volume is unknown, busy, or
    /// not ejectable.
    async fn eject(&self, volume_id: &VolumeId) -> Result<()>;
}

/// Watch the volume list for changes by polling [`Platform::volumes`] every
/// `poll_interval` (driven by [`Spawner::timer`], so tests run on fake time).
///
/// The stream emits the current list immediately, then again whenever the
/// list differs from the previously emitted one. Dropping the [`WatchGuard`]
/// stops polling and ends the stream. Polling errors are skipped (the last
/// good list stands); an error on the *initial* read emits an empty list.
pub fn watch_volumes(
    platform: Arc<dyn Platform>,
    spawner: &Arc<dyn Spawner>,
    poll_interval: Duration,
) -> (BoxStream<'static, Vec<VolumeInfo>>, WatchGuard) {
    let (tx, rx) = async_channel::unbounded();
    let alive = Arc::new(AtomicBool::new(true));
    let pump_alive = alive.clone();
    let timer_spawner = spawner.clone();
    spawner.spawn(Box::pin(async move {
        let mut last = platform.volumes().await.unwrap_or_default();
        if tx.send(last.clone()).await.is_err() {
            return; // stream consumer already gone
        }
        loop {
            timer_spawner.timer(poll_interval).await;
            if !pump_alive.load(Ordering::SeqCst) {
                return; // guard dropped — dropping tx ends the stream
            }
            let Ok(current) = platform.volumes().await else {
                continue;
            };
            if current != last {
                last = current.clone();
                if tx.send(current).await.is_err() {
                    return;
                }
            }
        }
    }));
    let guard = WatchGuard::new(move || alive.store(false, Ordering::SeqCst));
    (rx.boxed(), guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::TestSpawner;
    use futures::executor::block_on;

    fn ejectable_names(volumes: &[VolumeInfo]) -> Vec<&str> {
        volumes
            .iter()
            .filter(|v| v.ejectable)
            .map(|v| v.name.as_str())
            .collect()
    }

    #[test]
    fn stub_volumes_are_fixed_and_deterministic() {
        let platform = StubPlatform::new();
        let first = block_on(platform.volumes()).unwrap();
        let second = block_on(platform.volumes()).unwrap();
        assert_eq!(first, second, "repeated reads are identical");
        assert_eq!(
            first,
            block_on(StubPlatform::new().volumes()).unwrap(),
            "every instance serves the same fixed list"
        );

        let names: Vec<&str> = first.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Macintosh HD", "External SSD", "Camera"]);
        assert_eq!(ejectable_names(&first), ["External SSD", "Camera"]);
        assert_eq!(first[0].path, PathBuf::from("/"));
        assert!(first.iter().all(|v| v.total > 0 && v.free < v.total));
        assert!(
            first
                .iter()
                .all(|v| v.volume_id == VolumeId::from_path(&v.path))
        );
    }

    #[test]
    fn stub_eject_removes_only_ejectable_volumes() {
        let platform = StubPlatform::new();
        let root = VolumeId::from_path(Path::new("/"));
        let ssd = VolumeId::from_path(Path::new("/Volumes/External SSD"));

        let err = block_on(platform.eject(&root)).unwrap_err();
        assert!(err.to_string().contains("not ejectable"), "{err}");

        block_on(platform.eject(&ssd)).unwrap();
        let names: Vec<String> = block_on(platform.volumes())
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(names, ["Macintosh HD", "Camera"]);

        let err = block_on(platform.eject(&ssd)).unwrap_err();
        assert!(err.to_string().contains("no such volume"), "{err}");
    }

    #[test]
    fn watch_volumes_emits_initial_list_then_changes_then_ends_on_guard_drop() {
        let spawner_impl = Arc::new(TestSpawner::new());
        let spawner: Arc<dyn Spawner> = spawner_impl.clone();
        let platform = Arc::new(StubPlatform::new());
        let interval = Duration::from_secs(5);

        let (mut stream, guard) =
            watch_volumes(platform.clone() as Arc<dyn Platform>, &spawner, interval);

        let initial = block_on(stream.next()).expect("initial emission");
        assert_eq!(initial.len(), 3);

        // An unchanged poll emits nothing; the next change is coalesced into
        // one emission carrying the up-to-date list.
        spawner_impl.advance(interval);
        block_on(platform.eject(&VolumeId::from_path(Path::new("/Volumes/Camera")))).unwrap();
        spawner_impl.advance(interval);
        let updated = block_on(stream.next()).expect("changed emission");
        assert_eq!(updated.len(), 2);
        assert!(updated.iter().all(|v| v.name != "Camera"));

        drop(guard);
        spawner_impl.advance(interval);
        assert_eq!(block_on(stream.next()), None, "guard drop ends the stream");
    }
}
