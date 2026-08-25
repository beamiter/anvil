//! Session persistence for anvil windows.
//!
//! Each tab stores its title, whether it was user-renamed, and a `PaneLayout`
//! tree mirroring the live GTK `Paned` structure — so nested splits, each pane's
//! working directory, terminal mode and any restorable command (ssh / nix
//! develop / docker exec …) are restored.
//!
//! anvil is a `NON_UNIQUE` application: every launch is a separate process.
//! A single `tabs.state` therefore lets unrelated windows overwrite each other.
//! New snapshots use a random per-process token plus a companion owner lock:
//! `tabs.<token>.state` and `tabs.<token>.lock`. The process holds an exclusive
//! `flock` on that lock for its lifetime, so liveness remains correct across PID
//! reuse and PID namespaces. The old `tabs.state` and `tabs.<pid>.state` names
//! remain readable; PID-owned files are retained conservatively because their
//! owner identity cannot be proven from another namespace. New payloads use a
//! versioned envelope whose explicit empty tombstone and predecessor claim make
//! restore/close checkpoints recoverable across every rename window.

use gtk::glib;
use relm4::gtk;
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LEGACY_STATE_FILE: &str = "tabs.state";
const STATE_PREFIX: &str = "tabs.";
const STATE_SUFFIX: &str = ".state";
const LOCK_SUFFIX: &str = ".lock";
const LOCK_PROTOCOL_FILE: &str = ".session-lock-protocol";
const CLAIM_MARKER: &str = ".claim.";
const MAX_RECOVERABLE_SNAPSHOTS: usize = 32;
/// Bound one startup scan independently of directory size. The state directory
/// is private, but stale files can accumulate after repeated crashes and must
/// not turn startup into unbounded I/O or serde work.
const MAX_DIRECTORY_ENTRIES_PER_SCAN: usize = 4_096;
const MAX_CANDIDATES_PER_SCAN: usize = MAX_RECOVERABLE_SNAPSHOTS;
const MAX_CANDIDATE_BYTES_PER_SCAN: u64 = 16 * 1024 * 1024;
const MAX_CLAIM_CHAIN_DEPTH: usize = 32;
const MAX_CLAIM_CHAIN_BYTES: u64 = 16 * 1024 * 1024;
const LOCK_PROTOCOL_TIMEOUT: Duration = Duration::from_millis(500);
const LOCK_PROTOCOL_RETRY_INTERVAL: Duration = Duration::from_millis(5);
const SESSION_ENVELOPE_FORMAT: &str = "anvil-session";
const SESSION_ENVELOPE_VERSION: u8 = 1;
/// Largest session snapshot this window will read back. A snapshot is a tab list
/// with one cwd and one argv per pane — kilobytes — so anything past this is a
/// runaway writer or another program's file at a colliding name, and rejecting
/// it by size gives a better message than a JSON parse error would.
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_RESTORED_TABS: usize = 32;
pub(crate) const MAX_RESTORED_PANES_PER_TAB: usize = 16;
pub(crate) const MAX_RESTORED_PANES_TOTAL: usize = 64;
const MAX_RESTORED_TITLE_BYTES: usize = 4 * 1024;
const MAX_RESTORED_COMMAND_ARGS: usize = 256;
const MAX_RESTORED_COMMAND_ARG_BYTES: usize = 64 * 1024;
const MAX_RESTORED_COMMAND_BYTES: usize = 256 * 1024;
/// A pane tree with at most `MAX_RESTORED_PANES_PER_TAB` leaves cannot nest
/// deeper than that, so the same number bounds recursion while decoding — long
/// before `serde_json`'s own generic recursion limit would notice.
const MAX_RESTORED_LAYOUT_DEPTH: usize = MAX_RESTORED_PANES_PER_TAB;
const MAX_RESTORED_MODE_BYTES: usize = 64;
const MAX_RESTORED_CWD_BYTES: usize = 16 * 1024;
const MAX_RESTORED_REMOTE_NAME_BYTES: usize = 256;
const MAX_RESTORED_SID_BYTES: usize = crate::config::MAX_SESSION_ID_BYTES;
const MAX_RESTORED_AI_CONVERSATION_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESTORED_ENVELOPE_FORMAT_BYTES: usize = 64;
const MAX_RESTORED_SUPERSEDES_BYTES: usize = 256;
const OWNER_TOKEN_ATTEMPTS: usize = 128;
static SNAPSHOT_OWNER: OnceLock<Result<SnapshotOwner, String>> = OnceLock::new();

/// One node of a tab's pane tree: either a terminal leaf or a split of two
/// subtrees. Mirrors forge's `PaneLayout`.
/// Deliberately not `Deserialize`: [`decode_saved_session`] is the only wire
/// path, and it enforces the tab, pane, depth, field, and argv budgets while
/// decoding instead of after a whole snapshot has been allocated.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum PaneLayout {
    Leaf {
        /// Legacy pane backend recorded by older snapshots.  Restores use the
        /// current `terminal_mode` configuration instead, so changing the
        /// configuration takes effect on the next launch.
        mode: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// True when `cwd` was reported from an ssh/mosh/container namespace
        /// and must not be reused as a local spawn or file-tree directory.
        #[serde(default)]
        cwd_external: bool,
        /// Name of a currently configured managed remote. Storing only the
        /// identifier avoids persisting SSH options/commands in a mutable
        /// session file; restore re-resolves the validated live configuration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_name: Option<String>,
        /// Stable local jsh identity learned through OSC 7770.
        #[serde(skip_serializing_if = "Option::is_none")]
        sid: Option<String>,
        /// Restorable command argv to replay on restore (e.g. `["ssh", "host"]`).
        /// Keeping it structured prevents shell metacharacters inside one
        /// argument from becoming a different local command after a restart.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "jterm_core::process::deserialize_restorable_argv"
        )]
        cmds: Option<Vec<String>>,
    },
    Split {
        /// 'h' = horizontal (left/right), 'v' = vertical (top/bottom).
        orientation: char,
        position: i32,
        start: Box<PaneLayout>,
        end: Box<PaneLayout>,
    },
}

fn truncate_string_to_bytes(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn retain_bounded_string(value: Option<String>, limit: usize) -> Option<String> {
    value.filter(|text| text.len() <= limit)
}

fn restorable_command_within_limits(argv: &[String]) -> bool {
    if argv.is_empty() || argv.len() > MAX_RESTORED_COMMAND_ARGS {
        return false;
    }
    let mut total = 0usize;
    for argument in argv {
        if argument.len() > MAX_RESTORED_COMMAND_ARG_BYTES || argument.chars().any(char::is_control)
        {
            return false;
        }
        let Some(next) = total
            .checked_add(argument.len())
            .and_then(|bytes| bytes.checked_add(1))
        else {
            return false;
        };
        if next > MAX_RESTORED_COMMAND_BYTES {
            return false;
        }
        total = next;
    }
    true
}

impl PaneLayout {
    /// Build one live pane snapshot under the exact budgets enforced by the
    /// decoder. Display-only text may be shortened, while identity-bearing
    /// fields are dropped whole so truncation can never select a different
    /// directory, remote profile, session, or command.
    pub(crate) fn captured_leaf(
        mode: String,
        cwd: Option<String>,
        cwd_external: bool,
        remote_name: Option<String>,
        sid: Option<String>,
        cmds: Option<Vec<String>>,
    ) -> Self {
        Self::Leaf {
            mode: if mode.len() <= MAX_RESTORED_MODE_BYTES {
                mode
            } else {
                "block".to_string()
            },
            cwd: retain_bounded_string(cwd, MAX_RESTORED_CWD_BYTES),
            cwd_external,
            remote_name: retain_bounded_string(remote_name, MAX_RESTORED_REMOTE_NAME_BYTES),
            sid: retain_bounded_string(sid, MAX_RESTORED_SID_BYTES)
                .filter(|value| crate::config::valid_session_id(value)),
            cmds: cmds.filter(|argv| restorable_command_within_limits(argv)),
        }
    }

    pub(crate) fn empty_leaf() -> Self {
        Self::captured_leaf("block".to_string(), None, false, None, None, None)
    }
}

#[derive(Serialize, Debug, Clone)]
pub(crate) struct SavedTab {
    pub title: String,
    pub custom_title: bool,
    /// Pinned tabs stay pinned across restarts. Older snapshots predate this
    /// field and therefore restore as unpinned.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub private_title: bool,
    pub layout: PaneLayout,
}

impl SavedTab {
    pub(crate) fn captured(
        title: String,
        custom_title: bool,
        pinned: bool,
        private_title: bool,
        layout: PaneLayout,
    ) -> Self {
        Self {
            title: truncate_string_to_bytes(title, MAX_RESTORED_TITLE_BYTES),
            custom_title,
            pinned,
            private_title,
            layout,
        }
    }
}

#[derive(Serialize, Debug, Clone, Default)]
pub(crate) struct SavedSession {
    pub active: usize,
    pub tabs: Vec<SavedTab>,
    /// Bounded, versioned JSON produced by `jterm_core::ai::ConversationSnapshot`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_conversation: Option<String>,
}

impl SavedSession {
    pub(crate) fn captured(
        active: usize,
        tabs: Vec<SavedTab>,
        ai_conversation: Option<String>,
    ) -> Self {
        Self {
            active,
            tabs,
            // Semantic validation parses up to 2 MiB of JSON, so it belongs in
            // the persistence worker rather than the GTK capture path.
            ai_conversation: ai_conversation
                .filter(|encoded| encoded.len() <= MAX_RESTORED_AI_CONVERSATION_BYTES),
        }
    }
}

pub(crate) fn can_add_persisted_tab(
    current_tabs: usize,
    current_panes: usize,
    adds_pane: bool,
) -> bool {
    current_tabs < MAX_RESTORED_TABS && (!adds_pane || current_panes < MAX_RESTORED_PANES_TOTAL)
}

pub(crate) fn can_add_persisted_pane(current_tab_panes: usize, current_panes: usize) -> bool {
    current_tab_panes < MAX_RESTORED_PANES_PER_TAB && current_panes < MAX_RESTORED_PANES_TOTAL
}

/// New snapshots use a small versioned envelope. `SavedSession` itself remains
/// unchanged so bare snapshots from every previous release stay readable.
/// The envelope also carries the immediate claimed predecessor: after a crash
/// between durable checkpoint and claim cleanup, the next owner can finish the
/// chain without ever reviving an older workspace.
#[derive(Serialize)]
struct SessionEnvelope<T> {
    format: String,
    version: u8,
    payload: SessionEnvelopePayload<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "session", rename_all = "snake_case")]
enum SessionEnvelopePayload<T> {
    Workspace(T),
    Empty,
}

#[derive(Debug)]
enum SnapshotState {
    Workspace(SavedSession),
    Empty,
}

fn state_dir() -> PathBuf {
    glib::user_config_dir().join("anvil")
}

fn valid_instance_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn state_file_path_for_token(dir: &Path, token: &str) -> PathBuf {
    dir.join(format!("{STATE_PREFIX}{token}{STATE_SUFFIX}"))
}

fn lock_file_path_for_token(dir: &Path, token: &str) -> PathBuf {
    dir.join(format!("{STATE_PREFIX}{token}{LOCK_SUFFIX}"))
}

/// Path helper retained for parsing and testing snapshots written by releases
/// that used a bare PID as their owner identity.
fn state_file_path_in(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("tabs.{pid}.state"))
}

#[cfg(unix)]
fn try_lock_file_exclusive(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: `file` owns this descriptor for the duration of the call;
        // flock stores no userspace pointer.
        let result =
            unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error
            .raw_os_error()
            .is_some_and(|code| code == nix::libc::EAGAIN || code == nix::libc::EWOULDBLOCK)
        {
            return Ok(false);
        }
        return Err(error);
    }
}

#[cfg(not(unix))]
fn try_lock_file_exclusive(_file: &File) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "session owner locks require flock",
    ))
}

/// Serializes publication and removal of owner-lock pathnames. The file is
/// intentionally persistent and is never considered by orphan cleanup.
struct LockProtocolGuard {
    _directory: File,
    _file: File,
}

#[cfg(unix)]
fn unlock_file(file: &File) {
    // SAFETY: `file` owns a live descriptor. An explicit unlock matters when
    // another thread forks while this guard is held: the child inherits the
    // same open-file description, so merely closing the parent's descriptor
    // would otherwise leave the lock alive until the child execs or exits.
    let result = unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_UN) };
    if result != 0 {
        log::warn!(
            "Failed to release session lock: {}",
            io::Error::last_os_error()
        );
    }
}

impl Drop for LockProtocolGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            unlock_file(&self._file);
            unlock_file(&self._directory);
        }
    }
}

/// A short-lived flock whose logical lifetime must not be extended by a file
/// descriptor inherited across a concurrent `fork`. Closing the local `File`
/// is insufficient in that case because the child shares its open-file
/// description; `LOCK_UN` releases that shared lock at the actual boundary.
struct TemporaryExclusiveLock<'a> {
    file: &'a File,
}

impl Drop for TemporaryExclusiveLock<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        unlock_file(self.file);
    }
}

fn try_temporary_exclusive_lock(file: &File) -> io::Result<Option<TemporaryExclusiveLock<'_>>> {
    try_lock_file_exclusive(file).map(|locked| locked.then_some(TemporaryExclusiveLock { file }))
}

struct HeldRetiredOwnerLock {
    token: String,
    file: File,
}

impl Drop for HeldRetiredOwnerLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unlock_file(&self.file);
    }
}

fn ensure_regular_lock_file(file: &File, path: &Path) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session lock {} is not a regular file", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("session lock {} has multiple hard links", path.display()),
            ));
        }
        // SAFETY: `geteuid` has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "session lock {} is not owned by the current user",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

impl LockProtocolGuard {
    fn open_directory(dir: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(
                nix::libc::O_DIRECTORY
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_NONBLOCK
                    | nix::libc::O_CLOEXEC,
            );
        }
        let directory = options.open(dir)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("session state path {} is not a directory", dir.display()),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no preconditions and only reads process state.
            if metadata.uid() != unsafe { nix::libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "session state directory {} is not owned by the current user",
                        dir.display()
                    ),
                ));
            }
        }
        Ok(directory)
    }

    fn open_file(dir: &Path) -> io::Result<File> {
        let path = dir.join(LOCK_PROTOCOL_FILE);
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
        }
        let file = options.open(&path)?;
        ensure_regular_lock_file(&file, &path)?;
        Ok(file)
    }

    fn from_locked_files(directory: File, file: File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                unlock_file(&file);
                unlock_file(&directory);
                return Err(error);
            }
        }
        Ok(Self {
            _directory: directory,
            _file: file,
        })
    }

    fn acquire(dir: &Path) -> io::Result<Self> {
        Self::acquire_with_timeout(dir, LOCK_PROTOCOL_TIMEOUT)
    }

    fn acquire_with_timeout(dir: &Path, timeout: Duration) -> io::Result<Self> {
        let started = Instant::now();
        let wait = |file: &File| -> io::Result<()> {
            loop {
                if try_lock_file_exclusive(file)? {
                    return Ok(());
                }
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out after {} ms waiting for session lock protocol in {}",
                            timeout.as_millis(),
                            dir.display()
                        ),
                    ));
                }
                std::thread::sleep(LOCK_PROTOCOL_RETRY_INTERVAL.min(timeout - elapsed));
            }
        };

        let directory = Self::open_directory(dir)?;
        wait(&directory)?;
        let file = match Self::open_file(dir) {
            Ok(file) => file,
            Err(error) => {
                unlock_file(&directory);
                return Err(error);
            }
        };
        if let Err(error) = wait(&file) {
            unlock_file(&directory);
            return Err(error);
        }
        Self::from_locked_files(directory, file)
    }

    fn try_acquire(dir: &Path) -> io::Result<Option<Self>> {
        let directory = Self::open_directory(dir)?;
        if !try_lock_file_exclusive(&directory)? {
            return Ok(None);
        }
        let file = match Self::open_file(dir) {
            Ok(file) => file,
            Err(error) => {
                unlock_file(&directory);
                return Err(error);
            }
        };
        match try_lock_file_exclusive(&file) {
            Ok(true) => {}
            Ok(false) => {
                unlock_file(&directory);
                return Ok(None);
            }
            Err(error) => {
                unlock_file(&directory);
                return Err(error);
            }
        }
        Self::from_locked_files(directory, file).map(Some)
    }
}

