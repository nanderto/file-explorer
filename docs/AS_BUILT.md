# As Built

The living record of what is **actually implemented**, as opposed to what is planned
(`docs/file-explorer-plan.md`). Every PR that changes code must update this file —
the quality gate (`scripts/gate.sh`) and CI enforce it.

How to update: add or amend the relevant component section below, and add a row to the
change log. Record what exists, how it deviates from the plan (and why), and any known
limitations. Keep it truthful — this file is only useful if it matches the code.

## Status

**Current milestone:** M0 in progress — cargo workspace exists with a minimal
GPUI window skeleton and visual regression test infrastructure.

## Components

### Workspace / build tooling
- Quality gate (`scripts/gate.sh`), Claude Code hooks (`.claude/settings.json`),
  CI (`.github/workflows/ci.yml`), branch protection script
  (`scripts/setup-branch-protection.sh`).
- Cargo workspace (edition 2024, toolchain pinned to 1.97.1) with `crates/app`.
  gpui + gpui_platform pinned to zed rev `fd82517a` (needed for
  `VisualTestAppContext`).

### app (GPUI)
- `WorkspaceView`: static M0 skeleton of the reference layout — titlebar,
  sidebar (Devices/Favorites/Tags placeholders), main pane with status line,
  info panel. No interactivity yet.
- `theme` module: hard-coded dark + light palettes (`Theme::dark()/light()`);
  the JSON theme system replaces this at M7.
- **Visual regression tests**: `visual_test_runner` binary (feature
  `visual-tests`, macOS-only at runtime) renders `WorkspaceView` off-screen via
  `gpui::VisualTestAppContext`, captures Metal-rendered screenshots, compares
  against baselines in `crates/app/test_fixtures/visual_tests/` (≥99% match,
  channel tolerance 3, union-canvas so size changes always fail). Pixel-diff
  logic in `visual_diff` module is platform-independent and unit-tested.
  Scenarios: `workspace_dark`, `workspace_light`.
- CI: `Visual regression tests (macOS)` job runs the comparison per PR;
  `update-visual-baselines.yml` (manual dispatch, non-main branches) regenerates
  baselines on the same runner image and commits them to the branch.

### fs-core
- Not started.

### theme (crate)
- Not started (interim `theme` module lives inside `crates/app`).

## Deviations from the plan

- Theme lives as a module in `crates/app` instead of its own crate until the
  JSON theme system lands (M7); the plan's crate split is deferred to avoid a
  near-empty crate.
- Visual regression testing (not explicitly in the plan's testing section) was
  added ahead of M1, modeled on Zed's visual test runner.

## Change log

| Date | PR | Change |
|---|---|---|
| 2026-08-22 | — | Repo bootstrapped: plan, CLAUDE.md, quality gate, hooks, CI, this file. |
| 2026-08-22 | — | Cargo workspace + M0 window skeleton (`WorkspaceView`, dark/light `Theme`); GPUI visual regression tests (runner binary, unit-tested pixel diff, baselines dir, CI job, baseline-update workflow). |
