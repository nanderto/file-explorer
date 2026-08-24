//! The info panel (ARCHITECTURE.md §1 `info_panel.rs`, §9's M5 line): the
//! right-hand column of the plan §2 blueprint screenshot — preview, header,
//! a collapsible **General** section and a collapsible **Permissions** grid,
//! or the §2 multi-selection summary when more than one row is selected.
//!
//! Three rules shape it, and they are the same three [`crate::thumbnails`]
//! established for the icon grid:
//!
//! * **One subject at a time.** [`Subject`] is what the panel describes: a
//!   single path (the selected entry, or — with an empty selection — the
//!   folder the pane has open), a [`SelectionSummary`], or nothing at all.
//!   It is **path-keyed**, never entry-keyed (§2 invariant 2): everything the
//!   panel shows about a single subject comes from the load, so the panel
//!   never has to be told which `FileEntry` a path resolved to.
//! * **Debounced, single-slot loading.** Arrow-keying down a listing retargets
//!   the panel once per row. Each retarget *replaces* the load task, which
//!   drops it — cancelling the pending [`Spawner::timer`] before it fires, so
//!   holding `down` through a thousand rows costs one `stat`, not a thousand.
//!   Same caveat as `Platform::thumbnail`: work already handed to the
//!   background executor runs to completion with its result discarded, so the
//!   cost of a cancellation is bounded at one orphan, not zero.
//! * **Nothing blocking on the UI thread.** `Vfs::metadata`,
//!   `Platform::file_attrs` and `Platform::thumbnail` are all awaited on the
//!   *background* executor (`cx.background_executor().spawn`), so an
//!   `NSFileManager` round-trip or a QuickLook wait cannot land on the render
//!   thread even if a `Platform` implementation forgets to unblock. Only the
//!   debounce timer, the field assignment and the `cx.notify()` run on the UI
//!   thread — and `render` never touches the disk at all.
//!
//! **Whose selection?** The active pane's (AS_BUILT's "the info panel is
//! workspace-level, not per-pane" gap). The workspace pushes the active
//! pane's `DirView` down through [`InfoPanel::follow`] on every notify from
//! it and on every change of active pane, so clicking into the other half of
//! a split retargets the panel with the same code path a selection change
//! takes.
//!
//! The permission checkboxes and the octal field are **read-only in M5**:
//! they render as disabled controls with no click handlers at all, because a
//! control that looks live and silently does nothing is worse than one that
//! looks inert. Editing them is M6's `chmod` work.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fs_core::{
    EntryId, EntryMeta, FileAttrs, ListingSnapshot, PermBit, PermClass, SelectionSummary,
    is_previewable, summarize,
};
use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, RenderImage, SharedString, Task, Window, div,
    img, prelude::*, px,
};

use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::pane::format_bytes;
use crate::theme::Theme;
use crate::thumbnails::render_image;
use crate::views::details_list::format_modified;

/// How long the panel waits after the selection settles before it stats
/// anything. Long enough that arrow-keying through a listing (or a marquee
/// dragged across it) fires one load rather than one per row, short enough
/// that a deliberate click feels immediate.
pub const LOAD_DEBOUNCE: Duration = Duration::from_millis(130);

/// Longest edge, in pixels, the preview is requested at: twice
/// [`PREVIEW_MAX_HEIGHT`] so it is still sharp on a 2x display. The painted
/// size is constrained by the slot, so this can change without touching any
/// geometry.
const PREVIEW_PX: u32 = (PREVIEW_MAX_HEIGHT as u32) * 2;

/// Height of the preview slot. Fixed, so an arriving preview never reflows the
/// sections beneath it (the same rule as the icon grid's tile slot).
const PREVIEW_MAX_HEIGHT: f32 = 200.0;

/// Opacity the read-only M5 permission controls are drawn at, so they read as
/// disabled rather than as checkboxes nobody wired up.
const DISABLED_ALPHA: f32 = 0.55;

/// What the panel currently describes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Subject {
    /// No folder open and nothing selected — the only state that renders an
    /// empty message.
    #[default]
    Nothing,
    /// A single path: the selected entry, or the pane's open folder when the
    /// selection is empty.
    One { path: Arc<Path>, kind: OneKind },
    /// More than one entry selected: the §2 multi-selection summary.
    Many(SelectionSummary),
}

/// Why a [`Subject::One`] is being described — the difference between "here is
/// the file you clicked" and "nothing is selected, so here is the folder you
/// are looking at".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneKind {
    Selected,
    OpenFolder,
}

/// Everything the load brings back for a [`Subject::One`]: the stat plus the
/// [`FileAttrs`] that need an OS call beyond it.
struct Details {
    /// `None` when the path vanished between the selection and the stat.
    meta: Option<EntryMeta>,
    attrs: FileAttrs,
}

/// Cheap witness of "what the panel describes might have changed".
///
/// [`InfoPanel::follow`] runs on **every** notify from the active pane's
/// `DirView` — a scroll, a hover, an arriving thumbnail, a scrollbar fade —
/// and deriving the subject means building the flat projection, which is
/// O(listing). So the projection is only built when one of these O(1) values
/// moves:
///
/// * `dir` — the pane navigated somewhere else;
/// * `snapshot` — the pane's listing snapshot **by `Arc` identity**, which a
///   fresh load, a re-sort or a watcher patch replaces wholesale, so this is
///   also what makes a file rewritten while it is selected get its attributes
///   and preview re-read. The `Arc` is *held*, not reduced to a raw pointer:
///   a dropped snapshot's address is free for the allocator to hand back to
///   its replacement, and a witness that compared addresses would then miss
///   the change entirely. One extra snapshot stays resident for it, which the
///   pane's own `ListingCache` already holds sixteen of;
/// * `expansion` — an in-place folder expanded or collapsed, which changes
///   which entries the projection contains;
/// * the selection's size, cursor and extremes — every selection mutation
///   notifies exactly once, and no single mutation can leave all four
///   unchanged (a toggle changes the size; a range or a click changes the
///   cursor).
#[derive(Clone)]
struct Witness {
    dir: Option<Arc<Path>>,
    snapshot: Option<Arc<ListingSnapshot>>,
    expansion: (usize, usize),
    selected: usize,
    cursor: Option<EntryId>,
    first: Option<EntryId>,
    last: Option<EntryId>,
}

impl PartialEq for Witness {
    fn eq(&self, other: &Self) -> bool {
        let same_snapshot = match (&self.snapshot, &other.snapshot) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        same_snapshot
            && self.dir == other.dir
            && self.expansion == other.expansion
            && self.selected == other.selected
            && self.cursor == other.cursor
            && self.first == other.first
            && self.last == other.last
    }
}

impl Witness {
    fn of(view: &DirView, cx: &gpui::App) -> Self {
        let selection = view.selection();
        let snapshot = view
            .pane_entity()
            .and_then(|pane| pane.read(cx).snapshot().cloned());
        Self {
            dir: view.current_dir(cx),
            snapshot,
            expansion: view.expansion_state_sizes(),
            selected: selection.len(),
            cursor: selection.cursor().cloned(),
            first: selection.selected().iter().next().cloned(),
            last: selection.selected().iter().next_back().cloned(),
        }
    }
}

