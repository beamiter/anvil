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
    )
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
    /// Sent to negotiated hosts when the desktop locks or becomes unknown.
    /// A host must clear every displayed history page.
    DialogueHistoryWithdrawn {
        reason: String,
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
