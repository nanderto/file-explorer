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

pub use ::theme::{
    Appearance, FileColors, LoadedTheme, Theme, ThemeColors, ThemeRegistry, ThemeSelection, color,
};

/// gpui reports four appearances (each has a "vibrant" variant); the theme
/// model has two. Vibrant is the same side of the light/dark line.
pub fn appearance_of(appearance: gpui::WindowAppearance) -> Appearance {
    match appearance {
        gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight => Appearance::Light,
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => Appearance::Dark,
    }
}

/// Debounce for the themes folder. Editors save in bursts (write, rename,
/// chmod); one reload per burst is enough, and the folder is tiny.
pub const THEME_WATCH_LATENCY: Duration = Duration::from_millis(150);

/// The disclosure glyphs, in one place so the sidebar's sections, the
/// sidebar's tree and the details list cannot drift apart.
///
/// Chevrons rather than the filled triangles the app used through M7b:
/// triangles read as a heavier, more "structural" control than a disclosure
/// wants to be, and both Finder and ForkLift use chevrons. Still Unicode
/// glyphs rather than an icon set — see the M7d gap.
pub const DISCLOSURE_COLLAPSED: &str = "\u{203a}";
pub const DISCLOSURE_EXPANDED: &str = "\u{2304}";

/// Only `.json` files are themes. Anything else in the folder — a README, an
/// editor's swap file — is ignored without comment.
const THEME_EXTENSION: &str = "json";

/// The active theme plus everything the picker needs to offer.
pub struct ActiveTheme {
    registry: ThemeRegistry,
    active: Theme,
    /// What the user *asked* for, which is not always what is painted: a
    /// settings file naming a user theme whose file is missing paints the
    /// default, and re-adding the file must bring the user's choice back
    /// without them re-picking it. A `Dynamic` selection also names two
    /// themes at once and only one of them is active (M7b).
    selection: ThemeSelection,
    /// The system's light/dark state, as last reported by
    /// `Window::observe_window_appearance`. Only a `Dynamic` selection reads
    /// it; a `Static` one paints the same theme either way.
    system_appearance: Appearance,
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
    /// watching the user's themes folder. `selection` is what the settings
    /// file asked for.
    pub fn init(selection: ThemeSelection, cx: &mut App) {
        Self::init_in(Self::default_themes_dir(), selection, cx);
    }

