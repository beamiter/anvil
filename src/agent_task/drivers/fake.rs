//! Deterministic, thread-free driver used to exercise runtime semantics.

use super::super::driver::{
    agent_event_channel, agent_event_channel_with_limits, AgentCancellation, AgentCommand,
    AgentDriver, AgentDriverError, AgentEventQueueLimits, AgentEventReceiveError,
    AgentEventReceiver, AgentEventSender, AgentEventSink, AgentStartRequest,
    InvalidAgentEventQueueLimits,
};
use super::super::{AgentEvent, AgentEventKind, AgentProvider};
use std::collections::VecDeque;

pub const FAKE_AGENT_COMMAND_CAPACITY: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeAgentEvent {
    pub kind: AgentEventKind,
    pub detail: Option<String>,
}

impl FakeAgentEvent {
    pub fn new(kind: AgentEventKind, detail: Option<String>) -> Self {
        Self { kind, detail }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeAgentProgress {
    Emitted { sequence: u64 },
    Backpressured,
    Finished,
}

/// A single-threaded fake: each `advance` attempts exactly one scripted event.
/// Full queues leave the current script entry untouched for deterministic retry.
pub struct FakeAgentDriver {
    provider: AgentProvider,
    script: VecDeque<FakeAgentEvent>,
    event_sender: Option<AgentEventSender>,
    event_receiver: AgentEventReceiver,
    sink: Option<AgentEventSink>,
    cancellation: AgentCancellation,
    cancellation_emitted: bool,
    started: bool,
    commands: VecDeque<AgentCommand>,
    command_capacity: usize,
}

impl FakeAgentDriver {
    pub fn new(provider: AgentProvider, script: impl IntoIterator<Item = FakeAgentEvent>) -> Self {
        let (event_sender, event_receiver) = agent_event_channel();
        Self::from_channel(provider, script, event_sender, event_receiver)
    }

    pub fn with_event_limits(
        provider: AgentProvider,
        script: impl IntoIterator<Item = FakeAgentEvent>,
        limits: AgentEventQueueLimits,
    ) -> Result<Self, InvalidAgentEventQueueLimits> {
        let (event_sender, event_receiver) = agent_event_channel_with_limits(limits)?;
        Ok(Self::from_channel(
            provider,
            script,
            event_sender,
            event_receiver,
        ))
    }

    fn from_channel(
        provider: AgentProvider,
        script: impl IntoIterator<Item = FakeAgentEvent>,
        event_sender: AgentEventSender,
        event_receiver: AgentEventReceiver,
    ) -> Self {
        Self {
            provider,
            script: script.into_iter().collect(),
            event_sender: Some(event_sender),
            event_receiver,
            sink: None,
            cancellation: AgentCancellation::new(),
            cancellation_emitted: false,
            started: false,
            commands: VecDeque::new(),
            command_capacity: FAKE_AGENT_COMMAND_CAPACITY,
        }
    }

    pub fn with_command_capacity(mut self, capacity: usize) -> Self {
        self.command_capacity = capacity.max(1);
        self
    }

    pub fn cancellation(&self) -> AgentCancellation {
        self.cancellation.clone()
    }

    pub fn queued_commands(&self) -> usize {
        self.commands.len()
    }

    pub fn take_command(&mut self) -> Option<AgentCommand> {
        self.commands.pop_front()
    }

    pub fn remaining_script_events(&self) -> usize {
        self.script.len()
    }

    /// Attempt one deterministic provider step without consuming a UI event.
    pub fn advance(&mut self) -> Result<FakeAgentProgress, AgentDriverError> {
        let sink = self.sink.as_ref().ok_or(AgentDriverError::NotStarted)?;
        let cancellation_event = self.cancellation.is_cancelled() && !self.cancellation_emitted;
        if self.cancellation.is_cancelled() && self.cancellation_emitted {
            return Ok(FakeAgentProgress::Finished);
        }

        let scripted = if cancellation_event {
            FakeAgentEvent::new(AgentEventKind::Cancelled, None)
        } else if let Some(scripted) = self.script.front() {
            scripted.clone()
        } else {
            return Ok(FakeAgentProgress::Finished);
        };

        match sink.try_emit(scripted.kind, scripted.detail) {
            Ok(sequence) => {
                if cancellation_event {
                    self.cancellation_emitted = true;
                } else {
                    self.script.pop_front();
                }
                Ok(FakeAgentProgress::Emitted { sequence })
            }
            Err(error) if error.is_backpressure() => Ok(FakeAgentProgress::Backpressured),
            Err(error) => Err(AgentDriverError::EventQueue(error)),
        }
    }

    fn receive_queued(&self) -> Result<Option<AgentEvent>, AgentDriverError> {
        match self.event_receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(AgentEventReceiveError::Empty) => Ok(None),
            Err(AgentEventReceiveError::Closed) => Err(AgentDriverError::Closed),
        }
    }
}

impl AgentDriver for FakeAgentDriver {
    fn provider(&self) -> AgentProvider {
        self.provider
    }

