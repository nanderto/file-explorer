//! Rubber-band marquee selection (ARCHITECTURE.md §8 "Rubber-band marquee").
//!
//! `MarqueeState` is a **field** of [`DirView`] (`marquee:
//! Option<MarqueeState>`), the same shape as `rename::RenameState` —
//! never its own entity. The gesture is owned by gpui's drag machinery, per
//! §8: the details list's background surface carries
//! `on_drag(`[`MarqueeStart`]`, empty ghost)` so gpui takes mouse capture for
//! the whole gesture, and a global `on_drag_move::<MarqueeStart>` (it fires
//! for every move while the drag lives, inside the element or not) feeds the
//! moving corner in.
//!
//! **Everything geometric here is arithmetic against the uniform row band**,
//! never a scan of painted elements: `uniform_list` virtualizes off-screen
//! rows away, so a marquee that has autoscrolled past them still has to
//! select them. The three pure functions below — [`ContentPoint::from_window`]
//! (window space → content space, i.e. past the scroll offset),
//! [`rows_in_rect`] (content-space rect → half-open row-index range) and
//! [`autoscroll_for`] (pointer → two-speed edge autoscroll) — are the whole
//! model, and are unit-tested headlessly.
//!
//! **Where a marquee may start.** Only in empty space: a press that lands on
//! a painted row band is the start of a *file* drag (`drag.rs`), not a
//! marquee. That test is arithmetic too — the press's content-space `y` must
//! sit past the last row band. Details-list rows are full-width, so (exactly
//! as in Explorer's own details view) the only empty space is below the last
//! row, which by definition exists only when the listing is shorter than the
//! viewport; edge autoscroll therefore engages when the projection *grows*
//! mid-gesture (a child listing landing, a watcher patch) or the window
//! shrinks under the drag. It is the icon grid (M4), whose empty space is
//! plentiful, that leans on it routinely.
//!
//! **Selection** is mutated only through [`crate::selection::SelectionModel`]
//! and is path-keyed like every other selection change:
//! `SelectionModel::select_marquee` sets `base ∪ band`, where `base` is empty
//! for a plain drag (which replaces the selection) and the pre-gesture
//! selection for an additive `cmd`-drag (which unions, matching Explorer) —
//! so shrinking the band gives back only the rows the band itself added.

use std::collections::BTreeSet;
use std::ops::Range;
use std::time::Duration;

use fs_core::EntryId;
use gpui::{
    Bounds, Context, Div, DragMoveEvent, IntoElement, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, Render, Stateful, Task, Window, div, point, prelude::*, px,
};

use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::views::details_list::ROW_HEIGHT;

/// How close to a viewport edge the pointer must be for the *slow* autoscroll
/// speed; at or past the edge itself it switches to the fast one (§8
/// "two-speed edge autoscroll").
pub const AUTOSCROLL_SLOW_BAND: f32 = 24.0;
/// Content pixels per tick at the two speeds.
const AUTOSCROLL_SLOW_STEP: f32 = 8.0;
const AUTOSCROLL_FAST_STEP: f32 = 28.0;
/// Autoscroll tick interval. Runs on [`fs_core::Spawner::timer`], so tests
/// drive it with fake time.
pub const AUTOSCROLL_TICK: Duration = Duration::from_millis(30);

/// Fill / border alpha of the band, applied to the theme accent — the app
/// crate never names a color.
const MARQUEE_FILL_ALPHA: f32 = 0.18;
const MARQUEE_BORDER_ALPHA: f32 = 0.8;
/// A purely vertical (or horizontal) drag still has to paint something.
const MIN_BAND_PX: f32 = 1.0;

/// The `on_drag` payload, and so the type token `on_drag_move` filters on.
///
/// Deviation from §8's `MarqueeStart { origin }`: the origin lives in
/// `MarqueeState` instead, because a drag payload is built at *render*
/// time, before any press exists. Capturing it in the surface's
/// `on_mouse_down` also gives the true press point rather than the position
/// at which gpui's 2px drag threshold happened to trip.
pub struct MarqueeStart;

/// The drag's "preview": gpui owns mouse capture for the gesture, but the
/// marquee paints its own band inside the list, so the ghost renders nothing
/// (same trick as the workspace splitters).
pub struct MarqueeGhost;

impl Render for MarqueeGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// A point in the list's **content** space: `x` from the viewport's left
/// edge, `y` from the top of the *first row* — so it keeps meaning for rows
/// scrolled out of the viewport, which is exactly what the marquee needs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContentPoint {
    pub x: f32,
    pub y: f32,
}

impl ContentPoint {
    /// Window-space pointer → content space. `scroll_y` is the list's scroll
    /// offset as gpui stores it: `0` at the top and **more negative** the
    /// further down the content is scrolled, so subtracting it adds the
    /// scrolled-away height back.
    pub fn from_window(pointer: Point<Pixels>, viewport: Bounds<Pixels>, scroll_y: f32) -> Self {
        Self {
            x: f32::from(pointer.x - viewport.left()),
            y: f32::from(pointer.y - viewport.top()) - scroll_y,
        }
    }
}

