//! Owned PTY: fork+exec a shell on a fresh pseudo-terminal, then stream its
//! output to the GTK main thread via an eventfd-signaled mpsc channel. Ported
//! from forge — the block view drives its own PTY (rather than vte4's) so it can
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
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
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
    /// Linux wakeup for a reader blocked on an idle PTY. `None` means eventfd
    /// creation failed (or this is not Linux), so the reader retains its 50 ms
    /// cancellation polling fallback.
    reader_cancel_eventfd: Option<Arc<OwnedFd>>,
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
    /// One-shot secret delivered to the interactive shell through an inherited
    /// pipe descriptor. The token itself is never placed in argv or the
    /// environment, and the bundled integration closes the descriptor before
    /// any user command can inherit it.
    shell_integration_token: Option<String>,
    /// Slave end of the bare PTY pair used by dispatch tests. Holding it open
    /// keeps protocol replies writable and gives tests a drain point.
    #[cfg(test)]
    test_slave: Option<OwnedFd>,
    /// Recorded foreground answer for a bare test PTY, which has no session to
    /// answer the real `tcgetpgrp` probe.
    #[cfg(test)]
    test_foreground: Option<bool>,
}

#[cfg(target_os = "linux")]
fn move_fd_above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: the source is an owned, live descriptor. The duplicate is the
    // first free descriptor at or above 3 and remains CLOEXEC until the child
    // explicitly admits only the read end across exec.
    let duplicated = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new descriptor owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

#[cfg(target_os = "linux")]
fn shell_token_channel() -> io::Result<Option<(String, OwnedFd, OwnedFd)>> {
    let mut random = [0_u8; 16];
    // SAFETY: `random` is writable for the exact supplied length.
    let read = unsafe {
        libc::getrandom(
            random.as_mut_ptr().cast(),
            random.len(),
            libc::GRND_NONBLOCK,
        )
    };
    if read != random.len() as isize {
        // Entropy temporarily being unavailable disables Agent execution; it
        // must never fall back to a predictable token.
        return Ok(None);
    }
    let mut token = String::with_capacity(random.len() * 2);
    for byte in random {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }

    let mut fds = [-1; 2];
    // SAFETY: `fds` points to two writable integers. Both ends start CLOEXEC;
    // only the child-side read end is deliberately opened across exec later.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two fresh descriptors. Take ownership of both
    // before either fallible duplicate, so an error cannot leak the other end.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let read_fd = move_fd_above_stdio(read_fd)?;
    let write_fd = move_fd_above_stdio(write_fd)?;
    Ok(Some((token, read_fd, write_fd)))
}

#[cfg(not(target_os = "linux"))]
fn shell_token_channel() -> io::Result<Option<(String, OwnedFd, OwnedFd)>> {
    Ok(None)
}

// Raw GLib FFI for g_unix_fd_add_full (not exposed by glib-rs 0.22).
// This source replaces the permanent 8 ms polling timer on Linux: GTK sleeps
// while the PTY is idle and is woken only when the reader enqueues a message.
#[cfg(target_os = "linux")]
extern "C" {
    fn g_unix_fd_add_full(
        priority: i32,
        fd: i32,
        condition: u32,
        function: extern "C" fn(fd: i32, condition: u32, user_data: *mut std::ffi::c_void) -> i32,
        user_data: *mut std::ffi::c_void,
        notify: extern "C" fn(data: *mut std::ffi::c_void),
    ) -> u32;
}

#[cfg(target_os = "linux")]
const G_IO_IN: u32 = 1;
#[cfg(target_os = "linux")]
const G_PRIORITY_DEFAULT_IDLE: i32 = 200;

#[cfg(target_os = "linux")]
struct FdWatchData<F: FnMut() -> bool> {
    callback: F,
}

#[cfg(target_os = "linux")]
extern "C" fn fd_watch_callback<F: FnMut() -> bool>(
    _fd: i32,
    _condition: u32,
    user_data: *mut std::ffi::c_void,
) -> i32 {
    // SAFETY: `unix_fd_add_local` passes a live `FdWatchData<F>` allocation
    // and GLib serializes this local source on the owning main context.
    let data = unsafe { &mut *(user_data as *mut FdWatchData<F>) };
    i32::from((data.callback)())
}

