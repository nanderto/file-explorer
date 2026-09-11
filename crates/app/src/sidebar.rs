//! The sidebar (ARCHITECTURE.md §2 `Sidebar` entity, §8 "Sidebar tree",
//! plan §2 sidebar blueprint) — M2 surface, rebuilt at M7d-b.
//!
//! **Two levels, always.** A section header, and its rows. Nothing in the
//! sidebar expands: depth is navigated in the file pane(s), which is how both
//! Finder and ForkLift behave and what M7c's Explorer-style folder *tree*
//! got wrong. That tree could open to arbitrary depth (`Macintosh HD › dev ›
//! fd › 3 › …`), which made the sidebar a second, worse file browser beside
//! the real one — and made it the only unbounded thing in the column.
//!
//! Five sections, in this order:
//!
//! - **Devices**: mounted volumes from the [`fs_core::Platform`] seam (name,
//!   free space, an eject affordance on ejectable volumes), kept current by
//!   the polling [`watch_volumes`] stream (fake time under `#[gpui::test]`).
//! - **Locations** (M7d-b): what [`fs_core::resolve_locations`] finds on this
//!   machine — iCloud Drive, every OneDrive tenant root, home, `/Network`,
//!   Trash. Resolved once on the background executor; only what exists is
//!   listed, so no row navigates to nothing.
//! - **Favorites**: user-pinned folders from [`AppSettings`] — click
//!   navigates, `+` in the header pins the active pane's folder, `✕` on a row
//!   unpins, rows reorder by drag; every change persists immediately through
//!   `Vfs::atomic_write` on the background executor. Seeded once with
//!   Desktop/Documents/Downloads (M7d-b), guarded by a flag rather than by
//!   emptiness so unpinning them all stays unpinned.
//! - **Recents** (M7d-b): the folders the panes have opened, most recent
//!   first, from [`AppSettings::recents`].
//! - **Tags** (M6b): the Finder tags [`fs_core::Platform::known_tags`]
//!   reports, each with its fixed macOS palette dot. Clicking one filters the
//!   active pane to the items in the open folder carrying it; clicking the lit
//!   one again clears the filter. **Deliberate deviation:** Finder's tag click
//!   is a volume-wide Spotlight query — see [`crate::tags`] and
//!   `docs/AS_BUILT.md`.
//!
//! **Collapsing reflows.** Every section is an ordinary block in a single
//! scrolling column — no `flex_1`, no pinned footer. Collapsing one moves
//! everything below it *up*, which is what Finder and ForkLift do and what
//! M7c could not do while the tree ate the leftover height and pinned Tags to
//! the bottom edge.
//!
//! Events up, method calls down (§2): the sidebar only emits
//! [`SidebarEvent`]s; the owning [`Workspace`] navigates the active pane and
//! runs `Platform::eject` on the background executor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs_core::{
    Location, LocationKind, Tag, VolumeId, VolumeInfo, WatchGuard, resolve_locations, watch_volumes,
};
use futures::StreamExt as _;
use gpui::{
    Context, EventEmitter, ExternalPaths, IntoElement, Render, SharedString, Subscription, Task,
    WeakEntity, Window, div, prelude::*, px,
};

use crate::app_state::FsContext;
use crate::drag::{self, DraggedEntries, DraggedFavorite};
use crate::icons::{self, Icon};
use crate::pane::format_bytes;
use crate::settings::AppSettings;
use crate::workspace::Workspace;
use ::theme::Theme;

/// How often the volume list is re-polled (ARCHITECTURE.md §6: change
/// detection is a poller on `Spawner::timer`, so tests advance a fake clock).
pub const VOLUME_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The folders a fresh profile gets pinned, in display order (M7d-b).
///
/// Relative to home and filtered to what exists, so a machine without one of
/// them simply does not get that row. **Applications is deliberately absent**:
/// Finder's Applications favorite points at `/Applications`, which is not
/// under home at all, and this Mac also has a separate `~/Applications` — two
/// different folders with one name is a row that means the wrong thing half
/// the time.
const DEFAULT_FAVORITES: [&str; 3] = ["Desktop", "Documents", "Downloads"];

/// The height a sidebar row occupies. Also the unit the Favorites drop zone's
/// minimum height is expressed in.
const ROW_HEIGHT: f32 = 22.0;
/// Favorites reorder drop-target tint (the "insert before this row" cue).
const FAVORITE_REORDER_ALPHA: f32 = 0.35;

/// Events up (ARCHITECTURE.md §2): the workspace subscribes and acts.
pub enum SidebarEvent {
    /// Navigate the active pane to this folder (volume, favorite, tree row).
    NavigateTo(PathBuf),
    /// Eject this volume (workspace runs `Platform::eject` off the UI thread).
    Eject(VolumeId),
    /// Filter the active pane by a tag, or — with `None` — stop filtering
    /// (M6b). Events up: the sidebar never touches a pane itself (§2).
    FilterByTag(Option<Tag>),
}

/// The sidebar's collapsible sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Devices,
    Locations,
    Favorites,
    Recents,
    Tags,
}

pub struct Sidebar {
    workspace: WeakEntity<Workspace>,
    volumes: Vec<VolumeInfo>,
    collapsed_devices: bool,
    collapsed_locations: bool,
    collapsed_favorites: bool,
    collapsed_recents: bool,
    collapsed_tags: bool,
    /// What this machine actually has (M7d-b). Empty until the one-shot
    /// resolve lands, so the first paint does no I/O and the section simply
    /// is not there yet rather than showing rows that might not exist.
    locations: Vec<Location>,
    /// The one-shot locations resolve **and** favorites seeding; a field,
    /// never detached (§5).
    _startup: Option<Task<()>>,
    /// Which of [`DEFAULT_FAVORITES`] this machine actually has, once the
    /// startup probe has answered. `None` until then — which is *not* the same
    /// as "none of them exist", and seeding must not confuse the two.
    default_favorites: Option<Vec<PathBuf>>,
    /// The tags the **Tags** section lists: the palette plus whatever the user
    /// has, loaded once off the UI thread. Seeded with
    /// [`fs_core::standard_tags`] so the section is never empty and the first
    /// paint does no I/O.
    tags: Vec<Tag>,
    /// The one-shot `known_tags` load; a field, never detached (§5).
    _tags_load: Option<Task<()>>,
    /// Paths dropped on Favorites that still need their "is this a folder?"
    /// probe. A **queue**, because a second drop must not cancel the first:
    /// while it is non-empty the task below is alive and will drain it.
    pending_favorite_drops: Vec<PathBuf>,
    /// The in-flight probe behind Favorites drag-to-add; a field, never
    /// detached (§5).
    _favorite_drop: Option<Task<()>>,
    /// The volume-watch pump; held so it dies with the view (§5).
    _volumes_pump: Task<()>,
    /// Dropping this stops the volume poller.
    _volumes_guard: WatchGuard,
    /// Repaint when [`AppSettings`] changes — render reads the global for the
    /// Favorites rows, and the boot-time background load swaps it in *after*
    /// the first paint (`settings::init`), so without this observer persisted
    /// favorites could stay invisible until an unrelated repaint.
    _settings_observer: Subscription,
}

