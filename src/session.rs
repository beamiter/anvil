//! Session persistence for jterm1 windows.
//!
//! Each tab stores its title, whether it was user-renamed, and a `PaneLayout`
//! tree mirroring the live GTK `Paned` structure — so nested splits, each pane's
//! working directory, terminal mode and any restorable command (ssh / nix
//! develop / docker exec …) are restored.
//!
//! jterm1 is a `NON_UNIQUE` application: every launch is a separate process.
//! A single `tabs.state` therefore lets unrelated windows overwrite each other.
//! New snapshots use `tabs.<pid>.state`. On startup we ignore files whose owner
//! is still alive, then atomically claim and consume the newest valid snapshot
//! left by an exited process. The old `tabs.state` name remains readable.

use gtk::glib;
use relm4::gtk;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LEGACY_STATE_FILE: &str = "tabs.state";
const STATE_PREFIX: &str = "tabs.";
const STATE_SUFFIX: &str = ".state";
const CLAIM_MARKER: &str = ".claim.";

/// One node of a tab's pane tree: either a terminal leaf or a split of two
/// subtrees. Mirrors jterm4's `PaneLayout`.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum PaneLayout {
    Leaf {
        /// Legacy pane backend recorded by older snapshots.  Restores use the
        /// current `terminal_mode` configuration instead, so changing the
        /// configuration takes effect on the next launch.
        mode: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Restorable command to replay on restore (e.g. "ssh host").
        #[serde(skip_serializing_if = "Option::is_none")]
        cmds: Option<String>,
    },
    Split {
        /// 'h' = horizontal (left/right), 'v' = vertical (top/bottom).
        orientation: char,
        position: i32,
        start: Box<PaneLayout>,
        end: Box<PaneLayout>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct SavedTab {
    pub title: String,
    pub custom_title: bool,
    pub layout: PaneLayout,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct SavedSession {
    pub active: usize,
    pub tabs: Vec<SavedTab>,
}

fn state_dir() -> PathBuf {
    glib::user_config_dir().join("jterm1")
}

fn state_file_path_in(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("tabs.{pid}.state"))
}

pub(crate) fn state_file_path() -> PathBuf {
    state_file_path_in(&state_dir(), std::process::id())
}

pub(crate) fn save_session(session: &SavedSession) {
    let path = state_file_path();
    if session.tabs.is_empty() {
        // Do not leave an older non-empty snapshot behind after this window's
        // last tab is closed. A missing per-process file means "start fresh".
        if let Err(err) = fs::remove_file(&path) {
            if err.kind() != io::ErrorKind::NotFound {
                log::error!(
                    "Failed to remove empty session snapshot {}: {err}",
                    path.display()
                );
            }
        }
        return;
    }

    let payload = match serde_json::to_string(session) {
        Ok(payload) => payload,
        Err(err) => {
            log::error!(
                "Failed to serialize session snapshot {}: {err}",
                path.display()
            );
            return;
        }
    };
    if let Err(err) = atomic_write(&path, payload.as_bytes()) {
        log::error!(
            "Failed to atomically save session snapshot {}: {err}",
            path.display()
        );
    }
}

/// Load and consume the newest valid snapshot whose owning process has exited.
/// Corrupt/unreadable files are deliberately retained for inspection/recovery.
pub(crate) fn load_session() -> Option<SavedSession> {
    load_session_from(&state_dir(), std::process::id(), &process_is_alive)
}

/// Write to a sibling temp file, fsync it, then replace the destination with a
/// single rename. The previous valid snapshot is never removed first, so a
/// write/rename failure leaves it intact.
fn atomic_write(path: &Path, payload: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    fs::create_dir_all(parent)?;

    let tmp = path.with_extension("state.tmp");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(payload)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;

    // Persist the renamed directory entry where supported. The snapshot is
    // already valid if this best-effort durability step fails.
    if let Err(err) = fs::File::open(parent).and_then(|dir| dir.sync_all()) {
        log::warn!(
            "Session snapshot {} was saved, but syncing directory {} failed: {err}",
            path.display(),
            parent.display()
        );
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StateFileName {
    /// Canonical pre-claim filename (`tabs.<pid>.state` or legacy `tabs.state`).
    base_name: String,
    /// Process that wrote the snapshot. Legacy files have no owner metadata.
    owner_pid: Option<u32>,
    /// Process that claimed this file but exited before consuming it.
    claimer_pid: Option<u32>,
}

fn parse_unclaimed_name(name: &str) -> Option<(String, Option<u32>)> {
    if name == LEGACY_STATE_FILE {
        return Some((name.to_string(), None));
    }
    let pid = name
        .strip_prefix(STATE_PREFIX)?
        .strip_suffix(STATE_SUFFIX)?
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)?;
    Some((name.to_string(), Some(pid)))
}

fn parse_state_file_name(name: &str) -> Option<StateFileName> {
    if let Some((base, claimer)) = name.rsplit_once(CLAIM_MARKER) {
        let claimer_pid = claimer.parse::<u32>().ok().filter(|pid| *pid > 0)?;
        let (base_name, owner_pid) = parse_unclaimed_name(base)?;
        return Some(StateFileName {
            base_name,
            owner_pid,
            claimer_pid: Some(claimer_pid),
        });
    }
    let (base_name, owner_pid) = parse_unclaimed_name(name)?;
    Some(StateFileName {
        base_name,
        owner_pid,
        claimer_pid: None,
    })
}

fn state_file_is_recoverable(
    file: &StateFileName,
    current_pid: u32,
    is_alive: &dyn Fn(u32) -> bool,
) -> bool {
    if let Some(claimer) = file.claimer_pid {
        return claimer != current_pid && !is_alive(claimer);
    }
    match file.owner_pid {
        Some(owner) => owner != current_pid && !is_alive(owner),
        // Compatibility with the old `tabs.state` format. It has no owner PID,
        // so there is no live process identity to exclude.
        None => true,
    }
}

#[derive(Debug)]
struct SessionCandidate {
    path: PathBuf,
    file_name: StateFileName,
    modified: SystemTime,
    session: SavedSession,
}

fn sort_candidates_newest_first(candidates: &mut [SessionCandidate]) {
    candidates.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.path.file_name().cmp(&a.path.file_name()))
    });
}

