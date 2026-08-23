//! Context menus (ARCHITECTURE.md §8 "Context menu").
//!
//! The §8 row, implemented as specified: a single `Option<ContextMenuState>`
//! **field** of [`DirView`] (like `rename`, `marquee`, and `drop` — never its
//! own entity) carrying the invocation point, rendered as
//! `deferred(anchored().position(p))`, with right-click selecting its target
//! first and dismiss-on-click-away clearing the state.
//!
//! **Items dispatch actions, never handlers.** §0 is binding here: "Context
//! menus (M3) and the native menu bar (M8) dispatch the same boxed actions, so
//! each command's logic exists exactly once." Every row holds a
//! `Box<dyn Action>` from [`crate::actions`] and activating it calls
//! `window.dispatch_action` from the [`DirView`]'s focus handle — the same
//! path a keystroke takes, so the action lands on whichever entity owns the
//! command (the view for clipboard rows, the pane for `NewFolder`/`Refresh`/
//! `SortBy`, the workspace for `ToggleHiddenFiles`/`DeletePermanently`). No
//! menu item calls a method on a view.
//!
//! **Which menu.** Right-click is hit-tested with the same arithmetic the
//! marquee and the drop targets use ([`DirView::index_at_content`]), so
//! a `uniform_list`-virtualized row is found as reliably as a painted one: a
//! press on a row band opens the **row menu** (selecting that row first unless
//! it is already part of the selection), a press in the empty space below the
//! last row opens the **background menu**.
//!
//! **Nothing is hidden, only disabled** (plan §3 / Explorer): Paste with an
//! empty clipboard, Rename with a multi-selection, and every command on an
//! empty selection render greyed and inert rather than disappearing, so the
//! menu's shape is stable enough to learn.
//!
//! Every row carries a `debug_selector` keyed on its label, so a
//! `#[gpui::test]` clicks the row's **painted bounds** rather than a computed
//! guess — the menu's real geometry, including the `anchored()` fit, is part
//! of what the tests exercise.

use fs_core::{SortKey, SortSpec};
use gpui::{
    Action, AnyElement, App, Context, Div, MouseButton, MouseDownEvent, Pixels, Point,
    SharedString, Stateful, Window, anchored, deferred, div, point, prelude::*, px,
};

use crate::actions::{
    Copy, Cut, DeletePermanently, DeleteToTrash, Duplicate, NewFile, NewFolder, OpenSelected,
    Paste, Refresh, RenameSelected, SortBy, ToggleHiddenFiles,
};
use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::marquee::{ContentPoint, list_viewport, scroll_y};

// ----------------------------------------------------------------------
// Geometry. Fixed row heights, so a panel's height is its content's — no
// scrolling, no measurement, and a `debug_bounds` lookup in a test lands on
// the same pixel a user's pointer would.
// ----------------------------------------------------------------------

/// Panel width; submenus get their own, narrower one.
const MENU_WIDTH: f32 = 210.0;
const SUBMENU_WIDTH: f32 = 150.0;
/// Height of one command row.
const MENU_ITEM_HEIGHT: f32 = 22.0;
/// Height of a separator row: a 1px rule with 3px of air either side.
const MENU_SEPARATOR_HEIGHT: f32 = 7.0;
/// Panel padding and border, which every row offset starts past.
const MENU_PADDING: f32 = 4.0;
const MENU_BORDER: f32 = 1.0;
/// Leading gutter inside a row, holding the ✓ of a checked item.
const MENU_GUTTER: f32 = 14.0;
/// How far a submenu overlaps its parent panel's right edge, so sliding the
/// pointer across the seam never leaves the menu.
const SUBMENU_OVERLAP: f32 = 2.0;
/// Hover tint, from the theme accent (the app crate never names a color).
const MENU_HOVER_ALPHA: f32 = 0.35;

// ----------------------------------------------------------------------
// Items
// ----------------------------------------------------------------------

/// One command row: the label, the boxed action it dispatches, whether it can
/// apply right now, and whether it renders a ✓ (a toggle that is on).
pub struct MenuCommand {
    pub label: SharedString,
    pub action: Box<dyn Action>,
    pub enabled: bool,
    pub checked: bool,
}

/// A row of a menu. Submenus are deliberately **one level deep** — that is
/// everything plan §3 asks for (`New ▸`, `Sort by ▸`), and it keeps a submenu
/// row's own children flat.
pub enum MenuItem {
    Command(MenuCommand),
    Submenu {
        label: SharedString,
        items: Vec<MenuCommand>,
    },
    Separator,
}

impl MenuItem {
    /// The row's visible text (`""` for a separator).
    pub fn label(&self) -> &str {
        match self {
            MenuItem::Command(command) => &command.label,
            MenuItem::Submenu { label, .. } => label,
            MenuItem::Separator => "",
        }
    }

    pub fn command(&self) -> Option<&MenuCommand> {
        match self {
            MenuItem::Command(command) => Some(command),
            _ => None,
        }
    }

    pub fn submenu(&self) -> Option<&[MenuCommand]> {
        match self {
            MenuItem::Submenu { items, .. } => Some(items),
            _ => None,
        }
    }
}

fn command(label: &'static str, action: Box<dyn Action>, enabled: bool) -> MenuItem {
    MenuItem::Command(MenuCommand {
        label: SharedString::new_static(label),
        action,
        enabled,
        checked: false,
    })
}

fn toggle(label: &'static str, action: Box<dyn Action>, checked: bool) -> MenuItem {
    MenuItem::Command(MenuCommand {
        label: SharedString::new_static(label),
        action,
        enabled: true,
        checked,
    })
}