/// A normalized rubber band in content space: `left <= right`, `top <=
/// bottom`, whichever way round the user dragged.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MarqueeRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl MarqueeRect {
    /// The band between two corners, in either order (an upward and/or
    /// leftward drag normalizes to the same rect as its mirror).
    pub fn between(a: ContentPoint, b: ContentPoint) -> Self {
        Self {
            left: a.x.min(b.x),
            top: a.y.min(b.y),
            right: a.x.max(b.x),
            bottom: a.y.max(b.y),
        }
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }
}

/// The rows a band covers, as a **half-open** index range into the flat
/// projection.
///
/// The overlap rule, exactly: row `i` occupies the content band
/// `[i * row_height, (i + 1) * row_height)`, and row `i` is in the range iff
/// that band and the rubber band **overlap as open intervals** —
/// `i * row_height < bottom && (i + 1) * row_height > top`. Consequences
/// worth knowing:
///
/// * A band whose edge lands *exactly* on a row boundary does not reach into
///   the next row (dragging down to the top edge of row 3 selects 0..3).
/// * Any non-zero overlap does count, however small: one pixel into a row
///   selects it (Explorer behavior — the marquee is not a majority vote).
/// * A degenerate (zero-height) band selects the single row it sits inside,
///   and nothing when it sits exactly on a boundary.
///
/// Which is the same thing as `floor(top / h) .. ceil(bottom / h)`, clamped
/// to the listing — the form actually computed here, so the whole test costs
/// two divisions no matter how many rows are virtualized away.
pub fn rows_in_rect(rect: MarqueeRect, row_height: f32, row_count: usize) -> Range<usize> {
    if row_count == 0 || row_height <= 0.0 {
        return 0..0;
    }
    let content_bottom = row_count as f32 * row_height;
    let top = rect.top.clamp(0.0, content_bottom);
    let bottom = rect.bottom.clamp(0.0, content_bottom);
    let start = ((top / row_height).floor() as usize).min(row_count);
    let end = ((bottom / row_height).ceil() as usize).clamp(start, row_count);
    start..end
}

/// The two-speed edge autoscroll (§8), as a direction + speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoScroll {
    UpFast,
    UpSlow,
    DownSlow,
    DownFast,
}

impl AutoScroll {
    /// Signed content pixels per tick: positive moves the content *up* under
    /// the viewport (revealing later rows).
    pub fn step(self) -> f32 {
        match self {
            AutoScroll::UpFast => -AUTOSCROLL_FAST_STEP,
            AutoScroll::UpSlow => -AUTOSCROLL_SLOW_STEP,
            AutoScroll::DownSlow => AUTOSCROLL_SLOW_STEP,
            AutoScroll::DownFast => AUTOSCROLL_FAST_STEP,
        }
    }
}

/// Which autoscroll (if any) a pointer at `pointer_y` asks for, against a
/// viewport spanning `[top, bottom)`. Inside the [`AUTOSCROLL_SLOW_BAND`] of
/// an edge scrolls slowly; at or beyond the edge scrolls fast; the middle
/// does not scroll. The nearer edge wins, so a viewport shorter than two
/// bands still behaves.
pub fn autoscroll_for(pointer_y: f32, top: f32, bottom: f32) -> Option<AutoScroll> {
    let from_top = pointer_y - top;
    let from_bottom = bottom - pointer_y;
    if from_top <= from_bottom {
        if from_top <= 0.0 {
            Some(AutoScroll::UpFast)
        } else if from_top < AUTOSCROLL_SLOW_BAND {
            Some(AutoScroll::UpSlow)
        } else {
            None
        }
    } else if from_bottom <= 0.0 {
        Some(AutoScroll::DownFast)
    } else if from_bottom < AUTOSCROLL_SLOW_BAND {
        Some(AutoScroll::DownSlow)
    } else {
        None
    }
}

/// One in-flight rubber-band gesture. Lives at `DirView.marquee`; dropping it
/// (on mouse-up, or with the view) cancels its autoscroll task.
pub(crate) struct MarqueeState {
    /// The anchored corner, fixed in **content** space for the whole gesture
    /// — so autoscrolling keeps the band pinned to the row it started from.
    origin: ContentPoint,
    /// The moving corner. `None` until gpui's drag threshold trips and the
    /// gesture really becomes a marquee: a press that never moves is a click.
    current: Option<ContentPoint>,
    /// The selection the gesture unions onto: empty for a plain drag
    /// (replace), the pre-gesture selection for an additive `cmd`-drag.
    base: BTreeSet<EntryId>,
    /// The armed autoscroll, so the task is only respawned when it changes.
    scroll: Option<AutoScroll>,
    /// §8: **exactly one** `Option<Task>` slot. Dropping it stops the scroll.
    _autoscroll: Option<Task<()>>,
}

impl MarqueeState {
    fn new(origin: ContentPoint, base: BTreeSet<EntryId>) -> Self {
        Self {
            origin,
            current: None,
            base,
            scroll: None,
            _autoscroll: None,
        }
    }

    /// The band, once the gesture has actually started dragging.
    pub(crate) fn rect(&self) -> Option<MarqueeRect> {
        Some(MarqueeRect::between(self.origin, self.current?))
    }

