//! Visual regression test runner.
//!
//! Renders real windows off-screen with GPUI's `VisualTestAppContext`
//! (real Metal rendering + deterministic TestDispatcher), captures
//! screenshots, and compares them against baseline PNGs committed under
//! `crates/app/test_fixtures/visual_tests/`.
//!
//! macOS-only at runtime (Metal); compiles to a stub elsewhere.
//!
//! Run the tests:
//!   cargo run -p file-explorer-app --bin visual_test_runner --features visual-tests
//!
//! Update baselines (when the UI intentionally changes):
//!   UPDATE_BASELINE=1 cargo run -p file-explorer-app --bin visual_test_runner --features visual-tests
//!
//! Environment:
//!   UPDATE_BASELINE        - write baselines instead of comparing
//!   VISUAL_TEST_OUTPUT_DIR - where screenshots/diffs go (default: target/visual_tests)

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("visual_test_runner only runs on macOS (it needs the Metal renderer).");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    macos::main()
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context as _, Result, anyhow, bail};
    use file_explorer_app::{Theme, WorkspaceView, visual_diff};
    use gpui::{AnyWindowHandle, AppContext as _, VisualTestAppContext, px, size};
    use std::path::PathBuf;

    /// Minimum fraction of matching pixels for a test to pass.
    const MATCH_THRESHOLD: f64 = 0.99;
    /// Per-channel tolerance, absorbs GPU/AA rounding noise.
    const PIXEL_TOLERANCE: u8 = 3;
    /// Window size for all scenarios; must stay stable or baselines churn.
    const WINDOW_SIZE: (f32, f32) = (1200.0, 760.0);

    pub fn main() {
        let update_baseline = std::env::var("UPDATE_BASELINE").is_ok();
        match run(update_baseline) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("visual tests failed: {e:#}");
                std::process::exit(1);
            }
        }
    }

    /// Every visual scenario: (name, theme). Add new UI states here.
    fn scenarios() -> Vec<(&'static str, Theme)> {
        vec![
            ("workspace_dark", Theme::dark()),
            ("workspace_light", Theme::light()),
        ]
    }

    fn run(update_baseline: bool) -> Result<()> {
        let mut cx = VisualTestAppContext::new(gpui_platform::current_platform(false));

        let mut passed = 0u32;
        let mut updated = 0u32;
        let mut failures: Vec<String> = Vec::new();

        for (name, theme) in scenarios() {
            println!("visual test: {name}");
            match run_scenario(&mut cx, name, theme, update_baseline) {
                Ok(ScenarioResult::Passed) => {
                    println!("  PASS");
                    passed += 1;
                }
                Ok(ScenarioResult::BaselineUpdated) => {
                    println!("  baseline updated");
                    updated += 1;
                }
                Err(e) => {
                    eprintln!("  FAIL: {e:#}");
                    failures.push(name.to_string());
                }
            }
        }

        println!(
            "\n=== visual test summary: {passed} passed, {updated} updated, {} failed ===",
            failures.len()
        );
        if !failures.is_empty() {
            bail!("failed scenarios: {}", failures.join(", "));
        }
        Ok(())
    }

    enum ScenarioResult {
        Passed,
        BaselineUpdated,
    }

    fn run_scenario(
        cx: &mut VisualTestAppContext,
        name: &str,
        theme: Theme,
        update_baseline: bool,
    ) -> Result<ScenarioResult> {
        let window = cx
            .open_offscreen_window(size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), |_, cx| {
                cx.new(|_| WorkspaceView::new(theme))
            })
            .map_err(|e| anyhow!("failed to open off-screen window: {e:?}"))?;
        let handle: AnyWindowHandle = window.into();

        cx.run_until_parked();
        cx.update_window(handle, |_, window, _| window.refresh())
            .map_err(|e| anyhow!("failed to refresh window: {e:?}"))?;
        cx.run_until_parked();

        let screenshot = cx
            .capture_screenshot(handle)
            .map_err(|e| anyhow!("failed to capture screenshot: {e:?}"))?;

        // Close the window before comparing so a failure can't leak windows
        // into the next scenario.
        cx.update_window(handle, |_, window, _| window.remove_window())
            .ok();
        cx.run_until_parked();

        let output_dir = PathBuf::from(
            std::env::var("VISUAL_TEST_OUTPUT_DIR")
                .unwrap_or_else(|_| "target/visual_tests".to_string()),
        );
        std::fs::create_dir_all(&output_dir)?;
        let output_path = output_dir.join(format!("{name}.png"));
        screenshot
            .save(&output_path)
            .with_context(|| format!("saving screenshot to {}", output_path.display()))?;
        println!("  screenshot: {}", output_path.display());

        let baseline_path = baseline_path(name);
        if update_baseline {
            if let Some(parent) = baseline_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            screenshot.save(&baseline_path)?;
            println!("  baseline: {}", baseline_path.display());
            return Ok(ScenarioResult::BaselineUpdated);
        }

        if !baseline_path.exists() {
            bail!(
                "baseline not found: {}\n  Generate baselines on macOS with UPDATE_BASELINE=1, \
                 or trigger the 'Update visual baselines' GitHub workflow.",
                baseline_path.display()
            );
        }

        let baseline = image::open(&baseline_path)
            .with_context(|| format!("opening baseline {}", baseline_path.display()))?
            .to_rgba8();
        let cmp = visual_diff::compare(&screenshot, &baseline, PIXEL_TOLERANCE);
        println!(
            "  match: {:.3}% ({} differing pixels of {})",
            cmp.match_fraction * 100.0,
            cmp.diff_pixel_count,
            cmp.total_pixels
        );

        if cmp.match_fraction >= MATCH_THRESHOLD {
            Ok(ScenarioResult::Passed)
        } else {
            let diff_path = output_dir.join(format!("{name}_diff.png"));
            cmp.diff_image.save(&diff_path)?;
            bail!(
                "image mismatch: {:.3}% match (threshold {:.0}%). Diff: {}",
                cmp.match_fraction * 100.0,
                MATCH_THRESHOLD * 100.0,
                diff_path.display()
            );
        }
    }

    /// Baselines live next to this crate so they version with the UI code.
    fn baseline_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_fixtures/visual_tests")
            .join(format!("{name}.png"))
    }
}
