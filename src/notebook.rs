//! Lightweight executable notebook: `.jtnb.md` files = markdown + runnable
//! shell code fences. The viewer is a modal dialog; supported shell fences
//! become cards with Run/Stop controls, bounded inline output, and sequential
//! Run All / Stop All orchestration.
//!
//! Why minimal:
//! - No external markdown crate (offline-build constraint; pulldown_cmark
//!   isn't in the cargo cache). We implement just enough to recognise code
//!   fences and apply trivial styling (headings, bold/italic/inline-code)
//!   via pango markup.
//! - Cells run in isolated process groups rooted at the notebook's own
//!   directory — they do NOT touch the user's active terminal. Closing or
//!   stopping the notebook kills the whole group so descendants cannot leak.
//! - Output captured via two reader threads (stdout/stderr each on their
//!   own thread) feeding an mpsc channel polled on the GLib main loop.
//!   Avoids the classic single-pipe-fills-and-blocks deadlock.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use relm4::adw;
use relm4::gtk;
use relm4::gtk::gio;
use relm4::prelude::*;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Max bytes of captured output retained per cell run before truncation.
/// Matches the spirit of `block.rs`'s raw-output cap — bounded memory even
/// for runaway commands.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_NOTEBOOK_BYTES: u64 = 1024 * 1024;
const MAX_NOTEBOOK_SEGMENTS: usize = 512;
const MAX_NOTEBOOK_CELLS: usize = 128;
const MAX_NOTEBOOK_CELL_BYTES: usize = 256 * 1024;
const MAX_NOTEBOOK_TEXT_SEGMENT_BYTES: usize = 256 * 1024;
const MAX_NOTEBOOK_PATH_DISPLAY_BYTES: usize = 4 * 1024;
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(40);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_OUTPUT_EVENTS_PER_TICK: usize = 32;
const MAX_CONCURRENT_CELL_WORKERS: usize = 8;
static ACTIVE_CELL_WORKERS: AtomicI32 = AtomicI32::new(0);

struct CellWorkerPermit;

impl CellWorkerPermit {
    fn acquire() -> Option<Self> {
        // CAS preserves the permit bound without requiring Atomic::try_update.
        let mut active = ACTIVE_CELL_WORKERS.load(Ordering::Acquire);
        loop {
            if active >= MAX_CONCURRENT_CELL_WORKERS as i32 {
                return None;
            }
            match ACTIVE_CELL_WORKERS.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self),
                Err(current) => active = current,
            }
        }
    }
}

impl Drop for CellWorkerPermit {
    fn drop(&mut self) {
        ACTIVE_CELL_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) use jterm_core::notebook_text::{parse_segments, render_text_to_pango, Segment};

fn read_notebook_file(path: &Path) -> io::Result<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW refuses a symlinked notebook (ELOOP surfaces through the
        // open error below); O_NONBLOCK keeps a planted FIFO from hanging.
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "notebook is not a regular file",
        ));
    }
    if metadata.len() > MAX_NOTEBOOK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("notebook exceeds the {MAX_NOTEBOOK_BYTES}-byte limit"),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_NOTEBOOK_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_NOTEBOOK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("notebook exceeds the {MAX_NOTEBOOK_BYTES}-byte limit"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn notebook_cell_issue(source: &str) -> Option<&'static str> {
    if source.len() > MAX_NOTEBOOK_CELL_BYTES {
        return Some("cell exceeds the execution size limit");
    }
    if source.chars().any(|ch| {
        !matches!(ch, '\n' | '\t')
            && (ch.is_control() || crate::review_input::is_visual_spoofing_character(ch))
    }) {
        return Some("cell contains a hidden, bidirectional, or unsafe control character");
    }
    None
}

fn bounded_notebook_display(source: &str, max_bytes: usize) -> String {
    crate::review_input::safe_multiline_display(source, max_bytes)
}

#[derive(Debug, Clone)]
struct CommandSpec {
    argv: Vec<String>,
    source: String,
    cwd: PathBuf,
}

fn language_name(info: &str) -> &str {
    info.split_whitespace().next().unwrap_or("")
}

/// Select an interpreter from the fence's first info-string word. Unlabelled
/// and `shell` cells use the user's login shell; explicit fences are kept
/// deterministic and do not source interactive profiles.
fn shell_argv_for_info(info: &str, configured_shell: &[String]) -> Option<Vec<String>> {
    let language = language_name(info).to_ascii_lowercase();
    match language.as_str() {
        "" | "shell" => (!configured_shell.is_empty()).then(|| configured_shell.to_vec()),
        "bash" | "sh" | "zsh" | "fish" => Some(vec![language]),
        "pwsh" => Some(vec![
            "pwsh".to_owned(),
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "-".to_owned(),
        ]),
        "powershell" => Some(vec![
            "powershell".to_owned(),
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "-".to_owned(),
        ]),
        _ => None,
    }
}

/// Why a fence is shown without Run/Stop. `None` means the cell can execute.
fn notebook_preview_reason<'a>(
    info: &str,
    configured_shell: &[String],
    source: &'a str,
) -> Option<&'a str> {
    if let Some(issue) = notebook_cell_issue(source) {
        return Some(issue);
    }
    shell_argv_for_info(info, configured_shell).is_none().then_some(
        "Only shell fences are executable; this cell is preview-only. Open the file in an editor to change it.",
    )
}

fn launch_notebook_in_editor(path: &Path, parent: &adw::ApplicationWindow) {
    let file = gio::File::for_path(path);
    let launcher = gtk::FileLauncher::new(Some(&file));
    let parent = parent.clone();
    launcher.launch(Some(&parent), gio::Cancellable::NONE, |result| {
        if let Err(err) = result {
            log::warn!(
                "notebook: open in editor failed: {}",
                crate::review_input::safe_inline_display(&err.to_string(), 1024)
            );
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CellOutcome {
    Exited(i32),
    Cancelled,
    Failed(String),
}

impl CellOutcome {
    fn failed(&self) -> bool {
        !matches!(self, Self::Exited(0))
    }
}

enum WorkerEvent {
    Output(OutputStream, Vec<u8>),
    Done(CellOutcome),
}

/// Per-cell execution handle so Stop, Stop All, and dialog close can terminate
/// the same process while the worker remains its sole reaper.
struct CellHandle {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    /// Diagnostic telemetry only. Signals require the owned child under its
    /// mutex; this cached number must never authorize a signal after reaping.
    pgid: Arc<AtomicI32>,
}

impl CellHandle {
    fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            pgid: Arc::new(AtomicI32::new(0)),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                terminate_child_group(child);
            }
        }
    }
}

fn signal_process_group(pgid: i32) {
    #[cfg(unix)]
    if pgid > 0 {
        // SAFETY: `kill` receives a valid signal number; a negative PID targets
        // the process group created for this cell. Failure (already exited) is
        // intentionally harmless.
        unsafe {
            nix::libc::kill(-pgid, nix::libc::SIGKILL);
        }
    }
}

fn terminate_child_group(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            // Every cell is its own process-group leader. Addressing -pid kills
            // descendants as well as the interpreter, even if the latter has
            // already forked a long-running background command.
            unsafe {
                nix::libc::kill(-pid, nix::libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

/// Wait without retaining the child mutex between polls, leaving cancellation
/// able to signal the process group at any time.
fn wait_for_shared_child(child_slot: &Arc<Mutex<Option<Child>>>) -> std::io::Result<i32> {
    loop {
        let result = {
            let mut guard = child_slot
                .lock()
                .map_err(|_| std::io::Error::other("child handle mutex poisoned"))?;
            let child = guard
                .as_mut()
                .ok_or_else(|| std::io::Error::other("child handle missing before exit"))?;
            let pid = i32::try_from(child.id())
                .map_err(|_| std::io::Error::other("child pid does not fit i32"))?;
            let flags = nix::sys::wait::WaitPidFlag::WEXITED
                | nix::sys::wait::WaitPidFlag::WNOHANG
                | nix::sys::wait::WaitPidFlag::WNOWAIT;
            let observed = nix::sys::wait::waitid(
                nix::sys::wait::Id::Pid(nix::unistd::Pid::from_raw(pid)),
                flags,
            );
            match observed {
                Ok(
                    nix::sys::wait::WaitStatus::Exited(..)
                    | nix::sys::wait::WaitStatus::Signaled(..),
                ) => {
                    // Keep the leader unreaped until its group is terminated.
                    // Cancellation takes this same mutex and cannot use a PID
                    // between reaping it and clearing the ownership slot.
                    signal_process_group(pid);
                    let status = child.wait();
                    guard.take();
                    Some(status?.code().unwrap_or(-1))
                }
                Ok(_) | Err(nix::errno::Errno::EINTR) => None,
                Err(error) => {
                    if error == nix::errno::Errno::ECHILD {
                        // An external reaper already released this identity.
                        // Forget it without any last signal to its old number.
                        guard.take();
                    }
                    return Err(std::io::Error::from_raw_os_error(error as i32));
                }
            }
        };
        if let Some(code) = result {
            return Ok(code);
        }
        // Never retain the ownership lock while sleeping: Stop must stay live.
        std::thread::sleep(CHILD_POLL_INTERVAL);
    }
}

fn fail_cell_io_setup(
    child_slot: &Arc<Mutex<Option<Child>>>,
    pgid: &Arc<AtomicI32>,
    sender: &mpsc::SyncSender<WorkerEvent>,
    threads: Vec<std::thread::JoinHandle<()>>,
    error: io::Error,
    pipes: &CellPipeControl,
) {
    pipes.retire();
    if let Ok(mut guard) = child_slot.lock() {
        if let Some(child) = guard.as_mut() {
            terminate_child_group(child);
        }
    }
    let _ = wait_for_shared_child(child_slot);
    for thread in threads {
        let _ = thread.join();
    }
    pgid.store(0, Ordering::SeqCst);
    let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(format!(
        "I/O worker thread spawn failed: {error}"
    ))));
}

/// Escaped sessions can retain our pipe ends after the owned group exits.
/// Retire only our I/O, allowing a short final drain without signalling any
/// process outside that group. Running cells have no artificial time limit.
#[derive(Clone, Default)]
struct CellPipeControl {
    retired: Arc<Mutex<Option<std::time::Instant>>>,
    deferred_output: Arc<Mutex<[Vec<u8>; 2]>>,
}

impl CellPipeControl {
    fn retire(&self) {
        self.retired
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert_with(std::time::Instant::now);
    }

    fn check(&self) -> io::Result<()> {
        if self
            .retired
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some_and(|when| when.elapsed() >= Duration::from_millis(100))
        {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "notebook pipe remained open after cell exit",
            ))
        } else {
            Ok(())
        }
    }

    fn wrap<T: std::os::fd::AsRawFd>(&self, inner: T) -> io::Result<CellPipe<T>> {
        let fd = inner.as_raw_fd();
        // SAFETY: `inner` continues to own fd; only its file status changes.
        let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFL) };
        if flags < 0
            || unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(CellPipe {
            inner,
            control: self.clone(),
        })
    }

    fn send_output(
        &self,
        sender: &mpsc::SyncSender<WorkerEvent>,
        mut event: WorkerEvent,
    ) -> Result<(), WorkerEvent> {
        loop {
            if self.check().is_err() {
                return Err(event);
            }
            match sender.try_send(event) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Disconnected(pending)) => return Err(pending),
                Err(mpsc::TrySendError::Full(pending)) => event = pending,
            }
            // Backpressure is bounded by the same retirement deadline as I/O.
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send_tail(&self, sender: &mpsc::SyncSender<WorkerEvent>) {
        // All I/O threads have joined and closed their descriptors first.
        // Delivering events may wait for the UI, just as the final Done does.
        let tails = std::mem::take(
            &mut *self
                .deferred_output
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for (stream, bytes) in [OutputStream::Stdout, OutputStream::Stderr]
            .into_iter()
            .zip(tails)
        {
            if !bytes.is_empty() && sender.send(WorkerEvent::Output(stream, bytes)).is_err() {
                break;
            }
        }
    }
}

