//! The filesystem seam (ARCHITECTURE.md §6, `vfs.rs`).
//!
//! [`Vfs`] starts with only the M1 (read-only browsing) methods and grows
//! additively in later milestones — no stubs. [`RealVfs`] wraps `std::fs`,
//! running every blocking call off-thread via [`SpawnerExt::unblock`].
//! [`FakeVfs`] (available under `cfg(test)` and the `test-support` feature) is
//! an in-memory tree built from `json!` fixtures with pausable/flushable
//! watcher events.

use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::BoxStream;

use crate::entry::{EntryKind, EntryMeta, FileEntry, TargetKind};
use crate::exec::{Spawner, SpawnerExt as _};
use crate::platform::trash as trash_engine;
use crate::watcher::{self, PathEvent, WatchGuard};

/// The permission bits [`Vfs::mode`] reports and [`Vfs::set_mode`] writes:
/// `rwxrwxrwx` plus setuid/setgid/sticky, the same window
/// [`crate::UnixPerms`] keeps. The file-type bits are never part of a mode
/// *value* — `chmod` cannot change them.
pub const PERM_BITS: u32 = 0o7777;

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

/// Options for [`Vfs::create_file`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreateOptions {
    /// Replace an existing file instead of failing.
    pub overwrite: bool,
}

/// Options for [`Vfs::rename`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenameOptions {
    /// Replace an existing destination instead of failing.
    pub overwrite: bool,
}

/// Options for [`Vfs::remove`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoveOptions {
    /// Remove non-empty directories (and their contents).
    pub recursive: bool,
}

/// Progress callback for [`Vfs::copy`]: `(bytes_done, bytes_total)` — invoked
/// before the first chunk and after every chunk. Returning `false` aborts the
/// copy between chunks (the M3 cancellation point *inside* a file): the
/// partial destination is removed and `copy` fails with [`CopyCancelled`].
pub type ProgressFn = Arc<dyn Fn(u64, u64) -> bool + Send + Sync>;

/// Marker error returned by [`Vfs::copy`] when the [`ProgressFn`] aborted the
/// copy. Downcast with `error.is::<CopyCancelled>()` to tell cancellation from
/// real I/O failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyCancelled;

impl fmt::Display for CopyCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "copy cancelled by progress callback")
    }
}

impl std::error::Error for CopyCancelled {}

/// Undo token returned by [`Vfs::trash`] and consumed by [`Vfs::restore`]
/// (ARCHITECTURE.md §6: `trash(path) -> Result<TrashId>`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrashId {
    /// Where the item lived before it was trashed — the restore target.
    pub original: PathBuf,
    /// Where the trashed payload lives now: a `.fake-trash` entry on the
    /// portable scheme, or the real trash URL's path on macOS.
    pub trashed: PathBuf,
}

/// Typed restore failures (ARCHITECTURE.md §6) — each variant has distinct UX
/// (toast / conflict-style prompt / silent no-op) and is directly assertable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrashRestoreError {
    /// Trash item gone (emptied externally).
    NotFound,
    /// Original path now occupied.
    Collision(PathBuf),
    /// Token consumed (double-undo race).
    AlreadyRestored,
}

impl fmt::Display for TrashRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "item is no longer in the trash"),
            Self::Collision(path) => {
                write!(f, "original location is occupied: {}", path.display())
            }
            Self::AlreadyRestored => write!(f, "item was already restored"),
        }
    }
}

impl std::error::Error for TrashRestoreError {}

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

    /// Create a directory with `create_dir_all` semantics: missing ancestors
    /// are created, an existing directory succeeds (folder merges replay it),
    /// a file in the way fails.
    async fn create_dir(&self, path: &Path) -> Result<()>;

    /// Create an empty file. Fails if `path` exists unless
    /// [`CreateOptions::overwrite`] is set; the parent directory must exist.
    async fn create_file(&self, path: &Path, opts: CreateOptions) -> Result<()>;

    /// Copy one file's bytes from `from` to `to`. Directories are expanded
    /// into per-file copies by op planning ([`crate::ops`]), never here.
    /// Cancellation-safe: when `on_progress` returns `false` the copy aborts
    /// between chunks, the partial destination is removed, and the call fails
    /// with [`CopyCancelled`] — no partial file survives.
    async fn copy(&self, from: &Path, to: &Path, on_progress: ProgressFn) -> Result<()>;

    /// Move `from` to `to` (file or whole subtree; same-volume rename, so
    /// mtimes are preserved). Fails if `to` exists unless
    /// [`RenameOptions::overwrite`] is set.
    async fn rename(&self, from: &Path, to: &Path, opts: RenameOptions) -> Result<()>;

    /// Permanently remove a file or directory. A non-empty directory requires
    /// [`RemoveOptions::recursive`]; a missing path is an error (callers know
    /// what they expect to delete).
    async fn remove(&self, path: &Path, opts: RemoveOptions) -> Result<()>;

    /// The item's unix permission bits (`st_mode & 0o7777`), or `Ok(None)`
    /// where the platform has no unix mode at all (Windows). A missing or
    /// unreadable path is an `Err` — unlike [`metadata`], because the only
    /// caller is [`crate::FileOp::Chmod`], which must capture the *previous*
    /// mode before it writes and cannot treat "gone" as "no mode".
    ///
    /// **Follows symlinks**, and so does [`set_mode`]: the pair has to describe
    /// the same inode or an undo would write a link's mode onto its target.
    /// (Note the divergence from [`crate::Platform::file_attrs`], which
    /// `lstat`s because the info panel describes the item the user clicked.)
    ///
    /// Defaulted to `Ok(None)` so a test double that does not model permissions
    /// keeps compiling; every implementation that can answer overrides it.
    ///
    /// [`metadata`]: Vfs::metadata
    /// [`set_mode`]: Vfs::set_mode
    async fn mode(&self, _path: &Path) -> Result<Option<u32>> {
        Ok(None)
    }

    /// Set the item's unix permission bits (`chmod`; `mode` is masked to
    /// `0o7777`). Follows symlinks — see [`mode`].
    ///
    /// Note for undo: `chmod` changes **ctime, not mtime**, so an
    /// mtime-fingerprint cannot see it — [`crate::UndoEntry`] guards these ops
    /// with [`crate::AttrGuard`] instead.
    ///
    /// Defaulted to an error rather than a silent success: a `Vfs` that cannot
    /// chmod must say so, never pretend.
    ///
    /// [`mode`]: Vfs::mode
    async fn set_mode(&self, path: &Path, _mode: u32) -> Result<()> {
        anyhow::bail!(
            "this filesystem cannot change unix permissions: {}",
            path.display()
        )
    }

    /// Move `path` to the trash, returning the undo token [`restore`]
    /// consumes. The real macOS trash sits behind `cfg(target_os = "macos")`;
    /// everywhere else (and in `FakeVfs`) a `.fake-trash` directory holds
    /// restorable subtrees so trash→restore and undo-of-delete run as tests on
    /// Windows CI (ARCHITECTURE.md §6/§9).
    ///
    /// [`restore`]: Vfs::restore
    async fn trash(&self, path: &Path) -> Result<TrashId>;

    /// Put a trashed item back where it came from, returning the restored
    /// path. Failures are typed ([`TrashRestoreError`]), not stringly.
    async fn restore(&self, id: TrashId) -> Result<PathBuf, TrashRestoreError>;

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
    /// Tokens already restored this session — the `AlreadyRestored`
    /// double-undo guard (an in-memory set suffices: the race it guards is
    /// two undos of the same entry within one run).
    consumed_trash: Mutex<HashSet<TrashId>>,
}

