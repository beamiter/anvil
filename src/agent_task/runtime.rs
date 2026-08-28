//! Runtime owner for provider-native Agent sessions.
//!
//! [`TaskManager`] remains the authoritative lifecycle reducer. This module
//! owns the live provider adapters, drains their already-bounded event queues
//! with an additional frame budget, and retains the adapter's bounded view
//! after its worker has stopped. No method here waits for a running worker.

use super::drivers::codex_app_server::{
    CodexAppServerDriver, CodexAppServerExitCause, CodexAppServerExitReport,
    CodexAppServerViewSnapshot,
};
use super::native::{
    build_native_follow_up_prompt, build_native_task_prompt, prepare_native_agent_workspace,
    NativePromptError, NativePromptPolicy, NativeWorkspaceError, PreparedNativeCodexHome,
    PreparedNativeWorkspace,
};
use super::{
    AgentCommand, AgentDriver, AgentDriverError, AgentEventError, AgentEventStream,
    AgentLaunchError, AgentLaunchSpec, AgentPrompt, AgentProvider, AgentSessionOutcome,
    AgentStartRequest, AgentTask, ApprovalDecision, ApprovalId, NativeCodexHomeError, TaskId,
    TaskManager, TaskRuntimeKind, TaskStatus, TaskValidationStatus,
};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Global event-work limit for one UI frame.
///
/// Driver events are independently capped at 64 KiB, so this also places a
/// conservative 4 MiB upper bound on provider event data inspected per frame.
pub const NATIVE_AGENT_EVENTS_PER_FRAME: usize = 64;
/// Prevent one noisy provider from consuming the whole global frame budget.
pub const NATIVE_AGENT_EVENTS_PER_TASK_PER_FRAME: usize = 16;
/// Bound simultaneous native preparations and live provider sessions.
/// Idle multi-turn sessions retain a process, cgroup, credential grant, and
/// pinned workspace, so they count against the same product-wide resource cap.
pub const NATIVE_AGENT_PREPARATIONS_MAX: usize = 8;

struct RunningCodexAgent {
    driver: CodexAppServerDriver,
    stream: AgentEventStream,
    worker_joined: bool,
    forced_failure: Option<String>,
    exit_report: Option<CodexAppServerExitReport>,
    pending_prompt: Option<super::AgentTurnId>,
    finish_requested: bool,
}

struct RetainedCodexAgent {
    view: CodexAppServerViewSnapshot,
    exit_report: Option<CodexAppServerExitReport>,
    effective_outcome: AgentSessionOutcome,
}

struct PreparedCodexStart {
    task: AgentTask,
    workspace: PreparedNativeWorkspace,
    prompt: AgentPrompt,
    launch_argv: Vec<String>,
    native_home: PreparedNativeCodexHome,
}

struct PreparationResult {
    generation: u64,
    result: Result<PreparedCodexStart, AgentRuntimeError>,
}

struct PendingCodexPreparation {
    generation: u64,
    policy: NativePromptPolicy,
    receiver: Receiver<PreparationResult>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

/// Process-local owner of native provider adapters and their bounded UI views.
///
/// Runtime instances are deliberately not serialized. Stable task metadata is
/// owned by [`TaskManager`]; provider workers and descriptor capabilities must
/// be recreated explicitly after process restart.
#[derive(Default)]
pub struct AgentRuntimeManager {
    preparing: HashMap<TaskId, PendingCodexPreparation>,
    cancelled_preparations: Vec<PendingCodexPreparation>,
    running: HashMap<TaskId, RunningCodexAgent>,
    retained: HashMap<TaskId, RetainedCodexAgent>,
    next_preparation_generation: u64,
    next_poll_index: usize,
}

#[derive(Debug)]
pub enum AgentRuntimeError {
    UnknownTask(TaskId),
    AlreadyRunning(TaskId),
    NotRunning(TaskId),
    UnsupportedProvider {
        task_id: TaskId,
        provider: AgentProvider,
    },
    Workspace(NativeWorkspaceError),
    Prompt(NativePromptError),
    NativeHome(NativeCodexHomeError),
    Launch(AgentLaunchError),
    Driver(AgentDriverError),
    Event(AgentEventError),
    Preparation(String),
    StartRollback {
        start: Box<AgentRuntimeError>,
        rollback: AgentEventError,
    },
}

impl fmt::Display for AgentRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTask(task_id) => write!(formatter, "Agent task {task_id} is unavailable"),
            Self::AlreadyRunning(task_id) => {
                write!(
                    formatter,
                    "native Agent task {task_id} is already preparing or running"
                )
            }
            Self::NotRunning(task_id) => {
                write!(
                    formatter,
                    "native Agent task {task_id} is not preparing or running"
                )
            }
            Self::UnsupportedProvider { task_id, provider } => write!(
                formatter,
                "native Agent task {task_id} uses unsupported provider {}",
                provider.display_name()
            ),
            Self::Workspace(error) => error.fmt(formatter),
            Self::Prompt(error) => error.fmt(formatter),
            Self::NativeHome(error) => error.fmt(formatter),
            Self::Launch(error) => error.fmt(formatter),
            Self::Driver(error) => error.fmt(formatter),
            Self::Event(error) => error.fmt(formatter),
            Self::Preparation(detail) => {
                write!(formatter, "native Agent preparation failed: {detail}")
            }
            Self::StartRollback { start, rollback } => write!(
                formatter,
                "{start}; native stream rollback also failed: {rollback}"
            ),
        }
    }
}

