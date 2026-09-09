//! The settings pane (M7c, plan §7's "Settings window").
//!
//! **A pane, not a window and not a modal.** It takes over the browsing
//! region — everything between the sidebar and the window edge — while the
//! sidebar and titlebar stay put, so the app is still recognisably itself
//! underneath. A modal would have blocked the window, which is the one thing
//! you must not do to a settings surface whose whole job is changing how the
//! window looks: picking a theme is only meaningful if you can see it land.
//!
//! Everything here writes through [`AppSettings`] and saves immediately —
//! there is no OK/Cancel, which matches how the rest of the app treats
//! settings (a pinned favorite persists the moment it is pinned). The store
//! serializes those writes, so clicking three checkboxes quickly is three
//! saves that cannot race (M7b).
//!
//! The pane is also the home for the two diagnostic channels M7b collects and
//! could not show: what was wrong with `settings.json` and with `keymap.json`.
//! Until this existed, a typo in either file was correct-but-silent.

use gpui::{
    App, BorrowAppContext as _, Context, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, Window, div, prelude::*, px,
};

use crate::settings::AppSettings;
use crate::theme::{ActiveTheme, Theme, ThemeSelection};

/// Which section is showing. Three, matching the three things a user came
/// here to do: change a behavior, change how it looks, find out what a key
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Appearance,
    Keyboard,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 3] = [Self::General, Self::Appearance, Self::Keyboard];

    pub fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Keyboard => "Keyboard",
        }
    }
}

/// The settings pane.
pub struct SettingsView {
    section: SettingsSection,
    focus_handle: FocusHandle,
    /// Repaint when anything changes the settings — this pane's own writes,
    /// or a text editor (M7b's file watcher). The pane must never show a
    /// value the store disagrees with.
    _settings_observer: Subscription,
    /// Repaint when the *theme system* changes, which is not the same thing
    /// as the painted theme changing: a user theme dropped into the folder
    /// joins the registry without altering what is on screen, so
    /// `ActiveTheme` does not refresh the windows and the picker would go on
    /// listing the themes that existed when it opened. Same for a theme file
    /// whose diagnostics changed.
    _theme_observer: Subscription,
}

impl SettingsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            section: SettingsSection::General,
            focus_handle: cx.focus_handle(),
            _settings_observer: cx.observe_global::<AppSettings>(|_, cx| cx.notify()),
            _theme_observer: cx.observe_global::<ActiveTheme>(|_, cx| cx.notify()),
        }
    }

    pub fn section(&self) -> SettingsSection {
        self.section
    }

    pub fn show_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        if self.section == section {
            return;
        }
        self.section = section;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Writes. Each is "mutate the global, then persist" — the same two steps
    // the sidebar's favorites have used since M2.
    // ------------------------------------------------------------------

    fn set_confirm_delete(&mut self, confirm: bool, cx: &mut Context<Self>) {
        cx.update_global::<AppSettings, _>(|settings, cx| {
            if settings.set_confirm_delete_to_trash(confirm) {
                settings.save(cx);
            }
        });
        cx.notify();
    }

    fn set_folders_first(&mut self, folders_first: bool, cx: &mut Context<Self>) {
        cx.update_global::<AppSettings, _>(|settings, cx| {
            if settings.set_folders_first(folders_first) {
                settings.save(cx);
            }
        });
        cx.notify();
    }

    /// Pick a theme, or the follow-the-system pair. Applies to the running
    /// app **and** persists, in that order, so the change is visible before
    /// the disk write completes.
    fn choose_theme(&mut self, selection: ThemeSelection, cx: &mut Context<Self>) {
        ActiveTheme::set_selection(selection.clone(), cx);
        cx.update_global::<AppSettings, _>(|settings, cx| {
            if settings.set_theme_selection(selection) {
                settings.save(cx);
            }
        });
        cx.notify();
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx).clone();
        div()
            .track_focus(&self.focus_handle)
            .key_context("Settings")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .bg(theme.surface)
            .text_color(theme.text)
            .child(self.render_tabs(&theme, cx))
            .child(
                div()
                    .id("settings-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(match self.section {
                        SettingsSection::General => self.render_general(&theme, cx),
                        SettingsSection::Appearance => self.render_appearance(&theme, cx),
                        SettingsSection::Keyboard => self.render_keyboard(&theme, cx),
                    }),
            )
    }
}

