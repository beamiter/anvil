//! Child-process termination helpers, ported from jterm4's state.rs. Used by the
//! block-view PTY to escalate SIGHUP → SIGTERM → SIGKILL off the GTK main thread.
//!
//! Shell quoting, restorable-command classification, and the `/proc` probes
//! live in `jterm_core::process` (seeded from this file); the app-facing
//! helpers are re-exported here so call sites keep their `crate::process::`
//! paths.

pub(crate) use jterm_core::process::{
    command_requires_block_integration, command_uses_external_cwd, foreground_process_name,
    foreground_uses_external_cwd, restorable_command, shell_quote_argv_for, shell_quote_path,
};

use jterm_core::process::{process_stat, process_stat_result};
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
    #[cfg(target_os = "linux")]
    use super::{
        open_pidfd, process_stat, send_pidfd_signal, signal_pid_and_session_until_quiet,
        SESSION_KILL_DRAIN_TIMEOUT,
    };
    use super::{repeat_session_scans_until_quiet, ChildLifecycle};
    use nix::unistd::Pid;

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