impl std::error::Error for AgentRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Workspace(error) => Some(error),
            Self::Prompt(error) => Some(error),
            Self::NativeHome(error) => Some(error),
            Self::Launch(error) => Some(error),
            Self::Driver(error) => Some(error),
            Self::Event(error) => Some(error),
            Self::StartRollback { start, .. } => Some(start.as_ref()),
            Self::UnknownTask(_)
            | Self::AlreadyRunning(_)
            | Self::NotRunning(_)
            | Self::Preparation(_)
            | Self::UnsupportedProvider { .. } => None,
        }
    }
}

impl From<NativeWorkspaceError> for AgentRuntimeError {
    fn from(error: NativeWorkspaceError) -> Self {
        Self::Workspace(error)
    }
}

impl From<NativePromptError> for AgentRuntimeError {
    fn from(error: NativePromptError) -> Self {
        Self::Prompt(error)
    }
}

impl From<NativeCodexHomeError> for AgentRuntimeError {
    fn from(error: NativeCodexHomeError) -> Self {
        Self::NativeHome(error)
    }
}

impl From<AgentLaunchError> for AgentRuntimeError {
    fn from(error: AgentLaunchError) -> Self {
        Self::Launch(error)
    }
}

impl From<AgentDriverError> for AgentRuntimeError {
    fn from(error: AgentDriverError) -> Self {
        Self::Driver(error)
    }
}

