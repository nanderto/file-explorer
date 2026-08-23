//! View-mode row rendering for `DirView` (ARCHITECTURE.md §8): details list
//! at M1, icon grid at M4; Miller columns (`columns.rs`) stay a stretch item.
//! Which one paints is [`crate::pane::ViewMode`], read by `DirView::render`.

pub mod details_list;
pub mod icon_grid;
