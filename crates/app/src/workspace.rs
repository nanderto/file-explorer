//! The window root entity (ARCHITECTURE.md §2 `Workspace`), grown out of the
//! M0 `WorkspaceView` skeleton — same chrome (titlebar, info-panel
//! placeholder), the main pane as a real [`Pane`] entity, the root carrying
//! the `Workspace` key context, and (M2) the real [`Sidebar`] entity plus
//! hand-built resizable splitters (§8 "Resizable splitters").
//!
//! Panes live in a `Vec` from day one (len 1 for M1) so the M4 split-pane
//! toggle grows the vector instead of reshaping the tree.

use std::path::Path;

use fs_core::{Conflict, FileOp, JobId, OpReceipt, PrevAttrs, UndoOutcome};
use gpui::{
    AnyElement, App, Context, DragMoveEvent, Entity, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Subscription, Window, deferred, div, prelude::*, px,
};

use crate::actions::{
    DeletePermanently, FocusAddressBar, FocusSearch, Redo, ToggleHiddenFiles, ToggleInfoPanel,
    ToggleSplitPane, Undo,
};
use crate::app_state::FsContext;
use crate::dialogs::{ConfirmDialog, ConfirmDialogEvent, ConflictDialog, ConflictDialogEvent};
use crate::info_panel::InfoPanel;
use crate::jobs_model::{JobsEvent, JobsModel};
use crate::jobs_ui::{JobsIndicator, ToastLayer};
use crate::pane::{Pane, PaneEvent};
use crate::sidebar::{Sidebar, SidebarEvent};
use crate::theme::Theme;

/// Font used for all UI text. Pinned to a face that ships with macOS so
/// visual-test screenshots are stable across machines and CI runners.
pub const UI_FONT: &str = "Helvetica";

/// Splitter clamp bounds (§8: widths "clamped to sane min/max"). Defaults
/// match the M0/M1 fixed widths so baselines only change where the UI did.
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 220.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 160.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 400.0;
pub const INFO_PANEL_DEFAULT_WIDTH: f32 = 260.0;
pub const INFO_PANEL_MIN_WIDTH: f32 = 180.0;
pub const INFO_PANEL_MAX_WIDTH: f32 = 420.0;
/// Narrowest a pane may be squeezed to by the split splitter (M4). Below this
/// the details list's columns stop being readable at all, so the drag stops
/// rather than letting one pane be dragged out of existence — collapsing the
/// split is `cmd-shift-o`, not a gesture you can trigger by accident.
pub const PANE_MIN_WIDTH: f32 = 240.0;
/// Width of the invisible grab strip straddling each region border.
const SPLITTER_HITBOX_WIDTH: f32 = 6.0;
/// Height of the per-pane active marker painted above a split pane (M4).
const PANE_MARKER_HEIGHT: f32 = 2.0;

/// Which divider is being dragged (the `on_drag` payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitterSide {
    Sidebar,
    InfoPanel,
    /// The divider between the two panes of a split (M4). Its drag math is
    /// relative to the **pane strip**, not the whole body row, so it is
    /// handled by the strip's own `on_drag_move` (see
    /// [`Workspace::handle_pane_splitter_drag`]).
    Pane,
}

/// Clamp a dragged first-pane width so **both** panes keep at least
/// [`PANE_MIN_WIDTH`]. Pure so the edge cases (a strip too narrow to honor
/// both minimums, a degenerate zero-width strip) are unit-testable without a
/// window.
fn clamp_pane_width(width: f32, strip_width: f32) -> f32 {
    // A strip narrower than two minimums cannot satisfy both; the first pane
    // wins the minimum and the second is simply squeezed (it is still
    // scrollable, and the user's next drag can only make things better).
    let max = (strip_width - PANE_MIN_WIDTH).max(PANE_MIN_WIDTH);
    width.clamp(PANE_MIN_WIDTH, max)
}

/// Drag payload for a splitter; carries no data beyond the side.
struct DraggedSplitter {
    side: SplitterSide,
}

/// Empty drag preview: gpui owns mouse capture during the drag, but a
/// splitter renders no ghost.
struct SplitterGhost;

impl Render for SplitterGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// A pending confirmation (currently: the §0 `DeletePermanently` guard).
/// The op is held by the workspace and submitted only on `Confirmed`.
pub struct ConfirmRequest {
    pub title: SharedString,
    pub message: SharedString,
    pub confirm_label: SharedString,
    pub op: FileOp,
}

/// Which dialog `Workspace.modal` currently shows (§8 "Dialogs").
pub enum Modal {
    Confirm {
        view: Entity<ConfirmDialog>,
        op: FileOp,
    },
    Conflict {
        view: Entity<ConflictDialog>,
        job: JobId,
    },
}

