//! Native Codex adapter backed by the experimental app-server JSONL protocol.
//!
//! The UI-facing methods on [`CodexAppServerDriver`] never perform provider I/O.
//! A bounded command channel feeds a single worker which owns the stdio pipes,
//! the descriptor-pinned workspace capability, and the child process through
//! final process-group termination and reap.

use crate::agent_task::driver::{
    agent_event_channel, AgentCancellation, AgentCommand, AgentDriver, AgentDriverError,
    AgentEventReceiveError, AgentEventReceiver, AgentEventSendError, AgentEventSender,
    AgentEventSink, AgentPrompt, AgentStartRequest, ApprovalDecision,
};
use crate::agent_task::native::{
    NativeCodexCredentials, PreparedNativeCodexHome, PreparedNativeWorkspace,
};
use crate::agent_task::{
    AgentEvent, AgentEventKind, AgentLaunchSpec, AgentProvider, AgentSessionOutcome, AgentTurnId,
    ApprovalId, ProviderSessionId,
};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Maximum UI-to-worker commands retained by one driver.
pub const CODEX_APP_SERVER_COMMAND_CAPACITY: usize = 32;
/// Maximum complete JSONL record accepted from or queued to app-server.
pub const CODEX_APP_SERVER_JSONL_MAX_BYTES: usize = 1024 * 1024;
/// Maximum queued-but-not-yet-written protocol bytes.
pub const CODEX_APP_SERVER_WRITE_QUEUE_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Maximum queued-but-not-yet-written protocol records.
pub const CODEX_APP_SERVER_WRITE_QUEUE_MAX_MESSAGES: usize = 64;
/// Maximum rendered assistant text retained in a view snapshot.
pub const CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES: usize = 256 * 1024;
/// Maximum command records retained in a view snapshot.
pub const CODEX_APP_SERVER_COMMAND_VIEW_CAPACITY: usize = 16;
/// Maximum file-change records retained in a view snapshot.
pub const CODEX_APP_SERVER_FILE_VIEW_CAPACITY: usize = 16;
/// Maximum simultaneous approval requests retained and correlated.
pub const CODEX_APP_SERVER_APPROVAL_CAPACITY: usize = 8;
/// Aggregate exact string bytes retained by all pending approval snapshots.
pub const CODEX_APP_SERVER_APPROVAL_FROZEN_MAX_BYTES: usize = 256 * 1024;
/// Maximum sequential turns one live native session may own. Keeping every
/// completed provider turn identity for the full session prevents an old ID
/// from regaining authority after bounded tombstone eviction.
pub const CODEX_APP_SERVER_LIVE_TURN_MAX: usize = 32;
/// Maximum completed turn projections retained before the current/latest turn.
pub const CODEX_APP_SERVER_TURN_HISTORY_CAPACITY: usize = 8;
/// Hard accounted byte budget for all retained completed turn projections.
pub const CODEX_APP_SERVER_TURN_HISTORY_MAX_BYTES: usize = 1024 * 1024;
const RESOLVED_APPROVAL_TOMBSTONE_CAPACITY: usize = 32;

const STDERR_TAIL_MAX_BYTES: usize = 64 * 1024;
const COMMAND_OUTPUT_MAX_BYTES: usize = 32 * 1024;
const FILE_DIFF_MAX_BYTES: usize = 8 * 1024;
const FILE_CHANGES_PER_ITEM: usize = 16;
const FIELD_MAX_BYTES: usize = 4096;
const TOOL_PATH_MAX_BYTES: usize = 16 * 1024;
const TOOL_PATH_MAX_DIRECTORIES: usize = 64;
const IO_POLL_INTERVAL: Duration = Duration::from_millis(5);
const IDLE_IO_POLL_INTERVAL: Duration = Duration::from_millis(75);
const CANCEL_INTERRUPT_GRACE: Duration = Duration::from_secs(2);
const STARTUP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINATE_GRACE: Duration = Duration::from_millis(500);
const READ_BYTES_PER_TICK: usize = 256 * 1024;
const READ_MESSAGES_PER_TICK: usize = 64;
const CONTAINMENT_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const SYSTEM_SHELL: &str = "/bin/sh";
const TRUSTED_KILL: &str = "/bin/kill";
const TRUSTED_ENV: &str = "/usr/bin/env";
const TRUSTED_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
// The app-server and external-token login used by this first slice are still
// experimental. Bind the attested schema/default-feature set to the exact CLI
// release audited by Anvil; a newer install falls back to the terminal path
// instead of silently gaining new provider authority.
const SUPPORTED_CODEX_USER_AGENT_PREFIX: &str = "anvil/0.147.0 ";

const SYSTEMD_WRAPPER_ENV_ALLOWLIST: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LOGNAME",
    "USER",
    "XDG_RUNTIME_DIR",
];
const PROVIDER_ENV_ALLOWLIST: &[&str] = &[
    "ALL_PROXY",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LOGNAME",
    "NO_PROXY",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "TZ",
    "USER",
    "all_proxy",
    "https_proxy",
    "http_proxy",
    "no_proxy",
];
const NATIVE_CODEX_DISABLED_FEATURES: &[&str] = &[
    "apps",
    "auth_elicitation",
    "browser_use",
    "browser_use_external",
    "browser_use_full_cdp_access",
    "code_mode",
    "code_mode_host",
    "code_mode_only",
    "computer_use",
    "enable_mcp_apps",
    "external_agent_memory_import",
    "goals",
    "guardian_approval",
    "hooks",
    "image_generation",
    "in_app_browser",
    "in_app_updates",
    "memories",
    "mentions_v2",
    "multi_agent",
    "plugin_sharing",
    "plugins",
    "recommended_plugins",
    "remote_control",
    "remote_plugin",
    "shell_snapshot",
    "skill_mcp_dependency_install",
    "skill_search",
    "tool_call_mcp_elicitation",
    "tool_suggest",
    "view_image",
    "workspace_dependencies",
];

/// Coarse transport/lifecycle state intended for frame-level UI rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexAppServerPhase {
    Created,
    Spawning,
    Initializing,
    StartingThread,
    StartingTurn,
    Ready,
    Running,
    WaitingForApproval,
    Cancelling,
    Stopping,
    Ended,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexAppServerApprovalKind {
    Command,
    FileChange,
}

/// Bounded, display-safe approval information. The JSON-RPC request ID stays
/// private in the worker and is addressed only through Anvil's `ApprovalId`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerApproval {
    pub id: ApprovalId,
    pub kind: CodexAppServerApprovalKind,
    pub item_id: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    /// Complete file paths affected by this exact file-change item. Empty for
    /// command approvals. Approval registration fails if this projection is
    /// incomplete or truncated.
    pub file_paths: Vec<String>,
    /// Immutable, exact file-change evidence captured when the request was
    /// registered. Later item notifications cannot mutate this authority.
    pub file_changes: Vec<CodexAppServerApprovalFileChange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerApprovalFileChange {
    pub path: String,
    pub kind: String,
    pub diff: String,
    pub move_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerCommandView {
    pub item_id: String,
    pub command: String,
    pub cwd: String,
    pub status: String,
    pub output: String,
    pub output_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerFileChange {
    pub path: String,
    pub path_exact: bool,
    pub kind: String,
    pub kind_exact: bool,
    pub diff: String,
    pub diff_truncated: bool,
    pub diff_exact: bool,
    pub move_path: Option<String>,
    pub move_path_exact: bool,
    shape_exact: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerFileChangeView {
    pub item_id: String,
    pub status: String,
    pub changes: Vec<CodexAppServerFileChange>,
    pub changes_truncated: bool,
}

/// Compact, non-authoritative command evidence from one completed turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerTurnCommandSummary {
    pub command: String,
    pub status: String,
    /// True when command output existed (including an already-truncated flat
    /// projection) but was intentionally omitted from compact history.
    pub output_omitted: bool,
}

/// Compact, non-authoritative file evidence from one completed turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerTurnFileSummary {
    pub status: String,
    /// First display-safe affected path, when the item reported one.
    pub path: Option<String>,
    pub change_count: usize,
    /// True when additional changes were omitted from this compact summary or
    /// the current-turn projection had already truncated its change list.
    pub changes_truncated: bool,
    /// True when the retained path is only a display-safe projection.
    pub path_truncated: bool,
}

/// Bounded presentation history. These local identities and summaries never
/// participate in provider correlation or approval authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerTurnHistory {
    /// Stable, one-based turn position within this live native session.
    pub ordinal: usize,
    pub local_turn_id: AgentTurnId,
    /// User feedback after the runtime's consent gate, optional secret
    /// redaction, and control/bidirectional-character rejection. This is not
    /// a provider-framed prompt. The initial task prompt is deliberately not
    /// retained here.
    pub follow_up_feedback: Option<String>,
    pub agent_text: String,
    pub agent_text_truncated: bool,
    pub commands: Vec<CodexAppServerTurnCommandSummary>,
    pub file_changes: Vec<CodexAppServerTurnFileSummary>,
    pub dropped_updates: u64,
}

/// Cheap clone of the worker's bounded presentation state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerViewSnapshot {
    pub phase: CodexAppServerPhase,
    pub provider_thread_id: Option<String>,
    pub provider_turn_id: Option<String>,
    /// Stable Anvil-local identity for the current/latest flat projection.
    pub displayed_turn_id: Option<AgentTurnId>,
    pub displayed_turn_ordinal: Option<usize>,
    /// Consent-gated user feedback that started the displayed turn. The
    /// initial task prompt is deliberately not retained here.
    pub displayed_follow_up_feedback: Option<String>,
    pub agent_text: String,
    pub agent_text_truncated: bool,
    pub commands: Vec<CodexAppServerCommandView>,
    pub file_changes: Vec<CodexAppServerFileChangeView>,
    pub pending_approvals: Vec<CodexAppServerApproval>,
    /// Completed turns in this still-loaded provider session.
    pub completed_turns: usize,
    /// Oldest-to-newest completed turns preceding the current/latest flat
    /// projection. Its independent history-only budget excludes the already
    /// bounded flat projection. Arc keeps frame-level snapshot cloning
    /// independent of the retained history payload size.
    pub turn_history: Arc<[CodexAppServerTurnHistory]>,
    /// Number of oldest history turns discarded by the count or byte budget.
    pub dropped_turns: usize,
    pub last_error: Option<String>,
    pub dropped_updates: u64,
}

impl Default for CodexAppServerViewSnapshot {
    fn default() -> Self {
        Self {
            phase: CodexAppServerPhase::Created,
            provider_thread_id: None,
            provider_turn_id: None,
            displayed_turn_id: None,
            displayed_turn_ordinal: None,
            displayed_follow_up_feedback: None,
            agent_text: String::new(),
            agent_text_truncated: false,
            commands: Vec::new(),
            file_changes: Vec::new(),
            pending_approvals: Vec::new(),
            completed_turns: 0,
            turn_history: Arc::from([]),
            dropped_turns: 0,
            last_error: None,
            dropped_updates: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexAppServerExitCause {
    Clean,
    Cancelled,
    SpawnFailed,
    ProtocolFailed,
    IoFailed,
    ProviderFailed,
    EventDeliveryFailed,
    WorkerPanicked,
}

/// Process status is recorded only after `wait`/`try_wait` reaped the leader.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodexAppServerProcessExit {
    pub spawned: bool,
    /// True only after containment was armed, the launch gate was observed
    /// stopped, and SIGCONT successfully released the provider payload.
    pub provider_released: bool,
    pub reaped: bool,
    pub containment_verified_empty: bool,
    pub success: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Out-of-band terminal evidence used by the runtime when a critical event
/// could not be delivered. For a spawned process this is published only after
/// the process group has been stopped and its leader reaped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppServerExitReport {
    pub outcome: AgentSessionOutcome,
    pub cause: CodexAppServerExitCause,
    pub detail: Option<String>,
    pub process: CodexAppServerProcessExit,
    pub critical_event_delivery_failed: bool,
    pub stderr_tail: String,
}

/// Version-gated Codex app-server driver for the experimental native slice.
/// `workspace` is deliberately consumed and cannot be cloned; the worker
/// retains it until after child reap.
pub(crate) struct CodexAppServerDriver {
    launch_argv: Vec<String>,
    workspace: Option<PreparedNativeWorkspace>,
    native_home: Option<PreparedNativeCodexHome>,
    command_sender: Sender<AgentCommand>,
    command_receiver: Option<Receiver<AgentCommand>>,
    event_sender: Option<AgentEventSender>,
    event_receiver: AgentEventReceiver,
    cancellation: AgentCancellation,
    view: Arc<Mutex<CodexAppServerViewSnapshot>>,
    exit_report: Arc<Mutex<Option<CodexAppServerExitReport>>>,
    worker: Option<JoinHandle<()>>,
    started: bool,
}

impl CodexAppServerDriver {
    pub(crate) fn new(
        launch_argv: Vec<String>,
        workspace: PreparedNativeWorkspace,
        native_home: PreparedNativeCodexHome,
    ) -> Self {
        debug_assert!(!launch_argv.is_empty());
        let (event_sender, event_receiver) = agent_event_channel();
        let (command_sender, command_receiver) = bounded(CODEX_APP_SERVER_COMMAND_CAPACITY);
        Self {
            launch_argv,
            workspace: Some(workspace),
            native_home: Some(native_home),
            command_sender,
            command_receiver: Some(command_receiver),
            event_sender: Some(event_sender),
            event_receiver,
            cancellation: AgentCancellation::new(),
            view: Arc::new(Mutex::new(CodexAppServerViewSnapshot::default())),
            exit_report: Arc::new(Mutex::new(None)),
            worker: None,
            started: false,
        }
    }

    pub(crate) fn view_snapshot(&self) -> CodexAppServerViewSnapshot {
        self.view.lock().clone()
    }

    pub(crate) fn phase(&self) -> CodexAppServerPhase {
        self.view.lock().phase
    }

    pub(crate) fn take_exit_report(&self) -> Option<CodexAppServerExitReport> {
        self.exit_report.lock().take()
    }

    pub(crate) fn worker_is_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Join only an already-finished worker. This method never waits for future
    /// provider progress and is therefore safe to call from the runtime tick.
    pub(crate) fn join_finished_worker(&mut self) -> Result<bool, AgentDriverError> {
        if !self.worker_is_finished() {
            return Ok(false);
        }
        let Some(worker) = self.worker.take() else {
            return Ok(false);
        };
        worker.join().map_err(|_| {
            let mut report = self.exit_report.lock();
            if report.is_none() {
                *report = Some(CodexAppServerExitReport {
                    outcome: AgentSessionOutcome::Failed,
                    cause: CodexAppServerExitCause::WorkerPanicked,
                    detail: Some("Codex app-server worker panicked".into()),
                    process: CodexAppServerProcessExit::default(),
                    critical_event_delivery_failed: true,
                    stderr_tail: String::new(),
                });
            }
            AgentDriverError::Provider("Codex app-server worker panicked".into())
        })?;
        Ok(true)
    }

    fn receive_event(&self) -> Result<Option<AgentEvent>, AgentDriverError> {
        match self.event_receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(AgentEventReceiveError::Empty) => Ok(None),
            Err(AgentEventReceiveError::Closed) => Err(AgentDriverError::Closed),
        }
    }
}

impl AgentDriver for CodexAppServerDriver {
    fn provider(&self) -> AgentProvider {
        AgentProvider::Codex
    }

    fn start(&mut self, request: AgentStartRequest) -> Result<(), AgentDriverError> {
        if self.started {
            return Err(AgentDriverError::AlreadyStarted);
        }
        request.validate_for_provider(AgentProvider::Codex)?;
        if request.resume_from.is_some() {
            return Err(AgentDriverError::Provider(
                "Codex app-server resume is not enabled for native task sessions".into(),
            ));
        }
        let workspace = self
            .workspace
            .as_ref()
            .ok_or(AgentDriverError::AlreadyStarted)?;
        if request.worktree_path != workspace.display_path() {
            return Err(AgentDriverError::InvalidWorktree);
        }

        let workspace = self.workspace.take().ok_or(AgentDriverError::Closed)?;
        let native_home = self.native_home.take().ok_or(AgentDriverError::Closed)?;
        let commands = self
            .command_receiver
            .take()
            .ok_or(AgentDriverError::Closed)?;
        let sender = self.event_sender.take().ok_or(AgentDriverError::Closed)?;
        let sink = AgentEventSink::new(request.stream.clone(), sender);
        let launch_argv = self.launch_argv.clone();
        let cancellation = self.cancellation.clone();
        let view = Arc::clone(&self.view);
        let report = Arc::clone(&self.exit_report);
        view.lock().phase = CodexAppServerPhase::Spawning;

        let worker = thread::Builder::new()
            .name("anvil-codex-app-server".into())
            .spawn(move || {
                let panic_view = Arc::clone(&view);
                let panic_report = Arc::clone(&report);
                let process_snapshot = Arc::new(Mutex::new(CodexAppServerProcessExit::default()));
                let panic_process_snapshot = Arc::clone(&process_snapshot);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_worker(WorkerContext {
                        launch_argv,
                        workspace,
                        native_home,
                        request,
                        commands,
                        cancellation,
                        sink,
                        view,
                        process_snapshot,
                    })
                }));
                let terminal_report = match result {
                    Ok(report_value) => report_value,
                    Err(_) => {
                        panic_view.lock().phase = CodexAppServerPhase::Failed;
                        CodexAppServerExitReport {
                            outcome: AgentSessionOutcome::Failed,
                            cause: CodexAppServerExitCause::WorkerPanicked,
                            detail: Some("Codex app-server worker panicked".into()),
                            process: *panic_process_snapshot.lock(),
                            critical_event_delivery_failed: true,
                            stderr_tail: String::new(),
                        }
                    }
                };
                *panic_report.lock() = Some(terminal_report);
            })
            // Failure to create a worker proves that its closure was never run,
            // hence no provider process was spawned. The moved workspace is
            // dropped together with the rejected closure.
            .map_err(|error| {
                AgentDriverError::Provider(format!(
                    "could not create Codex app-server worker: {error}"
                ))
            })?;
        self.worker = Some(worker);
        self.started = true;
        Ok(())
    }

    fn send(&mut self, command: AgentCommand) -> Result<(), AgentDriverError> {
        if !self.started {
            return Err(AgentDriverError::NotStarted);
        }
        command.validate()?;
        if self.cancellation.is_cancelled() || self.worker_is_finished() {
            return Err(AgentDriverError::Closed);
        }
        try_send_command(&self.command_sender, &self.view, command)
    }

    fn cancel(&mut self) {
        self.cancellation.cancel();
    }

    fn try_next_event(&mut self) -> Result<Option<AgentEvent>, AgentDriverError> {
        if !self.started {
            return Err(AgentDriverError::NotStarted);
        }
        self.receive_event()
    }
}

fn try_send_command(
    sender: &Sender<AgentCommand>,
    view: &Arc<Mutex<CodexAppServerViewSnapshot>>,
    command: AgentCommand,
) -> Result<(), AgentDriverError> {
    let reservation = match &command {
        AgentCommand::Prompt(_) => Some(CodexAppServerPhase::StartingTurn),
        AgentCommand::FinishSession => Some(CodexAppServerPhase::Stopping),
        AgentCommand::Steer { .. } | AgentCommand::DecideApproval { .. } => None,
    };
    let mut snapshot = reservation
        .map(|phase| {
            let mut snapshot = view.lock();
            if snapshot.phase != CodexAppServerPhase::Ready {
                return Err(AgentDriverError::TurnActive);
            }
            if matches!(&command, AgentCommand::Prompt(_))
                && snapshot.completed_turns >= CODEX_APP_SERVER_LIVE_TURN_MAX
            {
                return Err(AgentDriverError::TurnLimitReached {
                    limit: CODEX_APP_SERVER_LIVE_TURN_MAX,
                });
            }
            snapshot.phase = phase;
            Ok(snapshot)
        })
        .transpose()?;

    match sender.try_send(command) {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(snapshot) = snapshot.as_mut() {
                snapshot.phase = CodexAppServerPhase::Ready;
            }
            Err(match error {
                TrySendError::Full(_) => AgentDriverError::Backpressure {
                    queued_messages: sender.len(),
                    message_capacity: CODEX_APP_SERVER_COMMAND_CAPACITY,
                },
                TrySendError::Disconnected(_) => AgentDriverError::Closed,
            })
        }
    }
}

impl Drop for CodexAppServerDriver {
    fn drop(&mut self) {
        self.cancellation.cancel();
        // Ownership of a native process is never detached. The worker's
        // cancellation grace is bounded, after which it kills the complete
        // private process group and reaps the leader before returning.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RpcId {
    Integer(i64),
    String(String),
}

impl RpcId {
    fn parse(value: &Value) -> Result<Self, WorkerFailure> {
        match value {
            Value::Number(number) => number
                .as_i64()
                .map(Self::Integer)
                .ok_or_else(|| WorkerFailure::protocol("JSON-RPC id is not an integer")),
            Value::String(value) if !value.is_empty() && value.len() <= FIELD_MAX_BYTES => {
                Ok(Self::String(value.clone()))
            }
            _ => Err(WorkerFailure::protocol(
                "JSON-RPC id must be a bounded string or integer",
            )),
        }
    }

    fn value(&self) -> Value {
        match self {
            Self::Integer(value) => Value::from(*value),
            Self::String(value) => Value::String(value.clone()),
        }
    }
}

#[derive(Debug)]
enum PendingClientRequest {
    Initialize,
    ConfigRead,
    AuthLogin,
    ThreadStart,
    TurnStart(AgentTurnId),
    Steer,
    Interrupt,
}

#[derive(Clone, Debug)]
struct PendingApproval {
    rpc_id: RpcId,
    local_turn_id: AgentTurnId,
    kind: CodexAppServerApprovalKind,
    item_id: String,
}

#[derive(Debug)]
struct PendingWrite {
    bytes: Vec<u8>,
    written: usize,
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        // One startup record carries a short-lived access token. Zero every
        // record uniformly so no secret-bearing allocation survives either a
        // successful write or an early protocol/process failure.
        self.bytes.fill(0);
    }
}

#[derive(Debug, Default)]
struct WireWriteQueue {
    pending: VecDeque<PendingWrite>,
    pending_bytes: usize,
}

impl WireWriteQueue {
    fn enqueue(&mut self, message: &Value) -> Result<(), WorkerFailure> {
        self.enqueue_serializable(message)
    }

    fn enqueue_serializable<T: Serialize + ?Sized>(
        &mut self,
        message: &T,
    ) -> Result<(), WorkerFailure> {
        let mut bytes = serde_json::to_vec(message)
            .map_err(|error| WorkerFailure::protocol(format!("cannot encode JSON-RPC: {error}")))?;
        bytes.push(b'\n');
        if bytes.len() > CODEX_APP_SERVER_JSONL_MAX_BYTES {
            return Err(WorkerFailure::protocol(format!(
                "outbound JSONL record exceeds {} bytes",
                CODEX_APP_SERVER_JSONL_MAX_BYTES
            )));
        }
        if self.pending.len() >= CODEX_APP_SERVER_WRITE_QUEUE_MAX_MESSAGES
            || self
                .pending_bytes
                .checked_add(bytes.len())
                .is_none_or(|size| size > CODEX_APP_SERVER_WRITE_QUEUE_MAX_BYTES)
        {
            return Err(WorkerFailure::io("bounded app-server write queue is full"));
        }
        self.pending_bytes += bytes.len();
        self.pending.push_back(PendingWrite { bytes, written: 0 });
        Ok(())
    }

