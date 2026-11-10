//! "A new version is available" — the notify-only half of plan §7 M8.
//!
//! One GET to the GitHub Releases API at launch, compared against this
//! build's version, and a dismissible bar if there is something newer. It
//! does **not** download or install anything: that is Sparkle's job, and
//! Sparkle is a framework-sized decision this does not pre-empt. What it does
//! do — the version comparison and the "tell the user" surface — is what a
//! real auto-updater would reuse.
//!
//! Three rules it follows, because an update check is the one piece of an app
//! that annoys people when it misbehaves:
//!
//! * **Silent on failure.** No network, a rate limit, a malformed payload and
//!   a missing release are the same outcome: nothing is shown. A failed check
//!   must never interrupt anyone.
//! * **Once per launch.** Not on a timer, not per window.
//! * **Opt-out.** `check_for_updates: false` in `settings.json` and the check
//!   never runs — the request is not made at all, rather than made and
//!   ignored.

use std::sync::Arc;

use fs_core::{Platform, Version};
use gpui::{App, SharedString};

/// Where to ask. The releases API returns the newest **published**,
/// non-draft, non-prerelease release, which is exactly what should prompt.
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/nanderto/file-explorer/releases/latest";

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: Version,
    /// The release page to send the user to. Never a download link: with no
    /// Developer ID signature yet, the page carries the "right-click → Open"
    /// instruction the download alone cannot.
    pub url: SharedString,
}

/// Pull `(tag_name, html_url)` out of a GitHub release payload and decide
/// whether it is newer than `current`.
///
/// Separate from the fetch so the decision is testable against a recorded
/// payload rather than the live API.
pub fn update_from_release_json(body: &str, current: Version) -> Option<AvailableUpdate> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    let version = Version::parse(tag)?;
    if !fs_core::is_update(version, current) {
        return None;
    }
    let url = value
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("https://github.com/nanderto/file-explorer/releases/latest");
    Some(AvailableUpdate {
        version,
        url: SharedString::from(url.to_string()),
    })
}

/// Ask whether a newer release exists. `None` for *every* failure.
pub async fn check(platform: Arc<dyn Platform>, current: Version) -> Option<AvailableUpdate> {
    let body = platform.http_get_text(LATEST_RELEASE_API).await?;
    update_from_release_json(&body, current)
}

/// Whether a check should run at all, given the user's setting.
pub fn enabled(cx: &App) -> bool {
    crate::settings::AppSettings::global(cx).check_for_updates()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed copy of a real GitHub payload — the fields this reads, plus
    /// enough noise that a parser keying on the wrong thing would notice.
    fn release_json(tag: &str) -> String {
        format!(
            r#"{{
                "url": "https://api.github.com/repos/x/y/releases/1",
                "html_url": "https://github.com/x/y/releases/tag/{tag}",
                "tag_name": "{tag}",
                "name": "File Explorer {tag}",
                "draft": false,
                "prerelease": false,
                "assets": []
            }}"#
        )
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn a_newer_release_is_reported_with_its_page() {
        let found = update_from_release_json(&release_json("v0.2.0"), v("0.1.0")).unwrap();
        assert_eq!(found.version, v("0.2.0"));
        assert_eq!(found.url, "https://github.com/x/y/releases/tag/v0.2.0");
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        assert_eq!(
            update_from_release_json(&release_json("v0.1.0"), v("0.1.0")),
            None
        );
    }

    /// A local build ahead of the last release must not be told to go back.
    #[test]
    fn an_older_release_is_not_an_update() {
        assert_eq!(
            update_from_release_json(&release_json("v0.1.0"), v("0.2.0")),
            None
        );
    }

    /// Every failure shape collapses to "nothing to report" — an update check
    /// must never interrupt anyone because it went wrong.
    #[test]
    fn every_malformed_payload_is_silent() {
        for body in [
            "",
            "not json",
            "{}",
            r#"{"tag_name": null}"#,
            r#"{"tag_name": "nightly"}"#,
            r#"{"message": "API rate limit exceeded"}"#,
            "[]",
        ] {
            assert_eq!(
                update_from_release_json(body, v("0.1.0")),
                None,
                "{body:?} should be silent"
            );
        }
    }

    /// A release with no `html_url` still reports, pointing at the releases
    /// page — the version is the useful part.
    #[test]
    fn a_missing_page_falls_back_to_the_releases_list() {
        let found = update_from_release_json(r#"{"tag_name": "v9.0.0"}"#, v("0.1.0")).unwrap();
        assert!(found.url.ends_with("/releases/latest"));
    }

    #[gpui::test]
    async fn a_failed_fetch_reports_nothing(cx: &mut gpui::TestAppContext) {
        let platform = Arc::new(fs_core::StubPlatform::new());
        // The stub answers `None`, which is what no network looks like.
        let found = cx.update(|_| check(platform.clone(), v("0.1.0"))).await;
        assert_eq!(found, None);
    }

    #[gpui::test]
    async fn a_served_release_is_compared(cx: &mut gpui::TestAppContext) {
        let platform = Arc::new(fs_core::StubPlatform::new());
        platform.set_http_response(Some(release_json("v0.9.0")));
        let found = cx
            .update(|_| check(platform.clone(), v("0.1.0")))
            .await
            .unwrap();
        assert_eq!(found.version, v("0.9.0"));
    }
}
