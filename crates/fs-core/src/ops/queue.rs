//! The job queue (ARCHITECTURE.md §6, `ops/queue.rs`).
//!
//! Every job runs on a single serial lane keyed by its **destination** volume
//! ([`crate::Vfs::volume_key`] of [`FileOp::lane_path`]): same-volume ops are
//! strictly ordered, ops targeting different volumes parallelize, and there is
//! no two-lock scheme — one lane per job, zero deadlock surface. Events flow
//! over one channel ([`JobQueue::subscribe`]) whose sole consumer is the app's
//! JobsModel. An RAII [`JobTracker`] guarantees exactly one terminal event per
//! job even on panic. Conflicts park the lane on a oneshot until
//! [`JobQueue::resolve`]; [`JobQueue::cancel`] trips a flag checked between
//! files and (via the copy progress callback) between copy chunks.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Result, anyhow, bail};
use futures::StreamExt as _;
use futures::channel::oneshot;

use crate::entry::{EntryKind, EntryMeta};
use crate::exec::Spawner;
use crate::ops::job::{
    Conflict, ConflictChoice, JobEvent, JobId, JobInfo, JobKind, OpReceipt, Resolution,
};
use crate::ops::{FileOp, keep_both_candidates};
use crate::vfs::{
    CopyCancelled, CreateOptions, ProgressFn, RemoveOptions, RenameOptions, Vfs, VolumeKey,
};

/// Serial-per-destination-volume executor for [`FileOp`]s.
pub struct JobQueue {
    vfs: Arc<dyn Vfs>,
    spawner: Arc<dyn Spawner>,
    events_tx: async_channel::Sender<JobEvent>,
    events_rx: async_channel::Receiver<JobEvent>,
    state: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    next_job_id: u64,
    lanes: HashMap<VolumeKey, async_channel::Sender<QueuedJob>>,
    jobs: HashMap<JobId, JobHandle>,
}

struct JobHandle {
    cancelled: Arc<AtomicBool>,
    decision_tx: Option<oneshot::Sender<Resolution>>,
}

struct QueuedJob {
    id: JobId,
    op: FileOp,
}

/// One step of a planned copy tree, in parent-before-child order.
enum PlannedAction {
    EnsureDir {
        src: PathBuf,
        dest: PathBuf,
    },
    CopyFile {
        src: PathBuf,
        dest: PathBuf,
        size: u64,
    },
}

impl JobQueue {
    pub fn new(vfs: Arc<dyn Vfs>, spawner: Arc<dyn Spawner>) -> Arc<Self> {
        let (events_tx, events_rx) = async_channel::unbounded();
        Arc::new(Self {
            vfs,
            spawner,
            events_tx,
            events_rx,
            state: Mutex::new(QueueState::default()),
        })
    }

    /// The queue's event stream. Single-consumer by contract (the JobsModel);
    /// multiple receivers would steal events from each other.
    pub fn subscribe(&self) -> async_channel::Receiver<JobEvent> {
        self.events_rx.clone()
    }

    /// Enqueue `op` on its destination volume's serial lane.
    pub fn submit(self: &Arc<Self>, op: FileOp) -> JobId {
        let lane_key = self.vfs.volume_key(op.lane_path());
        let mut state = self.state.lock().unwrap();
        let id = JobId(state.next_job_id);
        state.next_job_id += 1;
        state.jobs.insert(
            id,
            JobHandle {
                cancelled: Arc::new(AtomicBool::new(false)),
                decision_tx: None,
            },
        );
        let spawner = self.spawner.clone();
        let queue = Arc::downgrade(self);
        let lane = state.lanes.entry(lane_key).or_insert_with(|| {
            let (tx, rx) = async_channel::unbounded::<QueuedJob>();
            spawner.spawn(Box::pin(lane_worker(queue, rx)));
            tx
        });
        let _ = lane.try_send(QueuedJob { id, op }); // unbounded: never full
        id
    }

    /// Un-park a job waiting on [`JobEvent::NeedsDecision`].
    pub fn resolve(&self, id: JobId, resolution: Resolution) {
        let mut state = self.state.lock().unwrap();
        if let Some(handle) = state.jobs.get_mut(&id)
            && let Some(tx) = handle.decision_tx.take()
        {
            let _ = tx.send(resolution);
        }
    }

    /// Cancel a job. The flag is checked between files and between copy
    /// chunks; a job parked on a conflict is woken and cancelled.
    pub fn cancel(&self, id: JobId) {
        let mut state = self.state.lock().unwrap();
        if let Some(handle) = state.jobs.get_mut(&id) {
            handle.cancelled.store(true, Ordering::SeqCst);
            if let Some(tx) = handle.decision_tx.take() {
                let _ = tx.send(Resolution::skip());
            }
        }
    }

    fn emit(&self, event: JobEvent) {
        let _ = self.events_tx.try_send(event);
    }

    fn emit_started(&self, id: JobId, kind: JobKind, total_bytes: u64, total_items: u64) {
        self.emit(JobEvent::Started {
            info: JobInfo {
                id,
                kind,
                total_bytes,
                total_items,
            },
        });
    }

