//! Key bindings: the defaults, transcribed 1:1 from the ARCHITECTURE.md §0
//! traceability table (M1/M2 rows + the M3 job-spine rows), and (M7b) the
//! user's JSON overrides layered on top. When a default changes, the table
//! changes in the same PR — it stays authoritative for defaults, and the
//! override file is expressed *against* it.
//!
//! `keymap.json` lives beside `settings.json` and reads like Zed's:
//!
//! ```json
//! [
//!   {
//!     "context": "DirView && !renaming",
//!     "bindings": {
//!       "cmd-shift-r": "RenameSelected",
//!       "backspace": null
//!     }
//!   }
//! ]
//! ```
//!
//! An action is named either fully (`file_explorer::RenameSelected`) or by its
//! bare name when that is unambiguous; `null` unbinds a key. Anything wrong
//! with a row — an action that does not exist, a keystroke that does not
//! parse, a context expression that does not compile — is **reported and
//! skipped**, never fatal: a typo in this file must not leave the app with no
//! keyboard. Overrides are applied by rebuilding the whole keymap (defaults
//! first, then the file), so removing a row from the file restores the
//! default it replaced.
//!
//! Key contexts (§3): `Workspace` (root), `Pane`, `DirView` (+ dynamic
//! `renaming` token), `AddressBar`, `TextInput`, and (M3) the modal
//! `ConflictDialog` / `ConfirmDialog` contexts. Every context is guarded by
//! a dispatch test — the tripwire for a missing `track_focus` on the node
//! carrying `key_context`, which gpui fails silently, not at compile time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs_core::Vfs;
use futures::StreamExt;
use gpui::{
    App, AppContext as _, BorrowAppContext as _, Global, KeyBinding, KeyBindingContextPredicate,
    NoAction, Task,
};
use serde::Deserialize;

use crate::actions::*;
use crate::app_state::FsContext;
use crate::watch_guard::BackgroundWatchGuard;

/// Debounce for `keymap.json`, matching the settings and themes watchers.
pub const KEYMAP_WATCH_LATENCY: Duration = Duration::from_millis(150);

/// Install the default keymap. Called once at boot (and by tests).
pub fn init(cx: &mut App) {
    bind_defaults(cx);
}

/// Install the defaults, then the user's `keymap.json` on top, and keep that
/// file watched. Boot calls this; `init` remains the defaults-only entry
/// point that every test uses.
pub fn init_with_overrides(cx: &mut App, path: PathBuf) {
    cx.set_global(UserKeymap {
        path: path.clone(),
        diagnostics: Vec::new(),
        _load: None,
        _watch: None,
        _guard: None,
    });
    init(cx);
    reload(cx);
    start_watching(cx, path);
}

/// `<config dir>/file-explorer/keymap.json` — a sibling of `settings.json`.
pub fn default_keymap_path() -> PathBuf {
    crate::settings::AppSettings::default_path()
        .parent()
        .map(|dir| dir.join("keymap.json"))
        .unwrap_or_else(|| PathBuf::from("keymap.json"))
}

/// What the user's `keymap.json` did, and what was wrong with it.
pub struct UserKeymap {
    path: PathBuf,
    diagnostics: Vec<String>,
    _load: Option<Task<()>>,
    _watch: Option<Task<()>>,
    _guard: Option<BackgroundWatchGuard>,
}

impl Global for UserKeymap {}

impl UserKeymap {
    /// Complaints about the file as last read — for the settings window, and
    /// the only way a user finds out a row was skipped.
    pub fn diagnostics(cx: &App) -> Vec<String> {
        cx.try_global::<UserKeymap>()
            .map(|keymap| keymap.diagnostics.clone())
            .unwrap_or_default()
    }
}