impl Sidebar {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let fs = FsContext::global(cx);
        let platform = fs.platform.clone();
        let startup_vfs = fs.vfs.clone();
        let startup_home = fs.home.clone();
        let (mut stream, guard) =
            watch_volumes(fs.platform.clone(), &fs.spawner, VOLUME_POLL_INTERVAL);
        let pump = cx.spawn(async move |this, cx| {
            while let Some(volumes) = stream.next().await {
                let alive = this.update(cx, |this, cx| this.set_volumes(volumes, cx));
                if alive.is_err() {
                    return; // sidebar dropped
                }
            }
        });
        let settings_observer = cx.observe_global::<AppSettings>(|this: &mut Self, cx| {
            // The settings load landing is one of the two things seeding waits
            // for (the other is the startup probe above).
            this.maybe_seed_favorites(cx);
            cx.notify();
        });
        // M6b: the Tags section's rows. One `known_tags` call, on the
        // background executor — the sidebar never touches the OS on the UI
        // thread (§5), and the palette is already painted while it runs.
        let tags_load = cx.spawn(async move |this, cx| {
            let known = cx
                .background_executor()
                .spawn(async move { platform.known_tags().await })
                .await;
            if let Ok(known) = known
                && !known.is_empty()
            {
                this.update(cx, |this, cx| {
                    this.tags = known;
                    cx.notify();
                })
                .ok();
            }
        });
        // M7d-b: Locations and the seeded Favorites, in one background pass —
        // both want the same home directory and the same `Vfs`, and both are
        // startup-only. Captured before the spawn (the `tags_load` shape just
        // above), because the globals are not reachable from the async
        // context. Off the UI thread, like every other disk read (§5).
        let startup = cx.spawn(async move |this, cx| {
            let seed_vfs = startup_vfs.clone();
            let seed_home = startup_home.clone();
            let (locations, present_defaults) = cx
                .background_executor()
                .spawn(async move {
                    let locations = resolve_locations(seed_vfs.as_ref(), &seed_home).await;
                    let mut present: Vec<PathBuf> = Vec::new();
                    for name in DEFAULT_FAVORITES {
                        let path = seed_home.join(name);
                        if matches!(seed_vfs.metadata(&path).await, Ok(Some(_))) {
                            present.push(path);
                        }
                    }
                    (locations, present)
                })
                .await;
            this.update(cx, |this, cx| {
                this.locations = locations;
                this.default_favorites = Some(present_defaults);
                // Whichever finishes last — this probe or the settings load —
                // does the seeding. See `maybe_seed_favorites`.
                this.maybe_seed_favorites(cx);
                cx.notify();
            })
            .ok();
        });
        Self {
            workspace,
            volumes: Vec::new(),
            collapsed_devices: false,
            collapsed_locations: false,
            collapsed_favorites: false,
            collapsed_recents: false,
            collapsed_tags: false,
            locations: Vec::new(),
            _startup: Some(startup),
            default_favorites: None,
            tags: fs_core::standard_tags(),
            _tags_load: Some(tags_load),
            pending_favorite_drops: Vec::new(),
            _favorite_drop: None,
            _volumes_pump: pump,
            _volumes_guard: guard,
            _settings_observer: settings_observer,
        }
    }

    // ------------------------------------------------------------------
    // Devices
    // ------------------------------------------------------------------

    pub fn volumes(&self) -> &[VolumeInfo] {
        &self.volumes
    }

    fn set_volumes(&mut self, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
        self.volumes = volumes;
        cx.notify();
    }

    /// Ask the workspace to eject (the sidebar itself never touches the OS).
    pub fn request_eject(&mut self, volume_id: VolumeId, cx: &mut Context<Self>) {
        cx.emit(SidebarEvent::Eject(volume_id));
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    /// Emit a navigation request for any clicked row (volume, favorite, tree).
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        cx.emit(SidebarEvent::NavigateTo(path));
    }

    // ------------------------------------------------------------------
    // Favorites (persisted immediately via AppSettings)
    // ------------------------------------------------------------------

    /// Pin the active pane's folder (the `+` affordance; M3 adds context
    /// menus and drag-to-add). Persists immediately.
    pub fn add_current_folder(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(path) = workspace
            .read(cx)
            .active_pane()
            .read(cx)
            .path()
            .map(Path::to_path_buf)
        else {
            return;
        };
        let changed =
            cx.update_global::<AppSettings, bool>(|settings, _| settings.add_favorite(path));
        if changed {
            AppSettings::global(cx).save(cx);
            cx.notify();
        }
    }

    /// Drag-to-add (§8 drag & drop; the gap M2 deferred): folders dragged from
    /// a pane — or in from Finder — become favorites. **Only folders**: each
    /// dropped path is stat'ed on the background executor first (the UI thread
    /// never touches the disk), then the survivors are appended and persisted
    /// in one write.
    pub fn add_favorites_from_drop(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        // A single `Option<Task>` slot would make the *second* drop cancel the
        // first mid-probe — a folder dropped from a slow mount would silently
        // never get pinned. So drops queue, and only one task drains them.
        // A non-empty queue is exactly "a task is alive": it is drained in the
        // same update that applies the results, with no await in between.
        let was_idle = self.pending_favorite_drops.is_empty();
        self.pending_favorite_drops.extend(paths);
        if !was_idle {
            return;
        }
        let vfs = FsContext::global(cx).vfs.clone();
        // Held in a field (§5), so dropping the sidebar cancels the probe.
        self._favorite_drop = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(batch) = this.read_with(cx, |this, _| this.pending_favorite_drops.clone())
                else {
                    return; // sidebar dropped
                };
                if batch.is_empty() {
                    return;
                }
                let mut folders = Vec::new();
                for path in &batch {
                    let vfs = vfs.clone();
                    let probe = path.clone();
                    let meta = cx
                        .background_spawn(async move { vfs.metadata(&probe).await })
                        .await;
                    if let Ok(Some(meta)) = meta
                        && meta.kind.is_dir_like()
                    {
                        folders.push(path.clone());
                    }
                }
                let applied = this.update(cx, |this, cx| {
                    this.pending_favorite_drops.drain(..batch.len());
                    let changed = cx.update_global::<AppSettings, bool>(|settings, _| {
                        // Every folder is added (no short-circuit: a duplicate
                        // in the middle of the drag must not drop the rest).
                        let mut changed = false;
                        for path in folders {
                            changed |= settings.add_favorite(path);
                        }
                        changed
                    });
                    if changed {
                        AppSettings::global(cx).save(cx);
                        cx.notify();
                    }
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    /// Drag-to-reorder (§8; the other half of the M2-deferred gap): move a
    /// favorite immediately before `before`, or to the end when a favorite is
    /// dropped on the section rather than on a row. Persists immediately.
    pub fn reorder_favorite(&mut self, path: &Path, before: Option<&Path>, cx: &mut Context<Self>) {
        let changed = cx
            .update_global::<AppSettings, bool>(|settings, _| settings.move_favorite(path, before));
        if changed {
            AppSettings::global(cx).save(cx);
            cx.notify();
        }
    }

    /// Unpin a favorite (the per-row `✕` button). Persists immediately.
    pub fn remove_favorite(&mut self, path: &Path, cx: &mut Context<Self>) {
        let changed =
            cx.update_global::<AppSettings, bool>(|settings, _| settings.remove_favorite(path));
        if changed {
            AppSettings::global(cx).save(cx);
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // Sections
    // ------------------------------------------------------------------

    pub fn section_collapsed(&self, section: Section) -> bool {
        match section {
            Section::Devices => self.collapsed_devices,
            Section::Favorites => self.collapsed_favorites,
            Section::Tags => self.collapsed_tags,
            Section::Locations => self.collapsed_locations,
            Section::Recents => self.collapsed_recents,
        }
    }

    pub fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let flag = match section {
            Section::Devices => &mut self.collapsed_devices,
            Section::Favorites => &mut self.collapsed_favorites,
            Section::Tags => &mut self.collapsed_tags,
            Section::Locations => &mut self.collapsed_locations,
            Section::Recents => &mut self.collapsed_recents,
        };
        *flag = !*flag;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Rendering (every color from the Theme)
    // ------------------------------------------------------------------

    fn section_header(
        &self,
        section: Section,
        title: &'static str,
        with_add: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = crate::theme::theme(cx).clone();
        let collapsed = self.section_collapsed(section);
        let mut header = div()
            .id(title)
            .debug_selector(|| format!("sidebar-section-{title}"))
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(12.0))
            .pt(px(12.0))
            .pb(px(2.0))
            .text_size(px(11.0))
            .text_color(theme.muted)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(section, cx)))
            // Label first, chevron last: the disclosure sits at the **far
            // right** of the header (Finder and ForkLift both put it there),
            // so the section titles all start on one left margin instead of
            // being pushed in by a control.
            .child(div().flex_1().child(SharedString::new_static(title)));
        if with_add {
            header = header.child(
                div()
                    .id("sidebar-favorites-add")
                    .debug_selector(|| "sidebar-favorites-add".into())
                    .px(px(4.0))
                    .rounded(px(3.0))
                    .hover(|s| s.bg(theme.accent.opacity(0.15)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.add_current_folder(cx);
                    }))
                    .flex()
                    .items_center()
                    .child(icons::icon(Icon::Plus, theme.muted)),
            );
        }
        header.child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .child(icons::icon(icons::disclosure(!collapsed), theme.muted)),
        )
    }

    fn render_devices(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = crate::theme::theme(cx).clone();
        let rows: Vec<_> = self
            .volumes
            .iter()
            .enumerate()
            .map(|(ix, volume)| {
                let navigate_path = volume.path.clone();
                let eject_id = volume.volume_id.clone();
                let volume_name = volume.name.clone();
                let mut row = sidebar_row(("sidebar-volume", ix), &theme)
                    .debug_selector(move || format!("sidebar-volume-{volume_name}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_path(navigate_path.clone(), cx);
                    }))
                    .child(icons::icon(Icon::Drive, theme.muted))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .child(SharedString::from(volume.name.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.muted)
                            .child(SharedString::from(format_bytes(volume.free))),
                    );
                if volume.ejectable {
                    let eject_name = volume.name.clone();
                    row = row.child(
                        div()
                            .id(("sidebar-eject", ix))
                            .debug_selector(|| format!("sidebar-eject-{eject_name}"))
                            .px(px(4.0))
                            .rounded(px(3.0))
                            .text_color(theme.muted)
                            .hover(|s| s.bg(theme.accent.opacity(0.25)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.request_eject(eject_id.clone(), cx);
                            }))
                            .flex()
                            .items_center()
                            .child(icons::sized_icon(
                                Icon::Eject,
                                px(icons::SMALL_ICON_PX),
                                theme.muted,
                            )),
                    );
                }
                row
            })
            .collect();
        div().flex().flex_col().children(rows)
    }

    /// The whole Favorites section (header + rows) as one drop zone (§8): a
    /// folder dragged in from a pane or from Finder is pinned, and a favorite
    /// dropped on the section — rather than on a row — moves to the end.
    ///
    /// A `div`'s hitbox is its content, so the zone gets a **minimum height**:
    /// with nothing pinned (the default on first run) or the section collapsed
    /// the content is the 32px header alone, and a drop one pixel below it —
    /// inside the sidebar, in what reads as the Favorites area — would land on
    /// nothing at all. Every payload the section accepts also tints on hover,
    /// so the boundary is visible rather than guessed at.
    fn favorites_section(
        &self,
        favorites: &[PathBuf],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = crate::theme::theme(cx).clone();
        let external_theme = theme.clone();
        let favorite_theme = theme.clone();
        let mut section = div()
            .id("sidebar-favorites-drop-zone")
            .debug_selector(|| "sidebar-favorites-drop-zone".into())
            .flex()
            .flex_col()
            .min_h(px(ROW_HEIGHT * 3.0))
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style.bg(external_theme.accent.opacity(drag::FAVORITES_DROP_ALPHA))
            })
            .drag_over::<DraggedFavorite>(move |style, _, _, _| {
                style.bg(favorite_theme.accent.opacity(drag::FAVORITES_DROP_ALPHA))
            })
            .on_drop(cx.listener(|this, dragged: &DraggedEntries, _, cx| {
                let paths = dragged
                    .paths()
                    .iter()
                    .map(|path| path.to_path_buf())
                    .collect();
                this.add_favorites_from_drop(paths, cx);
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_favorites_from_drop(paths.paths().to_vec(), cx);
            }))
            .on_drop(cx.listener(|this, dragged: &DraggedFavorite, _, cx| {
                this.reorder_favorite(&dragged.path, None, cx);
            }))
            .drag_over::<DraggedEntries>(move |style, _, _, _| {
                style.bg(theme.accent.opacity(drag::FAVORITES_DROP_ALPHA))
            })
            .child(self.section_header(Section::Favorites, "Favorites", true, cx));
        if !self.collapsed_favorites {
            section = section.child(self.render_favorites(favorites, cx));
        }
        section
    }

    fn render_favorites(
        &self,
        favorites: &[PathBuf],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = crate::theme::theme(cx).clone();
        let rows: Vec<_> = favorites
            .iter()
            .enumerate()
            .map(|(ix, path)| {
                let name = display_name(path);
                let navigate_path = path.clone();
                let remove_path = path.clone();
                let drag_path = path.clone();
                let insert_before = path.clone();
                let ghost_label = SharedString::from(name.clone());
                // Path-keyed, not index-keyed: this row is a drag source, and
                // gpui persists a stateful element's pending press by element
                // id across frames — an index would let a press on one
                // favorite start a drag carrying whichever favorite the list
                // has since shuffled into that slot (invariant #2).
                sidebar_row(gpui::ElementId::Path(Arc::from(path.as_path())), &theme)
                    .debug_selector(move || format!("sidebar-favorite-{ix}"))
                    // §8 reordering: a row is both a drag source and the
                    // "insert before me" target. Highlighted as a background
                    // tint rather than an insertion rule, so arming a target
                    // never nudges the rows below it.
                    .on_drag(DraggedFavorite { path: drag_path }, move |_, _, _, cx| {
                        drag::ghost(ghost_label.clone(), cx)
                    })
                    .drag_over::<DraggedFavorite>({
                        let theme = theme.clone();
                        move |style, _, _, _| style.bg(theme.accent.opacity(FAVORITE_REORDER_ALPHA))
                    })
                    .on_drop(cx.listener(move |this, dragged: &DraggedFavorite, _, cx| {
                        this.reorder_favorite(&dragged.path, Some(&insert_before), cx);
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_path(navigate_path.clone(), cx);
                    }))
                    .child(icons::icon(Icon::Folder, theme.muted))
                    .child(div().flex_1().truncate().child(SharedString::from(name)))
                    .child(
                        div()
                            .id(gpui::ElementId::NamedChild(
                                Arc::new(gpui::ElementId::Path(Arc::from(remove_path.as_path()))),
                                SharedString::new_static("unpin"),
                            ))
                            .debug_selector(|| format!("sidebar-favorite-remove-{ix}"))
                            .px(px(4.0))
                            .rounded(px(3.0))
                            .flex()
                            .items_center()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_favorite(&remove_path, cx);
                            }))
                            // The hover recolour lives on the `svg` and not on
                            // this wrapper: gpui tints an icon from the
                            // element's *own* `text_color`, so a parent's
                            // hovered colour never reaches it (`icons`).
                            .child(
                                icons::sized_icon(
                                    Icon::Close,
                                    px(icons::SMALL_ICON_PX),
                                    theme.muted,
                                )
                                .hover(|s| s.text_color(theme.error)),
                            ),
                    )
            })
            .collect();
        div().flex().flex_col().children(rows)
    }

    // ------------------------------------------------------------------
    // Locations (M7d-b) and Recents (M7d-b)
    // ------------------------------------------------------------------

    /// Seed the default Favorites, **but only once the settings load has
    /// landed** (M7d-b).
    ///
    /// This gate is not a nicety. `settings::init_with_path` seeds the global
    /// with defaults, loads the file in the background, and then *discards*
    /// that load if the content changed in the meantime — so a write during
    /// that window does not merely race, it throws the user's whole file away
    /// and persists defaults over it. Seeding without this gate destroyed a
    /// real profile's saved theme the first time it ran outside a fixture.
    ///
    /// Called from both things it waits on — the startup probe and the
    /// settings observer — so whichever resolves last performs the seed.
    /// `AppSettings::seed_favorites` is idempotent, so being called twice is
    /// harmless.
    fn maybe_seed_favorites(&mut self, cx: &mut Context<Self>) {
        let Some(candidates) = self.default_favorites.clone() else {
            return; // the probe has not answered yet
        };
        let settings = AppSettings::global(cx);
        if !settings.is_loaded() {
            return; // the file has not landed yet
        }
        // Read before write. This runs from `observe_global::<AppSettings>`,
        // and `update_global` notifies that observer — so calling it
        // unconditionally re-enters here forever, even though
        // `seed_favorites` itself is idempotent. The cheap read is what makes
        // the recursion terminate.
        if settings.favorites_seeded() {
            return;
        }
        // **Seeded in memory, never written here** (M7d-c). The seed is
        // deterministic — the same defaults, filtered by the same existence
        // check — so re-deriving it on each launch produces exactly the same
        // rows. Persisting it would be a disk write at startup for a result
        // the app can recompute for free.
        //
        // It reaches disk the first time the user changes anything, because
        // `save` writes the whole document. That is also what keeps "emptying
        // them stays empty" working: unpinning is a user change, so the empty
        // list and the seeded flag persist together, and the next launch sees
        // the flag and does not re-seed.
        cx.update_global::<AppSettings, bool>(|settings, _| settings.seed_favorites(&candidates));
        cx.notify();
    }

    /// What this machine has, as last resolved.
    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    /// The **Locations** rows. Icon keyed off [`LocationKind`] rather than off
    /// the name, which is localized ("iCloud Drive"), tenant-suffixed
    /// ("OneDrive - BidOne Ltd") or the user's own login name.
    fn render_locations(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = crate::theme::theme(cx).clone();
        let rows: Vec<_> = self
            .locations
            .iter()
            .enumerate()
            .map(|(ix, location)| {
                let navigate_path = location.path.clone();
                let name = SharedString::from(location.name.clone());
                sidebar_row(("sidebar-location", ix), &theme)
                    .debug_selector(move || format!("sidebar-location-{ix}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_path(navigate_path.clone(), cx);
                    }))
                    .child(icons::icon(location_icon(location.kind), theme.muted))
                    .child(div().flex_1().truncate().child(name))
            })
            .collect();
        div().flex().flex_col().children(rows)
    }

    /// The **Recents** rows: folders the panes have opened, most recent first.
    ///
    /// Read straight from [`AppSettings`] rather than mirrored on the sidebar,
    /// for the same reason the Favorites rows are: the list is written by the
    /// workspace on every navigation, and a mirror would be one repaint behind
    /// the folder the user is standing in.
    fn render_recents(
        &self,
        recents: &[PathBuf],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = crate::theme::theme(cx).clone();
        let rows: Vec<_> = recents
            .iter()
            .enumerate()
            .map(|(ix, path)| {
                let navigate_path = path.clone();
                let name = SharedString::from(display_name(path));
                sidebar_row(("sidebar-recent", ix), &theme)
                    .debug_selector(move || format!("sidebar-recent-{ix}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_path(navigate_path.clone(), cx);
                    }))
                    .child(icons::icon(Icon::Folder, theme.muted))
                    .child(div().flex_1().truncate().child(name))
            })
            .collect();
        div().flex().flex_col().children(rows)
    }

    /// The tags the section lists (the palette plus the user's).
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }

    /// The tag the **active pane** is currently filtered by, read straight off
    /// that pane rather than mirrored here: the pane also drops the filter on
    /// its own (navigating away), and a mirror would keep a row lit for a
    /// filter that is gone.
    pub fn active_tag(&self, cx: &gpui::App) -> Option<Tag> {
        let workspace = self.workspace.upgrade()?;
        let pane = workspace.read(cx).active_pane().clone();
        pane.read(cx)
            .tag_filter()
            .map(|filter| filter.tag().clone())
    }

    /// Click a tag row: filter the active pane by it, or clear the filter when
    /// the lit row is clicked again (Finder's toggle).
    pub fn toggle_tag_filter(&mut self, tag: &Tag, cx: &mut Context<Self>) {
        let active = self.active_tag(cx);
        let next = if active.as_ref() == Some(tag) {
            None
        } else {
            Some(tag.clone())
        };
        cx.emit(SidebarEvent::FilterByTag(next));
        cx.notify();
    }

    /// The **Tags** section's rows: the palette dot, the name, and the active
    /// row tinted like a selected favorite. Every colour but the dot comes from
    /// the theme — the dot is macOS's (see [`crate::tags`]).
    fn render_tags(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = crate::theme::theme(cx).clone();
        let active = self.active_tag(cx);
        let rows: Vec<_> = self
            .tags
            .iter()
            .enumerate()
            .map(|(ix, tag)| {
                let is_active = active.as_ref() == Some(tag);
                let clicked = tag.clone();
                let name = SharedString::new(&tag.name);
                let mut row = sidebar_row(("sidebar-tag", ix), &theme)
                    .debug_selector(move || format!("sidebar-tag-{ix}"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_tag_filter(&clicked, cx);
                    }))
                    .children(crate::tags::tag_dot(tag.color).or_else(|| {
                        // An uncoloured tag still needs the dot's width, or its
                        // name would not line up under the coloured ones.
                        Some(
                            div()
                                .flex_none()
                                .w(px(crate::tags::TAG_DOT_PX))
                                .h(px(crate::tags::TAG_DOT_PX))
                                .rounded(px(crate::tags::TAG_DOT_PX / 2.0))
                                .border_1()
                                .border_color(theme.border)
                                .into_any_element(),
                        )
                    }))
                    .child(div().flex_1().truncate().child(name));
                if is_active {
                    row = row.bg(theme.accent.opacity(FAVORITE_REORDER_ALPHA));
                }
                row
            })
            .collect();
        div().flex().flex_col().children(rows)
    }
}

