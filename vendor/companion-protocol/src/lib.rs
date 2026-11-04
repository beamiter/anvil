//! Bounded, content-free coordination protocol. Same-UID peers are not a security boundary.
use serde::{Deserialize, Serialize};
pub mod history;
pub mod strict_json;
pub const VERSION: u16 = 1;
pub const MAX_FRAME: usize = 65536;
pub const MAX_REGIONS: usize = 64;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub companion_id: String,
    pub host_id: String,
    pub incarnation: u64,
    pub boot_id: String,
    pub epoch: u64,
    pub surface_id: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetirementCapability {
    None,
    NativePresented,
    CompositorPresented,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub id: String,
    pub incarnation: u64,
    pub surface_id: String,
    pub retirement: RetirementCapability,
}
/// Features are negotiated only through the opt-in extended host handshake.
/// Legacy Hello/Accepted frames retain their exact schema and semantics.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostFeature {
    WithdrawalResumeV1,
    /// Explicit read-only dialogue history pages. Requires `WithdrawalResumeV1`.
    DialogueHistoryV1,
    /// Explicit review of Eva's retained spoken reply for memory. Requires history.
    VoiceTextV1,
    /// Proactive notices from validated background cognition. Requires voice text.
    CognitionNoticeV1,
    /// Reviewable proposals (watch a task, remember something) and goals. Requires notices.
    CognitionProposalV1,
    /// Agent-task handoffs into the host's own task composer, and their receipts.
    /// Requires proposals.
    CognitionDelegationV1,
}
/// The only accepted feature sets; order is fixed so a list cannot smuggle duplicates.
pub fn valid_host_features(features: &[HostFeature]) -> bool {
    matches!(
        features,
        [HostFeature::WithdrawalResumeV1]
            | [
                HostFeature::WithdrawalResumeV1,
                HostFeature::DialogueHistoryV1
            ]
            | [
                HostFeature::WithdrawalResumeV1,
                HostFeature::DialogueHistoryV1,
                HostFeature::VoiceTextV1
            ]
            | [
                HostFeature::WithdrawalResumeV1,
                HostFeature::DialogueHistoryV1,
                HostFeature::VoiceTextV1,
                HostFeature::CognitionNoticeV1
            ]
            | [
                HostFeature::WithdrawalResumeV1,
                HostFeature::DialogueHistoryV1,
                HostFeature::VoiceTextV1,
                HostFeature::CognitionNoticeV1,
                HostFeature::CognitionProposalV1
            ]
            | [
                HostFeature::WithdrawalResumeV1,
                HostFeature::DialogueHistoryV1,
                HostFeature::VoiceTextV1,
                HostFeature::CognitionNoticeV1,
                HostFeature::CognitionProposalV1,
                HostFeature::CognitionDelegationV1
            ]
    )
}
pub const MAX_HANDOFF_TITLE: usize = 256;
pub const MAX_HANDOFF_GOAL: usize = 4096;
/// One approved handoff. The host opens its own task composer prefilled with
/// this; the user still chooses repository, provider and validation and
/// creates the task there. Nothing here starts an agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHandoff {
    pub operation_id: String,
    pub context: ContextRef,
    pub title: String,
    pub goal: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum HandoffOutcome {
    ComposerOpened,
    /// The user closed the composer without creating a task.
    ComposerClosed,
    /// The host created this task from the handoff (its own stable task id).
    TaskCreated {
        agent_task_id: String,
    },
    /// Agent tasks are disabled or busy here; nothing was opened.
    Unavailable {
        reason: String,
    },
}
/// Mirrors the host task runtime's own lifecycle; reported, never commanded.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskState {
    Created,
    Starting,
    Working,
    WaitingForApproval,
    WaitingForHuman,
    ReadyForReview,
    Completed,
    Failed,
    Cancelled,
    Archived,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentValidation {
    NotRun,
    Running,
    Passed,
    Failed,
    Inconclusive,
    Cancelled,
}
pub fn valid_handoff(h: &AgentHandoff) -> bool {
    identifier(&h.operation_id)
        && valid_context(&h.context)
        && notice_text(&h.title, MAX_HANDOFF_TITLE)
        && !h.title.contains('\n')
        && notice_text(&h.goal, MAX_HANDOFF_GOAL)
}
pub const MAX_PROPOSAL_VALUE: usize = 1024;
pub const MAX_GOALS_LISTED: usize = 32;
/// What an approved proposal does. The approval is the user's exact review:
/// the digest covers this change, the proposal id and the origin context.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProposalChange {
    /// Tell the user once when this task's current or next run finishes.
    /// Only lets Eva notice and tell; it runs nothing.
    WatchTask {
        task_ref: String,
        notify_on: Vec<TaskStatus>,
        expires_at_ms: u64,
    },
    /// Hand a task to this host's agent task composer (negotiated
    /// `CognitionDelegationV1` only). Approval opens the composer prefilled; the
    /// user still creates the task there. Approval also watches that task.
    AgentTask { title: String, goal: String },
    /// Save one memory entry, or correct the existing entry `supersedes`.
    Remember {
        scope: MemoryScope,
        key: String,
        value: String,
        /// `note`, `preference`, `project_knowledge` or `experience`.
        memory_kind: String,
        supersedes: Option<i64>,
        source_refs: Vec<String>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CognitionProposal {
    pub proposal_id: String,
    pub digest: String,
    /// The context it was proposed in; approval must name this exact context.
    pub context: ContextRef,
    pub change: ProposalChange,
    pub explanation: String,
}
/// An active goal, shown so the user can see and stop it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalView {
    pub goal_id: String,
    pub task_ref: String,
    pub notify_on: Vec<TaskStatus>,
    pub expires_at_ms: u64,
    /// `explicit` or `approved_suggestion`.
    pub origin: String,
}
fn digest_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
fn terminal_status(s: &TaskStatus) -> bool {
    matches!(
        s,
        TaskStatus::Failed | TaskStatus::NeedsReview | TaskStatus::Succeeded
    )
}
fn task_ref_ok(r: &str) -> bool {
    r.strip_prefix("task:").is_some_and(identifier)
}
/// Hosts check every received proposal; an invalid one is dropped, never shown.
pub fn valid_proposal(p: &CognitionProposal) -> bool {
    identifier(&p.proposal_id)
        && digest_hex(&p.digest)
        && valid_context(&p.context)
        && notice_text(&p.explanation, MAX_NOTICE_EXPLANATION)
        && match &p.change {
            ProposalChange::WatchTask {
                task_ref,
                notify_on,
                ..
            } => {
                task_ref_ok(task_ref)
                    && !notify_on.is_empty()
                    && notify_on.len() <= 3
                    && notify_on.iter().all(terminal_status)
            }
            ProposalChange::AgentTask { title, goal } => {
                notice_text(title, MAX_HANDOFF_TITLE)
                    && !title.contains('\n')
                    && notice_text(goal, MAX_HANDOFF_GOAL)
            }
            ProposalChange::Remember {
                scope,
                key,
                value,
                memory_kind,
                supersedes,
                source_refs,
            } => {
                valid_scope(scope)
                    && identifier(key)
                    && notice_text(value, MAX_PROPOSAL_VALUE)
                    && matches!(
                        memory_kind.as_str(),
                        "note" | "preference" | "project_knowledge" | "experience"
                    )
                    && supersedes.is_none_or(|id| id > 0)
                    && source_refs.len() <= MAX_NOTICE_EVIDENCE
                    && source_refs.iter().all(|r| notice_ref(r))
            }
        }
}
pub fn valid_goal(g: &GoalView) -> bool {
    identifier(&g.goal_id)
        && task_ref_ok(&g.task_ref)
        && !g.notify_on.is_empty()
        && g.notify_on.len() <= 3
        && g.notify_on.iter().all(terminal_status)
        && matches!(g.origin.as_str(), "explicit" | "approved_suggestion")
}
pub const MAX_NOTICE_TEXT: usize = 2048;
pub const MAX_NOTICE_EXPLANATION: usize = 512;
pub const MAX_NOTICE_OFFER: usize = 768;
pub const MAX_NOTICE_EVIDENCE: usize = 8;
/// One proactive notice from a background cognitive turn that passed the local
/// commit check. It is generated for the receiving host's current context; an
/// offer is a label for something not executed. Nothing here grants authority.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub notice_id: String,
    pub context: ContextRef,
    /// `task:<id>` the notice is about.
    pub task_ref: String,
    /// Decision mode, e.g. `report_status` or `offer_help`.
    pub mode: String,
    pub text: String,
    /// "Why am I seeing this": one or two sentences.
    pub explanation: String,
    pub evidence_refs: Vec<String>,
    pub offer: Option<String>,
}
fn notice_text(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max && !s.chars().any(|c| c.is_control() && c != '\n')
}
fn notice_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 192
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}
/// Hosts check every received notice; an invalid one is dropped, never shown.
pub fn valid_notice(n: &Notice) -> bool {
    identifier(&n.notice_id)
        && valid_context(&n.context)
        && n.task_ref.strip_prefix("task:").is_some_and(identifier)
        && identifier(&n.mode)
        && notice_text(&n.text, MAX_NOTICE_TEXT)
        && notice_text(&n.explanation, MAX_NOTICE_EXPLANATION)
        && n.evidence_refs.len() <= MAX_NOTICE_EVIDENCE
        && n.evidence_refs.iter().all(|r| notice_ref(r))
        && n.offer
            .as_deref()
            .is_none_or(|o| notice_text(o, MAX_NOTICE_OFFER))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMetadata {
    Focus {
        focused: bool,
    },
    Input {
        active: bool,
        selecting: bool,
    },
    Geometry {
        revision: u64,
        viewport: Rect,
        candidates: Vec<Rect>,
        exclusions: Vec<Rect>,
    },
    ContextObserved {
        context: ContextRef,
    },
}
impl HostMetadata {
    pub fn into_request(self) -> Request {
        match self {
            Self::Focus { focused } => Request::Focus { focused },
            Self::Input { active, selecting } => Request::Input { active, selecting },
            Self::Geometry {
                revision,
                viewport,
                candidates,
                exclusions,
            } => Request::Geometry {
                revision,
                viewport,
                candidates,
                exclusions,
            },
            Self::ContextObserved { context } => Request::ContextObserved { context },
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    FollowMe,
    StayHere,
    Hidden,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub enum AudioState {
    Idle,
    Connecting,
    Listening,
    Thinking,
    Speaking,
    Cancelling,
    Stopped,
    Error,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    NeedsReview,
    Succeeded,
    Failed,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VoiceLease {
    pub source_id: String,
    pub session_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContextRef {
    pub host_id: String,
    pub pane_id: String,
    pub task_id: Option<String>,
    pub project_id: Option<String>,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SelectionEvidence {
    pub context: ContextRef,
    pub text: String,
    pub consent_id: String,
    pub source_event_id: String,
    pub captured_at_ms: u64,
}
pub fn valid_context(c: &ContextRef) -> bool {
    identifier(&c.host_id)
        && identifier(&c.pane_id)
        && c.task_id.as_ref().is_none_or(|s| identifier(s))
        && c.project_id.as_ref().is_none_or(|s| identifier(s))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MemoryScope {
    Global,
    Project(String),
    Task(String),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryChange {
    Put {
        scope: MemoryScope,
        key: String,
        value: String,
        source_event_id: String,
        expected_revision: Option<u64>,
        expires_at: Option<u64>,
    },
    Delete {
        id: i64,
        expected_revision: u64,
    },
    Clear {
        scope: MemoryScope,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryEntry {
    pub id: i64,
    pub scope: MemoryScope,
    pub key: String,
    pub value: String,
    pub source_event_id: String,
    pub consent_status: String,
    pub revision: u64,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryReview {
    pub id: u64,
    pub digest: String,
    pub change: MemoryChange,
    pub generation: u64,
}
pub fn valid_scope(s: &MemoryScope) -> bool {
    match s {
        MemoryScope::Global => true,
        MemoryScope::Project(id) | MemoryScope::Task(id) => identifier(id),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskScopeInfo {
    pub task_id: String,
    pub project_id: Option<String>,
    pub owner_host_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub protocol_version: u16,
    pub event_id: String,
    pub source_id: String,
    pub source_seq: u64,
    pub boot_id: String,
    pub session_id: String,
    pub body: Request,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    MemoryScopes {
        after: Option<String>,
    },
    TaskScopes {
        after: Option<String>,
    },
    TaskSharePropose {
        task_id: String,
        target_host_id: String,
    },
    TaskShareApprove {
        review_id: String,
        digest: String,
    },
    MemoryScopeReviewRequested {
        context: ContextRef,
        scope: MemoryScope,
    },
    MemoryScopeApprove {
        context: ContextRef,
        review_id: String,
        digest: String,
    },
    Withdraw,
    WithdrawIntent {
        connection_nonce: String,
        intent: u64,
        previous_token: Option<Token>,
    },
    ResumeWithdrawal {
        connection_nonce: String,
        intent: u64,
    },
    ActivateHost {
        connection_nonce: String,
    },
    HostMetadata {
        connection_nonce: String,
        admission_epoch: u64,
        metadata: HostMetadata,
    },
    DesktopHello,
    VoiceContext {
        session_id: String,
        include_global_memory: bool,
    },
    ProfileName {
        name: String,
    },
    Comfort {
        quiet: bool,
        night: bool,
    },
    VoiceAcquire {
        session_id: String,
    },
    VoiceRelease {
        session_id: String,
    },
    ContextObserved {
        context: ContextRef,
    },
    ContextAccessRequested {
        context: ContextRef,
        text: String,
        source_event_id: String,
        captured_at_ms: u64,
    },
    ContextAccessGranted {
        context: ContextRef,
        consent_id: String,
    },
    MemoryUse {
        context: ContextRef,
        scopes: Vec<MemoryScope>,
    },
    MemoryList {
        context: ContextRef,
        scope: MemoryScope,
        query: String,
        before_id: Option<i64>,
    },
    MemoryPropose {
        context: ContextRef,
        change: MemoryChange,
    },
    MemoryApprove {
        context: ContextRef,
        review_id: u64,
        digest: String,
    },
    MemoryCancel {
        review_id: u64,
    },
    MemoryPause {
        paused: bool,
    },
    DialogueStart {
        turn_id: String,
        context: ContextRef,
        text: String,
        approved_selection: Option<SelectionEvidence>,
    },
    DialogueCancel {
        turn_id: String,
    },
    DialogueDisplayed {
        turn_id: String,
    },
    /// One explicit page request. `before` is an opaque cursor from the previous page.
    DialogueHistory {
        request_id: u64,
        context: ContextRef,
        scope: history::Scope,
        before: Option<String>,
    },
    /// Ask Eva, through the coordinator, for its latest retained spoken reply
    /// and turn it into an ordinary exact-change memory review for `scope`.
    VoiceTextReview {
        request_id: u64,
        context: ContextRef,
        scope: MemoryScope,
    },
    /// Ask Eva to drop every retained spoken reply now.
    VoiceTextForget {
        request_id: u64,
        context: ContextRef,
    },
    /// The host actually showed this notice (negotiated `CognitionNoticeV1`).
    CognitionNoticeDisplayed {
        notice_id: String,
    },
    /// The user dismissed this notice; the coordinator quiets that cause longer.
    CognitionNoticeDismissed {
        notice_id: String,
    },
    /// The user approved the exact proposal shown (negotiated `CognitionProposalV1`).
    CognitionProposalApprove {
        proposal_id: String,
        context: ContextRef,
        digest: String,
    },
    CognitionProposalDecline {
        proposal_id: String,
    },
    /// Explicit button: watch the task of this exact current context.
    WatchTask {
        context: ContextRef,
    },
    GoalCancel {
        goal_id: String,
    },
    /// Goals visible to this exact current context.
    GoalList {
        context: ContextRef,
    },
    /// What happened to a handoff this host received (negotiated `CognitionDelegationV1`).
    AgentHandoffReceipt {
        operation_id: String,
        outcome: HandoffOutcome,
    },
    /// The latest state of the task created from a handoff, as the host's task
    /// runtime reports it. Sent on change and again after reconnecting.
    AgentTaskStatus {
        operation_id: String,
        agent_task_id: String,
        state: AgentTaskState,
        validation: AgentValidation,
    },
    EvaHello,
    Audio {
        turn_id: String,
        state: AudioState,
        generated_samples: u64,
        played_samples: Option<u64>,
        interrupted: bool,
    },
    SessionLock {
        locked: bool,
    },
    Resume,
    Appearance {
        skin: String,
        motion: String,
    },
    TaskObservation {
        task_id: String,
        context_revision: u64,
        status: TaskStatus,
    },
    Hello {
        host: Host,
    },
    HelloWithFeatures {
        host: Host,
        features: Vec<HostFeature>,
    },
    CompositorHello {
        host: Host,
    },
    Prepared {
        epoch: u64,
    },
    Retired {
        token: Token,
        presentation_serial: u64,
    },
    Presented {
        token: Token,
    },
    Recall,
    Focus {
        focused: bool,
    },
    Presence {
        mode: Presence,
    },
    Geometry {
        revision: u64,
        viewport: Rect,
        candidates: Vec<Rect>,
        exclusions: Vec<Rect>,
    },
    Input {
        active: bool,
        selecting: bool,
    },
    Resync,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    HostAccepted {
        boot_id: String,
        companion_id: String,
        session_id: String,
        features: Vec<HostFeature>,
        connection_nonce: String,
    },
    HostActivated {
        connection_nonce: String,
        host_id: String,
        incarnation: u64,
        surface_id: String,
        admission_epoch: u64,
    },
    WithdrawalReady {
        connection_nonce: String,
        host_id: String,
        incarnation: u64,
        surface_id: String,
        intent: u64,
        retired_token: Option<Token>,
    },
    WithdrawalResumed {
        connection_nonce: String,
        admission_epoch: u64,
        host_id: String,
        incarnation: u64,
        surface_id: String,
        intent: u64,
        retired_token: Option<Token>,
    },
    MemoryScopes {
        scopes: Vec<MemoryScope>,
        next_after: Option<String>,
    },
    TaskScopes {
        scopes: Vec<TaskScopeInfo>,
        next_after: Option<String>,
    },
    OperationResult {
        operation: String,
        status: String,
    },
    TaskShareReview {
        review_id: String,
        digest: String,
        task_id: String,
        project_id: Option<String>,
        owner_host_id: String,
        target_host_id: String,
        target_incarnation: u64,
    },
    MemoryScopeReview {
        context: ContextRef,
        scope: MemoryScope,
        review_id: String,
        digest: String,
    },
    WithdrawalAccepted {
        host_id: String,
        incarnation: u64,
    },
    RetirementAccepted {
        token: Token,
    },
    VoiceReleased {
        session_id: String,
    },
    VoiceContext {
        session_id: String,
        companion_id: String,
        profile_revision: u64,
        name: String,
        global_memory: Vec<MemoryEntry>,
        memory_status: String,
    },
    ProfileName {
        name: String,
    },
    Comfort {
        quiet: bool,
        night: bool,
    },
    OperationError {
        operation: String,
        reason: String,
    },
    VoicePermit {
        session_id: String,
        granted: bool,
    },
    ContextAccessReview {
        evidence: SelectionEvidence,
    },
    MemoryList {
        context: ContextRef,
        scope: MemoryScope,
        entries: Vec<MemoryEntry>,
        next_before_id: Option<i64>,
        paused: bool,
    },
    MemoryReview {
        context: ContextRef,
        review: MemoryReview,
    },
    MemoryResult {
        review_id: Option<u64>,
        status: String,
    },
    Dialogue {
        turn_id: String,
        context: ContextRef,
        status: String,
        reply: Option<String>,
    },
    DialogueHistory {
        request_id: u64,
        context: ContextRef,
        scope: history::Scope,
        status: String,
        entries: Vec<history::Entry>,
        next_before: Option<String>,
    },
    /// Outcome of a voice text request. A ready review arrives separately as
    /// `MemoryReview` and still needs explicit approval.
    VoiceText {
        request_id: u64,
        status: String,
    },
    /// Sent to negotiated hosts when the desktop locks or becomes unknown.
    /// A host must clear every displayed history page.
    DialogueHistoryWithdrawn {
        reason: String,
    },
    /// Sent only to hosts that negotiated `CognitionNoticeV1`.
    CognitionNotice {
        notice: Notice,
    },
    /// Clear every displayed notice (desktop locked or unknown).
    CognitionNoticeWithdrawn {
        reason: String,
    },
    /// A reviewable suggestion for this host's context (negotiated `CognitionProposalV1`).
    CognitionProposal {
        proposal: CognitionProposal,
    },
    /// Outcome of an approval, decline, watch or cancel. Never implies more than
    /// its status: `goal_active`, `remembered`, `declined`, `goal_cancelled`, or a reason.
    CognitionProposalResult {
        proposal_id: Option<String>,
        status: String,
    },
    /// Active goals visible to `context` (newest first, bounded).
    Goals {
        context: ContextRef,
        goals: Vec<GoalView>,
    },
    /// Open the task composer prefilled (negotiated `CognitionDelegationV1`).
    AgentHandoff {
        handoff: AgentHandoff,
    },
    Accepted {
        boot_id: String,
        companion_id: String,
        session_id: String,
    },
    Rejected {
        reason: String,
    },
    Prepare {
        epoch: u64,
    },
    Revoke {
        token: Token,
    },
    Grant {
        token: Token,
        revision: u64,
    },
    Snapshot {
        revision: u64,
        token: Option<Token>,
        pose: String,
        mode: Presence,
        recovery_blocked: bool,
    },
    Appearance {
        skin: String,
        motion: String,
    },
    Ack,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Rect {
    pub fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite() && v.abs() < 1e7)
            && self.width > 0.0
            && self.height > 0.0
    }
    pub fn contains(self, r: Self) -> bool {
        r.x >= self.x
            && r.y >= self.y
            && r.x + r.width <= self.x + self.width
            && r.y + r.height <= self.y + self.height
    }
    pub fn overlaps(self, r: Self) -> bool {
        self.x < r.x + r.width
            && r.x < self.x + self.width
            && self.y < r.y + r.height
            && r.y < self.y + self.height
    }
}
pub fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}
pub fn valid_token(token: &Token) -> bool {
    identifier(&token.companion_id)
        && identifier(&token.host_id)
        && identifier(&token.boot_id)
        && identifier(&token.surface_id)
        && token.incarnation > 0
        && token.epoch > 0
}
pub fn decode(bytes: &[u8]) -> Result<Envelope, String> {
    if bytes.len() > MAX_FRAME {
        return Err("frame_limit".into());
    }
    let value: serde_json::Value = strict_json::parse(bytes).map_err(|_| "invalid_json")?;
    if let Some(body) = value.get("body").and_then(|v| v.as_object()) {
        if matches!(
            body.get("type").and_then(|v| v.as_str()),
            Some("recall" | "resync" | "eva_hello" | "desktop_hello" | "withdraw" | "resume")
        ) && body.len() != 1
        {
            return Err("unknown_fields".into());
        }
    }
    // Deserialize directly to structs: serde rejects duplicate and unknown fields, unlike Value roundtrips.
    let e: Envelope = serde_json::from_slice(bytes).map_err(|_| "invalid_schema")?;
    if e.protocol_version != VERSION {
        return Err("protocol_version".into());
    }
    if !identifier(&e.event_id)
        || !identifier(&e.source_id)
        || !identifier(&e.session_id)
        || e.source_seq == 0
    {
        return Err("invalid_envelope".into());
    }
    match &e.body {
        Request::WithdrawIntent {
            intent,
            previous_token,
            ..
        } if *intent == 0
            || *intent >= u64::MAX - 1024
            || previous_token.as_ref().is_some_and(|t| !valid_token(t)) =>
        {
            return Err("invalid_withdrawal_intent".into())
        }
        Request::ResumeWithdrawal { intent, .. } if *intent == 0 || *intent >= u64::MAX - 1024 => {
            return Err("invalid_withdrawal_intent".into())
        }
        Request::ActivateHost { connection_nonce }
        | Request::WithdrawIntent {
            connection_nonce, ..
        }
        | Request::ResumeWithdrawal {
            connection_nonce, ..
        }
        | Request::HostMetadata {
            connection_nonce, ..
        } if !identifier(connection_nonce) => return Err("invalid_connection_nonce".into()),
        Request::HostMetadata {
            admission_epoch,
            metadata,
            ..
        } if *admission_epoch == 0
            || match metadata {
                HostMetadata::Geometry {
                    viewport,
                    candidates,
                    exclusions,
                    ..
                } => {
                    !viewport.valid()
                        || candidates.len() > MAX_REGIONS
                        || exclusions.len() > MAX_REGIONS
                        || candidates.iter().chain(exclusions).any(|r| !r.valid())
                }
                HostMetadata::ContextObserved { context } => !valid_context(context),
                _ => false,
            } =>
        {
            return Err("invalid_host_metadata".into())
        }
        Request::HelloWithFeatures { features, .. } if !valid_host_features(features) => {
            return Err("invalid_host_features".into())
        }
        Request::DialogueHistory {
            request_id,
            context,
            before,
            ..
        } if *request_id == 0
            || !valid_context(context)
            || before.as_ref().is_some_and(|c| !history::cursor_valid(c)) =>
        {
            return Err("invalid_history_request".into())
        }
        Request::VoiceTextReview {
            request_id,
            context,
            scope,
        } if *request_id == 0 || !valid_context(context) || !valid_scope(scope) => {
            return Err("invalid_voice_text_request".into())
        }
        Request::VoiceTextForget {
            request_id,
            context,
        } if *request_id == 0 || !valid_context(context) => {
            return Err("invalid_voice_text_request".into())
        }
        Request::CognitionNoticeDisplayed { notice_id }
        | Request::CognitionNoticeDismissed { notice_id }
            if !identifier(notice_id) =>
        {
            return Err("invalid_notice".into())
        }
        Request::CognitionProposalApprove {
            proposal_id,
            context,
            digest,
        } if !identifier(proposal_id) || !valid_context(context) || !digest_hex(digest) => {
            return Err("invalid_proposal".into())
        }
        Request::CognitionProposalDecline { proposal_id } if !identifier(proposal_id) => {
            return Err("invalid_proposal".into())
        }
        Request::WatchTask { context } | Request::GoalList { context }
            if !valid_context(context) =>
        {
            return Err("invalid_goal_request".into())
        }
        Request::GoalCancel { goal_id } if !identifier(goal_id) => {
            return Err("invalid_goal_request".into())
        }
        Request::AgentHandoffReceipt {
            operation_id,
            outcome,
        } if !identifier(operation_id)
            || match outcome {
                HandoffOutcome::TaskCreated { agent_task_id } => !identifier(agent_task_id),
                HandoffOutcome::Unavailable { reason } => !identifier(reason),
                _ => false,
            } =>
        {
            return Err("invalid_handoff_receipt".into())
        }
        Request::AgentTaskStatus {
            operation_id,
            agent_task_id,
            ..
        } if !identifier(operation_id) || !identifier(agent_task_id) => {
            return Err("invalid_handoff_receipt".into())
        }
        Request::MemoryScopes { after: Some(after) }
            if !(after == "global"
                || after
                    .strip_prefix("project:")
                    .or_else(|| after.strip_prefix("task:"))
                    .is_some_and(identifier)) =>
        {
            return Err("invalid_memory_scope_cursor".into())
        }
        Request::TaskScopes { after: Some(after) } if !identifier(after) => {
            return Err("invalid_scope_cursor".into())
        }
        Request::TaskSharePropose {
            task_id,
            target_host_id,
        } if !identifier(task_id) || !identifier(target_host_id) => {
            return Err("invalid_task_share".into())
        }
        Request::TaskShareApprove { review_id, digest }
        | Request::MemoryScopeApprove {
            review_id, digest, ..
        } if !identifier(review_id)
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            return Err("invalid_scope_review".into())
        }
        Request::MemoryScopeReviewRequested { context, scope }
            if !valid_context(context) || !valid_scope(scope) =>
        {
            return Err("invalid_memory_management_scope".into())
        }
        Request::ProfileName { name }
            if name.trim().is_empty() || name.len() > 64 || name.chars().any(char::is_control) =>
        {
            return Err("invalid_profile_name".into())
        }
        Request::VoiceAcquire { session_id }
        | Request::VoiceRelease { session_id }
        | Request::VoiceContext { session_id, .. }
            if !identifier(session_id) =>
        {
            return Err("invalid_voice_session".into())
        }
        Request::ContextObserved { context }
        | Request::MemoryUse { context, .. }
        | Request::ContextAccessGranted { context, .. }
            if !valid_context(context) =>
        {
            return Err("invalid_context".into())
        }
        Request::MemoryUse { scopes, .. }
            if scopes.len() > 3 || scopes.iter().any(|s| !valid_scope(s)) =>
        {
            return Err("invalid_memory_scopes".into())
        }
        Request::ContextAccessRequested {
            context,
            text,
            source_event_id,
            ..
        } if !valid_context(context)
            || text.is_empty()
            || text.len() > 4096
            || !identifier(source_event_id) =>
        {
            return Err("invalid_context_selection".into())
        }
        Request::MemoryList {
            context,
            scope,
            query,
            before_id,
        } if !valid_context(context)
            || !valid_scope(scope)
            || query.len() > 256
            || before_id.is_some_and(|i| i <= 0) =>
        {
            return Err("invalid_memory_query".into())
        }
        Request::MemoryPropose { context, change }
            if !valid_context(context)
                || match change {
                    MemoryChange::Put {
                        scope,
                        key,
                        value,
                        source_event_id,
                        ..
                    } => {
                        !valid_scope(scope)
                            || !identifier(key)
                            || !identifier(source_event_id)
                            || value.is_empty()
                            || value.len() > 8192
                    }
                    MemoryChange::Delete {
                        id,
                        expected_revision,
                    } => *id <= 0 || *expected_revision == 0,
                    MemoryChange::Clear { scope } => !valid_scope(scope),
                } =>
        {
            return Err("invalid_memory_change".into())
        }
        Request::MemoryApprove {
            context,
            review_id,
            digest,
        } if !valid_context(context)
            || *review_id == 0
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            return Err("invalid_memory_review".into())
        }
        Request::DialogueStart {
            turn_id,
            context,
            text,
            approved_selection,
        } if !identifier(turn_id)
            || !valid_context(context)
            || text.is_empty()
            || text.len() > 8192
            || approved_selection.as_ref().is_some_and(|s| {
                s.context != *context
                    || s.text.len() > 4096
                    || !identifier(&s.consent_id)
                    || !identifier(&s.source_event_id)
            }) =>
        {
            return Err("invalid_dialogue_context_or_limits".into())
        }
        Request::DialogueCancel { turn_id } | Request::DialogueDisplayed { turn_id }
            if !identifier(turn_id) =>
        {
            return Err("invalid_turn".into())
        }
        Request::Hello { host }
        | Request::CompositorHello { host }
        | Request::HelloWithFeatures { host, .. }
            if host.id != e.source_id
                || !identifier(&host.id)
                || !identifier(&host.surface_id)
                || host.incarnation == 0 =>
        {
            return Err("invalid_host".into())
        }
        Request::Appearance { skin, motion }
            if ![
                "cat", "fox", "robot", "ghost", "cursor", "beetle", "night", "stardust",
            ]
            .contains(&skin.as_str())
                || !["full", "calm", "static"].contains(&motion.as_str()) =>
        {
            return Err("invalid_appearance".into())
        }
        Request::TaskObservation { task_id, .. } if !identifier(task_id) => {
            return Err("invalid_task".into())
        }
        Request::Audio {
            turn_id,
            generated_samples,
            played_samples,
            ..
        } if !identifier(turn_id) || played_samples.is_some_and(|p| p > *generated_samples) => {
            return Err("invalid_audio".into())
        }
        Request::Geometry {
            viewport,
            candidates,
            exclusions,
            ..
        } if !viewport.valid()
            || candidates.len() > MAX_REGIONS
            || exclusions.len() > MAX_REGIONS
            || candidates.iter().chain(exclusions).any(|r| !r.valid()) =>
        {
            return Err("invalid_geometry".into())
        }
        _ => {}
    }
    Ok(e)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_unknown_duplicate_and_oversize() {
        let s = r#"{"protocol_version":1,"event_id":"x","source_id":"h","source_seq":1,"boot_id":"b","session_id":"s","body":{"type":"recall"}}"#;
        assert!(decode(s.as_bytes()).is_ok());
        assert!(decode(
            s.replace("\"source_seq\":1", "\"source_seq\":1,\"source_seq\":2")
                .as_bytes()
        )
        .is_err());
        assert!(decode(
            s.replace("\"type\":\"recall\"", "\"type\":\"recall\",\"shell\":\"x\"")
                .as_bytes()
        )
        .is_err());
        assert!(decode(&vec![b' '; MAX_FRAME + 1]).is_err());
    }
}

#[cfg(test)]
mod host_feature_tests {
    use super::*;
    #[test]
    fn notice_feature_extends_the_ordered_chain_only() {
        use HostFeature::*;
        assert!(valid_host_features(&[
            WithdrawalResumeV1,
            DialogueHistoryV1,
            VoiceTextV1,
            CognitionNoticeV1
        ]));
        for skipped in [
            vec![WithdrawalResumeV1, CognitionNoticeV1],
            vec![WithdrawalResumeV1, DialogueHistoryV1, CognitionNoticeV1],
            vec![
                WithdrawalResumeV1,
                DialogueHistoryV1,
                CognitionNoticeV1,
                VoiceTextV1,
            ],
        ] {
            assert!(!valid_host_features(&skipped), "{skipped:?}");
        }
        assert_eq!(
            serde_json::to_value(CognitionNoticeV1).unwrap(),
            "cognition_notice_v1"
        );
    }
    fn notice() -> Notice {
        Notice {
            notice_id: "n-1".into(),
            context: ContextRef {
                host_id: "h".into(),
                pane_id: "p".into(),
                task_id: Some("build".into()),
                project_id: Some("alpha".into()),
                revision: 2,
            },
            task_ref: "task:build".into(),
            mode: "report_status".into(),
            text: "build 失败了。\n第二行".into(),
            explanation: "收到失败回执。".into(),
            evidence_refs: vec!["evt:task:build:3:failed".into()],
            offer: Some("交给 Agent 检查".into()),
        }
    }
    #[test]
    fn notices_are_bounded_and_carry_no_control_bytes() {
        assert!(valid_notice(&notice()));
        let bad: Vec<fn(&mut Notice)> = vec![
            |n| n.notice_id = "n 1".into(),
            |n| n.task_ref = "build".into(),
            |n| n.task_ref = "task:a b".into(),
            |n| n.text = "  ".into(),
            |n| n.text = "x".repeat(MAX_NOTICE_TEXT + 1),
            |n| n.text = "ok\u{1b}[2J".into(),
            |n| n.explanation = "ok\u{9b}m".into(),
            |n| n.evidence_refs = vec!["evt bad".into()],
            |n| n.evidence_refs = vec!["e".into(); MAX_NOTICE_EVIDENCE + 1],
            |n| n.offer = Some("\t".into()),
            |n| n.context.host_id = "h h".into(),
        ];
        for mutate in bad {
            let mut n = notice();
            mutate(&mut n);
            assert!(!valid_notice(&n), "{n:?}");
        }
        let wire = serde_json::json!({"type":"cognition_notice","notice":notice()});
        let back: Reply = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(back, Reply::CognitionNotice { notice: notice() });
        let mut extra = wire;
        extra["notice"]["authorized"] = true.into();
        assert!(serde_json::from_value::<Reply>(extra).is_err());
    }
    fn proposal(change: ProposalChange) -> CognitionProposal {
        CognitionProposal {
            proposal_id: "prop-1".into(),
            digest: "a".repeat(64),
            context: notice().context,
            change,
            explanation: "你让我盯着这个构建。".into(),
        }
    }
    #[test]
    fn proposals_and_goals_are_bounded_and_strict() {
        let watch = ProposalChange::WatchTask {
            task_ref: "task:build".into(),
            notify_on: vec![TaskStatus::Failed, TaskStatus::Succeeded],
            expires_at_ms: 10,
        };
        let remember = ProposalChange::Remember {
            scope: MemoryScope::Global,
            key: "eva-pref-1".into(),
            value: "工作时少聊天，完成再说".into(),
            memory_kind: "preference".into(),
            supersedes: Some(4),
            source_refs: vec!["mem:1".into()],
        };
        assert!(valid_proposal(&proposal(watch.clone())));
        assert!(valid_proposal(&proposal(remember.clone())));
        let mut p = proposal(watch.clone());
        p.digest = "A".repeat(64);
        assert!(!valid_proposal(&p));
        let bad: Vec<ProposalChange> = vec![
            ProposalChange::WatchTask {
                task_ref: "build".into(),
                notify_on: vec![TaskStatus::Failed],
                expires_at_ms: 1,
            },
            ProposalChange::WatchTask {
                task_ref: "task:build".into(),
                notify_on: vec![TaskStatus::Running],
                expires_at_ms: 1,
            },
            ProposalChange::Remember {
                scope: MemoryScope::Global,
                key: "k".into(),
                value: "ok\u{1b}[2J".into(),
                memory_kind: "note".into(),
                supersedes: None,
                source_refs: vec![],
            },
            ProposalChange::Remember {
                scope: MemoryScope::Global,
                key: "k".into(),
                value: "x".repeat(MAX_PROPOSAL_VALUE + 1),
                memory_kind: "note".into(),
                supersedes: None,
                source_refs: vec![],
            },
            ProposalChange::Remember {
                scope: MemoryScope::Global,
                key: "k".into(),
                value: "v".into(),
                memory_kind: "secret".into(),
                supersedes: Some(0),
                source_refs: vec![],
            },
        ];
        for change in bad {
            assert!(!valid_proposal(&proposal(change.clone())), "{change:?}");
        }
        let wire = serde_json::json!({"type":"cognition_proposal","proposal":proposal(remember)});
        let mut extra = wire.clone();
        extra["proposal"]["change"]["approved"] = true.into();
        assert!(serde_json::from_value::<Reply>(extra).is_err());
        assert!(serde_json::from_value::<Reply>(wire).is_ok());
        let goal = GoalView {
            goal_id: "goal-1".into(),
            task_ref: "task:build".into(),
            notify_on: vec![TaskStatus::Succeeded],
            expires_at_ms: 1,
            origin: "explicit".into(),
        };
        assert!(valid_goal(&goal));
        assert!(!valid_goal(&GoalView {
            origin: "model".into(),
            ..goal
        }));
    }
    #[test]
    fn handoffs_and_receipts_are_strict() {
        let h = AgentHandoff {
            operation_id: "op-1".into(),
            context: notice().context,
            title: "检查构建失败".into(),
            goal: "找出 build 失败原因并给出最小修复。\n先只读。".into(),
        };
        assert!(valid_handoff(&h));
        for bad in [
            AgentHandoff {
                title: "a\nb".into(),
                ..h.clone()
            },
            AgentHandoff {
                goal: "x".repeat(MAX_HANDOFF_GOAL + 1),
                ..h.clone()
            },
            AgentHandoff {
                goal: "ok\u{1b}[2J".into(),
                ..h.clone()
            },
            AgentHandoff {
                operation_id: "op 1".into(),
                ..h.clone()
            },
        ] {
            assert!(!valid_handoff(&bad));
        }
        let created = serde_json::json!({"type":"agent_handoff_receipt","operation_id":"op-1",
            "outcome":{"outcome":"task_created","agent_task_id":"5f0c-ab"}});
        assert!(decode(&wire(created)).is_ok());
        let bad_id = serde_json::json!({"type":"agent_handoff_receipt","operation_id":"op-1",
            "outcome":{"outcome":"task_created","agent_task_id":"a b"}});
        assert!(decode(&wire(bad_id)).is_err());
        let status = serde_json::json!({"type":"agent_task_status","operation_id":"op-1",
            "agent_task_id":"t-1","state":"ready_for_review","validation":"passed"});
        assert!(decode(&wire(status)).is_ok());
        let mut extra = serde_json::json!({"type":"agent_task_status","operation_id":"op-1",
            "agent_task_id":"t-1","state":"completed","validation":"passed"});
        extra["merged"] = true.into();
        assert!(decode(&wire(extra)).is_err());
        let chain: Vec<HostFeature> = vec![
            HostFeature::WithdrawalResumeV1,
            HostFeature::DialogueHistoryV1,
            HostFeature::VoiceTextV1,
            HostFeature::CognitionNoticeV1,
            HostFeature::CognitionProposalV1,
            HostFeature::CognitionDelegationV1,
        ];
        assert!(valid_host_features(&chain));
        assert!(!valid_host_features(&[chain[0], chain[5]]));
    }
    #[test]
    fn proposal_and_goal_requests_need_identifiers() {
        let ctx = serde_json::to_value(notice().context).unwrap();
        let hex = "b".repeat(64);
        assert!(decode(&wire(serde_json::json!({"type":"cognition_proposal_approve","proposal_id":"p-1","context":ctx,"digest":hex}))).is_ok());
        assert!(decode(&wire(serde_json::json!({"type":"cognition_proposal_approve","proposal_id":"p-1","context":ctx,"digest":"short"}))).is_err());
        assert!(decode(&wire(
            serde_json::json!({"type":"cognition_proposal_decline","proposal_id":"p 1"})
        ))
        .is_err());
        assert!(decode(&wire(
            serde_json::json!({"type":"watch_task","context":ctx})
        ))
        .is_ok());
        assert!(decode(&wire(serde_json::json!({"type":"goal_list","context":ctx}))).is_ok());
        assert!(decode(&wire(
            serde_json::json!({"type":"goal_cancel","goal_id":"goal-1"})
        ))
        .is_ok());
        assert!(decode(&wire(
            serde_json::json!({"type":"goal_cancel","goal_id":"goal 1"})
        ))
        .is_err());
        assert!(valid_host_features(&[
            HostFeature::WithdrawalResumeV1,
            HostFeature::DialogueHistoryV1,
            HostFeature::VoiceTextV1,
            HostFeature::CognitionNoticeV1,
            HostFeature::CognitionProposalV1
        ]));
        assert!(!valid_host_features(&[
            HostFeature::WithdrawalResumeV1,
            HostFeature::DialogueHistoryV1,
            HostFeature::VoiceTextV1,
            HostFeature::CognitionProposalV1
        ]));
    }
    #[test]
    fn notice_acknowledgements_need_identifiers() {
        for kind in ["cognition_notice_displayed", "cognition_notice_dismissed"] {
            assert!(decode(&wire(serde_json::json!({"type":kind,"notice_id":"n-1"}))).is_ok());
            assert!(decode(&wire(serde_json::json!({"type":kind,"notice_id":"n 1"}))).is_err());
            assert!(decode(&wire(
                serde_json::json!({"type":kind,"notice_id":"n-1","extra":1})
            ))
            .is_err());
        }
    }
    fn wire(body: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"protocol_version":1,"event_id":"e","source_id":"h","source_seq":1,"boot_id":"boot","session_id":"session","body":body})).unwrap()
    }
    fn host() -> serde_json::Value {
        serde_json::json!({"id":"h","incarnation":1,"surface_id":"s","retirement":"native_presented"})
    }
    #[test]
    fn extended_host_handshake_is_opt_in_and_strictly_bounded() {
        assert!(decode(&wire(serde_json::json!({"type":"hello","host":host()}))).is_ok());
        assert!(decode(&wire(serde_json::json!({"type":"hello_with_features","host":host(),"features":["withdrawal_resume_v1"]}))).is_ok());
        for features in [
            serde_json::json!([]),
            serde_json::json!(["withdrawal_resume_v1", "withdrawal_resume_v1"]),
            serde_json::json!(["unknown"]),
        ] {
            assert!(decode(&wire(
                serde_json::json!({"type":"hello_with_features","host":host(),"features":features})
            ))
            .is_err());
        }
        let mut other = host();
        other["id"] = "other".into();
        assert!(decode(&wire(serde_json::json!({"type":"hello_with_features","host":other,"features":["withdrawal_resume_v1"]}))).is_err());
        assert!(decode(&wire(
            serde_json::json!({"type":"hello","host":host(),"features":[]})
        ))
        .is_err());
    }
    #[test]
    fn history_feature_is_ordered_and_requests_are_bounded() {
        assert!(decode(&wire(serde_json::json!({"type":"hello_with_features","host":host(),"features":["withdrawal_resume_v1","dialogue_history_v1"]}))).is_ok());
        for features in [
            serde_json::json!(["dialogue_history_v1"]),
            serde_json::json!(["dialogue_history_v1", "withdrawal_resume_v1"]),
            serde_json::json!([
                "withdrawal_resume_v1",
                "dialogue_history_v1",
                "dialogue_history_v1"
            ]),
        ] {
            assert!(decode(&wire(
                serde_json::json!({"type":"hello_with_features","host":host(),"features":features})
            ))
            .is_err());
        }
        let context = serde_json::json!({"host_id":"h","pane_id":"p","task_id":null,"project_id":"a","revision":1});
        let request = |id: u64, before: serde_json::Value, context: serde_json::Value| {
            wire(
                serde_json::json!({"type":"dialogue_history","request_id":id,"context":context,"scope":"project","before":before}),
            )
        };
        assert!(decode(&request(1, serde_json::Value::Null, context.clone())).is_ok());
        assert!(decode(&request(1, "ab".repeat(32).into(), context.clone())).is_ok());
        assert!(decode(&request(0, serde_json::Value::Null, context.clone())).is_err());
        for cursor in ["AB".repeat(32), "ab".repeat(33), "../".into()] {
            assert!(decode(&request(1, cursor.into(), context.clone())).is_err());
        }
        let mut foreign = context.clone();
        foreign["pane_id"] = "".into();
        assert!(decode(&request(1, serde_json::Value::Null, foreign)).is_err());
        let mut unknown = serde_json::json!({"type":"dialogue_history","request_id":1,"context":context,"scope":"everything","before":null});
        assert!(decode(&wire(unknown.clone())).is_err());
        unknown["scope"] = "voice".into();
        unknown["include_text"] = true.into();
        assert!(decode(&wire(unknown)).is_err());
    }
    #[test]
    fn voice_text_feature_extends_history_and_requests_are_bounded() {
        assert!(valid_host_features(&[
            HostFeature::WithdrawalResumeV1,
            HostFeature::DialogueHistoryV1,
            HostFeature::VoiceTextV1
        ]));
        assert!(!valid_host_features(&[
            HostFeature::WithdrawalResumeV1,
            HostFeature::VoiceTextV1
        ]));
        let context = serde_json::json!({"host_id":"h","pane_id":"p","task_id":null,"project_id":"a","revision":1});
        let review = |id: u64, scope: serde_json::Value| {
            wire(
                serde_json::json!({"type":"voice_text_review","request_id":id,"context":context,"scope":scope}),
            )
        };
        assert!(decode(&review(1, serde_json::json!({"kind":"global"}))).is_ok());
        assert!(decode(&review(1, serde_json::json!({"kind":"project","id":"a"}))).is_ok());
        assert!(decode(&review(0, serde_json::json!({"kind":"global"}))).is_err());
        assert!(decode(&review(1, serde_json::json!({"kind":"project","id":"a b"}))).is_err());
        assert!(decode(&wire(
            serde_json::json!({"type":"voice_text_forget","request_id":1,"context":context})
        ))
        .is_ok());
        assert!(decode(&wire(serde_json::json!({"type":"voice_text_forget","request_id":1,"context":context,"text":"x"}))).is_err());
    }
    #[test]
    fn connection_and_intent_fields_cannot_be_omitted_zero_or_exhausted() {
        for kind in ["withdraw_intent", "resume_withdrawal"] {
            for intent in [0, u64::MAX] {
                let mut body =
                    serde_json::json!({"type":kind,"connection_nonce":"n","intent":intent});
                if kind == "withdraw_intent" {
                    body["previous_token"] = serde_json::Value::Null;
                }
                assert!(decode(&wire(body)).is_err());
            }
        }
        assert!(decode(&wire(
            serde_json::json!({"type":"resume_withdrawal","connection_nonce":"n","intent":1})
        ))
        .is_ok());
        assert!(decode(&wire(
            serde_json::json!({"type":"resume_withdrawal","intent":1})
        ))
        .is_err());
        assert!(decode(&wire(
            serde_json::json!({"type":"activate_host","connection_nonce":"x".repeat(129)})
        ))
        .is_err());
    }
    #[test]
    fn admission_metadata_rejects_bad_epochs_geometry_and_context() {
        let mut body = serde_json::json!({"type":"host_metadata","connection_nonce":"n","admission_epoch":1,"metadata":{"type":"focus","focused":true}});
        assert!(decode(&wire(body.clone())).is_ok());
        body["admission_epoch"] = 0.into();
        assert!(decode(&wire(body.clone())).is_err());
        body["admission_epoch"] = 1.into();
        body["metadata"] = serde_json::json!({"type":"geometry","revision":1,"viewport":{"x":0.,"y":0.,"width":-1.,"height":1.},"candidates":[],"exclusions":[]});
        assert!(decode(&wire(body.clone())).is_err());
        body["metadata"] = serde_json::json!({"type":"context_observed","context":{"host_id":"","pane_id":"p","task_id":null,"project_id":null,"revision":1}});
        assert!(decode(&wire(body.clone())).is_err());
        body["metadata"] =
            serde_json::json!({"type":"resume_withdrawal","intent":1,"connection_nonce":"n"});
        assert!(
            decode(&wire(body)).is_err(),
            "metadata cannot wrap ordered control or another scope"
        );
    }
}