pub struct InfoPanel {
    theme: Theme,
    subject: Subject,
    /// `None` until the debounced load for the current subject returns.
    details: Option<Details>,
    /// The decoded preview for the current subject, when it has one.
    preview: Option<Arc<RenderImage>>,
    /// Previews replaced by a retarget, waiting for a frame that has a
    /// `Window` to hand them back to the atlas (see [`Self::render`]).
    retired: Vec<Arc<RenderImage>>,
    /// A preview has been asked for and has not come back yet — the only
    /// difference between "this subject has no preview" and "its preview is
    /// still in flight", which is what [`Self::is_settled`] needs.
    preview_pending: bool,
    /// The single slot: the debounce timer, the stat, the attribute lookup and
    /// the preview fetch are one task, so replacing it cancels all four.
    _load: Option<Task<()>>,
    /// The last witness [`Self::follow`] acted on.
    witness: Option<Witness>,
    general_open: bool,
    permissions_open: bool,
    /// How many loads have actually reached the panel — the debounce's only
    /// observable (a load that was cancelled before its timer fired is
    /// invisible to every other piece of state).
    #[cfg(test)]
    loads: usize,
}

impl InfoPanel {
    pub fn new(theme: Theme) -> Self {
        Self {
            theme,
            subject: Subject::Nothing,
            details: None,
            preview: None,
            retired: Vec::new(),
            preview_pending: false,
            _load: None,
            witness: None,
            general_open: true,
            permissions_open: true,
            #[cfg(test)]
            loads: 0,
        }
    }

    /// Point the panel at `dir_view`'s selection. Idempotent per [`Witness`]:
    /// the notifies that do not change what is being described cost two `Arc`
    /// clones and a comparison, and specifically do **not** restart the
    /// debounce — a preview that takes longer than the repaint cadence would
    /// otherwise never arrive.
    pub fn follow(&mut self, dir_view: &Entity<DirView>, cx: &mut Context<Self>) {
        // The witness is compared *before* the subject is derived, and that
        // order is the whole point of having one: `subject_of` builds the flat
        // projection, which is O(listing) and allocates, and the notifies that
        // reach here are overwhelmingly the idle ones.
        let witness = Witness::of(dir_view.read(cx), cx);
        if self.witness.as_ref() == Some(&witness) {
            return;
        }
        self.witness = Some(witness);
        let subject = subject_of(dir_view.read(cx), cx);
        self.retarget(subject, cx);
    }

