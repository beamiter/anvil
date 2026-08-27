//! Bounded background persistence for UI-owned snapshots.
//!
//! GTK state must be copied while the widgets are on the main thread, but file
//! creation, encoding, compression and `fsync` do not belong there.  The
//! persistence system owns two background lanes: session checkpoints are
//! isolated from ordinary history/organism writes so a blocked filesystem
//! target cannot prevent the final workspace snapshot from being flushed.  At
//! most one pending job is kept per target; a newer snapshot replaces an older
//! snapshot that has not begun yet, while different targets retain FIFO ordering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_PENDING_TARGETS: usize = 128;
const MAX_PENDING_SESSION_TARGETS: usize = 1;
const MAX_REPORTED_FAILURES: usize = 32;
/// Bound snapshot memory retained by submitted work, including the one task
/// currently executing. Keeping the running task charged matters because a
/// slow `fsync` can otherwise make room for another full-size snapshot before
/// the first closure releases its owned bytes.
pub(crate) const MAX_PENDING_ESTIMATED_BYTES: usize = 512 * 1024 * 1024;

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
    attempt: u64,
    operation: String,
    estimated_bytes: usize,
    task: PersistenceTask,
}

struct WorkerState {
    accepting: bool,
    running: bool,
    exited: bool,
    next_attempt: u64,
    order: VecDeque<PersistenceKey>,
    pending: HashMap<PersistenceKey, PendingJob>,
    retained_estimated_bytes: usize,
    failures: VecDeque<(PersistenceKey, u64, PersistenceFailure)>,
    failed_targets: HashSet<PersistenceKey>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            accepting: true,
            running: false,
            exited: false,
            next_attempt: 0,
            order: VecDeque::new(),
            pending: HashMap::new(),
            retained_estimated_bytes: 0,
            failures: VecDeque::new(),
            failed_targets: HashSet::new(),
        }
    }

    fn allocate_attempt(&mut self) -> io::Result<u64> {
        let attempt = self.next_attempt;
        self.next_attempt = self
            .next_attempt
            .checked_add(1)
            .ok_or_else(|| io::Error::other("persistence attempt counter exhausted"))?;
        Ok(attempt)
    }

    fn record_failure(&mut self, key: PersistenceKey, attempt: u64, failure: PersistenceFailure) {
        // A failing mount can reject every autosave in a burst. Report one
        // event per target until it is reported or that target saves
        // successfully again.
        if !self.failed_targets.contains(&key) && self.failed_targets.len() == MAX_REPORTED_FAILURES
        {
            if let Some(stale_key) = self.failed_targets.iter().next().cloned() {
                self.failed_targets.remove(&stale_key);
                self.failures
                    .retain(|(failed_key, _, _)| failed_key != &stale_key);
            }
        }
        if self.failed_targets.contains(&key) {
            if let Some(existing) = self
                .failures
                .iter_mut()
                .find(|(existing_key, _, _)| existing_key == &key)
            {
                // An older in-flight task may complete after a newer attempt
                // was rejected. Never let that stale result hide the failure
                // that actually describes the newest lost update.
                if attempt >= existing.1 {
                    existing.1 = attempt;
                    existing.2 = failure;
                }
            }
            return;
        }
        if self.failures.len() == MAX_REPORTED_FAILURES {
            if let Some((stale_key, _, _)) = self.failures.pop_front() {
                self.failed_targets.remove(&stale_key);
            }
        }
        self.failed_targets.insert(key.clone());
        self.failures.push_back((key, attempt, failure));
    }

    fn release_estimated_bytes(&mut self, estimated_bytes: usize) {
        self.retained_estimated_bytes = self
            .retained_estimated_bytes
            .checked_sub(estimated_bytes)
            .expect("persistence estimated-byte accounting underflow");
    }

    fn clear_failure(&mut self, key: &PersistenceKey, successful_attempt: u64) {
        let clears_recorded_failure = self
            .failures
            .iter()
            .any(|(failed_key, attempt, _)| failed_key == key && *attempt <= successful_attempt);
        if !clears_recorded_failure {
            return;
        }
        self.failed_targets.remove(key);
        self.failures
            .retain(|(failed_key, attempt, _)| failed_key != key || *attempt > successful_attempt);
    }
}

