# As Built

The living record of what is **actually implemented**, as opposed to what is planned
(`docs/file-explorer-plan.md`). Every PR that changes code must update this file —
the quality gate (`scripts/gate.sh`) and CI enforce it.

How to update: add or amend the relevant component section below, and add a row to the
change log. Record what exists, how it deviates from the plan (and why), and any known
limitations. Keep it truthful — this file is only useful if it matches the code.

## Status

**Current milestone:** M1 code-complete — fs-core crate (M1 subset:
exec/entry/vfs/sort/listing/watcher), app state layer
(actions/keymap/FsContext/Workspace/Pane), details view
(`dir_view.rs` + `views/details_list.rs`), and the address bar
(`address_bar.rs` + vendored `input/text_input.rs`, see `crates/app/VENDORED.md`)
are all built and tested (68 unit/gpui tests green on Windows). Visual
scenarios `listing_populated`, `listing_sorted_by_size`, `address_bar_editing`
added to the runner on deterministic FakeVfs fixtures; all five baselines
(incl. the two M0 ones — the new address-bar chrome row intentionally changes
the workspace layout) regenerate via the update-visual-baselines workflow.

Additional M1 notes:
- Address bar: breadcrumb rendered by the Pane (clickable segments; blank-space
  click or `cmd-l` enters editing); `AddressBar` entity owns the vendored
  `InputState`, background-listed autocomplete (dirs-only, prefix,
  generation-guarded), `tab` accept-in-place, background `Confirm` validation
  with inline errors, `escape` cancel; both outcomes return focus to the pane.
- `Theme` gained an `error` color (both palettes).
- Vendored input drives editing keys via `TextInput`-context bindings in
  `keymap.rs` forwarding to `input_state`-namespace actions; `set_value` was
  fixed to replace the whole content (see VENDORED.md mods 5–7).

## Components

### Workspace / build tooling
- Quality gate (`scripts/gate.sh`), Claude Code hooks (`.claude/settings.json`),
  CI (`.github/workflows/ci.yml`), branch protection script
  (`scripts/setup-branch-protection.sh`).
- Cargo workspace (edition 2024, toolchain pinned to 1.97.1) with `crates/app`.
  gpui + gpui_platform pinned to zed rev `fd82517a` (needed for
  `VisualTestAppContext`).

### app (GPUI)
- `workspace.rs`: `Workspace` entity (refactor of the M0 `WorkspaceView`,
  which is deleted) — same chrome (titlebar, sidebar placeholder, info-panel
  placeholder, pixel-identical to the M0 baselines), root `track_focus` +
  `Workspace` key context, owns `panes: Vec<Entity<Pane>>` (len 1 for M1) +
  `active_pane_ix`; handles `FocusAddressBar` (forwards to the active pane)
  and `ToggleHiddenFiles` (fans out to every pane).
- `pane.rs`: `Pane` entity per ARCHITECTURE.md §2/§4a — `NavHistory` of
  `NavEntry { path, cursor: Option<EntryId>, scroll_top }` with restore
  semantics (back/forward restore cursor + scroll; cursor dropped if its path
  vanished; any navigation truncates the forward stack), per-pane
  `ListingCache` render-cached-then-refresh navigation (cache hits paint
  stale-marked in the same frame; hits are skipped when sort/show-hidden
  differ), generation-guarded background `list_dir` loads, status-line data
  (item count + free space via `Vfs::free_space`), `AddressBarMode`
  (Breadcrumb/Editing state only — the input entity is the address-bar build
  step), `SortBy`/`Refresh`/`GoUp`/history action handlers + mouse buttons
  4/5. Until `dir_view.rs` lands, the pane holds snapshot/cursor/scroll itself
  and renders a placeholder body.
- `actions.rs`: the §0 table's M1 action set (`actions!` namespace
  `file_explorer`) + parameterized `SortBy { key: SortKey }`. Deviation from
  the ARCHITECTURE.md §3 sketch: `SortBy` derives `Action` with `no_json`
  instead of serde — this gpui rev requires `schemars::JsonSchema` for
  JSON-buildable actions, which isn't needed until user keymap overrides (M7);
  `SortBy` is mouse-dispatched so nothing is lost in M1.
- `keymap.rs`: the §0 M1 rows transcribed 1:1 into `cx.bind_keys` with the
  declared contexts (`Workspace`, `Pane`, `DirView && !renaming`,
  `AddressBar`, `TextInput`).
