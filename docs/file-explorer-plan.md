# File Explorer for macOS — Development Plan

A native macOS file manager written in Rust on [GPUI](https://gpui.rs/) (Zed's GPU-accelerated UI framework), styled after the reference screenshot (`docs/requirements/Basic window.png`, a ForkLift-style layout) but with **Windows File Explorer behavior**, not Finder behavior. Themeable from day one.

**Scope decision:** `OVERVIEW.md` is the full ForkLift feature list (SFTP/S3/WebDAV, folder sync, remote editing, Git awareness, …). This plan scopes **v1 to local file management** — the sidebar, dual-pane browsing, basic file operations, preview/info panel, search, and themes visible in the screenshot. The remote/sync features are captured in [Deferred features](#10-deferred-features) so the architecture leaves room for them (a `VirtualFileSystem` trait boundary), but none are built in v1.

---

## 1. Product goals

- **Fast**: virtualized lists, instant directory loads, thumbnails and metadata computed off the UI thread. Benchmark target: a 50k-entry directory scrolls at 120 fps and lists in < 150 ms.
- **Keyboard-first, Explorer-like**: see the behavior spec in §3. This is the differentiator vs. Finder.
- **Themeable**: JSON theme files (Zed-style), light/dark following the system, user-supplied custom themes.
- **Native**: real trash, real volume list, Finder tags, Quick Look-quality previews — not a cross-platform lowest common denominator.

### Non-goals for v1

Remote volumes, folder sync, archive browsing, Git status, app deleter, multi-language UI, iCloud favorites sync, command-line tool integration. (All deferred, see §10.)

## 2. UI blueprint (mapped from the screenshot)

```
┌────────────┬──────────────────────────────┬───────────────────┬─────────────┐
│ Titlebar: ●●●  ◀ ▶  Folder name  [grid|list|columns] ★ ⚙  ⟳ ⧉ + 🗑  Search  │
├────────────┼──────────────────────────────┼───────────────────┼─────────────┤
│ SIDEBAR    │ PANE 1                       │ PANE 2 (optional) │ INFO PANEL  │
│  Devices   │  Breadcrumb / editable path  │  Breadcrumb       │  Preview    │
│  Shares    │  Status: n items, free space │  Status line      │  General    │
│  Favorites │  Details list (Name/Size/    │  Icon grid with   │   Path,Size │
│  Tags      │  Date, sortable headers,     │  thumbnails       │   Dates,Ext │
│            │  colored tag dots)           │                   │  Permissions│
│            │                              │                   │   rwx grid, │
│            │                              │                   │   octal,    │
│            │                              │                   │   owner/grp │
└────────────┴──────────────────────────────┴───────────────────┴─────────────┘
```

Region by region:

| Region | Contents | Notes |
|---|---|---|
| **Titlebar/toolbar** | traffic lights, back/forward, folder title, view-mode switcher (icons / list / columns), favorite toggle, tools menu, refresh, duplicate, new folder, delete, search field | Custom client-side titlebar (GPUI supports this; Zed does the same). |
| **Sidebar** | *Devices* (mounted volumes + free space), *Shares* (placeholder section in v1), *Favorites* (user-pinned folders, reorderable, drag-to-add), *Tags* (colored dots, click to filter) | Collapsible sections. Also add an Explorer-style **folder tree** section (toggleable) — Finder lacks this, Explorer users expect it. |
| **Panes** | 1 or 2 browsing panes, each with its own breadcrumb, history, view mode, and status line (`19 items, 42.39 GB available` / `1 of 9 selected (1.5 MB)`) | Single pane by default (Explorer-like); split toggle for the dual-pane view in the screenshot. Tabs per pane later (M8). |
| **Views** | Details list (default, like Explorer), icon grid with thumbnails, column/Miller view (stretch) | Details view: sortable Name / Size / Date Modified columns, folders expandable in place (disclosure triangles), tag dots after the name. |
| **Info panel** | Large preview (image/thumbnail), *General* section (path, size with exact bytes, modified/created/added, extension, hide-extension, hidden), *Permissions* section (R/W/X checkbox grid for owner/group/others, octal field, owner & group dropdowns, locked flag) | Toggleable. Read-only in M5, permission editing in M6. |

## 3. Behavior spec: Explorer, not Finder

These choices are the product's identity. Every one is the Windows convention, adapted to a Mac keyboard (Cmd where Windows uses Ctrl):

| Behavior | This app (Explorer-style) | Finder does instead |
|---|---|---|
| Open item | **Enter** or double-click | Cmd+O / Cmd+Down (Enter renames) |
| Rename | **F2** (and slow-second-click) inline edit, name selected without extension | Enter |
| Delete to trash | **Delete** key, no modifier; confirmation optional (setting) | Cmd+Delete |
| Go up | **Backspace** and Alt+Up | Cmd+Up only |
| Address bar | Breadcrumb that becomes an **editable text path** on click (Cmd+L focuses it), with path autocomplete | No editable path |
| Cut/paste files | **Cmd+X / Cmd+C / Cmd+V**; cut items render dimmed; paste moves them | No cut; "move" is Cmd+Opt+V after copy |
| New folder / file | Context menu **New ▸ Folder / Text file…**, Cmd+Shift+N | New Folder only |
| Sorting | Folders always grouped first (setting); click column headers, arrow indicator | Mixed sort by default |
| Selection | Click, Cmd+click toggle, Shift+click range, rubber-band drag in empty space, Cmd+A | Same-ish, kept identical |
| Type-ahead | Typing letters jumps to the next matching name | Same, kept |
| Conflict dialog | Explorer-style: Replace / Skip / **Keep both** / Apply to all, with size+date comparison | Keep-both buried |
| Free space | Always visible in the pane status line | Hidden by default |
| Hidden files | Toolbar toggle + Cmd+Shift+. | Shortcut only |

Undo (Cmd+Z) works for rename, move, copy, new folder, and trash (restore).

## 4. Tech stack

| Concern | Choice | Rationale |
|---|---|---|
| UI framework | **gpui** | GPU-rendered, Metal-native on macOS, powers Zed. Pre-1.0: pin the exact version. |
| Component library | **gpui-component** (longbridge) | Mature set: themed widgets, virtualized `Table`/`List`, resizable panels, dock, sidebar, breadcrumb, context menu, inputs, modals, notifications — covers most of the screenshot. Its JSON theme system is the base of our theming. Let its `Cargo.toml` dictate the gpui revision to avoid version skew. [adabraka-ui](https://github.com/Augani/adabraka-ui) is a fallback reference if a widget is missing. |
| FS watching | **notify** (FSEvents backend) | Live-refresh panes when the directory changes underneath us. Debounce with `notify-debouncer-full`. |
| Trash | **trash** crate | Uses the native macOS trash API; supports restore metadata. |
| Open files/apps | **open** crate + `NSWorkspace` (via objc2) for "Open with…" app lists | |
| Volumes, tags, thumbnails | **objc2 / objc2-foundation** | `NSFileManager` mounted volumes + free space; Finder tags via `com.apple.metadata:_kMDItemUserTags` xattr; `QuickLookThumbnailing` for real thumbnails (fallback: `image` crate decode). |
| Permissions | `std::os::unix::fs::PermissionsExt`, `libc` for chown, owner/group name lookup via `getpwuid`/`getgrgid` | |
| Config/themes | **serde / serde_json / schemars** | Settings + themes as JSON in `~/Library/Application Support/FileExplorer/`. |
| Async | GPUI's own executors (`background_executor` / `foreground_executor`) | No tokio; GPUI ships its own smol-based runtime and all UI updates must come back through its foreground executor. |
| Packaging | **cargo-bundle** → `.app`; `codesign` + `notarytool` later | |

**Step 0 of M0 is to verify current crate versions** (gpui on crates.io vs. git pin, gpui-component release) — this ecosystem moves fast and the plan should not hard-code versions.

**Developing on Windows:** GPUI runs on Windows, so day-to-day UI development works on this machine. All macOS-specific integrations (trash, tags, volumes, thumbnails, permissions/ownership) live behind a `Platform` trait with a stub Windows impl, so the app compiles and runs everywhere but is only *complete* on macOS. CI builds both; real verification happens on Mac hardware each milestone.

## 5. Architecture

Cargo workspace, three crates:

```
file-explorer/
├── crates/
│   ├── fs-core/        # No GPUI dependency. Everything testable headless.
│   │   ├── entry.rs        # FileEntry { path, kind, size, dates, perms, tags, hidden }
│   │   ├── listing.rs      # read_dir → Vec<FileEntry>; sort keys; natural sort; filters
│   │   ├── ops/            # copy, move, rename, delete(trash), new_folder, duplicate
│   │   │   ├── job.rs      #   Job { id, kind, progress, cancel_token, conflicts }
│   │   │   └── queue.rs    #   serial-per-volume job queue, progress events (channel)
│   │   ├── undo.rs         # undo/redo stack of inverse operations
│   │   ├── watcher.rs      # notify wrapper → debounced DirEvent stream
│   │   ├── platform.rs     # Platform trait: volumes(), trash(), tags(), thumbnail(), open()
│   │   │   ├── macos.rs    #   objc2 implementation
│   │   │   └── stub.rs     #   dev-on-Windows/Linux implementation
│   │   └── vfs.rs          # VirtualFileSystem trait (local impl only in v1; the seam
│   │                       #   where SFTP/S3 providers plug in later)
│   ├── theme/          # Theme model, JSON loader, system light/dark detection,
│   │                   #   built-in themes, file-type icon mapping
│   └── app/            # GPUI application
│       ├── workspace.rs    # window root: sidebar + panes + info panel + toolbar
│       ├── pane.rs         # PaneState: location, history, view mode, selection, sort
│       ├── views/          # details_list.rs, icon_grid.rs, columns.rs (stretch)
│       ├── sidebar.rs, breadcrumb.rs, info_panel.rs, search.rs
│       ├── dialogs/        # conflict.rs, progress.rs, confirm.rs
│       ├── actions.rs      # every command as a GPUI action (drives menus + keymap)
│       └── keymap.rs       # Explorer-convention bindings from §3, user-overridable
└── docs/
```

**Data flow.** GPUI entities hold state (`Workspace` → `Pane` → `DirListing`). A `DirListing` loads on the background executor, returns sorted `Vec<FileEntry>`, and the pane re-renders. The watcher pushes `DirEvent`s that patch the listing incrementally (no full reload on every change). File operations run as `Job`s in `fs-core`'s queue; progress events cross to the UI over a channel and render as an activity popover (the ForkLift-style Activity view). Conflicts pause the job and raise the Replace/Skip/Keep-both dialog.

**Threading rule.** The UI thread never touches the disk. Every `stat`, `read_dir`, thumbnail, and operation goes through the background executor. Metadata for the info panel is fetched lazily per selection.

## 6. Theming

- Theme = JSON file: `{ name, appearance: light|dark, colors: { surface, sidebar, text, muted, accent, selection, border, ... }, file_colors: { folder, image, code, ... } }` — same shape as gpui-component/Zed themes so existing themes are easy to adapt.
- Built-ins: one dark (matching the screenshot's graphite look) and one light. `appearance: system` follows macOS and switches live via the system appearance notification.
- User themes dropped into `~/Library/Application Support/FileExplorer/themes/` hot-reload via the same `notify` watcher.
- **Rule: no widget hard-codes a color.** Everything reads the active theme through gpui-component's theme context. Enforced by review + a grep check in CI for hex literals in `crates/app`.
- Tag colors (Orange/Purple/Blue/…) come from macOS's fixed Finder tag palette, not the theme.

## 7. Milestones

Each milestone ends runnable and demoable. Rough sizing assumes one developer, part-time; treat as ordering, not promises.

- **M0 — Skeleton (1 wk).** Workspace compiles; window opens with custom titlebar, empty sidebar/pane/info-panel layout with resizable splitters; dark theme loads from JSON; light/dark follows system. *Accept: window screenshot matches the layout grid in §2.*
- **M1 — Read-only browsing (2 wk).** Details list view (virtualized), natural sort, folders-first, sortable columns; navigation: double-click/Enter, Backspace/Alt+Up, back/forward history, breadcrumb + Cmd+L editable path with autocomplete; type-ahead; hidden-files toggle; status line with item count and free space. *Accept: browse a 50k-file directory smoothly; every §3 navigation row works.*
- **M2 — Sidebar (1 wk).** Devices with volume free space, Favorites (add via drag/context-menu, reorder, persist), folder tree section, section collapse. *Accept: favorites survive restart; ejectable volumes show eject.*
- **M3 — File operations (3 wk).** Selection model (click/Cmd/Shift/rubber-band/Cmd+A); context menus; new folder/file; F2 inline rename; Delete→trash; Cmd+X/C/V cut-copy-paste including cut dimming and paste-into-same-folder "copy" naming; duplicate; drag-and-drop within and between panes and from/to other apps; job queue with progress popover, cancel, conflict dialog (Replace/Skip/Keep both/Apply-all); undo/redo. *Accept: a scripted torture sequence (copy tree with conflicts, cancel mid-copy, undo a move) leaves the filesystem correct — verified by an integration test in `fs-core`.*
- **M4 — Icon view + dual pane (2 wk).** Icon grid with background-loaded thumbnails (QuickLook on mac) + LRU cache; view-mode switcher; split-pane toggle giving the screenshot's list+grid layout; drag between panes. *Accept: screenshot reproduction side-by-side comparison.*
- **M5 — Info panel (1.5 wk).** Preview (images natively; other types via icon + metadata), General section, Permissions display (rwx grid, octal, owner/group, locked), multi-selection summary. *Accept: panel matches screenshot fields for a JPEG.*
- **M6 — Search, tags, permission editing (2 wk).** Toolbar search filtering the current folder, then recursive with streamed results; Finder tags read/write + tag dots + sidebar tag filter; permission checkboxes and octal field actually chmod; owner/group change where privileged. *Accept: tagging a file here shows in Finder and vice versa.*
- **M7 — Theme polish + settings (1 wk).** Settings window (general behaviors from §3 that are opt-out, keymap overrides, theme picker), custom-theme hot reload, second built-in theme. *Accept: a hand-written user theme applies without restart.*
- **M8 — Ship prep (2 wk).** Tabs per pane; menu bar with full action set; app icon; `cargo-bundle` .app; codesign + notarize; crash-safe settings writes; performance pass (profile 100k-entry dirs, thumbnail memory). *Accept: notarized .app runs on a clean Mac.*

## 8. Testing

- `fs-core` is UI-free: unit + integration tests against `tempfile` trees for listing, sorting, every operation, conflict handling, undo inverses, and the watcher (these run in CI on all platforms).
- GPUI supports `#[gpui::test]` with a headless test context — use it for pane state, selection model, history, and keymap dispatch.
- One scripted end-to-end smoke run per milestone on real macOS (manual until worth automating).
- CI: fmt + clippy + tests on macOS and Windows runners; macOS runner also builds the .app bundle.

## 9. Risks

| Risk | Mitigation |
|---|---|
| gpui API churn (pre-1.0, tracks Zed) | Pin versions; take the gpui revision from gpui-component; upgrade only at milestone boundaries. |
| gpui-component gaps (e.g., Miller columns, rubber-band selection) | Budgeted custom widgets in M3/M4; adabraka-ui and Zed's own workspace code as reference implementations. |
| Thumbnail performance/memory in big folders | Generate only for visible+margin rows, LRU byte-budget cache, cancel on scroll-away. |
| Trash/undo edge cases (volumes without trash, restore races) | `trash` crate handles per-volume trash; undo entries invalidate when the source changed underneath. |
| Finder-tag xattr format quirks | It's a binary plist array of `Name\n<colorindex>` strings — well documented; round-trip test against Finder in M6. |
| Developing for mac on a Windows machine | Platform trait + stubs keep ~90% of the work portable; schedule real-Mac time at each milestone gate. |

## 10. Deferred features

From `OVERVIEW.md` (ForkLift parity), in rough future order: archive browsing as folders → Open in Terminal → workspaces (saved layouts) → multi-rename presets → synchronized browsing → folder compare/sync → remote volumes (SFTP first, then S3/WebDAV/SMB — via the `vfs.rs` seam) → transfer queue with bandwidth limits → remote editing → Git status column → app deleter → iCloud favorites sync → localization.