struct CellPipe<T> {
    inner: T,
    control: CellPipeControl,
}

impl<T: std::os::fd::AsRawFd> CellPipe<T> {
    fn wait_ready(&self, events: nix::libc::c_short) -> io::Result<()> {
        self.control.check()?;
        let mut poll = nix::libc::pollfd {
            fd: self.inner.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: poll points to one live descriptor still owned by `inner`.
        let result = unsafe { nix::libc::poll(&mut poll, 1, 10) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        self.control.check()
    }
}

impl<T: Read + std::os::fd::AsRawFd> Read for CellPipe<T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            self.control.check()?;
            match self.inner.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_ready(nix::libc::POLLIN)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl<T: Write + std::os::fd::AsRawFd> Write for CellPipe<T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            self.control.check()?;
            match self.inner.write(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_ready(nix::libc::POLLOUT)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn stream_cell_output<T: Read + std::os::fd::AsRawFd>(
    mut output: CellPipe<T>,
    stream: OutputStream,
    sender: mpsc::SyncSender<WorkerEvent>,
) {
    let mut buffer = [0u8; 4096];
    let tail = loop {
        match output.read(&mut buffer) {
            Ok(0) => return,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => break Vec::new(),
            Err(_) => return,
            Ok(count) => {
                match output.control.send_output(
                    &sender,
                    WorkerEvent::Output(stream, buffer[..count].to_vec()),
                ) {
                    Ok(()) => {}
                    Err(WorkerEvent::Output(_, bytes)) if output.control.check().is_err() => {
                        break bytes
                    }
                    Err(_) => return,
                }
            }
        }
    };
    // A busy UI must not lose the root's already-buffered tail just because
    // its channel was full during retirement. Drain the currently available
    // nonblocking bytes into a bounded final event, then close the descriptor.
    // One byte beyond the display cap preserves its truncation notification.
    let mut tail = tail;
    while tail.len() <= MAX_OUTPUT_BYTES {
        let limit = buffer.len().min(MAX_OUTPUT_BYTES + 1 - tail.len());
        match output.inner.read(&mut buffer[..limit]) {
            Ok(0) | Err(_) => break,
            Ok(count) => tail.extend_from_slice(&buffer[..count]),
        }
    }
    let index = match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
    };
    output
        .control
        .deferred_output
        .lock()
        .unwrap_or_else(|error| error.into_inner())[index] = tail;
}

const NON_UTF8_FLATPAK_CWD_ERROR: &str =
    "Notebook working directory contains non-UTF-8 bytes; Flatpak cannot pass it to the host safely.";

fn host_bridge_cwd(cwd: &Path, host_bridge: bool) -> Result<Option<String>, &'static str> {
    if !host_bridge {
        return Ok(None);
    }
    cwd.to_str()
        .map(|cwd| Some(cwd.to_owned()))
        .ok_or(NON_UTF8_FLATPAK_CWD_ERROR)
}

#[derive(Default)]
struct CellIoSpawner {
    // Per-worker fault injection exercises setup failure without exhausting
    // system thread resources or changing another concurrently running cell.
    #[cfg(test)]
    fail_at: Option<usize>,
    #[cfg(test)]
    attempts: usize,
}

impl CellIoSpawner {
    fn spawn<F>(&mut self, name: &str, task: F) -> io::Result<std::thread::JoinHandle<()>>
    where
        F: FnOnce() + Send + 'static,
    {
        #[cfg(test)]
        {
            self.attempts += 1;
            if self.fail_at == Some(self.attempts) {
                return Err(io::Error::other(
                    "injected notebook I/O thread spawn failure",
                ));
            }
        }
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(task)
    }
}

fn spawn_cell_worker(spec: CommandSpec, handle: &CellHandle) -> mpsc::Receiver<WorkerEvent> {
    spawn_cell_worker_with_io_spawner(spec, handle, CellIoSpawner::default())
}

fn spawn_cell_worker_with_io_spawner(
    spec: CommandSpec,
    handle: &CellHandle,
    mut io_spawner: CellIoSpawner,
) -> mpsc::Receiver<WorkerEvent> {
    // Bound queued output as well as the rendered buffers: a command that
    // writes faster than GTK can paint applies backpressure instead of growing
    // an unbounded cross-thread queue.
    let (sender, receiver) = mpsc::sync_channel(64);
    let child_slot = handle.child.clone();
    let cancelled = handle.cancelled.clone();
    let pgid = handle.pgid.clone();
    let host_bridge = crate::host::is_flatpak();
    let cwd_for_bridge = match host_bridge_cwd(&spec.cwd, host_bridge) {
        Ok(cwd) => cwd,
        Err(error) => {
            let _ = sender.try_send(WorkerEvent::Done(CellOutcome::Failed(error.to_owned())));
            return receiver;
        }
    };
    let Some(permit) = CellWorkerPermit::acquire() else {
        let _ = sender.try_send(WorkerEvent::Done(CellOutcome::Failed(format!(
            "at most {MAX_CONCURRENT_CELL_WORKERS} notebook cells may run concurrently"
        ))));
        return receiver;
    };
    let spawn_failure_sender = sender.clone();

    let spawn = std::thread::Builder::new()
        .name("anvil-notebook-cell".to_owned())
        .spawn(move || {
            let _permit = permit;
            let executable_argv =
                crate::host::wrap_argv(&spec.argv, cwd_for_bridge.as_deref(), &[]);
            let Some((program, arguments)) = executable_argv.split_first() else {
                let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(
                    "no shell executable configured".to_owned(),
                )));
                return;
            };

            let mut command = Command::new(program);
            command
                .args(arguments)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if !host_bridge {
                command.current_dir(&spec.cwd);
            }
            #[cfg(unix)]
            command.process_group(0);

            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(format!(
                        "spawn failed: {error}"
                    ))));
                    return;
                }
            };
            if let Ok(id) = i32::try_from(child.id()) {
                pgid.store(id, Ordering::SeqCst);
            }

            let pipes = CellPipeControl::default();
            let io = (|| {
                let (Some(stdin), Some(stdout), Some(stderr)) =
                    (child.stdin.take(), child.stdout.take(), child.stderr.take())
                else {
                    return Err(io::Error::other(
                        "spawned shell did not expose all requested pipes",
                    ));
                };
                Ok((pipes.wrap(stdin)?, pipes.wrap(stdout)?, pipes.wrap(stderr)?))
            })();
            let (mut stdin, stdout, stderr) = match io {
                Ok(pipes) => pipes,
                Err(error) => {
                    terminate_child_group(&mut child);
                    let _ = child.wait();
                    pgid.store(0, Ordering::SeqCst);
                    let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(format!(
                        "pipe setup failed: {error}"
                    ))));
                    return;
                }
            };
            match child_slot.lock() {
                Ok(mut guard) => {
                    *guard = Some(child);
                    if cancelled.load(Ordering::SeqCst) {
                        if let Some(child) = guard.as_mut() {
                            terminate_child_group(child);
                        }
                    }
                }
                Err(_) => {
                    terminate_child_group(&mut child);
                    let _ = child.wait();
                    let _ = sender.send(WorkerEvent::Done(CellOutcome::Failed(
                        "child handle mutex poisoned".to_owned(),
                    )));
                    return;
                }
            }

            // Use stdin instead of a giant `-c` argument. This avoids ARG_MAX and
            // gives every supported interpreter the exact same source bytes.
            let mut io_threads = Vec::with_capacity(3);
            let source = spec.source;
            match io_spawner.spawn("anvil-notebook-stdin", move || {
                let _ = stdin.write_all(source.as_bytes());
                if !source.ends_with('\n') {
                    let _ = stdin.write_all(b"\n");
                }
            }) {
                Ok(thread) => io_threads.push(thread),
                Err(error) => {
                    fail_cell_io_setup(&child_slot, &pgid, &sender, io_threads, error, &pipes);
                    return;
                }
            }

            let stdout_sender = sender.clone();
            match io_spawner.spawn("anvil-notebook-stdout", move || {
                stream_cell_output(stdout, OutputStream::Stdout, stdout_sender);
            }) {
                Ok(thread) => io_threads.push(thread),
                Err(error) => {
                    fail_cell_io_setup(&child_slot, &pgid, &sender, io_threads, error, &pipes);
                    return;
                }
            }

            let stderr_sender = sender.clone();
            match io_spawner.spawn("anvil-notebook-stderr", move || {
                stream_cell_output(stderr, OutputStream::Stderr, stderr_sender);
            }) {
                Ok(thread) => io_threads.push(thread),
                Err(error) => {
                    fail_cell_io_setup(&child_slot, &pgid, &sender, io_threads, error, &pipes);
                    return;
                }
            }

            let exit = wait_for_shared_child(&child_slot);
            // The wait owner retires the private group before reaping. Pipe
            // holders in an escaped session are outside that ownership scope.
            pipes.retire();
            for thread in io_threads {
                let _ = thread.join();
            }
            pipes.send_tail(&sender);

            let outcome = match exit {
                Ok(_) if cancelled.load(Ordering::SeqCst) => CellOutcome::Cancelled,
                Ok(code) => CellOutcome::Exited(code),
                Err(error) => CellOutcome::Failed(format!("wait failed: {error}")),
            };
            pgid.store(0, Ordering::SeqCst);
            let _ = sender.send(WorkerEvent::Done(outcome));
        });
    if let Err(error) = spawn {
        let _ = spawn_failure_sender.try_send(WorkerEvent::Done(CellOutcome::Failed(format!(
            "worker thread spawn failed: {error}"
        ))));
    }

    receiver
}

