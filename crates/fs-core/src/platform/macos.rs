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
//! Thumbnails (M4) go through `QuickLookThumbnailing` — the plan §4 stack — with
//! the plan's own fallback (`image`-crate decode) when QuickLook declines a
//! file. See [`quicklook_blocking`] for the threading argument.
//!
//! Finder tags (M6b) are the `com.apple.metadata:_kMDItemUserTags` extended
//! attribute holding a **binary plist array of strings**. Two mechanisms, both
//! chosen to avoid a new dependency (and the full workspace rebuild a
//! `Cargo.toml` dependency change costs): the xattr syscalls are declared
//! directly in [`xattr`] rather than pulling in the `xattr` crate or `libc`
//! (same precedent as [`UF_IMMUTABLE`]), and the plist is serialized by
//! `NSPropertyListSerialization` from the already-present `objc2-foundation`
//! (two extra *features* on that dependency, no new crate). Foundation is also
//! why reading accepts XML as well as binary for free — it sniffs the format.
//!
//! NOTE: this file only compiles under `cfg(target_os = "macos")`. Earlier
//! milestones were written on a Windows machine and checked only by macOS CI;
//! from M4 on it is also compiled and tested locally.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;

use super::{Platform, VolumeId, VolumeInfo};
use crate::attrs::{FileAttrs, UnixPerms};
use crate::exec::{Spawner, SpawnerExt as _};
use crate::tags::{Tag, TagColor, decode_tag_strings, encode_tag_strings, standard_tags};
use crate::thumbnail::{Thumbnail, validate_px};

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

    /// QuickLook first, `image`-crate decode as the fallback (plan §4:
    /// "`QuickLookThumbnailing` for real thumbnails (fallback: `image` crate
    /// decode)"). Both halves are blocking, so both run inside a single
    /// [`SpawnerExt::unblock`] — the UI thread only awaits.
    async fn thumbnail(&self, path: &Path, px: u32) -> Result<Thumbnail> {
        let px = validate_px(px)?;
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || match quicklook_blocking(&path, px) {
                Ok(thumbnail) => Ok(thumbnail),
                Err(ql_error) => decode_image_blocking(&path, px).map_err(|decode_error| {
                    // Both paths failed: report both, because "QuickLook said
                    // no" and "this isn't an image either" are different
                    // diagnoses and the caller sees only the message.
                    anyhow!(
                        "no thumbnail for {}: quicklook: {ql_error}; image decode: {decode_error}",
                        path.display()
                    )
                }),
            })
            .await
    }

    /// Three lookups, each degrading on its own (see [`file_attrs_blocking`]),
    /// all inside one [`SpawnerExt::unblock`].
    async fn file_attrs(&self, path: &Path) -> Result<FileAttrs> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || file_attrs_blocking(&path))
            .await
    }

    /// One `getxattr` plus a Foundation plist parse, both blocking, both
    /// inside one [`SpawnerExt::unblock`].
    async fn read_tags(&self, path: &Path) -> Result<Vec<Tag>> {
        let path = path.to_path_buf();
        self.spawner
            .unblock(move || read_tags_blocking(&path))
            .await
    }

    /// Encode → binary plist → `setxattr`, or `removexattr` when the set is
    /// empty. All blocking, all inside one [`SpawnerExt::unblock`].
    async fn write_tags(&self, path: &Path, tags: &[Tag]) -> Result<()> {
        let path = path.to_path_buf();
        let tags = tags.to_vec();
        self.spawner
            .unblock(move || write_tags_blocking(&path, &tags))
            .await
    }

    /// The standard palette plus the user's Finder favourites (see
    /// [`favorite_tag_names`]); reading a preferences file, hence `unblock`.
    async fn known_tags(&self) -> Result<Vec<Tag>> {
        self.spawner.unblock(known_tags_blocking).await
    }

    /// `NSFileManager setAttributes:ofItemAtPath:error:` with
    /// `NSFileOwnerAccountName` / `NSFileGroupOwnerAccountName` — the *name*
    /// keys, so Foundation performs the account lookup and we never hand-roll
    /// `getpwnam`/`getgrnam` (nor take a `libc` dependency for them). It is the
    /// exact API [`account_names`] already reads through, so a successful write
    /// is guaranteed to show up in the panel's owner row.
    ///
    /// Blocking, inside one [`SpawnerExt::unblock`]. An unprivileged run gets
    /// Foundation's `EPERM` error text back as an ordinary `Err`.
    async fn set_ownership(
        &self,
        path: &Path,
        owner: Option<&str>,
        group: Option<&str>,
    ) -> Result<()> {
        let path = path.to_path_buf();
        let owner = owner.map(str::to_string);
        let group = group.map(str::to_string);
        self.spawner
            .unblock(move || set_ownership_blocking(&path, owner.as_deref(), group.as_deref()))
            .await
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

/// `UF_IMMUTABLE` from `<sys/stat.h>`: the user-immutable flag, which is what
/// Finder's "Locked" checkbox sets. Spelled out here rather than pulled in from
/// `libc` — a whole new dependency (and the full rebuild that a `Cargo.toml`
/// change costs) for one constant that has been stable since 4.4BSD.
const UF_IMMUTABLE: u32 = 0x0000_0002;

/// Extended attributes for the info panel (M5). Blocking — always called
/// through `unblock`.
///
/// Only the `lstat` can fail the call: if the item is not there, there are no
/// attributes to show. The three richer lookups are independent and each
/// degrades to `None`/`false` on failure, so a file on a filesystem that has no
/// Date Added still reports its mode and owner.
///
/// `lstat` (not `stat`) on purpose: the panel describes the item the user
/// selected, and for a symlink that is the link's own mode and owner, not its
/// target's.
///
/// Uses `std::os::macos::fs::MetadataExt` for mode/uid/gid/flags instead of
/// calling `libc::lstat` directly — same `struct stat`, no new dependency —
/// and `NSFileManager`/`NSURL` (already in use above) for the names and the
/// Foundation-only values, so no `getpwuid_r`/`getgrgid_r` either.
///
/// Deliberately **unbounded**, unlike [`quicklook_blocking`]: there is no
/// completion handler to abandon, so bounding a `stat` would mean leaking a
/// thread per stalled call rather than parking one. A selection on a hung
/// network mount therefore parks its pool thread until the mount answers —
/// recorded as a Known gap.
fn file_attrs_blocking(path: &Path) -> Result<FileAttrs> {
    use std::os::macos::fs::MetadataExt as _;

    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat {} for its attributes", path.display()))?;
    // The Foundation lookups below go through `NSString`, which can only carry
    // a UTF-8 path: for anything else (raw bytes on an SMB or exFAT volume) a
    // lossy conversion would query a *different* path, and could attribute
    // another file's owner and type description to this one. Honestly absent
    // beats plausibly wrong.
    if path.to_str().is_none() {
        return Ok(FileAttrs {
            perms: Some(UnixPerms::from_mode(meta.st_mode())),
            owner: Some(meta.st_uid().to_string()),
            group: Some(meta.st_gid().to_string()),
            locked: meta.st_flags() & UF_IMMUTABLE != 0,
            ..FileAttrs::default()
        });
    }
    let (owner, group) = account_names(path)
        .unwrap_or_default()
        // No account names? Fall back to the numeric ids, which we always have.
        .into_pair_or(meta.st_uid(), meta.st_gid());
    let resources = url_resource_values(path).unwrap_or_default();

    Ok(FileAttrs {
        perms: Some(UnixPerms::from_mode(meta.st_mode())),
        owner: Some(owner),
        group: Some(group),
        locked: meta.st_flags() & UF_IMMUTABLE != 0,
        added: resources.added,
        extension_hidden: resources.extension_hidden,
        type_description: resources.type_description,
    })
}

/// Change the owner and/or group of `path` by name. Blocking — always called
/// through `unblock`.
///
/// A non-UTF-8 path is refused rather than lossily converted, for the same
/// reason [`file_attrs_blocking`] refuses it: the lossy form would name a
/// *different* file, and here that would mean giving away the wrong one.
#[allow(unused_unsafe)] // see volumes_blocking
fn set_ownership_blocking(path: &Path, owner: Option<&str>, group: Option<&str>) -> Result<()> {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{
        NSDictionary, NSFileAttributeKey, NSFileGroupOwnerAccountName, NSFileManager,
        NSFileOwnerAccountName, NSString,
    };

    if owner.is_none() && group.is_none() {
        return Ok(()); // nothing asked for, nothing to do
    }
    let path_str = path.to_str().ok_or_else(|| {
        anyhow!(
            "cannot change ownership of a non-UTF-8 path: {}",
            path.display()
        )
    })?;

    let owner_value = owner.map(NSString::from_str);
    let group_value = group.map(NSString::from_str);
    let mut keys: Vec<&NSFileAttributeKey> = Vec::new();
    let mut values: Vec<&AnyObject> = Vec::new();
    if let Some(value) = &owner_value {
        keys.push(unsafe { NSFileOwnerAccountName });
        values.push(value.as_ref());
    }
    if let Some(value) = &group_value {
        keys.push(unsafe { NSFileGroupOwnerAccountName });
        values.push(value.as_ref());
    }
    let attributes: objc2::rc::Retained<NSDictionary<NSFileAttributeKey, AnyObject>> =
        NSDictionary::from_slices(&keys, &values);

    // Captured for the verification below, which is not optional: see the
    // `ignored` check.
    let before = account_names(path).unwrap_or_default();

    let manager = unsafe { NSFileManager::defaultManager() };
    unsafe { manager.setAttributes_ofItemAtPath_error(&attributes, &NSString::from_str(path_str)) }
        .map_err(|error| anyhow!("chown {}: {}", path.display(), error.localizedDescription()))?;

    // `setAttributes:` reports **success** for an account name it could not
    // resolve, and simply leaves that half alone (verified on macOS 15: a
    // nonexistent group name returns no error and changes nothing). Reporting
    // that as done would hand the info panel a lie and record an undo entry for
    // a change that never happened, so read the ownership back and insist it
    // took.
    let after = account_names(path).unwrap_or_default();
    let ignored = |requested: Option<&str>, after: &Option<String>, before: &Option<String>| {
        // Only a *stuck* value is a failure: a value that moved but does not
        // string-match the request is an alias (a uid spelled numerically, a
        // directory service canonicalizing a name), not a silent no-op.
        requested.is_some_and(|want| after.as_deref() != Some(want) && after == before)
    };
    if ignored(owner, &after.owner, &before.owner) || ignored(group, &after.group, &before.group) {
        bail!(
            "chown {}: the system ignored the request (unknown account name?)",
            path.display()
        );
    }
    Ok(())
}

/// Owner and group *names*, either of which the directory service may decline
/// to resolve (a uid with no account, a network directory that is down).
#[derive(Default)]
struct AccountNames {
    owner: Option<String>,
    group: Option<String>,
}

impl AccountNames {
    /// Each missing name becomes its numeric id rendered as a string, which is
    /// what `ls -l` does and what [`FileAttrs::owner`] documents.
    fn into_pair_or(self, uid: u32, gid: u32) -> (String, String) {
        (
            self.owner.unwrap_or_else(|| uid.to_string()),
            self.group.unwrap_or_else(|| gid.to_string()),
        )
    }
}

/// `NSFileManager attributesOfItemAtPath:error:` for the owner and group names.
#[allow(unused_unsafe)] // see volumes_blocking
fn account_names(path: &Path) -> Option<AccountNames> {
    use objc2_foundation::{
        NSFileAttributeKey, NSFileGroupOwnerAccountName, NSFileManager, NSFileOwnerAccountName,
        NSString,
    };

    let manager = unsafe { NSFileManager::defaultManager() };
    let attributes = unsafe {
        manager.attributesOfItemAtPath_error(&NSString::from_str(&path.to_string_lossy()))
    }
    .ok()?;
    let string_for = |key: &NSFileAttributeKey| -> Option<String> {
        attributes
            .objectForKey(key)
            .and_then(|value| value.downcast_ref::<NSString>().map(|s| s.to_string()))
    };
    Some(AccountNames {
        owner: unsafe { string_for(NSFileOwnerAccountName) },
        group: unsafe { string_for(NSFileGroupOwnerAccountName) },
    })
}

/// The `FileAttrs` fields that only Foundation knows.
#[derive(Default)]
struct UrlResources {
    added: Option<SystemTime>,
    extension_hidden: bool,
    type_description: Option<String>,
}

/// `NSURL resourceValuesForKeys:error:` for Date Added, the extension-hidden
/// flag and the localized type description. Any individual key may be absent
/// (Date Added, for one, only exists on filesystems that record it).
#[allow(unused_unsafe)] // see volumes_blocking
fn url_resource_values(path: &Path) -> Option<UrlResources> {
    use objc2_foundation::{
        NSArray, NSDate, NSNumber, NSString, NSURL, NSURLAddedToDirectoryDateKey,
        NSURLHasHiddenExtensionKey, NSURLLocalizedTypeDescriptionKey, NSURLResourceKey,
    };

    let keys: [&NSURLResourceKey; 3] = unsafe {
        [
            NSURLAddedToDirectoryDateKey,
            NSURLHasHiddenExtensionKey,
            NSURLLocalizedTypeDescriptionKey,
        ]
    };
    let keys = NSArray::from_slice(&keys);
    let url = unsafe { NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy())) };
    let values = unsafe { url.resourceValuesForKeys_error(&keys) }.ok()?;

    let added = unsafe { values.objectForKey(NSURLAddedToDirectoryDateKey) }
        .and_then(|value| value.downcast_ref::<NSDate>().map(system_time_from))
        .flatten();
    let extension_hidden = unsafe { values.objectForKey(NSURLHasHiddenExtensionKey) }
        .and_then(|value| value.downcast_ref::<NSNumber>().map(|n| n.boolValue()))
        .unwrap_or(false);
    let type_description = unsafe { values.objectForKey(NSURLLocalizedTypeDescriptionKey) }
        .and_then(|value| value.downcast_ref::<NSString>().map(|s| s.to_string()));

    Some(UrlResources {
        added,
        extension_hidden,
        type_description,
    })
}

