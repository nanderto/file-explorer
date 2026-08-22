//! Details-list rendering for [`DirView`] (ARCHITECTURE.md §8 "Details
//! list"): a `uniform_list` of fixed-height rows with Name / Size / Date
//! Modified columns, and a sortable header row whose cells dispatch
//! [`SortBy`] (with an arrow indicator on the active column). Folders-first
//! ordering comes from the snapshot's [`SortSpec`] — rows render in snapshot
//! order. Every color reads the [`Theme`]; no literals.

use std::ops::Range;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fs_core::{FileEntry, ListingSnapshot, SortDirection, SortKey, SortSpec};
use gpui::{
    ClickEvent, Context, Hsla, IntoElement, SharedString, Stateful, UniformList, div, prelude::*,
    px, uniform_list,
};

use crate::actions::SortBy;
use crate::dir_view::DirView;
use crate::pane::format_bytes;
use crate::theme::Theme;

/// Fixed row height (uniform_list requirement) shared by header and rows.
pub(crate) const ROW_HEIGHT: f32 = 24.0;
const SIZE_COL_WIDTH: f32 = 90.0;
const DATE_COL_WIDTH: f32 = 150.0;
/// Selection tint: the theme accent at partial alpha, so selected-row text
/// keeps its normal contrast in both appearances.
const SELECTION_ALPHA: f32 = 0.35;

/// The sortable column header row. Cells dispatch `SortBy { key }` through
/// the action system so header clicks, and nothing else, own the sort logic
/// (the pane handles the action).
pub(crate) fn render_header(
    theme: &Theme,
    sort: SortSpec,
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
        .child(header_cell("Name", SortKey::Name, sort, true, cx))
        .child(header_cell("Size", SortKey::Size, sort, false, cx).w(px(SIZE_COL_WIDTH)))
        .child(
            header_cell("Date Modified", SortKey::DateModified, sort, false, cx)
                .w(px(DATE_COL_WIDTH)),
        )
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

/// The virtualized row list over the snapshot (only the visible range
/// renders).
pub(crate) fn render_rows(
    dir_view: &DirView,
    snapshot: Arc<ListingSnapshot>,
    cx: &mut Context<DirView>,
) -> UniformList {
    let item_count = snapshot.entries.len();
    uniform_list(
        "details-rows",
        item_count,
        cx.processor(move |this, range: Range<usize>, _window, cx| {
            range
                .map(|ix| render_row(this, &snapshot.entries[ix], ix, cx))
                .collect::<Vec<_>>()
        }),
    )
    .flex_1()
    .track_scroll(dir_view.scroll_handle())
}

fn render_row(
    this: &mut DirView,
    entry: &FileEntry,
    ix: usize,
    cx: &mut Context<DirView>,
) -> Stateful<gpui::Div> {
    let theme = this.theme().clone();
    let selected = this.cursor() == Some(&entry.id());
    let name: SharedString = SharedString::new(entry.name.clone());
    let size: SharedString = if entry.is_dir_like() {
        SharedString::new_static("—")
    } else {
        SharedString::new(format_bytes(entry.size))
    };
    let modified: SharedString = SharedString::new(format_modified(entry.modified));
    let click_entry = entry.clone();

    let mut row = div()
        .id(ix)
        .debug_selector(|| format!("dir-row-{ix}"))
        .flex()
        .items_center()
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
            if event.click_count() >= 2 {
                this.open_entry(&click_entry, cx);
            } else {
                this.select_entry(&click_entry, cx);
            }
        }))
        .child(div().flex_1().truncate().child(name))
        .child(
            div()
                .w(px(SIZE_COL_WIDTH))
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(size),
        )
        .child(
            div()
                .w(px(DATE_COL_WIDTH))
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(modified),
        );
    if selected {
        row = row.bg(selection_color(&theme));
    }
    row
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
    fn civil_from_days_round_trip_edges() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }
}
