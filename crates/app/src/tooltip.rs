//! A minimal themed tooltip.
//!
//! gpui's `.tooltip(..)` takes a builder returning an `AnyView`, so a tooltip
//! has to *be* a view — there is no "just show this string" shorthand. This is
//! that view: one line of text in a small panel, themed like every other
//! surface in the app.
//!
//! Deliberately plain. Zed's own `ui` crate has a richer tooltip (key
//! equivalents, secondary lines), but it is GPL and not a dependency here, and
//! a label is all the controls in this app need.

use gpui::{App, Context, IntoElement, Render, SharedString, Window, div, prelude::*, px};

/// One line of text, shown on hover.
pub struct Tooltip {
    label: SharedString,
}

impl Tooltip {
    /// Build the `.tooltip(..)` argument for `label`.
    ///
    /// ```ignore
    /// .tooltip(Tooltip::text("Copy path"))
    /// ```
    pub fn text(
        label: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
        let label = label.into();
        move |_window, cx| {
            let label = label.clone();
            cx.new(|_| Tooltip { label }).into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx).clone();
        div()
            .px(px(6.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .text_size(px(12.0))
            .text_color(theme.text)
            .child(self.label.clone())
    }
}
