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
    use file_explorer_app::{Theme, Workspace, keymap, visual_diff};
    use fs_core::{FakeVfs, FileOp, SortKey, Spawner, Vfs};
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
            let vfs: Arc<dyn Vfs> = vfs;
            file_explorer_app::app_state::install(
                cx,
                vfs,
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
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
            .map_err(|e| anyhow!("navigate failed: {e:?}"))
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
