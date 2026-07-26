//! Session persistence for jterm1 windows.
//!
//! Each tab stores its title, whether it was user-renamed, and a `PaneLayout`
//! tree mirroring the live GTK `Paned` structure — so nested splits, each pane's
//! working directory, terminal mode and any restorable command (ssh / nix
//! develop / docker exec …) are restored.
//!
//! jterm1 is a `NON_UNIQUE` application: every launch is a separate process.
//! A single `tabs.state` therefore lets unrelated windows overwrite each other.
//! New snapshots use a random per-process token plus a companion owner lock:
//! `tabs.<token>.state` and `tabs.<token>.lock`. The process holds an exclusive
//! `flock` on that lock for its lifetime, so liveness remains correct across PID
//! reuse and PID namespaces. The old `tabs.state` and `tabs.<pid>.state` names
//! remain readable; PID-owned files are retained conservatively because their
//! owner identity cannot be proven from another namespace.

use gtk::glib;
use relm4::gtk;
use serde::{Deserialize, Deserializer, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const LEGACY_STATE_FILE: &str = "tabs.state";
const STATE_PREFIX: &str = "tabs.";
const STATE_SUFFIX: &str = ".state";
const LOCK_SUFFIX: &str = ".lock";
const LOCK_PROTOCOL_FILE: &str = ".session-lock-protocol";
const CLAIM_MARKER: &str = ".claim.";
const MAX_RECOVERABLE_SNAPSHOTS: usize = 32;
const OWNER_TOKEN_ATTEMPTS: usize = 128;
static SNAPSHOT_OWNER: OnceLock<Result<SnapshotOwner, String>> = OnceLock::new();

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
        /// True when `cwd` was reported from an ssh/mosh/container namespace
        /// and must not be reused as a local spawn or file-tree directory.
        #[serde(default)]
        cwd_external: bool,
        /// Name of a currently configured managed remote. Storing only the
        /// identifier avoids persisting SSH options/commands in a mutable
        /// session file; restore re-resolves the validated live configuration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_name: Option<String>,
        /// Stable local rsh identity learned through OSC 7770.
        #[serde(skip_serializing_if = "Option::is_none")]
        sid: Option<String>,
        /// Restorable command argv to replay on restore (e.g. `["ssh", "host"]`).
        /// Keeping it structured prevents shell metacharacters inside one
        /// argument from becoming a different local command after a restart.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_restorable_argv"
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

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredRestorableCommand {
    Argv(Vec<String>),
    LegacyString(String),
}

/// Older snapshots stored a shell command string assembled with `argv.join`.
/// Its original argument boundaries cannot be recovered safely, so accept the
/// old shape for session compatibility but deliberately do not auto-execute it.
fn deserialize_restorable_argv<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let stored = Option::<StoredRestorableCommand>::deserialize(deserializer)?;
    Ok(match stored {
        Some(StoredRestorableCommand::Argv(argv)) if !argv.is_empty() => Some(argv),
        Some(StoredRestorableCommand::LegacyString(_)) => {
            log::warn!("Ignoring legacy session restore command without argv boundaries");
            None
        }
        _ => None,
    })
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct SavedTab {
    pub title: String,
    pub custom_title: bool,
    /// Pinned tabs stay pinned across restarts. Older snapshots predate this
    /// field and therefore restore as unpinned.
    #[serde(default)]
    pub pinned: bool,
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

#[cfg(unix)]
fn lock_file_exclusive(file: &File) -> io::Result<()> {
    loop {
        // SAFETY: `file` owns this descriptor for the duration of the call;
        // flock stores no userspace pointer.
        let result = unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
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

#[cfg(not(unix))]
fn lock_file_exclusive(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "session lock protocol requires flock",
    ))
}

/// Serializes publication and removal of owner-lock pathnames. The file is
/// intentionally persistent and is never considered by orphan cleanup.
struct LockProtocolGuard {
    _file: File,
}

impl LockProtocolGuard {
    fn open(dir: &Path) -> io::Result<File> {
        let path = dir.join(LOCK_PROTOCOL_FILE);
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
        }
        options.open(path)
    }

    fn from_locked_file(file: File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self { _file: file })
    }

    fn acquire(dir: &Path) -> io::Result<Self> {
        let file = Self::open(dir)?;
        lock_file_exclusive(&file)?;
        Self::from_locked_file(file)
    }

    fn try_acquire(dir: &Path) -> io::Result<Option<Self>> {
        let file = Self::open(dir)?;
        if try_lock_file_exclusive(&file)? {
            Self::from_locked_file(file).map(Some)
        } else {
            Ok(None)
        }
    }
}

