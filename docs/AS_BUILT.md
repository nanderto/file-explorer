# As Built

The living record of what is **actually implemented**, as opposed to what is planned
(`docs/file-explorer-plan.md`). Every PR that changes code must update this file —
the quality gate (`scripts/gate.sh`) and CI enforce it.

How to update: add or amend the relevant component section below, and add a row to the
change log. Record what exists, how it deviates from the plan (and why), and any known
limitations. Keep it truthful — this file is only useful if it matches the code.

## Status

**Current milestone:** M2 in progress — details-view **in-place folder
expansion** (`ExpandSelected`/`CollapseSelected`: right/left keys + disclosure
triangles over a flat row projection with depth-based indentation) and the M1
column-alignment fix (details rows now render fixed-width Size / Date cells
aligned under the headers) are built and tested (88 unit/gpui tests green on
Windows). Previously in M2: the sidebar entity (`sidebar.rs`:
Devices/Favorites/folder-tree sections, `SidebarEvent` wiring into the
Workspace, live volume list, favorites persisted immediately) and hand-built
resizable splitters (sidebar + info-panel widths, clamped), 83 tests at that
point. Earlier in M2: fs-core
platform trait (volumes/eject: `MacPlatform` via objc2 + `StubPlatform`),
`Vfs::load`/`Vfs::atomic_write` persistence primitives, `settings.rs`
favorites stub global, and the `FsContext.platform` handle (77 tests at that
point). Previously: M1 code-complete — fs-core crate (M1 subset:
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

Known M1 gaps (follow-up):
- Process note: baseline commits pushed by the update-visual-baselines
  workflow use `GITHUB_TOKEN`, which GitHub excludes from triggering CI —
  after baselines land on a branch, push any normal commit to get the
  required `CI` check on the new HEAD.

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
  which is deleted) — same chrome (titlebar, info-panel placeholder), root
  `track_focus` + `Workspace` key context, owns `panes: Vec<Entity<Pane>>`
  (len 1 for M1) + `active_pane_ix` and (M2) `Entity<Sidebar>`; handles
  `FocusAddressBar` (forwards to the active pane), `ToggleHiddenFiles` (fans
  out to every pane), and `SidebarEvent` (`NavigateTo` → active pane;
  `Eject` → `Platform::eject` spawned on the fs-core `Spawner`, never the UI
  thread). M2 splitters (ARCHITECTURE.md §8 "Resizable splitters"): invisible
  6px grab strips straddle the sidebar/pane and pane/info-panel borders —
  `on_drag` with an empty drag ghost starts the resize, a body-row
  `on_drag_move::<DraggedSplitter>` recomputes the region width from the
  mouse position, clamped (sidebar 160–400, info panel 180–420; defaults
  220/260 match the old fixed widths). Deviation from the §8 sketch: the
  widths live as plain `Workspace` fields read by `render` instead of a
  shared `Rc<RefCell<Vec<f32>>>` — the workspace itself owns both the drag
  handler and the layout, so no shared handle is needed; same behavior,
  fewer moving parts.
- `sidebar.rs` (M2): `Sidebar` entity per ARCHITECTURE.md §2/§8 —
  emits `SidebarEvent::{NavigateTo(PathBuf), Eject(VolumeId)}` (events up,
  method calls down; the workspace acts). Three collapsible sections:
  **Devices** (volumes from the `Platform` seam with free space and an eject
  affordance on ejectable ones; kept live by `watch_volumes` polling on
  `Spawner::timer` — 2s interval, fake time in tests; pump task + WatchGuard
  held in fields so they die with the view), **Favorites** (rows from
  `AppSettings`; click navigates, `+` in the header pins the active pane's
  folder, per-row `✕` unpins; every change persists immediately via
  `AppSettings::save` → `Vfs::atomic_write` on the background executor;
  drag-to-add, context menus, and the plan-§2 favorite *reordering* are
  deferred to M3's drag infrastructure — the M2 acceptance row needs only
  persistence + eject), **Folders** (Explorer-style tree:
  volume roots at depth 0, expanded nodes' background-loaded dirs-only
  children spliced beneath with a depth field — the §8 flat projection —
  rendered by `uniform_list`; disclosure triangles mutate the expansion set
  and re-flatten; child listings are cached so collapse/re-expand is
  instant; unreadable dirs simply have no children). All colors from the
  `Theme`.
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
  4/5. The pane owns the listing pipeline; the cursor lives in its child
  `DirView` (delegating accessors keep `NavEntry` capture/restore working).
  Cursor retention/restore checks the snapshot **and** the DirView's injected
  expansion rows, so a cursor on an expanded folder's child survives fresh
  loads and refreshes.