fn create_owner_lock(path: &Path, before_flock: &mut dyn FnMut(&Path)) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    ensure_regular_lock_file(&file, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    before_flock(path);
    if !try_lock_file_exclusive(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "new session owner lock is unexpectedly held: {}",
                path.display()
            ),
        ));
    }
    Ok(file)
}

struct SnapshotOwner {
    token: String,
    state_path: PathBuf,
    /// A recovered snapshot remains at its claimed name until this owner has
    /// durably checkpointed the restored workspace. If the process exits or a
    /// checkpoint fails first, the claim becomes recoverable when this lock is
    /// released instead of losing the only last-good copy.
    pending_restore: Mutex<Option<PendingRestoreClaim>>,
    /// Kept open for the process lifetime. Do not unlink this path while the
    /// lock is held: another process could create a new inode under the same
    /// name and incorrectly appear to own the token.
    _lock_file: File,
}

impl Drop for SnapshotOwner {
    fn drop(&mut self) {
        #[cfg(unix)]
        unlock_file(&self._lock_file);
    }
}

impl SnapshotOwner {
    fn create_in(dir: &Path) -> io::Result<Self> {
        Self::create_in_with_publish_hook(dir, &mut |_| {})
    }

    fn create_in_with_publish_hook(
        dir: &Path,
        before_flock: &mut dyn FnMut(&Path),
    ) -> io::Result<Self> {
        ensure_private_directory(dir)?;
        // Holding this guard from before the final pathname is created until
        // after its flock is acquired prevents orphan cleanup from observing
        // an unlocked, partially published owner lock.
        let _protocol = LockProtocolGuard::acquire(dir)?;
        for _ in 0..OWNER_TOKEN_ATTEMPTS {
            let token = glib::uuid_string_random().to_string();
            debug_assert!(valid_instance_token(&token));
            let state_path = state_file_path_for_token(dir, &token);
            let lock_path = lock_file_path_for_token(dir, &token);
            let lock_file = match create_owner_lock(&lock_path, before_flock) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };

            // `create_new` prevents another live owner from sharing this token.
            // Also reject a state/claim whose companion lock was externally
            // deleted, so even a theoretical UUID collision cannot overwrite it.
            if token_referenced_by_state_files(dir, &token)? {
                drop(lock_file);
                fs::remove_file(&lock_path)?;
                continue;
            }
            let owner = Self {
                token,
                state_path,
                pending_restore: Mutex::new(None),
                _lock_file: lock_file,
            };
            // End the publication transaction before the owner escapes to its
            // caller. Keeping this boundary explicit also prevents later
            // refactors from accidentally extending the directory-wide lock
            // across unrelated snapshot probes.
            drop(_protocol);
            return Ok(owner);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique session snapshot owner token",
        ))
    }
}

fn snapshot_owner() -> io::Result<&'static SnapshotOwner> {
    SNAPSHOT_OWNER
        .get_or_init(|| SnapshotOwner::create_in(&state_dir()).map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

pub(crate) fn state_file_path() -> io::Result<PathBuf> {
    snapshot_owner().map(|owner| owner.state_path.clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenLockState {
    Held,
    Available,
    Missing,
    Unknown,
}

fn token_lock_state_in(dir: &Path, token: &str) -> TokenLockState {
    // Use the same protocol as publishers and removers so the final pathname
    // can never be observed between create_new and its lifetime flock.
    let _protocol = match LockProtocolGuard::try_acquire(dir) {
        Ok(Some(protocol)) => protocol,
        Ok(None) => return TokenLockState::Unknown,
        Err(error) => {
            log::warn!(
                "Cannot enter session owner lock protocol in {}: {error}",
                dir.display()
            );
            return TokenLockState::Unknown;
        }
    };
    let path = lock_file_path_for_token(dir, token);
    let file = match open_existing_lock_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return TokenLockState::Missing,
        Err(error) => {
            log::warn!(
                "Cannot inspect session owner lock {}: {error}",
                path.display()
            );
            return TokenLockState::Unknown;
        }
    };
    let state = match try_temporary_exclusive_lock(&file) {
        Ok(Some(_lock)) => TokenLockState::Available,
        Ok(None) => TokenLockState::Held,
        Err(error) => {
            log::warn!(
                "Cannot probe session owner lock {}: {error}",
                path.display()
            );
            TokenLockState::Unknown
        }
    };
    state
}

/// Open a published lock without following a replaced pathname, and ensure a
/// concurrently forked shell cannot inherit the lock across `exec`. Without
/// `O_CLOEXEC`, a child process can keep a window snapshot looking live after
/// its actual owner exits, or hold the directory-wide publication protocol for
/// the lifetime of an unrelated command.
fn open_existing_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    ensure_regular_lock_file(&file, path)?;
    Ok(file)
}

/// The pinned shared-core revision predates its no-follow snapshot opener, so
/// enforce the complete persisted-file contract locally until the next core
/// release is available to pin: bounded, regular, singly linked, current-user,
/// nonblocking, and close-on-exec.
fn read_snapshot_bounded_to(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session snapshot {} is not a regular file", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "session snapshot {} has multiple hard links",
                    path.display()
                ),
            ));
        }
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "session snapshot {} is not owned by the current user",
                    path.display()
                ),
            ));
        }
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!(
                "session snapshot {} exceeds {} bytes",
                path.display(),
                max_bytes
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!(
                "session snapshot {} exceeds {} bytes",
                path.display(),
                max_bytes
            ),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("session snapshot {} is not valid UTF-8", path.display()),
        )
    })
}

#[cfg(test)]
fn read_snapshot_bounded(path: &Path) -> io::Result<String> {
    read_snapshot_bounded_to(path, MAX_SNAPSHOT_BYTES)
}

// ---------------------------------------------------------------------------
// Bounded snapshot decoding
// ---------------------------------------------------------------------------

/// Pane budget shared by every seed decoding one snapshot.
///
/// The file is already capped at `MAX_SNAPSHOT_BYTES`, but 4 MiB describes
/// thousands of tabs, a pane tree nested as deep as the JSON parser allows, or
/// a handful of near-file-sized strings. Ordinary Serde deserialization builds
/// all of that and only then meets `session_within_restore_limits`, so the
/// seeds below charge as they decode and stop at the first value that does not
/// fit. The post-decode audit stays as the semantic backstop.
struct RestoreBudget {
    remaining_panes: usize,
}

/// Decode one string, rejecting it before it is owned if it does not fit.
struct BoundedText {
    field: &'static str,
    limit: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for BoundedText {
    type Value = String;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> serde::de::Visitor<'de> for BoundedText {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "a '{}' string of at most {} bytes",
            self.field, self.limit
        )
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value.len() > self.limit {
            return Err(E::custom(format_args!(
                "'{}' exceeds its {}-byte restore limit",
                self.field, self.limit
            )));
        }
        Ok(value.to_owned())
    }
}

/// `Option<String>` with the same bound, for nullable fields.
struct BoundedOptionalText(BoundedText);

impl<'de> serde::de::DeserializeSeed<'de> for BoundedOptionalText {
    type Value = Option<String>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> serde::de::Visitor<'de> for BoundedOptionalText {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.expecting(formatter)
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        serde::de::DeserializeSeed::deserialize(self.0, deserializer).map(Some)
    }
}

/// The upstream argv decoder is already counted, byte-bounded, and
/// spoofing-aware, and it degrades an unusable argv to `None` rather than
/// failing the whole restore. Reuse it verbatim instead of writing a second,
/// subtly different rule.
struct RestorableArgvSeed;

impl<'de> serde::de::DeserializeSeed<'de> for RestorableArgvSeed {
    type Value = Option<Vec<String>>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        jterm_core::process::deserialize_restorable_argv(deserializer)
    }
}

struct PaneLayoutSeed<'a> {
    budget: &'a mut RestoreBudget,
    /// Panes still allowed in the tab being decoded.
    tab_panes: &'a mut usize,
    depth: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for PaneLayoutSeed<'_> {
    type Value = PaneLayout;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for PaneLayoutSeed<'_> {
    type Value = PaneLayout;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a pane layout node")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;

        let Self {
            budget,
            tab_panes,
            depth,
        } = self;
        if depth == 0 {
            return Err(A::Error::custom(format_args!(
                "pane layout nests deeper than {MAX_RESTORED_LAYOUT_DEPTH} levels"
            )));
        }

        // The variants' field sets are disjoint, so every field can be decoded
        // as it arrives and the tag resolved at the end. That keeps the
        // internally tagged shape readable in any key order without buffering
        // the node the way a derived implementation would.
        let mut kind: Option<String> = None;
        let mut mode = None;
        let mut cwd = None;
        let mut cwd_external = None;
        let mut remote_name = None;
        let mut sid = None;
        let mut cmds = None;
        let mut orientation = None;
        let mut position = None;
        let mut start: Option<Box<PaneLayout>> = None;
        let mut end: Option<Box<PaneLayout>> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => {
                    kind = Some(map.next_value_seed(BoundedText {
                        field: "type",
                        limit: 32,
                    })?)
                }
                "mode" => {
                    mode = Some(map.next_value_seed(BoundedText {
                        field: "mode",
                        limit: MAX_RESTORED_MODE_BYTES,
                    })?)
                }
                "cwd" => {
                    cwd = map.next_value_seed(BoundedOptionalText(BoundedText {
                        field: "cwd",
                        limit: MAX_RESTORED_CWD_BYTES,
                    }))?
                }
                "cwd_external" => cwd_external = Some(map.next_value::<bool>()?),
                "remote_name" => {
                    remote_name = map.next_value_seed(BoundedOptionalText(BoundedText {
                        field: "remote_name",
                        limit: MAX_RESTORED_REMOTE_NAME_BYTES,
                    }))?
                }
                "sid" => {
                    sid = map.next_value_seed(BoundedOptionalText(BoundedText {
                        field: "sid",
                        limit: MAX_RESTORED_SID_BYTES,
                    }))?
                }
                "cmds" => cmds = map.next_value_seed(RestorableArgvSeed)?,
                "orientation" => orientation = Some(map.next_value::<char>()?),
                "position" => position = Some(map.next_value::<i32>()?),
                "start" => {
                    start = Some(Box::new(map.next_value_seed(PaneLayoutSeed {
                        budget: &mut *budget,
                        tab_panes: &mut *tab_panes,
                        depth: depth - 1,
                    })?))
                }
                "end" => {
                    end = Some(Box::new(map.next_value_seed(PaneLayoutSeed {
                        budget: &mut *budget,
                        tab_panes: &mut *tab_panes,
                        depth: depth - 1,
                    })?))
                }
                // Fields from other releases are ignored, not rejected: a
                // snapshot written by a newer or older anvil must still
                // restore. `IgnoredAny` skips them without allocating.
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }

        match kind.as_deref() {
            Some("leaf") => {
                if *tab_panes == 0 {
                    return Err(A::Error::custom(format_args!(
                        "tab exceeds its {MAX_RESTORED_PANES_PER_TAB}-pane limit"
                    )));
                }
                if budget.remaining_panes == 0 {
                    return Err(A::Error::custom(format_args!(
                        "session exceeds its {MAX_RESTORED_PANES_TOTAL}-pane limit"
                    )));
                }
                *tab_panes -= 1;
                budget.remaining_panes -= 1;
                Ok(PaneLayout::Leaf {
                    mode: mode.ok_or_else(|| A::Error::missing_field("mode"))?,
                    cwd,
                    cwd_external: cwd_external.unwrap_or_default(),
                    remote_name,
                    sid: sid.filter(|value| crate::config::valid_session_id(value)),
                    cmds,
                })
            }
            Some("split") => Ok(PaneLayout::Split {
                orientation: orientation.ok_or_else(|| A::Error::missing_field("orientation"))?,
                position: position.ok_or_else(|| A::Error::missing_field("position"))?,
                start: start.ok_or_else(|| A::Error::missing_field("start"))?,
                end: end.ok_or_else(|| A::Error::missing_field("end"))?,
            }),
            Some(other) => Err(A::Error::unknown_variant(other, &["leaf", "split"])),
            None => Err(A::Error::missing_field("type")),
        }
    }
}

struct SavedTabSeed<'a> {
    budget: &'a mut RestoreBudget,
}

impl<'de> serde::de::DeserializeSeed<'de> for SavedTabSeed<'_> {
    type Value = SavedTab;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for SavedTabSeed<'_> {
    type Value = SavedTab;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a saved tab")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;

        let budget = self.budget;
        let mut title = None;
        let mut custom_title = None;
        let mut pinned = None;
        let mut private_title = None;
        let mut layout = None;
        let mut tab_panes = MAX_RESTORED_PANES_PER_TAB;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "title" => {
                    title = Some(map.next_value_seed(BoundedText {
                        field: "title",
                        limit: MAX_RESTORED_TITLE_BYTES,
                    })?)
                }
                "custom_title" => custom_title = Some(map.next_value::<bool>()?),
                "pinned" => pinned = Some(map.next_value::<bool>()?),
                "private_title" => private_title = Some(map.next_value::<bool>()?),
                "layout" => {
                    layout = Some(map.next_value_seed(PaneLayoutSeed {
                        budget: &mut *budget,
                        tab_panes: &mut tab_panes,
                        depth: MAX_RESTORED_LAYOUT_DEPTH,
                    })?)
                }
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        Ok(SavedTab {
            title: title.ok_or_else(|| A::Error::missing_field("title"))?,
            custom_title: custom_title.ok_or_else(|| A::Error::missing_field("custom_title"))?,
            pinned: pinned.unwrap_or_default(),
            private_title: private_title.unwrap_or_default(),
            layout: layout.ok_or_else(|| A::Error::missing_field("layout"))?,
        })
    }
}

struct SavedTabsSeed<'a> {
    budget: &'a mut RestoreBudget,
}

impl<'de> serde::de::DeserializeSeed<'de> for SavedTabsSeed<'_> {
    type Value = Vec<SavedTab>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> serde::de::Visitor<'de> for SavedTabsSeed<'_> {
    type Value = Vec<SavedTab>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "at most {MAX_RESTORED_TABS} saved tabs")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;

        let budget = self.budget;
        let mut tabs = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_RESTORED_TABS));
        while tabs.len() < MAX_RESTORED_TABS {
            let Some(tab) = seq.next_element_seed(SavedTabSeed {
                budget: &mut *budget,
            })?
            else {
                return Ok(tabs);
            };
            tabs.push(tab);
        }
        // Prove the array is over-wide without building tab 33.
        if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
            return Err(A::Error::custom(format_args!(
                "session exceeds its {MAX_RESTORED_TABS}-tab limit"
            )));
        }
        Ok(tabs)
    }
}

struct SavedSessionSeed<'a> {
    budget: &'a mut RestoreBudget,
}

impl<'de> serde::de::DeserializeSeed<'de> for SavedSessionSeed<'_> {
    type Value = SavedSession;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for SavedSessionSeed<'_> {
    type Value = SavedSession;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a saved session")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;

        let budget = self.budget;
        let mut active = None;
        let mut tabs = None;
        let mut ai_conversation = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "active" => active = Some(map.next_value::<usize>()?),
                "tabs" => {
                    tabs = Some(map.next_value_seed(SavedTabsSeed {
                        budget: &mut *budget,
                    })?)
                }
                "ai_conversation" => {
                    let encoded = map.next_value_seed(BoundedText {
                        field: "ai_conversation",
                        limit: MAX_RESTORED_AI_CONVERSATION_BYTES,
                    })?;
                    if jterm_core::ai::ConversationSnapshot::from_json(&encoded).is_ok() {
                        ai_conversation = Some(encoded);
                    } else {
                        log::warn!("Ignoring invalid AI conversation in session snapshot");
                    }
                }
                _ => {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        Ok(SavedSession {
            active: active.ok_or_else(|| A::Error::missing_field("active"))?,
            tabs: tabs.ok_or_else(|| A::Error::missing_field("tabs"))?,
            ai_conversation,
        })
    }
}

/// Decoded form of a versioned envelope. The payload is adjacently tagged, and
/// the workspace is the only variant carrying content, so both key orders
/// decode without buffering.
struct DecodedEnvelope {
    format: String,
    version: u8,
    state: SnapshotState,
    supersedes: Option<String>,
}

struct EnvelopeSeed;

