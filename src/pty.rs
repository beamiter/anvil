//! Owned PTY: fork+exec a shell on a fresh pseudo-terminal, then stream its
//! output to the GTK main thread via an eventfd-signaled mpsc channel. Ported
//! from jterm4 — the block view drives its own PTY (rather than vte4's) so it can
//! intercept the raw stream for OSC 133 block detection.

use crate::process::{ChildLifecycle, EscalationPolicy, ReapOwner};
use gtk::glib;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::{self, ForkResult, Pid};
use relm4::gtk;
use std::borrow::Cow;
use std::ffi::{CString, OsStr};
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

enum PtyMsg {
    Data(Vec<u8>),
    Exit(i32),
}

pub struct OwnedPty {
    master: Arc<std::sync::Mutex<Option<OwnedFd>>>,
    /// `send` on this unbounded channel never waits for the kernel PTY buffer.
    /// A dedicated worker preserves ordering and completes short writes away
    /// from GTK's main thread.
    input_tx: mpsc::Sender<Vec<u8>>,
    child_lifecycle: Arc<ChildLifecycle>,
    reader_cancelled: Arc<AtomicBool>,
    /// Tracks bracketed-paste writes whose start/body/end arrive as separate
    /// `write_bytes` calls. While active, embedded line endings are intentional
    /// paste content and must be forwarded unchanged.
    outgoing_bracketed_paste: AtomicBool,
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

const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";
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

/// Start an ordered, non-UI-blocking writer for a duplicated PTY descriptor.
///
/// The queue is deliberately unbounded: terminal input must not be truncated,
/// and both producers are already paced (human/VTE input or the bounded PTY
/// reader). Kernel backpressure is isolated to this worker instead of freezing
/// window input, rendering, and close handling.
pub(crate) fn spawn_fd_writer(
    fd: OwnedFd,
    thread_name: &'static str,
) -> io::Result<mpsc::Sender<Vec<u8>>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            for data in rx {
                if let Err(err) = write_all_fd(fd.as_raw_fd(), &data) {
                    log::warn!("{thread_name} stopped: {err}");
                    break;
                }
            }
        })?;
    Ok(tx)
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

