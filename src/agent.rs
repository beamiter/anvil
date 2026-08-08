//! Agent mode — multi-turn LLM that proposes shell commands, watches their
//! output, and iterates. Inspired by Warp 2.x's agent.
//!
//! ## Safety model (immutable, by design)
//!
//! 1. **Per-command approval.** No "yes to all" / "yolo mode". Every command
//!    the model proposes shows up as an *Approve & Run* card that the user
//!    must click — including obvious commands like `ls`. This is the price
//!    of letting an LLM touch a real terminal. Users who want unattended
//!    execution can write a shell script.
//! 2. **Dangerous-command flagging.** A small regex blacklist (rm -rf /,
//!    mkfs.*, dd of=/dev/*, fork bomb, curl|sh) flips the Approve button
//!    to a destructive style and prefixes a `⚠ destructive` chip. Users
//!    can still approve — we just slow them down with a colour change so
//!    a stray Enter doesn't nuke their disk.
//! 3. **Single concurrent session.** AppModel holds at most one
//!    `AgentSession` at a time. Opening a second panel closes the first.
//! 4. **Turn cap.** `agent_max_turns` (default 20) bounds runaway loops.
//! 5. **Transcript byte cap.** Before sending to the LLM, the transcript is
//!    head+tail elided to `MAX_TRANSCRIPT_BYTES` so a chatty session can't
//!    OOM the prompt.
//! 6. **Output sample bound.** Each observation feeds the model at most
//!    `MAX_OBSERVATION_BYTES` of captured output (head+tail).
//! 7. **Cancel on close.** Closing the dialog calls `AgentSession::cancel`,
//!    which both flips the cancelled flag (suppressing pending LLM
//!    callbacks) and clears `awaiting_command`. Already-queued replies and
//!    terminal events also carry the task epoch, so they cannot attach to a
//!    replacement session.

use adw::prelude::*;
use relm4::adw;
use relm4::gtk;
use relm4::prelude::*;
use std::rc::Rc;

pub(crate) use jterm_core::agent::{
    is_dangerous, AgentSessionEpoch, AgentState, ApprovedCommand, CancellationToken, ModelOutcome,
    ProposalId, ProposalStatus, SessionError, Turn,
};
use jterm_core::agent::{parse_action, ParseError, ParsedAction};

use jterm_core::agent::AgentSession as CoreSession;

const MAX_LOCAL_AGENT_COMMAND_BYTES: usize = 16 * 1024;
const MAX_AGENT_DISPLAY_BYTES: usize = 32 * 1024;
/// App-level ceiling for one raw model reply, applied before parsing and
/// before any transcript mutation. The protocol layer bounds each decoded
/// field, but the raw reply arrives from the provider bounded only by the
/// transport; a reply this large is a malfunctioning provider, not an action.
const MAX_AGENT_MODEL_REPLY_BYTES: usize = 128 * 1024;

fn agent_display_text(text: &str, preserve_multiline: bool) -> String {
    crate::text_safety::bounded_display_text(text, MAX_AGENT_DISPLAY_BYTES, preserve_multiline)
}

pub(crate) fn local_agent_command_issue(command: &str) -> Option<&'static str> {
    if command.trim().is_empty() {
        return Some("command is empty");
    }
    if command.len() > MAX_LOCAL_AGENT_COMMAND_BYTES {
        return Some("command exceeds the local Agent size limit");
    }
    if command.chars().any(char::is_control) {
        return Some("command contains a control character");
    }
    if crate::text_safety::contains_visual_spoof(command) {
        return Some("command contains an invisible or bidirectional formatting character");
    }
    None
}

fn local_agent_command_error(command: &str) -> Option<SessionError> {
    local_agent_command_issue(command)
        .map(|issue| SessionError::Protocol(ParseError::InvalidCommand(issue.to_string())))
}

/// A UI action's binding to one proposal of one session generation.
///
/// A transcript index alone identifies a *row*, and rows move: New Task,
/// a restore, or a replacement session all renumber them while a click,
/// an edit dialog, or a queued message is still in flight. The epoch makes a
/// stale action detectable instead of letting it land on whatever proposal now
/// occupies that row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AgentProposalRef {
    pub(crate) epoch: AgentSessionEpoch,
    pub(crate) id: ProposalId,
}

