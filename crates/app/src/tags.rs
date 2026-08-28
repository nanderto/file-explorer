//! Finder tags in the UI (ARCHITECTURE.md §6 `tags.rs` / `Platform::read_tags`,
//! plan §7 M6b): the dots painted after a name, the sidebar's **Tags** filter,
//! and the `Tags ▸` submenu that writes them.
//!
//! Three machines live here, and none of them is new in shape:
//!
//! * [`TagState`] — a **field of [`DirView`]** (the seventh, after rename,
//!   marquee, drop, menu, thumbnails and the scrollbar), holding the
//!   window-shaped tag cache and the single cancellable `Task` slot that fills
//!   it. Reading a tag set is one `getxattr` **per path**, so it is loaded
//!   exactly the way [`crate::thumbnails`] loads previews: lazily, only for
//!   the visible band plus a margin, cancelled when that band moves, and
//!   always on the background executor. The request window comes from the
//!   scroll offset and the viewport — deliberately **not** from the row range
//!   `uniform_list` hands its processor, which gpui calls with `0..1` twice a
//!   frame just to measure an item (the M4 bug that stopped every thumbnail
//!   from ever loading).
//! * [`TagFilter`] — a **field of [`Pane`]**, the sidebar's "show me what is
//!   tagged Red". It does not invent a second filtered projection: its rows
//!   are served through [`Pane::filtered_rows`], the same accessor M6a's
//!   search results go through, so the marquee, drag & drop, the context menu,
//!   the icon grid, thumbnails, the scrollbar and the selection pruning all
//!   keep working with no knowledge that a filter is on. The scan itself is
//!   M6a's pump shape too: a background walk, a channel, and batches folded in
//!   on [`crate::search::SEARCH_THROTTLE`].
//! * The `Tags ▸` submenu's write path ([`DirView::toggle_tag_on_selection`]),
//!   which submits [`fs_core::FileOp::SetTags`] through the job spine — so a
//!   tagging is undoable with `cmd-z` like every other file operation.
//!
//! **Tag colours are the one exception to "no hard-coded colors in
//! `crates/app`"** — and they are not even hard-coded *here*: they come from
//! [`fs_core::TagColor::rgba`], macOS's own fixed palette (plan §6 says so
//! explicitly). A tag dot that is not the colour Finder paints is the wrong
//! dot, so it cannot be a [`Theme`](crate::theme::Theme) value. Everything
//! else in this file — text, hover, the active row's tint — comes from the
//! theme as usual.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::{FileEntry, FileOp, ListingSnapshot, SortSpec, Tag, TagColor};
use futures::StreamExt as _;
use gpui::{
    AnyElement, Context, IntoElement, SharedString, Styled, Task, div, prelude::*, px, rgba,
};

use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::pane::Pane;
use crate::thumbnails::{MARGIN_ROWS, request_window, visible_rows};

/// Diameter of one tag dot. Small enough to sit inside a 24 px details row and
/// a 88 px tile's label line without changing either's geometry.
pub(crate) const TAG_DOT_PX: f32 = 7.0;

/// Gap between dots when an item carries several.
const TAG_DOT_GAP: f32 = 2.0;

/// How many dots a row paints before it stops.
///
/// Finder shows at most a few and then a `+`; a row that painted eight dots
/// would eat the name it belongs to. The cap is on *coloured* tags: an
/// uncoloured tag has no dot at all (see [`fs_core::TagColor::rgba`]), so it
/// is invisible in a row either way and only shows in the info panel.
const MAX_ROW_DOTS: usize = 4;

/// The dots for one item's tag set, or `None` when it has no coloured tag.
///
/// Fixed size and `flex_none`: an arriving tag set must not reflow the name
/// beside it, for exactly the reason an arriving thumbnail must not reflow the
/// tile lattice — every hit test in the pane is arithmetic against that
/// lattice.
pub(crate) fn tag_dots(tags: &[Tag]) -> Option<AnyElement> {
    let colors: Vec<TagColor> = tags
        .iter()
        .map(|tag| tag.color)
        .filter(|color| *color != TagColor::None)
        .take(MAX_ROW_DOTS)
        .collect();
    if colors.is_empty() {
        return None;
    }
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(TAG_DOT_GAP))
            .children(colors.into_iter().filter_map(tag_dot))
            .into_any_element(),
    )
}

/// One dot in `color`, or `None` for [`TagColor::None`] — a tag with no colour
/// draws no dot at all (painting transparent pixels would leave a gap where a
/// dot is not).
pub(crate) fn tag_dot(color: TagColor) -> Option<AnyElement> {
    if color == TagColor::None {
        return None;
    }
    Some(
        div()
            .flex_none()
            .w(px(TAG_DOT_PX))
            .h(px(TAG_DOT_PX))
            .rounded(px(TAG_DOT_PX / 2.0))
            // The one non-theme colour in the app crate: macOS's own palette
            // (see the module docs).
            .bg(rgba(color.rgba()))
            .into_any_element(),
    )
}

