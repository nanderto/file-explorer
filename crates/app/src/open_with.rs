//! "Open with ▸ Zed / Visual Studio Code" and "Open Terminal Here" — the
//! [`DirView`] half of [`fs_core::apps`].
//!
//! Three pieces, mirroring [`crate::tags`]:
//!
//! * a **one-shot load** of the installed applications, because the context
//!   menu is built synchronously from [`crate::context_menu::MenuFacts`] and
//!   cannot await an OS call (nor may `render` touch the disk, §5);
//! * the **targets** each command acts on, derived from the selection;
//! * the **handlers** for the two actions, which hand off to
//!   [`fs_core::Platform`] on the background executor.
//!
//! The Space-switching behavior that motivates all of this lives in
//! `fs_core::apps` — read that module's docs before changing how anything is
//! launched.

use std::path::{Path, PathBuf};

use fs_core::apps::{AppKind, ExternalApp, enclosing_directory};
use gpui::{Context, Task};

use crate::actions::{OpenTerminalHere, OpenWith};
use crate::app_state::FsContext;
use crate::dir_view::DirView;

/// The installed applications the `Open with ▸` submenu offers, plus the
/// one-shot load that fills them.
#[derive(Default)]
pub(crate) struct AppState {
    /// What [`fs_core::Platform::external_apps`] reported. Empty until the
    /// load resolves, which is why the submenu is omitted rather than shown
    /// empty — an empty "Open with ▸" is worse than no "Open with ▸".
    apps: Vec<ExternalApp>,
    /// The one-shot load; a field, never detached (§5).
    _load: Option<Task<()>>,
}

impl AppState {
    /// Drop the discovered applications, for the test that covers the
    /// not-yet-discovered state the menu represents by omitting its rows.
    #[cfg(test)]
    pub(crate) fn forget_for_test(&mut self) {
        self.apps.clear();
    }
}

impl DirView {
    /// The applications the `Open with ▸` submenu offers. Never reads the
    /// disk — `render` must not (§5).
    pub(crate) fn external_apps(&self) -> &[ExternalApp] {
        &self.apps.apps
    }

