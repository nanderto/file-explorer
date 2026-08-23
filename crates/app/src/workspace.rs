//! The window root entity (ARCHITECTURE.md §2 `Workspace`), grown out of the
//! M0 `WorkspaceView` skeleton — same chrome (titlebar, info-panel
//! placeholder), the main pane as a real [`Pane`] entity, the root carrying
//! the `Workspace` key context, and (M2) the real [`Sidebar`] entity plus
//! hand-built resizable splitters (§8 "Resizable splitters").
//!
//! Panes live in a `Vec` from day one (len 1 for M1) so the M4 split-pane
//! toggle grows the vector instead of reshaping the tree.

use fs_core::{Conflict, FileOp, JobId, UndoOutcome};
use gpui::{
    AnyElement, App, Context, DragMoveEvent, Entity, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Subscription, Window, deferred, div, prelude::*, px,
};

use crate::actions::{DeletePermanently, FocusAddressBar, Redo, ToggleHiddenFiles, Undo};
use crate::app_state::FsContext;
use crate::dialogs::{ConfirmDialog, ConfirmDialogEvent, ConflictDialog, ConflictDialogEvent};
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
/// Width of the invisible grab strip straddling each region border.
const SPLITTER_HITBOX_WIDTH: f32 = 6.0;

/// Which divider is being dragged (the `on_drag` payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitterSide {
    Sidebar,
    InfoPanel,
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
    panes: Vec<gpui::Entity<Pane>>,
    active_pane_ix: usize,
    show_hidden: bool,
    sidebar_width: f32,
    info_panel_width: f32,
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
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            focus_handle,
            theme,
            sidebar,
            panes: vec![pane],
            active_pane_ix: 0,
            show_hidden: false,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            info_panel_width: INFO_PANEL_DEFAULT_WIDTH,
            jobs,
            jobs_indicator,
            toast_layer,
            modal: None,
            _subscriptions: vec![sidebar_subscription, pane_subscription, jobs_subscription],
        }
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
                }
            }
        }
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
        }
    }

    // ------------------------------------------------------------------
    // Job spine: conflicts → modal, undo/redo, dialogs (§2, §4b, §8)
    // ------------------------------------------------------------------

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
            // `Failed` is consumed by whichever view submitted the job (e.g.
            // the rename editor); the workspace has nothing job-id-specific
            // to do beyond the toast `JobsModel` already pushed.
            JobsEvent::RowsChanged | JobsEvent::Completed { .. } | JobsEvent::Failed { .. } => {}
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
        }
    }

    /// The invisible grab strip straddling a region border (§8 hand-built
    /// splitters): a stateful div whose `on_drag` starts the resize; the body
    /// row's `on_drag_move` does the math.
    fn splitter_handle(&self, side: SplitterSide) -> impl IntoElement {
        let theme = self.theme.clone();
        let handle = div()
            .id(match side {
                SplitterSide::Sidebar => "sidebar-splitter",
                SplitterSide::InfoPanel => "info-panel-splitter",
            })
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
            SplitterSide::Sidebar => handle.right(px(-SPLITTER_HITBOX_WIDTH / 2.0)),
            SplitterSide::InfoPanel => handle.left(px(-SPLITTER_HITBOX_WIDTH / 2.0)),
        }
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
            .on_action(cx.listener(Self::handle_toggle_hidden_files))
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
                    .child(self.jobs_indicator.clone()),
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
                    // Pane strip (len 1 in M1)
                    .children(self.panes.iter().cloned())
                    // Info panel (placeholder until M5), resizable
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .w(px(self.info_panel_width))
                            .bg(theme.panel)
                            .border_l_1()
                            .border_color(theme.border)
                            .text_size(px(13.0))
                            .text_color(theme.muted)
                            .child("No selection")
                            .child(self.splitter_handle(SplitterSide::InfoPanel)),
                    ),
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
    use crate::pane::AddressBarMode;
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
