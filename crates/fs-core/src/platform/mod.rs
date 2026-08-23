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

use crate::attrs::FileAttrs;
use crate::exec::Spawner;
use crate::thumbnail::Thumbnail;
use crate::watcher::WatchGuard;

#[cfg(target_os = "macos")]
pub(crate) mod macos;
mod stub;
pub(crate) mod trash;

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

    /// A thumbnail of `path` whose longest edge is at most `px` pixels
    /// (aspect ratio preserved, so the result is usually smaller on one axis
    /// and never upscaled past the source).
    ///
    /// Returns decoded RGBA rather than an encoded blob — see [`Thumbnail`] for
    /// why. Errors are ordinary and expected: not every file *has* a
    /// thumbnail, and the icon grid falls back to a type icon rather than
    /// treating the failure as fatal. Callers should treat an `Err` as "no
    /// preview available", and must not retry in a loop.
    ///
    /// Every implementation runs its blocking work (QuickLook, objc2, image
    /// decode) through [`crate::SpawnerExt::unblock`]; the UI thread only ever
    /// awaits this.
    ///
    /// **Cancellation is expected and must be safe.** The icon grid drops this
    /// future whenever the tile it belongs to scrolls out of view, so an
    /// implementation must leave nothing behind that a drop would corrupt: work
    /// already handed to a background thread may run to completion and be
    /// discarded, but no shared state may be left half-written.
    async fn thumbnail(&self, path: &Path, px: u32) -> Result<Thumbnail>;

    /// Attributes that need an OS call beyond [`crate::Vfs::metadata`]: unix
    /// mode, owner/group names, the locked flag, Date Added, extension-hidden,
    /// and the localized type description (M5's info panel).
    ///
    /// Blocking work goes through [`crate::SpawnerExt::unblock`], so the UI
    /// thread only ever awaits this. An `Err` means the item could not be
    /// stat'ed at all (gone, or unreadable); a *field* that could not be
    /// resolved degrades to `None`/`false` inside a successful [`FileAttrs`],
    /// because the panel would rather show four of six rows than none.
    async fn file_attrs(&self, path: &Path) -> Result<FileAttrs>;
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
    fn stub_thumbnails_are_deterministic_and_path_dependent() {
        let path = Path::new("/root/photo.png");
        let first = block_on(StubPlatform::new().thumbnail(path, 64)).unwrap();
        let second = block_on(StubPlatform::new().thumbnail(path, 64)).unwrap();
        assert_eq!(first, second, "same path + size ⇒ byte-identical pixels");

        let other =
            block_on(StubPlatform::new().thumbnail(Path::new("/root/other.png"), 64)).unwrap();
        assert_ne!(
            first.rgba(),
            other.rgba(),
            "different paths get visibly different pixels"
        );

        // Not a flat swatch — the grid needs something that reads as an image.
        assert!(
            first.rgba().chunks_exact(4).map(|px| px[0]).max()
                > first.rgba().chunks_exact(4).map(|px| px[0]).min(),
            "the synthesized pattern varies across the tile"
        );
        assert!(
            first.rgba().chunks_exact(4).all(|px| px[3] == 0xff),
            "synthesized thumbnails are fully opaque"
        );
    }

    #[test]
    fn stub_thumbnails_fit_the_requested_box_and_preserve_a_ratio() {
        let platform = StubPlatform::new();
        // Three different paths, chosen to cover the stub's three aspect
        // ratios: square, landscape, portrait.
        let mut shapes = Vec::new();
        for name in ["a", "b", "c", "d", "e", "f"] {
            let thumbnail =
                block_on(platform.thumbnail(&PathBuf::from(format!("/{name}")), 40)).unwrap();
            assert!(
                thumbnail.width() <= 40 && thumbnail.height() <= 40,
                "{name}: {}x{} exceeds the 40px box",
                thumbnail.width(),
                thumbnail.height()
            );
            assert_eq!(thumbnail.width().max(thumbnail.height()), 40, "{name}");
            assert_eq!(
                thumbnail.byte_len(),
                (thumbnail.width() * thumbnail.height() * 4) as usize
            );
            shapes.push((thumbnail.width(), thumbnail.height()));
        }
        assert!(
            shapes.iter().any(|(w, h)| w == h)
                && shapes.iter().any(|(w, h)| w > h)
                && shapes.iter().any(|(w, h)| w < h),
            "the stub produces square, landscape and portrait tiles: {shapes:?}"
        );
    }

    #[test]
    fn stub_thumbnail_rejects_sizes_outside_the_contract() {
        let platform = StubPlatform::new();
        let path = Path::new("/root/photo.png");
        for px in [0, crate::thumbnail::MAX_PX + 1] {
            let err = block_on(platform.thumbnail(path, px)).unwrap_err();
            assert!(err.to_string().contains("out of range"), "{err}");
        }
        // The smallest legal request still yields a valid 1px-edge thumbnail.
        let tiny = block_on(platform.thumbnail(path, 1)).unwrap();
        assert_eq!(tiny.width().max(tiny.height()), 1);
        assert_eq!(tiny.byte_len(), 4);
    }

    #[test]
    fn stub_file_attrs_are_deterministic_and_path_derived() {
        let platform = StubPlatform::new();
        let path = Path::new("/root/photo.png");

        let first = block_on(platform.file_attrs(path)).unwrap();
        let second = block_on(StubPlatform::new().file_attrs(path)).unwrap();
        assert_eq!(first, second, "same path ⇒ identical attributes");

        // Fixed, asserted values — the whole point of the stub.
        assert_eq!(first.owner.as_deref(), Some("stub-owner"));
        assert_eq!(first.group.as_deref(), Some("stub-group"));
        assert_eq!(first.type_description.as_deref(), Some("PNG image"));
        let mode = first.perms.expect("perms").mode();
        assert!(
            [0o644, 0o755, 0o600, 0o664].contains(&mode),
            "unexpected stub mode {mode:o}"
        );

        // Date Added sits inside the fixed 2024-01-01 day, with no clock read.
        let base = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_704_067_200);
        let added = first.added.expect("added");
        assert!(added >= base && added < base + Duration::from_secs(86_400));

        // Different paths get different attributes (not one constant blob).
        let other = block_on(platform.file_attrs(Path::new("/root/notes.md"))).unwrap();
        assert_eq!(
            other.type_description.as_deref(),
            Some("Plain text document")
        );
        assert_ne!(other.added, first.added);

        // No extension ⇒ nothing to describe.
        let extensionless = block_on(platform.file_attrs(Path::new("/root/Makefile"))).unwrap();
        assert_eq!(extensionless.type_description, None);
    }

    /// The stub's `locked` and `extension_hidden` are path-derived, so **both**
    /// values occur — which is why no test may assert one of them over a path
    /// it did not fix (a `tempfile` directory's random suffix, say). Pinned
    /// here because the portable `tests/attrs.rs` used to assert `!locked` over
    /// exactly such paths and failed on roughly half of all runs off macOS.
    #[test]
    fn stub_file_attrs_flags_are_path_derived_not_constant() {
        let platform = StubPlatform::new();
        let flags: Vec<(bool, bool)> = (0..64)
            .map(|ix| {
                let attrs =
                    block_on(platform.file_attrs(&PathBuf::from(format!("/root/f{ix}.txt"))))
                        .unwrap();
                (attrs.locked, attrs.extension_hidden)
            })
            .collect();
        assert!(
            flags.iter().any(|(locked, _)| *locked) && flags.iter().any(|(locked, _)| !*locked),
            "the stub's locked flag must vary with the path: {flags:?}"
        );
        assert!(
            flags.iter().any(|(_, hidden)| *hidden) && flags.iter().any(|(_, hidden)| !*hidden),
            "…and so must extension_hidden: {flags:?}"
        );
    }

    #[test]
    fn stub_file_attrs_need_no_filesystem() {
        // A path that cannot exist still answers — visual scenarios and
        // FakeVfs-backed tests depend on that.
        let attrs = block_on(StubPlatform::new().file_attrs(Path::new("/nope/missing.jpg")));
        assert_eq!(
            attrs.unwrap().type_description.as_deref(),
            Some("JPEG image")
        );
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