/// The active modal plus what it needs to tear down cleanly: the focus to
/// restore and the dialog-event subscription (dies with the modal).
struct ModalState {
    modal: Modal,
    prev_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    theme: Theme,
    sidebar: Entity<Sidebar>,
    /// One or two panes (§2 "Dual-pane readiness without PaneGroup"): a flat
    /// `Vec`, never a recursive member tree.
    panes: Vec<gpui::Entity<Pane>>,
    /// Parallel to `panes` — index `i` is the `PaneEvent` subscription for
    /// `panes[i]`, so collapsing a split drops the closed pane's subscription
    /// with the pane instead of leaving a dead one behind.
    pane_subscriptions: Vec<Subscription>,
    /// Parallel to `panes` too: index `i` observes `panes[i]`'s `DirView`, so
    /// any notify from it (a selection change, a watcher patch, a navigation)
    /// re-points the info panel. `DirView` has no `SelectionChanged` event to
    /// subscribe to — the change is a `cx.notify()` — so this is an *observe*,
    /// and [`InfoPanel::follow`] is the cheap idempotent filter that keeps a
    /// scroll or an arriving thumbnail from restarting its debounce.
    dir_view_observations: Vec<Subscription>,
    active_pane_ix: usize,
    show_hidden: bool,
    sidebar_width: f32,
    info_panel_width: f32,
    /// Width of the **first** pane while split. `None` = an even split (both
    /// panes `flex_1`), which is what a fresh split and every collapse reset
    /// to; a splitter drag pins it.
    first_pane_width: Option<f32>,
    /// The right-hand column (M5). Workspace-level, not per-pane: it follows
    /// whichever pane is active (see [`Self::sync_info_panel`]).
    info_panel: Entity<InfoPanel>,
    show_info_panel: bool,
    jobs: Entity<JobsModel>,
    jobs_indicator: Entity<JobsIndicator>,
    toast_layer: Entity<ToastLayer>,
    modal: Option<ModalState>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(theme: Theme, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| Pane::new(theme.clone(), window, cx));
        // Events up (§2): a pane's watcher batches are the only news the
        // sidebar tree gets about external changes.
        let pane_subscription = cx.subscribe(&pane, Self::handle_pane_event);
        let workspace = cx.weak_entity();
        let sidebar = cx.new(|cx| Sidebar::new(theme.clone(), workspace, cx));
        // Events up, method calls down (§2): the sidebar reports navigation
        // and eject requests; the workspace acts on them.
        let sidebar_subscription = cx.subscribe(&sidebar, Self::handle_sidebar_event);
        // §2: the workspace observes the JobsModel for parked conflicts
        // (NeedsDecision → modal); jobs_ui observes it for progress/toasts.
        let jobs = FsContext::global(cx).jobs.clone();
        let jobs_subscription = cx.subscribe_in(&jobs, window, Self::handle_jobs_event);
        let jobs_indicator = cx.new(|cx| JobsIndicator::new(theme.clone(), jobs.clone(), cx));
        let toast_layer = cx.new(|cx| ToastLayer::new(theme.clone(), jobs.clone(), cx));
        let info_panel = cx.new(|_| InfoPanel::new(theme.clone()));
        let dir_view_observation = Self::observe_dir_view(&pane, cx);
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        let mut workspace = Self {
            focus_handle,
            theme,
            sidebar,
            panes: vec![pane],
            pane_subscriptions: vec![pane_subscription],
            dir_view_observations: vec![dir_view_observation],
            active_pane_ix: 0,
            show_hidden: false,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            info_panel_width: INFO_PANEL_DEFAULT_WIDTH,
            first_pane_width: None,
            info_panel,
            show_info_panel: true,
            jobs,
            jobs_indicator,
            toast_layer,
            modal: None,
            _subscriptions: vec![sidebar_subscription, jobs_subscription],
        };
        // The panel opens describing the pane's state, not a stale default.
        workspace.sync_info_panel(cx);
        workspace
    }

    /// The sidebar tree caches child listings of its own, so an external
    /// change a pane's watcher reported has to reach it too (§6: cached child
    /// listings must not survive a change to the folder they came from).
    fn handle_pane_event(&mut self, pane: Entity<Pane>, event: &PaneEvent, cx: &mut Context<Self>) {
        match event {
            PaneEvent::DirsChanged(dirs) => {
                let dirs = dirs.clone();
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.invalidate_children(&dirs, cx));
            }
            // Focus landed anywhere inside a pane, so that pane becomes the
            // one every workspace-level command targets (M4 dual pane).
            PaneEvent::FocusIn => {
                if let Some(ix) = self.panes.iter().position(|p| p == &pane) {
                    self.active_pane_ix = ix;
                    // "Whose selection does the info panel describe" is
                    // answered by focus, exactly as `cmd-z`'s target is.
                    self.sync_info_panel(cx);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Info panel (§0 `ToggleInfoPanel`, §1 `info_panel.rs`, M5)
    // ------------------------------------------------------------------

    /// Observe a pane's `DirView` so every notify from it re-points the info
    /// panel. Strong ref down (the pane owns the view), the subscription
    /// stored on the subscriber (§2).
    fn observe_dir_view(pane: &Entity<Pane>, cx: &mut Context<Self>) -> Subscription {
        let dir_view = pane.read(cx).dir_view().clone();
        cx.observe(&dir_view, |this, _, cx| this.sync_info_panel(cx))
    }

    /// Point the info panel at the **active** pane's selection.
    ///
    /// Cheap enough to call on every `DirView` notify: `InfoPanel::follow`
    /// compares an O(1) witness first and returns without touching the
    /// projection when nothing it describes has moved. A hidden panel is told
    /// nothing at all, so it stats nothing.
    fn sync_info_panel(&mut self, cx: &mut Context<Self>) {
        let info_panel = self.info_panel.clone();
        if !self.show_info_panel {
            info_panel.update(cx, |panel, cx| panel.clear(cx));
            return;
        }
        let dir_view = self.active_pane().read(cx).dir_view().clone();
        info_panel.update(cx, |panel, cx| panel.follow(&dir_view, cx));
    }

    /// Whether the right-hand column is showing.
    pub fn show_info_panel(&self) -> bool {
        self.show_info_panel
    }

    pub fn info_panel(&self) -> &Entity<InfoPanel> {
        &self.info_panel
    }

    fn handle_toggle_info_panel(
        &mut self,
        _: &ToggleInfoPanel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_info_panel(cx);
    }

    /// §0 "Info panel toggle". Hiding it leaves the pane strip alone — the
    /// strip is `flex_1` and the panel `flex_none`, so the panes simply gain
    /// the width, and neither the split's pinned first-pane width nor the
    /// sidebar's is touched.
    pub fn toggle_info_panel(&mut self, cx: &mut Context<Self>) {
        self.show_info_panel = !self.show_info_panel;
        // Re-describing on show is what stops a panel that was hidden through
        // a navigation from coming back showing the folder you left.
        self.sync_info_panel(cx);
        cx.notify();
    }

    fn handle_sidebar_event(
        &mut self,
        _sidebar: Entity<Sidebar>,
        event: &SidebarEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::NavigateTo(path) => {
                let path = path.clone();
                self.active_pane()
                    .clone()
                    .update(cx, |pane, cx| pane.navigate_to(&path, cx));
            }
            SidebarEvent::Eject(volume_id) => {
                // Platform::eject blocks on the OS — run it on the background
                // executor (§5: the UI thread never touches the disk/OS).
                let fs = FsContext::global(cx);
                let platform = fs.platform.clone();
                let volume_id = volume_id.clone();
                fs.spawner.spawn(Box::pin(async move {
                    if let Err(error) = platform.eject(&volume_id).await {
                        eprintln!("eject {} failed: {error:#}", volume_id.as_str());
                    }
                }));
            }
            // M6b: the Tags section filters the **active** pane, which is the
            // same rule every other workspace-level command follows (§0's
            // "whose selection" question, answered by focus).
            SidebarEvent::FilterByTag(tag) => {
                let pane = self.active_pane().clone();
                match tag {
                    Some(tag) => {
                        let tag = tag.clone();
                        pane.update(cx, |pane, cx| pane.set_tag_filter(tag, cx));
                    }
                    None => pane.update(cx, |pane, cx| pane.clear_tag_filter(cx)),
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Job spine: conflicts → modal, undo/redo, dialogs (§2, §4b, §8)
    // ------------------------------------------------------------------

    /// The paths whose Finder tags a completed job changed, so the views that
    /// paint them can drop their caches (see [`Workspace::handle_jobs_event`]).
    ///
    /// Read from the **receipt**, not from the submitted op: the receipt lists
    /// only the paths that really changed (an attribute op attempts every path
    /// and records the ones it could not do in `OpReceipt::failed`), and its
    /// `restored_attrs` covers an *undo* of a tagging as well as the tagging
    /// itself.
    fn tagged_paths(receipt: &OpReceipt) -> Vec<std::path::PathBuf> {
        receipt
            .restored_attrs
            .iter()
            .filter(|(_, prev)| matches!(prev, PrevAttrs::Tags(_)))
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn handle_jobs_event(
        &mut self,
        _jobs: &Entity<JobsModel>,
        event: &JobsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            JobsEvent::NeedsDecision { id, conflict } => {
                // A busy modal keeps priority; the pending conflict is
                // re-checked when it closes (close_modal).
                if self.modal.is_none() {
                    self.open_conflict_modal(*id, conflict.clone(), window, cx);
                }
            }
            JobsEvent::DecisionObsolete { id } => {
                // The parked job ended (cancelled from the popover, failed):
                // a modal still showing it is stale.
                if matches!(
                    &self.modal,
                    Some(ModalState { modal: Modal::Conflict { job, .. }, .. }) if job == id
                ) {
                    self.close_modal(window, cx);
                }
            }
            // M6b: an xattr write changes no directory entry and no mtime, so
            // no pane's watcher can see it. Every view that paints tags has to
            // be told, or the dots (and the info panel's Tags row) would keep
            // showing what the file was tagged with *before* the job the user
            // just ran. Undo and redo come back through here too, so the same
            // one line covers them.
            JobsEvent::Completed { receipt, .. } => {
                let changed = Self::tagged_paths(receipt);
                if !changed.is_empty() {
                    for pane in self.panes.clone() {
                        let dir_view = pane.read(cx).dir_view().clone();
                        dir_view.update(cx, |view, cx| view.invalidate_tags(&changed, cx));
                    }
                    self.info_panel
                        .update(cx, |panel, cx| panel.invalidate_tags(cx));
                    self.sync_info_panel(cx);
                }
                // The same blindness, one layer out: `chmod` moves ctime and
                // `chown` moves nothing a watcher reports, so the panel that
                // *submitted* the change would keep painting the old mode
                // (and the pane's snapshot would keep the old owner) until
                // something else happened to that folder. An attribute
                // receipt therefore re-reads the panel's subject outright —
                // no witness, no notify chain to wait on. Undo and redo come
                // back through here too.
                if receipt
                    .restored_attrs
                    .iter()
                    .any(|(_, prev)| !matches!(prev, PrevAttrs::Tags(_)))
                {
                    self.info_panel.update(cx, |panel, cx| panel.reload(cx));
                }
            }
            // `Failed` is consumed by whichever view submitted the job (e.g.
            // the rename editor); the workspace has nothing job-id-specific
            // to do beyond the toast `JobsModel` already pushed.
            JobsEvent::RowsChanged | JobsEvent::Failed { .. } => {}
        }
    }

    fn open_conflict_modal(
        &mut self,
        job: JobId,
        conflict: Conflict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| ConflictDialog::new(self.theme.clone(), conflict, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            move |this, _, event: &ConflictDialogEvent, window, cx| {
                this.handle_conflict_event(job, *event, window, cx);
            },
        );
        self.open_modal(Modal::Conflict { view, job }, subscription, window, cx);
    }

    /// Show the destructive-action confirmation (§0 `DeletePermanently`:
    /// "confirm dialog first"). The op is submitted only on `Confirmed`.
    pub fn show_confirm(
        &mut self,
        request: ConfirmRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ConfirmRequest {
            title,
            message,
            confirm_label,
            op,
        } = request;
        let view =
            cx.new(|cx| ConfirmDialog::new(self.theme.clone(), title, message, confirm_label, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this, _, event: &ConfirmDialogEvent, window, cx| {
                this.handle_confirm_event(*event, window, cx);
            },
        );
        self.open_modal(Modal::Confirm { view, op }, subscription, window, cx);
    }

    fn open_modal(
        &mut self,
        modal: Modal,
        subscription: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev_focus = window.focused(cx);
        let dialog_focus = match &modal {
            Modal::Confirm { view, .. } => view.focus_handle(cx),
            Modal::Conflict { view, .. } => view.focus_handle(cx),
        };
        self.modal = Some(ModalState {
            modal,
            prev_focus,
            _subscription: subscription,
        });
        window.focus(&dialog_focus, cx);
        cx.notify();
    }

    /// Tear the modal down: restore the pre-modal focus, then surface the
    /// next parked conflict, if any (a conflict that arrived while another
    /// modal was up).
    fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.modal.take() {
            if let Some(prev) = state.prev_focus {
                window.focus(&prev, cx);
            } else {
                window.focus(&self.focus_handle, cx);
            }
            cx.notify();
        }
        if let Some((id, conflict)) = self
            .jobs
            .read(cx)
            .pending_decision()
            .cloned()
            .filter(|_| self.modal.is_none())
        {
            self.open_conflict_modal(id, conflict, window, cx);
        }
    }

    fn handle_conflict_event(
        &mut self,
        job: JobId,
        event: ConflictDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let queue = FsContext::global(cx).queue.clone();
        match event {
            // §4b: the resolution un-parks the waiting lane.
            ConflictDialogEvent::Resolved(resolution) => queue.resolve(job, resolution),
            // §0: escape dismisses the dialog and cancels the job.
            ConflictDialogEvent::Cancelled => queue.cancel(job),
        }
        // Pop the handled decision first so close_modal's pending re-check
        // cannot re-open the same conflict.
        self.jobs
            .update(cx, |jobs, cx| jobs.decision_handled(job, cx));
        self.close_modal(window, cx);
    }

    fn handle_confirm_event(
        &mut self,
        event: ConfirmDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event == ConfirmDialogEvent::Confirmed
            && let Some(ModalState {
                modal: Modal::Confirm { op, .. },
                ..
            }) = &self.modal
        {
            let op = op.clone();
            FsContext::global(cx).queue.submit(op);
        }
        self.close_modal(window, cx);
    }

    /// The active modal, for tests and (later) chrome state.
    pub fn active_modal(&self) -> Option<&Modal> {
        self.modal.as_ref().map(|state| &state.modal)
    }

    fn handle_undo(&mut self, _: &Undo, _window: &mut Window, cx: &mut Context<Self>) {
        Self::apply_undo_redo(false, cx);
    }

    fn handle_redo(&mut self, _: &Redo, _window: &mut Window, cx: &mut Context<Self>) {
        Self::apply_undo_redo(true, cx);
    }

    /// §0 Undo/Redo (Workspace → UndoStack): validate + submit through the
    /// queue on the foreground executor. Inverse jobs are registered with the
    /// JobsModel **synchronously after submission** (no intervening await),
    /// so their completions never push fresh undo entries; an invalidated
    /// entry surfaces as a toast instead of applying against stale state.
    fn apply_undo_redo(redo: bool, cx: &mut Context<Self>) {
        let fs = FsContext::global(cx);
        let vfs = fs.vfs.clone();
        let queue = fs.queue.clone();
        let undo = fs.undo.clone();
        let jobs = fs.jobs.clone();
        // One-shot task (not a held pump): detached so a dropped workspace
        // can't cancel an undo between popping the entry and submitting it.
        cx.spawn(async move |_, cx| {
            let outcome = if redo {
                undo.lock().await.redo(&vfs, &queue).await
            } else {
                undo.lock().await.undo(&vfs, &queue).await
            };
            match outcome {
                UndoOutcome::Applied { jobs: ids } => {
                    jobs.update(cx, |jobs, _| jobs.suppress_undo_for(&ids));
                }
                UndoOutcome::Invalidated { reason, .. } => {
                    let verb = if redo { "redo" } else { "undo" };
                    jobs.update(cx, |jobs, cx| {
                        jobs.push_undo_invalidated(format!("Can't {verb} — {reason}"), cx);
                    });
                }
                UndoOutcome::Nothing => {}
            }
        })
        .detach();
    }

    /// The scrim + centered dialog, painted over everything (§8 "Dialogs":
    /// `deferred` overlay + scrim).
    fn render_modal_overlay(&self) -> Option<impl IntoElement> {
        let state = self.modal.as_ref()?;
        let dialog: AnyElement = match &state.modal {
            Modal::Confirm { view, .. } => view.clone().into_any_element(),
            Modal::Conflict { view, .. } => view.clone().into_any_element(),
        };
        let theme = &self.theme;
        Some(
            deferred(
                div()
                    .id("modal-scrim")
                    .absolute()
                    .size_full()
                    .occlude()
                    .bg(theme.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(dialog),
            )
            .with_priority(100),
        )
    }

    pub fn active_pane(&self) -> &gpui::Entity<Pane> {
        &self.panes[self.active_pane_ix]
    }

    pub fn panes(&self) -> &[gpui::Entity<Pane>] {
        &self.panes
    }

    pub fn sidebar(&self) -> &Entity<Sidebar> {
        &self.sidebar
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    // ------------------------------------------------------------------
    // Dual pane (§0 `ToggleSplitPane`, §2 "Dual-pane readiness")
    // ------------------------------------------------------------------

    /// Whether the second pane is open.
    pub fn is_split(&self) -> bool {
        self.panes.len() > 1
    }

    /// Index of the pane every workspace-level command targets. Set by
    /// `PaneEvent::FocusIn`, so any click inside a pane retargets them.
    pub fn active_pane_ix(&self) -> usize {
        self.active_pane_ix
    }

    fn handle_toggle_split_pane(
        &mut self,
        _: &ToggleSplitPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_split_pane(window, cx);
    }

    /// §0 "Split-pane toggle": one pane becomes two, two become one.
    pub fn toggle_split_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_split() {
            self.collapse_split(window, cx);
        } else {
            self.split_pane(window, cx);
        }
    }

    /// Open the second pane, showing the active pane's directory.
    ///
    /// The new pane is a **fully independent** `Pane` entity: its own history
    /// (empty — the split is not a fork of where you have been), sort,
    /// selection, address bar and status line. Two things are seeded rather
    /// than defaulted:
    ///
    /// * the **directory**, copied from the active pane, because a split whose
    ///   new half showed "No folder open" would make the user re-navigate to
    ///   the place they just split from; and
    /// * the **view mode**, set to the *complement* of the active pane's, per
    ///   plan §2, whose blueprint screenshot is a details list beside an icon
    ///   grid. One `cmd-1`/`cmd-2` undoes it if that is not wanted.
    ///
    /// `show_hidden` is workspace-global (§0 `ToggleHiddenFiles` fans out), so
    /// the new pane adopts the current value instead of resetting it.
    fn split_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (path, view_mode) = {
            let active = self.active_pane().read(cx);
            (
                active.path().map(Path::to_path_buf),
                active.view_mode().complement(),
            )
        };
        let show_hidden = self.show_hidden;
        let theme = self.theme.clone();
        let pane = cx.new(|cx| Pane::new(theme, window, cx));
        let subscription = cx.subscribe(&pane, Self::handle_pane_event);
        pane.update(cx, |new_pane, cx| {
            new_pane.set_show_hidden(show_hidden, cx);
            new_pane.set_view_mode(view_mode, cx);
            if let Some(path) = &path {
                new_pane.navigate_to(path, cx);
            }
        });
        let observation = Self::observe_dir_view(&pane, cx);
        self.panes.push(pane.clone());
        self.pane_subscriptions.push(subscription);
        self.dir_view_observations.push(observation);
        // The new pane is the one you are working in, so it takes focus — and
        // focus is what makes it active (`PaneEvent::FocusIn`); the index is
        // set here too so the state is right even without a focus round trip.
        self.active_pane_ix = self.panes.len() - 1;
        self.first_pane_width = None;
        let handle = pane.focus_handle(cx);
        window.focus(&handle, cx);
        // The new pane is active from this line on, so the panel follows it
        // even before the focus round trip delivers `PaneEvent::FocusIn`.
        self.sync_info_panel(cx);
        cx.notify();
    }

    /// Close the split. **The active pane survives** — collapsing while you
    /// work in the right-hand pane must not throw away the directory you are
    /// looking at. The closed pane's state (its history, selection, view mode
    /// and scroll position) goes away with the entity: nothing is stashed for
    /// a later re-split, because a resurrected pane pointing at a directory
    /// that has since changed is worse than a fresh one.
    fn collapse_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_split() {
            return;
        }
        let survivor = self.panes[self.active_pane_ix].clone();
        let subscription = self.pane_subscriptions.remove(self.active_pane_ix);
        let observation = self.dir_view_observations.remove(self.active_pane_ix);
        // Dropping the other handle drops the pane: its watch registration,
        // load tasks and watch pump all die with it (§6) — and with the
        // observation removed alongside, so does the info panel's interest in
        // a selection nobody can see any more.
        self.panes = vec![survivor.clone()];
        self.pane_subscriptions = vec![subscription];
        self.dir_view_observations = vec![observation];
        self.active_pane_ix = 0;
        self.first_pane_width = None;
        let handle = survivor.focus_handle(cx);
        window.focus(&handle, cx);
        self.sync_info_panel(cx);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Resizable splitters (§8: drag adjusts the shared widths, clamped)
    // ------------------------------------------------------------------

    pub fn sidebar_width(&self) -> f32 {
        self.sidebar_width
    }

    pub fn info_panel_width(&self) -> f32 {
        self.info_panel_width
    }

    pub fn set_sidebar_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        if width != self.sidebar_width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// Pinned width of the first pane while split, or `None` for an even
    /// split.
    pub fn first_pane_width(&self) -> Option<f32> {
        self.first_pane_width
    }

    /// Pin the first pane's width (M4 split splitter). Clamped to
    /// [`PANE_MIN_WIDTH`]; the *upper* bound depends on the strip's painted
    /// width, so the drag handler applies [`clamp_pane_width`] first.
    pub fn set_first_pane_width(&mut self, width: f32, cx: &mut Context<Self>) {
        // A non-finite width would poison the layout for the rest of the
        // session (and `f32::clamp` propagates NaN rather than rejecting it).
        if !width.is_finite() {
            return;
        }
        let width = width.max(PANE_MIN_WIDTH);
        if self.first_pane_width != Some(width) {
            self.first_pane_width = Some(width);
            cx.notify();
        }
    }

    pub fn set_info_panel_width(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(INFO_PANEL_MIN_WIDTH, INFO_PANEL_MAX_WIDTH);
        if width != self.info_panel_width {
            self.info_panel_width = width;
            cx.notify();
        }
    }

    /// Body-wide drag handler: while a splitter drags, the mouse position
    /// (relative to the body row's bounds) becomes the new region width.
    fn handle_splitter_drag(
        &mut self,
        event: &DragMoveEvent<DraggedSplitter>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.drag(cx).side {
            SplitterSide::Sidebar => {
                let width = f32::from(event.event.position.x - event.bounds.left());
                self.set_sidebar_width(width, cx);
            }
            SplitterSide::InfoPanel => {
                let width = f32::from(event.bounds.right() - event.event.position.x);
                self.set_info_panel_width(width, cx);
            }
            // Measured against the pane strip, whose bounds this handler does
            // not have — `handle_pane_splitter_drag` owns it. Both handlers
            // run for every move (gpui's `on_drag_move` listeners are not
            // hover-gated), so each must ignore the other's side or they would
            // fight over the same width with different origins.
            SplitterSide::Pane => {}
        }
    }

    /// The split splitter's drag math, on the pane strip's own bounds: the
    /// pointer's offset into the strip *is* the first pane's width.
    fn handle_pane_splitter_drag(
        &mut self,
        event: &DragMoveEvent<DraggedSplitter>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.drag(cx).side != SplitterSide::Pane {
            return;
        }
        let width = f32::from(event.event.position.x - event.bounds.left());
        let strip = f32::from(event.bounds.size.width);
        self.set_first_pane_width(clamp_pane_width(width, strip), cx);
    }

    /// The invisible grab strip straddling a region border (§8 hand-built
    /// splitters): a stateful div whose `on_drag` starts the resize; the body
    /// row's `on_drag_move` does the math.
    fn splitter_handle(&self, side: SplitterSide) -> impl IntoElement {
        let theme = self.theme.clone();
        let name = match side {
            SplitterSide::Sidebar => "sidebar-splitter",
            SplitterSide::InfoPanel => "info-panel-splitter",
            SplitterSide::Pane => "pane-splitter",
        };
        let handle = div()
            .id(name)
            // So a test can assert *where* the grab strip was painted: a
            // splitter pushed outside the strip it divides is undraggable, and
            // that is invisible to any state assertion.
            .debug_selector(move || name.to_string())
            .absolute()
            .top_0()
            .h_full()
            .w(px(SPLITTER_HITBOX_WIDTH))
            .cursor_col_resize()
            .occlude()
            .hover(|s| s.bg(theme.accent.opacity(0.5)))
            .on_drag(DraggedSplitter { side }, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| SplitterGhost)
            });
        match side {
            // Both straddle the *right* edge of the region they follow.
            SplitterSide::Sidebar | SplitterSide::Pane => {
                handle.right(px(-SPLITTER_HITBOX_WIDTH / 2.0))
            }
            SplitterSide::InfoPanel => handle.left(px(-SPLITTER_HITBOX_WIDTH / 2.0)),
        }
    }

    /// The pane strip: one pane (`flex_1`, exactly the M1–M3 layout) or two
    /// with a draggable divider between them.
    ///
    /// While split, each pane wears a 2px marker above it — the active pane's
    /// in the theme accent — because "which pane does `cmd-z` act on" must be
    /// answerable by looking, and a focus ring inside a pane is invisible when
    /// focus sits on a status line or a breadcrumb.
    fn render_pane_strip(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let split = self.is_split();
        let mut strip = div()
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .on_drag_move(cx.listener(Self::handle_pane_splitter_drag));
        if !split {
            return strip.children(self.panes.iter().cloned());
        }
        for (ix, pane) in self.panes.iter().enumerate() {
            let first = ix == 0;
            let active = ix == self.active_pane_ix;
            // Both panes carry [`PANE_MIN_WIDTH`] as a floor at *layout* time,
            // not only at drag time. A pinned first-pane width is a width the
            // strip may since have stopped being able to honor — widen the
            // sidebar and the info panel, or make the window narrower, and the
            // stored value outgrows the strip. With the first pane
            // `flex_none` (grow 0, **shrink 0**) and the second `min_w(0)`,
            // that overflowed the strip and squeezed the second pane's whole
            // content area — breadcrumb, rows and the §3 free-space status
            // line — to zero pixels, with the splitter that caused it parked
            // outside the strip where no drag can reach it. Making the pinned
            // pane shrinkable and giving both a real minimum lets flexbox
            // degrade the pin gracefully instead: the second pane freezes at
            // its minimum and the first gives up the difference.
            let mut wrapper = div().relative().flex().flex_col().min_w(px(PANE_MIN_WIDTH));
            wrapper = match self.first_pane_width.filter(|_| first) {
                Some(width) => wrapper.w(px(width)).flex_grow_0().flex_shrink_1(),
                None => wrapper.flex_1(),
            };
            if first {
                wrapper = wrapper.border_r_1().border_color(theme.border);
            }
            wrapper = wrapper
                .child(
                    div()
                        .flex_none()
                        .h(px(PANE_MARKER_HEIGHT))
                        .w_full()
                        .bg(if active { theme.accent } else { theme.border }),
                )
                .child(pane.clone());
            if first {
                wrapper = wrapper.child(self.splitter_handle(SplitterSide::Pane));
            }
            strip = strip.child(wrapper);
        }
        strip
    }

    /// The §0 toolbar affordance for `ToggleSplitPane`, in the titlebar beside
    /// the jobs indicator (the workspace's own chrome — the split is not a
    /// per-pane control). It **dispatches the boxed action** the keymap binds,
    /// so the toggle logic exists exactly once (§0), and it deliberately does
    /// *not* take focus first: whichever pane is active stays active, and the
    /// new pane inherits that pane's directory.
    fn render_split_toggle(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let active = self.is_split();
        div()
            .id("split-pane-toggle")
            .debug_selector(|| "split-pane-toggle".to_string())
            .flex()
            .items_center()
            .justify_center()
            .w(px(22.0))
            .h(px(20.0))
            .rounded(px(3.0))
            .text_size(px(12.0))
            .cursor_pointer()
            .when(active, |el| el.bg(theme.accent.opacity(0.30)))
            .text_color(if active { theme.text } else { theme.muted })
            .hover(|s| s.bg(theme.accent.opacity(0.15)))
            .on_click(cx.listener(|_, _, window: &mut Window, cx| {
                window.dispatch_action(Box::new(ToggleSplitPane), cx);
            }))
            .child(SharedString::new_static("◫"))
    }

    /// The §0 toolbar affordance for `ToggleInfoPanel`, beside the split
    /// toggle. Same shape and same rule: it dispatches the boxed action the
    /// keymap binds, so the toggle logic exists exactly once (§0), and it does
    /// not take focus, so the active pane the panel follows is unchanged by
    /// the click that reveals it.
    fn render_info_panel_toggle(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let active = self.show_info_panel;
        div()
            .id("info-panel-toggle")
            .debug_selector(|| "info-panel-toggle".to_string())
            .flex()
            .items_center()
            .justify_center()
            .w(px(22.0))
            .h(px(20.0))
            .rounded(px(3.0))
            .text_size(px(12.0))
            .cursor_pointer()
            .when(active, |el| el.bg(theme.accent.opacity(0.30)))
            .text_color(if active { theme.text } else { theme.muted })
            .hover(|s| s.bg(theme.accent.opacity(0.15)))
            .on_click(cx.listener(|_, _, window: &mut Window, cx| {
                window.dispatch_action(Box::new(ToggleInfoPanel), cx);
            }))
            .child(SharedString::new_static("ⓘ"))
    }

    /// The right-hand column: the [`InfoPanel`] entity plus its splitter,
    /// rendered only while the panel is showing.
    fn render_info_panel(&self) -> Option<impl IntoElement + use<>> {
        if !self.show_info_panel {
            return None;
        }
        let theme = self.theme.clone();
        Some(
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_none()
                .w(px(self.info_panel_width))
                .bg(theme.panel)
                .border_l_1()
                .border_color(theme.border)
                .child(self.info_panel.clone())
                .child(self.splitter_handle(SplitterSide::InfoPanel)),
        )
    }

    fn handle_focus_address_bar(
        &mut self,
        _: &FocusAddressBar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane()
            .clone()
            .update(cx, |pane, cx| pane.focus_address_bar(window, cx));
    }

    /// §0 `FocusSearch` (`cmd-f`, M6a). Same shape as `cmd-l`: bound in the
    /// `Workspace` context because the field belongs to whichever pane is
    /// active, and forwarded there.
    fn handle_focus_search(
        &mut self,
        _: &FocusSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_pane()
            .clone()
            .update(cx, |pane, cx| pane.focus_search(window, cx));
    }

    /// §0 `DeletePermanently` ("Bypass trash (confirm dialog first)"), bound
    /// in the `DirView` context so `!renaming` guards it but handled here,
    /// because the workspace owns the modal. Reached by `shift-delete` and by
    /// the row context menu's **Delete Permanently** — one handler for both.
    fn handle_delete_permanently(
        &mut self,
        _: &DeletePermanently,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Root-most paths only: deleting a folder already takes its contents.
        let paths = self
            .active_pane()
            .read(cx)
            .dir_view()
            .read(cx)
            .selection()
            .selected_paths_rootmost();
        if paths.is_empty() {
            return;
        }
        let message = match paths.as_slice() {
            [only] => format!(
                "\u{201c}{}\u{201d} will be deleted immediately. This can't be undone.",
                only.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| only.display().to_string())
            ),
            many => format!(
                "{} items will be deleted immediately. This can't be undone.",
                many.len()
            ),
        };
        self.show_confirm(
            ConfirmRequest {
                title: "Delete Permanently".into(),
                message: message.into(),
                confirm_label: "Delete".into(),
                op: FileOp::Delete { paths },
            },
            window,
            cx,
        );
    }

    fn handle_toggle_hidden_files(
        &mut self,
        _: &ToggleHiddenFiles,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_hidden = !self.show_hidden;
        let show_hidden = self.show_hidden;
        for pane in self.panes.clone() {
            pane.update(cx, |pane, cx| pane.set_show_hidden(show_hidden, cx));
        }
        cx.notify();
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        div()
            .track_focus(&self.focus_handle)
            .key_context("Workspace")
            .on_action(cx.listener(Self::handle_focus_address_bar))
            .on_action(cx.listener(Self::handle_focus_search))
            .on_action(cx.listener(Self::handle_toggle_hidden_files))
            .on_action(cx.listener(Self::handle_toggle_split_pane))
            .on_action(cx.listener(Self::handle_toggle_info_panel))
            .on_action(cx.listener(Self::handle_delete_permanently))
            .on_action(cx.listener(Self::handle_undo))
            .on_action(cx.listener(Self::handle_redo))
            .flex()
            .flex_col()
            .size_full()
            .font_family(UI_FONT)
            .bg(theme.surface)
            .text_color(theme.text)
            // Titlebar (with the jobs indicator, visible only while jobs run)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(40.0))
                    .px(px(80.0))
                    .bg(theme.titlebar)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(13.0))
                    .child("file-explorer")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(self.render_split_toggle(cx))
                            .child(self.render_info_panel_toggle(cx))
                            .child(self.jobs_indicator.clone()),
                    ),
            )
            // Body: sidebar | pane(s) | info panel, separated by splitters
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .on_drag_move(cx.listener(Self::handle_splitter_drag))
                    // Sidebar (real entity since M2), resizable
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(px(self.sidebar_width))
                            .bg(theme.sidebar)
                            .border_r_1()
                            .border_color(theme.border)
                            .child(self.sidebar.clone())
                            .child(self.splitter_handle(SplitterSide::Sidebar)),
                    )
                    // Pane strip: one pane, or two with a divider (M4)
                    .child(self.render_pane_strip(cx))
                    // Info panel (M5), resizable, hidden by `cmd-shift-i`
                    .children(self.render_info_panel()),
            )
            // Toast overlay (renders nothing while empty)
            .child(self.toast_layer.clone())
            // Modal overlay + scrim (§8 "Dialogs")
            .children(self.render_modal_overlay())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{FsContext, GpuiSpawner, LoggingOpener};
    use crate::dir_view::DirView;
    use crate::pane::{AddressBarMode, ViewMode};
    use fs_core::{FakeVfs, Spawner, Vfs as _};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({
                    "a.txt": "a",
                    ".hidden": "h",
                }),
            );
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
            crate::settings::init_with_path(
                cx,
                std::path::PathBuf::from("/config/file-explorer/settings.json"),
            );
            vfs
        })
    }

    fn build_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|window, cx| Workspace::new(Theme::dark(), window, cx))
    }

    #[gpui::test]
    fn workspace_owns_one_focused_pane(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 1);
            assert!(!workspace.show_hidden());
        });
    }

    // §8 "Resizable splitters": drag math funnels through the width setters,
    // which clamp to the sane min/max bounds.
    #[gpui::test]
    fn splitter_widths_clamp_to_bounds(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);

        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.sidebar_width(), SIDEBAR_DEFAULT_WIDTH);
            assert_eq!(workspace.info_panel_width(), INFO_PANEL_DEFAULT_WIDTH);
        });

        workspace.update(cx, |workspace, cx| {
            workspace.set_sidebar_width(10.0, cx);
            workspace.set_info_panel_width(10_000.0, cx);
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.sidebar_width(), SIDEBAR_MIN_WIDTH);
            assert_eq!(workspace.info_panel_width(), INFO_PANEL_MAX_WIDTH);
        });

        workspace.update(cx, |workspace, cx| {
            workspace.set_sidebar_width(10_000.0, cx);
            workspace.set_info_panel_width(10.0, cx);
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.sidebar_width(), SIDEBAR_MAX_WIDTH);
            assert_eq!(workspace.info_panel_width(), INFO_PANEL_MIN_WIDTH);
        });

        // In-range values apply unclamped.
        workspace.update(cx, |workspace, cx| {
            workspace.set_sidebar_width(300.0, cx);
            workspace.set_info_panel_width(200.0, cx);
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.sidebar_width(), 300.0);
            assert_eq!(workspace.info_panel_width(), 200.0);
        });
    }

    // Keymap dispatch guard for the `Workspace` context (§9, M6a): cmd-f must
    // reach handle_focus_search and land focus in the **active** pane's search
    // field — the same tracked-focus-handle path as cmd-l below.
    #[gpui::test]
    fn cmd_f_focuses_the_active_panes_search_field(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);

        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();

        let field = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .search_bar()
                .read(cx)
                .focus_handle(cx)
        });
        let focused = cx.update(|window, cx| window.focused(cx));
        assert_eq!(focused, Some(field), "cmd-f focused the search field");

        // Split, then cmd-f again: the *new* active pane's field takes focus,
        // never pane 0's.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_split_pane(window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let handle = workspace.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();
        let second = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_pane()
                .read(cx)
                .search_bar()
                .read(cx)
                .focus_handle(cx)
        });
        let focused = cx.update(|window, cx| window.focused(cx));
        assert_eq!(
            focused,
            Some(second),
            "the active pane's field, not pane 0's"
        );
    }

    // Keymap dispatch guard for the `Workspace` context (§9): cmd-l must
    // reach handle_focus_address_bar via the tracked focus handle.
    #[gpui::test]
    fn cmd_l_switches_active_pane_address_bar_to_editing(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);

        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).address_bar_mode(),
                AddressBarMode::Breadcrumb
            );
        });

        cx.simulate_keystrokes("cmd-l");
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).address_bar_mode(),
                AddressBarMode::Editing
            );
        });
    }

    // ------------------------------------------------------------------
    // M3 job spine (§9 "jobs_model.rs / dialogs" rows)
    // ------------------------------------------------------------------

    fn contents(vfs: &Arc<FakeVfs>, path: &str) -> Vec<u8> {
        futures::executor::block_on(vfs.load(Path::new(path))).unwrap()
    }

    fn exists(vfs: &Arc<FakeVfs>, path: &str) -> bool {
        futures::executor::block_on(vfs.metadata(Path::new(path)))
            .unwrap()
            .is_some()
    }

    fn queue_of(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> std::sync::Arc<fs_core::JobQueue> {
        workspace.read_with(cx, |_, cx| FsContext::global(cx).queue.clone())
    }

    fn jobs_of(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<crate::jobs_model::JobsModel> {
        workspace.read_with(cx, |_, cx| FsContext::global(cx).jobs.clone())
    }

    // §4b + §9: a parked NeedsDecision opens the workspace's conflict modal,
    // and the dialog's `k` binding reaches `queue.resolve` (keep both).
    #[gpui::test]
    fn needs_decision_opens_modal_and_keep_both_key_resolves(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));
        let (workspace, cx) = build_workspace(cx);
        let queue = queue_of(&workspace, cx);

        queue.submit(fs_core::FileOp::Copy {
            sources: vec![std::path::PathBuf::from("/src/a.txt")],
            dest_dir: std::path::PathBuf::from("/dest"),
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(
                matches!(workspace.active_modal(), Some(Modal::Conflict { .. })),
                "NeedsDecision must open the conflict modal"
            );
        });

        // The modal focused its dialog on open; `k` resolves as Keep both.
        cx.simulate_keystrokes("k");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active_modal().is_none(), "modal closed");
        });
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"old");
        assert_eq!(contents(&vfs, "/dest/a copy.txt"), b"new");
    }

    // §0: `a` toggles Apply-to-all, `r` replaces — one prompt for two
    // conflicts, and no second modal appears.
    #[gpui::test]
    fn apply_to_all_replace_resolves_every_conflict_with_one_prompt(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        vfs.insert_tree("/src", json!({ "a.txt": "new a", "b.txt": "new b" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old a", "b.txt": "old b" }));
        let (workspace, cx) = build_workspace(cx);
        let queue = queue_of(&workspace, cx);

        queue.submit(fs_core::FileOp::Copy {
            sources: vec![
                std::path::PathBuf::from("/src/a.txt"),
                std::path::PathBuf::from("/src/b.txt"),
            ],
            dest_dir: std::path::PathBuf::from("/dest"),
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("a r");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal().is_none(), "no second prompt");
            assert!(
                FsContext::global(cx)
                    .jobs
                    .read(cx)
                    .pending_decision()
                    .is_none()
            );
        });
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"new a");
        assert_eq!(contents(&vfs, "/dest/b.txt"), b"new b");
    }

    // §0: escape dismisses the dialog AND cancels the job.
    #[gpui::test]
    fn escape_dismisses_the_conflict_dialog_and_cancels_the_job(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));
        let (workspace, cx) = build_workspace(cx);
        let queue = queue_of(&workspace, cx);

        queue.submit(fs_core::FileOp::Copy {
            sources: vec![std::path::PathBuf::from("/src/a.txt")],
            dest_dir: std::path::PathBuf::from("/dest"),
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.active_modal().is_none());
            let jobs = FsContext::global(cx).jobs.read(cx);
            assert!(jobs.rows().is_empty(), "cancelled job removed");
            assert!(jobs.pending_decision().is_none());
        });
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"old", "nothing written");
    }

    // A job cancelled from the popover while its conflict modal is up makes
    // the decision obsolete: the stale modal closes by itself.
    #[gpui::test]
    fn cancelling_a_parked_job_closes_its_stale_modal(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));
        let (workspace, cx) = build_workspace(cx);
        let queue = queue_of(&workspace, cx);
        let jobs = jobs_of(&workspace, cx);

        queue.submit(fs_core::FileOp::Copy {
            sources: vec![std::path::PathBuf::from("/src/a.txt")],
            dest_dir: std::path::PathBuf::from("/dest"),
        });
        cx.run_until_parked();
        let id = jobs.read_with(cx, |jobs, _| jobs.pending_decision().expect("parked").0);
        workspace.read_with(cx, |workspace, _| {
            assert!(matches!(
                workspace.active_modal(),
                Some(Modal::Conflict { .. })
            ));
        });

        jobs.read_with(cx, |jobs, _| jobs.cancel_job(id));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.active_modal().is_none(),
                "stale conflict modal closed on DecisionObsolete"
            );
        });
    }

    // §8 ConfirmDialog: enter submits the held op (delete permanently),
    // escape leaves the world untouched. Dispatch guard for the
    // `ConfirmDialog` key context on the real entity.
    #[gpui::test]
    fn confirm_dialog_enter_submits_and_escape_aborts(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);

        let request = || ConfirmRequest {
            title: "Delete Permanently".into(),
            message: "\u{201c}a.txt\u{201d} will be deleted immediately.".into(),
            confirm_label: "Delete".into(),
            op: fs_core::FileOp::Delete {
                paths: vec![std::path::PathBuf::from("/root/a.txt")],
            },
        };

        // Escape first: nothing happens.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.show_confirm(request(), window, cx)
            });
        });
        workspace.read_with(cx, |workspace, _| {
            assert!(matches!(
                workspace.active_modal(),
                Some(Modal::Confirm { .. })
            ));
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active_modal().is_none());
        });
        assert!(exists(&vfs, "/root/a.txt"), "escape never submits");

        // Enter: the held op is submitted.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.show_confirm(request(), window, cx)
            });
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active_modal().is_none());
        });
        assert!(!exists(&vfs, "/root/a.txt"), "enter submitted the delete");
    }

    // §0 Undo/Redo through the Workspace context: cmd-z rolls a completed
    // rename back, cmd-shift-z re-applies it — and the inverse jobs push no
    // fresh undo entries (the stacks never feed themselves).
    #[gpui::test]
    fn cmd_z_undoes_and_cmd_shift_z_redoes_a_rename(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let queue = queue_of(&workspace, cx);

        queue.submit(fs_core::FileOp::Rename {
            from: std::path::PathBuf::from("/root/a.txt"),
            to: std::path::PathBuf::from("/root/b.txt"),
        });
        cx.run_until_parked();
        assert!(exists(&vfs, "/root/b.txt"));

        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();
        assert!(exists(&vfs, "/root/a.txt"), "undo renamed back");
        assert!(!exists(&vfs, "/root/b.txt"));

        cx.simulate_keystrokes("cmd-shift-z");
        cx.run_until_parked();
        assert!(exists(&vfs, "/root/b.txt"), "redo re-applied the rename");
        assert!(!exists(&vfs, "/root/a.txt"));

        // One more undo works (the redo re-armed it); then the stack is
        // empty — the inverse jobs themselves pushed nothing.
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();
        assert!(exists(&vfs, "/root/a.txt"));
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();
        assert!(
            exists(&vfs, "/root/a.txt"),
            "empty stack: second undo is a no-op"
        );
    }

    // §6 undo invalidation: a fingerprint mismatch surfaces as a toast and
    // never applies against stale state.
    #[gpui::test]
    fn invalidated_undo_surfaces_a_toast_instead_of_applying(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let queue = queue_of(&workspace, cx);
        let jobs = jobs_of(&workspace, cx);

        queue.submit(fs_core::FileOp::Rename {
            from: std::path::PathBuf::from("/root/a.txt"),
            to: std::path::PathBuf::from("/root/b.txt"),
        });
        cx.run_until_parked();

        // The world changes underneath the entry.
        vfs.insert_file("/root/b.txt", 99);

        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();

        assert!(exists(&vfs, "/root/b.txt"), "stale undo never applied");
        jobs.read_with(cx, |jobs, _| {
            assert!(
                jobs.toasts()
                    .iter()
                    .any(|t| t.message.contains("Can't undo")),
                "invalidation toast shown: {:?}",
                jobs.toasts()
            );
        });
    }

    // Keymap dispatch guard for `cmd-shift-.` in the `Workspace` context.
    #[gpui::test]
    fn toggle_hidden_files_fans_out_to_panes(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| assert_eq!(pane.item_count(), 1));

        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-shift-.");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, _| assert!(workspace.show_hidden()));
        pane.read_with(cx, |pane, _| {
            assert!(pane.show_hidden());
            assert_eq!(pane.item_count(), 2, "hidden file now listed");
        });

        cx.simulate_keystrokes("cmd-shift-.");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.item_count(), 1, "toggled back off");
        });
    }

    // §0 `DeletePermanently` end to end: bound in the `DirView` context (so
    // `!renaming` guards it) and handled here, behind the ConfirmDialog. Both
    // `shift-delete` and the row context menu's "Delete Permanently" arrive as
    // this one action, so this is the whole path for both.
    #[gpui::test]
    fn shift_delete_confirms_first_then_deletes(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx);
        });
        cx.update(|window, cx| {
            let handle = dir_view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });

        cx.simulate_keystrokes("shift-delete");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(
                matches!(workspace.active_modal(), Some(Modal::Confirm { .. })),
                "delete-permanently must ask first — it is not undoable"
            );
        });
        assert!(exists(&vfs, "/root/a.txt"), "nothing submitted yet");

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active_modal().is_none());
        });
        assert!(!exists(&vfs, "/root/a.txt"), "confirmed, so it is gone");
    }

    // ------------------------------------------------------------------
    // M4 dual pane (§0 `ToggleSplitPane`, §2 "Dual-pane readiness")
    // ------------------------------------------------------------------

    /// Focus the workspace root, where the `Workspace`-context bindings live.
    fn focus_workspace(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
    }

    /// Focus a pane the way a click into it does (gpui focuses a descendant;
    /// `on_focus_in` fires for the whole subtree either way).
    ///
    /// **The window must be active**: gpui zeroes both focus paths of an
    /// inactive window, so no `focus_in`/`focus_out` listener fires there —
    /// and a test window starts inactive. Activating it is not test
    /// scaffolding around the feature, it is the state the app actually runs
    /// in (a click that focuses a pane also activates the window).
    fn focus_pane(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.activate_window();
            let handle = pane.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();
    }

    fn open_split_workspace(
        cx: &mut TestAppContext,
    ) -> (Arc<FakeVfs>, Entity<Workspace>, &mut VisualTestContext) {
        let vfs = init_test(cx);
        vfs.insert_tree("/root", json!({ "beta": { "kept.txt": "k" } }));
        vfs.insert_tree("/other", json!({ "o.txt": "o" }));
        let (workspace, cx) = build_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.toggle_split_pane(window, cx));
        });
        cx.run_until_parked();
        (vfs, workspace, cx)
    }

    /// The painted content width of each pane's list, in pane order — the
    /// thing that goes to zero when a pinned splitter overflows the strip.
    fn pane_list_widths(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<f32> {
        let panes = workspace.read_with(cx, |workspace, _| workspace.panes().to_vec());
        panes
            .iter()
            .map(|pane| {
                let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
                dir_view.read_with(cx, |view, _| {
                    f32::from(crate::marquee::list_viewport(view).size.width)
                })
            })
            .collect()
    }

    #[gpui::test]
    fn the_titlebar_split_toggle_clicks_the_split_open_and_shut(cx: &mut TestAppContext) {
        // The button dispatches the boxed `ToggleSplitPane` without focusing
        // anything first (deliberately — whichever pane is active stays
        // active), so it depends entirely on the focused element's dispatch
        // path containing the Workspace node. Nothing clicked it, so a change
        // to focus handling or key context would have broken it with no
        // compile error and no failing test.
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        let click_toggle = |cx: &mut VisualTestContext| {
            let at = cx
                .debug_bounds("split-pane-toggle")
                .expect("the titlebar toggle paints")
                .center();
            cx.simulate_click(at, gpui::Modifiers::none());
            cx.run_until_parked();
        };

        // Focus inside a pane, which is where it is in practice.
        focus_pane(&pane, cx);
        click_toggle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 2, "the click opened the split");
        });
        click_toggle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 1, "and closed it again");
        });

        // ...and with focus on the workspace root, where the action's own
        // context lives.
        focus_workspace(&workspace, cx);
        click_toggle(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 2);
        });
    }

    // §0 "Info panel toggle | cmd-shift-i, **toolbar**" (M5). Same guard as
    // the split toggle above, and for the same reason: the button dispatches
    // the boxed action without focusing anything, so it lives or dies by the
    // focused element's dispatch path reaching the Workspace node. It also
    // pins the one thing a state assertion cannot see — that hiding the panel
    // takes its splitter's grab strip with it, rather than leaving an
    // undraggable one behind over the pane strip.
    #[gpui::test]
    fn the_titlebar_info_panel_toggle_hides_and_shows_the_column(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        let click_toggle = |cx: &mut VisualTestContext| {
            let at = cx
                .debug_bounds("info-panel-toggle")
                .expect("the titlebar toggle paints")
                .center();
            cx.simulate_click(at, gpui::Modifiers::none());
            cx.run_until_parked();
        };

        assert!(workspace.read_with(cx, |workspace, _| workspace.show_info_panel()));
        assert!(
            cx.debug_bounds("info-panel").is_some(),
            "the panel paints while it is showing"
        );
        assert!(cx.debug_bounds("info-panel-splitter").is_some());

        focus_pane(&pane, cx);
        click_toggle(cx);
        assert!(!workspace.read_with(cx, |workspace, _| workspace.show_info_panel()));
        assert!(
            cx.debug_bounds("info-panel").is_none(),
            "the hidden panel paints nothing"
        );
        assert!(
            cx.debug_bounds("info-panel-splitter").is_none(),
            "and takes its grab strip with it"
        );

        // ...and with focus on the workspace root, where the action's own
        // context lives.
        focus_workspace(&workspace, cx);
        click_toggle(cx);
        assert!(workspace.read_with(cx, |workspace, _| workspace.show_info_panel()));
        assert!(cx.debug_bounds("info-panel").is_some());
    }

    #[gpui::test]
    fn a_pinned_splitter_never_squeezes_the_other_pane_out_of_existence(cx: &mut TestAppContext) {
        // `PANE_MIN_WIDTH`'s own doc comment promises "the drag stops rather
        // than letting one pane be dragged out of existence" — but the pin was
        // only clamped against the strip *at drag time*. Widening the side
        // panels afterwards (no window resize, no second drag) left the first
        // pane overflowing the strip and the second with a 0px content area:
        // no breadcrumb, no rows, and no free-space status line, with the
        // splitter parked outside the strip under the info panel where no
        // drag could undo it.
        let (_vfs, workspace, cx) = open_split_workspace(cx);
        let even = pane_list_widths(&workspace, cx);
        assert_eq!(even.len(), 2);
        let strip: f32 = even.iter().sum();

        // Exactly what a legal splitter drag to the far right produces.
        workspace.update(cx, |workspace, cx| {
            workspace.set_first_pane_width(clamp_pane_width(strip, strip), cx);
        });
        cx.run_until_parked();
        for (ix, width) in pane_list_widths(&workspace, cx).iter().enumerate() {
            assert!(
                *width > 0.0,
                "pane {ix} vanished on the drag itself: {width}"
            );
        }

        // Now shrink the strip under the pin, both ways a user can: the side
        // panels out to their own documented maxima...
        workspace.update(cx, |workspace, cx| {
            workspace.set_sidebar_width(SIDEBAR_MAX_WIDTH, cx);
            workspace.set_info_panel_width(INFO_PANEL_MAX_WIDTH, cx);
        });
        cx.run_until_parked();
        let squeezed = pane_list_widths(&workspace, cx);
        assert!(
            squeezed.iter().all(|width| *width > 0.0),
            "widening the side panels emptied a pane: {squeezed:?}"
        );

        // ...and the window itself narrower.
        cx.simulate_resize(gpui::size(px(900.0), px(760.0)));
        cx.run_until_parked();
        let narrow = pane_list_widths(&workspace, cx);
        assert!(
            narrow.iter().all(|width| *width > 0.0),
            "a narrower window emptied a pane: {narrow:?}"
        );

        // And the splitter is still to the left of the surviving pane's
        // content rather than parked past it under the info panel, so the drag
        // that produced this is reversible by mouse.
        let handle = cx
            .debug_bounds("pane-splitter")
            .expect("the split splitter paints while split");
        let second = workspace.read_with(cx, |workspace, _| workspace.panes()[1].clone());
        let second_view = second.read_with(cx, |pane, _| pane.dir_view().clone());
        let second_bounds =
            second_view.read_with(cx, |view, _| crate::marquee::list_viewport(view));
        assert!(
            handle.origin.x < second_bounds.right(),
            "the splitter drifted past the second pane: splitter={handle:?} pane={second_bounds:?}"
        );
    }

    // §8 "Resizable splitters" for the split divider: both panes keep a
    // usable width, and a strip too narrow for two minimums does not panic.
    #[test]
    fn clamp_pane_width_keeps_both_panes_usable() {
        let strip = 1000.0;
        assert_eq!(clamp_pane_width(500.0, strip), 500.0);
        assert_eq!(clamp_pane_width(10.0, strip), PANE_MIN_WIDTH);
        assert_eq!(
            clamp_pane_width(990.0, strip),
            strip - PANE_MIN_WIDTH,
            "the second pane keeps its minimum"
        );
        // Degenerate strips: the minimum wins, and nothing panics on a
        // clamp whose bounds would otherwise invert.
        assert_eq!(clamp_pane_width(100.0, 0.0), PANE_MIN_WIDTH);
        assert_eq!(clamp_pane_width(1_000.0, 100.0), PANE_MIN_WIDTH);
        assert!(clamp_pane_width(f32::NAN, strip).is_nan());
    }

    #[gpui::test]
    fn split_pane_width_clamps_and_rejects_non_finite(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = open_split_workspace(cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.first_pane_width(),
                None,
                "a fresh split is an even split"
            );
        });
        workspace.update(cx, |workspace, cx| {
            workspace.set_first_pane_width(400.0, cx);
            workspace.set_first_pane_width(f32::NAN, cx);
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.first_pane_width(),
                Some(400.0),
                "NaN must not reach the layout"
            );
        });
        workspace.update(cx, |workspace, cx| {
            workspace.set_first_pane_width(1.0, cx);
        });
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.first_pane_width(), Some(PANE_MIN_WIDTH));
        });
    }

    // §0 Split-pane toggle: cmd-shift-o grows the pane Vec to two and back to
    // one, and the fresh pane opens on the same directory in the *other* view
    // mode (plan §2's list-beside-grid blueprint).
    #[gpui::test]
    fn cmd_shift_o_splits_and_collapses(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        vfs.insert_tree("/root", json!({ "beta": {} }));
        let (workspace, cx) = build_workspace(cx);
        let first = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        first.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        focus_workspace(&workspace, cx);
        cx.simulate_keystrokes("cmd-shift-o");
        cx.run_until_parked();

        let second = workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.is_split());
            assert_eq!(workspace.panes().len(), 2);
            assert_eq!(
                workspace.active_pane_ix(),
                1,
                "the new pane is the one you are working in"
            );
            let second = workspace.panes()[1].clone();
            assert_eq!(
                second.read(cx).path(),
                Some(Path::new("/root")),
                "the split opens where you split from"
            );
            assert_eq!(second.read(cx).view_mode(), ViewMode::Icons);
            assert_eq!(workspace.panes()[0].read(cx).view_mode(), ViewMode::List);
            second
        });

        cx.simulate_keystrokes("cmd-shift-o");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(!workspace.is_split());
            assert_eq!(workspace.panes().len(), 1);
            assert_eq!(workspace.active_pane_ix(), 0);
            assert_eq!(
                workspace.active_pane(),
                &second,
                "the ACTIVE pane survives the collapse"
            );
        });
        assert_ne!(
            workspace.read_with(cx, |workspace, _| workspace.active_pane().clone()),
            first,
            "and the other pane is gone with its state"
        );
    }

    // Collapsing while the *first* pane is active keeps that one instead —
    // the rule is "the active pane survives", not "pane 0 survives".
    #[gpui::test]
    fn collapsing_keeps_whichever_pane_is_active(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = open_split_workspace(cx);
        let (first, second) = workspace.read_with(cx, |workspace, _| {
            (workspace.panes()[0].clone(), workspace.panes()[1].clone())
        });
        second.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();

        focus_pane(&first, cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.active_pane_ix(), 0, "focus retargets commands");
        });

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.toggle_split_pane(window, cx));
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 1);
            assert_eq!(workspace.active_pane(), &first);
            assert_eq!(
                workspace.active_pane().read(cx).path(),
                Some(Path::new("/root")),
                "the surviving pane keeps its own directory"
            );
        });
    }

    // §2 `PaneEvent::FocusIn` through the real gesture: a **click** inside a
    // pane makes it the active one. gpui focuses the deepest handle under the
    // pointer (a list row, not the pane node), which is why the pane
    // subscribes with `on_focus_in` (subtree) rather than `on_focus`.
    #[gpui::test]
    fn clicking_into_a_pane_makes_it_active(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = open_split_workspace(cx);
        let first = workspace.read_with(cx, |workspace, _| workspace.panes()[0].clone());
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.active_pane_ix(), 1, "the split focused pane 1");
        });

        let first_view = first.read_with(cx, |pane, _| pane.dir_view().clone());
        let point = cx.update(|window, cx| {
            window.activate_window();
            let viewport = crate::marquee::list_viewport(first_view.read(cx));
            gpui::point(
                viewport.left() + px(40.0),
                viewport.top() + px(DirView::ROW_HEIGHT / 2.0),
            )
        });
        cx.run_until_parked();
        cx.simulate_click(point, gpui::Modifiers::none());
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.active_pane_ix(),
                0,
                "a click inside a pane retargets every workspace command"
            );
            assert_eq!(workspace.active_pane(), &first);
        });
    }

    // The second pane is a fully independent Pane: path, history, sort, view
    // mode and selection all move on their own.
    #[gpui::test]
    fn the_two_panes_navigate_sort_and_select_independently(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = open_split_workspace(cx);
        let (first, second) = workspace.read_with(cx, |workspace, _| {
            (workspace.panes()[0].clone(), workspace.panes()[1].clone())
        });

        second.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();
        second.update(cx, |pane, cx| pane.sort_by(fs_core::SortKey::Size, cx));
        second.update(cx, |pane, cx| pane.set_view_mode(ViewMode::List, cx));
        cx.run_until_parked();

        first.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")));
            assert_eq!(pane.sort().key, fs_core::SortKey::Name);
            assert_eq!(pane.view_mode(), ViewMode::List);
            assert!(!pane.can_go_back(), "the first pane never moved");
        });
        second.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/other")));
            assert_eq!(pane.sort().key, fs_core::SortKey::Size);
            assert!(pane.can_go_back(), "its own history, not a shared one");
        });

        // Selection is per-DirView too.
        let first_view = first.read_with(cx, |pane, _| pane.dir_view().clone());
        let second_view = second.read_with(cx, |pane, _| pane.dir_view().clone());
        first_view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx)
        });
        second_view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/other/o.txt")], cx)
        });
        first_view.read_with(cx, |view, _| {
            assert_eq!(
                view.selection().selected_rootmost(),
                vec![Arc::from(Path::new("/root/a.txt"))]
            );
        });
        second_view.read_with(cx, |view, _| {
            assert_eq!(
                view.selection().selected_rootmost(),
                vec![Arc::from(Path::new("/other/o.txt"))]
            );
        });

        // Second pane's history works on its own: back returns it to /root
        // while the first pane sits still.
        second.update(cx, |pane, cx| pane.go_back(cx));
        cx.run_until_parked();
        second.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")))
        });
        first.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")))
        });
    }

    // §2 `PaneEvent::FocusIn` → `active_pane_ix`: the workspace-level commands
    // must follow focus, not an index. cmd-l is the cheapest probe (it edits
    // exactly one pane's address bar), and `shift-delete` proves the same for
    // the destructive path, which reads the ACTIVE pane's selection.
    #[gpui::test]
    fn workspace_actions_target_the_focused_pane(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = open_split_workspace(cx);
        let (first, second) = workspace.read_with(cx, |workspace, _| {
            (workspace.panes()[0].clone(), workspace.panes()[1].clone())
        });

        // Splitting focused the new pane, so it is active.
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.active_pane_ix(), 1));
        cx.simulate_keystrokes("cmd-l");
        cx.run_until_parked();
        second.read_with(cx, |pane, _| {
            assert_eq!(pane.address_bar_mode(), AddressBarMode::Editing)
        });
        first.read_with(cx, |pane, _| {
            assert_eq!(
                pane.address_bar_mode(),
                AddressBarMode::Breadcrumb,
                "the other pane's address bar is untouched"
            );
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // Focus the first pane: the same keystroke now lands there.
        focus_pane(&first, cx);
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.active_pane_ix(), 0));
        cx.simulate_keystrokes("cmd-l");
        cx.run_until_parked();
        first.read_with(cx, |pane, _| {
            assert_eq!(pane.address_bar_mode(), AddressBarMode::Editing)
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        // Hidden files stay workspace-global: the toggle fans out to BOTH
        // panes, whichever one is active.
        focus_workspace(&workspace, cx);
        cx.simulate_keystrokes("cmd-shift-.");
        cx.run_until_parked();
        for pane in [&first, &second] {
            pane.read_with(cx, |pane, _| assert!(pane.show_hidden()));
        }
        cx.simulate_keystrokes("cmd-shift-.");
        cx.run_until_parked();

        // The destructive path reads the active pane's selection: with the
        // *second* pane active, the first pane's selection must not be what
        // shift-delete deletes.
        let first_view = first.read_with(cx, |pane, _| pane.dir_view().clone());
        let second_view = second.read_with(cx, |pane, _| pane.dir_view().clone());
        first_view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx)
        });
        second_view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/beta")], cx)
        });
        cx.update(|window, cx| {
            let handle = second_view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("shift-delete");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(
            !exists(&vfs, "/root/beta"),
            "the active pane's selection is what went"
        );
        assert!(
            exists(&vfs, "/root/a.txt"),
            "the inactive pane's selection was never touched"
        );
    }

    // ARCHITECTURE claims cross-pane drag "already works — the payload is
    // window-global". This is that claim, tested: a real drag out of the left
    // pane and into the right one, where `drop_copies`'s same-volume rule
    // makes it a **move** (§3 Explorer behavior).
    #[gpui::test]
    fn dragging_between_panes_moves_the_entry(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = open_split_workspace(cx);
        let (first, second) = workspace.read_with(cx, |workspace, _| {
            (workspace.panes()[0].clone(), workspace.panes()[1].clone())
        });
        second.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();

        let source_view = first.read_with(cx, |pane, _| pane.dir_view().clone());
        let dest_view = second.read_with(cx, |pane, _| pane.dir_view().clone());

        // Rows of /root, dirs first: beta, a.txt.
        let rows: Vec<std::path::PathBuf> = source_view.read_with(cx, |view, _| {
            view.flat_rows()
                .iter()
                .map(|row| row.entry.path.to_path_buf())
                .collect()
        });
        let source_ix = rows
            .iter()
            .position(|p| p == Path::new("/root/a.txt"))
            .expect("a.txt is listed");

        let (from, to) = cx.update(|_, cx| {
            let source = crate::marquee::list_viewport(source_view.read(cx));
            let dest = crate::marquee::list_viewport(dest_view.read(cx));
            assert!(
                source.right() <= dest.left(),
                "the two panes must be laid out side by side, got {source:?} and {dest:?}"
            );
            (
                gpui::point(
                    source.left() + px(40.0),
                    source.top()
                        + px(source_ix as f32 * DirView::ROW_HEIGHT + DirView::ROW_HEIGHT / 2.0),
                ),
                // Empty space below the destination pane's single row: the
                // background target, i.e. "into the folder this pane shows".
                gpui::point(
                    dest.left() + px(40.0),
                    dest.top() + px(3.0 * DirView::ROW_HEIGHT),
                ),
            )
        });

        cx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            from + gpui::point(px(6.0), px(6.0)),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_move(to, gpui::MouseButton::Left, gpui::Modifiers::none());

        // The *destination* pane armed the target; the source pane cleared its
        // own (the payload crossed the pane boundary because it is
        // window-global — `DraggedEntries.source_pane` only records where it
        // came from).
        dest_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_drop_target(cx),
                Some(&crate::drag::DropTarget::Background),
                "the other pane accepted the drag"
            );
        });
        source_view.read_with(cx, |view, cx| {
            assert!(view.active_drop_target(cx).is_none());
        });

        cx.simulate_mouse_up(to, gpui::MouseButton::Left, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(exists(&vfs, "/other/a.txt"), "moved into the other pane");
        assert!(
            !exists(&vfs, "/root/a.txt"),
            "one volume: a cross-pane drag moves (§3), it does not copy"
        );
    }

    #[gpui::test]
    fn delete_permanently_with_nothing_selected_opens_no_dialog(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        cx.update(|window, cx| {
            let handle = dir_view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });

        cx.simulate_keystrokes("shift-delete");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.active_modal().is_none());
        });
    }
}
