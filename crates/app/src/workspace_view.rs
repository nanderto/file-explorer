//! The window root view: M0 skeleton of the layout in
//! docs/requirements/Basic window.png — titlebar, sidebar, main pane with
//! status line, and info panel. Content is placeholder until M1+.

use gpui::{Context, IntoElement, Render, SharedString, Window, div, prelude::*, px};

use crate::theme::Theme;

/// Font used for all UI text. Pinned to a face that ships with macOS so
/// visual-test screenshots are stable across machines and CI runners.
pub const UI_FONT: &str = "Helvetica";

pub struct WorkspaceView {
    theme: Theme,
}

impl WorkspaceView {
    pub fn new(theme: Theme) -> Self {
        Self { theme }
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

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        div()
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
            // Body: sidebar | main pane | info panel
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    // Sidebar
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
                    // Main pane
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_1()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(13.0))
                                    .text_color(theme.muted)
                                    .child("No folder open"),
                            )
                            .child(
                                // Status line
                                div()
                                    .flex()
                                    .items_center()
                                    .h(px(24.0))
                                    .px(px(12.0))
                                    .border_t_1()
                                    .border_color(theme.border)
                                    .text_size(px(11.0))
                                    .text_color(theme.muted)
                                    .child("0 items"),
                            ),
                    )
                    // Info panel
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