fn sub_command(label: &'static str, action: Box<dyn Action>, enabled: bool) -> MenuCommand {
    MenuCommand {
        label: SharedString::new_static(label),
        action,
        enabled,
        checked: false,
    }
}

/// One `Sort by ▸` row. The **active** column renders checked *and disabled*:
/// `SortBy` means "sort by this column, flipping direction if it is already
/// the one" — right for a column-header click, wrong for a menu whose only
/// feedback is a ✓ that does not move, where re-picking the checked row would
/// silently reverse the listing. Explorer's submenu carries explicit
/// Ascending / Descending rows; until the action set is parameterized (see
/// AS_BUILT "Known gaps") the checked row is simply inert.
fn sort_command(label: &'static str, key: SortKey, sort: SortSpec) -> MenuCommand {
    let active = sort.key == key;
    MenuCommand {
        label: SharedString::new_static(label),
        action: Box::new(SortBy { key }),
        enabled: !active,
        checked: active,
    }
}

/// Everything a menu's shape depends on, gathered once so the builders below
/// stay pure (and headlessly testable — enabling rules are where a context
/// menu actually goes wrong).
#[derive(Clone, Debug)]
pub struct MenuFacts {
    /// How many rows are selected *after* the right-click's own selection.
    pub selection_len: usize,
    /// Whether the pane has a directory open at all.
    pub has_dir: bool,
    pub clipboard_empty: bool,
    pub show_hidden: bool,
    pub sort: SortSpec,
}

impl MenuFacts {
    /// Paste needs somewhere to paste into *and* something to paste.
    fn can_paste(&self) -> bool {
        self.has_dir && !self.clipboard_empty
    }
}

/// The row menu (right-click on an entry). Explorer's order and wording.
///
/// `Paste` here pastes into the pane's **current directory**, not into a
/// right-clicked folder: it dispatches the one `Paste` action, and giving the
/// menu a destination of its own would mean a second implementation of paste.
pub fn row_menu(facts: &MenuFacts) -> Vec<MenuItem> {
    let any = facts.selection_len > 0;
    vec![
        // Enabled for *any* selection because `OpenSelected` opens all of it
        // (Explorer behavior), not just the cursor row.
        command("Open", Box::new(OpenSelected), any),
        MenuItem::Separator,
        command("Cut", Box::new(Cut), any),
        command("Copy", Box::new(Copy), any),
        command("Paste", Box::new(Paste), facts.can_paste()),
        MenuItem::Separator,
        command("Duplicate", Box::new(Duplicate), any),
        // One name, one editor: renaming a multi-selection is meaningless.
        command("Rename", Box::new(RenameSelected), facts.selection_len == 1),
        MenuItem::Separator,
        command("Delete", Box::new(DeleteToTrash), any),
        // §0 "Bypass trash (confirm dialog first)": the workspace's
        // ConfirmDialog is the guard, exactly as for shift-delete.
        command("Delete Permanently", Box::new(DeletePermanently), any),
    ]
}

/// The background menu (right-click in the empty space below the rows). Every
/// command here acts on the folder, not on the selection — which is why a
/// background right-click leaves the selection alone.
///
/// `New ▸ Text file…` is this menu's reason to exist: plan §3 gives `NewFile`
/// no key binding at all, so the context menu is its **only** entry point.
pub fn background_menu(facts: &MenuFacts) -> Vec<MenuItem> {
    vec![
        command("Paste", Box::new(Paste), facts.can_paste()),
        MenuItem::Separator,
        MenuItem::Submenu {
            label: SharedString::new_static("New"),
            items: vec![
                sub_command("Folder", Box::new(NewFolder), facts.has_dir),
                sub_command("Text File…", Box::new(NewFile), facts.has_dir),
            ],
        },
        MenuItem::Separator,
        command("Refresh", Box::new(Refresh), facts.has_dir),
        MenuItem::Submenu {
            label: SharedString::new_static("Sort by"),
            items: vec![
                sort_command("Name", SortKey::Name, facts.sort),
                sort_command("Size", SortKey::Size, facts.sort),
                sort_command("Date Modified", SortKey::DateModified, facts.sort),
            ],
        },
        MenuItem::Separator,
        toggle(
            "Show Hidden Files",
            Box::new(ToggleHiddenFiles),
            facts.show_hidden,
        ),
    ]
}

// ----------------------------------------------------------------------
// The state (§8's `Option<(menu_state, Point, Subscription)>`)
// ----------------------------------------------------------------------

/// One open menu. Lives at `DirView.menu`; dropping it closes the menu.
pub struct ContextMenuState {
    /// Where the right-click landed, in **window** coordinates — §8's `Point`,
    /// fed straight to `anchored().position(p)`.
    position: Point<Pixels>,
    items: Vec<MenuItem>,
    /// Which row's submenu is open (hover or click); at most one.
    submenu: Option<usize>,
}

impl ContextMenuState {
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    pub fn position(&self) -> Point<Pixels> {
        self.position
    }

    pub fn submenu(&self) -> Option<usize> {
        self.submenu
    }
}

// ----------------------------------------------------------------------
// The machine, as DirView methods (a field's machine, like rename.rs)
// ----------------------------------------------------------------------

