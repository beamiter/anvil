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
        self.last_manual_completed = None;
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
    /// Command line of the manual block attached as untrusted context to the
    /// next model request, if any.
    pub(crate) attached_context: Option<String>,
}

#[derive(Debug)]
pub(crate) enum AgentPanelMsg {
    Open {
        provider_name: String,
        view: AgentPanelView,
    },
    Render(AgentPanelView),
    Submit,
    ContinueTask,
    NewTask,
    ClearContext,
    Closed,
}

#[derive(Debug)]
pub(crate) enum AgentPanelOutput {
    Send(String),
    Approve(AgentProposalRef),
    Edit(AgentProposalRef, String),
    Reject(AgentProposalRef),
    Continue,
    NewTask,
    ClearContext,
    Closed,
}

pub(crate) struct AgentPanelModel {
    parent: adw::ApplicationWindow,
    view: AgentPanelView,
}

#[relm4::component(pub(crate))]
impl Component for AgentPanelModel {
    type Init = adw::ApplicationWindow;
    type Input = AgentPanelMsg;
    type Output = AgentPanelOutput;
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_title: "AI agent",
            set_content_width: 820,
            set_content_height: 640,
            connect_closed => AgentPanelMsg::Closed,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 8,
                    set_margin_all: 12,

                    #[name(intro)]
                    gtk::Label {
                        set_wrap: true,
                        set_halign: gtk::Align::Start,
                        add_css_class: "dim-label",
                    },

                    gtk::ScrolledWindow {
                        set_hexpand: true,
                        set_vexpand: true,

                        #[name(transcript_box)]
                        gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 10,
                            set_margin_top: 4,
                        },
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 8,

                        #[name(status)]
                        gtk::Label {
                            set_halign: gtk::Align::Start,
                            set_hexpand: true,
                            add_css_class: "dim-label",
                        },

                        #[name(spinner)]
                        gtk::Spinner {
                            set_visible: false,
                        },

                        #[name(continue_button)]
                        gtk::Button {
                            set_label: "Continue task",
                            set_visible: false,
                            add_css_class: "suggested-action",
                            connect_clicked => AgentPanelMsg::ContinueTask,
                        },

                        #[name(new_task_button)]
                        gtk::Button {
                            set_label: "New task",
                            set_visible: false,
                            connect_clicked => AgentPanelMsg::NewTask,
                        },

                        #[name(clear_context_button)]
                        gtk::Button {
                            set_label: "Detach context",
                            set_visible: false,
                            set_tooltip_text: Some(
                                "Stop attaching the last manual command to model requests",
                            ),
                            connect_clicked => AgentPanelMsg::ClearContext,
                        },
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,

                        #[name(input)]
                        gtk::Entry {
                            set_placeholder_text: Some("What do you want to do? (Enter to send)"),
                            set_hexpand: true,
                            connect_activate => AgentPanelMsg::Submit,
                        },

                        #[name(send_button)]
                        gtk::Button {
                            set_label: "Send",
                            add_css_class: "suggested-action",
                            connect_clicked => AgentPanelMsg::Submit,
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
            view: AgentPanelView {
                epoch: None,
                transcript: Vec::new(),
                turns_used: 0,
                max_turns: 1,
                state: AgentState::Ready,
                loading: false,
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
                let provider_name = agent_display_text(&provider_name, false);
                widgets.intro.set_label(&format!(
                    "Talk to {provider_name}. The model proposes one command per turn; you approve each before it runs. Output is fed back automatically. Max {} turns.",
                    view.max_turns
                ));
                self.view = view;
                self.render(widgets, sender);
                root.present(Some(&self.parent));
                widgets.input.grab_focus();
            }
            AgentPanelMsg::Render(view) => {
                self.view = view;
                self.render(widgets, sender);
            }
            AgentPanelMsg::Submit => {
                let text = widgets.input.text();
                let text = text.trim();
                if !text.is_empty() && self.view.state == AgentState::Ready {
                    let _ = sender.output(AgentPanelOutput::Send(text.to_string()));
                    widgets.input.set_text("");
                }
            }
            AgentPanelMsg::ContinueTask => {
                let _ = sender.output(AgentPanelOutput::Continue);
            }
            AgentPanelMsg::NewTask => {
                let _ = sender.output(AgentPanelOutput::NewTask);
            }
            AgentPanelMsg::ClearContext => {
                let _ = sender.output(AgentPanelOutput::ClearContext);
            }
            AgentPanelMsg::Closed => {
                let _ = sender.output(AgentPanelOutput::Closed);
            }
        }
    }
}

