//! Semantic task announcements. This state belongs to the application, not a
//! panel or row. It retains only the latest result per task in the complete
//! manager collection; it never retains stream text or a growing event log.
use std::collections::{HashMap, HashSet};

use crate::agent_task::{
    AgentRuntimeCompletion, AgentSessionOutcome, AgentTurnId, AppliedAgentTurn, TaskId, TaskStatus,
    TaskValidationStatus,
};

pub(crate) struct TaskAnnouncementSnapshot {
    pub(crate) id: TaskId,
    pub(crate) status: TaskStatus,
    pub(crate) terminal: Option<String>,
    pub(crate) validation_attempt: u64,
    pub(crate) validation_status: TaskValidationStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TaskResult {
    status: TaskStatus,
    turn: Option<(u64, AgentTurnId)>,
    terminal: Option<String>,
}

/// One semantic terminal outcome, shared by the reducer-status and delayed
/// runtime-completion paths. Full TaskId is the containing map key.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OutcomeKey {
    turn: Option<(u64, AgentTurnId)>,
    terminal: Option<String>,
    outcome: AgentSessionOutcome,
}

fn result_outcome(result: &TaskResult) -> Option<OutcomeKey> {
    let outcome = match result.status {
        TaskStatus::Failed => AgentSessionOutcome::Failed,
        TaskStatus::Cancelled => AgentSessionOutcome::Cancelled,
        _ => return None,
    };
    Some(OutcomeKey {
        turn: result.turn,
        terminal: result.terminal.clone(),
        outcome,
    })
}

#[derive(Default)]
struct ObservedTask {
    result: Option<TaskResult>,
    validation: Option<(u64, TaskValidationStatus)>,
    outcome: Option<OutcomeKey>,
}

#[derive(Default)]
pub(crate) struct TaskAnnouncements {
    tasks: HashMap<TaskId, ObservedTask>,
    turns: HashMap<TaskId, AppliedAgentTurn>,
    issue_details: HashMap<TaskId, String>,
}

impl TaskAnnouncements {
    /// Consume only boundaries reported AFTER successful reducer application.
    /// The asynchronously advancing driver presentation is never consulted.
    pub(crate) fn applied_turns(&mut self, turns: &[AppliedAgentTurn]) {
        for turn in turns {
            self.turns.insert(turn.task_id, *turn);
        }
    }

    pub(crate) fn retain_issue_details(&mut self, details: HashMap<TaskId, String>) {
        self.issue_details.extend(details);
    }

    pub(crate) fn issue_detail(&self, id: TaskId) -> Option<&str> {
        self.issue_details.get(&id).map(String::as_str)
    }

    /// Share terminal-outcome identity with observe, including when a terminal
    /// event exhausts a frame and worker completion arrives in the next poll.
    /// A previous turn/terminal result must not mute a genuinely new outcome.
    pub(crate) fn sessions_finished(
        &mut self,
        completions: &[AgentRuntimeCompletion],
        messages: &mut Vec<(TaskId, String)>,
    ) {
        for completion in completions {
            let Some(task) = self.tasks.get_mut(&completion.task_id) else {
                continue;
            };
            let outcome = OutcomeKey {
                turn: self
                    .turns
                    .get(&completion.task_id)
                    .map(|turn| (turn.epoch.get(), turn.turn_id)),
                terminal: None,
                outcome: completion.outcome,
            };
            if task.outcome.as_ref() == Some(&outcome) {
                continue;
            }
            task.outcome = Some(outcome);
            let words = match completion.outcome {
                AgentSessionOutcome::Clean => "Agent session stopped cleanly",
                AgentSessionOutcome::Failed => "Agent session failed",
                AgentSessionOutcome::Cancelled => "Agent session cancelled",
            };
            if let Some((_, message)) = messages
                .iter_mut()
                .find(|(id, _)| *id == completion.task_id)
            {
                // Preserve one combined announcement when task and session end
                // arrive together. Task ReadyForReview is not proof of success:
                // core deliberately preserves reviewability after a late error.
                message.push_str("; ");
                message.push_str(words);
            } else {
                messages.push((completion.task_id, words.into()));
            }
        }
    }