impl From<AgentEventError> for AgentRuntimeError {
    fn from(error: AgentEventError) -> Self {
        Self::Event(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRuntimeIssue {
    pub task_id: TaskId,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentRuntimePollReport {
    pub preparations_finished: usize,
    pub preparations_started: usize,
    pub events_drained: usize,
    pub events_applied: usize,
    pub workers_finished: usize,
    pub budget_exhausted: bool,
    pub issues: Vec<AgentRuntimeIssue>,
    pub completions: Vec<AgentRuntimeCompletion>,
}

impl AgentRuntimePollReport {
    pub fn made_progress(&self) -> bool {
        self.preparations_finished > 0
            || self.preparations_started > 0
            || self.events_drained > 0
            || self.workers_finished > 0
            || !self.issues.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRuntimeCompletion {
    pub task_id: TaskId,
    pub outcome: AgentSessionOutcome,
    pub cause: CodexAppServerExitCause,
    pub detail: Option<String>,
}

impl AgentRuntimeManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin preparing a real Codex app-server session without blocking the UI.
    ///
    /// Workspace identity, prompt consent, launcher trust, and the private
    /// Codex home are resolved on a background worker. The task remains
    /// Created/Unassigned until [`Self::poll`] receives the matching generation,
    /// rechecks the immutable task snapshot, and atomically selects native
    /// authority. Dropping a cancelled/stale receiver drops every prepared FD,
    /// credential buffer, and private directory when the worker finishes.
    pub fn start_codex(
        &mut self,
        task_manager: &mut TaskManager,
        task_id: TaskId,
        policy: NativePromptPolicy,
    ) -> Result<(), AgentRuntimeError> {
        if self.running.contains_key(&task_id) || self.preparing.contains_key(&task_id) {
            return Err(AgentRuntimeError::AlreadyRunning(task_id));
        }
        self.reap_cancelled_preparations();
        if self
            .preparing
            .len()
            .saturating_add(self.cancelled_preparations.len())
            .saturating_add(self.running.len())
            >= NATIVE_AGENT_PREPARATIONS_MAX
        {
            return Err(AgentRuntimeError::Preparation(format!(
                "at most {NATIVE_AGENT_PREPARATIONS_MAX} native tasks may prepare or remain live concurrently"
            )));
        }
        let task = task_manager
            .get(task_id)
            .cloned()
            .ok_or(AgentRuntimeError::UnknownTask(task_id))?;
        if task.provider != AgentProvider::Codex {
            return Err(AgentRuntimeError::UnsupportedProvider {
                task_id,
                provider: task.provider,
            });
        }
        if !policy.share_command_context {
            return Err(AgentRuntimeError::Prompt(
                NativePromptError::SharingDisabled,
            ));
        }
        if task.status != TaskStatus::Created
            || task.runtime_kind != TaskRuntimeKind::Unassigned
            || task.terminal_session_id.is_some()
            || task.validation.status == TaskValidationStatus::Running
            || task_manager.has_active_agent_event_stream(task_id)
        {
            return Err(AgentRuntimeError::Preparation(format!(
                "task must remain Created and unassigned (currently {} / {:?})",
                task.status.label(),
                task.runtime_kind
            )));
        }

        let generation = self
            .next_preparation_generation
            .checked_add(1)
            .ok_or_else(|| AgentRuntimeError::Preparation("generation counter exhausted".into()))?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker = std::thread::Builder::new()
            .name(format!("anvil-codex-prepare-{task_id}-{generation}"))
            .spawn(move || {
                let result = prepare_codex_start(task, policy, worker_cancel);
                let _ = sender.send(PreparationResult { generation, result });
            })
            .map_err(|error| {
                AgentRuntimeError::Preparation(format!(
                    "could not start background preparation worker: {error}"
                ))
            })?;
        self.next_preparation_generation = generation;
        self.preparing.insert(
            task_id,
            PendingCodexPreparation {
                generation,
                policy,
                receiver,
                cancel,
                worker: Some(worker),
            },
        );
        Ok(())
    }

    fn start_prepared_codex(
        &mut self,
        task_manager: &mut TaskManager,
        prepared: PreparedCodexStart,
    ) -> Result<(), AgentRuntimeError> {
        let task_id = prepared.task.id;
        let current = task_manager
            .get(task_id)
            .ok_or(AgentRuntimeError::UnknownTask(task_id))?;
        if current != &prepared.task {
            return Err(AgentRuntimeError::Preparation(
                "task changed while native prerequisites were being prepared".into(),
            ));
        }
        let stream = task_manager.start_agent_event_stream(task_id)?;
        let worktree_path = prepared.task.worktree_path;
        let mut driver = CodexAppServerDriver::new(
            prepared.launch_argv,
            prepared.workspace,
            prepared.native_home,
        );
        let request = AgentStartRequest {
            provider: AgentProvider::Codex,
            stream: stream.clone(),
            worktree_path,
            // The prompt contains the policy-approved, redacted evidence. Do
            // not also hand the adapter the raw semantic snapshot.
            source_context: None,
            initial_prompt: Some(prepared.prompt),
            resume_from: None,
        };
        if let Err(error) = driver.start(request) {
            // CodexAppServerDriver::start has a strict pre-spawn error
            // contract: once it has created a worker it returns Ok and reports
            // every later spawn/protocol/process failure asynchronously. That
            // makes this stopped-runtime rollback authoritative.
            driver.cancel();
            return Err(rollback_failed_start(
                task_manager,
                &stream,
                AgentRuntimeError::Driver(error),
            ));
        }

        self.retained.remove(&task_id);
        self.running.insert(
            task_id,
            RunningCodexAgent {
                driver,
                stream,
                worker_joined: false,
                forced_failure: None,
                exit_report: None,
                pending_prompt: None,
                finish_requested: false,
            },
        );
        Ok(())
    }

    /// Drain native events without waiting for future provider work.
    ///
    /// A completed worker is joined only after `worker_is_finished` reports
    /// true and its event queue has been drained for this frame. The exit report
    /// is accepted as stop authority only when no child was spawned or the
    /// spawned child was reaped.
    pub fn poll(
        &mut self,
        task_manager: &mut TaskManager,
        current_policy: NativePromptPolicy,
    ) -> AgentRuntimePollReport {
        let mut report = AgentRuntimePollReport::default();
        self.reap_cancelled_preparations();
        self.poll_preparations(task_manager, current_policy, &mut report);
        let mut task_ids: Vec<_> = self.running.keys().copied().collect();
        if task_ids.is_empty() {
            self.next_poll_index = 0;
            return report;
        }

        let start = self.next_poll_index % task_ids.len();
        task_ids.rotate_left(start);
        self.next_poll_index = (start + 1) % task_ids.len();
        let mut remaining = NATIVE_AGENT_EVENTS_PER_FRAME;

        for task_id in task_ids {
            if remaining == 0 {
                report.budget_exhausted = true;
                break;
            }
            let allowance = remaining.min(NATIVE_AGENT_EVENTS_PER_TASK_PER_FRAME);
            let mut queue_drained = false;
            let mut completion = None;

            {
                let Some(runtime) = self.running.get_mut(&task_id) else {
                    continue;
                };
                for _ in 0..allowance {
                    match runtime.driver.try_next_event() {
                        Ok(Some(event)) => {
                            remaining -= 1;
                            report.events_drained += 1;

                            // Once a terminal event has removed the stream,
                            // discard any provider protocol tail. It has no
                            // remaining lifecycle authority.
                            if runtime.forced_failure.is_some()
                                || !task_manager.has_active_agent_event_stream(task_id)
                            {
                                continue;
                            }
                            let started_turn = match event.kind() {
                                super::AgentEventKind::TurnStarted { turn_id } => Some(*turn_id),
                                _ => None,
                            };
                            match task_manager.apply_agent_event(event) {
                                Ok(_) => {
                                    report.events_applied += 1;
                                    if started_turn.is_some()
                                        && runtime.pending_prompt == started_turn
                                    {
                                        runtime.pending_prompt = None;
                                    }
                                }
                                Err(error) => {
                                    let detail = bounded_runtime_detail(format!(
                                        "native Agent event was rejected: {error}"
                                    ));
                                    if runtime.forced_failure.is_none() {
                                        runtime.forced_failure = Some(detail.clone());
                                    }
                                    runtime.driver.cancel();
                                    report.issues.push(AgentRuntimeIssue { task_id, detail });
                                }
                            }
                        }
                        Ok(None) | Err(AgentDriverError::Closed) => {
                            queue_drained = true;
                            break;
                        }
                        Err(error) => {
                            let detail = bounded_runtime_detail(format!(
                                "native Agent event transport failed: {error}"
                            ));
                            if runtime.forced_failure.is_none() {
                                runtime.forced_failure = Some(detail.clone());
                            }
                            runtime.driver.cancel();
                            report.issues.push(AgentRuntimeIssue { task_id, detail });
                            queue_drained = true;
                            break;
                        }
                    }
                }

                let worker_stopped = runtime.worker_joined || runtime.driver.worker_is_finished();
                if queue_drained && worker_stopped {
                    if !runtime.worker_joined {
                        match runtime.driver.join_finished_worker() {
                            Ok(true) => runtime.worker_joined = true,
                            Ok(false) => {}
                            Err(error) => {
                                runtime.worker_joined = true;
                                let detail = bounded_runtime_detail(format!(
                                    "native Agent worker join failed: {error}"
                                ));
                                if runtime.forced_failure.is_none() {
                                    runtime.forced_failure = Some(detail.clone());
                                }
                                report.issues.push(AgentRuntimeIssue { task_id, detail });
                            }
                        }
                    }
                    if runtime.worker_joined && runtime.exit_report.is_none() {
                        runtime.exit_report = runtime.driver.take_exit_report();
                    }
                    if runtime.exit_report.as_ref().is_some_and(|exit| {
                        !exit.process.spawned
                            || (exit.process.reaped && exit.process.containment_verified_empty)
                    }) {
                        let exit = runtime
                            .exit_report
                            .take()
                            .expect("exit report was checked above");
                        let outcome = if runtime.forced_failure.is_some() {
                            AgentSessionOutcome::Failed
                        } else {
                            exit.outcome
                        };
                        let detail = runtime
                            .forced_failure
                            .clone()
                            .or_else(|| exit.detail.clone());
                        completion = Some((
                            runtime.stream.clone(),
                            runtime.driver.view_snapshot(),
                            exit,
                            outcome,
                            detail,
                        ));
                    }
                }
            }

            let Some((stream, view, exit_report, outcome, detail)) = completion else {
                continue;
            };
            // The report proves the worker has stopped and its child either was
            // never spawned or was reaped. Only now may validation be unlocked.
            if task_manager.has_active_agent_event_stream(task_id) {
                if let Err(error) = task_manager.finish_agent_event_stream_after_stop(
                    &stream,
                    outcome,
                    detail.clone(),
                ) {
                    report.issues.push(AgentRuntimeIssue {
                        task_id,
                        detail: bounded_runtime_detail(format!(
                            "native Agent exit could not update its task: {error}"
                        )),
                    });
                }
            }
            self.running.remove(&task_id);
            report.completions.push(AgentRuntimeCompletion {
                task_id,
                outcome,
                cause: exit_report.cause,
                detail: detail.clone(),
            });
            self.retained.insert(
                task_id,
                RetainedCodexAgent {
                    view,
                    exit_report: Some(exit_report),
                    effective_outcome: outcome,
                },
            );
            report.workers_finished += 1;
        }

        // Consuming the full global allowance means more provider work may be
        // queued even if the final dequeue happened to empty a queue exactly.
        report.budget_exhausted |= remaining == 0;
        report
    }

    pub fn cancel(&mut self, task_id: TaskId) -> Result<(), AgentRuntimeError> {
        if let Some(preparation) = self.preparing.remove(&task_id) {
            preparation.cancel.store(true, Ordering::Release);
            // Keep ownership of the receiver until the worker acknowledges
            // cancellation or exits. This bounds rapid Start/Cancel cycles to
            // the same global worker cap instead of detaching unlimited Git
            // and credential-preparation threads.
            self.cancelled_preparations.push(preparation);
            return Ok(());
        }
        let runtime = self
            .running
            .get_mut(&task_id)
            .ok_or(AgentRuntimeError::NotRunning(task_id))?;
        runtime.driver.cancel();
        Ok(())
    }

    pub fn decide_approval(
        &mut self,
        task_id: TaskId,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), AgentRuntimeError> {
        let runtime = self
            .running
            .get_mut(&task_id)
            .ok_or(AgentRuntimeError::NotRunning(task_id))?;
        runtime
            .driver
            .send(AgentCommand::DecideApproval {
                id: approval_id,
                decision,
            })
            .map_err(AgentRuntimeError::Driver)
    }

    /// Start another bounded user turn on the same loaded Codex thread.
    ///
    /// The task reducer and driver both repeat the idle-session gate. The
    /// driver's nonblocking command boundary reserves the idle phase before
    /// returning so duplicate UI actions cannot queue overlapping turns.
    pub fn prompt_codex(
        &mut self,
        task_manager: &TaskManager,
        task_id: TaskId,
        text: &str,
        policy: NativePromptPolicy,
    ) -> Result<(), AgentRuntimeError> {
        let task = task_manager
            .get(task_id)
            .ok_or(AgentRuntimeError::UnknownTask(task_id))?;
        if task.status != TaskStatus::ReadyForReview
            || task.runtime_kind != TaskRuntimeKind::Native
            || !task_manager.has_active_agent_event_stream(task_id)
        {
            return Err(AgentRuntimeError::Preparation(format!(
                "task must be at a live native review point (currently {} / {:?})",
                task.status.label(),
                task.runtime_kind
            )));
        }
        let prompt = build_native_follow_up_prompt(text, policy)?;
        let turn_id = prompt.turn_id;
        let runtime = self
            .running
            .get_mut(&task_id)
            .ok_or(AgentRuntimeError::NotRunning(task_id))?;
        if runtime.pending_prompt.is_some() || runtime.finish_requested {
            return Err(AgentRuntimeError::Preparation(
                "native Codex session already has a queued turn or finish request".into(),
            ));
        }
        runtime
            .driver
            .send(AgentCommand::Prompt(prompt))
            .map_err(AgentRuntimeError::Driver)?;
        runtime.pending_prompt = Some(turn_id);
        Ok(())
    }

    /// Finish an idle live session. Validation remains locked until the worker
    /// has stopped, containment is empty, and the provider leader is reaped.
    pub fn finish_codex(
        &mut self,
        task_manager: &TaskManager,
        task_id: TaskId,
    ) -> Result<(), AgentRuntimeError> {
        let task = task_manager
            .get(task_id)
            .ok_or(AgentRuntimeError::UnknownTask(task_id))?;
        if task.status != TaskStatus::ReadyForReview
            || task.runtime_kind != TaskRuntimeKind::Native
            || !task_manager.has_active_agent_event_stream(task_id)
        {
            return Err(AgentRuntimeError::Preparation(format!(
                "task must be at a live native review point (currently {} / {:?})",
                task.status.label(),
                task.runtime_kind
            )));
        }
        let runtime = self
            .running
            .get_mut(&task_id)
            .ok_or(AgentRuntimeError::NotRunning(task_id))?;
        if runtime.pending_prompt.is_some() || runtime.finish_requested {
            return Err(AgentRuntimeError::Preparation(
                "native Codex session already has a queued turn or finish request".into(),
            ));
        }
        runtime
            .driver
            .send(AgentCommand::FinishSession)
            .map_err(AgentRuntimeError::Driver)?;
        runtime.finish_requested = true;
        Ok(())
    }

    /// Return the current bounded view, or the final view retained after exit.
    pub fn snapshot(&self, task_id: TaskId) -> Option<CodexAppServerViewSnapshot> {
        self.running
            .get(&task_id)
            .map(|runtime| runtime.driver.view_snapshot())
            .or_else(|| {
                self.retained
                    .get(&task_id)
                    .map(|retained| retained.view.clone())
            })
    }

    pub fn exit_report(&self, task_id: TaskId) -> Option<&CodexAppServerExitReport> {
        self.retained
            .get(&task_id)
            .and_then(|retained| retained.exit_report.as_ref())
    }

    pub fn take_exit_report(&mut self, task_id: TaskId) -> Option<CodexAppServerExitReport> {
        self.retained
            .get_mut(&task_id)
            .and_then(|retained| retained.exit_report.take())
    }

    /// True only when a fully stopped native session ended unsuccessfully and
    /// its retained process evidence can authorize an explicit PTY recovery.
    /// Clean Finish remains a review/validation path, not a silent authority
    /// switch to the opaque provider CLI.
    pub fn can_continue_in_terminal(&self, task_id: TaskId) -> bool {
        !self.running.contains_key(&task_id)
            && self.retained.get(&task_id).is_some_and(|retained| {
                matches!(
                    retained.effective_outcome,
                    AgentSessionOutcome::Failed | AgentSessionOutcome::Cancelled
                ) && retained.exit_report.as_ref().is_some_and(|exit| {
                    !exit.process.spawned
                        || (exit.process.reaped && exit.process.containment_verified_empty)
                })
            })
    }

    pub fn has_running(&self, task_id: TaskId) -> bool {
        self.running.contains_key(&task_id)
    }

    pub fn has_preparing(&self, task_id: TaskId) -> bool {
        self.preparing.contains_key(&task_id)
    }

    pub fn has_any_running(&self) -> bool {
        !self.running.is_empty()
    }

    /// True while a provider is starting, executing, waiting, or stopping.
    /// A loaded thread parked at an idle review point needs only low-frequency
    /// lifecycle polling until the user sends another turn or finishes it.
    pub fn needs_fast_poll(&self) -> bool {
        self.running
            .values()
            .any(|runtime| runtime.driver.phase() != super::CodexAppServerPhase::Ready)
    }

    pub fn has_any_activity(&self) -> bool {
        !self.preparing.is_empty()
            || !self.cancelled_preparations.is_empty()
            || !self.running.is_empty()
    }

    pub fn clear_retained(&mut self, task_id: TaskId) {
        self.retained.remove(&task_id);
    }
}

impl Drop for AgentRuntimeManager {
    fn drop(&mut self) {
        for preparation in self.preparing.values_mut() {
            preparation.cancel.store(true, Ordering::Release);
        }
        for preparation in &mut self.cancelled_preparations {
            preparation.cancel.store(true, Ordering::Release);
        }
        for runtime in self.running.values_mut() {
            runtime.driver.cancel();
        }
        for preparation in self.preparing.values_mut() {
            if let Some(worker) = preparation.worker.take() {
                let _ = worker.join();
            }
        }
        for preparation in &mut self.cancelled_preparations {
            if let Some(worker) = preparation.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

impl AgentRuntimeManager {
    fn reap_cancelled_preparations(&mut self) {
        self.cancelled_preparations.retain_mut(|pending| {
            if matches!(pending.receiver.try_recv(), Err(TryRecvError::Empty)) {
                return true;
            }
            if let Some(worker) = pending.worker.take() {
                let _ = worker.join();
            }
            false
        });
    }

    fn poll_preparations(
        &mut self,
        task_manager: &mut TaskManager,
        current_policy: NativePromptPolicy,
        report: &mut AgentRuntimePollReport,
    ) {
        let task_ids: Vec<_> = self.preparing.keys().copied().collect();
        for task_id in task_ids {
            let policy_changed = self
                .preparing
                .get(&task_id)
                .is_some_and(|pending| pending.policy != current_policy);
            if policy_changed {
                let pending = self
                    .preparing
                    .remove(&task_id)
                    .expect("preparation was checked above");
                pending.cancel.store(true, Ordering::Release);
                self.cancelled_preparations.push(pending);
                report.preparations_finished += 1;
                report.issues.push(AgentRuntimeIssue {
                    task_id,
                    detail: bounded_runtime_detail(
                        "native Agent preparation was cancelled because the AI sharing policy changed"
                            .to_string(),
                    ),
                });
                continue;
            }
            let received = self
                .preparing
                .get(&task_id)
                .map(|pending| pending.receiver.try_recv());
            let message = match received {
                Some(Ok(message)) => message,
                Some(Err(TryRecvError::Empty)) | None => continue,
                Some(Err(TryRecvError::Disconnected)) => PreparationResult {
                    generation: self
                        .preparing
                        .get(&task_id)
                        .map_or(0, |pending| pending.generation),
                    result: Err(AgentRuntimeError::Preparation(
                        "background preparation worker stopped unexpectedly".into(),
                    )),
                },
            };
            let Some(mut pending) = self.preparing.remove(&task_id) else {
                continue;
            };
            if let Some(worker) = pending.worker.take() {
                if worker.join().is_err() {
                    report.preparations_finished += 1;
                    report.issues.push(AgentRuntimeIssue {
                        task_id,
                        detail: bounded_runtime_detail(
                            "native Agent preparation worker panicked".to_string(),
                        ),
                    });
                    continue;
                }
            }
            report.preparations_finished += 1;
            if message.generation != pending.generation {
                report.issues.push(AgentRuntimeIssue {
                    task_id,
                    detail: bounded_runtime_detail(
                        "native Agent preparation generation did not match its request".to_string(),
                    ),
                });
                continue;
            }
            let result = message
                .result
                .and_then(|prepared| self.start_prepared_codex(task_manager, prepared));
            match result {
                Ok(()) => report.preparations_started += 1,
                Err(error) => report.issues.push(AgentRuntimeIssue {
                    task_id,
                    detail: bounded_runtime_detail(error.to_string()),
                }),
            }
        }
    }
}

fn preparation_cancelled(cancel: &AtomicBool) -> Result<(), AgentRuntimeError> {
    if cancel.load(Ordering::Acquire) {
        Err(AgentRuntimeError::Preparation("cancelled".into()))
    } else {
        Ok(())
    }
}

fn prepare_codex_start(
    task: AgentTask,
    policy: NativePromptPolicy,
    cancel: Arc<AtomicBool>,
) -> Result<PreparedCodexStart, AgentRuntimeError> {
    preparation_cancelled(cancel.as_ref())?;
    // The capability owns its pinned directory descriptors. It stays in this
    // result until the UI thread either consumes it or drops a stale result.
    let workspace = prepare_native_agent_workspace(&task, Arc::clone(&cancel))?;
    preparation_cancelled(cancel.as_ref())?;
    let prompt = build_native_task_prompt(&task, workspace.relative_cwd(), policy)?;
    preparation_cancelled(cancel.as_ref())?;
    let launch_argv = AgentLaunchSpec::resolve_native(
        AgentProvider::Codex,
        &task.repo_root,
        &task.worktree_path,
    )?;
    preparation_cancelled(cancel.as_ref())?;
    let native_home = PreparedNativeCodexHome::prepare()?;
    preparation_cancelled(cancel.as_ref())?;
    Ok(PreparedCodexStart {
        task,
        workspace,
        prompt,
        launch_argv,
        native_home,
    })
}

fn rollback_failed_start(
    task_manager: &mut TaskManager,
    stream: &AgentEventStream,
    start: AgentRuntimeError,
) -> AgentRuntimeError {
    let detail = bounded_runtime_detail(start.to_string());
    match task_manager.rollback_agent_event_stream_before_spawn(stream, detail) {
        Ok(_) => start,
        Err(rollback) => AgentRuntimeError::StartRollback {
            start: Box::new(start),
            rollback,
        },
    }
}

fn bounded_runtime_detail(detail: String) -> String {
    super::event::bounded_event_detail(Some(detail))
        .unwrap_or_else(|| "native Agent runtime failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_POLICY: NativePromptPolicy = NativePromptPolicy {
        share_command_context: true,
        redact_secrets: true,
    };

    fn pending_preparation(
        runtime: &mut AgentRuntimeManager,
        task_id: TaskId,
        generation: u64,
    ) -> mpsc::SyncSender<PreparationResult> {
        let (sender, receiver) = mpsc::sync_channel(1);
        runtime.preparing.insert(
            task_id,
            PendingCodexPreparation {
                generation,
                policy: TEST_POLICY,
                receiver,
                cancel: Arc::new(AtomicBool::new(false)),
                worker: None,
            },
        );
        sender
    }

    fn task_manager() -> (TaskManager, TaskId) {
        let mut tasks = TaskManager::new();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let task_id = tasks
            .create(super::super::NewTask {
                title: "async native preparation".into(),
                provider: AgentProvider::Codex,
                repo_root: format!("/tmp/anvil-runtime-test-repository-{token}").into(),
                worktree_path: format!("/tmp/anvil-runtime-test-worktree-{token}").into(),
                branch: "anvil/async-native-preparation".into(),
                base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
                source_context: None,
            })
            .unwrap();
        (tasks, task_id)
    }

    #[test]
    fn empty_runtime_has_no_running_or_retained_state() {
        let mut runtime = AgentRuntimeManager::new();
        let task_id = TaskId::new();
        assert!(!runtime.has_running(task_id));
        assert!(!runtime.has_preparing(task_id));
        assert!(!runtime.has_any_running());
        assert!(!runtime.has_any_activity());
        assert!(runtime.snapshot(task_id).is_none());
        assert!(runtime.exit_report(task_id).is_none());
        assert!(matches!(
            runtime.cancel(task_id),
            Err(AgentRuntimeError::NotRunning(id)) if id == task_id
        ));
    }

    #[test]
    fn empty_poll_is_bounded_and_idle() {
        let mut runtime = AgentRuntimeManager::new();
        let mut tasks = TaskManager::new();
        let report = runtime.poll(&mut tasks, TEST_POLICY);
        assert_eq!(report, AgentRuntimePollReport::default());
        assert!(!report.made_progress());
    }

    #[test]
    fn terminal_continuation_requires_a_stopped_unsuccessful_effective_outcome() {
        let task_id = TaskId::new();
        let retained = |effective_outcome, process| RetainedCodexAgent {
            view: CodexAppServerViewSnapshot::default(),
            exit_report: Some(CodexAppServerExitReport {
                // A runtime-forced failure may override a raw clean provider
                // report, so eligibility intentionally uses the retained
                // effective outcome instead of this field.
                outcome: AgentSessionOutcome::Clean,
                cause: super::super::CodexAppServerExitCause::Clean,
                detail: None,
                process,
                critical_event_delivery_failed: false,
                stderr_tail: String::new(),
            }),
            effective_outcome,
        };

        let stopped = super::super::CodexAppServerProcessExit {
            spawned: true,
            provider_released: true,
            reaped: true,
            containment_verified_empty: true,
            success: true,
            code: Some(0),
            signal: None,
        };
        let mut runtime = AgentRuntimeManager::new();
        runtime
            .retained
            .insert(task_id, retained(AgentSessionOutcome::Failed, stopped));
        assert!(runtime.can_continue_in_terminal(task_id));

        runtime
            .retained
            .insert(task_id, retained(AgentSessionOutcome::Clean, stopped));
        assert!(!runtime.can_continue_in_terminal(task_id));

        runtime.retained.insert(
            task_id,
            retained(
                AgentSessionOutcome::Cancelled,
                super::super::CodexAppServerProcessExit {
                    containment_verified_empty: false,
                    ..stopped
                },
            ),
        );
        assert!(!runtime.can_continue_in_terminal(task_id));
    }

    #[test]
    fn pending_preparation_is_activity_rejects_duplicate_start_and_cancels_promptly() {
        let mut runtime = AgentRuntimeManager::new();
        let task_id = TaskId::new();
        let sender = pending_preparation(&mut runtime, task_id, 7);
        let mut tasks = TaskManager::new();

        assert!(runtime.has_preparing(task_id));
        assert!(!runtime.has_running(task_id));
        assert!(runtime.has_any_activity());
        assert!(matches!(
            runtime.start_codex(
                &mut tasks,
                task_id,
                TEST_POLICY,
            ),
            Err(AgentRuntimeError::AlreadyRunning(id)) if id == task_id
        ));

        runtime.cancel(task_id).unwrap();
        assert!(!runtime.has_preparing(task_id));
        assert!(runtime.has_any_activity());
        assert!(sender
            .send(PreparationResult {
                generation: 7,
                result: Err(AgentRuntimeError::Preparation("late".into())),
            })
            .is_ok());
        assert_eq!(
            runtime.poll(&mut tasks, TEST_POLICY),
            AgentRuntimePollReport::default()
        );
        assert!(runtime.cancelled_preparations.is_empty());
        assert!(!runtime.has_any_activity());
    }

    #[test]
    fn preparation_failure_is_reported_without_selecting_task_runtime() {
        let (mut tasks, task_id) = task_manager();
        let before = tasks.get(task_id).unwrap().clone();
        let mut runtime = AgentRuntimeManager::new();
        let sender = pending_preparation(&mut runtime, task_id, 11);
        sender
            .send(PreparationResult {
                generation: 11,
                result: Err(AgentRuntimeError::Preparation(
                    "fixture prerequisite failed".into(),
                )),
            })
            .unwrap();

        let report = runtime.poll(&mut tasks, TEST_POLICY);
        assert_eq!(report.preparations_finished, 1);
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0]
            .detail
            .contains("fixture prerequisite failed"));
        assert_eq!(tasks.get(task_id), Some(&before));
        assert!(!tasks.has_active_agent_event_stream(task_id));
        assert!(!runtime.has_any_activity());
    }

    #[test]
    fn changed_sharing_policy_cancels_even_a_ready_preparation_generation() {
        let mut runtime = AgentRuntimeManager::new();
        let task_id = TaskId::new();
        let sender = pending_preparation(&mut runtime, task_id, 13);
        sender
            .send(PreparationResult {
                generation: 13,
                result: Err(AgentRuntimeError::Preparation(
                    "ready result must not win revoked consent".into(),
                )),
            })
            .unwrap();
        let mut tasks = TaskManager::new();

        let report = runtime.poll(
            &mut tasks,
            NativePromptPolicy {
                share_command_context: false,
                redact_secrets: true,
            },
        );
        assert_eq!(report.preparations_finished, 1);
        assert_eq!(report.preparations_started, 0);
        assert_eq!(report.issues.len(), 1);
        assert!(report.issues[0].detail.contains("sharing policy changed"));
        assert!(!runtime.has_preparing(task_id));
        assert!(runtime.has_any_activity());

        assert_eq!(
            runtime.poll(&mut tasks, TEST_POLICY),
            AgentRuntimePollReport::default()
        );
        assert!(!runtime.has_any_activity());
        assert!(!tasks.has_active_agent_event_stream(task_id));
    }

    #[test]
    fn cancelled_preparation_workers_remain_globally_bounded_until_they_exit() {
        let mut runtime = AgentRuntimeManager::new();
        let mut senders = Vec::new();
        for generation in 1..=NATIVE_AGENT_PREPARATIONS_MAX as u64 {
            let task_id = TaskId::new();
            senders.push(pending_preparation(&mut runtime, task_id, generation));
            runtime.cancel(task_id).unwrap();
        }
        assert_eq!(
            runtime.cancelled_preparations.len(),
            NATIVE_AGENT_PREPARATIONS_MAX
        );

        let mut tasks = TaskManager::new();
        let other = TaskId::new();
        assert!(matches!(
            runtime.start_codex(
                &mut tasks,
                other,
                TEST_POLICY,
            ),
            Err(AgentRuntimeError::Preparation(detail)) if detail.contains("at most")
        ));

        drop(senders.pop());
        assert!(matches!(
            runtime.start_codex(
                &mut tasks,
                other,
                TEST_POLICY,
            ),
            Err(AgentRuntimeError::UnknownTask(id)) if id == other
        ));
    }

    #[test]
    fn real_preparation_failure_arrives_asynchronously_and_keeps_task_retryable() {
        let (mut tasks, task_id) = task_manager();
        let before = tasks.get(task_id).unwrap().clone();
        let mut runtime = AgentRuntimeManager::new();
        runtime
            .start_codex(&mut tasks, task_id, TEST_POLICY)
            .unwrap();
        assert!(runtime.has_preparing(task_id));
        assert_eq!(tasks.get(task_id), Some(&before));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let report = loop {
            let report = runtime.poll(&mut tasks, TEST_POLICY);
            if report.preparations_finished > 0 {
                break report;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(report.preparations_finished, 1);
        assert_eq!(report.preparations_started, 0);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(tasks.get(task_id), Some(&before));
        assert!(!runtime.has_any_activity());
    }
}
