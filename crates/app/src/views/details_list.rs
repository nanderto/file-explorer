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

use crate::actions::{Cancel, Confirm, SortBy};
use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::input::text_input as ti;
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
        // Spacer over the body rows' disclosure-triangle slot so "Name"
        // aligns with depth-0 names.
        .child(div().w(px(DISCLOSURE_WIDTH)).flex_none())
        .child(header_cell("Name", SortKey::Name, sort, true, cx))
        .child(
            header_cell("Size", SortKey::Size, sort, false, cx)
                .w(px(SIZE_COL_WIDTH))
                .flex_none(),
        )
        .child(
            header_cell("Date Modified", SortKey::DateModified, sort, false, cx)
                .w(px(DATE_COL_WIDTH))
                .flex_none(),
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

/// The virtualized row list over the DirView's flat projection (only the
/// visible range renders; expansion just changes the projection length).
pub(crate) fn render_rows(dir_view: &DirView, cx: &mut Context<DirView>) -> UniformList {
    let item_count = dir_view.flat_rows().len();
    uniform_list(
        "details-rows",
        item_count,
        cx.processor(move |this, range: Range<usize>, _window, cx| {
            range
                .filter_map(|ix| {
                    let row = this.flat_rows().get(ix)?.clone();
                    Some(render_row(this, &row, ix, cx))
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
        return render_rename_row(this, row, ix, cx);
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
            .id(("dir-row-disclosure", ix))
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
        .id(ix)
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
        .child(
            div()
                .w(px(SIZE_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(size),
        )
        .child(
            div()
                .w(px(DATE_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(modified),
        );
    if selected {
        styled_row = styled_row.bg(selection_color(&theme));
    }
    if cut_pending {
        styled_row = styled_row.opacity(CUT_DIM_OPACITY);
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
/// cell is the vendored [`ti`] editor (or, once `Confirm` has submitted the
/// op, the plain pending name — not editable); `Confirm`/`Cancel` and the
/// editor's own editing keys are wired here, same pattern as
/// `address_bar.rs`'s `TextInput` context. An inline validation error (local
/// or reported by the op) renders as a `deferred` popup under the row.
fn render_rename_row(
    this: &mut DirView,
    row: &crate::dir_view::ProjectedRow,
    ix: usize,
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
    // renamed — only the name cell becomes the editor.
    let size = size_cell(&row.entry);
    let modified: SharedString = SharedString::new(format_modified(row.entry.modified));

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

    // `track_focus` so the row's `TextInput` key context is actually part of
    // the dispatch chain while the embedded editor holds focus (a plain
    // `.child(input)` without it leaves `Confirm`/`Cancel` unreachable).
    let input_focus = input.read(cx).focus_handle(cx);
    let mut styled_row = div()
        .id(("dir-row-rename", ix))
        .debug_selector(|| format!("dir-row-{ix}"))
        .track_focus(&input_focus)
        .key_context("TextInput")
        .on_action(cx.listener(|this, _: &Confirm, window, cx| this.confirm_rename(window, cx)))
        .on_action(cx.listener(|this, _: &Cancel, window, cx| this.cancel_rename(window, cx)))
        // Forward the vendored input's editing actions (bound in
        // keymap.rs, `TextInput` context) into the row's editor, same
        // pattern as `address_bar.rs`.
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Left, w, cx| input.update(cx, |i, cx| i.left(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Right, w, cx| input.update(cx, |i, cx| i.right(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::SelectLeft, w, cx| input.update(cx, |i, cx| i.select_left(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::SelectRight, w, cx| input.update(cx, |i, cx| i.select_right(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::SelectAll, w, cx| input.update(cx, |i, cx| i.select_all(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Home, w, cx| input.update(cx, |i, cx| i.home(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::End, w, cx| input.update(cx, |i, cx| i.end(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Backspace, w, cx| input.update(cx, |i, cx| i.backspace(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Delete, w, cx| input.update(cx, |i, cx| i.delete(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Copy, w, cx| input.update(cx, |i, cx| i.copy(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Cut, w, cx| input.update(cx, |i, cx| i.cut(a, w, cx))
        }))
        .on_action(cx.listener({
            let input = input.clone();
            move |_, a: &ti::Paste, w, cx| input.update(cx, |i, cx| i.paste(a, w, cx))
        }))
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
        .child(
            div()
                .w(px(SIZE_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(size),
        )
        .child(
            div()
                .w(px(DATE_COL_WIDTH))
                .flex_none()
                .flex()
                .justify_end()
                .text_color(theme.muted)
                .child(modified),
        );

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
}
