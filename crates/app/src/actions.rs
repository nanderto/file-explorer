//! Every user command, as a gpui action (ARCHITECTURE.md §3).
//!
//! The list is exactly the Action column of the §0 traceability table's
//! M1/M2 rows plus the M3 job-spine rows (undo/redo, conflict dialog) —
//! later milestones grow it additively in the same PR that adds the table
//! rows. Keymap bindings (`crate::keymap`), context menus (M3), and the
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
        // search (M6a, §0 "Search field focus" — the workspace forwards it to
        // the active pane, whose toolbar owns the field)
        FocusSearch,
        Refresh,
        // selection & movement (DirView cursor)
        SelectAll,
        SelectNext,
        SelectPrev,
        SelectFirst,
        SelectLast,
        ExtendSelectionNext,
        ExtendSelectionPrev,
        // The horizontal half of §0's "Cursor movement (+shift- extends)",
        // which only the M4 icon grid has an axis for: in the details list
        // there is nothing to the left or right of a full-width row, so these
        // are deliberately inert there rather than aliases of up/down.
        ExtendSelectionRight,
        ExtendSelectionLeft,
        PageUp,
        PageDown,
        ExpandSelected,
        CollapseSelected,
        // clipboard & file operations (M3)
        Cut,
        Copy,
        DeleteToTrash,
        DeletePermanently,
        NewFolder,
        NewFile,
        RenameSelected,
        Duplicate,
        // view
        ToggleHiddenFiles,
        // view mode (M4, §0 "View mode switcher" — the pane owns the state)
        SetViewList,
        SetViewIcons,
        SetViewColumns,
        // dual pane (M4, §0 "Split-pane toggle" — the workspace owns the
        // pane strip; declared here so the whole §0 M4 row set exists in one
        // place)
        ToggleSplitPane,
        // info panel (M5, §0 "Info panel toggle" — the workspace owns the
        // right-hand column, so the action is handled there beside the split)
        ToggleInfoPanel,
        // editing-mode (address bar / rename editor / dialogs)
        Confirm,
        Cancel,
        // undo/redo (M3, Workspace → UndoStack)
        Undo,
        Redo,
        // conflict dialog (M3)
        ConflictReplace,
        ConflictSkip,
        ConflictKeepBoth,
        ToggleApplyToAll,
    ]
);

/// Paste the clipboard (§0 "Paste" row). `dest` is the folder to paste
/// **into**: `None` means the pane's open directory — what `cmd-v` and the
/// background context menu mean — and `Some(dir)` is the *row* context menu
/// pasting into the folder that was right-clicked, which is what Explorer
/// does. Parameterized rather than duplicated so the single `DirView` handler
/// still owns the whole operation (§3: one command, one implementation).
#[derive(Clone, Debug, Default, PartialEq, Action)]
#[action(namespace = file_explorer, no_json)]
pub struct Paste {
    pub dest: Option<std::path::PathBuf>,
}

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