impl DirView {
    /// Right press anywhere on the list surface: pick the target, select it if
    /// it is not already selected, and open the matching menu.
    pub(crate) fn open_context_menu(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The inline editor owns the view while it is up (same rule as the
        // marquee): a menu here would fight it for the selection.
        if self.rename.is_some() {
            return;
        }
        // Nothing else may treat this press as a marquee arm or a row click.
        cx.stop_propagation();
        // A right-click focuses the list, which is also what makes
        // `dispatch_action` land on this view's node (and bubble from there to
        // the pane and the workspace).
        window.focus(self.focus_handle_ref(), cx);
        self.disarm_rename_click();

        let target = self.row_at_pointer(event.position, cx);
        let items = match target {
            Some(entry) => {
                // Explorer: right-clicking a row outside the selection selects
                // it; right-clicking inside a multi-selection keeps the whole
                // selection and only moves the cursor, so the menu's commands
                // still act on everything that looks selected.
                if self.selection().is_selected(&entry.id()) {
                    self.restore_cursor(Some(entry.id()), cx);
                } else {
                    self.select_entry(&entry, cx);
                }
                row_menu(&self.menu_facts(cx))
            }
            None => background_menu(&self.menu_facts(cx)),
        };
        self.menu = Some(ContextMenuState {
            position: event.position,
            items,
            submenu: None,
        });
        cx.notify();
    }

    /// The projected row (or grid tile) under a window-space pointer, or
    /// `None` for empty space. Arithmetic against the painted lattice, so a
    /// virtualized item is found like any other.
    fn row_at_pointer(&self, pointer: Point<Pixels>, cx: &App) -> Option<fs_core::FileEntry> {
        let viewport = list_viewport(self);
        if viewport.size.height <= px(0.0) {
            return None;
        }
        let content = ContentPoint::from_window(pointer, viewport, scroll_y(self));
        // Mode-aware (rows vs tiles) — see `DirView::index_at_content`.
        self.index_at_content(content, cx)
            .and_then(|ix| self.flat_rows().get(ix))
            // A not-yet-created phantom row (§4c `New ▸`) is not a menu target;
            // it can only exist while the editor is up, which already returned.
            .filter(|row| !self.is_new_entry_row(row))
            .map(|row| row.entry.clone())
    }

    fn menu_facts(&self, cx: &App) -> MenuFacts {
        let pane = self.pane_entity();
        MenuFacts {
            selection_len: self.selection().len(),
            has_dir: pane
                .as_ref()
                .is_some_and(|pane| pane.read(cx).path().is_some()),
            clipboard_empty: FsContext::global(cx).clipboard.is_empty(),
            show_hidden: pane
                .as_ref()
                .is_some_and(|pane| pane.read(cx).show_hidden()),
            sort: pane
                .as_ref()
                .map(|pane| pane.read(cx).sort())
                .unwrap_or_default(),
        }
    }

    /// Dismiss: `escape` (the `menu` key-context token), a click away, or an
    /// item being activated. Focus goes back to the list.
    pub(crate) fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            window.focus(self.focus_handle_ref(), cx);
            cx.notify();
        }
    }

    pub(crate) fn context_menu(&self) -> Option<&ContextMenuState> {
        self.menu.as_ref()
    }

    /// Activate a row: close the menu, then dispatch its action from this
    /// view's focus handle (§0 — the keymap's own path).
    fn activate_menu_item(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_context_menu(window, cx);
        window.dispatch_action(action, cx);
    }

    /// Open (or close) one row's submenu. Hovering any other row closes it,
    /// which is the whole of the submenu's lifetime management.
    fn set_open_submenu(&mut self, submenu: Option<usize>, cx: &mut Context<Self>) {
        if let Some(menu) = self.menu.as_mut()
            && menu.submenu != submenu
        {
            menu.submenu = submenu;
            cx.notify();
        }
    }
}

// ----------------------------------------------------------------------
// Render
// ----------------------------------------------------------------------

/// Add the right-click trigger and the menu overlay to the list surface — the
/// same element the marquee and the drop targets hang off, so the menu adds no
/// layout node and the row hit test has one geometry to agree with.
pub(crate) fn with_context_menu(
    surface: Stateful<Div>,
    view: &DirView,
    window: &mut Window,
    cx: &mut Context<DirView>,
) -> Stateful<Div> {
    let menu = render_menu(view, window, cx).unwrap_or_default();
    surface
        .on_mouse_down(MouseButton::Right, cx.listener(DirView::open_context_menu))
        .children(menu)
}

/// The overlay: an invisible full-window **scrim** that swallows the next
/// press anywhere else (§8 "dismiss-on-click-away"), then the panel itself
/// above it.
///
/// The scrim, rather than an `on_mouse_down_out` on the panel, is what makes
/// dismissal correct with a submenu open: a submenu panel is positioned
/// *outside* its parent panel's bounds, so an out-handler on the parent would
/// fire on the way to clicking a submenu row and close the menu under the
/// pointer. Both panels `occlude`, so a press on either never reaches the
/// scrim; a press anywhere else does. It also gets the Explorer behavior that
/// the click which dismisses a menu does not also act on what it landed on.
fn render_menu(
    view: &DirView,
    window: &mut Window,
    cx: &mut Context<DirView>,
) -> Option<Vec<AnyElement>> {
    let state = view.context_menu()?;
    let theme = view.theme().clone();
    let open_submenu = state.submenu;
    let items = state.items();
    let viewport = window.viewport_size();

    let scrim = div()
        .id("context-menu-scrim")
        .occlude()
        .w(viewport.width)
        .h(viewport.height)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.close_context_menu(window, cx);
            }),
        )
        // A right-press dismisses and, if it landed in the list, immediately
        // opens the menu for wherever it landed.
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.close_context_menu(window, cx);
                if list_viewport(this).contains(&event.position) {
                    this.open_context_menu(event, window, cx);
                }
            }),
        );

    let mut panel = div()
        .id("context-menu")
        .debug_selector(|| "context-menu".to_string())
        // Clicks on the menu must not reach the rows (or the scrim) beneath it.
        .occlude()
        .relative()
        .flex()
        .flex_col()
        .w(px(MENU_WIDTH))
        .p(px(MENU_PADDING))
        .border(px(MENU_BORDER))
        .border_color(theme.border)
        .rounded(px(6.0))
        .bg(theme.panel)
        .text_size(px(12.0));

    for (ix, item) in items.iter().enumerate() {
        panel = panel.child(match item {
            MenuItem::Separator => div()
                .h(px(MENU_SEPARATOR_HEIGHT))
                .flex()
                .items_center()
                .child(div().h(px(1.0)).w_full().bg(theme.border))
                .into_any_element(),
            MenuItem::Command(command) => {
                render_command(command, ("context-menu-item", ix), None, &theme, cx)
                    .into_any_element()
            }
            MenuItem::Submenu {
                label,
                items: children,
            } => render_submenu_row(
                label.clone(),
                children,
                ix,
                open_submenu == Some(ix),
                &theme,
                cx,
            )
            .into_any_element(),
        });
    }

    Some(vec![
        deferred(
            anchored()
                .position(point(px(0.0), px(0.0)))
                .snap_to_window()
                .child(scrim),
        )
        .into_any_element(),
        // Above the row list, the drop highlights, and the scrim.
        deferred(anchored().position(state.position()).child(panel))
            .with_priority(1)
            .into_any_element(),
    ])
}

