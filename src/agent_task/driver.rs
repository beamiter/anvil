//! Provider-neutral native Agent driver contract and bounded event transport.
//!
//! Driver methods are deliberately synchronous but non-blocking: production
//! adapters enqueue work for their worker and return immediately. They must not
//! perform provider I/O, wait for a subprocess, or sleep on the UI thread.

use super::event::{AgentTurnId, ApprovalId, ProviderSessionId};
use super::{AgentEvent, AgentEventKind, AgentEventStream, AgentProvider, SemanticCommandContext};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fmt;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Event-count bound for one native driver.
pub const AGENT_EVENT_QUEUE_MESSAGE_CAPACITY: usize = 128;
/// Total retained event bytes for one native driver.
pub const AGENT_EVENT_QUEUE_BYTE_CAPACITY: usize = 8 * 1024 * 1024;
/// A provider must split or reject a normalized event larger than this.
pub const AGENT_EVENT_MAX_BYTES: usize = 64 * 1024;
/// Slots unavailable to noisy deltas, reserved for lifecycle/attention events.
pub const AGENT_EVENT_CRITICAL_RESERVE_MESSAGES: usize = 8;
/// Bytes unavailable to noisy deltas, reserved for lifecycle/attention events.
pub const AGENT_EVENT_CRITICAL_RESERVE_BYTES: usize = 512 * 1024;

/// Maximum text accepted by one prompt, steer, or denial command.
pub const AGENT_DRIVER_COMMAND_MAX_BYTES: usize = 256 * 1024;

/// Simultaneous message-count and byte limits for a driver event queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentEventQueueLimits {
    pub message_capacity: usize,
    pub byte_capacity: usize,
    pub max_event_bytes: usize,
    pub critical_reserve_messages: usize,
    pub critical_reserve_bytes: usize,
}

/// Public configuration name used by runtime wiring.
pub type AgentEventQueueConfig = AgentEventQueueLimits;

impl AgentEventQueueLimits {
    pub const PRODUCTION: Self = Self {
        message_capacity: AGENT_EVENT_QUEUE_MESSAGE_CAPACITY,
        byte_capacity: AGENT_EVENT_QUEUE_BYTE_CAPACITY,
        max_event_bytes: AGENT_EVENT_MAX_BYTES,
        critical_reserve_messages: AGENT_EVENT_CRITICAL_RESERVE_MESSAGES,
        critical_reserve_bytes: AGENT_EVENT_CRITICAL_RESERVE_BYTES,
    };

    fn validate(self) -> Result<Self, InvalidAgentEventQueueLimits> {
        let valid = self.message_capacity > 1
            && self.byte_capacity > 1
            && self.max_event_bytes > 0
            && self.max_event_bytes <= self.byte_capacity
            && self.critical_reserve_messages > 0
            && self.critical_reserve_messages < self.message_capacity
            && self
                .critical_reserve_messages
                .checked_mul(self.max_event_bytes)
                .is_some_and(|required| required <= self.critical_reserve_bytes)
            && self.critical_reserve_bytes < self.byte_capacity;
        valid.then_some(self).ok_or(InvalidAgentEventQueueLimits)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidAgentEventQueueLimits;

impl fmt::Display for InvalidAgentEventQueueLimits {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "Agent event queue limits require positive capacities, a proper critical reserve, and a max event that fits both the queue and reserve",
        )
    }
}

impl std::error::Error for InvalidAgentEventQueueLimits {}

/// A snapshot used for diagnostics and frame-level drain budgeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentEventQueueStats {
    pub queued_messages: usize,
    pub queued_bytes: usize,
    pub closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEventSendError {
    Full {
        event_bytes: usize,
        queued_bytes: usize,
        byte_capacity: usize,
        queued_messages: usize,
        message_capacity: usize,
        critical: bool,
    },
    TooLarge {
        event_bytes: usize,
        max_event_bytes: usize,
    },
    Closed,
    SequenceExhausted,
}

