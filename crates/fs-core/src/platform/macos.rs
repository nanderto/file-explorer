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
//! NOTE: this file only compiles under `cfg(target_os = "macos")`. Earlier
//! milestones were written on a Windows machine and checked only by macOS CI;
//! from M4 on it is also compiled and tested locally.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;

use super::{Platform, VolumeId, VolumeInfo};
use crate::exec::{Spawner, SpawnerExt as _};
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
    fn unpremultiply_restores_straight_alpha_and_leaves_opaque_pixels_alone() {
        // Half-alpha grey premultiplied (64 over alpha 128) is 128 straight.
        let mut pixels = vec![64, 64, 64, 128, 10, 20, 30, 255, 0, 0, 0, 0];
        unpremultiply(&mut pixels);
        assert_eq!(&pixels[0..4], &[128, 128, 128, 128]);
        assert_eq!(&pixels[4..8], &[10, 20, 30, 255], "opaque is untouched");
        assert_eq!(&pixels[8..12], &[0, 0, 0, 0], "transparent is untouched");
    }
}