/// One command row. A disabled row gets no click listener and no hover tint,
/// so clicking it does nothing at all — not even dismiss the menu (Explorer).
fn render_command(
    command: &MenuCommand,
    id: (&'static str, usize),
    // `Some(parent_ix)` when this row lives inside that submenu; `None` for a
    // top-level row.
    submenu_of: Option<usize>,
    theme: &crate::theme::Theme,
    cx: &mut Context<DirView>,
) -> Stateful<Div> {
    let mut row = div()
        .id(id)
        .debug_selector({
            let label = command.label.clone();
            move || format!("context-menu-item-{label}")
        })
        .flex()
        .items_center()
        .h(px(MENU_ITEM_HEIGHT))
        .px(px(6.0))
        .rounded(px(3.0))
        .text_color(if command.enabled {
            theme.text
        } else {
            theme.muted
        })
        .child(
            div()
                .w(px(MENU_GUTTER))
                .flex_none()
                .child(SharedString::new_static(if command.checked {
                    "✓"
                } else {
                    ""
                })),
        )
        .child(div().flex_1().truncate().child(command.label.clone()));
    if command.enabled {
        let action = command.action.boxed_clone();
        let hover = theme.accent.opacity(MENU_HOVER_ALPHA);
        row = row
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            // Hovering a top-level row closes whatever submenu was open;
            // hovering a submenu's own child keeps that submenu open, which
            // is the only reason `submenu_of` is threaded down here.
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if *hovered {
                    this.set_open_submenu(submenu_of, cx);
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_menu_item(action.boxed_clone(), window, cx);
            }));
    }
    row
}

/// A `label ▸` row plus, when open, its one-level submenu panel positioned to
/// the right of it, overlapping the seam so the pointer can slide across.
fn render_submenu_row(
    label: SharedString,
    children: &[MenuCommand],
    ix: usize,
    open: bool,
    theme: &crate::theme::Theme,
    cx: &mut Context<DirView>,
) -> Stateful<Div> {
    let hover = theme.accent.opacity(MENU_HOVER_ALPHA);
    let mut row = div()
        .id(("context-menu-submenu", ix))
        .debug_selector({
            let label = label.clone();
            move || format!("context-menu-item-{label}")
        })
        .relative()
        .flex()
        .items_center()
        .h(px(MENU_ITEM_HEIGHT))
        .px(px(6.0))
        .rounded(px(3.0))
        .text_color(theme.text)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered {
                this.set_open_submenu(Some(ix), cx);
            }
        }))
        // Clicking the parent row opens it too — the submenu is reachable
        // without a hover, which is also what makes it clickable in a test.
        .on_click(cx.listener(move |this, _, _, cx| this.set_open_submenu(Some(ix), cx)))
        .child(div().w(px(MENU_GUTTER)).flex_none())
        .child(div().flex_1().truncate().child(label))
        .child(
            div()
                .flex_none()
                .text_color(theme.muted)
                .child(SharedString::new_static("▸")),
        );

    if open {
        let mut panel = div()
            .id(("context-submenu", ix))
            .occlude()
            .absolute()
            // The parent row's box starts one border + one padding inside the
            // panel, which the submenu's own origin has to undo.
            .left(px(MENU_WIDTH
                - SUBMENU_OVERLAP
                - MENU_BORDER
                - MENU_PADDING))
            .top(px(-(MENU_BORDER + MENU_PADDING)))
            .flex()
            .flex_col()
            .w(px(SUBMENU_WIDTH))
            .p(px(MENU_PADDING))
            .border(px(MENU_BORDER))
            .border_color(theme.border)
            .rounded(px(6.0))
            .bg(theme.panel);
        for (child_ix, child) in children.iter().enumerate() {
            panel = panel.child(render_command(
                child,
                ("context-submenu-item", ix * 16 + child_ix),
                Some(ix),
                theme,
                cx,
            ));
        }
        row = row.child(panel);
    }
    row
}

#[cfg(test)]
mod tests {
    //! §9 context-menu rows. The enabling rules first, headlessly — they are
    //! where a context menu actually goes wrong. Then the menu itself, driven
    //! by real simulated right-clicks and by real clicks on the **painted
    //! bounds** of real rows (`debug_selector` → `debug_bounds`, so a test
    //! clicks where the pixel is rather than where it ought to be), asserting
    //! the *effect* of each dispatch — a job on the queue, an editor open, a
    //! sort column flipped — never just that a click happened.

