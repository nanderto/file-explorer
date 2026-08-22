//! File entries and their identity (ARCHITECTURE.md §6, `entry.rs`).

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

/// Identity key everywhere: selection, undo, drag payloads, `NavEntry.cursor`.
///
/// Path-keyed (never index-keyed) so identity survives watcher patches,
/// re-sorts, and in-place folder expansion.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub Arc<Path>);

/// What a symlink points at (resolved once at listing time).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetKind {
    File,
    Dir,
    /// Broken or unresolvable symlink.
    Unknown,
}

/// The kind of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
    Symlink { target_kind: TargetKind },
}

impl EntryKind {
    /// True for directories and symlinks that resolve to directories —
    /// the set treated as "folders" by folders-first sorting and navigation.
    pub fn is_dir_like(&self) -> bool {
        matches!(
            self,
            EntryKind::Dir
                | EntryKind::Symlink {
                    target_kind: TargetKind::Dir
                }
        )
    }
}

/// One row of a directory listing.
///
/// M1 carries the browsing fields; later milestones add fields additively
/// (e.g. lazy permissions for the info panel at M5, tags at M6).
#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub path: Arc<Path>,
    /// Cached `file_name` for sort/render.
    pub name: Arc<str>,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: SystemTime,
    pub created: Option<SystemTime>,
    /// Dotfile (M1); the Finder hidden flag joins via the platform trait later.
    pub hidden: bool,
}

impl FileEntry {
    /// This entry's stable, path-keyed identity.
    pub fn id(&self) -> EntryId {
        EntryId(self.path.clone())
    }

    /// See [`EntryKind::is_dir_like`].
    pub fn is_dir_like(&self) -> bool {
        self.kind.is_dir_like()
    }
}

/// Stat result for a single path (`Vfs::metadata`) — the entry's attributes
/// without its location.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryMeta {
    pub kind: EntryKind,
    pub size: u64,
    pub modified: SystemTime,
    pub created: Option<SystemTime>,
    pub hidden: bool,
}

impl EntryMeta {
    /// Combine stat data with a location into a listing row.
    pub fn into_entry(self, path: Arc<Path>) -> FileEntry {
        let name: Arc<str> = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        FileEntry {
            path,
            name,
            kind: self.kind,
            size: self.size,
            modified: self.modified,
            created: self.created,
            hidden: self.hidden,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn dir_like_covers_dirs_and_dir_symlinks() {
        assert!(EntryKind::Dir.is_dir_like());
        assert!(
            EntryKind::Symlink {
                target_kind: TargetKind::Dir
            }
            .is_dir_like()
        );
        assert!(!EntryKind::File.is_dir_like());
        assert!(
            !EntryKind::Symlink {
                target_kind: TargetKind::File
            }
            .is_dir_like()
        );
        assert!(
            !EntryKind::Symlink {
                target_kind: TargetKind::Unknown
            }
            .is_dir_like()
        );
    }

    #[test]
    fn entry_meta_into_entry_derives_name_from_path() {
        let meta = EntryMeta {
            kind: EntryKind::File,
            size: 7,
            modified: SystemTime::UNIX_EPOCH,
            created: None,
            hidden: false,
        };
        let entry = meta.into_entry(Arc::from(PathBuf::from("/tmp/report.pdf")));
        assert_eq!(&*entry.name, "report.pdf");
        assert_eq!(entry.size, 7);
        assert_eq!(
            entry.id(),
            EntryId(Arc::from(PathBuf::from("/tmp/report.pdf")))
        );
    }
}
