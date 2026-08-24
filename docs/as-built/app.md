# As Built — `crates/app` (GPUI)

<!-- Split out of docs/AS_BUILT.md: that file is read by every agent on
every milestone, and the component detail had grown past 1,500 lines.
AS_BUILT.md stays the index (status, known gaps, deviations, change log);
this file carries the detail for one crate. Update both: the index's
change log row, and the relevant section here. -->

Back to the index: [docs/AS_BUILT.md](../AS_BUILT.md).

- `workspace.rs`: `Workspace` entity (refactor of the M0 `WorkspaceView`,
  which is deleted) — same chrome (titlebar; the info-panel column was a
  placeholder until M5 replaced it with the real `Entity<InfoPanel>`), root
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
  spliced beneath it with `depth + 1` — **in `ViewMode::List` only**: a tile
  has no indentation, no disclosure triangle, and `left`/`right` are 2D motion
  there, so children projected into the grid painted as ordinary top-level
  tiles of a folder they do not live in. `expanded` itself is untouched by the
  switch, so `cmd-1` restores the tree exactly as it was. `expanded: BTreeSet<Arc<Path>>` +
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
- `views/icon_grid.rs` + `pane.rs`'s `ViewMode` (M4, §0 "View mode switcher" /
  §8 "Icon grid"): `ViewMode { List, Icons }` lives on the **Pane** (§3
  "handlers live on the entity that owns the state"), which handles
  `SetViewList` / `SetViewIcons` from `cmd-1` / `cmd-2` **and** from a
  segmented toolbar control that dispatches the same boxed actions — so the
  switch logic exists exactly once and the M8 menu bar can reuse it.
  `SetViewColumns` is declared (the §0 table needs the row) but Miller columns
  stay a §8 stretch: the handler pushes a `JobsModel` notice
  (`COLUMNS_UNAVAILABLE_NOTICE`) and leaves the current mode alone, rather
  than being a menu item that silently does nothing — and the action is
  deliberately **unbound**, with a keymap test that fails if a binding
  appears.
  The grid is a `uniform_list` whose **items are grid rows**: each item lays
  out up to `cols` fixed-size tiles (`TILE_WIDTH` 96 × `TILE_HEIGHT` 88), so
  `ceil(n / cols)` items cover the listing and virtualization is untouched.
  `cols` is recomputed from the list's painted width (`cols_for_width`, floor,
  never zero). Every piece of geometry is a **pure function of `(cols, len)`**
  — `grid_row_count`, `row_items`, `step_index` (2D nav: ±1 / ±cols, no
  wrapping, `Down` from above a ragged last row lands on the last tile, stale
  indices clamped), `tile_at`, `tiles_in_rect` — and all of it is unit-tested
  without a window, because the cursor, the marquee and the drop-target hit
  test have to agree with the painted lattice.
  The grid renders **DirView's one path-keyed `SelectionModel`**, so switching
  mode preserves a multi-selection and the cursor and reloads nothing (a
  `#[gpui::test]` asserts exactly that). It reuses the details list's other
  behaviors rather than copying them: cut-dimming (`CUT_DIM_OPACITY`),
  theme-only colors, `handle_row_click` (double-click opens, cmd/shift select,
  slow second click arms rename), the drag payload + external Finder payload,
  the drop-target tint, and — via the new shared
  `rename::with_editor_actions` — the inline editor's focus/`TextInput`
  context/action wiring, which `views/details_list.rs` now also calls instead
  of spelling out twelve `on_action` forwards.
  Mode-dependent behavior on `DirView` is routed through three small readers:
  `view_mode(cx)` (read from the pane, never cached), `grid_cols()`, and
  `index_at_content()` — the single hit test shared by the marquee's
  empty-space rule, drag & drop's target arming and the context menu's target,
  so no two gestures can disagree about what the pointer is over.
  `up`/`down` step a whole line and PageUp/PageDown a viewport of lines
  (`rows * cols` entries); `right`/`left` are horizontal motion in the grid
  and expand/collapse in the list; `shift-left`/`shift-right`
  (`ExtendSelectionLeft`/`Right`) extend the range by **one tile**, which is
  the horizontal half of §0's "Cursor movement (+`shift-` extends)" and is
  deliberately inert in the details list, where a full-width row has nothing
  beside it to extend onto; `scroll_to_item` is called with `ix / cols`
  in the grid, because a `uniform_list` item is a *line* there. A switch
  between modes also scrolls the cursor back into view on the first frame
  painted in the new mode (`painted_mode`): the two views share one **pixel**
  scroll offset but measure their items differently (22px per entry vs 88px
  per `cols` entries), so the selection the switch preserves would otherwise
  often be off-screen with nothing to bring it back. The sortable
  column header paints only in list mode (the `SortBy` action still works — it
  lives on the pane). The tile's image slot (`ICON_PX` 48) paints the decoded
  thumbnail when there is one and a type glyph when there is not — the slot's
  size never depends on which, which is what lets a preview arrive mid-scroll
  without reflowing the grid or invalidating a hit test (see `thumbnails.rs`).
  A new `icon_grid` visual scenario (grid + a two-entry selection) is declared
  and **awaits its first baseline from the macOS runner**.