fn scan_candidates(
    dir: &Path,
    current_pid: u32,
    is_alive: &dyn Fn(u32) -> bool,
) -> Vec<SessionCandidate> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            log::error!("Failed to list session state dir {}: {err}", dir.display());
            return Vec::new();
        }
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                log::warn!("Failed to inspect an entry in {}: {err}", dir.display());
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(file_name) = parse_state_file_name(&name) else {
            continue;
        };
        if !state_file_is_recoverable(&file_name, current_pid, is_alive) {
            log::debug!(
                "Leaving live session snapshot {} untouched",
                entry.path().display()
            );
            continue;
        }

        let path = entry.path();
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) => {
                log::error!(
                    "Cannot read recoverable session snapshot {}: {err}; file retained",
                    path.display()
                );
                continue;
            }
        };
        let session = match serde_json::from_str::<SavedSession>(&contents) {
            Ok(session) if !session.tabs.is_empty() => session,
            Ok(_) => {
                log::warn!(
                    "Session snapshot {} contains no tabs; file retained",
                    path.display()
                );
                continue;
            }
            Err(err) => {
                log::error!(
                    "Corrupt session snapshot {}: {err}; file retained",
                    path.display()
                );
                continue;
            }
        };
        let modified = match entry.metadata().and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(err) => {
                log::warn!(
                    "Cannot read modification time for session snapshot {}: {err}; treating it as oldest",
                    path.display()
                );
                UNIX_EPOCH
            }
        };
        candidates.push(SessionCandidate {
            path,
            file_name,
            modified,
            session,
        });
    }

    sort_candidates_newest_first(&mut candidates);
    candidates
}