fn create_owner_lock(path: &Path, before_flock: &mut dyn FnMut(&Path)) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
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
    /// Kept open for the process lifetime. Do not unlink this path while the
    /// lock is held: another process could create a new inode under the same
    /// name and incorrectly appear to own the token.
    _lock_file: File,
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
            return Ok(Self {
                token,
                state_path,
                _lock_file: lock_file,
            });
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
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
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
    match try_lock_file_exclusive(&file) {
        Ok(true) => TokenLockState::Available,
        Ok(false) => TokenLockState::Held,
        Err(error) => {
            log::warn!(
                "Cannot probe session owner lock {}: {error}",
                path.display()
            );
            TokenLockState::Unknown
        }
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

pub(crate) fn save_session(session: &SavedSession) {
    let owner = match snapshot_owner() {
        Ok(owner) => owner,
        Err(err) => {
            log::error!("Cannot initialize session snapshot owner: {err}");
            return;
        }
    };
    let path = &owner.state_path;
    if session.tabs.is_empty() {
        // Do not leave an older non-empty snapshot behind after this window's
        // last tab is closed. A missing per-process file means "start fresh".
        if let Err(err) = fs::remove_file(path) {
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
    match atomic_write(path, payload.as_bytes()) {
        Ok(()) => {
            let directory = state_dir();
            prune_recoverable_snapshots(
                &directory,
                Some(&owner.token),
                &|token| token_lock_state_in(&directory, token),
                MAX_RECOVERABLE_SNAPSHOTS,
            );
        }
        Err(err) => {
            log::error!(
                "Failed to atomically save session snapshot {}: {err}",
                path.display()
            );
        }
    }
}

/// Load and consume the newest valid snapshot whose owning process has exited.
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
    let session = load_session_from(&directory, &owner.token, &lock_state);
    prune_recoverable_snapshots(
        &directory,
        Some(&owner.token),
        &lock_state,
        MAX_RECOVERABLE_SNAPSHOTS,
    );
    session
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Write to a sibling temp file, fsync it, then replace the destination with a
/// single rename. The previous valid snapshot is never removed first, so a
/// write/rename failure leaves it intact.
fn atomic_write(path: &Path, payload: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    ensure_private_directory(parent)?;

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
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
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
    match try_lock_file_exclusive(&file) {
        Ok(true) => match token_referenced_by_state_files(dir, token) {
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
        Ok(_) => {}
        Err(error) => {
            log::warn!(
                "Cannot lock retired session owner lock {}: {error}",
                path.display()
            );
        }
    }
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
    current_token: Option<&str>,
    lock_state: &dyn Fn(&str) -> TokenLockState,
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
        if !state_file_is_recoverable(&file_name, current_token, lock_state) {
            log::debug!(
                "Leaving live or identity-ambiguous session snapshot {} untouched",
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

/// Atomically move a recoverable snapshot to this instance's unique claim name
/// without replacing any existing file. jterm1 targets Linux; on another
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

fn load_session_from(
    dir: &Path,
    current_token: &str,
    lock_state: &dyn Fn(&str) -> TokenLockState,
) -> Option<SavedSession> {
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
        match rename_noreplace(&candidate.path, &claim_path) {
            Ok(()) => {
                if let Err(err) = fs::remove_file(&claim_path) {
                    log::error!(
                        "Claimed session snapshot {} as {}, but could not consume it: {err}; claimed file retained and no restore performed",
                        candidate.path.display(),
                        claim_path.display()
                    );
                    return None;
                }
                cleanup_locks_referenced_by(dir, &candidate.file_name);
                cleanup_orphaned_locks(dir);
                log::info!(
                    "Restored and consumed session snapshot {}",
                    candidate.path.display()
                );
                return Some(candidate.session);
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
                pinned: false,
                layout: PaneLayout::Leaf {
                    mode: "block".to_string(),
                    cwd: None,
                    cwd_external: false,
                    remote_name: None,
                    sid: None,
                    cmds: None,
                },
            }],
        }
    }

    #[test]
    fn pane_session_id_round_trips_and_old_snapshots_remain_compatible() {
        let with_sid = PaneLayout::Leaf {
            mode: "block".to_string(),
            cwd: Some("/tmp".to_string()),
            cwd_external: false,
            remote_name: None,
            sid: Some("rsh-session-42".to_string()),
            cmds: None,
        };
        let encoded = serde_json::to_string(&with_sid).unwrap();
        assert!(encoded.contains("rsh-session-42"));
        let decoded: PaneLayout = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            PaneLayout::Leaf {
                sid: Some(ref sid),
                ..
            } if sid == "rsh-session-42"
        ));

        let legacy: PaneLayout =
            serde_json::from_str(r#"{"type":"leaf","mode":"block","cwd":"/tmp","cmds":null}"#)
                .unwrap();
        assert!(matches!(legacy, PaneLayout::Leaf { sid: None, .. }));
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
        let decoded: PaneLayout = serde_json::from_str(&encoded).unwrap();
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
        let legacy: PaneLayout = serde_json::from_str(
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
    fn pinned_round_trips_and_legacy_snapshots_default_to_unpinned() {
        let mut current = saved_session("pinned");
        current.tabs[0].pinned = true;
        let encoded = serde_json::to_vec(&current).expect("serialize pinned session");
        let decoded: SavedSession =
            serde_json::from_slice(&encoded).expect("deserialize pinned session");
        assert!(decoded.tabs[0].pinned);

        let legacy = br#"{
            "active": 0,
            "tabs": [{
                "title": "legacy",
                "custom_title": false,
                "layout": {"type": "leaf", "mode": "block"}
            }]
        }"#;
        let decoded: SavedSession =
            serde_json::from_slice(legacy).expect("deserialize legacy session");
        assert!(!decoded.tabs[0].pinned);
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
    fn owner_flock_keeps_live_snapshot_while_exited_snapshot_is_consumed() {
        let dir = TestDir::new("live-owner");
        let live_owner = SnapshotOwner::create_in(dir.path()).unwrap();
        write_session(&live_owner.state_path, "live");
        let exited_owner = SnapshotOwner::create_in(dir.path()).unwrap();
        let exited_path = exited_owner.state_path.clone();
        let exited_lock = lock_file_path_for_token(dir.path(), &exited_owner.token);
        write_session(&exited_path, "exited");
        drop(exited_owner);
        let loader = SnapshotOwner::create_in(dir.path()).unwrap();

        let restored = load_session_from(dir.path(), &loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("restore exited owner");
        assert_eq!(restored.tabs[0].title, "exited");
        assert!(
            live_owner.state_path.exists(),
            "live owner's file must remain"
        );
        assert!(!exited_path.exists(), "restored file must be consumed");
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
        let first_saved: SavedSession =
            serde_json::from_str(&fs::read_to_string(&first.state_path).unwrap()).unwrap();
        let second_saved: SavedSession =
            serde_json::from_str(&fs::read_to_string(&second.state_path).unwrap()).unwrap();
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

    #[test]
    fn ambiguous_pid_snapshot_is_retained_and_oldest_legacy_state_still_restores() {
        let dir = TestDir::new("legacy-corrupt");
        let pid_path = state_file_path_in(dir.path(), 10);
        write_session(&pid_path, "pid-era");
        let legacy_path = dir.path().join(LEGACY_STATE_FILE);
        write_session(&legacy_path, "legacy");
        let current = token(99);

        let restored = load_session_from(dir.path(), &current, &|_| TokenLockState::Available)
            .expect("restore valid legacy state");
        assert_eq!(restored.tabs[0].title, "legacy");
        assert!(pid_path.exists(), "PID-era state must remain conservative");
        assert!(!legacy_path.exists(), "legacy state must be consumed once");
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

        let restored = load_session_from(dir.path(), &loader.token, &|token| {
            token_lock_state_in(dir.path(), token)
        })
        .expect("recover orphaned claim");
        assert_eq!(restored.tabs[0].title, "orphaned claim");
        assert!(!claim_path.exists());
        assert!(!original_lock.exists());
        assert!(!claimer_lock.exists());
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