- `thumbnails.rs` (M4, §8 "Icon grid — generate only for visible+margin
  rows"): the **fifth field-shaped machine** on `DirView` (after rename,
  marquee, drop and menu). `ThumbnailState` holds fs-core's byte-budget
  `ThumbnailCache` (the decoded-RGBA source of truth), a small
  `HashMap<ThumbnailKey, Arc<RenderImage>>` of the GPU-side images the painted
  tiles actually reference, the set of keys with **no preview available**, and
  one `Task` slot.
  - **Where the window comes from**: the icon grid's `uniform_list` processor
    is the only code that knows what is on screen, so it calls
    `DirView::request_thumbnails(rows, cols, …)` with the grid-row range gpui
    asked it to paint; `request_window` (pure, unit-tested) widens that by
    `MARGIN_ROWS` = 1 line either side and converts it to entry indices,
    saturating throughout because a row range can outlive a re-projection.
    Folders are skipped — Explorer previews file *content*.
  - **Cancel on scroll-away**: one task fetches the whole window
    sequentially, and a window change *replaces* it — which drops it, and with
    it the in-flight `Platform::thumbnail` future for a tile that has left the
    viewport. Same single-slot pattern as the marquee's autoscroll ticker.
    Proven, not asserted-by-comment: a test installs a deliberately **slow**
    recording `Platform` (it parks on a `Spawner` timer), catches the top
    band's first fetch in flight, scrolls to the bottom, and shows that fetch
    *started and never finished* while the surviving window's did.
  - **Nothing blocking on the UI thread**: each call is awaited via
    `cx.background_executor().spawn`, so neither the QuickLook round-trip nor
    the `image` fallback decode can reach the render thread even if a
    `Platform` implementation forgot to unblock. Only the cache insert and the
    `notify` run on the UI thread.
  - **Idempotence** is what keeps this out of a render loop: an arriving
    thumbnail notifies, the next frame re-requests the *same* window, and a
    `fetching` flag makes that a no-op. A window that has since gained entries
    is picked up when the flag clears, rather than by cancelling live work.
  - **Two caches, on purpose**: the fs-core cache is the bounded bitmap store
    (64 MB), the image map is viewport-sized and pruned on every window move,
    with `cx.drop_image` handing each texture back rather than leaking an
    atlas slot for the life of the window. Scrolling back is therefore a
    re-upload, not a re-decode. Conversion is one pass:
    fs-core hands out non-premultiplied **RGBA**, `RenderImage` is **BGRA**
    (gpui's own loaders do the same swap), asserted by a unit test on a
    known pixel — a silent channel swap would look plausible and be wrong.
  - An `Err` from `Platform::thumbnail` is an ordinary "no preview": the key
    goes in `missing` (stamped, so an edited file is retried) and the tile
    keeps its glyph. Nothing is surfaced to the user and nothing retries in a
    loop, per the trait's contract.
- `info_panel.rs` (M5, §1 `info_panel.rs`, §9's M5 line): the `InfoPanel`
  entity that replaced the M0 placeholder — the right-hand column of
  `docs/requirements/Basic window.png`.
  - **One `Subject` at a time**, path-keyed (§2 invariant 2), never
    entry-keyed: `Nothing` (no folder open), `One { path, kind }` where `kind`
    distinguishes a selected entry from the open folder shown when the
    selection is empty, or `Many(SelectionSummary)`. Everything displayed
    about a `One` comes from the load, so the panel never has to be told which
    `FileEntry` a path resolved to — and the multi-selection case needs no
    load at all, because `fs_core::summarize` over the projection is pure.
  - **One load task, four cancellations.** The `Spawner::timer` debounce
    (130 ms), `Vfs::metadata`, `Platform::file_attrs` and — only when
    `fs_core::is_previewable` says so — `Platform::thumbnail` all live in a
    single `Task` slot, so a retarget drops the lot. Arrow-keying through a
    thousand rows costs one stat, not a thousand (unit-tested by counting the
    recording platform's calls). Same bounded-orphan caveat as
    `Platform::thumbnail`: work already handed to the background executor runs
    to completion and has its result discarded.
  - **Nothing blocking on the UI thread.** Both lookups and the preview fetch
    are awaited on `cx.background_executor().spawn`, so an `NSFileManager`
    round-trip or a QuickLook wait cannot land on the render thread even if a
    `Platform` impl forgets to `unblock`; `render` touches no disk API at all.
    A test with a platform that answers only when the fake clock advances
    proves the app still parks and stays interactive while a load is in
    flight.
  - **Whose selection: the active pane's.** `Workspace` holds one
    `cx.observe` per pane's `DirView` (parallel to `pane_subscriptions`, so a
    collapse drops the closed pane's with the pane) and pushes the *active*
    view down through `InfoPanel::follow`. `DirView` has no
    `SelectionChanged` event — a selection change is a `cx.notify()` — so this
    is an observe rather than a subscribe, and the filter that keeps a scroll,
    a hover or an arriving thumbnail from restarting the debounce is an O(1)
    `Witness`: the open directory, the pane's `ListingSnapshot` **by held
    `Arc` identity** (held, not reduced to a raw pointer — a dropped
    snapshot's address is free for the allocator to hand to its replacement),
    the expansion bookkeeping sizes, and the selection's size, cursor and
    extremes. Only when the witness moves is `projected_rows` built — the
    comparison happens **first**, before the subject is derived, which is the
    whole point of having a witness and which a review fix restored (it was
    deriving the subject eagerly, so every idle notify paid for a full
    projection); `an_idle_follow_does_not_build_the_projection` pins it against
    a thread-local `dir_view::projections_built()` probe. It is also why the
    subject is derived from `projected_rows` and not from `flat_rows`: the
    latter is the *last painted* projection, and an observer fires before the
    frame that would refresh it.
  - **A re-read keeps what is painted.** `retarget` clears `details` and the
    preview only when the subject actually *changes*. Re-describing the same
    path — which is what every watcher patch does, and patches arrive every
    `pane::WATCH_LATENCY` (100 ms), shorter than the 130 ms debounce — leaves
    the values on screen and swaps the new ones in when they land, so a
    download or a copy job in the open folder no longer pins the panel at em
    dashes (`repeated_relistings_keep_the_values_painted`). A preview is
    dropped mid-flight only if the fresh stat says the subject is no longer
    previewable. The cost is that a file being written *while selected* shows
    values from the last quiet moment; recorded as a Known gap.
  - **The values themselves are pure functions.** `header_text`,
    `general_rows` and `perm_matrix` take `(path, kind, meta, attrs)` and
    return text and a `[class][bit]` matrix, so the Path row's
    parent-vs-own-path rule, the em dash for every value before the load lands,
    a folder's absent Size (matching `details_list::size_cell`, which shows an
    em dash for `is_dir_like` rows — a directory's inode size is not the
    folder's size) and the R/W/X grid's orientation are all unit-tested rather
    than left to a baseline nobody opens.
  - `is_settled()` reports whether the stat, the attributes and (when the
    subject has one) the preview have all arrived. It exists for the visual
    runner, which asserts it before every capture.
  - **Sections** match the blueprint: a fixed-height preview slot (decoded
    preview or the type glyph — fixed so an arriving image reflows nothing),
    the name, `"<type description> — <size>"`, a collapsible **General**
    (containing folder for a selected item / its own path for the open folder,
    `format_size` = human + comma-grouped exact bytes, modified, created,
    added, extension, hide-extension, hidden) and a collapsible
    **Permissions** (R/W/X grid for owner/group/others, symbolic + boxed
    octal, owner and group as disabled dropdowns with the blueprint's `⌄`,
    locked). The permission controls are **read-only**
    for M5: drawn at 55% opacity with *no* click handlers, because a control
    that looks live and silently does nothing is worse than one that looks
    inert. `thumbnails::render_image` is shared rather than forked, and the
    panel keeps a single-slot image (plus a `retired` list drained in `render`,
    the only place a `Window` is available for `cx.drop_image`) instead of the
    icon grid's viewport-shaped cache.
  - `ToggleInfoPanel` (§0: `cmd-shift-i` + titlebar `ⓘ` dispatching the same
    boxed action, handled on `Workspace`). Hiding it removes the column and
    its splitter and leaves the pane strip's own state untouched (the strip is
    `flex_1`, the panel `flex_none`), and the panel is told to `clear()`, so a
    hidden panel stats nothing; showing it re-describes the current selection.
  - **Visual scenarios** (three, all `Theme::dark()`): `info_panel_jpeg` —
    §8's named M5 row and the milestone's acceptance criterion, a selected
    `/home/Pictures/photo.jpg` so the preview, "JPEG image — 24.0 KB", the
    `jpg` extension row and the `rw-rw-r-- 664` grid are pinned on an image;
    `info_panel_selection` — the same shape over `report.pdf` in a populated
    folder; `info_panel_multi_selection` — four selected entries, one of them
    a folder, so the summary's Items/Folders/Files/Total size are all
    non-trivial and the absence of General/Permissions is pinned too. Each
    advances the deterministic clock past `LOAD_DEBOUNCE` before capturing
    (`settle_info_panel` in the runner, called after every navigation **and**
    after every scenario's own gesture except `MarqueeActive`'s held drag,
    whose 30 ms autoscroll tick would walk the list). Settling after the
    navigation alone is not enough — a gesture that moves the cursor arms a
    fresh debounce, which is how `details_rename_editing` and `split_panes`
    came to capture a panel of em dashes — so `run_scenario` also asserts
    `InfoPanel::is_settled()` before capturing, and any future scenario that
    forgets is a hard failure rather than a plausible-looking baseline. The JPEG subject
    is inserted with `FakeVfs::insert_file` after `insert_tree`, not as a
    fixture key, because `FakeVfs` mtimes come from an insertion-order counter
    and a new key inside the tree would shift every node declared after it.
- `search.rs` (M6a, §0 "Search field focus", §8's TextInput row "reused by
  address bar, rename, **search**"): the toolbar search, in two halves in one
  file — the `SearchBar` entity and a `SearchState` machine that is a *field of
  the `Pane`* (the same field-not-an-entity shape as `rename`/`marquee`/`drop`
  on the `DirView`), with its methods in an `impl Pane` block at the bottom of
  the module.
  - **Where the state lives: the `Pane`.** One search per pane, so the M4 split
    gives two independent searches rather than one that silently retargets; and
    the pane already owns the snapshot the instant filter reads, the sort the
    results are presented in, and the `show_hidden` flag the walk takes as an
    argument. The pane's fields are `search: Option<SearchState>`,
    `_search_task: Option<Task<()>>`, `search_generation: u64` and
    `search_recursive: bool`.
  - **The field** is the vendored `InputState` (`⌕` glyph, placeholder
    "Search", a `✕` clear button and a "☐/☑ Subfolders" toggle that appears
    only once there is a query), rendered at the right-hand end of the *pane's*
    chrome row — the blueprint screenshot's top-right control. It carries
    `key_context("TextInput")` **and `track_focus` of the input's own handle**,
    because the vendored `InputState::render` does not track it itself and
    without that the node carrying the context is not on the focused element's
    dispatch path (§9's named silent-failure mode — `escape` was dead until
    this was added; `address_bar.rs` has the same omission and the same latent
    bug, untested there and left alone by this PR).
  - **Instant, folder-local filtering.** Typing runs `fs_core::filter_snapshot`
    — pure, no stat, nothing off-thread — inside the keystroke. The matching
    ids come back in snapshot order, so resolving them to entries is a single
    two-cursor pass with no lookup structure; the keystroke path allocates the
    id vector and the row vector and nothing else. A test asserts the recording
    `Vfs`'s `read_dir` count is **zero** across a filtering keystroke, both
    before and after the executor is allowed to run.
  - **Recursive, streamed.** "Subfolders" starts `fs_core::search_recursive`.
    The stream is polled inside `cx.background_spawn`; its events cross to the
    UI thread through an unbounded channel that the foreground task drains in
    `SEARCH_THROTTLE` (100 ms) batches: park on the first arrival, wait one
    window on `Spawner::timer`, drain everything queued, fold it in once. So a
    50k-hit walk repaints ~10×/s, not 50 000×. Progress is *coalesced* by
    fs-core (one event per 16 directories plus an exact final count), so the
    pane assigns `dirs_scanned` rather than incrementing it.
  - **Cancellation** is one `Task` slot. Dropping it drops the foreground pump,
    which drops the receiver *and* the background walk task held on its stack;
    the walk stops between directory reads. `search_generation` is belt and
    braces on top, so a batch resolved for a superseded query cannot apply. The
    slot is replaced on every query change and dropped on clear and on
    navigation. Proven with a `RecordingVfs` that parks on a timer before each
    `read_dir`: a walk caught mid-flight and retargeted contributes exactly one
    directory read and never resumes, and a walk abandoned by a navigation
    leaves the new folder's own listing as the *only* read that follows.
  - **A restart starts from nothing.** `restart_search` is the one funnel every
    respawn goes through, and it clears `hits`, `dirs_scanned`, `skipped` and
    `running` before deciding whether to spawn. All three of the walk's
    arguments — the query, the scope, the pane's `show_hidden` — change what
    counts as a hit, so anything the previous walk accumulated is stale by
    definition: keeping it is how a hidden hit survived the toggle that hid
    every other trace of it, how repeated toggles stacked duplicate hits, and
    how "N folders searched" summed two walks. Clearing `running` there is what
    makes "Subfolders off mid-walk" stop claiming a walk is running (the flag
    the visual runner's `settle_search` spins on, waiting for a `Done` that can
    no longer arrive).
  - **The accumulated hits are capped** at `MAX_SEARCH_HITS` (10 000), with
    "showing the first 10000 — narrow the search" on the status line. The
    per-batch dedupe and sort of the whole result set runs on the **UI thread**,
    so an uncapped walk over a big tree stopped the window painting — including
    the keystroke that would have cancelled it. It runs once per batch, too:
    `apply_search_batch` leaves the rebuild to `prune_view_state`, which has to
    re-derive the rows anyway before pruning the selection against them. A
    thread-local `ROWS_REBUILT` probe (the shape of `dir_view`'s
    `PROJECTIONS_BUILT`) pins "one batch, one rebuild".
  - **The results are the `DirView`'s projection**, not a second listing.
    `projected_rows` returns early with the pane's search rows when a query is
    live: flat, depth 0, no expansion splice (Explorer's search results are
    flat, and hits from other directories have no place in this folder's tree).
    "No disclosure triangles" is carried by `ProjectedRow::disclosure`, which
    the search branch sets `false` and `details_list` renders from instead of
    `is_dir_like()`, with `toggle_expanded` / `expand_selected` /
    `collapse_selected` no-ops while a search is live. Both halves are needed:
    the triangle used to paint on every folder row and do nothing visible while
    inserting expansion state and starting a child `read_dir`, and the state
    outlived the search — clearing the query brought the folder back
    pre-expanded over a stale cached listing, which is the bug
    `prune_expansion_state` exists to prevent. Everything downstream — marquee
    arithmetic, drop targets, the context menu's row band, the grid's
    `painted_cols`, thumbnail windowing, the scrollbar's content height, the
    info panel's witness — reads the projection and needed no change.
  - **Pruning.** `retain_selection_in_listing` now prunes the *selection*
    against the projection (the search rows when one is live) and the
    *expansion state* against the listing, always. A row the filter hides must
    not stay selected — `delete` would act on something invisible — while the
    tree the filter hides must come back untouched when the query is cleared.
    `listing_contains` (the pane's cursor-restore question) is search-aware for
    the same reason.
  - **Ordering / refresh rules.** Navigating to a different directory drops the
    query, the results, the scope and the field's text (Explorer's rule; a
    search is *of* a folder). An **in-place** reload — refresh, sort flip,
    hidden-files toggle — keeps the search and re-derives its rows from the new
    snapshot; the hidden toggle additionally restarts the walk, because
    `show_hidden` is an argument to it, while a sort flip does not, because the
    rows are sorted at presentation time. A **watcher patch** re-derives the
    rows too, which is what stops an externally created non-matching file from
    appearing (and lets a matching one in).
  - **Row provenance.** A hit outside the open folder renders its containing
    folder, relative to the searched root, muted beside the name in the details
    list. Explorer gives this a whole "Folder" column; the inline qualifier
    keeps the column-fit arithmetic and every existing baseline untouched — the
    column is a recorded gap, as is showing it on icon-grid tiles.
  - **Status line.** A live search replaces the pane's item *count*: `"N
    results for “needle”"`, plus `"· K folders scanned so far…/searched"` and
    `"· J skipped"` while recursive — the `Progress` and `Skipped` events,
    surfaced rather than swallowed. The free-space figure stays either way: it
    is a property of the volume, not of the query, and §3 puts it on the line
    unconditionally. `J` counts `Skipped` *events*, and fs-core emits one per
    unstattable entry as well as one per unreadable directory — a recorded gap.
  - **The empty state.** A query that matches nothing paints "No items match
    your search", not "Empty folder" — the folder is as full as it was
    (`DirView::empty_placeholder`, which the ordinary empty listing still reads
    for its own wording).
  - **Narrow panes.** The chrome row is `overflow_hidden`, its breadcrumb is
    `min_w(0)` and clips, and the field shrinks from 180 px to a 90 px floor:
    at `PANE_MIN_WIDTH`, or in a split pane with the info panel open, the
    breadcrumb + view switcher + field do not fit, and flex items default to
    their min-content width — so without all three the field drew over the
    splitter and the neighbouring pane (the M4 narrow-split failure class). The
    breadcrumb clips mid-word rather than eliding; that ellipsis is a recorded
    gap.
  - **The sticky scope lives on the pane, not the widget.** The field's text
    reaches the pane one effect-flush *later* than its toggle (the text travels
    `InputState` → `SearchBar` → `Pane`), so a toggle clicked before the text
    lands would be dropped against a `None` search. It was: the first
    `search_results` capture showed a lit "☑ Subfolders" and "0 results". Now
    `Pane::search_recursive` remembers it and a new query inherits it; the
    widget's own flag only draws the checkbox. Regression-tested, and the
    visual runner now `bail!`s if a search scenario has no result rows to
    capture rather than baking a plausible-looking empty pane into a baseline.
    The other direction is `clear_search`, which resets **both** halves: the
    pane's scope and, through `SearchBar::reset`, the field's mirror. Resetting
    only the pane's left the *next* query with a lit "☑ Subfolders" over a
    folder-local filter whose first click did nothing, because the pane already
    believed the scope off.
  - **The staged clear.** `Pane::load` is window-free and every navigation goes
    through it, but the vendored input cannot rewrite its own text without a
    `&mut Window`. So `SearchBar::reset` stages `pending_reset` and `render`
    (which has a window) applies it on the next paint.
  - **Visual scenarios.** Two, `search_filtered` and `search_results`, driven
    by `Setup::SearchActive(path, query, recursive)` — the same query (`"o"` in
    `/home`) with the toggle off and on, so the only difference between the two
    baselines is what the recursive walk adds. The first pins the focused field
    with text in it, the clear button, the unchecked toggle, a listing cut down
    to three folders and a file, and the count-only status line. The second
    pins the lit toggle, the finished `"5 folders searched"` line and **both**
    kinds of result row in one frame: the four local matches unlabelled, and
    four deeper hits (`Documents/notes.txt`, `Downloads/notes.txt`,
    `Documents/report.pdf`, `Pictures/photo.jpg`) each carrying its
    containing-folder qualifier, in the pane's sort order rather than the
    walk's arrival order. Both are driven *through the field* (focus, then
    `set_text`, then `set_recursive`) so the frame shows the real focused
    control. `settle_search` is the search's `settle_info_panel`: it advances
    the deterministic clock one `SEARCH_THROTTLE` window at a time until the
    pane reports `!SearchState::is_running()` — the flag the same batch that
    folds in `Done` clears, so once it is false every hit is already in `rows`
    — and then asserts four things: there *is* a search, it has rows, its
    **scope is the one the scenario asked for**, and its rows carry
    containing-folder labels iff that scope is recursive. Every one of them
    matters: a frame captured mid-walk would pin whichever prefix of the hits
    had landed plus a `"scanning so far…"` status, i.e. a race rather than a
    state; a search that found nothing captures an "Empty folder" pane that
    reads as entirely fine in review (the first `search_results` capture was a
    lit toggle over "0 results"); and if the text/toggle ordering ever regresses
    again, `search_results` captures the *folder-local* frame under a lit
    checkbox — which "there is a search with rows" happily accepts, and which
    the pixel compare cannot catch because the initial capture is the one moment
    there is no baseline to compare against. Running out of settle rounds is a
    failure, never a capture.
- `scrollbar.rs` (M4, §8 widget list "Auto-hide scrollbar"): a thin overlay,
  not a layout node — an absolutely-positioned child of the marquee's list
  surface (the same positioning context as the rubber band), so it reserves no
  width and shifts no row or tile, which matters because every mouse hit test
  is arithmetic over the painted band. `thumb(viewport, content, offset)` is
  pure and unit-tested: `None` when there is nothing to scroll (an auto-hide
  bar shows *no* chrome for a folder that fits), proportional height with a
  24px floor, clamped inside the track for over-scrolled or stale offsets, and
  NaN-safe. Visibility is driven by comparing the scroll offset between two
  frames in `render` (which deliberately does **not** notify — the frame it is
  deciding about is the one being built), and the fade is a single-slot
  `Spawner::timer` task, so a second scroll restarts the delay instead of
  letting the first expire. **Determinism**: opening a folder is not a scroll,
  so a captured scenario that never scrolls never shows a bar, and no baseline
  depends on when the screenshot was taken; the fade is a timer, not an
  animation, so there are exactly two states. `DirView::content_height(cx)` is
  the one mode-aware content-height calculation, now shared with the marquee's
  autoscroll clamp — which had been counting a grid in *row* heights and could
  therefore scroll a long way past the last line of tiles.
- `actions.rs` / `context_menu.rs` (M4): `Paste` is now parameterized —
  `Paste { dest: Option<PathBuf> }` (`no_json`, like `SortBy`). `None` means
  the pane's open directory (`cmd-v`, the background menu); the **row** menu
  passes the right-clicked folder, so Explorer's "paste into this folder"
  works through the one `DirView` handler rather than a second implementation
  of paste. `MenuFacts` carries the destination so the menu builders stay pure.
  Closes the Known-gaps entry that had been waiting on the M8 menu bar.
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
- `workspace.rs` (M4 dual pane, §0 `ToggleSplitPane` / §2 "Dual-pane readiness
  without PaneGroup"): `cmd-shift-o` (Workspace context) and a titlebar toolbar
  button — which **dispatches the same boxed action**, so the toggle logic
  exists once — grow `panes` from one entity to two and back. No recursive
  member tree: the flat `Vec` + `active_pane_ix` the plan reserved for this is
  what changed, plus a parallel `pane_subscriptions: Vec<Subscription>` so a
  closed pane's `PaneEvent` subscription dies with it instead of lingering.
  Decisions worth knowing:
  - **A fresh split seeds only two things**: the *directory* (copied from the
    active pane — a split whose new half said "No folder open" would make the
    user re-navigate to where they just were) and the *view mode*, set to
    `ViewMode::complement()` so the split lands as a details list beside an
    icon grid (plan §2's blueprint screenshot). Everything else — history,
    sort, selection, address bar, status line, scroll, `ListingCache` — is the
    new `Pane`'s own and starts empty. `show_hidden` stays workspace-global
    (§0 fans it out), so the new pane adopts the current value.
  - **Collapsing keeps the ACTIVE pane**, not pane 0: collapsing while you work
    in the right-hand pane must not throw away the directory you are looking
    at. The closed pane's state is *not* stashed for a later re-split (a
    resurrected pane pointing at a directory that has since changed is worse
    than a fresh one); dropping the entity drops its watch registration, load
    tasks and watch pump with it.
  - **The splitter** reuses the M2 hand-built approach: `first_pane_width:
    Option<f32>` where `None` = an even split (both panes `flex_1`, which is
    what a fresh split and every collapse reset to) and a drag pins a width.
    Its drag math is relative to the **pane strip**, not the body row, so the
    strip carries its own `on_drag_move` and each handler ignores the other's
    `SplitterSide` — gpui's drag-move listeners are not hover-gated, so both
    fire for every mouse move and would otherwise fight over one width with
    two different origins. `clamp_pane_width` (pure, unit-tested) keeps both
    panes ≥ `PANE_MIN_WIDTH` = 240; the setter rejects non-finite widths, which
    `f32::clamp` propagates rather than refusing.
  - **Which pane is active is visible**: while split, each pane wears a 2px
    marker above it, the active one in the theme accent. A focus ring inside a
    pane would be invisible when focus sits on a breadcrumb or status line, and
    "which pane does `cmd-z` act on" has to be answerable by looking.
  - **Active-pane tracking** now has tests, which found the sharp edge: gpui
    zeroes both focus paths of an **inactive** window, so no `focus_in`
    listener fires there — a test window starts inactive, and a test that
    focuses a pane must `window.activate_window()` first. Real usage always
    does (a click both activates the window and focuses the pane). Every
    workspace-level command already routed through `active_pane()`; the audit
    found no index-0 assumptions left in `workspace.rs`, `sidebar.rs`, `main.rs`
    or the visual runner, and the routing is now pinned by tests (`cmd-l`,
    `cmd-shift-.` fan-out, `shift-delete` on the active pane's selection).
  - **Cross-pane drag** needed no code: the payload is window-global, and
    ARCHITECTURE's claim is now a test — a real mouse drag from the left pane
    into the right one **moves** (one volume, per `drop_copies`), with the
    destination pane arming the target and the source pane clearing its own.
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
