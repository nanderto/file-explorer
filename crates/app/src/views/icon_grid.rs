//! Icon-grid rendering for [`DirView`] (ARCHITECTURE.md §8 "Icon grid",
//! milestone M4).
//!
//! The list is a `uniform_list` whose **items are grid rows**, not entries:
//! each item lays out up to `cols` fixed-size tiles, so `ceil(n / cols)`
//! items cover the whole listing and virtualization survives untouched — a
//! 50k-entry directory paints the handful of rows the viewport can show, the
//! same as the details list. `cols` is recomputed from the pane's painted
//! width ([`cols_for_width`]), which is why every piece of geometry here is a
//! **pure function of `(cols, len)`**: the cursor arithmetic, the marquee hit
//! test and the drop-target hit test all have to agree with the layout, and
//! all of them can be unit-tested without a window.
//!
//! What the grid deliberately does *not* own: selection (it renders
//! [`DirView`]'s one path-keyed `SelectionModel`, so switching view mode
//! preserves the selection exactly), the clipboard's cut-dimming rule, the
//! drag payload, the drop-target arming, and the inline rename editor's
//! wiring — each of those is the same single implementation the details list
//! uses. Every color comes from the [`Theme`].

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    ClickEvent, Context, IntoElement, RenderImage, SharedString, Stateful, UniformList, div, img,
    prelude::*, px, uniform_list,
};

use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::drag;
use crate::marquee::MarqueeRect;
use crate::theme::Theme;

/// Tile footprint, in pixels. Fixed (like the details list's row height)
/// because both `uniform_list` and every hit test below need the geometry to
/// be arithmetic rather than measurement.
pub(crate) const TILE_WIDTH: f32 = 96.0;
/// One `uniform_list` item is one grid row, so this is the item height.
pub(crate) const TILE_HEIGHT: f32 = 88.0;
/// The square image slot at the top of a tile — the fixed box a decoded
/// thumbnail is fitted into (see [`tile_image`]), and the logical size
/// [`crate::thumbnails::THUMBNAIL_PX`] is derived from. Because the slot's
/// size never depends on whether a preview has arrived, no tile geometry
/// (and therefore no hit test above) changes when one does.
pub(crate) const ICON_PX: f32 = 48.0;
/// Selection tint alpha, matching the details list's selected row.
const SELECTION_ALPHA: f32 = 0.35;
/// Row opacity for cut-pending entries (plan §3: "cut items render dimmed").
const CUT_DIM_OPACITY: f32 = 0.5;

// ----------------------------------------------------------------------
// Geometry — pure functions of (cols, len). Everything that has to agree
// with the painted layout lives here and is unit-tested directly.
// ----------------------------------------------------------------------

/// How many tiles fit across `width` pixels. Always at least one: a pane
/// narrower than a tile still has to show its entries (they clip rather than
/// vanish), and a zero column count would make every index computation below
/// divide by zero.
pub(crate) fn cols_for_width(width: f32) -> usize {
    if !width.is_finite() || width < TILE_WIDTH {
        return 1;
    }
    ((width / TILE_WIDTH).floor() as usize).max(1)
}

/// Grid rows needed for `len` tiles — `ceil(len / cols)`, and the
/// `uniform_list` item count.
pub(crate) fn grid_row_count(len: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    len.div_ceil(cols)
}

/// The tile indices on grid row `row`. The final row is ragged: it stops at
/// `len` rather than padding, and a row past the end is empty (never a panic
/// — `uniform_list` can ask for a range built before the last re-projection).
pub(crate) fn row_items(row: usize, cols: usize, len: usize) -> Range<usize> {
    let cols = cols.max(1);
    let start = (row * cols).min(len);
    let end = (start + cols).min(len);
    start..end
}

/// One keyboard step across the grid. `Up`/`Down` are `±cols`, `Left`/`Right`
/// are `±1` (§8 "2D keyboard nav = index arithmetic").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GridStep {
    Left,
    Right,
    Up,
    Down,
}

