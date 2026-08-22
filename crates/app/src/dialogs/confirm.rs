//! Confirmation dialog (ARCHITECTURE.md §8 "Dialogs"): guards destructive
//! actions — concretely the §0 `DeletePermanently` row ("Bypass trash
//! (confirm dialog first)"). Dumb by design: it renders, tracks focus in its
//! own `ConfirmDialog` key context (`enter` → [`crate::actions::Confirm`],
//! `escape` → [`crate::actions::Cancel`]), and emits — the workspace owns
//! the pending operation and submits it on [`ConfirmDialogEvent::Confirmed`].

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString, Window,
    div, prelude::*, px,
};

use crate::actions::{Cancel, Confirm};
use crate::theme::Theme;

/// What the dialog resolved to; the workspace closes the modal either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmDialogEvent {
    Confirmed,
    Cancelled,
}

pub struct ConfirmDialog {
    theme: Theme,
    title: SharedString,
    message: SharedString,
    confirm_label: SharedString,
    focus_handle: FocusHandle,
}

impl EventEmitter<ConfirmDialogEvent> for ConfirmDialog {}

impl ConfirmDialog {
    pub fn new(
        theme: Theme,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            theme,
            title: title.into(),
            message: message.into(),
            confirm_label: confirm_label.into(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn handle_confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ConfirmDialogEvent::Confirmed);
    }

    fn handle_cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ConfirmDialogEvent::Cancelled);
    }
}

impl Focusable for ConfirmDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConfirmDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        div()
            .track_focus(&self.focus_handle)
            .key_context("ConfirmDialog")
            .on_action(cx.listener(Self::handle_confirm))
            .on_action(cx.listener(Self::handle_cancel))
            .occlude()
            .flex()
            .flex_col()
            .w(px(420.0))
            .p(px(16.0))
            .gap(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .text_color(theme.text)
            .child(div().text_size(px(14.0)).child(self.title.clone()))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(self.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(dialog_button(
                        "confirm-cancel",
                        SharedString::new_static("Cancel"),
                        theme.text,
                        &theme,
                        cx.listener(|_this, _, _, cx| cx.emit(ConfirmDialogEvent::Cancelled)),
                    ))
                    .child(dialog_button(
                        "confirm-accept",
                        self.confirm_label.clone(),
                        theme.error,
                        &theme,
                        cx.listener(|_this, _, _, cx| cx.emit(ConfirmDialogEvent::Confirmed)),
                    )),
            )
    }
}

/// A plain dialog button. Shared with the conflict dialog's row of choices.
pub(crate) fn dialog_button(
    id: &'static str,
    label: SharedString,
    label_color: gpui::Hsla,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(12.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(theme.border)
        .text_size(px(12.0))
        .text_color(label_color)
        .cursor_pointer()
        .hover(|s| s.bg(theme.accent.opacity(0.15)))
        .on_click(on_click)
        .child(label)
}
