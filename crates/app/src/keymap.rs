//! Default key bindings, transcribed 1:1 from the ARCHITECTURE.md §0
//! traceability table (M1/M2 rows + the M3 job-spine rows). When a binding
//! changes, the table
//! changes in the same PR. JSON user overrides are deferred to M7; this table
//! stays authoritative for defaults.
//!
//! Key contexts (§3): `Workspace` (root), `Pane`, `DirView` (+ dynamic
//! `renaming` token), `AddressBar`, `TextInput`, and (M3) the modal
//! `ConflictDialog` / `ConfirmDialog` contexts. Every context is guarded by
//! a dispatch test — the tripwire for a missing `track_focus` on the node
//! carrying `key_context`, which gpui fails silently, not at compile time.

use gpui::{App, KeyBinding};

use crate::actions::*;

/// Install the default keymap. Called once at boot (and by tests).
pub fn init(cx: &mut App) {
    cx.bind_keys([
        // §0 Open item (M1)
        KeyBinding::new("enter", OpenSelected, Some("DirView && !renaming")),
        // §0 Go up (M1) — gpui names the large mac key `backspace`
        KeyBinding::new("backspace", GoUp, Some("DirView && !renaming")),
        KeyBinding::new("alt-up", GoUp, Some("DirView && !renaming")),
        // §0 Back / Forward (M1); mouse buttons 4/5 dispatch on the Pane div
        KeyBinding::new("cmd-[", GoBack, Some("Pane")),
        KeyBinding::new("cmd-]", GoForward, Some("Pane")),
        // §0 Address bar (M1)
        KeyBinding::new("cmd-l", FocusAddressBar, Some("Workspace")),
        KeyBinding::new("tab", AcceptSuggestion, Some("AddressBar")),
        KeyBinding::new("enter", Confirm, Some("TextInput")),
        KeyBinding::new("escape", Cancel, Some("TextInput")),
        // §0 Selection (M1)
        KeyBinding::new("cmd-a", SelectAll, Some("DirView && !renaming")),
        KeyBinding::new("down", SelectNext, Some("DirView && !renaming")),
        KeyBinding::new("up", SelectPrev, Some("DirView && !renaming")),
        KeyBinding::new("home", SelectFirst, Some("DirView && !renaming")),
        KeyBinding::new("end", SelectLast, Some("DirView && !renaming")),
        KeyBinding::new(
            "shift-down",
            ExtendSelectionNext,
            Some("DirView && !renaming"),
        ),
        KeyBinding::new(
            "shift-up",
            ExtendSelectionPrev,
            Some("DirView && !renaming"),
        ),
        // The horizontal half of the same §0 row, which exists only in the
        // M4 icon grid: `shift-down` there jumps a whole line, so without
        // these the grid could only grow a range by `cols` entries at a time.
        // Inert in the details list (see the action's own comment), and the
        // `TextInput` rows below win while an inline editor has focus.
        KeyBinding::new(
            "shift-right",
            ExtendSelectionRight,
            Some("DirView && !renaming"),
        ),
        KeyBinding::new(
            "shift-left",
            ExtendSelectionLeft,
            Some("DirView && !renaming"),
        ),
        KeyBinding::new("pageup", PageUp, Some("DirView && !renaming")),
        KeyBinding::new("pagedown", PageDown, Some("DirView && !renaming")),
        // §0 Views (M2): in-place folder expansion. The §0 "triangle click"
        // trigger is row-targeted mouse dispatch (like SortBy header clicks):
        // it calls DirView::toggle_expanded, the same single implementation
        // these cursor-relative actions funnel into.
        KeyBinding::new("right", ExpandSelected, Some("DirView && !renaming")),
        KeyBinding::new("left", CollapseSelected, Some("DirView && !renaming")),
        // §0 Cut/paste (M3): cut sources render dimmed; paste moves on cut
        KeyBinding::new("cmd-x", Cut, Some("DirView && !renaming")),
        KeyBinding::new("cmd-c", Copy, Some("DirView && !renaming")),
        KeyBinding::new(
            "cmd-v",
            // `cmd-v` always means "into the folder I am looking at"; only the
            // row context menu carries a destination of its own.
            Paste::default(),
            Some("DirView && !renaming"),
        ),
        // §0 Rename (M3): f2, or a slow second click handled entirely by
        // DirView's own click-arming state (never a keymap row).
        KeyBinding::new("f2", RenameSelected, Some("DirView && !renaming")),
        // toolbar "Duplicate selection" (M3)
        KeyBinding::new("cmd-d", Duplicate, Some("DirView && !renaming")),
        // §0 Delete (M3): plain delete → trash; shift-delete bypasses the
        // trash behind the ConfirmDialog guard
        KeyBinding::new("delete", DeleteToTrash, Some("DirView && !renaming")),
        KeyBinding::new(
            "shift-delete",
            DeletePermanently,
            Some("DirView && !renaming"),
        ),
        // §0 New folder (M3); New ▸ Text file… is context-menu only (no key)
        KeyBinding::new("cmd-shift-n", NewFolder, Some("Pane")),
        // §0 Context menu (M3): escape dismisses it. The `menu` token is on
        // the DirView node only while a menu is open (the same dynamic-token
        // shape as `renaming`), so this row is dead the rest of the time and
        // never shadows the rename editor's own `TextInput` escape.
        KeyBinding::new("escape", Cancel, Some("DirView && menu")),
        // §0 Hidden files (M1)
        KeyBinding::new("cmd-shift-.", ToggleHiddenFiles, Some("Workspace")),
        // §0 Refresh (M1)
        KeyBinding::new("cmd-r", Refresh, Some("Pane")),
        // §0 View mode switcher (M4). The §0 trigger column also names the
        // toolbar control (`pane.rs`'s segmented buttons), which dispatches
        // these same boxed actions. `SetViewColumns` is deliberately
        // *unbound*: Miller columns are a post-v1 stretch (§8), and the pane's
        // handler says so out loud rather than pretending to switch.
        KeyBinding::new("cmd-1", SetViewList, Some("Pane")),
        KeyBinding::new("cmd-2", SetViewIcons, Some("Pane")),
        // §0 Split-pane toggle (M4). Workspace context, not Pane: the
        // workspace owns `panes` and decides which pane survives a collapse,
        // and the binding must work with focus anywhere in the window.
        KeyBinding::new("cmd-shift-o", ToggleSplitPane, Some("Workspace")),
        // §0 Undo / Redo (M3)
        KeyBinding::new("cmd-z", Undo, Some("Workspace")),
        KeyBinding::new("cmd-shift-z", Redo, Some("Workspace")),
        // §0 Conflict dialog (M3)
        KeyBinding::new("r", ConflictReplace, Some("ConflictDialog")),
        KeyBinding::new("s", ConflictSkip, Some("ConflictDialog")),
        KeyBinding::new("k", ConflictKeepBoth, Some("ConflictDialog")),
        KeyBinding::new("a", ToggleApplyToAll, Some("ConflictDialog")),
        KeyBinding::new("enter", Confirm, Some("ConflictDialog")),
        KeyBinding::new("escape", Cancel, Some("ConflictDialog")),
        // §0 Delete-permanently confirmation dialog (M3)
        KeyBinding::new("enter", Confirm, Some("ConfirmDialog")),
        KeyBinding::new("escape", Cancel, Some("ConfirmDialog")),
    ]);

    // Editing keys inside the vendored text input (its own action namespace,
    // see crates/app/src/input/text_input.rs). Bound only in the TextInput
    // context so they never shadow the DirView/Pane bindings above.
    {
        use crate::input::text_input as ti;
        cx.bind_keys([
            KeyBinding::new("left", ti::Left, Some("TextInput")),
            KeyBinding::new("right", ti::Right, Some("TextInput")),
            KeyBinding::new("shift-left", ti::SelectLeft, Some("TextInput")),
            KeyBinding::new("shift-right", ti::SelectRight, Some("TextInput")),
            KeyBinding::new("cmd-a", ti::SelectAll, Some("TextInput")),
            KeyBinding::new("home", ti::Home, Some("TextInput")),
            KeyBinding::new("cmd-left", ti::Home, Some("TextInput")),
            KeyBinding::new("end", ti::End, Some("TextInput")),
            KeyBinding::new("cmd-right", ti::End, Some("TextInput")),
            KeyBinding::new("backspace", ti::Backspace, Some("TextInput")),
            KeyBinding::new("delete", ti::Delete, Some("TextInput")),
            KeyBinding::new("cmd-c", ti::Copy, Some("TextInput")),
            KeyBinding::new("cmd-x", ti::Cut, Some("TextInput")),
            KeyBinding::new("cmd-v", ti::Paste, Some("TextInput")),
        ]);
    }
}

