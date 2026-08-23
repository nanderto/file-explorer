# As Built

The living record of what is **actually implemented**, as opposed to what is planned
(`docs/file-explorer-plan.md`). Every PR that changes code must update this file —
the quality gate (`scripts/gate.sh`) and CI enforce it.

How to update: add or amend the relevant component section below, and add a row to the
change log. Record what exists, how it deviates from the plan (and why), and any known
limitations. Keep it truthful — this file is only useful if it matches the code.

## Status

**M3 in progress** — the fs-core ops engine, the app job/event spine
(JobsModel, dialogs, progress/toasts, undo), the keyboard file operations
(cut/copy/paste with dimming, delete-to-trash, new folder/file) and the
inline rename overlay (`f2` + slow-second-click) with `Duplicate` all work
end-to-end, and the open directory is now **live** — the fs-core watcher is
wired into the pane's listing pipeline, so external changes (and completed
jobs) patch the listing with no explicit refresh. All three mouse surfaces are
in: rubber-band marquee selection, **drag & drop** (within the pane, from
Finder, out to Finder, with folder spring-load, plus the sidebar Favorites
drag-to-add/reordering that M2 deferred), and now **context menus** — row and
background, whose every row dispatches the same boxed action the keymap does.
Two gaps closed with them: `New ▸ Folder / Text file…` now opens the §4c
inline editor on a phantom row instead of auto-naming (nothing is created
until the name is committed), and `DeletePermanently` — bound since the
keyboard-ops step but reaching **no handler** — is now wired to the workspace's
ConfirmDialog. The four mouse-surface lanes were then reviewed by four
adversarial passes and the findings applied (watch registration moved off the
UI thread, path-keyed row identity, an inline editor that could be orphaned by
a watcher patch, Explorer's cross-volume copy rule, spring-load only on a
target the drop would accept, click-in-empty-space deselect, and a patch that
no longer yanks the scroll to the top); the three M3 visual scenarios
(`cut_dimmed`, `context_menu_open`, `marquee_active`) are declared and **await
their first baselines from the macOS runner**. 283 tests green (93 fs-core
unit + 3 integration + 187 app).

