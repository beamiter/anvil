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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use relm4::adw;
use relm4::gtk;
use relm4::prelude::*;
use serde_json::{Map, Value};

/// Hard cap on transcript bytes sent to the LLM. Past this, the middle is
/// elided. Chosen well below typical 100k context windows so the system
/// prompt + few-shots still fit comfortably alongside.
const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024;
/// Per-observation output sample cap (head+tail). Keeps the model from
/// drowning in a `find /` dump.
const MAX_OBSERVATION_BYTES: usize = 4 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_THOUGHT_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ProposalId(u64);

impl ProposalId {
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
}

/// A single entry in the agent's running transcript. The conversation is
/// reconstructed from this list every turn (we don't cache server-side
/// chat history because the API contract varies between providers and
/// resending is fine for short sessions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Turn {
    /// User's free-text input. The first turn is always a User.
    User(String),
    /// The model's chain-of-thought sentence — surfaced dimly in the UI so
    /// the user can see *why* a command was proposed. Optional per turn.
    AssistantThought(String),
    /// The model's chat response that does NOT propose a command. Used for
    /// clarifying questions, summaries, and the final "done" answer.
    AssistantSay(String),
    /// The model proposed a command. Proposal ids are stable for the entire
    /// session and all approval/observation transitions validate them.
    AssistantProposed {
        id: ProposalId,
        command: String,
        status: ProposalStatus,
    },
    /// The captured outcome of an approved command. `output_sample` is
    /// already truncated to `MAX_OBSERVATION_BYTES`.
    Observation {
        proposal_id: ProposalId,
        exit_code: i32,
        output_sample: String,
    },
    /// A provider or protocol failure. It is shown explicitly and never
    /// interpreted as a command or normal assistant message.
    ProtocolError(String),
}

