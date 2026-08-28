//! The sidebar (ARCHITECTURE.md §2 `Sidebar` entity, §8 "Sidebar tree",
//! plan §2 sidebar blueprint) — M2 surface.
//!
//! Three collapsible sections:
//! - **Devices**: mounted volumes from the [`fs_core::Platform`] seam (name,
//!   free space, an eject affordance on ejectable volumes), kept current by
//!   the polling [`watch_volumes`] stream (fake time under `#[gpui::test]`).
//! - **Favorites**: user-pinned folders from [`AppSettings`] — click
//!   navigates, `+` in the header pins the active pane's folder, `✕` on a row
//!   unpins; every change persists immediately through `Vfs::atomic_write` on
//!   the background executor. (Drag-to-add, context menus, and reordering
//!   arrive at M3 with the drag infrastructure.)
//! - **Tags** (M6b): the Finder tags [`fs_core::Platform::known_tags`] reports,
//!   each with its fixed macOS palette dot. Clicking one filters the active
//!   pane to the items in the open folder carrying it; clicking the lit one
//!   again clears the filter. **Deliberate deviation:** Finder's tag click is a
//!   volume-wide Spotlight query — see [`crate::tags`] and
//!   `docs/AS_BUILT.md`.
//! - **Folders**: an Explorer-style tree. Expanded nodes are flattened into a
//!   `Vec<TreeRow>` rendered by `uniform_list`; disclosure triangles mutate
//!   the expansion set and re-flatten (§8 flat-projection technique, shared
//!   with the details view's in-place expansion).
//!
//! Events up, method calls down (§2): the sidebar only emits
//! [`SidebarEvent`]s; the owning [`Workspace`] navigates the active pane and
//! runs `Platform::eject` on the background executor.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs_core::{
    FileEntry, SortSpec, Tag, VolumeId, VolumeInfo, WatchGuard, list_dir, watch_volumes,
};
use futures::StreamExt as _;
use gpui::{
    Context, EventEmitter, ExternalPaths, IntoElement, Render, SharedString, Subscription, Task,
    WeakEntity, Window, div, prelude::*, px, uniform_list,
};

use crate::app_state::FsContext;
use crate::drag::{self, DraggedEntries, DraggedFavorite};
use crate::pane::format_bytes;
use crate::settings::AppSettings;
use crate::theme::Theme;
use crate::workspace::Workspace;

/// How often the volume list is re-polled (ARCHITECTURE.md §6: change
/// detection is a poller on `Spawner::timer`, so tests advance a fake clock).
pub const VOLUME_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Fixed tree row height (uniform_list requirement).
const TREE_ROW_HEIGHT: f32 = 22.0;
/// Favorites reorder drop-target tint (the "insert before this row" cue).
const FAVORITE_REORDER_ALPHA: f32 = 0.35;
/// Horizontal indent per tree depth level.
const TREE_INDENT: f32 = 12.0;

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
    Favorites,
    Tags,
    Tree,
}

/// One visible row of the flattened folder tree (§8 flat projection).
#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub path: Arc<Path>,
    pub name: SharedString,
    pub depth: usize,
    pub expanded: bool,
}

pub struct Sidebar {
    theme: Theme,
    workspace: WeakEntity<Workspace>,
    volumes: Vec<VolumeInfo>,
    collapsed_devices: bool,
    collapsed_favorites: bool,
    collapsed_tags: bool,
    collapsed_tree: bool,
    /// The tags the **Tags** section lists: the palette plus whatever the user
    /// has, loaded once off the UI thread. Seeded with
    /// [`fs_core::standard_tags`] so the section is never empty and the first
    /// paint does no I/O.
    tags: Vec<Tag>,
    /// The one-shot `known_tags` load; a field, never detached (§5).
    _tags_load: Option<Task<()>>,
    /// Expanded tree nodes, path-keyed so expansion survives re-flattening.
    expanded: BTreeSet<Arc<Path>>,
    /// Background-loaded, sorted, dirs-only child listings per tree node.
    children: HashMap<Arc<Path>, Vec<FileEntry>>,
    /// The flat projection rendered by `uniform_list`.
    flat: Vec<TreeRow>,
    /// In-flight child loads, held so dropping the sidebar cancels them.
    _child_loads: HashMap<Arc<Path>, Task<()>>,
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
    pub fn new(theme: Theme, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let fs = FsContext::global(cx);
        let platform = fs.platform.clone();
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
        let settings_observer = cx.observe_global::<AppSettings>(|_, cx| cx.notify());
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
        Self {
            theme,
            workspace,
            volumes: Vec::new(),
            collapsed_devices: false,
            collapsed_favorites: false,
            collapsed_tags: false,
            collapsed_tree: false,
            tags: fs_core::standard_tags(),
            _tags_load: Some(tags_load),
            expanded: BTreeSet::new(),
            children: HashMap::new(),
            flat: Vec::new(),
            _child_loads: HashMap::new(),
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
        self.reflatten();
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
            Section::Tree => self.collapsed_tree,
        }
    }

