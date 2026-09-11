//! M7d-b cross-module check: `resolve_locations` over a whole tree, which is
//! where the unit tests stop being enough. Those cover the pure halves —
//! the fixed candidate table and the OneDrive name rule. What only a tree can
//! show is the part that decides what a person actually sees: the stat that
//! drops a location this machine does not have, the `read_dir` that finds a
//! tenant-suffixed sync root, and the interleaved order the two produce
//! together.
//!
//! `FakeVfs` rather than a temp tree: the interesting fixtures here are
//! *absences* (iCloud switched off, no OneDrive, an unreadable home), and a
//! declarative tree states those far more legibly than a sequence of
//! `create_dir` calls. `search.rs` makes the opposite trade for the opposite
//! reason — it needs symlinks and permission bits, which `FakeVfs` has no way
//! to express.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_core::{FakeVfs, Location, LocationKind, Spawner, TestSpawner, Vfs, resolve_locations};
use futures::executor::block_on;
use serde_json::json;

const HOME: &str = "/Users/me";

/// A Mac with everything switched on: iCloud Drive, one OneDrive tenant, a
/// Trash, and a `/Network` mount.
fn full_mac() -> Arc<FakeVfs> {
    let fake = FakeVfs::new(Arc::new(TestSpawner::new()) as Arc<dyn Spawner>);
    fake.insert_tree(
        HOME,
        json!({
            "Library": {
                "Mobile Documents": {
                    "com~apple~CloudDocs": { "notes.txt": "synced" }
                }
            },
            "OneDrive - BidOne Ltd": { "work.docx": "x" },
            "Desktop": {},
            "Documents": {},
            ".Trash": {},
        }),
    );
    fake.insert_tree("/Network", json!({ "Servers": {} }));
    fake
}

fn kinds(locations: &[Location]) -> Vec<LocationKind> {
    locations.iter().map(|l| l.kind).collect()
}

fn names(locations: &[Location]) -> Vec<&str> {
    locations.iter().map(|l| l.name.as_str()).collect()
}

/// The headline case, and the one that pins **order**: clouds first (iCloud,
/// then the tenant roots beside it), then the local rows. Order is the whole
/// point of a sidebar section, so it is asserted as a sequence rather than as
/// a set.
#[test]
fn a_fully_equipped_mac_lists_every_location_in_order() {
    let fake = full_mac();
    let found = block_on(resolve_locations(&*fake, Path::new(HOME)));
    assert_eq!(
        kinds(&found),
        vec![
            LocationKind::ICloudDrive,
            LocationKind::OneDrive,
            LocationKind::Home,
            LocationKind::Network,
            LocationKind::Trash,
        ],
    );
    assert_eq!(
        names(&found),
        vec![
            "iCloud Drive",
            "OneDrive - BidOne Ltd",
            "me",
            "Network",
            "Trash"
        ],
    );
}

/// The reason this is async and stat-driven rather than a const table: a Mac
/// with iCloud Drive switched off has no `com~apple~CloudDocs` folder at all,
/// and a row pointing at nothing navigates to an error.
#[test]
fn icloud_switched_off_produces_no_icloud_row() {
    let fake = FakeVfs::new(Arc::new(TestSpawner::new()) as Arc<dyn Spawner>);
    fake.insert_tree(HOME, json!({ "Desktop": {}, ".Trash": {} }));
    let found = block_on(resolve_locations(&*fake, Path::new(HOME)));
    assert!(
        !kinds(&found).contains(&LocationKind::ICloudDrive),
        "an absent iCloud folder must not be offered: {:?}",
        names(&found),
    );
    // ...and the rows that *do* exist are unaffected by the one that doesn't.
    assert_eq!(
        kinds(&found),
        vec![LocationKind::Home, LocationKind::Trash],
        "no /Network in this fixture either",
    );
}

/// A machine signed into two organizations genuinely has two sync roots, and
/// both are real folders a person needs to reach. They keep listing order.
#[test]
fn every_onedrive_tenant_gets_its_own_row() {
    let fake = FakeVfs::new(Arc::new(TestSpawner::new()) as Arc<dyn Spawner>);
    fake.insert_tree(
        HOME,
        json!({
            "OneDrive - Alpha Ltd": {},
            "OneDrive - Beta Inc": {},
            // The trap: the user's own folder that merely starts the same way.
            "OneDriveBackups": {},
        }),
    );
    let found = block_on(resolve_locations(&*fake, Path::new(HOME)));
    let drives: Vec<&str> = found
        .iter()
        .filter(|l| l.kind == LocationKind::OneDrive)
        .map(|l| l.name.as_str())
        .collect();
    assert_eq!(drives, vec!["OneDrive - Alpha Ltd", "OneDrive - Beta Inc"]);
    assert!(
        !names(&found).contains(&"OneDriveBackups"),
        "a lookalike folder must not be claimed as a sync root",
    );
}

/// Failing to read home costs the OneDrive rows and nothing else — the fixed
/// candidates are stat'ed independently, so a home that cannot be listed still
/// yields a usable sidebar rather than an empty one.
#[test]
fn an_unreadable_home_still_yields_the_fixed_locations() {
    let fake = FakeVfs::new(Arc::new(TestSpawner::new()) as Arc<dyn Spawner>);
    // /Network exists; home does not, so `read_dir` of it fails.
    fake.insert_tree("/Network", json!({ "Servers": {} }));
    let found = block_on(resolve_locations(&*fake, Path::new("/Users/ghost")));
    assert_eq!(kinds(&found), vec![LocationKind::Network]);
}

/// Every returned row must be navigable — that is the entire contract. A row
/// whose path does not resolve is worse than a missing row, because it looks
/// like a working affordance.
#[test]
fn every_returned_location_actually_exists() {
    let fake = full_mac();
    let found = block_on(resolve_locations(&*fake, Path::new(HOME)));
    assert!(!found.is_empty());
    for location in &found {
        let meta = block_on(fake.metadata(&location.path));
        assert!(
            matches!(meta, Ok(Some(_))),
            "{} points at {:?}, which does not exist",
            location.name,
            location.path,
        );
    }
}

/// Trash is the boot volume's only. This pins the *documented limit* so it
/// stays a recorded deviation rather than turning into a surprise: a file
/// trashed from an external drive lands in `/Volumes/<name>/.Trashes/<uid>`,
/// which this row does not union in the way Finder's single Trash icon does.
#[test]
fn trash_is_the_boot_volumes_only() {
    let fake = full_mac();
    fake.insert_tree("/Volumes/External/.Trashes", json!({ "501": {} }));
    let found = block_on(resolve_locations(&*fake, Path::new(HOME)));
    let trash: Vec<&PathBuf> = found
        .iter()
        .filter(|l| l.kind == LocationKind::Trash)
        .map(|l| &l.path)
        .collect();
    assert_eq!(
        trash,
        vec![&PathBuf::from("/Users/me/.Trash")],
        "one Trash row, and it is home's — the external volume's is not merged in",
    );
}