#[derive(Debug)]
pub(crate) enum NotebookMsg {
    Open(PathBuf),
    Closed,
}

pub(crate) struct NotebookInit {
    pub(crate) parent: adw::ApplicationWindow,
    pub(crate) safe_mode: bool,
    pub(crate) configured_shell: Vec<String>,
}

pub(crate) struct NotebookModel {
    parent: adw::ApplicationWindow,
    safe_mode: bool,
    configured_shell: Vec<String>,
    runtime: Option<Rc<NotebookRuntime>>,
}

#[relm4::component(pub(crate))]
impl Component for NotebookModel {
    type Init = NotebookInit;
    type Input = NotebookMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_content_width: 880,
            set_content_height: 680,
            connect_closed => NotebookMsg::Closed,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::ScrolledWindow {
                    set_hexpand: true,
                    set_vexpand: true,

                    #[name(content)]
                    gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 12,
                        set_margin_top: 12,
                        set_margin_bottom: 12,
                        set_margin_start: 16,
                        set_margin_end: 16,
                    },
                },
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            parent: init.parent,
            safe_mode: init.safe_mode,
            configured_shell: init.configured_shell,
            runtime: None,
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        _sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            NotebookMsg::Open(path) => {
                // Safe mode is an isolated diagnostic session: do not even
                // read an external notebook, and never expose its Run buttons.
                if self.safe_mode {
                    log::warn!("notebook: ignored open request in safe mode");
                    return;
                }
                let text = match read_notebook_file(&path) {
                    Ok(text) => text,
                    Err(error) => {
                        log::warn!("notebook: cannot read {}: {error}", path.display());
                        return;
                    }
                };
                if let Some(runtime) = self.runtime.take() {
                    runtime.shutdown();
                }
                while let Some(child) = widgets.content.first_child() {
                    widgets.content.remove(&child);
                }
                let title = path
                    .file_name()
                    .map(crate::file_tree::display_os_str)
                    .unwrap_or_else(|| crate::file_tree::display_full_path(&path));
                root.set_title(&format!(
                    "Notebook: {}",
                    bounded_notebook_display(&title, 512)
                ));
                let cwd = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    });
                let configured_shell = self.configured_shell.clone();

                let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                let run_all_status = gtk::Label::new(None);
                run_all_status.set_xalign(0.0);
                run_all_status.set_hexpand(true);
                run_all_status.add_css_class("dim-label");
                run_all_status.set_visible(false);
                let open_button = gtk::Button::with_label("Open in Editor");
                open_button.add_css_class("flat");
                open_button.set_tooltip_text(Some(
                    "Opens this notebook in the desktop's default editor. The viewer does not edit or save cells.",
                ));
                let stop_all_button = gtk::Button::with_label("Stop All");
                stop_all_button.set_sensitive(false);
                let run_all_button = gtk::Button::with_label("Run All");
                run_all_button.add_css_class("suggested-action");
                actions.append(&run_all_status);
                actions.append(&open_button);
                actions.append(&stop_all_button);
                actions.append(&run_all_button);
                widgets.content.append(&actions);

                let intro = gtk::Label::new(Some(
                    "Read-only viewer. Shell fences (unlabelled, shell, bash, sh, zsh, fish, pwsh, powershell) run in isolated processes. Other languages are preview-only — copy them or open the file to edit.",
                ));
                intro.set_wrap(true);
                intro.set_xalign(0.0);
                intro.add_css_class("dim-label");
                widgets.content.append(&intro);

                let segments = parse_segments(&text);
                let mut content_truncated = segments.len() > MAX_NOTEBOOK_SEGMENTS;
                let mut cells = Vec::new();
                for segment in segments.into_iter().take(MAX_NOTEBOOK_SEGMENTS) {
                    match segment {
                        Segment::Text(text) => {
                            let text =
                                bounded_notebook_display(&text, MAX_NOTEBOOK_TEXT_SEGMENT_BYTES);
                            let label = gtk::Label::new(None);
                            label.set_use_markup(true);
                            label.set_markup(&render_text_to_pango(&text));
                            label.set_wrap(true);
                            label.set_xalign(0.0);
                            label.set_halign(gtk::Align::Fill);
                            label.set_selectable(true);
                            widgets.content.append(&label);
                        }
                        Segment::Code { lang, src } => {
                            if cells.len() >= MAX_NOTEBOOK_CELLS {
                                content_truncated = true;
                                break;
                            }
                            let cell = CellController::new(
                                cells.len(),
                                &lang,
                                &src,
                                &configured_shell,
                                &cwd,
                            );
                            widgets.content.append(&cell.frame);
                            cells.push(cell);
                        }
                    }
                }
                if content_truncated {
                    let warning = gtk::Label::new(Some(
                        "Notebook content was truncated at the safe segment/cell limit.",
                    ));
                    warning.set_xalign(0.0);
                    warning.set_wrap(true);
                    warning.add_css_class("warning");
                    widgets.content.append(&warning);
                }
                let cwd_display = bounded_notebook_display(
                    &crate::file_tree::display_full_path(&cwd),
                    MAX_NOTEBOOK_PATH_DISPLAY_BYTES,
                );
                let footer = gtk::Label::new(Some(&format!(
                    "Cells run in isolated process groups with cwd {}. `shell` and unlabeled cells use: {}. Source is provided on stdin; active terminals are never modified.",
                    cwd_display,
                    bounded_notebook_display(&configured_shell.join(" "), 4 * 1024)
                )));
                footer.set_wrap(true);
                footer.set_xalign(0.0);
                footer.set_selectable(true);
                footer.add_css_class("dim-label");
                widgets.content.append(&footer);

                let runtime = Rc::new(NotebookRuntime {
                    cells,
                    queue: RefCell::new(VecDeque::new()),
                    run_all_active: Cell::new(false),
                    closed: Cell::new(false),
                    stats: RefCell::new(RunAllStats::default()),
                    run_all_button: run_all_button.clone(),
                    stop_all_button: stop_all_button.clone(),
                    status: run_all_status,
                });
                run_all_button.set_sensitive(runtime.cells.iter().any(|cell| cell.runnable()));
                if !run_all_button.is_sensitive() {
                    run_all_button
                        .set_tooltip_text(Some("No executable shell fences in this notebook"));
                }

                let parent_for_open = self.parent.clone();
                let path_for_open = path.clone();
                open_button.connect_clicked(move |_| {
                    launch_notebook_in_editor(&path_for_open, &parent_for_open);
                });

                let weak_runtime = Rc::downgrade(&runtime);
                run_all_button.connect_clicked(move |_| {
                    if let Some(runtime) = weak_runtime.upgrade() {
                        runtime.start_run_all();
                    }
                });
                let weak_runtime = Rc::downgrade(&runtime);
                stop_all_button.connect_clicked(move |_| {
                    if let Some(runtime) = weak_runtime.upgrade() {
                        runtime.stop_all();
                    }
                });
                self.runtime = Some(runtime);
                root.present(Some(&self.parent));
            }
            NotebookMsg::Closed => {
                if let Some(runtime) = self.runtime.take() {
                    runtime.shutdown();
                }
            }
        }
    }
}

