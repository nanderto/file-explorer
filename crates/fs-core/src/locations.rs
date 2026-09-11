//! The sidebar's **Locations**: the fixed places a Mac always has, plus the
//! cloud folders this particular Mac happens to have (plan §7 M7d).
//!
//! GPUI-free and `Vfs`-driven, like the rest of `fs-core`: the UI asks for
//! [`resolve_locations`] on the background executor and paints whatever comes
//! back. Nothing here assumes macOS at compile time — the paths are just
//! paths, so the module builds and tests on Windows and Linux against a
//! `FakeVfs` tree shaped like a Mac.
//!
//! **Only what exists is returned.** A Mac with iCloud Drive switched off has
//! no `com~apple~CloudDocs` folder, and a row pointing at a missing directory
//! is a row that navigates to an error. Every fixed candidate is stat'ed and
//! the misses dropped, which is why this is async rather than a const table.
//!
//! Two things Finder shows here are deliberately absent:
//!
//! * **AirDrop** has no filesystem presence at all — it is a Finder-only view
//!   over a private sharing service. There is no path to navigate to, so
//!   there is no honest row to draw.
//! * **Connected servers** appear under `/Volumes` once mounted and are
//!   already listed as Devices, so a second row would be the same mount
//!   twice.

use std::path::{Path, PathBuf};

use futures::StreamExt as _;

use crate::entry::FileEntry;
use crate::vfs::Vfs;

/// Which location a row is, so the UI can pick an icon without matching on
/// the display name — which is localized, tenant-suffixed, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LocationKind {
    /// `~/Library/Mobile Documents/com~apple~CloudDocs`.
    ICloudDrive,
    /// `~/OneDrive - <Tenant>`, or plain `~/OneDrive` for a personal account.
    OneDrive,
    /// The user's home folder, shown under its short name.
    Home,
    /// `/Network` — an autofs mount point, and a real navigable directory
    /// despite plan §7's note that it "is not a path at all".
    Network,
    /// `~/.Trash`. See [`resolve_locations`] for what this row does *not*
    /// cover.
    Trash,
}

/// One row of the Locations section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub kind: LocationKind,
    /// What the row is labelled. Not derivable from the path: iCloud Drive's
    /// folder is literally named `com~apple~CloudDocs`, and `.Trash` is
    /// hidden and dot-prefixed.
    pub name: String,
    pub path: PathBuf,
}

/// Where iCloud Drive lives, relative to home. The `~` in the folder name is
/// macOS's own escaping of `.` in the container id, not a typo.
const ICLOUD_RELATIVE: &str = "Library/Mobile Documents/com~apple~CloudDocs";

/// Finder's own label. The folder carries no `.localized` marker, so the
/// display name has to be supplied rather than read off disk.
const ICLOUD_NAME: &str = "iCloud Drive";

/// The autofs mount every Mac has, and Finder's "Network".
const NETWORK_PATH: &str = "/Network";

/// The prefix a OneDrive sync root always starts with. The rest is the
/// tenant — `OneDrive - BidOne Ltd` — so this cannot be a fixed path.
const ONEDRIVE_PREFIX: &str = "OneDrive";

/// The fixed candidates, in the order they are shown, before the OneDrive
/// roots are spliced in and before anything is stat'ed.
///
/// `home` is a parameter rather than an `env::home_dir()` call so this stays
/// a pure function and tests can point it at a fixture tree.
///
/// Home is a *location* rather than a favorite deliberately: it is the one
/// row here that cannot be unpinned, because the sidebar would be lying about
/// the machine if it could.
pub fn fixed_candidates(home: &Path) -> Vec<Location> {
    let home_name = home
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| home.display().to_string());
    vec![
        Location {
            kind: LocationKind::ICloudDrive,
            name: ICLOUD_NAME.to_string(),
            path: home.join(ICLOUD_RELATIVE),
        },
        Location {
            kind: LocationKind::Home,
            name: home_name,
            path: home.to_path_buf(),
        },
        Location {
            kind: LocationKind::Network,
            name: "Network".to_string(),
            path: PathBuf::from(NETWORK_PATH),
        },
        Location {
            kind: LocationKind::Trash,
            name: "Trash".to_string(),
            path: home.join(".Trash"),
        },
    ]
}

/// The OneDrive sync roots among `home`'s entries.
///
/// A pure function over an already-read listing, so the scan is testable
/// without a filesystem and [`resolve_locations`] pays for exactly one
/// `read_dir`.
///
/// Matching is `OneDrive` or `OneDrive - <anything>`, **not** merely "starts
/// with OneDrive": a folder called `OneDriveBackups` is the user's own, not a
/// sync root. Every tenant is returned, in listing order, because a machine
/// signed into two organizations genuinely has two roots.
pub fn onedrive_candidates(home_entries: &[FileEntry]) -> Vec<Location> {
    home_entries
        .iter()
        .filter(|entry| entry.is_dir_like())
        .filter(|entry| is_onedrive_name(&entry.name))
        .map(|entry| Location {
            kind: LocationKind::OneDrive,
            name: entry.name.to_string(),
            path: entry.path.to_path_buf(),
        })
        .collect()
}

