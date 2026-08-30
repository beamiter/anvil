//! Asynchronous, read-only Git diff surface for agent tasks.
//!
//! The UI thread only starts a worker and polls a channel. The worker invokes
//! Git directly with fixed arguments (never through a shell), disables pager,
//! color, external diff and text-conversion helpers, and drains stdout/stderr
//! concurrently under independent retention limits.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Maximum Git diff bytes retained for display.
pub const MAX_DIFF_STDOUT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum `git status --short` bytes retained for the file summary header.
pub const MAX_STATUS_STDOUT_BYTES: usize = 256 * 1024;
/// Maximum Git diagnostic bytes retained for an error message.
pub const MAX_DIFF_STDERR_BYTES: usize = 64 * 1024;

const READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_DIFF_CWD_DISPLAY_BYTES: usize = 4 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const TRUSTED_GIT_CANDIDATES: &[&str] = &["/usr/bin/git", "/bin/git"];

const GIT_ENVIRONMENT_OVERRIDES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_COUNT",
    // This older injection format can carry the same command-line config as
    // GIT_CONFIG_COUNT/KEY_n/VALUE_n and must not survive either.
    "GIT_CONFIG_PARAMETERS",
    "GIT_EXEC_PATH",
];

/// User-visible state of the most recent diff request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentDiffState {
    pub loading: bool,
    pub error: Option<String>,
    pub truncated: bool,
    pub text: String,
}

/// A request is deliberately single-flight so repeated UI actions cannot
/// create an unbounded number of Git processes or reader threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffRequestError {
    Busy,
    InvalidBase,
    WorkerSpawn(String),
}

impl fmt::Display for DiffRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("a Git diff request is already running"),
            Self::InvalidBase => formatter.write_str("Git diff base is not a full object ID"),
            Self::WorkerSpawn(error) => {
                write!(formatter, "could not start Git diff worker: {error}")
            }
        }
    }
}

impl std::error::Error for DiffRequestError {}

#[derive(Debug)]
struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
struct ProcessOutput {
    status: ExitStatus,
    stdout: CapturedBytes,
    stderr: CapturedBytes,
}

#[derive(Debug)]
struct DiffWorkerOutput {
    status_summary: ProcessOutput,
    tracked_diff: Option<ProcessOutput>,
}

type WorkerResult = Result<DiffWorkerOutput, String>;

/// Embeddable state for one task/worktree diff.
///
/// Call [`Self::request`] from a user action, then [`Self::poll`] from the UI
/// update loop; rendering lives in the iced tasks dashboard. Both request and
/// poll paths never block the UI thread.
#[derive(Default)]
pub struct AgentDiffPanel {
    pub is_open: bool,
    state: AgentDiffState,
    requested_cwd: Option<PathBuf>,
    requested_base: Option<String>,
    pending: Option<Receiver<WorkerResult>>,
}

