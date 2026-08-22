//! The plan §7 M3 acceptance test: a scripted torture sequence — copy a tree
//! onto a destination with pre-seeded conflicts (resolved mixed: keep-both
//! some, skip some, replace some via apply-to-all), cancel a second copy
//! mid-flight, move a directory and undo the move, delete to trash and undo
//! (restore) — leaves the filesystem **exactly** correct. The same script runs
//! against `RealVfs` on a real `tempfile` tree AND against `FakeVfs`
//! (ARCHITECTURE.md §9, fs-core integration row). The final assertion is a
//! full-tree snapshot compare through the `Vfs`, so no partial or temp file
//! can survive unnoticed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use fs_core::{
    ConflictChoice, CopyCancelled, EntryKind, FakeVfs, FileOp, JobEvent, JobId, JobQueue,
    OpReceipt, ProgressFn, RealVfs, RemoveOptions, Resolution, Spawner, TestSpawner, UndoEntry,
    UndoOutcome, UndoStack, Vfs,
};
use futures::StreamExt as _;
use futures::executor::block_on;
use serde_json::json;

struct Harness {
    vfs: Arc<dyn Vfs>,
    queue: Arc<JobQueue>,
    events: async_channel::Receiver<JobEvent>,
    root: PathBuf,
    /// Keeps the RealVfs temp tree alive for the harness's lifetime.
    _temp: Option<tempfile::TempDir>,
}

impl Harness {
    fn real() -> Self {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs: Arc<dyn Vfs> = Arc::new(RealVfs::new(spawner.clone()));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let queue = JobQueue::new(vfs.clone(), spawner);
        let events = queue.subscribe();
        Self {
            vfs,
            queue,
            events,
            root,
            _temp: Some(temp),
        }
    }

