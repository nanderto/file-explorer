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
//!
//! M6a adds [`search`]: the pure per-keystroke name filter over a loaded
//! listing, and the breadth-first streamed recursive walk under a folder.
//!
//! M5 backs the info panel with [`attrs`]: unix permissions, the
//! multi-selection summary, the previewable-type gate, and
//! [`platform::Platform::file_attrs`] for the attributes that need an OS call.

pub mod attrs;
pub mod clipboard;
pub mod entry;
pub mod exec;
pub mod listing;
pub mod ops;
pub mod platform;
pub mod search;
pub mod sort;
pub mod thumbnail;
pub mod undo;
pub mod vfs;
pub mod watcher;

pub use attrs::{
    FileAttrs, PREVIEW_SIZE_CEILING, PermBit, PermClass, SelectionSummary, UnixPerms,
    is_previewable, is_previewable_entry, summarize,
};
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
pub use search::{
    CYCLE_REPEATS, MAX_CONCURRENT_DIR_READS, MAX_CYCLE_PERIOD, MAX_DEPTH, PROGRESS_EVERY_DIRS,
    SearchEvent, SearchQuery, filter_snapshot, looks_like_a_directory_cycle, search_recursive,
};
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