| Milestone | State |
|---|---|
| M0 skeleton + visual-test infra | ✅ merged (#1) |
| Phase A architecture | ✅ merged (#3) |
| M1 read-only browsing | ✅ merged (#4) |
| M2 sidebar + in-place expansion | ✅ merged (#5) |
| M3 file operations | 🔄 engine + job spine + keyboard ops + inline rename/duplicate + marquee + drag & drop + context menus + review fixes done; the three new visual scenarios need their baselines generated on the macOS runner |
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
  `AppSettings::save` → `Vfs::atomic_write` on the background executor.
  **M3 closed the deferred drag gaps** (§8): the whole section is one drop
  zone accepting `DraggedEntries` and `ExternalPaths` — dropped paths are
  stat'ed on the background executor and only *folders* are pinned (a dragged
  file is silently refused), matching the sidebar's "pin a place" meaning
  rather than moving files. Three review fixes to that zone: it carries a
  **minimum height** (`3 × TREE_ROW_HEIGHT`), because a `div`'s hitbox is its
  content and with nothing pinned (the default on first run) or the section
  collapsed the whole target was the 32px header — a drop one pixel below it,
  inside the sidebar, in what reads as the Favorites area, silently pinned
  nothing; it tints on hover for **every** payload it accepts
  (`DraggedEntries`, `ExternalPaths`, `DraggedFavorite`), not just the first,
  so the boundary is visible rather than guessed at; and the "is this a folder?"
  probes **queue** behind one task instead of replacing a single
  `Option<Task>` slot, which cancelled an in-flight probe whenever a second
  drop arrived (a folder from a slow mount silently never got pinned). A
  non-empty queue *is* the "a task is alive" signal, drained in the same update
  that applies its results.
  Rows are drag sources for `DraggedFavorite`
  (its own payload type, so gpui's type-keyed drop dispatch can't confuse
  reordering with a file move), where dropping on a row inserts **before** it
  and dropping on the section moves to the end, both through the path-keyed
  `AppSettings::move_favorite(path, before)` and persisted at once. Reorder
  targets highlight as a row background tint, not an insertion rule, so
  arming one never nudges the rows below. Those rows are **path-keyed**
  (`ElementId::Path`, like the details rows) because they are drag sources: an
  index would let a press on one favorite start a drag carrying whichever
  favorite the list had since shuffled into that slot. The sidebar has no
  context menu of
  its own: `context_menu.rs` is wired to the details view only), **Folders**
  (Explorer-style tree:
  volume roots at depth 0, expanded nodes' background-loaded dirs-only
  children spliced beneath with a depth field — the §8 flat projection —
  rendered by `uniform_list`; disclosure triangles mutate the expansion set
  and re-flatten; child listings are cached so collapse/re-expand is
  instant; unreadable dirs simply have no children;
  `invalidate_children(dirs)` — reached from the active pane's watcher batches
  via the workspace — re-lists an expanded node in place and drops a collapsed
  node's cache, so the tree can't show children the filesystem no longer has).
  All colors from the `Theme`.
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
  **Live directory (§4a watcher patch loop):** a `load` that actually *changes
  directory* starts `Vfs::watch(path, WATCH_LATENCY = 100ms)` and keeps the
  guard + pump `Task` in fields, so navigating away unregisters the directory we
  left and dropping the pane stops everything. Three properties of that, all
  from the M3 review:
  (1) **registration runs on the background executor.** `Vfs::watch` is
  blocking, disk-touching work — for `RealVfs` it stats *and* canonicalizes the
  path and stops/starts the backend's run-loop thread — so the pump task
  registers it (`cx.background_spawn`) and comes back only to store the guard;
  a stalled mount can no longer freeze a half-painted frame, and the observable
  form of the rule is a `#[gpui::test]` asserting that no registration exists
  until the executor has run.
  (2) **unregistration is off-thread too**, via `BackgroundWatchGuard` — a
  wrapper whose `Drop` hands the real `WatchGuard` to the `BackgroundExecutor`,
  so every path out (stored, superseded mid-registration, pane gone) unwatches
  there rather than on the UI thread.
  (3) **an in-place reload reuses the live watch.** `sort_by`, `set_show_hidden`
  and `refresh` all reload the *same* path; re-registering cost a full
  stop/restart cycle per click **and** lost every change in the gap (a fresh
  stream starts from now), while leaving `notify` a duplicate path in its
  watch array. The watch is now keyed by a separate `watch_generation`, bumped
  only when the directory changes, which is also what batches are gated on.
  Each debounced batch is
  generation-checked *before* any I/O, resolved off the UI thread
  (`resolve_watch_batch`), then folded in: `Rescan` → `refresh()`, otherwise
  `patch_listing` + cache write-back + `prune_view_state` — which prunes
  vanished paths from the selection (clearing a dangling cursor with them) and
  drops an inline editor whose row went away. A patch deliberately does **not**
  run the `NavEntry` restore any more: `scroll_top` is pane bookkeeping that
  nothing updates while the user scrolls, so re-applying it snapped the list to
  the top on every external change (and fought the marquee's autoscroll, which
  writes the offset directly). Patches apply only to a
  snapshot *of the watched directory* — a stale cross-directory paint mid-
  navigation is left alone — and `snapshot_is_stale` is never cleared by a
  patch, so an in-flight fresh load still wins. This is what ARCHITECTURE §4b
  means by "no explicit refresh — the dest dir's watcher batch patches the
  listing": completed file operations show up through this path. The pane also
  emits `PaneEvent::DirsChanged(Vec<Arc<Path>>)` per batch so cached child
  listings elsewhere can be invalidated (deviation: the watch lives in the
  `Pane`, not the `DirView` as §4a's diagram sketches — the pane has owned the
  snapshot/cache since M1, and the snapshot is what gets patched).
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
  M1 alignment gap). **M3:** one dispatcher (`handle_row_click`) owns every
  row click — double-click opens, `cmd`-click toggles, `shift`-click ranges,
  a plain click selects *and arms the row*, and a later plain click on an
  armed row is the §0 **slow second click** that opens the inline editor
  (arming runs on `Spawner::timer`, `RENAME_CLICK_ARM_DELAY` = 500ms, so a
  double-click lands as `click_count >= 2` and disarms instead). The root key
  context becomes `DirView renaming` while the editor is up, which is what
  every `DirView && !renaming` binding is guarded by. `Duplicate` (`cmd-d`)
  submits `FileOp::Duplicate` for the root-most selection (keep-both names
  planned by ops). `invalidate_children(dirs)` (called by the pane on every
  watcher batch) drops cached child listings the filesystem changed under: a
  collapsed folder simply loses its cache (re-listed on the next expansion),
  an expanded one keeps its rows painted while a fresh listing loads over the
  top — so the selection inside it survives the reload, and any row the reload
  dropped is pruned when the new listing lands. The rubber-band marquee is a
  second `Option<_>` field on the same view (`marquee`, see `marquee.rs`
  below), and the row list is now wrapped in that module's background surface.
  **M3 review fixes here:**
  - **Rows are keyed by path, not by index** (`ElementId::Path`; the disclosure
    triangle is a `NamedChild` of it, and the sidebar's Favorites rows got the
    same treatment). gpui persists a stateful element's `pending_mouse_down`
    across frames by `GlobalElementId` and starts a drag from a later
    mouse-move **without re-hit-testing**, so with index ids a re-projection
    between press and move — which the watcher made routine — handed the press
    to whichever entry had slid into that index, and the row's `on_drag`
    payload turned that into a real `FileOp::Move` of a file nobody touched.
    `debug_selector`s stay index-based: they name a position on screen, which
    is what a test clicking "the third row" means.
  - **`listing_ids` is the one definition of "in the listing"**, shared by
    `retain_selection_in_listing`, the cursor-restore check the pane calls, and
    the orphaned-editor check. It counts the snapshot's rows plus the loaded
    children of expanded folders **whose own row is still there** — walking
    `expanded` in path order, so a folder the watcher removed contributes
    nothing and neither do the folders nested inside it. Previously the
    selection (and the cursor, via the pane's own `injected_contains`, now
    deleted) kept pointing at invisible, nonexistent rows that a later
    cut/paste, Duplicate or Delete would have acted on.
  - **`OpenSelected` opens the whole selection** (root-most, in projection
    order), not just the cursor row — Explorer's behavior, and what the row
    menu's `Open` row (enabled for any non-empty selection) reads as promising.
    At most one folder is entered, because one pane can only show one directory.
  - `DirView::list_viewport()` / `DirView::ROW_HEIGHT` are now **public**, so
    the visual scenarios can aim real mouse input at real painted pixels
    instead of re-deriving the chrome's height.
- `rename.rs` + the rename row in `views/details_list.rs` (M3): the §4c
  inline-rename state machine as a **field** of `DirView`
  (`rename: Option<RenameState>`), not an entity. `begin_rename` (from `f2`
  or the slow second click) creates one vendored `InputState`, sets the name
  and preselects the **stem** via `fs_core::split_name` (now `pub`, so the
  split is shared rather than re-implemented), focuses it and subscribes to
  its focus loss. `views/details_list.rs` swaps that row's name cell for the
  editor (the Size / Date cells stay filled — only the name cell changes,
  which is what Explorer does; `size_cell` is shared by both row renderers so
  the column can't diverge), wiring `Confirm`/`Cancel` plus the vendored
  input's editing actions in a row-level `TextInput` key context (same
  pattern as `address_bar.rs`);
  the row `track_focus`es the editor's handle so those actions actually
  resolve. `Confirm` validates locally (nonempty, no path separator, trimmed;
  an unchanged name just closes), then submits `FileOp::Rename` and shows the
  pending name until the job's terminal event — success moves the selection
  onto the new path and tears the editor down, while a failure (a collision is
  a plain `rename` error, `overwrite: false`) lands back in the still-open
  editor as a `deferred` error popup under the row. That failure path is why
  `JobsEvent` gained `Failed { id, error }`: the submitter reacts per job id
  instead of parsing toast text. `Escape`, focus loss, and *leaving* the
  directory all tear the editor down (an in-place reload — refresh, sort flip,
  hidden toggle — deliberately does not; `Pane::load` only cancels when the
  path actually changes). **So does the row itself vanishing**
  (`cancel_rename_if_target_vanished`, called by the pane after every snapshot
  swap with the snapshot it swapped in): an external delete of the row being
  renamed — or a background job of our own finishing on it — used to leave
  `rename = Some(..)` forever, and that state is a **total lockout** of the
  details view, not a cosmetic leak: the projection stops yielding the target
  row so no editor paints and its unpainted `TextInput` node leaves the
  dispatch tree (taking `escape` with it), while the root context stays pinned
  to `DirView renaming` — killing every `DirView && !renaming` binding: `f2`,
  `cmd-a`, arrows, cut/copy/paste, delete, page-up/down — and the marquee, the
  context menu and every row's `on_drag` all keep bailing on `rename.is_some()`.
  Only navigating to another directory cleared it. Two carve-outs keep the fix
  honest: a §4c phantom row is injected unconditionally (it can never
  "vanish"), and an editor already `processing` is *expected* to leave the
  listing — that is its own rename landing, and the job's terminal event owns
  the teardown because it is what moves the selection onto the new name. This
  is also the one teardown path with no `Window` in hand (it arrives on the
  pane's async batch pump), so `RenameState` records the `AnyWindowHandle` it
  was opened in and the teardown `cx.defer`s a focus restore through it —
  otherwise focus would sit on the editor's unpainted handle and the keyboard
  would land nowhere until the user clicked a row.
  **New-entry naming** (§4c's `is_new_entry: true`, closing the gap this
  document used to record): `RenameState` gained
  `new_entry: Option<(NewEntryKind, FileEntry)>` and `DirView::begin_new_entry`
  opens the *same* editor on a **phantom row** — a `FileEntry` for a path that
  does not exist yet (mtime `UNIX_EPOCH`; the details row renders its Size and
  Date cells blank, so nothing wall-clock-dependent ever paints), appended last
  by `projected_rows` and scrolled to. `Confirm` submits
  `CreateDir`/`CreateFile` instead of `Rename`; validation, the processing
  state, the inline collision error (both create ops fail on an existing path,
  so a taken name lands in the still-open editor exactly as a colliding rename
  does), escape, blur and navigation are all the shared code. **Nothing reaches
  the disk until the name is committed**, so `Escape` leaves the directory
  untouched, and `Pane` supplies only the destination and the deconflicted
  placeholder ("New Folder" / "New Folder 2", "New Text File.txt") — §0's
  handler column for both rows is literally "Pane → DirView". The phantom is
  excluded from drop targets and context-menu targets (`is_new_entry_row`): it
  is folder-shaped but nothing has created it.
- `marquee.rs` (M3, §8 "Rubber-band marquee"): rubber-band selection as a
  **field** of `DirView` (`marquee: Option<MarqueeState>`), same shape as
  `rename`. `marquee::list_surface` builds the details list's background
  surface — the element that now parents the row list — and carries the whole
  gesture: `on_drag(MarqueeStart, empty ghost)` so gpui owns mouse capture,
  `on_drag_move::<MarqueeStart>` (which fires for every move while the drag
  lives, in or out of the element) for the moving corner, and
  `on_mouse_up` + `on_mouse_up_out` to end it wherever the release lands.
  **Every hit test is arithmetic against the uniform row band**, never a scan
  of painted elements, because `uniform_list` virtualizes off-screen rows
  away: three pure, headlessly unit-tested functions are the whole model —
  `ContentPoint::from_window` (window space → content space, undoing the
  viewport origin and the negative-going-down scroll offset),
  `rows_in_rect` (normalized content rect → half-open row range;
  `floor(top/h) .. ceil(bottom/h)` clamped to the listing, i.e. row `i` is in
  iff its band `[i·h, (i+1)·h)` and the rubber band overlap as *open*
  intervals — any non-zero overlap counts, an edge landing exactly on a row
  boundary does not reach into that row) and `autoscroll_for` (pointer →
  `AutoScroll::{Up,Down}{Slow,Fast}`: within 24px of a viewport edge is slow,
  at or past it is fast, nearer edge wins). Rows are full width, so only the
  vertical span picks rows; the horizontal span is normalized and painted but
  does not filter. Autoscroll is §8's **single** `Option<Task>` slot on
  `Spawner::timer` (30ms ticks, 8/28 content-px per tick), respawned only when
  the direction/speed changes, dropped when the pointer comes back inside, and
  dropped with the state on release; each tick moves the scroll offset
  (clamped to `rows·h − viewport`) and shifts the band's moving corner by the
  same amount, since the anchor is fixed in *content* space. Selection goes
  through `SelectionModel::select_marquee(base, rows, focus)`, recomputed from
  scratch every move: `base` is empty for a plain drag (which replaces) and
  the pre-gesture selection for an additive `cmd`-drag (which unions, matching
  Explorer), so shrinking the band gives back only rows the band itself added;
  the cursor/anchor follow the moving corner so a following `shift`-arrow
  extends from where the drag stopped. A marquee starts **only in empty
  space** — a press whose content `y` falls inside the painted row band is the
  start of a *file* drag (`drag.rs`), and arms nothing — and never while the
  inline rename editor is up. **A plain press in empty space also deselects
  immediately** (M3 review): a press that never crosses gpui's drag threshold
  is a *click*, and nothing else owns it (the surface has no `on_click`), so
  clearing only inside the band's own selection pass left click-to-deselect
  doing nothing at all — with the selection still live under a highlight the
  user believed they had dismissed, right before reaching for Delete. A
  `cmd`-press is exempt: it is the additive gesture, and keeps the selection to
  union the band onto. The band renders as an absolutely-positioned
  translucent `accent` rectangle (fill 0.18 / border 0.8 alpha), clamped to
  the viewport. Two deviations from the §8 sketch: (1) `MarqueeStart` is a
  **unit** payload rather than `MarqueeStart { origin }` — a drag payload is
  constructed at *render* time, before any press exists, so the origin is
  captured in the surface's `on_mouse_down` (which also gives the true press
  point instead of wherever gpui's 2px drag threshold happened to trip) and
  lives in `MarqueeState`; (2) the surface is described as the *pane*
  background but is built as a `DirView` element, because the pane never sees
  the row geometry or the scroll handle the hit test needs.
- `drag.rs` (M3, §8 "Drag & drop"): the §8 row as specified —
  `DraggedEntries { grabbed, selection: Arc<[Arc<Path>]>, source_pane }` built
  at *render* time by `details_list` (so the payload is the selection as last
  painted, which is exactly Explorer's rule, because a press changes nothing
  and a click does: a grabbed row that was selected drags the whole
  **root-most** selection, one that was not drags only itself). A payload is
  built for **every drag-capable row on every frame**, which shapes two things:
  the root-most selection is reduced once per frame in `DirView::render` into a
  shared `Arc<[Arc<Path>]>` (a selected row's payload is then a refcount bump,
  not a rescan), and `SelectionModel::selected_rootmost` — now the shared
  primitive behind cut/copy/trash's `selected_paths_rootmost` too — is a
  **linear** walk of the ordered set with one "last kept" anchor instead of the
  old pairwise scan, which is sound because path ordering is component-wise
  (every descendant sorts directly after its ancestor, while a same-prefix
  *sibling* like `/d/subtle.txt` does not). The outbound Finder dir flags are
  resolved lazily for the same reason: the `external_drag_payload` resolver
  reads the view through a `WeakEntity` if and only if the drag actually leaves
  the window. Then: a single
  `drop: Option<DropState>` per pane — the third state machine to live as a
  *field* of `DirView` rather than an entity, after `rename` and `marquee` —
  carrying the `DropTarget::{Folder(path), Background}`, whether the drop would
  copy, whether it would do anything at all, and the spring-load task.
  The **same background surface** the marquee uses carries the drop side
  (`drag::with_drop_handlers` chains onto `marquee::list_surface`'s element, so
  neither gesture adds a layout node): `on_drag_move` for both payload types
  (they fire for every move while a drag lives, in the element or out, which is
  what makes §8's **out-of-bounds self-clear** possible — a pointer outside the
  surface clears this pane's target), `on_drop` for both, and
  `on_mouse_up_out` so a release elsewhere ends it. Destinations are pure and
  headlessly tested: `row_at` (content `y` → row index, arithmetic against the
  uniform band like the marquee, so virtualization is irrelevant),
  `target_for_row` (folder row → that folder; a **file** row injected by
  in-place expansion → *its* folder, not the pane's, because that is the row
  the user aimed at; anything else → `Background`), and `plan_drop`, which is
  both the op planner and the highlight predicate: `Move` or `Copy` as
  `drop_copies` decides, `None` for a destination inside/equal to a source and
  for a move whose sources already live there (a same-folder *copy* is kept —
  it is a deliberate duplicate and op planning names it).
  **`drop_copies` is the single place move-vs-copy is decided** (M3 review), so
  the highlight predicate, the cursor and the submitted op cannot disagree, and
  it implements Explorer's rule rather than "always move": a drag **within one
  volume moves**, a drag **across volumes copies** (`Vfs::volume_key`, the same
  derivation the job queue lanes by — dragging a file off a USB stick must not
  empty it), **⌥ forces a copy** and **⇧ forces a move** (⇧ is range-select on
  a *click*, but a drag never range-selects, so the two cannot collide). A
  mixed-volume drag copies: one gesture cannot be half a move. The old
  always-move default matched neither Explorer nor Finder and would have
  deleted the source of a no-modifier drag off a removable volume.
  The **drop itself reads the armed state, not the modifiers at mouse-up**:
  releasing ⌥ a frame before the button used to re-derive `copy = false`, which
  `plan_drop` then refused — a lit, valid-looking target that silently did
  nothing (and the mirror case turned a promised move into a copy). The
  modifier fallback survives only for a drag that entered exactly on the
  release, which has no armed state to read. fs-core's own guards
  (`queue.rs` fails dest-inside-source, skips move-into-own-folder) stay the
  backstop; refusing to submit means a slip of the mouse produces silence
  instead of a failure toast, and nothing here duplicates the engine's
  semantics. **Spring-load** is §8's 500 ms `Spawner::timer` task in one slot,
  re-armed when the hovered folder changes and dropped when the pointer leaves
  it; it emits `DirViewEvent::NavigateTo` (events up — the pane owns history)
  and then re-points the target at `Background`, so a release right after a
  spring-open lands in the folder just entered. Two review fixes: it is armed
  **only for a target the drop would accept** (`valid`), because arming it for
  a refused one meant that merely *beginning* a drag on a folder row — press,
  move 2px, hold, still inside the 24px band — navigated the pane into that
  folder 500 ms later while the cursor said `OperationNotAllowed`, losing the
  gesture; and the "same target, keep the timer running" fast path now
  **re-arms a spent one**. A drag that leaves the window takes gpui's
  `active_drag` and dispatches no mouse event, so the armed `DropState` outlives
  it with a timer that has already fired and declined; adopting that state
  killed spring-load for that folder for the rest of the session. `spring_load`
  therefore clears its own slot when it declines for want of a live drag (a
  *finished* `Task` is still `Some`, so nothing else can tell the two apart),
  and the fast path re-arms when the slot is empty. The modifier check flips the
  cursor through `App::set_active_drag_cursor_style` (`DragCopy` / `Arrow`,
  plus `OperationNotAllowed` where the drop would do nothing). Highlights come
  from the `Theme` accent and are **layout-neutral by construction**: a folder
  target is a row background tint, a `Background` target an absolutely
  positioned accent ring — arming one never moves a row. Both readers require a
  live gpui drag (`App::has_active_drag`), which is how a platform file drag
  that leaves the window — it takes gpui's `active_drag` with it and dispatches
  no event we can see — fails to leave a stale highlight or a spring-load
  behind it. **Interop:** every target also accepts `ExternalPaths` (Finder →
  us; gpui translates a platform file drop into an ordinary internal drag, so
  the inbound half is exercised headlessly in tests), and every row's `on_drag`
  pairs with `external_drag_payload` (us → Finder) resolving to
  `ExternalDragPayload::Files` with each path's dir flag read off the rows we
  already have — the UI thread never stats the disk mid-gesture. An external
  drop **always copies**: the platform strips modifiers from file drops, and
  guessing "move" would delete another app's files. This module also closes the
  M2 note that a press-and-drag from a row created an invisible no-op drag: the
  row's own `on_drag` claims the gesture first (gpui stops propagation when it
  starts a drag), so the surface's marquee drag never begins.
- `context_menu.rs` (M3, §8 "Context menu"): the §8 row —
  `menu: Option<ContextMenuState>` as the **fourth** field-shaped machine on
  `DirView` (after `rename`, `marquee`, `drop`), carrying the invocation
  `Point`, the built rows, and which submenu is open; rendered as
  `deferred(anchored().position(p))`. **Every row holds a `Box<dyn Action>`**
  and activating it calls `window.dispatch_action` from the DirView's focus
  handle — the keymap's own path — so the action lands wherever its handler
  lives (this view for the clipboard rows, the pane for
  `NewFolder`/`NewFile`/`Refresh`/`SortBy`, the workspace for
  `ToggleHiddenFiles`/`DeletePermanently`) and §0's "each command's logic
  exists exactly once" holds literally: no menu row calls a method on a view.
  The **trigger** is one `on_mouse_down(MouseButton::Right)` on the same
  background surface the marquee and the drop targets use (gpui converts
  macOS ctrl-click to a right click in the platform layer, so there is no
  second chord to handle), hit-tested with the same `row_at` band arithmetic:
  a row band opens the **row menu** — selecting that row first if it was not
  already selected, and otherwise keeping the whole multi-selection while
  moving the cursor onto the clicked row so `Open`/`Rename` target it — and
  the empty space below the last row opens the **background menu**, leaving
  the selection alone because every command there acts on the folder.
  Row menu: Open (which dispatches `OpenSelected` and so acts on the **whole**
  selection — see `dir_view.rs`; it was enabled for a multi-selection while
  opening only the cursor row) · Cut/Copy/Paste · Duplicate/Rename ·
  Delete/Delete Permanently. Background menu: Paste · `New ▸` Folder / Text File… ·
  Refresh · `Sort by ▸` Name / Size / Date Modified (✓ on the active column,
  which is also **disabled** — see the Known gaps entry: `SortBy` flips the
  direction for an unchanged key, which is right for a header click but in a
  menu whose only feedback is a stationary ✓ it silently reversed the listing) ·
  Show Hidden Files (✓ when on). Rows that cannot apply render **disabled, not
  absent** (plan §3): they lose their click listener and hover entirely, so
  clicking one does nothing at all — not even dismiss. The two pure builders
  (`row_menu`/`background_menu` over a `MenuFacts` snapshot of
  selection size / open directory / clipboard / sort / hidden flag) are the
  unit-tested part; the enabling rules live only there. Submenus are one level
  deep, opened by hover *or* click. **Dismissal** is an invisible full-window
  scrim rendered under the panel rather than an `on_mouse_down_out` on it:
  both panels `occlude`, so a press on either never reaches the scrim while a
  press anywhere else does — which is the only correct answer with a submenu
  open, because a submenu panel sits *outside* its parent's bounds and an
  out-handler on the parent would fire on the way to clicking a submenu row and
  close the menu under the pointer. It also gets Explorer's behavior that the
  dismissing click does not act on what it landed on; a right-press on the
  scrim dismisses and re-opens for wherever it landed. `escape` dismisses via a
  `menu` token added to the root key context while a menu is up (the same
  dynamic-token shape as `renaming`), and no menu opens while the inline editor
  is up. Colors are all `Theme` (accent at `MENU_HOVER_ALPHA`, panel, border,
  muted for disabled).
- `jobs_model.rs` (M3): `JobsModel` non-render entity — the **sole** consumer
  of the fs-core `JobEvent` channel (one `_pump: Task` held in a field, §2).
  Folds events into `Vec<JobRow>` (progress popover data; terminal rows are
  replaced by toasts), queues parked conflicts (`pending: VecDeque`, emitting
  `NeedsDecision` for the front and `DecisionObsolete` when a parked job dies),
  and pushes each completed op's `UndoEntry` onto the shared undo stack
  **exactly once** — inverse jobs submitted by undo/redo are registered in a
  suppression set (synchronously after submit, no intervening await, so the
  pump can't observe their completion first) and never push, keeping the
  stacks from feeding themselves. Toasts (`Success`/`Error`/`Info`) auto-
  dismiss via a `Spawner::timer` task per toast (fake time in tests; expiry
  detaches its own task rather than self-cancelling). Emits
  `JobsEvent::{RowsChanged, NeedsDecision, DecisionObsolete, Completed}`.
- `dialogs/` (M3, §8 "Dialogs"): minimal in-house modal —
  `Workspace.modal: Option<ModalState>` rendered as a `deferred` overlay +
  scrim (theme-derived, `occlude`d; no click-away dismissal). `ConfirmDialog`
  (title/message/destructive label; own `ConfirmDialog` key context with
  `track_focus`, enter/escape) guards `DeletePermanently`: the workspace
  holds the pending `FileOp` (`Workspace::show_confirm(ConfirmRequest)`) and
  submits it only on `Confirmed`. `ConflictDialog` (own `ConflictDialog` key
  context per §0: `r`/`s`/`k` resolve, `a` toggles apply-to-all, `enter` =
  Replace default, `escape` dismisses **and cancels the job**) shows the §3
  size+date comparison of both sides. Both dialogs are dumb emitters; the
  workspace mediates: `NeedsDecision` → modal (focused on open; a busy modal
  keeps priority and the pending conflict re-checks on close), resolution →
  `queue.resolve`/`cancel` → `JobsModel::decision_handled` (announces the
  next parked conflict); prior focus is restored on close. Deviation
  recorded in ARCHITECTURE §0/§3 (same PR): a `ConfirmDialog` key context
  row was added — the table previously bound enter/escape only for the
  conflict dialog.
- `jobs_ui.rs` (M3, §8 "Progress popover + toasts"): pure observers of
  `JobsModel`. `JobsIndicator` — titlebar button (rendered only while jobs
  run, so idle chrome and existing baselines are untouched) toggling an
  `anchored`+`deferred` popover listing job rows with verb + current file,
  an accent progress bar, and a per-job cancel ✕ (`JobQueue::cancel`).
  `ToastLayer` — bottom-right overlay rows (completion/error/undo-
  invalidation), click to dismiss early.
- `workspace.rs` (watcher wiring): subscribes to each pane and forwards
  `PaneEvent::DirsChanged` to `Sidebar::invalidate_children` — the sidebar
  tree's own child caches would otherwise never learn about external changes
  (events up, method calls down; the sidebar opens no watch of its own).
- `workspace.rs` (M3 additions): owns the modal state + jobs subscription
  (`subscribe_in`, so handlers can focus/restore), embeds the indicator and
  toast layer, and handles `Undo`/`Redo` (§0 `cmd-z`/`cmd-shift-z`): a
  detached one-shot task locks the shared async-mutex `UndoStack`
  (`app_state::SharedUndoStack` — a blocking lock would deadlock the
  foreground executor across the validate await), applies, then registers
  the inverse job ids with `JobsModel`; `UndoOutcome::Invalidated` surfaces
  as a "Can't undo — …" toast, never applied against stale state.
  Also handles `DeletePermanently` (§0 "Bypass trash (confirm dialog first)"),
  which until now had **no handler anywhere** — bound in the `DirView` context
  so `!renaming` guards it, but implemented here because the workspace owns the
  modal: it builds a `ConfirmRequest` over the active pane's root-most
  selection (singular/plural message) and `FileOp::Delete` is submitted only on
  `Confirmed`. `shift-delete` and the row menu's **Delete Permanently** are the
  same one path.
- `app_state.rs` (M3): `FsContext` grew `queue: Arc<JobQueue>`,
  `undo: SharedUndoStack`, and `jobs: Entity<JobsModel>`;
  `app_state::install(cx, vfs, spawner, opener, platform)` is the single
  wiring point for the spine, shared by boot, the visual runner, and every
  test (`init` delegates to it).
- `actions.rs`: the §0 table's M1 action set (`actions!` namespace
  `file_explorer`) + parameterized `SortBy { key: SortKey }`. Deviation from
  the ARCHITECTURE.md §3 sketch: `SortBy` derives `Action` with `no_json`
  instead of serde — this gpui rev requires `schemars::JsonSchema` for
  JSON-buildable actions, which isn't needed until user keymap overrides (M7);
  `SortBy` is mouse-dispatched so nothing is lost in M1.
- `keymap.rs`: the §0 M1 rows transcribed 1:1 into `cx.bind_keys` with the
  declared contexts (`Workspace`, `Pane`, `DirView && !renaming`,
  `AddressBar`, `TextInput`), plus the M3 job-spine rows: `cmd-z`/`cmd-shift-z`
  (`Workspace`), `r`/`s`/`k`/`a`/`enter`/`escape` (`ConflictDialog`), and
  `enter`/`escape` (`ConfirmDialog`) — each new context has a probe dispatch
  guard plus real-entity coverage in `workspace.rs` tests. One row added with
  the context menus: `escape` → `Cancel` in `DirView && menu`, guarded from
  both sides (it fires with the token, and is dead without it) so it can never
  shadow the rename editor's own `TextInput` escape.
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
- Watcher coverage (all on fake time — `advance_clock(WATCH_LATENCY)` is the
  only thing between an injected event and its batch): `pane.rs` — external
  create/remove/rename patch the listing with no `Refresh` (and nothing applies
  before the debounce window closes); a patch keeps the selection and cursor
  and prunes a vanished cursor; a batch for a directory the pane has left is
  ignored while the new directory is watched instead (`watcher_count` proves
  the old registration is gone); `Rescan` reloads in full. `dir_view.rs` — an
  expanded folder re-lists in place when its contents change externally, a
  collapsed one re-lists on the next expansion, and a watched-directory patch
  re-projects without collapsing the subtree. `sidebar.rs` — the tree's cached
  children are invalidated end-to-end through
  `Pane → PaneEvent::DirsChanged → Workspace → Sidebar`, expanded and
  collapsed.
- Marquee coverage: the arithmetic first, headlessly and hard (`marquee.rs`
  unit tests) — inverted drags (upward *and* leftward normalize to the same
  band and the same rows), partial-row overlap, an edge landing exactly on a
  row boundary, a degenerate zero-height band, bands clamped to the listing
  from both ends, an empty listing and a zero row height, scrolled content
  (`from_window` with a negative offset picks a row `uniform_list` never
  painted), and the two-speed edge table incl. the nearer-edge tie-break in a
  viewport shorter than two bands. Then the gesture through real simulated
  mouse input on a laid-out window: a background drag selects exactly the rows
  it crosses and leaves the cursor on the band's leading row; a plain marquee
  replaces the selection while a `cmd` marquee unions with it; shrinking the
  band gives back only the rows the band added; the band's rect stays anchored
  to the press in content space (asserted unclamped, past the viewport's left
  edge); a press on a row band arms nothing; edge autoscroll advances exactly
  one fast step per `AUTOSCROLL_TICK` of fake time, keeps ticking while the
  pointer stays out, grows the selection over the rows it scrolls onto, and
  stops dead on release; and no marquee starts while the rename editor is up.
- Drag & drop coverage (`drag.rs`): the pure parts headlessly — the payload
  rule (a selected grab carries the whole selection *as the same allocation the
  frame built* — asserted with `Arc::ptr_eq`, which is what keeps a large
  selection from being quadratic — an unselected one carries itself, root-most
  only, and building a payload never mutates the selection; `selection.rs`
  covers the linear root-most walk itself, including nested levels and the
  same-prefix sibling that must **not** be pruned),
  `row_at`'s band arithmetic (boundaries, past-the-end, empty listing, zero row
  height), `target_for_row`'s three cases, `dest_dir`, the copy modifier, the
  outbound `external_payload` (paths paired with dir flags; empty → `None`),
  and every `plan_drop` branch (move/copy, dest-inside-source, onto-itself,
  same-folder move as nothing vs. same-folder copy as a duplicate, and a mixed
  drag keeping only what has somewhere to go). Then the gesture through real
  simulated mouse input on a laid-out window, asserted against the `FakeVfs`
  **tree** — the queue really runs, so these prove the file actually moved:
  an unselected row drops into a folder as a move (and the row's drag, not the
  marquee, owns the press); a selected row carries the whole selection while a
  selected-but-not-grabbed sibling stays put; the ⌥ modifier makes it a copy
  and flips the cursor to `DragCopy`; a background drop moves into the open
  directory; a drop on an expanded folder's child row lands in *that* folder; a
  folder dropped on itself submits nothing and shows `OperationNotAllowed`; a
  drop back into the source directory does nothing; leaving the list clears the
  target and a release out there drops nothing on us; spring-load navigates on
  fake time after exactly `SPRING_LOAD_DELAY` (not before) and the following
  release lands inside, while moving off the folder cancels it; a Finder-style
  `ExternalPaths` drag (driven through real `FileDropEvent`s) arms the same
  target and **copies**, leaving the other app's file alone; and a drag that
  leaves the window stops inviting a drop and never springs anything open
  behind itself. Favorites drag (`sidebar.rs` + `settings.rs`):
  `move_favorite`'s insert-before/to-the-end arithmetic and all five of its
  no-op cases headlessly, then `#[gpui::test]`s that a reorder persists to the
  settings file immediately and survives a restart, and that drag-to-add pins
  only folders (a dragged file is refused, a re-dropped favorite changes
  nothing) — **plus both gestures through real mouse input** at
  `debug_bounds("sidebar-favorite-{ix}")` / `…-favorites-drop-zone` (a favorite
  row dropped on another row lands before it; one dropped on the section header
  goes to the end; a *pane* row dropped under the Favorites header — inside the
  zone's minimum height, with nothing pinned — is pinned), and that two drops
  landing in the same tick both survive.
- **M3 review-fix coverage** (each test fails on the pre-fix code; several were
  written against a deliberately reverted fix to confirm that):
  `pane.rs` — no watch registration exists until the executor has run (the
  observable form of "registration is not UI-thread work"); four in-place
  reloads (two sort flips, a hidden toggle, a refresh) leave
  `FakeVfs::watch_registrations()` at **1** and the live watch still delivers,
  while actually leaving the directory takes it to 2 and unwatches the old one;
  a watcher patch on a scrolled 60-row listing leaves the scroll handle's
  offset **exactly** where it was. `rename.rs` — a patch that removes the
  target closes the editor, hands focus back to the list and lets `cmd-a`
  select again (the whole-view lockout), while an editor that is `processing`
  survives its own rename leaving the listing and still moves the cursor onto
  the new name. `dir_view.rs` — removing an expanded folder drops its injected
  children from the selection *and* the cursor; `enter` on a multi-selection
  opens every selected file (via a `RecordingOpener`) and enters at most one
  folder. `drag.rs` — `drop_copies`'s volume matrix (same/cross/mixed volume, ⌥,
  ⇧, ⇧ beating ⌥); a refused target never springs open even after
  `SPRING_LOAD_DELAY * 2`; a `DropState` left behind by a drag that left the
  window does not stop the *next* hover of the same folder from springing it;
  a drop advertised as a copy with ⌥ **stays** a copy when ⌥ is released before
  the button; and a watcher patch between the press and the move still drags
  the row that was pressed, not whatever slid into its index.
  `marquee.rs` — a plain press in empty space clears the selection and the
  cursor before any band exists, while a `cmd`-press keeps them.
  `context_menu.rs` — re-picking the checked `Sort by` column changes neither
  key nor direction and does not dismiss the menu, and `Open` is enabled for a
  multi-selection.
  `selection.rs` covers `select_marquee`'s replace/union/shrink and
  empty-band semantics view-independently.
- Context-menu coverage (`context_menu.rs`): the enabling rules headlessly
  first, because that is where a context menu actually goes wrong — every row
  command's action **name** (so a row can never silently stop dispatching a
  real action), `SortBy`'s per-row payload and its ✓ on the active column,
  `New ▸`'s two rows as the only entry point `NewFile` has, and disabled-not-
  absent for Paste with an empty clipboard, every row command with an empty
  selection, Rename with a multi-selection, and creation/refresh with no
  directory open. Then the menu itself through **real simulated right-clicks
  and real clicks on painted item bounds** (`debug_selector` →
  `VisualTestContext::debug_bounds`, so the click lands where the pixel
  actually is, `anchored()` fit included): a right-click outside the selection
  selects that row and opens the row menu at the invocation point; one inside a
  three-row selection keeps all of it and moves only the cursor; a right-click
  below the rows opens the background menu and leaves the selection alone; a
  `Copy` row really fills the clipboard and a `Paste` row really lands
  `a copy.txt` in the `FakeVfs` tree (the queue runs — the effect is asserted,
  not the click); a `Sort by ▸ Size` row reaches the **pane's** handler two
  nodes up; a disabled `Paste` row does nothing and does not even dismiss,
  while an enabled row in the same menu still works; clicking away and `escape`
  both dismiss; and a right-click while the inline editor is up opens nothing
  and tears nothing down. New-entry naming: `New ▸ Folder` opens the editor on
  a projected phantom row with **nothing on disk**, and committing a typed name
  creates exactly that; `New ▸ Text file…` preselects only the stem so typing
  keeps `.txt`; `escape` leaves the directory and the projection as they were;
  a name that is already taken comes back as an inline error in the still-open
  editor; and the placeholder skips a "New Folder" that already exists
  (`next_available_name` unit-tested in `pane.rs`). `drag.rs` adds the guard
  that a phantom row is not a drop target — an external drop aimed at it lands
  in the pane's directory, never inside a folder that does not exist.
  `workspace.rs` covers `DeletePermanently` end to end: `shift-delete` on a
  selection opens the ConfirmDialog and submits nothing until `enter`, and with
  an empty selection it opens no dialog at all.
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
  `listing_sorted_by_size`, `address_bar_editing`, (M2)
  `sidebar_tree_expanded` (navigates, then expands `/` and `/home` in the
  sidebar tree) and `details_folder_expanded` (navigates to `/home`, then
  expands `/home/Documents` in place in the details view), and (M3)
  `conflict_dialog` (submits a copy that parks on a fixture conflict —
  `Downloads/notes.txt` onto `Documents/notes.txt` — so the workspace's
  modal + scrim render; FakeVfs counter mtimes keep the size/date panel
  deterministic) and `details_rename_editing` (navigates to
  `/home/Documents`, then `begin_rename`s `report.pdf` so the row editor
  renders with the stem selected). The three M3 mouse-surface states
  ARCHITECTURE §8 asks for round it out: `cut_dimmed` (selects
  `archive.zip` + `readme.md` and `Cut`s them, so the rows paint at
  `CUT_DIM_OPACITY` under the selection tint), `context_menu_open` (a real
  simulated **right-click** in the empty space below the last row, so the
  background panel renders where it was invoked — ✓ on the sorted column, two
  submenu arrows, a disabled Paste) and `marquee_active` (a real press in that
  same empty space, then two moves up over the rows, deliberately **not**
  released, so the band, its border and the rows it has selected are all live
  in the captured frame). All three drive the production input path rather than
  calling handlers, using the new public `DirView::list_viewport()` /
  `DirView::ROW_HEIGHT` to place the pointer — no chrome heights are hard-coded
  and nothing wall-clock-dependent paints (a §4c phantom's mtime is
  `UNIX_EPOCH` for exactly this reason). The runner installs the FakeVfs fixture
  **and** a fixture settings file (two favorites) via
  `settings::init_with_path`, so all content is deterministic. The fixture +
  job spine are installed **per scenario**, not once for the run: the queue
  and `JobsModel` are globals, so a single install let `conflict_dialog`'s
  permanently parked job paint its titlebar "1 job" indicator into every
  scenario declared after it (caught the first time a scenario was added
  below it). Baselines for new scenarios must be generated via the
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
  configurable free space, `watcher_count()` (live registrations) and
  `watch_registrations()` (registrations *ever* made — the only way to prove a
  caller reuses a watch instead of tearing it down and rebuilding it, which
  `watcher_count` cannot show because the rebuild nets out to one).
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
  `resolve_watch_batch(vfs, dir, batch) -> ResolvedBatch { patches, reload,
  changed_dirs }` turns one debounced watcher batch into patches: it stats
  only the changed **direct children** of `dir` (once per path, newest event
  kind wins), resolves a `Created`/`Changed` path that no longer stats to
  `Remove` (created-and-deleted inside one debounce window can't leave a ghost
  row), sets `reload` for a `Rescan`, and reports every directory the batch
  touched as the invalidation set for cached *child* listings.
