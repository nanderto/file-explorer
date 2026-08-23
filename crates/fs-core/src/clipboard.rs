//! The file clipboard (ARCHITECTURE.md §6, `clipboard.rs`) — a plain struct
//! held by the app's `FsContext`. Cut entries render dimmed (the DirView
//! checks membership at render); paste turns the clipboard into a
//! [`crate::ops::FileOp`] (`Copy` for copy-mode, `Move` for cut-mode), and a
//! cut clipboard empties after its paste.

use std::path::{Path, PathBuf};

use crate::entry::EntryId;
use crate::ops::FileOp;

/// Whether a paste copies or moves the clipboard's entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipboardMode {
    #[default]
    Copy,
    Cut,
}

/// Path-keyed clipboard contents (`EntryId` per invariant #2 — never indices).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileClipboard {
    pub entries: Vec<EntryId>,
    pub mode: ClipboardMode,
}

impl FileClipboard {
    /// Replace the clipboard (Cut/Copy actions).
    pub fn set(&mut self, entries: Vec<EntryId>, mode: ClipboardMode) {
        self.entries = entries;
        self.mode = mode;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.mode = ClipboardMode::Copy;
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when `path` is cut-pending — the render-dimming check.
    pub fn is_cut(&self, path: &Path) -> bool {
        self.mode == ClipboardMode::Cut && self.entries.iter().any(|id| &*id.0 == path)
    }

    /// The clipboard's paths as owned buffers (op submission input).
    pub fn paths(&self) -> Vec<PathBuf> {
        self.entries.iter().map(|id| id.0.to_path_buf()).collect()
    }

    /// Consume the clipboard for a paste: returns the paths and mode. A cut
    /// clipboard empties (its entries are gone from their old location); a
    /// copy clipboard stays for repeated pastes. `None` when empty.
    pub fn take_for_paste(&mut self) -> Option<(Vec<PathBuf>, ClipboardMode)> {
        if self.is_empty() {
            return None;
        }
        let mode = self.mode;
        let paths = self.paths();
        if mode == ClipboardMode::Cut {
            self.clear();
        }
        Some((paths, mode))
    }

    /// Whether `dest_dir` is inside — or *is* — one of the clipboard's
    /// entries. Pasting there would be a self-move or a self-copy: the same
    /// rule drag & drop applies before arming a drop target, kept here so
    /// every paste path (`cmd-v`, the row menu's `Paste { dest }` on a folder,
    /// the background menu) obeys it identically.
    pub fn contains_destination(&self, dest_dir: &Path) -> bool {
        self.entries.iter().any(|id| dest_dir.starts_with(&*id.0))
    }

    /// Turn a paste into the [`FileOp`] the job queue runs (§4b): copy-mode
    /// pastes as `Copy`, cut-mode as `Move` (consuming the clipboard via
    /// [`FileClipboard::take_for_paste`]). Planning proper happens in ops —
    /// submitting the returned op resolves paste-into-same-folder keep-both
    /// names at planning time. `None` when the clipboard is empty.
    ///
    /// Also `None` — **without consuming the clipboard** — when the
    /// destination is inside or equal to a source
    /// ([`FileClipboard::contains_destination`]). Consuming it would be the
    /// worst outcome available: a `Move` of a folder into itself fails at
    /// execution (`rename(2)` gives `EINVAL`), so the user got a failure toast
    /// *and* silently lost the cut they now have to redo, while the `Copy`
    /// variant instead succeeded into a nested self-copy that Explorer refuses
    /// outright.
    pub fn paste_op(&mut self, dest_dir: &Path) -> Option<FileOp> {
        if self.contains_destination(dest_dir) {
            return None;
        }
        let (sources, mode) = self.take_for_paste()?;
        let dest_dir = dest_dir.to_path_buf();
        Some(match mode {
            ClipboardMode::Copy => FileOp::Copy { sources, dest_dir },
            ClipboardMode::Cut => FileOp::Move { sources, dest_dir },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn id(path: &str) -> EntryId {
        EntryId(Arc::from(Path::new(path)))
    }

    #[test]
    fn pasting_into_a_source_is_refused_without_consuming_the_clipboard() {
        // Cut a folder, right-click that same folder, Paste. The op would be
        // `Move { sources: [/root/target], dest_dir: /root/target }`, which
        // `rename(2)` rejects — so the user would have got a failure toast and
        // silently lost the cut. The clipboard must survive intact.
        let mut clipboard = FileClipboard::default();
        clipboard.set(vec![id("/root/target")], ClipboardMode::Cut);
        assert!(clipboard.contains_destination(Path::new("/root/target")));
        assert_eq!(clipboard.paste_op(Path::new("/root/target")), None);
        assert!(
            !clipboard.is_empty(),
            "a refused paste must not consume the cut"
        );
        assert!(clipboard.is_cut(Path::new("/root/target")));

        // ...and *into* a source is refused too, at any depth.
        assert!(clipboard.contains_destination(Path::new("/root/target/deep/er")));
        assert_eq!(clipboard.paste_op(Path::new("/root/target/deep/er")), None);
        assert!(!clipboard.is_empty());

        // A copy clipboard would otherwise have succeeded into a nested
        // self-copy at /root/target/target, which Explorer refuses outright.
        let mut clipboard = FileClipboard::default();
        clipboard.set(vec![id("/root/target")], ClipboardMode::Copy);
        assert_eq!(clipboard.paste_op(Path::new("/root/target")), None);
        assert!(!clipboard.is_empty(), "a copy clipboard is never consumed");

        // A sibling destination is still perfectly legal.
        assert!(!clipboard.contains_destination(Path::new("/root/elsewhere")));
        assert!(clipboard.paste_op(Path::new("/root/elsewhere")).is_some());
        // ...and so is a path that merely shares a name prefix.
        assert!(!clipboard.contains_destination(Path::new("/root/target2")));
    }

    #[test]
    fn cut_dims_only_cut_entries_and_paste_consumes_them() {
        let mut clipboard = FileClipboard::default();
        assert!(clipboard.is_empty());
        assert!(clipboard.take_for_paste().is_none());

        clipboard.set(vec![id("/d/a.txt"), id("/d/b.txt")], ClipboardMode::Cut);
        assert!(clipboard.is_cut(Path::new("/d/a.txt")));
        assert!(!clipboard.is_cut(Path::new("/d/c.txt")));

        let (paths, mode) = clipboard.take_for_paste().unwrap();
        assert_eq!(mode, ClipboardMode::Cut);
        assert_eq!(
            paths,
            vec![PathBuf::from("/d/a.txt"), PathBuf::from("/d/b.txt")]
        );
        assert!(clipboard.is_empty(), "cut clipboard empties after paste");
    }

    #[test]
    fn copy_mode_never_dims_and_survives_repeated_pastes() {
        let mut clipboard = FileClipboard::default();
        clipboard.set(vec![id("/d/a.txt")], ClipboardMode::Copy);
        assert!(!clipboard.is_cut(Path::new("/d/a.txt")));

        let first = clipboard.take_for_paste().unwrap();
        let second = clipboard.take_for_paste().unwrap();
        assert_eq!(first, second, "copy clipboard pastes repeatedly");
        assert!(!clipboard.is_empty());

        clipboard.clear();
        assert!(clipboard.is_empty());
        assert_eq!(clipboard.mode, ClipboardMode::Copy);
    }

    #[test]
    fn paste_op_hands_cut_off_as_a_move_and_consumes_the_clipboard() {
        let mut clipboard = FileClipboard::default();
        assert!(clipboard.paste_op(Path::new("/dest")).is_none());

        clipboard.set(vec![id("/d/a.txt")], ClipboardMode::Cut);
        assert_eq!(
            clipboard.paste_op(Path::new("/dest")),
            Some(FileOp::Move {
                sources: vec![PathBuf::from("/d/a.txt")],
                dest_dir: PathBuf::from("/dest"),
            })
        );
        assert!(clipboard.is_empty(), "cut is consumed on paste");
        assert!(clipboard.paste_op(Path::new("/dest")).is_none());

        clipboard.set(vec![id("/d/a.txt")], ClipboardMode::Copy);
        assert_eq!(
            clipboard.paste_op(Path::new("/dest")),
            Some(FileOp::Copy {
                sources: vec![PathBuf::from("/d/a.txt")],
                dest_dir: PathBuf::from("/dest"),
            })
        );
        assert!(!clipboard.is_empty(), "copy survives for repeated pastes");
    }

    #[test]
    fn same_folder_paste_plans_keep_both_names_through_ops() {
        use crate::exec::{Spawner, TestSpawner};
        use crate::ops::{JobEvent, JobQueue};
        use crate::vfs::{FakeVfs, Vfs};
        use futures::executor::block_on;
        use serde_json::json;

        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = FakeVfs::new(spawner.clone());
        let queue = JobQueue::new(vfs.clone() as Arc<dyn Vfs>, spawner);
        let events = queue.subscribe();
        vfs.insert_tree("/pics", json!({ "photo.jpg": "img" }));

        // Copy photo.jpg, paste it into its own folder: the handed-off op is
        // planned by ops (keep-both resolved at planning time — no conflict
        // prompt), landing as "photo copy.jpg".
        let mut clipboard = FileClipboard::default();
        clipboard.set(vec![id("/pics/photo.jpg")], ClipboardMode::Copy);
        let op = clipboard.paste_op(Path::new("/pics")).unwrap();
        let job = queue.submit(op);
        loop {
            match block_on(events.recv()).expect("event stream open") {
                JobEvent::Completed { id: i, receipt } if i == job => {
                    assert_eq!(receipt.created, vec![PathBuf::from("/pics/photo copy.jpg")]);
                    break;
                }
                JobEvent::NeedsDecision { .. } => {
                    panic!("same-folder paste never prompts: names were planned")
                }
                JobEvent::Failed { id: i, error } if i == job => panic!("paste failed: {error}"),
                _ => {}
            }
        }
        assert_eq!(
            block_on(vfs.load(Path::new("/pics/photo copy.jpg"))).unwrap(),
            b"img"
        );
    }
}