impl SettingsView {
    /// The section switcher, in the same chrome row position a pane's
    /// breadcrumb occupies — so the region's top edge stays put when the
    /// settings pane replaces a browsing pane.
    fn render_tabs(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let mut row = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .h(px(34.0))
            .px(px(12.0))
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(12.0));
        for section in SettingsSection::ALL {
            let active = section == self.section;
            let theme = theme.clone();
            row = row.child(
                div()
                    .id(section.title())
                    .debug_selector(move || format!("settings-tab-{}", section.title()))
                    .px(px(10.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .when(active, |el| el.bg(theme.accent.opacity(0.30)))
                    .text_color(if active { theme.text } else { theme.muted })
                    .hover(|s| s.bg(theme.accent.opacity(0.15)))
                    .on_click(cx.listener(move |this, _, _, cx| this.show_section(section, cx)))
                    .child(SharedString::new_static(section.title())),
            );
        }
        row
    }

    /// Plan §3's behaviors that are settings. Each row says what it does in
    /// the app's own terms — "Delete moves items to the Trash" is a fact
    /// about this file manager, not a generic preference label.
    fn render_general(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let settings = AppSettings::global(cx);
        let confirm = settings.confirm_delete_to_trash();
        let folders_first = settings.folders_first();
        let warnings = settings.warnings().to_vec();

        section(
            theme,
            "Behavior",
            vec![
                toggle_row(
                    "settings-confirm-delete",
                    "Ask before moving items to the Trash",
                    "The Trash is undoable, so this is off by default — Explorer does not ask.",
                    confirm,
                    theme,
                    cx.listener(move |this, _, _, cx| this.set_confirm_delete(!confirm, cx)),
                ),
                toggle_row(
                    "settings-folders-first",
                    "Group folders before files",
                    "Off sorts folders and files together, the way Finder does.",
                    folders_first,
                    theme,
                    cx.listener(move |this, _, _, cx| this.set_folders_first(!folders_first, cx)),
                ),
            ],
        )
        .child(diagnostics_block(
            theme,
            "settings.json",
            &warnings,
            "Every setting was understood.",
        ))
        .into_any_element()
    }

    /// The theme picker: every theme in the registry, plus the
    /// follow-the-system pair, plus whatever was wrong with the themes folder.
    fn render_appearance(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let registry = ActiveTheme::registry(cx);
        let selection = ActiveTheme::selection(cx);
        let active_name = ActiveTheme::requested_name(cx);
        let diagnostics = ActiveTheme::diagnostics(cx);

        let mut rows = vec![choice_row(
            "settings-theme-system",
            "Follow the system",
            "Light and dark themes, switched by macOS.",
            selection.is_dynamic(),
            theme,
            cx.listener(|this, _, _, cx| this.choose_theme(ThemeSelection::system(), cx)),
        )];
        for installed in registry.themes() {
            let name = installed.name.clone();
            let chosen = !selection.is_dynamic() && name == active_name;
            let appearance = installed.appearance;
            let pick = name.clone();
            rows.push(choice_row_owned(
                format!("settings-theme-{name}"),
                name.to_string(),
                format!("{appearance} theme"),
                chosen,
                theme,
                cx.listener(move |this, _, _, cx| {
                    this.choose_theme(ThemeSelection::Static(pick.to_string()), cx)
                }),
            ));
        }

        let messages: Vec<String> = diagnostics
            .iter()
            .map(|diagnostic| {
                let file = diagnostic
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| diagnostic.path.display().to_string());
                format!("{file}: {}", diagnostic.message)
            })
            .collect();

        section(theme, "Theme", rows)
            .child(hint(
                theme,
                "Drop a .json theme into the themes folder beside settings.json — it appears here, and edits apply while the app is running.",
            ))
            .child(diagnostics_block(
                theme,
                "the themes folder",
                &messages,
                "Every theme file loaded cleanly.",
            ))
            .into_any_element()
    }

    /// Read-only, deliberately: rebinding by chord capture needs conflict
    /// detection and a write path for a file the user may also be hand-editing
    /// (recorded as a gap). What it does do is make the keymap *visible* —
    /// including the overrides that `keymap.json` applied and the rows it
    /// could not.
    fn render_keyboard(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let diagnostics = crate::keymap::UserKeymap::diagnostics(cx);
        let bindings = crate::keymap::visible_bindings(cx);
        let rows = bindings
            .into_iter()
            .map(|(keystrokes, action, context)| binding_row(theme, keystrokes, action, context))
            .collect();

        section(theme, "Keys", rows)
            .child(hint(
                theme,
                "Edit keymap.json beside settings.json to override any of these; changes apply without a restart.",
            ))
            .child(diagnostics_block(
                theme,
                "keymap.json",
                &diagnostics,
                "No overrides, or every override was understood.",
            ))
            .into_any_element()
    }
}

