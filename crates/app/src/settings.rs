//! The settings store (ARCHITECTURE.md §1 `settings.rs`, §M7).
//!
//! M2 shipped a stub: one JSON file holding `favorites`, read once at boot.
//! M7b makes it the real thing, in the shape the theme system already
//! established:
//!
//! * **Embedded defaults, refined by the user file.** `settings/defaults.json`
//!   is compiled in and is the complete document; the file on disk is a
//!   *partial* overlay, so a key the user never wrote — or one this version
//!   does not recognise — can never leave a setting undefined. Unknown keys
//!   are reported rather than silently dropped.
//! * **The file is watched.** Editing `settings.json` in a text editor
//!   applies live, exactly like a theme file. Our own writes come back
//!   through the same watcher and are recognised as no-ops by comparison.
//! * **Writes are serialized.** Two settings changed in quick succession must
//!   not race two `atomic_write`s to the same path — the second could land
//!   first and lose the first change. One writer task drains a single pending
//!   slot, so the last content written always wins and no write interleaves.
//! * **Only what differs from the defaults is written.** The file stays small
//!   and readable, and a default this app changes later reaches users who
//!   never overrode it.
//!
//! Everything still goes through the `Vfs` on the background executor: the UI
//! thread never touches the disk (§5).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fs_core::Vfs;
use futures::StreamExt;
use futures::future::BoxFuture;
use gpui::{App, AppContext as _, BorrowAppContext as _, Global, Task};
use serde::{Deserialize, Serialize};

use crate::app_state::FsContext;
use crate::theme::{ActiveTheme, ThemeSelection};
use crate::watch_guard::BackgroundWatchGuard;

/// Debounce for the settings file. An editor saves in a burst (write, rename,
/// chmod) and one reload per burst is enough.
pub const SETTINGS_WATCH_LATENCY: Duration = Duration::from_millis(150);

/// How many recent folders the sidebar keeps. Finder shows a comparable
/// handful; the section has to stay scannable at a glance, and the sidebar is
/// not a history browser (that is what `cmd-[` is for).
pub const MAX_RECENTS: usize = 10;

/// The complete document. Compiled in, so a setting always has a value.
const DEFAULTS_JSON: &str = include_str!("../settings/defaults.json");

/// The settings, as the app reads them: defaults with the user's file applied
/// on top. Every field is present — "unset" is not a state a reader has to
/// think about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsContent {
    /// Sidebar favorites, in display order.
    pub favorites: Vec<PathBuf>,
    /// Whether the default Favorites have ever been seeded (M7d-b).
    ///
    /// A flag rather than "is `favorites` empty?", because those are
    /// different questions: a user who unpins everything *means* it, and
    /// re-seeding on the next launch would be the app arguing with them. Once
    /// true it stays true.
    pub favorites_seeded: bool,
    /// Recently visited folders, most recent first (M7d-b). Capped at
    /// [`MAX_RECENTS`]; every pane navigation pushes onto it.
    pub recents: Vec<PathBuf>,
    /// The chosen theme: one name, or the light/dark pair that follows the
    /// system (plan §6's `appearance: system`).
    pub theme: ThemeSelection,
    /// Plan §3: "Delete to trash — **Delete** key, no modifier; confirmation
    /// optional (setting)". Off by default, which is the Explorer behavior
    /// the app has had since M3.
    pub confirm_delete_to_trash: bool,
    /// Plan §3: "Folders always grouped first (setting)". On by default,
    /// which is `SortSpec::default()`.
    pub folders_first: bool,
}

impl Default for SettingsContent {
    fn default() -> Self {
        match serde_json::from_str(DEFAULTS_JSON) {
            Ok(content) => content,
            // A malformed embedded default is a bug in this crate, not
            // anything a user did — and a test parses it on every build.
            Err(error) => panic!("embedded settings defaults are malformed: {error}"),
        }
    }
}

/// The file on disk: every key optional, unknown keys kept so they can be
/// reported.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsPartial {
    favorites: Option<Vec<PathBuf>>,
    favorites_seeded: Option<bool>,
    recents: Option<Vec<PathBuf>>,
    theme: Option<ThemeSelection>,
    confirm_delete_to_trash: Option<bool>,
    folders_first: Option<bool>,
    #[serde(flatten)]
    unknown: BTreeMap<String, serde_json::Value>,
}

impl SettingsPartial {
    fn merge_onto(self, mut base: SettingsContent, warnings: &mut Vec<String>) -> SettingsContent {
        if let Some(favorites) = self.favorites {
            base.favorites = favorites;
        }
        if let Some(seeded) = self.favorites_seeded {
            base.favorites_seeded = seeded;
        }
        if let Some(recents) = self.recents {
            base.recents = recents;
        }
        if let Some(theme) = self.theme {
            base.theme = theme;
        }
        if let Some(confirm) = self.confirm_delete_to_trash {
            base.confirm_delete_to_trash = confirm;
        }
        if let Some(folders_first) = self.folders_first {
            base.folders_first = folders_first;
        }
        for key in self.unknown.keys() {
            warnings.push(format!("unknown setting `{key}` ignored"));
        }
        base
    }
}