impl<'de> serde::de::DeserializeSeed<'de> for EnvelopeSeed {
    type Value = DecodedEnvelope;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for EnvelopeSeed {
    type Value = DecodedEnvelope;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a session envelope")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;

        let mut budget = RestoreBudget {
            remaining_panes: MAX_RESTORED_PANES_TOTAL,
        };
        let mut format = None;
        let mut version = None;
        let mut supersedes = None;
        let mut kind: Option<String> = None;
        let mut session = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "format" => {
                    format = Some(map.next_value_seed(BoundedText {
                        field: "format",
                        limit: MAX_RESTORED_ENVELOPE_FORMAT_BYTES,
                    })?)
                }
                "version" => version = Some(map.next_value::<u8>()?),
                "supersedes" => {
                    supersedes = map.next_value_seed(BoundedOptionalText(BoundedText {
                        field: "supersedes",
                        limit: MAX_RESTORED_SUPERSEDES_BYTES,
                    }))?
                }
                "payload" => {
                    let (payload_kind, payload_session) =
                        map.next_value_seed(EnvelopePayloadSeed {
                            budget: &mut budget,
                        })?;
                    kind = Some(payload_kind);
                    session = payload_session;
                }
                other => {
                    return Err(A::Error::unknown_field(
                        other,
                        &["format", "version", "payload", "supersedes"],
                    ))
                }
            }
        }
        let state = match (kind.as_deref(), session) {
            (Some("workspace"), Some(session)) => SnapshotState::Workspace(session),
            (Some("empty"), None) => SnapshotState::Empty,
            (Some(other), _) => {
                return Err(A::Error::custom(format_args!(
                    "session envelope payload '{other}' does not match its content"
                )))
            }
            (None, _) => return Err(A::Error::missing_field("payload")),
        };
        Ok(DecodedEnvelope {
            format: format.ok_or_else(|| A::Error::missing_field("format"))?,
            version: version.ok_or_else(|| A::Error::missing_field("version"))?,
            state,
            supersedes,
        })
    }
}

struct EnvelopePayloadSeed<'a> {
    budget: &'a mut RestoreBudget,
}

impl<'de> serde::de::DeserializeSeed<'de> for EnvelopePayloadSeed<'_> {
    type Value = (String, Option<SavedSession>);

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for EnvelopePayloadSeed<'_> {
    type Value = (String, Option<SavedSession>);

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a session envelope payload")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;

        let budget = self.budget;
        let mut kind = None;
        let mut session = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "kind" => {
                    kind = Some(map.next_value_seed(BoundedText {
                        field: "kind",
                        limit: 32,
                    })?)
                }
                "session" => {
                    session = Some(map.next_value_seed(SavedSessionSeed {
                        budget: &mut *budget,
                    })?)
                }
                other => return Err(A::Error::unknown_field(other, &["kind", "session"])),
            }
        }
        Ok((
            kind.ok_or_else(|| A::Error::missing_field("kind"))?,
            session,
        ))
    }
}

/// Decode a bare legacy snapshot under the restore budgets.
fn decode_saved_session(contents: &str) -> Result<SavedSession, serde_json::Error> {
    jterm_core::bounded_json::validate_no_duplicate_members(contents.as_bytes())?;
    decode_saved_session_after_preflight(contents)
}

fn decode_saved_session_after_preflight(contents: &str) -> Result<SavedSession, serde_json::Error> {
    let mut budget = RestoreBudget {
        remaining_panes: MAX_RESTORED_PANES_TOTAL,
    };
    let mut deserializer = serde_json::Deserializer::from_str(contents);
    let session = serde::de::DeserializeSeed::deserialize(
        SavedSessionSeed {
            budget: &mut budget,
        },
        &mut deserializer,
    )?;
    deserializer.end()?;
    Ok(session)
}

#[cfg(test)]
fn decode_pane_layout(contents: &str) -> Result<PaneLayout, serde_json::Error> {
    jterm_core::bounded_json::validate_no_duplicate_members(contents.as_bytes())?;
    let mut budget = RestoreBudget {
        remaining_panes: MAX_RESTORED_PANES_TOTAL,
    };
    let mut tab_panes = MAX_RESTORED_PANES_PER_TAB;
    let mut deserializer = serde_json::Deserializer::from_str(contents);
    let layout = serde::de::DeserializeSeed::deserialize(
        PaneLayoutSeed {
            budget: &mut budget,
            tab_panes: &mut tab_panes,
            depth: MAX_RESTORED_LAYOUT_DEPTH,
        },
        &mut deserializer,
    )?;
    deserializer.end()?;
    Ok(layout)
}

/// Decode a versioned envelope under the restore budgets.
fn decode_session_envelope(contents: &str) -> Result<DecodedEnvelope, serde_json::Error> {
    jterm_core::bounded_json::validate_no_duplicate_members(contents.as_bytes())?;
    decode_session_envelope_after_preflight(contents)
}

fn decode_session_envelope_after_preflight(
    contents: &str,
) -> Result<DecodedEnvelope, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(contents);
    let envelope = serde::de::DeserializeSeed::deserialize(EnvelopeSeed, &mut deserializer)?;
    deserializer.end()?;
    Ok(envelope)
}

fn pane_layout_within_restore_limits(layout: &PaneLayout) -> Option<usize> {
    fn count(layout: &PaneLayout, remaining: &mut usize) -> Option<usize> {
        match layout {
            PaneLayout::Leaf {
                mode,
                cwd,
                remote_name,
                sid,
                cmds,
                ..
            } => {
                if mode.len() > MAX_RESTORED_MODE_BYTES
                    || cwd
                        .as_ref()
                        .is_some_and(|value| value.len() > MAX_RESTORED_CWD_BYTES)
                    || remote_name
                        .as_ref()
                        .is_some_and(|value| value.len() > MAX_RESTORED_REMOTE_NAME_BYTES)
                    || sid.as_ref().is_some_and(|value| {
                        value.len() > MAX_RESTORED_SID_BYTES
                            || !crate::config::valid_session_id(value)
                    })
                    || cmds
                        .as_deref()
                        .is_some_and(|argv| !restorable_command_within_limits(argv))
                    || *remaining == 0
                {
                    return None;
                }
                *remaining -= 1;
                Some(1)
            }
            PaneLayout::Split { start, end, .. } => {
                let left = count(start, remaining)?;
                let right = count(end, remaining)?;
                left.checked_add(right)
            }
        }
    }

    let mut remaining = MAX_RESTORED_PANES_PER_TAB;
    count(layout, &mut remaining)
}

fn session_within_restore_limits(session: &SavedSession) -> bool {
    if session.tabs.is_empty() || session.tabs.len() > MAX_RESTORED_TABS {
        return false;
    }
    if session.ai_conversation.as_ref().is_some_and(|encoded| {
        encoded.len() > MAX_RESTORED_AI_CONVERSATION_BYTES
            || jterm_core::ai::ConversationSnapshot::from_json(encoded).is_err()
    }) {
        return false;
    }
    let mut total = 0usize;
    for tab in &session.tabs {
        if tab.title.len() > MAX_RESTORED_TITLE_BYTES {
            return false;
        }
        let Some(panes) = pane_layout_within_restore_limits(&tab.layout) else {
            return false;
        };
        let Some(next_total) = total.checked_add(panes) else {
            return false;
        };
        if next_total > MAX_RESTORED_PANES_TOTAL {
            return false;
        }
        total = next_total;
    }
    true
}

/// Reclassify every argv after deserialization and immediately before the
/// restored layout can reach any pane-spawn path. The serde shape check only
/// proves that argv boundaries survived; it does not prove an on-disk writer
/// stored one of the deliberately replayable command families.
fn sanitize_restorable_commands(session: &mut SavedSession) {
    fn sanitize_layout(layout: &mut PaneLayout) {
        match layout {
            PaneLayout::Leaf { cmds, .. } => {
                *cmds = cmds
                    .take()
                    .and_then(|argv| jterm_core::process::match_restorable_command(&argv));
            }
            PaneLayout::Split { start, end, .. } => {
                sanitize_layout(start);
                sanitize_layout(end);
            }
        }
    }

    for tab in &mut session.tabs {
        sanitize_layout(&mut tab.layout);
    }
}

/// Count recoverable and currently active snapshots without exposing paths.
/// Token snapshots use owner locks; ambiguous PID-era snapshots are counted as
/// active so diagnostics never encourage deleting a possibly live window.
pub(crate) fn session_snapshot_counts() -> (usize, usize) {
    let directory = state_dir();
    session_snapshot_counts_in(&directory, None, &|token| {
        token_lock_state_in(&directory, token)
    })
}

fn session_snapshot_counts_in(
    dir: &Path,
    current_token: Option<&str>,
    lock_state: &dyn Fn(&str) -> TokenLockState,
) -> (usize, usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut ready = 0;
    let mut active = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let Some(file) = file_name.to_str().and_then(parse_state_file_name) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        if state_file_is_recoverable(&file, current_token, lock_state) {
            ready += 1;
        } else {
            active += 1;
        }
    }
    (ready, active)
}

struct BoundedSnapshotWriter {
    bytes: Vec<u8>,
}

impl BoundedSnapshotWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(64 * 1024),
        }
    }
}

impl Write for BoundedSnapshotWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next_len = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::FileTooLarge))?;
        if next_len > MAX_SNAPSHOT_BYTES as usize {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("serialized session snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes"),
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_snapshot_bounded(
    owner: &SnapshotOwner,
    session: &SavedSession,
) -> io::Result<Vec<u8>> {
    if !session.tabs.is_empty() && !session_within_restore_limits(session) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "session exceeds the {MAX_RESTORED_TABS}-tab, \
                 {MAX_RESTORED_PANES_PER_TAB}-panes-per-tab, \
                 {MAX_RESTORED_PANES_TOTAL}-total-pane, or field-size limits"
            ),
        ));
    }
    let envelope = SessionEnvelope {
        format: SESSION_ENVELOPE_FORMAT.to_string(),
        version: SESSION_ENVELOPE_VERSION,
        payload: if session.tabs.is_empty() {
            SessionEnvelopePayload::Empty
        } else {
            SessionEnvelopePayload::Workspace(session)
        },
        supersedes: owner.pending_claim_file_name()?,
    };
    let mut writer = BoundedSnapshotWriter::new();
    serde_json::to_writer(&mut writer, &envelope).map_err(|error| {
        let kind = error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData);
        io::Error::new(
            kind,
            format!("failed to serialize session snapshot: {error}"),
        )
    })?;
    Ok(writer.bytes)
}

/// Write the new snapshot durably without committing its predecessor. Kept as
/// a separate transaction phase so crash-window tests can pin recovery after
/// the rename/fsync but before claim cleanup.
fn write_snapshot_for_owner(owner: &SnapshotOwner, session: &SavedSession) -> io::Result<()> {
    let payload = serialize_snapshot_bounded(owner, session)?;
    atomic_write(&owner.state_path, &payload)
}

/// Validate and durably checkpoint either a workspace or the explicit empty
/// tombstone. The pending restore is committed only after replacement and its
/// parent directory have been synced.
fn checkpoint_snapshot_for_owner(owner: &SnapshotOwner, session: &SavedSession) -> io::Result<()> {
    write_snapshot_for_owner(owner, session)?;
    owner.commit_pending_restore()
}

fn drop_restorable_commands(layout: &mut PaneLayout) -> usize {
    match layout {
        PaneLayout::Leaf { cmds, .. } => usize::from(cmds.take().is_some()),
        PaneLayout::Split { start, end, .. } => {
            drop_restorable_commands(start) + drop_restorable_commands(end)
        }
    }
}

fn drop_invalid_ai_conversation(session: &mut SavedSession) -> bool {
    let invalid = session
        .ai_conversation
        .as_ref()
        .is_some_and(|encoded| jterm_core::ai::ConversationSnapshot::from_json(encoded).is_err());
    if invalid {
        session.ai_conversation = None;
    }
    invalid
}

/// Keep the terminal workspace recoverable even when individually valid AI
/// and argv fields combine into a payload larger than the aggregate disk cap.
/// A failed bounded serialization has not touched the prior snapshot, so it is
/// safe to retry after dropping optional payloads in least-essential order.
fn checkpoint_snapshot_resilient(
    owner: &SnapshotOwner,
    mut session: SavedSession,
) -> io::Result<()> {
    if drop_invalid_ai_conversation(&mut session) {
        log::warn!("Ignoring invalid AI conversation state while saving the terminal workspace");
    }
    match checkpoint_snapshot_for_owner(owner, &session) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {}
        Err(error) => return Err(error),
    }

    if session.ai_conversation.take().is_some() {
        log::warn!(
            "Session snapshot exceeded {} bytes; retrying without optional AI conversation state",
            MAX_SNAPSHOT_BYTES
        );
        match checkpoint_snapshot_for_owner(owner, &session) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {}
            Err(error) => return Err(error),
        }
    }

    let dropped_commands: usize = session
        .tabs
        .iter_mut()
        .map(|tab| drop_restorable_commands(&mut tab.layout))
        .sum();
    if dropped_commands == 0 {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("session snapshot still exceeds {MAX_SNAPSHOT_BYTES} bytes"),
        ));
    }
    log::warn!(
        "Session snapshot exceeded {} bytes; retrying without {dropped_commands} optional restorable command(s)",
        MAX_SNAPSHOT_BYTES
    );
    checkpoint_snapshot_for_owner(owner, &session)
}

fn save_session_now(session: SavedSession) -> io::Result<()> {
    let owner = snapshot_owner()?;
    checkpoint_snapshot_resilient(owner, session)?;
    let directory = state_dir();
    prune_recoverable_snapshots(
        &directory,
        Some(&owner.token),
        &|token| token_lock_state_in(&directory, token),
        MAX_RECOVERABLE_SNAPSHOTS,
    );
    Ok(())
}

/// Queue a main-thread snapshot for bounded, coalescing background persistence.
/// Capturing GTK-owned state stays synchronous; JSON encoding, atomic replace,
/// file/directory fsync, claim cleanup, and pruning all run on the worker.
pub(crate) fn save_session(session: SavedSession) {
    let key = crate::persistence::PersistenceKey::for_path("session", &state_dir());
    if let Err(error) =
        crate::persistence::enqueue_session(key, "save session snapshot", move || {
            save_session_now(session)
        })
    {
        log::error!("Cannot queue session snapshot: {error}");
    }
}

/// Claim the newest valid snapshot whose owning process has exited. The claim
/// remains on disk until [`save_session`] has durably checkpointed the restored
/// workspace, so a crash during restore never consumes the only copy.
/// Corrupt/unreadable files are deliberately retained for inspection/recovery.
pub(crate) fn load_session() -> Option<SavedSession> {
    let directory = state_dir();
    let owner = match snapshot_owner() {
        Ok(owner) => owner,
        Err(error) => {
            log::error!("Cannot initialize session snapshot owner: {error}");
            return None;
        }
    };
    let lock_state = |token: &str| token_lock_state_in(&directory, token);
    let session = claim_session_from(&directory, &owner.token, &lock_state)
        .map(|claimed| adopt_claimed_snapshot(owner, claimed))
        .transpose()
        .unwrap_or_else(|error| {
            log::error!(
                "Cannot adopt claimed session until its checkpoint: {error}; claim left on disk"
            );
            None
        })
        .flatten();
    prune_recoverable_snapshots(
        &directory,
        Some(&owner.token),
        &lock_state,
        MAX_RECOVERABLE_SNAPSHOTS,
    );
    session
}

fn adopt_claimed_snapshot(
    owner: &SnapshotOwner,
    claimed: ClaimedSnapshot,
) -> io::Result<Option<SavedSession>> {
    owner.remember_pending_restore(claimed.pending)?;
    match claimed.state {
        SnapshotState::Workspace(session) => Ok(Some(session)),
        SnapshotState::Empty => {
            // Propagate the tombstone under the current owner before committing
            // its claimed predecessor. If this write fails, the old tombstone
            // stays recoverable and still prevents workspace resurrection.
            checkpoint_snapshot_for_owner(owner, &SavedSession::default())?;
            Ok(None)
        }
    }
}