/// `NSDate` → [`SystemTime`]. Foundation dates can predate the unix epoch and
/// can be non-finite, so both directions are handled and anything
/// unrepresentable becomes `None` rather than a panic.
#[allow(unused_unsafe)] // see volumes_blocking
fn system_time_from(date: &objc2_foundation::NSDate) -> Option<SystemTime> {
    let seconds = unsafe { date.timeIntervalSince1970() };
    if !seconds.is_finite() {
        return None;
    }
    let magnitude = Duration::try_from_secs_f64(seconds.abs()).ok()?;
    if seconds.is_sign_negative() {
        SystemTime::UNIX_EPOCH.checked_sub(magnitude)
    } else {
        SystemTime::UNIX_EPOCH.checked_add(magnitude)
    }
}

/// How long to wait for QuickLook before giving up and trying the fallback.
/// QuickLook generation is an XPC round-trip to a helper that can be cold,
/// stuck, or absent (a fresh CI runner, a sandbox); an unbounded wait would
/// park an executor thread forever, so it is bounded and failure falls through
/// to `decode_image_blocking`.
const QUICKLOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Ask QuickLook for the most representative thumbnail of `path` and convert
/// it to straight RGBA8.
///
/// Blocking — only ever called inside `unblock`. The completion handler runs on
/// a QuickLook-owned queue, so the CoreGraphics image is converted to plain
/// bytes *inside* the handler and only `Vec<u8>` crosses the channel:
/// `CFRetained<CGImage>` is not `Send`, and moving one between threads would be
/// unsound even though it would compile.
fn quicklook_blocking(path: &Path, px: u32) -> Result<Thumbnail> {
    use block2::RcBlock;
    use objc2::AnyThread as _;
    use objc2::rc::Retained;
    use objc2_core_foundation::CGSize;
    use objc2_foundation::{NSError, NSString, NSURL};
    use objc2_quick_look_thumbnailing::{
        QLThumbnailGenerationRequest, QLThumbnailGenerationRequestRepresentationTypes,
        QLThumbnailGenerator, QLThumbnailRepresentation,
    };

    if !path.exists() {
        bail!("{} does not exist", path.display());
    }

    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let request = unsafe {
        QLThumbnailGenerationRequest::initWithFileAtURL_size_scale_representationTypes(
            QLThumbnailGenerationRequest::alloc(),
            &url,
            CGSize::new(f64::from(px), f64::from(px)),
            1.0,
            QLThumbnailGenerationRequestRepresentationTypes::All,
        )
    };

    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Thumbnail, String>>(1);
    let handler = RcBlock::new(
        move |representation: *mut QLThumbnailRepresentation, error: *mut NSError| {
            let outcome = if representation.is_null() {
                let detail = unsafe { error.as_ref() }
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "no representation and no error".to_string());
                Err(detail)
            } else {
                // Retain for the duration of the conversion; QuickLook owns the
                // object only for the length of this callback.
                let representation =
                    unsafe { Retained::retain(representation) }.expect("non-null representation");
                let cg_image = unsafe { representation.CGImage() };
                cg_image_to_thumbnail(&cg_image).map_err(|e| e.to_string())
            };
            // A full channel means we already timed out and stopped listening.
            let _ = tx.try_send(outcome);
        },
    );

    let generator = unsafe { QLThumbnailGenerator::sharedGenerator() };
    unsafe { generator.generateBestRepresentationForRequest_completionHandler(&request, &handler) };

    match rx.recv_timeout(QUICKLOOK_TIMEOUT) {
        Ok(Ok(thumbnail)) => Ok(thumbnail),
        Ok(Err(detail)) => bail!("{detail}"),
        Err(_) => {
            unsafe { generator.cancelRequest(&request) };
            bail!("timed out after {}s", QUICKLOOK_TIMEOUT.as_secs());
        }
    }
}