/// App settings global (ARCHITECTURE.md §2 `AppSettings`). Mutate via
/// `cx.update_global::<AppSettings, _>(...)` and call [`AppSettings::save`]
/// afterwards to persist.
pub struct AppSettings {
    content: SettingsContent,
    path: PathBuf,
    /// Complaints about the last file read (unknown or malformed keys),
    /// for the settings window to show.
    warnings: Vec<String>,
    /// The one place a pending write lives, shared with the writer task.
    writer: Arc<Mutex<WriterState>>,
    _watch: Option<Task<()>>,
    _guard: Option<BackgroundWatchGuard>,
}

impl Global for AppSettings {}

/// Serializes writes: one task drains this, so the newest content always wins
/// and two saves can never interleave on the same path.
#[derive(Default)]
struct WriterState {
    pending: Option<SettingsContent>,
    writing: bool,
}

impl AppSettings {
    /// Defaults, persisted at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            content: SettingsContent::default(),
            path,
            warnings: Vec::new(),
            writer: Arc::new(Mutex::new(WriterState::default())),
            _watch: None,
            _guard: None,
        }
    }

    /// Default location: `<platform config dir>/file-explorer/settings.json`
    /// (e.g. `~/Library/Application Support` on macOS, `%APPDATA%` on Windows).
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("file-explorer")
            .join("settings.json")
    }

    /// Load settings from `path` through the Vfs. A missing or unparseable
    /// file yields the defaults — settings must never block or fail boot.
    pub async fn load(vfs: Arc<dyn Vfs>, path: PathBuf) -> Self {
        let mut settings = Self::new(path.clone());
        let Ok(bytes) = vfs.load(&path).await else {
            return settings;
        };
        let mut warnings = Vec::new();
        match serde_json::from_slice::<SettingsPartial>(&bytes) {
            Ok(partial) => {
                settings.content = partial.merge_onto(SettingsContent::default(), &mut warnings)
            }
            Err(error) => warnings.push(format!("settings.json could not be read: {error}")),
        }
        settings.warnings = warnings;
        settings
    }

    pub fn global(cx: &App) -> &AppSettings {
        cx.global::<AppSettings>()
    }

    /// Everything as read, for the settings window and for tests.
    pub fn content(&self) -> &SettingsContent {
        &self.content
    }

    /// Complaints about the file as last read.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn favorites(&self) -> &[PathBuf] {
        &self.content.favorites
    }

    /// The chosen theme. What the picker reads and writes.
    pub fn theme_selection(&self) -> &ThemeSelection {
        &self.content.theme
    }

    /// Record the chosen theme. Returns whether anything changed.
    pub fn set_theme_selection(&mut self, selection: ThemeSelection) -> bool {
        if self.content.theme == selection {
            return false;
        }
        self.content.theme = selection;
        true
    }

    /// Plan §3's optional delete confirmation.
    pub fn confirm_delete_to_trash(&self) -> bool {
        self.content.confirm_delete_to_trash
    }

    pub fn set_confirm_delete_to_trash(&mut self, confirm: bool) -> bool {
        if self.content.confirm_delete_to_trash == confirm {
            return false;
        }
        self.content.confirm_delete_to_trash = confirm;
        true
    }

    /// Plan §3's "folders always grouped first".
    pub fn folders_first(&self) -> bool {
        self.content.folders_first
    }

    pub fn set_folders_first(&mut self, folders_first: bool) -> bool {
        if self.content.folders_first == folders_first {
            return false;
        }
        self.content.folders_first = folders_first;
        true
    }

    /// Convenience readers for call sites that only have `&App` — and which
    /// must keep working before the global is installed (a `#[gpui::test]`
    /// exercising one view does not boot the settings store).
    pub fn folders_first_or_default(cx: &App) -> bool {
        cx.try_global::<AppSettings>()
            .map(|settings| settings.content.folders_first)
            .unwrap_or_else(|| SettingsContent::default().folders_first)
    }

    pub fn confirm_delete_or_default(cx: &App) -> bool {
        cx.try_global::<AppSettings>()
            .map(|settings| settings.content.confirm_delete_to_trash)
            .unwrap_or_else(|| SettingsContent::default().confirm_delete_to_trash)
    }

    /// Append a favorite (deduplicated). Returns whether anything changed.
    pub fn add_favorite(&mut self, path: PathBuf) -> bool {
        if self.content.favorites.contains(&path) {
            return false;
        }
        self.content.favorites.push(path);
        true
    }

    /// Seed the default Favorites **once** (M7d-b): the folders Finder pins
    /// for a new user, filtered to the ones this machine actually has.
    ///
    /// Returns whether anything changed, so the caller knows to persist.
    ///
    /// The flag, not emptiness, is the guard. A user who unpins every favorite
    /// has expressed a preference, and re-seeding on the next launch would be
    /// the app overruling them — the M7d brief calls this out by name
    /// ("so emptying them stays empty"). Seeding also *appends*, so a user who
    /// already has favorites from an earlier version keeps them and their
    /// order.
    pub fn seed_favorites(&mut self, candidates: &[PathBuf]) -> bool {
        if self.content.favorites_seeded {
            return false;
        }
        self.content.favorites_seeded = true;
        for candidate in candidates {
            if !self.content.favorites.contains(candidate) {
                self.content.favorites.push(candidate.clone());
            }
        }
        // Always a change, even when every candidate was already pinned or
        // none of them exist: the *flag* moved, and it has to reach disk or a
        // machine with no default folders re-seeds on every launch forever.
        true
    }

    /// Whether the defaults have been seeded.
    pub fn favorites_seeded(&self) -> bool {
        self.content.favorites_seeded
    }

    /// Recently visited folders, most recent first.
    pub fn recents(&self) -> &[PathBuf] {
        &self.content.recents
    }

    /// Record a visit (M7d-b). Most-recent-first, deduplicated, capped at
    /// [`MAX_RECENTS`]. Returns whether the list changed, so a re-visit of the
    /// folder already at the top costs no write.
    ///
    /// Dedup is by exact path and moves the entry to the front rather than
    /// adding a second copy — a list where "Documents" appears four times is
    /// a worse list, not a more accurate one.
    pub fn push_recent(&mut self, path: PathBuf) -> bool {
        if self.content.recents.first() == Some(&path) {
            return false;
        }
        self.content.recents.retain(|p| p != &path);
        self.content.recents.insert(0, path);
        self.content.recents.truncate(MAX_RECENTS);
        true
    }

    /// Reorder the favorites (M3 drag-to-reorder, the gap M2 deferred): move
    /// `path` so it sits immediately **before** `before`, or to the end when
    /// `before` is `None`. Path-keyed on both sides like every other identity
    /// in the app (invariant #2) — a caller never passes an index, so a list
    /// that changed under the gesture can't silently reorder the wrong row.
    /// Returns whether the order actually changed.
    pub fn move_favorite(&mut self, path: &Path, before: Option<&Path>) -> bool {
        let Some(from) = self.content.favorites.iter().position(|p| p == path) else {
            return false;
        };
        let target = match before {
            Some(before) if before == path => return false, // onto itself
            Some(before) => match self.content.favorites.iter().position(|p| p == before) {
                Some(ix) => ix,
                None => return false,
            },
            None => self.content.favorites.len(),
        };
        // Removing `from` first shifts everything after it down one.
        let insert_at = if target > from { target - 1 } else { target };
        if insert_at == from {
            return false;
        }
        let moved = self.content.favorites.remove(from);
        self.content.favorites.insert(insert_at, moved);
        true
    }

    /// Remove a favorite. Returns whether anything changed.
    pub fn remove_favorite(&mut self, path: &Path) -> bool {
        let before = self.content.favorites.len();
        self.content.favorites.retain(|p| p != path);
        self.content.favorites.len() != before
    }

    /// The serialize-and-persist future. [`save`](Self::save) drives it on the
    /// background executor; tests await it directly for determinism.
    pub fn save_future(&self, vfs: Arc<dyn Vfs>) -> BoxFuture<'static, anyhow::Result<()>> {
        write_future(vfs, self.path.clone(), self.content.clone())
    }

    /// Persist the current settings in the background. Serialized against any
    /// write already in flight (see [`WriterState`]); a failure is logged.
    pub fn save(&self, cx: &App) {
        let fs = FsContext::global(cx);
        let vfs = fs.vfs.clone();
        let path = self.path.clone();
        let writer = self.writer.clone();

        let start = {
            let mut state = writer.lock().unwrap();
            state.pending = Some(self.content.clone());
            let start = !state.writing;
            state.writing = true;
            start
        };
        if !start {
            // A writer is already running; it will pick this content up.
            return;
        }

        fs.spawner.spawn(Box::pin(async move {
            loop {
                let next = {
                    let mut state = writer.lock().unwrap();
                    match state.pending.take() {
                        Some(content) => content,
                        None => {
                            state.writing = false;
                            return;
                        }
                    }
                };
                if let Err(error) = write_future(vfs.clone(), path.clone(), next).await {
                    eprintln!("settings: failed to save: {error:#}");
                }
            }
        }));
    }
}

