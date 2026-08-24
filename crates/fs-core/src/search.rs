//! Search (ARCHITECTURE.md §9, M6): the toolbar's name filter over the loaded
//! listing, and the recursive streamed walk under the current folder.
//!
//! Two halves, deliberately different in cost:
//!
//! * [`filter_snapshot`] is **pure** — no I/O at all — so the UI may call it on
//!   every keystroke against an already-loaded [`ListingSnapshot`].
//! * [`search_recursive`] returns a stream of [`SearchEvent`]s and is driven by
//!   the caller: it does the directory reads only while it is polled, so the
//!   pane runs it on the background executor and cancels by dropping it. It
//!   never spawns work of its own, which is what makes drop-cancellation exact
//!   (nothing survives the stream).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::{BoxStream, FuturesUnordered};

use crate::entry::{EntryId, EntryKind, FileEntry};
use crate::listing::ListingSnapshot;
use crate::vfs::Vfs;

/// Case-insensitive substring match on the entry **name** — Explorer's default
/// (it does not match on path, contents, or metadata).
///
/// Case folding uses `str::to_lowercase` (full Unicode simple lowercasing), so
/// `Ä` matches `ä` and `Σ` matches `σ`. It deliberately stops there: there is
/// no Unicode normalization, so a decomposed `e` + U+0301 name does *not* match
/// a precomposed `é` needle. Normalizing every candidate name would cost an
/// allocation per row on every keystroke, and macOS stores NFD names
/// consistently enough that the two forms rarely mix inside one folder.
///
/// ASCII names (the overwhelming majority) are matched byte-wise with no
/// allocation at all; only a name containing non-ASCII pays for a lowercased
/// copy of itself. See [`SearchQuery::matches_name`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    /// The needle, pre-lowercased once at construction.
    needle: String,
    /// True when `needle` is pure ASCII — the no-allocation matching path.
    needle_is_ascii: bool,
    /// The trimmed text the user typed, for the UI ("No matches for …").
    text: String,
}

impl SearchQuery {
    /// Build a query from raw input text, trimming surrounding whitespace.
    ///
    /// Returns `None` for blank or whitespace-only input: an empty search is
    /// *no search*, and the caller shows the unfiltered listing rather than an
    /// empty result set.
    pub fn new(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        let needle = trimmed.to_lowercase();
        Some(Self {
            needle_is_ascii: needle.is_ascii(),
            needle,
            text: trimmed.to_string(),
        })
    }

    /// True when `name` contains the needle, case-insensitively.
    pub fn matches_name(&self, name: &str) -> bool {
        if name.is_ascii() {
            // A non-ASCII needle cannot occur inside an ASCII name, and an
            // ASCII needle is already lowercased — compare bytes in place.
            self.needle_is_ascii && ascii_contains_ignore_case(name, &self.needle)
        } else {
            name.to_lowercase().contains(&self.needle)
        }
    }

    /// The trimmed query text as typed, for display.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Case-insensitive substring search over ASCII text, allocation-free.
/// `needle` must already be lowercase (it always is: [`SearchQuery::new`]
/// lowercases once).
fn ascii_contains_ignore_case(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.len() > haystack.len() {
        return false;
    }
    let first = needle[0];
    for start in 0..=(haystack.len() - needle.len()) {
        if haystack[start].to_ascii_lowercase() != first {
            continue;
        }
        if haystack[start + 1..start + needle.len()]
            .iter()
            .zip(&needle[1..])
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return true;
        }
    }
    false
}

/// Filter an already-loaded listing, returning the matching entries' ids in the
/// snapshot's own (sorted) order.
///
/// **Pure**: no I/O, no stat, nothing off-thread — safe to call from the UI
/// thread inside a keystroke handler. Non-matching entries cost one
/// `matches_name` call and no allocation; only matches allocate (an `Arc`
/// clone into the result vector).
pub fn filter_snapshot(snapshot: &ListingSnapshot, query: &SearchQuery) -> Vec<EntryId> {
    snapshot
        .entries
        .iter()
        .filter(|entry| query.matches_name(&entry.name))
        .map(FileEntry::id)
        .collect()
}

/// What a recursive search reports as it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum SearchEvent {
    /// A matching entry (file, directory or symlink), reported as soon as the
    /// directory containing it has been read.
    Hit(FileEntry),
    /// Progress: directories visited so far, for the pane's status line.
    /// Coalesced to one event per [`PROGRESS_EVERY_DIRS`] directories, plus a
    /// final exact count immediately before [`SearchEvent::Done`].
    Progress { dirs_scanned: usize },
    /// A directory that could not be read (permission denied, vanished
    /// mid-walk) or was not descended into (depth cap) — surfaced, never
    /// fatal; the walk continues.
    Skipped { path: PathBuf, error: String },
    /// The walk finished on its own. A dropped stream ends *without* this.
    Done,
}