    fn start(&mut self, request: AgentStartRequest) -> Result<(), AgentDriverError> {
        if self.started {
            return Err(AgentDriverError::AlreadyStarted);
        }
        request.validate_for_provider(self.provider)?;
        if let Some(prompt) = request.initial_prompt {
            if self.commands.len() == self.command_capacity {
                return Err(AgentDriverError::Backpressure {
                    queued_messages: self.commands.len(),
                    message_capacity: self.command_capacity,
                });
            }
            self.commands.push_back(AgentCommand::Prompt(prompt));
        }
        let sender = self.event_sender.take().ok_or(AgentDriverError::Closed)?;
        self.sink = Some(AgentEventSink::new(request.stream, sender));
        self.started = true;
        Ok(())
    }

    fn send(&mut self, command: AgentCommand) -> Result<(), AgentDriverError> {
        if !self.started {
            return Err(AgentDriverError::NotStarted);
        }
        command.validate()?;
        if self.cancellation.is_cancelled() {
            return Err(AgentDriverError::Closed);
        }
        if self.commands.len() == self.command_capacity {
            return Err(AgentDriverError::Backpressure {
                queued_messages: self.commands.len(),
                message_capacity: self.command_capacity,
            });
        }
        self.commands.push_back(command);
        Ok(())
    }

    fn cancel(&mut self) {
        self.cancellation.cancel();
    }

    fn try_next_event(&mut self) -> Result<Option<AgentEvent>, AgentDriverError> {
        if !self.started {
            return Err(AgentDriverError::NotStarted);
        }
        if let Some(event) = self.receive_queued()? {
            return Ok(Some(event));
        }
        match self.advance()? {
            FakeAgentProgress::Emitted { .. } => self.receive_queued(),
            FakeAgentProgress::Backpressured | FakeAgentProgress::Finished => Ok(None),
        }
    }
}

impl Drop for FakeAgentDriver {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(sink) = &self.sink {
            sink.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::driver::AgentPrompt;
    use crate::agent_task::{
        event::next_agent_event_epoch, AgentEventStream, AgentTurnId, NativeAgentSessionId,
        ProviderSessionId, TaskId,
    };
    use std::path::PathBuf;

    fn stream(name: &str) -> AgentEventStream {
        AgentEventStream::new(
            TaskId::new(),
            NativeAgentSessionId::parse(name).unwrap(),
            next_agent_event_epoch().unwrap(),
        )
    }

    fn request(stream: AgentEventStream, provider: AgentProvider) -> AgentStartRequest {
        AgentStartRequest {
            provider,
            stream,
            worktree_path: PathBuf::from("/tmp/fake-agent-worktree"),
            source_context: None,
            initial_prompt: Some(AgentPrompt::new("fix the failure")),
            resume_from: None,
        }
    }