struct WorkerShared {
    state: Mutex<WorkerState>,
    changed: Condvar,
    capacity: usize,
    estimated_byte_capacity: usize,
}

/// A byte-budget charge whose lifetime can extend beyond the worker closure
/// which created a retained result. Dropping the last owner releases the
/// charge; acquisition is non-blocking so the single worker can always
/// discard a result instead of waiting behind its own pending jobs.
pub(crate) struct EstimatedBytesReservation {
    shared: Arc<WorkerShared>,
    estimated_bytes: usize,
}

impl EstimatedBytesReservation {
    pub(crate) const fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }
}

impl Drop for EstimatedBytesReservation {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.release_estimated_bytes(self.estimated_bytes);
        self.shared.changed.notify_all();
    }
}

struct PersistenceWorker {
    shared: Arc<WorkerShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl PersistenceWorker {
    fn new(capacity: usize) -> io::Result<Self> {
        Self::new_named(capacity, "anvil-persistence")
    }

    fn new_named(capacity: usize, thread_name: &str) -> io::Result<Self> {
        Self::new_named_with_limits(capacity, thread_name, MAX_PENDING_ESTIMATED_BYTES)
    }

    #[cfg(test)]
    fn new_with_limits(capacity: usize, estimated_byte_capacity: usize) -> io::Result<Self> {
        Self::new_named_with_limits(capacity, "anvil-persistence-test", estimated_byte_capacity)
    }

    fn new_named_with_limits(
        capacity: usize,
        thread_name: &str,
        estimated_byte_capacity: usize,
    ) -> io::Result<Self> {
        let shared = Arc::new(WorkerShared {
            state: Mutex::new(WorkerState::new()),
            changed: Condvar::new(),
            capacity,
            estimated_byte_capacity,
        });
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name(thread_name.to_string())
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
        self.enqueue_weighted(key, operation, 0, task)
    }

    fn enqueue_weighted(
        &self,
        key: PersistenceKey,
        operation: String,
        estimated_bytes: usize,
        task: PersistenceTask,
    ) -> io::Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let attempt = state.allocate_attempt()?;
        if !state.accepting {
            let error = io::Error::new(
                io::ErrorKind::BrokenPipe,
                "persistence worker is shutting down",
            );
            state.record_failure(
                key,
                attempt,
                PersistenceFailure {
                    operation,
                    error: error.to_string(),
                },
            );
            // A rejected task may own a retained-result permit whose Drop
            // re-enters this ledger. Release the mutex before dropping it.
            drop(state);
            return Err(error);
        }

        let previous_estimated_bytes = state
            .pending
            .get(&key)
            .map_or(0, |previous| previous.estimated_bytes);
        let replacing_pending = state.pending.contains_key(&key);
        if !replacing_pending && state.pending.len() >= self.shared.capacity {
            let error = io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "persistence queue is full ({} distinct targets)",
                    self.shared.capacity
                ),
            );
            state.record_failure(
                key,
                attempt,
                PersistenceFailure {
                    operation,
                    error: error.to_string(),
                },
            );
            drop(state);
            return Err(error);
        }

        let retained_without_previous = state
            .retained_estimated_bytes
            .checked_sub(previous_estimated_bytes)
            .expect("pending persistence bytes exceed retained-byte accounting");
        let next_retained_estimated_bytes = retained_without_previous.checked_add(estimated_bytes);
        if next_retained_estimated_bytes
            .is_none_or(|bytes| bytes > self.shared.estimated_byte_capacity)
        {
            let error = io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "persistence queue estimated-byte budget exceeded ({} bytes retained, {} byte submission, {} byte limit)",
                    retained_without_previous,
                    estimated_bytes,
                    self.shared.estimated_byte_capacity
                ),
            );
            state.record_failure(
                key,
                attempt,
                PersistenceFailure {
                    operation,
                    error: error.to_string(),
                },
            );
            drop(state);
            return Err(error);
        }
        let next_retained_estimated_bytes = next_retained_estimated_bytes
            .expect("checked above: persistence retained-byte addition fits");

        let job = PendingJob {
            key: key.clone(),
            attempt,
            operation: operation.clone(),
            estimated_bytes,
            task,
        };
        if replacing_pending {
            // Keep the target's original queue position, but replace all owned
            // snapshot bytes with the newest state. Admission was checked before
            // touching `previous`, so a rejected replacement leaves it intact.
            let previous = state
                .pending
                .insert(key, job)
                .expect("replacing_pending guarantees an existing job");
            state.retained_estimated_bytes = next_retained_estimated_bytes;
            // Pending closures may own external permits/leases whose Drop
            // locks this WorkerState. Never run user-owned destructors while
            // the ledger mutex is held.
            drop(state);
            drop(previous);
            return Ok(());
        }

        state.order.push_back(key.clone());
        state.pending.insert(key, job);
        state.retained_estimated_bytes = next_retained_estimated_bytes;
        self.shared.changed.notify_one();
        Ok(())
    }

    fn try_reserve_estimated_bytes(
        &self,
        estimated_bytes: usize,
    ) -> io::Result<EstimatedBytesReservation> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(next_retained_estimated_bytes) = state
            .retained_estimated_bytes
            .checked_add(estimated_bytes)
            .filter(|bytes| *bytes <= self.shared.estimated_byte_capacity)
        else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "persistence retained-result byte budget exceeded ({} bytes retained, {} byte reservation, {} byte limit)",
                    state.retained_estimated_bytes,
                    estimated_bytes,
                    self.shared.estimated_byte_capacity,
                ),
            ));
        };
        state.retained_estimated_bytes = next_retained_estimated_bytes;
        Ok(EstimatedBytesReservation {
            shared: Arc::clone(&self.shared),
            estimated_bytes,
        })
    }

    fn drain_failures(&self) -> Vec<PersistenceFailure> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let drained: Vec<_> = state.failures.drain(..).collect();
        for (key, _, _) in &drained {
            state.failed_targets.remove(key);
        }
        drained.into_iter().map(|(_, _, failure)| failure).collect()
    }

    fn begin_shutdown(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.accepting = false;
        self.shared.changed.notify_all();
    }

    fn finish_shutdown(&self, deadline: Instant, lane: &str) -> io::Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        while !state.exited {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{lane} persistence worker did not flush before shutdown"),
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
                    format!("{lane} persistence worker did not flush before shutdown"),
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

    fn shutdown(&self, timeout: Duration) -> io::Result<()> {
        let deadline = shutdown_deadline(timeout)?;
        self.begin_shutdown();
        self.finish_shutdown(deadline, "ordinary")
    }
}

