use std::path::PathBuf;

use file_explorer_app::{Theme, Workspace, app_state, keymap, settings};
use gpui::{App, AppContext as _, Bounds, Focusable as _, WindowBounds, WindowOptions, px, size};
use gpui_platform::application;

fn main() {
    application().run(|cx: &mut App| {
        app_state::init(cx);
        settings::init(cx);
        keymap::init(cx);
        let bounds = Bounds::centered(None, size(px(1200.0), px(760.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Workspace::new(Theme::dark(), window, cx)),
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