- `watcher.rs`: `PathEvent{Created,Changed,Removed,Rescan}`; debounce pump
  runs on `Spawner::spawn` + `Spawner::timer` (fake time in tests), coalesces
  a batch (duplicates dropped, any `Rescan` collapses the batch); RAII
  `WatchGuard` unregisters on drop. Real impl: one process-global `notify`
  watcher with per-root registrations; `watch` is best-effort (failure ⇒
  terminated stream + noop guard). The notify path is exercised manually
  (per-milestone Mac checklist), not by unit tests — tests drive the FakeVfs
  event path per the §9 map.
- Tests: 93 unit tests + 3 integration (`cargo test -p fs-core`) covering the
  §9 rows for sort/listing/cache/watcher/exec — including
  `resolve_watch_batch`: create/remove folding, one stat per changed path,
  vanished-create → removal, descendants reported as `changed_dirs` without
  patching, and `Rescan` → reload — plus `RealVfs` list/stat/free-space against
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

- **The info panel always reads "No selection"** — it is still the M0 chrome
  placeholder and ignores the selection entirely, which the M3 baselines now
  pin: `cut_dimmed`, `marquee_active` and `details_rename_editing` all show
  selected rows beside a panel claiming nothing is selected. Cosmetic, but it
  makes four committed baselines look wrong to a reviewer. *M5 (info panel).*

