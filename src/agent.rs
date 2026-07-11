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
//!    `MAX_OBS_BYTES` of captured output (head+tail).
//! 7. **Cancel on close.** Closing the dialog calls `AgentSession::cancel`,
//!    which both flips the cancelled flag (suppressing pending LLM
//!    callbacks) and clears `awaiting_output` so a late block-finished
//!    event won't attach to a dead session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use relm4::adw;
use relm4::gtk;
use relm4::prelude::*;

/// Hard cap on transcript bytes sent to the LLM. Past this, the middle is
/// elided. Chosen well below typical 100k context windows so the system
/// prompt + few-shots still fit comfortably alongside.
const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024;
/// Per-observation output sample cap (head+tail). Keeps the model from
/// drowning in a `find /` dump.
const MAX_OBS_BYTES: usize = 4 * 1024;

/// A single entry in the agent's running transcript. The conversation is
/// reconstructed from this list every turn (we don't cache server-side
/// chat history because the API contract varies between providers and
/// resending is fine for short sessions).
#[derive(Debug, Clone)]
pub(crate) enum Turn {
    /// User's free-text input. The first turn is always a User.
    User(String),
    /// The model's chain-of-thought sentence — surfaced dimly in the UI so
    /// the user can see *why* a command was proposed. Optional per turn.
    AssistantThought(String),
    /// The model's chat response that does NOT propose a command. Used for
    /// clarifying questions, summaries, and the final "done" answer.
    AssistantSay(String),
    /// The model proposed a command. `approved` tracks user verdict:
    /// `None` = pending, `Some(true)` = ran (Observation follows),
    /// `Some(false)` = rejected.
    AssistantProposed { cmd: String, approved: Option<bool> },
    /// The captured outcome of an approved command. `output_sample` is
    /// already truncated to `MAX_OBS_BYTES`.
    Observation { exit: i32, output_sample: String },
}

impl Turn {
    /// Approximate byte size used for transcript-cap eviction.
    fn size(&self) -> usize {
        match self {
            Turn::User(s) | Turn::AssistantThought(s) | Turn::AssistantSay(s) => s.len() + 8,
            Turn::AssistantProposed { cmd, .. } => cmd.len() + 16,
            Turn::Observation { output_sample, .. } => output_sample.len() + 16,
        }
    }

    /// Render this turn for the LLM prompt. Format is plain text with
    /// `User:` / `Assistant:` / `Output:` markers — matches the few-shot
    /// examples in `build_agent_system_prompt`.
    fn to_prompt_line(&self) -> String {
        match self {
            Turn::User(s) => format!("User: {s}"),
            Turn::AssistantThought(s) => format!("Assistant (thought): {s}"),
            Turn::AssistantSay(s) => {
                // Wrap as a `say` action so the model sees its own format.
                let payload = serde_json::json!({"action": "say", "message": s});
                format!("Assistant: {payload}")
            }
            Turn::AssistantProposed { cmd, approved } => {
                let payload = serde_json::json!({"action": "run", "command": cmd});
                match approved {
                    None => format!("Assistant: {payload}"),
                    Some(true) => format!("Assistant: {payload}\n[user approved & ran]"),
                    Some(false) => format!("Assistant: {payload}\n[user rejected]"),
                }
            }
            Turn::Observation {
                exit,
                output_sample,
            } => {
                format!("Output (exit={exit}):\n{output_sample}")
            }
        }
    }
}

/// The model's reply, parsed from JSON. Falls back to `Say(raw_text)` when
/// the JSON is malformed so the session stays usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedAction {
    Run {
        thought: Option<String>,
        command: String,
    },
    Say {
        thought: Option<String>,
        message: String,
    },
    Done {
        thought: Option<String>,
        message: String,
    },
}

