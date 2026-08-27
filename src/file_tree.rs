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
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

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
const MAX_CONCURRENT_SCANS: usize = 16;
static ACTIVE_SCANS: AtomicUsize = AtomicUsize::new(0);

struct ScanPermit;

impl ScanPermit {
    fn acquire() -> io::Result<Self> {
        ACTIVE_SCANS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_CONCURRENT_SCANS).then_some(active + 1)
            })
            .map(|_| Self)
            .map_err(|_| io::Error::other("file-tree scan concurrency limit reached"))
    }
}

impl Drop for ScanPermit {
    fn drop(&mut self) {
        ACTIVE_SCANS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
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

pub(crate) fn scan_dir(dir: &Path) -> io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)?
        .take(MAX_DIRECTORY_ENTRIES)
        .flatten()
    {
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
    }
    sort_entries(&mut entries);
    Ok(entries)
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
        location: location.clone(),
        remote_profile,
    }
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

/// Scan `dir` on a worker thread under the shared permit, then hand the
/// result to `apply` on the GTK thread via the glib poll. `loc` + `hosts`
/// snapshot the backend at request time; `remote_fs::list_dir` does the work.
pub(crate) fn request_dir_scan<F>(
    loc: crate::remote_fs::FsLocation,
    hosts: Vec<crate::config::RemoteHost>,
    dir: PathBuf,
    apply: F,
) -> io::Result<()>
where
    F: FnOnce(io::Result<Vec<FileEntry>>) + 'static,
{
    request_fs_op(
        move || crate::remote_fs::list_dir(&loc, &hosts, &dir),
        apply,
    )
}

/// One event from a streaming filesystem op: throttled byte progress, then
/// exactly one terminal result.
pub(crate) enum FsOpOutcome<T> {
    Progress(u64),
    Done(io::Result<T>),
}

