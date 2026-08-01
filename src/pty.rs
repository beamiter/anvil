//! Owned PTY: fork+exec a shell on a fresh pseudo-terminal, then stream its
//! output to the GTK main thread via an eventfd-signaled mpsc channel. Ported
//! from jterm4 — the block view drives its own PTY (rather than vte4's) so it can
//! intercept the raw stream for OSC 133 block detection.

use crate::child_env;
use crate::process::{ChildLifecycle, EscalationPolicy, ReapOwner};
use crate::pty_input;
use gtk::glib;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{self, ForkResult, Pid};
use relm4::gtk;
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};

enum PtyMsg {
    Data(Vec<u8>),
    Exit(i32),
}

pub struct OwnedPty {
    master: Arc<std::sync::Mutex<Option<OwnedFd>>>,
    /// A bounded, non-blocking userspace queue keeps kernel PTY backpressure
    /// away from GTK without letting a query storm grow memory indefinitely.
    input_tx: FdWriter,
    child_lifecycle: Arc<ChildLifecycle>,
    /// Host-bridged shells live in another PID namespace, so their foreground
    /// process-group number cannot be compared with this local child PID.
    foreground_identity_available: bool,
    reader_cancelled: Arc<AtomicBool>,
    /// The shared outgoing-write filter. Holds the "a frame this app opened is
    /// still open" state across `write_bytes` calls, so a body that arrives as
    /// its own write still has paste markers removed instead of being waved
    /// through. A mutex rather than an atomic because the guard's decision and
    /// its state update have to be one step per chunk, in write order.
    input_guard: std::sync::Mutex<pty_input::InputGuard>,
    /// Mirrors the shell's DECSET 2004 state observed on the PTY output stream.
    /// At an interactive prompt this lets insertion-only multiline input be
    /// wrapped safely instead of executing one command per line.
    shell_bracketed_paste: Arc<AtomicBool>,
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
const FD_WRITER_MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
const FD_WRITER_MAX_MESSAGES: usize = 256;
const FD_WRITER_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

const BRACKETED_PASTE_ENABLE: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_DISABLE: &[u8] = b"\x1b[?2004l";

/// Write a complete byte slice to a blocking fd, retrying interrupted and
/// partial writes. This must run only on a background writer thread: a full
/// PTY buffer is allowed to backpressure that worker, never GTK's main loop.
pub(crate) fn write_all_fd(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let written = unsafe { libc::write(fd, data.as_ptr().cast::<libc::c_void>(), data.len()) };
        if written > 0 {
            data = &data[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "PTY write returned zero",
            ));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
    Ok(())
}

struct FdWriterQueue {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}

struct FdWriterShared {
    queue: Mutex<FdWriterQueue>,
    ready: Condvar,
    senders: AtomicUsize,
}

pub(crate) struct FdWriter {
    shared: Arc<FdWriterShared>,
}

#[derive(Debug)]
pub(crate) struct FdWriterSendError {
    len: usize,
    reason: &'static str,
}

impl FdWriterSendError {
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl std::fmt::Display for FdWriterSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({} bytes)", self.reason, self.len)
    }
}

impl Clone for FdWriter {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for FdWriter {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut queue = self
                .shared
                .queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.closed = true;
            self.shared.ready.notify_one();
        }
    }
}

impl FdWriter {
    pub(crate) fn send(&self, data: Vec<u8>) -> Result<(), FdWriterSendError> {
        let len = data.len();
        if len == 0 {
            return Ok(());
        }
        if len > FD_WRITER_MAX_MESSAGE_BYTES {
            return Err(FdWriterSendError {
                len,
                reason: "PTY write exceeds the per-message safety limit",
            });
        }
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.closed {
            return Err(FdWriterSendError {
                len,
                reason: "PTY writer is closed",
            });
        }
        if queue.messages.len() >= FD_WRITER_MAX_MESSAGES
            || queue.bytes.saturating_add(len) > FD_WRITER_MAX_QUEUED_BYTES
        {
            return Err(FdWriterSendError {
                len,
                reason: "PTY writer queue safety limit reached",
            });
        }
        queue.bytes += len;
        queue.messages.push_back(data);
        drop(queue);
        self.shared.ready.notify_one();
        Ok(())
    }
}