impl AgentDiffPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &AgentDiffState {
        &self.state
    }

    pub fn requested_cwd(&self) -> Option<&Path> {
        self.requested_cwd.as_deref()
    }

    /// Start a read-only `HEAD` diff for `cwd` on a background thread.
    ///
    /// At most one request may be active. A successful request opens the
    /// built-in window; callers embedding its contents may set
    /// `is_open` back to false without affecting the worker.
    pub fn request(&mut self, cwd: impl Into<PathBuf>) -> Result<(), DiffRequestError> {
        self.request_inner(cwd.into(), "HEAD".to_string())
    }

    /// Start a read-only diff against the immutable commit captured when a
    /// task worktree was created. A full object ID avoids revision ambiguity
    /// and option injection.
    pub fn request_from(
        &mut self,
        cwd: impl Into<PathBuf>,
        base_commit: impl Into<String>,
    ) -> Result<(), DiffRequestError> {
        let base_commit = base_commit.into();
        if !valid_diff_base(&base_commit) || base_commit == "HEAD" {
            return Err(DiffRequestError::InvalidBase);
        }
        self.request_inner(cwd.into(), base_commit)
    }

    fn request_inner(&mut self, cwd: PathBuf, base: String) -> Result<(), DiffRequestError> {
        self.poll();
        if self.pending.is_some() {
            return Err(DiffRequestError::Busy);
        }

        self.state = AgentDiffState {
            loading: true,
            error: None,
            truncated: false,
            text: String::new(),
        };
        self.requested_cwd = Some(cwd.clone());
        self.requested_base = Some(base.clone());
        self.is_open = true;

        let (sender, receiver) = mpsc::channel();
        let spawn = std::thread::Builder::new()
            .name("anvil-agent-git-diff".to_string())
            .spawn(move || {
                let _ = sender.send(run_git_diff(&cwd, &base));
            });
        match spawn {
            Ok(_worker) => {
                self.pending = Some(receiver);
                Ok(())
            }
            Err(error) => {
                let error = DiffRequestError::WorkerSpawn(error.to_string());
                self.state.loading = false;
                self.state.error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Poll the worker once. Returns true when visible state changed.
    pub fn poll(&mut self) -> bool {
        let Some(receiver) = self.pending.as_ref() else {
            return false;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.pending = None;
                self.apply_result(result);
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                self.state.loading = false;
                self.state.error =
                    Some("Git diff worker ended without returning a result".to_string());
                true
            }
        }
    }

    /// Base revision the current or last request diffed against, for display.
    pub fn requested_base(&self) -> Option<&str> {
        self.requested_base.as_deref()
    }

    fn apply_result(&mut self, result: WorkerResult) {
        self.state.loading = false;
        match result {
            Ok(output) => {
                let DiffWorkerOutput {
                    status_summary,
                    tracked_diff,
                } = output;
                self.state.truncated = status_summary.stdout.truncated
                    || status_summary.stderr.truncated
                    || tracked_diff
                        .as_ref()
                        .is_some_and(|diff| diff.stdout.truncated || diff.stderr.truncated);

                let status_text =
                    bounded_lossy_text(status_summary.stdout.bytes, MAX_STATUS_STDOUT_BYTES);
                let status_body = if status_text.is_empty() {
                    "(working tree clean)"
                } else {
                    status_text.trim_end()
                };
                self.state.text =
                    format!("$ git status --short --untracked-files=all\n{status_body}");

                if !status_summary.status.success() {
                    self.state.error = Some(process_failure_message(
                        "git status --short",
                        status_summary.status,
                        status_summary.stderr.bytes,
                    ));
                    return;
                }

                let Some(diff) = tracked_diff else {
                    self.state.error = Some("tracked Git diff did not run".to_string());
                    return;
                };
                let diff_text = bounded_lossy_text(diff.stdout.bytes, MAX_DIFF_STDOUT_BYTES);
                let base = self.requested_base.as_deref().unwrap_or("HEAD");
                let diff_body = if diff_text.is_empty() {
                    format!("(no tracked changes relative to {base})")
                } else {
                    diff_text.trim_end().to_string()
                };
                self.state.text.push_str(&format!(
                    "\n\n$ git --no-pager diff --no-ext-diff --no-textconv --no-color {base} --\n"
                ));
                self.state.text.push_str(&diff_body);

                self.state.error = (!diff.status.success())
                    .then(|| process_failure_message("git diff", diff.status, diff.stderr.bytes));
            }
            Err(error) => {
                self.state.error = Some(error);
                self.state.truncated = false;
                self.state.text.clear();
            }
        }
    }
}

fn trusted_git_path() -> Result<PathBuf, String> {
    let mut failures = Vec::new();
    for candidate in TRUSTED_GIT_CANDIDATES {
        match validate_trusted_git_candidate(Path::new(candidate)) {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{candidate}: {error}")),
        }
    }
    Err(format!(
        "no trusted system Git executable is available ({})",
        failures.join("; ")
    ))
}

fn validate_trusted_git_candidate(candidate: &Path) -> Result<PathBuf, String> {
    if !candidate.is_absolute() {
        return Err("candidate is not absolute".to_string());
    }
    let resolved = std::fs::canonicalize(candidate)
        .map_err(|error| format!("cannot resolve executable: {error}"))?;
    if !resolved.is_absolute() {
        return Err("resolved executable is not absolute".to_string());
    }
    let metadata = std::fs::metadata(&resolved)
        .map_err(|error| format!("cannot inspect resolved executable: {error}"))?;
    if !metadata.is_file() {
        return Err("resolved executable is not a regular file".to_string());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if !is_trusted_system_owner(metadata.uid()) {
            return Err("resolved executable is not system-owned".to_string());
        }
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            return Err("resolved executable is not executable".to_string());
        }
        if mode & 0o022 != 0 {
            return Err("resolved executable is group- or world-writable".to_string());
        }

        // Execute the canonical target, not the candidate symlink. Validate
        // both the fixed candidate's directory chain and the canonical
        // target's chain: a symlink can otherwise jump between two unrelated
        // directory trees while only one of them is checked.
        let candidate_parent = candidate
            .parent()
            .ok_or_else(|| "candidate has no parent directory".to_string())?;
        let candidate_parent = std::fs::canonicalize(candidate_parent)
            .map_err(|error| format!("cannot resolve candidate directory: {error}"))?;
        validate_trusted_directory_chain(&candidate_parent)?;
        let resolved_parent = resolved
            .parent()
            .ok_or_else(|| "resolved executable has no parent directory".to_string())?;
        validate_trusted_directory_chain(resolved_parent)?;
    }

    Ok(resolved)
}