/// Create the state directory and make it private.
///
/// Kept local because the owner-lock protocol below
/// needs the directory to exist and be `0700` *before* it creates any file in
/// it: a lock published under a world-readable directory is the window this
/// guards against.
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        // Re-open the final component without following it, then validate and
        // chmod through that descriptor. `create_dir_all` accepts an existing
        // symlink-to-directory; a pathname chmod here would otherwise tighten
        // and populate the symlink target rather than anvil's own state dir.
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(
                nix::libc::O_DIRECTORY
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_NONBLOCK
                    | nix::libc::O_CLOEXEC,
            )
            .open(path)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("session state path {} is not a directory", path.display()),
            ));
        }
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "session state directory {} is not owned by the current user",
                    path.display()
                ),
            ));
        }
        directory.set_permissions(fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Durably replace a snapshot, creating its `0700` directory if needed.
///
/// The shared lower-level atomic writer owns the temp-write/fsync/rename dance
/// and the `0600` mode. anvil first validates and tightens its owned directory
/// through a no-follow descriptor, avoiding a later path-based chmod race.
fn atomic_write(path: &Path, payload: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session snapshot path must have an explicit parent directory",
            )
        })?;
    ensure_private_directory(parent)?;
    jterm_core::atomic_file::write_atomic(path, payload)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            nix::libc::O_DIRECTORY
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_NONBLOCK
                | nix::libc::O_CLOEXEC,
        );
    }
    options.open(path)?.sync_all()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InstanceIdentity {
    Token(String),
    LegacyPid(u32),
}

impl InstanceIdentity {
    fn token(&self) -> Option<&str> {
        match self {
            Self::Token(token) => Some(token),
            Self::LegacyPid(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StateFileName {
    /// Canonical pre-claim filename (`tabs.<token>.state`, a PID-era filename,
    /// or the oldest `tabs.state` form).
    base_name: String,
    /// Instance that wrote the snapshot. `tabs.state` has no owner metadata.
    owner: Option<InstanceIdentity>,
    /// Instance that claimed this file but exited before consuming it.
    claimer: Option<InstanceIdentity>,
}

struct PendingRestoreClaim {
    path: PathBuf,
    source_file: StateFileName,
    supersedes: Option<ClaimReference>,
    _retired_owner_locks: Vec<HeldRetiredOwnerLock>,
}

#[derive(Debug)]
struct ClaimReference {
    path: PathBuf,
    file_name: StateFileName,
}

struct ClaimedSnapshot {
    state: SnapshotState,
    pending: PendingRestoreClaim,
}

impl SnapshotOwner {
    fn remember_pending_restore(&self, pending: PendingRestoreClaim) -> io::Result<()> {
        let mut slot = self.pending_restore.lock().map_err(|_| {
            io::Error::other("session pending-restore state was poisoned by a panic")
        })?;
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this window already has a pending restored-session claim",
            ));
        }
        *slot = Some(pending);
        Ok(())
    }

    fn pending_claim_file_name(&self) -> io::Result<Option<String>> {
        let slot = self.pending_restore.lock().map_err(|_| {
            io::Error::other("session pending-restore state was poisoned by a panic")
        })?;
        slot.as_ref()
            .map(|pending| {
                pending
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "claimed session filename is not valid UTF-8",
                        )
                    })
            })
            .transpose()
    }

    fn commit_pending_restore(&self) -> io::Result<()> {
        let mut slot = self.pending_restore.lock().map_err(|_| {
            io::Error::other("session pending-restore state was poisoned by a panic")
        })?;
        let Some(pending) = slot.as_ref() else {
            return Ok(());
        };
        let directory = pending
            .path
            .parent()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "claimed session path has no parent directory",
                )
            })?
            .to_path_buf();
        let mut remaining_bytes = MAX_CLAIM_CHAIN_BYTES;
        let mut cleaned_files = Vec::new();
        if let Some(superseded) = &pending.supersedes {
            remove_superseded_claim_chain(
                &directory,
                superseded,
                0,
                &mut remaining_bytes,
                &mut cleaned_files,
            )?;
        }
        match fs::remove_file(&pending.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        cleaned_files.push(pending.source_file.clone());
        sync_directory(&directory)?;
        // Release retired owner flocks before lock-file garbage collection;
        // otherwise our own chain guard would make every stale lock look live.
        drop(slot.take().expect("pending restore was present above"));
        for file in &cleaned_files {
            cleanup_locks_referenced_by(&directory, file);
        }
        cleanup_orphaned_locks(&directory);
        Ok(())
    }
}

fn parse_instance_identity(value: &str) -> Option<InstanceIdentity> {
    if let Some(pid) = value.parse::<u32>().ok().filter(|pid| *pid > 0) {
        return Some(InstanceIdentity::LegacyPid(pid));
    }
    valid_instance_token(value).then(|| InstanceIdentity::Token(value.to_ascii_lowercase()))
}

fn parse_unclaimed_name(name: &str) -> Option<(String, Option<InstanceIdentity>)> {
    if name == LEGACY_STATE_FILE {
        return Some((name.to_string(), None));
    }
    let owner_text = name
        .strip_prefix(STATE_PREFIX)?
        .strip_suffix(STATE_SUFFIX)?;
    let owner = parse_instance_identity(owner_text)?;
    Some((name.to_string(), Some(owner)))
}

fn parse_state_file_name(name: &str) -> Option<StateFileName> {
    if let Some((base, claimer)) = name.rsplit_once(CLAIM_MARKER) {
        let claimer = parse_instance_identity(claimer)?;
        let (base_name, owner) = parse_unclaimed_name(base)?;
        return Some(StateFileName {
            base_name,
            owner,
            claimer: Some(claimer),
        });
    }
    let (base_name, owner) = parse_unclaimed_name(name)?;
    Some(StateFileName {
        base_name,
        owner,
        claimer: None,
    })
}

fn superseded_claim_reference(
    dir: &Path,
    source_file: &StateFileName,
    name: &str,
) -> io::Result<ClaimReference> {
    let path = Path::new(name);
    if path.file_name().and_then(|part| part.to_str()) != Some(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session envelope supersedes must be one UTF-8 filename",
        ));
    }
    let file_name = parse_state_file_name(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "session envelope supersedes is not a snapshot claim name",
        )
    })?;
    let writer = source_file
        .owner
        .as_ref()
        .and_then(InstanceIdentity::token)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "versioned session envelope has no token owner",
            )
        })?;
    let claimer = file_name
        .claimer
        .as_ref()
        .and_then(InstanceIdentity::token)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "session envelope supersedes must name a token-owned claim",
            )
        })?;
    if !claimer.eq_ignore_ascii_case(writer) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session envelope supersedes was not claimed by its writer",
        ));
    }
    Ok(ClaimReference {
        path: dir.join(name),
        file_name,
    })
}

fn parse_snapshot_payload(
    contents: &str,
    dir: &Path,
    source_file: &StateFileName,
) -> io::Result<(SnapshotState, Option<ClaimReference>)> {
    jterm_core::bounded_json::validate_no_duplicate_members(contents.as_bytes()).map_err(
        |error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ambiguous session snapshot: {error}"),
            )
        },
    )?;
    match decode_session_envelope_after_preflight(contents) {
        Ok(envelope) => {
            if envelope.format != SESSION_ENVELOPE_FORMAT
                || envelope.version != SESSION_ENVELOPE_VERSION
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "unsupported session envelope format/version: {}/{}",
                        envelope.format, envelope.version
                    ),
                ));
            }
            let supersedes = envelope
                .supersedes
                .as_deref()
                .map(|name| superseded_claim_reference(dir, source_file, name))
                .transpose()?;
            let state = match envelope.state {
                SnapshotState::Workspace(mut session) => {
                    if !session_within_restore_limits(&session) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "session envelope workspace exceeds restore limits",
                        ));
                    }
                    sanitize_restorable_commands(&mut session);
                    SnapshotState::Workspace(session)
                }
                SnapshotState::Empty => SnapshotState::Empty,
            };
            Ok((state, supersedes))
        }
        Err(envelope_error) => {
            let mut session =
                decode_saved_session_after_preflight(contents).map_err(|legacy_error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "neither a valid session envelope ({envelope_error}) nor a legacy snapshot ({legacy_error})"
                    ),
                )
            })?;
            if !session_within_restore_limits(&session) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy session snapshot exceeds restore limits",
                ));
            }
            sanitize_restorable_commands(&mut session);
            Ok((SnapshotState::Workspace(session), None))
        }
    }
}

fn hold_retired_owner_lock(
    dir: &Path,
    token: &str,
    locks: &mut Vec<HeldRetiredOwnerLock>,
) -> io::Result<bool> {
    if locks
        .iter()
        .any(|lock| lock.token.eq_ignore_ascii_case(token))
    {
        return Ok(true);
    }
    let file = match open_existing_lock_file(&lock_file_path_for_token(dir, token)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !try_lock_file_exclusive(&file)? {
        return Ok(false);
    }
    locks.push(HeldRetiredOwnerLock {
        token: token.to_string(),
        file,
    });
    Ok(true)
}

fn hold_reference_chain_locks(
    dir: &Path,
    reference: &ClaimReference,
    depth: usize,
    remaining_bytes: &mut u64,
    locks: &mut Vec<HeldRetiredOwnerLock>,
) -> io::Result<bool> {
    if depth >= MAX_CLAIM_CHAIN_DEPTH || *remaining_bytes == 0 {
        return Ok(false);
    }
    let read_limit = (*remaining_bytes).min(MAX_SNAPSHOT_BYTES);
    let contents = match read_snapshot_bounded_to(&reference.path, read_limit) {
        Ok(contents) => contents,
        // A missing predecessor means an earlier commit completed its unlink;
        // the current envelope remains valid and there is no branch to lock.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    *remaining_bytes -= contents.len() as u64;
    let (_, supersedes) = parse_snapshot_payload(&contents, dir, &reference.file_name)?;
    match supersedes {
        Some(next) => {
            // `reference` is protected by its claimer lock, which the caller
            // already holds. Its owner wrote the nested pointer, so acquire
            // that owner's lock before descending to the next claim.
            let Some(writer) = reference
                .file_name
                .owner
                .as_ref()
                .and_then(InstanceIdentity::token)
            else {
                return Ok(false);
            };
            if !hold_retired_owner_lock(dir, writer, locks)? {
                return Ok(false);
            }
            hold_reference_chain_locks(dir, &next, depth + 1, remaining_bytes, locks)
        }
        None => Ok(true),
    }
}

fn hold_candidate_chain_locks(
    dir: &Path,
    candidate: &SessionCandidate,
) -> io::Result<Option<Vec<HeldRetiredOwnerLock>>> {
    // Preserve the global lock order used by publisher/cleanup paths while
    // converting available owner locks into held predecessor guards. Without
    // the protocol, cleanup could unlink a pathname between our open and flock,
    // leaving us holding only a detached inode that protects no future probe.
    let _protocol = LockProtocolGuard::acquire(dir)?;
    let mut locks = Vec::new();
    if let Some(identity) = candidate
        .file_name
        .claimer
        .as_ref()
        .or(candidate.file_name.owner.as_ref())
        .and_then(InstanceIdentity::token)
    {
        if !hold_retired_owner_lock(dir, identity, &mut locks)? {
            return Ok(None);
        }
    }
    if let Some(writer) = candidate
        .file_name
        .owner
        .as_ref()
        .and_then(InstanceIdentity::token)
    {
        if !hold_retired_owner_lock(dir, writer, &mut locks)? {
            return Ok(None);
        }
    }
    if let Some(supersedes) = &candidate.supersedes {
        let mut remaining_bytes = MAX_CLAIM_CHAIN_BYTES;
        if !hold_reference_chain_locks(dir, supersedes, 0, &mut remaining_bytes, &mut locks)? {
            return Ok(None);
        }
    }
    Ok(Some(locks))
}

fn remove_superseded_claim_chain(
    dir: &Path,
    reference: &ClaimReference,
    depth: usize,
    remaining_bytes: &mut u64,
    cleaned_files: &mut Vec<StateFileName>,
) -> io::Result<()> {
    if depth >= MAX_CLAIM_CHAIN_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session supersedes chain exceeds its depth limit",
        ));
    }
    if *remaining_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "session supersedes chain exceeds its byte budget",
        ));
    }
    let read_limit = (*remaining_bytes).min(MAX_SNAPSHOT_BYTES);
    let contents = match read_snapshot_bounded_to(&reference.path, read_limit) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            cleaned_files.push(reference.file_name.clone());
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    *remaining_bytes -= contents.len() as u64;
    let (_, supersedes) = parse_snapshot_payload(&contents, dir, &reference.file_name)?;
    if let Some(next) = &supersedes {
        remove_superseded_claim_chain(dir, next, depth + 1, remaining_bytes, cleaned_files)?;
    }
    match fs::remove_file(&reference.path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    cleaned_files.push(reference.file_name.clone());
    Ok(())
}

fn state_file_is_recoverable(
    file: &StateFileName,
    current_token: Option<&str>,
    lock_state: &dyn Fn(&str) -> TokenLockState,
) -> bool {
    let identity = file.claimer.as_ref().or(file.owner.as_ref());
    match identity {
        Some(InstanceIdentity::Token(token)) => {
            if current_token.is_some_and(|current| token.eq_ignore_ascii_case(current)) {
                return false;
            }
            matches!(lock_state(token), TokenLockState::Available)
        }
        // A raw PID does not identify the same process across PID namespaces
        // and can be reused before another launch scans the directory. Without
        // a companion lock, neither kill(0) success nor failure proves that the
        // writer/claimer is dead. Preserve these files for manual recovery.
        Some(InstanceIdentity::LegacyPid(_)) => false,
        // Compatibility with the oldest `tabs.state` format, which was written
        // before NON_UNIQUE multi-window snapshots and has no owner identity.
        None => true,
    }
}

fn state_file_references_token(file: &StateFileName, token: &str) -> bool {
    file.owner
        .as_ref()
        .and_then(InstanceIdentity::token)
        .is_some_and(|owner| owner.eq_ignore_ascii_case(token))
        || file
            .claimer
            .as_ref()
            .and_then(InstanceIdentity::token)
            .is_some_and(|claimer| claimer.eq_ignore_ascii_case(token))
}

fn token_referenced_by_state_files(dir: &Path, token: &str) -> io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .and_then(parse_state_file_name)
            .is_some_and(|file| state_file_references_token(&file, token))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_lock_file_token(name: &str) -> Option<String> {
    let token = name.strip_prefix(STATE_PREFIX)?.strip_suffix(LOCK_SUFFIX)?;
    valid_instance_token(token).then(|| token.to_ascii_lowercase())
}

/// Remove a retired token's companion lock only after acquiring its flock and
/// proving no remaining state/claim references it. This avoids unlinking a
/// lock inode that a live process still owns.
fn cleanup_lock_if_unreferenced(dir: &Path, token: &str) {
    let _protocol = match LockProtocolGuard::try_acquire(dir) {
        Ok(Some(protocol)) => protocol,
        Ok(None) => return,
        Err(error) => {
            log::warn!(
                "Cannot enter session owner lock cleanup protocol in {}: {error}",
                dir.display()
            );
            return;
        }
    };
    cleanup_lock_if_unreferenced_under_protocol(dir, token);
}

/// Caller must hold `LockProtocolGuard`, preserving the global lock order
/// protocol -> owner lock.
fn cleanup_lock_if_unreferenced_under_protocol(dir: &Path, token: &str) {
    let path = lock_file_path_for_token(dir, token);
    let file = match open_existing_lock_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            log::warn!(
                "Cannot open retired session owner lock {}: {error}",
                path.display()
            );
            return;
        }
    };
    match try_temporary_exclusive_lock(&file) {
        Ok(Some(_lock)) => match token_referenced_by_state_files(dir, token) {
            Ok(false) => {
                if let Err(error) = fs::remove_file(&path) {
                    log::warn!(
                        "Failed to remove retired session owner lock {}: {error}",
                        path.display()
                    );
                }
            }
            Ok(true) => {}
            Err(error) => {
                log::warn!(
                    "Cannot verify that retired session owner lock {} is unreferenced: {error}",
                    path.display()
                );
            }
        },
        Ok(None) => {}
        Err(error) => {
            log::warn!(
                "Cannot lock retired session owner lock {}: {error}",
                path.display()
            );
        }
    };
}

fn cleanup_locks_referenced_by(dir: &Path, file: &StateFileName) {
    if let Some(token) = file.owner.as_ref().and_then(InstanceIdentity::token) {
        cleanup_lock_if_unreferenced(dir, token);
    }
    if let Some(token) = file.claimer.as_ref().and_then(InstanceIdentity::token) {
        cleanup_lock_if_unreferenced(dir, token);
    }
}

fn cleanup_orphaned_locks(dir: &Path) {
    let _protocol = match LockProtocolGuard::try_acquire(dir) {
        Ok(Some(protocol)) => protocol,
        Ok(None) => return,
        Err(error) => {
            log::warn!(
                "Cannot enter orphaned session lock cleanup protocol in {}: {error}",
                dir.display()
            );
            return;
        }
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let tokens: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            parse_lock_file_token(name.to_str()?)
        })
        .collect();
    for token in tokens {
        cleanup_lock_if_unreferenced_under_protocol(dir, &token);
    }
}