/// Start an ordered, bounded, non-UI-blocking writer for a duplicated PTY
/// descriptor. Overload rejects a whole message instead of retaining
/// unbounded input or partially enqueueing it.
pub(crate) fn spawn_fd_writer(fd: OwnedFd, thread_name: &'static str) -> io::Result<FdWriter> {
    let shared = Arc::new(FdWriterShared {
        queue: Mutex::new(FdWriterQueue {
            messages: VecDeque::new(),
            bytes: 0,
            closed: false,
        }),
        ready: Condvar::new(),
        senders: AtomicUsize::new(1),
    });
    let worker_shared = shared.clone();
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || loop {
            let data = {
                let mut queue = worker_shared
                    .queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                while queue.messages.is_empty() && !queue.closed {
                    queue = worker_shared
                        .ready
                        .wait(queue)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                let Some(data) = queue.messages.pop_front() else {
                    break;
                };
                queue.bytes = queue.bytes.saturating_sub(data.len());
                data
            };
            if let Err(err) = write_all_fd(fd.as_raw_fd(), &data) {
                log::warn!("{thread_name} stopped: {err}");
                let mut queue = worker_shared
                    .queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                queue.closed = true;
                queue.messages.clear();
                queue.bytes = 0;
                break;
            }
        })?;
    Ok(FdWriter { shared })
}

/// Observe DECSET/DECRST 2004 in a stream that may split an escape sequence
/// across read chunks. `tail` retains only the bytes needed to bridge the next
/// boundary; the returned bool is the mode after processing `data`.
fn observe_bracketed_paste_mode(current: bool, tail: &mut Vec<u8>, data: &[u8]) -> bool {
    let mut combined = Vec::with_capacity(tail.len() + data.len());
    combined.extend_from_slice(tail);
    combined.extend_from_slice(data);

    let mut enabled = current;
    let mut index = 0usize;
    while index < combined.len() {
        let rest = &combined[index..];
        if rest.starts_with(BRACKETED_PASTE_ENABLE) {
            enabled = true;
            index += BRACKETED_PASTE_ENABLE.len();
        } else if rest.starts_with(BRACKETED_PASTE_DISABLE) {
            enabled = false;
            index += BRACKETED_PASTE_DISABLE.len();
        } else {
            index += 1;
        }
    }

    let bridge_len = BRACKETED_PASTE_ENABLE
        .len()
        .max(BRACKETED_PASTE_DISABLE.len())
        .saturating_sub(1);
    let keep_from = combined.len().saturating_sub(bridge_len);
    tail.clear();
    tail.extend_from_slice(&combined[keep_from..]);
    enabled
}

/// jterm1's child-environment policy for a directly exec'd shell.
///
/// `less_default` is the one opinion this repo asserts: `LESS=R` keeps colored
/// git output, keeps the interactive pager even for a short `git log`, and lets
/// less use the alternate screen so pager content stays ephemeral. (Git would
/// otherwise default it to `FRX`, where `F` quits for short output and `X`
/// disables the alternate screen.) Locale rewriting and `LS_COLORS` stay off:
/// jterm1 draws UTF-8 either way and has never set colour defaults, so turning
/// them on here would override variables the user chose deliberately.
fn child_environment_options() -> child_env::ChildEnv<'static> {
    child_env::ChildEnv {
        less_default: Some("R"),
        ..child_env::ChildEnv::from_identity()
    }
}