    fn cancel_flag(&self, id: JobId) -> Arc<AtomicBool> {
        self.state
            .lock()
            .unwrap()
            .jobs
            .get(&id)
            .map(|handle| handle.cancelled.clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
    }

    async fn run_job(&self, job: QueuedJob) {
        let QueuedJob { id, op } = job;
        let mut tracker = JobTracker::new(self.events_tx.clone(), id);
        let cancelled = self.cancel_flag(id);
        match self.execute(id, op, &cancelled).await {
            Ok(Some(receipt)) => tracker.completed(receipt),
            Ok(None) => tracker.cancelled(),
            Err(_) if cancelled.load(Ordering::SeqCst) => tracker.cancelled(),
            Err(error) => tracker.failed(error.to_string()),
        }
        self.state.lock().unwrap().jobs.remove(&id);
    }

    /// Run one op to a terminal outcome: `Ok(Some(receipt))` completed,
    /// `Ok(None)` cancelled, `Err` failed.
    async fn execute(
        &self,
        id: JobId,
        op: FileOp,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<Option<OpReceipt>> {
        match op.clone() {
            FileOp::Copy { sources, dest_dir } => {
                let pairs = sources
                    .into_iter()
                    .map(|src| (src, dest_dir.clone()))
                    .collect();
                self.run_copy(id, op, pairs, false, cancelled).await
            }
            FileOp::Duplicate { sources } => {
                let mut pairs = Vec::new();
                for src in sources {
                    let parent = src
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .ok_or_else(|| anyhow!("cannot duplicate a root: {}", src.display()))?
                        .to_path_buf();
                    pairs.push((src, parent));
                }
                self.run_copy(id, op, pairs, true, cancelled).await
            }
            FileOp::Move { sources, dest_dir } => {
                self.run_move(id, op, sources, dest_dir, cancelled).await
            }
            FileOp::Rename { from, to } => {
                self.emit_started(id, JobKind::Rename, 1, 1);
                self.vfs
                    .rename(&from, &to, RenameOptions::default())
                    .await?;
                let mut receipt = OpReceipt::empty(op);
                receipt.moved.push((from, to));
                Ok(Some(receipt))
            }
            FileOp::TrashOp { paths } => {
                let total = paths.len() as u64;
                self.emit_started(id, JobKind::Trash, total, total);
                let mut receipt = OpReceipt::empty(op);
                for (index, path) in paths.iter().enumerate() {
                    if cancelled.load(Ordering::SeqCst) {
                        return Ok(None);
                    }
                    self.emit(JobEvent::Progress {
                        id,
                        done_bytes: index as u64,
                        total_bytes: total,
                        current: path.clone(),
                    });
                    receipt.trashed.push(self.vfs.trash(path).await?);
                }
                Ok(Some(receipt))
            }
            FileOp::Restore { ids } => {
                let total = ids.len() as u64;
                self.emit_started(id, JobKind::Restore, total, total);
                let mut receipt = OpReceipt::empty(op);
                for token in ids {
                    if cancelled.load(Ordering::SeqCst) {
                        return Ok(None);
                    }
                    match self.vfs.restore(token.clone()).await {
                        Ok(path) => receipt.restored.push((token, path)),
                        Err(error) => {
                            bail!("restore {}: {error}", token.original.display())
                        }
                    }
                }
                Ok(Some(receipt))
            }
            FileOp::CreateDir { path } => {
                self.emit_started(id, JobKind::CreateDir, 1, 1);
                // New-folder semantics: never adopt (and make undo-deletable)
                // a directory that already existed.
                if self.vfs.metadata(&path).await?.is_some() {
                    bail!("already exists: {}", path.display());
                }
                self.vfs.create_dir(&path).await?;
                let mut receipt = OpReceipt::empty(op);
                receipt.created.push(path);
                Ok(Some(receipt))
            }
            FileOp::CreateFile { path } => {
                self.emit_started(id, JobKind::CreateFile, 1, 1);
                self.vfs
                    .create_file(&path, CreateOptions::default())
                    .await?;
                let mut receipt = OpReceipt::empty(op);
                receipt.created.push(path);
                Ok(Some(receipt))
            }
            FileOp::Delete { paths } => {
                let total = paths.len() as u64;
                self.emit_started(id, JobKind::Delete, total, total);
                for (index, path) in paths.iter().enumerate() {
                    if cancelled.load(Ordering::SeqCst) {
                        return Ok(None);
                    }
                    self.emit(JobEvent::Progress {
                        id,
                        done_bytes: index as u64,
                        total_bytes: total,
                        current: path.clone(),
                    });
                    self.vfs
                        .remove(path, RemoveOptions { recursive: true })
                        .await?;
                }
                // Permanent removal has no inverse: an empty receipt.
                Ok(Some(OpReceipt::empty(op)))
            }
        }
    }

    /// Copy `pairs` of `(source, dest_dir)`. Keep-both names are resolved at
    /// planning time for paste-into-same-folder sources (and always for
    /// Duplicate via `forced_keep_both`); other collisions park on the
    /// conflict dialog at runtime.
    async fn run_copy(
        &self,
        id: JobId,
        op: FileOp,
        pairs: Vec<(PathBuf, PathBuf)>,
        forced_keep_both: bool,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<Option<OpReceipt>> {
        // --- Plan: top-level names (keep-both resolved HERE, §4b) ---
        let mut taken: HashMap<PathBuf, BTreeSet<String>> = HashMap::new();
        let mut top_levels: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (src, dest_dir) in &pairs {
            // A destination inside (or equal to) its source is always a user
            // mistake: copying would nest the tree into itself.
            if dest_dir.starts_with(src) {
                bail!(
                    "cannot copy {} into itself ({})",
                    src.display(),
                    dest_dir.display()
                );
            }
            if !taken.contains_key(dest_dir) {
                let names = self.read_names(dest_dir).await?;
                taken.insert(dest_dir.clone(), names);
            }
            let names = taken.get_mut(dest_dir).expect("inserted above");
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| !n.is_empty())
                .ok_or_else(|| anyhow!("cannot copy a root: {}", src.display()))?;
            let same_folder = src.parent() == Some(dest_dir.as_path());
            let final_name = if (forced_keep_both || same_folder) && names.contains(&name) {
                keep_both_candidates(&name)
                    .find(|candidate| !names.contains(candidate))
                    .expect("candidate sequence is unbounded")
            } else {
                name
            };
            names.insert(final_name.clone());
            top_levels.push((src.clone(), dest_dir.join(final_name)));
        }

        // --- Plan: expand directory sources into per-file actions ---
        let mut actions: Vec<PlannedAction> = Vec::new();
        let mut total_bytes = 0u64;
        for (src, dest) in &top_levels {
            let (mut tree_actions, tree_bytes) = self.plan_tree(src, dest).await?;
            actions.append(&mut tree_actions);
            total_bytes += tree_bytes;
        }
        self.emit_started(id, op.kind(), total_bytes, actions.len() as u64);

        // --- Execute ---
        let top_srcs: BTreeSet<PathBuf> = top_levels.iter().map(|(src, _)| src.clone()).collect();
        let mut receipt = OpReceipt::empty(op);
        let mut sticky: Option<Resolution> = None;
        let mut done_bytes = 0u64;
        // Dest prefixes the user skipped / keep-both-renamed at runtime.
        let mut skips: Vec<PathBuf> = Vec::new();
        let mut remaps: Vec<(PathBuf, PathBuf)> = Vec::new();

        for action in actions {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(None);
            }
            match action {
                PlannedAction::EnsureDir { src, dest } => {
                    let dest = apply_remaps(&remaps, &dest);
                    if is_skipped(&skips, &dest) {
                        continue;
                    }
                    let mut final_dest = dest.clone();
                    // Only directories this job actually created belong in
                    // the receipt — undoing a copy must never delete a merged
                    // pre-existing destination directory.
                    let mut created_here = true;
                    match self.vfs.metadata(&dest).await? {
                        // An existing directory merges silently (Explorer
                        // folder-merge behavior).
                        Some(meta) if matches!(meta.kind, EntryKind::Dir) => created_here = false,
                        Some(meta) => {
                            let Some(resolution) = self
                                .decide(id, &src, &dest, meta, &mut sticky, cancelled)
                                .await?
                            else {
                                return Ok(None);
                            };
                            match resolution.choice {
                                ConflictChoice::Skip => {
                                    skips.push(dest);
                                    continue;
                                }
                                ConflictChoice::Replace => {
                                    self.vfs
                                        .remove(&dest, RemoveOptions { recursive: true })
                                        .await?;
                                    self.vfs.create_dir(&dest).await?;
                                }
                                ConflictChoice::KeepBoth => {
                                    final_dest = self.free_name(&dest).await?;
                                    remaps.push((dest, final_dest.clone()));
                                    self.vfs.create_dir(&final_dest).await?;
                                }
                            }
                        }
                        None => self.vfs.create_dir(&dest).await?,
                    }
                    if created_here && top_srcs.contains(&src) {
                        receipt.created.push(final_dest);
                    }
                }
                PlannedAction::CopyFile { src, dest, size } => {
                    let dest = apply_remaps(&remaps, &dest);
                    if is_skipped(&skips, &dest) {
                        continue;
                    }
                    let mut target = dest;
                    if let Some(meta) = self.vfs.metadata(&target).await? {
                        let Some(resolution) = self
                            .decide(id, &src, &target, meta, &mut sticky, cancelled)
                            .await?
                        else {
                            return Ok(None);
                        };
                        match resolution.choice {
                            ConflictChoice::Skip => {
                                done_bytes += size;
                                continue;
                            }
                            ConflictChoice::Replace => {
                                self.vfs
                                    .remove(&target, RemoveOptions { recursive: true })
                                    .await?;
                            }
                            ConflictChoice::KeepBoth => {
                                target = self.free_name(&target).await?;
                            }
                        }
                    }
                    let progress = self.progress_fn(id, done_bytes, total_bytes, &src, cancelled);
                    match self.vfs.copy(&src, &target, progress).await {
                        Ok(()) => {
                            done_bytes += size;
                            if top_srcs.contains(&src) {
                                receipt.created.push(target);
                            }
                        }
                        Err(error)
                            if error.is::<CopyCancelled>() || cancelled.load(Ordering::SeqCst) =>
                        {
                            return Ok(None);
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(Some(receipt))
    }

    async fn run_move(
        &self,
        id: JobId,
        op: FileOp,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<Option<OpReceipt>> {
        // Reject destinations inside a source up front: past the rename
        // failure, the copy+remove fallback would copy the tree into itself
        // and then delete source AND destination in one recursive remove.
        for src in &sources {
            if dest_dir.starts_with(src) {
                bail!(
                    "cannot move {} into itself ({})",
                    src.display(),
                    dest_dir.display()
                );
            }
        }
        let total = sources.len() as u64;
        self.emit_started(id, op.kind(), total, total);
        let mut receipt = OpReceipt::empty(op);
        let mut sticky: Option<Resolution> = None;
        for (index, src) in sources.iter().enumerate() {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(None);
            }
            if src.parent() == Some(dest_dir.as_path()) {
                continue; // moving into its own folder is a no-op
            }
            let name = src
                .file_name()
                .ok_or_else(|| anyhow!("cannot move a root: {}", src.display()))?;
            let mut dest = dest_dir.join(name);
            if let Some(meta) = self.vfs.metadata(&dest).await? {
                let Some(resolution) = self
                    .decide(id, src, &dest, meta, &mut sticky, cancelled)
                    .await?
                else {
                    return Ok(None);
                };
                match resolution.choice {
                    ConflictChoice::Skip => continue,
                    ConflictChoice::Replace => {
                        self.vfs
                            .remove(&dest, RemoveOptions { recursive: true })
                            .await?;
                    }
                    ConflictChoice::KeepBoth => dest = self.free_name(&dest).await?,
                }
            }
            self.emit(JobEvent::Progress {
                id,
                done_bytes: index as u64,
                total_bytes: total,
                current: src.clone(),
            });
            if self
                .vfs
                .rename(src, &dest, RenameOptions::default())
                .await
                .is_err()
            {
                // Cross-volume (or otherwise un-renameable): copy + remove.
                if !self.copy_tree_for_move(id, src, &dest, cancelled).await? {
                    // Cancelled mid-fallback: no half-moved destination.
                    let _ = self
                        .vfs
                        .remove(&dest, RemoveOptions { recursive: true })
                        .await;
                    return Ok(None);
                }
                self.vfs
                    .remove(src, RemoveOptions { recursive: true })
                    .await?;
            }
            receipt.moved.push((src.clone(), dest));
        }
        Ok(Some(receipt))
    }

    /// The cross-volume move fallback: copy the tree without conflict prompts
    /// (the destination name was already resolved). Returns `Ok(false)` when
    /// cancelled.
    async fn copy_tree_for_move(
        &self,
        id: JobId,
        src: &Path,
        dest: &Path,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<bool> {
        let (actions, total_bytes) = self.plan_tree(src, dest).await?;
        let mut done_bytes = 0u64;
        for action in actions {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(false);
            }
            match action {
                PlannedAction::EnsureDir { dest, .. } => self.vfs.create_dir(&dest).await?,
                PlannedAction::CopyFile { src, dest, size } => {
                    let progress = self.progress_fn(id, done_bytes, total_bytes, &src, cancelled);
                    match self.vfs.copy(&src, &dest, progress).await {
                        Ok(()) => done_bytes += size,
                        Err(error) if error.is::<CopyCancelled>() => return Ok(false),
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(true)
    }

    /// Expand one source into parent-before-child actions plus total bytes.
    async fn plan_tree(&self, src: &Path, dest: &Path) -> Result<(Vec<PlannedAction>, u64)> {
        let meta = self
            .vfs
            .metadata(src)
            .await?
            .ok_or_else(|| anyhow!("source missing: {}", src.display()))?;
        let mut actions = Vec::new();
        let mut total_bytes = 0u64;
        if matches!(meta.kind, EntryKind::Dir) {
            actions.push(PlannedAction::EnsureDir {
                src: src.to_path_buf(),
                dest: dest.to_path_buf(),
            });
            let mut stack = vec![(src.to_path_buf(), dest.to_path_buf())];
            while let Some((src_dir, dest_dir)) = stack.pop() {
                let mut stream = self.vfs.read_dir(&src_dir).await?;
                let mut children = Vec::new();
                while let Some(entry) = stream.next().await {
                    children.push(entry?);
                }
                children.sort_by(|a, b| a.name.cmp(&b.name)); // deterministic
                for entry in children {
                    let child_dest = dest_dir.join(&*entry.name);
                    if matches!(entry.kind, EntryKind::Dir) {
                        actions.push(PlannedAction::EnsureDir {
                            src: entry.path.to_path_buf(),
                            dest: child_dest.clone(),
                        });
                        stack.push((entry.path.to_path_buf(), child_dest));
                    } else {
                        total_bytes += entry.size;
                        actions.push(PlannedAction::CopyFile {
                            src: entry.path.to_path_buf(),
                            dest: child_dest,
                            size: entry.size,
                        });
                    }
                }
            }
        } else {
            total_bytes = meta.size;
            actions.push(PlannedAction::CopyFile {
                src: src.to_path_buf(),
                dest: dest.to_path_buf(),
                size: meta.size,
            });
        }
        Ok((actions, total_bytes))
    }

    /// Park on a conflict until [`JobQueue::resolve`] (or a sticky
    /// apply-to-all short-circuits). `Ok(None)` means the job was cancelled
    /// while parked.
    async fn decide(
        &self,
        id: JobId,
        src: &Path,
        dest: &Path,
        dest_meta: EntryMeta,
        sticky: &mut Option<Resolution>,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<Option<Resolution>> {
        if let Some(resolution) = *sticky {
            return Ok(Some(resolution));
        }
        let src_meta = self
            .vfs
            .metadata(src)
            .await?
            .ok_or_else(|| anyhow!("conflict source missing: {}", src.display()))?;
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            match state.jobs.get_mut(&id) {
                Some(handle) if !handle.cancelled.load(Ordering::SeqCst) => {
                    handle.decision_tx = Some(tx);
                }
                _ => return Ok(None),
            }
        }
        self.emit(JobEvent::NeedsDecision {
            id,
            conflict: Conflict {
                source: src.to_path_buf(),
                dest: dest.to_path_buf(),
                src_meta,
                dest_meta,
            },
        });
        let resolution = rx.await.unwrap_or_else(|_| Resolution::skip());
        if cancelled.load(Ordering::SeqCst) {
            return Ok(None);
        }
        if resolution.apply_to_all {
            *sticky = Some(resolution);
        }
        Ok(Some(resolution))
    }

    /// First free keep-both destination next to `dest` (runtime conflicts).
    async fn free_name(&self, dest: &Path) -> Result<PathBuf> {
        let parent = dest
            .parent()
            .ok_or_else(|| anyhow!("no parent for {}", dest.display()))?;
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("no file name for {}", dest.display()))?;
        for candidate in keep_both_candidates(&name).take(10_000) {
            let path = parent.join(candidate);
            if self.vfs.metadata(&path).await?.is_none() {
                return Ok(path);
            }
        }
        bail!("no free keep-both name for {}", dest.display());
    }

    /// Names currently present in `dir` (keep-both planning input).
    async fn read_names(&self, dir: &Path) -> Result<BTreeSet<String>> {
        let mut stream = self.vfs.read_dir(dir).await?;
        let mut names = BTreeSet::new();
        while let Some(entry) = stream.next().await {
            names.insert(entry?.name.to_string());
        }
        Ok(names)
    }

    /// Copy progress callback: forwards chunk progress as [`JobEvent::Progress`]
    /// (offset by the bytes of already-finished files) and aborts the copy
    /// between chunks once the job is cancelled.
    fn progress_fn(
        &self,
        id: JobId,
        base_bytes: u64,
        total_bytes: u64,
        current: &Path,
        cancelled: &Arc<AtomicBool>,
    ) -> ProgressFn {
        let tx = self.events_tx.clone();
        let current = current.to_path_buf();
        let cancelled = cancelled.clone();
        Arc::new(move |file_done, _file_total| {
            let _ = tx.try_send(JobEvent::Progress {
                id,
                done_bytes: base_bytes + file_done,
                total_bytes,
                current: current.clone(),
            });
            !cancelled.load(Ordering::SeqCst)
        })
    }
}

async fn lane_worker(queue: Weak<JobQueue>, rx: async_channel::Receiver<QueuedJob>) {
    while let Ok(job) = rx.recv().await {
        let Some(queue) = queue.upgrade() else {
            break;
        };
        queue.run_job(job).await;
    }
}

fn apply_remaps(remaps: &[(PathBuf, PathBuf)], dest: &Path) -> PathBuf {
    for (from, to) in remaps {
        if let Ok(suffix) = dest.strip_prefix(from) {
            return if suffix.as_os_str().is_empty() {
                to.clone()
            } else {
                to.join(suffix)
            };
        }
    }
    dest.to_path_buf()
}

fn is_skipped(skips: &[PathBuf], dest: &Path) -> bool {
    skips.iter().any(|prefix| dest.starts_with(prefix))
}

/// RAII guard: every job sends exactly one terminal event, even if its future
/// panics or is dropped mid-run.
struct JobTracker {
    tx: async_channel::Sender<JobEvent>,
    id: JobId,
    terminal_sent: bool,
}

impl JobTracker {
    fn new(tx: async_channel::Sender<JobEvent>, id: JobId) -> Self {
        Self {
            tx,
            id,
            terminal_sent: false,
        }
    }

    fn completed(&mut self, receipt: OpReceipt) {
        self.terminal_sent = true;
        let _ = self.tx.try_send(JobEvent::Completed {
            id: self.id,
            receipt,
        });
    }

    fn failed(&mut self, error: String) {
        self.terminal_sent = true;
        let _ = self.tx.try_send(JobEvent::Failed { id: self.id, error });
    }

    fn cancelled(&mut self) {
        self.terminal_sent = true;
        let _ = self.tx.try_send(JobEvent::Cancelled { id: self.id });
    }
}

impl Drop for JobTracker {
    fn drop(&mut self) {
        if !self.terminal_sent {
            let _ = self.tx.try_send(JobEvent::Failed {
                id: self.id,
                error: "job ended without a terminal event (worker panicked?)".to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::TestSpawner;
    use crate::vfs::FakeVfs;
    use futures::executor::block_on;
    use serde_json::json;

    fn setup() -> (
        Arc<FakeVfs>,
        Arc<JobQueue>,
        async_channel::Receiver<JobEvent>,
    ) {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        let vfs = FakeVfs::new(spawner.clone());
        let queue = JobQueue::new(vfs.clone() as Arc<dyn Vfs>, spawner);
        let events = queue.subscribe();
        (vfs, queue, events)
    }

    fn recv(events: &async_channel::Receiver<JobEvent>) -> JobEvent {
        block_on(events.recv()).expect("event stream open")
    }

    /// Drain events until `id` reaches a terminal state, returning everything
    /// seen (terminal included) in arrival order.
    fn drain_until_terminal(
        events: &async_channel::Receiver<JobEvent>,
        id: JobId,
    ) -> Vec<JobEvent> {
        let mut seen = Vec::new();
        loop {
            let event = recv(events);
            let terminal = matches!(
                &event,
                JobEvent::Completed { id: i, .. }
                | JobEvent::Failed { id: i, .. }
                | JobEvent::Cancelled { id: i } if *i == id
            );
            seen.push(event);
            if terminal {
                return seen;
            }
        }
    }

    fn receipt_of(events: &[JobEvent], id: JobId) -> OpReceipt {
        events
            .iter()
            .find_map(|event| match event {
                JobEvent::Completed { id: i, receipt } if *i == id => Some(receipt.clone()),
                _ => None,
            })
            .expect("job completed")
    }

    /// Wait for `id` to park on a conflict, returning it.
    fn wait_for_decision(events: &async_channel::Receiver<JobEvent>, id: JobId) -> Conflict {
        loop {
            match recv(events) {
                JobEvent::NeedsDecision { id: i, conflict } if i == id => return conflict,
                JobEvent::Failed { id: i, error } if i == id => {
                    panic!("job failed instead of parking: {error}")
                }
                _ => {}
            }
        }
    }

    fn resolution(choice: ConflictChoice, apply_to_all: bool) -> Resolution {
        Resolution {
            choice,
            apply_to_all,
        }
    }

    fn contents(vfs: &Arc<FakeVfs>, path: &str) -> Vec<u8> {
        block_on(vfs.load(Path::new(path))).unwrap()
    }

    fn exists(vfs: &Arc<FakeVfs>, path: &str) -> bool {
        block_on(vfs.metadata(Path::new(path))).unwrap().is_some()
    }

    #[test]
    fn copy_job_copies_a_tree_and_reports_started_progress_completed() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree(
            "/src",
            json!({ "tree": { "a.txt": "aaa", "sub": { "b.txt": "bb" } } }),
        );
        vfs.insert_tree("/dest", json!({}));

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/tree")],
            dest_dir: PathBuf::from("/dest"),
        });
        let seen = drain_until_terminal(&events, id);

        let started_ix = seen
            .iter()
            .position(|e| matches!(e, JobEvent::Started { info } if info.id == id))
            .expect("Started emitted");
        assert!(matches!(
            &seen[started_ix],
            JobEvent::Started { info } if info.total_bytes == 5 && info.total_items == 4
        ));
        assert!(
            seen.iter()
                .any(|e| matches!(e, JobEvent::Progress { id: i, .. } if *i == id)),
            "per-chunk progress emitted: {seen:?}"
        );
        let receipt = receipt_of(&seen, id);
        assert_eq!(receipt.created, vec![PathBuf::from("/dest/tree")]);
        assert_eq!(contents(&vfs, "/dest/tree/a.txt"), b"aaa");
        assert_eq!(contents(&vfs, "/dest/tree/sub/b.txt"), b"bb");
        assert_eq!(contents(&vfs, "/src/tree/a.txt"), b"aaa", "source intact");
    }

    #[test]
    fn paste_into_same_folder_resolves_keep_both_names_at_planning_time() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/d", json!({ "a.txt": "one" }));

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/d/a.txt")],
            dest_dir: PathBuf::from("/d"),
        });
        let seen = drain_until_terminal(&events, id);
        let receipt = receipt_of(&seen, id);
        assert_eq!(receipt.created, vec![PathBuf::from("/d/a copy.txt")]);
        assert_eq!(contents(&vfs, "/d/a copy.txt"), b"one");
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, JobEvent::NeedsDecision { .. })),
            "same-folder paste never asks: names were planned"
        );

        // Pasting again escalates to "copy 2".
        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/d/a.txt")],
            dest_dir: PathBuf::from("/d"),
        });
        drain_until_terminal(&events, id);
        assert!(exists(&vfs, "/d/a copy 2.txt"));
    }

    #[test]
    fn duplicate_always_keeps_both() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/d", json!({ "report.pdf": "pdf" }));
        let id = queue.submit(FileOp::Duplicate {
            sources: vec![PathBuf::from("/d/report.pdf")],
        });
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(receipt.created, vec![PathBuf::from("/d/report copy.pdf")]);
        assert_eq!(contents(&vfs, "/d/report copy.pdf"), b"pdf");
    }

    #[test]
    fn conflict_parks_and_replace_overwrites() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        let conflict = wait_for_decision(&events, id);
        assert_eq!(conflict.source, PathBuf::from("/src/a.txt"));
        assert_eq!(conflict.dest, PathBuf::from("/dest/a.txt"));
        assert_eq!(conflict.src_meta.size, 3);
        assert_eq!(conflict.dest_meta.size, 3);

        queue.resolve(id, resolution(ConflictChoice::Replace, false));
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"new");
        assert_eq!(receipt.created, vec![PathBuf::from("/dest/a.txt")]);
    }

    #[test]
    fn conflict_skip_keeps_destination_untouched() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        wait_for_decision(&events, id);
        queue.resolve(id, resolution(ConflictChoice::Skip, false));
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"old");
        assert!(receipt.created.is_empty(), "a skip creates nothing");
    }

    #[test]
    fn conflict_keep_both_creates_a_copy_name_at_runtime() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        wait_for_decision(&events, id);
        queue.resolve(id, resolution(ConflictChoice::KeepBoth, false));
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"old");
        assert_eq!(contents(&vfs, "/dest/a copy.txt"), b"new");
        assert_eq!(receipt.created, vec![PathBuf::from("/dest/a copy.txt")]);
    }

    #[test]
    fn apply_to_all_asks_once_for_many_conflicts() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/src", json!({ "a.txt": "new a", "b.txt": "new b" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old a", "b.txt": "old b" }));

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt"), PathBuf::from("/src/b.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        wait_for_decision(&events, id);
        queue.resolve(id, resolution(ConflictChoice::Replace, true));
        let seen = drain_until_terminal(&events, id);
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, JobEvent::NeedsDecision { .. })),
            "apply-to-all suppresses further prompts: {seen:?}"
        );
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"new a");
        assert_eq!(contents(&vfs, "/dest/b.txt"), b"new b");
    }

    #[test]
    fn cancel_while_parked_cancels_cleanly() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        wait_for_decision(&events, id);
        queue.cancel(id);
        let seen = drain_until_terminal(&events, id);
        assert!(
            matches!(seen.last(), Some(JobEvent::Cancelled { id: i }) if *i == id),
            "terminal is Cancelled: {seen:?}"
        );
        assert_eq!(contents(&vfs, "/dest/a.txt"), b"old", "nothing was written");
    }

    #[test]
    fn same_destination_volume_jobs_serialize() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));

        // Job 1 parks on a conflict, holding the "/" lane.
        let first = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        wait_for_decision(&events, first);
        // Job 2 targets the same volume: it must wait behind job 1.
        let second = queue.submit(FileOp::CreateDir {
            path: PathBuf::from("/dest/newdir"),
        });
        queue.resolve(first, resolution(ConflictChoice::Skip, false));
        let seen = drain_until_terminal(&events, second);
        let first_terminal = seen
            .iter()
            .position(|e| {
                matches!(e, JobEvent::Completed { id, .. } | JobEvent::Cancelled { id } | JobEvent::Failed { id, .. } if *id == first)
            })
            .expect("first job finished");
        let second_started = seen
            .iter()
            .position(|e| matches!(e, JobEvent::Started { info } if info.id == second))
            .expect("second job started");
        assert!(
            second_started > first_terminal,
            "serial lane: second started only after first finished: {seen:?}"
        );
    }

    #[test]
    fn different_destination_volumes_run_in_parallel() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/src", json!({ "a.txt": "new" }));
        vfs.insert_tree("/dest", json!({ "a.txt": "old" }));
        vfs.insert_tree("/Volumes/SSD", json!({}));

        // Job 1 parks on a conflict, holding the "/" lane.
        let first = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/src/a.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        wait_for_decision(&events, first);
        // Job 2 targets another volume and completes while job 1 is parked.
        let second = queue.submit(FileOp::CreateDir {
            path: PathBuf::from("/Volumes/SSD/newdir"),
        });
        let seen = drain_until_terminal(&events, second);
        assert!(
            !seen.iter().any(|e| {
                matches!(e, JobEvent::Completed { id, .. } | JobEvent::Cancelled { id } if *id == first)
            }),
            "first job still parked while the other lane ran"
        );
        assert!(exists(&vfs, "/Volumes/SSD/newdir"));
        queue.resolve(first, resolution(ConflictChoice::Skip, false));
        drain_until_terminal(&events, first);
    }

    #[test]
    fn move_renames_within_a_volume_and_conflicts_park() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/a", json!({ "f.txt": "f", "g.txt": "g" }));
        vfs.insert_tree("/b", json!({ "g.txt": "old g" }));

        let id = queue.submit(FileOp::Move {
            sources: vec![PathBuf::from("/a/f.txt"), PathBuf::from("/a/g.txt")],
            dest_dir: PathBuf::from("/b"),
        });
        wait_for_decision(&events, id); // g.txt collides
        queue.resolve(id, resolution(ConflictChoice::Replace, false));
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert!(!exists(&vfs, "/a/f.txt"), "move leaves no source behind");
        assert_eq!(contents(&vfs, "/b/f.txt"), b"f");
        assert_eq!(contents(&vfs, "/b/g.txt"), b"g");
        assert_eq!(
            receipt.moved,
            vec![
                (PathBuf::from("/a/f.txt"), PathBuf::from("/b/f.txt")),
                (PathBuf::from("/a/g.txt"), PathBuf::from("/b/g.txt")),
            ]
        );
    }

    #[test]
    fn move_into_its_own_subtree_fails_without_destroying_data() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/a", json!({ "b": { "keep.txt": "k" }, "f.txt": "f" }));
        let before = vfs.snapshot();

        // rename(/a, /a/b/a) fails on both impls; without a guard the
        // cross-volume fallback would copy /a into /a/b/a and then
        // remove(/a, recursive) — deleting the destination it just wrote.
        let id = queue.submit(FileOp::Move {
            sources: vec![PathBuf::from("/a")],
            dest_dir: PathBuf::from("/a/b"),
        });
        let seen = drain_until_terminal(&events, id);
        assert!(
            matches!(seen.last(), Some(JobEvent::Failed { id: i, .. }) if *i == id),
            "moving a folder into its own subtree must fail: {seen:?}"
        );
        assert_eq!(vfs.snapshot(), before, "nothing was copied or removed");

        // Moving a folder onto itself is the same class of mistake.
        let id = queue.submit(FileOp::Move {
            sources: vec![PathBuf::from("/a")],
            dest_dir: PathBuf::from("/a"),
        });
        let seen = drain_until_terminal(&events, id);
        assert!(
            matches!(seen.last(), Some(JobEvent::Failed { id: i, .. }) if *i == id),
            "{seen:?}"
        );
        assert_eq!(vfs.snapshot(), before);
    }

    #[test]
    fn copy_into_its_own_subtree_fails_cleanly() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/a", json!({ "b": {}, "f.txt": "f" }));
        let before = vfs.snapshot();

        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/a")],
            dest_dir: PathBuf::from("/a/b"),
        });
        let seen = drain_until_terminal(&events, id);
        assert!(
            matches!(seen.last(), Some(JobEvent::Failed { id: i, .. }) if *i == id),
            "copying a folder into its own subtree must fail: {seen:?}"
        );
        assert_eq!(vfs.snapshot(), before, "no recursive self-copy was made");
    }

    #[test]
    fn move_into_its_own_folder_is_a_noop() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/a", json!({ "f.txt": "f" }));
        let before = vfs.snapshot();
        let id = queue.submit(FileOp::Move {
            sources: vec![PathBuf::from("/a/f.txt")],
            dest_dir: PathBuf::from("/a"),
        });
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert!(receipt.moved.is_empty());
        assert_eq!(vfs.snapshot(), before, "nothing changed");
    }

    #[test]
    fn rename_trash_and_restore_jobs_produce_undoable_receipts() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/d", json!({ "old.txt": "x", "doomed.txt": "y" }));

        let id = queue.submit(FileOp::Rename {
            from: PathBuf::from("/d/old.txt"),
            to: PathBuf::from("/d/new.txt"),
        });
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(
            receipt.moved,
            vec![(PathBuf::from("/d/old.txt"), PathBuf::from("/d/new.txt"))]
        );

        let id = queue.submit(FileOp::TrashOp {
            paths: vec![PathBuf::from("/d/doomed.txt")],
        });
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(receipt.trashed.len(), 1);
        assert!(!exists(&vfs, "/d/doomed.txt"));
        let token = receipt.trashed[0].clone();

        let id = queue.submit(FileOp::Restore {
            ids: vec![token.clone()],
        });
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(
            receipt.restored,
            vec![(token, PathBuf::from("/d/doomed.txt"))]
        );
        assert_eq!(contents(&vfs, "/d/doomed.txt"), b"y");
    }

    #[test]
    fn create_dir_and_file_jobs_fail_on_existing_paths() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/d", json!({ "taken": {}, "taken.txt": "x" }));

        let id = queue.submit(FileOp::CreateDir {
            path: PathBuf::from("/d/taken"),
        });
        let seen = drain_until_terminal(&events, id);
        assert!(
            matches!(seen.last(), Some(JobEvent::Failed { .. })),
            "adopting an existing dir would make undo destructive: {seen:?}"
        );

        let id = queue.submit(FileOp::CreateFile {
            path: PathBuf::from("/d/taken.txt"),
        });
        let seen = drain_until_terminal(&events, id);
        assert!(matches!(seen.last(), Some(JobEvent::Failed { .. })));

        let id = queue.submit(FileOp::CreateDir {
            path: PathBuf::from("/d/fresh"),
        });
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert_eq!(receipt.created, vec![PathBuf::from("/d/fresh")]);
        assert!(exists(&vfs, "/d/fresh"));
    }

    #[test]
    fn delete_job_removes_permanently_with_an_uninvertible_receipt() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/d", json!({ "dir": { "a.txt": "a" } }));
        let id = queue.submit(FileOp::Delete {
            paths: vec![PathBuf::from("/d/dir")],
        });
        let receipt = receipt_of(&drain_until_terminal(&events, id), id);
        assert!(!exists(&vfs, "/d/dir"));
        assert!(receipt.created.is_empty());
        assert!(receipt.moved.is_empty());
        assert!(receipt.trashed.is_empty());
    }

    #[test]
    fn failing_source_produces_failed_terminal() {
        let (vfs, queue, events) = setup();
        vfs.insert_tree("/dest", json!({}));
        let id = queue.submit(FileOp::Copy {
            sources: vec![PathBuf::from("/nope/missing.txt")],
            dest_dir: PathBuf::from("/dest"),
        });
        let seen = drain_until_terminal(&events, id);
        assert!(
            matches!(seen.last(), Some(JobEvent::Failed { id: i, .. }) if *i == id),
            "{seen:?}"
        );
    }
}
