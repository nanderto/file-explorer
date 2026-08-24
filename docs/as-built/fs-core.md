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

## theme (crate)
- Not started (interim `theme` module lives inside `crates/app`).
