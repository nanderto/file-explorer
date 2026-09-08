//! The app's handle on the theme system: which [`Theme`] is active, where the
//! user's themes come from, and how a change reaches the screen.
//!
//! The model itself lives in `crates/theme` (plan §6) — this module is the
//! glue:
//!
//! * **[`ActiveTheme`] is a gpui `Global`.** Before M7 every view was handed a
//!   `Theme` at construction and kept a clone, which is unanswerable once a
//!   theme can change while the app runs. Now a view reads [`theme`] inside
//!   `render` and a switch is one [`ActiveTheme::select`] plus a window
//!   refresh.
//! * **User themes hot-reload.** `~/Library/Application Support/file-explorer/themes/`
//!   is watched with the same `Vfs::watch` the pane uses for the open
//!   directory; a save re-reads the folder on the background executor and
//!   repaints. A file that fails to parse is reported and *skipped* — the
//!   previous good copy of that theme stays on screen rather than the app
//!   going unpainted.
//! * **Nothing here touches the disk on the UI thread** (§5). Reading the
//!   folder, parsing, and registering/unregistering the watch all happen on
//!   the background executor; only the finished registry comes back.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fs_core::Vfs;
use futures::StreamExt;
use gpui::{App, AppContext as _, BorrowAppContext as _, Global, SharedString, Task};

use crate::app_state::FsContext;
use crate::watch_guard::BackgroundWatchGuard;

pub use ::theme::{Appearance, FileColors, LoadedTheme, Theme, ThemeColors, ThemeRegistry, color};

/// Debounce for the themes folder. Editors save in bursts (write, rename,
/// chmod); one reload per burst is enough, and the folder is tiny.
pub const THEME_WATCH_LATENCY: Duration = Duration::from_millis(150);

/// Only `.json` files are themes. Anything else in the folder — a README, an
/// editor's swap file — is ignored without comment.
const THEME_EXTENSION: &str = "json";

/// The active theme plus everything the picker needs to offer.
pub struct ActiveTheme {
    registry: ThemeRegistry,
    active: Theme,
    /// The name the user *asked* for, which is not always the name of
    /// [`Self::active`]: a settings file naming a user theme whose file is
    /// missing paints the default, and re-adding the file must bring the
    /// user's choice back without them re-picking it.
    requested: SharedString,
    themes_dir: PathBuf,
    /// Per-file complaints from the last folder read — bad syntax, unknown
    /// keys. Surfaced by the settings window (M7b); kept here because the
    /// reload that produced them is asynchronous and nothing else outlives it.
    diagnostics: Vec<ThemeDiagnostic>,
    _reload: Option<Task<()>>,
    _watch: Option<Task<()>>,
    _guard: Option<BackgroundWatchGuard>,
}

impl Global for ActiveTheme {}

/// What went wrong with one file in the themes folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeDiagnostic {
    pub path: PathBuf,
    pub message: String,
    /// A warning (the theme loaded anyway) rather than an error.
    pub warning: bool,
}

/// The active theme. **This is what `render` calls.**
///
/// Falls back to the built-in dark theme when the global has not been
/// installed, so a `#[gpui::test]` that only exercises one view does not have
/// to boot the theme system to paint.
pub fn theme(cx: &App) -> &Theme {
    match cx.try_global::<ActiveTheme>() {
        Some(active) => &active.active,
        None => default_theme(),
    }
}

fn default_theme() -> &'static Theme {
    static DEFAULT: OnceLock<Theme> = OnceLock::new();
    DEFAULT.get_or_init(Theme::dark)
}

impl ActiveTheme {
    /// Install the global with the built-ins only, then read and start
    /// watching the user's themes folder. `requested` is the theme name from
    /// the settings file.
    pub fn init(requested: impl Into<SharedString>, cx: &mut App) {
        Self::init_in(Self::default_themes_dir(), requested, cx);
    }

