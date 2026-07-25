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
//!    callbacks) and clears `awaiting_command` so a late block-finished
//!    event won't attach to a dead session.

use adw::prelude::*;
use relm4::adw;
use relm4::gtk;
use relm4::prelude::*;

pub(crate) use jterm_core::agent::{
    is_dangerous, AgentState, ApprovedCommand, CancellationToken, ModelOutcome, ProposalId,
    ProposalStatus, SessionError, Turn,
};

use jterm_core::agent::AgentSession as CoreSession;

/// jterm1's Agent session. The pure protocol state machine (turn caps,
/// approval transitions, transcript bounds, prompt assembly) lives in
/// `jterm_core::agent`; this wrapper adds what is jterm1-specific: the
/// tab/pane binding, the approved-command correlation slot, and ownership
/// of the in-flight LLM request handle.
pub(crate) struct AgentSession {
    inner: CoreSession,
    /// The approved proposal currently executing in the bound pane. Keeping
    /// the id with the command prevents same-text/stale block completions
    /// from being attached to the wrong proposal.
    pub(crate) awaiting_command: Option<(ProposalId, String)>,
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
    pub(crate) fn from_restored(inner: CoreSession, bound_tab: u64, bound_pane: u64) -> Self {
        Self::wrap(inner, bound_tab, bound_pane)
    }

    fn wrap(inner: CoreSession, bound_tab: u64, bound_pane: u64) -> Self {
        Self {
            inner,
            awaiting_command: None,
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

    pub(crate) fn proposal_id_at(&self, transcript_index: usize) -> Option<ProposalId> {
        match self.inner.transcript().get(transcript_index) {
            Some(Turn::AssistantProposed { id, .. }) => Some(*id),
            _ => None,
        }
    }

    pub(crate) fn submit_user(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.inner.submit_user(message)
    }

    pub(crate) fn accept_model_reply(&mut self, raw: &str) -> Result<ModelOutcome, SessionError> {
        self.in_flight = None;
        self.inner.accept_model_reply(raw)
    }

    /// Record a provider/transport failure without consuming a model turn.
    pub(crate) fn model_failed(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.in_flight = None;
        self.inner.model_failed(message)
    }

    pub(crate) fn approve(&mut self, id: ProposalId) -> Result<ApprovedCommand, SessionError> {
        let approved = self.inner.approve(id)?;
        self.awaiting_command = Some((approved.proposal_id, approved.command.clone()));
        Ok(approved)
    }

    pub(crate) fn edit_and_approve(
        &mut self,
        id: ProposalId,
        edited_command: impl Into<String>,
    ) -> Result<ApprovedCommand, SessionError> {
        let approved = self.inner.edit_and_approve(id, edited_command)?;
        self.awaiting_command = Some((approved.proposal_id, approved.command.clone()));
        Ok(approved)
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
    Approve(usize),
    Edit(usize, String),
    Reject(usize),
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
        for (index, turn) in self.view.transcript.iter().enumerate() {
            let widget = match turn {
                Turn::User(message) => render_user(message),
                Turn::AssistantThought(message) => render_thought(message),
                Turn::AssistantSay(message) => render_say(message),
                Turn::AssistantProposed {
                    id,
                    command,
                    status,
                } => render_proposed(
                    index,
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
            Some(command) => format!("{status} · context: {command}"),
            None => status,
        };
        widgets.status.set_label(&status);
        widgets.continue_button.set_visible(
            self.view.state == AgentState::Completed
                && self.view.turns_used < self.view.max_turns,
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
    Open(usize, String),
    Submit,
    Close,
}

#[derive(Debug)]
pub(crate) enum AgentEditOutput {
    Approved(usize, String),
}

pub(crate) struct AgentEditModel {
    parent: adw::ApplicationWindow,
    index: usize,
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
        let model = Self { parent, index: 0 };
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
            AgentEditMsg::Open(index, command) => {
                self.index = index;
                widgets.entry.set_text(&command);
                widgets.entry.select_region(0, -1);
                root.present(Some(&self.parent));
                widgets.entry.grab_focus();
            }
            AgentEditMsg::Submit => {
                let command = widgets.entry.text();
                let command = command.trim();
                if !command.is_empty() {
                    root.force_close();
                    let _ =
                        sender.output(AgentEditOutput::Approved(self.index, command.to_string()));
                }
            }
            AgentEditMsg::Close => root.force_close(),
        }
    }
}

fn render_user(msg: &str) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    frame.set_halign(gtk::Align::End);
    let l = gtk::Label::new(Some(msg));
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
    let l = gtk::Label::new(Some(&format!("💭 {msg}")));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_halign(gtk::Align::Start);
    l.add_css_class("dim-label");
    l.set_selectable(true);
    l.upcast()
}

fn render_say(msg: &str) -> gtk::Widget {
    let l = gtk::Label::new(Some(msg));
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
    let label = gtk::Label::new(Some(&format!("Protocol/provider error: {message}")));
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
    idx: usize,
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

    let danger = is_dangerous(command);
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
    cmd_view.buffer().set_text(command);
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
            approve.set_sensitive(is_current);
            let edit = gtk::Button::with_label("Edit");
            let reject = gtk::Button::with_label("Reject");
            edit.set_sensitive(is_current);
            reject.set_sensitive(is_current);
            {
                let sender = sender.clone();
                approve.connect_clicked(move |_| {
                    let _ = sender.output(AgentPanelOutput::Approve(idx));
                });
            }
            {
                let sender = sender.clone();
                let cmd_str = command.to_string();
                edit.connect_clicked(move |_| {
                    let _ = sender.output(AgentPanelOutput::Edit(idx, cmd_str.clone()));
                });
            }
            {
                let sender = sender.clone();
                reject.connect_clicked(move |_| {
                    let _ = sender.output(AgentPanelOutput::Reject(idx));
                });
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
    view.buffer().set_text(output_sample);
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
        let ModelOutcome::Proposal { id, command, danger } = outcome else {
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
            Some((id, "ls -la".to_string())),
            "approval must arm the block-completion correlation slot"
        );
        assert_eq!(s.state(), AgentState::AwaitingObservation { proposal_id: id });

        s.observe(id, 0, "total 0").unwrap();
        assert!(s.awaiting_command.is_none());
        assert_eq!(s.state(), AgentState::AwaitingModel);

        s.accept_model_reply(
            &serde_json::json!({"action":"done","message":"Listed."}).to_string(),
        )
        .unwrap();
        assert_eq!(s.state(), AgentState::Completed);
        assert!(s.is_sealed());
    }

    #[test]
    fn proposal_id_at_maps_transcript_rows_to_proposals() {
        let mut s = session(10);
        s.submit_user("run something").unwrap();
        let ModelOutcome::Proposal { id, .. } =
            s.accept_model_reply(&run_reply("true")).unwrap()
        else {
            panic!("expected proposal");
        };
        assert_eq!(s.proposal_id_at(0), None, "row 0 is the user turn");
        assert_eq!(s.proposal_id_at(1), Some(id));
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
        assert_eq!(s.awaiting_command, Some((id, "rm -r ./build/tmp".to_string())));
    }

    #[test]
    fn reject_returns_to_model_without_arming_execution() {
        let mut s = session(10);
        s.submit_user("try something").unwrap();
        let ModelOutcome::Proposal { id, .. } =
            s.accept_model_reply(&run_reply("true")).unwrap()
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
        s.approve(id).unwrap();
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
        let ModelOutcome::Proposal { id, .. } =
            s.accept_model_reply(&run_reply("true")).unwrap()
        else {
            panic!("expected proposal");
        };
        s.approve(id).unwrap();
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
