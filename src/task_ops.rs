//! Application-side execution for the agent Tasks panel.
//!
//! The panel component stages [`TaskPanelAction`] values; this module resolves
//! the task again at execution time, drives the ported `agent_task` domain
//! (task manager, native runtime, diff worker), spawns task terminals as
//! pruned tabs, and pushes composed snapshots back to the panel. The flow
//! mirrors ember's `app/tasks.rs` action executor and poll loop.

use std::collections::HashSet;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use relm4::gtk::glib;
use relm4::{ComponentController, ComponentSender};

use crate::agent_task::{
    AgentProvider, ApprovalDecision, CodexAppServerPhase, NativePromptPolicy, NewTask, TaskId,
    TaskRuntimeKind, TaskStatus, TaskTerminalRole, TaskValidationStatus,
};
use crate::agent_task_ui::{self, row_status_line, TaskPanelAction, TaskRowSnapshot};
use crate::app_msg::AppMsg;
use crate::dialogs::tasks_panel::{DiffSync, TaskDetailSync, TasksPanelMsg, TasksPanelSync};
use crate::workspace::Pane;
use crate::AppModel;

const HOSTED_VALIDATION_VIEW: &str = "anvil-hosted-validation-view";

fn is_hosted_validation_view(pane: &Pane) -> bool {
    use relm4::gtk::prelude::*;
    // This marker belongs to the view widget, so it survives pane moves and
    // disappears with the widget. It never represents execution authority.
    unsafe {
        pane.terminal
            .widget()
            .data::<bool>(HOSTED_VALIDATION_VIEW)
            .is_some_and(|value| *value.as_ref())
    }
}

/// Read-only history remains available when the attach capability has expired.
/// It is never supplied to the task reducer as execution evidence.
fn validation_history_preview(
    detail: Option<&str>,
    history: Option<&jterm_core::agent_workspace::HostedValidationSnapshot>,
    max_bytes: usize,
    multiline: bool,
) -> Option<String> {
    let mut text = detail.unwrap_or_default().to_string();
    if let Some(history) = history {
        if history.preparing {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("Preparing validation in the session host…");
        }
        if !history.output.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(if history.output_truncated {
                "Saved validation output (earlier output was not retained):\n"
            } else {
                "Saved validation output:\n"
            });
            text.push_str(&history.output);
        }
    }
    if text.is_empty() {
        return None;
    }
    let redacted = jterm_core::redact::redact_secrets(&text);
    Some(if multiline {
        jterm_core::review_input::safe_multiline_display(&redacted, max_bytes)
    } else {
        jterm_core::review_input::safe_inline_display(&redacted, max_bytes)
    })
}

/// Poll cadence while any task machinery is active; the runtime's own frame
/// budgets make each tick a bounded, nonblocking drain.
const TASKS_POLL_FAST: Duration = Duration::from_millis(120);
/// Idle cadence while tasks exist but nothing is running; keeps archived
/// tasks from ever waking the loop.
const TASKS_POLL_SLOW: Duration = Duration::from_millis(2_000);

/// Apply task-model effects for a terminal that never crossed exec.
fn record_task_terminal_launch_failure(
    manager: &mut crate::agent_task::TaskManager,
    session_id: &str,
    role: crate::agent_task::TaskTerminalRole,
) {
    match role {
        crate::agent_task::TaskTerminalRole::Validation => {
            // A validation PTY that never launched is a cancelled attempt, not
            // an inconclusive exit from a child that disappeared mid-flight.
            manager.handle_terminal_session_closed(session_id);
        }
        crate::agent_task::TaskTerminalRole::Agent => {
            manager.handle_terminal_session_exit(session_id, None);
        }
    }
}

/// Agent-task anchor gate, stricter than the shared block preflight.
///
/// [`crate::agent_task::context::block_agent_context_disabled_reason`] treats
/// commandless captured output as attachable evidence; isolated worktree tasks
/// must anchor on a shell-reported command instead.
fn block_agent_task_anchor_disabled_reason(
    evidence: &crate::block_view::BlockAgentEvidence,
) -> Option<&'static str> {
    if evidence.is_background {
        return Some("Background output blocks are not shell commands");
    }
    crate::agent_task::context::block_agent_context_disabled_reason(
        evidence.command.as_deref(),
        evidence.command_exact,
        evidence.command_truncated,
        evidence.cwd.as_deref(),
        Some(evidence.output_available),
    )
}

/// Build the semantic evidence for a new task from one block snapshot.
///
/// The synthetic execution id is panel-local provenance: it never crosses a
/// provider boundary, but keeps the task's evidence keyed to the exact block
/// it came from. Command/cwd exactness flags come from the block lifecycle,
/// so a screen scrape can never pose as the shell's own report.
fn semantic_context_from_evidence(
    pane: &Pane,
    evidence: crate::block_view::BlockAgentEvidence,
    source_shell: Option<String>,
) -> Option<crate::agent_task::SemanticCommandContext> {
    let source_session_id = pane
        .session_id
        .clone()
        .filter(|session_id| jterm_core::execution_journal::is_valid_jsh_session_id(session_id))?;
    Some(crate::agent_task::SemanticCommandContext {
        source_session_id,
        source_execution_id: format!("block-{}", evidence.block_id),
        source_sequence: evidence.block_id,
        source_shell,
        command: evidence.command,
        command_exact: evidence.command_exact,
        command_truncated: evidence.command_truncated,
        cwd: evidence.cwd.clone(),
        cwd_after: evidence.cwd,
        exit_code: evidence.exit_code,
        duration_ms: evidence.duration_ms,
        output_text: evidence.output_text,
        output_available: evidence.output_available,
        output_truncated: evidence.output_truncated,
        output_total_bytes: evidence.output_total_bytes,
        started_at: evidence.started_at,
        finished_at: evidence.finished_at,
    })
}

/// Disabling task creation must not revoke controls for already-owned work.
fn task_action_allowed(enabled: bool, action: &TaskPanelAction) -> bool {
    enabled
        || matches!(
            action,
            TaskPanelAction::Select(_)
                | TaskPanelAction::Close
                | TaskPanelAction::LoadNativeHistory(_, _)
                | TaskPanelAction::StopCodex(_)
                | TaskPanelAction::FinishCodex(_)
                | TaskPanelAction::Deny(_, _)
                | TaskPanelAction::FocusTerminal(_)
                | TaskPanelAction::FocusValidation(_)
                | TaskPanelAction::ReviewDiff(_)
                | TaskPanelAction::Archive(_)
                | TaskPanelAction::Restore(_)
                | TaskPanelAction::Complete(_)
                | TaskPanelAction::AcceptReviewed(_)
        )
}

/// Why Hide is unavailable. The manager's own error names a raw task ID.
const ARCHIVE_BLOCKED: &str =
    "Stop the Agent and wait for any running validation before hiding this task.";