/// One terminal execution belonging to one Agent task generation.
///
/// Generations restart when an Agent session is replaced. The epoch therefore
/// remains part of the identity at every asynchronous terminal boundary; a
/// generation on its own is never sufficient authority to complete or cancel
/// the current session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AgentExecutionRef {
    pub(crate) epoch: AgentSessionEpoch,
    pub(crate) generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingAgentCommand {
    pub(crate) proposal_id: ProposalId,
    pub(crate) command: String,
    /// Locally generated one-shot execution identity. It never comes from PTY
    /// output and must be armed before the approved bytes are written.
    pub(crate) execution: AgentExecutionRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentExecutionMatch {
    Matched(ProposalId),
    CommandMismatch,
    Stale,
}

/// anvil's Agent session. The pure protocol state machine (turn caps,
/// approval transitions, transcript bounds, prompt assembly) lives in
/// `jterm_core::agent`; this wrapper adds what is anvil-specific: the
/// tab/pane binding, the approved-command correlation slot, and ownership
/// of the in-flight LLM request handle.
pub(crate) struct AgentSession {
    inner: CoreSession,
    /// The approved proposal currently executing in the bound pane. Command
    /// text is only a secondary check; the locally armed epoch + generation
    /// pair is the authoritative correlation identity.
    pub(crate) awaiting_command: Option<PendingAgentCommand>,
    next_execution_generation: u64,
    /// Held so dropping the session cancels an in-flight LLM request.
    pub(crate) in_flight: Option<crate::ai::AiHandle>,
    /// Tab + pane the session is bound to. Commands are typed into this
    /// pane only; a BlockFinished from a different pane is ignored even
    /// if the command text matches.
    pub(crate) bound_tab: u64,
    pub(crate) bound_pane: u64,
    /// Most recent command the user ran manually in the bound pane while the
    /// session was active. Attached to model requests as untrusted block
    /// context so "why did that fail?" has something to look at.
    pub(crate) last_manual_completed: Option<crate::ai::BlockContext>,
}

impl AgentSession {
    pub(crate) fn new(bound_tab: u64, bound_pane: u64, max_turns: u32) -> Self {
        Self::wrap(CoreSession::new(max_turns), bound_tab, bound_pane)
    }

    /// Rebind a session restored from a cross-restart snapshot to the pane
    /// the user reopened the Agent on.
    pub(crate) fn from_restored(
        inner: CoreSession,
        bound_tab: u64,
        bound_pane: u64,
    ) -> Option<Self> {
        let safe = inner.transcript().iter().all(|turn| match turn {
            Turn::AssistantProposed { command, .. } => local_agent_command_issue(command).is_none(),
            _ => true,
        });
        safe.then(|| Self::wrap(inner, bound_tab, bound_pane))
    }

    fn wrap(inner: CoreSession, bound_tab: u64, bound_pane: u64) -> Self {
        Self {
            inner,
            awaiting_command: None,
            next_execution_generation: 0,
            in_flight: None,
            bound_tab,
            bound_pane,
            last_manual_completed: None,
        }
    }

    pub(crate) fn transcript(&self) -> &[Turn] {
        self.inner.transcript()
    }

    pub(crate) fn state(&self) -> AgentState {
        self.inner.state()
    }

    pub(crate) fn turns_used(&self) -> u32 {
        self.inner.turns_used()
    }

    pub(crate) fn max_turns(&self) -> u32 {
        self.inner.max_turns()
    }

    pub(crate) fn is_sealed(&self) -> bool {
        matches!(
            self.inner.state(),
            AgentState::Completed | AgentState::Cancelled | AgentState::TurnLimitReached
        )
    }

    pub(crate) fn can_submit(&self) -> bool {
        self.inner.state() == AgentState::Ready && self.inner.turns_used() < self.inner.max_turns()
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancellation_token()
    }

    pub(crate) fn epoch(&self) -> AgentSessionEpoch {
        self.inner.epoch()
    }

    /// Resolve a UI action to its proposal, refusing one raised against an
    /// earlier task generation or a session that has since been replaced.
    pub(crate) fn resolve_proposal(&self, reference: AgentProposalRef) -> Option<ProposalId> {
        (reference.epoch == self.inner.epoch()).then_some(reference.id)
    }

    pub(crate) fn submit_user(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.inner.submit_user(message)
    }

    pub(crate) fn accept_model_reply(&mut self, raw: &str) -> Result<ModelOutcome, SessionError> {
        self.in_flight = None;
        if raw.len() > MAX_AGENT_MODEL_REPLY_BYTES {
            // Recorded as a provider failure: no turn is consumed, nothing is
            // parsed, and the oversized bytes never reach the transcript.
            self.inner.model_failed(format!(
                "model reply of {} bytes exceeds the {MAX_AGENT_MODEL_REPLY_BYTES}-byte Agent limit",
                raw.len()
            ))?;
            return Err(SessionError::Protocol(ParseError::FieldTooLarge(
                "model reply",
            )));
        }
        if let Ok(ParsedAction::Run { command, .. }) = parse_action(raw) {
            if local_agent_command_issue(&command).is_some() {
                // Drive the old core through its normal protocol-error
                // transition so the unsafe proposal is never stored or shown.
                // The staged jagent release performs this validation itself.
                return self
                    .inner
                    .accept_model_reply(r#"{"action":"run","command":"\n"}"#);
            }
        }
        self.inner.accept_model_reply(raw)
    }

    /// Apply an asynchronous provider result only to the task that launched
    /// it. A callback can already be queued when New Task or replacement
    /// cancels its request, so cancellation alone is not an ownership check.
    pub(crate) fn apply_llm_reply(
        &mut self,
        epoch: AgentSessionEpoch,
        reply: Result<String, String>,
    ) -> Result<bool, SessionError> {
        if epoch != self.epoch() || self.is_cancelled() {
            return Ok(false);
        }
        match reply {
            Ok(raw) => self.accept_model_reply(&raw).map(|_| true),
            Err(error) => self.model_failed(error).map(|_| true),
        }
    }

    /// Record a provider/transport failure without consuming a model turn.
    pub(crate) fn model_failed(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.in_flight = None;
        self.inner.model_failed(message)
    }

    pub(crate) fn can_retry_model(&self) -> bool {
        self.inner.can_retry_model()
    }

    pub(crate) fn retry_model(&mut self) -> Result<(), SessionError> {
        self.inner.retry_model()
    }

    /// Cancel only the current provider request and return the protocol to a
    /// retryable Ready state. This differs from closing the Agent, which seals
    /// the entire session and invalidates its epoch.
    pub(crate) fn stop_model_request(&mut self) -> Result<bool, SessionError> {
        if self.inner.state() != AgentState::AwaitingModel || self.in_flight.is_none() {
            return Ok(false);
        }
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
        }
        self.inner
            .model_failed("Model request stopped. Retry it or revise the instruction.")?;
        Ok(true)
    }

    pub(crate) fn approve(&mut self, id: ProposalId) -> Result<ApprovedCommand, SessionError> {
        if let Some(command) = self.inner.transcript().iter().find_map(|turn| match turn {
            Turn::AssistantProposed {
                id: proposal_id,
                command,
                ..
            } if *proposal_id == id => Some(command.as_str()),
            _ => None,
        }) {
            if let Some(error) = local_agent_command_error(command) {
                return Err(error);
            }
        }
        let approved = self.inner.approve(id)?;
        self.arm_approved(&approved)?;
        Ok(approved)
    }

    pub(crate) fn edit_and_approve(
        &mut self,
        id: ProposalId,
        edited_command: impl Into<String>,
    ) -> Result<ApprovedCommand, SessionError> {
        let edited_command = edited_command.into();
        if let Some(error) = local_agent_command_error(&edited_command) {
            return Err(error);
        }
        let approved = self.inner.edit_and_approve(id, edited_command)?;
        self.arm_approved(&approved)?;
        Ok(approved)
    }

    /// Arm the one-shot execution identity for an approved command.
    ///
    /// The counter is checked, never wrapped: a reused generation would let a
    /// late completion from an earlier execution attach its output to this
    /// approval. Exhaustion is unreachable in practice — it needs 2^64
    /// approvals in one session — so the honest response is to seal the
    /// session rather than to start reusing identities.
    fn arm_approved(&mut self, approved: &ApprovedCommand) -> Result<(), SessionError> {
        let Some(generation) = self.next_execution_generation.checked_add(1) else {
            self.cancel();
            return Err(SessionError::Protocol(ParseError::InvalidCommand(
                "Agent execution identities are exhausted".to_string(),
            )));
        };
        self.next_execution_generation = generation;
        self.awaiting_command = Some(PendingAgentCommand {
            proposal_id: approved.proposal_id,
            command: approved.command.clone(),
            execution: AgentExecutionRef {
                epoch: self.epoch(),
                generation,
            },
        });
        Ok(())
    }

    /// Correlate a terminal completion without mutating protocol state. Manual
    /// completions are represented by `None` at the integration layer and do
    /// not enter this identity-bearing path.
    pub(crate) fn correlate_execution(
        &self,
        execution: AgentExecutionRef,
        command: &str,
    ) -> AgentExecutionMatch {
        match self.awaiting_command.as_ref() {
            Some(pending) if pending.execution != execution => AgentExecutionMatch::Stale,
            Some(pending) if pending.command.trim() == command.trim() => {
                AgentExecutionMatch::Matched(pending.proposal_id)
            }
            Some(_) => AgentExecutionMatch::CommandMismatch,
            None => AgentExecutionMatch::Stale,
        }
    }

    /// Approval changed the pure protocol state, but the terminal could not
    /// atomically arm and submit that exact execution. There is no safe
    /// observation to fabricate or rollback transition, so seal the session.
    pub(crate) fn execution_start_failed(&mut self, execution: AgentExecutionRef) -> bool {
        let matches = self
            .awaiting_command
            .as_ref()
            .is_some_and(|pending| pending.execution == execution);
        if matches {
            self.cancel();
        }
        matches
    }

    pub(crate) fn reject(&mut self, id: ProposalId) -> Result<(), SessionError> {
        self.inner.reject(id)
    }

    pub(crate) fn edit_for_manual_review(
        &mut self,
        id: ProposalId,
        edited_command: impl Into<String>,
    ) -> Result<String, SessionError> {
        let edited_command = edited_command.into();
        if let Some(error) = local_agent_command_error(&edited_command) {
            return Err(error);
        }
        self.inner.edit_for_manual_review(id, edited_command)
    }

    pub(crate) fn observe(
        &mut self,
        id: ProposalId,
        exit_code: i32,
        output: &str,
    ) -> Result<(), SessionError> {
        self.inner.observe(id, exit_code, output)?;
        self.awaiting_command = None;
        Ok(())
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
        }
        self.awaiting_command = None;
        self.inner.cancel();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancellation_token().is_cancelled()
    }

    pub(crate) fn can_continue_after_completion(&self) -> bool {
        self.inner.can_continue_after_completion()
    }

    /// Follow up on a completed task in the same transcript (budget allowing).
    pub(crate) fn continue_after_completion(&mut self) -> Result<(), SessionError> {
        self.inner.continue_after_completion()
    }

    /// Drop the finished transcript and start fresh in the same pane binding.
    pub(crate) fn start_new_task(&mut self) -> Result<(), SessionError> {
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
        }
        self.awaiting_command = None;
        self.inner.start_new_task()
    }

    /// Build the user-side prompt for the next LLM turn. The system prompt
    /// lives in `ai::build_agent_system_prompt` — this is the transcript dump
    /// with the shared safety budget applied.
    pub(crate) fn build_user_prompt(&self) -> String {
        self.inner.build_user_prompt()
    }

    /// Capture the protocol state for cross-restart persistence; None when
    /// there is nothing worth saving (empty or cancelled).
    pub(crate) fn snapshot(&self) -> Option<jterm_core::agent::AgentSessionSnapshot> {
        self.inner.snapshot()
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
        }
        self.inner.cancel();
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentPanelView {
    /// Task generation the transcript below belongs to. Every action the panel
    /// emits carries it back so a stale click cannot bind to a new proposal.
    pub(crate) epoch: Option<AgentSessionEpoch>,
    pub(crate) transcript: Vec<Turn>,
    pub(crate) turns_used: u32,
    pub(crate) max_turns: u32,
    pub(crate) state: AgentState,
    pub(crate) loading: bool,
    pub(crate) can_retry_model: bool,
    pub(crate) prompt_status: crate::block_view::CommandPromptStatus,
    pub(crate) cwd: String,
    pub(crate) compact: bool,
    /// Selected finished Block attached as untrusted context to upcoming model
    /// requests, if any.
    pub(crate) attached_context: Option<crate::ai::BlockContext>,
}

