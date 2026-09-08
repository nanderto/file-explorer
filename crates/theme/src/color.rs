//! The theme file's color syntax.
//!
//! Two spellings are accepted, and both round-trip:
//!
//! * `hsl(240, 4%, 12%)` / `hsla(240, 4%, 12%, 0.35)` — the native form.
//!   `Hsla` is what gpui paints with, so a theme written this way reaches the
//!   screen with no conversion and no rounding at all.
//! * `#1d1d21` / `#1d1d21a0` — the hex form every Zed/gpui-component theme is
//!   written in, so an existing theme can be adapted by copying its colors
//!   across (plan §6). Hex is converted to HSL on load, which is lossy in the
//!   last bit or two; that is inherent to the format, not to this parser.
//!
//! Both are case-insensitive, and whitespace around the components is free.

use gpui::{Hsla, Rgba, hsla};

/// Parse one theme color. The error is the user-facing message a bad theme
/// file reports, so it names the offending text.
pub fn parse(input: &str) -> anyhow::Result<Hsla> {
    let text = input.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex)
            .ok_or_else(|| anyhow::anyhow!("`{input}` is not a #rgb/#rrggbb/#rrggbbaa color"));
    }
    parse_hsla(text).ok_or_else(|| {
        anyhow::anyhow!(
            "`{input}` is not a color: expected `hsl(h, s%, l%)`, `hsla(h, s%, l%, a)` or `#rrggbb`"
        )
    })
}

/// Render a color back out in the native `hsl()`/`hsla()` form. Used by the
/// settings window when it writes a theme out, and by the round-trip tests.
pub fn format(color: Hsla) -> String {
    let h = color.h * 360.0;
    let s = color.s * 100.0;
    let l = color.l * 100.0;
    if color.a >= 1.0 {
        format!("hsl({}, {}%, {}%)", trim(h), trim(s), trim(l))
    } else {
        format!(
            "hsla({}, {}%, {}%, {})",
            trim(h),
            trim(s),
            trim(l),
            trim(color.a)
        )
    }
}

/// `240.0` prints as `240`, `12.5` as `12.5` — no trailing `.0` noise in a
/// file a human is expected to edit.
fn trim(value: f32) -> String {
    let rounded = (value * 1000.0).round() / 1000.0;
    let mut text = format!("{rounded}");
    if let Some(stripped) = text.strip_suffix(".0") {
        text = stripped.to_string();
    }
    text
}

fn parse_hsla(text: &str) -> Option<Hsla> {
    let lower = text.to_ascii_lowercase();
    let (expected_alpha, body) = match lower.strip_prefix("hsla") {
        Some(body) => (true, body),
        None => (false, lower.strip_prefix("hsl")?),
    };
    let body = body.trim().strip_prefix('(')?.strip_suffix(')')?;
    // Commas are the documented separator; spaces alone (the CSS Color 4
    // spelling) are accepted too, because half the themes in the wild use it.
    let parts: Vec<&str> = if body.contains(',') {
        body.split(',').collect()
    } else {
        body.split_whitespace().collect()
    };
    if parts.len() != if expected_alpha { 4 } else { 3 } {
        return None;
    }
    let h = number(parts[0])? / 360.0;
    let s = percentage(parts[1])?;
    let l = percentage(parts[2])?;
    let a = match parts.get(3) {
        Some(part) => percentage_or_unit(part)?,
        None => 1.0,
    };
    Some(hsla(h, s, l, a))
}