#[cfg(target_os = "linux")]
extern "C" fn fd_watch_destroy<F: FnMut() -> bool>(user_data: *mut std::ffi::c_void) {
    // SAFETY: GLib invokes the destroy notifier once for the allocation handed
    // to `g_unix_fd_add_full` below.
    unsafe {
        drop(Box::from_raw(user_data as *mut FdWatchData<F>));
    }
}

#[cfg(target_os = "linux")]
fn unix_fd_add_local<F: FnMut() -> bool + 'static>(fd: RawFd, func: F) {
    let data = Box::new(FdWatchData { callback: func });
    let ptr = Box::into_raw(data) as *mut std::ffi::c_void;
    // SAFETY: `fd` remains owned by the callback, and `ptr` remains owned by
    // GLib until it invokes `fd_watch_destroy`.
    unsafe {
        g_unix_fd_add_full(
            G_PRIORITY_DEFAULT_IDLE,
            fd,
            G_IO_IN,
            fd_watch_callback::<F>,
            ptr,
            fd_watch_destroy::<F>,
        );
    }
}

const READER_CANCEL_FALLBACK_POLL_MS: i32 = 50;

#[derive(Debug)]
enum ReaderPoll {
    PtyReady,
    Cancelled,
    TimedOut,
    /// The cancel descriptor could not be watched. The caller must discard it
    /// and continue with the bounded polling fallback instead of stopping the
    /// otherwise healthy PTY reader.
    CancelUnavailable(io::Error),
}

#[cfg(target_os = "linux")]
fn create_reader_cancel_eventfd() -> Option<Arc<OwnedFd>> {
    // SAFETY: eventfd returns a fresh descriptor on success; ownership moves
    // immediately into OwnedFd. Nonblocking keeps kill/Drop unable to stall.
    let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if raw < 0 {
        log::warn!(
            "PTY reader cancel eventfd unavailable; using {} ms polling: {}",
            READER_CANCEL_FALLBACK_POLL_MS,
            io::Error::last_os_error()
        );
        return None;
    }
    // SAFETY: `raw` is a fresh successful eventfd result owned by this process.
    Some(Arc::new(unsafe { OwnedFd::from_raw_fd(raw) }))
}

#[cfg(not(target_os = "linux"))]
fn create_reader_cancel_eventfd() -> Option<Arc<OwnedFd>> {
    None
}

/// Wait until the PTY can be read or teardown requests reader cancellation.
///
/// With a cancel fd this blocks indefinitely, so an idle reader has no timer
/// wakeups. Without one it preserves the historical 50 ms atomic check. PTY
/// HUP/ERR are reported as readable so the following `read` observes EOF/EIO;
/// an invalid PTY fd remains a real reader failure.
fn poll_pty_or_reader_cancel(pty_fd: RawFd, cancel_fd: Option<RawFd>) -> io::Result<ReaderPoll> {
    loop {
        let mut ready = [
            libc::pollfd {
                fd: pty_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: cancel_fd.unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let has_cancel_fd = cancel_fd.is_some();
        let descriptor_count: libc::nfds_t = if has_cancel_fd { 2 } else { 1 };
        let timeout = if has_cancel_fd {
            -1
        } else {
            READER_CANCEL_FALLBACK_POLL_MS
        };
        // SAFETY: `ready` contains `descriptor_count` initialized pollfd values
        // and remains writable for the duration of this blocking call.
        let polled = unsafe { libc::poll(ready.as_mut_ptr(), descriptor_count, timeout) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return if has_cancel_fd {
                Ok(ReaderPoll::CancelUnavailable(error))
            } else {
                Err(error)
            };
        }
        if polled == 0 {
            return Ok(ReaderPoll::TimedOut);
        }

        if has_cancel_fd {
            let cancel_events = ready[1].revents;
            if cancel_events & libc::POLLIN != 0 {
                return Ok(ReaderPoll::Cancelled);
            }
            if cancel_events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Ok(ReaderPoll::CancelUnavailable(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("cancel eventfd poll failure: revents={cancel_events:#x}"),
                )));
            }
        }

        let pty_events = ready[0].revents;
        if pty_events & libc::POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY reader descriptor became invalid",
            ));
        }
        if pty_events & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            return Ok(ReaderPoll::PtyReady);
        }
        // A signal or platform-specific event may produce no relevant bits.
        // Rebuild pollfd values so stale revents can never leak into a retry.
    }
}

