//! Toolbar search (ARCHITECTURE.md §0 "Search field focus", §8 TextInput
//! "Reused by address bar, rename, search…", M6a).
//!
//! Two halves, same file, same split as [`crate::rename`] and
//! [`crate::marquee`]: a small [`SearchBar`] entity (the vendored
//! [`InputState`] behind a magnifier glyph, top-right of the pane's chrome
//! row) and a [`SearchState`] machine that lives as a **field of the
//! [`Pane`]** — one search per pane, like every other pane-scoped thing since
//! M4 — whose methods are the `impl Pane` block at the bottom of this file.
//!
//! Behavior, Explorer's:
//!
//! * Typing filters the **open folder** instantly through
//!   [`fs_core::filter_snapshot`], which is pure: no stat, no `read_dir`,
//!   nothing off-thread, so it runs inside the keystroke.
//! * "Subfolders" turns the same query into a recursive
//!   [`fs_core::search_recursive`] walk whose hits stream in. The walk is
//!   polled on the **background executor** and its events cross to the UI
//!   thread through a channel that the foreground task drains in
//!   [`SEARCH_THROTTLE`]-wide batches — a 50k-hit tree repaints ten times a
//!   second, not fifty thousand times.
//! * One cancellable `Task` slot ([`Pane::_search_task`]) holds that pump, so
//!   replacing it on a query change, clearing the search, or navigating away
//!   cancels the walk by dropping it (the same single-slot shape as
//!   [`crate::info_panel`] and [`crate::thumbnails`]).
//! * `escape` (or emptying the field) clears the search and restores the
//!   unfiltered listing; `cmd-f` focuses the field (§0 `FocusSearch`, handled
//!   by the workspace, forwarded to the active pane).
//!
//! The results themselves are not a second listing: they become the
//! [`crate::dir_view::DirView`]'s projection, so the marquee, drag & drop, the
//! context menu, the icon grid, thumbnails and the scrollbar all keep working
//! against them with no knowledge that a search is on.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fs_core::{
    FileEntry, ListingSnapshot, SearchEvent, SearchQuery, SortSpec, filter_snapshot,
    search_recursive,
};
use futures::StreamExt as _;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Subscription, Window, div, prelude::*, px,
};

use crate::actions::{Cancel, Confirm};
use crate::app_state::FsContext;
use crate::input::text_input as ti;
use crate::input::{InputEvent, InputState};
use crate::pane::Pane;
use crate::theme::Theme;

/// How long arriving [`SearchEvent`]s pile up before one batch is folded into
/// the pane and repainted. Runs on [`fs_core::Spawner::timer`], so
/// `#[gpui::test]`s drive it with `advance_clock`.
///
/// 100 ms is the same cadence as the directory watcher's debounce
/// ([`crate::pane::WATCH_LATENCY`]): a search of a large tree is a firehose,
/// and the alternative — notifying per hit — repaints once per file found.
pub const SEARCH_THROTTLE: Duration = Duration::from_millis(100);

/// Placeholder text of the empty field (plan §2's blueprint screenshot).
pub const SEARCH_PLACEHOLDER: &str = "Search";

/// How many recursive hits one search accumulates before it stops taking more.
///
/// Everything downstream of `hits` is per-batch work on the **UI thread**: the
/// rows are rebuilt, de-duplicated and re-sorted every throttle window, so an
/// uncapped `hits` over (say) a home folder full of `e` turns a 100 ms window
/// into seconds of sorting and the window stops painting — including the
/// keystroke that would have cancelled the search. Explorer's own answer is the
/// same one: report the first N and say so. Ten thousand rows is far past what
/// anyone scrolls and still sorts in a few milliseconds.
pub const MAX_SEARCH_HITS: usize = 10_000;

// ----------------------------------------------------------------------
// The widget
// ----------------------------------------------------------------------

/// What the field reports to the owning [`Pane`].
#[derive(Debug, Clone, PartialEq)]
pub enum SearchBarEvent {
    /// The text changed. The empty string means "no search" — the pane
    /// restores the unfiltered listing.
    Changed(String),
    /// The "Subfolders" toggle was clicked.
    RecursiveToggled(bool),
    /// `enter`: the user is done typing and wants to work with the results, so
    /// focus moves to them (Explorer's behavior — the query stays live).
    Submitted,
    /// `escape`: clear the search and hand focus back to the pane.
    Dismissed,
}

/// The toolbar search field: magnifier glyph, [`InputState`], a clear button,
/// and the "Subfolders" toggle that appears once there is something to search
/// for.
pub struct SearchBar {
    theme: Theme,
    input: Entity<InputState>,
    recursive: bool,
    /// A clear staged by [`Self::reset`], applied by the next paint.
    ///
    /// The vendored [`InputState`] cannot rewrite its own text without a
    /// `&mut Window` (it re-runs the input handler's replace path), and the
    /// pane clears the field from `Pane::load` — a window-free code path that
    /// every navigation goes through. So the pane stages the clear and
    /// [`Render`], which does have a window, performs it.
    pending_reset: bool,
    _input_subscription: Subscription,
}

impl EventEmitter<SearchBarEvent> for SearchBar {}

impl SearchBar {
    pub fn new(theme: Theme, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(cx)
                .input_type(ti::InputType::Search)
                .placeholder(SEARCH_PLACEHOLDER)
                .with_colors(theme.muted, theme.accent, theme.accent.opacity(0.25))
        });
        let subscription = cx.subscribe(&input, Self::on_input_event);
        Self {
            theme,
            input,
            recursive: false,
            pending_reset: false,
            _input_subscription: subscription,
        }
    }

    /// `cmd-f`: focus the field with its current text selected, so typing
    /// replaces a previous query (same rule as the address bar).
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.select_all(&ti::SelectAll, window, cx);
        });
        let handle = self.input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).content().to_string()
    }

    /// Whether the field holds anything — including text whose clear is only
    /// staged, which still has to be flushed.
    pub fn has_text(&self, cx: &App) -> bool {
        self.pending_reset || !self.input.read(cx).content().is_empty()
    }

    pub fn recursive(&self) -> bool {
        self.recursive
    }

    /// Type `text` into the field, exactly as a keystroke would: the input's
    /// `Change` event carries it to the pane. Used by tests and the visual
    /// scenarios, so what they drive is the real wiring rather than the pane's
    /// setter with an empty-looking field beside it.
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_reset = false;
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        cx.notify();
    }

    /// Click "Subfolders" to a known state (same driver rationale as
    /// [`Self::set_text`]).
    pub fn set_recursive(&mut self, recursive: bool, cx: &mut Context<Self>) {
        if self.recursive != recursive {
            self.toggle_recursive(cx);
        }
    }

    /// Stage an empty field — the pane calls this when *it* drops the search
    /// (navigation, `escape`, the clear button). See [`Self::pending_reset`]
    /// for why it is staged rather than applied here.
    ///
    /// The applied clear does emit [`InputEvent::Change`] and so a
    /// [`SearchBarEvent::Changed("")`](SearchBarEvent::Changed); the pane's
    /// handler is idempotent for the empty string (it clears a search that is
    /// already gone), so the round trip is harmless and the field stays the
    /// one place that knows the text.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.recursive = false;
        self.pending_reset = !self.input.read(cx).content().is_empty();
        cx.notify();
    }

    fn on_input_event(
        &mut self,
        _input: Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        // As in `address_bar.rs`: the vendored input also emits Enter/Blur,
        // but our keymap dispatches Confirm/Cancel in the TextInput context
        // (wired in `render`) — Change is the only event consumed here.
        if let InputEvent::Change = event {
            let text = self.input.read(cx).content().to_string();
            cx.emit(SearchBarEvent::Changed(text));
        }
    }

    fn toggle_recursive(&mut self, cx: &mut Context<Self>) {
        self.recursive = !self.recursive;
        cx.emit(SearchBarEvent::RecursiveToggled(self.recursive));
        cx.notify();
    }
}