fn current_cell_handle(active: &RefCell<Option<Rc<CellHandle>>>, handle: &Rc<CellHandle>) -> bool {
    active
        .borrow()
        .as_ref()
        .is_some_and(|current| Rc::ptr_eq(current, handle))
}

/// Decode each pipe independently: a read boundary is not a UTF-8 boundary.
/// Only an incomplete trailing code point survives between output events.
struct CellOutputCapture {
    remaining: usize,
    pending: [Vec<u8>; 2],
    truncated: bool,
}

impl CellOutputCapture {
    fn new(limit: usize) -> Self {
        Self {
            remaining: limit,
            pending: Default::default(),
            truncated: false,
        }
    }

    fn append(&mut self, stream: OutputStream, bytes: &[u8]) -> (String, bool) {
        let count = bytes.len().min(self.remaining);
        self.remaining -= count;
        let first_truncation = count < bytes.len() && !self.truncated;
        self.truncated |= count < bytes.len();
        let pending = &mut self.pending[match stream {
            OutputStream::Stdout => 0,
            OutputStream::Stderr => 1,
        }];
        pending.extend_from_slice(&bytes[..count]);
        let mut text = String::new();
        loop {
            match std::str::from_utf8(pending) {
                Ok(valid) => {
                    text.push_str(valid);
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    text.push_str(
                        std::str::from_utf8(&pending[..valid]).expect("validated prefix"),
                    );
                    if let Some(invalid) = error.error_len() {
                        text.push('\u{fffd}');
                        pending.drain(..valid + invalid);
                    } else {
                        pending.drain(..valid);
                        break;
                    }
                }
            }
        }
        (text, first_truncation)
    }

    fn finish(&mut self) -> [String; 2] {
        self.pending.each_mut().map(|pending| {
            let text = String::from_utf8_lossy(pending).into_owned();
            pending.clear();
            text
        })
    }
}

struct OutputPane {
    root: gtk::Box,
    buffer: gtk::TextBuffer,
    scroll: gtk::ScrolledWindow,
}

impl OutputPane {
    fn new(title: &str, is_error: bool) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 3);
        root.set_visible(false);

        let label = gtk::Label::new(Some(title));
        label.set_xalign(0.0);
        label.add_css_class("dim-label");
        if is_error {
            label.add_css_class("error");
        }
        root.append(&label);

        let buffer = gtk::TextBuffer::new(None);
        let view = gtk::TextView::with_buffer(&buffer);
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.add_css_class("notebook-output");
        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .max_content_height(260)
            .child(&view)
            .build();
        scroll.set_propagate_natural_height(true);
        root.append(&scroll);

        Self {
            root,
            buffer,
            scroll,
        }
    }

    fn clear(&self) {
        self.buffer.set_text("");
        self.root.set_visible(false);
    }

    fn append(&self, text: &str) {
        self.root.set_visible(true);
        let mut end = self.buffer.end_iter();
        self.buffer.insert(&mut end, text);
        let adjustment = self.scroll.vadjustment();
        adjustment.set_value(adjustment.upper());
    }
}

type Completion = Box<dyn FnOnce(CellOutcome)>;

struct CellController {
    index: usize,
    frame: gtk::Frame,
    command: Option<CommandSpec>,
    run_button: gtk::Button,
    stop_button: gtk::Button,
    stdout: OutputPane,
    stderr: OutputPane,
    status: gtk::Label,
    active: RefCell<Option<Rc<CellHandle>>>,
    externally_locked: Cell<bool>,
}

