//! Icon-grid thumbnails (ARCHITECTURE.md §8 "Icon grid — generate only for
//! visible+margin rows", milestone M4).
//!
//! The fifth state machine to live as a **field** of [`DirView`] (after
//! rename, marquee, drop and menu): [`ThumbnailState`] holds the byte-budget
//! [`ThumbnailCache`] fs-core produces into, the handful of GPU-side
//! [`RenderImage`]s the painted tiles actually reference, the keys that have
//! no preview at all, and the **single `Task` slot** that fetches them.
//!
//! Three rules shape it:
//!
//! * **Only visible + margin.** The request window is the band of grid rows
//!   the scroll offset and the list viewport put on screen, widened by
//!   [`MARGIN_ROWS`] lines so a slow scroll finds the next line's previews
//!   already decoded. Deliberately **not** the row range `uniform_list` hands
//!   its processor: gpui calls that processor twice per frame with `0..1`
//!   purely to measure one item (`uniform_list::measure_item`, from both
//!   `request_layout` and `prepaint`), so a window derived from it flips
//!   three times a frame — and since a moved window cancels the fetch in
//!   flight, no thumbnail slower than the repaint cadence would ever finish.
//!   The scroll offset and the viewport are stable for the whole frame, so
//!   [`DirView::render`] is where the window comes from.
//! * **Cancel on scroll-away.** One `Task` fetches the whole window
//!   sequentially; when the window changes, that task is *replaced*, which
//!   drops it — and with it the in-flight `Platform::thumbnail` future for a
//!   tile that is no longer on screen. This is the same single-slot pattern
//!   as the marquee's autoscroll ticker, and the reason a fast scroll through
//!   a 50k-entry photo folder does not queue 50k decodes. Note what "cancel"
//!   can and cannot mean: everything *not yet started* is abandoned, but a
//!   request already handed to the background executor runs to completion and
//!   has its result discarded, because `Spawner::unblock` polls the blocking
//!   closure exactly once (so a `MacPlatform` QuickLook wait keeps a queue
//!   thread until it answers or times out). That is what `Platform::thumbnail`
//!   documents and what makes the head-of-line cost bounded — one orphan per
//!   cancellation — rather than free.
//! * **Nothing blocking on the UI thread.** Each fetch is awaited on the
//!   *background* executor (`cx.background_executor().spawn`), so neither the
//!   QuickLook round-trip nor the `image`-crate fallback decode can land on
//!   the render thread even if a `Platform` implementation forgets to unblock.
//!   Only the cache insert and the `cx.notify()` run on the UI thread.
//!
//! Both the image map and the known-missing set are pruned to the window on
//! every move, keyed on the whole [`ThumbnailKey`] (path **and** content
//! stamp) so a file that is rewritten while visible does not leave its
//! superseded texture behind.
//!
//! An arriving thumbnail never reflows the grid: it replaces the *child* of
//! the tile's fixed-size image slot ([`crate::views::icon_grid::ICON_PX`]),
//! so every hit test derived from the tile lattice is unaffected.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use fs_core::{FileEntry, Thumbnail, ThumbnailCache, ThumbnailKey};
use gpui::{Context, RenderImage, Task, Window};

use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::views::icon_grid::ICON_PX;

/// Longest edge, in pixels, thumbnails are requested at: twice the tile's
/// logical image slot, so the preview is still sharp on a 2x display. The
/// slot's size is what constrains the *painted* size, so this constant can
/// change without touching any geometry.
pub(crate) const THUMBNAIL_PX: u32 = (ICON_PX as u32) * 2;

/// Grid rows of margin kept warm on each side of the painted range.
pub(crate) const MARGIN_ROWS: usize = 1;

/// The grid rows on screen, from the scroll offset and viewport height — the
/// two pieces of state that are stable across every call gpui makes within
/// one frame. `top` is the distance scrolled from the start of the content
/// (so non-negative; the caller flips gpui's negative offset), `height` the
/// list viewport's height.
///
/// Pure, and the whole reason the request window is idempotent: rendering the
/// same frame twice produces the same band, so an in-flight fetch survives.
pub(crate) fn visible_rows(
    top: f32,
    height: f32,
    row_height: f32,
    row_count: usize,
) -> Range<usize> {
    if row_count == 0 || !top.is_finite() || !height.is_finite() || row_height <= 0.0 {
        return 0..0;
    }
    let top = top.max(0.0);
    // A viewport that has not been laid out yet still gets its first line, so
    // the very first thumbnail's arrival is what notifies the real geometry
    // into existence rather than nothing ever being asked for.
    let height = height.max(row_height);
    let first = (top / row_height).floor().max(0.0) as usize;
    let last = ((top + height) / row_height).ceil().max(0.0) as usize;
    let first = first.min(row_count);
    first..last.clamp(first, row_count)
}

