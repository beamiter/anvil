//! AI Agent session orchestration for the Relm4 application model.
//!
//! The existing `AppModel`, `AppMsg`, Relm4 controllers, and update loop remain
//! authoritative. This module only groups the Agent-specific inherent methods.

use super::*;

impl AppModel {
    // ── Agent mode ───────────────────────────────────────────────────────

    pub(crate) fn open_agent_panel(&self, _sender: &ComponentSender<AppModel>) {
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
        drop(cfg);
        let active_is_block = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane))
            .is_some_and(|pane| matches!(pane.mode, TerminalMode::Block));
        if !active_is_block {
            self.show_toast(
                "AI Agent requires a Block-mode pane so command results can be observed.",
            );
            return;
        }
        let Some(client) = ai::AiClient::from_env() else {
            log::warn!("agent: no AI provider configured");
            self.show_toast("No AI provider is configured.");
            return;
        };

        // Cancel any pre-existing session before replacing.
        if let Some(prev) = self.active_agent.borrow_mut().take() {
            prev.cancel();
        }

        let tab_id = self.tabs.get(self.active).map(|t| t.id).unwrap_or(0);
        let pane_id = self
            .tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .map(|p| p.id)
            .unwrap_or(0);
        *self.active_agent.borrow_mut() = Some(agent::AgentSession::new(tab_id, pane_id));

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
            transcript: session.transcript.clone(),
            turns_used: session.turns_used,
            max_turns: self.config.borrow().agent_max_turns,
            awaiting_command: session.awaiting_command.is_some(),
            sealed: session.sealed,
            loading: session.in_flight.is_some(),
        })
    }

    pub(crate) fn refresh_agent_panel(&self) {
        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Render(view));
        }
    }

    /// Push a user turn and kick off the next LLM turn.
    pub(crate) fn agent_send(&self, text: String, sender: &ComponentSender<AppModel>) {
        if self.active_agent.borrow().is_none() {
            return;
        }
        {
            let mut guard = self.active_agent.borrow_mut();
            let sess = guard.as_mut().unwrap();
            if sess.sealed {
                return;
            }
            sess.transcript.push(agent::Turn::User(text));
        }
        self.refresh_agent_panel();
        self.agent_kick_llm(sender);
    }

    pub(crate) fn agent_approve(
        &self,
        idx: usize,
        edited: Option<String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        let (cmd, tab_id, pane_id) = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            if sess.sealed {
                return;
            }
            let final_cmd = match sess.transcript.get_mut(idx) {
                Some(agent::Turn::AssistantProposed { cmd, approved }) => {
                    if let Some(new_cmd) = edited {
                        *cmd = new_cmd;
                    }
                    *approved = Some(true);
                    cmd.clone()
                }
                _ => return,
            };
            sess.awaiting_command = Some(final_cmd.clone());
            (final_cmd, sess.bound_tab, sess.bound_pane)
        };
        // Type the command into the bound pane, autosubmit with \r since
        // the user has explicitly approved.
        if let Some(term) = self.terminal_for(tab_id, pane_id) {
            let mut bytes = cmd.into_bytes();
            bytes.push(b'\r');
            term.emit(VteInput::WriteInput(bytes));
            term.emit(VteInput::GrabFocus);
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_reject(&self, idx: usize, sender: &ComponentSender<AppModel>) {
        {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            if let Some(agent::Turn::AssistantProposed { approved, .. }) =
                sess.transcript.get_mut(idx)
            {
                *approved = Some(false);
            }
        }
        self.refresh_agent_panel();
        // Kick the LLM again so it can suggest something else.
        self.agent_kick_llm(sender);
    }

    pub(crate) fn agent_handle_block_finished(
        &self,
        tab_id: u64,
        pane_id: u64,
        command: String,
        exit_code: i32,
        output_sample: String,
        sender: &ComponentSender<AppModel>,
    ) {
        let should_feed = {
            let guard = self.active_agent.borrow();
            let Some(sess) = guard.as_ref() else { return };
            if sess.bound_tab != tab_id || sess.bound_pane != pane_id {
                return;
            }
            match sess.awaiting_command.as_ref() {
                Some(expected) if expected.trim() == command.trim() => true,
                // The user typed something themselves while the agent was
                // waiting — drop this block and keep waiting.
                _ => false,
            }
        };
        if !should_feed {
            return;
        }
        {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            sess.awaiting_command = None;
            sess.transcript.push(agent::Turn::Observation {
                exit: exit_code,
                output_sample: agent::sample_observation(&output_sample),
            });
        }
        self.refresh_agent_panel();
        self.agent_kick_llm(sender);
    }

    pub(crate) fn agent_handle_reply(
        &self,
        reply: Result<String, String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            if sess.is_cancelled() {
                return;
            }
            sess.in_flight = None;
            sess.turns_used = sess.turns_used.saturating_add(1);

            match reply {
                Err(e) => {
                    sess.transcript.push(agent::Turn::AssistantSay(format!(
                        "[error contacting model: {e}]"
                    )));
                }
                Ok(raw) => {
                    let parsed = agent::parse_action(&raw);
                    match parsed {
                        agent::ParsedAction::Run { thought, command } => {
                            if let Some(t) = thought {
                                sess.transcript.push(agent::Turn::AssistantThought(t));
                            }
                            sess.transcript.push(agent::Turn::AssistantProposed {
                                cmd: command,
                                approved: None,
                            });
                        }
                        agent::ParsedAction::Say { thought, message } => {
                            if let Some(t) = thought {
                                sess.transcript.push(agent::Turn::AssistantThought(t));
                            }
                            sess.transcript.push(agent::Turn::AssistantSay(message));
                        }
                        agent::ParsedAction::Done { thought, message } => {
                            if let Some(t) = thought {
                                sess.transcript.push(agent::Turn::AssistantThought(t));
                            }
                            sess.transcript.push(agent::Turn::AssistantSay(message));
                            sess.sealed = true;
                        }
                    }
                }
            }
            // Turn-cap seal.
            let cap = self.config.borrow().agent_max_turns;
            if sess.turns_used >= cap {
                sess.sealed = true;
            }
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_kick_llm(&self, sender: &ComponentSender<AppModel>) {
        let Some(client) = ai::AiClient::from_env() else {
            return;
        };
        // Build the prompt outside the borrow.
        let (system, user) = {
            let guard = self.active_agent.borrow();
            let Some(sess) = guard.as_ref() else { return };
            if sess.sealed {
                return;
            }
            // Don't double-fire while still waiting for a command's output.
            if sess.awaiting_command.is_some() {
                return;
            }
            let cwd = self
                .active_cwd()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string());
            let shell = self
                .shell_argv
                .first()
                .cloned()
                .unwrap_or_else(|| "/bin/sh".to_string());
            let os = std::env::consts::OS.to_string();
            (
                ai::build_agent_system_prompt(&cwd, &shell, &os),
                sess.build_user_prompt(),
            )
        };

        let sender_for_reply = sender.clone();
        let cancelled = {
            let guard = self.active_agent.borrow();
            guard.as_ref().map(|s| s.cancelled.clone())
        };
        let handle = ai::ask(client, system, user, move |result| {
            // Cancelled-check is already done by ask() against its own flag,
            // but the agent session may have moved on between fire and
            // delivery — re-check here.
            if let Some(c) = &cancelled {
                if c.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
            }
            sender_for_reply.input(AppMsg::AgentLlmReply(result));
        });
        // Stash the handle on the business session; the panel derives its
        // spinner state from `in_flight` through a fresh view snapshot.
        {
            let mut guard = self.active_agent.borrow_mut();
            if let Some(sess) = guard.as_mut() {
                sess.in_flight = Some(handle);
            }
        }
        self.refresh_agent_panel();
    }

    pub(crate) fn agent_close(&self) {
        self.agent_edit.emit(agent::AgentEditMsg::Close);
        if let Some(prev) = self.active_agent.borrow_mut().take() {
            prev.cancel();
        }
    }

    pub(crate) fn terminal_for(&self, tab_id: u64, pane_id: u64) -> Option<&TermCtl> {
        self.tabs
            .iter()
            .find(|t| t.id == tab_id)
            .and_then(|t| t.panes.iter().find(|p| p.id == pane_id))
            .map(|p| &p.terminal)
    }
}