    fn flush(&mut self, stdin: &mut ChildStdin) -> Result<(), WorkerFailure> {
        while let Some(front) = self.pending.front_mut() {
            match stdin.write(&front.bytes[front.written..]) {
                Ok(0) => return Err(WorkerFailure::io("app-server stdin closed while writing")),
                Ok(written) => {
                    front.written += written;
                    self.pending_bytes = self.pending_bytes.saturating_sub(written);
                    if front.written == front.bytes.len() {
                        self.pending.pop_front();
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(WorkerFailure::io(format!(
                        "cannot write app-server stdin: {error}"
                    )))
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct JsonLineReader {
    buffer: Vec<u8>,
    eof: bool,
}

impl JsonLineReader {
    fn read_available<R: Read>(&mut self, stdout: &mut R) -> Result<Vec<Value>, WorkerFailure> {
        let mut chunk = [0_u8; 16 * 1024];
        let mut read_budget = READ_BYTES_PER_TICK;
        let mut messages = Vec::new();
        self.extract_lines(&mut messages)?;
        while read_budget > 0 && messages.len() < READ_MESSAGES_PER_TICK {
            let wanted = chunk.len().min(read_budget);
            match stdout.read(&mut chunk[..wanted]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(read) => {
                    self.buffer.extend_from_slice(&chunk[..read]);
                    read_budget -= read;
                    self.extract_lines(&mut messages)?;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(WorkerFailure::io(format!(
                        "cannot read app-server stdout: {error}"
                    )))
                }
            }
        }

        // A buffered complete line waits for the next tick when the message
        // budget is exhausted. Only the current first record is length-bound.
        let first_record_len = self
            .buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(self.buffer.len());
        if first_record_len > CODEX_APP_SERVER_JSONL_MAX_BYTES {
            return Err(WorkerFailure::protocol(format!(
                "inbound JSONL record exceeds {} bytes",
                CODEX_APP_SERVER_JSONL_MAX_BYTES
            )));
        }
        if self.eof && !self.buffer.is_empty() && !self.buffer.contains(&b'\n') {
            return Err(WorkerFailure::protocol(
                "app-server stdout ended with a partial JSONL record",
            ));
        }
        Ok(messages)
    }

    fn extract_lines(&mut self, messages: &mut Vec<Value>) -> Result<(), WorkerFailure> {
        while messages.len() < READ_MESSAGES_PER_TICK {
            let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            if newline > CODEX_APP_SERVER_JSONL_MAX_BYTES {
                return Err(WorkerFailure::protocol(format!(
                    "inbound JSONL record exceeds {} bytes",
                    CODEX_APP_SERVER_JSONL_MAX_BYTES
                )));
            }
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            jterm_core::bounded_json::validate_no_duplicate_members(&line).map_err(|error| {
                WorkerFailure::protocol(format!("invalid app-server JSONL record: {error}"))
            })?;
            let message: Value = serde_json::from_slice(&line).map_err(|error| {
                WorkerFailure::protocol(format!("invalid app-server JSONL record: {error}"))
            })?;
            if !message.is_object() {
                return Err(WorkerFailure::protocol(
                    "app-server JSONL record is not an object",
                ));
            }
            messages.push(message);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct WorkerFailure {
    cause: CodexAppServerExitCause,
    detail: String,
    critical_event_delivery_failed: bool,
}

impl WorkerFailure {
    fn spawn(detail: impl Into<String>) -> Self {
        Self::new(CodexAppServerExitCause::SpawnFailed, detail, false)
    }

    fn protocol(detail: impl Into<String>) -> Self {
        Self::new(CodexAppServerExitCause::ProtocolFailed, detail, false)
    }

    fn io(detail: impl Into<String>) -> Self {
        Self::new(CodexAppServerExitCause::IoFailed, detail, false)
    }

    fn provider(detail: impl Into<String>) -> Self {
        Self::new(CodexAppServerExitCause::ProviderFailed, detail, false)
    }

    fn event(error: AgentEventSendError) -> Self {
        Self::new(
            CodexAppServerExitCause::EventDeliveryFailed,
            format!("critical Agent event could not be queued: {error}"),
            true,
        )
    }

    fn new(
        cause: CodexAppServerExitCause,
        detail: impl Into<String>,
        critical_event_delivery_failed: bool,
    ) -> Self {
        let (detail, _) = visible_bounded(&detail.into(), FIELD_MAX_BYTES);
        Self {
            cause,
            detail,
            critical_event_delivery_failed,
        }
    }
}

#[derive(Clone, Debug)]
struct TerminalIntent {
    outcome: AgentSessionOutcome,
    cause: CodexAppServerExitCause,
    detail: Option<String>,
    critical_event_delivery_failed: bool,
}

struct WorkerContext {
    launch_argv: Vec<String>,
    workspace: PreparedNativeWorkspace,
    native_home: PreparedNativeCodexHome,
    request: AgentStartRequest,
    commands: Receiver<AgentCommand>,
    cancellation: AgentCancellation,
    sink: AgentEventSink,
    view: Arc<Mutex<CodexAppServerViewSnapshot>>,
    process_snapshot: Arc<Mutex<CodexAppServerProcessExit>>,
}

impl TerminalIntent {
    fn clean() -> Self {
        Self {
            outcome: AgentSessionOutcome::Clean,
            cause: CodexAppServerExitCause::Clean,
            detail: None,
            critical_event_delivery_failed: false,
        }
    }

    fn cancelled(detail: impl Into<String>) -> Self {
        Self {
            outcome: AgentSessionOutcome::Cancelled,
            cause: CodexAppServerExitCause::Cancelled,
            detail: Some(detail.into()),
            critical_event_delivery_failed: false,
        }
    }

    fn failed(failure: WorkerFailure) -> Self {
        Self {
            outcome: AgentSessionOutcome::Failed,
            cause: failure.cause,
            detail: Some(failure.detail),
            critical_event_delivery_failed: failure.critical_event_delivery_failed,
        }
    }
}

struct ChildProcessGuard {
    child: Option<Child>,
    process_snapshot: Arc<Mutex<CodexAppServerProcessExit>>,
    containment: Option<CgroupContainment>,
    scope_unit: String,
}

impl ChildProcessGuard {
    fn new(
        child: Child,
        process_snapshot: Arc<Mutex<CodexAppServerProcessExit>>,
        scope_unit: String,
    ) -> Self {
        process_snapshot.lock().spawned = true;
        Self {
            child: Some(child),
            process_snapshot,
            containment: None,
            scope_unit,
        }
    }

    fn attach_containment(&mut self) -> Result<(), WorkerFailure> {
        let pid = self
            .child
            .as_ref()
            .ok_or_else(|| WorkerFailure::io("app-server wrapper already exited"))?
            .id();
        let containment = CgroupContainment::attach(pid, &self.scope_unit)?;
        // Entering the systemd scope happens before the launch-gate shell runs
        // `SIGSTOP`. SIGCONT is not queued, so observing only the cgroup would
        // race and could leave a later STOP asleep forever. Require both
        // conditions under the same bounded setup deadline.
        wait_for_launch_gate_stop(pid)?;
        self.containment = Some(containment);
        release_launch_gate(pid as i32)?;
        self.process_snapshot.lock().provider_released = true;
        Ok(())
    }

    fn cleanup_failed_attachment(&mut self) -> Result<(), WorkerFailure> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let pid = child.id() as i32;
        let _ = signal_process_group(pid, libc::SIGKILL);
        let status = loop {
            match child.wait() {
                Ok(status) => break status,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(WorkerFailure::io(format!(
                        "cannot reap failed systemd-run setup: {error}"
                    )))
                }
            }
        };
        self.record_reaped(status);
        self.child.take();
        // The trusted launch gate stops before exec'ing the provider. Therefore
        // a failed attachment cannot have created any untrusted descendants.
        self.process_snapshot.lock().containment_verified_empty = true;
        Ok(())
    }

    fn child_mut(&mut self) -> Result<&mut Child, WorkerFailure> {
        self.child
            .as_mut()
            .ok_or_else(|| WorkerFailure::io("app-server child was already reaped"))
    }

    /// Observe leader exit without reaping it. Keeping the zombie leader
    /// anchors the private PGID until every descendant has been SIGKILLed.
    fn leader_has_exited(&mut self) -> Result<bool, WorkerFailure> {
        let Some(child) = self.child.as_ref() else {
            return Ok(true);
        };
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(WorkerFailure::io(format!(
                "cannot inspect app-server process: {}",
                io::Error::last_os_error()
            )));
        }
        let info = unsafe { info.assume_init() };
        Ok(unsafe { info.si_pid() } != 0)
    }

    /// Stop the whole private process group and reap its leader. SIGKILL is
    /// always sent before leader reap, even if the leader exited naturally or
    /// after SIGTERM, so TERM-ignoring grandchildren cannot retain pipes/FDs.
    fn stop_and_reap(&mut self) -> Result<ExitStatus, WorkerFailure> {
        let Some(child) = self.child.as_ref() else {
            return Err(WorkerFailure::io(
                "app-server process was reaped before shutdown",
            ));
        };
        let pid = child.id() as i32;
        let term_error = signal_process_group(pid, libc::SIGTERM).err();
        let deadline = Instant::now()
            .checked_add(TERMINATE_GRACE)
            .unwrap_or_else(Instant::now);
        loop {
            match self.leader_has_exited() {
                Ok(true) => break,
                Ok(false) if Instant::now() < deadline => thread::sleep(IO_POLL_INTERVAL),
                Ok(false) => break,
                Err(_) => break,
            }
        }
        let containment = self
            .containment
            .as_mut()
            .ok_or_else(|| WorkerFailure::io("app-server scope containment was not established"))?;
        // Do not reap the leader until cgroup.events proves that no escaped
        // descendant (including setsid daemons) remains in the scope.
        containment.kill_all_and_wait_empty()?;
        self.process_snapshot.lock().containment_verified_empty = true;
        // Keep the PGID kill as a defence in depth for setup failures and old
        // kernels. cgroup.kill is what covers daemonized/setsid descendants.
        let kill_error = signal_process_group(pid, libc::SIGKILL).err();
        let status = loop {
            match self
                .child
                .as_mut()
                .expect("child remains owned until wait succeeds")
                .wait()
            {
                Ok(status) => break status,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(WorkerFailure::io(format!(
                        "cannot reap app-server after SIGKILL: {error}"
                    )))
                }
            }
        };
        self.record_reaped(status);
        self.child.take();
        if let Some(error) = kill_error.or(term_error) {
            // ESRCH is already normalized by signal_process_group. A different
            // signalling failure is meaningful even though wait proved reap.
            return Err(WorkerFailure::io(format!(
                "app-server was reaped but process-group signalling failed: {}",
                error.detail
            )));
        }
        Ok(status)
    }

    fn record_reaped(&self, status: ExitStatus) {
        let mut snapshot = self.process_snapshot.lock();
        snapshot.reaped = true;
        snapshot.success = status.success();
        snapshot.code = status.code();
        snapshot.signal = status.signal();
    }
}

fn wait_for_launch_gate_stop(pid: u32) -> Result<(), WorkerFailure> {
    let deadline = Instant::now()
        .checked_add(CONTAINMENT_ATTACH_TIMEOUT)
        .unwrap_or_else(Instant::now);
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WSTOPPED | libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(WorkerFailure::spawn(format!(
                "cannot observe app-server launch gate: {}",
                io::Error::last_os_error()
            )));
        }
        let info = unsafe { info.assume_init() };
        if unsafe { info.si_pid() } != 0 {
            if info.si_code == libc::CLD_STOPPED {
                return Ok(());
            }
            return Err(WorkerFailure::spawn(
                "app-server launch gate exited before containment was armed",
            ));
        }
        if Instant::now() >= deadline {
            return Err(WorkerFailure::spawn(
                "app-server launch gate did not stop before the containment deadline",
            ));
        }
        thread::sleep(IO_POLL_INTERVAL);
    }
}

struct CgroupContainment {
    kill: File,
    events: File,
    guardian: CgroupGuardian,
}

impl CgroupContainment {
    fn attach(pid: u32, expected_unit: &str) -> Result<Self, WorkerFailure> {
        let deadline = Instant::now()
            .checked_add(CONTAINMENT_ATTACH_TIMEOUT)
            .unwrap_or_else(Instant::now);
        let cgroup_path = loop {
            match read_unified_cgroup(pid) {
                Ok(path)
                    if path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == expected_unit) =>
                {
                    break path;
                }
                Ok(_) if Instant::now() < deadline => thread::sleep(IO_POLL_INTERVAL),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(IO_POLL_INTERVAL);
                }
                Ok(path) => {
                    return Err(WorkerFailure::spawn(format!(
                        "systemd scope did not attach app-server wrapper (last cgroup {})",
                        path.display()
                    )))
                }
                Err(error) => return Err(error),
            }
        };
        let root = PathBuf::from(CGROUP_ROOT);
        let relative = cgroup_path
            .strip_prefix("/")
            .map_err(|_| WorkerFailure::spawn("systemd reported a non-absolute unified cgroup"))?;
        let resolved_root = std::fs::canonicalize(&root).map_err(|error| {
            WorkerFailure::spawn(format!("cannot resolve cgroup v2 root: {error}"))
        })?;
        let resolved = std::fs::canonicalize(root.join(relative)).map_err(|error| {
            WorkerFailure::spawn(format!("cannot resolve transient scope cgroup: {error}"))
        })?;
        if !resolved.starts_with(&resolved_root)
            || resolved.file_name().and_then(|name| name.to_str()) != Some(expected_unit)
        {
            return Err(WorkerFailure::spawn(
                "transient scope cgroup identity did not match its random unit name",
            ));
        }
        let kill = OpenOptions::new()
            .write(true)
            .open(resolved.join("cgroup.kill"))
            .map_err(|error| {
                WorkerFailure::spawn(format!("cgroup.kill is unavailable: {error}"))
            })?;
        let events = File::open(resolved.join("cgroup.events")).map_err(|error| {
            WorkerFailure::spawn(format!("cgroup.events is unavailable: {error}"))
        })?;
        let guardian = CgroupGuardian::spawn(&kill)?;
        Ok(Self {
            kill,
            events,
            guardian,
        })
    }

    fn kill_all_and_wait_empty(&mut self) -> Result<(), WorkerFailure> {
        // The guardian is outside the provider scope. Closing its private
        // trigger pipe makes it write cgroup.kill; kernel EOF does the same if
        // Anvil is abruptly killed, covering daemonized setsid descendants.
        // Guardian failure (including ENODEV from a concurrently collected
        // scope) is not itself evidence that the scope is empty. Record it,
        // issue the direct kill, and decide only from pinned cgroup.events.
        let mut kill_failures = Vec::new();
        if let Err(failure) = self.guardian.trigger_and_wait() {
            kill_failures.push(failure.detail);
        }
        // A direct write is idempotent and covers a guardian that exited after
        // an interrupted shell builtin but before reporting status.
        if let Err(error) = self.kill.seek(SeekFrom::Start(0)) {
            kill_failures.push(format!("cannot rewind cgroup.kill: {error}"));
        }
        match self.kill.write_all(b"1\n") {
            Ok(()) => {}
            Err(error) => kill_failures.push(format!("cgroup.kill failed: {error}")),
        }
        let deadline = Instant::now()
            .checked_add(TERMINATE_GRACE)
            .unwrap_or_else(Instant::now);
        loop {
            match self.events.seek(SeekFrom::Start(0)) {
                Ok(_) => {}
                // With `--collect`, systemd removes an empty scope
                // immediately. An already-open cgroup.events descriptor then
                // returns ENODEV; removal itself proves no member remains in
                // this exact pinned cgroup.
                Err(error) if error.raw_os_error() == Some(libc::ENODEV) => return Ok(()),
                Err(error) => {
                    return Err(WorkerFailure::io(format!(
                        "cannot rewind cgroup.events: {error}"
                    )))
                }
            }
            let mut state = String::new();
            match self.events.read_to_string(&mut state) {
                Ok(_) => {}
                Err(error) if error.raw_os_error() == Some(libc::ENODEV) => return Ok(()),
                Err(error) => {
                    return Err(WorkerFailure::io(format!(
                        "cannot read cgroup.events: {error}"
                    )))
                }
            }
            if state.lines().any(|line| line == "populated 0") {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let kill_detail = if kill_failures.is_empty() {
                    String::new()
                } else {
                    format!("; cleanup attempts: {}", kill_failures.join("; "))
                };
                return Err(WorkerFailure::io(format!(
                    "transient app-server cgroup remained populated after cgroup.kill{kill_detail}"
                )));
            }
            thread::sleep(IO_POLL_INTERVAL);
        }
    }
}

struct CgroupGuardian {
    child: Option<Child>,
    trigger: Option<ChildStdin>,
}

impl CgroupGuardian {
    fn spawn(kill: &File) -> Result<Self, WorkerFailure> {
        use std::os::unix::process::CommandExt;

        let guardian_output = kill.try_clone().map_err(|error| {
            WorkerFailure::spawn(format!("cannot clone cgroup guardian output: {error}"))
        })?;
        let mut command = Command::new(SYSTEM_SHELL);
        clear_guardian_environment(&mut command);
        command
            .arg("-c")
            .arg("while IFS= read -r anvil_guard_line; do :; done; printf '1\\n'")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(guardian_output))
            .stderr(Stdio::null());
        command.process_group(0);
        // SAFETY: process-group setup is handled by CommandExt; signal(2) is
        // async-signal-safe. Ignored dispositions survive exec so terminal
        // group signals cannot kill the out-of-scope guardian before provider
        // cleanup is triggered by the parent's pipe EOF.
        unsafe {
            command.pre_exec(|| {
                for signal in [libc::SIGINT, libc::SIGHUP, libc::SIGTERM] {
                    if libc::signal(signal, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            WorkerFailure::spawn(format!("cannot start cgroup cleanup guardian: {error}"))
        })?;
        let trigger = child.stdin.take().ok_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            WorkerFailure::spawn("cgroup cleanup guardian has no trigger pipe")
        })?;
        Ok(Self {
            child: Some(child),
            trigger: Some(trigger),
        })
    }

    fn trigger_and_wait(&mut self) -> Result<(), WorkerFailure> {
        self.trigger.take();
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let status = loop {
            match child.wait() {
                Ok(status) => break status,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(WorkerFailure::io(format!(
                        "cannot reap cgroup cleanup guardian: {error}"
                    )))
                }
            }
        };
        self.child.take();
        if status.success() {
            Ok(())
        } else {
            Err(WorkerFailure::io(format!(
                "cgroup cleanup guardian exited with {status}"
            )))
        }
    }
}

fn clear_guardian_environment(command: &mut Command) {
    command.env_clear();
}

impl Drop for CgroupGuardian {
    fn drop(&mut self) {
        self.trigger.take();
        if let Some(child) = self.child.as_mut() {
            loop {
                match child.wait() {
                    Ok(_) => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }
        self.child.take();
    }
}

fn read_unified_cgroup(pid: u32) -> Result<PathBuf, WorkerFailure> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).map_err(|error| {
        WorkerFailure::spawn(format!("cannot inspect app-server cgroup: {error}"))
    })?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
        .ok_or_else(|| WorkerFailure::spawn("host does not expose a unified cgroup v2 path"))
}

impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        // This guard exists for unwind and exceptional setup paths. Never let
        // a provider or descendant retain the workspace capability afterward.
        while self.child.is_some() && !self.process_snapshot.lock().reaped {
            if self.containment.is_some() {
                let _ = self.stop_and_reap();
            } else {
                // Before containment is installed the launch gate has never
                // been released, so no provider code can have executed. A
                // panic in this narrow setup window must still kill and reap
                // the stopped wrapper instead of retrying stop_and_reap's
                // missing-containment error forever.
                let _ = self.cleanup_failed_attachment();
            }
            if self.child.is_some() && !self.process_snapshot.lock().reaped {
                thread::sleep(IO_POLL_INTERVAL);
            }
        }
    }
}

fn signal_process_group(pid: i32, signal: i32) -> Result<(), WorkerFailure> {
    // The leader remains unreaped while signalling, anchoring its private PGID
    // and preventing accidental signalling after PID/PGID reuse.
    let result = unsafe { libc::kill(-pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(WorkerFailure::io(format!(
            "cannot signal app-server process group: {error}"
        )))
    }
}

fn release_launch_gate(pid: i32) -> Result<(), WorkerFailure> {
    let result = unsafe { libc::kill(-pid, libc::SIGCONT) };
    if result == 0 {
        Ok(())
    } else {
        Err(WorkerFailure::spawn(format!(
            "cannot release app-server launch gate: {}",
            io::Error::last_os_error()
        )))
    }
}

fn selected_environment(
    source: &[(OsString, OsString)],
    allowlist: &[&str],
) -> Vec<(OsString, OsString)> {
    source
        .iter()
        .filter(|(name, _)| {
            allowlist
                .iter()
                .any(|allowed| name.as_os_str() == OsStr::new(allowed))
        })
        .cloned()
        .collect()
}

fn native_tool_environment(
    source: &[(OsString, OsString)],
    repository: &std::path::Path,
    worktree: &std::path::Path,
) -> Result<BTreeMap<String, String>, WorkerFailure> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let source_path = source
        .iter()
        .find(|(name, _)| name.as_os_str() == OsStr::new("PATH"))
        .map(|(_, value)| value.as_os_str())
        .unwrap_or_else(|| OsStr::new(TRUSTED_PATH));
    let mut seen = HashSet::new();
    let mut directories = Vec::new();
    for candidate in std::env::split_paths(source_path) {
        if !candidate.is_absolute() {
            continue;
        }
        let Ok(candidate) = std::fs::canonicalize(candidate) else {
            continue;
        };
        if candidate.starts_with(repository) || candidate.starts_with(worktree) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        // Permit user-owned toolchains (rustup, nvm) and root-owned system
        // directories, but never repo paths, unrelated owners, or directories
        // writable by group/other.
        let current_user = unsafe { libc::geteuid() };
        if !metadata.is_dir()
            || (metadata.uid() != current_user && metadata.uid() != 0)
            || metadata.permissions().mode() & 0o022 != 0
        {
            continue;
        }
        let Some(candidate) = candidate.to_str() else {
            continue;
        };
        if seen.insert(candidate.to_string()) {
            directories.push(candidate.to_string());
        }
        if directories.len() >= TOOL_PATH_MAX_DIRECTORIES {
            break;
        }
    }
    if directories.is_empty() {
        return Err(WorkerFailure::spawn(
            "native Codex has no safe absolute tool directories in PATH",
        ));
    }
    let path = directories.join(":");
    if path.len() > TOOL_PATH_MAX_BYTES {
        return Err(WorkerFailure::spawn(
            "native Codex safe tool PATH exceeds its byte limit",
        ));
    }

    let mut environment = BTreeMap::from([("PATH".to_string(), path)]);
    if let Some((_, home)) = source
        .iter()
        .find(|(name, _)| name.as_os_str() == OsStr::new("HOME"))
    {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            if let Ok(home) = std::fs::canonicalize(home) {
                if home.is_dir()
                    && !home.starts_with(repository)
                    && !home.starts_with(worktree)
                    && home.to_str().is_some()
                {
                    environment.insert("HOME".into(), home.to_string_lossy().into_owned());
                }
            }
        }
    }
    for key in ["LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "TZ", "USER"] {
        let Some((_, value)) = source
            .iter()
            .find(|(name, _)| name.as_os_str() == OsStr::new(key))
        else {
            continue;
        };
        let Some(value) = value.to_str() else {
            continue;
        };
        if value.len() <= FIELD_MAX_BYTES && !value.chars().any(char::is_control) {
            environment.insert(key.to_string(), value.to_string());
        }
    }
    Ok(environment)
}

