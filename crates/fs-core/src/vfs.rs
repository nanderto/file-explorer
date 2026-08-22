//! The filesystem seam (ARCHITECTURE.md §6, `vfs.rs`).
//!
//! [`Vfs`] starts with only the M1 (read-only browsing) methods and grows
//! additively in later milestones — no stubs. [`RealVfs`] wraps `std::fs`,
//! running every blocking call off-thread via [`SpawnerExt::unblock`].
//! [`FakeVfs`] (available under `cfg(test)` and the `test-support` feature) is
//! an in-memory tree built from `json!` fixtures with pausable/flushable
//! watcher events.

use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;

use crate::entry::{EntryKind, EntryMeta, FileEntry, TargetKind};
use crate::exec::{Spawner, SpawnerExt as _};
use crate::watcher::{self, PathEvent, WatchGuard};

/// Identifies the volume a path lives on. Used for job-lane routing (M3);
/// derived from the path shape in M1 (real volume enumeration is a platform
/// concern that lands at M2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VolumeKey(pub Arc<str>);

/// Derive a [`VolumeKey`] from a path alone: the drive prefix on Windows,
/// `/Volumes/<name>` on macOS-style paths, otherwise the root volume `/`.
pub fn volume_key_for(path: &Path) -> VolumeKey {
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix)) => {
            VolumeKey(prefix.as_os_str().to_string_lossy().into_owned().into())
        }
        Some(Component::RootDir) => {
            if let (Some(Component::Normal(volumes)), Some(Component::Normal(name))) =
                (components.next(), components.next())
                && volumes == "Volumes"
            {
                return VolumeKey(format!("/Volumes/{}", name.to_string_lossy()).into());
            }
            VolumeKey("/".into())
        }
        _ => VolumeKey("/".into()),
    }
}

/// The only door to the disk. Every implementation is safe to call from any
/// thread; `RealVfs` performs all blocking work on the background executor via
/// the [`Spawner`] it was constructed with.
#[async_trait]
pub trait Vfs: Send + Sync {
    /// Stream the entries of a directory (unsorted; hidden entries included —
    /// filtering and sorting happen in [`crate::listing::list_dir`]).
    async fn read_dir(&self, path: &Path) -> Result<BoxStream<'static, Result<FileEntry>>>;

    /// Stat a single path. A missing path is `Ok(None)`, not an error.
    async fn metadata(&self, path: &Path) -> Result<Option<EntryMeta>>;

    /// Read the entire contents of a file.
    async fn load(&self, path: &Path) -> Result<Vec<u8>>;

    /// Atomically replace `path` with `data`: write to a temp file **in the
    /// same directory**, sync it, then rename it over the destination — a
    /// crash or failure part-way leaves either the old contents or the new,
    /// never a truncated mix. Missing parent directories are created (settings
    /// write into a config dir that may not exist yet).
    async fn atomic_write(&self, path: &Path, data: Vec<u8>) -> Result<()>;

    /// The volume a path lives on (lane routing; status line grouping).
    fn volume_key(&self, path: &Path) -> VolumeKey;

    /// Free bytes on the volume containing `path` (status line).
    async fn free_space(&self, path: &Path) -> Result<u64>;

