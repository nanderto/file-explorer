//! Portable [`Platform`] stub (ARCHITECTURE.md §6 `platform/stub.rs`): a fixed
//! fake volume list so the sidebar renders deterministically on Windows/Linux
//! dev machines, in tests, and in visual scenarios. `eject` removes the volume
//! from subsequent reads so the eject flow is testable end to end.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{Platform, VolumeId, VolumeInfo};

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
