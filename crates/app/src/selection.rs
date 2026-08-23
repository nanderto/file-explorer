//! The selection model (ARCHITECTURE.md §2 `SelectionModel`) — a **plain
//! struct** field of [`crate::dir_view::DirView`], view-mode agnostic.
//!
//! **Path-keyed** (invariant #2): a `BTreeSet<EntryId>` plus a cursor and a
//! range anchor, never indices — so selection survives re-sorts, watcher
//! patches, and in-place expansion re-projection. Range operations take the
//! current visual row order as a slice of ids; the model itself never sees
//! rows or views.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::EntryId;

/// Multi-selection state: the selected set, the keyboard cursor, and the
/// anchor that shift-click / shift-arrow ranges extend from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectionModel {
    selected: BTreeSet<EntryId>,
    cursor: Option<EntryId>,
    anchor: Option<EntryId>,
}

impl SelectionModel {
    pub fn cursor(&self) -> Option<&EntryId> {
        self.cursor.as_ref()
    }

    pub fn is_selected(&self, id: &EntryId) -> bool {
        self.selected.contains(id)
    }

    pub fn selected(&self) -> &BTreeSet<EntryId> {
        &self.selected
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// The selected paths as owned buffers, in path order — the op-submission
    /// input (clipboard set, trash, delete).
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected.iter().map(|id| id.0.to_path_buf()).collect()
    }

    /// [`Self::selected_paths`] with descendants of selected folders dropped:
    /// when a folder *and* something inside it are both selected (in-place
    /// expansion makes this easy), ops must act on the root-most paths only —
    /// trashing the folder already takes its children with it.
    pub fn selected_paths_rootmost(&self) -> Vec<PathBuf> {
        self.selected_rootmost()
            .iter()
            .map(|path| path.to_path_buf())
            .collect()
    }

    /// [`Self::selected_paths_rootmost`] without the `PathBuf` copies — the
    /// `Arc<Path>` keys themselves, for the drag payload, which is rebuilt on
    /// every frame a drag-capable row paints.
    ///
    /// Linear in the selection: the set is ordered by path, and path ordering
    /// is component-wise, so *every* descendant of a kept path sorts directly
    /// after it and before anything that is not a descendant. One walk with a
    /// single "last kept" anchor therefore drops exactly the descendants
    /// (`"/a"` keeps `"/a b"` — `Path::starts_with` is component-wise too —
    /// while dropping `"/a/b"` and `"/a/b/c"`).
    pub fn selected_rootmost(&self) -> Vec<Arc<Path>> {
        let mut rootmost: Vec<Arc<Path>> = Vec::new();
        for id in &self.selected {
            let covered = rootmost
                .last()
                .is_some_and(|kept| id.0.starts_with(kept) && id.0 != *kept);
            if !covered {
                rootmost.push(id.0.clone());
            }
        }
        rootmost
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
    }

    /// Plain click / cursor movement: the selection becomes exactly `id`.
    pub fn select_only(&mut self, id: EntryId) {
        self.selected.clear();
        self.selected.insert(id.clone());
        self.cursor = Some(id.clone());
        self.anchor = Some(id);
    }

    /// `cmd`-click: toggle membership. The cursor and anchor move to the
    /// clicked row either way (Explorer behavior).
    pub fn toggle(&mut self, id: EntryId) {
        if !self.selected.remove(&id) {
            self.selected.insert(id.clone());
        }
        self.cursor = Some(id.clone());
        self.anchor = Some(id);
    }

    /// `shift`-click / `shift`-arrow: the selection becomes the contiguous
    /// range from the anchor to `target` in the given visual `order`. The
    /// anchor stays put (successive shift-selects re-range from it); the
    /// cursor moves to `target`. With no anchor yet, the range starts at the
    /// first row.
    pub fn select_range_to(&mut self, target: EntryId, order: &[EntryId]) {
        let Some(target_ix) = order.iter().position(|id| *id == target) else {
            return;
        };
        let anchor = self.anchor.clone().or_else(|| self.cursor.clone());
        let anchor_ix = anchor
            .as_ref()
            .and_then(|a| order.iter().position(|id| id == a))
            .unwrap_or(0);
        let (lo, hi) = if anchor_ix <= target_ix {
            (anchor_ix, target_ix)
        } else {
            (target_ix, anchor_ix)
        };
        self.selected = order[lo..=hi].iter().cloned().collect();
        self.cursor = Some(target);
        self.anchor = Some(order[anchor_ix].clone());
    }

    /// `cmd-a`: select every visible row. Cursor and anchor are unchanged.
    pub fn select_all(&mut self, order: &[EntryId]) {
        self.selected = order.iter().cloned().collect();
    }

