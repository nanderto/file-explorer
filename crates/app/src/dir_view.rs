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
//! consistent without reloading children. Those caches are **invalidated by
//! the pane's watcher** (`DirView::invalidate_children`): a cached child
//! listing must not survive an external change to the folder it came from.
//!
//! **Inline rename (M3, §4c):** the view owns the rename state machine as a
//! field (`rename: Option<RenameState>`, see [`crate::rename`]) — never its
//! own entity. `RenameSelected` (`f2`) and the §0 *slow second click* both
//! call `begin_rename`; while it is up the root key context gains the
//! `renaming` token, which every `DirView && !renaming` binding is guarded
//! by. `Duplicate` (`cmd-d`) submits `FileOp::Duplicate` for the selection
//! (keep-both names planned by ops).
//!
//! **Rubber-band marquee (M3, §8):** the same field-not-an-entity shape —
//! `marquee: Option<MarqueeState>` (see [`crate::marquee`]). The list's
//! background surface (built by `marquee::list_surface`, which also parents
//! the row list) carries the gpui drag that owns the gesture, and every hit
//! test is arithmetic against the uniform row band, so rows `uniform_list`
//! virtualized away still get selected.
//!
//! **Drag & drop (M3, §8):** the third field-shaped machine —
//! `drop: Option<DropState>` (see [`crate::drag`]). Rows start the drag
//! (`details_list` builds the `DraggedEntries` payload at render time); the
//! same background surface the marquee uses carries the drop side, so a press
//! on a row is a file drag and a press on empty space is a marquee.
//!
//! **Context menus (M3, §8):** the fourth — `menu: Option<ContextMenuState>`
//! (see [`crate::context_menu`]). The same surface carries the right-click
//! trigger, hit-tested with the same row arithmetic; while a menu is up the
//! root key context gains a `menu` token, which binds `escape` to `Cancel`
//! for the menu alone. Menu rows dispatch the boxed [`crate::actions`] the
//! keymap dispatches — including `NewFolder`/`NewFile`, whose editor this
//! view opens on a **phantom row** (§4c) that the projection appends until
//! the entry is really created.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs_core::{ClipboardMode, EntryId, FileEntry, FileOp, ListingSnapshot, SortSpec, list_dir};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, KeyDownEvent, Modifiers,
    Render, ScrollStrategy, Task, UniformListScrollHandle, WeakEntity, Window, div, point,
    prelude::*, px,
};

use crate::actions::Cancel;
use crate::actions::{
    CollapseSelected, Copy, Cut, DeleteToTrash, Duplicate, ExpandSelected, ExtendSelectionLeft,
    ExtendSelectionNext, ExtendSelectionPrev, ExtendSelectionRight, OpenSelected, PageDown, PageUp,
    Paste, RenameSelected, SelectAll, SelectFirst, SelectLast, SelectNext, SelectPrev,
};
use crate::app_state::FsContext;
use crate::context_menu::{self, ContextMenuState};
use crate::drag::{self, DropState};
use crate::marquee::{self, MarqueeState};
use crate::pane::{Pane, ViewMode};
use crate::rename::RenameState;
use crate::scrollbar::ScrollbarState;
use crate::selection::SelectionModel;
use crate::theme::Theme;
use crate::thumbnails::ThumbnailState;
use crate::views::{details_list, icon_grid};

/// Quiet period after which the type-ahead prefix resets. Every keystroke
/// restarts it (the previous timer task is dropped, cancelling it).
pub const TYPE_AHEAD_TIMEOUT: Duration = Duration::from_millis(1000);

/// How long after a plain click the same row becomes *rename-armed* — the §0
/// "slow second click" trigger. A second click that lands before this is a
/// double-click (gpui reports `click_count >= 2`, which opens the entry and
/// cancels the pending arm), so only a genuinely separate later click starts
/// the inline editor. Runs on [`fs_core::Spawner::timer`], so tests drive it
/// with fake time.
pub const RENAME_CLICK_ARM_DELAY: Duration = Duration::from_millis(500);

/// Rows to move on PageUp/PageDown when the list has not been laid out yet.
const FALLBACK_PAGE_ROWS: usize = 20;

/// Events up (ARCHITECTURE.md §2): the pane subscribes and navigates.
pub enum DirViewEvent {
    /// A folder was opened (Enter / double-click).
    NavigateTo(PathBuf),
}