    /// Watch `path` for changes, delivering debounced batches after `latency`
    /// of quiet (driven by [`Spawner::timer`], so tests use fake time).
    /// Dropping the [`WatchGuard`] unregisters the watch and ends the stream.
    fn watch(
        &self,
        path: &Path,
        latency: Duration,
    ) -> (BoxStream<'static, Vec<PathEvent>>, WatchGuard);

    fn is_fake(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// RealVfs
// ---------------------------------------------------------------------------

/// [`Vfs`] over `std::fs`. All blocking calls run through
/// [`SpawnerExt::unblock`] — never on the caller's thread.
pub struct RealVfs {
    spawner: Arc<dyn Spawner>,
}

impl RealVfs {
    pub fn new(spawner: Arc<dyn Spawner>) -> Self {
        Self { spawner }
    }
}

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

fn entry_from_dirent(dirent: &std::fs::DirEntry) -> Result<FileEntry> {
    let path: Arc<Path> = Arc::from(dirent.path().as_path());
    let name: Arc<str> = dirent.file_name().to_string_lossy().into_owned().into();
    let metadata = dirent.metadata()?;
    let kind = kind_of(&metadata, &path);
    Ok(FileEntry {
        hidden: is_hidden_name(&name),
        name,
        kind,
        size: metadata.len(),
        modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
        created: metadata.created().ok(),
        path,
    })
}

fn kind_of(metadata: &std::fs::Metadata, path: &Path) -> EntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target_kind = match std::fs::metadata(path) {
            Ok(target) if target.is_dir() => TargetKind::Dir,
            Ok(_) => TargetKind::File,
            Err(_) => TargetKind::Unknown,
        };
        EntryKind::Symlink { target_kind }
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

#[async_trait]
impl Vfs for RealVfs {
    async fn read_dir(&self, path: &Path) -> Result<BoxStream<'static, Result<FileEntry>>> {
        let open_path = path.to_path_buf();
        let iter = self
            .spawner
            .unblock(move || std::fs::read_dir(open_path))
            .await?;
        let (tx, rx) = async_channel::bounded(128);
        let pump = self.spawner.unblock(move || {
            for dirent in iter {
                let item = dirent
                    .map_err(anyhow::Error::from)
                    .and_then(|d| entry_from_dirent(&d));
                if tx.send_blocking(item).is_err() {
                    break; // consumer dropped the stream
                }
            }
        });
        self.spawner.spawn(Box::pin(async move {
            pump.await;
        }));
        Ok(rx.boxed())
    }

    async fn metadata(&self, path: &Path) -> Result<Option<EntryMeta>> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    Ok(Some(EntryMeta {
                        kind: kind_of(&metadata, &path),
                        size: metadata.len(),
                        modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                        created: metadata.created().ok(),
                        hidden: is_hidden_name(&name),
                    }))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            })
            .await
    }

    async fn load(&self, path: &Path) -> Result<Vec<u8>> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || Ok(std::fs::read(&path)?))
            .await
    }

    async fn atomic_write(&self, path: &Path, data: Vec<u8>) -> Result<()> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || {
                use std::io::Write as _;
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("atomic_write: {} has no parent directory", path.display())
                    })?;
                std::fs::create_dir_all(parent)?;
                // Temp file in the destination's own directory so the final
                // rename stays on one filesystem (that is what makes it atomic).
                let mut temp = tempfile::NamedTempFile::new_in(parent)?;
                temp.write_all(&data)?;
                temp.as_file().sync_all()?;
                temp.persist(&path)
                    .map_err(|persist_error| anyhow::Error::from(persist_error.error))?;
                Ok(())
            })
            .await
    }

    fn volume_key(&self, path: &Path) -> VolumeKey {
        volume_key_for(path)
    }

    async fn free_space(&self, path: &Path) -> Result<u64> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || Ok(fs4::available_space(&path)?))
            .await
    }

    fn watch(
        &self,
        path: &Path,
        latency: Duration,
    ) -> (BoxStream<'static, Vec<PathEvent>>, WatchGuard) {
        watcher::watch_real(&self.spawner, path, latency)
    }
}

// ---------------------------------------------------------------------------
// FakeVfs (test-support)
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeVfs;

#[cfg(any(test, feature = "test-support"))]
mod fake {
    use super::*;
    use crate::watcher::PathEventKind;
    use anyhow::anyhow;
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::SystemTime;

    #[derive(Clone, Debug)]
    struct FakeNode {
        kind: EntryKind,
        size: u64,
        modified: SystemTime,
        contents: Vec<u8>,
    }

    struct FakeWatchSub {
        id: u64,
        root: PathBuf,
        tx: async_channel::Sender<PathEvent>,
    }

    #[derive(Default)]
    struct FakeState {
        tree: BTreeMap<PathBuf, FakeNode>,
        errors: HashMap<PathBuf, String>,
        free_space: u64,
        paused: bool,
        buffered: Vec<PathEvent>,
        watchers: Vec<FakeWatchSub>,
        next_watch_id: u64,
        next_mtime: u64,
    }