    /// Forget whatever was being described (the workspace calls this when the
    /// panel is hidden, so a hidden panel stats nothing at all).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.witness = None;
        self.retarget(Subject::Nothing, cx);
    }

    pub fn subject(&self) -> &Subject {
        &self.subject
    }

    /// Whether everything the panel is going to paint for its current subject
    /// has arrived: the stat, the attributes and — when the subject has one —
    /// the preview.
    ///
    /// The visual test runner asserts this before it captures: the panel's
    /// values are debounced, so a scenario whose gesture moves the selection
    /// after the last clock advance would otherwise bake em dashes and a
    /// placeholder glyph into a baseline that looks plausible in code review.
    pub fn is_settled(&self) -> bool {
        match self.subject {
            Subject::One { .. } => self.details.is_some() && !self.preview_pending,
            Subject::Nothing | Subject::Many(_) => true,
        }
    }

    /// Point the panel at `subject` and (re)start its load.
    ///
    /// Deliberately **not** skipped when `subject == self.subject`: the same
    /// path can need re-reading (a listing patch after the selected file was
    /// rewritten leaves the subject identical and its size, mtime and preview
    /// stale), and the guard that keeps idle repaints from restarting the
    /// debounce is the [`Witness`] in [`Self::follow`], not this comparison.
    /// An unchanged subject does, however, keep its values painted while the
    /// re-read runs — see below.
    fn retarget(&mut self, subject: Subject, cx: &mut Context<Self>) {
        // A re-read of the *same* subject keeps what is already painted and
        // swaps the new values in when they land. Anything changing inside the
        // open folder replaces the pane's snapshot every `WATCH_LATENCY`
        // (100 ms), which is *shorter* than `LOAD_DEBOUNCE`, so clearing here
        // would leave the panel permanently at em dashes with no preview for as
        // long as a download, a log or a copy job kept the folder busy.
        let re_read = matches!(subject, Subject::One { .. }) && subject == self.subject;
        self.subject = subject;
        // Cancels the pending debounce, the stat in flight and the preview
        // fetch behind it, all in one drop.
        self._load = None;
        self.preview_pending = false;
        if !re_read {
            self.details = None;
            if let Some(image) = self.preview.take() {
                self.retired.push(image);
            }
        }
        if let Subject::One { path, .. } = &self.subject {
            self.spawn_load(path.clone(), cx);
        }
        cx.notify();
    }

    /// The debounced load. Everything that touches the disk or the OS is
    /// awaited on the background executor; the UI thread only parks on the
    /// timer and folds the results in.
    fn spawn_load(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
        let fs = FsContext::global(cx);
        let vfs = fs.vfs.clone();
        let platform = fs.platform.clone();
        let spawner = fs.spawner.clone();
        self._load = Some(cx.spawn(async move |this, cx| {
            // §7 invariant: all debounce logic goes through `Spawner::timer`,
            // so this runs on fake time under `#[gpui::test]`.
            spawner.timer(LOAD_DEBOUNCE).await;

            let (meta, attrs) = {
                let (vfs, platform, path) = (vfs.clone(), platform.clone(), path.clone());
                cx.background_executor()
                    .spawn(async move {
                        // A missing path is `Ok(None)`, and a platform that
                        // cannot answer degrades to default attributes — the
                        // panel shows what is known rather than an error.
                        let meta = vfs.metadata(&path).await.ok().flatten();
                        let attrs = platform.file_attrs(&path).await.unwrap_or_default();
                        (meta, attrs)
                    })
                    .await
            };
            let previewable = meta
                .as_ref()
                .is_some_and(|meta| !meta.kind.is_dir_like() && is_previewable(&path, meta.size));
            if this
                .update(cx, |this, cx| {
                    this.details = Some(Details { meta, attrs });
                    this.preview_pending = previewable;
                    // A re-read kept the previous preview painted (see
                    // `retarget`); if the file is no longer previewable — grown
                    // past the ceiling, replaced by a folder — that preview is
                    // now a lie, so it goes back to the atlas.
                    if !previewable && let Some(image) = this.preview.take() {
                        this.retired.push(image);
                    }
                    #[cfg(test)]
                    {
                        this.loads += 1;
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            if !previewable {
                return;
            }

            // `fs_core::is_previewable`: never ask QuickLook about every `.o`
            // in `/usr/lib`, and never about a multi-gigabyte disk image. The
            // panel is the *only* caller of the gate so far — the icon grid's
            // `thumbnails::pending_thumbnails` still asks for every tile, which
            // is a recorded Known gap (switching it over moves every icon-grid
            // baseline).
            let thumbnail = cx
                .background_executor()
                .spawn(async move { platform.thumbnail(&path, PREVIEW_PX).await })
                .await;
            let Ok(thumbnail) = thumbnail else {
                this.update(cx, |this, _| this.preview_pending = false).ok();
                // "No preview available" is an ordinary outcome, not an error
                // to surface: the header keeps its type glyph. Nothing retries
                // it, because the only thing that requests a preview is a
                // change of subject.
                return;
            };
            this.update(cx, |this, cx| {
                this.preview_pending = false;
                if let Some(image) = render_image(&thumbnail).map(Arc::new) {
                    if let Some(previous) = this.preview.replace(image) {
                        this.retired.push(previous);
                    }
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Test window into the machine: the number of loads that have landed and
    /// whether a preview is decoded.
    #[cfg(test)]
    pub(crate) fn load_debug(&self) -> (usize, bool) {
        (self.loads, self.preview.is_some())
    }

    #[cfg(test)]
    pub(crate) fn attrs(&self) -> Option<&FileAttrs> {
        self.details.as_ref().map(|details| &details.attrs)
    }

    fn toggle_general(&mut self, cx: &mut Context<Self>) {
        self.general_open = !self.general_open;
        cx.notify();
    }

    fn toggle_permissions(&mut self, cx: &mut Context<Self>) {
        self.permissions_open = !self.permissions_open;
        cx.notify();
    }
}

/// What the panel should describe, given the active pane's view. Pure with
/// respect to the disk: the projection and the selection are both already in
/// memory, and the *values* (name, size, permissions) come from the load.
fn subject_of(view: &DirView, cx: &gpui::App) -> Subject {
    let selection = view.selection();
    if selection.is_empty() {
        return match view.current_dir(cx) {
            Some(path) => Subject::One {
                path,
                kind: OneKind::OpenFolder,
            },
            None => Subject::Nothing,
        };
    }
    // Built here rather than read from `DirView::flat_rows`, which is the
    // *last painted* projection: this runs from an observer, which fires
    // before the frame that would refresh it, so a navigation's first notify
    // would otherwise describe the previous directory's rows forever.
    let rows = view.projected_rows(cx);
    let selected: Vec<&fs_core::FileEntry> = rows
        .iter()
        .map(|row| &row.entry)
        .filter(|entry| selection.is_selected(&entry.id()))
        .collect();
    match selected.as_slice() {
        // A selection whose paths all left the listing (a delete the watcher
        // has patched in, but whose `retain` has not run yet) describes the
        // folder, not a phantom.
        [] => match view.current_dir(cx) {
            Some(path) => Subject::One {
                path,
                kind: OneKind::OpenFolder,
            },
            None => Subject::Nothing,
        },
        [only] => Subject::One {
            path: only.path.clone(),
            kind: OneKind::Selected,
        },
        many => Subject::Many(summarize(many.iter().copied())),
    }
}

// ----------------------------------------------------------------------
// Rendering
// ----------------------------------------------------------------------

impl Render for InfoPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Hand the atlas slots of superseded previews back on the first frame
        // that has a `Window` — a retarget happens in an observer, which has
        // none.
        for image in std::mem::take(&mut self.retired) {
            cx.drop_image(image, Some(window));
        }

        let theme = self.theme.clone();
        let body: AnyElement = match &self.subject {
            Subject::Nothing => self.render_empty(),
            Subject::Many(summary) => self.render_summary(*summary),
            Subject::One { path, kind } => self.render_one(path.clone(), *kind, cx),
        };
        div()
            .id("info-panel")
            .debug_selector(|| "info-panel".to_string())
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .text_size(px(11.0))
            .text_color(theme.text)
            .child(body)
    }
}

impl InfoPanel {
    fn render_empty(&self) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .px(px(12.0))
            .text_size(px(12.0))
            .text_color(self.theme.muted)
            .child(SharedString::new_static("Nothing to show"))
            .into_any_element()
    }

    /// The §2 multi-selection summary. Deliberately *not* the single-entry
    /// General/Permissions sections with one row's values in them: nine files
    /// have nine modes, and showing the first one's would be a lie.
    fn render_summary(&self, summary: SelectionSummary) -> AnyElement {
        let theme = &self.theme;
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .child(SharedString::new(format!("{} items", summary.count()))),
                    )
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(SharedString::new(format_bytes(summary.total_size))),
                    ),
            )
            .child(self.section(
                "Selection",
                vec![
                    ("Items", SharedString::new(summary.count().to_string())),
                    ("Folders", SharedString::new(summary.dirs.to_string())),
                    ("Files", SharedString::new(summary.files.to_string())),
                    (
                        "Total size",
                        SharedString::new(format_size(summary.total_size)),
                    ),
                ],
            ))
            .into_any_element()
    }

    fn render_one(&self, path: Arc<Path>, kind: OneKind, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme.clone();
        let meta = self.details.as_ref().and_then(|d| d.meta.as_ref());
        let attrs = self.details.as_ref().map(|d| &d.attrs);
        let (name, subtitle) = header_text(&path, meta, attrs);
        let general = general_rows(&path, kind, meta, attrs);

        div()
            .flex()
            .flex_col()
            .child(self.render_preview(meta))
            // Header: the name, then "<type description> — <size>".
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(12.0))
                    .pt(px(8.0))
                    .pb(px(10.0))
                    .child(div().text_size(px(13.0)).child(SharedString::new(name)))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child(SharedString::new(subtitle)),
                    )
                    .when(kind == OneKind::OpenFolder, |el| {
                        // The M5 bug this milestone kills was a panel reading
                        // "No selection" beside visibly selected rows; the
                        // opposite mistake is a panel describing the folder
                        // while the user thinks they are looking at a file.
                        el.child(
                            div()
                                .text_color(theme.muted)
                                .child(SharedString::new_static("Open folder — nothing selected")),
                        )
                    }),
            )
            .child(
                self.section_header("info-general", "General", self.general_open, cx)
                    .into_any_element(),
            )
            .when(self.general_open, |el| {
                el.child(self.rows(general))
                    .child(self.checkbox_row(
                        "Hide Extension",
                        attrs.is_some_and(|attrs| attrs.extension_hidden),
                    ))
                    .child(self.checkbox_row("Hidden", meta.is_some_and(|meta| meta.hidden)))
            })
            .child(
                self.section_header("info-permissions", "Permissions", self.permissions_open, cx)
                    .into_any_element(),
            )
            .when(self.permissions_open, |el| {
                el.child(self.render_permissions(attrs))
            })
            .into_any_element()
    }

    /// The preview slot: the decoded preview when there is one, the type glyph
    /// when there is not. Fixed height, so an arriving preview never reflows
    /// the sections beneath it.
    fn render_preview(&self, meta: Option<&EntryMeta>) -> AnyElement {
        let theme = &self.theme;
        let slot = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w_full()
            .h(px(PREVIEW_MAX_HEIGHT))
            .px(px(12.0))
            .pt(px(12.0));
        match self.preview.clone() {
            Some(image) => slot
                .child(
                    img(image)
                        .max_w_full()
                        .max_h(px(PREVIEW_MAX_HEIGHT - 12.0))
                        .rounded(px(3.0)),
                )
                .into_any_element(),
            None => {
                let folder = meta.is_some_and(|meta| meta.kind.is_dir_like());
                slot.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(96.0))
                        .h(px(96.0))
                        .rounded(px(4.0))
                        .bg(theme.accent.opacity(if folder { 0.20 } else { 0.10 }))
                        .text_size(px(34.0))
                        .text_color(theme.muted)
                        .child(SharedString::new_static(if folder { "▣" } else { "▢" })),
                )
                .into_any_element()
            }
        }
    }

    /// A collapsible section's header row: the title and its disclosure
    /// chevron, clickable as one target.
    fn section_header(
        &self,
        id: &'static str,
        title: &'static str,
        open: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let general = title == "General";
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex()
            .items_center()
            .justify_between()
            .px(px(12.0))
            .py(px(5.0))
            .border_t_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.accent.opacity(0.10)))
            .text_size(px(12.0))
            .child(SharedString::new_static(title))
            .child(
                div()
                    .text_color(theme.muted)
                    .child(SharedString::new_static(if open { "⌄" } else { "›" })),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if general {
                    this.toggle_general(cx);
                } else {
                    this.toggle_permissions(cx);
                }
            }))
    }

    /// A section with static rows and no disclosure of its own (the summary
    /// state, whose one section is always open).
    fn section(
        &self,
        title: &'static str,
        rows: Vec<(&'static str, SharedString)>,
    ) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .px(px(12.0))
                    .py(px(5.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(12.0))
                    .child(SharedString::new_static(title)),
            )
            .child(self.rows(rows))
    }

    /// Label on the left, value on the right — the screenshot's General rows.
    fn rows(&self, rows: Vec<(&'static str, SharedString)>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        div()
            .flex()
            .flex_col()
            .children(rows.into_iter().map(move |(label, value)| {
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(3.0))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.muted)
                            .child(SharedString::new_static(label)),
                    )
                    .child(div().flex_1().text_right().child(value))
            }))
    }

    /// A label with a **read-only** checkbox on the right (Hide Extension,
    /// Hidden, Locked).
    fn checkbox_row(&self, label: &'static str, checked: bool) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(12.0))
            .py(px(3.0))
            .child(
                div()
                    .text_color(theme.muted)
                    .child(SharedString::new_static(label)),
            )
            .child(checkbox(&theme, checked))
    }

    /// A label with a **read-only** dropdown on the right (Owner, Group): the
    /// value in a bordered box with a disclosure chevron, drawn at
    /// [`DISABLED_ALPHA`] and with no click handler, exactly like the octal
    /// field beside it.
    fn dropdown_row(&self, label: &'static str, value: SharedString) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(3.0))
            .child(
                div()
                    .flex_none()
                    .text_color(theme.muted)
                    .child(SharedString::new_static(label)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(5.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface.opacity(DISABLED_ALPHA))
                    .child(value)
                    .child(
                        div()
                            .text_color(theme.muted.opacity(DISABLED_ALPHA))
                            .child(SharedString::new_static("⌄")),
                    ),
            )
    }

    /// The R/W/X grid, the octal field, owner, group and Locked — the
    /// screenshot's Permissions section, read-only for M5.
    fn render_permissions(&self, attrs: Option<&FileAttrs>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let perms = attrs.and_then(|attrs| attrs.perms);
        let matrix = perm_matrix(perms);
        let column = |label: &'static str| {
            div()
                .flex()
                .flex_none()
                .w(px(24.0))
                .items_center()
                .justify_center()
                .child(SharedString::new_static(label))
        };
        let header = div()
            .flex()
            .items_center()
            .px(px(12.0))
            .py(px(3.0))
            .text_color(theme.muted)
            .child(div().flex_1())
            .child(column("R"))
            .child(column("W"))
            .child(column("X"));

        let class_row = |label: &'static str, class: PermClass| {
            let mut row = div().flex().items_center().px(px(12.0)).py(px(3.0)).child(
                div()
                    .flex_1()
                    .text_color(theme.muted)
                    .child(SharedString::new_static(label)),
            );
            for bit in [PermBit::Read, PermBit::Write, PermBit::Exec] {
                row = row.child(
                    div()
                        .flex()
                        .flex_none()
                        .w(px(24.0))
                        .items_center()
                        .justify_center()
                        .child(checkbox(&theme, matrix[class_ix(class)][bit_ix(bit)])),
                );
            }
            row
        };

        div()
            .flex()
            .flex_col()
            .child(header)
            .child(class_row("Owner", PermClass::Owner))
            .child(class_row("Group", PermClass::Group))
            .child(class_row("Others", PermClass::Others))
            // Octal: the symbolic form beside the boxed mode, as in the
            // screenshot. A field-looking box, but with no `TextInput` behind
            // it until M6 makes it editable.
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(3.0))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.muted)
                            .child(SharedString::new_static("Octal")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .text_color(theme.muted)
                                    .child(SharedString::new(text_of(
                                        perms.map(|perms| perms.symbolic()),
                                    ))),
                            )
                            .child(
                                div()
                                    .px(px(5.0))
                                    .rounded(px(3.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.surface.opacity(DISABLED_ALPHA))
                                    .child(SharedString::new(text_of(
                                        perms.map(|perms| perms.octal()),
                                    ))),
                            ),
                    ),
            )
            // Owner and group are *dropdowns* in the blueprint screenshot
            // ("johnappleseed ⌄", "staff ⌄"), so they get the same disabled
            // shape the octal field has: a control M6's `chown` can fill in,
            // not static text that would have to be rebuilt as one.
            .child(self.dropdown_row(
                "Owner",
                SharedString::new(text_of(attrs.and_then(|attrs| attrs.owner.clone()))),
            ))
            .child(self.dropdown_row(
                "Group",
                SharedString::new(text_of(attrs.and_then(|attrs| attrs.group.clone()))),
            ))
            .child(self.checkbox_row("Locked", attrs.is_some_and(|attrs| attrs.locked)))
    }
}

