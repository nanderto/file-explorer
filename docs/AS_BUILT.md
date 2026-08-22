# As Built

The living record of what is **actually implemented**, as opposed to what is planned
(`docs/file-explorer-plan.md`). Every PR that changes code must update this file —
the quality gate (`scripts/gate.sh`) and CI enforce it.

How to update: add or amend the relevant component section below, and add a row to the
change log. Record what exists, how it deviates from the plan (and why), and any known
limitations. Keep it truthful — this file is only useful if it matches the code.

## Status

**M3 in progress** — part 1 (the fs-core file-operations engine) is done; part 2
(the app UI surfaces) is the next workflow on this branch. 141 tests green
(88 fs-core unit + 3 integration + 50 app).

| Milestone | State |
|---|---|
| M0 skeleton + visual-test infra | ✅ merged (#1) |
| Phase A architecture | ✅ merged (#3) |
| M1 read-only browsing | ✅ merged (#4) |
| M2 sidebar + in-place expansion | ✅ merged (#5) |
| M3 file operations | 🔄 part 1 (fs-core engine) built; part 2 (UI) next |
| M4 icon view + dual pane → M8 ship | not started |

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
- Address bar (M1): breadcrumb rendered by the Pane (clickable segments;
  blank-space click or `cmd-l` enters editing); `AddressBar` entity owns the
  vendored `InputState`, background-listed autocomplete (dirs-only, prefix,
  generation-guarded), `tab` accept-in-place, background `Confirm` validation
  with inline errors, `escape` cancel; both outcomes return focus to the pane.
  `Theme` gained an `error` color (both palettes). The vendored input drives
  editing keys via `TextInput`-context bindings in `keymap.rs` forwarding to
  `input_state`-namespace actions; `set_value` replaces the whole content
  (VENDORED.md mods 5–7).

### fs-core
- Crate `crates/fs-core` (edition 2024, **no gpui dependency**, builds/tests
  headless on Windows). M1 subset per ARCHITECTURE.md §6/§10 plus the M2
  platform/persistence additions and the M3 ops/undo/clipboard modules below.
  Feature `test-support` exposes `FakeVfs`/`TestSpawner` to downstream crates;
  this crate's own tests see them via `cfg(any(test, feature))`, and the
  crate's integration tests get them via a self dev-dependency enabling the
  feature.
- `Vfs` M3 mutation surface (§6 names/signatures): `create_dir`
  (`create_dir_all` semantics so folder merges replay), `create_file(path,
  CreateOptions{overwrite})`, `copy(from, to, ProgressFn)` (single-file,
  chunked — 1 MiB chunks in `RealVfs`, 1 KiB in `FakeVfs`; the callback
  returns `bool`, `false` aborts between chunks, removes the partial
  destination, and fails with the typed `CopyCancelled` marker; the cleanup is
  scoped to failures *after* the destination was created — a failure before
  the first write (missing source, pre-copy cancel) never touches a
  pre-existing destination — and directories
  are expanded by op planning, never by `copy`), `rename(from, to,
  RenameOptions{overwrite})` (subtree move, mtimes preserved),
  `remove(path, RemoveOptions{recursive})` (missing path is an error),
  `trash(path) -> TrashId { original, trashed }`, and
  `restore(TrashId) -> Result<PathBuf, TrashRestoreError>` with the §6 typed
  variants. `RealVfs` implements everything via `SpawnerExt::unblock` and
  holds an in-memory consumed-token set (the `AlreadyRestored` double-undo
  guard). `FakeVfs` mirrors all mutations against its in-memory tree with
  watcher events, keeps rename mtimes, and gained a `snapshot()` helper for
  exact-tree undo assertions; per-path error injection covers the new methods.
- Trash mechanism: on macOS, `platform/macos.rs::trash_item_blocking` uses
  `NSFileManager trashItemAtURL:resultingItemURL:error:` (the resulting trash
  URL becomes `TrashId::trashed`); everywhere else
  `platform/trash.rs::fake_trash_blocking` moves the item into
  `<parent>/.fake-trash/<n>-<name>/<name>` with a sidecar meta file (original
  path + mtime fingerprint, per §6). Restore is shared
  (`platform/trash.rs::restore_blocking`): typed checks
  (NotFound/Collision), rename back, `.fake-trash` entry cleanup. All three
  `TrashRestoreError` variants are unit-tested on Windows through both
  `FakeVfs` and `RealVfs` (§9); the real-macOS trash path is compile-checked
  by macOS CI and exercised by the per-milestone Mac checklist.
- `ops/` (M3): `FileOp { Copy, Move, Rename, TrashOp, Restore, CreateDir,
  CreateFile, Duplicate, Delete }` — `Delete` (permanent removal, empty
  receipt so it is never undoable) is additive to §6's abbreviated list; it
  backs the §0 `DeletePermanently` row and copy-undo. `plan_keep_both_names
  (sources, dest_dir, existing) -> Vec<(src, final_dest)>` is the pure,
  unit-tested keep-both resolver (`"name copy.ext"`, `"name copy 2.ext"`, …;
  batch-internal reservations; dotfiles keep the whole name as stem).
  `ops/job.rs`: `JobId`, `JobKind`, `JobInfo`, `Conflict{source, dest,
  src_meta, dest_meta}`, `Resolution{choice: Replace|Skip|KeepBoth,
  apply_to_all}`, `OpReceipt{op, created, moved, trashed, restored}`, and the
  `JobEvent` enum
  (Started/Progress/NeedsDecision/Completed/Failed/Cancelled).
- `ops/queue.rs`: `JobQueue::new(vfs, spawner)` (deviation from §6's
  abbreviated `new(spawner)` sketch: execution needs the Vfs, which the
  sketch omits) — `submit(op) -> JobId` routes to one serial lane per
  **destination** volume (`volume_key(op.lane_path())`; lanes are spawned
  worker loops holding a `Weak` back-reference so a dropped queue ends
  them); `subscribe()` returns the single-consumer event receiver;
  `resolve(id, Resolution)` un-parks a conflict oneshot; `cancel(id)` trips
  an `AtomicBool` checked between files and, via the copy progress callback,
  between chunks (and wakes a parked conflict). Copy jobs plan first
  (same-folder paste and Duplicate get keep-both names at planning time —
  §4b; directory sources expand into parent-before-child actions with total
  bytes), then execute with runtime conflict parking (existing dirs merge
  silently; Skip prunes the subtree, runtime KeepBoth remaps descendant
  destinations; merged pre-existing top-level dirs are *not* recorded in
  `receipt.created`, so copy-undo can never delete pre-existing data). Move
  renames per source (same-folder move is a no-op; conflicts park like
  copy) with a copy-tree + remove fallback for cross-volume moves, cleaning
  the partial destination if cancelled mid-fallback. Copy and Move reject a
  destination inside (or equal to) one of their sources up front (`Failed`,
  nothing touched) — past the rename failure, the move fallback would
  otherwise copy the tree into itself and then recursively remove source
  *and* destination. CreateDir/CreateFile
  fail on pre-existing paths (undo safety). An RAII `JobTracker` guarantees
  exactly one terminal event per job even on panic. Non-copy jobs report
  item-count progress in the bytes fields.
- `undo.rs` (M3): `UndoEntry { inverse: Vec<FileOp>, redo: Vec<FileOp>,
  fingerprints }` built by `UndoEntry::from_receipt` (moves invert to
  `Rename` back-pairs in reverse order; created paths invert to `Delete`;
  trash inverts to `Restore`; restores invert to `TrashOp`; `Delete`
  receipts have no inverse). `UndoStack::undo/redo` validate fingerprints
  via `Vfs::metadata` (mismatch/missing → `UndoOutcome::Invalidated { entry,
  reason }` — the entry is skipped and handed back for the toast, never
  applied), then submit through the `JobQueue`; a successful apply pushes
  the inverted entry (fingerprints remapped through rename pairs, since
  renames preserve mtimes) onto the opposite stack; `push` truncates redo.
- `clipboard.rs` (M3): `FileClipboard { entries: Vec<EntryId>, mode:
  Copy|Cut }` plain struct — `is_cut(path)` for render dimming,
  `take_for_paste()` (cut empties after paste, copy pastes repeatedly), and
  `paste_op(dest_dir) -> Option<FileOp>` — the §4b handoff turning a paste
  into `FileOp::Copy` (copy-mode) or `FileOp::Move` (cut-mode, consuming);
  submitting the op reaches ops planning, where paste-into-same-folder
  keep-both names are resolved (unit-tested end-to-end through the
  `JobQueue` against `FakeVfs`).
- Integration tests `crates/fs-core/tests/torture.rs` (plan §7 M3 acceptance
  + the §9 fs-core-integration row): the torture sequence is one scripted
  function run twice — against `RealVfs` on a `tempfile` tree **and** against
  `FakeVfs` (the world is seeded and walked through the `Vfs` itself, so the
  script is implementation-generic) — copy a tree onto a destination with
  pre-seeded conflicts resolved **mixed** (keep-both a, skip b, replace c with
  apply-to-all — the pump asserts conflicts park in name order, that d is
  replaced *without* a fourth prompt, and that merged dirs never enter
  `receipt.created`), cancel a second copy mid-flight (parked-on-conflict via
  `JobQueue::cancel`, and between copy chunks via the progress callback —
  typed `CopyCancelled`, no partial file), move a **directory** then undo it,
  and delete-to-trash then undo (restore), both undone LIFO through one real
  `UndoStack`; the final assertion walks the entire tree and compares
  path-by-path, byte-by-byte (which also proves no partial/temp/`.fake-trash`
  leftovers survive anywhere). Plus: every `FileOp` variant end-to-end
  through the `JobQueue` against `RealVfs` (CreateDir/CreateFile/Duplicate/
  Copy/Move/Rename/TrashOp→Restore via the on-disk `.fake-trash`/Delete).
- Known M3-part-1 limitations (accepted, revisit if a milestone needs them):
  **symlinks** — Move/Rename/Trash relocate the link itself (rename-based);
  `copy` of a file symlink copies the *target's* bytes (dereferences), and a
  symlink-to-directory inside a copied tree fails the job (planned as a file,
  unopenable) — no link-preserving copy exists yet. **Fingerprints** are
  `(path, mtime)` per §6 — same-mtime content edits and changes deep inside a
  copied/created directory (which don't touch the top-level mtime) escape
  invalidation. **Failed jobs carry no receipt** (`JobEvent::Failed { id,
  error }`), so the completed part of a multi-source op that fails midway is
  not undoable — the watcher keeps listings truthful, and part 2's JobsModel
  owns any richer contract. **Names** pass through `to_string_lossy` (the M1
  `Arc<str>` naming decision), so keep-both planning on a non-Unicode filename
  would target the lossy spelling.
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

## Known gaps

Each entry names the milestone expected to resolve it. Mechanics live in the
component sections; this list is the scannable index.

- **Symlink copy policy** — copy dereferences file links; symlink-to-dir inside
  a copied tree fails the job (details under fs-core). *Revisit when a
  milestone needs link-preserving copy.*
- **Undo fingerprints are `(path, mtime)`, top-level only** — same-mtime or
  deep-in-tree edits escape invalidation (accepted per ARCHITECTURE §6). *M7+
  if ever.*
- **`JobEvent::Failed` carries no receipt** — the completed part of a midway-
  failed multi-source op is not undoable. *M3 part 2 (JobsModel contract).*
- **Undo/redo stacks are optimistic** — they flip before the inverse jobs
  complete. *M3 part 2 (JobsModel integration).*
- **Favorites reordering** deferred. *M3 part 2 (drag infrastructure).*
- **Settings boot-race merge semantics** (early mutation can suppress the disk
  load for a session). *M7 settings store.*
- **Sidebar/dir-view child caches never invalidate** (stale after ejects /
  external changes). *M3 part 2 watcher re-projection.*
- **Eject errors only logged**, no UI surfacing. *M5-ish polish / Mac checklist.*
- **Process**: baseline commits are pushed with `GITHUB_TOKEN`, which never
  triggers CI — after baselines land, push a normal commit so the required `CI`
  check runs on the new HEAD. *Standing procedure.*

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
| 2026-08-22 | #1,#2 | Bootstrap + M0: plan, CLAUDE.md, gate/hooks/CI, workspace, `WorkspaceView`, visual-test infra. |
| 2026-08-22 | #3 | Phase A: ARCHITECTURE.md (research→draft→judge workflow); gpui-component rejected; agent pack. |
| 2026-08-22 | #4 | M1: fs-core (listings/sort/watcher), app shell, details view, address bar, vendored TextInput. 68 tests. |
| 2026-08-22 | #5 | M2: Platform trait (volumes/eject), favorites persistence, sidebar + splitters, in-place expansion, M1 column fix. 89 tests. |
| 2026-08-22 | #5 | M2 review fix (sidebar observes `AppSettings`); objc2 constant-name CI fix; baselines regenerated. |
| 2026-08-22 | — | M3 part 1: Vfs mutation surface, ops/JobQueue (keep-both, conflict lanes, cancel), undo, clipboard, trash, torture test. 138 tests. |
| 2026-08-22 | — | M3 part 1 review: into-itself data-loss guard; copy-cleanup scoping; macOS dead_code fix; FakeVfs restore events. 141 tests. |