    /// In-memory [`Vfs`] for tests: trees built from `serde_json::json!`
    /// fixtures (objects are directories, strings are file contents), explicit
    /// event injection with pause/flush, and per-path error injection.
    pub struct FakeVfs {
        spawner: Arc<dyn Spawner>,
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeVfs {
        pub fn new(spawner: Arc<dyn Spawner>) -> Arc<Self> {
            Arc::new(Self {
                spawner,
                state: Arc::new(Mutex::new(FakeState {
                    free_space: 42 * 1024 * 1024 * 1024,
                    ..FakeState::default()
                })),
            })
        }

        /// Build a subtree at `root` from a `json!` fixture: objects become
        /// directories, strings become files whose size is the string length.
        pub fn insert_tree(&self, root: impl AsRef<Path>, tree: serde_json::Value) {
            let mut state = self.state.lock().unwrap();
            insert_tree_locked(&mut state, root.as_ref(), &tree);
        }

        /// Add (or overwrite) a file node and emit `Created`/`Changed`.
        pub fn insert_file(&self, path: impl AsRef<Path>, size: u64) {
            let path = path.as_ref().to_path_buf();
            let mut state = self.state.lock().unwrap();
            let existed = state.tree.contains_key(&path);
            let modified = next_mtime(&mut state);
            state.tree.insert(
                path.clone(),
                FakeNode {
                    kind: EntryKind::File,
                    size,
                    modified,
                    contents: vec![0; size as usize],
                },
            );
            let kind = if existed {
                PathEventKind::Changed
            } else {
                PathEventKind::Created
            };
            emit_locked(&mut state, path_event(&path, kind));
        }

        /// Add a directory node and emit `Created`.
        pub fn insert_dir(&self, path: impl AsRef<Path>) {
            let path = path.as_ref().to_path_buf();
            let mut state = self.state.lock().unwrap();
            let modified = next_mtime(&mut state);
            state.tree.insert(
                path.clone(),
                FakeNode {
                    kind: EntryKind::Dir,
                    size: 0,
                    modified,
                    contents: Vec::new(),
                },
            );
            emit_locked(&mut state, path_event(&path, PathEventKind::Created));
        }

        /// Remove a node (and any descendants) and emit `Removed`.
        pub fn remove_path(&self, path: impl AsRef<Path>) {
            let path = path.as_ref().to_path_buf();
            let mut state = self.state.lock().unwrap();
            state.tree.retain(|p, _| !p.starts_with(&path));
            emit_locked(&mut state, path_event(&path, PathEventKind::Removed));
        }

        /// Inject a raw watcher event without touching the tree.
        pub fn emit_event(&self, event: PathEvent) {
            let mut state = self.state.lock().unwrap();
            emit_locked(&mut state, event);
        }

        /// Buffer subsequent events instead of delivering them.
        pub fn pause_events(&self) {
            self.state.lock().unwrap().paused = true;
        }

        /// Deliver everything buffered while paused, and resume delivery.
        pub fn flush_events(&self) {
            let mut state = self.state.lock().unwrap();
            state.paused = false;
            let buffered = std::mem::take(&mut state.buffered);
            for event in buffered {
                route_locked(&state, event);
            }
        }

        /// Make `read_dir`/`metadata` on `path` fail with `message`.
        pub fn set_error(&self, path: impl AsRef<Path>, message: &str) {
            self.state
                .lock()
                .unwrap()
                .errors
                .insert(path.as_ref().to_path_buf(), message.to_string());
        }

        /// Configure the value returned by [`Vfs::free_space`].
        pub fn set_free_space(&self, bytes: u64) {
            self.state.lock().unwrap().free_space = bytes;
        }

        /// Number of live watch registrations (guard-drop assertions).
        pub fn watcher_count(&self) -> usize {
            self.state.lock().unwrap().watchers.len()
        }
    }

    fn path_event(path: &Path, kind: PathEventKind) -> PathEvent {
        PathEvent {
            path: Arc::from(path),
            kind,
        }
    }

    fn next_mtime(state: &mut FakeState) -> SystemTime {
        state.next_mtime += 1;
        std::time::UNIX_EPOCH + Duration::from_secs(state.next_mtime)
    }

    fn insert_tree_locked(state: &mut FakeState, root: &Path, tree: &serde_json::Value) {
        let modified = next_mtime(state);
        match tree {
            serde_json::Value::Object(children) => {
                state.tree.insert(
                    root.to_path_buf(),
                    FakeNode {
                        kind: EntryKind::Dir,
                        size: 0,
                        modified,
                        contents: Vec::new(),
                    },
                );
                for (name, child) in children {
                    insert_tree_locked(state, &root.join(name), child);
                }
            }
            serde_json::Value::String(contents) => {
                state.tree.insert(
                    root.to_path_buf(),
                    FakeNode {
                        kind: EntryKind::File,
                        size: contents.len() as u64,
                        modified,
                        contents: contents.as_bytes().to_vec(),
                    },
                );
            }
            other => panic!("FakeVfs::insert_tree: unsupported fixture value {other:?}"),
        }
    }