/// Bound stale/ready snapshots without touching files whose owner lock is held
/// or cannot be inspected. PID-era files are likewise retained because their
/// process identity is ambiguous across namespaces.
fn prune_recoverable_snapshots(
    dir: &Path,
    current_token: Option<&str>,
    lock_state: &dyn Fn(&str) -> TokenLockState,
    keep: usize,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut snapshots: Vec<(SystemTime, PathBuf, StateFileName)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name();
            let file = parse_state_file_name(name.to_str()?)?;
            if !path.is_file() || !state_file_is_recoverable(&file, current_token, lock_state) {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH);
            Some((modified, path, file))
        })
        .collect();
    snapshots.sort_by(|(left_time, left_path, _), (right_time, right_path, _)| {
        right_time
            .cmp(left_time)
            .then_with(|| right_path.cmp(left_path))
    });
    for (_, path, file) in snapshots.into_iter().skip(keep) {
        match fs::remove_file(&path) {
            Ok(()) => cleanup_locks_referenced_by(dir, &file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                log::warn!(
                    "Failed to prune old session snapshot {}: {error}",
                    path.display()
                );
            }
        }
    }
    cleanup_orphaned_locks(dir);
}

#[derive(Debug)]
struct SessionCandidate {
    path: PathBuf,
    file_name: StateFileName,
    modified: SystemTime,
    state: SnapshotState,
    supersedes: Option<ClaimReference>,
}

struct CandidateDescriptor {
    path: PathBuf,
    file_name: StateFileName,
    modified: SystemTime,
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
    current_token: Option<&str>,
    lock_state: &dyn Fn(&str) -> TokenLockState,
) -> Vec<SessionCandidate> {
    scan_candidates_with_reader(
        dir,
        current_token,
        lock_state,
        &mut read_snapshot_bounded_to,
    )
}

fn scan_candidates_with_reader(
    dir: &Path,
    current_token: Option<&str>,
    lock_state: &dyn Fn(&str) -> TokenLockState,
    read_snapshot: &mut dyn FnMut(&Path, u64) -> io::Result<String>,
) -> Vec<SessionCandidate> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            log::error!("Failed to list session state dir {}: {err}", dir.display());
            return Vec::new();
        }
    };

    // First collect only cheap metadata, then select the newest bounded subset
    // before reading or parsing any payload. This keeps both resident decoded
    // sessions and total serde work bounded even if the directory contains a
    // very large number of plausible snapshot names.
    let mut descriptors = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES_PER_SCAN {
            log::warn!(
                "Session state directory {} exceeds the {}-entry startup scan limit",
                dir.display(),
                MAX_DIRECTORY_ENTRIES_PER_SCAN
            );
            break;
        }
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
        if !state_file_is_recoverable(&file_name, current_token, lock_state) {
            log::debug!(
                "Leaving live or identity-ambiguous session snapshot {} untouched",
                entry.path().display()
            );
            continue;
        }

        let path = entry.path();
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
        descriptors.push(CandidateDescriptor {
            path,
            file_name,
            modified,
        });
    }

    descriptors.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.path.file_name().cmp(&a.path.file_name()))
    });
    descriptors.truncate(MAX_CANDIDATES_PER_SCAN);

    let mut candidates = Vec::with_capacity(descriptors.len());
    let mut remaining_bytes = MAX_CANDIDATE_BYTES_PER_SCAN;
    for descriptor in descriptors {
        if remaining_bytes == 0 {
            break;
        }
        let read_limit = remaining_bytes.min(MAX_SNAPSHOT_BYTES);
        let contents = match read_snapshot(&descriptor.path, read_limit) {
            Ok(contents) => contents,
            Err(err) => {
                log::error!(
                    "Cannot read recoverable session snapshot {}: {err}; file retained",
                    descriptor.path.display()
                );
                continue;
            }
        };
        let payload_bytes = contents.len() as u64;
        if payload_bytes > read_limit {
            log::error!(
                "Snapshot reader exceeded its {}-byte limit for {}; file retained",
                read_limit,
                descriptor.path.display()
            );
            break;
        }
        remaining_bytes -= payload_bytes;

        let (state, supersedes) =
            match parse_snapshot_payload(&contents, dir, &descriptor.file_name) {
                Ok(parsed) => parsed,
                Err(err) => {
                    log::error!(
                        "Invalid session snapshot {}: {err}; file retained",
                        descriptor.path.display()
                    );
                    continue;
                }
            };
        candidates.push(SessionCandidate {
            path: descriptor.path,
            file_name: descriptor.file_name,
            modified: descriptor.modified,
            state,
            supersedes,
        });
    }

    // A durable envelope supersedes its predecessor regardless of mtime
    // granularity or directory iteration order. Removing referenced claims
    // here ensures two equally-timestamped crash remnants cannot make the old
    // workspace win merely by filename tie-break.
    let superseded_paths: HashSet<PathBuf> = candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .supersedes
                .as_ref()
                .map(|reference| reference.path.clone())
        })
        .collect();
    candidates.retain(|candidate| !superseded_paths.contains(&candidate.path));
    sort_candidates_newest_first(&mut candidates);
    candidates
}

/// Atomically move a recoverable snapshot to this instance's unique claim name
/// without replacing any existing file. anvil targets Linux; on another
/// platform the conservative fallback is to leave the snapshot untouched.
#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "session source path contains an embedded NUL byte",
        )
    })?;
    let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "session claim path contains an embedded NUL byte",
        )
    })?;
    // SAFETY: both C strings remain alive for the syscall and contain no NUL
    // bytes. RENAME_NOREPLACE makes the claim atomic across app instances.
    let result = unsafe {
        nix::libc::renameat2(
            nix::libc::AT_FDCWD,
            source.as_ptr(),
            nix::libc::AT_FDCWD,
            target.as_ptr(),
            nix::libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_source: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace session claims require Linux renameat2",
    ))
}

