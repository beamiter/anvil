//! AI Agent session orchestration for the Relm4 application model.
//!
//! The pure state machine in `agent` owns every protocol transition. This
//! integration layer only snapshots views, starts provider requests, and
//! performs a command after an explicit approval token is returned.

use super::*;
use jterm_core::agent::{AgentSessionSnapshot, AgentSnapshotError, MAX_AGENT_SNAPSHOT_JSON_BYTES};
use std::path::Path;

pub(crate) struct AgentBlockCompletion {
    pub(crate) tab_id: u64,
    pub(crate) pane_id: u64,
    pub(crate) command: String,
    pub(crate) exit_code: i32,
    pub(crate) output: String,
    pub(crate) agent_execution: Option<agent::AgentExecutionRef>,
}

fn should_publish_reply_activity(result: &Result<bool, agent::SessionError>) -> bool {
    // Ok(false) is the one stale/cancelled callback outcome. Protocol errors
    // are current replies too, and the state machine records their safe error
    // turn before returning Err.
    !matches!(result, Ok(false))
}

#[cfg(test)]
fn read_agent_snapshot(path: &Path) -> Option<AgentSessionSnapshot> {
    let _parent_lock = crate::config_store::PrivateParentLock::acquire(path).ok()?;
    let bytes = crate::config_store::read_private_bytes(path, MAX_AGENT_SNAPSHOT_JSON_BYTES as u64)
        .ok()??;
    let encoded = std::str::from_utf8(&bytes).ok()?;
    AgentSessionSnapshot::from_json(encoded).ok()
}

/// Atomically claim, validate, restore, and consume exactly once while holding
/// anvil's directory namespace lock. Multiple NON_UNIQUE anvil processes can
/// open concurrently; only the process that wins the core claim may receive
/// this session. Invalid evidence is moved aside for inspection.
fn restore_agent_snapshot_once(path: &Path) -> Option<jterm_core::agent::AgentSession> {
    restore_agent_snapshot_once_with_sync(path, crate::config_store::sync_config_parent)
}

fn restore_agent_snapshot_once_with_sync(
    path: &Path,
    sync_parent: impl FnOnce(&Path) -> Result<(), crate::config_store::ConfigWriteError>,
) -> Option<jterm_core::agent::AgentSession> {
    let _parent_lock = match crate::config_store::PrivateParentLock::acquire(path) {
        Ok(lock) => lock,
        Err(error) => {
            log::warn!("agent: could not lock snapshot namespace: {error}");
            return None;
        }
    };
    match jterm_core::agent::try_claim_session_file(path) {
        Ok(jterm_core::agent::SessionClaim::Vacant) => None,
        Ok(jterm_core::agent::SessionClaim::Restored(restored)) => {
            if let Err(error) = sync_parent(path) {
                log::warn!(
                    "agent: snapshot claim for {} was not durable: {error}",
                    path.display()
                );
                return None;
            }
            Some(restored)
        }
        Ok(jterm_core::agent::SessionClaim::Quarantined {
            path: quarantined,
            error,
        }) => {
            log::warn!(
                "agent: invalid snapshot {} quarantined at {}: {error}",
                path.display(),
                quarantined.display()
            );
            if let Err(sync_error) = sync_parent(path) {
                log::warn!(
                    "agent: snapshot quarantine for {} was not durable: {sync_error}",
                    path.display()
                );
            }
            None
        }
        Err(error) => {
            log::warn!(
                "agent: could not atomically claim snapshot {}: {error}",
                path.display()
            );
            None
        }
    }
}

/// Serialize and atomically replace an Agent snapshot under the exact shared
/// protocol budget, without using the pinned core's predictable legacy stage.
fn write_agent_snapshot(
    path: &Path,
    snapshot: &AgentSessionSnapshot,
) -> Result<(), AgentSnapshotError> {
    let _parent_lock = crate::config_store::PrivateParentLock::acquire(path)
        .map_err(|error| AgentSnapshotError::Encode(format!("lock {}: {error}", path.display())))?;
    let encoded = snapshot.to_json()?;
    crate::config_store::write_private_bytes(
        path,
        encoded.as_bytes(),
        MAX_AGENT_SNAPSHOT_JSON_BYTES,
    )
    .map_err(|error| AgentSnapshotError::Encode(format!("write {}: {error}", path.display())))
}

fn remove_agent_snapshot(path: &Path) {
    let Ok(_parent_lock) = crate::config_store::PrivateParentLock::acquire(path) else {
        return;
    };
    // Validate before unlinking so a planted link, FIFO, or oversized evidence
    // file is retained rather than acted on.
    if let Ok(Some(_)) =
        crate::config_store::read_private_bytes(path, MAX_AGENT_SNAPSHOT_JSON_BYTES as u64)
    {
        if std::fs::remove_file(path).is_ok() {
            let _ = crate::config_store::sync_config_parent(path);
        }
    }
}

impl AppModel {
    // ── Agent mode ───────────────────────────────────────────────────────

    fn agent_append_activity_at(&self, tab_id: u64, pane_id: u64, speaker: &str, body: &str) {
        let compact = self.config.borrow().block_compact;
        let message = agent::build_agent_message_block(speaker, body, compact);
        let Some(terminal) = self.terminal_for(tab_id, pane_id) else {
            return;
        };
        terminal.insert_inline_notice(&message);
        if self
            .active_agent
            .borrow()
            .as_ref()
            .is_some_and(|session| session.bound_tab == tab_id && session.bound_pane == pane_id)
        {
            let card: gtk::Widget = self.agent_panel.widget().clone().upcast();
            terminal.insert_inline_notice(&card);
        }
    }

