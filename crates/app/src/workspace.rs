//! The window root entity (ARCHITECTURE.md §2 `Workspace`), grown out of the
//! M0 `WorkspaceView` skeleton — same chrome (titlebar, info-panel
//! placeholder), the main pane as a real [`Pane`] entity, the root carrying
//! the `Workspace` key context, and (M2) the real [`Sidebar`] entity plus
//! hand-built resizable splitters (§8 "Resizable splitters").
//!
//! Panes live in a `Vec` from day one (len 1 for M1) so the M4 split-pane
//! toggle grows the vector instead of reshaping the tree.

use gpui::{
    App, Context, DragMoveEvent, Entity, FocusHandle, Focusable, IntoElement, Render, Subscription,
    Window, div, prelude::*, px,
};

use crate::actions::{FocusAddressBar, ToggleHiddenFiles};
use crate::app_state::FsContext;
use crate::pane::Pane;
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

pub struct Workspace {
    focus_handle: FocusHandle,
    theme: Theme,
    sidebar: Entity<Sidebar>,
    panes: Vec<gpui::Entity<Pane>>,
    active_pane_ix: usize,
    show_hidden: bool,
    sidebar_width: f32,
    info_panel_width: f32,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(theme: Theme, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| Pane::new(theme.clone(), window, cx));
        let workspace = cx.weak_entity();
        let sidebar = cx.new(|cx| Sidebar::new(theme.clone(), workspace, cx));
        // Events up, method calls down (§2): the sidebar reports navigation
        // and eject requests; the workspace acts on them.
        let sidebar_subscription = cx.subscribe(&sidebar, Self::handle_sidebar_event);
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
            _subscriptions: vec![sidebar_subscription],
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
            .flex()
            .flex_col()
            .size_full()
            .font_family(UI_FONT)
            .bg(theme.surface)
            .text_color(theme.text)
            // Titlebar
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(px(40.0))
                    .px(px(80.0))
                    .bg(theme.titlebar)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(13.0))
                    .child("file-explorer"),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{FsContext, GpuiSpawner, LoggingOpener};
    use crate::pane::AddressBarMode;
    use fs_core::{FakeVfs, Spawner};
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
            cx.set_global(FsContext {
                vfs: vfs.clone(),
                spawner,
                opener: Arc::new(LoggingOpener),
                platform: Arc::new(fs_core::StubPlatform::new()),
            });
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
}