- `dir_view.rs` + `views/details_list.rs` (M1, expanded in M2): the details
  view — `uniform_list` over a **flat row projection** (ARCHITECTURE.md §2/§8):
  snapshot rows at depth 0, each expanded folder's background-loaded children
  spliced beneath it with `depth + 1`. `expanded: BTreeSet<Arc<Path>>` +
  cached raw child listings (hidden entries included, loaded once; sorted and
  hidden-filtered at projection time with the snapshot's live
  `SortSpec`/show-hidden, so sort flips and the hidden toggle apply to
  injected children without reloads; collapse keeps caches and descendant
  expansion so re-expanding restores nested state instantly — same policy as
  the sidebar tree). `ExpandSelected` (`right`) expands the cursor's folder;
  `CollapseSelected` (`left`) collapses an expanded folder, and on a
  non-expanded child moves the cursor to its parent row (Explorer behavior);
  disclosure triangles (rendered from the row's depth/expanded fields, files
  get an alignment spacer) dispatch the same toggle. Collapsing a subtree
  holding the cursor pulls the cursor up to the collapsed folder. Cursor
  movement, type-ahead, and open all walk the projection. Rows and the
  sortable header share fixed column-width constants (Size 90px, Date 150px,
  disclosure slot 16px) with `w_full` rows and `flex_none` cells, so body
  cells align under the Name / Size / Date Modified headers (closes the known
  M1 alignment gap).
- `actions.rs`: the §0 table's M1 action set (`actions!` namespace
  `file_explorer`) + parameterized `SortBy { key: SortKey }`. Deviation from
  the ARCHITECTURE.md §3 sketch: `SortBy` derives `Action` with `no_json`
  instead of serde — this gpui rev requires `schemars::JsonSchema` for
  JSON-buildable actions, which isn't needed until user keymap overrides (M7);
  `SortBy` is mouse-dispatched so nothing is lost in M1.
- `keymap.rs`: the §0 M1 rows transcribed 1:1 into `cx.bind_keys` with the
  declared contexts (`Workspace`, `Pane`, `DirView && !renaming`,
  `AddressBar`, `TextInput`).
- `app_state.rs`: `FsContext` global (`Arc<dyn Vfs>` + `Arc<dyn Spawner>` +
  `Arc<dyn Platform>` since M2 — `MacPlatform` on macOS, `StubPlatform`
  elsewhere and in tests/visual scenarios; job queue/undo/clipboard join at
  M3) and `GpuiSpawner`, the fs-core `Spawner` adapter over
  `gpui::BackgroundExecutor` (timers run on the deterministic test clock
  under `#[gpui::test]`).
