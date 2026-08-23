//! Filesystem logic for file-explorer, with **no gpui dependency** — this crate
//! builds and tests headless on any platform (see `docs/ARCHITECTURE.md` §6).
//!
//! M1 scope (read-only browsing): the executor seam ([`exec::Spawner`]), file
//! entries ([`entry::FileEntry`]), the [`vfs::Vfs`] trait with its real and fake
//! implementations, natural sorting ([`sort`]), directory listings + LRU cache
//! ([`listing`]), and the debounced watcher wrapper ([`watcher`]).
//!
//! M2 adds the OS-services seam ([`platform::Platform`]: volumes + eject, with
//! a macOS implementation and a portable deterministic stub) and the
//! persistence primitives `Vfs::load` / `Vfs::atomic_write`.
//!
//! M3 grows the mutation surface: the [`vfs::Vfs`] trait's
//! `create_dir`/`create_file`/`copy`/`rename`/`remove`/`trash`/`restore`
//! methods (trash as a `.fake-trash` directory everywhere but macOS, so the
//! whole flow tests on Windows), file operations with planning and the
//! destination-volume job queue ([`ops`]), inverse-op undo with fingerprint
//! invalidation ([`undo`]), and the cut/copy clipboard ([`clipboard`]).

pub mod clipboard;
pub mod entry;
pub mod exec;
pub mod listing;
pub mod ops;
pub mod platform;
pub mod sort;
pub mod thumbnail;
pub mod undo;
pub mod vfs;
pub mod watcher;

pub use clipboard::{ClipboardMode, FileClipboard};
pub use entry::{EntryId, EntryKind, EntryMeta, FileEntry, TargetKind};
pub use exec::{Spawner, SpawnerExt};
pub use listing::{
    ListingCache, ListingPatch, ListingSnapshot, ResolvedBatch, list_dir, patch_listing,
    resolve_watch_batch,
};
pub use ops::{
    Conflict, ConflictChoice, FileOp, JobEvent, JobId, JobInfo, JobKind, JobQueue, OpReceipt,
    Resolution, keep_both_candidates, plan_keep_both_names, split_name,
};
#[cfg(target_os = "macos")]
pub use platform::MacPlatform;
pub use platform::{Platform, StubPlatform, VolumeId, VolumeInfo, watch_volumes};
pub use sort::{SortDirection, SortKey, SortSpec, natural_cmp};
pub use thumbnail::{
    ContentStamp, MAX_PX, Thumbnail, ThumbnailCache, ThumbnailKey,
    validate_px as validate_thumbnail_px,
};
pub use undo::{Fingerprint, UndoEntry, UndoOutcome, UndoStack};
pub use vfs::{
    CopyCancelled, CreateOptions, ProgressFn, RealVfs, RemoveOptions, RenameOptions, TrashId,
    TrashRestoreError, Vfs, VolumeKey,
};
pub use watcher::{PathEvent, PathEventKind, WatchGuard};

#[cfg(any(test, feature = "test-support"))]
pub use exec::TestSpawner;
#[cfg(any(test, feature = "test-support"))]
pub use vfs::FakeVfs;