    fn fake() -> Self {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let fake = FakeVfs::new(spawner.clone());
        fake.insert_tree("/torture", json!({}));
        let vfs: Arc<dyn Vfs> = fake;
        let queue = JobQueue::new(vfs.clone(), spawner);
        let events = queue.subscribe();
        Self {
            vfs,
            queue,
            events,
            root: PathBuf::from("/torture"),
            _temp: None,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn mkdir(&self, rel: &str) {
        block_on(self.vfs.create_dir(&self.path(rel))).unwrap();
    }

    /// Seed a file through the `Vfs` (parents created), so the same script
    /// builds the world on both implementations.
    fn write(&self, rel: &str, contents: &str) {
        self.write_bytes(rel, contents.as_bytes().to_vec());
    }

    fn write_bytes(&self, rel: &str, contents: Vec<u8>) {
        block_on(self.vfs.atomic_write(&self.path(rel), contents)).unwrap();
    }

    fn exists(&self, rel: &str) -> bool {
        block_on(self.vfs.metadata(&self.path(rel)))
            .unwrap()
            .is_some()
    }

    fn contents(&self, rel: &str) -> Vec<u8> {
        block_on(self.vfs.load(&self.path(rel))).unwrap()
    }

    fn next_event(&self) -> JobEvent {
        block_on(self.events.recv()).expect("event stream open")
    }

    /// Pump events until `id` reaches a terminal state, answering each
    /// conflict with the next scripted decision and asserting the job parked
    /// on the scripted destination (children execute in name order). Panics
    /// if the job asks more or fewer questions than the script holds — an
    /// apply-to-all must suppress the prompts after it.
    fn run_scripted(&self, id: JobId, script: &[(PathBuf, Resolution)]) -> JobEvent {
        let mut script = script.iter();
        loop {
            match self.next_event() {
                JobEvent::NeedsDecision { id: i, conflict } if i == id => {
                    let (expected_dest, resolution) = script
                        .next()
                        .expect("a scripted decision for every conflict");
                    assert_eq!(
                        &conflict.dest, expected_dest,
                        "conflicts park in scripted order"
                    );
                    self.queue.resolve(id, *resolution);
                }
                event @ (JobEvent::Completed { .. }
                | JobEvent::Failed { .. }
                | JobEvent::Cancelled { .. }) => {
                    let terminal_id = match &event {
                        JobEvent::Completed { id, .. }
                        | JobEvent::Failed { id, .. }
                        | JobEvent::Cancelled { id } => *id,
                        _ => unreachable!(),
                    };
                    if terminal_id == id {
                        assert!(
                            script.next().is_none(),
                            "every scripted decision was consumed"
                        );
                        return event;
                    }
                }
                _ => {}
            }
        }
    }

    fn expect_completed(&self, id: JobId) -> OpReceipt {
        self.expect_completed_scripted(id, &[])
    }

    fn expect_completed_scripted(&self, id: JobId, script: &[(PathBuf, Resolution)]) -> OpReceipt {
        match self.run_scripted(id, script) {
            JobEvent::Completed { receipt, .. } => receipt,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Wait until `id` parks on its first conflict (the cancel-while-parked
    /// entry point).
    fn wait_parked(&self, id: JobId) {
        loop {
            match self.next_event() {
                JobEvent::NeedsDecision { id: i, .. } if i == id => return,
                JobEvent::Completed { id: i, .. }
                | JobEvent::Failed { id: i, .. }
                | JobEvent::Cancelled { id: i }
                    if i == id =>
                {
                    panic!("job reached a terminal state instead of parking")
                }
                _ => {}
            }
        }
    }

    /// Wait for every job in `jobs` to complete (undo submissions must all
    /// succeed).
    fn wait_jobs(&self, jobs: &[JobId]) {
        let mut pending: Vec<JobId> = jobs.to_vec();
        while !pending.is_empty() {
            match self.next_event() {
                JobEvent::Completed { id, .. } => pending.retain(|j| *j != id),
                JobEvent::Failed { id, error } if pending.contains(&id) => {
                    panic!("undo job failed: {error}")
                }
                JobEvent::Cancelled { id } if pending.contains(&id) => {
                    panic!("undo job cancelled")
                }
                _ => {}
            }
        }
    }

    /// Walk the whole tree through the `Vfs` into
    /// `(relative path, Option<contents>)` — `None` marks a directory. The
    /// exactness oracle for the final assertion.
    fn walk(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let stream = block_on(self.vfs.read_dir(&dir)).expect("read_dir");
            let entries: Vec<_> = block_on(stream.collect::<Vec<_>>());
            for entry in entries {
                let entry = entry.expect("dir entry");
                let relative = entry.path.strip_prefix(&self.root).unwrap().to_path_buf();
                if matches!(entry.kind, EntryKind::Dir) {
                    out.insert(relative, None);
                    stack.push(entry.path.to_path_buf());
                } else {
                    let contents = block_on(self.vfs.load(&entry.path)).expect("file contents");
                    out.insert(relative, Some(contents));
                }
            }
        }
        out
    }
}

fn resolution(choice: ConflictChoice, apply_to_all: bool) -> Resolution {
    Resolution {
        choice,
        apply_to_all,
    }
}

fn expected_tree(entries: &[(&str, Option<&str>)]) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    entries
        .iter()
        .map(|(path, contents)| (PathBuf::from(path), contents.map(|c| c.as_bytes().to_vec())))
        .collect()
}

/// The plan §7 M3 acceptance script, generic over the `Vfs` implementation.
fn run_torture_sequence(harness: &Harness) {
    // The world before the torture: a source tree, a destination already
    // holding conflicting entries (and a mergeable subdirectory), a
    // cancellation target, a move victim, and a trash victim.
    harness.mkdir("dest/tree/sub");
    harness.mkdir("move-dest");
    for (path, contents) in [
        ("src/tree/a.txt", "new a"),
        ("src/tree/b.txt", "new b"),
        ("src/tree/c.txt", "new c"),
        ("src/tree/d.txt", "new d"),
        ("src/tree/sub/e.txt", "new e"),
        ("dest/tree/a.txt", "old a"),
        ("dest/tree/b.txt", "old b"),
        ("dest/tree/c.txt", "old c"),
        ("dest/tree/d.txt", "old d"),
        ("cancel-src/x.txt", "never copied"),
        ("cancel-dest/x.txt", "untouched"),
        ("move-src/reports/q1.txt", "q1"),
        ("move-src/reports/deep/q2.txt", "q2"),
        ("docs/doomed.txt", "bye"),
    ] {
        harness.write(path, contents);
    }

    // ---- Act 1: copy a tree onto a destination with conflicts, resolving
    // them mixed. dest/tree and dest/tree/sub merge silently (dirs merge);
    // the conflicting files park in name order: a.txt → Keep both, b.txt →
    // Skip, c.txt → Replace with apply-to-all — which must silently replace
    // d.txt too (run_scripted panics on a fourth prompt). ----
    let copy = harness.queue.submit(FileOp::Copy {
        sources: vec![harness.path("src/tree")],
        dest_dir: harness.path("dest"),
    });
    let receipt = harness.expect_completed_scripted(
        copy,
        &[
            (
                harness.path("dest/tree/a.txt"),
                resolution(ConflictChoice::KeepBoth, false),
            ),
            (
                harness.path("dest/tree/b.txt"),
                resolution(ConflictChoice::Skip, false),
            ),
            (
                harness.path("dest/tree/c.txt"),
                resolution(ConflictChoice::Replace, true),
            ),
        ],
    );
    assert!(
        receipt.created.is_empty(),
        "merged top-level dir is not recorded as created (undo must not delete it): {receipt:?}"
    );

    // ---- Act 2a: cancel a second copy mid-flight, parked on its conflict. ----
    let parked = harness.queue.submit(FileOp::Copy {
        sources: vec![harness.path("cancel-src/x.txt")],
        dest_dir: harness.path("cancel-dest"),
    });
    harness.wait_parked(parked);
    harness.queue.cancel(parked);
    let terminal = harness.run_scripted(parked, &[]);
    assert!(
        matches!(terminal, JobEvent::Cancelled { id } if id == parked),
        "cancel while parked ends in Cancelled: {terminal:?}"
    );
    assert_eq!(harness.contents("cancel-dest/x.txt"), b"untouched");

    // ---- Act 2b: cancel mid-file (between chunks) leaves no partial file. ----
    // 3 MiB clears both chunk sizes (RealVfs 1 MiB, FakeVfs 1 KiB).
    harness.write_bytes("cancel-src/big.bin", vec![9u8; 3 * 1024 * 1024]);
    let abort_after_first_chunk: ProgressFn = Arc::new(|done, _| done == 0);
    let error = block_on(harness.vfs.copy(
        &harness.path("cancel-src/big.bin"),
        &harness.path("cancel-dest/big.bin"),
        abort_after_first_chunk,
    ))
    .unwrap_err();
    assert!(error.is::<CopyCancelled>(), "typed cancellation: {error}");
    assert!(
        !harness.exists("cancel-dest/big.bin"),
        "cancel mid-copy leaves no partial file"
    );
    block_on(harness.vfs.remove(
        &harness.path("cancel-src/big.bin"),
        RemoveOptions::default(),
    ))
    .unwrap();

    // ---- Act 3: move a directory (whole subtree), recorded for undo. ----
    let mut undo_stack = UndoStack::new();
    let move_job = harness.queue.submit(FileOp::Move {
        sources: vec![harness.path("move-src/reports")],
        dest_dir: harness.path("move-dest"),
    });
    let receipt = harness.expect_completed(move_job);
    assert_eq!(
        receipt.moved,
        vec![(
            harness.path("move-src/reports"),
            harness.path("move-dest/reports")
        )]
    );
    assert!(!harness.exists("move-src/reports"));
    assert_eq!(harness.contents("move-dest/reports/deep/q2.txt"), b"q2");
    let entry = block_on(UndoEntry::from_receipt(&harness.vfs, &receipt))
        .expect("a directory move is undoable");
    undo_stack.push(entry);

    // ---- Act 4: delete to trash, recorded for undo. ----
    let trash_job = harness.queue.submit(FileOp::TrashOp {
        paths: vec![harness.path("docs/doomed.txt")],
    });
    let receipt = harness.expect_completed(trash_job);
    assert!(!harness.exists("docs/doomed.txt"));
    assert_eq!(receipt.trashed.len(), 1);
    let entry = block_on(UndoEntry::from_receipt(&harness.vfs, &receipt))
        .expect("a trash is undoable (restore)");
    undo_stack.push(entry);

    // ---- Undo both, LIFO through the real stack: first the trash (restore
    // from the .fake-trash), then the directory move (rename back). ----
    for label in ["undo of trash (restore)", "undo of directory move"] {
        let outcome = block_on(undo_stack.undo(&harness.vfs, &harness.queue));
        let UndoOutcome::Applied { jobs } = outcome else {
            panic!("{label}: expected Applied, got {outcome:?}");
        };
        harness.wait_jobs(&jobs);
    }
    assert!(!undo_stack.can_undo());

    // ---- Final oracle: the whole tree, exactly (also proves no partial,
    // temp, or leftover .fake-trash entries survive anywhere). ----
    let expected = expected_tree(&[
        // Sources are untouched by the copy.
        ("src", None),
        ("src/tree", None),
        ("src/tree/a.txt", Some("new a")),
        ("src/tree/b.txt", Some("new b")),
        ("src/tree/c.txt", Some("new c")),
        ("src/tree/d.txt", Some("new d")),
        ("src/tree/sub", None),
        ("src/tree/sub/e.txt", Some("new e")),
        // The conflicted copy: kept-both a, skipped b, replaced c and
        // (via apply-to-all) d, merged sub with fresh e.
        ("dest", None),
        ("dest/tree", None),
        ("dest/tree/a.txt", Some("old a")),
        ("dest/tree/a copy.txt", Some("new a")),
        ("dest/tree/b.txt", Some("old b")),
        ("dest/tree/c.txt", Some("new c")),
        ("dest/tree/d.txt", Some("new d")),
        ("dest/tree/sub", None),
        ("dest/tree/sub/e.txt", Some("new e")),
        // Both cancellations left the world exactly as it was.
        ("cancel-src", None),
        ("cancel-src/x.txt", Some("never copied")),
        ("cancel-dest", None),
        ("cancel-dest/x.txt", Some("untouched")),
        // The undone move: directory back home, destination empty again.
        ("move-src", None),
        ("move-src/reports", None),
        ("move-src/reports/q1.txt", Some("q1")),
        ("move-src/reports/deep", None),
        ("move-src/reports/deep/q2.txt", Some("q2")),
        ("move-dest", None),
        // The undone trash: restored, .fake-trash cleaned away.
        ("docs", None),
        ("docs/doomed.txt", Some("bye")),
    ]);
    assert_eq!(
        harness.walk(),
        expected,
        "the filesystem is exactly correct"
    );
}

#[test]
fn torture_sequence_on_a_real_temp_tree() {
    run_torture_sequence(&Harness::real());
}

#[test]
fn torture_sequence_on_the_fake_vfs() {
    run_torture_sequence(&Harness::fake());
}

/// ARCHITECTURE.md §9 ops row: every [`FileOp`] variant runs through the
/// [`JobQueue`] against `RealVfs` on a `tempfile` tree (the FakeVfs half of
/// the row lives in `ops/queue.rs`'s unit tests). Copy/Move/Rename get their
/// deep coverage in the torture sequence above; this exercises the remaining
/// vocabulary end to end.
#[test]
fn every_file_op_runs_against_a_real_temp_tree() {
    let harness = Harness::real();
    let root = harness.root.clone();
    for (path, contents) in [
        ("work/report.pdf", "pdf bytes"),
        ("work/junk/old.log", "stale"),
        ("work/doomed.txt", "trash me"),
    ] {
        harness.write(path, contents);
    }
    harness.mkdir("dest");

    // CreateDir / CreateFile.
    let id = harness.queue.submit(FileOp::CreateDir {
        path: root.join("work/newdir"),
    });
    let receipt = harness.expect_completed(id);
    assert_eq!(receipt.created, vec![root.join("work/newdir")]);
    assert!(root.join("work/newdir").is_dir());

    let id = harness.queue.submit(FileOp::CreateFile {
        path: root.join("work/notes.txt"),
    });
    let receipt = harness.expect_completed(id);
    assert_eq!(receipt.created, vec![root.join("work/notes.txt")]);
    assert!(root.join("work/notes.txt").is_file());

    // Duplicate: keep-both name next to the source.
    let id = harness.queue.submit(FileOp::Duplicate {
        sources: vec![root.join("work/report.pdf")],
    });
    let receipt = harness.expect_completed(id);
    assert_eq!(receipt.created, vec![root.join("work/report copy.pdf")]);
    assert_eq!(
        std::fs::read(root.join("work/report copy.pdf")).unwrap(),
        b"pdf bytes"
    );

    // Copy / Move / Rename (deep coverage in the torture sequence).
    let id = harness.queue.submit(FileOp::Copy {
        sources: vec![root.join("work/report.pdf")],
        dest_dir: root.join("dest"),
    });
    harness.expect_completed(id);
    assert_eq!(
        std::fs::read(root.join("dest/report.pdf")).unwrap(),
        b"pdf bytes"
    );

    let id = harness.queue.submit(FileOp::Move {
        sources: vec![root.join("work/notes.txt")],
        dest_dir: root.join("dest"),
    });
    harness.expect_completed(id);
    assert!(!root.join("work/notes.txt").exists());
    assert!(root.join("dest/notes.txt").is_file());

    let id = harness.queue.submit(FileOp::Rename {
        from: root.join("dest/notes.txt"),
        to: root.join("dest/renamed.txt"),
    });
    let receipt = harness.expect_completed(id);
    assert_eq!(
        receipt.moved,
        vec![(root.join("dest/notes.txt"), root.join("dest/renamed.txt"))]
    );
    assert!(root.join("dest/renamed.txt").is_file());

    // TrashOp → Restore round-trips through the on-disk `.fake-trash`.
    let id = harness.queue.submit(FileOp::TrashOp {
        paths: vec![root.join("work/doomed.txt")],
    });
    let receipt = harness.expect_completed(id);
    assert!(!root.join("work/doomed.txt").exists());
    assert_eq!(receipt.trashed.len(), 1);
    let token = receipt.trashed[0].clone();
    assert!(token.trashed.exists(), "payload is parked in .fake-trash");

    let id = harness.queue.submit(FileOp::Restore {
        ids: vec![token.clone()],
    });
    let receipt = harness.expect_completed(id);
    assert_eq!(
        receipt.restored,
        vec![(token, root.join("work/doomed.txt"))]
    );
    assert_eq!(
        std::fs::read(root.join("work/doomed.txt")).unwrap(),
        b"trash me"
    );
    assert!(
        !root.join("work/.fake-trash").exists(),
        "last restore cleans the .fake-trash root"
    );

    // Delete: permanent, recursive, uninvertible receipt.
    let id = harness.queue.submit(FileOp::Delete {
        paths: vec![root.join("work/junk")],
    });
    let receipt = harness.expect_completed(id);
    assert!(!root.join("work/junk").exists());
    assert!(receipt.created.is_empty());
    assert!(receipt.moved.is_empty());
    assert!(receipt.trashed.is_empty());
}
