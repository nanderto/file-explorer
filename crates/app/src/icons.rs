//! The bundled icon set (plan §7 M7d).
//!
//! Through M7c every "icon" in this app was a Unicode glyph typed inline in
//! the source — `▸ ⏏ ⌕ ✕ ▣ ▢ ⓘ ⚙ ☰ ▦`. Those are *text*: they render in
//! whatever face the font stack resolves them to, at wildly different optical
//! weights and baselines from each other, and several (`ⓘ`, `▣`, `⏏`) have no
//! glyph at all in some faces and fall back to a box. The sidebar could not
//! read as a peer of Finder's while its rows were lettered.
//!
//! **The mechanism.** Each icon is a vendored [Lucide](https://lucide.dev)
//! SVG in `crates/app/assets/icons/`, compiled in with `include_bytes!` and
//! painted through [`gpui::svg`]'s `data()` path. That deliberately avoids a
//! [`gpui::AssetSource`]: an asset source is installed on the `Application`,
//! which the `#[gpui::test]` contexts and the visual runner never build, so
//! path-addressed icons would be present in the shipped app and *absent* in
//! every test — the failure mode where a baseline is regenerated from a frame
//! whose icons all silently painted nothing. Bytes in the binary are the same
//! bytes everywhere. It also keeps `Cargo.toml` untouched (no `rust-embed`),
//! which matters here only because a manifest change forces a full workspace
//! rebuild (CLAUDE.md).
//!
//! **Colour.** gpui rasterizes an SVG to an *alpha mask* and tints it with the
//! element's own `text_color` — it does not inherit the ambient text style the
//! way a string child does. So every constructor here takes the colour
//! explicitly and there is no way to spell an untinted icon (an icon with no
//! `text_color` paints nothing at all, silently). No hard-coded colours: the
//! caller passes a theme token (`crates/app` convention).
//!
//! **Licence.** Lucide is ISC; `crates/app/assets/icons/LICENSE` is the
//! upstream text and `VENDORED.md` records the version. The files are
//! byte-for-byte as published, so re-vendoring is a re-download rather than a
//! merge.

use gpui::{Div, Hsla, ParentElement as _, Pixels, Styled as _, Svg, div, px, svg};

/// Every icon the app can draw. One variant per vendored file; adding a
/// variant without adding the file is a compile error, and an unused variant
/// is a dead-code warning — so this enum and the folder cannot drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// A collapsed disclosure (sidebar sections, sidebar tree, details rows).
    ChevronRight,
    /// An expanded disclosure, and a descending sort.
    ChevronDown,
    /// An ascending sort, on a details-view column header.
    ChevronUp,
    /// A checked state: a context-menu tick, a settings checkbox.
    Check,
    /// A folder, where no thumbnail is available.
    Folder,
    /// A file of no particular kind, where no thumbnail is available.
    File,
    /// Unmount an ejectable volume.
    Eject,
    /// Dismiss: unpin a favorite, clear the search field, cancel a job.
    Close,
    /// Pin the active folder to Favorites.
    Plus,
    /// The search field's magnifier.
    Search,
    /// The details (list) view mode.
    ViewList,
    /// The icon (grid) view mode.
    ViewGrid,
    /// The dual-pane toggle.
    SplitPane,
    /// The info-panel toggle.
    Info,
    /// The settings pane toggle.
    Settings,
}

impl Icon {
    /// The vendored SVG source, compiled into the binary.
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Icon::ChevronRight => include_bytes!("../assets/icons/chevron-right.svg"),
            Icon::ChevronDown => include_bytes!("../assets/icons/chevron-down.svg"),
            Icon::ChevronUp => include_bytes!("../assets/icons/chevron-up.svg"),
            Icon::Check => include_bytes!("../assets/icons/check.svg"),
            Icon::Folder => include_bytes!("../assets/icons/folder.svg"),
            Icon::File => include_bytes!("../assets/icons/file.svg"),
            Icon::Eject => include_bytes!("../assets/icons/eject.svg"),
            Icon::Close => include_bytes!("../assets/icons/x.svg"),
            Icon::Plus => include_bytes!("../assets/icons/plus.svg"),
            Icon::Search => include_bytes!("../assets/icons/search.svg"),
            Icon::ViewList => include_bytes!("../assets/icons/list.svg"),
            Icon::ViewGrid => include_bytes!("../assets/icons/grid-2x2.svg"),
            Icon::SplitPane => include_bytes!("../assets/icons/columns-2.svg"),
            Icon::Info => include_bytes!("../assets/icons/info.svg"),
            Icon::Settings => include_bytes!("../assets/icons/settings.svg"),
        }
    }

    /// Every variant, for the exhaustiveness tests below. Kept beside
    /// [`Icon::bytes`] so a new variant is one edit away from being covered.
    pub const ALL: &'static [Icon] = &[
        Icon::ChevronRight,
        Icon::ChevronDown,
        Icon::ChevronUp,
        Icon::Check,
        Icon::Folder,
        Icon::File,
        Icon::Eject,
        Icon::Close,
        Icon::Plus,
        Icon::Search,
        Icon::ViewList,
        Icon::ViewGrid,
        Icon::SplitPane,
        Icon::Info,
        Icon::Settings,
    ];
}

/// The default control-icon size: a 14px box, which is the cap height of the
/// 13px UI text plus a hair, so an icon beside a label reads as the same
/// weight rather than as a larger sibling.
pub const ICON_PX: f32 = 14.0;