    pub fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let flag = match section {
            Section::Devices => &mut self.collapsed_devices,
            Section::Favorites => &mut self.collapsed_favorites,
            Section::Tags => &mut self.collapsed_tags,
            Section::Tree => &mut self.collapsed_tree,
        };
        *flag = !*flag;
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Folder tree (§8 flat projection)
    // ------------------------------------------------------------------

    /// The current flat projection (test observability).
    pub fn flat_rows(&self) -> &[TreeRow] {
        &self.flat
    }

    /// Disclosure triangle: expand (loading children in the background on
    /// first expansion) or collapse, then re-flatten.
    pub fn toggle_expanded(&mut self, path: &Path, cx: &mut Context<Self>) {
        let key: Arc<Path> = Arc::from(path);
        if !self.expanded.remove(&key) {
            self.expanded.insert(key.clone());
            self.load_children(key, cx);
        }
        self.reflatten();
        cx.notify();
    }

    /// An external change (a pane's watcher batch, forwarded by the
    /// workspace) landed in folders the tree caches child listings for: those
    /// listings are stale. A collapsed node just loses its cache (re-listed on
    /// the next expansion); an expanded one keeps its rows painted while a
    /// fresh listing loads over the top.
    pub fn invalidate_children(&mut self, dirs: &[Arc<Path>], cx: &mut Context<Self>) {
        for dir in dirs {
            // An in-flight load would otherwise satisfy `load_children`'s
            // staleness check and never re-run.
            let was_loading = self._child_loads.remove(dir).is_some();
            if self.expanded.contains(dir) {
                if was_loading || self.children.contains_key(dir) {
                    self.start_child_load(dir.clone(), cx);
                }
            } else {
                self.children.remove(dir);
            }
        }
    }

    /// Background-list a node's children (dirs only, default sort). Results
    /// are cached; collapsing keeps them so re-expanding paints instantly.
    fn load_children(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
        if self.children.contains_key(&path) || self._child_loads.contains_key(&path) {
            return;
        }
        self.start_child_load(path, cx);
    }

