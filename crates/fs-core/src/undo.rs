//! Undo/redo of file operations (ARCHITECTURE.md §6, `undo.rs`).
//!
//! An [`UndoEntry`] is the *inverse* op recorded from an [`OpReceipt`] at job
//! completion: rename→rename-back, move→move-back, copy→remove-created,
//! new-folder→remove, trash→restore. Applying an entry re-submits through the
//! [`JobQueue`], so undo gets progress/conflicts for free. Each entry stores
//! `(path, mtime)` fingerprints; if the world changed underneath, the entry is
//! **skipped** with a first-class, testable outcome
//! ([`UndoOutcome::Invalidated`]) rather than destroying data.
//!
//! M6b adds the attribute ops (chmod → chmod-back, chown → chown-back,
//! set-tags → set-the-old-tags), whose inverse is built from the *previous*
//! values the job captured ([`OpReceipt::restored_attrs`]). They are guarded by
//! [`AttrGuard`] rather than a fingerprint, because `chmod` changes ctime and
//! not mtime and an mtime fingerprint therefore cannot see it at all.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use crate::ops::job::{JobId, OpReceipt, PrevAttrs};
use crate::ops::{FileOp, JobQueue};
use crate::platform::Platform;
use crate::tags::{decode_tag_strings, encode_tag_strings};
use crate::vfs::{PERM_BITS, Vfs};

/// Expected `(path, mtime)` — undo validation input.
#[derive(Clone, Debug, PartialEq)]
pub struct Fingerprint {
    pub path: PathBuf,
    pub modified: SystemTime,
}

/// An attribute guard: `path`'s attribute must still hold `expected` — the
/// value this entry's recorded job left there — for the inverse to be safe.
///
/// Why not a [`Fingerprint`]: fingerprints are `(path, mtime)`, and **`chmod`
/// does not change mtime** (it changes ctime), so an mtime fingerprint is blind
/// to exactly the change these ops make. Two files whose modes were swapped by
/// another process behind our back have identical mtimes, and an mtime-guarded
/// undo would happily overwrite the newer mode. So each attribute op guards the
/// dimension it actually wrote: the mode, the owner/group, or the tag set.
#[derive(Clone, Debug, PartialEq)]
pub struct AttrGuard {
    pub path: PathBuf,
    /// The value the recorded job wrote. For [`PrevAttrs::Ownership`] only the
    /// `Some` halves are compared — a `Chown` that changed the group alone must
    /// not be invalidated by an unrelated owner change.
    pub expected: PrevAttrs,
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
    /// Attribute guards for the M6b permission/ownership/tag ops, which
    /// `fingerprints` structurally cannot cover (see [`AttrGuard`]).
    pub attr_guards: Vec<AttrGuard>,
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
        // M6b attribute ops: one inverse op per distinct previous value, so a
        // mixed selection (644 here, 755 there) comes back exactly as it was
        // rather than being flattened to one mode.
        inverse.extend(inverse_attr_ops(&receipt.restored_attrs));
        let changed: Vec<PathBuf> = receipt
            .restored_attrs
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        // Guards cover only the paths that actually changed — a path in the
        // submitted op that failed (EPERM, vanished) was never written, so
        // guarding it would invalidate an otherwise perfectly good undo.
        let attr_guards = attr_guards_for(&receipt.op, &changed);

