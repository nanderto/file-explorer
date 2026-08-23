//! Drag & drop (ARCHITECTURE.md §8 "Drag & drop").
//!
//! The §8 row, implemented as specified: a [`DraggedEntries`] payload built
//! at render time, a single `Option<DropState>` per pane (a **field** of
//! [`DirView`], like `rename` and `marquee` — never its own entity) with
//! out-of-bounds self-clear, a 500 ms spring-load task on folders running on
//! [`fs_core::Spawner::timer`], and a modifier check that flips the move/copy
//! cursor. Every target also accepts [`ExternalPaths`] (Finder → us), and the
//! row that starts a drag pairs its `on_drag` with `external_drag_payload`
//! (us → Finder).
//!
//! **What is dragged** (Explorer behavior): grabbing a row that is part of the
//! selection drags the whole selection; grabbing one that is not drags only
//! that row. The payload is path-keyed like every other identity in the app
//! (invariant #2) and carries the *root-most* selected paths — dragging a
//! folder and something inside it must not move the child twice.
//!
//! **Where it lands.** A drop on a folder row goes into that folder; a drop on
//! the pane background goes into the pane's current directory; a drop on a
//! *file* row injected by in-place expansion goes into **that row's** folder,
//! not the pane's directory (the row the user aimed at is what they mean). The
//! destination is turned into a `FileOp` by [`plan_drop`] and submitted to
//! `FsContext.queue` — this module never touches the disk.
//!
//! **No-ops are refused before submission, not by the engine.** fs-core
//! already guards the dangerous shapes (`queue.rs`: a move/copy whose
//! destination is inside a source fails cleanly; a move into the source's own
//! folder is skipped), and those guards stay the backstop. [`plan_drop`]
//! declines to submit them at all, so dropping a folder on itself produces
//! silence rather than a failure toast.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs_core::{EntryId, FileOp};
use gpui::{
    App, Bounds, Context, CursorStyle, Div, DragMoveEvent, Entity, EntityId, ExternalDragPayload,
    ExternalPaths, FileDragPaths, IntoElement, Modifiers, MouseButton, MouseUpEvent, Pixels, Point,
    Render, SharedString, Stateful, Task, Window, div, prelude::*, px,
};

use crate::app_state::FsContext;
use crate::dir_view::{DirView, DirViewEvent, ProjectedRow};
use crate::marquee::{ContentPoint, scroll_y};
use crate::theme::Theme;

/// §8: hovering a folder for this long during a drag navigates into it.
/// Runs on [`fs_core::Spawner::timer`], so tests drive it with fake time.
pub const SPRING_LOAD_DELAY: Duration = Duration::from_millis(500);

/// Drop-highlight alphas applied to the theme accent — the app crate never
/// names a color. The row tint is stronger than the selection tint
/// (`details_list::SELECTION_ALPHA`) so a drop target reads as the hotter of
/// the two when a selected folder is also the target.
const DROP_ROW_ALPHA: f32 = 0.55;
const DROP_RING_ALPHA: f32 = 0.8;
/// Ring thickness for the background (whole-directory) drop target.
const DROP_RING_WIDTH: f32 = 2.0;
/// Sidebar Favorites drop-zone tint (drag-to-add / reorder).
pub(crate) const FAVORITES_DROP_ALPHA: f32 = 0.18;

/// The copy modifier for a drag: **alt** (⌥ on macOS, `Alt` elsewhere).
///
/// It is the one modifier free of conflicts in this app: `platform` (⌘) is the
/// multi-select toggle on row clicks, `shift` is range-select, and `control` is
/// macOS's context-menu chord. ⌥-drag is also the native copy-drag gesture,
/// and sits where Explorer's ctrl-drag does on a PC keyboard.
pub fn copy_modifier_held(modifiers: Modifiers) -> bool {
    modifiers.alt
}

/// The modifier that forces a **move** where the default would be a copy:
/// **shift**, exactly as in Explorer (and Finder). `shift` means range-select
/// on a *click*, but a drag never range-selects, so the two cannot collide.
pub fn move_modifier_held(modifiers: Modifiers) -> bool {
    modifiers.shift
}

/// Whether a drag with these modifiers, from `sources` into `dest_dir`, should
/// **copy** — the single place that decision is made, so the highlight
/// predicate, the cursor and the submitted op can never disagree.
///
/// Explorer's rule, which plan §3 makes binding ("Windows File Explorer
/// behavior, not Finder"): a drag **within one volume moves**, a drag **across
/// volumes copies** (dragging off a USB stick must not empty it), ⌥ forces a
/// copy either way, and ⇧ forces a move. `vfs.volume_key` is the same
/// derivation the job queue lanes by, so "same volume" means one thing in the
/// app.
pub fn drop_copies(
    vfs: &dyn fs_core::Vfs,
    dest_dir: &Path,
    sources: &[Arc<Path>],
    modifiers: Modifiers,
) -> bool {
    if move_modifier_held(modifiers) {
        return false;
    }
    if copy_modifier_held(modifiers) {
        return true;
    }
    let dest = vfs.volume_key(dest_dir);
    // Any source from another volume makes the whole drag a copy: one gesture
    // cannot be half a move.
    sources
        .iter()
        .any(|source| vfs.volume_key(source.as_ref()) != dest)
}

// ----------------------------------------------------------------------
// Payloads
// ----------------------------------------------------------------------

/// The §8 drag payload for file entries: what was grabbed, what is coming
/// along, and which pane it came from (M4 uses the last for cross-pane drags;
/// today it makes the source identifiable in tests and logs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedEntries {
    pub grabbed: Arc<Path>,
    pub selection: Arc<[Arc<Path>]>,
    pub source_pane: EntityId,
}

impl DraggedEntries {
    /// The Explorer rule: a grabbed row that is part of the selection drags
    /// the whole selection; one that is not drags only itself — and neither
    /// case alters the selection, because a press is not a click.
    ///
    /// `selection` is the frame's **shared, root-most** selected paths
    /// (`DirView::drag_payload` hands it over): a payload is built for every
    /// drag-capable row on every frame, so re-deriving it per row would make a
    /// large selection quadratic in the number of painted rows. Sharing the
    /// `Arc` makes the selected case a refcount bump.
    pub fn for_grab(
        grabbed: Arc<Path>,
        grabbed_is_selected: bool,
        selection: &Arc<[Arc<Path>]>,
        source_pane: EntityId,
    ) -> Self {
        let selection = if grabbed_is_selected {
            selection.clone()
        } else {
            Arc::from(vec![grabbed.clone()])
        };
        Self {
            grabbed,
            selection,
            source_pane,
        }
    }

    pub fn paths(&self) -> &[Arc<Path>] {
        &self.selection
    }

    /// The drag ghost's caption: the grabbed name, or a count for a multiple
    /// selection (Explorer shows the same two shapes).
    pub fn label(&self) -> SharedString {
        if self.selection.len() > 1 {
            SharedString::from(format!("{} items", self.selection.len()))
        } else {
            SharedString::from(
                self.grabbed
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.grabbed.display().to_string()),
            )
        }
    }
}

/// A sidebar Favorites row being dragged to a new position (the M2-deferred
/// reordering gap). Separate from [`DraggedEntries`] on purpose: the two mean
/// different things at a drop site — this one reorders a list, that one moves
/// files — and gpui dispatches drops by payload type, so the distinction is
/// what keeps a favorite from being "pasted" into a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraggedFavorite {
    pub path: PathBuf,
}

