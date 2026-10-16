//! PTY allocation. Linux creates both descriptors close-on-exec atomically:
//! unrelated Rust, GLib and VTE spawns need not cooperate with a fork mutex.

use nix::libc;
use nix::pty::{OpenptyResult, Winsize};
use std::io;
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};

#[cfg(target_os = "linux")]
fn syscall(stage: &'static str, call: impl FnOnce() -> libc::c_int) -> io::Result<libc::c_int> {
    #[cfg(test)]
    tests::before_syscall(stage)?;
    #[cfg(not(test))]
    let _ = stage;
    let result = call();
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Keep default kernel termios, as with `openpty(size, None)`. O_NOCTTY prevents
/// either open from acquiring a controlling terminal; the child explicitly
/// performs setsid/TIOCSCTTY and admits only its standard streams later.
#[cfg(target_os = "linux")]
pub(crate) fn openpty_cloexec(size: Option<&Winsize>) -> io::Result<OpenptyResult> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: no pointers are passed; a successful call returns a fresh fd.
    let master = syscall("posix_openpt", || unsafe { libc::posix_openpt(flags) })?;
    // Take ownership before any subsequent fallible operation.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: master is live and owned for all calls below.
    syscall("grantpt", || unsafe { libc::grantpt(master.as_raw_fd()) })?;
    syscall("unlockpt", || unsafe { libc::unlockpt(master.as_raw_fd()) })?;

    // Linux devpts names fit here (a numeric PTY index). Unlike ptsname, the
    // reentrant variant does not share a buffer with concurrent pane creation.
    let mut name = [0 as libc::c_char; 128];
    // ptsname_r returns an errno value, rather than the usual -1/errno pair.
    let error = syscall("ptsname_r", || unsafe {
        libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len())
    })?;
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    // SAFETY: successful ptsname_r wrote a NUL-terminated path in name. The
    // slave, like the master, is CLOEXEC from the instant it enters the fd table.
    let slave = syscall("open_slave", || unsafe { libc::open(name.as_ptr(), flags) })?;
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    if let Some(size) = size {
        // SAFETY: slave is live, and size points to a valid Winsize for the call.
        syscall("winsize", || unsafe {
            libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, size)
        })?;
    }
    Ok(OpenptyResult { master, slave })
}

