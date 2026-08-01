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
    pub(crate) agent_generation: Option<u64>,
}

/// Read an Agent snapshot through jterm1's descriptor-validated persistence
/// path. Unsafe, oversized, corrupt, and missing entries all fail closed to a
/// fresh session.
fn read_agent_snapshot_unlocked(path: &Path) -> Option<AgentSessionSnapshot> {
    let bytes = crate::config_store::read_private_bytes(path, MAX_AGENT_SNAPSHOT_JSON_BYTES as u64)
        .ok()??;
    let encoded = std::str::from_utf8(&bytes).ok()?;
    AgentSessionSnapshot::from_json(encoded).ok()
}

#[cfg(test)]
fn read_agent_snapshot(path: &Path) -> Option<AgentSessionSnapshot> {
    let _parent_lock = crate::config_store::PrivateParentLock::acquire(path).ok()?;
    read_agent_snapshot_unlocked(path)
}

/// Validate, restore, and consume exactly once while holding the directory
/// namespace lock. Multiple NON_UNIQUE jterm1 processes can open concurrently;
/// only the process that removes the pathname may receive this session.
fn restore_agent_snapshot_once(path: &Path) -> Option<jterm_core::agent::AgentSession> {
    let _parent_lock = match crate::config_store::PrivateParentLock::acquire(path) {
        Ok(lock) => lock,
        Err(error) => {
            log::warn!("agent: could not lock snapshot namespace: {error}");
            return None;
        }
    };
    let snapshot = read_agent_snapshot_unlocked(path)?;
    let restored = match jterm_core::agent::AgentSession::restore(snapshot) {
        Ok(restored) => restored,
        Err(error) => {
            log::warn!(
                "agent: invalid snapshot {} retained for inspection: {error}",
                path.display()
            );
            return None;
        }
    };
    if let Err(error) = std::fs::remove_file(path) {
        log::warn!(
            "agent: restored snapshot {} but could not consume it: {error}",
            path.display()
        );
        return None;
    }
    if let Err(error) = crate::config_store::sync_config_parent(path) {
        log::warn!(
            "agent: snapshot removal for {} was not durable: {error}",
            path.display()
        );
        return None;
    }
    Some(restored)
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
        let client = match ai::client_from_config(&cfg) {
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
        // A snapshot persisted by the previous run is restored one-shot and
        // rebound to the pane the user reopened the Agent on.
        let snapshot_file = Self::agent_snapshot_path();
        let restored = restore_agent_snapshot_once(&snapshot_file);
        let session = match restored {
            Some(inner) => match agent::AgentSession::from_restored(inner, tab_id, pane_id) {
                Some(session) => {
                    self.show_toast("Restored the previous agent session.");
                    session
                }
                None => {
                    log::warn!(
                        "agent: discarded a restored session containing unsafe command text"
                    );
                    self.show_toast("Discarded an unsafe saved Agent session.");
                    agent::AgentSession::new(tab_id, pane_id, max_turns)
                }
            },
            None => agent::AgentSession::new(tab_id, pane_id, max_turns),
        };
        *self.active_agent.borrow_mut() = Some(session);

        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Open {
                provider_name: client.display_name(),
                view,
            });
        }
        // A restored session may have died mid-request; resume it.
        if self.agent_is_awaiting_model() {
            self.agent_kick_llm(_sender);
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
            attached_context: session
                .last_manual_completed
                .as_ref()
                .map(|context| context.cmd.clone()),
        })
    }

    pub(crate) fn refresh_agent_panel(&self) {
        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Render(view));
        }
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
        }
        self.refresh_agent_panel();
    }

    /// Detach the remembered manual command from future model requests.
    pub(crate) fn agent_clear_context(&self) {
        {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            session.last_manual_completed = None;
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

        // The backend re-checks prompt cleanliness, arms the generation and
        // writes the bytes as one UI-thread operation. A queued failure event
        // seals the already-approved protocol state instead of accepting an
        // unrelated completion later.
        terminal.emit(VteInput::RunAgentCommand {
            generation: pending.generation,
            command: pending.command,
        });
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
        completion: AgentBlockCompletion,
        sender: &ComponentSender<AppModel>,
    ) {
        let AgentBlockCompletion {
            tab_id,
            pane_id,
            command,
            exit_code,
            output,
            agent_generation,
        } = completion;
        let proposal_id = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            if session.bound_tab != tab_id || session.bound_pane != pane_id {
                return;
            }
            match session.awaiting_command.as_ref() {
                Some(pending)
                    if agent_generation == Some(pending.generation)
                        && pending.command.trim() == command.trim() =>
                {
                    pending.proposal_id
                }
                Some(pending) if agent_generation == Some(pending.generation) => {
                    let generation = pending.generation;
                    session.execution_start_failed(generation);
                    self.show_toast("Agent stopped because command completion correlation failed.");
                    return;
                }
                // A stale/internal generation can never become model context.
                _ if agent_generation.is_some() => return,
                // A manual command completed in the bound pane. Never attach
                // its output to the approved proposal — remember it instead
                // as untrusted block context for the next model request.
                _ => {
                    let command = command.trim();
                    if !command.is_empty() {
                        session.last_manual_completed = Some(ai::BlockContext {
                            cmd: command.to_string(),
                            output: output.clone(),
                            cwd: None,
                            exit_code,
                            truncated: output.contains("elided"),
                        });
                    }
                    return;
                }
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

    pub(crate) fn agent_execution_start_failed(&self, generation: u64) {
        let failed = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(session) = guard.as_mut() else {
                return;
            };
            let matches = session
                .awaiting_command
                .as_ref()
                .is_some_and(|pending| pending.generation == generation);
            if matches {
                session.execution_start_failed(generation);
            }
            matches
        };
        if failed {
            self.show_toast("Agent stopped because the target prompt was no longer ready.");
            self.refresh_agent_panel();
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
            // Cached repo probe with a bounded UI wait; None outside a repo.
            let git = jterm_core::git_meta::read(std::path::Path::new(cwd));
            (
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn snapshot_fixture() -> AgentSessionSnapshot {
        let mut session = jterm_core::agent::AgentSession::new(4);
        session.submit_user("persist this session").unwrap();
        session.snapshot().expect("non-empty session snapshots")
    }

    fn test_directory(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "jterm1-agent-snapshot-{label}-{}-{}",
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
