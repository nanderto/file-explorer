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

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{Platform, VolumeId, VolumeInfo};
use crate::attrs::{FileAttrs, UnixPerms};
use crate::thumbnail::{Thumbnail, validate_px};

/// Fixed fake volumes: one internal root plus two ejectable externals.
pub struct StubPlatform {
    volumes: Mutex<Vec<VolumeInfo>>,
}

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
        Ok(FileAttrs {
            perms: Some(UnixPerms::from_mode(STUB_MODES[(hash % 4) as usize])),
            owner: Some("stub-owner".to_string()),
            group: Some("stub-group".to_string()),
            locked: hash.is_multiple_of(8),
            added: Some(
                SystemTime::UNIX_EPOCH + Duration::from_secs(STUB_ADDED_SECS + hash % 86_400),
            ),
            extension_hidden: hash.is_multiple_of(3),
            type_description: stub_type_description(path),
        })
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
