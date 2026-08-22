//! The directory view (ARCHITECTURE.md §2 `DirView`, §4a data flow).
//!
//! Owns the full **path-keyed** [`SelectionModel`] (M3, §2): cursor +
//! multi-select set + range anchor — click, `cmd`-click toggle,
//! `shift`-click/`shift`-arrow ranges, select-all — surviving re-sorts,
//! watcher patches, and in-place expansion re-projection. Renders the owning
//! pane's current [`ListingSnapshot`] as the details list
//! (`views/details_list.rs`). Handles `OpenSelected` (folder →
//! `DirViewEvent::NavigateTo`, which the pane turns into navigation; file →
//! the [`crate::app_state::Opener`] stub), cursor movement, type-ahead
//! (§0: printable characters are *not* an action — they arrive via
//! `on_key_down` fallthrough when no binding matched; the reset delay runs on
//! [`fs_core::Spawner::timer`] so tests use fake time), and the M3
//! clipboard/delete rows. `Cut`/`Copy` fill the [`fs_core::FileClipboard`] in
//! [`FsContext`] (cut sources render dimmed), `Paste` turns it into a
//! `FileOp` (move on cut, keep-both names planned by ops), and
//! `DeleteToTrash` submits a `TrashOp` for the selection.
//! `DeletePermanently` is bound in this view's context (so `!renaming`
//! guards it) but handled by the workspace, which owns the confirm modal.
//!
//! **In-place folder expansion (M2, §2/§8):** the view holds
//! `expanded: BTreeSet<Arc<Path>>`; the visible row list is a **flat
//! projection** over the pane's snapshot — each expanded folder's
//! background-loaded child listing is spliced beneath it with a depth field
//! (the same flatten technique as the sidebar tree), so `uniform_list`
//! virtualization is untouched. `ExpandSelected`/`CollapseSelected`
//! (right/left keys, disclosure-triangle clicks) mutate the set and
//! re-project. Child listings are cached raw (hidden entries included) and
//! sorted/filtered at projection time with the snapshot's current
//! `SortSpec`/hidden flag, so sort flips and the hidden toggle stay
//! consistent without reloading children.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs_core::{ClipboardMode, EntryId, FileEntry, FileOp, ListingSnapshot, SortSpec, list_dir};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, Render,
    ScrollStrategy, Task, UniformListScrollHandle, WeakEntity, Window, div, point, prelude::*, px,
};

use crate::actions::{
    CollapseSelected, Copy, Cut, DeleteToTrash, ExpandSelected, ExtendSelectionNext,
    ExtendSelectionPrev, OpenSelected, PageDown, PageUp, Paste, SelectAll, SelectFirst, SelectLast,
    SelectNext, SelectPrev,
};
use crate::app_state::FsContext;
use crate::pane::Pane;
use crate::selection::SelectionModel;
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

/// One visible row of the flat projection (§8): a snapshot entry or an
/// injected child of an expanded folder, with its indentation depth.
#[derive(Clone, Debug)]
pub struct ProjectedRow {
    pub entry: FileEntry,
    /// 0 for snapshot rows; +1 per expansion level for injected children.
    pub depth: usize,
    /// True when this row is a folder currently expanded in place.
    pub expanded: bool,
}