    /// Rubber-band marquee ([`crate::marquee`]): the selection becomes
    /// `base ∪ rows`, recomputed from scratch on every pointer move.
    ///
    /// `base` is the selection the gesture started from — **empty** for a
    /// plain drag, which replaces, and the pre-gesture set for an additive
    /// `cmd`-drag, which unions (Explorer behavior). Recomputing rather than
    /// accumulating is what makes shrinking the band give back exactly the
    /// rows the band itself had added, and nothing the user had selected
    /// before it.
    ///
    /// `focus` is the row under the band's moving corner: the cursor and
    /// anchor follow it so a `shift`-arrow after the drag extends from where
    /// the pointer stopped. An empty band leaves them where they were.
    pub fn select_marquee(
        &mut self,
        base: &BTreeSet<EntryId>,
        rows: &[EntryId],
        focus: Option<EntryId>,
    ) {
        self.selected = base.clone();
        self.selected.extend(rows.iter().cloned());
        if let Some(focus) = focus {
            self.cursor = Some(focus.clone());
            self.anchor = Some(focus);
        }
    }

    /// Survival across fresh loads and watcher patches: drop ids that
    /// vanished. A pruned cursor/anchor is cleared rather than left dangling.
    pub fn retain(&mut self, keep: impl Fn(&EntryId) -> bool) {
        self.selected.retain(&keep);
        if self.cursor.as_ref().is_some_and(|id| !keep(id)) {
            self.cursor = None;
        }
        if self.anchor.as_ref().is_some_and(|id| !keep(id)) {
            self.anchor = None;
        }
    }

    /// NavEntry restore: place the cursor (and anchor) on `cursor`, ensuring
    /// it is selected, **without** collapsing the rest of the selection —
    /// refresh and re-sort restore through this, and a multi-selection must
    /// survive them. `None` clears only the cursor/anchor.
    pub fn restore_cursor(&mut self, cursor: Option<EntryId>) {
        if let Some(id) = &cursor {
            self.selected.insert(id.clone());
        }
        self.anchor = cursor.clone();
        self.cursor = cursor;
    }
}

#[cfg(test)]
mod tests {
    //! §9 `selection.rs` rows: click/cmd/shift/select-all mutations and
    //! path-keyed survival (`retain`) — the view-independent half; the
    //! projection-integrated half lives in `dir_view.rs` tests.

    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    fn id(path: &str) -> EntryId {
        EntryId(Arc::from(Path::new(path)))
    }

    fn ids(paths: &[&str]) -> Vec<EntryId> {
        paths.iter().map(|p| id(p)).collect()
    }

    fn selected(model: &SelectionModel) -> Vec<EntryId> {
        model.selected().iter().cloned().collect()
    }

    #[test]
    fn select_only_replaces_and_moves_cursor_and_anchor() {
        let mut model = SelectionModel::default();
        assert!(model.is_empty());
        model.select_only(id("/d/a"));
        model.select_only(id("/d/b"));
        assert_eq!(selected(&model), ids(&["/d/b"]));
        assert_eq!(model.cursor(), Some(&id("/d/b")));
    }

    #[test]
    fn toggle_adds_and_removes_moving_the_cursor() {
        let mut model = SelectionModel::default();
        model.select_only(id("/d/a"));
        model.toggle(id("/d/c"));
        assert_eq!(selected(&model), ids(&["/d/a", "/d/c"]));
        assert_eq!(model.cursor(), Some(&id("/d/c")));

        // Toggling a selected row deselects it but keeps the cursor there.
        model.toggle(id("/d/a"));
        assert_eq!(selected(&model), ids(&["/d/c"]));
        assert_eq!(model.cursor(), Some(&id("/d/a")));
        assert!(!model.is_selected(&id("/d/a")));
    }

    #[test]
    fn range_select_spans_anchor_to_target_in_visual_order() {
        let order = ids(&["/d/a", "/d/b", "/d/c", "/d/d", "/d/e"]);
        let mut model = SelectionModel::default();
        model.select_only(id("/d/b"));

        model.select_range_to(id("/d/d"), &order);
        assert_eq!(selected(&model), ids(&["/d/b", "/d/c", "/d/d"]));
        assert_eq!(model.cursor(), Some(&id("/d/d")));

        // The anchor holds: re-ranging upward replaces, not extends.
        model.select_range_to(id("/d/a"), &order);
        assert_eq!(selected(&model), ids(&["/d/a", "/d/b"]));
        assert_eq!(model.cursor(), Some(&id("/d/a")));
    }

    #[test]
    fn range_select_without_anchor_starts_at_the_first_row() {
        let order = ids(&["/d/a", "/d/b", "/d/c"]);
        let mut model = SelectionModel::default();
        model.select_range_to(id("/d/b"), &order);
        assert_eq!(selected(&model), ids(&["/d/a", "/d/b"]));
    }

    #[test]
    fn range_select_to_unknown_target_is_a_no_op() {
        let order = ids(&["/d/a"]);
        let mut model = SelectionModel::default();
        model.select_only(id("/d/a"));
        model.select_range_to(id("/d/gone"), &order);
        assert_eq!(selected(&model), ids(&["/d/a"]));
    }

