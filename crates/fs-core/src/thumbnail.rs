//! Thumbnails: the pixel type the app hands to gpui, and the LRU **byte-budget**
//! cache that bounds their memory (ARCHITECTURE.md §"M4 — Icon view + dual pane":
//! `Platform::thumbnail` + LRU byte-budget cache).
//!
//! Producing thumbnails is a [`crate::Platform`] concern (QuickLook on macOS, a
//! synthesized pattern in the stub); *keeping* them is this module's, and both
//! halves are headless — no gpui, no rendering, no UI-thread work.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Result, bail};

use crate::entry::{EntryMeta, FileEntry};

/// A decoded thumbnail: tightly packed, **non-premultiplied RGBA8** pixels in
/// row-major top-down order, plus their dimensions.
///
/// ## Why decoded pixels and not an encoded blob
///
/// [`Platform::thumbnail`](crate::Platform::thumbnail) could just as easily hand
/// back PNG bytes, and that would be smaller. Decoded RGBA wins on three counts
/// that matter more here:
///
/// 1. **The decode stays off the UI thread.** gpui ingests images as raw RGBA
///    (`gpui::RenderImage` wraps frames of RGBA pixels). Returning an encoded
///    blob would only move the decode later — and "later" is the render pass,
///    which is exactly where the plan forbids work (`docs/file-explorer-plan.md`
///    §"Threading rule"). fs-core owns the background executor seam, so fs-core
///    is the right place to finish the job.
/// 2. **The byte budget becomes exact.** The cache below evicts on *bytes
///    resident*, and the resident cost of a decoded thumbnail is exactly
///    `width * height * 4`. A compressed blob's real cost is whatever it
///    inflates to at paint time, which the budget could not see — an entropy-
///    dependent, silently-wrong budget is not a budget.
/// 3. **No format contract leaks across the seam.** RGBA8 is the one shape every
///    consumer already wants; an encoded blob would force fs-core to name a
///    container (PNG? TIFF?) and the app to keep a decoder that agrees with it.
///
/// The pixels live behind an [`Arc`] so a cache hit, and handing the result to
/// the app, are both pointer copies rather than megabyte memcpys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

impl Thumbnail {
    /// Wrap RGBA8 pixels, verifying they match `width * height * 4`.
    ///
    /// Fails rather than panics: the bytes come from OS thumbnail APIs and
    /// image decoders, so a mismatch is a runtime condition, not a bug in the
    /// caller.
    pub fn new(width: u32, height: u32, rgba: impl Into<Arc<[u8]>>) -> Result<Self> {
        let rgba = rgba.into();
        if width == 0 || height == 0 {
            bail!("thumbnail has a zero dimension ({width}x{height})");
        }
        let expected = expected_len(width, height)?;
        if rgba.len() != expected {
            bail!(
                "thumbnail pixel length {} does not match {width}x{height} RGBA ({expected})",
                rgba.len()
            );
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The pixels: `width * height * 4` bytes, RGBA8, top-down, no padding.
    pub fn rgba(&self) -> &Arc<[u8]> {
        &self.rgba
    }

    /// Resident cost, and the unit [`ThumbnailCache`] budgets in.
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

fn expected_len(width: u32, height: u32) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("thumbnail dimensions overflow: {width}x{height}"))
}

/// Largest thumbnail edge any [`Platform::thumbnail`](crate::Platform::thumbnail)
/// implementation will produce. Beyond this a single thumbnail costs more than
/// 64 MB, which no icon tile needs and [`ThumbnailCache`]'s default budget
/// would refuse outright.
pub const MAX_PX: u32 = 4096;

/// Reject a nonsensical requested size before doing any work. Shared by every
/// `Platform` implementation so the contract is one rule, not three.
pub fn validate_px(px: u32) -> Result<u32> {
    if px == 0 || px > MAX_PX {
        bail!("thumbnail size {px}px is out of range (1..={MAX_PX})");
    }
    Ok(px)
}

/// The cheap "has this file changed underneath us?" witness: last-modified time
/// plus size.
///
/// It is part of the cache key, not a side note. A thumbnail keyed on path
/// alone survives an edit of the file it depicts, and a picture of the *old*
/// contents is a real bug, not a cosmetic one. `mtime` catches an edit that
/// preserves size; `size` catches a write that a coarse or backdated mtime
/// hides. Both are already on [`FileEntry`], so the icon grid can build a key
/// from a listing row it already has, with no extra `stat`.
///
/// The mtime is stored as nanoseconds since the Unix epoch (saturating at zero
/// for pre-epoch times) so the key is `Hash + Eq`, which [`SystemTime`] is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentStamp {
    modified_nanos: u128,
    size: u64,
}

