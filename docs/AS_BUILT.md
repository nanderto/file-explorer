# As Built

The living record of what is **actually implemented**, as opposed to what is planned
(`docs/file-explorer-plan.md`). Every PR that changes code must update this file —
the quality gate (`scripts/gate.sh`) and CI enforce it.

**This file is the index.** It holds the status table, known gaps, deviations from the
plan, and the change log — the parts every reader needs. Per-crate component detail
lives in `docs/as-built/`, because this file is read in full by every agent on every
milestone and the detail had grown past 1,500 lines:

| Where | What |
|---|---|
| this file | status, known gaps, deviations, change log |
| [`docs/as-built/app.md`](as-built/app.md) | `crates/app` — every GPUI entity, view and widget |
| [`docs/as-built/fs-core.md`](as-built/fs-core.md) | `crates/fs-core` and `crates/theme` |

How to update: amend the relevant component section in `docs/as-built/`, **and** add a
row to the change log here (the gate requires this file to change alongside code).
Record what exists, how it deviates from the plan and why, and any known limitations.
Keep it truthful — these files are only useful if they match the code.

## Status

**M5 complete in code** — on top of M1–M4 (read-only browsing, the sidebar
and in-place expansion, the full file-operations engine: job spine, undo,
keyboard operations, inline rename, marquee, drag & drop and context menus,
and then the icon view and the second pane), M5 adds the right-hand column of
`docs/requirements/Basic window.png`:

* **Attributes in fs-core** — `attrs.rs` (`UnixPerms`, `FileAttrs`,
  `SelectionSummary`/`summarize`, the `is_previewable` allowlist + 64 MiB
  ceiling) and `Platform::file_attrs`, macOS-native behind the trait with a
  deterministic path-derived stub.
* **The info panel** — `info_panel.rs`, one `Subject` at a time (selected
  entry / open folder / multi-selection summary / nothing), a **single** load
  task carrying the debounce, the stat, the attribute lookup and the preview,
  so a retarget cancels all four and none of it touches the UI thread.
* **Whose selection** — the panel is workspace-level but follows the *active*
  pane, through one `cx.observe` per pane's `DirView` filtered by an O(1)
  witness.
* **`ToggleInfoPanel`** — `cmd-shift-i` and the titlebar `ⓘ`; a hidden panel
  stats nothing.

Permissions, owner and group are **read-only** at M5 (editing them is a
`chmod`/`chown` `FileOp` — M6). Three new visual scenarios pin the panel:
`info_panel_jpeg` (§8's named M5 row and the milestone's acceptance
criterion), `info_panel_selection` and `info_panel_multi_selection`.

M4 remains as built below — the second view and the second pane:

* **Thumbnails in fs-core** — `Platform::thumbnail` (QuickLook on macOS behind
  the `Platform` trait, with a deterministic stub elsewhere) feeding an
  LRU byte-budget `ThumbnailCache` keyed on path + pixel size + content stamp.
* **Icon view** — `views/icon_grid.rs`, a `uniform_list` whose items are grid
  *lines*, with 2D keyboard navigation by index arithmetic, plus the two-button
  view-mode switcher (`cmd-1` / `cmd-2`) on the pane.
* **Dual pane** — `ToggleSplitPane` (`cmd-shift-o` and a titlebar button),
  two fully independent `Pane` entities with a draggable splitter between them.
