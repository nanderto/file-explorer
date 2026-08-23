//! The single JobEvent → gpui bridge (ARCHITECTURE.md §2 `JobsModel`).
//!
//! A **non-render** entity that is the **sole consumer** of fs-core's
//! [`JobEvent`] channel: one held `_pump` task folds events into job rows
//! (progress popover data), queues parked conflicts for the workspace's
//! modal, pushes each completed op's inverse onto the shared undo stack
//! **exactly once** (undo/redo-submitted jobs are suppressed so the stacks
//! never feed themselves), and turns terminal events into timed toasts.
//! Views (`jobs_ui`, `Workspace`) observe/subscribe; nothing else touches
//! the channel (invariant #9).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fs_core::{
    Conflict, JobEvent, JobId, JobInfo, JobKind, JobQueue, OpReceipt, Spawner, UndoEntry, Vfs,
};
use gpui::{Context, EventEmitter, SharedString, Task};

use crate::app_state::SharedUndoStack;

/// How long a toast stays before auto-dismissing (via `Spawner::timer`, so
/// tests run it on the fake clock).
pub const TOAST_DURATION: Duration = Duration::from_secs(5);

/// What the model emits to its subscribers (ARCHITECTURE.md §2).
#[derive(Clone, Debug)]
pub enum JobsEvent {
    /// Job rows or toasts changed (progress popover / toast layer repaint).
    RowsChanged,
    /// The front-most parked conflict awaiting a user decision — the
    /// workspace opens the conflict modal for it.
    NeedsDecision { id: JobId, conflict: Conflict },
    /// A previously announced decision is moot (its job ended while parked,
    /// e.g. cancelled from the popover) — the workspace closes a stale modal.
    DecisionObsolete { id: JobId },
    /// A job finished successfully (its undo entry, if any, is already
    /// pushed).
    Completed { id: JobId, receipt: OpReceipt },
    /// A job ended in an error (a toast is already pushed for it). Per-`id`
    /// so a subscriber tracking one submitted job — e.g. the rename editor
    /// waiting to show a collision reported by the op — can react without
    /// re-deriving it from the toast text.
    Failed { id: JobId, error: String },
}

/// Whether a live job is running or parked on a conflict. Terminal jobs are
/// removed from the rows (a toast replaces them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobRowState {
    Running,
    AwaitingDecision,
}

/// One live job, as shown in the progress popover.
#[derive(Clone, Debug)]
pub struct JobRow {
    pub info: JobInfo,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub current: Option<PathBuf>,
    pub state: JobRowState,
}

impl JobRow {
    /// Completed fraction in `0.0..=1.0` (zero-total jobs read as started).
    pub fn fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.done_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
        }
    }
}

/// Toast severity (drives the accent color in `jobs_ui`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
    /// Notices (undo invalidation).
    Info,
}

/// One timed overlay row.
#[derive(Clone, Debug)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub message: SharedString,
}

/// Human label for a job kind ("Copy complete", "Move failed: …").
pub fn kind_label(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Copy => "Copy",
        JobKind::Move => "Move",
        JobKind::Rename => "Rename",
        JobKind::Trash => "Move to Trash",
        JobKind::Restore => "Restore",
        JobKind::CreateDir => "New Folder",
        JobKind::CreateFile => "New File",
        JobKind::Duplicate => "Duplicate",
        JobKind::Delete => "Delete",
    }
}

/// Progress-popover verb for a running job ("Copying …").
pub fn kind_verb(kind: JobKind) -> &'static str {
    match kind {
        JobKind::Copy => "Copying",
        JobKind::Move => "Moving",
        JobKind::Rename => "Renaming",
        JobKind::Trash => "Moving to Trash",
        JobKind::Restore => "Restoring",
        JobKind::CreateDir => "Creating folder",
        JobKind::CreateFile => "Creating file",
        JobKind::Duplicate => "Duplicating",
        JobKind::Delete => "Deleting",
    }
}