impl Focusable for SearchBar {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for SearchBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        // The staged clear, applied now that there is a window (see
        // `pending_reset`). Before `has_query` is read, so the frame that
        // clears the text also drops the clear button and the toggle.
        if std::mem::take(&mut self.pending_reset) {
            self.input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        let has_query = !self.input.read(cx).content().is_empty();
        let recursive = self.recursive;

        let input_focus = self.input.read(cx).focus_handle(cx);
        let field = div()
            // `TextInput` carries Confirm/Cancel and the vendored editing
            // actions. The **input's own** focus handle is tracked on this
            // node, exactly as `rename::with_editor_actions` does: the
            // vendored `InputState::render` does not track it itself, so
            // without this the node carrying `key_context` is not on the
            // focused element's dispatch path and every binding in it is
            // silently dead (§9's named failure mode).
            .track_focus(&input_focus)
            .key_context("TextInput")
            .on_action(cx.listener(|_, _: &Confirm, _, cx| cx.emit(SearchBarEvent::Submitted)))
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(SearchBarEvent::Dismissed)))
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
            .gap(px(4.0))
            // 180 px when the chrome row has it to give, down to 90 in a narrow
            // split pane rather than overflowing it (the breadcrumb beside it
            // shrinks too — see `Pane::render_chrome_row`).
            .w(px(180.0))
            .min_w(px(90.0))
            .flex_shrink_1()
            .h(px(22.0))
            .px(px(6.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(if has_query {
                theme.accent
            } else {
                theme.border
            })
            .bg(theme.surface)
            .text_size(px(12.0))
            .child(
                div()
                    .flex_none()
                    .text_color(theme.muted)
                    .child(SharedString::new_static("⌕")),
            )
            .child(div().flex_1().min_w(px(0.0)).child(self.input.clone()))
            .when(has_query, |el| {
                el.child(
                    div()
                        .id("search-clear")
                        .debug_selector(|| "search-clear".to_string())
                        .flex_none()
                        .cursor_pointer()
                        .text_color(theme.muted)
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(SearchBarEvent::Dismissed);
                        }))
                        .child(SharedString::new_static("✕")),
                )
            });

        div()
            .flex()
            .items_center()
            // Shrinkable, with the field's own `min_w` as the floor: in a
            // narrow split pane the row has to give the space to *something*,
            // and the breadcrumb has already given all it has.
            .flex_shrink_1()
            .min_w(px(0.0))
            .gap(px(6.0))
            .ml(px(8.0))
            .child(field)
            // Explorer's "Search subfolders". Mouse-only and deliberately not
            // an action, on the same precedent as the info panel's section
            // headers (§0): it is this widget's own presentation state, with no
            // keymap row, menu item or other dispatcher wanting it. It appears
            // only once there is a query, so the idle toolbar stays the
            // blueprint's single field.
            .when(has_query, |el| {
                el.child(
                    div()
                        .id("search-subfolders")
                        .debug_selector(|| "search-subfolders".to_string())
                        .flex_none()
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(if recursive { theme.text } else { theme.muted })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_recursive(cx)))
                        .child(SharedString::new_static(if recursive {
                            "☑ Subfolders"
                        } else {
                            "☐ Subfolders"
                        })),
                )
            })
    }
}

// ----------------------------------------------------------------------
// The pane's search state
// ----------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// How many times [`SearchState::rebuild_rows`] has run on this thread —
    /// the same shape as `dir_view`'s `PROJECTIONS_BUILT`. Rebuilding is the
    /// per-batch cost that scales with the result set, so "once per batch" is
    /// a property worth pinning rather than rediscovering in a profile.
    static ROWS_REBUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One pane's live search (a field of [`Pane`], `None` when nothing is being
/// searched for).
pub struct SearchState {
    query: SearchQuery,
    /// Whether the recursive walk is on ("Subfolders").
    recursive: bool,
    /// Recursive hits accumulated so far, in the walk's breadth-first arrival
    /// order. Empty while the search is folder-local.
    hits: Vec<FileEntry>,
    /// The rows the projection renders: the instant local filter of the open
    /// folder, plus every `hits` entry that filter did not already contain,
    /// in the pane's sort order. An `Arc` because the projection, the
    /// selection pruning and the status line all read it per frame.
    rows: Arc<[FileEntry]>,
    /// [`SearchEvent::Progress`] — directories the walk has read.
    dirs_scanned: usize,
    /// [`SearchEvent::Skipped`] — things the walk could not look at: a
    /// directory it could not read or descend, or an entry it could not stat.
    /// One per event, so it counts *reports*, not distinct paths.
    skipped: usize,
    /// Whether [`MAX_SEARCH_HITS`] was reached and hits are being dropped.
    truncated: bool,
    /// True from starting a recursive walk until its `Done` (or its
    /// cancellation).
    running: bool,
}

impl SearchState {
    fn new(query: SearchQuery) -> Self {
        Self {
            query,
            recursive: false,
            hits: Vec::new(),
            rows: Arc::from(Vec::new()),
            dirs_scanned: 0,
            skipped: 0,
            truncated: false,
            running: false,
        }
    }

    pub fn query(&self) -> &SearchQuery {
        &self.query
    }

    pub fn recursive(&self) -> bool {
        self.recursive
    }