/// The entry indices whose thumbnails are worth holding, given the grid
/// **rows** on screen. Pure so the windowing rule can be tested without a
/// window; `saturating` throughout because a row band built before the last
/// re-projection may run past the listing.
pub(crate) fn request_window(
    rows: Range<usize>,
    cols: usize,
    len: usize,
    margin_rows: usize,
) -> Range<usize> {
    if len == 0 || rows.is_empty() {
        return 0..0;
    }
    let cols = cols.max(1);
    let first_row = rows.start.saturating_sub(margin_rows);
    let last_row = rows.end.saturating_add(margin_rows);
    let start = first_row.saturating_mul(cols).min(len);
    let end = last_row.saturating_mul(cols).clamp(start, len);
    start..end
}

/// Per-view thumbnail state (see the module docs).
#[derive(Default)]
pub(crate) struct ThumbnailState {
    /// fs-core's LRU byte-budget cache — the decoded-RGBA source of truth.
    cache: ThumbnailCache,
    /// The GPU-side images built from `cache` for tiles that actually paint.
    /// Pruned to the request window, so this map stays viewport-sized while
    /// the byte budget governs the bitmap bytes behind it.
    images: HashMap<ThumbnailKey, Arc<RenderImage>>,
    /// Keys with no preview available (`Platform::thumbnail` said `Err`).
    /// Remembered so the tile falls back to its type glyph *once* rather than
    /// re-asking on every scroll — the trait's "must not retry in a loop".
    /// Keyed with the content stamp, so an edited file is tried again, and
    /// pruned to the request window alongside `images` so it cannot grow one
    /// key per non-previewable file the pane has ever scrolled past.
    missing: HashSet<ThumbnailKey>,
    /// The window the current task was spawned for.
    window: Option<Range<usize>>,
    /// Whether that task is still working through it.
    fetching: bool,
    /// The single slot. Replacing it cancels the previous fetch.
    _fetch: Option<Task<()>>,
}

impl DirView {
    /// Keep the thumbnails for the rows on screen (and [`MARGIN_ROWS`] either
    /// side) coming. Called once per frame from [`DirView::render`], with the
    /// `cols` that frame is painting with; the row band itself is derived from
    /// the scroll offset and the list viewport, which are the only inputs
    /// stable enough to make this idempotent (see the module docs).
    ///
    /// Idempotent per window: re-rendering the same window — which every
    /// arriving thumbnail, every scrollbar fade, every cursor move inside the
    /// visible band and every watcher patch causes — does not respawn the
    /// fetch, so a slow `Platform::thumbnail` runs to completion instead of
    /// being restarted from scratch by the next repaint.
    pub(crate) fn request_thumbnails(
        &mut self,
        cols: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let len = self.flat_rows().len();
        let viewport = crate::marquee::list_viewport(self);
        let rows = visible_rows(
            -crate::marquee::scroll_y(self),
            f32::from(viewport.size.height),
            crate::views::icon_grid::TILE_HEIGHT,
            crate::views::icon_grid::grid_row_count(len, cols),
        );
        let requested = request_window(rows, cols, len, MARGIN_ROWS);
        let moved = self.thumbnails.window.as_ref() != Some(&requested);
        if moved {
            self.thumbnails.window = Some(requested.clone());
            // Scroll-away: drop the task, cancelling the in-flight fetch of a
            // tile nobody is looking at any more.
            self.thumbnails._fetch = None;
            self.thumbnails.fetching = false;
        }
        // Every pass, not only the ones that scrolled: the keep set is keyed
        // on path *and* content stamp, so a file rewritten under a stationary
        // viewport (a download in progress, a log being appended to) mints a
        // new key and would otherwise leave its predecessor's texture behind
        // for the life of the view. Pruning when nothing changed removes
        // nothing, so this costs one key per visible row and no churn.
        self.prune_thumbnail_images(&requested, window, cx);
        if !moved && self.thumbnails.fetching {
            return;
        }

        let pending = self.pending_thumbnails(&requested);
        if pending.is_empty() {
            return;
        }
        self.spawn_thumbnail_fetch(pending, cx);
    }

