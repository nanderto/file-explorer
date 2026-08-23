//! Details-list rendering for [`DirView`] (ARCHITECTURE.md §8 "Details
//! list"): a `uniform_list` of fixed-height rows with Name / Size / Date
//! Modified columns, and a sortable header row whose cells dispatch
//! [`SortBy`] (with an arrow indicator on the active column). Rows render in
//! the DirView's flat projection order (snapshot order plus expanded
//! folders' injected children, M2 §8); disclosure triangles and indentation
//! render from each row's depth field. Header and body cells share the
//! column-width constants below, so values align under their headers. Every
//! color reads the [`Theme`]; no literals.

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use fs_core::{SortDirection, SortKey, SortSpec};
use gpui::{
    ClickEvent, Context, Hsla, IntoElement, SharedString, Stateful, UniformList, anchored,
    deferred, div, prelude::*, px, uniform_list,
};

use crate::actions::SortBy;
use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::drag;
use crate::pane::format_bytes;
use crate::theme::Theme;

/// Fixed row height (uniform_list requirement) shared by header and rows.
pub(crate) const ROW_HEIGHT: f32 = 24.0;
/// Column widths shared by the header row and every body row — the single
/// source of the details view's column alignment.
const SIZE_COL_WIDTH: f32 = 90.0;
const DATE_COL_WIDTH: f32 = 150.0;
/// Width of the disclosure-triangle slot (also the per-depth indent), part
/// of the Name column.
const DISCLOSURE_WIDTH: f32 = 16.0;
/// Selection tint: the theme accent at partial alpha, so selected-row text
/// keeps its normal contrast in both appearances.
const SELECTION_ALPHA: f32 = 0.35;
/// Row opacity for cut-pending entries (plan §3: "cut items render dimmed").
const CUT_DIM_OPACITY: f32 = 0.5;
/// The Name column never shrinks past this. `Size` and `Date Modified` are
/// fixed-width `flex_none` cells while Name is `flex_1` with `flex-basis: 0`,
/// so in a narrow pane the name is squeezed to nothing and every filename
/// vanishes — which is exactly what the M4 split pane does (~270 px leaves
/// ~14 px for the name). Explorer keeps the name readable and lets the
/// metadata columns go, so past this floor the trailing columns drop out:
/// Date first, then Size.
pub(crate) const NAME_MIN_WIDTH: f32 = 120.0;
/// Horizontal padding on the header and every body row, which the column-fit
/// arithmetic has to subtract before deciding what fits.
const ROW_PADDING_X: f32 = 8.0;

/// Which metadata columns fit beside a readable Name column at `available`
/// content width.
///
/// The header and body rows must be handed the **same** value within a frame,
/// or values stop aligning under their headers (the M1 column-alignment bug
/// class). [`DirView::render`] measures once and passes it to both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct VisibleColumns {
    pub(crate) size: bool,
    pub(crate) date: bool,
}

impl VisibleColumns {
    /// Every column — the assumption before a real measurement exists.
    pub(crate) const ALL: Self = Self {
        size: true,
        date: true,
    };
}

/// Decide which metadata columns fit at `available` content width.
pub(crate) fn visible_columns(available: f32) -> VisibleColumns {
    // An unmeasured width (first frame, before the list has painted bounds)
    // must assume the full set: dropping columns on a zero measurement would
    // flash the narrow layout every time a pane opens.
    if !available.is_finite() || available <= 0.0 {
        return VisibleColumns::ALL;
    }
    // What Name is left with once padding, the disclosure slot and the fixed
    // metadata cells are taken out.
    let for_name = |fixed: f32| available - 2.0 * ROW_PADDING_X - DISCLOSURE_WIDTH - fixed;
    if for_name(SIZE_COL_WIDTH + DATE_COL_WIDTH) >= NAME_MIN_WIDTH {
        VisibleColumns::ALL
    } else if for_name(SIZE_COL_WIDTH) >= NAME_MIN_WIDTH {
        VisibleColumns {
            size: true,
            date: false,
        }
    } else {
        VisibleColumns {
            size: false,
            date: false,
        }
    }
}