/// The header's two lines: the subject's display name, and
/// `"<type description> — <size>"` (the size omitted for a folder, whose own
/// inode size is not what anyone means by a folder's size).
///
/// Pure, so the values the panel paints are testable without a frame.
fn header_text(
    path: &Path,
    meta: Option<&EntryMeta>,
    attrs: Option<&FileAttrs>,
) -> (String, String) {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        // The volume root has no file name of its own.
        .unwrap_or_else(|| path.display().to_string());
    let type_description = attrs
        .and_then(|attrs| attrs.type_description.clone())
        .unwrap_or_else(|| {
            match meta.map(|meta| meta.kind.is_dir_like()) {
                Some(true) => "Folder",
                Some(false) => "File",
                // The load has not landed yet; the header's second line fills
                // in rather than the panel flashing empty.
                None => "…",
            }
            .to_string()
        });
    let subtitle = match meta {
        Some(meta) if !meta.kind.is_dir_like() => {
            format!("{type_description} — {}", format_bytes(meta.size))
        }
        _ => type_description,
    };
    (name, subtitle)
}

/// The General section's label/value rows, in the blueprint's order. Pure: it
/// reads only the path and the loaded values, so every rule below (which path
/// a `Path` row shows, the em dash for a value that has not landed, a folder's
/// absent size) is unit-testable without rendering a frame.
fn general_rows(
    path: &Path,
    kind: OneKind,
    meta: Option<&EntryMeta>,
    attrs: Option<&FileAttrs>,
) -> Vec<(&'static str, SharedString)> {
    let dir_like = meta.is_some_and(|meta| meta.kind.is_dir_like());
    vec![
        (
            "Path",
            // The *containing* folder for a selected item, as in the blueprint
            // screenshot ("/Users/johnappleseed/Photos" beside "Air
            // Balloon.jpg") — but a folder being described because nothing is
            // selected shows its own path, which is the thing the user is
            // actually looking at.
            SharedString::new(match kind {
                OneKind::OpenFolder => path.display().to_string(),
                OneKind::Selected => path
                    .parent()
                    .map(|parent| parent.display().to_string())
                    .unwrap_or_else(|| path.display().to_string()),
            }),
        ),
        (
            "Size",
            // A directory's `size` is its own inode size, not its contents' —
            // the details list already shows an em dash in that column for
            // exactly this reason, and the two must not disagree. Recursive
            // folder sizing is a separate cancellable job (M6).
            SharedString::new(if dir_like {
                "—".to_string()
            } else {
                text_of(meta.map(|m| format_size(m.size)))
            }),
        ),
        (
            "Modified",
            SharedString::new(text_of(meta.map(|m| format_modified(m.modified)))),
        ),
        (
            "Created",
            SharedString::new(text_of(meta.and_then(|m| m.created).map(format_modified))),
        ),
        (
            "Added",
            SharedString::new(text_of(
                attrs.and_then(|attrs| attrs.added).map(format_modified),
            )),
        ),
        (
            "Extension",
            SharedString::new(
                path.extension()
                    .map(|ext| ext.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "—".to_string()),
            ),
        ),
    ]
}