pub struct DirView {
    focus_handle: FocusHandle,
    theme: Theme,
    pane: WeakEntity<Pane>,
    /// The full path-keyed selection (§2): cursor + multi-select set + range
    /// anchor. Path keys are what make it survive re-sorts, watcher patches,
    /// and in-place expansion re-projection.
    selection: SelectionModel,
    /// Folders expanded in place, path-keyed so expansion survives
    /// re-projection. Collapsing keeps descendants' entries so re-expanding
    /// restores nested expansion (same policy as the sidebar tree).
    expanded: BTreeSet<Arc<Path>>,
    /// Background-loaded raw child listings per expanded folder (hidden
    /// entries included; sorted/filtered at projection time).
    children: HashMap<Arc<Path>, Vec<FileEntry>>,
    /// The flat projection rendered by `uniform_list`; rebuilt in `render`.
    flat: Vec<ProjectedRow>,
    /// In-flight child loads, held so dropping the view cancels them (§5).
    _child_loads: HashMap<Arc<Path>, Task<()>>,
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
            selection: SelectionModel::default(),
            expanded: BTreeSet::new(),
            children: HashMap::new(),
            flat: Vec::new(),
            _child_loads: HashMap::new(),
            scroll_handle: UniformListScrollHandle::new(),
            type_ahead: String::new(),
            _type_ahead_reset: None,
        }
    }

    // ------------------------------------------------------------------
    // Selection (path-keyed SelectionModel, §2)
    // ------------------------------------------------------------------

    pub fn cursor(&self) -> Option<&EntryId> {
        self.selection.cursor()
    }

    /// M1-shaped cursor API, kept for the pane and tests: `Some` selects only
    /// that entry; `None` clears the whole selection (navigation across
    /// directories).
    pub fn set_cursor(&mut self, cursor: Option<EntryId>, cx: &mut Context<Self>) {
        match cursor {
            Some(id) => self.selection.select_only(id),
            None => self.selection.clear(),
        }
        cx.notify();
    }

    pub fn selection(&self) -> &SelectionModel {
        &self.selection
    }

    /// Multi-select driver for tests and visual scenarios: the selection
    /// becomes exactly `paths` (cursor on the last).
    pub fn select_paths(&mut self, paths: &[&Path], cx: &mut Context<Self>) {
        self.selection.clear();
        for path in paths {
            self.selection.toggle(EntryId(Arc::from(*path)));
        }
        cx.notify();
    }

    /// `cmd`-click on a row: toggle its membership.
    pub(crate) fn toggle_entry_selection(&mut self, entry: &FileEntry, cx: &mut Context<Self>) {
        self.selection.toggle(entry.id());
        cx.notify();
    }

    /// `shift`-click on a row: range from the anchor over the projection.
    pub(crate) fn range_select_to(&mut self, entry: &FileEntry, cx: &mut Context<Self>) {
        let order: Vec<EntryId> = self
            .projected_rows(cx)
            .iter()
            .map(|row| row.entry.id())
            .collect();
        self.selection.select_range_to(entry.id(), &order);
        cx.notify();
    }

    /// `cmd-a`: select every visible (projected) row.
    fn select_all(&mut self, cx: &mut Context<Self>) {
        let order: Vec<EntryId> = self
            .projected_rows(cx)
            .iter()
            .map(|row| row.entry.id())
            .collect();
        if order.is_empty() {
            return;
        }
        self.selection.select_all(&order);
        cx.notify();
    }

    /// Survival on fresh loads/watcher patches (called by the pane after a
    /// snapshot swap): drop selected paths that are neither in the snapshot
    /// nor injected by in-place expansion.
    pub(crate) fn retain_selection_in_listing(
        &mut self,
        snapshot: Option<&ListingSnapshot>,
        cx: &mut Context<Self>,
    ) {
        let mut keep: BTreeSet<EntryId> = snapshot
            .map(|snap| snap.entries.iter().map(FileEntry::id).collect())
            .unwrap_or_default();
        for dir in &self.expanded {
            if let Some(kids) = self.children.get(dir) {
                keep.extend(kids.iter().map(FileEntry::id));
            }
        }
        self.selection.retain(|id| keep.contains(id));
        cx.notify();
    }

    /// NavEntry restore (pane back/forward/refresh): re-place the cursor
    /// without collapsing a wider selection — refresh and re-sort restore
    /// through this, and a multi-selection must survive them.
    pub(crate) fn restore_cursor(&mut self, cursor: Option<EntryId>, cx: &mut Context<Self>) {
        self.selection.restore_cursor(cursor);
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Clipboard + delete (§0 M3 rows; §4b flow)
    // ------------------------------------------------------------------

    /// `cmd-x`: fill the clipboard in cut mode — sources render dimmed until
    /// pasted (the paste then *moves* them).
    pub fn cut_selection(&mut self, cx: &mut Context<Self>) {
        self.fill_clipboard(ClipboardMode::Cut, cx);
    }

    /// `cmd-c`: fill the clipboard in copy mode.
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        self.fill_clipboard(ClipboardMode::Copy, cx);
    }

    fn fill_clipboard(&mut self, mode: ClipboardMode, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        // Root-most paths only: cutting a folder already carries everything
        // inside it; a selected descendant would double-move.
        let entries: Vec<EntryId> = self
            .selection
            .selected_paths_rootmost()
            .iter()
            .map(|path| EntryId(Arc::from(path.as_path())))
            .collect();
        FsContext::global_mut(cx).clipboard.set(entries, mode);
        cx.notify();
    }

    /// `cmd-v`: turn the clipboard into a job (§4b) — `Copy` for copy-mode,
    /// `Move` for cut-mode (consuming the clipboard, so the dimming clears).
    /// Keep-both names (incl. paste-into-same-folder) are planned by ops.
    pub fn paste_into_current(&mut self, cx: &mut Context<Self>) {
        let Some(dest) = self
            .pane
            .upgrade()
            .and_then(|pane| pane.read(cx).path().map(Path::to_path_buf))
        else {
            return;
        };
        let Some(op) = FsContext::global_mut(cx).clipboard.paste_op(&dest) else {
            return;
        };
        FsContext::global(cx).queue.submit(op);
        cx.notify();
    }

    /// `delete`: move the selection to the trash (undoable via restore).
    pub fn delete_selection_to_trash(&mut self, cx: &mut Context<Self>) {
        let paths = self.selection.selected_paths_rootmost();
        if paths.is_empty() {
            return;
        }
        FsContext::global(cx)
            .queue
            .submit(FileOp::TrashOp { paths });
    }

    /// The pane's current snapshot — the DirView renders the pane's listing
    /// (ARCHITECTURE.md §4a); it holds no copy of its own.
    fn snapshot(&self, cx: &App) -> Option<Arc<ListingSnapshot>> {
        self.pane
            .upgrade()
            .and_then(|pane| pane.read(cx).snapshot().cloned())
    }

    // ------------------------------------------------------------------
    // Flat projection (M2, §8)
    // ------------------------------------------------------------------

    /// Build the flat row projection: snapshot rows at depth 0, each expanded
    /// folder's cached children spliced beneath it with `depth + 1`, sorted
    /// and hidden-filtered by the snapshot's current settings.
    pub fn projected_rows(&self, cx: &App) -> Vec<ProjectedRow> {
        let Some(snapshot) = self.snapshot(cx) else {
            return Vec::new();
        };
        let mut flat = Vec::new();
        for entry in snapshot.entries.iter() {
            self.project_into(&mut flat, entry, 0, snapshot.sort, snapshot.show_hidden);
        }
        flat
    }

    fn project_into(
        &self,
        flat: &mut Vec<ProjectedRow>,
        entry: &FileEntry,
        depth: usize,
        sort: SortSpec,
        show_hidden: bool,
    ) {
        let expanded = entry.is_dir_like() && self.expanded.contains(&*entry.path);
        flat.push(ProjectedRow {
            entry: entry.clone(),
            depth,
            expanded,
        });
        if expanded && let Some(kids) = self.children.get(&*entry.path) {
            let mut kids: Vec<&FileEntry> = kids
                .iter()
                .filter(|kid| show_hidden || !kid.hidden)
                .collect();
            kids.sort_by(|a, b| sort.compare(a, b));
            for kid in kids {
                self.project_into(flat, kid, depth + 1, sort, show_hidden);
            }
        }
    }

    /// The projection as last rendered (uniform_list row source and test
    /// observability).
    pub fn flat_rows(&self) -> &[ProjectedRow] {
        &self.flat
    }

    /// Whether this path is present among the loaded children of expanded
    /// folders — the pane consults this (in addition to its own snapshot) so
    /// a cursor on an injected row survives fresh loads. Reads only this
    /// view's own state, so the pane may call it mid-update.
    pub(crate) fn injected_contains(&self, id: &EntryId) -> bool {
        self.expanded.iter().any(|dir| {
            self.children
                .get(dir)
                .is_some_and(|kids| kids.iter().any(|kid| kid.id() == *id))
        })
    }

    // ------------------------------------------------------------------
    // Expansion (M2 ExpandSelected / CollapseSelected)
    // ------------------------------------------------------------------

    /// `right` on a collapsed folder row: expand it in place (children load
    /// in the background and splice in when they land).
    fn expand_selected(&mut self, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let Some(ix) = self.cursor_ix(&rows) else {
            return;
        };
        let row = &rows[ix];
        if row.entry.is_dir_like() && !row.expanded {
            self.expand(row.entry.path.clone(), cx);
        }
    }

    /// `left`: collapse the expanded folder under the cursor; on any other
    /// row deeper than the top level, move the cursor to its parent row
    /// (Explorer behavior).
    fn collapse_selected(&mut self, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let Some(ix) = self.cursor_ix(&rows) else {
            return;
        };
        let row = rows[ix].clone();
        if row.expanded {
            self.collapse(&row.entry.path, cx);
        } else if row.depth > 0
            && let Some(parent) = row.entry.path.parent()
            && let Some(parent_ix) = rows.iter().position(|r| &*r.entry.path == parent)
        {
            self.move_cursor_to(parent_ix, &rows, cx);
        }
    }

    /// Disclosure-triangle click (and visual-scenario driver): expand or
    /// collapse this folder.
    pub fn toggle_expanded(&mut self, path: &Path, cx: &mut Context<Self>) {
        let key: Arc<Path> = Arc::from(path);
        if self.expanded.contains(&key) {
            self.collapse(&key, cx);
        } else {
            self.expand(key, cx);
        }
    }

    fn expand(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
        self.expanded.insert(path.clone());
        self.load_children(path, cx);
        cx.notify();
    }

    /// Remove a folder from the expansion set (its subtree leaves the
    /// projection; descendants keep their own expansion entries so
    /// re-expanding restores them). Selected rows inside the removed subtree
    /// are dropped from the selection — a path-keyed selection must not keep
    /// acting on rows that left the projection — and a cursor inside it is
    /// pulled up to the collapsed folder rather than silently vanishing.
    fn collapse(&mut self, path: &Arc<Path>, cx: &mut Context<Self>) {
        self.expanded.remove(path);
        let cursor_was_inside = self
            .selection
            .cursor()
            .is_some_and(|cursor| cursor.0.starts_with(path) && *cursor.0 != **path);
        let key = path.clone();
        self.selection
            .retain(|id| !(id.0.starts_with(&key) && *id.0 != *key));
        if cursor_was_inside {
            self.selection.select_only(EntryId(path.clone()));
        }
        cx.notify();
    }

    /// Background-list an expanded folder's children (raw: hidden entries
    /// included, default sort — projection re-sorts/filters with the live
    /// settings). Results are cached; collapsing keeps them so re-expanding
    /// paints instantly.
    fn load_children(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
        if self.children.contains_key(&path) || self._child_loads.contains_key(&path) {
            return;
        }
        let vfs = FsContext::global(cx).vfs.clone();
        let load_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(list_dir(
                    vfs,
                    load_path.clone(),
                    SortSpec::default(),
                    true,
                    0,
                ))
                .await;
            this.update(cx, |this, cx| {
                // An unreadable folder simply has no children in place.
                let entries = match result {
                    Ok(snapshot) => (*snapshot.entries).clone(),
                    Err(_) => Vec::new(),
                };
                this.children.insert(load_path, entries);
                cx.notify();
            })
            .ok();
        });
        self._child_loads.insert(path, task);
    }

    // ------------------------------------------------------------------
    // Cursor movement over the projection
    // ------------------------------------------------------------------

    fn cursor_ix(&self, rows: &[ProjectedRow]) -> Option<usize> {
        let cursor = self.selection.cursor()?;
        rows.iter().position(|row| row.entry.id() == *cursor)
    }

    /// Move the cursor to `ix` (plain movement: selection collapses to the
    /// row) and keep it visible (§8: `scroll_to_item` on every cursor move).
    fn move_cursor_to(&mut self, ix: usize, rows: &[ProjectedRow], cx: &mut Context<Self>) {
        let Some(row) = rows.get(ix) else {
            return;
        };
        self.selection.select_only(row.entry.id());
        self.scroll_handle
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// `shift-down`/`shift-up`: move the cursor and re-range the selection
    /// from the anchor (§0 "Cursor movement (+shift- extends)").
    fn extend_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let len = rows.len();
        if len == 0 {
            return;
        }
        let ix = match self.cursor_ix(&rows) {
            Some(ix) => (ix as isize + delta).clamp(0, len as isize - 1) as usize,
            None if delta >= 0 => 0,
            None => len - 1,
        };
        let order: Vec<EntryId> = rows.iter().map(|row| row.entry.id()).collect();
        self.selection.select_range_to(order[ix].clone(), &order);
        self.scroll_handle
            .scroll_to_item(ix, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn step_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let len = rows.len();
        if len == 0 {
            return;
        }
        let ix = match self.cursor_ix(&rows) {
            Some(ix) => (ix as isize + delta).clamp(0, len as isize - 1) as usize,
            // No cursor yet: any downward motion lands on the first row, any
            // upward motion on the last.
            None if delta >= 0 => 0,
            None => len - 1,
        };
        self.move_cursor_to(ix, &rows, cx);
    }

    fn move_cursor_to_end(&mut self, first: bool, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let len = rows.len();
        if len == 0 {
            return;
        }
        let ix = if first { 0 } else { len - 1 };
        self.move_cursor_to(ix, &rows, cx);
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
        let rows = self.projected_rows(cx);
        let Some(ix) = self.cursor_ix(&rows) else {
            return;
        };
        let entry = rows[ix].entry.clone();
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

    /// Jump to the next row whose name starts with the typed prefix
    /// (case-insensitive). A fresh single-character prefix searches *after*
    /// the cursor so repeated letters cycle through matches; a longer prefix
    /// keeps refining from the current row. Wraps around.
    fn jump_to_type_ahead_match(&mut self, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let len = rows.len();
        if len == 0 {
            return;
        }
        let prefix = self.type_ahead.to_lowercase();
        let fresh = prefix.chars().count() == 1;
        let start = match self.cursor_ix(&rows) {
            Some(ix) if fresh => ix + 1,
            Some(ix) => ix,
            None => 0,
        };
        for offset in 0..len {
            let ix = (start + offset) % len;
            if rows[ix].entry.name.to_lowercase().starts_with(&prefix) {
                self.move_cursor_to(ix, &rows, cx);
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
        let sort = pane.as_ref().map(|p| p.read(cx).sort()).unwrap_or_default();
        let load_error = pane
            .as_ref()
            .and_then(|p| p.read(cx).load_error().map(str::to_string));

        // Rebuild the flat projection this frame; the uniform_list row
        // processor reads it back by index.
        self.flat = self.projected_rows(cx);

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
        } else if !self.flat.is_empty() {
            details_list::render_rows(self, cx).into_any_element()
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
            // §0 shift-arrows: extend the range from the anchor (M3).
            .on_action(
                cx.listener(|this, _: &ExtendSelectionNext, _, cx| this.extend_selection(1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ExtendSelectionPrev, _, cx| this.extend_selection(-1, cx)),
            )
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                this.step_cursor(this.rows_per_page() as isize, cx)
            }))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                this.step_cursor(-(this.rows_per_page() as isize), cx)
            }))
            // §0 Views (M2): in-place expansion.
            .on_action(cx.listener(|this, _: &ExpandSelected, _, cx| this.expand_selected(cx)))
            .on_action(cx.listener(|this, _: &CollapseSelected, _, cx| this.collapse_selected(cx)))
            // §0 select-all over the visible projection (M3).
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            // §0 clipboard rows (M3): cut dims sources, paste moves on cut.
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut_selection(cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy_selection(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste_into_current(cx)))
            // §0 delete-to-trash (M3). DeletePermanently deliberately has no
            // handler here: it bubbles to the workspace, which owns the
            // ConfirmDialog guard.
            .on_action(
                cx.listener(|this, _: &DeleteToTrash, _, cx| this.delete_selection_to_trash(cx)),
            )
            .on_key_down(cx.listener(Self::handle_key_down))
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(details_list::render_header(&theme, sort, cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    //! §9 dir_view rows for M2: expand injects children, collapse removes the
    //! subtree, the (path-keyed) cursor survives re-projection, and the
    //! `right`/`left` bindings dispatch on the real focused entity.

    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::pane::Pane;
    use fs_core::{FakeVfs, Spawner};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({
                    "sub": {
                        "deep": { "x.txt": "x" },
                        "inner.txt": "abc",
                        ".dot": "hidden",
                    },
                    "zeta": {},
                    "a.txt": "..",
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
            vfs
        })
    }

    fn build_pane(cx: &mut TestAppContext) -> (Entity<Pane>, &mut VisualTestContext) {
        cx.add_window_view(|window, cx| Pane::new(Theme::dark(), window, cx))
    }

    fn entry_id(path: &str) -> EntryId {
        EntryId(Arc::from(Path::new(path)))
    }

    /// The projection as (path, depth) pairs; `PathBuf` comparison is
    /// component-wise, so assertions hold on Windows and Unix alike.
    fn rows(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<(PathBuf, usize)> {
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        cx.update(|_, cx| {
            dir_view
                .read(cx)
                .projected_rows(cx)
                .iter()
                .map(|row| (row.entry.path.to_path_buf(), row.depth))
                .collect()
        })
    }

    fn expect(entries: &[(&str, usize)]) -> Vec<(PathBuf, usize)> {
        entries
            .iter()
            .map(|(path, depth)| (PathBuf::from(path), *depth))
            .collect()
    }

    fn open_root(cx: &mut TestAppContext) -> (Entity<Pane>, &mut VisualTestContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        (pane, cx)
    }

    #[gpui::test]
    fn expand_injects_children_into_the_projection(cx: &mut TestAppContext) {
        let (pane, cx) = open_root(cx);

        assert_eq!(
            rows(&pane, cx),
            expect(&[("/root/sub", 0), ("/root/zeta", 0), ("/root/a.txt", 0)])
        );

        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.set_cursor(Some(entry_id("/root/sub")), cx);
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();

        // Children splice in beneath their folder with depth 1, sorted
        // folders-first; the hidden dotfile stays filtered.
        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/inner.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
            ])
        );

        // Cursor movement walks the projection, entering the subtree.
        dir_view.update(cx, |dir_view, cx| dir_view.step_cursor(1, cx));
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/sub/deep")));
        });
    }

    #[gpui::test]
    fn collapse_removes_the_subtree_and_reexpand_restores_nesting(cx: &mut TestAppContext) {
        let (pane, cx) = open_root(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());

        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub/deep"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/deep/x.txt", 2),
                ("/root/sub/inner.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
            ])
        );

        // Collapse removes the whole subtree from the projection…
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        assert_eq!(
            rows(&pane, cx),
            expect(&[("/root/sub", 0), ("/root/zeta", 0), ("/root/a.txt", 0)])
        );

        // …and re-expanding restores it instantly from cached children,
        // including the still-expanded nested folder.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/deep/x.txt", 2),
                ("/root/sub/inner.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
            ])
        );
    }

    #[gpui::test]
    fn cursor_survives_reprojection_and_collapse_pulls_it_up(cx: &mut TestAppContext) {
        let (pane, cx) = open_root(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());

        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();
        dir_view.update(cx, |dir_view, cx| {
            dir_view.set_cursor(Some(entry_id("/root/sub/inner.txt")), cx);
        });

        // Expanding another folder re-projects; the path-keyed cursor holds.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/zeta"), cx);
        });
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/sub/inner.txt")));
        });

        // Refresh keeps a cursor that lives on an injected row (the pane
        // consults the DirView's injected paths, not just its snapshot).
        pane.update(cx, |pane, cx| pane.refresh(cx));
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/sub/inner.txt")));
        });

        // Collapsing the subtree holding the cursor pulls it up to the
        // collapsed folder instead of letting it vanish.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/sub")));
        });
    }

    // Dispatch guard on the real entity (§9): right expands, left collapses,
    // and left on a non-expanded child moves the cursor to its parent row.
    #[gpui::test]
    fn right_and_left_keys_drive_expansion_on_the_focused_view(cx: &mut TestAppContext) {
        let (pane, cx) = open_root(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());

        dir_view.update(cx, |dir_view, cx| {
            dir_view.set_cursor(Some(entry_id("/root/sub")), cx);
        });
        cx.update(|window, cx| {
            let handle = dir_view.focus_handle(cx);
            window.focus(&handle, cx);
        });

        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/inner.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
            ]),
            "right on a collapsed folder expands it"
        );

        // Left on a non-expanded child row moves the cursor to its parent.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.set_cursor(Some(entry_id("/root/sub/inner.txt")), cx);
        });
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/sub")));
        });

        // Left on the (now-selected) expanded folder collapses it.
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        assert_eq!(
            rows(&pane, cx),
            expect(&[("/root/sub", 0), ("/root/zeta", 0), ("/root/a.txt", 0)]),
            "left on an expanded folder collapses it"
        );

        // Left on a top-level, non-expanded row is a no-op.
        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/sub")));
        });
    }

    #[gpui::test]
    fn hidden_toggle_and_sort_apply_to_injected_children(cx: &mut TestAppContext) {
        let (pane, cx) = open_root(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());

        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();

        // Children were cached raw; the hidden dotfile appears the moment the
        // pane shows hidden files — no reload.
        pane.update(cx, |pane, cx| pane.set_show_hidden(true, cx));
        cx.run_until_parked();
        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/.dot", 1),
                ("/root/sub/inner.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
            ]),
            "hidden children appear without reloading (name-sorted, folders first)"
        );
    }

    // ------------------------------------------------------------------
    // M3: clipboard behaviors (plan §3 "Cut/paste files" row) end-to-end
    // through the real handlers, clipboard, JobQueue and FakeVfs.
    // ------------------------------------------------------------------

    /// Open `/root`, select one entry, and return the pane plus the vfs so the
    /// tree can be asserted after an operation settles.
    fn open_root_with_vfs(
        cx: &mut TestAppContext,
    ) -> (Arc<FakeVfs>, Entity<Pane>, &mut VisualTestContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        (vfs, pane, cx)
    }

    fn select(pane: &Entity<Pane>, cx: &mut VisualTestContext, path: &str) {
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        let path = PathBuf::from(path);
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.select_paths(&[path.as_path()], cx)));
    }

    fn dir_view_of(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Entity<DirView> {
        pane.read_with(cx, |pane, _| pane.dir_view().clone())
    }

    fn tree_has(vfs: &Arc<FakeVfs>, path: &str) -> bool {
        vfs.snapshot().keys().any(|p| p == Path::new(path))
    }

    #[gpui::test]
    fn cut_marks_sources_dimmed_then_paste_moves_them(cx: &mut TestAppContext) {
        let (vfs, pane, cx) = open_root_with_vfs(cx);
        select(&pane, cx, "/root/a.txt");

        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.cut_selection(cx)));

        // Plan §3: cut sources render dimmed — the render path asks the
        // clipboard, so assert exactly what it asks.
        cx.update(|_, cx| {
            let clipboard = &FsContext::global(cx).clipboard;
            assert!(
                clipboard.is_cut(Path::new("/root/a.txt")),
                "cut source must report as cut-pending (drives dimming)"
            );
            assert!(
                !clipboard.is_cut(Path::new("/root/zeta")),
                "unrelated entries are not cut-pending"
            );
        });

        // Paste into a different directory: cut pastes as a MOVE.
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root/zeta"), cx));
        cx.run_until_parked();
        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.paste_into_current(cx)));
        cx.run_until_parked();

        assert!(
            tree_has(&vfs, "/root/zeta/a.txt"),
            "paste-after-cut moves the entry into the destination"
        );
        assert!(
            !tree_has(&vfs, "/root/a.txt"),
            "paste-after-cut removes the source (move, not copy)"
        );
        cx.update(|_, cx| {
            assert!(
                !FsContext::global(cx)
                    .clipboard
                    .is_cut(Path::new("/root/a.txt")),
                "the cut is consumed by the paste — dimming must clear"
            );
        });
    }

    #[gpui::test]
    fn copy_then_paste_duplicates_and_keeps_the_source(cx: &mut TestAppContext) {
        let (vfs, pane, cx) = open_root_with_vfs(cx);
        select(&pane, cx, "/root/a.txt");

        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.copy_selection(cx)));
        cx.update(|_, cx| {
            assert!(
                !FsContext::global(cx)
                    .clipboard
                    .is_cut(Path::new("/root/a.txt")),
                "copy mode never dims the source"
            );
        });

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root/zeta"), cx));
        cx.run_until_parked();
        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.paste_into_current(cx)));
        cx.run_until_parked();

        assert!(
            tree_has(&vfs, "/root/zeta/a.txt"),
            "copy lands at the destination"
        );
        assert!(tree_has(&vfs, "/root/a.txt"), "copy keeps the source");
    }

    #[gpui::test]
    fn paste_into_the_same_folder_keeps_both(cx: &mut TestAppContext) {
        // The plan §7 M3 acceptance row, exercised through the UI path:
        // copy + paste in place must produce a keep-both name, never clobber.
        let (vfs, pane, cx) = open_root_with_vfs(cx);
        select(&pane, cx, "/root/a.txt");

        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| {
            dir_view.update(cx, |view, cx| {
                view.copy_selection(cx);
                view.paste_into_current(cx);
            })
        });
        cx.run_until_parked();

        assert!(tree_has(&vfs, "/root/a.txt"), "the original survives");
        let copies: Vec<_> = vfs
            .snapshot()
            .into_keys()
            .filter(|p| {
                p.parent() == Some(Path::new("/root"))
                    && p.file_name().is_some_and(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("a") && n != "a.txt"
                    })
            })
            .collect();
        assert_eq!(
            copies.len(),
            1,
            "same-folder paste yields exactly one keep-both sibling, got {copies:?}"
        );
    }

    #[gpui::test]
    fn delete_moves_the_selection_to_the_trash(cx: &mut TestAppContext) {
        let (vfs, pane, cx) = open_root_with_vfs(cx);
        select(&pane, cx, "/root/a.txt");

        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.delete_selection_to_trash(cx)));
        cx.run_until_parked();

        assert!(
            !tree_has(&vfs, "/root/a.txt"),
            "delete removes the entry from its directory"
        );
        assert!(
            vfs.snapshot()
                .keys()
                .any(|p| p.components().any(|c| c.as_os_str() == ".fake-trash")),
            "the entry is recoverable from the trash, not destroyed"
        );
    }
}