/// A 12px icon, for the places that were drawing an 11–12px glyph: the
/// titlebar's toggle buttons and a volume row's eject.
pub const SMALL_ICON_PX: f32 = 12.0;

/// The disclosure icon for an expansion state, in one place so the sidebar's
/// sections, the sidebar's tree and the details list cannot drift apart (the
/// job `theme::DISCLOSURE_*` held through M7c).
pub fn disclosure(expanded: bool) -> Icon {
    if expanded {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    }
}

/// The type icon for an entry with no thumbnail — the icon grid's tile and
/// the info panel's preview slot. A folder and a not-a-folder are the only two
/// answers the app has today; per-kind icons are the `file_colors` design step
/// M7c deferred, and this is the seam they will land behind.
pub fn entry_icon(is_dir_like: bool) -> Icon {
    if is_dir_like {
        Icon::Folder
    } else {
        Icon::File
    }
}

/// The checkbox outline drawn by [`check_box`].
pub const CHECK_BOX_PX: f32 = 13.0;

/// The tick inside a 13px checkbox (info panel permissions, settings
/// toggles). Smaller than [`SMALL_ICON_PX`] because it has to sit *inside* a
/// box with a 1px border rather than beside a label.
pub const CHECK_PX: f32 = 10.0;

/// An icon at [`ICON_PX`], tinted with a theme colour.
pub fn icon(which: Icon, color: Hsla) -> Svg {
    sized_icon(which, px(ICON_PX), color)
}

/// An icon at an explicit size, tinted with a theme colour.
///
/// `flex_none` because every call site puts an icon in a flex row beside a
/// `flex_1` truncating label: without it the icon is the element that gives up
/// width when the sidebar narrows, and it squashes to nothing before the text
/// it labels ever truncates.
pub fn sized_icon(which: Icon, size: Pixels, color: Hsla) -> Svg {
    svg()
        .flex_none()
        .size(size)
        .data(which.bytes())
        .text_color(color)
}

/// The app's checkbox, in one place: a rounded outline that fills with `fill`
/// and gains a [`Icon::Check`] when set.
///
/// Three call sites drew this independently before M7d — the settings pane's
/// behavior toggles, the info panel's read-only attribute rows, and the search
/// field's "Subfolders" — and the third of them was not a box at all but the
/// characters `☐`/`☑`, which is why "Subfolders" sat visibly lighter and
/// smaller than the checkbox two panels over.
///
/// Colours are passed in rather than read from the theme here so a caller can
/// hand over *dimmed* ones: the info panel draws an unknown-yet attribute at
/// `DISABLED_ALPHA` and must not paint a live-looking control over a value it
/// does not have.
pub fn check_box(checked: bool, border: Hsla, fill: Hsla, tick: Hsla) -> Div {
    let box_ = div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(CHECK_BOX_PX))
        .rounded(px(3.0))
        .border_1()
        .border_color(border);
    if checked {
        box_.bg(fill)
            .child(sized_icon(Icon::Check, px(CHECK_PX), tick))
    } else {
        box_
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_carries_a_non_empty_svg() {
        for &which in Icon::ALL {
            let bytes = which.bytes();
            assert!(
                !bytes.is_empty(),
                "{which:?} vendored an empty file — the fetch failed silently"
            );
            let text =
                std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("{which:?} is not UTF-8 XML"));
            assert!(
                text.contains("<svg"),
                "{which:?} is not an SVG document: {text:.80}"
            );
        }
    }

    /// Every icon shares one 24×24 user-space box, so `size()` means the same
    /// thing for all of them and no icon is optically larger than its row
    /// neighbours at the same `px`. A re-vendored icon on a different grid
    /// would break that silently — a slightly-too-big magnifier is exactly the
    /// kind of thing a reviewer reads past.
    #[test]
    fn every_icon_shares_the_24px_grid() {
        for &which in Icon::ALL {
            let text = std::str::from_utf8(which.bytes()).unwrap();
            assert!(
                text.contains(r#"viewBox="0 0 24 24""#),
                "{which:?} is not on the 24×24 grid the sizes assume"
            );
        }
    }

    /// The tint contract: gpui paints an SVG as an alpha mask coloured by the
    /// element's `text_color`, which only works because the vendored art is
    /// stroked in `currentColor` rather than a baked-in literal. An icon that
    /// arrived with `stroke="#000"` would still paint — the mask is the same —
    /// but the next person to re-vendor would have no way to know the
    /// invariant existed.
    #[test]
    fn every_icon_is_stroked_in_current_color() {
        for &which in Icon::ALL {
            let text = std::str::from_utf8(which.bytes()).unwrap();
            assert!(
                text.contains("currentColor"),
                "{which:?} bakes in its own colour instead of inheriting"
            );
        }
    }

    /// Two variants pointing at the same file is a copy-paste slip that paints
    /// a plausible-but-wrong icon — a `Close` that draws a `Plus` looks like a
    /// control, just the wrong one — so nothing downstream would fail.
    #[test]
    fn no_two_variants_share_a_file() {
        for (i, &a) in Icon::ALL.iter().enumerate() {
            for &b in &Icon::ALL[i + 1..] {
                assert_ne!(
                    a.bytes(),
                    b.bytes(),
                    "{a:?} and {b:?} vendored the same artwork"
                );
            }
        }
    }
}
