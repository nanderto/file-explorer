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

/// How wide the content column is allowed to get. The pane spans the whole
/// browsing region, which at 1200px is far wider than a settings form should
/// be — a label on the left and its checkbox against the right edge read as
/// unrelated.
const CONTENT_MAX_WIDTH: f32 = 560.0;

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
    /// The Keyboard section's filter field. ~70 bindings is a reference
    /// list, not something anyone reads top to bottom — the question people
    /// actually arrive with is "what does F2 do" or "how do I paste".
    key_filter: gpui::Entity<crate::input::InputState>,
    focus_handle: FocusHandle,
    /// Repaint when anything changes the settings — this pane's own writes,
    /// or a text editor (M7b's file watcher). The pane must never show a
    /// value the store disagrees with.
    _settings_observer: Subscription,
    /// Repaint as the Keyboard filter is typed into.
    _filter_observer: Subscription,
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
        let colors = crate::input::input_colors(cx);
        let key_filter = cx.new(|cx| {
            crate::input::InputState::new(cx)
                .input_type(crate::input::text_input::InputType::Search)
                .placeholder("Filter by key or command")
                .with_colors(colors.0, colors.1, colors.2)
        });
        let filter_observer = cx.subscribe(&key_filter, |_, _, event, cx| {
            if matches!(event, crate::input::InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            section: SettingsSection::General,
            key_filter,
            focus_handle: cx.focus_handle(),
            _settings_observer: cx.observe_global::<AppSettings>(|_, cx| cx.notify()),
            _filter_observer: filter_observer,
            _theme_observer: cx.observe_global::<ActiveTheme>(|_, cx| cx.notify()),
        }
    }

    /// Type into the Keyboard filter, for tests (the real path is the user
    /// typing into the vendored field).
    #[cfg(test)]
    pub(crate) fn set_key_filter_for_test(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.key_filter.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx)
        });
        cx.notify();
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
                    // Content is capped at a readable width rather than
                    // stretched across the whole region: a checkbox 950px
                    // from the label it belongs to is not a control, it is
                    // two unrelated things on one line. The cap is what a
                    // settings surface does everywhere; the pane is wide
                    // because it inherited the file list's width, not
                    // because this content wants to be.
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .child(match self.section {
                                SettingsSection::General => self.render_general(&theme, cx),
                                SettingsSection::Appearance => self.render_appearance(&theme, cx),
                                SettingsSection::Keyboard => self.render_keyboard(&theme, cx),
                            }),
                    ),
            )
    }
}

impl SettingsView {
    /// The Keyboard filter field. Wired through the shared
    /// [`crate::rename::with_editor_actions`] so it gets the same dispatch
    /// node every other text field in the app has — including `track_focus`
    /// of the *input's own* handle, without which every binding in its
    /// `TextInput` context is silently dead (§9's named failure mode).
    fn render_key_filter(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        crate::input::refresh_input_colors(&self.key_filter, cx);
        let field = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .w(px(220.0))
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(theme.border)
            .text_size(px(12.0))
            .child(
                div()
                    .text_color(theme.muted)
                    .child(SharedString::new_static("\u{2315}")),
            )
            .child(div().flex_1().min_w(px(0.0)).child(self.key_filter.clone()));
        crate::rename::with_editor_actions(
            field,
            &self.key_filter,
            cx,
            |_, _, _| {},
            |this, window, cx| {
                // Escape clears the filter rather than closing the pane: the
                // field is the innermost focused thing, so it has first claim
                // on the key.
                this.key_filter
                    .update(cx, |input, cx| input.set_value(String::new(), window, cx));
                cx.notify();
            },
        )
        .into_any_element()
    }

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
        let query = self.key_filter.read(cx).content().to_string();
        let all = crate::keymap::visible_bindings(cx);
        let total = all.len();
        let matched: Vec<_> = all
            .into_iter()
            .filter(|row| binding_matches(row, &query))
            .collect();
        let shown = matched.len();
        let rows = matched
            .into_iter()
            .map(|row| binding_row(theme, row.keystrokes, row.action, row.context))
            .collect();

