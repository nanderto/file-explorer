//! Job identities, events, conflicts, and receipts (ARCHITECTURE.md §6,
//! `ops/job.rs`).

use std::path::PathBuf;

use crate::entry::EntryMeta;
use crate::ops::FileOp;
use crate::tags::Tag;
use crate::vfs::TrashId;

/// Identity of one submitted job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// What kind of operation a job performs (progress UI grouping).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    Rename,
    Trash,
    Restore,
    CreateDir,
    CreateFile,
    Duplicate,
    Delete,
    Chmod,
    Chown,
    SetTags,
}

/// Summary emitted with [`JobEvent::Started`], after planning.
#[derive(Clone, Debug, PartialEq)]
pub struct JobInfo {
    pub id: JobId,
    pub kind: JobKind,
    /// Total payload bytes for copy-shaped jobs; item count otherwise.
    pub total_bytes: u64,
    /// Planned action count.
    pub total_items: u64,
}

/// A destination that already exists, parked for a user decision
/// (Explorer-style Replace / Skip / Keep both).
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub src_meta: EntryMeta,
    pub dest_meta: EntryMeta,
}

/// The user's choice for one conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Replace,
    Skip,
    KeepBoth,
}

/// A conflict decision, optionally sticky for the rest of the job
/// (ARCHITECTURE.md §6: `Resolution { Replace | Skip | KeepBoth, apply_to_all }`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub choice: ConflictChoice,
    pub apply_to_all: bool,
}

impl Resolution {
    pub fn skip() -> Self {
        Self {
            choice: ConflictChoice::Skip,
            apply_to_all: false,
        }
    }
}

/// One path's attribute value, captured **before** an attribute op changed it
/// so undo can put back exactly what was there (M6b).
///
/// Also used the other way round, as an [`crate::AttrGuard`]'s `expected`: the
/// value the job *left behind*, which must still hold for its undo to be safe.
/// One type for both because the shape is identical and a second near-identical
/// enum would be one more thing to keep in step.
#[derive(Clone, Debug, PartialEq)]
pub enum PrevAttrs {
    /// Unix permission bits (`0o7777` window), from [`crate::Vfs::mode`].
    Mode(u32),
    /// Owner and group *names*, from [`crate::FileAttrs`]. Either may be
    /// [`None`] when the platform could not resolve it — or, in a guard, when
    /// the op did not ask to change that half.
    Ownership {
        owner: Option<String>,
        group: Option<String>,
    },
    /// The whole Finder tag set, from [`crate::Platform::read_tags`]. An empty
    /// vector is a real value ("this file had no tags"), not a missing one.
    Tags(Vec<Tag>),
}

/// What a completed job actually did — everything the undo stack needs to
/// build the inverse operation ([`crate::undo::UndoEntry::from_receipt`]).
#[derive(Clone, Debug, PartialEq)]
pub struct OpReceipt {
    /// The operation as submitted (redo re-submits it).
    pub op: FileOp,
    /// Top-level paths this job created (copy destinations, new dirs/files).
    pub created: Vec<PathBuf>,
    /// `(from, to)` pairs this job moved or renamed.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// Trash tokens this job produced.
    pub trashed: Vec<TrashId>,
    /// `(token, restored_path)` pairs this job restored.
    pub restored: Vec<(TrashId, PathBuf)>,
    /// Per-path attribute values as they were **before** this job changed them
    /// ([`FileOp::Chmod`] / [`FileOp::Chown`] / [`FileOp::SetTags`]), in the
    /// order they were applied. Only paths that actually changed appear, so the
    /// inverse built from it is exact even when part of the selection failed.
    pub restored_attrs: Vec<(PathBuf, PrevAttrs)>,
    /// Paths this job could **not** change, with the reason (an EPERM chmod, a
    /// path that vanished mid-job). A non-empty `failed` alongside a non-empty
    /// `restored_attrs` is a partial success: the job still completes, so what
    /// did land stays undoable, and the UI reports the rest.
    ///
    /// Only the attribute ops populate this — every other op in the vocabulary
    /// still fails the whole job on the first error (see `docs/AS_BUILT.md`).
    pub failed: Vec<(PathBuf, String)>,
}

impl OpReceipt {
    pub(crate) fn empty(op: FileOp) -> Self {
        Self {
            op,
            created: Vec::new(),
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
            restored_attrs: Vec::new(),
            failed: Vec::new(),
        }
    }
}

/// Events on the queue's single channel, consumed only by the app's JobsModel
/// (ARCHITECTURE.md §6). Every job ends with exactly one terminal event
/// (`Completed` / `Failed` / `Cancelled`), guaranteed by an RAII tracker.
#[derive(Clone, Debug, PartialEq)]
pub enum JobEvent {
    Started {
        info: JobInfo,
    },
    Progress {
        id: JobId,
        done_bytes: u64,
        total_bytes: u64,
        current: PathBuf,
    },
    NeedsDecision {
        id: JobId,
        conflict: Conflict,
    },
    Completed {
        id: JobId,
        receipt: OpReceipt,
    },
    Failed {
        id: JobId,
        error: String,
    },
    Cancelled {
        id: JobId,
    },
}