pub struct JobsModel {
    queue: Arc<JobQueue>,
    spawner: Arc<dyn Spawner>,
    rows: Vec<JobRow>,
    /// Parked conflicts in arrival order; the front is the one the modal
    /// shows.
    pending: VecDeque<(JobId, Conflict)>,
    toasts: Vec<Toast>,
    next_toast_id: u64,
    /// Jobs whose completion must **not** push an undo entry: they were
    /// submitted *by* undo/redo (`UndoStack` maintains its own stacks for
    /// those). Registered synchronously after submit — no await between
    /// `UndoStack::undo/redo` returning and registration, so on the
    /// single-threaded foreground executor the pump cannot observe the
    /// completion first.
    suppressed_undo: HashSet<JobId>,
    _toast_timers: HashMap<u64, Task<()>>,
    _pump: Task<()>,
}

impl EventEmitter<JobsEvent> for JobsModel {}

impl JobsModel {
    pub fn new(
        queue: Arc<JobQueue>,
        vfs: Arc<dyn Vfs>,
        undo: SharedUndoStack,
        spawner: Arc<dyn Spawner>,
        cx: &mut Context<Self>,
    ) -> Self {
        let rx = queue.subscribe();
        let pump = cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                // Completed ops push their inverse exactly once, before the
                // event folds into the rows (the fingerprints must be taken
                // right at completion, and `Completed` is emitted exactly
                // once per job by the queue's RAII tracker).
                if let JobEvent::Completed { id, receipt } = &event {
                    let suppressed = this
                        .update(cx, |model, _| model.suppressed_undo.remove(id))
                        .unwrap_or(true);
                    if !suppressed && let Some(entry) = UndoEntry::from_receipt(&vfs, receipt).await
                    {
                        undo.lock().await.push(entry);
                    }
                }
                if this.update(cx, |model, cx| model.apply(event, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            queue,
            spawner,
            rows: Vec::new(),
            pending: VecDeque::new(),
            toasts: Vec::new(),
            next_toast_id: 0,
            suppressed_undo: HashSet::new(),
            _toast_timers: HashMap::new(),
            _pump: pump,
        }
    }

    // ------------------------------------------------------------------
    // Observer surface
    // ------------------------------------------------------------------

    pub fn rows(&self) -> &[JobRow] {
        &self.rows
    }

    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    /// The conflict a modal should currently show, if any.
    pub fn pending_decision(&self) -> Option<&(JobId, Conflict)> {
        self.pending.front()
    }

    /// Cancel a job (progress-popover ✕ button).
    pub fn cancel_job(&self, id: JobId) {
        self.queue.cancel(id);
    }

    // ------------------------------------------------------------------
    // Commands from the workspace
    // ------------------------------------------------------------------

    /// The workspace resolved (or cancelled) the modal for `id`: drop it from
    /// the pending queue and, if another conflict is waiting, announce it.
    pub fn decision_handled(&mut self, id: JobId, cx: &mut Context<Self>) {
        self.remove_pending(id, cx, false);
        if let Some(row) = self.row_mut(id) {
            row.state = JobRowState::Running;
        }
        cx.emit(JobsEvent::RowsChanged);
        cx.notify();
    }

    /// Register undo/redo-submitted jobs whose completion must not push a
    /// fresh undo entry.
    pub fn suppress_undo_for(&mut self, jobs: &[JobId]) {
        self.suppressed_undo.extend(jobs.iter().copied());
    }

    /// Surface an undo/redo invalidation notice
    /// ("Can't undo — 'report.pdf' was modified since").
    pub fn push_undo_invalidated(&mut self, message: String, cx: &mut Context<Self>) {
        self.push_notice(message, cx);
    }

    /// Surface a plain informational notice as a timed toast — the one
    /// user-visible channel for "this command exists but cannot act", used by
    /// the pane's `SetViewColumns` handler (§8 stretch item) so an
    /// unimplemented view mode is announced rather than silently ignored.
    pub fn push_notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.push_toast(ToastKind::Info, message, cx);
        cx.emit(JobsEvent::RowsChanged);
        cx.notify();
    }