fn push_native_tool_policy(command: &mut Command, environment: &BTreeMap<String, String>) {
    command
        .arg("-c")
        .arg("allow_login_shell=false")
        .arg("-c")
        .arg("shell_environment_policy.inherit=\"none\"")
        .arg("-c")
        .arg("shell_environment_policy.ignore_default_excludes=false");
    for (name, value) in environment {
        command.arg("-c").arg(format!(
            "shell_environment_policy.set.{name}={}",
            toml::Value::String(value.clone())
        ));
    }
}

fn launch_gate_script() -> String {
    format!(
        "{TRUSTED_KILL} -STOP \"$$\" || exit 125; exec {TRUSTED_ENV} -u DBUS_SESSION_BUS_ADDRESS -u XDG_RUNTIME_DIR -u _ \"$@\""
    )
}

fn run_worker(context: WorkerContext) -> CodexAppServerExitReport {
    let WorkerContext {
        launch_argv,
        workspace,
        mut native_home,
        request,
        commands,
        cancellation,
        sink,
        view,
        process_snapshot,
    } = context;
    if cancellation.is_cancelled() {
        return finish_cancelled_without_child(
            &sink,
            &view,
            process_snapshot,
            "Native Codex was cancelled before final prerequisite verification",
        );
    }
    if let Err(error) = workspace.revalidate_before_spawn(cancellation.shared_flag()) {
        if cancellation.is_cancelled() {
            return finish_cancelled_without_child(
                &sink,
                &view,
                process_snapshot,
                "Native Codex was cancelled during final worktree verification",
            );
        }
        return finish_without_child(
            &sink,
            &view,
            process_snapshot,
            WorkerFailure::spawn(format!(
                "native worktree changed after background preparation: {error}"
            )),
        );
    }
    let refreshed_launch = match AgentLaunchSpec::resolve_native(
        AgentProvider::Codex,
        workspace.repository_path(),
        workspace.display_path(),
    ) {
        Ok(argv) => argv,
        Err(error) => {
            return finish_without_child(
                &sink,
                &view,
                process_snapshot,
                WorkerFailure::spawn(format!(
                    "native launcher changed after background preparation: {error}"
                )),
            )
        }
    };
    if refreshed_launch != launch_argv {
        return finish_without_child(
            &sink,
            &view,
            process_snapshot,
            WorkerFailure::spawn("native launcher changed after background preparation"),
        );
    }
    if cancellation.is_cancelled() {
        return finish_cancelled_without_child(
            &sink,
            &view,
            process_snapshot,
            "Native Codex was cancelled before credential handoff",
        );
    }
    let executable = launch_argv
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("codex"));
    let native_home_path = native_home.path().to_path_buf();
    let native_credentials = match native_home.take_credentials() {
        Ok(credentials) => credentials,
        Err(error) => {
            return finish_without_child(
                &sink,
                &view,
                process_snapshot,
                WorkerFailure::spawn(error.to_string()),
            )
        }
    };
    let wire_root = workspace.wire_path().to_string_lossy().into_owned();
    let wire_cwd = workspace.wire_cwd().to_string_lossy().into_owned();
    let mut stderr_tail = String::new();
    let scope_unit = format!("anvil-codex-{}.scope", Uuid::new_v4());
    let inherited_environment: Vec<_> = std::env::vars_os().collect();
    let systemd_environment =
        selected_environment(&inherited_environment, SYSTEMD_WRAPPER_ENV_ALLOWLIST);
    let provider_environment = selected_environment(&inherited_environment, PROVIDER_ENV_ALLOWLIST);
    let tool_environment = match native_tool_environment(
        &inherited_environment,
        workspace.repository_path(),
        workspace.display_path(),
    ) {
        Ok(environment) => environment,
        Err(failure) => return finish_without_child(&sink, &view, process_snapshot, failure),
    };
    let mut command = Command::new(SYSTEMD_RUN);
    command
        .env_clear()
        .env("PATH", TRUSTED_PATH)
        .arg("--user")
        .arg("--scope")
        .arg("--quiet")
        .arg("--collect")
        .arg(format!("--unit={scope_unit}"))
        .arg("--property=KillMode=control-group")
        .arg("--property=TimeoutStopSec=500ms")
        .arg("--")
        .arg(SYSTEM_SHELL)
        .arg("-c")
        .arg(launch_gate_script())
        .arg("anvil-codex-launch-gate")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for argument in &launch_argv {
        command.arg(argument);
    }
    command.arg("app-server").arg("--strict-config");
    for feature in NATIVE_CODEX_DISABLED_FEATURES {
        command.arg("--disable").arg(feature);
    }
    command.arg("-c").arg("web_search=\"disabled\"");
    push_native_tool_policy(&mut command, &tool_environment);
    command.arg("--stdio");
    for (name, value) in systemd_environment {
        command.env(name, value);
    }
    for (name, value) in provider_environment {
        command.env(name, value);
    }
    command
        .env("HOME", &native_home_path)
        .env("CODEX_HOME", &native_home_path);
    // This single native boundary performs private process-group setup,
    // descriptor-based fchdir, capability inheritance, and PDEATHSIG.
    workspace.configure_child_command(&mut command);

    if cancellation.is_cancelled() {
        return finish_cancelled_without_child(
            &sink,
            &view,
            process_snapshot,
            "Native Codex was cancelled before provider spawn",
        );
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return finish_without_child(
                &sink,
                &view,
                process_snapshot,
                WorkerFailure::spawn(format!("cannot start {}: {error}", executable.display())),
            )
        }
    };
    let mut process = ChildProcessGuard::new(child, Arc::clone(&process_snapshot), scope_unit);
    if let Err(failure) = process.attach_containment() {
        // A trusted systemd-run wrapper does not execute its payload before
        // scope attachment. Kill both the known unit and private PGID, then
        // reap before publishing the setup failure.
        let mut intent = TerminalIntent::failed(failure);
        if let Err(cleanup_failure) = process.cleanup_failed_attachment() {
            intent = TerminalIntent::failed(cleanup_failure);
        }
        return finish_report(&sink, &view, process_snapshot, intent, stderr_tail);
    }
    let pipes = take_and_configure_pipes(&mut process);
    let (mut stdin, mut stdout, mut stderr) = match pipes {
        Ok(pipes) => pipes,
        Err(failure) => {
            let mut intent = TerminalIntent::failed(failure);
            if let Some(stop_failure) = ensure_stopped_and_reaped(&mut process) {
                intent = TerminalIntent::failed(stop_failure);
            }
            return finish_report(&sink, &view, process_snapshot, intent, stderr_tail);
        }
    };

    view.lock().phase = CodexAppServerPhase::Initializing;
    let mut machine = ProtocolMachine::new(
        wire_root,
        wire_cwd,
        NativeProtocolAuthority {
            expected_codex_home: native_home_path,
            expected_tool_environment: tool_environment,
            credentials: native_credentials,
        },
        request.initial_prompt,
        sink,
        Arc::clone(&view),
    );
    let mut writes = WireWriteQueue::default();
    let mut reader = JsonLineReader::default();
    let mut cancellation_deadline = None;
    let startup_deadline = Instant::now()
        .checked_add(STARTUP_HANDSHAKE_TIMEOUT)
        .unwrap_or_else(Instant::now);
    let mut interrupt_queued = false;
    let mut terminal: Option<TerminalIntent> = machine
        .queue_initialize(&mut writes)
        .err()
        .map(TerminalIntent::failed);

    while terminal.is_none() {
        if !machine.startup_handshake_complete() && Instant::now() >= startup_deadline {
            terminal = Some(TerminalIntent::failed(WorkerFailure::provider(
                "Codex app-server startup handshake timed out",
            )));
        }
        if cancellation.is_cancelled() {
            if cancellation_deadline.is_none() {
                view.lock().phase = CodexAppServerPhase::Cancelling;
                cancellation_deadline = Some(
                    Instant::now()
                        .checked_add(CANCEL_INTERRUPT_GRACE)
                        .unwrap_or_else(Instant::now),
                );
            }
            if !interrupt_queued && machine.can_interrupt() {
                match machine.queue_interrupt(&mut writes) {
                    Ok(()) => interrupt_queued = true,
                    Err(failure) => terminal = Some(TerminalIntent::failed(failure)),
                }
            }
            if machine.is_idle() {
                terminal = Some(TerminalIntent::cancelled(
                    "native Codex session cancelled before an active turn",
                ));
            } else if cancellation_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                terminal = Some(TerminalIntent::cancelled(
                    "native Codex turn interrupt timed out; provider was stopped",
                ));
            }
        }

        if terminal.is_none() {
            if let Err(failure) = writes.flush(&mut stdin) {
                terminal = Some(TerminalIntent::failed(failure));
            }
        }

        if terminal.is_none() {
            match reader.read_available(&mut stdout) {
                Ok(messages) => {
                    for message in messages {
                        match machine.handle_message(
                            message,
                            &mut writes,
                            cancellation.is_cancelled(),
                        ) {
                            Ok(Some(intent)) => {
                                terminal = Some(intent);
                                break;
                            }
                            Ok(None) => {}
                            Err(failure) => {
                                terminal = Some(TerminalIntent::failed(failure));
                                break;
                            }
                        }
                    }
                }
                Err(failure) => terminal = Some(TerminalIntent::failed(failure)),
            }
        }

        if terminal.is_none() && reader.eof {
            terminal = Some(TerminalIntent::failed(WorkerFailure::protocol(
                "app-server stdout closed before Anvil ended the native session",
            )));
        }

        drain_stderr(&mut stderr, &mut stderr_tail);

        if terminal.is_none() {
            loop {
                match commands.try_recv() {
                    Ok(command) => match machine.handle_command(command, &mut writes) {
                        Ok(Some(intent)) => {
                            terminal = Some(intent);
                            break;
                        }
                        Ok(None) => {}
                        Err(failure) => {
                            terminal = Some(TerminalIntent::failed(failure));
                            break;
                        }
                    },
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }
        }

        if terminal.is_none() {
            match process.leader_has_exited() {
                Ok(true) => {
                    let detail =
                        "Codex app-server exited before Anvil ended the native session".to_string();
                    terminal = Some(if cancellation.is_cancelled() {
                        TerminalIntent::cancelled(detail)
                    } else {
                        TerminalIntent::failed(WorkerFailure::provider(detail))
                    });
                }
                Ok(false) => {}
                Err(failure) => terminal = Some(TerminalIntent::failed(failure)),
            }
        }

        if terminal.is_none() {
            thread::sleep(if machine.is_idle() {
                IDLE_IO_POLL_INTERVAL
            } else {
                IO_POLL_INTERVAL
            });
        }
    }

    view.lock().phase = CodexAppServerPhase::Stopping;
    drop(stdin);
    drop(stdout);
    drop(stderr);
    if !process_snapshot.lock().reaped {
        if let Some(failure) = ensure_stopped_and_reaped(&mut process) {
            terminal = Some(TerminalIntent::failed(failure));
        }
    }
    // `workspace` remains alive through the preceding reap. It is dropped only
    // after this function constructs the final report.
    finish_report(
        &machine.sink,
        &view,
        process_snapshot,
        terminal.unwrap_or_else(TerminalIntent::clean),
        stderr_tail,
    )
}

fn ensure_stopped_and_reaped(process: &mut ChildProcessGuard) -> Option<WorkerFailure> {
    let mut last_failure = None;
    loop {
        match process.stop_and_reap() {
            Ok(_) => return last_failure,
            Err(failure) => {
                last_failure = Some(failure);
                if process.process_snapshot.lock().reaped {
                    return last_failure;
                }
                // A terminal event or exit report is less important than the
                // ownership invariant. Retry until wait proves the child has
                // been reaped; cancellation/drop therefore cannot detach it.
                thread::sleep(IO_POLL_INTERVAL);
            }
        }
    }
}

fn take_and_configure_pipes(
    process: &mut ChildProcessGuard,
) -> Result<(ChildStdin, ChildStdout, ChildStderr), WorkerFailure> {
    let child = process.child_mut()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| WorkerFailure::io("app-server stdin pipe is unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| WorkerFailure::io("app-server stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| WorkerFailure::io("app-server stderr pipe is unavailable"))?;
    set_nonblocking(stdin.as_raw_fd())?;
    set_nonblocking(stdout.as_raw_fd())?;
    set_nonblocking(stderr.as_raw_fd())?;
    Ok((stdin, stdout, stderr))
}