/// The shared shape of every clickable sidebar row: one line, an icon gutter,
/// a truncating label. Factored out because five sections draw it and they
/// drifted apart before M7d-b (different paddings, different hover tints).
fn sidebar_row(id: impl Into<gpui::ElementId>, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    let hover = theme.accent.opacity(0.15);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(ROW_HEIGHT))
        .px(px(16.0))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
}

/// The icon for a location, by kind.
fn location_icon(kind: LocationKind) -> Icon {
    match kind {
        // Both cloud roots get the same glyph: they are the same *idea*, and
        // the row is labelled with which one it is.
        LocationKind::ICloudDrive | LocationKind::OneDrive => Icon::Cloud,
        LocationKind::Home => Icon::Home,
        LocationKind::Network => Icon::Network,
        LocationKind::Trash => Icon::Trash,
    }
}

/// A path's last component, or the path itself when it has none (`/`). Shared
/// by Favorites and Recents so a row is never labelled with the empty string.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::theme(cx).clone();
        let settings = AppSettings::global(cx);
        let favorites: Vec<PathBuf> = settings.favorites().to_vec();
        let recents: Vec<PathBuf> = settings.recents().to_vec();

        // **One scrolling column, every section an ordinary block.** No
        // `flex_1` anywhere: that is what pinned Tags to the bottom edge
        // through M7c and left a gap above it. With every section sized by its
        // own content, collapsing one moves everything below it up — the
        // Finder/ForkLift behavior — and the column as a whole scrolls when
        // the sections together outgrow it.
        let mut root = div()
            .id("sidebar-scroll")
            .debug_selector(|| "sidebar-scroll".into())
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .text_size(px(13.0))
            .text_color(theme.text)
            .child(self.section_header(Section::Devices, "Devices", false, cx));
        if !self.collapsed_devices {
            root = root.child(self.render_devices(cx));
        }

        // Locations is absent, not empty, until the resolve lands — and on a
        // machine with none of them it stays absent rather than showing a
        // header over nothing.
        if !self.locations.is_empty() {
            root = root.child(self.section_header(Section::Locations, "Locations", false, cx));
            if !self.collapsed_locations {
                root = root.child(self.render_locations(cx));
            }
        }

        root = root.child(self.favorites_section(&favorites, cx));

        if !recents.is_empty() {
            root = root.child(self.section_header(Section::Recents, "Recents", false, cx));
            if !self.collapsed_recents {
                root = root.child(self.render_recents(&recents, cx));
            }
        }

        root = root.child(self.section_header(Section::Tags, "Tags", false, cx));
        if !self.collapsed_tags {
            root = root.child(self.render_tags(cx));
        }
        // A little breathing room under the last section, so the final row is
        // not flush against the window edge when the column is full.
        root.child(div().h(px(8.0)).flex_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use fs_core::{FakeVfs, Spawner, Vfs as _};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::sync::Arc;

    const SETTINGS_PATH: &str = "/config/file-explorer/settings.json";
    /// The fixture home (M7d-b): Locations and the seeded Favorites both
    /// resolve relative to it.
    const TEST_HOME: &str = "/home/me";

    /// The default fixture: an **already-seeded** profile, so the M7d-b
    /// default Favorites do not appear in tests about pinning, reordering and
    /// dropping. Those tests assert on exact favorites lists, and seeding
    /// would otherwise put two rows they never asked for in front of every
    /// assertion. `init_fresh_profile` is the opposite fixture, used by the
    /// seeding tests themselves.
    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        let vfs = init_fresh_profile(cx);
        cx.update(|cx| {
            // Seeding with no candidates flips the flag and pins nothing —
            // exactly the state of a profile that was seeded long ago.
            cx.update_global::<AppSettings, bool>(|settings, _| settings.seed_favorites(&[]));
        });
        vfs
    }

    fn init_fresh_profile(cx: &mut TestAppContext) -> Arc<FakeVfs> {
        cx.update(|cx| {
            let spawner: Arc<dyn Spawner> =
                Arc::new(GpuiSpawner::new(cx.background_executor().clone()));
            let vfs = FakeVfs::new(spawner.clone());
            vfs.insert_tree(
                "/",
                json!({
                    "root": {
                        "sub": { "deeper": {} },
                        "file.txt": "abc",
                        ".hidden-dir": {},
                    },
                    "other": { "b.txt": "b" },
                    // M7d-b: a Mac-shaped home, so Locations resolves to
                    // something and the seeded Favorites have folders to find.
                    // `Desktop`/`Documents` exist and `Downloads` deliberately
                    // does not — the seeder has to drop what is missing.
                    "home": {
                        "me": {
                            "Library": {
                                "Mobile Documents": { "com~apple~CloudDocs": {} }
                            },
                            "OneDrive - Test Ltd": {},
                            "Desktop": {},
                            "Documents": {},
                            ".Trash": {},
                        }
                    },
                    "Network": { "Servers": {} },
                }),
            );
            crate::keymap::init(cx);
            crate::app_state::install(
                cx,
                vfs.clone(),
                spawner,
                Arc::new(LoggingOpener),
                Arc::new(fs_core::StubPlatform::new()),
            );
            crate::settings::init_with_path(cx, PathBuf::from(SETTINGS_PATH));
            // The env's real home is not in this FakeVfs, so without this
            // every Locations row would resolve to nothing (M7d-b).
            crate::app_state::FsContext::global_mut(cx).home = PathBuf::from(TEST_HOME);
            vfs
        })
    }

    fn build_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(Workspace::new)
    }

    fn sidebar_of(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Sidebar> {
        workspace.read_with(cx, |workspace, _| workspace.sidebar().clone())
    }

    #[gpui::test]
    fn sidebar_lists_stub_volumes_and_eject_updates_the_list(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);

        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            let names: Vec<&str> = sidebar.volumes().iter().map(|v| v.name.as_str()).collect();
            assert_eq!(names, ["Macintosh HD", "External SSD", "Camera"]);
            let ejectable: Vec<&str> = sidebar
                .volumes()
                .iter()
                .filter(|v| v.ejectable)
                .map(|v| v.name.as_str())
                .collect();
            assert_eq!(ejectable, ["External SSD", "Camera"]);
        });

        // Eject flows sidebar → workspace → Platform::eject (background), and
        // the poller picks up the removal on its next tick.
        let ssd = VolumeId::from_path(Path::new("/Volumes/External SSD"));
        sidebar.update(cx, |sidebar, cx| sidebar.request_eject(ssd, cx));
        cx.run_until_parked();
        cx.executor().advance_clock(VOLUME_POLL_INTERVAL);
        cx.run_until_parked();
        sidebar.read_with(cx, |sidebar, _| {
            let names: Vec<&str> = sidebar.volumes().iter().map(|v| v.name.as_str()).collect();
            assert_eq!(names, ["Macintosh HD", "Camera"], "SSD ejected");
        });
    }

    #[gpui::test]
    fn sidebar_navigate_event_reaches_the_active_pane(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);

        sidebar.update(cx, |sidebar, cx| {
            sidebar.open_path(PathBuf::from("/root"), cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.active_pane().read(cx).path(),
                Some(Path::new("/root")),
                "NavigateTo must reach the active pane"
            );
        });
    }

    #[gpui::test]
    fn favorites_add_and_remove_persist_immediately(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        sidebar.update(cx, |sidebar, cx| sidebar.add_current_folder(cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).favorites(),
                [PathBuf::from("/root")]
            );
        });
        // Persisted immediately: the settings file already holds the favorite.
        let bytes = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH)))
            .expect("settings file written");
        assert_eq!(written_favorites(&bytes), [PathBuf::from("/root")]);

        // Adding the same folder again is a no-op.
        sidebar.update(cx, |sidebar, cx| sidebar.add_current_folder(cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(AppSettings::global(cx).favorites().len(), 1);
        });

        // Removal persists too.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.remove_favorite(Path::new("/root"), cx);
        });
        cx.run_until_parked();
        let bytes = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH)))
            .expect("settings file rewritten");
        assert!(written_favorites(&bytes).is_empty(), "removal persisted");
    }

    /// The persisted favorites, read back off the settings file — the only
    /// proof that a change survives a restart.
    fn persisted_favorites(vfs: &Arc<FakeVfs>) -> Vec<PathBuf> {
        let bytes = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH)))
            .expect("settings file written");
        written_favorites(&bytes)
    }

    /// M7b writes **only the keys that differ from the compiled-in
    /// defaults**, so the file is not a whole `SettingsContent` and must not
    /// be parsed as one — an absent `favorites` key means "none", not a
    /// malformed file.
    fn written_favorites(bytes: &[u8]) -> Vec<PathBuf> {
        let value: serde_json::Value = serde_json::from_slice(bytes).expect("valid JSON");
        value
            .get("favorites")
            .map(|favorites| {
                serde_json::from_value::<Vec<PathBuf>>(favorites.clone()).expect("path list")
            })
            .unwrap_or_default()
    }

    /// Press on `from`, cross gpui's 2px drag threshold, settle on `to`, and
    /// release — the real gesture, dispatched at real painted coordinates.
    fn drag_and_drop(
        cx: &mut VisualTestContext,
        from: gpui::Point<gpui::Pixels>,
        to: gpui::Point<gpui::Pixels>,
    ) {
        use gpui::{Modifiers, MouseButton};
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            from + gpui::point(px(6.0), px(6.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing painted for {selector:?}"))
    }

    fn set_favorites(cx: &mut VisualTestContext, paths: &[&str]) {
        cx.update(|_, cx| {
            cx.update_global::<AppSettings, ()>(|settings, _| {
                for path in paths {
                    settings.add_favorite(PathBuf::from(path));
                }
            });
        });
        cx.run_until_parked();
    }

    // The gesture, not the method: press a real favorite row, drop it on
    // another row's painted centre, and assert the persisted order. A payload
    // mix-up or a mis-captured `insert_before` at either drop site would leave
    // the method-level tests green while drag-to-reorder silently stopped
    // working — and this is one of the two behaviors the step exists to
    // deliver. Sidebar rows carry `debug_selector`s, so no arithmetic is
    // needed: `debug_bounds` gives the pixels the pointer would land on.
    #[gpui::test]
    fn dragging_a_favorite_row_onto_another_reorders_it(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        set_favorites(cx, &["/root", "/other", "/root/sub"]);

        // Row 2 (`/root/sub`) onto row 1 (`/other`): insert *before* it.
        let from = bounds(cx, "sidebar-favorite-2").center();
        let onto = bounds(cx, "sidebar-favorite-1").center();
        drag_and_drop(cx, from, onto);

        assert_eq!(
            persisted_favorites(&vfs),
            [
                PathBuf::from("/root"),
                PathBuf::from("/root/sub"),
                PathBuf::from("/other"),
            ],
            "dropping a favorite on a row inserts it before that row"
        );

        // Dropped on the section rather than on a row (here its header, the
        // one part of the zone that is never a row): to the end.
        let from = bounds(cx, "sidebar-favorite-0").center();
        let header = bounds(cx, "sidebar-section-Favorites");
        let onto = gpui::point(header.left() + px(20.0), header.center().y);
        drag_and_drop(cx, from, onto);
        assert_eq!(
            persisted_favorites(&vfs),
            [
                PathBuf::from("/root/sub"),
                PathBuf::from("/other"),
                PathBuf::from("/root"),
            ],
            "dropping on the section moves the favorite to the end"
        );
        let _ = workspace;
    }

    // Drag-to-add through the real wiring: a *pane* row (a `DraggedEntries`
    // payload built by `details_list`) dropped on the Favorites section.
    #[gpui::test]
    fn dragging_a_pane_folder_row_onto_favorites_pins_it(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        // With nothing pinned the section's content is its 32px header alone,
        // so the drop zone's own minimum height is the only thing making the
        // Favorites area a target at all. Drop *below* the header to prove it.
        let header = bounds(cx, "sidebar-section-Favorites");
        let zone = bounds(cx, "sidebar-favorites-drop-zone");
        assert!(
            zone.bottom() > header.bottom() + px(ROW_HEIGHT),
            "an empty Favorites list must still present a real drop target \
             (zone {zone:?}, header {header:?})"
        );
        let from = bounds(cx, "dir-row-0").center(); // /root/sub, a folder
        let onto = gpui::point(zone.center().x, header.bottom() + px(4.0));
        drag_and_drop(cx, from, onto);

        assert_eq!(
            persisted_favorites(&vfs),
            [PathBuf::from("/root/sub")],
            "a folder dragged from the pane and dropped under the header is pinned"
        );
    }

    #[gpui::test]
    fn favorites_reorder_persists(cx: &mut TestAppContext) {
        // §8 drag & drop closes the M2-deferred favorites-reordering gap: a
        // favorite dropped on a row lands immediately before it, one dropped
        // on the section lands at the end, and both persist at once. The
        // gesture itself is covered above; this pins the *rule* (including the
        // restart) without depending on painted geometry.
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.update(|_, cx| {
            cx.update_global::<AppSettings, ()>(|settings, _| {
                for path in ["/root", "/other", "/root/sub"] {
                    settings.add_favorite(PathBuf::from(path));
                }
            });
        });

        sidebar.update(cx, |sidebar, cx| {
            sidebar.reorder_favorite(Path::new("/root/sub"), Some(Path::new("/other")), cx)
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).favorites(),
                [
                    PathBuf::from("/root"),
                    PathBuf::from("/root/sub"),
                    PathBuf::from("/other"),
                ]
            );
        });
        assert_eq!(
            persisted_favorites(&vfs),
            [
                PathBuf::from("/root"),
                PathBuf::from("/root/sub"),
                PathBuf::from("/other"),
            ],
            "the new order is on disk immediately"
        );

        // Dropped on the section rather than a row: to the end.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.reorder_favorite(Path::new("/root"), None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            persisted_favorites(&vfs),
            [
                PathBuf::from("/root/sub"),
                PathBuf::from("/other"),
                PathBuf::from("/root"),
            ]
        );

        // A restart reads the reordered list back.
        cx.update(|_, cx| crate::settings::init_with_path(cx, PathBuf::from(SETTINGS_PATH)));
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).favorites(),
                [
                    PathBuf::from("/root/sub"),
                    PathBuf::from("/other"),
                    PathBuf::from("/root"),
                ],
                "order survives a restart"
            );
        });
    }

    #[gpui::test]
    fn dragging_folders_onto_favorites_pins_only_the_folders(cx: &mut TestAppContext) {
        // Drag-to-add (the other half of the M2-deferred gap): only folders
        // can be pinned, so each dropped path is stat'ed off the UI thread
        // first — a dragged *file* is silently refused.
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);

        sidebar.update(cx, |sidebar, cx| {
            sidebar.add_favorites_from_drop(
                vec![
                    PathBuf::from("/root/sub"),
                    PathBuf::from("/root/file.txt"),
                    PathBuf::from("/other"),
                ],
                cx,
            )
        });
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).favorites(),
                [PathBuf::from("/root/sub"), PathBuf::from("/other")],
                "folders pinned in drop order; the file was refused"
            );
        });
        assert_eq!(
            persisted_favorites(&vfs),
            [PathBuf::from("/root/sub"), PathBuf::from("/other")],
            "one write, already on disk"
        );

        // Re-dropping a pinned folder changes (and persists) nothing.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.add_favorites_from_drop(vec![PathBuf::from("/other")], cx)
        });
        cx.run_until_parked();
        assert_eq!(persisted_favorites(&vfs).len(), 2);
    }

    // A second drop arriving while the first is still stat'ing must not cancel
    // it: the probes are queued behind one task rather than replacing a single
    // `Option<Task>` slot, so both folders get pinned.
    #[gpui::test]
    fn a_second_drop_does_not_cancel_an_in_flight_one(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);

        // Both drops land before the executor is ever allowed to run, which is
        // exactly the "first probe still awaiting" window.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.add_favorites_from_drop(vec![PathBuf::from("/root/sub")], cx)
        });
        sidebar.update(cx, |sidebar, cx| {
            sidebar.add_favorites_from_drop(vec![PathBuf::from("/other")], cx)
        });
        cx.run_until_parked();

        assert_eq!(
            persisted_favorites(&vfs),
            [PathBuf::from("/root/sub"), PathBuf::from("/other")],
            "neither drop was swallowed"
        );

        // ...and the queue is empty again, so a later drop starts a fresh task.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.add_favorites_from_drop(vec![PathBuf::from("/root/sub/deeper")], cx)
        });
        cx.run_until_parked();
        assert_eq!(persisted_favorites(&vfs).len(), 3);
    }

    // Regression: `settings::init` swaps the disk-loaded global in from a
    // background task *after* the sidebar's first paint — the sidebar must
    // observe the global and repaint, or boot-persisted favorites stay
    // invisible until an unrelated redraw.
    #[gpui::test]
    fn external_settings_swap_repaints_the_sidebar(cx: &mut TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;

        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        let notified = Rc::new(Cell::new(false));
        let flag = notified.clone();
        let _observer = cx.update(|_, cx| cx.observe(&sidebar, move |_, _| flag.set(true)));

        // Simulate the boot-time load completing (settings::init's set_global).
        cx.update(|_, cx| {
            let mut settings = AppSettings::new(PathBuf::from(SETTINGS_PATH));
            settings.add_favorite(PathBuf::from("/root"));
            cx.set_global(settings);
        });
        cx.run_until_parked();
        assert!(
            notified.get(),
            "sidebar must repaint when AppSettings is swapped in externally"
        );
    }

    #[gpui::test]
    fn sections_collapse_and_reopen(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);

        sidebar.update(cx, |sidebar, cx| {
            assert!(!sidebar.section_collapsed(Section::Devices));
            sidebar.toggle_section(Section::Devices, cx);
            assert!(sidebar.section_collapsed(Section::Devices));
            assert!(
                !sidebar.section_collapsed(Section::Favorites),
                "sections collapse independently"
            );
            sidebar.toggle_section(Section::Devices, cx);
            assert!(!sidebar.section_collapsed(Section::Devices));
        });
    }

    // ------------------------------------------------------------------
    // M7d-b: Locations, seeded Favorites, Recents, and the two-level rule
    // ------------------------------------------------------------------

    fn location_names(sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext) -> Vec<String> {
        sidebar.read_with(cx, |sidebar, _| {
            sidebar.locations().iter().map(|l| l.name.clone()).collect()
        })
    }

    /// The section the M7d brief asked for, resolved against the fixture's
    /// Mac-shaped home and in the order a person reads it.
    #[gpui::test]
    fn locations_resolve_from_the_home_directory(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        assert_eq!(
            location_names(&sidebar, cx),
            vec![
                "iCloud Drive".to_string(),
                "OneDrive - Test Ltd".to_string(),
                "me".to_string(),
                "Network".to_string(),
                "Trash".to_string(),
            ],
        );
    }

    /// Clicking a location navigates the active pane, like every other row —
    /// the sidebar emits, the workspace acts (§2).
    #[gpui::test]
    fn clicking_a_location_navigates_the_active_pane(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        let target = sidebar.read_with(cx, |sidebar, _| {
            sidebar
                .locations()
                .iter()
                .find(|l| l.kind == LocationKind::Home)
                .expect("a Home location")
                .path
                .clone()
        });
        sidebar.update(cx, |sidebar, cx| sidebar.open_path(target.clone(), cx));
        cx.run_until_parked();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        assert_eq!(
            pane.read_with(cx, |pane, _| pane.path().map(Path::to_path_buf)),
            Some(target),
        );
    }

    /// A fresh profile gets the defaults — and only the ones that exist. The
    /// fixture has Desktop and Documents but deliberately no Downloads.
    #[gpui::test]
    fn a_fresh_profile_is_seeded_with_the_defaults_that_exist(cx: &mut TestAppContext) {
        let _vfs = init_fresh_profile(cx);
        let (workspace, cx) = build_workspace(cx);
        let _sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).favorites(),
                [
                    PathBuf::from("/home/me/Desktop"),
                    PathBuf::from("/home/me/Documents"),
                ],
                "Downloads is absent from the fixture, so it must not be pinned"
            );
            assert!(AppSettings::global(cx).favorites_seeded());
        });
    }

    /// The M7d brief's own words — "so emptying them stays empty". Seeding is
    /// guarded by the flag, not by the list being empty, so unpinning
    /// everything survives the next launch.
    #[gpui::test]
    fn unpinning_every_seeded_favorite_survives_a_relaunch(cx: &mut TestAppContext) {
        let _vfs = init_fresh_profile(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        for path in [
            PathBuf::from("/home/me/Desktop"),
            PathBuf::from("/home/me/Documents"),
        ] {
            sidebar.update(cx, |sidebar, cx| sidebar.remove_favorite(&path, cx));
        }
        cx.run_until_parked();
        cx.update(|_, cx| assert!(AppSettings::global(cx).favorites().is_empty()));

        // A second sidebar is the next launch: same settings global, same
        // seeded flag, and it must not re-pin anything.
        let (workspace2, cx) = build_workspace(cx);
        let _sidebar2 = sidebar_of(&workspace2, cx);
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(
                AppSettings::global(cx).favorites().is_empty(),
                "the user emptied them deliberately; a relaunch must not argue"
            );
        });
    }

    /// Recents is written by the **workspace** off `PaneEvent::Navigated`, so
    /// this drives a real navigation rather than calling `push_recent`.
    #[gpui::test]
    fn navigating_records_a_recent_most_recent_first(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let _sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        for dir in ["/root", "/other", "/root/sub"] {
            pane.update(cx, |pane, cx| pane.navigate_to(Path::new(dir), cx));
            cx.run_until_parked();
        }

        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).recents(),
                [
                    PathBuf::from("/root/sub"),
                    PathBuf::from("/other"),
                    PathBuf::from("/root"),
                ],
                "most recent first"
            );
        });
    }

    /// An in-place reload is not a navigation. Re-entering the folder the pane
    /// is already in must not push a duplicate — this is the guard on
    /// `path_changed` in `Pane::load`, checked end to end.
    #[gpui::test]
    fn re_entering_the_same_folder_does_not_duplicate_a_recent(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let _sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        for _ in 0..3 {
            pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
            cx.run_until_parked();
        }

        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).recents(),
                [PathBuf::from("/root")],
                "one entry, not three"
            );
        });
    }

    /// The two-level rule, as a compile-and-behavior check rather than a
    /// comment: the sidebar exposes no way to expand anything. If a future
    /// change reintroduces a tree, this section list is where it shows up.
    #[gpui::test]
    fn every_section_collapses_independently(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        let sections = [
            Section::Devices,
            Section::Locations,
            Section::Favorites,
            Section::Recents,
            Section::Tags,
        ];
        sidebar.update(cx, |sidebar, cx| {
            for section in sections {
                assert!(
                    !sidebar.section_collapsed(section),
                    "{section:?} starts open"
                );
                sidebar.toggle_section(section, cx);
                assert!(sidebar.section_collapsed(section));
            }
            // All five collapsed at once: nothing is pinned, nothing is
            // `flex_1`, so the column is just five headers.
            for section in sections {
                assert!(sidebar.section_collapsed(section));
                sidebar.toggle_section(section, cx);
                assert!(!sidebar.section_collapsed(section));
            }
        });
    }

    /// Collapsing a section moves the ones below it **up** — the behavior the
    /// M7d brief asked for by name, and the thing M7c's `flex_1` folder tree
    /// made impossible (it ate the spare height and pinned Tags to the bottom
    /// edge). Asserted on painted geometry, not on the section list.
    #[gpui::test]
    fn collapsing_a_section_moves_the_ones_below_it_up(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        let before = bounds(cx, "sidebar-section-Tags").origin.y;
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_section(Section::Devices, cx)
        });
        cx.run_until_parked();
        let after = bounds(cx, "sidebar-section-Tags").origin.y;

        assert!(
            after < before,
            "Tags should rise when Devices collapses, but went {before:?} -> {after:?}"
        );
    }

    /// **Regression (M7d-b).** Neither startup writer may touch settings
    /// before the initial load has landed.
    ///
    /// Why this matters is documented on `AppSettings::loaded` and pinned by
    /// `settings::tests::a_write_inside_the_load_window_discards_the_file`:
    /// a write inside that window makes `init_with_path` discard the entire
    /// on-disk file, and the following `save` persists defaults over it. On a
    /// real profile that silently deleted a saved `Graphite Light`.
    ///
    /// The window cannot be reproduced through `build_workspace` — it parks
    /// the executor, so the load has always landed by the time a test can
    /// navigate. So this puts the global *back* into the not-yet-loaded state
    /// and drives both writers at it: the sidebar's seeding (via the settings
    /// observer) and the workspace's Recents (via a real navigation).
    #[gpui::test]
    fn neither_startup_writer_touches_settings_before_the_load_lands(cx: &mut TestAppContext) {
        let _vfs = init_fresh_profile(cx);
        let (workspace, cx) = build_workspace(cx);
        let _sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        // Back into the boot window: a fresh global holds defaults and has not
        // loaded. Replacing it also notifies the sidebar's observer, which is
        // one of the two writers under test.
        cx.update(|_, cx| {
            cx.set_global(AppSettings::new(PathBuf::from(SETTINGS_PATH)));
            assert!(!AppSettings::global(cx).is_loaded());
        });

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        cx.update(|_, cx| {
            let settings = AppSettings::global(cx);
            assert!(
                settings.recents().is_empty(),
                "Recents must not be recorded before the load lands: {:?}",
                settings.recents(),
            );
            assert!(
                !settings.favorites_seeded(),
                "seeding must wait for the load too"
            );
            assert_eq!(
                settings.content(),
                &crate::settings::SettingsContent::default(),
                "nothing at all was written into the load window"
            );
        });
    }

    /// And the gates delay the work rather than cancelling it: once the load
    /// has landed, a fresh profile still gets seeded and navigation still
    /// records.
    #[gpui::test]
    fn both_writers_resume_once_the_load_has_landed(cx: &mut TestAppContext) {
        let _vfs = init_fresh_profile(cx);
        let (workspace, cx) = build_workspace(cx);
        let _sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        cx.update(|_, cx| {
            let settings = AppSettings::global(cx);
            assert!(settings.favorites_seeded(), "seeding ran, just later");
            assert_eq!(
                settings.favorites(),
                [
                    PathBuf::from("/home/me/Desktop"),
                    PathBuf::from("/home/me/Documents"),
                ],
            );
            assert_eq!(settings.recents(), [PathBuf::from("/root")]);
        });
    }

    // ------------------------------------------------------------------
    // M7d-c: no writes at startup, and Recents debounced
    // ------------------------------------------------------------------

    /// How many times the settings file has been written.
    fn settings_writes(vfs: &Arc<FakeVfs>) -> usize {
        vfs.write_count(Path::new(SETTINGS_PATH))
    }

    /// **Booting must not touch the disk.** Seeding is deterministic — the
    /// same defaults filtered by the same existence check — so it is derived
    /// in memory on every launch rather than persisted. A startup write buys
    /// nothing and costs a disk round-trip on the one path where the app
    /// should feel instant.
    #[gpui::test]
    fn a_fresh_profile_boots_without_writing_anything(cx: &mut TestAppContext) {
        let vfs = init_fresh_profile(cx);
        let (workspace, cx) = build_workspace(cx);
        let _sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        cx.update(|_, cx| {
            let settings = AppSettings::global(cx);
            assert!(
                settings.favorites_seeded(),
                "seeded in memory, so the rows are there"
            );
            assert_eq!(
                settings.favorites(),
                [
                    PathBuf::from("/home/me/Desktop"),
                    PathBuf::from("/home/me/Documents"),
                ],
            );
        });
        assert_eq!(
            settings_writes(&vfs),
            0,
            "a fresh profile must reach a usable sidebar with no disk write"
        );
    }

    /// And the seed still reaches disk — carried by the first change the user
    /// actually makes, because `save` writes the whole document. This is what
    /// keeps "emptying them stays empty" working without a startup write.
    #[gpui::test]
    fn the_seed_is_persisted_by_the_first_real_change(cx: &mut TestAppContext) {
        let vfs = init_fresh_profile(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();
        assert_eq!(settings_writes(&vfs), 0);

        // The user unpins one. That is a real change, so it persists — and it
        // carries the seeded flag and the surviving seeded row with it.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.remove_favorite(Path::new("/home/me/Desktop"), cx)
        });
        cx.run_until_parked();

        assert!(settings_writes(&vfs) > 0, "a user change persists");
        let written = persisted_favorites(&vfs);
        assert_eq!(
            written,
            vec![PathBuf::from("/home/me/Documents")],
            "the unpin and the surviving seeded row both reached disk"
        );
    }

    /// **Browsing is a burst, and it must cost one write, not one per click.**
    /// Four navigations used to be four full rewrites of settings.json, each a
    /// temp file plus a rename, for a list only interesting once the user
    /// stops moving.
    #[gpui::test]
    fn a_burst_of_navigation_costs_one_recents_write(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let _sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();
        let before = settings_writes(&vfs);

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        for dir in ["/root", "/other", "/root/sub", "/root"] {
            pane.update(cx, |pane, cx| pane.navigate_to(Path::new(dir), cx));
            cx.run_until_parked();
        }
        assert_eq!(
            settings_writes(&vfs),
            before,
            "nothing written while the user is still moving"
        );

        // The list is live in memory the whole time — the debounce delays the
        // *disk*, never the sidebar.
        cx.update(|_, cx| {
            assert_eq!(
                AppSettings::global(cx).recents().first(),
                Some(&PathBuf::from("/root")),
            );
        });

        cx.executor()
            .advance_clock(crate::workspace::RECENTS_FLUSH_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(
            settings_writes(&vfs),
            before + 1,
            "one write once it settles, not four"
        );
    }
}
