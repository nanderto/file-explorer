//! Finder tags (ARCHITECTURE.md §6 `platform/`, plan §7 M6): the tag model and
//! the **pure codec** for the payload macOS stores in the
//! `com.apple.metadata:_kMDItemUserTags` extended attribute.
//!
//! The xattr's value is a property list containing an array of strings, one per
//! tag, each of the form `"Name\ncolorindex"` (a bare `"Name"` with no newline
//! is a tag with no colour). This module owns *that* string form — the part of
//! the format that decides whether Finder and this app agree about a file — and
//! nothing else: reading and writing the xattr, and turning the array of strings
//! into a binary plist, belong to the macOS [`Platform`](crate::Platform)
//! implementation.
//!
//! Everything here is pure: no filesystem, no clock, no OS calls, identical on
//! macOS, Windows and Linux, and exhaustively unit-tested. [`TagColor`]'s
//! discriminants are **on-disk values**; renumbering them would silently
//! recolour every tagged file on the user's disk, so a test pins them.

use std::sync::Arc;

/// A Finder tag: a name plus one of macOS's fixed colour slots.
///
/// The name is the identity — Finder's own UI treats two tags with the same
/// name as the same tag, and so do [`encode_tag_strings`] and
/// [`decode_tag_strings`], which de-duplicate by name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tag {
    pub name: Arc<str>,
    pub color: TagColor,
}

impl Tag {
    /// A tag with `name` in colour slot `color`.
    pub fn new(name: impl AsRef<str>, color: TagColor) -> Self {
        Self {
            name: name.as_ref().into(),
            color,
        }
    }

    /// A tag with no colour (`TagColor::None`) — what Finder creates when the
    /// user types a new tag name without picking a dot.
    pub fn uncolored(name: impl AsRef<str>) -> Self {
        Self::new(name, TagColor::None)
    }
}

/// macOS's fixed tag palette.
///
/// The integer values are the on-disk colour indices Finder writes into the
/// `_kMDItemUserTags` strings, so they are **part of the file format — do not
/// renumber them**. `None` (0) is a tag with no dot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TagColor {
    #[default]
    None = 0,
    Gray = 1,
    Green = 2,
    Purple = 3,
    Blue = 4,
    Yellow = 5,
    Red = 6,
    Orange = 7,
}

impl TagColor {
    /// The seven coloured slots in the order Finder lists them in its tag
    /// menus and sidebar — **not** index order, which is a historical
    /// artefact of the old Mac OS label numbering.
    pub const PALETTE: [TagColor; 7] = [
        TagColor::Red,
        TagColor::Orange,
        TagColor::Yellow,
        TagColor::Green,
        TagColor::Blue,
        TagColor::Purple,
        TagColor::Gray,
    ];

    /// The on-disk colour index.
    pub fn index(self) -> u8 {
        self as u8
    }

    /// The colour for an on-disk index, or `None` if the index is not one of
    /// the eight macOS defines. Callers decode defensively: a file tagged by a
    /// future macOS (or by a buggy third-party tool) must not lose its tag
    /// *name* just because the colour byte is unfamiliar.
    pub fn from_index(i: u8) -> Option<Self> {
        Some(match i {
            0 => TagColor::None,
            1 => TagColor::Gray,
            2 => TagColor::Green,
            3 => TagColor::Purple,
            4 => TagColor::Blue,
            5 => TagColor::Yellow,
            6 => TagColor::Red,
            7 => TagColor::Orange,
            _ => return None,
        })
    }

    /// The dot colour as `0xRRGGBBAA`.
    ///
    /// **These are the only colours in the product that do not come from the
    /// `Theme`** (plan §6 says so explicitly): they are macOS's own tag
    /// palette, and a tag dot that is not the colour Finder paints is simply
    /// the wrong dot. `TagColor::None` is fully transparent — a tag with no
    /// colour draws no dot at all, so the caller should branch on
    /// [`TagColor::None`] rather than paint transparent pixels.
    pub fn rgba(self) -> u32 {
        match self {
            TagColor::None => 0x0000_0000,
            TagColor::Gray => 0xA2A2_A2FF,
            TagColor::Green => 0x62BA_46FF,
            TagColor::Purple => 0xC86E_DFFF,
            TagColor::Blue => 0x0A7C_FFFF,
            TagColor::Yellow => 0xFFC6_00FF,
            TagColor::Red => 0xFF52_57FF,
            TagColor::Orange => 0xF782_1BFF,
        }
    }