    /// The result rows, cheap to clone (see [`Self::rows`]).
    pub fn rows(&self) -> Arc<[FileEntry]> {
        self.rows.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Rebuild [`Self::rows`] from a snapshot plus whatever the walk has found.
    ///
    /// Called on every query change **and** on every snapshot swap (fresh
    /// load, refresh, sort flip, hidden-files toggle, watcher patch) — which is
    /// what stops an externally created file that does not match the query
    /// from appearing in the results, and what keeps the results sorted like
    /// the listing they replace.
    fn rebuild_rows(&mut self, snapshot: Option<&ListingSnapshot>, sort: SortSpec) {
        #[cfg(test)]
        ROWS_REBUILT.with(|count| count.set(count.get() + 1));
        let mut rows: Vec<FileEntry> = Vec::new();
        if let Some(snapshot) = snapshot {
            // The pinned-pure filter gives the matching ids **in snapshot
            // order**, so walking ids and entries together resolves them back
            // to entries in one pass with no lookup structure — the keystroke
            // path allocates the id vector and nothing else.
            let ids = filter_snapshot(snapshot, &self.query);
            let mut wanted = ids.iter();
            let mut next = wanted.next();
            for entry in snapshot.entries.iter() {
                if next == Some(&entry.id()) {
                    rows.push(entry.clone());
                    next = wanted.next();
                }
            }
        }
        if !self.hits.is_empty() {
            // A recursive walk re-reports the open folder's own matches (its
            // root is level one), so the instant local rows win and the hit is
            // dropped rather than shown twice.
            let mut seen: HashSet<Arc<Path>> =
                rows.iter().map(|entry| entry.path.clone()).collect();
            for hit in &self.hits {
                if seen.insert(hit.path.clone()) {
                    rows.push(hit.clone());
                }
            }
            // Breadth-first arrival order is what made the *streaming* useful;
            // presentation follows the pane's sort column like every other
            // row list (Explorer sorts search results too).
            rows.sort_by(|a, b| sort.compare(a, b));
        }
        self.rows = Arc::from(rows);
    }

    /// The status line while a search is live (plan §3's line, search flavor).
    fn status_text(&self) -> String {
        let count = self.rows.len();
        let mut text = format!(
            "{count} result{} for \u{201c}{}\u{201d}",
            if count == 1 { "" } else { "s" },
            self.query.text()
        );
        if self.recursive {
            text.push_str(&format!(
                " \u{b7} {} folder{} {}",
                self.dirs_scanned,
                if self.dirs_scanned == 1 { "" } else { "s" },
                if self.running {
                    "scanned so far\u{2026}"
                } else {
                    "searched"
                }
            ));
            if self.skipped > 0 {
                text.push_str(&format!(" \u{b7} {} skipped", self.skipped));
            }
            if self.truncated {
                text.push_str(&format!(
                    " \u{b7} showing the first {MAX_SEARCH_HITS} \u{2014} narrow the search"
                ));
            }
        }
        text
    }
}

/// The label a search result row shows beside its name when the hit does not
/// live in the folder being searched: the containing directory relative to
/// that folder. `None` for hits in the open folder itself, which is where an
/// unqualified name already means the right thing.
///
/// Explorer devotes a whole "Folder" column to this; we render it inline
/// beside the name (see `docs/AS_BUILT.md` — the column is a recorded gap).
pub fn search_parent_label(root: Option<&Path>, entry: &FileEntry) -> Option<SharedString> {
    let parent = entry.path.parent()?;
    let root = root?;
    if parent == root {
        return None;
    }
    let relative = parent.strip_prefix(root).unwrap_or(parent);
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(SharedString::from(relative.display().to_string()))
}

impl Pane {
    /// §0 `FocusSearch` (`cmd-f`), forwarded here by the workspace.
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let bar = self.search_bar().clone();
        bar.update(cx, |bar, cx| bar.focus(window, cx));
        cx.notify();
    }

    /// Set (or clear) the query. Blank text is *no search*
    /// ([`SearchQuery::new`] returns `None`), which restores the unfiltered
    /// listing.
    pub fn set_search_text(&mut self, text: &str, cx: &mut Context<Self>) {
        match SearchQuery::new(text) {
            None => self.clear_search(cx),
            Some(query) => {
                // Same needle as the live search: nothing to redo, and in
                // particular no reason to restart a running walk (the field
                // emits Change for cursor-only edits too).
                if self
                    .search
                    .as_ref()
                    .is_some_and(|search| *search.query() == query)
                {
                    return;
                }
                // "Subfolders" is sticky and lives on the pane, not inside the
                // query being replaced (`Pane::search_recursive`). That is what
                // makes the two events order-independent: the field's text
                // reaches the pane one flush *later* than its toggle (the text
                // travels InputState -> SearchBar -> Pane), so a toggle
                // arriving while `search` is still `None` must be remembered
                // rather than dropped — dropping it is exactly what made the
                // first `search_results` capture show a lit checkbox and zero
                // results.
                let recursive = self.search_recursive;
                let mut state = SearchState::new(query);
                state.recursive = recursive;
                self.search = Some(state);
                self.restart_search(cx);
            }
        }
    }

    /// Toggle "Subfolders" on the live search.
    pub fn set_search_recursive(&mut self, recursive: bool, cx: &mut Context<Self>) {
        if self.search_recursive == recursive {
            return;
        }
        self.search_recursive = recursive;
        let Some(search) = self.search.as_mut() else {
            return; // remembered for the next query
        };
        search.recursive = recursive;
        self.restart_search(cx);
    }