fn shutdown_deadline(timeout: Duration) -> io::Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "shutdown timeout is too large"))
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
            attempt,
            operation,
            estimated_bytes,
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
                attempt,
                PersistenceFailure {
                    operation,
                    error: error.to_string(),
                },
            );
            state.release_estimated_bytes(estimated_bytes);
            state.running = false;
            shared.changed.notify_all();
            continue;
        }

        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.clear_failure(&key, attempt);
        state.release_estimated_bytes(estimated_bytes);
        state.running = false;
        shared.changed.notify_all();
    }
}

struct PersistenceWorkers {
    ordinary: PersistenceWorker,
    session: PersistenceWorker,
}

impl PersistenceWorkers {
    fn new(ordinary_capacity: usize, session_capacity: usize) -> io::Result<Self> {
        let ordinary = PersistenceWorker::new_named(ordinary_capacity, "anvil-persistence")?;
        let session =
            match PersistenceWorker::new_named(session_capacity, "anvil-session-persistence") {
                Ok(worker) => worker,
                Err(error) => {
                    // No task can have reached this private, not-yet-published
                    // worker, so cleanup is bounded and should complete at once.
                    let _ = ordinary.shutdown(Duration::from_secs(1));
                    return Err(error);
                }
            };
        Ok(Self { ordinary, session })
    }