        if inverse.is_empty() {
            return None;
        }
        Some(UndoEntry {
            inverse,
            redo: vec![receipt.op.clone()],
            fingerprints,
            attr_guards,
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
        // Redo's attribute guards are the mirror image: whatever *this* entry's
        // inverse ops wrote must still hold before re-applying the original.
        let attr_guards = self
            .inverse
            .iter()
            .flat_map(|op| attr_guards_for(op, op_paths(op)))
            .collect();
        UndoEntry {
            inverse: self.redo.clone(),
            redo: self.inverse.clone(),
            fingerprints,
            attr_guards,
        }
    }
}

/// The paths an attribute op targets (empty for every other op).
fn op_paths(op: &FileOp) -> &[PathBuf] {
    match op {
        FileOp::Chmod { paths, .. }
        | FileOp::Chown { paths, .. }
        | FileOp::SetTags { paths, .. } => paths,
        _ => &[],
    }
}

/// The guards for an attribute op that has just been applied to `paths`: each
/// path's attribute must still hold the value the op wrote.
fn attr_guards_for(op: &FileOp, paths: &[PathBuf]) -> Vec<AttrGuard> {
    let expected = match op {
        FileOp::Chmod { mode, .. } => PrevAttrs::Mode(mode & PERM_BITS),
        FileOp::Chown { owner, group, .. } => PrevAttrs::Ownership {
            owner: owner.clone(),
            group: group.clone(),
        },
        // Round-tripped through the codec because that is what actually landed
        // on disk: blank names dropped, duplicate names collapsed.
        FileOp::SetTags { tags, .. } => {
            PrevAttrs::Tags(decode_tag_strings(&encode_tag_strings(tags)))
        }
        _ => return Vec::new(),
    };
    paths
        .iter()
        .map(|path| AttrGuard {
            path: path.clone(),
            expected: expected.clone(),
        })
        .collect()
}

/// Group `(path, previous value)` pairs into as few attribute ops as restore
/// them exactly: one op per distinct previous value, paths in first-seen order
/// so the result is deterministic (and diffable in a test).
fn inverse_attr_ops(restored: &[(PathBuf, PrevAttrs)]) -> Vec<FileOp> {
    let mut groups: Vec<(PrevAttrs, Vec<PathBuf>)> = Vec::new();
    for (path, previous) in restored {
        match groups.iter_mut().find(|(value, _)| value == previous) {
            Some((_, paths)) => paths.push(path.clone()),
            None => groups.push((previous.clone(), vec![path.clone()])),
        }
    }
    groups
        .into_iter()
        .map(|(previous, paths)| match previous {
            PrevAttrs::Mode(mode) => FileOp::Chmod { paths, mode },
            PrevAttrs::Ownership { owner, group } => FileOp::Chown {
                paths,
                owner,
                group,
            },
            PrevAttrs::Tags(tags) => FileOp::SetTags { paths, tags },
        })
        .collect()
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
        if let Some(reason) = validate_entry(vfs, queue, &entry).await {
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
        if let Some(reason) = validate_entry(vfs, queue, &entry).await {
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

/// `None` when the entry is still safe to apply: every mtime fingerprint holds
/// **and** every attribute guard holds.
async fn validate_entry(
    vfs: &Arc<dyn Vfs>,
    queue: &Arc<JobQueue>,
    entry: &UndoEntry,
) -> Option<String> {
    if let Some(reason) = validate(vfs, &entry.fingerprints).await {
        return Some(reason);
    }
    validate_attrs(vfs, queue.platform().as_ref(), &entry.attr_guards).await
}

/// `None` when every attribute still holds the value the recorded job wrote.
///
/// A guard whose value cannot be read at all is **skipped**, not treated as a
/// mismatch: for ownership and tags the read needs [`Platform`] services the
/// queue may not have been given, and for a filesystem with no unix mode there
/// is nothing to guard. The residual risk of a skipped guard is the same one
/// undo has always had — the op itself still fails loudly if the path is gone.
async fn validate_attrs(
    vfs: &Arc<dyn Vfs>,
    platform: Option<&Arc<dyn Platform>>,
    guards: &[AttrGuard],
) -> Option<String> {
    for guard in guards {
        let name = display_name(&guard.path);
        match &guard.expected {
            PrevAttrs::Mode(expected) => match vfs.mode(&guard.path).await {
                Ok(Some(actual)) if actual == *expected => {}
                Ok(Some(_)) => return Some(format!("'{name}' permissions changed since")),
                Ok(None) => {} // no unix mode here: nothing to guard
                Err(error) => return Some(format!("'{name}': {error}")),
            },
            PrevAttrs::Ownership { owner, group } => {
                let Some(platform) = platform else { continue };
                match platform.file_attrs(&guard.path).await {
                    Ok(attrs) => {
                        // Only the halves the op actually set are compared.
                        let owner_ok = owner.is_none() || attrs.owner == *owner;
                        let group_ok = group.is_none() || attrs.group == *group;
                        if !owner_ok || !group_ok {
                            return Some(format!("'{name}' ownership changed since"));
                        }
                    }
                    Err(error) => return Some(format!("'{name}': {error}")),
                }
            }
            PrevAttrs::Tags(expected) => {
                let Some(platform) = platform else { continue };
                match platform.read_tags(&guard.path).await {
                    Ok(actual) if actual == *expected => {}
                    Ok(_) => return Some(format!("'{name}' tags changed since")),
                    Err(error) => return Some(format!("'{name}': {error}")),
                }
            }
        }
    }
    None
}

fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `None` when every fingerprint still holds; otherwise why the entry is
/// stale (drives the invalidation toast).
async fn validate(vfs: &Arc<dyn Vfs>, fingerprints: &[Fingerprint]) -> Option<String> {
    for fingerprint in fingerprints {
        let name = display_name(&fingerprint.path);
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
    use crate::platform::StubPlatform;
    use crate::tags::{Tag, TagColor};
    use crate::vfs::FakeVfs;
    use futures::executor::block_on;
    use serde_json::json;
    use std::path::Path;

    struct Fixture {
        vfs: Arc<FakeVfs>,
        vfs_dyn: Arc<dyn Vfs>,
        platform: Arc<StubPlatform>,
        queue: Arc<JobQueue>,
        events: async_channel::Receiver<JobEvent>,
    }

    fn setup() -> Fixture {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = FakeVfs::new(spawner.clone());
        let vfs_dyn: Arc<dyn Vfs> = vfs.clone();
        let platform = Arc::new(StubPlatform::new());
        let queue = JobQueue::with_platform(
            vfs_dyn.clone(),
            platform.clone() as Arc<dyn Platform>,
            spawner,
        );
        let events = queue.subscribe();
        Fixture {
            vfs,
            vfs_dyn,
            platform,
            queue,
            events,
        }
    }

    fn mode_of(fx: &Fixture, path: &str) -> u32 {
        block_on(fx.vfs_dyn.mode(Path::new(path)))
            .expect("mode readable")
            .expect("fake vfs models a mode")
    }

    fn tags_of(fx: &Fixture, path: &str) -> Vec<Tag> {
        block_on(fx.platform.read_tags(Path::new(path))).unwrap()
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

    // ---------------------------------------------------------------------
    // M6b attribute ops: exact undo from captured previous values, guarded on
    // the dimension the op actually wrote.
    // ---------------------------------------------------------------------

    #[test]
    fn undo_of_a_mixed_chmod_restores_every_path_to_its_own_mode() {
        let fx = setup();
        fx.vfs
            .insert_tree("/root", json!({ "a.txt": "a", "b.sh": "b", "c.txt": "c" }));
        block_on(fx.vfs_dyn.set_mode(Path::new("/root/b.sh"), 0o755)).unwrap();

        let receipt = fx.run(FileOp::Chmod {
            paths: vec![
                PathBuf::from("/root/a.txt"),
                PathBuf::from("/root/b.sh"),
                PathBuf::from("/root/c.txt"),
            ],
            mode: 0o600,
        });
        let entry = fx.entry_for(&receipt);
        assert_eq!(
            entry.inverse,
            vec![
                // One op per distinct previous mode, a.txt and c.txt grouped.
                FileOp::Chmod {
                    paths: vec![PathBuf::from("/root/a.txt"), PathBuf::from("/root/c.txt")],
                    mode: 0o644,
                },
                FileOp::Chmod {
                    paths: vec![PathBuf::from("/root/b.sh")],
                    mode: 0o755,
                },
            ]
        );
        assert_eq!(
            entry.attr_guards.len(),
            3,
            "every changed path is guarded: {:?}",
            entry.attr_guards
        );
        assert!(
            entry.fingerprints.is_empty(),
            "no mtime fingerprint: chmod does not change mtime"
        );

        let mut stack = UndoStack::new();
        stack.push(entry);
        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(mode_of(&fx, "/root/a.txt"), 0o644);
        assert_eq!(mode_of(&fx, "/root/b.sh"), 0o755);
        assert_eq!(mode_of(&fx, "/root/c.txt"), 0o644);

        // …and redo re-applies the original mode to all three.
        assert!(matches!(
            fx.apply_redo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(mode_of(&fx, "/root/a.txt"), 0o600);
        assert_eq!(mode_of(&fx, "/root/b.sh"), 0o600);
        assert_eq!(mode_of(&fx, "/root/c.txt"), 0o600);
    }

    #[test]
    fn an_interleaved_permission_change_invalidates_the_undo() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({ "a.txt": "a" }));
        let receipt = fx.run(FileOp::Chmod {
            paths: vec![PathBuf::from("/root/a.txt")],
            mode: 0o600,
        });
        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));

        // Somebody else chmods the file behind our back. An mtime fingerprint
        // could not see this — the AttrGuard is the whole point.
        block_on(fx.vfs_dyn.set_mode(Path::new("/root/a.txt"), 0o777)).unwrap();

        let outcome = fx.apply_undo(&mut stack);
        let UndoOutcome::Invalidated { reason, .. } = outcome else {
            panic!("expected Invalidated, got {outcome:?}");
        };
        assert!(reason.contains("permissions changed since"), "{reason}");
        assert_eq!(
            mode_of(&fx, "/root/a.txt"),
            0o777,
            "the newer mode was left alone"
        );
    }

    #[test]
    fn undo_of_a_partly_failed_chmod_covers_exactly_what_changed() {
        let fx = setup();
        fx.vfs
            .insert_tree("/root", json!({ "ok.txt": "o", "denied.txt": "d" }));
        fx.vfs.set_error("/root/denied.txt", "Permission denied");

        let receipt = fx.run(FileOp::Chmod {
            paths: vec![
                PathBuf::from("/root/ok.txt"),
                PathBuf::from("/root/denied.txt"),
            ],
            mode: 0o600,
        });
        assert_eq!(receipt.failed.len(), 1);
        let entry = fx.entry_for(&receipt);
        assert_eq!(
            entry.inverse,
            vec![FileOp::Chmod {
                paths: vec![PathBuf::from("/root/ok.txt")],
                mode: 0o644,
            }],
            "the failed path is not in the inverse"
        );
        assert_eq!(
            entry.attr_guards.len(),
            1,
            "…nor guarded, or the undo would be invalidated by a path it never touched"
        );

        let mut stack = UndoStack::new();
        stack.push(entry);
        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(mode_of(&fx, "/root/ok.txt"), 0o644);
    }

    #[test]
    fn undo_of_set_tags_restores_the_previous_sets_including_the_empty_one() {
        let fx = setup();
        fx.vfs
            .insert_tree("/root", json!({ "a.txt": "a", "b.txt": "b" }));
        fx.platform
            .seed_tags("/root/a.txt", vec![Tag::new("Work", TagColor::Blue)]);

        let receipt = fx.run(FileOp::SetTags {
            paths: vec![PathBuf::from("/root/a.txt"), PathBuf::from("/root/b.txt")],
            tags: vec![Tag::new("Red", TagColor::Red)],
        });
        let entry = fx.entry_for(&receipt);
        assert_eq!(
            entry.inverse,
            vec![
                FileOp::SetTags {
                    paths: vec![PathBuf::from("/root/a.txt")],
                    tags: vec![Tag::new("Work", TagColor::Blue)],
                },
                FileOp::SetTags {
                    paths: vec![PathBuf::from("/root/b.txt")],
                    tags: vec![],
                },
            ]
        );

        let mut stack = UndoStack::new();
        stack.push(entry);
        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        assert_eq!(
            tags_of(&fx, "/root/a.txt"),
            [Tag::new("Work", TagColor::Blue)]
        );
        assert_eq!(tags_of(&fx, "/root/b.txt"), [], "back to untagged");
    }

    #[test]
    fn tags_changed_behind_our_back_invalidate_the_undo() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({ "a.txt": "a" }));
        let receipt = fx.run(FileOp::SetTags {
            paths: vec![PathBuf::from("/root/a.txt")],
            tags: vec![Tag::new("Red", TagColor::Red)],
        });
        let mut stack = UndoStack::new();
        stack.push(fx.entry_for(&receipt));

        // Finder (or another window) retags the file.
        fx.platform
            .seed_tags("/root/a.txt", vec![Tag::new("Later", TagColor::Purple)]);

        let outcome = fx.apply_undo(&mut stack);
        let UndoOutcome::Invalidated { reason, .. } = outcome else {
            panic!("expected Invalidated, got {outcome:?}");
        };
        assert!(reason.contains("tags changed since"), "{reason}");
        assert_eq!(
            tags_of(&fx, "/root/a.txt"),
            [Tag::new("Later", TagColor::Purple)]
        );
    }

    #[test]
    fn undo_of_a_chown_puts_both_halves_back() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({ "a.txt": "a" }));
        let receipt = fx.run(FileOp::Chown {
            paths: vec![PathBuf::from("/root/a.txt")],
            owner: None,
            group: Some("wheel".to_string()),
        });
        let entry = fx.entry_for(&receipt);
        assert_eq!(
            entry.inverse,
            vec![FileOp::Chown {
                paths: vec![PathBuf::from("/root/a.txt")],
                owner: Some("stub-owner".to_string()),
                group: Some("stub-group".to_string()),
            }]
        );
        // The guard only names the half the op set, so an unrelated owner
        // change elsewhere cannot invalidate a group-only undo.
        assert_eq!(
            entry.attr_guards,
            vec![AttrGuard {
                path: PathBuf::from("/root/a.txt"),
                expected: PrevAttrs::Ownership {
                    owner: None,
                    group: Some("wheel".to_string()),
                },
            }]
        );

        let mut stack = UndoStack::new();
        stack.push(entry);
        assert!(matches!(
            fx.apply_undo(&mut stack),
            UndoOutcome::Applied { .. }
        ));
        let attrs = block_on(fx.platform.file_attrs(Path::new("/root/a.txt"))).unwrap();
        assert_eq!(attrs.group.as_deref(), Some("stub-group"));
        assert_eq!(attrs.owner.as_deref(), Some("stub-owner"));
    }