    /// Supply the FULL task collection, including background/archived tasks.
    /// First sightings seed silently, so restored history and reopening the
    /// panel never replay results. Call before and after domain mutations.
    pub(crate) fn observe(
        &mut self,
        snapshots: impl IntoIterator<Item = TaskAnnouncementSnapshot>,
    ) -> Vec<(TaskId, String)> {
        let mut present = HashSet::new();
        let mut messages = Vec::new();
        for snapshot in snapshots {
            let id = snapshot.id;
            present.insert(id);
            let task_words = match snapshot.status {
                TaskStatus::ReadyForReview
                    if self.turns.get(&id).is_none_or(|turn| turn.completed) =>
                {
                    Some("Ready for review")
                }
                TaskStatus::Completed => Some("Task completed"),
                TaskStatus::Failed => Some("Task failed; open Tasks for details"),
                TaskStatus::Cancelled => Some("Task cancelled"),
                _ => None,
            };
            let result = task_words.map(|_| TaskResult {
                status: snapshot.status,
                turn: self
                    .turns
                    .get(&id)
                    .map(|turn| (turn.epoch.get(), turn.turn_id)),
                terminal: snapshot.terminal,
            });
            let validation_words = match snapshot.validation_status {
                TaskValidationStatus::Passed => Some("passed"),
                TaskValidationStatus::Failed => Some("failed"),
                TaskValidationStatus::Cancelled => Some("cancelled"),
                TaskValidationStatus::Inconclusive => Some("ended without a confirmed result"),
                _ => None,
            };
            let validation =
                validation_words.map(|_| (snapshot.validation_attempt, snapshot.validation_status));
            let Some(previous) = self.tasks.get_mut(&id) else {
                self.tasks.insert(
                    id,
                    ObservedTask {
                        outcome: result.as_ref().and_then(result_outcome),
                        result,
                        validation,
                    },
                );
                continue;
            };
            let mut words = Vec::new();
            if let Some(result) = result {
                if previous.result.as_ref() != Some(&result) {
                    let outcome = result_outcome(&result);
                    let already_announced = outcome.is_some() && previous.outcome == outcome;
                    if !already_announced {
                        words.push(task_words.unwrap().to_string());
                    }
                    if outcome.is_some() {
                        previous.outcome = outcome;
                    }
                    previous.result = Some(result);
                }
            }
            if let Some(validation) = validation {
                if previous.validation != Some(validation) {
                    words.push(format!(
                        "Validation #{} {}",
                        validation.0,
                        validation_words.unwrap()
                    ));
                    previous.validation = Some(validation);
                }
            }
            if !words.is_empty() {
                messages.push((id, words.join("; ")));
            }
        }
        self.tasks.retain(|id, _| present.contains(id));
        self.turns.retain(|id, _| present.contains(id));
        self.issue_details.retain(|id, _| present.contains(id));
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{
        AgentEventKind, AgentEventStream, AgentProvider, NewTask, TaskManager,
    };

    fn fixture() -> (TaskManager, TaskId, AgentEventStream) {
        let mut tasks = TaskManager::new();
        let id = tasks
            .create(NewTask {
                title: "Same name".into(),
                provider: AgentProvider::Codex,
                repo_root: "/fixture".into(),
                worktree_path: "/fixture/task".into(),
                branch: "test/announce".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap();
        let stream = tasks.start_agent_event_stream(id).unwrap();
        (tasks, id, stream)
    }
    fn observe(state: &mut TaskAnnouncements, tasks: &TaskManager) -> Vec<(TaskId, String)> {
        state.observe(tasks.tasks().iter().map(|task| TaskAnnouncementSnapshot {
            id: task.id,
            status: task.status,
            terminal: task.terminal_session_id.clone(),
            validation_attempt: task.validation.attempt,
            validation_status: task.validation.status,
        }))
    }
    // Core's report tests exercise the actual capture helper. Here the real
    // reducer supplies coherent accepted boundaries to the frontend consumer.
    fn apply(
        state: &mut TaskAnnouncements,
        tasks: &mut TaskManager,
        stream: &AgentEventStream,
        sequence: u64,
        kind: AgentEventKind,
    ) {
        let boundary = match kind {
            AgentEventKind::TurnStarted { turn_id } => Some((turn_id, false)),
            AgentEventKind::TurnCompleted { turn_id } => Some((turn_id, true)),
            _ => None,
        };
        let status = tasks
            .apply_agent_event(stream.event(sequence + 1, kind, None))
            .unwrap();
        if let Some((turn_id, completed)) = boundary {
            state.applied_turns(&[AppliedAgentTurn {
                task_id: stream.task_id(),
                epoch: stream.epoch(),
                turn_id,
                completed,
                status,
            }]);
        }
    }
    fn begin(state: &mut TaskAnnouncements, tasks: &mut TaskManager, stream: &AgentEventStream) {
        apply(
            state,
            tasks,
            stream,
            0,
            AgentEventKind::SessionStarted {
                provider_session_id: None,
                resumed: false,
            },
        );
    }

    #[test]
    fn driver_ahead_cannot_announce_or_suppress_actual_new_turn() {
        let (mut tasks, id, stream) = fixture();
        let mut state = TaskAnnouncements::default();
        observe(&mut state, &tasks);
        begin(&mut state, &mut tasks, &stream);
        let old = AgentTurnId::new();
        let new = AgentTurnId::new();
        apply(
            &mut state,
            &mut tasks,
            &stream,
            1,
            AgentEventKind::TurnStarted { turn_id: old },
        );
        apply(
            &mut state,
            &mut tasks,
            &stream,
            2,
            AgentEventKind::TurnCompleted { turn_id: old },
        );
        assert_eq!(
            observe(&mut state, &tasks),
            vec![(id, "Ready for review".into())]
        );
        // An asynchronous driver now presents StartingTurn(new), while reducer
        // still says Ready(old). Driver views are not an input to this code.
        assert!(observe(&mut state, &tasks).is_empty());
        apply(
            &mut state,
            &mut tasks,
            &stream,
            3,
            AgentEventKind::TurnStarted { turn_id: new },
        );
        assert!(observe(&mut state, &tasks).is_empty());
        apply(
            &mut state,
            &mut tasks,
            &stream,
            4,
            AgentEventKind::TurnCompleted { turn_id: new },
        );
        assert_eq!(observe(&mut state, &tasks).len(), 1);
        assert!(observe(&mut state, &tasks).is_empty());
    }

    #[test]
    fn same_poll_start_finish_and_cross_tick_tokens_are_coherent() {
        let (mut tasks, _, stream) = fixture();
        let mut state = TaskAnnouncements::default();
        observe(&mut state, &tasks);
        begin(&mut state, &mut tasks, &stream);
        for (start, end) in [(1, 2), (3, 68)] {
            let turn = AgentTurnId::new();
            apply(
                &mut state,
                &mut tasks,
                &stream,
                start,
                AgentEventKind::TurnStarted { turn_id: turn },
            );
            for sequence in start + 1..end {
                apply(
                    &mut state,
                    &mut tasks,
                    &stream,
                    sequence,
                    AgentEventKind::TextDelta,
                );
                assert!(observe(&mut state, &tasks).is_empty());
            }
            apply(
                &mut state,
                &mut tasks,
                &stream,
                end,
                AgentEventKind::TurnCompleted { turn_id: turn },
            );
            assert_eq!(observe(&mut state, &tasks).len(), 1);
        }
    }

    #[test]
    fn failed_later_turn_is_not_announced_ready_when_reviewability_survives() {
        let (mut tasks, id, stream) = fixture();
        let mut state = TaskAnnouncements::default();
        observe(&mut state, &tasks);
        begin(&mut state, &mut tasks, &stream);
        let a = AgentTurnId::new();
        let b = AgentTurnId::new();
        apply(
            &mut state,
            &mut tasks,
            &stream,
            1,
            AgentEventKind::TurnStarted { turn_id: a },
        );
        apply(
            &mut state,
            &mut tasks,
            &stream,
            2,
            AgentEventKind::TurnCompleted { turn_id: a },
        );
        observe(&mut state, &tasks);
        apply(
            &mut state,
            &mut tasks,
            &stream,
            3,
            AgentEventKind::TurnStarted { turn_id: b },
        );
        apply(
            &mut state,
            &mut tasks,
            &stream,
            4,
            AgentEventKind::Error { fatal: true },
        );
        assert_eq!(tasks.get(id).unwrap().status, TaskStatus::ReadyForReview);
        let mut messages = observe(&mut state, &tasks);
        assert!(messages.is_empty());
        let completion = AgentRuntimeCompletion {
            task_id: id,
            outcome: AgentSessionOutcome::Failed,
            cause: crate::agent_task::CodexAppServerExitCause::IoFailed,
            detail: Some("not spoken".into()),
        };
        state.sessions_finished(std::slice::from_ref(&completion), &mut messages);
        assert_eq!(messages, vec![(id, "Agent session failed".into())]);
        let mut repeated = Vec::new();
        state.sessions_finished(&[completion], &mut repeated);
        assert!(repeated.is_empty());
    }

    #[test]
    fn cancelled_later_turn_is_announced_once_when_reviewability_survives() {
        let (mut tasks, id, stream) = fixture();
        let mut state = TaskAnnouncements::default();
        observe(&mut state, &tasks);
        begin(&mut state, &mut tasks, &stream);
        let a = AgentTurnId::new();
        let b = AgentTurnId::new();
        apply(
            &mut state,
            &mut tasks,
            &stream,
            1,
            AgentEventKind::TurnStarted { turn_id: a },
        );
        apply(
            &mut state,
            &mut tasks,
            &stream,
            2,
            AgentEventKind::TurnCompleted { turn_id: a },
        );
        observe(&mut state, &tasks);
        apply(
            &mut state,
            &mut tasks,
            &stream,
            3,
            AgentEventKind::TurnStarted { turn_id: b },
        );
        apply(
            &mut state,
            &mut tasks,
            &stream,
            4,
            AgentEventKind::SessionEnded {
                outcome: AgentSessionOutcome::Cancelled,
            },
        );
        assert_eq!(tasks.get(id).unwrap().status, TaskStatus::ReadyForReview);
        let mut messages = observe(&mut state, &tasks);
        assert!(messages.is_empty());
        let completion = AgentRuntimeCompletion {
            task_id: id,
            outcome: AgentSessionOutcome::Cancelled,
            cause: crate::agent_task::CodexAppServerExitCause::IoFailed,
            detail: Some("not spoken".into()),
        };
        state.sessions_finished(std::slice::from_ref(&completion), &mut messages);
        assert_eq!(messages, vec![(id, "Agent session cancelled".into())]);
        let mut repeated = Vec::new();
        state.sessions_finished(&[completion], &mut repeated);
        assert!(repeated.is_empty());
    }

    #[test]
    fn initial_history_panel_reopen_and_validation_attempts() {
        let id = TaskId::new();
        let mut state = TaskAnnouncements::default();
        let snapshot = |attempt, status| TaskAnnouncementSnapshot {
            id,
            status: TaskStatus::ReadyForReview,
            terminal: None,
            validation_attempt: attempt,
            validation_status: status,
        };
        for _ in 0..100 {
            assert!(state
                .observe([snapshot(1, TaskValidationStatus::Passed)])
                .is_empty());
        }
        for attempt in 2..=3 {
            assert!(state
                .observe([snapshot(attempt, TaskValidationStatus::Running)])
                .is_empty());
            assert_eq!(
                state.observe([snapshot(attempt, TaskValidationStatus::Failed)]),
                vec![(id, format!("Validation #{attempt} failed"))]
            );
            assert!(state
                .observe([snapshot(attempt, TaskValidationStatus::Failed)])
                .is_empty());
        }
        state.retain_issue_details(HashMap::from([(id, "safe explanation".into())]));
        assert_eq!(state.issue_detail(id), Some("safe explanation"));
        state.observe([]);
        assert!(state.tasks.is_empty() && state.turns.is_empty() && state.issue_details.is_empty());
    }

    #[test]
    fn independent_tasks_completion_failure_and_terminal_retry() {
        let a = TaskId::new();
        let b = TaskId::new();
        let mut state = TaskAnnouncements::default();
        let snapshot = |id, status, terminal: &str| TaskAnnouncementSnapshot {
            id,
            status,
            terminal: Some(terminal.into()),
            validation_attempt: 0,
            validation_status: TaskValidationStatus::NotRun,
        };
        state.observe([
            snapshot(a, TaskStatus::Working, "a"),
            snapshot(b, TaskStatus::Working, "b"),
        ]);
        let messages = state.observe([
            snapshot(a, TaskStatus::Completed, "a"),
            snapshot(b, TaskStatus::Failed, "b"),
        ]);
        assert_eq!(messages.len(), 2);
        assert_eq!((messages[0].0, messages[1].0), (a, b));
        assert!(state
            .observe([
                snapshot(a, TaskStatus::Completed, "a"),
                snapshot(b, TaskStatus::Failed, "b")
            ])
            .is_empty());
        assert_eq!(
            state
                .observe([
                    snapshot(a, TaskStatus::Completed, "a"),
                    snapshot(b, TaskStatus::Failed, "retry")
                ])
                .len(),
            1
        );
    }
}

#[cfg(test)]
mod reviewer_regressions {
    use crate::agent_task::*;
    use crate::task_announcements::{TaskAnnouncementSnapshot, TaskAnnouncements};
    fn observe(state: &mut TaskAnnouncements, tasks: &TaskManager) -> Vec<(TaskId, String)> {
        state.observe(tasks.tasks().iter().map(|t| TaskAnnouncementSnapshot {
            id: t.id,
            status: t.status,
            terminal: t.terminal_session_id.clone(),
            validation_attempt: t.validation.attempt,
            validation_status: t.validation.status,
        }))
    }
    fn split_outcome(outcome: AgentSessionOutcome, task_message: &str, same_poll: bool) {
        let mut tasks = TaskManager::new();
        let id = tasks
            .create(NewTask {
                title: "fixture".into(),
                provider: AgentProvider::Codex,
                repo_root: "/fixture".into(),
                worktree_path: "/fixture/task".into(),
                branch: "test/review".into(),
                base_commit: "a".repeat(40),
                source_context: None,
            })
            .unwrap();
        let stream = tasks.start_agent_event_stream(id).unwrap();
        let mut state = TaskAnnouncements::default();
        observe(&mut state, &tasks);
        tasks
            .apply_agent_event(stream.event(
                1,
                AgentEventKind::SessionStarted {
                    provider_session_id: None,
                    resumed: false,
                },
                None,
            ))
            .unwrap();
        let turn = AgentTurnId::new();
        let status = tasks
            .apply_agent_event(stream.event(2, AgentEventKind::TurnStarted { turn_id: turn }, None))
            .unwrap();
        state.applied_turns(&[AppliedAgentTurn {
            task_id: id,
            epoch: stream.epoch(),
            turn_id: turn,
            completed: false,
            status,
        }]);
        for sequence in 3..16 {
            tasks
                .apply_agent_event(stream.event(sequence, AgentEventKind::TextDelta, None))
                .unwrap();
        }
        // The 16th event consumes this task's entire production allowance.
        // poll cannot see queue_drained until the following frame, so its
        // completion report is necessarily absent from this observation.
        tasks
            .apply_agent_event(stream.event(
                16,
                AgentEventKind::SessionEnded { outcome },
                Some("transport stopped".into()),
            ))
            .unwrap();
        let first = observe(&mut state, &tasks);
        assert_eq!(first, vec![(id, task_message.into())]);
        let mut second = if same_poll {
            first.clone()
        } else {
            observe(&mut state, &tasks)
        };
        state.sessions_finished(
            &[AgentRuntimeCompletion {
                task_id: id,
                outcome,
                cause: CodexAppServerExitCause::IoFailed,
                detail: None,
            }],
            &mut second,
        );
        if same_poll {
            assert_eq!(
                second, first,
                "same-poll completion must share the emitted outcome"
            );
        } else {
            assert!(
                second.is_empty(),
                "delayed completion must not repeat the already-announced terminal outcome"
            );
        }
    }
    #[test]
    fn split_terminal_event_and_completion_announces_failure_once() {
        split_outcome(
            AgentSessionOutcome::Failed,
            "Task failed; open Tasks for details",
            false,
        );
    }
    #[test]
    fn split_terminal_event_and_completion_announces_cancellation_once() {
        split_outcome(AgentSessionOutcome::Cancelled, "Task cancelled", false);
    }
    #[test]
    fn same_poll_failed_and_cancelled_outcomes_are_each_spoken_once() {
        split_outcome(
            AgentSessionOutcome::Failed,
            "Task failed; open Tasks for details",
            true,
        );
        split_outcome(AgentSessionOutcome::Cancelled, "Task cancelled", true);
    }
}

#[cfg(test)]
#[path = "task_announcements_native.rs"]
mod native_tests;