- `settings.rs` (M2 stub per §1, grows into the real store at M7):
  `AppSettings` global — `SettingsContent { favorites: Vec<PathBuf> }` as
  serde JSON at `dirs::config_dir()/file-explorer/settings.json` (path
  injectable for tests). `settings::init` (called by `main` after
  `app_state::init`) installs defaults immediately, then swaps in the
  background-loaded file unless the global was already mutated; missing or
  corrupt files load as defaults, unknown fields are tolerated. `save()`
  serializes and spawns `Vfs::atomic_write` on the fs-core `Spawner` —
  fire-and-forget, never on the UI thread. Tests: favorites round-trip +
  removal, corrupt/missing/sparse files, and a `#[gpui::test]` proving
  save-then-restart-load survives (the M2 acceptance row's persistence half).
- `#[gpui::test]` coverage (49 app tests incl. unit tests): keymap dispatch guards for every
  declared M1 key context — `Workspace` and `Pane` through the real entities,
  `DirView` (incl. the `!renaming` guard), `AddressBar`, and `TextInput`
  through a probe view carrying the same context tokens until those entities
  land; pane NavHistory/restore/truncation, stale-load generation guard,
  cached-then-fresh swap, refresh-preserves-cursor, sort flip, hidden toggle,
  and error surfacing, all against `FakeVfs` fixtures. M2 sidebar tests:
  stub volumes render + eject updates the list on the next (fake-time) poll,
  `NavigateTo` reaches the active pane through the workspace subscription,
  favorites add/remove persist immediately to the injectable settings path
  (file contents asserted through `FakeVfs`), tree expand/collapse
  re-flattens correctly (depths, hidden/file exclusion, cached re-expand,
  nested expansion preserved), independent section collapse, and splitter
  width clamping. M2 details-expansion tests (`dir_view.rs`): expand injects
  children at the right depths (hidden filtered), collapse removes the
  subtree and re-expand restores nesting from cache, the path-keyed cursor
  survives re-projection and refresh (injected-row cursor retained) and is
  pulled up on subtree collapse, `right`/`left` dispatch on the real focused
  DirView (incl. left-moves-to-parent and top-level no-op), and the hidden
  toggle applies to already-loaded children without a reload.
- `theme` module: hard-coded dark + light palettes (`Theme::dark()/light()`);
  the JSON theme system replaces this at M7.
- **Visual regression tests**: `visual_test_runner` binary (feature
  `visual-tests`, macOS-only at runtime) renders `WorkspaceView` off-screen via
  `gpui::VisualTestAppContext`, captures Metal-rendered screenshots, compares
  against baselines in `crates/app/test_fixtures/visual_tests/` (≥99% match,
  channel tolerance 3, union-canvas so size changes always fail). Pixel-diff
  logic in `visual_diff` module is platform-independent and unit-tested.
  Scenarios: `workspace_dark`, `workspace_light`, `listing_populated`,
  `listing_sorted_by_size`, `address_bar_editing`, and (M2)
  `sidebar_tree_expanded` (navigates, then expands `/` and `/home` in the
  sidebar tree) and `details_folder_expanded` (navigates to `/home`, then
  expands `/home/Documents` in place in the details view). The runner
  installs the FakeVfs fixture **and** a fixture settings file (two
  favorites) via `settings::init_with_path`, so all content is deterministic.
  The M2 sidebar changes every scenario's sidebar region, and the M2 column
  alignment/disclosure-slot fix changes every listing scenario's rows — all
  baselines (plus the two new scenarios') must be regenerated via the
  update-visual-baselines workflow on the PR branch.
- CI: `Visual regression tests (macOS)` job runs the comparison per PR;
  `update-visual-baselines.yml` (manual dispatch, non-main branches) regenerates
  baselines on the same runner image and commits them to the branch.

### fs-core
- Crate `crates/fs-core` (edition 2024, **no gpui dependency**, builds/tests
  headless on Windows). M1 subset per ARCHITECTURE.md §6/§10 plus the M2
  platform/persistence additions below; ops/undo/clipboard are deliberately
  absent (M3, additive growth — no stubs). Feature `test-support` exposes
  `FakeVfs`/`TestSpawner` to downstream crates; this crate's own tests see
  them via `cfg(any(test, feature))`.
- `platform/` (M2): `Platform` trait — the M2 surface only (`volumes() ->
  Vec<VolumeInfo { volume_id, name, path, total, free, ejectable }>`,
  `eject(&VolumeId)`); later milestones add tags/thumbnail/open/reveal
  additively. `VolumeId` wraps the mount path (unique per mounted volume;
  exactly what eject and navigation need — a UUID adds nothing at M2).
  `watch_volumes(platform, spawner, interval)` is a free function returning
  `(BoxStream<Vec<VolumeInfo>>, WatchGuard)` — ARCHITECTURE §6 specifies no
  watch method on the trait, so change detection is a poller on
  `Spawner::timer` (fake time in tests; emits initial list, then only on
  change; guard drop ends the stream). `macos.rs`
  (`cfg(target_os = "macos")`): volumes via objc2-foundation
  `NSFileManager mountedVolumeURLsIncludingResourceValuesForKeys:options:`
  with name/capacity/ejectable resource values, wrapped in
  `SpawnerExt::unblock`. **Deviation:** `eject` shells out to
  `diskutil eject` instead of Foundation's block-based
  `unmountVolumeAtURL:...` (avoids a `block2` dependency and a run-loop
  delivery assumption; revisit at the M2 Mac checklist). `stub.rs` (all
  platforms): fixed deterministic list — Macintosh HD (/, not ejectable),
  External SSD + Camera (/Volumes/..., ejectable); `eject` removes the volume
  from subsequent reads so the sidebar eject flow is testable; used by
  Windows/Linux dev builds, tests, and visual scenarios.
- `Vfs` grew `load(path) -> Vec<u8>` and `atomic_write(path, data)` (M2, per
  §6): RealVfs writes a `tempfile::NamedTempFile` **in the destination's own
  directory**, syncs, then persists (rename) over the destination — old or
  new contents, never a truncated mix; missing parent dirs are created
  (settings write into a config dir that may not exist). FakeVfs stores file
  contents (fixture strings are now real bytes), mirrors the
  create-missing-parents/fail-on-file-ancestor semantics, and emits
  Created/Changed watcher events on atomic_write.
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
- Tests: 39 unit tests (`cargo test -p fs-core`) covering the §9 rows for
  sort/listing/cache/watcher/exec, plus `RealVfs` list/stat/free-space against
  a `tempfile` tree and FakeVfs fixture/error/pause-flush behavior; M2 adds
  atomic_write crash-safety semantics (round-trip, replace, no temp leftovers,
  failed write leaves destination intact) on both Vfs impls, stub-volume
  determinism, stub eject rules, and the volume-watch poller on fake time.
  The objc2 macOS path compiles only on macOS (exercised by CI + the
  per-milestone Mac checklist, like the notify watcher).

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
| 2026-08-22 | — | M2 fs-core platform + persistence: `platform/` (`Platform` trait volumes/eject, `VolumeInfo`/`VolumeId`, `MacPlatform` via objc2-foundation NSFileManager + `diskutil eject` deviation, portable deterministic `StubPlatform`, polling `watch_volumes`), `Vfs::load` + `Vfs::atomic_write` (tempfile-in-same-dir + persist; FakeVfs gained file contents), app `settings.rs` stub global (favorites JSON via atomic_write off the UI thread, injectable path), `FsContext.platform` handle. 77 tests green (39 fs-core + 38 app). Deps: fs-core + tempfile, objc2-foundation (macOS only); app + serde, serde_json (now unconditional), dirs. |
| 2026-08-22 | — | M2 sidebar + splitters: `sidebar.rs` (`Sidebar` entity — Devices via live `watch_volumes` with eject affordance, Favorites from `AppSettings` with add-current-folder `+` / per-row `✕` persisted immediately, Explorer folder tree as a flat projection over `uniform_list`, collapsible section headers, `SidebarEvent::{NavigateTo, Eject}`); `workspace.rs` owns/renders `Entity<Sidebar>`, subscribes (NavigateTo → active pane, Eject → `Platform::eject` on the background spawner), and gains hand-built resizable splitters (drag strips over the sidebar/info-panel borders, widths clamped 160–400 / 180–420). Visual runner: `sidebar_tree_expanded` scenario + fixture settings file; all baselines need workflow regeneration. 83 tests green (39 fs-core + 44 app). |
| 2026-08-22 | — | M2 details-view in-place expansion + M1 column-alignment fix: `ExpandSelected`/`CollapseSelected` actions bound to `right`/`left` in `DirView && !renaming` (§0 M2 rows; dispatch tests extended); `dir_view.rs` gains `expanded`/cached children/flat projection (children sorted + hidden-filtered at projection time), left-on-child moves cursor to the parent row, collapse pulls a subtree-bound cursor up; `details_list.rs` renders depth indents + disclosure triangles and fixes body rows to `w_full` with `flex_none` fixed-width cells so columns align under the headers (Known-M1-gap closed); pane cursor retention consults injected rows. Visual runner: `details_folder_expanded` scenario; every listing baseline changes — regenerate via workflow. 88 tests green (39 fs-core + 49 app). |
| 2026-08-22 | — | M2 review fix: `Sidebar` now holds a `cx.observe_global::<AppSettings>` subscription — render reads the global for Favorites rows, and the boot-time background load (`settings::init`) swaps it in after first paint, so without the observer persisted favorites could stay invisible until an unrelated repaint (and visual baselines could race). Regression test added. 89 tests green (39 fs-core + 50 app). |