fn load_session_from(
    dir: &Path,
    current_pid: u32,
    is_alive: &dyn Fn(u32) -> bool,
) -> Option<SavedSession> {
    // A competing startup can win the rename between our scan and claim. Retry
    // so simultaneous launches can each recover a different exited window
    // without ever consuming the same snapshot.
    for _ in 0..8 {
        let candidate = scan_candidates(dir, current_pid, is_alive)
            .into_iter()
            .next()?;
        let claim_path = dir.join(format!(
            "{}{}{}",
            candidate.file_name.base_name, CLAIM_MARKER, current_pid
        ));
        // PIDs are unique among live processes, so no legitimate concurrent
        // loader can own our claim name. Refuse to let rename replace a stale
        // same-PID claim (possible after PID reuse); both snapshots stay intact.
        if claim_path.exists() {
            log::error!(
                "Cannot claim session snapshot {}: claim path {} already exists; both files retained",
                candidate.path.display(),
                claim_path.display()
            );
            return None;
        }
        match fs::rename(&candidate.path, &claim_path) {
            Ok(()) => {
                if let Err(err) = fs::remove_file(&claim_path) {
                    log::error!(
                        "Claimed session snapshot {} as {}, but could not consume it: {err}; claimed file retained and no restore performed",
                        candidate.path.display(),
                        claim_path.display()
                    );
                    return None;
                }
                log::info!(
                    "Restored and consumed session snapshot {}",
                    candidate.path.display()
                );
                return Some(candidate.session);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                log::debug!(
                    "Session snapshot {} was claimed by another process; rescanning",
                    candidate.path.display()
                );
            }
            Err(err) => {
                log::error!(
                    "Failed to claim newest session snapshot {} as {}: {err}; file retained",
                    candidate.path.display(),
                    claim_path.display()
                );
                return None;
            }
        }
    }
    log::warn!("Session restore gave up after repeated concurrent claims");
    None
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let rc = unsafe { nix::libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(nix::libc::EPERM)
    )
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // jterm1 currently targets Unix PTYs. If that changes, treating an unknown
    // owner as live is safer than stealing another window's snapshot.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm1-session-{label}-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test state dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn saved_session(title: &str) -> SavedSession {
        SavedSession {
            active: 0,
            tabs: vec![SavedTab {
                title: title.to_string(),
                custom_title: true,
                layout: PaneLayout::Leaf {
                    mode: "block".to_string(),
                    cwd: None,
                    cmds: None,
                },
            }],
        }
    }

    fn write_session(path: &Path, title: &str) {
        let payload = serde_json::to_vec(&saved_session(title)).expect("serialize test session");
        atomic_write(path, &payload).expect("write test session");
    }

    #[test]
    fn parses_process_legacy_and_orphan_claim_names() {
        assert_eq!(
            parse_state_file_name("tabs.state"),
            Some(StateFileName {
                base_name: "tabs.state".to_string(),
                owner_pid: None,
                claimer_pid: None,
            })
        );
        assert_eq!(
            parse_state_file_name("tabs.42.state"),
            Some(StateFileName {
                base_name: "tabs.42.state".to_string(),
                owner_pid: Some(42),
                claimer_pid: None,
            })
        );
        assert_eq!(
            parse_state_file_name("tabs.42.state.claim.77"),
            Some(StateFileName {
                base_name: "tabs.42.state".to_string(),
                owner_pid: Some(42),
                claimer_pid: Some(77),
            })
        );
        assert!(parse_state_file_name("tabs.42.state.tmp").is_none());
        assert!(parse_state_file_name("tabs.not-a-pid.state").is_none());
        assert!(parse_state_file_name("tabs.42.state.claim.77.claim.88").is_none());
    }

    #[test]
    fn recoverability_uses_owner_or_claimer_without_real_processes() {
        let owned = parse_state_file_name("tabs.10.state").unwrap();
        let claimed = parse_state_file_name("tabs.10.state.claim.20").unwrap();
        let legacy = parse_state_file_name("tabs.state").unwrap();
        let alive = |pid| pid == 10 || pid == 20;

        assert!(!state_file_is_recoverable(&owned, 99, &alive));
        assert!(!state_file_is_recoverable(&claimed, 99, &alive));
        assert!(state_file_is_recoverable(&legacy, 99, &alive));
        assert!(state_file_is_recoverable(&owned, 99, &|_| false));
        assert!(state_file_is_recoverable(&claimed, 99, &|_| false));
        assert!(!state_file_is_recoverable(&owned, 10, &|_| false));
        assert!(!state_file_is_recoverable(&claimed, 20, &|_| false));
    }

    #[test]
    fn live_process_snapshot_is_left_while_exited_snapshot_is_consumed() {
        let dir = TestDir::new("live-owner");
        let live_path = state_file_path_in(dir.path(), 10);
        let exited_path = state_file_path_in(dir.path(), 20);
        write_session(&live_path, "live");
        write_session(&exited_path, "exited");

        let restored =
            load_session_from(dir.path(), 99, &|pid| pid == 10).expect("restore exited owner");
        assert_eq!(restored.tabs[0].title, "exited");
        assert!(live_path.exists(), "live owner's file must remain");
        assert!(!exited_path.exists(), "restored file must be consumed");
    }

    #[test]
    fn corrupt_new_format_is_retained_and_legacy_state_still_restores() {
        let dir = TestDir::new("legacy-corrupt");
        let corrupt_path = state_file_path_in(dir.path(), 10);
        fs::write(&corrupt_path, "{ definitely not json").expect("write corrupt state");
        let legacy_path = dir.path().join(LEGACY_STATE_FILE);
        write_session(&legacy_path, "legacy");

        let restored =
            load_session_from(dir.path(), 99, &|_| false).expect("restore valid legacy state");
        assert_eq!(restored.tabs[0].title, "legacy");
        assert!(
            corrupt_path.exists(),
            "invalid state must be retained for diagnosis"
        );
        assert!(!legacy_path.exists(), "legacy state must be consumed once");
    }

    #[test]
    fn orphaned_claim_from_exited_loader_is_recoverable() {
        let dir = TestDir::new("orphan-claim");
        let claim_path = dir.path().join("tabs.10.state.claim.20");
        write_session(&claim_path, "orphaned claim");

        let restored =
            load_session_from(dir.path(), 99, &|_| false).expect("recover orphaned claim");
        assert_eq!(restored.tabs[0].title, "orphaned claim");
        assert!(!claim_path.exists());
    }

    #[test]
    fn newest_valid_candidate_is_selected() {
        let dir = TestDir::new("newest");
        let older = SessionCandidate {
            path: dir.path().join("tabs.1.state"),
            file_name: parse_state_file_name("tabs.1.state").unwrap(),
            modified: UNIX_EPOCH + Duration::from_secs(1),
            session: saved_session("older"),
        };
        let newer = SessionCandidate {
            path: dir.path().join("tabs.2.state"),
            file_name: parse_state_file_name("tabs.2.state").unwrap(),
            modified: UNIX_EPOCH + Duration::from_secs(2),
            session: saved_session("newer"),
        };
        let mut candidates = vec![older, newer];
        sort_candidates_newest_first(&mut candidates);
        assert_eq!(candidates[0].session.tabs[0].title, "newer");
    }
}
