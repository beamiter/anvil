//! Bounded background persistence for UI-owned snapshots.
//!
//! GTK state must be copied while the widgets are on the main thread, but file
//! creation, encoding, compression and `fsync` do not belong there.  This
//! worker owns exactly one background thread.  At most one pending job is kept
//! per target; a newer snapshot replaces an older snapshot that has not begun
//! yet, while different pane/session targets retain FIFO ordering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_PENDING_TARGETS: usize = 128;
const MAX_REPORTED_FAILURES: usize = 32;

type PersistenceTask = Box<dyn FnOnce() -> io::Result<()> + Send + 'static>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PersistenceKey {
    kind: String,
    path: PathBuf,
    nonce: Option<u64>,
}

impl PersistenceKey {
    pub(crate) fn for_path(kind: &str, path: &Path) -> Self {
        Self {
            kind: kind.to_string(),
            path: path.to_path_buf(),
            nonce: None,
        }
    }

    /// Reads are not coalescible: two panes may intentionally restore from the
    /// same legacy file and each owns a different completion route.
    pub(crate) fn unique_for_path(kind: &str, path: &Path) -> Self {
        static NEXT_UNIQUE_KEY: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT_UNIQUE_KEY.fetch_add(1, Ordering::Relaxed);
        Self {
            kind: kind.to_string(),
            path: path.to_path_buf(),
            nonce: Some(sequence),
        }
    }

    #[cfg(test)]
    fn named(name: &str) -> Self {
        Self::for_path("test", Path::new(name))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistenceFailure {
    pub(crate) operation: String,
    pub(crate) error: String,
}

impl fmt::Display for PersistenceFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.error)
    }
}

struct PendingJob {
    key: PersistenceKey,
    operation: String,
    task: PersistenceTask,
}

struct WorkerState {
    accepting: bool,
    running: bool,
    exited: bool,
    order: VecDeque<PersistenceKey>,
    pending: HashMap<PersistenceKey, PendingJob>,
    failures: VecDeque<(PersistenceKey, PersistenceFailure)>,
    failed_targets: HashSet<PersistenceKey>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            accepting: true,
            running: false,
            exited: false,
            order: VecDeque::new(),
            pending: HashMap::new(),
            failures: VecDeque::new(),
            failed_targets: HashSet::new(),
        }
    }

    fn record_failure(&mut self, key: PersistenceKey, failure: PersistenceFailure) {
        // A failing mount can reject every autosave in a burst. Report one
        // event per target until that target saves successfully again.
        if !self.failed_targets.contains(&key) && self.failed_targets.len() == MAX_REPORTED_FAILURES
        {
            if let Some(stale_key) = self.failed_targets.iter().next().cloned() {
                self.failed_targets.remove(&stale_key);
                self.failures
                    .retain(|(failed_key, _)| failed_key != &stale_key);
            }
        }
        if self.failed_targets.contains(&key) {
            if let Some(existing) = self
                .failures
                .iter_mut()
                .find(|(existing_key, _)| existing_key == &key)
            {
                existing.1 = failure;
            }
            return;
        }
        if self.failures.len() == MAX_REPORTED_FAILURES {
            if let Some((stale_key, _)) = self.failures.pop_front() {
                self.failed_targets.remove(&stale_key);
            }
        }
        self.failed_targets.insert(key.clone());
        self.failures.push_back((key, failure));
    }
}

struct WorkerShared {
    state: Mutex<WorkerState>,
    changed: Condvar,
    capacity: usize,
}

struct PersistenceWorker {
    shared: Arc<WorkerShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl PersistenceWorker {
    fn new(capacity: usize) -> io::Result<Self> {
        let shared = Arc::new(WorkerShared {
            state: Mutex::new(WorkerState::new()),
            changed: Condvar::new(),
            capacity,
        });
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("anvil-persistence".to_string())
            .spawn(move || run_worker(worker_shared))?;
        Ok(Self {
            shared,
            thread: Mutex::new(Some(handle)),
        })
    }

    fn enqueue(
        &self,
        key: PersistenceKey,
        operation: String,
        task: PersistenceTask,
    ) -> io::Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.accepting {
            let error = io::Error::new(
                io::ErrorKind::BrokenPipe,
                "persistence worker is shutting down",
            );
            state.record_failure(
                key,
                PersistenceFailure {
                    operation,
                    error: error.to_string(),
                },
            );
            return Err(error);
        }

        let job = PendingJob {
            key: key.clone(),
            operation: operation.clone(),
            task,
        };
        if let Some(previous) = state.pending.get_mut(&key) {
            // Keep the target's original queue position, but replace all owned
            // snapshot bytes with the newest state.
            *previous = job;
            return Ok(());
        }
        if state.pending.len() >= self.shared.capacity {
            let error = io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "persistence queue is full ({} distinct targets)",
                    self.shared.capacity
                ),
            );
            state.record_failure(
                key,
                PersistenceFailure {
                    operation,
                    error: error.to_string(),
                },
            );
            return Err(error);
        }

        state.order.push_back(key.clone());
        state.pending.insert(key, job);
        self.shared.changed.notify_one();
        Ok(())
    }

    fn drain_failures(&self) -> Vec<PersistenceFailure> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .failures
            .drain(..)
            .map(|(_, failure)| failure)
            .collect()
    }

    fn shutdown(&self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.accepting = false;
        self.shared.changed.notify_all();

        while !state.exited {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "persistence worker did not flush before shutdown",
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, wait) = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if wait.timed_out() && !state.exited {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "persistence worker did not flush before shutdown",
                ));
            }
        }
        drop(state);

        if let Some(handle) = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle
                .join()
                .map_err(|_| io::Error::other("persistence worker panicked"))?;
        }
        Ok(())
    }
}