    fn enqueue(
        &self,
        key: PersistenceKey,
        operation: String,
        task: PersistenceTask,
    ) -> io::Result<()> {
        self.ordinary.enqueue(key, operation, task)
    }

    fn enqueue_weighted(
        &self,
        key: PersistenceKey,
        operation: String,
        estimated_bytes: usize,
        task: PersistenceTask,
    ) -> io::Result<()> {
        self.ordinary
            .enqueue_weighted(key, operation, estimated_bytes, task)
    }

    fn enqueue_session(
        &self,
        key: PersistenceKey,
        operation: String,
        task: PersistenceTask,
    ) -> io::Result<()> {
        self.session.enqueue(key, operation, task)
    }

    fn enqueue_session_weighted(
        &self,
        key: PersistenceKey,
        operation: String,
        estimated_bytes: usize,
        task: PersistenceTask,
    ) -> io::Result<()> {
        self.session
            .enqueue_weighted(key, operation, estimated_bytes, task)
    }

    fn try_reserve_estimated_bytes(
        &self,
        estimated_bytes: usize,
    ) -> io::Result<EstimatedBytesReservation> {
        // Retained results are charged to the ordinary lane's ledger; the
        // session lane holds only its single coalescing snapshot target.
        self.ordinary.try_reserve_estimated_bytes(estimated_bytes)
    }

    fn drain_failures(&self) -> Vec<PersistenceFailure> {
        let mut failures = self.session.drain_failures();
        failures.extend(self.ordinary.drain_failures());
        failures
    }

    fn shutdown(&self, timeout: Duration) -> io::Result<()> {
        // Stop both lanes from accepting work before waiting.  Both receive
        // the same absolute deadline, so this is one total shutdown budget,
        // not `timeout` once per lane.  The session lane is joined first: a
        // stuck ordinary target can consume only the time left afterwards.
        let deadline = shutdown_deadline(timeout)?;
        self.session.begin_shutdown();
        self.ordinary.begin_shutdown();

        let session_result = self.session.finish_shutdown(deadline, "session");
        let ordinary_result = self.ordinary.finish_shutdown(deadline, "ordinary");
        session_result.and(ordinary_result)
    }
}

static PERSISTENCE_WORKERS: OnceLock<Result<PersistenceWorkers, String>> = OnceLock::new();