    /// [`init`](Self::init) against an explicit folder — what tests use, so
    /// they can point the watcher at a `FakeVfs` path.
    pub fn init_in(themes_dir: PathBuf, selection: ThemeSelection, cx: &mut App) {
        // The system's current state, before any window exists to observe it.
        let system_appearance = appearance_of(cx.window_appearance());
        let registry = ThemeRegistry::builtins_only();
        let active = registry.get_or_default(selection.name_for(system_appearance));
        cx.set_global(ActiveTheme {
            registry,
            active,
            selection,
            system_appearance,
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

    /// What the user picked, which is what the settings file stores.
    pub fn selection(cx: &App) -> ThemeSelection {
        cx.try_global::<ActiveTheme>()
            .map(|active| active.selection.clone())
            .unwrap_or_default()
    }

    /// The theme name in force right now — the selection resolved against the
    /// current system appearance.
    pub fn requested_name(cx: &App) -> SharedString {
        cx.try_global::<ActiveTheme>()
            .map(|active| {
                SharedString::from(
                    active
                        .selection
                        .name_for(active.system_appearance)
                        .to_string(),
                )
            })
            .unwrap_or_else(|| Theme::dark().name)
    }

    /// The system's light/dark state as the theme system last saw it.
    pub fn system_appearance(cx: &App) -> Appearance {
        cx.try_global::<ActiveTheme>()
            .map(|active| active.system_appearance)
            .unwrap_or(Appearance::Dark)
    }

    pub fn diagnostics(cx: &App) -> Vec<ThemeDiagnostic> {
        cx.try_global::<ActiveTheme>()
            .map(|active| active.diagnostics.clone())
            .unwrap_or_default()
    }

    /// Switch to one named theme. A name that is not installed is still
    /// *remembered* but paints the default. Returns whether the painted theme
    /// changed.
    pub fn select(name: impl Into<SharedString>, cx: &mut App) -> bool {
        Self::set_selection(ThemeSelection::Static(name.into().to_string()), cx)
    }

    /// Switch to a whole selection — one theme, or the light/dark pair that
    /// follows the system (plan §6's `appearance: system`).
    pub fn set_selection(selection: ThemeSelection, cx: &mut App) -> bool {
        if cx.try_global::<ActiveTheme>().is_none() {
            return false;
        }
        let changed = cx.update_global::<ActiveTheme, _>(|active, _| {
            active.selection = selection;
            active.resolve()
        });
        if changed {
            cx.refresh_windows();
        }
        changed
    }

    /// The system flipped between light and dark. A no-op for a `Static`
    /// selection, which is why the observer is cheap to leave installed.
    pub fn set_system_appearance(appearance: Appearance, cx: &mut App) -> bool {
        if cx.try_global::<ActiveTheme>().is_none() {
            return false;
        }
        let changed = cx.update_global::<ActiveTheme, _>(|active, _| {
            if active.system_appearance == appearance {
                return false;
            }
            active.system_appearance = appearance;
            active.resolve()
        });
        if changed {
            cx.refresh_windows();
        }
        changed
    }

    /// Re-derive [`Self::active`] from the selection, the system appearance
    /// and the registry — the one place that decides what is painted.
    /// Returns whether it moved.
    fn resolve(&mut self) -> bool {
        let next = self
            .registry
            .get_or_default(self.selection.name_for(self.system_appearance));
        let changed = next != self.active;
        self.active = next;
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
            active.resolve();
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
            ActiveTheme::init_in(PathBuf::from(THEMES), ThemeSelection::default(), cx);
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

    /// Plan §6's `appearance: system`: the selection names a theme per side
    /// of the system's light/dark switch, and flipping the system swaps which
    /// one is painted without the user touching anything.
    #[gpui::test]
    fn a_dynamic_selection_follows_the_system_appearance(cx: &mut TestAppContext) {
        boot(cx, json!({}));
        cx.update(|cx| {
            ActiveTheme::set_selection(ThemeSelection::system(), cx);
            // Whatever the test platform reports at boot, pin both sides
            // explicitly and check each.
            ActiveTheme::set_system_appearance(Appearance::Dark, cx);
            assert_eq!(theme(cx), &Theme::dark());

            assert!(ActiveTheme::set_system_appearance(Appearance::Light, cx));
            assert_eq!(theme(cx), &Theme::light());
            assert_eq!(
                ActiveTheme::requested_name(cx),
                SharedString::from("Graphite Light"),
                "the name in force follows the appearance too"
            );

            // Reporting the same appearance twice is not a repaint.
            assert!(!ActiveTheme::set_system_appearance(Appearance::Light, cx));
        });
    }

    #[gpui::test]
    fn a_static_selection_ignores_the_system_appearance(cx: &mut TestAppContext) {
        boot(cx, json!({}));
        cx.update(|cx| {
            ActiveTheme::select("Graphite Dark", cx);
            ActiveTheme::set_system_appearance(Appearance::Dark, cx);
            assert!(
                !ActiveTheme::set_system_appearance(Appearance::Light, cx),
                "a static selection must not repaint on a system flip"
            );
            assert_eq!(theme(cx), &Theme::dark());
        });
    }

    /// A dynamic selection naming a *user* theme still hot-reloads — the
    /// resolve path is shared, so this is really a check that the folder
    /// reload re-resolves through the selection rather than a stored name.
    #[gpui::test]
    fn a_dynamic_selection_picks_up_an_edited_user_theme(cx: &mut TestAppContext) {
        let vfs = boot(
            cx,
            json!({ "themes": { "night.json": theme_json("Night", "hsl(0, 100%, 50%)") } }),
        );
        cx.update(|cx| {
            ActiveTheme::set_selection(
                ThemeSelection::Dynamic {
                    light: "Graphite Light".into(),
                    dark: "Night".into(),
                },
                cx,
            );
            ActiveTheme::set_system_appearance(Appearance::Dark, cx);
            assert_eq!(theme(cx).name, SharedString::from("Night"));
        });

        vfs.write_file(
            Path::new("/config/themes/night.json"),
            theme_json("Night", "hsl(120, 100%, 50%)").into_bytes(),
        );
        settle_watch(cx);
        cx.update(|cx| {
            assert_eq!(
                theme(cx).accent,
                color::parse("hsl(120, 100%, 50%)").unwrap(),
                "the dark half of the pair did not reload"
            );
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