/// jterm1's policy for the PTY boundary net.
///
/// `strip_controls` stays off here: the boundary sees every keystroke and every
/// escape sequence this app answers a query with, and stripping controls there
/// would eat the input it exists to protect. Marker removal is unconditional
/// inside [`pty_input::InputGuard`] regardless of this policy — that is the part
/// that stops an injected `ESC[201~` from ending a frame early. `FirstLineOnly`
/// preserves jterm1's existing fallback for unframed multiline input.
fn boundary_policy() -> pty_input::PastePolicy {
    pty_input::PastePolicy {
        unbracketed_multiline: pty_input::UnbracketedMultiline::FirstLineOnly,
        strip_controls: false,
        submit: false,
    }
}

/// Apply the shared PTY boundary policy and materialize the result for the
/// writer queue. The current exact-pinned core handles marker removal and the
/// independent multiline policy in one pass; feeding an owned rewrite through
/// the guard a second time would incorrectly reinterpret its generated frame.
fn filter_boundary_input(
    guard: &mut pty_input::InputGuard,
    data: &[u8],
    modes: pty_input::PasteModes,
    policy: pty_input::PastePolicy,
) -> Vec<u8> {
    guard.filter(data, modes, policy).into_owned()
}

/// Kill and reap a freshly forked child that no [`ChildLifecycle`] could be
/// built for.
///
/// This is the one window where a raw `kill` is still the right tool: the
/// child has just been forked and nobody has reaped it, so its pid cannot have
/// been reused, and the very failure being handled is that
/// `ChildLifecycle::new` could not open a reference to it (a full descriptor
/// table, in practice). Every other teardown path goes through the lifecycle.
fn kill_and_reap_unreferenced_child(child: Pid) {
    let pid = child.as_raw();
    unsafe {
        if libc::getpgid(pid) == pid {
            libc::kill(-pid, libc::SIGKILL);
        }
        libc::kill(pid, libc::SIGKILL);
    }
    while let Err(nix::errno::Errno::EINTR) = nix::sys::wait::waitpid(child, None) {}
}

impl OwnedPty {
    fn close_master_fd(&self) {
        if let Ok(mut guard) = self.master.lock() {
            guard.take();
        }
    }