/// Serialize **only what differs from the defaults** and write it atomically.
fn write_future(
    vfs: Arc<dyn Vfs>,
    path: PathBuf,
    content: SettingsContent,
) -> BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move {
        let json = serde_json::to_vec_pretty(&user_overrides(&content))?;
        vfs.atomic_write(&path, json).await
    })
}

/// The document to write: the keys whose value is not the compiled-in default.
/// Keeps the file small and readable, and lets a later change to a default
/// reach every user who never overrode it.
fn user_overrides(content: &SettingsContent) -> serde_json::Value {
    let defaults = SettingsContent::default();
    let mut map = serde_json::Map::new();
    if content.favorites != defaults.favorites {
        map.insert("favorites".into(), serde_json::json!(content.favorites));
    }
    if content.favorites_seeded != defaults.favorites_seeded {
        map.insert(
            "favorites_seeded".into(),
            serde_json::json!(content.favorites_seeded),
        );
    }
    if content.recents != defaults.recents {
        map.insert("recents".into(), serde_json::json!(content.recents));
    }
    if content.theme != defaults.theme {
        map.insert("theme".into(), serde_json::json!(content.theme));
    }
    if content.confirm_delete_to_trash != defaults.confirm_delete_to_trash {
        map.insert(
            "confirm_delete_to_trash".into(),
            serde_json::json!(content.confirm_delete_to_trash),
        );
    }
    if content.folders_first != defaults.folders_first {
        map.insert(
            "folders_first".into(),
            serde_json::json!(content.folders_first),
        );
    }
    serde_json::Value::Object(map)
}