impl AgentEventSendError {
    pub fn is_backpressure(&self) -> bool {
        matches!(self, Self::Full { .. })
    }
}

impl fmt::Display for AgentEventSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full {
                event_bytes,
                queued_bytes,
                byte_capacity,
                queued_messages,
                message_capacity,
                critical,
            } => write!(
                formatter,
                "Agent event queue full: {event_bytes}-byte {} event, {queued_bytes}/{byte_capacity} bytes and {queued_messages}/{message_capacity} messages accounted",
                if *critical { "critical" } else { "ordinary" }
            ),
            Self::TooLarge {
                event_bytes,
                max_event_bytes,
            } => write!(
                formatter,
                "Agent event is too large: {event_bytes} bytes exceeds the {max_event_bytes}-byte limit"
            ),
            Self::Closed => formatter.write_str("Agent event receiver has closed"),
            Self::SequenceExhausted => {
                formatter.write_str("Agent event sequence has been exhausted")
            }
        }
    }
}

impl std::error::Error for AgentEventSendError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentEventReceiveError {
    Empty,
    Closed,
}

impl fmt::Display for AgentEventReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("Agent event queue is empty"),
            Self::Closed => formatter.write_str("Agent event queue is closed"),
        }
    }
}

impl std::error::Error for AgentEventReceiveError {}

struct QueuedAgentEvent {
    event: AgentEvent,
    accounted_bytes: usize,
}

struct AgentEventQueueState {
    pending: VecDeque<QueuedAgentEvent>,
    accounted_bytes: usize,
    sender_count: usize,
    receiver_alive: bool,
    explicitly_closed: bool,
}

struct AgentEventQueue {
    state: Mutex<AgentEventQueueState>,
    limits: AgentEventQueueLimits,
}

impl AgentEventQueue {
    fn new(limits: AgentEventQueueLimits) -> Self {
        Self {
            state: Mutex::new(AgentEventQueueState {
                pending: VecDeque::new(),
                accounted_bytes: 0,
                sender_count: 1,
                receiver_alive: true,
                explicitly_closed: false,
            }),
            limits,
        }
    }

    fn effective_capacity(&self, critical: bool) -> (usize, usize) {
        if critical {
            (self.limits.byte_capacity, self.limits.message_capacity)
        } else {
            (
                self.limits
                    .byte_capacity
                    .saturating_sub(self.limits.critical_reserve_bytes),
                self.limits
                    .message_capacity
                    .saturating_sub(self.limits.critical_reserve_messages),
            )
        }
    }

    fn try_enqueue(&self, event: &AgentEvent) -> Result<(), AgentEventSendError> {
        let event_bytes = accounted_event_bytes(event);
        if event_bytes > self.limits.max_event_bytes {
            return Err(AgentEventSendError::TooLarge {
                event_bytes,
                max_event_bytes: self.limits.max_event_bytes,
            });
        }

        let critical = is_critical_event(event.kind());
        let (byte_capacity, message_capacity) = self.effective_capacity(critical);
        let mut state = self.state.lock();
        if state.explicitly_closed || !state.receiver_alive {
            return Err(AgentEventSendError::Closed);
        }
        let has_byte_capacity = state
            .accounted_bytes
            .checked_add(event_bytes)
            .is_some_and(|bytes| bytes <= byte_capacity);
        let has_message_capacity = state.pending.len() < message_capacity;
        if !has_byte_capacity || !has_message_capacity {
            return Err(AgentEventSendError::Full {
                event_bytes,
                queued_bytes: state.accounted_bytes,
                byte_capacity,
                queued_messages: state.pending.len(),
                message_capacity,
                critical,
            });
        }

        // Clone only after all limits pass. AgentEvent constructors already
        // bound owned strings, so the retained clone matches byte accounting.
        state.pending.push_back(QueuedAgentEvent {
            event: event.clone(),
            accounted_bytes: event_bytes,
        });
        state.accounted_bytes += event_bytes;
        Ok(())
    }