/// The sortable column header row. Cells dispatch `SortBy { key }` through
/// the action system so header clicks, and nothing else, own the sort logic
/// (the pane handles the action).
pub(crate) fn render_header(
    theme: &Theme,
    sort: SortSpec,
    columns: VisibleColumns,
    cx: &mut Context<DirView>,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(8.0))
        .border_b_1()
        .border_color(theme.border)
        .text_size(px(11.0))
        .text_color(theme.muted)
        // Spacer over the body rows' disclosure-triangle slot so "Name"
        // aligns with depth-0 names.
        .child(div().w(px(DISCLOSURE_WIDTH)).flex_none())
        .child(header_cell("Name", SortKey::Name, sort, true, cx))
        .when(columns.size, |el| {
            el.child(
                header_cell("Size", SortKey::Size, sort, false, cx)
                    .w(px(SIZE_COL_WIDTH))
                    .flex_none(),
            )
        })
        .when(columns.date, |el| {
            el.child(
                header_cell("Date Modified", SortKey::DateModified, sort, false, cx)
                    .w(px(DATE_COL_WIDTH))
                    .flex_none(),
            )
        })
}

fn header_cell(
    label: &'static str,
    key: SortKey,
    sort: SortSpec,
    grow: bool,
    cx: &mut Context<DirView>,
) -> Stateful<gpui::Div> {
    let arrow = if sort.key == key {
        match sort.direction {
            SortDirection::Ascending => " ▲",
            SortDirection::Descending => " ▼",
        }
    } else {
        ""
    };
    let mut cell = div()
        .id(label)
        .debug_selector(|| format!("sort-header-{label}"))
        .flex()
        .items_center()
        .cursor_pointer()
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            // Make dispatch deterministic regardless of prior focus: the
            // action bubbles from this view up to the pane's handler.
            window.focus(this.focus_handle_ref(), cx);
            window.dispatch_action(Box::new(SortBy { key }), cx);
        }))
        .child(format!("{label}{arrow}"));
    if grow {
        cell = cell.flex_1();
    } else {
        cell = cell.justify_end();
    }
    cell
}

/// The virtualized row list over the DirView's flat projection (only the
/// visible range renders; expansion just changes the projection length).
pub(crate) fn render_rows(
    dir_view: &DirView,
    columns: VisibleColumns,
    cx: &mut Context<DirView>,
) -> UniformList {
    let item_count = dir_view.flat_rows().len();
    uniform_list(
        "details-rows",
        item_count,
        cx.processor(move |this, range: Range<usize>, _window, cx| {
            range
                .filter_map(|ix| {
                    let row = this.flat_rows().get(ix)?.clone();
                    Some(render_row(this, &row, ix, columns, cx))
                })
                .collect::<Vec<_>>()
        }),
    )
    .flex_1()
    .track_scroll(dir_view.scroll_handle())
}

