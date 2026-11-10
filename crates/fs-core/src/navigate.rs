//! Turning what someone typed into the path field into somewhere to go.
//!
//! The address bar accepts whatever is pasted or typed into it, and a path
//! that is *nearly* right is far more common than one that is exactly right:
//! a path copied from a stale note, a folder that has since been renamed, a
//! file path when a folder was expected. Refusing those with "Path does not
//! exist" leaves the user to delete components by hand until it works, which
//! is the one thing they could already do.
//!
//! So resolution has three useful answers rather than one, and the UI decides
//! what each means:
//!
//! | Typed | [`PathTarget`] | What the pane does |
//! |---|---|---|
//! | an existing folder | `Folder` | navigate there |
//! | an existing file | `File` | open it in its default application |
//! | `…/gone/deeper` | `Nearest` | navigate to the deepest part that *does* exist |
//!
//! Walking up to the nearest existing ancestor is the behavior that makes a
//! pasted path always useful: you land as close to the intent as the
//! filesystem allows, and can see why the rest of it is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::vfs::Vfs;

/// Where a typed path resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathTarget {
    /// An existing directory (or a symlink to one) — navigate to it.
    Folder(PathBuf),
    /// An existing file — the pane opens it rather than navigating.
    File(PathBuf),
    /// Nothing at the typed path, but this ancestor of it exists: the
    /// "closest matching folder". Never equal to the typed path itself.
    Nearest(PathBuf),
    /// Nothing the user typed matched: no ancestor below the filesystem root
    /// exists, or there was nothing to resolve. The field reports this rather
    /// than moving the pane.
    Nothing,
}

/// Resolve `input` against the filesystem.
///
/// Empty or whitespace-only input is [`PathTarget::Nothing`]; so is a path
/// none of whose ancestors exist (a nonexistent volume, say). A relative path
/// is resolved as-is — the caller decides what it is relative *to*, and the
/// address bar only ever hands this absolute paths.
///
/// Every probe goes through [`Vfs::metadata`], so this never blocks the UI
/// thread and is testable against a `FakeVfs`.
pub async fn resolve_entered_path(vfs: Arc<dyn Vfs>, input: &str) -> PathTarget {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PathTarget::Nothing;
    }
    // A trailing separator means "the folder itself"; `Path` keeps it as a
    // component, which would make `/a/b/` miss `/a/b`.
    let typed = PathBuf::from(trimmed);
    let typed = trim_trailing_separator(&typed);

    if let Ok(Some(meta)) = vfs.metadata(&typed).await {
        return if meta.kind.is_dir_like() {
            PathTarget::Folder(typed)
        } else {
            PathTarget::File(typed)
        };
    }

    // Walk up. The first ancestor that exists *and is a directory* is where
    // the user can actually be put; an ancestor that is a file (someone typed
    // `/a/file.txt/nope`) is not somewhere to stand, so keep climbing.
    let mut cursor = typed.as_path();
    while let Some(parent) = cursor.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Ok(Some(meta)) = vfs.metadata(parent).await
            && meta.kind.is_dir_like()
        {
            // The bare root does not count as a match. Every absolute path
            // has it as an ancestor, so accepting it would turn any typo
            // into a silent trip to `/` — "closest matching folder" is only
            // useful when something the user actually typed matched.
            if parent.parent().is_none() {
                return PathTarget::Nothing;
            }
            return PathTarget::Nearest(parent.to_path_buf());
        }
        cursor = parent;
    }
    PathTarget::Nothing
}