// ----------------------------------------------------------------------
// Row/section widgets. Local to this module: the info panel's look is a
// two-column "label — value" list for *facts*, and these are controls.
// ----------------------------------------------------------------------

fn section(theme: &Theme, title: &'static str, rows: Vec<gpui::AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .px(px(16.0))
                .pt(px(14.0))
                .pb(px(6.0))
                .text_size(px(11.0))
                .text_color(theme.muted)
                .child(SharedString::new_static(title)),
        )
        .children(rows)
}

/// A clickable row carrying a checkbox: label, one line of why, and the box.
fn toggle_row(
    id: &'static str,
    label: &'static str,
    detail: &'static str,
    checked: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    row_shell(id.to_string(), theme, on_click)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .child(SharedString::new_static(label)),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.muted)
                        .child(SharedString::new_static(detail)),
                ),
        )
        .child(check_box(theme, checked))
        .into_any_element()
}

fn choice_row(
    id: &'static str,
    label: &'static str,
    detail: &'static str,
    chosen: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    choice_row_owned(
        id.to_string(),
        label.to_string(),
        detail.to_string(),
        chosen,
        theme,
        on_click,
    )
}

/// The same row for a name only known at runtime (an installed theme).
fn choice_row_owned(
    id: String,
    label: String,
    detail: String,
    chosen: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    row_shell(id, theme, on_click)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .child(div().text_size(px(13.0)).child(SharedString::from(label)))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(detail)),
                ),
        )
        // A dot, not a checkbox: these are one-of-many, and the shape should
        // say so before the label is read.
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(13.0))
                .h(px(13.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.border)
                .when(chosen, |el| el.bg(theme.accent)),
        )
        .into_any_element()
}

fn row_shell(
    id: String,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(16.0))
        .py(px(7.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.accent.opacity(0.10)))
        .on_click(on_click)
}

fn check_box(theme: &Theme, checked: bool) -> gpui::Div {
    let mut box_ = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(13.0))
        .h(px(13.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(theme.border);
    if checked {
        box_ = box_
            .bg(theme.accent)
            .text_size(px(9.0))
            .text_color(theme.text)
            .child(SharedString::new_static("✓"));
    }
    box_
}

/// One key binding, as the keymap actually resolved it.
fn binding_row(
    theme: &Theme,
    keystrokes: String,
    action: String,
    context: Option<String>,
) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(16.0))
        .py(px(3.0))
        .text_size(px(12.0))
        .child(
            div()
                .flex_none()
                .w(px(130.0))
                .text_color(theme.accent)
                .child(SharedString::from(keystrokes)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(SharedString::from(action)),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(theme.muted)
                .child(SharedString::from(context.unwrap_or_default())),
        )
        .into_any_element()
}

fn hint(theme: &Theme, text: &'static str) -> gpui::Div {
    div()
        .px(px(16.0))
        .pt(px(10.0))
        .text_size(px(11.0))
        .text_color(theme.muted)
        .child(SharedString::new_static(text))
}