impl ContentStamp {
    pub fn new(modified: SystemTime, size: u64) -> Self {
        let modified_nanos = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            modified_nanos,
            size,
        }
    }

    /// Stamp for a listing row — the form the icon grid uses.
    pub fn from_entry(entry: &FileEntry) -> Self {
        Self::new(entry.modified, entry.size)
    }

    /// Stamp from a bare `stat` ([`crate::Vfs::metadata`]).
    pub fn from_meta(meta: &EntryMeta) -> Self {
        Self::new(meta.modified, meta.size)
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn modified_nanos(&self) -> u128 {
        self.modified_nanos
    }
}

/// Cache identity of one thumbnail: which file, at which pixel size, of which
/// version of its contents.
///
/// `px` is part of the key because the same file at 48px and at 256px are
/// different images — sharing one entry between them would either blur the big
/// tile or waste memory on the small one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThumbnailKey {
    pub path: Arc<Path>,
    /// Requested longest edge, in pixels (see [`crate::Platform::thumbnail`]).
    pub px: u32,
    pub stamp: ContentStamp,
}

impl ThumbnailKey {
    pub fn new(path: Arc<Path>, px: u32, stamp: ContentStamp) -> Self {
        Self { path, px, stamp }
    }

    /// Key for a listing row at `px`.
    pub fn for_entry(entry: &FileEntry, px: u32) -> Self {
        Self::new(entry.path.clone(), px, ContentStamp::from_entry(entry))
    }

    /// Same file, same size — ignoring which version of the contents. The
    /// cache slot a fresh thumbnail *replaces*.
    fn same_slot(&self, other: &Self) -> bool {
        self.px == other.px && self.path == other.path
    }
}

/// LRU cache of decoded thumbnails bounded by **bytes resident**, not entry
/// count (ARCHITECTURE.md §M4).
///
/// An entry-count cap cannot bound thumbnail memory: 64 entries is 590 KB of
/// 64px tiles or 67 MB of 2048px ones. So the budget is bytes, insertion evicts
/// least-recently-used entries until the newcomer fits, and a single thumbnail
/// larger than the entire budget is **rejected rather than admitted** — it can
/// neither be stored nor wedge the cache by evicting everything and still not
/// fitting.
///
/// Styled after [`ListingCache`](crate::listing::ListingCache): a MRU-first
/// deque, `get` promotes, `insert` is the write-back.
pub struct ThumbnailCache {
    budget_bytes: usize,
    used_bytes: usize,
    /// Most-recently-used first.
    entries: VecDeque<(ThumbnailKey, Thumbnail)>,
}

impl ThumbnailCache {
    /// Default budget: 64 MB — roughly 1000 tiles at 128px, comfortably more
    /// than the visible-plus-margin working set the icon grid asks for.
    pub const DEFAULT_BUDGET_BYTES: usize = 64 * 1024 * 1024;

    pub fn new(budget_bytes: usize) -> Self {
        assert!(budget_bytes > 0, "ThumbnailCache budget must be nonzero");
        Self {
            budget_bytes,
            used_bytes: 0,
            entries: VecDeque::new(),
        }
    }

