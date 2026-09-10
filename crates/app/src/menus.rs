//! The macOS menu bar (§0's action set, ARCHITECTURE.md's M8 "menu bar with
//! full action set" — pulled forward, because without it the keyboard does
//! not work at all).
//!
//! **This is not decoration.** On macOS, `NSApplication` offers every
//! Command chord to the main menu before anything else sees it, and an app
//! with no main menu never gets those events back. Until this module existed,
//! *every* `cmd-` binding in the §0 table was dead in the real app — `cmd-l`,
//! `cmd-f`, `cmd-z`, `cmd-a`, `cmd-x/c/v`, `cmd-1/2`, `cmd-r`, `cmd-[`/`]`,
//! `cmd-shift-n/o/i/.` — while every unmodified key (Enter, F2, Delete,
//! arrows, type-ahead) worked. Nothing caught it: gpui's test platform
//! dispatches keystrokes straight into the keymap and never involves AppKit,
//! so the dispatch tests pass on bindings the OS never delivers.
//!
//! Every item here dispatches the **same boxed action** the keymap and the
//! context menus do (§3: one command, one implementation) — the menu is
//! another trigger, never a second code path. gpui reads the key equivalents
//! off the keymap, so the chords shown beside each item are by construction
//! the ones `keymap.rs` bound, including a user's `keymap.json` overrides.

use gpui::{App, Menu, MenuItem, OsAction};

use crate::actions::*;

/// Install the menu bar. Must run **after** `keymap::init*`, because gpui
/// reads each item's key equivalent out of the keymap as it builds the menu.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    rebuild(cx);
}

/// Rebuild the menu from the current keymap. Called again whenever
/// `keymap.json` is reloaded: the key equivalents are baked into the menu
/// when it is built, so without this a rebound chord leaves the **old** one
/// beside the item — and, because macOS resolves Command chords through the
/// menu, the old chord keeps working while the new one does not.
pub fn rebuild(cx: &mut App) {
    cx.set_menus(menus());
}

/// The menu tree. Split out so a test can assert its shape without a
/// platform: the interesting property is that every §0 action with a `cmd-`
/// binding appears somewhere, since an absent item means a dead chord.
pub fn menus() -> Vec<Menu> {
    vec![
        // The application menu. macOS titles this one after the app itself
        // and always shows it first.
        Menu {
            name: crate::APP_DISPLAY_NAME.into(),
            items: vec![
                MenuItem::action("Settings…", ToggleSettings),
                MenuItem::separator(),
                MenuItem::action("Quit File Explorer", Quit),
            ],
            disabled: false,
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Folder", NewFolder),
                MenuItem::action("New File", NewFile),
                MenuItem::separator(),
                MenuItem::action("Open", OpenSelected),
                MenuItem::action("Rename", RenameSelected),
                MenuItem::action("Duplicate", Duplicate),
                MenuItem::separator(),
                MenuItem::action("Move to Trash", DeleteToTrash),
                MenuItem::action("Delete Permanently", DeletePermanently),
            ],
            disabled: false,
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                // The six editing commands are declared with their **OS
                // action**, which is not a cosmetic detail. A plain
                // `MenuItem::action` binds the chord to one fixed action
                // whatever has focus, so `cmd-c` inside the rename editor
                // or the address bar would copy the selected *files*
                // instead of the selected text. Declaring the OS action
                // wires the item to AppKit's standard selector, so a
                // focused text field handles it natively and the app's
                // action fires only when nothing else claims it.
                MenuItem::os_action("Undo", Undo, OsAction::Undo),
                MenuItem::os_action("Redo", Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", Cut, OsAction::Cut),
                MenuItem::os_action("Copy", Copy, OsAction::Copy),
                MenuItem::os_action("Paste", Paste::default(), OsAction::Paste),
                MenuItem::separator(),
                MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("As List", SetViewList),
                MenuItem::action("As Icons", SetViewIcons),
                MenuItem::separator(),
                MenuItem::action("Show Hidden Files", ToggleHiddenFiles),
                MenuItem::action("Split Pane", ToggleSplitPane),
                MenuItem::action("Info Panel", ToggleInfoPanel),
                MenuItem::separator(),
                MenuItem::action("Refresh", Refresh),
            ],
            disabled: false,
        },
        Menu {
            name: "Go".into(),
            items: vec![
                MenuItem::action("Back", GoBack),
                MenuItem::action("Forward", GoForward),
                MenuItem::action("Enclosing Folder", GoUp),
                MenuItem::separator(),
                MenuItem::action("Go to Folder…", FocusAddressBar),
                MenuItem::action("Search", FocusSearch),
            ],
            disabled: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn item_actions(menus: &[Menu]) -> Vec<String> {
        menus
            .iter()
            .flat_map(|menu| &menu.items)
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.name().to_string()),
                _ => None,
            })
            .collect()
    }

    /// The regression guard for the bug this module exists to fix: a `cmd-`
    /// binding with no menu item is a chord macOS will eat before the window
    /// ever sees it. Read off the live keymap, so a *new* `cmd-` binding
    /// added without a menu item fails here rather than silently not working.
    #[gpui::test]
    fn every_command_chord_in_the_keymap_has_a_menu_item(cx: &mut TestAppContext) {
        cx.update(crate::keymap::init);
        let bound = cx.update(|cx| {
            crate::keymap::visible_bindings(cx)
                .into_iter()
                .filter(|row| row.uses_platform_modifier)
                .collect::<Vec<_>>()
        });
        assert!(
            !bound.is_empty(),
            "the keymap should have platform-modifier chords"
        );

        let in_menus = item_actions(&menus());
        let missing: Vec<&crate::keymap::BindingRow> = bound
            .iter()
            .filter(|row| {
                // The vendored text input's own editing chords are handled by
                // the focused field, not by the app's action set, and macOS
                // delivers them to the key view rather than the menu.
                !row.action.starts_with("input_state::") && !in_menus.contains(&row.action)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "these cmd- chords have no menu item, so macOS will swallow them: {missing:?}"
        );
    }

    /// Quit has to be here or the app cannot be quit from the keyboard at
    /// all — there is no window-close fallback for `cmd-q`.
    #[gpui::test]
    fn the_app_menu_offers_settings_and_quit(cx: &mut TestAppContext) {
        cx.update(crate::keymap::init);
        let menus = menus();
        let app_menu = &menus[0];
        assert_eq!(app_menu.name.as_ref(), crate::APP_DISPLAY_NAME);
        let actions = item_actions(std::slice::from_ref(app_menu));
        assert!(actions.iter().any(|name| name.ends_with("ToggleSettings")));
        assert!(actions.iter().any(|name| name.ends_with("Quit")));
    }
}