    pub fn spawn(argv: &[&str], cwd: Option<&str>, env_extra: &[(&str, &str)]) -> io::Result<Self> {
        let argv_owned: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
        let host_bridge = crate::host::is_flatpak();
        let effective_cwd = cwd.filter(|directory| {
            let usable = crate::host::working_directory_available(directory);
            if !usable {
                log::warn!("PTY working directory is unavailable; using the application directory");
            }
            usable
        });
        let executable_argv = crate::host::wrap_argv(&argv_owned, effective_cwd, env_extra);
        if executable_argv.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty PTY argv",
            ));
        }

        // Prepare every allocation that exec needs before fork. GTK processes
        // are multi-threaded; panicking or allocating Rust strings in the child
        // can deadlock on a lock held by a thread that no longer exists there.
        let c_argv = executable_argv
            .iter()
            .map(|argument| {
                CString::new(argument.as_bytes()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "PTY command contains an embedded NUL byte",
                    )
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let child_cwd = if host_bridge { None } else { effective_cwd };
        let c_cwd = child_cwd
            .map(|directory| {
                CString::new(directory.as_bytes()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "PTY working directory contains an embedded NUL byte",
                    )
                })
            })
            .transpose()?;

        // Block mode is jterm1's default and libvte never spawns the child here
        // (it is handed a foreign PTY master), so nothing else injects the
        // terminal identity: before this, the child got `TERM` and no
        // `COLORTERM`, and bat/delta/lazygit fell back to 256 colours.
        //
        // `env_extra` goes in as the overlay's `extra` when we exec directly. In
        // Flatpak mode `crate::host::wrap_argv` has already turned it into
        // `flatpak-spawn --env=` arguments for the host child, so passing it here
        // as well would set it in the sandbox process too.
        let bridged_extra: &[(&str, &str)] = if host_bridge { &[] } else { env_extra };
        let c_environment = child_env::envp(&child_environment_options(), bridged_extra)?;
        // Resolve against the PATH the *child* will run with, which the overlay
        // above never rewrites, so this cannot silently search a different PATH
        // than the one the shell inherits.
        let child_path = std::env::var_os("PATH");
        let executable_path =
            crate::host::resolve_executable(&executable_argv[0], child_path.as_deref(), child_cwd)?;
        let c_executable = CString::new(executable_path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "resolved PTY executable contains an embedded NUL byte",
            )
        })?;
        // `nix::unistd::execvpe` constructs these pointer arrays internally.
        // Build them here instead so the post-fork child does not allocate.
        let mut c_argv_ptrs = c_argv
            .iter()
            .map(|argument| argument.as_ptr())
            .collect::<Vec<_>>();
        c_argv_ptrs.push(std::ptr::null());
        let mut c_environment_ptrs = c_environment
            .iter()
            .map(|entry| entry.as_ptr())
            .collect::<Vec<_>>();
        c_environment_ptrs.push(std::ptr::null());

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
                let slave_fd = slave.into_raw_fd();
                unsafe {
                    if libc::setsid() < 0 {
                        libc::_exit(126);
                    }
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0
                        || libc::dup2(slave_fd, 0) < 0
                        || libc::dup2(slave_fd, 1) < 0
                        || libc::dup2(slave_fd, 2) < 0
                    {
                        libc::_exit(126);
                    }
                    // dup2(oldfd, oldfd) is a no-op and therefore does not
                    // clear FD_CLOEXEC. Explicitly keep all three standard
                    // streams alive across exec even if openpty returned one
                    // of those descriptor numbers as the slave.
                    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags < 0
                            || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                        {
                            libc::_exit(126);
                        }
                    }
                    if slave_fd > libc::STDERR_FILENO {
                        libc::close(slave_fd);
                    }
                }

                if let Some(directory) = c_cwd.as_ref() {
                    unsafe {
                        // The directory was checked before fork. If it vanished
                        // in the small race window, retain the application
                        // directory instead of killing the restored pane.
                        libc::chdir(directory.as_ptr());
                    }
                }
                unsafe {
                    libc::execve(
                        c_executable.as_ptr(),
                        c_argv_ptrs.as_ptr(),
                        c_environment_ptrs.as_ptr(),
                    );
                    libc::_exit(127);
                }
            }
            Ok(ForkResult::Parent { child }) => {
                drop(slave);
                // Take ownership of the child's termination path first: from
                // here on every failure below can tear it down through the
                // lifecycle instead of an unverified pid.
                let child_lifecycle = match ChildLifecycle::new(child.as_raw(), ReapOwner::Ours) {
                    Ok(lifecycle) => lifecycle,
                    Err(error) => {
                        drop(master);
                        kill_and_reap_unreferenced_child(child);
                        return Err(error);
                    }
                };
                let writer_fd = match master.try_clone() {
                    Ok(fd) => fd,
                    Err(error) => {
                        drop(master);
                        child_lifecycle.force_kill_and_reap();
                        return Err(error);
                    }
                };
                let input_tx = match spawn_fd_writer(writer_fd, "jterm1-pty-writer") {
                    Ok(tx) => tx,
                    Err(error) => {
                        drop(master);
                        child_lifecycle.force_kill_and_reap();
                        return Err(error);
                    }
                };
                Ok(OwnedPty {
                    master: Arc::new(std::sync::Mutex::new(Some(master))),
                    input_tx,
                    child_lifecycle,
                    foreground_identity_available: !host_bridge,
                    reader_cancelled: Arc::new(AtomicBool::new(false)),
                    input_guard: std::sync::Mutex::new(pty_input::InputGuard::new()),
                    shell_bracketed_paste: Arc::new(AtomicBool::new(false)),
                })
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }

    pub fn pid_i32(&self) -> i32 {
        self.child_lifecycle.pid()
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

    /// Whether the PTY child/shell owns the foreground process group. `None`
    /// means the comparison is unavailable (notably Flatpak host bridging).
    pub(crate) fn shell_is_foreground(&self) -> Option<bool> {
        if !self.foreground_identity_available {
            return None;
        }
        let fd = self.master_fd_raw();
        if fd < 0 {
            return None;
        }
        let foreground = unsafe { libc::tcgetpgrp(fd) };
        (foreground > 0).then(|| foreground == self.child_lifecycle.pid())
    }

    /// Filter one outgoing chunk and queue it.
    ///
    /// Every write to the shell goes through here, which is what covers this
    /// repo's ad-hoc writers — the history palette's raw command bytes, the
    /// queued startup command formatted with a bare trailing CR — that never see
    /// [`pty_input::encode_paste`].
    pub(crate) fn try_write_bytes(&self, data: &[u8]) -> Result<(), FdWriterSendError> {
        let modes = pty_input::PasteModes {
            // The guard does not track DECSET 2004; the reader thread below does.
            bracketed: self.shell_bracketed_paste.load(Ordering::Relaxed),
        };
        let filtered = match self.input_guard.lock() {
            Ok(mut guard) => filter_boundary_input(&mut guard, data, modes, boundary_policy()),
            Err(poisoned) => {
                // A poisoned guard means another thread panicked mid-filter. Its
                // frame state is unknowable, so start a fresh guard rather than
                // writing unfiltered bytes to a shell.
                let mut guard = poisoned.into_inner();
                *guard = pty_input::InputGuard::new();
                filter_boundary_input(&mut guard, data, modes, boundary_policy())
            }
        };

        if filtered.is_empty() {
            return Ok(());
        }
        self.input_tx.send(filtered)
    }

    pub fn write_bytes(&self, data: &[u8]) {
        if let Err(err) = self.try_write_bytes(data) {
            log::warn!("PTY input queue rejected {} byte(s): {err}", err.len());
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
        self.reader_cancelled.store(true, Ordering::Release);
        self.close_master_fd();
        self.child_lifecycle
            .terminate(EscalationPolicy::SESSION_DRAIN);
    }

    /// Spawn a background reader thread; deliver bounded data through a
    /// backpressured channel paced on the GLib main thread.
    pub fn start_reader<F, E>(&self, callback: F, on_exit: E) -> io::Result<()>
    where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        let reader_fd = match self
            .master
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().and_then(|fd| fd.try_clone().ok()))
        {
            Some(fd) => fd,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "PTY reader descriptor is unavailable",
                ))
            }
        };

        let child_lifecycle = Arc::clone(&self.child_lifecycle);
        let reader_cancelled = Arc::clone(&self.reader_cancelled);
        let (tx, rx) = mpsc::sync_channel::<PtyMsg>(PTY_QUEUE_CAPACITY);
        let shell_bracketed_paste = Arc::clone(&self.shell_bracketed_paste);

        self.start_reader_timed(
            reader_fd,
            child_lifecycle,
            reader_cancelled,
            tx,
            rx,
            callback,
            on_exit,
            shell_bracketed_paste,
        )
    }

    // The transport endpoints and the two one-shot callbacks have independent
    // ownership/lifetimes; a wrapper struct would add indirection without
    // reducing the unsafe FD boundary this helper centralizes.
    #[allow(clippy::too_many_arguments)]
    fn start_reader_timed<F, E>(
        &self,
        reader_fd: OwnedFd,
        child_lifecycle: Arc<ChildLifecycle>,
        reader_cancelled: Arc<AtomicBool>,
        tx: mpsc::SyncSender<PtyMsg>,
        rx: mpsc::Receiver<PtyMsg>,
        mut callback: F,
        on_exit: E,
        shell_bracketed_paste: Arc<AtomicBool>,
    ) -> io::Result<()>
    where
        F: FnMut(Vec<u8>) + 'static,
        E: FnOnce(i32) + 'static,
    {
        let reader = std::thread::Builder::new()
            .name("jterm1-pty-reader".to_string())
            .spawn(move || {
                // The reader owns a duplicated descriptor. It can never observe a
                // different file after the model closes and the kernel reuses the
                // original descriptor number.
                let mut file = std::fs::File::from(reader_fd);
                let fd = file.as_raw_fd();
                let mut buf = [0u8; 32 * 1024];
                let mut mode_tail =
                    Vec::with_capacity(BRACKETED_PASTE_ENABLE.len().saturating_sub(1));
                loop {
                    if reader_cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let mut ready = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let polled = unsafe { libc::poll(&mut ready, 1, 50) };
                    if polled < 0 {
                        if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        break;
                    }
                    if polled == 0 {
                        continue;
                    }
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            break;
                        }
                        Ok(n) => {
                            let mut combined = Vec::with_capacity(n + 4096);
                            combined.extend_from_slice(&buf[..n]);
                            coalesce_pending(fd, &mut file, &mut buf, &mut combined);
                            let mode = observe_bracketed_paste_mode(
                                shell_bracketed_paste.load(Ordering::Relaxed),
                                &mut mode_tail,
                                &combined,
                            );
                            shell_bracketed_paste.store(mode, Ordering::Relaxed);
                            if tx.send(PtyMsg::Data(combined)).is_err() {
                                break;
                            }
                        }
                    }
                }
                if reader_cancelled.load(Ordering::Acquire) {
                    // Dropping the sole producer disconnects the GLib receiver,
                    // which removes its source and releases all captured
                    // widgets/OwnedPty handles even if a descendant keeps the
                    // slave side open.
                    return;
                }
                reap_child(&child_lifecycle, &tx);
            });
        if let Err(error) = reader {
            self.close_master_fd();
            self.child_lifecycle.force_kill_and_reap();
            return Err(error);
        }

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
        Ok(())
    }
}