fn set_nonblocking(fd: i32) -> Result<(), WorkerFailure> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != 0 {
        return Err(WorkerFailure::io(format!(
            "cannot make app-server pipe nonblocking: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn finish_without_child(
    sink: &AgentEventSink,
    view: &Arc<Mutex<CodexAppServerViewSnapshot>>,
    process_snapshot: Arc<Mutex<CodexAppServerProcessExit>>,
    failure: WorkerFailure,
) -> CodexAppServerExitReport {
    finish_report(
        sink,
        view,
        process_snapshot,
        TerminalIntent::failed(failure),
        String::new(),
    )
}

fn finish_cancelled_without_child(
    sink: &AgentEventSink,
    view: &Arc<Mutex<CodexAppServerViewSnapshot>>,
    process_snapshot: Arc<Mutex<CodexAppServerProcessExit>>,
    detail: impl Into<String>,
) -> CodexAppServerExitReport {
    finish_report(
        sink,
        view,
        process_snapshot,
        TerminalIntent::cancelled(detail),
        String::new(),
    )
}

fn finish_report(
    sink: &AgentEventSink,
    view: &Arc<Mutex<CodexAppServerViewSnapshot>>,
    process_snapshot: Arc<Mutex<CodexAppServerProcessExit>>,
    mut intent: TerminalIntent,
    stderr_tail: String,
) -> CodexAppServerExitReport {
    let process_exit = *process_snapshot.lock();
    assert!(
        !process_exit.spawned || (process_exit.reaped && process_exit.containment_verified_empty),
        "terminal Agent events require an empty containment scope and reaped app-server leader"
    );
    let final_event = AgentEventKind::SessionEnded {
        outcome: intent.outcome,
    };
    if let Err(error) = sink.try_emit(final_event, intent.detail.clone()) {
        intent.outcome = AgentSessionOutcome::Failed;
        intent.cause = CodexAppServerExitCause::EventDeliveryFailed;
        intent.critical_event_delivery_failed = true;
        intent.detail = Some(WorkerFailure::event(error).detail);
    }
    sink.close();
    {
        let mut snapshot = view.lock();
        snapshot.pending_approvals.clear();
        snapshot.phase = match intent.outcome {
            AgentSessionOutcome::Clean | AgentSessionOutcome::Cancelled => {
                CodexAppServerPhase::Ended
            }
            AgentSessionOutcome::Failed => CodexAppServerPhase::Failed,
        };
        if intent.outcome == AgentSessionOutcome::Failed {
            snapshot.last_error = intent.detail.clone();
        }
    }
    CodexAppServerExitReport {
        outcome: intent.outcome,
        cause: intent.cause,
        detail: intent
            .detail
            .map(|detail| visible_bounded(&detail, FIELD_MAX_BYTES).0),
        process: process_exit,
        critical_event_delivery_failed: intent.critical_event_delivery_failed,
        stderr_tail,
    }
}

struct ProtocolMachine {
    wire_root: String,
    wire_cwd: String,
    expected_codex_home: String,
    expected_tool_environment: BTreeMap<String, String>,
    credentials: Option<NativeCodexCredentials>,
    sink: AgentEventSink,
    view: Arc<Mutex<CodexAppServerViewSnapshot>>,
    next_request_id: i64,
    pending_requests: HashMap<RpcId, PendingClientRequest>,
    provider_thread_id: Option<String>,
    provider_turn_id: Option<String>,
    local_turn_id: Option<AgentTurnId>,
    turn_started_emitted: bool,
    startup_complete: bool,
    initial_prompt: Option<AgentPrompt>,
    deferred_commands: VecDeque<AgentCommand>,
    approvals: HashMap<ApprovalId, PendingApproval>,
    resolved_approvals: VecDeque<ApprovalId>,
    agent_delta_items: HashSet<String>,
    completed_provider_turn_ids: VecDeque<String>,
    displayed_turn_completed: bool,
}

struct NativeProtocolAuthority {
    expected_codex_home: PathBuf,
    expected_tool_environment: BTreeMap<String, String>,
    credentials: NativeCodexCredentials,
}

impl ProtocolMachine {
    fn new(
        wire_root: String,
        wire_cwd: String,
        authority: NativeProtocolAuthority,
        initial_prompt: Option<AgentPrompt>,
        sink: AgentEventSink,
        view: Arc<Mutex<CodexAppServerViewSnapshot>>,
    ) -> Self {
        Self {
            wire_root,
            wire_cwd,
            expected_codex_home: authority.expected_codex_home.to_string_lossy().into_owned(),
            expected_tool_environment: authority.expected_tool_environment,
            credentials: Some(authority.credentials),
            sink,
            view,
            next_request_id: 1,
            pending_requests: HashMap::new(),
            provider_thread_id: None,
            provider_turn_id: None,
            local_turn_id: None,
            turn_started_emitted: false,
            startup_complete: false,
            initial_prompt,
            deferred_commands: VecDeque::new(),
            approvals: HashMap::new(),
            resolved_approvals: VecDeque::new(),
            agent_delta_items: HashSet::new(),
            completed_provider_turn_ids: VecDeque::new(),
            displayed_turn_completed: false,
        }
    }

    fn startup_handshake_complete(&self) -> bool {
        self.startup_complete
    }

    fn queue_initialize(&mut self, writes: &mut WireWriteQueue) -> Result<(), WorkerFailure> {
        let params = json!({
            "clientInfo": {
                "name": "anvil",
                "title": "Anvil",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                // External in-memory ChatGPT tokens avoid copying the user's
                // refresh token or trust/config state into the private home.
                "experimentalApi": true,
                "requestAttestation": false
            }
        });
        self.queue_request(
            "initialize",
            params,
            PendingClientRequest::Initialize,
            writes,
        )
    }

    fn queue_request(
        &mut self,
        method: &str,
        params: Value,
        pending: PendingClientRequest,
        writes: &mut WireWriteQueue,
    ) -> Result<(), WorkerFailure> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| WorkerFailure::protocol("JSON-RPC request id exhausted"))?;
        let rpc_id = RpcId::Integer(request_id);
        if self.pending_requests.len() >= CODEX_APP_SERVER_WRITE_QUEUE_MAX_MESSAGES {
            return Err(WorkerFailure::protocol(
                "too many outstanding app-server requests",
            ));
        }
        writes.enqueue(&json!({ "id": request_id, "method": method, "params": params }))?;
        self.pending_requests.insert(rpc_id, pending);
        Ok(())
    }

    fn can_interrupt(&self) -> bool {
        self.provider_thread_id.is_some() && self.provider_turn_id.is_some()
    }

    fn is_idle(&self) -> bool {
        self.provider_thread_id.is_some()
            && self.provider_turn_id.is_none()
            && self.local_turn_id.is_none()
            && self.approvals.is_empty()
            && !self
                .pending_requests
                .values()
                .any(|request| matches!(request, PendingClientRequest::TurnStart(_)))
    }

    fn queue_interrupt(&mut self, writes: &mut WireWriteQueue) -> Result<(), WorkerFailure> {
        let thread_id = self
            .provider_thread_id
            .as_deref()
            .ok_or_else(|| WorkerFailure::protocol("cannot interrupt before thread start"))?;
        let turn_id = self
            .provider_turn_id
            .as_deref()
            .ok_or_else(|| WorkerFailure::protocol("cannot interrupt without an active turn"))?;
        self.queue_request(
            "turn/interrupt",
            json!({ "threadId": thread_id, "turnId": turn_id }),
            PendingClientRequest::Interrupt,
            writes,
        )
    }

    fn handle_message(
        &mut self,
        message: Value,
        writes: &mut WireWriteQueue,
        cancellation_requested: bool,
    ) -> Result<Option<TerminalIntent>, WorkerFailure> {
        let object = message
            .as_object()
            .ok_or_else(|| WorkerFailure::protocol("JSONL message is not an object"))?;
        match (
            object.get("method").and_then(Value::as_str),
            object.get("id"),
        ) {
            (Some(method), Some(id)) => self.handle_server_request(method, id, object, writes),
            (Some(method), None) => {
                self.handle_notification(method, object.get("params"), cancellation_requested)
            }
            (None, Some(id)) => self.handle_response(id, object, writes),
            (None, None) => Err(WorkerFailure::protocol(
                "JSON-RPC message has neither method nor id",
            )),
        }
    }

    fn handle_response(
        &mut self,
        id: &Value,
        object: &Map<String, Value>,
        writes: &mut WireWriteQueue,
    ) -> Result<Option<TerminalIntent>, WorkerFailure> {
        let id = RpcId::parse(id)?;
        let Some(pending) = self.pending_requests.remove(&id) else {
            // A late response to a request whose terminal notification already
            // arrived carries no new authority and is safe to ignore.
            return Ok(None);
        };
        if let Some(error) = object.get("error") {
            return Err(WorkerFailure::provider(format!(
                "app-server request {pending:?} failed: {}",
                compact_json(error, FIELD_MAX_BYTES)
            )));
        }
        let result = object
            .get("result")
            .ok_or_else(|| WorkerFailure::protocol("JSON-RPC response has no result"))?;
        match pending {
            PendingClientRequest::Initialize => {
                let result = result.as_object().ok_or_else(|| {
                    WorkerFailure::protocol("initialize response result is not an object")
                })?;
                let user_agent =
                    result
                        .get("userAgent")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            WorkerFailure::protocol(
                                "initialize response has no app-server version identity",
                            )
                        })?;
                if !user_agent.starts_with(SUPPORTED_CODEX_USER_AGENT_PREFIX) {
                    return Err(WorkerFailure::provider(format!(
                        "native Codex requires app-server 0.147.0; installed identity is {}",
                        visible_bounded(user_agent, 128).0
                    )));
                }
                let codex_home =
                    result
                        .get("codexHome")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            WorkerFailure::protocol(
                                "initialize response has no Codex home identity",
                            )
                        })?;
                if codex_home != self.expected_codex_home {
                    return Err(WorkerFailure::protocol(
                        "app-server did not use Anvil's isolated Codex home",
                    ));
                }
                writes.enqueue(&json!({ "method": "initialized" }))?;
                self.queue_request(
                    "config/read",
                    json!({
                        "cwd": self.wire_cwd,
                        "includeLayers": true
                    }),
                    PendingClientRequest::ConfigRead,
                    writes,
                )?;
            }
            PendingClientRequest::ConfigRead => {
                attest_native_codex_config(
                    result,
                    &self.expected_codex_home,
                    &self.expected_tool_environment,
                )?;
                let credentials = self.credentials.take().ok_or_else(|| {
                    WorkerFailure::protocol("native Codex login grant is unavailable")
                })?;
                self.queue_external_login(&credentials, writes)?;
                // The serialized write record owns the only remaining token
                // bytes and zeroes them when written or discarded.
                drop(credentials);
            }
            PendingClientRequest::AuthLogin => {
                if result.get("type").and_then(Value::as_str) != Some("chatgptAuthTokens") {
                    return Err(WorkerFailure::protocol(
                        "native Codex external login returned an unexpected result",
                    ));
                }
                self.queue_thread_start(writes)?;
            }
            PendingClientRequest::ThreadStart => {
                let thread_id = required_bounded_string(result, &["thread", "id"], "thread id")?;
                self.accept_thread_id(thread_id)?;
                if let Some(prompt) = self.initial_prompt.take() {
                    self.queue_turn(prompt, false, writes)?;
                } else {
                    self.startup_complete = true;
                    self.view.lock().phase = CodexAppServerPhase::Ready;
                }
                if let Some(intent) = self.flush_deferred(writes)? {
                    return Ok(Some(intent));
                }
            }
            PendingClientRequest::TurnStart(local_turn_id) => {
                let turn_id = required_bounded_string(result, &["turn", "id"], "turn id")?;
                self.accept_turn_started(local_turn_id, turn_id)?;
            }
            PendingClientRequest::Steer | PendingClientRequest::Interrupt => {
                if !result.is_object() {
                    return Err(WorkerFailure::protocol(
                        "app-server command response result is not an object",
                    ));
                }
            }
        }
        Ok(None)
    }

    fn queue_external_login(
        &mut self,
        credentials: &NativeCodexCredentials,
        writes: &mut WireWriteQueue,
    ) -> Result<(), WorkerFailure> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| WorkerFailure::protocol("JSON-RPC request id exhausted"))?;
        if self.pending_requests.len() >= CODEX_APP_SERVER_WRITE_QUEUE_MAX_MESSAGES {
            return Err(WorkerFailure::protocol(
                "too many outstanding app-server requests",
            ));
        }
        #[derive(Serialize)]
        struct ExternalLoginParams<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            #[serde(rename = "accessToken")]
            access_token: &'a str,
            #[serde(rename = "chatgptAccountId")]
            account_id: &'a str,
        }
        #[derive(Serialize)]
        struct ExternalLoginRequest<'a> {
            id: i64,
            method: &'static str,
            params: ExternalLoginParams<'a>,
        }
        writes.enqueue_serializable(&ExternalLoginRequest {
            id: request_id,
            method: "account/login/start",
            params: ExternalLoginParams {
                kind: "chatgptAuthTokens",
                access_token: credentials.access_token(),
                account_id: credentials.account_id(),
            },
        })?;
        self.pending_requests
            .insert(RpcId::Integer(request_id), PendingClientRequest::AuthLogin);
        Ok(())
    }

    fn queue_thread_start(&mut self, writes: &mut WireWriteQueue) -> Result<(), WorkerFailure> {
        self.view.lock().phase = CodexAppServerPhase::StartingThread;
        self.queue_request(
            "thread/start",
            json!({
                "cwd": self.wire_cwd,
                // Never inherit a ChatGPT account's default/remote execution
                // environment. Native tasks are confined to Anvil's pinned
                // local worktree and containment owner.
                "environments": [],
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                // A read-only thread does not persist project trust or enable
                // project .codex layers. Every turn supplies its explicit
                // descriptor-bound workspaceWrite policy separately.
                "sandbox": "read-only",
                "serviceName": "anvil",
                "ephemeral": true,
                "sessionStartSource": "startup"
            }),
            PendingClientRequest::ThreadStart,
            writes,
        )
    }

    fn handle_server_request(
        &mut self,
        method: &str,
        id: &Value,
        object: &Map<String, Value>,
        writes: &mut WireWriteQueue,
    ) -> Result<Option<TerminalIntent>, WorkerFailure> {
        let rpc_id = RpcId::parse(id)?;
        match method {
            "account/chatgptAuthTokens/refresh" => {
                return Err(WorkerFailure::provider(
                    "native Codex access token expired; run `codex login` again and retry",
                ));
            }
            "item/commandExecution/requestApproval" => self.register_approval(
                rpc_id,
                CodexAppServerApprovalKind::Command,
                object.get("params"),
            )?,
            "item/fileChange/requestApproval" => self.register_approval(
                rpc_id,
                CodexAppServerApprovalKind::FileChange,
                object.get("params"),
            )?,
            _ => {
                writes.enqueue(&json!({
                    "id": rpc_id.value(),
                    "error": { "code": -32601, "message": "method not supported by Anvil" }
                }))?;
            }
        }
        Ok(None)
    }

    fn register_approval(
        &mut self,
        rpc_id: RpcId,
        kind: CodexAppServerApprovalKind,
        params: Option<&Value>,
    ) -> Result<(), WorkerFailure> {
        if self.approvals.len() >= CODEX_APP_SERVER_APPROVAL_CAPACITY {
            return Err(WorkerFailure::protocol(
                "too many simultaneous app-server approval requests",
            ));
        }
        if self
            .approvals
            .values()
            .any(|approval| approval.rpc_id == rpc_id)
        {
            return Err(WorkerFailure::protocol("duplicate approval request id"));
        }
        let params = params
            .and_then(Value::as_object)
            .ok_or_else(|| WorkerFailure::protocol("approval params are not an object"))?;
        self.require_active_correlation(params)?;
        let local_turn_id = self
            .local_turn_id
            .ok_or_else(|| WorkerFailure::protocol("approval has no active local turn"))?;
        let item_id = bounded_optional_string(params.get("itemId"), "approval item id")?
            .ok_or_else(|| WorkerFailure::protocol("approval has no item id"))?;
        if self.approvals.values().any(|approval| {
            approval.kind == CodexAppServerApprovalKind::FileChange && approval.item_id == item_id
        }) {
            return Err(WorkerFailure::protocol(
                "file-change item already has a pending approval snapshot",
            ));
        }
        fail_closed_approval_extensions(kind, params)?;
        let approval_id = ApprovalId::new();
        let command = bounded_optional_string(params.get("command"), "approval command")?;
        let cwd = bounded_optional_string(params.get("cwd"), "approval cwd")?;
        let reason = bounded_optional_string(params.get("reason"), "approval reason")?;
        let file_changes = match kind {
            CodexAppServerApprovalKind::FileChange => {
                let view = self.view.lock();
                let known = view.file_changes.iter().find(|change| {
                    change.item_id == item_id
                        && !change.changes.is_empty()
                        && !change.changes_truncated
                        && change.changes.iter().all(|entry| {
                            !entry.path.is_empty()
                                && entry.path_exact
                                && entry.kind_exact
                                && entry.diff_exact
                                && entry.move_path_exact
                                && entry.shape_exact
                        })
                });
                let Some(known) = known else {
                    return Err(WorkerFailure::protocol(
                        "file approval has no complete, displayable file-change evidence",
                    ));
                };
                known
                    .changes
                    .iter()
                    .map(|change| CodexAppServerApprovalFileChange {
                        path: change.path.clone(),
                        kind: change.kind.clone(),
                        diff: change.diff.clone(),
                        move_path: change.move_path.clone(),
                    })
                    .collect()
            }
            CodexAppServerApprovalKind::Command => Vec::new(),
        };
        let file_paths = file_changes
            .iter()
            .map(|change| change.path.clone())
            .collect();
        let approval_view = CodexAppServerApproval {
            id: approval_id,
            kind,
            item_id: item_id.clone(),
            command,
            cwd,
            reason,
            file_paths,
            file_changes,
        };
        let retained_bytes = self.view.lock().pending_approvals.iter().fold(
            approval_retained_bytes(&approval_view),
            |total, approval| total.saturating_add(approval_retained_bytes(approval)),
        );
        if retained_bytes > CODEX_APP_SERVER_APPROVAL_FROZEN_MAX_BYTES {
            return Err(WorkerFailure::protocol(format!(
                "pending approval snapshots exceed {} retained bytes",
                CODEX_APP_SERVER_APPROVAL_FROZEN_MAX_BYTES
            )));
        }
        self.approvals.insert(
            approval_id,
            PendingApproval {
                rpc_id,
                local_turn_id,
                kind,
                item_id,
            },
        );
        {
            let mut view = self.view.lock();
            view.phase = CodexAppServerPhase::WaitingForApproval;
            view.pending_approvals.push(approval_view.clone());
        }
        let event = match kind {
            CodexAppServerApprovalKind::Command => AgentEventKind::ApprovalRequested {
                turn_id: local_turn_id,
                approval_id,
            },
            CodexAppServerApprovalKind::FileChange => AgentEventKind::PermissionRequested {
                turn_id: local_turn_id,
                approval_id,
            },
        };
        self.emit_critical(event, Some(approval_detail(&approval_view)))?;
        Ok(())
    }

    fn handle_notification(
        &mut self,
        method: &str,
        params: Option<&Value>,
        cancellation_requested: bool,
    ) -> Result<Option<TerminalIntent>, WorkerFailure> {
        let known = matches!(
            method,
            "thread/started"
                | "turn/started"
                | "turn/completed"
                | "item/agentMessage/delta"
                | "item/reasoning/textDelta"
                | "item/reasoning/summaryTextDelta"
                | "turn/plan/updated"
                | "item/plan/delta"
                | "item/started"
                | "item/completed"
                | "item/commandExecution/outputDelta"
                | "item/fileChange/patchUpdated"
                | "thread/tokenUsage/updated"
                | "serverRequest/resolved"
        );
        if !known {
            return Ok(None);
        }
        let params = params
            .and_then(Value::as_object)
            .ok_or_else(|| WorkerFailure::protocol(format!("{method} params are not an object")))?;
        match method {
            "thread/started" => {
                let thread_id =
                    required_bounded_string_from_map(params, &["thread", "id"], "thread id")?;
                self.accept_thread_id(thread_id)?;
            }
            "turn/started" => {
                self.require_thread_correlation(params)?;
                let provider_turn_id =
                    required_bounded_string_from_map(params, &["turn", "id"], "turn id")?;
                let local_turn_id = self.pending_local_turn().ok_or_else(|| {
                    WorkerFailure::protocol("turn/started has no pending local turn")
                })?;
                self.accept_turn_started(local_turn_id, provider_turn_id)?;
            }
            "turn/completed" => {
                self.require_thread_correlation(params)?;
                let completed_turn_id =
                    required_bounded_string_from_map(params, &["turn", "id"], "turn id")?;
                if self.provider_turn_id.as_deref() != Some(completed_turn_id.as_str()) {
                    return Err(WorkerFailure::protocol(
                        "turn/completed does not match the active turn",
                    ));
                }
                let status =
                    required_bounded_string_from_map(params, &["turn", "status"], "turn status")?;
                let local_turn_id = self
                    .local_turn_id
                    .ok_or_else(|| WorkerFailure::protocol("turn/completed has no local turn"))?;
                return match status.as_str() {
                    "completed" => {
                        if !self.approvals.is_empty() {
                            return Err(WorkerFailure::protocol(
                                "turn/completed arrived with pending approval authority",
                            ));
                        }
                        self.emit_critical(
                            AgentEventKind::TurnCompleted {
                                turn_id: local_turn_id,
                            },
                            None,
                        )?;
                        self.complete_turn(completed_turn_id);
                        Ok(None)
                    }
                    "interrupted" if cancellation_requested => {
                        self.clear_terminal_turn();
                        Ok(Some(TerminalIntent::cancelled(
                            "native Codex turn was interrupted",
                        )))
                    }
                    "interrupted" => {
                        self.clear_terminal_turn();
                        Ok(Some(TerminalIntent::failed(WorkerFailure::provider(
                            "Codex turn was interrupted without an Anvil cancellation",
                        ))))
                    }
                    "failed" => {
                        self.clear_terminal_turn();
                        let detail = params
                            .get("turn")
                            .and_then(|turn| turn.get("error"))
                            .map(|error| compact_json(error, FIELD_MAX_BYTES))
                            .unwrap_or_else(|| "Codex turn failed".into());
                        Ok(Some(TerminalIntent::failed(WorkerFailure::provider(
                            detail,
                        ))))
                    }
                    other => Err(WorkerFailure::protocol(format!(
                        "unexpected terminal turn status {other}"
                    ))),
                };
            }
            "item/agentMessage/delta" => {
                self.require_active_correlation(params)?;
                let item_id = required_bounded_string_from_map(params, &["itemId"], "item id")?;
                let delta = required_string_from_map(params, &["delta"], "agent delta")?;
                if !self.agent_delta_items.contains(&item_id)
                    && self.agent_delta_items.len() >= CODEX_APP_SERVER_COMMAND_VIEW_CAPACITY
                {
                    return Err(WorkerFailure::protocol(
                        "too many simultaneous agent message streams",
                    ));
                }
                self.agent_delta_items.insert(item_id);
                self.append_agent_text(&delta);
                self.emit_delta(AgentEventKind::TextDelta, &delta)?;
            }
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                self.require_active_correlation(params)?;
                let delta = required_string_from_map(params, &["delta"], "reasoning delta")?;
                self.emit_delta(AgentEventKind::ReasoningDelta, &delta)?;
            }
            "turn/plan/updated" | "item/plan/delta" => {
                self.require_active_correlation(params)?;
                self.emit_update(
                    AgentEventKind::PlanUpdated,
                    Some(compact_json(
                        &Value::Object(params.clone()),
                        FIELD_MAX_BYTES,
                    )),
                )?;
            }
            "item/started" => self.handle_item(params, false)?,
            "item/completed" => self.handle_item(params, true)?,
            "item/commandExecution/outputDelta" => {
                self.require_active_correlation(params)?;
                let item_id = required_bounded_string_from_map(params, &["itemId"], "item id")?;
                let delta = required_string_from_map(params, &["delta"], "command output")?;
                self.append_command_output(&item_id, &delta);
                self.emit_delta(AgentEventKind::CommandOutput, &delta)?;
            }
            "item/fileChange/patchUpdated" => {
                self.require_active_correlation(params)?;
                let item_id = required_bounded_string_from_map(params, &["itemId"], "item id")?;
                self.reject_pending_file_change_mutation(&item_id)?;
                let changes = params
                    .get("changes")
                    .and_then(Value::as_array)
                    .ok_or_else(|| WorkerFailure::protocol("patch update has no changes"))?;
                self.update_file_view(&item_id, "inProgress", changes);
                self.emit_update(
                    AgentEventKind::DiffUpdated,
                    Some(format!("{} file change(s)", changes.len())),
                )?;
            }
            "thread/tokenUsage/updated" => {
                self.require_active_correlation(params)?;
                self.emit_update(AgentEventKind::UsageUpdated, None)?;
            }
            "serverRequest/resolved" => {
                self.require_thread_correlation(params)?;
                let request_id = RpcId::parse(params.get("requestId").ok_or_else(|| {
                    WorkerFailure::protocol("resolved request has no requestId")
                })?)?;
                let removed: Vec<_> = self
                    .approvals
                    .iter()
                    .filter_map(|(id, pending)| (pending.rpc_id == request_id).then_some(*id))
                    .collect();
                for id in &removed {
                    self.approvals.remove(id);
                    self.remember_settled_approval(*id);
                }
                if !removed.is_empty() {
                    let mut view = self.view.lock();
                    view.pending_approvals
                        .retain(|approval| !removed.contains(&approval.id));
                    if self.approvals.is_empty() {
                        view.phase = CodexAppServerPhase::Running;
                    }
                }
                if !removed.is_empty() && self.approvals.is_empty() {
                    if let Some(turn_id) = self.local_turn_id {
                        self.emit_critical(AgentEventKind::WorkResumed { turn_id }, None)?;
                    }
                }
            }
            // Stable notifications not needed for Anvil's current view are
            // ignored for forward/backward compatibility.
            _ => {}
        }
        Ok(None)
    }

    fn handle_command(
        &mut self,
        command: AgentCommand,
        writes: &mut WireWriteQueue,
    ) -> Result<Option<TerminalIntent>, WorkerFailure> {
        if self.provider_thread_id.is_none() {
            if self.deferred_commands.len() >= CODEX_APP_SERVER_COMMAND_CAPACITY {
                return Err(WorkerFailure::protocol(
                    "pre-initialize Agent command queue is full",
                ));
            }
            self.deferred_commands.push_back(command);
            return Ok(None);
        }
        match command {
            AgentCommand::Prompt(prompt) => {
                self.queue_turn(prompt, true, writes)?;
                Ok(None)
            }
            AgentCommand::FinishSession => {
                if !self.is_idle() {
                    return Err(WorkerFailure::protocol(
                        "finish session command requires an idle provider thread",
                    ));
                }
                Ok(Some(TerminalIntent::clean()))
            }
            AgentCommand::Steer { turn_id, text } => {
                if self.local_turn_id != Some(turn_id) {
                    return Err(WorkerFailure::protocol(
                        "steer command does not match the active local turn",
                    ));
                }
                let thread_id = self
                    .provider_thread_id
                    .as_deref()
                    .ok_or_else(|| WorkerFailure::protocol("steer has no active thread"))?;
                let provider_turn_id = self
                    .provider_turn_id
                    .as_deref()
                    .ok_or_else(|| WorkerFailure::protocol("steer has no active provider turn"))?;
                self.queue_request(
                    "turn/steer",
                    json!({
                        "threadId": thread_id,
                        "expectedTurnId": provider_turn_id,
                        "input": [{ "type": "text", "text": text, "text_elements": [] }]
                    }),
                    PendingClientRequest::Steer,
                    writes,
                )?;
                Ok(None)
            }
            AgentCommand::DecideApproval { id, decision } => {
                if self.resolved_approvals.contains(&id) {
                    return Ok(None);
                }
                let Some(pending) = self.approvals.get(&id).cloned() else {
                    // ApprovalId is generated locally and validated at the
                    // command boundary. Unknown values therefore represent a
                    // stale UI frame and are safe idempotent no-ops.
                    return Ok(None);
                };
                if matches!(&decision, ApprovalDecision::Approve) {
                    return Err(WorkerFailure::protocol(
                        "native approval is disabled because accepted actions cannot yet be bound to Anvil's workspace capability",
                    ));
                }
                self.approvals.remove(&id);
                let (wire_decision, denial_reason) = match decision {
                    ApprovalDecision::Approve => ("accept", None),
                    ApprovalDecision::Deny { reason } => ("decline", reason),
                };
                // Deliberately use only the stable one-shot decisions. Anvil
                // never emits acceptForSession, cancel, or policy amendments.
                writes.enqueue(&json!({
                    "id": pending.rpc_id.value(),
                    "result": { "decision": wire_decision }
                }))?;
                self.remember_settled_approval(id);
                {
                    let mut view = self.view.lock();
                    view.pending_approvals.retain(|approval| approval.id != id);
                    if self.approvals.is_empty() {
                        view.phase = CodexAppServerPhase::Running;
                    }
                    if let Some(reason) = denial_reason {
                        view.last_error = Some(visible_bounded(&reason, FIELD_MAX_BYTES).0);
                    }
                }
                if self.approvals.is_empty() {
                    self.emit_critical(
                        AgentEventKind::WorkResumed {
                            turn_id: pending.local_turn_id,
                        },
                        None,
                    )?;
                }
                Ok(None)
            }
        }
    }

    fn flush_deferred(
        &mut self,
        writes: &mut WireWriteQueue,
    ) -> Result<Option<TerminalIntent>, WorkerFailure> {
        while let Some(command) = self.deferred_commands.pop_front() {
            if let Some(intent) = self.handle_command(command, writes)? {
                return Ok(Some(intent));
            }
        }
        Ok(None)
    }

    fn remember_settled_approval(&mut self, id: ApprovalId) {
        if !self.resolved_approvals.contains(&id) {
            self.resolved_approvals.push_back(id);
        }
        if self.resolved_approvals.len() > RESOLVED_APPROVAL_TOMBSTONE_CAPACITY {
            self.resolved_approvals.pop_front();
        }
    }

    fn queue_turn(
        &mut self,
        prompt: AgentPrompt,
        retain_feedback: bool,
        writes: &mut WireWriteQueue,
    ) -> Result<(), WorkerFailure> {
        if !self.is_idle() {
            return Err(WorkerFailure::protocol(
                "Codex app-server session is not idle",
            ));
        }
        if self.completed_provider_turn_ids.len() >= CODEX_APP_SERVER_LIVE_TURN_MAX {
            return Err(WorkerFailure::provider(format!(
                "native Codex session reached its {}-turn limit; finish the session before validation",
                CODEX_APP_SERVER_LIVE_TURN_MAX
            )));
        }
        let thread_id = self
            .provider_thread_id
            .clone()
            .ok_or_else(|| WorkerFailure::protocol("cannot start turn before thread start"))?;
        let local_turn_id = prompt.turn_id;
        let follow_up_feedback = retain_feedback.then(|| prompt.text.clone());
        self.queue_request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": prompt.text, "text_elements": [] }],
                "cwd": self.wire_cwd,
                "environments": [],
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "sandboxPolicy": {
                    "type": "workspaceWrite",
                    "writableRoots": [self.wire_root],
                    "networkAccess": false,
                    "excludeSlashTmp": true,
                    "excludeTmpdirEnvVar": true
                }
            }),
            PendingClientRequest::TurnStart(local_turn_id),
            writes,
        )?;
        self.local_turn_id = Some(local_turn_id);
        self.turn_started_emitted = false;
        self.agent_delta_items.clear();
        self.resolved_approvals.clear();
        // Commit presentation state only after the turn/start record and its
        // request correlation have both been queued successfully. Archive and
        // flat-view replacement share one critical section, so snapshots can
        // never observe the same turn in both projections. A failed queue
        // leaves the completed flat view and history untouched.
        let view_handle = Arc::clone(&self.view);
        let mut view = view_handle.lock();
        self.archive_displayed_completed_turn(&mut view);
        self.displayed_turn_completed = false;
        view.phase = CodexAppServerPhase::StartingTurn;
        view.provider_turn_id = None;
        view.displayed_turn_id = Some(local_turn_id);
        view.displayed_turn_ordinal = Some(self.completed_provider_turn_ids.len() + 1);
        view.displayed_follow_up_feedback = follow_up_feedback;
        view.agent_text.clear();
        view.agent_text_truncated = false;
        view.commands.clear();
        view.file_changes.clear();
        view.pending_approvals.clear();
        view.last_error = None;
        view.dropped_updates = 0;
        Ok(())
    }

    fn archive_displayed_completed_turn(&mut self, view: &mut CodexAppServerViewSnapshot) {
        if !self.displayed_turn_completed {
            return;
        }
        let (Some(local_turn_id), Some(ordinal)) =
            (view.displayed_turn_id, view.displayed_turn_ordinal)
        else {
            debug_assert!(false, "completed displayed turn has no local identity");
            return;
        };
        let commands = view
            .commands
            .iter()
            .map(|command| CodexAppServerTurnCommandSummary {
                command: command.command.clone(),
                status: command.status.clone(),
                output_omitted: command.output_truncated || !command.output.is_empty(),
            })
            .collect();
        let file_changes = view
            .file_changes
            .iter()
            .map(|file| {
                let first = file.changes.first();
                CodexAppServerTurnFileSummary {
                    status: file.status.clone(),
                    path: first.map(|change| change.path.clone()),
                    change_count: file.changes.len(),
                    changes_truncated: file.changes_truncated || file.changes.len() > 1,
                    path_truncated: first.is_some_and(|change| !change.path_exact),
                }
            })
            .collect();
        let archived = CodexAppServerTurnHistory {
            ordinal,
            local_turn_id,
            follow_up_feedback: view.displayed_follow_up_feedback.take(),
            agent_text: view.agent_text.clone(),
            agent_text_truncated: view.agent_text_truncated,
            commands,
            file_changes,
            dropped_updates: view.dropped_updates,
        };
        let mut history: VecDeque<_> = view.turn_history.iter().cloned().collect();
        history.push_back(archived);
        while history.len() > CODEX_APP_SERVER_TURN_HISTORY_CAPACITY
            || turn_history_retained_bytes(&history) > CODEX_APP_SERVER_TURN_HISTORY_MAX_BYTES
        {
            history.pop_front();
            view.dropped_turns = view.dropped_turns.saturating_add(1);
        }
        view.turn_history = Arc::from(history.into_iter().collect::<Vec<_>>());
        self.displayed_turn_completed = false;
    }

    fn complete_turn(&mut self, provider_turn_id: String) {
        self.remember_completed_turn(provider_turn_id);
        self.displayed_turn_completed = true;
        self.provider_turn_id = None;
        self.local_turn_id = None;
        self.turn_started_emitted = false;
        self.pending_requests.retain(|_, request| {
            !matches!(
                request,
                PendingClientRequest::TurnStart(_)
                    | PendingClientRequest::Steer
                    | PendingClientRequest::Interrupt
            )
        });
        self.approvals.clear();
        self.resolved_approvals.clear();
        self.agent_delta_items.clear();
        let mut view = self.view.lock();
        view.provider_turn_id = None;
        view.pending_approvals.clear();
        view.completed_turns = self.completed_provider_turn_ids.len();
        view.phase = CodexAppServerPhase::Ready;
    }

    fn clear_terminal_turn(&mut self) {
        self.provider_turn_id = None;
        self.view.lock().provider_turn_id = None;
    }

    fn remember_completed_turn(&mut self, provider_turn_id: String) {
        if !self.completed_provider_turn_ids.contains(&provider_turn_id) {
            self.completed_provider_turn_ids.push_back(provider_turn_id);
        }
        debug_assert!(self.completed_provider_turn_ids.len() <= CODEX_APP_SERVER_LIVE_TURN_MAX);
    }

    fn accept_thread_id(&mut self, thread_id: String) -> Result<(), WorkerFailure> {
        if let Some(current) = &self.provider_thread_id {
            if current != &thread_id {
                return Err(WorkerFailure::protocol(
                    "thread identity changed during native session",
                ));
            }
            return Ok(());
        }
        let provider_session_id = ProviderSessionId::new(AgentProvider::Codex, thread_id.clone())
            .map_err(|error| WorkerFailure::protocol(error.to_string()))?;
        self.provider_thread_id = Some(thread_id.clone());
        self.view.lock().provider_thread_id = Some(thread_id);
        self.emit_critical(
            AgentEventKind::SessionStarted {
                provider_session_id: Some(provider_session_id),
                resumed: false,
            },
            None,
        )
    }

    fn pending_local_turn(&self) -> Option<AgentTurnId> {
        self.local_turn_id.or_else(|| {
            self.pending_requests
                .values()
                .find_map(|pending| match pending {
                    PendingClientRequest::TurnStart(turn_id) => Some(*turn_id),
                    _ => None,
                })
        })
    }

    fn accept_turn_started(
        &mut self,
        local_turn_id: AgentTurnId,
        provider_turn_id: String,
    ) -> Result<(), WorkerFailure> {
        if self.completed_provider_turn_ids.contains(&provider_turn_id) {
            return Err(WorkerFailure::protocol(
                "provider reused an already completed turn identity",
            ));
        }
        if self.local_turn_id != Some(local_turn_id) {
            return Err(WorkerFailure::protocol(
                "provider turn does not match pending local turn",
            ));
        }
        if let Some(current) = &self.provider_turn_id {
            if current != &provider_turn_id {
                return Err(WorkerFailure::protocol(
                    "provider turn identity changed during active turn",
                ));
            }
        } else {
            self.provider_turn_id = Some(provider_turn_id.clone());
            let mut view = self.view.lock();
            view.provider_turn_id = Some(provider_turn_id);
            view.phase = CodexAppServerPhase::Running;
        }
        if !self.turn_started_emitted {
            self.emit_critical(
                AgentEventKind::TurnStarted {
                    turn_id: local_turn_id,
                },
                None,
            )?;
            self.turn_started_emitted = true;
        }
        self.startup_complete = true;
        Ok(())
    }

    fn require_thread_correlation(&self, params: &Map<String, Value>) -> Result<(), WorkerFailure> {
        let received = required_bounded_string_from_map(params, &["threadId"], "thread id")?;
        if self.provider_thread_id.as_deref() == Some(received.as_str()) {
            Ok(())
        } else {
            Err(WorkerFailure::protocol(
                "notification does not match the active thread",
            ))
        }
    }

    fn require_active_correlation(&self, params: &Map<String, Value>) -> Result<(), WorkerFailure> {
        self.require_thread_correlation(params)?;
        let received = required_bounded_string_from_map(params, &["turnId"], "turn id")?;
        if self.provider_turn_id.as_deref() == Some(received.as_str()) {
            Ok(())
        } else {
            Err(WorkerFailure::protocol(
                "notification does not match the active turn",
            ))
        }
    }

    fn handle_item(
        &mut self,
        params: &Map<String, Value>,
        completed: bool,
    ) -> Result<(), WorkerFailure> {
        self.require_active_correlation(params)?;
        let item = params
            .get("item")
            .and_then(Value::as_object)
            .ok_or_else(|| WorkerFailure::protocol("item notification has no item object"))?;
        let item_type = required_bounded_string_from_map(item, &["type"], "item type")?;
        let item_id = required_bounded_string_from_map(item, &["id"], "item id")?;
        match item_type.as_str() {
            "agentMessage" if completed => {
                if !self.agent_delta_items.remove(&item_id) {
                    let text = required_string_from_map(item, &["text"], "agent message")?;
                    self.append_agent_text(&text);
                    self.emit_delta(AgentEventKind::TextDelta, &text)?;
                }
            }
            "reasoning" => {
                if completed {
                    self.emit_update(AgentEventKind::ReasoningDelta, None)?;
                }
            }
            "plan" => self.emit_update(AgentEventKind::PlanUpdated, None)?,
            "commandExecution" => {
                self.update_command_view(&item_id, item);
                let command = bounded_optional_string(item.get("command"), "command")?
                    .unwrap_or_else(|| "command".into());
                self.emit_update(
                    if completed {
                        AgentEventKind::CommandFinished
                    } else {
                        AgentEventKind::CommandStarted
                    },
                    Some(command),
                )?;
            }
            "fileChange" => {
                self.reject_pending_file_change_mutation(&item_id)?;
                let status = bounded_optional_string(item.get("status"), "file status")?
                    .unwrap_or_else(|| if completed { "completed" } else { "inProgress" }.into());
                let changes = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .ok_or_else(|| WorkerFailure::protocol("file item has no changes"))?;
                self.update_file_view(&item_id, &status, changes);
                self.emit_update(
                    if completed {
                        AgentEventKind::FileChanged
                    } else {
                        AgentEventKind::DiffUpdated
                    },
                    Some(format!("{} file change(s)", changes.len())),
                )?;
            }
            "mcpToolCall" | "dynamicToolCall" => self.emit_update(
                if completed {
                    AgentEventKind::ToolFinished
                } else {
                    AgentEventKind::ToolStarted
                },
                Some(item_type),
            )?,
            _ => {}
        }
        Ok(())
    }

    fn reject_pending_file_change_mutation(&self, item_id: &str) -> Result<(), WorkerFailure> {
        if self.approvals.values().any(|approval| {
            approval.kind == CodexAppServerApprovalKind::FileChange && approval.item_id == item_id
        }) {
            Err(WorkerFailure::protocol(
                "file-change item mutated while its approval snapshot was pending",
            ))
        } else {
            Ok(())
        }
    }

    fn update_command_view(&self, item_id: &str, item: &Map<String, Value>) {
        let command = item
            .get("command")
            .and_then(Value::as_str)
            .map(|value| visible_bounded(value, FIELD_MAX_BYTES).0)
            .unwrap_or_default();
        let cwd = item
            .get("cwd")
            .and_then(Value::as_str)
            .map(|value| visible_bounded(value, FIELD_MAX_BYTES).0)
            .unwrap_or_default();
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .map(|value| visible_bounded(value, 64).0)
            .unwrap_or_default();
        let output = item
            .get("aggregatedOutput")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (output, output_truncated) = visible_bounded(output, COMMAND_OUTPUT_MAX_BYTES);
        let mut view = self.view.lock();
        if let Some(existing) = view
            .commands
            .iter_mut()
            .find(|entry| entry.item_id == item_id)
        {
            existing.command = command;
            existing.cwd = cwd;
            existing.status = status;
            if !output.is_empty() {
                existing.output = output;
                existing.output_truncated |= output_truncated;
            }
            return;
        }
        if view.commands.len() == CODEX_APP_SERVER_COMMAND_VIEW_CAPACITY {
            view.commands.remove(0);
            view.dropped_updates = view.dropped_updates.saturating_add(1);
        }
        view.commands.push(CodexAppServerCommandView {
            item_id: visible_bounded(item_id, FIELD_MAX_BYTES).0,
            command,
            cwd,
            status,
            output,
            output_truncated,
        });
    }

    fn append_command_output(&self, item_id: &str, delta: &str) {
        let mut view = self.view.lock();
        let Some(command) = view
            .commands
            .iter_mut()
            .find(|entry| entry.item_id == item_id)
        else {
            view.dropped_updates = view.dropped_updates.saturating_add(1);
            return;
        };
        append_visible_bounded(
            &mut command.output,
            &mut command.output_truncated,
            delta,
            COMMAND_OUTPUT_MAX_BYTES,
        );
    }

    fn update_file_view(&self, item_id: &str, status: &str, changes: &[Value]) {
        let mut parsed = Vec::with_capacity(changes.len().min(FILE_CHANGES_PER_ITEM));
        let mut changes_truncated = changes.len() > FILE_CHANGES_PER_ITEM;
        for change in changes.iter().take(FILE_CHANGES_PER_ITEM) {
            let Some(change) = change.as_object() else {
                changes_truncated = true;
                continue;
            };
            let path = change
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let kind_value = change.get("kind");
            let kind = kind_value
                .and_then(|kind| kind.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let move_path_value = kind_value.and_then(|kind| kind.get("move_path"));
            let move_path = move_path_value.and_then(Value::as_str).unwrap_or_default();
            let raw_diff = change
                .get("diff")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (visible_diff, diff_truncated) = visible_bounded(raw_diff, FILE_DIFF_MAX_BYTES);
            let (visible_path, path_changed) = visible_bounded(path, FIELD_MAX_BYTES);
            let path_exact =
                !path_changed && path.len() <= FIELD_MAX_BYTES && visible_path.as_str() == path;
            let (visible_kind, kind_changed) = visible_bounded(kind, 64);
            let kind_object = kind_value.and_then(Value::as_object);
            let kind_shape_exact = kind_object.is_some_and(|object| {
                let allowed = if kind == "update" { 2 } else { 1 };
                object.len() <= allowed
                    && object.keys().all(|key| key == "type" || key == "move_path")
                    && (kind == "add" || kind == "delete" || kind == "update")
                    && (kind == "update" || !object.contains_key("move_path"))
                    && (!object.contains_key("move_path")
                        || move_path_value
                            .is_some_and(|value| value.is_null() || value.is_string()))
            });
            let kind_exact = !kind_changed && visible_kind == kind && kind_shape_exact;
            let diff_exact = !diff_truncated
                && raw_diff.len() <= FILE_DIFF_MAX_BYTES
                && exact_display_string(raw_diff, true);
            let diff = if diff_exact {
                raw_diff.to_string()
            } else {
                visible_diff
            };
            let (visible_move_path, move_path_changed) =
                visible_bounded(move_path, FIELD_MAX_BYTES);
            let move_path_exact = move_path_value.is_none_or(Value::is_null)
                || (!move_path_changed
                    && !move_path.is_empty()
                    && visible_move_path.as_str() == move_path);
            let shape_exact = change.len() == 3
                && change.contains_key("path")
                && change.contains_key("kind")
                && change.contains_key("diff")
                && change.get("path").is_some_and(Value::is_string)
                && change.get("diff").is_some_and(Value::is_string);
            parsed.push(CodexAppServerFileChange {
                path: visible_path,
                path_exact,
                kind: visible_kind,
                kind_exact,
                diff,
                diff_truncated,
                diff_exact,
                move_path: move_path_value
                    .filter(|value| !value.is_null())
                    .map(|_| visible_move_path),
                move_path_exact,
                shape_exact,
            });
        }
        let mut view = self.view.lock();
        if let Some(existing) = view
            .file_changes
            .iter_mut()
            .find(|entry| entry.item_id == item_id)
        {
            existing.status = visible_bounded(status, 64).0;
            existing.changes = parsed;
            existing.changes_truncated = changes_truncated;
            return;
        }
        if view.file_changes.len() == CODEX_APP_SERVER_FILE_VIEW_CAPACITY {
            view.file_changes.remove(0);
            view.dropped_updates = view.dropped_updates.saturating_add(1);
        }
        view.file_changes.push(CodexAppServerFileChangeView {
            item_id: visible_bounded(item_id, FIELD_MAX_BYTES).0,
            status: visible_bounded(status, 64).0,
            changes: parsed,
            changes_truncated,
        });
    }

    fn append_agent_text(&self, delta: &str) {
        let mut view = self.view.lock();
        let mut text = std::mem::take(&mut view.agent_text);
        let mut truncated = view.agent_text_truncated;
        append_visible_bounded(
            &mut text,
            &mut truncated,
            delta,
            CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES,
        );
        view.agent_text = text;
        view.agent_text_truncated = truncated;
    }

    fn emit_critical(
        &self,
        kind: AgentEventKind,
        detail: Option<String>,
    ) -> Result<(), WorkerFailure> {
        self.sink
            .try_emit(kind, detail)
            .map(|_| ())
            .map_err(WorkerFailure::event)
    }

    fn emit_update(
        &self,
        kind: AgentEventKind,
        detail: Option<String>,
    ) -> Result<(), WorkerFailure> {
        match self.sink.try_emit(kind, detail) {
            Ok(_) => Ok(()),
            Err(error) if error.is_backpressure() => {
                let mut view = self.view.lock();
                view.dropped_updates = view.dropped_updates.saturating_add(1);
                Ok(())
            }
            Err(error) => Err(WorkerFailure::event(error)),
        }
    }

    fn emit_delta(&self, kind: AgentEventKind, delta: &str) -> Result<(), WorkerFailure> {
        let (detail, _) = visible_bounded(delta, FIELD_MAX_BYTES);
        self.emit_update(kind, (!detail.is_empty()).then_some(detail))
    }
}

