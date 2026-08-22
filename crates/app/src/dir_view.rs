//! The directory view (ARCHITECTURE.md §2 `DirView`, §4a data flow).
//!
//! Owns the cursor/selection (single-select for M1, **path-keyed** per §2 so
//! it survives re-sorts and watcher patches) and renders the owning pane's
//! current [`ListingSnapshot`] as the details list (`views/details_list.rs`).
//! Handles `OpenSelected` (folder → `DirViewEvent::NavigateTo`, which the
//! pane turns into navigation; file → the [`crate::app_state::Opener`] stub),
//! cursor movement, and type-ahead (§0: printable characters are *not* an
//! action — they arrive via `on_key_down` fallthrough when no binding
//! matched; the reset delay runs on [`fs_core::Spawner::timer`] so tests use
//! fake time).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fs_core::{EntryId, FileEntry, ListingSnapshot};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render,
    ScrollStrategy, Task, UniformListScrollHandle, WeakEntity, Window, div, point, prelude::*, px,
};

use crate::actions::{
    ExtendSelectionNext, ExtendSelectionPrev, OpenSelected, PageDown, PageUp, SelectAll,
    SelectFirst, SelectLast, SelectNext, SelectPrev,
};
use crate::app_state::FsContext;
use crate::pane::Pane;
use crate::theme::Theme;
use crate::views::details_list;

/// Quiet period after which the type-ahead prefix resets. Every keystroke
/// restarts it (the previous timer task is dropped, cancelling it).
pub const TYPE_AHEAD_TIMEOUT: Duration = Duration::from_millis(1000);

/// Rows to move on PageUp/PageDown when the list has not been laid out yet.
const FALLBACK_PAGE_ROWS: usize = 20;

/// Events up (ARCHITECTURE.md §2): the pane subscribes and navigates.
pub enum DirViewEvent {
    /// A folder was opened (Enter / double-click).
    NavigateTo(PathBuf),
}

pub struct DirView {
    focus_handle: FocusHandle,
    theme: Theme,
    pane: WeakEntity<Pane>,
    /// Path-keyed cursor = the single selection in M1. The full
    /// `SelectionModel` (multi/range/marquee) lands at M3.
    cursor: Option<EntryId>,
    scroll_handle: UniformListScrollHandle,
    /// Pending type-ahead prefix (lowercased on match, kept as typed).
    type_ahead: String,
    /// Dropping this cancels the pending reset — replacing it on every
    /// keystroke is what makes the timeout restart.
    _type_ahead_reset: Option<Task<()>>,
}