    use super::*;
    use crate::views::details_list::ROW_HEIGHT;

    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::pane::Pane;
    use crate::theme::Theme;
    use fs_core::{ClipboardMode, EntryId, FakeVfs, SortDirection, SortSpec, Spawner};
    use gpui::{Bounds, Entity, Modifiers, TestAppContext, VisualTestContext};
    use serde_json::json;

    fn facts() -> MenuFacts {
        MenuFacts {
            selection_len: 1,
            has_dir: true,
            clipboard_empty: true,
            show_hidden: false,
            sort: SortSpec::default(),
        }
    }

    fn find(items: &[MenuItem], label: &str) -> usize {
        items
            .iter()
            .position(|item| item.label() == label)
            .unwrap_or_else(|| panic!("no menu item labelled {label:?}"))
    }

    fn enabled(items: &[MenuItem], label: &str) -> bool {
        items[find(items, label)]
            .command()
            .expect("a command row")
            .enabled
    }

    // ------------------------------------------------------------------
    // The item builders (pure)
    // ------------------------------------------------------------------

    #[test]
    fn row_menu_dispatches_the_keymap_actions_for_every_command() {
        let items = row_menu(&facts());
        let named: Vec<&str> = items
            .iter()
            .filter_map(MenuItem::command)
            .map(|command| command.action.name())
            .collect();
        assert_eq!(
            named,
            vec![
                "file_explorer::OpenSelected",
                "file_explorer::Cut",
                "file_explorer::Copy",
                "file_explorer::Paste",
                "file_explorer::Duplicate",
                "file_explorer::RenameSelected",
                "file_explorer::DeleteToTrash",
                "file_explorer::DeletePermanently",
            ],
            "every row-menu command dispatches an existing §0 action"
        );
    }

    #[test]
    fn background_menu_is_the_only_entry_point_for_new_file() {
        let items = background_menu(&facts());
        let new = items[find(&items, "New")].submenu().unwrap();
        let named: Vec<&str> = new.iter().map(|c| c.action.name()).collect();
        assert_eq!(
            named,
            vec!["file_explorer::NewFolder", "file_explorer::NewFile"]
        );

        let sort = items[find(&items, "Sort by")].submenu().unwrap();
        assert!(
            sort.iter()
                .all(|c| c.action.name() == "file_explorer::SortBy"),
            "sort rows dispatch the SortBy action, not a handler"
        );
        assert!(sort[0].checked, "the default sort column is checked");
        assert!(
            !sort[0].enabled,
            "and inert: re-picking the active column would silently reverse \
             the direction, with the ✓ never moving to say so"
        );
        assert!(!sort[1].checked);
        assert!(sort[1].enabled, "the other columns are live");
        assert!(
            sort[2].action.partial_eq(&SortBy {
                key: SortKey::DateModified
            }),
            "each Sort by row carries its own key"
        );
    }

    #[test]
    fn commands_that_cannot_apply_are_disabled_not_absent() {
        let base = facts();
        // Empty clipboard: Paste present, disabled, in both menus.
        for items in [row_menu(&base), background_menu(&base)] {
            assert!(
                !enabled(&items, "Paste"),
                "Paste with an empty clipboard is disabled, not hidden"
            );
        }
        let items = background_menu(&MenuFacts {
            clipboard_empty: false,
            ..base.clone()
        });
        assert!(enabled(&items, "Paste"));

        // Empty selection: every row command is dead, none of them vanish.
        let items = row_menu(&MenuFacts {
            selection_len: 0,
            ..base.clone()
        });
        for label in [
            "Open",
            "Cut",
            "Copy",
            "Duplicate",
            "Rename",
            "Delete",
            "Delete Permanently",
        ] {
            assert!(
                !enabled(&items, label),
                "{label} must be disabled with nothing selected"
            );
        }

        // Rename needs exactly one target; deleting many is fine.
        let many = row_menu(&MenuFacts {
            selection_len: 3,
            ..base.clone()
        });
        assert!(!enabled(&many, "Rename"));
        assert!(enabled(&many, "Delete"));

        // No directory open: creation and refresh are dead.
        let items = background_menu(&MenuFacts {
            has_dir: false,
            ..base
        });
        assert!(!enabled(&items, "Refresh"));
        assert!(
            items[find(&items, "New")]
                .submenu()
                .unwrap()
                .iter()
                .all(|c| !c.enabled)
        );
    }

    #[test]
    fn show_hidden_files_renders_its_toggle_state() {
        let checked = |show_hidden| {
            let items = background_menu(&MenuFacts {
                show_hidden,
                ..facts()
            });
            items[find(&items, "Show Hidden Files")]
                .command()
                .unwrap()
                .checked
        };
        assert!(!checked(false));
        assert!(checked(true));
    }

    // ------------------------------------------------------------------
    // The real thing
    // ------------------------------------------------------------------

