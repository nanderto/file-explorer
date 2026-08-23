//! One browsing pane (ARCHITECTURE.md §2 `Pane`, §4a data flow).
//!
//! Owns the navigation state for a directory view: [`NavHistory`] whose
//! entries restore *state* (cursor + scroll), the per-pane [`ListingCache`]
//! for instant back/forward paints (render-cached-then-refresh), the current
//! [`ListingSnapshot`] guarded by a generation counter against stale loads,
//! and the status-line data (item count + free space via the Vfs).
//!
//! M1 note: the pane owns the listing pipeline (snapshot, cache, generation
//! guard) and scroll bookkeeping; the cursor/selection is **owned by the
//! child [`DirView`]** (ARCHITECTURE.md §2) — the accessors here delegate so
//! `NavEntry` capture/restore keeps working against the one true cursor.
//!
//! The open directory is kept **live** by its own debounced watch
//! (`Pane::start_watch`): each batch is resolved off the UI thread and folded
//! in with [`fs_core::patch_listing`], so external changes — and completed
//! file operations — appear with no explicit `Refresh` (§4b: "no explicit
//! refresh — the dest dir's watcher batch patches the listing"). Because the
//! pane owns the snapshot, the watch lives here rather than in the `DirView`
//! as §4a's diagram sketches.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs_core::{
    EntryId, ListingCache, ListingSnapshot, ResolvedBatch, SortDirection, SortKey, SortSpec, Vfs,
    WatchGuard, list_dir, patch_listing, resolve_watch_batch,
};
use futures::StreamExt as _;
use gpui::{
    App, BackgroundExecutor, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseButton, NavigationDirection, Render, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};

use crate::actions::{
    GoBack, GoForward, GoUp, NewFile, NewFolder, Refresh, SetViewColumns, SetViewIcons,
    SetViewList, SortBy,
};
use crate::address_bar::{AddressBar, AddressBarEvent};
use crate::app_state::FsContext;
use crate::dir_view::{DirView, DirViewEvent};
use crate::rename::NewEntryKind;
use crate::theme::Theme;

/// Debounce window for the open directory's watcher (ARCHITECTURE.md §4a:
/// `vfs.watch(path, 100ms)`). Runs on [`fs_core::Spawner::timer`], so
/// `#[gpui::test]`s drive it with `advance_clock`.
pub const WATCH_LATENCY: Duration = Duration::from_millis(100);

/// Events up (ARCHITECTURE.md §2): the workspace subscribes and acts.
pub enum PaneEvent {
    /// A watcher batch reported external changes inside these directories.
    /// The pane has already patched its own listing and invalidated its
    /// details view's expansion children; the workspace forwards this to the
    /// sidebar, whose tree caches child listings of its own.
    DirsChanged(Vec<Arc<Path>>),
    /// Focus entered this pane — its own node or any descendant (the details
    /// view or icon grid, the address-bar editor, the rename editor). The
    /// workspace makes the emitting pane the **active** one, so every
    /// workspace-level command (undo/redo, hidden-files toggle, `cmd-l`,
    /// delete-permanently, sidebar navigation) targets the pane the user is
    /// actually working in rather than pane 0 (M4 dual pane).
    FocusIn,
}

/// One history slot: where we were **and** what it looked like — back/forward
/// must restore cursor and scroll, not just location (ARCHITECTURE.md §2).
#[derive(Clone, Debug, PartialEq)]
pub struct NavEntry {
    pub path: PathBuf,
    /// Path-keyed; ignored on restore if the entry no longer exists.
    pub cursor: Option<EntryId>,
    /// uniform_list logical offset.
    pub scroll_top: f32,
}

impl NavEntry {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cursor: None,
            scroll_top: 0.0,
        }
    }
}

/// Back/forward stacks of [`NavEntry`]s. Navigating anywhere (including
/// [`GoUp`]) truncates the forward stack.
#[derive(Default)]
pub struct NavHistory {
    back: Vec<NavEntry>,
    forward: Vec<NavEntry>,
}

impl NavHistory {
    /// Record a plain navigation away from `from` (enter folder, breadcrumb,
    /// go-up): pushes the departed state and clears the forward stack.
    pub fn record_navigation(&mut self, from: Option<NavEntry>) {
        if let Some(from) = from {
            self.back.push(from);
        }
        self.forward.clear();
    }

    /// Pop the back stack, capturing `current` onto the forward stack.
    pub fn pop_back(&mut self, current: NavEntry) -> Option<NavEntry> {
        let target = self.back.pop()?;
        self.forward.push(current);
        Some(target)
    }

    /// Pop the forward stack, capturing `current` onto the back stack.
    pub fn pop_forward(&mut self, current: NavEntry) -> Option<NavEntry> {
        let target = self.forward.pop()?;
        self.back.push(current);
        Some(target)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

/// A [`WatchGuard`] whose *unregistration* is kept off the UI thread.
///
/// Registering a watch is not the only blocking, disk-touching half of
/// `Vfs::watch`: dropping the guard calls the backend's `unwatch`, which on
/// macOS stops and joins an FSEvents run-loop thread and canonicalizes the
/// path again. So the guard is never dropped in place — dropping this wrapper
/// hands it to the background executor (§5: the UI thread never touches the
/// disk).
struct BackgroundWatchGuard {
    guard: Option<WatchGuard>,
    executor: BackgroundExecutor,
}

impl BackgroundWatchGuard {
    fn new(guard: WatchGuard, executor: BackgroundExecutor) -> Self {
        Self {
            guard: Some(guard),
            executor,
        }
    }
}

impl Drop for BackgroundWatchGuard {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            self.executor
                .spawn(async move {
                    drop(guard);
                })
                .detach();
        }
    }
}

/// What `SetViewColumns` tells the user while `views/columns.rs` is still a
/// §8 stretch item. A constant so the toast text and its test cannot drift.
pub const COLUMNS_UNAVAILABLE_NOTICE: &str = "Column view isn't available yet";

/// Whether the address bar renders as breadcrumb segments or as the editable
/// path input (ARCHITECTURE.md §2). The input itself is `address_bar.rs` (M1,
/// separate build step); the mode lives here because the pane owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressBarMode {
    Breadcrumb,
    Editing,
}

/// How the pane's [`DirView`] lays its entries out (ARCHITECTURE.md §2
/// "Pane … view_mode", §8 "Icon grid"). Only the two shipped modes exist:
/// Miller columns are a post-v1 stretch (§8), so there is no `Columns`
/// variant to switch *into* — see [`Pane::set_view_columns`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    /// The M1 details list: one fixed-height row per entry, sortable columns.
    #[default]
    List,
    /// The M4 icon grid: fixed-size tiles, `cols` recomputed from pane width.
    Icons,
}

impl ViewMode {
    /// The *other* shipped mode. A fresh split pane opens in it (plan §2's
    /// blueprint screenshot is a details list beside an icon grid), so the
    /// split is immediately useful as two different views of the same tree.
    pub fn complement(self) -> Self {
        match self {
            ViewMode::List => ViewMode::Icons,
            ViewMode::Icons => ViewMode::List,
        }
    }
}

