//! macOS [`Platform`] implementation (ARCHITECTURE.md §6 `platform/macos.rs`).
//!
//! Volume enumeration uses `NSFileManager mountedVolumeURLsIncludingResourceValuesForKeys:options:`
//! via objc2-foundation (plan §4's prescribed stack), with every blocking call
//! wrapped in [`SpawnerExt::unblock`] so it never runs on the UI thread.
//!
//! Eject shells out to `diskutil eject` instead of Foundation's block-based
//! `unmountVolumeAtURL:options:completionHandler:` — that API would add a
//! `block2` dependency and a run-loop delivery assumption for its completion
//! handler; `diskutil` is synchronous, dependency-free, and runs on the
//! background executor like every other blocking call. Recorded as a
//! deviation in `docs/AS_BUILT.md`; revisit if the M2 Mac checklist finds it
//! lacking (e.g. no force-eject prompt integration).
//!
//! NOTE: this file only compiles under `cfg(target_os = "macos")` — on this
//! Windows dev machine it is checked by macOS CI, not locally.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;

use super::{Platform, VolumeId, VolumeInfo};
use crate::exec::{Spawner, SpawnerExt as _};

/// Real macOS platform services, constructed once at boot with the app's
/// [`Spawner`].
pub struct MacPlatform {
    spawner: Arc<dyn Spawner>,
}

impl MacPlatform {
    pub fn new(spawner: Arc<dyn Spawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Platform for MacPlatform {
    async fn volumes(&self) -> Result<Vec<VolumeInfo>> {
        self.spawner.unblock(volumes_blocking).await
    }

    async fn eject(&self, volume_id: &VolumeId) -> Result<()> {
        let mount_point = volume_id.as_str().to_string();
        self.spawner
            .unblock(move || {
                let output = Command::new("/usr/sbin/diskutil")
                    .arg("eject")
                    .arg(&mount_point)
                    .output()
                    .context("running diskutil eject")?;
                if output.status.success() {
                    Ok(())
                } else {
                    bail!(
                        "diskutil eject '{mount_point}' failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
            })
            .await
    }
}

/// Move `path` to the real macOS trash via `NSFileManager
/// trashItemAtURL:resultingItemURL:error:` (plan §4's prescribed mechanism).
/// Blocking — [`crate::RealVfs::trash`] calls it through `unblock`. The
/// resulting trash URL becomes [`TrashId::trashed`], so restore is a plain
/// rename back (`platform::trash::restore_blocking`).
#[allow(unused_unsafe)] // see volumes_blocking below
pub(crate) fn trash_item_blocking(path: &std::path::Path) -> Result<crate::vfs::TrashId> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let manager = unsafe { NSFileManager::defaultManager() };
    let url = unsafe { NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())) };
    let mut resulting = None;
    unsafe { manager.trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting)) }
        .map_err(|error| anyhow!("trash {}: {error}", path.display()))?;
    let trashed = resulting
        .and_then(|trash_url| unsafe { trash_url.path() })
        .map(|s| PathBuf::from(s.to_string()))
        .ok_or_else(|| anyhow!("trash {}: no resulting URL returned", path.display()))?;
    Ok(crate::vfs::TrashId {
        original: path.to_path_buf(),
        trashed,
    })
}

/// Enumerate mounted volumes with name, capacity, and ejectability resource
/// values. Blocking — always called through `unblock`.
#[allow(unused_unsafe)] // objc2's generated bindings flip between safe and
// unsafe signatures across releases; this file is written on Windows and only
// compiled by macOS CI, so tolerate either rather than fail `-D warnings`.
fn volumes_blocking() -> Result<Vec<VolumeInfo>> {
    use objc2_foundation::{
        NSArray, NSFileManager, NSNumber, NSString, NSURLResourceKey,
        NSURLVolumeAvailableCapacityKey, NSURLVolumeIsEjectableKey, NSURLVolumeNameKey,
        NSURLVolumeTotalCapacityKey, NSVolumeEnumerationOptions,
    };

    let keys: [&NSURLResourceKey; 4] = unsafe {
        [
            NSURLVolumeNameKey,
            NSURLVolumeTotalCapacityKey,
            NSURLVolumeAvailableCapacityKey,
            NSURLVolumeIsEjectableKey,
        ]
    };
    let keys = NSArray::from_slice(&keys);

    let manager = unsafe { NSFileManager::defaultManager() };
    let urls = unsafe {
        manager.mountedVolumeURLsIncludingResourceValuesForKeys_options(
            Some(&keys),
            NSVolumeEnumerationOptions::SkipHiddenVolumes,
        )
    }
    .ok_or_else(|| anyhow!("NSFileManager returned no mounted volume list"))?;

    let mut volumes = Vec::new();
    for url in urls.iter() {
        let Some(path_string) = (unsafe { url.path() }) else {
            continue; // non-file-path volume URL
        };
        let path = PathBuf::from(path_string.to_string());
        let Ok(values) = (unsafe { url.resourceValuesForKeys_error(&keys) }) else {
            continue; // volume vanished mid-enumeration
        };

        let string_for = |key: &NSURLResourceKey| -> Option<String> {
            values
                .objectForKey(key)
                .and_then(|value| value.downcast_ref::<NSString>().map(|s| s.to_string()))
        };
        let u64_for = |key: &NSURLResourceKey| -> Option<u64> {
            values.objectForKey(key).and_then(|value| {
                value
                    .downcast_ref::<NSNumber>()
                    .map(|n| n.unsignedLongLongValue())
            })
        };
        let bool_for = |key: &NSURLResourceKey| -> Option<bool> {
            values
                .objectForKey(key)
                .and_then(|value| value.downcast_ref::<NSNumber>().map(|n| n.boolValue()))
        };

        let name = unsafe { string_for(NSURLVolumeNameKey) }
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| path.display().to_string());
        volumes.push(VolumeInfo {
            volume_id: VolumeId::from_path(&path),
            name,
            total: unsafe { u64_for(NSURLVolumeTotalCapacityKey) }.unwrap_or(0),
            free: unsafe { u64_for(NSURLVolumeAvailableCapacityKey) }.unwrap_or(0),
            ejectable: unsafe { bool_for(NSURLVolumeIsEjectableKey) }.unwrap_or(false),
            path,
        });
    }
    Ok(volumes)
}