/// One `[{ context, bindings }]` entry.
#[derive(Debug, Deserialize)]
struct KeymapSection {
    /// Absent means "everywhere" — a binding with no context predicate.
    #[serde(default)]
    context: Option<String>,
    /// `null` unbinds; a string names an action; `[name, data]` names one
    /// that takes parameters (`Paste { into }`).
    #[serde(default)]
    bindings: BTreeMap<String, Option<ActionSpec>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ActionSpec {
    Name(String),
    WithData(String, serde_json::Value),
}

/// Rebuild the whole keymap: defaults, then `source` on top. Returns what was
/// wrong with the file, which is never allowed to be fatal.
fn apply(cx: &mut App, source: Option<&str>) -> Vec<String> {
    cx.clear_key_bindings();
    bind_defaults(cx);
    let Some(source) = source else {
        return Vec::new();
    };
    let sections: Vec<KeymapSection> = match serde_json::from_str(source) {
        Ok(sections) => sections,
        Err(error) => return vec![format!("keymap.json could not be read: {error}")],
    };

    let mut diagnostics = Vec::new();
    let mut bindings = Vec::new();
    for section in sections {
        let predicate = match section.context.as_deref() {
            Some(context) => match KeyBindingContextPredicate::parse(context) {
                Ok(predicate) => Some(std::rc::Rc::new(predicate)),
                Err(error) => {
                    diagnostics.push(format!("context `{context}` is not valid: {error}"));
                    continue;
                }
            },
            None => None,
        };
        for (keystrokes, spec) in section.bindings {
            let action = match spec {
                // `null`: gpui's own "this key does nothing here", which
                // shadows the default rather than deleting it.
                None => Box::new(NoAction) as Box<dyn gpui::Action>,
                Some(spec) => {
                    let (name, data) = match spec {
                        ActionSpec::Name(name) => (name, None),
                        ActionSpec::WithData(name, data) => (name, Some(data)),
                    };
                    match build_action(cx, &name, data) {
                        Ok(action) => action,
                        Err(error) => {
                            diagnostics.push(format!("`{keystrokes}`: {error}"));
                            continue;
                        }
                    }
                }
            };
            match KeyBinding::load(
                &keystrokes,
                action,
                predicate.clone(),
                false,
                None,
                cx.keyboard_mapper().as_ref(),
            ) {
                Ok(binding) => bindings.push(binding),
                Err(error) => {
                    diagnostics.push(format!("`{keystrokes}` is not a keystroke: {error}"))
                }
            }
        }
    }
    cx.bind_keys(bindings);
    diagnostics
}

/// Resolve an action by name. The fully-qualified form
/// (`file_explorer::RenameSelected`) always works; a bare `RenameSelected` is
/// accepted too when exactly one registered action ends that way, because
/// that is what a person writing this file by hand will type.
fn build_action(
    cx: &App,
    name: &str,
    data: Option<serde_json::Value>,
) -> anyhow::Result<Box<dyn gpui::Action>> {
    let build_error = match cx.build_action(name, data.clone()) {
        Ok(action) => return Ok(action),
        Err(error) => error,
    };
    // A fully-qualified name that failed did not fail for being unknown —
    // report what actually went wrong rather than searching for a name the
    // user already spelled out in full.
    if name.contains("::") {
        return Err(describe(cx, name, build_error));
    }
    let matches: Vec<&str> = cx
        .all_action_names()
        .iter()
        .copied()
        .filter(|full| full.rsplit("::").next() == Some(name))
        .collect();
    match matches.as_slice() {
        [full] => cx
            .build_action(full, data)
            .map_err(|error| describe(cx, full, error)),
        [] => Err(anyhow::anyhow!("no action named `{name}`")),
        many => Err(anyhow::anyhow!(
            "`{name}` is ambiguous: {}",
            many.join(", ")
        )),
    }
}

/// Turn gpui's build failure into something a person editing `keymap.json`
/// can act on. The interesting case is an action that *exists* but is
/// declared `no_json` because it carries parameters (`Paste`, `SortBy`,
/// `ToggleTag`) — "not found" would be a lie.
fn describe(cx: &App, name: &str, error: gpui::ActionBuildError) -> anyhow::Error {
    if cx.all_action_names().contains(&name) {
        anyhow::anyhow!(
            "`{name}` exists but cannot be bound from keymap.json (it takes parameters): {error}"
        )
    } else {
        anyhow::anyhow!("no action named `{name}`")
    }
}

/// Re-read `keymap.json` in the background and apply it.
pub fn reload(cx: &mut App) {
    let Some(keymap) = cx.try_global::<UserKeymap>() else {
        return;
    };
    let path = keymap.path.clone();
    let vfs = FsContext::global(cx).vfs.clone();
    let task = cx.spawn(async move |cx| {
        let source = cx
            .background_spawn(async move { read_keymap(vfs, path).await })
            .await;
        cx.update(|cx| {
            if !cx.has_global::<UserKeymap>() {
                return;
            }
            let diagnostics = apply(cx, source.as_deref());
            cx.update_global::<UserKeymap, _>(|keymap, _| keymap.diagnostics = diagnostics);
        });
    });
    cx.update_global::<UserKeymap, _>(|keymap, _| keymap._load = Some(task));
}

/// A missing file is not an error — most installs have no overrides.
async fn read_keymap(vfs: std::sync::Arc<dyn Vfs>, path: PathBuf) -> Option<String> {
    let bytes = vfs.load(&path).await.ok()?;
    String::from_utf8(bytes).ok()
}

/// Watch the **parent directory**: an atomic write replaces the file, which
/// some backends report as a directory change rather than a change to the
/// (now different) inode. Same shape as the settings and themes watchers.
fn start_watching(cx: &mut App, path: PathBuf) {
    let vfs = FsContext::global(cx).vfs.clone();
    let executor = cx.background_executor().clone();
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let watch_vfs = vfs.clone();
    let task = cx.spawn(async move |cx| {
        let (mut stream, guard) = cx
            .background_spawn(async move { watch_vfs.watch(&dir, KEYMAP_WATCH_LATENCY) })
            .await;
        let guard = BackgroundWatchGuard::new(guard, executor);
        let stored = cx.update(|cx| {
            if !cx.has_global::<UserKeymap>() {
                return false;
            }
            cx.update_global::<UserKeymap, _>(|keymap, _| keymap._guard = Some(guard));
            true
        });
        if !stored {
            return;
        }
        while let Some(batch) = stream.next().await {
            if !batch.iter().any(|event| event.path.as_ref() == path) {
                continue;
            }
            cx.update(reload);
        }
    });
    cx.update_global::<UserKeymap, _>(|keymap, _| keymap._watch = Some(task));
}

/// The §0 table itself.
fn bind_defaults(cx: &mut App) {
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
        // §0 Info panel toggle (M5). Workspace context for the same reasons as
        // the split: the workspace owns the right-hand column, and the
        // titlebar button dispatches this action with focus on the root.
        KeyBinding::new("cmd-shift-i", ToggleInfoPanel, Some("Workspace")),
        // §0 Search field focus (M6a). Workspace context like `cmd-l`: the
        // field belongs to the *active* pane, and the binding has to work with
        // focus anywhere in the window (including inside the other pane's
        // list). The workspace forwards it to that pane.
        KeyBinding::new("cmd-f", FocusSearch, Some("Workspace")),
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
                .on_action(record!(ToggleInfoPanel, "ToggleInfoPanel"))
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

    // ------------------------------------------------------------------
    // M7b: user overrides (`keymap.json`)
    // ------------------------------------------------------------------

    /// Apply an override document over the defaults and return whatever it
    /// complained about. Pure — no file, no watcher — so the parsing and
    /// binding rules are testable without booting the store.
    fn overrides(cx: &mut TestAppContext, source: &str) -> Vec<String> {
        cx.update(|cx| apply(cx, Some(source)))
    }

    #[gpui::test]
    fn an_override_replaces_the_default_for_that_key(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView");
        let complaints = cx.update(|_, cx| {
            apply(
                cx,
                Some(r#"[{ "context": "DirView", "bindings": { "f2": "Duplicate" } }]"#),
            )
        });
        assert!(complaints.is_empty(), "{complaints:?}");

        cx.simulate_keystrokes("f2 cmd-d");
        assert_eq!(
            *fired.borrow(),
            vec!["Duplicate", "Duplicate"],
            "f2 should now do what the file says, and cmd-d is untouched"
        );
    }

    #[gpui::test]
    fn a_fully_qualified_action_name_works_too(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView");
        let complaints = cx.update(|_, cx| {
            apply(
                cx,
                Some(
                    r#"[{ "context": "DirView",
                          "bindings": { "f2": "file_explorer::Duplicate" } }]"#,
                ),
            )
        });
        assert!(complaints.is_empty(), "{complaints:?}");
        cx.simulate_keystrokes("f2");
        assert_eq!(*fired.borrow(), vec!["Duplicate"]);
    }