impl GridStep {
    /// The signed index delta this step applies in a `cols`-wide grid — the
    /// form the *range-extending* (`shift-arrow`) path wants, because
    /// [`crate::selection::SelectionModel::select_range_to`] needs a plain
    /// clamped index, not the edge rules [`step_index`] applies.
    pub(crate) fn delta(self, cols: usize) -> isize {
        let cols = cols.max(1) as isize;
        match self {
            GridStep::Left => -1,
            GridStep::Right => 1,
            GridStep::Up => -cols,
            GridStep::Down => cols,
        }
    }
}

/// Move the cursor one step from `ix` in a `cols`-wide grid of `len` tiles.
///
/// Edge rules — all of them "stay put", never wrap, because a wrapping cursor
/// in a file manager is how a user deletes the wrong file:
///
/// * `Left` on the first tile, `Right` on the last: no move (`Left`
///   deliberately *does* cross a row boundary backwards, and `Right`
///   forwards, which is what reading order means).
/// * `Up` from the first row: no move.
/// * `Down` from the last row: no move.
/// * `Down` from a row above a **ragged** last row whose cell in this column
///   does not exist: the last tile. Explorer's behavior, and the alternative
///   (refusing to move) traps the cursor in the second-to-last row.
///
/// Out-of-range inputs are clamped rather than trusted: `cols` can change
/// under the cursor between two frames (a pane resize), so `ix` may be stale.
pub(crate) fn step_index(ix: usize, len: usize, cols: usize, step: GridStep) -> usize {
    if len == 0 {
        return 0;
    }
    let cols = cols.max(1);
    let ix = ix.min(len - 1);
    let rows = grid_row_count(len, cols);
    match step {
        GridStep::Left => ix.saturating_sub(1),
        GridStep::Right => (ix + 1).min(len - 1),
        GridStep::Up => {
            if ix >= cols {
                ix - cols
            } else {
                ix
            }
        }
        GridStep::Down => {
            if ix + cols < len {
                ix + cols
            } else if ix / cols + 1 == rows {
                ix
            } else {
                len - 1
            }
        }
    }
}

/// The tile under a point in **content** space (the marquee's coordinate
/// system: viewport-relative x, scroll-adjusted y), or `None` for empty space
/// — past the last row, past the last column of a ragged row, or outside the
/// grid's own column band on the right of a full row.
pub(crate) fn tile_at(x: f32, y: f32, cols: usize, len: usize) -> Option<usize> {
    if len == 0 || x < 0.0 || y < 0.0 {
        return None;
    }
    let cols = cols.max(1);
    let col = (x / TILE_WIDTH).floor() as usize;
    if col >= cols {
        return None;
    }
    let row = (y / TILE_HEIGHT).floor() as usize;
    let ix = row.checked_mul(cols)?.checked_add(col)?;
    (ix < len).then_some(ix)
}

/// Every tile a marquee band touches, in index order.
///
/// The rule matches the details list's ([`crate::marquee::rows_in_rect`]):
/// any non-zero overlap selects a tile, an edge landing exactly on a tile
/// boundary does not reach into the next one, and a degenerate (zero-area)
/// band selects the single tile it sits inside. Unlike the list, the result
/// is not contiguous — a band spanning two rows of a 5-wide grid skips the
/// columns it misses — so this returns the indices rather than a range.
pub(crate) fn tiles_in_rect(rect: MarqueeRect, cols: usize, len: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let cols = cols.max(1);
    let rows = grid_row_count(len, cols);
    let band = |lo: f32, hi: f32, size: f32, count: usize| -> Range<usize> {
        let extent = count as f32 * size;
        let lo = lo.clamp(0.0, extent);
        let hi = hi.clamp(0.0, extent);
        let start = ((lo / size).floor() as usize).min(count);
        let end = ((hi / size).ceil() as usize).clamp(start, count);
        start..end
    };
    let row_band = band(rect.top, rect.bottom, TILE_HEIGHT, rows);
    let col_band = band(rect.left, rect.right, TILE_WIDTH, cols);
    let mut hit = Vec::new();
    for row in row_band {
        for col in col_band.clone() {
            let ix = row * cols + col;
            if ix < len {
                hit.push(ix);
            }
        }
    }
    hit
}