/// Install the [`AppSettings`] global: defaults immediately (so readers never
/// find it missing), then the on-disk content swapped in from a background
/// load, and then the file is kept watched. Requires [`FsContext`] to be
/// initialized first.
pub fn init(cx: &mut App) {
    init_with_path(cx, AppSettings::default_path());
}

/// [`init`] with an injectable settings-file path (tests, visual scenarios).
pub fn init_with_path(cx: &mut App, path: PathBuf) {
    let vfs = FsContext::global(cx).vfs.clone();
    cx.set_global(AppSettings::new(path.clone()));
    let load_path = path.clone();
    let load_vfs = vfs.clone();
    cx.spawn(async move |cx| {
        let loaded = AppSettings::load(load_vfs, load_path).await;
        cx.update(|cx| {
            // Don't clobber changes made between boot and load completion.
            if AppSettings::global(cx).content == SettingsContent::default() {
                apply_loaded(loaded, cx);
            }
        });
    })
    .detach();
    start_watching(cx, path);
}

/// Swap in freshly-read content and let the rest of the app act on it. The
/// theme is the one setting with a live consumer outside the global itself;
/// everything else is read from the global at the point of use, and the
/// `observe_global` subscribers repaint on the swap.
///
/// The store's own machinery — the writer slot, the watch task and its guard —
/// belongs to the *store*, not to the content it is holding, so only the
/// content and its warnings are replaced here.
fn apply_loaded(loaded: AppSettings, cx: &mut App) {
    let AppSettings {
        content, warnings, ..
    } = loaded;
    let theme_changed = cx.update_global::<AppSettings, _>(|settings, _| {
        let theme_changed = settings.content.theme != content.theme;
        settings.content = content;
        settings.warnings = warnings;
        theme_changed
    });
    // **Only** when the file actually moved it. Pushing unconditionally would
    // mean every settings load overrides whatever the theme system was told
    // by anyone else — which is precisely what the visual-test runner does
    // when it installs a per-scenario theme while a settings load is still in
    // flight, and it would have rendered `workspace_light` in the dark theme.
    if theme_changed {
        let selection = AppSettings::global(cx).content.theme.clone();
        ActiveTheme::set_selection(selection, cx);
    }
}