/// Protect insertion-only input from silently becoming multiple submissions.
///
/// Block recall has several UI entry points. Some already pass only the first
/// logical line, while the hover action and context menu pass the complete
/// captured command. Writing an unframed newline stream directly to a shell PTY
/// executes every completed line immediately. The same hazard exists when a
/// shell has bracketed paste disabled and the user pastes multiple lines.
///
/// The PTY boundary is the one place all of those writes share, so enforce one
/// consistent contract here:
///
/// - explicit or already-active bracketed-paste payloads pass through;
/// - data ending in CR is an explicit submission and passes through;
/// - multiline insertion is automatically bracket-wrapped when the shell
///   advertises DECSET 2004;
/// - otherwise multiline insertion safely falls back to its first line;
/// - ordinary single-line input is unchanged.
///
/// Returns the bytes to write plus the outgoing bracketed-paste state for the
/// next call.
fn sanitize_input_chunk(
    data: &[u8],
    paste_active: bool,
    shell_supports_bracketed_paste: bool,
) -> (Cow<'_, [u8]>, bool) {
    let starts_paste = data.starts_with(BRACKETED_PASTE_START);
    let ends_paste = data.ends_with(BRACKETED_PASTE_END);
    let protected_by_paste = paste_active || starts_paste;
    let next_paste_active = if ends_paste {
        false
    } else if starts_paste {
        true
    } else {
        paste_active
    };

    // A final CR is how terminal callers explicitly press Enter. Do not rewrite
    // those command-execution paths, even when the submitted command is multiline.
    if protected_by_paste || data.ends_with(b"\r") {
        return (Cow::Borrowed(data), next_paste_active);
    }

    let Some(first_break) = data.iter().position(|&byte| byte == b'\r' || byte == b'\n') else {
        return (Cow::Borrowed(data), next_paste_active);
    };

    if shell_supports_bracketed_paste {
        let mut wrapped = Vec::with_capacity(
            BRACKETED_PASTE_START.len() + data.len() + BRACKETED_PASTE_END.len(),
        );
        wrapped.extend_from_slice(BRACKETED_PASTE_START);
        wrapped.extend_from_slice(data);
        wrapped.extend_from_slice(BRACKETED_PASTE_END);
        return (Cow::Owned(wrapped), next_paste_active);
    }

    (Cow::Owned(data[..first_break].to_vec()), next_paste_active)
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Resolve the executable before `fork` so the child can call only `execve`.
/// Relative PATH entries follow `execvp` semantics and are interpreted from
/// the directory the child will enter.
fn resolve_executable(
    executable: &str,
    path: Option<&OsStr>,
    child_cwd: Option<&str>,
) -> io::Result<PathBuf> {
    let executable_path = Path::new(executable);
    if executable_path.is_absolute() {
        return is_executable_file(executable_path)
            .then(|| executable_path.to_path_buf())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "PTY executable does not exist or is not executable",
                )
            });
    }

    // A process can outlive the directory it started in. Absolute PATH
    // entries remain usable in that situation, so do not fail the whole pane
    // merely because getcwd can no longer reconstruct the application cwd.
    let current_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let child_directory = child_cwd
        .map(PathBuf::from)
        .map(|directory| {
            if directory.is_absolute() {
                directory
            } else {
                current_directory.join(directory)
            }
        })
        .unwrap_or_else(|| current_directory.clone());
    if executable.as_bytes().contains(&b'/') {
        let candidate = child_directory.join(executable_path);
        return is_executable_file(&candidate)
            .then_some(candidate)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "PTY executable does not exist or is not executable",
                )
            });
    }

    let search_path = path.unwrap_or_else(|| OsStr::new("/bin:/usr/bin"));
    for directory in std::env::split_paths(search_path) {
        let directory = if directory.as_os_str().is_empty() {
            child_directory.clone()
        } else if directory.is_absolute() {
            directory
        } else {
            child_directory.join(directory)
        };
        let candidate = directory.join(executable_path);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "PTY executable was not found in PATH",
    ))
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

        let mut environment = std::collections::BTreeMap::new();
        environment.extend(std::env::vars_os());
        environment.insert("TERM".into(), "xterm-256color".into());
        if !host_bridge {
            for (key, value) in env_extra {
                environment.insert((*key).into(), (*value).into());
            }
        }
        let executable_path = resolve_executable(
            &executable_argv[0],
            environment
                .get(OsStr::new("PATH"))
                .map(|value| value.as_os_str()),
            child_cwd,
        )?;
        let c_executable = CString::new(executable_path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "resolved PTY executable contains an embedded NUL byte",
            )
        })?;
        let c_environment = environment
            .into_iter()
            .map(|(key, value)| {
                let mut entry =
                    Vec::with_capacity(key.as_bytes().len() + value.as_bytes().len() + 1);
                entry.extend_from_slice(key.as_bytes());
                entry.push(b'=');
                entry.extend_from_slice(value.as_bytes());
                CString::new(entry).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "PTY environment contains an embedded NUL byte",
                    )
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
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
                    reader_cancelled: Arc::new(AtomicBool::new(false)),
                    outgoing_bracketed_paste: AtomicBool::new(false),
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

    pub fn write_bytes(&self, data: &[u8]) {
        let paste_active = self.outgoing_bracketed_paste.load(Ordering::Relaxed);
        let shell_supports_bracketed_paste = self.shell_bracketed_paste.load(Ordering::Relaxed);
        let (safe_data, next_paste_active) =
            sanitize_input_chunk(data, paste_active, shell_supports_bracketed_paste);
        self.outgoing_bracketed_paste
            .store(next_paste_active, Ordering::Relaxed);

        if safe_data.is_empty() {
            return;
        }
        if let Err(err) = self.input_tx.send(safe_data.into_owned()) {
            log::warn!(
                "PTY input queue is closed; discarded {} byte(s)",
                err.0.len()
            );
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

    #[test]
    fn executable_resolution_is_absolute_and_rejects_missing_commands() {
        let path = std::env::var_os("PATH");
        let resolved = resolve_executable("sh", path.as_deref(), None).unwrap();
        assert!(resolved.is_absolute());
        assert!(is_executable_file(&resolved));

        let missing = resolve_executable(
            "jterm1-command-that-does-not-exist",
            Some(OsStr::new("/bin:/usr/bin")),
            None,
        )
        .unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
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
    fn unframed_multiline_insert_falls_back_to_first_line_without_shell_support() {
        let (safe, active) = sanitize_input_chunk(b"echo first\necho second", false, false);
        assert_eq!(safe.as_ref(), b"echo first");
        assert!(!active);

        let (safe, _) = sanitize_input_chunk(b"echo first\r\necho second", false, false);
        assert_eq!(safe.as_ref(), b"echo first");
    }

    #[test]
    fn shell_supported_multiline_insert_is_automatically_bracketed() {
        let input = b"echo first\necho second";
        let (safe, active) = sanitize_input_chunk(input, false, true);
        let mut expected = Vec::new();
        expected.extend_from_slice(BRACKETED_PASTE_START);
        expected.extend_from_slice(input);
        expected.extend_from_slice(BRACKETED_PASTE_END);
        assert_eq!(safe.as_ref(), expected.as_slice());
        assert!(!active);
    }

    #[test]
    fn explicit_submission_preserves_multiline_bytes() {
        let submitted = b"if true; then\necho ok\nfi\r";
        let (safe, active) = sanitize_input_chunk(submitted, false, false);
        assert_eq!(safe.as_ref(), submitted);
        assert!(!active);
    }

    #[test]
    fn bracketed_paste_preserves_multiline_body_across_writes() {
        let (start, active) = sanitize_input_chunk(BRACKETED_PASTE_START, false, false);
        assert_eq!(start.as_ref(), BRACKETED_PASTE_START);
        assert!(active);

        let body = b"echo first\necho second";
        let (safe_body, active) = sanitize_input_chunk(body, active, false);
        assert_eq!(safe_body.as_ref(), body);
        assert!(active);

        let (end, active) = sanitize_input_chunk(BRACKETED_PASTE_END, active, false);
        assert_eq!(end.as_ref(), BRACKETED_PASTE_END);
        assert!(!active);
    }

    #[test]
    fn ordinary_single_line_input_is_unchanged() {
        let input = b"git status";
        let (safe, active) = sanitize_input_chunk(input, false, false);
        assert_eq!(safe.as_ref(), input);
        assert!(!active);
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