// ----------------------------------------------------------------------
// Rendering
// ----------------------------------------------------------------------

/// The virtualized grid: one `uniform_list` item per grid row, `cols` tiles
/// per item. Shares the DirView's scroll handle, so `scroll_to_item` (called
/// with a *grid row* index — see `DirView::scroll_cursor_into_view`) and the
/// marquee's content-space arithmetic both keep working.
pub(crate) fn render_grid(
    dir_view: &DirView,
    cols: usize,
    cx: &mut Context<DirView>,
) -> UniformList {
    let len = dir_view.flat_rows().len();
    uniform_list(
        "icon-grid-rows",
        grid_row_count(len, cols),
        cx.processor(move |this, rows: Range<usize>, window, cx| {
            // Runs after gpui has written this frame's real list bounds onto
            // the shared scroll handle, so it is the earliest place a resize
            // can be noticed — see `DirView::note_painted_grid_cols`. (The
            // thumbnail window is *not* derived here: gpui calls this
            // processor twice a frame with `0..1` merely to measure an item,
            // and a request window that flipped like that would cancel its
            // own fetch on every repaint. `DirView::render` drives it.)
            this.note_painted_grid_cols(cols, window, cx);
            rows.map(|row| {
                let items = row_items(row, cols, this.flat_rows().len());
                let mut line = div()
                    .flex()
                    .items_start()
                    .h(px(TILE_HEIGHT))
                    .w_full()
                    .child(div().w(px(0.0)).flex_none());
                for ix in items {
                    let Some(projected) = this.flat_rows().get(ix).cloned() else {
                        continue;
                    };
                    line = line.child(render_tile(this, &projected, ix, cx));
                }
                line
            })
            .collect::<Vec<_>>()
        }),
    )
    .flex_1()
    .track_scroll(dir_view.scroll_handle())
}

