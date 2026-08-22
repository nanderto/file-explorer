//! Every user command, as a gpui action (ARCHITECTURE.md §3).
//!
//! The list is exactly the Action column of the §0 traceability table's **M1
//! rows** — later milestones grow it additively in the same PR that adds the
//! table rows. Keymap bindings (`crate::keymap`), context menus (M3), and the
//! native menu bar (M8) all dispatch these same boxed actions, so each
//! command's logic exists exactly once.

use gpui::{Action, actions};

actions!(
    file_explorer,
    [
        // navigation
        OpenSelected,
        GoUp,
        GoBack,
        GoForward,
        FocusAddressBar,
        AcceptSuggestion,
        Refresh,
        // selection & movement (DirView cursor)
        SelectAll,
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        ExtendSelectionNext,
        ExtendSelectionPrev,
        PageUp,
        PageDown,
        // view
        ToggleHiddenFiles,
        // editing-mode (address bar / rename editor / dialogs)
        Confirm,
        Cancel,
    ]
);

/// Sort the active listing by a column (§0 "Sorting" row). Dispatched by
/// header clicks (mouse), never bound in the keymap.
///
/// `no_json`: the M1 keymap is code-generated from the §0 table, so JSON
/// deserialization is not needed until user keymap overrides land at M7
/// (avoids a schemars dependency until then).
#[derive(Clone, Copy, Debug, PartialEq, Action)]
#[action(namespace = file_explorer, no_json)]
pub struct SortBy {
    pub key: fs_core::SortKey,
}