    /// Drop the search entirely and restore the unfiltered listing. Cancels a
    /// running walk by dropping its task.
    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        // Both halves of the scope, before the early return. The pane owns it,
        // but the field renders the checkbox from its own mirror, so resetting
        // one without the other leaves a lit "☑ Subfolders" over the *next*
        // query's folder-local filter — and a first click on it that does
        // nothing, because the pane already thinks it is off. The scope can
        // also have been toggled with no query typed yet, which is why this
        // sits above the early return.
        self.search_recursive = false;
        self.search_bar()
            .clone()
            .update(cx, |bar, cx| bar.reset(cx));
        if self.search.is_none() {
            return;
        }
        self.search = None;
        self.search_generation += 1;
        self._search_task = None;
        self.after_search_change(cx);
    }

    /// Navigation away from the searched folder drops the search — Explorer's
    /// rule, and the only coherent one: the results are *of* a folder, and the
    /// walk under it is no longer what the user is looking at. The field is
    /// emptied with it, so the toolbar cannot claim a filter that is gone.
    pub(crate) fn cancel_search_for_navigation(&mut self, cx: &mut Context<Self>) {
        let bar = self.search_bar().clone();
        if self.search.is_none() && !bar.read(cx).has_text(cx) && !self.search_recursive {
            return;
        }
        // `clear_search` empties the field and resets its checkbox with it.
        self.clear_search(cx);
    }

    /// The live search's result rows, or `None` when no search is on — the
    /// [`crate::dir_view::DirView`]'s projection source and the set every
    /// gesture is pruned against.
    pub(crate) fn search_rows(&self) -> Option<Arc<[FileEntry]>> {
        self.search.as_ref().map(SearchState::rows)
    }

    pub fn search(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    /// The status line's search half, `None` when nothing is being searched.
    pub(crate) fn search_status_text(&self) -> Option<String> {
        self.search.as_ref().map(SearchState::status_text)
    }

    /// Re-derive the result rows from the pane's current snapshot. Called from
    /// [`Pane::prune_view_state`], i.e. after **every** snapshot swap, before
    /// anything prunes the selection against the rows.
    pub(crate) fn refresh_search_rows(&mut self) {
        let snapshot = self.snapshot().cloned();
        let sort = self.sort();
        if let Some(search) = self.search.as_mut() {
            search.rebuild_rows(snapshot.as_deref(), sort);
        }
    }

    /// A query change: re-derive the instant local rows, then (re)start the
    /// recursive walk if "Subfolders" is on. The single `Task` slot is
    /// replaced either way, so the previous walk is cancelled here even when
    /// the new search is folder-local.
    pub(crate) fn restart_search(&mut self, cx: &mut Context<Self>) {
        self.search_generation += 1;
        self._search_task = None;
        if let Some(search) = self.search.as_mut() {
            // Everything the *previous* walk accumulated is stale by
            // definition: whatever changed (the query, the scope, the pane's
            // `show_hidden` — all three are arguments to the walk) changes what
            // counts as a hit. Keeping them is how a hidden hit survived the
            // hidden-files toggle that hid every other trace of it, and how
            // "N folders searched" ended up summing two walks.
            search.hits.clear();
            search.dirs_scanned = 0;
            search.skipped = 0;
            // Cleared here, set again below if a walk is really spawned: a
            // cancelled walk that is not replaced (Subfolders off mid-flight)
            // must not leave `is_running()` waiting for a `Done` that can
            // never arrive.
            search.running = false;
            search.truncated = false;
        }
        if self
            .search
            .as_ref()
            .is_some_and(|search| search.recursive && self.path().is_some())
        {
            self.spawn_recursive_search(cx);
        }
        self.after_search_change(cx);
    }

    /// The recursive walk, streamed.
    ///
    /// **Nothing here runs on the UI thread**: `search_recursive`'s stream is
    /// polled inside `cx.background_spawn`, and the only work the foreground
    /// task does is park on a [`fs_core::Spawner::timer`], drain a channel and
    /// fold the batch in. Both tasks live inside the one slot — the background
    /// half is held on the foreground task's stack, so dropping the slot drops
    /// it and the walk stops.
    fn spawn_recursive_search(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.path().map(Path::to_path_buf) else {
            return;
        };
        let Some(query) = self.search.as_ref().map(|s| s.query().clone()) else {
            return;
        };
        let show_hidden = self.show_hidden();
        let fs = FsContext::global(cx);
        let vfs = fs.vfs.clone();
        let spawner = fs.spawner.clone();
        let generation = self.search_generation;
        if let Some(search) = self.search.as_mut() {
            search.running = true;
        }

        self._search_task = Some(cx.spawn(async move |this, cx| {
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            // Held, never detached: dropping this task drops the walk with it.
            let _walk = cx.background_spawn(async move {
                let mut stream = search_recursive(vfs, root, query, show_hidden);
                while let Some(event) = stream.next().await {
                    if tx.unbounded_send(event).is_err() {
                        break; // the pane stopped listening
                    }
                }
            });

            // Park until *something* arrives, then let the firehose pile up for
            // one throttle window and fold the whole pile in at once. One
            // repaint per window, however many hits it holds.
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                spawner.timer(SEARCH_THROTTLE).await;
                // Drain whatever piled up during the window, without parking:
                // `Err` here means "nothing queued right now" *or* "the walk
                // finished", and the next `rx.next().await` tells the two
                // apart by returning `None`.
                while let Ok(event) = rx.try_recv() {
                    batch.push(event);
                }
                if this
                    .update(cx, |this, cx| {
                        this.apply_search_batch(generation, batch, cx)
                    })
                    .is_err()
                {
                    return; // pane dropped
                }
            }
        }));
    }

    /// Fold one throttled batch of [`SearchEvent`]s into the search state.
    fn apply_search_batch(
        &mut self,
        generation: u64,
        batch: Vec<SearchEvent>,
        cx: &mut Context<Self>,
    ) {
        // Belt and braces beside the task slot: a batch whose search has been
        // superseded (new query, cleared, navigated) can never apply.
        if generation != self.search_generation {
            return;
        }
        let Some(search) = self.search.as_mut() else {
            return;
        };
        for event in batch {
            match event {
                SearchEvent::Hit(entry) => {
                    // Past the cap the hit is dropped, not queued: see
                    // `MAX_SEARCH_HITS`. The walk itself keeps going (its
                    // progress and its `Skipped` reports still mean something),
                    // it just stops feeding the row list.
                    if search.hits.len() < MAX_SEARCH_HITS {
                        search.hits.push(entry);
                    } else {
                        search.truncated = true;
                    }
                }
                // Coalesced by fs-core (one event per N directories, plus an
                // exact final count), so this is an assignment, not a `+= 1`.
                SearchEvent::Progress { dirs_scanned } => search.dirs_scanned = dirs_scanned,
                SearchEvent::Skipped { .. } => search.skipped += 1,
                SearchEvent::Done => search.running = false,
            }
        }
        // The rows are rebuilt exactly once per batch, by `prune_view_state`
        // inside `after_search_change` — which has to re-derive them anyway
        // before pruning the selection against them. Rebuilding here as well
        // doubled the per-batch sort of the whole result set.
        self.after_search_change(cx);
    }

    /// What every search mutation ends with: the rows changed, so the view's
    /// selection and cursor have to be pruned to them (a hidden row must not
    /// stay actionable) and the pane repainted.
    fn after_search_change(&mut self, cx: &mut Context<Self>) {
        self.prune_view_state(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(super) fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: Arc::from(Path::new(path)),
            name: Arc::from(
                Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
                    .as_str(),
            ),
            kind: fs_core::EntryKind::File,
            size: 1,
            modified: std::time::SystemTime::UNIX_EPOCH,
            created: None,
            hidden: false,
        }
    }

    #[test]
    fn search_parent_labels_only_hits_outside_the_searched_folder() {
        let root = PathBuf::from("/root");
        assert_eq!(
            search_parent_label(Some(&root), &entry("/root/report.pdf")),
            None,
            "a hit in the open folder needs no qualifier"
        );
        assert_eq!(
            search_parent_label(Some(&root), &entry("/root/sub/report.pdf")),
            Some(SharedString::from("sub".to_string()))
        );
        assert_eq!(
            search_parent_label(Some(&root), &entry("/root/a/b/report.pdf")),
            Some(SharedString::from(
                PathBuf::from("a").join("b").display().to_string()
            ))
        );
        // A hit that is somehow not under the root keeps its whole parent
        // rather than rendering as nothing.
        assert!(
            search_parent_label(Some(&root), &entry("/elsewhere/report.pdf")).is_some(),
            "an out-of-tree parent still names itself"
        );
        assert_eq!(search_parent_label(None, &entry("/root/x")), None);
    }

    #[test]
    fn status_text_reports_counts_progress_and_skips() {
        let mut state = SearchState::new(SearchQuery::new("rep").expect("query"));
        state.rows = Arc::from(vec![entry("/root/report.pdf")]);
        assert_eq!(state.status_text(), "1 result for \u{201c}rep\u{201d}");

        state.rows = Arc::from(vec![entry("/root/a"), entry("/root/b")]);
        assert_eq!(state.status_text(), "2 results for \u{201c}rep\u{201d}");

        // Recursive adds progress; a running walk says so, a finished one
        // reports the total it reached.
        state.recursive = true;
        state.running = true;
        state.dirs_scanned = 12;
        assert_eq!(
            state.status_text(),
            "2 results for \u{201c}rep\u{201d} \u{b7} 12 folders scanned so far\u{2026}"
        );
        state.running = false;
        state.skipped = 3;
        assert_eq!(
            state.status_text(),
            "2 results for \u{201c}rep\u{201d} \u{b7} 12 folders searched \u{b7} 3 skipped"
        );
    }
}

