//! "Open with ▸" and "Open Terminal Here" — the [`DirView`] half of
//! [`fs_core::apps`].
//!
//! The list is **type-aware**: Launch Services answers "which applications can
//! open *this*", so a `.rs` file and a folder get different submenus. That
//! makes the cache shape different from [`crate::tags`]'s one-shot load — it
//! is keyed by *type*, not loaded once, because the answer changes as the
//! cursor moves between a folder and a file.
//!
//! Keying by type rather than by path is what keeps it cheap: every `.rs` file
//! in a folder shares one Launch Services query.
//!
//! The Space-switching behavior that motivates all of this lives in
//! `fs_core::apps` — read that module's docs before changing how anything is
//! launched.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use fs_core::apps::{ExternalApp, enclosing_directory};
use gpui::{App, Context, Task};

use crate::actions::{OpenTerminalHere, OpenWith};
use crate::app_state::FsContext;
use crate::dir_view::DirView;

/// What a Launch Services answer is cached against.
///
/// Folders are one bucket; files are bucketed by lowercased extension, which
/// is exactly the granularity Launch Services itself answers at. A file with
/// no extension gets its own bucket rather than being lumped in with folders.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AppsKey {
    Directory,
    Extension(Option<String>),
}

impl AppsKey {
    pub(crate) fn of(path: &Path, is_dir: bool) -> Self {
        if is_dir {
            AppsKey::Directory
        } else {
            AppsKey::Extension(
                path.extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase()),
            )
        }
    }
}

/// The per-type application cache behind `Open with ▸`, plus the terminal.
#[derive(Default)]
pub(crate) struct AppState {
    /// Launch Services' answer per type. Absent means "not asked yet", which
    /// is why the submenu is omitted rather than shown empty.
    by_type: HashMap<AppsKey, Vec<ExternalApp>>,
    /// Keys with a query in flight, so a re-render does not re-ask.
    in_flight: HashSet<AppsKey>,
    /// The preferred installed terminal, once discovered.
    terminal: Option<ExternalApp>,
    terminal_requested: bool,
    /// Live query tasks; fields, never detached (§5), so they die with the view.
    _tasks: Vec<Task<()>>,
}

impl AppState {
    /// Drop everything discovered, for the tests that cover the
    /// not-yet-discovered state the menu represents by omitting its rows.
    #[cfg(test)]
    pub(crate) fn forget_for_test(&mut self) {
        self.by_type.clear();
        self.terminal = None;
    }
}