    fn try_dequeue(&self) -> Result<AgentEvent, AgentEventReceiveError> {
        let mut state = self.state.lock();
        if let Some(queued) = state.pending.pop_front() {
            state.accounted_bytes = state.accounted_bytes.saturating_sub(queued.accounted_bytes);
            return Ok(queued.event);
        }
        if state.explicitly_closed || state.sender_count == 0 {
            Err(AgentEventReceiveError::Closed)
        } else {
            Err(AgentEventReceiveError::Empty)
        }
    }

    fn stats(&self) -> AgentEventQueueStats {
        let state = self.state.lock();
        AgentEventQueueStats {
            queued_messages: state.pending.len(),
            queued_bytes: state.accounted_bytes,
            closed: state.explicitly_closed || !state.receiver_alive || state.sender_count == 0,
        }
    }

    fn close_senders(&self) {
        self.state.lock().explicitly_closed = true;
    }

    fn drop_receiver(&self) {
        let mut state = self.state.lock();
        state.receiver_alive = false;
        state.explicitly_closed = true;
        state.pending.clear();
        state.accounted_bytes = 0;
    }
}

/// Cloneable, non-blocking producer for normalized events.
pub struct AgentEventSender {
    queue: Arc<AgentEventQueue>,
}

impl Clone for AgentEventSender {
    fn clone(&self) -> Self {
        self.queue.state.lock().sender_count += 1;
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}

impl Drop for AgentEventSender {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock();
        debug_assert!(state.sender_count > 0);
        state.sender_count = state.sender_count.saturating_sub(1);
    }
}

impl AgentEventSender {
    /// Attempt an all-or-nothing enqueue without waiting for capacity.
    pub fn try_send(&self, event: &AgentEvent) -> Result<(), AgentEventSendError> {
        self.queue.try_enqueue(event)
    }

    /// Refuse future sends while preserving already queued events for drain.
    pub fn close(&self) {
        self.queue.close_senders();
    }

    pub fn stats(&self) -> AgentEventQueueStats {
        self.queue.stats()
    }
}

/// Single-consumer, non-blocking end of the driver event queue.
pub struct AgentEventReceiver {
    queue: Arc<AgentEventQueue>,
}

impl AgentEventReceiver {
    pub fn try_recv(&self) -> Result<AgentEvent, AgentEventReceiveError> {
        self.queue.try_dequeue()
    }

    pub fn stats(&self) -> AgentEventQueueStats {
        self.queue.stats()
    }
}

impl Drop for AgentEventReceiver {
    fn drop(&mut self) {
        self.queue.drop_receiver();
    }
}

/// Construct the production event queue.
pub fn agent_event_channel() -> (AgentEventSender, AgentEventReceiver) {
    // The compile-time production constants satisfy validation. Keep the same
    // checked constructor as tests/custom adapters rather than duplicating it.
    agent_event_channel_with_limits(AgentEventQueueLimits::PRODUCTION)
        .expect("production Agent event queue limits must be valid")
}

/// Construct a queue with explicit limits, primarily for adapters and tests.
pub fn agent_event_channel_with_limits(
    limits: AgentEventQueueLimits,
) -> Result<(AgentEventSender, AgentEventReceiver), InvalidAgentEventQueueLimits> {
    let queue = Arc::new(AgentEventQueue::new(limits.validate()?));
    Ok((
        AgentEventSender {
            queue: Arc::clone(&queue),
        },
        AgentEventReceiver { queue },
    ))
}

/// Sequenced producer bound to exactly one task/session/epoch stream.
///
/// Clones share their sequence allocator. A failed enqueue does not consume a
/// sequence number, so a provider can retry the same semantic event after
/// backpressure without creating a reducer-visible gap.
#[derive(Clone)]
pub struct AgentEventSink {
    stream: AgentEventStream,
    sender: AgentEventSender,
    next_sequence: Arc<Mutex<Option<u64>>>,
}

