//! M6b cross-module check: the three attribute ops — `Chmod`, `Chown`,
//! `SetTags` — running on the real job spine over a real `tempfile` tree, with
//! undo built from the receipts and applied back through the same queue
//! (ARCHITECTURE.md §9, fs-core integration row).
//!
//! Unit tests cover each piece against `FakeVfs`/`StubPlatform`; only an
//! integration test can catch the pieces disagreeing about the *same* real
//! file — a mode that reads back differently than it was written, an undo whose
//! inverse names the wrong path, a guard that invalidates a perfectly good
//! entry.
//!
//! The `Platform` half is `MacPlatform` on macOS (so the tag legs really do
//! write `com.apple.metadata:_kMDItemUserTags` on this machine) and
//! `StubPlatform` everywhere else, per CLAUDE.md's two-machine rule. The
//! `Chmod` legs are `cfg(unix)`; Windows has no unix mode, and the honest
//! failure it produces instead is asserted too.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::{
    FileOp, JobEvent, JobId, JobQueue, OpReceipt, Platform, RealVfs, STUB_PRIVILEGED_OWNER,
    Spawner, Tag, TagColor, TestSpawner, UndoEntry, UndoOutcome, UndoStack, Vfs,
};
use futures::executor::block_on;

#[cfg(target_os = "macos")]
fn platform(spawner: Arc<dyn Spawner>) -> Arc<dyn Platform> {
    Arc::new(fs_core::MacPlatform::new(spawner))
}

#[cfg(not(target_os = "macos"))]
fn platform(_spawner: Arc<dyn Spawner>) -> Arc<dyn Platform> {
    Arc::new(fs_core::StubPlatform::new())
}