fn required_bounded_string(
    root: &Value,
    path: &[&str],
    label: &str,
) -> Result<String, WorkerFailure> {
    let object = root
        .as_object()
        .ok_or_else(|| WorkerFailure::protocol(format!("{label} parent is not an object")))?;
    required_bounded_string_from_map(object, path, label)
}

fn required_bounded_string_from_map(
    root: &Map<String, Value>,
    path: &[&str],
    label: &str,
) -> Result<String, WorkerFailure> {
    let value = value_at_map_path(root, path)
        .and_then(Value::as_str)
        .ok_or_else(|| WorkerFailure::protocol(format!("missing {label}")))?;
    if value.is_empty() || value.len() > FIELD_MAX_BYTES || value.chars().any(char::is_control) {
        return Err(WorkerFailure::protocol(format!("invalid {label}")));
    }
    Ok(value.to_string())
}

fn required_string_from_map(
    root: &Map<String, Value>,
    path: &[&str],
    label: &str,
) -> Result<String, WorkerFailure> {
    let value = value_at_map_path(root, path)
        .and_then(Value::as_str)
        .ok_or_else(|| WorkerFailure::protocol(format!("missing {label}")))?;
    if value.len() > CODEX_APP_SERVER_JSONL_MAX_BYTES {
        return Err(WorkerFailure::protocol(format!("oversized {label}")));
    }
    Ok(value.to_string())
}

fn value_at_map_path<'a>(root: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut value = root.get(*first)?;
    for component in rest {
        value = value.get(*component)?;
    }
    Some(value)
}

fn bounded_optional_string(
    value: Option<&Value>,
    label: &str,
) -> Result<Option<String>, WorkerFailure> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            // Approval fields grant authority. Never display a harmless
            // prefix while responding to a request whose dangerous suffix was
            // hidden from the user.
            if value.len() > FIELD_MAX_BYTES {
                return Err(WorkerFailure::protocol(format!("oversized {label}")));
            }
            let (visible, changed) = visible_bounded(value, FIELD_MAX_BYTES);
            if changed || visible != *value {
                return Err(WorkerFailure::protocol(format!(
                    "{label} cannot be displayed exactly"
                )));
            }
            Ok(Some(visible))
        }
        Some(_) => Err(WorkerFailure::protocol(format!("invalid {label}"))),
    }
}

fn exact_display_string(value: &str, allow_line_feed: bool) -> bool {
    value.chars().all(|character| {
        (allow_line_feed && character == '\n')
            || (!character.is_control()
                && !jterm_core::review_input::is_visual_spoofing_character(character))
    })
}

fn approval_retained_bytes(approval: &CodexAppServerApproval) -> usize {
    let optional = |value: &Option<String>| value.as_ref().map_or(0, String::len);
    let mut total = approval
        .item_id
        .len()
        .saturating_add(optional(&approval.command))
        .saturating_add(optional(&approval.cwd))
        .saturating_add(optional(&approval.reason));
    for path in &approval.file_paths {
        total = total.saturating_add(path.len());
    }
    for change in &approval.file_changes {
        total = total
            .saturating_add(change.path.len())
            .saturating_add(change.kind.len())
            .saturating_add(change.diff.len())
            .saturating_add(change.move_path.as_ref().map_or(0, String::len));
    }
    total
}

fn turn_history_retained_bytes(history: &VecDeque<CodexAppServerTurnHistory>) -> usize {
    history.iter().fold(0usize, |total, turn| {
        total.saturating_add(turn_history_entry_retained_bytes(turn))
    })
}

fn turn_history_entry_retained_bytes(turn: &CodexAppServerTurnHistory) -> usize {
    let mut total = std::mem::size_of::<CodexAppServerTurnHistory>()
        .saturating_add(turn.follow_up_feedback.as_ref().map_or(0, String::len))
        .saturating_add(turn.agent_text.len());
    for command in &turn.commands {
        total = total
            .saturating_add(std::mem::size_of::<CodexAppServerTurnCommandSummary>())
            .saturating_add(command.command.len())
            .saturating_add(command.status.len());
    }
    for file in &turn.file_changes {
        total = total
            .saturating_add(std::mem::size_of::<CodexAppServerTurnFileSummary>())
            .saturating_add(file.status.len())
            .saturating_add(file.path.as_ref().map_or(0, String::len));
    }
    total
}

fn approval_detail(approval: &CodexAppServerApproval) -> String {
    match approval.kind {
        CodexAppServerApprovalKind::Command => approval
            .command
            .clone()
            .unwrap_or_else(|| "Codex requests command execution approval".into()),
        CodexAppServerApprovalKind::FileChange => approval
            .reason
            .clone()
            .unwrap_or_else(|| "Codex requests file change approval".into()),
    }
}

fn fail_closed_approval_extensions(
    kind: CodexAppServerApprovalKind,
    params: &Map<String, Value>,
) -> Result<(), WorkerFailure> {
    let allowed: &[&str] = match kind {
        CodexAppServerApprovalKind::Command => &[
            "approvalId",
            "additionalPermissions",
            "availableDecisions",
            "command",
            "commandActions",
            "cwd",
            "environmentId",
            "itemId",
            "networkApprovalContext",
            "proposedExecpolicyAmendment",
            "proposedNetworkPolicyAmendments",
            "reason",
            "startedAtMs",
            "threadId",
            "turnId",
        ],
        CodexAppServerApprovalKind::FileChange => &[
            "grantRoot",
            "itemId",
            "reason",
            "startedAtMs",
            "threadId",
            "turnId",
        ],
    };
    if let Some(unknown) = params.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(WorkerFailure::protocol(format!(
            "approval contains unsupported field {unknown}"
        )));
    }
    // All native approvals are deny-only. Stable authority-bearing fields may
    // therefore be accepted for compatibility without being interpreted or
    // granted; unknown schema extensions still fail closed above.
    Ok(())
}