/// Draw a `CGImage` into a device-RGB bitmap context and hand back straight
/// (non-premultiplied) RGBA8 — the shape [`Thumbnail`] promises.
///
/// `CGBitmapContext` cannot produce non-premultiplied RGBA directly
/// (`kCGImageAlphaLast` is not a supported pixel format), so the draw is
/// premultiplied and then undone below. Opaque pixels — which thumbnails
/// mostly are — are unchanged by that step.
fn cg_image_to_thumbnail(image: &objc2_core_graphics::CGImage) -> Result<Thumbnail> {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
        CGImageByteOrderInfo,
    };

    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    if width == 0 || height == 0 {
        bail!("quicklook returned a {width}x{height} image");
    }
    let bytes_per_row = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("image row overflow at width {width}"))?;
    let len = bytes_per_row
        .checked_mul(height)
        .ok_or_else(|| anyhow!("image size overflow at {width}x{height}"))?;

    let mut pixels = vec![0u8; len];
    let color_space =
        CGColorSpace::new_device_rgb().ok_or_else(|| anyhow!("no device RGB color space"))?;
    // 32-bit big-endian byte order + alpha-last is the one configuration whose
    // in-memory layout is literally R, G, B, A per pixel — which is what
    // `Thumbnail` promises. (The host-endian variants would hand back BGRA.)
    let bitmap_info = CGImageByteOrderInfo::Order32Big.0 | CGImageAlphaInfo::PremultipliedLast.0;
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            width,
            height,
            8,
            bytes_per_row,
            Some(&color_space),
            bitmap_info,
        )
    }
    .ok_or_else(|| anyhow!("could not create a {width}x{height} bitmap context"))?;

    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(width as f64, height as f64),
    );
    CGContext::draw_image(Some(&context), rect, Some(image));
    drop(context); // flushes the draw and releases the borrow of `pixels`

    unpremultiply(&mut pixels);
    Thumbnail::new(
        u32::try_from(width).map_err(|_| anyhow!("width {width} exceeds u32"))?,
        u32::try_from(height).map_err(|_| anyhow!("height {height} exceeds u32"))?,
        pixels,
    )
}