impl CellController {
    fn new(
        index: usize,
        info: &str,
        source: &str,
        configured_shell: &[String],
        cwd: &Path,
    ) -> Rc<Self> {
        let source_issue = notebook_cell_issue(source);
        let argv = source_issue
            .is_none()
            .then(|| shell_argv_for_info(info, configured_shell))
            .flatten();
        let command = argv.map(|argv| CommandSpec {
            argv,
            source: source.to_owned(),
            cwd: cwd.to_path_buf(),
        });

        let frame = gtk::Frame::new(None);
        frame.add_css_class("card");
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.set_margin_top(8);
        body.set_margin_bottom(8);
        body.set_margin_start(8);
        body.set_margin_end(8);

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let language = bounded_notebook_display(language_name(info), 256);
        let language_label = gtk::Label::new(Some(if language.is_empty() {
            "shell"
        } else {
            &language
        }));
        language_label.set_xalign(0.0);
        language_label.set_hexpand(true);
        language_label.add_css_class("dim-label");
        toolbar.append(&language_label);

        let copy_button = gtk::Button::with_label("Copy");
        copy_button.add_css_class("flat");
        if source_issue.is_some() {
            copy_button.set_tooltip_text(Some(
                "Copies the original blocked source; inspect it in a control-character-aware editor before use.",
            ));
        }
        let source_for_copy = source.to_owned();
        copy_button.connect_clicked(move |_| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&source_for_copy);
            }
        });
        toolbar.append(&copy_button);

        let run_button = gtk::Button::with_label("Run");
        let stop_button = gtk::Button::with_label("Stop");
        stop_button.set_sensitive(false);
        let preview_reason = notebook_preview_reason(info, configured_shell, source);
        if command.is_some() {
            run_button.add_css_class("suggested-action");
            toolbar.append(&run_button);
            toolbar.append(&stop_button);
        } else if let Some(issue) = preview_reason {
            if source_issue.is_some() {
                language_label.add_css_class("error");
            }
            language_label.set_tooltip_text(Some(issue));
        }
        body.append(&toolbar);

        let source_buffer = gtk::TextBuffer::new(None);
        source_buffer.set_text(&bounded_notebook_display(source, MAX_NOTEBOOK_CELL_BYTES));
        let source_view = gtk::TextView::with_buffer(&source_buffer);
        source_view.set_editable(false);
        source_view.set_cursor_visible(false);
        source_view.set_monospace(true);
        source_view.set_wrap_mode(gtk::WrapMode::None);
        source_view.add_css_class("notebook-source");
        let source_scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .max_content_height(220)
            .child(&source_view)
            .build();
        source_scroll.set_propagate_natural_height(true);
        body.append(&source_scroll);

        let stdout = OutputPane::new("stdout", false);
        body.append(&stdout.root);
        let stderr = OutputPane::new("stderr", true);
        body.append(&stderr.root);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.add_css_class("dim-label");
        status.set_visible(false);
        body.append(&status);
        frame.set_child(Some(&body));

        let cell = Rc::new(Self {
            index,
            frame,
            command,
            run_button,
            stop_button,
            stdout,
            stderr,
            status,
            active: RefCell::new(None),
            externally_locked: Cell::new(false),
        });

        let weak = Rc::downgrade(&cell);
        cell.run_button.connect_clicked(move |_| {
            if let Some(cell) = weak.upgrade() {
                let _ = cell.run(None);
            }
        });
        let weak = Rc::downgrade(&cell);
        cell.stop_button.connect_clicked(move |_| {
            if let Some(cell) = weak.upgrade() {
                cell.cancel();
            }
        });
        cell
    }

    fn runnable(&self) -> bool {
        self.command.is_some()
    }

    fn is_running(&self) -> bool {
        self.active.borrow().is_some()
    }

    fn set_external_lock(&self, locked: bool) {
        self.externally_locked.set(locked);
        self.sync_buttons();
    }

    fn sync_buttons(&self) {
        let running = self.is_running();
        self.run_button
            .set_sensitive(self.runnable() && !running && !self.externally_locked.get());
        self.stop_button.set_sensitive(running);
    }

    fn cancel(&self) {
        if let Some(handle) = self.active.borrow().as_ref() {
            handle.cancel();
            self.status.set_text("Cancelling…");
            self.stop_button.set_sensitive(false);
        }
    }

    fn append_output(&self, stream: OutputStream, text: &str) {
        match stream {
            OutputStream::Stdout => self.stdout.append(text),
            OutputStream::Stderr => self.stderr.append(text),
        }
    }

    fn finish(&self, outcome: &CellOutcome) {
        self.status.remove_css_class("error");
        self.status.remove_css_class("warning");
        match outcome {
            CellOutcome::Exited(code) => {
                self.status.set_text(&format!("exit {code}"));
                if *code != 0 {
                    self.status.add_css_class("error");
                }
            }
            CellOutcome::Cancelled => {
                self.status.set_text("cancelled");
                self.status.add_css_class("warning");
            }
            CellOutcome::Failed(error) => {
                self.status.set_text(&format!("failed: {error}"));
                self.status.add_css_class("error");
            }
        }
        self.sync_buttons();
    }

    fn run(self: &Rc<Self>, completion: Option<Completion>) -> bool {
        let Some(command) = self.command.clone() else {
            return false;
        };
        if self.is_running() {
            return false;
        }

        self.stdout.clear();
        self.stderr.clear();
        self.status.set_visible(true);
        self.status.set_text("Running…");
        self.status.remove_css_class("error");
        self.status.remove_css_class("warning");

        let handle = Rc::new(CellHandle::new());
        *self.active.borrow_mut() = Some(handle.clone());
        self.sync_buttons();
        let receiver = spawn_cell_worker(command, &handle);
        let weak_cell = Rc::downgrade(self);
        let mut completion = completion;
        let mut output = CellOutputCapture::new(MAX_OUTPUT_BYTES);

        gtk::glib::timeout_add_local(OUTPUT_POLL_INTERVAL, move || {
            let Some(cell) = weak_cell.upgrade() else {
                handle.cancel();
                return gtk::glib::ControlFlow::Break;
            };
            // An obsolete timer must not write into or finish a replacement run.
            if !current_cell_handle(&cell.active, &handle) {
                handle.cancel();
                return gtk::glib::ControlFlow::Break;
            }

            let mut processed = 0usize;
            loop {
                match receiver.try_recv() {
                    Ok(WorkerEvent::Output(stream, bytes)) => {
                        let (text, first_truncation) = output.append(stream, &bytes);
                        if !text.is_empty() {
                            cell.append_output(stream, &text);
                        }
                        if first_truncation {
                            cell.stderr.append("\n[output truncated]\n");
                        }
                        processed += 1;
                        if processed >= MAX_OUTPUT_EVENTS_PER_TICK {
                            return gtk::glib::ControlFlow::Continue;
                        }
                    }
                    Ok(WorkerEvent::Done(outcome)) => {
                        let [stdout, stderr] = output.finish();
                        if !stdout.is_empty() {
                            cell.stdout.append(&stdout);
                        }
                        if !stderr.is_empty() {
                            cell.stderr.append(&stderr);
                        }
                        let is_current = cell
                            .active
                            .borrow()
                            .as_ref()
                            .is_some_and(|active| Rc::ptr_eq(active, &handle));
                        if is_current {
                            cell.active.borrow_mut().take();
                        }
                        cell.finish(&outcome);
                        if let Some(callback) = completion.take() {
                            callback(outcome);
                        }
                        return gtk::glib::ControlFlow::Break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let [stdout, stderr] = output.finish();
                        if !stdout.is_empty() {
                            cell.stdout.append(&stdout);
                        }
                        if !stderr.is_empty() {
                            cell.stderr.append(&stderr);
                        }
                        let outcome = CellOutcome::Failed("worker disconnected".to_owned());
                        cell.active.borrow_mut().take();
                        cell.finish(&outcome);
                        if let Some(callback) = completion.take() {
                            callback(outcome);
                        }
                        return gtk::glib::ControlFlow::Break;
                    }
                }
            }
        });
        true
    }
}

#[derive(Default)]
struct RunAllStats {
    total: usize,
    finished: usize,
    failed: usize,
}

struct NotebookRuntime {
    cells: Vec<Rc<CellController>>,
    queue: RefCell<VecDeque<usize>>,
    run_all_active: Cell<bool>,
    closed: Cell<bool>,
    stats: RefCell<RunAllStats>,
    run_all_button: gtk::Button,
    stop_all_button: gtk::Button,
    status: gtk::Label,
}

impl NotebookRuntime {
    fn start_run_all(self: &Rc<Self>) {
        if self.closed.get() || self.run_all_active.get() {
            return;
        }
        if self.cells.iter().any(|cell| cell.is_running()) {
            self.status
                .set_text("Wait for individually running cells, or stop them first.");
            self.status.add_css_class("warning");
            self.status.set_visible(true);
            return;
        }

        let queue: VecDeque<usize> = self
            .cells
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| cell.runnable().then_some(index))
            .collect();
        if queue.is_empty() {
            self.status.set_text("No runnable shell cells.");
            self.status.set_visible(true);
            return;
        }

