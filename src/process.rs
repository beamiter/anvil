//! Child-process termination helpers, ported from jterm4's state.rs. Used by the
//! block-view PTY to escalate SIGHUP → SIGTERM → SIGKILL off the GTK main thread.

use nix::errno::Errno;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const SESSION_KILL_DRAIN_TIMEOUT: Duration = Duration::from_millis(150);
const SESSION_SCAN_INTERVAL: Duration = Duration::from_millis(10);
const REQUIRED_QUIET_SESSION_SCANS: u8 = 2;

#[derive(Default)]
struct ChildState {
    exit_code: Option<i32>,
    reaped: bool,
    termination_started: bool,
    cleanup_complete: bool,
}

/// Serializes child reaping with every signal sent during shutdown.
///
/// A raw pid becomes unsafe the instant `waitpid` releases it: the kernel may
/// immediately assign it to an unrelated process. Keeping both operations
/// behind one mutex means no cleanup worker can signal between a successful
/// reap and the corresponding state update.
pub(crate) struct ChildLifecycle {
    pid: Pid,
    state: Mutex<ChildState>,
}

impl ChildLifecycle {
    pub(crate) fn new(pid: Pid) -> Arc<Self> {
        Arc::new(Self {
            pid,
            state: Mutex::new(ChildState::default()),
        })
    }