/// Undo alpha premultiplication in place, skipping the opaque and fully
/// transparent pixels (which need no work).
fn unpremultiply(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        let alpha = px[3];
        if alpha == 0 || alpha == 0xff {
            continue;
        }
        for channel in &mut px[..3] {
            *channel = ((u32::from(*channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha))
                .min(255) as u8;
        }
    }
}

/// Fallback: decode `path` as an image file ourselves and downscale it to fit
/// `px`. Blocking; only reached when QuickLook declined.
fn decode_image_blocking(path: &Path, px: u32) -> Result<Thumbnail> {
    let source = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("sniffing the format of {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {} as an image", path.display()))?;

    // `DynamicImage::thumbnail` preserves the aspect ratio but *does* scale up
    // a source smaller than the box, which the trait forbids (and which would
    // waste cache budget on invented pixels), so shrink only when it is
    // actually a shrink.
    let rgba = if source.width() > px || source.height() > px {
        source.thumbnail(px, px).to_rgba8()
    } else {
        source.to_rgba8()
    };
    let (width, height) = (rgba.width(), rgba.height());
    Thumbnail::new(width, height, rgba.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::TestSpawner;
    use futures::executor::block_on;

    /// A 4x2 PNG written to a temp dir — a real file on disk, because both
    /// paths under test (QuickLook and the fallback decoder) read the disk.
    fn png_fixture(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("fixture.png");
        let mut pixels = image::RgbaImage::new(4, 2);
        for (x, y, px) in pixels.enumerate_pixels_mut() {
            *px = image::Rgba([(x * 60) as u8, (y * 120) as u8, 30, 255]);
        }
        pixels.save(&path).expect("write png fixture");
        path
    }

    /// Independent proof that what [`write_tags_blocking`] leaves on disk is a
    /// *Finder* tag, not merely something our own reader accepts: it asks
    /// **Foundation's public tag API** (`NSURL`'s `NSURLTagNamesKey`, the same
    /// key Finder and every tag-aware app use) what the file's tags are. That
    /// code path shares nothing with ours — different framework, no xattr call,
    /// no plist parsing of ours — so agreement means the on-disk format is
    /// right. `tests/tags.rs` pins the bytes; this pins the interpretation.
    #[allow(unused_unsafe)] // see volumes_blocking
    fn foundation_tag_names(path: &Path) -> Vec<String> {
        use objc2_foundation::{NSArray, NSString, NSURL, NSURLResourceKey, NSURLTagNamesKey};

        let keys: [&NSURLResourceKey; 1] = unsafe { [NSURLTagNamesKey] };
        let keys = NSArray::from_slice(&keys);
        let url = unsafe { NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap())) };
        let values = unsafe { url.resourceValuesForKeys_error(&keys) }.expect("resource values");
        let Some(names) = (unsafe { values.objectForKey(NSURLTagNamesKey) }) else {
            return Vec::new(); // no tags at all
        };
        names
            .downcast_ref::<NSArray>()
            .expect("NSURLTagNamesKey is an array")
            .iter()
            .filter_map(|item| item.downcast_ref::<NSString>().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn foundations_own_tag_api_sees_the_tags_we_write() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tagged.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(foundation_tag_names(&file).is_empty(), "starts untagged");

        write_tags_blocking(
            &file,
            &[Tag::new("Red", TagColor::Red), Tag::uncolored("Quarterly")],
        )
        .unwrap();
        assert_eq!(foundation_tag_names(&file), ["Red", "Quarterly"]);

        // …and clearing them makes Foundation agree the file is untagged.
        write_tags_blocking(&file, &[]).unwrap();
        assert!(foundation_tag_names(&file).is_empty());
    }

    /// The reverse direction through Foundation: tags set with
    /// `NSURL setResourceValue:forKey:` (what a tag-aware Cocoa app does) are
    /// what [`read_tags_blocking`] returns.
    #[test]
    #[allow(unused_unsafe)] // see volumes_blocking
    fn tags_set_through_foundation_are_read_back() {
        use objc2_foundation::{NSArray, NSString, NSURL, NSURLTagNamesKey};

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cocoa.txt");
        std::fs::write(&file, b"x").unwrap();

        let url = unsafe { NSURL::fileURLWithPath(&NSString::from_str(file.to_str().unwrap())) };
        let names = NSArray::from_retained_slice(&[
            NSString::from_str("Green"),
            NSString::from_str("Later"),
        ]);
        unsafe { url.setResourceValue_forKey_error(Some(&names), NSURLTagNamesKey) }
            .expect("set tag names");

        let tags = read_tags_blocking(&file).unwrap();
        let read_names: Vec<&str> = tags.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(read_names, ["Green", "Later"]);
        // Foundation resolves the standard colour for "Green" itself, which is
        // exactly the interop we care about.
        assert_eq!(tags[0].color, TagColor::Green);
    }

    #[test]
    fn thumbnail_of_a_real_image_never_exceeds_the_requested_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = png_fixture(dir.path());
        let platform = MacPlatform::new(Arc::new(TestSpawner::new()));

        // Whichever tier answers (QuickLook on a normal Mac, the `image`
        // fallback on a machine where it is unavailable), the contract holds.
        let thumbnail = block_on(platform.thumbnail(&path, 32)).expect("thumbnail");
        assert!(thumbnail.width() <= 32 && thumbnail.height() <= 32);
        assert_eq!(
            thumbnail.byte_len(),
            (thumbnail.width() * thumbnail.height() * 4) as usize
        );
    }

    #[test]
    fn thumbnail_rejects_an_out_of_range_size_without_touching_the_disk() {
        let platform = MacPlatform::new(Arc::new(TestSpawner::new()));
        let err = block_on(platform.thumbnail(std::path::Path::new("/nope"), 0)).unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn thumbnail_of_a_missing_path_reports_both_tiers() {
        let platform = MacPlatform::new(Arc::new(TestSpawner::new()));
        let missing = std::path::Path::new("/definitely/not/here.bin");
        let err = block_on(platform.thumbnail(missing, 64)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("quicklook:"), "{message}");
        assert!(message.contains("image decode:"), "{message}");
    }

    #[test]
    fn fallback_decode_downscales_and_never_upscales() {
        let dir = tempfile::tempdir().unwrap();
        let path = png_fixture(dir.path());

        // The fixture is 4x2, smaller than the request: no upscaling.
        let small = decode_image_blocking(&path, 64).unwrap();
        assert_eq!((small.width(), small.height()), (4, 2));

        // And a request below the source size fits inside it, aspect kept.
        let big = image::RgbaImage::new(400, 100);
        let big_path = dir.path().join("wide.png");
        big.save(&big_path).unwrap();
        let scaled = decode_image_blocking(&big_path, 40).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (40, 10));
    }

    #[test]
    fn fallback_decode_rejects_a_non_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"not an image").unwrap();
        let err = decode_image_blocking(&path, 64).unwrap_err();
        assert!(format!("{err:#}").contains("notes.txt"), "{err:#}");
    }

    #[test]
    fn file_attrs_reports_the_real_mode_owner_and_type() {
        use std::os::unix::fs::PermissionsExt as _;

        let before = SystemTime::now() - Duration::from_secs(1);
        let dir = tempfile::tempdir().unwrap();
        let path = png_fixture(dir.path());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let platform = MacPlatform::new(Arc::new(TestSpawner::new()));
        let attrs = block_on(platform.file_attrs(&path)).expect("attrs");

        let perms = attrs.perms.expect("a mode on a real APFS file");
        assert_eq!(perms.octal(), "640");
        assert_eq!(perms.symbolic(), "rw-r-----");
        // Owner and group always resolve to *something* — a name, or the id.
        assert!(attrs.owner.is_some_and(|o| !o.is_empty()));
        assert!(attrs.group.is_some_and(|g| !g.is_empty()));
        assert!(!attrs.locked, "a freshly written file is not locked");
        // Foundation describes a .png; the exact wording is localized, so only
        // its presence is asserted.
        assert!(
            attrs.type_description.is_some(),
            "no localized type description"
        );
        // Date Added is recorded by APFS, so on a temp dir it is always there
        // and always inside the window the fixture was written in. Asserting
        // only `is_none_or(post-epoch)` would pass even if the key were misread
        // or `system_time_from` returned `None` for everything.
        let added = attrs.added.expect("APFS records Date Added");
        assert!(
            added >= before && added <= SystemTime::now() + Duration::from_secs(1),
            "Date Added {added:?} is outside the window the fixture was written in"
        );
    }

    #[test]
    fn system_time_from_handles_both_sides_of_the_epoch_and_rejects_the_rest() {
        use objc2_foundation::NSDate;

        let at = |seconds: f64| NSDate::dateWithTimeIntervalSince1970(seconds);
        assert_eq!(
            system_time_from(&at(0.0)),
            Some(std::time::UNIX_EPOCH),
            "the epoch itself"
        );
        assert_eq!(
            system_time_from(&at(1.0)),
            Some(std::time::UNIX_EPOCH + Duration::from_secs(1))
        );
        assert_eq!(
            // 1969: `NSDate` predates the unix epoch happily, and a sign slip
            // here would report a *future* date.
            system_time_from(&at(-86_400.0)),
            Some(std::time::UNIX_EPOCH - Duration::from_secs(86_400))
        );
        assert_eq!(system_time_from(&at(f64::NAN)), None);
        assert_eq!(system_time_from(&at(f64::INFINITY)), None);
    }

    #[test]
    fn file_attrs_of_a_missing_path_fails() {
        let platform = MacPlatform::new(Arc::new(TestSpawner::new()));
        let err = block_on(platform.file_attrs(Path::new("/definitely/not/here.bin"))).unwrap_err();
        assert!(format!("{err:#}").contains("here.bin"), "{err:#}");
    }

    #[test]
    fn file_attrs_describes_the_symlink_itself_not_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"payload").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let platform = MacPlatform::new(Arc::new(TestSpawner::new()));
        let attrs = block_on(platform.file_attrs(&link)).expect("attrs");
        // lstat, so the mode is the link's own — never the target's.
        let expected = {
            use std::os::macos::fs::MetadataExt as _;
            UnixPerms::from_mode(std::fs::symlink_metadata(&link).unwrap().st_mode())
        };
        assert_eq!(attrs.perms, Some(expected));
        assert_ne!(
            attrs.perms.expect("mode").octal(),
            "644",
            "the target's mode leaked through: stat was used instead of lstat"
        );
    }

    /// The `Chown` "where privileged" path, exercised on a real Mac: an
    /// unprivileged process must get a plain error and leave the file alone —
    /// never a panic, never a half-applied change. Skipped when the test
    /// happens to run as root, where the give-away would actually succeed.
    #[test]
    fn chown_to_root_is_refused_cleanly_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("owned.txt");
        std::fs::write(&file, b"mine").unwrap();
        let owner_before = account_names(&file).unwrap().owner;
        if owner_before.as_deref() == Some("root") {
            return; // running as root: nothing to prove
        }

        let error = set_ownership_blocking(&file, Some("root"), None).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("chown"), "{message}");
        assert!(
            message.contains(&file.display().to_string()),
            "the message names the file the UI must report: {message}"
        );
        assert_eq!(
            account_names(&file).unwrap().owner,
            owner_before,
            "a refused chown leaves the owner exactly as it was"
        );
    }

    #[test]
    fn chown_to_an_unknown_account_is_an_error_not_a_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("owned.txt");
        std::fs::write(&file, b"mine").unwrap();
        let group_before = account_names(&file).unwrap().group;

        let error =
            set_ownership_blocking(&file, None, Some("no-such-group-exists-here")).unwrap_err();
        assert!(error.to_string().contains("chown"), "{error}");
        assert_eq!(account_names(&file).unwrap().group, group_before);
    }

    /// Asking for nothing does nothing — and in particular does not touch the
    /// file, so the info panel can call the op unconditionally.
    #[test]
    fn chown_with_neither_half_set_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("owned.txt");
        std::fs::write(&file, b"mine").unwrap();
        set_ownership_blocking(&file, None, None).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"mine");
    }

    /// The group half must be settable by an unprivileged process for any group
    /// the user belongs to — the realistic half of "where privileged".
    #[test]
    fn chown_to_one_of_the_users_own_groups_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("owned.txt");
        std::fs::write(&file, b"mine").unwrap();
        let group_before = account_names(&file)
            .unwrap()
            .group
            .expect("a group name on a temp file");

        // Setting the group to the one it already has needs no privilege at all.
        set_ownership_blocking(&file, None, Some(&group_before)).unwrap();
        assert_eq!(
            account_names(&file).unwrap().group.as_deref(),
            Some(group_before.as_str())
        );
    }

    #[test]
    fn account_names_fall_back_to_the_numeric_ids() {
        let named = AccountNames {
            owner: Some("alice".into()),
            group: Some("staff".into()),
        };
        assert_eq!(
            named.into_pair_or(501, 20),
            ("alice".to_string(), "staff".to_string())
        );
        assert_eq!(
            AccountNames::default().into_pair_or(501, 20),
            ("501".to_string(), "20".to_string())
        );
    }

    #[test]
    fn unpremultiply_restores_straight_alpha_and_leaves_opaque_pixels_alone() {
        // Half-alpha grey premultiplied (64 over alpha 128) is 128 straight.
        let mut pixels = vec![64, 64, 64, 128, 10, 20, 30, 255, 0, 0, 0, 0];
        unpremultiply(&mut pixels);
        assert_eq!(&pixels[0..4], &[128, 128, 128, 128]);
        assert_eq!(&pixels[4..8], &[10, 20, 30, 255], "opaque is untouched");
        assert_eq!(&pixels[8..12], &[0, 0, 0, 0], "transparent is untouched");
    }
}

