//! Inline rename overlay (ARCHITECTURE.md §4c, §8 "Inline rename overlay").
//!
//! [`RenameState`] is the §4c state machine — a field of [`DirView`], never
//! its own entity — built around one vendored [`InputState`] per `DirView`,
//! swapped into the row of the entry being renamed (`views/details_list.rs`
//! renders the swap). `F2` and a slow-second-click (armed by `DirView`'s own
//! click handling, on `Spawner::timer`) both call [`DirView::begin_rename`],
//! which preselects the file-name **stem** (extension excluded, via
//! [`fs_core::split_name`]). `Confirm` validates locally — nonempty, no path
//! separator — then submits a [`FileOp::Rename`] through the job queue and
//! shows a "processing" state with the pending name until the job's
//! terminal event: success moves the selection onto the new path and tears
//! the editor down; a collision (or any other error) the op itself reports
//! lands back in the still-open editor as an inline error. `Escape`, blur,
//! and navigating away (`DirView::cancel_rename_for_navigation`, called by
//! the pane when it loads a different directory) all tear the editor down
//! cleanly, restoring the pre-rename focus.
//!
//! **Naming a new entry** (§4c's `is_new_entry: true`, reached from
//! `cmd-shift-n` and from the context menu's `New ▸ Folder / Text file…`):
//! the *same* machine, opened by `DirView::begin_new_entry` on a **phantom
//! row** — a `FileEntry` for a path that does not exist yet, which
//! `DirView::projected_rows` appends so the row swap has somewhere to render.
//! `Confirm` submits `CreateDir`/`CreateFile` instead of `Rename`; everything
//! else (validation, the processing state, an inline collision error, escape,
//! blur, navigation) is shared code. Nothing reaches the disk until the name
//! is committed, so `Escape` leaves the directory exactly as it was.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use fs_core::{EntryId, EntryKind, FileEntry, FileOp, JobId, split_name};
use gpui::{
    Context, Entity, FocusHandle, ScrollStrategy, SharedString, Subscription, Window, prelude::*,
};

use crate::actions::{Cancel, Confirm};
use crate::app_state::FsContext;
use crate::dir_view::DirView;
use crate::input::InputState;
use crate::jobs_model::JobsEvent;

/// Wire an element to *be* the inline editor's dispatch node: the input's
/// focus handle (without `track_focus` the `TextInput` key context is not in
/// the dispatch chain and `Confirm`/`Cancel` are silently unreachable), the
/// `TextInput` key context, the rename machine's confirm/cancel, and the
/// vendored input's own editing actions (bound in `keymap.rs`'s `TextInput`
/// context).
///
/// Shared by every view mode that can host the editor — the details row
/// (`views/details_list.rs`) and the grid tile (`views/icon_grid.rs`) — so
/// the wiring, like every other command in §0, exists exactly once.
pub(crate) fn with_editor_actions<E: gpui::InteractiveElement>(
    element: E,
    input: &Entity<InputState>,
    cx: &mut Context<DirView>,
) -> E {
    use crate::input::text_input as ti;

    let focus = input.read(cx).focus_handle(cx);
    let mut element = element
        .track_focus(&focus)
        .key_context("TextInput")
        .on_action(cx.listener(|this, _: &Confirm, window, cx| this.confirm_rename(window, cx)))
        .on_action(cx.listener(|this, _: &Cancel, window, cx| this.cancel_rename(window, cx)));
    macro_rules! forward {
        ($action:ty, $method:ident) => {{
            let input = input.clone();
            element = element.on_action(cx.listener(move |_, action: &$action, window, cx| {
                input.update(cx, |input, cx| input.$method(action, window, cx))
            }));
        }};
    }
    forward!(ti::Left, left);
    forward!(ti::Right, right);
    forward!(ti::SelectLeft, select_left);
    forward!(ti::SelectRight, select_right);
    forward!(ti::SelectAll, select_all);
    forward!(ti::Home, home);
    forward!(ti::End, end);
    forward!(ti::Backspace, backspace);
    forward!(ti::Delete, delete);
    forward!(ti::Copy, copy);
    forward!(ti::Cut, cut);
    forward!(ti::Paste, paste);
    element
}

/// What a `New ▸` command is naming (ARCHITECTURE.md §4c `is_new_entry`).
/// The editor for these runs the **identical** machine a rename does; only the
/// op `Confirm` submits differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewEntryKind {
    Folder,
    File,
}

