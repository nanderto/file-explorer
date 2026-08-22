//! Job identities, events, conflicts, and receipts (ARCHITECTURE.md §6,
//! `ops/job.rs`).

use std::path::PathBuf;

use crate::entry::EntryMeta;
use crate::ops::FileOp;
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
}

impl OpReceipt {
    pub(crate) fn empty(op: FileOp) -> Self {
        Self {
            op,
            created: Vec::new(),
            moved: Vec::new(),
            trashed: Vec::new(),
            restored: Vec::new(),
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
