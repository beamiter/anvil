//! Owned PTY: fork+exec a shell on a fresh pseudo-terminal, then stream its
//! output to the GTK main thread via an eventfd-signaled mpsc channel. Ported
//! from jterm4 — the block view drives its own PTY (rather than vte4's) so it can
//! intercept the raw stream for OSC 133 block detection.

use crate::process::terminate_terminal_process;
use gtk4::glib;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{self, ForkResult, Pid};
use std::ffi::CString;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;

enum PtyMsg {
    Data(Vec<u8>),
    Exit(i32),
}

pub struct OwnedPty {
    master: std::sync::Arc<std::sync::Mutex<Option<OwnedFd>>>,
    pid: Pid,
}

/// Process exactly one bounded chunk per GLib dispatch. Even at idle priority,
/// one callback must return quickly enough for GTK to run input/layout sources.
/// Eight 256 KiB chunks used to keep the main thread inside VTE feeding for
/// long stretches, so mouse clicks appeared to do nothing while keyboard tab
/// shortcuts occasionally slipped through.
const MAX_MESSAGES_PER_DISPATCH: usize = 1;
/// Bound queued PTY data and let the kernel PTY apply normal backpressure to a
/// runaway producer. An unbounded channel can otherwise grow indefinitely while
/// GTK consumes output more slowly than the child writes it.
const PTY_QUEUE_CAPACITY: usize = 8;
/// Pace main-thread terminal feeding. A readiness source immediately becomes
/// ready again while a producer is chatty, so priority alone does not provide a
/// frame boundary. At 8 ms and 32 KiB chunks the cap is about 4 MiB/s per PTY,
/// while pointer and keyboard handling get a scheduling opportunity each tick.
const PTY_DISPATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

impl OwnedPty {
    fn close_master_fd(&self) {
        if let Ok(mut guard) = self.master.lock() {
            guard.take();
        }
    }

    pub fn spawn(argv: &[&str], cwd: Option<&str>, env_extra: &[(&str, &str)]) -> io::Result<Self> {
        let initial_size = nix::pty::Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let OpenptyResult { master, slave } =
            openpty(Some(&initial_size), None).map_err(io::Error::other)?;

        match unsafe { unistd::fork() } {
            Ok(ForkResult::Child) => {
                drop(master);
                let slave_fd = slave.as_raw_fd();
                unsafe {
                    if libc::setsid() < 0 {
                        eprintln!("setsid() failed: {}", std::io::Error::last_os_error());
                        std::process::exit(1);
                    }
                    libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
                    libc::dup2(slave_fd, 0);
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                }
                drop(slave);

                if let Some(dir) = cwd {
                    let _ = std::env::set_current_dir(dir);
                }
                for (key, val) in env_extra {
                    unsafe { std::env::set_var(key, val) };
                }
                unsafe { std::env::set_var("TERM", "xterm-256color") };

                let c_argv: Vec<CString> = argv.iter().map(|a| CString::new(*a).unwrap()).collect();
                let _ = unistd::execvp(&c_argv[0], &c_argv);
                std::process::exit(127);
            }
            Ok(ForkResult::Parent { child }) => {
                drop(slave);
                Ok(OwnedPty {
                    master: std::sync::Arc::new(std::sync::Mutex::new(Some(master))),
                    pid: child,
                })
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }

    pub fn pid_i32(&self) -> i32 {
        self.pid.as_raw()
    }

    /// Raw master-side fd, or -1 if already closed. Borrowed for the lifetime of
    /// the PTY; used only for `tcgetpgrp`-style foreground-process probing.
    pub fn master_fd_raw(&self) -> i32 {
        self.master
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|fd| fd.as_raw_fd()))
            .unwrap_or(-1)
    }