    /// Dismiss a toast (timer expiry or click). The timer task is detached
    /// rather than dropped: expiry calls this from *inside* that task, and a
    /// task must not cancel itself mid-poll (a click-detached timer fires
    /// later into a no-op).
    pub fn dismiss_toast(&mut self, id: u64, cx: &mut Context<Self>) {
        self.toasts.retain(|toast| toast.id != id);
        if let Some(timer) = self._toast_timers.remove(&id) {
            timer.detach();
        }
        cx.emit(JobsEvent::RowsChanged);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Event folding (the pump's single entry point)
    // ------------------------------------------------------------------

    fn apply(&mut self, event: JobEvent, cx: &mut Context<Self>) {
        match event {
            JobEvent::Started { info } => {
                self.rows.push(JobRow {
                    done_bytes: 0,
                    total_bytes: info.total_bytes,
                    current: None,
                    state: JobRowState::Running,
                    info,
                });
            }
            JobEvent::Progress {
                id,
                done_bytes,
                total_bytes,
                current,
            } => {
                if let Some(row) = self.row_mut(id) {
                    row.done_bytes = done_bytes;
                    row.total_bytes = total_bytes;
                    row.current = Some(current);
                }
            }
            JobEvent::NeedsDecision { id, conflict } => {
                if let Some(row) = self.row_mut(id) {
                    row.state = JobRowState::AwaitingDecision;
                }
                self.pending.push_back((id, conflict.clone()));
                if self.pending.len() == 1 {
                    cx.emit(JobsEvent::NeedsDecision { id, conflict });
                }
            }
            JobEvent::Completed { id, receipt } => {
                let kind = self.remove_row(id);
                self.remove_pending(id, cx, true);
                self.push_toast(
                    ToastKind::Success,
                    format!("{} complete", kind_label(kind)),
                    cx,
                );
                cx.emit(JobsEvent::Completed { id, receipt });
            }
            JobEvent::Failed { id, error } => {
                let kind = self.remove_row(id);
                self.remove_pending(id, cx, true);
                self.push_toast(
                    ToastKind::Error,
                    format!("{} failed: {error}", kind_label(kind)),
                    cx,
                );
                cx.emit(JobsEvent::Failed { id, error });
            }
            JobEvent::Cancelled { id } => {
                self.remove_row(id);
                self.remove_pending(id, cx, true);
            }
        }
        cx.emit(JobsEvent::RowsChanged);
        cx.notify();
    }

    fn row_mut(&mut self, id: JobId) -> Option<&mut JobRow> {
        self.rows.iter_mut().find(|row| row.info.id == id)
    }

    /// Remove a terminal job's row, returning its kind for the toast label.
    fn remove_row(&mut self, id: JobId) -> JobKind {
        let kind = self
            .rows
            .iter()
            .find(|row| row.info.id == id)
            .map(|row| row.info.kind)
            // Defensive: a job that failed before `Started` still gets a toast.
            .unwrap_or(JobKind::Copy);
        self.rows.retain(|row| row.info.id != id);
        kind
    }

    /// Drop `id`'s pending conflicts. When the front changes, announce the
    /// next conflict; `announce_obsolete` additionally tells the workspace
    /// the removed front's modal is stale (terminal-while-parked path).
    fn remove_pending(&mut self, id: JobId, cx: &mut Context<Self>, announce_obsolete: bool) {
        let was_front = self.pending.front().is_some_and(|(front, _)| *front == id);
        self.pending.retain(|(job, _)| *job != id);
        if was_front {
            if announce_obsolete {
                cx.emit(JobsEvent::DecisionObsolete { id });
            }
            if let Some((next, conflict)) = self.pending.front().cloned() {
                cx.emit(JobsEvent::NeedsDecision { id: next, conflict });
            }
        }
    }

    fn push_toast(&mut self, kind: ToastKind, message: String, cx: &mut Context<Self>) {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        self.toasts.push(Toast {
            id,
            kind,
            message: message.into(),
        });
        // Timed dismissal on the Spawner clock (fake time under tests,
        // invariant #7).
        let timer = self.spawner.timer(TOAST_DURATION);
        let task = cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |model, cx| model.dismiss_toast(id, cx))
                .ok();
        });
        self._toast_timers.insert(id, task);
    }
}

