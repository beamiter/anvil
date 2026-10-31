//! Application-owned asynchronous launch with the public VTE PTY child setup.
//! Fork stays on GTK's thread. Exec observation and optional scope setup run
//! off that thread; every post-fork outcome retains the exclusive wait owner.
use crate::gtk_process::{self, PendingLaunch};
use jterm_core::process::{ChildLifecycle, EscalationPolicy, ReapOwner};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use vte4::{
    gtk::{gio, glib},
    Terminal, TerminalExt,
};

const EXEC_TIMEOUT: Duration = Duration::from_secs(20);
type Completion = Box<dyn FnOnce(io::Result<Arc<ChildLifecycle>>)>;

struct Prepared {
    executable: CString,
    argv: Vec<CString>,
    environment: Vec<CString>,
    cwd: Option<CString>,
    fallback_argv: Vec<CString>,
    descriptor_limit: u64,
}

fn invalid(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, detail)
}

impl Prepared {
    fn new(argv: &[&str], envv: &[&str], inherit: bool, cwd: Option<&str>) -> io::Result<Self> {
        let Some(program) = argv.first().filter(|program| !program.is_empty()) else {
            return Err(invalid("terminal executable is empty"));
        };
        let mut variables = BTreeMap::<OsString, OsString>::new();
        if inherit {
            variables.extend(std::env::vars_os());
        }
        for entry in envv {
            let Some((name, value)) = entry.split_once('=') else {
                return Err(invalid(
                    "terminal environment entry has no name/value separator",
                ));
            };
            if name.is_empty() {
                return Err(invalid("terminal environment name is empty"));
            }
            variables.insert(name.into(), value.into());
        }
        let path = variables.get(OsStr::new("PATH")).map(OsString::as_os_str);
        let executable = jterm_core::host::resolve_executable(program, path, cwd)?;
        let executable = CString::new(executable.as_os_str().as_bytes())
            .map_err(|_| invalid("terminal executable contains a NUL byte"))?;
        let argv = argv
            .iter()
            .map(|arg| {
                CString::new(*arg).map_err(|_| invalid("terminal argument contains a NUL byte"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let environment = variables
            .into_iter()
            .map(|(name, value)| {
                let mut entry = name.into_vec();
                entry.push(b'=');
                entry.extend(value.into_vec());
                CString::new(entry).map_err(|_| invalid("terminal environment contains a NUL byte"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let cwd = cwd
            .map(CString::new)
            .transpose()
            .map_err(|_| invalid("terminal working directory contains a NUL byte"))?;
        let mut fallback_argv = vec![CString::new("/bin/sh").unwrap(), executable.clone()];
        fallback_argv.extend(argv.iter().skip(1).cloned());
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if limit.rlim_cur == libc::RLIM_INFINITY {
            return Err(invalid("terminal descriptor limit cannot be determined"));
        }
        Ok(Self {
            executable,
            argv,
            environment,
            cwd,
            fallback_argv,
            descriptor_limit: limit.rlim_cur,
        })
    }
}

fn pointers(values: &[CString]) -> Vec<*const libc::c_char> {
    values
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    let copy = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// Only async-signal-safe libc operations follow VTE's public child setup.
/// Linux's CLOEXEC operation avoids walking the GUI's live descriptor table.
unsafe fn close_on_exec_from_three(limit: u64) -> bool {
    #[cfg(target_os = "linux")]
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } == 0
    {
        return true;
    }
    for fd in 3..limit {
        let flags = unsafe { libc::fcntl(fd as i32, libc::F_GETFD) };
        if flags < 0 {
            continue;
        }
        if unsafe { libc::fcntl(fd as i32, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return false;
        }
    }
    true
}

unsafe fn child_error(fd: i32, error: i32) -> ! {
    let bytes = error.to_ne_bytes();
    let frame = [b'E', bytes[0], bytes[1], bytes[2], bytes[3]];
    // A single tiny pipe frame is atomic. No Rust destructors run after fork.
    unsafe {
        libc::write(fd, frame.as_ptr().cast(), frame.len());
        libc::_exit(127)
    }
}

fn exec_result(fd: OwnedFd, child: &ChildLifecycle, closing: &AtomicBool) -> io::Result<()> {
    let mut file = std::fs::File::from(fd);
    let started = Instant::now();
    let mut bytes = Vec::with_capacity(6);
    let mut stopped = false;
    loop {
        if closing.load(Ordering::Acquire) && !stopped {
            let _ = child.signal(libc::SIGKILL);
            stopped = true;
        }
        if started.elapsed() >= EXEC_TIMEOUT {
            let _ = child.signal(libc::SIGKILL);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal launch timed out",
            ));
        }
        let mut poll = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, 50) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            continue;
        }
        let mut chunk = [0; 8];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.len() > 6 {
                    return Err(invalid("invalid terminal launch report"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    if closing.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "terminal launch was closed",
        ));
    }
    let error = if bytes.first() == Some(&b'S') {
        &bytes[1..]
    } else {
        &bytes[..]
    };
    if error.len() == 5 && error[0] == b'E' {
        return Err(io::Error::from_raw_os_error(i32::from_ne_bytes(
            error[1..].try_into().unwrap(),
        )));
    }
    if bytes == b"S" {
        return Ok(());
    }
    Err(io::Error::other("terminal child setup did not complete"))
}

/// Preserve the optional user-manager scope. The original child stays
/// unreaped throughout this request, so its numeric PID cannot be reused.
fn systemd_scope(pid: i32) -> Result<(), glib::Error> {
    use glib::variant::ToVariant;
    let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)?;
    let deadline = Instant::now() + EXEC_TIMEOUT;
    let remaining = || {
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(1, 20_000) as i32
    };
    let destination = Some("org.freedesktop.systemd1");
    let flags = gio::DBusCallFlags::NO_AUTO_START;
    let parent = bus.call_sync(
        destination,
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "GetUnitByPID",
        Some(&(std::process::id(),).to_variant()),
        None,
        flags,
        remaining(),
        None::<&gio::Cancellable>,
    )?;
    let path = parent.child_value(0).str().unwrap_or("").to_owned();
    let property = |interface: &str, name: &str| -> Result<glib::Variant, glib::Error> {
        let result = bus.call_sync(
            destination,
            &path,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(interface, name).to_variant()),
            None,
            flags,
            remaining(),
            None::<&gio::Cancellable>,
        )?;
        Ok(result.child_value(0).as_variant().unwrap())
    };
    let id = property("org.freedesktop.systemd1.Unit", "Id")?;
    let interface = if id.str().is_some_and(|id| id.ends_with(".scope")) {
        "org.freedesktop.systemd1.Scope"
    } else {
        "org.freedesktop.systemd1.Service"
    };
    let slice = property(interface, "Slice")
        .ok()
        .and_then(|value| value.str().map(str::to_owned))
        .unwrap_or_else(|| "app-org.gnome.vte.slice".into());
    let name = format!("vte-spawn-{}.scope", glib::uuid_string_random());
    let description = format!(
        "Terminal child {pid} launched by process {}",
        std::process::id()
    );
    let properties = vec![
        ("Description", description.to_variant()),
        ("PIDs", vec![pid as u32].to_variant()),
        ("Slice", slice.to_variant()),
    ];
    let auxiliaries: Vec<(String, Vec<(String, glib::Variant)>)> = Vec::new();
    bus.call_sync(
        destination,
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        Some(&(name, "fail", properties, auxiliaries).to_variant()),
        None,
        flags,
        remaining(),
        None::<&gio::Cancellable>,
    )?;
    Ok(())
}

fn fail_before_fork(launch: Rc<PendingLaunch>, callback: Completion, error: io::Error) {
    glib::idle_add_local_once(move || {
        launch.settle();
        callback(Err(error));
    });
}

/// All fork-created children use our exclusive reaper, including failed execs
/// and requests closed before their asynchronous result was delivered.
pub(crate) fn spawn(
    terminal: &Terminal,
    argv: &[&str],
    envv: &[&str],
    inherit: bool,
    cwd: Option<&str>,
    callback: impl FnOnce(io::Result<Arc<ChildLifecycle>>) + 'static,
) -> Rc<PendingLaunch> {
    if let Some(existing) = gtk_process::vte_launch(terminal).filter(|launch| !launch.ready()) {
        glib::idle_add_local_once(move || {
            callback(Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the terminal already owns a live launch or child",
            )))
        });
        return existing;
    }
    let launch = PendingLaunch::begin(terminal);
    let callback: Completion = Box::new(callback);
    if launch.closing() {
        fail_before_fork(
            launch.clone(),
            callback,
            io::Error::new(io::ErrorKind::Interrupted, "terminal is closed"),
        );
        return launch;
    }
    let prepared = match Prepared::new(argv, envv, inherit, cwd) {
        Ok(prepared) => prepared,
        Err(error) => {
            fail_before_fork(launch.clone(), callback, error);
            return launch;
        }
    };
    let pty = match terminal.pty_new_sync(vte4::PtyFlags::DEFAULT, None::<&gio::Cancellable>) {
        Ok(pty) => pty,
        Err(error) => {
            fail_before_fork(
                launch.clone(),
                callback,
                io::Error::other(error.to_string()),
            );
            return launch;
        }
    };
    let (read, write) = match gtk_process::exec_barrier()
        .and_then(|(read, write)| Ok((above_stdio(read)?, above_stdio(write)?)))
    {
        Ok(pipe) => pipe,
        Err(error) => {
            fail_before_fork(launch.clone(), callback, error);
            return launch;
        }
    };
    let argv_ptrs = pointers(&prepared.argv);
    let environment_ptrs = pointers(&prepared.environment);
    let fallback_ptrs = pointers(&prepared.fallback_argv);
    // VTE's supported child setup performs session/TTY/signal setup. Fork
    // stays on the main thread, as required by the GTK/VTE launch contract.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_before_fork(launch.clone(), callback, io::Error::last_os_error());
        return launch;
    }
    if pid == 0 {
        unsafe {
            libc::close(read.as_raw_fd());
            pty.child_setup();
            for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    child_error(
                        write.as_raw_fd(),
                        io::Error::last_os_error()
                            .raw_os_error()
                            .unwrap_or(libc::EIO),
                    );
                }
            }
            if !close_on_exec_from_three(prepared.descriptor_limit) {
                child_error(
                    write.as_raw_fd(),
                    io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EIO),
                );
            }
            if let Some(cwd) = &prepared.cwd {
                if libc::chdir(cwd.as_ptr()) != 0 {
                    child_error(
                        write.as_raw_fd(),
                        io::Error::last_os_error()
                            .raw_os_error()
                            .unwrap_or(libc::EIO),
                    );
                }
            }
            let ready = b'S';
            if libc::write(write.as_raw_fd(), (&ready as *const u8).cast(), 1) != 1 {
                libc::_exit(127);
            }
            libc::execve(
                prepared.executable.as_ptr(),
                argv_ptrs.as_ptr(),
                environment_ptrs.as_ptr(),
            );
            if io::Error::last_os_error().raw_os_error() == Some(libc::ENOEXEC) {
                libc::execve(
                    prepared.fallback_argv[0].as_ptr(),
                    fallback_ptrs.as_ptr(),
                    environment_ptrs.as_ptr(),
                );
            }
            child_error(
                write.as_raw_fd(),
                io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            );
        }
    }
    drop(write);
    let terminal = terminal.clone();
    let pty = pty.clone();
    let pending = launch.clone();
    let mut read = Some(read);
    let mut child: Option<Arc<ChildLifecycle>> = None;
    let mut receiver = None;
    let mut failure = None;
    let mut callback = Some(callback);
    // Acquiring a pidfd may temporarily fail (e.g. EMFILE). No other wait
    // owner exists: keep this direct child unreaped and retry, never reopen a
    // PID after handing it to a foreign watcher.
    glib::timeout_add_local(Duration::from_millis(20), move || {
        if child.is_none() {
            match ChildLifecycle::new(pid, ReapOwner::Ours) {
                Ok(owned) => {
                    pending.acquired(&owned);
                    child = Some(owned);
                }
                Err(error) => {
                    if failure.is_none() {
                        log::error!("Could not reference new terminal child {pid}: {error}; retaining launch ownership");
                        // Sole unreaped direct-child ownership pins this PID.
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                        failure = Some(error);
                    }
                    return glib::ControlFlow::Continue;
                }
            }
        }
        let owned = child.as_ref().unwrap();
        if failure.is_none() && receiver.is_none() {
            let (tx, rx) = mpsc::sync_channel(1);
            receiver = Some(rx);
            let fd = read.take().unwrap();
            let observed_child = Arc::clone(owned);
            let closing = pending.cancellation();
            let worker = std::thread::Builder::new().name("vte-launch-observer".into()).spawn(move || {
                let mut result = exec_result(fd, &observed_child, &closing);
                if result.is_ok() && !closing.load(Ordering::Acquire) {
                    if let Err(error) = systemd_scope(observed_child.pid()) {
                        log::debug!(target: "eva_native_host", "Optional terminal user scope unavailable: {error}");
                    }
                }
                if closing.load(Ordering::Acquire) {
                    if result.is_ok() { let _ = observed_child.signal(libc::SIGKILL); }
                    result = Err(io::Error::new(io::ErrorKind::Interrupted, "terminal launch was closed"));
                }
                let _ = tx.send(result);
            });
            if let Err(error) = worker {
                failure = Some(error);
            }
        }
        if failure.is_none() {
            match receiver.as_ref().unwrap().try_recv() {
                Ok(Ok(())) if !pending.closing() => {
                    terminal.set_pty(Some(&pty));
                    gtk_process::watch_vte(
                        &terminal,
                        Arc::clone(owned),
                        gtk_process::Guard::capture(pid, None),
                    );
                    pending.settle();
                    callback.take().unwrap()(Ok(Arc::clone(owned)));
                    return glib::ControlFlow::Break;
                }
                Ok(result) => {
                    failure = Some(result.err().unwrap_or_else(|| {
                        io::Error::new(io::ErrorKind::Interrupted, "terminal launch was closed")
                    }))
                }
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    failure = Some(io::Error::other("terminal launch observer stopped"))
                }
            }
        }
        drop(read.take());
        owned.terminate(EscalationPolicy::SESSION_DRAIN);
        if owned.exit_code().is_none() {
            return glib::ControlFlow::Continue;
        }
        // Failed/cancelled launches were never admitted. Their real wait owner
        // retires them quietly; no stale VTE exit signal can close a retry.
        pending.settle();
        callback.take().unwrap()(Err(failure.take().unwrap()));
        glib::ControlFlow::Break
    });
    launch
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        fs,
        path::PathBuf,
    };
    use vte4::gtk::prelude::*;

    fn until(main: &glib::MainContext, name: &str, ready: impl Fn() -> bool) {
        let started = Instant::now();
        while !ready() {
            while main.iteration(false) {
                assert!(
                    started.elapsed() < Duration::from_secs(8),
                    "{name}: dispatch limit"
                );
            }
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "{name}: timeout"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn kernel(pid: i32) -> Option<(char, u64)> {
        let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields: Vec<_> = raw[raw.rfind(')')? + 2..].split_whitespace().collect();
        Some((fields[0].chars().next()?, fields[19].parse().ok()?))
    }

    struct Fixture {
        root: PathBuf,
        terminal: Terminal,
        launch: Option<Rc<PendingLaunch>>,
        main: glib::MainContext,
    }
    impl Fixture {
        fn new(main: &glib::MainContext) -> Self {
            let root = std::env::temp_dir().join(format!(
                "eva-owned-launch-{}-{}",
                std::process::id(),
                glib::uuid_string_random()
            ));
            fs::create_dir(&root).unwrap();
            Self {
                root,
                terminal: Terminal::new(),
                launch: None,
                main: main.clone(),
            }
        }
        fn start(
            &mut self,
            program: &str,
            callback: impl FnOnce(io::Result<Arc<ChildLifecycle>>) + 'static,
        ) {
            let marker = self.root.join("ready");
            self.launch = Some(spawn(
                &self.terminal,
                &[
                    "/usr/bin/python3",
                    "-I",
                    "-S",
                    "-c",
                    program,
                    marker.to_str().unwrap(),
                ],
                &[
                    "PATH=/usr/bin:/bin",
                    "TERM=xterm-256color",
                    "VTE_VERSION=7802",
                ],
                false,
                Some("/tmp"),
                callback,
            ));
        }
        fn ready_without_dispatch(&self) -> Vec<i32> {
            let marker = self.root.join("ready");
            let started = Instant::now();
            loop {
                if let Ok(text) = fs::read_to_string(&marker) {
                    if text.ends_with('\n') {
                        return text.trim().split(',').map(|p| p.parse().unwrap()).collect();
                    }
                }
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "real child readiness"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            gtk_process::close_vte(&self.terminal);
            if let Some(launch) = &self.launch {
                let started = Instant::now();
                while !launch.ready() && started.elapsed() < Duration::from_secs(8) {
                    while self.main.iteration(false) {}
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const LIVE: &str = "import os,signal,sys\nsignal.signal(signal.SIGHUP,signal.SIG_IGN)\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nopen(sys.argv[1],'w').write(str(os.getpid())+'\\n')\nwhile True:signal.pause()";
    const WITH_JOB: &str = "import os,signal,sys,time\nsignal.signal(signal.SIGHUP,signal.SIG_IGN)\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nchild=os.fork()\nif child==0:\n os.setpgid(0,0)\n open(sys.argv[1]+'.job','w').write('ready')\n while True:signal.pause()\nwhile not os.path.exists(sys.argv[1]+'.job'):time.sleep(.001)\nopen(sys.argv[1],'w').write(str(os.getpid())+','+str(child)+'\\n')\nwhile True:signal.pause()";

    #[test]
    fn prepared_launch_keeps_frozen_environment_and_execvp_script_fallback() {
        let options = Prepared::new(
            &["sh", "-c", "printf ok"],
            &[
                "PATH=/usr/bin:/bin",
                "EVA_FROZEN=old",
                "EVA_FROZEN=chosen",
                "TERM=xterm-256color",
            ],
            false,
            Some("/tmp"),
        )
        .unwrap();
        assert!(options.executable.as_bytes().ends_with(b"/sh"));
        assert_eq!(options.argv[2].as_bytes(), b"printf ok");
        assert!(options
            .environment
            .iter()
            .any(|item| item.as_bytes() == b"EVA_FROZEN=chosen"));
        assert_eq!(options.environment.len(), 3);
        assert_eq!(options.fallback_argv[0].as_bytes(), b"/bin/sh");
        assert_eq!(options.fallback_argv[2].as_bytes(), b"-c");
        assert!(Prepared::new(&["sh"], &["PATH=/usr/bin", "BAD"], false, None).is_err());
        assert!(Prepared::new(&["sh", "bad\0argument"], &["PATH=/usr/bin"], false, None).is_err());
    }

    #[test]
    fn launch_report_pipe_survives_closed_standard_descriptors() {
        let (read, write) = gtk_process::exec_barrier().unwrap();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Isolated fork: never close the test runner's descriptors.
            unsafe {
                libc::close(read.as_raw_fd());
                for fd in 0..3 {
                    libc::close(fd);
                }
            }
            let result = gtk_process::exec_barrier()
                .and_then(|(read, write)| Ok((above_stdio(read)?, above_stdio(write)?)));
            if let Ok((low_read, low_write)) = result {
                if low_read.as_raw_fd() > 2 && low_write.as_raw_fd() > 2 {
                    unsafe {
                        libc::write(write.as_raw_fd(), b"ok".as_ptr().cast(), 2);
                        libc::_exit(0);
                    }
                }
            }
            unsafe {
                libc::_exit(127);
            }
        }
        drop(write);
        let mut report = String::new();
        fs::File::from(read).read_to_string(&mut report).unwrap();
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert_eq!(status, 0);
        assert_eq!(report, "ok");
    }

    #[test]
    #[ignore = "requires private GTK/DBus, Linux PTY and isolated real children; run alone with one test thread"]
    fn pending_launch_close_failure_retry_and_window_isolation() {
        vte4::gtk::init().expect("private GTK display");
        let main = glib::MainContext::default();
        let _main = main.acquire().unwrap();
        let first = vte4::gtk::Window::new();
        let other = vte4::gtk::Window::new();
        let mut survivor = Fixture::new(&main);
        let surviving_child = Rc::new(RefCell::new(None));
        let observed = surviving_child.clone();
        survivor.start(LIVE, move |result| {
            *observed.borrow_mut() = Some(result);
        });
        until(&main, "surviving real VTE", || {
            surviving_child.borrow().is_some()
        });
        assert!(surviving_child.borrow().as_ref().unwrap().is_ok());
        let survivor_pid = survivor.ready_without_dispatch()[0];
        let survivor_start = kernel(survivor_pid).unwrap().1;
        gtk_process::retain_children(
            &other,
            vec![gtk_process::vte_cleanup(&survivor.terminal).unwrap()],
        );

        // A real program and separate-group descendant exist before the main
        // context is allowed to deliver/acquire this launch's outcome.
        let mut pending = Fixture::new(&main);
        first.set_child(Some(&pending.terminal));
        let notifications = Rc::new(Cell::new(0));
        let notified = notifications.clone();
        let cancelled_result = Rc::new(Cell::new(false));
        let cancelled = cancelled_result.clone();
        pending.start(WITH_JOB, move |result| {
            cancelled.set(result.is_err());
            notified.set(notified.get() + 1);
        });
        let pids = pending.ready_without_dispatch();
        assert_eq!(pids.len(), 2);
        assert_eq!(unsafe { libc::getsid(pids[1]) }, pids[0]);
        assert_ne!(unsafe { libc::getpgid(pids[1]) }, pids[0]);
        assert!(!pending.launch.as_ref().unwrap().settled());
        gtk_process::retain_children(
            &first,
            vec![gtk_process::vte_cleanup(&pending.terminal).unwrap()],
        );
        first.set_child(None::<&Terminal>);
        let finishes = Rc::new(Cell::new(0));
        let finished = finishes.clone();
        let callback_window = first.clone();
        let reentrant_empty = Rc::new(Cell::new(false));
        let reentrant = reentrant_empty.clone();
        assert!(gtk_process::defer_children(&first, Vec::new(), move || {
            reentrant.set(!gtk_process::defer_children(
                &callback_window,
                Vec::new(),
                || {},
            ));
            finished.set(finished.get() + 1);
        }));
        assert!(!first.is_sensitive() && other.is_sensitive());
        let duplicate_callbacks = Rc::new(Cell::new(0));
        let duplicates = duplicate_callbacks.clone();
        assert!(gtk_process::defer_children(&first, Vec::new(), move || {
            duplicates.set(duplicates.get() + 1);
        }));
        assert_eq!(finishes.get(), 0);
        assert_eq!(notifications.get(), 0);
        until(&main, "closed pending launch and descendant drain", || {
            finishes.get() == 1
        });
        assert_eq!(notifications.get(), 1);
        assert!(cancelled_result.get() && reentrant_empty.get());
        assert_eq!(duplicate_callbacks.get(), 0);
        assert!(pending.launch.as_ref().unwrap().ready());
        assert!(kernel(pids[1]).is_none_or(|(state, _)| state == 'Z'));
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pids[0], &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(kernel(survivor_pid).unwrap().1, survivor_start);
        assert!(surviving_child
            .borrow()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .exit_code()
            .is_none());

        // The callback for an actual failed exec is delivered only after its
        // owned child is reaped; the same live terminal can then retry.
        let mut retry = Fixture::new(&main);
        let delivered = Rc::new(Cell::new(false));
        let done = delivered.clone();
        let failed_outcome = Rc::new(Cell::new(false));
        let failed = failed_outcome.clone();
        retry.launch = Some(spawn(
            &retry.terminal,
            &["/bin/sh"],
            &["PATH=/usr/bin:/bin"],
            false,
            Some(retry.root.join("missing-cwd").to_str().unwrap()),
            move |result| {
                failed.set(result.is_err());
                done.set(true);
            },
        ));
        until(&main, "real chdir failure cleanup", || delivered.get());
        assert!(failed_outcome.get() && retry.launch.as_ref().unwrap().ready());
        assert!(!gtk_process::vte_closed(&retry.terminal));
        let new_child = Rc::new(RefCell::new(None));
        let received = new_child.clone();
        retry.start(LIVE, move |result| {
            *received.borrow_mut() = Some(result);
        });
        until(&main, "retry after failed exec", || {
            new_child.borrow().is_some()
        });
        assert!(new_child.borrow().as_ref().unwrap().is_ok());
        let retry_pid = retry.ready_without_dispatch()[0];
        assert_ne!(retry_pid, pids[0]);
        assert!(retry.terminal.is_input_enabled());
        gtk_process::close_vte(&retry.terminal);
        until(&main, "retry child drained", || {
            retry.launch.as_ref().unwrap().ready()
        });

        // Delay handle acquisition using a real per-process descriptor limit.
        // Native child ownership stays pinned while EMFILE is recoverable.
        let mut pressured = Fixture::new(&main);
        let reported = Rc::new(Cell::new(false));
        let report = reported.clone();
        let pressure_outcome = Rc::new(Cell::new(false));
        let pressure_failed = pressure_outcome.clone();
        pressured.start(LIVE, move |result| {
            pressure_failed.set(result.is_err());
            report.set(true);
        });
        let pressure_pid = pressured.ready_without_dispatch()[0];
        struct Limit {
            original: libc::rlimit,
            fds: Vec<std::fs::File>,
        }
        impl Drop for Limit {
            fn drop(&mut self) {
                self.fds.clear();
                unsafe {
                    libc::setrlimit(libc::RLIMIT_NOFILE, &self.original);
                }
            }
        }
        let mut limit = Limit {
            original: unsafe { std::mem::zeroed() },
            fds: Vec::new(),
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit.original) },
            0
        );
        let count = fs::read_dir("/proc/self/fd").unwrap().count() as u64;
        let current = libc::rlimit {
            rlim_cur: count + 12,
            rlim_max: limit.original.rlim_max,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &current) }, 0);
        while let Ok(file) = std::fs::File::open("/dev/null") {
            limit.fds.push(file);
        }
        assert!(!limit.fds.is_empty());
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(120) {
            while main.iteration(false) {}
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!reported.get());
        assert!(!pressured.launch.as_ref().unwrap().ready());
        drop(limit);
        until(
            &main,
            "ownership acquisition recovered after EMFILE",
            || reported.get(),
        );
        assert!(pressure_outcome.get() && pressured.launch.as_ref().unwrap().ready());
        assert_eq!(
            unsafe { libc::waitpid(pressure_pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert_eq!(kernel(survivor_pid).unwrap().1, survivor_start);

        // Reuse a mapped terminal after a real child has exited, before the
        // old GTK exit notification is dispatched. The old startup batch must
        // never be written into the replacement shell's PTY.
        let replacement_window = vte4::gtk::Window::new();
        let mut replacement = Fixture::new(&main);
        replacement_window.set_child(Some(&replacement.terminal));
        replacement_window.present();
        let exit_notifications = Rc::new(Cell::new(0));
        let exits = exit_notifications.clone();
        replacement.terminal.connect_child_exited(move |_, _| {
            exits.set(exits.get() + 1);
        });
        let old_outcome = Rc::new(RefCell::new(None));
        let old_result = old_outcome.clone();
        replacement.launch = Some(spawn(
            &replacement.terminal,
            &["/bin/sh", "-c", "sleep .15"],
            &["PATH=/usr/bin:/bin", "TERM=xterm-256color"],
            false,
            Some("/tmp"),
            move |result| *old_result.borrow_mut() = Some(result),
        ));
        until(&main, "old real shell admitted", || {
            old_outcome.borrow().is_some()
        });
        let old_child = old_outcome.borrow_mut().take().unwrap().unwrap();
        let stale_marker = replacement.root.join("stale-command");
        let stale_commands = vec![format!("printf stale > '{}'", stale_marker.display())];
        gtk_process::schedule_initial_commands(
            &replacement.terminal,
            Arc::clone(&old_child),
            move || stale_commands,
        );
        let blocked = Instant::now();
        while old_child.exit_code().is_none() {
            assert!(
                blocked.elapsed() < Duration::from_secs(3),
                "old real shell reaped"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let exits_before_replacement = exit_notifications.get();
        let current_outcome = Rc::new(RefCell::new(None));
        let current_result = current_outcome.clone();
        replacement.launch = Some(spawn(
            &replacement.terminal,
            &["/bin/sh", "-i"],
            &["PATH=/usr/bin:/bin", "TERM=xterm-256color"],
            false,
            Some("/tmp"),
            move |result| *current_result.borrow_mut() = Some(result),
        ));
        until(&main, "replacement shell admitted", || {
            current_outcome.borrow().is_some()
        });
        let current_child = current_outcome.borrow_mut().take().unwrap().unwrap();
        assert!(!Arc::ptr_eq(&old_child, &current_child));
        let current_marker = replacement.root.join("current-command");
        let current_commands = vec![format!("printf current > '{}'", current_marker.display())];
        gtk_process::schedule_initial_commands(&replacement.terminal, current_child, move || {
            current_commands
        });
        until(&main, "current startup batch reached actual PTY", || {
            current_marker.exists()
        });
        assert_eq!(fs::read_to_string(&current_marker).unwrap(), "current");
        assert!(!stale_marker.exists());
        assert_eq!(exit_notifications.get(), exits_before_replacement);
        gtk_process::close_vte(&replacement.terminal);
        until(&main, "replacement shell drained", || {
            replacement.launch.as_ref().unwrap().ready()
        });
        replacement_window.close();

        // Exercise the real ENOEXEC fallback, frozen environment/cwd and
        // descriptor hygiene through the actual public VTE PTY setup.
        let mut stdio = Fixture::new(&main);
        let script = stdio.root.join("no-shebang");
        let metadata = stdio.root.join("metadata.json");
        let sentinel_path = stdio.root.join("private-parent-descriptor");
        let sentinel = fs::File::create(&sentinel_path).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(sentinel.as_raw_fd(), libc::F_SETFD, 0) },
            0
        );
        fs::write(&script, r#"exec /usr/bin/python3 -I -S -c 'import json,os,sys; links=[]
for fd in os.listdir("/proc/self/fd"):
 try: links.append(os.readlink("/proc/self/fd/"+fd))
 except FileNotFoundError: pass
open(sys.argv[2],"w").write(json.dumps(dict(argument=sys.argv[1],cwd=os.getcwd(),frozen=os.environ.get("EVA_FROZEN"),parent=os.environ.get("EVA_PARENT_ONLY"),tty=[os.isatty(n) for n in range(3)],links=links)))' "$@"
"#).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let stdio_outcome = Rc::new(RefCell::new(None));
        let stdio_result = stdio_outcome.clone();
        stdio.launch = Some(spawn(
            &stdio.terminal,
            &[
                script.to_str().unwrap(),
                "argument with spaces",
                metadata.to_str().unwrap(),
            ],
            &[
                "PATH=/usr/bin:/bin",
                "TERM=xterm-256color",
                "EVA_FROZEN=chosen",
            ],
            false,
            stdio.root.to_str(),
            move |result| *stdio_result.borrow_mut() = Some(result),
        ));
        until(&main, "no-shebang launch outcome", || {
            stdio_outcome.borrow().is_some()
        });
        assert!(stdio_outcome.borrow().as_ref().unwrap().is_ok());
        until(&main, "no-shebang child metadata and actual reap", || {
            metadata.exists() && stdio.launch.as_ref().unwrap().ready()
        });
        let metadata_text = fs::read_to_string(&metadata).unwrap();
        assert!(metadata_text.contains("\"argument\": \"argument with spaces\""));
        assert!(metadata_text.contains(&format!("\"cwd\": \"{}\"", stdio.root.display())));
        assert!(metadata_text.contains("\"frozen\": \"chosen\""));
        assert!(metadata_text.contains("\"parent\": null"));
        assert!(metadata_text.contains("\"tty\": [true, true, true]"));
        assert!(!metadata_text.contains(sentinel_path.to_str().unwrap()));

        gtk_process::close_vte(&survivor.terminal);
        until(&main, "other window explicitly closed", || {
            survivor.launch.as_ref().unwrap().ready()
        });
        first.close();
        other.close();
        println!("owned GTK launch: pending close + actual separate-group job drain + retry + real EMFILE recovery + per-window isolation + real PTY startup generation guard + ENOEXEC/frozen env/cwd/descriptor hygiene passed");
    }
}
