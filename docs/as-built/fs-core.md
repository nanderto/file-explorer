# As Built — `crates/fs-core` and `crates/theme`

<!-- Split out of docs/AS_BUILT.md: that file is read by every agent on
every milestone, and the component detail had grown past 1,500 lines.
AS_BUILT.md stays the index (status, known gaps, deviations, change log);
this file carries the detail for one crate. Update both: the index's
change log row, and the relevant section here. -->

Back to the index: [docs/AS_BUILT.md](../AS_BUILT.md).

## fs-core
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
- `platform/` (M4 addition): `Platform::thumbnail(path, px) -> Result<Thumbnail>`
  — "longest edge at most `px`, aspect preserved, never upscaled", with an
  `Err` meaning *no preview available* (an ordinary outcome the icon grid falls
  back to a type icon on, not a failure). `macos.rs` implements it in two tiers
  inside a single `SpawnerExt::unblock`, exactly as plan §4 prescribes:
  **QuickLookThumbnailing** (`objc2-quick-look-thumbnailing`
  `QLThumbnailGenerator generateBestRepresentationForRequest:completionHandler:`,
  representation types `All`), then an **`image`-crate decode** for anything
  QuickLook declines. Verified working locally on this Mac (a 300×200 PNG at
  px=64 comes back 64×43 with byte order confirmed RGBA). Three notes worth
  keeping: (1) the completion handler runs on a QuickLook-owned queue, so the
  `CGImage` is converted to plain bytes *inside* the handler and only a
  `Vec<u8>` crosses the channel — `CFRetained<CGImage>` is not `Send` and
  moving one would be unsound though it would compile; (2) the wait is bounded
  (10 s, then `cancelRequest`) because QuickLook is an XPC round-trip to a
  helper that can be cold, stuck, or absent, and an unbounded wait would park
  an executor thread forever — a timeout simply falls through to the decoder;
  (3) `CGBitmapContext` cannot produce straight RGBA, so the draw uses
  `kCGImageByteOrder32Big | kCGImageAlphaPremultipliedLast` and the
  premultiplication is undone afterwards, which makes the QuickLook and
  `image` tiers agree on one documented pixel format (opaque pixels, i.e. most
  thumbnails, are untouched by that step). `stub.rs` synthesizes pixels
  instead: an FNV-1a hash of the path picks a base colour and one of three
  aspect ratios (square/landscape/portrait, longest edge exactly `px`), and a
  checkerboard-under-a-diagonal-gradient fills the tile — no I/O, no clock,
  byte-identical on every platform and every run, so icon-grid unit tests and
  visual scenarios have something stable to paint. `DefaultHasher` was
  deliberately avoided (its output is explicitly unstable across releases and
  these pixels end up in committed baselines).
