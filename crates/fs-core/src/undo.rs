//! Undo/redo of file operations (ARCHITECTURE.md §6, `undo.rs`).
//!
//! An [`UndoEntry`] is the *inverse* op recorded from an [`OpReceipt`] at job
//! completion: rename→rename-back, move→move-back, copy→remove-created,
//! new-folder→remove, trash→restore. Applying an entry re-submits through the
//! [`JobQueue`], so undo gets progress/conflicts for free. Each entry stores
//! `(path, mtime)` fingerprints; if the world changed underneath, the entry is
//! **skipped** with a first-class, testable outcome
//! ([`UndoOutcome::Invalidated`]) rather than destroying data.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use crate::ops::job::{JobId, OpReceipt};
use crate::ops::{FileOp, JobQueue};
use crate::vfs::Vfs;

/// Expected `(path, mtime)` — undo validation input.
#[derive(Clone, Debug, PartialEq)]
pub struct Fingerprint {
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// One undoable step: the inverse ops to submit, the original ops for redo,
/// and the fingerprints that must still hold for the inverse to be safe.
#[derive(Clone, Debug, PartialEq)]
pub struct UndoEntry {
    /// Ops that undo the recorded operation (submitted in order; same-volume
    /// ops serialize on their lane).
    pub inverse: Vec<FileOp>,
    /// Ops that re-apply the recorded operation.
    pub redo: Vec<FileOp>,
    /// Guards: applying `inverse` is only safe while these still hold.
    pub fingerprints: Vec<Fingerprint>,
}

impl UndoEntry {
    /// Build the inverse of a completed job from its receipt, fingerprinting
    /// the paths the inverse will act on. `None` when the op has no inverse
    /// (permanent delete, or a job that ended up doing nothing).
    pub async fn from_receipt(vfs: &Arc<dyn Vfs>, receipt: &OpReceipt) -> Option<UndoEntry> {
        let mut inverse = Vec::new();
        let mut fingerprints = Vec::new();

        // Moves/renames come back in reverse order (a rename after a move
        // must be unwound before it).
        for (from, to) in receipt.moved.iter().rev() {
            inverse.push(FileOp::Rename {
                from: to.clone(),
                to: from.clone(),
            });
            if let Ok(Some(meta)) = vfs.metadata(to).await {
                fingerprints.push(Fingerprint {
                    path: to.clone(),
                    modified: meta.modified,
                });
            }
        }
        if !receipt.created.is_empty() {
            inverse.push(FileOp::Delete {
                paths: receipt.created.clone(),
            });
            for path in &receipt.created {
                if let Ok(Some(meta)) = vfs.metadata(path).await {
                    fingerprints.push(Fingerprint {
                        path: path.clone(),
                        modified: meta.modified,
                    });
                }
            }
        }
        if !receipt.trashed.is_empty() {
            // Restore races are guarded by TrashRestoreError, not fingerprints.
            inverse.push(FileOp::Restore {
                ids: receipt.trashed.clone(),
            });
        }
        if !receipt.restored.is_empty() {
            inverse.push(FileOp::TrashOp {
                paths: receipt
                    .restored
                    .iter()
                    .map(|(_, path)| path.clone())
                    .collect(),
            });
        }
        if inverse.is_empty() {
            return None;
        }
        Some(UndoEntry {
            inverse,
            redo: vec![receipt.op.clone()],
            fingerprints,
        })
    }

