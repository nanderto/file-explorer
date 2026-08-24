//! M6a cross-module check: search over a **real** tree through `RealVfs` —
//! `search_recursive`'s walk (`search.rs`) against real `read_dir`, and the
//! instant filter (`filter_snapshot`) against a real listing (`listing.rs`).
//! The unit tests drive `FakeVfs`, which cannot have a symlink node or an
//! unreadable directory; only a temp tree can show that the symlink policy and
//! the hidden-file rule hold against the actual filesystem.

use std::path::PathBuf;
use std::sync::Arc;

use fs_core::{
    RealVfs, SearchEvent, SearchQuery, Spawner, TestSpawner, Vfs, filter_snapshot, list_dir,
    search_recursive,
};
use futures::StreamExt as _;
use futures::executor::block_on;

/// A nested tree with a hidden file, a hidden directory, and (on unix) a
/// symlink that points back at the root — a loop a descending walk would never
/// escape.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("target-top.txt"), b"top").unwrap();
    std::fs::write(root.join("notes.md"), b"unrelated").unwrap();
    std::fs::write(root.join(".target-hidden.txt"), b"hidden").unwrap();

    std::fs::create_dir(root.join("a")).unwrap();
    std::fs::write(root.join("a").join("target-mid.txt"), b"mid").unwrap();
    std::fs::create_dir_all(root.join("a").join("deep").join("deeper")).unwrap();
    std::fs::write(
        root.join("a")
            .join("deep")
            .join("deeper")
            .join("target.txt"),
        b"buried",
    )
    .unwrap();

    std::fs::create_dir(root.join(".hidden-dir")).unwrap();
    std::fs::write(root.join(".hidden-dir").join("target-in-hidden.txt"), b"h").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(root, root.join("target-loop")).unwrap();

    dir
}

fn run(vfs: Arc<dyn Vfs>, root: PathBuf, query: &str, show_hidden: bool) -> Vec<SearchEvent> {
    let query = SearchQuery::new(query).expect("non-blank query");
    block_on(search_recursive(vfs, root, query, show_hidden).collect::<Vec<_>>())
}

fn hit_names(events: &[SearchEvent]) -> Vec<String> {
    let mut names: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            SearchEvent::Hit(entry) => Some(entry.name.to_string()),
            _ => None,
        })
        .collect();
    names.sort();
    names
}

#[test]
fn recursive_search_over_a_real_tree_honors_hidden_and_terminates_on_a_symlink_loop() {
    let temp = fixture();
    let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
    let vfs: Arc<dyn Vfs> = Arc::new(RealVfs::new(spawner));
    let root = temp.path().to_path_buf();

    // Hidden off: the dotfile and the whole hidden directory are invisible.
    // The symlink back to the root is reported by name but never descended —
    // if it were, this test would hang rather than fail.
    let events = run(vfs.clone(), root.clone(), "target", false);
    let mut expected = vec!["target-mid.txt", "target-top.txt", "target.txt"];
    if cfg!(unix) {
        expected.push("target-loop");
        expected.sort();
    }
    assert_eq!(hit_names(&events), expected);
    assert_eq!(events.last(), Some(&SearchEvent::Done), "the walk finished");

    // Every dot-prefixed entry appears once hidden files are shown.
    let events = run(vfs.clone(), root.clone(), "target", true);
    let names = hit_names(&events);
    assert!(names.contains(&".target-hidden.txt".to_string()));
    assert!(names.contains(&"target-in-hidden.txt".to_string()));

    // Nothing matched: still a clean finish, no hits.
    let events = run(vfs.clone(), root.clone(), "no-such-name", false);
    assert!(hit_names(&events).is_empty());
    assert_eq!(events.last(), Some(&SearchEvent::Done));

    // A directory that does not exist is surfaced, not fatal.
    let events = run(vfs.clone(), root.join("missing"), "target", false);
    assert!(matches!(events.first(), Some(SearchEvent::Skipped { .. })));
    assert_eq!(events.last(), Some(&SearchEvent::Done));
}

#[test]
fn instant_filter_agrees_with_the_recursive_walk_on_the_top_level() {
    let temp = fixture();
    let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
    let vfs: Arc<dyn Vfs> = Arc::new(RealVfs::new(spawner));

    let snapshot = block_on(list_dir(
        vfs.clone(),
        Arc::from(temp.path()),
        fs_core::SortSpec::default(),
        false,
        0,
    ))
    .expect("listing the real tree");

    let query = SearchQuery::new("TARGET").expect("non-blank");
    let filtered = filter_snapshot(&snapshot, &query);
    let mut filtered_names: Vec<String> = filtered
        .iter()
        .map(|id| {
            snapshot
                .entries
                .iter()
                .find(|entry| entry.path == id.0)
                .expect("filtered id came from the snapshot")
                .name
                .to_string()
        })
        .collect();
    filtered_names.sort();

    // The filter sees exactly the matching *direct children* — the same set the
    // recursive walk reports at depth 1, and nothing from the subtree.
    let mut expected = vec!["target-top.txt"];
    if cfg!(unix) {
        expected.push("target-loop");
        expected.sort();
    }
    assert_eq!(filtered_names, expected);
    assert!(!filtered_names.contains(&"target-mid.txt".to_string()));

    // A blank query is *no search*: the caller shows the unfiltered listing.
    assert!(SearchQuery::new("   ").is_none());
}
