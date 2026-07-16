//! AI Agent session orchestration for the Relm4 application model.
//!
//! The pure state machine in `agent` owns every protocol transition. This
//! integration layer only snapshots views, starts provider requests, and
//! performs a command after an explicit approval token is returned.

use super::*;

impl AppModel {
    // ── Agent mode ───────────────────────────────────────────────────────

    pub(crate) fn open_agent_panel(&self, _sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("AI Agent is unavailable in safe mode.");
            return;
        }
        let cfg = self.config.borrow();
        if !cfg.ai_enabled || !cfg.agent_enabled {
            log::info!(
                "agent: disabled (ai_enabled={}, agent_enabled={})",
                cfg.ai_enabled,
                cfg.agent_enabled
            );
            self.show_toast("AI Agent is disabled in configuration.");
            return;
        }
        let max_turns = cfg.agent_max_turns;
        let client = match ai::AiClient::from_config(&cfg) {
            Ok(client) => client,
            Err(error) => {
                log::warn!("agent: {error}");
                self.show_toast(format!("AI provider is unavailable: {error}"));
                return;
            }
        };
        drop(cfg);

        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(pane) = tab.panes.get(tab.active_pane) else {
            return;
        };
        if !matches!(pane.mode, TerminalMode::Block) {
            self.show_toast(
                "AI Agent requires a Block-mode pane so command results can be observed.",
            );
            return;
        }
        let (tab_id, pane_id) = (tab.id, pane.id);

        // Replacing a session invalidates both its provider callback and any
        // late BlockFinished event before the new identity becomes visible.
        if let Some(mut previous) = self.active_agent.borrow_mut().take() {
            previous.cancel();
        }
        *self.active_agent.borrow_mut() =
            Some(agent::AgentSession::new(tab_id, pane_id, max_turns));

        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Open {
                provider_name: client.display_name(),
                view,
            });
        }
    }

    pub(crate) fn agent_panel_view(&self) -> Option<agent::AgentPanelView> {
        let session = self.active_agent.borrow();
        let session = session.as_ref()?;
        Some(agent::AgentPanelView {
            transcript: session.transcript().to_vec(),
            turns_used: session.turns_used(),
            max_turns: session.max_turns(),
            state: session.state(),
            loading: session.in_flight.is_some(),
        })
    }

    pub(crate) fn refresh_agent_panel(&self) {
        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Render(view));
        }
    }

    /// Submit one user turn. The state machine rejects concurrent sends while
    /// a model, approval, or command observation is outstanding.
    pub(crate) fn agent_send(&self, text: String, sender: &ComponentSender<AppModel>) {
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
        self.refresh_agent_panel();
        self.agent_kick_llm(sender);
    }

    pub(crate) fn agent_approve(
        &self,
        transcript_index: usize,
        edited: Option<String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        // The panel is index-based because it renders a transcript, but the
        // protocol is not: resolve the index to its stable id, then let the
        // state machine reject stale or already-consumed proposals.
        let (proposal_id, tab_id, pane_id) = {
            let guard = self.active_agent.borrow();
            let Some(session) = guard.as_ref() else {
                return;
            };
            let Some(proposal_id) = session.proposal_id_at(transcript_index) else {
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
        if !terminal.can_accept_agent_command() {
            self.show_toast(
                "Agent target prompt is busy or already contains input; clear it before approval.",
            );
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

        // Approval is the only path that produces bytes for a PTY. The
        // command is submitted because this event came from the explicit
        // Approve/Run action in the panel.
        let mut bytes = approved.command.into_bytes();
        bytes.push(b'\r');
        terminal.emit(VteInput::WriteInput(bytes));
        terminal.emit(VteInput::GrabFocus);
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_reject(&self, transcript_index: usize, sender: &ComponentSender<AppModel>) {
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            let Some(proposal_id) = session.proposal_id_at(transcript_index) else {
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
        self.refresh_agent_panel();
        if self.agent_is_awaiting_model() {
            self.agent_kick_llm(sender);
        }
    }

    pub(crate) fn agent_handle_block_finished(
        &self,
        tab_id: u64,
        pane_id: u64,
        command: String,
        exit_code: i32,
        output: String,
        sender: &ComponentSender<AppModel>,
    ) {
        let proposal_id = {
            let guard = self.active_agent.borrow();
            let Some(session) = guard.as_ref() else {
                return;
            };
            if session.bound_tab != tab_id || session.bound_pane != pane_id {
                return;
            }
            match session.awaiting_command.as_ref() {
                Some((proposal_id, expected)) if expected.trim() == command.trim() => *proposal_id,
                // A manual command completed while the Agent was waiting.
                // Never attach its output to the approved proposal.
                _ => return,
            }
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

    pub(crate) fn agent_handle_reply(
        &self,
        reply: Result<String, String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        let result = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            if session.is_cancelled() {
                return;
            }
            match reply {
                Ok(raw) => session.accept_model_reply(&raw).map(|_| ()),
                Err(error) => session.model_failed(error),
            }
        };
        if let Err(error) = result {
            // Protocol and provider failures are already recorded as an
            // explicit ProtocolError turn; the toast is only a concise cue.
            self.report_agent_error("model reply", &error);
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_kick_llm(&self, sender: &ComponentSender<AppModel>) {
        let client = match ai::AiClient::from_config(&self.config.borrow()) {
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
                self.refresh_agent_panel();
                return;
            }
        };

        // Build the prompt only for the single legal request state. This
        // guards direct AppMsg injection as well as disabled panel controls.
        let (bound_tab, bound_pane, system, user, cancellation) = {
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
            (
                session.bound_tab,
                session.bound_pane,
                ai::build_agent_system_prompt(cwd, shell, std::env::consts::OS),
                session.build_user_prompt(),
                session.cancellation_token(),
            )
        };

        let sender_for_reply = sender.clone();
        let callback_cancellation = cancellation.clone();
        let handle = ai::ask(client, system, user, move |result| {
            if !callback_cancellation.is_cancelled() {
                sender_for_reply.input(AppMsg::AgentLlmReply(result));
            }
        });

        let mut handle = Some(handle);
        {
            let mut guard = self.active_agent.borrow_mut();
            if let Some(session) = guard.as_mut() {
                if session.bound_tab == bound_tab
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
        self.agent_edit.emit(agent::AgentEditMsg::Close);
        if let Some(mut previous) = self.active_agent.borrow_mut().take() {
            previous.cancel();
        }
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