#[cfg(test)]
mod gpui_tests {
    //! §9's M6a rows: typing filters the projection with no disk work, blank
    //! restores it, `escape`/navigation clear it, the recursive walk streams
    //! hits in on fake time, rapid query changes leave exactly one walk
    //! running, and a watcher patch cannot unfilter.

    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::pane::{Pane, WATCH_LATENCY};
    use crate::theme::Theme;
    use fs_core::{
        CreateOptions, EntryMeta, FakeVfs, PathEvent, ProgressFn, RemoveOptions, RenameOptions,
        Spawner, TrashId, TrashRestoreError, Vfs, VolumeKey, WatchGuard,
    };
    use futures::stream::BoxStream;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// How long [`RecordingVfs`] parks before each `read_dir`, so a walk can be
    /// caught part-way and cancelled.
    const READ_DELAY: Duration = Duration::from_millis(50);

    /// A [`Vfs`] that counts and delays every `read_dir` and delegates
    /// everything else. The delay is what makes cancellation observable: a walk
    /// dropped between two directory reads stops adding to the count.
    struct RecordingVfs {
        inner: Arc<dyn Vfs>,
        spawner: Arc<dyn Spawner>,
        reads: AtomicUsize,
        paths: Mutex<Vec<std::path::PathBuf>>,
        delay: Duration,
    }