    /// The entry that re-applies what this entry undoes. Fingerprints are
    /// remapped through the inverse's rename pairs (renames preserve mtimes);
    /// non-remappable fingerprints are dropped — the op vocabulary's own
    /// guards (conflict dialog, typed restore errors) still apply on redo.
    fn inverted(&self) -> UndoEntry {
        let fingerprints = self
            .fingerprints
            .iter()
            .filter_map(|fingerprint| {
                self.inverse.iter().find_map(|op| match op {
                    FileOp::Rename { from, to } if *from == fingerprint.path => Some(Fingerprint {
                        path: to.clone(),
                        modified: fingerprint.modified,
                    }),
                    _ => None,
                })
            })
            .collect();
        UndoEntry {
            inverse: self.redo.clone(),
            redo: self.inverse.clone(),
            fingerprints,
        }
    }
}

/// What one undo/redo attempt did.
#[derive(Debug)]
pub enum UndoOutcome {
    /// The entry's ops were submitted to the queue.
    Applied { jobs: Vec<JobId> },
    /// The world changed underneath the entry: it was skipped (and handed
    /// back for the UI's "Can't undo — '…' was modified since" toast), never
    /// applied against stale state.
    Invalidated { entry: UndoEntry, reason: String },
    /// The stack was empty.
    Nothing,
}

/// Undo/redo stacks of inverse operations.
#[derive(Default)]
pub struct UndoStack {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a completed operation (called with each job receipt's entry).
    /// New work truncates the redo stack, like every editor.
    pub fn push(&mut self, entry: UndoEntry) {
        self.undo.push(entry);
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Undo the most recent entry: validate its fingerprints, then submit its
    /// inverse ops through the queue.
    pub async fn undo(&mut self, vfs: &Arc<dyn Vfs>, queue: &Arc<JobQueue>) -> UndoOutcome {
        let Some(entry) = self.undo.pop() else {
            return UndoOutcome::Nothing;
        };
        if let Some(reason) = validate(vfs, &entry.fingerprints).await {
            return UndoOutcome::Invalidated { entry, reason };
        }
        let jobs = entry
            .inverse
            .iter()
            .map(|op| queue.submit(op.clone()))
            .collect();
        self.redo.push(entry.inverted());
        UndoOutcome::Applied { jobs }
    }

    /// Re-apply the most recently undone entry.
    pub async fn redo(&mut self, vfs: &Arc<dyn Vfs>, queue: &Arc<JobQueue>) -> UndoOutcome {
        let Some(entry) = self.redo.pop() else {
            return UndoOutcome::Nothing;
        };
        if let Some(reason) = validate(vfs, &entry.fingerprints).await {
            return UndoOutcome::Invalidated { entry, reason };
        }
        let jobs = entry
            .inverse
            .iter()
            .map(|op| queue.submit(op.clone()))
            .collect();
        self.undo.push(entry.inverted());
        UndoOutcome::Applied { jobs }
    }
}

/// `None` when every fingerprint still holds; otherwise why the entry is
/// stale (drives the invalidation toast).
async fn validate(vfs: &Arc<dyn Vfs>, fingerprints: &[Fingerprint]) -> Option<String> {
    for fingerprint in fingerprints {
        let name = fingerprint
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| fingerprint.path.display().to_string());
        match vfs.metadata(&fingerprint.path).await {
            Ok(Some(meta)) if meta.modified == fingerprint.modified => {}
            Ok(Some(_)) => return Some(format!("'{name}' was modified since")),
            Ok(None) => return Some(format!("'{name}' no longer exists")),
            Err(error) => return Some(format!("'{name}': {error}")),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Spawner, TestSpawner};
    use crate::ops::job::JobEvent;
    use crate::vfs::FakeVfs;
    use futures::executor::block_on;
    use serde_json::json;
    use std::path::Path;

    struct Fixture {
        vfs: Arc<FakeVfs>,
        vfs_dyn: Arc<dyn Vfs>,
        queue: Arc<JobQueue>,
        events: async_channel::Receiver<JobEvent>,
    }

    fn setup() -> Fixture {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = FakeVfs::new(spawner.clone());
        let vfs_dyn: Arc<dyn Vfs> = vfs.clone();
        let queue = JobQueue::new(vfs_dyn.clone(), spawner);
        let events = queue.subscribe();
        Fixture {
            vfs,
            vfs_dyn,
            queue,
            events,
        }
    }

    impl Fixture {
        /// Submit an op and wait for its receipt.
        fn run(&self, op: FileOp) -> OpReceipt {
            let id = self.queue.submit(op);
            loop {
                match block_on(self.events.recv()).expect("event stream open") {
                    JobEvent::Completed { id: i, receipt } if i == id => return receipt,
                    JobEvent::Failed { id: i, error } if i == id => {
                        panic!("job failed: {error}")
                    }
                    JobEvent::Cancelled { id: i } if i == id => panic!("job cancelled"),
                    _ => {}
                }
            }
        }

        /// Wait for every job in `jobs` to finish (undo submissions).
        fn wait_jobs(&self, jobs: &[JobId]) {
            let mut pending: Vec<JobId> = jobs.to_vec();
            while !pending.is_empty() {
                match block_on(self.events.recv()).expect("event stream open") {
                    JobEvent::Completed { id, .. } | JobEvent::Cancelled { id } => {
                        pending.retain(|j| *j != id)
                    }
                    JobEvent::Failed { id, error } if pending.contains(&id) => {
                        panic!("undo job failed: {error}")
                    }
                    _ => {}
                }
            }
        }

        fn entry_for(&self, receipt: &OpReceipt) -> UndoEntry {
            block_on(UndoEntry::from_receipt(&self.vfs_dyn, receipt)).expect("undoable receipt")
        }

        fn apply_undo(&self, stack: &mut UndoStack) -> UndoOutcome {
            let outcome = block_on(stack.undo(&self.vfs_dyn, &self.queue));
            if let UndoOutcome::Applied { jobs } = &outcome {
                self.wait_jobs(jobs);
            }
            outcome
        }

        fn apply_redo(&self, stack: &mut UndoStack) -> UndoOutcome {
            let outcome = block_on(stack.redo(&self.vfs_dyn, &self.queue));
            if let UndoOutcome::Applied { jobs } = &outcome {
                self.wait_jobs(jobs);
            }
            outcome
        }
    }

    #[test]
    fn undo_of_a_move_restores_the_tree_exactly_and_redo_reapplies() {
        let fx = setup();
        fx.vfs
            .insert_tree("/root", json!({ "a": { "f.txt": "f" }, "b": {} }));
        let before = fx.vfs.snapshot();

        let receipt = fx.run(FileOp::Move {
            sources: vec![PathBuf::from("/root/a/f.txt")],
            dest_dir: PathBuf::from("/root/b"),
        });
        let after_move = fx.vfs.snapshot();
        assert_ne!(before, after_move);

        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));
        assert!(stack.can_undo());

        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(fx.vfs.snapshot(), before, "undo restores the exact tree");
        assert!(stack.can_redo());

        assert!(matches!(
            fx.apply_redo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(fx.vfs.snapshot(), after_move, "redo re-applies the move");
        assert!(stack.can_undo(), "redo made the entry undoable again");
    }

    #[test]
    fn undo_of_a_copy_removes_only_what_the_copy_created() {
        let fx = setup();
        fx.vfs.insert_tree(
            "/root",
            json!({ "src": { "tree": { "x.txt": "x" } }, "dest": { "keep.txt": "k" } }),
        );
        let before = fx.vfs.snapshot();

        let receipt = fx.run(FileOp::Copy {
            sources: vec![PathBuf::from("/root/src/tree")],
            dest_dir: PathBuf::from("/root/dest"),
        });
        assert_eq!(receipt.created, vec![PathBuf::from("/root/dest/tree")]);

        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));
        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(
            fx.vfs.snapshot(),
            before,
            "undo removed the copied tree and nothing else"
        );
    }