    /// A hit returns the thumbnail (a pointer copy) and marks it
    /// most-recently-used. A key whose `stamp` differs from the cached one
    /// **misses** — that entry pictures contents the file no longer has.
    ///
    /// A miss deliberately does *not* evict the entry it failed to match. The
    /// cache cannot tell which of two stamps is the newer one, so evicting on
    /// mismatch would let a caller holding a stale stamp throw away a
    /// *fresher* thumbnail. The stale bytes are instead reclaimed by the
    /// [`insert`](Self::insert) that follows the miss, which replaces the whole
    /// `(path, px)` slot.
    pub fn get(&mut self, key: &ThumbnailKey) -> Option<Thumbnail> {
        let ix = self.entries.iter().position(|(k, _)| k == key)?;
        let hit = self.entries.remove(ix).expect("index from position");
        let thumbnail = hit.1.clone();
        self.entries.push_front(hit);
        Some(thumbnail)
    }

    /// Store `thumbnail` under `key`, replacing whatever occupied the same
    /// `(path, px)` slot (which is how a re-render after an edit reclaims the
    /// stale entry's bytes), then evict least-recently-used entries until the
    /// total fits the budget.
    ///
    /// Returns `false` when the thumbnail is larger than the whole budget: it
    /// is not stored, and nothing else is evicted for it.
    pub fn insert(&mut self, key: ThumbnailKey, thumbnail: Thumbnail) -> bool {
        self.remove_slot(&key);
        if thumbnail.byte_len() > self.budget_bytes {
            return false;
        }
        self.used_bytes += thumbnail.byte_len();
        self.entries.push_front((key, thumbnail));
        while self.used_bytes > self.budget_bytes {
            let (_, evicted) = self
                .entries
                .pop_back()
                .expect("used_bytes > 0 implies a resident entry");
            self.used_bytes -= evicted.byte_len();
        }
        true
    }

    /// Drop every cached size and version of `path` — the write-back for a
    /// watcher event that removed or replaced the file.
    pub fn invalidate(&mut self, path: &Path) {
        self.retain(|key| &*key.path != path);
    }

    /// Drop everything (e.g. leaving icon view).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.used_bytes = 0;
    }

    /// Bytes currently resident.
    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn remove_slot(&mut self, key: &ThumbnailKey) {
        self.retain(|k| !k.same_slot(key));
    }

    fn retain(&mut self, keep: impl Fn(&ThumbnailKey) -> bool) {
        let mut freed = 0;
        self.entries.retain(|(key, thumbnail)| {
            let keep = keep(key);
            if !keep {
                freed += thumbnail.byte_len();
            }
            keep
        });
        self.used_bytes -= freed;
    }
}