    /// [`init`](Self::init) against an explicit folder — what tests use, so
    /// they can point the watcher at a `FakeVfs` path.
    pub fn init_in(themes_dir: PathBuf, requested: impl Into<SharedString>, cx: &mut App) {
        let requested = requested.into();
        let registry = ThemeRegistry::builtins_only();
        let active = registry.get_or_default(&requested);
        cx.set_global(ActiveTheme {
            registry,
            active,
            requested,
            themes_dir,
            diagnostics: Vec::new(),
            _reload: None,
            _watch: None,
            _guard: None,
        });
        Self::reload(cx);
        Self::start_watching(cx);
    }

    /// `~/Library/Application Support/file-explorer/themes` (and the platform
    /// equivalent elsewhere) — a sibling of `settings.json`, per plan §6.
    pub fn default_themes_dir() -> PathBuf {
        crate::settings::AppSettings::default_path()
            .parent()
            .map(|dir| dir.join("themes"))
            .unwrap_or_else(|| PathBuf::from("themes"))
    }

    pub fn get(cx: &App) -> &Theme {
        theme(cx)
    }

    pub fn registry(cx: &App) -> ThemeRegistry {
        cx.try_global::<ActiveTheme>()
            .map(|active| active.registry.clone())
            .unwrap_or_default()
    }

    /// The name the user picked, which is what the settings file stores.
    pub fn requested_name(cx: &App) -> SharedString {
        cx.try_global::<ActiveTheme>()
            .map(|active| active.requested.clone())
            .unwrap_or_else(|| Theme::dark().name)
    }

    pub fn diagnostics(cx: &App) -> Vec<ThemeDiagnostic> {
        cx.try_global::<ActiveTheme>()
            .map(|active| active.diagnostics.clone())
            .unwrap_or_default()
    }

    /// Switch themes. A name that is not installed is still *remembered* (see
    /// [`Self::requested`]) but paints the default. Returns whether the
    /// painted theme changed.
    pub fn select(name: impl Into<SharedString>, cx: &mut App) -> bool {
        let name = name.into();
        let Some(active) = cx.try_global::<ActiveTheme>() else {
            return false;
        };
        let next = active.registry.get_or_default(&name);
        let changed = next != active.active;
        cx.update_global::<ActiveTheme, _>(|active, _| {
            active.requested = name;
            active.active = next;
        });
        if changed {
            cx.refresh_windows();
        }
        changed
    }

    /// Re-read the themes folder in the background and install the result.
    /// Safe to call at any time — a reload already in flight is superseded.
    pub fn reload(cx: &mut App) {
        let Some(active) = cx.try_global::<ActiveTheme>() else {
            return;
        };
        let dir = active.themes_dir.clone();
        let vfs = FsContext::global(cx).vfs.clone();
        let task = cx.spawn(async move |cx| {
            let loaded = cx.background_spawn(read_themes_dir(vfs, dir)).await;
            cx.update(|cx| Self::install(loaded, cx));
        });
        cx.update_global::<ActiveTheme, _>(|active, _| active._reload = Some(task));
    }

    /// Apply a folder read: built-ins, then every file that parsed, then
    /// re-resolve the active theme by the *requested* name.
    fn install(loaded: Vec<UserTheme>, cx: &mut App) {
        if cx.try_global::<ActiveTheme>().is_none() {
            return;
        }
        let changed = cx.update_global::<ActiveTheme, _>(|active, _| {
            let before = active.active.clone();
            active.registry.reset_to_builtins();
            active.diagnostics.clear();
            for user in loaded {
                match user.result {
                    Ok(theme) => {
                        for warning in theme.warnings {
                            active.diagnostics.push(ThemeDiagnostic {
                                path: user.path.clone(),
                                message: warning,
                                warning: true,
                            });
                        }
                        active.registry.insert(theme.theme);
                    }
                    Err(message) => active.diagnostics.push(ThemeDiagnostic {
                        path: user.path,
                        message,
                        warning: false,
                    }),
                }
            }
            active.active = active.registry.get_or_default(&active.requested);
            active.active != before
        });
        if changed {
            cx.refresh_windows();
        }
    }

