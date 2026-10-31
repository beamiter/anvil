//! Bounded Linux observation of one owned PTY session. This is local process
//! lifecycle evidence, never a presentation/retirement proof or shell attestation.
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Busy,
    Unknown,
}

#[derive(Debug)]
pub struct PtySession {
    pid: i32,
    start: Option<u64>,
}

impl PtySession {
    /// Capture after the fork/exec startup handshake; even an exited, unreaped
    /// child still pins its PID and session number for the owner.
    pub fn capture(pid: i32) -> Self {
        Self {
            pid,
            start: linux::read_process(std::path::Path::new("/proc"), pid)
                .ok()
                .filter(|state| state.pid == pid && state.session == pid && pid > 1)
                .map(|state| state.start),
        }
    }

    /// Only a complete observation with the same live leader and no other live
    /// member is idle. Background and stopped jobs count, including disowned
    /// jobs that remain in this session; processes that used setsid do not.
    pub fn activity(&self) -> Activity {
        match linux::members(self, std::path::Path::new("/proc"), false) {
            Ok(members) if members.is_empty() => Activity::Idle,
            Ok(_) => Activity::Busy,
            Err(_) => Activity::Unknown,
        }
    }

    /// The exclusive wait owner calls this after WNOWAIT observes a natural
    /// exit and before reaping. Reconfirm that ownership, retain the zombie
    /// anchor, and use pidfds for each member. A failed/incomplete drain leaves
    /// the anchor unreaped for a later poll; it never publishes a clean exit.
    pub fn drain_after_exit(&self) -> io::Result<()> {
        linux::drain(self)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::time::{Duration, Instant};

    const MAX_PROCESSES: usize = 16_384;
    const MAX_STAT: u64 = 16 * 1024;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct State {
        pub pid: i32,
        pub session: i32,
        pub start: u64,
        live: bool,
    }

    fn invalid() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "unverified PTY session identity",
        )
    }

    fn parse(text: &str) -> Option<State> {
        let (pid, _) = text.split_once('(')?;
        let fields: Vec<_> = text
            .get(text.rfind(')')? + 1..)?
            .split_whitespace()
            .collect();
        Some(State {
            pid: pid.trim().parse().ok()?,
            session: fields.get(3)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
            live: !matches!(*fields.first()?, "Z" | "X" | "x"),
        })
    }

    pub(super) fn read_process(root: &Path, pid: i32) -> io::Result<State> {
        if pid <= 1 {
            return Err(invalid());
        }
        let mut text = String::new();
        File::open(root.join(pid.to_string()).join("stat"))?
            .take(MAX_STAT + 1)
            .read_to_string(&mut text)?;
        if text.len() as u64 > MAX_STAT {
            return Err(invalid());
        }
        parse(&text)
            .filter(|state| state.pid == pid)
            .ok_or_else(invalid)
    }

    fn anchor(session: &PtySession, root: &Path, exited: bool) -> io::Result<()> {
        let state = read_process(root, session.pid)?;
        if session.start != Some(state.start)
            || state.session != session.pid
            || state.live == exited
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn members(
        session: &PtySession,
        root: &Path,
        exited: bool,
    ) -> io::Result<Vec<State>> {
        anchor(session, root, exited)?;
        let mut members = Vec::new();
        let mut seen = 0;
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            seen += 1;
            if seen > MAX_PROCESSES {
                return Err(io::Error::other(
                    "PTY process observation exceeded its bound",
                ));
            }
            if pid <= 1 || pid == session.pid {
                continue;
            }
            let state = match read_process(root, pid) {
                Ok(state) => state,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if state.live && state.session == session.pid {
                members.push(state);
            }
        }
        anchor(session, root, exited)?;
        Ok(members)
    }

    fn confirm_owned_exit(session: &PtySession) -> io::Result<()> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: WNOWAIT observes only this direct child and never reaps it.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                session.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { info.si_pid() } != session.pid {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "PTY child has not exited",
            ));
        }
        Ok(())
    }

    fn kill_member(session: &PtySession, member: State) -> io::Result<()> {
        // SAFETY: pidfd_open has no pointer arguments. Never use a numeric-PID
        // signal fallback if the kernel cannot bind the observed incarnation.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, member.pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let current = read_process(Path::new("/proc"), member.pid)?;
        if current != member {
            return Ok(());
        }
        confirm_owned_exit(session)?;
        anchor(session, Path::new("/proc"), true)?;
        // SAFETY: the fd pins this exact process; null siginfo uses SIGKILL's
        // ordinary semantics. Session/start were rechecked after opening it.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn drain(session: &PtySession) -> io::Result<()> {
        confirm_owned_exit(session)?;
        let deadline = Instant::now() + Duration::from_millis(25);
        let mut empty = false;
        loop {
            let current = members(session, Path::new("/proc"), true)?;
            if current.is_empty() && empty {
                return Ok(());
            }
            empty = current.is_empty();
            for member in current {
                if let Err(error) = kill_member(session, member) {
                    if !matches!(error.kind(), io::ErrorKind::NotFound)
                        && error.raw_os_error() != Some(libc::ESRCH)
                    {
                        return Err(error);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "PTY session drain is not complete",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};

        struct Owned(Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let session = PtySession::capture(self.0.id() as i32);
                let _ = self.0.kill();
                for _ in 0..20 {
                    if session.drain_after_exit().is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                let _ = self.0.wait();
            }
        }
        fn shell(body: &str) -> Owned {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", body])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            let mut child = Owned(command.spawn().unwrap());
            let mut line = String::new();
            BufReader::new(child.0.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "ready\n");
            child
        }
        #[test]
        fn live_background_group_blocks_and_natural_exit_drains_only_owned_session() {
            let mut control = Owned(Command::new("/bin/sleep").arg("30").spawn().unwrap());
            let mut child = shell("/usr/bin/python3 -c 'import os,signal,time; os.setpgrp(); signal.signal(signal.SIGHUP,signal.SIG_IGN); print(\"ready\",flush=True); time.sleep(30)' & read answer; exit 7");
            let identity = PtySession::capture(child.0.id() as i32);
            assert_eq!(identity.activity(), Activity::Busy);
            let before = members(&identity, Path::new("/proc"), false).unwrap();
            assert!(!before.is_empty());
            assert_eq!(unsafe { libc::kill(before[0].pid, libc::SIGSTOP) }, 0);
            assert_eq!(
                identity.activity(),
                Activity::Busy,
                "stopped work is still attached"
            );
            assert!(
                identity.drain_after_exit().is_err(),
                "a live child never authorizes drain"
            );
            child
                .0
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"finish\n")
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while identity.drain_after_exit().is_err() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(members(&identity, Path::new("/proc"), true)
                .unwrap()
                .is_empty());
            assert!(
                control.0.try_wait().unwrap().is_none(),
                "unrelated same-UID process was signaled"
            );
            assert_eq!(child.0.wait().unwrap().code(), Some(7));
            assert_eq!(identity.activity(), Activity::Unknown);
            assert!(
                identity.drain_after_exit().is_err(),
                "reaped numeric PID has no cleanup authority"
            );
        }
        #[test]
        fn idle_identity_mismatch_and_completed_child_are_distinct() {
            let mut child = shell("printf 'ready\\n'; read answer");
            let mut identity = PtySession::capture(child.0.id() as i32);
            assert_eq!(identity.activity(), Activity::Idle);
            identity.start = identity.start.map(|start| start + 1);
            assert_eq!(identity.activity(), Activity::Unknown);
            identity.start = identity.start.map(|start| start - 1);
            child
                .0
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"finish\n")
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while identity.drain_after_exit().is_err() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(child.0.wait().unwrap().success());
        }
        #[test]
        fn stat_parser_uses_last_parenthesis_and_rejects_truncation() {
            let text = format!("77 (a ) tricky) S 1 77 77 {} 900", "0 ".repeat(15));
            let state = parse(&text).unwrap();
            assert_eq!(
                (state.pid, state.session, state.start, state.live),
                (77, 77, 900, true)
            );
            assert!(parse("77 (truncated) S 1").is_none());
        }

        #[test]
        fn malformed_oversized_and_unreadable_records_do_not_prove_idle() {
            let root = std::env::temp_dir().join(format!("eva-pty-proc-{}", uuid::Uuid::new_v4()));
            for pid in [77, 78] {
                std::fs::create_dir_all(root.join(pid.to_string())).unwrap();
            }
            let record =
                |pid, state| format!("{pid} (name) {state} 1 77 77 {} 900", "0 ".repeat(15));
            std::fs::write(root.join("77/stat"), record(77, 'S')).unwrap();
            let identity = PtySession {
                pid: 77,
                start: Some(900),
            };
            std::fs::write(root.join("78/stat"), record(78, 'Z')).unwrap();
            assert!(members(&identity, &root, false).unwrap().is_empty());
            std::fs::write(root.join("78/stat"), "truncated").unwrap();
            assert!(members(&identity, &root, false).is_err());
            std::fs::write(root.join("78/stat"), "x".repeat(MAX_STAT as usize + 1)).unwrap();
            assert!(members(&identity, &root, false).is_err());
            std::fs::remove_file(root.join("78/stat")).unwrap();
            std::fs::create_dir(root.join("78/stat")).unwrap();
            assert!(members(&identity, &root, false).is_err());
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    use super::*;
    pub(super) struct State {
        pub pid: i32,
        pub session: i32,
        pub start: u64,
    }
    pub(super) fn read_process(_: &std::path::Path, _: i32) -> io::Result<State> {
        Err(io::Error::other("PTY session observation unavailable"))
    }
    pub(super) fn members(
        session: &PtySession,
        _: &std::path::Path,
        _: bool,
    ) -> io::Result<Vec<State>> {
        let _ = (session.pid, session.start);
        Err(io::Error::other("PTY session observation unavailable"))
    }
    pub(super) fn drain(session: &PtySession) -> io::Result<()> {
        let _ = (session.pid, session.start);
        Ok(())
    }
}
