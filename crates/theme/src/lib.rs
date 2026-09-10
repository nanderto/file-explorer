//! The theme model and its JSON loader (plan §6).
//!
//! A theme is a JSON document:
//!
//! ```json
//! {
//!   "name": "Graphite Dark",
//!   "appearance": "dark",
//!   "colors": { "surface": "hsl(240, 4%, 12%)", "accent": "#3b82f6" },
//!   "file_colors": { "folder": "hsl(210, 90%, 60%)" }
//! }
//! ```
//!
//! Both built-ins are exactly that — [`Theme::dark`] and [`Theme::light`]
//! parse `themes/*.json` embedded at compile time, so the shipped themes are
//! written in the same language a user theme is and cannot drift from it.
//!
//! A **user** theme (`~/Library/Application Support/file-explorer/themes/*.json`)
//! may set as few keys as it likes: everything absent comes from the built-in
//! of the same `appearance`, so a two-line file that only re-tints the accent
//! is a legal theme. A malformed one is an error the caller reports and
//! ignores — a bad file in the themes folder must never take the app down or
//! leave it unpainted.
//!
//! This crate is deliberately I/O-free: it turns text into a [`Theme`] and
//! back. Reading the themes folder and watching it for changes belongs to the
//! app, which owns the `Vfs` and the background executor.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context as _, bail};
use gpui::{Hsla, SharedString};
use serde::{Deserialize, Serialize};

pub mod color;

/// Which built-in appearance a theme renders — the light/dark half of the
/// system appearance, and the base a partial user theme inherits from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Light,
    Dark,
}

impl fmt::Display for Appearance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        })
    }
}

