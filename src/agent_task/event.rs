//! Correlated, provider-neutral lifecycle events for native Agent drivers.
//!
//! Provider transports are untrusted and asynchronous.  The types in this
//! module keep their identity separate from PTY session IDs, give each stream
//! incarnation a process-unique epoch, require a contiguous sequence, and
//! bound display text before it can enter long-lived task state.

use super::task::{AgentProvider, TaskId, TaskStatus};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// Maximum native session identity retained for event correlation.
pub const MAX_NATIVE_AGENT_SESSION_ID_BYTES: usize = 256;
/// Maximum provider-controlled detail retained on a task.
pub const MAX_AGENT_EVENT_DETAIL_BYTES: usize = 1024;
/// Maximum provider-owned resume identity retained by Anvil.
pub const PROVIDER_SESSION_ID_MAX_BYTES: usize = 512;

static NEXT_AGENT_EVENT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Anvil-local identity for one native Agent session.
///
/// This is deliberately neither a jsh/PTY session ID nor the provider's
/// opaque thread/session ID. A runtime generates it and adapters use it only
/// for routing events back to the matching Anvil task incarnation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NativeAgentSessionId(String);

impl NativeAgentSessionId {
    /// Generate an Anvil-owned identity. Provider adapters must not substitute
    /// their wire/session identifiers here.
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Parse a previously generated Anvil identity at an internal boundary.
    /// Native task persistence is not implemented yet; this is primarily
    /// useful for deterministic tests and future hardened checkpoint restore.
    #[cfg(test)]
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, InvalidNativeAgentSessionId> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_NATIVE_AGENT_SESSION_ID_BYTES
            || value.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || jterm_core::review_input::is_visual_spoofing_character(character)
            })
        {
            return Err(InvalidNativeAgentSessionId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for NativeAgentSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NativeAgentSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidNativeAgentSessionId;

impl fmt::Display for InvalidNativeAgentSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "native Agent session ID must be 1..={MAX_NATIVE_AGENT_SESSION_ID_BYTES} bytes without whitespace, controls, or invisible formatting"
        )
    }
}

impl std::error::Error for InvalidNativeAgentSessionId {}

/// Provider-namespaced opaque identity used only for resume/checkpoint data.
/// It never grants filesystem or process authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSessionId {
    provider: AgentProvider,
    opaque: String,
}

impl ProviderSessionId {
    pub fn new(
        provider: AgentProvider,
        opaque: impl Into<String>,
    ) -> Result<Self, InvalidProviderSessionId> {
        let opaque = opaque.into();
        if opaque.is_empty()
            || opaque.len() > PROVIDER_SESSION_ID_MAX_BYTES
            || opaque.chars().any(|character| {
                character.is_whitespace()
                    || character.is_control()
                    || jterm_core::review_input::is_visual_spoofing_character(character)
            })
        {
            return Err(InvalidProviderSessionId);
        }
        Ok(Self { provider, opaque })
    }

    pub fn provider(&self) -> AgentProvider {
        self.provider
    }

    pub fn opaque(&self) -> &str {
        &self.opaque
    }
}

impl fmt::Display for ProviderSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}",
            self.provider.display_name(),
            self.opaque
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidProviderSessionId;

impl fmt::Display for InvalidProviderSessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "provider session ID must be 1..={PROVIDER_SESSION_ID_MAX_BYTES} bytes without whitespace, controls, or invisible formatting"
        )
    }
}

impl std::error::Error for InvalidProviderSessionId {}

/// Anvil-local identity for one logical Agent turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentTurnId(Uuid);

impl AgentTurnId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AgentTurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AgentTurnId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Anvil-local claim token for one approval or permission request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ApprovalId(Uuid);

impl ApprovalId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ApprovalId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ApprovalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Process-local incarnation of a native Agent event stream.
///
/// A reconnect or replacement receives a new epoch even when the provider
/// resumes the same stable session ID, so callbacks from the old transport
/// cannot mutate the replacement task state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AgentEventEpoch(u64);

impl AgentEventEpoch {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Correlation token captured by every callback belonging to one event stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEventStream {
    task_id: TaskId,
    session_id: NativeAgentSessionId,
    epoch: AgentEventEpoch,
}

impl AgentEventStream {
    pub fn task_id(&self) -> TaskId {
        self.task_id
    }