impl AgentPanelModel {
    fn render(&self, widgets: &AgentPanelModelWidgets, sender: ComponentSender<Self>) {
        while let Some(child) = widgets.transcript_box.first_child() {
            widgets.transcript_box.remove(&child);
        }
        for turn in &self.view.transcript {
            let widget = match turn {
                Turn::User(message) => render_user(message),
                Turn::AssistantThought(message) => render_thought(message),
                Turn::AssistantSay(message) => render_say(message),
                Turn::AssistantProposed {
                    id,
                    command,
                    status,
                } => render_proposed(
                    self.view
                        .epoch
                        .map(|epoch| AgentProposalRef { epoch, id: *id }),
                    *id,
                    command,
                    *status,
                    sender.clone(),
                    matches!(
                        self.view.state,
                        AgentState::AwaitingApproval { proposal_id } if proposal_id == *id
                    ),
                ),
                Turn::Observation {
                    exit_code,
                    output_sample,
                    ..
                } => render_observation(*exit_code, output_sample),
                Turn::ProtocolError(message) => render_protocol_error(message),
            };
            widgets.transcript_box.append(&widget);
        }
        let status = match self.view.state {
            AgentState::Ready => format!(
                "Ready — turn {}/{}",
                self.view.turns_used, self.view.max_turns
            ),
            AgentState::AwaitingModel => format!(
                "Waiting for model — turn {}/{}",
                self.view.turns_used, self.view.max_turns
            ),
            AgentState::AwaitingApproval { proposal_id } => {
                format!("Proposal #{} needs approval", proposal_id.get())
            }
            AgentState::AwaitingObservation { proposal_id } => {
                format!("Waiting for proposal #{} output…", proposal_id.get())
            }
            AgentState::Completed => "Completed.".to_string(),
            AgentState::Cancelled => "Cancelled.".to_string(),
            AgentState::TurnLimitReached => format!(
                "Turn limit reached ({}/{}).",
                self.view.turns_used, self.view.max_turns
            ),
        };
        let status = match self.view.attached_context.as_deref() {
            Some(command) => format!("{status} · context: {}", agent_display_text(command, false)),
            None => status,
        };
        widgets.status.set_label(&status);
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
        widgets.send_button.set_sensitive(can_submit);
        widgets.input.set_sensitive(can_submit);
        let loading = self.view.loading || self.view.state == AgentState::AwaitingModel;
        widgets.spinner.set_visible(loading);
        if loading {
            widgets.spinner.start();
        } else {
            widgets.spinner.stop();
        }
    }
}

#[derive(Debug)]
pub(crate) enum AgentEditMsg {
    Open(AgentProposalRef, String),
    Submit,
    Close,
}

#[derive(Debug)]
pub(crate) enum AgentEditOutput {
    Approved(AgentProposalRef, String),
}

pub(crate) struct AgentEditModel {
    parent: adw::ApplicationWindow,
    /// The proposal the open dialog is editing. `None` until it is opened, so
    /// a Submit that somehow arrives first cannot approve anything.
    reference: Option<AgentProposalRef>,
}

#[relm4::component(pub(crate))]
impl Component for AgentEditModel {
    type Init = adw::ApplicationWindow;
    type Input = AgentEditMsg;
    type Output = AgentEditOutput;
    type CommandOutput = ();

    view! {
        root = adw::Dialog {
            set_title: "Edit command",
            set_content_width: 560,
            set_content_height: 180,

            #[wrap(Some)]
            set_child = &adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 8,
                    set_margin_all: 12,

                    #[name(entry)]
                    gtk::Entry {
                        set_hexpand: true,
                        connect_activate => AgentEditMsg::Submit,
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 6,
                        set_halign: gtk::Align::End,

                        gtk::Button {
                            set_label: "Cancel",
                            connect_clicked => AgentEditMsg::Close,
                        },

                        gtk::Button {
                            set_label: "Run",
                            add_css_class: "suggested-action",
                            connect_clicked => AgentEditMsg::Submit,
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
            reference: None,
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
            AgentEditMsg::Open(reference, command) => {
                self.reference = Some(reference);
                widgets.entry.set_text(&agent_display_text(&command, false));
                widgets.entry.select_region(0, -1);
                root.present(Some(&self.parent));
                widgets.entry.grab_focus();
            }
            AgentEditMsg::Submit => {
                let command = widgets.entry.text();
                let command = command.trim();
                if let (false, Some(reference)) = (command.is_empty(), self.reference.take()) {
                    root.force_close();
                    let _ =
                        sender.output(AgentEditOutput::Approved(reference, command.to_string()));
                }
            }
            AgentEditMsg::Close => {
                self.reference = None;
                root.force_close();
            }
        }
    }
}

fn render_user(msg: &str) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    frame.set_halign(gtk::Align::End);
    let display = agent_display_text(msg, true);
    let l = gtk::Label::new(Some(&display));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_margin_top(8);
    l.set_margin_bottom(8);
    l.set_margin_start(10);
    l.set_margin_end(10);
    l.set_selectable(true);
    frame.set_child(Some(&l));
    frame.upcast()
}

fn render_thought(msg: &str) -> gtk::Widget {
    let l = gtk::Label::new(Some(&format!("💭 {}", agent_display_text(msg, true))));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_halign(gtk::Align::Start);
    l.add_css_class("dim-label");
    l.set_selectable(true);
    l.upcast()
}

fn render_say(msg: &str) -> gtk::Widget {
    let display = agent_display_text(msg, true);
    let l = gtk::Label::new(Some(&display));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_halign(gtk::Align::Start);
    l.set_selectable(true);
    l.upcast()
}

fn render_protocol_error(message: &str) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    frame.add_css_class("error");
    let label = gtk::Label::new(Some(&format!(
        "Protocol/provider error: {}",
        agent_display_text(message, true)
    )));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_selectable(true);
    label.set_margin_top(8);
    label.set_margin_bottom(8);
    label.set_margin_start(10);
    label.set_margin_end(10);
    label.add_css_class("error");
    frame.set_child(Some(&label));
    frame.upcast()
}