    /// Unconditional (re)list of a node's children — the invalidation path,
    /// where a cached listing exists but is known stale.
    fn start_child_load(&mut self, path: Arc<Path>, cx: &mut Context<Self>) {
        let vfs = FsContext::global(cx).vfs.clone();
        let load_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(list_dir(
                    vfs,
                    load_path.clone(),
                    SortSpec::default(),
                    false,
                    0,
                ))
                .await;
            this.update(cx, |this, cx| {
                // An unreadable directory simply has no children in the tree.
                let dirs = match result {
                    Ok(snapshot) => snapshot
                        .entries
                        .iter()
                        .filter(|entry| entry.is_dir_like())
                        .cloned()
                        .collect(),
                    Err(_) => Vec::new(),
                };
                this.children.insert(load_path, dirs);
                this.reflatten();
                cx.notify();
            })
            .ok();
        });
        self._child_loads.insert(path, task);
    }

    /// Rebuild the flat projection: volume roots at depth 0, each expanded
    /// node's cached children spliced beneath it with `depth + 1`.
    fn reflatten(&mut self) {
        let mut flat = Vec::new();
        for volume in &self.volumes {
            let path: Arc<Path> = Arc::from(volume.path.as_path());
            Self::flatten_into(
                &self.expanded,
                &self.children,
                &mut flat,
                path,
                SharedString::from(volume.name.clone()),
                0,
            );
        }
        self.flat = flat;
    }

    fn flatten_into(
        expanded: &BTreeSet<Arc<Path>>,
        children: &HashMap<Arc<Path>, Vec<FileEntry>>,
        flat: &mut Vec<TreeRow>,
        path: Arc<Path>,
        name: SharedString,
        depth: usize,
    ) {
        let is_expanded = expanded.contains(&path);
        flat.push(TreeRow {
            path: path.clone(),
            name,
            depth,
            expanded: is_expanded,
        });
        if is_expanded && let Some(kids) = children.get(&path) {
            for kid in kids {
                Self::flatten_into(
                    expanded,
                    children,
                    flat,
                    kid.path.clone(),
                    SharedString::new(kid.name.clone()),
                    depth + 1,
                );
            }
        }
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
        let theme = self.theme.clone();
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
            .child(SharedString::new_static(if collapsed {
                "▸"
            } else {
                "▾"
            }))
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
                    .child(SharedString::new_static("+")),
            );
        }
        header
    }

    fn render_devices(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = self.theme.clone();
        let rows: Vec<_> = self
            .volumes
            .iter()
            .enumerate()
            .map(|(ix, volume)| {
                let navigate_path = volume.path.clone();
                let eject_id = volume.volume_id.clone();
                let volume_name = volume.name.clone();
                let mut row = div()
                    .id(("sidebar-volume", ix))
                    .debug_selector(|| format!("sidebar-volume-{volume_name}"))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(16.0))
                    .py(px(2.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.accent.opacity(0.15)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_path(navigate_path.clone(), cx);
                    }))
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
                            .child(SharedString::new_static("⏏")),
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
        let theme = self.theme.clone();
        let external_theme = theme.clone();
        let favorite_theme = theme.clone();
        let mut section = div()
            .id("sidebar-favorites-drop-zone")
            .debug_selector(|| "sidebar-favorites-drop-zone".into())
            .flex()
            .flex_col()
            .min_h(px(TREE_ROW_HEIGHT * 3.0))
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
        let theme = self.theme.clone();
        let rows: Vec<_> = favorites
            .iter()
            .enumerate()
            .map(|(ix, path)| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let navigate_path = path.clone();
                let remove_path = path.clone();
                let drag_path = path.clone();
                let insert_before = path.clone();
                let ghost_theme = theme.clone();
                let ghost_label = SharedString::from(name.clone());
                div()
                    // Path-keyed, not index-keyed: this row is a drag source,
                    // and gpui persists a stateful element's pending press by
                    // element id across frames — an index would let a press on
                    // one favorite start a drag carrying whichever favorite the
                    // list has since shuffled into that slot (invariant #2).
                    .id(gpui::ElementId::Path(Arc::from(path.as_path())))
                    .debug_selector(|| format!("sidebar-favorite-{ix}"))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(16.0))
                    .py(px(2.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.accent.opacity(0.15)))
                    // §8 reordering: a row is both a drag source and the
                    // "insert before me" target. Highlighted as a background
                    // tint rather than an insertion rule, so arming a target
                    // never nudges the rows below it.
                    .on_drag(DraggedFavorite { path: drag_path }, move |_, _, _, cx| {
                        drag::ghost(ghost_label.clone(), ghost_theme.clone(), cx)
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
                            .text_color(theme.muted)
                            .hover(|s| s.text_color(theme.error))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_favorite(&remove_path, cx);
                            }))
                            .child(SharedString::new_static("✕")),
                    )
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
        let theme = self.theme.clone();
        let active = self.active_tag(cx);
        let rows: Vec<_> = self
            .tags
            .iter()
            .enumerate()
            .map(|(ix, tag)| {
                let is_active = active.as_ref() == Some(tag);
                let clicked = tag.clone();
                let name = SharedString::new(&tag.name);
                let mut row = div()
                    .id(("sidebar-tag", ix))
                    .debug_selector(|| format!("sidebar-tag-{ix}"))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(16.0))
                    .py(px(2.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.accent.opacity(0.15)))
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

    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        uniform_list(
            "sidebar-tree",
            self.flat.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .map(|ix| render_tree_row(this, ix, cx))
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
    }
}