- `app_state.rs`: `FsContext` global (`Arc<dyn Vfs>` + `Arc<dyn Spawner>`;
  job queue/undo/clipboard join at M3) and `GpuiSpawner`, the fs-core
  `Spawner` adapter over `gpui::BackgroundExecutor` (timers run on the
  deterministic test clock under `#[gpui::test]`).
- `#[gpui::test]` coverage (28 app tests): keymap dispatch guards for every
  declared M1 key context — `Workspace` and `Pane` through the real entities,
  `DirView` (incl. the `!renaming` guard), `AddressBar`, and `TextInput`
  through a probe view carrying the same context tokens until those entities
  land; pane NavHistory/restore/truncation, stale-load generation guard,
  cached-then-fresh swap, refresh-preserves-cursor, sort flip, hidden toggle,
  and error surfacing, all against `FakeVfs` fixtures.
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
- Crate `crates/fs-core` (edition 2024, **no gpui dependency**, builds/tests
  headless on Windows). M1 subset per ARCHITECTURE.md §6/§10; ops/undo/
  clipboard/platform are deliberately absent (M2/M3, additive growth — no
  stubs). Feature `test-support` exposes `FakeVfs`/`TestSpawner` to downstream
  crates; this crate's own tests see them via `cfg(any(test, feature))`.
- `exec.rs`: `Spawner` trait (`spawn`, `timer`, object-safe `unblock_raw`) +
  `SpawnerExt::unblock<T>` exactly per §5; `TestSpawner` runs `spawn`/`unblock`
  on plain threads and `timer` on a controllable fake clock (`advance`/`now`).
  `advance` briefly waits (bounded, 2 s) for a pending timer so tests can't
  race a just-spawned pump thread registering its timer.
- `entry.rs`: `EntryId` (`Arc<Path>` newtype), `EntryKind`
  (`File`/`Dir`/`Symlink { target_kind }`), `FileEntry`
  (path/name/kind/size/modified/created/hidden), `EntryMeta`. The plan-§6
  `perms`/`tags` fields are deferred to M5/M6 (additive).