    #[test]
    fn trait_is_object_safe_send_and_fake_sequences_events() {
        fn assert_driver(_: Box<dyn AgentDriver>) {}

        let expected_stream = stream("strict-sequence");
        let turn_id = AgentTurnId::new();
        let mut fake = FakeAgentDriver::new(
            AgentProvider::Codex,
            [
                FakeAgentEvent::new(
                    AgentEventKind::SessionStarted {
                        provider_session_id: Some(
                            ProviderSessionId::new(AgentProvider::Codex, "fake-thread").unwrap(),
                        ),
                        resumed: false,
                    },
                    None,
                ),
                FakeAgentEvent::new(
                    AgentEventKind::TurnStarted { turn_id },
                    Some("working".into()),
                ),
            ],
        );
        fake.start(request(expected_stream.clone(), AgentProvider::Codex))
            .unwrap();
        assert_eq!(fake.queued_commands(), 1);

        let first = fake.try_next_event().unwrap().unwrap();
        let second = fake.try_next_event().unwrap().unwrap();
        assert_eq!(first.sequence(), 1);
        assert_eq!(second.sequence(), 2);
        assert_eq!(first.stream(), &expected_stream);
        assert_eq!(second.stream(), &expected_stream);
        assert!(matches!(
            first.kind(),
            AgentEventKind::SessionStarted {
                provider_session_id: Some(session),
                resumed: false,
            } if session.opaque() == "fake-thread"
        ));
        assert_driver(Box::new(fake));
    }

    #[test]
    fn full_event_queue_preserves_script_and_sequence_for_retry() {
        let event_stream = stream("backpressure");
        let sample = event_stream.event(1, AgentEventKind::TextDelta, None);
        let sample_bytes =
            std::mem::size_of::<AgentEvent>() + sample.stream().session_id().as_str().len();
        let limits = AgentEventQueueLimits {
            message_capacity: 2,
            byte_capacity: sample_bytes * 2,
            max_event_bytes: sample_bytes,
            critical_reserve_messages: 1,
            critical_reserve_bytes: sample_bytes,
        };
        let mut fake = FakeAgentDriver::with_event_limits(
            AgentProvider::Claude,
            [
                FakeAgentEvent::new(AgentEventKind::TextDelta, None),
                FakeAgentEvent::new(AgentEventKind::TextDelta, None),
            ],
            limits,
        )
        .unwrap();
        fake.start(request(event_stream, AgentProvider::Claude))
            .unwrap();

        assert_eq!(
            fake.advance().unwrap(),
            FakeAgentProgress::Emitted { sequence: 1 }
        );
        assert_eq!(fake.advance().unwrap(), FakeAgentProgress::Backpressured);
        assert_eq!(fake.remaining_script_events(), 1);
        assert_eq!(fake.try_next_event().unwrap().unwrap().sequence(), 1);
        assert_eq!(
            fake.advance().unwrap(),
            FakeAgentProgress::Emitted { sequence: 2 }
        );
        assert_eq!(fake.try_next_event().unwrap().unwrap().sequence(), 2);
    }

    #[test]
    fn cancel_bypasses_full_command_and_ordinary_event_capacity() {
        let event_stream = stream("cancel-priority");
        let sample = event_stream.event(1, AgentEventKind::TextDelta, None);
        let sample_bytes =
            std::mem::size_of::<AgentEvent>() + sample.stream().session_id().as_str().len();
        let limits = AgentEventQueueLimits {
            message_capacity: 2,
            byte_capacity: sample_bytes * 2,
            max_event_bytes: sample_bytes,
            critical_reserve_messages: 1,
            critical_reserve_bytes: sample_bytes,
        };
        let mut fake = FakeAgentDriver::with_event_limits(
            AgentProvider::OpenCode,
            [FakeAgentEvent::new(AgentEventKind::TextDelta, None)],
            limits,
        )
        .unwrap()
        .with_command_capacity(1);
        fake.start(request(event_stream, AgentProvider::OpenCode))
            .unwrap();
        assert!(matches!(
            fake.send(AgentCommand::Steer {
                turn_id: AgentTurnId::new(),
                text: "more".into(),
            }),
            Err(AgentDriverError::Backpressure { .. })
        ));
        assert_eq!(
            fake.advance().unwrap(),
            FakeAgentProgress::Emitted { sequence: 1 }
        );

        fake.cancel();
        assert!(fake.cancellation().is_cancelled());
        assert_eq!(
            fake.advance().unwrap(),
            FakeAgentProgress::Emitted { sequence: 2 }
        );
        assert_eq!(fake.try_next_event().unwrap().unwrap().sequence(), 1);
        let cancelled = fake.try_next_event().unwrap().unwrap();
        assert_eq!(cancelled.sequence(), 2);
        assert!(matches!(cancelled.kind(), AgentEventKind::Cancelled));
    }
}
