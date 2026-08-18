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
pub(crate) const PROBE_SCRIPT: &str = r#"# remote-fs probe v2 — runs under `sh -s -- <op> [args...]`.
# `list` stdout: NUL-separated pairs "<t>\0<name>\0", t in {d,f,l}, names relative.
# `cat` streams a file to stdout; `put` stores stdin as a new file;
# `tar`/`untar` move directories as tar streams on stdout/stdin.
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
    d=${2:-}
    case "$d" in /*) ;; *) exit 2 ;; esac
    [ -d "$d" ] || exit 3
    command -v tar >/dev/null 2>&1 || { echo "remote-fs probe: tar is not installed" >&2; exit 4; }
    tar xf - -C "$d" || exit 4
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
            FsLocation::Remote(index) => match hosts.get(*index) {
                Some(host) => location_label(host),
                None => "Remote".to_string(),
            },
        }
    }
}

fn location_label(host: &RemoteHost) -> String {
    let scheme = if host.docker { "docker" } else { "ssh" };
    format!("{scheme}: {}", host.name)
}

/// Dropdown labels for the header's location selector; index 0 is `Local`.
pub(crate) fn location_labels(hosts: &[RemoteHost]) -> Vec<String> {
    let mut labels = Vec::with_capacity(hosts.len() + 1);
    labels.push("Local".to_string());
    labels.extend(hosts.iter().map(location_label));
    labels
}

/// One remembered Copy/Cut row. Paste stays within `loc` in v1.
#[derive(Clone, Debug)]
pub(crate) struct FsClipboard {
    pub(crate) loc: FsLocation,
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
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
        FsLocation::Remote(index) => hosts.get(*index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "remote host is no longer configured",
            )
        }),
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
    let argv = probe_argv(host, op, args, ScriptDelivery::Stdin);
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

/// Kill-on-timeout for transfers: after `timeout`, flag and kill every child
/// (TRANSFER_TIMEOUT, 15 minutes overall per transfer). `Drop` cancels and
/// joins the watchdog thread.
struct TransferWatchdog {
    timed_out: Arc<AtomicBool>,
    cancel: mpsc::Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl TransferWatchdog {
    fn start(children: Vec<Arc<Mutex<Child>>>, timeout: Duration) -> io::Result<Self> {
        let timed_out = Arc::new(AtomicBool::new(false));
        let (cancel, rx) = mpsc::channel::<()>();
        let flag = timed_out.clone();
        let handle = std::thread::Builder::new()
            .name("anvil-fs-transfer-watchdog".to_string())
            .spawn(move || {
                if rx.recv_timeout(timeout).is_err() {
                    flag.store(true, Ordering::SeqCst);
                    for child in &children {
                        if let Ok(mut child) = child.lock() {
                            let _ = child.kill();
                        }
                    }
                }
            })?;
        Ok(Self {
            timed_out,
            cancel,
            handle: Some(handle),
        })
    }

    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }
}

impl Drop for TransferWatchdog {
    fn drop(&mut self) {
        let _ = self.cancel.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
    let argv = probe_argv(host, op, args, mode);
    spawn_probe_argv(&argv, mode)
}

/// Pump `from` into `to` in 64 KiB chunks, enforcing `max`; on overflow the
/// children are killed so no partial payload keeps moving. A broken pipe
/// means the far side exited early — its exit code tells the real story, so
/// the pump stops quietly and lets the caller read it.
fn stream_to<R: Read, W: Write>(
    mut from: R,
    mut to: W,
    max: u64,
    killers: &[Arc<Mutex<Child>>],
) -> io::Result<u64> {
    let mut buf = [0u8; STREAM_BUF_SIZE];
    let mut total = 0u64;
    loop {
        let read = from.read(&mut buf)?;
        if read == 0 {
            return Ok(total);
        }
        total += read as u64;
        if total > max {
            for child in killers {
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                }
            }
            return Err(too_large_error(max));
        }
        match to.write_all(&buf[..read]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(total),
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

/// Byte-exact "does this name already exist there" check over `list`, so an
/// upload can fail BEFORE streaming instead of merging into a remote dir.
fn remote_name_exists(host: &RemoteHost, dir: &Path, name: &OsStr) -> io::Result<bool> {
    let stdout = run_probe(host, "list", &[dir.as_os_str()], PROBE_LIST_TIMEOUT)?;
    let mut fields = stdout.split(|&byte| byte == 0);
    while let (Some(_kind), Some(entry)) = (fields.next(), fields.next()) {
        if entry == name.as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Transfer `src_path` between locations: download (Remote→Local), upload
/// (Local→Remote), or a local staging relay between two remote hosts.
/// Returns the destination path. Same-name collisions fail BEFORE streaming
/// (local metadata / remote list); file uploads are additionally enforced
/// atomically by the probe's `put`.
pub(crate) fn transfer(
    hosts: &[RemoteHost],
    src_loc: &FsLocation,
    src_path: &Path,
    dst_loc: &FsLocation,
    dst_dir: &Path,
    is_dir: bool,
) -> io::Result<PathBuf> {
    match (src_loc, dst_loc) {
        (FsLocation::Remote(_), FsLocation::Local) => {
            download(remote_host(src_loc, hosts)?, src_path, dst_dir, is_dir)
        }
        (FsLocation::Local, FsLocation::Remote(_)) => {
            upload(remote_host(dst_loc, hosts)?, src_path, dst_dir, is_dir)
        }
        (FsLocation::Remote(_), FsLocation::Remote(_)) => {
            // No host-to-host channel exists, so relay through a unique local
            // staging dir that is always cleaned up.
            let src_host = remote_host(src_loc, hosts)?;
            let dst_host = remote_host(dst_loc, hosts)?;
            let name = src_path.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "remote path has no file name")
            })?;
            if remote_name_exists(dst_host, dst_dir, name)? {
                return Err(already_exists_error(name));
            }
            let staging = StagingDir::new()?;
            let staged = download(src_host, src_path, staging.path(), is_dir)?;
            upload(dst_host, &staged, dst_dir, is_dir)
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
        )
    }
}

fn upload(
    host: &RemoteHost,
    local_path: &Path,
    remote_dir: &Path,
    is_dir: bool,
) -> io::Result<PathBuf> {
    require_absolute(remote_dir)?;
    let name = local_path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "local path has no file name")
    })?;
    // Fail before streaming: the remote list sees an existing destination.
    if remote_name_exists(host, remote_dir, name)? {
        return Err(already_exists_error(name));
    }
    let dst = remote_dir.join(name);
    if is_dir {
        upload_dir_with(
            || spawn_probe_streaming(host, "untar", &[dst.as_os_str()], ScriptDelivery::Argv),
            local_path,
            MAX_TRANSFER_BYTES,
        )?;
    } else {
        upload_file_with(
            || spawn_probe_streaming(host, "put", &[dst.as_os_str()], ScriptDelivery::Argv),
            local_path,
            MAX_TRANSFER_BYTES,
        )?;
    }
    Ok(dst)
}

/// Stream one remote regular file into `dir`. Takes the probe spawn as a
/// parameter so tests can drive the exact mechanics with a local `sh` as the
/// "remote"; `max` is the payload cap (MAX_TRANSFER_BYTES in production).
fn download_file_with(
    spawn: impl FnOnce() -> io::Result<ProbeChild>,
    name: &OsStr,
    dir: &Path,
    max: u64,
) -> io::Result<PathBuf> {
    let dst = dir.join(name);
    require_missing(&dst)?;
    let probe = spawn()?;
    let probe_handle = probe.handle();
    let mut stdout = probe
        .stdout
        .ok_or_else(|| io::Error::other("probe has no stdout"))?;
    let watchdog = TransferWatchdog::start(vec![probe_handle.clone()], TRANSFER_TIMEOUT)?;
    let temp = part_path(dir, name);
    let _guard = PartFile(temp.clone());
    let mut file = std::fs::File::create(&temp)?;
    let killers = [probe_handle.clone()];
    let streamed = stream_to(&mut stdout, &mut file, max, &killers);
    drop(stdout);
    let status = wait_child(&probe_handle)?;
    let stderr = join_reader(probe.stderr)?;
    if watchdog.timed_out() {
        return Err(transfer_timed_out_error());
    }
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
    let probe = spawn()?;
    let probe_handle = probe.handle();
    let stdin = probe
        .stdin
        .ok_or_else(|| io::Error::other("probe has no stdin"))?;
    // Drain the (normally empty) stdout so a chatty far side cannot block.
    let stdout_drain = probe
        .stdout
        .map(|pipe| spawn_bounded_reader(pipe, MAX_TRANSFER_STDERR_BYTES));
    let watchdog = TransferWatchdog::start(vec![probe_handle.clone()], TRANSFER_TIMEOUT)?;
    let file = std::fs::File::open(local_path)?;
    let killers = [probe_handle.clone()];
    let streamed = stream_to(file, stdin, max, &killers);
    // stream_to owns and drops stdin here, so the far side sees the payload
    // EOF before it finishes `put`.
    let status = wait_child(&probe_handle)?;
    let stderr = join_reader(probe.stderr)?;
    let _ = join_reader(stdout_drain);
    if watchdog.timed_out() {
        return Err(transfer_timed_out_error());
    }
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
) -> io::Result<()> {
    require_local_tar()?;
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
    let stdin = probe
        .stdin
        .ok_or_else(|| io::Error::other("probe has no stdin"))?;
    let stdout_drain = probe
        .stdout
        .map(|pipe| spawn_bounded_reader(pipe, MAX_TRANSFER_STDERR_BYTES));
    let watchdog =
        TransferWatchdog::start(vec![probe_handle.clone(), tar.clone()], TRANSFER_TIMEOUT)?;
    let killers = [probe_handle.clone(), tar.clone()];
    let streamed = stream_to(tar_stdout, stdin, max, &killers);
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
    if watchdog.timed_out() {
        return Err(transfer_timed_out_error());
    }
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
) -> io::Result<PathBuf> {
    let dst = dir.join(name);
    require_missing(&dst)?;
    require_local_tar()?;
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
    let watchdog =
        TransferWatchdog::start(vec![probe_handle.clone(), tar.clone()], TRANSFER_TIMEOUT)?;
    let killers = [probe_handle.clone(), tar.clone()];
    let streamed = stream_to(stdout, tar_stdin, max, &killers);
    if streamed.is_err() {
        if let Ok(mut child) = tar.lock() {
            let _ = child.kill();
        }
    }
    let tar_status = wait_child(&tar)?;
    let tar_stderr = join_reader(tar_stderr)?;
    let status = wait_child(&probe_handle)?;
    let stderr = join_reader(probe.stderr)?;
    if watchdog.timed_out() {
        let _ = std::fs::remove_dir_all(&dst);
        return Err(transfer_timed_out_error());
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
        assert_eq!(FsLocation::Remote(9).label(&hosts), "Remote");
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
    fn probe_v2_tar_untar_round_trip() {
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
        let capture = local_probe_payload(&["untar", out.to_str().unwrap()], &tarred.stdout);
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
                &["untar", tmp.path().join("missing").to_str().unwrap()],
                b"x"
            )
            .code,
            Some(3)
        );
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
        assert!(command.contains("remote-fs probe v2"));
        assert!(command.ends_with(" -- 'put' '/dst/a b'\\''c'"));

        // docker: script as one raw argv element, no quoting anywhere.
        let host = docker_host();
        let argv = docker_probe_argv(&host, "untar", &[OsStr::new("/dst")], ScriptDelivery::Argv);
        let text: Vec<_> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pos = text.iter().position(|a| a == "-c").expect("-c present");
        assert_eq!(text[pos + 1], PROBE_SCRIPT);
        assert_eq!(text[pos + 2..], ["--", "untar", "/dst"]);
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
        )
        .unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), content);

        let err = upload_file_with(
            || spawn_local(&["put", &dst_str], ScriptDelivery::Argv),
            &src,
            1 << 20,
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
            || spawn_local(&["untar", &relay_str], ScriptDelivery::Argv),
            &src,
            1 << 24,
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
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