    fn emit_locked(state: &mut FakeState, event: PathEvent) {
        if state.paused {
            state.buffered.push(event);
        } else {
            route_locked(state, event);
        }
    }

    fn route_locked(state: &FakeState, event: PathEvent) {
        for sub in &state.watchers {
            if event.kind == PathEventKind::Rescan || event.path.starts_with(&sub.root) {
                let _ = sub.tx.try_send(event.clone());
            }
        }
    }

    fn node_meta(path: &Path, node: &FakeNode) -> EntryMeta {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        EntryMeta {
            kind: node.kind,
            size: node.size,
            modified: node.modified,
            created: None,
            hidden: is_hidden_name(&name),
        }
    }

    #[async_trait]
    impl Vfs for FakeVfs {
        async fn read_dir(&self, path: &Path) -> Result<BoxStream<'static, Result<FileEntry>>> {
            let state = self.state.lock().unwrap();
            if let Some(message) = state.errors.get(path) {
                return Err(anyhow!("{message}"));
            }
            let dir = state
                .tree
                .get(path)
                .ok_or_else(|| anyhow!("no such directory: {}", path.display()))?;
            if !matches!(dir.kind, EntryKind::Dir) {
                return Err(anyhow!("not a directory: {}", path.display()));
            }
            let entries: Vec<Result<FileEntry>> = state
                .tree
                .iter()
                .filter(|(p, _)| p.parent() == Some(path))
                .map(|(p, node)| Ok(node_meta(p, node).into_entry(Arc::from(p.as_path()))))
                .collect();
            Ok(futures::stream::iter(entries).boxed())
        }

        async fn metadata(&self, path: &Path) -> Result<Option<EntryMeta>> {
            let state = self.state.lock().unwrap();
            if let Some(message) = state.errors.get(path) {
                return Err(anyhow!("{message}"));
            }
            Ok(state.tree.get(path).map(|node| node_meta(path, node)))
        }

        async fn load(&self, path: &Path) -> Result<Vec<u8>> {
            let state = self.state.lock().unwrap();
            if let Some(message) = state.errors.get(path) {
                return Err(anyhow!("{message}"));
            }
            let node = state
                .tree
                .get(path)
                .ok_or_else(|| anyhow!("no such file: {}", path.display()))?;
            if !matches!(node.kind, EntryKind::File) {
                return Err(anyhow!("not a file: {}", path.display()));
            }
            Ok(node.contents.clone())
        }

        async fn atomic_write(&self, path: &Path, data: Vec<u8>) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = state.errors.get(path) {
                return Err(anyhow!("{message}"));
            }
            if let Some(existing) = state.tree.get(path)
                && !matches!(existing.kind, EntryKind::File)
            {
                return Err(anyhow!("not a file: {}", path.display()));
            }
            // Create missing ancestors, mirroring RealVfs::atomic_write's
            // create_dir_all — but fail like it does when an ancestor is a file.
            let mut missing: Vec<PathBuf> = Vec::new();
            let mut cursor = path.parent();
            while let Some(dir) = cursor {
                if dir.as_os_str().is_empty() {
                    break;
                }
                if let Some(node) = state.tree.get(dir) {
                    if !matches!(node.kind, EntryKind::Dir) {
                        return Err(anyhow!("not a directory: {}", dir.display()));
                    }
                    break;
                }
                missing.push(dir.to_path_buf());
                cursor = dir.parent();
            }
            for dir in missing.into_iter().rev() {
                let modified = next_mtime(&mut state);
                state.tree.insert(
                    dir.clone(),
                    FakeNode {
                        kind: EntryKind::Dir,
                        size: 0,
                        modified,
                        contents: Vec::new(),
                    },
                );
                emit_locked(&mut state, path_event(&dir, PathEventKind::Created));
            }
            let existed = state.tree.contains_key(path);
            let modified = next_mtime(&mut state);
            state.tree.insert(
                path.to_path_buf(),
                FakeNode {
                    kind: EntryKind::File,
                    size: data.len() as u64,
                    modified,
                    contents: data,
                },
            );
            let kind = if existed {
                PathEventKind::Changed
            } else {
                PathEventKind::Created
            };
            emit_locked(&mut state, path_event(path, kind));
            Ok(())
        }

        fn volume_key(&self, path: &Path) -> VolumeKey {
            volume_key_for(path)
        }