fn render_tree_row(
    this: &mut Sidebar,
    ix: usize,
    cx: &mut Context<Sidebar>,
) -> gpui::Stateful<gpui::Div> {
    let theme = this.theme.clone();
    let row = this.flat[ix].clone();
    let toggle_path = row.path.clone();
    let navigate_path = row.path.to_path_buf();
    div()
        .id(("sidebar-tree-row", ix))
        .debug_selector(|| format!("sidebar-tree-row-{ix}"))
        .flex()
        .items_center()
        .h(px(TREE_ROW_HEIGHT))
        .pl(px(TREE_INDENT + row.depth as f32 * TREE_INDENT))
        .pr(px(8.0))
        .cursor_pointer()
        .hover(|s| s.bg(theme.accent.opacity(0.15)))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.open_path(navigate_path.clone(), cx);
        }))
        .child(
            div()
                .id(("sidebar-tree-toggle", ix))
                .debug_selector(|| format!("sidebar-tree-toggle-{ix}"))
                .w(px(14.0))
                .text_color(theme.muted)
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_expanded(&toggle_path, cx);
                }))
                .child(SharedString::new_static(if row.expanded {
                    "▾"
                } else {
                    "▸"
                })),
        )
        .child(div().flex_1().truncate().child(row.name.clone()))
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let favorites: Vec<PathBuf> = AppSettings::global(cx).favorites().to_vec();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .text_size(px(13.0))
            .text_color(theme.text)
            .child(self.section_header(Section::Devices, "Devices", false, cx));
        if !self.collapsed_devices {
            root = root.child(self.render_devices(cx));
        }
        root = root.child(self.favorites_section(&favorites, cx));
        root = root.child(self.section_header(Section::Tags, "Tags", false, cx));
        if !self.collapsed_tags {
            root = root.child(self.render_tags(cx));
        }
        root = root.child(self.section_header(Section::Tree, "Folders", false, cx));
        if !self.collapsed_tree {
            root = root.child(self.render_tree(cx));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{GpuiSpawner, LoggingOpener};
    use crate::settings::SettingsContent;
    use fs_core::{FakeVfs, Spawner, Vfs as _};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::sync::Arc;

    const SETTINGS_PATH: &str = "/config/file-explorer/settings.json";

    fn init_test(cx: &mut TestAppContext) -> Arc<FakeVfs> {
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
            vfs
        })
    }

    fn build_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.add_window_view(|window, cx| Workspace::new(crate::Theme::dark(), window, cx))
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
        let content: SettingsContent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(content.favorites, [PathBuf::from("/root")]);

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
        let content: SettingsContent = serde_json::from_slice(&bytes).unwrap();
        assert!(content.favorites.is_empty(), "removal persisted");
    }

    /// The persisted favorites, read back off the settings file — the only
    /// proof that a change survives a restart.
    fn persisted_favorites(vfs: &Arc<FakeVfs>) -> Vec<PathBuf> {
        let bytes = futures::executor::block_on(vfs.load(Path::new(SETTINGS_PATH)))
            .expect("settings file written");
        serde_json::from_slice::<SettingsContent>(&bytes)
            .unwrap()
            .favorites
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
            zone.bottom() > header.bottom() + px(TREE_ROW_HEIGHT),
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

    #[gpui::test]
    fn tree_expand_reflattens_and_collapse_restores(cx: &mut TestAppContext) {
        let _vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);
        cx.run_until_parked();

        // Compare as `PathBuf`s: path equality is component-wise, so the
        // assertions hold on Windows (`\`) and Unix (`/`) alike.
        let rows = |sidebar: &Entity<Sidebar>, cx: &mut VisualTestContext| {
            sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .flat_rows()
                    .iter()
                    .map(|row| (row.path.to_path_buf(), row.depth))
                    .collect::<Vec<_>>()
            })
        };
        let expect = |entries: &[(&str, usize)]| {
            entries
                .iter()
                .map(|(path, depth)| (PathBuf::from(path), *depth))
                .collect::<Vec<_>>()
        };

        // Volume roots only, depth 0.
        assert_eq!(
            rows(&sidebar, cx),
            expect(&[
                ("/", 0),
                ("/Volumes/External SSD", 0),
                ("/Volumes/Camera", 0),
            ])
        );

        // Expand "/": its dirs-only, sorted, non-hidden children splice in at
        // depth 1 once the background load lands.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/"), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            rows(&sidebar, cx),
            expect(&[
                ("/", 0),
                ("/other", 1),
                ("/root", 1),
                ("/Volumes/External SSD", 0),
                ("/Volumes/Camera", 0),
            ])
        );

        // Expand a nested node: depth 2 (hidden dirs and files excluded).
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/root"), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            rows(&sidebar, cx),
            expect(&[
                ("/", 0),
                ("/other", 1),
                ("/root", 1),
                ("/root/sub", 2),
                ("/Volumes/External SSD", 0),
                ("/Volumes/Camera", 0),
            ])
        );

        // Collapse "/": the whole subtree re-flattens away…
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/"), cx)
        });
        assert_eq!(
            rows(&sidebar, cx),
            expect(&[
                ("/", 0),
                ("/Volumes/External SSD", 0),
                ("/Volumes/Camera", 0),
            ])
        );

        // …and re-expanding restores it instantly from cached children,
        // including the still-expanded nested node.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/"), cx)
        });
        assert_eq!(
            rows(&sidebar, cx),
            expect(&[
                ("/", 0),
                ("/other", 1),
                ("/root", 1),
                ("/root/sub", 2),
                ("/Volumes/External SSD", 0),
                ("/Volumes/Camera", 0),
            ])
        );
    }

    // §6 invalidation: the tree caches child listings, and the only news it
    // gets about external changes is the active pane's watcher batch
    // (Pane → PaneEvent::DirsChanged → Workspace → Sidebar).
    #[gpui::test]
    fn external_change_invalidates_the_cached_tree_children(cx: &mut TestAppContext) {
        let vfs = init_test(cx);
        let (workspace, cx) = build_workspace(cx);
        let sidebar = sidebar_of(&workspace, cx);

        // The pane's watch is what observes /root.
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update(cx, |pane, cx| pane.navigate_to(Path::new("/root"), cx));
        cx.run_until_parked();

        // "/root" is only a visible row once its parent volume is expanded.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/"), cx)
        });
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/root"), cx)
        });
        cx.run_until_parked();
        // The rows *inside* /root (depth 2 under the expanded volume root).
        let child_rows = |cx: &mut VisualTestContext| {
            sidebar.read_with(cx, |sidebar, _| {
                sidebar
                    .flat_rows()
                    .iter()
                    .filter(|row| row.path.parent() == Some(Path::new("/root")))
                    .map(|row| row.path.to_path_buf())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(child_rows(cx), vec![PathBuf::from("/root/sub")]);

        // A folder appears in /root behind the app's back.
        vfs.insert_dir("/root/newdir");
        cx.executor().advance_clock(crate::pane::WATCH_LATENCY);
        cx.run_until_parked();

        assert_eq!(
            child_rows(cx),
            vec![PathBuf::from("/root/newdir"), PathBuf::from("/root/sub")],
            "the expanded node re-listed instead of keeping its stale children"
        );

        // A collapsed node's cache is dropped too: re-expanding re-lists.
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/root"), cx)
        });
        cx.run_until_parked();
        vfs.insert_dir("/root/later");
        cx.executor().advance_clock(crate::pane::WATCH_LATENCY);
        cx.run_until_parked();
        sidebar.update(cx, |sidebar, cx| {
            sidebar.toggle_expanded(Path::new("/root"), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            child_rows(cx),
            vec![
                PathBuf::from("/root/later"),
                PathBuf::from("/root/newdir"),
                PathBuf::from("/root/sub")
            ],
        );
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
}
