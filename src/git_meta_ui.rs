//! Non-blocking GTK-side cache for the shared Git metadata service.
//!
//! `jterm_core::git_meta::read` already runs Git in a bounded worker, but waits
//! briefly for that worker so command-line/background callers can receive a
//! fresh answer. A 12ms wait is still most of a 60Hz frame. This adapter adds a
//! second, tiny cache boundary: GTK reads the last completed value immediately,
//! while one app worker performs the possibly-waiting shared call.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;

use jterm_core::git_meta::RepoMeta;

const MAX_GIT_CWD_BYTES: usize = 16 * 1024;
const MAX_QUEUED_PROBES: usize = 64;
const MAX_CACHE_ENTRIES: usize = 256;

type ProbeResult = Option<RepoMeta>;

struct UiGitMetaService {
    request_tx: mpsc::SyncSender<PathBuf>,
    cache: Arc<Mutex<HashMap<PathBuf, ProbeResult>>>,
    pending: Arc<Mutex<HashSet<PathBuf>>>,
}

impl UiGitMetaService {
    fn new() -> Option<Self> {
        let (request_tx, request_rx) = mpsc::sync_channel(MAX_QUEUED_PROBES);
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let pending = Arc::new(Mutex::new(HashSet::new()));
        let worker_cache = cache.clone();
        let worker_pending = pending.clone();
        thread::Builder::new()
            .name("anvil-ui-git-meta".to_string())
            .spawn(move || worker_loop(request_rx, &worker_cache, &worker_pending))
            .ok()?;
        Some(Self {
            request_tx,
            cache,
            pending,
        })
    }

    fn cached(&self, path: &Path) -> Option<ProbeResult> {
        self.cache.lock().ok()?.get(path).cloned()
    }

    fn request(&self, path: &Path) -> bool {
        let path = path.to_path_buf();
        {
            let Ok(mut pending) = self.pending.lock() else {
                return false;
            };
            if !pending.insert(path.clone()) {
                return true;
            }
        }
        if self.request_tx.try_send(path.clone()).is_ok() {
            return true;
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&path);
        }
        false
    }
}

fn worker_loop(
    requests: mpsc::Receiver<PathBuf>,
    cache: &Mutex<HashMap<PathBuf, ProbeResult>>,
    pending: &Mutex<HashSet<PathBuf>>,
) {
    for path in requests {
        let result = jterm_core::git_meta::read(&path);
        if let Ok(mut cache) = cache.lock() {
            if !cache.contains_key(&path) && cache.len() >= MAX_CACHE_ENTRIES {
                if let Some(evicted) = cache.keys().next().cloned() {
                    cache.remove(&evicted);
                }
            }
            cache.insert(path.clone(), result);
        }
        if let Ok(mut pending) = pending.lock() {
            pending.remove(&path);
        }
    }
}

fn service() -> Option<&'static UiGitMetaService> {
    static SERVICE: OnceLock<Option<UiGitMetaService>> = OnceLock::new();
    SERVICE.get_or_init(UiGitMetaService::new).as_ref()
}

fn cwd_key_is_bounded(cwd: &Path) -> bool {
    let bytes = cwd.as_os_str().as_encoded_bytes();
    bytes.len() <= MAX_GIT_CWD_BYTES && !bytes.contains(&0)
}

/// Return cached metadata immediately and coalesce a background refresh.
pub(crate) fn read_cached_and_refresh(cwd: &Path) -> Option<RepoMeta> {
    if !cwd_key_is_bounded(cwd) {
        return None;
    }
    let service = service()?;
    let cached = service.cached(cwd).flatten();
    let _ = service.request(cwd);
    cached
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_requests_coalesce_without_waiting_for_a_reply() {
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let service = UiGitMetaService {
            request_tx,
            cache: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashSet::new())),
        };
        let path = Path::new("/work/repo");

        assert!(service.request(path));
        assert!(service.request(path));
        assert_eq!(request_rx.try_iter().count(), 1);
        assert_eq!(service.pending.lock().unwrap().len(), 1);
    }

    #[test]
    fn cache_keys_are_bounded_before_queueing() {
        assert!(cwd_key_is_bounded(Path::new("/work/repo")));
        assert!(!cwd_key_is_bounded(Path::new(
            &"x".repeat(MAX_GIT_CWD_BYTES + 1)
        )));
    }
}