    #[test]
    fn select_all_selects_the_visible_order() {
        let order = ids(&["/d/a", "/d/b"]);
        let mut model = SelectionModel::default();
        model.select_only(id("/d/b"));
        model.select_all(&order);
        assert_eq!(selected(&model), ids(&["/d/a", "/d/b"]));
        assert_eq!(model.cursor(), Some(&id("/d/b")), "cursor unchanged");
    }

    #[test]
    fn select_marquee_replaces_or_unions_and_follows_the_moving_corner() {
        let mut model = SelectionModel::default();
        model.select_only(id("/d/a"));

        // Plain drag: an empty base replaces the earlier selection, and the
        // cursor/anchor land on the band's moving corner.
        model.select_marquee(&BTreeSet::new(), &ids(&["/d/c", "/d/d"]), Some(id("/d/d")));
        assert_eq!(selected(&model), ids(&["/d/c", "/d/d"]));
        assert_eq!(model.cursor(), Some(&id("/d/d")));

        // Additive drag: the base survives, and shrinking the band gives back
        // only the row the band had added.
        let base = ids(&["/d/a"]).into_iter().collect::<BTreeSet<_>>();
        model.select_marquee(&base, &ids(&["/d/c", "/d/d"]), Some(id("/d/c")));
        assert_eq!(selected(&model), ids(&["/d/a", "/d/c", "/d/d"]));
        model.select_marquee(&base, &ids(&["/d/c"]), Some(id("/d/c")));
        assert_eq!(
            selected(&model),
            ids(&["/d/a", "/d/c"]),
            "/d/d let go, /d/a kept"
        );
        assert_eq!(model.cursor(), Some(&id("/d/c")));
    }

    #[test]
    fn select_marquee_with_an_empty_band_clears_but_keeps_the_cursor() {
        let mut model = SelectionModel::default();
        model.select_only(id("/d/b"));
        model.select_marquee(&BTreeSet::new(), &[], None);
        assert!(model.is_empty(), "a band over nothing selects nothing");
        assert_eq!(
            model.cursor(),
            Some(&id("/d/b")),
            "the cursor is left where it was, unselected"
        );
    }

    #[test]
    fn retain_prunes_vanished_ids_and_dangling_cursor() {
        let mut model = SelectionModel::default();
        model.select_only(id("/d/a"));
        model.toggle(id("/d/b"));
        model.retain(|entry| entry != &id("/d/b"));
        assert_eq!(selected(&model), ids(&["/d/a"]));
        assert_eq!(model.cursor(), None, "pruned cursor cleared");

        model.retain(|_| true);
        assert_eq!(selected(&model), ids(&["/d/a"]), "survivors kept");
    }

    #[test]
    fn restore_cursor_keeps_the_wider_selection() {
        let mut model = SelectionModel::default();
        model.select_only(id("/d/a"));
        model.toggle(id("/d/b"));
        model.restore_cursor(Some(id("/d/a")));
        assert_eq!(selected(&model), ids(&["/d/a", "/d/b"]));
        assert_eq!(model.cursor(), Some(&id("/d/a")));

        model.restore_cursor(None);
        assert_eq!(model.cursor(), None);
        assert_eq!(selected(&model), ids(&["/d/a", "/d/b"]), "set survives");
    }

    #[test]
    fn rootmost_paths_drop_selected_descendants() {
        let mut model = SelectionModel::default();
        model.select_only(id("/d/sub"));
        model.toggle(id("/d/sub/inner.txt"));
        model.toggle(id("/d/other.txt"));
        assert_eq!(
            model.selected_paths_rootmost(),
            vec![PathBuf::from("/d/other.txt"), PathBuf::from("/d/sub")]
        );
        assert_eq!(model.selected_paths().len(), 3, "raw list is unfiltered");
    }

    #[test]
    fn rootmost_walks_the_ordered_set_once_without_over_pruning() {
        // The linear "last kept" walk leans on path order being component-wise:
        // every descendant sorts directly after its ancestor, and a *sibling*
        // whose name merely starts with the same characters does not. Nested
        // levels, a name that is a string-prefix of the kept folder, and a
        // later unrelated subtree all have to come out right.
        let mut model = SelectionModel::default();
        for path in [
            "/d/sub",
            "/d/sub/deep",
            "/d/sub/deep/leaf.txt",
            "/d/subtle.txt",
            "/d/zed/inner",
            "/d/zed/inner/x.txt",
        ] {
            model.toggle(id(path));
        }
        // /d/sub covers both of its descendants (two levels down included);
        // /d/subtle.txt is a sibling, not a child, so it stays; /d/zed/inner
        // covers its own child even though /d/sub was the last kept before it.
        let expected: Vec<Arc<Path>> = ["/d/sub", "/d/subtle.txt", "/d/zed/inner"]
            .iter()
            .map(|p| Arc::from(Path::new(p)))
            .collect();
        assert_eq!(model.selected_rootmost(), expected);
        assert_eq!(
            model.selected_paths_rootmost(),
            vec![
                PathBuf::from("/d/sub"),
                PathBuf::from("/d/subtle.txt"),
                PathBuf::from("/d/zed/inner"),
            ]
        );
    }
}