#[cfg(test)]
mod tests {
    //! Dispatch guards for the M1 key contexts (§9 keymap row).
    //!
    //! `Workspace` and `Pane` are guarded through their real entities (see
    //! `workspace.rs` / `pane.rs` tests). The `DirView`, `AddressBar`, and
    //! `TextInput` entities are later M1 build steps, so their binding rows
    //! are guarded here with a probe view that declares the same key-context
    //! tokens those views will carry — proving each binding parses, matches
    //! its context (including the `!renaming` guard), and dispatches.

    use super::*;
    use gpui::{
        App, Context, FocusHandle, Focusable, IntoElement, Render, TestAppContext,
        VisualTestContext, Window, div, prelude::*,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    struct ContextProbe {
        focus_handle: FocusHandle,
        context: &'static str,
        fired: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Focusable for ContextProbe {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for ContextProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            macro_rules! record {
                ($action:ty, $name:literal) => {{
                    let fired = self.fired.clone();
                    move |_: &$action, _: &mut Window, _: &mut App| fired.borrow_mut().push($name)
                }};
            }
            div()
                .track_focus(&self.focus_handle)
                .key_context(self.context)
                .on_action(record!(OpenSelected, "OpenSelected"))
                .on_action(record!(GoUp, "GoUp"))
                .on_action(record!(SelectAll, "SelectAll"))
                .on_action(record!(SelectNext, "SelectNext"))
                .on_action(record!(SelectPrev, "SelectPrev"))
                .on_action(record!(SelectFirst, "SelectFirst"))
                .on_action(record!(SelectLast, "SelectLast"))
                .on_action(record!(ExtendSelectionNext, "ExtendSelectionNext"))
                .on_action(record!(ExtendSelectionPrev, "ExtendSelectionPrev"))
                .on_action(record!(ExtendSelectionRight, "ExtendSelectionRight"))
                .on_action(record!(ExtendSelectionLeft, "ExtendSelectionLeft"))
                .on_action(record!(PageUp, "PageUp"))
                .on_action(record!(PageDown, "PageDown"))
                .on_action(record!(ExpandSelected, "ExpandSelected"))
                .on_action(record!(CollapseSelected, "CollapseSelected"))
                .on_action(record!(Cut, "Cut"))
                .on_action(record!(Copy, "Copy"))
                .on_action(record!(Paste, "Paste"))
                .on_action(record!(DeleteToTrash, "DeleteToTrash"))
                .on_action(record!(DeletePermanently, "DeletePermanently"))
                .on_action(record!(NewFolder, "NewFolder"))
                .on_action(record!(SetViewList, "SetViewList"))
                .on_action(record!(SetViewIcons, "SetViewIcons"))
                .on_action(record!(SetViewColumns, "SetViewColumns"))
                .on_action(record!(RenameSelected, "RenameSelected"))
                .on_action(record!(Duplicate, "Duplicate"))
                .on_action(record!(AcceptSuggestion, "AcceptSuggestion"))
                .on_action(record!(Confirm, "Confirm"))
                .on_action(record!(Cancel, "Cancel"))
                .on_action(record!(Undo, "Undo"))
                .on_action(record!(Redo, "Redo"))
                .on_action(record!(ConflictReplace, "ConflictReplace"))
                .on_action(record!(ConflictSkip, "ConflictSkip"))
                .on_action(record!(ConflictKeepBoth, "ConflictKeepBoth"))
                .on_action(record!(ToggleApplyToAll, "ToggleApplyToAll"))
                .on_action(record!(ToggleHiddenFiles, "ToggleHiddenFiles"))
                .on_action(record!(ToggleSplitPane, "ToggleSplitPane"))
                .size_full()
        }
    }

    fn probe<'a>(
        cx: &'a mut TestAppContext,
        context: &'static str,
    ) -> (Rc<RefCell<Vec<&'static str>>>, &'a mut VisualTestContext) {
        cx.update(init);
        let fired: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let probe_fired = fired.clone();
        let (probe, cx) = cx.add_window_view(|_, cx| ContextProbe {
            focus_handle: cx.focus_handle(),
            context,
            fired: probe_fired,
        });
        cx.update(|window, cx| {
            let handle = probe.focus_handle(cx);
            window.focus(&handle, cx);
        });
        (fired, cx)
    }

