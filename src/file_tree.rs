//! Sidebar file browser: a lazy-loading `TreeView` rooted at the active tab's
//! working directory (falling back to `$HOME`). Directories expand on demand;
//! activating a file inserts its shell-quoted path into the active terminal.
//! Ports forge's `ui/file_tree.rs` to anvil's relm4 structure.
//!
//! GTK4 deprecated the TreeView/TreeStore family in 4.10 in favor of the new
//! list/column views, but they remain fully functional and a ColumnView rewrite
//! is out of scope; suppress the deprecation lints module-wide.
#![allow(deprecated)]

use relm4::gtk;

use gtk::glib;
use gtk::prelude::*;
use gtk::{
    CellRendererPixbuf, CellRendererText, TreeIter, TreeModelFilter, TreeRowReference, TreeStore,
    TreeView, TreeViewColumn,
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// TreeStore column indices.
pub(crate) const COL_NAME: u32 = 0;
pub(crate) const COL_PATH: u32 = 1;
pub(crate) const COL_IS_DIR: u32 = 2;
pub(crate) const COL_ICON: u32 = 3;
pub(crate) const COL_TOOLTIP: u32 = 4;
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(16);
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_FILE_NAME_DISPLAY_BYTES: usize = 512;
const MAX_FILE_PATH_DISPLAY_BYTES: usize = 4 * 1024;
const MAX_FILE_PATH_IDENTITY_BYTES: usize = 64 * 1024;
const PATH_IDENTITY_PREFIX: &str = "unix-path-v1:";
const FILE_TREE_RETRY_ACCESSIBLE_LABEL: &str = "Retry directory scan";
const MAX_VISIBLE_DIRECTORY_ERRORS: usize = 8;
const MAX_CONCURRENT_SCANS: usize = 16;
const MAX_PENDING_FS_JOBS: usize = 128;
const MAX_BACKGROUND_FS_JOBS: usize = 4;
const MAX_REMOTE_RUNNING_FS_JOBS: usize = 4;
const MAX_REMOTE_PENDING_FS_JOBS: usize = 32;
pub(crate) const MAX_BULK_REFRESH_DIRS: usize = 64;
pub(crate) const MAX_TTL_REVALIDATE_DIRS: usize = 8;
const DIRECTORY_SNAPSHOT_TTL: Duration = Duration::from_secs(30);
const MAX_FILE_TREE_HISTORY: usize = 50;
const MAX_ROOT_LISTING_CACHE: usize = 8;
const MAX_TYPED_FILE_TREE_PATH_BYTES: usize = 4 * 1024;
const SCHEDULER_WEIGHTS: [FsJobPriority; 7] = [
    FsJobPriority::Interactive,
    FsJobPriority::Interactive,
    FsJobPriority::Interactive,
    FsJobPriority::Interactive,
    FsJobPriority::Normal,
    FsJobPriority::Normal,
    FsJobPriority::Background,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FsJobPriority {
    Interactive,
    Normal,
    Background,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FsAuthorityKey {
    Local,
    Remote(Box<FsRemoteAuthorityKey>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FsRemoteAuthorityKey {
    identity: crate::config::RemoteHost,
    execution: crate::config::RemoteHost,
}

impl FsAuthorityKey {
    pub(crate) fn capture(
        location: &crate::remote_fs::FsLocation,
        hosts: &[crate::config::RemoteHost],
    ) -> io::Result<Self> {
        match location {
            crate::remote_fs::FsLocation::Local => Ok(Self::Local),
            crate::remote_fs::FsLocation::Remote(index) => {
                crate::config::checked_remote_host(hosts, *index)
                    .cloned()
                    .map(|host| {
                        Self::Remote(Box::new(FsRemoteAuthorityKey {
                            identity: host.clone(),
                            execution: host,
                        }))
                    })
                    .map_err(|message| io::Error::new(io::ErrorKind::NotFound, message))
            }
            crate::remote_fs::FsLocation::Transient(endpoint) => {
                crate::config::validate_remote_host(endpoint.identity())
                    .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
                crate::config::validate_remote_host(endpoint.execution())
                    .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
                Ok(Self::Remote(Box::new(FsRemoteAuthorityKey {
                    identity: endpoint.identity().clone(),
                    execution: endpoint.execution().clone(),
                })))
            }
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }
}

struct ScheduledFsJob {
    authority: FsAuthorityKey,
    queued_at: Instant,
    cancellation: Option<ScanCancellation>,
    run: Option<Box<dyn FnOnce(Duration) + Send + 'static>>,
    cancel: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl ScheduledFsJob {
    fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(ScanCancellation::is_cancelled)
    }

    fn run(mut self) {
        if self.is_cancelled() {
            self.cancel();
        } else if let Some(run) = self.run.take() {
            run(self.queued_at.elapsed());
        }
    }

    fn cancel(mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

struct SchedulerQueues {
    interactive: AuthorityQueueLane,
    normal: AuthorityQueueLane,
    background: AuthorityQueueLane,
    cursor: usize,
    capacity: usize,
}

struct AuthorityQueue {
    authority: FsAuthorityKey,
    jobs: VecDeque<ScheduledFsJob>,
}

#[derive(Default)]
struct AuthorityQueueLane {
    authorities: VecDeque<AuthorityQueue>,
}

impl AuthorityQueueLane {
    fn len(&self) -> usize {
        self.authorities.iter().map(|queue| queue.jobs.len()).sum()
    }

    fn authority_len(&self, authority: &FsAuthorityKey) -> usize {
        self.authorities
            .iter()
            .find(|queue| &queue.authority == authority)
            .map_or(0, |queue| queue.jobs.len())
    }

    fn push(&mut self, job: ScheduledFsJob) {
        if let Some(queue) = self
            .authorities
            .iter_mut()
            .find(|queue| queue.authority == job.authority)
        {
            queue.jobs.push_back(job);
        } else {
            self.authorities.push_back(AuthorityQueue {
                authority: job.authority.clone(),
                jobs: VecDeque::from([job]),
            });
        }
    }

    fn pop_allowed(
        &mut self,
        running_authorities: &[(FsAuthorityKey, usize)],
    ) -> Option<ScheduledFsJob> {
        let count = self.authorities.len();
        for _ in 0..count {
            let Some(mut queue) = self.authorities.pop_front() else {
                break;
            };
            let running = running_authorities
                .iter()
                .find(|(authority, _)| authority == &queue.authority)
                .map_or(0, |(_, running)| *running);
            if !queue.authority.is_remote() || running < MAX_REMOTE_RUNNING_FS_JOBS {
                let job = queue.jobs.pop_front();
                if !queue.jobs.is_empty() {
                    self.authorities.push_back(queue);
                }
                if job.is_some() {
                    return job;
                }
            } else {
                self.authorities.push_back(queue);
            }
        }
        None
    }

    fn drain_cancelled(&mut self, cancelled: &mut Vec<ScheduledFsJob>) {
        let authority_count = self.authorities.len();
        for _ in 0..authority_count {
            let Some(mut queue) = self.authorities.pop_front() else {
                break;
            };
            let job_count = queue.jobs.len();
            for _ in 0..job_count {
                let Some(job) = queue.jobs.pop_front() else {
                    break;
                };
                if job.is_cancelled() {
                    cancelled.push(job);
                } else {
                    queue.jobs.push_back(job);
                }
            }
            if !queue.jobs.is_empty() {
                self.authorities.push_back(queue);
            }
        }
    }
}

impl SchedulerQueues {
    fn new(capacity: usize) -> Self {
        Self {
            interactive: AuthorityQueueLane::default(),
            normal: AuthorityQueueLane::default(),
            background: AuthorityQueueLane::default(),
            cursor: 0,
            capacity,
        }
    }

    fn len(&self) -> usize {
        self.interactive.len() + self.normal.len() + self.background.len()
    }

    fn push(&mut self, priority: FsJobPriority, job: ScheduledFsJob) -> Result<(), ScheduledFsJob> {
        if self.len() >= self.capacity {
            return Err(job);
        }
        if job.authority.is_remote() {
            let pending = self.interactive.authority_len(&job.authority)
                + self.normal.authority_len(&job.authority)
                + self.background.authority_len(&job.authority);
            if pending >= MAX_REMOTE_PENDING_FS_JOBS {
                return Err(job);
            }
        }
        self.queue_mut(priority).push(job);
        Ok(())
    }

    fn pop_next(
        &mut self,
        allow_background: bool,
        running_authorities: &[(FsAuthorityKey, usize)],
    ) -> Option<(FsJobPriority, ScheduledFsJob)> {
        for _ in 0..SCHEDULER_WEIGHTS.len() {
            let priority = SCHEDULER_WEIGHTS[self.cursor];
            self.cursor = (self.cursor + 1) % SCHEDULER_WEIGHTS.len();
            if priority == FsJobPriority::Background && !allow_background {
                continue;
            }
            if let Some(job) = self.queue_mut(priority).pop_allowed(running_authorities) {
                return Some((priority, job));
            }
        }
        None
    }

    fn drain_cancelled(&mut self) -> Vec<ScheduledFsJob> {
        let mut cancelled = Vec::new();
        self.interactive.drain_cancelled(&mut cancelled);
        self.normal.drain_cancelled(&mut cancelled);
        self.background.drain_cancelled(&mut cancelled);
        cancelled
    }

    fn queue_mut(&mut self, priority: FsJobPriority) -> &mut AuthorityQueueLane {
        match priority {
            FsJobPriority::Interactive => &mut self.interactive,
            FsJobPriority::Normal => &mut self.normal,
            FsJobPriority::Background => &mut self.background,
        }
    }
}

struct SchedulerState {
    queues: SchedulerQueues,
    running_background: usize,
    running_authorities: Vec<(FsAuthorityKey, usize)>,
}

struct FsSchedulerInner {
    state: Mutex<SchedulerState>,
    wake: Condvar,
}

struct FsScheduler {
    inner: Arc<FsSchedulerInner>,
}

impl FsScheduler {
    fn new() -> io::Result<Self> {
        let inner = Arc::new(FsSchedulerInner {
            state: Mutex::new(SchedulerState {
                queues: SchedulerQueues::new(MAX_PENDING_FS_JOBS),
                running_background: 0,
                running_authorities: Vec::new(),
            }),
            wake: Condvar::new(),
        });
        let mut started = 0usize;
        for index in 0..MAX_CONCURRENT_SCANS {
            let worker = inner.clone();
            match std::thread::Builder::new()
                .name(format!("anvil-file-tree-worker-{index}"))
                .spawn(move || fs_scheduler_worker(worker))
            {
                Ok(_) => started += 1,
                Err(error) => log::error!("failed to start file-tree worker {index}: {error}"),
            }
        }
        if started == 0 {
            return Err(io::Error::other("could not start any file-tree workers"));
        }
        Ok(Self { inner })
    }

    fn global() -> io::Result<&'static Self> {
        static SCHEDULER: OnceLock<Result<FsScheduler, String>> = OnceLock::new();
        match SCHEDULER.get_or_init(|| Self::new().map_err(|error| error.to_string())) {
            Ok(scheduler) => Ok(scheduler),
            Err(message) => Err(io::Error::other(message.clone())),
        }
    }

    fn submit(&self, priority: FsJobPriority, job: ScheduledFsJob) -> io::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cancelled = state.queues.drain_cancelled();
        let pushed = state.queues.push(priority, job).is_ok();
        drop(state);
        for job in cancelled {
            job.cancel();
        }
        if !pushed {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file-tree scheduler capacity reached; retry shortly",
            ));
        }
        self.inner.wake.notify_one();
        Ok(())
    }
}

fn fs_scheduler_worker(inner: Arc<FsSchedulerInner>) {
    loop {
        let (cancelled, work) = {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                let cancelled = state.queues.drain_cancelled();
                let allow_background = state.running_background < MAX_BACKGROUND_FS_JOBS;
                let running_authorities = state.running_authorities.clone();
                if let Some((priority, job)) = state
                    .queues
                    .pop_next(allow_background, &running_authorities)
                {
                    if priority == FsJobPriority::Background {
                        state.running_background += 1;
                    }
                    if let Some((_, running)) = state
                        .running_authorities
                        .iter_mut()
                        .find(|(authority, _)| authority == &job.authority)
                    {
                        *running += 1;
                    } else {
                        state.running_authorities.push((job.authority.clone(), 1));
                    }
                    break (cancelled, Some((priority, job)));
                }
                if !cancelled.is_empty() {
                    break (cancelled, None);
                }
                state = inner
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        for job in cancelled {
            job.cancel();
        }
        let Some((priority, job)) = work else {
            continue;
        };
        let authority = job.authority.clone();
        job.run();
        {
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(index) = state
                .running_authorities
                .iter()
                .position(|(running_authority, _)| running_authority == &authority)
            {
                state.running_authorities[index].1 =
                    state.running_authorities[index].1.saturating_sub(1);
                if state.running_authorities[index].1 == 0 {
                    state.running_authorities.remove(index);
                }
            }
            if priority == FsJobPriority::Background {
                state.running_background = state.running_background.saturating_sub(1);
            }
        }
        inner.wake.notify_all();
    }
}

/// Cooperative cancellation shared by the GTK request registry, a queued
/// scheduler job, and the remote capture watchdog.
#[derive(Clone, Default)]
pub(crate) struct ScanCancellation(Arc<AtomicBool>);

impl ScanCancellation {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

fn cancelled_scan_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "directory scan was superseded")
}

#[derive(Clone, Debug)]
pub(crate) struct FileEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

impl FileEntry {
    /// `remote_fs` builds entries from probe output; the fields stay private
    /// so every entry is constructed through one place.
    pub(crate) fn new(name: String, path: PathBuf, is_dir: bool) -> Self {
        Self { name, path, is_dir }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[allow(dead_code)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[allow(dead_code)]
    pub(crate) fn is_dir(&self) -> bool {
        self.is_dir
    }
}

/// One bounded directory snapshot. `truncated` is explicit instead of silently
/// presenting the retained prefix as the complete remote directory.
#[derive(Clone, Debug)]
pub(crate) struct DirectoryListing {
    entries: Vec<FileEntry>,
    truncated: bool,
    completed_at: Instant,
}

impl DirectoryListing {
    pub(crate) fn new(entries: Vec<FileEntry>, truncated: bool) -> Self {
        Self {
            entries,
            truncated,
            completed_at: Instant::now(),
        }
    }

    pub(crate) fn into_parts(self) -> (Vec<FileEntry>, bool) {
        (self.entries, self.truncated)
    }

    pub(crate) fn completed_at(&self) -> Instant {
        self.completed_at
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Clone, Copy, Debug)]
struct DirectorySnapshot {
    completed_at: Instant,
    stale: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotFreshness {
    Missing,
    Fresh,
    Stale,
}

#[derive(Default)]
pub(crate) struct DirectorySnapshots {
    by_path: std::collections::HashMap<PathBuf, DirectorySnapshot>,
}

impl DirectorySnapshots {
    pub(crate) fn record_success(&mut self, path: PathBuf, completed_at: Instant) {
        self.by_path.insert(
            path,
            DirectorySnapshot {
                completed_at,
                stale: false,
            },
        );
    }

    pub(crate) fn mark_stale<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        for path in paths {
            if let Some(snapshot) = self.by_path.get_mut(path) {
                snapshot.stale = true;
            }
        }
    }

    pub(crate) fn freshness_at(&self, path: &Path, now: Instant) -> SnapshotFreshness {
        let Some(snapshot) = self.by_path.get(path) else {
            return SnapshotFreshness::Missing;
        };
        if snapshot.stale
            || now.saturating_duration_since(snapshot.completed_at) >= DIRECTORY_SNAPSHOT_TTL
        {
            SnapshotFreshness::Stale
        } else {
            SnapshotFreshness::Fresh
        }
    }

    pub(crate) fn needs_refresh(&self, path: &Path) -> bool {
        self.freshness_at(path, Instant::now()) == SnapshotFreshness::Stale
    }

    pub(crate) fn reset(&mut self) {
        self.by_path.clear();
    }

    pub(crate) fn due_paths_at(
        &self,
        paths: impl IntoIterator<Item = PathBuf>,
        now: Instant,
        limit: usize,
    ) -> Vec<PathBuf> {
        paths
            .into_iter()
            .filter(|path| self.freshness_at(path, now) == SnapshotFreshness::Stale)
            .take(limit)
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FileTreeHistoryEntry {
    pub(crate) location: crate::remote_fs::FsLocation,
    pub(crate) hosts: Vec<crate::config::RemoteHost>,
    pub(crate) root: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NavigationHistoryAction {
    Push,
    MoveTo(usize),
}

#[derive(Default)]
pub(crate) struct FileTreeNavigationHistory {
    authorities: Vec<AuthorityNavigationHistory>,
}

struct AuthorityNavigationHistory {
    authority: FsAuthorityKey,
    entries: Vec<FileTreeHistoryEntry>,
    cursor: Option<usize>,
}

impl FileTreeNavigationHistory {
    pub(crate) fn back(&self, authority: &FsAuthorityKey) -> Option<(usize, FileTreeHistoryEntry)> {
        let history = self
            .authorities
            .iter()
            .find(|history| &history.authority == authority)?;
        let index = history.cursor?.checked_sub(1)?;
        Some((index, history.entries.get(index)?.clone()))
    }

    pub(crate) fn forward(
        &self,
        authority: &FsAuthorityKey,
    ) -> Option<(usize, FileTreeHistoryEntry)> {
        let history = self
            .authorities
            .iter()
            .find(|history| &history.authority == authority)?;
        let index = history.cursor?.checked_add(1)?;
        Some((index, history.entries.get(index)?.clone()))
    }

    pub(crate) fn retry_action(
        &self,
        authority: &FsAuthorityKey,
        root: &Path,
    ) -> NavigationHistoryAction {
        self.back(authority)
            .filter(|(_, entry)| entry.root == root)
            .or_else(|| {
                self.forward(authority)
                    .filter(|(_, entry)| entry.root == root)
            })
            .map_or(NavigationHistoryAction::Push, |(index, _)| {
                NavigationHistoryAction::MoveTo(index)
            })
    }

    pub(crate) fn commit(&mut self, action: NavigationHistoryAction, entry: FileTreeHistoryEntry) {
        let Ok(authority) = FsAuthorityKey::capture(&entry.location, &entry.hosts) else {
            return;
        };
        let history = if let Some(index) = self
            .authorities
            .iter()
            .position(|history| history.authority == authority)
        {
            &mut self.authorities[index]
        } else {
            self.authorities.push(AuthorityNavigationHistory {
                authority,
                entries: Vec::new(),
                cursor: None,
            });
            self.authorities.last_mut().expect("history was inserted")
        };
        match action {
            NavigationHistoryAction::MoveTo(index) => {
                if history
                    .entries
                    .get(index)
                    .is_some_and(|candidate| candidate.root == entry.root)
                {
                    history.cursor = Some(index);
                }
            }
            NavigationHistoryAction::Push => {
                if history
                    .cursor
                    .and_then(|cursor| history.entries.get(cursor))
                    == Some(&entry)
                {
                    return;
                }
                let keep = history.cursor.map_or(0, |cursor| cursor.saturating_add(1));
                history.entries.truncate(keep);
                history.entries.push(entry);
                if history.entries.len() > MAX_FILE_TREE_HISTORY {
                    let overflow = history.entries.len() - MAX_FILE_TREE_HISTORY;
                    history.entries.drain(..overflow);
                }
                history.cursor = history.entries.len().checked_sub(1);
            }
        }
    }

    #[cfg(test)]
    fn len(&self, authority: &FsAuthorityKey) -> usize {
        self.authorities
            .iter()
            .find(|history| &history.authority == authority)
            .map_or(0, |history| history.entries.len())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTreeNavigation {
    pub(crate) token: u64,
    pub(crate) location: crate::remote_fs::FsLocation,
    pub(crate) hosts: Vec<crate::config::RemoteHost>,
    pub(crate) root: PathBuf,
    pub(crate) history: NavigationHistoryAction,
    pub(crate) status_request: u64,
    pub(crate) cached: Option<DirectoryListing>,
}

#[derive(Clone)]
struct CachedRootListing {
    authority: FsAuthorityKey,
    root: PathBuf,
    listing: DirectoryListing,
}

#[derive(Default)]
pub(crate) struct RootListingCache {
    entries: VecDeque<CachedRootListing>,
}

impl RootListingCache {
    pub(crate) fn insert(
        &mut self,
        authority: FsAuthorityKey,
        root: PathBuf,
        listing: DirectoryListing,
    ) {
        self.entries
            .retain(|entry| entry.authority != authority || entry.root != root);
        self.entries.push_back(CachedRootListing {
            authority,
            root,
            listing,
        });
        while self.entries.len() > MAX_ROOT_LISTING_CACHE {
            self.entries.pop_front();
        }
    }

    pub(crate) fn get(
        &mut self,
        authority: &FsAuthorityKey,
        root: &Path,
    ) -> Option<DirectoryListing> {
        let index = self
            .entries
            .iter()
            .position(|entry| &entry.authority == authority && entry.root == root)?;
        let entry = self.entries.remove(index)?;
        let listing = entry.listing.clone();
        self.entries.push_back(entry);
        Some(listing)
    }

    pub(crate) fn invalidate<'a>(
        &mut self,
        authority: &FsAuthorityKey,
        paths: impl IntoIterator<Item = &'a PathBuf>,
    ) {
        let paths: std::collections::HashSet<PathBuf> = paths.into_iter().cloned().collect();
        self.entries
            .retain(|entry| &entry.authority != authority || !paths.contains(&entry.root));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Clone, Debug)]
struct DirectoryFailureRecord {
    authority: FsAuthorityKey,
    path: PathBuf,
    count: u32,
    retry_at: Instant,
}

#[derive(Default)]
pub(crate) struct DirectoryFailureGate {
    records: Vec<DirectoryFailureRecord>,
}

impl DirectoryFailureGate {
    pub(crate) fn allows_auto_at(
        &self,
        authority: &FsAuthorityKey,
        path: &Path,
        now: Instant,
    ) -> bool {
        self.records
            .iter()
            .find(|record| &record.authority == authority && record.path == path)
            .is_none_or(|record| now >= record.retry_at)
    }

    pub(crate) fn record_success(&mut self, authority: &FsAuthorityKey, path: &Path) {
        self.records
            .retain(|record| &record.authority != authority || record.path != path);
    }

    pub(crate) fn record_failure_at(
        &mut self,
        authority: FsAuthorityKey,
        path: PathBuf,
        kind: crate::remote_fs::FsFailureKind,
        now: Instant,
    ) -> Duration {
        if kind == crate::remote_fs::FsFailureKind::Superseded {
            return Duration::ZERO;
        }
        let existing = self
            .records
            .iter_mut()
            .find(|record| record.authority == authority && record.path == path);
        let count = existing
            .as_ref()
            .map_or(1, |record| record.count.saturating_add(1));
        let delay = failure_retry_delay(kind, count);
        let record = DirectoryFailureRecord {
            authority,
            path,
            count,
            retry_at: now + delay,
        };
        if let Some(existing) = existing {
            *existing = record;
        } else {
            self.records.push(record);
        }
        delay
    }
}

fn failure_retry_delay(kind: crate::remote_fs::FsFailureKind, count: u32) -> Duration {
    use crate::remote_fs::FsFailureKind;
    match kind {
        FsFailureKind::Connection | FsFailureKind::TimedOut | FsFailureKind::QueueFull => {
            Duration::from_secs(
                1u64.checked_shl(count.saturating_sub(1).min(5))
                    .unwrap_or(32)
                    .min(30),
            )
        }
        FsFailureKind::Missing => Duration::from_secs(2),
        FsFailureKind::Permission
        | FsFailureKind::InvalidRequest
        | FsFailureKind::InvalidResponse => Duration::from_secs(30),
        FsFailureKind::Exists | FsFailureKind::Other => Duration::from_secs(4),
        FsFailureKind::Superseded => Duration::ZERO,
    }
}

pub(crate) fn validate_typed_file_tree_path(text: &str) -> Result<PathBuf, &'static str> {
    if text.is_empty() || text.len() > MAX_TYPED_FILE_TREE_PATH_BYTES {
        return Err("Enter an absolute path no longer than 4096 bytes.");
    }
    if crate::review_input::contains_visual_spoofing(text) || text.chars().any(char::is_control) {
        return Err("The path contains hidden, bidirectional, or control characters.");
    }
    if !text.starts_with('/') {
        return Err("Enter an absolute path beginning with '/'.");
    }
    if text
        .split('/')
        .any(|component| component == "." || component == "..")
    {
        return Err("Use a normalized absolute path without '.' or '..' components.");
    }
    Ok(PathBuf::from(text))
}

pub(crate) fn pending_navigation_is_current(
    token: u64,
    current_token: u64,
    expected_authority: &FsAuthorityKey,
    current_authority: &FsAuthorityKey,
) -> bool {
    token == current_token && expected_authority == current_authority
}

pub(crate) fn home_navigation_is_current(
    token: u64,
    current_token: u64,
    intent: &FileTreeIntent,
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> bool {
    token == current_token && file_tree_intent_is_current(intent, generation, location, hosts)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DirectoryScanTarget {
    Root(PathBuf),
    Expand(PathBuf),
    Refresh(PathBuf),
}

impl DirectoryScanTarget {
    fn path(&self) -> &Path {
        match self {
            Self::Root(path) | Self::Expand(path) | Self::Refresh(path) => path,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectoryScanPhase {
    Loading,
    Refreshing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryScanRunState {
    Queued,
    Running,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum VisibleDirectoryStatus {
    InFlight {
        request: u64,
        target: DirectoryScanTarget,
        phase: DirectoryScanPhase,
        run_state: DirectoryScanRunState,
        queue_wait: Option<Duration>,
        running_at: Option<Instant>,
    },
    Error {
        request: u64,
        target: DirectoryScanTarget,
        phase: DirectoryScanPhase,
        message: String,
    },
}

#[derive(Default)]
pub(crate) struct DirectoryStatusTracker {
    next: u64,
    statuses: Vec<VisibleDirectoryStatus>,
}

impl DirectoryStatusTracker {
    fn begin(&mut self, target: DirectoryScanTarget, phase: DirectoryScanPhase) -> u64 {
        self.next = self.next.wrapping_add(1);
        let request = self.next;
        // A retry or newer same-target request replaces that target's old
        // status. Unrelated errors stay queued and visible until the user has
        // had a chance to retry each failed directory.
        self.statuses.retain(|status| status.target() != &target);
        self.statuses.push(VisibleDirectoryStatus::InFlight {
            request,
            target,
            phase,
            run_state: DirectoryScanRunState::Queued,
            queue_wait: None,
            running_at: None,
        });
        request
    }

    fn mark_running(&mut self, request: u64, queue_wait: Duration) -> bool {
        let Some(VisibleDirectoryStatus::InFlight {
            run_state,
            queue_wait: current_wait,
            running_at,
            ..
        }) = self.statuses.iter_mut().find(|status| {
            matches!(
                status,
                VisibleDirectoryStatus::InFlight {
                    request: current,
                    ..
                } if *current == request
            )
        })
        else {
            return false;
        };
        *run_state = DirectoryScanRunState::Running;
        *current_wait = Some(queue_wait);
        *running_at = Some(Instant::now());
        true
    }

    fn finish_success(&mut self, request: u64) -> bool {
        let Some(index) = self.statuses.iter().position(|status| {
            matches!(
                status,
                VisibleDirectoryStatus::InFlight {
                    request: current,
                    ..
                } if *current == request
            )
        }) else {
            return false;
        };
        self.statuses.remove(index);
        true
    }

    fn finish_error(&mut self, request: u64, message: String) -> bool {
        let Some(status) = self.statuses.iter_mut().find(|status| {
            matches!(
                status,
                VisibleDirectoryStatus::InFlight {
                    request: current,
                    ..
                } if *current == request
            )
        }) else {
            return false;
        };
        let VisibleDirectoryStatus::InFlight { target, phase, .. } = status else {
            unreachable!("the lookup above accepts only an in-flight status")
        };
        *status = VisibleDirectoryStatus::Error {
            request,
            target: target.clone(),
            phase: *phase,
            message,
        };
        // Keep active Queued/Running rows until their terminal callback, but
        // bound completed failures so a long offline traversal cannot grow
        // the status registry forever. The newest failures remain retryable.
        while self
            .statuses
            .iter()
            .filter(|status| matches!(status, VisibleDirectoryStatus::Error { .. }))
            .count()
            > MAX_VISIBLE_DIRECTORY_ERRORS
        {
            let Some(oldest) = self
                .statuses
                .iter()
                .enumerate()
                .filter(|(_, status)| matches!(status, VisibleDirectoryStatus::Error { .. }))
                .min_by_key(|(_, status)| status.request())
                .map(|(index, _)| index)
            else {
                break;
            };
            self.statuses.remove(oldest);
        }
        true
    }

    /// A finished error takes precedence over progress so it cannot disappear
    /// merely because another directory is still scanning. Within each class,
    /// the newest request is shown first.
    fn visible(&self) -> Option<&VisibleDirectoryStatus> {
        self.statuses
            .iter()
            .filter(|status| matches!(status, VisibleDirectoryStatus::Error { .. }))
            .max_by_key(|status| status.request())
            .or_else(|| self.statuses.iter().max_by_key(|status| status.request()))
    }

    fn retry_target(&self) -> Option<DirectoryScanTarget> {
        match self.visible() {
            Some(VisibleDirectoryStatus::Error { target, .. }) => Some(target.clone()),
            _ => None,
        }
    }

    fn dismiss_target(&mut self, target: &DirectoryScanTarget) {
        self.statuses.retain(|status| status.target() != target);
    }

    fn reset(&mut self) {
        self.statuses.clear();
    }
}

impl VisibleDirectoryStatus {
    fn request(&self) -> u64 {
        match self {
            Self::InFlight { request, .. } | Self::Error { request, .. } => *request,
        }
    }

    fn target(&self) -> &DirectoryScanTarget {
        match self {
            Self::InFlight { target, .. } | Self::Error { target, .. } => target,
        }
    }
}

/// Visible status immediately above the tree. GTK4's legacy `TreeView` cannot
/// safely host a real focusable button in a cell, so Retry lives in the same
/// Files tree region while last-good rows remain untouched underneath it.
pub(crate) struct FileTreeStatusUi {
    bar: gtk::Box,
    label: gtk::Label,
    retry: gtk::Button,
    tracker: RefCell<DirectoryStatusTracker>,
    stale_count: Cell<usize>,
}

impl FileTreeStatusUi {
    pub(crate) fn new(on_retry: impl Fn(DirectoryScanTarget) + 'static) -> Rc<Self> {
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_margin_start(6);
        bar.set_margin_end(6);
        bar.set_margin_top(4);
        bar.set_margin_bottom(4);
        bar.set_visible(false);

        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let retry = gtk::Button::with_label("Retry");
        retry.set_widget_name("file-tree-retry");
        retry.set_focusable(true);
        retry.set_tooltip_text(Some("Retry this directory scan"));
        retry.update_property(&[gtk::accessible::Property::Label(
            FILE_TREE_RETRY_ACCESSIBLE_LABEL,
        )]);
        retry.set_visible(false);
        bar.append(&label);
        bar.append(&retry);

        let ui = Rc::new(Self {
            bar,
            label,
            retry,
            tracker: RefCell::new(DirectoryStatusTracker::default()),
            stale_count: Cell::new(0),
        });
        let weak = Rc::downgrade(&ui);
        ui.retry.connect_clicked(move |_| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let target = ui.tracker.borrow().retry_target();
            if let Some(target) = target {
                on_retry(target);
            }
        });
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if ui.bar.is_visible() {
                ui.sync_visible_status();
            }
            glib::ControlFlow::Continue
        });
        ui
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.bar
    }

    pub(crate) fn begin(&self, target: DirectoryScanTarget, phase: DirectoryScanPhase) -> u64 {
        let request = self.tracker.borrow_mut().begin(target, phase);
        self.sync_visible_status();
        request
    }

    pub(crate) fn finish_success(&self, request: u64) {
        if self.tracker.borrow_mut().finish_success(request) {
            self.sync_visible_status();
        }
    }

    pub(crate) fn mark_running(&self, request: u64, queue_wait: Duration) {
        if self.tracker.borrow_mut().mark_running(request, queue_wait) {
            self.sync_visible_status();
        }
    }

    pub(crate) fn finish_error(&self, request: u64, error: &io::Error) {
        let message = crate::remote_fs::user_facing_fs_error(error).to_string();
        self.finish_error_message(request, message);
    }

    pub(crate) fn finish_error_kind(&self, request: u64, kind: crate::remote_fs::FsFailureKind) {
        self.finish_error_message(
            request,
            crate::remote_fs::user_facing_failure_kind(kind).to_string(),
        );
    }

    fn finish_error_message(&self, request: u64, message: String) {
        if !self
            .tracker
            .borrow_mut()
            .finish_error(request, message.clone())
        {
            return;
        }
        self.sync_visible_status();
    }

    pub(crate) fn dismiss_target(&self, target: &DirectoryScanTarget) {
        self.tracker.borrow_mut().dismiss_target(target);
        self.sync_visible_status();
    }

    pub(crate) fn reset(&self) {
        self.tracker.borrow_mut().reset();
        self.stale_count.set(0);
        self.sync_visible_status();
    }

    pub(crate) fn set_stale_count(&self, count: usize) {
        self.stale_count.set(count);
        self.sync_visible_status();
    }

    fn sync_visible_status(&self) {
        let visible = self.tracker.borrow().visible().cloned();
        match visible {
            Some(VisibleDirectoryStatus::InFlight {
                target,
                phase,
                run_state,
                queue_wait,
                running_at,
                ..
            }) => {
                let path = display_full_path(target.path());
                let mut message = match (phase, run_state) {
                    (DirectoryScanPhase::Loading, DirectoryScanRunState::Queued) => {
                        format!("Queued to load {path}…")
                    }
                    (DirectoryScanPhase::Refreshing, DirectoryScanRunState::Queued) => {
                        format!("Queued to refresh {path}…")
                    }
                    (DirectoryScanPhase::Loading, DirectoryScanRunState::Running) => {
                        format!("Loading {path}…")
                    }
                    (DirectoryScanPhase::Refreshing, DirectoryScanRunState::Running) => {
                        format!("Refreshing {path}…")
                    }
                };
                if let Some(queue_wait) = queue_wait.filter(|wait| !wait.is_zero()) {
                    use std::fmt::Write as _;
                    let _ = write!(message, " queued {} ms", queue_wait.as_millis());
                }
                if let Some(running_at) = running_at {
                    let elapsed = running_at.elapsed();
                    if elapsed >= Duration::from_secs(1) {
                        use std::fmt::Write as _;
                        let _ = write!(message, " · running {} s", elapsed.as_secs());
                    }
                }
                self.bar.remove_css_class("error");
                self.label.set_label(&message);
                self.retry.set_visible(false);
                self.bar.set_visible(true);
            }
            Some(VisibleDirectoryStatus::Error {
                target,
                phase,
                message,
                ..
            }) => {
                let path = display_full_path(target.path());
                let action = match phase {
                    DirectoryScanPhase::Loading => "loading",
                    DirectoryScanPhase::Refreshing => "refreshing",
                };
                self.bar.add_css_class("error");
                self.label
                    .set_label(&format!("Error {action} {path}: {message}"));
                self.retry.set_visible(true);
                self.bar.set_visible(true);
            }
            None => {
                self.bar.remove_css_class("error");
                self.retry.set_visible(false);
                let stale = self.stale_count.get();
                if stale == 0 {
                    self.bar.set_visible(false);
                } else {
                    self.label.set_label(&format!(
                        "{stale} loaded director{} out of date; revalidation is queued",
                        if stale == 1 { "y is" } else { "ies are" }
                    ));
                    self.bar.set_visible(true);
                }
            }
        }
    }
}

/// Directories first, then case-insensitive name order — the one comparator
/// behind scans, inserts, and merge refreshes.
fn entry_cmp(a: &FileEntry, b: &FileEntry) -> std::cmp::Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

pub(crate) fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(entry_cmp);
}

pub(crate) fn scan_dir(dir: &Path) -> io::Result<DirectoryListing> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        entries.push(FileEntry {
            name: display_os_str(&entry.file_name()),
            // Do not follow directory symlinks: they can create cycles or turn
            // one expansion into a scan outside the tree the user selected.
            is_dir: file_type.is_dir(),
            path,
        });
        if entries.len() > MAX_DIRECTORY_ENTRIES {
            break;
        }
    }
    let truncated = entries.len() > MAX_DIRECTORY_ENTRIES;
    entries.truncate(MAX_DIRECTORY_ENTRIES);
    sort_entries(&mut entries);
    Ok(DirectoryListing::new(entries, truncated))
}

/// Encode a Linux path for storage in GTK's string-only tree model without
/// ever treating its bytes as UTF-8. The versioned hex form is reversible and
/// explicitly bounded before its 2x expansion.
pub(crate) fn encode_path_identity(path: &Path) -> Option<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > MAX_FILE_PATH_IDENTITY_BYTES {
        return None;
    }
    let encoded_len = PATH_IDENTITY_PREFIX
        .len()
        .checked_add(bytes.len().checked_mul(2)?)?;
    let mut encoded = String::with_capacity(encoded_len);
    encoded.push_str(PATH_IDENTITY_PREFIX);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Some(encoded)
}

/// Recover the exact Linux path bytes stored by [`encode_path_identity`].
/// Malformed, unversioned, or oversized model values are rejected.
pub(crate) fn decode_path_identity(encoded: &str) -> Option<PathBuf> {
    let hex = encoded.strip_prefix(PATH_IDENTITY_PREFIX)?;
    if hex.len() % 2 != 0 || hex.len() / 2 > MAX_FILE_PATH_IDENTITY_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        bytes.push((high << 4) | low);
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Render valid UTF-8 normally and make every invalid byte visible. Escaping
/// literal backslashes keeps a real `\\xff` name distinct from a raw `0xff`.
pub(crate) fn display_os_str(value: &OsStr) -> String {
    let mut remaining = value.as_bytes();
    let mut display = String::with_capacity(remaining.len());
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                push_valid_display(&mut display, valid);
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                let valid = std::str::from_utf8(&remaining[..valid_len])
                    .expect("Utf8Error::valid_up_to must end on a UTF-8 boundary");
                push_valid_display(&mut display, valid);
                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| remaining.len().saturating_sub(valid_len));
                for &byte in &remaining[valid_len..valid_len + invalid_len] {
                    use std::fmt::Write as _;
                    let _ = write!(display, "\\x{byte:02x}");
                }
                remaining = &remaining[valid_len + invalid_len..];
            }
        }
    }
    display
}

fn push_valid_display(display: &mut String, valid: &str) {
    for ch in valid.chars() {
        if ch == '\\' {
            display.push_str("\\\\");
        } else {
            display.push(ch);
        }
    }
}

pub(crate) fn is_notebook_path(path: &Path) -> bool {
    path.as_os_str().as_bytes().ends_with(b".jtnb.md")
}

/// The launch authority behind the file-tree header's terminal button.
/// Local trees carry their exact root as the new pane cwd; remote trees carry
/// only a freshly validated managed profile. A remote tree path is
/// intentionally absent because ssh/docker startup decides its own directory.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FileTreeTerminalTarget {
    Local(String),
    Remote(crate::config::RemoteHost),
    /// A process-observed unsaved destination opens a normal interactive SSH
    /// login; it must not silently turn the target into an Anvil/jsh profile.
    TemporarySsh(crate::config::RemoteHost),
}

/// Frozen launch authority chosen for a process-observed SSH destination.
/// Managed profiles are re-resolved by their complete value when the probe
/// returns; a transient profile is already its own immutable authority.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObservedRemoteAuthority {
    Managed {
        /// Exact saved profile used only for config revalidation/launch UI.
        source: crate::config::RemoteHost,
        /// Stable identity with every explicit ControlPath removed.
        identity: crate::config::RemoteHost,
    },
    Transient(crate::config::RemoteHost),
}

impl ObservedRemoteAuthority {
    pub(crate) fn profile(&self) -> &crate::config::RemoteHost {
        match self {
            Self::Managed { identity, .. } | Self::Transient(identity) => identity,
        }
    }

    pub(crate) fn session_location(
        &self,
        execution_overlay: &[String],
    ) -> Result<crate::remote_fs::FsLocation, &'static str> {
        let (identity, managed_profile) = match self {
            Self::Managed { source, identity } => (identity.clone(), Some(source.clone())),
            Self::Transient(identity) => (identity.clone(), None),
        };
        let mut effective_overlay = execution_overlay.to_vec();
        if effective_overlay.is_empty() {
            if let Some(source) = &managed_profile {
                // A direct observed SSH without ControlPath can still match a
                // saved profile that carries one. Stable matching ignores the
                // socket, but execution must not silently discard it.
                effective_overlay = split_control_path_ssh_args(&source.ssh_args).1;
            }
        }
        crate::remote_fs::SessionRemoteEndpoint::with_execution_overlay(
            identity,
            managed_profile,
            &effective_overlay,
        )
        .map(crate::remote_fs::FsLocation::session)
    }

    pub(crate) fn current_location(
        &self,
        hosts: &[crate::config::RemoteHost],
        execution_overlay: &[String],
    ) -> Option<crate::remote_fs::FsLocation> {
        match self {
            Self::Managed { source, identity } => {
                let exact = crate::config::unique_checked_remote_profile_index(hosts, source)?;
                (unique_managed_transport_profile_index(hosts, identity) == Some(exact))
                    .then(|| self.session_location(execution_overlay).ok())
                    .flatten()
            }
            Self::Transient(_) => self.session_location(execution_overlay).ok(),
        }
    }

    pub(crate) fn matches_location(
        &self,
        location: &crate::remote_fs::FsLocation,
        hosts: &[crate::config::RemoteHost],
    ) -> bool {
        match (self, location) {
            (Self::Managed { source, .. }, crate::remote_fs::FsLocation::Remote(index)) => {
                crate::config::checked_remote_host(hosts, *index)
                    .is_ok_and(|current| current == source)
            }
            (
                Self::Managed { source, identity },
                crate::remote_fs::FsLocation::Transient(endpoint),
            ) => endpoint.managed_profile() == Some(source) && endpoint.identity() == identity,
            (Self::Transient(expected), crate::remote_fs::FsLocation::Transient(endpoint)) => {
                !endpoint.is_managed() && endpoint.identity() == expected
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ObservedRemoteProfile {
    pub(crate) identity: crate::config::RemoteHost,
    /// Exact explicit ControlPath argv removed from stable identity and later
    /// appended to the immutable execution snapshot.
    pub(crate) execution_overlay: Vec<String>,
}

impl ObservedRemoteProfile {
    /// Add the live, core-validated jsh multiplex socket to the execution-only
    /// argv and re-run Anvil's complete structured profile gate. The socket is
    /// never folded into stable identity, but it is part of the exact process
    /// generation used by SSH-to-Files deduplication and final commit checks.
    pub(crate) fn with_reusable_control_path(
        mut self,
        control_path: Option<&str>,
    ) -> Result<Self, &'static str> {
        if let Some(path) = control_path {
            self.execution_overlay.push("-S".to_string());
            self.execution_overlay.push(path.to_string());
        }
        crate::remote_fs::SessionRemoteEndpoint::with_execution_overlay(
            self.identity.clone(),
            None,
            &self.execution_overlay,
        )?;
        Ok(self)
    }
}

fn ssh_o_option_is_control_path(option: &str) -> bool {
    option
        .split_once('=')
        .map_or(option, |(key, _)| key)
        .eq_ignore_ascii_case("controlpath")
}

/// Split only ControlPath from already structured SSH options. The shared
/// parser normalizes observed operand flags, while configured profiles may
/// still use attached `-Spath`/`-oName=value`; both representations remain
/// exact in the execution overlay.
fn split_control_path_ssh_args(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut stable = Vec::with_capacity(args.len());
    let mut overlay = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        let argument = &args[index];
        if argument == "-S" {
            overlay.push(argument.clone());
            if let Some(operand) = args.get(index + 1) {
                overlay.push(operand.clone());
                index += 1;
            }
        } else if argument.starts_with("-S") && argument.len() > 2 {
            overlay.push(argument.clone());
        } else if argument == "-o" {
            if let Some(option) = args.get(index + 1) {
                let target = if ssh_o_option_is_control_path(option) {
                    &mut overlay
                } else {
                    &mut stable
                };
                target.push(argument.clone());
                target.push(option.clone());
                index += 1;
            } else {
                stable.push(argument.clone());
            }
        } else if let Some(option) = argument.strip_prefix("-o") {
            if ssh_o_option_is_control_path(option) {
                overlay.push(argument.clone());
            } else {
                stable.push(argument.clone());
            }
        } else {
            stable.push(argument.clone());
        }
        index += 1;
    }
    (stable, overlay)
}

fn stable_remote_profile(profile: &crate::config::RemoteHost) -> crate::config::RemoteHost {
    let mut identity = profile.clone();
    identity.ssh_args = split_control_path_ssh_args(&profile.ssh_args).0;
    identity
}

/// Convert the family's process-level target into Anvil's richer launch
/// profile. Fields that have no meaning in the observed argv stay at safe,
/// session-only defaults; in particular this never enables deployment or
/// writes a ControlMaster configuration.
pub(crate) fn observed_remote_profile(
    observed: jterm_core::jsh_remote::RemoteHostConfig,
) -> Result<ObservedRemoteProfile, &'static str> {
    let deploy = jterm_core::jsh_remote::Deploy::parse(&observed.deploy)
        .ok_or("the observed SSH deployment mode is invalid")?;
    if observed.docker
        || !matches!(deploy, jterm_core::jsh_remote::Deploy::Off)
        || observed.deploy_artifact.is_some()
        || observed.session.is_some()
    {
        return Err("the observed process is not a session-only SSH login");
    }
    let mut identity = crate::config::RemoteHost {
        name: observed.name,
        host: observed.host,
        user: observed.user,
        docker: false,
        deploy_artifact: None,
        remote_shell: observed.remote_shell,
        session: None,
        ssh_args: observed.ssh_args,
        login_shell: true,
        multiplex: false,
        deploy,
    };
    crate::config::validate_remote_host(&identity)?;
    let (stable, execution_overlay) = split_control_path_ssh_args(&identity.ssh_args);
    identity.ssh_args = stable;
    crate::config::validate_remote_host(&identity)?;
    Ok(ObservedRemoteProfile {
        identity,
        execution_overlay,
    })
}

pub(crate) fn remote_profiles_share_filesystem(
    managed: &crate::config::RemoteHost,
    observed: &crate::config::RemoteHost,
) -> bool {
    let managed = stable_remote_profile(managed);
    !managed.docker
        && managed.host == observed.host
        && managed.user == observed.user
        && managed.ssh_args == observed.ssh_args
}

fn unique_managed_transport_profile_index(
    hosts: &[crate::config::RemoteHost],
    observed: &crate::config::RemoteHost,
) -> Option<usize> {
    let mut matches = hosts
        .iter()
        .take(crate::config::MAX_REMOTE_HOSTS)
        .enumerate()
        .filter_map(|(index, _)| {
            crate::config::checked_remote_host(hosts, index)
                .ok()
                .filter(|host| remote_profiles_share_filesystem(host, observed))
                .map(|_| index)
        });
    let index = matches.next()?;
    matches.next().is_none().then_some(index)
}

/// Prefer exactly one managed profile with the same process-observed SSH
/// transport. Ambiguity deliberately stays transient: picking between two
/// identity/proxy configurations by display order could browse the wrong
/// machine even when their visible destination strings match.
pub(crate) fn observed_remote_authority(
    observed: crate::config::RemoteHost,
    hosts: &[crate::config::RemoteHost],
) -> ObservedRemoteAuthority {
    match unique_managed_transport_profile_index(hosts, &observed) {
        Some(index) => {
            let source = crate::config::checked_remote_host(hosts, index)
                .expect("unique transport helper admits only validated profiles")
                .clone();
            ObservedRemoteAuthority::Managed {
                identity: stable_remote_profile(&source),
                source,
            }
        }
        None => ObservedRemoteAuthority::Transient(observed),
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SshFileTreeDetection {
    pub(crate) token: u64,
    pub(crate) pane_id: u64,
    /// Normalized process observation. Re-resolving a managed profile after a
    /// config edit must not turn one running SSH process into a second intent.
    pub(crate) observed: crate::config::RemoteHost,
    /// The actual foreground argv returned by the dedicated process observer.
    /// It is diagnostic/dedup state only and is never reparsed here.
    pub(crate) observed_argv: Vec<String>,
    /// Execution-only overlay proven by the process observer. It never
    /// participates in stable profile matching or source rechecks.
    pub(crate) execution_overlay: Vec<String>,
    pub(crate) authority: ObservedRemoteAuthority,
    pub(crate) tree_intent: FileTreeIntent,
    /// The tree already names this stable namespace. The replacement execution
    /// overlay must still pass the staged probe, but a successful probe swaps
    /// only the endpoint snapshot instead of navigating back to remote home.
    pub(crate) preserve_tree: bool,
    /// A user file action begun while the connection probe runs invalidates
    /// the switch without cancelling or hiding that newer work.
    pub(crate) operation_revision: u64,
    pub(crate) resolved: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum SshFileTreeObservation {
    Unsupported { pane_id: u64, reason: &'static str },
    Target(Box<SshFileTreeDetection>),
}

/// Process-key dedup is intentionally independent of probe success and the
/// captured file-action revision. A failed or user-cancelled attempt remains
/// the seen instance of this live argv; only an explicit Retry creates a new
/// token. Otherwise the periodic poll would silently turn cancellation into
/// another automatic attempt.
pub(crate) fn ssh_file_tree_observation_matches_target(
    observation: Option<&SshFileTreeObservation>,
    current_token: u64,
    pane_id: u64,
    argv: &[String],
    observed: &crate::config::RemoteHost,
    execution_overlay: &[String],
) -> bool {
    matches!(
        observation,
        Some(SshFileTreeObservation::Target(detection))
            if detection.token == current_token
                && detection.pane_id == pane_id
                && detection.observed == *observed
                && detection.observed_argv == argv
                && detection.execution_overlay == execution_overlay
    )
}

pub(crate) fn ssh_file_tree_retry_is_current(
    observation: Option<&SshFileTreeObservation>,
    pane_id: u64,
    token: u64,
) -> bool {
    matches!(
        observation,
        Some(SshFileTreeObservation::Target(detection))
            if detection.pane_id == pane_id
                && detection.token == token
                && detection.resolved
    )
}

/// Final pure gate for an observed-SSH probe. The worker token is checked by
/// the caller before this point; this covers the independently changing pane
/// process and file-tree authority, including navigation ABA.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ssh_file_tree_detection_is_current(
    detection: &SshFileTreeDetection,
    pane_id: u64,
    observed_argv: &[String],
    observed: &crate::config::RemoteHost,
    execution_overlay: &[String],
    operation_revision: u64,
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> bool {
    detection.pane_id == pane_id
        && detection.observed_argv == observed_argv
        && detection.observed == *observed
        && detection.execution_overlay == execution_overlay
        && detection.operation_revision == operation_revision
        && file_tree_intent_is_current(&detection.tree_intent, generation, location, hosts)
}

pub(crate) fn terminal_target(
    location: &crate::remote_fs::FsLocation,
    root: &Path,
    hosts: &[crate::config::RemoteHost],
) -> Result<FileTreeTerminalTarget, &'static str> {
    match location {
        crate::remote_fs::FsLocation::Local => {
            if !root.is_absolute() {
                return Err("The current file-tree directory is unavailable.");
            }
            let cwd = root.to_str().ok_or(
                "The current file-tree directory contains non-UTF-8 bytes and cannot be used as a terminal cwd.",
            )?;
            Ok(FileTreeTerminalTarget::Local(cwd.to_string()))
        }
        crate::remote_fs::FsLocation::Remote(index) => {
            crate::config::checked_remote_host(hosts, *index)
                .cloned()
                .map(FileTreeTerminalTarget::Remote)
        }
        crate::remote_fs::FsLocation::Transient(endpoint) => {
            crate::config::validate_remote_host(endpoint.identity())?;
            if endpoint.is_managed() {
                let profile = endpoint
                    .managed_profile()
                    .ok_or("The matching saved remote profile is unavailable.")?;
                crate::config::validate_remote_host(profile)?;
                Ok(FileTreeTerminalTarget::Remote(profile.clone()))
            } else {
                crate::config::validate_remote_host(endpoint.execution())?;
                Ok(FileTreeTerminalTarget::TemporarySsh(
                    endpoint.execution().clone(),
                ))
            }
        }
    }
}

/// Authority captured when a user opens a delayed file-operation dialog.
/// Paths alone are not enough: after the dialog appears, the tree can move to
/// another filesystem or an index-backed remote profile can be edited in
/// place. Confirming such a stale dialog must never reinterpret its old path
/// against the new backend.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FileTreeIntent {
    generation: u64,
    /// Present for actions captured from visible tree UI. Internal navigation
    /// probes omit it because a harmless in-place reconciliation must not
    /// cancel their backend authority.
    content_revision: Option<u64>,
    location: crate::remote_fs::FsLocation,
    remote_profile: Option<crate::config::RemoteHost>,
}

pub(crate) fn capture_file_tree_intent(
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> FileTreeIntent {
    let remote_profile = match location {
        crate::remote_fs::FsLocation::Local => None,
        crate::remote_fs::FsLocation::Remote(index) => {
            crate::config::checked_remote_host(hosts, *index)
                .ok()
                .cloned()
        }
        crate::remote_fs::FsLocation::Transient(endpoint) => {
            let profile = endpoint
                .managed_profile()
                .unwrap_or_else(|| endpoint.identity());
            crate::config::validate_remote_host(profile)
                .ok()
                .map(|()| profile.clone())
        }
    };
    FileTreeIntent {
        generation,
        content_revision: None,
        location: location.clone(),
        remote_profile,
    }
}

/// Capture a visible-row/header action. Unlike an internal navigation probe,
/// it is revoked when a successful reconciliation changes the loaded model.
pub(crate) fn capture_file_tree_user_intent(
    generation: u64,
    content_revision: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> FileTreeIntent {
    let mut intent = capture_file_tree_intent(generation, location, hosts);
    intent.content_revision = Some(content_revision);
    intent
}

/// User actions additionally require the exact loaded-model revision they saw.
/// Async filesystem settlement deliberately uses the authority-only predicate
/// below, so already-dispatched work can still finish and report honestly.
pub(crate) fn file_tree_user_intent_is_current(
    intent: &FileTreeIntent,
    content_revision: u64,
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> bool {
    intent
        .content_revision
        .is_none_or(|captured| captured == content_revision)
        && file_tree_intent_is_current(intent, generation, location, hosts)
}

/// Revalidate every part of a delayed operation's launch authority. An
/// invalid remote slot deliberately cannot match itself: both its original
/// and current profile would otherwise be `None` and accidentally pass.
pub(crate) fn file_tree_intent_is_current(
    intent: &FileTreeIntent,
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
) -> bool {
    if intent.generation != generation
        || !crate::remote_fs::locations_share_filesystem(&intent.location, location, hosts)
    {
        return false;
    }
    let captured_is_valid = match (&intent.location, &intent.remote_profile) {
        (crate::remote_fs::FsLocation::Local, None) => true,
        (crate::remote_fs::FsLocation::Remote(index), Some(expected)) => {
            crate::config::checked_remote_host(hosts, *index)
                .is_ok_and(|current| current == expected)
        }
        (crate::remote_fs::FsLocation::Transient(current), Some(expected)) => {
            let stable = crate::config::validate_remote_host(current.identity()).is_ok()
                && crate::config::validate_remote_host(current.execution()).is_ok()
                && if let Some(managed) = current.managed_profile() {
                    managed == expected
                        && crate::config::unique_checked_remote_profile_index(hosts, expected)
                            .is_some()
                } else {
                    current.identity() == expected
                };
            stable
        }
        _ => false,
    };
    let live_is_valid = match location {
        crate::remote_fs::FsLocation::Local => true,
        crate::remote_fs::FsLocation::Remote(index) => {
            crate::config::checked_remote_host(hosts, *index).is_ok()
        }
        crate::remote_fs::FsLocation::Transient(endpoint) => {
            crate::config::validate_remote_host(endpoint.identity()).is_ok()
                && crate::config::validate_remote_host(endpoint.execution()).is_ok()
                && endpoint.managed_profile().is_none_or(|profile| {
                    crate::config::unique_checked_remote_profile_index(hosts, profile).is_some()
                })
        }
    };
    captured_is_valid && live_is_valid
}

/// Revalidate a background callback's tree authority and, for transfers, its
/// monotonic UI identity. Filesystem/clipboard settlement is intentionally
/// independent of this predicate; only progress, toasts, and refreshes are
/// allowed to publish when it returns true.
pub(crate) fn file_tree_async_ui_is_current(
    intent: &FileTreeIntent,
    generation: u64,
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
    expected_transfer: Option<u64>,
    current_transfer: u64,
) -> bool {
    file_tree_intent_is_current(intent, generation, location, hosts)
        && expected_transfer.is_none_or(|expected| expected == current_transfer)
}

/// Following a managed pane must rebuild on a backend change even when both
/// hosts report the same textual cwd. Paths are meaningful only together with
/// their filesystem location; retaining the old rows would relabel B as A.
pub(crate) fn file_tree_follow_requires_reroot(
    current_location: &crate::remote_fs::FsLocation,
    target_location: &crate::remote_fs::FsLocation,
    current_root: &Path,
    target_root: &Path,
) -> bool {
    current_location != target_location || current_root != target_root
}

/// Scan `dir` on a fixed scheduler worker, then hand the
/// result to `apply` on the GTK thread via the glib poll. `loc` + `hosts`
/// snapshot the backend at request time; `remote_fs::list_dir` does the work.
pub(crate) fn request_dir_scan<S, F>(
    loc: crate::remote_fs::FsLocation,
    hosts: Vec<crate::config::RemoteHost>,
    dir: PathBuf,
    started: S,
    apply: F,
) -> io::Result<()>
where
    S: FnOnce(Duration) + 'static,
    F: FnOnce(io::Result<DirectoryListing>) + 'static,
{
    let authority = FsAuthorityKey::capture(&loc, &hosts)?;
    request_fs_op_scheduled(
        authority,
        FsJobPriority::Normal,
        None,
        move || crate::remote_fs::list_dir(&loc, &hosts, &dir),
        started,
        apply,
    )
}

pub(crate) fn request_dir_scan_cancellable<S, F>(
    loc: crate::remote_fs::FsLocation,
    hosts: Vec<crate::config::RemoteHost>,
    dir: PathBuf,
    cancellation: ScanCancellation,
    started: S,
    apply: F,
) -> io::Result<()>
where
    S: FnOnce(Duration) + 'static,
    F: FnOnce(io::Result<DirectoryListing>) + 'static,
{
    let authority = FsAuthorityKey::capture(&loc, &hosts)?;
    let op_cancellation = cancellation.clone();
    request_fs_op_scheduled(
        authority,
        FsJobPriority::Normal,
        Some(cancellation),
        move || crate::remote_fs::list_dir_with_cancellation(&loc, &hosts, &dir, &op_cancellation),
        started,
        apply,
    )
}

/// Latest-wins publication tokens for in-place directory refreshes. The tree
/// generation protects navigation changes; this finer-grained registry also
/// orders two refreshes of the same path within one generation. Finishing the
/// current request consumes its entry, so an older completion arriving later
/// cannot become current again.
#[derive(Default)]
pub(crate) struct DirectoryRefreshRevisions {
    next: u64,
    latest: std::collections::HashMap<PathBuf, DirectoryRefreshTicket>,
}

#[derive(Clone)]
pub(crate) struct DirectoryRefreshTicket {
    revision: u64,
    cancellation: ScanCancellation,
}

impl DirectoryRefreshTicket {
    pub(crate) fn cancellation(&self) -> ScanCancellation {
        self.cancellation.clone()
    }
}

impl DirectoryRefreshRevisions {
    pub(crate) fn begin(&mut self, path: &Path) -> DirectoryRefreshTicket {
        self.next = self.next.wrapping_add(1);
        let ticket = DirectoryRefreshTicket {
            revision: self.next,
            cancellation: ScanCancellation::default(),
        };
        if let Some(previous) = self.latest.insert(path.to_path_buf(), ticket.clone()) {
            previous.cancellation.cancel();
        }
        ticket
    }

    /// Return true only for the latest request for `path`, consuming that
    /// request. A duplicate completion and every superseded request fail.
    pub(crate) fn finish_if_latest(
        &mut self,
        path: &Path,
        ticket: &DirectoryRefreshTicket,
    ) -> bool {
        if self.latest.get(path).map(|latest| latest.revision) != Some(ticket.revision) {
            return false;
        }
        self.latest.remove(path);
        true
    }

    /// A reroot invalidates every outstanding per-directory publication while
    /// retaining the monotonic counter, including across an A -> B -> A ABA.
    pub(crate) fn cancel_all(&mut self) {
        for ticket in self.latest.values() {
            ticket.cancellation.cancel();
        }
        self.latest.clear();
    }

    pub(crate) fn is_pending(&self, path: &Path) -> bool {
        self.latest.contains_key(path)
    }
}

/// A non-root refresh may publish only back into the exact row it captured.
/// Missing rows and a row reference that now resolves to another identity are
/// both rejected; callers must never reinterpret either case as the root.
pub(crate) fn refresh_row_identity_is_current(
    expected_identity: &str,
    current_identity: Option<&str>,
) -> bool {
    current_identity == Some(expected_identity)
}

/// One event from a streaming filesystem op: throttled byte progress, then
/// exactly one terminal result.
pub(crate) enum FsOpOutcome<T> {
    Progress(u64),
    Done(io::Result<T>),
}

/// Run one blocking op on a fixed scheduler worker, streaming
/// throttled progress events and the terminal result to `apply` on the GTK
/// thread via the glib poll. The worker's progress callback is non-blocking:
/// a stalled UI must never back-pressure a transfer.
pub(crate) fn request_fs_op_streaming_at<T, O, F>(
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
    op: O,
    apply: F,
) -> io::Result<()>
where
    O: FnOnce(&dyn Fn(u64)) -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    F: FnMut(FsOpOutcome<T>) + 'static,
{
    let authority = FsAuthorityKey::capture(location, hosts)?;
    let (tx, rx) = mpsc::sync_channel::<FsOpOutcome<T>>(64);
    let job = ScheduledFsJob {
        authority,
        queued_at: Instant::now(),
        cancellation: None,
        cancel: None,
        run: Some(Box::new(move |queue_wait| {
            let running_at = Instant::now();
            let progress = |bytes: u64| {
                let _ = tx.try_send(FsOpOutcome::Progress(bytes));
            };
            let result = op(&progress);
            log::debug!(
                "file-tree background job timing: queued={}ms running={}ms",
                queue_wait.as_millis(),
                running_at.elapsed().as_millis()
            );
            let _ = tx.send(FsOpOutcome::Done(result));
        })),
    };
    FsScheduler::global()?.submit(FsJobPriority::Background, job)?;

    let mut apply = Some(apply);
    glib::timeout_add_local(SCAN_POLL_INTERVAL, move || {
        let mut flow = glib::ControlFlow::Continue;
        loop {
            match rx.try_recv() {
                Ok(FsOpOutcome::Progress(bytes)) => {
                    if let Some(apply) = apply.as_mut() {
                        apply(FsOpOutcome::Progress(bytes));
                    }
                }
                Ok(FsOpOutcome::Done(result)) => {
                    if let Some(mut apply) = apply.take() {
                        apply(FsOpOutcome::Done(result));
                    }
                    flow = glib::ControlFlow::Break;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(mut apply) = apply.take() {
                        apply(FsOpOutcome::Done(Err(io::Error::other(
                            "file-tree op worker disconnected",
                        ))));
                    }
                    flow = glib::ControlFlow::Break;
                    break;
                }
            }
        }
        flow
    });
    Ok(())
}

pub(crate) fn request_fs_op_at<T, O, F>(
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
    op: O,
    apply: F,
) -> io::Result<()>
where
    O: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    F: FnOnce(io::Result<T>) + 'static,
{
    request_fs_op_scheduled(
        FsAuthorityKey::capture(location, hosts)?,
        FsJobPriority::Interactive,
        None,
        op,
        |_| {},
        apply,
    )
}

pub(crate) fn request_fs_op_cancellable_at<T, O, F>(
    location: &crate::remote_fs::FsLocation,
    hosts: &[crate::config::RemoteHost],
    cancellation: ScanCancellation,
    op: O,
    apply: F,
) -> io::Result<()>
where
    O: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    F: FnOnce(io::Result<T>) + 'static,
{
    request_fs_op_scheduled(
        FsAuthorityKey::capture(location, hosts)?,
        FsJobPriority::Interactive,
        Some(cancellation),
        op,
        |_| {},
        apply,
    )
}

enum FsOpEvent<T> {
    Started(Duration),
    Done(io::Result<T>),
}

fn request_fs_op_scheduled<T, O, S, F>(
    authority: FsAuthorityKey,
    priority: FsJobPriority,
    cancellation: Option<ScanCancellation>,
    op: O,
    started: S,
    apply: F,
) -> io::Result<()>
where
    O: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    S: FnOnce(Duration) + 'static,
    F: FnOnce(io::Result<T>) + 'static,
{
    let (tx, rx) = mpsc::sync_channel(2);
    let run_tx = tx.clone();
    let run_cancellation = cancellation.clone();
    let cancel_tx = tx.clone();
    let job = ScheduledFsJob {
        authority,
        queued_at: Instant::now(),
        cancellation,
        cancel: Some(Box::new(move || {
            let _ = cancel_tx.send(FsOpEvent::Done(Err(cancelled_scan_error())));
        })),
        run: Some(Box::new(move |queue_wait| {
            if run_cancellation
                .as_ref()
                .is_some_and(ScanCancellation::is_cancelled)
            {
                let _ = run_tx.send(FsOpEvent::Done(Err(cancelled_scan_error())));
                return;
            }
            if run_tx.send(FsOpEvent::Started(queue_wait)).is_err() {
                return;
            }
            let running_at = Instant::now();
            let result = op();
            log::debug!(
                "file-tree job timing: queued={}ms running={}ms",
                queue_wait.as_millis(),
                running_at.elapsed().as_millis()
            );
            let _ = run_tx.send(FsOpEvent::Done(result));
        })),
    };
    FsScheduler::global()?.submit(priority, job)?;

    let mut started = Some(started);
    let mut apply = Some(apply);
    glib::timeout_add_local(SCAN_POLL_INTERVAL, move || match rx.try_recv() {
        Ok(FsOpEvent::Started(queue_wait)) => {
            if let Some(started) = started.take() {
                started(queue_wait);
            }
            glib::ControlFlow::Continue
        }
        Ok(FsOpEvent::Done(result)) => {
            if let Some(apply) = apply.take() {
                apply(result);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => {
            if let Some(apply) = apply.take() {
                apply(Err(io::Error::other("file-tree scan worker disconnected")));
            }
            glib::ControlFlow::Break
        }
    });
    Ok(())
}

/// Display name, reversible path identity, is-directory, icon name, safe tooltip.
pub(crate) fn new_store() -> TreeStore {
    TreeStore::new(&[
        glib::Type::STRING,
        glib::Type::STRING,
        glib::Type::BOOL,
        glib::Type::STRING,
        glib::Type::STRING,
    ])
}

/// Build the headerless `TreeView` (icon + name in one column) over a
/// `TreeModelFilter` driven by `filter`, with multi-selection enabled. No
/// signals wired. Returns the filter model and the view; every path/iter the
/// view hands out (signals, path_at_pos, selection) is in FILTER-model
/// coordinates and must be converted before indexing `store`.
pub(crate) fn new_view(
    store: &TreeStore,
    filter: &Rc<RefCell<TreeFilter>>,
) -> (TreeModelFilter, TreeView) {
    let filter_model = TreeModelFilter::new(store, None::<&gtk::TreePath>);
    {
        let filter = filter.clone();
        filter_model.set_visible_func(move |model, iter| {
            let state = filter.borrow();
            let identity: String = model
                .get_value(iter, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            // Placeholders (empty identity) never count as matches.
            if identity.is_empty() {
                return !state.is_active();
            }
            let name: String = model
                .get_value(iter, COL_NAME as i32)
                .get()
                .unwrap_or_default();
            state.shows_name(&name) && state.is_visible(&identity)
        });
    }
    let view = TreeView::with_model(&filter_model);
    view.set_headers_visible(false);
    view.set_vexpand(true);
    view.set_tooltip_column(COL_TOOLTIP as i32);
    view.selection().set_mode(gtk::SelectionMode::Multiple);

    let column = TreeViewColumn::new();
    let icon = CellRendererPixbuf::new();
    column.pack_start(&icon, false);
    column.add_attribute(&icon, "icon-name", COL_ICON as i32);
    let text = CellRendererText::new();
    column.pack_start(&text, true);
    column.add_attribute(&text, "text", COL_NAME as i32);
    view.append_column(&column);
    (filter_model, view)
}

/// Insert one entry row under `parent` at `position` (None = append), with
/// the lazy-expansion placeholder child for directories.
fn insert_entry_row(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    position: Option<u32>,
    entry: &FileEntry,
) {
    let FileEntry { name, path, is_dir } = entry;
    let icon = if *is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    };
    let Some(path_identity) = encode_path_identity(path) else {
        log::warn!(
            "file-tree path exceeds the {}-byte identity limit: {}",
            MAX_FILE_PATH_IDENTITY_BYTES,
            display_full_path(path)
        );
        return;
    };
    let display_name = crate::review_input::safe_inline_display(name, MAX_FILE_NAME_DISPLAY_BYTES);
    let tooltip = display_full_path(path);
    let iter = store.insert_with_values(
        parent,
        position,
        &[
            (COL_NAME, &display_name),
            (COL_PATH, &path_identity),
            (COL_IS_DIR, is_dir),
            (COL_ICON, &icon),
            (COL_TOOLTIP, &tooltip),
        ],
    );
    if *is_dir {
        // Placeholder child (empty path) → expander shows, loaded lazily.
        store.insert_with_values(
            Some(&iter),
            None,
            &[
                (COL_NAME, &""),
                (COL_PATH, &""),
                (COL_IS_DIR, &false),
                (COL_ICON, &""),
                (COL_TOOLTIP, &""),
            ],
        );
    }
}

/// Insert one row per pre-scanned directory entry under `parent`.
pub(crate) fn append_entries(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    entries: Vec<FileEntry>,
) {
    for entry in &entries {
        insert_entry_row(store, parent, None, entry);
    }
}

/// Find the first row whose COL_PATH identity matches, walking the whole
/// model. Used to target an in-place refresh at one materialized directory.
pub(crate) fn find_row_by_identity(store: &TreeStore, identity: &str) -> Option<TreeIter> {
    fn walk(store: &TreeStore, parent: Option<&TreeIter>, identity: &str) -> Option<TreeIter> {
        let mut index = 0;
        while let Some(iter) = store.iter_nth_child(parent, index) {
            let value: String = store
                .get_value(&iter, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if value == identity {
                return Some(iter);
            }
            if let Some(found) = walk(store, Some(&iter), identity) {
                return Some(found);
            }
            index += 1;
        }
        None
    }
    walk(store, None, identity)
}

#[derive(Default)]
pub(crate) struct TreeSelectionSnapshot {
    selected: Vec<String>,
    cursor: Option<String>,
}

#[allow(deprecated)]
pub(crate) fn capture_tree_selection(
    store: &TreeStore,
    filter_model: &TreeModelFilter,
    view: &TreeView,
) -> TreeSelectionSnapshot {
    let (selected_paths, _) = view.selection().selected_rows();
    let selected = selected_paths
        .iter()
        .filter_map(|path| filter_model.convert_path_to_child_path(path))
        .filter_map(|path| store.iter(&path))
        .filter_map(|iter| {
            store
                .get_value(&iter, COL_PATH as i32)
                .get::<String>()
                .ok()
                .filter(|identity| !identity.is_empty())
        })
        .collect();
    let cursor = gtk::prelude::TreeViewExt::cursor(view)
        .0
        .and_then(|path| filter_model.convert_path_to_child_path(&path))
        .and_then(|path| store.iter(&path))
        .and_then(|iter| {
            store
                .get_value(&iter, COL_PATH as i32)
                .get::<String>()
                .ok()
                .filter(|identity| !identity.is_empty())
        });
    TreeSelectionSnapshot { selected, cursor }
}

#[allow(deprecated)]
pub(crate) fn restore_tree_selection(
    store: &TreeStore,
    filter_model: &TreeModelFilter,
    view: &TreeView,
    snapshot: TreeSelectionSnapshot,
) {
    let selection = view.selection();
    let mut surviving_paths = Vec::new();
    let mut first_survivor = None;
    for identity in snapshot.selected {
        let Some(iter) = find_row_by_identity(store, &identity) else {
            continue;
        };
        let child_path = store.path(&iter);
        let Some(view_path) = filter_model.convert_child_path_to_path(&child_path) else {
            continue;
        };
        if first_survivor.is_none() {
            first_survivor = Some(view_path.clone());
        }
        surviving_paths.push(view_path);
    }
    let cursor = snapshot
        .cursor
        .and_then(|identity| find_row_by_identity(store, &identity))
        .map(|iter| store.path(&iter))
        .and_then(|path| filter_model.convert_child_path_to_path(&path))
        .or(first_survivor);
    if let Some(path) = cursor {
        gtk::prelude::TreeViewExt::set_cursor(view, &path, None::<&TreeViewColumn>, false);
    }
    selection.unselect_all();
    for path in surviving_paths {
        selection.select_path(&path);
    }
}

#[cfg(test)]
fn surviving_selection_identities(
    selected: &[String],
    loaded: &std::collections::HashSet<String>,
) -> Vec<String> {
    selected
        .iter()
        .filter(|identity| loaded.contains(*identity))
        .cloned()
        .collect()
}

/// Attach identities to a fresh scan, dropping paths too long to encode.
fn identified(entries: Vec<FileEntry>) -> Vec<(String, FileEntry)> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let identity = encode_path_identity(&entry.path)?;
            Some((identity, entry))
        })
        .collect()
}

/// The edits that reconcile one directory's rows with a fresh scan.
struct MergeEdit<'a> {
    /// Indexes of current children to remove, ascending.
    removals: Vec<usize>,
    /// (position, entry) inserts in ascending order; positions apply to the
    /// post-removal model as the inserts land one by one.
    inserts: Vec<(u32, &'a FileEntry)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CurrentMergeRow {
    identity: String,
    is_dir: bool,
}

/// Pure merge computation behind [`merge_refresh_children`]: rows whose path
/// vanished are removed, new entries are inserted in sort order, survivors
/// keep their place (and with it their children and expansion). Returns None
/// when a placeholder child marks a never-expanded directory — its lazy scan
/// sees the fresh state on expansion, so the row stays untouched.
fn plan_merge_refresh<'a>(
    current: &[CurrentMergeRow],
    fresh: &'a [(String, FileEntry)],
) -> Option<MergeEdit<'a>> {
    if current.iter().any(|row| row.identity.is_empty()) {
        return None;
    }
    let fresh_by_id: std::collections::HashMap<&str, &FileEntry> = fresh
        .iter()
        .map(|(id, entry)| (id.as_str(), entry))
        .collect();

    let mut removals = Vec::new();
    let mut survivors: Vec<&str> = Vec::new();
    for (index, row) in current.iter().enumerate() {
        if fresh_by_id
            .get(row.identity.as_str())
            .is_some_and(|entry| entry.is_dir == row.is_dir)
        {
            survivors.push(row.identity.as_str());
        } else {
            removals.push(index);
        }
    }

    let mut inserts = Vec::new();
    let mut insert_at = 0u32;
    let mut survivor_index = 0;
    for (identity, entry) in fresh {
        if survivors.contains(&identity.as_str()) {
            continue;
        }
        while survivor_index < survivors.len() {
            let survivor = fresh_by_id[survivors[survivor_index]];
            if entry_cmp(entry, survivor) == std::cmp::Ordering::Less {
                break;
            }
            survivor_index += 1;
            insert_at += 1;
        }
        inserts.push((insert_at, entry));
        insert_at += 1;
    }
    Some(MergeEdit { removals, inserts })
}

/// Reconcile one directory's rows with a fresh scan, preserving surviving
/// rows (and their expansion). `parent: None` merges at the top level.
pub(crate) fn merge_refresh_children(
    store: &TreeStore,
    parent: Option<&TreeIter>,
    fresh: Vec<FileEntry>,
) -> bool {
    let fresh = identified(fresh);
    let mut current = Vec::new();
    let mut index = 0;
    while let Some(iter) = store.iter_nth_child(parent, index) {
        current.push(CurrentMergeRow {
            identity: store
                .get_value(&iter, COL_PATH as i32)
                .get::<String>()
                .unwrap_or_default(),
            is_dir: store
                .get_value(&iter, COL_IS_DIR as i32)
                .get::<bool>()
                .unwrap_or(false),
        });
        index += 1;
    }
    let Some(edit) = plan_merge_refresh(&current, &fresh) else {
        return false;
    };
    let changed = !edit.removals.is_empty() || !edit.inserts.is_empty();
    // Descending removal keeps the still-valid lower indexes intact.
    for index in edit.removals.iter().rev() {
        if let Some(iter) = store.iter_nth_child(parent, *index as i32) {
            store.remove(&iter);
        }
    }
    for (position, entry) in edit.inserts {
        insert_entry_row(store, parent, Some(position), entry);
    }
    changed
}

/// Lazily fill a directory row's real children on first expansion. `location`
/// decides the backend (local disk or one remote host); a location switch
/// mid-scan drops the stale result before it touches the store.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_expand(
    store: &TreeStore,
    iter: &TreeIter,
    scan_generation: &Rc<Cell<u64>>,
    location: &Rc<RefCell<crate::remote_fs::FsLocation>>,
    hosts: Vec<crate::config::RemoteHost>,
    snapshots: &Rc<RefCell<DirectorySnapshots>>,
    status: &Rc<FileTreeStatusUi>,
    failure_gate: &Rc<RefCell<DirectoryFailureGate>>,
    bypass_failure_gate: bool,
) {
    // A not-yet-loaded directory has a single placeholder child (empty path).
    let Some(first_child) = store.iter_children(Some(iter)) else {
        return;
    };
    let child_path: String = store
        .get_value(&first_child, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    if !child_path.is_empty() {
        return; // already populated
    }
    let scan_in_progress: bool = store
        .get_value(&first_child, COL_IS_DIR as i32)
        .get()
        .unwrap_or(false);
    if scan_in_progress {
        return;
    }
    let dir_identity: String = store
        .get_value(iter, COL_PATH as i32)
        .get()
        .unwrap_or_default();
    if dir_identity.is_empty() {
        return;
    }
    let Some(dir_path) = decode_path_identity(&dir_identity) else {
        log::warn!("file-tree row contains an invalid path identity");
        return;
    };
    let loc = location.borrow().clone();
    let Ok(authority) = FsAuthorityKey::capture(&loc, &hosts) else {
        return;
    };
    if !bypass_failure_gate
        && !failure_gate
            .borrow()
            .allows_auto_at(&authority, &dir_path, Instant::now())
    {
        return;
    }
    let Some(row_ref) = TreeRowReference::new(store, &store.path(iter)) else {
        return;
    };

    store.set(&first_child, &[(COL_IS_DIR, &true)]);
    let store_for_result = store.clone();
    let active_generation = scan_generation.clone();
    let generation = active_generation.get();
    let expected_identity = dir_identity.clone();
    let expected_display = display_full_path(&dir_path);
    let expected_dir = dir_path.clone();
    let start_error_dir = dir_path.clone();
    let snapshots_for_result = snapshots.clone();
    let failures_for_result = failure_gate.clone();
    let authority_for_result = authority.clone();
    let status_request = status.begin(
        DirectoryScanTarget::Expand(dir_path.clone()),
        DirectoryScanPhase::Loading,
    );
    let status_for_started = status.clone();
    let status_for_result = status.clone();
    let active_location = location.clone();
    let expected_loc = loc.clone();
    if let Err(error) = request_dir_scan(
        loc,
        hosts,
        dir_path,
        move |queue_wait| status_for_started.mark_running(status_request, queue_wait),
        move |result| {
            if active_generation.get() != generation || *active_location.borrow() != expected_loc {
                status_for_result.finish_success(status_request);
                return;
            }
            let Some(row_path) = row_ref.path() else {
                status_for_result.finish_success(status_request);
                return;
            };
            let Some(parent) = store_for_result.iter(&row_path) else {
                status_for_result.finish_success(status_request);
                return;
            };
            let current_path: String = store_for_result
                .get_value(&parent, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if current_path != expected_identity {
                status_for_result.finish_success(status_request);
                return;
            }
            let Some(placeholder) = store_for_result.iter_children(Some(&parent)) else {
                status_for_result.finish_success(status_request);
                return;
            };
            let placeholder_path: String = store_for_result
                .get_value(&placeholder, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if !placeholder_path.is_empty() {
                status_for_result.finish_success(status_request);
                return;
            }
            match result {
                Ok(listing) => {
                    let completed_at = listing.completed_at();
                    store_for_result.remove(&placeholder);
                    let (entries, truncated) = listing.into_parts();
                    append_entries(&store_for_result, Some(&parent), entries);
                    snapshots_for_result
                        .borrow_mut()
                        .record_success(expected_dir.clone(), completed_at);
                    failures_for_result
                        .borrow_mut()
                        .record_success(&authority_for_result, &expected_dir);
                    status_for_result.finish_success(status_request);
                    if truncated {
                        log::warn!(
                        "directory listing retained only the first {} entries: {expected_display}",
                        MAX_DIRECTORY_ENTRIES
                    );
                    }
                }
                Err(error) => {
                    store_for_result.set(&placeholder, &[(COL_IS_DIR, &false)]);
                    failures_for_result.borrow_mut().record_failure_at(
                        authority_for_result.clone(),
                        expected_dir.clone(),
                        crate::remote_fs::classify_fs_error(&error),
                        Instant::now(),
                    );
                    status_for_result.finish_error(status_request, &error);
                    log::warn!("failed to scan directory {expected_display}: {error}");
                }
            }
        },
    ) {
        store.set(&first_child, &[(COL_IS_DIR, &false)]);
        failure_gate.borrow_mut().record_failure_at(
            authority,
            start_error_dir,
            crate::remote_fs::classify_fs_error(&error),
            Instant::now(),
        );
        status.finish_error(status_request, &error);
        log::warn!("failed to start directory scan: {error}");
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Abbreviate the home directory to `~` for the header label.
pub(crate) fn display_path(path: &Path) -> String {
    let display = if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", display_os_str(rel.as_os_str()))
            }
        } else {
            display_os_str(path.as_os_str())
        }
    } else {
        display_os_str(path.as_os_str())
    };
    crate::review_input::safe_inline_display(&display, MAX_FILE_PATH_DISPLAY_BYTES)
}

pub(crate) fn display_full_path(path: &Path) -> String {
    crate::review_input::safe_inline_display(
        &display_os_str(path.as_os_str()),
        MAX_FILE_PATH_DISPLAY_BYTES,
    )
}

/// The Copy Path payload: the row's full path text, display-escaped so a
/// non-UTF-8 name stays unambiguous. Remote rows intentionally get the plain
/// path with no prefix — that is what users paste into the remote shell.
pub(crate) fn copy_path_payload(path: &Path) -> String {
    display_full_path(path)
}

/// Right-click target resolution: a click inside the current selection aims
/// the menu at the whole selection; a click outside collapses the selection
/// to the clicked row first (the bool tells the caller to reselect).
pub(crate) fn menu_targets(selected: &[PathBuf], clicked: &Path) -> (Vec<PathBuf>, bool) {
    if selected.iter().any(|path| path == clicked) {
        (selected.to_vec(), false)
    } else {
        (vec![clicked.to_path_buf()], true)
    }
}

// ---------------------------------------------------------------------------
// Client-side filter of the loaded tree
// ---------------------------------------------------------------------------

/// Live filter state for the sidebar tree. While active, `visible` holds the
/// identities shown (matches plus ancestors); clearing restores the expansion
/// snapshot taken when filtering began.
pub(crate) struct TreeFilter {
    query: String,
    visible: std::collections::HashSet<String>,
    saved_expansion: Option<std::collections::HashSet<String>>,
    show_hidden: bool,
}

impl TreeFilter {
    pub(crate) fn new() -> Self {
        Self {
            query: String::new(),
            visible: std::collections::HashSet::new(),
            saved_expansion: None,
            show_hidden: false,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    fn is_visible(&self, identity: &str) -> bool {
        !self.is_active() || self.visible.contains(identity)
    }

    fn shows_name(&self, name: &str) -> bool {
        self.show_hidden || !name.starts_with('.')
    }
}

/// Change the dotfile policy over already-loaded rows. No filesystem work is
/// needed, so expansion state and remote authority remain untouched.
pub(crate) fn set_tree_show_hidden(
    filter_model: &TreeModelFilter,
    state: &mut TreeFilter,
    show_hidden: bool,
) {
    if state.show_hidden != show_hidden {
        state.show_hidden = show_hidden;
        filter_model.refilter();
    }
}

/// One loaded row for filter planning: path identity, display name, parent
/// (index into the same list, depth-first order).
pub(crate) struct FilterRow {
    pub(crate) identity: String,
    pub(crate) name: String,
    pub(crate) parent: Option<usize>,
}

/// Rows whose name contains `query` (case-insensitive) plus every ancestor
/// of a match. An empty query keeps everything.
pub(crate) fn filter_visible(rows: &[FilterRow], query: &str) -> Vec<bool> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return vec![true; rows.len()];
    }
    let mut visible = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if row.name.to_lowercase().contains(&query) {
            visible[index] = true;
            let mut parent = row.parent;
            while let Some(index) = parent {
                if visible[index] {
                    break;
                }
                visible[index] = true;
                parent = rows[index].parent;
            }
        }
    }
    visible
}

/// All loaded rows in depth-first order with their store paths.
fn collect_filter_rows(store: &TreeStore) -> (Vec<FilterRow>, Vec<gtk::TreePath>) {
    fn walk(
        store: &TreeStore,
        parent: Option<&TreeIter>,
        parent_index: Option<usize>,
        rows: &mut Vec<FilterRow>,
        paths: &mut Vec<gtk::TreePath>,
    ) {
        let mut index = 0;
        while let Some(iter) = store.iter_nth_child(parent, index) {
            let identity: String = store
                .get_value(&iter, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            if identity.is_empty() {
                index += 1;
                continue; // placeholders are not filterable rows
            }
            let name: String = store
                .get_value(&iter, COL_NAME as i32)
                .get()
                .unwrap_or_default();
            let row_index = rows.len();
            rows.push(FilterRow {
                identity,
                name,
                parent: parent_index,
            });
            paths.push(store.path(&iter));
            walk(store, Some(&iter), Some(row_index), rows, paths);
            index += 1;
        }
    }
    let mut rows = Vec::new();
    let mut paths = Vec::new();
    walk(store, None, None, &mut rows, &mut paths);
    (rows, paths)
}

/// Apply or update the filter: recompute visibility over the loaded rows,
/// refilter, and auto-expand ancestors of matches. On clear, restore the
/// expansion snapshot from when filtering began. Pure lookup — never scans.
pub(crate) fn apply_tree_filter(
    store: &TreeStore,
    view: &TreeView,
    filter_model: &TreeModelFilter,
    state: &mut TreeFilter,
    query: &str,
) {
    let was_active = state.is_active();
    if !was_active && !query.is_empty() {
        state.saved_expansion = Some(collect_expanded_identities(store, view, filter_model));
    }
    state.query.clear();
    state.query.push_str(query);
    if state.is_active() {
        let (rows, _) = collect_filter_rows(store);
        let visible = filter_visible(&rows, query);
        state.visible = rows
            .iter()
            .zip(visible.iter())
            .filter(|(_, visible)| **visible)
            .map(|(row, _)| row.identity.clone())
            .collect();
    } else {
        state.visible.clear();
    }
    filter_model.refilter();
    if state.is_active() {
        // Expand every ancestor of a visible row. Those rows are all fully
        // loaded (a loaded descendant implies a loaded chain), so this never
        // triggers a scan.
        let (rows, paths) = collect_filter_rows(store);
        let mut expand = vec![false; rows.len()];
        for index in 0..rows.len() {
            if !state.visible.contains(&rows[index].identity) {
                continue;
            }
            let mut parent = rows[index].parent;
            while let Some(p) = parent {
                expand[p] = true;
                parent = rows[p].parent;
            }
        }
        for (index, _) in rows.iter().enumerate() {
            if !expand[index] {
                continue;
            }
            if let Some(filter_path) = filter_model.convert_child_path_to_path(&paths[index]) {
                view.expand_row(&filter_path, false);
            }
        }
    } else if was_active {
        view.collapse_all();
        if let Some(saved) = state.saved_expansion.take() {
            let (rows, paths) = collect_filter_rows(store);
            for (index, row) in rows.iter().enumerate() {
                if !saved.contains(&row.identity) {
                    continue;
                }
                if let Some(filter_path) = filter_model.convert_child_path_to_path(&paths[index]) {
                    view.expand_row(&filter_path, false);
                }
            }
        }
    }
}

/// Identities of every currently expanded row, for the clear-time restore.
fn collect_expanded_identities(
    store: &TreeStore,
    view: &TreeView,
    filter_model: &TreeModelFilter,
) -> std::collections::HashSet<String> {
    let (rows, paths) = collect_filter_rows(store);
    let mut expanded = std::collections::HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let expanded_now = filter_model
            .convert_child_path_to_path(&paths[index])
            .is_some_and(|filter_path| view.row_expanded(&filter_path));
        if expanded_now {
            expanded.insert(row.identity.clone());
        }
    }
    expanded
}

/// Absolute paths for materialized expanded directory rows. The root is not a
/// model row and is intentionally supplied by the caller. Sorting makes bulk
/// refresh admission deterministic before its hard cap is applied.
pub(crate) fn expanded_directory_paths(
    store: &TreeStore,
    view: &TreeView,
    filter_model: &TreeModelFilter,
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = collect_expanded_identities(store, view, filter_model)
        .into_iter()
        .filter_map(|identity| decode_path_identity(&identity))
        .collect();
    paths.sort();
    paths
}

/// Lexical containment gate before a materialized directory row can become
/// the new root. The authoritative model/type lookup remains in the caller.
pub(crate) fn directory_navigation_path_is_allowed(root: &Path, target: &Path) -> bool {
    root.is_absolute() && target.is_absolute() && target != root && target.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn directory_status_retains_each_failure_and_distinguishes_refreshing() {
        let mut tracker = DirectoryStatusTracker::default();
        let expand_target = DirectoryScanTarget::Expand(PathBuf::from("/remote/first"));
        let refresh_target = DirectoryScanTarget::Refresh(PathBuf::from("/remote/second"));
        let expand_request = tracker.begin(expand_target.clone(), DirectoryScanPhase::Loading);
        let refresh_request = tracker.begin(refresh_target.clone(), DirectoryScanPhase::Refreshing);

        assert!(matches!(
            tracker.visible(),
            Some(VisibleDirectoryStatus::InFlight {
                request,
                target,
                phase: DirectoryScanPhase::Refreshing,
                run_state: DirectoryScanRunState::Queued,
                ..
            }) if *request == refresh_request && target == &refresh_target
        ));
        assert!(tracker.mark_running(refresh_request, Duration::from_millis(7)));
        assert!(matches!(
            tracker.visible(),
            Some(VisibleDirectoryStatus::InFlight {
                request,
                run_state: DirectoryScanRunState::Running,
                queue_wait: Some(wait),
                running_at: Some(_),
                ..
            }) if *request == refresh_request && *wait == Duration::from_millis(7)
        ));

        assert!(tracker.finish_error(expand_request, "permission denied".to_string()));
        assert!(matches!(
            tracker.visible(),
            Some(VisibleDirectoryStatus::Error {
                request,
                target,
                phase: DirectoryScanPhase::Loading,
                message,
            }) if *request == expand_request
                && target == &expand_target
                && message == "permission denied"
        ));
        assert!(tracker.finish_success(refresh_request));
        assert_eq!(tracker.retry_target(), Some(expand_target.clone()));

        let retry_request = tracker.begin(expand_target.clone(), DirectoryScanPhase::Loading);
        assert!(matches!(
            tracker.visible(),
            Some(VisibleDirectoryStatus::InFlight {
                request,
                target,
                phase: DirectoryScanPhase::Loading,
                run_state: DirectoryScanRunState::Queued,
                ..
            }) if *request == retry_request && target == &expand_target
        ));
        assert!(
            !tracker.finish_error(expand_request, "stale failure".to_string()),
            "a superseded completion cannot replace the retry's state"
        );
        assert!(tracker.finish_success(retry_request));
        assert!(tracker.visible().is_none());
    }

    #[test]
    fn directory_status_bounds_finished_errors_without_dropping_active_work() {
        let mut tracker = DirectoryStatusTracker::default();
        let active = tracker.begin(
            DirectoryScanTarget::Refresh(PathBuf::from("/remote/still-running")),
            DirectoryScanPhase::Refreshing,
        );
        assert!(tracker.mark_running(active, Duration::ZERO));
        for index in 0..(MAX_VISIBLE_DIRECTORY_ERRORS + 5) {
            let request = tracker.begin(
                DirectoryScanTarget::Expand(PathBuf::from(format!("/remote/error-{index}"))),
                DirectoryScanPhase::Loading,
            );
            assert!(tracker.finish_error(request, "classified failure".to_string()));
        }
        let errors = tracker
            .statuses
            .iter()
            .filter(|status| matches!(status, VisibleDirectoryStatus::Error { .. }))
            .count();
        assert_eq!(errors, MAX_VISIBLE_DIRECTORY_ERRORS);
        assert!(tracker.statuses.iter().any(|status| matches!(
            status,
            VisibleDirectoryStatus::InFlight { request, .. } if *request == active
        )));
        assert!(tracker.finish_success(active));
    }

    #[test]
    fn snapshot_completed_time_and_explicit_stale_state_drive_refresh() {
        let path = PathBuf::from("/remote/work");
        let completed_at = Instant::now();
        let mut snapshots = DirectorySnapshots::default();
        assert_eq!(
            snapshots.freshness_at(&path, completed_at),
            SnapshotFreshness::Missing
        );
        snapshots.record_success(path.clone(), completed_at);
        assert_eq!(
            snapshots.freshness_at(&path, completed_at + Duration::from_secs(29)),
            SnapshotFreshness::Fresh
        );
        assert_eq!(
            snapshots.freshness_at(&path, completed_at + DIRECTORY_SNAPSHOT_TTL),
            SnapshotFreshness::Stale
        );
        snapshots.record_success(path.clone(), completed_at);
        snapshots.mark_stale(std::iter::once(&path));
        assert_eq!(
            snapshots.freshness_at(&path, completed_at),
            SnapshotFreshness::Stale
        );
        snapshots.reset();
        assert_eq!(
            snapshots.freshness_at(&path, completed_at),
            SnapshotFreshness::Missing
        );
    }

    #[test]
    fn ttl_revalidation_is_ordered_bounded_and_ignores_missing_rows() {
        let now = Instant::now();
        let mut snapshots = DirectorySnapshots::default();
        for path in ["/root", "/root/a", "/root/b"] {
            snapshots.record_success(
                PathBuf::from(path),
                now - DIRECTORY_SNAPSHOT_TTL - Duration::from_secs(1),
            );
        }
        let due = snapshots.due_paths_at(
            ["/root", "/root/a", "/root/missing", "/root/b"]
                .into_iter()
                .map(PathBuf::from),
            now,
            2,
        );
        assert_eq!(due, [PathBuf::from("/root"), PathBuf::from("/root/a")]);
    }

    #[test]
    fn typed_path_validation_is_absolute_normalized_and_spoof_safe() {
        assert_eq!(
            validate_typed_file_tree_path("/srv/项目 one").unwrap(),
            PathBuf::from("/srv/项目 one")
        );
        for invalid in [
            "relative/path",
            "/srv/../secret",
            "/srv/./work",
            "/srv/\0work",
            "/srv/\u{202e}txt",
        ] {
            assert!(
                validate_typed_file_tree_path(invalid).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn pending_navigation_rejects_stale_token_and_authority() {
        let local = FsAuthorityKey::Local;
        let remote = remote_authority(remote_host());
        assert!(pending_navigation_is_current(7, 7, &remote, &remote));
        assert!(!pending_navigation_is_current(6, 7, &remote, &remote));
        assert!(!pending_navigation_is_current(7, 7, &remote, &local));
        let first = crate::remote_fs::SessionRemoteEndpoint::new(
            remote_host(),
            false,
            Some("/tmp/anvil-control-a"),
        )
        .map(crate::remote_fs::FsLocation::session)
        .unwrap();
        let second = crate::remote_fs::SessionRemoteEndpoint::new(
            remote_host(),
            false,
            Some("/tmp/anvil-control-b"),
        )
        .map(crate::remote_fs::FsLocation::session)
        .unwrap();
        assert_ne!(
            FsAuthorityKey::capture(&first, &[]).unwrap(),
            FsAuthorityKey::capture(&second, &[]).unwrap(),
            "execution overlays are part of immutable scheduler/navigation authority"
        );
    }

    #[test]
    fn failure_gate_is_typed_exponential_and_authority_bound() {
        let authority = remote_authority(remote_host());
        let other = FsAuthorityKey::Local;
        let path = PathBuf::from("/remote/work");
        let now = Instant::now();
        let mut gate = DirectoryFailureGate::default();
        assert_eq!(
            gate.record_failure_at(
                authority.clone(),
                path.clone(),
                crate::remote_fs::FsFailureKind::Connection,
                now,
            ),
            Duration::from_secs(1)
        );
        assert!(!gate.allows_auto_at(&authority, &path, now));
        assert!(gate.allows_auto_at(&other, &path, now));
        assert_eq!(
            gate.record_failure_at(
                authority.clone(),
                path.clone(),
                crate::remote_fs::FsFailureKind::Connection,
                now + Duration::from_secs(1),
            ),
            Duration::from_secs(2)
        );
        gate.record_success(&authority, &path);
        assert!(gate.allows_auto_at(&authority, &path, now));
        assert_eq!(
            failure_retry_delay(crate::remote_fs::FsFailureKind::Permission, 9),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn navigation_history_is_success_commit_only_bounded_and_per_authority() {
        let local = FsAuthorityKey::Local;
        let remote_host = remote_host();
        let remote_authority = remote_authority(remote_host.clone());
        let mut history = FileTreeNavigationHistory::default();
        for index in 0..(MAX_FILE_TREE_HISTORY + 5) {
            history.commit(
                NavigationHistoryAction::Push,
                FileTreeHistoryEntry {
                    location: crate::remote_fs::FsLocation::Local,
                    hosts: Vec::new(),
                    root: PathBuf::from(format!("/local/{index}")),
                },
            );
        }
        assert_eq!(history.len(&local), MAX_FILE_TREE_HISTORY);
        assert_eq!(history.len(&remote_authority), 0);
        let (back_index, back_entry) = history.back(&local).unwrap();
        assert_eq!(
            history.retry_action(&local, &back_entry.root),
            NavigationHistoryAction::MoveTo(back_index)
        );
        history.commit(
            NavigationHistoryAction::Push,
            FileTreeHistoryEntry {
                location: crate::remote_fs::FsLocation::Remote(0),
                hosts: vec![remote_host],
                root: PathBuf::from("/remote/home"),
            },
        );
        assert_eq!(history.len(&remote_authority), 1);
        assert!(history.back(&remote_authority).is_none());
        assert!(history.back(&local).is_some());
    }

    #[test]
    fn root_listing_cache_is_authority_bound_lru_and_exactly_invalidated() {
        let local = FsAuthorityKey::Local;
        let remote = remote_authority(remote_host());
        let mut cache = RootListingCache::default();
        for index in 0..(MAX_ROOT_LISTING_CACHE + 2) {
            cache.insert(
                local.clone(),
                PathBuf::from(format!("/root/{index}")),
                DirectoryListing::new(Vec::new(), false),
            );
        }
        assert_eq!(cache.len(), MAX_ROOT_LISTING_CACHE);
        assert!(cache.get(&local, Path::new("/root/0")).is_none());
        cache.insert(
            remote.clone(),
            PathBuf::from("/root/9"),
            DirectoryListing::new(Vec::new(), false),
        );
        cache.invalidate(&local, [&PathBuf::from("/root/9")]);
        assert!(cache.get(&remote, Path::new("/root/9")).is_some());
    }

    #[test]
    fn retry_accessibility_contract_is_nonempty_and_action_specific() {
        assert_eq!(FILE_TREE_RETRY_ACCESSIBLE_LABEL, "Retry directory scan");
        assert!(!FILE_TREE_RETRY_ACCESSIBLE_LABEL.trim().is_empty());
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn retry_widget_is_a_focusable_button() {
        gtk::init().expect("GTK display");
        let status = FileTreeStatusUi::new(|_| {});
        assert!(status.retry.is_focusable());
        assert_eq!(status.retry.widget_name(), "file-tree-retry");
        assert_eq!(
            status.retry.tooltip_text().as_deref(),
            Some("Retry this directory scan")
        );
    }

    fn remote_host() -> crate::config::RemoteHost {
        crate::config::RemoteHost {
            name: "staging".to_string(),
            host: "server.example.com".to_string(),
            user: Some("deploy".to_string()),
            docker: false,
            deploy_artifact: None,
            remote_shell: "jsh".to_string(),
            session: None,
            ssh_args: vec!["-p".to_string(), "2222".to_string()],
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Persist,
        }
    }

    fn remote_authority(host: crate::config::RemoteHost) -> FsAuthorityKey {
        FsAuthorityKey::Remote(Box::new(FsRemoteAuthorityKey {
            identity: host.clone(),
            execution: host,
        }))
    }

    fn observed_profile(argv: &[&str]) -> crate::config::RemoteHost {
        let argv = argv
            .iter()
            .map(|argument| argument.to_string())
            .collect::<Vec<_>>();
        let jterm_core::jsh_remote::ObservedSshTarget::Target(observed) =
            jterm_core::jsh_remote::observed_ssh_target(&argv)
        else {
            panic!("expected process-observed SSH target");
        };
        observed_remote_profile(observed)
            .expect("observed target converts to Anvil profile")
            .identity
    }

    fn session_location(
        host: crate::config::RemoteHost,
        managed: bool,
    ) -> crate::remote_fs::FsLocation {
        crate::remote_fs::SessionRemoteEndpoint::new(host, managed, None)
            .map(crate::remote_fs::FsLocation::session)
            .expect("valid session endpoint")
    }

    #[test]
    fn entries_sort_directories_first_then_by_name() {
        let mut entries = vec![
            FileEntry {
                name: "Zulu.txt".into(),
                path: PathBuf::from("Zulu.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "beta".into(),
                path: PathBuf::from("beta"),
                is_dir: true,
            },
            FileEntry {
                name: "Alpha.txt".into(),
                path: PathBuf::from("Alpha.txt"),
                is_dir: false,
            },
            FileEntry {
                name: "Able".into(),
                path: PathBuf::from("Able"),
                is_dir: true,
            },
        ];

        sort_entries(&mut entries);

        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["Able", "beta", "Alpha.txt", "Zulu.txt"]);
    }

    fn scheduler_test_job(
        id: usize,
        cancellation: Option<ScanCancellation>,
        ran: &Arc<Mutex<Vec<usize>>>,
        cancelled: &Arc<Mutex<Vec<usize>>>,
    ) -> ScheduledFsJob {
        scheduler_test_job_at(id, FsAuthorityKey::Local, cancellation, ran, cancelled)
    }

    fn scheduler_test_job_at(
        id: usize,
        authority: FsAuthorityKey,
        cancellation: Option<ScanCancellation>,
        ran: &Arc<Mutex<Vec<usize>>>,
        cancelled: &Arc<Mutex<Vec<usize>>>,
    ) -> ScheduledFsJob {
        let ran = ran.clone();
        let cancelled = cancelled.clone();
        ScheduledFsJob {
            authority,
            queued_at: Instant::now(),
            cancellation,
            run: Some(Box::new(move |_| ran.lock().unwrap().push(id))),
            cancel: Some(Box::new(move || cancelled.lock().unwrap().push(id))),
        }
    }

    #[test]
    fn scheduler_has_a_hard_pending_cap_and_cancelled_jobs_free_capacity() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let mut queues = SchedulerQueues::new(2);
        let cancellation = ScanCancellation::default();
        assert!(queues
            .push(
                FsJobPriority::Normal,
                scheduler_test_job(1, Some(cancellation.clone()), &ran, &cancelled),
            )
            .is_ok());
        assert!(queues
            .push(
                FsJobPriority::Interactive,
                scheduler_test_job(2, None, &ran, &cancelled),
            )
            .is_ok());
        assert!(queues
            .push(
                FsJobPriority::Background,
                scheduler_test_job(3, None, &ran, &cancelled),
            )
            .is_err());

        cancellation.cancel();
        let retired = queues.drain_cancelled();
        assert_eq!(retired.len(), 1);
        for job in retired {
            job.cancel();
        }
        assert_eq!(*cancelled.lock().unwrap(), [1]);
        assert!(queues
            .push(
                FsJobPriority::Background,
                scheduler_test_job(3, None, &ran, &cancelled),
            )
            .is_ok());
    }

    #[test]
    fn scheduler_queue_stress_never_exceeds_the_production_pending_limit() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let mut queues = SchedulerQueues::new(MAX_PENDING_FS_JOBS);
        let mut tokens = Vec::new();
        for id in 0..MAX_PENDING_FS_JOBS {
            let token = ScanCancellation::default();
            assert!(queues
                .push(
                    FsJobPriority::Normal,
                    scheduler_test_job(id, Some(token.clone()), &ran, &cancelled),
                )
                .is_ok());
            tokens.push(token);
        }
        assert_eq!(queues.len(), MAX_PENDING_FS_JOBS);
        assert!(queues
            .push(
                FsJobPriority::Interactive,
                scheduler_test_job(999, None, &ran, &cancelled),
            )
            .is_err());

        for token in tokens.iter().step_by(2) {
            token.cancel();
        }
        let retired = queues.drain_cancelled();
        assert_eq!(retired.len(), MAX_PENDING_FS_JOBS / 2);
        for job in retired {
            job.cancel();
        }
        assert_eq!(queues.len(), MAX_PENDING_FS_JOBS / 2);
        assert_eq!(cancelled.lock().unwrap().len(), MAX_PENDING_FS_JOBS / 2);
    }

    #[test]
    fn scheduler_is_fifo_within_weighted_fair_priority_lanes() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let mut queues = SchedulerQueues::new(32);
        for id in 10..14 {
            assert!(queues
                .push(
                    FsJobPriority::Interactive,
                    scheduler_test_job(id, None, &ran, &cancelled),
                )
                .is_ok());
        }
        for id in 20..22 {
            assert!(queues
                .push(
                    FsJobPriority::Normal,
                    scheduler_test_job(id, None, &ran, &cancelled),
                )
                .is_ok());
        }
        assert!(queues
            .push(
                FsJobPriority::Background,
                scheduler_test_job(30, None, &ran, &cancelled),
            )
            .is_ok());

        while let Some((_priority, job)) = queues.pop_next(true, &[]) {
            job.run();
        }
        assert_eq!(*ran.lock().unwrap(), [10, 11, 12, 13, 20, 21, 30]);
        assert!(cancelled.lock().unwrap().is_empty());
    }

    #[test]
    fn scheduler_physically_skips_a_superseded_job_before_start() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let cancellation = ScanCancellation::default();
        let mut queues = SchedulerQueues::new(1);
        assert!(queues
            .push(
                FsJobPriority::Normal,
                scheduler_test_job(7, Some(cancellation.clone()), &ran, &cancelled),
            )
            .is_ok());
        cancellation.cancel();
        for job in queues.drain_cancelled() {
            job.cancel();
        }
        assert!(queues.pop_next(true, &[]).is_none());
        assert!(ran.lock().unwrap().is_empty());
        assert_eq!(*cancelled.lock().unwrap(), [7]);
    }

    #[test]
    fn scheduler_round_robins_authorities_and_enforces_remote_caps() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let authority_a = remote_authority(remote_host());
        let mut second = remote_host();
        second.name = "production".to_string();
        second.host = "prod.example.com".to_string();
        let authority_b = remote_authority(second);
        let mut queues = SchedulerQueues::new(MAX_PENDING_FS_JOBS);

        for id in 0..MAX_REMOTE_PENDING_FS_JOBS {
            assert!(queues
                .push(
                    FsJobPriority::Normal,
                    scheduler_test_job_at(id, authority_a.clone(), None, &ran, &cancelled,),
                )
                .is_ok());
        }
        assert!(
            queues
                .push(
                    FsJobPriority::Interactive,
                    scheduler_test_job_at(999, authority_a.clone(), None, &ran, &cancelled,),
                )
                .is_err(),
            "one remote cannot occupy more than its pending quota"
        );
        assert!(
            queues
                .push(
                    FsJobPriority::Normal,
                    scheduler_test_job_at(1000, authority_b.clone(), None, &ran, &cancelled,),
                )
                .is_ok(),
            "another authority keeps independent capacity"
        );

        let running = vec![(authority_a.clone(), MAX_REMOTE_RUNNING_FS_JOBS)];
        let (_, job) = queues
            .pop_next(true, &running)
            .expect("unblocked authority must remain runnable");
        assert_eq!(job.authority, authority_b);
    }

    #[test]
    fn scheduler_is_round_robin_within_one_priority_lane() {
        let ran = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let authority_a = remote_authority(remote_host());
        let mut second = remote_host();
        second.host = "other.example.com".to_string();
        let authority_b = remote_authority(second);
        let mut queues = SchedulerQueues::new(8);
        for (id, authority) in [
            (1, authority_a.clone()),
            (2, authority_a),
            (10, authority_b.clone()),
            (11, authority_b),
        ] {
            assert!(queues
                .push(
                    FsJobPriority::Normal,
                    scheduler_test_job_at(id, authority, None, &ran, &cancelled),
                )
                .is_ok());
        }
        while let Some((_, job)) = queues.pop_next(true, &[]) {
            job.run();
        }
        assert_eq!(*ran.lock().unwrap(), [1, 10, 2, 11]);
    }

    #[test]
    fn directory_refresh_revisions_are_latest_wins_per_path() {
        let mut revisions = DirectoryRefreshRevisions::default();
        let first_a = revisions.begin(Path::new("/remote/a"));
        let only_b = revisions.begin(Path::new("/remote/b"));
        let second_a = revisions.begin(Path::new("/remote/a"));

        assert!(first_a.cancellation().is_cancelled());
        assert!(!second_a.cancellation().is_cancelled());
        assert!(!revisions.finish_if_latest(Path::new("/remote/a"), &first_a));
        assert!(revisions.finish_if_latest(Path::new("/remote/b"), &only_b));
        assert!(revisions.finish_if_latest(Path::new("/remote/a"), &second_a));
        assert!(
            !revisions.finish_if_latest(Path::new("/remote/a"), &second_a),
            "one terminal result may publish only once"
        );
    }

    #[test]
    fn reroot_and_invalid_row_target_fail_closed() {
        let mut revisions = DirectoryRefreshRevisions::default();
        let request = revisions.begin(Path::new("/remote/project"));
        revisions.cancel_all();
        assert!(request.cancellation().is_cancelled());
        assert!(!revisions.finish_if_latest(Path::new("/remote/project"), &request));

        assert!(refresh_row_identity_is_current("row-a", Some("row-a")));
        assert!(!refresh_row_identity_is_current("row-a", Some("row-b")));
        assert!(
            !refresh_row_identity_is_current("row-a", None),
            "a vanished non-root target must not be treated as the root"
        );
    }

    #[test]
    fn terminal_target_keeps_local_cwd_but_remote_launches_only_the_profile() {
        assert_eq!(
            terminal_target(
                &crate::remote_fs::FsLocation::Local,
                Path::new("/work/tree"),
                &[]
            ),
            Ok(FileTreeTerminalTarget::Local("/work/tree".to_string()))
        );

        let host = remote_host();
        assert_eq!(
            terminal_target(
                &crate::remote_fs::FsLocation::Remote(0),
                Path::new("/remote/browsed/path"),
                std::slice::from_ref(&host)
            ),
            Ok(FileTreeTerminalTarget::Remote(host))
        );
    }

    #[test]
    fn transient_terminal_target_carries_its_own_validated_profile() {
        let observed =
            observed_profile(&["/usr/bin/ssh", "root@dsw-notebook.example.com", "-p", "22"]);
        let location = crate::remote_fs::SessionRemoteEndpoint::new(
            observed.clone(),
            false,
            Some("/run/user/1000/live-cm-%C"),
        )
        .map(crate::remote_fs::FsLocation::session)
        .expect("temporary execution endpoint");
        let mut execution = observed;
        execution
            .ssh_args
            .extend(["-S".to_string(), "/run/user/1000/live-cm-%C".to_string()]);
        assert_eq!(
            terminal_target(&location, Path::new("/remote/path"), &[]),
            Ok(FileTreeTerminalTarget::TemporarySsh(execution))
        );
    }

    #[test]
    fn actual_jsh_launcher_fixture_keeps_base_identity_and_overlays_control_path() {
        let argv = [
            "/bin/sh",
            "/home/alice/.cache/jsh/jsh-remote.sh",
            "--persist",
            "--local-jsh",
            "/home/alice/.local/bin/jsh",
            "root@dsw-notebook.example.com",
            "--",
            "-p",
            "22",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let command = jterm_core::process::ObservedSshCommand {
            target: jterm_core::jsh_remote::observed_ssh_target(&argv),
            argv: argv.clone(),
            reusable_control_path: Some("/run/user/1000/cm-%C".to_string()),
        };
        assert_eq!(command.argv, argv, "dedup retains the real wrapper argv");
        let jterm_core::jsh_remote::ObservedSshTarget::Target(target) = command.target else {
            panic!("the production jsh wrapper shape must classify as SSH")
        };
        let mut profile = observed_remote_profile(target).expect("base target profile");
        profile
            .execution_overlay
            .extend(["-S".to_string(), "/run/user/1000/cm-%C".to_string()]);
        let authority = observed_remote_authority(profile.identity.clone(), &[]);
        let location = authority
            .session_location(&profile.execution_overlay)
            .expect("validated endpoint overlay");
        let crate::remote_fs::FsLocation::Transient(endpoint) = location else {
            panic!("observed target must be value-owned")
        };
        assert_eq!(endpoint.identity(), &profile.identity);
        assert_eq!(endpoint.identity().ssh_args, ["-p", "22"]);
        assert_eq!(
            endpoint.execution().ssh_args,
            ["-p", "22", "-S", "/run/user/1000/cm-%C"]
        );
    }

    #[test]
    fn explicit_control_path_is_execution_overlay_and_saved_matching_ignores_it() {
        let argv = [
            "ssh",
            "-p2222",
            "-S/run/user/1000/live-cm-%C",
            "deploy@server.example.com",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let jterm_core::jsh_remote::ObservedSshTarget::Target(target) =
            jterm_core::jsh_remote::observed_ssh_target(&argv)
        else {
            panic!("direct SSH with -S must be observable")
        };
        let observed = observed_remote_profile(target).expect("validated observed profile");
        assert_eq!(observed.identity.ssh_args, ["-p", "2222"]);
        assert_eq!(
            observed.execution_overlay,
            ["-S", "/run/user/1000/live-cm-%C"]
        );

        let mut managed = remote_host();
        managed
            .ssh_args
            .extend(["-S".to_string(), "/saved/cm-%C".to_string()]);
        let authority = observed_remote_authority(observed.identity.clone(), &[managed.clone()]);
        assert!(matches!(
            &authority,
            ObservedRemoteAuthority::Managed { source, identity }
                if source == &managed && identity.ssh_args == ["-p", "2222"]
        ));
        let location = authority
            .current_location(&[managed.clone()], &observed.execution_overlay)
            .expect("unique saved transport remains authoritative");
        let crate::remote_fs::FsLocation::Transient(endpoint) = location else {
            panic!("followed saved profile uses a frozen endpoint")
        };
        assert_eq!(endpoint.managed_profile(), Some(&managed));
        assert_eq!(endpoint.identity().ssh_args, ["-p", "2222"]);
        assert_eq!(
            endpoint.execution().ssh_args,
            ["-p", "2222", "-S", "/run/user/1000/live-cm-%C"]
        );
        let saved_fallback = authority
            .current_location(&[managed.clone()], &[])
            .expect("saved explicit ControlPath is the execution fallback");
        let crate::remote_fs::FsLocation::Transient(saved_fallback) = saved_fallback else {
            panic!("saved follow uses a session endpoint")
        };
        assert_eq!(
            saved_fallback.execution().ssh_args,
            ["-p", "2222", "-S", "/saved/cm-%C"]
        );

        let option_argv = [
            "ssh",
            "-o",
            "ControlPath=/tmp/direct-cm-%C",
            "deploy@server.example.com",
            "-p",
            "2222",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let jterm_core::jsh_remote::ObservedSshTarget::Target(target) =
            jterm_core::jsh_remote::observed_ssh_target(&option_argv)
        else {
            panic!("direct SSH with -o ControlPath must be observable")
        };
        let option = observed_remote_profile(target).expect("validated -o profile");
        assert_eq!(option.identity.ssh_args, ["-p", "2222"]);
        assert_eq!(
            option.execution_overlay,
            ["-o", "ControlPath=/tmp/direct-cm-%C"]
        );

        let mut duplicate = managed.clone();
        duplicate.name = "same transport, other socket".to_string();
        duplicate.ssh_args.pop();
        duplicate.ssh_args.pop();
        duplicate
            .ssh_args
            .extend(["-S".to_string(), "/other/cm-%C".to_string()]);
        assert!(matches!(
            observed_remote_authority(observed.identity, &[managed, duplicate]),
            ObservedRemoteAuthority::Transient(_)
        ));
    }

    #[test]
    fn observed_ssh_prefers_one_exact_managed_transport_but_not_ambiguity() {
        let observed = observed_profile(&["ssh", "deploy@server.example.com", "-p", "2222"]);
        let managed = remote_host();
        let authority = observed_remote_authority(observed.clone(), std::slice::from_ref(&managed));
        assert!(matches!(
            &authority,
            ObservedRemoteAuthority::Managed { source, .. } if source == &managed
        ));
        assert!(authority
            .current_location(std::slice::from_ref(&managed), &[])
            .is_some());

        let mut same_transport = managed.clone();
        same_transport.name = "same endpoint, different workflow".to_string();
        same_transport.remote_shell = "bash".to_string();
        assert!(matches!(
            observed_remote_authority(
                observed.clone(),
                &[managed.clone(), same_transport.clone()]
            ),
            ObservedRemoteAuthority::Transient(profile) if profile == observed
        ));
        assert_eq!(
            authority.current_location(&[managed, same_transport], &[]),
            None,
            "a second transport match appearing during the probe cancels managed commit"
        );
    }

    #[test]
    fn detected_ssh_commit_requires_same_process_and_tree_intent() {
        let observed = observed_profile(&["ssh", "deploy@server.example.com", "-p2222"]);
        let location = crate::remote_fs::FsLocation::Local;
        let detection = SshFileTreeDetection {
            token: 9,
            pane_id: 44,
            observed: observed.clone(),
            observed_argv: vec!["ssh".to_string(), "deploy@server.example.com".to_string()],
            execution_overlay: Vec::new(),
            authority: ObservedRemoteAuthority::Transient(observed.clone()),
            tree_intent: capture_file_tree_intent(7, &location, &[]),
            preserve_tree: false,
            operation_revision: 3,
            resolved: false,
        };
        assert!(ssh_file_tree_detection_is_current(
            &detection,
            44,
            &detection.observed_argv,
            &observed,
            &detection.execution_overlay,
            3,
            7,
            &location,
            &[],
        ));
        assert!(!ssh_file_tree_detection_is_current(
            &detection,
            45,
            &detection.observed_argv,
            &observed,
            &detection.execution_overlay,
            3,
            7,
            &location,
            &[],
        ));
        assert!(!ssh_file_tree_detection_is_current(
            &detection,
            44,
            &[
                "ssh".to_string(),
                "deploy@server.example.com".to_string(),
                "-v".to_string()
            ],
            &observed,
            &detection.execution_overlay,
            3,
            7,
            &location,
            &[],
        ));

        let mut failed = detection.clone();
        failed.resolved = true;
        let observation = SshFileTreeObservation::Target(Box::new(failed));
        assert!(ssh_file_tree_retry_is_current(Some(&observation), 44, 9));
        assert!(ssh_file_tree_observation_matches_target(
            Some(&observation),
            9,
            44,
            &detection.observed_argv,
            &observed,
            &detection.execution_overlay,
        ));
        assert!(
            !ssh_file_tree_detection_is_current(
                &detection,
                44,
                &detection.observed_argv,
                &observed,
                &detection.execution_overlay,
                4,
                7,
                &location,
                &[],
            ) && ssh_file_tree_observation_matches_target(
                Some(&observation),
                9,
                44,
                &detection.observed_argv,
                &observed,
                &detection.execution_overlay,
            ),
            "a user-cancelled retry stays deduplicated instead of auto-rearming the same argv"
        );
        assert!(!ssh_file_tree_observation_matches_target(
            Some(&observation),
            10,
            44,
            &detection.observed_argv,
            &observed,
            &detection.execution_overlay,
        ), "a focus-epoch change deliberately permits a fresh staged probe when A becomes active again");
        let rotated_socket = vec!["-S".to_string(), "/tmp/jsh-new.sock".to_string()];
        assert!(!ssh_file_tree_observation_matches_target(
            Some(&observation),
            9,
            44,
            &detection.observed_argv,
            &observed,
            &rotated_socket,
        ));
        assert!(!ssh_file_tree_detection_is_current(
            &detection,
            44,
            &detection.observed_argv,
            &observed,
            &rotated_socket,
            3,
            7,
            &location,
            &[],
        ));
        assert!(!ssh_file_tree_retry_is_current(Some(&observation), 44, 8));
        assert!(!ssh_file_tree_detection_is_current(
            &detection,
            44,
            &detection.observed_argv,
            &observed,
            &detection.execution_overlay,
            4,
            7,
            &location,
            &[],
        ));
        assert!(!ssh_file_tree_detection_is_current(
            &detection,
            44,
            &detection.observed_argv,
            &observed,
            &detection.execution_overlay,
            3,
            8,
            &location,
            &[],
        ));

        let replacement = observed_profile(&["ssh", "deploy@other.example.com", "-p2222"]);
        assert!(!ssh_file_tree_detection_is_current(
            &detection,
            44,
            &detection.observed_argv,
            &replacement,
            &detection.execution_overlay,
            3,
            7,
            &location,
            &[],
        ));
    }

    #[test]
    fn transient_intent_freezes_the_complete_session_profile() {
        let observed = observed_profile(&["ssh", "deploy@server.example.com", "-p2222"]);
        let location = session_location(observed.clone(), false);
        let intent = capture_file_tree_intent(12, &location, &[]);
        assert!(file_tree_intent_is_current(&intent, 12, &location, &[]));

        let mut replacement = observed;
        replacement.ssh_args = vec!["-p".to_string(), "22".to_string()];
        assert!(!file_tree_intent_is_current(
            &intent,
            12,
            &session_location(replacement, false),
            &[],
        ));
    }

    #[test]
    fn same_namespace_socket_upgrade_preserves_pending_file_intent() {
        let managed = remote_host();
        let observed = observed_profile(&["ssh", "deploy@server.example.com", "-p", "2222"]);
        let hosts = vec![managed];
        let old_location = crate::remote_fs::FsLocation::Remote(0);
        let intent = capture_file_tree_intent(21, &old_location, &hosts);
        let upgraded = observed_remote_authority(observed, &hosts)
            .session_location(&["-S".to_string(), "/run/user/1000/live-cm-%C".to_string()])
            .expect("same-target live endpoint");

        assert!(crate::remote_fs::locations_share_filesystem(
            &old_location,
            &upgraded,
            &hosts
        ));
        assert!(file_tree_intent_is_current(&intent, 21, &upgraded, &hosts));
        let crate::remote_fs::FsLocation::Transient(endpoint) = upgraded else {
            panic!("socket upgrade must be value-owned")
        };
        assert_eq!(
            &endpoint.execution().ssh_args[endpoint.execution().ssh_args.len() - 2..],
            ["-S", "/run/user/1000/live-cm-%C"]
        );
    }

    #[test]
    fn terminal_target_rejects_unusable_local_roots_and_stale_remote_slots() {
        assert!(terminal_target(
            &crate::remote_fs::FsLocation::Local,
            Path::new("relative"),
            &[]
        )
        .is_err());
        let non_utf8 = PathBuf::from(OsString::from_vec(b"/work/\xff".to_vec()));
        assert!(terminal_target(&crate::remote_fs::FsLocation::Local, &non_utf8, &[]).is_err());
        assert!(terminal_target(
            &crate::remote_fs::FsLocation::Remote(1),
            Path::new("/ignored"),
            &[remote_host()]
        )
        .is_err());
    }

    #[test]
    fn delayed_file_tree_intent_requires_the_same_generation_and_location() {
        let intent = capture_file_tree_intent(41, &crate::remote_fs::FsLocation::Local, &[]);
        assert!(file_tree_intent_is_current(
            &intent,
            41,
            &crate::remote_fs::FsLocation::Local,
            &[]
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            42,
            &crate::remote_fs::FsLocation::Local,
            &[]
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            41,
            &crate::remote_fs::FsLocation::Remote(0),
            &[remote_host()]
        ));
    }

    #[test]
    fn reconciliation_revision_revokes_delayed_ui_but_not_dispatched_settlement() {
        let location = crate::remote_fs::FsLocation::Local;
        let intent = capture_file_tree_user_intent(41, 7, &location, &[]);
        assert!(file_tree_user_intent_is_current(
            &intent,
            7,
            41,
            &location,
            &[]
        ));
        assert!(
            !file_tree_user_intent_is_current(&intent, 8, 41, &location, &[]),
            "a changed model revokes a menu or confirmation dialog captured before reconciliation"
        );
        assert!(
            file_tree_async_ui_is_current(&intent, 41, &location, &[], None, 0),
            "already-dispatched filesystem settlement remains bound to backend authority, not row presentation"
        );
    }

    #[test]
    fn delayed_remote_intent_requires_the_complete_original_profile() {
        let host = remote_host();
        let intent = capture_file_tree_intent(
            7,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host),
        );
        assert!(file_tree_intent_is_current(
            &intent,
            7,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host)
        ));

        let mut edited = host.clone();
        edited.host = "replacement.example.com".to_string();
        assert!(!file_tree_intent_is_current(
            &intent,
            7,
            &crate::remote_fs::FsLocation::Remote(0),
            &[edited]
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            7,
            &crate::remote_fs::FsLocation::Local,
            &[host]
        ));
    }

    #[test]
    fn remote_home_navigation_intent_cannot_cross_authority_or_generation() {
        let host = remote_host();
        let location = crate::remote_fs::FsLocation::Remote(0);
        let intent = capture_file_tree_intent(55, &location, std::slice::from_ref(&host));
        assert!(home_navigation_is_current(
            9,
            9,
            &intent,
            55,
            &location,
            std::slice::from_ref(&host),
        ));
        assert!(
            !home_navigation_is_current(9, 10, &intent, 55, &location, std::slice::from_ref(&host),),
            "Home -> Up/Ctrl+L/location must deterministically retire the old Home reply"
        );
        let mut replacement = host.clone();
        replacement.user = Some("other-user".to_string());
        assert!(!home_navigation_is_current(
            9,
            9,
            &intent,
            55,
            &location,
            &[replacement],
        ));
        assert!(!home_navigation_is_current(
            9,
            9,
            &intent,
            56,
            &location,
            std::slice::from_ref(&host),
        ));
    }

    #[test]
    fn remote_home_probe_cannot_cross_generation_or_reused_numeric_slot() {
        let host_a = remote_host();
        let intent = capture_file_tree_intent(
            17,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
        );

        // A -> Local -> B -> slot 0 can end at the same numeric FsLocation,
        // but both the intervening tree generation and profile identity are
        // part of the frozen probe authority.
        let mut host_b = host_a.clone();
        host_b.host = "replacement.example.com".to_string();
        assert!(!file_tree_intent_is_current(
            &intent,
            19,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_b),
        ));
        assert!(!file_tree_intent_is_current(
            &intent,
            17,
            &crate::remote_fs::FsLocation::Remote(0),
            &[host_b],
        ));
    }

    #[test]
    fn delayed_header_terminal_and_drop_require_the_open_time_tree_authority() {
        let local = capture_file_tree_intent(23, &crate::remote_fs::FsLocation::Local, &[]);
        assert!(!file_tree_intent_is_current(
            &local,
            24,
            &crate::remote_fs::FsLocation::Local,
            &[],
        ));

        let host_a = remote_host();
        let mut host_b = host_a.clone();
        host_b.name = "production".to_string();
        host_b.host = "production.example.com".to_string();
        let remote = capture_file_tree_intent(
            30,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
        );
        assert!(!file_tree_intent_is_current(
            &remote,
            30,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_b),
        ));
        assert!(!file_tree_intent_is_current(
            &remote,
            31,
            &crate::remote_fs::FsLocation::Remote(1),
            &[host_b, host_a],
        ));
    }

    #[test]
    fn invalid_remote_slot_never_authorizes_a_delayed_operation() {
        let intent = capture_file_tree_intent(
            9,
            &crate::remote_fs::FsLocation::Remote(1),
            &[remote_host()],
        );
        assert!(!file_tree_intent_is_current(
            &intent,
            9,
            &crate::remote_fs::FsLocation::Remote(1),
            &[remote_host()]
        ));
    }

    #[test]
    fn async_ui_publication_requires_tree_authority_and_latest_transfer_identity() {
        let host_a = remote_host();
        let intent = capture_file_tree_intent(
            12,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
        );

        assert!(file_tree_async_ui_is_current(
            &intent,
            12,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
            Some(8),
            8,
        ));

        // A -> B suppresses late progress, success, and error publication.
        let mut host_b = host_a.clone();
        host_b.host = "replacement.example.com".to_string();
        for (event, transfer) in [
            ("operation success/error", None),
            ("transfer progress/success/error", Some(8)),
        ] {
            assert!(
                !file_tree_async_ui_is_current(
                    &intent,
                    12,
                    &crate::remote_fs::FsLocation::Remote(0),
                    std::slice::from_ref(&host_b),
                    transfer,
                    8,
                ),
                "stale {event} must not publish after A -> B"
            );
        }

        // Starting a newer transfer suppresses an older callback even when
        // both transfers target the same tree (including an ABA payload).
        assert!(!file_tree_async_ui_is_current(
            &intent,
            12,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
            Some(8),
            9,
        ));

        // Ordinary operations have no transfer identity but still fail closed
        // across a root generation change.
        assert!(!file_tree_async_ui_is_current(
            &intent,
            13,
            &crate::remote_fs::FsLocation::Remote(0),
            std::slice::from_ref(&host_a),
            None,
            9,
        ));
    }

    #[test]
    fn remote_follow_reroots_when_only_the_backend_changes() {
        let same_path = Path::new("/home/deploy");
        assert!(file_tree_follow_requires_reroot(
            &crate::remote_fs::FsLocation::Remote(0),
            &crate::remote_fs::FsLocation::Remote(1),
            same_path,
            same_path,
        ));
        assert!(!file_tree_follow_requires_reroot(
            &crate::remote_fs::FsLocation::Remote(1),
            &crate::remote_fs::FsLocation::Remote(1),
            same_path,
            same_path,
        ));
    }

    #[test]
    fn non_utf8_path_identities_round_trip_without_colliding() {
        let ff = PathBuf::from(OsString::from_vec(b"a\xff".to_vec()));
        let fe = PathBuf::from(OsString::from_vec(b"a\xfe".to_vec()));

        let ff_identity = encode_path_identity(&ff).expect("bounded path should encode");
        let fe_identity = encode_path_identity(&fe).expect("bounded path should encode");

        assert_ne!(ff_identity, fe_identity);
        assert_eq!(decode_path_identity(&ff_identity), Some(ff.clone()));
        assert_eq!(decode_path_identity(&fe_identity), Some(fe.clone()));
        assert_eq!(display_os_str(ff.as_os_str()), r"a\xff");
        assert_eq!(display_os_str(fe.as_os_str()), r"a\xfe");
    }

    #[test]
    fn path_identity_rejects_malformed_or_oversized_values() {
        assert_eq!(decode_path_identity(""), None);
        assert_eq!(decode_path_identity("unix-path-v1:0"), None);
        assert_eq!(decode_path_identity("unix-path-v1:gg"), None);
        let oversized = PathBuf::from(OsString::from_vec(vec![
            b'a';
            MAX_FILE_PATH_IDENTITY_BYTES + 1
        ]));
        assert_eq!(encode_path_identity(&oversized), None);
    }

    #[test]
    fn copy_path_payload_is_the_plain_display_path() {
        assert_eq!(
            copy_path_payload(Path::new("/etc/hostname")),
            "/etc/hostname"
        );
        // Non-UTF-8 names keep the unambiguous escaped display form.
        let weird = PathBuf::from(OsString::from_vec(b"/tmp/a\xffb".to_vec()));
        assert_eq!(copy_path_payload(&weird), r"/tmp/a\xffb");
    }

    #[test]
    fn menu_targets_selection_inside_vs_outside() {
        let selected = vec![PathBuf::from("/a/one"), PathBuf::from("/a/two")];

        // A click inside the selection targets the whole selection and keeps it.
        let (targets, collapse) = menu_targets(&selected, Path::new("/a/two"));
        assert_eq!(targets, selected);
        assert!(!collapse);

        // A click outside targets that row alone and collapses the selection.
        let (targets, collapse) = menu_targets(&selected, Path::new("/a/other"));
        assert_eq!(targets, [PathBuf::from("/a/other")]);
        assert!(collapse);

        // No selection at all behaves like a plain single-row click.
        let (targets, collapse) = menu_targets(&[], Path::new("/a/one"));
        assert_eq!(targets, [PathBuf::from("/a/one")]);
        assert!(collapse);
    }

    // -- tree filter (loaded rows only) ---------------------------------------

    /// rows: (identity, name, parent index)
    fn filter_rows(spec: &[(&str, &str, Option<usize>)]) -> Vec<FilterRow> {
        spec.iter()
            .map(|(identity, name, parent)| FilterRow {
                identity: (*identity).to_string(),
                name: (*name).to_string(),
                parent: *parent,
            })
            .collect()
    }

    fn visible_names<'a>(rows: &'a [FilterRow], query: &str) -> Vec<&'a str> {
        rows.iter()
            .zip(filter_visible(rows, query))
            .filter(|(_, visible)| *visible)
            .map(|(row, _)| row.name.as_str())
            .collect()
    }

    #[test]
    fn filter_visible_matches_and_keeps_ancestors() {
        let rows = filter_rows(&[
            ("/r", "r", None),                         // 0
            ("/r/docs", "docs", Some(0)),              // 1
            ("/r/docs/notes.md", "notes.md", Some(1)), // 2
            ("/r/src", "src", Some(0)),                // 3
            ("/r/src/main.rs", "main.rs", Some(3)),    // 4
            ("/r/README.md", "README.md", Some(0)),    // 5
        ]);

        // Nested match keeps the whole ancestor chain, hides siblings.
        assert_eq!(visible_names(&rows, "notes"), ["r", "docs", "notes.md"]);
        // Case-insensitive.
        assert_eq!(visible_names(&rows, "README"), ["r", "README.md"]);
        // Multiple matches keep each ancestor chain once.
        assert_eq!(
            visible_names(&rows, "md"),
            ["r", "docs", "notes.md", "README.md"]
        );
        // No match → nothing visible.
        assert!(visible_names(&rows, "zzz").is_empty());
        // Empty query is the identity.
        assert_eq!(filter_visible(&rows, ""), vec![true; rows.len()]);
        assert_eq!(filter_visible(&rows, "  "), vec![false; 6]); // "  " matches nothing
    }

    #[test]
    fn hidden_file_policy_is_independent_from_name_filtering() {
        let mut state = TreeFilter::new();
        assert!(!state.shows_name(".git"));
        assert!(state.shows_name("src"));
        assert!(!state.is_active());

        state.show_hidden = true;
        assert!(state.shows_name(".git"));
        assert!(!state.is_active(), "dotfile visibility is not a text query");
    }

    // -- merge refresh (in-place directory update) ----------------------------

    fn entry(path: &str, is_dir: bool) -> FileEntry {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        FileEntry::new(name, PathBuf::from(path), is_dir)
    }

    fn identity_of(path: &str) -> String {
        encode_path_identity(Path::new(path)).expect("short paths encode")
    }

    fn current_rows(spec: &[(&str, bool)]) -> Vec<CurrentMergeRow> {
        spec.iter()
            .map(|(path, is_dir)| CurrentMergeRow {
                identity: identity_of(path),
                is_dir: *is_dir,
            })
            .collect()
    }

    /// Simulate the model after applying an edit: stale rows removed, inserts
    /// at their planned positions, survivors untouched.
    fn apply_plan(current: &[CurrentMergeRow], edit: &MergeEdit) -> Vec<String> {
        let mut model: Vec<String> = current.iter().map(|row| row.identity.clone()).collect();
        for index in edit.removals.iter().rev() {
            model.remove(*index);
        }
        for (position, entry) in &edit.inserts {
            model.insert(
                *position as usize,
                encode_path_identity(&entry.path).expect("short paths encode"),
            );
        }
        model
            .iter()
            .map(|identity| {
                decode_path_identity(identity)
                    .expect("rows carry valid identities")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn merge_plan_removes_stale_inserts_sorted_and_keeps_survivors() {
        let current = current_rows(&[
            ("/r/aaa", true),
            ("/r/bbb", true),
            ("/r/file1", false),
            ("/r/file2", false),
        ]);
        let mut fresh_entries = vec![
            entry("/r/aaa", true),    // survives
            entry("/r/ccc", true),    // new dir
            entry("/r/file0", false), // new file
            entry("/r/file1", false), // survives
        ];
        sort_entries(&mut fresh_entries);
        let fresh = identified(fresh_entries);

        let edit = plan_merge_refresh(&current, &fresh).expect("no placeholder");
        assert_eq!(edit.removals, [1, 3], "bbb and file2 are removed");
        let insert_paths: Vec<(u32, String)> = edit
            .inserts
            .iter()
            .map(|(position, entry)| (*position, entry.path.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            insert_paths,
            [(1, "/r/ccc".to_string()), (2, "/r/file0".to_string())],
            "ccc and file0 land at their sorted positions"
        );
        assert_eq!(
            apply_plan(&current, &edit),
            ["/r/aaa", "/r/ccc", "/r/file0", "/r/file1"]
        );
    }

    #[test]
    fn merge_plan_skips_placeholder_rows() {
        // A never-expanded directory has one placeholder child (empty path).
        let current = vec![CurrentMergeRow {
            identity: String::new(),
            is_dir: false,
        }];
        let fresh = identified(vec![entry("/r/aaa/new", false)]);
        assert!(plan_merge_refresh(&current, &fresh).is_none());
    }

    #[test]
    fn merge_plan_handles_rename_shape_and_empty_results() {
        let current = current_rows(&[("/r/alpha.txt", false), ("/r/zeta.txt", false)]);
        let mut fresh_entries = vec![entry("/r/alpha.txt", false), entry("/r/mid.txt", false)];
        sort_entries(&mut fresh_entries);
        let fresh = identified(fresh_entries);
        let edit = plan_merge_refresh(&current, &fresh).expect("no placeholder");
        assert_eq!(edit.removals, [1]);
        assert_eq!(apply_plan(&current, &edit), ["/r/alpha.txt", "/r/mid.txt"]);

        // Everything vanished: the children are all removed.
        let edit = plan_merge_refresh(&current, &[]).expect("no placeholder");
        assert_eq!(edit.removals, [0, 1]);
        assert!(edit.inserts.is_empty());
        assert!(apply_plan(&current, &edit).is_empty());
    }

    #[test]
    fn merge_plan_replaces_a_same_path_type_flip() {
        let current = current_rows(&[("/r/node", false)]);
        let fresh = identified(vec![entry("/r/node", true)]);
        let edit = plan_merge_refresh(&current, &fresh).expect("loaded parent");
        assert_eq!(edit.removals, [0]);
        assert_eq!(edit.inserts.len(), 1);
        assert!(edit.inserts[0].1.is_dir);
        assert_eq!(apply_plan(&current, &edit), ["/r/node"]);
    }

    #[test]
    fn selection_restore_keeps_only_surviving_identities_in_original_order() {
        let selected = vec![
            identity_of("/r/removed"),
            identity_of("/r/kept-b"),
            identity_of("/r/kept-a"),
        ];
        let loaded = [identity_of("/r/kept-a"), identity_of("/r/kept-b")]
            .into_iter()
            .collect();
        assert_eq!(
            surviving_selection_identities(&selected, &loaded),
            [identity_of("/r/kept-b"), identity_of("/r/kept-a")]
        );
    }

    #[test]
    fn entering_a_directory_is_lexically_confined_to_the_current_root() {
        assert!(directory_navigation_path_is_allowed(
            Path::new("/remote/work"),
            Path::new("/remote/work/src")
        ));
        assert!(!directory_navigation_path_is_allowed(
            Path::new("/remote/work"),
            Path::new("/remote/work")
        ));
        assert!(!directory_navigation_path_is_allowed(
            Path::new("/remote/work"),
            Path::new("/remote/other")
        ));
        assert!(!directory_navigation_path_is_allowed(
            Path::new("relative"),
            Path::new("relative/child")
        ));
    }
}