    /// True while the moving corner is at or below the anchor — which end of
    /// the band the cursor should land on.
    fn downward(&self) -> bool {
        self.current
            .is_some_and(|current| current.y >= self.origin.y)
    }
}

// ----------------------------------------------------------------------
// List geometry helpers (the scroll handle is the only source of truth for
// where the rows were actually painted)
// ----------------------------------------------------------------------

/// The list's viewport in window space, as of the last paint.
pub(crate) fn list_viewport(view: &DirView) -> Bounds<Pixels> {
    view.scroll_handle().0.borrow().base_handle.bounds()
}

/// The list's scroll offset (`0` at the top, more negative further down).
pub(crate) fn scroll_y(view: &DirView) -> f32 {
    f32::from(view.scroll_handle().0.borrow().base_handle.offset().y)
}

fn set_scroll_y(view: &DirView, y: f32) {
    view.scroll_handle()
        .0
        .borrow()
        .base_handle
        .set_offset(point(px(0.0), px(y)));
}

// ----------------------------------------------------------------------
// The gesture, as DirView methods (a field's machine, like rename.rs)
// ----------------------------------------------------------------------

impl DirView {
    /// Left press on the list surface: arm a marquee **if the press landed in
    /// empty space**. A press on a painted row band belongs to the file drag
    /// (`drag.rs`), so it arms nothing; once rows carry their own `on_drag`
    /// they also stop the surface's from starting at all.
    pub(crate) fn arm_marquee(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marquee = None;
        // A marquee while the inline editor is up would fight it for the
        // selection; the editor keeps the view.
        if self.rename.is_some() {
            return;
        }
        let viewport = list_viewport(self);
        if viewport.size.height <= px(0.0) {
            return;
        }
        let origin = ContentPoint::from_window(event.position, viewport, scroll_y(self));
        let occupied = self.flat_rows().len() as f32 * ROW_HEIGHT;
        if origin.y < occupied {
            return;
        }
        let base = if event.modifiers.platform {
            self.selection().selected().clone()
        } else {
            // Explorer (and Finder): a plain press in empty space deselects
            // **immediately**, before any drag — a press that never crosses
            // gpui's drag threshold is a click, and no other handler owns it,
            // so clearing only inside the marquee's own selection pass would
            // leave a click-to-deselect doing nothing at all. It is the gesture
            // users reach for before pressing Delete, so the selection must not
            // survive it under a highlight they believe they dismissed.
            self.selection_mut().clear();
            self.disarm_rename_click();
            cx.notify();
            BTreeSet::new()
        };
        self.marquee = Some(MarqueeState::new(origin, base));
    }

    /// gpui's drag constructor ran: the press has moved past the drag
    /// threshold, so the armed press is now a real marquee.
    pub(crate) fn begin_marquee(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(marquee) = self.marquee.as_mut() else {
            return;
        };
        marquee.current = Some(marquee.origin);
        // A gesture in empty space is not the §0 "slow second click".
        self.disarm_rename_click();
        window.focus(self.focus_handle_ref(), cx);
        cx.notify();
    }

    /// Every mouse move for the life of the drag (this fires outside the
    /// element too, which is what lets the band and the autoscroll follow a
    /// pointer dragged clean out of the list).
    pub(crate) fn update_marquee(
        &mut self,
        event: &DragMoveEvent<MarqueeStart>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marquee.is_none() {
            return;
        }
        // The listener's own element bounds *are* the list viewport (the
        // surface is the list's only child), and they are correct even for a
        // move event delivered while the pointer is elsewhere.
        let viewport = event.bounds;
        let current = ContentPoint::from_window(event.event.position, viewport, scroll_y(self));
        if let Some(marquee) = self.marquee.as_mut() {
            marquee.current = Some(current);
        }
        let scroll = autoscroll_for(
            f32::from(event.event.position.y),
            f32::from(viewport.top()),
            f32::from(viewport.bottom()),
        );
        self.arm_autoscroll(scroll, cx);
        self.apply_marquee_selection(cx);
    }

    /// Mouse-up anywhere ends the gesture; the selection it produced stays.
    /// Dropping the state stops the autoscroll task with it.
    pub(crate) fn end_marquee(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marquee.take().is_some() {
            cx.notify();
        }
    }

    /// Fold the band into the selection: `base ∪ rows(band)`, path-keyed,
    /// through [`crate::selection::SelectionModel`].
    fn apply_marquee_selection(&mut self, cx: &mut Context<Self>) {
        let Some((rect, base, downward)) = self
            .marquee
            .as_ref()
            .and_then(|m| Some((m.rect()?, m.base.clone(), m.downward())))
        else {
            return;
        };
        let rows = self.flat_rows();
        let range = rows_in_rect(rect, ROW_HEIGHT, rows.len());
        let ids: Vec<EntryId> = rows[range.clone()]
            .iter()
            .map(|row| row.entry.id())
            .collect();
        // The cursor follows the moving corner, so a following shift-arrow
        // extends from where the drag ended rather than from wherever the
        // cursor happened to be.
        let focus = if ids.is_empty() {
            None
        } else if downward {
            ids.last().cloned()
        } else {
            ids.first().cloned()
        };
        self.selection_mut().select_marquee(&base, &ids, focus);
        cx.notify();
    }