/// Keep `settings.json` live. The **parent directory** is watched rather than
/// the file: an atomic write replaces the file, which some backends report as
/// a directory change rather than a change to the (now different) inode.
fn start_watching(cx: &mut App, path: PathBuf) {
    let vfs = FsContext::global(cx).vfs.clone();
    let executor = cx.background_executor().clone();
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let watch_vfs = vfs.clone();
    let task = cx.spawn(async move |cx| {
        let (mut stream, guard) = cx
            .background_spawn(async move { watch_vfs.watch(&dir, SETTINGS_WATCH_LATENCY) })
            .await;
        let guard = BackgroundWatchGuard::new(guard, executor);
        let stored = cx.update(|cx| {
            if !cx.has_global::<AppSettings>() {
                return false;
            }
            cx.update_global::<AppSettings, _>(|settings, _| settings._guard = Some(guard));
            true
        });
        if !stored {
            return;
        }
        while let Some(batch) = stream.next().await {
            if !batch.iter().any(|event| event.path.as_ref() == path) {
                continue;
            }
            let reloaded = cx
                .background_spawn(AppSettings::load(vfs.clone(), path.clone()))
                .await;
            cx.update(|cx| {
                if !cx.has_global::<AppSettings>() {
                    return;
                }
                // Our own `save` comes back through here; recognising it as a
                // no-op is what stops a write from causing a repaint storm.
                if AppSettings::global(cx).content == reloaded.content {
                    return;
                }
                apply_loaded(reloaded, cx);
            });
        }
    });
    cx.update_global::<AppSettings, _>(|settings, _| settings._watch = Some(task));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::theme::ThemeSelection;
    use fs_core::{FakeVfs, Spawner, StubPlatform, TestSpawner};
    use futures::executor::block_on;
    use gpui::TestAppContext;

    fn fake_vfs() -> Arc<FakeVfs> {
        FakeVfs::new(Arc::new(TestSpawner::new()))
    }

    #[test]
    fn the_chosen_theme_round_trips_and_old_files_still_load() {
        let vfs = fake_vfs();
        let path = PathBuf::from("/config/file-explorer/settings.json");

        let mut settings = AppSettings::new(path.clone());
        assert_eq!(
            settings.theme_selection(),
            &ThemeSelection::default(),
            "the compiled-in default until a choice is made"
        );
        assert!(settings.set_theme_selection(ThemeSelection::Static("Midnight".into())));
        assert!(
            !settings.set_theme_selection(ThemeSelection::Static("Midnight".into())),
            "same selection, no change"
        );
        block_on(settings.save_future(vfs.clone())).unwrap();

        let loaded = block_on(AppSettings::load(vfs.clone(), path.clone()));
        assert_eq!(
            loaded.theme_selection(),
            &ThemeSelection::Static("Midnight".into())
        );

        // The `appearance: system` form survives the same trip.
        let mut settings = loaded;
        assert!(settings.set_theme_selection(ThemeSelection::system()));
        block_on(settings.save_future(vfs.clone())).unwrap();
        let loaded = block_on(AppSettings::load(vfs.clone(), path.clone()));
        assert_eq!(loaded.theme_selection(), &ThemeSelection::system());

        // A settings.json written before M7 has no `theme` key at all; it must
        // keep loading, with the default theme.
        block_on(vfs.atomic_write(&path, br#"{ "favorites": ["/home/me"] }"#.to_vec())).unwrap();
        let old = block_on(AppSettings::load(vfs, path));
        assert_eq!(old.theme_selection(), &ThemeSelection::default());
        assert_eq!(old.favorites(), [PathBuf::from("/home/me")]);
    }

    #[test]
    fn favorites_round_trip_through_the_vfs() {
        let vfs = fake_vfs();
        let path = PathBuf::from("/config/file-explorer/settings.json");

        let mut settings = AppSettings::new(path.clone());
        assert!(settings.add_favorite(PathBuf::from("/home/me/Projects")));
        assert!(settings.add_favorite(PathBuf::from("/home/me/Downloads")));
        assert!(
            !settings.add_favorite(PathBuf::from("/home/me/Projects")),
            "duplicates are ignored"
        );
        block_on(settings.save_future(vfs.clone())).unwrap();

        let loaded = block_on(AppSettings::load(vfs.clone(), path.clone()));
        assert_eq!(
            loaded.favorites(),
            [
                PathBuf::from("/home/me/Projects"),
                PathBuf::from("/home/me/Downloads"),
            ],
            "favorites survive a save/load cycle in order"
        );

        // Remove one, save again, load again.
        let mut loaded = loaded;
        assert!(loaded.remove_favorite(Path::new("/home/me/Projects")));
        assert!(!loaded.remove_favorite(Path::new("/home/me/Projects")));
        block_on(loaded.save_future(vfs.clone())).unwrap();
        let reloaded = block_on(AppSettings::load(vfs, path));
        assert_eq!(reloaded.favorites(), [PathBuf::from("/home/me/Downloads")]);
    }

    #[test]
    fn move_favorite_inserts_before_the_target_or_at_the_end() {
        let mut settings = AppSettings::new(PathBuf::from("/config/settings.json"));
        for name in ["a", "b", "c"] {
            settings.add_favorite(PathBuf::from(format!("/{name}")));
        }
        let names = |settings: &AppSettings| {
            settings
                .favorites()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        };

        // Dropping "a" on "c" puts it immediately before "c".
        assert!(settings.move_favorite(Path::new("/a"), Some(Path::new("/c"))));
        assert_eq!(names(&settings), ["/b", "/a", "/c"]);

        // Dropping "c" on the first row moves it to the front.
        assert!(settings.move_favorite(Path::new("/c"), Some(Path::new("/b"))));
        assert_eq!(names(&settings), ["/c", "/b", "/a"]);

        // Dropping on the section (no row) sends it to the end.
        assert!(settings.move_favorite(Path::new("/c"), None));
        assert_eq!(names(&settings), ["/b", "/a", "/c"]);

        // No-ops report no change, so nothing is persisted for nothing:
        // onto itself, onto the row it already precedes, already last, and
        // paths that are not favorites at all.
        assert!(!settings.move_favorite(Path::new("/b"), Some(Path::new("/b"))));
        assert!(!settings.move_favorite(Path::new("/b"), Some(Path::new("/a"))));
        assert!(!settings.move_favorite(Path::new("/c"), None));
        assert!(!settings.move_favorite(Path::new("/nope"), None));
        assert!(!settings.move_favorite(Path::new("/b"), Some(Path::new("/nope"))));
        assert_eq!(names(&settings), ["/b", "/a", "/c"], "unchanged throughout");
    }

    #[test]
    fn missing_or_corrupt_settings_files_load_as_defaults() {
        let vfs = fake_vfs();
        let path = PathBuf::from("/config/settings.json");

        let missing = block_on(AppSettings::load(vfs.clone(), path.clone()));
        assert!(missing.favorites().is_empty());

        block_on(vfs.atomic_write(&path, b"{ not json !!".to_vec())).unwrap();
        let corrupt = block_on(AppSettings::load(vfs.clone(), path.clone()));
        assert!(
            corrupt.favorites().is_empty(),
            "corrupt file loads defaults"
        );

        // Unknown fields and a missing `favorites` key are tolerated.
        block_on(vfs.atomic_write(&path, br#"{"future_field": 42}"#.to_vec())).unwrap();
        let sparse = block_on(AppSettings::load(vfs, path));
        assert!(sparse.favorites().is_empty());
    }

    #[test]
    fn the_embedded_defaults_parse_and_are_the_documented_behavior() {
        let defaults = SettingsContent::default();
        assert!(defaults.favorites.is_empty());
        assert_eq!(
            defaults.theme,
            ThemeSelection::Static("Graphite Dark".into())
        );
        // Both §3 behaviors default to what the app already did before they
        // became settings, which is why M7b moves no baseline.
        assert!(!defaults.confirm_delete_to_trash);
        assert!(defaults.folders_first);
        assert_eq!(defaults, SettingsContent::default(), "parse is stable");
    }

    #[test]
    fn a_partial_file_keeps_every_default_it_does_not_mention() {
        let vfs = fake_vfs();
        let path = PathBuf::from("/config/settings.json");
        block_on(vfs.atomic_write(&path, br#"{ "folders_first": false }"#.to_vec())).unwrap();

        let loaded = block_on(AppSettings::load(vfs, path));
        assert!(!loaded.folders_first(), "the one key the file set");
        assert_eq!(loaded.theme_selection(), &ThemeSelection::default());
        assert!(!loaded.confirm_delete_to_trash());
        assert!(loaded.favorites().is_empty());
        assert!(loaded.warnings().is_empty());
    }

    #[test]
    fn unknown_and_malformed_keys_are_reported_rather_than_swallowed() {
        let vfs = fake_vfs();
        let path = PathBuf::from("/config/settings.json");

        block_on(vfs.atomic_write(
            &path,
            br#"{ "folders_first": false, "foldrs_first": true }"#.to_vec(),
        ))
        .unwrap();
        let typo = block_on(AppSettings::load(vfs.clone(), path.clone()));
        assert!(!typo.folders_first());
        assert_eq!(
            typo.warnings(),
            ["unknown setting `foldrs_first` ignored".to_string()]
        );

        // A structurally broken file loads the defaults *and says so*, rather
        // than looking like an empty settings file.
        block_on(vfs.atomic_write(&path, b"{ not json".to_vec())).unwrap();
        let broken = block_on(AppSettings::load(vfs, path));
        assert!(broken.folders_first(), "back to the default");
        assert_eq!(broken.warnings().len(), 1);
        assert!(broken.warnings()[0].contains("could not be read"));
    }

    /// The write path is a diff against the defaults, so a settings file only
    /// ever grows keys the user actually changed.
    #[test]
    fn only_overridden_keys_are_written() {
        let mut settings = AppSettings::new(PathBuf::from("/config/settings.json"));
        assert_eq!(user_overrides(settings.content()), serde_json::json!({}));

        settings.set_folders_first(false);
        assert_eq!(
            user_overrides(settings.content()),
            serde_json::json!({ "folders_first": false })
        );

        // Setting it back to the default removes it from the file again.
        settings.set_folders_first(true);
        assert_eq!(user_overrides(settings.content()), serde_json::json!({}));
    }

    /// Two saves in a row must not race two `atomic_write`s at one path: the
    /// second could land first and lose the newer content.
    #[gpui::test]
    async fn rapid_saves_are_serialized_and_the_last_one_wins(cx: &mut TestAppContext) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor.clone()));
        let vfs = FakeVfs::new(spawner.clone());
        let path = PathBuf::from("/config/settings.json");
        cx.update(|cx| {
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(StubPlatform::new()),
            );
            let mut settings = AppSettings::new(path.clone());
            for name in ["a", "b", "c"] {
                settings.add_favorite(PathBuf::from(format!("/{name}")));
                settings.save(cx);
            }
            cx.set_global(settings);
        });
        cx.background_executor.run_until_parked();

        let bytes = vfs.load(&path).await.expect("written");
        let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            written,
            serde_json::json!({ "favorites": ["/a", "/b", "/c"] }),
            "the newest content must be what is on disk"
        );
    }

    /// Editing `settings.json` in a text editor applies live — and the theme,
    /// the one setting with a consumer outside the global, follows it.
    #[gpui::test]
    async fn an_external_edit_is_picked_up_without_a_restart(cx: &mut TestAppContext) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor.clone()));
        let vfs = FakeVfs::new(spawner.clone());
        let path = PathBuf::from("/config/settings.json");
        vfs.insert_dir("/config");
        cx.update(|cx| {
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(StubPlatform::new()),
            );
            crate::theme::ActiveTheme::init_in(
                PathBuf::from("/config/themes"),
                ThemeSelection::default(),
                cx,
            );
            init_with_path(cx, path.clone());
        });
        cx.run_until_parked();
        cx.update(|cx| assert!(AppSettings::global(cx).folders_first()));

        vfs.write_file(
            &path,
            br#"{ "folders_first": false, "theme": "Graphite Light" }"#.to_vec(),
        );
        cx.executor()
            .advance_clock(SETTINGS_WATCH_LATENCY + Duration::from_millis(10));
        cx.run_until_parked();

        cx.update(|cx| {
            assert!(
                !AppSettings::global(cx).folders_first(),
                "the external edit did not reach the global"
            );
            assert_eq!(
                crate::theme::theme(cx).name,
                gpui::SharedString::from("Graphite Light"),
                "the theme named by the edited file is not painted"
            );
        });
    }

    /// A settings load that does not change the theme must leave the theme
    /// alone. The visual-test runner installs a per-scenario theme while the
    /// settings load is still in flight; before this rule, that load pushed
    /// the file's (default) theme over it and every light scenario captured
    /// the dark one.
    #[gpui::test]
    async fn a_settings_load_that_changes_no_theme_does_not_touch_the_theme(
        cx: &mut TestAppContext,
    ) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor.clone()));
        let vfs = FakeVfs::new(spawner.clone());
        let path = PathBuf::from("/config/settings.json");
        // A file with no `theme` key at all — the common case.
        vfs.insert_tree("/config", serde_json::json!({}));
        block_on(vfs.atomic_write(&path, br#"{ "favorites": ["/home/me"] }"#.to_vec())).unwrap();

        cx.update(|cx| {
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(StubPlatform::new()),
            );
            crate::theme::ActiveTheme::init_in(
                PathBuf::from("/config/themes"),
                ThemeSelection::default(),
                cx,
            );
            init_with_path(cx, path.clone());
            // Someone else picks a theme while the load is in flight — the
            // runner does exactly this.
            crate::theme::ActiveTheme::select("Graphite Light", cx);
        });
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                crate::theme::theme(cx).name,
                gpui::SharedString::from("Graphite Light"),
                "the settings load overrode a theme it had no opinion about"
            );
            assert_eq!(
                AppSettings::global(cx).favorites(),
                [PathBuf::from("/home/me")],
                "...but the rest of the file still loaded"
            );
        });
    }

    #[gpui::test]
    async fn save_persists_in_the_background_and_init_loads_at_boot(cx: &mut TestAppContext) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor.clone()));
        let vfs = FakeVfs::new(spawner.clone());
        let path = PathBuf::from("/config/file-explorer/settings.json");
        cx.update(|cx| {
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(StubPlatform::new()),
            );
        });

        // save() runs on the background executor, off the UI thread.
        let settings_path = path.clone();
        cx.update(|cx| {
            let mut settings = AppSettings::new(settings_path);
            settings.add_favorite(PathBuf::from("/home/me/Music"));
            settings.save(cx);
            cx.set_global(settings);
        });
        cx.background_executor.run_until_parked();
        let bytes = vfs.load(&path).await.expect("settings file was written");
        // Only what differs from the compiled-in defaults is written, so the
        // file is one key wide however many settings exist.
        let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            written,
            serde_json::json!({ "favorites": ["/home/me/Music"] }),
            "the file should carry the override and nothing else"
        );

        // A fresh boot's init_with_path swaps the persisted content in.
        cx.update(|cx| init_with_path(cx, path));
        cx.background_executor.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                AppSettings::global(cx).favorites(),
                [PathBuf::from("/home/me/Music")],
                "favorites survive restart"
            );
        });
    }

    // ------------------------------------------------------------------
    // M7d-b: seeded default Favorites, and Recents
    // ------------------------------------------------------------------

    fn settings() -> AppSettings {
        AppSettings::new(PathBuf::from("/config/settings.json"))
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn seeding_pins_the_defaults_on_a_fresh_profile() {
        let mut s = settings();
        assert!(!s.favorites_seeded());
        assert!(s.seed_favorites(&paths(&[
            "/Users/me/Desktop",
            "/Users/me/Documents",
            "/Users/me/Downloads",
        ])));
        assert_eq!(
            s.favorites(),
            paths(&[
                "/Users/me/Desktop",
                "/Users/me/Documents",
                "/Users/me/Downloads"
            ])
            .as_slice(),
            "seeded in the order given, which is the order they are shown"
        );
        assert!(s.favorites_seeded());
    }

    /// The point of the flag, and the M7d brief's actual words: "so emptying
    /// them stays empty". A user who unpins everything has expressed a
    /// preference; re-seeding on the next launch is the app overruling them.
    #[test]
    fn emptying_the_favorites_stays_empty_across_a_reseed() {
        let mut s = settings();
        s.seed_favorites(&paths(&["/Users/me/Desktop", "/Users/me/Documents"]));
        for path in paths(&["/Users/me/Desktop", "/Users/me/Documents"]) {
            s.remove_favorite(&path);
        }
        assert!(s.favorites().is_empty());

        // The next launch seeds again — and must change nothing.
        assert!(
            !s.seed_favorites(&paths(&["/Users/me/Desktop", "/Users/me/Documents"])),
            "a second seed is a no-op and must not ask to be persisted"
        );
        assert!(
            s.favorites().is_empty(),
            "the user emptied them deliberately; they stay empty"
        );
    }

    /// Seeding appends rather than replaces, so a profile that predates the
    /// defaults keeps what it had — and keeps it *first*.
    #[test]
    fn seeding_preserves_favorites_that_were_already_there() {
        let mut s = settings();
        s.add_favorite(PathBuf::from("/Users/me/Projects"));
        s.seed_favorites(&paths(&["/Users/me/Desktop", "/Users/me/Projects"]));
        assert_eq!(
            s.favorites(),
            paths(&["/Users/me/Projects", "/Users/me/Desktop"]).as_slice(),
            "the existing pin keeps its slot and is not duplicated"
        );
    }

    /// A machine with none of the default folders must not retry forever:
    /// the flag has to move (and be persisted) even when nothing was added.
    #[test]
    fn seeding_nothing_still_marks_the_profile_seeded() {
        let mut s = settings();
        assert!(
            s.seed_favorites(&[]),
            "the flag moved, so this must ask to be persisted"
        );
        assert!(s.favorites_seeded());
        assert!(!s.seed_favorites(&paths(&["/Users/me/Desktop"])));
        assert!(
            s.favorites().is_empty(),
            "the second call is a no-op, defaults or not"
        );
    }

    #[test]
    fn recents_are_most_recent_first_and_deduplicated() {
        let mut s = settings();
        assert!(s.push_recent(PathBuf::from("/a")));
        assert!(s.push_recent(PathBuf::from("/b")));
        assert!(s.push_recent(PathBuf::from("/c")));
        assert_eq!(s.recents(), paths(&["/c", "/b", "/a"]).as_slice());

        // Re-visiting moves it to the front rather than adding a second copy:
        // a list where one folder appears three times is a worse list.
        assert!(s.push_recent(PathBuf::from("/a")));
        assert_eq!(s.recents(), paths(&["/a", "/c", "/b"]).as_slice());
    }

    /// Navigating to the folder already on top is the commonest case there is
    /// (a refresh, a sort flip, a pane re-focus). It must not cost a write.
    #[test]
    fn revisiting_the_newest_recent_changes_nothing() {
        let mut s = settings();
        s.push_recent(PathBuf::from("/a"));
        assert!(
            !s.push_recent(PathBuf::from("/a")),
            "already at the front — no change, so no persist"
        );
        assert_eq!(s.recents(), paths(&["/a"]).as_slice());
    }

    #[test]
    fn recents_are_capped() {
        let mut s = settings();
        for i in 0..(MAX_RECENTS + 5) {
            s.push_recent(PathBuf::from(format!("/dir{i}")));
        }
        assert_eq!(s.recents().len(), MAX_RECENTS);
        assert_eq!(
            s.recents()[0],
            PathBuf::from(format!("/dir{}", MAX_RECENTS + 4)),
            "newest first"
        );
        assert!(
            !s.recents().contains(&PathBuf::from("/dir0")),
            "the oldest fell off the end"
        );
    }

    /// Both new keys are round-tripped through the override writer, and both
    /// stay out of the file while they hold their default.
    #[test]
    fn the_new_keys_are_written_only_when_they_differ() {
        let mut s = settings();
        assert_eq!(
            user_overrides(s.content()),
            serde_json::json!({}),
            "a pristine profile writes nothing"
        );
        s.seed_favorites(&paths(&["/Users/me/Desktop"]));
        s.push_recent(PathBuf::from("/Users/me/Desktop"));
        assert_eq!(
            user_overrides(s.content()),
            serde_json::json!({
                "favorites": ["/Users/me/Desktop"],
                "favorites_seeded": true,
                "recents": ["/Users/me/Desktop"],
            }),
        );
    }
}