/// The drag preview rendered under the cursor. Colors from the [`Theme`].
pub struct DragGhost {
    label: SharedString,
    theme: Theme,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(self.theme.accent)
            .bg(self.theme.panel)
            .text_size(px(12.0))
            .text_color(self.theme.text)
            .child(self.label.clone())
    }
}

/// Build a drag ghost (called from `on_drag` constructors).
pub(crate) fn ghost(label: SharedString, theme: Theme, cx: &mut App) -> Entity<DragGhost> {
    cx.new(|_| DragGhost { label, theme })
}

/// The outbound (us → Finder) payload for `external_drag_payload`: real paths
/// paired with whether each is a directory, so the platform never has to stat
/// them mid-gesture. `None` for an empty drag, which offers the platform
/// nothing.
pub fn external_payload(entries: &[(PathBuf, bool)]) -> Option<ExternalDragPayload> {
    if entries.is_empty() {
        return None;
    }
    Some(ExternalDragPayload::Files(FileDragPaths::new(
        entries.iter().cloned(),
    )))
}

// ----------------------------------------------------------------------
// Drop targets (pure)
// ----------------------------------------------------------------------

/// Where a drop would land, per §8: a specific folder, or the pane's own
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropTarget {
    Folder(Arc<Path>),
    Background,
}

impl DropTarget {
    /// The destination directory this target resolves to. `Background` needs
    /// the pane's current directory, which is `None` before anything is open.
    pub fn dest_dir<'a>(&'a self, current_dir: Option<&'a Path>) -> Option<&'a Path> {
        match self {
            DropTarget::Folder(path) => Some(path),
            DropTarget::Background => current_dir,
        }
    }
}

/// Which projected row a content-space `y` lands on, or `None` for the empty
/// space past the last row. Arithmetic against the uniform row band, like the
/// marquee's [`crate::marquee::rows_in_rect`] — `uniform_list` virtualizes
/// rows away, so scanning painted elements would miss the scrolled ones.
pub fn row_at(content_y: f32, row_height: f32, row_count: usize) -> Option<usize> {
    if row_count == 0 || row_height <= 0.0 || content_y < 0.0 {
        return None;
    }
    let ix = (content_y / row_height).floor() as usize;
    (ix < row_count).then_some(ix)
}

/// The drop target for the row under the pointer:
///
/// * a folder row → into that folder;
/// * a **file** row injected by in-place expansion (`depth > 0`) → into that
///   row's own parent folder, which is the folder the user is pointing at;
/// * a top-level file row, or empty space → the pane's directory.
pub fn target_for_row(row: Option<&ProjectedRow>) -> DropTarget {
    match row {
        Some(row) if row.entry.is_dir_like() => DropTarget::Folder(row.entry.path.clone()),
        Some(row) if row.depth > 0 => match row.entry.path.parent() {
            Some(parent) => DropTarget::Folder(Arc::from(parent)),
            None => DropTarget::Background,
        },
        _ => DropTarget::Background,
    }
}

/// The op a drop would submit, or `None` when the drop is a no-op or a
/// mistake — which is also the predicate the drop highlight uses, so the UI
/// never invites a drop that would do nothing:
///
/// * a destination inside (or equal to) a source is refused outright (fs-core
///   fails these; refusing here means no failure toast for a slip of the
///   mouse);
/// * for a **move**, sources already living in the destination are dropped —
///   moving into your own folder is nothing — and a drag made up entirely of
///   those submits nothing;
/// * for a **copy**, same-folder sources are kept: that is a deliberate
///   duplicate, and op planning gives it a keep-both name.
pub fn plan_drop(dest_dir: &Path, sources: &[Arc<Path>], copy: bool) -> Option<FileOp> {
    if sources
        .iter()
        .any(|source| dest_dir.starts_with(source.as_ref()))
    {
        return None;
    }
    let kept: Vec<PathBuf> = sources
        .iter()
        .filter(|source| copy || source.parent() != Some(dest_dir))
        .map(|source| source.to_path_buf())
        .collect();
    if kept.is_empty() {
        return None;
    }
    Some(if copy {
        FileOp::Copy {
            sources: kept,
            dest_dir: dest_dir.to_path_buf(),
        }
    } else {
        FileOp::Move {
            sources: kept,
            dest_dir: dest_dir.to_path_buf(),
        }
    })
}

// ----------------------------------------------------------------------
// The per-pane drop state (a DirView field, like rename/marquee)
// ----------------------------------------------------------------------

/// The armed drop target. Lives at `DirView.drop`; dropping it (on release,
/// on leaving the list, or with the view) cancels the spring-load task.
pub(crate) struct DropState {
    target: DropTarget,
    /// Whether the copy modifier was held at the last pointer move — drives
    /// the cursor and which op the drop plans.
    copy: bool,
    /// Whether a drop here would actually do something ([`plan_drop`]).
    /// Highlights only paint for a valid target.
    valid: bool,
    /// §8: the 500 ms spring-load timer for a folder target. Exactly one
    /// slot; re-armed when the hovered folder changes, dropped when the
    /// pointer leaves it.
    _spring: Option<Task<()>>,
}

impl DirView {
    /// The §8 payload for a drag grabbed on `grabbed`, over the frame's shared
    /// root-most selection (rebuilt once per `render`, not once per row).
    pub(crate) fn drag_payload(&self, grabbed: Arc<Path>) -> DraggedEntries {
        let selected = self.selection().is_selected(&EntryId(grabbed.clone()));
        DraggedEntries::for_grab(grabbed, selected, self.drag_selection(), self.pane_id())
    }

    /// The outbound (us → Finder) entries for a live drag: its paths, each
    /// paired with whether it is a folder. Resolved **lazily**, when gpui
    /// promotes the drag to a platform session, rather than per row per frame
    /// — and in one pass over the projection, so a huge selection stays linear.
    pub(crate) fn external_drag_entries(&self, dragged: &DraggedEntries) -> Vec<(PathBuf, bool)> {
        let dirs: std::collections::HashSet<&Path> = self
            .flat_rows()
            .iter()
            .filter(|row| row.entry.is_dir_like())
            .map(|row| row.entry.path.as_ref())
            .collect();
        dragged
            .paths()
            .iter()
            .map(|path| (path.to_path_buf(), dirs.contains(path.as_ref())))
            .collect()
    }

    /// The armed drop target, when a drop there would do something — the
    /// render path's question (`details_list` tints the folder row,
    /// [`background_highlight`] rings the list).
    pub(crate) fn active_drop_target(&self, cx: &App) -> Option<&DropTarget> {
        // A drag can end without any event this element sees: a platform file
        // drag that leaves the window takes gpui's `active_drag` with it and
        // dispatches no mouse event at all. Gating on a live drag is what
        // stops a stale highlight from outliving the gesture.
        if !cx.has_active_drag() {
            return None;
        }
        self.drop
            .as_ref()
            .filter(|state| state.valid)
            .map(|state| &state.target)
    }

    /// Every pointer move while an internal file drag is live (fires inside
    /// the list or out of it, which is what makes the out-of-bounds clear
    /// possible).
    pub(crate) fn drag_entries_over(
        &mut self,
        event: &DragMoveEvent<DraggedEntries>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dragged = event.drag(cx).clone();
        self.hover_drop(
            event.event.position,
            event.bounds,
            dragged.paths(),
            Some(event.event.modifiers),
            window,
            cx,
        );
    }