impl AgentEventSink {
    pub fn new(stream: AgentEventStream, sender: AgentEventSender) -> Self {
        Self {
            stream,
            sender,
            next_sequence: Arc::new(Mutex::new(Some(1))),
        }
    }

    pub fn stream(&self) -> &AgentEventStream {
        &self.stream
    }

    pub fn try_emit(
        &self,
        kind: AgentEventKind,
        detail: Option<String>,
    ) -> Result<u64, AgentEventSendError> {
        let mut next_sequence = self.next_sequence.lock();
        let sequence = (*next_sequence).ok_or(AgentEventSendError::SequenceExhausted)?;
        let following = sequence
            .checked_add(1)
            .ok_or(AgentEventSendError::SequenceExhausted)?;
        let event = self.stream.event(sequence, kind, detail);
        self.sender.try_send(&event)?;
        *next_sequence = Some(following);
        Ok(sequence)
    }

    pub fn close(&self) {
        self.sender.close();
    }

    pub fn stats(&self) -> AgentEventQueueStats {
        self.sender.stats()
    }
}

/// Cancellation cannot be starved by either the command or event queue.
#[derive(Clone, Default)]
pub struct AgentCancellation {
    cancelled: Arc<AtomicBool>,
}

impl AgentCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn shared_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPrompt {
    pub turn_id: AgentTurnId,
    pub text: String,
}

impl AgentPrompt {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            turn_id: AgentTurnId::new(),
            text: text.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny { reason: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentCommand {
    Prompt(AgentPrompt),
    FinishSession,
    Steer {
        turn_id: AgentTurnId,
        text: String,
    },
    DecideApproval {
        id: ApprovalId,
        decision: ApprovalDecision,
    },
}

impl AgentCommand {
    pub(crate) fn validate(&self) -> Result<(), AgentDriverError> {
        let text = match self {
            Self::Prompt(prompt) => Some(prompt.text.as_str()),
            Self::FinishSession => None,
            Self::Steer { text, .. } => Some(text.as_str()),
            Self::DecideApproval {
                decision:
                    ApprovalDecision::Deny {
                        reason: Some(reason),
                    },
                ..
            } => Some(reason.as_str()),
            Self::DecideApproval { .. } => None,
        };
        if text.is_some_and(|text| text.is_empty() || text.len() > AGENT_DRIVER_COMMAND_MAX_BYTES) {
            return Err(AgentDriverError::InvalidCommand);
        }
        Ok(())
    }
}

/// Start or resume one already-correlated native Agent stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentStartRequest {
    /// Provider selected by the task. A driver must reject requests for a
    /// different adapter even when no provider session is being resumed.
    pub provider: AgentProvider,
    pub stream: AgentEventStream,
    pub worktree_path: PathBuf,
    pub source_context: Option<SemanticCommandContext>,
    pub initial_prompt: Option<AgentPrompt>,
    /// Optional provider-owned conversation identity. The Anvil event stream
    /// still receives a fresh local session/epoch for stale-callback defense.
    pub resume_from: Option<ProviderSessionId>,
}

impl AgentStartRequest {
    pub fn validate(&self) -> Result<(), AgentDriverError> {
        if !self.worktree_path.is_absolute() {
            return Err(AgentDriverError::InvalidWorktree);
        }
        if let Some(prompt) = &self.initial_prompt {
            AgentCommand::Prompt(prompt.clone()).validate()?;
        }
        Ok(())
    }