/// How many directory reads may be in flight at once.
///
/// Directory reads are latency-bound syscalls, and `RealVfs` puts each one on a
/// blocking executor thread, so a small amount of overlap hides per-call
/// latency (very visibly on network volumes) while a large amount would just
/// occupy the blocking pool that copies and thumbnails also share, and destroy
/// read locality on spinning disks. Eight is the smallest value that kept a
/// deep tree read-bound rather than latency-bound in practice.
pub const MAX_CONCURRENT_DIR_READS: usize = 8;

/// One [`SearchEvent::Progress`] per this many directories scanned. Progress is
/// a status-line string; reporting every directory would wake the UI thousands
/// of times a second for no visible difference.
pub const PROGRESS_EVERY_DIRS: usize = 16;

/// Deepest directory level below the search root that is read.
///
/// Symlinked directories are never descended (see [`search_recursive`]), which
/// rules out the ordinary way a tree becomes a cycle, and a self-repeating path
/// is caught much earlier by [`looks_like_a_directory_cycle`]. This cap is the
/// last resort behind both: real user trees are an order of magnitude
/// shallower, so it only ever fires on a pathological one, and when it does the
/// walk terminates and says so via [`SearchEvent::Skipped`] instead of running
/// forever.
pub const MAX_DEPTH: usize = 64;

/// Longest repeating path tail [`looks_like_a_directory_cycle`] looks for.
pub const MAX_CYCLE_PERIOD: usize = 8;

/// How many times that tail must repeat, back to back, before a directory is
/// treated as an alias of one of its own ancestors.
///
/// Two would already be suspicious, but `docs/img/docs/img` is a real path
/// somebody has; three consecutive repeats is not.
pub const CYCLE_REPEATS: usize = 3;

/// True when `path` ends in the same run of components repeated
/// [`CYCLE_REPEATS`] times — `…/Data/System/Volumes/Data/System/Volumes/Data`.
///
/// This is the cheap, portable half of cycle safety, and it is the half that
/// matters for the case [`MAX_DEPTH`] alone handles badly: a **real** directory
/// aliasing an ancestor (macOS firmlinks such as `/System/Volumes/Data`, some
/// network mounts) has no `EntryKind` to give it away, so the walk descends it
/// and every level of the loop costs a full re-walk of the aliased subtree.
/// Bounding only the depth turns one loop into `MAX_DEPTH / period` complete
/// re-walks of a whole volume — every file matched and reported once per lap.
/// Catching the repeat instead stops it after two laps, whatever `MAX_DEPTH` is.
///
/// Pure path arithmetic: no `stat`, and nothing platform-specific. The exact
/// fix is device+inode identity, which needs a new `Platform` seam — recorded
/// as a gap in `docs/AS_BUILT.md`.
pub fn looks_like_a_directory_cycle(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    for period in 1..=MAX_CYCLE_PERIOD {
        let needed = period * CYCLE_REPEATS;
        if components.len() < needed {
            break;
        }
        let tail = &components[components.len() - needed..];
        if tail.chunks(period).all(|lap| lap == &tail[..period]) {
            return true;
        }
    }
    false
}

/// Recursive search under `root`, streamed.
///
/// Breadth-first, so shallow hits — the ones the user most likely wants —
/// arrive first, with up to [`MAX_CONCURRENT_DIR_READS`] directory reads in
/// flight. Hits are yielded as they are found; nothing accumulates a result
/// list, and the only state that grows with the tree is the queue of *pending
/// directories* (breadth-first's inherent cost, one entry per directory in the
/// current and next levels, not one per file).
///
/// "Breadth-first" describes the **frontier**, which is FIFO, not the exact
/// interleaving of the output: the in-flight reads are unordered, so a
/// directory that comes back late does not hold back the hits of the seven
/// beside it (one stalled network directory used to stop the whole stream and
/// stop the in-flight set being topped up). Reads still start shallowest-first,
/// so shallow hits still arrive first whenever the reads cost the same.
///
/// * **Symlinks**: reported as hits by name, never descended into. A symlinked
///   directory is a leaf here, exactly as it is a leaf in the listing view,
///   which makes the walk cycle-free without a visited set (and stops one file
///   being reported twice through two aliases).
/// * **Cycles through real directories**: a directory whose path repeats its
///   own tail ([`looks_like_a_directory_cycle`]) is reported as
///   [`SearchEvent::Skipped`] and not read.
/// * **Depth**: directories deeper than [`MAX_DEPTH`] are reported as
///   [`SearchEvent::Skipped`] and not read — see the constant.
/// * **Errors**: an unreadable directory yields [`SearchEvent::Skipped`] and
///   the walk continues; a search never fails as a whole.
/// * **Hidden**: `show_hidden` mirrors the pane's setting; when off, hidden
///   entries are neither reported nor descended into.
/// * **Cancellation**: drop the stream. The walk only advances while polled,
///   so dropping it stops every read that has not started and abandons the
///   in-flight ones; no [`SearchEvent::Done`] is emitted and nothing is left
///   behind (the search writes nothing anywhere).
pub fn search_recursive(
    vfs: Arc<dyn Vfs>,
    root: PathBuf,
    query: SearchQuery,
    show_hidden: bool,
) -> BoxStream<'static, SearchEvent> {
    let walk = Walk {
        vfs,
        query,
        show_hidden,
        frontier: VecDeque::from([(root, 0)]),
        in_flight: FuturesUnordered::new(),
        pending: VecDeque::new(),
        dirs_scanned: 0,
        progress_reported: 0,
        finished: false,
    };
    futures::stream::unfold(walk, |mut walk| async move {
        walk.next_event().await.map(|event| (event, walk))
    })
    .boxed()
}