fn global_workers() -> io::Result<&'static PersistenceWorkers> {
    PERSISTENCE_WORKERS
        .get_or_init(|| {
            PersistenceWorkers::new(MAX_PENDING_TARGETS, MAX_PENDING_SESSION_TARGETS)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

pub(crate) fn enqueue(
    key: PersistenceKey,
    operation: impl Into<String>,
    task: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    global_workers()?.enqueue(key, operation.into(), Box::new(task))
}

/// Submit work with a conservative estimate of the memory it retains. The
/// charge remains active while the task runs and is released only after its
/// success, failure, or caught panic has been recorded.
pub(crate) fn enqueue_weighted(
    key: PersistenceKey,
    operation: impl Into<String>,
    estimated_bytes: usize,
    task: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    global_workers()?.enqueue_weighted(key, operation.into(), estimated_bytes, Box::new(task))
}

/// Try to charge a retained result which escapes its worker closure. This never
/// waits for capacity: callers on the persistence thread must shrink or drop
/// the result on `WouldBlock`, otherwise they could deadlock behind queued work
/// whose own charge is preventing admission.
pub(crate) fn try_reserve_estimated_bytes(
    estimated_bytes: usize,
) -> io::Result<EstimatedBytesReservation> {
    global_workers()?.try_reserve_estimated_bytes(estimated_bytes)
}

/// Queue the sole coalescing workspace snapshot target on a lane that cannot
/// be head-of-line blocked by ordinary history or organism persistence.
pub(crate) fn enqueue_session(
    key: PersistenceKey,
    operation: impl Into<String>,
    task: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    global_workers()?.enqueue_session(key, operation.into(), Box::new(task))
}

/// The session-lane counterpart of [`enqueue_weighted`]: the one pending
/// workspace snapshot is still charged against that lane's byte budget.
pub(crate) fn enqueue_session_weighted(
    key: PersistenceKey,
    operation: impl Into<String>,
    estimated_bytes: usize,
    task: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    global_workers()?.enqueue_session_weighted(
        key,
        operation.into(),
        estimated_bytes,
        Box::new(task),
    )
}

pub(crate) fn drain_failures() -> Vec<PersistenceFailure> {
    match PERSISTENCE_WORKERS.get() {
        Some(Ok(workers)) => workers.drain_failures(),
        Some(Err(error)) => vec![PersistenceFailure {
            operation: "start background persistence".to_string(),
            error: error.clone(),
        }],
        None => Vec::new(),
    }
}

pub(crate) fn shutdown(timeout: Duration) -> io::Result<()> {
    match PERSISTENCE_WORKERS.get() {
        Some(Ok(workers)) => workers.shutdown(timeout),
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

    fn retained_estimated_bytes(worker: &PersistenceWorker) -> usize {
        worker
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retained_estimated_bytes
    }

    #[test]
    fn estimated_byte_budget_accepts_exact_limit_and_rejects_limit_plus_one() {
        let worker = PersistenceWorker::new_with_limits(4, 10).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        worker
            .enqueue_weighted(
                PersistenceKey::named("exact"),
                "save exact".into(),
                10,
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 10);

        let error = worker
            .enqueue_weighted(
                PersistenceKey::named("overflow"),
                "save overflow".into(),
                1,
                Box::new(|| Ok(())),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(retained_estimated_bytes(&worker), 10);

        release_tx.send(()).unwrap();
        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 0);
    }

    #[test]
    fn weighted_replacement_updates_accounting_without_dropping_previous_on_rejection() {
        let worker = PersistenceWorker::new_with_limits(4, 10).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        worker
            .enqueue(
                PersistenceKey::named("blocker"),
                "block worker".into(),
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let writes = Arc::new(Mutex::new(Vec::new()));
        for (estimated_bytes, value) in [(6, 6), (4, 4), (10, 10)] {
            let writes = Arc::clone(&writes);
            worker
                .enqueue_weighted(
                    PersistenceKey::named("snapshot"),
                    "save snapshot".into(),
                    estimated_bytes,
                    Box::new(move || {
                        writes.lock().unwrap().push(value);
                        Ok(())
                    }),
                )
                .unwrap();
            assert_eq!(retained_estimated_bytes(&worker), estimated_bytes);
        }

        let rejected_writes = Arc::clone(&writes);
        let error = worker
            .enqueue_weighted(
                PersistenceKey::named("snapshot"),
                "save snapshot".into(),
                11,
                Box::new(move || {
                    rejected_writes.lock().unwrap().push(11);
                    Ok(())
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(retained_estimated_bytes(&worker), 10);

        release_tx.send(()).unwrap();
        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(*writes.lock().unwrap(), [10]);
        assert_eq!(retained_estimated_bytes(&worker), 0);
        let failures = worker.drain_failures();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].error.contains("estimated-byte budget exceeded"));
    }

    #[test]
    fn replacing_task_drops_reentrant_result_permit_outside_worker_lock() {
        let worker = PersistenceWorker::new_with_limits(4, 10).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        worker
            .enqueue(
                PersistenceKey::named("blocker"),
                "block worker".into(),
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let reservation = worker.try_reserve_estimated_bytes(3).unwrap();
        worker
            .enqueue(
                PersistenceKey::named("replace-me"),
                "old load result".into(),
                Box::new(move || {
                    drop(reservation);
                    Ok(())
                }),
            )
            .unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 3);

        // Replacing the pending closure drops its captured reservation. Its
        // destructor locks WorkerState, so this call deadlocked before the old
        // PendingJob was moved out of the mutex guard.
        worker
            .enqueue(
                PersistenceKey::named("replace-me"),
                "new load result".into(),
                Box::new(|| Ok(())),
            )
            .unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 0);

        release_tx.send(()).unwrap();
        worker.shutdown(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn running_and_pending_jobs_share_the_estimated_byte_budget() {
        let worker = PersistenceWorker::new_with_limits(4, 10).unwrap();
        let (first_started_tx, first_started_rx) = mpsc::sync_channel(0);
        let (first_release_tx, first_release_rx) = mpsc::sync_channel(0);
        worker
            .enqueue_weighted(
                PersistenceKey::named("first"),
                "save first".into(),
                4,
                Box::new(move || {
                    first_started_tx.send(()).unwrap();
                    first_release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        first_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let (second_started_tx, second_started_rx) = mpsc::sync_channel(0);
        let (second_release_tx, second_release_rx) = mpsc::sync_channel(0);
        worker
            .enqueue_weighted(
                PersistenceKey::named("second"),
                "save second".into(),
                6,
                Box::new(move || {
                    second_started_tx.send(()).unwrap();
                    second_release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 10);

        let error = worker
            .enqueue_weighted(
                PersistenceKey::named("third"),
                "save third".into(),
                1,
                Box::new(|| Ok(())),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(retained_estimated_bytes(&worker), 10);

        first_release_tx.send(()).unwrap();
        second_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 6);
        worker
            .enqueue_weighted(
                PersistenceKey::named("third"),
                "save third".into(),
                4,
                Box::new(|| Ok(())),
            )
            .unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 10);

        second_release_tx.send(()).unwrap();
        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(retained_estimated_bytes(&worker), 0);
    }

    #[test]
    fn retained_result_reservation_stays_charged_until_drop() {
        let worker = PersistenceWorker::new_with_limits(4, 10).unwrap();
        let first = worker.try_reserve_estimated_bytes(6).unwrap();
        let second = worker.try_reserve_estimated_bytes(4).unwrap();
        assert_eq!(first.estimated_bytes(), 6);
        assert_eq!(retained_estimated_bytes(&worker), 10);

        let error = match worker.try_reserve_estimated_bytes(1) {
            Ok(_) => panic!("reservation above the exact limit unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(retained_estimated_bytes(&worker), 10);

        drop(first);
        assert_eq!(retained_estimated_bytes(&worker), 4);
        drop(second);
        assert_eq!(retained_estimated_bytes(&worker), 0);
        worker.shutdown(Duration::from_secs(1)).unwrap();
    }

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
    fn a_success_clears_an_undrained_failure_for_the_same_target() {
        let worker = PersistenceWorker::new(2).unwrap();
        let (failing_tx, failing_rx) = mpsc::sync_channel(0);
        let key = PersistenceKey::named("recovering");
        worker
            .enqueue(
                key.clone(),
                "save recovering target".into(),
                Box::new(move || {
                    failing_tx.send(()).unwrap();
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "offline"))
                }),
            )
            .unwrap();
        failing_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker
            .enqueue(key, "save recovering target".into(), Box::new(|| Ok(())))
            .unwrap();

        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert!(worker.drain_failures().is_empty());
    }

    #[test]
    fn a_failure_can_be_reported_again_after_the_previous_one_was_drained() {
        let worker = PersistenceWorker::new(2).unwrap();
        let key = PersistenceKey::named("still-broken");
        worker
            .enqueue(
                key.clone(),
                "save broken target".into(),
                Box::new(|| Err(io::Error::new(io::ErrorKind::PermissionDenied, "first"))),
            )
            .unwrap();
        let (barrier_tx, barrier_rx) = mpsc::sync_channel(0);
        worker
            .enqueue(
                PersistenceKey::named("barrier"),
                "barrier".into(),
                Box::new(move || {
                    barrier_tx.send(()).unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        barrier_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_eq!(worker.drain_failures()[0].error, "first");
        worker
            .enqueue(
                key,
                "save broken target".into(),
                Box::new(|| Err(io::Error::new(io::ErrorKind::PermissionDenied, "second"))),
            )
            .unwrap();

        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(worker.drain_failures()[0].error, "second");
    }

    #[test]
    fn an_older_in_flight_success_does_not_hide_a_newer_rejected_attempt() {
        let worker = PersistenceWorker::new(1).unwrap();
        let key = PersistenceKey::named("newest-must-win");
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        worker
            .enqueue(
                key.clone(),
                "save older snapshot".into(),
                Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let timeout = worker.shutdown(Duration::from_millis(10)).unwrap_err();
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        let rejected = worker
            .enqueue(key, "save newest snapshot".into(), Box::new(|| Ok(())))
            .unwrap_err();
        assert_eq!(rejected.kind(), io::ErrorKind::BrokenPipe);

        release_tx.send(()).unwrap();
        worker.shutdown(Duration::from_secs(1)).unwrap();
        assert_eq!(
            worker.drain_failures(),
            [PersistenceFailure {
                operation: "save newest snapshot".into(),
                error: "persistence worker is shutting down".into(),
            }]
        );
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

    #[test]
    fn session_lane_flushes_latest_snapshot_while_ordinary_lane_is_blocked() {
        let workers = PersistenceWorkers::new(1, 1).unwrap();
        let (ordinary_started_tx, ordinary_started_rx) = mpsc::sync_channel(0);
        let (ordinary_release_tx, ordinary_release_rx) = mpsc::sync_channel(0);
        workers
            .enqueue(
                PersistenceKey::named("organism"),
                "save organism memory".into(),
                Box::new(move || {
                    ordinary_started_tx.send(()).unwrap();
                    ordinary_release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        ordinary_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let session_key = PersistenceKey::named("session");
        let (session_started_tx, session_started_rx) = mpsc::sync_channel(0);
        let (session_release_tx, session_release_rx) = mpsc::sync_channel(0);
        workers
            .enqueue_session(
                session_key.clone(),
                "save first session snapshot".into(),
                Box::new(move || {
                    session_started_tx.send(()).unwrap();
                    session_release_rx.recv().unwrap();
                    Ok(())
                }),
            )
            .unwrap();
        // Reaching this barrier while the ordinary job is still blocked is a
        // deterministic proof that the session job has its own worker lane.
        session_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let (latest_flushed_tx, latest_flushed_rx) = mpsc::sync_channel(0);
        workers
            .enqueue_session(
                session_key,
                "save final session snapshot".into(),
                Box::new(move || {
                    latest_flushed_tx.send(()).unwrap();
                    Ok(())
                }),
            )
            .unwrap();

        // Model force_quit having stopped both queues. Pending jobs must still
        // drain, with the session lane completing before the ordinary blocker.
        workers.session.begin_shutdown();
        workers.ordinary.begin_shutdown();
        thread::scope(|scope| {
            let shutdown = scope.spawn(|| workers.shutdown(Duration::from_secs(1)));
            session_release_tx.send(()).unwrap();
            latest_flushed_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            ordinary_release_tx.send(()).unwrap();
            shutdown.join().unwrap().unwrap();
        });
    }

    #[test]
    fn failures_are_drained_from_both_persistence_lanes() {
        let workers = PersistenceWorkers::new(1, 1).unwrap();
        workers
            .enqueue(
                PersistenceKey::named("ordinary-failure"),
                "save ordinary target".into(),
                Box::new(|| Err(io::Error::other("ordinary failed"))),
            )
            .unwrap();
        workers
            .enqueue_session(
                PersistenceKey::named("session-failure"),
                "save session target".into(),
                Box::new(|| Err(io::Error::other("session failed"))),
            )
            .unwrap();
        workers.shutdown(Duration::from_secs(1)).unwrap();

        let failures = workers.drain_failures();
        assert_eq!(failures.len(), 2);
        assert!(failures
            .iter()
            .any(|failure| failure.error == "ordinary failed"));
        assert!(failures
            .iter()
            .any(|failure| failure.error == "session failed"));
    }
}
