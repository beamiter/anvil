//! GTK backend integration for the canonical, bounded PTY process observer.
//! VTE owns I/O; this adapter owns waiting. The child remains unreaped until
//! its attached session has drained, including jobs in other process groups.
use companion_process::{
    pty_session::{Activity, PtySession},
    shell_identity::ShellIdentity,
};
use jterm_core::process::ChildLifecycle;
use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};
use vte4::{
    gtk::{glib, prelude::*},
    Terminal, TerminalExt,
};

/// EOF on this close-on-exec pipe proves the forked child has completed its
/// session/PTY setup and entered exec (or exited). Do not capture SID before it.
pub(crate) fn exec_barrier() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [-1; 2];
    // SAFETY: pipe2 initializes both descriptors on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    })
}

pub(crate) fn wait_exec_barrier(
    read: std::os::fd::OwnedFd,
    write: std::os::fd::OwnedFd,
) -> std::io::Result<()> {
    use std::io::Read;
    drop(write);
    let mut file = std::fs::File::from(read);
    loop {
        match file.read(&mut [0]) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(std::io::Error::other("unexpected PTY launch barrier data")),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Preserve a bounded amount of already-readable final output. A background
/// writer cannot prevent exit forever by keeping the PTY readable.
pub(crate) fn drain_tail(file: &mut std::fs::File, mut emit: impl FnMut(Vec<u8>) -> bool) {
    use std::{io::Read, os::fd::AsRawFd};
    let mut remaining = 1024 * 1024;
    let mut buf = [0u8; 32 * 1024];
    while remaining > 0 {
        let mut fd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, zero wait; read only when readable.
        if unsafe { libc::poll(&mut fd, 1, 0) } <= 0 || fd.revents & libc::POLLIN == 0 {
            break;
        }
        match file.read(&mut buf[..remaining.min(32 * 1024)]) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                remaining -= n;
                if !emit(buf[..n].to_vec()) {
                    break;
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Guard {
    pid: i32,
    session: PtySession,
    shell: ShellIdentity,
}

impl Guard {
    pub(crate) fn capture(pid: i32, program: Option<&Path>) -> Arc<Self> {
        let guard = Arc::new(Self {
            pid,
            session: PtySession::capture(pid),
            shell: ShellIdentity::capture(pid, program),
        });
        log::debug!(target: "eva_native_host", "PTY session captured {:?}", guard.session);
        guard
    }

    /// Capture the exact currently confirmed process image for this close,
    /// independently of the expected interactive shell. This only detects
    /// subsequent work/image changes; it never admits an ordinary close.
    pub(crate) fn withdrawal_work_probe(self: &Arc<Self>) -> Box<dyn Fn() -> bool> {
        let image = std::path::PathBuf::from(format!("/proc/{}/exe", self.pid));
        let program = ShellIdentity::capture(self.pid, Some(&image));
        let guard = Arc::clone(self);
        Box::new(move || {
            guard.session.activity() != Activity::Idle || !program.matches_current_program()
        })
    }

    pub(crate) fn blocker(&self, empty_prompt: bool, foreground: Option<String>) -> Option<String> {
        if foreground.is_some() {
            return foreground;
        }
        match self.session.activity() {
            Activity::Busy => Some("Attached shell job".into()),
            Activity::Unknown => Some("Shell activity could not be verified".into()),
            Activity::Idle if !empty_prompt || !self.shell.matches_current_program() => {
                Some("Shell is not at a verified empty prompt".into())
            }
            Activity::Idle => None,
        }
    }

    /// A cached exit was already drained by the explicit termination owner.
    /// Otherwise do not call the older core's poll_reap until our full-session
    /// drain succeeds: that call would immediately release the leader anchor.
    pub(crate) fn ready_to_reap(&self, child: &ChildLifecycle) -> bool {
        if child.exit_code().is_some() {
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            match self.session.drain_after_exit() {
                Ok(()) => true,
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::WouldBlock {
                        log::debug!(target: "eva_native_host", "PTY session drain deferred {:?}: {error}", self.session);
                    }
                    false
                }
            }
        }
        // The bounded /proc observer is Linux-specific. Preserve the former
        // non-Linux reap path rather than interpreting its no-op drain as exit.
        #[cfg(not(target_os = "linux"))]
        {
            child.poll_reap().is_some()
        }
    }

    pub(crate) fn poll_reap(&self, child: &ChildLifecycle) -> Option<i32> {
        if self.ready_to_reap(child) {
            child.poll_reap()
        } else {
            None
        }
    }
}

/// One launch exists before fork and stays attached to its window until the
/// outcome was delivered AND any created child completed the owned drain.
/// Cancellation is intent; it is never a reaping or completion receipt.
pub(crate) struct PendingLaunch {
    closing: Arc<AtomicBool>,
    settled: Cell<bool>,
    child: RefCell<Option<Arc<ChildLifecycle>>>,
    terminal: glib::WeakRef<Terminal>,
}

impl PendingLaunch {
    pub(crate) fn begin(terminal: &Terminal) -> Rc<Self> {
        let closed = vte_closed(terminal);
        let launch = Rc::new(Self {
            closing: Arc::new(AtomicBool::new(closed)),
            settled: Cell::new(false),
            child: RefCell::new(None),
            terminal: terminal.downgrade(),
        });
        unsafe {
            terminal.set_data("eva-pending-launch", Rc::clone(&launch));
        }
        terminal.set_input_enabled(!closed);
        log::debug!(target: "eva_native_host", "GTK VTE launch pending");
        launch
    }

    pub(crate) fn closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub(crate) fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.closing)
    }

    pub(crate) fn acquired(&self, child: &Arc<ChildLifecycle>) {
        *self.child.borrow_mut() = Some(Arc::clone(child));
    }

    pub(crate) fn settle(&self) {
        self.settled.set(true);
        log::debug!(target: "eva_native_host", "GTK VTE launch settled owned={} closing={}",
            self.child.borrow().is_some(), self.closing());
    }

    pub(crate) fn settled(&self) -> bool {
        self.settled.get()
    }

    pub(crate) fn ready(&self) -> bool {
        self.settled.get()
            && self
                .child
                .borrow()
                .as_ref()
                .is_none_or(|child| child.exit_code().is_some())
    }

    /// Only an irreversible pane/window teardown calls this. The spawn worker
    /// observes the request while retaining the original wait ownership.
    fn close(&self) {
        self.closing.store(true, Ordering::Release);
        if let Some(terminal) = self.terminal.upgrade() {
            unsafe {
                terminal.set_data("eva-child-closed", true);
            }
            terminal.set_input_enabled(false);
        }
        if self.settled.get() {
            let child = self.child.borrow().clone();
            if let Some(child) = child {
                child.terminate(jterm_core::process::EscalationPolicy::SESSION_DRAIN);
            }
        }
    }
}

pub(crate) fn vte_launch(terminal: &Terminal) -> Option<Rc<PendingLaunch>> {
    unsafe {
        terminal
            .data::<Rc<PendingLaunch>>("eva-pending-launch")
            .map(|launch| Rc::clone(launch.as_ref()))
    }
}

pub(crate) fn vte_closed(terminal: &Terminal) -> bool {
    unsafe {
        terminal
            .data::<bool>("eva-child-closed")
            .is_some_and(|closed| *closed.as_ref())
    }
}

#[derive(Clone)]
pub(crate) enum ChildDrain {
    Owned(Arc<ChildLifecycle>),
    Launch(Rc<PendingLaunch>),
}

impl From<Arc<ChildLifecycle>> for ChildDrain {
    fn from(child: Arc<ChildLifecycle>) -> Self {
        Self::Owned(child)
    }
}

impl ChildDrain {
    fn ready(&self) -> bool {
        match self {
            Self::Owned(child) => child.exit_code().is_some(),
            Self::Launch(launch) => launch.ready(),
        }
    }

    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Owned(a), Self::Owned(b)) => Arc::ptr_eq(a, b),
            (Self::Launch(a), Self::Launch(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    fn terminate(&self) {
        match self {
            Self::Owned(child) => {
                child.terminate(jterm_core::process::EscalationPolicy::SESSION_DRAIN);
            }
            Self::Launch(launch) => launch.close(),
        }
    }
}

pub(crate) fn vte_cleanup(terminal: &Terminal) -> Option<ChildDrain> {
    vte_launch(terminal)
        .map(ChildDrain::Launch)
        .or_else(|| vte_child(terminal).map(ChildDrain::Owned))
}

/// Retain exact child handles before their pane leaves the widget/model tree.
/// The collection belongs to one window; closing one window never drains the
/// children of another. Finished handles are pruned on the next close action.
#[derive(Default)]
struct PendingChildren {
    children: Vec<ChildDrain>,
    waiting: bool,
}

impl PendingChildren {
    fn retain(&mut self, children: Vec<ChildDrain>) {
        self.children.retain(|child| !child.ready());
        for child in children {
            if !child.ready() && !self.children.iter().any(|known| known.same(&child)) {
                self.children.push(child);
            }
        }
    }
}

fn pending_children(window: &vte4::gtk::Window) -> Rc<RefCell<PendingChildren>> {
    const KEY: &str = "eva-pending-children";
    // The only object-data boundary for this exact type/key. GTK access and
    // registry mutation stay on the window's main context.
    unsafe {
        if let Some(state) = window.data::<Rc<RefCell<PendingChildren>>>(KEY) {
            return Rc::clone(state.as_ref());
        }
        let state = Rc::new(RefCell::new(PendingChildren::default()));
        window.set_data(KEY, Rc::clone(&state));
        state
    }
}

pub(crate) fn retain_children(window: &vte4::gtk::Window, children: Vec<ChildDrain>) {
    let state = pending_children(window);
    state.borrow_mut().retain(children);
    log::debug!(target: "eva_native_host", "GTK child handles retained count={}", state.borrow().children.len());
}

/// Keep GTK alive until active AND already-detached children finish cleanup.
/// An empty current tab list cannot bypass an earlier asynchronous drain.
/// A second close request joins the existing wait; its callback is not run in
/// parallel. Newly detached panes join the same per-window collection.
pub(crate) fn defer_children(
    window: &vte4::gtk::Window,
    children: Vec<ChildDrain>,
    on_ready: impl FnOnce() + 'static,
) -> bool {
    let state = pending_children(window);
    let children = {
        let mut state = state.borrow_mut();
        state.retain(children);
        if state.waiting {
            return true;
        }
        if state.children.is_empty() {
            return false;
        }
        state.waiting = true;
        state.children.clone()
    };
    log::debug!(target: "eva_native_host", "GTK child drain pending count={}", children.len());
    window.set_sensitive(false);
    for child in &children {
        child.terminate();
    }
    let window = window.downgrade();
    let mut on_ready = Some(on_ready);
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let Some(_window) = window.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let children = state.borrow().children.clone();
        for child in &children {
            child.terminate();
        }
        let done = {
            let mut state = state.borrow_mut();
            state.retain(Vec::new());
            if state.children.is_empty() {
                state.waiting = false;
                true
            } else {
                false
            }
        };
        if done {
            log::debug!(target: "eva_native_host", "GTK child drain finished");
            on_ready.take().unwrap()();
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
    true
}

pub(crate) fn vte_child(terminal: &Terminal) -> Option<Arc<ChildLifecycle>> {
    unsafe {
        terminal
            .data::<Arc<ChildLifecycle>>("eva-owned-child")
            .map(|child| Arc::clone(child.as_ref()))
    }
}

/// Startup input belongs to the exact admitted child, even when a terminal
/// widget survives a failed launch or is reused for another generation.
pub(crate) fn can_feed(terminal: &Terminal, child: &Arc<ChildLifecycle>) -> bool {
    !vte_closed(terminal)
        && terminal.root().is_some()
        && child.exit_code().is_none()
        && vte_child(terminal).is_some_and(|current| Arc::ptr_eq(&current, child))
}

pub(crate) fn schedule_initial_commands(
    terminal: &Terminal,
    child: Arc<ChildLifecycle>,
    commands: impl FnOnce() -> Vec<String> + 'static,
) {
    let terminal = terminal.clone();
    glib::timeout_add_local_once(Duration::from_millis(500), move || {
        if !can_feed(&terminal, &child) {
            return;
        }
        for command in commands() {
            if !can_feed(&terminal, &child) {
                break;
            }
            terminal.feed_child(format!("{command}\r").as_bytes());
        }
    });
}

/// Emit the same raw wait-status-shaped signal the VTE watcher exposed, after
/// our exclusive reaper finishes. VTE must NOT also watch this child. Only the
/// family's normalized code is public, so encode it as a normal exit status.
pub(crate) fn watch_vte(terminal: &Terminal, child: Arc<ChildLifecycle>, guard: Arc<Guard>) {
    let watched_launch = vte_launch(terminal).as_ref().map(Rc::downgrade);
    // Only a successfully acquired, still-owned child enters these slots.
    unsafe {
        terminal.set_data("eva-owned-child", Arc::clone(&child));
        terminal.set_data("eva-process-guard", Arc::clone(&guard));
        if terminal
            .data::<bool>("eva-child-closed")
            .is_some_and(|flag| *flag.as_ref())
        {
            child.terminate(jterm_core::process::EscalationPolicy::SESSION_DRAIN);
        }
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let fallback_child = Arc::clone(&child);
    let fallback_guard = Arc::clone(&guard);
    let worker = std::thread::Builder::new()
        .name("vte-session-reaper".into())
        .spawn(move || loop {
            if let Some(code) = guard.poll_reap(&child) {
                let _ = tx.send(code);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        });
    if let Err(error) = &worker {
        log::error!("VTE reaper thread unavailable; using bounded main-context polling: {error}");
    }
    let terminal = terminal.clone();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let code = if worker.is_ok() {
            rx.try_recv().ok()
        } else {
            fallback_guard.poll_reap(&fallback_child)
        };
        if let Some(code) = code {
            let same_launch = match (&watched_launch, vte_launch(&terminal)) {
                (Some(watched), Some(current)) => watched
                    .upgrade()
                    .is_some_and(|watched| Rc::ptr_eq(&watched, &current)),
                (None, None) => true,
                _ => false,
            };
            if same_launch
                && vte_child(&terminal)
                    .is_some_and(|current| Arc::ptr_eq(&current, &fallback_child))
            {
                terminal.emit_by_name::<()>("child-exited", &[&((code & 255) << 8)]);
            } else {
                log::debug!(target: "eva_native_host", "Retired VTE launch skipped stale exit notification");
            }
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

pub(crate) fn vte_blocker(terminal: &Terminal, foreground: Option<String>) -> Option<String> {
    vte_activity(terminal, foreground, false)
}

/// Only after the application's normal confirmation: detect new attached
/// work during withdrawal. This never grants ordinary VTE close permission;
/// an uninstrumented prompt still requires the existing confirmation.
pub(crate) fn vte_withdrawal_work_probe(terminal: &Terminal) -> Box<dyn Fn() -> bool> {
    let probe = unsafe {
        terminal
            .data::<Arc<Guard>>("eva-process-guard")
            .map(|guard| guard.as_ref().withdrawal_work_probe())
    };
    let terminal = terminal.downgrade();
    Box::new(move || {
        terminal.upgrade().is_some_and(|terminal| {
            let exited = unsafe {
                terminal
                    .data::<Arc<ChildLifecycle>>("eva-owned-child")
                    .is_some_and(|child| child.as_ref().exit_code().is_some())
            };
            !exited && probe.as_ref().is_none_or(|probe| probe())
        })
    })
}

fn vte_activity(
    terminal: &Terminal,
    foreground: Option<String>,
    empty_prompt: bool,
) -> Option<String> {
    if let Some(launch) = vte_launch(terminal) {
        if !launch.settled() {
            return Some(
                if launch.closing() {
                    "Terminal is stopping"
                } else {
                    "Terminal is starting"
                }
                .into(),
            );
        }
        if launch.ready() {
            return None;
        }
    }
    unsafe {
        if terminal
            .data::<Arc<ChildLifecycle>>("eva-owned-child")
            .is_some_and(|child| child.as_ref().exit_code().is_some())
        {
            return None;
        }
        terminal
            .data::<Arc<Guard>>("eva-process-guard")
            .map_or_else(
                || Some("Shell activity could not be verified".into()),
                |guard| guard.as_ref().blocker(empty_prompt, foreground),
            )
    }
}

pub(crate) fn close_vte(terminal: &Terminal) {
    if let Some(launch) = vte_launch(terminal) {
        launch.close();
        return;
    }
    unsafe {
        terminal.set_data("eva-child-closed", true);
        if let Some(child) = terminal.data::<Arc<ChildLifecycle>>("eva-owned-child") {
            child
                .as_ref()
                .terminate(jterm_core::process::EscalationPolicy::SESSION_DRAIN);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, Write},
        os::unix::process::CommandExt,
        process::{Command, Stdio},
        time::Instant,
    };

    #[test]
    fn pending_close_probe_pins_confirmed_image_without_authorizing_an_idle_shell() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf 'ready\\n'; read answer; exec /bin/sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        // SAFETY: only setsid runs in the post-fork child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = Child(command.spawn().unwrap());
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let guard = Guard::capture(child.0.id() as i32, None);
        // No expected shell or semantic prompt: ordinary admission stays
        // conservative even though the confirmed baseline has no descendants.
        assert!(guard.blocker(true, None).is_some());
        let probe = guard.withdrawal_work_probe();
        assert!(!probe());
        child.0.stdin.take().unwrap().write_all(b"go\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !probe() {
            assert!(Instant::now() < deadline, "same-PID exec was not detected");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(Guard::capture(-1, None).withdrawal_work_probe()());
    }

    #[test]
    #[ignore = "requires a private GTK display and synthetic owned children"]
    #[allow(
        clippy::zombie_processes,
        reason = "ChildLifecycle is the exclusive wait owner"
    )]
    fn detached_children_join_one_window_drain_without_reentering_or_crossing_windows() {
        vte4::gtk::init().expect("private GTK display");
        let main = glib::MainContext::default();
        let _main = main.acquire().unwrap();
        struct Root {
            _process: std::process::Child,
            child: Arc<ChildLifecycle>,
        }
        impl Drop for Root {
            fn drop(&mut self) {
                self.child.force_kill_and_reap();
            }
        }
        fn live() -> Root {
            let mut command = Command::new("/usr/bin/python3");
            command.args(["-I", "-S", "-c", "import signal\nsignal.signal(signal.SIGHUP,signal.SIG_IGN)\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nprint('ready',flush=True)\nwhile True:signal.pause()"])
                .stdout(Stdio::piped());
            // SAFETY: setsid is the only post-fork, pre-exec operation.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            let mut process = command.spawn().unwrap();
            let child =
                ChildLifecycle::new(process.id() as i32, jterm_core::process::ReapOwner::Ours)
                    .unwrap();
            let mut ready = String::new();
            std::io::BufReader::new(process.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready, "ready\n");
            Root {
                _process: process,
                child,
            }
        }
        let first = vte4::gtk::Window::new();
        let other = vte4::gtk::Window::new();
        let a = live();
        let b = live();
        let unrelated = live();
        retain_children(&first, vec![a.child.clone().into()]);
        retain_children(&other, vec![unrelated.child.clone().into()]);
        let completed = Rc::new(std::cell::Cell::new(0));
        let result = completed.clone();
        let callback_window = first.clone();
        let expected_a = a.child.clone();
        let expected_b = b.child.clone();
        assert!(defer_children(&first, Vec::new(), move || {
            assert!(expected_a.exit_code().is_some() && expected_b.exit_code().is_some());
            // Callback runs after releasing the registry borrow and after all
            // registered work, including the later detached child, was reaped.
            assert!(!defer_children(&callback_window, Vec::new(), || panic!(
                "empty drain resumed"
            )));
            result.set(result.get() + 1);
        }));
        assert!(defer_children(&first, Vec::new(), || panic!(
            "second close callback ran"
        )));
        retain_children(&first, vec![b.child.clone().into()]);
        let start = Instant::now();
        while completed.get() == 0 {
            while main.iteration(false) {
                assert!(start.elapsed() < Duration::from_secs(5));
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "window drain did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(completed.get(), 1);
        assert!(
            unrelated.child.exit_code().is_none(),
            "other window's child was drained"
        );
        assert_eq!(
            PtySession::capture(unrelated.child.pid()).activity(),
            Activity::Idle
        );
        assert!(!defer_children(&first, Vec::new(), || panic!(
            "completed drain ran twice"
        )));
    }

    #[test]
    #[allow(
        clippy::zombie_processes,
        reason = "ChildLifecycle is the exclusive wait owner; std Child must not reap a second time"
    )]
    fn owned_natural_exit_drains_other_groups_before_legacy_reap() {
        let mut process = Command::new("/usr/bin/python3");
        process
            .args([
                "-I",
                "-S",
                "-c",
                r#"
import os,signal,sys,time
reader,writer=os.pipe()
pid=os.fork()
if pid==0:
 os.close(reader)
 os.setpgid(0,0)
 signal.signal(signal.SIGHUP,signal.SIG_IGN)
 signal.signal(signal.SIGTERM,signal.SIG_IGN)
 os.write(writer,b'1');os.close(writer)
 while True:time.sleep(1)
os.close(writer);os.read(reader,1);os.close(reader)
print(pid,flush=True)
sys.stdin.readline()
sys.exit(7)
"#,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        // SAFETY: only an async-signal-safe syscall runs after fork.
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut parent = process.spawn().unwrap();
        let child =
            ChildLifecycle::new(parent.id() as i32, jterm_core::process::ReapOwner::Ours).unwrap();
        struct Cleanup(Arc<ChildLifecycle>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.force_kill_and_reap();
            }
        }
        let _cleanup = Cleanup(Arc::clone(&child));
        let mut ready = String::new();
        std::io::BufReader::new(parent.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let job: i32 = ready.trim().parse().unwrap();
        let guard = Guard::capture(parent.id() as i32, Some(Path::new("/usr/bin/python3")));
        assert!(
            guard.blocker(true, None).is_some(),
            "separate-group job must block ordinary close"
        );
        assert!(
            guard.poll_reap(&child).is_none(),
            "a live leader must remain owned"
        );
        let mut unrelated = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        parent.stdin.take().unwrap().write_all(b"exit\n").unwrap();
        let start = Instant::now();
        let code = loop {
            if let Some(code) = guard.poll_reap(&child) {
                break code;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "session drain did not finish"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(code, 7);
        let stat = std::fs::read_to_string(format!("/proc/{job}/stat")).unwrap_or_default();
        assert!(
            stat.is_empty()
                || stat
                    .rsplit_once(')')
                    .unwrap()
                    .1
                    .trim_start()
                    .starts_with('Z')
        );
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "unrelated same-UID process must survive"
        );
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        assert_eq!(guard.poll_reap(&child), Some(7));
    }
}
