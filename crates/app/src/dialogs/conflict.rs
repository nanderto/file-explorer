//! The Explorer-style conflict dialog (plan §3, ARCHITECTURE.md §0/§8):
//! Replace / Skip / Keep both with a size + date comparison of both sides
//! and an "Apply to all" toggle. Carries its own `ConflictDialog` key
//! context (`track_focus`): `r`/`s`/`k` resolve, `a` toggles apply-to-all,
//! `enter` activates the default (Replace), `escape` dismisses **and
//! cancels the job**. Dumb by design: it emits [`ConflictDialogEvent`]; the
//! workspace forwards the resolution to `JobQueue::resolve`/`cancel` and
//! tells the `JobsModel` the decision was handled.

use fs_core::{Conflict, ConflictChoice, EntryMeta, Resolution};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString, Window,
    div, prelude::*, px,
};

use crate::actions::{
    Cancel, Confirm, ConflictKeepBoth, ConflictReplace, ConflictSkip, ToggleApplyToAll,
};
use crate::dialogs::confirm::dialog_button;
use crate::pane::format_bytes;
use crate::theme::Theme;
use crate::views::details_list::format_modified;

/// The user's verdict; the workspace acts on it and closes the modal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictDialogEvent {
    Resolved(Resolution),
    /// Escape: dismiss the dialog and cancel the whole job (§0).
    Cancelled,
}

pub struct ConflictDialog {
    theme: Theme,
    conflict: Conflict,
    apply_to_all: bool,
    focus_handle: FocusHandle,
}

impl EventEmitter<ConflictDialogEvent> for ConflictDialog {}

impl ConflictDialog {
    pub fn new(theme: Theme, conflict: Conflict, cx: &mut Context<Self>) -> Self {
        Self {
            theme,
            conflict,
            apply_to_all: false,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn conflict(&self) -> &Conflict {
        &self.conflict
    }

    pub fn apply_to_all(&self) -> bool {
        self.apply_to_all
    }

    fn resolve(&mut self, choice: ConflictChoice, cx: &mut Context<Self>) {
        cx.emit(ConflictDialogEvent::Resolved(Resolution {
            choice,
            apply_to_all: self.apply_to_all,
        }));
    }

    fn handle_replace(&mut self, _: &ConflictReplace, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve(ConflictChoice::Replace, cx);
    }

    fn handle_skip(&mut self, _: &ConflictSkip, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve(ConflictChoice::Skip, cx);
    }

    fn handle_keep_both(&mut self, _: &ConflictKeepBoth, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve(ConflictChoice::KeepBoth, cx);
    }

    fn handle_toggle_apply_to_all(
        &mut self,
        _: &ToggleApplyToAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_to_all = !self.apply_to_all;
        cx.notify();
    }

    /// `enter` activates the default choice (Replace, like Explorer).
    fn handle_confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.resolve(ConflictChoice::Replace, cx);
    }

    fn handle_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ConflictDialogEvent::Cancelled);
    }

    /// One side of the size + date comparison (plan §3 "with size+date
    /// comparison").
    fn comparison_column(&self, label: &'static str, meta: &EntryMeta) -> impl IntoElement {
        let theme = &self.theme;
        div()
            .flex()
            .flex_col()
            .flex_1()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.muted)
                    .child(SharedString::new_static(label)),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .child(SharedString::new(format_bytes(meta.size))),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(SharedString::new(format_modified(meta.modified))),
            )
    }
}

impl Focusable for ConflictDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConflictDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let name: SharedString = self
            .conflict
            .dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();

        div()
            .track_focus(&self.focus_handle)
            .key_context("ConflictDialog")
            .on_action(cx.listener(Self::handle_replace))
            .on_action(cx.listener(Self::handle_skip))
            .on_action(cx.listener(Self::handle_keep_both))
            .on_action(cx.listener(Self::handle_toggle_apply_to_all))
            .on_action(cx.listener(Self::handle_confirm))
            .on_action(cx.listener(Self::handle_cancel))
            .occlude()
            .flex()
            .flex_col()
            .w(px(460.0))
            .p(px(16.0))
            .gap(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .text_color(theme.text)
            .child(div().text_size(px(14.0)).child(SharedString::new(format!(
                "An item named \u{201c}{name}\u{201d} already exists in this location"
            ))))
            // Size + date comparison: destination (existing) vs source (new).
            .child(
                div()
                    .flex()
                    .gap(px(16.0))
                    .p(px(8.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.border)
                    .child(self.comparison_column("Existing", &self.conflict.dest_meta))
                    .child(self.comparison_column("New", &self.conflict.src_meta)),
            )
            // Apply-to-all toggle (a).
            .child(
                div()
                    .id("conflict-apply-to-all")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.apply_to_all = !this.apply_to_all;
                        cx.notify();
                    }))
                    .child(SharedString::new_static(if self.apply_to_all {
                        "☑"
                    } else {
                        "☐"
                    }))
                    .child(SharedString::new_static("Apply to all (a)")),
            )
            // Choices: Replace (r, default/enter) · Skip (s) · Keep both (k).
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.0))
                    .child(dialog_button(
                        "conflict-skip",
                        SharedString::new_static("Skip (s)"),
                        theme.text,
                        &theme,
                        cx.listener(|this, _, _, cx| this.resolve(ConflictChoice::Skip, cx)),
                    ))
                    .child(dialog_button(
                        "conflict-keep-both",
                        SharedString::new_static("Keep both (k)"),
                        theme.text,
                        &theme,
                        cx.listener(|this, _, _, cx| this.resolve(ConflictChoice::KeepBoth, cx)),
                    ))
                    .child(dialog_button(
                        "conflict-replace",
                        SharedString::new_static("Replace (r)"),
                        theme.accent,
                        &theme,
                        cx.listener(|this, _, _, cx| this.resolve(ConflictChoice::Replace, cx)),
                    )),
            )
    }
}