#[derive(Debug)]
pub(crate) enum AgentPanelMsg {
    Open {
        provider_name: String,
        view: AgentPanelView,
    },
    Render(AgentPanelView),
    PromptStatus(crate::block_view::CommandPromptStatus),
    Submit,
    InputChanged,
    StopRequest,
    RetryRequest,
    ContinueTask,
    NewTask,
    AttachContext,
    ClearContext,
    OpenSettings,
    Closed,
    Reset,
    Focus,
}

#[derive(Debug)]
pub(crate) enum AgentPanelOutput {
    Send(String),
    Approve(AgentProposalRef, String),
    Insert(AgentProposalRef, String),
    Reject(AgentProposalRef),
    StopRequest,
    RetryRequest,
    Continue,
    NewTask,
    AttachContext,
    ClearContext,
    OpenSettings,
    Closed,
}

pub(crate) struct AgentPanelModel {
    parent: adw::ApplicationWindow,
    provider_name: String,
    view: AgentPanelView,
}

#[relm4::component(pub(crate))]
impl Component for AgentPanelModel {
    type Init = adw::ApplicationWindow;
    type Input = AgentPanelMsg;
    type Output = AgentPanelOutput;
    type CommandOutput = ();

    view! {
        root = gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_spacing: 0,
            set_hexpand: true,
            set_vexpand: false,
            set_visible: false,
            add_css_class: "block-finished",
            add_css_class: "block-assistant",
            add_css_class: "block-agent",

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 8,
                add_css_class: "block-header",
                set_margin_start: 12,
                set_margin_end: 8,
                set_margin_top: 6,
                set_margin_bottom: 2,

                gtk::Image {
                    set_icon_name: Some("system-run-symbolic"),
                    add_css_class: "agent-card-icon",
                },

                gtk::Label {
                    set_label: "Shell Agent",
                    set_xalign: 0.0,
                    add_css_class: "agent-card-title",
                },

                #[name(binding_label)]
                gtk::Label {
                    set_hexpand: true,
                    set_halign: gtk::Align::End,
                    set_ellipsize: gtk::pango::EllipsizeMode::Middle,
                    add_css_class: "agent-card-binding",
                },

                gtk::Button {
                    set_icon_name: "emblem-system-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Shell Agent settings"),
                    add_css_class: "flat",
                    connect_clicked => AgentPanelMsg::OpenSettings,
                },