    pub fn session_id(&self) -> &NativeAgentSessionId {
        &self.session_id
    }

    pub fn epoch(&self) -> AgentEventEpoch {
        self.epoch
    }

    /// Build one bounded event carrying this stream's complete correlation.
    pub(crate) fn event(
        &self,
        sequence: u64,
        kind: AgentEventKind,
        detail: Option<String>,
    ) -> AgentEvent {
        AgentEvent::new(self.clone(), sequence, kind, detail)
    }

    pub(super) fn new(
        task_id: TaskId,
        session_id: NativeAgentSessionId,
        epoch: AgentEventEpoch,
    ) -> Self {
        Self {
            task_id,
            session_id,
            epoch,
        }
    }
}

/// Lifecycle signals normalized by provider adapters.
///
/// Content deltas, tool payloads, and diffs belong to their own bounded stores;
/// this deliberately small enum contains only signals that may change the
/// dashboard task status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEventKind {
    SessionStarted {
        provider_session_id: Option<ProviderSessionId>,
        resumed: bool,
    },
    TurnStarted {
        turn_id: AgentTurnId,
    },
    TextDelta,
    ReasoningDelta,
    PlanUpdated,
    CommandStarted,
    CommandOutput,
    CommandFinished,
    FileChanged,
    DiffUpdated,
    ApprovalRequested {
        turn_id: AgentTurnId,
        approval_id: ApprovalId,
    },
    PermissionRequested {
        turn_id: AgentTurnId,
        approval_id: ApprovalId,
    },
    InputRequested {
        turn_id: AgentTurnId,
    },
    ToolStarted,
    ToolFinished,
    UsageUpdated,
    WorkResumed {
        turn_id: AgentTurnId,
    },
    TurnCompleted {
        turn_id: AgentTurnId,
    },
    SessionEnded {
        outcome: AgentSessionOutcome,
    },
    Error {
        fatal: bool,
    },
    Cancelled,
}

impl AgentEventKind {
    pub(crate) fn owned_payload_bytes(&self) -> usize {
        match self {
            Self::SessionStarted {
                provider_session_id: Some(session),
                ..
            } => session.opaque().len(),
            _ => 0,
        }
    }
}

/// Provider-neutral outcome reported when a native session actually ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSessionOutcome {
    Clean,
    Failed,
    Cancelled,
}

/// One normalized event with task/session/epoch/sequence correlation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEvent {
    stream: AgentEventStream,
    sequence: u64,
    kind: AgentEventKind,
    detail: Option<String>,
}

impl AgentEvent {
    pub(crate) fn new(
        stream: AgentEventStream,
        sequence: u64,
        kind: AgentEventKind,
        detail: Option<String>,
    ) -> Self {
        Self {
            stream,
            sequence,
            kind,
            detail: bounded_event_detail(detail),
        }
    }

