//! Provider-neutral task lifecycle and stable PTY-session bindings.
//!
//! A task is deliberately independent from tab and pane indices: both are UI
//! positions that change when sessions are inserted, moved, or closed.  The
//! manager only stores stable session IDs, so opaque PTY agents and future
//! native drivers can share the same task/dashboard model.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[allow(unused_imports)] // re-exported for consumers of `task` types
pub use super::event::{
    AgentEvent, AgentEventEpoch, AgentEventError, AgentEventKind, AgentEventStream,
    AgentSessionOutcome, AgentTurnId, ApprovalId, InvalidNativeAgentSessionId,
    NativeAgentSessionId, ProviderSessionId, MAX_AGENT_EVENT_DETAIL_BYTES,
    MAX_NATIVE_AGENT_SESSION_ID_BYTES,
};

const MAX_TASK_TITLE_BYTES: usize = 256;
const MAX_BRANCH_BYTES: usize = 512;

/// Stable identity for a task, independent of its worktree and UI location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Provider identity without leaking a provider's transport into task/UI code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProvider {
    Codex,
    Claude,
    OpenCode,
}

impl AgentProvider {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::OpenCode => "OpenCode",
        }
    }
}

/// Normalized task activity used by both opaque PTY agents and native drivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Starting,
    Working,
    WaitingForApproval,
    WaitingForHuman,
    ReadyForReview,
    Completed,
    Failed,
    Cancelled,
    Archived,
}

/// Result of the most recent task-validation attempt.
///
/// Validation is deliberately orthogonal to [`TaskStatus`]: a successful
/// validation makes a task safer to accept, but never accepts it on the
/// user's behalf.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskValidationStatus {
    #[default]
    NotRun,
    Running,
    Passed,
    Failed,
    Inconclusive,
    Cancelled,
}

impl TaskValidationStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotRun => "Not run",
            Self::Running => "Running",
            Self::Passed => "Passed",
            Self::Failed => "Failed",
            Self::Inconclusive => "Needs review",
            Self::Cancelled => "Cancelled",
        }
    }

    /// Whether a finished validation attempt needs a human decision.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Failed | Self::Inconclusive | Self::Cancelled)
    }
}

/// Serializable summary of the most recent validation attempt.
///
/// The terminal session is a stable jsh ID, not a tab or pane index. Keeping
/// the attempt counter with task state lets later execution records
/// correlate results even after a validation terminal has disappeared.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskValidationState {
    pub status: TaskValidationStatus,
    pub attempt: u64,
    pub terminal_session_id: Option<String>,
    pub exit_code: Option<i32>,
    pub status_detail: Option<String>,
}

/// Why a terminal session belongs to a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTerminalRole {
    Agent,
    Validation,
}

/// Runtime family selected for a task. This choice survives an individual
/// stream/process ending so a task cannot silently switch authority models.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRuntimeKind {
    #[default]
    Unassigned,
    Terminal,
    Native,
    /// A native attempt already consumed this task's one-shot authority; only
    /// the opaque PTY compatibility path may be retried from here.
    TerminalFallback,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Starting => "Starting",
            Self::Working => "Working",
            Self::WaitingForApproval => "Waiting for approval",
            Self::WaitingForHuman => "Waiting for you",
            Self::ReadyForReview => "Ready for review",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Cancelled => "Cancelled",
            Self::Archived => "Archived",
        }
    }

    pub fn is_running(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Working | Self::WaitingForApproval | Self::WaitingForHuman
        )
    }

    /// Whether the dashboard should pull this task to the user's attention.
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::WaitingForApproval | Self::WaitingForHuman | Self::ReadyForReview | Self::Failed
        )
    }

    /// States that native driver events may never leave. Ready-for-review is
    /// intentionally not terminal: review feedback may start another turn.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Archived
        )
    }
}

/// Provenance link back to the semantic command that created the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSource {
    pub session_id: String,
    pub execution_id: String,
}

/// Validated input for registering an already-created task worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTask {
    pub title: String,
    pub provider: AgentProvider,
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    /// Immutable commit the isolated worktree was created from. Native review
    /// compares against this baseline even if the Agent creates commits.
    pub base_commit: String,
    /// Immutable owned evidence captured when a semantic command created this
    /// task. It survives source-session closure and scrollback eviction.
    pub source_context: Option<super::SemanticCommandContext>,
}

/// One isolated unit of work. Runtime links use stable IDs, never pane/index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTask {
    pub id: TaskId,
    pub title: String,
    pub provider: AgentProvider,
    pub status: TaskStatus,
    pub repo_root: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub source: Option<TaskSource>,
    pub source_context: Option<super::SemanticCommandContext>,
    #[serde(default)]
    pub runtime_kind: TaskRuntimeKind,
    /// Stable jsh/PTY session ID for the opaque terminal fallback.
    ///
    /// This is intentionally separate from native run epochs and opaque
    /// provider session identities.
    #[serde(alias = "agent_session_id")]
    pub terminal_session_id: Option<String>,
    /// The latest validation attempt, if any. Older serialized tasks restore
    /// as [`TaskValidationStatus::NotRun`].
    #[serde(default)]
    pub validation: TaskValidationState,
    pub exit_code: Option<i32>,
    pub status_detail: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl AgentTask {
    pub fn needs_attention(&self) -> bool {
        // While validation is running, ReadyForReview is only an internal
        // prerequisite and should not pull the task to the user prematurely.
        if self.validation.status == TaskValidationStatus::Running {
            return false;
        }
        self.status.needs_attention() || self.validation.status.needs_attention()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskError {
    InvalidTitle,
    InvalidBranch,
    InvalidBaseCommit,
    RepoRootMustBeAbsolute,
    WorktreePathMustBeAbsolute,
    WorktreeMatchesRepoRoot,
    InvalidSourceContext,
    InvalidTerminalSessionId,
    NativeRuntimeAlreadySelected(TaskId),
    NativeEventStreamActive(TaskId),
    CannotBindTerminalInState {
        task_id: TaskId,
        status: TaskStatus,
    },
    CannotBindValidationInState {
        task_id: TaskId,
        status: TaskStatus,
    },
    ValidationAlreadyRunning {
        task_id: TaskId,
        session_id: String,
    },
    ValidationAttemptExhausted(TaskId),
    NativeEventStreamActiveDuringValidation(TaskId),
    CompletionRequiresValidation(TaskId),
    CannotCompleteAfterValidation {
        task_id: TaskId,
        status: TaskStatus,
        validation_status: TaskValidationStatus,
    },
    CannotLeaveTerminalState {
        task_id: TaskId,
        current: TaskStatus,
        requested: TaskStatus,
    },
    UnknownTask(TaskId),
    TerminalSessionAlreadyBound {
        session_id: String,
        task_id: TaskId,
    },
    TaskAlreadyBoundToTerminal {
        task_id: TaskId,
        session_id: String,
    },
    TerminalFallbackUnavailable {
        task_id: TaskId,
        status: TaskStatus,
    },
    TerminalRetryUnavailable {
        task_id: TaskId,
        status: TaskStatus,
    },
    CannotArchiveRunning(TaskId),
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTitle => {
                formatter.write_str("task title is empty, too long, or contains control characters")
            }
            Self::InvalidBranch => formatter
                .write_str("task branch is empty, too long, or contains control characters"),
            Self::InvalidBaseCommit => {
                formatter.write_str("task base commit is not a full Git object ID")
            }
            Self::RepoRootMustBeAbsolute => formatter.write_str("repository root must be absolute"),
            Self::WorktreePathMustBeAbsolute => {
                formatter.write_str("worktree path must be absolute")
            }
            Self::WorktreeMatchesRepoRoot => {
                formatter.write_str("task worktree must differ from the source repository")
            }
            Self::InvalidSourceContext => {
                formatter.write_str("task source context has invalid stable identifiers")
            }
            Self::InvalidTerminalSessionId => {
                formatter.write_str("Agent terminal session ID is invalid")
            }
            Self::NativeRuntimeAlreadySelected(task_id) => write!(
                formatter,
                "task {task_id} has selected the native Agent runtime"
            ),
            Self::NativeEventStreamActive(task_id) => write!(
                formatter,
                "task {task_id} still has an active native Agent event stream"
            ),
            Self::CannotBindTerminalInState { task_id, status } => write!(
                formatter,
                "cannot bind an Agent terminal to task {task_id} in state {}",
                status.label()
            ),
            Self::CannotBindValidationInState { task_id, status } => write!(
                formatter,
                "cannot bind a validation terminal to task {task_id} in state {}",
                status.label()
            ),
            Self::ValidationAlreadyRunning {
                task_id,
                session_id,
            } => write!(
                formatter,
                "task {task_id} validation is already running in session {session_id}"
            ),
            Self::ValidationAttemptExhausted(task_id) => write!(
                formatter,
                "task {task_id} validation attempt counter is exhausted"
            ),
            Self::NativeEventStreamActiveDuringValidation(task_id) => write!(
                formatter,
                "task {task_id} still has an active native Agent event stream"
            ),
            Self::CompletionRequiresValidation(task_id) => write!(
                formatter,
                "task {task_id} can only be completed through its latest passing validation"
            ),
            Self::CannotCompleteAfterValidation {
                task_id,
                status,
                validation_status,
            } => write!(
                formatter,
                "cannot complete task {task_id} in state {} after validation {}",
                status.label(),
                validation_status.label()
            ),
            Self::CannotLeaveTerminalState {
                task_id,
                current,
                requested,
            } => write!(
                formatter,
                "cannot move task {task_id} from terminal state {} to {}",
                current.label(),
                requested.label()
            ),
            Self::UnknownTask(task_id) => write!(formatter, "unknown task {task_id}"),
            Self::TerminalSessionAlreadyBound {
                session_id,
                task_id,
            } => {
                write!(
                    formatter,
                    "session {session_id} is already bound to task {task_id}"
                )
            }
            Self::TaskAlreadyBoundToTerminal {
                task_id,
                session_id,
            } => {
                write!(
                    formatter,
                    "task {task_id} is already bound to session {session_id}"
                )
            }
            Self::TerminalFallbackUnavailable { task_id, status } => write!(
                formatter,
                "terminal fallback is unavailable for task {task_id} in state {}",
                status.label()
            ),
            Self::TerminalRetryUnavailable { task_id, status } => write!(
                formatter,
                "terminal retry is unavailable for task {task_id} in state {}",
                status.label()
            ),
            Self::CannotArchiveRunning(task_id) => {
                write!(formatter, "cannot archive running task {task_id}")
            }
        }
    }
}

impl std::error::Error for TaskError {}

/// Runtime task registry and stable task↔PTY lookup table.
#[derive(Debug, Default)]
pub struct TaskManager {
    tasks: Vec<AgentTask>,
    task_indices: HashMap<TaskId, usize>,
    tasks_by_terminal_session: HashMap<String, TaskId>,
    tasks_by_validation_session: HashMap<String, TaskId>,
    exited_terminal_sessions: HashSet<String>,
    native_event_streams: HashMap<TaskId, NativeEventStreamState>,
}