                gtk::Button {
                    set_icon_name: "window-close-symbolic",
                    set_focus_on_click: false,
                    set_tooltip_text: Some("Cancel Agent and close this card"),
                    add_css_class: "flat",
                    connect_clicked => AgentPanelMsg::Closed,
                },
            },

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 8,
                set_margin_start: 12,
                set_margin_end: 12,
                set_margin_top: 2,
                set_margin_bottom: 10,

                #[name(context_card)]
                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 8,
                    set_visible: false,
                    add_css_class: "agent-context-card",

                    #[name(context_label)]
                    gtk::Label {
                        set_xalign: 0.0,
                        set_hexpand: true,
                        set_ellipsize: gtk::pango::EllipsizeMode::Middle,
                    },

                    #[name(clear_context_button)]
                    gtk::Button {
                        set_icon_name: "window-close-symbolic",
                        set_tooltip_text: Some("Detach selected Block context"),
                        add_css_class: "flat",
                        connect_clicked => AgentPanelMsg::ClearContext,
                    },
                },

                #[name(transcript_box)]
                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                },

                #[name(proposal_box)]
                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 8,
                    set_visible: false,
                    add_css_class: "agent-proposal-card",
                },

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    add_css_class: "agent-status-card",

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 8,

                        #[name(spinner)]
                        gtk::Spinner {
                            set_visible: false,
                        },

                        #[name(status)]
                        gtk::Label {
                            set_halign: gtk::Align::Start,
                            set_hexpand: true,
                            set_wrap: true,
                            set_xalign: 0.0,
                            add_css_class: "agent-status",
                        },

                        #[name(retry_button)]
                        gtk::Button {
                            set_label: "Retry",
                            set_visible: false,
                            set_tooltip_text: Some("Retry the failed model turn without duplicating input"),
                            connect_clicked => AgentPanelMsg::RetryRequest,
                        },

                        #[name(stop_button)]
                        gtk::Button {
                            set_label: "Stop",
                            set_visible: false,
                            set_tooltip_text: Some("Stop this model request and keep the Agent session"),
                            add_css_class: "destructive-action",
                            connect_clicked => AgentPanelMsg::StopRequest,
                        },

                        #[name(continue_button)]
                        gtk::Button {
                            set_label: "Follow up",
                            set_visible: false,
                            connect_clicked => AgentPanelMsg::ContinueTask,
                        },

                        #[name(new_task_button)]
                        gtk::Button {
                            set_label: "New task",
                            set_visible: false,
                            connect_clicked => AgentPanelMsg::NewTask,
                        },

                        #[name(prompt_status)]
                        gtk::Label {
                            add_css_class: "agent-prompt-status",
                            add_css_class: "agent-prompt-blocked",
                        },

                        #[name(turn_label)]
                        gtk::Label {
                            add_css_class: "agent-turn-label",
                        },
                    },

                    #[name(turn_progress)]
                    gtk::ProgressBar {
                        set_hexpand: true,
                        set_fraction: 0.0,
                    },
                },

                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    add_css_class: "agent-composer",

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,

                        #[name(input)]
                        gtk::Entry {
                            set_placeholder_text: Some("Describe a task for this pane…"),
                            set_hexpand: true,
                            add_css_class: "agent-input",
                            connect_activate => AgentPanelMsg::Submit,
                            connect_changed => AgentPanelMsg::InputChanged,
                        },

                        #[name(send_button)]
                        gtk::Button {
                            set_label: "Send",
                            set_sensitive: false,
                            add_css_class: "suggested-action",
                            add_css_class: "agent-send",
                            connect_clicked => AgentPanelMsg::Submit,
                        },
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,

                        gtk::Label {
                            set_label: "Enter sends · every proposed command stays editable and requires approval",
                            set_xalign: 0.0,
                            set_hexpand: true,
                            add_css_class: "agent-input-hint",
                        },

                        #[name(attach_context_button)]
                        gtk::Button {
                            set_label: "Attach selected Block",
                            set_tooltip_text: Some("Attach the selected finished Block as untrusted context"),
                            add_css_class: "flat",
                            connect_clicked => AgentPanelMsg::AttachContext,
                        },
                    },
                },
            },
        }
    }

    fn init(
        parent: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = Self {
            parent,
            provider_name: String::new(),
            view: AgentPanelView {
                epoch: None,
                transcript: Vec::new(),
                turns_used: 0,
                max_turns: 1,
                state: AgentState::Ready,
                loading: false,
                can_retry_model: false,
                prompt_status: crate::block_view::CommandPromptStatus::Initializing,
                cwd: ".".to_string(),
                compact: false,
                attached_context: None,
            },
        };
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            AgentPanelMsg::Open {
                provider_name,
                view,
            } => {
                self.provider_name = agent_display_text(&provider_name, false);
                self.view = view;
                self.render(widgets, sender);
                root.set_visible(true);
                widgets.input.grab_focus();
            }
            AgentPanelMsg::Render(view) => {
                self.view = view;
                self.render(widgets, sender);
            }
            AgentPanelMsg::PromptStatus(status) => {
                self.view.prompt_status = status;
                render_prompt_status(widgets, status);
            }
            AgentPanelMsg::Submit => {
                let text = widgets.input.text();
                let text = text.trim();
                if !text.is_empty() && self.view.state == AgentState::Ready {
                    let _ = sender.output(AgentPanelOutput::Send(text.to_string()));
                    widgets.input.set_text("");
                }
            }
            AgentPanelMsg::InputChanged => {
                widgets.send_button.set_sensitive(
                    self.view.state == AgentState::Ready && !widgets.input.text().trim().is_empty(),
                );
            }
            AgentPanelMsg::StopRequest => {
                let _ = sender.output(AgentPanelOutput::StopRequest);
            }
            AgentPanelMsg::RetryRequest => {
                let _ = sender.output(AgentPanelOutput::RetryRequest);
            }
            AgentPanelMsg::ContinueTask => {
                let _ = sender.output(AgentPanelOutput::Continue);
            }
            AgentPanelMsg::NewTask => {
                let _ = sender.output(AgentPanelOutput::NewTask);
            }
            AgentPanelMsg::AttachContext => {
                let _ = sender.output(AgentPanelOutput::AttachContext);
            }
            AgentPanelMsg::ClearContext => {
                let _ = sender.output(AgentPanelOutput::ClearContext);
            }
            AgentPanelMsg::OpenSettings => {
                let _ = sender.output(AgentPanelOutput::OpenSettings);
            }
            AgentPanelMsg::Closed => {
                let _ = sender.output(AgentPanelOutput::Closed);
            }
            AgentPanelMsg::Reset => {
                root.set_visible(false);
                widgets.input.set_text("");
                while let Some(child) = widgets.transcript_box.first_child() {
                    widgets.transcript_box.remove(&child);
                }
                while let Some(child) = widgets.proposal_box.first_child() {
                    widgets.proposal_box.remove(&child);
                }
            }
            AgentPanelMsg::Focus => {
                widgets.input.grab_focus();
            }
        }
    }
}