    /// Keep the single autoscroll slot in step with what the pointer asks
    /// for: respawn on a change of direction/speed, drop it when the pointer
    /// comes back inside, leave it alone otherwise.
    fn arm_autoscroll(&mut self, scroll: Option<AutoScroll>, cx: &mut Context<Self>) {
        let Some(marquee) = self.marquee.as_ref() else {
            return;
        };
        if marquee.scroll == scroll {
            return;
        }
        if scroll.is_none() {
            if let Some(marquee) = self.marquee.as_mut() {
                marquee.scroll = None;
                marquee._autoscroll = None;
            }
            return;
        }
        let spawner = FsContext::global(cx).spawner.clone();
        let task = cx.spawn(async move |this, cx| {
            loop {
                spawner.timer(AUTOSCROLL_TICK).await;
                match this.update(cx, |this, cx| this.tick_marquee_autoscroll(cx)) {
                    Ok(true) => {}
                    _ => break,
                }
            }
        });
        if let Some(marquee) = self.marquee.as_mut() {
            marquee.scroll = scroll;
            marquee._autoscroll = Some(task);
        }
    }

    /// One autoscroll step. Returns `false` when the task should stop.
    fn tick_marquee_autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(scroll) = self.marquee.as_ref().and_then(|m| m.scroll) else {
            return false;
        };
        if self.marquee.as_ref().is_none_or(|m| m.current.is_none()) {
            return false;
        }
        let viewport = list_viewport(self);
        let content = self.flat_rows().len() as f32 * ROW_HEIGHT;
        let max_scroll = (content - f32::from(viewport.size.height)).max(0.0);
        let offset = scroll_y(self);
        let target = (offset - scroll.step()).clamp(-max_scroll, 0.0);
        set_scroll_y(self, target);
        // The pointer stands still in window space, so scrolling the content
        // by `applied` moves the band's moving corner by the same amount in
        // content space (the anchor stays put — the band grows).
        let applied = offset - target;
        if let Some(current) = self
            .marquee
            .as_mut()
            .and_then(|marquee| marquee.current.as_mut())
        {
            current.y += applied;
        }
        self.apply_marquee_selection(cx);
        cx.notify();
        true
    }
}

// ----------------------------------------------------------------------
// Rendering
// ----------------------------------------------------------------------

/// The list's background surface: the element gpui's drag machinery hangs
/// off, the positioning context for the band, and the parent of the row list
/// itself. Built here so all the marquee wiring lives in one place.
pub(crate) fn list_surface(
    view: &DirView,
    body: gpui::AnyElement,
    cx: &mut Context<DirView>,
) -> Stateful<Div> {
    let dir_view = cx.weak_entity();
    div()
        .id("dir-view-list-surface")
        .debug_selector(|| "dir-view-list-surface".to_string())
        // Positioning context for the absolutely-placed band.
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .on_mouse_down(MouseButton::Left, cx.listener(DirView::arm_marquee))
        .on_drag(MarqueeStart, move |_, _, window, cx| {
            dir_view
                .update(cx, |view, cx| view.begin_marquee(window, cx))
                .ok();
            cx.new(|_| MarqueeGhost)
        })
        .on_drag_move(cx.listener(DirView::update_marquee))
        .on_mouse_up(MouseButton::Left, cx.listener(DirView::end_marquee))
        .on_mouse_up_out(MouseButton::Left, cx.listener(DirView::end_marquee))
        .child(body)
        .children(render_marquee(view))
}

/// The band itself: an absolutely-positioned translucent accent rectangle,
/// clamped to the viewport (the band lives in content space and may run well
/// past both edges).
fn render_marquee(view: &DirView) -> Option<Div> {
    let rect = view.marquee.as_ref()?.rect()?;
    let viewport = list_viewport(view);
    let (width, height) = (
        f32::from(viewport.size.width),
        f32::from(viewport.size.height),
    );
    if height <= 0.0 {
        return None;
    }
    let scroll = scroll_y(view);
    let (top, bottom) = (rect.top + scroll, rect.bottom + scroll);
    if bottom <= 0.0 || top >= height {
        return None;
    }
    let (top, bottom) = (top.max(0.0), bottom.min(height));
    let (left, right) = (
        rect.left.clamp(0.0, width),
        rect.right
            .clamp(0.0, width)
            .max(rect.left.clamp(0.0, width)),
    );
    let theme = view.theme();
    Some(
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px((right - left).max(MIN_BAND_PX)))
            .h(px((bottom - top).max(MIN_BAND_PX)))
            .bg(theme.accent.opacity(MARQUEE_FILL_ALPHA))
            .border_1()
            .border_color(theme.accent.opacity(MARQUEE_BORDER_ALPHA)),
    )
}

#[cfg(test)]
mod tests {
    //! §9 marquee rows. The arithmetic first, hard and headlessly — it is the
    //! part virtualization makes impossible to eyeball: inverted drags,
    //! partial-row overlap, exact boundaries, scrolled content, empty
    //! listings. Then the gesture itself, driven through real simulated mouse
    //! input on a laid-out window.

    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::pane::Pane;
    use crate::theme::Theme;
    use fs_core::{FakeVfs, Spawner};
    use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
    use serde_json::{Value, json};