/// One tile: image slot over a truncating label, with the selection tint,
/// cut-dimming, drop-target tint, click/double-click dispatch and drag
/// payload the details row has — the same single implementations, applied to
/// a different box.
fn render_tile(
    this: &mut DirView,
    row: &crate::dir_view::ProjectedRow,
    ix: usize,
    cx: &mut Context<DirView>,
) -> Stateful<gpui::Div> {
    // §4c: the tile of the entry being renamed hosts the editor instead of
    // its label, exactly as the details row does.
    if this
        .rename
        .as_ref()
        .is_some_and(|rename| *rename.target() == row.entry.id())
    {
        return render_rename_tile(this, row, ix, cx);
    }

    let entry = &row.entry;
    let theme = this.theme().clone();
    let selected = this.selection().is_selected(&entry.id());
    // Cut-pending entries render dimmed (§4b, same clipboard check as the
    // details row).
    let cut_pending = FsContext::global(cx).clipboard.is_cut(&entry.path);
    let name: SharedString = SharedString::new(entry.name.clone());
    let click_entry = entry.clone();
    // Resolved before the element chain is built, because it needs `this`
    // mutably (the cache promotes on read) — and `None` simply means the
    // placeholder, so an arriving preview swaps in without reflowing.
    let thumbnail = this.thumbnail_image(entry);
    // M6b tag dots, beside the label rather than under it: the tile's height is
    // fixed (`TILE_HEIGHT`) and every hit test in the grid is arithmetic
    // against that lattice, so an arriving tag set must not add a row to it.
    let tags = crate::tags::tag_dots(this.entry_tags(&entry.path));

    let mut tile = tile_frame(entry, ix)
        .cursor_pointer()
        .text_size(px(11.0))
        .text_color(if entry.hidden {
            theme.muted
        } else {
            theme.text
        })
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            window.focus(this.focus_handle_ref(), cx);
            // The same dispatcher the details row uses: fast double-click
            // opens, cmd/shift click select, a slow second click arms rename.
            this.handle_row_click(
                &click_entry,
                event.modifiers(),
                event.click_count(),
                window,
                cx,
            );
        }))
        .child(tile_image(entry, thumbnail, &theme))
        .child(
            div()
                .w(px(TILE_WIDTH - 12.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(3.0))
                .child(div().min_w(px(0.0)).truncate().child(name))
                .children(tags),
        );

    if selected {
        tile = tile.bg(theme.accent.opacity(SELECTION_ALPHA));
    }
    // §8 drag & drop: this tile is the armed folder drop target. Background
    // only, painted over the selection tint — arming a highlight never moves
    // a tile.
    if drag::row_is_drop_target(this, &entry.path, cx) {
        tile = tile.bg(drag::drop_row_color(&theme));
    }
    if cut_pending {
        tile = tile.opacity(CUT_DIM_OPACITY);
    }
    // §8: the tile starts the file drag, with the same payload rule as the
    // details row (a grabbed selected tile drags the whole selection, an
    // unselected one drags itself), and the same outbound Finder payload.
    if this.rename.is_none() {
        let dragged = this.drag_payload(entry.path.clone());
        let ghost_label = dragged.label();
        let ghost_theme = theme.clone();
        let view = cx.weak_entity();
        tile = tile
            .on_drag(dragged, move |_, _, _, cx| {
                drag::ghost(ghost_label.clone(), ghost_theme.clone(), cx)
            })
            .external_drag_payload(move |dragged: &drag::DraggedEntries, _, cx| {
                let entries = view
                    .read_with(cx, |view, _| view.external_drag_entries(dragged))
                    .ok()?;
                drag::external_payload(&entries)
            });
    }
    tile
}

/// The tile box itself. **Path-keyed, not index-keyed**, for the same reason
/// the details row is (invariant #2): gpui persists a stateful element's
/// pending mouse-down by element id, and an index id would hand a press on
/// tile `n` to whatever entry occupies index `n` after a re-projection —
/// turning a drag into a filesystem move of a file the user never touched.
/// The *selector* stays index-based: it names a position on screen.
fn tile_frame(entry: &fs_core::FileEntry, ix: usize) -> Stateful<gpui::Div> {
    div()
        .id(gpui::ElementId::Path(entry.path.clone()))
        .debug_selector(|| format!("dir-tile-{ix}"))
        .flex()
        .flex_col()
        .flex_none()
        .items_center()
        .justify_start()
        .gap(px(4.0))
        .w(px(TILE_WIDTH))
        .h(px(TILE_HEIGHT))
        .px(px(6.0))
        .py(px(6.0))
        .rounded(px(4.0))
}

/// The tile's fixed-size image slot: the decoded thumbnail when there is one,
/// the type glyph (folder vs file, from the theme) when there is not.
///
/// The image is *fitted* inside the slot rather than filling it, so a portrait
/// preview is not stretched (`Platform::thumbnail` preserves aspect ratio, so
/// a thumbnail is usually smaller than the slot on one axis). Thumbnails are
/// requested — and cancelled on scroll-away — by [`crate::thumbnails`]; this
/// function only paints what has already arrived.
fn tile_image(
    entry: &fs_core::FileEntry,
    thumbnail: Option<Arc<RenderImage>>,
    theme: &Theme,
) -> impl IntoElement + use<> {
    let slot = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(ICON_PX))
        .h(px(ICON_PX))
        .rounded(px(3.0));
    match thumbnail {
        Some(image) => slot
            .child(img(image).max_w(px(ICON_PX)).max_h(px(ICON_PX)))
            .into_any_element(),
        None => slot
            .bg(theme
                .accent
                .opacity(if entry.is_dir_like() { 0.20 } else { 0.10 }))
            .text_size(px(20.0))
            .text_color(theme.muted)
            .child(SharedString::new_static(if entry.is_dir_like() {
                "▣"
            } else {
                "▢"
            }))
            .into_any_element(),
    }
}

