//! Settings persistence stub (ARCHITECTURE.md §1 `settings.rs` — "M7, stub M2").
//!
//! M2 surface: a small JSON file (`favorites: Vec<PathBuf>`) under the platform
//! config dir, loaded at boot and saved through [`Vfs::atomic_write`] on the
//! background executor — the UI thread never touches the disk. The file path is
//! injectable so tests point it at a `FakeVfs` location. M7 grows this into the
//! real settings store (embedded defaults, watch, keymap overrides).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::Vfs;
use futures::future::BoxFuture;
use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use crate::app_state::FsContext;

/// The persisted content — everything serde, everything optional-with-default
/// so old files keep loading as the schema grows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsContent {
    /// Sidebar favorites, in display order.
    #[serde(default)]
    pub favorites: Vec<PathBuf>,
}

/// App settings global (ARCHITECTURE.md §2 `AppSettings`). Mutate via
/// `cx.update_global::<AppSettings, _>(...)` and call [`AppSettings::save`]
/// afterwards to persist.
pub struct AppSettings {
    content: SettingsContent,
    path: PathBuf,
}

impl Global for AppSettings {}

impl AppSettings {
    /// Defaults, persisted at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            content: SettingsContent::default(),
            path,
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
    /// file yields defaults — settings must never block or fail boot.
    pub async fn load(vfs: Arc<dyn Vfs>, path: PathBuf) -> Self {
        let content = match vfs.load(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => SettingsContent::default(),
        };
        Self { content, path }
    }

    pub fn global(cx: &App) -> &AppSettings {
        cx.global::<AppSettings>()
    }

    pub fn favorites(&self) -> &[PathBuf] {
        &self.content.favorites
    }

    /// Append a favorite (deduplicated). Returns whether anything changed.
    pub fn add_favorite(&mut self, path: PathBuf) -> bool {
        if self.content.favorites.contains(&path) {
            return false;
        }
        self.content.favorites.push(path);
        true
    }

    /// Remove a favorite. Returns whether anything changed.
    pub fn remove_favorite(&mut self, path: &Path) -> bool {
        let before = self.content.favorites.len();
        self.content.favorites.retain(|p| p != path);
        self.content.favorites.len() != before
    }

    /// The serialize-and-persist future. [`save`](Self::save) spawns it on the
    /// background executor; tests await it directly for determinism.
    pub fn save_future(&self, vfs: Arc<dyn Vfs>) -> BoxFuture<'static, anyhow::Result<()>> {
        let content = self.content.clone();
        let path = self.path.clone();
        Box::pin(async move {
            let json = serde_json::to_vec_pretty(&content)?;
            vfs.atomic_write(&path, json).await
        })
    }

    /// Persist the current settings in the background (fire-and-forget; a
    /// failure is logged — M7 adds real error surfacing).
    pub fn save(&self, cx: &App) {
        let fs = FsContext::global(cx);
        let fut = self.save_future(fs.vfs.clone());
        fs.spawner.spawn(Box::pin(async move {
            if let Err(error) = fut.await {
                eprintln!("settings: failed to save: {error:#}");
            }
        }));
    }
}

/// Install the [`AppSettings`] global: defaults immediately (so readers never
/// find it missing), then the on-disk content swapped in from a background
/// load. Requires [`FsContext`] to be initialized first.
pub fn init(cx: &mut App) {
    init_with_path(cx, AppSettings::default_path());
}

/// [`init`] with an injectable settings-file path (tests, visual scenarios).
pub fn init_with_path(cx: &mut App, path: PathBuf) {
    let vfs = FsContext::global(cx).vfs.clone();
    cx.set_global(AppSettings::new(path.clone()));
    cx.spawn(async move |cx| {
        let loaded = AppSettings::load(vfs, path).await;
        cx.update(|cx| {
            // Don't clobber changes made between boot and load completion.
            if AppSettings::global(cx).content == SettingsContent::default() {
                cx.set_global(loaded);
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use fs_core::{FakeVfs, Spawner, StubPlatform, TestSpawner};
    use futures::executor::block_on;
    use gpui::TestAppContext;

    fn fake_vfs() -> Arc<FakeVfs> {
        FakeVfs::new(Arc::new(TestSpawner::new()))
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

    #[gpui::test]
    async fn save_persists_in_the_background_and_init_loads_at_boot(cx: &mut TestAppContext) {
        let spawner: Arc<dyn Spawner> = Arc::new(GpuiSpawner::new(cx.background_executor.clone()));
        let vfs = FakeVfs::new(spawner.clone());
        let path = PathBuf::from("/config/file-explorer/settings.json");
        cx.update(|cx| {
            cx.set_global(FsContext {
                vfs: vfs.clone(),
                spawner,
                opener: Arc::new(LoggingOpener),
                platform: Arc::new(StubPlatform::new()),
            });
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
        let content: SettingsContent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(content.favorites, [PathBuf::from("/home/me/Music")]);

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
}