fn reap_child(child_lifecycle: &Arc<ChildLifecycle>, tx: &mpsc::SyncSender<PtyMsg>) {
    let started = std::time::Instant::now();
    let mut termination_requested = false;
    loop {
        if let Some(code) = child_lifecycle.poll_reap() {
            let _ = tx.send(PtyMsg::Exit(code));
            return;
        }
        // EOF normally precedes shell exit by only a few milliseconds. If an
        // unusual detached child keeps the pid alive, terminate it instead of
        // leaving the reader source and its captured widget tree resident.
        if !termination_requested && started.elapsed() >= std::time::Duration::from_secs(5) {
            log::warn!(
                "PTY reader reached EOF but child {} is still alive; terminating it",
                child_lifecycle.pid()
            );
            child_lifecycle.terminate(EscalationPolicy::SESSION_DRAIN);
            termination_requested = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
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
        self.reader_cancelled.store(true, Ordering::Release);
        self.close_master_fd();
        self.child_lifecycle
            .terminate(EscalationPolicy::SESSION_DRAIN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_rejects_nul_before_forking() {
        let error = OwnedPty::spawn(&["sh", "bad\0argument"], None, &[])
            .err()
            .expect("embedded NUL must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    /// The identity variables are `jterm_core::child_env`'s to get right (and its
    /// tests do); what is jterm1's is which of the optional policies it turns on.
    /// Block mode is the default and libvte never spawns the child here, so this
    /// overlay is the *only* thing that tells the shell what terminal it is in.
    #[test]
    fn the_child_environment_asserts_the_pager_and_nothing_else() {
        let options = child_environment_options();
        assert_eq!(options.less_default, Some("R"));
        assert!(
            !options.normalize_locale,
            "jterm1 draws UTF-8 either way; a deliberate LANG is the user's"
        );
        assert!(!options.color_defaults, "jterm1 has never set LS_COLORS");
        assert_eq!(
            options.vte_version,
            Some(child_env::EMULATED_VTE_VERSION),
            "distro vte.sh gates its OSC 7 cwd emitter on this, and block mode \
             learns the cwd from that emitter"
        );

        // COLORTERM is the fix this adoption carries: before it, a block-mode
        // child got TERM and nothing else, so bat/delta/lazygit fell back to 256
        // colours.
        let overlay = child_env::pairs(&options, &[("JSH_SESSION_ID", "7")]);
        let value = |name: &str| {
            overlay
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().to_string())
        };
        assert_eq!(value("COLORTERM").as_deref(), Some(child_env::COLORTERM));
        assert_eq!(value("TERM").as_deref(), Some(child_env::TERM));
        assert!(value("TERM_PROGRAM_VERSION").is_some());
        assert_eq!(value("JSH_SESSION_ID").as_deref(), Some("7"));
    }

    /// Replaces `executable_resolution_is_absolute_and_rejects_missing_commands`,
    /// which pinned the local `resolve_executable` this file donated to
    /// `jterm_core::host` (whose own tests cover the lookup rules, including the
    /// empty PATH entry resolving against the child's cwd). What is still
    /// jterm1's to guarantee is the wiring: resolution happens before `fork`, so
    /// the pane that asked gets the error instead of a child that exits 127.
    #[test]
    fn spawn_reports_an_unresolvable_command_before_forking() {
        let error = OwnedPty::spawn(&["jterm1-command-that-does-not-exist"], None, &[])
            .err()
            .expect("an unresolvable command must fail the spawn");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn background_writer_preserves_large_payload_and_order() {
        let (mut reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let tx = spawn_fd_writer(writer.into(), "jterm1-test-writer").unwrap();

        // Larger than a typical Unix socket buffer: the worker will encounter
        // kernel backpressure, while both queue sends must return immediately.
        let payload = vec![0x5a; 2 * 1024 * 1024];
        tx.send(payload.clone()).unwrap();
        tx.send(b"tail".to_vec()).unwrap();
        drop(tx);

        let mut received = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut received).unwrap();
        assert_eq!(received.len(), payload.len() + 4);
        assert_eq!(&received[..payload.len()], payload.as_slice());
        assert_eq!(&received[payload.len()..], b"tail");
    }

    #[test]
    fn background_writer_rejects_oversize_and_bounded_queue_overload() {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let tx = spawn_fd_writer(writer.into(), "jterm1-test-bounded-writer").unwrap();

        let oversized = vec![0_u8; FD_WRITER_MAX_MESSAGE_BYTES + 1];
        assert_eq!(
            tx.send(oversized).unwrap_err().len(),
            FD_WRITER_MAX_MESSAGE_BYTES + 1
        );

        let payload = vec![0x5a; FD_WRITER_MAX_QUEUED_BYTES];
        let mut rejected = false;
        for _ in 0..3 {
            if tx.send(payload.clone()).is_err() {
                rejected = true;
                break;
            }
        }
        assert!(
            rejected,
            "a blocked writer retained more than its queue budget"
        );
        drop(reader);
        drop(tx);
    }

    /// The four tests below replace the `sanitize_input_chunk` suite: the encoder
    /// is `jterm_core::pty_input::InputGuard` now, and what stays jterm1's is the
    /// policy this boundary hands it. `boundary_policy()` is exercised directly
    /// because `write_bytes` needs a live child to reach.
    fn boundary(bracketed: bool) -> (pty_input::PasteModes, pty_input::PastePolicy) {
        (pty_input::PasteModes { bracketed }, boundary_policy())
    }

    #[test]
    fn unframed_multiline_insert_falls_back_to_first_line_without_shell_support() {
        let (modes, policy) = boundary(false);
        let mut guard = pty_input::InputGuard::new();
        assert_eq!(
            &*guard.filter(b"echo first\necho second", modes, policy),
            b"echo first"
        );
        assert_eq!(
            &*guard.filter(b"echo first\r\necho second", modes, policy),
            b"echo first"
        );
    }

    #[test]
    fn shell_supported_multiline_insert_is_automatically_bracketed() {
        let (modes, policy) = boundary(true);
        let mut guard = pty_input::InputGuard::new();
        assert_eq!(
            &*guard.filter(b"echo first\necho second", modes, policy),
            b"\x1b[200~echo first\necho second\x1b[201~"
        );
        assert!(!guard.in_frame(), "a whole frame does not stay open");
    }

    /// Marker removal must not bypass the independent multiline policy.
    #[test]
    fn embedded_marker_cannot_bypass_either_multiline_policy_branch() {
        let payload = b"one\x1b[201~\ntwo\r";

        let (modes, policy) = boundary(true);
        let mut guard = pty_input::InputGuard::new();
        assert_eq!(
            filter_boundary_input(&mut guard, payload, modes, policy),
            b"\x1b[200~one\ntwo\x1b[201~\r"
        );

        let (modes, policy) = boundary(false);
        let mut guard = pty_input::InputGuard::new();
        assert_eq!(
            filter_boundary_input(&mut guard, payload, modes, policy),
            b"one"
        );
    }

    #[test]
    fn explicit_single_line_submission_is_unchanged() {
        let (modes, policy) = boundary(false);
        let mut guard = pty_input::InputGuard::new();
        assert_eq!(
            &*guard.filter(b"git status\r", modes, policy),
            b"git status\r"
        );
        assert_eq!(&*guard.filter(b"git status", modes, policy), b"git status");
    }

    /// The injection this boundary exists to stop, and the case the old
    /// `sanitize_input_chunk` got wrong: with a frame already open, the body was
    /// waved through, so a clipboard carrying `ESC[201~` closed the frame early
    /// and the bytes after it arrived as keystrokes the shell then executed.
    #[test]
    fn a_split_frames_body_cannot_close_the_frame_early() {
        let (modes, policy) = boundary(true);
        let mut guard = pty_input::InputGuard::new();

        assert_eq!(
            filter_boundary_input(&mut guard, pty_input::PASTE_START, modes, policy),
            pty_input::PASTE_START
        );
        assert!(guard.in_frame());

        let body = b"docs\x1b[201~\rrm -rf ~\r";
        let filtered = filter_boundary_input(&mut guard, body, modes, policy);
        assert_eq!(filtered, b"docs\rrm -rf ~\r");
        assert!(guard.in_frame(), "the caller has not closed the frame yet");

        assert_eq!(
            filter_boundary_input(&mut guard, pty_input::PASTE_END, modes, policy),
            pty_input::PASTE_END
        );
        assert!(!guard.in_frame());
    }

    /// jterm1 keeps control bytes at this boundary: every keystroke and every
    /// reply this app writes to a capability query passes through here.
    #[test]
    fn the_boundary_does_not_strip_control_bytes() {
        let (modes, policy) = boundary(false);
        let mut guard = pty_input::InputGuard::new();
        assert_eq!(&*guard.filter(b"\x1b[A", modes, policy), b"\x1b[A");
        assert_eq!(&*guard.filter(b"\x03", modes, policy), b"\x03");
    }

    #[test]
    fn observes_split_bracketed_paste_mode_sequences() {
        let mut tail = Vec::new();
        let enabled = observe_bracketed_paste_mode(false, &mut tail, b"prompt\x1b[?20");
        assert!(!enabled);
        let enabled = observe_bracketed_paste_mode(enabled, &mut tail, b"04h");
        assert!(enabled);

        let enabled = observe_bracketed_paste_mode(enabled, &mut tail, b"\x1b[?2004l");
        assert!(!enabled);
    }
}