    #[gpui::test]
    fn null_unbinds_a_default(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView");
        let complaints = cx.update(|_, cx| {
            apply(
                cx,
                Some(r#"[{ "context": "DirView", "bindings": { "f2": null } }]"#),
            )
        });
        assert!(complaints.is_empty(), "{complaints:?}");
        cx.simulate_keystrokes("f2 cmd-d");
        assert_eq!(
            *fired.borrow(),
            vec!["Duplicate"],
            "f2 was unbound, cmd-d still works"
        );
    }

    /// Dropping the row restores the default it replaced — overrides are
    /// applied by rebuilding, not by accumulating.
    #[gpui::test]
    fn removing_an_override_restores_the_default(cx: &mut TestAppContext) {
        let (fired, cx) = probe(cx, "DirView");
        cx.update(|_, cx| {
            apply(
                cx,
                Some(r#"[{ "context": "DirView", "bindings": { "f2": "Duplicate" } }]"#),
            )
        });
        cx.update(|_, cx| apply(cx, Some("[]")));
        cx.simulate_keystrokes("f2");
        assert_eq!(*fired.borrow(), vec!["RenameSelected"]);
    }

    /// Every way a row can be wrong is reported and skipped — and the
    /// defaults survive, because a typo in this file must never leave the app
    /// without a keyboard.
    #[gpui::test]
    fn every_broken_row_is_reported_and_skipped(cx: &mut TestAppContext) {
        let complaints = overrides(
            cx,
            r#"[{
                  "context": "DirView",
                  "bindings": {
                      "f2": "NoSuchAction",
                      "nonsense-key": "Duplicate"
                  }
              },
              { "context": "DirView &&", "bindings": { "cmd-9": "Duplicate" } }]"#,
        );
        assert_eq!(complaints.len(), 3, "{complaints:?}");
        assert!(complaints.iter().any(|c| c.contains("NoSuchAction")));
        assert!(complaints.iter().any(|c| c.contains("nonsense-key")));
        assert!(complaints.iter().any(|c| c.contains("DirView &&")));

        // ...and the defaults are still there.
        let (fired, cx) = probe(cx, "DirView");
        cx.update(|_, cx| {
            apply(
                cx,
                Some(r#"[{ "context": "DirView", "bindings": { "f2": "NoSuchAction" } }]"#),
            )
        });
        cx.simulate_keystrokes("f2");
        assert_eq!(*fired.borrow(), vec!["RenameSelected"]);
    }

    #[gpui::test]
    fn a_structurally_broken_file_leaves_the_defaults_alone(cx: &mut TestAppContext) {
        let complaints = overrides(cx, "{ not json");
        assert_eq!(complaints.len(), 1);
        assert!(
            complaints[0].contains("could not be read"),
            "{complaints:?}"
        );

        let (fired, cx) = probe(cx, "DirView");
        cx.update(|_, cx| apply(cx, Some("{ not json")));
        cx.simulate_keystrokes("f2");
        assert_eq!(*fired.borrow(), vec!["RenameSelected"]);
    }

    /// The three parameterized actions (`Paste`, `SortBy`, `ToggleTag`) are
    /// declared `no_json` and so cannot be built from the file. That is a
    /// recorded gap, not a mystery: the message has to say the action exists
    /// and why it cannot be bound, because "no action named `Paste`" would be
    /// a lie a user could not act on.
    #[gpui::test]
    fn a_parameterized_action_explains_why_it_cannot_be_bound(cx: &mut TestAppContext) {
        let complaints = overrides(
            cx,
            r#"[{ "context": "DirView",
                  "bindings": { "cmd-9": ["file_explorer::Paste", { "dest": "/tmp" }] } }]"#,
        );
        assert_eq!(complaints.len(), 1, "{complaints:?}");
        assert!(complaints[0].contains("takes parameters"), "{complaints:?}");
        assert!(
            complaints[0].contains("file_explorer::Paste"),
            "{complaints:?}"
        );
    }

    #[gpui::test]
    fn a_qualified_name_that_does_not_exist_says_so(cx: &mut TestAppContext) {
        let complaints = overrides(
            cx,
            r#"[{ "context": "DirView", "bindings": { "cmd-9": "file_explorer::Nope" } }]"#,
        );
        assert_eq!(complaints.len(), 1, "{complaints:?}");
        assert!(complaints[0].contains("no action named"), "{complaints:?}");
    }

    /// `Paste` exists in two namespaces (ours and the vendored text input's),
    /// so the bare name is genuinely ambiguous — and saying so, with both
    /// candidates, is more use than silently picking one.
    #[gpui::test]
    fn an_ambiguous_bare_name_names_its_candidates(cx: &mut TestAppContext) {
        let complaints = overrides(
            cx,
            r#"[{ "context": "DirView", "bindings": { "cmd-9": "Paste" } }]"#,
        );
        assert_eq!(complaints.len(), 1, "{complaints:?}");
        assert!(complaints[0].contains("ambiguous"), "{complaints:?}");
        assert!(
            complaints[0].contains("file_explorer::Paste"),
            "{complaints:?}"
        );
        assert!(
            complaints[0].contains("input_state::Paste"),
            "{complaints:?}"
        );
    }

    /// Editing `keymap.json` applies without a restart, like the themes
    /// folder and `settings.json`.
    #[gpui::test]
    fn editing_the_file_reloads_the_keymap(cx: &mut TestAppContext) {
        use crate::app_state::{GpuiSpawner, LoggingOpener};
        use fs_core::{FakeVfs, Spawner, StubPlatform};
        use std::sync::Arc;

        let path = PathBuf::from("/config/keymap.json");
        let vfs = cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_dir("/config");
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(StubPlatform::new()),
            );
            init_with_overrides(cx, path.clone());
            vfs
        });
        cx.run_until_parked();

        let fired: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        let probe_fired = fired.clone();
        let (probe, cx) = cx.add_window_view(|_, cx| ContextProbe {
            focus_handle: cx.focus_handle(),
            context: "DirView",
            fired: probe_fired,
        });
        cx.update(|window, cx| {
            let handle = probe.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("f2");
        assert_eq!(*fired.borrow(), vec!["RenameSelected"], "the default");

        vfs.write_file(
            &path,
            br#"[{ "context": "DirView", "bindings": { "f2": "Duplicate" } }]"#.to_vec(),
        );
        cx.executor()
            .advance_clock(KEYMAP_WATCH_LATENCY + Duration::from_millis(10));
        cx.run_until_parked();

        fired.borrow_mut().clear();
        cx.simulate_keystrokes("f2");
        assert_eq!(
            *fired.borrow(),
            vec!["Duplicate"],
            "the edited file did not reach the keymap"
        );
        cx.update(|_, cx| assert!(UserKeymap::diagnostics(cx).is_empty()));
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
        cx.simulate_keystrokes("cmd-shift-o cmd-shift-. cmd-shift-i");
        assert_eq!(
            *fired.borrow(),
            vec!["ToggleSplitPane", "ToggleHiddenFiles", "ToggleInfoPanel"]
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
