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

**M6 complete in code** — search (M6a) and now tags + permission editing
(M6b), on top of M1–M5. M6's acceptance criterion ("tagging a file here shows
in Finder and vice versa") is verified by `fs-core/tests/tags.rs` against
Apple's own `xattr`/`plutil` and Foundation's `NSURLTagNamesKey`; the
Finder-side half of it is on the manual Mac checklist (see Known gaps).

* **Search in fs-core** — `search.rs`: `SearchQuery` (trimmed,
  pre-lowercased, case-insensitive substring on the *name*), the pure
  `filter_snapshot` an instant keystroke filter can call on the UI thread, and
  `search_recursive` — a breadth-first, bounded-concurrency, cycle-safe
  streamed walk emitting `Hit`/`Progress`/`Skipped`/`Done` and cancelled by
  dropping the stream.
* **The toolbar field** — `crates/app/src/search.rs`: a `SearchBar` over the
  vendored `TextInput` at the right-hand end of each pane's chrome row
  (`docs/requirements/Basic window.png`'s top-right control), with a clear
  button and a "Subfolders" toggle. `FocusSearch` (`cmd-f`) is handled by the
  `Workspace` and forwarded to the active pane, exactly like `cmd-l`.
* **Where the state lives** — the **`Pane`**, one search each, so the M4 split
  searches independently. The results become the `DirView`'s projection, so
  the marquee, drag & drop, context menus, the icon grid, thumbnails, the
  scrollbar and the info panel all keep working with no knowledge of search.
* **Streaming without repaint storms** — the recursive walk is polled on the
  background executor and its events are folded in on a 100 ms
  `Spawner::timer` batch, in one cancellable `Task` slot (the
  `info_panel.rs`/`thumbnails.rs` shape). Progress and skipped-directory
  counts show in the pane's status line.
* **The rules** — `escape` or an empty field clears the search; navigating to
  another folder drops it (query, results, scope and text); an in-place reload
  (refresh, sort flip, hidden toggle) keeps it and re-derives the rows; a
  watcher patch re-derives them too, so it can never unfilter.

Two new visual scenarios pin the search: `search_filtered` (the instant
folder-local filter, "Subfolders" unchecked) and `search_results` (the same
query with the toggle lit, so one frame carries the local matches unlabelled
beside deeper hits with their containing-folder qualifier, and the finished
"N folders searched" status line). **Neither has a committed baseline yet, and
the field repaints every other scenario** — see Known gaps.

M5 remains as built below — on top of M1–M4 (read-only browsing, the sidebar
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
panel mid-load). Three more have run over M6a; see the M6a review-fixes row.
441 tests green (151 fs-core unit + 6 fs-core integration + 284 app), and 19
visual scenarios — two of which have no committed baseline yet (see Known
gaps).

| Milestone | State |
|---|---|
| M0 skeleton + visual-test infra | ✅ merged (#1) |
| Phase A architecture | ✅ merged (#3) |
| M1 read-only browsing | ✅ merged (#4) |
| M2 sidebar + in-place expansion | ✅ merged (#5) |
| M3 file operations | ✅ engine + job spine + keyboard ops + inline rename/duplicate + marquee + drag & drop + context menus + review fixes; all baselines committed |
| M4 icon view + dual pane | ✅ complete — fs-core thumbnails, icon view + view-mode switcher, dual pane, grid thumbnails (visible+margin, cancel-on-scroll-away), the auto-hide scrollbar, and the narrow-pane column fit. All 14 visual baselines regenerated on the macOS runner (the titlebar's split-pane button changed every existing scenario) |
| M5 info panel | ✅ code complete — `fs-core::attrs` (`UnixPerms`, `FileAttrs`, `SelectionSummary`, `is_previewable`) + `Platform::file_attrs`; `crates/app/src/info_panel.rs` with the debounced single-slot load, the preview, the General and Permissions sections and the multi-selection summary; `ToggleInfoPanel` (`cmd-shift-i` + titlebar button); three new visual scenarios (`info_panel_jpeg`, `info_panel_selection`, `info_panel_multi_selection`). All 17 visual baselines were regenerated on the macOS runner and merged with the milestone (#12), so every declared M5 scenario has a committed baseline |
| M6a search | ✅ code complete — `fs-core::search` (`SearchQuery`, `filter_snapshot`, `search_recursive`); `crates/app/src/search.rs` (`SearchBar` + the pane's `SearchState`, throttled streaming, single-slot cancellation); `FocusSearch` (`cmd-f`); results as the `DirView` projection; two new visual scenarios (`search_filtered`, `search_results`); three adversarial reviews applied (see the change log). all its baselines were regenerated with M6b's (see the M6b row) |
| M6b tags + permission editing | ✅ code complete — fs-core: `tags.rs` (the `Tag`/`TagColor` model and the xattr codec) + `Platform::read_tags`/`write_tags`/`known_tags`, and `FileOp::Chmod`/`Chown`/`SetTags` as first-class undoable ops on the existing job spine (per-path failures in `OpReceipt::failed`, exact previous values in `restored_attrs`, undo guarded by `AttrGuard` because an mtime `Fingerprint` structurally cannot see a chmod). App: `crates/app/src/tags.rs` (row dots on the thumbnail loader's window shape, the sidebar **Tags** section and its filter through `Pane::filtered_rows`, the `Tags ▸` submenu writing `SetTags`), and the info panel's Permissions section made **live** — the R/W/X grid, the octal field and Owner/Group all submit `Chmod`/`Chown` **through the job queue**, so every permission change is undoable with `cmd-z`. Two new visual scenarios (`info_panel_permissions`, `tag_filter`). **All 21 visual baselines need regenerating** — see Known gaps |
| M7 → M8 ship | not started |

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

- **M6b: Owner and Group are name fields, not the blueprint's dropdowns.**
  Listing the machine's accounts needs a directory-service enumeration
  `Platform` does not have (it can *set* an owner by name, not list
  candidates), and a dropdown whose only row is the current owner could not
  change anything — so the info panel edits them as text and the chevron is
  gone rather than decorative. A `Platform::known_owners`/`known_groups` pair
  (and the popup `context_menu.rs` already knows how to draw) would close it.
  *M7 or later; the functional requirement — "owner/group change where
  privileged" — is met.*
- **M6b: a refused `chmod` shows only as a toast.** The panel deliberately
  does not paint optimistically, so a denied change leaves the grid exactly as
  it was and the only signal is the job toast ("changed 0 of 1 — …"). That is
  honest but quiet; Finder shows an alert. *M7 polish.*
- **M6b: the two new visual scenarios have no baseline, and every existing one
  is stale.** The sidebar grew a **Tags** section, the fixture seeds two
  tagged entries (so dots appear in every `/home` frame) and the permission
  grid's checkboxes are drawn at full strength now that they are live — so all
  19 committed baselines moved, plus `info_panel_permissions` and `tag_filter`
  have none. One all-or-nothing runner run: `gh workflow run
  update-visual-baselines.yml --ref <branch>`.
- **M6b: nothing pins the permission grid mid-flight or after a refusal.**
  Both are job-state frames rather than settled UI states, and the runner
  captures settled states by construction.
- **M6b: a tag name whose own last line is a bare integer loses that line.**
  `decode_tag_strings` splits at the **last** newline, which is what makes an
  ordinary multi-line tag name survive both directions; the cost is that
  `"odd\n5"` tagged red decodes back as `"odd"` + colour 5 twice over. Finder's
  own encoding has the same ambiguity (there is no escape in the format), so
  matching it is the compatible choice. Pinned by
  `decode_treats_a_non_numeric_trailing_line_as_part_of_the_name` so it is a
  recorded limit rather than a surprise.
- **M6b: user-defined Finder tags come back uncoloured from `known_tags`.**
  `~/Library/Preferences/com.apple.finder.plist`'s `FavoriteTagNames` gives the
  user's tag *names* only; the name→colour assignments live in the
  SyncedPreferences store, whose format is undocumented. A favourite that is
  not one of the seven standard colour names therefore has no dot in the
  sidebar until a tagged file reveals its colour. Merging colours in from the
  tags actually seen on disk is the cheap fix, and belongs to the sidebar lane.
- **M6b: legacy Finder *label* colours (`com.apple.FinderInfo`) are not read.**
  Finder still shows a file coloured by the pre-Mavericks label mechanism as
  though it were tagged, and `Finder`'s AppleScript `label index` writes exactly
  that — verified on this Mac: setting a label index leaves
  `com.apple.FinderInfo` and no `_kMDItemUserTags` at all, so such a file reads
  as untagged here. Writes are unaffected (we write what modern Finder writes);
  only display of legacy-labelled files is short. Reading the FinderInfo colour
  byte as a fallback is a contained addition to `read_tags_blocking`.
- **M6b: Spotlight's view of our tags could not be verified from this shell.**
  The check the acceptance criterion would love — `mdls -name kMDItemUserTags`
  on a file we tagged — returns `(null)` here even for `kMDItemFSName`, i.e.
  this shell gets nothing out of the Spotlight index at all (no Full Disk
  Access), not "the tag is missing". The xattr bytes were verified with `xattr`
  and `plutil` and the tags with Foundation's own `NSURLTagNamesKey` instead,
  which is the stronger check anyway. **On the manual Mac checklist:** tag a
  file in the app, open the enclosing folder in Finder, confirm the dot and the
  tag name in Get Info; then tag another file in Finder and confirm the app
  shows it.
- **M6b: cancelling an attribute job mid-selection leaves what it already
  applied, with no undo entry.** `Cancelled` carries no receipt (the spine's
  contract for every op), so a `Chmod` over 200 files cancelled at file 100
  leaves 100 changed modes and nothing on the undo stack. Identical to the
  existing behaviour of a cancelled multi-file `Move`, and left identical on
  purpose rather than special-cased; the honest fix is a receipt on
  `Cancelled`, which is an op-spine change worth its own PR.
- **M6b: `Chmod`/`SetTags` undo guards are read-then-act, not atomic.** The
  guard reads the mode (or tag set) back, finds it unchanged, and *then* submits
  the inverse op; a change landing in that window is overwritten. The window is
  the same one every fingerprint-guarded undo has had since M3, and closing it
  needs a compare-and-set at the syscall level that `chmod` does not offer.
- **M6b: `Vfs::mode`/`set_mode` follow symlinks, `Platform::file_attrs` does
  not.** The chmod pair must describe one inode or an undo would write a link's
  mode onto its target, so it follows the link (what `chmod(1)` does); the info
  panel `lstat`s, because it describes the item the user clicked. For a symlink
  the panel therefore shows `lrwxrwxrwx` while the permission editor edits the
  target's mode. The panel lane should either say so in the UI or refuse to edit
  a symlink's permissions.
- **M6b: `Chown` needs privilege and does not ask for it.** An unprivileged run
  gets EPERM per path and reports it; there is no authorization prompt
  (`SMJobBless`/`AuthorizationExecuteWithPrivileges`), so "owner/group change
  where privileged" means "works when already privileged". Changing the *group*
  to one the user belongs to does work unprivileged, which is the common case.
- **M6a: there is no "Folder" column for search results.** Explorer devotes a
  whole column to a hit's containing folder; we render it muted beside the name
  in the details list instead, so the M4 narrow-pane column-fit arithmetic
  (`visible_columns`) and every existing baseline stay untouched. The icon
  grid's tiles show only the name, so a recursive hit is unlocatable there
  until you switch to the list. *A real column is M6b/M7 polish; the grid label
  is the smaller half of it.*
- **M6a: a recursive search re-sorts and re-deduplicates its whole result set
  on every 100 ms batch.** `SearchState::rebuild_rows` rebuilds the row vector
  from the snapshot filter plus the accumulated hits, which is O(hits log hits)
  plus a `HashSet` of that size, ten times a second. It now runs **once** per
  batch (it used to run twice — `apply_search_batch` rebuilt and then
  `prune_view_state` rebuilt again) and `hits` is capped at
  `search::MAX_SEARCH_HITS` (10 000, with "showing the first 10000 — narrow the
  search" on the status line), which bounds that cost whatever the tree does.
  *The remaining fix is an incremental sorted insert per batch (the comparator
  already gives the index, exactly as `patch_listing` does). M7 polish, and only
  if a profile asks for it.*
- **M6a: past the hit cap the walk keeps walking.** Reaching
  `MAX_SEARCH_HITS` stops hits being *accumulated*, not the directory reads:
  the walk's progress and `Skipped` reports still mean something, and a task
  cannot cleanly drop itself from inside its own `update`. So a huge tree still
  costs I/O after the row list has stopped growing. *M7, together with a
  `SearchEvent`-level stop that the pane can ask for.*
- **M6a: `Skipped` counts reports, not distinct paths.** `fs-core` emits one
  `SearchEvent::Skipped` per unreadable directory, per over-deep directory,
  per suspected cycle **and** per entry whose stat failed (tagged with the
  containing directory), and the pane's "N skipped" counts events. One
  directory holding a hundred unstattable children therefore reads as "100
  skipped". *The fix is a distinct variant for per-entry failures, or counting
  distinct paths; M6b, with the rest of the status line's wording.*
- **M6a: the cycle heuristic is path-shaped, not identity-based.** A real
  directory that aliases an ancestor (macOS firmlinks, some network mounts) is
  caught by `fs_core::search::looks_like_a_directory_cycle` — the path's tail
  repeating three times over — because `MAX_DEPTH` alone turned one loop into
  ~21 complete re-walks of the volume, every match re-reported under each alias
  path. Two consequences: a loop still costs two laps before it is cut, and a
  genuine tree containing `a/b/a/b/a/b` is skipped (loudly — it arrives as
  `Skipped`) when it should not be. *The exact fix is a visited set keyed on
  device+inode, which needs a new `Platform`/`Vfs` seam to stay portable;
  deliberately not opened inside M6a.*
- **M6a: a search result row cannot be located by revealing it in place.**
  Double-clicking a hit opens it (a folder navigates into it, which also drops
  the search) but there is no "show in enclosing folder" for a file hit —
  Explorer's context-menu row. *M6b, beside the tag rows the same menu grows.*
- **M6a: which reloads keep the search.** The rule keys on the *path* changing:
  navigation to a different folder drops the query, the results, the scope and
  the field's text (Explorer's rule, and tested), while an **in-place** reload —
  refresh, sort flip, hidden-files toggle, and a cache-miss `GoBack`/`GoForward`
  into the same directory — keeps the search and re-derives its rows from the
  new snapshot. Noted so a future reader does not read either half as an
  oversight. *(An earlier version of this bullet claimed the opposite about
  back/forward; the code and `a_refresh_keeps_the_search_and_re_derives_its_rows`
  say what is written here.)*
- **`address_bar.rs` has the `track_focus` omission M6a fixed in the search
  field.** Its `TextInput` node carries the key context but not the input's
  focus handle, so `escape`/`enter` in the address bar reach it only because
  something else in the chain happens to be focused — the exact §9
  silent-failure mode. The address bar's tests drive `confirm`/`cancel`
  directly rather than through keystrokes, so nothing catches it. *Left alone
  deliberately: it is outside M6a's blast radius and deserves its own change
  plus a keystroke test. M7 chrome pass.*
- **Every committed visual baseline needs regenerating for M6a, exactly as it
  did for M4's split-pane button.** The search field paints in *every* pane's
  chrome row, so all 17 committed baselines moved, and `search_filtered` and
  `search_results` are an eighteenth and nineteenth scenario with **no
  committed baseline at all** — which hard-fails the macOS visual job, so the
  run is all-or-nothing: `gh workflow run update-visual-baselines.yml --ref
  m6a-search`, then **open the nineteen PNGs and look at them** (definition of
  done item 7). The two search frames were inspected from a local render
  first — `search_filtered` shows the focused field holding "o", `✕`, an
  unchecked "☐ Subfolders", four rows and "4 results for “o”";
  `search_results` shows "☑ Subfolders", eight rows with `Documents` /
  `Downloads` / `Pictures` qualifiers on the four deeper ones, and
  "8 results for “o” · 5 folders searched" (searched, not "scanning so
  far…"). A local Mac diverges from the runner image, so those renders prove
  the UI paints, not the pixels. *This PR.*
- **M6a: no baseline pins a narrow pane with a live search.** The chrome row is
  now clipped (`overflow_hidden`) with a shrinkable breadcrumb and a search
  field that gives way from 180 px to a 90 px floor, so a split pane with the
  info panel open keeps every control inside its own pane instead of drawing
  over the splitter — verified by opening the regenerated `split_panes` render,
  where the right pane's breadcrumb clips mid-word (no ellipsis; a `truncate()`
  on the segment row would need the segments to be one text run). But both
  search scenarios are single-pane and full-width, so nothing pins
  split + info panel + active search. *M7 chrome pass, with the ellipsis.*
- **No baseline pins a search that found nothing, a search with a `Skipped`
  directory, or search results in the icon grid.** All three are deliberate:
  the empty-result and skipped-directory status lines are covered by
  `status_text_reports_counts_progress_and_skips` (a pure unit test over the
  formatting) and, for the empty case, by
  `a_query_that_matches_nothing_says_so_instead_of_claiming_the_folder_is_empty`
  over `DirView::empty_placeholder`, and `FakeVfs` has no
  unreadable-directory fixture to drive the
  skipped case through the UI at all; the icon grid's tiles show no
  containing-folder label (the gap above), so a grid search scenario would pin
  the missing half rather than the feature. *M6b/M7, with the "Folder" column
  and the grid label.*
- **No baseline pins a search mid-walk, and by design cannot.** `settle_search`
  waits for `!SearchState::is_running()` before every search capture, so the
  streaming half — the throttled batches, the "N folders scanned so far…"
  status — is proved only by the `#[gpui::test]`s over fake time
  (`recursive_search_streams_hits_in_on_the_throttle`,
  `rapid_query_changes_leave_exactly_one_walk_running`). A frame captured
  part-way through a walk would pin whichever prefix of the hits had landed,
  which is a race, not a state. *Accepted; the fix if a picture is ever wanted
  is a fixture `Vfs` that parks a specific directory read forever, the shape
  `conflict_dialog` already uses for a parked job.*
- ~~**Every committed visual baseline needs regenerating for M5, not just the
  three new info-panel scenarios**~~ **closed (M5, #12)**: all seventeen were
  regenerated on the macOS runner and merged with the milestone, so every
  declared M5 scenario has a committed baseline. (M6a makes all seventeen stale
  again — see the M6a entry above.) What the run had to cover, kept here
  because the mechanics recur every milestone: a scenario with no committed
  baseline **hard-fails** the macOS visual job, so the run is
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
  quietly bake a half-loaded panel into a baseline again. All seventeen PNGs
  were opened and looked at before the milestone merged (definition of done
  item 7) — which is how the "panel taller than the window" gap below got its
  real wording. *Closed (M5, #12).*
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
- **The panel is taller than the window, so the bottom of the Permissions
  section is off every baseline.** Verified by opening the regenerated runner
  PNGs (the earlier wording here was written from local renders and understated
  it): at the fixed 1200×760 capture size a fully expanded single-entry panel
  runs past the bottom edge, and the last row that renders is **"Others"** —
  `info_panel_jpeg` and `details_rename_editing` cut off the **octal field, the
  owner and group dropdowns and the "Locked" row entirely**, and `split_panes`
  cuts off one row earlier still (it ends at "Group", because its breadcrumb row
  is taller). So four of the panel's Permissions fields — including the two the
  M5 review reshaped into dropdowns — are pinned by **no** baseline at all, and
  a regression in them would pass CI. The column is `overflow_y_scroll`, so all
  of it is reachable in the app; this is a coverage hole, not a broken panel.
  Nor does any baseline pin the panel with its sections *collapsed*, hidden, or
  in the light theme (`workspace_light` has no folder open, so it only shows the
  empty state). The cheap fix is a scenario that captures the panel scrolled to
  its bottom, or one with `General` collapsed so `Permissions` fits.
  *M6, which makes exactly those fields editable and must pin them.*
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
| 2026-08-28 | — | M6b, app lane (permission editing) + milestone close-out. **The Permissions section does what M5 only drew:** clicking one box in the R/W/X grid submits the whole flipped mode (`toggled_mode`, unit-tested to preserve the setuid/setgid/sticky bits the nine-box grid cannot show and to mask the file-type bits off), the octal box and the Owner/Group boxes open the vendored inline editor, and every one of them writes **through the job queue** as `FileOp::Chmod`/`Chown` — the panel owns no `Vfs` or `Platform` call at all, so a permission change is undoable with `cmd-z`, off the UI thread, and reported by the same toast machinery as a copy (`a_permission_change_is_undoable_like_every_other_file_operation`). `parse_octal_mode` accepts one to four octal digits and **nothing else**: junk closes the editor and writes nothing rather than guessing, because guessing at `"7778"` is how a file ends up world-writable. **Deliberately not optimistic:** `chmod`'s honest failure mode is refusal, so the grid keeps showing what was last *read* until the job completes — and since an attribute write moves no directory entry and no mtime, no watcher can see it, so `Workspace::handle_jobs_event` re-reads the panel's subject (`InfoPanel::reload`) for any receipt with non-tag `restored_attrs`, which is what the owner test actually asserts end to end. **Reuse over a third copy:** `rename::with_editor_actions` is now generic over the view and takes its confirm/cancel as function items, so the info panel's editor shares the rename overlay's dispatch node (focus tracking, `TextInput` key context, the twelve forwarded editing actions) instead of hand-copying it — `search.rs`'s older copy is left alone. `checkbox(theme, checked, live)` gained the liveness as a *visual* argument: full strength when the control writes, `DISABLED_ALPHA` for Hide Extension/Hidden/Locked, and nothing at all is live before the load lands (a click on a grid of em dashes would submit a mode nobody chose). **Deviation:** Owner and Group are name fields rather than the blueprint's dropdowns — `Platform` can set an owner by name but cannot enumerate accounts, and a one-row popup is not a control; recorded as a gap. Two new visual scenarios, both rendered locally and **opened** — and the first regenerated `info_panel_permissions` baseline was rejected on that inspection: with **General** open, the octal field, Owner and Group sit below the capture's window edge, so the "open editor" scenario had no editor in it. The setup now collapses General first (`InfoPanel::set_general_open`), which is what makes the whole section fit: `info_panel_permissions` (a tagged file selected, the grid live, the octal editor open — a state that otherwise exists only while a human holds the mouse still) and `tag_filter` (the sidebar's Tags section with Red active, the filtered rows, the `2 items tagged “Red”` status line). The fixture now seeds two tagged entries, so tag dots are pinned by every `/home` frame rather than only the new one; combined with the sidebar's new section and the full-strength checkboxes, **all 21 baselines need one runner run**. `FakeVfs` gained a sync `mode_of` accessor (a `#[gpui::test]` cannot await `Vfs::mode`), and `FAKE_FILE_MODE`/`FAKE_DIR_MODE` are re-exported with it. Also fixed here: five clippy errors left in the tree by the earlier lanes (`unnecessary_to_owned` in `context_menu.rs`/`sidebar.rs`, `cloned_ref_to_slice_refs` in `tags.rs`). 525 tests (194 fs-core unit + 21 fs-core integration + 310 app), 9 new in this lane (2 unit for the two pure helpers, 7 `#[gpui::test]`: the click path, undo, the octal editor's commit/escape, junk rejection, the chown round trip through the workspace re-read, the no-op commit, and nothing-editable-before-the-load). |
| 2026-08-24 | — | M6b, fs-core ops/undo lane: `FileOp::Chmod`/`Chown`/`SetTags` as first-class undoable ops on the **existing** job spine — same lanes, same cancel flag, same `OpReceipt`, one shared `run_attrs` loop driven by a private `AttrChange` enum. **Mutation surface split by what the change actually is:** a unix mode is file I/O, so `Vfs::mode`/`set_mode` sit beside `remove`/`rename` (defaulted on the trait — `Ok(None)` / an explicit "cannot change permissions" error — so the app's test-double `Vfs` keeps compiling, overridden by `RealVfs` and modelled by `FakeVfs`, which now carries a per-node mode); resolving an owner *name* to a uid is a directory-service lookup, so `Platform::set_ownership` is a platform method (`NSFileManager setAttributes:ofItemAtPath:error:` with the `NSFileOwnerAccountName`/`NSFileGroupOwnerAccountName` keys — the same API `file_attrs` already reads through, so no `getpwnam` and no `libc`). The queue therefore needs a `Platform`: `JobQueue::new` is unchanged and a new `JobQueue::with_platform` adds it, so a platformless queue runs everything else and fails only the two OS-service ops, loudly. **Both `mode` and `set_mode` follow symlinks** — they must name one inode or an undo would write a link's mode onto its target — which diverges from `file_attrs`' `lstat`; recorded as a gap. **Exact undo:** every op captures the previous value *before* it writes (`OpReceipt::restored_attrs: Vec<(PathBuf, PrevAttrs)>`), and `UndoEntry::from_receipt` groups those into one inverse op per distinct previous value, so a mixed selection (644 here, 755 there) comes back exactly as it was instead of being flattened. **Guarding is the interesting part:** `chmod` changes ctime, **not** mtime, so the existing `(path, mtime)` `Fingerprint` is structurally blind to exactly the change these ops make — a new `AttrGuard { path, expected: PrevAttrs }` guards the dimension the op wrote (mode / owner+group / tag set), reading it back through the `Vfs` and the queue's `Platform` at undo time and yielding `UndoOutcome::Invalidated { reason: "'x' permissions changed since" }` rather than clobbering a newer value; guards cover only the paths that *actually changed*, and for `Chown` only the halves the op set. Residual risk, documented as gaps: the read-then-act window, and a mid-job cancel leaving applied changes with no undo entry (identical to a cancelled multi-file `Move`, deliberately not special-cased). **Partial failure deliberately deviates from the rest of the spine:** copy/move fail the whole job on the first error and a `Failed` job records no undo entry, which for a half-applied chmod over a big selection would mean no way back — so these ops attempt every path, record failures in the new `OpReceipt::failed: Vec<(PathBuf, String)>`, complete as long as one path changed (keeping that half undoable), and fail outright only when nothing changed. The app must surface `failed` as a "changed 3 of 5" toast. **A real macOS finding:** `setAttributes:` reports **success** for an account name it cannot resolve and silently leaves that half alone (verified on this Mac with a nonexistent group), so `set_ownership_blocking` reads the ownership back and fails if the request was ignored — otherwise the panel would show a change that never happened and undo would record it. `StubPlatform` models ownership as storage layered over `file_attrs` and refuses `STUB_PRIVILEGED_OWNER` ("root"), giving the EPERM path a deterministic test on every OS; the same constant makes one assertion cover both the real macOS refusal and the stub's. **No `Cargo.toml` change in this lane.** 215 fs-core tests, 33 new (27 unit: the `AttrChange` spine, masking, denied/vanished/all-failed, cancel, platformless queue, mixed-selection undo + redo, both invalidation reasons, the guard/inverse unit checks, the `FakeVfs` mtime-asymmetry pin, `RealVfs` real-chmod + symlink-follow, stub ownership, and four macOS `set_ownership` tests including the real EPERM refusal; 6 integration in the new `tests/attr_ops.rs` over a real `tempfile` tree with `MacPlatform`, so the tag legs really write and undo the xattr), plus the "every `FileOp` variant" torture test extended with all three ops. **Known breakage for the app lane** (this lane touched no app code): `crates/app/src/jobs_model.rs:97` and `:112` match `JobKind` exhaustively and now miss `Chmod`/`Chown`/`SetTags`, and the two test-only `impl Platform` doubles (`thumbnails.rs:484`, `info_panel.rs:1290`) now also miss `set_ownership` on top of the three tag methods. |
| 2026-08-24 | — | M6b tags, fs-core lane: new `crates/fs-core/src/tags.rs` (`Tag`, `TagColor` with the on-disk palette indices, `rgba`/`standard_name`/`PALETTE`/`standard_tags`, and the pure `encode_tag_strings`/`decode_tag_strings` codec) plus `Platform::read_tags`/`write_tags`/`known_tags` on macOS and on the stub. **The format is the milestone**, so the codec is pure and exhaustively tested (bare names, out-of-range colour indices, non-numeric trailing lines, blanks, duplicates, non-ASCII, empty arrays) and the palette's discriminants are pinned by a test that fails on any renumbering — they are on-disk values, and reordering the enum would recolour every tagged file on the user's disk. **Two mechanism choices, both avoiding a new crate** (a dependency change costs a full silent workspace rebuild): `getxattr`/`setxattr`/`removexattr` are declared in a private `mod xattr` instead of pulling in the `xattr` crate or `libc` — the precedent set by the hand-written `UF_IMMUTABLE` — and the plist goes through `NSPropertyListSerialization` from the already-present `objc2-foundation`, writing **binary** (what Finder writes) and letting Foundation sniff binary-or-XML on read. That did need `Cargo.toml`: two extra *features* on `objc2-foundation` (`NSData`, `NSPropertyList`), no new dependency — **the next lane starts on a cold cache**. `ENOATTR`/`ENOTSUP` mean "no tags", `ERANGE` is retried, paths go through `OsStrExt::as_bytes` (a lossy path would tag a different file), symlinks are followed (a mode belongs to the link, a tag to the item), an empty set removes the xattr, and everything blocking is inside one `SpawnerExt::unblock` per call. **Acceptance criterion, proven four independent ways on this Mac:** `tests/tags.rs` writes tags and reads the bytes back with Apple's `xattr -px` (asserting `bplist00`) and `plutil -convert xml1` (asserting the exact `<string>Red\n6</string>` / `Wörk\n0` / `Später\n3` array), then reverses it with `plutil -convert binary1` + `xattr -wx` and with a hand-written XML payload via `xattr -w`; and two tests in `macos.rs` cross-check **Foundation's own public tag API** (`NSURL`'s `NSURLTagNamesKey`) in both directions — a code path sharing nothing with our reader, which is what makes the agreement mean something. All pass. The stub stores tags in a `BTreeMap` rather than deriving them from a path hash, deliberately: tags are the one thing the app *writes*, and a hash-derived answer over a `tempfile` path is exactly the shape of the M5 off-macOS flake. 192 fs-core tests (167 unit + 25 integration), 25 of them new here (13 codec, 1 stub, 2 Foundation cross-checks, 9 in `tests/tags.rs`). **Known breakage for the next lane:** the two test-only `impl fs_core::Platform` doubles in `crates/app` (`thumbnails.rs:484`, `info_panel.rs:1290`) now miss three trait methods — three-line additions each, left to the app lane per this lane's boundary. Gaps recorded: the last-newline ambiguity, uncoloured user favourites, legacy `com.apple.FinderInfo` labels not read, and `mdls` being unusable from this shell (manual Finder check on the Mac checklist instead). |
| 2026-08-24 | — | M6a review fixes (three adversarial reviewers, 20 findings). **Blockers/majors fixed.** `restart_search` now clears the previous walk's `hits`, `dirs_scanned`, `skipped` and `running` — every argument to the walk (query, scope, `show_hidden`) is what changed, so everything it accumulated is stale by definition. Before, the hidden-files toggle restarted the walk but kept its hits, so a hidden hit stayed in the results after every other surface stopped showing hidden entries, non-hidden hits were re-appended on each toggle, and the folder count summed two walks (`a_hidden_toggle_during_a_recursive_search_re_walks_from_scratch`); and turning "Subfolders" off mid-walk cancelled the walk without clearing `running`, so `is_running()` — the predicate `settle_search` spins on — waited forever for a `Done` that could not arrive (`turning_subfolders_off_mid_walk_stops_reporting_a_running_walk`). `clear_search` now resets **both** halves of the sticky scope: it stages the field's reset too, so emptying the field can no longer leave a lit "☑ Subfolders" over the next query's folder-local filter (whose first click did nothing, the pane already believing it off) — `emptying_the_field_resets_the_subfolders_checkbox_with_it`. `ProjectedRow` gained `disclosure`, and search result rows set it `false` while `toggle_expanded`/`expand_selected`/`collapse_selected` no-op during a search: results are flat, so the triangle every folder row painted was a dead control that silently inserted expansion state and started a child `read_dir`, and that state outlived the search — clearing the query brought the folder back pre-expanded over a stale cached listing, the exact bug `prune_expansion_state` exists to prevent (`search_result_folder_rows_have_no_working_disclosure`). The chrome row is clipped and its breadcrumb and search field shrink (180 px → a 90 px floor), so a narrow split pane with the info panel open no longer overflows the field over the splitter (the M4 narrow-pane class; verified by opening the `split_panes` render). A zero-result query says "No items match your search" instead of "Empty folder", which was a false claim about the folder (`DirView::empty_placeholder`). Rows are rebuilt **once** per batch instead of twice, and `hits` is capped at `MAX_SEARCH_HITS` (10 000) with the cap on the status line — the per-batch dedupe+sort of the whole result set runs on the UI thread, so an uncapped 200k-hit walk stopped the window painting, including the keystroke that would have cancelled it (`one_batch_rebuilds_the_rows_once_and_the_hit_count_is_capped`, over a thread-local `ROWS_REBUILT` probe in the shape of `dir_view`'s `PROJECTIONS_BUILT`). In fs-core: the in-flight set is now `FuturesUnordered`, because an *ordered* one let one stalled network directory buffer every completed sibling's hits and stop the set being topped up — the very case the concurrency bound exists for (`a_slow_directory_does_not_hold_back_the_hits_beside_it`, a fixture directory that yields 64 times); `looks_like_a_directory_cycle` skips a directory whose path tail repeats three times over, so a **real** directory aliasing an ancestor (macOS firmlinks) costs two laps instead of `MAX_DEPTH / period` complete re-walks of the volume with every match re-reported per lap (`a_real_directory_aliasing_its_parent_is_skipped_not_re_walked`, `cycle_detection_needs_the_tail_to_repeat_three_times`); and `dirs_scanned` counts only directories that were actually opened, so "N folders searched · M skipped" no longer double-counts a failed read. Minors: the status line keeps the free-space figure while a search is live (it is a property of the volume, not the query); the dead `key_context("SearchBar")` — bound nowhere — is gone; §0 gained rows for `escape`, `enter` and the `✕` button; `navigating_away_cancels_the_walk_and_empties_the_field` now asserts the *total* read count and that the abandoned walk never reached `sub2`, since its old assertion (`read_count_of("/root") == 0`) held whether or not the walk was cancelled. **Rejected:** nothing — the two "the label says folders" claims about `skipped` were half right (the wording is "N skipped", but the count really does include per-entry stat failures), recorded as a gap rather than reworded here. **Deferred as gaps:** identity-based (device+inode) cycle detection, stopping the walk at the hit cap, `Skipped` counting distinct paths, the breadcrumb's missing ellipsis, and a baseline for split pane + info panel + search. 441 tests (151 fs-core unit + 6 fs-core integration + 284 app); 9 new. Baselines still need the runner (definition of done item 7 is the orchestrator's to close). |
| 2026-08-24 | — | M6a search, scenarios + docs lane: **two** visual scenarios rather than one, `search_filtered` and `search_results`, driven by `Setup::SearchActive(path, query, recursive)` with the **same** query (`"o"` in `/home`) and only the "Subfolders" toggle differing — so the delta between the two baselines is exactly what the recursive walk adds, and a regression in the recursive half cannot hide behind a differently-shaped frame. `search_filtered` pins the focused field with text in it, the `✕` clear button, the *unchecked* toggle, an instantly filtered listing (three folders and a file, so the filter is visibly not folders-only), no containing-folder labels anywhere, and the count-only status line. `search_results` pins the lit toggle, the finished `"8 results for “o” · 5 folders searched"` line and — the reason the query was changed from `"note"`, which matched nothing in `/home` — **both** kinds of result row in one frame: four local matches unlabelled beside `Documents/notes.txt`, `Downloads/notes.txt`, `Documents/report.pdf` and `Pictures/photo.jpg` each carrying its containing-folder qualifier, in the pane's sort order rather than the walk's arrival order. **Settling:** the app lane's fixed four-round wait became `settle_search`, the search's `settle_info_panel` — advance the deterministic clock one `SEARCH_THROTTLE` window at a time until the pane reports `!SearchState::is_running()` (the flag the same batch that folds in `Done` clears, so once it is false every hit is already in `rows`), then assert there *is* a search and that it has rows, and treat running out of rounds as a failure rather than a capture. Both halves of that assertion are load-bearing: a mid-walk frame pins whichever prefix of the hits had landed plus a `"scanning so far…"` status (a race, not a state), and a search that found nothing captures an "Empty folder" pane that reads as fine in review. No fixture change — the fixture's mtime counter means any new key shifts every node after it, and the tree already contains everything both frames need. Both PNGs were rendered locally and **opened**: field, filtered rows, qualifiers and status line all paint real text (the M4 "every filename rendered as nothing" failure mode). Docs: `as-built/fs-core.md` (the search module — API, ASCII fast path and the no-normalization decision, the 8-read concurrency bound, the never-descend-a-symlink cycle policy plus the `MAX_DEPTH` brace, `Skipped`-is-not-fatal and drop-is-exact-cancellation), `as-built/app.md` (the search bar and the pane's state, plus the scenarios and `settle_search`), `ARCHITECTURE.md` §8's scenario row, and this file's intro, status table, change log and gaps. **Stale claim corrected:** the M5 status row and Known gap still said all 17 baselines needed regenerating; they were regenerated on the runner and merged in #12, so both are now marked closed and the M6a entry carries the live regeneration (17 stale + 2 with no baseline = 19, one all-or-nothing runner run). New gaps recorded: nothing pins an empty result set, a `Skipped` directory or search in the icon grid (and `FakeVfs` cannot make a directory unreadable), and nothing pins a search mid-walk by design. No code behavior changed, so test counts are unchanged at 432. |
| 2026-08-24 | — | M6a search, app lane (`crates/app/src/search.rs`, new): the toolbar search field and the pane's search state. **Where the state lives:** the `Pane` (`search: Option<SearchState>`, `_search_task`, `search_generation`, `search_recursive`), one per pane so the M4 split searches independently, and because the pane already owns the snapshot the instant filter reads, the sort the results are presented in and the `show_hidden` the walk takes as an argument. The `SearchBar` entity reuses the vendored `TextInput` (`⌕`, placeholder "Search", `✕` clear, a "☐/☑ Subfolders" toggle that appears only with a query) at the right end of each pane's chrome row. **Typing** filters the open folder through the pure `fs_core::filter_snapshot` inside the keystroke — the two-cursor pass back to entries allocates nothing beyond the id and row vectors, and a test asserts zero `read_dir`s across a filtering keystroke. **Subfolders** starts `search_recursive` polled inside `cx.background_spawn`, with events crossing to the UI thread through a channel the foreground task drains in 100 ms `Spawner::timer` batches (park on the first arrival, wait one window, drain, fold in once), so a 50k-hit walk repaints ~10×/s. **Cancellation** is one `Task` slot: dropping it drops the pump, the receiver and the background walk held on its stack; `search_generation` guards a batch from a superseded query. Proven with a `RecordingVfs` that parks before each `read_dir` — a walk retargeted mid-flight contributes exactly one directory read and never resumes. **Results are the `DirView` projection**, flat and unindented, so the marquee, drop targets, the context menu's row band, the grid's `painted_cols`, thumbnail windowing, the scrollbar's content height and the info panel's witness all needed no change; selection pruning is now against the projection while expansion pruning stays against the listing, so a filtered-out row stops being actionable but the tree comes back intact. Rules: navigation drops the search, an in-place reload keeps it and re-derives its rows (the hidden toggle also restarts the walk, a sort flip does not), a watcher patch re-derives them so it cannot unfilter. `FocusSearch` + `cmd-f` in the `Workspace` context, forwarded to the active pane like `cmd-l`. **Two bugs the work found:** the field's `TextInput` node needed `track_focus` of the input's own handle or every binding in it was silently dead (§9's named failure mode — `escape` did nothing; `address_bar.rs` still has the same omission, recorded as a gap); and the field's text reaches the pane one effect-flush later than its toggle, so "Subfolders" clicked first was dropped against a `None` search — the first `search_results` capture showed a lit checkbox and "0 results", which is exactly the class of plausible-looking baseline CLAUDE.md's definition of done item 7 exists for. The scope now lives on the pane, the widget's flag only draws the checkbox, and the runner `bail!`s if a search scenario has no rows to capture. New visual scenario `search_results` (recursive; the scenarios lane then reshaped its query and added a folder-local sibling — see the row above). 432 tests (148 fs-core unit + 6 fs-core integration + 278 app), of which 16 in `search.rs` and 1 in `workspace.rs` are new here. |
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