/// Prove that the provider resolved configuration only from Anvil's private
/// empty user config, an empty system layer, and the exact disable flags on
/// this process. This runs before external login and before `thread/start`, so
/// MCP commands, SessionStart hooks, plugins, apps, and project trust never
/// gain an execution opportunity when the proof fails.
fn attest_native_codex_config(
    result: &Value,
    expected_codex_home: &str,
    expected_tool_environment: &BTreeMap<String, String>,
) -> Result<(), WorkerFailure> {
    let result = result
        .as_object()
        .ok_or_else(|| WorkerFailure::protocol("config/read result is not an object"))?;
    let config = result
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(|| WorkerFailure::protocol("config/read has no effective config"))?;

    for field in ["mcp_servers", "plugins", "marketplaces"] {
        if !config
            .get(field)
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            return Err(WorkerFailure::protocol(format!(
                "native Codex effective config contains {field} authority"
            )));
        }
    }
    for field in [
        "agents",
        "apps",
        "default_permissions",
        "hooks",
        "notify",
        "orchestrator",
        "permissions",
    ] {
        if config.get(field).is_some_and(|value| {
            !value.is_null()
                && !value.as_object().is_some_and(Map::is_empty)
                && !value.as_array().is_some_and(Vec::is_empty)
        }) {
            return Err(WorkerFailure::protocol(format!(
                "native Codex effective config contains {field} authority"
            )));
        }
    }
    let features = config
        .get("features")
        .and_then(Value::as_object)
        .ok_or_else(|| WorkerFailure::protocol("native Codex feature config is unavailable"))?;
    for feature in NATIVE_CODEX_DISABLED_FEATURES {
        if features.get(*feature).and_then(Value::as_bool) != Some(false) {
            return Err(WorkerFailure::protocol(format!(
                "native Codex feature {feature} is not disabled"
            )));
        }
    }
    if let Some((feature, _)) = features
        .iter()
        .find(|(_, value)| value.as_bool() == Some(true))
    {
        return Err(WorkerFailure::protocol(format!(
            "native Codex effective config enables unaudited feature {feature}"
        )));
    }
    if let Some((feature, _)) = features
        .iter()
        .find(|(_, value)| !value.is_boolean() && !value.is_null())
    {
        return Err(WorkerFailure::protocol(format!(
            "native Codex feature {feature} has an unaudited value"
        )));
    }
    if config.get("web_search").and_then(Value::as_str) != Some("disabled") {
        return Err(WorkerFailure::protocol(
            "native Codex hosted web search is not disabled",
        ));
    }
    if config.get("allow_login_shell").and_then(Value::as_bool) != Some(false)
        || !shell_environment_matches(
            config.get("shell_environment_policy"),
            expected_tool_environment,
        )
    {
        return Err(WorkerFailure::protocol(
            "native Codex tool environment policy is not isolated",
        ));
    }

    let layers = result
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| WorkerFailure::protocol("config/read omitted config layers"))?;
    let expected_config = PathBuf::from(expected_codex_home).join("config.toml");
    let expected_config = expected_config.to_string_lossy();
    let mut session_flags_seen = false;
    let mut user_seen = false;
    let mut system_seen = false;
    for layer in layers {
        let layer = layer
            .as_object()
            .ok_or_else(|| WorkerFailure::protocol("config/read layer is not an object"))?;
        let name = layer
            .get("name")
            .and_then(Value::as_object)
            .ok_or_else(|| WorkerFailure::protocol("config/read layer has no identity"))?;
        let kind = name
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| WorkerFailure::protocol("config/read layer identity has no type"))?;
        let layer_config = layer
            .get("config")
            .and_then(Value::as_object)
            .ok_or_else(|| WorkerFailure::protocol("config/read layer has no config object"))?;
        match kind {
            "sessionFlags" if !session_flags_seen => {
                session_flags_seen = true;
                if layer_config.len() != 4
                    || layer_config.get("web_search").and_then(Value::as_str) != Some("disabled")
                    || layer_config
                        .get("allow_login_shell")
                        .and_then(Value::as_bool)
                        != Some(false)
                    || !shell_environment_matches(
                        layer_config.get("shell_environment_policy"),
                        expected_tool_environment,
                    )
                {
                    return Err(WorkerFailure::protocol(
                        "native Codex session config contains unexpected overrides",
                    ));
                }
                let layer_features = layer_config
                    .get("features")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        WorkerFailure::protocol("native Codex session feature layer is malformed")
                    })?;
                if layer_features.len() != NATIVE_CODEX_DISABLED_FEATURES.len()
                    || NATIVE_CODEX_DISABLED_FEATURES.iter().any(|feature| {
                        layer_features.get(*feature).and_then(Value::as_bool) != Some(false)
                    })
                {
                    return Err(WorkerFailure::protocol(
                        "native Codex session feature disables are incomplete",
                    ));
                }
            }
            "user" if !user_seen => {
                user_seen = true;
                if name.get("file").and_then(Value::as_str) != Some(expected_config.as_ref())
                    || name
                        .get("profile")
                        .is_some_and(|profile| !profile.is_null())
                    || !layer_config.is_empty()
                {
                    return Err(WorkerFailure::protocol(
                        "native Codex user config is not Anvil's private empty config",
                    ));
                }
            }
            "system" if !system_seen => {
                system_seen = true;
                if !layer_config.is_empty() {
                    return Err(WorkerFailure::protocol(
                        "native Codex refuses non-empty system configuration",
                    ));
                }
            }
            // Project, enterprise/MDM, legacy-managed, duplicate, and future
            // layers all fail closed. Terminal fallback remains available.
            _ => {
                return Err(WorkerFailure::protocol(format!(
                    "native Codex refuses config layer type {kind}"
                )))
            }
        }
    }
    if !(session_flags_seen && user_seen && system_seen) || layers.len() != 3 {
        return Err(WorkerFailure::protocol(
            "native Codex config layer proof is incomplete",
        ));
    }

    let origins = result
        .get("origins")
        .and_then(Value::as_object)
        .ok_or_else(|| WorkerFailure::protocol("config/read omitted config origins"))?;
    if origins.len() != NATIVE_CODEX_DISABLED_FEATURES.len() + 4 + expected_tool_environment.len() {
        return Err(WorkerFailure::protocol(
            "native Codex effective config has unexpected origins",
        ));
    }
    for feature in NATIVE_CODEX_DISABLED_FEATURES {
        let origin = origins
            .get(&format!("features.{feature}"))
            .and_then(Value::as_object)
            .and_then(|origin| origin.get("name"))
            .and_then(Value::as_object)
            .and_then(|name| name.get("type"))
            .and_then(Value::as_str);
        if origin != Some("sessionFlags") {
            return Err(WorkerFailure::protocol(format!(
                "native Codex feature {feature} did not originate from Anvil's flags"
            )));
        }
    }
    let web_search_origin = origins
        .get("web_search")
        .and_then(Value::as_object)
        .and_then(|origin| origin.get("name"))
        .and_then(Value::as_object)
        .and_then(|name| name.get("type"))
        .and_then(Value::as_str);
    if web_search_origin != Some("sessionFlags") {
        return Err(WorkerFailure::protocol(
            "native Codex web-search policy did not originate from Anvil's flags",
        ));
    }
    for field in [
        "allow_login_shell",
        "shell_environment_policy.inherit",
        "shell_environment_policy.ignore_default_excludes",
    ] {
        if config_origin_type(origins, field) != Some("sessionFlags") {
            return Err(WorkerFailure::protocol(format!(
                "native Codex policy {field} did not originate from Anvil's flags"
            )));
        }
    }
    for name in expected_tool_environment.keys() {
        let field = format!("shell_environment_policy.set.{name}");
        if config_origin_type(origins, &field) != Some("sessionFlags") {
            return Err(WorkerFailure::protocol(format!(
                "native Codex tool environment {name} did not originate from Anvil's flags"
            )));
        }
    }
    Ok(())
}

fn shell_environment_matches(value: Option<&Value>, expected: &BTreeMap<String, String>) -> bool {
    let Some(policy) = value.and_then(Value::as_object) else {
        return false;
    };
    policy.get("inherit").and_then(Value::as_str) == Some("none")
        && policy
            .get("ignore_default_excludes")
            .and_then(Value::as_bool)
            == Some(false)
        && policy
            .get("set")
            .and_then(Value::as_object)
            .is_some_and(|set| {
                set.len() == expected.len()
                    && expected
                        .iter()
                        .all(|(name, value)| set.get(name).and_then(Value::as_str) == Some(value))
            })
        && policy.get("exclude").is_none_or(|value| value.is_null())
        && policy
            .get("include_only")
            .is_none_or(|value| value.is_null())
        && policy.get("filters").is_none_or(|value| value.is_null())
        && policy
            .get("experimental_use_profile")
            .is_none_or(|value| value.is_null())
}

fn config_origin_type<'a>(origins: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    origins
        .get(field)
        .and_then(Value::as_object)
        .and_then(|origin| origin.get("name"))
        .and_then(Value::as_object)
        .and_then(|name| name.get("type"))
        .and_then(Value::as_str)
}

fn compact_json(value: &Value, limit: usize) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "<invalid json>".into());
    visible_bounded(&encoded, limit).0
}

fn visible_bounded(value: &str, limit: usize) -> (String, bool) {
    let mut bounded = String::with_capacity(value.len().min(limit));
    let mut truncated = false;
    for character in value.chars() {
        let visible = if character.is_control()
            || jterm_core::review_input::is_visual_spoofing_character(character)
        {
            '\u{fffd}'
        } else {
            character
        };
        if bounded.len() + visible.len_utf8() > limit {
            truncated = true;
            break;
        }
        bounded.push(visible);
    }
    (bounded, truncated)
}

fn append_visible_bounded(target: &mut String, truncated: &mut bool, delta: &str, limit: usize) {
    if target.len() >= limit {
        *truncated |= !delta.is_empty();
        return;
    }
    let (bounded, clipped) = visible_bounded(delta, limit - target.len());
    target.push_str(&bounded);
    *truncated |= clipped;
}

