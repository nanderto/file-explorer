# Kickoff prompt — paste into a new Claude Code session in this repo

---

Build the file-explorer application described in `docs/file-explorer-plan.md`, using multi-agent orchestration: **use workflows** (the Workflow orchestrator) both to define the overall application architecture and to build out the features. Work milestone by milestone until the plan's v1 scope is shipped.

## Ground truth — read before anything else

1. `CLAUDE.md` — quality gates and working conventions. These are non-negotiable and enforced by hooks, CI, and branch protection.
2. `docs/file-explorer-plan.md` — product goals, the Explorer-not-Finder behavior spec (§3, the product's identity), UI blueprint, tech stack, milestones M0–M8.
3. `docs/AS_BUILT.md` — what already exists (the index: status, known gaps, deviations, change log; per-crate detail in `docs/as-built/`). Do not redo it: the cargo workspace, M0 window skeleton (`WorkspaceView`, dark/light `Theme`), and the visual regression infrastructure (runner binary, baselines, CI jobs, `update-visual-baselines.yml`) are built and merged.
4. `docs/requirements/Basic window.png` — the target layout.

## Phase A — Architecture ✅ DONE (PR #3) — do not re-run

`docs/ARCHITECTURE.md` exists and is the blueprint every milestone follows. The
record below is kept for provenance only. Skip to Phase B.

<details><summary>How it was produced</summary>

Run a workflow that produces `docs/ARCHITECTURE.md` — the entity/module blueprint the feature milestones will follow. Structure it as parallel research, then a judged synthesis:

**Research fan-out (parallel agents, each reads real source, not docs alone):**
- **Zed itself** (github.com/zed-industries/zed, pin-read at the rev in our `Cargo.toml`): how `workspace`, `pane`, `project_panel`, and `dock` structure entities, actions, focus, and keymap dispatch; how `settings` and `theme` crates load/watch JSON; how Zed separates platform code. These are the reference patterns for a serious GPUI app.
- **gpui-component** (github.com/longbridge/gpui-component): inventory the widgets we need (virtualized Table/List, resizable panels, sidebar, context menu, inputs, modals, breadcrumb, theming API) — what exists, what's missing, how its theme context works, and which gpui rev it currently tracks (this decides whether we can adopt it now or defer to M4).
- **adabraka-ui** (github.com/Augani/adabraka-ui) and Zed's own `ui` crate: fallback widget patterns for anything gpui-component lacks (rubber-band selection, Miller columns).
- **Our plan §5** as the constraint: `fs-core` (no GPUI) / `theme` / `app` crate boundaries, job queue, watcher, undo, `Platform` trait, `VirtualFileSystem` seam.

**Synthesis (judge panel):** 2–3 independent architecture drafts scored against: fidelity to the plan's crate boundaries, testability (fs-core headless; UI via `#[gpui::test]`; visual scenarios), Explorer behavior spec coverage, and incremental deliverability per milestone. The winning draft, with the best ideas grafted from runners-up, becomes `docs/ARCHITECTURE.md`: entity graph (Workspace → Pane → DirListing → Selection), action/keymap architecture, data-flow diagrams for navigation and file operations, gpui-component adoption decision, and per-milestone build order.

**Licensing rule for all research:** Zed's app crates are GPL — study the patterns, never copy code into this repo. `gpui` and `gpui-component` are Apache-2.0.

Land Phase A as its own PR (it updates `docs/AS_BUILT.md` too).

</details>

## Phase B — Features (one orchestrated workflow per milestone, one PR per milestone)

Work through the plan's milestones in order: **M1 read-only browsing → M2 sidebar → M3 file operations → M4 icon view + dual pane → M5 info panel → M6 search/tags/permissions → M7 themes/settings → M8 ship prep.**

**Which milestone is next is decided by `docs/AS_BUILT.md`'s Status table, not by this document** — it is the authority on what is already built, and this file is not updated per milestone. Before doing anything: read that table, confirm against `git log --oneline -15` and `gh pr list --state all`, and start at the first milestone not marked complete. Never re-run a finished milestone.

For each milestone:

1. **Scout inline** (cheap, no agents): confirm the milestone's acceptance criteria from the plan and the relevant ARCHITECTURE.md sections; list the files/modules to touch.
2. **Run a build workflow**: decompose into parallel implementation lanes where independent (e.g. M1: listing engine in fs-core / details-view rendering / navigation+history / breadcrumb+address bar), pipeline each lane through implement → unit+integration+UI tests → adversarial review (reviewer agents try to break Explorer-spec conformance and the threading rule "UI thread never touches the disk"). Confirmed findings loop back before the PR.
3. **Every PR must satisfy the CLAUDE.md definition of done** — build, clippy `-D warnings`, fmt, unit + integration + `#[gpui::test]` UI tests created and green, `docs/AS_BUILT.md` updated. The pre-commit/pre-push hooks and CI enforce this; never bypass with `--no-verify`.
4. **Visual regression**: add/extend scenarios in `crates/app/src/bin/visual_test_runner.rs` for every new UI state worth pinning. When the UI intentionally changes, regenerate baselines with `gh workflow run update-visual-baselines.yml --ref <branch>` (never locally, **even on a Mac** — baselines must come from the same macOS runner image CI compares against). Inspect the `visual-test-output` artifact before touching baselines on any unexpected diff.
5. Merge only through PRs with the `CI` check green and the branch up to date; get explicit user approval before each merge unless standing approval has been given in-session.
6. After merge: update `docs/AS_BUILT.md` status, then proceed to the next milestone.

## Constraints and cautions

- **Platform**: development happens on **either macOS or Windows** (check which, with `uname`, before assuming); the product targets macOS. See CLAUDE.md §"Development machines". Portable code — all of `fs-core`, the `Platform` stub, every test — must build and pass on both. `cfg(target_os = "macos")` code compiles only on a Mac: on one, compile-check and exercise it locally; on Windows it is invisible locally, so rely on the CI macOS jobs and fix forward quickly. Keep mac-specific code behind the `Platform` trait in fs-core with portable stub impls.
- **Dependency discipline**: `gpui`/`gpui_platform` are pinned to a zed rev in the workspace `Cargo.toml`; if you adopt gpui-component, let *its* gpui requirement drive the pin, and bump revs only in a dedicated PR at a milestone boundary (visual baselines will need regenerating after any gpui bump).
- **Budget sanity**: one workflow per milestone, sized to the milestone; don't fan out for trivial glue work. Verify adversarially where correctness matters most (file operations in M3, permissions in M6).
- If a milestone's acceptance criteria can't be met as planned, deliver what's solid, record the deviation in `docs/AS_BUILT.md`, and flag it in the PR — do not silently shrink scope.

## Starting a session

1. Read `CLAUDE.md` (non-negotiable gates), then `docs/AS_BUILT.md`'s Status table.
2. Check for work in flight before starting anything new: `gh pr list --state open`
   (an open PR may be a finished milestone awaiting merge), and `git status` /
   `git branch --show-current` (uncommitted work from an interrupted run — verify
   the gate before assuming it is broken *or* good).
3. Start the first milestone the Status table does not mark complete, at Phase B
   step 1. Phase A is done; do not re-run it.

Then proceed, milestone by milestone, until the plan's v1 scope is shipped.