#[derive(Debug)]
struct NativeEventStreamState {
    stream: AgentEventStream,
    next_sequence: u64,
    session_started: bool,
    active_turn: Option<AgentTurnId>,
    review_point_reached: bool,
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&mut self, new_task: NewTask) -> Result<TaskId, TaskError> {
        validate_new_task(&new_task)?;
        let id = TaskId::new();
        let now = unix_time_ms();
        let task = AgentTask {
            id,
            title: new_task.title.trim().to_string(),
            provider: new_task.provider,
            status: TaskStatus::Created,
            repo_root: new_task.repo_root,
            worktree_path: new_task.worktree_path,
            branch: new_task.branch,
            base_commit: new_task.base_commit,
            source: new_task.source_context.as_ref().map(|context| TaskSource {
                session_id: context.source_session_id.clone(),
                execution_id: context.source_execution_id.clone(),
            }),
            source_context: new_task.source_context,
            runtime_kind: TaskRuntimeKind::Unassigned,
            terminal_session_id: None,
            validation: TaskValidationState::default(),
            exit_code: None,
            status_detail: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.task_indices.insert(id, self.tasks.len());
        self.tasks.push(task);
        Ok(id)
    }

    pub fn tasks(&self) -> &[AgentTask] {
        &self.tasks
    }

    pub fn get(&self, task_id: TaskId) -> Option<&AgentTask> {
        self.task_indices
            .get(&task_id)
            .and_then(|index| self.tasks.get(*index))
    }

    pub fn task_for_terminal_session(&self, session_id: &str) -> Option<&AgentTask> {
        self.task_and_role_for_terminal_session(session_id)
            .map(|(task, _)| task)
    }

    /// Resolve both stable task identity and the terminal's purpose. This is
    /// the role-aware form UI/session-close paths should prefer when their
    /// copy or behavior differs for Agent and validation terminals.
    pub fn task_and_role_for_terminal_session(
        &self,
        session_id: &str,
    ) -> Option<(&AgentTask, TaskTerminalRole)> {
        if let Some(task_id) = self.tasks_by_validation_session.get(session_id) {
            return self
                .get(*task_id)
                .map(|task| (task, TaskTerminalRole::Validation));
        }
        self.tasks_by_terminal_session
            .get(session_id)
            .and_then(|task_id| self.get(*task_id))
            .map(|task| (task, TaskTerminalRole::Agent))
    }

    pub fn terminal_role_for_session(&self, session_id: &str) -> Option<TaskTerminalRole> {
        self.task_and_role_for_terminal_session(session_id)
            .map(|(_, role)| role)
    }

    pub fn has_active_agent_event_stream(&self, task_id: TaskId) -> bool {
        self.native_event_streams.contains_key(&task_id)
    }

    /// Return the product-level attention state for one task.
    ///
    /// A live native stream can now remain idle at ReadyForReview until the
    /// user sends feedback or explicitly finishes it. That review point is
    /// actionable even though the stream is still active; suppressing it would
    /// leave a background-completed turn parked indefinitely with no badge.
    pub fn task_needs_attention(&self, task_id: TaskId) -> bool {
        self.get(task_id)
            .is_some_and(|task| task.status != TaskStatus::Archived && task.needs_attention())
    }

    pub fn attention_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| self.task_needs_attention(task.id))
            .count()
    }

    /// Start a correlated event stream and atomically select the native
    /// runtime for an unassigned task. Selecting a native session is one-shot:
    /// that live stream may carry sequential turns, but an already-finished
    /// session cannot be restarted through this entry point.
    pub fn start_agent_event_stream(
        &mut self,
        task_id: TaskId,
    ) -> Result<AgentEventStream, AgentEventError> {
        if self.native_event_streams.contains_key(&task_id) {
            return Err(AgentEventError::StreamAlreadyActive(task_id));
        }
        let status = self
            .get(task_id)
            .ok_or(AgentEventError::UnknownTask(task_id))?
            .status;
        if status.is_terminal() {
            return Err(AgentEventError::TerminalState { task_id, status });
        }
        // Install performs the higher-priority ownership gates first
        // (terminal binding and active validation). Only then apply this
        // product's one-shot lifecycle policy.
        let (runtime_kind, terminal_bound, validation_running) = self
            .get(task_id)
            .map(|task| {
                (
                    task.runtime_kind,
                    task.terminal_session_id.is_some(),
                    task.validation.status == TaskValidationStatus::Running,
                )
            })
            .ok_or(AgentEventError::UnknownTask(task_id))?;
        if terminal_bound
            || matches!(
                runtime_kind,
                TaskRuntimeKind::Terminal | TaskRuntimeKind::TerminalFallback
            )
        {
            return Err(AgentEventError::TerminalSessionBound(task_id));
        }
        if validation_running {
            return Err(AgentEventError::ValidationActive(task_id));
        }
        if status != TaskStatus::Created {
            return Err(AgentEventError::NativeStartRequiresCreated { task_id, status });
        }
        self.install_agent_event_stream(task_id, NativeAgentSessionId::new(), false)
    }

    /// Test-only transport-incarnation hook. A live native Codex session may
    /// carry several turns, but it has no production restart authority after
    /// the stream ends.
    #[cfg(test)]
    fn replace_agent_event_stream_after_stop(
        &mut self,
        expected: &AgentEventStream,
    ) -> Result<AgentEventStream, AgentEventError> {
        let task_id = self.verify_agent_event_stream(expected)?;
        self.install_agent_event_stream(task_id, expected.session_id().clone(), true)
    }

    /// Converge a stream after its runtime has stopped but could not enqueue a
    /// final event (for example, a worker/channel failure). This is an
    /// out-of-band compare-and-swap operation: a stale runtime cannot finish a
    /// replacement stream.
    pub fn finish_agent_event_stream_after_stop(
        &mut self,
        expected: &AgentEventStream,
        outcome: AgentSessionOutcome,
        detail: Option<String>,
    ) -> Result<TaskStatus, AgentEventError> {
        let task_id = self.verify_agent_event_stream(expected)?;
        let current = self
            .get(task_id)
            .ok_or(AgentEventError::UnknownTask(task_id))?
            .status;
        if current.is_terminal() {
            return Err(AgentEventError::TerminalState {
                task_id,
                status: current,
            });
        }
        let review_point_reached = self
            .native_event_streams
            .get(&task_id)
            .is_some_and(|active| active.review_point_reached);
        let event = AgentEventKind::SessionEnded { outcome };
        let (mut next, _) = super::event::status_after_event(current, &event).ok_or(
            AgentEventError::InvalidTransition {
                task_id,
                status: current,
                event,
            },
        )?;
        if review_point_reached && outcome != AgentSessionOutcome::Clean {
            next = TaskStatus::ReadyForReview;
        }
        let detail = super::event::bounded_event_detail(detail);
        let task = self
            .task_mut(task_id)
            .map_err(|_| AgentEventError::UnknownTask(task_id))?;
        task.status = next;
        if next != current || detail.is_some() {
            task.status_detail = detail;
        }
        task.updated_at_ms = unix_time_ms();
        self.native_event_streams.remove(&task_id);
        Ok(next)
    }

    /// Undo native-runtime selection only when adapter startup returned before
    /// creating a worker or provider process. The caller owns that pre-spawn
    /// proof; this method contributes the stream-incarnation CAS so a stale
    /// startup attempt cannot reset a replacement task.
    pub(crate) fn rollback_agent_event_stream_before_spawn(
        &mut self,
        expected: &AgentEventStream,
        detail: String,
    ) -> Result<(), AgentEventError> {
        let task_id = self.verify_agent_event_stream(expected)?;
        let task = self
            .task_mut(task_id)
            .map_err(|_| AgentEventError::UnknownTask(task_id))?;
        if task.status != TaskStatus::Starting || task.runtime_kind != TaskRuntimeKind::Native {
            return Err(AgentEventError::InvalidTransition {
                task_id,
                status: task.status,
                event: AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Failed,
                },
            });
        }
        task.runtime_kind = TaskRuntimeKind::Unassigned;
        task.status = TaskStatus::Created;
        task.status_detail = super::event::bounded_event_detail(Some(detail));
        task.updated_at_ms = unix_time_ms();
        self.native_event_streams.remove(&task_id);
        Ok(())
    }

    fn verify_agent_event_stream(
        &self,
        expected: &AgentEventStream,
    ) -> Result<TaskId, AgentEventError> {
        let task_id = expected.task_id();
        let Some(active) = self.native_event_streams.get(&task_id) else {
            return Err(AgentEventError::NoActiveStream(task_id));
        };
        if active.stream.epoch() != expected.epoch() {
            return Err(AgentEventError::EpochMismatch {
                task_id,
                expected: active.stream.epoch(),
                received: expected.epoch(),
            });
        }
        if active.stream.session_id() != expected.session_id() {
            return Err(AgentEventError::SessionMismatch(task_id));
        }
        Ok(task_id)
    }

    fn install_agent_event_stream(
        &mut self,
        task_id: TaskId,
        session_id: NativeAgentSessionId,
        replacing: bool,
    ) -> Result<AgentEventStream, AgentEventError> {
        let (status, runtime_kind, terminal_bound, validation_status, validation_session) = self
            .get(task_id)
            .map(|task| {
                (
                    task.status,
                    task.runtime_kind,
                    task.terminal_session_id.is_some(),
                    task.validation.status,
                    task.validation.terminal_session_id.clone(),
                )
            })
            .ok_or(AgentEventError::UnknownTask(task_id))?;
        if status.is_terminal() {
            return Err(AgentEventError::TerminalState { task_id, status });
        }
        if terminal_bound
            || matches!(
                runtime_kind,
                TaskRuntimeKind::Terminal | TaskRuntimeKind::TerminalFallback
            )
        {
            return Err(AgentEventError::TerminalSessionBound(task_id));
        }
        if validation_status == TaskValidationStatus::Running {
            return Err(AgentEventError::ValidationActive(task_id));
        }
        let epoch = super::event::next_agent_event_epoch()?;
        let stream = AgentEventStream::new(task_id, session_id, epoch);
        if !replacing {
            if let Some(validation_session) = validation_session.as_deref() {
                self.tasks_by_validation_session.remove(validation_session);
            }
            let task = self
                .task_mut(task_id)
                .map_err(|_| AgentEventError::UnknownTask(task_id))?;
            if runtime_kind == TaskRuntimeKind::Unassigned {
                task.runtime_kind = TaskRuntimeKind::Native;
                task.status = TaskStatus::Starting;
                task.status_detail = None;
            }
            if task.validation != TaskValidationState::default() {
                let attempt = task.validation.attempt;
                task.validation = TaskValidationState {
                    attempt,
                    status_detail: Some(
                        "Agent work resumed; the previous validation result is stale".to_string(),
                    ),
                    ..TaskValidationState::default()
                };
            }
            task.updated_at_ms = unix_time_ms();
        }
        let (active_turn, review_point_reached) = if replacing {
            self.native_event_streams
                .get(&task_id)
                .map(|active| (active.active_turn, active.review_point_reached))
                .unwrap_or((None, false))
        } else {
            (None, false)
        };
        self.native_event_streams.insert(
            task_id,
            NativeEventStreamState {
                stream: stream.clone(),
                next_sequence: 1,
                session_started: false,
                active_turn,
                review_point_reached,
            },
        );
        Ok(stream)
    }

    /// Apply one lifecycle event only when all correlation fields and the
    /// contiguous sequence match the current stream. Any rejection leaves the
    /// task and stream cursor unchanged.
    pub fn apply_agent_event(
        &mut self,
        agent_event: AgentEvent,
    ) -> Result<TaskStatus, AgentEventError> {
        let (stream, sequence, kind, detail) = agent_event.into_parts();
        let task_id = stream.task_id();
        let (current, expected_provider) = self
            .get(task_id)
            .map(|task| (task.status, task.provider))
            .ok_or(AgentEventError::UnknownTask(task_id))?;
        if current.is_terminal() {
            return Err(AgentEventError::TerminalState {
                task_id,
                status: current,
            });
        }

        let active = self
            .native_event_streams
            .get(&task_id)
            .ok_or(AgentEventError::NoActiveStream(task_id))?;
        if active.stream.epoch() != stream.epoch() {
            return Err(AgentEventError::EpochMismatch {
                task_id,
                expected: active.stream.epoch(),
                received: stream.epoch(),
            });
        }
        if active.stream.session_id() != stream.session_id() {
            return Err(AgentEventError::SessionMismatch(task_id));
        }
        if active.next_sequence != sequence {
            return Err(AgentEventError::InvalidSequence {
                task_id,
                expected: active.next_sequence,
                received: sequence,
            });
        }
        if let AgentEventKind::SessionStarted {
            provider_session_id,
            resumed,
        } = &kind
        {
            if *resumed && provider_session_id.is_none() {
                return Err(AgentEventError::MissingResumeSession(task_id));
            }
            if let Some(provider_session_id) = provider_session_id {
                let received = provider_session_id.provider();
                if received != expected_provider {
                    return Err(AgentEventError::ProviderMismatch {
                        task_id,
                        expected: expected_provider,
                        received,
                    });
                }
            }
        }
        let is_session_started = matches!(&kind, AgentEventKind::SessionStarted { .. });
        let may_end_before_start = matches!(
            &kind,
            AgentEventKind::Cancelled
                | AgentEventKind::SessionEnded { .. }
                | AgentEventKind::Error { fatal: true }
        );
        if !active.session_started && !is_session_started && !may_end_before_start {
            return Err(AgentEventError::SessionNotStarted(task_id));
        }
        if active.session_started && is_session_started {
            return Err(AgentEventError::InvalidTransition {
                task_id,
                status: current,
                event: kind,
            });
        }
        match &kind {
            AgentEventKind::TurnStarted { turn_id } => {
                if let Some(active_turn) = active.active_turn {
                    return Err(AgentEventError::TurnAlreadyActive {
                        task_id,
                        turn_id: active_turn,
                    });
                }
                let _ = turn_id;
            }
            AgentEventKind::ApprovalRequested { turn_id, .. }
            | AgentEventKind::PermissionRequested { turn_id, .. }
            | AgentEventKind::InputRequested { turn_id }
            | AgentEventKind::WorkResumed { turn_id }
            | AgentEventKind::TurnCompleted { turn_id } => match active.active_turn {
                Some(active_turn) if active_turn != *turn_id => {
                    return Err(AgentEventError::TurnMismatch {
                        task_id,
                        expected: active_turn,
                        received: *turn_id,
                    });
                }
                None => return Err(AgentEventError::NoActiveTurn(task_id)),
                Some(_) => {}
            },
            _ => {}
        }
        let review_point_reached = active.review_point_reached;
        let (mut next, updates_task) = super::event::status_after_event(current, &kind).ok_or(
            AgentEventError::InvalidTransition {
                task_id,
                status: current,
                event: kind.clone(),
            },
        )?;
        if review_point_reached
            && matches!(
                &kind,
                AgentEventKind::Cancelled
                    | AgentEventKind::SessionEnded {
                        outcome: AgentSessionOutcome::Cancelled | AgentSessionOutcome::Failed,
                    }
                    | AgentEventKind::Error { fatal: true }
            )
        {
            next = TaskStatus::ReadyForReview;
        }
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(AgentEventError::SequenceExhausted(task_id))?;

        // A provider session may own more than one logical turn. Starting a
        // later turn is work resumption even though the native stream itself
        // never ended, so no validation result from an earlier review point
        // may survive it. Do this only after every correlation, transition,
        // and sequence check above has succeeded: a stale/rejected turn event
        // must leave both validation state and its stable session mapping
        // untouched.
        let turn_started = matches!(&kind, AgentEventKind::TurnStarted { .. });
        let invalidated_validation_session = if turn_started {
            self.get(task_id)
                .filter(|task| task.validation != TaskValidationState::default())
                .and_then(|task| task.validation.terminal_session_id.clone())
        } else {
            None
        };

        if updates_task {
            let task = self
                .task_mut(task_id)
                .map_err(|_| AgentEventError::UnknownTask(task_id))?;
            if turn_started && task.validation != TaskValidationState::default() {
                let attempt = task.validation.attempt;
                task.validation = TaskValidationState {
                    attempt,
                    status_detail: Some(
                        "Agent turn started; the previous validation result is stale".to_string(),
                    ),
                    ..TaskValidationState::default()
                };
            }
            task.status = next;
            if next != current || detail.is_some() {
                task.status_detail = detail;
            }
            task.updated_at_ms = unix_time_ms();
        }
        if let Some(session_id) = invalidated_validation_session {
            if self.tasks_by_validation_session.get(&session_id) == Some(&task_id) {
                self.tasks_by_validation_session.remove(&session_id);
            }
        }

        if next.is_terminal() || super::event::event_ends_stream(&kind) {
            self.native_event_streams.remove(&task_id);
        } else if let Some(active) = self.native_event_streams.get_mut(&task_id) {
            active.next_sequence = next_sequence;
            active.session_started |= is_session_started;
            match &kind {
                AgentEventKind::TurnStarted { turn_id } => active.active_turn = Some(*turn_id),
                AgentEventKind::TurnCompleted { .. } => {
                    active.active_turn = None;
                    active.review_point_reached = true;
                }
                _ => {}
            }
        }
        Ok(next)
    }

    /// Bind a freshly spawned PTY to a task. Existing bindings are immutable:
    /// replacement must be an explicit future lifecycle operation, never a
    /// side effect of a tab/index change.
    pub fn bind_terminal_session(
        &mut self,
        task_id: TaskId,
        session_id: String,
    ) -> Result<(), TaskError> {
        if !jterm_core::execution_journal::is_valid_jsh_session_id(&session_id) {
            return Err(TaskError::InvalidTerminalSessionId);
        }
        if let Some(existing_task_id) = self.tasks_by_validation_session.get(&session_id).copied() {
            return Err(TaskError::TerminalSessionAlreadyBound {
                session_id,
                task_id: existing_task_id,
            });
        }
        if let Some(existing_task_id) = self.tasks_by_terminal_session.get(&session_id).copied() {
            if existing_task_id == task_id {
                return Ok(());
            }
            return Err(TaskError::TerminalSessionAlreadyBound {
                session_id,
                task_id: existing_task_id,
            });
        }

        let task = self.task_mut(task_id)?;
        if task.runtime_kind == TaskRuntimeKind::Native {
            return Err(TaskError::NativeRuntimeAlreadySelected(task_id));
        }
        if !matches!(task.status, TaskStatus::Created | TaskStatus::Starting) {
            return Err(TaskError::CannotBindTerminalInState {
                task_id,
                status: task.status,
            });
        }
        if let Some(existing_session_id) = &task.terminal_session_id {
            return Err(TaskError::TaskAlreadyBoundToTerminal {
                task_id,
                session_id: existing_session_id.clone(),
            });
        }
        if task.runtime_kind != TaskRuntimeKind::TerminalFallback {
            task.runtime_kind = TaskRuntimeKind::Terminal;
        }
        task.terminal_session_id = Some(session_id.clone());
        // PTY creation returns only after chdir + exec crossed the startup
        // pipe, so a successful binding is already a reliable Working signal.
        task.status = TaskStatus::Working;
        task.status_detail = None;
        task.updated_at_ms = unix_time_ms();
        self.tasks_by_terminal_session.insert(session_id, task_id);
        Ok(())
    }

    /// Return the old stable session only when its child exit was observed and
    /// every retry ownership gate currently passes. Binding repeats these
    /// checks after the replacement PTY has crossed exec.
    pub fn terminal_retry_session_id(&self, task_id: TaskId) -> Result<&str, TaskError> {
        if self.native_event_streams.contains_key(&task_id) {
            return Err(TaskError::NativeEventStreamActive(task_id));
        }
        let task = self.get(task_id).ok_or(TaskError::UnknownTask(task_id))?;
        let old_session = task.terminal_session_id.as_deref();
        if task.status != TaskStatus::Failed
            || !matches!(
                task.runtime_kind,
                TaskRuntimeKind::Terminal | TaskRuntimeKind::TerminalFallback
            )
            || task.validation.status == TaskValidationStatus::Running
            || old_session.is_none()
            || old_session.and_then(|session| self.tasks_by_terminal_session.get(session))
                != Some(&task_id)
            || old_session.is_none_or(|session| !self.exited_terminal_sessions.contains(session))
        {
            return Err(TaskError::TerminalRetryUnavailable {
                task_id,
                status: task.status,
            });
        }
        Ok(old_session.expect("retry eligibility checked the terminal session"))
    }

    /// Atomically replace a failed, authoritatively exited PTY with a newly
    /// spawned retry. Until this commit succeeds the old task↔transcript
    /// binding, Failed state, and exit status remain untouched.
    pub fn bind_terminal_retry_session(
        &mut self,
        task_id: TaskId,
        expected_old_session: &str,
        new_session_id: String,
    ) -> Result<(), TaskError> {
        if !jterm_core::execution_journal::is_valid_jsh_session_id(&new_session_id) {
            return Err(TaskError::InvalidTerminalSessionId);
        }
        if let Some(existing_task_id) = self
            .tasks_by_validation_session
            .get(&new_session_id)
            .copied()
            .or_else(|| self.tasks_by_terminal_session.get(&new_session_id).copied())
        {
            return Err(TaskError::TerminalSessionAlreadyBound {
                session_id: new_session_id,
                task_id: existing_task_id,
            });
        }
        let old_session = self.terminal_retry_session_id(task_id)?;
        if old_session != expected_old_session {
            let status = self
                .get(task_id)
                .map_or(TaskStatus::Failed, |task| task.status);
            return Err(TaskError::TerminalRetryUnavailable { task_id, status });
        }
        let runtime_kind = self
            .get(task_id)
            .expect("retry eligibility checked the task")
            .runtime_kind;

        let task = self.task_mut(task_id)?;
        task.terminal_session_id = Some(new_session_id.clone());
        task.exit_code = None;
        task.status = TaskStatus::Working;
        task.status_detail = None;
        task.updated_at_ms = unix_time_ms();
        debug_assert_eq!(task.runtime_kind, runtime_kind);
        self.tasks_by_terminal_session.remove(expected_old_session);
        self.exited_terminal_sessions.remove(expected_old_session);
        self.tasks_by_terminal_session
            .insert(new_session_id, task_id);
        Ok(())
    }

    /// Check whether a fully stopped native task can explicitly continue in a
    /// compatibility PTY. The runtime owner separately proves process stop;
    /// this domain gate deliberately performs no state transition before a new
    /// PTY has crossed exec.
    // The library target does not compile the desktop Tasks UI that consumes
    // this crate-private gate; keep it sealed despite that target-local fact.
    #[allow(dead_code)]
    pub(crate) fn native_terminal_fallback_eligible(
        &self,
        task_id: TaskId,
    ) -> Result<(), TaskError> {
        if self.native_event_streams.contains_key(&task_id) {
            return Err(TaskError::NativeEventStreamActive(task_id));
        }
        let task = self.get(task_id).ok_or(TaskError::UnknownTask(task_id))?;
        if !matches!(task.status, TaskStatus::Failed | TaskStatus::ReadyForReview)
            || task.runtime_kind != TaskRuntimeKind::Native
            || task.terminal_session_id.is_some()
            || task.validation.status == TaskValidationStatus::Running
        {
            return Err(TaskError::TerminalFallbackUnavailable {
                task_id,
                status: task.status,
            });
        }
        Ok(())
    }

    /// Atomically bind an already-spawned compatibility PTY to a stopped
    /// native task. Until this succeeds, the task's review point, validation,
    /// and native provenance remain unchanged. A successful bind makes the
    /// terminal fallback sticky and invalidates any older validation result.
    #[allow(dead_code)]
    pub(crate) fn bind_native_terminal_fallback_session(
        &mut self,
        task_id: TaskId,
        session_id: String,
    ) -> Result<(), TaskError> {
        if !jterm_core::execution_journal::is_valid_jsh_session_id(&session_id) {
            return Err(TaskError::InvalidTerminalSessionId);
        }
        if let Some(existing_task_id) = self
            .tasks_by_validation_session
            .get(&session_id)
            .copied()
            .or_else(|| self.tasks_by_terminal_session.get(&session_id).copied())
        {
            return Err(TaskError::TerminalSessionAlreadyBound {
                session_id,
                task_id: existing_task_id,
            });
        }
        self.native_terminal_fallback_eligible(task_id)?;
        let previous_validation = self
            .get(task_id)
            .map(|task| task.validation.clone())
            .ok_or(TaskError::UnknownTask(task_id))?;
        let invalidated_validation_session = previous_validation.terminal_session_id.clone();
        let task = self.task_mut(task_id)?;
        task.runtime_kind = TaskRuntimeKind::TerminalFallback;
        task.terminal_session_id = Some(session_id.clone());
        task.exit_code = None;
        task.status = TaskStatus::Working;
        task.validation = if previous_validation == TaskValidationState::default() {
            TaskValidationState::default()
        } else {
            TaskValidationState {
                attempt: previous_validation.attempt,
                status_detail: Some(
                    "Terminal continuation started; the previous validation result is stale"
                        .to_string(),
                ),
                ..TaskValidationState::default()
            }
        };
        task.status_detail =
            Some("Native Codex stopped; continuing in the terminal compatibility path".to_string());
        task.updated_at_ms = unix_time_ms();
        if let Some(validation_session) = invalidated_validation_session {
            if self.tasks_by_validation_session.get(&validation_session) == Some(&task_id) {
                self.tasks_by_validation_session.remove(&validation_session);
            }
        }
        self.tasks_by_terminal_session.insert(session_id, task_id);
        Ok(())
    }

    /// Bind a freshly spawned validation PTY to a task awaiting review.
    ///
    /// A completed attempt may be replaced explicitly. The old session lookup
    /// is removed only after every check for the new binding succeeds, so a
    /// rejected re-run leaves the previous result and correlation intact.
    pub fn bind_validation_session(
        &mut self,
        task_id: TaskId,
        session_id: String,
    ) -> Result<(), TaskError> {
        if !jterm_core::execution_journal::is_valid_jsh_session_id(&session_id) {
            return Err(TaskError::InvalidTerminalSessionId);
        }
        let next_attempt = self.next_validation_attempt(task_id)?;
        let old_session_id = self
            .get(task_id)
            .ok_or(TaskError::UnknownTask(task_id))?
            .validation
            .terminal_session_id
            .clone();

        if let Some(existing_task_id) = self.tasks_by_terminal_session.get(&session_id).copied() {
            return Err(TaskError::TerminalSessionAlreadyBound {
                session_id,
                task_id: existing_task_id,
            });
        }
        if let Some(existing_task_id) = self.tasks_by_validation_session.get(&session_id).copied() {
            return Err(TaskError::TerminalSessionAlreadyBound {
                session_id,
                task_id: existing_task_id,
            });
        }

        if let Some(old_session_id) = old_session_id.as_deref() {
            self.tasks_by_validation_session.remove(old_session_id);
        }
        let task = self.task_mut(task_id)?;
        task.validation = TaskValidationState {
            status: TaskValidationStatus::Running,
            attempt: next_attempt,
            terminal_session_id: Some(session_id.clone()),
            exit_code: None,
            status_detail: None,
        };
        task.updated_at_ms = unix_time_ms();
        self.tasks_by_validation_session.insert(session_id, task_id);
        Ok(())
    }

    /// Read-only eligibility check used before resolving a shell or spawning
    /// a PTY. Binding repeats every check after spawn so future concurrent
    /// callers cannot turn this advisory result into authority.
    pub fn next_validation_attempt(&self, task_id: TaskId) -> Result<u64, TaskError> {
        if self.native_event_streams.contains_key(&task_id) {
            return Err(TaskError::NativeEventStreamActiveDuringValidation(task_id));
        }
        let task = self.get(task_id).ok_or(TaskError::UnknownTask(task_id))?;
        if task.status != TaskStatus::ReadyForReview {
            return Err(TaskError::CannotBindValidationInState {
                task_id,
                status: task.status,
            });
        }
        if task.validation.status == TaskValidationStatus::Running {
            return Err(TaskError::ValidationAlreadyRunning {
                task_id,
                session_id: task
                    .validation
                    .terminal_session_id
                    .clone()
                    .unwrap_or_default(),
            });
        }
        task.validation
            .attempt
            .checked_add(1)
            .ok_or(TaskError::ValidationAttemptExhausted(task_id))
    }

    pub fn update_status(
        &mut self,
        task_id: TaskId,
        status: TaskStatus,
        detail: Option<String>,
    ) -> Result<(), TaskError> {
        if self.native_event_streams.contains_key(&task_id) {
            return Err(TaskError::NativeEventStreamActive(task_id));
        }
        if status == TaskStatus::Completed {
            return Err(TaskError::CompletionRequiresValidation(task_id));
        }
        let (current, validation_status, validation_session_id) = self
            .get(task_id)
            .map(|task| {
                (
                    task.status,
                    task.validation.status,
                    task.validation.terminal_session_id.clone(),
                )
            })
            .ok_or(TaskError::UnknownTask(task_id))?;
        if current.is_terminal() && current != status {
            return Err(TaskError::CannotLeaveTerminalState {
                task_id,
                current,
                requested: status,
            });
        }
        if validation_status == TaskValidationStatus::Running && status != current {
            return Err(TaskError::ValidationAlreadyRunning {
                task_id,
                session_id: validation_session_id.unwrap_or_default(),
            });
        }
        let invalidate_validation = status.is_running()
            && !matches!(
                validation_status,
                TaskValidationStatus::NotRun | TaskValidationStatus::Running
            );
        if invalidate_validation {
            if let Some(session_id) = validation_session_id.as_deref() {
                self.tasks_by_validation_session.remove(session_id);
            }
        }
        {
            let task = self.task_mut(task_id)?;
            if invalidate_validation {
                let attempt = task.validation.attempt;
                task.validation = TaskValidationState {
                    attempt,
                    status_detail: Some(
                        "Task work resumed; the previous validation result is stale".to_string(),
                    ),
                    ..TaskValidationState::default()
                };
            }
            task.status = status;
            task.status_detail = super::event::bounded_event_detail(detail);
            task.updated_at_ms = unix_time_ms();
        }
        if status.is_terminal() {
            self.native_event_streams.remove(&task_id);
        }
        Ok(())
    }

    /// Apply the authoritative child-process result before its tab disappears.
    /// A zero exit only means the opaque Agent process finished; its worktree
    /// still requires human review and is never treated as accepted/merged.
    pub fn handle_terminal_session_exit(
        &mut self,
        session_id: &str,
        exit_code: Option<i32>,
    ) -> Option<TaskId> {
        if let Some(task_id) = self.tasks_by_validation_session.get(session_id).copied() {
            let task = self.task_mut(task_id).ok()?;
            if task.validation.status != TaskValidationStatus::Running {
                return Some(task_id);
            }
            task.validation.exit_code = exit_code;
            match exit_code {
                Some(0) => {
                    task.validation.status = TaskValidationStatus::Passed;
                    task.validation.status_detail = Some("Validation passed".to_string());
                }
                Some(code) => {
                    task.validation.status = TaskValidationStatus::Failed;
                    task.validation.status_detail =
                        Some(format!("Validation process exited with code {code}"));
                }
                None => {
                    task.validation.status = TaskValidationStatus::Inconclusive;
                    task.validation.status_detail =
                        Some("Validation ended without an authoritative exit status".to_string());
                }
            }
            task.updated_at_ms = unix_time_ms();
            return Some(task_id);
        }
        let task_id = self.tasks_by_terminal_session.get(session_id).copied()?;
        self.exited_terminal_sessions.insert(session_id.to_string());
        let task = self.task_mut(task_id).ok()?;
        if matches!(
            task.status,
            TaskStatus::ReadyForReview
                | TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Archived
        ) {
            return Some(task_id);
        }
        task.exit_code = exit_code;
        match exit_code {
            Some(0) => {
                task.status = TaskStatus::ReadyForReview;
                task.status_detail =
                    Some("Agent process finished; review its worktree changes".to_string());
            }
            Some(code) => {
                task.status = TaskStatus::Failed;
                task.status_detail = Some(format!("Agent process exited with code {code}"));
            }
            None => {
                task.status = TaskStatus::Failed;
                task.status_detail = Some("Agent process ended without an exit status".to_string());
            }
        }
        task.updated_at_ms = unix_time_ms();
        Some(task_id)
    }

    /// Record an explicit UI/session close when no child wait status was
    /// observed. This is not a process failure and must not leave the task
    /// looking perpetually active in the dashboard.
    pub fn handle_terminal_session_closed(&mut self, session_id: &str) -> Option<TaskId> {
        if let Some(task_id) = self.tasks_by_validation_session.get(session_id).copied() {
            let task = self.task_mut(task_id).ok()?;
            if task.validation.status != TaskValidationStatus::Running {
                return Some(task_id);
            }
            task.validation.status = TaskValidationStatus::Cancelled;
            task.validation.exit_code = None;
            task.validation.status_detail = Some("Validation terminal was closed".to_string());
            task.updated_at_ms = unix_time_ms();
            return Some(task_id);
        }
        let task_id = self.tasks_by_terminal_session.get(session_id).copied()?;
        let task = self.task_mut(task_id).ok()?;
        if matches!(
            task.status,
            TaskStatus::ReadyForReview
                | TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Archived
        ) {
            return Some(task_id);
        }
        task.status = TaskStatus::Cancelled;
        task.exit_code = None;
        task.status_detail = Some("Agent terminal was closed".to_string());
        task.updated_at_ms = unix_time_ms();
        Some(task_id)
    }

    pub fn archive(&mut self, task_id: TaskId) -> Result<(), TaskError> {
        if self.native_event_streams.contains_key(&task_id) {
            return Err(TaskError::CannotArchiveRunning(task_id));
        }
        let task = self.task_mut(task_id)?;
        if task.status.is_running() || task.validation.status == TaskValidationStatus::Running {
            return Err(TaskError::CannotArchiveRunning(task_id));
        }
        task.status = TaskStatus::Archived;
        task.updated_at_ms = unix_time_ms();
        Ok(())
    }

    /// Accept a reviewed task only after its latest validation passed.
    pub fn complete_after_validation(&mut self, task_id: TaskId) -> Result<(), TaskError> {
        if self.native_event_streams.contains_key(&task_id) {
            return Err(TaskError::NativeEventStreamActive(task_id));
        }
        let task = self.task_mut(task_id)?;
        if task.status != TaskStatus::ReadyForReview
            || task.validation.status != TaskValidationStatus::Passed
        {
            return Err(TaskError::CannotCompleteAfterValidation {
                task_id,
                status: task.status,
                validation_status: task.validation.status,
            });
        }
        task.status = TaskStatus::Completed;
        task.status_detail = Some("Validation passed; task accepted".to_string());
        task.updated_at_ms = unix_time_ms();
        Ok(())
    }

    fn task_mut(&mut self, task_id: TaskId) -> Result<&mut AgentTask, TaskError> {
        let index = self
            .task_indices
            .get(&task_id)
            .copied()
            .ok_or(TaskError::UnknownTask(task_id))?;
        self.tasks
            .get_mut(index)
            .ok_or(TaskError::UnknownTask(task_id))
    }
}