    /// The same, for a drag that came from another application (Finder). The
    /// platform strips modifiers from file drops, so an external drop is
    /// always a copy — never a move that would delete another app's files.
    pub(crate) fn external_paths_over(
        &mut self,
        event: &DragMoveEvent<ExternalPaths>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sources = external_sources(event.drag(cx));
        // `None` modifiers: the platform strips them from file drops, so an
        // external drag is unconditionally a copy.
        self.hover_drop(
            event.event.position,
            event.bounds,
            &sources,
            None,
            window,
            cx,
        );
    }

    /// Arm (or clear) the drop target for a pointer at `pointer`, over a list
    /// whose surface is `viewport`. `modifiers` is `None` for an external
    /// (platform) drag, which carries none and always copies.
    fn hover_drop(
        &mut self,
        pointer: Point<Pixels>,
        viewport: Bounds<Pixels>,
        sources: &[Arc<Path>],
        modifiers: Option<Modifiers>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // §8 "out-of-bounds self-clear": the pointer left this pane's list, so
        // this pane no longer has a drop target — whatever it is over now
        // (another pane, the sidebar, nothing) owns the gesture.
        if !viewport.contains(&pointer) {
            self.clear_drop_target(cx);
            return;
        }
        // Mode-aware hit test (`DirView::index_at_content`): rows in the
        // details list, tiles in the icon grid — the same function the
        // marquee's empty-space rule uses, so the two gestures can never
        // disagree about what the pointer is over.
        let content = ContentPoint::from_window(pointer, viewport, scroll_y(self));
        let row = self
            .index_at_content(content, cx)
            .and_then(|ix| self.flat_rows().get(ix))
            // A §4c new-entry phantom row is not a folder anyone can drop into —
            // it does not exist yet. A drop aimed at it lands in the pane's own
            // directory, which is where it would have gone a moment earlier.
            .filter(|row| !self.is_new_entry_row(row))
            .cloned();
        let target = target_for_row(row.as_ref());

        let current = self.current_dir(cx);
        let dest = target.dest_dir(current.as_deref()).map(Path::to_path_buf);
        // One decision, made once (see [`drop_copies`]): the cursor, the
        // highlight predicate and the op this drop will submit all read it.
        let copy = match (&dest, modifiers) {
            (_, None) => true,
            (Some(dest), Some(modifiers)) => {
                drop_copies(&*FsContext::global(cx).vfs, dest, sources, modifiers)
            }
            (None, _) => false,
        };
        let valid = dest
            .as_deref()
            .and_then(|dest| plan_drop(dest, sources, copy))
            .is_some();
        // §8 "modifier check flips move/copy cursor" — plus a refusal cursor
        // where the drop would do nothing.
        cx.set_active_drag_cursor_style(
            if !valid {
                CursorStyle::OperationNotAllowed
            } else if copy {
                CursorStyle::DragCopy
            } else {
                CursorStyle::Arrow
            },
            window,
        );

        // Same target: only the modifier/validity can have changed, and the
        // spring-load timer must keep running rather than restart. Unless it
        // is *spent* — a state left behind by a gesture that ended with no
        // event we saw (a platform drag leaving the window takes gpui's
        // `active_drag` and dispatches nothing) would otherwise be adopted by
        // the next drag and never re-arm, killing spring-load for that folder
        // for the rest of the session.
        let arm_spring = matches!(target, DropTarget::Folder(_)) && valid;
        if let Some(state) = self.drop.as_mut()
            && state.target == target
        {
            let changed = state.copy != copy || state.valid != valid;
            state.copy = copy;
            state.valid = valid;
            let rearm = arm_spring && state._spring.is_none();
            let disarm = !arm_spring && state._spring.is_some();
            if disarm {
                state._spring = None;
            }
            if rearm {
                let DropTarget::Folder(path) = &target else {
                    unreachable!("arm_spring implies a folder target");
                };
                let task = self.spring_load_task(path.clone(), cx);
                if let Some(state) = self.drop.as_mut() {
                    state._spring = Some(task);
                }
            }
            if changed || rearm || disarm {
                cx.notify();
            }
            return;
        }
        // Only a target a drop would actually *accept* springs open: arming the
        // timer for a refused target (dragging a folder onto itself, say) would
        // navigate into it 500 ms later while the cursor said "not allowed",
        // losing the gesture.
        let spring = match &target {
            DropTarget::Folder(path) if arm_spring => Some(self.spring_load_task(path.clone(), cx)),
            _ => None,
        };
        self.drop = Some(DropState {
            target,
            copy,
            valid,
            _spring: spring,
        });
        cx.notify();
    }

    /// Drop the armed target (release, leaving the list, or a cancelled drag).
    pub(crate) fn clear_drop_target(&mut self, cx: &mut Context<Self>) {
        if self.drop.take().is_some() {
            cx.notify();
        }
    }

    /// §8 spring-load: after [`SPRING_LOAD_DELAY`] over the same folder,
    /// navigate into it. Dropping the task (a different target, a release, or
    /// the view going away) cancels it.
    fn spring_load_task(&self, path: Arc<Path>, cx: &mut Context<Self>) -> Task<()> {
        let spawner = FsContext::global(cx).spawner.clone();
        cx.spawn(async move |this, cx| {
            spawner.timer(SPRING_LOAD_DELAY).await;
            this.update(cx, |this, cx| this.spring_load(&path, cx)).ok();
        })
    }

    fn spring_load(&mut self, path: &Arc<Path>, cx: &mut Context<Self>) {
        // A gesture that ended without an event we saw (see
        // `active_drop_target`) must not spring anything open behind it.
        if !cx.has_active_drag() {
            // ...and the timer is now **spent**: forget it, so the next drag
            // over this same target arms a fresh one instead of adopting a
            // state that can never fire again (a finished `Task` is still
            // `Some`, so only clearing it here can tell the two apart).
            if let Some(state) = self.drop.as_mut() {
                state._spring = None;
            }
            return;
        }
        // Still the armed folder, and still a drop we would accept: a stale
        // timer must never navigate.
        let still_hovered = self.drop.as_ref().is_some_and(|state| {
            state.valid && matches!(&state.target, DropTarget::Folder(hovered) if hovered == path)
        });
        if !still_hovered {
            return;
        }
        // Events up (§2): the pane owns navigation and history.
        cx.emit(DirViewEvent::NavigateTo(path.to_path_buf()));
        // The folder we sprang open *is* the directory now, so a release here
        // lands in it (Explorer behavior) and the timer is spent.
        if let Some(state) = self.drop.as_mut() {
            state.target = DropTarget::Background;
            state._spring = None;
        }
        cx.notify();
    }

    /// Release over this list with an internal drag: submit the op.
    pub(crate) fn drop_entries(
        &mut self,
        dragged: &DraggedEntries,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sources = dragged.paths().to_vec();
        // **The armed state decides, not the modifiers at mouse-up.** The
        // highlight and the cursor the user was looking at came from the last
        // drag-move; re-deriving the operation here would let releasing ⌥ a
        // frame early turn an advertised, valid-looking copy into a move that
        // `plan_drop` then refuses — a lit target that silently does nothing.
        // The fallback covers a drag that entered exactly on the release.
        let copy = match self.drop.as_ref() {
            Some(state) => state.copy,
            None => {
                let sources_ref: &[Arc<Path>] = &sources;
                self.current_dir(cx).is_some_and(|dest| {
                    drop_copies(
                        &*FsContext::global(cx).vfs,
                        &dest,
                        sources_ref,
                        window.modifiers(),
                    )
                })
            }
        };
        self.submit_drop(&sources, copy, cx);
    }