- `attrs.rs` (M5) + `platform/` (M5 addition): the info panel's data.
  **Pure, clock-free, filesystem-free** — safe to call from the UI thread and
  exact-assertable on every platform:
  - `UnixPerms` keeps the low 12 bits of `st_mode` (the nine rwx bits plus
    setuid/setgid/sticky — never the file-type bits, which are already
    `EntryKind`). `octal()` is three digits normally and four when a special
    bit is set (a leading zero would read as C source, not as a mode);
    `symbolic()` is the nine `ls -l` characters with the special bits folded
    into the class they modify (`s`/`S`, `t`/`T`); `allows(class, bit)` is what
    the panel's checkbox grid reads.
  - `FileAttrs { perms, owner, group, locked, added, extension_hidden,
    type_description }`, every field independently optional so a platform that
    cannot answer one lookup degrades that field instead of failing the call.
  - `SelectionSummary { files, dirs, total_size }` + `summarize(entries)` for
    the §2 multi-selection summary. `total_size` sums **files only**: a
    directory's `size` is its own inode size, not its contents', and recursive
    folder sizing is a separate cancellable job.
  - `is_previewable(path, size)` — an extension allowlist (images, PDF, plain
    text/markup/source, audio, video, office/iWork, camera raw and
    `psd`/`ai`/`eps`) plus an inclusive 64 MiB ceiling
    (`PREVIEW_SIZE_CEILING`), so nothing asks QuickLook about every `.o` in
    `/usr/lib` or starts decoding a multi-gigabyte disk image. An allowlist
    rather than a denylist because the long tail of file types is
    overwhelmingly *not* previewable and each question costs an XPC round
    trip. `is_previewable_entry(&FileEntry)` is the companion that also
    excludes directories — the pinned `(path, size)` form cannot tell a folder
    named `Album.png` from a file without a `stat`, and the UI thread may not
    stat. Only the info panel's preview goes through the gate so far — the icon
    grid's `thumbnails.rs` still asks for every tile, so the two can disagree
    about a format the allowlist has not heard of (see Known gaps).
  - `Platform::file_attrs(path) -> Result<FileAttrs>` is the OS half. macOS
    does it inside **one** `SpawnerExt::unblock`: an `lstat` via
    `std::os::macos::fs::MetadataExt` for mode/uid/gid and `UF_IMMUTABLE`
    (0x2) for "Locked", `NSFileManager attributesOfItemAtPath:` for the owner
    and group *names*, and one `NSURL resourceValuesForKeys:` for
    `AddedToDirectoryDate`, `HasHiddenExtension` and
    `LocalizedTypeDescription`. No `libc` dependency was added; the only new
    Cargo change is the `NSDate` feature on the macOS-only `objc2-foundation`
    dep. `lstat`, not `stat`, deliberately: the panel describes the *selected
    item*, so a symlink reports its own mode. A path whose bytes are not valid
    UTF-8 (network and FAT-family volumes carry them; APFS rejects them)
    returns the lstat-only fields and nothing Foundation-derived: `NSString`
    can only carry UTF-8, and a lossily converted path would ask about a
    *different* file. The call is deliberately **unbounded** — unlike the
    QuickLook path, which is a cancellable completion-handler API — so a hung
    mount parks its pool thread; recorded as a Known gap. `stub.rs` derives
    every field from the path by the same FNV-1a hash the thumbnails use, so
    the info panel renders identically on every platform and in every baseline
    — and because `locked` and `extension_hidden` are *path*-derived, no test
    may assert a value for them over a path it did not fix (a `tempfile`
    suffix is random; the portable `tests/attrs.rs` asserted `!locked` over
    exactly such paths and failed on roughly half of all off-macOS runs).
  - **Deviation from ARCHITECTURE.md §6**, which sketches
    `perms: Option<UnixPerms>` as a `FileEntry` field: attributes are fetched
    per selection through `file_attrs` instead, which is what §9's M5 line
    actually describes. A lazily-empty field on every listing row would have
    forced a `FileEntry` churn across all of M1–M4 for data only one panel
    reads.
