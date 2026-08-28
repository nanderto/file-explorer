//! Visual regression test runner.
//!
//! Renders real windows off-screen with GPUI's `VisualTestAppContext`
//! (real Metal rendering + deterministic TestDispatcher), captures
//! screenshots, and compares them against baseline PNGs committed under
//! `crates/app/test_fixtures/visual_tests/`.
//!
//! macOS-only at runtime (Metal); compiles to a stub elsewhere.
//!
//! Run the tests:
//!   cargo run -p file-explorer-app --bin visual_test_runner --features visual-tests
//!
//! Update baselines (when the UI intentionally changes):
//!   UPDATE_BASELINE=1 cargo run -p file-explorer-app --bin visual_test_runner --features visual-tests
//!
//! Environment:
//!   UPDATE_BASELINE        - write baselines instead of comparing
//!   VISUAL_TEST_OUTPUT_DIR - where screenshots/diffs go (default: target/visual_tests)

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("visual_test_runner only runs on macOS (it needs the Metal renderer).");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    macos::main()
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context as _, Result, anyhow, bail};
    use file_explorer_app::app_state::{FsContext, GpuiSpawner, LoggingOpener};
    use file_explorer_app::dir_view::DirView;
    use file_explorer_app::info_panel::PermField;
    use file_explorer_app::pane::{Pane, ViewMode};
    use file_explorer_app::{Theme, Workspace, keymap, visual_diff};
    use fs_core::{FakeVfs, FileOp, SortKey, Spawner, Tag, TagColor, Vfs};
    use gpui::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, Modifiers, MouseButton, Pixels,
        VisualTestAppContext, point, px, size,
    };
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// Minimum fraction of matching pixels for a test to pass.
    const MATCH_THRESHOLD: f64 = 0.99;
    /// Per-channel tolerance, absorbs GPU/AA rounding noise.
    const PIXEL_TOLERANCE: u8 = 3;
    /// Window size for all scenarios; must stay stable or baselines churn.
    const WINDOW_SIZE: (f32, f32) = (1200.0, 760.0);

    pub fn main() {
        let update_baseline = std::env::var("UPDATE_BASELINE").is_ok();
        match run(update_baseline) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("visual tests failed: {e:#}");
                std::process::exit(1);
            }
        }
    }

    /// UI state a scenario sets up after the window opens (§9 testing map).
    #[derive(Clone, Copy)]
    enum Setup {
        /// The boot state: no folder open.
        None,
        /// Navigate the active pane to a fixture directory.
        Navigate(&'static str),
        /// Navigate, then sort by a column.
        NavigateSorted(&'static str, SortKey),
        /// Navigate, then swap the breadcrumb for the path editor
        /// (prefilled + autocomplete popup from the fixture).
        AddressBarEditing(&'static str),
        /// Navigate, then expand sidebar folder-tree nodes (M2).
        SidebarTreeExpanded(&'static str, &'static [&'static str]),
        /// Navigate, then expand folders in place in the details view (M2).
        DetailsFolderExpanded(&'static str, &'static [&'static str]),
        /// Navigate, then submit a copy that parks on a conflict so the
        /// workspace opens the conflict modal (M3).
        ConflictDialogOpen(&'static str),
        /// Navigate, then open the inline rename editor on one entry with its
        /// stem preselected (M3, §4c).
        RenameEditing(&'static str, &'static str),
        /// Navigate, select entries and `Cut` them: the sources render dimmed
        /// (M3, plan §3 "cut items render dimmed").
        CutSelection(&'static str, &'static [&'static str]),
        /// Navigate, then right-click the empty space below the last row so the
        /// **background** context menu is open (M3, §8).
        ContextMenuOpen(&'static str),
        /// Navigate, then press in the empty space below the rows and drag up
        /// **without releasing**, so the rubber band and the rows it has
        /// selected are both live in the captured frame (M3, §8).
        MarqueeActive(&'static str),
        /// Navigate, switch the pane to the icon grid and select entries, so
        /// one frame pins the tile lattice, the placeholder image slot, the
        /// truncating labels and the selection tint together (M4, §8).
        IconGrid(&'static str, &'static [&'static str]),
        /// Navigate, split the workspace, then navigate the **new** pane
        /// somewhere else (M4, §2): one frame pins the two-pane strip, the
        /// divider, the per-pane active marker and the fresh pane's
        /// complementary view mode (a details list beside an icon grid, the
        /// plan §2 blueprint) together.
        SplitPanes(&'static str, &'static str),
        /// Navigate, then select one entry so the M5 info panel paints its
        /// preview, header, General rows and the read-only Permissions grid
        /// (ARCHITECTURE §8's `info_panel_jpeg` row).
        InfoPanelSelection(&'static str, &'static str),
        /// Navigate, then select several entries so the info panel paints the
        /// §2 multi-selection summary instead — the other half of the M5
        /// panel, and the state that must *not* show one row's mode as if it
        /// spoke for all of them (M5, §8).
        InfoPanelMultiSelection(&'static str, &'static [&'static str]),
        /// Navigate, select one entry, collapse **General** and open the info
        /// panel's **octal editor** (M6b): one frame pins the now-live
        /// Permissions grid (full-strength checkboxes, editable Owner/Group
        /// boxes) together with an open inline editor, which is the state a
        /// click on a field leaves the panel in. General is collapsed because
        /// the whole section does not otherwise fit the capture height.
        InfoPanelPermissions(&'static str, &'static str),
        /// Navigate, then turn on the sidebar's tag filter for a seeded tag
        /// (M6b): pins the **Tags** section with an active row, the rows the
        /// filter kept, and the tag dots painted after their names.
        TagFilter(&'static str, &'static str),
        /// Navigate, then type a query into the toolbar search field (M6a,
        /// §0 "Search field focus"), with the third argument turning
        /// "Subfolders" on. One frame pins the focused field, the toggle in
        /// the state that argument chose, the filtered/flat result rows (with
        /// their containing-folder labels when the walk is recursive) and the
        /// search flavor of the status line together. The walk is waited out
        /// before the capture — see `settle_search`.
        SearchActive(&'static str, &'static str, bool),
    }

    /// Every visual scenario: (name, theme, setup). Add new UI states here.
    /// All content comes from the deterministic FakeVfs fixture below — fixed
    /// sizes, counter-based mtimes, no wall clock.
    fn scenarios() -> Vec<(&'static str, Theme, Setup)> {
        vec![
            ("workspace_dark", Theme::dark(), Setup::None),
            ("workspace_light", Theme::light(), Setup::None),
            ("listing_populated", Theme::dark(), Setup::Navigate("/home")),
            (
                "listing_sorted_by_size",
                Theme::dark(),
                Setup::NavigateSorted("/home", SortKey::Size),
            ),
            (
                "address_bar_editing",
                Theme::dark(),
                Setup::AddressBarEditing("/home/Documents"),
            ),
            (
                "sidebar_tree_expanded",
                Theme::dark(),
                Setup::SidebarTreeExpanded("/home", &["/", "/home"]),
            ),
            (
                "details_folder_expanded",
                Theme::dark(),
                Setup::DetailsFolderExpanded("/home", &["/home/Documents"]),
            ),
            (
                "conflict_dialog",
                Theme::dark(),
                Setup::ConflictDialogOpen("/home"),
            ),
            (
                "details_rename_editing",
                Theme::dark(),
                Setup::RenameEditing("/home/Documents", "/home/Documents/report.pdf"),
            ),
            // The three M3 mouse-surface states ARCHITECTURE §8 asks for.
            (
                "cut_dimmed",
                Theme::dark(),
                Setup::CutSelection("/home", &["/home/archive.zip", "/home/readme.md"]),
            ),
            (
                "context_menu_open",
                Theme::dark(),
                Setup::ContextMenuOpen("/home"),
            ),
            (
                "marquee_active",
                Theme::dark(),
                Setup::MarqueeActive("/home"),
            ),
            // M4: the icon grid, with a selection in it. One frame rather
            // than two because the tint is drawn *by the tile* — a tile
            // regression and a selection regression would both show here.
            (
                "icon_grid",
                Theme::dark(),
                Setup::IconGrid("/home", &["/home/Documents", "/home/readme.md"]),
            ),
            // M4: the dual-pane layout of the plan §2 screenshot.
            (
                "split_panes",
                Theme::dark(),
                Setup::SplitPanes("/home", "/home/Documents"),
            ),
            // M5: the info panel describing a previewable file — the right
            // column of the plan §2 blueprint screenshot.
            (
                "info_panel_selection",
                Theme::dark(),
                Setup::InfoPanelSelection("/home/Documents", "/home/Documents/report.pdf"),
            ),
            // M5, §8's named row and the milestone's acceptance criterion:
            // "the panel matches the screenshot fields for a JPEG". Same
            // mechanics as the row above, deliberately a *different* subject —
            // an image, in its own folder, so the type description ("JPEG
            // image"), the extension row and the preview slot are pinned on
            // the file kind the blueprint actually shows.
            (
                "info_panel_jpeg",
                Theme::dark(),
                Setup::InfoPanelSelection("/home/Pictures", "/home/Pictures/photo.jpg"),
            ),
            // M5: the multi-selection summary. Nothing else pins it, and it
            // is the one info-panel state with no General/Permissions at all.
            (
                "info_panel_multi_selection",
                Theme::dark(),
                Setup::InfoPanelMultiSelection(
                    "/home",
                    &[
                        "/home/Documents",
                        "/home/archive.zip",
                        "/home/big-video.mov",
                        "/home/readme.md",
                    ],
                ),
            ),
            // M6a: the toolbar search's two states, one scenario each. Same
            // query in both, deliberately: the only difference between the two
            // baselines is what "Subfolders" does, so a regression in the
            // recursive half cannot hide behind a differently-shaped frame.
            //
            // Folder-local — the instant filter of the open folder. Pins the
            // focused field with text in it, the clear button, "Subfolders"
            // **unchecked**, a listing cut down to its matches (three folders
            // and a file, so the filter is visibly not folders-only), no
            // containing-folder labels at all, and the non-recursive status
            // line, which carries no folder count.
            (
                "search_filtered",
                Theme::dark(),
                Setup::SearchActive("/home", "o", false),
            ),
            // Recursive — the same query with the toggle lit, which is what
            // makes this the one frame that carries **both** kinds of result
            // row: the four local matches, unlabelled, and four deeper hits
            // (both `notes.txt`, `report.pdf`, `photo.jpg`) each carrying its
            // containing-folder qualifier, all in the pane's sort order rather
            // than the walk's arrival order. Plus the *finished*
            // "N folders searched" status line — not "scanning so far…", which
            // is what makes the frame a state instead of a race (see
            // `settle_search`).
            (
                "search_results",
                Theme::dark(),
                Setup::SearchActive("/home", "o", true),
            ),
            // M6b: the Permissions section doing what M5 only drew. The
            // subject is a file the fixture tags, so the panel's Tags row has
            // content in the same frame, and the octal editor is open — the
            // state that would otherwise exist only while a human holds the
            // mouse still.
            (
                "info_panel_permissions",
                Theme::dark(),
                Setup::InfoPanelPermissions("/home", "/home/readme.md"),
            ),
            // M6b: the sidebar's Tags section with a filter *on*. The dots
            // themselves are pinned by every /home scenario (the fixture seeds
            // tags), so what only this frame carries is the active tag row and
            // a listing cut down to that tag's items.
            (
                "tag_filter",
                Theme::dark(),
                Setup::TagFilter("/home", "Red"),
            ),
        ]
    }

    /// The fixture tree every scenario renders. Sizes are chosen so
    /// sort-by-size differs visibly from sort-by-name.
    fn install_fixture_vfs(cx: &mut VisualTestAppContext) {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/",
                json!({
                    "home": {
                        "Documents": {
                            "notes.txt": "0123456789",
                            "report.pdf": "This is a much longer fixture file body.",
                        },
                        "Downloads": {
                            // Collides with Documents/notes.txt: the
                            // conflict_dialog scenario copies it there.
                            "notes.txt": "a different set of notes",
                        },
                        "Desktop": {},
                        "Pictures": {},
                        "archive.zip": "zip",
                        "big-video.mov": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                        "readme.md": "hello world",
                        ".hidden-config": "secret",
                    },
                    "config": {
                        // Deterministic sidebar favorites for every scenario.
                        "settings.json": r#"{"favorites": ["/home/Documents", "/home/Downloads"]}"#,
                    }
                }),
            );
            // The info panel's JPEG subject, added *after* the tree rather
            // than inside it: `FakeVfs` hands out mtimes from a counter in
            // insertion order, so a new key in the fixture object would shift
            // every entry inserted after it, and this way no existing node
            // moves at all. The size is fixed here rather than derived from a
            // literal body, so "24 KB" in the panel is a number this file
            // states outright.
            vfs.insert_file("/home/Pictures/photo.jpg", 24_576);
            let vfs: Arc<dyn Vfs> = vfs;
            // M6b: two tagged entries, seeded rather than written — the dots
            // have to render for tags Finder (or a previous session) left
            // behind, and the sidebar's Tags section lists the palette
            // regardless. Deterministic like everything else in the fixture.
            let platform = fs_core::StubPlatform::new();
            platform.seed_tags("/home/readme.md", vec![tag("Red", TagColor::Red)]);
            platform.seed_tags(
                "/home/Documents",
                vec![tag("Red", TagColor::Red), tag("Work", TagColor::None)],
            );
            file_explorer_app::app_state::install(
                cx,
                vfs,
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(platform),
            );
            file_explorer_app::settings::init_with_path(
                cx,
                PathBuf::from("/config/settings.json"),
            );
        });
    }

    fn run(update_baseline: bool) -> Result<()> {
        let mut cx = VisualTestAppContext::new(gpui_platform::current_platform(false));
        cx.update(|cx| {
            keymap::init(cx);
        });

        let mut passed = 0u32;
        let mut updated = 0u32;
        let mut failures: Vec<String> = Vec::new();

        for (name, theme, setup) in scenarios() {
            println!("visual test: {name}");
            match run_scenario(&mut cx, name, theme, setup, update_baseline) {
                Ok(ScenarioResult::Passed) => {
                    println!("  PASS");
                    passed += 1;
                }
                Ok(ScenarioResult::BaselineUpdated) => {
                    println!("  baseline updated");
                    updated += 1;
                }
                Err(e) => {
                    eprintln!("  FAIL: {e:#}");
                    failures.push(name.to_string());
                }
            }
        }

        println!(
            "\n=== visual test summary: {passed} passed, {updated} updated, {} failed ===",
            failures.len()
        );
        if !failures.is_empty() {
            bail!("failed scenarios: {}", failures.join(", "));
        }
        Ok(())
    }

    enum ScenarioResult {
        Passed,
        BaselineUpdated,
    }

    /// Drive the workspace into the scenario's UI state. Navigation loads run
    /// on the (deterministic) background executor; callers `run_until_parked`
    /// after this returns.
    fn apply_setup(
        cx: &mut VisualTestAppContext,
        handle: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        setup: Setup,
    ) -> Result<()> {
        let navigate = |cx: &mut VisualTestAppContext, path: &str| {
            let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
            cx.update_window(handle, |_, _, cx| {
                pane.update(cx, |pane, cx| pane.navigate_to(Path::new(path), cx));
            })
            .map_err(|e| anyhow!("navigate failed: {e:?}"))?;
            // The listing has to be in before the info panel's debounce can
            // be waited out, and the wait has to happen before any scenario
            // starts a gesture with timers of its own.
            cx.run_until_parked();
            settle_info_panel(cx);
            Ok::<(), anyhow::Error>(())
        };

        match setup {
            Setup::None => {}
            Setup::Navigate(path) => {
                navigate(cx, path)?;
            }
            Setup::NavigateSorted(path, key) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
                cx.update_window(handle, |_, _, cx| {
                    pane.update(cx, |pane, cx| pane.sort_by(key, cx));
                })
                .map_err(|e| anyhow!("sort failed: {e:?}"))?;
            }
            Setup::AddressBarEditing(path) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
                cx.update_window(handle, |_, window, cx| {
                    pane.update(cx, |pane, cx| pane.focus_address_bar(window, cx));
                })
                .map_err(|e| anyhow!("focus_address_bar failed: {e:?}"))?;
            }
            Setup::SidebarTreeExpanded(path, expand) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let sidebar = cx.read(|cx| workspace.read(cx).sidebar().clone());
                for node in expand {
                    cx.update_window(handle, |_, _, cx| {
                        sidebar.update(cx, |sidebar, cx| {
                            sidebar.toggle_expanded(Path::new(node), cx);
                        });
                    })
                    .map_err(|e| anyhow!("tree expand failed: {e:?}"))?;
                    // Each expansion's children load before the next level.
                    cx.run_until_parked();
                }
            }
            Setup::DetailsFolderExpanded(path, expand) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
                let dir_view = cx.read(|cx| pane.read(cx).dir_view().clone());
                for node in expand {
                    cx.update_window(handle, |_, _, cx| {
                        dir_view.update(cx, |dir_view, cx| {
                            dir_view.toggle_expanded(Path::new(node), cx);
                        });
                    })
                    .map_err(|e| anyhow!("details expand failed: {e:?}"))?;
                    // Each expansion's children load before the next level.
                    cx.run_until_parked();
                }
            }
            Setup::SearchActive(path, query, recursive) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
                let bar = cx.read(|cx| pane.read(cx).search_bar().clone());
                cx.update_window(handle, |_, window, cx| {
                    // Driven through the field, so the captured frame shows
                    // the real focused control and its toggle rather than a
                    // filtered listing beside an empty-looking search box.
                    pane.update(cx, |pane, cx| pane.focus_search(window, cx));
                    bar.update(cx, |bar, cx| {
                        bar.set_text(query, window, cx);
                        bar.set_recursive(recursive, cx);
                    });
                })
                .map_err(|e| anyhow!("search failed: {e:?}"))?;
                settle_search(cx, &pane, recursive)?;
            }
            Setup::ConflictDialogOpen(path) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                // Copy Downloads/notes.txt onto Documents/notes.txt: the job
                // parks on the conflict and the workspace opens the modal.
                cx.update(|cx| {
                    FsContext::global(cx).queue.submit(FileOp::Copy {
                        sources: vec![PathBuf::from("/home/Downloads/notes.txt")],
                        dest_dir: PathBuf::from("/home/Documents"),
                    });
                });
            }
            Setup::RenameEditing(path, target) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
                let dir_view = cx.read(|cx| pane.read(cx).dir_view().clone());
                cx.update_window(handle, |_, window, cx| {
                    dir_view.update(cx, |dir_view, cx| {
                        let target = Path::new(target);
                        let entry = dir_view
                            .projected_rows(cx)
                            .into_iter()
                            .find(|row| row.entry.path.as_ref() == target)
                            .map(|row| row.entry)
                            .expect("rename target is listed in the fixture");
                        dir_view.set_cursor(Some(entry.id()), cx);
                        dir_view.begin_rename(&entry, window, cx);
                    });
                })
                .map_err(|e| anyhow!("rename setup failed: {e:?}"))?;
            }
            Setup::CutSelection(path, targets) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let dir_view = active_dir_view(cx, workspace);
                cx.update_window(handle, |_, _, cx| {
                    dir_view.update(cx, |dir_view, cx| {
                        let paths: Vec<&Path> = targets.iter().map(|p| Path::new(*p)).collect();
                        dir_view.select_paths(&paths, cx);
                        dir_view.cut_selection(cx);
                    });
                })
                .map_err(|e| anyhow!("cut setup failed: {e:?}"))?;
            }
            Setup::ContextMenuOpen(path) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                // Right-click the empty space below the last row: the
                // background menu, which is the richer of the two (a ✓, two
                // submenu arrows, and a disabled Paste).
                let at = below_last_row(cx, workspace, 30.0);
                cx.simulate_mouse_down(handle, at, MouseButton::Right, Modifiers::none());
                cx.simulate_mouse_up(handle, at, MouseButton::Right, Modifiers::none());
            }
            Setup::MarqueeActive(path) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                // Press in the empty space below the rows, then drag up over
                // them. The gesture is deliberately **not** released, so the
                // band, its border and the rows it has selected all paint.
                let viewport = list_geometry(cx, workspace).0;
                let from = below_last_row(cx, workspace, 40.0);
                let to = point(
                    viewport.left() + px(360.0),
                    viewport.top() + px(2.0 * DirView::ROW_HEIGHT + 10.0),
                );
                cx.simulate_mouse_down(handle, from, MouseButton::Left, Modifiers::none());
                // The first move trips gpui's 2px drag threshold and creates
                // the drag; the second is the one the marquee follows.
                cx.simulate_mouse_move(
                    handle,
                    from + point(px(6.0), px(-6.0)),
                    MouseButton::Left,
                    Modifiers::none(),
                );
                cx.simulate_mouse_move(handle, to, MouseButton::Left, Modifiers::none());
            }
            Setup::IconGrid(path, select) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
                let dir_view = cx.read(|cx| pane.read(cx).dir_view().clone());
                let selected: Vec<PathBuf> = select.iter().map(PathBuf::from).collect();
                cx.update_window(handle, |_, _, cx| {
                    pane.update(cx, |pane, cx| pane.set_view_mode(ViewMode::Icons, cx));
                    dir_view.update(cx, |dir_view, cx| {
                        let paths: Vec<&Path> = selected.iter().map(PathBuf::as_path).collect();
                        dir_view.select_paths(&paths, cx);
                    });
                })
                .map_err(|e| anyhow!("icon grid setup failed: {e:?}"))?;
            }
            Setup::InfoPanelSelection(path, target) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let dir_view = active_dir_view(cx, workspace);
                cx.update_window(handle, |_, _, cx| {
                    dir_view.update(cx, |dir_view, cx| {
                        dir_view.select_paths(&[Path::new(target)], cx);
                    });
                })
                .map_err(|e| anyhow!("info panel selection failed: {e:?}"))?;
                settle_info_panel(cx);
            }
            Setup::InfoPanelPermissions(path, target) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let dir_view = active_dir_view(cx, workspace);
                let panel = cx.read(|cx| workspace.read(cx).info_panel().clone());
                cx.update_window(handle, |_, _, cx| {
                    dir_view.update(cx, |dir_view, cx| {
                        dir_view.select_paths(&[Path::new(target)], cx);
                    });
                })
                .map_err(|e| anyhow!("info panel selection failed: {e:?}"))?;
                // The editor can only open on a value that has been read, so
                // the load has to land *first* — settling twice is not
                // belt-and-braces here, it is the ordering.
                settle_info_panel(cx);
                cx.update_window(handle, |_, window, cx| {
                    panel.update(cx, |panel, cx| {
                        // General is collapsed *first*: the panel is one
                        // scrolling column, and with General open the octal
                        // field, Owner and Group sit below the window edge at
                        // the fixed capture size — which is exactly how the
                        // first cut of this baseline came back showing an
                        // "open editor" scenario with no editor in it.
                        panel.set_general_open(false, cx);
                        panel.begin_field_edit(PermField::Octal, window, cx);
                    });
                })
                .map_err(|e| anyhow!("opening the octal editor failed: {e:?}"))?;
                settle_info_panel(cx);
            }
            Setup::TagFilter(path, tag_name) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
                let tag = tag(tag_name, TagColor::Red);
                cx.update_window(handle, |_, _, cx| {
                    pane.update(cx, |pane, cx| pane.set_tag_filter(tag, cx));
                })
                .map_err(|e| anyhow!("tag filter failed: {e:?}"))?;
                settle_tag_filter(cx, &pane)?;
            }
            Setup::InfoPanelMultiSelection(path, targets) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                let dir_view = active_dir_view(cx, workspace);
                cx.update_window(handle, |_, _, cx| {
                    dir_view.update(cx, |dir_view, cx| {
                        let paths: Vec<&Path> = targets.iter().map(|p| Path::new(*p)).collect();
                        dir_view.select_paths(&paths, cx);
                    });
                })
                .map_err(|e| anyhow!("info panel multi-selection failed: {e:?}"))?;
                // The summary is computed from the listing the panel already
                // holds, so there is no load to wait for — but settling anyway
                // keeps the captured frame free of any timer the selection
                // itself started.
                settle_info_panel(cx);
            }
            Setup::SplitPanes(path, second_path) => {
                navigate(cx, path)?;
                cx.run_until_parked();
                cx.update_window(handle, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| workspace.toggle_split_pane(window, cx));
                })
                .map_err(|e| anyhow!("split failed: {e:?}"))?;
                cx.run_until_parked();
                // The split focused the new pane, so this is that pane.
                let second = cx.read(|cx| workspace.read(cx).active_pane().clone());
                cx.update_window(handle, |_, _, cx| {
                    second.update(cx, |pane, cx| pane.navigate_to(Path::new(second_path), cx));
                })
                .map_err(|e| anyhow!("second pane navigate failed: {e:?}"))?;
            }
        }
        // Every gesture above can move the selection, replace the pane's
        // listing snapshot or change the expansion state, and each of those
        // retargets the info panel and arms a fresh `LOAD_DEBOUNCE`. Settling
        // once more here — after the gesture, not only after the navigation —
        // is what keeps a capture from pinning a half-loaded panel; before this
        // existed, `details_rename_editing` and `split_panes` baked em dashes
        // and a placeholder glyph into their baselines.
        //
        // Not for `MarqueeActive`: its drag is deliberately still held, and its
        // 30 ms `AUTOSCROLL_TICK` would walk the list under the band. Its
        // selection is a multi-selection, which the panel summarizes with no
        // load at all, so there is nothing to settle there anyway — asserted
        // for every scenario by `run_scenario`.
        if !matches!(setup, Setup::MarqueeActive(_)) {
            cx.run_until_parked();
            settle_info_panel(cx);
        }
        Ok(())
    }

    /// The info panel's attribute load is debounced
    /// (`info_panel::LOAD_DEBOUNCE`), so without advancing the deterministic
    /// clock past it every captured frame would pin the panel mid-load — em
    /// dashes where the size, dates and permissions belong. Deliberately
    /// shorter than the scrollbar's 900 ms fade and the drop target's 500 ms
    /// spring-load, so waiting for the panel cannot fire either.
    fn settle_info_panel(cx: &mut VisualTestAppContext) {
        cx.advance_clock(file_explorer_app::info_panel::LOAD_DEBOUNCE * 3);
        cx.run_until_parked();
    }

    /// How many throttle windows a search scenario waits for. The fixture tree
    /// is five directories deep-ish and the walk reads eight at a time, so a
    /// couple of rounds is the real cost; the rest is headroom, and running out
    /// of it is a failure rather than a capture.
    const SEARCH_SETTLE_ROUNDS: usize = 16;

    /// Wait out a search the way `settle_info_panel` waits out the panel.
    ///
    /// The recursive walk is polled on the background executor and its hits
    /// reach the pane in `search::SEARCH_THROTTLE` batches, so a frame captured
    /// mid-walk pins *whichever* prefix of the hits happened to have landed and
    /// a status line reading "N folders scanned so far…" — a baseline that is a
    /// race, not a state. Advance the deterministic clock a throttle window at
    /// a time until the pane says the walk is done, then assert the two things
    /// a plausible-looking-but-wrong capture would violate: that there is a
    /// search at all, and that it has rows. A search scenario whose results
    /// never arrived captures an "Empty folder" pane that reads as entirely
    /// fine in code review (CLAUDE.md's definition of done, item 7 — the first
    /// `search_results` capture was exactly that, a lit toggle over
    /// "0 results"), so fail loudly instead of baking one into a baseline.
    /// Wait out the tag filter's background scan, for the same reason
    /// [`settle_search`] waits out the recursive walk: capturing while it runs
    /// would pin a partial row list, and which partial list depends on thread
    /// timing.
    fn settle_tag_filter(cx: &mut VisualTestAppContext, pane: &Entity<Pane>) -> Result<()> {
        for _ in 0..SEARCH_SETTLE_ROUNDS {
            cx.run_until_parked();
            cx.advance_clock(file_explorer_app::search::SEARCH_THROTTLE * 2);
            cx.run_until_parked();
            if !cx.read(|cx| {
                pane.read(cx)
                    .tag_filter()
                    .is_some_and(|filter| filter.is_running())
            }) {
                let rows = cx.read(|cx| {
                    pane.read(cx)
                        .tag_filter()
                        .map(|filter| filter.rows().len())
                        .unwrap_or(0)
                });
                if rows == 0 {
                    bail!("the tag filter scenario produced no rows to capture");
                }
                return Ok(());
            }
        }
        bail!(
            "the tag filter was still scanning after {SEARCH_SETTLE_ROUNDS} throttle windows: capturing it would pin a partial row list"
        )
    }

    /// One tag, the way the fixture and the scenarios name them.
    fn tag(name: &str, color: TagColor) -> Tag {
        Tag {
            name: name.into(),
            color,
        }
    }

    fn settle_search(
        cx: &mut VisualTestAppContext,
        pane: &Entity<Pane>,
        recursive: bool,
    ) -> Result<()> {
        let mut finished = false;
        for _ in 0..SEARCH_SETTLE_ROUNDS {
            cx.run_until_parked();
            cx.advance_clock(file_explorer_app::search::SEARCH_THROTTLE * 2);
            cx.run_until_parked();
            // `running` is cleared by the same batch that folds in `Done`, so
            // once it is false every hit is already in `rows`.
            if !cx.read(|cx| {
                pane.read(cx)
                    .search()
                    .is_some_and(|search| search.is_running())
            }) {
                finished = true;
                break;
            }
        }
        if !finished {
            bail!(
                "the recursive search was still running after {SEARCH_SETTLE_ROUNDS} throttle windows: capturing it would pin a partial result list"
            );
        }
        let (rows, is_recursive, out_of_folder_hits) = cx.read(|cx| {
            let pane = pane.read(cx);
            let root = pane.path().map(std::path::Path::to_path_buf);
            pane.search()
                .map(|search| {
                    let rows = search.rows();
                    let out_of_folder = rows
                        .iter()
                        .filter(|entry| entry.path.parent() != root.as_deref())
                        .count();
                    (rows.len(), search.recursive(), out_of_folder)
                })
                .ok_or_else(|| anyhow!("the search scenario left the pane with no search at all"))
        })?;
        if rows == 0 {
            bail!("search scenario produced no result rows to capture");
        }
        // The scope, not just the row count. The field's text and its toggle
        // reach the pane one effect flush apart, and when that ordering broke
        // the capture was a *folder-local* result set under a lit
        // "☑ Subfolders" — which `is_running()` and a non-empty row list both
        // accept. Nothing else can catch it: the initial capture is the one
        // moment there is no baseline to compare against.
        if is_recursive != recursive {
            bail!(
                "search scenario asked for recursive={recursive} but the pane's search is \
                 recursive={is_recursive}: the captured frame would contradict its own checkbox"
            );
        }
        match (recursive, out_of_folder_hits) {
            (true, 0) => bail!(
                "a recursive search scenario captured no hit from outside the searched folder, \
                 so its baseline would be indistinguishable from the folder-local one"
            ),
            (false, n) if n > 0 => {
                bail!("a folder-local search scenario captured {n} row(s) from another folder")
            }
            _ => {}
        }
        Ok(())
    }

    fn active_dir_view(
        cx: &mut VisualTestAppContext,
        workspace: &Entity<Workspace>,
    ) -> Entity<DirView> {
        let pane = cx.read(|cx| workspace.read(cx).active_pane().clone());
        cx.read(|cx| pane.read(cx).dir_view().clone())
    }

    /// The details list's painted viewport and how many rows it is showing —
    /// the two numbers every pointer coordinate below is derived from, so a
    /// scenario never hard-codes the chrome's height.
    fn list_geometry(
        cx: &mut VisualTestAppContext,
        workspace: &Entity<Workspace>,
    ) -> (Bounds<Pixels>, usize) {
        let dir_view = active_dir_view(cx, workspace);
        cx.read(|cx| {
            let view = dir_view.read(cx);
            (view.list_viewport(), view.flat_rows().len())
        })
    }

    /// A point `offset` px below the last row, inside the list's empty space —
    /// where a marquee may start and where a right-click opens the background
    /// menu.
    fn below_last_row(
        cx: &mut VisualTestAppContext,
        workspace: &Entity<Workspace>,
        offset: f32,
    ) -> gpui::Point<Pixels> {
        let (viewport, rows) = list_geometry(cx, workspace);
        point(
            viewport.left() + px(80.0),
            viewport.top() + px(rows as f32 * DirView::ROW_HEIGHT + offset),
        )
    }

    fn run_scenario(
        cx: &mut VisualTestAppContext,
        name: &str,
        theme: Theme,
        setup: Setup,
        update_baseline: bool,
    ) -> Result<ScenarioResult> {
        // Fresh fixture + job spine per scenario. The queue and `JobsModel`
        // are globals, so a single install would let one scenario's state
        // bleed into every later one — `conflict_dialog` parks a job forever,
        // which would paint its titlebar "1 job" indicator into the baselines
        // of every scenario declared after it. Same reason the window is
        // closed below: scenarios must not depend on declaration order.
        install_fixture_vfs(cx);

        let mut workspace_slot: Option<Entity<Workspace>> = None;
        let window = cx
            .open_offscreen_window(size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(theme, window, cx));
                workspace_slot = Some(workspace.clone());
                workspace
            })
            .map_err(|e| anyhow!("failed to open off-screen window: {e:?}"))?;
        let handle: AnyWindowHandle = window.into();
        let workspace = workspace_slot.expect("window build ran");

        cx.run_until_parked();
        apply_setup(cx, handle, &workspace, setup)?;
        cx.run_until_parked();
        // A frame captured while the info panel is still loading shows em
        // dashes where the size, dates and permissions belong — a baseline that
        // reads as fine in a code review and is wrong in every pixel that
        // matters. `apply_setup` settles the panel; this is the guard that a
        // future scenario cannot quietly stop doing so.
        let settled = cx.read(|cx| workspace.read(cx).info_panel().read(cx).is_settled());
        if !settled {
            bail!(
                "the info panel is still loading: capturing it would bake a mid-load frame into the baseline"
            );
        }
        cx.update_window(handle, |_, window, _| window.refresh())
            .map_err(|e| anyhow!("failed to refresh window: {e:?}"))?;
        cx.run_until_parked();

        let screenshot = cx
            .capture_screenshot(handle)
            .map_err(|e| anyhow!("failed to capture screenshot: {e:?}"))?;

        // Close the window before comparing so a failure can't leak windows
        // into the next scenario.
        cx.update_window(handle, |_, window, _| window.remove_window())
            .ok();
        cx.run_until_parked();

        let output_dir = PathBuf::from(
            std::env::var("VISUAL_TEST_OUTPUT_DIR")
                .unwrap_or_else(|_| "target/visual_tests".to_string()),
        );
        std::fs::create_dir_all(&output_dir)?;
        let output_path = output_dir.join(format!("{name}.png"));
        screenshot
            .save(&output_path)
            .with_context(|| format!("saving screenshot to {}", output_path.display()))?;
        println!("  screenshot: {}", output_path.display());

        let baseline_path = baseline_path(name);
        if update_baseline {
            if let Some(parent) = baseline_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            screenshot.save(&baseline_path)?;
            println!("  baseline: {}", baseline_path.display());
            return Ok(ScenarioResult::BaselineUpdated);
        }

        if !baseline_path.exists() {
            bail!(
                "baseline not found: {}\n  Generate baselines on macOS with UPDATE_BASELINE=1, \
                 or trigger the 'Update visual baselines' GitHub workflow.",
                baseline_path.display()
            );
        }

        let baseline = image::open(&baseline_path)
            .with_context(|| format!("opening baseline {}", baseline_path.display()))?
            .to_rgba8();
        let cmp = visual_diff::compare(&screenshot, &baseline, PIXEL_TOLERANCE);
        println!(
            "  match: {:.3}% ({} differing pixels of {})",
            cmp.match_fraction * 100.0,
            cmp.diff_pixel_count,
            cmp.total_pixels
        );

        if cmp.match_fraction >= MATCH_THRESHOLD {
            Ok(ScenarioResult::Passed)
        } else {
            let diff_path = output_dir.join(format!("{name}_diff.png"));
            cmp.diff_image.save(&diff_path)?;
            bail!(
                "image mismatch: {:.3}% match (threshold {:.0}%). Diff: {}",
                cmp.match_fraction * 100.0,
                MATCH_THRESHOLD * 100.0,
                diff_path.display()
            );
        }
    }

    /// Baselines live next to this crate so they version with the UI code.
    fn baseline_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test_fixtures/visual_tests")
            .join(format!("{name}.png"))
    }
}
