//! Application-side execution for the agent Tasks panel.
//!
//! The panel component stages [`TaskPanelAction`] values; this module resolves
//! the task again at execution time, drives the ported `agent_task` domain
//! (task manager, native runtime, diff worker), spawns task terminals as
//! pruned tabs, and pushes composed snapshots back to the panel. The flow
//! mirrors ember's `app/tasks.rs` action executor and poll loop.

use std::collections::HashMap;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use relm4::gtk::glib;
use relm4::{ComponentController, ComponentSender};

use crate::agent_task::{
    AgentProvider, AgentSessionOutcome, ApprovalDecision, CodexAppServerPhase, NativePromptPolicy,
    NewTask, TaskId, TaskRuntimeKind, TaskStatus, TaskTerminalRole, TaskValidationStatus,
};
use crate::agent_task_ui::{
    self, order_task_rows, row_status_line, TaskPanelAction, TaskRowSnapshot,
};
use crate::app_msg::AppMsg;
use crate::dialogs::tasks_panel::{DiffSync, TaskDetailSync, TasksPanelMsg, TasksPanelSync};
use crate::workspace::Pane;
use crate::AppModel;

/// Poll cadence while any task machinery is active; the runtime's own frame
/// budgets make each tick a bounded, nonblocking drain.
const TASKS_POLL_FAST: Duration = Duration::from_millis(120);
/// Idle cadence while tasks exist but nothing is running; keeps archived
/// tasks from ever waking the loop.
const TASKS_POLL_SLOW: Duration = Duration::from_millis(2_000);

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