fn run_worker(shared: Arc<WorkerShared>) {
    loop {
        let job = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                if let Some(key) = state.order.pop_front() {
                    if let Some(job) = state.pending.remove(&key) {
                        state.running = true;
                        break Some(job);
                    }
                    continue;
                }
                if !state.accepting {
                    state.exited = true;
                    shared.changed.notify_all();
                    break None;
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        let Some(job) = job else {
            return;
        };
        let PendingJob {
            key,
            operation,
            task,
        } = job;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task))
            .unwrap_or_else(|_| Err(io::Error::other("persistence task panicked")));
        if let Err(error) = result {
            log::error!("{operation}: {error}");
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.record_failure(
                key,
                PersistenceFailure {
                    operation,
                    error: error.to_string(),
                },
            );
            state.running = false;
            shared.changed.notify_all();
            continue;
        }

        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.failed_targets.remove(&key);
        state.running = false;
        shared.changed.notify_all();
    }
}

static PERSISTENCE_WORKER: OnceLock<Result<PersistenceWorker, String>> = OnceLock::new();

fn global_worker() -> io::Result<&'static PersistenceWorker> {
    PERSISTENCE_WORKER
        .get_or_init(|| {
            PersistenceWorker::new(MAX_PENDING_TARGETS).map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

pub(crate) fn enqueue(
    key: PersistenceKey,
    operation: impl Into<String>,
    task: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    global_worker()?.enqueue(key, operation.into(), Box::new(task))
}

pub(crate) fn drain_failures() -> Vec<PersistenceFailure> {
    match PERSISTENCE_WORKER.get() {
        Some(Ok(worker)) => worker.drain_failures(),
        _ => Vec::new(),
    }
}

pub(crate) fn shutdown(timeout: Duration) -> io::Result<()> {
    match PERSISTENCE_WORKER.get() {
        Some(Ok(worker)) => worker.shutdown(timeout),
        Some(Err(error)) => Err(io::Error::other(error.clone())),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    #[test]
    fn non_utf8_paths_keep_their_original_identity() {
        let first = PathBuf::from(OsString::from_vec(vec![b'h', 0x80]));
        let second = PathBuf::from(OsString::from_vec(vec![b'h', 0x81]));
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(
            PersistenceKey::for_path("history", &first),
            PersistenceKey::for_path("history", &second)
        );
    }

    #[test]
    fn slow_io_keeps_submit_nonblocking_and_latest_pending_snapshot_wins() {
        let worker = PersistenceWorker::new(4).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let writes = Arc::new(Mutex::new(Vec::new()));

        let writes_first = Arc::clone(&writes);
        worker
            .enqueue(
                PersistenceKey::named("session"),
                "save session".into(),
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    writes_first.lock().unwrap().push(1);
                    Ok(())
                }),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let writes_stale = Arc::clone(&writes);
        worker
            .enqueue(
                PersistenceKey::named("session"),
                "save session".into(),
                Box::new(move || {
                    writes_stale.lock().unwrap().push(2);
                    Ok(())
                }),
            )
            .unwrap();
        let writes_latest = Arc::clone(&writes);
        worker
            .enqueue(
                PersistenceKey::named("session"),
                "save session".into(),
                Box::new(move || {
                    writes_latest.lock().unwrap().push(3);
                    Ok(())
                }),
            )
            .unwrap();

        // Enqueue returned while the first task was deliberately blocked.
        assert!(writes.lock().unwrap().is_empty());
        release_tx.send(()).unwrap();
        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(*writes.lock().unwrap(), [1, 3]);
    }

    #[test]
    fn write_failure_is_reported_and_does_not_stop_later_jobs() {
        let worker = PersistenceWorker::new(4).unwrap();
        let completed = Arc::new(AtomicUsize::new(0));
        worker
            .enqueue(
                PersistenceKey::named("broken"),
                "save block history".into(),
                Box::new(|| Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only"))),
            )
            .unwrap();
        let completed_job = Arc::clone(&completed);
        worker
            .enqueue(
                PersistenceKey::named("healthy"),
                "save session".into(),
                Box::new(move || {
                    completed_job.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            )
            .unwrap();

        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(
            worker.drain_failures(),
            [PersistenceFailure {
                operation: "save block history".into(),
                error: "read-only".into(),
            }]
        );
    }

    #[test]
    fn failures_with_the_same_operation_remain_distinct_per_target() {
        let worker = PersistenceWorker::new(4).unwrap();
        for target in ["left", "right"] {
            worker
                .enqueue(
                    PersistenceKey::named(target),
                    "Save Block history".into(),
                    Box::new(move || Err(io::Error::new(io::ErrorKind::PermissionDenied, target))),
                )
                .unwrap();
        }
        worker.shutdown(Duration::from_secs(1)).unwrap();
        let failures = worker.drain_failures();
        assert_eq!(failures.len(), 2);
        assert!(failures.iter().any(|failure| failure.error == "left"));
        assert!(failures.iter().any(|failure| failure.error == "right"));
    }

    #[test]
    fn shutdown_times_out_while_io_is_stuck_then_flushes_after_release() {
        let worker = PersistenceWorker::new(1).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        worker
            .enqueue(
                PersistenceKey::named("slow"),
                "save session".into(),
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let error = worker.shutdown(Duration::from_millis(10)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        release_tx.send(()).unwrap();
        worker.shutdown(Duration::from_secs(1)).unwrap();
    }
}