/// Declares a color group twice over: the complete struct the app paints from,
/// and the all-optional struct a user theme deserializes into. One field list
/// so the two can never drift, which is the whole reason for the macro.
macro_rules! color_group {
    (
        $(#[$meta:meta])*
        $name:ident / $partial:ident {
            $($(#[$field_meta:meta])* $field:ident),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct $name {
            $($(#[$field_meta])* pub $field: Hsla,)*
        }

        /// The same group as it appears in a user theme: every key optional,
        /// every value still text at this stage.
        #[derive(Debug, Clone, Default, Deserialize)]
        #[serde(default)]
        struct $partial {
            $($field: Option<String>,)*
            /// Keys we do not know. Kept so a typo can be reported rather
            /// than silently ignored.
            #[serde(flatten)]
            unknown: BTreeMap<String, serde_json::Value>,
        }

        impl $partial {
            /// Apply the keys this file actually set on top of `base`.
            fn merge_onto(self, base: $name, group: &str, warnings: &mut Vec<String>)
                -> anyhow::Result<$name>
            {
                let mut merged = base;
                $(
                    if let Some(text) = self.$field {
                        merged.$field = color::parse(&text)
                            .with_context(|| format!("{}.{}", group, stringify!($field)))?;
                    }
                )*
                for key in self.unknown.keys() {
                    warnings.push(format!("unknown key `{group}.{key}` ignored"));
                }
                Ok(merged)
            }

            /// Read the group with **no** base to fall back on: every key must
            /// be present. This is how the embedded built-ins are parsed, and
            /// it is what stops the base-and-override scheme from being
            /// circular — a built-in inherits from nothing.
            fn into_complete(self, group: &str, warnings: &mut Vec<String>)
                -> anyhow::Result<$name>
            {
                $(
                    let $field = {
                        let text = self.$field.ok_or_else(|| anyhow::anyhow!(
                            "`{}.{}` is missing", group, stringify!($field)))?;
                        color::parse(&text)
                            .with_context(|| format!("{}.{}", group, stringify!($field)))?
                    };
                )*
                // `self` is partially moved by now; the field access is still
                // fine, a method call would not be.
                for key in self.unknown.keys() {
                    warnings.push(format!("unknown key `{group}.{key}` ignored"));
                }
                Ok($name { $($field,)* })
            }
        }
    };
}

color_group! {
    /// The chrome palette. Every widget in `crates/app` takes its colors from
    /// here — no hard-coded colors anywhere (plan §6).
    ThemeColors / ThemeColorsPartial {
        /// Main content pane background.
        surface,
        /// Sidebar background.
        sidebar,
        /// Info panel background.
        panel,
        /// Titlebar background.
        titlebar,
        /// Primary text.
        text,
        /// Secondary/muted text (section headers, status lines).
        muted,
        /// Accent for selection and highlights.
        accent,
        /// Background of a selected row. Distinct from `accent` so a theme can
        /// tint selection without moving every focus ring with it.
        selection,
        /// Hairline borders between regions.
        border,
        /// Errors and destructive emphasis (invalid path, failed operation).
        error,
    }
}

color_group! {
    /// Per-file-kind tints for icons and glyphs (plan §6 `file_colors`).
    /// Distinct from the Finder **tag** palette, which is fixed by macOS and
    /// is not themeable.
    FileColors / FileColorsPartial {
        folder,
        image,
        code,
        archive,
        audio,
        video,
        document,
        /// Anything the kind table does not classify.
        other,
    }
}

/// A complete, ready-to-paint theme.
///
/// [`Deref`](std::ops::Deref)s to its [`ThemeColors`], so a widget writes
/// `theme.surface` and never has to know which group a color lives in.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Display name, and the key the theme picker and settings file use.
    pub name: SharedString,
    pub appearance: Appearance,
    pub colors: ThemeColors,
    pub file_colors: FileColors,
}

impl std::ops::Deref for Theme {
    type Target = ThemeColors;

    fn deref(&self) -> &Self::Target {
        &self.colors
    }
}

/// The wire form: what a theme file literally contains.
#[derive(Debug, Deserialize)]
struct ThemeFile {
    name: String,
    appearance: Appearance,
    #[serde(default)]
    colors: ThemeColorsPartial,
    #[serde(default)]
    file_colors: FileColorsPartial,
    #[serde(flatten)]
    unknown: BTreeMap<String, serde_json::Value>,
}

impl ThemeFile {
    /// Validate the two keys every theme must carry, and warn about
    /// top-level keys we do not know.
    fn header(&self, warnings: &mut Vec<String>) -> anyhow::Result<(SharedString, Appearance)> {
        if self.name.trim().is_empty() {
            bail!("a theme needs a non-empty `name`");
        }
        for key in self.unknown.keys() {
            warnings.push(format!("unknown key `{key}` ignored"));
        }
        Ok((self.name.trim().to_string().into(), self.appearance))
    }
}

/// A parsed theme plus everything survivable that was wrong with it. The
/// warnings are shown once, in the settings window; they never block loading.
#[derive(Debug, Clone)]
pub struct LoadedTheme {
    pub theme: Theme,
    pub warnings: Vec<String>,
}

const DARK_JSON: &str = include_str!("../themes/graphite-dark.json");
const LIGHT_JSON: &str = include_str!("../themes/graphite-light.json");

impl Theme {
    /// The built-in dark theme (the graphite look of the reference screenshot).
    /// The default appearance.
    pub fn dark() -> Self {
        builtin(DARK_JSON)
    }

    /// The built-in light theme.
    pub fn light() -> Self {
        builtin(LIGHT_JSON)
    }

    /// The built-in for an appearance — the base a partial user theme inherits.
    pub fn builtin(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

    /// Parse a **complete** theme — every color key required, nothing
    /// inherited. The built-ins are loaded this way (see
    /// [`ThemeColorsPartial::into_complete`]).
    fn from_json_complete(source: &str) -> anyhow::Result<LoadedTheme> {
        let file: ThemeFile = serde_json::from_str(source).context("not a valid theme file")?;
        let mut warnings = Vec::new();
        let (name, appearance) = file.header(&mut warnings)?;
        Ok(LoadedTheme {
            theme: Theme {
                name,
                appearance,
                colors: file.colors.into_complete("colors", &mut warnings)?,
                file_colors: file
                    .file_colors
                    .into_complete("file_colors", &mut warnings)?,
            },
            warnings,
        })
    }

    /// Parse a user theme. Absent keys come from [`Theme::builtin`] for the
    /// declared `appearance`, so a partial file is a legal file.
    pub fn from_json(source: &str) -> anyhow::Result<LoadedTheme> {
        let file: ThemeFile = serde_json::from_str(source).context("not a valid theme file")?;
        let mut warnings = Vec::new();
        let (name, appearance) = file.header(&mut warnings)?;
        let base = Theme::builtin(appearance);
        let colors = file
            .colors
            .merge_onto(base.colors, "colors", &mut warnings)?;
        let file_colors =
            file.file_colors
                .merge_onto(base.file_colors, "file_colors", &mut warnings)?;
        Ok(LoadedTheme {
            theme: Theme {
                name,
                appearance,
                colors,
                file_colors,
            },
            warnings,
        })
    }
}

/// Parse one of the embedded built-ins. A failure here is a bug in this crate,
/// not in anything a user did — and it is covered by a test in every build.
fn builtin(source: &str) -> Theme {
    match Theme::from_json_complete(source) {
        Ok(loaded) => loaded.theme,
        Err(error) => panic!("built-in theme is malformed: {error:#}"),
    }
}

/// What the user picked, which is not always one theme.
///
/// `Static` is a single named theme. `Dynamic` is the plan's
/// `appearance: system`: a *pair*, so the app follows macOS's light/dark
/// switch live rather than picking one and staying there. The setting is
/// stored in `settings.json` and reads naturally in both forms:
///
/// ```json
/// "theme": "Graphite Dark"
/// "theme": { "light": "Graphite Light", "dark": "Graphite Dark" }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThemeSelection {
    /// One theme, whatever the system is doing.
    Static(String),
    /// One theme per system appearance.
    Dynamic { light: String, dark: String },
}

impl ThemeSelection {
    /// Follow the system, with the built-ins as the pair.
    pub fn system() -> Self {
        Self::Dynamic {
            light: Theme::light().name.to_string(),
            dark: Theme::dark().name.to_string(),
        }
    }

    /// Which theme name applies right now.
    pub fn name_for(&self, appearance: Appearance) -> &str {
        match (self, appearance) {
            (Self::Static(name), _) => name,
            (Self::Dynamic { light, .. }, Appearance::Light) => light,
            (Self::Dynamic { dark, .. }, Appearance::Dark) => dark,
        }
    }

    /// Whether this selection tracks the system appearance — the thing the
    /// settings window shows as a "Follow system" checkbox, and the reason
    /// the app bothers to observe appearance changes at all.
    pub fn is_dynamic(&self) -> bool {
        matches!(self, Self::Dynamic { .. })
    }
}

impl Default for ThemeSelection {
    fn default() -> Self {
        Self::Static(Theme::dark().name.to_string())
    }
}

/// Every theme the app can switch to: the built-ins, plus whatever loaded out
/// of the user's themes folder.
///
/// Name is identity (it is what the settings file stores), so a user theme
/// named exactly like a built-in **replaces** it — the documented way to
/// re-tint the shipped look without renaming it everywhere.
#[derive(Debug, Clone)]
pub struct ThemeRegistry {
    themes: Vec<Theme>,
}

impl Default for ThemeRegistry {
    fn default() -> Self {
        Self::builtins_only()
    }
}

impl ThemeRegistry {
    /// The two shipped themes, in picker order.
    pub fn builtins_only() -> Self {
        Self {
            themes: vec![Theme::dark(), Theme::light()],
        }
    }

    /// Add or replace a theme by name. Returns whether it displaced one.
    pub fn insert(&mut self, theme: Theme) -> bool {
        match self.themes.iter().position(|t| t.name == theme.name) {
            Some(ix) => {
                self.themes[ix] = theme;
                true
            }
            None => {
                self.themes.push(theme);
                false
            }
        }
    }

    /// Drop every theme that is not a built-in — what a reload of the themes
    /// folder starts from, so a deleted file actually disappears.
    pub fn reset_to_builtins(&mut self) {
        *self = Self::builtins_only();
    }

    pub fn get(&self, name: &str) -> Option<&Theme> {
        self.themes.iter().find(|t| t.name == name)
    }

    /// The active theme for a stored name: the named theme, or the default
    /// when it has gone away (an uninstalled user theme must not leave the app
    /// unpainted).
    pub fn get_or_default(&self, name: &str) -> Theme {
        self.get(name).cloned().unwrap_or_else(Theme::dark)
    }

    pub fn themes(&self) -> &[Theme] {
        &self.themes
    }

    pub fn names(&self) -> impl Iterator<Item = &SharedString> {
        self.themes.iter().map(|t| &t.name)
    }

    pub fn len(&self) -> usize {
        self.themes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.themes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Hsla, hsla};

    #[test]
    fn built_in_themes_parse_and_keep_their_appearances() {
        assert_eq!(Theme::dark().appearance, Appearance::Dark);
        assert_eq!(Theme::light().appearance, Appearance::Light);
        assert!(Theme::dark().surface.l < Theme::light().surface.l);
    }

    #[test]
    fn built_ins_load_without_a_single_warning() {
        for source in [DARK_JSON, LIGHT_JSON] {
            let loaded = Theme::from_json_complete(source).unwrap();
            assert!(
                loaded.warnings.is_empty(),
                "{} warned: {:?}",
                loaded.theme.name,
                loaded.warnings
            );
        }
    }

    /// The M7 promise that let the built-ins move out of Rust and into JSON
    /// without moving a single pixel: these are the exact literals `theme.rs`
    /// carried through M6.
    #[test]
    fn the_dark_built_in_is_bit_identical_to_the_pre_m7_palette() {
        let dark = Theme::dark();
        assert_eq!(dark.surface, hsla(240.0 / 360.0, 0.04, 0.12, 1.0));
        assert_eq!(dark.sidebar, hsla(240.0 / 360.0, 0.05, 0.15, 1.0));
        assert_eq!(dark.panel, hsla(240.0 / 360.0, 0.04, 0.10, 1.0));
        assert_eq!(dark.titlebar, hsla(240.0 / 360.0, 0.05, 0.15, 1.0));
        assert_eq!(dark.text, hsla(0.0, 0.0, 0.92, 1.0));
        assert_eq!(dark.muted, hsla(0.0, 0.0, 0.55, 1.0));
        assert_eq!(dark.accent, hsla(210.0 / 360.0, 0.90, 0.55, 1.0));
        assert_eq!(dark.border, hsla(0.0, 0.0, 0.0, 0.35));
        assert_eq!(dark.error, hsla(0.0, 0.75, 0.58, 1.0));
    }

    #[test]
    fn the_light_built_in_is_bit_identical_to_the_pre_m7_palette() {
        let light = Theme::light();
        assert_eq!(light.surface, hsla(0.0, 0.0, 1.0, 1.0));
        assert_eq!(light.sidebar, hsla(240.0 / 360.0, 0.08, 0.96, 1.0));
        assert_eq!(light.panel, hsla(240.0 / 360.0, 0.08, 0.97, 1.0));
        assert_eq!(light.titlebar, hsla(240.0 / 360.0, 0.08, 0.96, 1.0));
        assert_eq!(light.text, hsla(0.0, 0.0, 0.12, 1.0));
        assert_eq!(light.muted, hsla(0.0, 0.0, 0.45, 1.0));
        assert_eq!(light.accent, hsla(210.0 / 360.0, 0.90, 0.45, 1.0));
        assert_eq!(light.border, hsla(0.0, 0.0, 0.0, 0.12));
        assert_eq!(light.error, hsla(0.0, 0.72, 0.45, 1.0));
    }

    /// M7c gave `selection` a real consumer (the details list and the icon
    /// grid). The built-ins define it as exactly what those two painted
    /// before — the accent at 0.35 alpha — which is what made that wiring
    /// baseline-neutral. If this ever needs changing, the baselines move.
    #[test]
    fn the_built_in_selection_is_the_accent_at_the_alpha_the_views_used() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_eq!(
                theme.selection,
                Hsla {
                    a: 0.35,
                    ..theme.accent
                },
                "{} moved its selection tint",
                theme.name
            );
        }
    }

    #[test]
    fn text_contrasts_with_surface_in_both_built_ins() {
        for theme in [Theme::dark(), Theme::light()] {
            let delta = (theme.text.l - theme.surface.l).abs();
            assert!(delta > 0.5, "text/surface contrast too low: {delta}");
        }
    }

    #[test]
    fn a_partial_user_theme_inherits_the_rest_from_its_appearance() {
        let loaded = Theme::from_json(
            r#"{ "name": "Just Red", "appearance": "dark",
                 "colors": { "accent": "hsl(0, 100%, 50%)" } }"#,
        )
        .unwrap();
        let theme = loaded.theme;
        assert_eq!(theme.name, SharedString::from("Just Red"));
        assert_eq!(theme.accent, color::parse("hsl(0, 100%, 50%)").unwrap());
        // Everything else is the dark built-in, untouched.
        assert_eq!(theme.surface, Theme::dark().surface);
        assert_eq!(theme.file_colors, Theme::dark().file_colors);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn a_light_partial_inherits_from_the_light_built_in() {
        let theme = Theme::from_json(r#"{ "name": "Pale", "appearance": "light" }"#)
            .unwrap()
            .theme;
        assert_eq!(theme.colors, Theme::light().colors);
        assert_eq!(theme.appearance, Appearance::Light);
    }

    #[test]
    fn an_unknown_key_warns_but_still_loads() {
        let loaded = Theme::from_json(
            r#"{ "name": "Typo", "appearance": "dark",
                 "colours": {}, "colors": { "surfase": "hsl(0, 0%, 0%)" } }"#,
        )
        .unwrap();
        assert_eq!(loaded.theme.colors, Theme::dark().colors);
        assert_eq!(
            loaded.warnings,
            vec![
                "unknown key `colours` ignored".to_string(),
                "unknown key `colors.surfase` ignored".to_string(),
            ]
        );
    }

    #[test]
    fn a_bad_color_names_the_key_that_carries_it() {
        let error = Theme::from_json(
            r#"{ "name": "Bad", "appearance": "dark", "colors": { "accent": "puce" } }"#,
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("colors.accent"), "{text}");
        assert!(text.contains("puce"), "{text}");
    }

    #[test]
    fn structurally_broken_files_are_errors_not_panics() {
        for bad in [
            "",
            "{",
            r#"{ "appearance": "dark" }"#,               // no name
            r#"{ "name": "X" }"#,                        // no appearance
            r#"{ "name": "X", "appearance": "grey" }"#,  // not an appearance
            r#"{ "name": "  ", "appearance": "dark" }"#, // blank name
        ] {
            assert!(Theme::from_json(bad).is_err(), "{bad:?} should not load");
        }
    }

    #[test]
    fn the_registry_ships_both_built_ins_and_takes_user_themes() {
        let mut registry = ThemeRegistry::builtins_only();
        assert_eq!(registry.len(), 2);
        assert!(registry.get("Graphite Dark").is_some());

        let mine = Theme::from_json(r#"{ "name": "Mine", "appearance": "dark" }"#)
            .unwrap()
            .theme;
        assert!(!registry.insert(mine));
        assert_eq!(registry.len(), 3);
        assert_eq!(registry.get("Mine").unwrap().appearance, Appearance::Dark);
    }

    #[test]
    fn a_user_theme_may_replace_a_built_in_by_name() {
        let mut registry = ThemeRegistry::builtins_only();
        let override_theme = Theme::from_json(
            r#"{ "name": "Graphite Dark", "appearance": "dark",
                 "colors": { "accent": "hsl(0, 100%, 50%)" } }"#,
        )
        .unwrap()
        .theme;
        assert!(registry.insert(override_theme));
        assert_eq!(registry.len(), 2, "it replaced rather than appended");
        assert_eq!(
            registry.get("Graphite Dark").unwrap().accent,
            color::parse("hsl(0, 100%, 50%)").unwrap()
        );

        registry.reset_to_builtins();
        assert_eq!(
            registry.get("Graphite Dark").unwrap().accent,
            Theme::dark().accent
        );
    }

    #[test]
    fn a_static_selection_ignores_the_system_appearance() {
        let selection = ThemeSelection::Static("Mine".into());
        assert_eq!(selection.name_for(Appearance::Light), "Mine");
        assert_eq!(selection.name_for(Appearance::Dark), "Mine");
        assert!(!selection.is_dynamic());
    }

    #[test]
    fn a_dynamic_selection_picks_a_side() {
        let selection = ThemeSelection::system();
        assert_eq!(selection.name_for(Appearance::Light), "Graphite Light");
        assert_eq!(selection.name_for(Appearance::Dark), "Graphite Dark");
        assert!(selection.is_dynamic());
    }

    /// Both spellings have to survive `settings.json`, and the untagged enum
    /// is the only thing standing between them.
    #[test]
    fn both_selection_forms_round_trip_through_json() {
        for selection in [
            ThemeSelection::Static("Graphite Light".into()),
            ThemeSelection::system(),
        ] {
            let json = serde_json::to_string(&selection).unwrap();
            assert_eq!(
                serde_json::from_str::<ThemeSelection>(&json).unwrap(),
                selection,
                "{json} did not round-trip"
            );
        }
        // The shapes a human would actually type.
        assert_eq!(
            serde_json::from_str::<ThemeSelection>(r#""Graphite Dark""#).unwrap(),
            ThemeSelection::Static("Graphite Dark".into())
        );
        assert_eq!(
            serde_json::from_str::<ThemeSelection>(
                r#"{ "light": "Graphite Light", "dark": "Graphite Dark" }"#
            )
            .unwrap(),
            ThemeSelection::system()
        );
    }

    #[test]
    fn a_missing_theme_falls_back_to_the_default_rather_than_nothing() {
        let registry = ThemeRegistry::builtins_only();
        assert_eq!(registry.get_or_default("Uninstalled"), Theme::dark());
        assert_eq!(registry.get_or_default("Graphite Light"), Theme::light());
    }
}