    /// The English name macOS ships for this colour's default tag (`Red`,
    /// `Orange`, …), or `None` for [`TagColor::None`], which has no default
    /// tag. Used to give the standard palette tags their names and to guess a
    /// colour for a tag name read out of Finder's preferences.
    pub fn standard_name(self) -> Option<&'static str> {
        Some(match self {
            TagColor::None => return None,
            TagColor::Gray => "Gray",
            TagColor::Green => "Green",
            TagColor::Purple => "Purple",
            TagColor::Blue => "Blue",
            TagColor::Yellow => "Yellow",
            TagColor::Red => "Red",
            TagColor::Orange => "Orange",
        })
    }

    /// The colour whose [`standard_name`](Self::standard_name) matches `name`,
    /// ignoring ASCII case. Finder's preferences store favourite tags by name
    /// only, so the colour of `"Red"` has to be recovered this way.
    pub fn from_standard_name(name: &str) -> Option<Self> {
        TagColor::PALETTE.into_iter().find(|color| {
            color
                .standard_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
    }
}

/// The seven standard palette tags (`Red`, `Orange`, … `Gray`) in Finder's
/// order — the baseline set every [`Platform::known_tags`](crate::Platform::known_tags)
/// implementation offers in the sidebar.
pub fn standard_tags() -> Vec<Tag> {
    TagColor::PALETTE
        .into_iter()
        .map(|color| {
            Tag::new(
                color.standard_name().expect("palette colours are named"),
                color,
            )
        })
        .collect()
}

/// Encode tags into the strings that go in the `_kMDItemUserTags` plist array.
///
/// Each tag becomes `"Name\nindex"` — the colour index is written **always**,
/// including `0`. Finder accepts both forms, and always writing the index keeps
/// the encoding total and reversible: a tag whose *name* contains a newline
/// (Finder permits it) still decodes back to the same name, because
/// [`decode_tag_strings`] splits at the **last** newline.
///
/// Tags with a blank name are dropped (there is no such tag in Finder's UI, and
/// an empty string in the array would show as a nameless row), and duplicate
/// names are collapsed keeping the **first** occurrence, so the caller's order
/// is preserved. Order is otherwise untouched: Finder displays tags in array
/// order, so it is the caller's business, not the codec's.
pub fn encode_tag_strings(tags: &[Tag]) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::with_capacity(tags.len());
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        let name = tag.name.as_ref();
        if name.trim().is_empty() || seen.contains(&name) {
            continue;
        }
        seen.push(name);
        out.push(format!("{name}\n{}", tag.color.index()));
    }
    out
}

