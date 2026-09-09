use std::path::PathBuf;

use file_explorer_app::{ActiveTheme, ThemeSelection, Workspace, app_state, keymap, settings};
use gpui::{
    App, AppContext as _, Bounds, Focusable as _, TitlebarOptions, WindowBounds, WindowOptions,
    point, px, size,
};
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
                    // Draw our own titlebar instead of letting macOS paint a
                    // grey one above ours. Without this the window has *two*
                    // bars — the system's, which no theme can reach, above
                    // the workspace's themed row — and a theme that re-tints
                    // the frame visibly stops at the top of it.
                    //
                    // `appears_transparent` hides the system bar but keeps
                    // the traffic lights, so they now sit inside the
                    // workspace's 40px row: positioned 14px in and 14px down
                    // to centre them in it, and cleared by the `px(80.0)`
                    // left padding that row has always carried.
                    titlebar: Some(TitlebarOptions {
                        title: Some(file_explorer_app::APP_DISPLAY_NAME.into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(14.0), px(14.0))),
                    }),
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