impl AgentPanelModel {
    fn render(&self, widgets: &AgentPanelModelWidgets, sender: ComponentSender<Self>) {
        while let Some(child) = widgets.transcript_box.first_child() {
            widgets.transcript_box.remove(&child);
        }
        while let Some(child) = widgets.proposal_box.first_child() {
            widgets.proposal_box.remove(&child);
        }
        widgets.proposal_box.set_visible(false);
        for turn in &self.view.transcript {
            match turn {
                Turn::User(message) => widgets
                    .transcript_box
                    .append(&render_message("You", message, false)),
                Turn::AssistantThought(message) => widgets.transcript_box.append(&render_message(
                    "Agent (thought)",
                    message,
                    false,
                )),
                Turn::AssistantSay(message) => widgets
                    .transcript_box
                    .append(&render_message("Agent", message, false)),
                Turn::AssistantProposed {
                    id,
                    command,
                    status,
                } => {
                    let current = matches!(
                        self.view.state,
                        AgentState::AwaitingApproval { proposal_id } if proposal_id == *id
                    );
                    if *status == ProposalStatus::Pending && current {
                        widgets.proposal_box.set_visible(true);
                        widgets.proposal_box.append(&render_proposed(
                            self.view
                                .epoch
                                .map(|epoch| AgentProposalRef { epoch, id: *id }),
                            *id,
                            command,
                            sender.clone(),
                            &self.parent,
                        ));
                    } else {
                        let verdict = match status {
                            ProposalStatus::Pending => "inactive",
                            ProposalStatus::Approved => "approved and ran",
                            ProposalStatus::Rejected => "rejected",
                            ProposalStatus::ManualReview => "moved to manual review",
                        };
                        widgets.transcript_box.append(&render_message(
                            "Agent",
                            &format!(
                                "Proposed command #{} ({verdict}):\n{}",
                                id.get(),
                                agent_display_text(command, false)
                            ),
                            false,
                        ));
                    }
                }
                Turn::Observation {
                    exit_code,
                    output_sample,
                    ..
                } => widgets.transcript_box.append(&render_message(
                    &format!("Output · exit {exit_code}"),
                    output_sample,
                    *exit_code != 0,
                )),
                Turn::ProtocolError(message) => widgets
                    .transcript_box
                    .append(&render_message("Error", message, true)),
            }
        }
        let status = match self.view.state {
            AgentState::Ready => "Ready for the next instruction".to_string(),
            AgentState::AwaitingModel => format!(
                "Thinking with {} · turn {}/{}",
                self.provider_name,
                self.view.turns_used.saturating_add(1),
                self.view.max_turns,
            ),
            AgentState::AwaitingApproval { proposal_id } => {
                format!("Proposal #{} is waiting for review", proposal_id.get())
            }
            AgentState::AwaitingObservation { .. } => "Running the approved command…".to_string(),
            AgentState::Completed => "Task completed".to_string(),
            AgentState::Cancelled => "Agent cancelled".to_string(),
            AgentState::TurnLimitReached => {
                "Turn limit reached. Start a new task to reset the context and budget.".to_string()
            }
        };
        widgets.status.set_label(&status);
        widgets.binding_label.set_label(&format!(
            "{} · review required · every command needs approval",
            agent_display_text(&self.view.cwd, false)
        ));
        widgets.binding_label.set_tooltip_text(Some(&self.view.cwd));

        if let Some(context) = self.view.attached_context.as_ref() {
            widgets
                .context_label
                .set_label(&agent_context_label(context));
            widgets
                .context_label
                .set_tooltip_text(Some(&agent_context_tooltip(context)));
            widgets.context_card.set_visible(true);
        } else {
            widgets.context_card.set_visible(false);
        }

        widgets.continue_button.set_visible(
            self.view.state == AgentState::Completed && self.view.turns_used < self.view.max_turns,
        );
        widgets.new_task_button.set_visible(matches!(
            self.view.state,
            AgentState::Completed | AgentState::TurnLimitReached
        ));
        widgets
            .clear_context_button
            .set_visible(self.view.attached_context.is_some());
        let can_submit = self.view.state == AgentState::Ready;
        widgets
            .send_button
            .set_sensitive(can_submit && !widgets.input.text().trim().is_empty());
        widgets.input.set_sensitive(can_submit);
        widgets.attach_context_button.set_sensitive(can_submit);
        widgets.clear_context_button.set_sensitive(can_submit);
        widgets
            .retry_button
            .set_visible(can_submit && self.view.can_retry_model);
        let loading = self.view.loading || self.view.state == AgentState::AwaitingModel;
        widgets.stop_button.set_visible(self.view.loading);
        widgets.stop_button.set_sensitive(self.view.loading);
        widgets.spinner.set_visible(loading);
        if loading {
            widgets.spinner.start();
        } else {
            widgets.spinner.stop();
        }
        widgets.turn_label.set_label(&format!(
            "{} / {} turns",
            self.view.turns_used, self.view.max_turns
        ));
        widgets
            .turn_progress
            .set_fraction(f64::from(self.view.turns_used) / f64::from(self.view.max_turns.max(1)));
        render_prompt_status(widgets, self.view.prompt_status);

        if self.view.compact {
            widgets.root.add_css_class("block-compact");
            widgets.root.set_margin_top(1);
            widgets.root.set_margin_bottom(1);
            widgets.root.set_margin_start(4);
            widgets.root.set_margin_end(4);
        } else {
            widgets.root.remove_css_class("block-compact");
            widgets.root.set_margin_top(4);
            widgets.root.set_margin_bottom(4);
            widgets.root.set_margin_start(8);
            widgets.root.set_margin_end(8);
        }
    }
}

fn render_prompt_status(
    widgets: &AgentPanelModelWidgets,
    status: crate::block_view::CommandPromptStatus,
) {
    widgets.prompt_status.set_label(status.short_label());
    widgets
        .prompt_status
        .set_tooltip_text(Some(status.blocked_message()));
    widgets.prompt_status.remove_css_class("agent-prompt-ready");
    widgets
        .prompt_status
        .remove_css_class("agent-prompt-blocked");
    widgets.prompt_status.add_css_class(if status.is_ready() {
        "agent-prompt-ready"
    } else {
        "agent-prompt-blocked"
    });
}