    /// Keys inside `window` that are neither decoded, nor known-missing, nor
    /// already in the cache — in paint order, so the tile at the top of the
    /// viewport fills in first.
    fn pending_thumbnails(&mut self, window: &Range<usize>) -> Vec<ThumbnailKey> {
        let candidates: Vec<ThumbnailKey> = self
            .flat_rows()
            .get(window.clone())
            .unwrap_or_default()
            .iter()
            // Folders keep their type glyph: Explorer previews file content,
            // and a folder has none of its own.
            .filter(|row| !row.entry.is_dir_like())
            .map(|row| ThumbnailKey::for_entry(&row.entry, THUMBNAIL_PX))
            .collect();
        let state = &mut self.thumbnails;
        candidates
            .into_iter()
            .filter(|key| {
                !state.images.contains_key(key)
                    && !state.missing.contains(key)
                    && state.cache.get(key).is_none()
            })
            .collect()
    }

    /// The single-slot fetch task: one `Platform::thumbnail` at a time, each
    /// awaited on the background executor, each folded into the cache on the
    /// UI thread and painted by the `notify` that follows.
    fn spawn_thumbnail_fetch(&mut self, keys: Vec<ThumbnailKey>, cx: &mut Context<Self>) {
        let platform = FsContext::global(cx).platform.clone();
        self.thumbnails.fetching = true;
        let task = cx.spawn(async move |this, cx| {
            for key in keys {
                let path = key.path.clone();
                let px = key.px;
                let platform = platform.clone();
                // The UI thread only ever awaits this handle; the QuickLook
                // round-trip and any decode happen on the pool.
                let result = cx
                    .background_executor()
                    .spawn(async move { platform.thumbnail(&path, px).await })
                    .await;
                let alive = this.update(cx, |this, cx| {
                    match result {
                        Ok(thumbnail) => {
                            this.thumbnails.cache.insert(key, thumbnail);
                        }
                        // "No preview available" is an ordinary outcome, not
                        // an error to surface: the tile keeps its type glyph.
                        Err(_) => {
                            this.thumbnails.missing.insert(key);
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, _| this.thumbnails.fetching = false)
                .ok();
        });
        self.thumbnails._fetch = Some(task);
    }

    /// Release the `RenderImage`s for tiles outside `window`, and forget the
    /// "no preview" verdicts for them too. The bitmap bytes stay in the
    /// fs-core cache under its own budget, so scrolling back is a re-upload
    /// rather than a re-decode.
    ///
    /// Keyed on the **whole** [`ThumbnailKey`], not just its path: the key
    /// carries the entry's content stamp, so a file rewritten while it is on
    /// screen (a download in progress, a log, a screenshot being replaced)
    /// gets a second entry under a new stamp. Keeping by path would retain
    /// the superseded one for the life of the view — unpaintable, since every
    /// lookup uses the full key, and never handed back to the atlas.
    fn prune_thumbnail_images(
        &mut self,
        window: &Range<usize>,
        gpui_window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.thumbnails.images.is_empty() && self.thumbnails.missing.is_empty() {
            return;
        }
        let keep: HashSet<ThumbnailKey> = self
            .flat_rows()
            .get(window.clone())
            .unwrap_or_default()
            .iter()
            .filter(|row| !row.entry.is_dir_like())
            .map(|row| ThumbnailKey::for_entry(&row.entry, THUMBNAIL_PX))
            .collect();
        let stale: Vec<ThumbnailKey> = self
            .thumbnails
            .images
            .keys()
            .filter(|key| !keep.contains(*key))
            .cloned()
            .collect();
        let dropped: Vec<Arc<RenderImage>> = stale
            .into_iter()
            .filter_map(|key| self.thumbnails.images.remove(&key))
            .collect();
        // Unbounded otherwise: one key per non-previewable file ever scrolled
        // past, for the pane's lifetime.
        self.thumbnails.missing.retain(|key| keep.contains(key));
        for image in dropped {
            // Hand the texture back to the atlas rather than leaking the slot
            // for the lifetime of the window.
            cx.drop_image(image, Some(gpui_window));
        }
    }

    /// The image to paint in `entry`'s tile slot, if one is decoded — built
    /// (BGRA, which is what [`RenderImage`] is) on first paint and kept until
    /// the tile scrolls out of the request window.
    pub(crate) fn thumbnail_image(&mut self, entry: &FileEntry) -> Option<Arc<RenderImage>> {
        if entry.is_dir_like() {
            return None;
        }
        let key = ThumbnailKey::for_entry(entry, THUMBNAIL_PX);
        if let Some(image) = self.thumbnails.images.get(&key) {
            return Some(image.clone());
        }
        let thumbnail = self.thumbnails.cache.get(&key)?;
        let image = Arc::new(render_image(&thumbnail)?);
        self.thumbnails.images.insert(key, image.clone());
        Some(image)
    }

    /// Test window into the machine: the request window, the decoded-image
    /// count, and whether a fetch is still running.
    #[cfg(test)]
    pub(crate) fn thumbnail_debug(&self) -> (Option<Range<usize>>, usize, bool) {
        (
            self.thumbnails.window.clone(),
            self.thumbnails.images.len(),
            self.thumbnails.fetching,
        )
    }

    /// Test window into the cache: how many decoded thumbnails it holds.
    #[cfg(test)]
    pub(crate) fn thumbnail_cache_len(&self) -> usize {
        self.thumbnails.cache.len()
    }

    /// Test window into the known-missing set, which must stay bounded by the
    /// request window rather than by everything ever scrolled past.
    #[cfg(test)]
    pub(crate) fn missing_thumbnail_count(&self) -> usize {
        self.thumbnails.missing.len()
    }
}

/// fs-core hands out **non-premultiplied RGBA**; [`RenderImage`] is BGRA
/// (`gpui::RenderImage`: "A cached and processed image, in BGRA format" — its
/// own loaders do the same channel swap). One allocation per thumbnail, once,
/// off the frame that scrolls.
///
/// Shared with [`crate::info_panel`], whose preview is the same conversion at
/// a larger `px`: the info panel keeps its own single-slot image rather than
/// this module's viewport-shaped cache, but the pixel handling must not fork.
pub(crate) fn render_image(thumbnail: &Thumbnail) -> Option<RenderImage> {
    #[cfg(test)]
    tests::note_render_image_call();
    let mut bgra = thumbnail.rgba().to_vec();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(thumbnail.width(), thumbnail.height(), bgra)?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use super::*;

    thread_local! {
        /// How many times [`render_image`] has run **on this test's thread**.
        /// The doc claim it pins is "one allocation per thumbnail, once" — a
        /// texture rebuilt on every idle frame is invisible to every other
        /// observable, because the image *count* is steady when the same tiles
        /// are re-inserted as fast as they are evicted. Thread-local rather
        /// than a global: `render_image` only ever runs on the UI thread,
        /// which under `#[gpui::test]` is the test's own, and a shared static
        /// would race with every other test in the binary.
        static RENDER_IMAGE_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn note_render_image_call() {
        RENDER_IMAGE_CALLS.with(|calls| calls.set(calls.get() + 1));
    }

    fn render_image_calls() -> usize {
        RENDER_IMAGE_CALLS.with(Cell::get)
    }

    fn reset_render_image_calls() {
        RENDER_IMAGE_CALLS.with(|calls| calls.set(0));
    }

    #[test]
    fn the_request_window_covers_the_painted_rows_plus_a_margin() {
        // 4 columns, 40 entries (10 rows). Painting rows 2..4 keeps rows 1..5.
        assert_eq!(request_window(2..4, 4, 40, 1), 4..20);
        // The margin cannot underflow at the top, or overrun the listing at
        // the bottom.
        assert_eq!(request_window(0..2, 4, 40, 1), 0..12);
        assert_eq!(request_window(8..10, 4, 40, 1), 28..40);
        // A ragged last row is included in full, up to `len`.
        assert_eq!(request_window(0..2, 4, 6, 0), 0..6);
        // Degenerate inputs: nothing painted, nothing listed, zero columns,
        // and a stale range from a longer listing.
        assert_eq!(request_window(0..0, 4, 40, 1), 0..0);
        assert_eq!(request_window(0..2, 4, 0, 1), 0..0);
        assert_eq!(request_window(1..2, 0, 40, 1), 0..3);
        assert_eq!(request_window(100..200, 4, 40, 1), 40..40);
        // No margin means exactly the painted band.
        assert_eq!(request_window(2..4, 4, 40, 0), 8..16);
    }

    #[test]
    fn a_thumbnail_becomes_a_bgra_render_image_of_the_same_size() {
        // One opaque red pixel and one transparent green one.
        let rgba: Vec<u8> = vec![255, 0, 0, 255, 0, 255, 0, 0];
        let thumbnail = Thumbnail::new(2, 1, rgba).unwrap();
        let image = render_image(&thumbnail).unwrap();
        assert_eq!(
            (image.size(0).width.0, image.size(0).height.0),
            (2, 1),
            "the slot paints it, so the pixel size must survive the conversion"
        );
        assert_eq!(
            image.as_bytes(0).unwrap(),
            // B G R A — the red and blue channels swapped, alpha untouched.
            &[0, 0, 255, 255, 0, 255, 0, 0],
        );
    }

    // ------------------------------------------------------------------
    // The machine, on a real laid-out grid
    // ------------------------------------------------------------------

    /// What a [`Platform`] was asked for, and what it actually delivered.
    /// Both halves matter: "only visible + margin" is a claim about the
    /// requests made, and "cancel on scroll-away" is a claim about a request
    /// that was *started and never finished*, which the cache alone cannot
    /// show.
    #[derive(Default)]
    struct Calls {
        started: std::sync::Mutex<Vec<std::path::PathBuf>>,
        finished: std::sync::Mutex<Vec<std::path::PathBuf>>,
    }

    impl Calls {
        fn started(&self) -> Vec<String> {
            names(&self.started.lock().unwrap())
        }

        fn finished(&self) -> Vec<String> {
            names(&self.finished.lock().unwrap())
        }
    }

    fn names(paths: &[std::path::PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// A recording [`fs_core::Platform`], optionally *slow*: with a `delay`
    /// it parks on a [`fs_core::Spawner`] timer between being called and
    /// answering, which is what gives a test a fetch it can catch in flight.
    struct RecordingPlatform {
        inner: fs_core::StubPlatform,
        spawner: Arc<dyn fs_core::Spawner>,
        delay: Option<std::time::Duration>,
        calls: Arc<Calls>,
    }

    #[async_trait::async_trait]
    impl fs_core::Platform for RecordingPlatform {
        async fn volumes(&self) -> anyhow::Result<Vec<fs_core::VolumeInfo>> {
            self.inner.volumes().await
        }

        async fn eject(&self, volume_id: &fs_core::VolumeId) -> anyhow::Result<()> {
            self.inner.eject(volume_id).await
        }

        async fn thumbnail(&self, path: &Path, px: u32) -> anyhow::Result<Thumbnail> {
            self.calls.started.lock().unwrap().push(path.to_path_buf());
            if let Some(delay) = self.delay {
                self.spawner.timer(delay).await;
            }
            self.calls.finished.lock().unwrap().push(path.to_path_buf());
            self.inner.thumbnail(path, px).await
        }

        async fn file_attrs(&self, path: &Path) -> anyhow::Result<fs_core::FileAttrs> {
            self.inner.file_attrs(path).await
        }
    }

    /// One slow fetch, long enough that nothing completes until a test
    /// advances the clock on purpose.
    const SLOW: std::time::Duration = std::time::Duration::from_millis(50);

    /// `/photos`: 200 files and one folder, open in the icon grid, in a
    /// window deliberately too small to paint them all.
    fn open_photos(
        cx: &mut gpui::TestAppContext,
        delay: Option<std::time::Duration>,
    ) -> (
        Arc<Calls>,
        gpui::Entity<DirView>,
        &mut gpui::VisualTestContext,
    ) {
        let (calls, _pane, dir_view, cx) = open_photos_pane(cx, delay);
        (calls, dir_view, cx)
    }

    /// As [`open_photos`], keeping the pane too (for the tests that need to
    /// re-list the directory).
    #[allow(clippy::type_complexity)] // one test helper's tuple, not an API
    fn open_photos_pane(
        cx: &mut gpui::TestAppContext,
        delay: Option<std::time::Duration>,
    ) -> (
        Arc<Calls>,
        gpui::Entity<crate::pane::Pane>,
        gpui::Entity<DirView>,
        &mut gpui::VisualTestContext,
    ) {
        let calls: Arc<Calls> = Arc::default();
        let platform_calls = calls.clone();
        cx.update(|cx| {
            let spawner: Arc<dyn fs_core::Spawner> = Arc::new(crate::app_state::GpuiSpawner::new(
                cx.background_executor().clone(),
            ));
            let vfs = fs_core::FakeVfs::new(spawner.clone());
            let mut files = serde_json::Map::new();
            files.insert("album".to_string(), serde_json::json!({}));
            for i in 0..200 {
                files.insert(format!("p{i:03}.png"), serde_json::json!("pixels"));
            }
            vfs.insert_tree("/photos", serde_json::Value::Object(files));
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs,
                spawner.clone(),
                Arc::new(crate::app_state::LoggingOpener),
                Arc::new(RecordingPlatform {
                    inner: fs_core::StubPlatform::new(),
                    spawner,
                    delay,
                    calls: platform_calls,
                }),
            );
        });
        let (pane, cx) = cx
            .add_window_view(|window, cx| crate::pane::Pane::new(crate::Theme::dark(), window, cx));
        pane.update(cx, |pane, cx| {
            pane.navigate_to(Path::new("/photos"), cx);
            pane.set_view_mode(crate::pane::ViewMode::Icons, cx);
        });
        // A small window on purpose: the default test window paints 200 tiles
        // at once, which would make "only visible + margin" vacuously true.
        cx.simulate_resize(gpui::size(gpui::px(520.0), gpui::px(420.0)));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        (calls, pane, dir_view, cx)
    }

    /// Names of the entries in the view's current request window.
    fn window_names(
        dir_view: &gpui::Entity<DirView>,
        cx: &mut gpui::VisualTestContext,
    ) -> Vec<String> {
        dir_view.read_with(cx, |view, _| {
            let window = view.thumbnail_debug().0.unwrap_or(0..0);
            view.flat_rows()
                .get(window)
                .unwrap_or_default()
                .iter()
                .map(|row| row.entry.name.to_string())
                .collect()
        })
    }

    #[gpui::test]
    fn thumbnails_are_requested_only_for_the_visible_window_and_then_painted(
        cx: &mut gpui::TestAppContext,
    ) {
        let (calls, dir_view, cx) = open_photos(cx, None);

        let (window, images, fetching) = dir_view.read_with(cx, |view, _| view.thumbnail_debug());
        let window = window.expect("the grid painted, so a window was requested");
        assert!(
            window.end < 201,
            "a 201-entry folder must not request every thumbnail at once: {window:?}"
        );
        assert!(!fetching, "the whole window finished while parked");
        assert!(images > 0, "the painted tiles built their images");

        let asked = calls.started();
        assert!(!asked.is_empty(), "the visible tiles were requested");
        assert_eq!(
            asked.len(),
            dir_view.read_with(cx, |view, _| view.thumbnail_cache_len()),
            "every request landed in the cache exactly once"
        );
        // Only entries inside the window, and never the folder — Explorer
        // previews file content, and a folder has none of its own.
        let in_window = window_names(&dir_view, cx);
        for name in &asked {
            assert!(
                in_window.contains(name),
                "{name} is outside the request window {window:?}"
            );
            assert_ne!(name, "album", "a folder has no content to preview");
        }
        assert!(
            !asked.iter().any(|name| name == "p199.png"),
            "the last tile of a 200-file folder is nowhere near the viewport"
        );

        // ...and a painted file tile really has an image to draw, while a
        // folder tile keeps its glyph.
        let (file_image, dir_image) = dir_view.update(cx, |view, _| {
            let rows = view.flat_rows().to_vec();
            let file = rows
                .iter()
                .find(|row| !row.entry.is_dir_like())
                .expect("the fixture has files");
            let dir = rows
                .iter()
                .find(|row| row.entry.is_dir_like())
                .expect("the fixture has one folder");
            (
                view.thumbnail_image(&file.entry).is_some(),
                view.thumbnail_image(&dir.entry).is_some(),
            )
        });
        assert!(file_image, "a decoded thumbnail paints in the tile slot");
        assert!(!dir_image, "a folder tile stays on its type glyph");
    }

    #[gpui::test]
    fn scrolling_away_cancels_the_fetch_it_left_in_flight(cx: &mut gpui::TestAppContext) {
        // A slow platform, so the top band's first fetch is still parked on
        // its timer when the viewport moves — the state a fast scroll through
        // a photo folder leaves behind on every line it passes.
        let (calls, dir_view, cx) = open_photos(cx, Some(SLOW));

        let started = calls.started();
        assert_eq!(
            started.len(),
            1,
            "the fetches are sequential, so exactly one is in flight: {started:?}"
        );
        assert!(
            calls.finished().is_empty(),
            "nothing can complete before the clock is advanced"
        );
        let abandoned = started[0].clone();

        // Scroll to the very bottom: a new window, and the in-flight fetch
        // above is now for a tile nobody is looking at.
        dir_view.update(cx, |view, cx| {
            view.apply_scroll_top(100_000.0);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            !window_names(&dir_view, cx).contains(&abandoned),
            "the abandoned tile really did leave the window"
        );

        // Let time pass generously: the surviving window finishes, the
        // abandoned fetch never does, because its task was dropped.
        for _ in 0..64 {
            cx.executor().advance_clock(SLOW * 2);
        }
        cx.run_until_parked();

        assert!(
            !calls.finished().contains(&abandoned),
            "{abandoned} scrolled out of view, so its fetch must have been dropped mid-flight"
        );
        let bottom = window_names(&dir_view, cx);
        let finished = calls.finished();
        assert!(
            bottom
                .iter()
                .filter(|name| name.ends_with(".png"))
                .all(|name| finished.contains(name)),
            "every tile in the surviving window was fetched: window={bottom:?} finished={finished:?}"
        );
        assert!(
            !finished.is_empty() && finished.len() < 200,
            "and the folder between the two windows was never fetched at all: {}",
            finished.len()
        );
    }

    #[test]
    fn a_zero_sized_thumbnail_cannot_produce_an_image() {
        // `Thumbnail::new` rejects mismatched buffers, but a 0-dimension
        // image would still make `from_raw` fail rather than panic in the
        // renderer.
        assert!(Thumbnail::new(0, 0, Vec::new()).is_err());
    }

    #[gpui::test]
    fn idle_repaints_neither_restart_the_fetch_nor_rebuild_a_texture(
        cx: &mut gpui::TestAppContext,
    ) {
        // The window is derived from the scroll offset and the viewport, not
        // from the row range `uniform_list` hands its processor — which gpui
        // calls twice a frame with `0..1` just to measure an item. A window
        // that flipped like that would drop the task in flight on every
        // repaint, and no thumbnail slower than the repaint cadence (the
        // scrollbar's own 900ms fade notify, an arrow-key move inside the
        // visible band, a watcher patch, a job progress tick) would ever
        // finish.
        let (calls, dir_view, cx) = open_photos(cx, Some(SLOW));
        let (window_before, _, _) = dir_view.read_with(cx, |view, _| view.thumbnail_debug());
        let started_before = calls.started();
        assert_eq!(
            started_before.len(),
            1,
            "the fetches are sequential, so exactly one is in flight: {started_before:?}"
        );

        // Ten repaints, no clock advance at all: nothing about what is on
        // screen changed, so nothing about the request may either.
        for _ in 0..10 {
            dir_view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }

        let (window_after, _, fetching) = dir_view.read_with(cx, |view, _| view.thumbnail_debug());
        assert_eq!(
            window_after, window_before,
            "an idle repaint must not move the request window"
        );
        assert_eq!(
            calls.started(),
            started_before,
            "the fetch in flight survived every repaint instead of being restarted"
        );
        assert!(fetching, "...and is still the same one, still working");

        // And it completes, which is the whole point: a restarting fetch
        // never gets there however long you wait.
        cx.executor().advance_clock(SLOW * 2);
        cx.run_until_parked();
        assert_eq!(
            calls.finished().first(),
            started_before.first(),
            "the fetch that survived the repaints is the one that finished"
        );
    }

    #[gpui::test]
    fn a_visible_thumbnail_is_uploaded_once_however_many_frames_pass(
        cx: &mut gpui::TestAppContext,
    ) {
        // Second harm of a window that flipped per frame: `prune_thumbnail_
        // images` computed `keep` from the `0..1` measurement band, evicted
        // every actually-visible texture through `cx.drop_image`, and the
        // visible-range call in the same frame rebuilt them all (alloc +
        // RGBA->BGRA swap + upload) — while the submitted scene still
        // referenced those atlas slots.
        let (_calls, dir_view, cx) = open_photos(cx, None);
        let images = dir_view.read_with(cx, |view, _| view.thumbnail_debug().1);
        assert!(images > 0, "the painted tiles built their images");

        reset_render_image_calls();
        for _ in 0..4 {
            dir_view.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }
        assert_eq!(
            render_image_calls(),
            0,
            "an idle frame paints the textures it already has: {images} images, 4 frames"
        );
        assert_eq!(
            dir_view.read_with(cx, |view, _| view.thumbnail_debug().1),
            images,
            "and the map neither grew nor was churned"
        );
    }

    #[gpui::test]
    fn rewriting_a_visible_file_releases_the_texture_of_the_stamp_it_replaced(
        cx: &mut gpui::TestAppContext,
    ) {
        // `ThumbnailKey` carries the content stamp, so a rewrite mints a new
        // key for the same path. Pruning by path alone kept the superseded
        // entry for the life of the view: unpaintable (every lookup uses the
        // full key) and never handed back to the atlas.
        let (_calls, pane, dir_view, cx) = open_photos_pane(cx, None);
        let before = dir_view.read_with(cx, |view, _| view.thumbnail_debug().1);
        assert!(before > 0, "the painted tiles built their images");

        // Rewrite the first visible file, over and over, as a download in
        // progress or a log being appended to would.
        for i in 0..5 {
            let vfs = cx.update(|_, cx| crate::app_state::FsContext::global(cx).vfs.clone());
            let body = "pixels".repeat(i + 2).into_bytes();
            cx.update(|_, cx| {
                cx.background_executor()
                    .spawn(async move {
                        vfs.atomic_write(Path::new("/photos/p000.png"), body)
                            .await
                            .ok();
                    })
                    .detach();
            });
            cx.run_until_parked();
            // The stamp lives on the snapshot's `FileEntry`, so the listing
            // has to come back round before the view sees a new key at all.
            pane.update(cx, |pane, cx| pane.refresh(cx));
            cx.run_until_parked();
        }

        let after = dir_view.read_with(cx, |view, _| view.thumbnail_debug().1);
        assert!(
            after <= before,
            "each rewrite superseded a stamp rather than leaking one: {before} -> {after}"
        );
    }

    #[gpui::test]
    fn scrolling_away_forgets_the_no_preview_verdicts_it_leaves_behind(
        cx: &mut gpui::TestAppContext,
    ) {
        // `missing` had no pruning and no cap: one key per non-previewable
        // file the pane had ever scrolled past, for the pane's lifetime.
        let (_calls, dir_view, cx) = open_photos(cx, None);
        let top = dir_view.read_with(cx, |view, _| view.thumbnail_debug().0.clone());

        dir_view.update(cx, |view, cx| {
            view.apply_scroll_top(100_000.0);
            cx.notify();
        });
        cx.run_until_parked();

        let (bottom, _, _) = dir_view.read_with(cx, |view, _| view.thumbnail_debug());
        assert_ne!(bottom, top, "the viewport really moved");
        assert!(
            dir_view.read_with(cx, |view, _| view.missing_thumbnail_count())
                <= dir_view.read_with(cx, |view, _| view.thumbnail_debug().0.unwrap_or(0..0).len()),
            "the known-missing set is bounded by the request window, not by history"
        );
    }

    #[test]
    fn the_visible_band_is_stable_and_covers_the_viewport() {
        // 88px rows. Scrolled to the top of a 10-row grid in a 300px viewport:
        // rows 0..4 (the partial fourth row counts, it is on screen).
        assert_eq!(visible_rows(0.0, 300.0, 88.0, 10), 0..4);
        // Scrolled by two whole rows.
        assert_eq!(visible_rows(176.0, 300.0, 88.0, 10), 2..6);
        // A partial first row is still on screen.
        assert_eq!(visible_rows(100.0, 300.0, 88.0, 10), 1..5);
        // Never past the end, never before the start.
        assert_eq!(visible_rows(100_000.0, 300.0, 88.0, 10), 10..10);
        assert_eq!(visible_rows(-50.0, 300.0, 88.0, 10), 0..4);
        // Nothing to show.
        assert_eq!(visible_rows(0.0, 300.0, 88.0, 0), 0..0);
        // Not laid out yet: the first line, so the first arrival is what
        // notifies the real geometry into existence.
        assert_eq!(visible_rows(0.0, 0.0, 88.0, 10), 0..1);
        // Degenerate inputs cannot panic or divide by zero.
        assert_eq!(visible_rows(f32::NAN, 300.0, 88.0, 10), 0..0);
        assert_eq!(visible_rows(0.0, f32::INFINITY, 88.0, 10), 0..0);
        assert_eq!(visible_rows(0.0, 300.0, 0.0, 10), 0..0);
    }
}