    #[gpui::test]
    fn dir_view_context_dispatches_every_m1_binding(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView");
        cx.simulate_keystrokes("enter backspace alt-up cmd-a down up home end pageup pagedown");
        cx.simulate_keystrokes("shift-down shift-up shift-right shift-left right left");
        cx.simulate_keystrokes("cmd-x cmd-c cmd-v delete shift-delete");
        cx.simulate_keystrokes("f2 cmd-d");
        assert_eq!(
            *fired.borrow(),
            vec![
                "OpenSelected",
                "GoUp",
                "GoUp",
                "SelectAll",
                "SelectNext",
                "SelectPrev",
                "SelectFirst",
                "SelectLast",
                "PageUp",
                "PageDown",
                "ExtendSelectionNext",
                "ExtendSelectionPrev",
                "ExtendSelectionRight",
                "ExtendSelectionLeft",
                "ExpandSelected",
                "CollapseSelected",
                "Cut",
                "Copy",
                "Paste",
                "DeleteToTrash",
                "DeletePermanently",
                "RenameSelected",
                "Duplicate",
            ]
        );
    }

    // §9 dispatch guard for the `Pane` `cmd-shift-n` row (the real-entity
    // creation flow is covered in `pane.rs` tests).
    #[gpui::test]
    fn pane_context_dispatches_new_folder(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "Pane");
        cx.simulate_keystrokes("cmd-shift-n");
        assert_eq!(*fired.borrow(), vec!["NewFolder"]);
    }

    // §9 dispatch guard for the M4 `Pane` view-mode rows. The real entity's
    // state change is covered in `pane.rs` tests; this is the tripwire for
    // the bindings themselves.
    #[gpui::test]
    fn pane_context_dispatches_the_view_mode_rows(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "Pane");
        cx.simulate_keystrokes("cmd-1 cmd-2");
        assert_eq!(*fired.borrow(), vec!["SetViewList", "SetViewIcons"]);
    }

    // §9 dispatch guard for the M4 `Workspace` split-pane row. The real
    // entity's split/collapse behavior is covered in `workspace.rs` tests;
    // this is the tripwire for the binding and its context (a `Pane`-context
    // binding would fire only while a pane had focus, and the toolbar button
    // dispatches with focus on the workspace root).
    #[gpui::test]
    fn workspace_context_dispatches_the_split_pane_row(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "Workspace");
        cx.simulate_keystrokes("cmd-shift-o cmd-shift-.");
        assert_eq!(
            *fired.borrow(),
            vec!["ToggleSplitPane", "ToggleHiddenFiles"]
        );
    }

    // §8 marks Miller columns a post-v1 stretch, so `SetViewColumns` has no
    // binding at all — a key that quietly did nothing would be worse than no
    // key. This test is what fails if someone adds one without implementing
    // the view.
    #[gpui::test]
    fn set_view_columns_has_no_binding(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "Pane");
        cx.simulate_keystrokes("cmd-3");
        assert!(
            fired.borrow().is_empty(),
            "SetViewColumns must stay unbound while Miller columns are unimplemented, got {:?}",
            fired.borrow()
        );
    }

    #[gpui::test]
    fn renaming_token_suppresses_dir_view_bindings(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView renaming");
        // §0 guard: every DirView row — including delete, the clipboard
        // keys, rename, and duplicate — must stay dead while the rename
        // editor is up.
        cx.simulate_keystrokes("enter backspace cmd-a");
        cx.simulate_keystrokes("cmd-x cmd-c cmd-v delete shift-delete");
        cx.simulate_keystrokes("f2 cmd-d");
        assert!(
            fired.borrow().is_empty(),
            "`!renaming` guard must block DirView bindings while renaming, got {:?}",
            fired.borrow()
        );
    }

    // §9 dispatch guard for the §8 context-menu row: `escape` reaches
    // `Cancel` only while the `menu` token is on the DirView node.
    #[gpui::test]
    fn menu_token_binds_escape_to_cancel(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView menu");
        cx.simulate_keystrokes("escape");
        assert_eq!(*fired.borrow(), vec!["Cancel"]);
    }

    #[gpui::test]
    fn escape_is_dead_in_the_dir_view_without_an_open_menu(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView");
        cx.simulate_keystrokes("escape");
        assert!(
            fired.borrow().is_empty(),
            "escape must not dispatch Cancel with no menu open, got {:?}",
            fired.borrow()
        );
    }

    #[gpui::test]
    fn address_bar_context_dispatches_accept_suggestion(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "AddressBar TextInput");
        cx.simulate_keystrokes("tab");
        assert_eq!(*fired.borrow(), vec!["AcceptSuggestion"]);
    }

    #[gpui::test]
    fn text_input_context_dispatches_confirm_and_cancel(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "TextInput");
        cx.simulate_keystrokes("enter escape");
        assert_eq!(*fired.borrow(), vec!["Confirm", "Cancel"]);
    }

    // §9 dispatch guard for the `ConflictDialog` context: every §0 conflict
    // row (r/s/k/a/enter/escape) must reach a handler. The real entity is
    // additionally exercised end-to-end in `workspace.rs` tests.
    #[gpui::test]
    fn conflict_dialog_context_dispatches_every_m3_binding(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "ConflictDialog");
        cx.simulate_keystrokes("r s k a enter escape");
        assert_eq!(
            *fired.borrow(),
            vec![
                "ConflictReplace",
                "ConflictSkip",
                "ConflictKeepBoth",
                "ToggleApplyToAll",
                "Confirm",
                "Cancel",
            ]
        );
    }

    // §9 dispatch guard for the `ConfirmDialog` context (enter/escape).
    #[gpui::test]
    fn confirm_dialog_context_dispatches_confirm_and_cancel(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "ConfirmDialog");
        cx.simulate_keystrokes("enter escape");
        assert_eq!(*fired.borrow(), vec!["Confirm", "Cancel"]);
    }

    // §9 dispatch guard for the M3 `Workspace` undo/redo rows.
    #[gpui::test]
    fn workspace_context_dispatches_undo_and_redo(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "Workspace");
        cx.simulate_keystrokes("cmd-z cmd-shift-z");
        assert_eq!(*fired.borrow(), vec!["Undo", "Redo"]);
    }
}