impl NewEntryKind {
    fn entry_kind(self) -> EntryKind {
        match self {
            NewEntryKind::Folder => EntryKind::Dir,
            NewEntryKind::File => EntryKind::File,
        }
    }

    fn op(self, path: PathBuf) -> FileOp {
        match self {
            NewEntryKind::Folder => FileOp::CreateDir { path },
            NewEntryKind::File => FileOp::CreateFile { path },
        }
    }
}

/// One in-flight rename edit. Lives at `DirView.rename`; dropping it (on any
/// teardown path) cancels its subscriptions.
pub(crate) struct RenameState {
    /// The entry being renamed, path-keyed so identity survives listing
    /// patches that don't touch it.
    target: EntryId,
    /// §4c `is_new_entry`: set when this editor is naming something that does
    /// **not exist yet**. Carries what `Confirm` will create and the phantom
    /// row the projection injects so the editor has somewhere to render.
    /// `None` for an ordinary rename.
    new_entry: Option<(NewEntryKind, FileEntry)>,
    /// The row editor — one per `DirView`, swapped into place.
    input: Entity<InputState>,
    /// Set once `Confirm` submits the op: the row shows this pending name
    /// (not editable) until the job's terminal event.
    processing: Option<SharedString>,
    /// The in-flight rename's destination, set alongside `processing` so
    /// completion can move the selection onto the new path.
    pending_to: Option<PathBuf>,
    /// Inline validation error — local (empty/separator) or reported by the
    /// op itself (collision) — rendered as a deferred popup under the row.
    error: Option<SharedString>,
    /// The submitted job, so a `JobsEvent` for an unrelated job is ignored.
    job: Option<JobId>,
    /// Focus to restore on teardown (Escape / blur / navigating away).
    prev_focus: Option<FocusHandle>,
    /// The window this editor was opened in. Every *user-driven* teardown has
    /// a `Window` to hand, but the one the watcher drives does not (it arrives
    /// through the pane's async batch pump), and focus is sitting on the
    /// editor's about-to-be-unpainted handle — so that path defers a focus
    /// restore through this handle rather than leaving the keyboard nowhere.
    window: gpui::AnyWindowHandle,
    /// Fires when the editor's focus handle loses focus while still editing
    /// (§4c "blur … tears the editor down cleanly").
    _blur_subscription: Subscription,
    _job_subscription: Option<Subscription>,
}

impl RenameState {
    pub(crate) fn target(&self) -> &EntryId {
        &self.target
    }

    pub(crate) fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    pub(crate) fn processing(&self) -> Option<&SharedString> {
        self.processing.as_ref()
    }

    pub(crate) fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// The phantom row this editor is sitting on, for an entry that does not
    /// exist yet (§4c). `None` for an ordinary rename.
    pub(crate) fn new_entry_row(&self) -> Option<&FileEntry> {
        self.new_entry.as_ref().map(|(_, row)| row)
    }

    /// True while this editor is naming a not-yet-created entry — the details
    /// row blanks its Size/Date cells, which have nothing to report yet.
    pub(crate) fn is_new_entry(&self) -> bool {
        self.new_entry.is_some()
    }
}

/// Local validation (§4c: "nonempty, no '/', not duplicate" minus the
/// duplicate check — a name collision is reported by the op itself, not
/// pre-checked here). Trims surrounding whitespace like a normal rename
/// dialog would.
pub(crate) fn validate_new_name(raw: &str) -> Result<String, SharedString> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SharedString::new_static("Name can't be empty"));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(SharedString::new_static(
            "Name can't contain a path separator",
        ));
    }
    Ok(trimmed.to_string())
}

/// The byte range of `name`'s stem (extension excluded), for the rename
/// editor's initial selection.
pub(crate) fn stem_range(name: &str) -> std::ops::Range<usize> {
    0..split_name(name).0.len()
}

