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
- macOS-specific code lives behind the `Platform` trait (`fs-core/src/platform.rs`) with a stub impl so the workspace builds on Windows/Linux.
- gpui / gpui-component versions are pinned; upgrade only at milestone boundaries, in a dedicated PR.