    /// Release over this list with paths from another application: a copy.
    pub(crate) fn drop_external_paths(
        &mut self,
        paths: &ExternalPaths,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sources = external_sources(paths);
        self.submit_drop(&sources, true, cx);
    }

    /// Turn the armed target into a queued [`FileOp`] (§4b: the UI thread
    /// never touches the disk — the queue does the work in the background and
    /// the destination directory's watcher paints the result).
    fn submit_drop(&mut self, sources: &[Arc<Path>], copy: bool, cx: &mut Context<Self>) {
        // Taking it clears the highlight and cancels any spring-load.
        let target = self
            .drop
            .take()
            .map(|state| state.target)
            // A drop with no armed target (an external drag that entered
            // exactly on the release) still means "into this directory".
            .unwrap_or(DropTarget::Background);
        cx.notify();
        let current = self.current_dir(cx);
        let Some(dest) = target.dest_dir(current.as_deref()) else {
            return;
        };
        let Some(op) = plan_drop(dest, sources, copy) else {
            return;
        };
        FsContext::global(cx).queue.submit(op);
    }
}

fn external_sources(paths: &ExternalPaths) -> Vec<Arc<Path>> {
    paths
        .paths()
        .iter()
        .map(|path| Arc::from(path.as_path()))
        .collect()
}

// ----------------------------------------------------------------------
// Rendering
// ----------------------------------------------------------------------

/// Hang the pane's drop machinery off the details list's background surface
/// (the same element the marquee's drag lives on — one element, no extra
/// layout node): both drag types' move tracking, both drops, the release-
/// elsewhere clear, and the background highlight.
pub(crate) fn with_drop_handlers(
    surface: Stateful<Div>,
    view: &DirView,
    cx: &mut Context<DirView>,
) -> Stateful<Div> {
    let highlight = background_highlight(view, cx);
    surface
        .on_drag_move(cx.listener(DirView::drag_entries_over))
        .on_drag_move(cx.listener(DirView::external_paths_over))
        .on_drop(cx.listener(|this, dragged: &DraggedEntries, window, cx| {
            this.drop_entries(dragged, window, cx)
        }))
        .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
            this.drop_external_paths(paths, window, cx)
        }))
        // A release anywhere else ends the gesture for us, highlight included.
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this, _: &MouseUpEvent, _, cx| this.clear_drop_target(cx)),
        )
        .children(highlight)
}

/// The whole-directory drop highlight: an accent ring inside the list, drawn
/// absolutely so arming it never moves a row.
fn background_highlight(view: &DirView, cx: &App) -> Option<Div> {
    match view.active_drop_target(cx)? {
        DropTarget::Background => {
            let theme = view.theme();
            Some(
                div()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .size_full()
                    .border(px(DROP_RING_WIDTH))
                    .border_color(theme.accent.opacity(DROP_RING_ALPHA)),
            )
        }
        DropTarget::Folder(_) => None,
    }
}

/// Whether this row is the armed folder drop target (`details_list` tints it).
pub(crate) fn row_is_drop_target(view: &DirView, path: &Path, cx: &App) -> bool {
    matches!(
        view.active_drop_target(cx),
        Some(DropTarget::Folder(target)) if target.as_ref() == path
    )
}

/// The folder-row drop tint (no color literals — the theme accent).
pub(crate) fn drop_row_color(theme: &Theme) -> gpui::Hsla {
    theme.accent.opacity(DROP_ROW_ALPHA)
}

#[cfg(test)]
mod tests {
    //! §9 drag & drop rows. The payload rule and the destination arithmetic
    //! first, headlessly; then the gesture itself through real simulated
    //! mouse input on a laid-out window, asserting against the `FakeVfs` tree
    //! (the op really runs — the queue is wired end to end in these tests).

    use super::*;
    use crate::views::details_list::ROW_HEIGHT;

    use std::sync::Arc;

    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::marquee::list_viewport;
    use crate::pane::Pane;
    use crate::selection::SelectionModel;
    use crate::theme::Theme;
    use fs_core::{FakeVfs, FileEntry, Spawner};
    use gpui::{Entity, FileDropEvent, Modifiers, TestAppContext, VisualTestContext};
    use serde_json::json;

    const H: f32 = ROW_HEIGHT;

    fn path(p: &str) -> Arc<Path> {
        Arc::from(Path::new(p))
    }

    // ---------------- the payload rule ----------------

    fn pane_id(cx: &mut TestAppContext) -> EntityId {
        cx.update(|cx| cx.new(|_| Marker).entity_id())
    }

    struct Marker;

    #[gpui::test]
    fn a_grabbed_row_inside_the_selection_drags_all_of_it(cx: &mut TestAppContext) {
        let id = pane_id(cx);
        let mut selection = SelectionModel::default();
        selection.select_only(EntryId(path("/d/a.txt")));
        selection.toggle(EntryId(path("/d/b.txt")));
        let shared: Arc<[Arc<Path>]> = Arc::from(selection.selected_rootmost());

        // Grabbing a selected row carries the whole selection — as the very
        // same allocation the frame built, not a rebuilt copy.
        let dragged = DraggedEntries::for_grab(path("/d/a.txt"), true, &shared, id);
        assert_eq!(
            dragged.paths().to_vec(),
            vec![path("/d/a.txt"), path("/d/b.txt")]
        );
        assert!(
            Arc::ptr_eq(&dragged.selection, &shared),
            "the selected case must be a refcount bump, not a rescan"
        );
        assert_eq!(dragged.grabbed, path("/d/a.txt"));
        assert_eq!(dragged.source_pane, id);
        assert_eq!(dragged.label(), SharedString::from("2 items"));

        // Grabbing an unselected row carries only that row.
        let dragged = DraggedEntries::for_grab(path("/d/c.txt"), false, &shared, id);
        assert_eq!(dragged.paths().to_vec(), vec![path("/d/c.txt")]);
        assert_eq!(dragged.label(), SharedString::from("c.txt"));
        assert_eq!(selection.len(), 2, "the payload never mutates selection");
    }

    #[gpui::test]
    fn a_dragged_selection_carries_only_its_root_most_paths(cx: &mut TestAppContext) {
        // In-place expansion makes "a folder and something inside it" easy to
        // select; the payload must not carry the child too (it would be moved
        // twice). The root-most reduction itself is `SelectionModel`'s, and is
        // tested there — this pins that the payload uses it.
        let id = pane_id(cx);
        let mut selection = SelectionModel::default();
        selection.select_only(EntryId(path("/d/folder")));
        selection.toggle(EntryId(path("/d/folder/inner.txt")));
        let shared: Arc<[Arc<Path>]> = Arc::from(selection.selected_rootmost());
        let dragged = DraggedEntries::for_grab(path("/d/folder"), true, &shared, id);
        assert_eq!(dragged.paths().to_vec(), vec![path("/d/folder")]);
    }

    // ---------------- destination arithmetic ----------------

    fn row(path: &str, dir: bool, depth: usize) -> ProjectedRow {
        ProjectedRow {
            entry: FileEntry {
                path: Arc::from(Path::new(path)),
                name: Arc::from(
                    Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                        .as_str(),
                ),
                kind: if dir {
                    fs_core::EntryKind::Dir
                } else {
                    fs_core::EntryKind::File
                },
                size: 0,
                modified: std::time::UNIX_EPOCH,
                created: None,
                hidden: false,
            },
            depth,
            expanded: false,
        }
    }

