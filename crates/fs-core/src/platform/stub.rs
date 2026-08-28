//! Portable [`Platform`] stub (ARCHITECTURE.md §6 `platform/stub.rs`): a fixed
//! fake volume list so the sidebar renders deterministically on Windows/Linux
//! dev machines, in tests, and in visual scenarios. `eject` removes the volume
//! from subsequent reads so the eject flow is testable end to end.
//!
//! [`Platform::thumbnail`] is likewise synthesized: pixels derived from the path
//! by a fixed hash, with no filesystem access and no clock, so the icon grid has
//! something stable to paint in unit tests and visual scenarios.
//! [`Platform::file_attrs`] (M5) follows the same rule — every field is a pure
//! function of the path, so the info panel renders identically everywhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{Platform, VolumeId, VolumeInfo};
use crate::attrs::{FileAttrs, UnixPerms};
use crate::tags::{Tag, standard_tags};
use crate::thumbnail::{Thumbnail, validate_px};

/// Fixed fake volumes: one internal root plus two ejectable externals, plus an
/// in-memory Finder-tag store (M6b) so tag reads and writes round-trip off
/// macOS.
pub struct StubPlatform {
    volumes: Mutex<Vec<VolumeInfo>>,
    /// Tags per path, empty until something writes. A `BTreeMap` so
    /// [`Platform::known_tags`] can enumerate in a stable order.
    ///
    /// Deliberately **not** path-derived, unlike the thumbnails and attributes
    /// above: tags are the one thing the app *writes*, so the stub has to behave
    /// like storage or the app's write-then-read tests would be fiction. Absent
    /// path ⇒ no tags, which is also the honest default for a fresh tree.
    tags: Mutex<BTreeMap<PathBuf, Vec<Tag>>>,
    /// Owner/group overrides written by [`Platform::set_ownership`], layered
    /// over the path-derived defaults in [`Platform::file_attrs`]. Storage
    /// rather than a hash for the same reason the tags are: ownership is
    /// something the app *writes*, so a write-then-read test would otherwise be
    /// fiction.
    ownership: Mutex<BTreeMap<PathBuf, (String, String)>>,
}

/// The one account name the stub refuses to hand a file to, standing in for the
/// `EPERM` a real unprivileged `chown` returns. Fixed rather than random so the
/// denied path is a deterministic test on every OS — and named `root` because
/// that is precisely the change a real run cannot make.
pub const STUB_PRIVILEGED_OWNER: &str = "root";

const GB: u64 = 1_000_000_000;

fn fixed_volumes() -> Vec<VolumeInfo> {
    let volume = |name: &str, path: &str, total: u64, free: u64, ejectable: bool| {
        let path = PathBuf::from(path);
        VolumeInfo {
            volume_id: VolumeId::from_path(&path),
            name: name.to_string(),
            path,
            total,
            free,
            ejectable,
        }
    };
    vec![
        volume("Macintosh HD", "/", 500 * GB, 250 * GB, false),
        volume(
            "External SSD",
            "/Volumes/External SSD",
            1000 * GB,
            750 * GB,
            true,
        ),
        volume("Camera", "/Volumes/Camera", 64 * GB, 32 * GB, true),
    ]
}

impl StubPlatform {
    pub fn new() -> Self {
        Self {
            volumes: Mutex::new(fixed_volumes()),
            tags: Mutex::new(BTreeMap::new()),
            ownership: Mutex::new(BTreeMap::new()),
        }
    }

    /// Pre-load tags on `path` without awaiting anything — for visual scenarios
    /// and synchronous test setup. Equivalent to
    /// [`Platform::write_tags`], including the "empty removes" rule.
    pub fn seed_tags(&self, path: impl Into<PathBuf>, tags: Vec<Tag>) {
        let mut store = self.tags.lock().unwrap();
        let path = path.into();
        if tags.is_empty() {
            store.remove(&path);
        } else {
            store.insert(path, tags);
        }
    }
}

