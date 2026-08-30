//! Local and remote filesystem access for the sidebar file tree.
//!
//! Remote hosts (`config::RemoteHost`) are browsed natively — no sshfs, no new
//! dependencies. Each operation spawns the system `ssh` or `docker` binary and
//! feeds it a small POSIX sh probe script on stdin (`sh -s -- <op> [args...]`),
//! mirroring the launcher's jsh-remote.sh-over-ssh philosophy. Everything here
//! blocks and is meant to run on worker threads behind the file tree's
//! thread + mpsc + glib-poll skeleton, never on the GTK thread.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::RemoteHost;
use crate::file_tree::FileEntry;

/// The POSIX sh probe every remote operation funnels through. It runs under
/// `sh -s -- <op> [args...]` with this script on stdin — except for the
/// payload ops (`put`, `untar`), which need stdin as a pure data channel and
/// therefore run as `sh -c '<script>' -- <op> [args...]` instead: a shell's
/// read-ahead on `sh -s` may legally swallow payload bytes into its own
/// buffer. Keep the exit-code contract in sync with `probe_result`.
pub(crate) const PROBE_SCRIPT: &str = r#"# remote-fs probe v3 — runs under `sh -s -- <op> [args...]`.
# `list` stdout: NUL-separated pairs "<t>\0<name>\0", t in {d,f,l}, names relative.
# `cat` streams a file to stdout; `put` stores stdin as a new file;
# `tar` streams a directory to stdout; `untar <dir> <name>` extracts stdin into
# <dir>, refusing an existing <dir>/<name> before anything is extracted;
# every creator treats a dangling symbolic link as an existing target;
# `stat` classifies links first and never opens a special leaf to obtain size;
# `stat` prints "<t> <size>" (bytes for regular files, otherwise 0).
# Exit codes: 0 ok, 2 usage/bad path, 3 cannot enter dir, 4 op failed, 17 target exists.
set -u
op=${1:-}
case "$op" in
  home)
    cd 2>/dev/null || cd / || exit 3
    pwd
    ;;
  list)
    d=${2:-}; limit=${3:-}
    case "$d" in /*) ;; *) exit 2 ;; esac
    case "$limit" in ''|*[!0-9]*) exit 2 ;; esac
    [ "$limit" -gt 0 ] 2>/dev/null || exit 2
    cd "$d" 2>/dev/null || exit 3
    count=0
    for f in * .[!.]* ..?*; do
      if [ -L "$f" ]; then t=l
      elif [ -d "$f" ]; then t=d
      elif [ -e "$f" ]; then t=f
      else continue
      fi
      printf '%s\0%s\0' "$t" "$f"
      count=$((count + 1))
      [ "$count" -ge "$limit" ] && break
    done
    ;;
  mkdir)
    p=${2:-}
    case "$p" in /*) ;; *) exit 2 ;; esac
    if [ -e "$p" ] || [ -L "$p" ]; then exit 17; fi
    mkdir "$p" || exit 4
    ;;
  mkfile)
    p=${2:-}
    case "$p" in /*) ;; *) exit 2 ;; esac
    if [ -e "$p" ] || [ -L "$p" ]; then exit 17; fi
    : > "$p" || exit 4
    ;;
  rm)
    p=${2:-}
    case "$p" in /*?*) ;; *) exit 2 ;; esac
    if [ -d "$p" ] && [ ! -L "$p" ]; then rm -rf "$p" || exit 4; else rm -f "$p" || exit 4; fi
    ;;
  mv)
    s=${2:-}; n=${3:-}
    case "$s" in /*) ;; *) exit 2 ;; esac
    case "$n" in /*) ;; *) exit 2 ;; esac
    if [ -e "$n" ] || [ -L "$n" ]; then exit 17; fi
    mv "$s" "$n" || exit 4
    ;;
  cp)
    s=${2:-}; n=${3:-}
    case "$s" in /*) ;; *) exit 2 ;; esac
    case "$n" in /*) ;; *) exit 2 ;; esac
    if [ -e "$n" ] || [ -L "$n" ]; then exit 17; fi
    cp -a "$s" "$n" || exit 4
    ;;
  cat)
    p=${2:-}
    case "$p" in /*) ;; *) exit 2 ;; esac
    { [ -f "$p" ] && [ -r "$p" ]; } || exit 3
    cat "$p" || exit 4
    ;;
  put)
    p=${2:-}
    case "$p" in /*) ;; *) exit 2 ;; esac
    if [ -e "$p" ] || [ -L "$p" ]; then exit 17; fi
    t="$p.fspart.$$"
    if cat > "$t"; then
      if [ -e "$p" ] || [ -L "$p" ]; then rm -f "$t"; exit 17; fi
      mv "$t" "$p" || { rm -f "$t"; exit 4; }
    else
      rm -f "$t"
      exit 4
    fi
    ;;
  tar)
    p=${2%/}
    case "$p" in /*) ;; *) exit 2 ;; esac
    [ -d "$p" ] || exit 3
    command -v tar >/dev/null 2>&1 || { echo "remote-fs probe: tar is not installed" >&2; exit 4; }
    d=${p%/*}
    d=${d:-/}
    tar cf - -C "$d" "${p##*/}" || exit 4
    ;;
  untar)
    d=${2:-}; n=${3:-}
    case "$d" in /*) ;; *) exit 2 ;; esac
    [ -d "$d" ] || exit 3
    case "$n" in ""|.|..|*/*) exit 2 ;; esac
    if [ -e "$d/$n" ] || [ -L "$d/$n" ]; then exit 17; fi
    command -v tar >/dev/null 2>&1 || { echo "remote-fs probe: tar is not installed" >&2; exit 4; }
    tar xf - -C "$d" || exit 4
    ;;
  stat)
    p=${2:-}
    case "$p" in /*) ;; *) exit 2 ;; esac
    if [ -L "$p" ]; then t=l; s=0
    elif [ -d "$p" ]; then t=d; s=0
    elif [ -f "$p" ]; then
      t=f
      s=$(wc -c < "$p") || exit 4
    elif [ -e "$p" ]; then t=f; s=0
    else
      exit 3
    fi
    printf '%s %s\n' "$t" "$s"
    ;;
  *) exit 2 ;;
esac
exit 0
"#;

/// Probe exit codes; see PROBE_SCRIPT's header comment.
const EXIT_USAGE: i32 = 2;
const EXIT_CANNOT_ENTER: i32 = 3;
const EXIT_EXISTS: i32 = 17;

/// Listing and `home` probes answer fast or not at all; mutations can walk a
/// large directory tree, so they get the longer budget.
const PROBE_LIST_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_OP_TIMEOUT: Duration = Duration::from_secs(60);
/// Ask the far side for one entry beyond the retained UI cap. Reaching that
/// extra record is the bounded, protocol-level `truncated` signal.
const LIST_PROBE_ENTRY_LIMIT: usize = crate::file_tree::MAX_DIRECTORY_ENTRIES + 1;
/// Transfers get a generous overall cap: ssh's own ConnectTimeout still
/// bounds the handshake, and this watchdog ends any transfer — busy or idle —
/// after 15 minutes, so a stuck connection can never wedge a worker thread.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// One directory holds at most MAX_DIRECTORY_ENTRIES shown entries of at most
/// 255 bytes; 2 MiB caps the capture without cutting a legitimate listing.
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
/// Payloads (files and directory tars) never exceed half a gigabyte.
pub(crate) const MAX_TRANSFER_BYTES: u64 = 512 * 1024 * 1024;
/// Local recursive copies refuse to descend deeper than this: a pathologically
/// deep tree errors out instead of exhausting the op worker's stack.
const MAX_COPY_DEPTH: usize = 128;
const MAX_TRANSFER_STDERR_BYTES: usize = 64 * 1024;
const STREAM_BUF_SIZE: usize = 64 * 1024;
const MAX_ERROR_DISPLAY_BYTES: usize = 512;
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A value-owned endpoint captured from a process-observed SSH login. The
/// identity is the validated base target used for matching and UI intent;
/// `execution` is a frozen launch snapshot which may additionally carry the
/// jsh-created ControlPath. Keeping those separate prevents an ephemeral
/// socket path from becoming filesystem identity while every async operation
/// still reuses the exact connection that was proved reachable.
#[derive(Clone, Debug)]
pub(crate) struct SessionRemoteEndpoint {
    identity: RemoteHost,
    execution: RemoteHost,
    /// Exact configured source, retained only for live config revocation and
    /// saved-profile terminal launches. It is deliberately excluded from
    /// equality because an explicit ControlPath is execution state, not stable
    /// filesystem identity.
    managed_profile: Option<RemoteHost>,
}

impl PartialEq for SessionRemoteEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.is_managed() == other.is_managed() && self.identity == other.identity
    }
}

impl SessionRemoteEndpoint {
    pub(crate) fn new(
        identity: RemoteHost,
        managed: bool,
        reusable_control_path: Option<&str>,
    ) -> Result<Self, &'static str> {
        let managed_profile = managed.then(|| identity.clone());
        let mut execution_overlay = Vec::new();
        if let Some(path) = reusable_control_path {
            execution_overlay.push("-S".to_string());
            execution_overlay.push(path.to_string());
        }
        Self::with_execution_overlay(identity, managed_profile, &execution_overlay)
    }

    pub(crate) fn with_execution_overlay(
        identity: RemoteHost,
        managed_profile: Option<RemoteHost>,
        execution_overlay: &[String],
    ) -> Result<Self, &'static str> {
        crate::config::validate_remote_host(&identity)?;
        if identity.docker {
            return Err("a process-observed SSH endpoint cannot be a container");
        }
        if let Some(profile) = &managed_profile {
            crate::config::validate_remote_host(profile)?;
        }
        let mut execution = identity.clone();
        execution.ssh_args.extend(execution_overlay.iter().cloned());
        // The overlay is never executed until the complete augmented profile
        // has passed the same structured argv gate as a saved profile.
        crate::config::validate_remote_host(&execution)?;
        Ok(Self {
            identity,
            execution,
            managed_profile,
        })
    }

    pub(crate) fn identity(&self) -> &RemoteHost {
        &self.identity
    }

    pub(crate) fn execution(&self) -> &RemoteHost {
        &self.execution
    }

    pub(crate) fn is_managed(&self) -> bool {
        self.managed_profile.is_some()
    }

    pub(crate) fn managed_profile(&self) -> Option<&RemoteHost> {
        self.managed_profile.as_ref()
    }

    pub(crate) fn has_execution_overlay(&self) -> bool {
        self.execution.ssh_args != self.identity.ssh_args
    }
}

/// Which filesystem the file tree browses. `Remote(i)` indexes
/// `config.remote_hosts`; `Transient` is an immutable process-observed SSH
/// endpoint kept only for this application session. It can remember that its
/// stable profile came from the saved list without borrowing a mutable index.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum FsLocation {
    Local,
    Remote(usize),
    Transient(Box<SessionRemoteEndpoint>),
}

impl FsLocation {
    pub(crate) fn session(endpoint: SessionRemoteEndpoint) -> Self {
        Self::Transient(Box::new(endpoint))
    }

    /// Selector label: "Local", or the host name prefixed by its scheme.
    pub(crate) fn label(&self, hosts: &[RemoteHost]) -> String {
        match self {
            FsLocation::Local => "Local".to_string(),
            FsLocation::Remote(index) => crate::config::checked_remote_host(hosts, *index)
                .map(location_label)
                .unwrap_or_else(|_| "Remote (unavailable)".to_string()),
            FsLocation::Transient(endpoint) => {
                crate::config::validate_remote_host(endpoint.identity())
                    .map(|()| {
                        let suffix = if endpoint.is_managed() {
                            ""
                        } else {
                            " (temporary)"
                        };
                        format!("{}{suffix}", location_label(endpoint.identity()))
                    })
                    .unwrap_or_else(|_| "Remote session (unavailable)".to_string())
            }
        }
    }

    pub(crate) fn is_remote(&self) -> bool {
        !matches!(self, Self::Local)
    }
}

/// Whether two location authorities name the same filesystem namespace.
/// Index-backed locations retain their exact-slot semantics, but a saved SSH
/// profile and a process-observed session endpoint are the same namespace when
/// their stable transport matches after removing ControlPath. This lets paste
/// use the live session endpoint directly instead of relaying through local
/// storage or later deleting through a stale/password-only saved connection.
pub(crate) fn locations_share_filesystem(
    left: &FsLocation,
    right: &FsLocation,
    hosts: &[RemoteHost],
) -> bool {
    match (left, right) {
        (FsLocation::Local, FsLocation::Local) => true,
        (FsLocation::Remote(left), FsLocation::Remote(right)) => left == right,
        (FsLocation::Transient(left), FsLocation::Transient(right)) => {
            crate::file_tree::remote_profiles_share_filesystem(left.identity(), right.identity())
        }
        (FsLocation::Remote(index), FsLocation::Transient(endpoint))
        | (FsLocation::Transient(endpoint), FsLocation::Remote(index)) => {
            crate::config::checked_remote_host(hosts, *index).is_ok_and(|managed| {
                crate::file_tree::remote_profiles_share_filesystem(managed, endpoint.identity())
            })
        }
        _ => false,
    }
}

// Keep the selector compact even for cloud-generated destinations. The split
// deliberately leaves enough room for the common `root@dsw` prefix and
// `aliyuncs.com` suffix; the complete endpoint remains available through the
// selector tooltip.
const LOCATION_LABEL_NAME_CHAR_LIMIT: usize = 21;

fn middle_ellipsize(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    if count <= limit || limit < 3 {
        return value.to_string();
    }
    let kept = limit - 1;
    // Cloud endpoints carry the provider/domain discriminator at the end, so
    // preserve a little more suffix than prefix (8 + ellipsis + 12 at the
    // selector's current limit).
    let left = (kept * 2) / 5;
    let right = kept - left;
    let prefix: String = value.chars().take(left).collect();
    let suffix: String = value
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

fn location_label(host: &RemoteHost) -> String {
    let scheme = if host.docker { "docker" } else { "ssh" };
    let name = jterm_core::review_input::safe_inline_display(&host.name, 256);
    let name = middle_ellipsize(&name, LOCATION_LABEL_NAME_CHAR_LIMIT);
    format!("{scheme}: {name}")
}

fn location_detail(host: &RemoteHost) -> String {
    let scheme = if host.docker { "docker" } else { "ssh" };
    let name = jterm_core::review_input::safe_inline_display(&host.name, 256);
    let endpoint = match &host.user {
        Some(user) => format!("{user}@{}", host.host),
        None => host.host.clone(),
    };
    let endpoint = jterm_core::review_input::safe_inline_display(&endpoint, 2048);
    let mut detail = if name == endpoint {
        format!("{scheme}: {endpoint}")
    } else {
        format!("{scheme}: {name} — {endpoint}")
    };
    if !host.docker && !host.ssh_args.is_empty() {
        let options = host
            .ssh_args
            .iter()
            .map(|arg| jterm_core::review_input::safe_inline_display(arg, 256))
            .collect::<Vec<_>>()
            .join(" ");
        let options = jterm_core::review_input::safe_inline_display(&options, 2048);
        detail.push_str(" · options: ");
        detail.push_str(&options);
    }
    detail
}

/// Dropdown labels for the header's location selector; index 0 is `Local`.
pub(crate) fn location_labels(hosts: &[RemoteHost]) -> Vec<String> {
    let mut labels = Vec::with_capacity(hosts.len().min(crate::config::MAX_REMOTE_HOSTS) + 1);
    labels.push("Local".to_string());
    labels.extend(
        hosts
            .iter()
            .take(crate::config::MAX_REMOTE_HOSTS)
            .enumerate()
            .map(|(index, host)| {
                if crate::config::checked_remote_host(hosts, index).is_ok() {
                    location_label(host)
                } else {
                    "Remote (unavailable)".to_string()
                }
            }),
    );
    labels
}

/// Full, safely rendered endpoint descriptions corresponding one-for-one to
/// [`location_labels`]. These are intentionally not middle-ellipsized: the
/// header exposes the selected entry as a tooltip so a compact label never
/// hides which machine will receive an operation.
pub(crate) fn location_details(hosts: &[RemoteHost]) -> Vec<String> {
    let mut details = Vec::with_capacity(hosts.len().min(crate::config::MAX_REMOTE_HOSTS) + 1);
    details.push("Local filesystem".to_string());
    details.extend(
        hosts
            .iter()
            .take(crate::config::MAX_REMOTE_HOSTS)
            .enumerate()
            .map(|(index, host)| {
                if crate::config::checked_remote_host(hosts, index).is_ok() {
                    location_detail(host)
                } else {
                    "Remote endpoint unavailable".to_string()
                }
            }),
    );
    details
}

/// Selector labels including the one non-persistent destination currently
/// being browsed. A transient entry disappears as soon as the user selects a
/// managed profile or Local; it is never written into `remote_hosts`.
pub(crate) fn location_labels_for(hosts: &[RemoteHost], location: &FsLocation) -> Vec<String> {
    let mut labels = location_labels(hosts);
    if let FsLocation::Transient(endpoint) = location {
        if !endpoint.is_managed() {
            labels.push(
                crate::config::validate_remote_host(endpoint.identity())
                    .map(|()| format!("{} (temporary)", location_label(endpoint.identity())))
                    .unwrap_or_else(|_| "Remote session (unavailable)".to_string()),
            );
        }
    }
    labels
}

/// Tooltip details matching [`location_labels_for`]. A temporary destination
/// keeps its complete process-observed endpoint here even though the visible
/// selector label is compact.
pub(crate) fn location_details_for(hosts: &[RemoteHost], location: &FsLocation) -> Vec<String> {
    let mut details = location_details(hosts);
    if let FsLocation::Transient(endpoint) = location {
        if !endpoint.is_managed() {
            details.push(
                crate::config::validate_remote_host(endpoint.identity())
                    .map(|()| format!("{} (temporary)", location_detail(endpoint.identity())))
                    .unwrap_or_else(|_| "Remote session endpoint unavailable".to_string()),
            );
        }
    }
    details
}

/// Keep a browsed remote filesystem bound to the exact profile that selected
/// it when the configured host list changes. An index is presentation state,
/// not identity: reordering may move the same profile, while reusing the slot
/// for a different profile must never redirect file operations or launches.
///
/// The match is deliberately over the complete [`RemoteHost`] value and must
/// be unique. A stale/invalid old slot, an edited or removed profile, or an
/// ambiguous duplicate all fail closed to Local.
pub(crate) fn remap_location_by_profile(
    location: &FsLocation,
    old_hosts: &[RemoteHost],
    new_hosts: &[RemoteHost],
) -> FsLocation {
    let old_index = match location {
        FsLocation::Local => return FsLocation::Local,
        FsLocation::Transient(endpoint) => {
            if crate::config::validate_remote_host(endpoint.identity()).is_err()
                || crate::config::validate_remote_host(endpoint.execution()).is_err()
            {
                return FsLocation::Local;
            }
            if !endpoint.is_managed() {
                return FsLocation::Transient(endpoint.clone());
            }
            let Some(managed_profile) = endpoint.managed_profile() else {
                return FsLocation::Local;
            };
            let mut matches = new_hosts
                .iter()
                .take(crate::config::MAX_REMOTE_HOSTS)
                .enumerate()
                .filter(|(index, host)| {
                    *host == managed_profile
                        && crate::config::checked_remote_host(new_hosts, *index).is_ok()
                });
            return if matches.next().is_some() && matches.next().is_none() {
                FsLocation::Transient(endpoint.clone())
            } else {
                FsLocation::Local
            };
        }
        FsLocation::Remote(old_index) => old_index,
    };
    let Ok(old_host) = crate::config::checked_remote_host(old_hosts, *old_index) else {
        return FsLocation::Local;
    };

    let mut matches = new_hosts
        .iter()
        .take(crate::config::MAX_REMOTE_HOSTS)
        .enumerate()
        .filter(|(index, host)| {
            *host == old_host && crate::config::checked_remote_host(new_hosts, *index).is_ok()
        })
        .map(|(index, _)| index);
    let Some(index) = matches.next() else {
        return FsLocation::Local;
    };
    if matches.next().is_some() {
        FsLocation::Local
    } else {
        FsLocation::Remote(index)
    }
}

/// One remembered Copy/Cut entry: a path and whether it is a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FsClipboardItem {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
}

/// The remembered Copy/Cut rows. Paste iterates `items`; cross-location
/// items stream, same-location items rename (cut) or copy.
#[derive(Clone, Debug)]
pub(crate) struct FsClipboard {
    pub(crate) loc: FsLocation,
    pub(crate) items: Vec<FsClipboardItem>,
    pub(crate) cut: bool,
    /// Identity of the user Copy/Cut action, not of its payload. Re-copying
    /// identical rows is a new intent and must survive older async completions.
    pub(crate) token: u64,
}

/// Resolve a delayed Paste through the live clipboard slot. Payload equality
/// is deliberately irrelevant: repeating Copy/Cut for the same rows is a new
/// user intent, while an exact profile reorder updates `loc` on the original
/// intent and must be observed by the eventual Paste.
pub(crate) fn clipboard_for_token(
    clipboard: &Option<FsClipboard>,
    expected_token: u64,
) -> Option<FsClipboard> {
    clipboard
        .as_ref()
        .filter(|clipboard| clipboard.token == expected_token)
        .cloned()
}

/// Capture a clipboard token only when the filesystem operation starts in the
/// clipboard's source authority. Comparing numeric locations later would reject
/// a safe profile reorder; capturing the token now keeps that same intent
/// recognizable without letting an unrelated Local/remote operation consume it.
pub(crate) fn clipboard_token_for_location(
    clipboard: &Option<FsClipboard>,
    operation_location: &FsLocation,
    hosts: &[RemoteHost],
) -> Option<u64> {
    clipboard
        .as_ref()
        .filter(|clipboard| locations_share_filesystem(&clipboard.loc, operation_location, hosts))
        .map(|clipboard| clipboard.token)
}

/// Remove sources made stale by a successful rename/delete from the exact
/// captured clipboard intent. Unrelated items remain available after a partial
/// batch; removing a directory also consumes clipboard descendants beneath it.
pub(crate) fn retire_clipboard_sources(
    clipboard: &mut Option<FsClipboard>,
    expected_token: Option<u64>,
    affected_paths: &[PathBuf],
) -> bool {
    let Some(expected_token) = expected_token else {
        return false;
    };
    let Some(current) = clipboard.as_mut() else {
        return false;
    };
    if current.token != expected_token {
        return false;
    }
    let previous_len = current.items.len();
    current.items.retain(|item| {
        !affected_paths
            .iter()
            .any(|affected| item.path == *affected || item.path.starts_with(affected))
    });
    let changed = current.items.len() != previous_len;
    if changed && current.items.is_empty() {
        *clipboard = None;
    }
    changed
}

/// Rebind an index-backed clipboard source through the same exact, unique,
/// validated profile rule as the visible tree. A safe reorder preserves the
/// clipboard token; any missing/edited/ambiguous identity clears it rather
/// than redirecting old paths to the profile that reused its index.
pub(crate) fn remap_clipboard_by_profile(
    clipboard: &mut Option<FsClipboard>,
    old_hosts: &[RemoteHost],
    new_hosts: &[RemoteHost],
) -> bool {
    let Some(current) = clipboard.as_mut() else {
        return false;
    };
    if matches!(current.loc, FsLocation::Local) {
        return false;
    }
    let remapped = remap_location_by_profile(&current.loc, old_hosts, new_hosts);
    match remapped {
        FsLocation::Remote(index) => {
            current.loc = FsLocation::Remote(index);
            false
        }
        FsLocation::Transient(endpoint) => {
            current.loc = FsLocation::Transient(endpoint);
            false
        }
        FsLocation::Local => {
            *clipboard = None;
            true
        }
    }
}

/// One finished probe run, output bounded on both streams.
#[derive(Debug)]
struct Capture {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// List one directory, locally via `std::fs` or remotely via the probe.
/// Entries are sorted directories-first, case-insensitive, and capped exactly
/// like `file_tree::scan_dir`.
pub(crate) fn list_dir(
    loc: &FsLocation,
    hosts: &[RemoteHost],
    dir: &Path,
) -> io::Result<crate::file_tree::DirectoryListing> {
    list_dir_inner(loc, hosts, dir, None)
}

pub(crate) fn list_dir_with_cancellation(
    loc: &FsLocation,
    hosts: &[RemoteHost],
    dir: &Path,
    cancellation: &crate::file_tree::ScanCancellation,
) -> io::Result<crate::file_tree::DirectoryListing> {
    list_dir_inner(loc, hosts, dir, Some(cancellation))
}

fn list_dir_inner(
    loc: &FsLocation,
    hosts: &[RemoteHost],
    dir: &Path,
    cancellation: Option<&crate::file_tree::ScanCancellation>,
) -> io::Result<crate::file_tree::DirectoryListing> {
    if cancellation.is_some_and(crate::file_tree::ScanCancellation::is_cancelled) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "directory scan was superseded",
        ));
    }
    match loc {
        FsLocation::Local => {
            let listing = crate::file_tree::scan_dir(dir)?;
            if cancellation.is_some_and(crate::file_tree::ScanCancellation::is_cancelled) {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "directory scan was superseded",
                ))
            } else {
                Ok(listing)
            }
        }
        FsLocation::Remote(_) | FsLocation::Transient(_) => {
            require_absolute(dir)?;
            let host = remote_host(loc, hosts)?;
            let limit = LIST_PROBE_ENTRY_LIMIT.to_string();
            let args = [dir.as_os_str(), OsStr::new(&limit)];
            let stdout = match cancellation {
                Some(cancellation) => run_probe_with_cancellation(
                    host,
                    "list",
                    &args,
                    PROBE_LIST_TIMEOUT,
                    cancellation,
                )?,
                None => run_probe(host, "list", &args, PROBE_LIST_TIMEOUT)?,
            };
            Ok(parse_list_output(&stdout, dir))
        }
    }
}

/// Where a fresh tree starts: `$HOME` locally (falling back to `/`), the
/// remote account's home directory over the probe otherwise.
pub(crate) fn start_dir(loc: &FsLocation, hosts: &[RemoteHost]) -> io::Result<PathBuf> {
    match loc {
        FsLocation::Local => Ok(crate::file_tree::home_dir().unwrap_or_else(|| PathBuf::from("/"))),
        FsLocation::Remote(_) | FsLocation::Transient(_) => {
            let host = remote_host(loc, hosts)?;
            let stdout = run_probe(host, "home", &[], PROBE_LIST_TIMEOUT)?;
            parse_home_output(&stdout)
        }
    }
}

/// Create one directory; fails with `AlreadyExists` when `path` is taken.
pub(crate) fn create_dir(loc: &FsLocation, hosts: &[RemoteHost], path: &Path) -> io::Result<()> {
    match loc {
        FsLocation::Local => std::fs::create_dir(path),
        FsLocation::Remote(_) | FsLocation::Transient(_) => {
            require_absolute(path)?;
            run_probe(
                remote_host(loc, hosts)?,
                "mkdir",
                &[path.as_os_str()],
                PROBE_OP_TIMEOUT,
            )?;
            Ok(())
        }
    }
}

/// Create one empty file; fails with `AlreadyExists` when `path` is taken.
pub(crate) fn create_file(loc: &FsLocation, hosts: &[RemoteHost], path: &Path) -> io::Result<()> {
    match loc {
        FsLocation::Local => std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop),
        FsLocation::Remote(_) | FsLocation::Transient(_) => {
            require_absolute(path)?;
            run_probe(
                remote_host(loc, hosts)?,
                "mkfile",
                &[path.as_os_str()],
                PROBE_OP_TIMEOUT,
            )?;
            Ok(())
        }
    }
}

/// Delete a file, symlink, or directory tree. `/` is refused Rust-side, and
/// the probe's `/*?*` pattern is the second line of defence.
pub(crate) fn delete(loc: &FsLocation, hosts: &[RemoteHost], path: &Path) -> io::Result<()> {
    validate_delete_target(path)?;
    match loc {
        FsLocation::Local => {
            // symlink_metadata does not follow: a symlink to a directory is
            // removed as a link, matching the probe's `[ -d ] && [ ! -L ]`.
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            }
        }
        FsLocation::Remote(_) | FsLocation::Transient(_) => {
            require_absolute(path)?;
            run_probe(
                remote_host(loc, hosts)?,
                "rm",
                &[path.as_os_str()],
                PROBE_OP_TIMEOUT,
            )?;
            Ok(())
        }
    }
}

/// Rename `src` to `dst`; fails with `AlreadyExists` when `dst` is taken.
pub(crate) fn rename(
    loc: &FsLocation,
    hosts: &[RemoteHost],
    src: &Path,
    dst: &Path,
) -> io::Result<()> {
    validate_not_into_self(src, dst)?;
    match loc {
        FsLocation::Local => rename_noreplace(src, dst),
        FsLocation::Remote(_) | FsLocation::Transient(_) => {
            require_absolute(src)?;
            require_absolute(dst)?;
            run_probe(
                remote_host(loc, hosts)?,
                "mv",
                &[src.as_os_str(), dst.as_os_str()],
                PROBE_OP_TIMEOUT,
            )?;
            Ok(())
        }
    }
}

/// Copy `src` to `dst`, directories recursively; fails with `AlreadyExists`
/// when `dst` is taken.
pub(crate) fn copy(
    loc: &FsLocation,
    hosts: &[RemoteHost],
    src: &Path,
    dst: &Path,
) -> io::Result<()> {
    validate_not_into_self(src, dst)?;
    match loc {
        FsLocation::Local => {
            require_missing(dst)?;
            copy_recursive(src, dst, 0)
        }
        FsLocation::Remote(_) | FsLocation::Transient(_) => {
            require_absolute(src)?;
            require_absolute(dst)?;
            run_probe(
                remote_host(loc, hosts)?,
                "cp",
                &[src.as_os_str(), dst.as_os_str()],
                PROBE_OP_TIMEOUT,
            )?;
            Ok(())
        }
    }
}

/// Where a paste lands: the clipboard entry's file name inside `dir`.
pub(crate) fn paste_destination(dir: &Path, clip_path: &Path) -> PathBuf {
    match clip_path.file_name() {
        Some(name) => dir.join(name),
        // "/" has no file name; joining it wholesale yields "/" again, which
        // the caller's src == dst check turns into a clear error.
        None => dir.join(clip_path.as_os_str()),
    }
}

/// Validate a New File / New Folder / Rename dialog name. UI-free so the
/// dialogs and the ops share one rule set; mirrors the remote filesystem's
/// own limits (NAME_MAX 255, no `/`, no NUL).
pub(crate) fn new_name_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("the name is empty");
    }
    if name.len() > 255 {
        return Some("the name is longer than 255 bytes");
    }
    if name == "." || name == ".." {
        return Some("\".\" and \"..\" are not valid names");
    }
    if name.contains('/') {
        return Some("the name must not contain '/'");
    }
    if name.contains('\0') {
        return Some("the name must not contain NUL");
    }
    None
}

fn remote_host<'a>(loc: &'a FsLocation, hosts: &'a [RemoteHost]) -> io::Result<&'a RemoteHost> {
    match loc {
        FsLocation::Local => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local location has no remote host",
        )),
        FsLocation::Remote(index) => {
            if *index >= crate::config::MAX_REMOTE_HOSTS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "remote host index exceeds the supported 128-profile limit",
                ));
            }
            if hosts.get(*index).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "that remote host is no longer configured",
                ));
            }
            crate::config::checked_remote_host(hosts, *index)
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))
        }
        FsLocation::Transient(endpoint) => {
            crate::config::validate_remote_host(endpoint.execution())
                .map(|()| endpoint.execution())
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))
        }
    }
}

/// Remote probe ops only accept absolute paths; check Rust-side so a
/// malformed tree row fails with a clear error instead of probe exit 2.
fn require_absolute(path: &Path) -> io::Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "remote file operations require an absolute path",
        ))
    }
}

fn validate_delete_target(path: &Path) -> io::Result<()> {
    if path.parent().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to delete the filesystem root",
        ));
    }
    Ok(())
}

/// `mv /a /a/b` and `cp -a /a /a/b` can never succeed; fail before spawning
/// anything for what the Rust side already knows is nonsense. This textual
/// comparison cannot see through symlink aliases; the local copier repeats
/// the check on canonicalized paths (see `copy_recursive`).
fn validate_not_into_self(src: &Path, dst: &Path) -> io::Result<()> {
    if dst == src || dst.starts_with(src) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot move or copy an entry into itself",
        ));
    }
    Ok(())
}

fn require_missing(path: &Path) -> io::Result<()> {
    // symlink_metadata: a dangling symlink still counts as taken.
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists",
                crate::file_tree::display_full_path(path)
            ),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn atomic_rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let src = std::ffi::CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let dst = std::ffi::CString::new(dst.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both C strings remain live for this single namespace syscall;
    // RENAME_NOREPLACE makes a concurrently-created destination an error.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
    ) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("atomic no-replace rename is unavailable: {error}"),
        ));
    }
    Err(error)
}

#[cfg(not(target_os = "linux"))]
fn atomic_rename_noreplace(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename requires Linux renameat2",
    ))
}

fn rename_noreplace_with(
    src: &Path,
    dst: &Path,
    commit: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    // Preserve the existing path-aware early diagnostic; the namespace
    // operation remains authoritative when another process wins afterward.
    require_missing(dst)?;
    commit(src, dst)
}

fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    rename_noreplace_with(src, dst, atomic_rename_noreplace)
}

/// Recursive copy mirroring `cp -a` semantics closely enough for the tree:
/// directory structure and file contents are preserved, symlinks are copied
/// as links rather than followed. The recursion is depth-bounded, and the
/// top level refuses to copy a directory into itself.
fn copy_recursive(src: &Path, dst: &Path, depth: usize) -> io::Result<()> {
    if depth >= MAX_COPY_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is nested too deeply to copy",
                crate::file_tree::display_full_path(src)
            ),
        ));
    }
    let metadata = std::fs::symlink_metadata(src)?;
    if metadata.is_dir() {
        if depth == 0 {
            // `validate_not_into_self` compares textually, which a symlink
            // alias defeats (`/link/a` vs `/a`); compare resolved paths once
            // at the root so `cp -a /a /a/b` fails through symlinks too.
            let canonical = src.canonicalize()?;
            let dst_parent = dst
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/"))
                .canonicalize()
                .unwrap_or_else(|_| dst.parent().map(Path::to_path_buf).unwrap_or_default());
            if dst_parent
                .join(dst.file_name().unwrap_or_default())
                .starts_with(&canonical)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot move or copy an entry into itself",
                ));
            }
        }
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            copy_recursive(&src.join(&name), &dst.join(&name), depth + 1)?;
        }
        Ok(())
    } else if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)
    } else {
        std::fs::copy(src, dst).map(drop)
    }
}

/// Parse `list` output: NUL-separated `<t>` `<name>` pairs, t in {d,f,l},
/// names relative to `dir`. Remote names must be valid UTF-8 before the same
/// text is used for both display and the actionable path. Duplicate records of
/// one type collapse to one row; conflicting types for one name suppress that
/// name entirely. Symlinks are files here and are never expandable.
fn parse_list_output(bytes: &[u8], dir: &Path) -> crate::file_tree::DirectoryListing {
    let mut entries: Vec<Option<FileEntry>> = Vec::new();
    let mut seen: std::collections::HashMap<String, (usize, bool)> =
        std::collections::HashMap::new();
    let mut collisions = std::collections::HashSet::new();
    let mut valid_records = 0usize;
    let mut fields = bytes.split(|&byte| byte == 0);
    while let (Some(kind), Some(name)) = (fields.next(), fields.next()) {
        // The probe's glob cannot produce these, but a hostile or buggy far
        // side must not smuggle a path outside `dir` into the tree.
        if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
            continue;
        }
        let is_dir = match kind {
            b"d" => true,
            b"f" | b"l" => false,
            _ => continue,
        };
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        valid_records = valid_records.saturating_add(1);
        if valid_records > LIST_PROBE_ENTRY_LIMIT {
            break;
        }
        if collisions.contains(name) {
            continue;
        }
        if let Some((index, previous_is_dir)) = seen.get(name).copied() {
            if previous_is_dir != is_dir {
                entries[index] = None;
                collisions.insert(name.to_string());
            }
            continue;
        }
        let name_os = OsStr::new(name);
        let index = entries.len();
        seen.insert(name.to_string(), (index, is_dir));
        entries.push(Some(FileEntry::new(
            crate::file_tree::display_os_str(name_os),
            dir.join(name_os),
            is_dir,
        )));
    }
    let truncated = valid_records > crate::file_tree::MAX_DIRECTORY_ENTRIES;
    let mut entries: Vec<FileEntry> = entries
        .into_iter()
        .flatten()
        .take(crate::file_tree::MAX_DIRECTORY_ENTRIES)
        .collect();
    crate::file_tree::sort_entries(&mut entries);
    crate::file_tree::DirectoryListing::new(entries, truncated)
}

fn parse_home_output(bytes: &[u8]) -> io::Result<PathBuf> {
    let line = bytes
        .split(|&byte| byte == b'\n')
        .next()
        .unwrap_or_default();
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote home probe returned nothing",
        ));
    }
    let path = PathBuf::from(OsString::from_vec(line.to_vec()));
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote home probe returned a relative path",
        ));
    }
    Ok(path)
}

fn run_probe(
    host: &RemoteHost,
    op: &str,
    args: &[&OsStr],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let argv = checked_probe_argv(host, op, args, ScriptDelivery::Stdin)?;
    probe_result(
        op,
        run_capture(&argv, PROBE_SCRIPT.as_bytes(), timeout, MAX_CAPTURE_BYTES)?,
    )
}

fn run_probe_with_cancellation(
    host: &RemoteHost,
    op: &str,
    args: &[&OsStr],
    timeout: Duration,
    cancellation: &crate::file_tree::ScanCancellation,
) -> io::Result<Vec<u8>> {
    let argv = checked_probe_argv(host, op, args, ScriptDelivery::Stdin)?;
    probe_result(
        op,
        run_capture_with_cancellation(
            &argv,
            PROBE_SCRIPT.as_bytes(),
            timeout,
            MAX_CAPTURE_BYTES,
            cancellation,
        )?,
    )
}

/// Map the probe's exit-code contract onto io error kinds; stderr text rides
/// along, bounded and made safe to display.
fn probe_result(op: &str, capture: Capture) -> io::Result<Vec<u8>> {
    match capture.code {
        Some(0) => Ok(capture.stdout),
        Some(EXIT_EXISTS) => Err(probe_error(
            io::ErrorKind::AlreadyExists,
            op,
            &capture,
            "target already exists",
        )),
        Some(EXIT_CANNOT_ENTER) => Err(probe_error(
            io::ErrorKind::NotFound,
            op,
            &capture,
            "directory does not exist",
        )),
        Some(EXIT_USAGE) => Err(probe_error(
            io::ErrorKind::InvalidInput,
            op,
            &capture,
            "the probe rejected the request",
        )),
        // The probe script never emits 255. OpenSSH reserves it for transport
        // or authentication failure, which should be classified without
        // exposing ssh's potentially sensitive stderr to the Files UI.
        Some(255) => Err(probe_error(
            io::ErrorKind::ConnectionAborted,
            op,
            &capture,
            "remote connection unavailable",
        )),
        _ => Err(probe_error(
            io::ErrorKind::Other,
            op,
            &capture,
            "operation failed",
        )),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FsFailureKind {
    Superseded,
    QueueFull,
    TimedOut,
    Missing,
    Permission,
    Exists,
    Connection,
    InvalidResponse,
    InvalidRequest,
    Other,
}

pub(crate) fn classify_fs_error(error: &io::Error) -> FsFailureKind {
    match error.kind() {
        io::ErrorKind::Interrupted => FsFailureKind::Superseded,
        io::ErrorKind::WouldBlock => FsFailureKind::QueueFull,
        io::ErrorKind::TimedOut => FsFailureKind::TimedOut,
        io::ErrorKind::NotFound => FsFailureKind::Missing,
        io::ErrorKind::PermissionDenied => FsFailureKind::Permission,
        io::ErrorKind::AlreadyExists => FsFailureKind::Exists,
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::NotConnected
        | io::ErrorKind::BrokenPipe => FsFailureKind::Connection,
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => FsFailureKind::InvalidResponse,
        io::ErrorKind::InvalidInput => FsFailureKind::InvalidRequest,
        _ => FsFailureKind::Other,
    }
}

/// Stable allow-listed copy for GTK. Detailed bounded stderr remains in logs,
/// but credentials, socket paths, shell fragments, and hostile Unicode from a
/// remote process never cross into a user-visible label or toast.
pub(crate) fn user_facing_fs_error(error: &io::Error) -> &'static str {
    user_facing_failure_kind(classify_fs_error(error))
}

pub(crate) fn user_facing_failure_kind(kind: FsFailureKind) -> &'static str {
    match kind {
        FsFailureKind::Superseded => "This request was superseded.",
        FsFailureKind::QueueFull => "Too many file operations are waiting; retry shortly.",
        FsFailureKind::TimedOut => "The filesystem did not respond in time.",
        FsFailureKind::Missing => "The path no longer exists or is unavailable.",
        FsFailureKind::Permission => "Permission was denied.",
        FsFailureKind::Exists => "The destination already exists.",
        FsFailureKind::Connection => "The remote connection is unavailable.",
        FsFailureKind::InvalidResponse => "The remote filesystem returned an invalid response.",
        FsFailureKind::InvalidRequest => "The filesystem request was rejected as invalid.",
        FsFailureKind::Other => "The filesystem operation failed.",
    }
}

fn probe_error(kind: io::ErrorKind, op: &str, capture: &Capture, fallback: &str) -> io::Error {
    let stderr = bounded_stderr_text(&capture.stderr);
    let message = if stderr.is_empty() {
        format!("remote {op}: {fallback}")
    } else {
        format!("remote {op}: {stderr}")
    };
    io::Error::new(kind, message)
}

/// Captured stderr as one trimmed, display-safe, bounded line for errors.
fn bounded_stderr_text(stderr: &[u8]) -> String {
    let text = crate::file_tree::display_os_str(OsStr::from_bytes(stderr));
    let text = text.trim();
    jterm_core::review_input::safe_inline_display(text, MAX_ERROR_DISPLAY_BYTES)
}

/// How the probe script reaches the far side's sh.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScriptDelivery {
    /// `sh -s -- <op> [args]`, script on stdin: everything without a payload.
    Stdin,
    /// `sh -c '<script>' -- <op> [args]`: stdin stays a pure payload channel
    /// (`put`, `untar`), immune to the shell's script read-ahead.
    Argv,
}

/// Build the local argv that runs the probe on the far side. The script
/// travels per `mode`; argv only carries `sh … -- <op> [args...]`.
fn probe_argv(host: &RemoteHost, op: &str, args: &[&OsStr], mode: ScriptDelivery) -> Vec<OsString> {
    if host.docker {
        docker_probe_argv(host, op, args, mode)
    } else {
        ssh_probe_argv(host, op, args, mode)
    }
}

fn checked_probe_argv(
    host: &RemoteHost,
    op: &str,
    args: &[&OsStr],
    mode: ScriptDelivery,
) -> io::Result<Vec<OsString>> {
    crate::config::validate_remote_host(host)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    Ok(probe_argv(host, op, args, mode))
}

/// The one remote command element for `sh -s`: `sh -s -- <op> [args]` with
/// every value single-quote-escaped.
fn sh_s_command(op: &str, args: &[&OsStr]) -> Vec<u8> {
    let mut command = b"sh -s -- ".to_vec();
    command.extend_from_slice(&sq_bytes(op.as_bytes()));
    for arg in args {
        command.push(b' ');
        command.extend_from_slice(&sq_bytes(arg.as_bytes()));
    }
    command
}

/// The one remote command element for `sh -c`: `sh -c '<script>' -- <op>
/// [args]`, so the script itself becomes `$0`'s neighbour (`--`) and the op
/// and args land in `$1`, `$2`, … exactly like the `-s` form.
fn sh_c_command(op: &str, args: &[&OsStr]) -> Vec<u8> {
    let mut command = b"sh -c ".to_vec();
    command.extend_from_slice(&sq_bytes(PROBE_SCRIPT.as_bytes()));
    command.extend_from_slice(b" -- ");
    command.extend_from_slice(&sq_bytes(op.as_bytes()));
    for arg in args {
        command.push(b' ');
        command.extend_from_slice(&sq_bytes(arg.as_bytes()));
    }
    command
}

/// ssh re-parses the command string with the far side's login shell, so the
/// whole probe invocation stays ONE argv element with every value
/// single-quote-escaped. Never interpolate an unquoted path here.
fn ssh_probe_argv(
    host: &RemoteHost,
    op: &str,
    args: &[&OsStr],
    mode: ScriptDelivery,
) -> Vec<OsString> {
    let dest = match &host.user {
        Some(user) => format!("{user}@{}", host.host),
        None => host.host.clone(),
    };
    let command = match mode {
        ScriptDelivery::Stdin => sh_s_command(op, args),
        ScriptDelivery::Argv => sh_c_command(op, args),
    };
    let mut argv: Vec<OsString> = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
        .into_iter()
        .map(OsString::from)
        .collect();
    // ssh_args were validated at config load (no control characters) and go
    // before the target, exactly like the interactive launcher.
    argv.extend(host.ssh_args.iter().map(OsString::from));
    // End option parsing before the user-owned destination.
    argv.push(OsString::from("--"));
    argv.push(OsString::from(dest));
    argv.push(OsString::from_vec(command));
    argv
}

/// `docker exec` passes argv raw — no shell joining anywhere, so no quoting.
/// `-i` keeps stdin open for the script or payload; `-t` would corrupt the
/// byte stream.
fn docker_probe_argv(
    host: &RemoteHost,
    op: &str,
    args: &[&OsStr],
    mode: ScriptDelivery,
) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("docker"),
        OsString::from("exec"),
        OsString::from("-i"),
    ];
    if let Some(user) = &host.user {
        argv.push(OsString::from("-u"));
        argv.push(OsString::from(user));
    }
    argv.push(OsString::from(&host.host));
    argv.push(OsString::from("sh"));
    match mode {
        ScriptDelivery::Stdin => argv.push(OsString::from("-s")),
        ScriptDelivery::Argv => {
            argv.push(OsString::from("-c"));
            argv.push(OsString::from(PROBE_SCRIPT));
        }
    }
    argv.push(OsString::from("--"));
    argv.push(OsString::from(op));
    argv.extend(args.iter().map(OsString::from));
    argv
}

/// POSIX single-quote escaping: `'` becomes `'\''`.
fn sq(s: &str) -> String {
    String::from_utf8(sq_bytes(s.as_bytes())).expect("single-quoting preserves UTF-8")
}

fn sq_bytes(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'\'');
    for &byte in s {
        if byte == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(byte);
        }
    }
    out.push(b'\'');
    out
}

/// Drain one child pipe on its own thread, keeping at most `max_out` bytes.
/// Overflow keeps draining into the void so the child is never wedged on a
/// full pipe; the returned flag lets the caller treat truncation as an error
/// instead of silently acting on a short capture.
type BoundedReader = std::thread::JoinHandle<io::Result<(Vec<u8>, bool)>>;

fn spawn_bounded_reader(pipe: impl Read + Send + 'static, max_out: usize) -> BoundedReader {
    std::thread::spawn(move || {
        let mut limited = pipe.take(max_out as u64 + 1);
        let mut buf = Vec::new();
        limited.read_to_end(&mut buf)?;
        if buf.len() <= max_out {
            return Ok((buf, false));
        }
        buf.truncate(max_out);
        io::copy(&mut limited.into_inner(), &mut io::sink())?;
        Ok((buf, true))
    })
}

fn join_reader(reader: Option<BoundedReader>) -> io::Result<(Vec<u8>, bool)> {
    match reader {
        Some(handle) => handle
            .join()
            .map_err(|_| io::Error::other("probe output reader panicked"))?,
        None => Ok((Vec::new(), false)),
    }
}

/// Kill the child and (Unix) its whole process group, which it was made to
/// lead at spawn: one signal reaps the probe and every descendant that did
/// not setsid away — a remote `tar`, a relay pipeline — instead of orphaning
/// them on the pipes. Returns immediately; the caller reaps.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: one kill on the group the child was made to lead at
            // spawn; failure (already exited, or never a group leader) is
            // harmless and the plain kill below still covers the child.
            unsafe {
                nix::libc::kill(-pid, nix::libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

/// Spawn `argv[0]` with piped stdio, feed it `stdin_bytes`, and capture both
/// output streams bounded to `max_out`. A watchdog kills the child once
/// `timeout` passes so a stuck ssh/docker can never wedge a worker thread.
fn run_capture(
    argv: &[OsString],
    stdin_bytes: &[u8],
    timeout: Duration,
    max_out: usize,
) -> io::Result<Capture> {
    run_capture_inner(argv, stdin_bytes, timeout, max_out, None)
}

fn run_capture_with_cancellation(
    argv: &[OsString],
    stdin_bytes: &[u8],
    timeout: Duration,
    max_out: usize,
    cancellation: &crate::file_tree::ScanCancellation,
) -> io::Result<Capture> {
    run_capture_inner(argv, stdin_bytes, timeout, max_out, Some(cancellation))
}

fn run_capture_inner(
    argv: &[OsString],
    stdin_bytes: &[u8],
    timeout: Duration,
    max_out: usize,
    cancellation: Option<&crate::file_tree::ScanCancellation>,
) -> io::Result<Capture> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty probe argv",
        ));
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, led by the child: one group kill below reaps the
        // probe and everything it forked.
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    // The script is smaller than any pipe buffer, so this write cannot
    // deadlock against the child's output. A far side that exits without
    // reading turns it into a broken pipe, which is not an error here.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_bytes);
    }
    let stdout_reader = child
        .stdout
        .take()
        .map(|pipe| spawn_bounded_reader(pipe, max_out));
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| spawn_bounded_reader(pipe, max_out));

    let started = std::time::Instant::now();
    let status = loop {
        if cancellation.is_some_and(crate::file_tree::ScanCancellation::is_cancelled) {
            kill_tree(&mut child);
            let _ = child.wait();
            let _ = join_reader(stdout_reader);
            let _ = join_reader(stderr_reader);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "directory scan was superseded",
            ));
        }
        match child.try_wait()? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                kill_tree(&mut child);
                let _ = child.wait();
                // The kill closes the pipes; collect the readers so no
                // thread outlives the capture it fed.
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} did not answer within {}s",
                        crate::file_tree::display_os_str(program),
                        timeout.as_secs()
                    ),
                ));
            }
            None => std::thread::sleep(WATCHDOG_POLL_INTERVAL),
        }
    };
    let (stdout, stdout_truncated) = join_reader(stdout_reader)?;
    let (stderr, stderr_truncated) = join_reader(stderr_reader)?;
    if stdout_truncated || stderr_truncated {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} produced more than {} bytes of output",
                crate::file_tree::display_os_str(program),
                max_out
            ),
        ));
    }
    Ok(Capture {
        code: status.code(),
        stdout,
        stderr,
    })
}

// ---------------------------------------------------------------------------
// Streaming transfers (cross-location paste)
// ---------------------------------------------------------------------------

/// A spawned probe prepared for streaming: script delivered per mode, pipes
/// detached, stderr draining on a bounded reader thread, and the child behind
/// a lock so the watchdog can kill it mid-stream.
struct ProbeChild {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<BoundedReader>,
}

impl ProbeChild {
    fn handle(&self) -> Arc<Mutex<Child>> {
        self.child.clone()
    }
}

/// try_wait in a loop: never hold the child lock across a blocking wait, or
/// the watchdog could not kill a hung transfer.
fn wait_child(child: &Arc<Mutex<Child>>) -> io::Result<std::process::ExitStatus> {
    loop {
        {
            let mut guard = child
                .lock()
                .map_err(|_| io::Error::other("probe child lock poisoned"))?;
            if let Some(status) = guard.try_wait()? {
                return Ok(status);
            }
        }
        std::thread::sleep(WATCHDOG_POLL_INTERVAL);
    }
}

/// Shared per-transfer state: cancellation from the UI, the overall timeout
/// from the guard thread, and the children either one kills. One control is
/// created per transfer before spawning; clones go to the worker, the cancel
/// button, and (for relays) each leg.
#[derive(Clone)]
pub(crate) struct TransferControl {
    inner: Arc<TransferControlInner>,
}

struct TransferControlInner {
    cancelled: AtomicBool,
    timed_out: AtomicBool,
    children: Mutex<Vec<Arc<Mutex<Child>>>>,
}

impl TransferControl {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(TransferControlInner {
                cancelled: AtomicBool::new(false),
                timed_out: AtomicBool::new(false),
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Register a child so cancel/timeout can kill it mid-stream. A cancel
    /// that already fired applies to late registrations immediately.
    fn register(&self, child: &Arc<Mutex<Child>>) {
        if let Ok(mut children) = self.inner.children.lock() {
            children.push(child.clone());
        }
        if self.is_cancelled() {
            Self::kill_child(child);
        }
    }

    fn kill_child(child: &Arc<Mutex<Child>>) {
        if let Ok(mut child) = child.lock() {
            kill_tree(&mut child);
        }
    }

    fn kill_all(&self) {
        if let Ok(children) = self.inner.children.lock() {
            for child in children.iter() {
                Self::kill_child(child);
            }
        }
    }

    /// UI cancel: flag, then kill every registered child exactly like the
    /// timeout does. Idempotent; safe to race a completed transfer (killing
    /// an exited child is a no-op).
    pub(crate) fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.kill_all();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn is_timed_out(&self) -> bool {
        self.inner.timed_out.load(Ordering::SeqCst)
    }

    /// The error a worker returns when it notices the cancel.
    pub(crate) fn check(&self) -> io::Result<()> {
        if self.is_cancelled() {
            Err(cancelled_error())
        } else {
            Ok(())
        }
    }

    /// Start the overall timeout: after `timeout`, flag and kill everything
    /// registered so far or later. The returned guard stops the timer on
    /// drop.
    fn arm_timeout(&self, timeout: Duration) -> io::Result<TransferTimeoutGuard> {
        let (cancel, rx) = mpsc::channel::<()>();
        let control = self.clone();
        let handle = std::thread::Builder::new()
            .name("anvil-fs-transfer-watchdog".to_string())
            .spawn(move || {
                if rx.recv_timeout(timeout).is_err() {
                    control.inner.timed_out.store(true, Ordering::SeqCst);
                    control.kill_all();
                }
            })?;
        Ok(TransferTimeoutGuard {
            cancel,
            handle: Some(handle),
        })
    }
}

/// Stops the transfer timeout thread on drop.
struct TransferTimeoutGuard {
    cancel: mpsc::Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for TransferTimeoutGuard {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Cancellation is not a failure: Interrupted is the neutral signal the UI
/// maps to a plain "cancelled" notice instead of an error toast.
pub(crate) fn cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "transfer cancelled")
}

fn transfer_timed_out_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "transfer exceeded the {}-minute limit",
            TRANSFER_TIMEOUT.as_secs() / 60
        ),
    )
}

fn too_large_error(max: u64) -> io::Error {
    io::Error::other(format!(
        "transfer exceeds the {} MiB limit",
        max / (1024 * 1024)
    ))
}

/// Spawn the probe for streaming: piped stdio, script delivered per `mode`
/// (for `Stdin` the script is written and stdin closed, so the far side's sh
/// starts executing), stderr draining bounded on a reader thread, and (Unix)
/// the child leading its own process group so cancel/timeout can reap the
/// whole pipeline.
fn spawn_probe_argv(argv: &[OsString], mode: ScriptDelivery) -> io::Result<ProbeChild> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty probe argv",
        ));
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take();
    if mode == ScriptDelivery::Stdin {
        if let Some(mut pipe) = stdin.take() {
            let _ = pipe.write_all(PROBE_SCRIPT.as_bytes());
        }
    }
    let stdout = child.stdout.take();
    let stderr = child
        .stderr
        .take()
        .map(|pipe| spawn_bounded_reader(pipe, MAX_TRANSFER_STDERR_BYTES));
    Ok(ProbeChild {
        child: Arc::new(Mutex::new(child)),
        stdin,
        stdout,
        stderr,
    })
}

fn spawn_probe_streaming(
    host: &RemoteHost,
    op: &str,
    args: &[&OsStr],
    mode: ScriptDelivery,
) -> io::Result<ProbeChild> {
    let argv = checked_probe_argv(host, op, args, mode)?;
    spawn_probe_argv(&argv, mode)
}

/// Progress is reported at most ~4 times per second and only once at least
/// 256 KiB have moved since the last emission; the final total is always
/// emitted by the pump at clean EOF.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(250);
const PROGRESS_MIN_DELTA_BYTES: u64 = 256 * 1024;

/// Time- and size-throttled progress accumulator behind `stream_to`.
struct ProgressThrottle {
    last_emit: std::time::Instant,
    last_bytes: u64,
    total: u64,
}

impl ProgressThrottle {
    fn new() -> Self {
        Self {
            last_emit: std::time::Instant::now(),
            last_bytes: 0,
            total: 0,
        }
    }

    #[cfg(test)]
    fn aged(age: Duration) -> Self {
        Self {
            last_emit: std::time::Instant::now() - age,
            last_bytes: 0,
            total: 0,
        }
    }

    /// Account for `delta` more bytes; Some(total) when an emission is due.
    fn update(&mut self, delta: u64) -> Option<u64> {
        self.total += delta;
        if self.total - self.last_bytes >= PROGRESS_MIN_DELTA_BYTES
            && self.last_emit.elapsed() >= PROGRESS_MIN_INTERVAL
        {
            self.last_emit = std::time::Instant::now();
            self.last_bytes = self.total;
            Some(self.total)
        } else {
            None
        }
    }

    fn total(&self) -> u64 {
        self.total
    }
}

/// 12.4 MiB-style formatting for transfer progress: binary units, one
/// decimal below 10, whole numbers at or above.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Pump `from` into `to` in 64 KiB chunks, enforcing `max` and reporting
/// throttled progress. On overflow every registered child is killed so no
/// partial payload keeps moving. A broken pipe means the far side exited
/// early — its exit code tells the real story, so the pump stops quietly and
/// lets the caller read it.
fn stream_to<R: Read, W: Write>(
    mut from: R,
    mut to: W,
    max: u64,
    control: &TransferControl,
    on_progress: &dyn Fn(u64),
) -> io::Result<u64> {
    let mut buf = [0u8; STREAM_BUF_SIZE];
    let mut throttle = ProgressThrottle::new();
    loop {
        let read = from.read(&mut buf)?;
        if read == 0 {
            let total = throttle.total();
            on_progress(total);
            return Ok(total);
        }
        if let Some(total) = throttle.update(read as u64) {
            on_progress(total);
        }
        if throttle.total() > max {
            control.kill_all();
            return Err(too_large_error(max));
        }
        match to.write_all(&buf[..read]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                return Ok(throttle.total());
            }
            Err(error) => return Err(error),
        }
    }
}

fn open_transfer_staging(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// One exclusively-created, owner-only staging file beside its eventual
/// destination. Its fixed-size basename neither exposes user-controlled bytes
/// nor grows with a filesystem-limit name.
struct StagedFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl StagedFile {
    fn beside(anchor: &Path) -> io::Result<(Self, std::fs::File)> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self::beside_with(anchor, || NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn beside_with(
        anchor: &Path,
        mut next: impl FnMut() -> usize,
    ) -> io::Result<(Self, std::fs::File)> {
        let parent = anchor
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..32 {
            let path = parent.join(format!(".anvil-fs-part-{}-{}", std::process::id(), next()));
            // A legitimate target may have our internal name shape. Never
            // reserve that target itself, even while it is absent.
            if path.file_name() == anchor.file_name() {
                continue;
            }
            match open_transfer_staging(&path) {
                Ok(file) => {
                    use std::os::unix::fs::MetadataExt;

                    let metadata = match file.metadata() {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            drop(file);
                            let _ = std::fs::remove_file(&path);
                            return Err(error);
                        }
                    };
                    return Ok((
                        Self {
                            path,
                            device: metadata.dev(),
                            inode: metadata.ino(),
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a private transfer staging path",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;

        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
fn reserve_part_then_spawn(
    path: &Path,
    spawn: impl FnOnce() -> io::Result<ProbeChild>,
) -> io::Result<(std::fs::File, ProbeChild)> {
    // Reserve before starting a producer: O_EXCL refuses a planted symlink,
    // and an open failure cannot leave an unobserved child behind.
    let file = open_transfer_staging(path)?;
    match spawn() {
        Ok(child) => Ok((file, child)),
        Err(error) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(error)
        }
    }
}

/// Unique staging directory for remote→remote relays, removed on drop.
struct StagingDir(PathBuf);

impl StagingDir {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "anvil-fs-relay-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Private same-parent extraction root for one downloaded directory. The
/// archive never writes into the final namespace, and cleanup only targets
/// this process-owned staging tree.
struct ExtractionDir(PathBuf);

impl ExtractionDir {
    fn beside(dst: &Path) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;

        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let parent = dst
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..32 {
            let path = parent.join(format!(
                ".anvil-fs-extract-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // A remote entry may legitimately have our hidden-name shape;
            // never let the private staging root alias its final path.
            if path.file_name() == dst.file_name() {
                continue;
            }
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a private directory extraction path",
        ))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ExtractionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn extracted_top_level(staging: &Path, dst: &Path) -> io::Result<PathBuf> {
    let expected_name = dst.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
    })?;
    let mut entries = std::fs::read_dir(staging)?;
    let entry = entries.next().transpose()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "directory archive extracted no top-level entry",
        )
    })?;
    if entry.file_name() != expected_name || entries.next().transpose()?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory archive has an unexpected top-level shape",
        ));
    }
    let path = entry.path();
    if !std::fs::symlink_metadata(&path)?.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory archive top-level entry is not a directory",
        ));
    }
    Ok(path)
}

/// Directory transfers shell out to the system `tar` on the local side too;
/// fail up-front with a clear error when it is missing.
fn require_local_tar() -> io::Result<()> {
    match Command::new("tar")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "directory transfers need a working `tar` on this machine",
        )),
    }
}

/// Parsed `stat` output: entry type (dirs first-class) and size in bytes
/// (always 0 for directories and symlinks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteStat {
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
}

/// `stat` one remote path; Ok(None) means it does not exist (probe exit 3).
fn remote_stat(host: &RemoteHost, path: &Path) -> io::Result<Option<RemoteStat>> {
    require_absolute(path)?;
    match run_probe(host, "stat", &[path.as_os_str()], PROBE_LIST_TIMEOUT) {
        Ok(stdout) => parse_stat_output(&stdout).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Parse one `<t> <size>` line; split_whitespace absorbs any `wc` padding.
fn parse_stat_output(bytes: &[u8]) -> io::Result<RemoteStat> {
    let line = bytes
        .split(|&byte| byte == b'\n')
        .next()
        .unwrap_or_default();
    let text = std::str::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stat probe returned non-UTF-8"))?;
    let mut fields = text.split_whitespace();
    let kind = fields.next().unwrap_or_default();
    let size = fields
        .next()
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "stat probe returned no size"))?;
    let is_dir = match kind {
        "d" => true,
        "f" | "l" => false,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stat probe returned an unknown entry type",
            ))
        }
    };
    Ok(RemoteStat { is_dir, size })
}

/// Transfer `src_path` between locations: download (Remote→Local), upload
/// (Local→Remote), or a local staging relay between two remote hosts.
/// Returns the destination path. Same-name collisions fail BEFORE streaming
/// via the probe's `stat`; file uploads and directory untars are additionally
/// enforced atomically by `put`/v3 `untar` exit 17. `control` carries
/// cancel/timeout kill semantics; `progress` receives throttled byte totals.
#[allow(clippy::too_many_arguments)]
pub(crate) fn transfer(
    hosts: &[RemoteHost],
    src_loc: &FsLocation,
    src_path: &Path,
    dst_loc: &FsLocation,
    dst_dir: &Path,
    is_dir: bool,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<PathBuf> {
    control.check()?;
    match (src_loc, dst_loc) {
        (src, FsLocation::Local) if src.is_remote() => download(
            remote_host(src_loc, hosts)?,
            src_path,
            dst_dir,
            is_dir,
            control,
            progress,
        ),
        (FsLocation::Local, dst) if dst.is_remote() => upload(
            remote_host(dst_loc, hosts)?,
            src_path,
            dst_dir,
            is_dir,
            control,
            progress,
        ),
        (src, dst) if src.is_remote() && dst.is_remote() => {
            // No host-to-host channel exists, so relay through a unique local
            // staging dir that is always cleaned up. Progress stays monotonic
            // across both legs: the upload leg offsets by the download's
            // final count.
            let src_host = remote_host(src_loc, hosts)?;
            let dst_host = remote_host(dst_loc, hosts)?;
            let name = src_path.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "remote path has no file name")
            })?;
            if remote_stat(dst_host, &dst_dir.join(name))?.is_some() {
                return Err(already_exists_error(name));
            }
            let staging = StagingDir::new()?;
            let leg1_total = Arc::new(AtomicUsize::new(0));
            let leg1_seen = leg1_total.clone();
            let leg1_progress = move |bytes: u64| {
                leg1_seen.store(bytes as usize, Ordering::Relaxed);
                progress(bytes);
            };
            let staged = download(
                src_host,
                src_path,
                staging.path(),
                is_dir,
                control,
                &leg1_progress,
            )?;
            control.check()?;
            let base = leg1_total.load(Ordering::Relaxed) as u64;
            let leg2_progress = move |bytes: u64| progress(base + bytes);
            upload(dst_host, &staged, dst_dir, is_dir, control, &leg2_progress)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local-to-local paste uses rename/copy, not a transfer",
        )),
    }
}

fn already_exists_error(name: &OsStr) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "{} already exists at the destination",
            crate::file_tree::display_os_str(name)
        ),
    )
}

fn download(
    host: &RemoteHost,
    remote_path: &Path,
    dir: &Path,
    is_dir: bool,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<PathBuf> {
    require_absolute(remote_path)?;
    let name = remote_path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "remote path has no file name")
    })?;
    if is_dir {
        download_dir_with(
            || {
                spawn_probe_streaming(
                    host,
                    "tar",
                    &[remote_path.as_os_str()],
                    ScriptDelivery::Stdin,
                )
            },
            name,
            dir,
            MAX_TRANSFER_BYTES,
            control,
            progress,
        )
    } else {
        download_file_with(
            || {
                spawn_probe_streaming(
                    host,
                    "cat",
                    &[remote_path.as_os_str()],
                    ScriptDelivery::Stdin,
                )
            },
            name,
            dir,
            MAX_TRANSFER_BYTES,
            control,
            progress,
        )
    }
}

fn upload(
    host: &RemoteHost,
    local_path: &Path,
    remote_dir: &Path,
    is_dir: bool,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<PathBuf> {
    require_absolute(remote_dir)?;
    let name = local_path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "local path has no file name")
    })?;
    let dst = remote_dir.join(name);
    // Fail before streaming: one `stat` answers the existence question.
    if remote_stat(host, &dst)?.is_some() {
        return Err(already_exists_error(name));
    }
    if is_dir {
        // v3 untar takes <dir> <name> and refuses an existing destination
        // before extracting anything; the tar stream carries the name.
        upload_dir_with(
            || {
                spawn_probe_streaming(
                    host,
                    "untar",
                    &[remote_dir.as_os_str(), name],
                    ScriptDelivery::Argv,
                )
            },
            local_path,
            MAX_TRANSFER_BYTES,
            control,
            progress,
        )?;
    } else {
        let result = upload_file_with(
            || spawn_probe_streaming(host, "put", &[dst.as_os_str()], ScriptDelivery::Argv),
            local_path,
            MAX_TRANSFER_BYTES,
            control,
            progress,
        );
        if let Err(error) = &result {
            // A kill (cancel/timeout) is the only way the probe's `.fspart`
            // temp survives — the script cleans up after its own failures.
            if error.kind() == io::ErrorKind::Interrupted || error.kind() == io::ErrorKind::TimedOut
            {
                cleanup_remote_part(host, &dst);
            }
        }
        result?;
    }
    Ok(dst)
}

/// The probe's `put` temp name is `"$p.fspart.$$"` with the remote shell's
/// pid unknown to us, so cancel cleanup globs the fixed suffix. The path is
/// single-quote-escaped before the suffix is appended, so nothing here is
/// shell-reinterpreted. Best-effort: errors are logged, never propagated.
fn cleanup_remote_part(host: &RemoteHost, dst: &Path) {
    let command = part_cleanup_command(dst);
    if let Err(message) = crate::config::validate_remote_host(host) {
        log::warn!("remote .fspart cleanup rejected by execution gate: {message}");
        return;
    }
    let argv = host_command_argv(host, &command);
    if let Err(error) = run_capture(&argv, &[], PROBE_OP_TIMEOUT, MAX_CAPTURE_BYTES) {
        log::warn!("remote .fspart cleanup failed to run: {error}");
    }
}

/// `rm -f '<dst>'.fspart.*` — a glob no-match is a harmless literal `rm -f`.
/// A non-UTF-8 destination lossy-mangles into a pattern that matches nothing,
/// which under best-effort semantics is a safe no-op, never a wrong delete.
fn part_cleanup_command(dst: &Path) -> String {
    format!("rm -f {}.fspart.*", sq(&dst.as_os_str().to_string_lossy()))
}

/// Run one small far-side shell command outside the probe protocol (ssh:
/// one re-parsed command element; docker: `sh -c`). Used only for the
/// best-effort cancel cleanup.
fn host_command_argv(host: &RemoteHost, command: &str) -> Vec<OsString> {
    if host.docker {
        let mut argv = vec![
            OsString::from("docker"),
            OsString::from("exec"),
            OsString::from("-i"),
        ];
        if let Some(user) = &host.user {
            argv.push(OsString::from("-u"));
            argv.push(OsString::from(user));
        }
        argv.push(OsString::from(&host.host));
        argv.push(OsString::from("sh"));
        argv.push(OsString::from("-c"));
        argv.push(OsString::from(command));
        argv
    } else {
        let dest = match &host.user {
            Some(user) => format!("{user}@{}", host.host),
            None => host.host.clone(),
        };
        let mut argv: Vec<OsString> = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
            .into_iter()
            .map(OsString::from)
            .collect();
        argv.extend(host.ssh_args.iter().map(OsString::from));
        argv.push(OsString::from("--"));
        argv.push(OsString::from(dest));
        argv.push(OsString::from(command));
        argv
    }
}

/// Stream one remote regular file into `dir`. Takes the probe spawn as a
/// parameter so tests can drive the exact mechanics with a local `sh` as the
/// "remote"; `max` is the payload cap (MAX_TRANSFER_BYTES in production).
fn download_file_with(
    spawn: impl FnOnce() -> io::Result<ProbeChild>,
    name: &OsStr,
    dir: &Path,
    max: u64,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<PathBuf> {
    let dst = dir.join(name);
    require_missing(&dst)?;
    control.check()?;
    // Reserve before spawning the producer. Occupied internal candidates are
    // retried, while failure to reserve cannot leave a child to reap.
    let (temp, mut file) = StagedFile::beside(&dst)?;
    let probe = spawn()?;
    let probe_handle = probe.handle();
    control.register(&probe_handle);
    let mut stdout = probe
        .stdout
        .ok_or_else(|| io::Error::other("probe has no stdout"))?;
    let _timeout = control.arm_timeout(TRANSFER_TIMEOUT)?;
    let streamed = stream_to(&mut stdout, &mut file, max, control, progress);
    drop(stdout);
    let status = wait_child(&probe_handle)?;
    // Transfer stderr is bounded error detail only; truncation is expected
    // and silently capped here, unlike the probe captures above.
    let (stderr, _) = join_reader(probe.stderr)?;
    if control.is_timed_out() {
        return Err(transfer_timed_out_error());
    }
    // A cancel wins over stream/probe errors: it is the user's intent, and
    // the killed far side has no meaningful exit code of its own.
    control.check()?;
    streamed?;
    probe_result(
        "cat",
        Capture {
            code: status.code(),
            stdout: Vec::new(),
            stderr,
        },
    )?;
    file.sync_all()?;
    drop(file);
    // Atomic backstop: a same-name file that appeared while streaming wins;
    // the commit itself, not a check before it, enforces no-overwrite.
    rename_noreplace(temp.path(), &dst)?;
    Ok(dst)
}

/// Stream one local regular file into the probe's `put`. Follows symlinks
/// like the remote `cat` does: a link uploads its target's content.
fn upload_file_with(
    spawn: impl FnOnce() -> io::Result<ProbeChild>,
    local_path: &Path,
    max: u64,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<()> {
    let metadata = std::fs::metadata(local_path)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a regular file",
                crate::file_tree::display_full_path(local_path)
            ),
        ));
    }
    if metadata.len() > max {
        return Err(too_large_error(max));
    }
    control.check()?;
    let probe = spawn()?;
    let probe_handle = probe.handle();
    control.register(&probe_handle);
    let stdin = probe
        .stdin
        .ok_or_else(|| io::Error::other("probe has no stdin"))?;
    // Drain the (normally empty) stdout so a chatty far side cannot block.
    let stdout_drain = probe
        .stdout
        .map(|pipe| spawn_bounded_reader(pipe, MAX_TRANSFER_STDERR_BYTES));
    let _timeout = control.arm_timeout(TRANSFER_TIMEOUT)?;
    let file = std::fs::File::open(local_path)?;
    let streamed = stream_to(file, stdin, max, control, progress);
    // stream_to owns and drops stdin here, so the far side sees the payload
    // EOF before it finishes `put`.
    let status = wait_child(&probe_handle)?;
    let (stderr, _) = join_reader(probe.stderr)?;
    let _ = join_reader(stdout_drain);
    if control.is_timed_out() {
        return Err(transfer_timed_out_error());
    }
    control.check()?;
    streamed?;
    probe_result(
        "put",
        Capture {
            code: status.code(),
            stdout: Vec::new(),
            stderr,
        },
    )
    .map(drop)
}

/// `tar` a local directory and stream it into the probe's `untar`.
fn upload_dir_with(
    spawn: impl FnOnce() -> io::Result<ProbeChild>,
    local_path: &Path,
    max: u64,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<()> {
    require_local_tar()?;
    control.check()?;
    let metadata = std::fs::symlink_metadata(local_path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a directory",
                crate::file_tree::display_full_path(local_path)
            ),
        ));
    }
    let name = local_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "directory has no name"))?;
    let parent = local_path.parent().unwrap_or_else(|| Path::new("/"));
    let mut tar_command = Command::new("tar");
    tar_command
        .args(["cf", "-", "-C"])
        .arg(parent)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The tar registers with the transfer control below; lead its own
        // group so the watchdog's group kill can never miss it.
        tar_command.process_group(0);
    }
    let mut tar = tar_command.spawn()?;
    let tar_stdout = tar
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("local tar has no stdout"))?;
    let tar_stderr = tar
        .stderr
        .take()
        .map(|pipe| spawn_bounded_reader(pipe, MAX_TRANSFER_STDERR_BYTES));
    let tar = Arc::new(Mutex::new(tar));
    let probe = spawn()?;
    let probe_handle = probe.handle();
    control.register(&probe_handle);
    control.register(&tar);
    let stdin = probe
        .stdin
        .ok_or_else(|| io::Error::other("probe has no stdin"))?;
    let stdout_drain = probe
        .stdout
        .map(|pipe| spawn_bounded_reader(pipe, MAX_TRANSFER_STDERR_BYTES));
    let _timeout = control.arm_timeout(TRANSFER_TIMEOUT)?;
    let streamed = stream_to(tar_stdout, stdin, max, control, progress);
    if streamed.is_err() {
        // After an overflow the tar can still be blocked writing; make sure
        // it is gone before reaping.
        if let Ok(mut child) = tar.lock() {
            kill_tree(&mut child);
        }
    }
    let tar_status = wait_child(&tar)?;
    let (tar_stderr, _) = join_reader(tar_stderr)?;
    let status = wait_child(&probe_handle)?;
    let (stderr, _) = join_reader(probe.stderr)?;
    let _ = join_reader(stdout_drain);
    if control.is_timed_out() {
        return Err(transfer_timed_out_error());
    }
    control.check()?;
    streamed?;
    if !tar_status.success() {
        let detail = bounded_stderr_text(&tar_stderr);
        return Err(io::Error::other(format!(
            "local tar failed: {}",
            if detail.is_empty() {
                format!("exit status {tar_status}")
            } else {
                detail
            }
        )));
    }
    probe_result(
        "untar",
        Capture {
            code: status.code(),
            stdout: Vec::new(),
            stderr,
        },
    )
    .map(drop)
}

/// Stream a remote directory's tar into `dir` through the local `tar`.
fn download_dir_with(
    spawn: impl FnOnce() -> io::Result<ProbeChild>,
    name: &OsStr,
    dir: &Path,
    max: u64,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<PathBuf> {
    let dst = dir.join(name);
    require_missing(&dst)?;
    require_local_tar()?;
    control.check()?;
    let staging = ExtractionDir::beside(&dst)?;
    let probe = spawn()?;
    let probe_handle = probe.handle();
    let stdout = probe
        .stdout
        .ok_or_else(|| io::Error::other("probe has no stdout"))?;
    let mut tar_command = Command::new("tar");
    tar_command
        .args(["xf", "-", "-C"])
        .arg(staging.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The tar registers with the transfer control below; lead its own
        // group so the watchdog's group kill can never miss it.
        tar_command.process_group(0);
    }
    let mut tar = tar_command.spawn()?;
    let tar_stdin = tar
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("local tar has no stdin"))?;
    let tar_stderr = tar
        .stderr
        .take()
        .map(|pipe| spawn_bounded_reader(pipe, MAX_TRANSFER_STDERR_BYTES));
    let tar = Arc::new(Mutex::new(tar));
    control.register(&probe_handle);
    control.register(&tar);
    let _timeout = control.arm_timeout(TRANSFER_TIMEOUT)?;
    let streamed = stream_to(stdout, tar_stdin, max, control, progress);
    if streamed.is_err() {
        if let Ok(mut child) = tar.lock() {
            kill_tree(&mut child);
        }
    }
    let tar_status = wait_child(&tar)?;
    let (tar_stderr, _) = join_reader(tar_stderr)?;
    let status = wait_child(&probe_handle)?;
    let (stderr, _) = join_reader(probe.stderr)?;
    if control.is_timed_out() {
        return Err(transfer_timed_out_error());
    }
    control.check()?;
    let outcome = streamed
        .and_then(|_| {
            // The remote's own exit code is the root cause more often than
            // the local tar's, so it wins the error report.
            probe_result(
                "tar",
                Capture {
                    code: status.code(),
                    stdout: Vec::new(),
                    stderr,
                },
            )
            .map(drop)
        })
        .and_then(|_| {
            if tar_status.success() {
                Ok(())
            } else {
                let detail = bounded_stderr_text(&tar_stderr);
                Err(io::Error::other(format!(
                    "local tar failed: {}",
                    if detail.is_empty() {
                        format!("exit status {tar_status}")
                    } else {
                        detail
                    }
                )))
            }
        });
    outcome?;
    let extracted = extracted_top_level(staging.path(), &dst)?;
    // The directory becomes visible only after a complete, validated archive;
    // a destination that raced the transfer wins intact.
    rename_noreplace(&extracted, &dst)?;
    Ok(dst)
}

// ---------------------------------------------------------------------------
// Drag-and-drop import
// ---------------------------------------------------------------------------

/// A file-manager drop is refused wholesale past 256 items.
pub(crate) const MAX_DROP_ITEMS: usize = 256;
/// Size-walk depth bound for dropped directories; deeper payloads are not
/// counted (the per-item stream cap still applies at transfer time).
const MAX_DROP_WALK_DEPTH: usize = 64;

/// One dropped item's fate: local recursive copy, or upload via `transfer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropAction {
    Copy,
    Upload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropItem {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    /// Walked payload estimate, used for progress and the total cap.
    pub(crate) size: u64,
    pub(crate) action: DropAction,
    /// Local destinations are collision-checked at plan time; remote ones
    /// fail per item at transfer time via the probe's existence checks.
    pub(crate) collides: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropPlan {
    pub(crate) items: Vec<DropItem>,
    pub(crate) total_bytes: u64,
}

/// Why a drop is refused wholesale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DropRejection {
    Empty,
    NotAbsolute(PathBuf),
    Unreadable(PathBuf),
    TooManyItems(usize),
    TooLarge(u64),
}

/// Total payload estimate of one dropped item: directories are walked without
/// ever following symlinks, bounded to depth 64.
fn drop_item_size(path: &Path, depth: usize) -> io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        if depth >= MAX_DROP_WALK_DEPTH {
            return Ok(0);
        }
        let mut total = 0u64;
        for entry in std::fs::read_dir(path)? {
            total = total.saturating_add(drop_item_size(&entry?.path(), depth + 1)?);
        }
        Ok(total)
    } else if metadata.file_type().is_symlink() {
        // Never followed: a linked directory is neither walked nor sized.
        Ok(0)
    } else {
        Ok(metadata.len())
    }
}

/// Plan a file-manager drop onto `dst_dir` in `dst_loc`: per-item action,
/// walked size, and local collision flag — or a wholesale refusal with the
/// reason. Production entry point uses MAX_TRANSFER_BYTES; tests pass a
/// smaller cap.
pub(crate) fn plan_drop(
    paths: &[PathBuf],
    dst_loc: &FsLocation,
    dst_dir: &Path,
) -> Result<DropPlan, DropRejection> {
    plan_drop_with_limit(paths, dst_loc, dst_dir, MAX_TRANSFER_BYTES)
}

fn plan_drop_with_limit(
    paths: &[PathBuf],
    dst_loc: &FsLocation,
    dst_dir: &Path,
    max_bytes: u64,
) -> Result<DropPlan, DropRejection> {
    if paths.is_empty() {
        return Err(DropRejection::Empty);
    }
    if paths.len() > MAX_DROP_ITEMS {
        return Err(DropRejection::TooManyItems(paths.len()));
    }
    let action = if dst_loc.is_remote() {
        DropAction::Upload
    } else {
        DropAction::Copy
    };
    let mut items = Vec::with_capacity(paths.len());
    let mut total_bytes = 0u64;
    for path in paths {
        if !path.is_absolute() {
            return Err(DropRejection::NotAbsolute(path.clone()));
        }
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| DropRejection::Unreadable(path.clone()))?;
        let size = drop_item_size(path, 0).map_err(|_| DropRejection::Unreadable(path.clone()))?;
        total_bytes = total_bytes.saturating_add(size);
        if total_bytes > max_bytes {
            return Err(DropRejection::TooLarge(total_bytes));
        }
        let collides = action == DropAction::Copy
            && path
                .file_name()
                .is_some_and(|name| std::fs::symlink_metadata(dst_dir.join(name)).is_ok());
        items.push(DropItem {
            path: path.clone(),
            is_dir: metadata.is_dir(),
            size,
            action,
            collides,
        });
    }
    Ok(DropPlan { items, total_bytes })
}

/// Per-item result of a multi-item batch (drop import, batch delete, batch
/// paste): how many succeeded, and (display name, error) per failed item.
/// Batches continue past ordinary failures and only abort on cancellation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchOutcome {
    pub(crate) done: usize,
    pub(crate) failed: Vec<(String, String)>,
}

/// A delete batch additionally retains the exact paths that were removed.
/// Display labels in `BatchOutcome::failed` are intentionally unsuitable as
/// identities (two paths can share a basename), so clipboard retirement must
/// consume this list rather than infer success from counts or labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeleteBatchOutcome {
    pub(crate) summary: BatchOutcome,
    pub(crate) succeeded: Vec<PathBuf>,
}

/// Format one failed batch item's error for the summary toast.
fn batch_failure(outcome: &mut BatchOutcome, display: String, error: &io::Error) {
    outcome
        .failed
        .push((display, user_facing_fs_error(error).to_string()));
}

/// Display name of a batch item path (escaped, unambiguous).
fn batch_display_name(path: &Path) -> String {
    let display = match path.file_name() {
        Some(name) => crate::file_tree::display_os_str(name),
        None => crate::file_tree::display_full_path(path),
    };
    jterm_core::review_input::safe_inline_display(&display, MAX_ERROR_DISPLAY_BYTES)
}

/// Paste every clipboard item into `dst_dir`: same-location items rename
/// (cut) or recursive-copy, cross-location items stream via `transfer` with
/// cancel/timeout intact. Cut sources are deleted only after their own
/// transfer succeeded. `progress` receives the running completed-item count.
pub(crate) fn direct_paste_execution_location<'a>(
    hosts: &[RemoteHost],
    clip_loc: &'a FsLocation,
    dst_loc: &'a FsLocation,
) -> Option<&'a FsLocation> {
    if !locations_share_filesystem(clip_loc, dst_loc, hosts) {
        return None;
    }
    // Prefer a proven live ControlPath on either side, destination first only
    // when both carry one. Then prefer any value-owned endpoint before falling
    // back to the index-backed destination.
    match (clip_loc, dst_loc) {
        (_, FsLocation::Transient(endpoint)) if endpoint.has_execution_overlay() => Some(dst_loc),
        (FsLocation::Transient(endpoint), _) if endpoint.has_execution_overlay() => Some(clip_loc),
        (_, FsLocation::Transient(_)) => Some(dst_loc),
        (FsLocation::Transient(_), _) => Some(clip_loc),
        _ => Some(dst_loc),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn paste_all(
    hosts: &[RemoteHost],
    clip_loc: &FsLocation,
    items: &[FsClipboardItem],
    dst_loc: &FsLocation,
    dst_dir: &Path,
    cut: bool,
    control: &TransferControl,
    progress: &dyn Fn(u64),
    source_consumed: &dyn Fn(&Path),
) -> io::Result<BatchOutcome> {
    // When a saved clipboard source and a temporary ControlPath endpoint share
    // one namespace, every direct mutation uses the destination's live
    // execution snapshot. The saved source is identity only; using it here
    // could prompt again or lose the socket between copy and cut deletion.
    let direct_location = direct_paste_execution_location(hosts, clip_loc, dst_loc);
    let same_location = direct_location.is_some();
    let mut outcome = BatchOutcome {
        done: 0,
        failed: Vec::new(),
    };
    let mut moved = 0u64;
    for item in items {
        control.check()?;
        let display = batch_display_name(&item.path);
        let dst = paste_destination(dst_dir, &item.path);
        let result = if same_location {
            if dst == item.path {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "source and destination are the same",
                ))
            } else if cut {
                rename(
                    direct_location.expect("same namespace has an execution endpoint"),
                    hosts,
                    &item.path,
                    &dst,
                )
            } else {
                copy(
                    direct_location.expect("same namespace has an execution endpoint"),
                    hosts,
                    &item.path,
                    &dst,
                )
            }
        } else {
            transfer(
                hosts,
                clip_loc,
                &item.path,
                dst_loc,
                dst_dir,
                item.is_dir,
                control,
                &|_| {},
            )
            .map(drop)
        };
        match result {
            Ok(()) => {
                outcome.done += 1;
                if cut && !same_location {
                    // The copy half of the cut landed; deleting the source is
                    // best-effort and reported per item.
                    match delete(clip_loc, hosts, &item.path) {
                        Ok(()) => source_consumed(&item.path),
                        Err(error) => {
                            let message = user_facing_fs_error(&error);
                            outcome.failed.push((
                                display,
                                format!("copied, but deleting the source failed: {message}"),
                            ));
                        }
                    }
                } else if cut {
                    source_consumed(&item.path);
                }
            }
            Err(error) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    return Err(error);
                }
                batch_failure(&mut outcome, display, &error);
            }
        }
        moved += 1;
        progress(moved);
    }
    Ok(outcome)
}

/// Delete every path, continuing past failures. `/` is refused per item by
/// the delete op itself.
pub(crate) fn delete_all(
    loc: &FsLocation,
    hosts: &[RemoteHost],
    paths: &[PathBuf],
) -> DeleteBatchOutcome {
    let mut outcome = BatchOutcome {
        done: 0,
        failed: Vec::new(),
    };
    let mut succeeded = Vec::new();
    for path in paths {
        match delete(loc, hosts, path) {
            Ok(()) => {
                outcome.done += 1;
                succeeded.push(path.clone());
            }
            Err(error) => batch_failure(&mut outcome, batch_display_name(path), &error),
        }
    }
    DeleteBatchOutcome {
        summary: outcome,
        succeeded,
    }
}

/// Execute a drop plan item by item: copies via the local recursive copier,
/// uploads through `transfer` (so progress/cancel/atomicity apply). Progress
/// is cumulative across items and never dips. A cancel aborts the batch with
/// the neutral Interrupted error.
pub(crate) fn run_drop(
    plan: &DropPlan,
    dst_loc: &FsLocation,
    hosts: &[RemoteHost],
    dst_dir: &Path,
    control: &TransferControl,
    progress: &dyn Fn(u64),
) -> io::Result<BatchOutcome> {
    let mut outcome = BatchOutcome {
        done: 0,
        failed: Vec::new(),
    };
    let mut moved = 0u64;
    let high_water = std::cell::Cell::new(0u64);
    let report = |bytes: u64| {
        if bytes > high_water.get() {
            high_water.set(bytes);
            progress(bytes);
        }
    };
    for item in &plan.items {
        control.check()?;
        let Some(name) = item.path.file_name() else {
            outcome.failed.push((
                crate::file_tree::display_full_path(&item.path),
                "the path has no file name".to_string(),
            ));
            continue;
        };
        let display = jterm_core::review_input::safe_inline_display(
            &crate::file_tree::display_os_str(name),
            MAX_ERROR_DISPLAY_BYTES,
        );
        if item.collides {
            outcome
                .failed
                .push((display, "already exists at the destination".to_string()));
            moved = moved.saturating_add(item.size);
            continue;
        }
        let dst = dst_dir.join(name);
        let result = match item.action {
            DropAction::Copy => copy(&FsLocation::Local, &[], &item.path, &dst),
            DropAction::Upload => {
                let base = moved;
                let item_progress = |bytes: u64| report(base + bytes);
                transfer(
                    hosts,
                    &FsLocation::Local,
                    &item.path,
                    dst_loc,
                    dst_dir,
                    item.is_dir,
                    control,
                    &item_progress,
                )
                .map(drop)
            }
        };
        match result {
            Ok(()) => outcome.done += 1,
            Err(error) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    return Err(error);
                }
                outcome
                    .failed
                    .push((display, user_facing_fs_error(&error).to_string()));
            }
        }
        moved = moved.saturating_add(item.size);
        report(moved);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ssh_host() -> RemoteHost {
        RemoteHost {
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

    fn docker_host() -> RemoteHost {
        RemoteHost {
            name: "service".to_string(),
            host: "my-service".to_string(),
            user: Some("devuser".to_string()),
            docker: true,
            deploy_artifact: None,
            remote_shell: "jsh".to_string(),
            session: None,
            ssh_args: vec!["-p".to_string(), "2222".to_string()],
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Off,
        }
    }

    fn session_location(host: RemoteHost, managed: bool) -> FsLocation {
        SessionRemoteEndpoint::new(host, managed, None)
            .map(FsLocation::session)
            .expect("valid session endpoint")
    }

    /// Unique temp directory that removes itself on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(test: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "anvil-remote-fs-{}-{}-{test}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir must be creatable");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The far-side sh invocation, exactly as the ssh/docker argv would shape
    /// it for each delivery mode.
    fn local_probe_argv(args: &[&str], mode: ScriptDelivery) -> Vec<OsString> {
        let mut argv = vec![OsString::from("sh")];
        match mode {
            ScriptDelivery::Stdin => argv.push(OsString::from("-s")),
            ScriptDelivery::Argv => {
                argv.push(OsString::from("-c"));
                argv.push(OsString::from(PROBE_SCRIPT));
            }
        }
        argv.push(OsString::from("--"));
        argv.extend(args.iter().map(OsString::from));
        argv
    }

    /// Run the probe script directly through the local `sh`, the same way
    /// ssh/docker would deliver it on a far side.
    fn local_probe(args: &[&str]) -> Capture {
        let argv = local_probe_argv(args, ScriptDelivery::Stdin);
        run_capture(
            &argv,
            PROBE_SCRIPT.as_bytes(),
            Duration::from_secs(10),
            MAX_CAPTURE_BYTES,
        )
        .expect("local sh probe must run")
    }

    /// Payload ops run Command-mode: the script is on argv, stdin is the data
    /// channel — the exact wire shape `put`/`untar` use over ssh/docker.
    fn local_probe_payload(args: &[&str], payload: &[u8]) -> Capture {
        let argv = local_probe_argv(args, ScriptDelivery::Argv);
        run_capture(&argv, payload, Duration::from_secs(10), MAX_CAPTURE_BYTES)
            .expect("local sh probe must run")
    }

    fn spawn_local(args: &[&str], mode: ScriptDelivery) -> io::Result<ProbeChild> {
        let argv = local_probe_argv(args, mode);
        spawn_probe_argv(&argv, mode)
    }

    /// Binary-safe fixture content with NULs and high bytes.
    fn binary_content(seed: u8, len: usize) -> Vec<u8> {
        (0..=255u8)
            .map(|b| b.wrapping_add(seed))
            .cycle()
            .take(len)
            .collect()
    }

    #[test]
    fn sq_single_quotes_and_escapes_embedded_quotes() {
        assert_eq!(sq("plain"), "'plain'");
        assert_eq!(sq("with space"), "'with space'");
        assert_eq!(sq("don't"), "'don'\\''t'");
        assert_eq!(sq("'wrapped'"), "''\\''wrapped'\\'''");
        assert_eq!(sq(""), "''");
    }

    #[test]
    fn ssh_argv_quotes_the_whole_probe_invocation() {
        let host = ssh_host();
        let dir = OsString::from("/tmp/a b");
        let argv = ssh_probe_argv(&host, "list", &[dir.as_os_str()], ScriptDelivery::Stdin);
        let text: Vec<_> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text[..5],
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]
        );
        // ssh_args land before the `--` that ends option parsing.
        let sep = text.iter().position(|a| a == "--").expect("separator");
        assert_eq!(&text[5..sep], ["-p", "2222"]);
        assert_eq!(text[sep + 1], "deploy@server.example.com");
        assert_eq!(text[sep + 2], "sh -s -- 'list' '/tmp/a b'");
        assert_eq!(text.len(), sep + 3, "the command is ONE argv element");
    }

    #[test]
    fn ssh_argv_escapes_quotes_inside_paths() {
        let host = ssh_host();
        let dir = OsString::from("/tmp/don't");
        let argv = ssh_probe_argv(&host, "list", &[dir.as_os_str()], ScriptDelivery::Stdin);
        let command = argv.last().expect("command element").to_string_lossy();
        assert_eq!(command, "sh -s -- 'list' '/tmp/don'\\''t'");
    }

    #[test]
    fn production_list_probe_argv_carries_entries_plus_one_limit() {
        let limit = LIST_PROBE_ENTRY_LIMIT.to_string();
        assert_eq!(
            LIST_PROBE_ENTRY_LIMIT,
            crate::file_tree::MAX_DIRECTORY_ENTRIES + 1
        );
        let dir = OsStr::new("/remote/tree");
        let ssh = ssh_probe_argv(
            &ssh_host(),
            "list",
            &[dir, OsStr::new(&limit)],
            ScriptDelivery::Stdin,
        );
        assert_eq!(
            ssh.last().expect("one remote command").to_string_lossy(),
            format!("sh -s -- 'list' '/remote/tree' '{limit}'")
        );

        let docker = docker_probe_argv(
            &docker_host(),
            "list",
            &[dir, OsStr::new(&limit)],
            ScriptDelivery::Stdin,
        );
        assert_eq!(docker[docker.len() - 2], OsStr::new("/remote/tree"));
        assert_eq!(docker.last().unwrap(), OsStr::new(&limit));
    }

    #[test]
    fn ssh_argv_without_user_uses_bare_host() {
        let mut host = ssh_host();
        host.user = None;
        let argv = ssh_probe_argv(&host, "home", &[], ScriptDelivery::Stdin);
        let text: Vec<_> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let sep = text.iter().position(|a| a == "--").expect("separator");
        assert_eq!(text[sep + 1], "server.example.com");
        assert_eq!(text[sep + 2], "sh -s -- 'home'");
    }

    #[test]
    fn docker_argv_passes_raw_words_without_quoting() {
        let host = docker_host();
        let dir = OsString::from("/tmp/a b'c");
        let argv = docker_probe_argv(&host, "list", &[dir.as_os_str()], ScriptDelivery::Stdin);
        let text: Vec<_> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text,
            [
                "docker",
                "exec",
                "-i",
                "-u",
                "devuser",
                "my-service",
                "sh",
                "-s",
                "--",
                "list",
                "/tmp/a b'c"
            ]
        );
    }

    #[test]
    fn docker_argv_omits_user_flag_when_unset() {
        let mut host = docker_host();
        host.user = None;
        let argv = docker_probe_argv(&host, "home", &[], ScriptDelivery::Stdin);
        let text: Vec<_> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text,
            [
                "docker",
                "exec",
                "-i",
                "my-service",
                "sh",
                "-s",
                "--",
                "home"
            ]
        );
    }

    #[test]
    fn list_output_parses_pairs_with_weird_names() {
        let dir = Path::new("/base");
        let mut bytes = Vec::new();
        for (kind, name) in [
            ("d", "dir with spaces"),
            ("f", "quo'te.txt"),
            ("l", "link\nname"),
            ("f", "back\\slash"),
        ] {
            bytes.extend_from_slice(kind.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
        }
        // A remote name that cannot be represented exactly as UTF-8 is not
        // admitted as an actionable path.
        bytes.extend_from_slice(b"f\0bad\xffname\0");
        let listing = parse_list_output(&bytes, dir);
        let entries = listing.entries();
        let names: Vec<_> = entries.iter().map(|e| e.name().to_string()).collect();
        assert_eq!(entries.len(), 4);
        assert_eq!(names[0], "dir with spaces", "directories sort first");
        assert!(entries[0].is_dir());
        assert!(names.contains(&"quo'te.txt".to_string()));
        assert!(names.contains(&"link\nname".to_string()));
        assert!(names.contains(&r"back\\slash".to_string()));
        assert!(entries.iter().all(|entry| entry.path().to_str().is_some()));
        assert!(!listing.truncated());
    }

    #[test]
    fn list_output_skips_malformed_and_dangerous_fields() {
        let dir = Path::new("/base");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"x\0unknown-kind\0");
        bytes.extend_from_slice(b"d\0\0"); // empty name
        bytes.extend_from_slice(b"d\0.\0");
        bytes.extend_from_slice(b"d\0..\0");
        bytes.extend_from_slice(b"f\0has/slash\0");
        bytes.extend_from_slice(b"d\0good\0");
        bytes.extend_from_slice(b"trailing-without-name");
        let listing = parse_list_output(&bytes, dir);
        let entries = listing.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "good");
        assert_eq!(entries[0].path(), PathBuf::from("/base/good"));
    }

    #[test]
    fn list_output_deduplicates_names_and_suppresses_type_collisions() {
        let dir = Path::new("/base");
        let bytes = b"f\0once\0f\0once\0d\0conflict\0f\0conflict\0d\0safe\0";
        let listing = parse_list_output(bytes, dir);
        let entries = listing.entries();
        let names: Vec<_> = entries.iter().map(FileEntry::name).collect();
        assert_eq!(names, ["safe", "once"]);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name() == "once")
                .count(),
            1
        );
        assert!(!entries.iter().any(|entry| entry.name() == "conflict"));
    }

    #[test]
    fn list_output_truncates_beyond_the_entry_cap() {
        let dir = Path::new("/base");
        let mut bytes = Vec::new();
        for i in 0..crate::file_tree::MAX_DIRECTORY_ENTRIES + 32 {
            bytes.extend_from_slice(format!("f\0file{i:05}\0").as_bytes());
        }
        let listing = parse_list_output(&bytes, dir);
        assert_eq!(
            listing.entries().len(),
            crate::file_tree::MAX_DIRECTORY_ENTRIES
        );
        assert!(listing.truncated());
    }

    #[test]
    fn list_output_empty_buffer_yields_no_entries() {
        assert!(parse_list_output(b"", Path::new("/base"))
            .entries()
            .is_empty());
        assert!(parse_list_output(b"\0\0", Path::new("/base"))
            .entries()
            .is_empty());
    }

    #[test]
    fn home_output_requires_an_absolute_first_line() {
        assert_eq!(
            parse_home_output(b"/root\n").unwrap(),
            PathBuf::from("/root")
        );
        assert_eq!(
            parse_home_output(b"/home/u\r\nextra\n").unwrap(),
            PathBuf::from("/home/u")
        );
        assert!(parse_home_output(b"").is_err());
        assert!(parse_home_output(b"relative/dir\n").is_err());
    }

    #[test]
    fn new_name_validation_matches_dialog_rules() {
        assert!(new_name_error("plain.txt").is_none());
        assert!(new_name_error("with space").is_none());
        assert!(new_name_error("").is_some());
        assert!(new_name_error(&"x".repeat(255)).is_none());
        assert!(new_name_error(&"x".repeat(256)).is_some());
        assert!(new_name_error(".").is_some());
        assert!(new_name_error("..").is_some());
        assert!(new_name_error("a/b").is_some());
        assert!(new_name_error("a\0b").is_some());
    }

    #[test]
    fn remote_ops_reject_relative_paths() {
        let hosts = vec![ssh_host()];
        let loc = FsLocation::Remote(0);
        let rel = Path::new("relative/dir");
        assert_eq!(
            list_dir(&loc, &hosts, rel).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            create_dir(&loc, &hosts, rel).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            delete(&loc, &hosts, rel).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            rename(&loc, &hosts, rel, Path::new("/abs"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            copy(&loc, &hosts, Path::new("/abs"), rel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn stale_remote_index_is_a_clean_not_found() {
        let loc = FsLocation::Remote(7);
        let hosts = vec![ssh_host()];
        assert_eq!(
            list_dir(&loc, &hosts, Path::new("/tmp"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            start_dir(&loc, &hosts).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn delete_refuses_the_filesystem_root() {
        let hosts = vec![ssh_host()];
        for loc in [FsLocation::Local, FsLocation::Remote(0)] {
            assert_eq!(
                delete(&loc, &hosts, Path::new("/")).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn into_self_moves_are_rejected_before_spawning() {
        let hosts = vec![ssh_host()];
        let src = Path::new("/data");
        let dst = Path::new("/data/inner");
        for loc in [FsLocation::Local, FsLocation::Remote(0)] {
            assert!(rename(&loc, &hosts, src, dst).is_err());
            assert!(copy(&loc, &hosts, src, dst).is_err());
            assert!(rename(&loc, &hosts, src, src).is_err());
        }
    }

    #[test]
    fn local_copy_into_self_through_a_symlink_alias_is_rejected() {
        let tmp = TestDir::new("copy-self-alias");
        let hosts: Vec<RemoteHost> = Vec::new();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("f.txt"), b"x").unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        // Textually outside `real`, but the alias resolves back inside it.
        let dst = alias.join("inner");
        let err = copy(&FsLocation::Local, &hosts, &real, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!real.join("inner").exists());
        // A sibling destination outside the source still copies fine.
        let sibling = tmp.path().join("sibling");
        copy(&FsLocation::Local, &hosts, &real, &sibling).unwrap();
        assert_eq!(std::fs::read(sibling.join("f.txt")).unwrap(), b"x");
    }

    #[test]
    fn local_copy_descends_no_deeper_than_the_copy_depth_limit() {
        let tmp = TestDir::new("copy-depth");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(src.join("inner")).unwrap();
        // At the limit the guard fires before the destination is created.
        let err = copy_recursive(&src, &tmp.path().join("dst"), MAX_COPY_DEPTH).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!tmp.path().join("dst").exists());
        // One level below, the child directory is what trips the guard.
        let err = copy_recursive(&src, &tmp.path().join("dst2"), MAX_COPY_DEPTH - 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn run_capture_rejects_output_beyond_the_cap_instead_of_truncating() {
        // Exactly at the cap the capture succeeds.
        let argv = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("printf '%01024d' 0"),
        ];
        let capture = run_capture(&argv, b"", Duration::from_secs(5), 1024)
            .expect("an exactly-at-cap capture is fine");
        assert_eq!(capture.stdout.len(), 1024);
        // One byte past it is an error, not a silent short listing.
        let argv = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("printf '%01025d' 0"),
        ];
        let err = run_capture(&argv, b"", Duration::from_secs(5), 1024)
            .map(drop)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn run_capture_drains_past_the_cap_so_a_verbose_child_cannot_wedge() {
        // 200 KB far exceeds a pipe buffer: without drain-to-sink the child
        // would block on write until the watchdog fired.
        let argv = vec![
            OsString::from("head"),
            OsString::from("-c"),
            OsString::from("200000"),
            OsString::from("/dev/zero"),
        ];
        let started = std::time::Instant::now();
        let err = run_capture(&argv, b"", Duration::from_secs(10), 1024)
            .map(drop)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an undrained child would have hit the watchdog timeout"
        );
    }

    #[test]
    fn cancelled_list_capture_kills_the_in_flight_process_group() {
        let argv = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("sleep 30"),
        ];
        let cancellation = crate::file_tree::ScanCancellation::default();
        let worker_cancellation = cancellation.clone();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            run_capture_with_cancellation(
                &argv,
                b"",
                Duration::from_secs(30),
                1024,
                &worker_cancellation,
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn kill_tree_reaps_the_whole_process_group() {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let pgid = child.id() as i32;
        // Give the background sleep time to exec before the kill lands.
        std::thread::sleep(Duration::from_millis(200));
        kill_tree(&mut child);
        let _ = child.wait();
        for _ in 0..50 {
            // SAFETY: signal 0 probes group existence without signaling it.
            let group_alive = unsafe { nix::libc::kill(-pgid, 0) } == 0;
            if !group_alive {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("process group {pgid} survived kill_tree");
    }

    #[test]
    fn probe_exit_codes_map_to_io_kinds() {
        let capture = |code, stderr: &[u8]| Capture {
            code: Some(code),
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        };
        assert_eq!(
            probe_result("mkdir", capture(17, b"")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            probe_result("list", capture(3, b"")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            probe_result("list", capture(2, b"")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            probe_result("list", capture(255, b"ssh transport failed"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            probe_result("rm", capture(4, b"rm: cannot remove"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
        let err = probe_result("rm", capture(4, b"rm: nope")).unwrap_err();
        assert!(err.to_string().contains("rm: nope"));
        assert_eq!(
            probe_result("list", capture(0, b"")).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn remote_failure_ui_is_classified_and_never_repeats_hostile_stderr() {
        let secret = "deploy:hunter2@private.example\u{202e}\u{1b}[31m";
        let error = probe_result(
            "list",
            Capture {
                code: Some(255),
                stdout: Vec::new(),
                stderr: secret.as_bytes().to_vec(),
            },
        )
        .unwrap_err();
        assert_eq!(classify_fs_error(&error), FsFailureKind::Connection);
        let visible = user_facing_fs_error(&error);
        assert_eq!(visible, "The remote connection is unavailable.");
        assert!(!visible.contains("hunter2"));
        assert!(!visible.contains("private.example"));
        assert!(!visible.chars().any(char::is_control));

        for (kind, expected) in [
            (io::ErrorKind::WouldBlock, FsFailureKind::QueueFull),
            (io::ErrorKind::TimedOut, FsFailureKind::TimedOut),
            (io::ErrorKind::NotFound, FsFailureKind::Missing),
            (io::ErrorKind::PermissionDenied, FsFailureKind::Permission),
            (io::ErrorKind::AlreadyExists, FsFailureKind::Exists),
            (io::ErrorKind::InvalidData, FsFailureKind::InvalidResponse),
            (io::ErrorKind::InvalidInput, FsFailureKind::InvalidRequest),
        ] {
            let error = io::Error::new(kind, secret);
            assert_eq!(classify_fs_error(&error), expected);
            assert!(!user_facing_fs_error(&error).contains("hunter2"));
        }

        let display = batch_display_name(Path::new("/tmp/report\u{202e}\u{1b}[31m.txt"));
        assert!(!display.contains('\u{202e}'));
        assert!(!display.chars().any(char::is_control));
    }

    #[test]
    fn probe_lists_a_real_directory_through_sh() {
        let tmp = TestDir::new("probe-list");
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("file.txt"), b"data").unwrap();
        std::os::unix::fs::symlink("file.txt", tmp.path().join("alink")).unwrap();
        std::os::unix::fs::symlink("subdir", tmp.path().join("dirlink")).unwrap();

        let limit = LIST_PROBE_ENTRY_LIMIT.to_string();
        let capture = local_probe(&["list", tmp.path().to_str().unwrap(), &limit]);
        assert_eq!(capture.code, Some(0));
        let listing = parse_list_output(&capture.stdout, tmp.path());
        let entries = listing.entries();
        let names: Vec<_> = entries
            .iter()
            .map(|e| (e.name().to_string(), e.is_dir()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("subdir".to_string(), true),
                ("alink".to_string(), false),
                ("dirlink".to_string(), false),
                ("file.txt".to_string(), false),
            ],
            "symlinks, including links to directories, are never expandable"
        );
    }

    #[test]
    fn probe_list_stops_at_the_client_supplied_hard_limit() {
        let tmp = TestDir::new("probe-list-limit");
        for name in ["a", "b", "c"] {
            std::fs::write(tmp.path().join(name), b"x").unwrap();
        }
        let capture = local_probe(&["list", tmp.path().to_str().unwrap(), "2"]);
        assert_eq!(capture.code, Some(0));
        assert_eq!(
            capture.stdout.split(|byte| *byte == 0).count() - 1,
            4,
            "two entries produce exactly four NUL-terminated fields"
        );
        assert_eq!(
            local_probe(&["list", tmp.path().to_str().unwrap(), "0"]).code,
            Some(EXIT_USAGE)
        );
        assert_eq!(
            local_probe(&["list", tmp.path().to_str().unwrap(), "not-a-limit"]).code,
            Some(EXIT_USAGE)
        );
    }

    #[test]
    fn probe_ops_round_trip_through_sh() {
        let tmp = TestDir::new("probe-ops");
        let dir = tmp.path().join("dir");
        let file = tmp.path().join("file");
        let moved = tmp.path().join("moved");
        let copied = tmp.path().join("copied");

        assert_eq!(local_probe(&["mkdir", dir.to_str().unwrap()]).code, Some(0));
        assert!(dir.is_dir());
        assert_eq!(
            local_probe(&["mkdir", dir.to_str().unwrap()]).code,
            Some(17)
        );

        assert_eq!(
            local_probe(&["mkfile", file.to_str().unwrap()]).code,
            Some(0)
        );
        assert!(file.is_file());
        assert_eq!(
            local_probe(&["mkfile", file.to_str().unwrap()]).code,
            Some(17)
        );

        assert_eq!(
            local_probe(&["mv", file.to_str().unwrap(), moved.to_str().unwrap()]).code,
            Some(0)
        );
        assert!(!file.exists() && moved.is_file());

        assert_eq!(
            local_probe(&["cp", dir.to_str().unwrap(), copied.to_str().unwrap()]).code,
            Some(0)
        );
        assert!(copied.is_dir());

        assert_eq!(local_probe(&["rm", moved.to_str().unwrap()]).code, Some(0));
        assert!(!moved.exists());
        assert_eq!(local_probe(&["rm", copied.to_str().unwrap()]).code, Some(0));
        assert!(!copied.exists());

        // Relative and root-level targets are usage errors, not operations.
        assert_eq!(local_probe(&["rm", "relative"]).code, Some(2));
        assert_eq!(local_probe(&["rm", "/"]).code, Some(2));
        assert_eq!(local_probe(&["bogus"]).code, Some(2));
        assert_eq!(
            local_probe(&["list", tmp.path().join("missing").to_str().unwrap(), "2",]).code,
            Some(3)
        );
    }

    #[test]
    fn probe_creators_refuse_dangling_symlink_targets() {
        fn assert_refused(capture: &Capture, link: &Path, victim: &Path) {
            assert_eq!(
                capture.code,
                Some(EXIT_EXISTS),
                "stderr: {:?}",
                capture.stderr
            );
            assert!(
                !victim.exists(),
                "probe followed dangling link and created {}",
                victim.display()
            );
            assert!(
                std::fs::symlink_metadata(link)
                    .expect("destination link must remain")
                    .file_type()
                    .is_symlink(),
                "probe replaced destination link {}",
                link.display()
            );
        }

        let tmp = TestDir::new("probe-dangling-targets");
        let dangling = |name: &str| {
            let victim = tmp.path().join(format!("outside-{name}"));
            let link = tmp.path().join(format!("link-{name}"));
            std::os::unix::fs::symlink(&victim, &link).unwrap();
            let argument = link.to_str().unwrap().to_string();
            (victim, link, argument)
        };

        let (victim, link, argument) = dangling("mkdir");
        assert_refused(&local_probe(&["mkdir", &argument]), &link, &victim);

        let (victim, link, argument) = dangling("mkfile");
        assert_refused(&local_probe(&["mkfile", &argument]), &link, &victim);

        let move_source = tmp.path().join("move-source");
        std::fs::write(&move_source, b"move").unwrap();
        let move_source_arg = move_source.to_str().unwrap().to_string();
        let (victim, link, argument) = dangling("move");
        assert_refused(
            &local_probe(&["mv", &move_source_arg, &argument]),
            &link,
            &victim,
        );
        assert_eq!(std::fs::read(&move_source).unwrap(), b"move");

        let copy_source = tmp.path().join("copy-source");
        std::fs::write(&copy_source, b"copy").unwrap();
        let copy_source_arg = copy_source.to_str().unwrap().to_string();
        let (victim, link, argument) = dangling("copy");
        assert_refused(
            &local_probe(&["cp", &copy_source_arg, &argument]),
            &link,
            &victim,
        );
        assert_eq!(std::fs::read(&copy_source).unwrap(), b"copy");

        let (victim, link, argument) = dangling("put");
        assert_refused(
            &local_probe_payload(&["put", &argument], b"payload"),
            &link,
            &victim,
        );

        let extraction_dir = tmp.path().join("extract");
        std::fs::create_dir(&extraction_dir).unwrap();
        let extraction_arg = extraction_dir.to_str().unwrap().to_string();
        let victim = tmp.path().join("outside-untar");
        let link = extraction_dir.join("tree");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert_refused(
            &local_probe_payload(
                &["untar", &extraction_arg, "tree"],
                b"not consulted when the target is occupied",
            ),
            &link,
            &victim,
        );
    }

    #[test]
    fn probe_home_returns_an_absolute_directory() {
        let capture = local_probe(&["home"]);
        assert_eq!(capture.code, Some(0));
        let home = parse_home_output(&capture.stdout).unwrap();
        assert!(home.is_absolute() && home.is_dir());
    }

    #[test]
    fn local_create_rename_copy_delete_round_trip() {
        let tmp = TestDir::new("local-ops");
        let hosts: Vec<RemoteHost> = Vec::new();
        let loc = FsLocation::Local;
        let dir = tmp.path().join("dir");
        let file = tmp.path().join("file.txt");

        create_dir(&loc, &hosts, &dir).unwrap();
        assert_eq!(
            create_dir(&loc, &hosts, &dir).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        create_file(&loc, &hosts, &file).unwrap();
        assert_eq!(
            create_file(&loc, &hosts, &file).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        let renamed = tmp.path().join("renamed.txt");
        rename(&loc, &hosts, &file, &renamed).unwrap();
        assert!(!file.exists() && renamed.is_file());
        create_file(&loc, &hosts, &file).unwrap();
        assert_eq!(
            rename(&loc, &hosts, &file, &renamed).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        // Recursive directory copy, then a collision on the same target.
        std::fs::write(dir.join("inner.txt"), b"inner").unwrap();
        let dir_copy = tmp.path().join("dir-copy");
        copy(&loc, &hosts, &dir, &dir_copy).unwrap();
        assert_eq!(std::fs::read(dir_copy.join("inner.txt")).unwrap(), b"inner");
        assert_eq!(
            copy(&loc, &hosts, &dir, &dir_copy).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        delete(&loc, &hosts, &dir_copy).unwrap();
        assert!(!dir_copy.exists());
        delete(&loc, &hosts, &renamed).unwrap();
        assert!(!renamed.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_rename_refuses_a_destination_created_after_preflight() {
        let tmp = TestDir::new("rename-noreplace-race");
        let src = tmp.path().join("source");
        let dst = tmp.path().join("destination");
        std::fs::write(&src, b"source bytes").unwrap();

        let error = rename_noreplace_with(&src, &dst, |src, dst| {
            // Occupy the name after require_missing returned, at the exact
            // boundary the production syscall must protect.
            std::fs::write(dst, b"racing winner")?;
            atomic_rename_noreplace(src, dst)
        })
        .expect_err("the concurrent destination must win");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&src).unwrap(), b"source bytes");
        assert_eq!(std::fs::read(&dst).unwrap(), b"racing winner");
    }

    #[test]
    fn local_delete_removes_the_symlink_not_its_target() {
        let tmp = TestDir::new("local-symlink");
        let hosts: Vec<RemoteHost> = Vec::new();
        let loc = FsLocation::Local;
        let target = tmp.path().join("target");
        create_dir(&loc, &hosts, &target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        delete(&loc, &hosts, &link).unwrap();
        assert!(!link.exists() && target.is_dir());
    }

    #[test]
    fn local_copy_preserves_symlinks_as_links() {
        let tmp = TestDir::new("local-copy-link");
        let hosts: Vec<RemoteHost> = Vec::new();
        let loc = FsLocation::Local;
        std::fs::write(tmp.path().join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink("real.txt", tmp.path().join("alink")).unwrap();
        let dst = tmp.path().join("copy");
        copy(&loc, &hosts, tmp.path().join("alink").as_path(), &dst).unwrap();
        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&dst).unwrap(), PathBuf::from("real.txt"));
    }

    #[test]
    fn paste_destination_joins_the_clipboard_file_name() {
        assert_eq!(
            paste_destination(Path::new("/dst"), Path::new("/src/note.txt")),
            PathBuf::from("/dst/note.txt")
        );
        assert_eq!(
            paste_destination(Path::new("/dst"), Path::new("/")),
            PathBuf::from("/")
        );
    }

    #[test]
    fn location_labels_cover_local_then_hosts() {
        let hosts = vec![ssh_host(), docker_host()];
        assert_eq!(
            location_labels(&hosts),
            ["Local", "ssh: staging", "docker: service"]
        );
        assert_eq!(FsLocation::Local.label(&hosts), "Local");
        assert_eq!(FsLocation::Remote(1).label(&hosts), "docker: service");
        assert_eq!(FsLocation::Remote(9).label(&hosts), "Remote (unavailable)");
        let transient = session_location(ssh_host(), false);
        assert_eq!(transient.label(&[]), "ssh: staging (temporary)");
        assert_eq!(
            location_labels_for(&hosts, &transient),
            [
                "Local",
                "ssh: staging",
                "docker: service",
                "ssh: staging (temporary)"
            ]
        );
        let managed_session = session_location(hosts[0].clone(), true);
        assert_eq!(managed_session.label(&hosts), "ssh: staging");
        assert_eq!(
            location_labels_for(&hosts, &managed_session),
            location_labels(&hosts)
        );

        let mut hostile = ssh_host();
        hostile.name = "secret\u{202e}marker".to_string();
        assert_eq!(
            location_labels(&[hostile]),
            ["Local", "Remote (unavailable)"]
        );
    }

    #[test]
    fn long_cloud_endpoint_label_is_compact_but_detail_stays_complete() {
        let endpoint = "dsw-notebook-dsw-l8rnh0wm7vs81o7z6j-22.vpc-0jlbz3pri2042fd5xw2ov.instance-forward.dsw.cn-wulanchabu.aliyuncs.com";
        let mut host = ssh_host();
        host.name = format!("root@{endpoint}");
        host.host = endpoint.to_string();
        host.user = Some("root".to_string());
        host.ssh_args = vec!["-p".to_string(), "22".to_string()];
        host.deploy = jterm_core::jsh_remote::Deploy::Off;
        host.multiplex = false;

        let location = session_location(host.clone(), false);
        let labels = location_labels_for(&[], &location);
        assert_eq!(labels.len(), 2);
        assert!(labels[1].starts_with("ssh: root@dsw"));
        assert!(labels[1].contains('…'));
        assert!(labels[1].contains("aliyuncs.com"));
        assert!(labels[1].ends_with(" (temporary)"));
        assert!(!labels[1].contains("instance-forward"));

        let details = location_details_for(&[], &location);
        assert_eq!(details.len(), labels.len());
        assert!(details[1].contains(&format!("root@{endpoint}")));
        assert!(details[1].ends_with("· options: -p 22 (temporary)"));
    }

    #[test]
    fn remote_location_reordering_follows_only_the_complete_profile() {
        let ssh = ssh_host();
        let docker = docker_host();
        let old = vec![ssh.clone(), docker.clone()];
        let reordered = vec![docker, ssh];

        assert_eq!(
            remap_location_by_profile(&FsLocation::Remote(1), &old, &reordered),
            FsLocation::Remote(0)
        );
        assert_eq!(
            remap_location_by_profile(&FsLocation::Remote(0), &old, &reordered),
            FsLocation::Remote(1)
        );
        assert_eq!(
            remap_location_by_profile(&FsLocation::Local, &old, &reordered),
            FsLocation::Local
        );

        let transient = session_location(ssh_host(), false);
        assert_eq!(
            remap_location_by_profile(&transient, &old, &reordered),
            transient,
            "session-only authority is independent of configured indexes"
        );
    }

    #[test]
    fn transient_remote_host_is_validated_from_the_location_itself() {
        let transient = session_location(ssh_host(), false);
        assert_eq!(
            remote_host(&transient, &[]).expect("valid transient host"),
            match &transient {
                FsLocation::Transient(endpoint) => endpoint.execution(),
                _ => unreachable!(),
            }
        );

        let mut invalid = ssh_host();
        invalid.host = "-option".to_string();
        assert!(SessionRemoteEndpoint::new(invalid, false, None).is_err());
    }

    #[test]
    fn session_endpoint_control_path_is_execution_only_and_reaches_probe_argv() {
        let base = ssh_host();
        let first =
            SessionRemoteEndpoint::new(base.clone(), false, Some("/run/user/1000/anvil/cm-%C"))
                .expect("safe execution overlay");
        let second =
            SessionRemoteEndpoint::new(base.clone(), false, Some("/run/user/1000/anvil/new-cm-%C"))
                .expect("safe execution overlay");
        assert_eq!(first, second, "socket paths are not stable identity");
        assert_eq!(first.identity().ssh_args, base.ssh_args);
        assert_eq!(
            &first.execution().ssh_args[first.execution().ssh_args.len() - 2..],
            ["-S", "/run/user/1000/anvil/cm-%C"]
        );

        let location = FsLocation::session(first);
        let execution = remote_host(&location, &[]).expect("frozen execution endpoint");
        let argv = ssh_probe_argv(execution, "home", &[], ScriptDelivery::Stdin);
        let argv = argv
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let socket = argv.iter().position(|argument| argument == "-S").unwrap();
        assert_eq!(argv[socket + 1], "/run/user/1000/anvil/cm-%C");
    }

    #[test]
    fn saved_clipboard_and_temporary_overlay_use_one_direct_live_namespace() {
        let mut saved = ssh_host();
        saved
            .ssh_args
            .extend(["-S".to_string(), "/saved/cm-%C".to_string()]);
        let mut stable = saved.clone();
        stable.ssh_args.truncate(2);
        let temporary = FsLocation::session(
            SessionRemoteEndpoint::with_execution_overlay(
                stable,
                None,
                &["-S".to_string(), "/live/cm-%C".to_string()],
            )
            .expect("live session endpoint"),
        );
        let saved_location = FsLocation::Remote(0);
        let hosts = vec![saved];
        assert!(locations_share_filesystem(
            &saved_location,
            &temporary,
            &hosts
        ));
        assert_eq!(
            direct_paste_execution_location(&hosts, &saved_location, &temporary),
            Some(&temporary),
            "same-namespace copy/cut must execute through the live destination overlay"
        );
        assert_eq!(
            direct_paste_execution_location(&hosts, &temporary, &FsLocation::Remote(0)),
            Some(&temporary),
            "temporary source to saved destination must retain the live source socket"
        );
        let clipboard = Some(FsClipboard {
            loc: saved_location,
            items: vec![FsClipboardItem {
                path: PathBuf::from("/remote/file"),
                is_dir: false,
            }],
            cut: true,
            token: 91,
        });
        assert_eq!(
            clipboard_token_for_location(&clipboard, &temporary, &hosts),
            Some(91),
            "rename/delete through the live endpoint retires the saved-source clipboard"
        );
        let live = remote_host(&temporary, &hosts).expect("execution profile");
        assert_eq!(
            &live.ssh_args[live.ssh_args.len() - 2..],
            ["-S", "/live/cm-%C"]
        );

        let mut other = temporary.clone();
        if let FsLocation::Transient(endpoint) = &mut other {
            let mut identity = endpoint.identity().clone();
            identity.ssh_args = vec!["-p".to_string(), "2200".to_string()];
            **endpoint = SessionRemoteEndpoint::new(identity, false, None).unwrap();
        }
        assert!(!locations_share_filesystem(
            &FsLocation::Remote(0),
            &other,
            &hosts
        ));
    }

    #[test]
    fn edited_removed_stale_or_ambiguous_remote_profiles_fall_back_local() {
        let ssh = ssh_host();
        let docker = docker_host();
        let old = vec![ssh.clone(), docker.clone()];

        let mut edited = docker.clone();
        edited.host.push_str("-replacement");
        assert_eq!(
            remap_location_by_profile(&FsLocation::Remote(1), &old, &[ssh.clone(), edited]),
            FsLocation::Local,
            "the same slot/name must not authorize an edited destination"
        );
        assert_eq!(
            remap_location_by_profile(&FsLocation::Remote(1), &old, &[ssh]),
            FsLocation::Local,
            "a removed profile must not redirect to another slot"
        );
        assert_eq!(
            remap_location_by_profile(&FsLocation::Remote(9), &old, &old),
            FsLocation::Local,
            "a stale old index has no identity to preserve"
        );
        assert_eq!(
            remap_location_by_profile(&FsLocation::Remote(1), &old, &[docker.clone(), docker]),
            FsLocation::Local,
            "an ambiguous exact identity fails closed"
        );
    }

    fn test_clipboard(loc: FsLocation, token: u64, path: &str) -> Option<FsClipboard> {
        Some(FsClipboard {
            loc,
            items: vec![FsClipboardItem {
                path: PathBuf::from(path),
                is_dir: false,
            }],
            cut: true,
            token,
        })
    }

    #[test]
    fn clipboard_retirement_requires_the_exact_user_intent_token() {
        let affected = [PathBuf::from("/remote/same.txt")];
        let mut newer_same_payload = test_clipboard(FsLocation::Remote(0), 12, "/remote/same.txt");

        assert!(!retire_clipboard_sources(
            &mut newer_same_payload,
            Some(11),
            &affected
        ));
        assert_eq!(
            newer_same_payload.as_ref().map(|clipboard| clipboard.token),
            Some(12),
            "re-copying identical rows is still a new intent"
        );
        assert!(!retire_clipboard_sources(
            &mut newer_same_payload,
            None,
            &affected,
        ));
        assert!(retire_clipboard_sources(
            &mut newer_same_payload,
            Some(12),
            &affected,
        ));
        assert!(newer_same_payload.is_none());
    }

    #[test]
    fn delayed_paste_resolves_only_its_live_token_and_observes_safe_remap() {
        let mut clipboard = test_clipboard(FsLocation::Remote(0), 21, "/remote/same.txt");
        assert_eq!(
            clipboard_for_token(&clipboard, 21)
                .expect("original intent resolves")
                .loc,
            FsLocation::Remote(0)
        );

        // Repeating the same Copy/Cut payload is still a new user intent.
        clipboard.as_mut().unwrap().token = 22;
        assert!(clipboard_for_token(&clipboard, 21).is_none());

        // An exact profile reorder mutates only the live source location; a
        // delayed menu must use that reconciled authority, not its old index.
        clipboard.as_mut().unwrap().loc = FsLocation::Remote(3);
        assert_eq!(
            clipboard_for_token(&clipboard, 22)
                .expect("live intent survives reorder")
                .loc,
            FsLocation::Remote(3)
        );
    }

    #[test]
    fn clipboard_source_retirement_is_bound_at_dispatch_and_covers_descendants() {
        let mut clipboard = test_clipboard(FsLocation::Remote(0), 31, "/tree/dir/child.txt");
        assert_eq!(
            clipboard_token_for_location(&clipboard, &FsLocation::Local, &[]),
            None,
            "a Local operation cannot consume a remote clipboard with the same path"
        );
        let token = clipboard_token_for_location(&clipboard, &FsLocation::Remote(0), &[]);
        assert_eq!(token, Some(31));

        // Reordering after dispatch is harmless because the token, rather than
        // the now-changed numeric location, identifies the same intent.
        clipboard.as_mut().unwrap().loc = FsLocation::Remote(2);
        assert!(retire_clipboard_sources(
            &mut clipboard,
            token,
            &[PathBuf::from("/tree/dir")]
        ));
        assert!(clipboard.is_none());
    }

    #[test]
    fn clipboard_profile_reorder_preserves_token_but_unsafe_remap_clears() {
        let ssh = ssh_host();
        let docker = docker_host();
        let old = vec![ssh.clone(), docker.clone()];
        let reordered = vec![docker, ssh.clone()];
        let mut clipboard = test_clipboard(FsLocation::Remote(0), 77, "/remote/source.txt");

        assert!(!remap_clipboard_by_profile(
            &mut clipboard,
            &old,
            &reordered
        ));
        let remapped = clipboard.as_ref().expect("exact profile survives reorder");
        assert_eq!(remapped.loc, FsLocation::Remote(1));
        assert_eq!(remapped.token, 77, "reorder preserves intent identity");
        assert!(retire_clipboard_sources(
            &mut clipboard,
            Some(77),
            &[PathBuf::from("/remote/source.txt")],
        ));

        let mut replaced = test_clipboard(FsLocation::Remote(0), 78, "/remote/source.txt");
        let mut edited = ssh;
        edited.host = "replacement.example.com".to_string();
        assert!(remap_clipboard_by_profile(&mut replaced, &old, &[edited]));
        assert!(replaced.is_none());
    }

    #[test]
    fn local_list_dir_matches_scan_dir() {
        let tmp = TestDir::new("local-list");
        std::fs::create_dir(tmp.path().join("zdir")).unwrap();
        std::fs::write(tmp.path().join("afile"), b"x").unwrap();
        let listing = list_dir(&FsLocation::Local, &[], tmp.path()).unwrap();
        let entries = listing.entries();
        let names: Vec<_> = entries.iter().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["zdir", "afile"]);
        assert!(!listing.truncated());
    }

    // -- probe v2 payload ops, end to end through local sh -------------------

    #[test]
    fn probe_v2_cat_streams_binary_content() {
        let tmp = TestDir::new("probe-cat");
        let content = binary_content(0, 10_000);
        let file = tmp.path().join("bin.dat");
        std::fs::write(&file, &content).unwrap();

        let capture = local_probe(&["cat", file.to_str().unwrap()]);
        assert_eq!(capture.code, Some(0));
        assert_eq!(capture.stdout, content);
        // Missing files and directories are exit 3, not a stream.
        assert_eq!(
            local_probe(&["cat", tmp.path().join("nope").to_str().unwrap()]).code,
            Some(3)
        );
        assert_eq!(
            local_probe(&["cat", tmp.path().to_str().unwrap()]).code,
            Some(3)
        );
    }

    #[test]
    fn probe_v2_put_writes_new_files_atomically() {
        let tmp = TestDir::new("probe-put");
        let file = tmp.path().join("out.bin");
        let payload = binary_content(7, 20_000);

        let capture = local_probe_payload(&["put", file.to_str().unwrap()], &payload);
        assert_eq!(capture.code, Some(0));
        assert_eq!(std::fs::read(&file).unwrap(), payload);

        // Existing destination is refused, and no temp litter remains.
        assert_eq!(
            local_probe_payload(&["put", file.to_str().unwrap()], b"x").code,
            Some(17)
        );
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains("fspart"))
            .collect();
        assert!(leftovers.is_empty(), "no .fspart temp files left behind");
    }

    #[test]
    fn probe_v3_tar_untar_round_trip() {
        let tmp = TestDir::new("probe-tar");
        let src = tmp.path().join("srcdir");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        let content = binary_content(3, 5_000);
        std::fs::write(src.join("nested").join("data.bin"), &content).unwrap();
        std::fs::write(src.join("top.txt"), b"hello").unwrap();

        let tarred = local_probe(&["tar", src.to_str().unwrap()]);
        assert_eq!(tarred.code, Some(0));
        assert!(!tarred.stdout.is_empty());

        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let capture =
            local_probe_payload(&["untar", out.to_str().unwrap(), "srcdir"], &tarred.stdout);
        assert_eq!(capture.code, Some(0));
        assert_eq!(
            std::fs::read(out.join("srcdir").join("nested").join("data.bin")).unwrap(),
            content
        );
        assert_eq!(
            std::fs::read(out.join("srcdir").join("top.txt")).unwrap(),
            b"hello"
        );

        // A non-directory source and a missing destination are exit 3.
        assert_eq!(
            local_probe(&["tar", src.join("top.txt").to_str().unwrap()]).code,
            Some(3)
        );
        assert_eq!(
            local_probe_payload(
                &[
                    "untar",
                    tmp.path().join("missing").to_str().unwrap(),
                    "srcdir"
                ],
                b"x"
            )
            .code,
            Some(3)
        );
        // Bad names are usage errors, not operations.
        for bad in ["", ".", "..", "a/b"] {
            assert_eq!(
                local_probe_payload(&["untar", out.to_str().unwrap(), bad], b"x").code,
                Some(2),
                "name {bad:?} is rejected"
            );
        }
    }

    #[test]
    fn probe_v3_untar_refuses_existing_name_before_extracting() {
        let tmp = TestDir::new("probe-untar-17");
        let src = tmp.path().join("srcdir");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        let tarred = local_probe(&["tar", src.to_str().unwrap()]);
        assert_eq!(tarred.code, Some(0));

        // The destination already holds a `srcdir` with a marker file.
        let out = tmp.path().join("out");
        std::fs::create_dir_all(out.join("srcdir")).unwrap();
        std::fs::write(out.join("srcdir").join("marker.txt"), b"keep").unwrap();

        let capture =
            local_probe_payload(&["untar", out.to_str().unwrap(), "srcdir"], &tarred.stdout);
        assert_eq!(capture.code, Some(17));
        assert_eq!(
            std::fs::read(out.join("srcdir").join("marker.txt")).unwrap(),
            b"keep",
            "the existing tree is untouched"
        );
        assert!(
            !out.join("srcdir").join("new.txt").exists(),
            "nothing was extracted before the refusal"
        );
    }

    #[test]
    fn probe_v3_stat_reports_type_and_size() {
        let tmp = TestDir::new("probe-stat");
        let content = binary_content(4, 7_654);
        let file = tmp.path().join("data.bin");
        std::fs::write(&file, &content).unwrap();
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::os::unix::fs::symlink("data.bin", tmp.path().join("link")).unwrap();
        std::os::unix::fs::symlink("dir", tmp.path().join("dir-link")).unwrap();
        let fifo = tmp.path().join("fifo");
        let fifo_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(
            // SAFETY: `fifo_path` is a live NUL-terminated path and the mode
            // contains only ordinary permission bits.
            unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) },
            0,
            "mkfifo failed: {}",
            io::Error::last_os_error()
        );

        let capture = local_probe(&["stat", file.to_str().unwrap()]);
        assert_eq!(capture.code, Some(0));
        assert_eq!(capture.stdout, format!("f {}\n", content.len()).as_bytes());

        let capture = local_probe(&["stat", tmp.path().join("dir").to_str().unwrap()]);
        assert_eq!(capture.stdout, b"d 0\n");
        let capture = local_probe(&["stat", tmp.path().join("link").to_str().unwrap()]);
        assert_eq!(capture.stdout, b"l 0\n");
        let capture = local_probe(&["stat", tmp.path().join("dir-link").to_str().unwrap()]);
        assert_eq!(
            capture.stdout, b"l 0\n",
            "a link to a directory must keep the list protocol's link type"
        );
        let capture = local_probe(&["stat", fifo.to_str().unwrap()]);
        assert_eq!(
            capture.stdout, b"f 0\n",
            "a FIFO must count as occupied without being opened for a size read"
        );
        assert_eq!(
            local_probe(&["stat", tmp.path().join("missing").to_str().unwrap()]).code,
            Some(3)
        );

        // The Rust parser agrees with the wire format.
        let stat = parse_stat_output(format!("f {}\n", content.len()).as_bytes()).unwrap();
        assert_eq!(
            stat,
            RemoteStat {
                is_dir: false,
                size: content.len() as u64
            }
        );
        assert_eq!(
            parse_stat_output(b"d 0\n").unwrap(),
            RemoteStat {
                is_dir: true,
                size: 0
            }
        );
        // wc padding on some platforms is absorbed.
        assert_eq!(parse_stat_output(b"f   42\n").unwrap().size, 42);
        assert!(parse_stat_output(b"x 1\n").is_err());
        assert!(parse_stat_output(b"f\n").is_err());
        assert!(parse_stat_output(b"").is_err());
    }

    #[test]
    fn payload_mode_argv_matches_the_delivery_contract() {
        // ssh: the whole `sh -c '<script>' -- op args` stays ONE element.
        let host = ssh_host();
        let argv = ssh_probe_argv(
            &host,
            "put",
            &[OsStr::new("/dst/a b'c")],
            ScriptDelivery::Argv,
        );
        let command = argv.last().expect("command element").to_string_lossy();
        assert!(command.starts_with("sh -c '"));
        assert!(command.contains("remote-fs probe v3"));
        assert!(command.ends_with(" -- 'put' '/dst/a b'\\''c'"));

        // docker: script as one raw argv element, no quoting anywhere.
        let host = docker_host();
        let argv = docker_probe_argv(
            &host,
            "untar",
            &[OsStr::new("/dst"), OsStr::new("tree")],
            ScriptDelivery::Argv,
        );
        let text: Vec<_> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = text.iter().position(|a| a == "-c").expect("-c present");
        assert_eq!(text[pos + 1], PROBE_SCRIPT);
        assert_eq!(text[pos + 2..], ["--", "untar", "/dst", "tree"]);
    }

    // -- streaming transfer mechanics ----------------------------------------

    #[test]
    fn stream_staging_refuses_a_symlink_before_spawning() {
        use std::os::unix::fs::PermissionsExt;

        let local = TestDir::new("staging-symlink");
        let victim = local.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let staging = local.path().join("staging");
        std::os::unix::fs::symlink(&victim, &staging).unwrap();
        let spawned = AtomicBool::new(false);

        let error = match reserve_part_then_spawn(&staging, || {
            spawned.store(true, Ordering::Relaxed);
            Err(io::Error::other("producer should not start"))
        }) {
            Ok(_) => panic!("an occupied staging name must be refused"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(!spawned.load(Ordering::Relaxed));
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(std::fs::symlink_metadata(&staging)
            .unwrap()
            .file_type()
            .is_symlink());

        let safe_staging = local.path().join("safe-staging");
        let file = open_transfer_staging(&safe_staging).unwrap();
        drop(file);
        assert_eq!(
            std::fs::metadata(&safe_staging)
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0,
            "partial transfer content must remain owner-only"
        );
    }

    #[test]
    fn unique_transfer_staging_skips_aliases_and_planted_symlinks() {
        use std::os::unix::fs::PermissionsExt;

        let local = TestDir::new("unique-staging");
        let pid = std::process::id();

        let anchor = local.path().join(format!(".anvil-fs-part-{pid}-7"));
        let mut sequence = [7, 8].into_iter();
        let (staging, file) =
            StagedFile::beside_with(&anchor, || sequence.next().expect("one retry is enough"))
                .unwrap();
        let staging_path = staging.path().to_path_buf();
        assert_ne!(staging_path, anchor);
        assert_eq!(
            std::fs::metadata(&staging_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0,
            "reserved staging must remain owner-only"
        );
        drop(file);
        drop(staging);
        assert!(!anchor.exists());
        assert!(!staging_path.exists());

        let victim = local.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let planted = local.path().join(format!(".anvil-fs-part-{pid}-11"));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        let ordinary_anchor = local.path().join("download.bin");
        let mut sequence = [11, 12].into_iter();
        let (staging, file) = StagedFile::beside_with(&ordinary_anchor, || {
            sequence.next().expect("one retry is enough")
        })
        .unwrap();
        let expected_name = format!(".anvil-fs-part-{pid}-12");
        assert_eq!(staging.path().file_name(), Some(OsStr::new(&expected_name)));
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink());
        drop(file);
        drop(staging);
        assert!(std::fs::symlink_metadata(&planted)
            .unwrap()
            .file_type()
            .is_symlink());

        let replacement_anchor = local.path().join("replacement.bin");
        let (staging, file) = StagedFile::beside(&replacement_anchor).unwrap();
        let staging_path = staging.path().to_path_buf();
        std::fs::remove_file(&staging_path).unwrap();
        std::fs::write(&staging_path, b"replacement").unwrap();
        drop(file);
        drop(staging);
        assert_eq!(std::fs::read(&staging_path).unwrap(), b"replacement");

        let failed = TestDir::new("staging-spawn-failure");
        let error = download_file_with(
            || Err(io::Error::other("injected spawn failure")),
            OsStr::new("download.bin"),
            failed.path(),
            1 << 20,
            &TransferControl::new(),
            &|_| {},
        )
        .expect_err("a producer spawn failure must be returned");
        assert_eq!(error.to_string(), "injected spawn failure");
        assert_eq!(
            std::fs::read_dir(failed.path()).unwrap().count(),
            0,
            "a failed spawn leaves no reserved staging file"
        );
    }

    #[test]
    fn download_streams_file_content_and_refuses_existing_dst() {
        let remote = TestDir::new("download-src");
        let local = TestDir::new("download-dst");
        let content = binary_content(11, 50_000);
        let src = remote.path().join("file.bin");
        std::fs::write(&src, &content).unwrap();
        let src_str = src.to_str().unwrap().to_string();

        let dst = download_file_with(
            || spawn_local(&["cat", &src_str], ScriptDelivery::Stdin),
            OsStr::new("file.bin"),
            local.path(),
            1 << 20,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap();
        assert_eq!(dst, local.path().join("file.bin"));
        assert_eq!(std::fs::read(&dst).unwrap(), content);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dst).unwrap().permissions().mode() & 0o077,
            0,
            "published downloads retain their private staging mode"
        );
        assert_eq!(
            std::fs::read_dir(local.path()).unwrap().count(),
            1,
            "no temp litter after a clean download"
        );

        let err = download_file_with(
            || spawn_local(&["cat", &src_str], ScriptDelivery::Stdin),
            OsStr::new("file.bin"),
            local.path(),
            1 << 20,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn download_accepts_a_name_at_the_component_limit() {
        let remote = TestDir::new("download-long-src");
        let local = TestDir::new("download-long-dst");
        let content = binary_content(13, 4_000);
        let src = remote.path().join("source.bin");
        std::fs::write(&src, &content).unwrap();
        let src_str = src.to_str().unwrap().to_string();
        let name = "x".repeat(255);

        let dst = download_file_with(
            || spawn_local(&["cat", &src_str], ScriptDelivery::Stdin),
            OsStr::new(&name),
            local.path(),
            1 << 20,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), content);
        assert_eq!(dst.file_name().unwrap().as_encoded_bytes().len(), 255);
        assert_eq!(
            std::fs::read_dir(local.path()).unwrap().count(),
            1,
            "fixed-size staging names leave no litter"
        );
    }

    #[test]
    fn download_cap_overflow_kills_and_cleans_up() {
        let remote = TestDir::new("download-cap-src");
        let local = TestDir::new("download-cap-dst");
        let src = remote.path().join("big.bin");
        std::fs::write(&src, vec![b'x'; 10_000]).unwrap();
        let src_str = src.to_str().unwrap().to_string();

        let err = download_file_with(
            || spawn_local(&["cat", &src_str], ScriptDelivery::Stdin),
            OsStr::new("big.bin"),
            local.path(),
            16,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("limit"));
        assert_eq!(
            std::fs::read_dir(local.path()).unwrap().count(),
            0,
            "no partial or temp file remains after an overflow abort"
        );
    }

    #[test]
    fn download_missing_source_is_not_found() {
        let remote = TestDir::new("download-404-src");
        let local = TestDir::new("download-404-dst");
        let missing = remote.path().join("missing").to_str().unwrap().to_string();
        let err = download_file_with(
            || spawn_local(&["cat", &missing], ScriptDelivery::Stdin),
            OsStr::new("missing"),
            local.path(),
            1 << 20,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn upload_streams_content_and_maps_put_exit_17() {
        let local = TestDir::new("upload-src");
        let remote = TestDir::new("upload-dst");
        let content = binary_content(5, 30_000);
        let src = local.path().join("up.bin");
        std::fs::write(&src, &content).unwrap();
        let dst = remote.path().join("up.bin");
        let dst_str = dst.to_str().unwrap().to_string();

        upload_file_with(
            || spawn_local(&["put", &dst_str], ScriptDelivery::Argv),
            &src,
            1 << 20,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), content);

        let err = upload_file_with(
            || spawn_local(&["put", &dst_str], ScriptDelivery::Argv),
            &src,
            1 << 20,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn upload_rejects_oversize_before_streaming() {
        let local = TestDir::new("upload-cap-src");
        let remote = TestDir::new("upload-cap-dst");
        let src = local.path().join("big.bin");
        std::fs::write(&src, vec![b'y'; 1_000]).unwrap();
        let dst = remote.path().join("big.bin");
        let dst_str = dst.to_str().unwrap().to_string();
        let err = upload_file_with(
            || spawn_local(&["put", &dst_str], ScriptDelivery::Argv),
            &src,
            16,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("limit"));
        assert!(!dst.exists(), "nothing was streamed for an oversize file");
    }

    #[test]
    fn directory_upload_download_round_trip() {
        let local_src = TestDir::new("dir-src");
        let relay = TestDir::new("dir-relay");
        let local_dst = TestDir::new("dir-dst");
        let src = local_src.path().join("tree");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let content = binary_content(9, 4_000);
        std::fs::write(src.join("sub").join("blob.bin"), &content).unwrap();
        std::fs::write(src.join("readme"), b"hi").unwrap();

        // "upload": local tar → the untar probe writing into the relay dir.
        let relay_str = relay.path().to_str().unwrap().to_string();
        upload_dir_with(
            || spawn_local(&["untar", &relay_str, "tree"], ScriptDelivery::Argv),
            &src,
            1 << 24,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::read(relay.path().join("tree").join("sub").join("blob.bin")).unwrap(),
            content
        );

        // "download": the tar probe → local tar extracting into the dst dir.
        let staged = relay.path().join("tree");
        let staged_str = staged.to_str().unwrap().to_string();
        let dst = download_dir_with(
            || spawn_local(&["tar", &staged_str], ScriptDelivery::Stdin),
            OsStr::new("tree"),
            local_dst.path(),
            1 << 24,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap();
        assert_eq!(
            std::fs::read(dst.join("sub").join("blob.bin")).unwrap(),
            content
        );
        assert_eq!(std::fs::read(dst.join("readme")).unwrap(), b"hi");
        assert_eq!(
            std::fs::read_dir(local_dst.path()).unwrap().count(),
            1,
            "a successful download leaves only the published directory"
        );
    }

    #[test]
    fn directory_download_validates_then_publishes_without_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let remote = TestDir::new("dir-transaction-src");
        let local = TestDir::new("dir-transaction-dst");
        let dst = local.path().join("tree");

        let staging_path = {
            let staging = ExtractionDir::beside(&dst).unwrap();
            let path = staging.path().to_path_buf();
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
                0,
                "archive staging must remain owner-only"
            );
            path
        };
        assert!(!staging_path.exists(), "private staging cleans up on drop");

        // A successful tar command with the wrong root is still invalid input:
        // none of its content may escape staging or become the destination.
        let wrong = remote.path().join("wrong-root");
        std::fs::create_dir(&wrong).unwrap();
        std::fs::write(wrong.join("payload"), b"wrong").unwrap();
        let wrong_str = wrong.to_str().unwrap().to_string();
        let error = download_dir_with(
            || spawn_local(&["tar", &wrong_str], ScriptDelivery::Stdin),
            OsStr::new("tree"),
            local.path(),
            1 << 24,
            &TransferControl::new(),
            &|_| {},
        )
        .expect_err("an unexpected archive root must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_dir(local.path()).unwrap().count(), 0);

        // Make a valid archive, but have its producer create the final
        // destination immediately before streaming. Atomic publication must
        // leave that racing directory intact and discard the private tree.
        let source = remote.path().join("tree");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("downloaded"), b"payload").unwrap();
        let source_str = source.to_str().unwrap().to_string();
        let capture = local_probe(&["tar", &source_str]);
        assert_eq!(capture.code, Some(0));
        let archive = remote.path().join("tree.tar");
        std::fs::write(&archive, capture.stdout).unwrap();
        let dst_arg = dst.as_os_str().to_os_string();
        let archive_arg = archive.as_os_str().to_os_string();

        let error = download_dir_with(
            || {
                let argv = vec![
                    OsString::from("sh"),
                    OsString::from("-c"),
                    OsString::from("mkdir \"$1\" && printf keep > \"$1/marker\" && cat \"$2\""),
                    OsString::from("--"),
                    dst_arg,
                    archive_arg,
                ];
                spawn_probe_argv(&argv, ScriptDelivery::Argv)
            },
            OsStr::new("tree"),
            local.path(),
            1 << 24,
            &TransferControl::new(),
            &|_| {},
        )
        .expect_err("the destination created during streaming must win");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(dst.join("marker")).unwrap(), b"keep");
        assert!(
            !dst.join("downloaded").exists(),
            "archive content must not merge into the racing destination"
        );
        let entries: Vec<_> = std::fs::read_dir(local.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, [OsString::from("tree")]);
    }

    #[test]
    fn staging_dir_is_removed_on_drop() {
        let path = {
            let staging = StagingDir::new().expect("staging dir");
            let path = staging.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists());
    }

    #[test]
    fn transfer_rejects_local_to_local() {
        let err = transfer(
            &[],
            &FsLocation::Local,
            Path::new("/a"),
            &FsLocation::Local,
            Path::new("/b"),
            false,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    // -- progress, cancellation, cleanup --------------------------------------

    #[test]
    fn human_bytes_formats_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(9 * 1024), "9.0 KiB");
        assert_eq!(human_bytes(10 * 1024), "10 KiB");
        assert_eq!(human_bytes(13_000_000), "12 MiB");
        assert_eq!(human_bytes(1 << 30), "1.0 GiB");
        assert_eq!(human_bytes(5 << 30), "5.0 GiB");
    }

    #[test]
    fn progress_throttle_gates_on_size_and_time() {
        // Below the 256 KiB delta: no emission even with time elapsed.
        let mut throttle = ProgressThrottle::aged(Duration::from_secs(1));
        assert_eq!(throttle.update(100 * 1024), None);
        // Crossing 256 KiB with the interval satisfied: emit the total.
        assert_eq!(throttle.update(200 * 1024), Some(300 * 1024));
        // Right after an emission the time gate blocks the next one.
        assert_eq!(throttle.update(512 * 1024), None);
        assert_eq!(throttle.total(), 812 * 1024);

        // A fresh throttle is inside the 250 ms interval: even a large first
        // chunk waits for the interval, keeping emissions at ~4 per second.
        let mut fresh = ProgressThrottle::new();
        assert_eq!(fresh.update(4 * 1024 * 1024), None);
    }

    #[test]
    fn progress_throttle_emits_after_the_interval() {
        let mut throttle = ProgressThrottle::new();
        assert_eq!(throttle.update(PROGRESS_MIN_DELTA_BYTES), None);
        std::thread::sleep(PROGRESS_MIN_INTERVAL + Duration::from_millis(60));
        assert_eq!(
            throttle.update(PROGRESS_MIN_DELTA_BYTES),
            Some(2 * PROGRESS_MIN_DELTA_BYTES)
        );
    }

    #[test]
    fn transfer_progress_reaches_the_exact_file_size() {
        let local = TestDir::new("progress-src");
        let remote = TestDir::new("progress-dst");
        let content = binary_content(2, 30_000);
        let src = local.path().join("up.bin");
        std::fs::write(&src, &content).unwrap();
        let dst = remote.path().join("up.bin");
        let dst_str = dst.to_str().unwrap().to_string();

        let events = std::sync::Mutex::new(Vec::new());
        upload_file_with(
            || spawn_local(&["put", &dst_str], ScriptDelivery::Argv),
            &src,
            1 << 20,
            &TransferControl::new(),
            &|bytes| events.lock().unwrap().push(bytes),
        )
        .unwrap();
        let events = events.into_inner().unwrap();
        assert!(
            !events.is_empty(),
            "the pump emits at least the final total"
        );
        assert_eq!(
            events.last().copied(),
            Some(content.len() as u64),
            "the final progress value is the exact payload size"
        );
        assert!(
            events.windows(2).all(|pair| pair[0] <= pair[1]),
            "progress is monotonic"
        );
    }

    #[test]
    fn cancel_kills_registered_children_and_flags_interrupted() {
        let control = TransferControl::new();
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let child = Arc::new(Mutex::new(child));
        control.register(&child);
        assert!(!control.is_cancelled());
        control.cancel();
        assert!(control.is_cancelled());
        assert_eq!(
            control.check().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        let status = wait_child(&child).unwrap();
        assert!(!status.success(), "the child was killed, not exited");
        // Racing a finished transfer: a second cancel is a harmless no-op.
        control.cancel();
        assert_eq!(
            control.check().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn cancel_before_start_prevents_the_spawn() {
        let local = TestDir::new("cancel-early");
        let control = TransferControl::new();
        control.cancel();
        let spawned = std::sync::atomic::AtomicBool::new(false);
        let err = download_file_with(
            || {
                spawned.store(true, Ordering::Relaxed);
                spawn_local(&["cat", "/dev/null"], ScriptDelivery::Stdin)
            },
            OsStr::new("null.bin"),
            local.path(),
            1 << 20,
            &control,
            &|_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(!spawned.load(Ordering::Relaxed), "no spawn after a cancel");
        assert_eq!(std::fs::read_dir(local.path()).unwrap().count(), 0);
    }

    #[test]
    fn cancel_during_download_cleans_up_and_is_not_an_error() {
        let local = TestDir::new("cancel-dl");
        let local_path = local.path().to_path_buf();
        let control = TransferControl::new();
        let worker_control = control.clone();
        let handle = std::thread::spawn(move || {
            // `yes` streams forever: the transfer is definitely in flight
            // when the cancel lands.
            download_file_with(
                || spawn_local_shell("yes transfer-payload"),
                OsStr::new("yes.bin"),
                &local_path,
                1 << 30,
                &worker_control,
                &|_| {},
            )
        });
        std::thread::sleep(Duration::from_millis(150));
        control.cancel();
        let err = handle.join().expect("worker finished").unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::Interrupted,
            "a cancel reports Interrupted, not a transfer failure"
        );
        assert_eq!(
            std::fs::read_dir(local.path()).unwrap().count(),
            0,
            "the partial temp file is cleaned up"
        );
    }

    /// Spawn any local shell command as a "remote" for cancel tests.
    fn spawn_local_shell(command: &str) -> io::Result<ProbeChild> {
        let argv = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from(command),
        ];
        spawn_probe_argv(&argv, ScriptDelivery::Argv)
    }

    #[test]
    fn part_cleanup_command_globs_only_the_fixed_suffix() {
        assert_eq!(
            part_cleanup_command(Path::new("/dst/dir name")),
            "rm -f '/dst/dir name'.fspart.*"
        );
        assert_eq!(
            part_cleanup_command(Path::new("/dst/don't")),
            "rm -f '/dst/don'\\''t'.fspart.*"
        );
    }

    #[test]
    fn part_cleanup_command_executes_against_local_sh() {
        let tmp = TestDir::new("part-cleanup");
        let dst = tmp.path().join("up.bin");
        std::fs::write(tmp.path().join("up.bin.fspart.111"), b"partial").unwrap();
        std::fs::write(tmp.path().join("up.bin.fspart.222"), b"partial").unwrap();
        std::fs::write(tmp.path().join("up.bin"), b"done").unwrap();
        std::fs::write(tmp.path().join("other.txt"), b"keep").unwrap();

        let command = part_cleanup_command(&dst);
        let argv = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from(command),
        ];
        let capture = run_capture(&argv, &[], Duration::from_secs(10), MAX_CAPTURE_BYTES).unwrap();
        assert_eq!(capture.code, Some(0));
        assert!(dst.is_file(), "the destination itself survives");
        assert!(tmp.path().join("other.txt").is_file());
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains("fspart"))
            .collect();
        assert!(leftovers.is_empty(), "the .fspart temps are gone");
    }

    // -- drag-and-drop import planning ----------------------------------------

    #[test]
    fn plan_drop_dispatches_copy_local_and_upload_remote() {
        let tmp = TestDir::new("plan-drop");
        let file = tmp.path().join("file.txt");
        std::fs::write(&file, b"data").unwrap();
        let dir = tmp.path().join("dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("inner.bin"), b"123456").unwrap();

        // Local destination: Copy actions, collision flags from the dst dir.
        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("file.txt"), b"old").unwrap();
        let plan = plan_drop(&[file.clone(), dir.clone()], &FsLocation::Local, &dst).unwrap();
        assert_eq!(plan.items.len(), 2);
        assert!(plan
            .items
            .iter()
            .all(|item| item.action == DropAction::Copy));
        assert!(plan.items[0].collides, "file.txt exists in dst");
        assert!(!plan.items[1].collides);
        assert!(plan.items[1].is_dir);
        assert_eq!(plan.total_bytes, 4 + 6);

        // Remote destination: Upload actions, collisions deferred to the probe.
        let plan = plan_drop(
            std::slice::from_ref(&file),
            &FsLocation::Remote(0),
            Path::new("/remote/dst"),
        )
        .unwrap();
        assert_eq!(plan.items[0].action, DropAction::Upload);
        assert!(!plan.items[0].collides);
    }

    #[test]
    fn plan_drop_refusals_are_wholesale_and_ordered() {
        let tmp = TestDir::new("plan-drop-refuse");
        let file = tmp.path().join("f");
        std::fs::write(&file, b"x").unwrap();

        assert_eq!(
            plan_drop(&[], &FsLocation::Local, tmp.path()),
            Err(DropRejection::Empty)
        );
        // Item-count cap fires before any filesystem reads.
        let many: Vec<PathBuf> = (0..=MAX_DROP_ITEMS)
            .map(|i| tmp.path().join(format!("nope-{i}")))
            .collect();
        assert_eq!(
            plan_drop(&many, &FsLocation::Local, tmp.path()),
            Err(DropRejection::TooManyItems(MAX_DROP_ITEMS + 1))
        );
        assert_eq!(
            plan_drop(
                &[PathBuf::from("relative/path")],
                &FsLocation::Local,
                tmp.path()
            ),
            Err(DropRejection::NotAbsolute(PathBuf::from("relative/path")))
        );
        assert_eq!(
            plan_drop(
                &[tmp.path().join("missing")],
                &FsLocation::Local,
                tmp.path()
            ),
            Err(DropRejection::Unreadable(tmp.path().join("missing")))
        );
        // Total-cap enforcement against a tiny test limit.
        let big = tmp.path().join("big");
        std::fs::write(&big, vec![b'x'; 100]).unwrap();
        assert!(matches!(
            plan_drop_with_limit(&[file, big], &FsLocation::Local, tmp.path(), 50),
            Err(DropRejection::TooLarge(101))
        ));
    }

    #[test]
    fn drop_size_walk_bounds_depth_and_never_follows_symlinks() {
        let tmp = TestDir::new("plan-drop-walk");
        // A 70-deep chain with a file at the bottom: beyond depth 64 it
        // contributes nothing.
        let mut deep = tmp.path().to_path_buf();
        for level in 0..70 {
            deep = deep.join(format!("d{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("bottom.txt"), b"deep").unwrap();

        let shallow = tmp.path().join("shallow");
        std::fs::create_dir(&shallow).unwrap();
        std::fs::write(shallow.join("a.txt"), b"1234").unwrap();

        // Symlinks are never followed: the linked dir's file counts nothing.
        std::os::unix::fs::symlink(&shallow, tmp.path().join("dirlink")).unwrap();
        std::os::unix::fs::symlink(shallow.join("a.txt"), tmp.path().join("filelink")).unwrap();

        let top = plan_drop(&[tmp.path().to_path_buf()], &FsLocation::Local, tmp.path())
            .unwrap()
            .total_bytes;
        assert_eq!(top, 4, "only the shallow real file counts");

        assert_eq!(drop_item_size(&tmp.path().join("dirlink"), 0).unwrap(), 0);
        assert_eq!(drop_item_size(&tmp.path().join("filelink"), 0).unwrap(), 0);
        assert_eq!(
            drop_item_size(&tmp.path().join("shallow"), 0).unwrap(),
            4,
            "a directly named directory is still walked"
        );
    }

    #[test]
    fn run_drop_copies_items_and_collects_failures() {
        let tmp = TestDir::new("run-drop");
        let src_file = tmp.path().join("note.txt");
        std::fs::write(&src_file, b"hello").unwrap();
        let src_dir = tmp.path().join("tree");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("inner"), b"42").unwrap();
        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("note.txt"), b"old").unwrap();

        let plan = plan_drop(&[src_file, src_dir], &FsLocation::Local, &dst).unwrap();
        let events = std::sync::Mutex::new(Vec::new());
        let outcome = run_drop(
            &plan,
            &FsLocation::Local,
            &[],
            &dst,
            &TransferControl::new(),
            &|bytes| events.lock().unwrap().push(bytes),
        )
        .unwrap();
        assert_eq!(outcome.done, 1, "the directory copied");
        assert_eq!(outcome.failed.len(), 1, "the colliding file failed");
        assert!(outcome.failed[0].1.contains("already exists"));
        assert_eq!(
            std::fs::read(dst.join("tree").join("inner")).unwrap(),
            b"42"
        );
        assert_eq!(
            std::fs::read(dst.join("note.txt")).unwrap(),
            b"old",
            "no overwrite"
        );
        let events = events.into_inner().unwrap();
        assert!(
            events.windows(2).all(|pair| pair[0] <= pair[1]),
            "progress never dips"
        );
    }

    #[test]
    fn run_drop_honors_a_pre_cancelled_control() {
        let tmp = TestDir::new("run-drop-cancel");
        let src = tmp.path().join("f.txt");
        std::fs::write(&src, b"data").unwrap();
        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        let plan = plan_drop(&[src], &FsLocation::Local, &dst).unwrap();

        let control = TransferControl::new();
        control.cancel();
        let err = run_drop(&plan, &FsLocation::Local, &[], &dst, &control, &|_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            std::fs::read_dir(&dst).unwrap().count(),
            0,
            "a cancelled batch copies nothing"
        );
    }

    // -- batch clipboard operations -------------------------------------------

    fn clip_item(path: &Path, is_dir: bool) -> FsClipboardItem {
        FsClipboardItem {
            path: path.to_path_buf(),
            is_dir,
        }
    }

    #[test]
    fn paste_all_copy_continues_past_collisions() {
        let tmp = TestDir::new("paste-copy");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, b"aaa").unwrap();
        std::fs::write(&b, b"bbb").unwrap();
        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("a.txt"), b"old").unwrap();

        let items = vec![clip_item(&a, false), clip_item(&b, false)];
        let outcome = paste_all(
            &[],
            &FsLocation::Local,
            &items,
            &FsLocation::Local,
            &dst,
            false,
            &TransferControl::new(),
            &|_| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(outcome.done, 1, "b.txt copied");
        assert_eq!(outcome.failed.len(), 1, "a.txt collided");
        assert!(outcome.failed[0].0.contains("a.txt"));
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read(dst.join("b.txt")).unwrap(), b"bbb");
    }

    #[test]
    fn paste_all_cut_removes_only_successful_sources() {
        let tmp = TestDir::new("paste-cut");
        let ok = tmp.path().join("ok.txt");
        let blocked = tmp.path().join("blocked.txt");
        std::fs::write(&ok, b"ok").unwrap();
        std::fs::write(&blocked, b"blocked").unwrap();
        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("blocked.txt"), b"existing").unwrap();

        let items = vec![clip_item(&ok, false), clip_item(&blocked, false)];
        let consumed = std::cell::RefCell::new(Vec::new());
        let outcome = paste_all(
            &[],
            &FsLocation::Local,
            &items,
            &FsLocation::Local,
            &dst,
            true,
            &TransferControl::new(),
            &|_| {},
            &|path| consumed.borrow_mut().push(path.to_path_buf()),
        )
        .unwrap();
        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.failed.len(), 1);
        assert!(!ok.exists(), "the moved source is gone");
        assert!(blocked.exists(), "the colliding source stays put");
        assert_eq!(std::fs::read(dst.join("ok.txt")).unwrap(), b"ok");
        assert_eq!(*consumed.borrow(), std::slice::from_ref(&ok));

        let mut clipboard = Some(FsClipboard {
            loc: FsLocation::Local,
            items,
            cut: true,
            token: 41,
        });
        assert!(retire_clipboard_sources(
            &mut clipboard,
            Some(41),
            &consumed.borrow(),
        ));
        let remaining = clipboard.expect("failed cut item remains retryable");
        assert_eq!(remaining.items.len(), 1);
        assert_eq!(remaining.items[0].path, blocked);
    }

    #[test]
    fn paste_all_cut_all_failures_preserve_the_clipboard() {
        let tmp = TestDir::new("paste-cut-all-fail");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("a.txt"), b"existing").unwrap();
        std::fs::write(dst.join("b.txt"), b"existing").unwrap();
        let items = vec![clip_item(&a, false), clip_item(&b, false)];
        let consumed = std::cell::RefCell::new(Vec::new());

        let outcome = paste_all(
            &[],
            &FsLocation::Local,
            &items,
            &FsLocation::Local,
            &dst,
            true,
            &TransferControl::new(),
            &|_| {},
            &|path| consumed.borrow_mut().push(path.to_path_buf()),
        )
        .unwrap();
        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.failed.len(), 2);
        assert!(consumed.borrow().is_empty());

        let mut clipboard = Some(FsClipboard {
            loc: FsLocation::Local,
            items: items.clone(),
            cut: true,
            token: 42,
        });
        assert!(!retire_clipboard_sources(
            &mut clipboard,
            Some(42),
            &consumed.borrow(),
        ));
        assert_eq!(clipboard.unwrap().items, items);
    }

    #[test]
    fn cancelled_batch_cut_settles_only_its_completed_prefix_and_not_a_newer_intent() {
        let tmp = TestDir::new("paste-cut-cancel-prefix");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let dst = tmp.path().join("dst");
        std::fs::create_dir(&dst).unwrap();
        let items = vec![clip_item(&a, false), clip_item(&b, false)];
        let control = TransferControl::new();
        let cancel = control.clone();
        let consumed = std::cell::RefCell::new(Vec::new());

        let error = paste_all(
            &[],
            &FsLocation::Local,
            &items,
            &FsLocation::Local,
            &dst,
            true,
            &control,
            &|done| {
                if done == 1 {
                    cancel.cancel();
                }
            },
            &|path| consumed.borrow_mut().push(path.to_path_buf()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(*consumed.borrow(), std::slice::from_ref(&a));
        assert!(!a.exists() && b.exists());

        let mut original = Some(FsClipboard {
            loc: FsLocation::Local,
            items: items.clone(),
            cut: true,
            token: 51,
        });
        assert!(retire_clipboard_sources(
            &mut original,
            Some(51),
            &consumed.borrow(),
        ));
        let remaining = original.expect("unfinished suffix remains");
        assert_eq!(remaining.items, [clip_item(&b, false)]);

        let mut newer = Some(FsClipboard {
            loc: FsLocation::Local,
            items: items.clone(),
            cut: true,
            token: 52,
        });
        assert!(!retire_clipboard_sources(
            &mut newer,
            Some(51),
            &consumed.borrow(),
        ));
        assert_eq!(newer.unwrap().items, items);
    }

    #[test]
    fn paste_all_refuses_same_directory_self_paste_per_item() {
        let tmp = TestDir::new("paste-self");
        let a = tmp.path().join("a.txt");
        std::fs::write(&a, b"aaa").unwrap();
        let items = vec![clip_item(&a, false)];
        let outcome = paste_all(
            &[],
            &FsLocation::Local,
            &items,
            &FsLocation::Local,
            tmp.path(),
            false,
            &TransferControl::new(),
            &|_| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.failed.len(), 1);
        assert!(a.exists());
    }

    #[test]
    fn delete_all_continues_past_failures_and_counts() {
        let tmp = TestDir::new("delete-all");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"2").unwrap();
        let missing = tmp.path().join("missing.txt");

        let outcome = delete_all(
            &FsLocation::Local,
            &[],
            &[a.clone(), missing.clone(), b.clone()],
        );
        assert_eq!(outcome.summary.done, 2);
        assert_eq!(outcome.summary.failed.len(), 1);
        assert!(outcome.summary.failed[0].0.contains("missing.txt"));
        assert_eq!(outcome.succeeded, [a.clone(), b.clone()]);
        assert!(!outcome.succeeded.contains(&missing));
        assert!(!a.exists() && !b.exists());

        // The filesystem root survives a batch delete.
        let outcome = delete_all(
            &FsLocation::Local,
            &[],
            std::slice::from_ref(&PathBuf::from("/")),
        );
        assert_eq!(outcome.summary.done, 0);
        assert_eq!(outcome.summary.failed.len(), 1);
        assert!(outcome.succeeded.is_empty());
        assert!(Path::new("/").is_dir());
    }

    #[test]
    fn production_probe_gate_rejects_invalid_runtime_hosts_before_spawn() {
        let mut host = ssh_host();
        host.host = "-oProxyCommand=attacker".to_string();
        let error = checked_probe_argv(&host, "home", &[], ScriptDelivery::Stdin).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let hosts = vec![ssh_host(); crate::config::MAX_REMOTE_HOSTS + 1];
        let error =
            remote_host(&FsLocation::Remote(crate::config::MAX_REMOTE_HOSTS), &hosts).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