/// What was wrong with a config file — the M7b diagnostics, finally on
/// screen. Says so explicitly when there is nothing wrong, because "no
/// errors shown" and "errors not shown" look identical otherwise.
fn diagnostics_block(
    theme: &Theme,
    file: &'static str,
    messages: &[String],
    all_clear: &'static str,
) -> gpui::Div {
    let mut block = div()
        .flex()
        .flex_col()
        .px(px(16.0))
        .pt(px(10.0))
        .pb(px(14.0))
        .text_size(px(11.0));
    if messages.is_empty() {
        block = block.child(
            div()
                .text_color(theme.muted)
                .child(SharedString::from(all_clear.to_string())),
        );
    } else {
        block = block.child(
            div()
                .text_color(theme.error)
                .child(SharedString::from(format!("Problems in {file}:"))),
        );
        for message in messages {
            block = block.child(
                div()
                    .text_color(theme.error)
                    .child(SharedString::from(format!("• {message}"))),
            );
        }
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workspace;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::theme::ActiveTheme;
    use fs_core::{FakeVfs, Spawner, StubPlatform, Vfs as _};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const SETTINGS_PATH: &str = "/config/settings.json";

    fn boot(cx: &mut TestAppContext) -> (Arc<FakeVfs>, Entity<Workspace>, &mut VisualTestContext) {
        let vfs = cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree("/root", json!({ "a.txt": "a", "z-folder": {} }));
            vfs.insert_dir("/config");
            // The themes folder has to exist as a directory for the folder
            // read to find anything in it — dropping a file into a directory
            // that is not there is not a scenario a user can produce.
            vfs.insert_dir("/config/themes");
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(StubPlatform::new()),
            );
            ActiveTheme::init_in(
                PathBuf::from("/config/themes"),
                ThemeSelection::default(),
                cx,
            );
            crate::settings::init_with_path(cx, PathBuf::from(SETTINGS_PATH));
            vfs
        });
        cx.run_until_parked();
        let (workspace, cx) = cx.add_window_view(Workspace::new);
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        (vfs, workspace, cx)
    }

    fn open_settings(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let handle = workspace.focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.simulate_keystrokes("cmd-,");
        cx.run_until_parked();
    }

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let at = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("`{selector}` is not painted"))
            .center();
        cx.simulate_click(at, gpui::Modifiers::none());
        cx.run_until_parked();
    }

    fn settings_view(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<SettingsView> {
        workspace.read_with(cx, |workspace, _| {
            workspace
                .settings_view()
                .expect("the settings pane is open")
        })
    }

    #[gpui::test]
    fn cmd_comma_opens_the_pane_and_escape_closes_it(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = boot(cx);
        workspace.read_with(cx, |workspace, _| assert!(!workspace.settings_open()));

        open_settings(&workspace, cx);
        workspace.read_with(cx, |workspace, _| assert!(workspace.settings_open()));
        // The browsing region really is replaced, not overlaid.
        assert!(
            cx.debug_bounds("dir-view-list-surface").is_none(),
            "the file list should be gone while settings are open"
        );
        assert!(cx.debug_bounds("settings-tab-General").is_some());

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| assert!(!workspace.settings_open()));
        assert!(
            cx.debug_bounds("dir-view-list-surface").is_some(),
            "closing must give the file list back"
        );
    }

    #[gpui::test]
    fn the_tabs_switch_sections(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);
        let view = settings_view(&workspace, cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.section(), SettingsSection::General)
        });

        click("settings-tab-Appearance", cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.section(), SettingsSection::Appearance)
        });
        click("settings-tab-Keyboard", cx);
        view.read_with(cx, |view, _| {
            assert_eq!(view.section(), SettingsSection::Keyboard)
        });
    }

    /// The whole point of the General section: the click changes the app's
    /// behavior *and* reaches the disk, with no OK button in between.
    #[gpui::test]
    fn toggling_a_behavior_applies_it_and_persists_it(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);

        cx.update(|_, cx| assert!(AppSettings::global(cx).folders_first()));
        click("settings-folders-first", cx);

        cx.update(|_, cx| {
            assert!(
                !AppSettings::global(cx).folders_first(),
                "the click did not reach the store"
            );
        });
        let written = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH)))
            .expect("settings were saved");
        let written: serde_json::Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(
            written.get("folders_first"),
            Some(&serde_json::json!(false)),
            "the click did not reach the disk: {written}"
        );

        // ...and clicking it back removes the override again, because the
        // file only carries what differs from the defaults (M7b).
        click("settings-folders-first", cx);
        let written = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH))).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(written.get("folders_first"), None, "{written}");
    }

    #[gpui::test]
    fn the_trash_confirmation_toggle_reaches_the_delete_key(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);
        click("settings-confirm-delete", cx);
        cx.update(|_, cx| assert!(AppSettings::global(cx).confirm_delete_to_trash()));
    }

    /// Picking a theme repaints immediately — including the pane the click
    /// happened in — and persists the choice.
    #[gpui::test]
    fn choosing_a_theme_applies_and_persists(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);
        click("settings-tab-Appearance", cx);
        cx.update(|_, cx| assert_eq!(crate::theme::theme(cx), &Theme::dark()));

        click("settings-theme-Graphite Light", cx);
        cx.update(|_, cx| {
            assert_eq!(crate::theme::theme(cx), &Theme::light(), "not repainted");
            assert_eq!(
                ActiveTheme::selection(cx),
                ThemeSelection::Static("Graphite Light".into())
            );
        });
        let written = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH))).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(
            written.get("theme"),
            Some(&serde_json::json!("Graphite Light")),
            "{written}"
        );
    }

    /// "Follow the system" writes the *pair*, which is the whole difference
    /// between it and picking the dark theme by hand.
    #[gpui::test]
    fn following_the_system_writes_a_pair(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);
        click("settings-tab-Appearance", cx);
        click("settings-theme-system", cx);

        cx.update(|_, cx| assert!(ActiveTheme::selection(cx).is_dynamic()));
        let written = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH))).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(
            written.get("theme"),
            Some(&serde_json::json!({ "light": "Graphite Light", "dark": "Graphite Dark" })),
            "{written}"
        );
    }

    /// A user theme dropped into the folder joins the picker without a
    /// restart — the M7a hot reload, now with somewhere to show itself.
    #[gpui::test]
    fn a_hot_reloaded_user_theme_appears_in_the_picker(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);
        click("settings-tab-Appearance", cx);
        assert!(cx.debug_bounds("settings-theme-Midnight").is_none());

        vfs.write_file(
            Path::new("/config/themes/midnight.json"),
            br#"{ "name": "Midnight", "appearance": "dark" }"#.to_vec(),
        );
        cx.executor().advance_clock(
            crate::theme::THEME_WATCH_LATENCY + std::time::Duration::from_millis(10),
        );
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("settings-theme-Midnight").is_some(),
            "the new theme did not reach the picker"
        );
        click("settings-theme-Midnight", cx);
        cx.update(|_, cx| assert_eq!(crate::theme::theme(cx).name, SharedString::from("Midnight")));
    }

    /// The point of `selection` being its own token: a theme can re-tint the
    /// selected row without moving the accent, which is what every focus
    /// ring, toolbar highlight and hover state is drawn from.
    #[gpui::test]
    fn a_user_theme_can_tint_selection_without_moving_the_accent(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = boot(cx);
        vfs.write_file(
            Path::new("/config/themes/tinted.json"),
            br#"{ "name": "Tinted", "appearance": "dark",
                  "colors": { "selection": "hsla(120, 100%, 50%, 0.4)" } }"#
                .to_vec(),
        );
        cx.executor().advance_clock(
            crate::theme::THEME_WATCH_LATENCY + std::time::Duration::from_millis(10),
        );
        cx.run_until_parked();

        open_settings(&workspace, cx);
        click("settings-tab-Appearance", cx);
        click("settings-theme-Tinted", cx);

        cx.update(|_, cx| {
            let theme = crate::theme::theme(cx);
            assert_eq!(
                theme.selection,
                crate::theme::color::parse("hsla(120, 100%, 50%, 0.4)").unwrap()
            );
            assert_eq!(
                theme.accent,
                Theme::dark().accent,
                "the accent must be untouched — that is the whole point"
            );
        });
    }

    /// The M7b diagnostics, finally visible. Before this pane existed a typo
    /// in a config file was correct-but-silent.
    #[gpui::test]
    fn a_broken_theme_file_is_reported_in_the_pane(cx: &mut TestAppContext) {
        let (vfs, workspace, cx) = boot(cx);
        vfs.write_file(
            Path::new("/config/themes/broken.json"),
            b"{ not json".to_vec(),
        );
        cx.executor().advance_clock(
            crate::theme::THEME_WATCH_LATENCY + std::time::Duration::from_millis(10),
        );
        cx.run_until_parked();

        open_settings(&workspace, cx);
        click("settings-tab-Appearance", cx);
        cx.update(|_, cx| {
            let diagnostics = ActiveTheme::diagnostics(cx);
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        });
    }

    /// The Keyboard list is read back out of gpui, so an override shows up as
    /// the override rather than as the default it replaced.
    #[gpui::test]
    fn the_keyboard_list_shows_what_the_keymap_actually_dispatches(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);

        let default_rows = cx.update(|_, cx| crate::keymap::visible_bindings(cx));
        assert!(
            default_rows
                .iter()
                .any(|(keys, action, _)| keys == "f2" && action.ends_with("RenameSelected")),
            "the §0 default is missing from the list"
        );

        cx.update(|_, cx| {
            crate::keymap::apply_for_test(
                cx,
                r#"[{ "context": "DirView", "bindings": { "f2": "Duplicate" } }]"#,
            )
        });
        let overridden = cx.update(|_, cx| crate::keymap::visible_bindings(cx));
        assert!(
            overridden
                .iter()
                .any(|(keys, action, _)| keys == "f2" && action.ends_with("Duplicate")),
            "the override is missing from the list"
        );
    }
}