pub struct Pane {
    focus_handle: FocusHandle,
    theme: Theme,
    vfs: Arc<dyn Vfs>,
    history: NavHistory,
    /// Directory currently shown (target of the newest load).
    path: Option<Arc<Path>>,
    /// Newest snapshot: fresh, or a cached/previous one while loading.
    snapshot: Option<Arc<ListingSnapshot>>,
    /// True while `snapshot` is a cached or previous-directory paint awaiting
    /// the fresh load (§6 render-cached-then-refresh).
    snapshot_is_stale: bool,
    /// The details view; owns the (path-keyed) cursor/selection.
    dir_view: Entity<DirView>,
    /// The editable-path editor swapped in for the breadcrumb (§8).
    address_bar_view: Entity<AddressBar>,
    scroll_top: f32,
    /// Restore waiting for the fresh snapshot (cache misses).
    pending_restore: Option<NavEntry>,
    address_bar: AddressBarMode,
    /// Details list or icon grid (§0 "View mode switcher"). Lives here, not
    /// on the DirView, because the pane owns the toolbar control and the §0
    /// handler — and because the DirView's selection must survive the switch.
    view_mode: ViewMode,
    sort: SortSpec,
    show_hidden: bool,
    cache: ListingCache,
    /// Navigation race guard: loads carry the generation they were spawned
    /// with; results from an older generation are dropped, never rendered.
    generation: u64,
    free_space: Option<u64>,
    load_error: Option<String>,
    _load_task: Option<Task<()>>,
    _free_space_task: Option<Task<()>>,
    /// Registration for the open directory's watch: dropping it unregisters
    /// (§6, off the UI thread — see [`BackgroundWatchGuard`]), and it is
    /// replaced whenever the *directory* changes — so navigating away stops
    /// watching the directory we left, and dropping the pane stops everything.
    _watch_guard: Option<BackgroundWatchGuard>,
    /// The pump folding that watch's debounced batches into the snapshot; a
    /// field, never detached (§5), so it dies with the pane.
    _watch_pump: Option<Task<()>>,
    /// Bumped only when the *watched directory* changes, so an in-place reload
    /// (refresh, sort flip, hidden toggle) does not invalidate the live pump.
    watch_generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl Pane {
    pub fn new(theme: Theme, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let vfs = FsContext::global(cx).vfs.clone();
        let focus_handle = cx.focus_handle();
        // Events up (§2): the workspace tracks the active pane **by focus**,
        // and gpui's focus-on-mouse-down means any click inside this pane
        // (list row, breadcrumb, status line) lands on a descendant handle —
        // hence `on_focus_in`, which fires for the subtree, not just this node.
        let focus_subscription = cx.on_focus_in(&focus_handle, window, |_, _, cx| {
            cx.emit(PaneEvent::FocusIn);
        });
        let pane = cx.weak_entity();
        let dir_view = cx.new(|cx| DirView::new(theme.clone(), pane, cx));
        // Events up, method calls down (§2): the DirView reports opened
        // folders; the pane navigates.
        let subscription = cx.subscribe(&dir_view, |this, _, event, cx| match event {
            DirViewEvent::NavigateTo(path) => this.navigate_to(path, cx),
        });
        let address_bar_view = cx.new(|cx| AddressBar::new(theme.clone(), cx));
        // §8 address bar: confirmed paths navigate; escape/cancel restores the
        // breadcrumb; both hand keyboard focus back to the pane.
        let bar_subscription = cx.subscribe_in(
            &address_bar_view,
            window,
            |this, _, event: &AddressBarEvent, window, cx| match event {
                AddressBarEvent::Navigated(path) => {
                    this.navigate_to(path, cx);
                    window.focus(&this.focus_handle, cx);
                }
                AddressBarEvent::Cancelled => {
                    this.address_bar = AddressBarMode::Breadcrumb;
                    window.focus(&this.focus_handle, cx);
                    cx.notify();
                }
            },
        );
        Self {
            address_bar_view,
            focus_handle,
            theme,
            vfs,
            history: NavHistory::default(),
            path: None,
            snapshot: None,
            snapshot_is_stale: false,
            dir_view,
            scroll_top: 0.0,
            pending_restore: None,
            address_bar: AddressBarMode::Breadcrumb,
            view_mode: ViewMode::default(),
            sort: SortSpec::default(),
            show_hidden: false,
            cache: ListingCache::default(),
            generation: 0,
            free_space: None,
            load_error: None,
            _load_task: None,
            _free_space_task: None,
            _watch_guard: None,
            _watch_pump: None,
            watch_generation: 0,
            _subscriptions: vec![subscription, bar_subscription, focus_subscription],
        }
    }

    // ------------------------------------------------------------------
    // Navigation (§4a)
    // ------------------------------------------------------------------

    /// Enter a directory (Enter/double-click on a folder, breadcrumb click,
    /// confirmed address-bar path). Pushes the departed state and clears the
    /// forward stack.
    pub fn navigate_to(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.path.as_deref() == Some(path) {
            return;
        }
        let departed = self.current_nav_entry(cx);
        self.history.record_navigation(departed);
        self.load(Arc::from(path), None, cx);
    }

    /// Parent directory (`GoUp`: backspace / alt-up). A plain navigation.
    pub fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(parent) = self.path.as_deref().and_then(Path::parent) else {
            return;
        };
        let parent = parent.to_path_buf();
        self.navigate_to(&parent, cx);
    }

    /// Back (`cmd-[` / mouse button 4), restoring the popped entry's cursor
    /// and scroll.
    pub fn go_back(&mut self, cx: &mut Context<Self>) {
        let Some(current) = self.current_nav_entry(cx) else {
            return;
        };
        if let Some(target) = self.history.pop_back(current) {
            self.load(Arc::from(target.path.as_path()), Some(target), cx);
        }
    }

    /// Forward (`cmd-]` / mouse button 5), restoring cursor and scroll.
    pub fn go_forward(&mut self, cx: &mut Context<Self>) {
        let Some(current) = self.current_nav_entry(cx) else {
            return;
        };
        if let Some(target) = self.history.pop_forward(current) {
            self.load(Arc::from(target.path.as_path()), Some(target), cx);
        }
    }