impl AppModel {
    pub(crate) fn toggle_tasks_panel(&mut self, sender: &ComponentSender<AppModel>) {
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
        let visible = !self.tasks_panel_visible.get();
        self.tasks_panel_visible.set(visible);
        self.sync_side_panel();
        if visible {
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
        self.agent_tasks_timer_armed.set(false);
        let mut keep_fast = false;

        if self.config.borrow().agent_tasks_enabled {
            let policy = self.task_prompt_policy();
            let report = self.agent_runtime.poll(&mut self.task_manager, policy);
            if let Some(issue) = report.issues.last() {
                self.show_toast(format!("Native Agent issue: {}", issue.detail));
            } else if let Some(completion) = report.completions.last() {
                let message = if report.completions.len() > 1 {
                    format!(
                        "{} native Codex sessions stopped; open Tasks for individual results",
                        report.completions.len()
                    )
                } else {
                    match completion.outcome {
                        AgentSessionOutcome::Clean => {
                            "Native Codex stopped cleanly; review its diff, then run validation"
                                .to_string()
                        }
                        AgentSessionOutcome::Cancelled => {
                            "Native Codex was cancelled and fully stopped".to_string()
                        }
                        AgentSessionOutcome::Failed => format!(
                            "Native Codex failed: {}",
                            completion
                                .detail
                                .as_deref()
                                .unwrap_or("provider session did not complete")
                        ),
                    }
                };
                self.show_toast(message);
            }
            keep_fast |= report.made_progress() || self.agent_runtime.needs_fast_poll();

            if let Some(pending) = &self.pending_task_creation {
                match pending.receiver.try_recv() {
                    Ok(Ok(prepared)) => {
                        self.pending_task_creation = None;
                        self.register_prepared_task(prepared, sender);
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

        let has_tasks = self.task_manager.tasks().iter().next().is_some();
        if keep_fast || has_tasks || self.tasks_panel_visible.get() {
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
        agent_task_ui::prompt_policy(&self.config.borrow())
    }

    /// Stage a panel action against the live domain. Every branch resolves
    /// the task again; a stale panel snapshot can never retarget an action.
    pub(crate) fn execute_task_panel_action(
        &mut self,
        action: TaskPanelAction,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.config.borrow().agent_tasks_enabled {
            self.show_toast("Agent tasks are disabled in the configuration");
            return;
        }
        match action {
            TaskPanelAction::CreateFromBlock => self.create_task_from_block(sender),
            TaskPanelAction::Select(task_id) => {
                self.selected_task = Some(task_id);
                self.sync_tasks_panel();
            }
            TaskPanelAction::Close => {
                self.tasks_panel_visible.set(false);
                self.sync_side_panel();
            }
            TaskPanelAction::StartCodex(task_id) => {
                let policy = self.task_prompt_policy();
                match self
                    .agent_runtime
                    .start_codex(&mut self.task_manager, task_id, policy)
                {
                    Ok(()) => {
                        self.show_toast("Preparing native Codex prerequisites in the background…")
                    }
                    Err(error) => self.show_toast(format!("Could not start native Codex: {error}")),
                }
            }
            TaskPanelAction::StartTerminal(task_id) => {
                self.start_task_agent_terminal(task_id, sender)
            }
            TaskPanelAction::StopCodex(task_id) => match self.agent_runtime.cancel(task_id) {
                Ok(()) => {
                    if self.agent_runtime.has_running(task_id) {
                        self.show_toast("Stopping Codex and waiting for process cleanup…");
                    } else {
                        self.show_toast(
                            "Native Codex preparation cancelled; finishing background cleanup…",
                        );
                    }
                }
                Err(error) => self.show_toast(error.to_string()),
            },
            TaskPanelAction::FollowUp(task_id, text) => {
                let policy = self.task_prompt_policy();
                match self
                    .agent_runtime
                    .prompt_codex(&self.task_manager, task_id, &text, policy)
                {
                    Ok(()) => self.show_toast("Follow-up queued on the existing Codex thread…"),
                    Err(error) => self.show_toast(error.to_string()),
                }
            }
            TaskPanelAction::FinishCodex(task_id) => {
                match self.agent_runtime.finish_codex(&self.task_manager, task_id) {
                    Ok(()) => self.show_toast(
                        "Finishing Codex and waiting for containment cleanup before validation…",
                    ),
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
                    Ok(()) => self.show_toast("Task marked complete after passing validation"),
                    Err(error) => self.show_toast(error.to_string()),
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
                self.agent_diff.is_open = true;
                if let Err(error) = self.agent_diff.request_from(worktree, base_commit) {
                    self.show_toast(format!("Could not open task diff: {error}"));
                }
            }
            TaskPanelAction::Archive(task_id) => match self.task_manager.archive(task_id) {
                Ok(()) => {
                    self.agent_runtime.clear_retained(task_id);
                    if self.selected_task == Some(task_id) {
                        self.selected_task = None;
                    }
                    self.show_toast("Task hidden; worktree left in place");
                }
                Err(error) => self.show_toast(error.to_string()),
            },
        }
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

    /// Begin task creation from the active pane's selected block. The block
    /// preflight is the exact one ember shares between its block menu and
    /// panel: exact shell-reported command and cwd are mandatory, and the
    /// sharing consent gates nothing here because no provider is contacted
    /// until Start Codex.
    fn create_task_from_block(&mut self, sender: &ComponentSender<AppModel>) {
        if self.pending_task_creation.is_some() {
            self.show_toast("Another task worktree is still being created");
            return;
        }
        let Some(pane) = self.active_pane() else {
            self.show_toast("No active terminal pane");
            return;
        };
        let Some(evidence) = pane.terminal.selected_block_agent_evidence(80) else {
            self.show_toast("Select a finished block in a Block-mode pane to create an agent task");
            return;
        };
        if let Some(reason) = crate::agent_task::context::block_agent_context_disabled_reason(
            evidence.command.as_deref(),
            evidence.command_exact,
            evidence.command_truncated,
            evidence.cwd.as_deref(),
            Some(evidence.output_available),
        ) {
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
        let new_task = NewTask {
            title: prepared.title,
            provider: prepared.provider,
            repo_root: prepared.worktree.repository.clone(),
            worktree_path: prepared.worktree.path.clone(),
            branch: prepared.worktree.branch.clone(),
            base_commit: prepared.worktree.head.clone(),
            source_context: Some(prepared.context),
        };
        match self.task_manager.create(new_task) {
            Ok(task_id) => {
                self.selected_task = Some(task_id);
                self.show_toast("Task created in an isolated worktree; start Codex when ready");
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
        if self.agent_runtime.has_preparing(task_id) {
            self.show_toast("Cancel native Codex preparation before starting a terminal");
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
            TaskTerminalRole::Agent,
            String::new(),
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
        self.show_toast(format!(
            "Opened {} in an isolated task terminal; task context remains in Anvil",
            provider.display_name()
        ));
    }

    /// Rerun the task's exact validation command in a fresh PTY inside the
    /// pinned worktree cwd. The prepared pin is retained until the pane
    /// reports its spawn, so the child enters the directory through the
    /// validated descriptor rather than a re-resolved path.
    fn start_task_validation(&mut self, task_id: TaskId, sender: &ComponentSender<AppModel>) {
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
            TaskTerminalRole::Validation,
            String::new(),
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
        self.pending_validation_pins.remove(&pane_id);
        let Some((session_id, role)) = self
            .pane(pane_id)
            .and_then(|pane| pane.task_session_id.clone().zip(pane.task_role))
        else {
            return false;
        };
        self.task_manager
            .handle_terminal_session_exit(&session_id, Some(exit_code));
        if let Some(task_id) = self
            .task_manager
            .handle_terminal_session_closed(&session_id)
        {
            if role == TaskTerminalRole::Validation {
                let outcome = self.task_manager.get(task_id).map(|task| {
                    (
                        task.validation.status,
                        task.validation.status_detail.clone(),
                    )
                });
                if let Some((status, detail)) = outcome {
                    let message = match status {
                        TaskValidationStatus::Passed => "Validation passed".to_string(),
                        TaskValidationStatus::Failed => {
                            format!("Validation failed (exit {exit_code})")
                        }
                        _ => detail.unwrap_or_else(|| "Validation ended".to_string()),
                    };
                    self.show_toast(message);
                }
            }
        }
        if self.tasks_panel_visible.get() {
            self.sync_tasks_panel();
        }
        true
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
    }

    /// Push the composed panel state. Rows are rebuilt from the domain every
    /// time; the panel never holds its own task copies.
    pub(crate) fn sync_tasks_panel(&self) {
        let policy = self.task_prompt_policy();
        let native_ai_enabled = policy.share_command_context;
        let mut rows: Vec<TaskRowSnapshot> = self
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
                has_agent_terminal: task.terminal_session_id.is_some(),
                has_validation_terminal: task.validation.terminal_session_id.is_some(),
                has_active_agent_stream: self.task_manager.has_active_agent_event_stream(task.id),
                native_preparing: self.agent_runtime.has_preparing(task.id),
                validation_status: task.validation.status,
                validation_attempt: task.validation.attempt,
                needs_attention: task.needs_attention(),
                status_detail: task.status_detail.clone(),
            })
            .collect();
        let updated: HashMap<TaskId, u64> = self
            .task_manager
            .tasks()
            .iter()
            .map(|task| (task.id, task.updated_at_ms))
            .collect();
        order_task_rows(&mut rows, |id| updated.get(&id).copied().unwrap_or(0));

        let selected = self
            .selected_task
            .filter(|id| self.task_manager.get(*id).is_some());
        let detail = selected.and_then(|id| self.task_detail_sync(id, native_ai_enabled));

        let create_hint = if self.pending_task_creation.is_some() {
            "Creating the isolated task worktree…".to_string()
        } else if !native_ai_enabled {
            "Start Codex needs AI enabled plus command-context sharing consent (config: ai_enabled, ai_share_command_context)"
                .to_string()
        } else {
            String::new()
        };
        let sync = TasksPanelSync {
            rows,
            selected,
            detail: detail.map(Box::new),
            create_enabled: true,
            create_hint,
            pending_creation: self.pending_task_creation.is_some(),
        };
        self.tasks_panel.emit(TasksPanelMsg::Sync(Box::new(sync)));
    }

    fn task_detail_sync(&self, task_id: TaskId, native_ai_enabled: bool) -> Option<TaskDetailSync> {
        let task = self.task_manager.get(task_id)?;
        let view = self.agent_runtime.snapshot(task_id);
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
        });
        if let Some(detail) = &task.status_detail {
            status_line = format!("{status_line} · {detail}");
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
            "Send another turn on this loaded Codex thread, or finish the session to unlock validation"
                .to_string()
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

        Some(TaskDetailSync {
            id: task.id,
            title: task.title.clone(),
            status_line,
            branch: task.branch.clone(),
            stream: view.map(Box::new),
            approvals: self
                .agent_runtime
                .snapshot(task_id)
                .map(|snapshot| snapshot.pending_approvals.clone())
                .unwrap_or_default(),
            completed_turns: self
                .agent_runtime
                .snapshot(task_id)
                .map_or(0, |snapshot| snapshot.completed_turns),
            can_start_codex: task.status == TaskStatus::Created
                && task.runtime_kind == TaskRuntimeKind::Unassigned
                && !native_preparing
                && native_ai_enabled,
            can_start_terminal,
            can_stop: (native_preparing || has_stream) && !native_idle,
            can_finish: native_idle,
            can_run_validation: task.status == TaskStatus::ReadyForReview
                && task.validation.status != TaskValidationStatus::Running
                && !has_stream,
            can_complete: task.status == TaskStatus::ReadyForReview
                && task.validation.status == TaskValidationStatus::Passed,
            can_follow_up: native_idle && native_ai_enabled,
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