* **Grid thumbnails** — requested for the visible band plus a line of margin,
  off a window derived from the scroll offset and viewport (never from
  `uniform_list`'s row range, which gpui also calls with `0..1` to measure),
  cancelled when they scroll away, and pruned by full cache key.
* **Auto-hide scrollbar** — an overlay, not a layout node, faded on a
  `Spawner` timer so no captured baseline depends on wall-clock time.

Three adversarial reviews of the M5 lanes have been applied; see the M5 rows
of the change log for what they found (a coin-flip test failure off macOS, an
O(listing) projection built on every idle notify, a panel that blanked itself
while the open folder was busy, and two baselines that would have captured the
panel mid-load). 398 tests green (132 fs-core unit + 4 integration + 262 app),
and 17 visual scenarios.

| Milestone | State |
|---|---|
| M0 skeleton + visual-test infra | ✅ merged (#1) |
| Phase A architecture | ✅ merged (#3) |
| M1 read-only browsing | ✅ merged (#4) |
| M2 sidebar + in-place expansion | ✅ merged (#5) |
| M3 file operations | ✅ engine + job spine + keyboard ops + inline rename/duplicate + marquee + drag & drop + context menus + review fixes; all baselines committed |
| M4 icon view + dual pane | ✅ complete — fs-core thumbnails, icon view + view-mode switcher, dual pane, grid thumbnails (visible+margin, cancel-on-scroll-away), the auto-hide scrollbar, and the narrow-pane column fit. All 14 visual baselines regenerated on the macOS runner (the titlebar's split-pane button changed every existing scenario) |
| M5 info panel | ✅ code complete — `fs-core::attrs` (`UnixPerms`, `FileAttrs`, `SelectionSummary`, `is_previewable`) + `Platform::file_attrs`; `crates/app/src/info_panel.rs` with the debounced single-slot load, the preview, the General and Permissions sections and the multi-selection summary; `ToggleInfoPanel` (`cmd-shift-i` + titlebar button); three new visual scenarios (`info_panel_jpeg`, `info_panel_selection`, `info_panel_multi_selection`). **All 17 visual baselines need regenerating** — see Known gaps |
| M6 → M8 ship | not started |

## Components

### Workspace / build tooling
- Quality gate (`scripts/gate.sh`), Claude Code hooks (`.claude/settings.json`),
  CI (`.github/workflows/ci.yml`), branch protection script
  (`scripts/setup-branch-protection.sh`).
- Cargo workspace (edition 2024, toolchain pinned to 1.97.1) with `crates/app`.
  gpui + gpui_platform pinned to zed rev `fd82517a` (needed for
  `VisualTestAppContext`).

### `crates/app` (GPUI)

Detail: **[docs/as-built/app.md](as-built/app.md)** — workspace, panes, dir view,
details list and icon grid, selection, rename, drag & drop, marquee, context
menus, dialogs, jobs UI, thumbnails, scrollbar, sidebar, address bar, settings.

### `crates/fs-core` and `crates/theme`

Detail: **[docs/as-built/fs-core.md](as-built/fs-core.md)** — vfs, listing, sort,
watcher, exec/spawner, ops and the job queue, undo, clipboard, thumbnails and
cache, the `Platform` trait with its macOS and stub impls.

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

- **Every committed visual baseline needs regenerating for M5, not just the
  three new info-panel scenarios.** The fourteen committed baselines are all
  stale, and `info_panel_jpeg`, `info_panel_selection` and
  `info_panel_multi_selection` have no baseline at all — a scenario with no
  committed baseline **hard-fails** the macOS visual job, so the run is
  all-or-nothing. The info panel is painted in every
  scenario, and M5 replaced its "No selection" placeholder with the real
  entity; the titlebar also gained the `ⓘ` toggle beside the split button. The
  visual runner additionally advances the deterministic clock past
  `info_panel::LOAD_DEBOUNCE` after each navigation **and again after each
  scenario's own gesture** (every scenario but `MarqueeActive`, whose held drag
  would autoscroll), so the panel's values are part of the baseline instead of
  a race — that advance is shorter than the scrollbar's 900 ms fade and the
  drop target's 500 ms spring-load, so it cannot fire either, but it is a
  change every baseline sees. `run_scenario` now *asserts*
  `InfoPanel::is_settled()` before it captures, so a future scenario cannot
  quietly bake a half-loaded panel into a baseline again. Run
  `gh workflow run update-visual-baselines.yml --ref m5-info-panel`, then
  **open the seventeen PNGs and look at them** (definition of done item 7) —
  in particular the three info-panel ones, where a mid-load capture would show
  em dashes where the size, dates and permissions belong.
  *This PR.*
- **Every committed visual baseline needed regenerating for M4** — done: all
  fourteen were regenerated on the macOS runner, `icon_grid` and `split_panes`
  included, so every declared M4 scenario has a committed baseline. (The code
  ships the scenario as `split_panes`; ARCHITECTURE.md §8 was amended to match,
  rather than the scenario renamed.) *Closed (M4).*
- **Thumbnails are requested for every non-folder in the window, one at a
  time.** The only filter in `thumbnails.rs::pending_thumbnails` is
  `!is_dir_like()` — no extension/UTI allowlist and no size ceiling — so an
  icon-view pass over `/usr/lib` or a `target/` directory asks QuickLook for a
  preview of every object file. And because the single-slot task awaits one key
  at a time, one pathological file head-of-line-blocks every later tile in the
  window for up to `MacPlatform`'s 10 s QuickLook timeout plus the
  `image`-crate fallback attempt, leaving the tiles after it on their type
  glyphs. Bounded and correct, just slower than it should be: the fix is a
  previewable-type allowlist plus a size ceiling before requesting, and a small
  amount of concurrency. **Half closed at M5**: `fs_core::is_previewable` (the
  extension allowlist + 64 MiB ceiling) exists and the info panel's preview
  goes through it, but `thumbnails.rs::pending_thumbnails` has *not* been
  changed to use it — doing so would change what every icon-grid tile paints
  and so every icon-grid baseline, which M5 deliberately kept out of its own
  baseline churn. *M6.* Related and deliberate: a cancelled request is *not*
  interrupted — `Spawner::unblock` polls the blocking closure exactly once, so
  a QuickLook wait already handed to the background queue runs to completion and
  has its result discarded (one orphan per cancellation; documented on
  `Platform::thumbnail` and in the `thumbnails.rs` module docs). A real cancel
  token through `unblock` would let the macOS path call `cancelRequest`
  immediately. *M5 (info panel previews), which needs the same allowlist.*
- **The open context menu's submenu is not pixel-pinned.** `context_menu_open`
  captures the background menu with no submenu expanded: opening one from the
  runner means computing a row centre from `context_menu.rs`'s private
  geometry constants, which would duplicate them in the runner and pin the
  scenario to them. The submenu's `anchored()`-less fit (see the right-edge gap
  below) is therefore still behavioral-tests-only. *Revisit when the submenu
  gets its own flip logic and needs a baseline anyway.*
- **A submenu can run off the right edge of the window.** The outer context-menu
  panel is `anchored()`, so it flips its anchor corner rather than overflow, but
  a submenu is positioned by ordinary layout inside that panel and gets no such
  fit: opening a menu within ~360px of the right edge puts `New ▸`/`Sort by ▸`
  partly off-screen. Fix is to flip the submenu to the parent's left side when
  it would not fit, which needs the panel's painted bounds. *M4-ish polish.*
- **Only the details view has a context menu.** The sidebar (volumes,
  favorites, tree rows) and the breadcrumb have none, so Explorer's
  "Eject" / "Remove from Favorites" / "Open in new pane" live nowhere yet.
  The trigger + panel are `DirView`-shaped (they hang off its list surface and
  its uniform row band); giving another view a menu means lifting
  `ContextMenuState` out of `DirView` or repeating the small wiring. *M4/M5,
  with dual panes.*
- **Context menus have no keyboard navigation.** `escape` closes and every row
  is clickable, but arrows/enter do not walk the rows, and while a menu is open
  the `DirView` bindings stay live (the `menu` token is additive, not a guard
  like `!renaming`), so `cmd-c` still copies behind an open menu. *M8
  accessibility polish, with the native menu bar.*
- **`Sort by ▸` has no direction rows; the active column is inert instead.**
  `SortBy { key }` means "sort by this column, flipping direction if it is
  already the one" — right for a header click, wrong for a menu whose only
  feedback is a stationary ✓, where re-picking the checked row silently
  reversed the listing. The checked row is now rendered **disabled**, so the
  menu can pick a column but not a direction (Explorer's submenu carries
  explicit Ascending / Descending rows). The fix is the same one the Paste gap
  above needs: a parameterized action (`SortBy { key, direction: Option<_> }`)
  so the single handler still owns the logic. *M8 menu bar, with the Paste
  parameterization.*
- **`NavEntry.scroll_top` is only ever written by tests**, so back/forward and
  refresh restore *cursor* but always scroll to the top: `Pane::set_scroll_top`
  has no production caller, and nothing samples the list's live offset when a
  history entry is captured. Discovered while fixing the watcher patch that was
  re-applying it (a patch no longer restores scroll at all, so external changes
  leave the list where the user left it). Fixing the restore properly means
  reading the scroll handle at capture time, which changes what several pane
  tests assert. *Still open after the M4 icon view: the grid scrolls by
  `ix / cols` items, which is the same handle and the same missing capture.*
- ~~**Expansion state is never pruned when a folder leaves the listing**~~
  **resolved (2026-08-23)**: `DirView::prune_expansion_state`, called from the
  same `retain_selection_in_listing` pass that prunes the selection, drops
  `expanded`, `children` **and any in-flight `_child_loads`** for folders no
  longer in the listing. A key survives iff some ancestor of it is still a row,
  so a collapsed folder's cached children live on while it is listed and go
  with it when it does not. Two bugs rather than mere untidiness: the maps grew
  for the life of the pane (a `children` entry is a whole child listing), and a
  folder deleted and later re-created with the same name came back
  **pre-expanded from a stale cache**.
- ~~**The row menu's Paste pastes into the pane's directory, not into the
  right-clicked folder**~~ **resolved (2026-08-23)**: the action was
  parameterized as predicted — `Paste { dest: Option<PathBuf> }`, `None`
  meaning the open directory — so the row menu pastes into the folder under the
  pointer with the same single handler. The right-clicked *entry* decides, not
  the selection: a right-click on a folder inside a wider selection is still a
  right-click on that folder. `Sort by ▸`'s direction rows still wait on M8;
  they need `SortBy { key, direction }`, the same shape.
- **Marquee edge autoscroll is rarely reachable in the details view.** Rows
  are full-width (Explorer's own details-view behavior), so the only empty
  space a marquee may start in is *below the last row* — which exists only
  when the listing is shorter than the viewport, i.e. when there is nothing to
  scroll. Autoscroll therefore only engages when the projection grows
  **mid-gesture** (an expanded folder's children landing — the path the
  `#[gpui::test]` drives — or a watcher patch) or the window shrinks under the
  drag. It is fully implemented per §8 and now pays off in the M4 icon grid,
  whose empty space *is* plentiful (a ragged last row leaves space to the right
  of its last tile, and `index_at_content` correctly calls that empty), so the
  gesture is routinely reachable there; giving details rows Explorer's narrower
  (columns-only) hit region would make it routinely reachable in the list too.
  Deliberately **not** done at M4: narrowing the row's hit region changes what
  a click, a drag start and a right-click land on in the list, i.e. the three
  gestures M3 spent its review fixes on, for a gain the grid already delivers.
  *Details-list half only; M7 polish, with a test matrix for the new dead
  zone.*
- **Finder interop can only be proved on a real Mac.** The *inbound* half is
  covered headlessly (gpui turns a platform file drop into an internal
  `ExternalPaths` drag, so `FileDropEvent`s drive the real code path), but the
  *outbound* half — gpui promoting our drag to an AppKit dragging session when
  the pointer leaves the window and handing over the
  `ExternalDragPayload::Files` we resolve — has no headless observation point
  (gpui's `TestWindow::external_drag_files` is `pub(crate)`). The payload
  builder is unit-tested; the handoff itself is the manual per-milestone Mac
  checklist's job. *Standing: §9 "Manual per-milestone on real Mac".*
- **One `WatchGuard` drop can still land on the UI thread: the pane's own.**
  Registration and every *replacement* unregistration go through the background
  executor (`BackgroundWatchGuard` hands the guard to `BackgroundExecutor` from
  its `Drop`), so navigation never blocks the render thread. A pane being
  dropped runs that same `Drop`, which spawns rather than blocks — but if the
  app is tearing down, the spawned unwatch may never be polled and `notify`'s
  own `Drop` finishes the job wherever it lands. One `unwatch` at teardown,
  not one per navigation. *Accepted; revisit only if teardown ever stalls.*
- **Whether the cross-volume copy rule holds on a real Mac is untested here.**
  `drop_copies` derives "same volume" from `Vfs::volume_key`, which is
  **path-shaped** (`/Volumes/<name>`, drive prefix on Windows) rather than read
  from the filesystem — so a bind mount, a disk image mounted somewhere other
  than `/Volumes`, or a network share reached by an unusual path reads as the
  root volume, and a plain drag off it would *move*. Real volume identity is a
  `Platform` concern. *Revisit with M5's volume work; the Mac checklist should
  spot-check a USB stick and an SMB share.*
- **An external (Finder) drop always copies, never moves.** gpui's file-drop
  translation carries no modifiers (`Modifiers::default()` on every synthesized
  event), so there is no signal to distinguish a move; copying is the
  non-destructive choice for another app's files. *Revisit only if gpui starts
  reporting drop modifiers.*
- **Sidebar drop targets are Favorites-only.** Dropping files on a *tree* row
  or a volume row does nothing, and dropping a folder on the Favorites list
  pins it rather than moving it inside that favorite (the sidebar means "pin a
  place" here). Explorer moves into nav-pane folders; wiring the tree rows to
  the same `plan_drop` is a small follow-up. *Still open after M4's cross-pane
  drag, which needed no sidebar work.*
- **The info panel's permission grid, octal field, owner and group are
  read-only.** They render as disabled controls (no click handlers at all, so
  nothing looks live and silently does nothing), because making them editable
  is a `chmod`/`chown` surface on the `Platform` trait plus an undoable
  `FileOp`, not a rendering change. *M6.*
- **The info panel does not show a folder's recursive size.** A folder subject
  shows its own inode size, which on macOS is a few kilobytes and on `FakeVfs`
  is zero, and a multi-selection's total sums the **files** only
  (`fs_core::summarize` documents this). Recursive sizing is a cancellable job,
  not a stat. *M6/M7 polish.*
- **The info panel shows no tags and no "Open with".** The blueprint
  screenshot's three titlebar glyphs above the panel (info / history /
  warnings) are also not built — the panel has one mode. *M6 (tags) / M7
  (chrome).*
- **The panel is taller than the window, so its last row is off every
  baseline.** Verified by opening the local renders (inspection only): at the
  fixed 1200×760 capture size a fully expanded single-entry panel runs past the
  bottom edge — `info_panel_jpeg` pins everything down to the owner/group
  dropdowns, and the "Locked" row's label is at the very bottom edge with its
  checkbox clipped away entirely. The column is
  `overflow_y_scroll`, so it is reachable in the app — but no baseline proves
  the bottom of it renders, and no baseline pins the panel with its sections
  *collapsed*, hidden, or in the light theme (`workspace_light` has no folder
  open, so it only shows the empty state). Cheap to add when one of those
  states next changes; not worth three more full-window captures now.
  *M7 (theme scenarios) or the next info-panel change.*
- **The info panel has no preview cache: re-selecting a file re-runs
  QuickLook.** `spawn_load` asks `Platform::thumbnail` on every retarget and
  keeps the result only in the live `preview` slot, so clicking A, B, A, B costs
  four generations at 400 px — where the icon grid goes through fs-core's
  byte-budgeted `ThumbnailCache`. Routing the panel through the same cache (its
  key already carries the entry stamp, so a rewritten file still misses) or
  keeping a one-entry path+stamp slot would fix it. *M6.*
- **A file being written *while it is selected* stops refreshing until the
  churn stops.** A watcher patch arrives every `pane::WATCH_LATENCY` (100 ms),
  which is shorter than `info_panel::LOAD_DEBOUNCE` (130 ms), so the debounce is
  restarted before it can fire. The panel keeps the values it already has
  painted (it no longer blanks itself — see the M5 review row in the change
  log) and refreshes the moment the folder goes quiet, but a live download's
  size does not tick up in the panel. Coalescing — re-reading only when the
  selected entry's own size/mtime moved — needs the witness to carry that
  stamp. *M6.*
- **A re-listed *child* of an in-place-expanded folder does not re-read the
  panel.** The `Witness` sees the pane's own snapshot `Arc` and
  `expansion_state_sizes()`; `DirView::reload_children` replaces a child `Vec`
  in place, changing neither. So a selected row *inside* an expanded subfolder
  keeps its old size, mode and preview after that subfolder is re-listed. The
  fix is a monotonic child-listing generation in the witness. *M6.*
- **`MacPlatform::file_attrs` has no timeout**, unlike the QuickLook path
  beside it: `symlink_metadata`, `NSFileManager attributesOfItemAtPath:` and
  `NSURL resourceValuesForKeys:` all block unbounded, so arrowing down a
  listing on a hung network mount parks one background-pool thread per row.
  QuickLook can be bounded because it is a completion-handler API that can be
  cancelled; bounding a `stat` means leaking a thread per stalled call instead
  of parking one, which is a deliberate M5 non-choice, documented on
  `file_attrs_blocking`. *M6/M7.*
- **The previewable allowlist and the icon grid still disagree.**
  `fs_core::is_previewable` covers images, PDF, text/source, media, office,
  iWork, camera raw and `psd`/`ai`/`eps`; anything else (`.sketch`, `.pxd`, a
  new format QuickLook learns about) previews as a tile in the icon grid — which
  filters nothing — and shows the placeholder glyph in the panel. One list, used
  by both, is the fix. *M6, with the `pending_thumbnails` change below.*
- **Dual pane has no keyboard way to switch panes and no pane-swap.** The
  active pane follows *focus*, so it changes by clicking (or by the split
  itself); there is no `tab`/`ctrl-tab` "focus the other pane" binding and no
  "swap the two panes" or "open the other pane's folder here" command. §0 lists
  none of them, so adding one means adding a table row first. *M7/M8 chrome.*
- **Neither the split nor the splitter widths are persisted.** `panes.len()`,
  `first_pane_width`, `sidebar_width` and `info_panel_width` are plain
  `Workspace` fields, so every launch starts single-pane at the default widths.
  `settings.rs` is the place for it. *M7 (settings).*
- **The info panel is workspace-level, not per-pane** — resolved at M5 by
  making it follow the **active** pane: the workspace observes each pane's
  `DirView` and pushes the active one down through `InfoPanel::follow`, so
  `PaneEvent::FocusIn` retargets the panel exactly as it retargets `cmd-z`.
  There is deliberately no second panel and no per-pane panel. *Closed (M5).*
- **A new entry is not created if the naming editor is abandoned** — the §4c
  design's consequence, and a deliberate divergence from Explorer, which
  creates `"New folder"` immediately and leaves it named if you press Escape.
  Here `Confirm` owns the `CreateDir`/`CreateFile`, so Escape (or navigating
  away, or clicking elsewhere, which blurs) leaves the directory untouched.
  *Accepted; revisit only if the "it vanished" reading turns out to surprise
  people.*
- **`UPDATE_BASELINE=1` rewrites every scenario, so unrelated PNGs churn by a
  byte or two per regeneration** — the M3 rename run rewrote 6 baselines with
  0–1 byte deltas (text-heavy scenarios only; `workspace_dark`/`light` were
  byte-identical), consistent with macOS text-rasterization noise between
  runner instances, which is what the ≥99% / tolerance-3 comparison exists to
  absorb. Harmless but it makes baseline commits noisy to review. *Revisit if
  a regeneration ever flips a comparison result: write only changed files.*

- **Symlink copy policy** — copy dereferences file links; symlink-to-dir inside
  a copied tree fails the job (details under fs-core). *Revisit when a
  milestone needs link-preserving copy.*
- **Undo fingerprints are `(path, mtime)`, top-level only** — same-mtime or
  deep-in-tree edits escape invalidation (accepted per ARCHITECTURE §6). *M7+
  if ever.*
- **`JobEvent::Failed` carries no receipt** — the completed part of a midway-
  failed multi-source op is not undoable; the JobsModel contract is an error
  toast + no undo entry. *Accepted; revisit if partial-undo is ever required.*
- **Undo/redo stacks are optimistic** — they flip before the inverse jobs
  complete (self-feeding is prevented via JobsModel suppression, but a failed
  inverse job leaves the stacks flipped). *Accepted; revisit with M8 polish.*
- ~~**Favorites drag interactions are tested at the method level**~~
  **resolved (2026-08-23)**: both gestures now run through real simulated mouse
  input against `debug_bounds("sidebar-favorite-{ix}")` /
  `debug_bounds("sidebar-favorites-drop-zone")`, so the production wiring
  (payload type, the row's insert-before target, the section's `on_drop`) is
  covered rather than just the methods it calls. No arithmetic was needed after
  all — the rows already carried `debug_selector`s. The method-level tests stay
  as the no-op/ordering matrix.
- **Settings boot-race merge semantics** (early mutation can suppress the disk
  load for a session). *M7 settings store.*
- **Child-cache invalidation reaches only what the watcher reports.** The
  details view's expansion children and the sidebar tree are invalidated from
  the active pane's watch, which is **non-recursive on the open directory**:
  changes deeper than a direct child are only reported by backends that
  deliver descendants (FakeVfs does, so the behavior is tested; macOS FSEvents
  filtering means a change two levels down may not arrive), and directories no
  pane has open are not watched at all — an ejected volume's cached tree
  children still linger. *Revisit if M4+ needs per-node watches.*
- **The open directory being deleted externally is not handled**: its removal
  is an event in the *parent*, which nothing watches, so the pane keeps
  painting its last listing (Explorer walks up to the nearest surviving
  ancestor). *M4-ish polish.*
- **Eject errors only logged**, no UI surfacing. *M5-ish polish / Mac checklist.*
- **Process (RESOLVED)**: baseline commits are pushed with `GITHUB_TOKEN`,
  which GitHub excludes from triggering workflows, so `pull_request` never
  fired and the required `CI` check never ran on the baseline commit — the PR
  sat at "no checks reported", unmergeable, with nothing visibly wrong (it
  blocked M4 twice). `update-visual-baselines.yml` now dispatches `ci.yml` on
  the commit it pushes, and `ci.yml` accepts `workflow_dispatch` with the
  docs/tests job still running (the aggregate `ci` job treats a skipped job as
  passing, so letting it skip would have quietly weakened the gate).

- ~~**No auto-hide scrollbar**~~ **resolved (2026-08-23)**: `scrollbar.rs`, a
  thin theme-colored overlay on a `Spawner::timer` fade. It is an
  **indicator, not a control**: it cannot be dragged (no jump-to-position, no
  page-on-track-click), and there is no horizontal bar because neither view
  scrolls horizontally. *Dragging it: M7 chrome.*
- **The scrollbar has no visual scenario.** It is unit-tested (thumb geometry)
  and behavior-tested (appears on a scroll, fades on fake time), but no
  captured frame pins how it looks, because every fixture folder fits in one
  viewport and pinning it means adding a deliberately tall fixture tree. Cheap
  to add with one; nothing else needs it yet. *M7 chrome, with the settings
  work that adds more scenarios anyway.*
- ~~**Icon-grid tiles paint a type placeholder, not a thumbnail**~~
  **resolved (2026-08-23)**: see `thumbnails.rs`. What is deliberately *not*
  there:
  - **Thumbnails are per-pane, and per-`DirView`.** Two panes showing the same
    folder decode the same previews twice, and navigating away and back
    re-fetches (the cache lives on the view, and a `DirView` outlives
    navigation but nothing shares it). A `FsContext`-level cache would fix both
    and needs a decision about whose byte budget it is. *M7.*
  - **No cross-tile request dedup.** The single-slot task means one fetch at a
    time per pane, so a window is filled serially; a slow first preview delays
    the rest of its window rather than being overtaken. Correct and bounded,
    but not parallel. *Revisit only if a real folder of large images feels
    slow on a Mac.*
  - **Only the icon grid has previews.** The details list still shows no
    thumbnail column and no per-row icon; Explorer's details view shows a small
    one. *M7 polish.*
  - **The tile requests at 2× `ICON_PX` unconditionally**, rather than reading
    the window's real scale factor — sharp on a Retina display, slightly
    over-fetched on a 1× one. *Accepted.*
- **Tile labels are single-line and truncated**; Explorer wraps an icon-view
  label to two lines and shows the full name for the selected tile. Wrapping
  would make tile height depend on the label, which the fixed-height
  `uniform_list` item (and every hit test derived from it) forbids — so this
  needs a second, taller tile variant rather than a flexible one. *M7 polish.*

## Deviations from the plan

- Theme lives as a module in `crates/app` instead of its own crate until the
  JSON theme system lands (M7); the plan's crate split is deferred to avoid a
  near-empty crate.
- Visual regression testing (not explicitly in the plan's testing section) was
  added ahead of M1, modeled on Zed's visual test runner.
- **Drag modifiers are ⌥ for copy and ⇧ for move**, not Explorer's ctrl/⇧ pair:
  `platform` (⌘) is the multi-select toggle on row clicks and `control` is
  macOS's context-menu chord, so neither is available, and ⌥-drag is the native
  copy-drag gesture on this platform. The *defaults* those override are
  Explorer's exactly (move within a volume, copy across volumes).
- **Menu rows do not carry keyboard-shortcut hints**, and the `Sort by ▸`
  submenu has no Ascending/Descending rows (see Known gaps) — both wait on the
  M8 menu bar forcing the action set to be parameterized.
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
| 2026-08-24 | — | M5 review fixes (three adversarial reviewers, 19 findings). **Blocker:** `crates/fs-core/tests/attrs.rs` asserted `!attrs.locked` over `tempfile`-random paths, and `StubPlatform` derives `locked` from a hash of the path — a ~40% coin-flip failure on Windows/Linux, invisible on macOS CI. Now asserts the flag is *stable per path* instead, with the hazard pinned by `stub_file_attrs_flags_are_path_derived_not_constant` and documented on `StubPlatform::file_attrs`. **Major:** `InfoPanel::follow` derived the subject *before* comparing the O(1) witness, so every idle `DirView` notify (a scroll frame, an arriving thumbnail, a 30 ms marquee tick) built the whole flat projection on the UI thread — the exact cost the witness exists to avoid; witness first now, pinned by `an_idle_follow_does_not_build_the_projection` over a thread-local `dir_view::projections_built()` probe. **Major:** every watcher patch retargeted the panel, which cleared `details` and the preview and restarted the 130 ms debounce — and patches arrive every 100 ms, so any file activity in the open folder left the panel permanently at em dashes with no preview; a re-read of the *same* subject now keeps what is painted and swaps the new values in (`repeated_relistings_keep_the_values_painted`), dropping a stale preview only if the subject stopped being previewable. **Major:** `settle_info_panel` ran only inside the runner's `navigate`, so `details_rename_editing` *and* `split_panes` captured the panel mid-load; the runner now settles after the gesture too (all setups but `MarqueeActive`, whose held drag would autoscroll) and `run_scenario` asserts `InfoPanel::is_settled()` before capturing — verified by running the runner locally for inspection with the settle disabled, which fails exactly those two. Minors fixed: a folder's General "Size" row is an em dash, matching the details list's Size column, instead of the directory's inode size; `general_rows`/`header_text`/`perm_matrix` extracted as pure functions and unit-tested (four tests) so the Path rule, the em-dash-before-load rule and the R/W/X matrix are covered by something other than a baseline; Owner and Group render as disabled dropdowns (`stub-owner ⌄`) as the blueprint shows, matching the octal field's shape; `format_size(1)` is "1 byte"; camera raw + `psd`/`ai`/`eps` added to the previewable allowlist; `MacPlatform::file_attrs` returns lstat-only attributes for a non-UTF-8 path rather than querying a lossily-converted one; the macOS `added` assertion is now bounded by the write window and `system_time_from` has its own test over both sides of the epoch and non-finite intervals; the "same gate the icon grid uses" comments corrected (the grid filters nothing). Rejected: the claim that nothing is clipped at 1200×760 — the local renders show "Locked"'s checkbox clipped, so that gap stands. Deferred as Known gaps: the preview cache, coalescing a re-read of a file being written, the child-listing generation in the witness, the unbounded macOS `stat`, and the allowlist/icon-grid disagreement. 398 tests (132 fs-core unit + 4 integration + 262 app). |
| 2026-08-24 | — | M5 info panel, scenarios + docs lane: `info_panel_jpeg` (§8's named M5 row and the plan's acceptance criterion — a selected `.jpg`, so the "JPEG image" type description, the `jpg` extension row and a painted preview are all pinned on the file kind the blueprint shows) and `info_panel_multi_selection` (a new `Setup` variant; the summary state nothing else pinned, with a folder in the selection so Folders/Files/Total size are all non-trivial). `info_panel_jpeg` reuses the existing `Setup::InfoPanelSelection` — the driving is identical and a second variant would only have duplicated it. Both wait out `info_panel::LOAD_DEBOUNCE` before capture, so neither frame is a mid-load race. The JPEG subject is `/home/Pictures/photo.jpg`, added with `FakeVfs::insert_file` **after** `insert_tree` rather than as a fixture key: `FakeVfs` hands out mtimes from a counter in insertion order, so a new key inside the tree would have shifted the mtime of every node inserted after it, and appending shifts nothing. It lives in `Pictures` (empty until now, and listed by no scenario) so no other scenario's rows change; the size is stated outright (24,576 B → "24.0 KB") instead of being a literal body's length. Baseline impact: all 14 committed baselines are stale for M5 anyway, and these are two of the three with no baseline at all. Docs: this file's intro and status row, the info-panel component sections in `as-built/app.md` and `as-built/fs-core.md`, the M5 known gaps opened and closed, and `ARCHITECTURE.md` §8's scenario list (also repairing a hard line break that had broken the table row). No new unit tests — a visual scenario *is* the test; counts unchanged at 390. |
| 2026-08-24 | — | M5 info panel, app lane (`crates/app/src/info_panel.rs`): the `InfoPanel` entity replaces the M0 "No selection" placeholder. One path-keyed `Subject` at a time (selected entry / open folder / `SelectionSummary` / nothing), a **single-slot** load task carrying the `Spawner::timer` debounce, the `Vfs::metadata` stat, the `Platform::file_attrs` lookup and the `Platform::thumbnail` preview — so one retarget cancels all four — with every one of them awaited on the background executor. Preview gated by `fs_core::is_previewable` and reusing `thumbnails::render_image` for the RGBA→BGRA conversion. Sections match `docs/requirements/Basic window.png`: preview, name + "&lt;type&gt; — &lt;size&gt;", collapsible **General** (path, human+exact size, modified/created/added, extension, hide-extension, hidden) and collapsible **Permissions** (R/W/X grid, symbolic + octal, owner, group, locked) — **read-only**, no click handlers. Workspace-level but follows the **active** pane, via one `cx.observe` per pane's `DirView` (there is no `SelectionChanged` event; the change is a notify) filtered by an O(1) `Witness` so a scroll or an arriving thumbnail costs nothing. `ToggleInfoPanel` (`cmd-shift-i` + titlebar `ⓘ` dispatching the same boxed action); a hidden panel is told to `clear()` and stats nothing. New `info_panel_selection` visual scenario. 390 tests (130 fs-core unit + 4 integration + 256 app); 17 of them new here — 16 in `info_panel.rs` plus the titlebar-toggle guard in `workspace.rs`. |
| 2026-08-24 | — | M5 info panel, fs-core lane: new `crates/fs-core/src/attrs.rs` — `UnixPerms` (octal/symbolic/`allows`, `ls`-style special bits), `FileAttrs` (perms, owner, group, locked, Date Added, extension-hidden, localized type description), `SelectionSummary` + `summarize`, and the previewable-type gate `is_previewable`/`is_previewable_entry` (extension allowlist + 64 MiB ceiling). New `Platform::file_attrs`, implemented on macOS from one `lstat` (`std::os::macos::fs::MetadataExt`, `UF_IMMUTABLE`), `NSFileManager attributesOfItemAtPath:` for owner/group *names* and one `NSURL resourceValuesForKeys:` for added/extension-hidden/type-description, all inside a single `SpawnerExt::unblock`; each richer lookup degrades to `None`/`false` rather than failing the call. Deliberately `lstat`, not `stat`: the panel describes the selected item, so a symlink reports its own mode. Deterministic path-derived stub. `ARCHITECTURE.md` §6 sketches `perms` as a `FileEntry` field; attributes are instead fetched per-selection through `file_attrs`, which is what §9's M5 line describes, and which keeps `FileEntry` (and M1–M4) untouched. |
| 2026-08-22 | #1,#2 | Bootstrap + M0: plan, CLAUDE.md, gate/hooks/CI, workspace, `WorkspaceView`, visual-test infra. |
| 2026-08-22 | #3 | Phase A: ARCHITECTURE.md (research→draft→judge workflow); gpui-component rejected; agent pack. |
| 2026-08-22 | #4 | M1: fs-core (listings/sort/watcher), app shell, details view, address bar, vendored TextInput. 68 tests. |
| 2026-08-22 | #5 | M2: Platform trait (volumes/eject), favorites persistence, sidebar + splitters, in-place expansion, M1 column fix. 89 tests. |
| 2026-08-22 | #5 | M2 review fix (sidebar observes `AppSettings`); objc2 constant-name CI fix; baselines regenerated. |
| 2026-08-22 | — | M3 part 1: Vfs mutation surface, ops/JobQueue (keep-both, conflict lanes, cancel), undo, clipboard, trash, torture test. 138 tests. |
| 2026-08-22 | — | M3 part 1 review: into-itself data-loss guard; copy-cleanup scoping; macOS dead_code fix; FakeVfs restore events. 141 tests. |
| 2026-08-22 | — | M3 job spine: JobsModel bridge, conflict/confirm dialogs, progress popover, toasts, undo/redo wiring, conflict_dialog scenario. 157 tests. |
| 2026-08-22 | — | M3 keyboard ops: SelectionModel, cut/copy/paste (+dimming), delete-to-trash, new folder/file; 4 end-to-end behavior tests. 171 tests. |
| 2026-08-23 | — | M3 inline rename (`rename.rs`: `f2` + slow-second-click, stem preselect, inline errors, `JobsEvent::Failed`) + `Duplicate`; `details_rename_editing` scenario. 182 tests. |
| 2026-08-23 | — | Watcher wired into the listing pipeline: `resolve_watch_batch` in fs-core; pane watch guard + pump with generation guard, patch + cache write-back; child-cache invalidation for dir-view expansion and the sidebar tree via `PaneEvent::DirsChanged`. 196 tests. |
| 2026-08-23 | — | M3 rubber-band marquee (`marquee.rs`): gpui-drag-owned gesture on a new list background surface, arithmetic row hit test against the uniform band (works for virtualized rows), `SelectionModel::select_marquee` (replace / `cmd`-union), single-slot two-speed edge autoscroll on `Spawner::timer`. 215 tests. |
| 2026-08-23 | — | M3 drag & drop (`drag.rs`): `DraggedEntries` payload built at render, per-pane `DropTarget` with out-of-bounds self-clear, 500ms spring-load on `Spawner::timer`, move/copy modifier + cursor flip, `ExternalPaths` in and `external_drag_payload` out; sidebar Favorites drag-to-add and reordering (the M2-deferred gap) via path-keyed `AppSettings::move_favorite`. 242 tests. |
| 2026-08-23 | — | M3 context menus (`context_menu.rs`): row + background menus whose every row dispatches a boxed `actions.rs` action via `window.dispatch_action`, right-click hit-tested with the shared row-band arithmetic (selects its target first), disabled-not-absent rows, one-level `New ▸` / `Sort by ▸` submenus, full-window scrim dismissal + `escape` via a `menu` key-context token. Closes two gaps: `New ▸ Folder / Text file…` now opens the §4c inline editor on a phantom row (`is_new_entry`, nothing created until commit), and `DeletePermanently` — bound but unhandled — is wired to the workspace ConfirmDialog. 266 tests. |
| 2026-08-23 | — | M3 review fixes + visual scenarios: watch registration **and** unregistration moved off the UI thread (`BackgroundWatchGuard`) and no longer torn down by an in-place reload (`watch_generation`); details/Favorites rows keyed by **path** so a mid-gesture re-projection can't hand a press to another entry; an inline editor whose row vanishes is torn down (it used to lock the whole view out) with focus handed back; a watcher patch no longer re-applies `scroll_top`; `listing_ids` stops the selection/cursor retaining children of a folder that left the listing; Explorer's volume rule for drag & drop (`drop_copies`: move within a volume, copy across, ⌥ copies, ⇧ moves) with the drop reading the armed state instead of the release modifiers; spring-load armed only for an accepted target and re-armed after a spent timer; a press in empty space deselects; the checked `Sort by` row is inert; `OpenSelected` opens the whole selection; Favorites drop zone given a minimum height, tints for every payload it accepts, and queued (never cancelled) probes. New scenarios `cut_dimmed`, `context_menu_open`, `marquee_active` — **baselines pending from the macOS runner**. 283 tests (93 fs-core unit + 3 integration + 187 app). |
| 2026-08-23 | — | M4 view modes: `ViewMode` on `Pane` with `SetViewList`/`SetViewIcons` (`cmd-1`/`cmd-2` + toolbar switcher, same boxed actions) and `SetViewColumns` as an explicit "not implemented" notice; `views/icon_grid.rs` — chunked-row `uniform_list`, pure `(cols, len)` geometry, 2D index-arithmetic navigation, DirView's shared selection, cut-dimming/drag/drop/rename reuse; `rename::with_editor_actions` extracted; mode-aware `index_at_content` shared by marquee, drag and context menu; `icon_grid` visual scenario declared (baseline pending). 320 tests (112 fs-core unit + 3 integration + 205 app). |
| 2026-08-23 | — | M4 dual pane: `ToggleSplitPane` (`cmd-shift-o` + titlebar button dispatching the same boxed action) grows `panes` 1↔2 on the flat `Vec`; a fresh pane inherits the active pane's directory and opens in the complementary `ViewMode`, everything else independent; collapsing keeps the **active** pane; hand-built pane splitter (`first_pane_width`, `clamp_pane_width`, 240px minimum per pane) reusing the M2 approach; per-pane active marker; `PaneEvent::FocusIn` active-pane routing now tested (incl. a real click) and cross-pane drag verified end to end; `split_panes` visual scenario declared (baseline pending). 330 tests (112 fs-core unit + 3 integration + 215 app). |
| 2026-08-23 | — | M4 fs-core thumbnails: `Platform::thumbnail(path, px)` returning decoded RGBA (`thumbnail::Thumbnail`), QuickLookThumbnailing on macOS with a bounded wait and an `image`-crate fallback tier, deterministic synthesized pixels in the stub, and the LRU **byte-budget** `ThumbnailCache` keyed on `(path, px, mtime+size)`. 302 tests (112 fs-core unit + 3 integration + 187 app). |
| 2026-08-23 | — | M4 thumbnails in the grid + polish: `thumbnails.rs` — visible+margin request window off the `uniform_list` processor's row range, **single-`Task`-slot cancel-on-scroll-away** (proven with a slow recording `Platform`), every fetch awaited on the background executor, RGBA→BGRA `RenderImage`s pruned with `drop_image` on each window move; `scrollbar.rs` — pure thumb geometry + `Spawner::timer` fade, invisible until the list actually scrolls so no baseline depends on the wall clock; `DirView::content_height` shared with the marquee autoscroll clamp (which had been measuring the grid in row heights). Three Known gaps closed: `Paste { dest }` parameterized so the row menu pastes **into** the right-clicked folder, expansion state (and in-flight child loads) pruned when a folder leaves the listing, auto-hide scrollbar built. 340 tests (112 fs-core unit + 3 integration + 225 app). |
| 2026-08-24 | — | **M4 adversarial-review fixes.** Four blockers: (1) a pinned pane splitter is now clamped at *layout* time — the pinned first wrapper is `flex_shrink_1` with `min_w(PANE_MIN_WIDTH)` on **both** wrappers, so widening the side panels or narrowing the window degrades the pin instead of squeezing the second pane's breadcrumb, rows and free-space status line to 0px with the splitter parked out of reach; (2) the thumbnail request window is derived from the scroll offset + list viewport in `DirView::render`, never from the `uniform_list` row range (gpui calls that processor twice a frame with `0..1` to measure an item, which flipped the window every frame and cancelled + restarted the in-flight fetch, so no thumbnail slower than the repaint cadence ever loaded); (3) foreign probe artifacts removed from the tree; (4) `painted_cols` — the grid's hit tests (`index_at_content`, `tiles_in_rect`, `content_height`, `page_step`, scroll-into-view) read the column count the last frame actually *painted* with, and `note_painted_grid_cols` notifies when a fresh measurement disagrees, so a resize no longer left every right-click, drop-target and marquee naming a different entry than the tile under the pointer. Two majors beyond the thumbnail window (per-frame `drop_image`/re-upload churn, and pruning keyed on path instead of the full `ThumbnailKey`, which leaked the superseded stamp of a file rewritten while visible — `missing` is pruned the same way now): `projected_rows` skips the in-place-expansion splice in `ViewMode::Icons` (children were painting as top-level tiles with no depth cue and no way to collapse them), and `FileClipboard::paste_op` refuses a destination inside or equal to a source **without consuming the clipboard**, the same rule `drag::plan_drop` already applied — a cut folder pasted onto itself used to fail at execution *and* lose the cut. Two minors: `shift-left`/`shift-right` (`ExtendSelectionLeft`/`Right`) extend the grid range by one tile — inert in the details list, where a full-width row has nothing beside it — and the first frame in a new `ViewMode` scrolls the cursor back into view (`painted_mode`), because the two views share one *pixel* offset while measuring items differently. ARCHITECTURE §0 amended for both the new horizontal row and `SetViewColumns` having no trigger in v1. 355 tests (113 fs-core unit + 3 integration + 239 app). |
| 2026-08-24 | #9 | M4 narrow-pane column fit: `details_list::visible_columns` — the Name cell is `flex_1` with `flex-basis: 0` beside fixed-width `Size`/`Date` cells, so the ~270 px M4 split pane squeezed it to ~14 px and **every filename rendered as nothing** (caught by inspecting the regenerated `split_panes` baseline, not by any test or reviewer). Past a `NAME_MIN_WIDTH` floor the trailing columns now drop out — Date first, then Size — and header, body rows and the rename row are all handed the *same* measurement per frame so columns keep aligning. Baselines regenerated on the runner. |