impl DirView {
    pub fn new(theme: Theme, pane: WeakEntity<Pane>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            theme,
            pane,
            cursor: None,
            scroll_handle: UniformListScrollHandle::new(),
            type_ahead: String::new(),
            _type_ahead_reset: None,
        }
    }

    // ------------------------------------------------------------------
    // Cursor (single-select M1)
    // ------------------------------------------------------------------

    pub fn cursor(&self) -> Option<&EntryId> {
        self.cursor.as_ref()
    }

    pub fn set_cursor(&mut self, cursor: Option<EntryId>, cx: &mut Context<Self>) {
        self.cursor = cursor;
        cx.notify();
    }

    /// The pane's current snapshot — the DirView renders the pane's listing
    /// (ARCHITECTURE.md §4a); it holds no copy of its own.
    fn snapshot(&self, cx: &App) -> Option<Arc<ListingSnapshot>> {
        self.pane
            .upgrade()
            .and_then(|pane| pane.read(cx).snapshot().cloned())
    }

    fn cursor_ix(&self, snapshot: &ListingSnapshot) -> Option<usize> {
        let cursor = self.cursor.as_ref()?;
        snapshot.entries.iter().position(|e| e.id() == *cursor)
    }

    /// Move the cursor to `ix` and keep it visible (§8: `scroll_to_item` on
    /// every cursor move).
    fn move_cursor_to(&mut self, ix: usize, snapshot: &ListingSnapshot, cx: &mut Context<Self>) {
        let Some(entry) = snapshot.entries.get(ix) else {
            return;
        };
        self.cursor = Some(entry.id());
        self.scroll_handle
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn step_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot(cx) else {
            return;
        };
        let len = snapshot.entries.len();
        if len == 0 {
            return;
        }
        let ix = match self.cursor_ix(&snapshot) {
            Some(ix) => (ix as isize + delta).clamp(0, len as isize - 1) as usize,
            // No cursor yet: any downward motion lands on the first row, any
            // upward motion on the last.
            None if delta >= 0 => 0,
            None => len - 1,
        };
        self.move_cursor_to(ix, &snapshot, cx);
    }

    fn move_cursor_to_end(&mut self, first: bool, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot(cx) else {
            return;
        };
        let len = snapshot.entries.len();
        if len == 0 {
            return;
        }
        let ix = if first { 0 } else { len - 1 };
        self.move_cursor_to(ix, &snapshot, cx);
    }

    /// Rows in one page: derived from the laid-out viewport when available.
    fn rows_per_page(&self) -> usize {
        let viewport = self
            .scroll_handle
            .0
            .borrow()
            .base_handle
            .bounds()
            .size
            .height;
        let rows = (f32::from(viewport) / details_list::ROW_HEIGHT) as usize;
        if rows == 0 { FALLBACK_PAGE_ROWS } else { rows }
    }

    // ------------------------------------------------------------------
    // Open (§0 "Open item": Enter / double-click)
    // ------------------------------------------------------------------

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot(cx) else {
            return;
        };
        let Some(ix) = self.cursor_ix(&snapshot) else {
            return;
        };
        let entry = snapshot.entries[ix].clone();
        self.open_entry(&entry, cx);
    }

    /// Folder → navigation event to the pane; file → the opener stub.
    pub(crate) fn open_entry(&mut self, entry: &FileEntry, cx: &mut Context<Self>) {
        if entry.is_dir_like() {
            cx.emit(DirViewEvent::NavigateTo(entry.path.to_path_buf()));
        } else {
            FsContext::global(cx).opener.open(&entry.path);
        }
    }

    /// Row single-click (details list): select, path-keyed.
    pub(crate) fn select_entry(&mut self, entry: &FileEntry, cx: &mut Context<Self>) {
        self.set_cursor(Some(entry.id()), cx);
    }

    // ------------------------------------------------------------------
    // Type-ahead (§0: on_key_down fallthrough, not an action)
    // ------------------------------------------------------------------

    fn handle_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        // Only bare printable input (shift/capitals allowed) is type-ahead;
        // anything else belongs to key bindings.
        if modifiers.platform || modifiers.control || modifiers.alt || modifiers.function {
            return;
        }
        let Some(typed) = keystroke.key_char.as_deref() else {
            return;
        };
        if typed.is_empty() || typed.chars().any(char::is_control) {
            return;
        }
        self.type_ahead.push_str(typed);
        self.jump_to_type_ahead_match(cx);

        // Restart the reset timer: dropping the previous task cancels it, and
        // Spawner::timer runs on fake time under #[gpui::test].
        let spawner = FsContext::global(cx).spawner.clone();
        self._type_ahead_reset = Some(cx.spawn(async move |this, cx| {
            spawner.timer(TYPE_AHEAD_TIMEOUT).await;
            this.update(cx, |this, _| this.type_ahead.clear()).ok();
        }));
    }

    /// Jump to the next entry whose name starts with the typed prefix
    /// (case-insensitive). A fresh single-character prefix searches *after*
    /// the cursor so repeated letters cycle through matches; a longer prefix
    /// keeps refining from the current row. Wraps around.
    fn jump_to_type_ahead_match(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot(cx) else {
            return;
        };
        let len = snapshot.entries.len();
        if len == 0 {
            return;
        }
        let prefix = self.type_ahead.to_lowercase();
        let fresh = prefix.chars().count() == 1;
        let start = match self.cursor_ix(&snapshot) {
            Some(ix) if fresh => ix + 1,
            Some(ix) => ix,
            None => 0,
        };
        for offset in 0..len {
            let ix = (start + offset) % len;
            if snapshot.entries[ix]
                .name
                .to_lowercase()
                .starts_with(&prefix)
            {
                self.move_cursor_to(ix, &snapshot, cx);
                return;
            }
        }
    }

    /// Pending type-ahead prefix (test observability).
    pub fn type_ahead(&self) -> &str {
        &self.type_ahead
    }

    // ------------------------------------------------------------------
    // Restore support (pane NavEntry semantics)
    // ------------------------------------------------------------------

    /// Best-effort application of a restored scroll offset to the list.
    /// The pane keeps the bookkeeping value (`NavEntry.scroll_top`); this
    /// pushes it into the scroll handle so the paint lands scrolled.
    pub(crate) fn apply_scroll_top(&mut self, scroll_top: f32) {
        self.scroll_handle
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.0), px(-scroll_top)));
    }

    pub(crate) fn theme(&self) -> &Theme {
        &self.theme
    }

    pub(crate) fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll_handle
    }

    pub(crate) fn focus_handle_ref(&self) -> &FocusHandle {
        &self.focus_handle
    }
}

impl EventEmitter<DirViewEvent> for DirView {}

impl Focusable for DirView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DirView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let pane = self.pane.upgrade();
        let snapshot = pane.as_ref().and_then(|p| p.read(cx).snapshot().cloned());
        let sort = pane.as_ref().map(|p| p.read(cx).sort()).unwrap_or_default();
        let load_error = pane
            .as_ref()
            .and_then(|p| p.read(cx).load_error().map(str::to_string));

        let body: gpui::AnyElement = if let Some(error) = load_error {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child(format!("Can't read folder: {error}"))
                .into_any_element()
        } else if let Some(snapshot) = snapshot.filter(|s| !s.entries.is_empty()) {
            details_list::render_rows(self, snapshot, cx).into_any_element()
        } else {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child("Empty folder")
                .into_any_element()
        };

        div()
            .track_focus(&self.focus_handle)
            .key_context("DirView")
            .on_action(cx.listener(|this, _: &OpenSelected, _, cx| this.open_selected(cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step_cursor(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.step_cursor(-1, cx)))
            .on_action(
                cx.listener(|this, _: &SelectFirst, _, cx| this.move_cursor_to_end(true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectLast, _, cx| this.move_cursor_to_end(false, cx)),
            )
            // M1 is single-select: extending just moves the cursor until the
            // full SelectionModel lands at M3.
            .on_action(cx.listener(|this, _: &ExtendSelectionNext, _, cx| this.step_cursor(1, cx)))
            .on_action(cx.listener(|this, _: &ExtendSelectionPrev, _, cx| this.step_cursor(-1, cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                this.step_cursor(this.rows_per_page() as isize, cx)
            }))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                this.step_cursor(-(this.rows_per_page() as isize), cx)
            }))
            // Single-select M1: select-all is a no-op until the M3
            // SelectionModel; bound here so the keystroke is owned.
            .on_action(cx.listener(|_, _: &SelectAll, _, _| {}))
            .on_key_down(cx.listener(Self::handle_key_down))
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(details_list::render_header(&theme, sort, cx))
            .child(body)
    }
}