fn render_row(
    this: &mut DirView,
    row: &crate::dir_view::ProjectedRow,
    ix: usize,
    columns: VisibleColumns,
    cx: &mut Context<DirView>,
) -> Stateful<gpui::Div> {
    // ARCHITECTURE.md §4c/§8 "Inline rename overlay": the row of the entry
    // being renamed swaps its name cell for the editor (or, once `Confirm`
    // has submitted the op, the pending name) instead of the normal label.
    if this
        .rename
        .as_ref()
        .is_some_and(|rename| *rename.target() == row.entry.id())
    {
        return render_rename_row(this, row, ix, columns, cx);
    }

    let entry = &row.entry;
    let theme = this.theme().clone();
    let selected = this.selection().is_selected(&entry.id());
    // Cut-pending entries render dimmed (§4b: the DirView checks membership
    // in the FsContext clipboard at render).
    let cut_pending = FsContext::global(cx).clipboard.is_cut(&entry.path);
    let name: SharedString = SharedString::new(entry.name.clone());
    let size = size_cell(entry);
    let modified: SharedString = SharedString::new(format_modified(entry.modified));
    let click_entry = entry.clone();

    // The Name column: per-depth indent, then a disclosure-triangle slot
    // (folders only — files keep an empty slot so names stay aligned), then
    // the truncating name.
    let disclosure: gpui::AnyElement = if entry.is_dir_like() {
        let toggle_path = entry.path.clone();
        div()
            // Path-keyed like the row itself: a click's press is persisted per
            // element id too, so an index here could toggle a folder the user
            // never pressed on after a mid-gesture re-projection.
            .id(gpui::ElementId::NamedChild(
                std::sync::Arc::new(gpui::ElementId::Path(entry.path.clone())),
                SharedString::new_static("disclosure"),
            ))
            .debug_selector(|| format!("dir-row-disclosure-{ix}"))
            .w(px(DISCLOSURE_WIDTH))
            .flex_none()
            .text_color(theme.muted)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                window.focus(this.focus_handle_ref(), cx);
                this.toggle_expanded(&toggle_path, cx);
            }))
            .child(SharedString::new_static(if row.expanded {
                "▾"
            } else {
                "▸"
            }))
            .into_any_element()
    } else {
        div().w(px(DISCLOSURE_WIDTH)).flex_none().into_any_element()
    };

    let mut styled_row = div()
        // **Path-keyed, not index-keyed** (invariant #2). gpui persists a
        // stateful element's `pending_mouse_down` across frames by its
        // `GlobalElementId`, and a drag started by a later mouse-move reads
        // that persisted press through *this* frame's drag listener without
        // re-hit-testing. With index ids, a watcher patch (or any
        // re-projection) landing between the press and the move would hand the
        // press on row `n` to whatever entry now sits at index `n` — and this
        // row's `on_drag` payload turns that into a filesystem move of a file
        // the user never touched. The path never moves between entries.
        .id(gpui::ElementId::Path(entry.path.clone()))
        // The *selector* stays index-based: it names a position on screen,
        // which is what a test clicking "the third row" means.
        .debug_selector(|| format!("dir-row-{ix}"))
        .flex()
        .items_center()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(8.0))
        .text_size(px(13.0))
        .text_color(if entry.hidden {
            theme.muted
        } else {
            theme.text
        })
        .cursor_pointer()
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            window.focus(this.focus_handle_ref(), cx);
            // §0 selection row (fast double-click / cmd-click / shift-click)
            // and the §0/§8 rename trigger (a slow second click on an
            // already-armed row) share one dispatcher — see `DirView`'s doc
            // comment on `handle_row_click`.
            this.handle_row_click(
                &click_entry,
                event.modifiers(),
                event.click_count(),
                window,
                cx,
            );
        }))
        .child(div().w(px(row.depth as f32 * DISCLOSURE_WIDTH)).flex_none())
        .child(disclosure)
        .child(div().flex_1().truncate().child(name))
        .when(columns.size, |el| {
            el.child(
                div()
                    .w(px(SIZE_COL_WIDTH))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_color(theme.muted)
                    .child(size),
            )
        })
        .when(columns.date, |el| {
            el.child(
                div()
                    .w(px(DATE_COL_WIDTH))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .text_color(theme.muted)
                    .child(modified),
            )
        });
    if selected {
        styled_row = styled_row.bg(selection_color(&theme));
    }
    // §8 drag & drop: this row is the armed folder drop target. Painted after
    // the selection tint (a target that is also selected reads as the target)
    // and as a background only — arming a highlight must never move a row.
    if drag::row_is_drop_target(this, &entry.path, cx) {
        styled_row = styled_row.bg(drag::drop_row_color(&theme));
    }
    if cut_pending {
        styled_row = styled_row.opacity(CUT_DIM_OPACITY);
    }
    // §8 "every `on_drag` pairs with `external_drag_payload`": the row starts
    // the file drag. The payload is built *here*, at render time, from the
    // selection as last painted — which is exactly the Explorer rule, because
    // a press does not change the selection (a click does, on release): a
    // grabbed row that was selected drags the whole selection, one that was
    // not drags itself. Not while the inline editor is up.
    if this.rename.is_none() {
        let dragged = this.drag_payload(entry.path.clone());
        let ghost_label = dragged.label();
        let ghost_theme = theme.clone();
        // The outbound (us → Finder) dir flags are resolved from this view
        // only if the drag actually leaves the window — never per frame, and
        // never by stat'ing the disk on the UI thread.
        let view = cx.weak_entity();
        styled_row = styled_row
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
    styled_row
}

