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
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::RemoteHost;
use crate::file_tree::FileEntry;

/// The POSIX sh probe every remote operation funnels through. It runs under
/// `sh -s -- <op> [args...]` with this script on stdin; keep the exit-code
/// contract in sync with `probe_result`.
pub(crate) const PROBE_SCRIPT: &str = r#"# remote-fs probe v1 — runs under `sh -s -- <op> [args...]`.
# `list` stdout: NUL-separated pairs "<t>\0<name>\0", t in {d,f,l}, names relative.
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
/// One directory holds at most MAX_DIRECTORY_ENTRIES shown entries of at most
/// 255 bytes; 2 MiB caps the capture without cutting a legitimate listing.
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
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
    let argv = probe_argv(host, op, args);
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
    let stderr = crate::file_tree::display_os_str(OsStr::from_bytes(&capture.stderr));
    let stderr = stderr.trim();
    let message = if stderr.is_empty() {
        format!("remote {op}: {fallback}")
    } else {
        format!(
            "remote {op}: {}",
            jterm_core::review_input::safe_inline_display(stderr, MAX_ERROR_DISPLAY_BYTES)
        )
    };
    io::Error::new(kind, message)
}

/// Build the local argv that runs the probe on the far side. The script
/// always travels on stdin; argv only carries `sh -s -- <op> [args...]`.
fn probe_argv(host: &RemoteHost, op: &str, args: &[&OsStr]) -> Vec<OsString> {
    if host.docker {
        docker_probe_argv(host, op, args)
    } else {
        ssh_probe_argv(host, op, args)
    }
}

/// ssh re-parses the command string with the far side's login shell, so the
/// whole probe invocation becomes ONE argv element with every value
/// single-quote-escaped. Never interpolate an unquoted path here.
fn ssh_probe_argv(host: &RemoteHost, op: &str, args: &[&OsStr]) -> Vec<OsString> {
    let dest = match &host.user {
        Some(user) => format!("{user}@{}", host.host),
        None => host.host.clone(),
    };
    let mut command = b"sh -s -- ".to_vec();
    command.extend_from_slice(&sq_bytes(op.as_bytes()));
    for arg in args {
        command.push(b' ');
        command.extend_from_slice(&sq_bytes(arg.as_bytes()));
    }
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
/// `-i` keeps stdin open for the script; `-t` would corrupt the byte stream.
fn docker_probe_argv(host: &RemoteHost, op: &str, args: &[&OsStr]) -> Vec<OsString> {
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
    argv.push(OsString::from("-s"));
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

    /// Run the probe script directly through the local `sh`, the same way
    /// ssh/docker would deliver it on a far side.
    fn local_probe(args: &[&str]) -> Capture {
        let argv: Vec<OsString> = ["sh", "-s", "--"]
            .into_iter()
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect();
        run_capture(
            &argv,
            PROBE_SCRIPT.as_bytes(),
            Duration::from_secs(10),
            MAX_CAPTURE_BYTES,
        )
        .expect("local sh probe must run")
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
        let argv = ssh_probe_argv(&host, "list", &[dir.as_os_str()]);
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
        let argv = ssh_probe_argv(&host, "list", &[dir.as_os_str()]);
        let command = argv.last().expect("command element").to_string_lossy();
        assert_eq!(command, "sh -s -- 'list' '/tmp/don'\\''t'");
    }

    #[test]
    fn ssh_argv_without_user_uses_bare_host() {
        let mut host = ssh_host();
        host.user = None;
        let argv = ssh_probe_argv(&host, "home", &[]);
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
        let argv = docker_probe_argv(&host, "list", &[dir.as_os_str()]);
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
        let argv = docker_probe_argv(&host, "home", &[]);
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
}
