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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::{
    EntryId, ListingCache, ListingSnapshot, SortDirection, SortKey, SortSpec, Vfs, list_dir,
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, MouseButton, NavigationDirection,
    Render, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::actions::{GoBack, GoForward, GoUp, Refresh, SortBy};
use crate::address_bar::{AddressBar, AddressBarEvent};
use crate::app_state::FsContext;
use crate::dir_view::{DirView, DirViewEvent};
use crate::theme::Theme;

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

/// Whether the address bar renders as breadcrumb segments or as the editable
/// path input (ARCHITECTURE.md §2). The input itself is `address_bar.rs` (M1,
/// separate build step); the mode lives here because the pane owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressBarMode {
    Breadcrumb,
    Editing,
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
    _subscriptions: Vec<Subscription>,
}

impl Pane {
    pub fn new(theme: Theme, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let vfs = FsContext::global(cx).vfs.clone();
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
            focus_handle: cx.focus_handle(),
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
            sort: SortSpec::default(),
            show_hidden: false,
            cache: ListingCache::default(),
            generation: 0,
            free_space: None,
            load_error: None,
            _load_task: None,
            _free_space_task: None,
            _subscriptions: vec![subscription, bar_subscription],
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
            // Old rows stay visible while loading (§4a); the cursor does not
            // carry across directories.
            self.snapshot_is_stale = self.snapshot.is_some();
            self.scroll_top = 0.0;
            self.dir_view.update(cx, |dir_view, cx| {
                dir_view.set_cursor(None, cx);
                dir_view.apply_scroll_top(0.0);
            });
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

    /// Apply a [`NavEntry`]'s cursor + scroll against the current snapshot
    /// (restore semantics), or — with no restore — drop a cursor whose path
    /// vanished from the fresh listing. The cursor lives in the [`DirView`].
    fn apply_restore(&mut self, restore: Option<NavEntry>, cx: &mut Context<Self>) {
        match restore {
            Some(entry) => {
                let cursor = entry.cursor.filter(|id| self.snapshot_contains(id));
                self.scroll_top = entry.scroll_top;
                let scroll_top = entry.scroll_top;
                self.dir_view.update(cx, |dir_view, cx| {
                    dir_view.set_cursor(cursor, cx);
                    dir_view.apply_scroll_top(scroll_top);
                });
            }
            None => {
                if let Some(cursor) = self.cursor(cx)
                    && !self.snapshot_contains(&cursor)
                {
                    self.dir_view
                        .update(cx, |dir_view, cx| dir_view.set_cursor(None, cx));
                }
            }
        }
    }

    fn snapshot_contains(&self, id: &EntryId) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snap| snap.entries.iter().any(|entry| entry.id() == *id))
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
            return row.child(self.address_bar_view.clone());
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
        row.child(segments).child(
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
    use crate::app_state::{FsContext, GpuiSpawner, LoggingOpener};
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
            vfs.set_free_space(2048);
            crate::keymap::init(cx);
            cx.set_global(FsContext {
                vfs: vfs.clone(),
                spawner,
                opener: Arc::new(LoggingOpener),
            });
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
}