fn drain_stderr(stderr: &mut ChildStderr, tail: &mut String) {
    let mut chunk = [0_u8; 8192];
    let mut budget = READ_BYTES_PER_TICK;
    while budget > 0 {
        let wanted = chunk.len().min(budget);
        match stderr.read(&mut chunk[..wanted]) {
            Ok(0) => break,
            Ok(read) => {
                budget -= read;
                let decoded = String::from_utf8_lossy(&chunk[..read]);
                let (visible, _) = visible_bounded(&decoded, read.saturating_mul(3));
                tail.push_str(&visible);
                trim_front_to_boundary(tail, STDERR_TAIL_MAX_BYTES);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn trim_front_to_boundary(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut start = value.len() - limit;
    while start < value.len() && !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::driver::{
        agent_event_channel_with_limits, AgentEventQueueLimits, AgentEventReceiver,
    };
    use crate::agent_task::event::next_agent_event_epoch;
    use crate::agent_task::{AgentEventStream, NativeAgentSessionId as SessionId, TaskId};
    use std::io::Cursor;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    fn stream() -> AgentEventStream {
        AgentEventStream::new(
            TaskId::new(),
            SessionId::parse("codex-app-server-test").unwrap(),
            next_agent_event_epoch().unwrap(),
        )
    }

    fn machine(
        prompt: Option<AgentPrompt>,
    ) -> (
        ProtocolMachine,
        WireWriteQueue,
        AgentEventReceiver,
        Arc<Mutex<CodexAppServerViewSnapshot>>,
    ) {
        let (sender, receiver) = agent_event_channel();
        let view = Arc::new(Mutex::new(CodexAppServerViewSnapshot::default()));
        let machine = ProtocolMachine::new(
            "/proc/self/fd/19".into(),
            "/proc/self/fd/19/nested".into(),
            NativeProtocolAuthority {
                expected_codex_home: PathBuf::from("/private/codex"),
                expected_tool_environment: test_tool_environment(),
                credentials: NativeCodexCredentials::new(
                    "test-access-token".into(),
                    "account-1".into(),
                )
                .unwrap(),
            },
            prompt,
            AgentEventSink::new(stream(), sender),
            Arc::clone(&view),
        );
        (machine, WireWriteQueue::default(), receiver, view)
    }

    fn pop_wire(queue: &mut WireWriteQueue) -> Value {
        let pending = queue.pending.pop_front().expect("queued JSON-RPC record");
        queue.pending_bytes = queue
            .pending_bytes
            .saturating_sub(pending.bytes.len().saturating_sub(pending.written));
        serde_json::from_slice(&pending.bytes[..pending.bytes.len() - 1]).unwrap()
    }

    fn test_tool_environment() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("HOME".into(), "/home/test".into()),
            ("PATH".into(), "/usr/bin".into()),
            ("USER".into(), "test".into()),
        ])
    }

    fn attested_config() -> Value {
        let tool_environment = test_tool_environment();
        let tool_environment_json: Map<String, Value> = tool_environment
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        let features: Map<String, Value> = NATIVE_CODEX_DISABLED_FEATURES
            .iter()
            .map(|feature| ((*feature).to_string(), Value::Bool(false)))
            .collect();
        let mut origins: Map<String, Value> = NATIVE_CODEX_DISABLED_FEATURES
            .iter()
            .map(|feature| {
                (
                    format!("features.{feature}"),
                    json!({"name": {"type": "sessionFlags"}, "version": "test"}),
                )
            })
            .collect();
        origins.insert(
            "web_search".into(),
            json!({"name": {"type": "sessionFlags"}, "version": "test"}),
        );
        for field in [
            "allow_login_shell",
            "shell_environment_policy.inherit",
            "shell_environment_policy.ignore_default_excludes",
        ] {
            origins.insert(
                field.into(),
                json!({"name": {"type": "sessionFlags"}, "version": "test"}),
            );
        }
        for name in tool_environment.keys() {
            origins.insert(
                format!("shell_environment_policy.set.{name}"),
                json!({"name": {"type": "sessionFlags"}, "version": "test"}),
            );
        }
        json!({
            "config": {
                "mcp_servers": {}, "plugins": {}, "marketplaces": {},
                "agents": null, "apps": null, "default_permissions": null,
                "hooks": null, "notify": null, "orchestrator": null,
                "permissions": null, "features": features, "web_search": "disabled",
                "allow_login_shell": false,
                "shell_environment_policy": {
                    "inherit": "none", "ignore_default_excludes": false,
                    "set": tool_environment_json
                }
            },
            "origins": origins,
            "layers": [
                {
                    "name": {"type": "sessionFlags"}, "version": "test",
                    "config": {
                        "features": features, "web_search": "disabled",
                        "allow_login_shell": false,
                        "shell_environment_policy": {
                            "inherit": "none", "ignore_default_excludes": false,
                            "set": tool_environment_json
                        }
                    }
                },
                {
                    "name": {"type": "user", "file": "/private/codex/config.toml", "profile": null},
                    "version": "test", "config": {}
                },
                {
                    "name": {"type": "system", "file": "/etc/codex/config.toml"},
                    "version": "test", "config": {}
                }
            ]
        })
    }

    fn active_machine() -> (
        ProtocolMachine,
        WireWriteQueue,
        AgentEventReceiver,
        Arc<Mutex<CodexAppServerViewSnapshot>>,
        AgentTurnId,
    ) {
        let (mut machine, queue, receiver, view) = machine(None);
        let turn_id = AgentTurnId::new();
        machine.provider_thread_id = Some("thread-1".into());
        machine.provider_turn_id = Some("turn-1".into());
        machine.local_turn_id = Some(turn_id);
        machine.turn_started_emitted = true;
        machine.startup_complete = true;
        {
            let mut snapshot = view.lock();
            snapshot.phase = CodexAppServerPhase::Running;
            snapshot.provider_thread_id = Some("thread-1".into());
            snapshot.provider_turn_id = Some("turn-1".into());
            snapshot.displayed_turn_id = Some(turn_id);
            snapshot.displayed_turn_ordinal = Some(1);
        }
        (machine, queue, receiver, view, turn_id)
    }

    fn archive_history_turn(
        machine: &mut ProtocolMachine,
        view: &Arc<Mutex<CodexAppServerViewSnapshot>>,
        ordinal: usize,
        follow_up_feedback: Option<String>,
        agent_text: String,
    ) -> AgentTurnId {
        let local_turn_id = AgentTurnId::new();
        machine.displayed_turn_completed = true;
        let view_handle = Arc::clone(view);
        let mut snapshot = view_handle.lock();
        snapshot.displayed_turn_id = Some(local_turn_id);
        snapshot.displayed_turn_ordinal = Some(ordinal);
        snapshot.displayed_follow_up_feedback = follow_up_feedback;
        snapshot.agent_text = agent_text;
        snapshot.agent_text_truncated = false;
        snapshot.commands.clear();
        snapshot.file_changes.clear();
        snapshot.pending_approvals.clear();
        snapshot.dropped_updates = 0;
        machine.archive_displayed_completed_turn(&mut snapshot);
        local_turn_id
    }

    #[test]
    fn integer_rpc_ids_complete_versioned_handshake_and_preserve_cwd_root_split() {
        let prompt = AgentPrompt::new("fix it");
        let local_turn = prompt.turn_id;
        let (mut machine, mut writes, receiver, _) = machine(Some(prompt));

        machine.queue_initialize(&mut writes).unwrap();
        let initialize = pop_wire(&mut writes);
        assert_eq!(initialize["id"], 1);
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["capabilities"]["experimentalApi"],
            true
        );
        assert_eq!(
            initialize["params"]["capabilities"]["requestAttestation"],
            false
        );

        machine
            .handle_message(
                json!({"id": 1, "result": {"userAgent": "anvil/0.147.0 (test)", "codexHome": "/private/codex"}}),
                &mut writes,
                false,
            )
            .unwrap();
        assert_eq!(pop_wire(&mut writes), json!({"method": "initialized"}));
        let config_read = pop_wire(&mut writes);
        assert_eq!(config_read["id"], 2);
        assert_eq!(config_read["method"], "config/read");
        assert_eq!(config_read["params"]["includeLayers"], true);
        machine
            .handle_message(
                json!({"id": 2, "result": attested_config()}),
                &mut writes,
                false,
            )
            .unwrap();
        let login = pop_wire(&mut writes);
        assert_eq!(login["id"], 3);
        assert_eq!(login["method"], "account/login/start");
        assert_eq!(login["params"]["type"], "chatgptAuthTokens");
        assert_eq!(login["params"]["accessToken"], "test-access-token");
        machine
            .handle_message(
                json!({"id": 3, "result": {"type": "chatgptAuthTokens"}}),
                &mut writes,
                false,
            )
            .unwrap();
        let thread_start = pop_wire(&mut writes);
        assert_eq!(thread_start["id"], 4);
        assert_eq!(thread_start["method"], "thread/start");
        assert_eq!(thread_start["params"]["cwd"], "/proc/self/fd/19/nested");
        assert_eq!(thread_start["params"]["environments"], json!([]));
        assert_eq!(thread_start["params"]["approvalPolicy"], "never");
        assert_eq!(thread_start["params"]["approvalsReviewer"], "user");
        assert_eq!(thread_start["params"]["sandbox"], "read-only");

        machine
            .handle_message(
                json!({"id": 4, "result": {"thread": {"id": "thread-1"}}}),
                &mut writes,
                false,
            )
            .unwrap();
        assert!(machine.pending_requests.values().any(
            |pending| matches!(pending, PendingClientRequest::TurnStart(id) if *id == local_turn)
        ));
        let turn_start = pop_wire(&mut writes);
        assert_eq!(turn_start["id"], 5);
        assert_eq!(turn_start["method"], "turn/start");
        assert_eq!(turn_start["params"]["cwd"], "/proc/self/fd/19/nested");
        assert_eq!(turn_start["params"]["environments"], json!([]));
        assert_eq!(
            turn_start["params"]["sandboxPolicy"],
            json!({
                "type": "workspaceWrite",
                "writableRoots": ["/proc/self/fd/19"],
                "networkAccess": false,
                "excludeSlashTmp": true,
                "excludeTmpdirEnvVar": true
            })
        );
        assert_eq!(turn_start["params"]["approvalPolicy"], "never");
        assert_eq!(turn_start["params"]["approvalsReviewer"], "user");

        let session_event = receiver.try_recv().unwrap();
        assert!(matches!(
            session_event.kind(),
            AgentEventKind::SessionStarted { resumed: false, .. }
        ));
        machine
            .handle_message(
                json!({"id": 5, "result": {"turn": {"id": "turn-1"}}}),
                &mut writes,
                false,
            )
            .unwrap();
        assert!(machine.pending_requests.is_empty());
        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::TurnStarted { turn_id } if *turn_id == local_turn
        ));
        assert!(machine.startup_handshake_complete());
    }

    #[test]
    fn completed_turn_becomes_idle_and_follow_up_reuses_thread_and_authority() {
        let (mut machine, mut writes, receiver, view, first_local_turn) = active_machine();
        machine.append_agent_text("first answer");
        machine.update_command_view(
            "command-1",
            json!({
                "command": "true", "cwd": "/workspace", "status": "completed",
                "aggregatedOutput": "ok"
            })
            .as_object()
            .unwrap(),
        );
        machine.update_file_view(
            "file-1",
            "completed",
            &[json!({
                "path": "src/lib.rs", "kind": {"type": "update"}, "diff": "+done"
            })],
        );
        machine.agent_delta_items.insert("agent-1".into());
        machine.pending_requests.insert(
            RpcId::Integer(90),
            PendingClientRequest::TurnStart(first_local_turn),
        );
        machine
            .pending_requests
            .insert(RpcId::Integer(91), PendingClientRequest::Steer);
        {
            let mut snapshot = view.lock();
            snapshot.last_error = Some("old turn warning".into());
            snapshot.dropped_updates = 7;
        }

        let terminal = machine
            .handle_message(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-1",
                        "turn": {"id": "turn-1", "status": "completed"}
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        assert!(terminal.is_none());
        assert!(machine.is_idle());
        assert!(machine.startup_handshake_complete());
        assert!(machine.pending_requests.is_empty());
        assert!(machine.agent_delta_items.is_empty());
        assert_eq!(
            machine.completed_provider_turn_ids,
            VecDeque::from(["turn-1".to_string()])
        );
        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::TurnCompleted { turn_id } if *turn_id == first_local_turn
        ));
        assert!(receiver.try_recv().is_err());
        {
            let snapshot = view.lock();
            assert_eq!(snapshot.phase, CodexAppServerPhase::Ready);
            assert_eq!(snapshot.provider_thread_id.as_deref(), Some("thread-1"));
            assert!(snapshot.provider_turn_id.is_none());
            assert_eq!(snapshot.displayed_turn_id, Some(first_local_turn));
            assert_eq!(snapshot.displayed_turn_ordinal, Some(1));
            assert!(snapshot.displayed_follow_up_feedback.is_none());
            assert_eq!(snapshot.agent_text, "first answer");
            assert_eq!(snapshot.commands.len(), 1);
            assert_eq!(snapshot.file_changes.len(), 1);
            assert!(snapshot.turn_history.is_empty());
        }

        // A response belonging to the already-completed turn was removed from
        // correlation state and cannot revive it.
        assert!(machine
            .handle_message(
                json!({"id": 90, "result": {"turn": {"id": "turn-1"}}}),
                &mut writes,
                false,
            )
            .unwrap()
            .is_none());

        let follow_up = AgentPrompt::new("continue with the next fix");
        let follow_up_local_turn = follow_up.turn_id;
        assert!(machine
            .handle_command(AgentCommand::Prompt(follow_up), &mut writes)
            .unwrap()
            .is_none());
        let turn_start = pop_wire(&mut writes);
        assert_eq!(turn_start["method"], "turn/start");
        assert_eq!(turn_start["params"]["threadId"], "thread-1");
        assert_eq!(turn_start["params"]["cwd"], "/proc/self/fd/19/nested");
        assert_eq!(turn_start["params"]["environments"], json!([]));
        assert_eq!(turn_start["params"]["approvalPolicy"], "never");
        assert_eq!(turn_start["params"]["approvalsReviewer"], "user");
        assert_eq!(
            turn_start["params"]["sandboxPolicy"],
            json!({
                "type": "workspaceWrite",
                "writableRoots": ["/proc/self/fd/19"],
                "networkAccess": false,
                "excludeSlashTmp": true,
                "excludeTmpdirEnvVar": true
            })
        );
        {
            let snapshot = view.lock();
            assert_eq!(snapshot.phase, CodexAppServerPhase::StartingTurn);
            assert_eq!(snapshot.provider_thread_id.as_deref(), Some("thread-1"));
            assert_eq!(snapshot.displayed_turn_id, Some(follow_up_local_turn));
            assert_eq!(snapshot.displayed_turn_ordinal, Some(2));
            assert_eq!(
                snapshot.displayed_follow_up_feedback.as_deref(),
                Some("continue with the next fix")
            );
            assert!(snapshot.agent_text.is_empty());
            assert!(snapshot.commands.is_empty());
            assert!(snapshot.file_changes.is_empty());
            assert!(snapshot.last_error.is_none());
            assert_eq!(snapshot.dropped_updates, 0);
            assert_eq!(snapshot.turn_history.len(), 1);
            let archived = &snapshot.turn_history[0];
            assert_eq!(archived.ordinal, 1);
            assert_eq!(archived.local_turn_id, first_local_turn);
            assert!(archived.follow_up_feedback.is_none());
            assert_eq!(archived.agent_text, "first answer");
            assert!(!archived.agent_text_truncated);
            assert_eq!(archived.dropped_updates, 7);
            assert_eq!(archived.commands.len(), 1);
            assert_eq!(archived.commands[0].command, "true");
            assert_eq!(archived.commands[0].status, "completed");
            assert!(archived.commands[0].output_omitted);
            assert_eq!(archived.file_changes.len(), 1);
            assert_eq!(archived.file_changes[0].status, "completed");
            assert_eq!(archived.file_changes[0].path.as_deref(), Some("src/lib.rs"));
            assert_eq!(archived.file_changes[0].change_count, 1);
            assert!(!archived.file_changes[0].changes_truncated);
            assert!(!archived.file_changes[0].path_truncated);
            assert_eq!(snapshot.dropped_turns, 0);
        }

        let cloned_snapshot = view.lock().clone();
        assert!(Arc::ptr_eq(
            &cloned_snapshot.turn_history,
            &view.lock().turn_history
        ));

        let request_id = turn_start["id"].clone();
        machine
            .handle_message(
                json!({"id": request_id, "result": {"turn": {"id": "turn-2"}}}),
                &mut writes,
                false,
            )
            .unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::TurnStarted { turn_id } if *turn_id == follow_up_local_turn
        ));
        assert_eq!(view.lock().phase, CodexAppServerPhase::Running);

        assert!(machine
            .handle_message(
                json!({
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "itemId": "late-old-item", "delta": "stale"
                    }
                }),
                &mut writes,
                false,
            )
            .is_err());
    }

    #[test]
    fn failed_follow_up_queue_preserves_completed_flat_view_and_history_transactionally() {
        let (mut machine, mut writes, receiver, view, first_local_turn) = active_machine();
        machine.append_agent_text("completed answer");
        machine
            .handle_message(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-1",
                        "turn": {"id": "turn-1", "status": "completed"}
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let _ = receiver.try_recv().unwrap();
        let before = view.lock().clone();
        assert_eq!(before.displayed_turn_id, Some(first_local_turn));
        assert!(before.turn_history.is_empty());

        for index in 0..CODEX_APP_SERVER_WRITE_QUEUE_MAX_MESSAGES {
            machine.pending_requests.insert(
                RpcId::Integer(10_000 + index as i64),
                PendingClientRequest::Steer,
            );
        }
        assert!(machine
            .handle_command(
                AgentCommand::Prompt(AgentPrompt::new("must remain retryable")),
                &mut writes,
            )
            .is_err());

        assert_eq!(*view.lock(), before);
        assert!(machine.displayed_turn_completed);
        assert!(writes.pending.is_empty());
    }

    #[test]
    fn turn_history_evicts_oldest_at_count_limit_and_counts_drops() {
        let (mut machine, _, _, view) = machine(None);
        let mut local_turn_ids = Vec::new();
        for ordinal in 1..=(CODEX_APP_SERVER_TURN_HISTORY_CAPACITY + 1) {
            local_turn_ids.push(archive_history_turn(
                &mut machine,
                &view,
                ordinal,
                Some(format!("feedback-{ordinal}")),
                format!("answer-{ordinal}"),
            ));
        }

        let snapshot = view.lock();
        assert_eq!(
            snapshot.turn_history.len(),
            CODEX_APP_SERVER_TURN_HISTORY_CAPACITY
        );
        assert_eq!(snapshot.dropped_turns, 1);
        assert_eq!(snapshot.turn_history[0].ordinal, 2);
        assert_eq!(snapshot.turn_history[0].local_turn_id, local_turn_ids[1]);
        assert_eq!(
            snapshot.turn_history.last().unwrap().ordinal,
            CODEX_APP_SERVER_TURN_HISTORY_CAPACITY + 1
        );
        assert_eq!(
            snapshot
                .turn_history
                .last()
                .unwrap()
                .follow_up_feedback
                .as_deref(),
            Some("feedback-9")
        );
    }

    #[test]
    fn turn_history_hard_byte_budget_evicts_oldest_entries() {
        let (mut machine, _, _, view) = machine(None);
        let full_flat_text = "x".repeat(CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES);
        for ordinal in 1..=4 {
            archive_history_turn(
                &mut machine,
                &view,
                ordinal,
                Some(format!("feedback-{ordinal}")),
                full_flat_text.clone(),
            );
        }

        let snapshot = view.lock();
        let accounted: VecDeque<_> = snapshot.turn_history.iter().cloned().collect();
        assert!(turn_history_retained_bytes(&accounted) <= CODEX_APP_SERVER_TURN_HISTORY_MAX_BYTES);
        assert!(snapshot.turn_history.len() < 4);
        assert_eq!(snapshot.dropped_turns, 4 - snapshot.turn_history.len());
        assert_eq!(
            snapshot.turn_history.last().unwrap().ordinal,
            4,
            "newest history remains when it fits independently"
        );
        assert_eq!(snapshot.turn_history[0].ordinal, snapshot.dropped_turns + 1);
    }

    #[test]
    fn finish_retains_history_and_latest_turn_feedback_without_duplication() {
        let (mut machine, mut writes, receiver, view, first_local_turn) = active_machine();
        machine.append_agent_text("first answer");
        machine
            .handle_message(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-1",
                        "turn": {"id": "turn-1", "status": "completed"}
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let _ = receiver.try_recv().unwrap();

        let second_prompt = AgentPrompt::new("review the remaining edge case");
        let second_local_turn = second_prompt.turn_id;
        machine
            .handle_command(AgentCommand::Prompt(second_prompt), &mut writes)
            .unwrap();
        let turn_start = pop_wire(&mut writes);
        machine
            .handle_message(
                json!({
                    "id": turn_start["id"].clone(),
                    "result": {"turn": {"id": "turn-2"}}
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let _ = receiver.try_recv().unwrap();
        machine.append_agent_text("second answer");
        machine
            .handle_message(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-1",
                        "turn": {"id": "turn-2", "status": "completed"}
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let _ = receiver.try_recv().unwrap();

        let intent = machine
            .handle_command(AgentCommand::FinishSession, &mut writes)
            .unwrap()
            .expect("idle finish should terminate cleanly");
        let report = finish_report(
            &machine.sink,
            &view,
            Arc::new(Mutex::new(CodexAppServerProcessExit::default())),
            intent,
            String::new(),
        );
        assert_eq!(report.outcome, AgentSessionOutcome::Clean);

        let snapshot = view.lock();
        assert_eq!(snapshot.phase, CodexAppServerPhase::Ended);
        assert_eq!(snapshot.completed_turns, 2);
        assert_eq!(snapshot.turn_history.len(), 1);
        assert_eq!(snapshot.turn_history[0].ordinal, 1);
        assert_eq!(snapshot.turn_history[0].local_turn_id, first_local_turn);
        assert_eq!(snapshot.turn_history[0].agent_text, "first answer");
        assert_eq!(snapshot.displayed_turn_id, Some(second_local_turn));
        assert_eq!(snapshot.displayed_turn_ordinal, Some(2));
        assert_eq!(
            snapshot.displayed_follow_up_feedback.as_deref(),
            Some("review the remaining edge case")
        );
        assert_eq!(snapshot.agent_text, "second answer");
    }

    #[test]
    fn finish_session_requires_idle_and_returns_a_clean_terminal_intent() {
        let (mut machine, mut writes, receiver, view, _) = active_machine();
        assert!(machine
            .handle_command(AgentCommand::FinishSession, &mut writes)
            .is_err());

        assert!(machine
            .handle_message(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-1",
                        "turn": {"id": "turn-1", "status": "completed"}
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap()
            .is_none());
        let _turn_completed = receiver.try_recv().unwrap();
        assert_eq!(view.lock().phase, CodexAppServerPhase::Ready);

        let intent = machine
            .handle_command(AgentCommand::FinishSession, &mut writes)
            .unwrap()
            .expect("idle finish should end the session");
        assert_eq!(intent.outcome, AgentSessionOutcome::Clean);
        assert_eq!(intent.cause, CodexAppServerExitCause::Clean);
        assert!(intent.detail.is_none());
    }

    #[test]
    fn non_completed_terminal_turn_statuses_end_the_session() {
        for (status, cancellation_requested, expected_outcome) in [
            ("interrupted", true, AgentSessionOutcome::Cancelled),
            ("interrupted", false, AgentSessionOutcome::Failed),
            ("failed", false, AgentSessionOutcome::Failed),
        ] {
            let (mut machine, mut writes, _receiver, view, _) = active_machine();
            let intent = machine
                .handle_message(
                    json!({
                        "method": "turn/completed",
                        "params": {
                            "threadId": "thread-1",
                            "turn": {
                                "id": "turn-1", "status": status,
                                "error": {"message": "provider failure"}
                            }
                        }
                    }),
                    &mut writes,
                    cancellation_requested,
                )
                .unwrap()
                .expect("non-completed status must terminate the session");
            assert_eq!(intent.outcome, expected_outcome);
            assert!(machine.provider_turn_id.is_none());
            assert!(view.lock().provider_turn_id.is_none());
        }
    }

    #[test]
    fn idle_send_reservation_rejects_duplicate_prompt_and_finish_actions() {
        let (sender, receiver) = bounded(CODEX_APP_SERVER_COMMAND_CAPACITY);
        let view = Arc::new(Mutex::new(CodexAppServerViewSnapshot {
            phase: CodexAppServerPhase::Ready,
            ..CodexAppServerViewSnapshot::default()
        }));
        let first = AgentPrompt::new("first");
        let first_turn = first.turn_id;
        try_send_command(&sender, &view, AgentCommand::Prompt(first)).unwrap();
        assert_eq!(view.lock().phase, CodexAppServerPhase::StartingTurn);
        assert_eq!(
            try_send_command(
                &sender,
                &view,
                AgentCommand::Prompt(AgentPrompt::new("duplicate"))
            ),
            Err(AgentDriverError::TurnActive)
        );
        assert!(matches!(
            receiver.try_recv().unwrap(),
            AgentCommand::Prompt(prompt) if prompt.turn_id == first_turn
        ));
        assert!(receiver.try_recv().is_err());

        view.lock().phase = CodexAppServerPhase::Running;
        assert_eq!(
            try_send_command(&sender, &view, AgentCommand::FinishSession),
            Err(AgentDriverError::TurnActive)
        );
        assert!(receiver.try_recv().is_err());

        view.lock().phase = CodexAppServerPhase::Ready;
        try_send_command(&sender, &view, AgentCommand::FinishSession).unwrap();
        assert_eq!(view.lock().phase, CodexAppServerPhase::Stopping);
        assert_eq!(
            try_send_command(&sender, &view, AgentCommand::FinishSession),
            Err(AgentDriverError::TurnActive)
        );
        assert_eq!(receiver.try_recv().unwrap(), AgentCommand::FinishSession);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn failed_idle_send_rolls_back_its_phase_reservation() {
        let (sender, _receiver) = bounded(CODEX_APP_SERVER_COMMAND_CAPACITY);
        let active_turn = AgentTurnId::new();
        for _ in 0..CODEX_APP_SERVER_COMMAND_CAPACITY {
            sender
                .try_send(AgentCommand::Steer {
                    turn_id: active_turn,
                    text: "queued".into(),
                })
                .unwrap();
        }
        let view = Arc::new(Mutex::new(CodexAppServerViewSnapshot {
            phase: CodexAppServerPhase::Ready,
            ..CodexAppServerViewSnapshot::default()
        }));
        assert!(matches!(
            try_send_command(
                &sender,
                &view,
                AgentCommand::Prompt(AgentPrompt::new("retry later"))
            ),
            Err(AgentDriverError::Backpressure { .. })
        ));
        assert_eq!(view.lock().phase, CodexAppServerPhase::Ready);
    }

    #[test]
    fn live_turn_limit_never_evicts_an_old_provider_identity() {
        let (sender, receiver) = bounded(CODEX_APP_SERVER_COMMAND_CAPACITY);
        let view = Arc::new(Mutex::new(CodexAppServerViewSnapshot {
            phase: CodexAppServerPhase::Ready,
            completed_turns: CODEX_APP_SERVER_LIVE_TURN_MAX,
            ..CodexAppServerViewSnapshot::default()
        }));
        assert_eq!(
            try_send_command(
                &sender,
                &view,
                AgentCommand::Prompt(AgentPrompt::new("one turn too many")),
            ),
            Err(AgentDriverError::TurnLimitReached {
                limit: CODEX_APP_SERVER_LIVE_TURN_MAX,
            })
        );
        assert_eq!(view.lock().phase, CodexAppServerPhase::Ready);
        assert!(receiver.try_recv().is_err());
        // Finishing remains available at the cap.
        try_send_command(&sender, &view, AgentCommand::FinishSession).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), AgentCommand::FinishSession);

        let (mut machine, mut writes, receiver, _view, _) = active_machine();
        machine
            .handle_message(
                json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": "thread-1",
                        "turn": {"id": "turn-1", "status": "completed"}
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let _ = receiver.try_recv().unwrap();
        for ordinal in 2..=CODEX_APP_SERVER_LIVE_TURN_MAX {
            machine.remember_completed_turn(format!("turn-{ordinal}"));
        }
        assert_eq!(
            machine.completed_provider_turn_ids.len(),
            CODEX_APP_SERVER_LIVE_TURN_MAX
        );
        assert!(machine
            .completed_provider_turn_ids
            .contains(&"turn-1".to_string()));
        assert!(machine
            .handle_command(
                AgentCommand::Prompt(AgentPrompt::new("must finish")),
                &mut writes,
            )
            .is_err());
        assert!(machine
            .completed_provider_turn_ids
            .contains(&"turn-1".into()));
    }

    #[test]
    fn rpc_id_numeric_representation_is_canonical() {
        assert_eq!(RpcId::parse(&json!(1)).unwrap(), RpcId::Integer(1));
        assert_eq!(RpcId::Integer(1).value(), json!(1));
        assert!(RpcId::parse(&json!(1.5)).is_err());
    }

    #[test]
    fn native_config_attestation_rejects_every_authority_source() {
        let mut cases = Vec::new();

        let mut mcp = attested_config();
        mcp["config"]["mcp_servers"] = json!({"evil": {"command": "/bin/true"}});
        cases.push(mcp);

        let mut project = attested_config();
        project["layers"].as_array_mut().unwrap().push(json!({
            "name": {"type": "project", "dotCodexFolder": "/workspace/.codex"},
            "version": "test", "disabledReason": "untrusted", "config": {}
        }));
        cases.push(project);

        let mut feature = attested_config();
        feature["config"]["features"]["hooks"] = Value::Bool(true);
        cases.push(feature);

        let mut web = attested_config();
        web["config"]["web_search"] = Value::String("live".into());
        cases.push(web);

        let mut unknown_enabled = attested_config();
        unknown_enabled["config"]["features"]["future_authority"] = Value::Bool(true);
        cases.push(unknown_enabled);

        let mut origin = attested_config();
        origin["origins"]["features.plugins"]["name"]["type"] = Value::String("system".into());
        cases.push(origin);

        for (index, config) in cases.iter().enumerate() {
            assert!(
                attest_native_codex_config(config, "/private/codex", &test_tool_environment(),)
                    .is_err(),
                "authority case {index} unexpectedly passed"
            );
        }
        assert!(attest_native_codex_config(
            &attested_config(),
            "/private/codex",
            &test_tool_environment(),
        )
        .is_ok());
    }

    #[test]
    fn native_handshake_rejects_codex_home_mismatch_and_token_refresh() {
        let (mut mismatch_machine, mut writes, _, _) = machine(None);
        mismatch_machine.queue_initialize(&mut writes).unwrap();
        let _initialize = pop_wire(&mut writes);
        assert!(mismatch_machine
            .handle_message(
                json!({"id": 1, "result": {"userAgent": "anvil/0.147.0 (test)", "codexHome": "/home/user/.codex"}}),
                &mut writes,
                false,
            )
            .is_err());

        let (mut version_machine, mut writes, _, _) = machine(None);
        version_machine.queue_initialize(&mut writes).unwrap();
        let _initialize = pop_wire(&mut writes);
        assert!(version_machine
            .handle_message(
                json!({"id": 1, "result": {"userAgent": "anvil/0.148.0 (test)", "codexHome": "/private/codex"}}),
                &mut writes,
                false,
            )
            .is_err());

        let (mut machine, mut writes, _, _) = machine(None);
        assert!(machine
            .handle_message(
                json!({
                    "id": "refresh-1",
                    "method": "account/chatgptAuthTokens/refresh",
                    "params": {"reason": "unauthorized", "previousAccountId": "account-1"}
                }),
                &mut writes,
                false,
            )
            .is_err());
        assert!(writes.pending.is_empty());
    }

    #[test]
    fn unknown_server_request_gets_method_not_found() {
        let (mut machine, mut writes, _, _) = machine(None);
        machine
            .handle_message(
                json!({"id": "opaque-7", "method": "future/doThing", "params": null}),
                &mut writes,
                false,
            )
            .unwrap();
        assert_eq!(
            pop_wire(&mut writes),
            json!({
                "id": "opaque-7",
                "error": {"code": -32601, "message": "method not supported by Anvil"}
            })
        );
    }

    #[test]
    fn unknown_notification_is_ignored_even_without_object_params() {
        let (mut machine, mut writes, _, _) = machine(None);
        assert!(machine
            .handle_message(
                json!({"method": "future/notification", "params": null}),
                &mut writes,
                false,
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn all_native_approvals_are_fail_closed_but_deny_remains() {
        let (mut machine, mut writes, receiver, view, _) = active_machine();
        machine
            .handle_message(
                json!({
                    "id": 71,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1", "itemId": "cmd-1",
                        "startedAtMs": 1, "command": "cargo test", "cwd": "/workspace"
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let approval_id = view.lock().pending_approvals[0].id;
        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::ApprovalRequested { approval_id: id, .. } if *id == approval_id
        ));
        let result = machine.handle_command(
            AgentCommand::DecideApproval {
                id: approval_id,
                decision: ApprovalDecision::Approve,
            },
            &mut writes,
        );
        assert!(result.is_err());
        assert!(writes.pending.is_empty());
        assert!(machine.approvals.contains_key(&approval_id));
        machine
            .handle_command(
                AgentCommand::DecideApproval {
                    id: approval_id,
                    decision: ApprovalDecision::Deny { reason: None },
                },
                &mut writes,
            )
            .unwrap();
        assert_eq!(
            pop_wire(&mut writes),
            json!({"id": 71, "result": {"decision": "decline"}})
        );
        let _resumed = receiver.try_recv().unwrap();

        machine.update_file_view(
            "file-1",
            "inProgress",
            &[json!({
                "path": "src/lib.rs",
                "kind": {"type": "update", "move_path": "src/main.rs"},
                "diff": "-old\n+fixed"
            })],
        );
        machine
            .handle_message(
                json!({
                    "id": "file-rpc",
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1", "itemId": "file-1",
                        "startedAtMs": 2, "reason": "edit source"
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let file_approval = view.lock().pending_approvals[0].id;
        let result = machine.handle_command(
            AgentCommand::DecideApproval {
                id: file_approval,
                decision: ApprovalDecision::Approve,
            },
            &mut writes,
        );
        assert!(result.is_err());
        assert!(writes.pending.is_empty());
        machine
            .handle_command(
                AgentCommand::DecideApproval {
                    id: file_approval,
                    decision: ApprovalDecision::Deny { reason: None },
                },
                &mut writes,
            )
            .unwrap();
        assert_eq!(
            pop_wire(&mut writes),
            json!({"id": "file-rpc", "result": {"decision": "decline"}})
        );
    }

    #[test]
    fn unknown_approval_authority_fields_fail_closed() {
        let cases = [
            (
                "item/commandExecution/requestApproval",
                json!({"futureAuthority": {"scope": "session"}}),
            ),
            (
                "item/fileChange/requestApproval",
                json!({"futureWriteGrant": "/outside"}),
            ),
        ];
        for (index, (method, extension)) in cases.into_iter().enumerate() {
            let (mut machine, mut writes, _, view, _) = active_machine();
            let mut params = json!({
                "threadId": "thread-1", "turnId": "turn-1", "itemId": "item",
                "startedAtMs": 1
            });
            params
                .as_object_mut()
                .unwrap()
                .extend(extension.as_object().unwrap().clone());
            let result = machine.handle_message(
                json!({"id": index as i64 + 1, "method": method, "params": params}),
                &mut writes,
                false,
            );
            assert!(result.is_err(), "case {index} unexpectedly passed");
            assert!(view.lock().pending_approvals.is_empty());
        }
    }

    #[test]
    fn stable_command_authority_fields_remain_display_and_deny_compatible() {
        let (mut machine, mut writes, _receiver, view, _) = active_machine();
        machine
            .handle_message(
                json!({
                    "id": 44,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1", "itemId": "network-only",
                        "startedAtMs": 1, "environmentId": "local",
                        "networkApprovalContext": {"host": "example.test"},
                        "availableDecisions": ["accept", "decline"],
                        "additionalPermissions": {"network": {"enabled": true}}
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let approval = view.lock().pending_approvals[0].id;
        assert!(machine
            .handle_command(
                AgentCommand::DecideApproval {
                    id: approval,
                    decision: ApprovalDecision::Approve,
                },
                &mut writes,
            )
            .is_err());
        machine
            .handle_command(
                AgentCommand::DecideApproval {
                    id: approval,
                    decision: ApprovalDecision::Deny { reason: None },
                },
                &mut writes,
            )
            .unwrap();
        assert_eq!(
            pop_wire(&mut writes),
            json!({"id": 44, "result": {"decision": "decline"}})
        );
    }

    #[test]
    fn file_approval_requires_exact_displayable_paths() {
        for path in [
            "\u{202e}".to_string(),
            "\u{0007}".to_string(),
            "src/evil\u{202e}.rs".to_string(),
            "src/bad\u{0007}.rs".to_string(),
            "x".repeat(FIELD_MAX_BYTES + 1),
        ] {
            let (mut machine, mut writes, _, view, _) = active_machine();
            machine.update_file_view(
                "file-unsafe",
                "inProgress",
                &[json!({
                    "path": path,
                    "kind": {"type": "update"},
                    "diff": "+change"
                })],
            );
            let result = machine.handle_message(
                json!({
                    "id": 9,
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "itemId": "file-unsafe", "startedAtMs": 1
                    }
                }),
                &mut writes,
                false,
            );
            assert!(
                result.is_err(),
                "unsafe path unexpectedly registered: {path:?}"
            );
            assert!(
                view.lock().pending_approvals.is_empty(),
                "unsafe path left approval authority behind: {path:?}"
            );
        }
    }

    #[test]
    fn file_approval_freezes_exact_changes_and_rejects_item_mutation() {
        let (mut machine, mut writes, _receiver, view, _) = active_machine();
        machine.update_file_view(
            "file-frozen",
            "inProgress",
            &[json!({
                "path": "src/old.rs",
                "kind": {"type": "update", "move_path": "src/new.rs"},
                "diff": "-old\n+new"
            })],
        );
        machine
            .handle_message(
                json!({
                    "id": "freeze-rpc",
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "itemId": "file-frozen", "startedAtMs": 1
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let frozen = view.lock().pending_approvals[0].clone();
        assert_eq!(frozen.file_paths, vec!["src/old.rs"]);
        assert_eq!(
            frozen.file_changes,
            vec![CodexAppServerApprovalFileChange {
                path: "src/old.rs".into(),
                kind: "update".into(),
                diff: "-old\n+new".into(),
                move_path: Some("src/new.rs".into()),
            }]
        );

        let patch_mutation = machine.handle_message(
            json!({
                "method": "item/fileChange/patchUpdated",
                "params": {
                    "threadId": "thread-1", "turnId": "turn-1",
                    "itemId": "file-frozen",
                    "changes": [{
                        "path": "src/other.rs", "kind": {"type": "add"}, "diff": "+other"
                    }]
                }
            }),
            &mut writes,
            false,
        );
        assert!(patch_mutation.is_err());
        let item_mutation = machine.handle_message(
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-1", "turnId": "turn-1",
                    "item": {
                        "type": "fileChange", "id": "file-frozen", "status": "completed",
                        "changes": [{
                            "path": "src/other.rs", "kind": {"type": "add"}, "diff": "+other"
                        }]
                    }
                }
            }),
            &mut writes,
            false,
        );
        assert!(item_mutation.is_err());
        assert_eq!(view.lock().pending_approvals[0], frozen);
    }

    #[test]
    fn file_approval_rejects_inexact_change_components_and_unknown_shape() {
        let cases = [
            json!({
                "path": "src/lib.rs",
                "kind": {"type": "update", "move_path": "src/evil\u{202e}.rs"},
                "diff": "+safe"
            }),
            json!({
                "path": "src/lib.rs", "kind": {"type": "update"},
                "diff": "+unsafe\u{0007}"
            }),
            json!({
                "path": "src/lib.rs", "kind": {"type": "up\u{202e}date"},
                "diff": "+safe"
            }),
            json!({
                "path": "src/lib.rs", "kind": {"type": "update"},
                "diff": "+safe", "hiddenAuthority": true
            }),
        ];
        for (index, change) in cases.into_iter().enumerate() {
            let (mut machine, mut writes, _, view, _) = active_machine();
            machine.update_file_view("file-unsafe", "inProgress", &[change]);
            let result = machine.handle_message(
                json!({
                    "id": index as i64 + 200,
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "itemId": "file-unsafe", "startedAtMs": 1
                    }
                }),
                &mut writes,
                false,
            );
            assert!(result.is_err(), "inexact case {index} unexpectedly passed");
            assert!(view.lock().pending_approvals.is_empty());
        }
    }

    #[test]
    fn pending_approval_frozen_bytes_are_aggregate_bounded() {
        let (mut machine, mut writes, _receiver, view, _) = active_machine();
        let full_changes: Vec<_> = (0..FILE_CHANGES_PER_ITEM)
            .map(|index| {
                json!({
                    "path": format!("src/file-{index}.rs"),
                    "kind": {"type": "update"},
                    "diff": "x".repeat(FILE_DIFF_MAX_BYTES)
                })
            })
            .collect();
        for index in 0..2 {
            let item_id = format!("budget-{index}");
            machine.update_file_view(&item_id, "inProgress", &full_changes);
            let result = machine.handle_message(
                json!({
                    "id": index as i64 + 301,
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "itemId": item_id, "startedAtMs": 1
                    }
                }),
                &mut writes,
                false,
            );
            if index == 0 {
                assert!(result.is_ok());
            } else {
                assert!(result.is_err());
            }
        }
        assert_eq!(view.lock().pending_approvals.len(), 1);
        assert!(!machine
            .approvals
            .values()
            .any(|pending| pending.item_id == "budget-1"));
    }

    #[test]
    fn server_request_resolved_removes_matching_approval() {
        let (mut machine, mut writes, receiver, view, turn_id) = active_machine();
        machine.update_file_view(
            "file-1",
            "inProgress",
            &[json!({
                "path": "src/lib.rs",
                "kind": {"type": "update"},
                "diff": "+fixed"
            })],
        );
        machine
            .handle_message(
                json!({
                    "id": 88,
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1", "itemId": "file-1",
                        "startedAtMs": 2
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let resolved_approval_id = view.lock().pending_approvals[0].id;
        let _approval_event = receiver.try_recv().unwrap();
        machine
            .handle_message(
                json!({
                    "method": "serverRequest/resolved",
                    "params": {"threadId": "thread-1", "requestId": 88}
                }),
                &mut writes,
                false,
            )
            .unwrap();
        assert!(machine.approvals.is_empty());
        assert!(view.lock().pending_approvals.is_empty());
        // A decision may have been created from the previous UI frame before
        // the resolved notification arrived. The bounded tombstone makes it
        // an idempotent no-op instead of killing the session.
        machine
            .handle_command(
                AgentCommand::DecideApproval {
                    id: resolved_approval_id,
                    decision: ApprovalDecision::Approve,
                },
                &mut writes,
            )
            .unwrap();
        assert!(writes.pending.is_empty());
        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::WorkResumed { turn_id: id } if *id == turn_id
        ));

        machine.update_file_view(
            "file-2",
            "inProgress",
            &[json!({
                "path": "src/main.rs", "kind": {"type": "add"}, "diff": "+main"
            })],
        );
        machine
            .handle_message(
                json!({
                    "id": 89,
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1", "itemId": "file-2",
                        "startedAtMs": 3
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        let clicked_id = view.lock().pending_approvals[0].id;
        machine
            .handle_command(
                AgentCommand::DecideApproval {
                    id: clicked_id,
                    decision: ApprovalDecision::Deny { reason: None },
                },
                &mut writes,
            )
            .unwrap();
        assert_eq!(
            pop_wire(&mut writes),
            json!({"id": 89, "result": {"decision": "decline"}})
        );
        // The server can resolve after Anvil's click was already queued.
        machine
            .handle_message(
                json!({
                    "method": "serverRequest/resolved",
                    "params": {"threadId": "thread-1", "requestId": 89}
                }),
                &mut writes,
                false,
            )
            .unwrap();
    }

    #[test]
    fn jsonl_reader_drains_more_than_one_message_budget_before_eof() {
        let input: String = (0..100)
            .map(|id| format!("{{\"id\":{id},\"result\":{{}}}}\n"))
            .collect();
        let mut cursor = Cursor::new(input.into_bytes());
        let mut reader = JsonLineReader::default();
        let first = reader.read_available(&mut cursor).unwrap();
        assert_eq!(first.len(), READ_MESSAGES_PER_TICK);
        assert!(!reader.eof);
        let second = reader.read_available(&mut cursor).unwrap();
        assert_eq!(second.len(), 100 - READ_MESSAGES_PER_TICK);
        assert!(reader.eof);
        assert!(reader.buffer.is_empty());
    }

    #[test]
    fn jsonl_reader_rejects_partial_eof_and_oversized_record() {
        let mut reader = JsonLineReader::default();
        assert!(reader
            .read_available(&mut Cursor::new(b"{\"id\":1}".to_vec()))
            .is_err());

        let mut oversized = vec![b'x'; CODEX_APP_SERVER_JSONL_MAX_BYTES + 1];
        oversized.push(b'\n');
        let mut reader = JsonLineReader::default();
        let mut cursor = Cursor::new(oversized);
        let mut rejected = false;
        for _ in 0..8 {
            if reader.read_available(&mut cursor).is_err() {
                rejected = true;
                break;
            }
        }
        assert!(rejected);
    }

    #[test]
    fn jsonl_reader_rejects_duplicate_rpc_correlation_members_recursively() {
        for encoded in [
            br#"{"id":1,"id":2,"result":{}}
"#
            .as_slice(),
            br#"{"method":"turn/completed","m\u0065thod":"item/completed","params":{}}
"#
            .as_slice(),
            br#"{"id":1,"result":{"turn":{"id":"turn-1","id":"turn-2"}}}
"#
            .as_slice(),
            br#"{"id":1,"result":{"future":[{"authority":"read","authority":"write"}]}}
"#
            .as_slice(),
        ] {
            let mut reader = JsonLineReader::default();
            let error = reader
                .read_available(&mut Cursor::new(encoded))
                .unwrap_err();
            assert_eq!(error.cause, CodexAppServerExitCause::ProtocolFailed);
            assert!(
                error.detail.contains("duplicate JSON object member"),
                "{}",
                error.detail
            );
            assert!(!error.detail.contains("\"id\""), "{}", error.detail);
        }
    }

    #[test]
    fn jsonl_reader_rejects_serde_json_raw_value_sentinel() {
        let mut reader = JsonLineReader::default();
        let error = reader
            .read_available(&mut Cursor::new(
                br#"{"$serde_json::private::RawValue":"{\"id\":1,\"id\":2,\"result\":{}}"}
"#,
            ))
            .unwrap_err();
        assert_eq!(error.cause, CodexAppServerExitCause::ProtocolFailed);
        assert!(
            error.detail.contains("reserved JSON object member"),
            "{}",
            error.detail
        );
        assert!(!error.detail.contains("RawValue"), "{}", error.detail);
        assert!(!error.detail.contains("\"id\""), "{}", error.detail);
    }

    #[test]
    fn repeated_delta_for_an_existing_item_does_not_spend_another_item_slot() {
        let (mut machine, mut writes, _receiver, _, _) = active_machine();
        for index in 0..CODEX_APP_SERVER_COMMAND_VIEW_CAPACITY {
            machine.agent_delta_items.insert(format!("item-{index}"));
        }
        machine
            .handle_message(
                json!({
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "itemId": "item-0", "delta": "more"
                    }
                }),
                &mut writes,
                false,
            )
            .unwrap();
        assert!(machine
            .handle_message(
                json!({
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": "thread-1", "turnId": "turn-1",
                        "itemId": "overflow", "delta": "new"
                    }
                }),
                &mut writes,
                false,
            )
            .is_err());
    }

    #[test]
    fn ordinary_event_backpressure_updates_view_without_reentrant_locking() {
        let limits = AgentEventQueueLimits {
            message_capacity: 3,
            byte_capacity: 256 * 1024,
            max_event_bytes: 64 * 1024,
            critical_reserve_messages: 1,
            critical_reserve_bytes: 64 * 1024,
        };
        let (sender, _receiver) = agent_event_channel_with_limits(limits).unwrap();
        let view = Arc::new(Mutex::new(CodexAppServerViewSnapshot::default()));
        let machine = ProtocolMachine::new(
            "/proc/self/fd/1".into(),
            "/proc/self/fd/1".into(),
            NativeProtocolAuthority {
                expected_codex_home: PathBuf::from("/private/codex"),
                expected_tool_environment: BTreeMap::from([("PATH".into(), "/usr/bin".into())]),
                credentials: NativeCodexCredentials::new(
                    "test-access-token".into(),
                    "account-1".into(),
                )
                .unwrap(),
            },
            None,
            AgentEventSink::new(stream(), sender),
            Arc::clone(&view),
        );
        machine
            .emit_update(AgentEventKind::TextDelta, None)
            .unwrap();
        machine
            .emit_update(AgentEventKind::TextDelta, None)
            .unwrap();
        machine
            .emit_update(AgentEventKind::TextDelta, None)
            .unwrap();
        assert_eq!(view.lock().dropped_updates, 1);
    }

    #[test]
    fn view_text_and_item_payloads_are_bounded() {
        assert_eq!(CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES, 256 * 1024);
        assert_eq!(CODEX_APP_SERVER_COMMAND_VIEW_CAPACITY, 16);
        assert_eq!(CODEX_APP_SERVER_FILE_VIEW_CAPACITY, 16);
        assert_eq!(CODEX_APP_SERVER_APPROVAL_CAPACITY, 8);
        assert_eq!(FILE_CHANGES_PER_ITEM, 16);
        let (machine, _, _, view, _) = active_machine();
        machine.append_agent_text(&"a".repeat(CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES + 8));
        for index in 0..(CODEX_APP_SERVER_COMMAND_VIEW_CAPACITY + 2) {
            machine.update_command_view(
                &format!("command-{index}"),
                json!({
                    "command": "x", "cwd": "/w", "status": "completed",
                    "aggregatedOutput": "z".repeat(COMMAND_OUTPUT_MAX_BYTES + 5)
                })
                .as_object()
                .unwrap(),
            );
        }
        let snapshot = view.lock();
        assert_eq!(
            snapshot.agent_text.len(),
            CODEX_APP_SERVER_AGENT_TEXT_MAX_BYTES
        );
        assert!(snapshot.agent_text_truncated);
        assert_eq!(
            snapshot.commands.len(),
            CODEX_APP_SERVER_COMMAND_VIEW_CAPACITY
        );
        assert!(snapshot.commands.iter().all(|command| {
            command.output.len() <= COMMAND_OUTPUT_MAX_BYTES && command.output_truncated
        }));
    }

    #[test]
    fn file_views_changes_and_pending_approvals_obey_capacities() {
        let (mut machine, mut writes, _receiver, view, _) = active_machine();
        let changes: Vec<_> = (0..(FILE_CHANGES_PER_ITEM + 1))
            .map(|index| {
                json!({
                    "path": format!("src/{index}.rs"),
                    "kind": {"type": "add"},
                    "diff": "+x"
                })
            })
            .collect();
        for index in 0..(CODEX_APP_SERVER_FILE_VIEW_CAPACITY + 2) {
            machine.update_file_view(&format!("file-{index}"), "inProgress", &changes);
        }
        {
            let snapshot = view.lock();
            assert_eq!(
                snapshot.file_changes.len(),
                CODEX_APP_SERVER_FILE_VIEW_CAPACITY
            );
            assert!(snapshot.file_changes.iter().all(|entry| {
                entry.changes.len() == FILE_CHANGES_PER_ITEM && entry.changes_truncated
            }));
        }

        for index in 0..CODEX_APP_SERVER_APPROVAL_CAPACITY {
            machine
                .handle_message(
                    json!({
                        "id": index as i64 + 500,
                        "method": "item/commandExecution/requestApproval",
                        "params": {
                            "threadId": "thread-1", "turnId": "turn-1",
                            "itemId": format!("command-{index}"), "startedAtMs": 1,
                            "command": "true", "cwd": "/workspace"
                        }
                    }),
                    &mut writes,
                    false,
                )
                .unwrap();
        }
        let overflow = machine.handle_message(
            json!({
                "id": 999,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "threadId": "thread-1", "turnId": "turn-1",
                    "itemId": "command-overflow", "startedAtMs": 1,
                    "command": "true", "cwd": "/workspace"
                }
            }),
            &mut writes,
            false,
        );
        assert!(overflow.is_err());
        assert_eq!(machine.approvals.len(), CODEX_APP_SERVER_APPROVAL_CAPACITY);
        assert_eq!(
            view.lock().pending_approvals.len(),
            CODEX_APP_SERVER_APPROVAL_CAPACITY
        );
    }

    #[test]
    fn cgroup_guardian_does_not_depend_on_a_single_digit_fd() {
        let held: Vec<_> = (0..32).map(|_| File::open("/dev/null").unwrap()).collect();
        let path =
            std::env::temp_dir().join(format!("anvil-cgroup-guardian-test-{}", Uuid::new_v4()));
        let mut target = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(target.as_raw_fd() > 9);
        let mut guardian = CgroupGuardian::spawn(&target).unwrap();
        guardian.trigger_and_wait().unwrap();
        target.seek(SeekFrom::Start(0)).unwrap();
        let mut written = String::new();
        target.read_to_string(&mut written).unwrap();
        assert_eq!(written, "1\n");
        drop(held);
        drop(target);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn guardian_and_launch_gate_environments_fail_closed() {
        let mut guardian_command = Command::new(SYSTEM_SHELL);
        guardian_command
            .env("BASH_ENV", "/tmp/untrusted")
            .env("ENV", "/tmp/untrusted")
            .env("BASH_FUNC_attack%%", "() { :; }");
        clear_guardian_environment(&mut guardian_command);
        assert_eq!(guardian_command.get_envs().count(), 0);

        let source = vec![
            (OsString::from("HOME"), OsString::from("/home/test")),
            (
                OsString::from("DBUS_SESSION_BUS_ADDRESS"),
                OsString::from("unix:path=/run/user/1/bus"),
            ),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                OsString::from("/run/user/1"),
            ),
            (OsString::from("BASH_ENV"), OsString::from("/tmp/evil")),
            (OsString::from("ENV"), OsString::from("/tmp/evil")),
            (
                OsString::from("BASH_FUNC_attack%%"),
                OsString::from("() { :; }"),
            ),
            (OsString::from("OPENAI_API_KEY"), OsString::from("test-key")),
        ];
        let wrapper = selected_environment(&source, SYSTEMD_WRAPPER_ENV_ALLOWLIST);
        let provider = selected_environment(&source, PROVIDER_ENV_ALLOWLIST);
        let wrapper_names: HashSet<_> = wrapper.iter().map(|(name, _)| name.clone()).collect();
        let provider_names: HashSet<_> = provider.iter().map(|(name, _)| name.clone()).collect();
        assert!(wrapper_names.contains(OsStr::new("DBUS_SESSION_BUS_ADDRESS")));
        assert!(wrapper_names.contains(OsStr::new("XDG_RUNTIME_DIR")));
        for forbidden in [
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "CODEX_HOME",
            "HOME",
            "OPENAI_API_KEY",
            "BASH_ENV",
            "ENV",
            "BASH_FUNC_attack%%",
        ] {
            assert!(!provider_names.contains(OsStr::new(forbidden)));
        }
        for hook in ["BASH_ENV", "ENV", "BASH_FUNC_attack%%"] {
            assert!(!wrapper_names.contains(OsStr::new(hook)));
        }
        let gate = launch_gate_script();
        assert!(gate.starts_with("/bin/kill -STOP"));
        assert!(gate.contains("exec /usr/bin/env"));
        assert!(gate.contains("-u DBUS_SESSION_BUS_ADDRESS"));
        assert!(gate.contains("-u XDG_RUNTIME_DIR"));
    }

    #[test]
    fn tool_environment_keeps_safe_toolchains_and_drops_repo_and_proxy_state() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("anvil-tool-env-{}", Uuid::new_v4()));
        let safe = root.join("safe-bin");
        let repository = root.join("repository");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&safe).unwrap();
        std::fs::create_dir_all(repository.join("bin")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let source = vec![
            (
                OsString::from("PATH"),
                OsString::from(format!(
                    ".:{}:{}",
                    repository.join("bin").display(),
                    safe.display()
                )),
            ),
            (OsString::from("HOME"), OsString::from(root.as_os_str())),
            (
                OsString::from("HTTPS_PROXY"),
                OsString::from("http://user:password@proxy.invalid"),
            ),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
        ];
        let environment = native_tool_environment(&source, &repository, &worktree).unwrap();
        assert_eq!(
            environment.get("PATH"),
            Some(&safe.to_string_lossy().into_owned())
        );
        assert_eq!(
            environment.get("HOME"),
            Some(&root.to_string_lossy().into_owned())
        );
        assert_eq!(environment.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert!(!environment.contains_key("HTTPS_PROXY"));
        assert!(!environment["PATH"].contains("repository"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guardian_write_failure_is_not_empty_without_events_evidence() {
        let kill = OpenOptions::new().write(true).open("/dev/full").unwrap();
        let path = std::env::temp_dir().join(format!(
            "anvil-cgroup-events-evidence-test-{}",
            Uuid::new_v4()
        ));
        let mut events_writer = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        events_writer.write_all(b"populated 0\n").unwrap();
        drop(events_writer);
        let events = File::open(&path).unwrap();
        let guardian = CgroupGuardian::spawn(&kill).unwrap();
        let mut containment = CgroupContainment {
            kill,
            events,
            guardian,
        };
        // /dev/full makes both guardian and direct writes fail. Success is
        // nevertheless safe because the independently pinned events file is
        // the sole evidence and explicitly says populated 0.
        containment.kill_all_and_wait_empty().unwrap();
        drop(containment);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pre_containment_guard_drop_kills_and_reaps_stopped_launch_gate() {
        let mut command = Command::new(SYSTEM_SHELL);
        command
            .arg("-c")
            .arg("/bin/kill -STOP \"$$\"; exit 99")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let child = command.spawn().unwrap();
        wait_for_launch_gate_stop(child.id()).unwrap();
        let snapshot = Arc::new(Mutex::new(CodexAppServerProcessExit::default()));
        {
            let _guard = ChildProcessGuard::new(
                child,
                Arc::clone(&snapshot),
                "unused-before-containment.scope".into(),
            );
        }
        let snapshot = *snapshot.lock();
        assert!(snapshot.spawned);
        assert!(!snapshot.provider_released);
        assert!(snapshot.reaped);
        assert!(snapshot.containment_verified_empty);
    }

    #[test]
    fn cgroup_scope_kills_term_ignoring_setsid_descendant_before_reap() {
        if !std::path::Path::new(SYSTEMD_RUN).is_file() {
            return;
        }
        let scope_unit = format!("anvil-codex-test-{}.scope", Uuid::new_v4());
        let mut command = Command::new(SYSTEMD_RUN);
        command
            .arg("--user")
            .arg("--scope")
            .arg("--quiet")
            .arg("--collect")
            .arg(format!("--unit={scope_unit}"))
            .arg("--property=KillMode=control-group")
            .arg("--")
            .arg(SYSTEM_SHELL)
            .arg("-c")
            .arg("kill -STOP $$ || exit 125; exec \"$@\"")
            .arg("gate")
            .arg(SYSTEM_SHELL)
            .arg("-c")
            .arg("trap '' TERM; setsid sh -c 'echo $$; trap \"\" TERM; while :; do sleep 1; done' & while :; do sleep 1; done")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command.process_group(0);
        let child = command.spawn().unwrap();
        let process_snapshot = Arc::new(Mutex::new(CodexAppServerProcessExit::default()));
        let mut guard = ChildProcessGuard::new(child, Arc::clone(&process_snapshot), scope_unit);
        guard.attach_containment().unwrap();
        let mut stdout = guard.child_mut().unwrap().stdout.take().unwrap();
        set_nonblocking(stdout.as_raw_fd()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = String::new();
        while !output.contains('\n') && Instant::now() < deadline {
            let mut chunk = [0_u8; 64];
            match stdout.read(&mut chunk) {
                Ok(read) if read > 0 => output.push_str(&String::from_utf8_lossy(&chunk[..read])),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(IO_POLL_INTERVAL)
                }
                Err(error) => panic!("cannot read descendant pid: {error}"),
            }
        }
        let descendant_pid: i32 = output.trim().parse().expect("setsid descendant pid");
        assert_eq!(unsafe { libc::getpgid(descendant_pid) }, descendant_pid);
        assert_eq!(unsafe { libc::getsid(descendant_pid) }, descendant_pid);
        guard.stop_and_reap().unwrap();
        let snapshot = *process_snapshot.lock();
        assert!(snapshot.provider_released);
        assert!(snapshot.reaped);
        assert!(snapshot.containment_verified_empty);
        let proc_state = std::fs::read_to_string(format!("/proc/{descendant_pid}/stat"));
        assert!(proc_state.is_err() || proc_state.unwrap().contains(") Z "));
    }
}