    pub fn stream(&self) -> &AgentEventStream {
        &self.stream
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn kind(&self) -> &AgentEventKind {
        &self.kind
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub(super) fn into_parts(self) -> (AgentEventStream, u64, AgentEventKind, Option<String>) {
        (self.stream, self.sequence, self.kind, self.detail)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEventError {
    UnknownTask(TaskId),
    TerminalState {
        task_id: TaskId,
        status: TaskStatus,
    },
    NoActiveStream(TaskId),
    StreamAlreadyActive(TaskId),
    NativeStartRequiresCreated {
        task_id: TaskId,
        status: TaskStatus,
    },
    ValidationActive(TaskId),
    TerminalSessionBound(TaskId),
    SessionMismatch(TaskId),
    ProviderMismatch {
        task_id: TaskId,
        expected: AgentProvider,
        received: AgentProvider,
    },
    MissingResumeSession(TaskId),
    EpochMismatch {
        task_id: TaskId,
        expected: AgentEventEpoch,
        received: AgentEventEpoch,
    },
    InvalidSequence {
        task_id: TaskId,
        expected: u64,
        received: u64,
    },
    SessionNotStarted(TaskId),
    TurnAlreadyActive {
        task_id: TaskId,
        turn_id: AgentTurnId,
    },
    NoActiveTurn(TaskId),
    TurnMismatch {
        task_id: TaskId,
        expected: AgentTurnId,
        received: AgentTurnId,
    },
    InvalidTransition {
        task_id: TaskId,
        status: TaskStatus,
        event: AgentEventKind,
    },
    EpochExhausted,
    SequenceExhausted(TaskId),
}

impl fmt::Display for AgentEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(task_id) => write!(formatter, "unknown task {task_id}"),
            Self::TerminalState { task_id, status } => write!(
                formatter,
                "task {task_id} is already in terminal state {}",
                status.label()
            ),
            Self::NoActiveStream(task_id) => {
                write!(formatter, "task {task_id} has no active native Agent event stream")
            }
            Self::StreamAlreadyActive(task_id) => write!(
                formatter,
                "task {task_id} already has an active native Agent event stream"
            ),
            Self::NativeStartRequiresCreated { task_id, status } => write!(
                formatter,
                "native Agent task {task_id} can start only from Created, not {}",
                status.label()
            ),
            Self::ValidationActive(task_id) => write!(
                formatter,
                "task {task_id} cannot resume Agent work while validation is running"
            ),
            Self::TerminalSessionBound(task_id) => write!(
                formatter,
                "task {task_id} is already bound to an opaque Agent terminal session"
            ),
            Self::SessionMismatch(task_id) => write!(
                formatter,
                "native Agent event session does not match task {task_id}"
            ),
            Self::ProviderMismatch {
                task_id,
                expected,
                received,
            } => write!(
                formatter,
                "native Agent provider {} does not match {} for task {task_id}",
                received.display_name(),
                expected.display_name()
            ),
            Self::MissingResumeSession(task_id) => write!(
                formatter,
                "resumed native Agent stream for task {task_id} did not report a provider session ID"
            ),
            Self::EpochMismatch {
                task_id,
                expected,
                received,
            } => write!(
                formatter,
                "native Agent event epoch {} does not match current epoch {} for task {task_id}",
                received.get(),
                expected.get()
            ),
            Self::InvalidSequence {
                task_id,
                expected,
                received,
            } => write!(
                formatter,
                "native Agent event sequence {received} is invalid for task {task_id}; expected {expected}"
            ),
            Self::SessionNotStarted(task_id) => write!(
                formatter,
                "native Agent event stream for task {task_id} must begin with SessionStarted"
            ),
            Self::TurnAlreadyActive { task_id, turn_id } => write!(
                formatter,
                "native Agent task {task_id} already has active turn {turn_id}"
            ),
            Self::NoActiveTurn(task_id) => {
                write!(formatter, "native Agent task {task_id} has no active turn")
            }
            Self::TurnMismatch {
                task_id,
                expected,
                received,
            } => write!(
                formatter,
                "native Agent turn {received} does not match active turn {expected} for task {task_id}"
            ),
            Self::InvalidTransition {
                task_id,
                status,
                event,
            } => write!(
                formatter,
                "native Agent event {event:?} cannot be applied to task {task_id} in state {}",
                status.label()
            ),
            Self::EpochExhausted => formatter.write_str("native Agent event epoch exhausted"),
            Self::SequenceExhausted(task_id) => {
                write!(formatter, "native Agent event sequence exhausted for task {task_id}")
            }
        }
    }
}

impl std::error::Error for AgentEventError {}

pub(super) fn next_agent_event_epoch() -> Result<AgentEventEpoch, AgentEventError> {
    NEXT_AGENT_EVENT_EPOCH
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0 && current != u64::MAX).then_some(current + 1)
        })
        .map(AgentEventEpoch)
        .map_err(|_| AgentEventError::EpochExhausted)
}