struct Harness {
    vfs: Arc<dyn Vfs>,
    platform: Arc<dyn Platform>,
    queue: Arc<JobQueue>,
    events: async_channel::Receiver<JobEvent>,
    root: PathBuf,
    _temp: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs: Arc<dyn Vfs> = Arc::new(RealVfs::new(spawner.clone()));
        let platform = platform(spawner.clone());
        let temp = tempfile::tempdir().unwrap();
        let queue = JobQueue::with_platform(vfs.clone(), platform.clone(), spawner);
        let events = queue.subscribe();
        Self {
            vfs,
            platform,
            queue,
            events,
            root: temp.path().to_path_buf(),
            _temp: temp,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let path = self.path(rel);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// The terminal event for `id`, ignoring everything else on the channel.
    fn terminal(&self, id: JobId) -> JobEvent {
        loop {
            let event = block_on(self.events.recv()).expect("event stream open");
            let terminal_id = match &event {
                JobEvent::Completed { id, .. }
                | JobEvent::Failed { id, .. }
                | JobEvent::Cancelled { id } => Some(*id),
                _ => None,
            };
            if terminal_id == Some(id) {
                return event;
            }
        }
    }

    fn expect_completed(&self, op: FileOp) -> OpReceipt {
        let id = self.queue.submit(op);
        match self.terminal(id) {
            JobEvent::Completed { receipt, .. } => receipt,
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    fn expect_failed(&self, op: FileOp) -> String {
        let id = self.queue.submit(op);
        match self.terminal(id) {
            JobEvent::Failed { error, .. } => error,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn mode(&self, path: &Path) -> Option<u32> {
        block_on(self.vfs.mode(path)).expect("mode readable")
    }

    fn tags(&self, path: &Path) -> Vec<Tag> {
        block_on(self.platform.read_tags(path)).expect("tags readable")
    }

    fn undo(&self, stack: &mut UndoStack) -> UndoOutcome {
        let outcome = block_on(stack.undo(&self.vfs, &self.queue));
        if let UndoOutcome::Applied { jobs } = &outcome {
            for job in jobs {
                match self.terminal(*job) {
                    JobEvent::Completed { .. } => {}
                    other => panic!("undo job did not complete: {other:?}"),
                }
            }
        }
        outcome
    }

    fn push_entry(&self, stack: &mut UndoStack, receipt: &OpReceipt) {
        let entry = block_on(UndoEntry::from_receipt(&self.vfs, receipt))
            .expect("an attribute change is undoable");
        stack.push(entry);
    }
}

#[cfg(unix)]
#[test]
fn chmod_over_a_mixed_selection_then_undo_restores_every_original_mode() {
    let harness = Harness::new();
    let readable = harness.write("readable.txt", "r");
    let script = harness.write("script.sh", "#!/bin/sh");
    block_on(harness.vfs.set_mode(&readable, 0o644)).unwrap();
    block_on(harness.vfs.set_mode(&script, 0o755)).unwrap();

    let receipt = harness.expect_completed(FileOp::Chmod {
        paths: vec![readable.clone(), script.clone()],
        mode: 0o600,
    });
    assert_eq!(harness.mode(&readable), Some(0o600));
    assert_eq!(harness.mode(&script), Some(0o600));
    assert!(receipt.failed.is_empty());

    let mut stack = UndoStack::new();
    harness.push_entry(&mut stack, &receipt);
    assert!(matches!(
        harness.undo(&mut stack),
        UndoOutcome::Applied { .. }
    ));
    assert_eq!(
        (harness.mode(&readable), harness.mode(&script)),
        (Some(0o644), Some(0o755)),
        "each file went back to its own mode, not one shared value"
    );
}

#[cfg(unix)]
#[test]
fn a_vanished_path_is_reported_while_the_rest_of_the_selection_lands() {
    let harness = Harness::new();
    let survivor = harness.write("survivor.txt", "s");
    block_on(harness.vfs.set_mode(&survivor, 0o644)).unwrap();
    let gone = harness.path("gone.txt");

    let receipt = harness.expect_completed(FileOp::Chmod {
        paths: vec![survivor.clone(), gone.clone()],
        mode: 0o640,
    });
    assert_eq!(harness.mode(&survivor), Some(0o640));
    assert_eq!(receipt.failed.len(), 1, "{:?}", receipt.failed);
    assert_eq!(receipt.failed[0].0, gone);

    // The undo entry covers the path that changed and only that one.
    let mut stack = UndoStack::new();
    harness.push_entry(&mut stack, &receipt);
    assert!(matches!(
        harness.undo(&mut stack),
        UndoOutcome::Applied { .. }
    ));
    assert_eq!(harness.mode(&survivor), Some(0o644));

    // A job where *every* path fails changes nothing and fails outright, so
    // there is nothing to undo.
    let error = harness.expect_failed(FileOp::Chmod {
        paths: vec![gone.clone()],
        mode: 0o600,
    });
    assert!(error.contains("gone.txt"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_permission_change_behind_our_back_invalidates_the_undo_on_a_real_file() {
    let harness = Harness::new();
    let file = harness.write("guarded.txt", "g");
    block_on(harness.vfs.set_mode(&file, 0o644)).unwrap();

    let receipt = harness.expect_completed(FileOp::Chmod {
        paths: vec![file.clone()],
        mode: 0o600,
    });
    let mut stack = UndoStack::new();
    harness.push_entry(&mut stack, &receipt);

    // Another process chmods the file. Note that its mtime is untouched — the
    // reason this needs an attribute guard and not a fingerprint.
    let mtime_before = std::fs::metadata(&file).unwrap().modified().unwrap();
    block_on(harness.vfs.set_mode(&file, 0o777)).unwrap();
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        mtime_before,
        "a real chmod really does leave mtime alone"
    );

    let outcome = harness.undo(&mut stack);
    let UndoOutcome::Invalidated { reason, .. } = outcome else {
        panic!("expected Invalidated, got {outcome:?}");
    };
    assert!(reason.contains("permissions changed since"), "{reason}");
    assert_eq!(
        harness.mode(&file),
        Some(0o777),
        "the newer mode survived untouched"
    );
}

#[cfg(not(unix))]
#[test]
fn chmod_fails_honestly_where_there_are_no_unix_permissions() {
    let harness = Harness::new();
    let file = harness.write("readable.txt", "r");
    let error = harness.expect_failed(FileOp::Chmod {
        paths: vec![file],
        mode: 0o600,
    });
    assert!(error.contains("no unix permissions"), "{error}");
}

#[test]
fn set_tags_then_undo_restores_the_previous_sets_on_real_files() {
    let harness = Harness::new();
    let tagged = harness.write("tagged.txt", "t");
    let untagged = harness.write("untagged.txt", "u");

    // Start from a known state: one file already carries a tag.
    let seeded = vec![Tag::new("Work", TagColor::Blue)];
    block_on(harness.platform.write_tags(&tagged, &seeded)).unwrap();
    assert_eq!(harness.tags(&tagged), seeded);

    let applied = vec![Tag::new("Red", TagColor::Red), Tag::uncolored("Später")];
    let receipt = harness.expect_completed(FileOp::SetTags {
        paths: vec![tagged.clone(), untagged.clone()],
        tags: applied.clone(),
    });
    assert_eq!(harness.tags(&tagged), applied);
    assert_eq!(harness.tags(&untagged), applied);
    assert!(receipt.failed.is_empty());

    let mut stack = UndoStack::new();
    harness.push_entry(&mut stack, &receipt);
    assert!(matches!(
        harness.undo(&mut stack),
        UndoOutcome::Applied { .. }
    ));
    assert_eq!(harness.tags(&tagged), seeded);
    assert_eq!(
        harness.tags(&untagged),
        Vec::<Tag>::new(),
        "an untagged file goes back to untagged, not to an empty tag array"
    );
}

#[test]
fn clearing_the_tags_is_undoable_too() {
    let harness = Harness::new();
    let file = harness.write("tagged.txt", "t");
    let seeded = vec![
        Tag::new("Work", TagColor::Blue),
        Tag::new("Q1", TagColor::Green),
    ];
    block_on(harness.platform.write_tags(&file, &seeded)).unwrap();

    let receipt = harness.expect_completed(FileOp::SetTags {
        paths: vec![file.clone()],
        tags: vec![],
    });
    assert_eq!(harness.tags(&file), Vec::<Tag>::new());

    let mut stack = UndoStack::new();
    harness.push_entry(&mut stack, &receipt);
    assert!(matches!(
        harness.undo(&mut stack),
        UndoOutcome::Applied { .. }
    ));
    assert_eq!(harness.tags(&file), seeded, "both tags came back, in order");
}

/// "Owner/group change where privileged": an ordinary test process is not, so
/// the op must fail cleanly on every path, leave the file alone, and produce no
/// undo entry. `root` is refused by `MacPlatform` (real EPERM) and by
/// `StubPlatform` (by construction), so one assertion covers both machines.
#[test]
fn an_unprivileged_chown_fails_cleanly_and_records_nothing_to_undo() {
    let harness = Harness::new();
    let file = harness.write("owned.txt", "o");
    let before = block_on(harness.platform.file_attrs(&file)).unwrap();
    if before.owner.as_deref() == Some(STUB_PRIVILEGED_OWNER) {
        return; // running as root: the give-away would legitimately succeed
    }

    let error = harness.expect_failed(FileOp::Chown {
        paths: vec![file.clone()],
        owner: Some(STUB_PRIVILEGED_OWNER.to_string()),
        group: None,
    });
    assert!(error.contains("owned.txt"), "{error}");

    let after = block_on(harness.platform.file_attrs(&file)).unwrap();
    assert_eq!(after.owner, before.owner, "ownership was not touched");
    assert_eq!(after.group, before.group);
    assert_eq!(std::fs::read(&file).unwrap(), b"o", "nor were the contents");
}