    fn agent_append_activity(&self, speaker: &str, body: &str) {
        let target = self
            .active_agent
            .borrow()
            .as_ref()
            .map(|session| (session.bound_tab, session.bound_pane));
        if let Some((tab_id, pane_id)) = target {
            self.agent_append_activity_at(tab_id, pane_id, speaker, body);
        }
    }

    fn agent_restore_activity(&self, transcript: &[agent::Turn]) {
        for turn in transcript {
            match turn {
                agent::Turn::User(message) => self.agent_append_activity("You", message),
                agent::Turn::AssistantThought(message) => {
                    self.agent_append_activity("Agent (thought)", message)
                }
                agent::Turn::AssistantSay(message) => self.agent_append_activity("Agent", message),
                agent::Turn::AssistantProposed {
                    id,
                    command,
                    status,
                } => {
                    let verdict = match status {
                        agent::ProposalStatus::Pending => "awaiting approval",
                        agent::ProposalStatus::Approved => "approved and ran",
                        agent::ProposalStatus::Rejected => "rejected",
                        agent::ProposalStatus::ManualReview => "moved to manual review",
                    };
                    self.agent_append_activity(
                        "Agent",
                        &format!("Proposed command #{} ({verdict}): {}", id.get(), command),
                    );
                }
                agent::Turn::Observation {
                    exit_code,
                    output_sample,
                    ..
                } => self
                    .agent_append_activity("Output", &format!("exit {exit_code}\n{output_sample}")),
                agent::Turn::ProtocolError(message) => self.agent_append_activity("Error", message),
            }
        }
    }

