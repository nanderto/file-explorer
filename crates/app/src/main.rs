use std::path::PathBuf;

use file_explorer_app::{ActiveTheme, ThemeSelection, Workspace, app_state, keymap, settings};
use gpui::{App, AppContext as _, Bounds, Focusable as _, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

fn main() {
    application().run(|cx: &mut App| {
        app_state::init(cx);
        // Before the window: the first frame must already have a theme, and
        // the folder read + watch that follow are asynchronous. The name from
        // `settings.json` is applied by `settings::init` when its own load
        // lands (both are in flight together; the theme system starts on the
        // default and switches once).
        ActiveTheme::init(ThemeSelection::default(), cx);
        settings::init(cx);
        keymap::init_with_overrides(cx, keymap::default_keymap_path());
        let bounds = Bounds::centered(None, size(px(1200.0), px(760.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Workspace::new(window, cx)),
            )
            .expect("failed to open window");
        // Open the home directory by default (M1: real listing on boot) and
        // give the details view keyboard focus.
        window
            .update(cx, |workspace, window, cx| {
                let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| pane.navigate_to(&home, cx));
                let dir_view = pane.read(cx).dir_view().clone();
                window.focus(&dir_view.focus_handle(cx), cx);
            })
            .expect("failed to open the initial directory");
        cx.activate(true);
    });
}
