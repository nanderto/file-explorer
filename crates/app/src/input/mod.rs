//! Text input, vendored from adabraka-ui (MIT). See ../../VENDORED.md.

pub mod text_input;

pub use text_input::{InputEvent, InputState};

use gpui::{App, Hsla};

/// The three colors the vendored [`InputState`] paints with, taken from the
/// active theme (placeholder, cursor, selection). One place, so every text
/// field in the app — address bar, search, inline rename — agrees, and so a
/// theme change has exactly one thing to update (see
/// [`refresh_input_colors`]).
pub(crate) fn input_colors(cx: &App) -> (Hsla, Hsla, Hsla) {
    let theme = crate::theme::theme(cx);
    (theme.muted, theme.accent, theme.accent.opacity(0.25))
}

/// Push the current theme's colors into an existing field. Every owner of an
/// [`InputState`] calls this from its own `render`, which is what makes a
/// live theme switch reach text fields — they are the one place in the app
/// that holds painted colors as *state* rather than reading them per frame
/// (the vendored widget's design, kept as vendored).
pub(crate) fn refresh_input_colors(input: &gpui::Entity<InputState>, cx: &mut App) {
    let (placeholder, cursor, selection) = input_colors(cx);
    input.update(cx, |state, _| {
        state.set_colors(placeholder, cursor, selection)
    });
}