#[cfg(test)]
thread_local! {
    /// Test-only probe: how many flat projections [`DirView::projected_rows`]
    /// has built **on this thread**. Thread-local rather than a static, so
    /// tests running in parallel cannot see each other's counts.
    ///
    /// It exists for the info panel, whose [`crate::info_panel::InfoPanel`]
    /// witness is a claim about work *not* done on an idle notify — and the
    /// only observable of "the projection was not built" is a counter.
    static PROJECTIONS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many projections this thread has built (see `PROJECTIONS_BUILT`).
#[cfg(test)]
pub(crate) fn projections_built() -> usize {
    PROJECTIONS_BUILT.with(std::cell::Cell::get)
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
    /// The in-flight inline rename (§4c), if any — a field, not an entity.
    /// `pub(crate)` because `views/details_list.rs` renders the row swap and
    /// [`crate::rename`] drives the machine.
    pub(crate) rename: Option<RenameState>,
    /// The row a slow second click would rename (§0 Rename trigger): set by
    /// the arming timer a beat after a plain click selected it, cleared by any
    /// other interaction.
    rename_armed: Option<EntryId>,
    /// Dropping this cancels a pending arm.
    _rename_arm: Option<Task<()>>,
    /// The in-flight rubber-band gesture (§8), if any — a field, not an
    /// entity. `pub(crate)` because [`crate::marquee`] drives the machine and
    /// renders the band.
    pub(crate) marquee: Option<MarqueeState>,
    /// The root-most selected paths as of this frame, shared by every row's
    /// drag payload (§8). Rebuilt in `render` beside `flat`, because a payload
    /// is constructed for every drag-capable row that paints and re-deriving
    /// it per row would be quadratic in a large selection.
    drag_selection: Arc<[Arc<Path>]>,
    /// §8's single `Option<DropTarget>` per pane, plus what the drop would do
    /// and its spring-load timer (see [`crate::drag`]) — the third state
    /// machine to live as a *field* of this view rather than as an entity.
    pub(crate) drop: Option<DropState>,
    /// The open context menu (§8), if any — the fourth field-shaped machine
    /// (see [`crate::context_menu`]). Holds the invocation point and the
    /// boxed actions its rows dispatch.
    pub(crate) menu: Option<ContextMenuState>,
    /// Icon-grid thumbnails (M4): the byte-budget cache, the decoded images
    /// the painted tiles reference, and the single-slot fetch task that
    /// cancels itself on scroll-away. See [`crate::thumbnails`].
    pub(crate) thumbnails: ThumbnailState,
    /// The auto-hide scrollbar's visibility + fade timer (M4). See
    /// [`crate::scrollbar`].
    pub(crate) scrollbar: ScrollbarState,
    /// The icon grid's column count **as the last painted frame laid it out**.
    ///
    /// Not derived on demand from the scroll handle's bounds: those bounds are
    /// only updated during `prepaint`, so a `cols` recomputed inside a hit
    /// test can be a frame *ahead* of the tiles on screen — and then the
    /// context menu, the drop target and the marquee all name a different
    /// entry than the tile under the pointer. Every piece of grid arithmetic
    /// therefore reads this field (see [`Self::grid_cols`]), which `render`
    /// writes with exactly the value it hands [`icon_grid::render_grid`].
    /// Convergence after a resize is [`Self::note_painted_grid_cols`]'s job.
    painted_cols: usize,
    /// The [`ViewMode`] the last frame painted, so the *first* frame of a new
    /// mode can put the cursor back on screen. The two views share one pixel
    /// scroll offset but measure their items differently (a 22px row per
    /// entry vs an 88px line per `cols` entries), so the selection a switch
    /// preserves is not necessarily still visible; see
    /// [`Self::scroll_cursor_into_view`].
    painted_mode: ViewMode,
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
            rename: None,
            rename_armed: None,
            _rename_arm: None,
            marquee: None,
            drag_selection: Arc::from(Vec::new()),
            drop: None,
            menu: None,
            thumbnails: ThumbnailState::default(),
            scrollbar: ScrollbarState::default(),
            // One column until the first layout, which makes an unpainted
            // grid behave exactly like the list rather than dividing by zero.
            painted_cols: 1,
            painted_mode: ViewMode::default(),
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

    /// Mutable access for a sibling selection gesture that owns its own
    /// mutation rule — currently only the rubber-band marquee
    /// ([`crate::marquee`]). Callers notify themselves.
    pub(crate) fn selection_mut(&mut self) -> &mut SelectionModel {
        &mut self.selection
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
        let keep = self.listing_ids(snapshot);
        self.selection.retain(|id| keep.contains(id));
        self.prune_expansion_state(&keep);
        cx.notify();
    }

    /// Drop expansion state for folders that have left the listing.
    ///
    /// The selection and the cursor are pruned above because an invisible row
    /// must not be actionable; `expanded`/`children` used to be left alone
    /// because the projection only injects children of folders it actually
    /// paints, so a dead entry was merely inert. Inert is not free: the maps
    /// grew for the life of the pane (a `children` entry holds a whole child
    /// listing), and a folder deleted and later re-created with the same name
    /// came back **pre-expanded from a stale cache**.
    ///
    /// A key survives iff some ancestor of it is still a row: `/a/b`'s cached
    /// children live on while `/a` is listed and are dropped with it. The open
    /// directory itself is not in `keep` (its entries are), so a path outside
    /// the listing entirely is pruned as well.
    fn prune_expansion_state(&mut self, keep: &BTreeSet<EntryId>) {
        let alive = |path: &Arc<Path>| {
            path.ancestors()
                .any(|ancestor| keep.contains(&EntryId(Arc::from(ancestor))))
        };
        self.expanded.retain(alive);
        self.children.retain(|path, _| alive(path));
        // ...and an in-flight child load for a folder that has gone: dropping
        // the task cancels it, so nothing lands in `children` afterwards.
        self._child_loads.retain(|path, _| alive(path));
    }

    /// Every id the projection would render over `snapshot`: its rows, plus
    /// the loaded children of expanded folders **whose own row is still
    /// there**. That qualifier is the whole point: when the watcher removes an
    /// expanded folder, its children stop being projected, and a selection
    /// (or cursor) that kept pointing at them would keep acting on rows that
    /// no longer render — or exist.
    ///
    /// Reads only this view's own state plus the snapshot handed in, so the
    /// pane may call it mid-update (unlike [`Self::projected_rows`], which
    /// reads the pane back).
    fn listing_ids(&self, snapshot: Option<&ListingSnapshot>) -> BTreeSet<EntryId> {
        let mut keep: BTreeSet<EntryId> = snapshot
            .map(|snap| snap.entries.iter().map(FileEntry::id).collect())
            .unwrap_or_default();
        // `expanded` is ordered by path, and path order is component-wise, so
        // a folder always precedes its own descendants: dropping one whose row
        // has gone drops everything nested inside it too.
        for dir in &self.expanded {
            if !keep.contains(&EntryId(dir.clone())) {
                continue;
            }
            if let Some(kids) = self.children.get(dir) {
                keep.extend(kids.iter().map(FileEntry::id));
            }
        }
        keep
    }

    /// Whether `id` is a row of the projection over `snapshot` — the pane's
    /// question when restoring a cursor (a cursor on an expanded folder's
    /// child must survive fresh loads and refreshes).
    pub(crate) fn listing_contains(
        &self,
        snapshot: Option<&ListingSnapshot>,
        id: &EntryId,
    ) -> bool {
        self.listing_ids(snapshot).contains(id)
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

    /// `cmd-v` / a menu's Paste: turn the clipboard into a job (§4b) —
    /// `Copy` for copy-mode, `Move` for cut-mode (consuming the clipboard, so
    /// the dimming clears). Keep-both names (incl. paste-into-same-folder) are
    /// planned by ops.
    ///
    /// `dest` is the folder to paste into, defaulting to the pane's open
    /// directory. The row context menu passes the **right-clicked folder**
    /// (Explorer's behavior); because it is a parameter of the one action,
    /// that menu row is not a second implementation of paste.
    pub fn paste_into(&mut self, dest: Option<PathBuf>, cx: &mut Context<Self>) {
        let Some(dest) = dest.or_else(|| {
            self.pane
                .upgrade()
                .and_then(|pane| pane.read(cx).path().map(Path::to_path_buf))
        }) else {
            return;
        };
        let Some(op) = FsContext::global_mut(cx).clipboard.paste_op(&dest) else {
            return;
        };
        FsContext::global(cx).queue.submit(op);
        cx.notify();
    }

    /// `cmd-d` / the toolbar's "Duplicate selection": copy each root-most
    /// selected item next to itself. The keep-both names (`"name copy.ext"`,
    /// `"name copy 2.ext"`) are resolved by op planning, not here (§4b).
    pub fn duplicate_selection(&mut self, cx: &mut Context<Self>) {
        let sources = self.selection.selected_paths_rootmost();
        if sources.is_empty() {
            return;
        }
        FsContext::global(cx)
            .queue
            .submit(FileOp::Duplicate { sources });
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

    /// The directory this view is showing — the pane owns it (§4a). Used by
    /// paste and by [`crate::drag`]'s background drop target.
    pub(crate) fn current_dir(&self, cx: &App) -> Option<Arc<Path>> {
        self.pane
            .upgrade()
            .and_then(|pane| pane.read(cx).path().map(Arc::from))
    }

    /// The owning pane's id, carried in the drag payload so a drop can tell
    /// which pane a dragged selection came from (§8; cross-pane drags at M4).
    pub(crate) fn pane_id(&self) -> gpui::EntityId {
        self.pane.entity_id()
    }

    /// The owning pane, for the handful of pane-owned facts a view-side
    /// widget needs to read (sort column, hidden-file toggle) — see
    /// [`crate::context_menu`]. Views never *mutate* the pane through this:
    /// commands go up as actions or events (§2).
    pub(crate) fn pane_entity(&self) -> Option<gpui::Entity<Pane>> {
        self.pane.upgrade()
    }

    /// The frame's shared root-most selection, for [`crate::drag`] payloads.
    pub(crate) fn drag_selection(&self) -> &Arc<[Arc<Path>]> {
        &self.drag_selection
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
    // (the test-only projection counter lives below the impl)

    /// Build the flat row projection: snapshot rows at depth 0, each expanded
    /// folder's cached children spliced beneath it with `depth + 1`, sorted
    /// and hidden-filtered by the snapshot's current settings.
    ///
    /// **In the icon grid the splice is skipped.** A tile has no indentation,
    /// no disclosure triangle, and `left`/`right` are 2D cursor motion there
    /// rather than expand/collapse — so children projected into the grid paint
    /// as ordinary top-level tiles of a folder they do not live in, with no
    /// way to collapse them short of `cmd-1`. Explorer's icon and tiles views
    /// never show a subfolder's contents inline. `self.expanded` is left
    /// untouched, so `cmd-1` restores the tree exactly as it was and switching
    /// mode stays a pure re-render.
    pub fn projected_rows(&self, cx: &App) -> Vec<ProjectedRow> {
        #[cfg(test)]
        PROJECTIONS_BUILT.with(|count| count.set(count.get() + 1));
        let mut flat = Vec::new();
        let splice_children = self.view_mode(cx) == ViewMode::List;
        if let Some(snapshot) = self.snapshot(cx) {
            for entry in snapshot.entries.iter() {
                self.project_into(
                    &mut flat,
                    entry,
                    0,
                    snapshot.sort,
                    snapshot.show_hidden,
                    splice_children,
                );
            }
        }
        // §4c `New ▸`: the phantom row of an entry being named but not yet
        // created. Appended last — it has no place in the sort order until it
        // exists on disk, and the real (sorted) row replaces it on completion.
        if let Some(entry) = self.new_entry_row() {
            flat.push(ProjectedRow {
                entry: entry.clone(),
                depth: 0,
                expanded: false,
            });
        }
        flat
    }

    /// The phantom row of an in-flight `New ▸ Folder`/`Text file…` (§4c), if
    /// any: a `FileEntry` for a path that does not exist yet.
    pub(crate) fn new_entry_row(&self) -> Option<&FileEntry> {
        self.rename.as_ref().and_then(RenameState::new_entry_row)
    }

    /// Whether `row` is that phantom. Gestures that mean "act on a real
    /// entry" (drop targets, context-menu targets) skip it.
    pub(crate) fn is_new_entry_row(&self, row: &ProjectedRow) -> bool {
        self.new_entry_row()
            .is_some_and(|entry| entry.path == row.entry.path)
    }

    fn project_into(
        &self,
        flat: &mut Vec<ProjectedRow>,
        entry: &FileEntry,
        depth: usize,
        sort: SortSpec,
        show_hidden: bool,
        splice_children: bool,
    ) {
        let expanded = entry.is_dir_like() && self.expanded.contains(&*entry.path);
        flat.push(ProjectedRow {
            entry: entry.clone(),
            depth,
            // The grid paints no disclosure triangle, so an "expanded" tile
            // would be an unreadable claim about rows that are not there.
            expanded: expanded && splice_children,
        });
        if splice_children
            && expanded
            && let Some(kids) = self.children.get(&*entry.path)
        {
            let mut kids: Vec<&FileEntry> = kids
                .iter()
                .filter(|kid| show_hidden || !kid.hidden)
                .collect();
            kids.sort_by(|a, b| sort.compare(a, b));
            for kid in kids {
                self.project_into(flat, kid, depth + 1, sort, show_hidden, splice_children);
            }
        }
    }

    /// The projection as last rendered (uniform_list row source and test
    /// observability).
    pub fn flat_rows(&self) -> &[ProjectedRow] {
        &self.flat
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

    /// An external change (watcher batch, via the pane) landed in folders we
    /// hold cached child listings for: those listings are stale and must not
    /// be shown again. A **collapsed** folder simply loses its cache, so the
    /// next expansion re-lists it; an **expanded** one keeps the stale rows
    /// painted (and the selection inside them alive) while a fresh listing
    /// loads over the top.
    pub(crate) fn invalidate_children(&mut self, dirs: &[Arc<Path>], cx: &mut Context<Self>) {
        for dir in dirs {
            // An in-flight load would otherwise satisfy `load_children`'s
            // staleness check and never re-run.
            let was_loading = self._child_loads.remove(dir).is_some();
            if self.expanded.contains(dir) {
                if was_loading || self.children.contains_key(dir) {
                    self.start_child_load(dir.clone(), cx);
                }
            } else if self.children.remove(dir).is_some() {
                cx.notify();
            }
        }
    }

    /// Background-list an expanded folder's children (raw: hidden entries
    /// included, default sort — projection re-sorts/filters with the live
    /// settings). Results are cached; collapsing keeps them so re-expanding
    /// paints instantly.
    fn load_children(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
        if self.children.contains_key(&path) || self._child_loads.contains_key(&path) {
            return;
        }
        self.start_child_load(path, cx);
    }

    /// Unconditional (re)list of a folder's children — the invalidation path,
    /// where a cached listing exists but is known stale.
    fn start_child_load(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
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
                // A re-list can drop rows an external change deleted: a
                // path-keyed selection must not keep acting on rows that left
                // the projection.
                let snapshot = this.snapshot(cx);
                this.retain_selection_in_listing(snapshot.as_deref(), cx);
                cx.notify();
            })
            .ok();
        });
        self._child_loads.insert(path, task);
    }

    // ------------------------------------------------------------------
    // Cursor movement over the projection
    // ------------------------------------------------------------------

    pub(crate) fn cursor_ix(&self, rows: &[ProjectedRow]) -> Option<usize> {
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
        self.scroll_cursor_into_view(ix, cx);
        cx.notify();
    }

    /// Keep entry index `ix` on screen. `uniform_list` items are *rows*, and
    /// in the icon grid one row holds `cols` entries — so the details list
    /// scrolls to the entry index and the grid to `ix / cols`. Getting this
    /// wrong does not fail loudly: it scrolls to the wrong place.
    fn scroll_cursor_into_view(&self, ix: usize, cx: &App) {
        let item = match self.view_mode(cx) {
            ViewMode::List => ix,
            ViewMode::Icons => ix / self.grid_cols().max(1),
        };
        self.scroll_handle
            .scroll_to_item(item, ScrollStrategy::Nearest);
    }

    /// The pane's current layout. The DirView renders it but does not own it
    /// (§2: `Pane … view_mode`), so every mode-dependent decision reads it
    /// here rather than caching a copy that could go stale.
    pub(crate) fn view_mode(&self, cx: &App) -> ViewMode {
        self.pane
            .upgrade()
            .map(|pane| pane.read(cx).view_mode())
            .unwrap_or_default()
    }

    /// Tiles across the icon grid (§8: "`cols` recomputed from pane width") —
    /// **the value the tiles on screen were laid out with**, not a fresh
    /// measurement. Every hit test and every index/geometry conversion goes
    /// through here, so none of them can ever disagree with the pixels.
    pub(crate) fn grid_cols(&self) -> usize {
        self.painted_cols.max(1)
    }

    /// The column count this frame's list width *would* give. Only `render`
    /// and [`Self::note_painted_grid_cols`] may use it: read anywhere else it
    /// is a frame ahead of the paint.
    pub(crate) fn measured_grid_cols(&self) -> usize {
        icon_grid::cols_for_width(f32::from(marquee::list_viewport(self).size.width))
    }

    /// Called from the grid's `uniform_list` processor, which runs *after*
    /// gpui has written this frame's real list bounds onto the scroll handle
    /// (`Interactivity::prepaint` -> `clamp_scroll_position`). If the width it
    /// finds there no longer agrees with the `cols` this frame is painting,
    /// ask for one more frame: a resize otherwise leaves the grid laid out for
    /// the old width until some unrelated repaint happens along.
    ///
    /// Cannot loop: `cols_for_width` is a pure function of a width that no
    /// longer changes, so the very next frame paints the value measured here
    /// and the two agree.
    pub(crate) fn note_painted_grid_cols(
        &mut self,
        painting_with: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.measured_grid_cols() == painting_with {
            return;
        }
        // Deferred, not a plain `notify`: we are *inside* the draw that is
        // painting this view, where gpui swallows both `notify` and
        // `Window::refresh` (`WindowInvalidator::not_drawing`) precisely to
        // stop an element dirtying the frame it is part of. `Window::defer`
        // runs after the draw, which is exactly when the follow-up frame can
        // be asked for.
        let this = cx.entity();
        window.defer(cx, move |_, cx| {
            this.update(cx, |_, cx| cx.notify());
        });
    }

    /// The full height of the projection in content pixels — rows in the
    /// details list, **grid lines** in the icon view. The one place that
    /// arithmetic lives: the marquee's autoscroll clamp and the auto-hide
    /// scrollbar's thumb both need it, and a grid whose content height was
    /// still counted in row heights would scroll to the wrong bottom.
    pub(crate) fn content_height(&self, cx: &App) -> f32 {
        let len = self.flat_rows().len();
        match self.view_mode(cx) {
            ViewMode::List => len as f32 * details_list::ROW_HEIGHT,
            ViewMode::Icons => {
                icon_grid::grid_row_count(len, self.grid_cols()) as f32 * icon_grid::TILE_HEIGHT
            }
        }
    }

    /// The entry index under a point in marquee **content** space, or `None`
    /// for empty space. The one hit test both mouse gestures use (the
    /// marquee's empty-space rule and drag & drop's target arming), so the
    /// two can never disagree about where a tile is.
    pub(crate) fn index_at_content(
        &self,
        point: crate::marquee::ContentPoint,
        cx: &App,
    ) -> Option<usize> {
        let len = self.flat_rows().len();
        match self.view_mode(cx) {
            ViewMode::List => drag::row_at(point.y, details_list::ROW_HEIGHT, len),
            ViewMode::Icons => icon_grid::tile_at(point.x, point.y, self.grid_cols(), len),
        }
    }

    /// 2D cursor movement in the icon grid (§8: "index arithmetic"). The edge
    /// rules live in [`icon_grid::step_index`], unit-tested there.
    fn grid_step_cursor(&mut self, step: icon_grid::GridStep, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        if rows.is_empty() {
            return;
        }
        let cols = self.grid_cols();
        let ix = match self.cursor_ix(&rows) {
            Some(ix) => icon_grid::step_index(ix, rows.len(), cols, step),
            // No cursor yet: forward motion lands on the first tile, backward
            // on the last — the same rule the list uses.
            None if step.delta(cols) >= 0 => 0,
            None => rows.len() - 1,
        };
        self.move_cursor_to(ix, &rows, cx);
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
        self.scroll_cursor_into_view(ix, cx);
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
    /// A "row" is a `uniform_list` item, so in the icon grid it is a whole
    /// line of tiles — [`Self::page_step`] turns that into entries.
    fn rows_per_page(&self, cx: &App) -> usize {
        let item_height = match self.view_mode(cx) {
            ViewMode::List => details_list::ROW_HEIGHT,
            ViewMode::Icons => icon_grid::TILE_HEIGHT,
        };
        let viewport = self
            .scroll_handle
            .0
            .borrow()
            .base_handle
            .bounds()
            .size
            .height;
        let rows = (f32::from(viewport) / item_height) as usize;
        if rows == 0 { FALLBACK_PAGE_ROWS } else { rows }
    }

    /// Entries one PageUp/PageDown moves the cursor: a viewport of rows in
    /// the list, a viewport of *lines* — `rows * cols` entries — in the grid.
    fn page_step(&self, cx: &App) -> isize {
        let rows = self.rows_per_page(cx) as isize;
        match self.view_mode(cx) {
            ViewMode::List => rows,
            ViewMode::Icons => rows * self.grid_cols().max(1) as isize,
        }
    }

    // ------------------------------------------------------------------
    // Open (§0 "Open item": Enter / double-click)
    // ------------------------------------------------------------------

    /// `enter` / double-click / the row menu's **Open**: open *everything*
    /// selected, which is what Explorer does (and what a menu row acting on a
    /// multi-selection has to mean). Root-most only, so opening a folder and
    /// something inside it does not open the child twice; the cursor row is
    /// the fallback when nothing is selected.
    ///
    /// One pane can only *show* one directory, so at most one folder is
    /// entered (Explorer opens a window per folder — dual panes are M4);
    /// every selected file is handed to the opener.
    fn open_selected(&mut self, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let selected = self.selection.selected_rootmost();
        let targets: Vec<FileEntry> = if selected.is_empty() {
            self.cursor_ix(&rows)
                .map(|ix| rows[ix].entry.clone())
                .into_iter()
                .collect()
        } else {
            let wanted: BTreeSet<&Path> = selected.iter().map(Arc::as_ref).collect();
            rows.iter()
                .filter(|row| wanted.contains(row.entry.path.as_ref()))
                .map(|row| row.entry.clone())
                .collect()
        };
        let mut entered_folder = false;
        for entry in targets {
            if entry.is_dir_like() {
                if entered_folder {
                    continue;
                }
                entered_folder = true;
            }
            self.open_entry(&entry, cx);
        }
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

    /// The one dispatcher for every row click (`views/details_list.rs` calls
    /// it): the §0 selection rows (double-click opens, `cmd`-click toggles,
    /// `shift`-click ranges from the anchor, plain click selects) plus the §0
    /// Rename trigger — a **slow second click** on a row already armed by an
    /// earlier click starts the inline editor instead of re-selecting.
    /// Anything that is not a plain click cancels a pending or standing arm,
    /// so a double-click never renames.
    pub(crate) fn handle_row_click(
        &mut self,
        entry: &FileEntry,
        modifiers: Modifiers,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            self.disarm_rename_click();
            self.open_entry(entry, cx);
            return;
        }
        if modifiers.platform {
            self.disarm_rename_click();
            self.toggle_entry_selection(entry, cx);
            return;
        }
        if modifiers.shift {
            self.disarm_rename_click();
            self.range_select_to(entry, cx);
            return;
        }
        if self.rename_armed.as_ref() == Some(&entry.id()) {
            self.disarm_rename_click();
            self.begin_rename(entry, window, cx);
            return;
        }
        self.select_entry(entry, cx);
        self.arm_rename_click(entry.id(), cx);
    }

    /// Arm `id` for a slow second click once the double-click interval has
    /// passed. Replacing the task cancels any previous pending arm, so only
    /// the most recently clicked row is ever armed.
    fn arm_rename_click(&mut self, id: EntryId, cx: &mut Context<Self>) {
        self.rename_armed = None;
        let spawner = FsContext::global(cx).spawner.clone();
        self._rename_arm = Some(cx.spawn(async move |this, cx| {
            spawner.timer(RENAME_CLICK_ARM_DELAY).await;
            this.update(cx, |this, _| this.rename_armed = Some(id)).ok();
        }));
    }

    /// Drop both the standing arm and any pending one.
    pub(crate) fn disarm_rename_click(&mut self) {
        self.rename_armed = None;
        self._rename_arm = None;
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

    /// The painted height of one details row (`uniform_list`'s fixed row
    /// height). Public so the visual scenarios in `bin/visual_test_runner.rs`
    /// can aim real mouse input at a real row instead of re-deriving the
    /// layout.
    pub const ROW_HEIGHT: f32 = details_list::ROW_HEIGHT;

    /// The details list's viewport in window space as of the last paint — the
    /// origin row coordinates are measured from. Public for the same reason as
    /// [`Self::ROW_HEIGHT`]; inside the crate this is
    /// [`crate::marquee::list_viewport`], which every gesture's hit test uses.
    pub fn list_viewport(&self) -> gpui::Bounds<gpui::Pixels> {
        marquee::list_viewport(self)
    }

    /// Expansion bookkeeping sizes: `(expanded folders, cached child
    /// listings)`. Test observability for the pruning rule, and (M5) the
    /// cheap witness [`crate::info_panel`] uses to notice that the flat
    /// projection has changed shape without rebuilding it.
    pub(crate) fn expansion_state_sizes(&self) -> (usize, usize) {
        (self.expanded.len(), self.children.len())
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let pane = self.pane.upgrade();
        let sort = pane.as_ref().map(|p| p.read(cx).sort()).unwrap_or_default();
        let load_error = pane
            .as_ref()
            .and_then(|p| p.read(cx).load_error().map(str::to_string));
        let view_mode = self.view_mode(cx);
        // One measurement, handed to *both* the header and the body rows: a
        // narrow pane (the M4 split leaves ~270 px) cannot fit Name + Size +
        // Date, and if the two disagree within a frame values stop aligning
        // under their headers. Width comes from the same painted bounds the
        // grid's column count and every gesture hit test read.
        let columns =
            details_list::visible_columns(f32::from(marquee::list_viewport(self).size.width));

        // The auto-hide scrollbar (M4): compare this frame's scroll offset
        // with the last one and (re)arm the fade. Before the projection is
        // rebuilt, because it reads only the scroll handle.
        self.note_scroll_for_scrollbar(cx);

        // Rebuild the flat projection this frame; the uniform_list row
        // processor reads it back by index.
        self.flat = self.projected_rows(cx);
        // ...and, once, the drag payload's selection (§8), which every
        // drag-capable row shares by cloning the Arc.
        self.drag_selection = Arc::from(self.selection.selected_rootmost());

        // First frame in a new mode: Explorer keeps the selected item in view
        // across a view change, and the shared *pixel* offset does not. Uses
        // the freshly measured column count rather than `painted_cols`, which
        // still describes the mode being left behind.
        if self.painted_mode != view_mode {
            self.painted_mode = view_mode;
            if let Some(ix) = self.cursor_ix(&self.flat) {
                let item = match view_mode {
                    ViewMode::List => ix,
                    ViewMode::Icons => ix / self.measured_grid_cols().max(1),
                };
                self.scroll_handle
                    .scroll_to_item(item, ScrollStrategy::Nearest);
            }
        }

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
            // §8: which widget paints the same projection is the pane's
            // `ViewMode`. Both are `uniform_list`s over the same scroll
            // handle, and both read the same path-keyed selection — which is
            // what makes switching mode a pure re-render.
            match view_mode {
                ViewMode::List => details_list::render_rows(self, columns, cx).into_any_element(),
                ViewMode::Icons => {
                    // The one place `cols` is measured: from here on this
                    // frame — and every hit test against the pixels it paints
                    // — reads `painted_cols` instead.
                    let cols = self.measured_grid_cols();
                    self.painted_cols = cols;
                    // M4: keep the visible band's thumbnails coming. Driven
                    // from `render`, once per frame, off the scroll offset and
                    // viewport rather than off the row range `uniform_list`
                    // asks for — gpui calls that processor twice per frame
                    // with `0..1` just to measure an item, and a window that
                    // flipped like that would cancel and restart its own
                    // fetch on every repaint.
                    self.request_thumbnails(cols, window, cx);
                    icon_grid::render_grid(self, cols, cx).into_any_element()
                }
            }
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
            // §0: while the inline editor is up the context gains `renaming`,
            // which every `DirView && !renaming` binding is guarded by — the
            // rename row's own `TextInput` context still resolves, because it
            // sits *below* this node in the dispatch chain.
            // ...and while a context menu is up it gains `menu`, which is
            // what binds `escape` to `Cancel` for the menu only (§8
            // "dismiss-on-click-away"; escape is the keyboard half).
            .key_context(match (self.rename.is_some(), self.menu.is_some()) {
                (true, _) => "DirView renaming",
                (false, true) => "DirView menu",
                (false, false) => "DirView",
            })
            .on_action(cx.listener(|this, _: &OpenSelected, _, cx| this.open_selected(cx)))
            // §0 cursor movement. In the details list `down`/`up` step one
            // row; in the icon grid they step one *line* (±cols) and
            // `right`/`left` take over horizontal movement from
            // expand/collapse below — the §3 "2D keyboard nav" rows.
            .on_action(
                cx.listener(|this, _: &SelectNext, _, cx| match this.view_mode(cx) {
                    ViewMode::List => this.step_cursor(1, cx),
                    ViewMode::Icons => this.grid_step_cursor(icon_grid::GridStep::Down, cx),
                }),
            )
            .on_action(
                cx.listener(|this, _: &SelectPrev, _, cx| match this.view_mode(cx) {
                    ViewMode::List => this.step_cursor(-1, cx),
                    ViewMode::Icons => this.grid_step_cursor(icon_grid::GridStep::Up, cx),
                }),
            )
            .on_action(
                cx.listener(|this, _: &SelectFirst, _, cx| this.move_cursor_to_end(true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectLast, _, cx| this.move_cursor_to_end(false, cx)),
            )
            // §0 shift-arrows: extend the range from the anchor (M3).
            // §0 shift-arrows: extend the range from the anchor. The grid
            // extends by whole lines; unlike a cursor *move* it clamps rather
            // than holding at the last line, because a range that stops
            // growing at a ragged row cannot reach the final entries.
            .on_action(cx.listener(|this, _: &ExtendSelectionNext, _, cx| {
                let delta = match this.view_mode(cx) {
                    ViewMode::List => 1,
                    ViewMode::Icons => icon_grid::GridStep::Down.delta(this.grid_cols()),
                };
                this.extend_selection(delta, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendSelectionPrev, _, cx| {
                let delta = match this.view_mode(cx) {
                    ViewMode::List => -1,
                    ViewMode::Icons => icon_grid::GridStep::Up.delta(this.grid_cols()),
                };
                this.extend_selection(delta, cx)
            }))
            // The horizontal half of the same §0 row. Only the grid has an
            // axis for it: a details-list row is full width, so there is no
            // entry to its left or right to extend onto, and aliasing these
            // to up/down would make `shift-left` mean "extend backwards" in
            // one view and "extend a line" in the other.
            .on_action(cx.listener(|this, _: &ExtendSelectionRight, _, cx| {
                if this.view_mode(cx) == ViewMode::Icons {
                    this.extend_selection(icon_grid::GridStep::Right.delta(this.grid_cols()), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ExtendSelectionLeft, _, cx| {
                if this.view_mode(cx) == ViewMode::Icons {
                    this.extend_selection(icon_grid::GridStep::Left.delta(this.grid_cols()), cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &PageDown, _, cx| this.step_cursor(this.page_step(cx), cx)),
            )
            .on_action(
                cx.listener(|this, _: &PageUp, _, cx| this.step_cursor(-this.page_step(cx), cx)),
            )
            // §0 Views (M2): in-place expansion — a details-list affordance
            // (the grid has no disclosure triangles and no depth). In the
            // icon grid the same `right`/`left` keys are the horizontal half
            // of 2D navigation, which is what Explorer does in both views.
            .on_action(
                cx.listener(|this, _: &ExpandSelected, _, cx| match this.view_mode(cx) {
                    ViewMode::List => this.expand_selected(cx),
                    ViewMode::Icons => this.grid_step_cursor(icon_grid::GridStep::Right, cx),
                }),
            )
            .on_action(cx.listener(
                |this, _: &CollapseSelected, _, cx| match this.view_mode(cx) {
                    ViewMode::List => this.collapse_selected(cx),
                    ViewMode::Icons => this.grid_step_cursor(icon_grid::GridStep::Left, cx),
                },
            ))
            // §0 select-all over the visible projection (M3).
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            // §0 clipboard rows (M3): cut dims sources, paste moves on cut.
            .on_action(cx.listener(|this, _: &Cut, _, cx| this.cut_selection(cx)))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy_selection(cx)))
            .on_action(
                cx.listener(|this, action: &Paste, _, cx| this.paste_into(action.dest.clone(), cx)),
            )
            // §0 delete-to-trash (M3). DeletePermanently deliberately has no
            // handler here: it bubbles to the workspace, which owns the
            // ConfirmDialog guard.
            .on_action(
                cx.listener(|this, _: &DeleteToTrash, _, cx| this.delete_selection_to_trash(cx)),
            )
            // §0 rename (M3): `f2` here, slow second click in `handle_row_click`.
            .on_action(
                cx.listener(|this, _: &RenameSelected, window, cx| {
                    this.rename_selected(window, cx)
                }),
            )
            // §0 toolbar row (M3): duplicate with keep-both names.
            .on_action(cx.listener(|this, _: &Duplicate, _, cx| this.duplicate_selection(cx)))
            // §8 context menu: `escape` dismisses it. Only reachable while the
            // `menu` token is on this node, so it never shadows the rename
            // editor's own `Cancel` (that row's `TextInput` node is deeper).
            .on_action(
                cx.listener(|this, _: &Cancel, window, cx| this.close_context_menu(window, cx)),
            )
            .on_key_down(cx.listener(Self::handle_key_down))
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            // Sortable column headers belong to the details list; the grid
            // has no columns to sort by clicking (the `SortBy` action itself
            // still works — it lives on the pane, not on the header).
            .children(match view_mode {
                ViewMode::List => Some(details_list::render_header(&theme, sort, columns, cx)),
                ViewMode::Icons => None,
            })
            // The rows live inside the marquee's background surface (§8): it
            // is the element gpui's drag hangs off, and the positioning
            // context for the band it paints. The same element carries the
            // drop side of drag & drop (`drag::with_drop_handlers`) — one
            // element, so neither gesture adds a layout node.
            // ...and the right-click trigger plus the deferred menu overlay
            // (`context_menu::with_context_menu`), for the same reason: one
            // element, one geometry, no extra layout node.
            .child(context_menu::with_context_menu(
                drag::with_drop_handlers(marquee::list_surface(self, body, cx), self, cx),
                self,
                window,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    //! §9 dir_view rows for M2: expand injects children, collapse removes the
    //! subtree, the (path-keyed) cursor survives re-projection, and the
    //! `right`/`left` bindings dispatch on the real focused entity.

    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::pane::{Pane, ViewMode};
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

    /// A 60-entry `/grid`, open in the icon grid.
    fn open_grid(cx: &mut TestAppContext) -> (Entity<Pane>, &mut VisualTestContext) {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            let mut files = serde_json::Map::new();
            for i in 0..60 {
                files.insert(format!("f{i:03}.txt"), json!("x"));
            }
            vfs.insert_tree("/grid", serde_json::Value::Object(files));
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs,
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
        });
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| {
            pane.navigate_to(Path::new("/grid"), cx);
            pane.set_view_mode(ViewMode::Icons, cx);
        });
        cx.run_until_parked();
        (pane, cx)
    }

    /// Every tile whose painted centre is inside the list viewport, paired
    /// with the entry the pointer hit test names for that centre.
    fn tiles_vs_hit_tests(
        dir_view: &Entity<DirView>,
        cx: &mut VisualTestContext,
        len: usize,
    ) -> (usize, Vec<(usize, Option<usize>)>) {
        let viewport = dir_view.read_with(cx, |view, _| crate::marquee::list_viewport(view));
        let mut mismatches = Vec::new();
        let mut first_line = 0usize;
        for ix in 0..len {
            // `debug_bounds` wants a `'static` selector and these are built
            // per index; a test-only leak of a handful of short strings.
            let selector: &'static str = format!("dir-tile-{ix}").leak();
            let Some(bounds) = cx.debug_bounds(selector) else {
                continue;
            };
            if bounds.origin.y == viewport.origin.y {
                first_line += 1;
            }
            let centre = bounds.center();
            if !viewport.contains(&centre) {
                continue;
            }
            let hit = dir_view.read_with(cx, |view, cx| {
                let scroll_y = crate::marquee::scroll_y(view);
                view.index_at_content(
                    crate::marquee::ContentPoint::from_window(centre, viewport, scroll_y),
                    cx,
                )
            });
            if hit != Some(ix) {
                mismatches.push((ix, hit));
            }
        }
        (first_line, mismatches)
    }

    #[gpui::test]
    fn the_icon_grid_shows_the_folder_it_is_open_on_and_nothing_spliced_into_it(
        cx: &mut TestAppContext,
    ) {
        // An expansion made in the details list kept projecting its children
        // into the grid, where a tile carries no indentation, no disclosure
        // triangle, and `left`/`right` are 2D cursor motion rather than
        // expand/collapse — so the user saw entries of `/root/sub` sitting in
        // `/root` with no way to collapse them short of `cmd-1`.
        let (pane, cx) = open_root(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |view, cx| {
            view.toggle_expanded(Path::new("/root/sub"), cx)
        });
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
            "the details list splices the expansion in, as M2 built it"
        );

        pane.update(cx, |pane, cx| pane.set_view_mode(ViewMode::Icons, cx));
        cx.run_until_parked();
        assert_eq!(
            rows(&pane, cx),
            expect(&[("/root/sub", 0), ("/root/zeta", 0), ("/root/a.txt", 0)]),
            "the grid shows exactly what /root contains"
        );
        assert!(
            !rows(&pane, cx)
                .iter()
                .any(|(path, _)| path.to_string_lossy().contains("inner.txt")),
            "a child of an expanded folder must not paint as a top-level tile"
        );

        // ...and the expansion itself is intact, so cmd-1 restores the tree
        // rather than silently dropping state.
        pane.update(cx, |pane, cx| pane.set_view_mode(ViewMode::List, cx));
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
            "switching mode is a pure re-render, so the expansion survives it"
        );
    }

    // Switching mode preserved the selection but not its *visibility*: the
    // two views share one pixel scroll offset while the item metric changes
    // from a 22px row per entry to an 88px line per `cols` entries, so the
    // cursor the switch carefully preserved could be anywhere — including off
    // the top of the grid, with nothing to scroll it back. Explorer keeps the
    // selected item in view across a view change.
    #[gpui::test]
    fn switching_view_mode_scrolls_the_cursor_back_into_view(cx: &mut TestAppContext) {
        let (pane, cx) = open_grid(cx);
        // Eight tiles wide: a grid line then covers eight entries' worth of
        // list rows, so the offset that shows an entry in the list is far
        // below the one that shows it in the grid.
        cx.simulate_resize(gpui::size(px(800.0), px(400.0)));
        set_view_mode(&pane, cx, ViewMode::List);
        cx.run_until_parked();
        let dir_view = dir_view_of(&pane, cx);

        // Put the cursor on a middle entry and scroll it into view *as the
        // list*, which is the state a user arrives in.
        focus_dir_view(&pane, cx);
        cx.update(|_, cx| {
            dir_view.update(cx, |view, cx| {
                view.select_paths(&[Path::new("/grid/f029.txt")], cx)
            })
        });
        // A cursor *move* is what scrolls in the list, so step onto f030 the
        // way a user does.
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dir-row-30").is_some(),
            "the cursor is on screen in the list before the switch"
        );

        set_view_mode(&pane, cx, ViewMode::Icons);
        cx.run_until_parked();

        let viewport = dir_view.read_with(cx, |view, _| crate::marquee::list_viewport(view));
        let tile = cx
            .debug_bounds("dir-tile-30")
            .expect("the cursor's tile is painted after the switch, not scrolled past");
        assert!(
            viewport.contains(&tile.center()),
            "the cursor's tile is inside the grid viewport: tile={tile:?} viewport={viewport:?}"
        );
    }

    #[gpui::test]
    fn a_resized_grid_hit_tests_the_tile_the_pointer_is_actually_over(cx: &mut TestAppContext) {
        // `cols` used to be recomputed on demand from the scroll handle's
        // bounds, which gpui only writes during `prepaint` — so the frame
        // drawn *in response to* a resize painted the old width's columns
        // while every hit test already divided by the new one. The tile
        // showing `f020.txt` right-clicked as `f006.txt`, and Delete/Cut/
        // Rename from that menu acted on a file the user never pointed at.
        let (pane, cx) = open_grid(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        let wide = dir_view.read_with(cx, |view, _| view.grid_cols());
        assert!(wide > 6, "the default test window is wide: {wide} cols");

        // Exactly the one draw a real resize produces.
        cx.simulate_resize(gpui::size(gpui::px(600.0), gpui::px(900.0)));
        cx.run_until_parked();

        let narrow = dir_view.read_with(cx, |view, _| view.grid_cols());
        assert!(
            narrow < wide,
            "the grid converged on the narrower width: {wide} -> {narrow}"
        );
        let (first_line, mismatches) = tiles_vs_hit_tests(&dir_view, cx, 60);
        assert_eq!(
            mismatches,
            vec![],
            "every visible tile must hit-test as itself (cols={narrow})"
        );
        assert_eq!(
            first_line, narrow,
            "...and the painted line holds exactly the columns the arithmetic assumes"
        );
    }

    #[gpui::test]
    fn repeated_resizes_all_converge_without_an_unrelated_repaint(cx: &mut TestAppContext) {
        // The convergence notify has to fire on *every* width change, and has
        // to stop once the two agree — otherwise a resize either leaves the
        // grid stale or spins the window redrawing forever.
        let (pane, cx) = open_grid(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        for width in [900.0f32, 500.0, 720.0, 340.0] {
            cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(900.0)));
            cx.run_until_parked();
            let (first_line, mismatches) = tiles_vs_hit_tests(&dir_view, cx, 60);
            let cols = dir_view.read_with(cx, |view, _| view.grid_cols());
            assert_eq!(mismatches, vec![], "at {width}px wide, cols={cols}");
            assert_eq!(
                first_line, cols,
                "at {width}px wide the painted line and the arithmetic disagree"
            );
            assert_eq!(
                dir_view.read_with(cx, |view, _| view.measured_grid_cols()),
                cols,
                "at {width}px wide the grid had not settled: another frame is still owed"
            );
        }
    }

    fn open_root(cx: &mut TestAppContext) -> (Entity<Pane>, &mut VisualTestContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        (pane, cx)
    }

    /// `/root` open with a [`RecordingOpener`] installed, so a test can assert
    /// exactly which entries a command opened.
    #[allow(clippy::type_complexity)] // one test's setup tuple, not an API
    fn open_root_recording(
        cx: &mut TestAppContext,
    ) -> (
        Arc<std::sync::Mutex<Vec<PathBuf>>>,
        Entity<Pane>,
        &mut VisualTestContext,
    ) {
        let log: Arc<std::sync::Mutex<Vec<PathBuf>>> = Arc::default();
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({ "sub": {}, "a.txt": "a", "b.txt": "b", "c.txt": "c" }),
            );
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs,
                spawner,
                Arc::new(crate::app_state::RecordingOpener(log.clone())),
                Arc::new(fs_core::StubPlatform::new()),
            );
        });
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        (log, pane, cx)
    }

    // §0 "Open item" acts on the **selection**, not on the cursor row alone —
    // Explorer opens every selected item, and the row context menu's `Open`
    // (enabled for any non-empty selection) dispatches this same action, so
    // opening only the cursor row silently ignored the rest.
    #[gpui::test]
    fn open_selected_opens_every_selected_entry(cx: &mut TestAppContext) {
        let (opened, pane, cx) = open_root_recording(cx);
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(
                &[
                    Path::new("/root/a.txt"),
                    Path::new("/root/b.txt"),
                    Path::new("/root/c.txt"),
                ],
                cx,
            )
        });
        cx.update(|window, cx| {
            let handle = dir_view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            opened.lock().unwrap().clone(),
            vec![
                PathBuf::from("/root/a.txt"),
                PathBuf::from("/root/b.txt"),
                PathBuf::from("/root/c.txt"),
            ],
            "every selected file is opened, in projection order"
        );
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")), "no folder involved");
        });

        // A folder in the selection is entered — once, because one pane can
        // only show one directory — and the files still open.
        opened.lock().unwrap().clear();
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(&[Path::new("/root/sub"), Path::new("/root/a.txt")], cx)
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            opened.lock().unwrap().clone(),
            vec![PathBuf::from("/root/a.txt")]
        );
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root/sub")));
        });
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
    // Watcher invalidation of the injected expansion children (§6): a cached
    // child listing must not survive an external change to its folder.
    // ------------------------------------------------------------------

    /// Let the pane's watch debounce window elapse and its pump run.
    fn settle_watch(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(crate::pane::WATCH_LATENCY);
        cx.run_until_parked();
    }

    #[gpui::test]
    fn external_change_reloads_expanded_children_in_place(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();

        // The change is *inside* the expanded folder, not the watched
        // directory: nothing to patch, but the cached child listing is stale.
        vfs.insert_file("/root/sub/added.txt", 2);
        vfs.remove_path("/root/sub/inner.txt");
        settle_watch(cx);

        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/added.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
            ]),
            "an expanded folder re-lists in place when its contents change"
        );
    }

    #[gpui::test]
    fn expanding_after_an_external_change_shows_the_new_children(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        // Expand then collapse: the child listing is now cached but hidden.
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();

        vfs.insert_file("/root/sub/added.txt", 2);
        settle_watch(cx);

        // Re-expanding must not paint the stale cache.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/added.txt", 1),
                ("/root/sub/inner.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
            ]),
            "the invalidated folder re-lists on the next expansion"
        );
    }

    #[gpui::test]
    fn external_change_in_the_watched_dir_patches_the_projection(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        // A patch of the pane's snapshot re-projects with expansion intact.
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx);
        });
        cx.run_until_parked();

        vfs.insert_file("/root/b.txt", 1);
        settle_watch(cx);
        assert_eq!(
            rows(&pane, cx),
            expect(&[
                ("/root/sub", 0),
                ("/root/sub/deep", 1),
                ("/root/sub/inner.txt", 1),
                ("/root/zeta", 0),
                ("/root/a.txt", 0),
                ("/root/b.txt", 0),
            ]),
            "the watched directory's new row appears without collapsing the subtree"
        );
    }

    // The watcher removing an **expanded folder** takes its injected children
    // out of the projection with it, so a path-keyed selection must let go of
    // them: keeping them alive left the selection (and the cursor) pointing at
    // invisible, nonexistent rows that the next cut/paste, Duplicate or Delete
    // would happily act on.
    #[gpui::test]
    fn removing_an_expanded_folder_drops_its_children_from_the_selection(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/sub"), cx)
        });
        cx.run_until_parked();
        dir_view.update(cx, |dir_view, cx| {
            // Toggled last, so the cursor lands on the injected child row.
            dir_view.select_paths(
                &[Path::new("/root/a.txt"), Path::new("/root/sub/inner.txt")],
                cx,
            );
        });
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/sub/inner.txt")));
        });

        // The whole folder disappears from under the expansion. Its own removal
        // is an event in `/root`, the watched directory — the children are
        // never mentioned.
        vfs.remove_path("/root/sub");
        settle_watch(cx);

        assert_eq!(
            rows(&pane, cx),
            expect(&[("/root/zeta", 0), ("/root/a.txt", 0)]),
            "the folder and its injected children left the projection"
        );
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(
                dir_view.selection().selected_paths(),
                vec![PathBuf::from("/root/a.txt")],
                "the child of the vanished folder is no longer selected"
            );
            assert_eq!(
                dir_view.cursor(),
                None,
                "and the cursor is not left on a row that does not exist"
            );
            // ...and the expansion bookkeeping for the dead folder is gone
            // too, so it cannot come back pre-expanded from a stale cache if
            // a folder of the same name is created later.
            assert_eq!(
                dir_view.expansion_state_sizes(),
                (0, 0),
                "the vanished folder's expansion state and cached children were pruned"
            );
        });

        // A folder that is still listed keeps its expansion across the same
        // kind of patch — the pruning rule is "gone from the listing", not
        // "any change at all".
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/zeta"), cx)
        });
        cx.run_until_parked();
        vfs.insert_tree("/root/b.txt", serde_json::Value::String("b".into()));
        settle_watch(cx);
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(
                dir_view.expansion_state_sizes(),
                (1, 1),
                "an unrelated change must not collapse what is still there"
            );
        });
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
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.paste_into(None, cx)));
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
        cx.update(|_, cx| dir_view.update(cx, |view, cx| view.paste_into(None, cx)));
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
                view.paste_into(None, cx);
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

    // ------------------------------------------------------------------
    // M4 view modes (§8 "Icon grid")
    // ------------------------------------------------------------------

    /// The entry index the (path-keyed) cursor sits on, as the grid's
    /// arithmetic sees it.
    fn cursor_index(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<usize> {
        let dir_view = dir_view_of(pane, cx);
        cx.update(|_, cx| {
            let view = dir_view.read(cx);
            let rows = view.projected_rows(cx);
            view.cursor_ix(&rows)
        })
    }

    fn set_view_mode(pane: &Entity<Pane>, cx: &mut VisualTestContext, mode: ViewMode) {
        pane.update(cx, |pane, cx| pane.set_view_mode(mode, cx));
        cx.run_until_parked();
    }

    fn focus_dir_view(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
        let dir_view = dir_view_of(pane, cx);
        cx.update(|window, cx| {
            let handle = dir_view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });
    }

    // The reason the grid reads DirView's one `SelectionModel` instead of
    // owning tile indices: switching layout is a re-render of the same data,
    // so a multi-selection and the cursor have to come out the other side
    // untouched — a user who selects fifty files and then switches view must
    // not lose them (and must not delete a *different* fifty afterwards).
    #[gpui::test]
    fn switching_view_mode_preserves_selection_and_cursor(cx: &mut TestAppContext) {
        let (_vfs, pane, cx) = open_root_with_vfs(cx);
        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| {
            dir_view.update(cx, |view, cx| {
                // `select_paths` leaves the cursor on the last path, so
                // this is a two-entry selection with a cursor inside it.
                view.select_paths(&[Path::new("/root/sub"), Path::new("/root/a.txt")], cx);
            })
        });

        let before = cx.update(|_, cx| {
            let selection = dir_view.read(cx).selection();
            (selection.selected_paths(), selection.cursor().cloned())
        });
        assert_eq!(
            before.0,
            vec![PathBuf::from("/root/a.txt"), PathBuf::from("/root/sub")],
            "two entries selected before the switch"
        );

        for mode in [ViewMode::Icons, ViewMode::List, ViewMode::Icons] {
            set_view_mode(&pane, cx, mode);
            let after = cx.update(|_, cx| {
                let selection = dir_view.read(cx).selection();
                (selection.selected_paths(), selection.cursor().cloned())
            });
            assert_eq!(after, before, "selection survived the switch to {mode:?}");
            // Nothing reloaded either: the switch is a pure re-render.
            pane.read_with(cx, |pane, _| {
                assert_eq!(pane.path(), Some(Path::new("/root")));
                assert_eq!(pane.item_count(), 3);
            });
        }
    }

    // §8: 2D navigation is index arithmetic, and `up`/`down` step a whole
    // *line* of tiles rather than one entry. The arithmetic itself is
    // unit-tested in `views/icon_grid.rs`; this asserts the grid's `cols`
    // (from the painted width) is what the bindings actually apply, which is
    // the half that can silently regress to the list's ±1.
    #[gpui::test]
    fn grid_arrow_keys_step_by_a_whole_line(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let mut wide = serde_json::Map::new();
        for i in 0..60 {
            wide.insert(format!("f{i:03}.txt"), json!("x"));
        }
        vfs.insert_tree("/grid", serde_json::Value::Object(wide));
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/grid"), cx));
        cx.run_until_parked();
        set_view_mode(&pane, cx, ViewMode::Icons);

        let dir_view = dir_view_of(&pane, cx);
        let cols = cx.update(|_, cx| dir_view.read(cx).grid_cols());
        assert!(
            cols > 1,
            "the test window must be wider than one tile for this to mean anything"
        );

        focus_dir_view(&pane, cx);
        cx.update(|_, cx| {
            dir_view.update(cx, |view, cx| {
                view.set_cursor(Some(entry_id("/grid/f000.txt")), cx)
            })
        });

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(cursor_index(&pane, cx), Some(cols), "down = +cols");

        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(cursor_index(&pane, cx), Some(cols + 1), "right = +1");

        cx.simulate_keystrokes("left left");
        cx.run_until_parked();
        assert_eq!(
            cursor_index(&pane, cx),
            Some(cols - 1),
            "left walks back across the row boundary, in reading order"
        );

        cx.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(
            cursor_index(&pane, cx),
            Some(cols - 1),
            "up from the first row holds rather than wrapping"
        );
    }

    // §0 "Cursor movement (+shift- extends)" has a horizontal half that only
    // the grid has an axis for. Without `shift-right`/`shift-left` the grid
    // could grow a range only by a whole line (`shift-down` = +cols), so the
    // tile beside the cursor was unreachable by keyboard entirely.
    #[gpui::test]
    fn shift_arrows_extend_the_grid_range_by_one_tile(cx: &mut TestAppContext) {
        let (pane, cx) = open_grid(cx);
        let dir_view = dir_view_of(&pane, cx);
        let cols = cx.update(|_, cx| dir_view.read(cx).grid_cols());
        assert!(cols > 2, "a one-tile-wide grid proves nothing here");

        focus_dir_view(&pane, cx);
        cx.update(|_, cx| {
            dir_view.update(cx, |view, cx| {
                view.select_paths(&[Path::new("/grid/f003.txt")], cx)
            })
        });

        cx.simulate_keystrokes("shift-right");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| dir_view.read(cx).selection().selected_paths()),
            vec![
                PathBuf::from("/grid/f003.txt"),
                PathBuf::from("/grid/f004.txt")
            ],
            "one tile, not a whole line"
        );
        assert_eq!(cursor_index(&pane, cx), Some(4));

        // ...and back the other way, past the anchor, which is what makes the
        // range direction-agnostic rather than a growing-only band.
        cx.simulate_keystrokes("shift-left shift-left shift-left");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| dir_view.read(cx).selection().selected_paths()),
            vec![
                PathBuf::from("/grid/f001.txt"),
                PathBuf::from("/grid/f002.txt"),
                PathBuf::from("/grid/f003.txt")
            ],
            "the anchor held while the far end walked left"
        );
    }

    // The details list has nothing to the left or right of a full-width row,
    // so the same keys are deliberately inert there — an alias of shift-up /
    // shift-down would make one keystroke mean two different things.
    #[gpui::test]
    fn shift_arrows_are_inert_in_the_details_list(cx: &mut TestAppContext) {
        let (_vfs, pane, cx) = open_root_with_vfs(cx);
        let dir_view = dir_view_of(&pane, cx);
        focus_dir_view(&pane, cx);
        cx.update(|_, cx| {
            dir_view.update(cx, |view, cx| {
                view.select_paths(&[Path::new("/root/zeta")], cx)
            })
        });
        let before = cx.update(|_, cx| dir_view.read(cx).selection().selected_paths());

        cx.simulate_keystrokes("shift-right shift-left");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_, cx| dir_view.read(cx).selection().selected_paths()),
            before,
            "the list selection did not move"
        );
    }

    // The same `right`/`left` keys mean expand/collapse in the details list
    // and horizontal motion in the grid (§0 Views vs §3 2D nav). A regression
    // here is invisible in the list tests above, and would leave the grid
    // splicing children into a layout that has no depth to show them at.
    #[gpui::test]
    fn right_arrow_moves_in_the_grid_instead_of_expanding(cx: &mut TestAppContext) {
        let (_vfs, pane, cx) = open_root_with_vfs(cx);
        set_view_mode(&pane, cx, ViewMode::Icons);
        let dir_view = dir_view_of(&pane, cx);
        cx.update(|_, cx| {
            dir_view.update(cx, |view, cx| {
                view.set_cursor(Some(entry_id("/root/sub")), cx)
            })
        });
        focus_dir_view(&pane, cx);

        cx.simulate_keystrokes("right");
        cx.run_until_parked();
        assert_eq!(
            rows(&pane, cx),
            expect(&[("/root/sub", 0), ("/root/zeta", 0), ("/root/a.txt", 0)]),
            "no children were spliced in: the grid has no disclosure triangles"
        );
        assert_eq!(cursor_index(&pane, cx), Some(1), "the cursor moved instead");
    }
}