/// The tile of the entry being renamed (§4c): the label slot becomes the
/// vendored editor (or the pending name once `Confirm` submitted the op).
/// The dispatch wiring is [`crate::rename::with_editor_actions`], shared with
/// the details row.
fn render_rename_tile(
    this: &mut DirView,
    row: &crate::dir_view::ProjectedRow,
    ix: usize,
    cx: &mut Context<DirView>,
) -> Stateful<gpui::Div> {
    let theme = this.theme().clone();
    let thumbnail = this.thumbnail_image(&row.entry);
    let rename = this
        .rename
        .as_ref()
        .expect("render_rename_tile requires an active rename");
    let input = rename.input().clone();
    let processing = rename.processing().cloned();
    let error = rename.error().cloned();

    let name_area: gpui::AnyElement = if let Some(pending) = processing {
        div()
            .truncate()
            .text_color(theme.muted)
            .child(pending)
            .into_any_element()
    } else {
        input.clone().into_any_element()
    };

    let mut tile = crate::rename::with_editor_actions(
        tile_frame(&row.entry, ix),
        &input,
        cx,
        DirView::confirm_rename,
        DirView::cancel_rename,
    )
    .text_size(px(11.0))
    .text_color(theme.text)
    .child(tile_image(&row.entry, thumbnail, &theme))
    .child(div().w(px(TILE_WIDTH - 12.0)).flex_none().child(name_area));

    if let Some(message) = error {
        tile = tile.child(
            div()
                .px(px(4.0))
                .rounded(px(3.0))
                .border_1()
                .border_color(theme.error)
                .bg(theme.panel)
                .text_size(px(10.0))
                .text_color(theme.error)
                .child(message),
        );
    }
    tile
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // cols from width
    // ------------------------------------------------------------------

    #[test]
    fn cols_for_width_floors_and_never_returns_zero() {
        assert_eq!(cols_for_width(TILE_WIDTH * 5.0), 5);
        // A partial trailing column does not count: a clipped tile is worse
        // than a wider gap.
        assert_eq!(cols_for_width(TILE_WIDTH * 5.0 - 1.0), 4);
        assert_eq!(cols_for_width(TILE_WIDTH), 1);
        // Degenerate widths (not laid out yet, or narrower than a tile) still
        // have to produce a usable divisor.
        assert_eq!(cols_for_width(TILE_WIDTH - 1.0), 1);
        assert_eq!(cols_for_width(0.0), 1);
        assert_eq!(cols_for_width(-10.0), 1);
        assert_eq!(cols_for_width(f32::NAN), 1);
        assert_eq!(cols_for_width(f32::INFINITY), 1);
    }

    // ------------------------------------------------------------------
    // row count / ragged last row
    // ------------------------------------------------------------------

    #[test]
    fn grid_rows_cover_every_tile_including_a_ragged_last_row() {
        assert_eq!(grid_row_count(0, 4), 0);
        assert_eq!(grid_row_count(1, 4), 1);
        assert_eq!(grid_row_count(4, 4), 1);
        assert_eq!(grid_row_count(5, 4), 2);
        // A zero `cols` must not divide by zero.
        assert_eq!(grid_row_count(5, 0), 5);
    }

    #[test]
    fn row_items_stops_at_the_listing_end() {
        assert_eq!(row_items(0, 4, 6), 0..4);
        assert_eq!(row_items(1, 4, 6), 4..6, "ragged final row");
        assert_eq!(row_items(2, 4, 6), 6..6, "past the end is empty");
        assert_eq!(row_items(0, 4, 0), 0..0);
        // Every row's items, concatenated, are exactly 0..len with no gaps.
        let len = 11;
        let cols = 3;
        let all: Vec<usize> = (0..grid_row_count(len, cols))
            .flat_map(|row| row_items(row, cols, len))
            .collect();
        assert_eq!(all, (0..len).collect::<Vec<_>>());
    }

    // ------------------------------------------------------------------
    // 2D keyboard navigation
    // ------------------------------------------------------------------

    #[test]
    fn horizontal_steps_walk_reading_order_and_stop_at_the_ends() {
        // 3 cols, 7 tiles: rows [0 1 2] [3 4 5] [6]
        assert_eq!(step_index(0, 7, 3, GridStep::Right), 1);
        assert_eq!(step_index(2, 7, 3, GridStep::Right), 3, "wraps to next row");
        assert_eq!(step_index(3, 7, 3, GridStep::Left), 2, "and back again");
        assert_eq!(step_index(0, 7, 3, GridStep::Left), 0, "first tile holds");
        assert_eq!(step_index(6, 7, 3, GridStep::Right), 6, "last tile holds");
    }

    #[test]
    fn vertical_steps_move_by_cols_and_clamp_at_the_edges() {
        // 3 cols, 7 tiles: rows [0 1 2] [3 4 5] [6]
        assert_eq!(step_index(0, 7, 3, GridStep::Down), 3);
        assert_eq!(step_index(3, 7, 3, GridStep::Down), 6);
        // Up from the first row must not move (and must not underflow).
        assert_eq!(step_index(0, 7, 3, GridStep::Up), 0);
        assert_eq!(step_index(2, 7, 3, GridStep::Up), 2);
        assert_eq!(step_index(4, 7, 3, GridStep::Up), 1);
        // Down from the last row must not move.
        assert_eq!(step_index(6, 7, 3, GridStep::Down), 6);
    }

    #[test]
    fn down_into_a_ragged_last_row_lands_on_the_last_tile() {
        // 3 cols, 7 tiles: the last row holds only index 6, so down from
        // index 4 or 5 has no cell of its own beneath it.
        assert_eq!(step_index(4, 7, 3, GridStep::Down), 6);
        assert_eq!(step_index(5, 7, 3, GridStep::Down), 6);
        // ...but down from *within* that last row still holds.
        assert_eq!(step_index(6, 7, 3, GridStep::Down), 6);
    }

    #[test]
    fn steps_survive_cols_changing_under_the_cursor_and_degenerate_input() {
        // A stale index from a wider grid is clamped, never indexed out.
        assert_eq!(step_index(99, 7, 3, GridStep::Right), 6);
        assert_eq!(step_index(99, 7, 3, GridStep::Up), 3);
        assert_eq!(step_index(99, 7, 3, GridStep::Down), 6);
        // One column: the grid degenerates into the details list.
        assert_eq!(step_index(3, 7, 1, GridStep::Down), 4);
        assert_eq!(step_index(3, 7, 1, GridStep::Up), 2);
        // Empty listing and cols == 0 must not panic.
        for step in [
            GridStep::Left,
            GridStep::Right,
            GridStep::Up,
            GridStep::Down,
        ] {
            assert_eq!(step_index(0, 0, 3, step), 0);
            assert_eq!(step_index(2, 7, 0, step), step_index(2, 7, 1, step));
        }
    }

    #[test]
    fn step_deltas_match_the_grid_width() {
        assert_eq!(GridStep::Left.delta(4), -1);
        assert_eq!(GridStep::Right.delta(4), 1);
        assert_eq!(GridStep::Up.delta(4), -4);
        assert_eq!(GridStep::Down.delta(4), 4);
        assert_eq!(GridStep::Down.delta(0), 1, "cols is floored at one");
    }

    // ------------------------------------------------------------------
    // Hit tests (marquee + drop target)
    // ------------------------------------------------------------------

    #[test]
    fn tile_at_finds_tiles_and_empty_space() {
        let center = |col: usize, row: usize| {
            (
                col as f32 * TILE_WIDTH + TILE_WIDTH / 2.0,
                row as f32 * TILE_HEIGHT + TILE_HEIGHT / 2.0,
            )
        };
        let (x, y) = center(0, 0);
        assert_eq!(tile_at(x, y, 3, 7), Some(0));
        let (x, y) = center(2, 1);
        assert_eq!(tile_at(x, y, 3, 7), Some(5));
        // Past the ragged row's last tile: empty space, not tile 7.
        let (x, y) = center(1, 2);
        assert_eq!(tile_at(x, y, 3, 7), None);
        // Right of the column band, and below the grid.
        let (x, y) = center(3, 0);
        assert_eq!(tile_at(x, y, 3, 7), None);
        let (x, y) = center(0, 9);
        assert_eq!(tile_at(x, y, 3, 7), None);
        // Negative content coordinates (a pointer above the first row) and an
        // empty listing.
        assert_eq!(tile_at(-1.0, 5.0, 3, 7), None);
        assert_eq!(tile_at(5.0, -1.0, 3, 7), None);
        assert_eq!(tile_at(5.0, 5.0, 3, 0), None);
    }

    fn rect(left: f32, top: f32, right: f32, bottom: f32) -> MarqueeRect {
        MarqueeRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn marquee_selects_every_tile_it_overlaps() {
        // 3 cols, 7 tiles. A band over the first two columns of both full
        // rows must skip the third column entirely.
        let band = rect(1.0, 1.0, TILE_WIDTH * 2.0 - 1.0, TILE_HEIGHT * 2.0 - 1.0);
        assert_eq!(tiles_in_rect(band, 3, 7), vec![0, 1, 3, 4]);
        // A one-pixel overlap counts (Explorer is not a majority vote).
        let sliver = rect(TILE_WIDTH - 1.0, 0.0, TILE_WIDTH + 1.0, 1.0);
        assert_eq!(tiles_in_rect(sliver, 3, 7), vec![0, 1]);
        // An edge exactly on a boundary does not reach into the next tile.
        let flush = rect(0.0, 0.0, TILE_WIDTH, TILE_HEIGHT);
        assert_eq!(tiles_in_rect(flush, 3, 7), vec![0]);
        // A degenerate band selects the tile it sits inside.
        let point = rect(
            TILE_WIDTH * 1.5,
            TILE_HEIGHT * 0.5,
            TILE_WIDTH * 1.5,
            TILE_HEIGHT * 0.5,
        );
        assert_eq!(tiles_in_rect(point, 3, 7), vec![1]);
    }

    #[test]
    fn marquee_clamps_to_the_grid_and_skips_the_ragged_gap() {
        // A band covering everything, and then some, selects every tile once
        // — including the ragged last row, and nothing beyond it.
        let all = rect(-500.0, -500.0, 5_000.0, 5_000.0);
        assert_eq!(tiles_in_rect(all, 3, 7), (0..7).collect::<Vec<_>>());
        // A band over the whole last row picks up only the tile that exists.
        let last = rect(
            0.0,
            TILE_HEIGHT * 2.0 + 1.0,
            TILE_WIDTH * 3.0,
            TILE_HEIGHT * 3.0,
        );
        assert_eq!(tiles_in_rect(last, 3, 7), vec![6]);
        // Empty listing, and a band entirely outside the grid.
        assert!(tiles_in_rect(all, 3, 0).is_empty());
        let below = rect(0.0, TILE_HEIGHT * 9.0, TILE_WIDTH, TILE_HEIGHT * 10.0);
        assert!(tiles_in_rect(below, 3, 7).is_empty());
    }
}