impl RealVfs {
    pub fn new(spawner: Arc<dyn Spawner>) -> Self {
        Self {
            spawner,
            consumed_trash: Mutex::new(HashSet::new()),
        }
    }
}

/// Chunk size for [`RealVfs`]'s copy loop — progress granularity and the
/// between-chunks cancellation interval.
const COPY_CHUNK_BYTES: usize = 1024 * 1024;

/// Blocking single-file copy loop. Runs inside `unblock`. Once the
/// destination has been created, any failure (cancel or I/O error) removes it
/// — no partial file survives; failures *before* that point (missing source,
/// directory source, pre-copy cancel) never touch the destination.
fn copy_file_blocking(from: &Path, to: &Path, on_progress: &ProgressFn) -> Result<()> {
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        anyhow::bail!(
            "copy: {} is a directory (directories are expanded by op planning)",
            from.display()
        );
    }
    let total = metadata.len();
    if !on_progress(0, total) {
        return Err(CopyCancelled.into());
    }
    let mut reader = std::fs::File::open(from)?;
    let writer = std::fs::File::create(to)?;
    let result = copy_chunks_blocking(&mut reader, writer, total, on_progress);
    if result.is_err() {
        // Cancelled or failed mid-write: never leave a partial file.
        let _ = std::fs::remove_file(to);
    }
    result
}