    pub fn validate_for_provider(&self, provider: AgentProvider) -> Result<(), AgentDriverError> {
        self.validate()?;
        if self.provider != provider
            || self
                .resume_from
                .as_ref()
                .is_some_and(|session| session.provider() != provider)
        {
            return Err(AgentDriverError::ProviderMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentDriverError {
    AlreadyStarted,
    NotStarted,
    InvalidWorktree,
    InvalidCommand,
    TurnActive,
    TurnLimitReached {
        limit: usize,
    },
    ProviderMismatch,
    Backpressure {
        queued_messages: usize,
        message_capacity: usize,
    },
    EventQueue(AgentEventSendError),
    Closed,
    Provider(String),
}

impl fmt::Display for AgentDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyStarted => formatter.write_str("Agent driver has already started"),
            Self::NotStarted => formatter.write_str("Agent driver has not started"),
            Self::InvalidWorktree => {
                formatter.write_str("Agent driver worktree must be an absolute path")
            }
            Self::InvalidCommand => write!(
                formatter,
                "Agent command text must be 1..={AGENT_DRIVER_COMMAND_MAX_BYTES} bytes"
            ),
            Self::TurnActive => formatter.write_str("Agent session is not idle"),
            Self::TurnLimitReached { limit } => write!(
                formatter,
                "Agent session reached its {limit}-turn limit; finish it before validation"
            ),
            Self::ProviderMismatch => {
                formatter.write_str("provider session ID does not belong to this Agent driver")
            }
            Self::Backpressure {
                queued_messages,
                message_capacity,
            } => write!(
                formatter,
                "Agent command queue full: {queued_messages}/{message_capacity} messages"
            ),
            Self::EventQueue(error) => error.fmt(formatter),
            Self::Closed => formatter.write_str("Agent driver has closed"),
            Self::Provider(error) => write!(formatter, "Agent provider failed: {error}"),
        }
    }
}

impl std::error::Error for AgentDriverError {}

impl From<AgentEventSendError> for AgentDriverError {
    fn from(error: AgentEventSendError) -> Self {
        Self::EventQueue(error)
    }
}

/// Provider adapter boundary used by the runtime and native Agent UI.
///
/// Every method must return promptly. Production implementations should use a
/// bounded command queue and a worker; `cancel` must only signal its independent
/// cancellation token. `try_next_event` must never wait for a future event.
pub trait AgentDriver: Send + 'static {
    fn provider(&self) -> AgentProvider;

    fn start(&mut self, request: AgentStartRequest) -> Result<(), AgentDriverError>;

    fn send(&mut self, command: AgentCommand) -> Result<(), AgentDriverError>;

    fn cancel(&mut self);

    fn try_next_event(&mut self) -> Result<Option<AgentEvent>, AgentDriverError>;
}

fn accounted_event_bytes(event: &AgentEvent) -> usize {
    size_of::<AgentEvent>()
        .saturating_add(event.stream().session_id().as_str().len())
        .saturating_add(event.kind().owned_payload_bytes())
        .saturating_add(event.detail().map_or(0, str::len))
}

fn is_critical_event(kind: &AgentEventKind) -> bool {
    matches!(
        kind,
        AgentEventKind::SessionStarted { .. }
            | AgentEventKind::TurnStarted { .. }
            | AgentEventKind::ApprovalRequested { .. }
            | AgentEventKind::PermissionRequested { .. }
            | AgentEventKind::InputRequested { .. }
            | AgentEventKind::WorkResumed { .. }
            | AgentEventKind::TurnCompleted { .. }
            | AgentEventKind::SessionEnded { .. }
            | AgentEventKind::Error { fatal: true }
            | AgentEventKind::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{
        event::{next_agent_event_epoch, PROVIDER_SESSION_ID_MAX_BYTES},
        NativeAgentSessionId, TaskId,
    };

    fn stream(name: &str) -> AgentEventStream {
        AgentEventStream::new(
            TaskId::new(),
            NativeAgentSessionId::parse(name).unwrap(),
            next_agent_event_epoch().unwrap(),
        )
    }

    fn limits_for(sample_bytes: usize) -> AgentEventQueueLimits {
        AgentEventQueueLimits {
            message_capacity: 3,
            byte_capacity: sample_bytes * 3,
            max_event_bytes: sample_bytes,
            critical_reserve_messages: 1,
            critical_reserve_bytes: sample_bytes,
        }
    }

    #[test]
    fn queue_reserves_message_and_byte_capacity_for_critical_events() {
        let stream = stream("reserve");
        let turn_id = AgentTurnId::new();
        let ordinary = stream.event(1, AgentEventKind::TextDelta, Some("x".repeat(32)));
        let critical = stream.event(
            2,
            AgentEventKind::ApprovalRequested {
                turn_id,
                approval_id: ApprovalId::new(),
            },
            Some("approve".into()),
        );
        let max_bytes = accounted_event_bytes(&ordinary).max(accounted_event_bytes(&critical));
        let (sender, receiver) = agent_event_channel_with_limits(limits_for(max_bytes)).unwrap();

        sender.try_send(&ordinary).unwrap();
        sender.try_send(&ordinary).unwrap();
        assert!(matches!(
            sender.try_send(&ordinary),
            Err(AgentEventSendError::Full {
                critical: false,
                ..
            })
        ));
        sender.try_send(&critical).unwrap();
        assert_eq!(sender.stats().queued_messages, 3);

        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::TextDelta
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::TextDelta
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().kind(),
            AgentEventKind::ApprovalRequested { .. }
        ));
    }