    const H: f32 = 24.0;

    fn at(x: f32, y: f32) -> ContentPoint {
        ContentPoint { x, y }
    }

    fn band(a: ContentPoint, b: ContentPoint) -> MarqueeRect {
        MarqueeRect::between(a, b)
    }

    // ---------------- the arithmetic ----------------

    #[test]
    fn marquee_rect_normalizes_either_drag_direction() {
        let down_right = band(at(10.0, 20.0), at(40.0, 90.0));
        let up_left = band(at(40.0, 90.0), at(10.0, 20.0));
        assert_eq!(down_right, up_left, "a drag and its mirror are one band");
        assert_eq!(
            down_right,
            MarqueeRect {
                left: 10.0,
                top: 20.0,
                right: 40.0,
                bottom: 90.0
            }
        );
        assert_eq!(down_right.width(), 30.0);
        assert_eq!(down_right.height(), 70.0);
    }

    #[test]
    fn rows_in_rect_takes_any_overlap_however_small() {
        // A band wholly inside row 1's [24, 48) band selects exactly row 1.
        assert_eq!(rows_in_rect(band(at(0.0, 30.0), at(0.0, 35.0)), H, 5), 1..2);
        // One pixel past row 0's bottom edge reaches into row 1.
        assert_eq!(
            rows_in_rect(band(at(0.0, 0.0), at(0.0, 24.5)), H, 5),
            0..2,
            "partial overlap counts"
        );
        // Spanning three whole bands selects three rows.
        assert_eq!(rows_in_rect(band(at(0.0, 0.0), at(0.0, 72.0)), H, 5), 0..3);
    }

    #[test]
    fn rows_in_rect_stops_at_an_exact_row_boundary() {
        // Dragging down to *exactly* the top of row 1 keeps row 1 out.
        assert_eq!(rows_in_rect(band(at(0.0, 0.0), at(0.0, 24.0)), H, 5), 0..1);
        // ...and starting exactly on it keeps row 0 out.
        assert_eq!(rows_in_rect(band(at(0.0, 24.0), at(0.0, 48.0)), H, 5), 1..2);
        // A degenerate band on a boundary selects nothing; inside a row it
        // selects that row.
        assert_eq!(rows_in_rect(band(at(0.0, 24.0), at(0.0, 24.0)), H, 5), 1..1);
        assert_eq!(rows_in_rect(band(at(0.0, 30.0), at(0.0, 30.0)), H, 5), 1..2);
    }

    #[test]
    fn rows_in_rect_is_indifferent_to_the_horizontal_drag_direction() {
        // Details-list rows are full-width, so only the vertical span picks
        // rows: a leftward drag selects the same rows as its mirror, and a
        // band far off to the right of the columns still selects them.
        let rightward = rows_in_rect(band(at(0.0, 10.0), at(900.0, 60.0)), H, 5);
        let leftward = rows_in_rect(band(at(900.0, 60.0), at(0.0, 10.0)), H, 5);
        let far_right = rows_in_rect(band(at(800.0, 10.0), at(900.0, 60.0)), H, 5);
        assert_eq!(rightward, 0..3);
        assert_eq!(leftward, rightward);
        assert_eq!(far_right, rightward);
    }

    #[test]
    fn rows_in_rect_clamps_to_the_listing() {
        // Dragging up out of the top and down past the end both clamp.
        assert_eq!(
            rows_in_rect(band(at(0.0, -400.0), at(0.0, 4_000.0)), H, 5),
            0..5
        );
        // Entirely above the first row, and entirely below the last: empty.
        assert_eq!(
            rows_in_rect(band(at(0.0, -80.0), at(0.0, -10.0)), H, 5),
            0..0
        );
        assert_eq!(
            rows_in_rect(band(at(0.0, 130.0), at(0.0, 400.0)), H, 5),
            5..5,
            "5 rows end at y=120, so a band below them selects nothing"
        );
    }

    #[test]
    fn rows_in_rect_on_an_empty_listing_is_empty() {
        assert_eq!(
            rows_in_rect(band(at(0.0, 0.0), at(500.0, 500.0)), H, 0),
            0..0
        );
        // A degenerate row height can't divide by zero into a panic either.
        assert_eq!(
            rows_in_rect(band(at(0.0, 0.0), at(0.0, 50.0)), 0.0, 5),
            0..0
        );
    }

    #[test]
    fn content_point_undoes_the_viewport_origin_and_the_scroll() {
        let viewport =
            Bounds::from_corners(point(px(60.0), px(100.0)), point(px(660.0), px(340.0)));
        // Unscrolled: the pointer's offset inside the viewport is the content
        // offset.
        let top = ContentPoint::from_window(point(px(160.0), px(150.0)), viewport, 0.0);
        assert_eq!(top, at(100.0, 50.0));

        // Scrolled ten rows down (gpui stores that as a negative offset), the
        // same pixel is 240px further into the content.
        let scrolled = ContentPoint::from_window(point(px(160.0), px(150.0)), viewport, -240.0);
        assert_eq!(scrolled, at(100.0, 290.0));
        assert_eq!(
            rows_in_rect(band(scrolled, scrolled), H, 40),
            12..13,
            "y=290 lands inside row 12, which uniform_list never painted"
        );
    }