        async fn free_space(&self, _path: &Path) -> Result<u64> {
            Ok(self.state.lock().unwrap().free_space)
        }

        fn watch(
            &self,
            path: &Path,
            latency: Duration,
        ) -> (BoxStream<'static, Vec<PathEvent>>, WatchGuard) {
            let (tx, rx) = async_channel::unbounded();
            let id = {
                let mut state = self.state.lock().unwrap();
                let id = state.next_watch_id;
                state.next_watch_id += 1;
                state.watchers.push(FakeWatchSub {
                    id,
                    root: path.to_path_buf(),
                    tx,
                });
                id
            };
            let state = self.state.clone();
            let guard = WatchGuard::new(move || {
                state.lock().unwrap().watchers.retain(|w| w.id != id);
            });
            (watcher::debounce(&self.spawner, rx, latency), guard)
        }

        fn is_fake(&self) -> bool {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::TestSpawner;
    use futures::executor::block_on;
    use serde_json::json;

    fn test_vfs() -> (Arc<TestSpawner>, Arc<FakeVfs>) {
        let spawner = Arc::new(TestSpawner::new());
        let vfs = FakeVfs::new(spawner.clone() as Arc<dyn Spawner>);
        (spawner, vfs)
    }

    #[test]
    fn volume_key_for_windows_and_unix_shapes() {
        #[cfg(windows)]
        assert_eq!(
            volume_key_for(Path::new("C:\\Users\\me")),
            VolumeKey("C:".into())
        );
        assert_eq!(
            volume_key_for(Path::new("/usr/local")),
            VolumeKey("/".into())
        );
        assert_eq!(
            volume_key_for(Path::new("/Volumes/Backup/photos")),
            VolumeKey("/Volumes/Backup".into())
        );
        assert_eq!(
            volume_key_for(Path::new("relative/path")),
            VolumeKey("/".into())
        );
    }

    #[test]
    fn fake_vfs_reads_json_tree() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree(
            "/root",
            json!({
                "docs": { "a.txt": "aaa" },
                "b.bin": "12345",
                ".hidden": "x",
            }),
        );
        let stream = block_on(vfs.read_dir(Path::new("/root"))).unwrap();
        let mut entries: Vec<FileEntry> = block_on(stream.map(|r| r.unwrap()).collect());
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = entries.iter().map(|e| &*e.name).collect();
        assert_eq!(names, [".hidden", "b.bin", "docs"]);
        let b = entries.iter().find(|e| &*e.name == "b.bin").unwrap();
        assert_eq!(b.size, 5);
        assert_eq!(b.kind, EntryKind::File);
        let docs = entries.iter().find(|e| &*e.name == "docs").unwrap();
        assert_eq!(docs.kind, EntryKind::Dir);
        let hidden = entries.iter().find(|e| &*e.name == ".hidden").unwrap();
        assert!(hidden.hidden);
    }

    #[test]
    fn fake_vfs_metadata_missing_is_none_and_errors_are_injectable() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree("/root", json!({ "a.txt": "abc" }));

        let meta = block_on(vfs.metadata(Path::new("/root/a.txt"))).unwrap();
        assert_eq!(meta.map(|m| m.size), Some(3));

        let missing = block_on(vfs.metadata(Path::new("/root/nope.txt"))).unwrap();
        assert!(missing.is_none());

        vfs.set_error("/root/a.txt", "disk on fire");
        let err = block_on(vfs.metadata(Path::new("/root/a.txt"))).unwrap_err();
        assert!(err.to_string().contains("disk on fire"));

