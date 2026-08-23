//! Directory listings: snapshots, incremental patching, and the per-pane LRU
//! cache (ARCHITECTURE.md §6, `listing.rs`).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt as _;

use crate::entry::FileEntry;
use crate::sort::SortSpec;
use crate::vfs::Vfs;
use crate::watcher::{PathEvent, PathEventKind};

/// One immutable, sorted view of a directory. Cheap to clone (`Arc` inside);
/// replaced wholesale on load and on watcher patches.
#[derive(Clone, Debug)]
pub struct ListingSnapshot {
    pub dir: Arc<Path>,
    pub entries: Arc<Vec<FileEntry>>,
    pub sort: SortSpec,
    /// Navigation race guard: a stale load's snapshot (older generation) is
    /// discarded by the consumer, never rendered.
    pub generation: u64,
    pub show_hidden: bool,
}

/// Stream `read_dir`, filter hidden entries, sort once, snapshot.
///
/// Owned arguments so the returned future is `'static` — callers run it
/// wholesale on the background executor.
pub async fn list_dir(
    vfs: Arc<dyn Vfs>,
    dir: Arc<Path>,
    sort: SortSpec,
    show_hidden: bool,
    generation: u64,
) -> Result<ListingSnapshot> {
    let mut stream = vfs.read_dir(&dir).await?;
    let mut entries = Vec::new();
    while let Some(entry) = stream.next().await {
        let entry = entry?;
        if show_hidden || !entry.hidden {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| sort.compare(a, b));
    Ok(ListingSnapshot {
        dir,
        entries: Arc::new(entries),
        sort,
        generation,
        show_hidden,
    })
}

/// One resolved change to apply to a snapshot. The consumer stats the changed
/// paths from a watcher batch and turns them into patches; a `Rescan` event has
/// no patch form — it triggers a full [`list_dir`] reload instead.
#[derive(Clone, Debug)]
pub enum ListingPatch {
    /// Insert a new entry, or replace the entry with the same path.
    Upsert(FileEntry),
    /// Remove the entry with this path (no-op if absent).
    Remove(Arc<Path>),
}

/// Apply a debounced batch of patches, preserving sort order: removal by path,
/// sorted insertion via binary search — no full re-sort for single events.
/// Hidden entries stay excluded while `show_hidden` is off.
pub fn patch_listing(snapshot: &ListingSnapshot, patches: Vec<ListingPatch>) -> ListingSnapshot {
    let mut entries: Vec<FileEntry> = (*snapshot.entries).clone();
    for patch in patches {
        match patch {
            ListingPatch::Upsert(entry) => {
                if let Some(ix) = entries.iter().position(|e| e.path == entry.path) {
                    entries.remove(ix);
                }
                if entry.hidden && !snapshot.show_hidden {
                    continue;
                }
                let ix = match entries.binary_search_by(|e| snapshot.sort.compare(e, &entry)) {
                    Ok(ix) | Err(ix) => ix,
                };
                entries.insert(ix, entry);
            }
            ListingPatch::Remove(path) => {
                entries.retain(|e| e.path != path);
            }
        }
    }
    ListingSnapshot {
        dir: snapshot.dir.clone(),
        entries: Arc::new(entries),
        sort: snapshot.sort,
        generation: snapshot.generation,
        show_hidden: snapshot.show_hidden,
    }
}

/// One debounced watcher batch resolved against the filesystem: what to fold
/// into the snapshot, whether the batch forces a full reload, and which
/// directories' contents it touched.
#[derive(Clone, Debug, Default)]
pub struct ResolvedBatch {
    /// Patches for direct children of the watched directory, in event order
    /// (feed straight to [`patch_listing`]).
    pub patches: Vec<ListingPatch>,
    /// A `Rescan` was in the batch: incremental events are no longer
    /// trustworthy, so the consumer reloads the directory in full
    /// ([`list_dir`]) instead of patching.
    pub reload: bool,
    /// Every directory whose contents the batch touched, sorted and
    /// deduplicated — the watched directory itself when one of its children
    /// changed, plus deeper directories when the backend reports descendants.
    /// This is the invalidation set for *cached child listings* (the details
    /// view's in-place expansion, the sidebar tree), which must not survive an
    /// external change to the folder they came from.
    pub changed_dirs: Vec<Arc<Path>>,
}

/// Turn one debounced watcher batch for `dir` into [`ListingPatch`]es.
///
/// Stats only the changed **direct children** of `dir` — a `Created`/`Changed`
/// path that no longer stats (created and deleted inside one debounce window,
/// or now unreadable) resolves to [`ListingPatch::Remove`], so a patched
/// snapshot never keeps a row the filesystem no longer has. Events for deeper
/// paths produce no patch; they only contribute their parent directory to
/// [`ResolvedBatch::changed_dirs`].
///
/// Owned arguments so the returned future is `'static` — callers run it
/// wholesale on the background executor (the UI thread never stats).
pub async fn resolve_watch_batch(
    vfs: Arc<dyn Vfs>,
    dir: Arc<Path>,
    batch: Vec<PathEvent>,
) -> ResolvedBatch {
    // Rescan swallows its batch in the watcher already; a full reload is the
    // only sound response, and every cached child listing is suspect.
    if batch
        .iter()
        .any(|event| event.kind == PathEventKind::Rescan)
    {
        return ResolvedBatch {
            patches: Vec::new(),
            reload: true,
            changed_dirs: vec![dir],
        };
    }

    let mut changed_dirs: BTreeSet<Arc<Path>> = BTreeSet::new();
    // First-seen order, newest kind per path: one stat per changed child even
    // when a paste-storm reported it several times.
    let mut order: Vec<Arc<Path>> = Vec::new();
    let mut kinds: HashMap<Arc<Path>, PathEventKind> = HashMap::new();
    for event in batch {
        let Some(parent) = event.path.parent() else {
            continue; // a root has no listing to patch
        };
        changed_dirs.insert(Arc::from(parent));
        if parent != dir.as_ref() {
            continue;
        }
        if kinds.insert(event.path.clone(), event.kind).is_none() {
            order.push(event.path);
        }
    }

    let mut patches = Vec::with_capacity(order.len());
    for path in order {
        let meta = if kinds[&path] == PathEventKind::Removed {
            None
        } else {
            vfs.metadata(&path).await.ok().flatten()
        };
        patches.push(match meta {
            Some(meta) => ListingPatch::Upsert(meta.into_entry(path)),
            None => ListingPatch::Remove(path),
        });
    }
    ResolvedBatch {
        patches,
        reload: false,
        changed_dirs: changed_dirs.into_iter().collect(),
    }
}

/// Small LRU of listing snapshots, one per pane — the reason back/forward
/// paints instantly (render-cached-then-refresh; the fresh load and watcher
/// patches write back so re-entering a watched directory is exact).
pub struct ListingCache {
    capacity: usize,
    /// Most-recently-used first, keyed by `snapshot.dir`.
    entries: VecDeque<Arc<ListingSnapshot>>,
}

impl ListingCache {
    /// Default capacity per ARCHITECTURE.md §6.
    pub const DEFAULT_CAPACITY: usize = 16;

    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "ListingCache capacity must be nonzero");
        Self {
            capacity,
            entries: VecDeque::new(),
        }
    }

    /// A hit returns the (possibly stale) snapshot and marks it
    /// most-recently-used; the caller paints it and refreshes in the
    /// background.
    pub fn get(&mut self, dir: &Path) -> Option<Arc<ListingSnapshot>> {
        let ix = self.entries.iter().position(|s| &*s.dir == dir)?;
        let snapshot = self.entries.remove(ix).expect("index from position");
        self.entries.push_front(snapshot.clone());
        Some(snapshot)
    }

    /// Insert or replace the snapshot for its directory (called on every load
    /// and every watcher patch — the write-back that keeps the cache exact).
    pub fn insert(&mut self, snapshot: Arc<ListingSnapshot>) {
        self.entries.retain(|s| s.dir != snapshot.dir);
        self.entries.push_front(snapshot);
        self.entries.truncate(self.capacity);
    }

    /// Drop any cached snapshot for `dir`.
    pub fn invalidate(&mut self, dir: &Path) {
        self.entries.retain(|s| &*s.dir != dir);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for ListingCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;
    use crate::exec::{Spawner, TestSpawner};
    use crate::sort::{SortDirection, SortKey};
    use crate::vfs::FakeVfs;
    use futures::executor::block_on;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn fixture_vfs() -> Arc<FakeVfs> {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = FakeVfs::new(spawner);
        vfs.insert_tree(
            "/root",
            json!({
                "zeta": {},
                "file10.txt": "..........",
                "file2.txt": "..",
                ".secret": "s",
                "Alpha": {},
            }),
        );
        vfs
    }

    fn list(vfs: &Arc<FakeVfs>, sort: SortSpec, show_hidden: bool) -> ListingSnapshot {
        block_on(list_dir(
            vfs.clone() as Arc<dyn Vfs>,
            Arc::from(PathBuf::from("/root")),
            sort,
            show_hidden,
            1,
        ))
        .unwrap()
    }

    fn names(snapshot: &ListingSnapshot) -> Vec<&str> {
        snapshot.entries.iter().map(|e| &*e.name).collect()
    }

    fn file(name: &str, hidden: bool) -> FileEntry {
        FileEntry {
            path: Arc::from(PathBuf::from(format!("/root/{name}"))),
            name: name.into(),
            kind: EntryKind::File,
            size: 1,
            modified: SystemTime::UNIX_EPOCH,
            created: None,
            hidden,
        }
    }

    fn snapshot_for(dir: &str) -> Arc<ListingSnapshot> {
        Arc::new(ListingSnapshot {
            dir: Arc::from(PathBuf::from(dir)),
            entries: Arc::new(Vec::new()),
            sort: SortSpec::default(),
            generation: 0,
            show_hidden: false,
        })
    }

    #[test]
    fn list_dir_sorts_folders_first_naturally() {
        let vfs = fixture_vfs();
        let snapshot = list(&vfs, SortSpec::default(), false);
        assert_eq!(
            names(&snapshot),
            ["Alpha", "zeta", "file2.txt", "file10.txt"]
        );
        assert_eq!(snapshot.generation, 1);
    }

    #[test]
    fn list_dir_hidden_filter() {
        let vfs = fixture_vfs();
        let without = list(&vfs, SortSpec::default(), false);
        assert!(!names(&without).contains(&".secret"));
        let with = list(&vfs, SortSpec::default(), true);
        assert_eq!(
            names(&with),
            ["Alpha", "zeta", ".secret", "file2.txt", "file10.txt"]
        );
    }

    #[test]
    fn list_dir_respects_sort_spec() {
        let vfs = fixture_vfs();
        let snapshot = list(
            &vfs,
            SortSpec {
                key: SortKey::Name,
                direction: SortDirection::Descending,
                folders_first: true,
            },
            false,
        );
        assert_eq!(
            names(&snapshot),
            ["zeta", "Alpha", "file10.txt", "file2.txt"]
        );
    }

    #[test]
    fn patch_listing_inserts_at_sorted_position() {
        let vfs = fixture_vfs();
        let snapshot = list(&vfs, SortSpec::default(), false);
        let patched = patch_listing(
            &snapshot,
            vec![ListingPatch::Upsert(file("file3.txt", false))],
        );
        assert_eq!(
            names(&patched),
            ["Alpha", "zeta", "file2.txt", "file3.txt", "file10.txt"]
        );
        // Original snapshot untouched.
        assert_eq!(snapshot.entries.len(), 4);
    }

    #[test]
    fn patch_listing_removes_and_updates_preserving_sort() {
        let vfs = fixture_vfs();
        let snapshot = list(&vfs, SortSpec::default(), false);

        let removed = patch_listing(
            &snapshot,
            vec![ListingPatch::Remove(Arc::from(PathBuf::from(
                "/root/file2.txt",
            )))],
        );
        assert_eq!(names(&removed), ["Alpha", "zeta", "file10.txt"]);

        // An update (upsert of an existing path) replaces in place.
        let mut updated_entry = file("file2.txt", false);
        updated_entry.size = 999;
        let updated = patch_listing(&snapshot, vec![ListingPatch::Upsert(updated_entry)]);
        assert_eq!(
            names(&updated),
            ["Alpha", "zeta", "file2.txt", "file10.txt"]
        );
        let entry = updated
            .entries
            .iter()
            .find(|e| &*e.name == "file2.txt")
            .unwrap();
        assert_eq!(entry.size, 999);
    }

    #[test]
    fn patch_listing_keeps_hidden_entries_out_when_hidden_filtered() {
        let vfs = fixture_vfs();
        let snapshot = list(&vfs, SortSpec::default(), false);
        let patched = patch_listing(&snapshot, vec![ListingPatch::Upsert(file(".sneaky", true))]);
        assert!(!names(&patched).contains(&".sneaky"));

        let showing = list(&vfs, SortSpec::default(), true);
        let patched = patch_listing(&showing, vec![ListingPatch::Upsert(file(".sneaky", true))]);
        assert!(names(&patched).contains(&".sneaky"));
    }

    // ------------------------------------------------------------------
    // resolve_watch_batch: watcher events → patches + invalidation set
    // ------------------------------------------------------------------

    fn event(path: &str, kind: PathEventKind) -> PathEvent {
        PathEvent {
            path: Arc::from(PathBuf::from(path).as_path()),
            kind,
        }
    }

    fn resolve(vfs: &Arc<FakeVfs>, dir: &str, batch: Vec<PathEvent>) -> ResolvedBatch {
        block_on(resolve_watch_batch(
            vfs.clone() as Arc<dyn Vfs>,
            Arc::from(PathBuf::from(dir)),
            batch,
        ))
    }

    #[test]
    fn resolve_watch_batch_folds_creates_and_removes_into_the_snapshot() {
        let vfs = fixture_vfs();
        // Snapshot first, then change the world behind it — exactly the order
        // a live pane sees.
        let snapshot = list(&vfs, SortSpec::default(), false);
        vfs.insert_file("/root/file3.txt", 3);
        vfs.remove_path("/root/file2.txt");

        let resolved = resolve(
            &vfs,
            "/root",
            vec![
                event("/root/file3.txt", PathEventKind::Created),
                event("/root/file2.txt", PathEventKind::Removed),
            ],
        );
        let patched = patch_listing(&snapshot, resolved.patches);
        assert_eq!(
            names(&patched),
            ["Alpha", "zeta", "file3.txt", "file10.txt"],
            "the created file lands in sort position, the removed one is gone"
        );
    }

    #[test]
    fn resolve_watch_batch_treats_a_vanished_create_as_a_removal() {
        // Created then deleted inside one debounce window: the stat misses, so
        // the patch must be a removal, never an upsert of a ghost row.
        let vfs = fixture_vfs();
        let resolved = resolve(
            &vfs,
            "/root",
            vec![event("/root/ghost.txt", PathEventKind::Created)],
        );
        assert!(matches!(
            resolved.patches.as_slice(),
            [ListingPatch::Remove(path)] if &**path == Path::new("/root/ghost.txt")
        ));
    }

    #[test]
    fn resolve_watch_batch_stats_each_changed_path_once() {
        let vfs = fixture_vfs();
        vfs.insert_file("/root/file2.txt", 77);
        let resolved = resolve(
            &vfs,
            "/root",
            vec![
                event("/root/file2.txt", PathEventKind::Created),
                event("/root/file2.txt", PathEventKind::Changed),
            ],
        );
        assert_eq!(resolved.patches.len(), 1, "one patch per changed path");
        assert!(matches!(
            &resolved.patches[0],
            ListingPatch::Upsert(entry) if entry.size == 77
        ));
    }

    #[test]
    fn resolve_watch_batch_reports_changed_dirs_without_patching_descendants() {
        let vfs = fixture_vfs();
        vfs.insert_file("/root/zeta/deep.txt", 1);
        let resolved = resolve(
            &vfs,
            "/root",
            vec![
                event("/root/zeta/deep.txt", PathEventKind::Created),
                event("/root/file2.txt", PathEventKind::Removed),
            ],
        );
        assert_eq!(
            resolved.patches.len(),
            1,
            "only direct children patch the listing"
        );
        let dirs: Vec<PathBuf> = resolved
            .changed_dirs
            .iter()
            .map(|d| d.to_path_buf())
            .collect();
        assert_eq!(
            dirs,
            vec![PathBuf::from("/root"), PathBuf::from("/root/zeta")],
            "both the watched dir and the deeper one are invalidation targets"
        );
        assert!(!resolved.reload);
    }

    #[test]
    fn resolve_watch_batch_rescan_requests_a_full_reload() {
        let vfs = fixture_vfs();
        let resolved = resolve(
            &vfs,
            "/root",
            vec![
                event("/root/file2.txt", PathEventKind::Removed),
                event("", PathEventKind::Rescan),
            ],
        );
        assert!(resolved.reload, "Rescan cannot be applied incrementally");
        assert!(resolved.patches.is_empty());
        assert_eq!(
            resolved
                .changed_dirs
                .iter()
                .map(|d| d.to_path_buf())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("/root")]
        );
    }

    #[test]
    fn cache_hit_returns_stale_snapshot_and_promotes_it() {
        let mut cache = ListingCache::new(2);
        let a = snapshot_for("/a");
        let b = snapshot_for("/b");
        cache.insert(a.clone());
        cache.insert(b.clone());

        let hit = cache.get(Path::new("/a")).expect("hit");
        assert!(
            Arc::ptr_eq(&hit, &a),
            "hit returns the cached (stale) snapshot"
        );
        assert!(cache.get(Path::new("/missing")).is_none());

        // /a was promoted to MRU, so inserting /c evicts /b, not /a.
        cache.insert(snapshot_for("/c"));
        assert!(cache.get(Path::new("/a")).is_some());
        assert!(cache.get(Path::new("/b")).is_none(), "LRU evicted");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn cache_write_back_replaces_same_directory_entry() {
        let mut cache = ListingCache::new(4);
        cache.insert(snapshot_for("/a"));
        let refreshed = Arc::new(ListingSnapshot {
            generation: 7,
            ..(*snapshot_for("/a")).clone()
        });
        cache.insert(refreshed);
        assert_eq!(cache.len(), 1, "write-back replaces, never duplicates");
        assert_eq!(cache.get(Path::new("/a")).unwrap().generation, 7);
    }

    #[test]
    fn cache_invalidate_and_capacity() {
        let mut cache = ListingCache::default();
        assert!(cache.is_empty());
        cache.insert(snapshot_for("/a"));
        cache.invalidate(Path::new("/a"));
        assert!(cache.get(Path::new("/a")).is_none());

        for i in 0..20 {
            cache.insert(snapshot_for(&format!("/dir{i}")));
        }
        assert_eq!(cache.len(), ListingCache::DEFAULT_CAPACITY);
    }

    #[test]
    fn list_dir_surfaces_vfs_errors() {
        let vfs = fixture_vfs();
        vfs.set_error("/root", "boom");
        let result = block_on(list_dir(
            vfs as Arc<dyn Vfs>,
            Arc::from(PathBuf::from("/root")),
            SortSpec::default(),
            false,
            0,
        ));
        assert!(result.is_err());
    }
}