/// Decode the strings from a `_kMDItemUserTags` plist array.
///
/// Tolerant of everything seen in the wild, because the alternative is losing a
/// user's tags:
///
/// * `"Name\n6"` → that name, that colour.
/// * `"Name"` (no newline) → that name, [`TagColor::None`].
/// * `"Name\n99"` (index outside the palette) → that name, [`TagColor::None`];
///   the unknown colour is dropped, never the name.
/// * `"Name\nnot-a-number"` → the **whole string** is the name (colour `None`),
///   which is also how a tag name that genuinely contains a newline survives.
/// * blank strings are skipped, and duplicate names collapse to their first
///   occurrence — matching [`encode_tag_strings`], so decode∘encode is the
///   identity on any already-normalized set.
///
/// Array order is preserved: it is the order Finder shows.
pub fn decode_tag_strings(raw: &[String]) -> Vec<Tag> {
    let mut out: Vec<Tag> = Vec::with_capacity(raw.len());
    for entry in raw {
        let (name, color) = match entry.rsplit_once('\n') {
            // A trailing line that is a plain integer is a colour index —
            // in range or not. Anything else is part of the name.
            Some((name, index)) => match index.parse::<u8>() {
                Ok(index) => (name, TagColor::from_index(index).unwrap_or(TagColor::None)),
                Err(_) => (entry.as_str(), TagColor::None),
            },
            None => (entry.as_str(), TagColor::None),
        };
        if name.trim().is_empty() || out.iter().any(|tag| tag.name.as_ref() == name) {
            continue;
        }
        out.push(Tag::new(name, color));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tags: &[Tag]) -> Vec<&str> {
        tags.iter().map(|t| t.name.as_ref()).collect()
    }

    /// This is a **file format**. If this test fails because someone reordered
    /// the enum, every tagged file on every user's disk just changed colour.
    #[test]
    fn color_indices_are_the_on_disk_values_and_never_change() {
        assert_eq!(
            [
                TagColor::None.index(),
                TagColor::Gray.index(),
                TagColor::Green.index(),
                TagColor::Purple.index(),
                TagColor::Blue.index(),
                TagColor::Yellow.index(),
                TagColor::Red.index(),
                TagColor::Orange.index(),
            ],
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        for index in 0u8..=7 {
            assert_eq!(
                TagColor::from_index(index).map(TagColor::index),
                Some(index),
                "index {index} must round-trip"
            );
        }
        assert_eq!(TagColor::from_index(8), None);
        assert_eq!(TagColor::from_index(u8::MAX), None);
        assert_eq!(TagColor::default(), TagColor::None);
    }

    #[test]
    fn every_palette_color_has_a_distinct_opaque_rgba_and_none_is_transparent() {
        assert_eq!(TagColor::None.rgba(), 0);
        let mut seen = Vec::new();
        for color in TagColor::PALETTE {
            let rgba = color.rgba();
            assert_eq!(rgba & 0xFF, 0xFF, "{color:?} must be opaque");
            assert!(!seen.contains(&rgba), "{color:?} duplicates another colour");
            seen.push(rgba);
        }
        // Pinned: these are macOS's colours, not ours, and the dots must match
        // Finder's. Changing one is a deliberate act.
        assert_eq!(TagColor::Red.rgba(), 0xFF52_57FF);
        assert_eq!(TagColor::Blue.rgba(), 0x0A7C_FFFF);
        assert_eq!(TagColor::Gray.rgba(), 0xA2A2_A2FF);
    }

    #[test]
    fn palette_is_finder_order_and_names_round_trip() {
        assert_eq!(
            TagColor::PALETTE.map(|c| c.standard_name().unwrap()),
            ["Red", "Orange", "Yellow", "Green", "Blue", "Purple", "Gray"]
        );
        assert_eq!(TagColor::None.standard_name(), None);
        for color in TagColor::PALETTE {
            let name = color.standard_name().unwrap();
            assert_eq!(TagColor::from_standard_name(name), Some(color));
            assert_eq!(
                TagColor::from_standard_name(&name.to_lowercase()),
                Some(color),
                "name matching ignores case"
            );
        }
        assert_eq!(TagColor::from_standard_name("Work"), None);
        assert_eq!(standard_tags().len(), 7);
        assert_eq!(names(&standard_tags())[0], "Red");
    }

    #[test]
    fn encode_writes_name_newline_index_always() {
        let encoded = encode_tag_strings(&[
            Tag::new("Red", TagColor::Red),
            Tag::new("Work", TagColor::None),
            Tag::new("Später", TagColor::Orange),
        ]);
        assert_eq!(encoded, ["Red\n6", "Work\n0", "Später\n7"]);
    }

    #[test]
    fn encode_drops_blank_names_and_collapses_duplicates_keeping_the_first() {
        let encoded = encode_tag_strings(&[
            Tag::new("Work", TagColor::Blue),
            Tag::new("", TagColor::Red),
            Tag::new("   ", TagColor::Red),
            Tag::new("Work", TagColor::Red),
        ]);
        assert_eq!(encoded, ["Work\n4"]);
    }

    #[test]
    fn encode_of_nothing_is_an_empty_array() {
        assert!(encode_tag_strings(&[]).is_empty());
    }

    #[test]
    fn decode_reads_the_finder_forms() {
        let decoded = decode_tag_strings(&[
            "Red\n6".to_string(),
            "Bare".to_string(),
            "Zero\n0".to_string(),
        ]);
        assert_eq!(
            decoded,
            [
                Tag::new("Red", TagColor::Red),
                Tag::new("Bare", TagColor::None),
                Tag::new("Zero", TagColor::None),
            ]
        );
    }

    #[test]
    fn decode_keeps_the_name_when_the_color_index_is_out_of_range() {
        let decoded = decode_tag_strings(&["Future\n42".to_string(), "Huge\n255".to_string()]);
        assert_eq!(names(&decoded), ["Future", "Huge"]);
        assert!(decoded.iter().all(|t| t.color == TagColor::None));
    }

    /// A name containing a newline is legal in Finder. The last-newline split
    /// makes it survive both directions, as long as its final line is not a
    /// bare integer — which is the documented, tested limit of the encoding.
    #[test]
    fn decode_treats_a_non_numeric_trailing_line_as_part_of_the_name() {
        let decoded = decode_tag_strings(&["two\nlines".to_string()]);
        assert_eq!(decoded, [Tag::new("two\nlines", TagColor::None)]);

        // …and the multi-line name round-trips through the encoder.
        let encoded = encode_tag_strings(&decoded);
        assert_eq!(encoded, ["two\nlines\n0"]);
        assert_eq!(decode_tag_strings(&encoded), decoded);

        // The stated limit: a name whose own last line is an integer loses
        // that line on decode. Recorded rather than papered over.
        let ambiguous = decode_tag_strings(&["odd\n5\n6".to_string()]);
        assert_eq!(ambiguous, [Tag::new("odd\n5", TagColor::Red)]);
    }

    #[test]
    fn decode_skips_blanks_and_duplicates_and_preserves_order() {
        let decoded = decode_tag_strings(&[
            "Beta\n2".to_string(),
            String::new(),
            "\n6".to_string(),
            " \n6".to_string(),
            "Alpha\n6".to_string(),
            "Beta\n4".to_string(),
        ]);
        assert_eq!(names(&decoded), ["Beta", "Alpha"]);
        assert_eq!(decoded[0].color, TagColor::Green, "first Beta wins");
    }

    #[test]
    fn decode_of_an_empty_array_is_no_tags() {
        assert!(decode_tag_strings(&[]).is_empty());
    }

    #[test]
    fn decode_after_encode_is_the_identity_including_non_ascii() {
        let tags = vec![
            Tag::new("Rot", TagColor::Red),
            Tag::new("日本語", TagColor::Purple),
            Tag::new("emoji 🏷", TagColor::Yellow),
            Tag::uncolored("no colour"),
        ];
        assert_eq!(decode_tag_strings(&encode_tag_strings(&tags)), tags);
    }

    #[test]
    fn encode_after_decode_is_the_identity_on_normalized_input() {
        let raw: Vec<String> = ["Red\n6", "Work\n0", "日本語\n3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(encode_tag_strings(&decode_tag_strings(&raw)), raw);
    }
}
