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
# `stat` prints "<t> <size>" (size 0 for d/l).
# Exit codes: 0 ok, 2 usage/bad path, 3 cannot enter dir, 4 op failed, 17 target exists.
set -u
op=${1:-}
case "$op" in
  home)
    cd 2>/dev/null || cd / || exit 3
    pwd
    ;;
  list)
    d=${2:-}
    case "$d" in /*) ;; *) exit 2 ;; esac
    cd "$d" 2>/dev/null || exit 3
    for f in * .[!.]* ..?*; do
      if [ -d "$f" ]; then t=d
      elif [ -L "$f" ]; then t=l
      elif [ -e "$f" ]; then t=f
      else continue
      fi
      printf '%s\0%s\0' "$t" "$f"
    done
    ;;
  mkdir)
    p=${2:-}
    case "$p" in /*) ;; *) exit 2 ;; esac
    [ -e "$p" ] && exit 17
    mkdir "$p" || exit 4
    ;;
  mkfile)
    p=${2:-}
    case "$p" in /*) ;; *) exit 2 ;; esac
    [ -e "$p" ] && exit 17
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
    [ -e "$n" ] && exit 17
    mv "$s" "$n" || exit 4
    ;;
  cp)
    s=${2:-}; n=${3:-}
    case "$s" in /*) ;; *) exit 2 ;; esac
    case "$n" in /*) ;; *) exit 2 ;; esac
    [ -e "$n" ] && exit 17
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
    [ -e "$p" ] && exit 17
    t="$p.fspart.$$"
    if cat > "$t"; then
      [ -e "$p" ] && { rm -f "$t"; exit 17; }
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
    if [ -d "$p" ]; then t=d; s=0
    elif [ -L "$p" ]; then t=l; s=0
    elif [ -f "$p" ]; then
      t=f
      s=$(wc -c < "$p") || exit 4
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
/// Transfers get a generous overall cap: ssh's own ConnectTimeout still
/// bounds the handshake, and this watchdog ends any transfer — busy or idle —
/// after 15 minutes, so a stuck connection can never wedge a worker thread.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// One directory holds at most MAX_DIRECTORY_ENTRIES shown entries of at most
/// 255 bytes; 2 MiB caps the capture without cutting a legitimate listing.
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
/// Payloads (files and directory tars) never exceed half a gigabyte.
pub(crate) const MAX_TRANSFER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TRANSFER_STDERR_BYTES: usize = 64 * 1024;
const STREAM_BUF_SIZE: usize = 64 * 1024;
const MAX_ERROR_DISPLAY_BYTES: usize = 512;
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Which filesystem the file tree browses. `Remote(i)` indexes
/// `config.remote_hosts`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum FsLocation {
    Local,
    Remote(usize),
}

impl FsLocation {
    /// Selector label: "Local", or the host name prefixed by its scheme.
    pub(crate) fn label(&self, hosts: &[RemoteHost]) -> String {
        match self {
            FsLocation::Local => "Local".to_string(),
            FsLocation::Remote(index) => crate::config::checked_remote_host(hosts, *index)
                .map(location_label)
                .unwrap_or_else(|_| "Remote (unavailable)".to_string()),
        }
    }
}

fn location_label(host: &RemoteHost) -> String {
    let scheme = if host.docker { "docker" } else { "ssh" };
    let name = jterm_core::review_input::safe_inline_display(&host.name, 256);
    format!("{scheme}: {name}")
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
}

/// One finished probe run, output bounded on both streams.
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
) -> io::Result<Vec<FileEntry>> {
    match loc {
        FsLocation::Local => crate::file_tree::scan_dir(dir),
        FsLocation::Remote(_) => {
            require_absolute(dir)?;
            let host = remote_host(loc, hosts)?;
            let stdout = run_probe(host, "list", &[dir.as_os_str()], PROBE_LIST_TIMEOUT)?;
            Ok(parse_list_output(&stdout, dir))
        }
    }
}

/// Where a fresh tree starts: `$HOME` locally (falling back to `/`), the
/// remote account's home directory over the probe otherwise.
pub(crate) fn start_dir(loc: &FsLocation, hosts: &[RemoteHost]) -> io::Result<PathBuf> {
    match loc {
        FsLocation::Local => Ok(crate::file_tree::home_dir().unwrap_or_else(|| PathBuf::from("/"))),
        FsLocation::Remote(_) => {
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
        FsLocation::Remote(_) => {
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
        FsLocation::Remote(_) => {
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
        FsLocation::Remote(_) => {
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
        FsLocation::Local => {
            require_missing(dst)?;
            std::fs::rename(src, dst)
        }
        FsLocation::Remote(_) => {
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
            copy_recursive(src, dst)
        }
        FsLocation::Remote(_) => {
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

fn remote_host<'a>(loc: &FsLocation, hosts: &'a [RemoteHost]) -> io::Result<&'a RemoteHost> {
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
/// anything for what the Rust side already knows is nonsense.
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

/// Recursive copy mirroring `cp -a` semantics closely enough for the tree:
/// directory structure and file contents are preserved, symlinks are copied
/// as links rather than followed.
fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(src)?;
    if metadata.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            copy_recursive(&src.join(&name), &dst.join(&name))?;
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
/// names relative to `dir`. Symlinks are files here — never expanded into
/// directories, matching `file_tree::scan_dir`.
fn parse_list_output(bytes: &[u8], dir: &Path) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    let mut fields = bytes.split(|&byte| byte == 0);
    while let (Some(kind), Some(name)) = (fields.next(), fields.next()) {
        if entries.len() >= crate::file_tree::MAX_DIRECTORY_ENTRIES {
            break;
        }
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
        let name = OsStr::from_bytes(name);
        entries.push(FileEntry::new(
            crate::file_tree::display_os_str(name),
            dir.join(name),
            is_dir,
        ));
    }
    crate::file_tree::sort_entries(&mut entries);
    entries
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
        _ => Err(probe_error(
            io::ErrorKind::Other,
            op,
            &capture,
            "operation failed",
        )),
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

fn spawn_bounded_reader(
    pipe: impl Read + Send + 'static,
    max_out: usize,
) -> std::thread::JoinHandle<io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        pipe.take(max_out as u64).read_to_end(&mut buf)?;
        Ok(buf)
    })
}

fn join_reader(
    reader: Option<std::thread::JoinHandle<io::Result<Vec<u8>>>>,
) -> io::Result<Vec<u8>> {
    match reader {
        Some(handle) => handle
            .join()
            .map_err(|_| io::Error::other("probe output reader panicked"))?,
        None => Ok(Vec::new()),
    }
}

/// Spawn argv[0] with piped stdio, feed it `stdin_bytes`, and capture both
/// output streams bounded to `max_out`. A watchdog kills the child once
/// `timeout` passes so a stuck ssh/docker can never wedge a worker thread.
fn run_capture(
    argv: &[OsString],
    stdin_bytes: &[u8],
    timeout: Duration,
    max_out: usize,
) -> io::Result<Capture> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty probe argv",
        ));
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
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
        match child.try_wait()? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
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
    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
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
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
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
            let _ = child.kill();
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
/// starts executing), stderr draining bounded on a reader thread.
fn spawn_probe_argv(argv: &[OsString], mode: ScriptDelivery) -> io::Result<ProbeChild> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty probe argv",
        ));
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
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

/// Temporary download name in the destination directory: dot-prefixed,
/// unique, and on the same filesystem so the final rename is atomic.
fn part_path(dir: &Path, name: &OsStr) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let bytes = name.as_bytes();
    let keep = bytes.len().min(180);
    let mut temp = OsString::from(".");
    temp.push(OsStr::from_bytes(&bytes[..keep]));
    temp.push(format!(
        ".fspart-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    dir.join(temp)
}

/// Removes a partial download on every error path; after the final rename
/// the temp path no longer exists, which makes the cleanup a no-op.
struct PartFile(PathBuf);

impl Drop for PartFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
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
        (FsLocation::Remote(_), FsLocation::Local) => download(
            remote_host(src_loc, hosts)?,
            src_path,
            dst_dir,
            is_dir,
            control,
            progress,
        ),
        (FsLocation::Local, FsLocation::Remote(_)) => upload(
            remote_host(dst_loc, hosts)?,
            src_path,
            dst_dir,
            is_dir,
            control,
            progress,
        ),
        (FsLocation::Remote(_), FsLocation::Remote(_)) => {
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
        (FsLocation::Local, FsLocation::Local) => Err(io::Error::new(
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
    let probe = spawn()?;
    let probe_handle = probe.handle();
    control.register(&probe_handle);
    let mut stdout = probe
        .stdout
        .ok_or_else(|| io::Error::other("probe has no stdout"))?;
    let _timeout = control.arm_timeout(TRANSFER_TIMEOUT)?;
    let temp = part_path(dir, name);
    let _guard = PartFile(temp.clone());
    let mut file = std::fs::File::create(&temp)?;
    let streamed = stream_to(&mut stdout, &mut file, max, control, progress);
    drop(stdout);
    let status = wait_child(&probe_handle)?;
    let stderr = join_reader(probe.stderr)?;
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
    // Atomic backstop: a same-name file that appeared while streaming must
    // never be overwritten.
    require_missing(&dst)?;
    std::fs::rename(&temp, &dst)?;
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
    let stderr = join_reader(probe.stderr)?;
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
    let mut tar = Command::new("tar")
        .args(["cf", "-", "-C"])
        .arg(parent)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
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
            let _ = child.kill();
        }
    }
    let tar_status = wait_child(&tar)?;
    let tar_stderr = join_reader(tar_stderr)?;
    let status = wait_child(&probe_handle)?;
    let stderr = join_reader(probe.stderr)?;
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
    let probe = spawn()?;
    let probe_handle = probe.handle();
    let stdout = probe
        .stdout
        .ok_or_else(|| io::Error::other("probe has no stdout"))?;
    let mut tar = Command::new("tar")
        .args(["xf", "-", "-C"])
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
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
            let _ = child.kill();
        }
    }
    let tar_status = wait_child(&tar)?;
    let tar_stderr = join_reader(tar_stderr)?;
    let status = wait_child(&probe_handle)?;
    let stderr = join_reader(probe.stderr)?;
    if control.is_timed_out() {
        let _ = std::fs::remove_dir_all(&dst);
        return Err(transfer_timed_out_error());
    }
    if let Err(error) = control.check() {
        // tar streams straight into the destination: drop the partial tree.
        let _ = std::fs::remove_dir_all(&dst);
        return Err(error);
    }
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
    if let Err(error) = outcome {
        // tar streams straight into the destination: drop the partial tree.
        let _ = std::fs::remove_dir_all(&dst);
        return Err(error);
    }
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
    let action = match dst_loc {
        FsLocation::Local => DropAction::Copy,
        FsLocation::Remote(_) => DropAction::Upload,
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

/// Format one failed batch item's error for the summary toast.
fn batch_failure(outcome: &mut BatchOutcome, display: String, error: &io::Error) {
    let message =
        jterm_core::review_input::safe_inline_display(&error.to_string(), MAX_ERROR_DISPLAY_BYTES);
    outcome.failed.push((display, message));
}

/// Display name of a batch item path (escaped, unambiguous).
fn batch_display_name(path: &Path) -> String {
    match path.file_name() {
        Some(name) => crate::file_tree::display_os_str(name),
        None => crate::file_tree::display_full_path(path),
    }
}

/// Paste every clipboard item into `dst_dir`: same-location items rename
/// (cut) or recursive-copy, cross-location items stream via `transfer` with
/// cancel/timeout intact. Cut sources are deleted only after their own
/// transfer succeeded. `progress` receives the running completed-item count.
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
) -> io::Result<BatchOutcome> {
    let same_location = clip_loc == dst_loc;
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
                rename(clip_loc, hosts, &item.path, &dst)
            } else {
                copy(clip_loc, hosts, &item.path, &dst)
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
                    if let Err(error) = delete(clip_loc, hosts, &item.path) {
                        let message = jterm_core::review_input::safe_inline_display(
                            &error.to_string(),
                            MAX_ERROR_DISPLAY_BYTES,
                        );
                        outcome.failed.push((
                            display,
                            format!("copied, but deleting the source failed: {message}"),
                        ));
                    }
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
) -> BatchOutcome {
    let mut outcome = BatchOutcome {
        done: 0,
        failed: Vec::new(),
    };
    for path in paths {
        match delete(loc, hosts, path) {
            Ok(()) => outcome.done += 1,
            Err(error) => batch_failure(&mut outcome, batch_display_name(path), &error),
        }
    }
    outcome
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
        let display = crate::file_tree::display_os_str(name);
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
                let message = jterm_core::review_input::safe_inline_display(
                    &error.to_string(),
                    MAX_ERROR_DISPLAY_BYTES,
                );
                outcome.failed.push((display, message));
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
        // A non-UTF-8 name survives byte-exact and displays escaped.
        bytes.extend_from_slice(b"f\0bad\xffname\0");
        let entries = parse_list_output(&bytes, dir);
        let names: Vec<_> = entries.iter().map(|e| e.name().to_string()).collect();
        assert_eq!(entries.len(), 5);
        assert_eq!(names[0], "dir with spaces", "directories sort first");
        assert!(entries[0].is_dir());
        assert!(names.contains(&r"bad\xffname".to_string()));
        assert!(names.contains(&"quo'te.txt".to_string()));
        assert!(names.contains(&"link\nname".to_string()));
        assert!(names.contains(&r"back\\slash".to_string()));
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
        let entries = parse_list_output(&bytes, dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), "good");
        assert_eq!(entries[0].path(), PathBuf::from("/base/good"));
    }

    #[test]
    fn list_output_truncates_beyond_the_entry_cap() {
        let dir = Path::new("/base");
        let mut bytes = Vec::new();
        for i in 0..crate::file_tree::MAX_DIRECTORY_ENTRIES + 32 {
            bytes.extend_from_slice(format!("f\0file{i:05}\0").as_bytes());
        }
        let entries = parse_list_output(&bytes, dir);
        assert_eq!(entries.len(), crate::file_tree::MAX_DIRECTORY_ENTRIES);
    }

    #[test]
    fn list_output_empty_buffer_yields_no_entries() {
        assert!(parse_list_output(b"", Path::new("/base")).is_empty());
        assert!(parse_list_output(b"\0\0", Path::new("/base")).is_empty());
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
    fn probe_lists_a_real_directory_through_sh() {
        let tmp = TestDir::new("probe-list");
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("file.txt"), b"data").unwrap();
        std::os::unix::fs::symlink("file.txt", tmp.path().join("alink")).unwrap();

        let capture = local_probe(&["list", tmp.path().to_str().unwrap()]);
        assert_eq!(capture.code, Some(0));
        let entries = parse_list_output(&capture.stdout, tmp.path());
        let names: Vec<_> = entries
            .iter()
            .map(|e| (e.name().to_string(), e.is_dir()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("subdir".to_string(), true),
                ("alink".to_string(), false),
                ("file.txt".to_string(), false),
            ],
            "symlinks are files, directories sort first"
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
            local_probe(&["list", tmp.path().join("missing").to_str().unwrap()]).code,
            Some(3)
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

        let mut hostile = ssh_host();
        hostile.name = "secret\u{202e}marker".to_string();
        assert_eq!(
            location_labels(&[hostile]),
            ["Local", "Remote (unavailable)"]
        );
    }

    #[test]
    fn local_list_dir_matches_scan_dir() {
        let tmp = TestDir::new("local-list");
        std::fs::create_dir(tmp.path().join("zdir")).unwrap();
        std::fs::write(tmp.path().join("afile"), b"x").unwrap();
        let entries = list_dir(&FsLocation::Local, &[], tmp.path()).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name().to_string()).collect();
        assert_eq!(names, ["zdir", "afile"]);
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

        let capture = local_probe(&["stat", file.to_str().unwrap()]);
        assert_eq!(capture.code, Some(0));
        assert_eq!(capture.stdout, format!("f {}\n", content.len()).as_bytes());

        let capture = local_probe(&["stat", tmp.path().join("dir").to_str().unwrap()]);
        assert_eq!(capture.stdout, b"d 0\n");
        let capture = local_probe(&["stat", tmp.path().join("link").to_str().unwrap()]);
        assert_eq!(capture.stdout, b"l 0\n");
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
        let outcome = paste_all(
            &[],
            &FsLocation::Local,
            &items,
            &FsLocation::Local,
            &dst,
            true,
            &TransferControl::new(),
            &|_| {},
        )
        .unwrap();
        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.failed.len(), 1);
        assert!(!ok.exists(), "the moved source is gone");
        assert!(blocked.exists(), "the colliding source stays put");
        assert_eq!(std::fs::read(dst.join("ok.txt")).unwrap(), b"ok");
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

        let outcome = delete_all(&FsLocation::Local, &[], &[a.clone(), missing, b.clone()]);
        assert_eq!(outcome.done, 2);
        assert_eq!(outcome.failed.len(), 1);
        assert!(outcome.failed[0].0.contains("missing.txt"));
        assert!(!a.exists() && !b.exists());

        // The filesystem root survives a batch delete.
        let outcome = delete_all(
            &FsLocation::Local,
            &[],
            std::slice::from_ref(&PathBuf::from("/")),
        );
        assert_eq!(outcome.done, 0);
        assert_eq!(outcome.failed.len(), 1);
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
