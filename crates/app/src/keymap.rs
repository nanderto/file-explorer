//! Default key bindings, transcribed 1:1 from the ARCHITECTURE.md §0
//! traceability table (M1 + M2 rows). When a binding changes, the table
//! changes in the same PR. JSON user overrides are deferred to M7; this table
//! stays authoritative for defaults.
//!
//! Key contexts (§3): `Workspace` (root), `Pane`, `DirView` (+ dynamic
//! `renaming` token), `AddressBar`, `TextInput`. Every context is guarded by
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
        KeyBinding::new("pageup", PageUp, Some("DirView && !renaming")),
        KeyBinding::new("pagedown", PageDown, Some("DirView && !renaming")),
        // §0 Views (M2): in-place folder expansion. The §0 "triangle click"
        // trigger is row-targeted mouse dispatch (like SortBy header clicks):
        // it calls DirView::toggle_expanded, the same single implementation
        // these cursor-relative actions funnel into.
        KeyBinding::new("right", ExpandSelected, Some("DirView && !renaming")),
        KeyBinding::new("left", CollapseSelected, Some("DirView && !renaming")),
        // §0 Hidden files (M1)
        KeyBinding::new("cmd-shift-.", ToggleHiddenFiles, Some("Workspace")),
        // §0 Refresh (M1)
        KeyBinding::new("cmd-r", Refresh, Some("Pane")),
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
                .on_action(record!(PageUp, "PageUp"))
                .on_action(record!(PageDown, "PageDown"))
                .on_action(record!(ExpandSelected, "ExpandSelected"))
                .on_action(record!(CollapseSelected, "CollapseSelected"))
                .on_action(record!(AcceptSuggestion, "AcceptSuggestion"))
                .on_action(record!(Confirm, "Confirm"))
                .on_action(record!(Cancel, "Cancel"))
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
        cx.simulate_keystrokes("shift-down shift-up right left");
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
                "ExpandSelected",
                "CollapseSelected",
            ]
        );
    }

    #[gpui::test]
    fn renaming_token_suppresses_dir_view_bindings(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView renaming");
        cx.simulate_keystrokes("enter backspace cmd-a");
        assert!(
            fired.borrow().is_empty(),
            "`!renaming` guard must block DirView bindings while renaming, got {:?}",
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
}