pub(super) fn status_after_event(
    current: TaskStatus,
    event: &AgentEventKind,
) -> Option<(TaskStatus, bool)> {
    use AgentEventKind as Event;
    use TaskStatus as Status;

    match (current, event) {
        (Status::Created, Event::SessionStarted { .. }) => Some((Status::Starting, true)),
        (
            Status::Starting
            | Status::Working
            | Status::WaitingForApproval
            | Status::WaitingForHuman
            | Status::ReadyForReview,
            Event::SessionStarted { .. },
        ) => Some((current, true)),
        (
            Status::Starting | Status::Working | Status::ReadyForReview,
            Event::TurnStarted { .. },
        ) => Some((Status::Working, true)),
        (
            Status::Starting | Status::Working | Status::WaitingForApproval,
            Event::ApprovalRequested { .. } | Event::PermissionRequested { .. },
        ) => Some((Status::WaitingForApproval, true)),
        (
            Status::Starting | Status::Working | Status::WaitingForHuman,
            Event::InputRequested { .. },
        ) => Some((Status::WaitingForHuman, true)),
        (Status::WaitingForApproval | Status::WaitingForHuman, Event::WorkResumed { .. }) => {
            Some((Status::Working, true))
        }
        (Status::Starting | Status::Working, Event::TurnCompleted { .. }) => {
            Some((Status::ReadyForReview, true))
        }
        (
            Status::Created
            | Status::Starting
            | Status::Working
            | Status::WaitingForApproval
            | Status::WaitingForHuman
            | Status::ReadyForReview,
            Event::SessionEnded {
                outcome: AgentSessionOutcome::Clean,
            },
        ) => Some((Status::ReadyForReview, true)),
        (
            Status::ReadyForReview,
            Event::SessionEnded {
                outcome: AgentSessionOutcome::Failed,
            }
            | Event::Error { fatal: true },
        ) => Some((Status::ReadyForReview, true)),
        (
            Status::Created
            | Status::Starting
            | Status::Working
            | Status::WaitingForApproval
            | Status::WaitingForHuman,
            Event::SessionEnded {
                outcome: AgentSessionOutcome::Failed,
            }
            | Event::Error { fatal: true },
        ) => Some((Status::Failed, true)),
        (
            Status::ReadyForReview,
            Event::SessionEnded {
                outcome: AgentSessionOutcome::Cancelled,
            }
            | Event::Cancelled,
        ) => Some((Status::ReadyForReview, true)),
        (
            Status::Created
            | Status::Starting
            | Status::Working
            | Status::WaitingForApproval
            | Status::WaitingForHuman,
            Event::SessionEnded {
                outcome: AgentSessionOutcome::Cancelled,
            },
        ) => Some((Status::Cancelled, true)),
        (
            Status::Created
            | Status::Starting
            | Status::Working
            | Status::WaitingForApproval
            | Status::WaitingForHuman,
            Event::Cancelled,
        ) => Some((Status::Cancelled, true)),
        (
            Status::Starting
            | Status::Working
            | Status::WaitingForApproval
            | Status::WaitingForHuman
            | Status::ReadyForReview,
            Event::TextDelta
            | Event::ReasoningDelta
            | Event::PlanUpdated
            | Event::CommandStarted
            | Event::CommandOutput
            | Event::CommandFinished
            | Event::FileChanged
            | Event::DiffUpdated
            | Event::ToolStarted
            | Event::ToolFinished
            | Event::UsageUpdated
            | Event::Error { fatal: false },
        ) => Some((current, false)),
        _ => None,
    }
}

pub(super) fn event_ends_stream(event: &AgentEventKind) -> bool {
    // TurnCompleted is deliberately absent: one provider session/stream may
    // carry another strictly correlated turn after its review point.
    matches!(
        event,
        AgentEventKind::SessionEnded { .. }
            | AgentEventKind::Cancelled
            | AgentEventKind::Error { fatal: true }
    )
}

pub(super) fn bounded_event_detail(detail: Option<String>) -> Option<String> {
    let detail = detail?;
    let mut bounded = String::with_capacity(detail.len().min(MAX_AGENT_EVENT_DETAIL_BYTES));
    for character in detail.chars() {
        let visible = if character.is_control()
            || jterm_core::review_input::is_visual_spoofing_character(character)
        {
            '?'
        } else {
            character
        };
        if bounded.len() + visible.len_utf8() > MAX_AGENT_EVENT_DETAIL_BYTES {
            break;
        }
        bounded.push(visible);
    }
    let trimmed = bounded.trim_matches(' ');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}