/// The Size column's text: folders show an em dash, files their byte size.
/// Shared by the normal and the rename row so the column can't diverge.
fn size_cell(entry: &fs_core::FileEntry) -> SharedString {
    if entry.is_dir_like() {
        SharedString::new_static("—")
    } else {
        SharedString::new(format_bytes(entry.size))
    }
}

/// The row of the entry being renamed (ARCHITECTURE.md §4c/§8): the name
/// cell is the vendored text editor (or, once `Confirm` has submitted the op,
/// the plain pending name — not editable). Its `Confirm`/`Cancel` and editing
/// keys come from [`crate::rename::with_editor_actions`], the same wiring the
/// grid tile uses. An inline validation error (local or reported by the op)
/// renders as a `deferred` popup under the row.
fn render_rename_row(
    this: &mut DirView,
    row: &crate::dir_view::ProjectedRow,
    ix: usize,
    columns: VisibleColumns,
    cx: &mut Context<DirView>,
) -> Stateful<gpui::Div> {
    let theme = this.theme().clone();
    let rename = this
        .rename
        .as_ref()
        .expect("render_rename_row requires an active rename");
    let input = rename.input().clone();
    let processing = rename.processing().cloned();
    let error = rename.error().cloned();
    let depth = row.depth;
    // Explorer keeps the Size / Date columns filled while a row is being
    // renamed — only the name cell becomes the editor. A §4c *new-entry*
    // phantom row has nothing to report in either column (and no real mtime),
    // so both stay blank until the entry exists.
    let (size, modified): (SharedString, SharedString) = if rename.is_new_entry() {
        (SharedString::default(), SharedString::default())
    } else {
        (
            size_cell(&row.entry),
            SharedString::new(format_modified(row.entry.modified)),
        )
    };

    let name_area: gpui::AnyElement = if let Some(pending) = processing {
        div()
            .flex_1()
            .truncate()
            .text_color(theme.muted)
            .child(pending)
            .into_any_element()
    } else {
        input.clone().into_any_element()
    };

    // The editor's dispatch node — focus, `TextInput` key context,
    // `Confirm`/`Cancel` and the vendored input's editing actions — is
    // [`crate::rename::with_editor_actions`], shared with the grid tile so
    // the wiring exists exactly once.
    let mut styled_row = crate::rename::with_editor_actions(
        div()
            // Path-keyed for the same reason as the normal row above; only one
            // of the two ever paints for a given path in a frame.
            .id(gpui::ElementId::Path(row.entry.path.clone()))
            .debug_selector(|| format!("dir-row-{ix}")),
        &input,
        cx,
    )
    .flex()
    .items_center()
    .w_full()
    .h(px(ROW_HEIGHT))
    .px(px(8.0))
    .text_size(px(13.0))
    .text_color(theme.text)
    .child(div().w(px(depth as f32 * DISCLOSURE_WIDTH)).flex_none())
    .child(div().w(px(DISCLOSURE_WIDTH)).flex_none())
    .child(name_area)
    .when(columns.size, |el| {
        el.child(
            div()
                .w(px(SIZE_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(size),
        )
    })
    .when(columns.date, |el| {
        el.child(
            div()
                .w(px(DATE_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(modified),
        )
    });

    if let Some(message) = error {
        styled_row = styled_row.child(deferred(
            anchored().child(
                div()
                    .absolute()
                    .top(px(ROW_HEIGHT))
                    .left(px(depth as f32 * DISCLOSURE_WIDTH + DISCLOSURE_WIDTH))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .border_1()
                    .border_color(theme.error)
                    .bg(theme.panel)
                    .text_size(px(11.0))
                    .text_color(theme.error)
                    .child(message),
            ),
        ));
    }

    styled_row
}

/// The selection background, derived from the active theme's accent (no
/// color literals in the app crate).
fn selection_color(theme: &Theme) -> Hsla {
    Hsla {
        a: SELECTION_ALPHA,
        ..theme.accent
    }
}

/// Format a modification time as a fixed-width UTC timestamp
/// (`YYYY-MM-DD HH:MM`). UTC keeps renders deterministic across machines;
/// local-time display is later polish.
pub(crate) fn format_modified(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3_600;
    let minute = (tod % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Days-since-epoch → (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days` algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: i64) -> SystemTime {
        if secs >= 0 {
            UNIX_EPOCH + Duration::from_secs(secs as u64)
        } else {
            UNIX_EPOCH - Duration::from_secs((-secs) as u64)
        }
    }

    #[test]
    fn format_modified_known_timestamps() {
        assert_eq!(format_modified(at(0)), "1970-01-01 00:00");
        // 2023-11-14 22:13:20 UTC
        assert_eq!(format_modified(at(1_700_000_000)), "2023-11-14 22:13");
        // Leap-day handling: 2024-02-29 12:00:00 UTC
        assert_eq!(format_modified(at(1_709_208_000)), "2024-02-29 12:00");
        // Pre-epoch times must not panic and stay calendar-correct.
        assert_eq!(format_modified(at(-86_400)), "1969-12-31 00:00");
    }

    #[test]
    fn size_cell_dashes_folders_and_formats_files() {
        let entry = |kind, size| fs_core::FileEntry {
            path: std::sync::Arc::from(std::path::Path::new("/root/x")),
            name: std::sync::Arc::from("x"),
            kind,
            size,
            modified: at(0),
            created: None,
            hidden: false,
        };
        assert_eq!(
            size_cell(&entry(fs_core::EntryKind::Dir, 4096)).as_ref(),
            "—",
            "folders never show a byte count"
        );
        assert_eq!(
            size_cell(&entry(fs_core::EntryKind::File, 10)).as_ref(),
            "10 B"
        );
    }

    #[test]
    fn civil_from_days_round_trip_edges() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    /// The floor that keeps filenames on screen. A single full-width pane fits
    /// everything; the M4 split pane does not, and the columns that go are the
    /// trailing ones — never the Name.
    #[test]
    fn narrow_panes_drop_trailing_columns_instead_of_the_name() {
        // Full-width single pane (the `listing_populated` scenario): all three.
        assert_eq!(visible_columns(760.0), VisibleColumns::ALL);

        // The exact width that stops fitting Name + Size + Date. Below it the
        // Date column goes first.
        let both = 2.0 * ROW_PADDING_X + DISCLOSURE_WIDTH + SIZE_COL_WIDTH + DATE_COL_WIDTH;
        assert_eq!(visible_columns(both + NAME_MIN_WIDTH), VisibleColumns::ALL);
        assert_eq!(
            visible_columns(both + NAME_MIN_WIDTH - 1.0),
            VisibleColumns {
                size: true,
                date: false
            },
            "one pixel under the fit, Date is the column that goes"
        );

        // The M4 split pane: ~270 px used to leave ~14 px for the name, which
        // rendered every filename as nothing. Now Name is guaranteed its floor.
        let split = visible_columns(271.0);
        assert!(
            !split.date,
            "a 271 px pane cannot afford Date and a readable name"
        );
        let leftover = 271.0
            - 2.0 * ROW_PADDING_X
            - DISCLOSURE_WIDTH
            - if split.size { SIZE_COL_WIDTH } else { 0.0 };
        assert!(
            leftover >= NAME_MIN_WIDTH,
            "Name kept {leftover} px, below the {NAME_MIN_WIDTH} px floor"
        );

        // Pathological widths must not panic or silently drop everything on a
        // first frame that has no painted bounds yet.
        assert_eq!(visible_columns(0.0), VisibleColumns::ALL, "unmeasured");
        assert_eq!(visible_columns(-50.0), VisibleColumns::ALL, "nonsense");
        assert_eq!(visible_columns(f32::NAN), VisibleColumns::ALL, "NaN");
        assert_eq!(
            visible_columns(f32::INFINITY),
            VisibleColumns::ALL,
            "infinite"
        );
        // Truly tiny: nothing but the name survives, and it still gets a cell.
        assert_eq!(
            visible_columns(60.0),
            VisibleColumns {
                size: false,
                date: false
            }
        );
    }
}