    pub(crate) fn open_agent_panel(&self, sender: &ComponentSender<AppModel>) {
        // Match Forge's stateful top-bar/shortcut behavior: invoking the action
        // again closes the one live inline session.
        if self.active_agent.borrow().is_some() {
            self.agent_close();
            return;
        }
        if self.safe_mode {
            self.show_toast("Shell Agent is unavailable in safe mode.");
            self.sync_agent_toggle();
            return;
        }
        let cfg = self.config.borrow();
        if !cfg.ai_enabled || !cfg.agent_enabled {
            log::info!(
                "agent: disabled (ai_enabled={}, agent_enabled={})",
                cfg.ai_enabled,
                cfg.agent_enabled
            );
            self.show_toast("Shell Agent is disabled in Settings.");
            self.sync_agent_toggle();
            return;
        }
        let max_turns = cfg.agent_max_turns;
        let client = match ai::client_from_config(&cfg) {
            Ok(client) => client,
            Err(error) => {
                log::warn!("agent: {error}");
                self.show_toast(format!("AI provider is unavailable: {error}"));
                self.sync_agent_toggle();
                return;
            }
        };
        drop(cfg);

        let Some(tab) = self.tabs.get(self.active) else {
            self.sync_agent_toggle();
            return;
        };
        let Some(pane) = tab.panes.get(tab.active_pane) else {
            self.sync_agent_toggle();
            return;
        };
        if !matches!(pane.mode, TerminalMode::Block) {
            self.show_toast("Shell Agent requires an active Block pane.");
            self.sync_agent_toggle();
            return;
        }
        let (tab_id, pane_id) = (tab.id, pane.id);
        let initial_context = pane.terminal.selected_block_context(80);
        let prompt_status = pane.terminal.agent_command_prompt_status();

        // Replacing a session invalidates both its provider callback and any
        // late BlockFinished event before the new identity becomes visible.
        if let Some(mut previous) = self.active_agent.borrow_mut().take() {
            previous.cancel();
        }
        // A snapshot persisted by the previous run is restored one-shot and
        // rebound to the pane the user reopened the Agent on.
        let snapshot_file = Self::agent_snapshot_path();
        let restored = restore_agent_snapshot_once(&snapshot_file);
        let (mut session, was_restored) = match restored {
            Some(inner) => match agent::AgentSession::from_restored(inner, tab_id, pane_id) {
                Some(session) => {
                    self.show_toast("Restored the previous agent session.");
                    (session, true)
                }
                None => {
                    log::warn!(
                        "agent: discarded a restored session containing unsafe command text"
                    );
                    self.show_toast("Discarded an unsafe saved Agent session.");
                    (agent::AgentSession::new(tab_id, pane_id, max_turns), false)
                }
            },
            None => (agent::AgentSession::new(tab_id, pane_id, max_turns), false),
        };
        session.last_manual_completed = initial_context;
        // Forge keeps typo correction and the multi-turn Agent mutually
        // exclusive. Cancel any visible/in-flight correction before the Agent
        // becomes the pane's active assistant surface.
        self.close_all_command_corrections();
        *self.active_agent.borrow_mut() = Some(session);
        let panel_generation = self.agent_panel_generation.get().wrapping_add(1);
        self.agent_panel_generation.set(panel_generation);

        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Open {
                provider_name: client.display_name(),
                view,
            });
        }
        let card: gtk::Widget = self.agent_panel.widget().clone().upcast();
        let inserted = self
            .terminal_for(tab_id, pane_id)
            .is_some_and(|terminal| terminal.insert_inline_notice(&card));
        if !inserted {
            self.show_toast("Shell Agent target pane is no longer available.");
            self.agent_close();
            return;
        }
        self.agent_panel
            .emit(agent::AgentPanelMsg::PromptStatus(prompt_status));
        self.agent_panel.emit(agent::AgentPanelMsg::Focus);
        self.sync_agent_toggle();

        self.agent_append_activity(
            "Agent",
            if self
                .active_agent
                .borrow()
                .as_ref()
                .and_then(|session| session.last_manual_completed.as_ref())
                .is_some()
            {
                "Bound to this Block pane with the selected finished Block attached as untrusted context. I can propose commands, but cannot run one without your explicit approval."
            } else {
                "Bound to this Block pane. I can propose commands, but cannot run one without your explicit approval."
            },
        );
        if was_restored {
            self.agent_append_activity(
                "Agent",
                "Restored the previous Agent session from your last run.",
            );
            let transcript = self
                .active_agent
                .borrow()
                .as_ref()
                .map(|session| session.transcript().to_vec())
                .unwrap_or_default();
            self.agent_restore_activity(&transcript);
        }

        // Keep the readiness chip live while the card is open without
        // rebuilding the editable proposal or transcript every tick.
        let active_agent = self.active_agent.clone();
        let live_generation = self.agent_panel_generation.clone();
        let prompt_sender = sender.clone();
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            if live_generation.get() != panel_generation {
                return gtk::glib::ControlFlow::Break;
            }
            let epoch = active_agent
                .borrow()
                .as_ref()
                .and_then(|session| (!session.is_cancelled()).then_some(session.epoch()));
            let Some(epoch) = epoch else {
                return gtk::glib::ControlFlow::Break;
            };
            prompt_sender.input(AppMsg::AgentRefreshPrompt(epoch));
            gtk::glib::ControlFlow::Continue
        });
        // A restored session may have died mid-request; resume it.
        if self.agent_is_awaiting_model() {
            self.agent_kick_llm(sender);
        }
    }

    pub(crate) fn agent_panel_view(&self) -> Option<agent::AgentPanelView> {
        let (
            epoch,
            transcript,
            turns_used,
            max_turns,
            state,
            loading,
            can_retry_model,
            bound_tab,
            bound_pane,
            attached_context,
        ) = {
            let session = self.active_agent.borrow();
            let session = session.as_ref()?;
            (
                session.epoch(),
                session.transcript().to_vec(),
                session.turns_used(),
                session.max_turns(),
                session.state(),
                session.in_flight.is_some(),
                session.can_retry_model(),
                session.bound_tab,
                session.bound_pane,
                session.last_manual_completed.clone(),
            )
        };
        let terminal = self.terminal_for(bound_tab, bound_pane);
        let prompt_status = terminal.map_or(
            crate::block_view::CommandPromptStatus::ShellIntegrationUnavailable,
            TermCtl::agent_command_prompt_status,
        );
        let cwd = self
            .tabs
            .iter()
            .find(|tab| tab.id == bound_tab)
            .and_then(|tab| tab.panes.iter().find(|pane| pane.id == bound_pane))
            .and_then(|pane| pane.cwd.as_deref())
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or(".")
            .to_string();
        Some(agent::AgentPanelView {
            epoch: Some(epoch),
            transcript,
            turns_used,
            max_turns,
            state,
            loading,
            can_retry_model,
            prompt_status,
            cwd,
            compact: self.config.borrow().block_compact,
            attached_context,
        })
    }

    pub(crate) fn refresh_agent_panel(&self) {
        if let Some(view) = self.agent_panel_view() {
            let pulse = if view.loading {
                Some(crate::organism::AgentPulse::Working)
            } else {
                match view.state {
                    agent::AgentState::Ready => None,
                    agent::AgentState::AwaitingModel
                    | agent::AgentState::AwaitingObservation { .. } => {
                        Some(crate::organism::AgentPulse::Working)
                    }
                    agent::AgentState::AwaitingApproval { .. } => {
                        Some(crate::organism::AgentPulse::AskingReview)
                    }
                    agent::AgentState::Completed | agent::AgentState::TurnLimitReached => {
                        Some(crate::organism::AgentPulse::Finished)
                    }
                    agent::AgentState::Cancelled => Some(crate::organism::AgentPulse::Gone),
                }
            };
            if let Some(pulse) = pulse {
                self.organism_hub.agent_signal().note_phase(pulse);
            }
            self.agent_panel.emit(agent::AgentPanelMsg::Render(view));
            self.pin_agent_panel();
        }
    }

    fn pin_agent_panel(&self) {
        let target = self
            .active_agent
            .borrow()
            .as_ref()
            .map(|session| (session.bound_tab, session.bound_pane));
        let Some((tab_id, pane_id)) = target else {
            return;
        };
        let card: gtk::Widget = self.agent_panel.widget().clone().upcast();
        if let Some(terminal) = self.terminal_for(tab_id, pane_id) {
            terminal.insert_inline_notice(&card);
        }
    }

    pub(crate) fn agent_refresh_prompt(&self, epoch: agent::AgentSessionEpoch) {
        let target = self.active_agent.borrow().as_ref().and_then(|session| {
            (session.epoch() == epoch).then_some((session.bound_tab, session.bound_pane))
        });
        let Some((tab_id, pane_id)) = target else {
            return;
        };
        let status = self.terminal_for(tab_id, pane_id).map_or(
            crate::block_view::CommandPromptStatus::ShellIntegrationUnavailable,
            TermCtl::agent_command_prompt_status,
        );
        self.agent_panel
            .emit(agent::AgentPanelMsg::PromptStatus(status));
    }

    /// Reopen a completed task for a follow-up question in the same
    /// transcript. The session returns to Ready; the user types the next turn.
    pub(crate) fn agent_continue(&self) {
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.continue_after_completion()
        };
        if let Err(error) = result {
            self.report_agent_error("continue", &error);
        }
        self.refresh_agent_panel();
    }

    /// Start a fresh task in the same pane binding, dropping the finished
    /// transcript and restoring the configured turn budget.
    pub(crate) fn agent_new_task(&self) {
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.start_new_task()
        };
        if let Err(error) = result {
            self.report_agent_error("start a new task", &error);
        } else {
            self.agent_append_activity(
                "Agent",
                "Started a fresh task in this pane. Previous activity remains visible but is no longer sent to the model.",
            );
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_stop_request(&self) {
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.stop_model_request()
        };
        match result {
            Ok(true) => {
                self.show_toast("Shell Agent model request stopped.");
                self.agent_append_activity(
                    "Stopped",
                    "Model request stopped. Retry it or revise the instruction.",
                );
            }
            Ok(false) => return,
            Err(error) => self.report_agent_error("stop model request", &error),
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_retry_request(&self, sender: &ComponentSender<AppModel>) {
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.retry_model()
        };
        if let Err(error) = result {
            self.report_agent_error("retry model request", &error);
            self.refresh_agent_panel();
            return;
        }
        self.refresh_agent_panel();
        self.agent_kick_llm(sender);
    }

    /// Attach or replace the currently selected finished Block in the bound
    /// pane. Context remains explicitly user-chosen; unrelated manual command
    /// completions never silently replace it.
    pub(crate) fn agent_attach_context(&self) {
        let target = {
            let guard = self.active_agent.borrow();
            let Some(session) = guard.as_ref() else {
                return;
            };
            if session.state() != agent::AgentState::Ready || session.in_flight.is_some() {
                self.show_toast("Block context can only be changed while Shell Agent is ready.");
                return;
            }
            (session.epoch(), session.bound_tab, session.bound_pane)
        };
        let Some(context) = self
            .terminal_for(target.1, target.2)
            .and_then(|terminal| terminal.selected_block_context(80))
        else {
            self.show_toast("Select a finished Block in the Agent pane first.");
            return;
        };
        let replaced = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut().filter(|session| session.epoch() == target.0) else {
                return;
            };
            session.last_manual_completed.replace(context).is_some()
        };
        self.show_toast(if replaced {
            "Replaced the Shell Agent's attached Block context."
        } else {
            "Attached the selected Block to Shell Agent."
        });
        self.agent_append_activity(
            "Agent",
            if replaced {
                "Replaced the attached Block context with the currently selected finished Block."
            } else {
                "Attached the selected finished Block as untrusted context for upcoming instructions."
            },
        );
        self.refresh_agent_panel();
    }

    /// Detach the selected Block from future model requests.
    pub(crate) fn agent_clear_context(&self) {
        let detached = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            if session.state() != agent::AgentState::Ready || session.in_flight.is_some() {
                self.show_toast("Block context can only be changed while Shell Agent is ready.");
                return;
            }
            session.last_manual_completed.take().is_some()
        };
        if detached {
            self.agent_append_activity(
                "Agent",
                "Selected Block context detached. Session activity is still retained.",
            );
        }
        self.refresh_agent_panel();
    }

    fn agent_snapshot_path() -> std::path::PathBuf {
        let mut path = crate::config::config_file_path();
        path.set_file_name("agent_session.json");
        path
    }

    /// Persist the live Agent session (if any) for the next run. Called on
    /// quit, before the session is dropped.
    pub(crate) fn persist_agent_session(&self) {
        let path = Self::agent_snapshot_path();
        let snapshot = {
            let guard = self.active_agent.borrow();
            guard.as_ref().and_then(|session| session.snapshot())
        };
        match snapshot {
            Some(snapshot) => {
                if let Err(error) = write_agent_snapshot(&path, &snapshot) {
                    log::warn!("agent: could not persist session: {error}");
                }
            }
            None => remove_agent_snapshot(&path),
        }
    }

    /// Submit one user turn. The state machine rejects concurrent sends while
    /// a model, approval, or command observation is outstanding.
    pub(crate) fn agent_send(&self, text: String, sender: &ComponentSender<AppModel>) {
        let visible_text = text.clone();
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.submit_user(text)
        };
        if let Err(error) = result {
            self.report_agent_error("send", &error);
            self.refresh_agent_panel();
            return;
        }
        self.agent_append_activity("You", &visible_text);
        self.refresh_agent_panel();
        self.agent_kick_llm(sender);
    }

    pub(crate) fn agent_approve(
        &self,
        reference: agent::AgentProposalRef,
        edited: Option<String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        // A click, an edit dialog, and a queued message can all outlive the
        // task generation they were raised against. Bind the action to that
        // generation and let the state machine reject stale or
        // already-consumed proposals within it.
        let (proposal_id, tab_id, pane_id) = {
            let guard = self.active_agent.borrow();
            let Some(session) = guard.as_ref() else {
                return;
            };
            let Some(proposal_id) = session.resolve_proposal(reference) else {
                self.show_toast("That Agent proposal is no longer available.");
                return;
            };
            (proposal_id, session.bound_tab, session.bound_pane)
        };

        let Some(terminal) = self.terminal_for(tab_id, pane_id) else {
            self.show_toast("Agent target pane is no longer available.");
            self.agent_close();
            return;
        };
        let prompt_status = terminal.agent_command_prompt_status();
        if !prompt_status.is_ready() {
            self.show_toast(prompt_status.blocked_message());
            self.agent_append_activity("Safety check", prompt_status.blocked_message());
            return;
        }

        let approval = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            match edited {
                Some(command) => session.edit_and_approve(proposal_id, command),
                None => session.approve(proposal_id),
            }
        };
        let approved = match approval {
            Ok(approved) => approved,
            Err(error) => {
                self.report_agent_error("approve", &error);
                self.refresh_agent_panel();
                return;
            }
        };
        debug_assert_eq!(approved.proposal_id, proposal_id);
        if let Some(reason) = approved.danger {
            log::warn!(
                "agent: user approved flagged proposal #{}: {reason}",
                approved.proposal_id.get()
            );
        }

        let pending = {
            let guard = self.active_agent.borrow();
            guard
                .as_ref()
                .and_then(|session| session.awaiting_command.clone())
        };
        let Some(pending) = pending else {
            self.show_toast("Agent could not arm the approved command.");
            self.agent_close();
            return;
        };
        debug_assert_eq!(pending.proposal_id, approved.proposal_id);
        debug_assert_eq!(pending.command, approved.command);

        // The backend re-checks prompt cleanliness, arms the execution and
        // writes the bytes as one UI-thread operation. A queued failure event
        // seals the already-approved protocol state instead of accepting an
        // unrelated completion later.
        terminal.emit(VteInput::RunAgentCommand {
            execution: pending.execution,
            command: pending.command,
        });
        terminal.emit(VteInput::GrabFocus);
        self.refresh_agent_panel();
    }

    /// Move a proposal to the normal shell editor without submitting it. The
    /// Agent records ManualReview and will not attribute a later manual run to
    /// itself or assume an observation.
    pub(crate) fn agent_insert_for_manual_review(
        &self,
        reference: agent::AgentProposalRef,
        command: String,
    ) {
        let (proposal_id, tab_id, pane_id) = {
            let guard = self.active_agent.borrow();
            let Some(session) = guard.as_ref() else {
                return;
            };
            let Some(proposal_id) = session.resolve_proposal(reference) else {
                self.show_toast("That Shell Agent proposal is no longer available.");
                return;
            };
            (proposal_id, session.bound_tab, session.bound_pane)
        };
        let Some(terminal) = self.terminal_for(tab_id, pane_id) else {
            self.show_toast("Shell Agent target pane is no longer available.");
            self.agent_close();
            return;
        };
        let prompt_status = terminal.command_prompt_status();
        if !prompt_status.is_ready() {
            self.show_toast(prompt_status.blocked_message());
            self.agent_append_activity("Safety check", prompt_status.blocked_message());
            return;
        }

        let reviewed = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.edit_for_manual_review(proposal_id, command)
        };
        let command = match reviewed {
            Ok(command) => command,
            Err(error) => {
                self.report_agent_error("move proposal to manual review", &error);
                self.refresh_agent_panel();
                return;
            }
        };
        if !terminal.try_insert_agent_command(&command) {
            // The direct Block-model call is synchronous, so this can only be
            // a target/backend invariant failure after the pre-check. The
            // protocol cannot safely roll ManualReview back to Pending.
            if let Some(session) = self.active_agent.borrow_mut().as_mut() {
                session.cancel();
            }
            self.show_toast("Shell Agent stopped because the target prompt was no longer ready.");
            self.refresh_agent_panel();
            return;
        }
        self.show_toast("Inserted the proposal for manual review. Shell Agent did not run it.");
        self.agent_append_activity(
            "You",
            "Moved the proposal to the shell prompt for manual review. The Agent did not run it and will not assume a result.",
        );
        terminal.emit(VteInput::GrabFocus);
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_reject(
        &self,
        reference: agent::AgentProposalRef,
        sender: &ComponentSender<AppModel>,
    ) {
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            let Some(proposal_id) = session.resolve_proposal(reference) else {
                self.show_toast("That Agent proposal is no longer available.");
                return;
            };
            session.reject(proposal_id)
        };
        if let Err(error) = result {
            self.report_agent_error("reject", &error);
            self.refresh_agent_panel();
            return;
        }
        self.agent_append_activity("You", "Rejected proposal; ask for another approach.");
        self.refresh_agent_panel();
        if self.agent_is_awaiting_model() {
            self.agent_kick_llm(sender);
        }
    }

    pub(crate) fn agent_handle_block_finished(
        &self,
        completion: AgentBlockCompletion,
        sender: &ComponentSender<AppModel>,
    ) {
        let AgentBlockCompletion {
            tab_id,
            pane_id,
            command,
            exit_code,
            output,
            agent_execution,
        } = completion;
        self.pin_agent_panel();
        let proposal_id = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            if session.bound_tab != tab_id || session.bound_pane != pane_id {
                return;
            }
            match agent_execution {
                Some(execution) => match session.correlate_execution(execution, &command) {
                    agent::AgentExecutionMatch::Matched(proposal_id) => Some(proposal_id),
                    agent::AgentExecutionMatch::CommandMismatch => {
                        let failed = session.execution_start_failed(execution);
                        debug_assert!(failed);
                        None
                    }
                    // A stale/internal execution can never become model context.
                    agent::AgentExecutionMatch::Stale => return,
                },
                // A manual command is never silently promoted to Agent
                // context. The user explicitly attaches a selected Block.
                None => return,
            }
        };
        let Some(proposal_id) = proposal_id else {
            self.show_toast("Agent stopped because command completion correlation failed.");
            self.agent_append_activity(
                "Safety check",
                "Agent stopped because command completion correlation failed.",
            );
            return;
        };

        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.observe(proposal_id, exit_code, &output)
        };
        if let Err(error) = result {
            self.report_agent_error("observe command output", &error);
            self.refresh_agent_panel();
            return;
        }
        self.refresh_agent_panel();
        if self.agent_is_awaiting_model() {
            self.agent_kick_llm(sender);
        }
    }

    pub(crate) fn agent_execution_start_failed(&self, execution: agent::AgentExecutionRef) {
        let failed = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.execution_start_failed(execution)
        };
        if failed {
            self.show_toast("Agent stopped because the target prompt was no longer ready.");
            self.agent_append_activity(
                "Safety check",
                "Agent stopped because the target prompt was no longer ready.",
            );
            self.refresh_agent_panel();
        }
    }

    pub(crate) fn agent_handle_reply(
        &self,
        epoch: agent::AgentSessionEpoch,
        reply: Result<String, String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        let (result, activity) = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            let before = session.transcript().len();
            let result = session.apply_llm_reply(epoch, reply);
            let activity = if should_publish_reply_activity(&result) {
                session.transcript()[before..].to_vec()
            } else {
                Vec::new()
            };
            (result, activity)
        };
        for turn in activity {
            match turn {
                agent::Turn::AssistantThought(message) => {
                    self.agent_append_activity("Agent (thought)", &message)
                }
                agent::Turn::AssistantSay(message) => self.agent_append_activity("Agent", &message),
                agent::Turn::ProtocolError(message) => {
                    self.agent_append_activity("Protocol error", &message)
                }
                agent::Turn::User(_)
                | agent::Turn::AssistantProposed { .. }
                | agent::Turn::Observation { .. } => {}
            }
        }
        match result {
            Ok(false) => return,
            Ok(true) => {}
            Err(error) => {
                // Protocol and provider failures are already recorded as an
                // explicit ProtocolError turn; the toast is only a concise cue.
                self.report_agent_error("model reply", &error);
            }
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_kick_llm(&self, sender: &ComponentSender<AppModel>) {
        let client = match ai::client_from_config(&self.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                let result = {
                    let mut guard = self.active_agent.borrow_mut();
                    guard.as_mut().and_then(|session| {
                        (session.state() == agent::AgentState::AwaitingModel)
                            .then(|| session.model_failed(error.clone()))
                    })
                };
                if let Some(Err(state_error)) = result {
                    self.report_agent_error("record provider failure", &state_error);
                }
                log::warn!("agent: {error}");
                self.show_toast(format!("AI provider is unavailable: {error}"));
                self.agent_append_activity("Error", &error);
                self.refresh_agent_panel();
                return;
            }
        };

        // Build the prompt only for the single legal request state. This
        // guards direct AppMsg injection as well as disabled panel controls.
        let (request_epoch, bound_tab, bound_pane, system, user, cancellation) = {
            let guard = self.active_agent.borrow();
            let Some(session) = guard.as_ref() else {
                return;
            };
            if session.state() != agent::AgentState::AwaitingModel
                || session.in_flight.is_some()
                || session.is_cancelled()
            {
                return;
            }
            let cwd = self
                .tabs
                .iter()
                .find(|tab| tab.id == session.bound_tab)
                .and_then(|tab| tab.panes.iter().find(|pane| pane.id == session.bound_pane))
                .and_then(|pane| pane.cwd.as_deref())
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
                .unwrap_or(".");
            let shell = self
                .shell_argv
                .first()
                .map(String::as_str)
                .unwrap_or("/bin/sh");
            // Cached repo probe with a bounded UI wait; None outside a repo.
            let git = jterm_core::git_meta::read(std::path::Path::new(cwd));
            (
                session.epoch(),
                session.bound_tab,
                session.bound_pane,
                ai::build_agent_system_prompt(),
                ai::agent_user_prompt(
                    &session.build_user_prompt(),
                    cwd,
                    shell,
                    std::env::consts::OS,
                    git.as_ref(),
                    session.last_manual_completed.as_ref(),
                ),
                session.cancellation_token(),
            )
        };

        let sender_for_reply = sender.clone();
        let callback_cancellation = cancellation.clone();
        let handle = ai::ask(client, system, user, move |result| {
            if !callback_cancellation.is_cancelled() {
                sender_for_reply.input(AppMsg::AgentLlmReply {
                    epoch: request_epoch,
                    reply: result,
                });
            }
        });

        let mut handle = Some(handle);
        {
            let mut guard = self.active_agent.borrow_mut();
            if let Some(session) = guard.as_mut() {
                if session.epoch() == request_epoch
                    && session.bound_tab == bound_tab
                    && session.bound_pane == bound_pane
                    && session.state() == agent::AgentState::AwaitingModel
                    && session.in_flight.is_none()
                    && !session.is_cancelled()
                {
                    session.in_flight = handle.take();
                }
            }
        }
        // A session cannot normally change synchronously while `ask` starts,
        // but cancelling here keeps the invariant robust if that changes.
        if let Some(orphaned) = handle {
            orphaned.cancel();
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_close(&self) {
        self.agent_panel_generation
            .set(self.agent_panel_generation.get().wrapping_add(1));
        let previous = self.active_agent.borrow_mut().take();
        if let Some(mut previous) = previous {
            self.organism_hub
                .agent_signal()
                .note_phase(crate::organism::AgentPulse::Gone);
            let target = (previous.bound_tab, previous.bound_pane);
            previous.cancel();
            let card: gtk::Widget = self.agent_panel.widget().clone().upcast();
            if let Some(terminal) = self.terminal_for(target.0, target.1) {
                terminal.remove_inline_notice(&card);
            }
        }
        self.agent_panel.emit(agent::AgentPanelMsg::Reset);
        self.sync_agent_toggle();
    }

    /// Small dashboard for Agent identity and safety-adjacent controls. The
    /// general Settings dialog remains available, but session activity never
    /// leaves the inline card/Block flow merely to inspect configuration.
    pub(crate) fn open_agent_settings(&self, sender: &ComponentSender<AppModel>) {
        let Some((tab_id, pane_id)) = self
            .active_agent
            .borrow()
            .as_ref()
            .map(|session| (session.bound_tab, session.bound_pane))
        else {
            self.show_toast("Open Shell Agent before viewing its dashboard.");
            return;
        };
        let cwd = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.panes.iter().find(|pane| pane.id == pane_id))
            .and_then(|pane| pane.cwd.clone())
            .unwrap_or_else(|| ".".to_string());
        let shell = self
            .shell_argv
            .first()
            .cloned()
            .unwrap_or_else(|| "sh".to_string());
        let (provider, model, correction_enabled) = {
            let config = self.config.borrow();
            (
                config.ai_provider.clone(),
                config.ai_model.clone(),
                config.command_correction_enabled,
            )
        };

        let dialog = adw::Dialog::builder()
            .title("Shell Agent settings")
            .content_width(620)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
        body.add_css_class("agent-dashboard");
        body.set_margin_start(12);
        body.set_margin_end(12);
        body.set_margin_top(10);
        body.set_margin_bottom(12);

        let overview = gtk::Box::new(gtk::Orientation::Vertical, 8);
        overview.add_css_class("agent-overview");
        let identity = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let icon = gtk::Image::from_icon_name("system-run-symbolic");
        icon.set_pixel_size(32);
        let identity_copy = gtk::Box::new(gtk::Orientation::Vertical, 2);
        identity_copy.set_hexpand(true);
        let title = gtk::Label::new(Some("Approval-gated shell assistant"));
        title.set_xalign(0.0);
        title.add_css_class("title-3");
        let safe_cwd = crate::review_input::safe_inline_display(&cwd, 4 * 1024);
        let target = gtk::Label::new(Some(&format!("Bound to Block pane · {safe_cwd}")));
        target.set_xalign(0.0);
        target.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        target.set_tooltip_text(Some(&safe_cwd));
        target.add_css_class("dim-label");
        identity_copy.append(&title);
        identity_copy.append(&target);
        identity.append(&icon);
        identity.append(&identity_copy);
        overview.append(&identity);

        let chips = gtk::FlowBox::new();
        chips.set_selection_mode(gtk::SelectionMode::None);
        chips.set_homogeneous(false);
        chips.set_row_spacing(6);
        chips.set_column_spacing(6);
        chips.set_min_children_per_line(1);
        chips.set_max_children_per_line(3);
        for (index, text) in [
            format!("{provider} · {model}"),
            format!("shell: {shell}"),
            "Review required".to_string(),
        ]
        .into_iter()
        .enumerate()
        {
            let text = crate::review_input::safe_inline_display(&text, 1024);
            let chip = gtk::Label::new(Some(&text));
            chip.add_css_class("agent-chip");
            if index == 2 {
                chip.add_css_class("agent-safety-chip");
            }
            chip.set_tooltip_text(Some(&text));
            chips.append(&chip);
        }
        overview.append(&chips);
        body.append(&overview);

        let auto_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        auto_row.add_css_class("agent-setting-card");
        let auto_copy = gtk::Box::new(gtk::Orientation::Vertical, 2);
        auto_copy.set_hexpand(true);
        let auto_title = gtk::Label::new(Some("Automatic command execution retired"));
        auto_title.set_xalign(0.0);
        auto_title.add_css_class("heading");
        let auto_hint = gtk::Label::new(Some(
            "Every proposal requires explicit approval; aliases, functions and tool flags make string-only auto-approval unsafe.",
        ));
        auto_hint.set_xalign(0.0);
        auto_hint.set_wrap(true);
        auto_hint.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        auto_hint.add_css_class("dim-label");
        auto_copy.append(&auto_title);
        auto_copy.append(&auto_hint);
        let auto_switch = gtk::Switch::builder()
            .active(false)
            .sensitive(false)
            .valign(gtk::Align::Center)
            .build();
        auto_switch.update_property(&[gtk::accessible::Property::Label(
            "Automatic command execution (retired and off)",
        )]);
        auto_switch.set_tooltip_text(Some("Automatic execution is disabled for safety"));
        auto_row.append(&auto_copy);
        auto_row.append(&auto_switch);
        body.append(&auto_row);

        let correction_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        correction_row.add_css_class("agent-setting-card");
        let correction_copy = gtk::Box::new(gtk::Orientation::Vertical, 2);
        correction_copy.set_hexpand(true);
        let correction_title = gtk::Label::new(Some("AI command correction"));
        correction_title.set_xalign(0.0);
        correction_title.add_css_class("heading");
        let correction_hint = gtk::Label::new(Some(
            "After typo-like failures, offer an editable correction; never insert or run it automatically.",
        ));
        correction_hint.set_xalign(0.0);
        correction_hint.set_wrap(true);
        correction_hint.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        correction_hint.add_css_class("dim-label");
        correction_copy.append(&correction_title);
        correction_copy.append(&correction_hint);
        let correction_switch = gtk::Switch::builder()
            .active(correction_enabled)
            .valign(gtk::Align::Center)
            .build();
        correction_switch
            .update_property(&[gtk::accessible::Property::Label("AI command correction")]);
        correction_switch.set_tooltip_text(Some("Enable review-first command correction"));
        correction_row.append(&correction_copy);
        correction_row.append(&correction_switch);
        body.append(&correction_row);

        let full_settings = gtk::Button::with_label("Open full AI settings");
        full_settings.set_halign(gtk::Align::End);
        full_settings.add_css_class("flat");
        body.append(&full_settings);

        {
            let sender = sender.clone();
            correction_switch.connect_active_notify(move |toggle| {
                sender.input(AppMsg::SettingsCommandCorrection(toggle.is_active()));
            });
        }
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            full_settings.connect_clicked(move |_| {
                dialog.close();
                sender.input(AppMsg::Action(Action::ToggleSettings));
            });
        }

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .vexpand(true)
            .child(&body)
            .build();
        toolbar.set_content(Some(&scroll));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(&self.window));
    }

    pub(crate) fn sync_agent_toggle(&self) {
        let cfg = self.config.borrow();
        let available = !self.safe_mode && cfg.ai_enabled && cfg.agent_enabled;
        drop(cfg);
        self.top_bar.emit(top_bar::TopBarMsg::SetAgentState {
            available,
            active: available && self.active_agent.borrow().is_some(),
        });
    }

    fn agent_is_awaiting_model(&self) -> bool {
        self.active_agent
            .borrow()
            .as_ref()
            .is_some_and(|session| session.state() == agent::AgentState::AwaitingModel)
    }

    fn report_agent_error(&self, operation: &str, error: &agent::SessionError) {
        log::warn!("agent: failed to {operation}: {error}");
        self.show_toast(format!("Agent: {error}"));
    }

    pub(crate) fn terminal_for(&self, tab_id: u64, pane_id: u64) -> Option<&TermCtl> {
        self.tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.panes.iter().find(|pane| pane.id == pane_id))
            .map(|pane| &pane.terminal)
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn snapshot_fixture() -> AgentSessionSnapshot {
        let mut session = jterm_core::agent::AgentSession::new(4);
        session.submit_user("persist this session").unwrap();
        session.snapshot().expect("non-empty session snapshots")
    }

    #[test]
    fn current_protocol_error_is_published_but_stale_reply_is_not() {
        let mut session = agent::AgentSession::new(1, 2, 4);
        session.submit_user("test protocol handling").unwrap();
        let current = session.apply_llm_reply(session.epoch(), Ok("not json".to_string()));
        assert!(current.is_err());
        assert!(should_publish_reply_activity(&current));

        let stale = Ok(false);
        assert!(!should_publish_reply_activity(&stale));
    }

    fn test_directory(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "anvil-agent-snapshot-{label}-{}-{}",
            std::process::id(),
            relm4::gtk::glib::uuid_string_random()
        ));
        std::fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }

    #[test]
    fn local_snapshot_io_round_trips_and_enforces_the_exact_budget() {
        let root = test_directory("roundtrip");
        let path = root.join("agent_session.json");
        write_agent_snapshot(&path, &snapshot_fixture()).unwrap();
        let restored = read_agent_snapshot(&path).expect("snapshot should round trip");
        assert!(jterm_core::agent::AgentSession::restore(restored).is_ok());

        let oversized = root.join("oversized.json");
        std::fs::write(&oversized, vec![b'x'; MAX_AGENT_SNAPSHOT_JSON_BYTES + 1]).unwrap();
        assert!(read_agent_snapshot(&oversized).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_shot_restore_is_consumed_by_only_one_concurrent_process() {
        let root = test_directory("one-shot");
        let path = root.join("agent_session.json");
        write_agent_snapshot(&path, &snapshot_fixture()).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                restore_agent_snapshot_once(&path).is_some()
            }));
        }
        barrier.wait();
        let restored = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|restored| *restored)
            .count();
        assert_eq!(restored, 1);
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_shot_restore_fails_closed_when_the_namespace_cannot_be_synced() {
        let root = test_directory("sync-failure");
        let path = root.join("agent_session.json");
        write_agent_snapshot(&path, &snapshot_fixture()).unwrap();
        let sync_called = std::cell::Cell::new(false);

        let restored = restore_agent_snapshot_once_with_sync(&path, |_| {
            sync_called.set(true);
            Err(crate::config_store::ConfigWriteError::Io(
                "injected directory sync failure".to_string(),
            ))
        });

        assert!(sync_called.get(), "a successful claim must sync its parent");
        assert!(restored.is_none(), "an undurable claim must fail closed");
        assert!(!path.exists(), "the claimed public name remains consumed");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn claim_error_keeps_the_public_path_and_does_not_sync() {
        let root = test_directory("claim-error");
        let path = root.join("agent_session.json");
        std::fs::create_dir(&path).unwrap();
        let sync_called = std::cell::Cell::new(false);

        assert!(restore_agent_snapshot_once_with_sync(&path, |_| {
            sync_called.set(true);
            Ok(())
        })
        .is_none());
        assert!(
            !sync_called.get(),
            "a failed claim did not mutate the namespace"
        );
        assert!(
            path.is_dir(),
            "claim errors must retain the public evidence"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_one_shot_restore_is_quarantined_for_inspection() {
        let root = test_directory("quarantine");
        let path = root.join("agent_session.json");
        let evidence = "not an Agent snapshot";
        std::fs::write(&path, evidence).unwrap();

        assert!(restore_agent_snapshot_once(&path).is_none());
        assert!(!path.exists(), "the invalid snapshot name must be claimed");

        let quarantined = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|candidate| {
                candidate != &path
                    && std::fs::read_to_string(candidate).is_ok_and(|contents| contents == evidence)
            })
            .expect("invalid evidence should remain under a quarantine name");
        assert_ne!(quarantined, path);
        assert!(restore_agent_snapshot_once(&path).is_none());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn local_snapshot_io_rejects_links_fifo_and_the_legacy_stage() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let root = test_directory("unsafe");
        let path = root.join("agent_session.json");
        let victim = root.join("victim.json");
        let legacy_stage = root.join(format!(".agent_session.json.next.{}", std::process::id()));
        std::fs::write(&victim, b"sentinel").unwrap();
        symlink(&victim, &legacy_stage).unwrap();

        write_agent_snapshot(&path, &snapshot_fixture()).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"sentinel");
        assert!(std::fs::symlink_metadata(&legacy_stage)
            .unwrap()
            .file_type()
            .is_symlink());

        let linked = root.join("linked.json");
        symlink(&path, &linked).unwrap();
        assert!(read_agent_snapshot(&linked).is_none());

        let hard_linked = root.join("hard-linked.json");
        std::fs::hard_link(&path, &hard_linked).unwrap();
        assert!(read_agent_snapshot(&hard_linked).is_none());

        let fifo = root.join("fifo.json");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_name is NUL-terminated and remains live for this call.
        assert_eq!(unsafe { nix::libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(read_agent_snapshot(&fifo).is_none());
        assert!(started.elapsed() < Duration::from_secs(1));

        std::fs::remove_dir_all(root).unwrap();
    }
}