    #[test]
    fn an_attribute_job_that_changed_nothing_has_no_inverse() {
        let fx = setup();
        fx.vfs.insert_tree("/root", json!({}));
        // A receipt with an empty `restored_attrs` (every path failed, or an
        // empty selection) must not produce an undo entry.
        let receipt = OpReceipt::empty(FileOp::Chmod {
            paths: vec![PathBuf::from("/root/gone.txt")],
            mode: 0o600,
        });
        assert!(block_on(UndoEntry::from_receipt(&fx.vfs_dyn, &receipt)).is_none());
    }

    #[test]
    fn attr_guards_are_only_built_for_the_attribute_ops() {
        let paths = vec![PathBuf::from("/root/a.txt")];
        assert!(
            attr_guards_for(
                &FileOp::Delete {
                    paths: paths.clone()
                },
                &paths
            )
            .is_empty()
        );
        // A blank tag name never reaches the disk, so the guard must expect the
        // normalized set — otherwise every tag undo would read as "changed".
        let guards = attr_guards_for(
            &FileOp::SetTags {
                paths: paths.clone(),
                tags: vec![Tag::new("Work", TagColor::Blue), Tag::uncolored("")],
            },
            &paths,
        );
        assert_eq!(
            guards[0].expected,
            PrevAttrs::Tags(vec![Tag::new("Work", TagColor::Blue)])
        );
    }
}