    impl RecordingVfs {
        fn new(inner: Arc<dyn Vfs>, spawner: Arc<dyn Spawner>, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                inner,
                spawner,
                reads: AtomicUsize::new(0),
                paths: Mutex::new(Vec::new()),
                delay,
            })
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }

        fn reset(&self) {
            self.reads.store(0, Ordering::SeqCst);
            self.paths.lock().expect("paths").clear();
        }

        fn read_count_of(&self, path: &str) -> usize {
            self.paths
                .lock()
                .expect("paths")
                .iter()
                .filter(|p| p.as_path() == Path::new(path))
                .count()
        }
    }

    #[async_trait::async_trait]
    impl Vfs for RecordingVfs {
        async fn read_dir(
            &self,
            path: &Path,
        ) -> anyhow::Result<BoxStream<'static, anyhow::Result<fs_core::FileEntry>>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.paths.lock().expect("paths").push(path.to_path_buf());
            if !self.delay.is_zero() {
                self.spawner.timer(self.delay).await;
            }
            self.inner.read_dir(path).await
        }
        async fn metadata(&self, path: &Path) -> anyhow::Result<Option<EntryMeta>> {
            self.inner.metadata(path).await
        }
        async fn create_dir(&self, path: &Path) -> anyhow::Result<()> {
            self.inner.create_dir(path).await
        }
        async fn create_file(&self, path: &Path, opts: CreateOptions) -> anyhow::Result<()> {
            self.inner.create_file(path, opts).await
        }
        async fn copy(&self, from: &Path, to: &Path, on: ProgressFn) -> anyhow::Result<()> {
            self.inner.copy(from, to, on).await
        }
        async fn rename(&self, from: &Path, to: &Path, opts: RenameOptions) -> anyhow::Result<()> {
            self.inner.rename(from, to, opts).await
        }
        async fn remove(&self, path: &Path, opts: RemoveOptions) -> anyhow::Result<()> {
            self.inner.remove(path, opts).await
        }
        async fn trash(&self, path: &Path) -> anyhow::Result<TrashId> {
            self.inner.trash(path).await
        }
        async fn restore(&self, id: TrashId) -> Result<std::path::PathBuf, TrashRestoreError> {
            self.inner.restore(id).await
        }
        async fn load(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
            self.inner.load(path).await
        }
        async fn atomic_write(&self, path: &Path, data: Vec<u8>) -> anyhow::Result<()> {
            self.inner.atomic_write(path, data).await
        }
        fn volume_key(&self, path: &Path) -> VolumeKey {
            self.inner.volume_key(path)
        }
        async fn free_space(&self, path: &Path) -> anyhow::Result<u64> {
            self.inner.free_space(path).await
        }
        fn watch(
            &self,
            path: &Path,
            latency: Duration,
        ) -> (BoxStream<'static, Vec<PathEvent>>, WatchGuard) {
            self.inner.watch(path, latency)
        }
        fn is_fake(&self) -> bool {
            true
        }
    }

    /// The fixture: three directories, one matching name at each level, so
    /// "report" is a local hit, a one-level-down hit and a two-levels-down hit.
    fn fixture(cx: &mut TestAppContext) -> (Arc<FakeVfs>, Arc<dyn Spawner>) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(
            cx.update(|cx| cx.background_executor().clone()),
        ));
        let vfs = FakeVfs::new(spawner.clone());
        vfs.insert_tree(
            "/root",
            json!({
                "report.pdf": "local",
                "notes.txt": "n",
                "sub1": { "report-draft.txt": "d", "other.txt": "o" },
                "sub2": { "misc.txt": "m" },
                ".hidden-report": "h",
            }),
        );
        (vfs, spawner)
    }

    /// Install a pane over the fixture behind a [`RecordingVfs`].
    fn setup(
        cx: &mut TestAppContext,
        delay: Duration,
    ) -> (
        Entity<Pane>,
        Arc<RecordingVfs>,
        Arc<FakeVfs>,
        &mut VisualTestContext,
    ) {
        let (fake, spawner) = fixture(cx);
        let recording = RecordingVfs::new(fake.clone(), spawner.clone(), delay);
        cx.update(|cx| {
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                recording.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
        });
        let (pane, cx) = cx.add_window_view(|window, cx| Pane::new(Theme::dark(), window, cx));
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        recording.reset();
        (pane, recording, fake, cx)
    }

    fn row_names(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<String> {
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view
                .projected_rows(cx)
                .iter()
                .map(|row| row.entry.name.to_string())
                .collect()
        })
    }

    fn type_query(pane: &Entity<Pane>, cx: &mut VisualTestContext, text: &str) {
        pane.update(cx, |pane, cx| pane.set_search_text(text, cx));
    }

    // ------------------------------------------------------------------
    // Instant folder-local filter
    // ------------------------------------------------------------------

    #[gpui::test]
    fn typing_filters_the_open_folder_and_reads_no_directory(cx: &mut TestAppContext) {
        let (pane, recording, _fake, cx) = setup(cx, Duration::ZERO);
        assert_eq!(
            row_names(&pane, cx),
            vec!["sub1", "sub2", "notes.txt", "report.pdf"]
        );

        type_query(&pane, cx, "report");
        // The filter is `fs_core::filter_snapshot`: pure, so the projection is
        // already narrowed *before* anything is allowed to run, and no
        // directory has been read on this or any other thread.
        assert_eq!(row_names(&pane, cx), vec!["report.pdf"]);
        assert_eq!(
            recording.reads(),
            0,
            "the instant filter must not touch the disk"
        );
        cx.run_until_parked();
        assert_eq!(recording.reads(), 0, "...and must not schedule any either");

        // Case-insensitive, and matches anywhere in the name.
        type_query(&pane, cx, "OTE");
        assert_eq!(row_names(&pane, cx), vec!["notes.txt"]);
        // Folders match by name like anything else.
        type_query(&pane, cx, "sub");
        assert_eq!(row_names(&pane, cx), vec!["sub1", "sub2"]);
    }

    // End-to-end through the widget, not the pane's setter: a keystroke in the
    // field reaches the query, and emptying it restores the listing. Every
    // other test drives `set_search_text` directly, so this is the one that
    // holds the InputState -> SearchBarEvent -> Pane wiring honest.
    #[gpui::test]
    fn typing_in_the_field_drives_the_panes_search(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        let bar = pane.read_with(cx, |pane, _| pane.search_bar().clone());

        bar.update_in(cx, |bar, window, cx| bar.set_text("report", window, cx));
        cx.run_until_parked();
        assert_eq!(row_names(&pane, cx), vec!["report.pdf"]);
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.search().expect("search").query().text(), "report");
        });

        // Clicking "Subfolders" travels the same way.
        bar.update(cx, |bar, cx| bar.set_recursive(true, cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert!(pane.search().expect("search").recursive());
        });

        bar.update_in(cx, |bar, window, cx| bar.set_text("", window, cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| assert!(pane.search().is_none()));
        assert_eq!(
            row_names(&pane, cx),
            vec!["sub1", "sub2", "notes.txt", "report.pdf"]
        );
    }

    #[gpui::test]
    fn a_blank_query_restores_the_unfiltered_projection(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        type_query(&pane, cx, "report");
        assert_eq!(row_names(&pane, cx), vec!["report.pdf"]);

        // Whitespace-only is blank too (`SearchQuery::new` trims).
        type_query(&pane, cx, "   ");
        assert!(pane.read_with(cx, |pane, _| pane.search().is_none()));
        assert_eq!(
            row_names(&pane, cx),
            vec!["sub1", "sub2", "notes.txt", "report.pdf"]
        );
    }

    #[gpui::test]
    fn the_status_line_reports_the_result_count_while_searching(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        assert!(
            pane.read_with(cx, |pane, _| pane.status_text())
                .starts_with("4 items"),
            "the ordinary line counts items"
        );
        type_query(&pane, cx, "sub");
        // The result counts replace the item count — but not the free space,
        // which is a property of the volume and has nothing to do with the
        // query (§3 puts it on the line unconditionally).
        let status = pane.read_with(cx, |pane, _| pane.status_text());
        assert!(
            status.starts_with("2 results for \u{201c}sub\u{201d}"),
            "{status:?}"
        );
        assert!(status.contains("free"), "the free space stayed: {status:?}");
    }

    #[gpui::test]
    fn a_selected_row_the_filter_hides_is_dropped(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(&[Path::new("/root/notes.txt")], cx);
        });

        type_query(&pane, cx, "report");
        dir_view.read_with(cx, |dir_view, _| {
            assert!(
                dir_view.selection().is_empty(),
                "a hidden row must not stay selected — delete would act on it"
            );
        });

        // A row the filter keeps stays selected.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(&[Path::new("/root/report.pdf")], cx);
        });
        type_query(&pane, cx, "rep");
        dir_view.read_with(cx, |dir_view, _| {
            assert!(
                dir_view
                    .selection()
                    .is_selected(&fs_core::EntryId(Arc::from(Path::new("/root/report.pdf"))))
            );
        });
    }

    #[gpui::test]
    fn clearing_the_search_brings_the_expansion_tree_back(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub1"), cx)
        });
        cx.run_until_parked();
        assert!(row_names(&pane, cx).contains(&"report-draft.txt".to_string()));

        // Search results are flat: no injected children while a query is live.
        type_query(&pane, cx, "sub");
        assert_eq!(row_names(&pane, cx), vec!["sub1", "sub2"]);

        // ...and the expansion survives, because pruning expansion state is
        // driven by the listing, never by the search.
        type_query(&pane, cx, "");
        assert!(
            row_names(&pane, cx).contains(&"report-draft.txt".to_string()),
            "clearing the search restores the tree exactly as it was"
        );
    }

    // ------------------------------------------------------------------
    // Recursive, streamed
    // ------------------------------------------------------------------

    #[gpui::test]
    fn recursive_search_streams_hits_in_on_the_throttle(cx: &mut TestAppContext) {
        let (pane, recording, _fake, cx) = setup(cx, Duration::ZERO);
        type_query(&pane, cx, "report");
        assert_eq!(row_names(&pane, cx), vec!["report.pdf"], "local only");

        pane.update(cx, |pane, cx| pane.set_search_recursive(true, cx));
        // Nothing has been read *yet*: the walk is on the background executor
        // and the batch pump has not been allowed to run.
        assert_eq!(
            recording.reads(),
            0,
            "starting a walk must not read a directory on the UI thread"
        );

        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_THROTTLE);
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_THROTTLE);
        cx.run_until_parked();

        // The subfolder hit arrived, sorted in beside the local one, and the
        // hidden entry did not (the pane's show_hidden is off).
        let names = row_names(&pane, cx);
        assert!(names.contains(&"report.pdf".to_string()));
        assert!(
            names.contains(&"report-draft.txt".to_string()),
            "the subfolder hit streamed in, got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("hidden")),
            "show_hidden is off, got {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| *n == "report.pdf").count(),
            1,
            "the walk re-reports the open folder; the local row must win once"
        );

        // Every directory of the tree was read, and the status line says so.
        assert_eq!(recording.reads(), 3, "root + sub1 + sub2");
        pane.read_with(cx, |pane, _| {
            let status = pane.status_text();
            assert!(
                status.contains("2 results") && status.contains("3 folders searched"),
                "status line was {status:?}"
            );
        });

        // Turning "Subfolders" back off drops the out-of-folder hits.
        pane.update(cx, |pane, cx| pane.set_search_recursive(false, cx));
        assert_eq!(row_names(&pane, cx), vec!["report.pdf"]);
    }

    // Regression: the toggle's click reaches the pane one flush *earlier* than
    // the field's text (the text travels InputState -> SearchBar -> Pane), so a
    // "Subfolders" toggle can land while there is no search yet. It must not be
    // dropped — the first `search_results` baseline was captured with the
    // toggle lit and zero results because it was.
    #[gpui::test]
    fn subfolders_survives_arriving_before_the_query(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        // Toggle first, with nothing being searched for.
        let bar = pane.read_with(cx, |pane, _| pane.search_bar().clone());
        bar.update(cx, |bar, cx| bar.set_recursive(true, cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| assert!(pane.search().is_none()));

        // ...then the query. It inherits the field's sticky preference, so the
        // walk really runs.
        type_query(&pane, cx, "report");
        pane.read_with(cx, |pane, _| {
            assert!(
                pane.search().expect("search").recursive(),
                "the query inherited the field's Subfolders setting"
            );
        });
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_THROTTLE * 4);
        cx.run_until_parked();
        let names = row_names(&pane, cx);
        assert!(names.contains(&"report-draft.txt".to_string()), "{names:?}");
    }

    #[gpui::test]
    fn rapid_query_changes_leave_exactly_one_walk_running(cx: &mut TestAppContext) {
        // Each `read_dir` parks first, so a walk can be caught between two
        // directories and cancelled there.
        let (pane, recording, _fake, cx) = setup(cx, READ_DELAY);
        type_query(&pane, cx, "a");
        pane.update(cx, |pane, cx| pane.set_search_recursive(true, cx));
        cx.run_until_parked();
        // The first walk has started its root read and is parked in it.
        assert_eq!(recording.reads(), 1);

        // Retarget mid-flight. Dropping the single `Task` slot drops the
        // channel, which stops the background walk where it stands.
        type_query(&pane, cx, "report");
        cx.executor().advance_clock(READ_DELAY * 10);
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_THROTTLE * 4);
        cx.run_until_parked();

        assert_eq!(
            recording.read_count_of("/root"),
            2,
            "one root read per walk — the superseded one did not restart"
        );
        assert_eq!(
            recording.reads(),
            4,
            "walk 1 stopped after its one directory; walk 2 read all three"
        );
        // ...and the surviving walk is the one the user asked for.
        let names = row_names(&pane, cx);
        assert!(names.contains(&"report-draft.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"misc.txt".to_string()), "{names:?}");
    }

    #[gpui::test]
    fn navigating_away_cancels_the_walk_and_empties_the_field(cx: &mut TestAppContext) {
        let (pane, recording, _fake, cx) = setup(cx, READ_DELAY);
        type_query(&pane, cx, "report");
        pane.update(cx, |pane, cx| pane.set_search_recursive(true, cx));
        cx.run_until_parked();
        assert_eq!(recording.reads(), 1, "parked in the root read");
        recording.reset();

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root/sub1"), cx));
        cx.executor().advance_clock(READ_DELAY * 10);
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            assert!(pane.search().is_none(), "the search left with the folder");
        });
        // The field is emptied too (staged by the pane, applied by the paint).
        cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
        cx.run_until_parked();
        let bar = pane.read_with(cx, |pane, _| pane.search_bar().clone());
        assert_eq!(bar.read_with(cx, |bar, cx| bar.text(cx)), "");
        // The only read after the reset is the new directory's own listing. Its
        // *total* is what discriminates: the abandoned walk was parked inside
        // /root's read with sub1 and sub2 still on its frontier, so had it
        // survived the navigation it would have gone on to read them — but it
        // would never have re-read /root, which is why counting /root alone
        // proves nothing.
        assert_eq!(
            recording.read_count_of("/root/sub2"),
            0,
            "the abandoned walk never reached the old folder's other subdirectory"
        );
        assert_eq!(
            recording.reads(),
            1,
            "exactly one read after the navigation: the new folder's listing, got {:?}",
            recording.paths.lock().expect("paths")
        );
        assert_eq!(
            row_names(&pane, cx),
            vec!["other.txt", "report-draft.txt"],
            "the unfiltered new folder"
        );
    }

    // Regression: `show_hidden` is an *argument* to the walk, so flipping it
    // restarts the walk — and the restart used to keep the finished walk's
    // hits, so a hidden hit stayed in the results after every other surface in
    // the pane had stopped showing hidden entries.
    #[gpui::test]
    fn a_hidden_toggle_during_a_recursive_search_re_walks_from_scratch(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        pane.update(cx, |pane, cx| pane.set_show_hidden(true, cx));
        cx.run_until_parked();
        type_query(&pane, cx, "report");
        pane.update(cx, |pane, cx| pane.set_search_recursive(true, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_THROTTLE * 4);
        cx.run_until_parked();
        let names = row_names(&pane, cx);
        assert!(
            names.contains(&".hidden-report".to_string()),
            "the hidden hit is found while hidden files are shown: {names:?}"
        );
        let scanned_with_hidden =
            pane.read_with(cx, |pane, _| pane.search().expect("search").dirs_scanned);

        pane.update(cx, |pane, cx| pane.set_show_hidden(false, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_THROTTLE * 4);
        cx.run_until_parked();
        let names = row_names(&pane, cx);
        assert!(
            !names.contains(&".hidden-report".to_string()),
            "hiding hidden files must drop the hidden hit too: {names:?}"
        );
        assert!(
            names.contains(&"report-draft.txt".to_string()),
            "the non-hidden hits are still there, exactly once each: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| *n == "report-draft.txt").count(),
            1,
            "the restarted walk did not stack its hits on the old ones: {names:?}"
        );
        // ...and the folder count is the new walk's, not two walks summed.
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.search().expect("search").dirs_scanned,
                scanned_with_hidden,
                "the restart re-counted the tree instead of adding to it"
            );
        });
    }

    // Regression: `restart_search` cancels the walk whether or not it spawns a
    // new one, so the flag the visual runner's `settle_search` spins on has to
    // be cleared there — not only by a `Done` that can no longer arrive.
    #[gpui::test]
    fn turning_subfolders_off_mid_walk_stops_reporting_a_running_walk(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, READ_DELAY);
        type_query(&pane, cx, "report");
        pane.update(cx, |pane, cx| pane.set_search_recursive(true, cx));
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.search().expect("search").is_running()));

        pane.update(cx, |pane, cx| pane.set_search_recursive(false, cx));
        pane.read_with(cx, |pane, _| {
            assert!(
                !pane.search().expect("search").is_running(),
                "no walk is running, so nothing may claim one is"
            );
        });
    }

    // Regression: the field mirrors the scope for its checkbox, and only
    // `cancel_search_for_navigation` used to reset that mirror — so emptying
    // the field left a lit "☑ Subfolders" over the next query's folder-local
    // filter, and its first click did nothing.
    #[gpui::test]
    fn emptying_the_field_resets_the_subfolders_checkbox_with_it(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        let bar = pane.read_with(cx, |pane, _| pane.search_bar().clone());
        bar.update_in(cx, |bar, window, cx| bar.set_text("report", window, cx));
        cx.run_until_parked();
        bar.update(cx, |bar, cx| bar.set_recursive(true, cx));
        cx.run_until_parked();

        bar.update_in(cx, |bar, window, cx| bar.set_text("", window, cx));
        cx.run_until_parked();
        assert!(
            !bar.read_with(cx, |bar, _| bar.recursive()),
            "the checkbox went out with the query"
        );

        bar.update_in(cx, |bar, window, cx| bar.set_text("sub", window, cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, cx| {
            assert_eq!(
                pane.search().expect("search").recursive(),
                pane.search_bar().read(cx).recursive(),
                "the checkbox and the search it describes agree"
            );
        });
    }

    #[gpui::test]
    fn a_query_that_matches_nothing_says_so_instead_of_claiming_the_folder_is_empty(
        cx: &mut TestAppContext,
    ) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        assert_eq!(
            dir_view.read_with(cx, |dir_view, cx| dir_view.empty_placeholder(cx)),
            "Empty folder"
        );
        type_query(&pane, cx, "zzz-nothing-matches");
        assert!(row_names(&pane, cx).is_empty());
        assert_eq!(
            dir_view.read_with(cx, |dir_view, cx| dir_view.empty_placeholder(cx)),
            "No items match your search",
            "the folder is not empty — the query is too narrow"
        );
    }

    // Regression: search results are flat, but every folder row still painted a
    // live disclosure triangle. Clicking it changed nothing on screen while
    // inserting expansion state and starting a child load — and the state
    // outlived the search, so clearing the query brought the folder back
    // pre-expanded over a stale cached listing.
    #[gpui::test]
    fn search_result_folder_rows_have_no_working_disclosure(cx: &mut TestAppContext) {
        let (pane, recording, _fake, cx) = setup(cx, Duration::ZERO);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        assert!(
            dir_view
                .update(cx, |dir_view, cx| dir_view.projected_rows(cx))
                .iter()
                .any(|row| row.disclosure),
            "the ordinary projection does give folders a triangle"
        );

        type_query(&pane, cx, "sub");
        let rows = dir_view.update(cx, |dir_view, cx| dir_view.projected_rows(cx));
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|row| !row.disclosure),
            "no result row paints a control that cannot work"
        );

        // ...and the gesture itself is inert: no expansion state, no child read.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub1"), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            recording.reads(),
            0,
            "expanding a search result must not list its children"
        );
        type_query(&pane, cx, "");
        assert_eq!(
            row_names(&pane, cx),
            vec!["sub1", "sub2", "notes.txt", "report.pdf"],
            "clearing the search leaves no expansion the user could not see"
        );
    }

    #[gpui::test]
    fn one_batch_rebuilds_the_rows_once_and_the_hit_count_is_capped(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        type_query(&pane, cx, "report");
        pane.update(cx, |pane, cx| pane.set_search_recursive(true, cx));
        cx.run_until_parked();

        // Fold one synthetic batch in directly: `hits` is uncapped input from
        // the filesystem, and everything downstream of it (dedupe + sort) is
        // per-batch work on the UI thread.
        let generation = pane.read_with(cx, |pane, _| pane.search_generation);
        let batch: Vec<SearchEvent> = (0..MAX_SEARCH_HITS + 25)
            .map(|i| {
                SearchEvent::Hit(super::tests::entry(&format!(
                    "/root/sub1/report-{i:06}.txt"
                )))
            })
            .collect();
        ROWS_REBUILT.with(|count| count.set(0));
        pane.update(cx, |pane, cx| {
            pane.apply_search_batch(generation, batch, cx);
        });
        assert_eq!(
            ROWS_REBUILT.with(std::cell::Cell::get),
            1,
            "one batch, one rebuild — sorting the whole result set twice per \
             throttle window is how a big search stops the window painting"
        );
        pane.read_with(cx, |pane, _| {
            let search = pane.search().expect("search");
            assert_eq!(search.hits.len(), MAX_SEARCH_HITS, "capped");
            assert!(
                search.status_text().contains("showing the first"),
                "and it says so: {}",
                search.status_text()
            );
        });
    }

    #[gpui::test]
    fn a_watcher_patch_during_a_search_does_not_unfilter(cx: &mut TestAppContext) {
        let (pane, _recording, fake, cx) = setup(cx, Duration::ZERO);
        type_query(&pane, cx, "report");
        assert_eq!(row_names(&pane, cx), vec!["report.pdf"]);

        // An external create of a *non-matching* file arrives as a patch.
        fake.insert_file("/root/unrelated.txt", 5);
        fake.emit_event(PathEvent {
            path: Arc::from(Path::new("/root/unrelated.txt")),
            kind: fs_core::PathEventKind::Created,
        });
        cx.executor().advance_clock(WATCH_LATENCY);
        cx.run_until_parked();

        assert_eq!(
            row_names(&pane, cx),
            vec!["report.pdf"],
            "the patch must not resurrect rows the filter excludes"
        );
        // ...and a *matching* create does show up, because the rows are
        // re-derived from the patched snapshot rather than frozen.
        fake.insert_file("/root/report2.pdf", 5);
        fake.emit_event(PathEvent {
            path: Arc::from(Path::new("/root/report2.pdf")),
            kind: fs_core::PathEventKind::Created,
        });
        cx.executor().advance_clock(WATCH_LATENCY);
        cx.run_until_parked();
        let names = row_names(&pane, cx);
        assert!(names.contains(&"report2.pdf".to_string()), "{names:?}");
        assert!(!names.contains(&"unrelated.txt".to_string()), "{names:?}");
    }

    #[gpui::test]
    fn a_refresh_keeps_the_search_and_re_derives_its_rows(cx: &mut TestAppContext) {
        let (pane, _recording, fake, cx) = setup(cx, Duration::ZERO);
        type_query(&pane, cx, "report");
        fake.insert_file("/root/report3.pdf", 1);
        pane.update(cx, |pane, cx| pane.refresh(cx));
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            assert!(
                pane.search().is_some(),
                "an in-place reload keeps the query"
            );
        });
        let names = row_names(&pane, cx);
        assert_eq!(
            names.len(),
            2,
            "still filtered, now over fresh rows: {names:?}"
        );
        assert!(names.contains(&"report3.pdf".to_string()), "{names:?}");
    }

    // ------------------------------------------------------------------
    // Keys and focus
    // ------------------------------------------------------------------

    #[gpui::test]
    fn escape_in_the_field_clears_the_search_and_gives_focus_back(cx: &mut TestAppContext) {
        let (pane, _recording, _fake, cx) = setup(cx, Duration::ZERO);
        pane.update_in(cx, |pane, window, cx| pane.focus_search(window, cx));
        type_query(&pane, cx, "report");
        cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(600.0)));
        assert_eq!(row_names(&pane, cx), vec!["report.pdf"]);

        let focused = cx.update(|window, cx| window.focused(cx));
        let input_handle =
            pane.read_with(cx, |pane, cx| pane.search_bar().read(cx).focus_handle(cx));
        assert_eq!(focused, Some(input_handle), "the field has focus");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert!(pane.search().is_none(), "escape cleared the search");
        });
        assert_eq!(
            row_names(&pane, cx),
            vec!["sub1", "sub2", "notes.txt", "report.pdf"]
        );
        let pane_handle = pane.read_with(cx, |pane, cx| pane.focus_handle(cx));
        let focused = cx.update(|window, cx| window.focused(cx));
        assert_eq!(focused, Some(pane_handle), "focus came back to the pane");
    }
}
