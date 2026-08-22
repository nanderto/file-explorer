//! The editable half of the address bar (ARCHITECTURE.md §8 "Breadcrumb /
//! address bar").
//!
//! The breadcrumb itself is rendered by [`crate::pane::Pane`] (it owns the
//! path and [`crate::pane::AddressBarMode`]); this entity is the editor that
//! replaces it in `Editing` mode: the vendored [`InputState`] prefilled with
//! the current path, an autocomplete popup fed by a background `read_dir`
//! (never the UI thread), `tab` = accept the highlighted suggestion in place,
//! `enter` = validate-and-navigate (invalid paths keep editing with an inline
//! error), `escape` = restore the breadcrumb.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Task, Window, div, prelude::*, px,
};

use crate::actions::{AcceptSuggestion, Cancel, Confirm};
use crate::app_state::FsContext;
use crate::input::text_input as ti;
use crate::input::{InputEvent, InputState};
use crate::theme::Theme;

/// Outcome of an edit, consumed by the owning Pane.
#[derive(Debug, Clone, PartialEq)]
pub enum AddressBarEvent {
    /// A valid directory path was confirmed.
    Navigated(PathBuf),
    /// Editing was abandoned (`escape` or blur) — restore the breadcrumb.
    Cancelled,
}

pub struct AddressBar {
    theme: Theme,
    input: Entity<InputState>,
    /// Directory-name completions for the segment being typed, as full paths.
    suggestions: Vec<SharedString>,
    /// Index into `suggestions` accepted by `tab`.
    selected_suggestion: usize,
    /// Inline validation error from the last `Confirm`.
    error: Option<SharedString>,
    /// Autocomplete race guard, same pattern as Pane's listing generation.
    generation: u64,
    _suggest_task: Option<Task<()>>,
    /// Separate slot: a pending Change-driven suggestion refresh must never
    /// cancel an in-flight confirm (dropping a Task cancels it).
    _confirm_task: Option<Task<()>>,
    _input_subscription: Subscription,
}

impl EventEmitter<AddressBarEvent> for AddressBar {}

