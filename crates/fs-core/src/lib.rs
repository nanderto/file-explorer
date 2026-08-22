//! Filesystem logic for file-explorer, with **no gpui dependency** — this crate
//! builds and tests headless on any platform (see `docs/ARCHITECTURE.md` §6).
//!
//! M1 scope (read-only browsing): the executor seam ([`exec::Spawner`]), file
//! entries ([`entry::FileEntry`]), the [`vfs::Vfs`] trait with its real and fake
//! implementations, natural sorting ([`sort`]), directory listings + LRU cache
//! ([`listing`]), and the debounced watcher wrapper ([`watcher`]). File
//! operations, undo, clipboard, and the platform trait land in later milestones
//! and grow this crate additively.

pub mod entry;
pub mod exec;
pub mod listing;
pub mod sort;
pub mod vfs;
pub mod watcher;

pub use entry::{EntryId, EntryKind, EntryMeta, FileEntry, TargetKind};
pub use exec::{Spawner, SpawnerExt};
pub use listing::{ListingCache, ListingPatch, ListingSnapshot, list_dir, patch_listing};
pub use sort::{SortDirection, SortKey, SortSpec, natural_cmp};
pub use vfs::{RealVfs, Vfs, VolumeKey};
pub use watcher::{PathEvent, PathEventKind, WatchGuard};

#[cfg(any(test, feature = "test-support"))]
pub use exec::TestSpawner;
#[cfg(any(test, feature = "test-support"))]
pub use vfs::FakeVfs;