    /// Keep the themes folder live. Registration and unregistration both run
    /// on the background executor — `Vfs::watch` is disk-touching at both ends
    /// (see [`BackgroundWatchGuard`]).
    fn start_watching(cx: &mut App) {
        let Some(active) = cx.try_global::<ActiveTheme>() else {
            return;
        };
        let dir = active.themes_dir.clone();
        let vfs = FsContext::global(cx).vfs.clone();
        let executor = cx.background_executor().clone();
        let task = cx.spawn(async move |cx| {
            let (mut stream, guard) = cx
                .background_spawn(async move { vfs.watch(&dir, THEME_WATCH_LATENCY) })
                .await;
            let guard = BackgroundWatchGuard::new(guard, executor);
            let stored = cx.update(|cx| {
                if !cx.has_global::<ActiveTheme>() {
                    return false;
                }
                cx.update_global::<ActiveTheme, _>(|active, _| active._guard = Some(guard));
                true
            });
            if !stored {
                return;
            }
            // The stream ends when the guard is dropped — which happens when
            // the global goes away, taking this task with it.
            while stream.next().await.is_some() {
                cx.update(ActiveTheme::reload);
            }
        });
        cx.update_global::<ActiveTheme, _>(|active, _| active._watch = Some(task));
    }
}

/// One file's worth of the themes folder.
struct UserTheme {
    path: PathBuf,
    result: Result<LoadedTheme, String>,
}

/// Read and parse every `*.json` in `dir`, in filename order so two themes
/// claiming the same name resolve the same way on every run. A folder that
/// does not exist is not an error — most installs have no user themes.
async fn read_themes_dir(vfs: Arc<dyn Vfs>, dir: PathBuf) -> Vec<UserTheme> {
    let Ok(mut entries) = vfs.read_dir(&dir).await else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    while let Some(entry) = entries.next().await {
        let Ok(entry) = entry else { continue };
        let path = entry.path.to_path_buf();
        if is_theme_file(&path) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let result = match vfs.load(&path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Theme::from_json(&text).map_err(|error| format!("{error:#}")),
                Err(_) => Err("not UTF-8".to_string()),
            },
            Err(error) => Err(format!("{error:#}")),
        };
        loaded.push(UserTheme { path, result });
    }
    loaded
}