/// The extended attribute Finder stores tags in. The `com.apple.metadata:`
/// prefix is what makes Spotlight index it (and what makes the tags survive a
/// copy through Finder), so the full name is load-bearing.
const USER_TAGS_XATTR: &str = "com.apple.metadata:_kMDItemUserTags";

/// Finder tags on `path`. Blocking — always called through `unblock`.
///
/// A missing attribute, an empty payload, or a filesystem that does not support
/// extended attributes all mean the same user-visible thing: no tags. Only a
/// real failure (permission denied, path gone) or a payload that is not a plist
/// array of strings is an error — a corrupt payload is reported loudly rather
/// than silently treated as "no tags", because the next write would overwrite
/// whatever it actually held.
fn read_tags_blocking(path: &Path) -> Result<Vec<Tag>> {
    let Some(bytes) = xattr::get(path, USER_TAGS_XATTR)? else {
        return Ok(Vec::new());
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let strings = plist_strings(&bytes)
        .with_context(|| format!("reading Finder tags of {}", path.display()))?;
    Ok(decode_tag_strings(&strings))
}

/// Replace the tag set on `path`. Blocking — always called through `unblock`.
///
/// Empty ⇒ remove the attribute entirely (see [`Platform::write_tags`]).
/// Removing an attribute that is not there is success, not an error, so
/// clearing an untagged file is a no-op rather than a spurious failure.
fn write_tags_blocking(path: &Path, tags: &[Tag]) -> Result<()> {
    let strings = encode_tag_strings(tags);
    if strings.is_empty() {
        return xattr::remove(path, USER_TAGS_XATTR)
            .with_context(|| format!("clearing Finder tags of {}", path.display()));
    }
    let plist = plist_data(&strings)?;
    xattr::set(path, USER_TAGS_XATTR, &plist)
        .with_context(|| format!("writing Finder tags of {}", path.display()))
}

/// Standard palette first, then any of the user's Finder favourite tag names we
/// do not already have. Never fails: an unreadable preferences file just means
/// the palette.
fn known_tags_blocking() -> Result<Vec<Tag>> {
    let mut known = standard_tags();
    for name in favorite_tag_names().unwrap_or_default() {
        if name.trim().is_empty() || known.iter().any(|tag| tag.name.as_ref() == name) {
            continue;
        }
        // Finder's preferences record favourites by *name* only; the colour
        // assignments live in the user's SyncedPreferences store, which is not
        // a documented format. A favourite whose name is not one of the seven
        // standard colour names therefore comes back uncoloured — recorded as a
        // Known gap in AS_BUILT, and harmless: the sidebar shows the tag, just
        // with no dot until the user's own files reveal its colour.
        let color = TagColor::from_standard_name(&name).unwrap_or(TagColor::None);
        known.push(Tag::new(name, color));
    }
    Ok(known)
}

/// The `FavoriteTagNames` array out of `~/Library/Preferences/com.apple.finder.plist`
/// — the tags Finder itself offers in its sidebar. Read straight from the file
/// with Foundation rather than through `NSUserDefaults`/`CFPreferences`, which
/// would read *our* app's domain unless we asked for a suite and would drag in
/// another objc2-foundation feature; the file is the user's own and the value is
/// advisory, so a stale read costs nothing. `None` on any failure at all.
fn favorite_tag_names() -> Option<Vec<String>> {
    use objc2_foundation::{NSDictionary, NSString};

    let home = std::env::var_os("HOME")?;
    let plist = PathBuf::from(home).join("Library/Preferences/com.apple.finder.plist");
    let bytes = std::fs::read(plist).ok()?;
    let object = plist_object(&bytes).ok()?;
    let dictionary = object.downcast_ref::<NSDictionary>()?;
    let favorites = dictionary.objectForKey(&*NSString::from_str("FavoriteTagNames"))?;
    let array = favorites.downcast_ref::<objc2_foundation::NSArray>()?;
    Some(
        array
            .iter()
            .filter_map(|item| item.downcast_ref::<NSString>().map(|s| s.to_string()))
            .collect(),
    )
}

/// Serialize `strings` as a **binary** plist array — the format Finder writes,
/// so a file tagged here is byte-shaped like a file tagged there.
#[allow(unused_unsafe)] // see volumes_blocking
fn plist_data(strings: &[String]) -> Result<Vec<u8>> {
    use objc2_foundation::{NSArray, NSPropertyListFormat, NSPropertyListSerialization, NSString};

    let items: Vec<objc2::rc::Retained<NSString>> =
        strings.iter().map(|s| NSString::from_str(s)).collect();
    let refs: Vec<&NSString> = items.iter().map(|s| &**s).collect();
    let array = NSArray::from_slice(&refs);
    let data = unsafe {
        NSPropertyListSerialization::dataWithPropertyList_format_options_error(
            &array,
            NSPropertyListFormat::BinaryFormat_v1_0,
            0,
        )
    }
    .map_err(|error| anyhow!("serializing tag plist: {error}"))?;
    Ok(data.to_vec())
}

/// Parse a plist payload into an object. Accepts **either** binary or XML:
/// Foundation sniffs the format, which matters because third-party taggers
/// (and `xattr -w` by hand) do write XML.
#[allow(unused_unsafe)] // see volumes_blocking
fn plist_object(bytes: &[u8]) -> Result<objc2::rc::Retained<objc2::runtime::AnyObject>> {
    use objc2_foundation::{NSData, NSPropertyListMutabilityOptions, NSPropertyListSerialization};

    let data = NSData::with_bytes(bytes);
    unsafe {
        NSPropertyListSerialization::propertyListWithData_options_format_error(
            &data,
            NSPropertyListMutabilityOptions::Immutable,
            std::ptr::null_mut(),
        )
    }
    .map_err(|error| anyhow!("not a property list: {error}"))
}

/// The strings of a top-level plist array. A payload that is not an array is an
/// error (see [`read_tags_blocking`]); a *non-string element* inside the array
/// is skipped, since one odd element is no reason to drop the other tags.
fn plist_strings(bytes: &[u8]) -> Result<Vec<String>> {
    use objc2_foundation::{NSArray, NSString};

    let object = plist_object(bytes)?;
    let array = object
        .downcast_ref::<NSArray>()
        .ok_or_else(|| anyhow!("tag payload is a property list but not an array"))?;
    Ok(array
        .iter()
        .filter_map(|item| item.downcast_ref::<NSString>().map(|s| s.to_string()))
        .collect())
}

/// The three extended-attribute syscalls, declared here rather than taken from
/// `libc` or the `xattr` crate: a new dependency costs a full workspace rebuild
/// (CLAUDE.md) for three stable BSD entry points, and the same argument already
/// justifies [`UF_IMMUTABLE`] above. Only these three, only for this file's use.
///
/// `options` is left at `0` throughout, i.e. symlinks are **followed** —
/// matching the `xattr` command-line tool's default and Finder's behaviour of
/// tagging what the user thinks they clicked. (`file_attrs` uses `lstat`
/// deliberately, but a *mode* belongs to the link while a *tag* belongs to the
/// item.)
mod xattr {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use anyhow::{Result, anyhow, bail};

    unsafe extern "C" {
        fn getxattr(
            path: *const c_char,
            name: *const c_char,
            value: *mut c_void,
            size: usize,
            position: u32,
            options: c_int,
        ) -> isize;
        fn setxattr(
            path: *const c_char,
            name: *const c_char,
            value: *const c_void,
            size: usize,
            position: u32,
            options: c_int,
        ) -> c_int;
        fn removexattr(path: *const c_char, name: *const c_char, options: c_int) -> c_int;
    }

    /// `<sys/xattr.h>`: no such attribute on this file — the overwhelmingly
    /// common case, and not an error.
    const ENOATTR: i32 = 93;
    /// `<sys/errno.h>`: the filesystem does not do extended attributes.
    const ENOTSUP: i32 = 45;
    /// `<sys/errno.h>`: the buffer we sized from the previous call is now too
    /// small, because someone else rewrote the attribute in between.
    const ERANGE: i32 = 34;

    /// How many times to re-size the buffer before giving up on a value that
    /// keeps growing under us. Three is generous for a tag list.
    const MAX_RESIZE_ATTEMPTS: usize = 3;

    fn c_path(path: &Path) -> Result<CString> {
        // Bytes, not `to_string_lossy`: a lossy path would address a *different*
        // file, and tagging the wrong file is worse than failing.
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| anyhow!("path contains an interior NUL: {}", path.display()))
    }

    /// The raw attribute value, or `None` when the file has no such attribute
    /// or the filesystem has no extended attributes at all.
    pub(super) fn get(path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
        let c_path = c_path(path)?;
        let c_name = CString::new(name)?;
        for _ in 0..MAX_RESIZE_ATTEMPTS {
            let size = unsafe {
                getxattr(
                    c_path.as_ptr(),
                    c_name.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                )
            };
            if size < 0 {
                return absent_or_error(path, name, "getxattr").map(|()| None);
            }
            let mut buffer = vec![0u8; size as usize];
            let read = unsafe {
                getxattr(
                    c_path.as_ptr(),
                    c_name.as_ptr(),
                    buffer.as_mut_ptr().cast::<c_void>(),
                    buffer.len(),
                    0,
                    0,
                )
            };
            if read >= 0 {
                buffer.truncate(read as usize);
                return Ok(Some(buffer));
            }
            if io::Error::last_os_error().raw_os_error() == Some(ERANGE) {
                continue; // grew between the sizing call and the read — retry
            }
            return absent_or_error(path, name, "getxattr").map(|()| None);
        }
        bail!(
            "{} of {} kept changing size while reading it",
            name,
            path.display()
        )
    }

    /// Replace the attribute's value.
    pub(super) fn set(path: &Path, name: &str, value: &[u8]) -> Result<()> {
        let c_path = c_path(path)?;
        let c_name = CString::new(name)?;
        let status = unsafe {
            setxattr(
                c_path.as_ptr(),
                c_name.as_ptr(),
                value.as_ptr().cast::<c_void>(),
                value.len(),
                0,
                0,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "setxattr {name} on {}: {}",
                path.display(),
                io::Error::last_os_error()
            ))
        }
    }

    /// Remove the attribute. Removing one that is not there succeeds.
    pub(super) fn remove(path: &Path, name: &str) -> Result<()> {
        let c_path = c_path(path)?;
        let c_name = CString::new(name)?;
        let status = unsafe { removexattr(c_path.as_ptr(), c_name.as_ptr(), 0) };
        if status == 0 {
            return Ok(());
        }
        absent_or_error(path, name, "removexattr")
    }

    /// Map the current `errno` to either "there is no such attribute here"
    /// (`Ok`) or a real error.
    fn absent_or_error(path: &Path, name: &str, call: &str) -> Result<()> {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(ENOATTR) | Some(ENOTSUP) => Ok(()),
            _ => Err(anyhow!("{call} {name} on {}: {error}", path.display())),
        }
    }
}