/// One completed directory read: the entries it produced, or why it produced
/// none.
struct DirRead {
    dir: PathBuf,
    depth: usize,
    outcome: Result<Vec<anyhow::Result<FileEntry>>, anyhow::Error>,
}

/// The breadth-first walk's state, advanced one event at a time by the stream.
struct Walk {
    vfs: Arc<dyn Vfs>,
    query: SearchQuery,
    show_hidden: bool,
    /// Directories still to read, with their depth below the root: FIFO, which
    /// is what makes the *reads* start breadth-first.
    frontier: VecDeque<(PathBuf, usize)>,
    /// The reads in flight. **Unordered** on purpose: one slow directory must
    /// not hold back the hits of the ones beside it, nor stop the set being
    /// topped up (see [`search_recursive`]).
    in_flight: FuturesUnordered<BoxFuture<'static, DirRead>>,
    /// Events produced by the last directory read, not yet yielded.
    pending: VecDeque<SearchEvent>,
    dirs_scanned: usize,
    /// `dirs_scanned` as of the last [`SearchEvent::Progress`].
    progress_reported: usize,
    finished: bool,
}

impl Walk {
    /// Yield the next event, reading directories as needed. `None` ends the
    /// stream (only after [`SearchEvent::Done`]).
    async fn next_event(&mut self) -> Option<SearchEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.finished {
                return None;
            }
            self.fill_in_flight();
            match self.in_flight.next().await {
                Some(read) => self.absorb(read),
                None => {
                    // Nothing queued and nothing in flight: the tree is walked.
                    if self.dirs_scanned != self.progress_reported {
                        self.pending.push_back(SearchEvent::Progress {
                            dirs_scanned: self.dirs_scanned,
                        });
                        self.progress_reported = self.dirs_scanned;
                    }
                    self.pending.push_back(SearchEvent::Done);
                    self.finished = true;
                }
            }
        }
    }

    /// Start reads from the frontier until the concurrency bound is reached.
    fn fill_in_flight(&mut self) {
        while self.in_flight.len() < MAX_CONCURRENT_DIR_READS {
            let Some((dir, depth)) = self.frontier.pop_front() else {
                return;
            };
            let vfs = self.vfs.clone();
            self.in_flight.push(Box::pin(async move {
                let outcome = match vfs.read_dir(&dir).await {
                    Ok(stream) => Ok(stream.collect::<Vec<_>>().await),
                    Err(error) => Err(error),
                };
                DirRead {
                    dir,
                    depth,
                    outcome,
                }
            }));
        }
    }

    /// Turn a completed directory read into events, and queue its subdirectories.
    fn absorb(&mut self, read: DirRead) {
        let entries = match read.outcome {
            // Counted only once the read succeeded: a directory that could not
            // be read is `Skipped`, and counting it as scanned too would have
            // the status line report more folders searched than it managed to
            // open.
            Ok(entries) => {
                self.dirs_scanned += 1;
                entries
            }
            Err(error) => {
                self.pending.push_back(SearchEvent::Skipped {
                    path: read.dir,
                    error: error.to_string(),
                });
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                // A single unreadable child (stat failed): report it against
                // the directory and keep going.
                Err(error) => {
                    self.pending.push_back(SearchEvent::Skipped {
                        path: read.dir.clone(),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            if entry.hidden && !self.show_hidden {
                continue;
            }
            if self.query.matches_name(&entry.name) {
                self.pending.push_back(SearchEvent::Hit(entry.clone()));
            }
            // Real directories only: a symlinked directory is a leaf, which is
            // what keeps the walk cycle-free.
            if matches!(entry.kind, EntryKind::Dir) {
                let depth = read.depth + 1;
                if looks_like_a_directory_cycle(&entry.path) {
                    self.pending.push_back(SearchEvent::Skipped {
                        path: entry.path.to_path_buf(),
                        error: "not searched: looks like a directory cycle".to_string(),
                    });
                } else if depth > MAX_DEPTH {
                    self.pending.push_back(SearchEvent::Skipped {
                        path: entry.path.to_path_buf(),
                        error: format!("not searched: deeper than {MAX_DEPTH} levels"),
                    });
                } else {
                    self.frontier.push_back((entry.path.to_path_buf(), depth));
                }
            }
        }
        if self.dirs_scanned - self.progress_reported >= PROGRESS_EVERY_DIRS {
            self.pending.push_back(SearchEvent::Progress {
                dirs_scanned: self.dirs_scanned,
            });
            self.progress_reported = self.dirs_scanned;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{EntryMeta, TargetKind};
    use crate::exec::TestSpawner;
    use crate::listing::list_dir;
    use crate::sort::SortSpec;
    use crate::vfs::{
        CreateOptions, FakeVfs, ProgressFn, RemoveOptions, RenameOptions, TrashId,
        TrashRestoreError, VolumeKey,
    };
    use crate::watcher::{PathEvent, WatchGuard};
    use anyhow::Result;
    use async_trait::async_trait;
    use futures::executor::block_on;
    use std::path::Path;
    use std::time::Duration;

    // -- SearchQuery ------------------------------------------------------

    #[test]
    fn blank_and_whitespace_queries_are_none() {
        assert!(SearchQuery::new("").is_none());
        assert!(SearchQuery::new("   ").is_none());
        assert!(SearchQuery::new("\t \n").is_none());
        assert!(SearchQuery::new("a").is_some());
    }

    #[test]
    fn query_text_is_the_trimmed_input() {
        let query = SearchQuery::new("  Report  ").expect("non-blank");
        assert_eq!(query.text(), "Report");
    }

    #[test]
    fn matching_is_case_insensitive_substring_on_the_name() {
        let query = SearchQuery::new("REP").expect("non-blank");
        assert!(query.matches_name("report.pdf"));
        assert!(query.matches_name("Q1-REPORT.pdf"));
        assert!(query.matches_name("prep"));
        assert!(!query.matches_name("readme.md"));
        // The needle is matched against the name only — never the path.
        assert!(!SearchQuery::new("tmp").unwrap().matches_name("report.pdf"));
    }

    #[test]
    fn needle_longer_than_name_never_matches() {
        let query = SearchQuery::new("annual-report").expect("non-blank");
        assert!(!query.matches_name("rep"));
    }

    #[test]
    fn non_ascii_names_and_needles_fold_case() {
        let query = SearchQuery::new("ÜNÏ").expect("non-blank");
        assert!(query.matches_name("Ünïcode Ärchive.txt"));
        assert!(query.matches_name("ünïx"));
        assert!(!query.matches_name("unicode.txt"), "no accent stripping");
        // An ASCII name cannot contain a non-ASCII needle.
        assert!(!query.matches_name("plain.txt"));
        // ...and an ASCII needle still matches inside a non-ASCII name.
        let ascii = SearchQuery::new("rchive").expect("non-blank");
        assert!(ascii.matches_name("Ünïcode Ärchive.txt"));
    }

    // -- filter_snapshot --------------------------------------------------

    fn snapshot_of(vfs: &Arc<FakeVfs>, dir: &str) -> ListingSnapshot {
        block_on(list_dir(
            vfs.clone() as Arc<dyn Vfs>,
            Arc::from(Path::new(dir)),
            SortSpec::default(),
            false,
            0,
        ))
        .expect("listing")
    }

    fn names(snapshot: &ListingSnapshot, ids: &[EntryId]) -> Vec<String> {
        ids.iter()
            .map(|id| {
                snapshot
                    .entries
                    .iter()
                    .find(|e| e.path == id.0)
                    .expect("id belongs to the snapshot")
                    .name
                    .to_string()
            })
            .collect()
    }

    fn listing_fixture() -> (Arc<FakeVfs>, ListingSnapshot) {
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        vfs.insert_tree(
            "/work",
            serde_json::json!({
                "Report.pdf": "a",
                "budget-report.xlsx": "b",
                "notes.txt": "c",
                "reports": {},
            }),
        );
        let snapshot = snapshot_of(&vfs, "/work");
        (vfs, snapshot)
    }

    #[test]
    fn filter_snapshot_keeps_matches_in_snapshot_order() {
        let (_vfs, snapshot) = listing_fixture();
        let ids = filter_snapshot(&snapshot, &SearchQuery::new("report").unwrap());
        // Folders-first natural sort: `reports` precedes the two files.
        assert_eq!(
            names(&snapshot, &ids),
            vec!["reports", "budget-report.xlsx", "Report.pdf"]
        );
        // The order is the snapshot's own, not the match order.
        let snapshot_order: Vec<String> = snapshot
            .entries
            .iter()
            .filter(|e| e.name.to_lowercase().contains("report"))
            .map(|e| e.name.to_string())
            .collect();
        assert_eq!(names(&snapshot, &ids), snapshot_order);
    }

    #[test]
    fn filter_snapshot_hit_miss_all_and_none() {
        let (_vfs, snapshot) = listing_fixture();
        assert_eq!(
            names(&snapshot, &filter_snapshot(&snapshot, &q("notes"))),
            vec!["notes.txt"]
        );
        assert!(filter_snapshot(&snapshot, &q("nothing-here")).is_empty());
        // A needle every name contains matches everything, in listing order.
        let all = filter_snapshot(&snapshot, &q("e"));
        assert_eq!(all.len(), snapshot.entries.len());
        assert_eq!(
            names(&snapshot, &all),
            snapshot
                .entries
                .iter()
                .map(|e| e.name.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn filter_snapshot_of_an_empty_listing_is_empty() {
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        vfs.insert_tree("/empty", serde_json::json!({}));
        let snapshot = snapshot_of(&vfs, "/empty");
        assert!(filter_snapshot(&snapshot, &q("anything")).is_empty());
    }

    fn q(text: &str) -> SearchQuery {
        SearchQuery::new(text).expect("non-blank test query")
    }

    // -- search_recursive -------------------------------------------------

    fn collect_all(stream: BoxStream<'static, SearchEvent>) -> Vec<SearchEvent> {
        block_on(stream.collect::<Vec<_>>())
    }

    fn hit_names(events: &[SearchEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                SearchEvent::Hit(entry) => Some(entry.name.to_string()),
                _ => None,
            })
            .collect()
    }

    fn tree_fixture() -> Arc<FakeVfs> {
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        vfs.insert_tree(
            "/root",
            serde_json::json!({
                "notes.txt": "x",
                "a": {
                    "target-shallow.txt": "x",
                    "deep": { "deeper": { "target-buried.txt": "x" } },
                },
                "b": { "target-mid.txt": "x" },
                ".hidden": { "target-hidden.txt": "x" },
                "target-top.txt": "x",
            }),
        );
        vfs
    }

    #[test]
    fn finds_nested_hits_and_finishes_with_done() {
        let vfs = tree_fixture();
        let events = collect_all(search_recursive(
            vfs as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("target"),
            false,
        ));
        let mut found = hit_names(&events);
        found.sort();
        assert_eq!(
            found,
            vec![
                "target-buried.txt",
                "target-mid.txt",
                "target-shallow.txt",
                "target-top.txt",
            ]
        );
        assert_eq!(events.last(), Some(&SearchEvent::Done));
        // The final Progress carries the exact directory count: /root, a, b,
        // a/deep, a/deep/deeper (`.hidden` is skipped entirely).
        let last_progress = events
            .iter()
            .filter_map(|e| match e {
                SearchEvent::Progress { dirs_scanned } => Some(*dirs_scanned),
                _ => None,
            })
            .next_back();
        assert_eq!(last_progress, Some(5));
    }

    #[test]
    fn hits_arrive_breadth_first() {
        let vfs = tree_fixture();
        let events = collect_all(search_recursive(
            vfs as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("target"),
            false,
        ));
        assert_eq!(
            hit_names(&events),
            vec![
                // depth 1 (from /root itself)
                "target-top.txt",
                // depth 2 (/root/a, then /root/b — frontier order)
                "target-shallow.txt",
                "target-mid.txt",
                // depth 4
                "target-buried.txt",
            ]
        );
    }

    #[test]
    fn show_hidden_controls_hidden_entries_and_hidden_directories() {
        let vfs = tree_fixture();
        let hidden_off = collect_all(search_recursive(
            vfs.clone() as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("target-hidden"),
            false,
        ));
        assert!(
            hit_names(&hidden_off).is_empty(),
            "hidden dir not descended"
        );

        let hidden_on = collect_all(search_recursive(
            vfs as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("target-hidden"),
            true,
        ));
        assert_eq!(hit_names(&hidden_on), vec!["target-hidden.txt"]);
    }

    #[test]
    fn unreadable_directory_is_skipped_and_the_walk_continues() {
        let vfs = tree_fixture();
        vfs.set_error("/root/a", "permission denied");
        let events = collect_all(search_recursive(
            vfs as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("target"),
            false,
        ));
        let skipped: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SearchEvent::Skipped { path, error } => Some((path.clone(), error.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            skipped,
            vec![(PathBuf::from("/root/a"), "permission denied".to_string())]
        );
        // Its siblings are still searched, and the walk still completes.
        let mut found = hit_names(&events);
        found.sort();
        assert_eq!(found, vec!["target-mid.txt", "target-top.txt"]);
        assert_eq!(events.last(), Some(&SearchEvent::Done));
        // A directory that could not be opened is not one of the folders
        // searched: /root and /root/b were, /root/a was skipped.
        let last_progress = events
            .iter()
            .filter_map(|e| match e {
                SearchEvent::Progress { dirs_scanned } => Some(*dirs_scanned),
                _ => None,
            })
            .next_back();
        assert_eq!(last_progress, Some(2), "the unreadable dir is not scanned");
    }

    #[test]
    fn a_real_directory_aliasing_its_parent_is_skipped_not_re_walked() {
        // The firmlink shape: `/root/alias` is a plain directory whose contents
        // are `/root`'s, so it reappears one level down forever and no
        // `EntryKind` check can tell. Bounding only the depth would re-walk the
        // whole tree `MAX_DEPTH` times over.
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        vfs.insert_dir("/root");
        vfs.insert_file("/root/target.txt", 1);
        let probe = Arc::new(LoopVfs::new(vfs, "/root/alias").presented_as_a_real_dir());

        let events = collect_all(search_recursive(
            probe.clone() as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("target"),
            false,
        ));
        assert_eq!(events.last(), Some(&SearchEvent::Done), "terminates");
        let reads = probe.read_dirs().len();
        assert!(
            reads <= 4,
            "the loop was cut after a couple of laps, not {MAX_DEPTH} of them: {:?}",
            probe.read_dirs()
        );
        // The lap that was refused says so, rather than vanishing.
        assert!(
            events.iter().any(|e| matches!(
                e,
                SearchEvent::Skipped { error, .. } if error.contains("cycle")
            )),
            "the alias is reported as skipped: {events:?}"
        );
        // Each lap re-reports the file under a different alias path; the point
        // is that there are a handful of them, not `MAX_DEPTH` worth.
        assert!(hit_names(&events).len() <= 4, "{:?}", hit_names(&events));
    }

    #[test]
    fn cycle_detection_needs_the_tail_to_repeat_three_times() {
        assert!(!looks_like_a_directory_cycle(Path::new("/a/b/a/b")));
        assert!(looks_like_a_directory_cycle(Path::new("/a/b/a/b/a/b")));
        assert!(looks_like_a_directory_cycle(Path::new("/x/d/d/d")));
        assert!(!looks_like_a_directory_cycle(Path::new("/x/d/d")));
        assert!(!looks_like_a_directory_cycle(Path::new(
            "/System/Volumes/Data/Users/me"
        )));
        assert!(looks_like_a_directory_cycle(Path::new(
            "/System/Volumes/Data/System/Volumes/Data/System/Volumes/Data"
        )));
    }

    #[test]
    fn a_slow_directory_does_not_hold_back_the_hits_beside_it() {
        // `/root/a-slow` is started *first* (it sorts first, so it heads the
        // frontier) but completes last, because it yields many times before
        // producing anything. With an *ordered* in-flight set its siblings'
        // hits — already in hand — are buffered behind it, and no further read
        // can start either.
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        vfs.insert_dir("/root");
        vfs.insert_dir("/root/a-slow");
        vfs.insert_file("/root/a-slow/target-slow.txt", 1);
        for i in 0..3 {
            let dir = PathBuf::from(format!("/root/fast-{i}"));
            vfs.insert_dir(&dir);
            vfs.insert_file(dir.join("target.txt"), 1);
        }
        let probe = Arc::new(LoopVfs::new(vfs, "/nonexistent").slow_read("/root/a-slow", 64));

        let events = collect_all(search_recursive(
            probe as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("target"),
            false,
        ));
        let hits = hit_names(&events);
        let slow_at = hits
            .iter()
            .position(|n| n == "target-slow.txt")
            .expect("the slow directory is still searched");
        assert_eq!(
            slow_at,
            hits.len() - 1,
            "every sibling's hit arrived before the slow directory's: {hits:?}"
        );
        assert_eq!(events.last(), Some(&SearchEvent::Done));
    }

    #[test]
    fn unreadable_root_yields_skipped_then_done() {
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        let events = collect_all(search_recursive(
            vfs as Arc<dyn Vfs>,
            PathBuf::from("/gone"),
            q("anything"),
            false,
        ));
        assert!(matches!(events[0], SearchEvent::Skipped { .. }));
        assert_eq!(events.last(), Some(&SearchEvent::Done));
    }

    #[test]
    fn symlinked_directories_are_hits_but_are_never_descended() {
        // /root/loop is a symlink-to-dir whose contents are /root's contents
        // (LoopVfs), so descending into it would recurse forever.
        let vfs = tree_fixture();
        let probe = Arc::new(LoopVfs::new(vfs, "/root/loop"));
        let events = collect_all(search_recursive(
            probe.clone() as Arc<dyn Vfs>,
            PathBuf::from("/root"),
            q("loop"),
            false,
        ));
        assert_eq!(hit_names(&events), vec!["loop"], "reported by name");
        assert_eq!(events.last(), Some(&SearchEvent::Done), "terminates");
        assert!(
            !probe.read_dirs().iter().any(|p| p.ends_with("loop")),
            "the symlinked directory was never read: {:?}",
            probe.read_dirs()
        );
    }

    #[test]
    fn directories_below_the_depth_cap_are_skipped_not_descended() {
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        vfs.insert_dir("/deep");
        let mut path = PathBuf::from("/deep");
        // Distinct component names at every level, so this exercises the depth
        // cap and *only* the depth cap — a chain of identically named
        // directories would trip the cycle heuristic three levels in.
        for level in 0..(MAX_DEPTH + 2) {
            path.push(format!("d{level}"));
            vfs.insert_dir(&path);
        }
        let deepest_file = path.join("target.txt");
        vfs.insert_file(&deepest_file, 1);

        let events = collect_all(search_recursive(
            vfs as Arc<dyn Vfs>,
            PathBuf::from("/deep"),
            q("target"),
            false,
        ));
        assert!(
            hit_names(&events).is_empty(),
            "the file below the cap is never reached"
        );
        let skipped: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SearchEvent::Skipped { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(skipped.len(), 1, "exactly the first over-deep directory");
        assert_eq!(
            skipped[0].components().count() - 2,
            MAX_DEPTH + 1,
            "the skipped directory sits one level past the cap"
        );
        assert_eq!(events.last(), Some(&SearchEvent::Done));
    }

    #[test]
    fn dropping_the_stream_stops_the_walk() {
        // A tree wide enough that a complete walk would read many directories.
        let vfs = FakeVfs::new(Arc::new(TestSpawner::new()));
        vfs.insert_dir("/wide");
        for i in 0..200 {
            let dir = PathBuf::from(format!("/wide/dir-{i:03}"));
            vfs.insert_dir(&dir);
            vfs.insert_file(dir.join("target.txt"), 1);
        }
        let probe = Arc::new(LoopVfs::new(vfs, "/nonexistent"));

        let mut stream = search_recursive(
            probe.clone() as Arc<dyn Vfs>,
            PathBuf::from("/wide"),
            q("target"),
            false,
        );
        // Take a couple of events, then cancel by dropping the stream.
        assert!(matches!(
            block_on(stream.next()),
            Some(SearchEvent::Hit(_)) | Some(SearchEvent::Progress { .. })
        ));
        let _ = block_on(stream.next());
        let reads_at_drop = probe.read_dirs().len();
        drop(stream);

        assert!(
            reads_at_drop < 30,
            "cancelled early, not after walking 201 directories: {reads_at_drop}"
        );
        // Nothing is spawned, so no read can happen after the drop. Give any
        // hypothetical background work a real chance to show up.
        std::thread::yield_now();
        assert_eq!(
            probe.read_dirs().len(),
            reads_at_drop,
            "no directory was read after the stream was dropped"
        );
    }

    // -- test doubles -----------------------------------------------------

    /// A [`Vfs`] wrapper that records every `read_dir` path and can present one
    /// path as a **symlinked directory whose contents are the search root's**
    /// — a cycle that a walk which descended into symlinks would never escape.
    /// (`FakeVfs` fixtures have no symlink nodes, so the loop is synthesised
    /// here, the way the app's tests wrap a recording `Platform`.)
    struct LoopVfs {
        inner: Arc<FakeVfs>,
        /// Path presented as an alias of its own parent.
        link: PathBuf,
        /// What the alias reports itself as. A `Symlink { target_kind: Dir }`
        /// is the ordinary cycle, which the walk refuses to descend by kind
        /// alone; a plain [`EntryKind::Dir`] is the firmlink case, which no
        /// kind check can see.
        kind: EntryKind,
        /// A directory whose read yields `usize` times before producing
        /// anything — a stalled network mount, deterministically.
        slow: Option<(PathBuf, usize)>,
        read_dirs: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl LoopVfs {
        fn new(inner: Arc<FakeVfs>, link: impl Into<PathBuf>) -> Self {
            Self {
                inner,
                link: link.into(),
                kind: EntryKind::Symlink {
                    target_kind: TargetKind::Dir,
                },
                slow: None,
                read_dirs: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// The same alias, presented as a real directory.
        fn presented_as_a_real_dir(mut self) -> Self {
            self.kind = EntryKind::Dir;
            self
        }

        /// Make one directory's read complete only after `yields` polls.
        fn slow_read(mut self, dir: impl Into<PathBuf>, yields: usize) -> Self {
            self.slow = Some((dir.into(), yields));
            self
        }

        fn read_dirs(&self) -> Vec<PathBuf> {
            self.read_dirs.lock().unwrap().clone()
        }

        fn link_name(&self) -> &std::ffi::OsStr {
            self.link.file_name().expect("link has a name")
        }

        fn aliased_parent(&self) -> &Path {
            self.link.parent().expect("link has a parent")
        }
    }

    #[async_trait]
    impl Vfs for LoopVfs {
        async fn read_dir(&self, path: &Path) -> Result<BoxStream<'static, Result<FileEntry>>> {
            self.read_dirs.lock().unwrap().push(path.to_path_buf());
            if let Some((slow, yields)) = &self.slow
                && slow == path
            {
                let mut left = *yields;
                std::future::poll_fn(move |task_cx| {
                    if left == 0 {
                        return std::task::Poll::Ready(());
                    }
                    left -= 1;
                    task_cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                })
                .await;
            }
            // Reading *through* the alias loops straight back to the aliased
            // directory's contents, however many aliases deep the path is
            // (`/root/loop/loop/…`), so a descending walk would never
            // terminate.
            let mut target = path.to_path_buf();
            let mut through_alias = false;
            while target.file_name() == Some(self.link_name()) {
                target.pop();
                through_alias = true;
            }
            if through_alias {
                target = self.aliased_parent().to_path_buf();
            }
            let mut entries: Vec<Result<FileEntry>> =
                self.inner.read_dir(&target).await?.collect().await;
            if target == self.aliased_parent() {
                let meta = EntryMeta {
                    kind: self.kind,
                    size: 0,
                    modified: std::time::UNIX_EPOCH,
                    created: None,
                    hidden: false,
                };
                // Named under the directory actually being read, so the alias
                // reappears one level down every time it is entered.
                entries.push(Ok(
                    meta.into_entry(Arc::from(path.join(self.link_name()).as_path()))
                ));
            }
            Ok(futures::stream::iter(entries).boxed())
        }

        async fn metadata(&self, path: &Path) -> Result<Option<EntryMeta>> {
            self.inner.metadata(path).await
        }
        async fn create_dir(&self, path: &Path) -> Result<()> {
            self.inner.create_dir(path).await
        }
        async fn create_file(&self, path: &Path, opts: CreateOptions) -> Result<()> {
            self.inner.create_file(path, opts).await
        }
        async fn copy(&self, from: &Path, to: &Path, on_progress: ProgressFn) -> Result<()> {
            self.inner.copy(from, to, on_progress).await
        }
        async fn rename(&self, from: &Path, to: &Path, opts: RenameOptions) -> Result<()> {
            self.inner.rename(from, to, opts).await
        }
        async fn remove(&self, path: &Path, opts: RemoveOptions) -> Result<()> {
            self.inner.remove(path, opts).await
        }
        async fn trash(&self, path: &Path) -> Result<TrashId> {
            self.inner.trash(path).await
        }
        async fn restore(&self, id: TrashId) -> Result<PathBuf, TrashRestoreError> {
            self.inner.restore(id).await
        }
        async fn load(&self, path: &Path) -> Result<Vec<u8>> {
            self.inner.load(path).await
        }
        async fn atomic_write(&self, path: &Path, data: Vec<u8>) -> Result<()> {
            self.inner.atomic_write(path, data).await
        }
        fn volume_key(&self, path: &Path) -> VolumeKey {
            self.inner.volume_key(path)
        }
        async fn free_space(&self, path: &Path) -> Result<u64> {
            self.inner.free_space(path).await
        }
        fn watch(
            &self,
            path: &Path,
            latency: Duration,
        ) -> (BoxStream<'static, Vec<PathEvent>>, WatchGuard) {
            self.inner.watch(path, latency)
        }
        fn is_fake(&self) -> bool {
            true
        }
    }
}