    #[test]
    fn queue_enforces_single_event_limit_before_allocation() {
        let stream = stream("single-cap");
        let event = stream.event(1, AgentEventKind::TextDelta, Some("payload".into()));
        let event_bytes = accounted_event_bytes(&event);
        let limits = AgentEventQueueLimits {
            message_capacity: 3,
            byte_capacity: event_bytes * 3,
            max_event_bytes: event_bytes - 1,
            critical_reserve_messages: 1,
            critical_reserve_bytes: event_bytes,
        };
        let (sender, _receiver) = agent_event_channel_with_limits(limits).unwrap();
        assert_eq!(
            sender.try_send(&event),
            Err(AgentEventSendError::TooLarge {
                event_bytes,
                max_event_bytes: event_bytes - 1,
            })
        );
        assert_eq!(sender.stats().queued_messages, 0);
    }

    #[test]
    fn queue_enforces_total_byte_budget_before_message_budget() {
        let stream = stream("byte-budget");
        let event = stream.event(1, AgentEventKind::TextDelta, Some("payload".into()));
        let event_bytes = accounted_event_bytes(&event);
        let limits = AgentEventQueueLimits {
            message_capacity: 6,
            // The ordinary capacity is two events minus one byte after the
            // critical reserve is removed.
            byte_capacity: event_bytes * 3 - 1,
            max_event_bytes: event_bytes,
            critical_reserve_messages: 1,
            critical_reserve_bytes: event_bytes,
        };
        let (sender, _receiver) = agent_event_channel_with_limits(limits).unwrap();
        sender.try_send(&event).unwrap();
        assert!(matches!(
            sender.try_send(&event),
            Err(AgentEventSendError::Full {
                queued_messages: 1,
                message_capacity: 5,
                ..
            })
        ));
    }

    #[test]
    fn sink_does_not_consume_sequence_when_queue_pushes_back() {
        let stream = stream("sequence");
        let sample = stream.event(1, AgentEventKind::TextDelta, None);
        let bytes = accounted_event_bytes(&sample);
        let limits = AgentEventQueueLimits {
            message_capacity: 2,
            byte_capacity: bytes * 2,
            max_event_bytes: bytes,
            critical_reserve_messages: 1,
            critical_reserve_bytes: bytes,
        };
        let (sender, receiver) = agent_event_channel_with_limits(limits).unwrap();
        let sink = AgentEventSink::new(stream, sender);

        assert_eq!(sink.try_emit(AgentEventKind::TextDelta, None), Ok(1));
        assert!(matches!(
            sink.try_emit(AgentEventKind::TextDelta, None),
            Err(AgentEventSendError::Full { .. })
        ));
        assert_eq!(receiver.try_recv().unwrap().sequence(), 1);
        assert_eq!(sink.try_emit(AgentEventKind::TextDelta, None), Ok(2));
        assert_eq!(receiver.try_recv().unwrap().sequence(), 2);
    }