    #[test]
    fn row_at_maps_content_y_onto_the_uniform_band() {
        assert_eq!(row_at(0.0, H, 4), Some(0));
        assert_eq!(row_at(H - 0.01, H, 4), Some(0));
        assert_eq!(
            row_at(H, H, 4),
            Some(1),
            "a boundary belongs to the row below"
        );
        assert_eq!(row_at(3.0 * H + 5.0, H, 4), Some(3));
        assert_eq!(
            row_at(4.0 * H, H, 4),
            None,
            "past the last row is empty space"
        );
        assert_eq!(row_at(-5.0, H, 4), None);
        assert_eq!(row_at(10.0, H, 0), None, "an empty listing has no rows");
        assert_eq!(row_at(10.0, 0.0, 4), None, "no divide-by-zero panic");
    }

    #[test]
    fn target_for_row_prefers_the_row_the_user_aimed_at() {
        assert_eq!(
            target_for_row(Some(&row("/d/folder", true, 0))),
            DropTarget::Folder(path("/d/folder"))
        );
        assert_eq!(
            target_for_row(Some(&row("/d/file.txt", false, 0))),
            DropTarget::Background,
            "a top-level file row means the open directory"
        );
        assert_eq!(
            target_for_row(Some(&row("/d/folder/inner.txt", false, 1))),
            DropTarget::Folder(path("/d/folder")),
            "an injected child file row means *its* folder, not the pane's"
        );
        assert_eq!(target_for_row(None), DropTarget::Background);
    }

    #[test]
    fn dest_dir_resolves_background_against_the_open_directory() {
        let current = Path::new("/d");
        assert_eq!(
            DropTarget::Background.dest_dir(Some(current)),
            Some(current)
        );
        assert_eq!(DropTarget::Background.dest_dir(None), None);
        assert_eq!(
            DropTarget::Folder(path("/d/folder")).dest_dir(Some(current)),
            Some(Path::new("/d/folder")),
            "a folder target ignores the open directory"
        );
    }

    #[test]
    fn plan_drop_moves_by_default_and_copies_with_the_modifier() {
        let sources = [path("/src/a.txt"), path("/src/b.txt")];
        assert_eq!(
            plan_drop(Path::new("/dest"), &sources, false),
            Some(FileOp::Move {
                sources: vec![PathBuf::from("/src/a.txt"), PathBuf::from("/src/b.txt")],
                dest_dir: PathBuf::from("/dest"),
            })
        );
        assert_eq!(
            plan_drop(Path::new("/dest"), &sources, true),
            Some(FileOp::Copy {
                sources: vec![PathBuf::from("/src/a.txt"), PathBuf::from("/src/b.txt")],
                dest_dir: PathBuf::from("/dest"),
            })
        );
    }

    #[test]
    fn plan_drop_refuses_a_destination_inside_a_source() {
        // fs-core fails these cleanly (`queue.rs` guards move *and* copy);
        // refusing to submit means no failure toast for a slip of the mouse.
        assert_eq!(plan_drop(Path::new("/a/b"), &[path("/a")], false), None);
        assert_eq!(plan_drop(Path::new("/a/b"), &[path("/a")], true), None);
        assert_eq!(
            plan_drop(Path::new("/a"), &[path("/a")], false),
            None,
            "onto itself"
        );
    }

    #[test]
    fn plan_drop_treats_a_move_into_the_source_folder_as_nothing() {
        // Explorer: dragging a file around inside its own folder does nothing.
        assert_eq!(plan_drop(Path::new("/d"), &[path("/d/a.txt")], false), None);
        // ...but a *copy* into the same folder is a deliberate duplicate (op
        // planning gives it a keep-both name).
        assert_eq!(
            plan_drop(Path::new("/d"), &[path("/d/a.txt")], true),
            Some(FileOp::Copy {
                sources: vec![PathBuf::from("/d/a.txt")],
                dest_dir: PathBuf::from("/d"),
            })
        );
        // A mixed drag keeps only what actually has somewhere to go.
        assert_eq!(
            plan_drop(
                Path::new("/d"),
                &[path("/d/a.txt"), path("/other/b.txt")],
                false
            ),
            Some(FileOp::Move {
                sources: vec![PathBuf::from("/other/b.txt")],
                dest_dir: PathBuf::from("/d"),
            })
        );
    }

    #[test]
    fn copy_modifier_is_alt_and_nothing_else() {
        assert!(copy_modifier_held(Modifiers::alt()));
        assert!(!copy_modifier_held(Modifiers::none()));
        assert!(!copy_modifier_held(Modifiers::command()));
        assert!(!copy_modifier_held(Modifiers::shift()));
        // ...and shift is the one that forces a move.
        assert!(move_modifier_held(Modifiers::shift()));
        assert!(!move_modifier_held(Modifiers::none()));
        assert!(!move_modifier_held(Modifiers::alt()));
    }

    // Explorer's volume rule (plan §3 is explicit that this app is Explorer,
    // not Finder): within a volume a plain drag *moves*, across volumes it
    // *copies* — dragging a file off a removable disk must not empty it — and
    // either default can be overridden.
    #[gpui::test]
    fn a_plain_drag_moves_within_a_volume_and_copies_across_them(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        cx.update(|cx| {
            let vfs = FsContext::global(cx).vfs.clone();
            let same = [path("/root/a.txt")];
            let other_volume = [path("/Volumes/Camera/DCIM/img.jpg")];

            assert!(
                !drop_copies(&*vfs, Path::new("/root/beta"), &same, Modifiers::none()),
                "same volume: a plain drag moves"
            );
            assert!(
                drop_copies(
                    &*vfs,
                    Path::new("/root/beta"),
                    &other_volume,
                    Modifiers::none()
                ),
                "off another volume: a plain drag copies"
            );
            assert!(
                drop_copies(
                    &*vfs,
                    Path::new("/Volumes/Camera/DCIM"),
                    &same,
                    Modifiers::none()
                ),
                "and onto another volume too"
            );
            // A mixed drag is a copy: one gesture cannot be half a move.
            let mixed = [path("/root/a.txt"), path("/Volumes/Camera/DCIM/img.jpg")];
            assert!(drop_copies(
                &*vfs,
                Path::new("/root/beta"),
                &mixed,
                Modifiers::none()
            ));

            // The modifiers override either default, and ⇧ wins over ⌥.
            assert!(drop_copies(
                &*vfs,
                Path::new("/root/beta"),
                &same,
                Modifiers::alt()
            ));
            assert!(!drop_copies(
                &*vfs,
                Path::new("/root/beta"),
                &other_volume,
                Modifiers::shift()
            ));
            assert!(
                !drop_copies(
                    &*vfs,
                    Path::new("/root/beta"),
                    &other_volume,
                    Modifiers {
                        shift: true,
                        alt: true,
                        ..Modifiers::none()
                    }
                ),
                "⇧ wins over ⌥"
            );
        });
    }

    #[test]
    fn external_payload_pairs_paths_with_their_dir_flag() {
        let entries = [
            (PathBuf::from("/d/folder"), true),
            (PathBuf::from("/d/a.txt"), false),
        ];
        let payload = external_payload(&entries).expect("non-empty");
        let ExternalDragPayload::Files(files) = payload;
        assert_eq!(files.entries(), entries);
        assert!(
            external_payload(&[]).is_none(),
            "an empty drag offers the platform nothing"
        );
    }

