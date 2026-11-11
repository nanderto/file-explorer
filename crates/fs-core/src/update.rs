//! Comparing this build's version against the latest published one.
//!
//! Deliberately *only* the comparison. Fetching is an OS call and lives behind
//! [`crate::Platform`]; parsing the release payload needs `serde_json` and
//! lives in the app, which already has it. What is left here is the part worth
//! testing exhaustively and the part that is easy to get subtly wrong: is
//! `0.10.0` newer than `0.9.0`? (Yes — and a string comparison says no.)

use std::fmt;

/// A semantic version, as far as this app cares: three numbers.
///
/// Pre-release and build metadata (`1.0.0-rc.1+abc`) are **parsed and then
/// ignored** for ordering. A tag carrying them is accepted rather than
/// rejected, but `1.0.0-rc.1` and `1.0.0` compare equal — which errs towards
/// *not* nagging someone about an update that may be a release candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parse `"0.1.0"`, or a tag like `"v0.1.0"`.
    ///
    /// Missing components default to zero, so `"v1"` and `"1.0.0"` are the
    /// same version — GitHub tags are written both ways and neither is wrong.
    /// Anything with a non-numeric component is `None`, because a version we
    /// cannot read must not be treated as newer.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or(text.strip_prefix('V'))
            .unwrap_or(text);
        // Drop pre-release / build metadata before splitting on dots.
        let core = text.split(['-', '+']).next().unwrap_or_default().trim();
        if core.is_empty() {
            return None;
        }
        let mut parts = core.split('.');
        let mut next = || -> Option<u32> {
            match parts.next() {
                Some(p) => p.trim().parse().ok(),
                None => Some(0),
            }
        };
        let major = next()?;
        let minor = next()?;
        let patch = next()?;
        // A fourth component means this is not a version we understand.
        if parts.next().is_some() {
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
        })
    }

    /// This build's version, from `CARGO_PKG_VERSION`.
    pub fn current() -> Self {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_default()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Whether `latest` is worth telling the user about.
///
/// Strictly newer: an equal version is not an update, and an *older* published
/// release (someone re-tagging, or a local build running ahead of the last
/// release) must never prompt a "downgrade".
pub fn is_update(latest: Version, current: Version) -> bool {
    latest > current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_parses_with_or_without_its_v() {
        assert_eq!(Version::parse("v0.1.0"), Version::parse("0.1.0"));
        assert_eq!(
            Version::parse("v1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
    }

    #[test]
    fn missing_components_default_to_zero() {
        assert_eq!(Version::parse("v1"), Version::parse("1.0.0"));
        assert_eq!(Version::parse("1.2"), Version::parse("1.2.0"));
    }

    /// The reason this is a parsed comparison and not a string one.
    #[test]
    fn ten_is_newer_than_nine() {
        let ten = Version::parse("0.10.0").unwrap();
        let nine = Version::parse("0.9.0").unwrap();
        assert!(is_update(ten, nine));
        assert!(
            "0.10.0" < "0.9.0",
            "string order disagrees — which is the whole point"
        );
    }

    #[test]
    fn an_equal_version_is_not_an_update() {
        let v = Version::parse("0.1.0").unwrap();
        assert!(!is_update(v, v));
    }

    /// A local build running ahead of the last release must not be told to
    /// "update" backwards.
    #[test]
    fn an_older_release_is_not_an_update() {
        let released = Version::parse("0.1.0").unwrap();
        let running = Version::parse("0.2.0").unwrap();
        assert!(!is_update(released, running));
    }

    #[test]
    fn each_component_orders_independently() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(is_update(v("1.0.0"), v("0.99.99")));
        assert!(is_update(v("0.2.0"), v("0.1.99")));
        assert!(is_update(v("0.1.2"), v("0.1.1")));
    }

    /// Accepted, but not treated as newer than the release it precedes — we
    /// would rather miss a release candidate than nag about one.
    #[test]
    fn pre_release_metadata_is_ignored_for_ordering() {
        let rc = Version::parse("v1.0.0-rc.1").unwrap();
        let release = Version::parse("v1.0.0").unwrap();
        assert_eq!(rc, release);
        assert!(!is_update(rc, release));
    }

    /// A version we cannot read must never count as an update.
    #[test]
    fn unreadable_versions_are_none() {
        for bad in ["", "   ", "latest", "v", "1.x.0", "1.2.3.4", "nightly"] {
            assert_eq!(Version::parse(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn the_current_version_is_readable() {
        // If `Cargo.toml`'s version ever stops parsing, the notifier would
        // silently compare against 0.0.0 and nag forever.
        assert!(
            Version::parse(env!("CARGO_PKG_VERSION")).is_some(),
            "the crate version must parse"
        );
    }
}