#[cfg(test)]
mod tests {
    //! §9 `jobs_model.rs` rows: JobEvent stream → JobsModel rows;
    //! Completed pushes an undo entry exactly once; undo/redo-submitted
    //! jobs never push; toasts appear and expire on the fake clock.

    use super::*;
    use crate::app_state::{FsContext, GpuiSpawner, LoggingOpener};
    use fs_core::{ConflictChoice, FakeVfs, FileOp, Resolution, UndoOutcome};
    use gpui::{Entity, TestAppContext};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn init_test(cx: &mut TestAppContext) -> (Arc<FakeVfs>, Entity<JobsModel>) {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree("/src", json!({ "a.txt": "aaa", "b.txt": "bb" }));
            vfs.insert_tree("/dest", json!({}));
            let jobs = crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
            (vfs, jobs)
        })
    }

    fn queue(cx: &mut TestAppContext) -> Arc<fs_core::JobQueue> {
        cx.update(|cx| FsContext::global(cx).queue.clone())
    }

    fn undo_stack(cx: &mut TestAppContext) -> SharedUndoStack {
        cx.update(|cx| FsContext::global(cx).undo.clone())
    }

    fn exists(vfs: &Arc<FakeVfs>, path: &str) -> bool {
        futures::executor::block_on(vfs.metadata(Path::new(path)))
            .unwrap()
            .is_some()
    }

    #[gpui::test]
    async fn job_events_fold_into_rows_and_completion_toasts(cx: &mut TestAppContext) {
        let (vfs, jobs) = init_test(cx);
        let queue = queue(cx);

        queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt"), PathBuf::from("/src/b.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        cx.run_until_parked();

        assert!(exists(&vfs, "/dest/a.txt"));
        jobs.read_with(cx, |jobs, _| {
            assert!(jobs.rows().is_empty(), "completed rows are removed");
            assert!(
                jobs.toasts()
                    .iter()
                    .any(|t| t.kind == ToastKind::Success && t.message.contains("Copy complete")),
                "completion toast pushed: {:?}",
                jobs.toasts()
            );
            assert!(jobs.pending_decision().is_none());
        });
    }

    #[gpui::test]
    async fn parked_conflicts_queue_and_rows_track_state(cx: &mut TestAppContext) {
        let (vfs, jobs) = init_test(cx);
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));
        let queue = queue(cx);

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        cx.run_until_parked();

        jobs.read_with(cx, |jobs, _| {
            let rows = jobs.rows();
            assert_eq!(rows.len(), 1, "one live job row");
            assert_eq!(rows[0].info.id, id);
            assert_eq!(rows[0].state, JobRowState::AwaitingDecision);
            let (pending_id, conflict) = jobs.pending_decision().expect("conflict pending");
            assert_eq!(*pending_id, id);
            assert_eq!(conflict.dest, PathBuf::from("/dest/a.txt"));
        });

        queue.resolve(
            id,
            Resolution {
                choice: ConflictChoice::Replace,
                apply_to_all: false,
            },
        );
        cx.update(|cx| {
            jobs.update(cx, |jobs, cx| jobs.decision_handled(id, cx));
        });
        cx.run_until_parked();
        jobs.read_with(cx, |jobs, _| {
            assert!(jobs.rows().is_empty());
            assert!(jobs.pending_decision().is_none());
        });
        assert!(exists(&vfs, "/dest/a.txt"));
    }

    #[gpui::test]
    async fn completed_pushes_undo_entry_exactly_once(cx: &mut TestAppContext) {
        let (vfs, jobs) = init_test(cx);
        let queue = queue(cx);
        let undo = undo_stack(cx);

        queue.submit(FileOp::Rename {
            from: PathBuf::from("/src/a.txt"),
            to: PathBuf::from("/src/renamed.txt"),
        });
        cx.run_until_parked();
        assert!(exists(&vfs, "/src/renamed.txt"));

        // Exactly one entry: the first undo applies, the second finds nothing
        // (the inverse job is suppressed, as the workspace's Undo handler
        // does, so its completion pushes no fresh entry).
        let vfs_dyn = cx.update(|cx| FsContext::global(cx).vfs.clone());
        let outcome = undo.lock().await.undo(&vfs_dyn, &queue).await;
        let UndoOutcome::Applied { jobs: ids } = outcome else {
            panic!("expected Applied, got {outcome:?}");
        };
        cx.update(|cx| jobs.update(cx, |jobs, _| jobs.suppress_undo_for(&ids)));
        cx.run_until_parked();
        assert!(exists(&vfs, "/src/a.txt"), "undo renamed back");
        let outcome = undo.lock().await.undo(&vfs_dyn, &queue).await;
        assert!(
            matches!(outcome, UndoOutcome::Nothing),
            "the receipt was pushed exactly once"
        );
    }

    #[gpui::test]
    async fn suppressed_jobs_do_not_push_undo_entries(cx: &mut TestAppContext) {
        let (vfs, jobs) = init_test(cx);
        let queue = queue(cx);
        let undo = undo_stack(cx);

        // Simulate an undo-submitted job: suppress before completion.
        let id = queue.submit(FileOp::Rename {
            from: PathBuf::from("/src/a.txt"),
            to: PathBuf::from("/src/z.txt"),
        });
        cx.update(|cx| {
            jobs.update(cx, |jobs, _| jobs.suppress_undo_for(&[id]));
        });
        cx.run_until_parked();
        assert!(exists(&vfs, "/src/z.txt"));

        let vfs_dyn = cx.update(|cx| FsContext::global(cx).vfs.clone());
        let outcome = undo.lock().await.undo(&vfs_dyn, &queue).await;
        assert!(
            matches!(outcome, UndoOutcome::Nothing),
            "suppressed completion pushed no entry"
        );
    }

    #[gpui::test]
    async fn failed_jobs_toast_errors_and_toasts_expire_on_the_fake_clock(cx: &mut TestAppContext) {
        let (_vfs, jobs) = init_test(cx);
        let queue = queue(cx);

        queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/nope/missing.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        cx.run_until_parked();
        jobs.read_with(cx, |jobs, _| {
            assert!(jobs.rows().is_empty());
            assert!(
                jobs.toasts()
                    .iter()
                    .any(|t| t.kind == ToastKind::Error && t.message.contains("failed")),
                "error toast pushed: {:?}",
                jobs.toasts()
            );
        });

        cx.executor().advance_clock(TOAST_DURATION);
        cx.run_until_parked();
        jobs.read_with(cx, |jobs, _| {
            assert!(jobs.toasts().is_empty(), "toast expired after its timer");
        });
    }

    #[gpui::test]
    async fn undo_invalidation_notice_becomes_an_info_toast(cx: &mut TestAppContext) {
        let (_vfs, jobs) = init_test(cx);
        cx.update(|cx| {
            jobs.update(cx, |jobs, cx| {
                jobs.push_undo_invalidated(
                    "Can't undo — 'report.pdf' was modified since".to_string(),
                    cx,
                );
            });
        });
        jobs.read_with(cx, |jobs, _| {
            assert_eq!(jobs.toasts().len(), 1);
            assert_eq!(jobs.toasts()[0].kind, ToastKind::Info);
            assert!(jobs.toasts()[0].message.contains("Can't undo"));
        });
    }
}
