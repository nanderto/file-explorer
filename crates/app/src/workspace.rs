//! The window root entity (ARCHITECTURE.md §2 `Workspace`), grown out of the
//! M0 `WorkspaceView` skeleton — same chrome (titlebar, sidebar placeholder,
//! info-panel placeholder), but the main pane is now a real [`Pane`] entity
//! and the root carries the `Workspace` key context.
//!
//! Panes live in a `Vec` from day one (len 1 for M1) so the M4 split-pane
//! toggle grows the vector instead of reshaping the tree.

use gpui::{
    App, Context, FocusHandle, Focusable, IntoElement, Render, SharedString, Window, div,
    prelude::*, px,
};

use crate::actions::{FocusAddressBar, ToggleHiddenFiles};
use crate::pane::Pane;
use crate::theme::Theme;

/// Font used for all UI text. Pinned to a face that ships with macOS so
/// visual-test screenshots are stable across machines and CI runners.
pub const UI_FONT: &str = "Helvetica";

pub struct Workspace {
    focus_handle: FocusHandle,
    theme: Theme,
    panes: Vec<gpui::Entity<Pane>>,
    active_pane_ix: usize,
    show_hidden: bool,
}

impl Workspace {
    pub fn new(theme: Theme, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| Pane::new(theme.clone(), window, cx));
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            focus_handle,
            theme,
            panes: vec![pane],
            active_pane_ix: 0,
            show_hidden: false,
        }
    }

    pub fn active_pane(&self) -> &gpui::Entity<Pane> {
        &self.panes[self.active_pane_ix]
    }

    pub fn panes(&self) -> &[gpui::Entity<Pane>] {
        &self.panes
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
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

    fn sidebar_section(&self, title: &'static str, items: &[&'static str]) -> impl IntoElement {
        let theme = &self.theme;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.muted)
                    .px(px(12.0))
                    .pt(px(12.0))
                    .child(SharedString::new_static(title)),
            )
            .children(items.iter().map(|item| {
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text)
                    .px(px(16.0))
                    .py(px(2.0))
                    .child(SharedString::new_static(item))
            }))
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
            // Body: sidebar | pane(s) | info panel
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    // Sidebar (placeholder until M2)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w(px(220.0))
                            .bg(theme.sidebar)
                            .border_r_1()
                            .border_color(theme.border)
                            .child(self.sidebar_section("Devices", &["Macintosh HD"]))
                            .child(self.sidebar_section(
                                "Favorites",
                                &["Desktop", "Downloads", "Documents"],
                            ))
                            .child(self.sidebar_section("Tags", &[])),
                    )
                    // Pane strip (len 1 in M1)
                    .children(self.panes.iter().cloned())
                    // Info panel (placeholder until M5)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .w(px(260.0))
                            .bg(theme.panel)
                            .border_l_1()
                            .border_color(theme.border)
                            .text_size(px(13.0))
                            .text_color(theme.muted)
                            .child("No selection"),
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
            });
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