impl Default for StubPlatform {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Platform for StubPlatform {
    async fn volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.volumes.lock().unwrap().clone())
    }

    /// Synthesized pixels — no I/O, no clock, identical on every platform and
    /// every run. The path picks a base color and one of three aspect ratios;
    /// the longest edge is exactly `px`, matching the real contract. Nothing
    /// here inspects the filesystem, so a path that does not exist still
    /// yields a thumbnail (which is what makes visual scenarios possible).
    async fn thumbnail(&self, path: &Path, px: u32) -> Result<Thumbnail> {
        let (width, height) = stub_dimensions(path, validate_px(px)?);
        Thumbnail::new(width, height, synth_pixels(path, width, height))
    }

    /// Path-derived attributes — no syscalls, no clock. The path's hash picks
    /// the mode, the locked and extension-hidden flags and the Date Added
    /// offset; the extension picks the type description. A path that does not
    /// exist still yields attributes, which is what makes visual scenarios and
    /// `FakeVfs`-backed tests possible.
    ///
    /// Because `locked`, `extension_hidden` and the mode are path-derived, a
    /// test must not assert a *value* for them over a path it did not fix (a
    /// `tempfile` directory's random suffix, say) — only that the same path
    /// always answers the same way.
    async fn file_attrs(&self, path: &Path) -> Result<FileAttrs> {
        let hash = path_hash(path);
        let (owner, group) = self
            .ownership
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_else(|| ("stub-owner".to_string(), "stub-group".to_string()));
        Ok(FileAttrs {
            perms: Some(UnixPerms::from_mode(STUB_MODES[(hash % 4) as usize])),
            owner: Some(owner),
            group: Some(group),
            locked: hash.is_multiple_of(8),
            added: Some(
                SystemTime::UNIX_EPOCH + Duration::from_secs(STUB_ADDED_SECS + hash % 86_400),
            ),
            extension_hidden: hash.is_multiple_of(3),
            type_description: stub_type_description(path),
        })
    }

    /// In-memory storage — whatever [`Platform::write_tags`] last stored for
    /// this exact path, and no tags for a path never written. No filesystem
    /// access, so it works over `FakeVfs` trees and paths that do not exist.
    async fn read_tags(&self, path: &Path) -> Result<Vec<Tag>> {
        Ok(self
            .tags
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default())
    }

    /// Stores the set verbatim after the codec's normalization (blank names
    /// dropped, duplicate names collapsed), so the stub agrees with what macOS
    /// would have persisted. An empty slice forgets the path entirely — the
    /// portable stand-in for removing the xattr.
    async fn write_tags(&self, path: &Path, tags: &[Tag]) -> Result<()> {
        let normalized = crate::tags::decode_tag_strings(&crate::tags::encode_tag_strings(tags));
        self.seed_tags(path.to_path_buf(), normalized);
        Ok(())
    }

    /// The standard palette plus every tag name the stub has actually stored,
    /// in `BTreeMap` order — deterministic, and enough for the sidebar's Tags
    /// section to show user tags off macOS.
    async fn known_tags(&self) -> Result<Vec<Tag>> {
        let mut known = standard_tags();
        for tag in self.tags.lock().unwrap().values().flatten() {
            if !known.iter().any(|k| k.name == tag.name) {
                known.push(tag.clone());
            }
        }
        Ok(known)
    }

    /// Records the change so [`Platform::file_attrs`] reports it, except for
    /// [`STUB_PRIVILEGED_OWNER`], which fails the way an unprivileged real
    /// `chown` does — nothing is stored, so a failed call changes nothing.
    async fn set_ownership(
        &self,
        path: &Path,
        owner: Option<&str>,
        group: Option<&str>,
    ) -> Result<()> {
        if owner == Some(STUB_PRIVILEGED_OWNER) || group == Some(STUB_PRIVILEGED_OWNER) {
            bail!(
                "chown {}: operation not permitted (only a privileged process may \
                 change ownership to '{STUB_PRIVILEGED_OWNER}')",
                path.display()
            );
        }
        if owner.is_none() && group.is_none() {
            return Ok(()); // nothing asked for, nothing changed
        }
        let mut store = self.ownership.lock().unwrap();
        let current = store
            .get(path)
            .cloned()
            .unwrap_or_else(|| ("stub-owner".to_string(), "stub-group".to_string()));
        store.insert(
            path.to_path_buf(),
            (
                owner.map(str::to_string).unwrap_or(current.0),
                group.map(str::to_string).unwrap_or(current.1),
            ),
        );
        Ok(())
    }

    async fn eject(&self, volume_id: &VolumeId) -> Result<()> {
        let mut volumes = self.volumes.lock().unwrap();
        let Some(ix) = volumes.iter().position(|v| &v.volume_id == volume_id) else {
            bail!("no such volume: {}", volume_id.as_str());
        };
        if !volumes[ix].ejectable {
            bail!("volume is not ejectable: {}", volumes[ix].name);
        }
        volumes.remove(ix);
        Ok(())
    }
}

/// The four modes the stub hands out, covering a read-only file, an executable,
/// a private file and a group-writable one — enough shapes for the info panel's
/// permission row to be exercised.
const STUB_MODES: [u32; 4] = [0o644, 0o755, 0o600, 0o664];

/// Fixed "Date Added" base: 2024-01-01T00:00:00Z as seconds since the unix
/// epoch, so the stub needs no date library and never reads a clock.
const STUB_ADDED_SECS: u64 = 1_704_067_200;

/// Type description from the extension alone — the stub cannot ask the OS.
fn stub_type_description(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "PNG image".to_string(),
        "jpg" | "jpeg" => "JPEG image".to_string(),
        "pdf" => "PDF document".to_string(),
        "txt" | "md" => "Plain text document".to_string(),
        other => format!("{} document", other.to_uppercase()),
    })
}

/// FNV-1a over the path's bytes — a fixed, dependency-free, platform-independent
/// hash. `DefaultHasher` would not do: its output is explicitly unstable across
/// releases, and these pixels are compared against committed baselines.
fn path_hash(path: &Path) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One of three aspect ratios (square, landscape, portrait) with the longest
/// edge at exactly `px`, so callers that lay out non-square previews are
/// exercised by the stub too.
fn stub_dimensions(path: &Path, px: u32) -> (u32, u32) {
    let short = (px * 3 / 4).max(1);
    match path_hash(path) % 3 {
        0 => (px, px),
        1 => (px, short),
        _ => (short, px),
    }
}

fn synth_pixels(path: &Path, width: u32, height: u32) -> Vec<u8> {
    let hash = path_hash(path);
    let base = [
        (hash >> 8) as u8 | 0x40,
        (hash >> 16) as u8 | 0x40,
        (hash >> 24) as u8 | 0x40,
    ];
    let cell = (width.max(height) / 8).max(1);
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height {
        for x in 0..width {
            // Checkerboard darkened by a diagonal gradient: visibly a picture
            // rather than a flat swatch, and a pure function of (path, x, y).
            let checker = ((x / cell) + (y / cell)).is_multiple_of(2);
            let gradient = ((x + y) * 96 / (width + height)) as u8;
            for channel in &base {
                let value = channel.saturating_sub(gradient);
                pixels.push(if checker { value } else { value / 2 });
            }
            pixels.push(0xff);
        }
    }
    pixels
}