impl DirView {
    /// `f2`: rename the cursor entry, if any.
    pub(crate) fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.projected_rows(cx);
        let Some(ix) = self.cursor_ix(&rows) else {
            return;
        };
        let entry = rows[ix].entry.clone();
        self.begin_rename(&entry, window, cx);
    }

    /// Open the row editor for `entry` (§4c), stem preselected. Called by
    /// `f2` and by a slow-second-click armed in `DirView`'s own click
    /// handling.
    pub fn begin_rename(
        &mut self,
        entry: &fs_core::FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_editor(entry.clone(), None, window, cx);
    }

    /// §0 `NewFolder` / `NewFile`, handed down by the pane (§0's handler
    /// column is literally "Pane → DirView"): open the **same** editor on a
    /// phantom row for `dir/name`, with the name preselected. Nothing is
    /// created until `Confirm`; `Escape` leaves the directory untouched.
    ///
    /// `name` is the pane's already-deconflicted placeholder ("New Folder",
    /// "New Folder 2", …), so the phantom's path cannot collide with a real
    /// row — which matters, because the row swap is keyed on that path.
    pub(crate) fn begin_new_entry(
        &mut self,
        kind: NewEntryKind,
        dir: &Path,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = FileEntry {
            path: Arc::from(dir.join(name).as_path()),
            name: Arc::from(name),
            kind: kind.entry_kind(),
            size: 0,
            // Never a wall-clock value: the phantom row renders no date at
            // all (`details_list`), so captured frames stay deterministic.
            modified: SystemTime::UNIX_EPOCH,
            created: None,
            hidden: false,
        };
        self.open_editor(entry, Some(kind), window, cx);
        // The phantom is appended last, so on a long listing it is off-screen
        // until the list scrolls to it. Its index is the length of the *last
        // painted* projection — read from this view's own state, never by
        // re-projecting, because the pane calls this from inside its own
        // `update` and reading it back would panic.
        if self.rename.is_some() {
            let last = self.flat_rows().len();
            self.scroll_handle()
                .scroll_to_item(last, ScrollStrategy::Nearest);
        }
    }

    /// The one editor-opening path, shared by rename and new-entry naming
    /// (§4c: "the identical machine with `is_new_entry: true`").
    fn open_editor(
        &mut self,
        entry: FileEntry,
        new_entry: Option<NewEntryKind>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rename.is_some() {
            return;
        }
        // A menu must not outlive the gesture that opened the editor.
        self.close_context_menu(window, cx);
        let prev_focus = window.focused(cx);
        let theme = self.theme().clone();
        let name = entry.name.to_string();
        let stem_end = stem_range(&name).end;
        let input = cx.new(|cx| {
            InputState::new(cx).with_colors(theme.muted, theme.accent, theme.accent.opacity(0.25))
        });
        input.update(cx, |input, cx| {
            input.set_value(name, window, cx);
            input.select_range(0..stem_end, window, cx);
        });
        let focus_handle = input.read(cx).focus_handle(cx);
        window.focus(&focus_handle, cx);
        // The vendored `InputState`'s own focus/blur hooks are never wired
        // to real window focus changes (nothing calls its `on_focus`/
        // `on_blur`) — use gpui's own per-handle focus-loss listener
        // instead, which fires precisely when this handle stops being the
        // focused element.
        let blur_subscription = cx.on_blur(&focus_handle, window, Self::on_rename_blur);
        self.rename = Some(RenameState {
            target: entry.id(),
            new_entry: new_entry.map(|kind| (kind, entry)),
            input,
            processing: None,
            pending_to: None,
            error: None,
            job: None,
            prev_focus,
            window: window.window_handle(),
            _blur_subscription: blur_subscription,
            _job_subscription: None,
        });
        cx.notify();
    }

    /// `Confirm` in the rename editor: local validation, then submit the op
    /// and switch to the "processing" state.
    pub(crate) fn confirm_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.rename.as_ref() else {
            return;
        };
        if rename.processing.is_some() {
            return; // already submitted; ignore a stray repeat Enter
        }
        let typed = rename.input.read(cx).content().to_string();
        let target = rename.target.clone();
        let new_entry = rename.new_entry.as_ref().map(|(kind, _)| *kind);

        let new_name = match validate_new_name(&typed) {
            Ok(name) => name,
            Err(message) => {
                self.rename.as_mut().expect("checked above").error = Some(message);
                cx.notify();
                return;
            }
        };
        let Some(parent) = target.0.parent() else {
            self.rename.as_mut().expect("checked above").error =
                Some(SharedString::new_static("Can't rename a root"));
            cx.notify();
            return;
        };
        let to = parent.join(&new_name);
        // Unchanged name: nothing to submit, just close cleanly — but only for
        // a real rename. Accepting a *new* entry's placeholder name unchanged
        // still has to create it.
        if new_entry.is_none() && to.as_path() == &*target.0 {
            self.cancel_rename(window, cx);
            return;
        }

        // §4c: the identical machine, differing only in the op it submits — so
        // a name collision comes back from the op as an inline error in the
        // still-open editor either way (`CreateDir`/`CreateFile` both fail on
        // an existing path).
        let op = match new_entry {
            Some(kind) => kind.op(to.clone()),
            None => FileOp::Rename {
                from: target.0.to_path_buf(),
                to: to.clone(),
            },
        };
        let job = FsContext::global(cx).queue.submit(op);
        let jobs = FsContext::global(cx).jobs.clone();
        let subscription = cx.subscribe_in(&jobs, window, move |this, _jobs, event, window, cx| {
            this.on_rename_job_event(job, event, window, cx);
        });

        let rename = self.rename.as_mut().expect("checked above");
        rename.processing = Some(SharedString::from(new_name));
        rename.pending_to = Some(to);
        rename.error = None;
        rename.job = Some(job);
        rename._job_subscription = Some(subscription);
        cx.notify();
    }

    /// `Escape` / blur: tear the editor down and restore the pre-rename
    /// focus.
    pub(crate) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        let focus = rename
            .prev_focus
            .unwrap_or_else(|| self.focus_handle_ref().clone());
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Navigating away (the pane is about to load a different directory):
    /// clear the editor without touching focus — the pane's own load path
    /// re-paints the view regardless.
    pub(crate) fn cancel_rename_for_navigation(&mut self, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            cx.notify();
        }
    }

    /// The row under the editor left the listing (an external delete, or a
    /// job of our own finishing on it, arriving as a watcher patch): drop the
    /// editor, because there is nothing left to rename.
    ///
    /// Without this the state machine deadlocks the whole view: the projection
    /// stops yielding the target row, so `details_list` never paints the editor
    /// (and its unpainted `TextInput` node leaves the dispatch tree, taking
    /// `escape` with it), while the root key context stays pinned to
    /// `DirView renaming` — killing every `DirView && !renaming` binding — and
    /// the marquee, the context menu and the row drags all bail on
    /// `rename.is_some()`. Only leaving the directory used to clear it.
    ///
    /// Called by the pane after any snapshot swap, with the snapshot it swapped
    /// in (so this never reads the pane back mid-update).
    pub(crate) fn cancel_rename_if_target_vanished(
        &mut self,
        snapshot: Option<&fs_core::ListingSnapshot>,
        cx: &mut Context<Self>,
    ) {
        let Some(rename) = self.rename.as_ref() else {
            return;
        };
        // A §4c new-entry phantom is injected unconditionally by the
        // projection and is never in the snapshot: its row cannot vanish.
        if rename.new_entry.is_some() {
            return;
        }
        // Once the op is in flight the row is *expected* to leave the listing
        // (that is the rename landing); the job's terminal event owns teardown.
        if rename.processing.is_some() {
            return;
        }
        let target = rename.target.clone();
        if self.listing_contains(snapshot, &target) {
            return;
        }
        let window = rename.window;
        let focus = rename
            .prev_focus
            .clone()
            .unwrap_or_else(|| self.focus_handle_ref().clone());
        self.cancel_rename_for_navigation(cx);
        // The editor still holds keyboard focus on a handle that is about to
        // stop being painted, and this teardown has no `Window` of its own
        // (the watcher batch arrives on the pane's async pump). Without this
        // the keyboard would land nowhere until the user clicked a row.
        cx.defer(move |cx| {
            window
                .update(cx, |_, window, cx| window.focus(&focus, cx))
                .ok();
        });
    }

    /// A blur while still editing (focus moved elsewhere) tears the editor
    /// down; a blur that fires once the job is already `processing` does
    /// not — the row's pending-name text is not focusable, so this only
    /// ever fires from a genuine window/user focus move.
    fn on_rename_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editing = self
            .rename
            .as_ref()
            .is_some_and(|rename| rename.processing.is_none());
        if editing {
            self.cancel_rename(window, cx);
        }
    }

    fn on_rename_job_event(
        &mut self,
        job: JobId,
        event: &JobsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_ours = self.rename.as_ref().is_some_and(|r| r.job == Some(job));
        if !is_ours {
            return;
        }
        match event {
            JobsEvent::Completed { id, .. } if *id == job => {
                let new_path = self.rename.as_ref().and_then(|r| r.pending_to.clone());
                self.rename = None;
                if let Some(new_path) = new_path {
                    self.set_cursor(Some(EntryId(Arc::from(new_path.as_path()))), cx);
                }
                window.focus(self.focus_handle_ref(), cx);
                cx.notify();
            }
            JobsEvent::Failed { id, error } if *id == job => {
                if let Some(rename) = self.rename.as_mut() {
                    rename.processing = None;
                    rename.pending_to = None;
                    rename.job = None;
                    rename._job_subscription = None;
                    rename.error = Some(SharedString::from(error.clone()));
                }
                cx.notify();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! §9 rename rows: the local validators, then the §4c machine driven
    //! through real key dispatch on a focused view — `f2` opens the editor
    //! with the stem preselected, `enter` submits the op and the selection
    //! follows the new path, a bad name and a collision both land back in the
    //! still-open editor, and `escape` / navigating away tear it down.

    use super::*;

    use std::path::Path;

    use crate::actions::Duplicate;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::dir_view::RENAME_CLICK_ARM_DELAY;
    use crate::pane::Pane;
    use crate::theme::Theme;
    use fs_core::{FakeVfs, Spawner, Vfs};
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use serde_json::json;

    #[test]
    fn validate_new_name_rejects_empty_and_separators() {
        assert!(validate_new_name("  ").is_err());
        assert!(validate_new_name("a/b").is_err());
        assert!(validate_new_name("a\\b").is_err());
        assert_eq!(validate_new_name("  report.pdf  ").unwrap(), "report.pdf");
    }

    #[test]
    fn stem_range_excludes_the_extension() {
        assert_eq!(stem_range("report.pdf"), 0..6);
        assert_eq!(stem_range(".dotfile"), 0..".dotfile".len());
        assert_eq!(stem_range("plain"), 0..5);
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({
                    "sub": { "inner.txt": "abc" },
                    "report.pdf": "pdf",
                    "a.txt": "..",
                }),
            );
            vfs.insert_tree("/other", json!({ "b.txt": "b" }));
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
            vfs
        })
    }

    fn entry_id(path: &str) -> EntryId {
        EntryId(Arc::from(Path::new(path)))
    }

    fn exists(vfs: &Arc<FakeVfs>, path: &str) -> bool {
        futures::executor::block_on(vfs.metadata(Path::new(path)))
            .unwrap()
            .is_some()
    }

    /// Root open, the details view focused, and the cursor on `report.pdf` —
    /// the starting point for every rename row below.
    fn open_root(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeVfs>,
        Entity<Pane>,
        Entity<DirView>,
        &mut VisualTestContext,
    ) {
        let vfs = init_test(cx);
        let (pane, cx) = cx.add_window_view(|window, cx| Pane::new(Theme::dark(), window, cx));
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        let dir_view = pane.read_with(cx, |pane, _| pane.dir_view().clone());
        dir_view.update(cx, |dir_view, cx| {
            dir_view.set_cursor(Some(entry_id("/root/report.pdf")), cx);
        });
        cx.update(|window, cx| {
            let handle = dir_view.read(cx).focus_handle_ref().clone();
            window.focus(&handle, cx);
        });
        (vfs, pane, dir_view, cx)
    }

    /// Replace the editor's text, as typing would.
    fn type_name(dir_view: &Entity<DirView>, text: &str, cx: &mut VisualTestContext) {
        let input = dir_view.read_with(cx, |dir_view, _| {
            dir_view
                .rename
                .as_ref()
                .expect("editor is up")
                .input()
                .clone()
        });
        cx.update(|window, cx| {
            input.update(cx, |input, cx| {
                input.set_value(text.to_string(), window, cx)
            })
        });
    }

    fn error_text(dir_view: &Entity<DirView>, cx: &mut VisualTestContext) -> Option<String> {
        dir_view.read_with(cx, |dir_view, _| {
            dir_view
                .rename
                .as_ref()
                .and_then(|rename| rename.error().map(ToString::to_string))
        })
    }

    #[gpui::test]
    fn f2_opens_the_editor_with_the_stem_preselected(cx: &mut TestAppContext) {
        let (_vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();

        dir_view.read_with(cx, |dir_view, cx| {
            let rename = dir_view.rename.as_ref().expect("f2 opened the editor");
            assert_eq!(rename.target(), &entry_id("/root/report.pdf"));
            let input = rename.input().read(cx);
            assert_eq!(input.content(), "report.pdf");
            assert_eq!(
                input.selected_range(),
                0..6,
                "the stem is preselected, the extension is not"
            );
            assert!(rename.processing().is_none());
            assert!(rename.error().is_none());
        });
    }

    #[gpui::test]
    fn confirm_submits_the_op_and_the_selection_follows_the_new_path(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        type_name(&dir_view, "final.pdf", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert!(exists(&vfs, "/root/final.pdf"), "the op ran");
        assert!(!exists(&vfs, "/root/report.pdf"));
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.rename.is_none(), "torn down on success");
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/final.pdf")));
        });
    }

    #[gpui::test]
    fn an_unchanged_name_closes_the_editor_without_an_op(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert!(exists(&vfs, "/root/report.pdf"));
        dir_view.read_with(cx, |dir_view, _| assert!(dir_view.rename.is_none()));
    }

    #[gpui::test]
    fn a_local_validation_failure_keeps_the_editor_open(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        type_name(&dir_view, "   ", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert_eq!(
            error_text(&dir_view, cx).as_deref(),
            Some("Name can't be empty")
        );
        dir_view.read_with(cx, |dir_view, _| {
            let rename = dir_view.rename.as_ref().expect("editor stays open");
            assert!(rename.processing().is_none(), "nothing was submitted");
        });
        assert!(exists(&vfs, "/root/report.pdf"));
    }

    #[gpui::test]
    fn a_collision_reported_by_the_op_lands_back_in_the_editor(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        type_name(&dir_view, "a.txt", cx);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert!(
            error_text(&dir_view, cx).is_some(),
            "the failed op's error is shown inline"
        );
        dir_view.read_with(cx, |dir_view, _| {
            let rename = dir_view.rename.as_ref().expect("editor stays open");
            assert!(
                rename.processing().is_none(),
                "back to editing, not stuck pending"
            );
        });
        // Neither side of the collision moved.
        assert!(exists(&vfs, "/root/report.pdf"));
        assert!(exists(&vfs, "/root/a.txt"));
    }

    #[gpui::test]
    fn escape_cancels_and_restores_the_previous_focus(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        type_name(&dir_view, "discarded.pdf", cx);
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        dir_view.read_with(cx, |dir_view, _| assert!(dir_view.rename.is_none()));
        assert!(exists(&vfs, "/root/report.pdf"), "escape submits nothing");
        let focused_dir_view = cx.update(|window, cx| {
            window.focused(cx) == Some(dir_view.read(cx).focus_handle_ref().clone())
        });
        assert!(focused_dir_view, "focus returns to the details view");
    }

    #[gpui::test]
    fn refresh_keeps_the_editor_but_leaving_the_directory_tears_it_down(cx: &mut TestAppContext) {
        let (_vfs, pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();

        // An in-place reload is not "navigating away" (§4c).
        pane.update(cx, |pane, cx| pane.refresh(cx));
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.rename.is_some(), "refresh keeps the editor");
        });

        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/other"), cx));
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.rename.is_none(), "leaving the directory closes it");
        });
    }

    // The row under the editor being deleted externally (or by one of our own
    // jobs finishing on it) used to leave `rename = Some(..)` forever: the
    // projection stops yielding the row, so no editor paints and its unpainted
    // `TextInput` node leaves the dispatch tree with `escape` inside it, while
    // the root context stays `DirView renaming` — killing every
    // `DirView && !renaming` binding — and the marquee, context menu and row
    // drags all keep bailing on `rename.is_some()`. Only navigating away
    // cleared it. The watcher makes that reachable with no user action at all.
    #[gpui::test]
    fn a_watcher_patch_that_removes_the_target_closes_the_editor(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| assert!(dir_view.rename.is_some()));

        // Deleted outside the app; the pane's watch fires a beat later.
        vfs.remove_path("/root/report.pdf");
        cx.executor().advance_clock(crate::pane::WATCH_LATENCY);
        cx.run_until_parked();

        dir_view.read_with(cx, |dir_view, _| {
            assert!(
                dir_view.rename.is_none(),
                "an editor whose row vanished must not survive the patch"
            );
            assert!(
                dir_view
                    .flat_rows()
                    .iter()
                    .all(|row| !row.entry.path.ends_with("report.pdf")),
                "and the row really is gone"
            );
        });

        // Focus came back to the list rather than staying on the editor's
        // unpainted handle — otherwise the keyboard would land nowhere.
        cx.update(|window, cx| {
            let view_handle = dir_view.read(cx).focus_handle_ref().clone();
            assert_eq!(
                window.focused(cx).as_ref(),
                Some(&view_handle),
                "the teardown handed keyboard focus back to the list"
            );
        });
        // The view is usable again: `cmd-a` is a `DirView && !renaming`
        // binding, so it only resolves once the `renaming` token is gone.
        cx.simulate_keystrokes("cmd-a");
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert_eq!(
                dir_view.selection().len(),
                dir_view.flat_rows().len(),
                "keyboard bindings live again"
            );
            assert!(!dir_view.selection().is_empty());
        });
    }

    // ...but a row leaving the listing because *this* rename landed is the
    // normal case, and the job's own terminal event owns that teardown (it is
    // what moves the selection onto the new name).
    #[gpui::test]
    fn the_pending_editor_survives_its_own_rename_leaving_the_listing(cx: &mut TestAppContext) {
        let (vfs, _pane, dir_view, cx) = open_root(cx);

        cx.simulate_keystrokes("f2");
        cx.run_until_parked();
        type_name(&dir_view, "renamed.pdf", cx);

        // Submit, then let the watcher patch land before the job's completion
        // event is delivered: the old row is gone from the listing, but the
        // editor is `processing` and must be left to its own job event.
        cx.update(|window, cx| {
            dir_view.update(cx, |dir_view, cx| dir_view.confirm_rename(window, cx))
        });
        dir_view.read_with(cx, |dir_view, _| {
            let rename = dir_view.rename.as_ref().expect("still up while processing");
            assert!(rename.processing.is_some());
        });
        cx.executor().advance_clock(crate::pane::WATCH_LATENCY);
        cx.run_until_parked();

        assert!(exists(&vfs, "/root/renamed.pdf"));
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.rename.is_none(), "the job event tore it down");
            assert_eq!(
                dir_view.cursor(),
                Some(&entry_id("/root/renamed.pdf")),
                "and the selection followed the new name, which is what the \
                 job event exists to do"
            );
        });
    }

    #[gpui::test]
    fn a_slow_second_click_renames_but_a_double_click_opens(cx: &mut TestAppContext) {
        let (_vfs, _pane, dir_view, cx) = open_root(cx);
        let entry = dir_view.read_with(cx, |dir_view, cx| {
            dir_view
                .projected_rows(cx)
                .into_iter()
                .find(|row| row.entry.path.ends_with("report.pdf"))
                .expect("report.pdf is listed")
                .entry
        });

        let click = |count: usize, cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                dir_view.update(cx, |dir_view, cx| {
                    dir_view.handle_row_click(&entry, Modifiers::default(), count, window, cx);
                })
            });
        };

        // A first click only selects — the row is not armed yet.
        click(1, cx);
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert!(dir_view.rename.is_none(), "the first click never renames");
            assert_eq!(dir_view.cursor(), Some(&entry_id("/root/report.pdf")));
        });

        // A fast second click is a double-click: it opens and disarms, so the
        // click after it starts the arming over rather than renaming.
        click(2, cx);
        cx.executor().advance_clock(RENAME_CLICK_ARM_DELAY);
        cx.run_until_parked();
        click(1, cx);
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            assert!(
                dir_view.rename.is_none(),
                "a double-click cancels the pending arm"
            );
        });

        // Once the arm delay has elapsed, the next plain click renames.
        cx.executor().advance_clock(RENAME_CLICK_ARM_DELAY);
        cx.run_until_parked();
        click(1, cx);
        cx.run_until_parked();
        dir_view.read_with(cx, |dir_view, _| {
            let rename = dir_view
                .rename
                .as_ref()
                .expect("the slow second click opened the editor");
            assert_eq!(rename.target(), &entry_id("/root/report.pdf"));
        });
    }

    #[gpui::test]
    fn duplicate_copies_the_selection_with_a_keep_both_name(cx: &mut TestAppContext) {
        let (vfs, _pane, _dir_view, cx) = open_root(cx);

        cx.dispatch_action(Duplicate);
        cx.run_until_parked();

        assert!(exists(&vfs, "/root/report.pdf"), "the source stays put");
        assert!(
            exists(&vfs, "/root/report copy.pdf"),
            "the duplicate gets a keep-both name"
        );
    }
}