    #[test]
    fn cancellation_is_independent_of_a_full_event_queue() {
        let cancellation = AgentCancellation::new();
        let stream = stream("cancel");
        let ordinary = stream.event(1, AgentEventKind::TextDelta, None);
        let bytes = accounted_event_bytes(&ordinary);
        let (sender, _receiver) = agent_event_channel_with_limits(AgentEventQueueLimits {
            message_capacity: 2,
            byte_capacity: bytes * 2,
            max_event_bytes: bytes,
            critical_reserve_messages: 1,
            critical_reserve_bytes: bytes,
        })
        .unwrap();
        sender.try_send(&ordinary).unwrap();
        assert!(matches!(
            sender.try_send(&ordinary),
            Err(AgentEventSendError::Full { .. })
        ));

        cancellation.cancel();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn receiver_drop_closes_all_producers_without_retaining_events() {
        let stream = stream("receiver-drop");
        let (sender, receiver) = agent_event_channel();
        sender
            .try_send(&stream.event(1, AgentEventKind::TextDelta, Some("queued".into())))
            .unwrap();
        drop(receiver);
        assert_eq!(sender.stats().queued_messages, 0);
        assert_eq!(
            sender.try_send(&stream.event(2, AgentEventKind::TextDelta, None)),
            Err(AgentEventSendError::Closed)
        );
    }

    #[test]
    fn provider_resume_ids_are_bounded_namespaced_and_checked_by_start() {
        let codex = ProviderSessionId::new(AgentProvider::Codex, "thread-42").unwrap();
        let claude = ProviderSessionId::new(AgentProvider::Claude, "thread-42").unwrap();
        assert_ne!(codex, claude);
        assert_eq!(codex.opaque(), "thread-42");
        assert!(ProviderSessionId::new(AgentProvider::Codex, "bad id").is_err());
        assert!(ProviderSessionId::new(
            AgentProvider::Codex,
            "x".repeat(PROVIDER_SESSION_ID_MAX_BYTES + 1)
        )
        .is_err());

        let request = AgentStartRequest {
            provider: AgentProvider::Codex,
            stream: stream("local-run"),
            worktree_path: PathBuf::from("/tmp/provider-mismatch"),
            source_context: None,
            initial_prompt: None,
            resume_from: Some(claude),
        };
        assert_eq!(
            request.validate_for_provider(AgentProvider::Codex),
            Err(AgentDriverError::ProviderMismatch)
        );
        let mut wrong_driver = request;
        wrong_driver.provider = AgentProvider::Claude;
        wrong_driver.resume_from = None;
        assert_eq!(
            wrong_driver.validate_for_provider(AgentProvider::Codex),
            Err(AgentDriverError::ProviderMismatch)
        );
    }

    #[test]
    fn provider_session_payload_counts_against_event_byte_limit() {
        let stream = stream("provider-payload");
        let provider_session = ProviderSessionId::new(
            AgentProvider::Codex,
            "x".repeat(PROVIDER_SESSION_ID_MAX_BYTES),
        )
        .unwrap();
        let without_provider = stream.event(
            1,
            AgentEventKind::SessionStarted {
                provider_session_id: None,
                resumed: false,
            },
            None,
        );
        let with_provider = stream.event(
            1,
            AgentEventKind::SessionStarted {
                provider_session_id: Some(provider_session),
                resumed: false,
            },
            None,
        );
        assert_eq!(
            accounted_event_bytes(&with_provider) - accounted_event_bytes(&without_provider),
            PROVIDER_SESSION_ID_MAX_BYTES
        );

        let event_bytes = accounted_event_bytes(&with_provider);
        let limits = AgentEventQueueLimits {
            message_capacity: 2,
            byte_capacity: event_bytes * 2,
            max_event_bytes: event_bytes - 1,
            critical_reserve_messages: 1,
            critical_reserve_bytes: event_bytes,
        };
        let (sender, _receiver) = agent_event_channel_with_limits(limits).unwrap();
        assert!(matches!(
            sender.try_send(&with_provider),
            Err(AgentEventSendError::TooLarge { .. })
        ));
    }
}