        let field = self.render_key_filter(theme, cx);
        section_with_control(theme, "Keys", field, rows)
            .child(filter_summary(theme, &query, shown, total))
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

/// Does this binding match the filter? Matched against the chord, the action
/// and the context together, because all three are things a person would
/// type: "cmd", "paste", "DirView". Terms are AND-ed, so "cmd paste" narrows.
fn binding_matches(row: &crate::keymap::BindingRow, query: &str) -> bool {
    // Normalized here rather than by the caller: a matcher that silently
    // requires a pre-lowercased query is a trap for the next call site.
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {}",
        row.keystrokes,
        row.action,
        row.context.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    query.split_whitespace().all(|term| haystack.contains(term))
}

/// "12 of 71" while filtering — so a query matching nothing reads as a query
/// that matched nothing, rather than as an empty keymap.
fn filter_summary(theme: &Theme, query: &str, shown: usize, total: usize) -> gpui::Div {
    let text = if query.is_empty() {
        format!("{total} bindings")
    } else if shown == 0 {
        format!("No binding matches \u{201c}{query}\u{201d} — {total} in total")
    } else {
        format!("{shown} of {total} bindings")
    };
    div()
        .px(px(16.0))
        .pt(px(8.0))
        .text_size(px(11.0))
        .text_color(theme.muted)
        .child(SharedString::from(text))
}

/// A section whose header carries a control on the right (the Keyboard
/// filter). Same header as [`section`], so the two read as one family.
fn section_with_control(
    theme: &Theme,
    title: &'static str,
    control: gpui::AnyElement,
    rows: Vec<gpui::AnyElement>,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .px(px(16.0))
                .pt(px(14.0))
                .pb(px(6.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme.muted)
                        .child(SharedString::new_static(title)),
                )
                .child(control),
        )
        .children(rows)
}

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

    /// The realistic path: focus is in the **file list**, not on the
    /// workspace root, which is where it actually sits when a user reaches
    /// for `cmd-,`. The binding lives in the `Workspace` context, so this
    /// only works if the list's node really is a descendant of the node
    /// carrying that context — the failure gpui reports by silently doing
    /// nothing (§9's named hazard).
    #[gpui::test]
    fn cmd_comma_works_with_focus_in_the_file_list(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = boot(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        cx.update(|window, cx| {
            window.activate_window();
            let handle = pane.read(cx).dir_view().focus_handle(cx);
            window.focus(&handle, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("cmd-,");
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.settings_open(),
                "cmd-, did nothing with focus where it actually lives"
            );
        });
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

    /// The filter is the difference between a reference list and a wall of
    /// ~70 rows. Matched against chord, action *and* context, because all
    /// three are things a person would type.
    #[gpui::test]
    fn the_keyboard_filter_narrows_the_list(cx: &mut TestAppContext) {
        let (_vfs, workspace, cx) = boot(cx);
        open_settings(&workspace, cx);
        click("settings-tab-Keyboard", cx);
        let view = settings_view(&workspace, cx);

        let all = cx.update(|_, cx| crate::keymap::visible_bindings(cx).len());
        assert!(all > 20, "the fixture keymap should be substantial");

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_key_filter_for_test("rename", window, cx)
            })
        });
        cx.run_until_parked();

        let shown = cx.update(|_, cx| {
            crate::keymap::visible_bindings(cx)
                .into_iter()
                .filter(|row| binding_matches(row, "rename"))
                .collect::<Vec<_>>()
        });
        assert!(!shown.is_empty(), "`rename` should match something");
        assert!(shown.len() < all, "the filter should narrow the list");
        assert!(
            shown
                .iter()
                .all(|row| row.action.to_lowercase().contains("rename")
                    || row
                        .context
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains("rename")),
            "{shown:?}"
        );
    }

    /// Every term has to match, so a second word narrows rather than widens.
    #[test]
    fn filter_terms_are_and_ed() {
        let row = crate::keymap::BindingRow {
            keystrokes: "\u{2318}V".to_string(),
            action: "file_explorer::Paste".to_string(),
            context: Some("DirView".to_string()),
            uses_platform_modifier: true,
        };
        assert!(binding_matches(&row, ""), "an empty filter matches all");
        assert!(binding_matches(&row, "paste"));
        assert!(binding_matches(&row, "paste dirview"), "both terms match");
        assert!(!binding_matches(&row, "paste sidebar"), "one term does not");
        assert!(binding_matches(&row, "PASTE"), "case-insensitive");
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
                .any(|row| row.keystrokes == "f2" && row.action.ends_with("RenameSelected")),
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
                .any(|row| row.keystrokes == "f2" && row.action.ends_with("Duplicate")),
            "the override is missing from the list"
        );
    }
}