#[cfg(unix)]
fn is_trusted_system_owner(uid: u32) -> bool {
    // Rootless/managed OCI images commonly expose image-root ownership through
    // Linux's overflow uid (`nobody`, conventionally 65534). Accept that form
    // only when Anvil itself is not running as the same uid; otherwise an
    // owner-writable executable would still be user-controlled. The mode
    // checks above additionally reject group/world-writable files and
    // directories.
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    uid == 0 || (uid == 65_534 && uid != effective_uid)
}

#[cfg(unix)]
fn validate_trusted_directory_chain(directory: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    for ancestor in directory.ancestors() {
        let metadata = std::fs::metadata(ancestor).map_err(|error| {
            format!(
                "cannot inspect executable ancestor {}: {error}",
                ancestor.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "executable ancestor {} is not a directory",
                ancestor.display()
            ));
        }
        if !is_trusted_system_owner(metadata.uid()) {
            return Err(format!(
                "executable ancestor {} is not system-owned",
                ancestor.display()
            ));
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "executable ancestor {} is group- or world-writable",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn is_counted_git_config_key(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("GIT_CONFIG_KEY_") || key.starts_with("GIT_CONFIG_VALUE_")
}

fn scrub_git_environment(
    command: &mut Command,
    inherited_keys: impl IntoIterator<Item = OsString>,
) {
    for key in GIT_ENVIRONMENT_OVERRIDES {
        command.env_remove(key);
    }
    // Removing GIT_CONFIG_COUNT makes leftover numbered keys inert, but remove
    // them too so the child receives no attacker-controlled config material at
    // all. Enumerating keys happens before the worker spawns Git.
    for key in inherited_keys {
        if is_counted_git_config_key(&key) {
            command.env_remove(key);
        }
    }
}

fn configure_read_only_git(command: &mut Command, cwd: &Path) {
    scrub_git_environment(command, std::env::vars_os().map(|(key, _value)| key));
    command
        .current_dir(cwd)
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        // Avoid optional index refreshes for this strictly read-only view.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn git_status_command(git: &Path, cwd: &Path) -> Command {
    debug_assert!(git.is_absolute());
    let mut command = Command::new(git);
    command
        .arg("--no-pager")
        .arg("-c")
        .arg("color.ui=false")
        .arg("-c")
        // Do not invoke a repository-configured fsmonitor hook.
        .arg("core.fsmonitor=false")
        .arg("status")
        .arg("--short")
        // Override status.showUntrackedFiles so task-created files cannot
        // disappear from this first-version surface.
        .arg("--untracked-files=all");
    configure_read_only_git(&mut command, cwd);
    command
}

fn valid_diff_base(base: &str) -> bool {
    base == "HEAD"
        || (matches!(base.len(), 40 | 64) && base.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn git_diff_command(git: &Path, cwd: &Path, base: &str) -> Command {
    debug_assert!(valid_diff_base(base));
    debug_assert!(git.is_absolute());
    let mut command = Command::new(git);
    command
        .arg("--no-pager")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("diff")
        .arg("--no-ext-diff")
        // A repository-controlled textconv driver is another external helper.
        .arg("--no-textconv")
        .arg("--no-color")
        .arg(base)
        .arg("--");
    configure_read_only_git(&mut command, cwd);
    command
}

fn run_git_diff(cwd: &Path, base: &str) -> WorkerResult {
    let git = trusted_git_path()?;
    let status_summary = run_git_command(
        git_status_command(&git, cwd),
        "git status --short",
        MAX_STATUS_STDOUT_BYTES,
    )?;
    if !status_summary.status.success() {
        return Ok(DiffWorkerOutput {
            status_summary,
            tracked_diff: None,
        });
    }

    let tracked_diff = run_git_command(
        git_diff_command(&git, cwd, base),
        "git diff",
        MAX_DIFF_STDOUT_BYTES,
    )?;
    Ok(DiffWorkerOutput {
        status_summary,
        tracked_diff: Some(tracked_diff),
    })
}

fn run_git_command(
    command: Command,
    label: &str,
    stdout_limit: usize,
) -> Result<ProcessOutput, String> {
    run_command_with_timeout(
        command,
        label,
        stdout_limit,
        MAX_DIFF_STDERR_BYTES,
        GIT_COMMAND_TIMEOUT,
    )
}

fn run_command_with_timeout(
    mut command: Command,
    label: &str,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Duration,
) -> Result<ProcessOutput, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A timed-out Git process can have descendants (for example while
        // inspecting submodules). Give the command a private process group so
        // timeout cleanup also closes every inherited pipe writer.
        command.process_group(0);
        #[cfg(target_os = "linux")]
        // SAFETY: only async-signal-safe libc calls run between fork and exec.
        // This read-only helper must not survive an abrupt Anvil exit either.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {label}: {error}"))?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("git stdout pipe was unavailable".to_string());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let _ = child.kill();
            let _ = child.wait();
            return Err("git stderr pipe was unavailable".to_string());
        }
    };

    // Do not reap the leader until both readers see EOF. The unreaped child
    // anchors its private PGID, preventing a drain-timeout cleanup from ever
    // signalling a newly reused PID. Nonblocking readers share the command's
    // original deadline, so inherited pipe writers cannot hang scoped joins.
    let (stdout, stderr) = std::thread::scope(|scope| {
        let stdout_reader = scope.spawn(move || {
            read_bounded_until(stdout, stdout_limit, deadline)
                .map_err(|error| format!("could not read git stdout: {error}"))
        });
        let stderr_reader = scope.spawn(move || {
            read_bounded_until(stderr, stderr_limit, deadline)
                .map_err(|error| format!("could not read git stderr: {error}"))
        });
        let stdout = stdout_reader
            .join()
            .map_err(|_| "Git stdout reader panicked".to_string())
            .and_then(|result| result);
        let stderr = stderr_reader
            .join()
            .map_err(|_| "Git stderr reader panicked".to_string())
            .and_then(|result| result);
        (stdout, stderr)
    });

    let (stdout, stdout_closed) = match stdout {
        Ok(stdout) => stdout,
        Err(error) => {
            let _ = kill_process_group_and_wait(&mut child);
            return Err(error);
        }
    };
    let (stderr, stderr_closed) = match stderr {
        Ok(stderr) => stderr,
        Err(error) => {
            let _ = kill_process_group_and_wait(&mut child);
            return Err(error);
        }
    };
    if !stdout_closed || !stderr_closed {
        let cleanup_error = kill_process_group_and_wait(&mut child).err();
        let elapsed_ms = timeout.as_millis();
        return Err(match cleanup_error {
            Some(error) => format!(
                "{label} timed out after {elapsed_ms} ms while draining output; cleanup also failed: {error}"
            ),
            None => {
                format!("{label} timed out after {elapsed_ms} ms while draining output")
            }
        });
    }
    let status = wait_for_child_deadline(&mut child, label, timeout, deadline)?;

    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn wait_for_child_deadline(
    child: &mut Child,
    label: &str,
    timeout: Duration,
    deadline: Instant,
) -> Result<ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // ECHILD (or another ambiguous wait error) can mean the
                // leader was reaped elsewhere. Never signal its cached PID as
                // a PGID after that point because the numeric ID may be reused.
                return Err(format!("could not wait for {label}: {error}"));
            }
        }

        let now = Instant::now();
        if now >= deadline {
            let cleanup_error = kill_process_group_and_wait(child).err();
            let elapsed_ms = timeout.as_millis();
            return Err(match cleanup_error {
                Some(error) => {
                    format!("{label} timed out after {elapsed_ms} ms; cleanup also failed: {error}")
                }
                None => format!("{label} timed out after {elapsed_ms} ms"),
            });
        }
        std::thread::sleep(CHILD_WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn kill_process_group_and_wait(child: &mut Child) -> Result<(), String> {
    let mut kill_error = kill_process_group_id(child.id()).err();

    #[cfg(unix)]
    if kill_error.is_some() {
        // A private-group kill should not fail for our own child, but still
        // kill the direct process as a last-resort convergence path before
        // waiting.
        if let Err(direct_error) = child.kill() {
            if direct_error.kind() != io::ErrorKind::InvalidInput {
                kill_error = Some(format!(
                    "{}; direct kill also failed: {direct_error}",
                    kill_error.take().unwrap_or_default()
                ));
            }
        }
    }

    #[cfg(not(unix))]
    if let Err(error) = child.kill() {
        if error.kind() != io::ErrorKind::InvalidInput {
            kill_error = Some(format!("could not kill process: {error}"));
        }
    }

    // Always reap, including ESRCH/already-exited races. Waiting after the
    // group kill also guarantees every group member has closed its inherited
    // pipe descriptors before scoped reader threads are joined.
    let wait_error = child
        .wait()
        .err()
        .map(|error| format!("could not reap process: {error}"));
    match (kill_error, wait_error) {
        (None, None) => Ok(()),
        (Some(error), None) | (None, Some(error)) => Err(error),
        (Some(kill), Some(wait)) => Err(format!("{kill}; {wait}")),
    }
}

#[cfg(unix)]
fn kill_process_group_id(child_id: u32) -> Result<(), String> {
    let process_group = i32::try_from(child_id)
        .map_err(|_| "Git process id does not fit a Unix process group id".to_string())?;
    // SAFETY: `process_group` is the positive pid returned by Child and the
    // command was spawned with `process_group(0)`, so its negation targets
    // exactly that private group. SIGKILL has no user-controlled payload.
    if unsafe { libc::kill(-process_group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("could not kill process group: {error}"))
    }
}

#[cfg(not(unix))]
fn kill_process_group_id(_child_id: u32) -> Result<(), String> {
    // Non-Unix callers kill the direct Child in kill_process_group_and_wait.
    Ok(())
}

fn process_failure_message(label: &str, status: ExitStatus, stderr_bytes: Vec<u8>) -> String {
    let stderr = bounded_lossy_text(stderr_bytes, MAX_DIFF_STDERR_BYTES);
    let status = status
        .code()
        .map_or_else(|| "signal".to_string(), |code| format!("exit {code}"));
    if stderr.trim().is_empty() {
        format!("{label} failed ({status})")
    } else {
        format!("{label} failed ({status}): {}", stderr.trim_end())
    }
}

#[cfg(any(test, not(unix)))]
fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<CapturedBytes> {
    let mut retained = Vec::with_capacity(limit.min(READ_CHUNK_BYTES));
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let available = limit.saturating_sub(retained.len());
        let keep = read.min(available);
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
    Ok(CapturedBytes {
        bytes: retained,
        truncated,
    })
}

#[cfg(unix)]
fn read_bounded_until(
    mut reader: impl Read + std::os::fd::AsRawFd,
    limit: usize,
    deadline: Instant,
) -> io::Result<(CapturedBytes, bool)> {
    let descriptor = reader.as_raw_fd();
    // SAFETY: fcntl only reads/updates flags on this live, owned pipe fd.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same descriptor; preserve all existing status flags.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut retained = Vec::with_capacity(limit.min(READ_CHUNK_BYTES));
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut truncated = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok((
                CapturedBytes {
                    bytes: retained,
                    truncated,
                },
                false,
            ));
        }
        match reader.read(&mut chunk) {
            Ok(0) => {
                return Ok((
                    CapturedBytes {
                        bytes: retained,
                        truncated,
                    },
                    true,
                ));
            }
            Ok(read) => {
                let keep = read.min(limit.saturating_sub(retained.len()));
                retained.extend_from_slice(&chunk[..keep]);
                truncated |= keep < read;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(
                    CHILD_WAIT_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
                );
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(unix))]
fn read_bounded_until(
    reader: impl Read,
    limit: usize,
    _deadline: Instant,
) -> io::Result<(CapturedBytes, bool)> {
    read_bounded(reader, limit).map(|captured| (captured, true))
}

/// Lossy decoding can expand one invalid input byte into a three-byte Unicode
/// replacement character. Apply the same byte ceiling after decoding so the
/// UI state itself remains under the advertised retention limit.
fn bounded_lossy_text(bytes: Vec<u8>, limit: usize) -> String {
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > limit {
        let mut end = limit;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    // Repository paths and file contents are attacker-influenced review data.
    // Preserve multiline/tab layout, but neutralize terminal controls and
    // invisible/bidirectional formatting before the UI displays the diff. `?`
    // is one byte, so this pass cannot exceed the retention ceiling above.
    text.chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            unsafe_character
                if unsafe_character.is_control()
                    || jterm_core::review_input::is_visual_spoofing_character(unsafe_character) =>
            {
                '?'
            }
            visible => visible,
        })
        .collect()
}

pub(crate) fn visible_diff_cwd(cwd: &Path) -> String {
    crate::review_text::visible_bounded(&cwd.to_string_lossy(), MAX_DIFF_CWD_DISPLAY_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_reader_retains_the_limit_and_drains_the_rest() {
        let input = vec![b'x'; READ_CHUNK_BYTES * 2 + 17];
        let captured = read_bounded(Cursor::new(&input), 100).expect("bounded read");

        assert_eq!(captured.bytes, vec![b'x'; 100]);
        assert!(captured.truncated);
    }

    #[test]
    fn exact_limit_is_not_reported_as_truncated() {
        let captured = read_bounded(Cursor::new(b"1234"), 4).expect("bounded read");

        assert_eq!(captured.bytes, b"1234");
        assert!(!captured.truncated);
    }

    #[test]
    fn zero_retention_still_reports_and_drains_output() {
        let captured = read_bounded(Cursor::new(b"discarded"), 0).expect("bounded read");

        assert!(captured.bytes.is_empty());
        assert!(captured.truncated);
    }

    #[test]
    fn lossy_decoding_cannot_expand_past_the_limit() {
        let text = bounded_lossy_text(vec![0xff; 32], 17);

        assert!(text.len() <= 17);
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn diff_display_neutralizes_controls_and_bidi_without_flattening_lines() {
        let text = bounded_lossy_text(b"safe\n\x1b[31mred\tend\xe2\x80\xae".to_vec(), 128);

        assert_eq!(text, "safe\n?[31mred\tend?");
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
    }

    #[test]
    fn diff_header_cannot_be_forged_by_a_hostile_workspace_path() {
        let visible = visible_diff_cwd(Path::new("/tmp/repo\nforged\u{202e}txt"));

        assert_eq!(visible, "/tmp/repo\\nforged\\u{202E}txt");
        assert!(!visible.contains('\n'));
        assert!(!visible.contains('\u{202e}'));
    }

    #[test]
    fn git_command_has_fixed_read_only_diff_arguments_and_no_shell() {
        let command = git_diff_command(Path::new("/usr/bin/git"), Path::new("/tmp/repo"), "HEAD");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let env = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(command.get_program(), "/usr/bin/git");
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp/repo")));
        assert_eq!(
            args,
            [
                "--no-pager",
                "-c",
                "core.fsmonitor=false",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "HEAD",
                "--"
            ]
        );
        assert_eq!(env.get("GIT_PAGER").map(String::as_str), Some("cat"));
        assert_eq!(env.get("PAGER").map(String::as_str), Some("cat"));
        assert_eq!(env.get("GIT_OPTIONAL_LOCKS").map(String::as_str), Some("0"));
    }

    #[test]
    fn status_header_command_lists_untracked_files_without_color_or_optional_locks() {
        let command = git_status_command(Path::new("/usr/bin/git"), Path::new("/tmp/repo"));
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let env = command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(
            args,
            [
                "--no-pager",
                "-c",
                "color.ui=false",
                "-c",
                "core.fsmonitor=false",
                "status",
                "--short",
                "--untracked-files=all"
            ]
        );
        assert_eq!(env.get("NO_COLOR").map(String::as_str), Some("1"));
        assert_eq!(env.get("GIT_OPTIONAL_LOCKS").map(String::as_str), Some("0"));
    }

    #[test]
    fn repository_and_counted_config_environment_is_explicitly_removed() {
        let mut command = Command::new("/usr/bin/git");
        scrub_git_environment(
            &mut command,
            [
                OsString::from("GIT_CONFIG_KEY_0"),
                OsString::from("GIT_CONFIG_VALUE_0"),
                OsString::from("UNRELATED"),
            ],
        );
        let removed = command
            .get_envs()
            .filter(|(_key, value)| value.is_none())
            .map(|(key, _value)| key.to_string_lossy().into_owned())
            .collect::<std::collections::HashSet<_>>();

        for required in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
        ] {
            assert!(
                removed.contains(required),
                "missing env_remove for {required}"
            );
        }
        assert!(!removed.contains("UNRELATED"));
    }

    #[test]
    fn counted_git_config_key_recognition_is_narrow() {
        assert!(is_counted_git_config_key(OsStr::new("GIT_CONFIG_KEY_12")));
        assert!(is_counted_git_config_key(OsStr::new("GIT_CONFIG_VALUE_12")));
        assert!(!is_counted_git_config_key(OsStr::new("GIT_CONFIG_COUNT")));
        assert!(!is_counted_git_config_key(OsStr::new(
            "NOT_GIT_CONFIG_KEY_0"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn hard_deadline_kills_reaps_and_converges_pipe_readers() {
        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .expect("system sleep executable");
        let mut command = Command::new(sleep);
        command
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = Instant::now();
        let error =
            run_command_with_timeout(command, "deadline test", 32, 32, Duration::from_millis(25))
                .expect_err("sleep must be killed at its deadline");

        assert!(error.contains("timed out after 25 ms"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout cleanup did not converge"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deadline_converges_when_exited_child_leaves_a_pipe_holding_descendant() {
        const HELPER_ENV: &str = "ANVIL_DIFF_PIPE_HOLDER_HELPER";

        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg("agent_task::diff::tests::pipe_holder_helper_process")
            .arg("--nocapture")
            .env(HELPER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = Instant::now();
        let error = run_command_with_timeout(
            command,
            "pipe holder test",
            4 * 1024,
            4 * 1024,
            Duration::from_millis(100),
        )
        .expect_err("a descendant holding inherited pipes must hit the shared deadline");

        assert!(
            error.contains("timed out after 100 ms while draining output"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "pipe-reader cleanup did not converge"
        );
    }

    #[cfg(unix)]
    #[test]
    // This helper must deliberately exit without waiting: the parent test is
    // verifying that the outer process-group cleanup closes an orphaned pipe
    // holder while the finished direct child remains deliberately unreaped as
    // a stable PGID anchor.
    #[allow(clippy::zombie_processes)]
    fn pipe_holder_helper_process() {
        if std::env::var_os("ANVIL_DIFF_PIPE_HOLDER_HELPER").is_none() {
            return;
        }

        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .expect("system sleep executable");
        // Keep the helper's stdout/stderr descriptors inherited. The direct
        // helper exits immediately, leaving this descendant as the only pipe
        // holder in the private process group created by the code under test.
        Command::new(sleep)
            .arg("5")
            .spawn()
            .expect("spawn pipe-holding descendant");
    }

    #[cfg(unix)]
    #[test]
    fn trusted_git_resolves_to_a_system_owned_absolute_binary() {
        let git = trusted_git_path().expect("trusted system Git");

        assert!(git.is_absolute());
        assert!(git.is_file());
        assert_ne!(git, Path::new("git"));
    }

    #[test]
    fn busy_request_does_not_replace_the_in_flight_receiver() {
        let (_sender, receiver) = mpsc::channel();
        let mut panel = AgentDiffPanel {
            pending: Some(receiver),
            state: AgentDiffState {
                loading: true,
                ..AgentDiffState::default()
            },
            ..AgentDiffPanel::default()
        };

        assert_eq!(
            panel.request("/tmp/another-repo"),
            Err(DiffRequestError::Busy)
        );
        assert!(panel.state.loading);
        assert!(panel.pending.is_some());
    }

    #[test]
    fn task_diff_base_requires_a_full_object_id() {
        let mut panel = AgentDiffPanel::new();
        for invalid in ["", "HEAD", "--output=/tmp/owned", "deadbeef"] {
            assert_eq!(
                panel.request_from("/tmp/repo", invalid),
                Err(DiffRequestError::InvalidBase)
            );
        }
        assert!(valid_diff_base("0123456789abcdef0123456789abcdef01234567"));
    }

    #[cfg(unix)]
    #[test]
    fn completed_state_starts_with_status_including_untracked_then_shows_diff() {
        use std::os::unix::process::ExitStatusExt;

        let success = || ExitStatus::from_raw(0);
        let mut panel = AgentDiffPanel::default();
        panel.state.loading = true;
        panel.apply_result(Ok(DiffWorkerOutput {
            status_summary: ProcessOutput {
                status: success(),
                stdout: CapturedBytes {
                    bytes: b" M src/lib.rs\n?? src/new.rs\n".to_vec(),
                    truncated: false,
                },
                stderr: CapturedBytes {
                    bytes: Vec::new(),
                    truncated: false,
                },
            },
            tracked_diff: Some(ProcessOutput {
                status: success(),
                stdout: CapturedBytes {
                    bytes: b"diff --git a/src/lib.rs b/src/lib.rs\n".to_vec(),
                    truncated: false,
                },
                stderr: CapturedBytes {
                    bytes: Vec::new(),
                    truncated: false,
                },
            }),
        }));

        assert!(!panel.state.loading);
        assert!(panel.state.error.is_none());
        assert!(panel.state.text.starts_with(
            "$ git status --short --untracked-files=all\n M src/lib.rs\n?? src/new.rs"
        ));
        assert!(panel.state.text.contains("\n\n$ git --no-pager diff"));
        assert!(panel.state.text.contains("diff --git a/src/lib.rs"));
    }

    #[test]
    fn immutable_task_base_keeps_committed_agent_changes_visible() {
        struct TempRepo(PathBuf);
        impl Drop for TempRepo {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let repository = TempRepo(
            std::env::temp_dir().join(format!("anvil-task-diff-base-{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir(&repository.0).expect("create repository");
        let git = trusted_git_path().expect("trusted Git");
        let checked = |args: &[&str]| {
            let status = Command::new(&git)
                .current_dir(&repository.0)
                .args(args)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LC_ALL", "C")
                .status()
                .expect("run fixture Git");
            assert!(status.success(), "Git fixture command failed: {args:?}");
        };
        checked(&["init", "--quiet"]);
        std::fs::write(repository.0.join("tracked.txt"), b"before\n").expect("write baseline");
        checked(&["add", "--", "tracked.txt"]);
        checked(&[
            "-c",
            "user.name=Anvil Tests",
            "-c",
            "user.email=anvil@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ]);
        let base = Command::new(&git)
            .current_dir(&repository.0)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("read baseline");
        assert!(base.status.success());
        let base = String::from_utf8(base.stdout)
            .expect("object id is UTF-8")
            .trim()
            .to_string();

        std::fs::write(repository.0.join("tracked.txt"), b"after\n").expect("write Agent change");
        checked(&["add", "--", "tracked.txt"]);
        checked(&[
            "-c",
            "user.name=Anvil Tests",
            "-c",
            "user.email=anvil@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "agent change",
        ]);

        let output = run_git_diff(&repository.0, &base).expect("task diff runs");
        assert!(output.status_summary.status.success());
        assert!(output.status_summary.stdout.bytes.is_empty());
        let diff = output.tracked_diff.expect("tracked diff result");
        assert!(diff.status.success());
        let text = String::from_utf8(diff.stdout.bytes).expect("diff is UTF-8");
        assert!(text.contains("-before"), "{text}");
        assert!(text.contains("+after"), "{text}");
    }
}