fn agent_context_label(context: &crate::ai::BlockContext) -> String {
    let exit = if context.exit_code == 0 {
        "exit 0".to_string()
    } else {
        format!("exit {}", context.exit_code)
    };
    format!(
        "Attached Block · {exit} · {}",
        agent_display_text(&context.cmd, false)
    )
}

fn agent_context_tooltip(context: &crate::ai::BlockContext) -> String {
    let cwd = context.cwd.as_deref().unwrap_or("unknown cwd");
    format!(
        "Command: {}\nWorking directory: {cwd}\nExit: {}{}",
        agent_display_text(&context.cmd, false),
        context.exit_code,
        if context.truncated {
            "\nOutput was truncated."
        } else {
            ""
        }
    )
}

fn render_message(speaker: &str, message: &str, error: bool) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
    card.add_css_class("agent-transcript-card");
    let speaker = gtk::Label::new(Some(speaker));
    speaker.set_xalign(0.0);
    speaker.add_css_class("agent-section-label");
    if error {
        speaker.add_css_class("error");
    }
    let body = gtk::Label::new(Some(&agent_display_text(message, true)));
    body.set_xalign(0.0);
    body.set_wrap(true);
    body.set_selectable(true);
    body.set_margin_start(10);
    body.set_margin_end(10);
    body.set_margin_bottom(8);
    if error {
        body.add_css_class("error");
    }
    card.append(&speaker);
    card.append(&body);
    card.upcast()
}