- `vfs.rs`: `Vfs` trait with only the M1 methods (`read_dir` → streamed
  entries, `metadata` with missing = `Ok(None)`, `volume_key`, `free_space`,
  `watch`). `RealVfs` wraps `std::fs` with every blocking call through
  `SpawnerExt::unblock`; free space via the `fs4` crate (statvfs-shaped, per
  §10's temporary M1 method). `volume_key` is derived purely from path shape
  (Windows drive prefix / `/Volumes/<name>` / `/`) until platform volumes land
  at M2. `hidden` = dotfile only in M1 (Finder hidden flag arrives with the
  platform trait). `FakeVfs` (test-support): in-memory `BTreeMap` tree from
  `serde_json::json!` fixtures, mutation helpers that emit watcher events,
  `emit_event`/`pause_events`/`flush_events`, per-path error injection,
  configurable free space.
- `sort.rs`: `SortKey { Name, Size, DateModified }`, `SortDirection`,
  `SortSpec` (folders-first + direction flip; flip never reorders the
  folders-first partition); hand-written `natural_cmp` (case-insensitive,
  numeric digit runs so `file2 < file10`, overflow-safe, total order via raw
  tie-break). `SortKey` derives serde for the future `SortBy` action.
- `listing.rs`: `list_dir(vfs, dir, sort, show_hidden, generation)` →
  `ListingSnapshot` (owned args so the future is `'static`);
  `patch_listing(snapshot, Vec<ListingPatch{Upsert|Remove}>)` preserves sort
  order via binary-search insertion and respects the hidden filter (`Rescan`
  has no patch form — consumers reload); `ListingCache` (hand-rolled LRU,
  default capacity 16) with hit-promotes/write-back-replaces semantics.
- `watcher.rs`: `PathEvent{Created,Changed,Removed,Rescan}`; debounce pump
  runs on `Spawner::spawn` + `Spawner::timer` (fake time in tests), coalesces
  a batch (duplicates dropped, any `Rescan` collapses the batch); RAII
  `WatchGuard` unregisters on drop. Real impl: one process-global `notify`
  watcher with per-root registrations; `watch` is best-effort (failure ⇒
  terminated stream + noop guard). The notify path is exercised manually
  (per-milestone Mac checklist), not by unit tests — tests drive the FakeVfs
  event path per the §9 map.
- Tests: 33 unit tests (`cargo test -p fs-core`) covering the §9 rows for
  sort/listing/cache/watcher/exec, plus `RealVfs` list/stat/free-space against
  a `tempfile` tree and FakeVfs fixture/error/pause-flush behavior.

### theme (crate)
- Not started (interim `theme` module lives inside `crates/app`).

### Architecture (Phase A)
- `docs/ARCHITECTURE.md` — the entity/module blueprint for M1–M8, produced by an
  orchestrated workflow (4 source-research agents over Zed @ pinned rev,
  gpui-component, adabraka-ui → 3 independent drafts → 3-judge panel →
  synthesis). Contains the behavior→action traceability table (source of truth
  for `keymap.rs`), entity graph, data-flow diagrams, threading model, fs-core
  internals, widget build-list, testing map, and per-milestone build order.
- `.claude/agents/` — Orchestrator/Proposer/Critic/Builder/Reviewer agent pack
  used to drive milestone workflows (Phase B).
- `docs/KICKOFF_PROMPT.md` — the standing build directive for new sessions.

## Deviations from the plan

- Theme lives as a module in `crates/app` instead of its own crate until the
  JSON theme system lands (M7); the plan's crate split is deferred to avoid a
  near-empty crate.
- Visual regression testing (not explicitly in the plan's testing section) was
  added ahead of M1, modeled on Zed's visual test runner.
- **gpui-component is NOT adopted**, overturning plan §4: it floats on zed main
  (no rev pin) which conflicts structurally with our `fd82517a` pin (required by
  `VisualTestAppContext`), its dialogs/menus demand an all-or-nothing `Root`
  runtime, and it drags heavy deps. Instead: ~6 hand-built widgets on gpui
  primitives + adabraka-ui's MIT text input vendored via `crates/app/VENDORED.md`.
  Full rationale and revisit conditions: ARCHITECTURE.md §7. Plan §4 amendment
  pending.

## Change log

| Date | PR | Change |
|---|---|---|
| 2026-08-22 | — | Repo bootstrapped: plan, CLAUDE.md, quality gate, hooks, CI, this file. |
| 2026-08-22 | — | Cargo workspace + M0 window skeleton (`WorkspaceView`, dark/light `Theme`); GPUI visual regression tests (runner binary, unit-tested pixel diff, baselines dir, CI job, baseline-update workflow). |
| 2026-08-22 | — | Phase A: `docs/ARCHITECTURE.md` via orchestrated research/draft/judge workflow; gpui-component rejected (see Deviations); agent pack (`.claude/agents/`) and kickoff prompt added. |
| 2026-08-22 | — | M1: `crates/fs-core` created (exec/entry/vfs/sort/listing/watcher, `test-support` feature with FakeVfs + TestSpawner, 33 unit tests). Deps: futures, async-channel, async-trait, notify, serde, fs4; serde_json/tempfile for tests. |
| 2026-08-22 | — | M1 app state layer: `actions.rs` + `keymap.rs` (§0 M1 rows) with per-context dispatch tests, `app_state.rs` (`FsContext` global + `GpuiSpawner` executor adapter), `workspace_view.rs` refactored into `workspace.rs` (`Workspace` entity, `Vec<Entity<Pane>>`), `pane.rs` (`NavHistory`/`NavEntry` restore, `ListingCache` cached-then-fresh loads, generation guard, status-line data). 28 app tests (`#[gpui::test]` + unit). App deps: + fs-core, futures; dev: gpui test-support, fs-core test-support, serde_json. |
| 2026-08-22 | — | M1 details view (`dir_view.rs`, `views/details_list.rs`: uniform_list, sortable headers, cursor selection, type-ahead on fake time, hidden toggle, open via Enter/double-click) and address bar (`address_bar.rs` + vendored adabraka `input/text_input.rs` per `VENDORED.md`; breadcrumb in Pane, editor entity with background autocomplete/validation). `Theme` + `error` color. Visual runner: FakeVfs fixture scenarios (`listing_populated`, `listing_sorted_by_size`, `address_bar_editing`). 68 tests green. Deps: + regex, once_cell, unicode-segmentation; serde_json optional under `visual-tests`. |