fn claim_session_from(
    dir: &Path,
    current_token: &str,
    lock_state: &dyn Fn(&str) -> TokenLockState,
) -> Option<ClaimedSnapshot> {
    // A competing startup can win the rename between our scan and claim. Retry
    // so simultaneous launches can each recover a different exited window
    // without ever consuming the same snapshot.
    for _ in 0..8 {
        let candidate = scan_candidates(dir, Some(current_token), lock_state)
            .into_iter()
            .next()?;
        let claim_path = dir.join(format!(
            "{}{}{}",
            candidate.file_name.base_name, CLAIM_MARKER, current_token
        ));
        let retired_owner_locks = match hold_candidate_chain_locks(dir, &candidate) {
            Ok(Some(locks)) => locks,
            Ok(None) => {
                log::debug!(
                    "Session snapshot {} or one of its predecessors became live; rescanning",
                    candidate.path.display()
                );
                continue;
            }
            Err(error) => {
                log::error!(
                    "Cannot lock session snapshot chain rooted at {}: {error}; files retained",
                    candidate.path.display()
                );
                return None;
            }
        };
        match rename_noreplace(&candidate.path, &claim_path) {
            Ok(()) => {
                if let Err(err) = sync_directory(dir) {
                    // The rename still has either its old or new durable name
                    // after a crash. Continue while retaining every predecessor
                    // lock, so no second instance can restore an older branch.
                    log::warn!(
                        "Claimed session snapshot {} as {}, but could not sync the claim directory: {err}; continuing with the recoverable claim",
                        candidate.path.display(),
                        claim_path.display()
                    );
                }
                log::info!(
                    "Claimed session snapshot {} as {}; it will be consumed after a durable checkpoint",
                    candidate.path.display(),
                    claim_path.display()
                );
                return Some(ClaimedSnapshot {
                    state: candidate.state,
                    pending: PendingRestoreClaim {
                        path: claim_path,
                        source_file: candidate.file_name,
                        supersedes: candidate.supersedes,
                        _retired_owner_locks: retired_owner_locks,
                    },
                });
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists
                ) =>
            {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    };
    use std::thread;
    use std::time::Duration;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("anvil-session-{label}-{}-{id}", std::process::id()));
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
                pinned: false,
                private_title: false,
                layout: PaneLayout::Leaf {
                    mode: "block".to_string(),
                    cwd: None,
                    cwd_external: false,
                    remote_name: None,
                    sid: None,
                    cmds: None,
                },
            }],
            ai_conversation: None,
        }
    }

    /// A minimal leaf, and a `depth`-deep chain of splits ending in leaves.
    fn leaf_json() -> String {
        r#"{"type":"leaf","mode":"block"}"#.to_string()
    }

    fn nested_layout_json(depth: usize) -> String {
        let mut layout = leaf_json();
        for _ in 0..depth {
            layout = format!(
                r#"{{"type":"split","orientation":"h","position":100,"start":{layout},"end":{}}}"#,
                leaf_json()
            );
        }
        layout
    }

    fn tabs_json(count: usize, layout: &str) -> String {
        (0..count)
            .map(|index| {
                format!(r#"{{"title":"tab {index}","custom_title":false,"layout":{layout}}}"#)
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn session_json(tabs: &str) -> String {
        format!(r#"{{"active":0,"tabs":[{tabs}]}}"#)
    }

    #[test]
    fn decoding_stops_before_building_an_over_wide_or_over_deep_snapshot() {
        let widest = session_json(&tabs_json(MAX_RESTORED_TABS, &leaf_json()));
        assert_eq!(
            decode_saved_session(&widest).unwrap().tabs.len(),
            MAX_RESTORED_TABS
        );

        // Thousands of tabs still fit the 4 MiB file cap, which is exactly the
        // amplification the seeds prevent.
        let over_wide = session_json(&tabs_json(MAX_RESTORED_TABS * 40, &leaf_json()));
        assert!((over_wide.len() as u64) < MAX_SNAPSHOT_BYTES);
        assert!(decode_saved_session(&over_wide)
            .unwrap_err()
            .to_string()
            .contains("32-tab limit"));

        // A split chain adds one leaf per level, so the deepest acceptable
        // tree is one level shallower than the pane budget.
        let deepest = session_json(&tabs_json(
            1,
            &nested_layout_json(MAX_RESTORED_PANES_PER_TAB - 1),
        ));
        assert!(decode_saved_session(&deepest).is_ok());
        let too_deep = session_json(&tabs_json(1, &nested_layout_json(64)));
        let error = decode_saved_session(&too_deep).unwrap_err().to_string();
        assert!(
            error.contains("nests deeper") || error.contains("16-pane limit"),
            "unexpected error: {error}"
        );

        // Panes are also budgeted across the whole session, not just per tab.
        let over_total = session_json(&tabs_json(
            MAX_RESTORED_TABS,
            &nested_layout_json(MAX_RESTORED_PANES_PER_TAB - 1),
        ));
        assert!(decode_saved_session(&over_total)
            .unwrap_err()
            .to_string()
            .contains("64-pane limit"));
    }

    #[test]
    fn decoding_charges_field_and_argv_budgets() {
        let long_title = "t".repeat(MAX_RESTORED_TITLE_BYTES + 1);
        let tab = format!(
            r#"{{"title":"{long_title}","custom_title":false,"layout":{}}}"#,
            leaf_json()
        );
        assert!(decode_saved_session(&session_json(&tab))
            .unwrap_err()
            .to_string()
            .contains("'title' exceeds"));

        // Escaped text is measured after unescaping, so a short encoded field
        // cannot smuggle a long decoded one.
        let escaped = "\\u0041".repeat(MAX_RESTORED_TITLE_BYTES + 1);
        let tab = format!(
            r#"{{"title":"{escaped}","custom_title":false,"layout":{}}}"#,
            leaf_json()
        );
        assert!(decode_saved_session(&session_json(&tab))
            .unwrap_err()
            .to_string()
            .contains("'title' exceeds"));

        let long_cwd = "c".repeat(MAX_RESTORED_CWD_BYTES + 1);
        let layout = format!(r#"{{"type":"leaf","mode":"block","cwd":"{long_cwd}"}}"#);
        assert!(decode_saved_session(&session_json(&tabs_json(1, &layout)))
            .unwrap_err()
            .to_string()
            .contains("'cwd' exceeds"));

        // An argv over its cumulative budget is dropped rather than restored,
        // and never fails the surrounding snapshot.
        let argument = "a".repeat(MAX_RESTORED_COMMAND_ARG_BYTES);
        let argv = (0..8)
            .map(|_| format!(r#""{argument}""#))
            .collect::<Vec<_>>()
            .join(",");
        let layout = format!(r#"{{"type":"leaf","mode":"block","cmds":[{argv}]}}"#);
        let decoded = decode_saved_session(&session_json(&tabs_json(1, &layout))).unwrap();
        assert!(matches!(
            decoded.tabs[0].layout,
            PaneLayout::Leaf { cmds: None, .. }
        ));
    }

    fn workspace(state: &SnapshotState) -> &SavedSession {
        match state {
            SnapshotState::Workspace(session) => session,
            SnapshotState::Empty => panic!("expected a workspace snapshot"),
        }
    }

    fn layout_with_leaves(count: usize) -> PaneLayout {
        assert!(count > 0);
        let leaf = || PaneLayout::Leaf {
            mode: "block".to_string(),
            cwd: None,
            cwd_external: false,
            remote_name: None,
            sid: None,
            cmds: None,
        };
        let mut layout = leaf();
        for _ in 1..count {
            layout = PaneLayout::Split {
                orientation: 'h',
                position: 50,
                start: Box::new(layout),
                end: Box::new(leaf()),
            };
        }
        layout
    }

    fn set_command_on_leaves(layout: &mut PaneLayout, argv: &[String]) {
        match layout {
            PaneLayout::Leaf { cmds, .. } => *cmds = Some(argv.to_vec()),
            PaneLayout::Split { start, end, .. } => {
                set_command_on_leaves(start, argv);
                set_command_on_leaves(end, argv);
            }
        }
    }

    fn layout_commands_are_empty(layout: &PaneLayout) -> bool {
        match layout {
            PaneLayout::Leaf { cmds, .. } => cmds.is_none(),
            PaneLayout::Split { start, end, .. } => {
                layout_commands_are_empty(start) && layout_commands_are_empty(end)
            }
        }
    }

    #[test]
    fn session_round_trip_restores_a_bounded_ai_chat_collection() {
        let turns = vec![
            jterm_core::ai::Turn {
                role: jterm_core::ai::Role::User,
                text: "why did this fail?".into(),
            },
            jterm_core::ai::Turn {
                role: jterm_core::ai::Role::Assistant,
                text: "because the file is missing".into(),
            },
        ];
        let chat = jterm_core::ai::ChatSnapshot::from_completed_history(
            1,
            "Failure",
            false,
            &turns,
            None,
            "next question",
        );
        let conversation = jterm_core::ai::ConversationSnapshot::from_chats(1, vec![chat])
            .unwrap()
            .to_json()
            .unwrap();
        let mut saved = saved_session("workspace");
        saved.ai_conversation = Some(conversation.clone());
        let decoded = decode_saved_session(&serde_json::to_string(&saved).unwrap()).unwrap();
        assert_eq!(
            decoded.ai_conversation.as_deref(),
            Some(conversation.as_str())
        );
    }

    #[test]
    fn invalid_ai_chat_json_does_not_poison_workspace_restore() {
        let encoded = session_json(&tabs_json(1, &leaf_json())).replace(
            r#""tabs":"#,
            r#""ai_conversation":"not a snapshot","tabs":"#,
        );
        let decoded = decode_saved_session(&encoded).unwrap();
        assert!(decoded.ai_conversation.is_none());
        assert_eq!(decoded.tabs.len(), 1);
    }

    #[test]
    fn restore_limits_bound_tabs_panes_and_user_visible_fields() {
        let base = saved_session("bounded").tabs.remove(0);
        let mut maximum = SavedSession {
            active: 0,
            tabs: (0..4)
                .map(|_| SavedTab {
                    layout: layout_with_leaves(16),
                    ..base.clone()
                })
                .collect(),
            ai_conversation: None,
        };
        assert!(session_within_restore_limits(&maximum));

        maximum.tabs.push(SavedTab {
            layout: layout_with_leaves(1),
            ..base.clone()
        });
        assert!(!session_within_restore_limits(&maximum));

        let too_many_tabs = SavedSession {
            active: 0,
            tabs: vec![base.clone(); MAX_RESTORED_TABS + 1],
            ai_conversation: None,
        };
        assert!(!session_within_restore_limits(&too_many_tabs));

        let too_wide = SavedSession {
            active: 0,
            tabs: vec![SavedTab {
                layout: layout_with_leaves(MAX_RESTORED_PANES_PER_TAB + 1),
                ..base.clone()
            }],
            ai_conversation: None,
        };
        assert!(!session_within_restore_limits(&too_wide));

        let oversized_title = SavedSession {
            active: 0,
            tabs: vec![SavedTab {
                title: "x".repeat(MAX_RESTORED_TITLE_BYTES + 1),
                ..base.clone()
            }],
            ai_conversation: None,
        };
        assert!(!session_within_restore_limits(&oversized_title));

        for argv in [
            std::iter::once("ssh".to_string())
                .chain((0..MAX_RESTORED_COMMAND_ARGS).map(|_| "x".to_string()))
                .collect::<Vec<_>>(),
            vec![
                "ssh".to_string(),
                "x".repeat(MAX_RESTORED_COMMAND_ARG_BYTES + 1),
            ],
            vec!["ssh".to_string(), "host\nname".to_string()],
        ] {
            let oversized_command = SavedSession {
                active: 0,
                tabs: vec![SavedTab {
                    layout: PaneLayout::Leaf {
                        mode: "block".to_string(),
                        cwd: None,
                        cwd_external: true,
                        remote_name: None,
                        sid: None,
                        cmds: Some(argv),
                    },
                    ..base.clone()
                }],
                ai_conversation: None,
            };
            assert!(!session_within_restore_limits(&oversized_command));
        }
    }

    #[test]
    fn capture_constructors_enforce_the_same_field_budgets_as_restore() {
        let maximum_sid = "s".repeat(MAX_RESTORED_SID_BYTES);
        let layout = PaneLayout::captured_leaf(
            "m".repeat(MAX_RESTORED_MODE_BYTES + 1),
            Some("c".repeat(MAX_RESTORED_CWD_BYTES + 1)),
            true,
            Some("r".repeat(MAX_RESTORED_REMOTE_NAME_BYTES + 1)),
            Some(maximum_sid.clone()),
            Some(vec![
                "ssh".to_string(),
                "x".repeat(MAX_RESTORED_COMMAND_ARG_BYTES + 1),
            ]),
        );
        assert!(matches!(
            &layout,
            PaneLayout::Leaf {
                mode,
                cwd: None,
                cwd_external: true,
                remote_name: None,
                sid: Some(sid),
                cmds: None,
            } if mode == "block" && sid == &maximum_sid
        ));

        let oversized_sid = PaneLayout::captured_leaf(
            "block".to_string(),
            None,
            false,
            None,
            Some("s".repeat(MAX_RESTORED_SID_BYTES + 1)),
            None,
        );
        assert!(matches!(oversized_sid, PaneLayout::Leaf { sid: None, .. }));

        let invalid_sid = PaneLayout::captured_leaf(
            "block".to_string(),
            None,
            false,
            None,
            Some("session.with.dots".to_string()),
            None,
        );
        assert!(matches!(invalid_sid, PaneLayout::Leaf { sid: None, .. }));

        let title = "界".repeat(MAX_RESTORED_TITLE_BYTES / "界".len() + 2);
        let tab = SavedTab::captured(title, true, false, false, layout);
        assert!(tab.title.len() <= MAX_RESTORED_TITLE_BYTES);
        assert!(tab.title.is_char_boundary(tab.title.len()));
        let mut captured = SavedSession::captured(0, vec![tab], Some("not a chat snapshot".into()));
        assert!(captured.ai_conversation.is_some());
        assert!(drop_invalid_ai_conversation(&mut captured));
        assert!(captured.ai_conversation.is_none());
        assert!(session_within_restore_limits(&captured));
    }

    #[test]
    fn live_workspace_growth_stops_at_the_restore_capacity() {
        assert!(can_add_persisted_tab(
            MAX_RESTORED_TABS - 1,
            MAX_RESTORED_PANES_TOTAL - 1,
            true
        ));
        assert!(!can_add_persisted_tab(
            MAX_RESTORED_TABS,
            MAX_RESTORED_TABS,
            false
        ));
        assert!(!can_add_persisted_tab(1, MAX_RESTORED_PANES_TOTAL, true));
        assert!(can_add_persisted_pane(
            MAX_RESTORED_PANES_PER_TAB - 1,
            MAX_RESTORED_PANES_TOTAL - 1
        ));
        assert!(!can_add_persisted_pane(
            MAX_RESTORED_PANES_PER_TAB,
            MAX_RESTORED_PANES_PER_TAB
        ));
        assert!(!can_add_persisted_pane(1, MAX_RESTORED_PANES_TOTAL));
    }

    #[test]
    fn save_rejects_every_restore_limit_and_keeps_last_good_snapshot() {
        let dir = TestDir::new("save-limits");
        let owner = SnapshotOwner::create_in(dir.path()).unwrap();
        let good = saved_session("last-good");
        checkpoint_snapshot_for_owner(&owner, &good).unwrap();
        let expected = fs::read(&owner.state_path).unwrap();
        let base = good.tabs[0].clone();

        let too_many_tabs = SavedSession {
            active: 0,
            tabs: vec![base.clone(); MAX_RESTORED_TABS + 1],
            ai_conversation: None,
        };
        let too_many_in_one_tab = SavedSession {
            active: 0,
            tabs: vec![SavedTab {
                layout: layout_with_leaves(MAX_RESTORED_PANES_PER_TAB + 1),
                ..base.clone()
            }],
            ai_conversation: None,
        };
        let too_many_total = SavedSession {
            active: 0,
            tabs: (0..5)
                .map(|_| SavedTab {
                    layout: layout_with_leaves(13),
                    ..base.clone()
                })
                .collect(),
            ai_conversation: None,
        };
        let oversized_payload = SavedSession {
            active: 0,
            tabs: vec![SavedTab {
                layout: PaneLayout::Leaf {
                    mode: "block".to_string(),
                    cwd: None,
                    cwd_external: true,
                    remote_name: None,
                    sid: None,
                    cmds: Some(vec![
                        "ssh".to_string(),
                        "x".repeat(MAX_SNAPSHOT_BYTES as usize),
                    ]),
                },
                ..base
            }],
            ai_conversation: None,
        };

        for rejected in [
            too_many_tabs,
            too_many_in_one_tab,
            too_many_total,
            oversized_payload,
        ] {
            assert!(checkpoint_snapshot_for_owner(&owner, &rejected).is_err());
            assert_eq!(
                fs::read(&owner.state_path).unwrap(),
                expected,
                "a rejected save must not replace the last-good snapshot"
            );
        }
    }

    #[test]
    fn aggregate_size_fallback_keeps_the_workspace_without_optional_commands() {
        let dir = TestDir::new("aggregate-save-budget");
        let owner = SnapshotOwner::create_in(dir.path()).unwrap();
        let base = saved_session("large-but-valid").tabs.remove(0);
        let argv = vec![
            "ssh".to_string(),
            "x".repeat(MAX_RESTORED_COMMAND_ARG_BYTES),
        ];
        let tabs = (0..4)
            .map(|index| {
                let mut layout = layout_with_leaves(MAX_RESTORED_PANES_PER_TAB);
                set_command_on_leaves(&mut layout, &argv);
                SavedTab {
                    title: format!("tab-{index}"),
                    layout,
                    ..base.clone()
                }
            })
            .collect();
        let session = SavedSession {
            active: 2,
            tabs,
            ai_conversation: None,
        };
        assert!(session_within_restore_limits(&session));
        assert_eq!(
            serialize_snapshot_bounded(&owner, &session)
                .unwrap_err()
                .kind(),
            io::ErrorKind::FileTooLarge
        );

        checkpoint_snapshot_resilient(&owner, session).unwrap();
        let encoded = read_snapshot_bounded(&owner.state_path).unwrap();
        let decoded = decode_session_envelope(&encoded).unwrap();
        let restored = workspace(&decoded.state);
        assert_eq!(restored.active, 2);
        assert_eq!(restored.tabs.len(), 4);
        assert!(restored
            .tabs
            .iter()
            .all(|tab| layout_commands_are_empty(&tab.layout)));
    }

    #[cfg(unix)]
    #[test]
    fn empty_tombstone_is_durable_consumed_and_never_restores_old_workspace() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("empty-tombstone");
        let original = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&original.state_path, "must-not-resurrect");
        drop(original);

        let closer = SnapshotOwner::create_in(dir.path()).unwrap();
        let claimed = claim_session_from(dir.path(), &closer.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("claim workspace before closing its final tab");
        assert_eq!(
            workspace(&claimed.state).tabs[0].title,
            "must-not-resurrect"
        );
        let old_claim_path = claimed.pending.path.clone();
        closer.remember_pending_restore(claimed.pending).unwrap();
        checkpoint_snapshot_for_owner(&closer, &SavedSession::default()).unwrap();

        assert!(!old_claim_path.exists());
        let tombstone_contents = read_snapshot_bounded(&closer.state_path).unwrap();
        let closer_file =
            parse_state_file_name(closer.state_path.file_name().unwrap().to_str().unwrap())
                .unwrap();
        let (state, _) =
            parse_snapshot_payload(&tombstone_contents, dir.path(), &closer_file).unwrap();
        assert!(matches!(state, SnapshotState::Empty));
        assert_eq!(
            fs::metadata(&closer.state_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        drop(closer);
        let next = SnapshotOwner::create_in(dir.path()).unwrap();
        let claimed_tombstone = claim_session_from(dir.path(), &next.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("next startup claims the empty tombstone");
        assert!(matches!(&claimed_tombstone.state, SnapshotState::Empty));
        let consumed_path = claimed_tombstone.pending.path.clone();
        assert!(adopt_claimed_snapshot(&next, claimed_tombstone)
            .unwrap()
            .is_none());
        assert!(!consumed_path.exists());
        assert!(
            next.state_path.exists(),
            "empty state must be propagated durably"
        );
    }

    #[cfg(unix)]
    #[test]
    fn crash_after_durable_tombstone_before_claim_cleanup_finishes_chain() {
        let dir = TestDir::new("tombstone-crash-window");
        let original = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&original.state_path, "old-workspace");
        drop(original);

        let crashed = SnapshotOwner::create_in(dir.path()).unwrap();
        let claimed = claim_session_from(dir.path(), &crashed.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("claim old workspace");
        let old_claim_path = claimed.pending.path.clone();
        crashed.remember_pending_restore(claimed.pending).unwrap();
        thread::sleep(Duration::from_millis(10));
        write_snapshot_for_owner(&crashed, &SavedSession::default()).unwrap();
        assert!(old_claim_path.exists());
        assert!(crashed.state_path.exists());

        // Simulate process death after the tombstone's rename+fsync and before
        // commit_pending_restore. The tombstone points to the still-durable old
        // claim, so a future owner can finish both cleanup steps.
        drop(crashed);
        let next = SnapshotOwner::create_in(dir.path()).unwrap();
        let recovered = claim_session_from(dir.path(), &next.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("recover durable tombstone");
        assert!(matches!(&recovered.state, SnapshotState::Empty));
        let tombstone_claim_path = recovered.pending.path.clone();

        let competitor = SnapshotOwner::create_in(dir.path()).unwrap();
        assert!(
            claim_session_from(dir.path(), &competitor.token, &|token| {
                token_lock_state_in(dir.path(), token)
            })
            .is_none(),
            "held predecessor locks must prevent a second startup from reviving the old workspace"
        );
        drop(competitor);

        assert!(adopt_claimed_snapshot(&next, recovered).unwrap().is_none());
        assert!(
            !old_claim_path.exists(),
            "superseded workspace claim must be removed"
        );
        assert!(
            !tombstone_claim_path.exists(),
            "consumed tombstone claim must be removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn crash_after_durable_workspace_before_claim_cleanup_never_revives_predecessor() {
        let dir = TestDir::new("workspace-crash-window");
        let original = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&original.state_path, "old-workspace");
        drop(original);

        let crashed = SnapshotOwner::create_in(dir.path()).unwrap();
        let claimed = claim_session_from(dir.path(), &crashed.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("claim old workspace");
        let old_claim_path = claimed.pending.path.clone();
        crashed.remember_pending_restore(claimed.pending).unwrap();
        thread::sleep(Duration::from_millis(10));
        write_snapshot_for_owner(&crashed, &saved_session("new-workspace")).unwrap();
        drop(crashed);

        let next = SnapshotOwner::create_in(dir.path()).unwrap();
        let recovered = claim_session_from(dir.path(), &next.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("recover newest durable workspace");
        let recovered_claim_path = recovered.pending.path.clone();
        let session = adopt_claimed_snapshot(&next, recovered)
            .unwrap()
            .expect("workspace envelope must restore");
        assert_eq!(session.tabs[0].title, "new-workspace");
        checkpoint_snapshot_for_owner(&next, &session).unwrap();

        assert!(!old_claim_path.exists());
        assert!(!recovered_claim_path.exists());
        drop(next);

        let final_loader = SnapshotOwner::create_in(dir.path()).unwrap();
        let final_snapshot = claim_session_from(dir.path(), &final_loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("only the replacement workspace remains");
        assert_eq!(
            workspace(&final_snapshot.state).tabs[0].title,
            "new-workspace"
        );
    }

    #[test]
    fn bare_legacy_snapshot_remains_compatible_with_envelope_reader() {
        let dir = TestDir::new("legacy-envelope-compat");
        let path = dir.path().join(LEGACY_STATE_FILE);
        write_session(&path, "legacy-bare");

        let claimed = claim_session_from(dir.path(), &token(99), &|_| TokenLockState::Available)
            .expect("legacy bare snapshot remains recoverable");
        assert_eq!(workspace(&claimed.state).tabs[0].title, "legacy-bare");
        assert!(claimed.pending.supersedes.is_none());
    }

    #[test]
    fn malformed_tombstone_is_rejected_and_retained() {
        let dir = TestDir::new("invalid-tombstone");
        let path = dir.path().join(LEGACY_STATE_FILE);
        atomic_write(
            &path,
            br#"{"format":"anvil-session","version":2,"payload":{"kind":"empty"}}"#,
        )
        .unwrap();

        assert!(scan_candidates(dir.path(), Some(&token(99)), &|_| {
            TokenLockState::Available
        })
        .is_empty());
        assert!(path.exists(), "invalid tombstone must remain inspectable");
    }

    #[test]
    fn tombstone_rejects_supersedes_path_traversal_without_touching_target() {
        let root = TestDir::new("tombstone-path-traversal");
        let dir = root.path().join("state");
        fs::create_dir(&dir).unwrap();
        let victim = root.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        let path = state_file_path_for_token(&dir, &token(10));
        atomic_write(
            &path,
            br#"{"format":"anvil-session","version":1,"payload":{"kind":"empty"},"supersedes":"../victim"}"#,
        )
        .unwrap();

        assert!(
            scan_candidates(&dir, Some(&token(99)), &|_| { TokenLockState::Available }).is_empty()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(path.exists());
    }

    #[test]
    fn disk_restore_drops_arbitrary_argv_and_preserves_known_remote_argv() {
        let dir = TestDir::new("sanitize-restorable-argv");
        let commands = [
            vec!["sh", "-c", "touch /tmp/must-not-run"],
            vec!["ssh", "example.test"],
            vec!["mosh", "example.test"],
            vec!["docker", "exec", "container", "bash"],
            vec!["podman", "compose", "exec", "service", "sh"],
        ];
        let tabs = commands
            .iter()
            .enumerate()
            .map(|(index, argv)| SavedTab {
                title: format!("command-{index}"),
                custom_title: false,
                pinned: false,
                private_title: false,
                layout: PaneLayout::Leaf {
                    mode: "block".to_string(),
                    cwd: None,
                    cwd_external: index > 0,
                    remote_name: None,
                    sid: None,
                    cmds: Some(argv.iter().map(|part| (*part).to_string()).collect()),
                },
            })
            .collect();
        let saved = SavedSession {
            active: 0,
            tabs,
            ai_conversation: None,
        };
        let path = dir.path().join(LEGACY_STATE_FILE);
        atomic_write(&path, &serde_json::to_vec(&saved).unwrap()).unwrap();

        let claimed = claim_session_from(dir.path(), &token(99), &|_| TokenLockState::Available)
            .expect("claim test snapshot");
        let restored: Vec<Option<Vec<String>>> = workspace(&claimed.state)
            .tabs
            .iter()
            .map(|tab| match &tab.layout {
                PaneLayout::Leaf { cmds, .. } => cmds.clone(),
                PaneLayout::Split { .. } => unreachable!(),
            })
            .collect();

        assert_eq!(restored[0], None, "sh -c must never reach pane spawn");
        for (actual, expected) in restored[1..].iter().zip(&commands[1..]) {
            assert_eq!(
                actual.as_ref().unwrap(),
                &expected
                    .iter()
                    .map(|part| (*part).to_string())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_reader_rejects_symlinks_hard_links_and_fifos() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;
        use std::os::unix::fs::symlink;

        let dir = TestDir::new("snapshot-file-types");
        let victim = dir.path().join("victim.json");
        let link = dir.path().join("link.state");
        let hard_link = dir.path().join("hard.state");
        let fifo = dir.path().join("fifo.state");
        fs::write(&victim, b"{}").unwrap();
        assert_eq!(read_snapshot_bounded(&victim).unwrap(), "{}");

        symlink(&victim, &link).unwrap();
        assert!(read_snapshot_bounded(&link).is_err());
        fs::hard_link(&victim, &hard_link).unwrap();
        assert!(read_snapshot_bounded(&hard_link).is_err());
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let started = std::time::Instant::now();
        assert!(read_snapshot_bounded(&fifo).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(fs::read(&victim).unwrap(), b"{}");
    }

    #[test]
    fn pane_session_id_round_trips_and_invalid_snapshots_degrade_safely() {
        let with_sid = PaneLayout::Leaf {
            mode: "block".to_string(),
            cwd: Some("/tmp".to_string()),
            cwd_external: false,
            remote_name: None,
            sid: Some("jsh-session-42".to_string()),
            cmds: None,
        };
        let encoded = serde_json::to_string(&with_sid).unwrap();
        assert!(encoded.contains("jsh-session-42"));
        let decoded = decode_pane_layout(&encoded).unwrap();
        assert!(matches!(
            decoded,
            PaneLayout::Leaf {
                sid: Some(ref sid),
                ..
            } if sid == "jsh-session-42"
        ));

        let legacy =
            decode_pane_layout(r#"{"type":"leaf","mode":"block","cwd":"/tmp","cmds":null}"#)
                .unwrap();
        assert!(matches!(legacy, PaneLayout::Leaf { sid: None, .. }));

        for invalid in ["session.with.dots", "session with spaces", "雪"] {
            let encoded = format!(
                r#"{{"type":"leaf","mode":"block","sid":{}}}"#,
                serde_json::to_string(invalid).unwrap()
            );
            let decoded = decode_pane_layout(&encoded).unwrap();
            assert!(matches!(decoded, PaneLayout::Leaf { sid: None, .. }));
        }
    }

    #[test]
    fn snapshot_decoders_reject_duplicate_members_at_every_depth() {
        fn duplicate_error<T>(result: Result<T, serde_json::Error>) -> String {
            match result {
                Ok(_) => panic!("ambiguous snapshot unexpectedly decoded"),
                Err(error) => error.to_string(),
            }
        }

        for error in [
            duplicate_error(decode_saved_session(
                r#"{"active":0,"tabs":[{"title":"tab","custom_title":false,"layout":{"type":"leaf","mode":"block","sid":"first","sid":"second"}}]}"#,
            )),
            duplicate_error(decode_saved_session(
                r#"{"active":0,"tabs":[],"future":{"scope":"read","scope":"write"}}"#,
            )),
            duplicate_error(decode_session_envelope(
                r#"{"format":"future","version":1,"payload":{"kind":"empty","k\u0069nd":"workspace"}}"#,
            )),
        ] {
            assert!(error.contains("duplicate JSON object member"), "{error}");
            assert!(!error.contains("sid"), "{error}");
            assert!(!error.contains("scope"), "{error}");
            assert!(!error.contains("kind"), "{error}");
        }
    }

    #[test]
    fn snapshot_decoders_reject_serde_json_raw_value_sentinel() {
        let error = match decode_saved_session(
            r#"{"active":0,"tabs":[],"future":{"$serde_json::private::RawValue":"{\"scope\":\"read\",\"scope\":\"write\"}"}}"#,
        ) {
            Ok(_) => panic!("reserved serde_json sentinel unexpectedly decoded"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("reserved JSON object member"), "{error}");
        assert!(!error.contains("RawValue"), "{error}");
        assert!(!error.contains("scope"), "{error}");
    }

    #[test]
    fn restorable_command_argv_round_trips_without_losing_boundaries() {
        let argv = vec![
            "ssh".to_string(),
            "example.test".to_string(),
            "printf '%s, %s'; touch /tmp/stays-remote".to_string(),
        ];
        let layout = PaneLayout::Leaf {
            mode: "block".to_string(),
            cwd: Some("/tmp".to_string()),
            cwd_external: true,
            remote_name: Some("example".to_string()),
            sid: None,
            cmds: Some(argv.clone()),
        };

        let encoded = serde_json::to_string(&layout).unwrap();
        let decoded = decode_pane_layout(&encoded).unwrap();
        assert!(matches!(
            decoded,
            PaneLayout::Leaf {
                cmds: Some(ref decoded_argv),
                cwd_external: true,
                remote_name: Some(ref remote_name),
                ..
            } if decoded_argv == &argv && remote_name == "example"
        ));
    }

    #[test]
    fn legacy_joined_restore_command_is_loaded_but_not_replayed() {
        let legacy = decode_pane_layout(
            r#"{"type":"leaf","mode":"block","cwd":"/tmp","cmds":"ssh host; touch /tmp/local"}"#,
        )
        .unwrap();
        assert!(matches!(legacy, PaneLayout::Leaf { cmds: None, .. }));
    }

    fn write_session(path: &Path, title: &str) {
        let payload = serde_json::to_vec(&saved_session(title)).expect("serialize test session");
        atomic_write(path, &payload).expect("write test session");
    }

    fn token(id: u64) -> String {
        format!("00000000-0000-4000-8000-{id:012x}")
    }

    #[test]
    fn tab_flags_round_trip_and_legacy_snapshots_default_off() {
        let mut current = saved_session("pinned");
        current.tabs[0].pinned = true;
        current.tabs[0].private_title = true;
        let encoded = serde_json::to_string(&current).expect("serialize pinned session");
        let decoded = decode_saved_session(&encoded).expect("deserialize pinned session");
        assert!(decoded.tabs[0].pinned);
        assert!(decoded.tabs[0].private_title);

        let legacy = r#"{
            "active": 0,
            "tabs": [{
                "title": "legacy",
                "custom_title": false,
                "layout": {"type": "leaf", "mode": "block"}
            }]
        }"#;
        let decoded = decode_saved_session(legacy).expect("deserialize legacy session");
        assert!(!decoded.tabs[0].pinned);
        assert!(!decoded.tabs[0].private_title);
    }

    #[test]
    fn parses_token_pid_legacy_and_claim_names() {
        let owner_token = token(1);
        let claimer_token = token(2);
        assert_eq!(
            parse_state_file_name("tabs.state"),
            Some(StateFileName {
                base_name: "tabs.state".to_string(),
                owner: None,
                claimer: None,
            })
        );
        assert_eq!(
            parse_state_file_name("tabs.42.state"),
            Some(StateFileName {
                base_name: "tabs.42.state".to_string(),
                owner: Some(InstanceIdentity::LegacyPid(42)),
                claimer: None,
            })
        );
        assert_eq!(
            parse_state_file_name("tabs.42.state.claim.77"),
            Some(StateFileName {
                base_name: "tabs.42.state".to_string(),
                owner: Some(InstanceIdentity::LegacyPid(42)),
                claimer: Some(InstanceIdentity::LegacyPid(77)),
            })
        );
        let token_name = format!("tabs.{owner_token}.state");
        assert_eq!(
            parse_state_file_name(&token_name),
            Some(StateFileName {
                base_name: token_name.clone(),
                owner: Some(InstanceIdentity::Token(owner_token.clone())),
                claimer: None,
            })
        );
        let claim_name = format!("{token_name}{CLAIM_MARKER}{claimer_token}");
        assert_eq!(
            parse_state_file_name(&claim_name),
            Some(StateFileName {
                base_name: token_name,
                owner: Some(InstanceIdentity::Token(owner_token)),
                claimer: Some(InstanceIdentity::Token(claimer_token)),
            })
        );
        assert!(parse_state_file_name("tabs.42.state.tmp").is_none());
        assert!(parse_state_file_name("tabs.not-a-pid.state").is_none());
        assert!(parse_state_file_name("tabs.42.state.claim.77.claim.88").is_none());
    }

    #[test]
    fn snapshot_counts_separate_recoverable_and_live_files() {
        let dir = TestDir::new("snapshot-counts");
        let available_owner = token(10);
        let live_owner = token(20);
        let claimed_owner = token(30);
        let exited_claimer = token(40);
        let live_claimer = token(50);
        let current = token(99);
        let names = [
            format!("tabs.{available_owner}.state"),
            format!("tabs.{live_owner}.state"),
            format!("tabs.{claimed_owner}.state.claim.{exited_claimer}"),
            format!("tabs.{claimed_owner}.state.claim.{live_claimer}"),
            format!("tabs.{current}.state"),
            "tabs.123.state".to_string(),
            "not-a-session.txt".to_string(),
        ];
        for name in names {
            fs::write(dir.path().join(name), b"{}").unwrap();
        }
        let counts = session_snapshot_counts_in(dir.path(), Some(&current), &|token| {
            if token == available_owner || token == exited_claimer {
                TokenLockState::Available
            } else {
                TokenLockState::Held
            }
        });
        assert_eq!(counts, (2, 4));
    }

    #[test]
    fn recoverability_requires_an_available_token_lock_and_preserves_pid_files() {
        let owner = token(10);
        let claimer = token(20);
        let current = token(99);
        let owned = parse_state_file_name(&format!("tabs.{owner}.state")).unwrap();
        let claimed =
            parse_state_file_name(&format!("tabs.{owner}.state.claim.{claimer}")).unwrap();
        let pid_owned = parse_state_file_name("tabs.10.state").unwrap();
        let pid_claimed = parse_state_file_name("tabs.10.state.claim.20").unwrap();
        let legacy = parse_state_file_name("tabs.state").unwrap();

        assert!(state_file_is_recoverable(&owned, Some(&current), &|_| {
            TokenLockState::Available
        }));
        assert!(!state_file_is_recoverable(&owned, Some(&current), &|_| {
            TokenLockState::Held
        }));
        assert!(!state_file_is_recoverable(&owned, Some(&current), &|_| {
            TokenLockState::Missing
        }));
        assert!(!state_file_is_recoverable(&owned, Some(&current), &|_| {
            TokenLockState::Unknown
        }));
        assert!(state_file_is_recoverable(
            &claimed,
            Some(&current),
            &|token| {
                if token == claimer {
                    TokenLockState::Available
                } else {
                    TokenLockState::Held
                }
            }
        ));
        assert!(!state_file_is_recoverable(&owned, Some(&owner), &|_| {
            TokenLockState::Available
        }));
        assert!(!state_file_is_recoverable(
            &pid_owned,
            Some(&current),
            &|_| TokenLockState::Available
        ));
        assert!(!state_file_is_recoverable(
            &pid_claimed,
            Some(&current),
            &|_| TokenLockState::Available
        ));
        assert!(state_file_is_recoverable(&legacy, Some(&current), &|_| {
            TokenLockState::Unknown
        }));
    }

    #[cfg(unix)]
    #[test]
    fn orphan_cleanup_cannot_observe_the_owner_lock_publication_window() {
        let dir = TestDir::new("owner-publish-race");
        let creator_dir = dir.path().to_path_buf();
        let cleanup_dir = creator_dir.clone();
        let (published_tx, published_rx) = mpsc::channel();
        let (publish_tx, publish_rx) = mpsc::channel();

        let creator = thread::spawn(move || {
            let mut before_flock = |path: &Path| {
                published_tx.send(path.to_path_buf()).unwrap();
                publish_rx.recv().unwrap();
            };
            SnapshotOwner::create_in_with_publish_hook(&creator_dir, &mut before_flock)
        });

        // Generous liveness bound: under a loaded parallel test run, thread
        // scheduling alone can exceed a small timeout and flake this test.
        let lock_path = published_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("creator must pause after publishing the final lock pathname");
        let publishing_token = parse_lock_file_token(
            lock_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            token_lock_state_in(dir.path(), &publishing_token),
            TokenLockState::Unknown,
            "liveness probes must remain conservative during publication"
        );
        let unlocked_owner_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(
            try_lock_file_exclusive(&unlocked_owner_file).unwrap(),
            "the hook must expose the exact pre-flock publication window"
        );
        drop(unlocked_owner_file);

        let protocol_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join(LOCK_PROTOCOL_FILE))
            .unwrap();
        assert!(
            !try_lock_file_exclusive(&protocol_file).unwrap(),
            "the publisher must hold the protocol guard throughout the window"
        );
        drop(protocol_file);

        let (cleanup_done_tx, cleanup_done_rx) = mpsc::channel();
        let cleanup = thread::spawn(move || {
            cleanup_orphaned_locks(&cleanup_dir);
            cleanup_done_tx.send(()).unwrap();
        });
        cleanup_done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("busy cleanup must conservatively skip without blocking");
        cleanup.join().unwrap();
        assert!(
            lock_path.exists(),
            "cleanup must not unlink a pathname whose publication is in progress"
        );

        publish_tx.send(()).unwrap();
        let owner = creator.join().unwrap().unwrap();

        assert_eq!(owner.state_path.with_extension("lock"), lock_path);
        assert!(
            lock_path.exists(),
            "cleanup must preserve the now-locked owner pathname"
        );
        assert_eq!(
            token_lock_state_in(dir.path(), &owner.token),
            TokenLockState::Held
        );

        drop(owner);
        cleanup_orphaned_locks(dir.path());
        assert!(
            !lock_path.exists(),
            "an exited owner lock must remain reclaimable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_flock_keeps_live_snapshot_while_exited_snapshot_is_checkpointed() {
        let dir = TestDir::new("live-owner");
        let live_owner = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&live_owner.state_path, "live");
        let exited_owner = SnapshotOwner::create_in(dir.path()).unwrap();
        let exited_path = exited_owner.state_path.clone();
        let exited_lock = lock_file_path_for_token(dir.path(), &exited_owner.token);
        write_session(&exited_path, "exited");
        drop(exited_owner);
        let loader = SnapshotOwner::create_in(dir.path()).unwrap();

        let claimed = claim_session_from(dir.path(), &loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("restore exited owner");
        assert_eq!(workspace(&claimed.state).tabs[0].title, "exited");
        assert!(claimed.pending.path.exists());
        let session = workspace(&claimed.state).clone();
        loader.remember_pending_restore(claimed.pending).unwrap();
        checkpoint_snapshot_for_owner(&loader, &session).unwrap();
        assert!(
            live_owner.state_path.exists(),
            "live owner's file must remain"
        );
        assert!(!exited_path.exists(), "restored file must remain claimed");
        assert!(
            !exited_lock.exists(),
            "consuming the last reference must clean its stale owner lock"
        );
        assert_eq!(
            token_lock_state_in(dir.path(), &live_owner.token),
            TokenLockState::Held
        );
    }

    #[cfg(unix)]
    #[test]
    fn unique_owner_tokens_never_overwrite_an_existing_snapshot() {
        let dir = TestDir::new("unique-owner");
        let first = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&first.state_path, "first");
        let second = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&second.state_path, "second");

        assert_ne!(first.token, second.token);
        assert_ne!(first.state_path, second.state_path);
        let first_saved =
            decode_saved_session(&fs::read_to_string(&first.state_path).unwrap()).unwrap();
        let second_saved =
            decode_saved_session(&fs::read_to_string(&second.state_path).unwrap()).unwrap();
        assert_eq!(first_saved.tabs[0].title, "first");
        assert_eq!(second_saved.tabs[0].title, "second");
        assert_eq!(
            token_lock_state_in(dir.path(), &first.token),
            TokenLockState::Held
        );
        assert_eq!(
            token_lock_state_in(dir.path(), &second.token),
            TokenLockState::Held
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_lock_descriptors_close_across_exec() {
        let dir = TestDir::new("lock-cloexec");
        let owner = SnapshotOwner::create_in(dir.path()).unwrap();
        let protocol = LockProtocolGuard::acquire(dir.path()).unwrap();
        let probe =
            open_existing_lock_file(&lock_file_path_for_token(dir.path(), &owner.token)).unwrap();

        for descriptor in [
            owner._lock_file.as_raw_fd(),
            protocol._file.as_raw_fd(),
            probe.as_raw_fd(),
        ] {
            // SAFETY: every descriptor is owned by a live `File` value above.
            let flags = unsafe { nix::libc::fcntl(descriptor, nix::libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & nix::libc::FD_CLOEXEC, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn protocol_acquire_times_out_instead_of_blocking_forever() {
        let dir = TestDir::new("protocol-timeout");
        let held = LockProtocolGuard::acquire(dir.path()).unwrap();
        let started = Instant::now();
        let error = LockProtocolGuard::acquire_with_timeout(dir.path(), Duration::from_millis(25))
            .err()
            .expect("a second protocol acquisition must time out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
        assert!(
            LockProtocolGuard::acquire_with_timeout(dir.path(), Duration::from_millis(25)).is_ok(),
            "the protocol must remain usable after the holder releases it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn protocol_directory_lock_survives_protocol_entry_replacement() {
        let dir = TestDir::new("protocol-entry-replacement");
        let held = LockProtocolGuard::acquire(dir.path()).unwrap();
        let protocol_path = dir.path().join(LOCK_PROTOCOL_FILE);
        let retired_path = dir.path().join("retired-protocol-lock");
        fs::rename(&protocol_path, &retired_path).unwrap();

        let error = LockProtocolGuard::acquire_with_timeout(dir.path(), Duration::from_millis(25))
            .err()
            .expect("replacing the lock pathname must not bypass the directory lock");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        drop(held);
        let reacquired =
            LockProtocolGuard::acquire_with_timeout(dir.path(), Duration::from_millis(25))
                .expect("the protocol remains usable after the original guard exits");
        drop(reacquired);
        fs::remove_file(retired_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dropping_logical_guards_unlocks_fork_inherited_descriptors() {
        use std::os::fd::FromRawFd;

        let dir = TestDir::new("lock-fork-inheritance");
        let owner = SnapshotOwner::create_in(dir.path()).unwrap();
        let owner_lock_path = lock_file_path_for_token(dir.path(), &owner.token);
        // `dup` models the shared open-file description inherited by fork.
        // SAFETY: `dup` returns a new owned descriptor on success.
        let inherited_owner_fd = unsafe { nix::libc::dup(owner._lock_file.as_raw_fd()) };
        assert_ne!(inherited_owner_fd, -1);
        // SAFETY: the successful `dup` transferred ownership of this descriptor.
        let inherited_owner = unsafe { File::from_raw_fd(inherited_owner_fd) };
        drop(owner);

        let owner_probe = open_existing_lock_file(&owner_lock_path).unwrap();
        assert!(try_lock_file_exclusive(&owner_probe).unwrap());
        unlock_file(&owner_probe);
        drop(owner_probe);
        drop(inherited_owner);

        let protocol = LockProtocolGuard::acquire(dir.path()).unwrap();
        // SAFETY: same `dup` ownership argument as above.
        let inherited_protocol_fd = unsafe { nix::libc::dup(protocol._file.as_raw_fd()) };
        assert_ne!(inherited_protocol_fd, -1);
        // SAFETY: the successful `dup` transferred ownership of this descriptor.
        let inherited_protocol = unsafe { File::from_raw_fd(inherited_protocol_fd) };
        drop(protocol);

        let reacquired = LockProtocolGuard::try_acquire(dir.path())
            .unwrap()
            .expect("logical guard drop must unlock despite an inherited descriptor");
        drop(reacquired);
        drop(inherited_protocol);

        let probe = open_existing_lock_file(&owner_lock_path).unwrap();
        // SAFETY: `dup` returns a separately owned descriptor for the same
        // open-file description, exactly as a concurrent fork would inherit.
        let inherited_probe_fd = unsafe { nix::libc::dup(probe.as_raw_fd()) };
        assert_ne!(inherited_probe_fd, -1);
        // SAFETY: the successful `dup` transferred ownership of this descriptor.
        let inherited_probe = unsafe { File::from_raw_fd(inherited_probe_fd) };
        {
            let _temporary = try_temporary_exclusive_lock(&probe)
                .unwrap()
                .expect("retired owner lock must be available");
        }

        let competing_probe = open_existing_lock_file(&owner_lock_path).unwrap();
        assert!(
            try_lock_file_exclusive(&competing_probe).unwrap(),
            "temporary logical lock drop must unlock an inherited descriptor"
        );
        unlock_file(&competing_probe);
        drop(competing_probe);
        drop(inherited_probe);
        drop(probe);
    }

    #[cfg(unix)]
    #[test]
    fn protocol_lock_rejects_symlinks_hard_links_and_special_files() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = TestDir::new("protocol-file-types");
        let protocol_path = dir.path().join(LOCK_PROTOCOL_FILE);
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep me").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o640)).unwrap();

        symlink(&victim, &protocol_path).unwrap();
        assert!(LockProtocolGuard::acquire(dir.path()).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_file(&protocol_path).unwrap();

        fs::hard_link(&victim, &protocol_path).unwrap();
        assert!(LockProtocolGuard::acquire(dir.path()).is_err());
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_file(&protocol_path).unwrap();

        mkfifo(&protocol_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let fifo_mode = fs::symlink_metadata(&protocol_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert!(LockProtocolGuard::acquire(dir.path()).is_err());
        assert_eq!(
            fs::symlink_metadata(&protocol_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            fifo_mode,
            "rejected protocol files must not be chmodded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_symlink_is_never_followed_or_chmodded() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = TestDir::new("state-dir-symlink");
        let outside = root.path().join("outside");
        let linked_state_dir = root.path().join("state");
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&outside, &linked_state_dir).unwrap();

        assert!(SnapshotOwner::create_in(&linked_state_dir).is_err());
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn ambiguous_pid_snapshot_is_retained_and_oldest_legacy_state_still_restores() {
        let dir = TestDir::new("legacy-corrupt");
        let pid_path = state_file_path_in(dir.path(), 10);
        write_session(&pid_path, "pid-era");
        let legacy_path = dir.path().join(LEGACY_STATE_FILE);
        write_session(&legacy_path, "legacy");
        let current = token(99);

        let restored = claim_session_from(dir.path(), &current, &|_| TokenLockState::Available)
            .expect("restore valid legacy state");
        assert_eq!(workspace(&restored.state).tabs[0].title, "legacy");
        assert!(pid_path.exists(), "PID-era state must remain conservative");
        assert!(!legacy_path.exists(), "legacy state must be claimed once");
        assert!(
            restored.pending.path.exists(),
            "claim must await checkpoint"
        );
    }

    #[cfg(unix)]
    #[test]
    fn orphaned_claim_from_exited_loader_is_recoverable() {
        let dir = TestDir::new("orphan-claim");
        let original_owner = SnapshotOwner::create_in(dir.path()).unwrap();
        let crashed_claimer = SnapshotOwner::create_in(dir.path()).unwrap();
        let original_lock = lock_file_path_for_token(dir.path(), &original_owner.token);
        let claimer_lock = lock_file_path_for_token(dir.path(), &crashed_claimer.token);
        let claim_path = dir.path().join(format!(
            "{}{}{}",
            original_owner
                .state_path
                .file_name()
                .unwrap()
                .to_string_lossy(),
            CLAIM_MARKER,
            crashed_claimer.token
        ));
        write_session(&claim_path, "orphaned claim");
        drop(original_owner);
        drop(crashed_claimer);
        let loader = SnapshotOwner::create_in(dir.path()).unwrap();

        let restored = claim_session_from(dir.path(), &loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("recover orphaned claim");
        assert_eq!(workspace(&restored.state).tabs[0].title, "orphaned claim");
        let session = workspace(&restored.state).clone();
        loader.remember_pending_restore(restored.pending).unwrap();
        checkpoint_snapshot_for_owner(&loader, &session).unwrap();
        assert!(!claim_path.exists());
        assert!(!original_lock.exists());
        assert!(!claimer_lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn crash_between_claim_and_checkpoint_leaves_snapshot_recoverable() {
        let dir = TestDir::new("crash-before-checkpoint");
        let original = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&original.state_path, "survives-crash");
        drop(original);

        let first_loader = SnapshotOwner::create_in(dir.path()).unwrap();
        let first_claim = claim_session_from(dir.path(), &first_loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("first loader claims snapshot");
        let first_claim_path = first_claim.pending.path.clone();
        first_loader
            .remember_pending_restore(first_claim.pending)
            .unwrap();
        assert!(first_claim_path.exists());

        // Dropping the owner models an abrupt exit: no checkpoint or commit
        // runs, but releasing the claimer lock makes the durable claim eligible
        // for the next startup.
        drop(first_loader);
        let second_loader = SnapshotOwner::create_in(dir.path()).unwrap();
        let recovered = claim_session_from(dir.path(), &second_loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("next loader recovers uncommitted claim");
        assert_eq!(workspace(&recovered.state).tabs[0].title, "survives-crash");
        assert!(!first_claim_path.exists());
        assert!(recovered.pending.path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_checkpoint_keeps_claim_for_next_startup() {
        let dir = TestDir::new("failed-checkpoint");
        let original = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&original.state_path, "survives-failure");
        drop(original);

        let loader = SnapshotOwner::create_in(dir.path()).unwrap();
        let claimed = claim_session_from(dir.path(), &loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("claim snapshot");
        let claim_path = claimed.pending.path.clone();
        loader.remember_pending_restore(claimed.pending).unwrap();
        let invalid = SavedSession {
            active: 0,
            tabs: vec![workspace(&claimed.state).tabs[0].clone(); MAX_RESTORED_TABS + 1],
            ai_conversation: None,
        };
        assert!(checkpoint_snapshot_for_owner(&loader, &invalid).is_err());
        assert!(claim_path.exists(), "failed save must retain the claim");
        assert!(
            !loader.state_path.exists(),
            "rejected checkpoint must not publish a replacement"
        );

        drop(loader);
        let next_loader = SnapshotOwner::create_in(dir.path()).unwrap();
        let recovered = claim_session_from(dir.path(), &next_loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("next loader recovers claim after failed checkpoint");
        assert_eq!(
            workspace(&recovered.state).tabs[0].title,
            "survives-failure"
        );
    }

    #[test]
    fn newest_valid_candidate_is_selected() {
        let dir = TestDir::new("newest");
        let older = SessionCandidate {
            path: dir.path().join("tabs.1.state"),
            file_name: parse_state_file_name("tabs.1.state").unwrap(),
            modified: UNIX_EPOCH + Duration::from_secs(1),
            state: SnapshotState::Workspace(saved_session("older")),
            supersedes: None,
        };
        let newer = SessionCandidate {
            path: dir.path().join("tabs.2.state"),
            file_name: parse_state_file_name("tabs.2.state").unwrap(),
            modified: UNIX_EPOCH + Duration::from_secs(2),
            state: SnapshotState::Workspace(saved_session("newer")),
            supersedes: None,
        };
        let mut candidates = vec![older, newer];
        sort_candidates_newest_first(&mut candidates);
        assert_eq!(workspace(&candidates[0].state).tabs[0].title, "newer");
    }

    #[test]
    fn durable_envelope_beats_its_predecessor_even_when_predecessor_mtime_is_newer() {
        let dir = TestDir::new("supersedes-mtime");
        let writer = token(20);
        let predecessor_name = format!(
            "{}{}{}",
            state_file_path_for_token(dir.path(), &token(10))
                .file_name()
                .unwrap()
                .to_string_lossy(),
            CLAIM_MARKER,
            writer
        );
        let envelope = SessionEnvelope {
            format: SESSION_ENVELOPE_FORMAT.to_string(),
            version: SESSION_ENVELOPE_VERSION,
            payload: SessionEnvelopePayload::Workspace(saved_session("replacement")),
            supersedes: Some(predecessor_name.clone()),
        };
        atomic_write(
            &state_file_path_for_token(dir.path(), &writer),
            &serde_json::to_vec(&envelope).unwrap(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(10));
        write_session(&dir.path().join(predecessor_name), "predecessor");

        let candidates =
            scan_candidates(dir.path(), Some(&token(99)), &|_| TokenLockState::Available);
        assert_eq!(candidates.len(), 1);
        assert_eq!(workspace(&candidates[0].state).tabs[0].title, "replacement");
    }

    #[test]
    fn candidate_scan_bounds_payload_reads_before_deserialization() {
        let dir = TestDir::new("bounded-candidate-scan");
        for index in 0..(MAX_CANDIDATES_PER_SCAN + 20) {
            fs::write(
                state_file_path_for_token(dir.path(), &token(index as u64 + 100)),
                b"placeholder",
            )
            .unwrap();
        }
        let encoded = serde_json::to_string(&saved_session("bounded")).unwrap();
        let mut read_calls = 0usize;
        let mut payload_bytes = 0u64;
        let candidates = scan_candidates_with_reader(
            dir.path(),
            Some(&token(99)),
            &|_| TokenLockState::Available,
            &mut |_path, limit| {
                read_calls += 1;
                let mut payload = encoded.clone();
                payload.push_str(&" ".repeat(limit as usize - payload.len()));
                payload_bytes += payload.len() as u64;
                Ok(payload)
            },
        );

        assert_eq!(payload_bytes, MAX_CANDIDATE_BYTES_PER_SCAN);
        assert_eq!(
            read_calls as u64,
            MAX_CANDIDATE_BYTES_PER_SCAN / MAX_SNAPSHOT_BYTES
        );
        assert!(read_calls <= MAX_CANDIDATES_PER_SCAN);
        assert_eq!(candidates.len(), read_calls);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claim_rename_never_replaces_an_existing_target() {
        let dir = TestDir::new("claim-no-replace");
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"source").unwrap();
        fs::write(&target, b"target").unwrap();

        let error = rename_noreplace(&source, &target).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"source");
        assert_eq!(fs::read(&target).unwrap(), b"target");
    }

    #[cfg(unix)]
    #[test]
    fn retention_prunes_only_unlocked_snapshots_and_cleans_their_locks() {
        let dir = TestDir::new("retention");
        let mut exited = Vec::new();
        for index in 0..4 {
            let owner = SnapshotOwner::create_in(dir.path()).unwrap();
            write_session(&owner.state_path, &format!("tab-{index}"));
            let state_path = owner.state_path.clone();
            let lock_path = lock_file_path_for_token(dir.path(), &owner.token);
            exited.push((state_path, lock_path));
            drop(owner);
        }
        let live = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&live.state_path, "live");

        prune_recoverable_snapshots(
            dir.path(),
            Some(&live.token),
            &|token| token_lock_state_in(dir.path(), token),
            2,
        );

        let retained = exited
            .iter()
            .filter(|(state_path, _)| state_path.exists())
            .count();
        assert_eq!(retained, 2);
        for (state_path, lock_path) in exited {
            if !state_path.exists() {
                assert!(
                    !lock_path.exists(),
                    "a pruned snapshot must not leave an orphan owner lock"
                );
            }
        }
        assert!(
            live.state_path.exists(),
            "a held owner lock must prevent pruning"
        );
    }

    /// The snapshot path came from a directory scan, so its size is not this
    /// process's to trust. An oversized file is rejected by size rather than read
    /// into memory and handed to serde, and — like every other unusable candidate
    /// here — it is retained for inspection instead of being silently consumed.
    #[test]
    fn an_oversized_snapshot_is_skipped_and_retained() {
        let dir = TestDir::new("oversize");
        let good_path = dir.path().join(LEGACY_STATE_FILE);
        write_session(&good_path, "reasonable");

        // Written second, so it is the newest snapshot and therefore the one the
        // loader prefers: the only reason it is not restored is the size bound.
        let fat_path = state_file_path_for_token(dir.path(), &token(10));
        let mut fat = serde_json::to_string(&saved_session("too-big")).unwrap();
        fat.push_str(&" ".repeat(MAX_SNAPSHOT_BYTES as usize));
        fs::write(&fat_path, fat.as_bytes()).unwrap();

        let restored = claim_session_from(dir.path(), &token(99), &|_| TokenLockState::Available)
            .expect("the snapshot that fits still restores");
        assert_eq!(
            workspace(&restored.state).tabs[0].title,
            "reasonable",
            "the oversized snapshot must not have been read at all"
        );
        assert!(
            fat_path.exists(),
            "an oversized snapshot must be left on disk for inspection"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_tightens_state_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("private-directory");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        write_session(&state_file_path_in(dir.path(), 10), "private");
        let mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