/// `"Red, Work"` — the info panel's textual tag list.
pub(crate) fn tag_names(tags: &[Tag]) -> Option<SharedString> {
    if tags.is_empty() {
        return None;
    }
    Some(SharedString::new(
        tags.iter()
            .map(|tag| tag.name.as_ref())
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

// ----------------------------------------------------------------------
// The view's lazy tag cache
// ----------------------------------------------------------------------

/// Per-view tag state (see the module docs).
pub(crate) struct TagState {
    /// Tags per path, as last read. An **empty** slice is a real value ("this
    /// item has no tags") and is cached like any other, so an untagged folder
    /// costs one `getxattr` per item per visit rather than one per frame. A
    /// read that *failed* (the path vanished, the volume refused) caches empty
    /// too: the trait's "must not retry in a loop", and the row simply paints
    /// no dot.
    tags: HashMap<Arc<Path>, Arc<[Tag]>>,
    /// The tags offered by the `Tags ▸` submenu: the standard palette until
    /// [`fs_core::Platform::known_tags`] answers, then whatever it reports.
    /// Seeded synchronously with [`fs_core::standard_tags`] so the submenu is
    /// never empty and the first paint does no I/O.
    known: Vec<Tag>,
    /// The window the current read task was spawned for.
    window: Option<Range<usize>>,
    /// Whether that task is still working through it.
    fetching: bool,
    /// The single read slot. Replacing it cancels the reads in flight.
    _fetch: Option<Task<()>>,
    /// The single slot for a `Tags ▸` write: read the current sets, then
    /// submit. Replacing it drops a submission the user superseded.
    _write: Option<Task<()>>,
    /// The one-shot `known_tags` load.
    _known_load: Option<Task<()>>,
}

impl Default for TagState {
    fn default() -> Self {
        Self {
            tags: HashMap::new(),
            known: fs_core::standard_tags(),
            window: None,
            fetching: false,
            _fetch: None,
            _write: None,
            _known_load: None,
        }
    }
}

impl DirView {
    /// Keep the tags for the rows on screen (and [`MARGIN_ROWS`] either side)
    /// coming. Called once per frame from [`DirView::render`] for **both** view
    /// modes — `cols` is 1 in the details list, the painted column count in the
    /// grid — with `row_height` the item height that mode lays out.
    ///
    /// Idempotent per window, for the same reason
    /// [`DirView::request_thumbnails`] is: every arriving tag set, every
    /// scrollbar fade and every watcher patch re-renders, and a window derived
    /// from anything less stable than the scroll offset would cancel its own
    /// reads on every repaint.
    pub(crate) fn request_tags(&mut self, cols: usize, row_height: f32, cx: &mut Context<Self>) {
        self.load_known_tags(cx);
        let len = self.flat_rows().len();
        let viewport = crate::marquee::list_viewport(self);
        let rows = visible_rows(
            -crate::marquee::scroll_y(self),
            f32::from(viewport.size.height),
            row_height,
            crate::views::icon_grid::grid_row_count(len, cols),
        );
        let requested = request_window(rows, cols, len, MARGIN_ROWS);
        let moved = self.tags.window.as_ref() != Some(&requested);
        if moved {
            self.tags.window = Some(requested.clone());
            // Scroll-away: drop the task, abandoning the reads queued for rows
            // nobody is looking at any more.
            self.tags._fetch = None;
            self.tags.fetching = false;
            // Bounded by the viewport, not by history: one entry per visible
            // row plus the margin. Re-reading on the way back is one
            // `getxattr`, which is cheaper than an unbounded map for the life
            // of the pane.
            self.prune_tag_cache(&requested);
        }
        if !moved && self.tags.fetching {
            return;
        }
        let pending = self.pending_tag_reads(&requested);
        if pending.is_empty() {
            return;
        }
        self.spawn_tag_reads(pending, cx);
    }

    /// The one-shot `known_tags` load: the sidebar has its own (it is a
    /// different entity), and this one feeds the `Tags ▸` submenu, which is
    /// built synchronously from [`crate::context_menu::MenuFacts`] and so
    /// cannot await anything.
    fn load_known_tags(&mut self, cx: &mut Context<Self>) {
        if self.tags._known_load.is_some() {
            return;
        }
        let platform = FsContext::global(cx).platform.clone();
        self.tags._known_load = Some(cx.spawn(async move |this, cx| {
            let known = cx
                .background_executor()
                .spawn(async move { platform.known_tags().await })
                .await;
            if let Ok(known) = known
                && !known.is_empty()
            {
                this.update(cx, |this, cx| {
                    this.tags.known = known;
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Paths inside `window` whose tags are not cached yet, in paint order.
    fn pending_tag_reads(&self, window: &Range<usize>) -> Vec<Arc<Path>> {
        self.flat_rows()
            .get(window.clone())
            .unwrap_or_default()
            .iter()
            .map(|row| row.entry.path.clone())
            .filter(|path| !self.tags.tags.contains_key(path))
            .collect()
    }

    /// The single-slot read task: one [`fs_core::Platform::read_tags`] at a
    /// time, each awaited on the **background** executor, each folded into the
    /// cache on the UI thread and painted by the `notify` that follows.
    fn spawn_tag_reads(&mut self, paths: Vec<Arc<Path>>, cx: &mut Context<Self>) {
        let platform = FsContext::global(cx).platform.clone();
        self.tags.fetching = true;
        self.tags._fetch = Some(cx.spawn(async move |this, cx| {
            for path in paths {
                let read_path = path.clone();
                let platform = platform.clone();
                // The UI thread only ever awaits this handle; the xattr read
                // itself happens on the pool.
                let tags = cx
                    .background_executor()
                    .spawn(async move { platform.read_tags(&read_path).await })
                    .await;
                let alive = this.update(cx, |this, cx| {
                    // A failed read caches as "no tags" — see `TagState::tags`.
                    this.tags
                        .tags
                        .insert(path, Arc::from(tags.unwrap_or_default()));
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, _| this.tags.fetching = false).ok();
        }));
    }

    fn prune_tag_cache(&mut self, window: &Range<usize>) {
        if self.tags.tags.is_empty() {
            return;
        }
        let keep: HashSet<Arc<Path>> = self
            .flat_rows()
            .get(window.clone())
            .unwrap_or_default()
            .iter()
            .map(|row| row.entry.path.clone())
            .collect();
        self.tags.tags.retain(|path, _| keep.contains(path));
    }

    /// The tags to paint for `path`: what the lazy read found, or nothing at
    /// all while it is still in flight. Never reads the disk — `render` must
    /// not (§5).
    pub(crate) fn entry_tags(&self, path: &Path) -> &[Tag] {
        self.tags
            .tags
            .get(path)
            .map(|tags| tags.as_ref())
            .unwrap_or_default()
    }

    /// The tags the `Tags ▸` submenu offers.
    pub(crate) fn known_tags(&self) -> &[Tag] {
        &self.tags.known
    }

    /// The tags **every** selected row is known to carry — the submenu's ✓.
    ///
    /// Derived from the painted cache, which is the honest answer: it is what
    /// the user can see. A selected row scrolled far out of the request window
    /// has no cached tags and therefore contributes none, so the ✓ can be
    /// missing for a tag that is in fact set. It cannot be *wrong* in the other
    /// direction, and the write path below never trusts this cache anyway — it
    /// re-reads every selected path before it computes what to store.
    pub(crate) fn tags_on_whole_selection(&self) -> Vec<Tag> {
        let selected: Vec<&Arc<Path>> = self
            .flat_rows()
            .iter()
            .filter(|row| self.selection().is_selected(&row.entry.id()))
            .map(|row| &row.entry.path)
            .collect();
        let Some((first, rest)) = selected.split_first() else {
            return Vec::new();
        };
        self.entry_tags(first)
            .iter()
            .filter(|tag| {
                rest.iter().all(|path| {
                    self.entry_tags(path)
                        .iter()
                        .any(|other| other.name == tag.name)
                })
            })
            .cloned()
            .collect()
    }

    /// `Tags ▸ <name>`: add `tag` to every selected item, or — when all of
    /// them already have it — remove it from all of them (Finder's toggle).
    ///
    /// [`fs_core::FileOp::SetTags`] replaces the **whole** set on each path, so
    /// the previous sets have to be known before anything can be written; they
    /// are re-read here rather than taken from the paint cache, because a cache
    /// entry is a frame old and writing a stale set would silently drop a tag
    /// somebody else added. Paths whose resulting set is identical are grouped
    /// into one job, so the common cases (one row; several rows with the same
    /// tags) are one job and therefore one `cmd-z`. A selection with genuinely
    /// different tag sets needs one job per distinct result — recorded as a
    /// Known gap rather than pretended away.
    pub(crate) fn toggle_tag_on_selection(&mut self, tag: Tag, cx: &mut Context<Self>) {
        let paths = self.selection().selected_paths();
        if paths.is_empty() {
            return;
        }
        let platform = FsContext::global(cx).platform.clone();
        self.tags._write = Some(cx.spawn(async move |this, cx| {
            let read_paths = paths.clone();
            let platform_read = platform.clone();
            // Every read off the UI thread, in one hop.
            let current: Vec<(PathBuf, Vec<Tag>)> = cx
                .background_executor()
                .spawn(async move {
                    let mut out = Vec::with_capacity(read_paths.len());
                    for path in read_paths {
                        let tags = platform_read.read_tags(&path).await.unwrap_or_default();
                        out.push((path, tags));
                    }
                    out
                })
                .await;
            let plan = plan_tag_toggle(&current, &tag);
            this.update(cx, |_, cx| {
                let queue = FsContext::global(cx).queue.clone();
                for (tags, paths) in plan {
                    queue.submit(FileOp::SetTags { paths, tags });
                }
            })
            .ok();
        }));
    }

    /// Forget the cached tags for `paths` (a `SetTags` job just changed them).
    ///
    /// An xattr write changes no directory entry and no mtime, so the pane's
    /// watcher cannot see it: without this, dots would keep showing the tags
    /// the file had before the user set them. Dropping the entries is enough —
    /// the next frame's [`Self::request_tags`] finds them missing from the
    /// cache and re-reads exactly the ones on screen.
    pub(crate) fn invalidate_tags(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let before = self.tags.tags.len();
        self.tags
            .tags
            .retain(|path, _| !paths.iter().any(|changed| changed == path.as_ref()));
        if self.tags.tags.len() != before {
            // The window has not moved, so `request_tags` would take the
            // "nothing pending" path unless the read slot is freed too.
            self.tags._fetch = None;
            self.tags.fetching = false;
            cx.notify();
        }
    }

    /// Test window into the machine: the request window, how many paths are
    /// cached, and whether a read is still running.
    #[cfg(test)]
    pub(crate) fn tag_debug(&self) -> (Option<Range<usize>>, usize, bool) {
        (
            self.tags.window.clone(),
            self.tags.tags.len(),
            self.tags.fetching,
        )
    }
}

/// Group `(path, current tags)` pairs into the [`FileOp::SetTags`] jobs a
/// toggle of `tag` needs: one job per distinct resulting tag set.
///
/// Pure, and where the toggle's actual *rule* lives: if every path already
/// carries `tag` (by **name** — a tag's name is its identity, as in Finder and
/// in [`fs_core::encode_tag_strings`]), the toggle removes it; otherwise it is
/// added to the ones that lack it, keeping their other tags and their order,
/// and appended last so an existing set is not reordered. Paths that need no
/// change at all are dropped, so "add Red to a mixed selection" writes only the
/// items that were missing it.
fn plan_tag_toggle(current: &[(PathBuf, Vec<Tag>)], tag: &Tag) -> Vec<(Vec<Tag>, Vec<PathBuf>)> {
    let has = |tags: &[Tag]| tags.iter().any(|t| t.name == tag.name);
    let removing = !current.is_empty() && current.iter().all(|(_, tags)| has(tags));
    let mut groups: Vec<(Vec<Tag>, Vec<PathBuf>)> = Vec::new();
    for (path, tags) in current {
        let mut next: Vec<Tag> = tags.clone();
        if removing {
            next.retain(|t| t.name != tag.name);
        } else if has(tags) {
            continue; // already tagged, and we are adding
        } else {
            next.push(tag.clone());
        }
        match groups.iter_mut().find(|(set, _)| *set == next) {
            Some((_, paths)) => paths.push(path.clone()),
            None => groups.push((next, vec![path.clone()])),
        }
    }
    groups
}

// ----------------------------------------------------------------------
// The pane's tag filter (the sidebar's Tags section)
// ----------------------------------------------------------------------

/// One pane's live tag filter (a field of [`Pane`], `None` when no tag is
/// selected in the sidebar).
///
/// **A deliberate deviation from Finder, recorded rather than hidden:** Finder's
/// sidebar tag click runs a *volume-wide* Spotlight query. This filters the
/// **open folder**, which is Explorer's filter-the-listing model, needs no
/// index, and reuses M6a's projection path exactly. See `docs/AS_BUILT.md`.
pub struct TagFilter {
    tag: Tag,
    /// Paths whose tags have been read, so a refresh knows what is left to
    /// scan without re-reading the folder.
    scanned: HashSet<Arc<Path>>,
    /// Of those, the ones carrying the tag.
    matched: HashSet<Arc<Path>>,
    /// The rows the projection renders: the snapshot's entries that matched, in
    /// the pane's sort order. An `Arc` because the projection, the selection
    /// pruning and the status line all read it per frame.
    rows: Arc<[FileEntry]>,
    /// True while the scan task is working.
    running: bool,
}

impl TagFilter {
    fn new(tag: Tag) -> Self {
        Self {
            tag,
            scanned: HashSet::new(),
            matched: HashSet::new(),
            rows: Arc::from(Vec::new()),
            running: false,
        }
    }

    pub fn tag(&self) -> &Tag {
        &self.tag
    }

    pub fn rows(&self) -> Arc<[FileEntry]> {
        self.rows.clone()
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Rebuild [`Self::rows`] from a snapshot and what the scan has matched so
    /// far. Called on every snapshot swap (fresh load, refresh, sort flip,
    /// hidden toggle, watcher patch), which is what keeps the filtered rows
    /// sorted like the listing they replace and stops a watcher patch from
    /// resurrecting a row the filter excludes.
    fn rebuild_rows(&mut self, snapshot: Option<&ListingSnapshot>, sort: SortSpec) {
        let mut rows: Vec<FileEntry> = snapshot
            .map(|snapshot| {
                snapshot
                    .entries
                    .iter()
                    .filter(|entry| self.matched.contains(&entry.path))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by(|a, b| sort.compare(a, b));
        self.rows = Arc::from(rows);
    }

    /// The status line while a tag filter is on (§3's line, tag flavor).
    fn status_text(&self) -> String {
        let count = self.rows.len();
        format!(
            "{count} item{} tagged \u{201c}{}\u{201d}{}",
            if count == 1 { "" } else { "s" },
            self.tag.name,
            if self.running {
                " \u{b7} scanning\u{2026}"
            } else {
                ""
            }
        )
    }
}

impl Pane {
    /// Show only the items in the open folder carrying `tag` (the sidebar's
    /// Tags section). Replaces a text search, if one is live: two filters over
    /// one projection would need a rule nobody has asked for, and Explorer's
    /// filter box behaves the same way.
    pub fn set_tag_filter(&mut self, tag: Tag, cx: &mut Context<Self>) {
        if self
            .tag_filter
            .as_ref()
            .is_some_and(|filter| filter.tag == tag)
        {
            return;
        }
        self.clear_search(cx);
        self.tag_filter = Some(TagFilter::new(tag));
        self.restart_tag_scan(cx);
    }

    /// Drop the filter and restore the unfiltered listing. Cancels the scan by
    /// dropping its task.
    pub fn clear_tag_filter(&mut self, cx: &mut Context<Self>) {
        if self.tag_filter.take().is_none() {
            return;
        }
        self.tag_filter_generation += 1;
        self._tag_filter_task = None;
        self.prune_view_state(cx);
        cx.notify();
    }

    pub fn tag_filter(&self) -> Option<&TagFilter> {
        self.tag_filter.as_ref()
    }

    /// The rows the [`DirView`]'s projection renders, or `None` when the
    /// listing is shown unfiltered — **the** projection source, shared by
    /// M6a's search results and M6b's tag filter, so neither is a second
    /// filtered-projection mechanism (see the module docs).
    pub(crate) fn filtered_rows(&self) -> Option<Arc<[FileEntry]>> {
        match self.tag_filter.as_ref() {
            Some(filter) => Some(filter.rows()),
            None => self.search_rows(),
        }
    }

    /// The status line's filter half, `None` when nothing is filtered.
    pub(crate) fn filter_status_text(&self) -> Option<String> {
        match self.tag_filter.as_ref() {
            Some(filter) => Some(filter.status_text()),
            None => self.search_status_text(),
        }
    }

    /// Re-derive the filtered rows from the pane's current snapshot, and pick
    /// up any entries the snapshot has that the scan never saw (a file created
    /// while the filter was on). Called from [`Pane::prune_view_state`], i.e.
    /// after every snapshot swap, before anything prunes the selection.
    pub(crate) fn refresh_tag_filter(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.snapshot().cloned();
        let sort = self.sort();
        let Some(filter) = self.tag_filter.as_mut() else {
            return;
        };
        filter.rebuild_rows(snapshot.as_deref(), sort);
        // Only when nothing is in flight: a busy folder swaps its snapshot
        // every `WATCH_LATENCY`, and restarting mid-scan would mean a large
        // folder never finished one.
        if filter.running {
            return;
        }
        let unscanned = snapshot
            .as_deref()
            .map(|snapshot| {
                snapshot
                    .entries
                    .iter()
                    .map(|entry| entry.path.clone())
                    .filter(|path| !filter.scanned.contains(path))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !unscanned.is_empty() {
            self.spawn_tag_scan(unscanned, cx);
        }
    }

    /// Navigation drops the filter, for M6a's reason: the results are *of* a
    /// folder, and the folder is no longer the one being looked at.
    pub(crate) fn cancel_tag_filter_for_navigation(&mut self, cx: &mut Context<Self>) {
        self.clear_tag_filter(cx);
    }

    /// Start (or restart) the scan of the open folder for the current filter.
    fn restart_tag_scan(&mut self, cx: &mut Context<Self>) {
        self.tag_filter_generation += 1;
        self._tag_filter_task = None;
        if let Some(filter) = self.tag_filter.as_mut() {
            filter.scanned.clear();
            filter.matched.clear();
            filter.rows = Arc::from(Vec::new());
            filter.running = false;
        }
        let paths: Vec<Arc<Path>> = self
            .snapshot()
            .map(|snapshot| {
                snapshot
                    .entries
                    .iter()
                    .map(|entry| entry.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        if !paths.is_empty() {
            self.spawn_tag_scan(paths, cx);
        }
        self.prune_view_state(cx);
        cx.notify();
    }

    /// The scan, streamed — M6a's `spawn_recursive_search` shape, with
    /// `Platform::read_tags` where the walk was.
    ///
    /// **Nothing here runs on the UI thread**: every read is awaited inside
    /// `cx.background_spawn`, and the foreground task only parks on a
    /// [`fs_core::Spawner::timer`], drains a channel and folds the batch in —
    /// so a thousand-file folder repaints ten times a second rather than a
    /// thousand times. Both halves live in the one slot (the background task is
    /// held on the foreground task's stack), so dropping the slot stops the
    /// scan.
    fn spawn_tag_scan(&mut self, paths: Vec<Arc<Path>>, cx: &mut Context<Self>) {
        let Some(filter) = self.tag_filter.as_mut() else {
            return;
        };
        let tag = filter.tag.clone();
        filter.running = true;
        let generation = self.tag_filter_generation;
        let fs = FsContext::global(cx);
        let platform = fs.platform.clone();
        let spawner = fs.spawner.clone();
        self._tag_filter_task = Some(cx.spawn(async move |this, cx| {
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            // Held, never detached: dropping this task drops the scan with it.
            let _scan = cx.background_spawn(async move {
                for path in paths {
                    let tags = platform.read_tags(&path).await.unwrap_or_default();
                    let hit = tags.iter().any(|t| t.name == tag.name);
                    if tx.unbounded_send((path, hit)).is_err() {
                        return; // the pane stopped listening
                    }
                }
            });
            // Park until something arrives, let the rest pile up for one
            // throttle window, fold the whole pile in at once.
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                spawner.timer(crate::search::SEARCH_THROTTLE).await;
                // `Err` here means "nothing queued right now" *or* "the scan
                // finished"; the next `rx.next().await` tells them apart.
                while let Ok(item) = rx.try_recv() {
                    batch.push(item);
                }
                if this
                    .update(cx, |this, cx| {
                        this.apply_tag_scan_batch(generation, batch, false, cx)
                    })
                    .is_err()
                {
                    return; // pane dropped
                }
            }
            // The channel closed: the scan is over, whether it delivered
            // anything or not.
            this.update(cx, |this, cx| {
                this.apply_tag_scan_batch(generation, Vec::new(), true, cx)
            })
            .ok();
        }));
    }

    /// Fold one throttled batch of `(path, matches)` results into the filter.
    fn apply_tag_scan_batch(
        &mut self,
        generation: u64,
        batch: Vec<(Arc<Path>, bool)>,
        done: bool,
        cx: &mut Context<Self>,
    ) {
        // Belt and braces beside the task slot: a batch whose filter has been
        // superseded (another tag clicked, cleared, navigated) cannot apply.
        if generation != self.tag_filter_generation {
            return;
        }
        let Some(filter) = self.tag_filter.as_mut() else {
            return;
        };
        for (path, hit) in batch {
            if hit {
                filter.matched.insert(path.clone());
            }
            filter.scanned.insert(path);
        }
        if done {
            filter.running = false;
        }
        // `prune_view_state` re-derives the rows (it has to, before pruning the
        // selection against them), so this does not rebuild them twice.
        self.prune_view_state(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    //! §9's M6b rows. The toggle *rule* first, headlessly — it decides what is
    //! written to disk, and `SetTags` replaces a whole set, so getting it wrong
    //! silently destroys tags. Then the three machines on a real laid-out
    //! window: the windowed read (with a recording, optionally slow `Platform`,
    //! the shape `thumbnails.rs` uses), the sidebar filter driven by clicks on
    //! painted rows, and the `Tags ▸` submenu's effect — a job on the queue and
    //! the xattr it left behind, never merely "a click happened".

    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::workspace::Workspace;
    use fs_core::{FakeVfs, Spawner, StubPlatform, TagColor};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::time::Duration;

    fn tag(name: &str, color: TagColor) -> Tag {
        Tag::new(name, color)
    }

    // ------------------------------------------------------------------
    // The toggle rule (pure)
    // ------------------------------------------------------------------

    fn current(pairs: &[(&str, &[Tag])]) -> Vec<(PathBuf, Vec<Tag>)> {
        pairs
            .iter()
            .map(|(path, tags)| (PathBuf::from(path), tags.to_vec()))
            .collect()
    }

    #[test]
    fn toggling_an_absent_tag_adds_it_keeping_the_other_tags() {
        let red = tag("Red", TagColor::Red);
        let work = tag("Work", TagColor::None);
        let plan = plan_tag_toggle(&current(&[("/a", std::slice::from_ref(&work))]), &red);
        assert_eq!(plan, vec![(vec![work, red], vec![PathBuf::from("/a")])]);
    }

    #[test]
    fn toggling_a_tag_every_item_has_removes_it_from_all_of_them() {
        let red = tag("Red", TagColor::Red);
        let work = tag("Work", TagColor::None);
        let plan = plan_tag_toggle(
            &current(&[
                ("/a", &[red.clone(), work.clone()]),
                ("/b", std::slice::from_ref(&red)),
            ]),
            &red,
        );
        // Two distinct results, so two jobs — and neither loses `Work`.
        assert_eq!(
            plan,
            vec![
                (vec![work], vec![PathBuf::from("/a")]),
                (Vec::new(), vec![PathBuf::from("/b")]),
            ]
        );
    }

    #[test]
    fn a_mixed_selection_is_brought_up_to_the_tag_and_touches_nothing_else() {
        let red = tag("Red", TagColor::Red);
        let plan = plan_tag_toggle(
            &current(&[("/has", std::slice::from_ref(&red)), ("/lacks", &[])]),
            &red,
        );
        assert_eq!(
            plan,
            vec![(vec![red], vec![PathBuf::from("/lacks")])],
            "the already-tagged path is not rewritten at all"
        );
    }

    #[test]
    fn paths_with_the_same_result_share_one_job_and_a_name_is_the_identity() {
        let red = tag("Red", TagColor::Red);
        // Same name, different colour slot: Finder treats these as one tag,
        // and so must the toggle — otherwise "Red" would be added twice.
        let red_ish = tag("Red", TagColor::Orange);
        let plan = plan_tag_toggle(
            &current(&[("/a", &[]), ("/b", &[]), ("/c", &[red_ish])]),
            &red,
        );
        assert_eq!(
            plan,
            vec![(vec![red], vec![PathBuf::from("/a"), PathBuf::from("/b")])],
            "one job for the two that needed it, none for the one that has the name"
        );
        assert!(plan_tag_toggle(&[], &tag("Red", TagColor::Red)).is_empty());
    }

    // ------------------------------------------------------------------
    // Dots (pure)
    // ------------------------------------------------------------------

    #[test]
    fn only_coloured_tags_get_a_dot_and_the_count_is_capped() {
        assert!(tag_dots(&[]).is_none());
        assert!(
            tag_dots(&[tag("Work", TagColor::None)]).is_none(),
            "an uncoloured tag paints no dot at all"
        );
        assert!(tag_dots(&[tag("Red", TagColor::Red)]).is_some());
        assert!(tag_dot(TagColor::None).is_none());
        assert!(tag_dot(TagColor::Blue).is_some());
        // Names are what the info panel shows, including uncoloured ones.
        assert_eq!(
            tag_names(&[tag("Red", TagColor::Red), tag("Work", TagColor::None)])
                .unwrap()
                .to_string(),
            "Red, Work"
        );
        assert!(tag_names(&[]).is_none());
    }

    // ------------------------------------------------------------------
    // The machines, on a real window
    // ------------------------------------------------------------------

    /// Which paths `read_tags` was asked about, and which of those it answered.
    /// Both halves matter: "only the visible band" is a claim about the
    /// requests, and "cancel on scroll-away" is a claim about a request that
    /// was started and never finished.
    #[derive(Default)]
    struct Calls {
        started: std::sync::Mutex<Vec<PathBuf>>,
        finished: std::sync::Mutex<Vec<PathBuf>>,
        writes: std::sync::Mutex<Vec<(PathBuf, Vec<Tag>)>>,
    }

    impl Calls {
        fn started(&self) -> Vec<String> {
            names(&self.started.lock().unwrap())
        }

        /// The distinct paths asked about. A window that *moves* before its
        /// reads land re-asks for the rows still in the new window — the same
        /// bounded re-request `thumbnails.rs` documents ("one orphan per
        /// cancellation"), so the interesting count is the distinct one.
        fn asked_once(&self) -> Vec<String> {
            let mut names = self.started();
            names.sort();
            names.dedup();
            names
        }

        fn finished(&self) -> Vec<String> {
            names(&self.finished.lock().unwrap())
        }
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// A recording [`fs_core::Platform`] over [`StubPlatform`], optionally
    /// *slow*: with a `delay` it parks on a [`Spawner`] timer between being
    /// called and answering, which is what gives a test a read it can catch in
    /// flight.
    struct RecordingPlatform {
        inner: StubPlatform,
        spawner: Arc<dyn Spawner>,
        delay: Option<Duration>,
        calls: Arc<Calls>,
    }

    #[async_trait::async_trait]
    impl fs_core::Platform for RecordingPlatform {
        async fn volumes(&self) -> anyhow::Result<Vec<fs_core::VolumeInfo>> {
            self.inner.volumes().await
        }

        async fn eject(&self, volume_id: &fs_core::VolumeId) -> anyhow::Result<()> {
            self.inner.eject(volume_id).await
        }

        async fn thumbnail(&self, path: &Path, px: u32) -> anyhow::Result<fs_core::Thumbnail> {
            self.inner.thumbnail(path, px).await
        }

        async fn file_attrs(&self, path: &Path) -> anyhow::Result<fs_core::FileAttrs> {
            self.inner.file_attrs(path).await
        }

        async fn read_tags(&self, path: &Path) -> anyhow::Result<Vec<Tag>> {
            self.calls.started.lock().unwrap().push(path.to_path_buf());
            if let Some(delay) = self.delay {
                self.spawner.timer(delay).await;
            }
            self.calls.finished.lock().unwrap().push(path.to_path_buf());
            self.inner.read_tags(path).await
        }

        async fn write_tags(&self, path: &Path, tags: &[Tag]) -> anyhow::Result<()> {
            self.calls
                .writes
                .lock()
                .unwrap()
                .push((path.to_path_buf(), tags.to_vec()));
            self.inner.write_tags(path, tags).await
        }

        async fn known_tags(&self) -> anyhow::Result<Vec<Tag>> {
            self.inner.known_tags().await
        }

        async fn set_ownership(
            &self,
            path: &Path,
            owner: Option<&str>,
            group: Option<&str>,
        ) -> anyhow::Result<()> {
            self.inner.set_ownership(path, owner, group).await
        }
    }

    /// One slow read, long enough that nothing completes until a test advances
    /// the clock on purpose.
    const SLOW: Duration = Duration::from_millis(50);

    /// `/home`: 60 files, three of them tagged, in a window deliberately too
    /// small to paint them all.
    fn open_home(
        cx: &mut TestAppContext,
        delay: Option<Duration>,
    ) -> (
        Arc<Calls>,
        Arc<FakeVfs>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let calls: Arc<Calls> = Arc::default();
        let platform_calls = calls.clone();
        let vfs = cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            let mut files = serde_json::Map::new();
            for i in 0..60 {
                files.insert(format!("f{i:02}.txt"), json!("body"));
            }
            vfs.insert_tree("/home", serde_json::Value::Object(files));
            let stub = StubPlatform::new();
            // Seeded rather than written: the dots have to render for tags a
            // *previous* session (or Finder) left behind.
            stub.seed_tags("/home/f00.txt", vec![tag("Red", TagColor::Red)]);
            stub.seed_tags("/home/f01.txt", vec![tag("Work", TagColor::None)]);
            stub.seed_tags("/home/f59.txt", vec![tag("Red", TagColor::Red)]);
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner.clone(),
                Arc::new(LoggingOpener),
                Arc::new(RecordingPlatform {
                    inner: stub,
                    spawner,
                    delay,
                    calls: platform_calls,
                }),
            );
            crate::settings::init_with_path(cx, PathBuf::from("/config/settings.json"));
            vfs
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(crate::Theme::dark(), window, cx));
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/home"), cx));
        // A small window on purpose: the default test window paints all 60
        // rows, which would make "only the visible band" vacuously true.
        cx.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(320.0)));
        cx.run_until_parked();
        (calls, vfs, workspace, cx)
    }

    fn pane_of(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
        workspace.read_with(cx, |workspace, _| workspace.active_pane().clone())
    }

    fn view_of(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<DirView> {
        let pane = pane_of(workspace, cx);
        pane.read_with(cx, |pane, _| pane.dir_view().clone())
    }

    fn row_names(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<String> {
        let view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        view.read_with(cx, |view, _| {
            view.flat_rows()
                .iter()
                .map(|row| row.entry.name.to_string())
                .collect()
        })
    }

    /// The names inside the view's current tag request window.
    fn window_names(view: &Entity<DirView>, cx: &mut VisualTestContext) -> Vec<String> {
        view.read_with(cx, |view, _| {
            let window = view.tag_debug().0.unwrap_or(0..0);
            view.flat_rows()
                .get(window)
                .unwrap_or_default()
                .iter()
                .map(|row| row.entry.name.to_string())
                .collect()
        })
    }

    #[gpui::test]
    fn dots_render_for_tagged_rows_in_the_visible_band_only(cx: &mut TestAppContext) {
        let (calls, _vfs, workspace, cx) = open_home(cx, None);
        let view = view_of(&workspace, cx);

        let (window, cached, fetching) = view.read_with(cx, |view, _| view.tag_debug());
        let window = window.expect("the list painted, so a window was requested");
        assert!(
            window.end < 60,
            "a 60-row folder must not read every tag set at once: {window:?}"
        );
        assert!(!fetching, "the whole window finished while parked");
        assert_eq!(
            cached,
            calls.asked_once().len(),
            "one cache entry per row read"
        );

        let asked = calls.started();
        let in_window = window_names(&view, cx);
        for name in &asked {
            assert!(
                in_window.contains(name),
                "{name} is outside the request window {window:?}"
            );
        }
        assert!(
            !asked.iter().any(|name| name == "f59.txt"),
            "the last row of a 60-row folder is nowhere near the viewport"
        );

        // ...and the tags that arrived are the ones the rows paint.
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.entry_tags(Path::new("/home/f00.txt")),
                [tag("Red", TagColor::Red)]
            );
            assert!(tag_dots(view.entry_tags(Path::new("/home/f00.txt"))).is_some());
            assert!(
                tag_dots(view.entry_tags(Path::new("/home/f01.txt"))).is_none(),
                "an uncoloured tag is read but paints no dot"
            );
            assert!(view.entry_tags(Path::new("/home/f02.txt")).is_empty());
            assert!(
                view.entry_tags(Path::new("/home/f59.txt")).is_empty(),
                "unread, so nothing to paint — not a claim that it is untagged"
            );
        });
    }

    #[gpui::test]
    fn scrolling_away_cancels_the_read_it_left_in_flight(cx: &mut TestAppContext) {
        // A slow platform, so the top band's first read is still parked on its
        // timer when the viewport moves — the state a fast scroll leaves behind
        // on every line it passes.
        let (calls, _vfs, workspace, cx) = open_home(cx, Some(SLOW));
        let view = view_of(&workspace, cx);

        let started = calls.asked_once();
        assert_eq!(
            started.len(),
            1,
            "the reads are sequential, so only one row has been asked about: {started:?}"
        );
        assert!(calls.finished().is_empty(), "nothing completed yet");
        let abandoned = started[0].clone();

        view.update(cx, |view, cx| {
            view.apply_scroll_top(100_000.0);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            !window_names(&view, cx).contains(&abandoned),
            "the abandoned row really did leave the window"
        );

        for _ in 0..64 {
            cx.executor().advance_clock(SLOW * 2);
        }
        cx.run_until_parked();

        assert!(
            !calls.finished().contains(&abandoned),
            "{abandoned} scrolled out of view, so its read must have been dropped"
        );
        let bottom = window_names(&view, cx);
        let finished = calls.finished();
        assert!(
            bottom.iter().all(|name| finished.contains(name)),
            "every row in the surviving window was read: window={bottom:?}"
        );
        assert!(
            finished.len() < 60,
            "and the rows between the two windows were never read: {}",
            finished.len()
        );
        // The bottom band's tags are painted, and the top band's cache entries
        // were pruned with the window rather than kept for the pane's life.
        let cached = view.read_with(cx, |view, _| view.tag_debug().1);
        assert!(
            cached <= bottom.len(),
            "the cache is bounded by the window, not by history: {cached} vs {}",
            bottom.len()
        );
    }

    #[gpui::test]
    fn idle_repaints_neither_restart_the_read_nor_re_ask_for_a_cached_row(cx: &mut TestAppContext) {
        // The window comes from the scroll offset, not from the row range
        // `uniform_list` hands its processor (which gpui calls with `0..1`
        // twice a frame just to measure) — a window that flipped like that
        // would cancel its own read on every repaint, and no read slower than
        // the repaint cadence would ever finish.
        let (calls, _vfs, workspace, cx) = open_home(cx, Some(SLOW));
        let view = view_of(&workspace, cx);
        let before = view.read_with(cx, |view, _| view.tag_debug().0);
        let started_before = calls.asked_once();
        assert_eq!(started_before.len(), 1);

        for _ in 0..10 {
            view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }

        let (after, _, fetching) = view.read_with(cx, |view, _| view.tag_debug());
        assert_eq!(after, before, "an idle repaint must not move the window");
        assert_eq!(
            calls.asked_once(),
            started_before,
            "the read in flight survived every repaint instead of being restarted"
        );
        assert!(fetching, "...and is still the same one, still working");

        cx.executor().advance_clock(SLOW * 2);
        cx.run_until_parked();
        assert_eq!(
            calls.finished().first(),
            started_before.first(),
            "the read that survived the repaints is the one that finished"
        );
    }

    #[gpui::test]
    fn a_frame_paints_while_every_tag_read_is_still_parked(cx: &mut TestAppContext) {
        // The §5 invariant, as far as a test can observe it: with a platform
        // that answers no faster than 50 ms of *fake* time, the list has
        // already painted its rows and reports a read in flight. Nothing on the
        // render path awaited it — a `read_tags` on the UI thread could not
        // return without the clock moving, so the frame could not exist.
        let (calls, _vfs, workspace, cx) = open_home(cx, Some(SLOW));
        let pane = pane_of(&workspace, cx);
        assert!(!row_names(&pane, cx).is_empty(), "the rows painted");
        assert!(calls.finished().is_empty(), "no read has answered");
        assert!(
            view_of(&workspace, cx).read_with(cx, |view, _| view.tag_debug().2),
            "and one is in flight"
        );
    }

    // ------------------------------------------------------------------
    // The sidebar's Tags section
    // ------------------------------------------------------------------

    #[gpui::test]
    fn the_sidebar_lists_the_known_tags_with_the_users_own_among_them(cx: &mut TestAppContext) {
        let (_calls, _vfs, workspace, cx) = open_home(cx, None);
        let sidebar = workspace.read_with(cx, |workspace, _| workspace.sidebar().clone());
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            let names: Vec<&str> = sidebar.tags().iter().map(|t| t.name.as_ref()).collect();
            assert_eq!(
                &names[..7],
                ["Red", "Orange", "Yellow", "Green", "Blue", "Purple", "Gray"],
                "the palette, in Finder's order"
            );
            assert!(
                names.contains(&"Work"),
                "the user's own tag came from known_tags: {names:?}"
            );
        });
        // Every row painted, dot and all.
        assert!(cx.debug_bounds("sidebar-tag-0").is_some());
    }

    #[gpui::test]
    fn clicking_a_sidebar_tag_filters_the_pane_and_clicking_it_again_clears(
        cx: &mut TestAppContext,
    ) {
        let (_calls, _vfs, workspace, cx) = open_home(cx, None);
        let pane = pane_of(&workspace, cx);
        assert_eq!(row_names(&pane, cx).len(), 60, "unfiltered to begin with");

        let bounds = cx
            .debug_bounds("sidebar-tag-0")
            .expect("the Red row painted");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        // The scan is throttled like a search: let its batches land.
        for _ in 0..4 {
            cx.executor().advance_clock(crate::search::SEARCH_THROTTLE);
            cx.run_until_parked();
        }

        assert_eq!(
            row_names(&pane, cx),
            ["f00.txt", "f59.txt"],
            "both Red items, including the one far below the viewport"
        );
        pane.read_with(cx, |pane, _| {
            let filter = pane.tag_filter().expect("a filter is on");
            assert_eq!(filter.tag().name.as_ref(), "Red");
            assert!(!filter.is_running(), "the scan finished");
            assert!(
                pane.status_text()
                    .starts_with("2 items tagged \u{201c}Red\u{201d}"),
                "the status line reports the filter: {}",
                pane.status_text()
            );
        });

        // Clicking the lit row again clears it (Finder's toggle).
        let bounds = cx.debug_bounds("sidebar-tag-0").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(row_names(&pane, cx).len(), 60, "the filter is gone");
        pane.read_with(cx, |pane, _| assert!(pane.tag_filter().is_none()));
    }

    #[gpui::test]
    fn navigating_away_drops_the_tag_filter(cx: &mut TestAppContext) {
        let (_calls, vfs, workspace, cx) = open_home(cx, None);
        vfs.insert_tree("/other", json!({ "x.txt": "x" }));
        let pane = pane_of(&workspace, cx);
        pane.update(cx, |pane, cx| {
            pane.set_tag_filter(tag("Red", TagColor::Red), cx)
        });
        cx.run_until_parked();
        cx.executor().advance_clock(crate::search::SEARCH_THROTTLE);
        cx.run_until_parked();
        assert!(pane.read_with(cx, |pane, _| pane.tag_filter().is_some()));

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert!(
                pane.tag_filter().is_none(),
                "the filter was *of* the folder we left"
            );
        });
        assert_eq!(row_names(&pane, cx), ["x.txt"]);
    }

    #[gpui::test]
    fn a_tag_filter_replaces_a_live_search_rather_than_stacking_with_it(cx: &mut TestAppContext) {
        let (_calls, _vfs, workspace, cx) = open_home(cx, None);
        let pane = pane_of(&workspace, cx);
        pane.update(cx, |pane, cx| pane.set_search_text("f0", cx));
        cx.run_until_parked();
        assert_eq!(row_names(&pane, cx).len(), 10, "f00..f09");

        pane.update(cx, |pane, cx| {
            pane.set_tag_filter(tag("Red", TagColor::Red), cx)
        });
        cx.run_until_parked();
        for _ in 0..4 {
            cx.executor().advance_clock(crate::search::SEARCH_THROTTLE);
            cx.run_until_parked();
        }
        pane.read_with(cx, |pane, _| {
            assert!(pane.search().is_none(), "the query was dropped");
        });
        assert_eq!(
            row_names(&pane, cx),
            ["f00.txt", "f59.txt"],
            "f59 is a Red hit the query would have hidden"
        );
    }

    // ------------------------------------------------------------------
    // The `Tags ▸` submenu: checks, the job it submits, and undo
    // ------------------------------------------------------------------

    #[gpui::test]
    fn the_submenu_checks_only_tags_the_whole_selection_has(cx: &mut TestAppContext) {
        let (_calls, _vfs, workspace, cx) = open_home(cx, None);
        let view = view_of(&workspace, cx);

        // f00 is Red, f01 is not.
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/home/f00.txt")], cx)
        });
        let checked = view.read_with(cx, |view, _| {
            view.tags_on_whole_selection()
                .iter()
                .map(|t| t.name.to_string())
                .collect::<Vec<_>>()
        });
        assert_eq!(checked, ["Red"]);

        view.update(cx, |view, cx| {
            view.select_paths(
                &[Path::new("/home/f00.txt"), Path::new("/home/f01.txt")],
                cx,
            )
        });
        assert!(
            view.read_with(cx, |view, _| view.tags_on_whole_selection().is_empty()),
            "a tag only one of them has must not render checked"
        );
    }

    #[gpui::test]
    fn the_submenu_writes_the_tag_through_the_job_spine_and_cmd_z_puts_it_back(
        cx: &mut TestAppContext,
    ) {
        let (calls, _vfs, workspace, cx) = open_home(cx, None);
        let view = view_of(&workspace, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/home/f01.txt")], cx)
        });

        // The action the menu row dispatches, dispatched the way the menu
        // dispatches it: from the view's focus handle.
        cx.update(|window, cx| {
            let handle = view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
            window.dispatch_action(
                Box::new(crate::actions::ToggleTag {
                    tag: tag("Blue", TagColor::Blue),
                }),
                cx,
            );
        });
        cx.run_until_parked();

        // The write went through `Platform::write_tags` — i.e. through the
        // queue, not from the view — and kept the tag that was already there.
        let writes = calls.writes.lock().unwrap().clone();
        assert_eq!(
            writes,
            [(
                PathBuf::from("/home/f01.txt"),
                vec![tag("Work", TagColor::None), tag("Blue", TagColor::Blue)]
            )]
        );
        // ...and the row's dots now show it, without a listing change to
        // prompt one (an xattr write moves no mtime).
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.entry_tags(Path::new("/home/f01.txt")),
                [tag("Work", TagColor::None), tag("Blue", TagColor::Blue)]
            );
        });

        // Undo: the tag set goes back to exactly what it was.
        cx.update(|window, cx| {
            let handle = gpui::Focusable::focus_handle(workspace.read(cx), cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();
        let restored = calls.writes.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            restored,
            (
                PathBuf::from("/home/f01.txt"),
                vec![tag("Work", TagColor::None)]
            ),
            "undo wrote the previous set back"
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.entry_tags(Path::new("/home/f01.txt")),
                [tag("Work", TagColor::None)],
                "and the dots followed the undo"
            );
        });
    }

    #[gpui::test]
    fn tagging_a_multi_selection_never_overwrites_a_tag_it_did_not_read(cx: &mut TestAppContext) {
        let (calls, _vfs, workspace, cx) = open_home(cx, None);
        let view = view_of(&workspace, cx);
        // f00 (Red), f01 (Work) and f02 (nothing) — three different starting
        // sets, and the write must preserve all three.
        view.update(cx, |view, cx| {
            view.select_paths(
                &[
                    Path::new("/home/f00.txt"),
                    Path::new("/home/f01.txt"),
                    Path::new("/home/f02.txt"),
                ],
                cx,
            )
        });
        cx.update(|window, cx| {
            let handle = view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
            window.dispatch_action(
                Box::new(crate::actions::ToggleTag {
                    tag: tag("Green", TagColor::Green),
                }),
                cx,
            );
        });
        cx.run_until_parked();

        let mut writes = calls.writes.lock().unwrap().clone();
        writes.sort();
        assert_eq!(
            writes,
            [
                (
                    PathBuf::from("/home/f00.txt"),
                    vec![tag("Red", TagColor::Red), tag("Green", TagColor::Green)]
                ),
                (
                    PathBuf::from("/home/f01.txt"),
                    vec![tag("Work", TagColor::None), tag("Green", TagColor::Green)]
                ),
                (
                    PathBuf::from("/home/f02.txt"),
                    vec![tag("Green", TagColor::Green)]
                ),
            ]
        );
    }
}