/// The existing portable fallback prevents persistent inheritance, but cannot
/// close the openpty-to-fcntl race. Atomic allocation is guaranteed on Linux.
#[cfg(not(target_os = "linux"))]
pub(crate) fn openpty_cloexec(size: Option<&Winsize>) -> io::Result<OpenptyResult> {
    let pair = nix::pty::openpty(size, None).map_err(io::Error::other)?;
    for fd in [&pair.master, &pair.slave] {
        // SAFETY: both descriptors remain owned by pair, including on errors.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pair)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::process::Command;
    use std::sync::{Arc, Barrier};

    type Hook = Box<dyn FnMut(&str) -> io::Result<()>>;
    thread_local! { static HOOK: RefCell<Option<Hook>> = RefCell::new(None); }

    pub(super) fn before_syscall(stage: &str) -> io::Result<()> {
        HOOK.with(|hook| match hook.borrow_mut().as_mut() {
            Some(hook) => hook(stage),
            None => Ok(()),
        })
    }

    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            HOOK.with(|hook| *hook.borrow_mut() = None);
        }
    }
    fn with_hook(hook: impl FnMut(&str) -> io::Result<()> + 'static) -> HookGuard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        HookGuard
    }

    fn open_fds() -> Vec<i32> {
        let mut fds: Vec<_> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .parse::<i32>()
                    .unwrap()
            })
            .collect();
        // read_dir's own fd has closed now; do not count it.
        fds.retain(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0);
        fds.sort_unstable();
        fds
    }

    // Descriptor inventories are process-global. Run each test in its own
    // exact-test process, even when the application suite runs in parallel.
    // Preserve any descriptors inherited from the outer runner as an explicit
    // whitelist; no descriptor allocated by this isolated test is admitted.
    fn isolated(name: &str) -> bool {
        let full_name = format!("pty_pair::tests::{name}");
        if std::env::var("ANVIL_PTY_ISOLATED").as_deref() == Ok(full_name.as_str()) {
            let allowed = open_fds()
                .into_iter()
                .filter(|fd| (unsafe { libc::fcntl(*fd, libc::F_GETFD) }) & libc::FD_CLOEXEC == 0)
                .map(|fd| fd.to_string())
                .collect::<Vec<_>>()
                .join(",");
            std::env::set_var("ANVIL_PTY_ALLOWED_FDS", allowed);
            return false;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &full_name, "--nocapture", "--test-threads=1"])
            .env("ANVIL_PTY_ISOLATED", &full_name)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        true
    }

    #[test]
    fn exec_fd_probe() {
        if std::env::var_os("ANVIL_PTY_EXEC_PROBE").is_none() {
            return;
        }
        let allowed = std::env::var("ANVIL_PTY_ALLOWED_FDS").unwrap_or_else(|_| "0,1,2".into());
        let allowed = allowed
            .split(',')
            .map(|fd| fd.parse::<i32>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(open_fds(), allowed, "exec inherited an unapproved fd");
    }

    fn probe_exec() -> std::process::Output {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pty_pair::tests::exec_fd_probe", "--nocapture"])
            .env("ANVIL_PTY_EXEC_PROBE", "1")
            .output()
            .unwrap()
    }

    fn assert_clean_exec() {
        let output = probe_exec();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "probe did not run"
        );
    }

    #[test]
    fn real_exec_at_every_allocation_boundary_has_only_stdio() {
        if isolated("real_exec_at_every_allocation_boundary_has_only_stdio") {
            return;
        }
        let _guard = with_hook(|_| {
            std::thread::spawn(assert_clean_exec).join().unwrap();
            Ok(())
        });
        let size = Winsize {
            ws_row: 41,
            ws_col: 121,
            ws_xpixel: 3,
            ws_ypixel: 4,
        };
        let pair = openpty_cloexec(Some(&size)).unwrap();
        assert_clean_exec();
        for fd in [&pair.master, &pair.slave] {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn staged_failures_close_every_owned_descriptor() {
        if isolated("staged_failures_close_every_owned_descriptor") {
            return;
        }
        for stage in [
            "posix_openpt",
            "grantpt",
            "unlockpt",
            "ptsname_r",
            "open_slave",
            "winsize",
        ] {
            let before = open_fds();
            let _guard = with_hook(move |current| {
                if current == stage {
                    Err(io::Error::from_raw_os_error(libc::EIO))
                } else {
                    Ok(())
                }
            });
            let size = Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                openpty_cloexec(Some(&size)).unwrap_err().raw_os_error(),
                Some(libc::EIO)
            );
            assert_eq!(open_fds(), before, "leak on {stage}");
        }
    }

    #[test]
    fn matches_openpty_default_termios_line_discipline_and_size() {
        if isolated("matches_openpty_default_termios_line_discipline_and_size") {
            return;
        }
        let size = Winsize {
            ws_row: 41,
            ws_col: 121,
            ws_xpixel: 300,
            ws_ypixel: 400,
        };
        let expected = nix::pty::openpty(Some(&size), None).unwrap();
        let actual = openpty_cloexec(Some(&size)).unwrap();
        let expected_attrs = nix::sys::termios::tcgetattr(&expected.slave).unwrap();
        let actual_attrs = nix::sys::termios::tcgetattr(&actual.slave).unwrap();
        assert_eq!(actual_attrs, expected_attrs);
        for fd in [&actual.master, &actual.slave] {
            let mut got = Winsize {
                ws_row: 0,
                ws_col: 0,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut got) },
                0
            );
            assert_eq!(
                (got.ws_row, got.ws_col, got.ws_xpixel, got.ws_ypixel),
                (41, 121, 300, 400)
            );
        }
        let mut expected_ld = -1;
        let mut actual_ld = -1;
        assert_eq!(
            unsafe { libc::ioctl(expected.slave.as_raw_fd(), libc::TIOCGETD, &mut expected_ld) },
            0
        );
        assert_eq!(
            unsafe { libc::ioctl(actual.slave.as_raw_fd(), libc::TIOCGETD, &mut actual_ld) },
            0
        );
        assert_eq!(actual_ld, expected_ld);
    }

    #[test]
    fn concurrent_pane_allocation_and_real_exec_do_not_leak_fds() {
        if isolated("concurrent_pane_allocation_and_real_exec_do_not_leak_fds") {
            return;
        }
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..128 {
                    let pair = openpty_cloexec(None).unwrap();
                    std::thread::yield_now();
                    drop(pair);
                }
            }));
        }
        barrier.wait();
        for _ in 0..64 {
            assert_clean_exec();
        }
        for worker in workers {
            worker.join().unwrap();
        }
    }
    #[test]
    fn legacy_openpty_gap_is_visible_to_real_exec() {
        if isolated("legacy_openpty_gap_is_visible_to_real_exec") {
            return;
        }
        // This is the exact old allocation-to-fcntl boundary: both real PTY
        // descriptors exist, and a different thread can exec before marking.
        let pair = nix::pty::openpty(None, None).unwrap();
        let output = std::thread::spawn(probe_exec).join().unwrap();
        assert!(!output.status.success(), "legacy gap was not detected");
        assert!(String::from_utf8_lossy(&output.stderr).contains("exec inherited an unapproved fd"));
        for fd in [&pair.master, &pair.slave] {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) },
                0
            );
        }
        assert_clean_exec();
    }

    #[test]
    fn pair_supports_child_controlling_terminal_and_stdio() {
        if isolated("pair_supports_child_controlling_terminal_and_stdio") {
            return;
        }
        use std::os::unix::process::CommandExt;
        let size = Winsize {
            ws_row: 41,
            ws_col: 121,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pair = openpty_cloexec(Some(&size)).unwrap();
        let slave = pair.slave.as_raw_fd();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "test -t 0 && test -t 1 && test -t 2 && test \"$(stty size < /dev/tty)\" = \"41 121\"",
        ]);
        // No allocations or synchronization in this post-fork closure. This
        // exercises the same controlling-terminal contract as OwnedPty.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(slave, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for fd in 0..=2 {
                    if libc::dup2(slave, fd) < 0 || libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        assert!(command.status().unwrap().success());
    }
}