fn render_proposed(
    reference: Option<AgentProposalRef>,
    id: ProposalId,
    command: &str,
    status: ProposalStatus,
    sender: ComponentSender<AgentPanelModel>,
    is_current: bool,
) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);
    outer.set_margin_start(10);
    outer.set_margin_end(10);

    let issue = local_agent_command_issue(command);
    let danger = is_dangerous(command);
    if let Some(issue) = issue {
        let warning = gtk::Label::new(Some(&format!("Blocked unsafe proposal: {issue}")));
        warning.add_css_class("error");
        warning.set_halign(gtk::Align::Start);
        outer.append(&warning);
    }
    if let Some(reason) = danger {
        let warn = gtk::Label::new(Some(&format!("⚠ destructive — {reason}")));
        warn.add_css_class("error");
        warn.set_halign(gtk::Align::Start);
        outer.append(&warn);
    }

    let cmd_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    cmd_view
        .buffer()
        .set_text(&agent_display_text(command, false));
    cmd_view.add_css_class("ai-explain-body");
    outer.append(&cmd_view);

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    btn_row.set_halign(gtk::Align::End);
    match status {
        ProposalStatus::Pending => {
            let approve = gtk::Button::with_label(if danger.is_some() {
                "Approve & Run (destructive)"
            } else {
                "Approve & Run"
            });
            if danger.is_some() {
                approve.add_css_class("destructive-action");
            } else {
                approve.add_css_class("suggested-action");
            }
            // Without an epoch the panel is rendering a transcript no live
            // session owns, so no action may be raised from it at all.
            let actionable = is_current && reference.is_some();
            approve.set_sensitive(actionable && issue.is_none());
            let edit = gtk::Button::with_label("Edit");
            let reject = gtk::Button::with_label("Reject");
            edit.set_sensitive(actionable);
            reject.set_sensitive(actionable);
            if let Some(reference) = reference {
                {
                    let sender = sender.clone();
                    approve.connect_clicked(move |_| {
                        let _ = sender.output(AgentPanelOutput::Approve(reference));
                    });
                }
                {
                    let sender = sender.clone();
                    let cmd_str = command.to_string();
                    edit.connect_clicked(move |_| {
                        let _ = sender.output(AgentPanelOutput::Edit(reference, cmd_str.clone()));
                    });
                }
                {
                    let sender = sender.clone();
                    reject.connect_clicked(move |_| {
                        let _ = sender.output(AgentPanelOutput::Reject(reference));
                    });
                }
            }
            btn_row.append(&reject);
            btn_row.append(&edit);
            btn_row.append(&approve);
            if !is_current {
                let stale = gtk::Label::new(Some(&format!("proposal #{} inactive", id.get())));
                stale.add_css_class("dim-label");
                btn_row.prepend(&stale);
            }
        }
        ProposalStatus::Approved => {
            let l = gtk::Label::new(Some("✓ ran"));
            l.add_css_class("dim-label");
            btn_row.append(&l);
        }
        ProposalStatus::Rejected => {
            let l = gtk::Label::new(Some("✗ rejected"));
            l.add_css_class("dim-label");
            btn_row.append(&l);
        }
        ProposalStatus::ManualReview => {
            let l = gtk::Label::new(Some("moved to prompt for manual review"));
            l.add_css_class("dim-label");
            btn_row.append(&l);
        }
    }
    outer.append(&btn_row);
    frame.set_child(Some(&outer));
    frame.upcast()
}

fn render_observation(exit: i32, output_sample: &str) -> gtk::Widget {
    let exp = gtk::Expander::new(Some(&format!(
        "Output (exit {exit}, {} bytes)",
        output_sample.len()
    )));
    if exit != 0 {
        exp.add_css_class("error");
    }
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    view.buffer()
        .set_text(&agent_display_text(output_sample, true));
    view.add_css_class("ai-explain-body");
    let scroll = gtk::ScrolledWindow::builder()
        .height_request(180)
        .child(&view)
        .build();
    exp.set_child(Some(&scroll));
    exp.upcast()
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