    // ---------------- the gesture ----------------

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/",
                json!({
                    "root": {
                        "alpha": {},
                        "beta": { "kept.txt": "k" },
                        "a.txt": "a",
                        "b.txt": "b",
                    },
                    "outside": { "note.txt": "n" },
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

    /// `/root` open and laid out. Rows (dirs first, then names): `alpha`,
    /// `beta`, `a.txt`, `b.txt` — so row `i` spans content `[i*24, i*24+24)`.
    fn open_root(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeVfs>,
        Entity<Pane>,
        Entity<DirView>,
        &mut VisualTestContext,
    ) {
        let vfs = init_test(cx);
        let (pane, cx) = cx.add_window_view(|window, cx| Pane::new(Theme::dark(), window, cx));
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        (vfs, pane, dir_view, cx)
    }

    fn rows(dir_view: &Entity<DirView>, cx: &mut VisualTestContext) -> Vec<PathBuf> {
        dir_view.read_with(cx, |view, _| {
            view.flat_rows()
                .iter()
                .map(|row| row.entry.path.to_path_buf())
                .collect()
        })
    }

    /// A window point in the middle of projected row `ix`.
    fn row_point(
        dir_view: &Entity<DirView>,
        cx: &mut VisualTestContext,
        ix: usize,
    ) -> Point<Pixels> {
        let viewport = dir_view.read_with(cx, |view, _| list_viewport(view));
        gpui::point(
            viewport.left() + px(40.0),
            viewport.top() + px(ix as f32 * H + H / 2.0),
        )
    }

    /// Empty space below the last row (still inside the list surface).
    fn background_point(
        dir_view: &Entity<DirView>,
        cx: &mut VisualTestContext,
        row_count: usize,
    ) -> Point<Pixels> {
        let viewport = dir_view.read_with(cx, |view, _| list_viewport(view));
        gpui::point(
            viewport.left() + px(40.0),
            viewport.top() + px(row_count as f32 * H + H),
        )
    }

    /// Press on `from`, cross gpui's 2px drag threshold, and settle on `to`.
    /// The gesture is left *open* — the caller releases (or does not).
    fn start_drag(
        cx: &mut VisualTestContext,
        from: Point<Pixels>,
        to: Point<Pixels>,
        modifiers: Modifiers,
    ) {
        cx.simulate_mouse_down(from, MouseButton::Left, modifiers);
        cx.simulate_mouse_move(
            from + gpui::point(px(6.0), px(6.0)),
            MouseButton::Left,
            modifiers,
        );
        cx.simulate_mouse_move(to, MouseButton::Left, modifiers);
    }

    fn tree_has(vfs: &Arc<FakeVfs>, path: &str) -> bool {
        vfs.snapshot().keys().any(|p| p == Path::new(path))
    }

    #[gpui::test]
    fn dropping_an_unselected_row_on_a_folder_moves_just_that_row(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        assert_eq!(
            rows(&dir_view, cx),
            [
                PathBuf::from("/root/alpha"),
                PathBuf::from("/root/beta"),
                PathBuf::from("/root/a.txt"),
                PathBuf::from("/root/b.txt"),
            ]
        );

        let from = row_point(&dir_view, cx, 2); // a.txt
        let onto = row_point(&dir_view, cx, 1); // beta
        start_drag(cx, from, onto, Modifiers::none());

        // The row's own drag claimed the gesture: no marquee, a live drag, and
        // the folder under the pointer is the armed target.
        dir_view.read_with(cx, |view, cx| {
            assert!(view.marquee.is_none(), "a row press is a file drag");
            assert_eq!(
                view.active_drop_target(cx),
                Some(&DropTarget::Folder(path("/root/beta")))
            );
        });
        assert!(cx.update(|_, cx| cx.has_active_drag()));
        assert_eq!(
            cx.update(|_, cx| cx.active_drag_cursor_style()),
            Some(CursorStyle::Arrow),
            "no modifier: a move"
        );

        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert!(tree_has(&vfs, "/root/beta/a.txt"), "moved into the folder");
        assert!(!tree_has(&vfs, "/root/a.txt"), "and gone from the source");
        assert!(tree_has(&vfs, "/root/b.txt"), "nothing else moved");
        dir_view.read_with(cx, |view, cx| {
            assert!(
                view.active_drop_target(cx).is_none(),
                "the release cleared it"
            );
        });
    }

    #[gpui::test]
    fn dropping_a_selected_row_moves_the_whole_selection(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        dir_view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt"), Path::new("/root/b.txt")], cx)
        });
        cx.run_until_parked();

        let from = row_point(&dir_view, cx, 2); // a.txt — part of the selection
        let onto = row_point(&dir_view, cx, 1); // beta
        start_drag(cx, from, onto, Modifiers::none());
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert!(tree_has(&vfs, "/root/beta/a.txt"));
        assert!(tree_has(&vfs, "/root/beta/b.txt"), "the selection came too");
        assert!(!tree_has(&vfs, "/root/a.txt"));
        assert!(!tree_has(&vfs, "/root/b.txt"));
    }

