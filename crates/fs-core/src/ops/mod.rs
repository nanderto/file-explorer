//! File operations (ARCHITECTURE.md §6, `ops/`): the [`FileOp`] vocabulary,
//! submit-time planning helpers — most importantly keep-both name resolution
//! ([`plan_keep_both_names`]), which runs at op-*planning* time so the planned
//! op already carries final destination names — and the job machinery
//! ([`job`], [`queue`]).

pub mod job;
pub mod queue;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::tags::Tag;
use crate::vfs::TrashId;

pub use job::{
    Conflict, ConflictChoice, JobEvent, JobId, JobInfo, JobKind, OpReceipt, PrevAttrs, Resolution,
};
pub use queue::JobQueue;

/// One user-level file operation, as submitted to the [`JobQueue`].
#[derive(Clone, Debug, PartialEq)]
pub enum FileOp {
    /// Copy `sources` (files or whole trees) into `dest_dir`.
    Copy {
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// Move `sources` into `dest_dir` (same-volume rename; cross-volume falls
    /// back to copy + remove). Moving into the source's own folder is a no-op.
    Move {
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// Move one path to another full path (inline rename and move-back undo).
    Rename { from: PathBuf, to: PathBuf },
    /// Move paths to the trash.
    TrashOp { paths: Vec<PathBuf> },
    /// Put trashed items back (undo of [`FileOp::TrashOp`]).
    Restore { ids: Vec<TrashId> },
    /// Create a new (not pre-existing) directory.
    CreateDir { path: PathBuf },
    /// Create a new (not pre-existing) empty file.
    CreateFile { path: PathBuf },
    /// Copy each source next to itself with a keep-both name.
    Duplicate { sources: Vec<PathBuf> },
    /// Permanent removal (shift-delete; undo of copy/create). Additive to
    /// §6's abbreviated op list — DeletePermanently is a §0 M3 row.
    Delete { paths: Vec<PathBuf> },
    /// Set the unix permission bits on each path (the info panel's permission
    /// checkboxes and octal field, M6b). `mode` is masked to `0o7777`; the
    /// file-type bits are not a mode value and `chmod` cannot change them.
    ///
    /// Applied per path and **partially tolerant**: a path that vanished or
    /// refused (EPERM) is recorded in [`OpReceipt::failed`] and the rest of the
    /// selection still goes through, so a mixed selection cannot be held
    /// hostage by one denied file.
    Chmod { paths: Vec<PathBuf>, mode: u32 },
    /// Change the owning user and/or group of each path, by name. `None` leaves
    /// that half alone. Needs privilege — an ordinary run fails with EPERM per
    /// path, which is reported, never panicked on.
    Chown {
        paths: Vec<PathBuf>,
        owner: Option<String>,
        group: Option<String>,
    },
    /// Replace the whole Finder tag set on each path (M6b). There is no
    /// add/remove: Finder rewrites the array too, and "add one" is a
    /// read-modify-write in the caller. An empty `tags` clears them.
    SetTags { paths: Vec<PathBuf>, tags: Vec<Tag> },
}

impl FileOp {
    pub fn kind(&self) -> JobKind {
        match self {
            FileOp::Copy { .. } => JobKind::Copy,
            FileOp::Move { .. } => JobKind::Move,
            FileOp::Rename { .. } => JobKind::Rename,
            FileOp::TrashOp { .. } => JobKind::Trash,
            FileOp::Restore { .. } => JobKind::Restore,
            FileOp::CreateDir { .. } => JobKind::CreateDir,
            FileOp::CreateFile { .. } => JobKind::CreateFile,
            FileOp::Duplicate { .. } => JobKind::Duplicate,
            FileOp::Delete { .. } => JobKind::Delete,
            FileOp::Chmod { .. } => JobKind::Chmod,
            FileOp::Chown { .. } => JobKind::Chown,
            FileOp::SetTags { .. } => JobKind::SetTags,
        }
    }

    /// The destination-side path that decides this op's serial lane
    /// (ARCHITECTURE.md §6: one lane per **destination** volume).
    pub(crate) fn lane_path(&self) -> &Path {
        let fallback = Path::new("/");
        match self {
            FileOp::Copy { dest_dir, .. } | FileOp::Move { dest_dir, .. } => dest_dir,
            FileOp::Rename { to, .. } => to,
            FileOp::TrashOp { paths }
            | FileOp::Delete { paths }
            | FileOp::Chmod { paths, .. }
            | FileOp::Chown { paths, .. }
            | FileOp::SetTags { paths, .. } => {
                paths.first().map(PathBuf::as_path).unwrap_or(fallback)
            }
            FileOp::Restore { ids } => ids
                .first()
                .map(|id| id.original.as_path())
                .unwrap_or(fallback),
            FileOp::CreateDir { path } | FileOp::CreateFile { path } => path,
            FileOp::Duplicate { sources } => {
                sources.first().and_then(|s| s.parent()).unwrap_or(fallback)
            }
        }
    }
}

/// Resolve keep-both destination names at planning time (ARCHITECTURE.md §6):
/// each source maps to `dest_dir/<name>` when free, otherwise to the first
/// free `"name copy.ext"` / `"name copy 2.ext"` candidate. `existing` is the
/// set of names already present in `dest_dir`; names planned earlier in the
/// same batch are taken too. Pure — directly unit-testable (the M3 acceptance
/// row for paste-into-same-folder naming).
pub fn plan_keep_both_names(
    sources: &[PathBuf],
    dest_dir: &Path,
    existing: &BTreeSet<String>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut taken = existing.clone();
    sources
        .iter()
        .map(|src| {
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let final_name = if taken.contains(&name) {
                keep_both_candidates(&name)
                    .find(|candidate| !taken.contains(candidate))
                    .expect("candidate sequence is unbounded")
            } else {
                name
            };
            taken.insert(final_name.clone());
            (src.clone(), dest_dir.join(final_name))
        })
        .collect()
}

/// The keep-both name sequence for `name`:
/// `"name copy.ext"`, `"name copy 2.ext"`, `"name copy 3.ext"`, …
pub fn keep_both_candidates(name: &str) -> impl Iterator<Item = String> + '_ {
    let (stem, ext) = split_name(name);
    (1u32..).map(move |i| {
        if i == 1 {
            format!("{stem} copy{ext}")
        } else {
            format!("{stem} copy {i}{ext}")
        }
    })
}

/// Split `"report.pdf"` into `("report", ".pdf")`. Dotfiles and extensionless
/// names keep the whole name as the stem: `(".env", "")`, `("folder", "")`.
/// `pub`: also the basis of the app's rename stem preselection
/// (ARCHITECTURE.md §4c) — one split, reused rather than re-implemented.
pub fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(ix) if ix > 0 => name.split_at(ix),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn planned_names(plan: &[(PathBuf, PathBuf)]) -> Vec<String> {
        plan.iter()
            .map(|(_, dest)| dest.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn keep_both_sequence_for_extension_names() {
        let names: Vec<String> = keep_both_candidates("photo.jpg").take(3).collect();
        assert_eq!(
            names,
            ["photo copy.jpg", "photo copy 2.jpg", "photo copy 3.jpg"]
        );
    }

    #[test]
    fn keep_both_sequence_for_extensionless_and_dotfile_names() {
        let names: Vec<String> = keep_both_candidates("folder").take(2).collect();
        assert_eq!(names, ["folder copy", "folder copy 2"]);
        let names: Vec<String> = keep_both_candidates(".env").take(2).collect();
        assert_eq!(names, [".env copy", ".env copy 2"]);
    }

    #[test]
    fn plan_passes_through_non_conflicting_names() {
        let plan = plan_keep_both_names(
            &[PathBuf::from("/src/a.txt"), PathBuf::from("/src/b.txt")],
            Path::new("/dest"),
            &existing(&["other.txt"]),
        );
        assert_eq!(
            plan,
            vec![
                (PathBuf::from("/src/a.txt"), PathBuf::from("/dest/a.txt")),
                (PathBuf::from("/src/b.txt"), PathBuf::from("/dest/b.txt")),
            ]
        );
    }

    #[test]
    fn paste_into_same_folder_produces_copy_names() {
        // The M3 acceptance row: pasting "photo.jpg" where it already lives.
        let plan = plan_keep_both_names(
            &[PathBuf::from("/pics/photo.jpg")],
            Path::new("/pics"),
            &existing(&["photo.jpg"]),
        );
        assert_eq!(planned_names(&plan), ["photo copy.jpg"]);

        // Pasting again once the copy exists escalates to "copy 2".
        let plan = plan_keep_both_names(
            &[PathBuf::from("/pics/photo.jpg")],
            Path::new("/pics"),
            &existing(&["photo.jpg", "photo copy.jpg"]),
        );
        assert_eq!(planned_names(&plan), ["photo copy 2.jpg"]);
    }

    #[test]
    fn plan_reserves_names_within_one_batch() {
        // Two same-named sources in one paste cannot collide with each other.
        let plan = plan_keep_both_names(
            &[PathBuf::from("/a/data.csv"), PathBuf::from("/b/data.csv")],
            Path::new("/dest"),
            &existing(&["data.csv"]),
        );
        assert_eq!(planned_names(&plan), ["data copy.csv", "data copy 2.csv"]);
    }

    #[test]
    fn lane_path_routes_by_destination() {
        let copy = FileOp::Copy {
            sources: vec![PathBuf::from("/a/x")],
            dest_dir: PathBuf::from("/Volumes/SSD/dest"),
        };
        assert_eq!(copy.lane_path(), Path::new("/Volumes/SSD/dest"));
        let rename = FileOp::Rename {
            from: PathBuf::from("/a/x"),
            to: PathBuf::from("/a/y"),
        };
        assert_eq!(rename.lane_path(), Path::new("/a/y"));
        assert_eq!(
            FileOp::Delete { paths: vec![] }.lane_path(),
            Path::new("/"),
            "empty ops fall back to the root lane"
        );
    }

    #[test]
    fn attribute_ops_route_and_label_themselves() {
        // The attribute ops have no destination: they act in place, so the lane
        // is the first path's volume — same as Trash and Delete.
        let paths = vec![
            PathBuf::from("/Volumes/SSD/a.txt"),
            PathBuf::from("/Volumes/SSD/b.txt"),
        ];
        for (op, kind) in [
            (
                FileOp::Chmod {
                    paths: paths.clone(),
                    mode: 0o644,
                },
                JobKind::Chmod,
            ),
            (
                FileOp::Chown {
                    paths: paths.clone(),
                    owner: Some("noel".into()),
                    group: None,
                },
                JobKind::Chown,
            ),
            (
                FileOp::SetTags {
                    paths: paths.clone(),
                    tags: vec![crate::tags::Tag::uncolored("Work")],
                },
                JobKind::SetTags,
            ),
        ] {
            assert_eq!(op.kind(), kind);
            assert_eq!(op.lane_path(), Path::new("/Volumes/SSD/a.txt"));
        }
        assert_eq!(
            FileOp::Chmod {
                paths: vec![],
                mode: 0o644
            }
            .lane_path(),
            Path::new("/")
        );
    }
}