fn is_theme_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case(THEME_EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{self, GpuiSpawner, LoggingOpener};
    use fs_core::{FakeVfs, Spawner, StubPlatform};
    use gpui::TestAppContext;
    use serde_json::json;

    const THEMES: &str = "/config/themes";

    fn boot(cx: &mut TestAppContext, tree: serde_json::Value) -> Arc<FakeVfs> {
        let vfs = cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree("/config", tree);
            app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(StubPlatform::new()),
            );
            ActiveTheme::init_in(PathBuf::from(THEMES), Theme::dark().name, cx);
            vfs
        });
        cx.run_until_parked();
        vfs
    }

    /// A watcher batch only arrives after the debounce; the deterministic
    /// executor needs the clock moved for it.
    fn settle_watch(cx: &mut TestAppContext) {
        cx.executor()
            .advance_clock(THEME_WATCH_LATENCY + Duration::from_millis(10));
        cx.run_until_parked();
    }

    fn theme_json(name: &str, accent: &str) -> String {
        json!({ "name": name, "appearance": "dark", "colors": { "accent": accent } }).to_string()
    }

    #[gpui::test]
    fn with_no_themes_folder_the_built_ins_are_the_whole_registry(cx: &mut TestAppContext) {
        boot(cx, json!({}));
        cx.update(|cx| {
            assert_eq!(ActiveTheme::registry(cx).len(), 2);
            assert_eq!(theme(cx), &Theme::dark());
            assert!(ActiveTheme::diagnostics(cx).is_empty());
        });
    }

    #[gpui::test]
    fn a_user_theme_in_the_folder_joins_the_registry(cx: &mut TestAppContext) {
        boot(
            cx,
            json!({ "themes": { "mine.json": theme_json("Mine", "hsl(0, 100%, 50%)") } }),
        );
        cx.update(|cx| {
            assert_eq!(ActiveTheme::registry(cx).len(), 3);
            assert!(ActiveTheme::registry(cx).get("Mine").is_some());
            // Not selected — only installed.
            assert_eq!(theme(cx).name, Theme::dark().name);
        });
    }

    #[gpui::test]
    fn selecting_a_theme_repaints_and_is_remembered(cx: &mut TestAppContext) {
        boot(
            cx,
            json!({ "themes": { "mine.json": theme_json("Mine", "hsl(0, 100%, 50%)") } }),
        );
        cx.update(|cx| {
            assert!(ActiveTheme::select("Mine", cx));
            assert_eq!(theme(cx).accent, color::parse("hsl(0, 100%, 50%)").unwrap());
            assert_eq!(ActiveTheme::requested_name(cx), SharedString::from("Mine"));
            // Selecting it again is a no-op, not a repaint.
            assert!(!ActiveTheme::select("Mine", cx));
        });
    }

    #[gpui::test]
    fn editing_a_theme_file_repaints_without_reselecting_it(cx: &mut TestAppContext) {
        let vfs = boot(
            cx,
            json!({ "themes": { "mine.json": theme_json("Mine", "hsl(0, 100%, 50%)") } }),
        );
        cx.update(|cx| assert!(ActiveTheme::select("Mine", cx)));

        vfs.write_file(
            Path::new("/config/themes/mine.json"),
            theme_json("Mine", "hsl(120, 100%, 50%)").into_bytes(),
        );
        settle_watch(cx);

        cx.update(|cx| {
            assert_eq!(
                theme(cx).accent,
                color::parse("hsl(120, 100%, 50%)").unwrap(),
                "the hot reload did not reach the active theme"
            );
        });
    }

    #[gpui::test]
    fn a_broken_theme_file_is_reported_and_skipped(cx: &mut TestAppContext) {
        boot(
            cx,
            json!({ "themes": {
                "good.json": theme_json("Good", "hsl(0, 100%, 50%)"),
                "broken.json": "{ not json",
            } }),
        );
        cx.update(|cx| {
            assert!(ActiveTheme::registry(cx).get("Good").is_some());
            let diagnostics = ActiveTheme::diagnostics(cx);
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(
                diagnostics[0].path,
                PathBuf::from("/config/themes/broken.json")
            );
            assert!(!diagnostics[0].warning);
        });
    }

    #[gpui::test]
    fn deleting_the_active_theme_falls_back_to_the_default(cx: &mut TestAppContext) {
        let vfs = boot(
            cx,
            json!({ "themes": { "mine.json": theme_json("Mine", "hsl(0, 100%, 50%)") } }),
        );
        cx.update(|cx| assert!(ActiveTheme::select("Mine", cx)));

        vfs.remove_path(Path::new("/config/themes/mine.json"));
        settle_watch(cx);
        cx.update(|cx| {
            assert_eq!(
                theme(cx),
                &Theme::dark(),
                "a missing theme must not blank the app"
            );
            // ...but the choice is remembered, so restoring the file restores it.
            assert_eq!(ActiveTheme::requested_name(cx), SharedString::from("Mine"));
        });

        vfs.write_file(
            Path::new("/config/themes/mine.json"),
            theme_json("Mine", "hsl(0, 100%, 50%)").into_bytes(),
        );
        settle_watch(cx);
        cx.update(|cx| assert_eq!(theme(cx).name, SharedString::from("Mine")));
    }

    #[gpui::test]
    fn non_json_files_in_the_folder_are_ignored(cx: &mut TestAppContext) {
        boot(
            cx,
            json!({ "themes": {
                "README.md": "these are themes",
                "mine.json": theme_json("Mine", "hsl(0, 100%, 50%)"),
            } }),
        );
        cx.update(|cx| {
            assert_eq!(ActiveTheme::registry(cx).len(), 3);
            assert!(ActiveTheme::diagnostics(cx).is_empty());
        });
    }

    #[gpui::test]
    fn the_watch_is_unregistered_when_the_global_goes_away(cx: &mut TestAppContext) {
        let vfs = boot(cx, json!({ "themes": {} }));
        assert_eq!(vfs.watcher_count(), 1);
        cx.update(|cx| cx.remove_global::<ActiveTheme>());
        cx.run_until_parked();
        assert_eq!(vfs.watcher_count(), 0, "the guard outlived the global");
    }
}