/// Best-effort JSON parser. Strips a markdown fence if the model wrapped
/// its reply, then validates required fields. Returns `Say(raw)` on any
/// failure — never errors, because surfacing the raw text in the panel
/// is more useful than a parse-error toast.
pub(crate) fn parse_action(raw: &str) -> ParsedAction {
    let trimmed = strip_fences(raw.trim()).trim();
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            return ParsedAction::Say {
                thought: None,
                message: raw.trim().to_string(),
            };
        }
    };
    let thought = value
        .get("thought")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let action = value.get("action").and_then(|a| a.as_str()).unwrap_or("");
    match action {
        "run" => {
            let cmd = value
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if cmd.is_empty() {
                ParsedAction::Say {
                    thought,
                    message: raw.trim().to_string(),
                }
            } else {
                ParsedAction::Run {
                    thought,
                    command: cmd,
                }
            }
        }
        "done" => ParsedAction::Done {
            thought,
            message: value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        },
        // "say" or anything unrecognised → treat as say so unknown action
        // names don't drop the model's reply on the floor.
        _ => ParsedAction::Say {
            thought,
            message: value
                .get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| raw.trim().to_string()),
        },
    }
}

fn strip_fences(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("```json") {
        let after = rest.trim_start_matches('\n');
        if let Some(inner) = after.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    if let Some(rest) = s.strip_prefix("```") {
        let after = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        if let Some(inner) = after.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    s
}

/// Match a command against the destructive-pattern blacklist. Returns a
/// short human-readable reason when the command is flagged, `None` when
/// it looks fine. False positives are preferable to false negatives — we
/// only warn, we don't block.
pub(crate) fn is_dangerous(cmd: &str) -> Option<&'static str> {
    let c = cmd.trim();
    let lower = c.to_ascii_lowercase();
    // Fork bomb (verbatim or close).
    if c.replace(' ', "").contains(":(){:|:&};:") {
        return Some("looks like a fork bomb");
    }
    // `rm -rf` against root, home, or a parent path.
    if has_rm_rf_dangerous_target(&lower) {
        return Some("rm -rf against a top-level path");
    }
    // mkfs.* — formats a filesystem.
    if lower
        .split_whitespace()
        .any(|t| t.starts_with("mkfs.") || t == "mkfs")
    {
        return Some("mkfs formats a filesystem");
    }
    // dd if=… of=/dev/sdX  — disk overwrite.
    if lower.contains("dd ") && lower.contains("of=/dev/") {
        return Some("dd writes raw bytes to a device");
    }
    // Pipe to shell from network — typical curl|sh / wget|sh footgun.
    if (lower.contains("curl ") || lower.contains("wget "))
        && (lower.contains("| sh")
            || lower.contains("|sh")
            || lower.contains("| bash")
            || lower.contains("|bash"))
    {
        return Some("piping network content directly to a shell");
    }
    // Redirect to a raw disk device.
    if let Some(idx) = lower.find("> /dev/sd") {
        let after = &lower[idx + 2..];
        // "> /dev/sda", "> /dev/sdb1", …
        if after
            .split_whitespace()
            .next()
            .is_some_and(|t| t.starts_with("/dev/sd"))
        {
            return Some("redirecting to a raw block device");
        }
    }
    // chmod 777 -R … on a top-level dir.
    if lower.contains("chmod")
        && lower.contains("777")
        && (lower.contains(" /") || lower.contains(" ~"))
    {
        return Some("recursive chmod 777 on a top-level path");
    }
    None
}

fn has_rm_rf_dangerous_target(lower: &str) -> bool {
    // Match `rm` with -r or -R (anywhere in flag block) and -f, then look at
    // the remaining arguments for a dangerous target. We split on whitespace
    // and tolerate flag clustering like `-rf`, `-fR`, etc.
    let toks: Vec<&str> = lower.split_whitespace().collect();
    let Some(rm_idx) = toks.iter().position(|t| *t == "rm") else {
        return false;
    };
    let rest = &toks[rm_idx + 1..];
    let mut has_r = false;
    let mut has_f = false;
    let mut targets: Vec<&str> = Vec::new();
    for tok in rest {
        if let Some(flags) = tok.strip_prefix("--") {
            // long options — only recursive matters here.
            if flags == "recursive" {
                has_r = true;
            }
            if flags == "force" {
                has_f = true;
            }
            continue;
        }
        if let Some(flags) = tok.strip_prefix('-') {
            for c in flags.chars() {
                if c == 'r' || c == 'R' {
                    has_r = true;
                } else if c == 'f' {
                    has_f = true;
                }
            }
            continue;
        }
        targets.push(tok);
    }
    if !(has_r && has_f) {
        return false;
    }
    for t in targets {
        if t == "/" || t == "/*" {
            return true;
        }
        if t == "~" || t == "$home" || t.starts_with("~/") {
            return true;
        }
        // Top-level system dirs.
        if matches!(
            t,
            "/bin"
                | "/boot"
                | "/etc"
                | "/home"
                | "/lib"
                | "/lib64"
                | "/opt"
                | "/root"
                | "/sbin"
                | "/srv"
                | "/sys"
                | "/usr"
                | "/var"
                | "/proc"
                | "/dev"
        ) {
            return true;
        }
        if t.starts_with("/home/") && t.matches('/').count() == 2 {
            // /home/<user> — whole user dir.
            return true;
        }
    }
    false
}

/// Live state for one agent conversation. Held in `AppModel.active_agent`
/// behind an `Rc<RefCell<Option<…>>>` — opening a new session replaces it,
/// closing the dialog clears it.
pub(crate) struct AgentSession {
    pub transcript: Vec<Turn>,
    /// Set when we've sent a command to the active pane and are waiting
    /// for the corresponding `BlockFinished` event. Stores the command
    /// text so we can match it against the finished block (the user may
    /// have typed something else in between).
    pub awaiting_command: Option<String>,
    /// Flag flipped to true on dialog close / max-turns reached. Pending
    /// LLM callbacks check this and bail.
    pub cancelled: Arc<AtomicBool>,
    /// How many model turns we've spent this session (incremented at
    /// each `next_turn` call). Compared against `agent_max_turns`.
    pub turns_used: u32,
    /// Held so dropping the session cancels an in-flight LLM request.
    pub in_flight: Option<crate::ai::AiHandle>,
    /// Tab + pane the session is bound to. Commands are typed into this
    /// pane only; a BlockFinished from a different pane is ignored even
    /// if the command text matches.
    pub bound_tab: u64,
    pub bound_pane: u64,
    /// `true` once we've reached `agent_max_turns` or the user explicitly
    /// stopped — Send is greyed out, future LLM replies dropped.
    pub sealed: bool,
}

impl AgentSession {
    pub(crate) fn new(bound_tab: u64, bound_pane: u64) -> Self {
        Self {
            transcript: Vec::new(),
            awaiting_command: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            turns_used: 0,
            in_flight: None,
            bound_tab,
            bound_pane,
            sealed: false,
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Build the user-side prompt for the next LLM turn. The system prompt
    /// lives in `ai::build_agent_system_prompt` — this is just the
    /// transcript dump.
    pub(crate) fn build_user_prompt(&self) -> String {
        let mut lines: Vec<String> = self.transcript.iter().map(Turn::to_prompt_line).collect();
        // Final hint to nudge JSON output.
        lines.push(
            "Reply with one JSON object per the protocol. Do not wrap in markdown.".to_string(),
        );
        let full = lines.join("\n\n");
        elide_middle(&full, MAX_TRANSCRIPT_BYTES)
    }
}

/// Sample raw command output for the model. Head + tail elision keeps the
/// beginning (where errors usually surface) and the end (where summary
/// lines live) while bounding bytes.
pub(crate) fn sample_observation(output: &str) -> String {
    elide_middle(output, MAX_OBS_BYTES)
}

#[derive(Debug, Clone)]
pub(crate) struct AgentPanelView {
    pub(crate) transcript: Vec<Turn>,
    pub(crate) turns_used: u32,
    pub(crate) max_turns: u32,
    pub(crate) awaiting_command: bool,
    pub(crate) sealed: bool,
    pub(crate) loading: bool,
}

#[derive(Debug)]
pub(crate) enum AgentPanelMsg {
    Open {
        provider_name: String,
        view: AgentPanelView,
    },
    Render(AgentPanelView),
    Submit,
    Closed,
}

#[derive(Debug)]
pub(crate) enum AgentPanelOutput {
    Send(String),
    Approve(usize),
    Edit(usize, String),
    Reject(usize),
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
                awaiting_command: false,
                sealed: false,
                loading: false,
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
                if !text.is_empty() && !self.view.sealed {
                    let _ = sender.output(AgentPanelOutput::Send(text.to_string()));
                    widgets.input.set_text("");
                }
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
                Turn::AssistantProposed { cmd, approved } => {
                    render_proposed(index, cmd, *approved, sender.clone(), self.view.sealed)
                }
                Turn::Observation {
                    exit,
                    output_sample,
                } => render_observation(*exit, output_sample),
            };
            widgets.transcript_box.append(&widget);
        }
        let status = if self.view.sealed {
            format!(
                "Session sealed — open a new agent for more turns. ({}/{})",
                self.view.turns_used, self.view.max_turns
            )
        } else if self.view.awaiting_command {
            "Waiting for command output…".to_string()
        } else {
            format!("turn {}/{}", self.view.turns_used, self.view.max_turns)
        };
        widgets.status.set_label(&status);
        widgets.send_button.set_sensitive(!self.view.sealed);
        widgets.input.set_sensitive(!self.view.sealed);
        widgets.spinner.set_visible(self.view.loading);
        if self.view.loading {
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

fn render_proposed(
    idx: usize,
    cmd: &str,
    approved: Option<bool>,
    sender: ComponentSender<AgentPanelModel>,
    sealed: bool,
) -> gtk::Widget {
    let frame = gtk::Frame::new(None);
    frame.add_css_class("card");
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);
    outer.set_margin_start(10);
    outer.set_margin_end(10);

    let danger = is_dangerous(cmd);
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
    cmd_view.buffer().set_text(cmd);
    cmd_view.add_css_class("ai-explain-body");
    outer.append(&cmd_view);

    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    btn_row.set_halign(gtk::Align::End);
    match approved {
        None => {
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
            if sealed {
                approve.set_sensitive(false);
            }
            let edit = gtk::Button::with_label("Edit");
            let reject = gtk::Button::with_label("Reject");
            {
                let sender = sender.clone();
                approve.connect_clicked(move |_| {
                    let _ = sender.output(AgentPanelOutput::Approve(idx));
                });
            }
            {
                let sender = sender.clone();
                let cmd_str = cmd.to_string();
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
        }
        Some(true) => {
            let l = gtk::Label::new(Some("✓ ran"));
            l.add_css_class("dim-label");
            btn_row.append(&l);
        }
        Some(false) => {
            let l = gtk::Label::new(Some("✗ rejected"));
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

fn elide_middle(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let half = max_bytes / 2;
    // Find char boundaries to avoid slicing inside a UTF-8 codepoint.
    let mut head_end = half.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len().saturating_sub(half);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start <= head_end {
        return s[..head_end].to_string();
    }
    let elided = s.len() - (head_end + (s.len() - tail_start));
    format!(
        "{}\n\n… [{} bytes elided] …\n\n{}",
        &s[..head_end],
        elided,
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_action_run_basic() {
        let r = parse_action(r#"{"action":"run","command":"ls -la"}"#);
        assert_eq!(
            r,
            ParsedAction::Run {
                thought: None,
                command: "ls -la".to_string()
            }
        );
    }

    #[test]
    fn parse_action_run_with_thought() {
        let r = parse_action(r#"{"thought":"need to inspect","action":"run","command":"du -sh"}"#);
        match r {
            ParsedAction::Run { thought, command } => {
                assert_eq!(thought.as_deref(), Some("need to inspect"));
                assert_eq!(command, "du -sh");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_action_say() {
        let r = parse_action(r#"{"action":"say","message":"What dir?"}"#);
        assert_eq!(
            r,
            ParsedAction::Say {
                thought: None,
                message: "What dir?".to_string()
            }
        );
    }

    #[test]
    fn parse_action_done() {
        let r = parse_action(r#"{"action":"done","message":"All clear."}"#);
        assert_eq!(
            r,
            ParsedAction::Done {
                thought: None,
                message: "All clear.".to_string()
            }
        );
    }

    #[test]
    fn parse_action_strips_json_fence() {
        let raw = "```json\n{\"action\":\"run\",\"command\":\"echo hi\"}\n```";
        match parse_action(raw) {
            ParsedAction::Run { command, .. } => assert_eq!(command, "echo hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_action_falls_back_to_say_on_garbage() {
        let r = parse_action("hello this is not JSON");
        assert_eq!(
            r,
            ParsedAction::Say {
                thought: None,
                message: "hello this is not JSON".to_string()
            }
        );
    }

    #[test]
    fn parse_action_unknown_action_treated_as_say() {
        let r = parse_action(r#"{"action":"frobnicate","message":"huh"}"#);
        assert_eq!(
            r,
            ParsedAction::Say {
                thought: None,
                message: "huh".to_string()
            }
        );
    }

    #[test]
    fn parse_action_empty_command_falls_back_to_say() {
        let r = parse_action(r#"{"action":"run","command":""}"#);
        match r {
            ParsedAction::Say { .. } => {}
            other => panic!("expected say, got {other:?}"),
        }
    }

    #[test]
    fn dangerous_catches_rm_rf_root() {
        assert!(is_dangerous("rm -rf /").is_some());
        assert!(is_dangerous("rm -rf /*").is_some());
        assert!(is_dangerous("rm -fr /").is_some());
        assert!(is_dangerous("rm -r -f /").is_some());
        assert!(is_dangerous("rm --recursive --force /").is_some());
    }

    #[test]
    fn dangerous_catches_rm_rf_home() {
        assert!(is_dangerous("rm -rf ~").is_some());
        assert!(is_dangerous("rm -rf ~/").is_some());
        assert!(is_dangerous("rm -rf /home/alice").is_some());
        assert!(is_dangerous("rm -rf /home").is_some());
    }

    #[test]
    fn dangerous_allows_rm_rf_in_tmp() {
        // We deliberately do not flag /tmp/foo — that's the user's call.
        assert!(is_dangerous("rm -rf /tmp/foo").is_none());
        assert!(is_dangerous("rm -rf ./build").is_none());
        assert!(is_dangerous("rm somefile").is_none());
    }

    #[test]
    fn dangerous_catches_mkfs() {
        assert!(is_dangerous("mkfs.ext4 /dev/sda1").is_some());
        assert!(is_dangerous("sudo mkfs.xfs /dev/sdb").is_some());
    }

    #[test]
    fn dangerous_catches_dd_to_device() {
        assert!(is_dangerous("dd if=foo of=/dev/sda bs=1M").is_some());
    }

    #[test]
    fn dangerous_catches_curl_pipe_sh() {
        assert!(is_dangerous("curl https://foo.sh | sh").is_some());
        assert!(is_dangerous("wget -qO- https://foo.sh | bash").is_some());
    }

    #[test]
    fn dangerous_catches_fork_bomb() {
        assert!(is_dangerous(":(){ :|:& };:").is_some());
    }

    #[test]
    fn dangerous_lets_normal_commands_through() {
        assert!(is_dangerous("ls -la").is_none());
        assert!(is_dangerous("git status").is_none());
        assert!(is_dangerous("docker ps").is_none());
        assert!(is_dangerous("cargo build --release").is_none());
    }

    #[test]
    fn transcript_prompt_includes_turns() {
        let mut s = AgentSession::new(0, 0);
        s.transcript.push(Turn::User("disk full".to_string()));
        s.transcript.push(Turn::AssistantProposed {
            cmd: "df -h".to_string(),
            approved: Some(true),
        });
        s.transcript.push(Turn::Observation {
            exit: 0,
            output_sample: "Filesystem      Size  Used".to_string(),
        });
        let prompt = s.build_user_prompt();
        assert!(prompt.contains("disk full"));
        assert!(prompt.contains("df -h"));
        assert!(prompt.contains("Filesystem"));
        assert!(prompt.contains("exit=0"));
    }

    #[test]
    fn elide_middle_passes_short_through() {
        assert_eq!(elide_middle("hi", 100), "hi");
    }

    #[test]
    fn elide_middle_truncates_long_text_keeping_head_and_tail() {
        let big = "x".repeat(10_000);
        let s = elide_middle(&big, 1000);
        assert!(s.contains("elided"));
        assert!(s.len() < 1500);
    }

    #[test]
    fn elide_middle_respects_utf8_boundaries() {
        // Multibyte chars near the cut points.
        let s: String = (0..2000).map(|_| "λ").collect();
        let out = elide_middle(&s, 200);
        // Must be valid UTF-8 and contain the elision marker.
        assert!(out.contains("elided"));
        assert!(out.chars().count() > 0);
    }
}