    pub fn write_bytes(&self, data: &[u8]) {
        if let Ok(guard) = self.master.lock() {
            if let Some(fd) = guard.as_ref() {
                let raw = fd.as_raw_fd();
                unsafe {
                    libc::write(raw, data.as_ptr() as *const libc::c_void, data.len());
                }
            }
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Ok(guard) = self.master.lock() {
            if let Some(fd) = guard.as_ref() {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ, &ws);
                }
            }
        }
    }

    pub fn kill(&self) {
        self.close_master_fd();
        terminate_terminal_process(self.pid.as_raw());
    }

    /// Spawn a background reader thread; deliver bounded data through a
    /// backpressured channel paced on the GLib main thread.
    pub fn start_reader<F, E>(&self, callback: F, on_exit: E)
    where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        let fd = match self
            .master
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|fd| fd.as_raw_fd()))
        {
            Some(fd) => fd,
            None => return,
        };

        let child_pid = self.pid;
        let (tx, rx) = mpsc::sync_channel::<PtyMsg>(PTY_QUEUE_CAPACITY);

        self.start_reader_timed(fd, child_pid, tx, rx, callback, on_exit);
    }

    fn start_reader_timed<F, E>(
        &self,
        fd: RawFd,
        child_pid: Pid,
        tx: mpsc::SyncSender<PtyMsg>,
        rx: mpsc::Receiver<PtyMsg>,
        mut callback: F,
        on_exit: E,
    ) where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        std::thread::spawn(move || {
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut buf = [0u8; 32 * 1024];
            loop {
                match file.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        std::mem::forget(file);
                        break;
                    }
                    Ok(n) => {
                        let mut combined = Vec::with_capacity(n + 4096);
                        combined.extend_from_slice(&buf[..n]);
                        coalesce_pending(fd, &mut file, &mut buf, &mut combined);
                        if tx.send(PtyMsg::Data(combined)).is_err() {
                            std::mem::forget(file);
                            break;
                        }
                    }
                }
            }
            reap_child(child_pid, &tx);
        });

        let on_exit = std::cell::Cell::new(Some(on_exit));
        let rx = std::cell::RefCell::new(rx);

        glib::timeout_add_local_full(
            PTY_DISPATCH_INTERVAL,
            glib::Priority::DEFAULT_IDLE,
            move || {
                let mut processed = 0usize;
                loop {
                    match rx.borrow().try_recv() {
                        Ok(PtyMsg::Data(data)) => {
                            callback(data);
                            processed += 1;
                            if processed >= MAX_MESSAGES_PER_DISPATCH {
                                return glib::ControlFlow::Continue;
                            }
                        }
                        Ok(PtyMsg::Exit(code)) => {
                            if let Some(f) = on_exit.take() {
                                f(code);
                            }
                            return glib::ControlFlow::Break;
                        }
                        Err(mpsc::TryRecvError::Empty) => {
                            return glib::ControlFlow::Continue;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            return glib::ControlFlow::Break;
                        }
                    }
                }
            },
        );
    }
}

fn reap_child(child_pid: Pid, tx: &mpsc::SyncSender<PtyMsg>) {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    let max_wait_secs = 5;
    for _ in 0..(max_wait_secs * 10) {
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                let _ = tx.send(PtyMsg::Exit(code));
                return;
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                let _ = tx.send(PtyMsg::Exit(128 + sig as i32));
                return;
            }
            Err(_) | Ok(_) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    match waitpid(child_pid, None) {
        Ok(WaitStatus::Exited(_, code)) => {
            let _ = tx.send(PtyMsg::Exit(code));
        }
        Ok(WaitStatus::Signaled(_, sig, _)) => {
            let _ = tx.send(PtyMsg::Exit(128 + sig as i32));
        }
        _ => {
            let _ = tx.send(PtyMsg::Exit(1));
        }
    }
}

/// Briefly poll the PTY master for more bytes already on the wire and append
/// them onto `combined`. Caps each delivered chunk at 32 KiB so even a steady
/// firehose cannot create a single long-running GTK callback; the 1ms timeout is a tiny
/// fraction of a 60Hz frame budget but enough to merge clear+repaint pairs
/// that one program emitted in a single render.
fn coalesce_pending(fd: RawFd, file: &mut std::fs::File, buf: &mut [u8], combined: &mut Vec<u8>) {
    const MAX_BYTES: usize = 32 * 1024;
    const MAX_FOLLOWUP_READS: u32 = 8;
    let mut follow_ups = 0u32;
    while combined.len() < MAX_BYTES && follow_ups < MAX_FOLLOWUP_READS {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd as *mut _, 1, 1) };
        if r <= 0 || (pfd.revents & libc::POLLIN) == 0 {
            break;
        }
        let remaining = MAX_BYTES - combined.len();
        let read_len = remaining.min(buf.len());
        match file.read(&mut buf[..read_len]) {
            Ok(0) | Err(_) => break,
            Ok(m) => combined.extend_from_slice(&buf[..m]),
        }
        follow_ups += 1;
    }
}

impl Drop for OwnedPty {
    fn drop(&mut self) {
        self.close_master_fd();
        terminate_terminal_process(self.pid.as_raw());
    }
}