impl DirView {
    /// The applications `Open with ▸` should offer for the current target.
    /// Never reads the disk — `render` must not (§5).
    pub(crate) fn external_apps(&self, cx: &App) -> &[ExternalApp] {
        let Some(key) = self.menu_target_key(cx) else {
            return &[];
        };
        self.apps
            .by_type
            .get(&key)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// The terminal `Open Terminal Here` should use, once discovered.
    pub(crate) fn terminal_app(&self) -> Option<&ExternalApp> {
        self.apps.terminal.as_ref()
    }

    /// Keep the applications for every type **on screen** loaded, plus the
    /// terminal.
    ///
    /// Prefetching the whole visible projection rather than just the current
    /// selection is not eagerness: a right-click both *changes* the selection
    /// and opens the menu in one gesture, so querying only on selection change
    /// leaves the first right-click on a new file type with no `Open with ▸`
    /// at all — the menu is built synchronously and cannot wait for the
    /// answer. Keying by type is what makes this cheap: a folder of a thousand
    /// `.rs` files is one query.
    ///
    /// Idempotent per key, for the same reason [`DirView::request_tags`] is:
    /// every arriving result and every scrollbar fade re-renders, and a query
    /// re-issued on each repaint would cancel its own answer.
    pub(crate) fn request_external_apps(&mut self, cx: &mut Context<Self>) {
        self.request_terminal(cx);
        for key in self.visible_app_keys(cx) {
            self.request_apps_for_key(key, cx);
        }
    }

    /// The distinct types visible in the current projection, plus the open
    /// folder (which the background menu acts on).
    ///
    /// Capped: a pathological folder with thousands of distinct extensions
    /// would otherwise issue thousands of queries for a menu nobody is going
    /// to open. The cap is generous enough that an ordinary folder is covered
    /// entirely.
    fn visible_app_keys(&self, cx: &App) -> Vec<(PathBuf, AppsKey)> {
        const MAX_TYPES: usize = 32;
        let mut out: Vec<(PathBuf, AppsKey)> = Vec::new();
        let mut seen: HashSet<AppsKey> = HashSet::new();
        // The open folder first: it is what a background right-click uses.
        if let Some(dir) = self
            .pane_entity()
            .and_then(|pane| pane.read(cx).path().map(|p| p.to_path_buf()))
        {
            seen.insert(AppsKey::Directory);
            out.push((dir, AppsKey::Directory));
        }
        for row in self.projected_rows(cx) {
            if out.len() >= MAX_TYPES {
                break;
            }
            let path = row.entry.path.to_path_buf();
            let key = AppsKey::of(&path, row.entry.is_dir_like());
            if seen.insert(key.clone()) {
                out.push((path, key));
            }
        }
        out
    }

    fn request_terminal(&mut self, cx: &mut Context<Self>) {
        if self.apps.terminal_requested {
            return;
        }
        self.apps.terminal_requested = true;
        let platform = FsContext::global(cx).platform.clone();
        let task = cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { platform.terminal().await })
                .await;
            if let Some(found) = found {
                this.update(cx, |this, cx| {
                    this.apps.terminal = Some(found);
                    cx.notify();
                })
                .ok();
            }
        });
        self.apps._tasks.push(task);
    }

    fn request_apps_for_key(&mut self, (target, key): (PathBuf, AppsKey), cx: &mut Context<Self>) {
        if self.apps.by_type.contains_key(&key) || self.apps.in_flight.contains(&key) {
            return;
        }
        self.apps.in_flight.insert(key.clone());
        let platform = FsContext::global(cx).platform.clone();
        let task = cx.spawn(async move |this, cx| {
            let apps = cx
                .background_executor()
                .spawn(async move { platform.apps_for(&target).await })
                .await;
            this.update(cx, |this, cx| {
                this.apps.in_flight.remove(&key);
                this.apps.by_type.insert(key, apps);
                cx.notify();
            })
            .ok();
        });
        self.apps._tasks.push(task);
    }

    /// The path a context menu would act on, and its cache key.
    fn menu_target(&self, cx: &App) -> Option<(PathBuf, AppsKey)> {
        match self.command_targets(cx).into_iter().next() {
            Some((path, is_dir)) => {
                let key = AppsKey::of(&path, is_dir);
                Some((path, key))
            }
            // Nothing selected: the background menu acts on the open folder.
            None => self
                .pane_entity()
                .and_then(|pane| pane.read(cx).path().map(|p| p.to_path_buf()))
                .map(|path| (path, AppsKey::Directory)),
        }
    }

    fn menu_target_key(&self, cx: &App) -> Option<AppsKey> {
        self.menu_target(cx).map(|(_, key)| key)
    }

    /// What `Open with ▸` hands over: every selected path, or the cursor row
    /// when nothing is selected — the same rule [`DirView::open_selected`]
    /// follows, so the two commands never disagree about what "this" means.
    pub(crate) fn open_with_targets(&self, cx: &App) -> Vec<PathBuf> {
        self.command_targets(cx)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    /// The folder "Open Terminal Here" should open.
    ///
    /// A selected folder means *that* folder; a selected file means the folder
    /// it lives in; nothing selected means the folder on screen. One terminal,
    /// never one per selected file — opening nine windows because nine rows
    /// were selected is not what anybody means by "here".
    pub(crate) fn terminal_target(&self, cx: &App) -> Option<PathBuf> {
        match self.command_targets(cx).first() {
            Some((path, is_dir)) => enclosing_directory(path, *is_dir),
            None => self
                .pane_entity()
                .and_then(|pane| pane.read(cx).path().map(|p| p.to_path_buf())),
        }
    }

    /// The selection as `(path, is_dir)`, or the cursor row when the selection
    /// is empty.
    pub(crate) fn command_targets(&self, cx: &App) -> Vec<(PathBuf, bool)> {
        let rows = self.projected_rows(cx);
        let selected = self.selection().selected_rootmost();
        let chosen: Vec<&crate::dir_view::ProjectedRow> = if selected.is_empty() {
            self.cursor_ix(&rows)
                .and_then(|ix| rows.get(ix))
                .into_iter()
                .collect()
        } else {
            rows.iter()
                .filter(|row| {
                    selected
                        .iter()
                        .any(|p| p.as_ref() == row.entry.path.as_ref())
                })
                .collect()
        };
        chosen
            .into_iter()
            .map(|row| (row.entry.path.to_path_buf(), row.entry.is_dir_like()))
            .collect()
    }

    /// `Open with ▸ <app>`: hand every target to `app`.
    pub(crate) fn handle_open_with(&mut self, action: &OpenWith, cx: &mut Context<Self>) {
        let targets = self.open_with_targets(cx);
        self.launch(action.app.clone(), targets, cx);
    }

    /// `Open Terminal Here`.
    pub(crate) fn handle_open_terminal_here(
        &mut self,
        _: &OpenTerminalHere,
        cx: &mut Context<Self>,
    ) {
        let (Some(dir), Some(terminal)) = (self.terminal_target(cx), self.terminal_app().cloned())
        else {
            return;
        };
        self.launch(terminal, vec![dir], cx);
    }

    /// Spawn the launches on the background executor — the UI thread never
    /// touches the disk, and spawning a process is disk work (§5).
    ///
    /// Failures become a log line rather than a dialog: the command either
    /// opens a window the user can see or it does not, and an application that
    /// Launch Services reported a moment ago failing to start is not something
    /// they can act on from here.
    fn launch(&mut self, app: ExternalApp, targets: Vec<PathBuf>, cx: &mut Context<Self>) {
        if targets.is_empty() {
            return;
        }
        let platform = FsContext::global(cx).platform.clone();
        cx.background_executor()
            .spawn(async move {
                for target in targets {
                    if let Err(error) = platform.open_with(&app, &target).await {
                        eprintln!(
                            "open with {} ({}): {error:#}",
                            app.label(),
                            target.display()
                        );
                    }
                }
            })
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use fs_core::{
        FakeVfs, STUB_PLAIN_APP, STUB_RECIPE_APP, STUB_TERMINAL_APP, Spawner, StubLaunch,
        StubPlatform,
    };
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;

    use super::*;
    use crate::app_state::GpuiSpawner;
    use crate::pane::Pane;

    /// A pane over `/root`, plus the [`StubPlatform`] it was installed with so
    /// the test can read back what would have been launched. The stub records
    /// rather than launches — a headless test must never spawn a GUI app.
    fn open_root(
        cx: &mut TestAppContext,
    ) -> (Arc<StubPlatform>, Entity<Pane>, &mut VisualTestContext) {
        let platform = Arc::new(StubPlatform::new());
        let handle = platform.clone();
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/root",
                json!({ "sub": { "deep.txt": "d" }, "a.txt": "a", "b.txt": "b" }),
            );
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs,
                spawner,
                Arc::new(crate::app_state::LoggingOpener),
                platform,
            );
        });
        let (pane, cx) = cx.add_window_view(Pane::new);
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();
        (handle, pane, cx)
    }

    fn dir_view(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Entity<DirView> {
        pane.read_with(cx, |pane, _| pane.dir_view().clone())
    }

    fn apps(view: &Entity<DirView>, cx: &mut VisualTestContext) -> Vec<ExternalApp> {
        cx.run_until_parked();
        view.update(cx, |view, cx| view.external_apps(cx).to_vec())
    }

    fn recipe_app(view: &Entity<DirView>, cx: &mut VisualTestContext) -> ExternalApp {
        apps(view, cx)
            .into_iter()
            .find(|a| a.bundle_id == STUB_RECIPE_APP)
            .expect("the stub's recipe app")
    }

    /// The submenu's contents arrive without anybody asking: the query is
    /// kicked off from `render`, because the menu is built synchronously.
    #[gpui::test]
    fn discovery_is_driven_by_render(cx: &mut TestAppContext) {
        let (_platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        let ids: Vec<String> = apps(&view, cx).into_iter().map(|a| a.bundle_id).collect();
        assert_eq!(
            ids,
            vec![STUB_RECIPE_APP.to_string(), STUB_PLAIN_APP.to_string()]
        );
    }

    /// The whole point of keying by type: `/root` holds two `.txt` files and
    /// a folder, so the cache settles at exactly two entries — one per type —
    /// not one per file.
    #[gpui::test]
    fn the_cache_holds_one_entry_per_type_not_per_file(cx: &mut TestAppContext) {
        let (_platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        cx.run_until_parked();
        let keys: HashSet<AppsKey> =
            view.read_with(cx, |view, _| view.apps.by_type.keys().cloned().collect());
        assert_eq!(
            keys,
            HashSet::from([AppsKey::Directory, AppsKey::Extension(Some("txt".into()))]),
            "two .txt files and a folder are two Launch Services queries"
        );
    }

    /// The prefetch covers what is on screen, so the very first right-click on
    /// a file type already has its submenu — a right-click changes the
    /// selection and opens the menu in one gesture, with no chance to await.
    #[gpui::test]
    fn a_types_apps_are_ready_before_it_is_ever_selected(cx: &mut TestAppContext) {
        let (_platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx)
        });
        // Deliberately no park: the answer must already be cached.
        let apps = view.update(cx, |view, cx| view.external_apps(cx).to_vec());
        assert!(!apps.is_empty(), "the submenu is warm on first right-click");
    }

    #[gpui::test]
    fn open_with_hands_every_selected_path_to_the_chosen_app(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        let app = recipe_app(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt"), Path::new("/root/b.txt")], cx);
            view.handle_open_with(&OpenWith { app }, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![
                StubLaunch {
                    app: Some(STUB_RECIPE_APP.to_string()),
                    target: PathBuf::from("/root/a.txt"),
                },
                StubLaunch {
                    app: Some(STUB_RECIPE_APP.to_string()),
                    target: PathBuf::from("/root/b.txt"),
                },
            ]
        );
    }

    /// An application without a recipe is still offered and still launches —
    /// it just cannot promise the Space.
    #[gpui::test]
    fn an_app_without_a_recipe_still_launches(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        let plain = apps(&view, cx)
            .into_iter()
            .find(|a| a.bundle_id == STUB_PLAIN_APP)
            .unwrap();
        assert!(!plain.keeps_current_space());
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx);
            view.handle_open_with(&OpenWith { app: plain }, cx);
        });
        cx.run_until_parked();
        assert_eq!(platform.launches().len(), 1);
    }

    /// "Here" means the folder a file lives in, not the file.
    #[gpui::test]
    fn open_terminal_here_uses_the_parent_of_a_selected_file(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx);
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![StubLaunch {
                app: Some(STUB_TERMINAL_APP.to_string()),
                target: PathBuf::from("/root"),
            }]
        );
    }

    #[gpui::test]
    fn open_terminal_here_uses_a_selected_folder_itself(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/sub")], cx);
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![StubLaunch {
                app: Some(STUB_TERMINAL_APP.to_string()),
                target: PathBuf::from("/root/sub"),
            }]
        );
    }

    /// Nothing selected ⇒ the folder on screen.
    #[gpui::test]
    fn open_terminal_here_falls_back_to_the_open_folder(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        apps(&view, cx);
        view.update(cx, |view, cx| {
            view.handle_open_terminal_here(&OpenTerminalHere, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![StubLaunch {
                app: Some(STUB_TERMINAL_APP.to_string()),
                target: PathBuf::from("/root"),
            }]
        );
    }

    /// One terminal, never one per selected row.
    #[gpui::test]
    fn a_multi_selection_still_opens_exactly_one_terminal(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt"), Path::new("/root/b.txt")], cx);
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert_eq!(platform.launches().len(), 1);
    }

    /// With no terminal discovered the handler does nothing rather than
    /// panic — the state the menu represents by omitting the row.
    #[gpui::test]
    fn open_terminal_here_is_inert_without_a_discovered_terminal(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        apps(&view, cx);
        view.update(cx, |view, cx| {
            view.apps.forget_for_test();
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert!(platform.launches().is_empty());
    }

    #[test]
    fn the_cache_key_buckets_by_type() {
        assert_eq!(AppsKey::of(Path::new("/a/b"), true), AppsKey::Directory);
        assert_eq!(
            AppsKey::of(Path::new("/a/b.TXT"), false),
            AppsKey::Extension(Some("txt".into())),
            "extension matching is case-insensitive"
        );
        assert_eq!(
            AppsKey::of(Path::new("/a/README"), false),
            AppsKey::Extension(None),
            "an extensionless file is its own bucket, not a folder"
        );
    }
}