fn request_reader_cancel(cancelled: &AtomicBool, cancel_eventfd: Option<&OwnedFd>) {
    cancelled.store(true, Ordering::Release);
    #[cfg(target_os = "linux")]
    if let Some(cancel_eventfd) = cancel_eventfd {
        if let Err(error) = signal_eventfd(cancel_eventfd.as_raw_fd()) {
            log::warn!("could not wake PTY reader cancellation poll: {error}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cancel_eventfd;
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

/// anvil's child-environment policy for a directly exec'd shell.
///
/// `less_default` is the one opinion this repo asserts: `LESS=R` keeps colored
/// git output, keeps the interactive pager even for a short `git log`, and lets
/// less use the alternate screen so pager content stays ephemeral. (Git would
/// otherwise default it to `FRX`, where `F` quits for short output and `X`
/// disables the alternate screen.) Locale rewriting and `LS_COLORS` stay off:
/// anvil draws UTF-8 either way and has never set colour defaults, so turning
/// them on here would override variables the user chose deliberately.
fn child_environment_options() -> child_env::ChildEnv<'static> {
    child_env::ChildEnv {
        less_default: Some("R"),
        ..child_env::ChildEnv::from_identity()
    }
}

/// anvil's policy for the PTY boundary net.
///
/// `strip_controls` stays off here: the boundary sees every keystroke and every
/// escape sequence this app answers a query with, and stripping controls there
/// would eat the input it exists to protect. Marker removal is unconditional
/// inside [`pty_input::InputGuard`] regardless of this policy — that is the part
/// that stops an injected `ESC[201~` from ending a frame early. `FirstLineOnly`
/// preserves anvil's existing fallback for unframed multiline input.
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
        Self::spawn_inner(argv, cwd, env_extra, false)
    }

    /// Spawn a shell with an optional private shell-integration token channel.
    /// Host-bridged/Flatpak children deliberately never receive this channel:
    /// the descriptor cannot cross that process boundary with equivalent
    /// ownership guarantees, so Agent execution remains fail-closed there.
    pub(crate) fn spawn_with_shell_token(
        argv: &[&str],
        cwd: Option<&str>,
        env_extra: &[(&str, &str)],
        enable_shell_token: bool,
    ) -> io::Result<Self> {
        Self::spawn_inner(argv, cwd, env_extra, enable_shell_token)
    }

    fn spawn_inner(
        argv: &[&str],
        cwd: Option<&str>,
        env_extra: &[(&str, &str)],
        enable_shell_token: bool,
    ) -> io::Result<Self> {
        let argv_owned: Vec<String> = argv.iter().map(|value| (*value).to_string()).collect();
        let host_bridge = crate::host::is_flatpak();
        let sanitized_extra: Vec<(&str, &str)> = env_extra
            .iter()
            .copied()
            .filter(|(name, _)| {
                !matches!(
                    *name,
                    "ANVIL_SHELL_INTEGRATION_FD" | "ANVIL_SHELL_INTEGRATION_TOKEN"
                )
            })
            .collect();
        let effective_cwd = cwd.filter(|directory| {
            let usable = crate::host::working_directory_available(directory);
            if !usable {
                log::warn!("PTY working directory is unavailable; using the application directory");
            }
            usable
        });
        let executable_argv = crate::host::wrap_argv(&argv_owned, effective_cwd, &sanitized_extra);
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

        // Block mode is anvil's default and libvte never spawns the child here
        // (it is handed a foreign PTY master), so nothing else injects the
        // terminal identity: before this, the child got `TERM` and no
        // `COLORTERM`, and bat/delta/lazygit fell back to 256 colours.
        //
        // `env_extra` goes in as the overlay's `extra` when we exec directly. In
        // Flatpak mode `crate::host::wrap_argv` has already turned it into
        // `flatpak-spawn --env=` arguments for the host child, so passing it here
        // as well would set it in the sandbox process too.
        let bridged_extra: &[(&str, &str)] = if host_bridge { &[] } else { &sanitized_extra };
        let mut c_environment = child_env::envp(&child_environment_options(), bridged_extra)?;
        let token_channel = if host_bridge || !enable_shell_token {
            None
        } else {
            shell_token_channel()?
        };
        // Neither caller-supplied spelling is authoritative. The public token
        // spelling is never used at all; the fd spelling is reconstructed from
        // the owned descriptor immediately below.
        c_environment.retain(|entry| {
            !entry.as_bytes().starts_with(b"ANVIL_SHELL_INTEGRATION_FD=")
                && !entry
                    .as_bytes()
                    .starts_with(b"ANVIL_SHELL_INTEGRATION_TOKEN=")
        });
        if let Some((_, read_fd, _)) = token_channel.as_ref() {
            c_environment.push(
                CString::new(format!(
                    "ANVIL_SHELL_INTEGRATION_FD={}",
                    read_fd.as_raw_fd()
                ))
                .expect("numeric shell integration descriptor contains no NUL"),
            );
        }
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

        let token_read_fd = token_channel
            .as_ref()
            .map(|(_, fd, _)| fd.as_raw_fd())
            .unwrap_or(-1);
        let token_write_fd = token_channel
            .as_ref()
            .map(|(_, _, fd)| fd.as_raw_fd())
            .unwrap_or(-1);

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
                if token_write_fd >= 0 {
                    // SAFETY: this is the child-side copy of a live pipe end.
                    unsafe { libc::close(token_write_fd) };
                }
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
                    if token_read_fd >= 0 && libc::fcntl(token_read_fd, libc::F_SETFD, 0) < 0 {
                        libc::_exit(126);
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
                let shell_integration_token =
                    if let Some((token, read_fd, write_fd)) = token_channel {
                        drop(read_fd);
                        let mut writer = std::fs::File::from(write_fd);
                        if writer
                            .write_all(format!("{token}\n").as_bytes())
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            drop(master);
                            drop(slave);
                            kill_and_reap_unreferenced_child(child);
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "could not deliver the shell integration token",
                            ));
                        }
                        Some(token)
                    } else {
                        None
                    };
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
                let input_tx = match spawn_fd_writer(writer_fd, "anvil-pty-writer") {
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
                    reader_cancel_eventfd: create_reader_cancel_eventfd(),
                    input_guard: std::sync::Mutex::new(pty_input::InputGuard::new()),
                    shell_bracketed_paste: Arc::new(AtomicBool::new(false)),
                    shell_integration_token,
                    #[cfg(test)]
                    test_slave: None,
                    #[cfg(test)]
                    test_foreground: None,
                })
            }
            Err(e) => Err(io::Error::other(e)),
        }
    }

    pub fn pid_i32(&self) -> i32 {
        self.child_lifecycle.pid()
    }

    pub(crate) fn shell_integration_token(&self) -> Option<&str> {
        self.shell_integration_token.as_deref()
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
        #[cfg(test)]
        if self.test_slave.is_some() {
            return self.test_foreground;
        }
        if !self.foreground_identity_available {
            return None;
        }
        let fd = self.master_fd_raw();
        if fd < 0 {
            return None;
        }
        let foreground = unsafe { libc::tcgetpgrp(fd) };
        let shell_group = unsafe { libc::getpgid(self.child_lifecycle.pid()) };
        (foreground > 0 && shell_group > 0).then_some(foreground == shell_group)
    }

    /// Keep the outgoing paste boundary in sync with parser-observed resets.
    /// The reader thread normally owns this bit; RIS is a semantic reset that
    /// must clear it before any later paste is encoded.
    pub(crate) fn set_shell_bracketed_paste(&self, enabled: bool) {
        self.shell_bracketed_paste.store(enabled, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn shell_bracketed_paste(&self) -> bool {
        self.shell_bracketed_paste.load(Ordering::Relaxed)
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
        request_reader_cancel(
            &self.reader_cancelled,
            self.reader_cancel_eventfd.as_deref(),
        );
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
        let reader_cancel_eventfd = self.reader_cancel_eventfd.as_ref().map(Arc::clone);
        #[cfg(target_os = "linux")]
        let eventfd = {
            // SAFETY: eventfd returns a fresh descriptor on success; ownership
            // transfers immediately to `OwnedFd`.
            let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            (raw >= 0).then(|| Arc::new(unsafe { OwnedFd::from_raw_fd(raw) }))
        };
        #[cfg(target_os = "linux")]
        let wake_pending = Arc::new(AtomicBool::new(false));
        #[cfg(target_os = "linux")]
        let eventfd_for_thread = eventfd.as_ref().map(Arc::clone);
        #[cfg(target_os = "linux")]
        let wake_pending_for_thread = Arc::clone(&wake_pending);
        let reader = std::thread::Builder::new()
            .name("anvil-pty-reader".to_string())
            .spawn(move || {
                let mut reader_cancel_eventfd = reader_cancel_eventfd;
                let notify = || {
                    #[cfg(target_os = "linux")]
                    if let Some(eventfd) = eventfd_for_thread.as_deref() {
                        notify_eventfd_once(eventfd, &wake_pending_for_thread);
                    }
                };
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
                    match poll_pty_or_reader_cancel(
                        fd,
                        reader_cancel_eventfd.as_ref().map(|eventfd| eventfd.as_raw_fd()),
                    ) {
                        Ok(ReaderPoll::PtyReady) => {}
                        Ok(ReaderPoll::Cancelled) => break,
                        Ok(ReaderPoll::TimedOut) => continue,
                        Ok(ReaderPoll::CancelUnavailable(error)) => {
                            log::warn!(
                                "PTY reader cancel eventfd unavailable; reverting to {} ms polling: {error}",
                                READER_CANCEL_FALLBACK_POLL_MS
                            );
                            reader_cancel_eventfd = None;
                            continue;
                        }
                        Err(error) => {
                            log::warn!("PTY reader poll stopped: {error}");
                            break;
                        }
                    }
                    match file.read(&mut buf) {
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
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
                            notify();
                        }
                    }
                }
                if reader_cancelled.load(Ordering::Acquire) {
                    // Dropping the sole producer disconnects the GLib receiver,
                    // which removes its source and releases all captured
                    // widgets/OwnedPty handles even if a descendant keeps the
                    // slave side open.
                    drop(tx);
                    notify();
                    return;
                }
                reap_child(&child_lifecycle, &tx);
                notify();
            });
        if let Err(error) = reader {
            self.close_master_fd();
            self.child_lifecycle.force_kill_and_reap();
            return Err(error);
        }

        #[cfg(target_os = "linux")]
        if let Some(eventfd) = eventfd {
            let on_exit = std::cell::Cell::new(Some(on_exit));

            unix_fd_add_local(eventfd.as_raw_fd(), move || {
                let _ = drain_eventfd(eventfd.as_raw_fd());

                // A producer may enqueue between the first empty read and
                // clearing `wake_pending`. Recheck after clearing so that
                // transition cannot lose its only eventfd notification.
                let message = match rx.try_recv() {
                    Ok(message) => message,
                    Err(mpsc::TryRecvError::Empty) => {
                        wake_pending.store(false, Ordering::Release);
                        match rx.try_recv() {
                            Ok(message) => {
                                wake_pending.store(true, Ordering::Release);
                                let _ = drain_eventfd(eventfd.as_raw_fd());
                                message
                            }
                            Err(mpsc::TryRecvError::Empty) => return true,
                            Err(mpsc::TryRecvError::Disconnected) => return false,
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => return false,
                };

                match message {
                    PtyMsg::Data(data) => {
                        callback(data);
                        let eventfd = Arc::clone(&eventfd);
                        let wake_pending = Arc::clone(&wake_pending);
                        glib::timeout_add_local_once(PTY_DISPATCH_INTERVAL, move || {
                            if signal_eventfd(eventfd.as_raw_fd()).is_err() {
                                wake_pending.store(false, Ordering::Release);
                            }
                        });
                        true
                    }
                    PtyMsg::Exit(code) => {
                        if let Some(f) = on_exit.take() {
                            f(code);
                        }
                        false
                    }
                }
            });
            return Ok(());
        }

        // eventfd is Linux-specific and can also fail under descriptor
        // pressure. Retain the original timer transport as a safe fallback.
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

#[cfg(target_os = "linux")]
fn notify_eventfd_once(eventfd: &OwnedFd, wake_pending: &AtomicBool) {
    if !wake_pending.swap(true, Ordering::AcqRel) && signal_eventfd(eventfd.as_raw_fd()).is_err() {
        // Do not leave the queue permanently armed without a kernel wakeup.
        // EINTR is retried below; this covers any other write failure.
        wake_pending.store(false, Ordering::Release);
    }
}

#[cfg(target_os = "linux")]
fn drain_eventfd(eventfd: RawFd) -> io::Result<()> {
    let mut value = 0u64;
    loop {
        // SAFETY: eventfd reads exactly one native u64. The descriptor is
        // nonblocking, so a raced or redundant drain is harmless.
        let read = unsafe {
            libc::read(
                eventfd,
                (&mut value as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        if read == std::mem::size_of::<u64>() as isize {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(error);
    }
}

#[cfg(target_os = "linux")]
fn signal_eventfd(eventfd: RawFd) -> io::Result<()> {
    let value = 1u64;
    loop {
        // SAFETY: eventfd writes exactly one native u64. EAGAIN is harmless:
        // it means a kernel wakeup is already pending in the counter.
        let written = unsafe {
            libc::write(
                eventfd,
                (&value as *const u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        if written == std::mem::size_of::<u64>() as isize {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(error);
    }
}

/// Bare, display-independent PTY plumbing for reader-dispatch tests.
#[cfg(test)]
impl OwnedPty {
    pub(crate) fn from_openpty(foreground: Option<bool>) -> io::Result<Self> {
        let OpenptyResult { master, slave } = openpty(None, None).map_err(io::Error::other)?;
        prepare_test_slave(&slave);
        // SAFETY: the child uses only async-signal-safe syscalls after fork.
        let child = match unsafe { unistd::fork() }.map_err(io::Error::other)? {
            ForkResult::Child => unsafe {
                libc::setpgid(0, 0);
                libc::_exit(0);
            },
            ForkResult::Parent { child } => child,
        };
        let child_lifecycle = match ChildLifecycle::new(child.as_raw(), ReapOwner::Ours) {
            Ok(lifecycle) => lifecycle,
            Err(error) => {
                kill_and_reap_unreferenced_child(child);
                return Err(error);
            }
        };
        let writer_fd = match master.try_clone() {
            Ok(fd) => fd,
            Err(error) => {
                child_lifecycle.force_kill_and_reap();
                return Err(error);
            }
        };
        let input_tx = match spawn_fd_writer(writer_fd, "anvil-test-pty-writer") {
            Ok(tx) => tx,
            Err(error) => {
                child_lifecycle.force_kill_and_reap();
                return Err(error);
            }
        };
        Ok(Self {
            master: Arc::new(Mutex::new(Some(master))),
            input_tx,
            child_lifecycle,
            foreground_identity_available: true,
            reader_cancelled: Arc::new(AtomicBool::new(false)),
            reader_cancel_eventfd: create_reader_cancel_eventfd(),
            input_guard: Mutex::new(pty_input::InputGuard::new()),
            shell_bracketed_paste: Arc::new(AtomicBool::new(false)),
            shell_integration_token: None,
            test_slave: Some(slave),
            test_foreground: foreground,
        })
    }

    pub(crate) fn drain_test_slave(&self, wait: std::time::Duration) -> Vec<u8> {
        let Some(slave) = self.test_slave.as_ref() else {
            return Vec::new();
        };
        let fd = slave.as_raw_fd();
        let deadline = std::time::Instant::now() + wait;
        let mut drained = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            // SAFETY: `buffer` is writable and `fd` is owned by this object.
            let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read > 0 {
                drained.extend_from_slice(&buffer[..read as usize]);
                continue;
            }
            if read == 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock
                && drained.is_empty()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            break;
        }
        drained
    }
}

#[cfg(test)]
fn prepare_test_slave(slave: &OwnedFd) {
    let fd = slave.as_raw_fd();
    // SAFETY: `fd` and the local termios value are valid for these syscalls.
    unsafe {
        let mut attrs: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut attrs) == 0 {
            libc::cfmakeraw(&mut attrs);
            let _ = libc::tcsetattr(fd, libc::TCSANOW, &attrs);
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

impl Drop for OwnedPty {
    fn drop(&mut self) {
        request_reader_cancel(
            &self.reader_cancelled,
            self.reader_cancel_eventfd.as_deref(),
        );
        self.close_master_fd();
        self.child_lifecycle
            .terminate(EscalationPolicy::SESSION_DRAIN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(target_os = "linux")]
    #[test]
    fn eventfd_wakeup_is_coalesced_until_consumer_rearms() {
        // Use a blocking eventfd so a missing notification fails this focused
        // unit test deterministically instead of producing an EAGAIN branch.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: eventfd returned a fresh descriptor owned by this test.
        let eventfd = unsafe { OwnedFd::from_raw_fd(raw) };
        let wake_pending = AtomicBool::new(false);

        notify_eventfd_once(&eventfd, &wake_pending);
        notify_eventfd_once(&eventfd, &wake_pending);

        let mut value = 0u64;
        let read = unsafe {
            libc::read(
                eventfd.as_raw_fd(),
                (&mut value as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(read as usize, std::mem::size_of::<u64>());
        assert_eq!(value, 1);

        wake_pending.store(false, Ordering::Release);
        notify_eventfd_once(&eventfd, &wake_pending);
        value = 0;
        let read = unsafe {
            libc::read(
                eventfd.as_raw_fd(),
                (&mut value as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        assert_eq!(read as usize, std::mem::size_of::<u64>());
        assert_eq!(value, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reader_cancel_eventfd_keeps_idle_poll_asleep_and_wakes_it() {
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let cancel_eventfd = create_reader_cancel_eventfd().expect("create cancel eventfd");
        let cancel_for_reader = Arc::clone(&cancel_eventfd);
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(poll_pty_or_reader_cancel(
                    reader.as_raw_fd(),
                    Some(cancel_for_reader.as_raw_fd()),
                ))
                .unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        match result_rx.recv_timeout(Duration::from_millis(120)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(result) => {
                reader.join().unwrap();
                panic!("idle reader poll returned without input or cancellation: {result:?}");
            }
            Err(error) => {
                reader.join().unwrap();
                panic!("reader poll result channel failed: {error}");
            }
        }

        let cancelled = AtomicBool::new(false);
        request_reader_cancel(&cancelled, Some(&cancel_eventfd));
        let result = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancel eventfd must wake the idle reader poll");
        reader.join().unwrap();
        assert!(cancelled.load(Ordering::Acquire));
        assert!(matches!(result, Ok(ReaderPoll::Cancelled)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_cancel_fd_downgrades_to_bounded_polling() {
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let result = poll_pty_or_reader_cancel(reader.as_raw_fd(), Some(i32::MAX)).unwrap();
        assert!(matches!(result, ReaderPoll::CancelUnavailable(_)));

        let started = Instant::now();
        let result = poll_pty_or_reader_cancel(reader.as_raw_fd(), None).unwrap();
        assert!(matches!(result, ReaderPoll::TimedOut));
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reader_poll_reports_pty_hangup_as_readable() {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let cancel_eventfd = create_reader_cancel_eventfd().expect("create cancel eventfd");
        drop(writer);

        let result =
            poll_pty_or_reader_cancel(reader.as_raw_fd(), Some(cancel_eventfd.as_raw_fd()))
                .unwrap();
        assert!(matches!(result, ReaderPoll::PtyReady));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn token_pipe_is_private_bounded_and_above_standard_io() {
        let (token, read_fd, write_fd) = shell_token_channel()
            .expect("create token pipe")
            .expect("Linux provides getrandom and pipe2");
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(read_fd.as_raw_fd() > libc::STDERR_FILENO);
        assert!(write_fd.as_raw_fd() > libc::STDERR_FILENO);

        let mut writer = std::fs::File::from(write_fd);
        writer.write_all(format!("{token}\n").as_bytes()).unwrap();
        drop(writer);
        let mut reader = std::fs::File::from(read_fd);
        let mut delivered = String::new();
        reader.read_to_string(&mut delivered).unwrap();
        assert_eq!(delivered, format!("{token}\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bundled_bash_consumes_fd_announces_token_and_scrubs_environment() {
        let integration = format!(
            "{}/scripts/shell-integration/anvil.bash",
            env!("CARGO_MANIFEST_DIR")
        );
        let pty = OwnedPty::spawn_with_shell_token(
            &[
                "/bin/bash",
                "--noprofile",
                "--rcfile",
                integration.as_str(),
                "-i",
            ],
            None,
            &[
                ("PS1", "anvil-fd-test$ "),
                ("ANVIL_SHELL_INTEGRATION_FD", "0"),
                ("ANVIL_SHELL_INTEGRATION_TOKEN", "forged"),
                ("__anvil_command_token", "export-seed"),
            ],
            true,
        )
        .expect("spawn token-aware bash");
        let token = pty
            .shell_integration_token()
            .expect("token was issued")
            .to_string();
        let ready = format!("\x1b]7771;{token}\x07");
        let output = read_until(
            pty.master_fd_raw(),
            ready.as_bytes(),
            Duration::from_secs(5),
        );
        assert!(
            output
                .windows(ready.len())
                .any(|window| window == ready.as_bytes()),
            "integration did not announce the issued token: {:?}",
            String::from_utf8_lossy(&output)
        );

        let mut resource = format!("source '{}'", integration.replace('\'', "'\\''")).into_bytes();
        resource.push(b'\r');
        pty.write_bytes(&resource);
        let output = read_until(
            pty.master_fd_raw(),
            ready.as_bytes(),
            Duration::from_secs(5),
        );
        assert!(
            output
                .windows(ready.len())
                .any(|window| window == ready.as_bytes()),
            "re-sourcing replaced the private token capability: {:?}",
            String::from_utf8_lossy(&output)
        );

        let mut command = br#"printf '\n__ANVIL_ENV[%s][%s][%s]__\n' "${ANVIL_SHELL_INTEGRATION_FD+leak}" "${ANVIL_SHELL_INTEGRATION_TOKEN+leak}" "$(env | grep -q '^__anvil_command_token=' && printf leak)""#.to_vec();
        command.push(b'\r');
        pty.write_bytes(&command);
        let clean = b"__ANVIL_ENV[][][]__";
        let output = read_until(pty.master_fd_raw(), clean, Duration::from_secs(5));
        assert!(
            output.windows(clean.len()).any(|window| window == clean),
            "integration metadata leaked into a user command: {:?}",
            String::from_utf8_lossy(&output)
        );
        pty.kill();
    }

    fn read_until(fd: RawFd, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        while Instant::now() < deadline {
            let mut poll_fd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll_fd is valid for one entry and the borrowed master
            // descriptor remains owned by `OwnedPty` for the test duration.
            if unsafe { libc::poll(&mut poll_fd, 1, 100) } <= 0 {
                continue;
            }
            let mut buffer = [0_u8; 256];
            // SAFETY: buffer is writable and fd is the live PTY master.
            let read =
                unsafe { libc::read(fd, buffer.as_mut_ptr().cast::<libc::c_void>(), buffer.len()) };
            if read < 0
                && matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                )
            {
                continue;
            }
            if read <= 0 {
                break;
            }
            output.extend_from_slice(&buffer[..read as usize]);
            if output.windows(needle.len()).any(|window| window == needle) {
                break;
            }
        }
        output
    }

    #[test]
    fn spawn_rejects_nul_before_forking() {
        let error = OwnedPty::spawn(&["sh", "bad\0argument"], None, &[])
            .err()
            .expect("embedded NUL must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    /// The identity variables are `jterm_core::child_env`'s to get right (and its
    /// tests do); what is anvil's is which of the optional policies it turns on.
    /// Block mode is the default and libvte never spawns the child here, so this
    /// overlay is the *only* thing that tells the shell what terminal it is in.
    #[test]
    fn the_child_environment_asserts_the_pager_and_nothing_else() {
        let options = child_environment_options();
        assert_eq!(options.less_default, Some("R"));
        assert!(
            !options.normalize_locale,
            "anvil draws UTF-8 either way; a deliberate LANG is the user's"
        );
        assert!(!options.color_defaults, "anvil has never set LS_COLORS");
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
    /// anvil's to guarantee is the wiring: resolution happens before `fork`, so
    /// the pane that asked gets the error instead of a child that exits 127.
    #[test]
    fn spawn_reports_an_unresolvable_command_before_forking() {
        let error = OwnedPty::spawn(&["anvil-command-that-does-not-exist"], None, &[])
            .err()
            .expect("an unresolvable command must fail the spawn");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn background_writer_preserves_large_payload_and_order() {
        let (mut reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let tx = spawn_fd_writer(writer.into(), "anvil-test-writer").unwrap();

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
        let tx = spawn_fd_writer(writer.into(), "anvil-test-bounded-writer").unwrap();

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
    /// is `jterm_core::pty_input::InputGuard` now, and what stays anvil's is the
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

    /// anvil keeps control bytes at this boundary: every keystroke and every
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