impl AddressBar {
    pub fn new(theme: Theme, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(cx).placeholder("Type a path…").with_colors(
                theme.muted,
                theme.accent,
                theme.accent.opacity(0.25),
            )
        });
        let subscription = cx.subscribe(&input, Self::on_input_event);
        Self {
            theme,
            input,
            suggestions: Vec::new(),
            selected_suggestion: 0,
            error: None,
            generation: 0,
            _suggest_task: None,
            _confirm_task: None,
            _input_subscription: subscription,
        }
    }

    /// Enter editing mode prefilled with `path`, focused, fully selected —
    /// typing replaces, arrow keys refine (Explorer behavior).
    pub fn begin_editing(
        &mut self,
        path: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = path.map(|p| p.display().to_string()).unwrap_or_default();
        self.error = None;
        self.suggestions.clear();
        self.selected_suggestion = 0;
        self.input.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            input.select_all(&crate::input::text_input::SelectAll, window, cx);
        });
        let handle = self.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).content().to_string()
    }

    fn on_input_event(
        &mut self,
        _input: Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        // The vendored input also emits Enter/Escape events, but our keymap
        // dispatches Confirm/Cancel actions in the TextInput context instead
        // (handled in render()) — Change is the only event consumed here.
        if let InputEvent::Change = event {
            self.error = None;
            self.refresh_suggestions(cx);
        }
    }

    /// §8: autocomplete = background `read_dir` of the typed path's parent,
    /// filtered to directories whose name starts with the typed prefix
    /// (case-insensitive). Never touches the disk on the UI thread.
    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let typed = self.text(cx);
        self.generation += 1;
        let generation = self.generation;

        // "/Users/foo/Doc" → parent "/Users/foo", prefix "doc".
        // A trailing separator means "list everything inside".
        let (parent, prefix) = split_for_completion(&typed);
        let Some(parent) = parent else {
            self.suggestions.clear();
            self.selected_suggestion = 0;
            cx.notify();
            return;
        };

        let vfs = FsContext::global(cx).vfs.clone();
        self._suggest_task = Some(cx.spawn(async move |this, cx| {
            let parent_arc: Arc<Path> = Arc::from(parent.as_path());
            let listed = cx
                .background_spawn(fs_core::list_dir(
                    vfs,
                    parent_arc,
                    fs_core::SortSpec::default(),
                    false,
                    generation,
                ))
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return; // stale — a newer keystroke superseded this load
                }
                this.suggestions = match listed {
                    Ok(snapshot) => snapshot
                        .entries
                        .iter()
                        .filter(|e| e.is_dir_like())
                        .filter(|e| {
                            prefix.is_empty()
                                || e.name.to_lowercase().starts_with(&prefix.to_lowercase())
                        })
                        .take(8)
                        .map(|e| SharedString::from(e.path.display().to_string()))
                        .collect(),
                    Err(_) => Vec::new(),
                };
                this.selected_suggestion = 0;
                cx.notify();
            })
            .ok();
        }));
    }

    /// `tab` — complete the highlighted suggestion in place and keep editing
    /// (a trailing separator invites the next segment).
    fn accept_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(suggestion) = self.suggestions.get(self.selected_suggestion).cloned() else {
            return;
        };
        let completed = format!("{}{}", suggestion, std::path::MAIN_SEPARATOR);
        self.input.update(cx, |input, cx| {
            input.set_value(completed, window, cx);
        });
        self.refresh_suggestions(cx);
        cx.notify();
    }

    /// `enter` — validate on the background executor; a directory navigates,
    /// anything else keeps editing with an inline error.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let typed = self.text(cx);
        let path = PathBuf::from(typed.trim());
        if path.as_os_str().is_empty() {
            self.error = Some("Enter a path".into());
            cx.notify();
            return;
        }
        let vfs = FsContext::global(cx).vfs.clone();
        let stat_path = path.clone();
        self._confirm_task = Some(cx.spawn(async move |this, cx| {
            let meta = cx
                .background_spawn(async move { vfs.metadata(&stat_path).await })
                .await;
            this.update(cx, |this, cx| match meta {
                Ok(Some(meta)) if meta.kind.is_dir_like() => {
                    cx.emit(AddressBarEvent::Navigated(path));
                }
                Ok(Some(_)) => {
                    this.error = Some("Not a folder".into());
                    cx.notify();
                }
                Ok(None) => {
                    this.error = Some("Path does not exist".into());
                    cx.notify();
                }
                Err(e) => {
                    this.error = Some(SharedString::from(format!("Cannot open: {e}")));
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(AddressBarEvent::Cancelled);
    }
}

/// Split typed text into (existing parent directory to list, prefix to match).
/// Returns `None` parent when the text has no usable directory component yet.
fn split_for_completion(typed: &str) -> (Option<PathBuf>, String) {
    if typed.is_empty() {
        return (None, String::new());
    }
    let ends_with_sep = typed.ends_with('/') || typed.ends_with('\\');
    let path = PathBuf::from(typed);
    if ends_with_sep {
        return (Some(path), String::new());
    }
    let prefix = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => (Some(parent.to_path_buf()), prefix),
        _ => (None, prefix),
    }
}

impl Focusable for AddressBar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for AddressBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let error = self.error.clone();
        let suggestions = self.suggestions.clone();
        let selected = self.selected_suggestion;

        div()
            // AddressBar context carries `tab` → AcceptSuggestion (§0); the
            // inner TextInput context carries Confirm/Cancel and the editing
            // keys. Focus lives on the input; contexts nest via DOM ancestry.
            .key_context("AddressBar")
            .on_action(cx.listener(|this, _: &AcceptSuggestion, window, cx| {
                this.accept_suggestion(window, cx)
            }))
            .flex()
            .flex_col()
            .flex_1()
            .child(
                div()
                    .key_context("TextInput")
                    .on_action(cx.listener(|this, _: &Confirm, _, cx| this.confirm(cx)))
                    .on_action(cx.listener(|this, _: &Cancel, _, cx| this.cancel(cx)))
                    // Forward the vendored input's editing actions (bound in
                    // keymap.rs, TextInput context) into the InputState.
                    .on_action(cx.listener(|this, a: &ti::Left, w, cx| {
                        this.input.update(cx, |i, cx| i.left(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::Right, w, cx| {
                        this.input.update(cx, |i, cx| i.right(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::SelectLeft, w, cx| {
                        this.input.update(cx, |i, cx| i.select_left(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::SelectRight, w, cx| {
                        this.input.update(cx, |i, cx| i.select_right(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::SelectAll, w, cx| {
                        this.input.update(cx, |i, cx| i.select_all(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::Home, w, cx| {
                        this.input.update(cx, |i, cx| i.home(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::End, w, cx| {
                        this.input.update(cx, |i, cx| i.end(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::Backspace, w, cx| {
                        this.input.update(cx, |i, cx| i.backspace(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::Delete, w, cx| {
                        this.input.update(cx, |i, cx| i.delete(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::Copy, w, cx| {
                        this.input.update(cx, |i, cx| i.copy(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::Cut, w, cx| {
                        this.input.update(cx, |i, cx| i.cut(a, w, cx))
                    }))
                    .on_action(cx.listener(|this, a: &ti::Paste, w, cx| {
                        this.input.update(cx, |i, cx| i.paste(a, w, cx))
                    }))
                    .flex()
                    .items_center()
                    .h(px(24.0))
                    .px(px(6.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(if error.is_some() {
                        theme.error
                    } else {
                        theme.accent
                    })
                    .bg(theme.surface)
                    .text_size(px(13.0))
                    .child(self.input.clone()),
            )
            .when_some(error, |el, error| {
                el.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.error)
                        .px(px(6.0))
                        .child(error),
                )
            })
            .when(!suggestions.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .mt(px(2.0))
                        .rounded(px(4.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.panel)
                        .children(suggestions.into_iter().enumerate().map(|(i, s)| {
                            div()
                                .px(px(6.0))
                                .py(px(2.0))
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .when(i == selected, |el| el.bg(theme.accent.opacity(0.2)))
                                .child(s)
                        })),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{FsContext, GpuiSpawner, LoggingOpener};
    use fs_core::{FakeVfs, Spawner};
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::sync::Arc;

    fn setup(cx: &mut TestAppContext) -> (Entity<AddressBar>, &mut VisualTestContext) {
        let fake = fake_vfs(cx);
        cx.update(|cx| {
            crate::keymap::init(cx);
            cx.set_global(FsContext {
                vfs: fake,
                spawner: Arc::new(GpuiSpawner::new(cx.background_executor().clone())),
                opener: Arc::new(LoggingOpener),
            });
        });
        let (bar, cx) = cx.add_window_view(|_, cx| AddressBar::new(Theme::dark(), cx));
        (bar, cx)
    }

    fn fake_vfs(cx: &mut TestAppContext) -> Arc<dyn fs_core::Vfs> {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(
            cx.update(|cx| cx.background_executor().clone()),
        ));
        let vfs = FakeVfs::new(spawner);
        vfs.insert_tree(
            "/",
            json!({
                "home": {
                    "Documents": { "notes.txt": "hi" },
                    "Downloads": {},
                    "Desktop": {},
                    "readme.md": "hello",
                }
            }),
        );
        vfs
    }

    #[gpui::test]
    async fn typing_fetches_directory_suggestions_in_background(cx: &mut TestAppContext) {
        let (bar, cx) = setup(cx);
        bar.update_in(cx, |bar, window, cx| {
            bar.begin_editing(None, window, cx);
            bar.input.update(cx, |input, cx| {
                input.set_value(format!("{}home{}Do", sep(), sep()), window, cx)
            });
        });
        cx.run_until_parked();
        bar.update(cx, |bar, _| {
            let names: Vec<_> = bar.suggestions.iter().map(|s| s.to_string()).collect();
            assert_eq!(names.len(), 2, "Documents + Downloads, got {names:?}");
            assert!(names.iter().all(|n| n.contains("Do")));
            assert!(
                !names.iter().any(|n| n.contains("readme")),
                "files are not completed, got {names:?}"
            );
        });
    }

    #[gpui::test]
    async fn tab_accepts_the_highlighted_suggestion_in_place(cx: &mut TestAppContext) {
        let (bar, cx) = setup(cx);
        bar.update_in(cx, |bar, window, cx| {
            bar.begin_editing(None, window, cx);
            bar.input.update(cx, |input, cx| {
                input.set_value(format!("{}home{}Doc", sep(), sep()), window, cx)
            });
        });
        cx.run_until_parked();
        bar.update_in(cx, |bar, window, cx| bar.accept_suggestion(window, cx));
        let text = bar.update(cx, |bar, cx| bar.text(cx));
        assert!(
            text.contains("Documents") && text.ends_with(std::path::MAIN_SEPARATOR),
            "completed with trailing separator, got {text:?}"
        );
    }

    #[gpui::test]
    async fn confirm_navigates_to_directories_and_rejects_files(cx: &mut TestAppContext) {
        let (bar, cx) = setup(cx);
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        bar.update(cx, |_, cx| {
            cx.subscribe(&cx.entity(), move |_, _, event: &AddressBarEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
            .detach();
        });

        // A directory navigates.
        bar.update_in(cx, |bar, window, cx| {
            bar.begin_editing(None, window, cx);
            bar.input.update(cx, |input, cx| {
                input.set_value(format!("{}home{}Documents", sep(), sep()), window, cx)
            });
            bar.confirm(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            events.borrow().as_slice(),
            &[AddressBarEvent::Navigated(PathBuf::from(format!(
                "{}home{}Documents",
                sep(),
                sep()
            )))]
        );

        // A file shows an inline error and does not navigate.
        bar.update_in(cx, |bar, window, cx| {
            bar.input.update(cx, |input, cx| {
                input.set_value(format!("{}home{}readme.md", sep(), sep()), window, cx)
            });
            bar.confirm(cx);
        });
        cx.run_until_parked();
        bar.update(cx, |bar, _| {
            assert_eq!(bar.error.as_deref(), Some("Not a folder"));
        });
        assert_eq!(events.borrow().len(), 1, "no second navigation");

        // A missing path errors too.
        bar.update_in(cx, |bar, window, cx| {
            bar.input.update(cx, |input, cx| {
                input.set_value(format!("{}nope", sep()), window, cx)
            });
            bar.confirm(cx);
        });
        cx.run_until_parked();
        bar.update(cx, |bar, _| {
            assert_eq!(bar.error.as_deref(), Some("Path does not exist"));
        });
    }

    #[gpui::test]
    async fn escape_emits_cancelled(cx: &mut TestAppContext) {
        let (bar, cx) = setup(cx);
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        bar.update(cx, |_, cx| {
            cx.subscribe(&cx.entity(), move |_, _, event: &AddressBarEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
            .detach();
        });
        bar.update(cx, |bar, cx| bar.cancel(cx));
        assert_eq!(events.borrow().as_slice(), &[AddressBarEvent::Cancelled]);
    }

    #[test]
    fn split_for_completion_shapes() {
        let s = std::path::MAIN_SEPARATOR;
        let (parent, prefix) = split_for_completion(&format!("{s}home{s}Do"));
        assert_eq!(parent, Some(PathBuf::from(format!("{s}home"))));
        assert_eq!(prefix, "Do");
        let (parent, prefix) = split_for_completion(&format!("{s}home{s}"));
        assert_eq!(parent, Some(PathBuf::from(format!("{s}home{s}"))));
        assert_eq!(prefix, "");
        assert_eq!(split_for_completion(""), (None, String::new()));
    }

    use std::cell::RefCell;
    use std::rc::Rc;

    fn sep() -> char {
        std::path::MAIN_SEPARATOR
    }
}