- **The three M3 visual scenarios are declared but have no baselines yet.**
  `cut_dimmed`, `context_menu_open` and `marquee_active` are in `scenarios()`
  (all three driven by real simulated mouse input at real painted pixels, via
  the new public `DirView::list_viewport()` / `DirView::ROW_HEIGHT`), and a
  scenario with no committed baseline **hard-fails** the macOS visual job — so
  `gh workflow run update-visual-baselines.yml --ref m3-file-operations` has to
  run on this branch before CI can be green. That run also confirms the two
  layout changes their authors flagged (lane 2's `relative/flex_1/min_h(0)`
  wrapper between the header and the row list, lane 3's Favorites section
  wrapper) are pixel-neutral. *This PR, from the orchestrating session.*
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
  tests assert. *M4, with the icon view's own scroll bookkeeping.*
- **Expansion state is never pruned when a folder leaves the listing.** The
  *selection* and the *cursor* are (a folder that vanished stops contributing
  its cached children — see `DirView::listing_ids`), and nothing invisible can
  be acted on any more, but `expanded`/`children` keep an entry for the dead
  folder until the pane navigates away. Harmless today: the projection only
  injects children of folders it actually paints. *Tidy up when M4 gives the
  expansion state a second consumer.*
- **The row menu's Paste pastes into the pane's directory, not into the
  right-clicked folder** (Explorer pastes into the folder). It dispatches the
  one `Paste` action; giving the menu its own destination would be a second
  implementation of paste. Resolving it properly means a parameterized
  `Paste { dest }`-style action so the single handler still owns the logic.
  *Revisit when the M8 menu bar forces the action set to be parameterized.*
- **Marquee edge autoscroll is rarely reachable in the details view.** Rows
  are full-width (Explorer's own details-view behavior), so the only empty
  space a marquee may start in is *below the last row* — which exists only
  when the listing is shorter than the viewport, i.e. when there is nothing to
  scroll. Autoscroll therefore only engages when the projection grows
  **mid-gesture** (an expanded folder's children landing — the path the
  `#[gpui::test]` drives — or a watcher patch) or the window shrinks under the
  drag. It is fully implemented per §8 and pays off in the M4 icon grid, whose
  empty space is plentiful; giving details rows Explorer's narrower
  (columns-only) hit region would make it routinely reachable in the list too.
  *M4.*
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
  the same `plan_drop` is a small follow-up. *M4 (with cross-pane drag).*
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
- **Process**: baseline commits are pushed with `GITHUB_TOKEN`, which never
  triggers CI — after baselines land, push a normal commit so the required `CI`
  check runs on the new HEAD. *Standing procedure.*

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