    /// Reload the current directory (`cmd-r`), preserving cursor and scroll.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        // Bypass the cached (possibly what we're refreshing away from) paint.
        self.cache.invalidate(&path);
        let restore = self.current_nav_entry(cx);
        self.load(path, restore, cx);
    }

    /// Change the sort column: same key flips direction, new key sorts
    /// ascending. Dispatched by header clicks (`SortBy` action).
    pub fn sort_by(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if self.sort.key == key {
            self.sort.direction = self.sort.direction.flipped();
        } else {
            self.sort.key = key;
            self.sort.direction = SortDirection::Ascending;
        }
        self.reload_in_place(cx);
    }

    /// Show or hide dotfiles; the workspace fans this out to every pane.
    pub fn set_show_hidden(&mut self, show_hidden: bool, cx: &mut Context<Self>) {
        if self.show_hidden == show_hidden {
            return;
        }
        self.show_hidden = show_hidden;
        self.reload_in_place(cx);
    }

    /// Switch layout (§0 "View mode switcher": `cmd-1`/`cmd-2` and the
    /// toolbar control, both dispatching the same boxed actions). Nothing
    /// reloads — the listing, the sort and the path-keyed selection are the
    /// same data drawn differently, which is exactly why the selection
    /// survives the switch.
    pub fn set_view_mode(&mut self, view_mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == view_mode {
            return;
        }
        self.view_mode = view_mode;
        cx.notify();
    }

    /// `SetViewColumns` (§0 row, M4) with §8's answer: Miller columns are a
    /// post-v1 stretch, so this says so in a toast instead of silently doing
    /// nothing. Declaring the action keeps the §0 table complete (the native
    /// menu bar at M8 dispatches the same one); the unimplemented *view* is
    /// visible to the user, not swallowed.
    pub fn set_view_columns(&mut self, cx: &mut Context<Self>) {
        let jobs = FsContext::global(cx).jobs.clone();
        jobs.update(cx, |jobs, cx| {
            jobs.push_notice(COLUMNS_UNAVAILABLE_NOTICE.to_string(), cx);
        });
    }

    /// Swap the breadcrumb for the editable path input (`cmd-l`, forwarded by
    /// the workspace). The input entity itself is the address-bar build step.
    pub fn focus_address_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.address_bar = AddressBarMode::Editing;
        let path = self.path.clone();
        self.address_bar_view.update(cx, |bar, cx| {
            bar.begin_editing(path.as_deref(), window, cx);
        });
        cx.notify();
    }

    /// §0 "New folder" (`cmd-shift-n`, context menu).
    pub fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create_new_entry(NewEntryKind::Folder, window, cx);
    }

    /// §0 "New text file" (context menu **New ▸ Text file…** — no key row).
    pub fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.create_new_entry(NewEntryKind::File, window, cx);
    }

    /// §0's handler column for both rows is "Pane → DirView": the pane owns
    /// the destination directory and picks the non-conflicting placeholder
    /// name, then the details view opens the §4c inline editor on a phantom
    /// row so the user names it. The `CreateDir`/`CreateFile` op is submitted
    /// by that editor's `Confirm` (see [`crate::rename`]), not here — so a
    /// cancelled naming leaves nothing behind.
    fn create_new_entry(
        &mut self,
        kind: NewEntryKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.path.clone() else {
            return;
        };
        let existing: std::collections::BTreeSet<String> = self
            .snapshot
            .as_ref()
            .map(|snap| snap.entries.iter().map(|e| e.name.to_string()).collect())
            .unwrap_or_default();
        let name = match kind {
            NewEntryKind::Folder => next_available_name("New Folder", "", &existing),
            NewEntryKind::File => next_available_name("New Text File", ".txt", &existing),
        };
        self.dir_view.update(cx, |dir_view, cx| {
            dir_view.begin_new_entry(kind, &dir, &name, window, cx);
        });
    }

    fn reload_in_place(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            cx.notify();
            return;
        };
        let restore = self.current_nav_entry(cx);
        self.load(path, restore, cx);
    }

    /// §4a: cache hit paints instantly (restore applied in the same frame);
    /// the fresh `list_dir` always runs on the background executor and
    /// replaces it, guarded by the generation counter.
    fn load(&mut self, path: Arc<Path>, restore: Option<NavEntry>, cx: &mut Context<Self>) {
        // Whether this load actually *leaves* the current directory, as
        // opposed to reloading it in place (refresh, sort flip, hidden toggle,
        // cache-miss back/forward to the same dir). Two things hang off it.
        let path_changed = self.path.as_deref() != Some(path.as_ref());
        // §4c "navigating away tears the editor down cleanly": an in-place
        // reload keeps an open rename; actually leaving the directory does not.
        if path_changed {
            self.dir_view
                .update(cx, |dir_view, cx| dir_view.cancel_rename_for_navigation(cx));
        }
        self.generation += 1;
        let generation = self.generation;
        self.path = Some(path.clone());
        self.load_error = None;
        self.address_bar = AddressBarMode::Breadcrumb;
        self.pending_restore = restore;

        let cached = self
            .cache
            .get(&path)
            .filter(|snap| snap.sort == self.sort && snap.show_hidden == self.show_hidden);
        if let Some(cached) = cached {
            self.snapshot = Some(cached);
            self.snapshot_is_stale = true;
            let restore = self.pending_restore.take();
            self.apply_restore(restore, cx);
        } else {
            self.snapshot_is_stale = self.snapshot.is_some();
            // Plain navigation (no restore pending): the selection does not
            // carry across directories. In-place reloads (refresh, sort flip,
            // hidden toggle, cache-miss back/forward) keep the current
            // selection visible on the old rows until the fresh load lands —
            // apply_restore then prunes and re-places the cursor.
            if self.pending_restore.is_none() {
                self.scroll_top = 0.0;
                self.dir_view.update(cx, |dir_view, cx| {
                    dir_view.set_cursor(None, cx);
                    dir_view.apply_scroll_top(0.0);
                });
            }
        }
        cx.notify();

        let vfs = self.vfs.clone();
        let sort = self.sort;
        let show_hidden = self.show_hidden;
        let load_path = path.clone();
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(list_dir(vfs, load_path, sort, show_hidden, generation))
                .await;
            this.update(cx, |this, cx| {
                if generation != this.generation {
                    return; // stale load — a newer navigation superseded it
                }
                match result {
                    Ok(snapshot) => {
                        let snapshot = Arc::new(snapshot);
                        this.cache.insert(snapshot.clone());
                        this.snapshot = Some(snapshot);
                        this.snapshot_is_stale = false;
                        let restore = this.pending_restore.take();
                        this.apply_restore(restore, cx);
                    }
                    Err(error) => {
                        this.snapshot_is_stale = false;
                        this.load_error = Some(format!("{error:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));

        // §4a: the open directory is kept live by its own watcher — no
        // explicit refresh after a file operation. Only (re)registered when
        // the directory really changed: an in-place reload watches the same
        // path, and tearing the OS watch down and back up would both cost a
        // full stop/restart cycle and open a window in which real changes are
        // lost (a fresh stream starts from *now*, not from where the old one
        // stopped).
        if path_changed || self._watch_pump.is_none() {
            self.watch_generation += 1;
            let watch_generation = self.watch_generation;
            self.start_watch(path.clone(), watch_generation, cx);
        }

        let vfs = self.vfs.clone();
        self._free_space_task = Some(cx.spawn(async move |this, cx| {
            let free = cx
                .background_spawn(async move { vfs.free_space(&path).await })
                .await
                .ok();
            this.update(cx, |this, cx| {
                if generation == this.generation {
                    this.free_space = free;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    // ------------------------------------------------------------------
    // Live directory (§4a watcher patch loop, §6 watcher.rs)
    // ------------------------------------------------------------------

    /// Watch the directory this load is for. The guard and the pump are both
    /// fields: assigning them here drops the previous directory's watch (the
    /// stream then ends, so the previous pump exits) and any pane drop tears
    /// everything down. `generation` is the *watch* generation, so a batch
    /// resolved for a directory the pane has already left can never apply,
    /// while an in-place reload leaves the live pump alone.
    ///
    /// **Registration runs on the background executor.** `Vfs::watch` is a
    /// blocking, disk-touching call — for `RealVfs` it stats and canonicalizes
    /// the path and stops/starts the backend's run-loop thread — so it may
    /// never be called from `render`/`load` on the UI thread (§5). Both halves
    /// it returns are `Send`, so the pump task registers the watch itself and
    /// only comes back to the UI thread to store the guard.
    fn start_watch(&mut self, path: Arc<Path>, generation: u64, cx: &mut Context<Self>) {
        let vfs = self.vfs.clone();
        let register_vfs = self.vfs.clone();
        let register_path = path.clone();
        let executor = cx.background_executor().clone();
        self._watch_pump = Some(cx.spawn(async move |this, cx| {
            let (mut stream, guard) = cx
                .background_spawn(async move { register_vfs.watch(&register_path, WATCH_LATENCY) })
                .await;
            // Wrapped *before* it can be dropped anywhere, so every path out
            // of here — stored, superseded, or pane gone — unregisters off the
            // UI thread. Storing it drops the previous directory's guard, which
            // ends that stream and so retires its pump.
            let guard = BackgroundWatchGuard::new(guard, executor);
            let stored = this.update(cx, |this, _| {
                if generation != this.watch_generation {
                    return false; // superseded while registering
                }
                this._watch_guard = Some(guard);
                true
            });
            if !stored.unwrap_or(false) {
                return;
            }
            while let Some(batch) = stream.next().await {
                // Generation-check *before* any I/O: nothing to stat for a
                // directory we have left.
                match this.read_with(cx, |this, _| this.watch_generation == generation) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => return, // pane dropped
                }
                // Every stat runs on the background executor (§5).
                let resolved = cx
                    .background_spawn(resolve_watch_batch(vfs.clone(), path.clone(), batch))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    this.apply_watch_batch(generation, resolved, cx);
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// Fold one resolved batch into the pane: invalidate stale child listings,
    /// then either reload (`Rescan`) or patch the snapshot in place, keeping
    /// the path-keyed selection and the cursor correct.
    fn apply_watch_batch(
        &mut self,
        generation: u64,
        resolved: ResolvedBatch,
        cx: &mut Context<Self>,
    ) {
        if generation != self.watch_generation {
            return; // the pane navigated away while the batch was resolving
        }
        // Cached child listings must not survive an external change to the
        // folder they came from: the details view's injected expansion
        // children here, the sidebar tree's own cache via the workspace.
        if !resolved.changed_dirs.is_empty() {
            let dirs = resolved.changed_dirs.clone();
            self.dir_view
                .update(cx, |dir_view, cx| dir_view.invalidate_children(&dirs, cx));
            cx.emit(PaneEvent::DirsChanged(resolved.changed_dirs));
        }
        if resolved.reload {
            // Events were dropped: the incremental path is untrustworthy (§6).
            self.refresh(cx);
            return;
        }
        if resolved.patches.is_empty() {
            return;
        }
        // Patch only a snapshot of the watched directory: during navigation
        // the visible snapshot can still be the previous directory's paint.
        let Some(snapshot) = self.snapshot.clone() else {
            return; // nothing painted yet — the in-flight load carries the change
        };
        if Some(&*snapshot.dir) != self.path.as_deref() {
            return;
        }
        let patched = Arc::new(patch_listing(&snapshot, resolved.patches));
        // §6: patched snapshots are written back, so re-entering a watched
        // directory is exact, not just close. `snapshot_is_stale` is left
        // alone — a stale paint stays stale until its fresh load lands.
        self.cache.insert(patched.clone());
        self.snapshot = Some(patched);
        // A patch is **not** a navigation, so it does not run the NavEntry
        // restore: `retain_selection_in_listing` already prunes vanished paths
        // and clears a dangling cursor, and re-applying `scroll_top` would
        // yank the list back to wherever the last navigation left it (nothing
        // updates that field while the user scrolls) on every external change.
        self.prune_view_state(cx);
        cx.notify();
    }

    /// Everything the view must forget when rows leave the listing: selected
    /// paths that vanished, and an inline editor whose row went with them.
    /// Reads only the pane's own snapshot, so it is safe to call mid-update.
    fn prune_view_state(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.snapshot.clone();
        self.dir_view.update(cx, |dir_view, cx| {
            dir_view.retain_selection_in_listing(snapshot.as_deref(), cx);
            // §4c: an editor whose target the filesystem removed under it would
            // otherwise keep the `renaming` key context — and with it every
            // dead `DirView && !renaming` binding — forever.
            dir_view.cancel_rename_if_target_vanished(snapshot.as_deref(), cx);
        });
    }

    /// Apply a [`NavEntry`]'s cursor + scroll against the current snapshot
    /// (restore semantics). Either way, selected paths that vanished from the
    /// listing are pruned (path-keyed survival, §2); a restore re-places the
    /// cursor **without** collapsing a wider selection, so multi-selections
    /// survive refresh/re-sort. The selection lives in the [`DirView`].
    fn apply_restore(&mut self, restore: Option<NavEntry>, cx: &mut Context<Self>) {
        self.prune_view_state(cx);
        if let Some(entry) = restore {
            let cursor = entry.cursor.filter(|id| self.listing_contains(id, cx));
            self.scroll_top = entry.scroll_top;
            let scroll_top = entry.scroll_top;
            self.dir_view.update(cx, |dir_view, cx| {
                dir_view.restore_cursor(cursor, cx);
                dir_view.apply_scroll_top(scroll_top);
            });
        }
    }

    /// Whether a path is visible in the listing: in the snapshot, or injected
    /// by the DirView's in-place expansion (M2) — so a cursor sitting on an
    /// expanded folder's child survives fresh loads and refreshes. The rule
    /// itself lives in the [`DirView`], which owns the expansion state; the
    /// pane supplies the snapshot it is restoring against.
    fn listing_contains(&self, id: &EntryId, cx: &App) -> bool {
        self.dir_view
            .read(cx)
            .listing_contains(self.snapshot.as_deref(), id)
    }

    fn current_nav_entry(&self, cx: &App) -> Option<NavEntry> {
        Some(NavEntry {
            path: self.path.as_deref()?.to_path_buf(),
            cursor: self.cursor(cx),
            scroll_top: self.scroll_top,
        })
    }

    // ------------------------------------------------------------------
    // State read/write for the DirView and tests
    // ------------------------------------------------------------------

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn snapshot(&self) -> Option<&Arc<ListingSnapshot>> {
        self.snapshot.as_ref()
    }

    /// True while the visible snapshot is a cached/previous paint awaiting
    /// the fresh load.
    pub fn snapshot_is_stale(&self) -> bool {
        self.snapshot_is_stale
    }

    /// The cursor, delegated to the owning [`DirView`] (path-keyed, §2).
    pub fn cursor(&self, cx: &App) -> Option<EntryId> {
        self.dir_view.read(cx).cursor().cloned()
    }

    pub fn set_cursor(&mut self, cursor: Option<EntryId>, cx: &mut Context<Self>) {
        self.dir_view
            .update(cx, |dir_view, cx| dir_view.set_cursor(cursor, cx));
    }

    pub fn dir_view(&self) -> &Entity<DirView> {
        &self.dir_view
    }

    pub fn scroll_top(&self) -> f32 {
        self.scroll_top
    }

    pub fn set_scroll_top(&mut self, scroll_top: f32) {
        self.scroll_top = scroll_top;
    }

    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    pub fn address_bar_mode(&self) -> AddressBarMode {
        self.address_bar
    }

    pub fn sort(&self) -> SortSpec {
        self.sort
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn free_space(&self) -> Option<u64> {
        self.free_space
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn can_go_back(&self) -> bool {
        self.history.can_go_back()
    }

    pub fn can_go_forward(&self) -> bool {
        self.history.can_go_forward()
    }

    pub fn item_count(&self) -> usize {
        self.snapshot.as_ref().map_or(0, |snap| snap.entries.len())
    }

    /// Status line per plan §3: item count, and free space once a directory
    /// is open.
    pub fn status_text(&self) -> String {
        let count = self.item_count();
        let items = format!("{count} item{}", if count == 1 { "" } else { "s" });
        match self.free_space {
            Some(bytes) if self.path.is_some() => {
                format!("{items} · {} free", format_bytes(bytes))
            }
            _ => items,
        }
    }
}

/// First free `"base<ext>"`, `"base 2<ext>"`, `"base 3<ext>"`, … name among
/// `existing` (Explorer-style New Folder naming; distinct from ops'
/// keep-both `"name copy"` sequence, which is for collision copies).
fn next_available_name(
    base: &str,
    ext: &str,
    existing: &std::collections::BTreeSet<String>,
) -> String {
    let first = format!("{base}{ext}");
    if !existing.contains(&first) {
        return first;
    }
    (2u32..)
        .map(|i| format!("{base} {i}{ext}"))
        .find(|name| !existing.contains(name))
        .expect("candidate sequence is unbounded")
}

/// Humanize a byte count for the status line ("13.7 GB free").
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl EventEmitter<PaneEvent> for Pane {}

impl Focusable for Pane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Pane {
    /// The address-bar row (§8): breadcrumb segments, or the editor while
    /// `AddressBarMode::Editing`. Clicking blank space enters editing mode.
    fn render_chrome_row(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let row = div()
            .flex()
            .items_start()
            .min_h(px(32.0))
            .px(px(8.0))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(13.0));

        if self.address_bar == AddressBarMode::Editing {
            return row
                .child(div().flex_1().child(self.address_bar_view.clone()))
                .child(self.render_view_switcher(cx));
        }

        // Breadcrumb: one clickable segment per path component; blank space
        // to the right enters editing (Explorer behavior).
        let mut segments = div().flex().items_center().gap(px(2.0));
        if let Some(path) = &self.path {
            let mut ancestor = PathBuf::new();
            let components: Vec<_> = path.components().collect();
            let count = components.len();
            for (i, component) in components.into_iter().enumerate() {
                ancestor.push(component.as_os_str());
                let target: Arc<Path> = Arc::from(ancestor.as_path());
                let label = match component {
                    std::path::Component::RootDir => continue, // separator glyph covers it
                    other => other.as_os_str().to_string_lossy().into_owned(),
                };
                if i > 0 {
                    segments = segments
                        .child(div().text_color(theme.muted).child(SharedString::from("›")));
                }
                segments = segments.child(
                    div()
                        .id(("breadcrumb-segment", i))
                        .px(px(4.0))
                        .rounded(px(3.0))
                        .text_color(if i + 1 == count {
                            theme.text
                        } else {
                            theme.muted
                        })
                        .hover(|s| s.bg(theme.accent.opacity(0.15)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate_to(&target, cx);
                        }))
                        .child(SharedString::from(label)),
                );
            }
        }
        row.child(segments)
            .child(
                // Blank filler: click to edit the path (cmd-l equivalent).
                div()
                    .id("breadcrumb-blank")
                    .flex_1()
                    .h_full()
                    .min_h(px(20.0))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.focus_address_bar(window, cx);
                    })),
            )
            .child(self.render_view_switcher(cx))
    }

    /// The §0 "View mode switcher" toolbar control: two segmented buttons
    /// that **dispatch the same boxed actions** the keymap binds, so the
    /// switch logic exists exactly once (§0) — the buttons know nothing about
    /// `view_mode` beyond which of them is currently lit.
    ///
    /// Each button focuses the pane before dispatching, like the details
    /// list's sort headers: the action then travels the focus chain to this
    /// pane's handler regardless of where focus was (a click on a control is
    /// not a reason for the action to land in a *different* pane).
    fn render_view_switcher(&self, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let mut switcher = div()
            .flex()
            .items_center()
            .flex_none()
            .gap(px(2.0))
            .ml(px(8.0));
        for (id, label, mode, action) in [
            (
                "view-mode-list",
                "☰",
                ViewMode::List,
                Box::new(SetViewList) as Box<dyn gpui::Action>,
            ),
            (
                "view-mode-icons",
                "▦",
                ViewMode::Icons,
                Box::new(SetViewIcons) as Box<dyn gpui::Action>,
            ),
        ] {
            let active = self.view_mode == mode;
            let boxed = action;
            switcher = switcher.child(
                div()
                    .id(id)
                    .debug_selector(|| id.to_string())
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(22.0))
                    .h(px(20.0))
                    .rounded(px(3.0))
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .when(active, |el| el.bg(theme.accent.opacity(0.30)))
                    .text_color(if active { theme.text } else { theme.muted })
                    .hover(|s| s.bg(theme.accent.opacity(0.15)))
                    .on_click(cx.listener(move |this, _, window: &mut Window, cx| {
                        window.focus(&this.focus_handle, cx);
                        window.dispatch_action(boxed.boxed_clone(), cx);
                    }))
                    .child(SharedString::new_static(label)),
            );
        }
        switcher
    }
}

impl Render for Pane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let body: gpui::AnyElement = if self.path.is_none() {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child("No folder open")
                .into_any_element()
        } else {
            // The details view (header + uniform_list rows).
            self.dir_view.clone().into_any_element()
        };

        div()
            .track_focus(&self.focus_handle)
            .key_context("Pane")
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.go_back(cx)))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.go_forward(cx)))
            .on_action(cx.listener(|this, _: &GoUp, _, cx| this.go_up(cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, action: &SortBy, _, cx| this.sort_by(action.key, cx)))
            // §0 New folder/file (M3): the pane owns the destination
            // directory and the placeholder name; the details view's inline
            // editor names it and submits the op (§4c).
            .on_action(cx.listener(|this, _: &NewFolder, window, cx| this.new_folder(window, cx)))
            .on_action(cx.listener(|this, _: &NewFile, window, cx| this.new_file(window, cx)))
            // §0 View mode switcher (M4): the keymap rows and the toolbar
            // control below dispatch these same boxed actions.
            .on_action(
                cx.listener(|this, _: &SetViewList, _, cx| this.set_view_mode(ViewMode::List, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SetViewIcons, _, cx| {
                    this.set_view_mode(ViewMode::Icons, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &SetViewColumns, _, cx| this.set_view_columns(cx)))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _, _, cx| this.go_back(cx)),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _, _, cx| this.go_forward(cx)),
            )
            .flex()
            .flex_col()
            .flex_1()
            .child(self.render_chrome_row(cx))
            .child(body)
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
                    .child(self.status_text()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use fs_core::{FakeVfs, Spawner};
    use gpui::{TestAppContext, VisualTestContext};
    use serde_json::json;

    fn nav(path: &str, cursor: Option<&str>, scroll_top: f32) -> NavEntry {
        NavEntry {
            path: PathBuf::from(path),
            cursor: cursor.map(|c| EntryId(Arc::from(Path::new(c)))),
            scroll_top,
        }
    }

    #[test]
    fn nav_history_back_forward_round_trip() {
        let mut history = NavHistory::default();
        history.record_navigation(None);
        assert!(!history.can_go_back());

        history.record_navigation(Some(nav("/a", Some("/a/x"), 10.0)));
        history.record_navigation(Some(nav("/b", None, 0.0)));
        assert!(history.can_go_back());
        assert!(!history.can_go_forward());

        let target = history.pop_back(nav("/c", None, 5.0)).unwrap();
        assert_eq!(target.path, PathBuf::from("/b"));
        assert!(history.can_go_forward());

        let target = history.pop_back(nav("/b", None, 0.0)).unwrap();
        assert_eq!(target.path, PathBuf::from("/a"));
        assert_eq!(target.cursor, Some(EntryId(Arc::from(Path::new("/a/x")))));
        assert_eq!(target.scroll_top, 10.0);
        assert!(!history.can_go_back());

        let forward = history.pop_forward(nav("/a", None, 0.0)).unwrap();
        assert_eq!(forward.path, PathBuf::from("/b"));
        assert!(history.can_go_back());
        assert!(history.pop_forward(nav("/b", None, 0.0)).is_some());
        assert!(history.pop_forward(nav("/c", None, 0.0)).is_none());
    }

    #[test]
    fn nav_history_navigation_truncates_forward_stack() {
        let mut history = NavHistory::default();
        history.record_navigation(Some(nav("/a", None, 0.0)));
        history.record_navigation(Some(nav("/b", None, 0.0)));
        history.pop_back(nav("/c", None, 0.0)).unwrap();
        assert!(history.can_go_forward());

        history.record_navigation(Some(nav("/b", None, 0.0)));
        assert!(!history.can_go_forward(), "navigation clears forward");
        assert!(history.can_go_back());
    }

    // The placeholder a `New ▸` editor opens on. It has to be free *before*
    // the editor opens, because the phantom row is keyed on that path and a
    // collision with a real row would swap the editor into the wrong row.
    #[test]
    fn next_available_name_skips_what_is_already_there() {
        let existing: std::collections::BTreeSet<String> = ["New Folder", "New Folder 2"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            next_available_name("New Folder", "", &Default::default()),
            "New Folder"
        );
        assert_eq!(
            next_available_name("New Folder", "", &existing),
            "New Folder 3"
        );
        // The extension rides along, and the counter goes before it.
        let taken: std::collections::BTreeSet<String> = ["New Text File.txt"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            next_available_name("New Text File", ".txt", &taken),
            "New Text File 2.txt"
        );
    }

    #[test]
    fn format_bytes_humanizes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(13_700_000_000), "12.8 GB");
    }

    // ------------------------------------------------------------------
    // gpui tests (§9 pane.rs rows)
    // ------------------------------------------------------------------

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({
                    "sub": { "inner.txt": "abc" },
                    "file2.txt": "..",
                    "file10.txt": "..........",
                    ".secret": "s",
                }),
            );
            vfs.insert_tree("/other", json!({ "b.txt": "b" }));
            // A listing taller than the viewport, so scroll assertions have
            // somewhere to scroll to.
            let mut tall = serde_json::Map::new();
            for i in 0..60 {
                tall.insert(format!("f{i:03}.txt"), json!("x"));
            }
            vfs.insert_tree("/tall", serde_json::Value::Object(tall));
            vfs.set_free_space(2048);
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

    #[gpui::test]
    fn navigate_loads_sorted_listing_and_status_data(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")));
            let snapshot = pane.snapshot().expect("fresh snapshot");
            assert!(!pane.snapshot_is_stale());
            let names: Vec<&str> = snapshot.entries.iter().map(|e| &*e.name).collect();
            // Folders first, natural order, hidden filtered.
            assert_eq!(names, ["sub", "file2.txt", "file10.txt"]);
            assert_eq!(pane.item_count(), 3);
            assert_eq!(pane.free_space(), Some(2048));
            assert_eq!(pane.status_text(), "3 items · 2.0 KB free");
        });
    }

    #[gpui::test]
    fn go_up_navigates_to_parent_and_is_history_recorded(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root/sub"), cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| pane.go_up(cx));
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")));
            assert!(pane.can_go_back());
            assert!(!pane.can_go_forward());
        });

        pane.update(cx, |pane, cx| pane.go_back(cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root/sub")));
            assert!(pane.can_go_forward());
        });
    }

    #[gpui::test]
    fn back_restores_cursor_and_scroll_then_forward_returns(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            pane.set_cursor(Some(entry_id("/root/file2.txt")), cx);
            pane.set_scroll_top(42.5);
        });

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, cx| {
            assert_eq!(pane.cursor(cx), None, "cursor does not leak across dirs");
            assert_eq!(pane.scroll_top(), 0.0);
        });

        pane.update(cx, |pane, cx| pane.go_back(cx));
        // /root is cache-warm: the stale snapshot paints immediately, with
        // cursor and scroll already restored (§4a cache-hit branch).
        pane.read_with(cx, |pane, cx| {
            assert_eq!(pane.path(), Some(Path::new("/root")));
            assert!(pane.snapshot().is_some(), "cached snapshot painted");
            assert!(pane.snapshot_is_stale());
            assert_eq!(pane.cursor(cx), Some(entry_id("/root/file2.txt")));
            assert_eq!(pane.scroll_top(), 42.5);
        });

        cx.run_until_parked();
        pane.read_with(cx, |pane, cx| {
            assert!(!pane.snapshot_is_stale(), "fresh load replaced the paint");
            assert_eq!(pane.cursor(cx), Some(entry_id("/root/file2.txt")));
            assert_eq!(pane.scroll_top(), 42.5);
        });

        pane.update(cx, |pane, cx| pane.go_forward(cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/other")));
        });
    }

    #[gpui::test]
    fn restored_cursor_is_dropped_when_its_path_vanished(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            pane.set_cursor(Some(entry_id("/root/file2.txt")), cx);
            pane.set_scroll_top(7.0);
        });
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();

        vfs.remove_path("/root/file2.txt");
        pane.update(cx, |pane, cx| pane.go_back(cx));
        cx.run_until_parked();

        pane.read_with(cx, |pane, cx| {
            assert_eq!(pane.path(), Some(Path::new("/root")));
            assert_eq!(
                pane.cursor(cx),
                None,
                "cursor whose entry vanished must not survive the fresh load"
            );
            assert_eq!(pane.scroll_top(), 7.0, "scroll restore is independent");
            assert_eq!(pane.item_count(), 2);
        });
    }

    #[gpui::test]
    fn rapid_navigation_keeps_only_the_newest_generation(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        // Two navigations before any load completes: the /root load is stale
        // by the time it lands and must never win over /other.
        pane.update(cx, |pane, cx| {
            pane.navigate_to(Path::new("/root"), cx);
            pane.navigate_to(Path::new("/other"), cx);
        });
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/other")));
            let snapshot = pane.snapshot().expect("snapshot");
            assert_eq!(&*snapshot.dir, Path::new("/other"));
            assert!(!pane.snapshot_is_stale());
            assert_eq!(pane.item_count(), 1);
        });
    }

    #[gpui::test]
    fn refresh_reloads_preserving_cursor(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| {
            pane.set_cursor(Some(entry_id("/root/file10.txt")), cx);
        });

        vfs.insert_file("/root/zzz.txt", 3);
        pane.update(cx, |pane, cx| pane.refresh(cx));
        cx.run_until_parked();

        pane.read_with(cx, |pane, cx| {
            assert_eq!(pane.item_count(), 4, "refresh picked up the new file");
            assert_eq!(pane.cursor(cx), Some(entry_id("/root/file10.txt")));
        });
    }

    #[gpui::test]
    fn sort_by_same_key_flips_direction_and_reloads(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        pane.update(cx, |pane, cx| pane.sort_by(SortKey::Name, cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.sort().direction, SortDirection::Descending);
            let names: Vec<&str> = pane
                .snapshot()
                .unwrap()
                .entries
                .iter()
                .map(|e| &*e.name)
                .collect();
            assert_eq!(names, ["sub", "file10.txt", "file2.txt"]);
        });

        pane.update(cx, |pane, cx| pane.sort_by(SortKey::Size, cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.sort().key, SortKey::Size);
            assert_eq!(pane.sort().direction, SortDirection::Ascending);
        });
    }

    #[gpui::test]
    fn show_hidden_toggle_reloads_with_dotfiles(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| assert_eq!(pane.item_count(), 3));

        pane.update(cx, |pane, cx| pane.set_show_hidden(true, cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.item_count(), 4, "dotfile now listed");
        });
    }

    #[gpui::test]
    fn load_errors_are_surfaced_not_fatal(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        vfs.set_error("/root", "disk on fire");
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            let error = pane.load_error().expect("error recorded");
            assert!(error.contains("disk on fire"));
            assert!(pane.snapshot().is_none());
        });
    }

    // ------------------------------------------------------------------
    // The watcher keeps the open directory live (§4a): external changes
    // appear with no explicit Refresh, and a batch for a directory we have
    // left can never apply.
    // ------------------------------------------------------------------

    fn names(pane: &Pane) -> Vec<String> {
        pane.snapshot()
            .map(|snap| snap.entries.iter().map(|e| e.name.to_string()).collect())
            .unwrap_or_default()
    }

    /// Let the debounce window elapse and the pump run: the fake clock is the
    /// only thing standing between an injected event and its batch.
    fn settle_watch(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(WATCH_LATENCY);
        cx.run_until_parked();
    }

    #[gpui::test]
    fn external_changes_patch_the_listing_without_a_refresh(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(names(pane), ["sub", "file2.txt", "file10.txt"]);
        });

        vfs.insert_file("/root/added.txt", 4);
        vfs.remove_path("/root/file2.txt");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                names(pane),
                ["sub", "file2.txt", "file10.txt"],
                "events are debounced — nothing applies before the window elapses"
            );
        });

        settle_watch(cx);
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                names(pane),
                ["sub", "added.txt", "file10.txt"],
                "one batch inserted the new file in sort position and dropped the removed one"
            );
            assert!(!pane.snapshot_is_stale());
            assert_eq!(pane.item_count(), 3);
        });

        // An external rename is a Removed + Created pair in the same batch.
        vfs.remove_path("/root/added.txt");
        vfs.insert_file("/root/renamed.txt", 4);
        settle_watch(cx);
        pane.read_with(cx, |pane, _| {
            assert_eq!(names(pane), ["sub", "file10.txt", "renamed.txt"]);
        });
    }

    #[gpui::test]
    fn watcher_patch_keeps_selection_and_cursor(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(
                &[Path::new("/root/file2.txt"), Path::new("/root/file10.txt")],
                cx,
            );
        });
        pane.update(cx, |pane, _| pane.set_scroll_top(12.0));

        // A patch that touches neither selected row leaves the selection and
        // the cursor exactly as they were.
        vfs.insert_file("/root/added.txt", 1);
        settle_watch(cx);
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(
                dir_view.selection().selected_paths(),
                vec![
                    PathBuf::from("/root/file10.txt"),
                    PathBuf::from("/root/file2.txt")
                ]
            );
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/file10.txt")));
        });
        pane.read_with(cx, |pane, _| assert_eq!(pane.scroll_top(), 12.0));

        // A patch that removes the cursor's row prunes it (path-keyed
        // survival) without disturbing the rest of the selection.
        vfs.remove_path("/root/file10.txt");
        settle_watch(cx);
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(
                dir_view.selection().selected_paths(),
                vec![PathBuf::from("/root/file2.txt")],
                "the vanished row left the selection"
            );
            assert_eq!(dir_view.cursor(), None, "dangling cursor cleared");
        });
    }

    // Finding: `Vfs::watch` is a blocking, disk-touching call (on macOS it
    // stats and canonicalizes the path and stops/starts an FSEvents run-loop
    // thread), so it may not run on the UI thread. The observable form of
    // "off the UI thread" is that no registration exists until the executor
    // has been allowed to run.
    #[gpui::test]
    fn registering_the_watch_never_happens_on_the_ui_thread(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        assert_eq!(
            vfs.watcher_count(),
            0,
            "navigate_to must not register the watch inline — that is disk I/O \
             on the render thread"
        );
        cx.run_until_parked();
        assert_eq!(vfs.watcher_count(), 1, "the background task registered it");

        // And it really is live.
        vfs.insert_file("/root/added.txt", 1);
        settle_watch(cx);
        pane.read_with(cx, |pane, _| {
            assert!(names(pane).iter().any(|name| name == "added.txt"));
        });
    }

    // An in-place reload (sort flip, hidden toggle, refresh) watches the same
    // directory, so it must reuse the live registration: each stop/restart
    // cycle is expensive *and* loses every change that happens in the gap.
    #[gpui::test]
    fn an_in_place_reload_reuses_the_live_watch(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        assert_eq!(vfs.watch_registrations(), 1);

        pane.update(cx, |pane, cx| pane.sort_by(SortKey::Name, cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| pane.sort_by(SortKey::Size, cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| pane.set_show_hidden(true, cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| pane.refresh(cx));
        cx.run_until_parked();
        assert_eq!(
            vfs.watch_registrations(),
            1,
            "four in-place reloads must not re-register the watch"
        );
        assert_eq!(vfs.watcher_count(), 1);

        // The one live watch still feeds the pane after all of that.
        vfs.insert_file("/root/late.txt", 1);
        settle_watch(cx);
        pane.read_with(cx, |pane, _| {
            assert!(names(pane).iter().any(|name| name == "late.txt"));
        });

        // Actually leaving the directory *does* re-register (once).
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();
        assert_eq!(vfs.watch_registrations(), 2);
        assert_eq!(
            vfs.watcher_count(),
            1,
            "and the directory we left is unwatched again"
        );
    }

    // A patch is not a navigation: `NavEntry.scroll_top` is pane bookkeeping
    // that nothing updates while the user scrolls, so re-applying it on every
    // external change would snap the list back to the top.
    #[gpui::test]
    fn a_watcher_patch_leaves_the_scroll_alone(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/tall"), cx));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, _| dir_view.apply_scroll_top(240.0));
        cx.run_until_parked();
        let scrolled = dir_view.read_with(cx, |view, _| crate::marquee::scroll_y(view));
        assert!(
            scrolled < 0.0,
            "the list really is scrolled down (offset {scrolled})"
        );

        vfs.insert_file("/tall/zzz.txt", 1);
        settle_watch(cx);
        pane.read_with(cx, |pane, _| assert_eq!(pane.item_count(), 61));
        assert_eq!(
            dir_view.read_with(cx, |view, _| crate::marquee::scroll_y(view)),
            scrolled,
            "an external change must not yank the list back to the top"
        );
    }

    #[gpui::test]
    fn watch_batch_for_a_left_directory_is_ignored(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        // The change happens while /root is still open, but the pane navigates
        // away before the debounce window closes: the batch belongs to a
        // directory (and generation) we have left.
        vfs.insert_file("/root/added.txt", 1);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();
        settle_watch(cx);

        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/other")));
            assert_eq!(
                names(pane),
                ["b.txt"],
                "a stale batch must never patch the new directory's listing"
            );
        });
        assert_eq!(
            vfs.watcher_count(),
            1,
            "navigating away unregistered the old watch"
        );

        // The new directory is watched instead.
        vfs.insert_file("/other/fresh.txt", 1);
        settle_watch(cx);
        pane.read_with(cx, |pane, _| {
            assert_eq!(names(pane), ["b.txt", "fresh.txt"]);
        });
    }

    #[gpui::test]
    fn rescan_event_reloads_the_directory_in_full(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        // Events were dropped: the tree changed with no usable event trail.
        vfs.insert_tree("/root/late", json!({}));
        vfs.emit_event(fs_core::PathEvent {
            path: Arc::from(Path::new("/root")),
            kind: fs_core::PathEventKind::Rescan,
        });
        settle_watch(cx);
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            assert_eq!(
                names(pane),
                ["late", "sub", "file2.txt", "file10.txt"],
                "Rescan falls back to a full reload"
            );
        });
    }

    // Keymap dispatch guard for the `Pane` context (§9): focus the pane,
    // send the bound keystrokes, assert the handlers actually fired.
    #[gpui::test]
    fn pane_key_context_dispatches_history_and_refresh(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();

        cx.update(|window, cx| {
            let handle = pane.focus_handle(cx);
            window.focus(&handle, cx);
        });

        cx.simulate_keystrokes("cmd-[");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")), "cmd-[ went back");
        });

        cx.simulate_keystrokes("cmd-]");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/other")), "cmd-] went forward");
        });

        vfs.insert_file("/other/new.txt", 1);
        cx.simulate_keystrokes("cmd-r");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/other")));
            assert_eq!(pane.item_count(), 2, "cmd-r reloaded the listing");
        });
    }

    // A fresh split pane opens in the *other* mode (plan §2's list-beside-grid
    // blueprint), so the complement must be an involution — flipping twice is
    // where you started, and neither mode maps to itself.
    #[test]
    fn view_mode_complement_is_the_other_shipped_mode() {
        assert_eq!(ViewMode::List.complement(), ViewMode::Icons);
        assert_eq!(ViewMode::Icons.complement(), ViewMode::List);
        for mode in [ViewMode::List, ViewMode::Icons] {
            assert_ne!(mode.complement(), mode);
            assert_eq!(mode.complement().complement(), mode);
        }
    }

    // §0 "View mode switcher": the keystrokes and the toolbar buttons are two
    // triggers for **one** handler, so this asserts both routes land on the
    // same pane state — a button wired to a method instead of the boxed
    // action would pass a state test and still break the menu bar at M8.
    #[gpui::test]
    fn view_mode_switches_from_both_the_keymap_and_the_toolbar(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.view_mode(), ViewMode::List, "list is the default");
        });

        cx.update(|window, cx| {
            let handle = pane.focus_handle(cx);
            window.focus(&handle, cx);
        });

        cx.simulate_keystrokes("cmd-2");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.view_mode(), ViewMode::Icons, "cmd-2 = icon grid");
        });

        cx.simulate_keystrokes("cmd-1");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.view_mode(), ViewMode::List, "cmd-1 = details list");
        });

        // The toolbar control, clicked at the pixels it actually painted.
        let icons = cx
            .debug_bounds("view-mode-icons")
            .expect("the toolbar switcher paints an icon-grid button");
        cx.simulate_click(icons.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.view_mode(), ViewMode::Icons, "toolbar button switched");
        });

        let list = cx
            .debug_bounds("view-mode-list")
            .expect("the toolbar switcher paints a details-list button");
        cx.simulate_click(list.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.view_mode(), ViewMode::List, "and switched back");
        });
    }

    // §8 keeps Miller columns a post-v1 stretch. The action exists (the §0
    // table and, at M8, the menu bar need it) but must not pretend: it tells
    // the user, and it leaves the current view alone.
    #[gpui::test]
    fn set_view_columns_announces_the_unimplemented_view(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (pane, cx) = build_pane(cx);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        pane.update(cx, |pane, cx| pane.set_view_mode(ViewMode::Icons, cx));

        cx.update(|window, cx| {
            let handle = pane.focus_handle(cx);
            window.focus(&handle, cx);
            window.dispatch_action(Box::new(SetViewColumns), cx);
        });
        cx.run_until_parked();

        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.view_mode(),
                ViewMode::Icons,
                "an unimplemented mode must not disturb the current one"
            );
        });
        cx.update(|_, cx| {
            let jobs = FsContext::global(cx).jobs.clone();
            let messages: Vec<String> = jobs
                .read(cx)
                .toasts()
                .iter()
                .map(|toast| toast.message.to_string())
                .collect();
            assert_eq!(
                messages,
                vec![COLUMNS_UNAVAILABLE_NOTICE.to_string()],
                "the user is told, rather than the command silently doing nothing"
            );
        });
    }
}
