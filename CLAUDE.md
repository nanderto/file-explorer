# file-explorer

A native macOS file manager in Rust on GPUI, with Windows File Explorer behavior (not Finder). Full plan: `docs/file-explorer-plan.md`. Requirements: `docs/requirements/` (screenshot + feature overview).

## Repo layout

- `crates/fs-core/` — filesystem logic, no GPUI dependency. All operations, listings, job queue, undo, platform trait.
- `crates/theme/` — theme model + JSON loader.
- `crates/app/` — the GPUI application (workspace, panes, views, dialogs).
- `docs/AS_BUILT.md` — living record of what is actually implemented. **Must be updated in every PR that changes code.**
- `scripts/gate.sh` — the quality gate (also run by hooks and CI).

## Commands

- Build: `cargo build --workspace --all-targets`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings` (warnings are errors; never `#[allow]` your way past a lint without a comment justifying it)
- Format: `cargo fmt --all` (checked in CI with `--check`)
- All tests: `cargo test --workspace`
- One crate: `cargo test -p fs-core`
- Full local gate (exactly what hooks/CI run): `bash scripts/gate.sh push`
- Visual regression tests (only runs on macOS): `cargo run -p file-explorer-app --bin visual_test_runner --features visual-tests`

## Development machines

Development happens on **either macOS or Windows** — both are first-class, and no
change may assume one of them. The product targets macOS, so the two differ in
what they can *verify*, not in what they may *contain*:

- **Everything portable** — all of `fs-core` (it is GPUI-free and headless), the
  `Platform` stub impl, and every unit/integration/`#[gpui::test]` test — must
  build and pass on both. `cargo test --workspace` green is the bar on either
  machine, and CI enforces it on macOS.
- **`cfg(target_os = "macos")` code** (objc2 volumes, tags, QuickLook
  thumbnails, trash) only compiles on a Mac. On a Mac, compile-check and
  exercise it locally — that is the whole point of being on one. On Windows it
  is invisible to the local build, so rely on the CI macOS jobs and fix forward
  quickly; never let it rot behind a stub that happens to satisfy the tests.
- **Visual regression tests** only run on macOS, and their *baselines* come from
  the CI runner image regardless of which machine you are on — see below.
- Never gate behavior on the developer's OS. If a task genuinely cannot be
  verified on the machine at hand, say so in the PR rather than leaving the gap
  silent.

## Visual regression tests

- Baselines live in `crates/app/test_fixtures/visual_tests/*.png`; the CI job `Visual regression tests (macOS)` renders off-screen windows with `gpui::VisualTestAppContext` and compares against them (≥99% pixel match, per-channel tolerance 3).
- Scenarios are declared in `crates/app/src/bin/visual_test_runner.rs` (`scenarios()`); add one for every new UI state worth pinning.
- When the UI **intentionally** changes, regenerate baselines from the PR branch — never hand-edit or locally regenerate them, **even on a Mac**: baselines must come from the same macOS runner image CI compares on, and a local Mac differs from it (OS version, font rendering, GPU) enough to produce baselines that then fail in CI.
  `gh workflow run update-visual-baselines.yml --ref <branch>` — it commits updated PNGs back to the branch.
- A visual-test failure uploads the captured screenshots and red/dimmed diff images as the `visual-test-output` CI artifact — inspect those before touching baselines.
- Keep renders deterministic: fixed window size (1200×760), fixed font (`Helvetica`), no wall-clock-dependent UI in captured states.

## Definition of done — every change, every PR

A change is not done until ALL of these hold. Do them as part of the work, not as an afterthought:

1. **It builds**: `cargo build --workspace --all-targets` succeeds.
2. **Lint passes**: `cargo clippy --workspace --all-targets -- -D warnings` is clean; `cargo fmt --all --check` is clean.
3. **Unit tests are created** for new/changed logic — in-module `#[cfg(test)] mod tests` with `#[test]`, primarily in `fs-core`. Every file operation, sort rule, and conflict path gets unit tests against `tempfile` trees.
4. **Integration tests are created** for cross-module behavior — `crates/<crate>/tests/*.rs` (e.g. copy-tree-with-conflicts, cancel mid-copy, undo-of-move leaving the filesystem correct).
5. **UI tests are created** for new/changed UI behavior — `#[gpui::test]` tests in `crates/app` covering pane state, selection, navigation history, and keymap dispatch.
6. **All tests pass**: `cargo test --workspace` is green. Never skip, `#[ignore]`, or delete a failing test to get green — fix the cause.
7. **Documentation is updated**, always including `docs/AS_BUILT.md` (what was built/changed, any deviation from the plan). Update `docs/file-explorer-plan.md` if the architecture or milestones changed, and rustdoc comments on public `fs-core` APIs.

## Enforcement (do not work around these)

- **Hooks** (`.claude/settings.json`): edited `.rs` files are auto-formatted; `git commit`, `git push`, and `gh pr create` are blocked by `scripts/gate.sh` unless the gate passes (build + clippy + fmt + all tests + "docs/AS_BUILT.md updated" + "tests accompany source changes").
- **CI** (`.github/workflows/ci.yml`): the same gate on every PR, on macOS. The aggregate check is named `CI`.
- **Branch protection**: `main` requires the `CI` status check and "require branches to be up to date before merging" (set up via `scripts/setup-branch-protection.sh`). Never push directly to `main`; branch and open a PR.
- Escape hatch, rare by design: a change that genuinely needs no tests/docs (e.g. pure comment fix) may include `[skip-checks]` in the commit command; in CI, apply the `skip-checks` PR label. If you use it, say so in the PR description and why.

## Working conventions

- Run `git commit` / `git push` as standalone commands (not chained with `&&`) so the gate hook output stays readable.
- Keep `fs-core` free of GPUI imports — it must build and test headless on any platform.
- The UI thread never touches the disk; all I/O goes through the background executor.
- No hard-coded colors in `crates/app` — every color comes from the active theme.
- macOS-specific code lives behind the `Platform` trait (`fs-core/src/platform/`) with a portable, deterministic stub impl, so the whole workspace builds and tests on macOS, Windows and Linux alike.
- gpui / gpui-component versions are pinned; upgrade only at milestone boundaries, in a dedicated PR.