impl AppModel {
    fn task_provider_name(&self, task_id: TaskId) -> &'static str {
        self.task_manager
            .get(task_id)
            .map_or("the Agent", |task| task.provider.display_name())
    }

    /// Observe authoritative task state, independent of panel visibility/selection.
    /// Keep the announcer on the persistent window, never a transient toast.
    fn announce_task_transitions(&mut self) {
        self.announce_task_transitions_with_report(&[], &[]);
    }

    fn announce_task_transitions_with_report(
        &mut self,
        issues: &[crate::agent_task::AgentRuntimeIssue],
        completions: &[crate::agent_task::AgentRuntimeCompletion],
    ) {
        use crate::task_announcements::TaskAnnouncementSnapshot;
        use relm4::gtk::prelude::Cast;
        let snapshots = self
            .task_manager
            .tasks()
            .iter()
            .map(|task| TaskAnnouncementSnapshot {
                id: task.id,
                status: task.status,
                terminal: task.terminal_session_id.clone(),
                validation_attempt: task.validation.attempt,
                validation_status: task.validation.status,
            });
        let mut messages = self.task_announcements.observe(snapshots);
        self.task_announcements
            .sessions_finished(completions, &mut messages);
        let details = agent_task_ui::merge_task_issue_feedback(&mut messages, issues);
        for (id, status) in messages {
            let message = format!(
                "{}: {status}",
                agent_task_ui::feedback_identity(&self.task_manager, id)
            );
            agent_task_ui::deliver_task_feedback(
                self.window.upcast_ref::<relm4::gtk::Widget>(),
                &self.task_feedback,
                &message,
                details.get(&id).map(String::as_str),
            );
        }
        self.task_announcements.retain_issue_details(details);
    }

    /// Keep the top-bar Tasks button in step with the domain. It is shown
    /// only where tasks can be used and counts attention by the shared rule.
    pub(crate) fn sync_task_badge(&self) {
        self.refresh_workspace_task_overview();
        let visible = !self.safe_mode
            && (self.config.borrow().agent_tasks_enabled || !self.task_manager.tasks().is_empty());
        let counts = self.task_manager.attention_counts();
        let state = (visible, counts.total(), counts.summary());
        if self.task_badge.borrow().as_ref() == Some(&state) {
            return;
        }
        *self.task_badge.borrow_mut() = Some(state.clone());
        let (visible, count, summary) = state;
        self.top_bar.emit(crate::top_bar::TopBarMsg::TaskAttention {
            visible,
            count,
            summary,
        });
    }

    pub(crate) fn toggle_tasks_panel(&mut self, sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("Agent tasks are unavailable in safe mode.");
            return;
        }
        if !self.config.borrow().agent_tasks_enabled && self.task_manager.tasks().is_empty() {
            self.show_toast(
                "Agent tasks are opt-in: set agent_tasks_enabled = true in the config file first",
            );
            return;
        }
        let visible = self.workspace_stack.visible_child_name().as_deref() != Some("tasks");
        self.tasks_panel_visible.set(visible);
        self.sync_side_panel();
        if visible {
            self.workspace_stack.set_visible_child_name("tasks");
            // Opening the panel (for example from the attention badge) shows
            // the ordinary views, never a hidden-tasks list left from before.
            self.tasks_panel.emit(TasksPanelMsg::LeaveHidden);
            self.sync_tasks_panel();
        }
        self.ensure_agent_tasks_timer(sender);
    }

    /// Keep one self-re-arming poll timer alive while task machinery has
    /// anything to do. The tick drains only already-buffered state.
    pub(crate) fn ensure_agent_tasks_timer(&self, sender: &ComponentSender<AppModel>) {
        if self.agent_tasks_timer_armed.replace(true) {
            return;
        }
        let sender = sender.clone();
        glib::timeout_add_local_once(TASKS_POLL_FAST, move || {
            sender.input(AppMsg::AgentTasksTick);
        });
    }

    /// One nonblocking drain of every task-facing worker: native runtime
    /// events, pending worktree creation, and the diff worker. Rearms the
    /// timer at the cadence current activity justifies.
    pub(crate) fn agent_tasks_tick(&mut self, sender: &ComponentSender<AppModel>) {
        self.announce_task_transitions();
        self.agent_tasks_timer_armed.set(false);
        let mut keep_fast = false;

        // Poll already-owned workers even after opt-out. The policy below
        // cancels pending provider preparation before it can launch.
        {
            let policy = self.task_prompt_policy();
            let selected = self
                .tasks_panel_visible
                .get()
                .then_some(self.selected_task)
                .flatten();
            self.agent_runtime.select_hosted_view(selected);
            let report = self.agent_runtime.poll(&mut self.task_manager, policy);
            for outcome in self.agent_runtime.take_prompt_outcomes() {
                if let Some(request) = self.hosted_followups.remove(&outcome.receipt) {
                    self.tasks_panel
                        .emit(TasksPanelMsg::FollowUpResolved(request, outcome.result));
                }
            }
            for (task_id, terminal) in self.agent_runtime.take_owned_terminal_launches() {
                if self.selected_task == Some(task_id) && self.tasks_panel_visible.get() {
                    self.open_hosted_validation_terminal(task_id, terminal, sender);
                }
            }
            for (task_id, session) in self.agent_runtime.take_terminal_handoffs() {
                if self.selected_task == Some(task_id) && self.tasks_panel_visible.get() {
                    self.open_hosted_native_terminal(session, sender);
                }
            }
            self.task_announcements.applied_turns(&report.applied_turns);
            // Report issues are consumed once, even when preparation failure
            // leaves the task status unchanged. Merge with terminal transitions
            // before delivery so one event has only one semantic speech owner.
            self.announce_task_transitions_with_report(&report.issues, &report.completions);
            keep_fast |= report.made_progress() || self.agent_runtime.needs_fast_poll();

            if let Some(pending) = &self.pending_task_creation {
                match pending.receiver.try_recv() {
                    Ok(Ok(prepared)) => {
                        self.pending_task_creation = None;
                        if self.config.borrow().agent_tasks_enabled {
                            self.register_prepared_task(prepared, sender);
                        } else {
                            if let Some(request) = prepared.project_request {
                                request.finish(Err("Agent tasks were disabled while preparing the worktree. Your draft is preserved.".into()));
                            }
                            self.show_toast("Task creation finished after tasks were disabled; its worktree was preserved without starting an agent");
                        }
                    }
                    Ok(Err(error)) => {
                        self.pending_task_creation = None;
                        self.show_toast(format!("Could not create task worktree: {error}"));
                    }
                    Err(TryRecvError::Empty) => {
                        keep_fast = true;
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.pending_task_creation = None;
                        self.show_toast("Task worktree worker stopped without a result");
                    }
                }
            }

            if self.agent_diff.poll() {
                keep_fast = true;
            }
            if self.agent_diff.state().loading {
                keep_fast = true;
            }

            if self.tasks_panel_visible.get() {
                self.sync_tasks_panel();
            }
        }

        self.announce_task_transitions();
        self.sync_task_badge();
        let has_tasks = self.task_manager.tasks().iter().next().is_some();
        if keep_fast
            || has_tasks
            || self.tasks_panel_visible.get()
            || self.agent_runtime.has_any_activity()
            || self.agent_runtime.needs_host_poll()
        {
            self.ensure_agent_tasks_timer_with(
                if keep_fast {
                    TASKS_POLL_FAST
                } else {
                    TASKS_POLL_SLOW
                },
                sender,
            );
        }
    }

    fn ensure_agent_tasks_timer_with(
        &self,
        interval: Duration,
        sender: &ComponentSender<AppModel>,
    ) {
        // The tick always re-arms through the guard flag so a burst of
        // messages cannot stack multiple timers.
        self.agent_tasks_timer_armed.set(true);
        let sender = sender.clone();
        glib::timeout_add_local_once(interval, move || {
            sender.input(AppMsg::AgentTasksTick);
        });
    }

    fn task_prompt_policy(&self) -> NativePromptPolicy {
        let config = self.config.borrow();
        let mut policy = agent_task_ui::prompt_policy(&config);
        // Preparation compares this policy before consuming any ready result.
        policy.share_command_context &= config.agent_tasks_enabled;
        policy
    }

    fn existing_task_terminal(
        &self,
        binding: Option<&str>,
        role: TaskTerminalRole,
    ) -> Option<(usize, usize)> {
        agent_task_ui::resolve_task_terminal(
            binding,
            self.tabs.iter().enumerate().flat_map(|(ti, tab)| {
                tab.panes
                    .iter()
                    .enumerate()
                    .filter(move |(_, pane)| pane.task_role == Some(role))
                    .filter_map(move |(pi, pane)| {
                        pane.task_session_id
                            .clone()
                            .map(|identity| (identity, (ti, pi)))
                    })
            }),
        )
    }

    fn focus_task_terminal(
        &mut self,
        task_id: TaskId,
        role: TaskTerminalRole,
        sender: &ComponentSender<AppModel>,
    ) {
        if !agent_task_ui::task_focus_allowed(
            self.tasks_panel_visible.get(),
            self.selected_task,
            task_id,
        ) {
            return;
        }
        if role == TaskTerminalRole::Validation {
            if let Some(terminal) = self
                .agent_runtime
                .hosted_validation_terminal(task_id)
                .cloned()
            {
                let binding = self
                    .task_manager
                    .get(task_id)
                    .and_then(|task| task.validation.terminal_session_id.as_deref());
                if self.existing_task_terminal(binding, role).is_none() {
                    self.open_hosted_validation_terminal(task_id, terminal, sender);
                    return;
                }
            }
        }
        let binding = self
            .task_manager
            .get(task_id)
            .and_then(|task| agent_task_ui::task_terminal_binding(task, role))
            .map(str::to_owned);
        let Some((ti, pi)) = self.existing_task_terminal(binding.as_deref(), role) else {
            self.show_toast("Task terminal is no longer available");
            return;
        };
        if self.tabs[ti].zoom.is_some() && self.tabs[ti].active_pane != pi {
            self.toggle_pane_zoom_for(ti);
            if self.tabs[ti].zoom.is_some() {
                self.show_toast("Task terminal cannot be shown while its split is changing");
                return;
            }
        }
        crate::enter_ownership::claim_held();
        let tab_id = self.tabs[ti].id;
        self.select_tab(tab_id, sender);
        // Reuse ordinary pane navigation so focus, headers and ownership agree.
        let delta = pi as i32 - self.tabs[ti].active_pane as i32;
        if delta != 0 {
            self.cycle_pane_focus(delta);
        }
    }

    /// Stage a panel action against the live domain. Every branch resolves
    /// the task again; a stale panel snapshot can never retarget an action.
    pub(crate) fn execute_task_panel_action(
        &mut self,
        action: TaskPanelAction,
        sender: &ComponentSender<AppModel>,
    ) {
        if !task_action_allowed(self.config.borrow().agent_tasks_enabled, &action) {
            if let TaskPanelAction::FollowUp(request) = &action {
                self.tasks_panel.emit(TasksPanelMsg::FollowUpResolved(
                    request.clone(),
                    Err("Agent tasks are disabled. Your follow-up draft is preserved.".into()),
                ));
            }
            if let TaskPanelAction::CreateProject(request) = &action {
                request.finish(Err("Agent tasks are disabled in the configuration".into()));
            }
            self.show_toast("Agent tasks are disabled in the configuration");
            return;
        }
        self.announce_task_transitions();
        let mutation = match &action {
            TaskPanelAction::Complete(id) => Some((
                *id,
                jterm_core::agent_workspace::NativeTaskMutation::Complete,
            )),
            TaskPanelAction::AcceptReviewed(id) => Some((
                *id,
                jterm_core::agent_workspace::NativeTaskMutation::AcceptReviewed,
            )),
            TaskPanelAction::Archive(id) => Some((
                *id,
                jterm_core::agent_workspace::NativeTaskMutation::Archive,
            )),
            TaskPanelAction::Restore(id) => Some((
                *id,
                jterm_core::agent_workspace::NativeTaskMutation::Restore,
            )),
            _ => None,
        };
        if let Some((task_id, mutation)) = mutation {
            if self.agent_runtime.conversation_id(task_id).is_some() {
                if let Err(error) = self.agent_runtime.mutate_hosted_task(task_id, mutation) {
                    self.show_toast(error.to_string());
                }
                // The host's accepted snapshot supplies the transition. Queueing
                // a request cannot make this local replica complete or archive it.
                self.ensure_agent_tasks_timer(sender);
                return;
            }
        }
        match action {
            TaskPanelAction::CreateFromBlock => self.create_task_from_block(sender),
            TaskPanelAction::ComposeProject => self.compose_project_task(sender),
            TaskPanelAction::CreateProject(draft) => self.create_project_task(draft, sender),
            TaskPanelAction::Select(task_id) => {
                self.agent_runtime.select_hosted_view(Some(task_id));
                self.selected_task = Some(task_id);
                self.sync_tasks_panel();
            }
            TaskPanelAction::Close => {
                self.agent_runtime.select_hosted_view(None);
                self.tasks_panel_visible.set(false);
                self.sync_side_panel();
            }
            TaskPanelAction::LoadNativeHistory(task_id, reset) => {
                let result = self.agent_runtime.load_native_history(task_id, reset);
                if let Err(error) = result {
                    self.show_toast(format!("Could not load provider history: {error}"));
                }
            }
            TaskPanelAction::StartNative(task_id) => {
                let Some(provider) = self.task_manager.get(task_id).map(|task| task.provider)
                else {
                    return;
                };
                let policy = self.task_prompt_policy();
                let result = match provider {
                    AgentProvider::Codex => {
                        self.agent_runtime
                            .start_codex(&mut self.task_manager, task_id, policy)
                    }
                    AgentProvider::Claude => {
                        self.agent_runtime
                            .start_claude(&mut self.task_manager, task_id, policy)
                    }
                    AgentProvider::Kimi => {
                        self.agent_runtime
                            .start_kimi(&mut self.task_manager, task_id, policy)
                    }
                    AgentProvider::OpenCode => {
                        self.show_toast("Use Start terminal for OpenCode");
                        return;
                    }
                };
                match result {
                    Ok(()) => self.show_toast(format!(
                        "Preparing {} in the background…",
                        provider.display_name()
                    )),
                    Err(error) => self.show_toast(format!(
                        "Could not start {}: {error}",
                        provider.display_name()
                    )),
                }
            }
            TaskPanelAction::StartTerminal(task_id) => {
                self.start_task_agent_terminal(task_id, sender)
            }
            TaskPanelAction::FocusTerminal(task_id) => {
                self.focus_task_terminal(task_id, TaskTerminalRole::Agent, sender)
            }
            TaskPanelAction::FocusValidation(task_id) => {
                self.focus_task_terminal(task_id, TaskTerminalRole::Validation, sender)
            }
            TaskPanelAction::StopCodex(task_id) => match self.agent_runtime.cancel(task_id) {
                Ok(()) => {
                    let provider = self.task_provider_name(task_id);
                    if self.agent_runtime.has_running(task_id) {
                        self.show_toast(format!(
                            "Stopping {provider} and waiting for process cleanup…"
                        ));
                    } else {
                        self.show_toast(format!(
                            "Native {provider} preparation cancelled; finishing background cleanup…"
                        ));
                    }
                }
                Err(error) => self.show_toast(error.to_string()),
            },
            TaskPanelAction::FollowUp(request) => {
                let policy = self.task_prompt_policy();
                let result = self
                    .agent_runtime
                    .prompt_native_tracked(
                        &self.task_manager,
                        request.task_id,
                        &request.text,
                        policy,
                        request.request_id.to_string(),
                    )
                    .map_err(|error| error.to_string());
                match &result {
                    Ok(()) => self.show_toast(format!(
                        "Follow-up queued on the existing {} session…",
                        self.task_provider_name(request.task_id)
                    )),
                    Err(error) => self.show_toast(error.clone()),
                }
                if result.is_ok() && self.agent_runtime.needs_host_poll() {
                    self.hosted_followups
                        .insert(request.request_id.to_string(), request);
                } else {
                    self.tasks_panel
                        .emit(TasksPanelMsg::FollowUpResolved(request, result));
                }
            }
            TaskPanelAction::FinishCodex(task_id) => {
                match self.agent_runtime.finish_codex(&self.task_manager, task_id) {
                    Ok(()) => self.show_toast(format!(
                        "Finishing {} and waiting for containment cleanup before validation…",
                        self.task_provider_name(task_id)
                    )),
                    Err(error) => self.show_toast(error.to_string()),
                }
            }
            TaskPanelAction::Approve(task_id, approval_id) => {
                self.decide_native_approval(task_id, approval_id, ApprovalDecision::Approve)
            }
            TaskPanelAction::Deny(task_id, approval_id) => self.decide_native_approval(
                task_id,
                approval_id,
                ApprovalDecision::Deny { reason: None },
            ),
            TaskPanelAction::RunValidation(task_id) => self.start_task_validation(task_id, sender),
            TaskPanelAction::Complete(task_id) => {
                match self.task_manager.complete_after_validation(task_id) {
                    // The transition observer supplies both visible feedback and
                    // the sole polite announcement for successful completion.
                    Ok(()) => {}
                    Err(error) => self.show_toast(error.to_string()),
                }
            }
            TaskPanelAction::AcceptReviewed(task_id) => {
                if let Err(error) = self.task_manager.complete_after_manual_review(task_id) {
                    self.show_toast(error.to_string());
                }
            }
            TaskPanelAction::ReviewDiff(task_id) => {
                let task_review = self
                    .task_manager
                    .get(task_id)
                    .map(|task| (task.worktree_path.clone(), task.base_commit.clone()));
                let Some((worktree, base_commit)) = task_review else {
                    self.show_toast("Task is no longer available");
                    return;
                };
                self.tasks_panel.emit(TasksPanelMsg::ShowPage("diff"));
                self.agent_diff.is_open = true;
                if let Err(error) = self.agent_diff.request_from(worktree, base_commit) {
                    self.show_toast(format!("Could not open task diff: {error}"));
                }
            }
            TaskPanelAction::Archive(task_id) => match self.task_manager.archive(task_id) {
                Ok(()) => {
                    // Keep any retained native view: Restore brings the task
                    // back with its transcript and recovery evidence.
                    if self.selected_task == Some(task_id) {
                        self.selected_task = None;
                    }
                    self.show_toast(
                        "Task hidden; its worktree is unchanged. Restore it from Hidden tasks.",
                    );
                }
                Err(_) => self.show_toast(ARCHIVE_BLOCKED),
            },
            TaskPanelAction::Restore(task_id) => match self.task_manager.restore(task_id) {
                Ok(()) => {
                    // Keep it selected so the restored task is easy to find.
                    self.selected_task = Some(task_id);
                    let state = self
                        .task_manager
                        .get(task_id)
                        .map_or("restored", |task| task.status.label());
                    self.show_toast(format!("Task restored · {state}"));
                }
                Err(error) => self.show_toast(error.to_string()),
            },
        }
        self.announce_task_transitions();
        self.sync_task_badge();
        self.ensure_agent_tasks_timer(sender);
        if self.tasks_panel_visible.get() {
            self.sync_tasks_panel();
        }
    }

    fn decide_native_approval(
        &mut self,
        task_id: TaskId,
        approval_id: crate::agent_task::ApprovalId,
        decision: ApprovalDecision,
    ) {
        let label = if matches!(&decision, ApprovalDecision::Approve) {
            "Approval sent to Codex"
        } else {
            "Denial sent to Codex"
        };
        match self
            .agent_runtime
            .decide_approval(task_id, approval_id, decision)
        {
            Ok(()) => self.show_toast(label),
            Err(error) => self.show_toast(error.to_string()),
        }
    }

    fn compose_project_task(&self, sender: &ComponentSender<AppModel>) {
        if self.pending_task_creation.is_some() {
            self.show_toast("Another task worktree is still being created");
            return;
        }
        let directory = self
            .active_pane()
            .and_then(|pane| pane.local_cwd())
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        let sender = sender.clone();
        agent_task_ui::task_composer::show(
            &self.window,
            directory,
            self.shell_argv.first().cloned(),
            move |draft| {
                sender.input(AppMsg::TaskPanelAction(TaskPanelAction::CreateProject(
                    draft,
                )));
            },
        );
    }

    fn create_project_task(
        &mut self,
        request: agent_task_ui::TaskProjectRequest,
        sender: &ComponentSender<AppModel>,
    ) {
        if self.pending_task_creation.is_some() {
            self.show_toast("Another task worktree is still being created");
            request.finish(Err("Another task worktree is still being created".into()));
            return;
        }
        match agent_task_ui::begin_project_worktree_creation(request.clone()) {
            Ok(pending) => {
                self.pending_task_creation = Some(pending);
                self.show_toast("Creating the isolated project task…");
                self.ensure_agent_tasks_timer(sender);
                self.sync_tasks_panel();
            }
            Err(error) => {
                request.finish(Err(error.clone()));
                self.show_toast(format!("Could not create task: {error}"));
            }
        }
    }

    /// Begin task creation from the active pane's selected block. The block
    /// preflight is the exact one ember shares between its block menu and
    /// panel: exact shell-reported command and cwd are mandatory, and the
    /// sharing consent gates nothing here because no provider is contacted
    /// until Start Codex.
    fn create_task_from_block(&mut self, sender: &ComponentSender<AppModel>) {
        let Some(pane_id) = self.active_pane().map(|pane| pane.id) else {
            self.show_toast("No active terminal pane");
            return;
        };
        self.create_task_from_block_in_pane(pane_id, false, sender);
    }

    /// The failed-block menu's Fix action (ember's `FixWithAgent`): create the
    /// worktree task for the failed block right-clicked in `pane_id`, rather
    /// than whatever pane happens to be active. The Tasks panel's own gates
    /// apply unchanged — safe mode and the `agent_tasks_enabled` opt-in —
    /// plus a failed-outcome recheck at dispatch. No provider is contacted
    /// until the user starts Codex, so the `ai_share_command_context` consent
    /// posture stays exactly where the native prompt gate already put it.
    pub(crate) fn fix_block_with_agent(
        &mut self,
        pane_id: u64,
        sender: &ComponentSender<AppModel>,
    ) {
        if self.safe_mode {
            self.show_toast("Agent tasks are unavailable in safe mode.");
            return;
        }
        if !self.config.borrow().agent_tasks_enabled {
            self.show_toast(
                "Agent tasks are opt-in: set agent_tasks_enabled = true in the config file first",
            );
            return;
        }
        self.create_task_from_block_in_pane(pane_id, true, sender);
    }

    /// Shared task creation from one pane's selected block. `require_failed`
    /// is the block menu's Fix gate: only a genuinely failed command block
    /// may anchor a fix task, re-checked here because the menu's visibility
    /// gate is renderer-side and could be stale by dispatch time.
    fn create_task_from_block_in_pane(
        &mut self,
        pane_id: u64,
        require_failed: bool,
        sender: &ComponentSender<AppModel>,
    ) {
        if self.pending_task_creation.is_some() {
            self.show_toast("Another task worktree is still being created");
            return;
        }
        let Some(pane) = self.pane(pane_id) else {
            self.show_toast("The block's terminal pane is no longer available");
            return;
        };
        let Some(evidence) = pane.terminal.selected_block_agent_evidence(80) else {
            self.show_toast("Select a finished block in a Block-mode pane to create an agent task");
            return;
        };
        // The card's own rule: an interrupt or a stop is not a failure.
        if require_failed
            && !evidence.command.as_deref().is_some_and(|command| {
                crate::block_view::block_is_failure(command, evidence.exit_code)
            })
        {
            self.show_toast("Fix tasks are available for failed command blocks");
            return;
        }
        if let Some(reason) = block_agent_task_anchor_disabled_reason(&evidence) {
            self.show_toast(format!("Cannot create an agent task: {reason}"));
            return;
        }
        let source_shell = self.shell_argv.first().cloned();
        let Some(context) = semantic_context_from_evidence(pane, evidence, source_shell) else {
            self.show_toast("Cannot create an agent task: the pane has no verified shell session");
            return;
        };
        match agent_task_ui::begin_worktree_creation(context, AgentProvider::Codex) {
            Ok(pending) => {
                self.pending_task_creation = Some(pending);
                self.show_toast("Creating the isolated task worktree in the background…");
                self.ensure_agent_tasks_timer(sender);
                if self.tasks_panel_visible.get() {
                    self.sync_tasks_panel();
                }
            }
            Err(error) => self.show_toast(format!("Could not create task: {error}")),
        }
    }

    fn register_prepared_task(
        &mut self,
        prepared: agent_task_ui::PreparedTask,
        sender: &ComponentSender<AppModel>,
    ) {
        if prepared
            .project_request
            .as_ref()
            .is_some_and(|request| request.cancelled())
        {
            return;
        }
        let project_request = prepared.project_request;
        let new_task = NewTask {
            title: prepared.title,
            provider: prepared.provider,
            repo_root: prepared.worktree.repository.clone(),
            worktree_path: prepared.worktree.path.clone(),
            branch: prepared.worktree.branch.clone(),
            base_commit: prepared.worktree.head.clone(),
            source_context: prepared.context,
        };
        let result = match prepared.brief {
            Some(brief) => self.task_manager.create_with_brief(new_task, brief),
            None => self.task_manager.create(new_task),
        };
        if let Some(request) = &project_request {
            request.finish(result.as_ref().map(|_| ()).map_err(ToString::to_string));
        }
        match result {
            Ok(task_id) => {
                self.selected_task = Some(task_id);
                self.show_toast("Task created in an isolated worktree; choose Start when ready");
            }
            Err(error) => {
                self.show_toast(format!("Could not register task: {error}"));
            }
        }
        self.ensure_agent_tasks_timer(sender);
        if self.tasks_panel_visible.get() {
            self.sync_tasks_panel();
        }
    }

    /// Open the provider CLI in an ordinary PTY inside the task worktree.
    /// This is the compatibility path: no native events, no approval cards;
    /// containment is the worktree plus the exact audited launcher argv.
    fn start_task_agent_terminal(&mut self, task_id: TaskId, sender: &ComponentSender<AppModel>) {
        if self.agent_runtime.conversation_id(task_id).is_some()
            && self.agent_runtime.can_continue_in_terminal(task_id)
        {
            if let Err(error) = self.agent_runtime.handoff_to_terminal(task_id) {
                self.show_toast(error.to_string());
            }
            self.ensure_agent_tasks_timer(sender);
            return;
        }
        if self.agent_runtime.has_preparing(task_id) {
            self.show_toast(format!(
                "Cancel native {} preparation before starting a terminal",
                self.task_provider_name(task_id)
            ));
            return;
        }
        let failed_terminal_retry = self
            .task_manager
            .terminal_retry_session_id(task_id)
            .ok()
            .map(str::to_owned);
        let native_recovery = failed_terminal_retry.is_none()
            && self.agent_runtime.can_continue_in_terminal(task_id)
            && self
                .task_manager
                .native_terminal_fallback_eligible(task_id)
                .is_ok();
        let launch = self.task_manager.get(task_id).and_then(|task| {
            ((task.status == TaskStatus::Created && task.terminal_session_id.is_none())
                || (native_recovery && task.terminal_session_id.is_none())
                || failed_terminal_retry
                    .as_deref()
                    .is_some_and(|old| task.terminal_session_id.as_deref() == Some(old)))
            .then(|| {
                (
                    task.provider,
                    task.title.clone(),
                    task.repo_root.clone(),
                    task.worktree_path.clone(),
                )
            })
        });
        let Some((provider, title, repository, worktree)) = launch else {
            self.show_toast("Task is no longer waiting for an Agent terminal");
            return;
        };
        let launch =
            match crate::agent_task::AgentLaunchSpec::resolve(provider, &repository, &worktree) {
                Ok(launch) => launch,
                Err(error) => {
                    if failed_terminal_retry.is_none() && !native_recovery {
                        // update_status preserves TerminalFallback provenance, so
                        // a failed compatibility launch remains terminal-only.
                        let _ = self.task_manager.update_status(
                            task_id,
                            TaskStatus::Created,
                            Some(error.to_string()),
                        );
                    }
                    self.show_toast(error.to_string());
                    return;
                }
            };
        let Some(task) = self.task_manager.get(task_id) else {
            return;
        };
        let authored = task.brief.is_some();
        let launch =
            match agent_task_ui::prepare_authored_terminal(task, self.task_prompt_policy(), launch)
            {
                Ok(launch) => launch,
                Err(error) => {
                    self.show_toast(error);
                    return;
                }
            };
        if failed_terminal_retry.is_none() && !native_recovery {
            let _ = self
                .task_manager
                .update_status(task_id, TaskStatus::Starting, None);
        }

        let session_name = format!(
            "{} · {}",
            provider.display_name(),
            crate::review_text::visible_bounded(&title, 96)
        );
        let Some((_, pane_id)) = self.add_task_terminal_tab(
            &session_name,
            launch.argv,
            Some(worktree.to_string_lossy().into_owned()),
            Vec::new(),
            // The real session id keys on the pane that does not exist yet;
            // it is stamped on immediately below.
            crate::workspace_ops::TaskTerminalIdentity {
                role: TaskTerminalRole::Agent,
                session_id: String::new(),
            },
            sender,
        ) else {
            if failed_terminal_retry.is_none() && !native_recovery {
                let _ = self.task_manager.update_status(
                    task_id,
                    TaskStatus::Created,
                    Some("tab budget refused the task terminal".to_string()),
                );
            }
            return;
        };
        // The synthetic identity keys on the pane that now exists; the
        // placeholder above only reserved the tab.
        let session_id = agent_task_ui::terminal_session_id(pane_id);
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.task_session_id = Some(session_id.clone());
        }

        let binding = if let Some(old_session) = failed_terminal_retry.as_deref() {
            self.task_manager
                .bind_terminal_retry_session(task_id, old_session, session_id.clone())
        } else if native_recovery {
            self.task_manager
                .bind_native_terminal_fallback_session(task_id, session_id.clone())
        } else {
            self.task_manager
                .bind_terminal_session(task_id, session_id.clone())
        };
        if let Err(error) = binding {
            // The pane never gained task authority; close it so a stray shell
            // cannot linger inside the task worktree.
            self.close_pane(pane_id, sender);
            if failed_terminal_retry.is_none() && !native_recovery {
                let _ = self.task_manager.update_status(
                    task_id,
                    TaskStatus::Created,
                    Some(error.to_string()),
                );
            }
            self.show_toast(error.to_string());
            return;
        }

        if native_recovery {
            self.agent_runtime.clear_retained(task_id);
        }
        self.persist_session();
        self.show_toast(if authored {
            format!(
                "Opened {} · Task brief: {}. Ask the Agent to read this file before working.",
                provider.display_name(),
                crate::agent_task::pty_task_brief_relative()
            )
        } else {
            format!(
                "Opened {} in an isolated task terminal; task context remains in Anvil",
                provider.display_name()
            )
        });
    }

    /// Open only the already admitted host PTY. Closing or exiting this
    /// helper is a view event; the host alone reduces validation outcomes.
    fn open_hosted_validation_terminal(
        &mut self,
        task_id: TaskId,
        terminal: jterm_core::agent_workspace::OwnedTerminalDescriptor,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(session_id) = self
            .task_manager
            .get(task_id)
            .and_then(|task| task.validation.terminal_session_id.clone())
        else {
            self.show_toast("Validation is no longer bound to this task");
            return;
        };
        if terminal.id.0 != format!("jterm:validation:{task_id}:{session_id}") {
            self.show_toast("A newer validation replaced this transcript; open its current output");
            return;
        }
        if self
            .existing_task_terminal(Some(&session_id), TaskTerminalRole::Validation)
            .is_some()
        {
            self.focus_task_terminal(task_id, TaskTerminalRole::Validation, sender);
            return;
        }
        let launch = match jterm_core::agent_workspace::owned_terminal_launch(&terminal) {
            Ok(launch) => launch,
            Err(error) => {
                self.show_toast(error);
                return;
            }
        };
        let argv = std::iter::once(launch.program).chain(launch.args).collect();
        let Some((_, pane_id)) = self.add_task_terminal_tab(
            &terminal.label,
            argv,
            Some(launch.cwd.to_string_lossy().into_owned()),
            Vec::new(),
            crate::workspace_ops::TaskTerminalIdentity {
                role: TaskTerminalRole::Validation,
                session_id,
            },
            sender,
        ) else {
            return;
        };
        if let Some(pane) = self.pane(pane_id) {
            use relm4::gtk::prelude::*;
            unsafe {
                pane.terminal
                    .widget()
                    .set_data(HOSTED_VALIDATION_VIEW, true);
            }
        }
    }

    /// Request a host-owned attempt, or prepare a local task's exact command
    /// in a fresh PTY with its validated cwd descriptor retained until spawn.
    fn start_task_validation(&mut self, task_id: TaskId, sender: &ComponentSender<AppModel>) {
        if self.agent_runtime.conversation_id(task_id).is_some() {
            if let Err(error) = self.agent_runtime.start_hosted_validation(task_id) {
                self.show_toast(error.to_string());
            }
            self.ensure_agent_tasks_timer(sender);
            return;
        }

        let next_attempt = match self.task_manager.next_validation_attempt(task_id) {
            Ok(attempt) => attempt,
            Err(error) => {
                self.show_toast(error.to_string());
                return;
            }
        };
        let prepared = {
            let Some(task) = self.task_manager.get(task_id) else {
                self.show_toast("Task is no longer available");
                return;
            };
            match crate::agent_task::prepare_task_validation(task) {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.show_toast(format!("Could not prepare validation: {error}"));
                    return;
                }
            }
        };
        let argv = match agent_task_ui::validation_command_argv(
            Some(prepared.source_shell.as_str()),
            &prepared.command,
        ) {
            Ok(argv) => argv,
            Err(error) => {
                self.show_toast(format!("Could not resolve validation shell: {error}"));
                return;
            }
        };
        let task_title = match self.task_manager.get(task_id) {
            Some(task) => task.title.clone(),
            None => {
                self.show_toast("Task is no longer available");
                return;
            }
        };
        let session_name = format!(
            "Validate #{} · {}",
            next_attempt,
            crate::review_text::visible_bounded(&task_title, 88)
        );
        let pinned_path = prepared.pinned_cwd.proc_path();
        let real_cwd = prepared.cwd.clone();
        let env_extra: Vec<(String, String)> = agent_task_ui::VALIDATION_ENV_OVERRIDES
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let Some((_, pane_id)) = self.add_task_terminal_tab(
            &session_name,
            argv,
            Some(pinned_path.to_string_lossy().into_owned()),
            env_extra,
            crate::workspace_ops::TaskTerminalIdentity {
                role: TaskTerminalRole::Validation,
                session_id: String::new(),
            },
            sender,
        ) else {
            return;
        };
        let session_id = agent_task_ui::terminal_session_id(pane_id);
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.task_session_id = Some(session_id.clone());
            // Display metadata shows the real path; the spawned child already
            // entered it through the pinned descriptor.
            pane.cwd = Some(real_cwd.to_string_lossy().into_owned());
        }

        if let Err(error) = self
            .task_manager
            .bind_validation_session(task_id, session_id.clone())
        {
            self.close_pane(pane_id, sender);
            self.show_toast(error.to_string());
            return;
        }
        // Hold the validated descriptor open until the spawn completes (or
        // the pane dies trying); see PaneLaunched / task terminal exit.
        self.pending_validation_pins.insert(pane_id, prepared);
        self.persist_session();
        self.show_toast(format!(
            "Validation #{next_attempt} is running in the isolated task worktree"
        ));
    }

    /// A pane process exited. When it was a task terminal, apply the
    /// authoritative exit to the task model before the pane closes; returns
    /// true when the pane was task-owned.
    pub(crate) fn note_task_terminal_exited(&mut self, pane_id: u64, exit_code: i32) -> bool {
        if self.pane(pane_id).is_some_and(is_hosted_validation_view) {
            return true;
        }
        self.announce_task_transitions();
        self.pending_validation_pins.remove(&pane_id);
        let Some((session_id, _role)) = self
            .pane(pane_id)
            .and_then(|pane| pane.task_session_id.clone().zip(pane.task_role))
        else {
            return false;
        };
        self.task_manager
            .handle_terminal_session_exit(&session_id, Some(exit_code));
        self.task_manager
            .handle_terminal_session_closed(&session_id);
        if self.tasks_panel_visible.get() {
            self.sync_tasks_panel();
        }
        self.announce_task_transitions();
        true
    }

    /// A rejected spawn has no wait status or later PaneExited message.
    /// Retire its task session now, so validation can be retried and its cwd
    /// descriptor is not held until the user closes the diagnostic pane.
    pub(crate) fn note_task_terminal_launch_failed(&mut self, pane_id: u64) {
        if self.pane(pane_id).is_some_and(is_hosted_validation_view) {
            return;
        }
        self.announce_task_transitions();
        self.pending_validation_pins.remove(&pane_id);
        let Some((session_id, role)) = self
            .pane(pane_id)
            .and_then(|pane| pane.task_session_id.clone().zip(pane.task_role))
        else {
            return;
        };
        record_task_terminal_launch_failure(&mut self.task_manager, &session_id, role);
        if self.tasks_panel_visible.get() {
            self.sync_tasks_panel();
        }
        self.announce_task_transitions();
    }

    /// PaneLaunched releases the validation cwd pin retained for that pane.
    pub(crate) fn note_pane_launched_task_pin(&mut self, pane_id: u64) {
        self.pending_validation_pins.remove(&pane_id);
    }

    /// A task-terminal pane is being removed without a process-exit message
    /// (the user closed its tab). This is the close half of ember's
    /// exit/closed split: a validation still marked Running becomes Cancelled
    /// rather than silently retaining a stale Running state, and any retained
    /// cwd pin is released. Safe to call after `note_task_terminal_exited`:
    /// the reducer ignores closes for terminals already in a terminal state.
    pub(crate) fn note_task_terminal_closed(&mut self, pane_id: u64) {
        if self.pane(pane_id).is_some_and(is_hosted_validation_view) {
            return;
        }
        self.announce_task_transitions();
        self.pending_validation_pins.remove(&pane_id);
        let Some(session_id) = self
            .pane(pane_id)
            .and_then(|pane| pane.task_session_id.clone())
        else {
            return;
        };
        if self
            .task_manager
            .handle_terminal_session_closed(&session_id)
            .is_some()
            && self.tasks_panel_visible.get()
        {
            self.sync_tasks_panel();
        }
        self.announce_task_transitions();
    }

    /// Push the composed panel state. Rows are rebuilt from the domain every
    /// time; the panel never holds its own task copies.
    pub(crate) fn sync_tasks_panel(&self) {
        {
            use relm4::gtk::prelude::*;
            crate::eva_companion::observe_task(
                self.window.upcast_ref(),
                self.task_manager
                    .tasks()
                    .iter()
                    .find(|t| Some(t.id) == self.selected_task),
            );
        }
        self.refresh_workspace_task_overview();
        let policy = self.task_prompt_policy();
        let native_ai_enabled = policy.share_command_context;
        let mut agent_terminals = HashSet::new();
        let mut validation_terminals = HashSet::new();
        for pane in self.tabs.iter().flat_map(|tab| tab.panes.iter()) {
            if let Some(identity) = &pane.task_session_id {
                match pane.task_role {
                    Some(TaskTerminalRole::Agent) => {
                        agent_terminals.insert(identity.clone());
                    }
                    Some(TaskTerminalRole::Validation) => {
                        validation_terminals.insert(identity.clone());
                    }
                    None => {}
                }
            }
        }

        let rows: Vec<TaskRowSnapshot> = self
            .task_manager
            .tasks()
            .iter()
            .map(|task| TaskRowSnapshot {
                id: task.id,
                title: task.title.clone(),
                provider: task.provider,
                status: task.status,
                runtime_kind: task.runtime_kind,
                branch: task.branch.clone(),
                has_agent_terminal: task
                    .terminal_session_id
                    .as_ref()
                    .is_some_and(|id| agent_terminals.contains(id)),
                has_validation_terminal: self
                    .agent_runtime
                    .hosted_validation_terminal(task.id)
                    .is_some()
                    || task
                        .validation
                        .terminal_session_id
                        .as_ref()
                        .is_some_and(|id| validation_terminals.contains(id)),
                has_active_agent_stream: self.task_manager.has_active_agent_event_stream(task.id),
                native_preparing: self.agent_runtime.has_preparing(task.id),
                validation_status: task.validation.status,
                validation_attempt: task.validation.attempt,
                needs_attention: task.needs_attention(),
                status_detail: task.status_detail.clone(),
                validation_detail: validation_history_preview(
                    task.validation.status_detail.as_deref(),
                    self.agent_runtime.hosted_validation_snapshot(task.id),
                    1024,
                    false,
                ),
            })
            .collect();

        let selected = self
            .selected_task
            .filter(|id| self.task_manager.get(*id).is_some());
        let detail = selected.and_then(|id| {
            let task = self.task_manager.get(id)?;
            let agent = task
                .terminal_session_id
                .as_ref()
                .is_some_and(|id| agent_terminals.contains(id));
            let validation = self.agent_runtime.hosted_validation_terminal(id).is_some()
                || task
                    .validation
                    .terminal_session_id
                    .as_ref()
                    .is_some_and(|id| validation_terminals.contains(id));
            self.task_detail_sync(id, native_ai_enabled, agent, validation)
        });

        let create_hint = if self.pending_task_creation.is_some() {
            "Creating the isolated task worktree…".to_string()
        } else if !native_ai_enabled {
            "Enable AI and command-context sharing in Settings to start a native Agent. Existing terminal sessions remain available."
                .to_string()
        } else {
            String::new()
        };
        // Hidden tasks stay in the projection so the panel can offer them for
        // restore; its scope excludes them from every ordinary view. The
        // panel chooses the final order, so no intermediate sort happens here.
        let dashboard = crate::agent_task::DashboardSnapshot::project_unsorted(
            self.task_manager.tasks(),
            |id| crate::agent_task::DashboardActivity {
                has_active_agent_stream: self.task_manager.has_active_agent_event_stream(id),
                native_preparing: self.agent_runtime.has_preparing(id),
                ..Default::default()
            },
            Default::default(),
        );
        let sync = TasksPanelSync {
            workspaces: self
                .task_manager
                .tasks()
                .iter()
                .map(|task| (task.id, task.repo_root.clone()))
                .collect(),
            rows,
            dashboard,
            selected,
            detail: detail.map(Box::new),
            create_enabled: true,
            create_hint,
            pending_creation: self.pending_task_creation.is_some(),
        };
        self.tasks_panel.emit(TasksPanelMsg::Sync(Box::new(sync)));
    }

    /// The compact Agent page and full Tasks page share the same selected
    /// identity and runtime permission gates.
    pub(crate) fn refresh_workspace_task_overview(&self) {
        let tasks = self.task_manager.tasks();
        let selected =
            jterm_core::reference_conversation::observed_inspector_task(tasks, self.selected_task);
        let mut model = jterm_core::reference_conversation::observed_task_inspector(
            tasks,
            self.selected_task,
            |id| crate::agent_task::DashboardActivity {
                has_active_agent_stream: self.task_manager.has_active_agent_event_stream(id),
                native_preparing: self.agent_runtime.has_preparing(id),
                ..Default::default()
            },
        );
        let private = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.private_title);
        if private {
            model = jterm_core::reference_conversation::Inspector::empty();
            model.title = "Private session".into();
            model.subtitle =
                "Task details are hidden. Open Tasks explicitly to inspect them.".into();
            model.metadata.clear();
            model.output_title = "Task details".into();
            model.output = "Task details are hidden in a private session.".into();
        }
        let detail = selected
            .filter(|_| !private && !self.safe_mode)
            .and_then(|task| {
                let agent = task.terminal_session_id.as_deref().is_some_and(|id| {
                    self.existing_task_terminal(Some(id), TaskTerminalRole::Agent)
                        .is_some()
                });
                let validation = self
                    .agent_runtime
                    .hosted_validation_terminal(task.id)
                    .is_some()
                    || task
                        .validation
                        .terminal_session_id
                        .as_deref()
                        .is_some_and(|id| {
                            self.existing_task_terminal(Some(id), TaskTerminalRole::Validation)
                                .is_some()
                        });
                self.task_detail_sync(
                    task.id,
                    self.task_prompt_policy().share_command_context,
                    agent,
                    validation,
                )
            });
        let allow_create = !self.safe_mode && self.config.borrow().agent_tasks_enabled;
        let allow_manage =
            !self.safe_mode && (self.config.borrow().agent_tasks_enabled || !tasks.is_empty());
        self.shell
            .sync_task_overview(&model, detail.as_ref(), allow_create, allow_manage);
    }

    fn task_detail_sync(
        &self,
        task_id: TaskId,
        native_ai_enabled: bool,
        can_focus_terminal: bool,
        can_focus_validation: bool,
    ) -> Option<TaskDetailSync> {
        let task = self.task_manager.get(task_id)?;
        // One shared view per provider change; idle ticks reuse it.
        let view = self.agent_runtime.shared_snapshot(task_id);
        let has_stream = self.task_manager.has_active_agent_event_stream(task_id);
        let native_idle = task.status == TaskStatus::ReadyForReview
            && has_stream
            && view
                .as_ref()
                .is_some_and(|view| view.phase == CodexAppServerPhase::Ready);
        let native_preparing = self.agent_runtime.has_preparing(task_id);
        let terminal_retry_available = self.task_manager.terminal_retry_session_id(task_id).is_ok();
        let native_terminal_fallback_available =
            self.agent_runtime.can_continue_in_terminal(task_id)
                && self
                    .task_manager
                    .native_terminal_fallback_eligible(task_id)
                    .is_ok();
        let mut status_line = row_status_line(&TaskRowSnapshot {
            id: task.id,
            title: task.title.clone(),
            provider: task.provider,
            status: task.status,
            runtime_kind: task.runtime_kind,
            branch: task.branch.clone(),
            has_agent_terminal: task.terminal_session_id.is_some(),
            has_validation_terminal: task.validation.terminal_session_id.is_some(),
            has_active_agent_stream: has_stream,
            native_preparing,
            validation_status: task.validation.status,
            validation_attempt: task.validation.attempt,
            needs_attention: task.needs_attention(),
            status_detail: task.status_detail.clone(),
            validation_detail: validation_history_preview(
                task.validation.status_detail.as_deref(),
                self.agent_runtime.hosted_validation_snapshot(task.id),
                1024,
                false,
            ),
        });
        if let Some(detail) = &task.status_detail {
            status_line = format!("{status_line} · {detail}");
        }
        if let Some(detail) = self.task_announcements.issue_detail(task_id) {
            status_line = format!("{status_line} · Latest runtime issue: {detail}");
        }
        if task.validation.attempt > 0 {
            status_line = format!(
                "{status_line} · validation #{} {}",
                task.validation.attempt,
                task.validation.status.label()
            );
            if let Some(detail) = &task.validation.status_detail {
                status_line = format!("{status_line} · {detail}");
            }
        }

        let can_start_terminal = (task.status == TaskStatus::Created
            && matches!(task.runtime_kind, TaskRuntimeKind::Unassigned)
            && !native_preparing)
            || (task.status == TaskStatus::Created
                && matches!(
                    task.runtime_kind,
                    TaskRuntimeKind::Terminal | TaskRuntimeKind::TerminalFallback
                ))
            || native_terminal_fallback_available
            || (task.status == TaskStatus::Failed && terminal_retry_available);
        let follow_up_hint = if native_idle && !native_ai_enabled {
            "Enable AI features and command-context sharing before sending another turn".to_string()
        } else if native_idle {
            format!(
                "{} session · Enter for a new line · Ctrl+Enter to send. Finish the session when ready to validate.",
                task.provider.display_name()
            )
        } else {
            String::new()
        };
        let diff = self
            .agent_diff
            .requested_cwd()
            .filter(|requested| *requested == task.worktree_path)
            .map(|_| DiffSync {
                header: format!(
                    "git diff {} · {}",
                    self.agent_diff.requested_base().unwrap_or("HEAD"),
                    crate::agent_task::diff::visible_diff_cwd(&task.worktree_path)
                ),
                scope: if self.agent_diff.requested_base() == Some("HEAD")
                    || self.agent_diff.requested_base().is_none()
                {
                    "Current working tree; this view can include changes that predate the Agent task."
                        .to_string()
                } else {
                    "Compared with the immutable task baseline; includes Agent commits plus current working-tree changes."
                        .to_string()
                },
                loading: self.agent_diff.state().loading,
                error: self.agent_diff.state().error.clone(),
                truncated: self.agent_diff.state().truncated,
                text: self.agent_diff.state().text.clone(),
            });

        let mut inspector = agent_task_ui::task_inspector_text(task);
        if let Some(history) = validation_history_preview(
            None,
            self.agent_runtime.hosted_validation_snapshot(task_id),
            8192,
            true,
        ) {
            inspector.push_str("\n\n");
            inspector.push_str(&history);
        }

        // Redaction may remove a recipe from this display replica. The host
        // retains the original task's review mode without exposing its command.
        let manual_review = self
            .agent_runtime
            .hosted_manual_review(task_id)
            .unwrap_or_else(|| task.requires_manual_review());
        let archived = task.status == TaskStatus::Archived;
        let archive_check = self.task_manager.can_archive(task_id);
        Some(TaskDetailSync {
            id: task.id,
            provider: task.provider,
            archived,
            can_archive: !archived && archive_check.is_ok(),
            archive_note: (!archived && archive_check.is_err())
                .then(|| ARCHIVE_BLOCKED.to_string()),
            can_restore: archived && task.archived_from.is_some(),
            title: task.title.clone(),
            status_line,
            branch: task.branch.clone(),
            inspector,
            validation_status: task.validation.status,
            guidance: crate::agent_task::TaskGuidance::from_task(
                task,
                crate::agent_task::DashboardActivity {
                    has_agent_terminal: can_focus_terminal,
                    has_validation_terminal: can_focus_validation,
                    has_active_agent_stream: has_stream,
                    native_preparing,
                },
            ),
            native_label: if self.agent_runtime.can_resume_codex(task_id) {
                "Resume native".into()
            } else {
                format!("Start {}", task.provider.display_name())
            },
            manual_review,
            can_accept_review: manual_review
                && task.status == TaskStatus::ReadyForReview
                && task.validation.status == TaskValidationStatus::NotRun
                && !has_stream,
            approvals: view
                .as_ref()
                .map(|snapshot| snapshot.pending_approvals.clone())
                .unwrap_or_default(),
            completed_turns: view.as_ref().map_or(0, |snapshot| snapshot.completed_turns),
            stream: view,
            can_start_native: native_ai_enabled
                && !native_preparing
                && (self.agent_runtime.can_resume_codex(task_id)
                    || (task.provider.supports_native_driver()
                        && (task.brief.is_some() || task.source_context.is_some())
                        && task.status == TaskStatus::Created
                        && task.runtime_kind == TaskRuntimeKind::Unassigned)),
            can_start_terminal,
            can_focus_terminal,
            can_focus_validation,
            can_stop: (native_preparing || has_stream) && !native_idle,
            can_finish: native_idle,
            can_run_validation: !manual_review
                && (if self.agent_runtime.conversation_id(task_id).is_some() {
                    self.agent_runtime.can_start_hosted_validation(task_id)
                } else {
                    task.validation_command().is_some()
                })
                && task.status == TaskStatus::ReadyForReview
                && task.validation.status != TaskValidationStatus::Running
                && !has_stream,
            can_complete: !manual_review
                && task.status == TaskStatus::ReadyForReview
                && task.validation.status == TaskValidationStatus::Passed
                && !has_stream,
            can_follow_up: native_idle
                && native_ai_enabled
                && self.agent_runtime.can_prompt_native(task_id),
            follow_up_hint,
            diff,
        })
    }

    fn active_pane(&self) -> Option<&Pane> {
        let tab = self.tabs.get(self.active)?;
        tab.panes.get(tab.active_pane)
    }

    fn pane(&self, pane_id: u64) -> Option<&Pane> {
        self.tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .find(|pane| pane.id == pane_id)
    }

    fn pane_mut(&mut self, pane_id: u64) -> Option<&mut Pane> {
        self.tabs
            .iter_mut()
            .flat_map(|tab| tab.panes.iter_mut())
            .find(|pane| pane.id == pane_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{block_agent_task_anchor_disabled_reason, record_task_terminal_launch_failure};
    use crate::agent_task::{
        NewTask, TaskManager, TaskStatus, TaskTerminalRole, TaskValidationStatus,
    };
    use crate::block_view::BlockAgentEvidence;

    fn sample_evidence(
        is_background: bool,
        command: Option<&str>,
        command_exact: bool,
    ) -> BlockAgentEvidence {
        BlockAgentEvidence {
            block_id: 1,
            command: command.map(str::to_string),
            command_exact,
            command_truncated: false,
            cwd: Some("/tmp".to_string()),
            exit_code: Some(0),
            duration_ms: Some(1),
            output_text: "ok\n".to_string(),
            output_available: true,
            output_truncated: false,
            output_total_bytes: 3,
            is_background,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn agent_task_anchor_rejects_background_output_even_when_preflight_passes() {
        let evidence = sample_evidence(true, None, false);
        assert_eq!(
            crate::agent_task::context::block_agent_context_disabled_reason(
                evidence.command.as_deref(),
                evidence.command_exact,
                evidence.command_truncated,
                evidence.cwd.as_deref(),
                Some(evidence.output_available),
            ),
            None,
            "shared preflight still treats commandless output as attachable"
        );
        assert_eq!(
            block_agent_task_anchor_disabled_reason(&evidence),
            Some("Background output blocks are not shell commands")
        );
    }

    #[test]
    fn agent_task_anchor_still_requires_exact_command_metadata() {
        let evidence = sample_evidence(false, Some("cargo test"), false);
        assert_eq!(
            block_agent_task_anchor_disabled_reason(&evidence),
            Some("Exact command metadata is required")
        );
        let evidence = sample_evidence(false, Some("cargo test"), true);
        assert_eq!(block_agent_task_anchor_disabled_reason(&evidence), None);
    }

    fn sample_new_task(title: &str) -> NewTask {
        NewTask {
            title: title.to_string(),
            provider: crate::agent_task::AgentProvider::Codex,
            repo_root: std::path::PathBuf::from("/tmp/repo"),
            worktree_path: std::path::PathBuf::from("/tmp/repo/wt"),
            branch: "task/1".into(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            source_context: None,
        }
    }

    #[test]
    fn validation_launch_failure_is_cancelled_not_inconclusive() {
        let mut manager = TaskManager::new();
        let task_id = manager.create(sample_new_task("validate-me")).unwrap();
        manager
            .update_status(task_id, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(task_id, "validation-launch-fail".into())
            .unwrap();

        record_task_terminal_launch_failure(
            &mut manager,
            "validation-launch-fail",
            TaskTerminalRole::Validation,
        );

        let task = manager.get(task_id).unwrap();
        assert_eq!(task.validation.status, TaskValidationStatus::Cancelled);
        assert_eq!(
            manager.next_validation_attempt(task_id).ok(),
            Some(2),
            "a cancelled validation attempt must not block the next run"
        );
    }

    #[test]
    fn launch_failure_clears_validation_pin_without_waiting_for_exit() {
        let mut manager = TaskManager::new();
        let task_id = manager.create(sample_new_task("pin-me")).unwrap();
        manager
            .update_status(task_id, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(task_id, "validation-pin".into())
            .unwrap();
        record_task_terminal_launch_failure(
            &mut manager,
            "validation-pin",
            TaskTerminalRole::Validation,
        );
        assert_eq!(
            manager.get(task_id).unwrap().validation.status,
            TaskValidationStatus::Cancelled
        );
    }

    #[test]
    fn agent_launch_failure_stays_failed_for_terminal_retry() {
        let mut manager = TaskManager::new();
        let task_id = manager.create(sample_new_task("agent-me")).unwrap();
        manager
            .bind_terminal_session(task_id, "agent-launch-fail".into())
            .unwrap();

        record_task_terminal_launch_failure(
            &mut manager,
            "agent-launch-fail",
            TaskTerminalRole::Agent,
        );

        let task = manager.get(task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(
            manager.terminal_retry_session_id(task_id).ok(),
            Some("agent-launch-fail")
        );
    }

    #[test]
    fn agent_launch_failure_does_not_cancel_validation() {
        let mut manager = TaskManager::new();
        let task_id = manager.create(sample_new_task("agent-only")).unwrap();
        manager
            .bind_terminal_session(task_id, "agent-only-fail".into())
            .unwrap();

        record_task_terminal_launch_failure(
            &mut manager,
            "agent-only-fail",
            TaskTerminalRole::Agent,
        );

        let task = manager.get(task_id).unwrap();
        assert_eq!(task.validation.status, TaskValidationStatus::NotRun);
        assert_ne!(task.validation.status, TaskValidationStatus::Cancelled);
    }

    #[test]
    fn validation_launch_failure_does_not_fail_agent_task() {
        let mut manager = TaskManager::new();
        let task_id = manager.create(sample_new_task("dual-terminal")).unwrap();
        manager
            .bind_terminal_session(task_id, "agent-still-ok".into())
            .unwrap();
        manager
            .update_status(task_id, TaskStatus::ReadyForReview, None)
            .unwrap();
        manager
            .bind_validation_session(task_id, "validation-launch-fail".into())
            .unwrap();

        record_task_terminal_launch_failure(
            &mut manager,
            "validation-launch-fail",
            TaskTerminalRole::Validation,
        );

        let task = manager.get(task_id).unwrap();
        assert_eq!(task.validation.status, TaskValidationStatus::Cancelled);
        assert_ne!(task.status, TaskStatus::Failed);
        assert_eq!(
            manager.terminal_retry_session_id(task_id).ok(),
            None,
            "validation launch failure must not mark the agent terminal for retry"
        );
    }
}

#[cfg(test)]
mod disabled_task_lifecycle_tests {
    use super::*;

    #[test]
    fn opt_out_preserves_stop_and_review_but_never_starts_work() {
        let task = TaskId::new();
        let approval = crate::agent_task::ApprovalId::new();
        for action in [
            TaskPanelAction::CreateFromBlock,
            TaskPanelAction::StartNative(task),
            TaskPanelAction::StartTerminal(task),
            TaskPanelAction::FollowUp(agent_task_ui::TaskFollowUpRequest::new(
                task,
                "next turn".into(),
            )),
            TaskPanelAction::Approve(task, approval),
            TaskPanelAction::RunValidation(task),
        ] {
            assert!(task_action_allowed(true, &action));
            assert!(!task_action_allowed(false, &action));
            assert!(
                task_action_allowed(true, &action),
                "re-enable restores access"
            );
        }
        for action in [
            TaskPanelAction::Select(task),
            TaskPanelAction::Close,
            TaskPanelAction::StopCodex(task),
            TaskPanelAction::FinishCodex(task),
            TaskPanelAction::Deny(task, approval),
            TaskPanelAction::FocusTerminal(task),
            TaskPanelAction::FocusValidation(task),
            TaskPanelAction::ReviewDiff(task),
            TaskPanelAction::Archive(task),
            TaskPanelAction::Complete(task),
        ] {
            assert!(task_action_allowed(false, &action));
        }
    }

    #[test]
    fn drain_survives_opt_out_without_registering_or_starting_new_work() {
        let source = include_str!("task_ops.rs");
        let tick = source
            .split("pub(crate) fn agent_tasks_tick(")
            .nth(1)
            .unwrap()
            .split("fn existing_task_terminal(")
            .next()
            .unwrap();
        let before_poll = tick.split(".poll(").next().unwrap();
        assert!(!before_poll.contains("if self.config.borrow().agent_tasks_enabled"));
        assert!(
            tick.contains(".has_any_activity()"),
            "cleanup remains scheduled even with no task rows"
        );
        assert!(tick.contains("if self.config.borrow().agent_tasks_enabled {"));
        assert!(tick.contains("its worktree was preserved without starting an agent"));
        let policy = source
            .split("fn task_prompt_policy(")
            .nth(1)
            .unwrap()
            .split("\n    }")
            .next()
            .unwrap();
        assert!(policy.contains("policy.share_command_context &= config.agent_tasks_enabled"));
        let dispatch = source
            .split("pub(crate) fn execute_task_panel_action(")
            .nth(1)
            .unwrap()
            .split("fn decide_native_approval(")
            .next()
            .unwrap();
        assert!(dispatch
            .contains("task_action_allowed(self.config.borrow().agent_tasks_enabled, &action)"));
        assert!(
            dispatch.contains(".cancel(task_id)"),
            "Stop keeps the requested stable identity"
        );
    }
}

#[cfg(test)]
mod hosted_validation_history_tests {
    use super::validation_history_preview;
    use jterm_core::agent_workspace::HostedValidationSnapshot;

    #[test]
    fn saved_output_is_readable_without_a_terminal_and_safe_for_both_views() {
        let secret = format!("sk-{}", "x".repeat(32));
        let history = HostedValidationSnapshot {
            run_id: "synthetic-validation".into(),
            preparing: false,
            terminal: None,
            output: format!("first line\n{secret}\n\u{1b}[31m\u{202e}failure"),
            output_truncated: true,
        };
        let detail = validation_history_preview(None, Some(&history), 8192, true).unwrap();
        assert!(detail.contains("first line\n"));
        assert!(detail.contains("earlier output was not retained"));
        assert!(detail.contains("REDACTED"));
        assert!(!detail.contains(&secret));
        assert!(!detail.contains('\u{1b}'));
        assert!(!detail.contains('\u{202e}'));
        let row =
            validation_history_preview(Some("Host restarted"), Some(&history), 96, false).unwrap();
        assert!(row.starts_with("Host restarted"));
        assert!(row.len() <= 96);
        assert!(!row.contains('\n'));
        assert!(!row.contains(&secret));
    }
}