/// Run one blocking op on a worker thread under the shared permit, streaming
/// throttled progress events and the terminal result to `apply` on the GTK
/// thread via the glib poll. The worker's progress callback is non-blocking:
/// a stalled UI must never back-pressure a transfer.
pub(crate) fn request_fs_op_streaming<T, O, F>(op: O, apply: F) -> io::Result<()>
where
    O: FnOnce(&dyn Fn(u64)) -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    F: FnMut(FsOpOutcome<T>) + 'static,
{
    let permit = ScanPermit::acquire()?;
    let (tx, rx) = mpsc::sync_channel::<FsOpOutcome<T>>(64);
    std::thread::Builder::new()
        .name("anvil-file-tree-op".to_string())
        .spawn(move || {
            let _permit = permit;
            let progress = |bytes: u64| {
                let _ = tx.try_send(FsOpOutcome::Progress(bytes));
            };
            let result = op(&progress);
            let _ = tx.send(FsOpOutcome::Done(result));
        })?;

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

/// Run one blocking filesystem op on a worker thread with the same permit /
/// glib-poll skeleton as directory scans, so mutations and listings share the
/// concurrency budget.
pub(crate) fn request_fs_op<T, O, F>(op: O, apply: F) -> io::Result<()>
where
    O: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
    F: FnOnce(io::Result<T>) + 'static,
{
    let permit = ScanPermit::acquire()?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("anvil-file-tree-scan".to_string())
        .spawn(move || {
            let _permit = permit;
            let _ = tx.send(op());
        })?;

    let mut apply = Some(apply);
    glib::timeout_add_local(SCAN_POLL_INTERVAL, move || match rx.try_recv() {
        Ok(result) => {
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
            if !state.is_active() {
                return true;
            }
            let identity: String = model
                .get_value(iter, COL_PATH as i32)
                .get()
                .unwrap_or_default();
            // Placeholders (empty identity) never count as matches.
            !identity.is_empty() && state.is_visible(&identity)
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

/// Pure merge computation behind [`merge_refresh_children`]: rows whose path
/// vanished are removed, new entries are inserted in sort order, survivors
/// keep their place (and with it their children and expansion). Returns None
/// when a placeholder child marks a never-expanded directory — its lazy scan
/// sees the fresh state on expansion, so the row stays untouched.
fn plan_merge_refresh<'a>(
    current: &[String],
    fresh: &'a [(String, FileEntry)],
) -> Option<MergeEdit<'a>> {
    if current.iter().any(String::is_empty) {
        return None;
    }
    let fresh_ids: std::collections::HashSet<&str> =
        fresh.iter().map(|(id, _)| id.as_str()).collect();
    let fresh_by_id: std::collections::HashMap<&str, &FileEntry> = fresh
        .iter()
        .map(|(id, entry)| (id.as_str(), entry))
        .collect();

    let mut removals = Vec::new();
    let mut survivors: Vec<&str> = Vec::new();
    for (index, identity) in current.iter().enumerate() {
        if fresh_ids.contains(identity.as_str()) {
            survivors.push(identity.as_str());
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
) {
    let fresh = identified(fresh);
    let mut current = Vec::new();
    let mut index = 0;
    while let Some(iter) = store.iter_nth_child(parent, index) {
        current.push(
            store
                .get_value(&iter, COL_PATH as i32)
                .get::<String>()
                .unwrap_or_default(),
        );
        index += 1;
    }
    let Some(edit) = plan_merge_refresh(&current, &fresh) else {
        return;
    };
    // Descending removal keeps the still-valid lower indexes intact.
    for index in edit.removals.iter().rev() {
        if let Some(iter) = store.iter_nth_child(parent, *index as i32) {
            store.remove(&iter);
        }
    }
    for (position, entry) in edit.inserts {
        insert_entry_row(store, parent, Some(position), entry);
    }
}

/// Lazily fill a directory row's real children on first expansion. `location`
/// decides the backend (local disk or one remote host); a location switch
/// mid-scan drops the stale result before it touches the store.
pub(crate) fn on_expand(
    store: &TreeStore,
    iter: &TreeIter,
    scan_generation: &Rc<Cell<u64>>,
    location: &Rc<RefCell<crate::remote_fs::FsLocation>>,
    hosts: Vec<crate::config::RemoteHost>,
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
    let Some(row_ref) = TreeRowReference::new(store, &store.path(iter)) else {
        return;
    };

    store.set(&first_child, &[(COL_IS_DIR, &true)]);
    let store_for_result = store.clone();
    let active_generation = scan_generation.clone();
    let generation = active_generation.get();
    let expected_identity = dir_identity.clone();
    let expected_display = display_full_path(&dir_path);
    let loc = location.borrow().clone();
    let active_location = location.clone();
    let expected_loc = loc.clone();
    if let Err(error) = request_dir_scan(loc, hosts, dir_path, move |result| {
        if active_generation.get() != generation || *active_location.borrow() != expected_loc {
            return;
        }
        let Some(row_path) = row_ref.path() else {
            return;
        };
        let Some(parent) = store_for_result.iter(&row_path) else {
            return;
        };
        let current_path: String = store_for_result
            .get_value(&parent, COL_PATH as i32)
            .get()
            .unwrap_or_default();
        if current_path != expected_identity {
            return;
        }
        let Some(placeholder) = store_for_result.iter_children(Some(&parent)) else {
            return;
        };
        let placeholder_path: String = store_for_result
            .get_value(&placeholder, COL_PATH as i32)
            .get()
            .unwrap_or_default();
        if !placeholder_path.is_empty() {
            return;
        }
        store_for_result.remove(&placeholder);
        match result {
            Ok(entries) => append_entries(&store_for_result, Some(&parent), entries),
            Err(error) => log::warn!("failed to scan directory {expected_display}: {error}"),
        }
    }) {
        store.set(&first_child, &[(COL_IS_DIR, &false)]);
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
}

impl TreeFilter {
    pub(crate) fn new() -> Self {
        Self {
            query: String::new(),
            visible: std::collections::HashSet::new(),
            saved_expansion: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.query.is_empty()
    }

    fn is_visible(&self, identity: &str) -> bool {
        !self.is_active() || self.visible.contains(identity)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

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

    // -- merge refresh (in-place directory update) ----------------------------

    fn entry(path: &str, is_dir: bool) -> FileEntry {
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        FileEntry::new(name, PathBuf::from(path), is_dir)
    }

    fn identity_of(path: &str) -> String {
        encode_path_identity(Path::new(path)).expect("short paths encode")
    }

    /// Simulate the model after applying an edit: stale rows removed, inserts
    /// at their planned positions, survivors untouched.
    fn apply_plan(current: &[String], edit: &MergeEdit) -> Vec<String> {
        let mut model: Vec<String> = current.to_vec();
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
        let current: Vec<String> = ["/r/aaa", "/r/bbb", "/r/file1", "/r/file2"]
            .into_iter()
            .map(identity_of)
            .collect();
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
        let current = vec![String::new()];
        let fresh = identified(vec![entry("/r/aaa/new", false)]);
        assert!(plan_merge_refresh(&current, &fresh).is_none());
    }

    #[test]
    fn merge_plan_handles_rename_shape_and_empty_results() {
        let current: Vec<String> = ["/r/alpha.txt", "/r/zeta.txt"]
            .into_iter()
            .map(identity_of)
            .collect();
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
}