    #[test]
    fn autoscroll_for_picks_a_speed_from_edge_proximity() {
        let (top, bottom) = (100.0, 500.0);
        assert_eq!(autoscroll_for(300.0, top, bottom), None, "the middle");
        // Inside the slow band of either edge.
        assert_eq!(
            autoscroll_for(bottom - 10.0, top, bottom),
            Some(AutoScroll::DownSlow)
        );
        assert_eq!(
            autoscroll_for(top + 10.0, top, bottom),
            Some(AutoScroll::UpSlow)
        );
        // At the edge, and dragged clean outside it.
        assert_eq!(
            autoscroll_for(bottom, top, bottom),
            Some(AutoScroll::DownFast)
        );
        assert_eq!(
            autoscroll_for(bottom + 200.0, top, bottom),
            Some(AutoScroll::DownFast)
        );
        assert_eq!(autoscroll_for(top, top, bottom), Some(AutoScroll::UpFast));
        assert_eq!(
            autoscroll_for(top - 200.0, top, bottom),
            Some(AutoScroll::UpFast)
        );
        // Just outside the slow band is not autoscroll at all.
        assert_eq!(
            autoscroll_for(top + AUTOSCROLL_SLOW_BAND, top, bottom),
            None
        );
    }

    #[test]
    fn autoscroll_steps_point_the_way_they_are_named() {
        assert!(AutoScroll::UpSlow.step() < 0.0);
        assert!(AutoScroll::UpFast.step() < AutoScroll::UpSlow.step());
        assert!(AutoScroll::DownSlow.step() > 0.0);
        assert!(AutoScroll::DownFast.step() > AutoScroll::DownSlow.step());
    }

    #[test]
    fn autoscroll_for_prefers_the_nearer_edge_in_a_tiny_viewport() {
        // A viewport shorter than two slow bands: every pixel is near both
        // edges, so the nearer one has to win rather than the first branch.
        let (top, bottom) = (0.0, 20.0);
        assert_eq!(autoscroll_for(4.0, top, bottom), Some(AutoScroll::UpSlow));
        assert_eq!(
            autoscroll_for(16.0, top, bottom),
            Some(AutoScroll::DownSlow)
        );
    }

    // ---------------- the gesture ----------------