    /// The one-shot `external_apps` load. Idempotent: every watcher patch and
    /// every scrollbar fade re-renders, and the installed application set is
    /// not going to change under us in a way worth re-probing for.
    pub(crate) fn load_external_apps(&mut self, cx: &mut Context<Self>) {
        if self.apps._load.is_some() {
            return;
        }
        let platform = FsContext::global(cx).platform.clone();
        self.apps._load = Some(cx.spawn(async move |this, cx| {
            let apps = cx
                .background_executor()
                .spawn(async move { platform.external_apps().await })
                .await;
            if !apps.is_empty() {
                this.update(cx, |this, cx| {
                    this.apps.apps = apps;
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// What `Open with ▸` should hand over: every selected path, or the cursor
    /// row when nothing is selected — the same rule
    /// [`DirView::open_selected`] follows, so the two commands never disagree
    /// about what "this" means.
    pub(crate) fn open_with_targets(&self, cx: &Context<Self>) -> Vec<PathBuf> {
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
    pub(crate) fn terminal_target(&self, cx: &Context<Self>) -> Option<PathBuf> {
        match self.command_targets(cx).first() {
            Some((path, is_dir)) => enclosing_directory(path, *is_dir),
            None => self
                .pane_entity()
                .and_then(|pane| pane.read(cx).path().map(|p| p.to_path_buf())),
        }
    }

    /// The selection as `(path, is_dir)`, or the cursor row when the selection
    /// is empty.
    fn command_targets(&self, cx: &Context<Self>) -> Vec<(PathBuf, bool)> {
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
        // Terminal is opened *at* a folder even from the `Open with` submenu,
        // so a file target resolves to its parent rather than being handed to
        // a terminal that cannot show it.
        let targets: Vec<PathBuf> = if action.app.kind.wants_directory() {
            self.command_targets(cx)
                .into_iter()
                .filter_map(|(path, is_dir)| enclosing_directory(&path, is_dir))
                .take(1)
                .collect()
        } else {
            targets
        };
        self.launch(action.app.clone(), targets, cx);
    }

    /// `Open Terminal Here`.
    pub(crate) fn handle_open_terminal_here(
        &mut self,
        _: &OpenTerminalHere,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = self.terminal_target(cx) else {
            return;
        };
        let Some(terminal) = self
            .external_apps()
            .iter()
            .find(|app| app.kind == AppKind::Terminal)
            .cloned()
        else {
            return;
        };
        self.launch(terminal, vec![dir], cx);
    }

    /// Spawn the launches on the background executor — the UI thread never
    /// touches the disk, and spawning a process is disk work (§5).
    ///
    /// Failures become a log line rather than a dialog: the command either
    /// opens a window the user can see or it does not, and an app that was
    /// discovered a moment ago failing to start is not something they can act
    /// on from here.
    fn launch(&mut self, app: ExternalApp, targets: Vec<PathBuf>, cx: &mut Context<Self>) {
        if targets.is_empty() {
            return;
        }
        let platform = FsContext::global(cx).platform.clone();
        cx.background_executor()
            .spawn(async move {
                for target in targets {
                    if let Err(error) = platform.open_with(&app, &target).await {
                        log_launch_failure(app.label(), &target, &error);
                    }
                }
            })
            .detach();
    }
}

fn log_launch_failure(label: &str, target: &Path, error: &anyhow::Error) {
    eprintln!("open with {label} ({}): {error:#}", target.display());
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use fs_core::{FakeVfs, Spawner, StubLaunch, StubPlatform};
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

    /// The apps discovery found. The load is driven by `render` (via
    /// `request_tags`), so by the time the harness has parked it has already
    /// resolved — no explicit call needed, which is the point of
    /// [`discovery_is_driven_by_render`].
    fn loaded_apps(view: &Entity<DirView>, cx: &mut VisualTestContext) -> Vec<ExternalApp> {
        cx.run_until_parked();
        view.read_with(cx, |view, _| view.external_apps().to_vec())
    }

    fn app(kind: AppKind, apps: &[ExternalApp]) -> ExternalApp {
        apps.iter()
            .find(|a| a.kind == kind)
            .unwrap_or_else(|| panic!("{kind:?} not discovered"))
            .clone()
    }

    /// The submenu's contents arrive without anybody asking for them: the
    /// load is kicked off from `render`, like the `Tags ▸` one, because the
    /// menu is built synchronously and cannot await.
    #[gpui::test]
    fn discovery_is_driven_by_render(cx: &mut TestAppContext) {
        let (_platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        let apps = view.read_with(cx, |view, _| view.external_apps().to_vec());
        assert_eq!(
            apps.iter().map(|a| a.kind).collect::<Vec<_>>(),
            AppKind::ALL.to_vec(),
            "a rendered pane has already discovered its applications"
        );
    }

    #[gpui::test]
    fn open_with_hands_every_selected_path_to_the_chosen_app(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        let apps = loaded_apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt"), Path::new("/root/b.txt")], cx);
            view.handle_open_with(
                &OpenWith {
                    app: app(AppKind::Zed, &apps),
                },
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![
                StubLaunch {
                    app: Some(AppKind::Zed),
                    target: PathBuf::from("/root/a.txt"),
                },
                StubLaunch {
                    app: Some(AppKind::Zed),
                    target: PathBuf::from("/root/b.txt"),
                },
            ]
        );
    }

    /// "Here" means the folder a file lives in, not the file.
    #[gpui::test]
    fn open_terminal_here_uses_the_parent_of_a_selected_file(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        loaded_apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx);
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![StubLaunch {
                app: Some(AppKind::Terminal),
                target: PathBuf::from("/root"),
            }]
        );
    }

    #[gpui::test]
    fn open_terminal_here_uses_a_selected_folder_itself(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        loaded_apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/sub")], cx);
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![StubLaunch {
                app: Some(AppKind::Terminal),
                target: PathBuf::from("/root/sub"),
            }]
        );
    }

    /// Nothing selected ⇒ the folder on screen.
    #[gpui::test]
    fn open_terminal_here_falls_back_to_the_open_folder(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        loaded_apps(&view, cx);
        view.update(cx, |view, cx| {
            view.handle_open_terminal_here(&OpenTerminalHere, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![StubLaunch {
                app: Some(AppKind::Terminal),
                target: PathBuf::from("/root"),
            }]
        );
    }

    /// One terminal, never one per selected row.
    #[gpui::test]
    fn a_multi_selection_still_opens_exactly_one_terminal(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        loaded_apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt"), Path::new("/root/b.txt")], cx);
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert_eq!(platform.launches().len(), 1);
    }

    /// `Open with ▸ Terminal` would be nonsense against a file, so the handler
    /// resolves to the enclosing folder there too.
    #[gpui::test]
    fn open_with_terminal_resolves_a_file_to_its_folder(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        let apps = loaded_apps(&view, cx);
        view.update(cx, |view, cx| {
            view.select_paths(&[Path::new("/root/a.txt")], cx);
            view.handle_open_with(
                &OpenWith {
                    app: app(AppKind::Terminal, &apps),
                },
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            platform.launches(),
            vec![StubLaunch {
                app: Some(AppKind::Terminal),
                target: PathBuf::from("/root"),
            }]
        );
    }

    /// With no terminal discovered the handler does nothing rather than
    /// panic — the state the menu represents by omitting the row.
    #[gpui::test]
    fn open_terminal_here_is_inert_without_a_discovered_terminal(cx: &mut TestAppContext) {
        let (platform, pane, cx) = open_root(cx);
        let view = dir_view(&pane, cx);
        loaded_apps(&view, cx);
        view.update(cx, |view, cx| {
            view.apps.forget_for_test();
            view.handle_open_terminal_here(&OpenTerminalHere, cx);
        });
        cx.run_until_parked();
        assert!(platform.launches().is_empty());
    }
}