    #[test]
    fn undo_of_a_trash_restores_from_the_fake_trash() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({ "doc.txt": "body" }));
        let before = fx.vfs.snapshot();

        let receipt = fx.run(FileOp::TrashOp {
            paths: vec![PathBuf::from("/root/doc.txt")],
        });
        assert!(
            block_on(fx.vfs_dyn.metadata(Path::new("/root/doc.txt")))
                .unwrap()
                .is_none()
        );

        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));
        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(fx.vfs.snapshot(), before, "trash undo is a restore");
    }

    #[test]
    fn undo_of_a_new_folder_removes_it() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({}));
        let before = fx.vfs.snapshot();
        let receipt = fx.run(FileOp::CreateDir {
            path: PathBuf::from("/root/untitled folder"),
        });
        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));
        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(fx.vfs.snapshot(), before);
    }

    #[test]
    fn fingerprint_mismatch_invalidates_instead_of_destroying_data() {
        let fx = setup();
        fx.vfs
            .insert_tree("/root", json!({ "a": { "f.txt": "f" }, "b": {} }));
        let receipt = fx.run(FileOp::Move {
            sources: vec![PathBuf::from("/root/a/f.txt")],
            dest_dir: PathBuf::from("/root/b"),
        });
        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));

        // The world changes underneath: the moved file is edited.
        fx.vfs.insert_file("/root/b/f.txt", 99);
        let outcome = fx.apply_undo(&mut stack);
        let UndoOutcome::Invalidated { entry, reason } = outcome else {
            panic!("expected Invalidated, got {outcome:?}");
        };
        assert!(reason.contains("was modified"), "{reason}");
        assert!(!entry.inverse.is_empty(), "entry handed back for the toast");
        assert!(
            block_on(fx.vfs_dyn.metadata(Path::new("/root/b/f.txt")))
                .unwrap()
                .is_some(),
            "nothing was moved or deleted"
        );

        // A vanished path invalidates with its own reason.
        let receipt = fx.run(FileOp::Move {
            sources: vec![PathBuf::from("/root/b/f.txt")],
            dest_dir: PathBuf::from("/root/a"),
        });
        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));
        fx.vfs.remove_path("/root/a/f.txt");
        let outcome = fx.apply_undo(&mut stack);
        let UndoOutcome::Invalidated { reason, .. } = outcome else {
            panic!("expected Invalidated, got {outcome:?}");
        };
        assert!(reason.contains("no longer exists"), "{reason}");
    }

    #[test]
    fn empty_stack_and_redo_truncation() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({ "f.txt": "f" }));
        let mut stack = UndoStack::new();
        assert!(matches!(fx.apply_undo(&mut stack), UndoOutcome::Nothing));
        assert!(matches!(fx.apply_redo(&mut stack), UndoOutcome::Nothing));

        let receipt = fx.run(FileOp::Rename {
            from: PathBuf::from("/root/f.txt"),
            to: PathBuf::from("/root/g.txt"),
        });
        stack.push(fx.entry_for(&receipt));
        fx.apply_undo(&mut stack);
        assert!(stack.can_redo());

        // New work truncates the redo stack.
        let receipt = fx.run(FileOp::CreateDir {
            path: PathBuf::from("/root/new"),
        });
        stack.push(fx.entry_for(&receipt));
        assert!(!stack.can_redo());
    }

    #[test]
    fn delete_receipts_have_no_inverse() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({ "f.txt": "f" }));
        let receipt = fx.run(FileOp::Delete {
            paths: vec![PathBuf::from("/root/f.txt")],
        });
        assert!(
            block_on(UndoEntry::from_receipt(&fx.vfs_dyn, &receipt)).is_none(),
            "permanent deletion is not undoable"
        );
    }
}
