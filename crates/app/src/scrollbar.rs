//! The auto-hide scrollbar (ARCHITECTURE.md §8 widget list, "Auto-hide
//! scrollbar | M4 polish").
//!
//! A thin overlay, not a layout node: it is an absolutely-positioned child of
//! the marquee's list surface, so adding it neither reserves width nor shifts
//! a single row or tile — which matters because every mouse hit test in the
//! details list and the icon grid is arithmetic over the painted band.
//!
//! **It only exists while the content is actually scrolling.** The trigger is
//! a change in the list's scroll offset observed between two frames, and the
//! bar fades out [`FADE_DELAY`] later on a [`fs_core::Spawner::timer`] —
//! fake time under `#[gpui::test]`, and, importantly, **wall-clock-free in
//! the visual runner**: a captured scenario that never scrolls never shows a
//! bar, so no baseline depends on when the screenshot was taken (CLAUDE.md's
//! "keep renders deterministic"). A scenario that *did* want the bar pinned
//! would scroll, capture, and never advance the clock — the fade is a timer,
//! not an animation, so there are exactly two states.
//!
//! Like the marquee's autoscroll, the fade lives in a **single `Task` slot**:
//! every scroll replaces the pending task, which cancels it, so the bar stays
//! up for `FADE_DELAY` after the *last* scroll rather than the first.
//!
//! Not implemented (recorded in AS_BUILT "Known gaps"): the bar is an
//! indicator, not a control — it cannot be dragged, and there is no
//! horizontal bar (neither view scrolls horizontally).

use std::time::Duration;

use gpui::{Context, Div, div, prelude::*, px};

use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::marquee;

/// How long the bar stays visible after the last scroll.
pub const FADE_DELAY: Duration = Duration::from_millis(900);

/// Track width and the thumb's inset from the list's right edge.
const THUMB_WIDTH: f32 = 5.0;
const THUMB_INSET: f32 = 2.0;
/// The thumb never shrinks below this, however long the listing is — a
/// one-pixel thumb in a 100k-entry folder is not a scroll indicator.
const MIN_THUMB_HEIGHT: f32 = 24.0;
/// Alpha applied to the theme's muted color (the app crate names no colors).
const THUMB_ALPHA: f32 = 0.55;

/// Where the thumb sits in the track, in viewport-relative pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Thumb {
    pub top: f32,
    pub height: f32,
}

/// The thumb for a viewport of `viewport` pixels over `content` pixels of
/// rows/tiles, scrolled to `offset` (gpui's convention: `0` at the top, more
/// **negative** further down).
///
/// `None` when there is nothing to scroll — the whole point of an auto-hide
/// bar is that a folder that fits shows no chrome at all.
pub(crate) fn thumb(viewport: f32, content: f32, offset: f32) -> Option<Thumb> {
    if !viewport.is_finite() || !content.is_finite() || !offset.is_finite() {
        return None;
    }
    if viewport <= 0.0 || content <= viewport {
        return None;
    }
    let height = (viewport * (viewport / content)).clamp(MIN_THUMB_HEIGHT.min(viewport), viewport);
    let max_scroll = content - viewport;
    let progress = (-offset / max_scroll).clamp(0.0, 1.0);
    Some(Thumb {
        top: progress * (viewport - height),
        height,
    })
}

/// The bar's own state: the last offset seen, whether it is showing, and the
/// single-slot fade timer.
#[derive(Default)]
pub(crate) struct ScrollbarState {
    /// `None` until the first frame has been observed — so simply *opening* a
    /// folder does not flash a bar.
    last_offset: Option<f32>,
    visible: bool,
    _fade: Option<gpui::Task<()>>,
}