impl Turn {
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
            Turn::AssistantProposed {
                command, status, ..
            } => {
                let payload = serde_json::json!({"action": "run", "command": command});
                let verdict = match status {
                    ProposalStatus::Pending => "[awaiting user approval]",
                    ProposalStatus::Approved => "[user approved; awaiting/received output]",
                    ProposalStatus::Rejected => "[user rejected this proposal]",
                };
                format!("Assistant: {payload}\n{verdict}")
            }
            Turn::Observation {
                exit_code,
                output_sample,
                ..
            } => {
                format!("Output (exit={exit_code}):\n{output_sample}")
            }
            Turn::ProtocolError(message) => {
                format!("[previous model/provider error: {message}]")
            }
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParseError {
    Empty,
    InvalidFence,
    InvalidJson(String),
    ExpectedObject,
    MissingField(&'static str),
    InvalidFieldType(&'static str),
    EmptyField(&'static str),
    FieldTooLarge(&'static str),
    UnknownAction(String),
    UnexpectedField(String),
    InvalidCommand(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty reply"),
            Self::InvalidFence => write!(f, "invalid or unterminated JSON code fence"),
            Self::InvalidJson(error) => write!(f, "invalid JSON: {error}"),
            Self::ExpectedObject => write!(f, "top-level JSON value must be an object"),
            Self::MissingField(field) => write!(f, "missing required field '{field}'"),
            Self::InvalidFieldType(field) => write!(f, "field '{field}' must be a string"),
            Self::EmptyField(field) => write!(f, "field '{field}' must not be empty"),
            Self::FieldTooLarge(field) => write!(f, "field '{field}' exceeds its size limit"),
            Self::UnknownAction(action) => write!(f, "unknown action '{action}'"),
            Self::UnexpectedField(field) => write!(f, "unexpected field '{field}'"),
            Self::InvalidCommand(message) => write!(f, "invalid command: {message}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse exactly one JSON object. A single optional `json` fence is accepted;
/// surrounding prose, multiple fences/objects, unknown keys, and invalid
/// values are protocol errors and never degrade into executable proposals.
pub(crate) fn parse_action(raw: &str) -> Result<ParsedAction, ParseError> {
    let payload = strip_json_fence(raw.trim())?;
    if payload.is_empty() {
        return Err(ParseError::Empty);
    }
    let value: Value = serde_json::from_str(payload)
        .map_err(|error| ParseError::InvalidJson(error.to_string()))?;
    let object = value.as_object().ok_or(ParseError::ExpectedObject)?;
    let action = required_string(object, "action", 32)?;
    let thought = optional_string(object, "thought", MAX_THOUGHT_BYTES)?;
    match action.as_str() {
        "run" => {
            reject_unexpected(object, &["action", "thought", "command"])?;
            let command = required_string(object, "command", MAX_COMMAND_BYTES)?;
            validate_command(&command)?;
            Ok(ParsedAction::Run { thought, command })
        }
        "say" => {
            reject_unexpected(object, &["action", "thought", "message"])?;
            let message = required_string(object, "message", MAX_MESSAGE_BYTES)?;
            Ok(ParsedAction::Say { thought, message })
        }
        "done" => {
            reject_unexpected(object, &["action", "thought", "message"])?;
            let message = required_string(object, "message", MAX_MESSAGE_BYTES)?;
            Ok(ParsedAction::Done { thought, message })
        }
        other => Err(ParseError::UnknownAction(other.to_string())),
    }
}

fn strip_json_fence(raw: &str) -> Result<&str, ParseError> {
    if !raw.starts_with("```") {
        return Ok(raw);
    }
    let newline = raw.find('\n').ok_or(ParseError::InvalidFence)?;
    let language = raw[3..newline].trim();
    if !language.is_empty() && !language.eq_ignore_ascii_case("json") {
        return Err(ParseError::InvalidFence);
    }
    raw[newline + 1..]
        .strip_suffix("```")
        .map(str::trim)
        .ok_or(ParseError::InvalidFence)
}

fn required_string(
    object: &Map<String, Value>,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, ParseError> {
    let value = object.get(field).ok_or(ParseError::MissingField(field))?;
    let value = value
        .as_str()
        .ok_or(ParseError::InvalidFieldType(field))?
        .trim();
    if value.is_empty() {
        return Err(ParseError::EmptyField(field));
    }
    if value.len() > max_bytes {
        return Err(ParseError::FieldTooLarge(field));
    }
    Ok(value.to_string())
}

fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, ParseError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or(ParseError::InvalidFieldType(field))?
        .trim();
    if value.is_empty() {
        return Err(ParseError::EmptyField(field));
    }
    if value.len() > max_bytes {
        return Err(ParseError::FieldTooLarge(field));
    }
    Ok(Some(value.to_string()))
}

fn reject_unexpected(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ParseError> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ParseError::UnexpectedField(field.clone()));
    }
    Ok(())
}

fn validate_command(command: &str) -> Result<(), ParseError> {
    if command.len() > MAX_COMMAND_BYTES {
        return Err(ParseError::FieldTooLarge("command"));
    }
    if command.contains('\0') {
        return Err(ParseError::InvalidCommand("contains a NUL byte".into()));
    }
    if command
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err(ParseError::InvalidCommand(
            "contains non-whitespace control characters".into(),
        ));
    }
    Ok(())
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
    transcript: Vec<Turn>,
    state: AgentState,
    turns_used: u32,
    max_turns: u32,
    next_proposal_id: u64,
    /// The approved proposal currently executing in the bound pane. Keeping
    /// the id with the command prevents same-text/stale block completions
    /// from being attached to the wrong proposal.
    pub(crate) awaiting_command: Option<(ProposalId, String)>,
    /// Pending callbacks share this token and must bail immediately after
    /// cancellation, even if a provider response arrives late.
    pub(crate) cancelled: Arc<AtomicBool>,
    /// Held so dropping the session cancels an in-flight LLM request.
    pub(crate) in_flight: Option<crate::ai::AiHandle>,
    /// Tab + pane the session is bound to. Commands are typed into this
    /// pane only; a BlockFinished from a different pane is ignored even
    /// if the command text matches.
    pub(crate) bound_tab: u64,
    pub(crate) bound_pane: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentState {
    Ready,
    AwaitingModel,
    AwaitingApproval { proposal_id: ProposalId },
    AwaitingObservation { proposal_id: ProposalId },
    Completed,
    Cancelled,
    TurnLimitReached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionError {
    EmptyUserMessage,
    InvalidTransition {
        operation: &'static str,
        state: AgentState,
    },
    Protocol(ParseError),
    StaleProposal {
        expected: ProposalId,
        received: ProposalId,
    },
    ProposalNotFound(ProposalId),
    TurnLimitReached,
    Cancelled,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUserMessage => write!(f, "user message must not be empty"),
            Self::InvalidTransition { operation, state } => {
                write!(f, "cannot {operation} while session is {state:?}")
            }
            Self::Protocol(error) => write!(f, "model protocol error: {error}"),
            Self::StaleProposal { expected, received } => write!(
                f,
                "proposal id {} is stale; expected {}",
                received.get(),
                expected.get()
            ),
            Self::ProposalNotFound(id) => write!(f, "proposal {} is not in transcript", id.get()),
            Self::TurnLimitReached => write!(f, "agent turn limit reached"),
            Self::Cancelled => write!(f, "agent session cancelled"),
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelOutcome {
    Proposal {
        id: ProposalId,
        command: String,
        danger: Option<&'static str>,
    },
    Said(String),
    Completed(String),
}

/// Explicit authorization token. Receiving a proposal never executes it;
/// only a successful approve/edit transition yields this value to the UI.
#[must_use = "approval only yields a command; the caller must deliberately handle it"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovedCommand {
    pub(crate) proposal_id: ProposalId,
    pub(crate) command: String,
    pub(crate) danger: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub(crate) struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl AgentSession {
    pub(crate) fn new(bound_tab: u64, bound_pane: u64, max_turns: u32) -> Self {
        Self {
            transcript: Vec::new(),
            state: AgentState::Ready,
            turns_used: 0,
            max_turns: max_turns.max(1),
            next_proposal_id: 1,
            awaiting_command: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            in_flight: None,
            bound_tab,
            bound_pane,
        }
    }

    pub(crate) fn transcript(&self) -> &[Turn] {
        &self.transcript
    }

    pub(crate) fn state(&self) -> AgentState {
        self.state
    }

    pub(crate) fn turns_used(&self) -> u32 {
        self.turns_used
    }

    pub(crate) fn max_turns(&self) -> u32 {
        self.max_turns
    }

    pub(crate) fn is_sealed(&self) -> bool {
        matches!(
            self.state,
            AgentState::Completed | AgentState::Cancelled | AgentState::TurnLimitReached
        )
    }

    pub(crate) fn can_submit(&self) -> bool {
        self.state == AgentState::Ready && self.turns_used < self.max_turns
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        CancellationToken(self.cancelled.clone())
    }

    pub(crate) fn proposal_id_at(&self, transcript_index: usize) -> Option<ProposalId> {
        match self.transcript.get(transcript_index) {
            Some(Turn::AssistantProposed { id, .. }) => Some(*id),
            _ => None,
        }
    }

    pub(crate) fn submit_user(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.check_not_cancelled()?;
        if self.turns_used >= self.max_turns {
            self.state = AgentState::TurnLimitReached;
            return Err(SessionError::TurnLimitReached);
        }
        if self.state != AgentState::Ready {
            return Err(self.invalid_transition("submit user input"));
        }
        let message = message.into();
        let message = message.trim();
        if message.is_empty() {
            return Err(SessionError::EmptyUserMessage);
        }
        self.transcript.push(Turn::User(message.to_string()));
        self.state = AgentState::AwaitingModel;
        Ok(())
    }

    pub(crate) fn receive_model(&mut self, raw: &str) -> Result<ModelOutcome, SessionError> {
        self.check_not_cancelled()?;
        if self.state != AgentState::AwaitingModel {
            return Err(self.invalid_transition("accept a model reply"));
        }
        if self.turns_used >= self.max_turns {
            self.state = AgentState::TurnLimitReached;
            return Err(SessionError::TurnLimitReached);
        }
        self.in_flight = None;
        self.turns_used = self.turns_used.saturating_add(1);
        let action = match parse_action(raw) {
            Ok(action) => action,
            Err(error) => {
                self.transcript.push(Turn::ProtocolError(error.to_string()));
                self.state = self.ready_or_limited();
                return Err(SessionError::Protocol(error));
            }
        };
        match action {
            ParsedAction::Run { thought, command } => {
                self.push_thought(thought);
                let id = ProposalId(self.next_proposal_id);
                self.next_proposal_id = self.next_proposal_id.saturating_add(1);
                self.transcript.push(Turn::AssistantProposed {
                    id,
                    command: command.clone(),
                    status: ProposalStatus::Pending,
                });
                self.state = AgentState::AwaitingApproval { proposal_id: id };
                Ok(ModelOutcome::Proposal {
                    id,
                    danger: is_dangerous(&command),
                    command,
                })
            }
            ParsedAction::Say { thought, message } => {
                self.push_thought(thought);
                self.transcript.push(Turn::AssistantSay(message.clone()));
                self.state = self.ready_or_limited();
                Ok(ModelOutcome::Said(message))
            }
            ParsedAction::Done { thought, message } => {
                self.push_thought(thought);
                self.transcript.push(Turn::AssistantSay(message.clone()));
                self.state = AgentState::Completed;
                Ok(ModelOutcome::Completed(message))
            }
        }
    }

    pub(crate) fn accept_model_reply(&mut self, raw: &str) -> Result<ModelOutcome, SessionError> {
        self.receive_model(raw)
    }

    /// Record a provider/transport failure without consuming a model turn.
    /// The session returns to Ready, so the user can retry or revise the
    /// request without weakening protocol parsing.
    pub(crate) fn model_failed(&mut self, message: impl Into<String>) -> Result<(), SessionError> {
        self.check_not_cancelled()?;
        if self.state != AgentState::AwaitingModel {
            return Err(self.invalid_transition("record a model failure"));
        }
        self.in_flight = None;
        let message = message.into();
        let message = message.trim();
        let message = if message.is_empty() {
            "provider request failed".to_string()
        } else {
            elide_middle(message, MAX_MESSAGE_BYTES)
        };
        self.transcript.push(Turn::ProtocolError(message));
        self.state = self.ready_or_limited();
        Ok(())
    }

    pub(crate) fn approve(&mut self, id: ProposalId) -> Result<ApprovedCommand, SessionError> {
        self.approve_inner(id, None)
    }

    pub(crate) fn edit_and_approve(
        &mut self,
        id: ProposalId,
        edited_command: impl Into<String>,
    ) -> Result<ApprovedCommand, SessionError> {
        self.approve_inner(id, Some(edited_command.into()))
    }

    fn approve_inner(
        &mut self,
        id: ProposalId,
        edited_command: Option<String>,
    ) -> Result<ApprovedCommand, SessionError> {
        self.check_not_cancelled()?;
        self.expect_pending_proposal(id, "approve a proposal")?;
        let edited_command = edited_command
            .map(|command| {
                let command = command.trim();
                if command.is_empty() {
                    return Err(SessionError::Protocol(ParseError::EmptyField("command")));
                }
                validate_command(command).map_err(SessionError::Protocol)?;
                Ok(command.to_string())
            })
            .transpose()?;
        let turn = self.proposal_mut(id)?;
        let Turn::AssistantProposed {
            command, status, ..
        } = turn
        else {
            unreachable!("proposal_mut only returns proposal turns")
        };
        if let Some(edited) = edited_command {
            *command = edited;
        }
        *status = ProposalStatus::Approved;
        let command = command.clone();
        let approved = ApprovedCommand {
            proposal_id: id,
            danger: is_dangerous(&command),
            command: command.clone(),
        };
        self.awaiting_command = Some((id, command));
        self.state = AgentState::AwaitingObservation { proposal_id: id };
        Ok(approved)
    }

    pub(crate) fn reject(&mut self, id: ProposalId) -> Result<(), SessionError> {
        self.check_not_cancelled()?;
        self.expect_pending_proposal(id, "reject a proposal")?;
        let turn = self.proposal_mut(id)?;
        if let Turn::AssistantProposed { status, .. } = turn {
            *status = ProposalStatus::Rejected;
        }
        self.state = if self.turns_used >= self.max_turns {
            AgentState::TurnLimitReached
        } else {
            AgentState::AwaitingModel
        };
        Ok(())
    }

    pub(crate) fn observe(
        &mut self,
        id: ProposalId,
        exit_code: i32,
        output: &str,
    ) -> Result<(), SessionError> {
        self.check_not_cancelled()?;
        match self.state {
            AgentState::AwaitingObservation { proposal_id } if proposal_id == id => {}
            AgentState::AwaitingObservation { proposal_id } => {
                return Err(SessionError::StaleProposal {
                    expected: proposal_id,
                    received: id,
                });
            }
            _ => return Err(self.invalid_transition("record command output")),
        }
        self.awaiting_command = None;
        self.transcript.push(Turn::Observation {
            proposal_id: id,
            exit_code,
            output_sample: sample_observation(output),
        });
        // The turn cap does not interrupt an approved command: the final
        // observation is always recorded before the session is sealed.
        self.state = if self.turns_used >= self.max_turns {
            AgentState::TurnLimitReached
        } else {
            AgentState::AwaitingModel
        };
        Ok(())
    }

    pub(crate) fn cancel(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
        }
        self.cancelled.store(true, Ordering::SeqCst);
        self.awaiting_command = None;
        self.state = AgentState::Cancelled;
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
        lines.push("Reply with exactly one JSON object from the protocol; no markdown.".into());
        let full = lines.join("\n\n");
        elide_middle(&full, MAX_TRANSCRIPT_BYTES)
    }

    fn proposal_mut(&mut self, id: ProposalId) -> Result<&mut Turn, SessionError> {
        self.transcript
            .iter_mut()
            .find(|turn| {
                matches!(turn, Turn::AssistantProposed { id: candidate, .. } if *candidate == id)
            })
            .ok_or(SessionError::ProposalNotFound(id))
    }

    fn expect_pending_proposal(
        &self,
        id: ProposalId,
        operation: &'static str,
    ) -> Result<(), SessionError> {
        match self.state {
            AgentState::AwaitingApproval { proposal_id } if proposal_id == id => Ok(()),
            AgentState::AwaitingApproval { proposal_id } => Err(SessionError::StaleProposal {
                expected: proposal_id,
                received: id,
            }),
            _ => Err(self.invalid_transition(operation)),
        }
    }

    fn push_thought(&mut self, thought: Option<String>) {
        if let Some(thought) = thought {
            self.transcript.push(Turn::AssistantThought(thought));
        }
    }

    fn ready_or_limited(&self) -> AgentState {
        if self.turns_used >= self.max_turns {
            AgentState::TurnLimitReached
        } else {
            AgentState::Ready
        }
    }

    fn check_not_cancelled(&self) -> Result<(), SessionError> {
        if self.is_cancelled() || self.state == AgentState::Cancelled {
            Err(SessionError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn invalid_transition(&self, operation: &'static str) -> SessionError {
        SessionError::InvalidTransition {
            operation,
            state: self.state,
        }
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            handle.cancel();
        }
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Sample raw command output for the model. Head + tail elision keeps the
/// beginning (where errors usually surface) and the end (where summary
/// lines live) while bounding bytes.
pub(crate) fn sample_observation(output: &str) -> String {
    elide_middle(output, MAX_OBSERVATION_BYTES)
}

#[derive(Debug, Clone)]
pub(crate) struct AgentPanelView {
    pub(crate) transcript: Vec<Turn>,
    pub(crate) turns_used: u32,
    pub(crate) max_turns: u32,
    pub(crate) state: AgentState,
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
                state: AgentState::Ready,
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
                if !text.is_empty() && self.view.state == AgentState::Ready {
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
            AgentState::Completed => "Completed — open a new agent to continue.".to_string(),
            AgentState::Cancelled => "Cancelled.".to_string(),
            AgentState::TurnLimitReached => format!(
                "Turn limit reached — open a new agent to continue. ({}/{})",
                self.view.turns_used, self.view.max_turns
            ),
        };
        widgets.status.set_label(&status);
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

    fn session(max_turns: u32) -> AgentSession {
        AgentSession::new(10, 20, max_turns)
    }

    fn run_reply(command: &str) -> String {
        serde_json::json!({"action":"run", "command": command}).to_string()
    }

    #[test]
    fn strict_parser_accepts_only_action_specific_schema() {
        assert_eq!(
            parse_action(r#"{"action":"run","command":"ls -la"}"#).unwrap(),
            ParsedAction::Run {
                thought: None,
                command: "ls -la".into()
            }
        );
        assert_eq!(
            parse_action(r#"{"action":"say","thought":"ask","message":"What dir?"}"#).unwrap(),
            ParsedAction::Say {
                thought: Some("ask".into()),
                message: "What dir?".into()
            }
        );
        assert_eq!(
            parse_action(r#"{"action":"done","message":"All clear."}"#).unwrap(),
            ParsedAction::Done {
                thought: None,
                message: "All clear.".into()
            }
        );
        assert!(matches!(
            parse_action(r#"{"action":"run","command":"ls","message":"extra"}"#),
            Err(ParseError::UnexpectedField(_))
        ));
    }

    #[test]
    fn parser_tolerates_one_json_fence_but_no_prose_or_extra_object() {
        let parsed =
            parse_action("```json\n{\"action\":\"done\",\"message\":\"ok\"}\n```").unwrap();
        assert!(matches!(parsed, ParsedAction::Done { .. }));
        assert!(parse_action("result: {\"action\":\"done\",\"message\":\"ok\"}").is_err());
        assert!(parse_action("```text\n{}\n```").is_err());
        assert!(parse_action("```json\n{}\n``` trailing").is_err());
        assert!(parse_action(
            "{\"action\":\"say\",\"message\":\"one\"}\n{\"action\":\"say\",\"message\":\"two\"}"
        )
        .is_err());
    }

    #[test]
    fn parser_rejects_unknown_wrong_type_empty_oversize_and_controls() {
        assert!(matches!(
            parse_action(r#"{"action":"frobnicate","message":"huh"}"#),
            Err(ParseError::UnknownAction(_))
        ));
        assert!(matches!(
            parse_action(r#"{"action":"run","command":7}"#),
            Err(ParseError::InvalidFieldType("command"))
        ));
        assert!(matches!(
            parse_action(r#"{"action":"run","command":""}"#),
            Err(ParseError::EmptyField("command"))
        ));
        assert!(matches!(
            parse_action(r#"{"action":"say","thought":"","message":"ok"}"#),
            Err(ParseError::EmptyField("thought"))
        ));
        let oversized = serde_json::json!({
            "action": "run",
            "command": "x".repeat(MAX_COMMAND_BYTES + 1)
        })
        .to_string();
        assert!(matches!(
            parse_action(&oversized),
            Err(ParseError::FieldTooLarge("command"))
        ));
        let control = serde_json::json!({"action":"run", "command":"echo\u{0007}bad"}).to_string();
        assert!(matches!(
            parse_action(&control),
            Err(ParseError::InvalidCommand(_))
        ));
    }

    #[test]
    fn approval_is_explicit_and_observation_advances_session() {
        let mut s = session(4);
        s.submit_user("show files").unwrap();
        let ModelOutcome::Proposal { id, .. } = s.receive_model(&run_reply("ls -la")).unwrap()
        else {
            panic!("expected proposal")
        };
        assert_eq!(s.state(), AgentState::AwaitingApproval { proposal_id: id });
        let approved = s.approve(id).unwrap();
        assert_eq!(approved.proposal_id, id);
        assert_eq!(approved.command, "ls -la");
        assert_eq!(s.awaiting_command.as_ref().map(|entry| entry.0), Some(id));
        assert_eq!(
            s.state(),
            AgentState::AwaitingObservation { proposal_id: id }
        );
        s.observe(id, 0, "a\nb").unwrap();
        assert_eq!(s.state(), AgentState::AwaitingModel);
        assert!(s.awaiting_command.is_none());
        assert!(matches!(
            s.transcript().last(),
            Some(Turn::Observation { .. })
        ));
    }

    #[test]
    fn proposal_ids_are_stable_monotonic_and_rejection_is_recorded() {
        let mut s = session(4);
        s.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id: first, .. } =
            s.receive_model(&run_reply("find /")).unwrap()
        else {
            panic!("expected proposal")
        };
        s.reject(first).unwrap();
        assert_eq!(s.state(), AgentState::AwaitingModel);
        let ModelOutcome::Proposal { id: second, .. } = s.receive_model(&run_reply("pwd")).unwrap()
        else {
            panic!("expected proposal")
        };
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
        assert!(matches!(
            s.transcript()
                .iter()
                .find(|turn| matches!(turn, Turn::AssistantProposed { id, .. } if *id == first)),
            Some(Turn::AssistantProposed {
                status: ProposalStatus::Rejected,
                ..
            })
        ));
        assert!(matches!(
            s.approve(first),
            Err(SessionError::StaleProposal { .. })
        ));
    }

    #[test]
    fn edit_and_approve_validates_and_returns_only_edited_command() {
        let mut s = session(3);
        s.submit_user("inspect").unwrap();
        let ModelOutcome::Proposal { id, .. } = s.receive_model(&run_reply("rm -rf /")).unwrap()
        else {
            panic!("expected proposal")
        };
        assert!(matches!(
            s.edit_and_approve(id, "  "),
            Err(SessionError::Protocol(ParseError::EmptyField("command")))
        ));
        assert_eq!(s.state(), AgentState::AwaitingApproval { proposal_id: id });
        let approved = s.edit_and_approve(id, "ls /").unwrap();
        assert_eq!(approved.command, "ls /");
        assert!(approved.danger.is_none());
        assert!(matches!(
            s.transcript().last(),
            Some(Turn::AssistantProposed {
                command,
                status: ProposalStatus::Approved,
                ..
            }) if command == "ls /"
        ));
    }

    #[test]
    fn stale_and_out_of_order_operations_fail_without_mutating_state() {
        let mut s = session(3);
        assert!(matches!(
            s.receive_model(&run_reply("pwd")),
            Err(SessionError::InvalidTransition { .. })
        ));
        assert!(matches!(
            s.submit_user("  "),
            Err(SessionError::EmptyUserMessage)
        ));
        assert!(matches!(
            s.approve(ProposalId(1)),
            Err(SessionError::InvalidTransition { .. })
        ));
        s.submit_user("inspect").unwrap();
        assert!(matches!(
            s.submit_user("second while busy"),
            Err(SessionError::InvalidTransition { .. })
        ));
        assert!(matches!(
            s.observe(ProposalId(1), 0, "wrong"),
            Err(SessionError::InvalidTransition { .. })
        ));
        let ModelOutcome::Proposal { id, .. } = s.receive_model(&run_reply("pwd")).unwrap() else {
            panic!("expected proposal")
        };
        assert!(matches!(
            s.reject(ProposalId(id.get() + 1)),
            Err(SessionError::StaleProposal { .. })
        ));
        assert!(matches!(
            s.edit_and_approve(ProposalId(id.get() + 1), "ls"),
            Err(SessionError::StaleProposal { .. })
        ));
        let _approved = s.approve(id).unwrap();
        assert!(matches!(
            s.reject(id),
            Err(SessionError::InvalidTransition { .. })
        ));
        assert!(matches!(
            s.observe(ProposalId(id.get() + 1), 0, "wrong"),
            Err(SessionError::StaleProposal { .. })
        ));
        assert_eq!(
            s.state(),
            AgentState::AwaitingObservation { proposal_id: id }
        );
    }

    #[test]
    fn malformed_reply_is_protocol_error_and_never_a_proposal() {
        let mut s = session(3);
        s.submit_user("inspect").unwrap();
        assert!(matches!(
            s.receive_model("run: rm -rf /"),
            Err(SessionError::Protocol(_))
        ));
        assert_eq!(s.state(), AgentState::Ready);
        assert!(matches!(
            s.transcript().last(),
            Some(Turn::ProtocolError(_))
        ));
        assert!(!s
            .transcript()
            .iter()
            .any(|turn| matches!(turn, Turn::AssistantProposed { .. })));
    }

    #[test]
    fn provider_failure_returns_to_ready_without_consuming_a_turn() {
        let mut s = session(3);
        s.submit_user("inspect").unwrap();
        s.model_failed("network timeout").unwrap();
        assert_eq!(s.state(), AgentState::Ready);
        assert_eq!(s.turns_used(), 0);
        assert!(s.can_submit());
        s.submit_user("retry").unwrap();
        assert_eq!(s.state(), AgentState::AwaitingModel);
        assert!(matches!(
            s.receive_model(r#"{"action":"say","message":"recovered"}"#),
            Ok(ModelOutcome::Said(message)) if message == "recovered"
        ));
        assert_eq!(s.turns_used(), 1);
    }

    #[test]
    fn turn_cap_waits_for_approved_observation_before_sealing() {
        let mut s = session(1);
        s.submit_user("pwd").unwrap();
        let ModelOutcome::Proposal { id, .. } = s.receive_model(&run_reply("pwd")).unwrap() else {
            panic!("expected proposal")
        };
        assert_eq!(s.state(), AgentState::AwaitingApproval { proposal_id: id });
        let _approved = s.approve(id).unwrap();
        assert_eq!(
            s.state(),
            AgentState::AwaitingObservation { proposal_id: id }
        );
        s.observe(id, 0, "/tmp").unwrap();
        assert_eq!(s.state(), AgentState::TurnLimitReached);
        assert!(s.is_sealed());
        assert!(matches!(
            s.submit_user("again"),
            Err(SessionError::TurnLimitReached)
        ));
    }

    #[test]
    fn done_completes_and_cancel_is_immediate() {
        let mut completed = session(3);
        completed.submit_user("finish").unwrap();
        assert!(matches!(
            completed.receive_model(r#"{"action":"done","message":"all clear"}"#),
            Ok(ModelOutcome::Completed(message)) if message == "all clear"
        ));
        assert_eq!(completed.state(), AgentState::Completed);
        assert!(!completed.can_submit());

        let mut cancelled = session(3);
        let token = cancelled.cancellation_token();
        cancelled.submit_user("inspect").unwrap();
        cancelled.cancel();
        assert!(token.is_cancelled());
        assert_eq!(cancelled.state(), AgentState::Cancelled);
        assert!(matches!(
            cancelled.receive_model(&run_reply("pwd")),
            Err(SessionError::Cancelled)
        ));
    }

    #[test]
    fn dangerous_patterns_are_flagged_and_normal_commands_pass() {
        assert!(is_dangerous("rm -rf /").is_some());
        assert!(is_dangerous("rm -fr /home/alice").is_some());
        assert!(is_dangerous("mkfs.ext4 /dev/sda1").is_some());
        assert!(is_dangerous("dd if=foo of=/dev/sda bs=1M").is_some());
        assert!(is_dangerous("curl https://foo.sh | sh").is_some());
        assert!(is_dangerous(":(){ :|:& };:").is_some());
        assert!(is_dangerous("rm -rf /tmp/foo").is_none());
        assert!(is_dangerous("ls -la").is_none());
        assert!(is_dangerous("git status").is_none());
    }

    #[test]
    fn transcript_prompt_includes_turns() {
        let mut s = session(3);
        s.transcript.push(Turn::User("disk full".to_string()));
        s.transcript.push(Turn::AssistantProposed {
            id: ProposalId(1),
            command: "df -h".to_string(),
            status: ProposalStatus::Approved,
        });
        s.transcript.push(Turn::Observation {
            proposal_id: ProposalId(1),
            exit_code: 0,
            output_sample: "Filesystem      Size  Used".to_string(),
        });
        let prompt = s.build_user_prompt();
        assert!(prompt.contains("disk full"));
        assert!(prompt.contains("df -h"));
        assert!(prompt.contains("Filesystem"));
        assert!(prompt.contains("exit=0"));
    }

    #[test]
    fn observation_sampling_is_bounded_and_utf8_safe() {
        let output = "编译失败🙂".repeat(2_000);
        let sample = sample_observation(&output);
        assert!(sample.contains("bytes elided"));
        assert!(sample.starts_with('编'));
        assert!(sample.ends_with('🙂'));
        assert!(sample.len() < MAX_OBSERVATION_BYTES + 128);
    }
}