        *self.stats.borrow_mut() = RunAllStats {
            total: queue.len(),
            ..RunAllStats::default()
        };
        *self.queue.borrow_mut() = queue;
        self.run_all_active.set(true);
        self.run_all_button.set_sensitive(false);
        self.stop_all_button.set_sensitive(true);
        self.status.remove_css_class("error");
        self.status.remove_css_class("warning");
        self.status.set_visible(true);
        for cell in &self.cells {
            cell.set_external_lock(true);
        }
        self.run_next();
    }

    fn run_next(self: &Rc<Self>) {
        if !self.run_all_active.get() || self.closed.get() {
            return;
        }
        let Some(index) = self.queue.borrow_mut().pop_front() else {
            self.finish_run_all();
            return;
        };

        let stats = self.stats.borrow();
        self.status.set_text(&format!(
            "Running cell {} of {}…",
            stats.finished + 1,
            stats.total
        ));
        drop(stats);

        let cell = self.cells[index].clone();
        let weak_runtime: Weak<Self> = Rc::downgrade(self);
        if !cell.run(Some(Box::new(move |outcome| {
            if let Some(runtime) = weak_runtime.upgrade() {
                runtime.cell_finished(outcome);
            }
        }))) {
            self.cell_finished(CellOutcome::Failed(format!(
                "cell {} could not start",
                cell.index + 1
            )));
        }
    }

    fn cell_finished(self: &Rc<Self>, outcome: CellOutcome) {
        if !self.run_all_active.get() {
            return;
        }
        {
            let mut stats = self.stats.borrow_mut();
            stats.finished += 1;
            if outcome.failed() {
                stats.failed += 1;
            }
        }
        self.run_next();
    }

    fn finish_run_all(&self) {
        self.run_all_active.set(false);
        self.run_all_button.set_sensitive(true);
        self.stop_all_button.set_sensitive(false);
        for cell in &self.cells {
            cell.set_external_lock(false);
        }

        let stats = self.stats.borrow();
        self.status.remove_css_class("warning");
        if stats.failed == 0 {
            self.status
                .set_text(&format!("Run All finished: {} cell(s).", stats.finished));
            self.status.remove_css_class("error");
        } else {
            self.status.set_text(&format!(
                "Run All finished: {} cell(s), {} failed.",
                stats.finished, stats.failed
            ));
            self.status.add_css_class("error");
        }
    }

    fn stop_all(&self) {
        let was_run_all = self.run_all_active.replace(false);
        self.queue.borrow_mut().clear();
        for cell in &self.cells {
            cell.cancel();
            cell.set_external_lock(false);
        }
        self.run_all_button.set_sensitive(!self.closed.get());
        self.stop_all_button.set_sensitive(false);
        if was_run_all && !self.closed.get() {
            self.status.set_text("Run All cancelled.");
            self.status.add_css_class("warning");
        }
    }

    fn shutdown(&self) {
        self.closed.set(true);
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_output_timer_requires_the_current_handle_identity() {
        let old = Rc::new(CellHandle::new());
        let current = Rc::new(CellHandle::new());
        let active = RefCell::new(Some(current.clone()));
        assert!(current_cell_handle(&active, &current));
        assert!(!current_cell_handle(&active, &old));
        *active.borrow_mut() = None;
        assert!(!current_cell_handle(&active, &current));
    }

    #[test]
    #[cfg(unix)]
    fn completed_handle_never_signals_stale_group_telemetry() {
        let mut sentinel = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let handle = CellHandle::new();
        handle.pgid.store(sentinel.id() as i32, Ordering::SeqCst);
        handle.cancel();
        std::thread::sleep(Duration::from_millis(50));
        let survived = sentinel.try_wait().unwrap().is_none();
        // This fixture owns and reaps its sentinel regardless of assertion.
        let _ = sentinel.kill();
        let _ = sentinel.wait();
        assert!(
            survived,
            "a completed cell signalled an unowned process group"
        );
    }

    #[test]
    #[cfg(unix)]
    fn externally_reaped_child_is_forgotten_without_late_signal_authority() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .spawn()
            .unwrap();
        child.wait().unwrap();
        let handle = CellHandle::new();
        *handle.child.lock().unwrap() = Some(child);
        let error = wait_for_shared_child(&handle.child).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(nix::libc::ECHILD));
        assert!(handle.child.lock().unwrap().is_none());
        handle.cancel();
    }

    fn notebook_test_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("anvil-notebook-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    #[cfg(unix)]
    fn flatpak_bridge_never_rewrites_a_non_utf8_working_directory() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let non_utf8 = PathBuf::from(OsString::from_vec(b"/tmp/notebook-\xff".to_vec()));
        assert_eq!(host_bridge_cwd(&non_utf8, false), Ok(None));
        assert_eq!(
            host_bridge_cwd(&non_utf8, true),
            Err(NON_UTF8_FLATPAK_CWD_ERROR)
        );
        assert_eq!(
            host_bridge_cwd(Path::new("/tmp/notebook"), true),
            Ok(Some("/tmp/notebook".to_owned()))
        );
    }

    #[test]
    fn notebook_input_is_regular_utf8_and_bounded() {
        let dir = notebook_test_dir("bounded");
        let valid = dir.join("valid.md");
        std::fs::write(&valid, "# hello\n").unwrap();
        assert_eq!(read_notebook_file(&valid).unwrap(), "# hello\n");

        let invalid = dir.join("invalid.md");
        std::fs::write(&invalid, [0xff]).unwrap();
        assert_eq!(
            read_notebook_file(&invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let oversized = dir.join("oversized.md");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_NOTEBOOK_BYTES + 1).unwrap();
        assert_eq!(
            read_notebook_file(&oversized).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn notebook_fifo_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = notebook_test_dir("fifo");
        let path = dir.join("blocked.md");
        let path_c = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path_c is a live NUL-terminated pathname for this call.
        assert_eq!(unsafe { nix::libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(
            read_notebook_file(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn notebook_symlink_is_rejected_but_the_real_file_reads() {
        let dir = notebook_test_dir("nofollow");
        let real = dir.join("real.md");
        std::fs::write(&real, "# real\n").unwrap();
        let link = dir.join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(read_notebook_file(&link).is_err());
        assert_eq!(read_notebook_file(&real).unwrap(), "# real\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn executable_cells_reject_hidden_controls_and_oversize_source() {
        assert_eq!(notebook_cell_issue("echo one\necho two\t# ok"), None);
        assert!(notebook_cell_issue("echo safe\u{202e}txt").is_some());
        assert!(notebook_cell_issue("echo safe\u{00ad}txt").is_some());
        assert!(notebook_cell_issue("echo safe\u{e0020}txt").is_some());
        assert!(notebook_cell_issue("printf '\u{1b}'").is_some());
        assert!(notebook_cell_issue(&"x".repeat(MAX_NOTEBOOK_CELL_BYTES + 1)).is_some());

        let display = bounded_notebook_display("safe\u{200b}\u{1b}text", 1024);
        assert_eq!(display, "safe��text");
        let truncated = bounded_notebook_display(&"界".repeat(100), 16);
        assert!(truncated.ends_with('…'));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn shell_fences_select_the_requested_interpreter() {
        let configured = vec!["/bin/zsh".to_owned(), "-l".to_owned()];
        assert_eq!(
            shell_argv_for_info("shell", &configured),
            Some(configured.clone())
        );
        assert_eq!(
            shell_argv_for_info("", &configured),
            Some(configured.clone())
        );
        for shell in ["bash", "sh", "zsh", "fish"] {
            assert_eq!(
                shell_argv_for_info(&format!("{shell} title=demo"), &configured),
                Some(vec![shell.to_owned()])
            );
        }
        assert_eq!(
            shell_argv_for_info("pwsh", &configured),
            Some(vec![
                "pwsh".to_owned(),
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                "-".to_owned(),
            ])
        );
        assert_eq!(
            shell_argv_for_info("powershell", &configured).and_then(|argv| argv.first().cloned()),
            Some("powershell".to_owned())
        );
        assert_eq!(shell_argv_for_info("python", &configured), None);
        assert_eq!(shell_argv_for_info("shell", &[]), None);
        assert!(notebook_preview_reason("python", &configured, "print(1)").is_some());
        assert!(notebook_preview_reason("bash", &configured, "echo ok").is_none());
        assert!(notebook_preview_reason("bash", &configured, "echo safe\u{202e}txt").is_some());
    }

    #[test]
    fn worker_keeps_stdout_and_stderr_separate() {
        let handle = CellHandle::new();
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".to_owned()],
                source: "printf out; printf err >&2; exit 7".to_owned(),
                cwd: std::env::temp_dir(),
            },
            &handle,
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = loop {
            match receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("worker event")
            {
                WorkerEvent::Output(OutputStream::Stdout, bytes) => stdout.extend(bytes),
                WorkerEvent::Output(OutputStream::Stderr, bytes) => stderr.extend(bytes),
                WorkerEvent::Done(outcome) => break outcome,
            }
        };
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
        assert_eq!(outcome, CellOutcome::Exited(7));
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_kills_and_reaps_the_entire_process_group() {
        let handle = CellHandle::new();
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".to_owned()],
                source: "sleep 30 & echo ready; wait".to_owned(),
                cwd: std::env::temp_dir(),
            },
            &handle,
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut stdout = Vec::new();
        while !stdout
            .windows(b"ready".len())
            .any(|chunk| chunk == b"ready")
        {
            assert!(std::time::Instant::now() < deadline, "cell did not start");
            match receiver
                .recv_timeout(Duration::from_millis(100))
                .expect("ready output")
            {
                WorkerEvent::Output(OutputStream::Stdout, bytes) => {
                    stdout.extend(bytes);
                }
                WorkerEvent::Output(OutputStream::Stderr, _) => {}
                WorkerEvent::Done(outcome) => panic!("cell ended before cancellation: {outcome:?}"),
            }
        }
        let group = handle.pgid.load(Ordering::SeqCst);
        assert!(group > 0, "worker did not publish its process group");
        handle.cancel();

        let outcome = loop {
            match receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("cancellation outcome")
            {
                WorkerEvent::Done(outcome) => break outcome,
                WorkerEvent::Output(_, _) => {}
            }
        };
        assert_eq!(outcome, CellOutcome::Cancelled);
        assert!(handle.child.lock().expect("child slot").is_none());

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            // Signal 0 checks group existence without modifying it.
            let exists = unsafe { nix::libc::kill(-group, 0) } == 0;
            if !exists {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a descendant survived notebook cancellation"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[cfg(unix)]
    fn normal_parent_exit_kills_background_pipe_holders_before_joining_readers() {
        let handle = CellHandle::new();
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".to_owned()],
                source: "sleep 30 & echo parent-done".to_owned(),
                cwd: std::env::temp_dir(),
            },
            &handle,
        );

        let mut stdout = Vec::new();
        let outcome = loop {
            match receiver
                .recv_timeout(Duration::from_secs(3))
                .expect("worker must not hang on a descendant holding stdout")
            {
                WorkerEvent::Output(OutputStream::Stdout, bytes) => stdout.extend(bytes),
                WorkerEvent::Output(OutputStream::Stderr, _) => {}
                WorkerEvent::Done(outcome) => break outcome,
            }
        };
        assert_eq!(outcome, CellOutcome::Exited(0));
        assert!(stdout.windows(11).any(|chunk| chunk == b"parent-done"));
        assert!(handle.child.lock().expect("child slot").is_none());
        assert_eq!(handle.pgid.load(Ordering::SeqCst), 0);
    }
}

#[cfg(all(test, unix))]
mod pipe_cleanup_tests {
    use super::*;
    use std::time::Instant;

    static PIPE_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct PipeFixture {
        root: PathBuf,
        handle: CellHandle,
    }

    impl PipeFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "notebook-pipe-fixture-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            Self {
                root,
                handle: CellHandle::new(),
            }
        }

        fn release_holder(&self) -> bool {
            // Only this fixture knows these private release paths. No process
            // enumeration or signals to an escaped numeric PID are needed.
            let _ = std::fs::write(self.root.join("release-holder"), b"");
            let deadline = Instant::now() + Duration::from_secs(3);
            while !self.root.join("holder-closed").exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.root.join("holder-closed").exists()
        }
    }

    impl Drop for PipeFixture {
        fn drop(&mut self) {
            self.handle.cancel();
            let _ = std::fs::write(self.root.join("release-root"), b"");
            if self.release_holder() {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
    }

    fn escaped_pipe_case(pipes: &str, cancel: bool) {
        let _serial = PIPE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let fixture = PipeFixture::new();
        let redirections = match pipes {
            "stdin" => "<&3 3<&- >/dev/null 2>/dev/null",
            "stdout" => "</dev/null 3<&- 2>/dev/null",
            "stderr" => "</dev/null 3<&- >/dev/null",
            "all" => "<&3 3<&-",
            _ => unreachable!(),
        };
        // The private holder announces readiness only after setsid. It holds
        // precisely the selected pipe ends, and exits on our release file.
        // A fixed safety ceiling also prevents leaks after a test-process crash.
        let script = format!(
            "exec 3<&0; setsid sh -c 'printf ready > holder-ready; n=0; \
             while [ ! -e release-holder ] && [ \"$n\" -lt 1500 ]; do \
             sleep 0.02; n=$((n+1)); done; \
             exec 0<&- 1>&- 2>&-; : > holder-closed' {redirections} & exec 3<&-; \
             while [ ! -e holder-ready ]; do sleep 0.01; done; \
             printf ready > root-ready; \
             while [ ! -e release-root ]; do sleep 0.01; done; \
             printf ordinary-out-tail; printf ordinary-err-tail >&2; exit 7"
        );
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".to_owned(), "-c".to_owned(), script],
                // The root deliberately does not consume this stdin. A source
                // larger than pipe capacity exercises the writer as well.
                source: "x".repeat(256 * 1024),
                cwd: fixture.root.clone(),
            },
            &fixture.handle,
        );
        let ready_deadline = Instant::now() + Duration::from_secs(3);
        while !fixture.root.join("root-ready").exists() {
            assert!(
                Instant::now() < ready_deadline,
                "fixture never acknowledged startup"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fixture.root.join("holder-ready").exists());
        let started = Instant::now();
        if cancel {
            fixture.handle.cancel();
        } else {
            std::fs::write(fixture.root.join("release-root"), b"").unwrap();
        }
        let deadline = started + Duration::from_secs(1);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut outcome = None;
        while Instant::now() < deadline {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(WorkerEvent::Output(OutputStream::Stdout, bytes)) => stdout.extend(bytes),
                Ok(WorkerEvent::Output(OutputStream::Stderr, bytes)) => stderr.extend(bytes),
                Ok(WorkerEvent::Done(done)) => {
                    outcome = Some(done);
                    break;
                }
                Err(_) => break,
            }
        }
        let elapsed = started.elapsed();
        let holder_was_still_live = !fixture.root.join("holder-closed").exists();
        let reaped = fixture.handle.child.lock().unwrap().is_none();
        // Clean up even against the old implementation, before asserting the
        // regression, so a failed test never strands its descriptor holder.
        assert!(
            fixture.release_holder(),
            "fixture holder failed to close its own pipes"
        );
        if outcome.is_none() {
            let cleanup_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < cleanup_deadline {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(WorkerEvent::Done(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    _ => {}
                }
            }
        }
        eprintln!("{pipes}, cancel={cancel}: completion={outcome:?}, elapsed={elapsed:?}, root_reaped={reaped}, escaped_holder_survived={holder_was_still_live}");
        assert!(
            holder_was_still_live,
            "worker must not terminate the escaped session"
        );
        assert!(reaped, "the ordinary wait owner must reap the root");
        assert_eq!(
            outcome,
            Some(if cancel {
                CellOutcome::Cancelled
            } else {
                CellOutcome::Exited(7)
            }),
            "worker hung on escaped {pipes} descriptors"
        );
        if !cancel {
            assert_eq!(stdout, b"ordinary-out-tail");
            assert_eq!(stderr, b"ordinary-err-tail");
        }
    }

    #[test]
    fn escaped_stdout_does_not_hold_completion() {
        escaped_pipe_case("stdout", false);
    }

    #[test]
    fn escaped_stderr_does_not_hold_completion() {
        escaped_pipe_case("stderr", false);
    }

    #[test]
    fn escaped_stdin_does_not_hold_completion() {
        escaped_pipe_case("stdin", false);
    }

    #[test]
    fn cancellation_finishes_with_all_escaped_pipe_ends() {
        escaped_pipe_case("all", true);
    }

    #[test]
    fn source_and_output_cross_pipe_capacity_without_losing_the_tail() {
        let _serial = PIPE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let handle = CellHandle::new();
        let out = "o".repeat(72 * 1024);
        let err = "e".repeat(72 * 1024);
        let source = format!("printf '%s' '{out}'; printf '%s' '{err}' >&2; printf out-tail; printf err-tail >&2; exit 7");
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".into()],
                source,
                cwd: std::env::temp_dir(),
            },
            &handle,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = loop {
            match receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("ordinary source/output must finish")
            {
                WorkerEvent::Output(OutputStream::Stdout, bytes) => stdout.extend(bytes),
                WorkerEvent::Output(OutputStream::Stderr, bytes) => stderr.extend(bytes),
                WorkerEvent::Done(outcome) => break outcome,
            }
        };
        assert_eq!(outcome, CellOutcome::Exited(7));
        assert_eq!(stdout, format!("{out}out-tail").as_bytes());
        assert_eq!(stderr, format!("{err}err-tail").as_bytes());
        assert!(handle.child.lock().unwrap().is_none());
    }
    #[test]
    fn slow_consumer_keeps_the_roots_buffered_output_tail() {
        let _serial = PIPE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let handle = CellHandle::new();
        let words = (0..100).map(|_| "x").collect::<Vec<_>>().join(" ");
        let receiver = spawn_cell_worker(
            CommandSpec {
                argv: vec!["sh".into()],
                source: format!(
                    "for value in {words}; do printf x; sleep 0.02; done; printf final-tail"
                ),
                cwd: std::env::temp_dir(),
            },
            &handle,
        );
        let mut stdout = Vec::new();
        // A real output event acknowledges that the root and reader started.
        while stdout.is_empty() {
            match receiver.recv_timeout(Duration::from_secs(3)).unwrap() {
                WorkerEvent::Output(OutputStream::Stdout, bytes) => stdout.extend(bytes),
                WorkerEvent::Done(outcome) => panic!("root ended before handshake: {outcome:?}"),
                _ => {}
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.child.lock().unwrap().is_some() {
            assert!(Instant::now() < deadline, "root did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Simulate a busy UI with a full output queue throughout retirement.
        std::thread::sleep(Duration::from_millis(200));
        let outcome = loop {
            match receiver.recv_timeout(Duration::from_secs(2)).unwrap() {
                WorkerEvent::Output(OutputStream::Stdout, bytes) => stdout.extend(bytes),
                WorkerEvent::Done(outcome) => break outcome,
                _ => {}
            }
        };
        assert_eq!(outcome, CellOutcome::Exited(0));
        assert_eq!(stdout, format!("{}final-tail", "x".repeat(100)).as_bytes());
    }

    #[test]
    fn deferred_output_is_bounded_even_when_bytes_are_always_ready() {
        // Deterministic adapter for a continuously readable escaped writer.
        // No poll is reached: the control is already retired before the read.
        struct AlwaysReady;
        impl Read for AlwaysReady {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                bytes.fill(b'x');
                Ok(bytes.len())
            }
        }
        impl std::os::fd::AsRawFd for AlwaysReady {
            fn as_raw_fd(&self) -> std::os::fd::RawFd {
                -1
            }
        }
        let control = CellPipeControl::default();
        *control.retired.lock().unwrap() = Some(Instant::now() - Duration::from_secs(1));
        let (sender, receiver) = mpsc::sync_channel(1);
        stream_cell_output(
            CellPipe {
                inner: AlwaysReady,
                control: control.clone(),
            },
            OutputStream::Stdout,
            sender.clone(),
        );
        assert_eq!(
            control.deferred_output.lock().unwrap()[0].len(),
            MAX_OUTPUT_BYTES + 1
        );
        assert!(control.deferred_output.lock().unwrap()[1].is_empty());
        control.send_tail(&sender);
        match receiver.recv().unwrap() {
            WorkerEvent::Output(OutputStream::Stdout, bytes) => {
                assert_eq!(bytes.len(), MAX_OUTPUT_BYTES + 1)
            }
            _ => panic!("missing bounded final output"),
        }
        assert!(control
            .deferred_output
            .lock()
            .unwrap()
            .iter()
            .all(Vec::is_empty));
    }

    #[test]
    fn output_backpressure_retires_without_waiting_for_a_consumer() {
        let control = CellPipeControl::default();
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender
            .send(WorkerEvent::Output(OutputStream::Stdout, vec![1]))
            .unwrap();
        let reader_control = control.clone();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (done_sender, done_receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            ready_sender.send(()).unwrap();
            let sent = reader_control
                .send_output(&sender, WorkerEvent::Output(OutputStream::Stdout, vec![2]));
            done_sender.send(sent.is_ok()).unwrap();
        });
        ready_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done_receiver
            .recv_timeout(Duration::from_millis(30))
            .is_err());
        let started = Instant::now();
        control.retire();
        assert!(!done_receiver.recv_timeout(Duration::from_secs(1)).unwrap());
        thread.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn retirement_drains_available_tail_then_bounds_an_open_reader() {
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let control = CellPipeControl::default();
        let mut reader = control.wrap(reader).unwrap();
        writer.write_all(b"tail").unwrap();
        control.retire();
        let first_retirement = *control.retired.lock().unwrap();
        let mut tail = [0u8; 4];
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"tail");
        control.retire();
        assert_eq!(*control.retired.lock().unwrap(), first_retirement);
        let started = Instant::now();
        let error = reader.read(&mut tail).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        // The peer is still owned and open; timeout did not depend on its EOF.
        writer.write_all(b"still-owned").unwrap();
    }

    #[test]
    fn pipe_setup_rejects_invalid_descriptors_without_io_workers() {
        struct InvalidDescriptor;
        impl std::os::fd::AsRawFd for InvalidDescriptor {
            fn as_raw_fd(&self) -> std::os::fd::RawFd {
                -1
            }
        }
        let result = CellPipeControl::default().wrap(InvalidDescriptor);
        assert!(matches!(result, Err(error) if error.raw_os_error() == Some(nix::libc::EBADF)));
    }

    #[test]
    fn partial_io_setup_failure_retires_readers_and_reaps_the_owned_root() {
        let _serial = PIPE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let child = Command::new("sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let handle = CellHandle::new();
        handle.pgid.store(child.id() as i32, Ordering::SeqCst);
        *handle.child.lock().unwrap() = Some(child);
        let control = CellPipeControl::default();
        let (reader, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut reader = control.wrap(reader).unwrap();
        let (read_sender, read_receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            read_sender
                .send(reader.read(&mut [0u8; 1]).map_err(|error| error.kind()))
                .unwrap();
        });
        let (sender, receiver) = mpsc::sync_channel(1);
        let started = Instant::now();
        fail_cell_io_setup(
            &handle.child,
            &handle.pgid,
            &sender,
            vec![thread],
            io::Error::other("fixture spawn failure"),
            &control,
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(read_receiver.recv().unwrap(), Err(io::ErrorKind::TimedOut));
        assert!(handle.child.lock().unwrap().is_none());
        assert_eq!(handle.pgid.load(Ordering::SeqCst), 0);
        assert!(
            matches!(receiver.recv().unwrap(), WorkerEvent::Done(CellOutcome::Failed(error)) if error.contains("fixture spawn failure"))
        );
        assert!(peer.write_all(b"closed-reader").is_err());
    }
    #[test]
    fn each_io_thread_spawn_failure_closes_workers_and_reaps_the_root() {
        let _serial = PIPE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for fail_at in 1..=3 {
            let handle = CellHandle::new();
            let started = Instant::now();
            let receiver = spawn_cell_worker_with_io_spawner(
                CommandSpec {
                    argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
                    source: "x".repeat(256 * 1024),
                    cwd: std::env::temp_dir(),
                },
                &handle,
                CellIoSpawner {
                    fail_at: Some(fail_at),
                    attempts: 0,
                },
            );
            let deadline = started + Duration::from_secs(2);
            let outcome = loop {
                if let WorkerEvent::Done(outcome) = receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("injected spawn failure must finish")
                {
                    break outcome;
                }
            };
            assert!(
                matches!(outcome, CellOutcome::Failed(ref error) if error.contains("injected notebook I/O thread spawn failure")),
                "{outcome:?}"
            );
            assert!(handle.child.lock().unwrap().is_none());
            assert_eq!(handle.pgid.load(Ordering::SeqCst), 0);
            // Disconnection proves no previously started I/O worker retains a
            // channel sender after Done and the worker closure has returned.
            assert!(matches!(
                receiver.recv_timeout(Duration::from_secs(1)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ));
            eprintln!(
                "I/O spawn position {fail_at}: controlled failure and cleanup in {:?}",
                started.elapsed()
            );
        }
    }
}

#[cfg(test)]
mod output_capture_tests {
    use super::{CellOutputCapture, OutputStream};

    #[test]
    fn output_preserves_utf8_at_every_pipe_boundary() {
        let source = "aé中文🙂z";
        for split in 0..=source.len() {
            let mut capture = CellOutputCapture::new(1024);
            let mut text = capture
                .append(OutputStream::Stdout, &source.as_bytes()[..split])
                .0;
            text.push_str(
                &capture
                    .append(OutputStream::Stdout, &source.as_bytes()[split..])
                    .0,
            );
            text.push_str(&capture.finish()[0]);
            assert_eq!(text, source, "split at byte {split}");
        }
    }

    #[test]
    fn output_streams_keep_independent_utf8_tails() {
        let mut capture = CellOutputCapture::new(1024);
        assert_eq!(capture.append(OutputStream::Stdout, &[0xe4, 0xb8]).0, "");
        assert_eq!(
            capture.append(OutputStream::Stderr, b"warning").0,
            "warning"
        );
        assert_eq!(capture.append(OutputStream::Stdout, &[0xad]).0, "中");
        assert_eq!(capture.finish(), ["", ""]);
    }

    #[test]
    fn output_flushes_invalid_and_incomplete_bytes_on_completion() {
        let mut capture = CellOutputCapture::new(1024);
        assert_eq!(
            capture
                .append(OutputStream::Stdout, &[0xff, b'a', 0xf0, 0x9f])
                .0,
            "�a"
        );
        assert_eq!(capture.finish(), ["�", ""]);
        assert_eq!(capture.finish(), ["", ""]);
    }

    #[test]
    fn final_crossing_chunk_reports_truncation_once() {
        let mut capture = CellOutputCapture::new(5);
        assert_eq!(
            capture.append(OutputStream::Stdout, b"abc"),
            ("abc".into(), false)
        );
        assert_eq!(
            capture.append(OutputStream::Stderr, b"def"),
            ("de".into(), true)
        );
        assert_eq!(
            capture.append(OutputStream::Stdout, b"more"),
            ("".into(), false)
        );
        assert_eq!(capture.finish(), ["", ""]);
    }

    #[test]
    fn exact_limit_is_complete_until_another_byte_arrives() {
        let mut capture = CellOutputCapture::new(3);
        assert_eq!(
            capture.append(OutputStream::Stdout, "中".as_bytes()),
            ("中".into(), false)
        );
        assert_eq!(
            capture.append(OutputStream::Stdout, b""),
            ("".into(), false)
        );
        assert_eq!(
            capture.append(OutputStream::Stderr, b"!"),
            ("".into(), true)
        );
        let mut zero = CellOutputCapture::new(0);
        assert_eq!(zero.append(OutputStream::Stdout, b"x"), ("".into(), true));
    }

    #[test]
    fn truncating_a_code_point_flushes_only_its_retained_prefix() {
        let mut capture = CellOutputCapture::new(2);
        assert_eq!(
            capture.append(OutputStream::Stdout, "中".as_bytes()),
            ("".into(), true)
        );
        assert_eq!(capture.finish(), ["�", ""]);
    }
}