    #[gpui::test]
    fn grabbing_an_unselected_row_leaves_the_selection_behind(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        dir_view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/b.txt")], cx)
        });
        cx.run_until_parked();

        let from = row_point(&dir_view, cx, 2); // a.txt — NOT selected
        let onto = row_point(&dir_view, cx, 1); // beta
        start_drag(cx, from, onto, Modifiers::none());
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert!(tree_has(&vfs, "/root/beta/a.txt"), "the grabbed row moved");
        assert!(
            tree_has(&vfs, "/root/b.txt"),
            "the selected-but-not-grabbed row stayed put"
        );
    }

    #[gpui::test]
    fn the_copy_modifier_turns_the_drop_into_a_copy(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        let from = row_point(&dir_view, cx, 2); // a.txt
        let onto = row_point(&dir_view, cx, 1); // beta
        start_drag(cx, from, onto, Modifiers::alt());

        assert_eq!(
            cx.update(|_, cx| cx.active_drag_cursor_style()),
            Some(CursorStyle::DragCopy),
            "§8: the modifier flips the cursor"
        );

        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::alt());
        cx.run_until_parked();

        assert!(tree_has(&vfs, "/root/beta/a.txt"), "copied in");
        assert!(tree_has(&vfs, "/root/a.txt"), "and the source survives");
    }

    #[gpui::test]
    fn dropping_on_the_background_moves_into_the_open_directory(cx: &mut TestAppContext) {
        let (vfs, pane, dir_view, cx) = open_root(cx);
        // Open `beta`, then drag its child out onto the background of... the
        // same folder (a no-op), and finally into `/root` proper via a real
        // cross-directory drag from the expanded parent listing.
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        // Expand `beta` so its child is a row of *this* listing at depth 1.
        dir_view.update(cx, |view, cx| {
            view.toggle_expanded(Path::new("/root/beta"), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            rows(&dir_view, cx),
            [
                PathBuf::from("/root/alpha"),
                PathBuf::from("/root/beta"),
                PathBuf::from("/root/beta/kept.txt"),
                PathBuf::from("/root/a.txt"),
                PathBuf::from("/root/b.txt"),
            ]
        );

        let from = row_point(&dir_view, cx, 2); // beta/kept.txt (depth 1)
        let onto = background_point(&dir_view, cx, 5);
        start_drag(cx, from, onto, Modifiers::none());
        dir_view.read_with(cx, |view, cx| {
            assert_eq!(view.active_drop_target(cx), Some(&DropTarget::Background));
        });
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert!(
            tree_has(&vfs, "/root/kept.txt"),
            "the background drop moved it into the open directory"
        );
        assert!(!tree_has(&vfs, "/root/beta/kept.txt"));
    }

    #[gpui::test]
    fn an_injected_child_row_targets_its_own_folder(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        dir_view.update(cx, |view, cx| {
            view.toggle_expanded(Path::new("/root/beta"), cx)
        });
        cx.run_until_parked();

        // Drag /root/a.txt onto beta's expanded child row: the destination is
        // beta, the folder that row lives in — not the open directory.
        let from = row_point(&dir_view, cx, 3); // a.txt
        let onto = row_point(&dir_view, cx, 2); // beta/kept.txt
        start_drag(cx, from, onto, Modifiers::none());
        dir_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_drop_target(cx),
                Some(&DropTarget::Folder(path("/root/beta")))
            );
        });
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert!(tree_has(&vfs, "/root/beta/a.txt"));
    }

    #[gpui::test]
    fn dropping_a_folder_on_itself_does_nothing(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        let before = vfs.snapshot();

        let beta = row_point(&dir_view, cx, 1);
        let beta_again = gpui::point(beta.x + px(20.0), beta.y);
        start_drag(cx, beta, beta_again, Modifiers::none());
        dir_view.read_with(cx, |view, cx| {
            assert!(
                view.active_drop_target(cx).is_none(),
                "an impossible drop is never highlighted"
            );
        });
        assert_eq!(
            cx.update(|_, cx| cx.active_drag_cursor_style()),
            Some(CursorStyle::OperationNotAllowed)
        );
        cx.simulate_mouse_up(beta_again, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(vfs.snapshot(), before, "nothing was submitted");
    }

    #[gpui::test]
    fn dropping_in_the_source_directory_does_nothing(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        let before = vfs.snapshot();

        let from = row_point(&dir_view, cx, 2); // a.txt, already in /root
        let onto = background_point(&dir_view, cx, 4);
        start_drag(cx, from, onto, Modifiers::none());
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(vfs.snapshot(), before);
    }

    #[gpui::test]
    fn leaving_the_list_clears_the_drop_target(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        let before = vfs.snapshot();
        let from = row_point(&dir_view, cx, 2);
        let onto = row_point(&dir_view, cx, 1);
        start_drag(cx, from, onto, Modifiers::none());
        dir_view.read_with(cx, |view, cx| {
            assert!(view.active_drop_target(cx).is_some())
        });

        // §8 out-of-bounds self-clear: drag clean above the list.
        let viewport = dir_view.read_with(cx, |view, _| list_viewport(view));
        let outside = gpui::point(viewport.left() + px(40.0), viewport.top() - px(80.0));
        cx.simulate_mouse_move(outside, MouseButton::Left, Modifiers::none());
        dir_view.read_with(cx, |view, cx| {
            assert!(view.active_drop_target(cx).is_none(), "cleared on leaving");
        });

        // Releasing out there drops nothing on us.
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(vfs.snapshot(), before);
    }

    #[gpui::test]
    fn spring_load_navigates_after_the_delay_and_the_drop_lands_inside(cx: &mut TestAppContext) {
        let (vfs, pane, dir_view, cx) = open_root(cx);
        let from = row_point(&dir_view, cx, 2); // a.txt
        let onto = row_point(&dir_view, cx, 1); // beta
        start_drag(cx, from, onto, Modifiers::none());

        // Not yet: the timer runs on fake time.
        cx.executor().advance_clock(SPRING_LOAD_DELAY / 2);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.path(), Some(Path::new("/root")), "still hovering");
        });

        cx.executor().advance_clock(SPRING_LOAD_DELAY);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.path(),
                Some(Path::new("/root/beta")),
                "500ms over a folder springs it open"
            );
        });
        dir_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_drop_target(cx),
                Some(&DropTarget::Background),
                "the folder we sprang into is now the destination"
            );
        });

        // Releasing now lands in the folder we sprang into.
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert!(tree_has(&vfs, "/root/beta/a.txt"));
        assert!(!tree_has(&vfs, "/root/a.txt"));
    }

    // Spring-load must not fire for a target the drop is *refused* on: simply
    // beginning a drag on a folder row hovers that row, and arming the timer
    // there navigated the pane into the folder the user was trying to drag —
    // while the cursor was saying "not allowed".
    #[gpui::test]
    fn a_refused_target_never_springs_open(cx: &mut TestAppContext) {
        let (vfs, pane, dir_view, cx) = open_root(cx);
        let before = vfs.snapshot();

        // Press on `beta` and hold, without leaving its row band — the start
        // of every folder drag there is.
        let beta = row_point(&dir_view, cx, 1);
        let beta_again = gpui::point(beta.x + px(20.0), beta.y);
        start_drag(cx, beta, beta_again, Modifiers::none());
        dir_view.read_with(cx, |view, cx| {
            assert!(
                view.active_drop_target(cx).is_none(),
                "refused, so no invite"
            );
        });

        cx.executor().advance_clock(SPRING_LOAD_DELAY * 2);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.path(),
                Some(Path::new("/root")),
                "a drop this pane would refuse must not spring the folder open"
            );
        });

        cx.simulate_mouse_up(beta_again, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(vfs.snapshot(), before);
    }

    // A drag that leaves the window takes gpui's `active_drag` with it and
    // dispatches no mouse event, so the armed `DropState` survives with a
    // spent timer. The next drag must not adopt it — that used to kill
    // spring-load for that folder for the rest of the session.
    #[gpui::test]
    fn a_stale_drop_state_does_not_disable_the_next_spring_load(cx: &mut TestAppContext) {
        let (_vfs, pane, dir_view, cx) = open_root(cx);
        let onto = row_point(&dir_view, cx, 1); // beta
        let entered = |cx: &mut VisualTestContext| {
            cx.simulate_event(FileDropEvent::Entered {
                position: onto,
                paths: ExternalPaths([PathBuf::from("/outside/note.txt")].into_iter().collect()),
            });
        };

        entered(cx);
        cx.simulate_event(FileDropEvent::Exited);
        // Let the abandoned timer fire and decline: it is now spent, but the
        // armed `DropState` it belonged to is still sitting there (nothing
        // dispatched a mouse event that could clear it).
        cx.executor().advance_clock(SPRING_LOAD_DELAY * 2);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.path(),
                Some(Path::new("/root")),
                "the abandoned timer must not navigate"
            );
        });

        // Same folder, a brand-new gesture: it has to re-arm.
        entered(cx);
        cx.executor().advance_clock(SPRING_LOAD_DELAY * 2);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.path(),
                Some(Path::new("/root/beta")),
                "the second hover springs the folder open like the first"
            );
        });
    }

    // What the drop submits is what the highlight and cursor advertised, taken
    // from the armed state — not re-derived from the modifiers that happen to
    // be held at mouse-up. Releasing ⌥ a frame early used to turn an
    // advertised copy into a move `plan_drop` then refused: a lit target that
    // silently did nothing.
    #[gpui::test]
    fn the_drop_submits_what_was_advertised_not_the_release_modifiers(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        let from = row_point(&dir_view, cx, 2); // a.txt
        let onto = row_point(&dir_view, cx, 1); // beta
        start_drag(cx, from, onto, Modifiers::alt());
        assert_eq!(
            cx.update(|_, cx| cx.active_drag_cursor_style()),
            Some(CursorStyle::DragCopy),
            "a copy was advertised"
        );

        // ⌥ released just before the button.
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        assert!(tree_has(&vfs, "/root/beta/a.txt"), "copied in");
        assert!(
            tree_has(&vfs, "/root/a.txt"),
            "and the source survives — the advertised copy is what ran"
        );
    }

    // Rows are keyed by **path**, not by index: gpui persists an element's
    // pending press across frames, so a re-projection between the press and
    // the move (here a watcher patch, which is now routine) would otherwise
    // hand the press to whichever entry slid into that index — and this row's
    // payload turns that into a real move of a file nobody touched.
    #[gpui::test]
    fn a_reprojection_mid_gesture_drags_the_row_that_was_pressed(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        let press = row_point(&dir_view, cx, 2); // a.txt
        cx.simulate_mouse_down(press, MouseButton::Left, Modifiers::none());

        // A new folder appears externally and sorts to the top: index 2 is now
        // `beta`, and `a.txt` has moved down to index 3.
        vfs.insert_dir("/root/aaa");
        cx.executor().advance_clock(crate::pane::WATCH_LATENCY);
        cx.run_until_parked();
        assert_eq!(
            rows(&dir_view, cx),
            [
                PathBuf::from("/root/aaa"),
                PathBuf::from("/root/alpha"),
                PathBuf::from("/root/beta"),
                PathBuf::from("/root/a.txt"),
                PathBuf::from("/root/b.txt"),
            ]
        );

        // Now cross the drag threshold and drop on the new folder.
        let onto = row_point(&dir_view, cx, 0); // aaa
        cx.simulate_mouse_move(
            press + gpui::point(px(6.0), px(6.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(onto, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(onto, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        assert!(
            tree_has(&vfs, "/root/aaa/a.txt"),
            "the row that was pressed is the row that moved"
        );
        assert!(
            tree_has(&vfs, "/root/beta"),
            "and the entry that slid into that index stayed put"
        );
    }

    #[gpui::test]
    fn leaving_the_folder_cancels_the_spring_load(cx: &mut TestAppContext) {
        let (_vfs, pane, dir_view, cx) = open_root(cx);
        let from = row_point(&dir_view, cx, 2);
        let onto = row_point(&dir_view, cx, 1); // beta
        start_drag(cx, from, onto, Modifiers::none());

        cx.executor().advance_clock(SPRING_LOAD_DELAY / 2);
        cx.run_until_parked();

        // Move off the folder before the timer elapses: the task is dropped
        // with the state it belonged to.
        let away = background_point(&dir_view, cx, 4);
        cx.simulate_mouse_move(away, MouseButton::Left, Modifiers::none());
        cx.executor().advance_clock(SPRING_LOAD_DELAY * 2);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.path(),
                Some(Path::new("/root")),
                "leaving cancels the spring-load"
            );
        });

        cx.simulate_mouse_up(away, MouseButton::Left, Modifiers::none());
    }

    #[gpui::test]
    fn paths_dragged_in_from_another_app_are_copied(cx: &mut TestAppContext) {
        // gpui turns a platform file drop into an ordinary internal drag whose
        // payload is `ExternalPaths` (window.rs), so the whole Finder → us
        // path is exercisable headlessly. Modifiers are *not* carried by the
        // platform events, which is why an external drop always copies.
        let (vfs, _pane, dir_view, cx) = open_root(cx);
        let onto = row_point(&dir_view, cx, 1); // beta

        cx.simulate_event(FileDropEvent::Entered {
            position: onto,
            paths: ExternalPaths([PathBuf::from("/outside/note.txt")].into_iter().collect()),
        });
        dir_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_drop_target(cx),
                Some(&DropTarget::Folder(path("/root/beta"))),
                "an external drag arms the same drop target"
            );
        });
        assert_eq!(
            cx.update(|_, cx| cx.active_drag_cursor_style()),
            Some(CursorStyle::DragCopy),
            "an external drop can only ever copy"
        );

        cx.simulate_event(FileDropEvent::Submit { position: onto });
        cx.run_until_parked();
        assert!(tree_has(&vfs, "/root/beta/note.txt"), "copied in");
        assert!(
            tree_has(&vfs, "/outside/note.txt"),
            "another app's file is never moved"
        );
    }

    // The §4c `New ▸` phantom row is a folder-shaped row for a path that does
    // not exist. A drop aimed at it must land in the pane's directory, not in
    // a folder nothing has created yet.
    #[gpui::test]
    fn a_new_entry_phantom_row_is_not_a_drop_target(cx: &mut TestAppContext) {
        let (vfs, pane, dir_view, cx) = open_root(cx);
        let before = rows(&dir_view, cx).len();

        cx.update(|window, cx| {
            pane.update(cx, |pane, cx| pane.new_folder(window, cx));
        });
        cx.run_until_parked();
        let phantom_ix = before; // appended last
        assert_eq!(
            rows(&dir_view, cx).get(phantom_ix).map(PathBuf::as_path),
            Some(Path::new("/root/New Folder")),
        );

        let onto = row_point(&dir_view, cx, phantom_ix);
        cx.simulate_event(FileDropEvent::Entered {
            position: onto,
            paths: ExternalPaths([PathBuf::from("/outside/note.txt")].into_iter().collect()),
        });
        dir_view.read_with(cx, |view, cx| {
            assert_eq!(
                view.active_drop_target(cx),
                Some(&DropTarget::Background),
                "the phantom row falls through to the pane's own directory"
            );
        });

        cx.simulate_event(FileDropEvent::Submit { position: onto });
        cx.run_until_parked();
        assert!(tree_has(&vfs, "/root/note.txt"), "landed in /root");
        assert!(
            !tree_has(&vfs, "/root/New Folder/note.txt"),
            "and never inside a folder that does not exist"
        );
    }

    #[gpui::test]
    fn a_drag_that_leaves_the_window_stops_inviting_and_springing(cx: &mut TestAppContext) {
        // A platform file drag that leaves the window takes gpui's active drag
        // with it and dispatches no mouse event at all — so nothing clears the
        // armed state. Both readers therefore require a live drag: the
        // highlight goes cold, and a spring-load timer already ticking must not
        // navigate behind a gesture that no longer exists.
        let (_vfs, pane, dir_view, cx) = open_root(cx);
        let onto = row_point(&dir_view, cx, 1); // beta
        cx.simulate_event(FileDropEvent::Entered {
            position: onto,
            paths: ExternalPaths([PathBuf::from("/outside/note.txt")].into_iter().collect()),
        });
        dir_view.read_with(cx, |view, cx| {
            assert!(view.active_drop_target(cx).is_some())
        });

        cx.simulate_event(FileDropEvent::Exited);
        cx.run_until_parked();
        assert!(!cx.update(|_, cx| cx.has_active_drag()));
        dir_view.read_with(cx, |view, cx| {
            assert!(
                view.active_drop_target(cx).is_none(),
                "no drag, no drop invitation"
            );
        });

        cx.executor().advance_clock(SPRING_LOAD_DELAY * 2);
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.path(),
                Some(Path::new("/root")),
                "the abandoned spring-load must not navigate"
            );
        });
    }
}