fn validate_new_task(task: &NewTask) -> Result<(), TaskError> {
    let title = task.title.trim();
    if title.is_empty() || title.len() > MAX_TASK_TITLE_BYTES || title.chars().any(char::is_control)
    {
        return Err(TaskError::InvalidTitle);
    }
    if task.branch.is_empty()
        || task.branch.len() > MAX_BRANCH_BYTES
        || task.branch.chars().any(char::is_control)
    {
        return Err(TaskError::InvalidBranch);
    }
    if !matches!(task.base_commit.len(), 40 | 64)
        || !task
            .base_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(TaskError::InvalidBaseCommit);
    }
    if !task.repo_root.is_absolute() {
        return Err(TaskError::RepoRootMustBeAbsolute);
    }
    if !task.worktree_path.is_absolute() {
        return Err(TaskError::WorktreePathMustBeAbsolute);
    }
    if task.repo_root == task.worktree_path {
        return Err(TaskError::WorktreeMatchesRepoRoot);
    }
    if task.source_context.as_ref().is_some_and(|context| {
        !jterm_core::execution_journal::is_valid_jsh_session_id(&context.source_session_id)
            || context.source_execution_id.is_empty()
            || context.source_execution_id.len() > 256
            || context.source_execution_id.chars().any(char::is_control)
    }) {
        return Err(TaskError::InvalidSourceContext);
    }
    Ok(())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::SemanticCommandContext;
    use std::path::Path;

    fn new_task(title: &str) -> NewTask {
        NewTask {
            title: title.to_string(),
            provider: AgentProvider::Codex,
            repo_root: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/tasks/task-one"),
            branch: "anvil/task-one".to_string(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            source_context: Some(SemanticCommandContext {
                source_session_id: "source-session".to_string(),
                source_execution_id: "execution-7".to_string(),
                source_sequence: 7,
                source_shell: Some("/bin/bash".to_string()),
                command: Some("cargo test".to_string()),
                command_exact: true,
                command_truncated: false,
                cwd: Some("/repo".to_string()),
                cwd_after: Some("/repo".to_string()),
                exit_code: Some(101),
                duration_ms: Some(42),
                output_text: "test failed".to_string(),
                output_available: true,
                output_truncated: false,
                output_total_bytes: 11,
                started_at: None,
                finished_at: None,
            }),
        }
    }

    fn session_started() -> AgentEventKind {
        AgentEventKind::SessionStarted {
            provider_session_id: None,
            resumed: false,
        }
    }

    fn turn_started(turn_id: AgentTurnId) -> AgentEventKind {
        AgentEventKind::TurnStarted { turn_id }
    }

    fn approval_requested(turn_id: AgentTurnId) -> AgentEventKind {
        AgentEventKind::ApprovalRequested {
            turn_id,
            approval_id: ApprovalId::new(),
        }
    }

    fn turn_completed(turn_id: AgentTurnId) -> AgentEventKind {
        AgentEventKind::TurnCompleted { turn_id }
    }

    fn input_requested(turn_id: AgentTurnId) -> AgentEventKind {
        AgentEventKind::InputRequested { turn_id }
    }

    fn work_resumed(turn_id: AgentTurnId) -> AgentEventKind {
        AgentEventKind::WorkResumed { turn_id }
    }

    #[test]
    fn creates_owned_task_with_stable_identity() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("Fix resize crash")).unwrap();
        let task = manager.get(id).unwrap();

        assert_eq!(task.id, id);
        assert_eq!(task.title, "Fix resize crash");
        assert_eq!(task.status, TaskStatus::Created);
        assert_eq!(task.provider.display_name(), "Codex");
        assert_eq!(task.worktree_path, Path::new("/tasks/task-one"));
        assert_eq!(task.source.as_ref().unwrap().execution_id, "execution-7");
        assert_eq!(
            task.source_context.as_ref().unwrap().command.as_deref(),
            Some("cargo test")
        );

        let serialized = serde_json::to_string(task).expect("task serializes");
        let restored: AgentTask = serde_json::from_str(&serialized).expect("task restores");
        assert_eq!(restored, task.clone());
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_metadata() {
        let mut manager = TaskManager::new();
        let mut task = new_task("   ");
        assert_eq!(manager.create(task.clone()), Err(TaskError::InvalidTitle));

        task.title = "valid".to_string();
        task.branch = "bad\nbranch".to_string();
        assert_eq!(manager.create(task.clone()), Err(TaskError::InvalidBranch));

        task.branch = "anvil/valid".to_string();
        task.base_commit = "short".to_string();
        assert_eq!(
            manager.create(task.clone()),
            Err(TaskError::InvalidBaseCommit)
        );

        task.base_commit = "0123456789abcdef0123456789abcdef01234567".to_string();
        task.repo_root = PathBuf::from("relative");
        assert_eq!(
            manager.create(task.clone()),
            Err(TaskError::RepoRootMustBeAbsolute)
        );

        task.repo_root = PathBuf::from("/same");
        task.worktree_path = PathBuf::from("/same");
        assert_eq!(
            manager.create(task),
            Err(TaskError::WorktreeMatchesRepoRoot)
        );
    }

    #[test]
    fn stable_session_binding_survives_unrelated_ui_reindexing() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("task")).unwrap();
        manager
            .bind_terminal_session(id, "stable-session-42".to_string())
            .unwrap();

        assert_eq!(
            manager
                .task_for_terminal_session("stable-session-42")
                .map(|task| task.id),
            Some(id)
        );
        assert!(manager.task_for_terminal_session("pane-index-0").is_none());
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Working);
    }

    #[test]
    fn session_cannot_be_silently_rebound() {
        let mut manager = TaskManager::new();
        let first = manager.create(new_task("first")).unwrap();
        let mut second_task = new_task("second");
        second_task.worktree_path = PathBuf::from("/tasks/task-two");
        second_task.branch = "anvil/task-two".to_string();
        let second = manager.create(second_task).unwrap();

        manager
            .bind_terminal_session(first, "agent-session".to_string())
            .unwrap();
        assert_eq!(
            manager.bind_terminal_session(second, "agent-session".to_string()),
            Err(TaskError::TerminalSessionAlreadyBound {
                session_id: "agent-session".to_string(),
                task_id: first,
            })
        );
    }

    #[test]
    fn validation_state_defaults_and_serializes_compatibly() {
        assert_eq!(
            TaskValidationState::default().status,
            TaskValidationStatus::NotRun
        );
        assert_eq!(TaskValidationStatus::Running.label(), "Running");
        assert!(!TaskValidationStatus::Passed.needs_attention());
        assert!(TaskValidationStatus::Failed.needs_attention());
        assert_eq!(TaskValidationStatus::Inconclusive.label(), "Needs review");
        assert!(TaskValidationStatus::Inconclusive.needs_attention());

        let mut manager = TaskManager::new();
        let id = manager.create(new_task("legacy validation state")).unwrap();
        let serialized = serde_json::to_value(manager.get(id).unwrap()).unwrap();
        let mut object = serialized.as_object().unwrap().clone();
        object.remove("validation");
        let restored: AgentTask = serde_json::from_value(object.into()).unwrap();
        assert_eq!(restored.validation, TaskValidationState::default());
    }

    #[test]
    fn validation_passes_without_accepting_task_and_can_then_complete() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("validate success")).unwrap();
        manager
            .update_status(id, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(id, "validation-success".to_string())
            .unwrap();

        let task = manager.get(id).unwrap();
        assert_eq!(task.validation.status, TaskValidationStatus::Running);
        assert_eq!(task.validation.attempt, 1);
        assert!(!task.needs_attention());
        assert_eq!(
            manager.terminal_role_for_session("validation-success"),
            Some(TaskTerminalRole::Validation)
        );
        assert_eq!(
            manager
                .task_for_terminal_session("validation-success")
                .map(|task| task.id),
            Some(id)
        );

        assert_eq!(
            manager.handle_terminal_session_exit("validation-success", Some(0)),
            Some(id)
        );
        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::ReadyForReview);
        assert_eq!(task.validation.status, TaskValidationStatus::Passed);
        assert_eq!(task.validation.exit_code, Some(0));
        assert!(task.needs_attention());

        manager.complete_after_validation(id).unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Completed);
    }

    #[test]
    fn validation_failure_and_close_are_recorded_without_changing_task_status() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("validate failure")).unwrap();
        manager
            .update_status(id, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(id, "validation-failure".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("validation-failure", Some(9));
        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::ReadyForReview);
        assert_eq!(task.validation.status, TaskValidationStatus::Failed);
        assert_eq!(task.validation.exit_code, Some(9));
        assert!(task
            .validation
            .status_detail
            .as_deref()
            .unwrap()
            .contains('9'));

        manager
            .bind_validation_session(id, "validation-inconclusive".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("validation-inconclusive", None);
        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::ReadyForReview);
        assert_eq!(task.validation.status, TaskValidationStatus::Inconclusive);
        assert_eq!(task.validation.exit_code, None);

        manager
            .bind_validation_session(id, "validation-cancelled".to_string())
            .unwrap();
        assert_eq!(
            manager.handle_terminal_session_closed("validation-cancelled"),
            Some(id)
        );
        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::ReadyForReview);
        assert_eq!(task.validation.status, TaskValidationStatus::Cancelled);
        assert_eq!(task.validation.exit_code, None);

        // A later child notification cannot overwrite the explicit close.
        manager.handle_terminal_session_exit("validation-cancelled", Some(0));
        assert_eq!(
            manager.get(id).unwrap().validation.status,
            TaskValidationStatus::Cancelled
        );
    }

    #[test]
    fn validation_rerun_replaces_only_its_previous_stable_mapping() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("rerun validation")).unwrap();
        manager
            .update_status(id, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(id, "validation-one".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("validation-one", Some(1));
        assert_eq!(
            manager.bind_validation_session(id, "validation-one".to_string()),
            Err(TaskError::TerminalSessionAlreadyBound {
                session_id: "validation-one".to_string(),
                task_id: id,
            })
        );
        manager
            .bind_validation_session(id, "validation-two".to_string())
            .unwrap();

        let task = manager.get(id).unwrap();
        assert_eq!(task.validation.attempt, 2);
        assert_eq!(
            task.validation.terminal_session_id.as_deref(),
            Some("validation-two")
        );
        assert!(manager
            .task_for_terminal_session("validation-one")
            .is_none());
        assert_eq!(
            manager
                .task_for_terminal_session("validation-two")
                .map(|task| task.id),
            Some(id)
        );
        assert_eq!(
            manager.handle_terminal_session_exit("validation-one", Some(0)),
            None
        );
        assert_eq!(
            manager.get(id).unwrap().validation.status,
            TaskValidationStatus::Running
        );
    }

    #[test]
    fn validation_binding_checks_state_running_collisions_and_attempt_overflow() {
        let mut manager = TaskManager::new();
        let first = manager.create(new_task("validation gates")).unwrap();
        assert_eq!(
            manager.bind_validation_session(first, "too-early".to_string()),
            Err(TaskError::CannotBindValidationInState {
                task_id: first,
                status: TaskStatus::Created,
            })
        );

        manager
            .update_status(first, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(first, "validation-running".to_string())
            .unwrap();
        assert_eq!(
            manager.bind_validation_session(first, "validation-other".to_string()),
            Err(TaskError::ValidationAlreadyRunning {
                task_id: first,
                session_id: "validation-running".to_string(),
            })
        );
        assert_eq!(
            manager.archive(first),
            Err(TaskError::CannotArchiveRunning(first))
        );

        let mut second_task = new_task("validation collision");
        second_task.worktree_path = PathBuf::from("/tasks/validation-collision");
        second_task.branch = "anvil/validation-collision".to_string();
        let second = manager.create(second_task).unwrap();
        assert_eq!(
            manager.bind_terminal_session(second, "validation-running".to_string()),
            Err(TaskError::TerminalSessionAlreadyBound {
                session_id: "validation-running".to_string(),
                task_id: first,
            })
        );
        manager
            .update_status(second, TaskStatus::ReadyForReview, None)
            .unwrap();
        assert_eq!(
            manager.bind_validation_session(second, "validation-running".to_string()),
            Err(TaskError::TerminalSessionAlreadyBound {
                session_id: "validation-running".to_string(),
                task_id: first,
            })
        );

        let mut agent_task = new_task("Agent session collision");
        agent_task.worktree_path = PathBuf::from("/tasks/agent-session-collision");
        agent_task.branch = "anvil/agent-session-collision".to_string();
        let agent_task = manager.create(agent_task).unwrap();
        manager
            .bind_terminal_session(agent_task, "agent-owned".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("agent-owned", Some(0));
        assert_eq!(
            manager.bind_validation_session(agent_task, "agent-owned".to_string()),
            Err(TaskError::TerminalSessionAlreadyBound {
                session_id: "agent-owned".to_string(),
                task_id: agent_task,
            })
        );

        manager.handle_terminal_session_exit("validation-running", Some(0));
        manager.task_mut(first).unwrap().validation.attempt = u64::MAX;
        assert_eq!(
            manager.bind_validation_session(first, "attempt-overflow".to_string()),
            Err(TaskError::ValidationAttemptExhausted(first))
        );
        assert_eq!(
            manager
                .task_for_terminal_session("validation-running")
                .map(|task| task.id),
            Some(first)
        );
    }

    #[test]
    fn validation_waits_until_a_native_event_stream_has_ended() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("native validation gate")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let turn = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(2, turn_started(turn), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(3, approval_requested(turn), None))
            .unwrap();
        assert!(manager.task_needs_attention(id));
        assert_eq!(manager.attention_count(), 1);
        manager
            .apply_agent_event(stream.event(4, work_resumed(turn), None))
            .unwrap();
        assert!(!manager.task_needs_attention(id));
        assert_eq!(manager.attention_count(), 0);
        manager
            .apply_agent_event(stream.event(5, turn_completed(turn), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);
        assert!(manager.task_needs_attention(id));
        assert_eq!(manager.attention_count(), 1);
        assert_eq!(
            manager.next_validation_attempt(id),
            Err(TaskError::NativeEventStreamActiveDuringValidation(id))
        );
        assert_eq!(
            manager.bind_validation_session(id, "validation-too-soon".to_string()),
            Err(TaskError::NativeEventStreamActiveDuringValidation(id))
        );

        // A later turn may fail after earlier useful work reached review. The
        // stopped session must preserve that review point instead of hiding
        // the accumulated worktree diff behind a terminal Failed state.
        let third_turn = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(6, turn_started(third_turn), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Working);
        manager
            .apply_agent_event(stream.event(
                7,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Failed,
                },
                Some("third turn failed".into()),
            ))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);
        assert_eq!(
            manager.get(id).unwrap().status_detail.as_deref(),
            Some("third turn failed")
        );
        assert!(manager.task_needs_attention(id));
        assert_eq!(manager.attention_count(), 1);
        manager
            .bind_validation_session(id, "validation-after-stop".to_string())
            .unwrap();
        assert_eq!(
            manager.get(id).unwrap().validation.status,
            TaskValidationStatus::Running
        );
        assert_eq!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::ValidationActive(id))
        );
    }

    #[test]
    fn one_shot_native_task_cannot_resume_after_validation() {
        let mut manager = TaskManager::new();
        let id = manager
            .create(new_task("invalidate stale validation"))
            .unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let turn = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(2, turn_started(turn), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(3, turn_completed(turn), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(
                4,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Clean,
                },
                None,
            ))
            .unwrap();
        manager
            .bind_validation_session(id, "validation-before-resume".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("validation-before-resume", Some(0));
        assert_eq!(
            manager.get(id).unwrap().validation.status,
            TaskValidationStatus::Passed
        );

        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::NativeStartRequiresCreated {
                task_id,
                status: TaskStatus::ReadyForReview,
            }) if task_id == id
        ));

        let validation = &manager.get(id).unwrap().validation;
        assert_eq!(validation.status, TaskValidationStatus::Passed);
        assert_eq!(validation.attempt, 1);
        assert_eq!(
            validation.terminal_session_id.as_deref(),
            Some("validation-before-resume")
        );
        assert!(manager
            .task_for_terminal_session("validation-before-resume")
            .is_some());
    }

    #[test]
    fn generic_work_resume_invalidates_validation_and_cannot_bypass_completion() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("generic validation gate")).unwrap();
        manager
            .update_status(id, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(id, "validation-generic".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("validation-generic", Some(0));

        assert_eq!(
            manager.update_status(id, TaskStatus::Completed, None),
            Err(TaskError::CompletionRequiresValidation(id))
        );
        manager
            .update_status(id, TaskStatus::Working, Some("more work".to_string()))
            .unwrap();
        let task = manager.get(id).unwrap();
        assert_eq!(task.validation.status, TaskValidationStatus::NotRun);
        assert_eq!(task.validation.attempt, 1);
        assert!(task
            .validation
            .status_detail
            .as_deref()
            .unwrap()
            .contains("stale"));
        assert!(manager
            .task_for_terminal_session("validation-generic")
            .is_none());
    }

    #[test]
    fn process_exit_becomes_review_or_failure_before_session_removal() {
        let mut manager = TaskManager::new();
        let success = manager.create(new_task("success")).unwrap();
        manager
            .bind_terminal_session(success, "success-session".to_string())
            .unwrap();
        assert_eq!(
            manager.handle_terminal_session_exit("success-session", Some(0)),
            Some(success)
        );
        let task = manager.get(success).unwrap();
        assert_eq!(task.status, TaskStatus::ReadyForReview);
        assert_eq!(task.exit_code, Some(0));
        assert!(task.needs_attention());

        let mut failed_task = new_task("failed");
        failed_task.worktree_path = PathBuf::from("/tasks/failed");
        failed_task.branch = "anvil/failed".to_string();
        let failed = manager.create(failed_task).unwrap();
        manager
            .bind_terminal_session(failed, "failed-session".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("failed-session", Some(17));
        let task = manager.get(failed).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.exit_code, Some(17));
        assert!(task.status_detail.as_deref().unwrap().contains("17"));
    }

    #[test]
    fn failed_direct_terminal_can_retry_without_changing_runtime_provenance() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("direct terminal retry")).unwrap();
        manager
            .bind_terminal_session(id, "direct-terminal-first".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("direct-terminal-first", Some(7));

        let failed = manager.get(id).unwrap();
        assert_eq!(failed.status, TaskStatus::Failed);
        assert_eq!(failed.runtime_kind, TaskRuntimeKind::Terminal);
        assert_eq!(failed.exit_code, Some(7));
        assert_eq!(
            manager
                .task_for_terminal_session("direct-terminal-first")
                .map(|task| task.id),
            Some(id)
        );
        manager
            .bind_terminal_retry_session(
                id,
                "direct-terminal-first",
                "direct-terminal-second".to_string(),
            )
            .unwrap();
        let rebound = manager.get(id).unwrap();
        assert_eq!(rebound.status, TaskStatus::Working);
        assert_eq!(rebound.runtime_kind, TaskRuntimeKind::Terminal);
        assert_eq!(rebound.exit_code, None);
        assert!(manager
            .task_for_terminal_session("direct-terminal-first")
            .is_none());
        assert_eq!(
            manager
                .task_for_terminal_session("direct-terminal-second")
                .map(|task| task.id),
            Some(id)
        );
    }

    #[test]
    fn terminal_retry_requires_an_authoritative_exit_and_preserves_old_binding_on_failure() {
        let mut manager = TaskManager::new();
        let id = manager
            .create(new_task("terminal retry authority"))
            .unwrap();
        manager
            .bind_terminal_session(id, "terminal-still-live".to_string())
            .unwrap();
        manager
            .update_status(id, TaskStatus::Failed, Some("synthetic failure".into()))
            .unwrap();
        let before = manager.get(id).unwrap().clone();

        assert!(matches!(
            manager.terminal_retry_session_id(id),
            Err(TaskError::TerminalRetryUnavailable { task_id, .. }) if task_id == id
        ));
        assert!(matches!(
            manager.bind_terminal_retry_session(
                id,
                "terminal-still-live",
                "terminal-must-not-bind".to_string(),
            ),
            Err(TaskError::TerminalRetryUnavailable { task_id, .. }) if task_id == id
        ));
        assert_eq!(manager.get(id), Some(&before));
        assert_eq!(
            manager
                .task_for_terminal_session("terminal-still-live")
                .map(|task| task.id),
            Some(id)
        );
        assert!(manager
            .task_for_terminal_session("terminal-must-not-bind")
            .is_none());
    }

    #[test]
    fn disconnect_without_wait_status_fails_closed() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("disconnected")).unwrap();
        manager
            .bind_terminal_session(id, "disconnected-session".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("disconnected-session", None);

        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.exit_code, None);
        assert!(task.status_detail.as_deref().unwrap().contains("without"));
    }

    #[test]
    fn active_task_cannot_be_archived() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("active")).unwrap();
        manager
            .bind_terminal_session(id, "active-session".to_string())
            .unwrap();

        assert_eq!(
            manager.archive(id),
            Err(TaskError::CannotArchiveRunning(id))
        );
        manager.handle_terminal_session_exit("active-session", Some(0));
        manager.archive(id).unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Archived);
    }

    #[test]
    fn manually_closed_agent_session_becomes_cancelled() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("cancelled")).unwrap();
        manager
            .bind_terminal_session(id, "cancelled-session".to_string())
            .unwrap();

        assert_eq!(
            manager.handle_terminal_session_closed("cancelled-session"),
            Some(id)
        );
        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert_eq!(task.exit_code, None);

        // A later channel disconnect must not overwrite the explicit reason.
        manager.handle_terminal_session_exit("cancelled-session", None);
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Cancelled);
    }

    #[test]
    fn native_session_ids_are_generated_locally_bounded_and_diagnostic_safe() {
        let first = NativeAgentSessionId::new();
        let second = NativeAgentSessionId::new();
        assert_ne!(first, second);
        assert!(!first.as_str().is_empty());
        assert!(first.as_str().len() <= MAX_NATIVE_AGENT_SESSION_ID_BYTES);
        for invalid in ["", "has space", "line\nbreak", "spoof\u{202e}id"] {
            assert!(NativeAgentSessionId::parse(invalid).is_err(), "{invalid:?}");
        }
        assert!(
            NativeAgentSessionId::parse("x".repeat(MAX_NATIVE_AGENT_SESSION_ID_BYTES + 1)).is_err()
        );
    }

    #[test]
    fn normalized_events_reduce_lifecycle_without_retaining_content_payloads() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("native lifecycle")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let turn_id = AgentTurnId::new();

        assert_eq!(
            manager
                .apply_agent_event(stream.event(1, session_started(), Some("connecting".into()),))
                .unwrap(),
            TaskStatus::Starting
        );
        assert_eq!(
            manager
                .apply_agent_event(stream.event(2, turn_started(turn_id), Some("thinking".into()),))
                .unwrap(),
            TaskStatus::Working
        );
        manager
            .apply_agent_event(stream.event(
                3,
                AgentEventKind::TextDelta,
                Some("untrusted transcript chunk".into()),
            ))
            .unwrap();
        assert_eq!(
            manager.get(id).unwrap().status_detail.as_deref(),
            Some("thinking")
        );

        manager
            .apply_agent_event(stream.event(
                4,
                approval_requested(turn_id),
                Some("run tests?".into()),
            ))
            .unwrap();
        assert_eq!(
            manager.get(id).unwrap().status,
            TaskStatus::WaitingForApproval
        );
        // Incidental output is ordered and consumed, but cannot hide an
        // outstanding request for attention.
        manager
            .apply_agent_event(stream.event(
                5,
                AgentEventKind::CommandOutput,
                Some("still draining".into()),
            ))
            .unwrap();
        assert_eq!(
            manager.get(id).unwrap().status,
            TaskStatus::WaitingForApproval
        );
        manager
            .apply_agent_event(stream.event(6, work_resumed(turn_id), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(
                7,
                input_requested(turn_id),
                Some("choose a target".into()),
            ))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::WaitingForHuman);
        manager
            .apply_agent_event(stream.event(8, work_resumed(turn_id), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(
                9,
                turn_completed(turn_id),
                Some("review changes".into()),
            ))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);
    }

    #[test]
    fn event_sequence_is_contiguous_and_rejections_do_not_consume_it() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("ordered events")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let turn_id = AgentTurnId::new();

        assert!(matches!(
            manager.apply_agent_event(stream.event(2, session_started(), None)),
            Err(AgentEventError::InvalidSequence {
                expected: 1,
                received: 2,
                ..
            })
        ));
        assert!(matches!(
            manager.apply_agent_event(stream.event(1, turn_started(turn_id), None)),
            Err(AgentEventError::SessionNotStarted(task_id)) if task_id == id
        ));
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        assert!(matches!(
            manager.apply_agent_event(stream.event(1, session_started(), None)),
            Err(AgentEventError::InvalidSequence {
                expected: 2,
                received: 1,
                ..
            })
        ));
        assert!(matches!(
            manager.apply_agent_event(stream.event(3, turn_started(turn_id), None)),
            Err(AgentEventError::InvalidSequence {
                expected: 2,
                received: 3,
                ..
            })
        ));
        assert_eq!(
            manager
                .apply_agent_event(stream.event(2, turn_started(turn_id), None))
                .unwrap(),
            TaskStatus::Working
        );
    }

    #[test]
    fn session_start_validates_provider_namespace_and_resume_identity() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("provider identity")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let claude = ProviderSessionId::new(AgentProvider::Claude, "claude-thread").unwrap();
        assert!(matches!(
            manager.apply_agent_event(stream.event(
                1,
                AgentEventKind::SessionStarted {
                    provider_session_id: Some(claude),
                    resumed: false,
                },
                None,
            )),
            Err(AgentEventError::ProviderMismatch {
                task_id,
                expected: AgentProvider::Codex,
                received: AgentProvider::Claude,
            }) if task_id == id
        ));
        assert!(matches!(
            manager.apply_agent_event(stream.event(
                1,
                AgentEventKind::SessionStarted {
                    provider_session_id: None,
                    resumed: true,
                },
                None,
            )),
            Err(AgentEventError::MissingResumeSession(task_id)) if task_id == id
        ));

        let codex = ProviderSessionId::new(AgentProvider::Codex, "codex-thread").unwrap();
        manager
            .apply_agent_event(stream.event(
                1,
                AgentEventKind::SessionStarted {
                    provider_session_id: Some(codex),
                    resumed: true,
                },
                None,
            ))
            .unwrap();
    }

    #[test]
    fn turn_and_approval_events_must_match_the_active_turn() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("turn correlation")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let active_turn = AgentTurnId::new();
        let stale_turn = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(2, turn_started(active_turn), None))
            .unwrap();
        assert!(matches!(
            manager.apply_agent_event(stream.event(3, approval_requested(stale_turn), None)),
            Err(AgentEventError::TurnMismatch {
                task_id,
                expected,
                received,
            }) if task_id == id && expected == active_turn && received == stale_turn
        ));
        manager
            .apply_agent_event(stream.event(3, approval_requested(active_turn), None))
            .unwrap();
        assert!(matches!(
            manager.apply_agent_event(stream.event(4, turn_completed(stale_turn), None)),
            Err(AgentEventError::TurnMismatch { .. })
        ));
        manager
            .apply_agent_event(stream.event(4, work_resumed(active_turn), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(5, turn_completed(active_turn), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);

        let next_turn = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(6, turn_started(next_turn), None))
            .unwrap();
        assert!(matches!(
            manager.apply_agent_event(stream.event(7, turn_started(AgentTurnId::new()), None)),
            Err(AgentEventError::TurnAlreadyActive { task_id, turn_id })
                if task_id == id && turn_id == next_turn
        ));
    }

    #[test]
    fn active_stream_runs_multiple_turns_and_invalidates_the_prior_review_point() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("multi-turn stream")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let first_turn = AgentTurnId::new();
        let second_turn = AgentTurnId::new();

        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Starting);
        manager
            .apply_agent_event(stream.event(2, turn_started(first_turn), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Working);
        manager
            .apply_agent_event(stream.event(3, turn_completed(first_turn), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);
        assert!(manager.task_needs_attention(id));
        assert_eq!(
            manager.next_validation_attempt(id),
            Err(TaskError::NativeEventStreamActiveDuringValidation(id))
        );

        // Model a retained result from an earlier review point. Public
        // validation entry points correctly refuse this while the stream is
        // active, but TurnStarted must still fail closed if restored or future
        // orchestration ever presents such state.
        manager.task_mut(id).unwrap().validation = TaskValidationState {
            status: TaskValidationStatus::Passed,
            attempt: 3,
            terminal_session_id: Some("validation-before-second-turn".to_string()),
            exit_code: Some(0),
            status_detail: Some("Validation passed".to_string()),
        };
        manager
            .tasks_by_validation_session
            .insert("validation-before-second-turn".to_string(), id);

        // A late event from the completed turn has no authority and does not
        // consume sequence 4 or invalidate its validation result.
        assert!(matches!(
            manager.apply_agent_event(stream.event(4, turn_completed(first_turn), None)),
            Err(AgentEventError::NoActiveTurn(task_id)) if task_id == id
        ));
        assert_eq!(
            manager.get(id).unwrap().validation.status,
            TaskValidationStatus::Passed
        );
        assert!(manager
            .task_for_terminal_session("validation-before-second-turn")
            .is_some());

        manager
            .apply_agent_event(stream.event(4, turn_started(second_turn), None))
            .unwrap();
        let working = manager.get(id).unwrap();
        assert_eq!(working.status, TaskStatus::Working);
        assert_eq!(working.validation.status, TaskValidationStatus::NotRun);
        assert_eq!(working.validation.attempt, 3);
        assert!(working.validation.terminal_session_id.is_none());
        assert!(working
            .validation
            .status_detail
            .as_deref()
            .unwrap()
            .contains("previous validation result is stale"));
        assert!(manager
            .task_for_terminal_session("validation-before-second-turn")
            .is_none());

        // Once turn two is active, delayed turn-one traffic is rejected by
        // identity. The valid event at the same sequence proves the rejection
        // did not advance the stream cursor.
        assert!(matches!(
            manager.apply_agent_event(stream.event(5, approval_requested(first_turn), None)),
            Err(AgentEventError::TurnMismatch {
                task_id,
                expected,
                received,
            }) if task_id == id && expected == second_turn && received == first_turn
        ));
        manager
            .apply_agent_event(stream.event(5, turn_completed(second_turn), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);
        assert!(manager.task_needs_attention(id));
        assert_eq!(
            manager.bind_validation_session(id, "validation-too-soon-multi".to_string()),
            Err(TaskError::NativeEventStreamActiveDuringValidation(id))
        );

        manager
            .apply_agent_event(stream.event(
                6,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Clean,
                },
                None,
            ))
            .unwrap();
        assert!(manager.task_needs_attention(id));
        assert_eq!(manager.next_validation_attempt(id), Ok(4));
        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::NativeStartRequiresCreated {
                task_id,
                status: TaskStatus::ReadyForReview,
            }) if task_id == id
        ));
    }

    #[test]
    fn replacement_stream_uses_fresh_identity_and_rejects_stale_callers() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("correlated events")).unwrap();
        let old = manager.start_agent_event_stream(id).unwrap();
        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::StreamAlreadyActive(task_id)) if task_id == id
        ));
        let replacement = manager.replace_agent_event_stream_after_stop(&old).unwrap();
        assert_ne!(old.epoch(), replacement.epoch());
        assert_eq!(old.session_id(), replacement.session_id());
        assert!(matches!(
            manager.apply_agent_event(old.event(1, session_started(), None)),
            Err(AgentEventError::EpochMismatch { .. })
        ));
        assert!(matches!(
            manager.replace_agent_event_stream_after_stop(&old),
            Err(AgentEventError::EpochMismatch { .. })
        ));
        manager
            .apply_agent_event(replacement.event(1, session_started(), None))
            .unwrap();

        let different_session = manager
            .replace_agent_event_stream_after_stop(&replacement)
            .unwrap();
        assert!(matches!(
            manager.apply_agent_event(replacement.event(
                2,
                turn_started(AgentTurnId::new()),
                None,
            )),
            Err(AgentEventError::EpochMismatch { task_id, .. }) if task_id == id
        ));
        manager
            .apply_agent_event(different_session.event(1, session_started(), None))
            .unwrap();
    }

    #[test]
    fn pre_spawn_native_rollback_restores_retryable_created_task() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("pre-spawn rollback")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();

        manager
            .rollback_agent_event_stream_before_spawn(
                &stream,
                "could not create worker thread".into(),
            )
            .unwrap();
        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::Created);
        assert_eq!(task.runtime_kind, TaskRuntimeKind::Unassigned);
        assert!(!manager.has_active_agent_event_stream(id));
        assert_eq!(
            task.status_detail.as_deref(),
            Some("could not create worker thread")
        );

        let replacement = manager.start_agent_event_stream(id).unwrap();
        assert_ne!(stream.epoch(), replacement.epoch());
        assert!(matches!(
            manager.rollback_agent_event_stream_before_spawn(&stream, "stale".into()),
            Err(AgentEventError::EpochMismatch { .. })
        ));
    }

    #[test]
    fn terminal_and_native_runtime_bindings_are_mutually_exclusive() {
        let mut terminal_manager = TaskManager::new();
        let terminal_task = terminal_manager.create(new_task("terminal mode")).unwrap();
        terminal_manager
            .bind_terminal_session(terminal_task, "terminal-mode".to_string())
            .unwrap();
        assert!(matches!(
            terminal_manager.start_agent_event_stream(terminal_task),
            Err(AgentEventError::TerminalSessionBound(task_id)) if task_id == terminal_task
        ));

        let mut native_manager = TaskManager::new();
        let native_task = native_manager.create(new_task("native mode")).unwrap();
        native_manager
            .start_agent_event_stream(native_task)
            .unwrap();
        assert_eq!(
            native_manager.get(native_task).unwrap().runtime_kind,
            TaskRuntimeKind::Native
        );
        assert_eq!(
            native_manager.get(native_task).unwrap().status,
            TaskStatus::Starting
        );
        assert_eq!(
            native_manager.update_status(
                native_task,
                TaskStatus::Working,
                Some("bypass reducer".into()),
            ),
            Err(TaskError::NativeEventStreamActive(native_task))
        );
        assert_eq!(
            native_manager.bind_terminal_session(native_task, "terminal-after-native".to_string()),
            Err(TaskError::NativeRuntimeAlreadySelected(native_task))
        );
    }

    #[test]
    fn runtime_selection_survives_stream_end_and_terminal_states_cannot_be_revived() {
        let mut manager = TaskManager::new();
        let native_task = manager.create(new_task("native remains native")).unwrap();
        let stream = manager.start_agent_event_stream(native_task).unwrap();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(
                2,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Clean,
                },
                None,
            ))
            .unwrap();
        assert_eq!(
            manager.get(native_task).unwrap().status,
            TaskStatus::ReadyForReview
        );
        assert_eq!(
            manager.bind_terminal_session(native_task, "late-terminal".to_string()),
            Err(TaskError::NativeRuntimeAlreadySelected(native_task))
        );

        let mut failed_task = new_task("failed stays failed");
        failed_task.worktree_path = PathBuf::from("/tasks/failed-terminal-bind");
        failed_task.branch = "anvil/failed-terminal-bind".to_string();
        let failed_task = manager.create(failed_task).unwrap();
        manager
            .update_status(failed_task, TaskStatus::Failed, Some("failed".into()))
            .unwrap();
        assert_eq!(
            manager.bind_terminal_session(failed_task, "revive-failed".to_string()),
            Err(TaskError::CannotBindTerminalInState {
                task_id: failed_task,
                status: TaskStatus::Failed,
            })
        );
    }

    #[test]
    fn stopped_failed_native_task_can_explicitly_continue_in_terminal() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("native fallback")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        manager
            .apply_agent_event(stream.event(
                1,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Failed,
                },
                Some("app-server startup failed".into()),
            ))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Failed);
        assert_eq!(
            manager.get(id).unwrap().runtime_kind,
            TaskRuntimeKind::Native
        );

        manager.native_terminal_fallback_eligible(id).unwrap();
        manager
            .bind_native_terminal_fallback_session(id, "native-fallback-terminal".to_string())
            .unwrap();
        let recovered = manager.get(id).unwrap();
        assert_eq!(recovered.status, TaskStatus::Working);
        assert_eq!(recovered.runtime_kind, TaskRuntimeKind::TerminalFallback);
        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::TerminalSessionBound(task_id)) if task_id == id
        ));
        manager.handle_terminal_session_exit("native-fallback-terminal", Some(7));
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Failed);
        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::TerminalState { task_id, status: TaskStatus::Failed })
                if task_id == id
        ));
        manager
            .bind_terminal_retry_session(
                id,
                "native-fallback-terminal",
                "native-fallback-terminal-2".to_string(),
            )
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Working);
        assert_eq!(
            manager.get(id).unwrap().runtime_kind,
            TaskRuntimeKind::TerminalFallback
        );
        assert!(manager
            .task_for_terminal_session("native-fallback-terminal")
            .is_none());
        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::TerminalSessionBound(task_id)) if task_id == id
        ));
    }

    #[test]
    fn stopped_native_review_can_atomically_fallback_and_invalidate_validation() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("native review fallback")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let turn = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(2, turn_started(turn), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(3, turn_completed(turn), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(
                4,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Failed,
                },
                Some("later provider failure".into()),
            ))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);

        manager
            .bind_validation_session(id, "review-validation".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("review-validation", Some(0));
        assert_eq!(
            manager.get(id).unwrap().validation.status,
            TaskValidationStatus::Passed
        );
        manager.native_terminal_fallback_eligible(id).unwrap();
        manager
            .bind_native_terminal_fallback_session(id, "review-fallback".to_string())
            .unwrap();

        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::Working);
        assert_eq!(task.runtime_kind, TaskRuntimeKind::TerminalFallback);
        assert_eq!(task.terminal_session_id.as_deref(), Some("review-fallback"));
        assert_eq!(task.validation.status, TaskValidationStatus::NotRun);
        assert_eq!(task.validation.attempt, 1);
        assert!(manager
            .task_for_terminal_session("review-validation")
            .is_none());
        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::StreamAlreadyActive(task_id))
                | Err(AgentEventError::TerminalSessionBound(task_id)) if task_id == id
        ));
    }

    #[test]
    fn active_native_stream_cannot_be_archived_from_ready_for_review() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("active review")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let turn_id = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(2, turn_started(turn_id), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(3, turn_completed(turn_id), None))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);
        assert_eq!(
            manager.archive(id),
            Err(TaskError::CannotArchiveRunning(id))
        );
        assert_eq!(
            manager
                .apply_agent_event(stream.event(
                    4,
                    AgentEventKind::Cancelled,
                    Some("stopped after turn".into()),
                ))
                .unwrap(),
            TaskStatus::ReadyForReview
        );
        manager.archive(id).unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Archived);
    }

    #[test]
    fn explicit_reconnect_preserves_working_and_waiting_state() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("native reconnect")).unwrap();
        let first = manager.start_agent_event_stream(id).unwrap();
        let turn_id = AgentTurnId::new();
        manager
            .apply_agent_event(first.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(first.event(2, turn_started(turn_id), None))
            .unwrap();

        let working = manager
            .replace_agent_event_stream_after_stop(&first)
            .unwrap();
        assert_eq!(
            manager
                .apply_agent_event(working.event(1, session_started(), None))
                .unwrap(),
            TaskStatus::Working
        );
        manager
            .apply_agent_event(working.event(
                2,
                approval_requested(turn_id),
                Some("approve".into()),
            ))
            .unwrap();

        let waiting = manager
            .replace_agent_event_stream_after_stop(&working)
            .unwrap();
        assert_eq!(
            manager
                .apply_agent_event(waiting.event(1, session_started(), None))
                .unwrap(),
            TaskStatus::WaitingForApproval
        );
        assert_eq!(
            manager.get(id).unwrap().status_detail.as_deref(),
            Some("approve")
        );
    }

    #[test]
    fn clean_session_end_converges_from_each_waiting_state() {
        for (label, expected_waiting) in [
            ("approval", TaskStatus::WaitingForApproval),
            ("human", TaskStatus::WaitingForHuman),
        ] {
            let mut manager = TaskManager::new();
            let id = manager.create(new_task(label)).unwrap();
            let stream = manager.start_agent_event_stream(id).unwrap();
            let turn_id = AgentTurnId::new();
            let waiting_event = if label == "approval" {
                approval_requested(turn_id)
            } else {
                input_requested(turn_id)
            };
            manager
                .apply_agent_event(stream.event(1, session_started(), None))
                .unwrap();
            manager
                .apply_agent_event(stream.event(2, turn_started(turn_id), None))
                .unwrap();
            manager
                .apply_agent_event(stream.event(3, waiting_event, None))
                .unwrap();
            assert_eq!(manager.get(id).unwrap().status, expected_waiting);
            assert_eq!(
                manager
                    .apply_agent_event(stream.event(
                        4,
                        AgentEventKind::SessionEnded {
                            outcome: AgentSessionOutcome::Clean,
                        },
                        Some("transport closed".into()),
                    ))
                    .unwrap(),
                TaskStatus::ReadyForReview
            );
            assert!(matches!(
                manager.apply_agent_event(stream.event(5, AgentEventKind::UsageUpdated, None)),
                Err(AgentEventError::NoActiveStream(task_id)) if task_id == id
            ));
        }
    }

    #[test]
    fn cancellation_before_provider_start_closes_the_stream() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("cancel during startup")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        assert_eq!(
            manager
                .apply_agent_event(stream.event(
                    1,
                    AgentEventKind::Cancelled,
                    Some("cancelled before handshake".into()),
                ))
                .unwrap(),
            TaskStatus::Cancelled
        );
        assert!(matches!(
            manager.apply_agent_event(stream.event(2, session_started(), None)),
            Err(AgentEventError::TerminalState {
                status: TaskStatus::Cancelled,
                ..
            })
        ));
    }

    #[test]
    fn stopped_runtime_can_finish_out_of_band_with_stream_cas() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("worker failure")).unwrap();
        let stale = manager.start_agent_event_stream(id).unwrap();
        let current = manager
            .replace_agent_event_stream_after_stop(&stale)
            .unwrap();
        assert!(matches!(
            manager.finish_agent_event_stream_after_stop(
                &stale,
                AgentSessionOutcome::Failed,
                Some("stale worker".into()),
            ),
            Err(AgentEventError::EpochMismatch { task_id, .. }) if task_id == id
        ));

        assert_eq!(
            manager
                .finish_agent_event_stream_after_stop(
                    &current,
                    AgentSessionOutcome::Failed,
                    Some("channel closed\n\u{202e}".into()),
                )
                .unwrap(),
            TaskStatus::Failed
        );
        let detail = manager.get(id).unwrap().status_detail.as_deref().unwrap();
        assert_eq!(detail, "channel closed??");
        assert!(matches!(
            manager.finish_agent_event_stream_after_stop(
                &current,
                AgentSessionOutcome::Failed,
                None,
            ),
            Err(AgentEventError::NoActiveStream(task_id)) if task_id == id
        ));

        for outcome in [AgentSessionOutcome::Failed, AgentSessionOutcome::Cancelled] {
            let mut manager = TaskManager::new();
            let id = manager
                .create(new_task("review before transport loss"))
                .unwrap();
            let stream = manager.start_agent_event_stream(id).unwrap();
            let completed = AgentTurnId::new();
            let interrupted = AgentTurnId::new();
            manager
                .apply_agent_event(stream.event(1, session_started(), None))
                .unwrap();
            manager
                .apply_agent_event(stream.event(2, turn_started(completed), None))
                .unwrap();
            manager
                .apply_agent_event(stream.event(3, turn_completed(completed), None))
                .unwrap();
            manager
                .apply_agent_event(stream.event(4, turn_started(interrupted), None))
                .unwrap();
            assert_eq!(
                manager
                    .finish_agent_event_stream_after_stop(
                        &stream,
                        outcome,
                        Some("provider transport stopped".into()),
                    )
                    .unwrap(),
                TaskStatus::ReadyForReview
            );
            assert!(!manager.has_active_agent_event_stream(id));
            assert!(manager.task_needs_attention(id));
            assert_eq!(manager.next_validation_attempt(id), Ok(1));
            assert_eq!(
                manager.get(id).unwrap().status_detail.as_deref(),
                Some("provider transport stopped")
            );
        }
    }

    #[test]
    fn generic_status_updates_cannot_revive_terminal_tasks() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("sticky failure")).unwrap();
        manager
            .update_status(id, TaskStatus::Failed, Some("failed".into()))
            .unwrap();
        assert_eq!(
            manager.update_status(id, TaskStatus::Working, Some("revived".into())),
            Err(TaskError::CannotLeaveTerminalState {
                task_id: id,
                current: TaskStatus::Failed,
                requested: TaskStatus::Working,
            })
        );
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Failed);
        manager
            .update_status(id, TaskStatus::Failed, Some("updated detail".into()))
            .unwrap();
        assert_eq!(
            manager.get(id).unwrap().status_detail.as_deref(),
            Some("updated detail")
        );
    }

    #[test]
    fn event_detail_is_bounded_and_neutralizes_display_spoofing() {
        let mut hostile = String::from("  safe\n\t\u{202e}");
        hostile.push_str(&"界".repeat(MAX_AGENT_EVENT_DETAIL_BYTES));
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("bounded event")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let event = stream.event(1, session_started(), Some(hostile));
        let detail = event.detail().unwrap().to_string();
        assert!(detail.len() <= MAX_AGENT_EVENT_DETAIL_BYTES);
        assert!(detail.contains('?'));
        assert!(!detail.chars().any(|character| {
            character.is_control()
                || jterm_core::review_input::is_visual_spoofing_character(character)
        }));

        manager.apply_agent_event(event).unwrap();
        assert_eq!(
            manager.get(id).unwrap().status_detail.as_deref(),
            Some(detail.as_str())
        );
    }

    #[test]
    fn ready_for_review_can_restart_but_sticky_terminal_state_fails_closed() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("terminal event")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let first_turn = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(2, turn_started(first_turn), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(
                3,
                turn_completed(first_turn),
                Some("authoritative result".into()),
            ))
            .unwrap();

        let review_turn = AgentTurnId::new();
        assert_eq!(
            manager
                .apply_agent_event(stream.event(
                    4,
                    turn_started(review_turn),
                    Some("addressing review".into()),
                ))
                .unwrap(),
            TaskStatus::Working
        );
        assert_eq!(
            manager.update_status(id, TaskStatus::Completed, Some("too early".into())),
            Err(TaskError::NativeEventStreamActive(id))
        );
        manager
            .apply_agent_event(stream.event(
                5,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Clean,
                },
                None,
            ))
            .unwrap();
        assert_eq!(
            manager.update_status(id, TaskStatus::Completed, Some("accepted".into())),
            Err(TaskError::CompletionRequiresValidation(id))
        );
        manager
            .bind_validation_session(id, "validation-accepted".to_string())
            .unwrap();
        manager.handle_terminal_session_exit("validation-accepted", Some(0));
        manager.complete_after_validation(id).unwrap();
        assert!(matches!(
            manager.apply_agent_event(stream.event(
                6,
                AgentEventKind::Error { fatal: true },
                Some("late transport error".into()),
            )),
            Err(AgentEventError::TerminalState {
                status: TaskStatus::Completed,
                ..
            })
        ));
        let task = manager.get(id).unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(
            task.status_detail.as_deref(),
            Some("Validation passed; task accepted")
        );
        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::TerminalState { .. })
        ));
    }

    #[test]
    fn session_end_outcome_and_error_fatality_are_explicit() {
        let mut manager = TaskManager::new();
        let id = manager.create(new_task("session outcome")).unwrap();
        let stream = manager.start_agent_event_stream(id).unwrap();
        let turn_id = AgentTurnId::new();
        manager
            .apply_agent_event(stream.event(1, session_started(), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(2, turn_started(turn_id), None))
            .unwrap();
        manager
            .apply_agent_event(stream.event(
                3,
                AgentEventKind::Error { fatal: false },
                Some("recoverable provider warning".into()),
            ))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::Working);
        assert_eq!(manager.get(id).unwrap().status_detail, None);
        manager
            .apply_agent_event(stream.event(
                4,
                AgentEventKind::SessionEnded {
                    outcome: AgentSessionOutcome::Clean,
                },
                Some("clean shutdown".into()),
            ))
            .unwrap();
        assert_eq!(manager.get(id).unwrap().status, TaskStatus::ReadyForReview);
        assert!(matches!(
            manager.apply_agent_event(stream.event(5, AgentEventKind::UsageUpdated, None)),
            Err(AgentEventError::NoActiveStream(task_id)) if task_id == id
        ));

        assert!(matches!(
            manager.start_agent_event_stream(id),
            Err(AgentEventError::NativeStartRequiresCreated {
                task_id,
                status: TaskStatus::ReadyForReview,
            }) if task_id == id
        ));
    }
}