    /// `n` files named `f000.txt`, `f001.txt`, ... — a listing whose row
    /// order the tests can predict from an index.
    fn files(n: usize) -> Value {
        let mut map = serde_json::Map::new();
        for i in 0..n {
            map.insert(format!("f{i:03}.txt"), json!("x"));
        }
        Value::Object(map)
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree("/root", files(10));
            vfs.insert_tree("/root/big", files(200));
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

    /// `/root` open (one folder `big` then `f000.txt`..`f009.txt`), the
    /// details view laid out.
    fn open_root(
        cx: &mut TestAppContext,
    ) -> (Entity<Pane>, Entity<DirView>, &mut VisualTestContext) {
        init_test(cx);
        let (pane, cx) = cx.add_window_view(|window, cx| Pane::new(Theme::dark(), window, cx));
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        (pane, dir_view, cx)
    }

    fn selected(dir_view: &Entity<DirView>, cx: &mut VisualTestContext) -> Vec<PathBuf> {
        dir_view.read_with(cx, |dir_view, _| dir_view.selection().selected_paths())
    }

    fn row_count(dir_view: &Entity<DirView>, cx: &mut VisualTestContext) -> usize {
        dir_view.read_with(cx, |dir_view, _| dir_view.flat_rows().len())
    }

    fn viewport(dir_view: &Entity<DirView>, cx: &mut VisualTestContext) -> Bounds<Pixels> {
        dir_view.read_with(cx, |dir_view, _| list_viewport(dir_view))
    }

    fn scroll_offset(dir_view: &Entity<DirView>, cx: &mut VisualTestContext) -> f32 {
        dir_view.read_with(cx, |dir_view, _| scroll_y(dir_view))
    }

    /// A window-space point at content-space `y`, in the middle of the list.
    fn window_point(viewport: Bounds<Pixels>, content_y: f32) -> Point<Pixels> {
        point(viewport.left() + px(40.0), viewport.top() + px(content_y))
    }

    /// Press in empty space at `from`, drag to `to`, and (unless `hold` says
    /// otherwise) release. Two moves: the first trips gpui's drag threshold
    /// and creates the drag, the second is the first one the marquee sees.
    fn drag(
        cx: &mut VisualTestContext,
        from: Point<Pixels>,
        to: Point<Pixels>,
        modifiers: Modifiers,
        release: bool,
    ) {
        cx.simulate_mouse_down(from, MouseButton::Left, modifiers);
        cx.simulate_mouse_move(from + point(px(6.0), px(6.0)), MouseButton::Left, modifiers);
        cx.simulate_mouse_move(to, MouseButton::Left, modifiers);
        if release {
            cx.simulate_mouse_up(to, MouseButton::Left, modifiers);
        }
    }

    #[gpui::test]
    fn a_background_drag_selects_the_rows_it_crosses(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        assert_eq!(row_count(&dir_view, cx), 11, "big/ + ten files");
        let vp = viewport(&dir_view, cx);

        // Rows end at y = 11 * 24 = 264; press below them, drag up to y=100.
        // The band [100, 270] covers rows floor(100/24)=4 .. ceil(270/24)=12
        // clamped to 11 — f003.txt through f009.txt.
        drag(
            cx,
            window_point(vp, 270.0),
            window_point(vp, 100.0),
            Modifiers::none(),
            true,
        );

        let expected: Vec<PathBuf> = (3..10)
            .map(|i| PathBuf::from(format!("/root/f{i:03}.txt")))
            .collect();
        assert_eq!(selected(&dir_view, cx), expected);
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.marquee.is_none(), "the release ended the gesture");
            assert_eq!(
                dir_view.cursor().map(|id| id.0.to_path_buf()),
                Some(PathBuf::from("/root/f003.txt")),
                "an upward drag leaves the cursor on the band's top row"
            );
        });
    }

    #[gpui::test]
    fn a_plain_marquee_replaces_and_a_cmd_marquee_unions(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        let vp = viewport(&dir_view, cx);
        let preselected = Path::new("/root/f000.txt");

        // Plain drag over the last two rows: the earlier selection is gone.
        dir_view.update(cx, |dir_view, cx| dir_view.select_paths(&[preselected], cx));
        drag(
            cx,
            window_point(vp, 270.0),
            window_point(vp, 220.0),
            Modifiers::none(),
            true,
        );
        assert_eq!(
            selected(&dir_view, cx),
            vec![
                PathBuf::from("/root/f008.txt"),
                PathBuf::from("/root/f009.txt")
            ],
            "a plain marquee replaces the selection"
        );

        // Same drag with cmd held: the band is added to what was selected.
        dir_view.update(cx, |dir_view, cx| dir_view.select_paths(&[preselected], cx));
        drag(
            cx,
            window_point(vp, 270.0),
            window_point(vp, 220.0),
            Modifiers::command(),
            true,
        );
        assert_eq!(
            selected(&dir_view, cx),
            vec![
                PathBuf::from("/root/f000.txt"),
                PathBuf::from("/root/f008.txt"),
                PathBuf::from("/root/f009.txt")
            ],
            "a cmd marquee unions with the pre-gesture selection"
        );
    }

    #[gpui::test]
    fn shrinking_the_band_gives_back_only_what_the_band_added(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        let vp = viewport(&dir_view, cx);
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(&[Path::new("/root/f000.txt")], cx)
        });

        // cmd-drag up over the tail, then back down so the band covers less.
        drag(
            cx,
            window_point(vp, 270.0),
            window_point(vp, 150.0),
            Modifiers::command(),
            false,
        );
        assert!(selected(&dir_view, cx).len() > 3, "the band grabbed rows");
        cx.simulate_mouse_move(
            window_point(vp, 250.0),
            MouseButton::Left,
            Modifiers::command(),
        );
        assert_eq!(
            selected(&dir_view, cx),
            vec![
                PathBuf::from("/root/f000.txt"),
                PathBuf::from("/root/f009.txt")
            ],
            "rows the band let go of are deselected; the pre-gesture one stays"
        );
        cx.simulate_mouse_up(
            window_point(vp, 250.0),
            MouseButton::Left,
            Modifiers::command(),
        );
    }

    #[gpui::test]
    fn the_band_is_anchored_in_content_space(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        let vp = viewport(&dir_view, cx);

        // Drag up and to the left out of the list's left edge: the band that
        // the renderer draws (and the hit test uses) is the normalized rect
        // between the *press* point and the pointer, in content space.
        cx.simulate_mouse_down(
            window_point(vp, 270.0),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            window_point(vp, 262.0),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(
            point(vp.left() - px(30.0), vp.top() + px(120.0)),
            MouseButton::Left,
            Modifiers::none(),
        );

        dir_view.read_with(cx, |dir_view, _| {
            let rect = dir_view
                .marquee
                .as_ref()
                .expect("dragging")
                .rect()
                .expect("the band exists once the drag started");
            assert_eq!(
                rect,
                MarqueeRect {
                    left: -30.0,
                    top: 120.0,
                    right: 40.0,
                    bottom: 270.0,
                },
                "anchored at the press, normalized, and unclamped in content space"
            );
        });
        cx.simulate_mouse_up(
            point(vp.left() - px(30.0), vp.top() + px(120.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        dir_view.read_with(cx, |dir_view, _| assert!(dir_view.marquee.is_none()));
    }

    // A press in empty space that never becomes a drag is a **click**, and
    // every file manager deselects on it — it is the gesture users reach for
    // before pressing Delete, so a selection that survived it would keep
    // acting under a highlight the user believes they dismissed. Nothing else
    // owns this press (the surface has no `on_click`), so the marquee's arm is
    // where it has to happen.
    #[gpui::test]
    fn a_click_in_empty_space_clears_the_selection(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        let vp = viewport(&dir_view, cx);
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(
                &[Path::new("/root/f000.txt"), Path::new("/root/f001.txt")],
                cx,
            )
        });
        assert_eq!(selected(&dir_view, cx).len(), 2);

        // Press and release below the last row, never crossing the drag
        // threshold: no band is ever created.
        let empty = window_point(vp, 270.0);
        cx.simulate_mouse_down(empty, MouseButton::Left, Modifiers::none());
        dir_view.read_with(cx, |dir_view, _| {
            assert!(
                dir_view
                    .marquee
                    .as_ref()
                    .is_some_and(|m| m.rect().is_none()),
                "armed, but not yet a band"
            );
        });
        assert!(
            selected(&dir_view, cx).is_empty(),
            "the press itself deselects (press-time, as Explorer does)"
        );
        cx.simulate_mouse_up(empty, MouseButton::Left, Modifiers::none());
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.marquee.is_none());
            assert_eq!(dir_view.cursor(), None, "and the cursor goes with it");
        });

        // A `cmd`-press in empty space is additive: it keeps the selection to
        // union a band onto.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.select_paths(&[Path::new("/root/f000.txt")], cx)
        });
        cx.simulate_mouse_down(empty, MouseButton::Left, Modifiers::command());
        assert_eq!(
            selected(&dir_view, cx),
            vec![PathBuf::from("/root/f000.txt")],
            "cmd keeps what was selected"
        );
        cx.simulate_mouse_up(empty, MouseButton::Left, Modifiers::command());
    }

    #[gpui::test]
    fn a_press_on_a_row_starts_no_marquee(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        let vp = viewport(&dir_view, cx);

        // y=60 is inside row 2's band: this press belongs to the file drag.
        cx.simulate_mouse_down(window_point(vp, 60.0), MouseButton::Left, Modifiers::none());
        dir_view.read_with(cx, |dir_view, _| assert!(dir_view.marquee.is_none()));
        cx.simulate_mouse_move(
            window_point(vp, 200.0),
            MouseButton::Left,
            Modifiers::none(),
        );
        dir_view.read_with(cx, |dir_view, _| {
            assert!(
                dir_view.marquee.is_none(),
                "no band, and no selection churn"
            );
        });
        assert!(selected(&dir_view, cx).is_empty());
        cx.simulate_mouse_up(
            window_point(vp, 200.0),
            MouseButton::Left,
            Modifiers::none(),
        );
    }

    #[gpui::test]
    fn edge_autoscroll_advances_the_band_on_fake_time(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        let vp = viewport(&dir_view, cx);

        // Start in the empty space below the eleven rows and begin dragging.
        drag(
            cx,
            window_point(vp, 270.0),
            window_point(vp, 200.0),
            Modifiers::none(),
            false,
        );

        // Now `big/`'s two hundred children splice in beneath it, so the
        // content is suddenly far taller than the viewport — the real way a
        // marquee in a full-width details list ends up with somewhere to
        // scroll.
        dir_view.update(cx, |dir_view, cx| {
            dir_view.toggle_expanded(Path::new("/root/big"), cx)
        });
        cx.run_until_parked();
        assert_eq!(row_count(&dir_view, cx), 211);
        assert_eq!(scroll_offset(&dir_view, cx), 0.0, "not scrolled yet");

        // Drag the pointer clean out of the bottom edge: fast autoscroll.
        cx.simulate_mouse_move(
            point(vp.left() + px(40.0), vp.bottom() + px(50.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        let before = selected(&dir_view, cx).len();

        cx.executor().advance_clock(AUTOSCROLL_TICK);
        cx.run_until_parked();
        let after_one = scroll_offset(&dir_view, cx);
        assert_eq!(
            after_one,
            -AutoScroll::DownFast.step(),
            "one tick scrolls one fast step"
        );

        cx.executor().advance_clock(AUTOSCROLL_TICK * 3);
        cx.run_until_parked();
        assert!(
            scroll_offset(&dir_view, cx) < after_one,
            "the task keeps ticking while the pointer stays out"
        );
        assert!(
            selected(&dir_view, cx).len() > before,
            "the band grows over the rows it scrolls onto"
        );

        // Releasing drops the state and with it the one autoscroll slot.
        cx.simulate_mouse_up(
            point(vp.left() + px(40.0), vp.bottom() + px(50.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        let settled = scroll_offset(&dir_view, cx);
        cx.executor().advance_clock(AUTOSCROLL_TICK * 4);
        cx.run_until_parked();
        assert_eq!(
            scroll_offset(&dir_view, cx),
            settled,
            "no autoscroll survives the drag"
        );
        dir_view.read_with(cx, |dir_view, _| assert!(dir_view.marquee.is_none()));
    }

    #[gpui::test]
    fn a_marquee_never_starts_while_the_inline_editor_is_up(cx: &mut TestAppContext) {
        let (_pane, dir_view, cx) = open_root(cx);
        let vp = viewport(&dir_view, cx);
        dir_view.update(cx, |dir_view, cx| {
            dir_view.set_cursor(Some(EntryId(Arc::from(Path::new("/root/f000.txt")))), cx)
        });
        cx.update(|window, cx| {
            let handle = dir_view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| assert!(dir_view.rename.is_some()));

        drag(
            cx,
            window_point(vp, 270.0),
            window_point(vp, 100.0),
            Modifiers::none(),
            true,
        );
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.marquee.is_none(), "the editor keeps the view");
        });
    }
}