    fn state(&self) -> MutexGuard<'_, ChildState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            log::error!("PTY child lifecycle lock was poisoned; recovering its state");
            poisoned.into_inner()
        })
    }

    fn poll_reap_locked(&self, state: &mut ChildState) -> Option<i32> {
        if state.reaped {
            return Some(state.exit_code.unwrap_or(1));
        }
        // During explicit shutdown the leader intentionally remains a zombie
        // until HUP → TERM → KILL has been delivered to its whole PTY session.
        // Retaining the pid prevents the kernel from reusing the session id
        // while the cleanup worker is still signaling descendants.
        if state.termination_started && !state.cleanup_complete {
            return None;
        }

        match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                state.exit_code = Some(code);
                state.reaped = true;
                Some(code)
            }
            Ok(WaitStatus::Signaled(_, signal, _)) => {
                let code = 128 + signal as i32;
                state.exit_code = Some(code);
                state.reaped = true;
                Some(code)
            }
            Err(Errno::ECHILD) => {
                // Another in-process owner should not reap this child, but if
                // it did, the pid is no longer ours and must never be signaled.
                state.exit_code = Some(1);
                state.reaped = true;
                Some(1)
            }
            Err(Errno::EINTR) | Ok(_) => None,
            Err(error) => {
                log::warn!("failed to poll PTY child {}: {error}", self.pid);
                None
            }
        }
    }

    pub(crate) fn poll_reap(&self) -> Option<i32> {
        let mut state = self.state();
        self.poll_reap_locked(&mut state)
    }

    fn signal_during_cleanup(&self, signal: std::ffi::c_int) -> bool {
        let state = self.state();
        if state.reaped {
            return false;
        }
        signal_pid_and_session(self.pid.as_raw(), signal);
        true
    }

    fn kill_session_during_cleanup(&self) -> bool {
        let state = self.state();
        if state.reaped {
            return false;
        }
        signal_pid_and_session_until_quiet(
            self.pid.as_raw(),
            nix::libc::SIGKILL,
            SESSION_KILL_DRAIN_TIMEOUT,
        );
        true
    }

    fn begin_termination(&self) -> bool {
        let mut state = self.state();
        if state.reaped || state.termination_started {
            return false;
        }
        state.termination_started = true;
        signal_pid_and_session(self.pid.as_raw(), nix::libc::SIGHUP);
        true
    }

    fn finish_termination_and_reap(&self) {
        {
            let mut state = self.state();
            state.cleanup_complete = true;
        }
        while self.poll_reap().is_none() {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Used only when a reader thread could not be created. No concurrent
    /// reader can reap this child, so kill and wait synchronously while holding
    /// the lifecycle lock and make every later Drop a no-op.
    pub(crate) fn force_kill_and_reap(&self) {
        let mut state = self.state();
        if state.reaped {
            return;
        }
        state.termination_started = true;
        signal_pid_and_session_until_quiet(
            self.pid.as_raw(),
            nix::libc::SIGKILL,
            SESSION_KILL_DRAIN_TIMEOUT,
        );
        let code = loop {
            match waitpid(self.pid, None) {
                Ok(WaitStatus::Exited(_, code)) => break code,
                Ok(WaitStatus::Signaled(_, signal, _)) => break 128 + signal as i32,
                Err(Errno::EINTR) | Ok(_) => continue,
                Err(error) => {
                    log::warn!("failed to reap PTY child {}: {error}", self.pid);
                    break 1;
                }
            }
        };
        state.exit_code = Some(code);
        state.reaped = true;
        state.cleanup_complete = true;
    }
}

/// Parse `/proc/<pid>/stat` and return the parent pid (the 4th field, after the
/// `comm` parenthesised name which may itself contain spaces/parens).
fn read_ppid(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = contents.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    fields.next(); // state
    fields.next()?.parse::<i32>().ok()
}

/// Read `/proc/<pid>/cmdline` as a NUL-separated argv vector.
fn read_proc_cmdline(pid: i32) -> Option<Vec<String>> {
    if pid <= 0 {
        return None;
    }
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    let args: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

/// Check if an argv matches a known restorable command pattern, returning the
/// original argument vector for session persistence. Keeping argv structured is
/// important: joining it here would discard quoting boundaries, so a remote
/// command argument containing `;` could become a new local command on restore.
pub(crate) fn match_restorable_command(args: &[String]) -> Option<Vec<String>> {
    if args.is_empty() {
        return None;
    }
    let bin = std::path::Path::new(&args[0])
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    match bin.as_str() {
        "nix" => {
            if args.len() >= 2 && args[1] == "develop" {
                Some(args.to_vec())
            } else {
                None
            }
        }
        // nix develop execs into e.g. `bash --rcfile /tmp/nix-shell.XXXXX`.
        "bash" | "zsh" | "fish" => {
            for arg in &args[1..] {
                if arg.starts_with("/tmp/nix-shell.") || arg.starts_with("/tmp/nix-shell-") {
                    return Some(vec!["nix".to_string(), "develop".to_string()]);
                }
            }
            None
        }
        "ssh" | "mosh" => Some(args.to_vec()),
        "docker" | "podman" => {
            if args.len() >= 2
                && (args[1] == "exec"
                    || (args[1] == "compose" && args.len() >= 3 && args[2] == "exec"))
            {
                Some(args.to_vec())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn path_basename(command: &str) -> &str {
    std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}

fn is_flatpak_host_wrapper(args: &[String]) -> bool {
    args.first()
        .is_some_and(|command| path_basename(command) == "flatpak-spawn")
        && args.iter().any(|argument| argument == "--host")
}

fn command_basename(args: &[String]) -> &str {
    let command = args.first().map(String::as_str).unwrap_or_default();
    let basename = path_basename(command);
    if basename != "flatpak-spawn" {
        return basename;
    }

    // `host::wrap_argv` makes flatpak-spawn the visible PTY child. Recover the
    // actual host command from the exact option forms that wrapper emits so
    // process-based namespace checks work identically inside and outside the
    // sandbox.
    args.iter()
        .skip(1)
        .map(String::as_str)
        .find(|argument| {
            !matches!(*argument, "--host" | "--watch-bus")
                && !argument.starts_with("--directory=")
                && !argument.starts_with("--env=")
        })
        .map(path_basename)
        .unwrap_or_default()
}

/// Whether OSC 7 paths reported while this command runs belong to another
/// filesystem namespace and therefore must not drive local cwd operations.
pub(crate) fn command_uses_external_cwd(args: &[String]) -> bool {
    matches!(
        command_basename(args),
        "ssh" | "mosh" | "mosh-client" | "docker" | "podman"
    )
}

/// Commands whose restored session semantics require the Block parser even
/// when local panes use the VTE compatibility backend.
pub(crate) fn command_requires_block_integration(args: &[String]) -> bool {
    matches!(command_basename(args), "ssh" | "mosh")
}

/// Render one argv as a single POSIX-shell command without changing argument
/// boundaries. Every argument is single-quoted; embedded single quotes use the
/// standard close/quoted-quote/reopen sequence.
///
/// Control characters are rejected even though a shell can quote some of them:
/// this command is injected through an interactive PTY, where bytes such as ESC
/// or a newline are interpreted by the line editor before shell parsing.
pub(crate) fn shell_quote_argv(args: &[String]) -> Option<String> {
    if args.is_empty()
        || args
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
    {
        return None;
    }
    Some(
        args.iter()
            .map(|argument| format!("'{}'", argument.replace('\'', "'\"'\"'")))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn powershell_quote_argv(args: &[String]) -> Option<String> {
    if args.is_empty()
        || args
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
    {
        return None;
    }
    Some(format!(
        "& {}",
        args.iter()
            .map(|argument| format!("'{}'", argument.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

/// Quote a restorable argv for the configured interactive shell. Unknown shell
/// grammars are deliberately not guessed: skipping automatic replay is safer
/// than changing argument boundaries.
pub(crate) fn shell_quote_argv_for(args: &[String], shell_argv: &[String]) -> Option<String> {
    let shell = std::path::Path::new(shell_argv.first()?)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    match shell.as_str() {
        "pwsh" | "powershell" | "powershell.exe" | "pwsh.exe" => powershell_quote_argv(args),
        "bash" | "dash" | "fish" | "ksh" | "mksh" | "sh" | "zsh" => shell_quote_argv(args),
        _ => None,
    }
}

fn tty_foreground_pgid(pty_fd: i32) -> Option<i32> {
    if pty_fd < 0 {
        return None;
    }
    let fg = unsafe { nix::libc::tcgetpgrp(pty_fd) };
    (fg > 0).then_some(fg)
}

/// The foreground process group id on a PTY master fd, or None if the shell
/// itself (`shell_pid`) is in the foreground (nothing interesting is running).
fn foreground_pgid(pty_fd: i32, shell_pid: i32) -> Option<i32> {
    tty_foreground_pgid(pty_fd).filter(|foreground| *foreground != shell_pid)
}

fn classify_foreground_external_cwd<ReadArgv, ReadParent>(
    shell_pid: i32,
    foreground_pid: i32,
    mut read_argv: ReadArgv,
    mut read_parent: ReadParent,
) -> Option<bool>
where
    ReadArgv: FnMut(i32) -> Option<Vec<String>>,
    ReadParent: FnMut(i32) -> Option<i32>,
{
    if shell_pid <= 1 || foreground_pid <= 1 {
        return None;
    }

    // A managed ssh/mosh pane launches the external command as the PTY child
    // itself. Reading it before the foreground comparison preserves that case.
    let shell_argv = read_argv(shell_pid)?;
    if command_uses_external_cwd(&shell_argv) {
        return Some(true);
    }
    if is_flatpak_host_wrapper(&shell_argv) {
        // A local shell launched through `flatpak-spawn --host` can start ssh
        // entirely outside this PID namespace. The wrapper argv proves neither
        // local nor external foreground state, so preserve the caller's sticky
        // classification unless OSC authority proves it.
        return None;
    }
    if foreground_pid == shell_pid {
        return Some(false);
    }

    let mut pid = foreground_pid;
    for _ in 0..16 {
        if pid == shell_pid {
            return Some(false);
        }
        if pid <= 1 {
            return None;
        }
        let argv = read_argv(pid)?;
        if command_uses_external_cwd(&argv) {
            return Some(true);
        }
        pid = read_parent(pid)?;
    }
    (pid == shell_pid).then_some(false)
}

/// Determine whether the PTY foreground belongs to an ssh/mosh/container
/// namespace. `None` means the tty or `/proc` ancestry could not be read, so a
/// caller must keep its previous conservative classification.
pub(crate) fn foreground_uses_external_cwd(pty_fd: i32, shell_pid: i32) -> Option<bool> {
    let foreground_pid = tty_foreground_pgid(pty_fd)?;
    classify_foreground_external_cwd(shell_pid, foreground_pid, read_proc_cmdline, read_ppid)
}

/// Detect a restorable interactive command (ssh/nix develop/docker exec/…) by
/// walking from the PTY's foreground process up to the shell. Mirrors jterm4's
/// `get_restorable_commands`.
pub(crate) fn restorable_command(pty_fd: i32, shell_pid: i32) -> Option<Vec<String>> {
    // Managed remote panes launch ssh/mosh as the PTY child itself rather than
    // underneath an interactive local shell. Preserve that allowlisted argv
    // too; ordinary bash/zsh/rsh children do not match and continue below.
    if let Some(command) =
        read_proc_cmdline(shell_pid).and_then(|args| match_restorable_command(&args))
    {
        return Some(command);
    }

    let mut pid = foreground_pgid(pty_fd, shell_pid)?;
    let mut visited = 0;
    while pid != shell_pid && pid > 1 && visited < 16 {
        if let Some(args) = read_proc_cmdline(pid) {
            if let Some(cmd) = match_restorable_command(&args) {
                return Some(cmd);
            }
        }
        pid = match read_ppid(pid) {
            Some(ppid) => ppid,
            None => break,
        };
        visited += 1;
    }
    None
}

/// Name of the foreground process on a PTY (e.g. "ssh", "vim"), or None if the
/// shell itself is in the foreground. Used for the close-confirmation prompt.
pub(crate) fn foreground_process_name(pty_fd: i32, shell_pid: i32) -> Option<String> {
    if let Some(args) = read_proc_cmdline(shell_pid) {
        if match_restorable_command(&args).is_some() {
            return std::path::Path::new(args.first()?)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string);
        }
    }
    let fg = foreground_pgid(pty_fd, shell_pid)?;
    let args = read_proc_cmdline(fg)?;
    std::path::Path::new(args.first()?)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessStat {
    state: char,
    process_group: i32,
    session: i32,
}

impl ProcessStat {
    fn is_live(self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }
}

fn parse_process_stat(contents: &str) -> Option<ProcessStat> {
    // stat format: pid (comm) state ppid pgrp ...; comm may contain spaces and
    // parens, so split after the last ')'.
    let rparen_pos = contents.rfind(')')?;
    let after_comm = &contents[rparen_pos + 1..];
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?.chars().next()?;
    fields.next()?; // parent pid
    let process_group = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    Some(ProcessStat {
        state,
        process_group,
        session,
    })
}

fn process_stat_result(pid: i32) -> std::io::Result<ProcessStat> {
    if pid <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "process id must be positive",
        ));
    }
    let path = format!("/proc/{pid}/stat");
    let contents = std::fs::read_to_string(path)?;
    parse_process_stat(&contents)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed proc stat"))
}

fn process_stat(pid: i32) -> Option<ProcessStat> {
    process_stat_result(pid).ok()
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: i32) -> std::io::Result<OwnedFd> {
    let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: pidfd_open returns a new owned descriptor on success.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
}

#[cfg(target_os = "linux")]
fn send_pidfd_signal(pidfd: &OwnedFd, signal: std::ffi::c_int) -> std::io::Result<()> {
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<nix::libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SessionScan {
    live_members: usize,
    failed_members: usize,
    complete: bool,
}

fn signal_session_members(session_leader: i32, signal: std::ffi::c_int) -> SessionScan {
    let mut scan = SessionScan::default();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return scan;
    };
    scan.complete = true;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        let Some(member) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if member == session_leader {
            continue;
        }
        let observed = match process_stat_result(member) {
            Ok(observed) => observed,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                scan.complete = false;
                continue;
            }
        };
        if observed.session != session_leader || !observed.is_live() {
            continue;
        }

        #[cfg(target_os = "linux")]
        {
            // Open the stable process reference before the authoritative second
            // stat read. If the numeric pid was reused at either boundary, the
            // second read will describe a different session and we skip it;
            // pidfd_send_signal can therefore never target that replacement.
            match open_pidfd(member) {
                Ok(pidfd) => {
                    let current = match process_stat_result(member) {
                        Ok(current) => current,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(_) => {
                            scan.complete = false;
                            scan.failed_members += 1;
                            continue;
                        }
                    };
                    if current.session != session_leader || !current.is_live() {
                        continue;
                    }
                    scan.live_members += 1;
                    if let Err(error) = send_pidfd_signal(&pidfd, signal) {
                        if error.raw_os_error() != Some(nix::libc::ESRCH) {
                            scan.failed_members += 1;
                        }
                    }
                }
                Err(_) => {
                    // Count a still-live member so the bounded final drain keeps
                    // retrying. Never fall back to kill(raw_pid): doing so would
                    // recreate the PID-reuse race this path exists to prevent.
                    match process_stat_result(member) {
                        Ok(current) if current.session == session_leader && current.is_live() => {
                            scan.live_members += 1;
                            scan.failed_members += 1;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => {
                            scan.complete = false;
                            scan.failed_members += 1;
                        }
                        Ok(_) => {}
                    }
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            scan.live_members += 1;
            let result = unsafe { nix::libc::kill(member, signal) };
            if result != 0 {
                scan.failed_members += 1;
            }
        }
    }
    scan
}

fn signal_pid_and_session(pid: i32, signal: std::ffi::c_int) -> SessionScan {
    if pid <= 0 {
        return SessionScan::default();
    }

    let ids = process_stat(pid);
    let scan = if ids.is_some_and(|stat| stat.session == pid) {
        // Shell job control places foreground commands in their own process
        // groups, but every one remains in the PTY's session. Enumerating that
        // session reaches stubborn foreground/background descendants that a
        // single `kill(-shell_pgid, ...)` would miss.
        signal_session_members(pid, signal)
    } else {
        if ids.is_some_and(|stat| stat.process_group == pid) {
            unsafe {
                nix::libc::kill(-pid, signal);
            }
        }
        SessionScan::default()
    };
    // Always retain a direct fallback for shells that have not yet called
    // setsid/setpgid. ChildLifecycle guarantees this raw pid still belongs to
    // this PTY for the duration of the call.
    unsafe {
        nix::libc::kill(pid, signal);
    }
    scan
}

fn repeat_session_scans_until_quiet<Scan>(
    deadline: Instant,
    interval: Duration,
    mut scan: Scan,
) -> bool
where
    Scan: FnMut() -> usize,
{
    let mut quiet_scans = 0u8;
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        if scan() == 0 {
            quiet_scans = quiet_scans.saturating_add(1);
            if quiet_scans >= REQUIRED_QUIET_SESSION_SCANS {
                return true;
            }
        } else {
            quiet_scans = 0;
        }

        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(interval.min(deadline.saturating_duration_since(now)));
    }
}

fn signal_pid_and_session_until_quiet(pid: i32, signal: std::ffi::c_int, timeout: Duration) {
    if pid <= 0 {
        return;
    }
    let is_session_leader = process_stat(pid).is_some_and(|stat| stat.session == pid);
    let deadline = Instant::now() + timeout;
    let initial = signal_pid_and_session(pid, signal);
    if !is_session_leader {
        return;
    }

    let mut remaining = initial.live_members;
    let mut failures = initial.failed_members;
    let mut scan_complete = initial.complete;
    let quiet = repeat_session_scans_until_quiet(deadline, SESSION_SCAN_INTERVAL, || {
        let current = signal_session_members(pid, signal);
        remaining = current.live_members;
        failures = current.failed_members;
        scan_complete = current.complete;
        if current.complete {
            current.live_members
        } else {
            usize::MAX
        }
    });
    if !quiet {
        log::warn!(
            "PTY session {pid} did not reach verified quiescence after SIGKILL drain \
             ({remaining} live member(s), {failures} safe-signal failure(s), scan complete: {scan_complete})"
        );
    }
}

pub(crate) fn terminate_terminal_process(child: Arc<ChildLifecycle>) {
    if !child.begin_termination() {
        return;
    }
    let worker_child = Arc::clone(&child);
    if let Err(error) = std::thread::Builder::new()
        .name("jterm1-process-cleanup".to_string())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            worker_child.signal_during_cleanup(nix::libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(250));
            worker_child.kill_session_during_cleanup();
            worker_child.finish_termination_and_reap();
        })
    {
        log::error!("failed to start terminal cleanup worker: {error}");
        child.force_kill_and_reap();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_foreground_external_cwd, command_requires_block_integration,
        command_uses_external_cwd, match_restorable_command, parse_process_stat,
        repeat_session_scans_until_quiet, shell_quote_argv, shell_quote_argv_for, ChildLifecycle,
        ProcessStat,
    };
    #[cfg(target_os = "linux")]
    use super::{
        open_pidfd, process_stat, send_pidfd_signal, signal_pid_and_session_until_quiet,
        SESSION_KILL_DRAIN_TIMEOUT,
    };
    use nix::unistd::Pid;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn restorable_commands_preserve_original_argv_boundaries() {
        let argv = strings(&[
            "/usr/bin/ssh",
            "example.test",
            "printf '%s, %s; still remote' one two",
        ]);
        assert_eq!(match_restorable_command(&argv), Some(argv));
    }

    #[test]
    fn restored_argv_is_shell_quoted_as_one_safe_command() {
        let argv = strings(&[
            "ssh",
            "example.test",
            "printf '%s, %s'; touch /tmp/must-stay-remote",
        ]);
        assert_eq!(
            shell_quote_argv(&argv).as_deref(),
            Some("'ssh' 'example.test' 'printf '\"'\"'%s, %s'\"'\"'; touch /tmp/must-stay-remote'")
        );
    }

    #[test]
    fn restored_argv_rejects_pty_control_characters() {
        assert!(shell_quote_argv(&strings(&["ssh", "host", "echo one\necho two"])).is_none());
        assert!(shell_quote_argv(&strings(&["ssh", "host", "\u{1b}[31m"])).is_none());
    }

    #[test]
    fn restored_argv_uses_the_configured_shell_grammar() {
        let argv = strings(&["ssh", "host", "printf 'safe'; still one argument"]);
        assert_eq!(
            shell_quote_argv_for(&argv, &strings(&["/usr/bin/pwsh"])).as_deref(),
            Some("& 'ssh' 'host' 'printf ''safe''; still one argument'")
        );
        assert!(shell_quote_argv_for(&argv, &strings(&["/usr/bin/unknown-shell"])).is_none());
    }

    #[test]
    fn external_cwd_and_block_requirements_are_classified_separately() {
        assert!(command_uses_external_cwd(&strings(&[
            "/usr/bin/docker",
            "exec",
            "container"
        ])));
        assert!(!command_requires_block_integration(&strings(&[
            "/usr/bin/docker",
            "exec",
            "container"
        ])));
        assert!(command_uses_external_cwd(&strings(&[
            "/usr/bin/ssh",
            "host"
        ])));
        assert!(command_requires_block_integration(&strings(&[
            "/usr/bin/ssh",
            "host"
        ])));
        assert!(command_uses_external_cwd(&strings(&[
            "/usr/bin/flatpak-spawn",
            "--host",
            "--watch-bus",
            "--env=TERM=xterm-256color",
            "/usr/bin/ssh",
            "host"
        ])));
        assert!(command_uses_external_cwd(&strings(&[
            "/usr/bin/mosh-client",
            "203.0.113.1",
            "60001"
        ])));
    }

    #[test]
    fn foreground_external_cwd_classification_is_conservative_and_walks_ancestry() {
        assert_eq!(
            classify_foreground_external_cwd(
                10,
                10,
                |_| Some(strings(&["/usr/bin/ssh", "host"])),
                |_| None,
            ),
            Some(true),
            "a managed external command can be the PTY child itself"
        );
        assert_eq!(
            classify_foreground_external_cwd(10, 10, |_| Some(strings(&["/bin/bash"])), |_| None,),
            Some(false),
            "an explicitly foreground local shell is local"
        );
        assert_eq!(
            classify_foreground_external_cwd(
                10,
                10,
                |_| {
                    Some(strings(&[
                        "/usr/bin/flatpak-spawn",
                        "--host",
                        "--watch-bus",
                        "/bin/bash",
                    ]))
                },
                |_| None,
            ),
            None,
            "a host shell wrapper cannot reveal commands launched in the host PID namespace"
        );

        let argv = |pid| match pid {
            10 => Some(strings(&["/bin/bash"])),
            20 => Some(strings(&["/usr/bin/docker", "exec", "dev"])),
            30 => Some(strings(&["/usr/bin/env"])),
            _ => None,
        };
        let parent = |pid| match pid {
            30 => Some(20),
            20 => Some(10),
            _ => None,
        };
        assert_eq!(
            classify_foreground_external_cwd(10, 30, argv, parent),
            Some(true),
            "an external command anywhere on the foreground ancestry is external"
        );

        let argv = |pid| match pid {
            10 => Some(strings(&["/bin/bash"])),
            20 => Some(strings(&["/usr/bin/sudo"])),
            30 => Some(strings(&["/usr/bin/env"])),
            _ => None,
        };
        let parent = |pid| match pid {
            30 => Some(20),
            20 => Some(10),
            _ => None,
        };
        assert_eq!(
            classify_foreground_external_cwd(10, 30, argv, parent),
            Some(false),
            "a complete non-external ancestry back to the shell is local"
        );
        assert_eq!(
            classify_foreground_external_cwd(10, 30, |_| None, |_| None),
            None,
            "an unreadable proc chain must not unlock an external cwd"
        );
    }

    #[test]
    fn proc_stat_parser_keeps_state_group_and_session_with_tricky_names() {
        let parsed =
            parse_process_stat("123 (name with ) parens) S 1 77 88 0 0 0").expect("valid stat");
        assert_eq!(
            parsed,
            ProcessStat {
                state: 'S',
                process_group: 77,
                session: 88,
            }
        );
        assert!(parsed.is_live());
        assert!(!parse_process_stat("124 (zombie) Z 1 77 88 0")
            .expect("valid zombie stat")
            .is_live());
    }

    #[test]
    fn session_drain_requires_quiet_after_a_late_fork_appears() {
        let mut scans = std::collections::VecDeque::from([1usize, 0, 1, 0, 0]);
        let mut calls = 0usize;
        let quiet = repeat_session_scans_until_quiet(
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::ZERO,
            || {
                calls += 1;
                scans.pop_front().unwrap_or_default()
            },
        );
        assert!(quiet);
        assert_eq!(calls, 5);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_signal_is_bound_to_the_opened_process() {
        use std::os::unix::process::ExitStatusExt;

        let mut child = std::process::Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("spawn pidfd target");
        let pidfd = match open_pidfd(child.id() as i32) {
            Ok(pidfd) => pidfd,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(nix::libc::ENOSYS) | Some(nix::libc::EPERM)
                ) =>
            {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("pidfd_open failed: {error}");
            }
        };
        if let Err(error) = send_pidfd_signal(&pidfd, nix::libc::SIGKILL) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pidfd_send_signal failed: {error}");
        }
        let status = child.wait().expect("reap pidfd target");
        assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_drain_kills_a_background_member_of_the_child_session() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        // SAFETY: setsid is async-signal-safe and the closure performs no
        // allocation or other work after Command forks.
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().expect("spawn session leader");
        let leader = child.id() as i32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let member = loop {
            let member = std::fs::read_dir("/proc")
                .expect("read proc")
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
                .find(|pid| {
                    *pid != leader
                        && process_stat(*pid)
                            .is_some_and(|stat| stat.session == leader && stat.is_live())
                });
            if let Some(member) = member {
                break member;
            }
            if std::time::Instant::now() >= deadline {
                unsafe {
                    nix::libc::kill(-leader, nix::libc::SIGKILL);
                    nix::libc::kill(leader, nix::libc::SIGKILL);
                }
                let _ = child.wait();
                panic!("background session member did not appear");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        if let Err(error) = open_pidfd(member) {
            unsafe {
                nix::libc::kill(-leader, nix::libc::SIGKILL);
                nix::libc::kill(leader, nix::libc::SIGKILL);
            }
            let _ = child.wait();
            if matches!(
                error.raw_os_error(),
                Some(nix::libc::ENOSYS) | Some(nix::libc::EPERM)
            ) {
                return;
            }
            panic!("pidfd_open for session member failed: {error}");
        }

        signal_pid_and_session_until_quiet(leader, nix::libc::SIGKILL, SESSION_KILL_DRAIN_TIMEOUT);
        let drained = std::fs::read_dir("/proc")
            .expect("read proc after drain")
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .all(|pid| {
                pid == leader
                    || process_stat(pid)
                        .is_none_or(|stat| stat.session != leader || !stat.is_live())
            });
        let status = child.wait().expect("reap session leader");
        assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
        assert!(
            drained,
            "a live descendant remained in the drained PTY session"
        );
    }

    #[test]
    #[allow(clippy::zombie_processes)] // ChildLifecycle, not Child, owns waitpid in this test.
    fn child_lifecycle_caches_reaped_status_for_later_drop_paths() {
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 23"])
            .spawn()
            .unwrap();
        let lifecycle = ChildLifecycle::new(Pid::from_raw(child.id() as i32));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let code = loop {
            if let Some(code) = lifecycle.poll_reap() {
                break code;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child was not reaped before the test deadline"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(code, 23);
        assert_eq!(lifecycle.poll_reap(), Some(23));
    }
}