fn copy_chunks_blocking(
    reader: &mut std::fs::File,
    mut writer: std::fs::File,
    total: u64,
    on_progress: &ProgressFn,
) -> Result<()> {
    use std::io::{Read as _, Write as _};

    let mut buf = vec![0u8; COPY_CHUNK_BYTES];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        writer.write_all(&buf[..n])?;
        done += n as u64;
        if !on_progress(done, total) {
            return Err(CopyCancelled.into());
        }
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

    async fn create_dir(&self, path: &Path) -> Result<()> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || Ok(std::fs::create_dir_all(&path)?))
            .await
    }

    async fn create_file(&self, path: &Path, opts: CreateOptions) -> Result<()> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || {
                let mut open = std::fs::OpenOptions::new();
                open.write(true);
                if opts.overwrite {
                    open.create(true).truncate(true);
                } else {
                    open.create_new(true);
                }
                open.open(&path)
                    .map(drop)
                    .map_err(|error| anyhow::anyhow!("create {}: {error}", path.display()))
            })
            .await
    }

    async fn copy(&self, from: &Path, to: &Path, on_progress: ProgressFn) -> Result<()> {
        let from = from.to_path_buf();
        let to = to.to_path_buf();
        self.spawner
            .unblock(move || copy_file_blocking(&from, &to, &on_progress))
            .await
    }

    async fn rename(&self, from: &Path, to: &Path, opts: RenameOptions) -> Result<()> {
        let from = from.to_path_buf();
        let to = to.to_path_buf();
        self.spawner
            .unblock(move || {
                if let Ok(existing) = std::fs::symlink_metadata(&to) {
                    if !opts.overwrite {
                        anyhow::bail!("rename: destination exists: {}", to.display());
                    }
                    if existing.is_dir() {
                        std::fs::remove_dir_all(&to)?;
                    } else {
                        std::fs::remove_file(&to)?;
                    }
                }
                Ok(std::fs::rename(&from, &to)?)
            })
            .await
    }

    async fn remove(&self, path: &Path, opts: RemoveOptions) -> Result<()> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || {
                let metadata = std::fs::symlink_metadata(&path)?;
                if metadata.is_dir() {
                    if opts.recursive {
                        std::fs::remove_dir_all(&path)?;
                    } else {
                        std::fs::remove_dir(&path)?;
                    }
                } else {
                    std::fs::remove_file(&path)?;
                }
                Ok(())
            })
            .await
    }

    async fn mode(&self, path: &Path) -> Result<Option<u32>> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || {
                // `metadata`, not `symlink_metadata`: this must report the mode
                // `set_mode` would overwrite (see the trait doc).
                let metadata = std::fs::metadata(&path)
                    .map_err(|error| anyhow!("read permissions of {}: {error}", path.display()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    Ok(Some(metadata.permissions().mode() & PERM_BITS))
                }
                #[cfg(not(unix))]
                {
                    let _ = metadata; // Windows has no unix mode to report.
                    Ok(None)
                }
            })
            .await
    }

    async fn set_mode(&self, path: &Path, mode: u32) -> Result<()> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        &path,
                        std::fs::Permissions::from_mode(mode & PERM_BITS),
                    )
                    .map_err(|error| {
                        anyhow!("chmod {:o} {}: {error}", mode & PERM_BITS, path.display())
                    })
                }
                #[cfg(not(unix))]
                {
                    let _ = mode;
                    anyhow::bail!(
                        "changing unix permissions is not supported on this platform: {}",
                        path.display()
                    )
                }
            })
            .await
    }

    async fn trash(&self, path: &Path) -> Result<TrashId> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || {
                #[cfg(target_os = "macos")]
                {
                    crate::platform::macos::trash_item_blocking(&path)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    trash_engine::fake_trash_blocking(&path)
                }
            })
            .await
    }

    async fn restore(&self, id: TrashId) -> Result<PathBuf, TrashRestoreError> {
        if self.consumed_trash.lock().unwrap().contains(&id) {
            return Err(TrashRestoreError::AlreadyRestored);
        }
        let blocking_id = id.clone();
        let restored = self
            .spawner
            .unblock(move || trash_engine::restore_blocking(&blocking_id))
            .await?;
        self.consumed_trash.lock().unwrap().insert(id);
        Ok(restored)
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
// The mode constants come out with it: a test that asserts "nothing wrote a
// mode here" needs to name the mode a `FakeVfs` node starts at.
pub use fake::{FAKE_DIR_MODE, FAKE_FILE_MODE, FakeVfs};

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
        /// Unix permission bits, modelled so [`Vfs::mode`]/[`Vfs::set_mode`]
        /// (and therefore `FileOp::Chmod` and its undo) are testable headlessly
        /// on Windows and Linux too. New nodes start at
        /// [`FAKE_FILE_MODE`]/[`FAKE_DIR_MODE`].
        mode: u32,
    }

    /// Mode a newly created `FakeVfs` file gets — `rw-r--r--`, what a real
    /// `umask 022` process would produce.
    pub const FAKE_FILE_MODE: u32 = 0o644;
    /// Mode a newly created `FakeVfs` directory gets — `rwxr-xr-x`.
    pub const FAKE_DIR_MODE: u32 = 0o755;

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
        next_trash_id: u64,
        consumed_trash: std::collections::HashSet<TrashId>,
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
                    mode: FAKE_FILE_MODE,
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
                    mode: FAKE_DIR_MODE,
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

        /// How many watches have *ever* been registered — ids are handed out
        /// monotonically, so this counts registrations rather than live ones.
        /// Lets a caller prove it does **not** tear a watch down and build it
        /// back up (each cycle on a real backend costs a run-loop stop, a
        /// thread join and a blocking re-stat, and loses any change in
        /// between), which `watcher_count` alone cannot show.
        pub fn watch_registrations(&self) -> u64 {
            self.state.lock().unwrap().next_watch_id
        }

        /// The modelled mode of one node, read **synchronously**.
        ///
        /// [`Vfs::mode`] is async, and a `#[gpui::test]` cannot await it
        /// without a foreground task; asserting "the panel's `chmod` landed"
        /// is a plain read of modelled state, so it gets a plain accessor.
        /// [`None`] for a path that does not exist.
        pub fn mode_of(&self, path: impl AsRef<Path>) -> Option<u32> {
            self.state
                .lock()
                .unwrap()
                .tree
                .get(path.as_ref())
                .map(|node| node.mode & PERM_BITS)
        }

        /// Full tree snapshot for equality assertions (undo round-trip tests):
        /// path → `None` for directories, `Some(contents)` for files.
        pub fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
            self.state
                .lock()
                .unwrap()
                .tree
                .iter()
                .map(|(path, node)| {
                    let contents = match node.kind {
                        EntryKind::Dir => None,
                        _ => Some(node.contents.clone()),
                    };
                    (path.clone(), contents)
                })
                .collect()
        }
    }

    /// Rekey the subtree rooted at `from` to `to`, preserving node data
    /// (rename semantics: mtimes survive). Returns the moved key count.
    fn rekey_subtree_locked(state: &mut FakeState, from: &Path, to: &Path) -> usize {
        let keys: Vec<PathBuf> = state
            .tree
            .keys()
            .filter(|p| p.starts_with(from))
            .cloned()
            .collect();
        let count = keys.len();
        for key in keys {
            let node = state.tree.remove(&key).expect("key just listed");
            let suffix = key.strip_prefix(from).expect("key under from");
            let new_key = if suffix.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                to.join(suffix)
            };
            state.tree.insert(new_key, node);
        }
        count
    }

    /// Insert missing ancestor directories of `path` (emitting `Created`),
    /// failing if a file blocks the way — `create_dir_all` semantics.
    fn ensure_parent_dirs_locked(state: &mut FakeState, path: &Path) -> Result<()> {
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
            let modified = next_mtime(state);
            state.tree.insert(
                dir.clone(),
                FakeNode {
                    kind: EntryKind::Dir,
                    mode: FAKE_DIR_MODE,
                    size: 0,
                    modified,
                    contents: Vec::new(),
                },
            );
            emit_locked(state, path_event(&dir, PathEventKind::Created));
        }
        Ok(())
    }

    fn check_error_locked(state: &FakeState, path: &Path) -> Result<()> {
        if let Some(message) = state.errors.get(path) {
            return Err(anyhow!("{message}"));
        }
        Ok(())
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
                        mode: FAKE_DIR_MODE,
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
                        mode: FAKE_FILE_MODE,
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

        async fn create_dir(&self, path: &Path) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            check_error_locked(&state, path)?;
            if let Some(existing) = state.tree.get(path) {
                return if matches!(existing.kind, EntryKind::Dir) {
                    Ok(()) // create_dir_all semantics: existing dir succeeds
                } else {
                    Err(anyhow!("not a directory: {}", path.display()))
                };
            }
            ensure_parent_dirs_locked(&mut state, path)?;
            let modified = next_mtime(&mut state);
            state.tree.insert(
                path.to_path_buf(),
                FakeNode {
                    kind: EntryKind::Dir,
                    mode: FAKE_DIR_MODE,
                    size: 0,
                    modified,
                    contents: Vec::new(),
                },
            );
            emit_locked(&mut state, path_event(path, PathEventKind::Created));
            Ok(())
        }

        async fn create_file(&self, path: &Path, opts: CreateOptions) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            check_error_locked(&state, path)?;
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .ok_or_else(|| anyhow!("create_file: {} has no parent", path.display()))?;
            match state.tree.get(parent) {
                Some(node) if matches!(node.kind, EntryKind::Dir) => {}
                Some(_) => return Err(anyhow!("not a directory: {}", parent.display())),
                None => return Err(anyhow!("no such directory: {}", parent.display())),
            }
            let existed = match state.tree.get(path) {
                Some(node) if matches!(node.kind, EntryKind::Dir) => {
                    return Err(anyhow!("is a directory: {}", path.display()));
                }
                Some(_) if !opts.overwrite => {
                    return Err(anyhow!("already exists: {}", path.display()));
                }
                Some(_) => true,
                None => false,
            };
            let modified = next_mtime(&mut state);
            state.tree.insert(
                path.to_path_buf(),
                FakeNode {
                    kind: EntryKind::File,
                    mode: FAKE_FILE_MODE,
                    size: 0,
                    modified,
                    contents: Vec::new(),
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

        async fn copy(&self, from: &Path, to: &Path, on_progress: ProgressFn) -> Result<()> {
            // Read the source under the lock, then run the chunked progress
            // loop outside it (mirroring RealVfs's chunked copy: a cancel can
            // land between chunks).
            let contents = {
                let state = self.state.lock().unwrap();
                check_error_locked(&state, from)?;
                check_error_locked(&state, to)?;
                let node = state
                    .tree
                    .get(from)
                    .ok_or_else(|| anyhow!("no such file: {}", from.display()))?;
                if matches!(node.kind, EntryKind::Dir) {
                    return Err(anyhow!(
                        "copy: {} is a directory (directories are expanded by op planning)",
                        from.display()
                    ));
                }
                node.contents.clone()
            };
            let total = contents.len() as u64;
            if !on_progress(0, total) {
                return Err(CopyCancelled.into());
            }
            const FAKE_CHUNK: u64 = 1024;
            let mut done = 0u64;
            while done < total {
                done = (done + FAKE_CHUNK).min(total);
                if !on_progress(done, total) {
                    return Err(CopyCancelled.into());
                }
            }
            let mut state = self.state.lock().unwrap();
            ensure_parent_dirs_locked(&mut state, to)?;
            let existed = state.tree.contains_key(to);
            let modified = next_mtime(&mut state);
            state.tree.insert(
                to.to_path_buf(),
                FakeNode {
                    kind: EntryKind::File,
                    mode: FAKE_FILE_MODE,
                    size: total,
                    modified,
                    contents,
                },
            );
            let kind = if existed {
                PathEventKind::Changed
            } else {
                PathEventKind::Created
            };
            emit_locked(&mut state, path_event(to, kind));
            Ok(())
        }

        async fn rename(&self, from: &Path, to: &Path, opts: RenameOptions) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            check_error_locked(&state, from)?;
            check_error_locked(&state, to)?;
            if !state.tree.contains_key(from) {
                return Err(anyhow!("no such path: {}", from.display()));
            }
            if to.starts_with(from) && to != from {
                return Err(anyhow!("cannot move {} into itself", from.display()));
            }
            if state.tree.contains_key(to) {
                if !opts.overwrite {
                    return Err(anyhow!("rename: destination exists: {}", to.display()));
                }
                state.tree.retain(|p, _| !p.starts_with(to));
            }
            ensure_parent_dirs_locked(&mut state, to)?;
            rekey_subtree_locked(&mut state, from, to);
            emit_locked(&mut state, path_event(from, PathEventKind::Removed));
            emit_locked(&mut state, path_event(to, PathEventKind::Created));
            Ok(())
        }

        async fn remove(&self, path: &Path, opts: RemoveOptions) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            check_error_locked(&state, path)?;
            let node = state
                .tree
                .get(path)
                .ok_or_else(|| anyhow!("no such path: {}", path.display()))?;
            if matches!(node.kind, EntryKind::Dir) && !opts.recursive {
                let has_children = state.tree.keys().any(|p| p.parent() == Some(path));
                if has_children {
                    return Err(anyhow!("directory not empty: {}", path.display()));
                }
            }
            state.tree.retain(|p, _| !p.starts_with(path));
            emit_locked(&mut state, path_event(path, PathEventKind::Removed));
            Ok(())
        }

        /// The node's modelled mode. A missing path is an `Err` (the trait's
        /// contract — `Chmod` may not treat "gone" as "no mode"), and an
        /// injected error surfaces here too, which is how the denied/EPERM
        /// path is tested on every OS.
        async fn mode(&self, path: &Path) -> Result<Option<u32>> {
            let state = self.state.lock().unwrap();
            check_error_locked(&state, path)?;
            state
                .tree
                .get(path)
                .map(|node| Some(node.mode & PERM_BITS))
                .ok_or_else(|| anyhow!("no such file: {}", path.display()))
        }

        /// Sets the modelled mode and emits `Changed` — but deliberately does
        /// **not** advance `modified`, because `chmod` changes ctime and not
        /// mtime. That asymmetry is exactly why undo of a permission change
        /// cannot be guarded by an mtime fingerprint, and a test pins it here.
        async fn set_mode(&self, path: &Path, mode: u32) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            check_error_locked(&state, path)?;
            let node = state
                .tree
                .get_mut(path)
                .ok_or_else(|| anyhow!("no such file: {}", path.display()))?;
            node.mode = mode & PERM_BITS;
            emit_locked(&mut state, path_event(path, PathEventKind::Changed));
            Ok(())
        }

        async fn trash(&self, path: &Path) -> Result<TrashId> {
            let mut state = self.state.lock().unwrap();
            check_error_locked(&state, path)?;
            if !state.tree.contains_key(path) {
                return Err(anyhow!("no such path: {}", path.display()));
            }
            let name = path
                .file_name()
                .ok_or_else(|| anyhow!("cannot trash {}", path.display()))?
                .to_string_lossy()
                .into_owned();
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .ok_or_else(|| anyhow!("cannot trash a root: {}", path.display()))?;
            state.next_trash_id += 1;
            let entry_dir = parent
                .join(crate::platform::trash::FAKE_TRASH_DIR)
                .join(format!("{}-{name}", state.next_trash_id));
            // Materialize the trash entry dir (and `.fake-trash` root).
            ensure_parent_dirs_locked(&mut state, &entry_dir.join("x"))?;
            let trashed = entry_dir.join(&name);
            rekey_subtree_locked(&mut state, path, &trashed);
            emit_locked(&mut state, path_event(path, PathEventKind::Removed));
            emit_locked(&mut state, path_event(&trashed, PathEventKind::Created));
            Ok(TrashId {
                original: path.to_path_buf(),
                trashed,
            })
        }

        async fn restore(&self, id: TrashId) -> Result<PathBuf, TrashRestoreError> {
            let mut state = self.state.lock().unwrap();
            if state.consumed_trash.contains(&id) {
                return Err(TrashRestoreError::AlreadyRestored);
            }
            if !state.tree.contains_key(&id.trashed) {
                return Err(TrashRestoreError::NotFound);
            }
            if state.tree.contains_key(&id.original) {
                return Err(TrashRestoreError::Collision(id.original.clone()));
            }
            if ensure_parent_dirs_locked(&mut state, &id.original).is_err() {
                return Err(TrashRestoreError::Collision(id.original.clone()));
            }
            rekey_subtree_locked(&mut state, &id.trashed, &id.original);
            // Clean the now-empty trash entry dir (and the `.fake-trash` root
            // if this was its last entry), emitting Removed for each — every
            // tree mutation must be visible to watchers.
            if let Some(entry_dir) = id.trashed.parent() {
                let entry_dir = entry_dir.to_path_buf();
                if state.tree.remove(&entry_dir).is_some() {
                    emit_locked(&mut state, path_event(&entry_dir, PathEventKind::Removed));
                }
                if let Some(root) = entry_dir.parent() {
                    let root = root.to_path_buf();
                    let empty = !state.tree.keys().any(|p| p.parent() == Some(&root));
                    if empty && state.tree.remove(&root).is_some() {
                        emit_locked(&mut state, path_event(&root, PathEventKind::Removed));
                    }
                }
            }
            emit_locked(&mut state, path_event(&id.trashed, PathEventKind::Removed));
            emit_locked(&mut state, path_event(&id.original, PathEventKind::Created));
            let original = id.original.clone();
            state.consumed_trash.insert(id);
            Ok(original)
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
                        mode: FAKE_DIR_MODE,
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
                    mode: FAKE_FILE_MODE,
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
    use crate::watcher::PathEventKind;
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

    // -----------------------------------------------------------------------
    // M3 mutation surface
    // -----------------------------------------------------------------------

    fn noop_progress() -> ProgressFn {
        Arc::new(|_, _| true)
    }

    fn real_test_vfs() -> RealVfs {
        RealVfs::new(Arc::new(TestSpawner::new()))
    }

    #[test]
    fn fake_vfs_create_dir_and_create_file() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree("/root", json!({ "a.txt": "abc" }));

        // create_dir_all semantics: ancestors created, existing dir is fine.
        block_on(vfs.create_dir(Path::new("/root/x/y"))).unwrap();
        block_on(vfs.create_dir(Path::new("/root/x/y"))).unwrap();
        assert_eq!(
            block_on(vfs.metadata(Path::new("/root/x")))
                .unwrap()
                .unwrap()
                .kind,
            EntryKind::Dir
        );
        // A file in the way fails.
        assert!(block_on(vfs.create_dir(Path::new("/root/a.txt"))).is_err());
        assert!(block_on(vfs.create_dir(Path::new("/root/a.txt/sub"))).is_err());

        // create_file: parent must exist; no silent overwrite.
        block_on(vfs.create_file(Path::new("/root/x/new.txt"), CreateOptions::default())).unwrap();
        assert_eq!(
            block_on(vfs.load(Path::new("/root/x/new.txt"))).unwrap(),
            Vec::<u8>::new()
        );
        assert!(
            block_on(vfs.create_file(Path::new("/root/x/new.txt"), CreateOptions::default()))
                .is_err(),
            "existing file fails without overwrite"
        );
        block_on(vfs.create_file(
            Path::new("/root/x/new.txt"),
            CreateOptions { overwrite: true },
        ))
        .unwrap();
        assert!(
            block_on(vfs.create_file(Path::new("/root/missing/f.txt"), CreateOptions::default()))
                .is_err(),
            "missing parent fails"
        );
        assert!(
            block_on(vfs.create_file(Path::new("/root/x"), CreateOptions { overwrite: true }))
                .is_err(),
            "cannot overwrite a directory with a file"
        );
    }

    #[test]
    fn fake_vfs_rename_moves_subtrees_and_preserves_mtimes() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree(
            "/root",
            json!({ "dir": { "a.txt": "a", "sub": { "b.txt": "b" } }, "other.txt": "o" }),
        );
        let before = block_on(vfs.metadata(Path::new("/root/dir/a.txt")))
            .unwrap()
            .unwrap();

        block_on(vfs.rename(
            Path::new("/root/dir"),
            Path::new("/root/renamed"),
            RenameOptions::default(),
        ))
        .unwrap();
        assert!(
            block_on(vfs.metadata(Path::new("/root/dir")))
                .unwrap()
                .is_none(),
            "source subtree gone"
        );
        let after = block_on(vfs.metadata(Path::new("/root/renamed/a.txt")))
            .unwrap()
            .unwrap();
        assert_eq!(after.modified, before.modified, "rename preserves mtimes");
        assert_eq!(
            block_on(vfs.load(Path::new("/root/renamed/sub/b.txt"))).unwrap(),
            b"b"
        );

        // Destination-exists rules.
        assert!(
            block_on(vfs.rename(
                Path::new("/root/renamed"),
                Path::new("/root/other.txt"),
                RenameOptions::default(),
            ))
            .is_err(),
            "existing destination fails without overwrite"
        );
        block_on(vfs.rename(
            Path::new("/root/renamed/a.txt"),
            Path::new("/root/other.txt"),
            RenameOptions { overwrite: true },
        ))
        .unwrap();
        assert_eq!(
            block_on(vfs.load(Path::new("/root/other.txt"))).unwrap(),
            b"a"
        );

        // Cannot move a directory into itself; missing source is an error.
        assert!(
            block_on(vfs.rename(
                Path::new("/root/renamed"),
                Path::new("/root/renamed/inside"),
                RenameOptions::default(),
            ))
            .is_err()
        );
        assert!(
            block_on(vfs.rename(
                Path::new("/root/nope"),
                Path::new("/root/anything"),
                RenameOptions::default(),
            ))
            .is_err()
        );
    }

    #[test]
    fn fake_vfs_remove_semantics() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree(
            "/root",
            json!({ "dir": { "a.txt": "a" }, "empty": {}, "f.txt": "f" }),
        );

        assert!(
            block_on(vfs.remove(Path::new("/root/dir"), RemoveOptions::default())).is_err(),
            "non-empty dir needs recursive"
        );
        block_on(vfs.remove(Path::new("/root/dir"), RemoveOptions { recursive: true })).unwrap();
        assert!(
            block_on(vfs.metadata(Path::new("/root/dir/a.txt")))
                .unwrap()
                .is_none()
        );

        block_on(vfs.remove(Path::new("/root/empty"), RemoveOptions::default())).unwrap();
        block_on(vfs.remove(Path::new("/root/f.txt"), RemoveOptions::default())).unwrap();
        assert!(
            block_on(vfs.remove(Path::new("/root/f.txt"), RemoveOptions::default())).is_err(),
            "missing path is an error"
        );
    }

    #[test]
    fn fake_vfs_copy_reports_chunked_progress_and_cancels_without_partials() {
        let (_spawner, vfs) = test_vfs();
        let contents = "x".repeat(2500); // 3 chunks of 1024
        vfs.insert_tree("/root", json!({ "src.bin": contents }));

        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorder = calls.clone();
        let progress: ProgressFn = Arc::new(move |done, total| {
            recorder.lock().unwrap().push((done, total));
            true
        });
        block_on(vfs.copy(
            Path::new("/root/src.bin"),
            Path::new("/root/dst.bin"),
            progress,
        ))
        .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(0, 2500), (1024, 2500), (2048, 2500), (2500, 2500)]
        );
        assert_eq!(
            block_on(vfs.load(Path::new("/root/dst.bin")))
                .unwrap()
                .len(),
            2500
        );

        // Abort after the first chunk: typed error, no destination node.
        let aborting: ProgressFn = Arc::new(|done, _| done < 1024);
        let error = block_on(vfs.copy(
            Path::new("/root/src.bin"),
            Path::new("/root/cancelled.bin"),
            aborting,
        ))
        .unwrap_err();
        assert!(error.is::<CopyCancelled>(), "typed cancel marker: {error}");
        assert!(
            block_on(vfs.metadata(Path::new("/root/cancelled.bin")))
                .unwrap()
                .is_none(),
            "cancel mid-copy leaves no partial file"
        );

        // Copying a directory is a planning-layer mistake, not silent.
        assert!(
            block_on(vfs.copy(Path::new("/root"), Path::new("/elsewhere"), noop_progress()))
                .is_err()
        );
        // Error injection covers copy destinations too.
        vfs.set_error("/root/locked.bin", "disk full");
        assert!(
            block_on(vfs.copy(
                Path::new("/root/src.bin"),
                Path::new("/root/locked.bin"),
                noop_progress(),
            ))
            .is_err()
        );
    }

    #[test]
    fn fake_vfs_trash_restore_round_trip_via_fake_trash_subtree() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree("/root", json!({ "dir": { "a.txt": "a" } }));
        let before = vfs.snapshot();

        let id = block_on(vfs.trash(Path::new("/root/dir"))).unwrap();
        assert_eq!(id.original, PathBuf::from("/root/dir"));
        assert!(id.trashed.starts_with("/root/.fake-trash"));
        assert!(
            block_on(vfs.metadata(Path::new("/root/dir")))
                .unwrap()
                .is_none(),
            "trashed item left its original location"
        );
        assert_eq!(
            block_on(vfs.load(&id.trashed.join("a.txt"))).unwrap(),
            b"a",
            ".fake-trash holds the restorable subtree"
        );

        let restored = block_on(vfs.restore(id)).unwrap();
        assert_eq!(restored, PathBuf::from("/root/dir"));
        assert_eq!(vfs.snapshot(), before, "restore is an exact round trip");
    }

    #[test]
    fn fake_vfs_restore_error_variants() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree("/root", json!({ "a.txt": "a", "b.txt": "b" }));

        // NotFound: trash emptied externally.
        let gone = block_on(vfs.trash(Path::new("/root/a.txt"))).unwrap();
        vfs.remove_path(&gone.trashed);
        assert_eq!(
            block_on(vfs.restore(gone)).unwrap_err(),
            TrashRestoreError::NotFound
        );

        // Collision: original path re-occupied.
        let occupied = block_on(vfs.trash(Path::new("/root/b.txt"))).unwrap();
        vfs.insert_file("/root/b.txt", 9);
        assert_eq!(
            block_on(vfs.restore(occupied.clone())).unwrap_err(),
            TrashRestoreError::Collision(PathBuf::from("/root/b.txt"))
        );

        // AlreadyRestored: token consumed (double-undo race).
        vfs.remove_path("/root/b.txt");
        block_on(vfs.restore(occupied.clone())).unwrap();
        assert_eq!(
            block_on(vfs.restore(occupied)).unwrap_err(),
            TrashRestoreError::AlreadyRestored
        );
    }

    #[test]
    fn fake_vfs_mutations_emit_watcher_events() {
        let test_spawner = Arc::new(TestSpawner::new());
        let spawner: Arc<dyn Spawner> = test_spawner.clone();
        let vfs = FakeVfs::new(spawner);
        vfs.insert_tree("/dir", json!({ "a.txt": "a" }));
        let (mut stream, _guard) = vfs.watch(Path::new("/dir"), Duration::from_millis(10));

        block_on(vfs.rename(
            Path::new("/dir/a.txt"),
            Path::new("/dir/b.txt"),
            RenameOptions::default(),
        ))
        .unwrap();
        test_spawner.advance(Duration::from_millis(10));
        let batch = block_on(stream.next()).expect("rename batch");
        let kinds: Vec<_> = batch.iter().map(|e| (e.path.clone(), e.kind)).collect();
        assert!(kinds.contains(&(Arc::from(Path::new("/dir/a.txt")), PathEventKind::Removed)));
        assert!(kinds.contains(&(Arc::from(Path::new("/dir/b.txt")), PathEventKind::Created)));
    }

    #[test]
    fn real_vfs_create_rename_remove_round_trip() {
        let vfs = real_test_vfs();
        let dir = tempfile::tempdir().unwrap();

        block_on(vfs.create_dir(&dir.path().join("a/b"))).unwrap();
        block_on(vfs.create_file(&dir.path().join("a/b/f.txt"), CreateOptions::default())).unwrap();
        assert!(
            block_on(vfs.create_file(&dir.path().join("a/b/f.txt"), CreateOptions::default()))
                .is_err(),
            "existing file fails without overwrite"
        );

        block_on(vfs.rename(
            &dir.path().join("a/b/f.txt"),
            &dir.path().join("a/g.txt"),
            RenameOptions::default(),
        ))
        .unwrap();
        assert!(dir.path().join("a/g.txt").exists());

        std::fs::write(dir.path().join("occupied.txt"), b"keep").unwrap();
        assert!(
            block_on(vfs.rename(
                &dir.path().join("a/g.txt"),
                &dir.path().join("occupied.txt"),
                RenameOptions::default(),
            ))
            .is_err(),
            "existing destination fails without overwrite"
        );
        block_on(vfs.rename(
            &dir.path().join("a/g.txt"),
            &dir.path().join("occupied.txt"),
            RenameOptions { overwrite: true },
        ))
        .unwrap();

        assert!(
            block_on(vfs.remove(&dir.path().join("a"), RemoveOptions::default())).is_err(),
            "non-empty dir needs recursive"
        );
        block_on(vfs.remove(&dir.path().join("a"), RemoveOptions { recursive: true })).unwrap();
        assert!(!dir.path().join("a").exists());
        assert!(
            block_on(vfs.remove(&dir.path().join("a"), RemoveOptions::default())).is_err(),
            "missing path is an error"
        );
    }

    #[test]
    fn real_vfs_copy_progress_and_cancel_leaves_no_partial_file() {
        let vfs = real_test_vfs();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.bin");
        std::fs::write(&src, vec![7u8; 3 * 1024 * 1024]).unwrap(); // 3 chunks

        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorder = calls.clone();
        let progress: ProgressFn = Arc::new(move |done, total| {
            recorder.lock().unwrap().push((done, total));
            true
        });
        block_on(vfs.copy(&src, &dir.path().join("copy.bin"), progress)).unwrap();
        assert_eq!(
            std::fs::metadata(dir.path().join("copy.bin"))
                .unwrap()
                .len(),
            3 * 1024 * 1024
        );
        {
            let calls = calls.lock().unwrap();
            assert!(calls.len() >= 4, "chunked progress: {calls:?}");
            assert_eq!(calls[0], (0, 3 * 1024 * 1024));
            assert_eq!(calls.last().unwrap().0, 3 * 1024 * 1024);
        }

        // Abort after the first chunk: typed error and no partial file.
        let aborting: ProgressFn = Arc::new(|done, _| done == 0);
        let error =
            block_on(vfs.copy(&src, &dir.path().join("partial.bin"), aborting)).unwrap_err();
        assert!(error.is::<CopyCancelled>(), "typed cancel marker: {error}");
        assert!(
            !dir.path().join("partial.bin").exists(),
            "cancel mid-copy leaves no partial file"
        );
    }

    #[test]
    fn real_vfs_copy_failure_before_writing_leaves_an_existing_destination_alone() {
        let vfs = real_test_vfs();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest.txt");
        std::fs::write(&dest, b"precious").unwrap();

        // Missing source: the copy fails before the destination is opened —
        // the "remove the partial file" cleanup must not fire.
        let error = block_on(vfs.copy(&dir.path().join("missing.txt"), &dest, noop_progress()))
            .unwrap_err();
        assert!(!error.is::<CopyCancelled>());
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"precious",
            "a failure before the first write never touches the destination"
        );

        // Cancelling before the first chunk leaves it alone too.
        let cancel_immediately: ProgressFn = Arc::new(|_, _| false);
        let error = block_on(vfs.copy(&dir.path().join("missing.txt"), &dest, cancel_immediately))
            .unwrap_err();
        assert!(!error.is::<CopyCancelled>(), "source check comes first");
        assert_eq!(std::fs::read(&dest).unwrap(), b"precious");
    }

    // The macOS build routes Vfs::trash through the real NSFileManager trash
    // (exercised by the per-milestone Mac checklist, like the notify watcher);
    // the portable `.fake-trash` scheme is what runs on Windows CI (§9).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn real_vfs_trash_restore_round_trip_and_error_variants() {
        let vfs = real_test_vfs();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("project");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"a").unwrap();

        let id = block_on(vfs.trash(&target)).unwrap();
        assert!(!target.exists());
        assert!(id.trashed.starts_with(dir.path().join(".fake-trash")));
        assert_eq!(std::fs::read(id.trashed.join("a.txt")).unwrap(), b"a");

        // Collision: original re-occupied.
        std::fs::create_dir(&target).unwrap();
        assert_eq!(
            block_on(vfs.restore(id.clone())).unwrap_err(),
            TrashRestoreError::Collision(target.clone())
        );
        std::fs::remove_dir(&target).unwrap();

        // Round trip, then AlreadyRestored on the double undo.
        assert_eq!(block_on(vfs.restore(id.clone())).unwrap(), target);
        assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"a");
        assert_eq!(
            block_on(vfs.restore(id)).unwrap_err(),
            TrashRestoreError::AlreadyRestored
        );

        // NotFound: trash emptied externally.
        let file = dir.path().join("doomed.txt");
        std::fs::write(&file, b"x").unwrap();
        let id = block_on(vfs.trash(&file)).unwrap();
        std::fs::remove_file(&id.trashed).unwrap();
        assert_eq!(
            block_on(vfs.restore(id)).unwrap_err(),
            TrashRestoreError::NotFound
        );
    }

    #[test]
    fn fake_vfs_models_a_mode_and_a_chmod_never_touches_the_mtime() {
        let (_spawner, vfs) = test_vfs();
        vfs.insert_tree("/root", json!({ "a.txt": "a", "sub": {} }));
        let file = Path::new("/root/a.txt");

        assert_eq!(
            block_on(vfs.mode(file)).unwrap(),
            Some(fake::FAKE_FILE_MODE)
        );
        assert_eq!(
            block_on(vfs.mode(Path::new("/root/sub"))).unwrap(),
            Some(fake::FAKE_DIR_MODE),
            "directories start executable"
        );
        let before = block_on(vfs.metadata(file)).unwrap().unwrap().modified;

        // The type bits are masked off: a mode is only ever the low 12 bits.
        block_on(vfs.set_mode(file, 0o100_600)).unwrap();
        assert_eq!(block_on(vfs.mode(file)).unwrap(), Some(0o600));
        // The sync accessor the UI tests read through agrees with the async
        // one, and answers `None` — not an error — for a missing path.
        assert_eq!(vfs.mode_of(file), Some(0o600));
        assert_eq!(vfs.mode_of("/root/gone.txt"), None);
        assert_eq!(
            block_on(vfs.metadata(file)).unwrap().unwrap().modified,
            before,
            "chmod changes ctime, not mtime"
        );

        // A missing path is an error, not `Ok(None)` — Chmod may not treat
        // "gone" as "no mode".
        let error = block_on(vfs.mode(Path::new("/root/gone.txt"))).unwrap_err();
        assert!(error.to_string().contains("no such file"), "{error}");
        let error = block_on(vfs.set_mode(Path::new("/root/gone.txt"), 0o600)).unwrap_err();
        assert!(error.to_string().contains("no such file"), "{error}");

        // Injected errors surface, which is how a denied chmod is tested off macOS.
        vfs.set_error("/root/a.txt", "Permission denied");
        assert!(block_on(vfs.set_mode(file, 0o644)).is_err());
    }

    /// `chmod` straight through `std::fs`, so the tests below can set up (and
    /// double-check) permissions without going through the code under test.
    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_vfs_reads_and_writes_a_unix_mode() {
        let dir = tempfile::tempdir().unwrap();
        let vfs = real_test_vfs();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"a").unwrap();

        chmod(&file, 0o644);
        assert_eq!(block_on(vfs.mode(&file)).unwrap(), Some(0o644));

        block_on(vfs.set_mode(&file, 0o600)).unwrap();
        assert_eq!(block_on(vfs.mode(&file)).unwrap(), Some(0o600));
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & PERM_BITS,
                0o600,
                "the real file on disk changed, not just our view of it"
            );
        }

        // Setuid and sticky survive the round trip (four-digit modes).
        block_on(vfs.set_mode(&file, 0o4755)).unwrap();
        assert_eq!(block_on(vfs.mode(&file)).unwrap(), Some(0o4755));

        let error = block_on(vfs.mode(&dir.path().join("gone.txt"))).unwrap_err();
        assert!(error.to_string().contains("read permissions of"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn real_vfs_permissions_follow_a_symlink_the_way_chmod_does() {
        let dir = tempfile::tempdir().unwrap();
        let vfs = real_test_vfs();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&target, b"t").unwrap();
        chmod(&target, 0o644);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // `mode` and `set_mode` must describe the SAME inode, or an undo would
        // write a link's mode onto its target.
        assert_eq!(block_on(vfs.mode(&link)).unwrap(), Some(0o644));
        block_on(vfs.set_mode(&link, 0o600)).unwrap();
        assert_eq!(block_on(vfs.mode(&target)).unwrap(), Some(0o600));
    }
}
