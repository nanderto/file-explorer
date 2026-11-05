# M8 — Ship prep (draft plan)

**Status: draft, not started.** M0–M7d are merged (PR #20, 2026-09-11). This
document proposes how M8 is built; it is not a record of anything built. When a
lane lands, its outcome goes to `docs/AS_BUILT.md`, not here.

Scope comes from `docs/file-explorer-plan.md` §7 (M8) and
`docs/ARCHITECTURE.md` §10:

> Tabs per pane; menu bar with full action set; app icon; `cargo-bundle` .app;
> codesign + notarize; crash-safe settings writes; performance pass (profile
> 100k-entry dirs, thumbnail memory). *Accept: notarized .app runs on a clean
> Mac.*

---

## 0. One finding that reshapes this milestone

`docs/ARCHITECTURE.md` §10 says tabs are nearly free:

> Tabs = `Pane.dir_views: Vec<Entity<DirView>>` + `active_ix` (the Pane→DirView
> edge was designed 1:N-ready)

**The code does not match that claim.** `Pane` (`crates/app/src/pane.rs:182`)
holds one `dir_view: Entity<DirView>` *and* roughly twenty-five further fields
that are per-tab concerns, not per-pane ones:

| Field group | Fields |
|---|---|
| Location & listing | `path`, `snapshot`, `snapshot_is_stale`, `cache`, `generation`, `free_space`, `load_error`, `_load_task`, `_free_space_task` |
| Navigation | `history`, `scroll_top`, `pending_restore` |
| Search (M6a) | `search`, `_search_task`, `search_generation`, `search_recursive` |
| Tag filter (M6b) | `tag_filter`, `_tag_filter_task`, `tag_filter_generation` |
| Presentation | `view_mode`, `sort`, `show_hidden` |
| Watching | `_watch_guard`, `_watch_pump`, `watch_generation` |

Making `dir_views` a `Vec` while that state stays on the `Pane` produces tabs
that share one history, one sort, one search and one watcher — which is not
tabs. The real work is **extracting a `Tab` struct** that owns all of the
above, leaving `Pane` a thin container (`focus_handle`, `vfs`, `tabs`,
`active_ix`, and the chrome it draws).

That is a large, mechanical, high-contention refactor of the single busiest
file in the app. It drives the sequencing below, and `ARCHITECTURE.md` §10 and
the Decisions table should be corrected in the same PR.

**Open question for the kickoff:** does the search bar and address bar stay
per-*pane* (one field, retargeted on tab switch) or become per-*tab* (text
survives a tab switch)? Explorer keeps the address bar per tab. Recommend
per-tab for `search`/`search_recursive`/`tag_filter` state (already listed
above) but a single `SearchBar`/`AddressBar` *view* per pane, retargeted —
mirroring how the workspace-level info panel already follows the active pane.

---

## 1. PR split

Five PRs, delivery order only. The split follows CLAUDE.md's contention rule —
`pane.rs` and `workspace.rs` are the usual collision points, and M8a owns both
for its whole duration.

| PR | Lane | Touches | Baselines |
|---|---|---|---|
| **M8a** | Tabs per pane | `pane.rs`, `workspace.rs`, `actions.rs`, `keymap.rs` | **all 23 move** + new |
| **M8b** | Full menu bar + keymap-reload fix | `menus.rs`, `actions.rs`, `settings.rs` | none |
| **M8c** | Crash-safe settings writes | `settings.rs`, `fs-core` | none |
| **M8d** | Performance pass | `fs-core/listing.rs`, `thumbnails.rs`, `views/` | none expected |
| **M8e** | App icon, bundle, codesign, notarize | `Cargo.toml`, `build/`, CI | none |

### Sequencing

- **M8a runs alone.** It rewrites `pane.rs` wholesale; nothing else may touch
  that file concurrently.
- **M8b, M8c, M8d run in parallel after M8a**, in isolated worktrees — they
  touch disjoint files. M8b needs M8a's tab actions to exist, so **pin the
  action names inline before M8a starts** (see §2) and M8b can then be written
  against that contract rather than waiting on the implementation.
- **M8e runs last and alone.** It changes `Cargo.toml`, which per CLAUDE.md
  forces a full silent workspace rebuild; warm the cache again before any
  agent runs afterwards.

---

## 2. M8a — Tabs per pane

The milestone's real work.

**Extract `Tab`.** A new `crates/app/src/tab.rs` owning every field in §0's
table plus the `DirView`. `Pane` keeps `focus_handle`, `vfs`, `tabs:
Vec<Entity<Tab>>`, `active_ix`, `address_bar_view`, `search_bar`, and
`_subscriptions`, and forwards to `self.active_tab()`. Every existing
`Pane` method either moves to `Tab` or becomes a one-line delegation — keeping
the public surface stable is what lets the ~199 existing app tests keep
passing and makes a regression legible.

**Tab strip.** Drawn in the pane's chrome row above the breadcrumb, hidden
entirely when a pane has one tab (Explorer and Finder both do this, and it
keeps all 23 existing single-tab baselines structurally comparable). Per tab:
the folder name, the entry-kind icon, a close affordance on hover.

**Actions and bindings** — the contract M8b is written against, to be added to
`ARCHITECTURE.md` §0's traceability table in this PR:

| Behavior | Trigger | Action | Context |
|---|---|---|---|
| New tab (current folder) | `cmd-t` | `NewTab` | `Pane` |
| Close tab | `cmd-w` | `CloseTab` | `Pane` |
| Next / previous tab | `ctrl-tab` / `ctrl-shift-tab` | `NextTab` / `PrevTab` | `Pane` |
| Jump to tab *n* | `cmd-1`…`cmd-9` | `SelectTab(n)` | `Pane` |
| Reopen closed tab | `cmd-shift-t` | `ReopenClosedTab` | `Pane` |

`cmd-w` on the last tab closes the **window**, not the tab — Explorer's rule
and the one users notice when it is wrong.

`SelectTab(n)` takes a parameter, so it joins `Paste`/`SortBy`/`ToggleTag` in
the `no_json` set the M7b gap already records — unless M8b's `schemars`
decision (§3) closes that gap, in which case bind it properly.

**Tests.** `#[gpui::test]`: a new tab inherits the current folder; closing the
active tab activates a neighbour; each tab keeps its own history across
switches; each tab keeps its own sort, view mode and search; the last tab's
`cmd-w` closes the window; a tab's watcher unregisters when that tab closes
(the leak this refactor most plausibly introduces). Unit tests for
`ReopenClosedTab`'s stack.

**Baselines.** The tab strip changes the chrome row, so **every one of the 23
baselines moves** even in single-tab panes if the strip reserves height — a
reason to hide it entirely at one tab. Regenerate on the runner
(`gh workflow run update-visual-baselines.yml --ref <branch>`) and **open the
PNGs**; this is precisely the M4 trap CLAUDE.md's definition of done exists
for. New scenarios: `tabs_three_open`, `tabs_split_panes` (each pane with its
own tabs).

---

## 3. M8b — Full menu bar

M7c pulled the menu bar forward as a bug fix: macOS resolves Command chords
through the main menu, so only actions with a menu item got their chords. It
covers chord-carrying actions plus Quit. M8 owns the rest.

Closes three recorded gaps:

- **M7c: the menu bar is minimal.** Add items with no keyboard equivalent —
  tags, sort, view columns — plus **Window** (tabs, split pane, minimize,
  zoom) and **Help**. The Window menu is new work that M8a's actions feed.
- **M7c: menu items do not follow a `keymap.json` override after a reload.**
  `menus::init` runs once at boot, so a rebound chord keeps the old key
  equivalent beside the item — and because macOS resolves through the menu,
  the *old* chord keeps working and the new one does not. Rebuild the menu on
  every keymap reload.
- **M7b: three actions cannot be bound from `keymap.json`.** `Paste`,
  `SortBy`, `ToggleTag` are `no_json` to avoid a `schemars` dependency.
  Decide here: either take the dependency (a `Cargo.toml` change — then fold
  this lane into M8e's rebuild window) or keep the limitation and document it
  as permanent. **Recommend taking it**: M8a adds `SelectTab(n)` to the same
  set, and a keymap that silently cannot bind five actions is a shipping
  defect rather than a note.

The standing guard
`menus::tests::every_command_chord_in_the_keymap_has_a_menu_item` must keep
passing; extend it to assert the reverse for the new items.

**Blind spot to respect:** gpui's test platform dispatches keystrokes straight
into the keymap and never involves AppKit, so these tests pass on chords the OS
never delivers. The only instrument for real delivery is the running app
(`FE_LOG_KEYS=1`). Every new chord gets checked by hand on the Mac.

---

## 4. M8c — Crash-safe settings writes

M7b writes atomically (temp file + rename) and M7d-c made the load a field-by-
field merge, so the foundation is sound. What remains:

- **Flush on terminate.** The recorded gap *"Recents can lose the last few
  seconds to a hard kill"* — `RECENTS_FLUSH_DELAY` is 3s. Flush pending writes
  on `NSApplicationWillTerminate` and on window close, which closes it for
  every ordinary quit and leaves only a true crash exposed.
- **Close the boot-Recents gap.** *"The folder open at boot is not recorded in
  Recents"* — both startup writers gate on `AppSettings::is_loaded()` and
  `main`'s boot navigation falls inside that window, so a first launch shows
  empty Recents. Have the workspace observe `AppSettings` and flush the active
  tab's path once the load resolves.
- **Survive a corrupt file.** A truncated or malformed `settings.json` already
  yields defaults with a warning rather than failing boot — add a test that a
  file truncated mid-write loads cleanly, and keep one backup generation so a
  user's favorites are recoverable.

Tests belong in `fs-core` against `tempfile` trees per CLAUDE.md §3.

---

## 5. M8d — Performance pass

The plan's §1 benchmark target is explicit and has never been measured:

> a 50k-entry directory scrolls at 120 fps and lists in < 150 ms

- **Measure first, in a committed benchmark**, so the number is a regression
  tripwire and not a one-off anecdote. Generate 50k and 100k entry trees in a
  `tempfile` fixture; time `listing` end to end.
- **Thumbnail memory.** The LRU has a byte budget; profile it against a folder
  of several thousand images and confirm cancel-on-scroll-away actually
  cancels. Record the ceiling in `AS_BUILT.md`.
- **`ListingCache` memory profile** — flagged in `ARCHITECTURE.md`'s Decisions
  table as "Memory profile in M8 perf pass", and now per-*tab*, so M8a
  multiplies whatever it costs by the tab count. Worth re-checking after M8a
  lands rather than before.

---

## 6. M8e — Icon, bundle, codesign, notarize

The acceptance criterion: **a notarized `.app` runs on a clean Mac.**

- **App icon.** None exists (`crates/app` has `icons/` for UI SVGs only, no
  `.icns`). Needs artwork at every required size.
- **`cargo-bundle`.** No `[package.metadata.bundle]` section exists yet. Adding
  it changes `Cargo.toml` → full workspace rebuild, hence this lane running
  alone and last.
- **Codesign + notarize.** Needs an Apple Developer ID, which is an *account
  prerequisite, not a code task* — confirm it exists before the lane starts, or
  the lane delivers an unsigned bundle and stops. Secrets go in CI as
  repository secrets; never in the repo.
- **Verify on a genuinely clean Mac** (or a fresh VM): Gatekeeper must open it
  without the unidentified-developer dialog. The dev Mac has already trusted
  the binary and cannot prove this.

---

## 7. Known gaps *not* in scope

Recorded in `AS_BUILT.md`, deliberately left open — each would be its own PR:

- **`file_colors` is modelled but unpainted**, and `icons::entry_icon` still
  answers only "folder" or "not a folder". A design step, not a wiring one.
- **No test proves an icon rasterizes to visible pixels** — the mitigation
  stays procedural (open the PNGs). Worth revisiting if M8a's baseline
  regeneration is painful.
- **Trash is the boot volume's only** — `~/.Trash` does not union in
  `/Volumes/<name>/.Trashes/<uid>`.
- **Owner/Group are name fields, not dropdowns** — needs
  `Platform::known_owners`/`known_groups`.
- **Legacy `com.apple.FinderInfo` label colours are not read.**
- **The Keyboard settings section is read-only, long and ungrouped.** Rebinding
  from the UI needs chord capture plus conflict detection.
- **A refused `chmod` shows only as a toast.**
- **Cancelling an attribute job leaves partial work with no undo entry** — an
  op-spine change (a receipt on `Cancelled`) worth its own PR.

---

## 8. Two observations from running the app (2026-11-02)

Seen while verifying the build; neither is diagnosed, both are cheap to check
at M8a kickoff since both live in pane/navigation code:

- **The breadcrumb renders empty at `/`.** At `/Users/noelanderton` it reads
  `› Users › noelanderton`; at the volume root the row is blank rather than
  showing the volume. Cosmetic, but the root is a reachable location from the
  Devices section.
- **Boot location and Recents did not behave as documented in one run.**
  `AS_BUILT.md` describes `main` navigating to home at boot; one launch opened
  at `/`, and Recents gained `/` and `Users` without deliberate navigation.
  Not reproduced on a second launch, so this is a *lead*, not a defect — but
  M8c touches boot-time Recents anyway, which is the moment to confirm it.

*(A third observation — the app appearing to boot into the Settings pane — did
not reproduce; `settings_view` initialises to `None` and no pane state is
persisted. Recorded only so it is not re-chased from scratch.)*

---

## 9. Definition of done

CLAUDE.md's gate applies unchanged to every PR above: build, clippy
`-D warnings`, `fmt --check`, unit + integration + `#[gpui::test]` tests
created and green, `docs/AS_BUILT.md` updated. Plus, for this milestone:

1. **M8a's regenerated baselines are opened and looked at** — all 23 move.
2. **Every new chord is verified in the running app**, not only in a dispatch
   test (§3's blind spot).
3. **The notarized `.app` is opened on a clean Mac**, which is the milestone's
   acceptance criterion and cannot be proven on the dev machine.
4. `docs/ARCHITECTURE.md` §10 and its Decisions table are corrected where §0
   above contradicts them.