- `search.rs` (M6a): the toolbar search's two halves, deliberately different
  in cost.
  - `SearchQuery::new(text) -> Option<Self>` trims and pre-lowercases the
    needle once; blank/whitespace-only input is `None`, i.e. *no search*, so
    the UI shows the unfiltered listing instead of "no results".
    `matches_name` is a case-insensitive substring match on the **name** only
    (Explorer's default — never the path or the contents). Case folding is
    `str::to_lowercase` (Unicode simple lowercasing: `Ä`↔`ä`), with an ASCII
    fast path that compares bytes in place, so an all-ASCII listing allocates
    nothing per row and only a name containing non-ASCII pays for a lowercased
    copy of itself. No Unicode normalization: a decomposed `e`+U+0301 name does
    not match a precomposed `é` needle — normalizing every candidate would cost
    an allocation per row on every keystroke.
  - `filter_snapshot(&ListingSnapshot, &SearchQuery) -> Vec<EntryId>` is
    **pure** (no I/O, no stat), returns matches in the snapshot's own sorted
    order, and allocates only for matches — which is what lets the UI call it
    inside a keystroke handler.
  - `search_recursive(vfs, root, query, show_hidden) -> BoxStream<SearchEvent>`
    is the recursive walk: **breadth-first** (shallow hits first) with at most
    `MAX_CONCURRENT_DIR_READS` = 8 directory reads in flight — enough overlap
    to hide per-`read_dir` latency (visibly so on network volumes) without
    occupying the blocking pool that copies and thumbnails share. Hits stream
    out as they are found; the only state that grows with the tree is the FIFO
    of pending *directories*. "Breadth-first" describes that FIFO — the order
    reads *start* — not the exact interleaving of the output: the in-flight set
    is a `FuturesUnordered`, deliberately, because an ordered one buffers every
    completed sibling behind whichever read happens to be at its head, so one
    stalled network directory used to stop the whole stream **and** stop the
    in-flight set being topped up — the exact case the concurrency bound exists
    for. `SearchEvent::Progress { dirs_scanned }` counts directories actually
    opened (a failed read is `Skipped`, not scanned) and is coalesced to one
    event per `PROGRESS_EVERY_DIRS` = 16 directories plus an exact final count,
    so a status line does not wake thousands of times a second.
  - **Cycle policy**, three layers. Symlinked directories are reported as hits
    by name and **never descended into** — a symlinked directory is a leaf here
    exactly as it is in the listing view, which makes the ordinary cycle
    impossible with no visited set and stops one file being reported twice
    through two aliases. `looks_like_a_directory_cycle` then catches the case a
    kind check cannot see: a **real** directory aliasing an ancestor (macOS
    firmlinks such as `/System/Volumes/Data`, some network mounts) shows up as a
    path whose tail repeats, so a path repeating its last `1..=MAX_CYCLE_PERIOD`
    components `CYCLE_REPEATS` = 3 times over is `Skipped` unread. That is the
    layer that matters for cost: depth alone bounds the *depth* of such a loop,
    not the work, so `MAX_DEPTH` = 64 by itself meant ~21 complete re-walks of
    the whole volume with every match re-reported under each alias path. The
    trade is that a genuine `a/b/a/b/a/b` tree is skipped (loudly — as
    `Skipped`); the exact fix, a device+inode visited set, needs a portable
    identity seam and is a recorded gap. `MAX_DEPTH` remains as the last resort
    behind both, reporting the over-deep directory as `Skipped` rather than
    silently dropping it.
  - **Failure and cancellation**: an unreadable directory (or a child whose
    stat fails) yields `SearchEvent::Skipped { path, error }` and the walk
    continues — a search never fails as a whole. The stream **spawns nothing**
    and advances only while polled, so dropping it is exact cancellation:
    every unstarted read is abandoned, no `Done` is emitted, and nothing is
    left behind (search writes nowhere). `tests/search.rs` proves the policies
    against a real temp tree (hidden dotfiles, a symlink pointing back at the
    root, a missing directory), and a recording `Vfs` wrapper in the unit tests
    proves the read count stops dead at the drop.
- `thumbnail.rs` (M4): the pixel type and the cache.
  **`Thumbnail`** carries `width`/`height` plus `Arc<[u8]>` of tightly packed,
  non-premultiplied, top-down RGBA8, constructed through a checked
  `Thumbnail::new` (the bytes come from OS APIs and decoders, so a length
  mismatch is a runtime condition, not a caller bug). *Decoded* pixels rather
  than an encoded blob, deliberately: gpui ingests raw RGBA, so a blob would
  only move the decode into the render pass that the plan's threading rule
  forbids; the byte budget below becomes exact (`w*h*4`) instead of
  entropy-dependent; and no container format has to leak across the fs-core
  seam. `MAX_PX = 4096` + `validate_px` are shared by every implementation so
  the size contract is one rule rather than three.
  **`ThumbnailCache`** is LRU bounded by *bytes resident* per ARCHITECTURE §M4
  (an entry cap cannot bound this: 64 entries is 590 KB of 64px tiles or 67 MB
  of 2048px ones), default budget 64 MB, styled after `ListingCache`
  (MRU-first `VecDeque`, `get` promotes, `insert` is the write-back). `insert`
  replaces the whole `(path, px)` slot, then evicts from the LRU end until the
  newcomer fits, and **returns `false` for a thumbnail bigger than the entire
  budget** — rejected rather than admitted, so it can neither be stored nor
  wedge the cache by evicting everything and still not fitting.
  **`ThumbnailKey` = `(path, px, ContentStamp)`** where `ContentStamp` is
  `(mtime-as-nanos, size)`: `px` because the same file at 48px and 256px are
  different images, and the stamp because a thumbnail keyed on path alone
  survives an edit and a picture of the old contents is a real bug. Both stamp
  fields are already on `FileEntry`, so the grid builds a key from a listing
  row with no extra `stat` (`ThumbnailKey::for_entry`). One deliberate design
  call: a stamp mismatch **misses without evicting** the entry it failed to
  match — the cache cannot tell which of two stamps is newer, so evicting on
  mismatch would let a caller holding a stale stamp discard a *fresher*
  thumbnail; the stale bytes are reclaimed by the `insert` that follows the
  miss. `invalidate(path)` drops every size and version of one path (the
  write-back for a watcher removal/replacement); `clear()` drops everything
  (e.g. leaving icon view).
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
- `tags.rs` (M6b, Finder tags): `Tag { name: Arc<str>, color: TagColor }`, the fixed macOS
  palette `TagColor { None=0, Gray=1, Green=2, Purple=3, Blue=4, Yellow=5,
  Red=6, Orange=7 }` with `index`/`from_index`/`rgba`/`standard_name`/
  `from_standard_name`/`PALETTE`, `standard_tags()`, and the pure codec
  `encode_tag_strings`/`decode_tag_strings` for the `"Name\ncolorindex"` strings
  macOS stores in the `com.apple.metadata:_kMDItemUserTags` xattr. The enum's
  discriminants **are the on-disk colour indices** — renumbering them would
  recolour every tagged file on the user's disk, so
  `color_indices_are_the_on_disk_values_and_never_change` fails if anyone does.
  `TagColor::rgba` is the one **theme-exempt** colour source in the product
  (plan §6): a tag dot that is not the colour Finder paints is the wrong dot, so
  the palette is pinned in fs-core (`Red = 0xFF5257FF`, …) rather than themed;
  `TagColor::None` is transparent and callers branch on it instead of painting
  it. Codec rules, all unit-tested: encode always writes the index (including
  `0`), drops blank names and collapses duplicate names keeping the first;
  decode reads `"Name\n6"`, a bare `"Name"` (colour `None`), an out-of-range
  index (**name kept, colour dropped** — a future macOS must not cost the user a
  tag), and a non-numeric trailing line as *part of the name*, which is how a
  tag name containing a newline survives both directions. Split at the **last**
  newline, so the one documented ambiguity is a name whose own final line is a
  bare integer; recorded as a gap rather than papered over. Array order is
  preserved — it is the order Finder shows.
- `Platform::read_tags` / `write_tags` / `known_tags` (M6b, ARCHITECTURE.md §6
  already listed the first two). `read_tags` is `Ok(vec![])` for an untagged
  item — the details rows ask for every visible row — and `Err` only for a real
  failure or a payload that is a property list but not an array (loud, because
  the next write would overwrite whatever it held). `write_tags` replaces the
  whole set and **removes the xattr** when handed an empty slice, so an
  untagged file is byte-identical to one never tagged. `known_tags` is
  best-effort: the standard palette plus whatever the implementation can
  discover.
- macOS tags (`platform/macos.rs`), two mechanisms, both chosen to avoid a new
  crate (a `Cargo.toml` dependency change costs a full silent workspace
  rebuild, CLAUDE.md): (a) the xattr syscalls `getxattr`/`setxattr`/
  `removexattr` are declared in a private `mod xattr` rather than pulled from
  the `xattr` crate or `libc` — three stable BSD entry points, the same
  argument that already justifies the hand-written `UF_IMMUTABLE`. `ENOATTR`
  and `ENOTSUP` mean "no tags", not failure; `ERANGE` between the sizing call
  and the read is retried up to three times; paths go through
  `OsStrExt::as_bytes`, never `to_string_lossy`, because a lossily-converted
  path would tag a *different* file. `options` is `0` throughout, i.e. symlinks
  are **followed** — a mode belongs to the link (hence `file_attrs`' `lstat`)
  but a tag belongs to the item the user clicked. (b) the plist is serialized
  by `NSPropertyListSerialization` from the already-present `objc2-foundation`
  — two extra *features* on that dependency (`NSData`, `NSPropertyList`), no
  new crate. Writes are **binary** (`BinaryFormat_v1_0`), which is what Finder
  writes; reads let Foundation sniff the format, so an XML payload from a
  third-party tagger or a hand-run `xattr -w` is read too. All of it inside one
  `SpawnerExt::unblock` per call — the UI thread only awaits.
- `known_tags` on macOS reads `FavoriteTagNames` out of
  `~/Library/Preferences/com.apple.finder.plist` (with the same Foundation
  parser, straight from the file rather than through `NSUserDefaults`, which
  would read *our* domain) and appends any favourite not already in the
  palette. Finder records favourites by name only — the colour assignments live
  in the user's SyncedPreferences store, which is not a documented format — so
  a favourite whose name is not one of the seven standard colour names comes
  back uncoloured. Recorded as a Known gap.
- Stub tags: an in-memory `BTreeMap<PathBuf, Vec<Tag>>` per `StubPlatform`,
  empty until written, plus a synchronous `seed_tags` for visual scenarios.
  Deliberately **storage, not a path hash** like the stub's thumbnails and
  attributes: tags are the one thing the app writes, so a write-then-read test
  over a stub that answered from a hash of a `tempfile` path would be fiction —
  and that is exactly the shape of the M5 flake (see the M5 review-fixes row).
  Writes go through the codec first, so the stub normalizes exactly as macOS
  would; `known_tags` is palette + stored names in `BTreeMap` order.
- Tag tests: 13 codec/palette unit tests in `tags.rs`, 1 stub test in
  `platform/mod.rs`, 2 in `platform/macos.rs` and 9 in `tests/tags.rs`. The
  **acceptance criterion** (a file tagged here shows in Finder and vice versa)
  is pinned by four of those that share no code with our own reader:
  `tests/tags.rs` writes tags and then reads the raw xattr back with **Apple's
  `xattr -px`**, asserts the bytes start with `bplist00`, and hands them to
  **`plutil -convert xml1`** to assert the array holds exactly
  `<string>Red\n6</string>`, `<string>Wörk\n0</string>`,
  `<string>Später\n3</string>`; the reverse direction builds a plist with
  `plutil -convert binary1`, installs it with `xattr -wx`, and asserts
  `read_tags` decodes it (plus an XML-payload variant via `xattr -w`). In
  `macos.rs`, two more cross-check against **Foundation's own public tag API**
  (`NSURL`'s `NSURLTagNamesKey`, the key Finder and every tag-aware app use):
  what we write, Foundation reports as tags, and what Foundation sets, we read.

### M6b attribute ops: Chmod / Chown / SetTags (ops + undo lane)
- Three new `FileOp` variants on the **existing** job spine — same
  destination-volume lanes (`lane_path` is the first path, as for Trash and
  Delete), same cancel flag, same `OpReceipt`, same `JobEvent` stream:
  `Chmod { paths, mode }`, `Chown { paths, owner: Option<String>, group:
  Option<String> }`, `SetTags { paths, tags }`, with `JobKind::Chmod/Chown/
  SetTags`. One shared `JobQueue::run_attrs` loop drives all three, dispatching
  on a private `AttrChange` enum (the op's payload minus its path list).
- **Where the mutation lives, and why.** A unix mode is file I/O, so
  `Vfs::mode(&Path) -> Result<Option<u32>>` and `Vfs::set_mode(&Path, u32)` sit
  beside `remove`/`rename`; resolving an owner *name* to a uid is a
  directory-service lookup, so `Platform::set_ownership(&Path, Option<&str>,
  Option<&str>)` is a platform method; tags were already
  `Platform::read_tags`/`write_tags`. Both `Vfs` methods are **defaulted**
  (`Ok(None)` and an explicit "this filesystem cannot change unix permissions"
  error) so the app's test-double `Vfs` keeps compiling; `RealVfs` overrides
  them (`std::fs::metadata` + `set_permissions` under `cfg(unix)`, masked to the
  new `PERM_BITS = 0o7777`; Windows reports `None` and refuses to write), and
  `FakeVfs` models a per-node mode (`FAKE_FILE_MODE` 0o644 / `FAKE_DIR_MODE`
  0o755) so the ops are testable headlessly on every OS. A missing path is an
  `Err` from `mode`, not `Ok(None)` — `Chmod` may not read "gone" as "no mode".
- **Symlinks**: `mode` and `set_mode` both *follow* them (they must name one
  inode, or an undo would write a link's mode onto its target), which diverges
  from `file_attrs`' `lstat`. Recorded as a Known gap for the panel lane.
- `Platform::set_ownership` on macOS is `NSFileManager
  setAttributes:ofItemAtPath:error:` with `NSFileOwnerAccountName` /
  `NSFileGroupOwnerAccountName` — the *name* keys, so Foundation does the
  account lookup and there is no `getpwnam` and no `libc` (and no new
  dependency: `NSDictionary` was already an enabled feature). It refuses a
  non-UTF-8 path rather than lossily converting it, for the same reason
  `file_attrs` does: the lossy form names a different file, and here that would
  give away the wrong one. **Verified on this Mac:** `setAttributes:` returns
  *success* for an account name it cannot resolve and silently changes nothing,
  so the implementation reads the ownership back afterwards and fails if the
  request was ignored (a "stuck" value only — a value that moved but does not
  string-match is treated as an alias, not a failure).
- `StubPlatform` models ownership as storage (a `BTreeMap` of overrides layered
  over `file_attrs`' path-derived defaults) rather than a path hash, for the same
  reason as tags: it is something the app *writes*. It refuses
  `STUB_PRIVILEGED_OWNER` (`"root"`), which gives the EPERM path a deterministic
  test on every OS — and since real macOS refuses the same name, one assertion
  covers both machines.
- **The queue now optionally holds a `Platform`.** `JobQueue::new` is unchanged;
  `JobQueue::with_platform(vfs, platform, spawner)` adds it, and
  `JobQueue::platform()` hands it to undo's guards. A platformless queue runs
  every other op exactly as before and fails `Chown`/`SetTags` with "…this
  JobQueue was built without a Platform" rather than silently doing nothing.
- **Exact undo.** Each op captures the previous value *before* it writes, into
  `OpReceipt::restored_attrs: Vec<(PathBuf, PrevAttrs)>` where `PrevAttrs` is
  `Mode(u32)` | `Ownership { owner, group }` | `Tags(Vec<Tag>)`.
  `UndoEntry::from_receipt` groups those into **one inverse op per distinct
  previous value**, so a mixed selection comes back exactly as it was rather
  than flattened to one mode. `Chown` captures *both* halves even when the op
  changes one, so the inverse is self-contained. An empty tag set is a real
  previous value ("this file had no tags"), so undo clears the tags again.
- **`AttrGuard`, not `Fingerprint`.** `chmod` changes ctime, **not** mtime, so
  the existing `(path, mtime)` fingerprint is structurally blind to exactly the
  change these ops make (pinned by a `FakeVfs` test *and* a real-file test).
  `UndoEntry::attr_guards: Vec<AttrGuard { path, expected: PrevAttrs }>` instead
  asserts that the attribute still holds what the job wrote, read back through
  the `Vfs` (mode) or the queue's `Platform` (ownership, tags) at undo time;
  a mismatch yields `UndoOutcome::Invalidated` with "'x' permissions changed
  since" / "ownership changed since" / "tags changed since" and touches nothing.
  Guards cover only the paths that actually changed (a path that failed was
  never written, and guarding it would invalidate a good undo), and for `Chown`
  only the halves the op set. A guard whose value cannot be read at all (no unix
  mode; no `Platform` on the queue) is skipped rather than treated as a
  mismatch.
- **Partial failure deliberately deviates from the rest of the spine.**
  Copy/move fail the whole job on the first error, and the app records no undo
  entry for a `Failed` job — which for a half-applied chmod over a large
  selection would mean no way back. So the attribute ops attempt every path,
  record per-path reasons in the new `OpReceipt::failed: Vec<(PathBuf, String)>`,
  **complete** as long as one path changed (keeping that half undoable), and fail
  outright only when nothing changed at all (nothing applied ⇒ nothing to undo,
  and the error names every reason). The app is expected to surface `failed` as
  a "changed 3 of 5" toast. A mid-job cancel still ends in `Cancelled`, which
  carries no receipt — same as a cancelled multi-file `Move`, recorded as a gap.
- Attribute-op tests: 27 unit (queue spine, mode masking, denied/vanished/
  all-failed selections, cancel, the platformless queue, mixed-selection undo and
  redo, both invalidation reasons, the guard/inverse pure helpers, the `FakeVfs`
  chmod-does-not-touch-mtime pin, `RealVfs` real chmod + symlink-follow, stub
  ownership, and four `set_ownership` tests on macOS including the real EPERM
  refusal) and 6 integration in `tests/attr_ops.rs` over a real `tempfile` tree
  with `MacPlatform`, so the tag legs really write, read and undo the xattr. The
  "every `FileOp` variant" test in `tests/torture.rs` covers all three too.


## theme (crate)
- Not started (interim `theme` module lives inside `crates/app`).
