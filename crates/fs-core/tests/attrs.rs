//! M5 cross-module check: the info panel's three ingredients agree on one real
//! tree — a directory listing through the `Vfs` (ARCHITECTURE.md §6
//! `listing.rs`), the multi-selection summary over those rows (`attrs.rs`), and
//! the per-item attributes from the `Platform` seam (`platform/`). Unit tests
//! cover each in isolation; only an integration test can catch the two halves
//! disagreeing about the *same* file.
//!
//! The `Platform` half runs against `MacPlatform` on macOS (real `lstat` +
//! Foundation) and against `StubPlatform` everywhere else, so the file is
//! meaningful on every development machine per CLAUDE.md.

use std::sync::Arc;

use fs_core::{
    Platform, SelectionSummary, Spawner, TestSpawner, Vfs, is_previewable_entry, list_dir,
    summarize,
};
use futures::executor::block_on;

#[cfg(target_os = "macos")]
fn platform(spawner: Arc<dyn Spawner>) -> Arc<dyn Platform> {
    Arc::new(fs_core::MacPlatform::new(spawner))
}

#[cfg(not(target_os = "macos"))]
fn platform(_spawner: Arc<dyn Spawner>) -> Arc<dyn Platform> {
    Arc::new(fs_core::StubPlatform::new())
}

/// `photo.png`, `notes.md`, `blob.bin` and a `sub/` directory in a temp tree.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("photo.png"), vec![0u8; 300]).unwrap();
    std::fs::write(dir.path().join("notes.md"), b"# notes").unwrap();
    std::fs::write(dir.path().join("blob.bin"), vec![0u8; 40]).unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("nested.txt"), b"nested").unwrap();
    dir
}

#[test]
fn listing_summary_preview_gate_and_attrs_agree_on_one_real_tree() {
    let temp = fixture();
    let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
    let vfs: Arc<dyn Vfs> = Arc::new(fs_core::RealVfs::new(spawner.clone()));
    let platform = platform(spawner);

    let snapshot = block_on(list_dir(
        vfs.clone(),
        Arc::from(temp.path()),
        fs_core::SortSpec::default(),
        false,
        0,
    ))
    .expect("listing");
    let entries = snapshot.entries.as_ref();
    assert_eq!(entries.len(), 4, "3 files + 1 dir: {entries:?}");

    // The summary counts the tree as the panel will show it: the folder is not
    // a file, and its own inode size is not added to the file total.
    assert_eq!(
        summarize(entries.iter()),
        SelectionSummary {
            files: 3,
            dirs: 1,
            total_size: 300 + 7 + 40,
        }
    );

    // The preview gate agrees with the same rows: the image and the markdown
    // are previewable, the opaque blob and the directory are not.
    let previewable: Vec<&str> = entries
        .iter()
        .filter(|entry| is_previewable_entry(entry))
        .map(|entry| entry.name.as_ref())
        .collect();
    assert_eq!(previewable, ["notes.md", "photo.png"]);

    // And every row has attributes, keyed by the very path the listing produced.
    for entry in entries {
        let attrs = block_on(platform.file_attrs(&entry.path))
            .unwrap_or_else(|e| panic!("attrs for {}: {e:#}", entry.path.display()));
        let perms = attrs
            .perms
            .unwrap_or_else(|| panic!("no mode for {}", entry.path.display()));
        assert_eq!(perms.octal().len(), 3, "{}", entry.path.display());
        assert_eq!(perms.symbolic().len(), 9, "{}", entry.path.display());
        assert!(
            attrs.owner.is_some_and(|owner| !owner.is_empty()),
            "no owner for {}",
            entry.path.display()
        );
        // `locked` is deliberately *not* asserted against a value here:
        // `StubPlatform` derives it (and `extension_hidden`) from a hash of the
        // path, and these paths carry `tempfile`'s random suffix, so any fixed
        // expectation is a coin flip on the machines that run the stub. What is
        // portable is that the flag is a *function of the path* — the real
        // `UF_IMMUTABLE` read is covered by `MacPlatform`'s own tests.
        let again = block_on(platform.file_attrs(&entry.path)).expect("attrs, again");
        assert_eq!(
            again.locked,
            attrs.locked,
            "{} reported two different locked flags",
            entry.path.display()
        );
    }
}