/// `/a/b/` → `/a/b`, leaving a bare root alone.
fn trim_trailing_separator(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches(std::path::MAIN_SEPARATOR);
    if trimmed.is_empty() {
        path.to_path_buf()
    } else {
        PathBuf::from(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::TestSpawner;
    use crate::vfs::FakeVfs;
    use serde_json::json;

    fn tree() -> Arc<dyn Vfs> {
        let spawner = Arc::new(TestSpawner::new());
        let vfs = FakeVfs::new(spawner);
        // Rooted at `/` so the root itself exists — the walk-up rule below
        // depends on telling "the root exists" apart from "nothing matched".
        vfs.insert_tree(
            "/",
            json!({
                "home": { "Documents": { "report.pdf": "x", "Work": {} }, "notes.txt": "y" }
            }),
        );
        vfs
    }

    async fn resolve(input: &str) -> PathTarget {
        resolve_entered_path(tree(), input).await
    }

    #[test]
    fn an_existing_folder_is_a_folder() {
        assert_eq!(
            futures::executor::block_on(resolve("/home/Documents")),
            PathTarget::Folder(PathBuf::from("/home/Documents"))
        );
    }

    /// A trailing separator is how people paste folders; `/a/b/` must not
    /// miss `/a/b`.
    #[test]
    fn a_trailing_separator_still_finds_the_folder() {
        assert_eq!(
            futures::executor::block_on(resolve("/home/Documents/")),
            PathTarget::Folder(PathBuf::from("/home/Documents"))
        );
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(
            futures::executor::block_on(resolve("  /home/Documents  ")),
            PathTarget::Folder(PathBuf::from("/home/Documents"))
        );
    }

    #[test]
    fn an_existing_file_is_a_file() {
        assert_eq!(
            futures::executor::block_on(resolve("/home/Documents/report.pdf")),
            PathTarget::File(PathBuf::from("/home/Documents/report.pdf"))
        );
    }

    /// The point of the whole module: a path that is nearly right lands you
    /// as close to it as the filesystem allows.
    #[test]
    fn a_dead_tail_falls_back_to_the_deepest_existing_folder() {
        assert_eq!(
            futures::executor::block_on(resolve("/home/Documents/gone/deeper/still")),
            PathTarget::Nearest(PathBuf::from("/home/Documents"))
        );
    }

    #[test]
    fn one_missing_component_falls_back_to_its_parent() {
        assert_eq!(
            futures::executor::block_on(resolve("/home/Documents/gone")),
            PathTarget::Nearest(PathBuf::from("/home/Documents"))
        );
    }

    /// An ancestor that is a *file* is not somewhere to stand, so the walk
    /// keeps climbing past it.
    #[test]
    fn a_file_in_the_middle_of_the_path_is_climbed_past() {
        assert_eq!(
            futures::executor::block_on(resolve("/home/notes.txt/nope")),
            PathTarget::Nearest(PathBuf::from("/home")),
            "a file cannot be the folder you land in"
        );
    }

    #[test]
    fn empty_input_resolves_to_nothing() {
        assert_eq!(
            futures::executor::block_on(resolve("")),
            PathTarget::Nothing
        );
        assert_eq!(
            futures::executor::block_on(resolve("   ")),
            PathTarget::Nothing
        );
    }

    /// Every absolute path has `/` as an ancestor, so accepting the bare
    /// root as a "closest match" would turn any typo into a silent trip to
    /// the filesystem root. Nothing the user typed matched, so this is an
    /// error, not a destination.
    #[test]
    fn a_complete_typo_is_nothing_rather_than_a_trip_to_the_root() {
        assert_eq!(
            futures::executor::block_on(resolve("/nowhere/at/all")),
            PathTarget::Nothing
        );
        assert_eq!(
            futures::executor::block_on(resolve("/nowhere")),
            PathTarget::Nothing
        );
    }

    /// But the root is still reachable when it is what you actually asked for.
    #[test]
    fn the_root_itself_still_resolves() {
        assert_eq!(
            futures::executor::block_on(resolve("/")),
            PathTarget::Folder(PathBuf::from("/"))
        );
    }

    /// `Nearest` is only ever an ancestor — never the thing that was typed,
    /// which would make "went nowhere" indistinguishable from "found it".
    #[test]
    fn nearest_is_never_the_typed_path() {
        let target = futures::executor::block_on(resolve("/home/Documents/gone"));
        assert_ne!(
            target,
            PathTarget::Nearest(PathBuf::from("/home/Documents/gone"))
        );
    }
}