impl Default for ThumbnailCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BUDGET_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn stamp(secs: u64, size: u64) -> ContentStamp {
        ContentStamp::new(SystemTime::UNIX_EPOCH + Duration::from_secs(secs), size)
    }

    fn key(path: &str, px: u32) -> ThumbnailKey {
        ThumbnailKey::new(Arc::from(PathBuf::from(path)), px, stamp(1, 10))
    }

    /// A thumbnail of exactly `bytes` bytes (bytes must be a multiple of 4).
    fn thumb(bytes: usize) -> Thumbnail {
        assert_eq!(bytes % 4, 0);
        Thumbnail::new(bytes as u32 / 4, 1, vec![0u8; bytes]).unwrap()
    }

    #[test]
    fn thumbnail_new_validates_pixel_length() {
        let ok = Thumbnail::new(2, 3, vec![7u8; 24]).unwrap();
        assert_eq!((ok.width(), ok.height(), ok.byte_len()), (2, 3, 24));

        let err = Thumbnail::new(2, 3, vec![0u8; 23]).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        let err = Thumbnail::new(0, 3, Vec::new()).unwrap_err();
        assert!(err.to_string().contains("zero dimension"), "{err}");
    }

    #[test]
    fn cache_hit_returns_the_stored_pixels_and_promotes_to_mru() {
        let mut cache = ThumbnailCache::new(1024);
        let a = thumb(100);
        cache.insert(key("/a", 64), a.clone());
        cache.insert(key("/b", 64), thumb(100));

        let hit = cache.get(&key("/a", 64)).expect("hit");
        assert_eq!(hit, a);
        assert!(
            Arc::ptr_eq(hit.rgba(), a.rgba()),
            "a hit is a pointer copy, not a pixel copy"
        );
        assert!(cache.get(&key("/missing", 64)).is_none());
        assert_eq!(cache.used_bytes(), 200);

        // /a was promoted, so the next eviction takes /b.
        cache.insert(key("/c", 64), thumb(900));
        assert!(cache.get(&key("/a", 64)).is_some());
        assert!(cache.get(&key("/b", 64)).is_none(), "LRU evicted");
    }

    #[test]
    fn eviction_is_least_recently_used_and_only_until_the_newcomer_fits() {
        let mut cache = ThumbnailCache::new(1000);
        for name in ["/a", "/b", "/c", "/d"] {
            cache.insert(key(name, 64), thumb(200));
        }
        assert_eq!((cache.len(), cache.used_bytes()), (4, 800));

        // Touch /a and /b so the LRU order is (mru) b, a, d, c (lru).
        assert!(cache.get(&key("/a", 64)).is_some());
        assert!(cache.get(&key("/b", 64)).is_some());

        // 600 more needs 400 freed: exactly /c and /d, and no more.
        cache.insert(key("/e", 64), thumb(600));
        assert_eq!(cache.used_bytes(), 1000);
        assert!(cache.get(&key("/c", 64)).is_none(), "LRU evicted first");
        assert!(cache.get(&key("/d", 64)).is_none(), "then the next LRU");
        assert!(cache.get(&key("/a", 64)).is_some(), "MRU survives");
        assert!(cache.get(&key("/b", 64)).is_some());
        assert!(cache.get(&key("/e", 64)).is_some());
    }

    #[test]
    fn an_entry_larger_than_the_whole_budget_is_rejected_without_wedging_the_cache() {
        let mut cache = ThumbnailCache::new(1000);
        cache.insert(key("/a", 64), thumb(400));
        cache.insert(key("/b", 64), thumb(400));

        assert!(
            !cache.insert(key("/huge", 64), thumb(2000)),
            "oversized entry is refused"
        );
        assert!(cache.get(&key("/huge", 64)).is_none(), "and not stored");
        assert_eq!(
            (cache.len(), cache.used_bytes()),
            (2, 800),
            "and evicted nothing on its way out"
        );

        // The cache still works afterwards.
        assert!(cache.insert(key("/c", 64), thumb(200)));
        assert_eq!(cache.used_bytes(), 1000);
    }

    #[test]
    fn an_entry_exactly_the_size_of_the_budget_is_admitted_and_takes_the_cache() {
        let mut cache = ThumbnailCache::new(1000);
        cache.insert(key("/a", 64), thumb(400));
        assert!(cache.insert(key("/exact", 64), thumb(1000)));
        assert_eq!((cache.len(), cache.used_bytes()), (1, 1000));
        assert!(cache.get(&key("/exact", 64)).is_some());
    }

    #[test]
    fn a_changed_mtime_or_size_misses_without_evicting_what_it_did_not_match() {
        for changed in [stamp(2, 10), stamp(1, 11)] {
            let mut cache = ThumbnailCache::new(1024);
            let fresh = ThumbnailKey::new(Arc::from(PathBuf::from("/a")), 64, stamp(1, 10));
            cache.insert(fresh.clone(), thumb(100));
            assert_eq!(cache.used_bytes(), 100);

            let after_edit = ThumbnailKey {
                stamp: changed,
                ..fresh.clone()
            };
            assert!(
                cache.get(&after_edit).is_none(),
                "a thumbnail of the old contents must not be served"
            );
            assert_eq!(
                (cache.len(), cache.used_bytes()),
                (1, 100),
                "but a miss must not evict what it failed to match — the cache \
                 cannot tell which stamp is newer, and a caller holding a \
                 stale stamp must not be able to discard a fresher thumbnail"
            );
            assert!(
                cache.get(&fresh).is_some(),
                "so the entry is still there for a caller with the right stamp"
            );
        }
    }

    #[test]
    fn inserting_a_new_version_replaces_the_slot_rather_than_growing_it() {
        let mut cache = ThumbnailCache::new(1024);
        let before = ThumbnailKey::new(Arc::from(PathBuf::from("/a")), 64, stamp(1, 10));
        let after = ThumbnailKey {
            stamp: stamp(9, 10),
            ..before.clone()
        };
        cache.insert(before.clone(), thumb(100));
        cache.insert(after.clone(), thumb(200));

        assert_eq!(
            (cache.len(), cache.used_bytes()),
            (1, 200),
            "the new version took the slot instead of adding to it"
        );
        assert!(cache.get(&after).is_some());
        assert!(cache.get(&before).is_none(), "the old version is gone");
    }

    #[test]
    fn px_is_part_of_the_key() {
        let mut cache = ThumbnailCache::new(1024);
        cache.insert(key("/a", 64), thumb(100));
        assert!(
            cache.get(&key("/a", 128)).is_none(),
            "a different requested size is a different image"
        );
        assert_eq!(
            (cache.len(), cache.used_bytes()),
            (1, 100),
            "and does not disturb the entry it did not match"
        );

        cache.insert(key("/a", 128), thumb(400));
        assert_eq!((cache.len(), cache.used_bytes()), (2, 500));
        assert_eq!(cache.get(&key("/a", 64)).unwrap().byte_len(), 100);
        assert_eq!(cache.get(&key("/a", 128)).unwrap().byte_len(), 400);
    }

    #[test]
    fn invalidate_drops_every_size_and_version_of_one_path() {
        let mut cache = ThumbnailCache::new(4096);
        cache.insert(key("/a", 64), thumb(100));
        cache.insert(key("/a", 256), thumb(200));
        cache.insert(key("/b", 64), thumb(300));

        cache.invalidate(Path::new("/a"));
        assert_eq!((cache.len(), cache.used_bytes()), (1, 300));
        assert!(cache.get(&key("/b", 64)).is_some());

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn content_stamp_is_built_from_listing_rows_and_stat_results() {
        use crate::entry::{EntryKind, EntryMeta, FileEntry};

        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let entry = FileEntry {
            path: Arc::from(PathBuf::from("/a/photo.png")),
            name: "photo.png".into(),
            kind: EntryKind::File,
            size: 4096,
            modified,
            created: None,
            hidden: false,
        };
        let meta = EntryMeta {
            kind: EntryKind::File,
            size: 4096,
            modified,
            created: None,
            hidden: false,
        };
        assert_eq!(
            ContentStamp::from_entry(&entry),
            ContentStamp::from_meta(&meta)
        );
        assert_eq!(ContentStamp::from_entry(&entry).size(), 4096);
        assert_eq!(
            ContentStamp::from_entry(&entry).modified_nanos(),
            1_700_000_000_000_000_000
        );

        // Pre-epoch times saturate instead of panicking.
        let ancient = ContentStamp::new(SystemTime::UNIX_EPOCH - Duration::from_secs(5), 1);
        assert_eq!(ancient.modified_nanos(), 0);
    }

    #[test]
    #[should_panic(expected = "budget must be nonzero")]
    fn a_zero_budget_is_a_programming_error() {
        ThumbnailCache::new(0);
    }
}