        vfs.set_error("/root", "unreadable");
        assert!(block_on(vfs.read_dir(Path::new("/root"))).is_err());
    }

    #[test]
    fn fake_vfs_free_space_is_configurable() {
        let (_spawner, vfs) = test_vfs();
        vfs.set_free_space(1234);
        assert_eq!(block_on(vfs.free_space(Path::new("/"))).unwrap(), 1234);
        assert!(vfs.is_fake());
    }

    #[test]
    fn fake_vfs_load_and_atomic_write_round_trip() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree("/root", json!({ "a.txt": "abc" }));

        assert_eq!(
            block_on(vfs.load(Path::new("/root/a.txt"))).unwrap(),
            b"abc"
        );
        assert!(block_on(vfs.load(Path::new("/root/missing"))).is_err());
        assert!(
            block_on(vfs.load(Path::new("/root"))).is_err(),
            "loading a directory is an error"
        );

        // New file, with missing parents created (mirrors RealVfs).
        block_on(vfs.atomic_write(Path::new("/root/cfg/settings.json"), b"{}".to_vec())).unwrap();
        assert_eq!(
            block_on(vfs.load(Path::new("/root/cfg/settings.json"))).unwrap(),
            b"{}"
        );
        let parent = block_on(vfs.metadata(Path::new("/root/cfg")))
            .unwrap()
            .unwrap();
        assert_eq!(parent.kind, EntryKind::Dir);

        // Overwrite replaces contents (and size) wholesale.
        block_on(vfs.atomic_write(Path::new("/root/cfg/settings.json"), b"[1,2]".to_vec()))
            .unwrap();
        assert_eq!(
            block_on(vfs.load(Path::new("/root/cfg/settings.json"))).unwrap(),
            b"[1,2]"
        );

        // Writing "under" a file fails like create_dir_all does.
        assert!(block_on(vfs.atomic_write(Path::new("/root/a.txt/child"), b"x".to_vec())).is_err());
        // Error injection applies to atomic_write too.
        vfs.set_error("/root/locked.json", "no space");
        let err =
            block_on(vfs.atomic_write(Path::new("/root/locked.json"), b"x".to_vec())).unwrap_err();
        assert!(err.to_string().contains("no space"));
    }

    #[test]
    fn real_vfs_atomic_write_round_trips_and_cleans_up_temp_files() {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = RealVfs::new(spawner);
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings.json");

        block_on(vfs.atomic_write(&target, b"first".to_vec())).unwrap();
        assert_eq!(block_on(vfs.load(&target)).unwrap(), b"first");

        block_on(vfs.atomic_write(&target, b"second, longer".to_vec())).unwrap();
        assert_eq!(block_on(vfs.load(&target)).unwrap(), b"second, longer");

        // The temp file was renamed away, not left behind: the directory holds
        // exactly the destination.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp files left: {leftovers:?}");

        // Missing parent directories are created.
        let nested = dir.path().join("deep/nested/settings.json");
        block_on(vfs.atomic_write(&nested, b"nested".to_vec())).unwrap();
        assert_eq!(block_on(vfs.load(&nested)).unwrap(), b"nested");
    }

    #[test]
    fn real_vfs_atomic_write_failure_never_corrupts_the_destination() {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = RealVfs::new(spawner);
        let dir = tempfile::tempdir().unwrap();

        // A write whose "parent directory" is actually a file fails before the
        // destination is ever touched (temp-then-rename: no partial states).
        let occupied = dir.path().join("occupied");
        std::fs::write(&occupied, b"precious").unwrap();
        let bad_target = occupied.join("settings.json");
        assert!(block_on(vfs.atomic_write(&bad_target, b"new".to_vec())).is_err());
        assert_eq!(
            std::fs::read(&occupied).unwrap(),
            b"precious",
            "existing data is untouched by a failed atomic_write"
        );
        assert!(block_on(vfs.load(&Path::new("/").join("nonexistent-file"))).is_err());
    }

    #[test]
    fn real_vfs_lists_stats_and_reports_free_space() {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = RealVfs::new(spawner);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();

        let stream = block_on(vfs.read_dir(dir.path())).unwrap();
        let mut entries: Vec<FileEntry> = block_on(stream.map(|r| r.unwrap()).collect());
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = entries.iter().map(|e| &*e.name).collect();
        assert_eq!(names, ["a.txt", "b.txt", "sub"]);
        let b = entries.iter().find(|e| &*e.name == "b.txt").unwrap();
        assert_eq!(b.size, 5);
        assert_eq!(b.kind, EntryKind::File);
        let sub = entries.iter().find(|e| &*e.name == "sub").unwrap();
        assert_eq!(sub.kind, EntryKind::Dir);

        let meta = block_on(vfs.metadata(&dir.path().join("a.txt"))).unwrap();
        assert_eq!(meta.map(|m| m.size), Some(1));
        let missing = block_on(vfs.metadata(&dir.path().join("missing"))).unwrap();
        assert!(missing.is_none());

        let free = block_on(vfs.free_space(dir.path())).unwrap();
        assert!(free > 0);

        let missing_dir = block_on(vfs.read_dir(&dir.path().join("nope")));
        assert!(
            missing_dir.is_err(),
            "missing directory is a read_dir error"
        );
    }
}