fn render_proposed(
    reference: Option<AgentProposalRef>,
    id: ProposalId,
    command: &str,
    sender: ComponentSender<AgentPanelModel>,
    parent: &adw::ApplicationWindow,
) -> gtk::Widget {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let title = gtk::Label::new(Some(&format!(
        "Command proposal · Shell Agent · #{}",
        id.get()
    )));
    title.set_xalign(0.0);
    title.add_css_class("agent-section-label");
    outer.append(&title);

    let issue = local_agent_command_issue(command);
    if let Some(issue) = issue {
        let warning = gtk::Label::new(Some(&format!("Blocked unsafe proposal: {issue}")));
        warning.add_css_class("error");
        warning.set_halign(gtk::Align::Start);
        outer.append(&warning);
    }

    let entry = gtk::Entry::new();
    entry.set_hexpand(true);
    entry.set_text(&agent_display_text(command, false));
    entry.add_css_class("agent-input");
    entry.set_sensitive(reference.is_some() && issue.is_none());
    outer.append(&entry);

    let feedback = gtk::Label::new(None);
    feedback.set_xalign(0.0);
    feedback.set_wrap(true);
    feedback.set_visible(false);
    outer.append(&feedback);

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    btn_row.set_halign(gtk::Align::End);
    let reject = gtk::Button::with_label("Reject");
    let insert = gtk::Button::with_label("Insert only");
    let approve = gtk::Button::with_label("Approve & Run");
    approve.add_css_class("suggested-action");
    let actionable = reference.is_some() && issue.is_none();
    reject.set_sensitive(actionable);
    insert.set_sensitive(actionable);
    approve.set_sensitive(actionable);

    if let Some(reference) = reference {
        {
            let sender = sender.clone();
            reject.connect_clicked(move |_| {
                let _ = sender.output(AgentPanelOutput::Reject(reference));
            });
        }
        {
            let sender = sender.clone();
            let entry = entry.clone();
            let feedback = feedback.clone();
            insert.connect_clicked(move |_| {
                let command = entry.text().trim().to_string();
                if let Some(issue) = local_agent_command_issue(&command) {
                    feedback.set_label(&format!("Cannot insert: {issue}"));
                    feedback.add_css_class("error");
                    feedback.set_visible(true);
                    return;
                }
                let _ = sender.output(AgentPanelOutput::Insert(reference, command));
            });
        }
        let approve_action: Rc<dyn Fn()> = {
            let sender = sender.clone();
            let entry = entry.clone();
            let feedback = feedback.clone();
            let parent = parent.clone();
            Rc::new(move || {
                let command = entry.text().trim().to_string();
                if let Some(issue) = local_agent_command_issue(&command) {
                    feedback.set_label(&format!("Cannot approve: {issue}"));
                    feedback.add_css_class("error");
                    feedback.set_visible(true);
                    return;
                }
                if let Some(reason) = is_dangerous(&command) {
                    let dialog = adw::AlertDialog::new(
                        Some("Run a potentially destructive command?"),
                        Some(&format!(
                            "{reason}. Verify the exact command below before continuing."
                        )),
                    );
                    dialog.add_responses(&[("cancel", "Cancel"), ("run", "Run Command")]);
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    dialog.set_response_appearance("run", adw::ResponseAppearance::Destructive);
                    let preview = gtk::Label::new(Some(&command));
                    preview.set_selectable(true);
                    preview.set_wrap(true);
                    preview.set_xalign(0.0);
                    preview.add_css_class("agent-danger-command");
                    dialog.set_extra_child(Some(&preview));
                    let sender = sender.clone();
                    dialog.connect_response(None, move |_, response| {
                        if response == "run" {
                            let _ = sender
                                .output(AgentPanelOutput::Approve(reference, command.clone()));
                        }
                    });
                    dialog.present(Some(&parent));
                } else {
                    let _ = sender.output(AgentPanelOutput::Approve(reference, command));
                }
            })
        };
        {
            let approve_action = approve_action.clone();
            approve.connect_clicked(move |_| approve_action());
        }
        entry.connect_activate(move |_| approve_action());
    }
    btn_row.append(&reject);
    btn_row.append(&insert);
    btn_row.append(&approve);
    outer.append(&btn_row);
    outer.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(max_turns: u32) -> AgentSession {
        AgentSession::new(10, 20, max_turns)
    }

    fn run_reply(command: &str) -> String {
        serde_json::json!({"action":"run", "command": command}).to_string()
    }

    #[test]
    fn full_flow_binds_pane_and_correlates_the_approved_command() {
        let mut s = session(10);
        assert_eq!((s.bound_tab, s.bound_pane), (10, 20));
        assert!(s.can_submit());
        s.submit_user("list files").unwrap();
        assert_eq!(s.state(), AgentState::AwaitingModel);

        let outcome = s.accept_model_reply(&run_reply("ls -la")).unwrap();
        let ModelOutcome::Proposal {
            id,
            command,
            danger,
        } = outcome
        else {
            panic!("expected proposal");
        };
        assert_eq!(command, "ls -la");
        assert!(danger.is_none());
        assert_eq!(s.state(), AgentState::AwaitingApproval { proposal_id: id });
        assert!(s.awaiting_command.is_none());

        let approved = s.approve(id).unwrap();
        assert_eq!(approved.command, "ls -la");
        assert_eq!(
            s.awaiting_command,
            Some(PendingAgentCommand {
                proposal_id: id,
                command: "ls -la".to_string(),
                execution: AgentExecutionRef {
                    epoch: s.epoch(),
                    generation: 1,
                },
            }),
            "approval must arm the block-completion correlation slot"
        );
        assert_eq!(
            s.state(),
            AgentState::AwaitingObservation { proposal_id: id }
        );

        s.observe(id, 0, "total 0").unwrap();
        assert!(s.awaiting_command.is_none());
        assert_eq!(s.state(), AgentState::AwaitingModel);

        s.accept_model_reply(&serde_json::json!({"action":"done","message":"Listed."}).to_string())
            .unwrap();
        assert_eq!(s.state(), AgentState::Completed);
        assert!(s.is_sealed());
    }

    #[test]
    fn a_ui_action_only_resolves_within_its_own_task_generation() {
        let mut s = session(10);
        s.submit_user("run something").unwrap();
        let ModelOutcome::Proposal { id, .. } = s.accept_model_reply(&run_reply("true")).unwrap()
        else {
            panic!("expected proposal");
        };
        let reference = AgentProposalRef {
            epoch: s.epoch(),
            id,
        };
        assert_eq!(s.resolve_proposal(reference), Some(id));

        // New Task renumbers proposals, so a click still in flight from the
        // previous transcript must not land on whatever now holds that id.
        s.reject(id).unwrap();
        s.accept_model_reply(&serde_json::json!({"action":"done","message":"done"}).to_string())
            .unwrap();
        s.start_new_task().unwrap();
        assert_ne!(s.epoch(), reference.epoch);
        assert_eq!(s.resolve_proposal(reference), None);

        s.submit_user("run something else").unwrap();
        let ModelOutcome::Proposal { id: fresh, .. } =
            s.accept_model_reply(&run_reply("false")).unwrap()
        else {
            panic!("expected proposal");
        };
        assert_eq!(
            s.resolve_proposal(AgentProposalRef {
                epoch: s.epoch(),
                id: fresh
            }),
            Some(fresh)
        );
    }

    #[test]
    fn an_oversized_model_reply_never_reaches_the_parser() {
        let mut s = session(10);
        s.submit_user("summarize").unwrap();
        let oversized = serde_json::json!({
            "action": "say",
            "message": "x".repeat(MAX_AGENT_MODEL_REPLY_BYTES),
        })
        .to_string();
        assert!(oversized.len() > MAX_AGENT_MODEL_REPLY_BYTES);
        assert!(matches!(
            s.accept_model_reply(&oversized),
            Err(SessionError::Protocol(ParseError::FieldTooLarge(
                "model reply"
            )))
        ));
        // Recorded as a provider failure: the model turn is not consumed, so
        // the user can retry, and no transcript entry carries the reply.
        assert_eq!(s.turns_used(), 0);
        assert!(matches!(
            s.transcript().last(),
            Some(Turn::ProtocolError(message)) if message.contains("exceeds the")
        ));
        assert!(s
            .transcript()
            .iter()
            .all(|turn| !matches!(turn, Turn::AssistantSay(_))));
    }

    #[test]
    fn edited_approval_records_the_edited_command() {
        let mut s = session(10);
        s.submit_user("delete stuff").unwrap();
        let ModelOutcome::Proposal { id, .. } =
            s.accept_model_reply(&run_reply("rm -r ./build")).unwrap()
        else {
            panic!("expected proposal");
        };
        let approved = s.edit_and_approve(id, "rm -r ./build/tmp").unwrap();
        assert_eq!(approved.command, "rm -r ./build/tmp");
        assert_eq!(
            s.awaiting_command,
            Some(PendingAgentCommand {
                proposal_id: id,
                command: "rm -r ./build/tmp".to_string(),
                execution: AgentExecutionRef {
                    epoch: s.epoch(),
                    generation: 1,
                },
            })
        );
    }

    #[test]
    fn local_gate_rejects_visual_spoof_model_and_edited_commands_before_arming() {
        for hidden in ['\u{00ad}', '\u{034f}', '\u{fe0f}', '\u{e0020}', '\u{e0100}'] {
            let mut proposed = session(10);
            proposed.submit_user("run something").unwrap();
            let raw = run_reply(&format!("echo safe{hidden}hidden"));
            assert!(proposed.accept_model_reply(&raw).is_err());
            assert_eq!(proposed.state(), AgentState::Ready);
            assert!(!proposed
                .transcript()
                .iter()
                .any(|turn| matches!(turn, Turn::AssistantProposed { .. })));
        }

        let mut edited = session(10);
        edited.submit_user("run something").unwrap();
        let ModelOutcome::Proposal { id, .. } =
            edited.accept_model_reply(&run_reply("echo safe")).unwrap()
        else {
            panic!("expected proposal");
        };
        assert!(edited
            .edit_and_approve(id, "echo safe\u{1bca0}hidden")
            .is_err());
        assert_eq!(
            edited.state(),
            AgentState::AwaitingApproval { proposal_id: id }
        );
        assert!(edited.awaiting_command.is_none());
    }

    #[test]
    fn local_restore_gate_discards_legacy_unsafe_proposals() {
        let mut core = CoreSession::new(10);
        core.submit_user("run something").unwrap();
        let raw = run_reply("echo safe\u{202e}hidden");
        if core.accept_model_reply(&raw).is_ok() {
            let snapshot = core.snapshot().expect("pending proposal snapshot");
            let restored = CoreSession::restore(snapshot).expect("legacy core restores snapshot");
            assert!(AgentSession::from_restored(restored, 10, 20).is_none());
        }
        // Once jagent itself rejects the command, the local fallback is simply
        // unreachable; both versions satisfy the fail-closed contract.
    }

    #[test]
    fn execution_identity_is_checked_and_start_failure_fails_closed() {
        let mut first = session(10);
        first.submit_user("run").unwrap();
        let ModelOutcome::Proposal { id, .. } =
            first.accept_model_reply(&run_reply("true")).unwrap()
        else {
            panic!("expected proposal");
        };
        let _ = first.approve(id).unwrap();
        let execution = first.awaiting_command.as_ref().unwrap().execution;
        let stale = AgentExecutionRef {
            epoch: execution.epoch,
            generation: execution.generation.wrapping_add(1),
        };
        assert!(!first.execution_start_failed(stale));
        assert!(!first.is_cancelled(), "stale failure must be ignored");
        assert!(first.execution_start_failed(execution));
        assert!(first.is_cancelled());
        assert!(first.awaiting_command.is_none());
    }

    #[test]
    fn queued_llm_reply_cannot_cross_new_task_or_replacement_epoch() {
        let mut reset = session(10);
        reset.submit_user("old task").unwrap();
        let old_epoch = reset.epoch();
        assert!(reset
            .apply_llm_reply(
                old_epoch,
                Ok(serde_json::json!({"action":"done","message":"old done"}).to_string()),
            )
            .unwrap());
        reset.start_new_task().unwrap();
        reset.submit_user("new task").unwrap();
        let reset_transcript = reset.transcript().to_vec();
        assert!(!reset
            .apply_llm_reply(old_epoch, Ok(run_reply("touch leaked-from-old-task")))
            .unwrap());
        assert_eq!(reset.transcript(), reset_transcript);
        assert_eq!(reset.state(), AgentState::AwaitingModel);

        let mut old = session(10);
        old.submit_user("replaced task").unwrap();
        let replaced_epoch = old.epoch();
        let mut replacement = session(10);
        replacement.submit_user("replacement task").unwrap();
        let replacement_transcript = replacement.transcript().to_vec();
        assert!(!replacement
            .apply_llm_reply(
                replaced_epoch,
                Ok(run_reply("touch leaked-from-replaced-session")),
            )
            .unwrap());
        assert_eq!(replacement.transcript(), replacement_transcript);
        assert_eq!(replacement.state(), AgentState::AwaitingModel);
    }

    #[test]
    fn old_execution_with_same_command_and_generation_cannot_touch_replacement() {
        fn armed(command: &str) -> (AgentSession, ProposalId) {
            let mut session = session(10);
            session.submit_user("run").unwrap();
            let ModelOutcome::Proposal { id, .. } =
                session.accept_model_reply(&run_reply(command)).unwrap()
            else {
                panic!("expected proposal");
            };
            let _ = session.approve(id).unwrap();
            (session, id)
        }

        let (old, _) = armed("true");
        let old_execution = old.awaiting_command.as_ref().unwrap().execution;
        let (mut replacement, replacement_id) = armed("true");
        let replacement_execution = replacement.awaiting_command.as_ref().unwrap().execution;

        assert_eq!(old_execution.generation, replacement_execution.generation);
        assert_ne!(old_execution.epoch, replacement_execution.epoch);
        assert_eq!(
            replacement.correlate_execution(old_execution, "true"),
            AgentExecutionMatch::Stale,
            "a stale completion must not resolve by command plus generation"
        );
        assert!(!replacement.execution_start_failed(old_execution));
        assert!(!replacement.is_cancelled());
        assert_eq!(
            replacement.correlate_execution(replacement_execution, "true"),
            AgentExecutionMatch::Matched(replacement_id)
        );
        assert_eq!(
            replacement.state(),
            AgentState::AwaitingObservation {
                proposal_id: replacement_id
            }
        );
    }

    #[test]
    fn reject_returns_to_model_without_arming_execution() {
        let mut s = session(10);
        s.submit_user("try something").unwrap();
        let ModelOutcome::Proposal { id, .. } = s.accept_model_reply(&run_reply("true")).unwrap()
        else {
            panic!("expected proposal");
        };
        s.reject(id).unwrap();
        assert!(s.awaiting_command.is_none());
        assert_eq!(s.state(), AgentState::AwaitingModel);
    }

    #[test]
    fn protocol_violations_surface_as_errors_and_keep_session_alive() {
        let mut s = session(10);
        s.submit_user("hello").unwrap();
        let error = s.accept_model_reply("not json").unwrap_err();
        assert!(matches!(error, SessionError::Protocol(_)));
        assert_eq!(s.state(), AgentState::Ready);
        assert!(matches!(
            s.transcript().last(),
            Some(Turn::ProtocolError(_))
        ));
    }

    #[test]
    fn model_failure_returns_to_ready_without_consuming_a_turn() {
        let mut s = session(10);
        s.submit_user("hello").unwrap();
        let used = s.turns_used();
        s.model_failed("network down").unwrap();
        assert_eq!(s.turns_used(), used);
        assert_eq!(s.state(), AgentState::Ready);
        assert!(s.can_retry_model());
        s.retry_model().unwrap();
        assert_eq!(s.state(), AgentState::AwaitingModel);
        assert_eq!(s.turns_used(), used);
    }

    #[test]
    fn manual_review_records_non_execution_and_does_not_arm_observation() {
        let mut s = session(10);
        s.submit_user("show files").unwrap();
        let ModelOutcome::Proposal { id, .. } = s.accept_model_reply(&run_reply("ls -la")).unwrap()
        else {
            panic!("expected proposal");
        };
        let command = s.edit_for_manual_review(id, "ls -lah").unwrap();
        assert_eq!(command, "ls -lah");
        assert_eq!(s.state(), AgentState::Ready);
        assert!(s.awaiting_command.is_none());
        assert!(matches!(
            s.transcript().iter().find(|turn| matches!(
                turn,
                Turn::AssistantProposed { id: proposal_id, .. } if *proposal_id == id
            )),
            Some(Turn::AssistantProposed {
                status: ProposalStatus::ManualReview,
                ..
            })
        ));
    }

    #[test]
    fn cancel_disarms_execution_and_seals_the_session() {
        let mut s = session(10);
        s.submit_user("run").unwrap();
        let ModelOutcome::Proposal { id, .. } =
            s.accept_model_reply(&run_reply("sleep 100")).unwrap()
        else {
            panic!("expected proposal");
        };
        let _ = s.approve(id).unwrap();
        let token = s.cancellation_token();
        s.cancel();
        assert!(s.awaiting_command.is_none());
        assert!(s.is_cancelled());
        assert!(token.is_cancelled());
        assert!(s.is_sealed());
        assert!(matches!(
            s.submit_user("more"),
            Err(SessionError::Cancelled)
        ));
    }

    #[test]
    fn turn_cap_still_records_the_final_observation() {
        let mut s = session(1);
        s.submit_user("one shot").unwrap();
        let ModelOutcome::Proposal { id, .. } = s.accept_model_reply(&run_reply("true")).unwrap()
        else {
            panic!("expected proposal");
        };
        let _ = s.approve(id).unwrap();
        s.observe(id, 0, "done").unwrap();
        assert_eq!(s.state(), AgentState::TurnLimitReached);
        assert!(!s.can_submit());
    }

    #[test]
    fn dangerous_commands_are_flagged_through_the_shared_blacklist() {
        assert!(is_dangerous("rm -rf /").is_some());
        assert!(is_dangerous("ls -la").is_none());
    }
}