fn is_onedrive_name(name: &str) -> bool {
    match name.strip_prefix(ONEDRIVE_PREFIX) {
        None => false,
        // Exactly "OneDrive" — a personal account.
        Some("") => true,
        // "OneDrive - Tenant". The separator is required, so `OneDriveOld`
        // (someone's own folder) does not match.
        Some(rest) => rest.starts_with(" - "),
    }
}

/// The Locations rows this machine actually has, in display order: iCloud
/// Drive, every OneDrive root, home, Network, Trash.
///
/// The fixed candidates are stat'ed and the missing ones dropped; the one
/// `read_dir` is of `home`, and failing to read it costs the OneDrive rows
/// only.
///
/// **What the Trash row does not cover:** `~/.Trash` is the *boot volume's*
/// trash. Every other mounted volume keeps its own at
/// `/Volumes/<name>/.Trashes/<uid>`, and Finder's single Trash icon is a
/// union of all of them. This row is the boot volume's alone — honest for the
/// common case, and incomplete the moment a file is trashed from an external
/// drive.
pub async fn resolve_locations(vfs: &dyn Vfs, home: &Path) -> Vec<Location> {
    let mut fixed = fixed_candidates(home);
    let onedrive = read_onedrive(vfs, home).await;

    // iCloud first, then the OneDrive roots beside it, then the rest: the
    // "somewhere else" rows grouped above the local ones.
    let tail = fixed.split_off(1);
    let mut ordered = fixed;
    ordered.extend(onedrive);
    ordered.extend(tail);

    let mut present = Vec::with_capacity(ordered.len());
    for location in ordered {
        // A root discovered by reading home obviously exists; only the fixed
        // candidates need the stat.
        if location.kind == LocationKind::OneDrive
            || matches!(vfs.metadata(&location.path).await, Ok(Some(_)))
        {
            present.push(location);
        }
    }
    present
}

async fn read_onedrive(vfs: &dyn Vfs, home: &Path) -> Vec<Location> {
    let Ok(stream) = vfs.read_dir(home).await else {
        return Vec::new();
    };
    let entries: Vec<FileEntry> = stream
        .filter_map(|entry| async move { entry.ok() })
        .collect()
        .await;
    onedrive_candidates(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icloud_is_named_rather_than_derived_from_its_folder() {
        let fixed = fixed_candidates(Path::new("/Users/me"));
        let icloud = &fixed[0];
        assert_eq!(icloud.kind, LocationKind::ICloudDrive);
        assert_eq!(
            icloud.path,
            PathBuf::from("/Users/me/Library/Mobile Documents/com~apple~CloudDocs"),
        );
        // The whole reason `name` is a field: the folder's own name is
        // unreadable garbage to a person.
        assert_eq!(icloud.name, "iCloud Drive");
    }

    #[test]
    fn home_is_labelled_with_its_short_name() {
        let fixed = fixed_candidates(Path::new("/Users/noelanderton"));
        let home = fixed
            .iter()
            .find(|l| l.kind == LocationKind::Home)
            .expect("home candidate");
        assert_eq!(home.name, "noelanderton");
        assert_eq!(home.path, PathBuf::from("/Users/noelanderton"));
    }

    /// `/` has no file name, and a row labelled with the empty string is an
    /// invisible, clickable blank.
    #[test]
    fn a_home_without_a_file_name_still_gets_a_label() {
        let fixed = fixed_candidates(Path::new("/"));
        let home = fixed
            .iter()
            .find(|l| l.kind == LocationKind::Home)
            .expect("home candidate");
        assert!(!home.name.is_empty(), "a row must never be labelled \"\"");
    }

    #[test]
    fn onedrive_matches_a_tenant_suffix_but_not_a_lookalike() {
        assert!(is_onedrive_name("OneDrive"), "a personal account");
        assert!(is_onedrive_name("OneDrive - BidOne Ltd"), "a tenant");
        assert!(
            is_onedrive_name("OneDrive - Contoso - Archive"),
            "a tenant whose own name contains the separator"
        );
        // The interesting half: folders of the user's own making that merely
        // start with the same letters must not be claimed as sync roots.
        assert!(!is_onedrive_name("OneDriveBackups"));
        assert!(!is_onedrive_name("OneDrive2"));
        assert!(!is_onedrive_name("MyOneDrive"));
        assert!(
            !is_onedrive_name("onedrive"),
            "case-sensitive, as macOS writes it"
        );
    }
}