fn parse_hex(hex: &str) -> Option<Hsla> {
    let hex = hex.trim();
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    // `#abc` is the CSS shorthand for `#aabbcc`.
    let expanded = if hex.len() == 3 || hex.len() == 4 {
        hex.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        hex.to_string()
    };
    let (rgb, alpha) = match expanded.len() {
        6 => (&expanded[..6], 255u32),
        8 => (
            &expanded[..6],
            u32::from_str_radix(&expanded[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    let value = u32::from_str_radix(rgb, 16).ok()?;
    Some(
        Rgba {
            r: ((value >> 16) & 0xff) as f32 / 255.0,
            g: ((value >> 8) & 0xff) as f32 / 255.0,
            b: (value & 0xff) as f32 / 255.0,
            a: alpha as f32 / 255.0,
        }
        .into(),
    )
}

fn number(text: &str) -> Option<f32> {
    let text = text.trim().trim_end_matches("deg");
    text.trim().parse::<f32>().ok().filter(|n| n.is_finite())
}

/// `4%` and the bare `0.04` both mean the same thing; a percentage is what a
/// theme file normally carries.
fn percentage(text: &str) -> Option<f32> {
    let text = text.trim();
    match text.strip_suffix('%') {
        Some(head) => Some(number(head)? / 100.0),
        None => number(text),
    }
}

fn percentage_or_unit(text: &str) -> Option<f32> {
    percentage(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_parses_to_the_exact_same_bits_a_rust_literal_would_produce() {
        // The built-in themes moved from Rust literals into JSON in M7; this
        // is the guarantee that made that move safe to do without touching a
        // single visual baseline.
        assert_eq!(
            parse("hsl(240, 4%, 12%)").unwrap(),
            hsla(240.0 / 360.0, 0.04, 0.12, 1.0)
        );
        assert_eq!(
            parse("hsla(0, 0%, 0%, 0.35)").unwrap(),
            hsla(0.0, 0.0, 0.0, 0.35)
        );
        assert_eq!(
            parse("hsl(210, 90%, 55%)").unwrap(),
            hsla(210.0 / 360.0, 0.90, 0.55, 1.0)
        );
    }

    #[test]
    fn hsl_round_trips_through_format() {
        for text in [
            "hsl(240, 4%, 12%)",
            "hsla(0, 0%, 0%, 0.35)",
            "hsl(210, 90%, 55%)",
            "hsl(0, 75%, 58%)",
        ] {
            let color = parse(text).unwrap();
            assert_eq!(format(color), text, "{text} did not round-trip");
            assert_eq!(parse(&format(color)).unwrap(), color);
        }
    }

    #[test]
    fn hex_parses_in_every_css_length() {
        let opaque = parse("#ff0000").unwrap();
        assert_eq!(parse("#f00").unwrap(), opaque);
        assert_eq!(parse("#FF0000FF").unwrap(), opaque);
        assert_eq!(opaque.a, 1.0);
        assert!((opaque.h - 0.0).abs() < 1e-6);
        assert!((opaque.s - 1.0).abs() < 1e-6);
        assert!((opaque.l - 0.5).abs() < 1e-6);

        let half = parse("#ff000080").unwrap();
        assert!((half.a - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn space_separated_and_uppercase_forms_are_accepted() {
        let comma = parse("hsl(240, 4%, 12%)").unwrap();
        assert_eq!(parse("HSL(240 4% 12%)").unwrap(), comma);
        assert_eq!(parse("  hsl( 240deg , 4% , 12% ) ").unwrap(), comma);
    }

    #[test]
    fn a_bare_fraction_is_as_good_as_a_percentage() {
        assert_eq!(
            parse("hsl(240, 0.04, 0.12)").unwrap(),
            parse("hsl(240, 4%, 12%)").unwrap()
        );
    }

    #[test]
    fn nonsense_is_rejected_with_the_offending_text_in_the_message() {
        for bad in [
            "blue",
            "#12345",
            "#gggggg",
            "hsl(240, 4%)",
            "hsl(240, 4%, 12%, 1)",
            "hsla(240, 4%, 12%)",
            "rgb(1,2,3)",
            "hsl(nan, 4%, 12%)",
        ] {
            let error = parse(bad).unwrap_err().to_string();
            assert!(error.contains(bad), "{bad}: unhelpful message {error:?}");
        }
    }
}
