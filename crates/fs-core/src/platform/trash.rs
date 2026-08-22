//! The portable trash mechanism (ARCHITECTURE.md §6): a `.fake-trash`
//! directory holding restorable subtrees, used by [`crate::RealVfs`] on
//! non-macOS platforms (the "stub" scheme) so trash→restore and undo-of-delete
//! run as tests on Windows CI. Each entry records the original path and an
//! mtime fingerprint next to the moved payload. [`restore_blocking`] is shared
//! with the macOS path too — restoring is a rename back from wherever the
//! trashed payload lives (real macOS trash or `.fake-trash`).
//!
//! Everything here is blocking `std::fs`; callers run it through
//! [`crate::SpawnerExt::unblock`].

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Result, anyhow};

use crate::vfs::{TrashId, TrashRestoreError};

/// Name of the portable trash directory, created in the trashed item's parent.
pub const FAKE_TRASH_DIR: &str = ".fake-trash";

/// Per-entry sidecar recording the original path + mtime fingerprint
/// (ARCHITECTURE.md §6's "entry = original path + mtime fingerprint + moved
/// payload"); the live restore data travels in [`TrashId`].
const META_FILE: &str = "meta.txt";

/// Move `path` into `<parent>/.fake-trash/<n>-<name>/<name>` and return the
/// undo token.
// On macOS builds `RealVfs::trash` uses the real NSFileManager trash instead,
// leaving this reachable only from this module's tests there — allow the
// lib-target dead_code lint rather than cfg-ing the tests off macOS.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn fake_trash_blocking(path: &Path) -> Result<TrashId> {
    let metadata = std::fs::symlink_metadata(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("cannot trash {}", path.display()))?
        .to_os_string();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("cannot trash a root: {}", path.display()))?;
    let root = parent.join(FAKE_TRASH_DIR);
    std::fs::create_dir_all(&root)?;

    // Claim a unique entry directory (create_dir is the atomicity point).
    let mut entry_dir = None;
    for n in 1u32.. {
        let candidate = root.join(format!("{n}-{}", name.to_string_lossy()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                entry_dir = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let entry_dir = entry_dir.expect("loop breaks only with an entry dir");

    let trashed = entry_dir.join(&name);
    std::fs::rename(path, &trashed)?;
    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Best-effort sidecar — the token itself carries the restore data.
    let _ = std::fs::write(
        entry_dir.join(META_FILE),
        format!("{}\n{mtime_secs}\n", path.display()),
    );
    Ok(TrashId {
        original: path.to_path_buf(),
        trashed,
    })
}

/// Rename the trashed payload back to its original path. Shared by the
/// `.fake-trash` scheme and the macOS real-trash path (both are same-volume
/// renames). `AlreadyRestored` is detected by the caller's consumed-token set
/// before this runs.
pub(crate) fn restore_blocking(id: &TrashId) -> Result<PathBuf, TrashRestoreError> {
    if std::fs::symlink_metadata(&id.trashed).is_err() {
        return Err(TrashRestoreError::NotFound);
    }
    if std::fs::symlink_metadata(&id.original).is_ok() {
        return Err(TrashRestoreError::Collision(id.original.clone()));
    }
    if let Some(parent) = id.original.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    // The rename can only fail here on a race (payload vanished between the
    // check and the move) or an OS-level refusal; both surface as the closest
    // typed variant.
    std::fs::rename(&id.trashed, &id.original).map_err(|_| TrashRestoreError::NotFound)?;
    cleanup_entry_dir(&id.trashed);
    Ok(id.original.clone())
}

/// Remove the (now payload-less) `.fake-trash` entry dir and, when it was the
/// last entry, the `.fake-trash` root itself. Best-effort; no-op when the
/// payload was not under a `.fake-trash` entry (macOS real trash).
fn cleanup_entry_dir(trashed: &Path) {
    let Some(entry_dir) = trashed.parent() else {
        return;
    };
    let Some(root) = entry_dir.parent() else {
        return;
    };
    if root.file_name().and_then(|n| n.to_str()) != Some(FAKE_TRASH_DIR) {
        return;
    }
    let _ = std::fs::remove_file(entry_dir.join(META_FILE));
    let _ = std::fs::remove_dir(entry_dir);
    let _ = std::fs::remove_dir(root); // fails (kept) while other entries remain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_then_restore_round_trips_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("doc.txt");
        std::fs::write(&target, b"precious").unwrap();

        let id = fake_trash_blocking(&target).unwrap();
        assert!(!target.exists(), "trashed item left its original location");
        assert!(id.trashed.starts_with(dir.path().join(FAKE_TRASH_DIR)));
        assert_eq!(std::fs::read(&id.trashed).unwrap(), b"precious");
        assert!(
            id.trashed.parent().unwrap().join(META_FILE).exists(),
            "entry records its sidecar meta"
        );

        let restored = restore_blocking(&id).unwrap();
        assert_eq!(restored, target);
        assert_eq!(std::fs::read(&target).unwrap(), b"precious");
        assert!(
            !dir.path().join(FAKE_TRASH_DIR).exists(),
            "last restore cleans the .fake-trash root"
        );
    }

    #[test]
    fn trash_holds_whole_subtrees_and_same_names_get_distinct_entries() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("project");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), b"a").unwrap();

        let id_dir = fake_trash_blocking(&sub).unwrap();
        assert_eq!(std::fs::read(id_dir.trashed.join("a.txt")).unwrap(), b"a");

        // A new item with the same name trashes into a distinct entry.
        std::fs::create_dir(&sub).unwrap();
        let id_again = fake_trash_blocking(&sub).unwrap();
        assert_ne!(id_dir.trashed, id_again.trashed);

        restore_blocking(&id_dir).unwrap();
        assert!(sub.join("a.txt").exists());
        // Second restore of the same original path collides.
        assert_eq!(
            restore_blocking(&id_again).unwrap_err(),
            TrashRestoreError::Collision(sub.clone())
        );
    }

    #[test]
    fn restore_not_found_when_trash_emptied_externally() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gone.txt");
        std::fs::write(&target, b"x").unwrap();
        let id = fake_trash_blocking(&target).unwrap();
        std::fs::remove_file(&id.trashed).unwrap(); // "empty trash" externally
        assert_eq!(
            restore_blocking(&id).unwrap_err(),
            TrashRestoreError::NotFound
        );
    }
}