/// Row index of a permission class in [`perm_matrix`] — owner, group, others,
/// top to bottom, as `ls -l` writes them.
fn class_ix(class: PermClass) -> usize {
    match class {
        PermClass::Owner => 0,
        PermClass::Group => 1,
        PermClass::Others => 2,
    }
}

/// Column index of a permission bit in [`perm_matrix`]: R, W, X.
fn bit_ix(bit: PermBit) -> usize {
    match bit {
        PermBit::Read => 0,
        PermBit::Write => 1,
        PermBit::Exec => 2,
    }
}

/// The checkbox grid as data: `[class][bit]`, all false when the mode is not
/// known. Pure, so a transposed grid is a failing test rather than a baseline
/// nobody opened.
fn perm_matrix(perms: Option<fs_core::UnixPerms>) -> [[bool; 3]; 3] {
    let mut matrix = [[false; 3]; 3];
    for class in [PermClass::Owner, PermClass::Group, PermClass::Others] {
        for bit in [PermBit::Read, PermBit::Write, PermBit::Exec] {
            matrix[class_ix(class)][bit_ix(bit)] = perms.is_some_and(|p| p.allows(class, bit));
        }
    }
    matrix
}

/// A read-only checkbox: filled with the theme accent when set, an empty
/// outline when not, and drawn at [`DISABLED_ALPHA`] with no click handler at
/// all so it reads as a disabled control rather than a dead one.
fn checkbox(theme: &Theme, checked: bool) -> impl IntoElement + use<> {
    let mut box_ = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(13.0))
        .h(px(13.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(theme.border);
    if checked {
        box_ = box_
            .bg(theme.accent.opacity(DISABLED_ALPHA))
            .text_size(px(9.0))
            .text_color(theme.text.opacity(DISABLED_ALPHA))
            .child(SharedString::new_static("✓"));
    }
    box_
}

/// A value, or the em dash the panel shows for "not known" — either because
/// the load has not landed yet or because the platform could not answer.
fn text_of(value: Option<String>) -> String {
    value.unwrap_or_else(|| "—".to_string())
}

/// Size the way the screenshot writes it: the human form and the exact byte
/// count, e.g. `"1.4 MB (1,469,302 bytes)"`. A single byte count is not
/// worth two forms, so it stays one.
pub(crate) fn format_size(bytes: u64) -> String {
    if bytes == 1 {
        return "1 byte".to_string();
    }
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    format!("{} ({} bytes)", format_bytes(bytes), group_digits(bytes))
}

/// Thousands-separated decimal, so a byte count is readable at a glance. The
/// separator matches the decimal point [`format_bytes`] already uses (`.`),
/// rather than the screenshot's European convention; locale-aware formatting
/// is theme/settings work, not M5.
fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::pane::Pane;
    use crate::workspace::Workspace;
    use fs_core::{Spawner, Thumbnail, VolumeId, VolumeInfo};
    use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn format_size_shows_the_human_form_beside_the_exact_bytes() {
        // Under a kibibyte there is only one sensible form.
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1), "1 byte");
        assert_eq!(format_size(1023), "1023 bytes");
        // Above it, both — the screenshot's "1,5 MB (1 469 302 bytes)" row.
        assert_eq!(format_size(1024), "1.0 KB (1,024 bytes)");
        assert_eq!(format_size(1_469_302), "1.4 MB (1,469,302 bytes)");
        assert_eq!(
            format_size(u64::MAX),
            // `format_bytes` tops out at TB, which is what the status line
            // already shows; the exact count carries the magnitude.
            "16777216.0 TB (18,446,744,073,709,551,615 bytes)"
        );
    }

    #[test]
    fn group_digits_separates_every_third_digit_from_the_right() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(12_345), "12,345");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn text_of_falls_back_to_an_em_dash() {
        assert_eq!(text_of(Some("staff".to_string())), "staff");
        assert_eq!(text_of(None), "—");
    }

    // ------------------------------------------------------------------
    // The values the panel paints, without a frame
    // ------------------------------------------------------------------

    fn stat_of(kind: fs_core::EntryKind, size: u64) -> EntryMeta {
        EntryMeta {
            kind,
            size,
            modified: std::time::UNIX_EPOCH + Duration::from_secs(86_400),
            created: None,
            hidden: false,
        }
    }

    fn value<'a>(rows: &'a [(&'static str, SharedString)], label: &str) -> &'a str {
        rows.iter()
            .find(|(name, _)| *name == label)
            .map(|(_, value)| value.as_ref())
            .unwrap_or_else(|| panic!("no {label} row"))
    }

    // The blueprint's rule, and the one a transposition would silently break:
    // a *selected* item's Path row is its containing folder, but a folder shown
    // because nothing is selected shows its own path.
    #[test]
    fn the_path_row_shows_the_parent_of_a_selection_and_the_folder_itself_otherwise() {
        let file = Path::new("/home/Pictures/photo.jpg");
        let meta = stat_of(fs_core::EntryKind::File, 24_576);
        let rows = general_rows(file, OneKind::Selected, Some(&meta), None);
        assert_eq!(value(&rows, "Path"), "/home/Pictures");
        assert_eq!(value(&rows, "Extension"), "jpg");
        assert_eq!(value(&rows, "Size"), "24.0 KB (24,576 bytes)");

        let folder = Path::new("/home/Pictures");
        let dir = stat_of(fs_core::EntryKind::Dir, 96);
        let rows = general_rows(folder, OneKind::OpenFolder, Some(&dir), None);
        assert_eq!(value(&rows, "Path"), "/home/Pictures");
        // A directory's inode size is not the folder's size — the details
        // list's Size column shows an em dash for the same reason.
        assert_eq!(value(&rows, "Size"), "—");
        assert_eq!(value(&rows, "Extension"), "—");
    }

    // Before the load lands every value is an em dash rather than a zero or a
    // 1970 date, which would both read as facts.
    #[test]
    fn general_rows_before_the_load_are_all_em_dashes() {
        let rows = general_rows(Path::new("/home/notes.md"), OneKind::Selected, None, None);
        for label in ["Size", "Modified", "Created", "Added"] {
            assert_eq!(value(&rows, label), "—", "{label}");
        }
        // ...except the two the path itself already answers.
        assert_eq!(value(&rows, "Path"), "/home");
        assert_eq!(value(&rows, "Extension"), "md");
    }

    #[test]
    fn the_header_omits_the_size_for_a_folder_and_waits_for_the_load() {
        let attrs = FileAttrs {
            type_description: Some("JPEG image".to_string()),
            ..FileAttrs::default()
        };
        let file = stat_of(fs_core::EntryKind::File, 24_576);
        assert_eq!(
            header_text(
                Path::new("/home/Pictures/photo.jpg"),
                Some(&file),
                Some(&attrs)
            ),
            ("photo.jpg".to_string(), "JPEG image — 24.0 KB".to_string())
        );
        // No type description from the platform: the kind still names it, and
        // a folder's line carries no size at all.
        let dir = stat_of(fs_core::EntryKind::Dir, 96);
        assert_eq!(
            header_text(Path::new("/home/Pictures"), Some(&dir), None),
            ("Pictures".to_string(), "Folder".to_string())
        );
        // Load in flight: an ellipsis, not an empty line that would reflow.
        assert_eq!(
            header_text(Path::new("/home/Pictures/photo.jpg"), None, None),
            ("photo.jpg".to_string(), "…".to_string())
        );
    }

    // The grid is three rows of three, in `ls -l` order. A transposed or
    // rotated matrix is exactly the kind of mistake only a baseline would
    // otherwise catch.
    #[test]
    fn perm_matrix_maps_ls_order_onto_the_grid() {
        // 0o640: owner rw-, group r--, others ---.
        let matrix = perm_matrix(Some(fs_core::UnixPerms::from_mode(0o640)));
        assert_eq!(
            matrix,
            [
                [true, true, false],
                [true, false, false],
                [false, false, false]
            ]
        );
        // 0o751 is asymmetric in every axis, so a rotation cannot pass.
        assert_eq!(
            perm_matrix(Some(fs_core::UnixPerms::from_mode(0o751))),
            [
                [true, true, true],
                [true, false, true],
                [false, false, true]
            ]
        );
        // No mode known ⇒ nothing checked, rather than a plausible 000.
        assert_eq!(perm_matrix(None), [[false; 3]; 3]);
    }

    // ------------------------------------------------------------------
    // The machine, on a real workspace
    // ------------------------------------------------------------------

    /// What the platform was asked for and what it actually delivered. Both
    /// halves matter: the debounce is a claim about the calls *made*, and
    /// cancellation is a claim about a call that was started and never
    /// finished, which the panel's own state cannot show.
    #[derive(Default)]
    struct Calls {
        attrs_started: std::sync::Mutex<Vec<PathBuf>>,
        attrs_finished: std::sync::Mutex<Vec<PathBuf>>,
        thumbnails: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl Calls {
        fn started(&self) -> Vec<PathBuf> {
            self.attrs_started.lock().unwrap().clone()
        }

        fn finished(&self) -> Vec<PathBuf> {
            self.attrs_finished.lock().unwrap().clone()
        }

        fn thumbnails(&self) -> Vec<PathBuf> {
            self.thumbnails.lock().unwrap().clone()
        }
    }

    /// A recording [`fs_core::Platform`], optionally *slow*: with a `delay` it
    /// parks on a [`Spawner`] timer between being called and answering, which
    /// is what gives a test a load it can catch in flight.
    struct RecordingPlatform {
        inner: fs_core::StubPlatform,
        spawner: Arc<dyn Spawner>,
        delay: Option<Duration>,
        calls: Arc<Calls>,
    }

    #[async_trait::async_trait]
    impl fs_core::Platform for RecordingPlatform {
        async fn volumes(&self) -> anyhow::Result<Vec<VolumeInfo>> {
            self.inner.volumes().await
        }

        async fn eject(&self, volume_id: &VolumeId) -> anyhow::Result<()> {
            self.inner.eject(volume_id).await
        }

        async fn thumbnail(&self, path: &Path, px: u32) -> anyhow::Result<Thumbnail> {
            self.calls.thumbnails.lock().unwrap().push(path.to_owned());
            self.inner.thumbnail(path, px).await
        }

        async fn file_attrs(&self, path: &Path) -> anyhow::Result<FileAttrs> {
            self.calls
                .attrs_started
                .lock()
                .unwrap()
                .push(path.to_owned());
            if let Some(delay) = self.delay {
                self.spawner.timer(delay).await;
            }
            self.calls
                .attrs_finished
                .lock()
                .unwrap()
                .push(path.to_owned());
            self.inner.file_attrs(path).await
        }
    }

    /// One slow lookup, deliberately far longer than [`LOAD_DEBOUNCE`] so a
    /// test can advance past the debounce — starting the lookup — without
    /// also letting it finish.
    const SLOW: Duration = Duration::from_secs(5);

    /// Just past the debounce: the load is started, and with a `SLOW`
    /// platform it is then parked inside `file_attrs`.
    fn advance_past_debounce(cx: &mut VisualTestContext) {
        cx.executor()
            .advance_clock(LOAD_DEBOUNCE + Duration::from_millis(10));
        cx.run_until_parked();
    }

    /// Let a `SLOW` platform answer.
    fn advance_past_slow(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(SLOW * 2);
        cx.run_until_parked();
    }

    /// `/root`: two text files, a previewable image, a folder, and a hidden
    /// dotfile — open in the active pane of a real workspace.
    fn open_root(
        cx: &mut TestAppContext,
        delay: Option<Duration>,
    ) -> (Arc<Calls>, Entity<Workspace>, &mut VisualTestContext) {
        let calls: Arc<Calls> = Arc::default();
        let platform_calls = calls.clone();
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = fs_core::FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({
                    "a.txt": "aaaa",
                    "b.txt": "bbbbbbbb",
                    "photo.png": "pixels",
                    "sub": { "nested.txt": "n" },
                    ".hidden": "h",
                }),
            );
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs,
                spawner.clone(),
                Arc::new(LoggingOpener),
                Arc::new(RecordingPlatform {
                    inner: fs_core::StubPlatform::new(),
                    spawner,
                    delay,
                    calls: platform_calls,
                }),
            );
            crate::settings::init_with_path(cx, PathBuf::from("/config/settings.json"));
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(Theme::dark(), window, cx));
        (calls, workspace, cx)
    }

    fn navigate(workspace: &Entity<Workspace>, path: &str, cx: &mut VisualTestContext) {
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new(path), cx));
        cx.run_until_parked();
    }

    fn active_dir_view(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<DirView> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.active_pane().read(cx).dir_view().clone()
        })
    }

    fn select(workspace: &Entity<Workspace>, paths: &[&str], cx: &mut VisualTestContext) {
        let dir_view = active_dir_view(workspace, cx);
        let owned: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        dir_view.update(cx, |view, cx| {
            let refs: Vec<&Path> = owned.iter().map(PathBuf::as_path).collect();
            view.select_paths(&refs, cx);
        });
        cx.run_until_parked();
    }

    fn panel(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<InfoPanel> {
        workspace.read_with(cx, |workspace, _| workspace.info_panel().clone())
    }

    fn subject(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Subject {
        panel(workspace, cx).read_with(cx, |panel, _| panel.subject().clone())
    }

    /// Let every debounce fire and everything it starts finish.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(LOAD_DEBOUNCE * 4);
        cx.run_until_parked();
    }

    #[gpui::test]
    fn with_no_folder_open_the_panel_is_empty_and_stats_nothing(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, None);
        settle(cx);
        assert_eq!(subject(&workspace, cx), Subject::Nothing);
        assert!(
            calls.started().is_empty(),
            "an empty panel must not stat anything: {:?}",
            calls.started()
        );
    }

    // The M5 bug this milestone kills: the placeholder read "No selection"
    // beside visibly selected rows. Selecting a row must retarget the panel,
    // and the panel must then describe *that* row.
    #[gpui::test]
    fn selecting_a_row_retargets_the_panel_at_it(cx: &mut TestAppContext) {
        let (_calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);

        // Nothing selected: the panel describes the open folder rather than
        // claiming there is nothing to say.
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root")),
                kind: OneKind::OpenFolder,
            }
        );

        select(&workspace, &["/root/a.txt"], cx);
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/a.txt")),
                kind: OneKind::Selected,
            }
        );
        // ...and the loaded values are the selected file's, not the folder's.
        panel(&workspace, cx).read_with(cx, |panel, _| {
            let attrs = panel.attrs().expect("the load landed");
            assert!(attrs.perms.is_some(), "the stub reports a mode");
            assert_eq!(attrs.owner.as_deref(), Some("stub-owner"));
        });

        // Moving the selection retargets it again.
        select(&workspace, &["/root/b.txt"], cx);
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/b.txt")),
                kind: OneKind::Selected,
            }
        );

        // Clearing it goes back to the folder, not to an empty panel.
        active_dir_view(&workspace, cx).update(cx, |view, cx| {
            view.selection_mut().clear();
            cx.notify();
        });
        cx.run_until_parked();
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root")),
                kind: OneKind::OpenFolder,
            }
        );
    }

    // §2 "multi-selection summary": more than one row replaces the
    // single-entry sections with counts and a total, and stats nothing at all
    // (nine files have nine modes; showing the first one's would be a lie).
    #[gpui::test]
    fn a_multi_selection_shows_the_summary_and_loads_nothing(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        settle(cx);
        let before = calls.started().len();

        select(&workspace, &["/root/a.txt", "/root/b.txt", "/root/sub"], cx);
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::Many(SelectionSummary {
                files: 2,
                dirs: 1,
                // a.txt (4) + b.txt (8); the folder's own inode size is not
                // its contents' (fs_core::summarize).
                total_size: 12,
            })
        );
        assert_eq!(
            calls.started().len(),
            before,
            "the summary is computed from the projection, with no stat at all"
        );
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert!(
                panel.attrs().is_none(),
                "a multi-selection has no single entry's attributes to show"
            );
        });
    }

    // The debounce: arrow-keying down a listing retargets the panel once per
    // row, and each retarget must cancel the previous load *before its timer
    // fires* — one stat for the row you stop on, not one per row.
    #[gpui::test]
    fn walking_the_listing_costs_one_load_not_one_per_row(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        settle(cx);
        let before = calls.started().len();
        let loads_before = panel(&workspace, cx).read_with(cx, |panel, _| panel.load_debug().0);

        // Four cursor moves in less than the debounce window.
        for path in ["/root/a.txt", "/root/b.txt", "/root/photo.png", "/root/sub"] {
            select(&workspace, &[path], cx);
            assert_eq!(
                calls.started().len(),
                before,
                "nothing may be stat'd while the selection is still moving"
            );
        }
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert_eq!(
                panel.load_debug().0,
                loads_before,
                "no further load has landed while the selection was moving"
            );
        });

        settle(cx);
        assert_eq!(
            calls.started().len(),
            before + 1,
            "exactly one stat, for the row the walk ended on: {:?}",
            calls.started()
        );
        assert_eq!(
            calls.started().last().unwrap(),
            Path::new("/root/sub"),
            "and it is the last row, not the first"
        );
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert_eq!(panel.load_debug().0, loads_before + 1);
        });
    }

    // Cancel-on-retarget for a load that is already *running*: the timer has
    // fired, `file_attrs` has been entered and is parked, and the selection
    // moves. The abandoned lookup must never reach the panel.
    #[gpui::test]
    fn retargeting_abandons_the_load_it_left_in_flight(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, Some(SLOW));
        navigate(&workspace, "/root", cx);
        // Let the folder's own load finish so it is not the one in flight.
        settle(cx);
        advance_past_slow(cx);
        let loads_before = panel(&workspace, cx).read_with(cx, |panel, _| panel.load_debug().0);

        select(&workspace, &["/root/a.txt"], cx);
        // Past the debounce, but not past the platform's own delay: the
        // lookup is started and parked.
        advance_past_debounce(cx);
        assert!(
            calls.started().contains(&PathBuf::from("/root/a.txt")),
            "the abandoned lookup really did start: {:?}",
            calls.started()
        );
        assert!(
            !calls.finished().contains(&PathBuf::from("/root/a.txt")),
            "and cannot have completed before the clock advances"
        );

        select(&workspace, &["/root/b.txt"], cx);
        advance_past_debounce(cx);
        advance_past_slow(cx);

        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/b.txt")),
                kind: OneKind::Selected,
            }
        );
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert_eq!(
                panel.load_debug().0,
                loads_before + 1,
                "only the surviving subject's load reached the panel"
            );
        });
    }

    // "Nothing blocking on the UI thread": with a platform that answers only
    // when the clock is advanced, the app still parks (a blocking call on the
    // render thread would never let it), the panel still renders, and other
    // UI work still lands — the load is genuinely awaited on the background
    // executor.
    #[gpui::test]
    fn a_slow_platform_never_blocks_the_ui_thread(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, Some(SLOW));
        navigate(&workspace, "/root", cx);
        select(&workspace, &["/root/a.txt"], cx);
        // If `file_attrs` ran on the UI thread this would never return.
        advance_past_debounce(cx);

        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert!(
                panel.attrs().is_none(),
                "the lookup is still parked on its timer"
            );
        });
        // The UI is live while it is: an unrelated command still works and the
        // panel still paints.
        workspace.update(cx, |workspace, cx| workspace.toggle_info_panel(cx));
        workspace.update(cx, |workspace, cx| workspace.toggle_info_panel(cx));
        cx.run_until_parked();

        advance_past_slow(cx);
        assert!(
            calls.finished().contains(&PathBuf::from("/root/a.txt")),
            "and the lookup completes once time passes: {:?}",
            calls.finished()
        );
    }

    // The preview is gated by `fs_core::is_previewable`: a `.png` gets one, a
    // folder never does, and a
    // `.txt`-shaped non-image extension outside the allowlist gets none.
    #[gpui::test]
    fn only_a_previewable_subject_asks_for_a_preview(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        settle(cx);
        assert!(
            calls.thumbnails().is_empty(),
            "a folder has no content to preview: {:?}",
            calls.thumbnails()
        );

        select(&workspace, &["/root/photo.png"], cx);
        settle(cx);
        assert_eq!(calls.thumbnails(), vec![PathBuf::from("/root/photo.png")]);
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert!(panel.load_debug().1, "the preview decoded into an image");
        });

        // A plain-text file is in the allowlist too, so pick the folder to
        // prove the negative: retargeting drops the image.
        select(&workspace, &["/root/sub"], cx);
        settle(cx);
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert!(
                !panel.load_debug().1,
                "the folder's subject has no preview of its own"
            );
        });
        assert_eq!(
            calls.thumbnails(),
            vec![PathBuf::from("/root/photo.png")],
            "and no preview was asked for the folder"
        );
    }

    // Idle notifies from the `DirView` (a scroll, an arriving thumbnail, the
    // scrollbar's fade) must not restart the debounce — a preview slower than
    // the repaint cadence would otherwise never arrive.
    #[gpui::test]
    fn idle_notifies_neither_restart_the_debounce_nor_reload(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        select(&workspace, &["/root/a.txt"], cx);
        settle(cx);
        let after_first = calls.started().len();

        let dir_view = active_dir_view(&workspace, cx);
        for _ in 0..10 {
            dir_view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
        settle(cx);
        assert_eq!(
            calls.started().len(),
            after_first,
            "ten repaints of an unchanged selection must cost no stats"
        );
    }

    // ...and they must not cost a *projection* either. `subject_of` is
    // O(listing) and allocates a row per entry, and `follow` runs on every
    // notify from the pane's `DirView` — a scroll frame, an arriving thumbnail,
    // a marquee autoscroll tick. The witness comparison is what keeps that off
    // the UI thread, so it has to happen before the subject is derived.
    #[gpui::test]
    fn an_idle_follow_does_not_build_the_projection(cx: &mut TestAppContext) {
        let (_calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        select(&workspace, &["/root/a.txt"], cx);
        settle(cx);

        let dir_view = active_dir_view(&workspace, cx);
        let panel = panel(&workspace, cx);
        let before = crate::dir_view::projections_built();
        panel.update(cx, |panel, cx| {
            for _ in 0..10 {
                panel.follow(&dir_view, cx);
            }
        });
        assert_eq!(
            crate::dir_view::projections_built(),
            before,
            "ten follows of an unchanged view must build no projection at all"
        );
    }

    // A folder that is being written to re-lists every `pane::WATCH_LATENCY`
    // (100 ms), which is *shorter* than `LOAD_DEBOUNCE`. Each re-list is a new
    // snapshot, so the panel re-reads its subject — but it must keep the values
    // it already has painted while it does, or a file being downloaded into the
    // open folder leaves the panel permanently at em dashes.
    #[gpui::test]
    fn repeated_relistings_keep_the_values_painted(cx: &mut TestAppContext) {
        let (_calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        select(&workspace, &["/root/photo.png"], cx);
        settle(cx);
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert!(panel.attrs().is_some(), "the first load landed");
            assert!(panel.load_debug().1, "and so did its preview");
        });

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        for tick in 0..5 {
            pane.update(cx, |pane, cx| pane.refresh(cx));
            cx.run_until_parked();
            // Deliberately *less* than the debounce: the next re-list arrives
            // before the load it triggered could ever have landed.
            cx.executor().advance_clock(LOAD_DEBOUNCE / 2);
            cx.run_until_parked();
            panel(&workspace, cx).read_with(cx, |panel, _| {
                assert!(
                    panel.attrs().is_some(),
                    "re-list {tick} blanked the attributes"
                );
                assert!(panel.load_debug().1, "re-list {tick} dropped the preview");
            });
        }

        // And once the churn stops, the fresh values do land.
        settle(cx);
        panel(&workspace, cx).read_with(cx, |panel, _| {
            assert!(panel.attrs().is_some());
            assert!(panel.load_debug().1);
        });
    }

    // A hidden panel describes nothing and stats nothing; showing it again
    // re-describes the current selection rather than a stale one.
    #[gpui::test]
    fn a_hidden_panel_stops_loading_and_a_shown_one_catches_up(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        select(&workspace, &["/root/a.txt"], cx);
        settle(cx);
        let while_shown = calls.started().len();

        workspace.update(cx, |workspace, cx| workspace.toggle_info_panel(cx));
        cx.run_until_parked();
        assert!(!workspace.read_with(cx, |workspace, _| workspace.show_info_panel()));
        assert_eq!(subject(&workspace, cx), Subject::Nothing);

        select(&workspace, &["/root/b.txt"], cx);
        settle(cx);
        assert_eq!(
            calls.started().len(),
            while_shown,
            "a hidden panel must not stat the selection it cannot show"
        );

        workspace.update(cx, |workspace, cx| workspace.toggle_info_panel(cx));
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/b.txt")),
                kind: OneKind::Selected,
            },
            "showing it again describes the current selection, not the old one"
        );
    }

    // The panel is workspace-level but follows the **active** pane (the
    // AS_BUILT gap M5 owns): splitting retargets it at the new pane, and
    // focusing back retargets it again.
    #[gpui::test]
    fn the_panel_follows_the_active_pane_across_a_split(cx: &mut TestAppContext) {
        let (_calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        select(&workspace, &["/root/a.txt"], cx);
        settle(cx);

        // Split: the new pane is active, showing the same directory with
        // nothing selected, so the panel describes the folder.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.toggle_split_pane(window, cx));
        });
        cx.run_until_parked();
        settle(cx);
        assert!(workspace.read_with(cx, |workspace, _| workspace.is_split()));
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root")),
                kind: OneKind::OpenFolder,
            }
        );

        // Select in the new (active) pane: the panel follows it, not the
        // first pane it was showing a moment ago.
        select(&workspace, &["/root/b.txt"], cx);
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/b.txt")),
                kind: OneKind::Selected,
            }
        );

        // Focus the first pane: `PaneEvent::FocusIn` retargets the panel at
        // *its* selection, which is still a.txt.
        let first: Entity<Pane> =
            workspace.read_with(cx, |workspace, _| workspace.panes()[0].clone());
        cx.update(|window, cx| {
            window.activate_window();
            let handle = first.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();
        settle(cx);
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.active_pane_ix()),
            0
        );
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/a.txt")),
                kind: OneKind::Selected,
            }
        );

        // Collapsing keeps the survivor's selection described, and drops the
        // closed pane's observation with the pane.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.toggle_split_pane(window, cx));
        });
        cx.run_until_parked();
        settle(cx);
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/a.txt")),
                kind: OneKind::Selected,
            }
        );
    }

    // §0 dispatch guard on the real entity: `cmd-shift-i` reaches the
    // workspace handler with focus on the workspace root, and the panel
    // leaves the pane strip's own layout state alone.
    #[gpui::test]
    fn cmd_shift_i_toggles_the_panel_without_disturbing_the_strip(cx: &mut TestAppContext) {
        let (_calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.toggle_split_pane(window, cx));
        });
        cx.run_until_parked();
        workspace.update(cx, |workspace, cx| {
            workspace.set_first_pane_width(320.0, cx);
        });
        let widths = workspace.read_with(cx, |workspace, _| {
            (
                workspace.first_pane_width(),
                workspace.sidebar_width(),
                workspace.info_panel_width(),
            )
        });

        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-shift-i");
        cx.run_until_parked();
        assert!(
            !workspace.read_with(cx, |workspace, _| workspace.show_info_panel()),
            "cmd-shift-i hides the panel"
        );
        cx.simulate_keystrokes("cmd-shift-i");
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, _| workspace.show_info_panel()));

        assert_eq!(
            workspace.read_with(cx, |workspace, _| {
                (
                    workspace.first_pane_width(),
                    workspace.sidebar_width(),
                    workspace.info_panel_width(),
                )
            }),
            widths,
            "hiding and showing the panel must not disturb the pane strip"
        );
    }

    // A file rewritten while it is selected must not keep showing the old
    // stat: the watcher patch replaces the pane's snapshot, which is what the
    // witness notices.
    #[gpui::test]
    fn a_relisting_re_reads_the_selected_entrys_attributes(cx: &mut TestAppContext) {
        let (calls, workspace, cx) = open_root(cx, None);
        navigate(&workspace, "/root", cx);
        select(&workspace, &["/root/a.txt"], cx);
        settle(cx);
        let before = calls.started().len();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.refresh(cx));
        cx.run_until_parked();
        settle(cx);

        assert_eq!(
            calls.started().len(),
            before + 1,
            "a fresh listing snapshot re-reads the subject: {:?}",
            calls.started()
        );
        assert_eq!(
            subject(&workspace, cx),
            Subject::One {
                path: Arc::from(Path::new("/root/a.txt")),
                kind: OneKind::Selected,
            }
        );
    }
}
