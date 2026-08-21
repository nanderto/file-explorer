//! Minimal theme model for the M0 skeleton.
//!
//! This is deliberately small: the full JSON-loaded theme system
//! (see docs/file-explorer-plan.md §6) replaces the hard-coded palettes here
//! at M7. Widgets must take every color from a `Theme` — never inline one.

use gpui::{Hsla, hsla};

/// Which built-in appearance a theme renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

/// The active color palette. All UI colors come from here.
#[derive(Debug, Clone)]
pub struct Theme {
    pub appearance: Appearance,
    /// Main content pane background.
    pub surface: Hsla,
    /// Sidebar background.
    pub sidebar: Hsla,
    /// Info panel background.
    pub panel: Hsla,
    /// Titlebar background.
    pub titlebar: Hsla,
    /// Primary text.
    pub text: Hsla,
    /// Secondary/muted text (section headers, status lines).
    pub muted: Hsla,
    /// Accent for selection and highlights.
    pub accent: Hsla,
    /// Hairline borders between regions.
    pub border: Hsla,
}

impl Theme {
    /// The built-in dark theme (graphite look from the reference screenshot).
    pub fn dark() -> Self {
        Self {
            appearance: Appearance::Dark,
            surface: hsla(240.0 / 360.0, 0.04, 0.12, 1.0),
            sidebar: hsla(240.0 / 360.0, 0.05, 0.15, 1.0),
            panel: hsla(240.0 / 360.0, 0.04, 0.10, 1.0),
            titlebar: hsla(240.0 / 360.0, 0.05, 0.15, 1.0),
            text: hsla(0.0, 0.0, 0.92, 1.0),
            muted: hsla(0.0, 0.0, 0.55, 1.0),
            accent: hsla(210.0 / 360.0, 0.90, 0.55, 1.0),
            border: hsla(0.0, 0.0, 0.0, 0.35),
        }
    }

    /// The built-in light theme.
    pub fn light() -> Self {
        Self {
            appearance: Appearance::Light,
            surface: hsla(0.0, 0.0, 1.0, 1.0),
            sidebar: hsla(240.0 / 360.0, 0.08, 0.96, 1.0),
            panel: hsla(240.0 / 360.0, 0.08, 0.97, 1.0),
            titlebar: hsla(240.0 / 360.0, 0.08, 0.96, 1.0),
            text: hsla(0.0, 0.0, 0.12, 1.0),
            muted: hsla(0.0, 0.0, 0.45, 1.0),
            accent: hsla(210.0 / 360.0, 0.90, 0.45, 1.0),
            border: hsla(0.0, 0.0, 0.0, 0.12),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_themes_have_distinct_appearances() {
        assert_eq!(Theme::dark().appearance, Appearance::Dark);
        assert_eq!(Theme::light().appearance, Appearance::Light);
        // Dark surface must actually be darker than light surface.
        assert!(Theme::dark().surface.l < Theme::light().surface.l);
    }

    #[test]
    fn text_contrasts_with_surface() {
        for theme in [Theme::dark(), Theme::light()] {
            let delta = (theme.text.l - theme.surface.l).abs();
            assert!(delta > 0.5, "text/surface contrast too low: {delta}");
        }
    }
}