    fn open_root(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeVfs>,
        Entity<Pane>,
        Entity<DirView>,
        &mut VisualTestContext,
    ) {
        let vfs = cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({
                    "a.txt": "a",
                    "b.txt": "bb",
                    "c.txt": "ccc",
                    "d.txt": "dddd",
                }),
            );
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
            vfs
        });
        let (pane, cx) = cx.add_window_view(|window, cx| Pane::new(Theme::dark(), window, cx));
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        let view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        // The list has keyboard focus, as it does the moment a real window
        // opens (the workspace focuses its pane) — otherwise `cmd-shift-n`,
        // bound in the `Pane` context, resolves against no context at all.
        cx.update(|window, cx| {
            let handle = view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });
        cx.run_until_parked();
        (vfs, pane, view, cx)
    }

    fn tree_has(vfs: &Arc<FakeVfs>, path: &str) -> bool {
        vfs.snapshot().keys().any(|p| p == Path::new(path))
    }

    /// The window point of projected row `ix`'s vertical middle, `x` px in
    /// from the list's left edge.
    fn row_point_at(
        view: &Entity<DirView>,
        cx: &mut VisualTestContext,
        ix: usize,
        x: f32,
    ) -> Point<Pixels> {
        let viewport = view.read_with(cx, |view, _| list_viewport(view));
        point(
            viewport.left() + px(x),
            viewport.top() + px(ix as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0),
        )
    }

    fn row_point(view: &Entity<DirView>, cx: &mut VisualTestContext, ix: usize) -> Point<Pixels> {
        row_point_at(view, cx, ix, 40.0)
    }

    /// A point in the empty space below the last row.
    fn background_point(view: &Entity<DirView>, cx: &mut VisualTestContext) -> Point<Pixels> {
        let rows = view.read_with(cx, |view, _| view.flat_rows().len());
        row_point(view, cx, rows + 1)
    }

    fn right_click(cx: &mut VisualTestContext, at: Point<Pixels>) {
        cx.simulate_mouse_down(at, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(at, MouseButton::Right, Modifiers::none());
        cx.run_until_parked();
    }

    /// Where the menu row with this selector was actually painted.
    fn item_bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("no painted menu row {selector:?}"))
    }

    /// Click the centre of a painted menu row.
    fn click_item(cx: &mut VisualTestContext, selector: &'static str) {
        let at = item_bounds(cx, selector).center();
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
    }

    fn selected(view: &Entity<DirView>, cx: &mut VisualTestContext) -> Vec<PathBuf> {
        view.read_with(cx, |view, _| view.selection().selected_paths())
    }

    fn menu_is_open(view: &Entity<DirView>, cx: &mut VisualTestContext) -> bool {
        view.read_with(cx, |view, _| view.context_menu().is_some())
    }

    #[gpui::test]
    fn right_click_on_an_unselected_row_selects_it_and_opens_the_row_menu(cx: &mut TestAppContext) {
        let (_vfs, _pane, view, cx) = open_root(cx);

        // a.txt selected, then right-click c.txt.
        let first = row_point(&view, cx, 0);
        cx.simulate_click(first, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(selected(&view, cx), vec![PathBuf::from("/root/a.txt")]);

        let at = row_point(&view, cx, 2);
        right_click(cx, at);
        assert_eq!(
            selected(&view, cx),
            vec![PathBuf::from("/root/c.txt")],
            "a right-click outside the selection selects that row first"
        );
        view.read_with(cx, |view, _| {
            let menu = view.context_menu().expect("the row menu is open");
            assert_eq!(menu.position(), at, "§8: anchored at the invocation point");
            assert_eq!(menu.items()[0].label(), "Open", "this is the row menu");
            assert!(
                menu.items().iter().all(|item| item.label() != "Refresh"),
                "the row menu is not the background menu"
            );
        });
        // ...and it really painted, where the click landed.
        let open = item_bounds(cx, "context-menu-item-Open");
        assert!(
            open.origin.y >= at.y,
            "the panel drops from the click point"
        );
        item_bounds(cx, "context-menu-item-Delete Permanently");
    }

    #[gpui::test]
    fn right_click_inside_a_multi_selection_preserves_it(cx: &mut TestAppContext) {
        let (_vfs, _pane, view, cx) = open_root(cx);

        // NB `Modifiers::command()`, not `secondary_key()`: production reads
        // `modifiers.platform`, which `secondary_key()` maps to control off
        // macOS — a silent no-op on a Windows dev box.
        for (ix, modifiers) in [
            (0usize, Modifiers::none()),
            (1, Modifiers::command()),
            (2, Modifiers::command()),
        ] {
            let at = row_point(&view, cx, ix);
            cx.simulate_click(at, modifiers);
        }
        cx.run_until_parked();
        let before = selected(&view, cx);
        assert_eq!(before.len(), 3);

        let at = row_point(&view, cx, 1);
        right_click(cx, at);
        assert_eq!(
            selected(&view, cx),
            before,
            "a right-click inside the selection keeps all of it"
        );
        view.read_with(cx, |view, _| {
            assert_eq!(
                view.cursor().map(|cursor| cursor.0.to_path_buf()),
                Some(PathBuf::from("/root/b.txt")),
                "the cursor still moves onto the clicked row, so Open/Rename target it"
            );
            let items = view.context_menu().expect("open").items();
            assert!(
                !enabled(items, "Rename"),
                "Rename is disabled for a multi-selection"
            );
            assert!(enabled(items, "Delete"));
            assert!(
                enabled(items, "Open"),
                "Open is live for a multi-selection — `OpenSelected` opens all \
                 of it (see dir_view), so the row is honest"
            );
        });
    }

    #[gpui::test]
    fn right_click_below_the_rows_opens_the_background_menu(cx: &mut TestAppContext) {
        let (_vfs, _pane, view, cx) = open_root(cx);

        let first = row_point(&view, cx, 0);
        cx.simulate_click(first, Modifiers::none());
        cx.run_until_parked();
        let before = selected(&view, cx);

        let at = background_point(&view, cx);
        right_click(cx, at);
        view.read_with(cx, |view, _| {
            let menu = view.context_menu().expect("the background menu is open");
            for label in ["Paste", "New", "Refresh", "Sort by", "Show Hidden Files"] {
                find(menu.items(), label);
            }
            assert!(menu.items().iter().all(|item| item.label() != "Rename"));
        });
        assert_eq!(
            selected(&view, cx),
            before,
            "background commands act on the folder, so the selection is left alone"
        );
    }

    // The point of the module: an item's effect *is* the action's, and the
    // action's logic lives in exactly one place (§0).
    #[gpui::test]
    fn a_menu_item_dispatches_its_action(cx: &mut TestAppContext) {
        let (vfs, _pane, view, cx) = open_root(cx);

        // Copy, from the row menu, really fills the clipboard...
        let at = row_point(&view, cx, 0);
        right_click(cx, at);
        click_item(cx, "context-menu-item-Copy");
        cx.update(|_, cx| {
            let clipboard = &FsContext::global(cx).clipboard;
            assert_eq!(
                clipboard.entries,
                vec![EntryId(Arc::from(Path::new("/root/a.txt")))]
            );
            assert_eq!(clipboard.mode, ClipboardMode::Copy);
        });
        assert!(!menu_is_open(&view, cx), "activating a row dismisses");

        // ...and Paste, from the background menu, turns it into the same job
        // cmd-v would submit — keep-both name resolved by op planning.
        let at = background_point(&view, cx);
        right_click(cx, at);
        view.read_with(cx, |view, _| {
            let items = view.context_menu().unwrap().items();
            assert!(enabled(items, "Paste"), "Paste is live now");
        });
        click_item(cx, "context-menu-item-Paste");
        cx.run_until_parked();
        assert!(
            tree_has(&vfs, "/root/a copy.txt"),
            "the Paste row submitted a real Copy op"
        );
    }

    #[gpui::test]
    fn a_menu_item_reaches_handlers_on_other_entities(cx: &mut TestAppContext) {
        let (_vfs, pane, view, cx) = open_root(cx);

        // `SortBy` is handled by the *pane*: a boxed action from a menu row
        // has to bubble out of the DirView node to reach it, exactly as a
        // keystroke does.
        let at = background_point(&view, cx);
        right_click(cx, at);
        click_item(cx, "context-menu-item-Sort by");
        assert!(
            menu_is_open(&view, cx),
            "clicking a submenu row opens it instead of dismissing"
        );
        click_item(cx, "context-menu-item-Size");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.sort().key, SortKey::Size);
            assert_eq!(pane.sort().direction, SortDirection::Ascending);
        });
        assert!(!menu_is_open(&view, cx));

        // `ToggleHiddenFiles` is handled by the *workspace*, two nodes further
        // up again; with no workspace above this pane the action reaches no
        // handler, and the menu must still dismiss cleanly rather than hang.
        let at = background_point(&view, cx);
        right_click(cx, at);
        click_item(cx, "context-menu-item-Show Hidden Files");
        assert!(!menu_is_open(&view, cx));
    }

    // Re-picking the column that is already sorted must do nothing: `SortBy`
    // flips the direction for an unchanged key (right for a header click), and
    // in a menu whose only feedback is a stationary ✓ that reads as a bug.
    #[gpui::test]
    fn re_picking_the_checked_sort_column_does_not_reverse_the_listing(cx: &mut TestAppContext) {
        let (_vfs, pane, view, cx) = open_root(cx);

        let at = background_point(&view, cx);
        right_click(cx, at);
        click_item(cx, "context-menu-item-Sort by");
        click_item(cx, "context-menu-item-Name"); // already the active column
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.sort().key, SortKey::Name);
            assert_eq!(
                pane.sort().direction,
                SortDirection::Ascending,
                "the checked row is inert, so the direction is untouched"
            );
        });
        assert!(
            menu_is_open(&view, cx),
            "a disabled row does not even dismiss the menu"
        );

        // Picking a different column still works, and *then* Name is live again.
        click_item(cx, "context-menu-item-Size");
        cx.run_until_parked();
        let at = background_point(&view, cx);
        right_click(cx, at);
        click_item(cx, "context-menu-item-Sort by");
        click_item(cx, "context-menu-item-Name");
        cx.run_until_parked();
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.sort().key, SortKey::Name);
            assert_eq!(pane.sort().direction, SortDirection::Ascending);
        });
    }

    #[gpui::test]
    fn a_disabled_item_does_nothing_and_leaves_the_menu_up(cx: &mut TestAppContext) {
        let (_vfs, _pane, view, cx) = open_root(cx);

        let at = background_point(&view, cx);
        right_click(cx, at);
        view.read_with(cx, |view, _| {
            let items = view.context_menu().unwrap().items();
            assert!(!enabled(items, "Paste"), "nothing has been copied yet");
        });
        click_item(cx, "context-menu-item-Paste");
        assert!(
            menu_is_open(&view, cx),
            "a disabled row is inert — it does not even dismiss the menu"
        );
        // An enabled row in the same menu still works.
        click_item(cx, "context-menu-item-Refresh");
        assert!(!menu_is_open(&view, cx));
    }

    #[gpui::test]
    fn clicking_away_dismisses_the_menu(cx: &mut TestAppContext) {
        let (_vfs, _pane, view, cx) = open_root(cx);

        // Open the menu well to the right, so the panel does not cover the
        // rows on the left that the click-away lands on.
        let at = row_point_at(&view, cx, 0, 400.0);
        right_click(cx, at);
        assert!(menu_is_open(&view, cx));
        let panel = item_bounds(cx, "context-menu-item-Open");

        let away = row_point_at(&view, cx, 3, 20.0);
        assert!(
            away.x < panel.origin.x,
            "the click-away point must really be outside the panel"
        );
        cx.simulate_click(away, Modifiers::none());
        cx.run_until_parked();
        assert!(!menu_is_open(&view, cx));
    }

    #[gpui::test]
    fn escape_dismisses_the_menu(cx: &mut TestAppContext) {
        let (_vfs, _pane, view, cx) = open_root(cx);

        let at = row_point(&view, cx, 0);
        right_click(cx, at);
        assert!(menu_is_open(&view, cx));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            !menu_is_open(&view, cx),
            "the `menu` key-context token binds escape to Cancel"
        );
    }

    #[gpui::test]
    fn a_right_click_never_opens_a_menu_over_the_inline_editor(cx: &mut TestAppContext) {
        let (_vfs, _pane, view, cx) = open_root(cx);

        let at = row_point(&view, cx, 0);
        cx.simulate_click(at, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.rename.is_some()));

        let at = row_point(&view, cx, 2);
        right_click(cx, at);
        view.read_with(cx, |view, _| {
            assert!(view.context_menu().is_none(), "the editor keeps the view");
            assert!(view.rename.is_some(), "and is not torn down");
        });
    }

    // The §4c gap this step closes: creation opens the editor on the new row
    // instead of silently auto-naming it.
    #[gpui::test]
    fn new_folder_from_the_menu_opens_the_rename_editor_on_the_new_row(cx: &mut TestAppContext) {
        let (vfs, _pane, view, cx) = open_root(cx);

        let at = background_point(&view, cx);
        right_click(cx, at);
        click_item(cx, "context-menu-item-New");
        click_item(cx, "context-menu-item-Folder");
        cx.run_until_parked();

        // §4c: a phantom row carrying the editor — nothing on disk yet.
        view.read_with(cx, |view, cx| {
            let rename = view.rename.as_ref().expect("the editor is open");
            let row = rename.new_entry_row().expect("on a new-entry row");
            assert_eq!(row.path.as_ref(), Path::new("/root/New Folder"));
            assert_eq!(rename.input().read(cx).content(), "New Folder");
            assert!(
                view.flat_rows()
                    .iter()
                    .any(|row| row.entry.path.as_ref() == Path::new("/root/New Folder")),
                "the phantom row is projected, so the editor has somewhere to render"
            );
        });
        assert!(
            !tree_has(&vfs, "/root/New Folder"),
            "nothing is created until the name is committed"
        );

        // Typing a name and committing runs CreateDir with what was typed.
        cx.simulate_input("Reports");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(tree_has(&vfs, "/root/Reports"));
        assert!(!tree_has(&vfs, "/root/New Folder"));
        view.read_with(cx, |view, _| {
            assert!(view.rename.is_none(), "the editor closes on completion");
            assert!(
                view.new_entry_row().is_none(),
                "and the phantom row goes with it"
            );
        });
    }

    // `New ▸ Text file…` has no key binding at all (plan §3), so this menu is
    // its only entry point — and the placeholder keeps its extension while
    // only the stem is preselected.
    #[gpui::test]
    fn new_text_file_is_reachable_only_from_the_menu(cx: &mut TestAppContext) {
        let (vfs, _pane, view, cx) = open_root(cx);

        let at = background_point(&view, cx);
        right_click(cx, at);
        click_item(cx, "context-menu-item-New");
        click_item(cx, "context-menu-item-Text File…");
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let rename = view.rename.as_ref().expect("the editor is open");
            assert_eq!(rename.input().read(cx).content(), "New Text File.txt");
        });

        // Typing replaces the preselected stem only.
        cx.simulate_input("notes");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(tree_has(&vfs, "/root/notes.txt"));
    }

    // Escape while naming a new entry must leave the directory untouched —
    // the whole reason `Confirm` owns the op.
    #[gpui::test]
    fn escaping_a_new_entry_creates_nothing(cx: &mut TestAppContext) {
        let (vfs, _pane, view, cx) = open_root(cx);
        let before = view.read_with(cx, |view, _| view.flat_rows().len());

        cx.simulate_keystrokes("cmd-shift-n");
        cx.run_until_parked();
        view.read_with(cx, |view, _| assert!(view.new_entry_row().is_some()));
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert!(view.rename.is_none());
            assert!(view.new_entry_row().is_none());
            assert_eq!(
                view.flat_rows().len(),
                before,
                "the phantom row leaves no trace in the projection"
            );
        });
        assert!(!tree_has(&vfs, "/root/New Folder"));
    }

    // A name that is already taken comes back from the op as an inline error
    // in the still-open editor — the same path a colliding rename takes.
    #[gpui::test]
    fn a_new_entry_name_that_already_exists_reports_inline(cx: &mut TestAppContext) {
        let (vfs, _pane, view, cx) = open_root(cx);

        cx.simulate_keystrokes("cmd-shift-n");
        cx.run_until_parked();
        cx.simulate_input("a.txt");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            let rename = view.rename.as_ref().expect("the editor stays open");
            assert!(
                rename.error().is_some(),
                "the collision is reported inline, not swallowed"
            );
        });
        // And the file that was there is untouched.
        assert!(tree_has(&vfs, "/root/a.txt"));
    }

    // The placeholder is deconflicted against the listing, so a second New
    // Folder while the first still exists cannot collide with it.
    #[gpui::test]
    fn the_placeholder_name_avoids_what_is_already_there(cx: &mut TestAppContext) {
        let (vfs, pane, view, cx) = open_root(cx);
        vfs.insert_dir("/root/New Folder");
        pane.update(cx, |pane, cx| pane.refresh(cx));
        cx.run_until_parked();

        cx.simulate_keystrokes("cmd-shift-n");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let row = view
                .new_entry_row()
                .expect("the editor is on a phantom row");
            assert_eq!(row.path.as_ref(), Path::new("/root/New Folder 2"));
        });
    }
}