impl DirView {
    /// Called once per render: show the bar if the list has scrolled since the
    /// last frame, and (re)start the fade.
    ///
    /// Deliberately does **not** `notify` — it runs inside `render`, and the
    /// frame it is deciding about is the one being built. The fade timer's
    /// expiry is the only notify in the machine, which is also why it cannot
    /// loop: the offset is unchanged on the frame it causes.
    pub(crate) fn note_scroll_for_scrollbar(&mut self, cx: &mut Context<Self>) {
        let offset = marquee::scroll_y(self);
        let previous = self.scrollbar.last_offset.replace(offset);
        match previous {
            Some(before) if before != offset => {}
            // First observed frame, or no movement: leave the bar as it is.
            _ => return,
        }
        self.scrollbar.visible = true;
        let spawner = FsContext::global(cx).spawner.clone();
        // Replacing the task cancels the previous one, so the delay is
        // measured from the *last* scroll.
        self.scrollbar._fade = Some(cx.spawn(async move |this, cx| {
            spawner.timer(FADE_DELAY).await;
            this.update(cx, |this, cx| {
                this.scrollbar.visible = false;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Whether the bar is currently showing (tests; the render path reads the
    /// field directly).
    #[cfg(test)]
    pub(crate) fn scrollbar_visible(&self) -> bool {
        self.scrollbar.visible
    }
}

/// The overlay, or `None` when the bar is hidden or there is nothing to
/// scroll. Rendered by [`crate::marquee::list_surface`], which is the
/// positioning context (the same one the marquee band uses).
pub(crate) fn render(view: &DirView, cx: &gpui::App) -> Option<Div> {
    if !view.scrollbar.visible {
        return None;
    }
    let viewport = f32::from(marquee::list_viewport(view).size.height);
    let thumb = thumb(viewport, view.content_height(cx), marquee::scroll_y(view))?;
    Some(
        div()
            .absolute()
            .top(px(thumb.top))
            .right(px(THUMB_INSET))
            .w(px(THUMB_WIDTH))
            .h(px(thumb.height))
            .rounded(px(THUMB_WIDTH / 2.0))
            .bg(view.theme().muted.opacity(THUMB_ALPHA)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn the_bar_appears_on_a_scroll_and_fades_on_the_spawner_clock(cx: &mut gpui::TestAppContext) {
        use std::path::Path;
        use std::sync::Arc;

        cx.update(|cx| {
            let spawner: Arc<dyn fs_core::Spawner> = Arc::new(crate::app_state::GpuiSpawner::new(
                cx.background_executor().clone(),
            ));
            let vfs = fs_core::FakeVfs::new(spawner.clone());
            let mut files = serde_json::Map::new();
            for i in 0..300 {
                files.insert(format!("f{i:03}.txt"), serde_json::json!("x"));
            }
            vfs.insert_tree("/tall", serde_json::Value::Object(files));
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs,
                spawner,
                Arc::new(crate::app_state::LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
        });
        let (pane, cx) = cx
            .add_window_view(|window, cx| crate::pane::Pane::new(crate::Theme::dark(), window, cx));
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/tall"), cx));
        cx.run_until_parked();
        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());

        // Opening a folder is not a scroll: no chrome at rest, which is also
        // what keeps the visual baselines free of a wall-clock-dependent bar.
        assert!(
            !dir_view.read_with(cx, |view, _| view.scrollbar_visible()),
            "a freshly opened folder shows no scrollbar"
        );
        // ...and there is genuinely something to scroll, so the absence above
        // is the auto-hide rule rather than a short listing.
        assert!(
            cx.update(|_, cx| dir_view.read(cx).content_height(cx))
                > f32::from(dir_view.read_with(cx, |view, _| view.list_viewport().size.height)),
            "300 rows must overflow the test viewport"
        );

        dir_view.update(cx, |view, cx| {
            view.apply_scroll_top(600.0);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            dir_view.read_with(cx, |view, _| view.scrollbar_visible()),
            "the bar shows while the list is being scrolled"
        );

        // Almost-but-not-quite the fade delay: still up (so the assertion
        // below is about the timer, not about any repaint).
        cx.executor().advance_clock(FADE_DELAY / 2);
        cx.run_until_parked();
        assert!(dir_view.read_with(cx, |view, _| view.scrollbar_visible()));

        // A second scroll restarts the timer rather than letting the first
        // one expire — the single-slot pattern.
        dir_view.update(cx, |view, cx| {
            view.apply_scroll_top(900.0);
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(FADE_DELAY / 2 + Duration::from_millis(10));
        cx.run_until_parked();
        assert!(
            dir_view.read_with(cx, |view, _| view.scrollbar_visible()),
            "the fade is measured from the last scroll, not the first"
        );

        cx.executor().advance_clock(FADE_DELAY);
        cx.run_until_parked();
        assert!(
            !dir_view.read_with(cx, |view, _| view.scrollbar_visible()),
            "and it fades once the list has been still for the whole delay"
        );
    }

    #[test]
    fn nothing_to_scroll_means_no_thumb() {
        // Content shorter than, or exactly, the viewport.
        assert_eq!(thumb(400.0, 100.0, 0.0), None);
        assert_eq!(thumb(400.0, 400.0, 0.0), None);
        // A viewport that has not been laid out yet.
        assert_eq!(thumb(0.0, 4000.0, 0.0), None);
        // Degenerate floats must not produce a NaN-positioned element.
        assert_eq!(thumb(f32::NAN, 4000.0, 0.0), None);
        assert_eq!(thumb(400.0, f32::INFINITY, 0.0), None);
        assert_eq!(thumb(400.0, 4000.0, f32::NAN), None);
    }

    #[test]
    fn the_thumb_is_proportional_and_tracks_the_offset() {
        // 400 of 4000 pixels visible: a tenth of the track.
        let top = thumb(400.0, 4000.0, 0.0).unwrap();
        assert_eq!(top.height, 40.0);
        assert_eq!(top.top, 0.0, "at the top of the track when unscrolled");
        // Scrolled to the very bottom: flush with the bottom of the track.
        let bottom = thumb(400.0, 4000.0, -3600.0).unwrap();
        assert_eq!(bottom.height, 40.0);
        assert_eq!(bottom.top + bottom.height, 400.0);
        // Halfway.
        let middle = thumb(400.0, 4000.0, -1800.0).unwrap();
        assert_eq!(middle.top, (400.0 - 40.0) / 2.0);
    }

    #[test]
    fn the_thumb_has_a_floor_and_stays_inside_the_track() {
        // A 100k-row listing would give a sub-pixel thumb.
        let tall = thumb(400.0, 400_000.0, -399_600.0).unwrap();
        assert_eq!(tall.height, MIN_THUMB_HEIGHT);
        assert_eq!(tall.top + tall.height, 400.0, "still flush at the bottom");
        // An over-scrolled offset (rubber-banding, or a stale offset after the
        // listing shrank) clamps rather than escaping the track.
        let over = thumb(400.0, 4000.0, -99_999.0).unwrap();
        assert_eq!(over.top + over.height, 400.0);
        let under = thumb(400.0, 4000.0, 500.0).unwrap();
        assert_eq!(under.top, 0.0);
        // A viewport shorter than the minimum thumb: the thumb fills it.
        let tiny = thumb(10.0, 1000.0, 0.0).unwrap();
        assert_eq!(tiny.height, 10.0);
        assert_eq!(tiny.top, 0.0);
    }
}
