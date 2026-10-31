//! One GUI-owned Eva adapter. Transport work never runs on the rendering thread.
//! A native presentation acknowledgement is not a compositor scanout guarantee.
pub mod dialogue_save;
pub mod history;
pub mod pty_session;
pub mod shell_identity;
pub mod voice_text;
pub use companion_core::geometry::{choose, Placement};
pub use companion_protocol::*;
pub use dialogue_save::DialogueSave;
pub use eva_character_atlas as atlas;
use std::{
    sync::{mpsc, Arc},
    time::Duration,
};

/// Explicit opt-in. A failed shared connection must never revive the legacy pet.
/// Fallback task identity when no globally unique execution ID is available.
/// A restored pane UUID plus a reset terminal counter alone is NOT a task ID.
pub fn host_scoped_task_id(host_id: &str, pane_id: &str, sequence: u64) -> String {
    format!("{host_id}-{pane_id}-{sequence}")
}

pub fn shared_mode_requested() -> bool {
    std::env::var("EVA_COMPANION_MODE").as_deref() == Ok("shared")
}

/// A cancelling Escape belongs to the dialog through release and focus loss.
/// Unknown/synthetic blur release is conservative: consume one focused cycle
/// rather than forwarding a held repeat into newly restored terminal work.
#[derive(Debug, Default)]
pub struct CancelKeyLease {
    state: CancelKeyState,
}
#[derive(Debug, Default)]
enum CancelKeyState {
    #[default]
    Idle,
    Held,
    Released,
    AwaitingRelease,
}
#[derive(Debug, PartialEq, Eq)]
pub enum CancelKeyAction {
    Pass,
    Cancel,
    Suppress,
}
impl CancelKeyLease {
    pub fn press(&mut self, available: bool, repeat: bool) -> CancelKeyAction {
        match self.state {
            CancelKeyState::Held | CancelKeyState::AwaitingRelease => {
                return CancelKeyAction::Suppress
            }
            CancelKeyState::Released if repeat => {
                self.state = CancelKeyState::Held;
                return CancelKeyAction::Suppress;
            }
            _ => self.state = CancelKeyState::Idle,
        }
        if available && !repeat {
            self.state = CancelKeyState::Held;
            CancelKeyAction::Cancel
        } else {
            CancelKeyAction::Pass
        }
    }
    pub fn release(&mut self, focused: bool) -> bool {
        if matches!(self.state, CancelKeyState::Idle) {
            return false;
        }
        self.state = if focused {
            CancelKeyState::Released
        } else {
            CancelKeyState::AwaitingRelease
        };
        true
    }
    pub fn focus_lost(&mut self) {
        if !matches!(self.state, CancelKeyState::Idle) {
            self.state = CancelKeyState::AwaitingRelease;
        }
    }
    pub fn other_input(&mut self) {
        if matches!(self.state, CancelKeyState::Released) {
            self.state = CancelKeyState::Idle;
        }
    }
}

#[derive(Debug)]
struct WithdrawalAttempt {
    intent: u64,
    previous_token: Option<Token>,
    sent: bool,
    ready: bool,
    resume_requested: bool,
    resume_sent: bool,
}
#[derive(Debug)]
pub struct HostGate {
    modern: bool,
    connection_nonce: Option<String>,
    admitted: bool,
    admission_epoch: u64,
    admission_generation: u64,
    activation_sent: bool,
    last_intent: u64,
    withdrawal: Option<WithdrawalAttempt>,
    reclose_after_resume: bool,
    host: Host,
    boot: Option<String>,
    token: Option<Token>,
    retired: Option<(Token, u64)>,
    retirement_submitted: bool,
    retirement_accepted: Option<Token>,
    withdrawal_accepted: bool,
    drawn: Option<(Token, u64)>,
    presented: Option<Token>,
    frame: u64,
    revision: u64,
    connection_generation: u64,
    pose: String,
    skin: String,
    motion: String,
    status: String,
    quiet: bool,
    night: bool,
    operation_error: Option<String>,
    history_supported: bool,
    voice_text_supported: bool,
}
impl HostGate {
    pub fn new(host: Host) -> Self {
        Self {
            modern: false,
            connection_nonce: None,
            admitted: false,
            admission_epoch: 0,
            admission_generation: 0,
            activation_sent: false,
            last_intent: 0,
            withdrawal: None,
            reclose_after_resume: false,
            host,
            boot: None,
            token: None,
            retired: None,
            retirement_submitted: false,
            retirement_accepted: None,
            withdrawal_accepted: false,
            drawn: None,
            presented: None,
            frame: 0,
            revision: 0,
            connection_generation: 0,
            pose: "idle".into(),
            skin: "cat".into(),
            motion: "calm".into(),
            status: "Eva disconnected — shared body hidden".into(),
            quiet: false,
            night: false,
            operation_error: None,
            history_supported: false,
            voice_text_supported: false,
        }
    }
    /// The current connection negotiated `VoiceTextV1`.
    pub fn voice_text_supported(&self) -> bool {
        self.voice_text_supported
    }
    /// The current connection negotiated `DialogueHistoryV1`.
    pub fn history_supported(&self) -> bool {
        self.history_supported
    }
    pub fn withdrawal_accepted(&self) -> bool {
        if self.modern {
            self.withdrawal
                .as_ref()
                .is_some_and(|w| w.ready && !w.resume_requested)
        } else {
            self.withdrawal_accepted
        }
    }
    pub fn new_modern(host: Host) -> Self {
        Self {
            modern: true,
            ..Self::new(host)
        }
    }
    pub fn admission_generation(&self) -> u64 {
        self.admission_generation
    }
    pub fn resuming_withdrawal(&self) -> bool {
        self.withdrawal.as_ref().is_some_and(|w| w.resume_requested)
    }
    pub fn withdrawal_pending(&self) -> bool {
        self.withdrawal.is_some()
    }
    fn begin_withdrawal(&mut self) {
        if self.withdrawal.as_ref().is_some_and(|w| w.resume_requested) {
            self.reclose_after_resume = true;
            return;
        }
        if self.withdrawal.is_none() {
            let Some(intent) = self
                .last_intent
                .checked_add(1)
                .filter(|n| *n < u64::MAX - 1024)
            else {
                self.operation_error = Some("withdrawal intent counter exhausted".into());
                return;
            };
            self.last_intent = intent;
            self.withdrawal = Some(WithdrawalAttempt {
                intent,
                previous_token: self.possible_body_token().cloned(),
                sent: false,
                ready: false,
                resume_requested: false,
                resume_sent: false,
            });
        }
        self.drawn = None;
        self.presented = None;
    }
    fn cancel_withdrawal(&mut self) {
        self.reclose_after_resume = false;
        if let Some(withdrawal) = self.withdrawal.as_mut() {
            withdrawal.resume_requested = true;
        }
    }
    fn next_control(&mut self) -> Option<Request> {
        let nonce = self.connection_nonce.clone()?;
        if let Some(withdrawal) = self.withdrawal.as_mut() {
            if !withdrawal.sent {
                withdrawal.sent = true;
                return Some(Request::WithdrawIntent {
                    connection_nonce: nonce,
                    intent: withdrawal.intent,
                    previous_token: withdrawal.previous_token.clone(),
                });
            }
            if withdrawal.ready && withdrawal.resume_requested && !withdrawal.resume_sent {
                withdrawal.resume_sent = true;
                return Some(Request::ResumeWithdrawal {
                    connection_nonce: nonce,
                    intent: withdrawal.intent,
                });
            }
        } else if !self.admitted
            && !self.activation_sent
            && self.retired.as_ref().is_none_or(|(token, _)| {
                self.retirement_submitted && self.retirement_accepted(token)
            })
        {
            self.activation_sent = true;
            return Some(Request::ActivateHost {
                connection_nonce: nonce,
            });
        }
        None
    }
    fn modern_ack_matches(&self, nonce: &str, host: &str, incarnation: u64, surface: &str) -> bool {
        self.modern
            && self.connection_nonce.as_deref() == Some(nonce)
            && self.host.id == host
            && self.host.incarnation == incarnation
            && self.host.surface_id == surface
    }
    fn retirement_matches(&self, token: Option<&Token>) -> bool {
        self.token.is_none()
            && match token {
                Some(token) => self.retirement_submitted && self.retirement_accepted(token),
                None => self.possible_body_token().is_none(),
            }
    }
    pub fn retirement_accepted(&self, token: &Token) -> bool {
        self.retirement_accepted.as_ref() == Some(token)
    }
    pub fn possible_body_token(&self) -> Option<&Token> {
        self.token.as_ref().or_else(|| {
            self.retired
                .as_ref()
                .map(|(t, _)| t)
                .filter(|t| !self.retirement_accepted(t))
        })
    }
    pub fn host_id(&self) -> &str {
        &self.host.id
    }
    pub fn connection_generation(&self) -> u64 {
        self.connection_generation
    }
    /// Latest accepted control connection as observed by poll. This is not
    /// admission permission or evidence that any possible body was removed.
    pub fn has_connection(&self) -> bool {
        self.boot.is_some() && (!self.modern || self.connection_nonce.is_some())
    }
    pub fn token(&self) -> Option<&Token> {
        if self.modern && (!self.admitted || self.withdrawal.is_some()) {
            return None;
        }
        self.token.as_ref()
    }
    pub fn pose(&self) -> &str {
        &self.pose
    }
    pub fn skin(&self) -> &str {
        &self.skin
    }
    pub fn motion(&self) -> &str {
        &self.motion
    }
    pub fn quiet(&self) -> bool {
        self.quiet
    }
    pub fn night(&self) -> bool {
        self.night
    }
    pub fn operation_error(&self) -> Option<&str> {
        self.operation_error.as_deref()
    }
    pub fn status(&self) -> &str {
        &self.status
    }
    pub fn pending_retirement(&self) -> bool {
        self.retired.is_some() && !self.retirement_submitted && self.boot.is_some()
    }
    /// Identity of the still-required native clearance, including its frame
    /// boundary. A renderer must re-arm when this changes, even if it never
    /// observed an intervening frame with `pending_retirement() == false`.
    pub fn pending_retirement_boundary(&self) -> Option<(&Token, u64)> {
        if !self.pending_retirement() {
            return None;
        }
        self.retired.as_ref().map(|(token, after)| (token, *after))
    }
    pub fn begin_frame(&mut self, serial: u64) {
        self.frame = self.frame.max(serial);
    }
    fn matches_host(&self, t: &Token) -> bool {
        t.host_id == self.host.id
            && t.incarnation == self.host.incarnation
            && t.surface_id == self.host.surface_id
    }
    pub fn accepts_callback(&self, t: &Token) -> bool {
        self.token() == Some(t) && self.boot.as_deref() == Some(t.boot_id.as_str())
    }
    pub fn body_submitted(&mut self, serial: u64) {
        if let Some(token) = self.token.clone() {
            self.drawn = Some((token, serial));
        }
    }
    /// The admitted body still needs a native presentation receipt. This is
    /// separate from drawing and must not be cleared by a timer or a grant.
    pub fn body_presentation_pending(&self) -> bool {
        self.retired.is_none()
            && self
                .token()
                .is_some_and(|token| self.presented.as_ref() != Some(token))
    }
    pub fn disconnect(&mut self) {
        self.history_supported = false;
        self.voice_text_supported = false;
        self.connection_nonce = None;
        self.admitted = false;
        self.admission_epoch = 0;
        self.activation_sent = false;
        if let Some(withdrawal) = self.withdrawal.as_mut() {
            withdrawal.sent = false;
            withdrawal.ready = false;
            withdrawal.resume_sent = false;
        }
        self.drawn = None;
        self.presented = None;
        if let Some(t) = self.token.take() {
            self.retired = Some((t, self.frame));
        }
        self.retirement_submitted = false;
        self.boot = None;
        self.status = "Eva disconnected — shared body hidden".into();
    }
    pub fn receive(&mut self, reply: Reply) -> Option<Request> {
        match reply {
            Reply::HostAccepted {
                boot_id,
                features,
                connection_nonce,
                ..
            } if self.modern => {
                if !valid_host_features(&features) || !identifier(&connection_nonce) {
                    self.disconnect();
                    self.status = "Eva host capabilities unavailable".into();
                    return None;
                }
                self.history_supported = features.contains(&HostFeature::DialogueHistoryV1);
                self.voice_text_supported = features.contains(&HostFeature::VoiceTextV1);
                self.connection_generation = self.connection_generation.saturating_add(1);
                self.admission_generation = self.admission_generation.saturating_add(1);
                self.admitted = false;
                self.admission_epoch = 0;
                self.activation_sent = false;
                self.connection_nonce = Some(connection_nonce);
                if let Some((_, after)) = self.retired.as_mut() {
                    // Reconcile the old exact token with a clearance begun
                    // after this new connection was accepted, never by
                    // relabelling a delayed old native callback.
                    *after = self.frame;
                    self.retirement_submitted = false;
                }
                if let Some(withdrawal) = self.withdrawal.as_mut() {
                    withdrawal.sent = false;
                    withdrawal.ready = false;
                    withdrawal.resume_sent = false;
                }
                if self.boot.as_ref() != Some(&boot_id) {
                    self.token = None;
                    self.revision = 0;
                }
                self.boot = Some(boot_id);
                self.status = "Eva connected — verifying host admission".into();
            }
            Reply::HostActivated {
                connection_nonce,
                host_id,
                incarnation,
                surface_id,
                admission_epoch,
            } if self.modern_ack_matches(&connection_nonce, &host_id, incarnation, &surface_id)
                && self.activation_sent
                && admission_epoch > 0
                && admission_epoch >= self.admission_epoch =>
            {
                if !self.admitted {
                    self.admitted = true;
                    self.admission_epoch = admission_epoch;
                    self.admission_generation = self.admission_generation.saturating_add(1);
                    self.status = "Eva connected — awaiting residence".into();
                }
            }
            Reply::WithdrawalReady {
                connection_nonce,
                host_id,
                incarnation,
                surface_id,
                intent,
                retired_token,
            } if self.modern_ack_matches(&connection_nonce, &host_id, incarnation, &surface_id)
                && self.retirement_matches(retired_token.as_ref()) =>
            {
                if let Some(withdrawal) = self
                    .withdrawal
                    .as_mut()
                    .filter(|w| w.intent == intent && w.previous_token == retired_token)
                {
                    withdrawal.ready = true;
                }
            }
            Reply::WithdrawalResumed {
                connection_nonce,
                host_id,
                incarnation,
                surface_id,
                intent,
                retired_token,
                admission_epoch,
            } if self.modern_ack_matches(&connection_nonce, &host_id, incarnation, &surface_id)
                && self.retirement_matches(retired_token.as_ref())
                && self.withdrawal.as_ref().is_some_and(|w| {
                    w.intent == intent
                        && w.ready
                        && w.resume_requested
                        && w.resume_sent
                        && w.previous_token == retired_token
                })
                && admission_epoch > self.admission_epoch =>
            {
                self.withdrawal = None;
                self.admitted = true;
                self.admission_epoch = admission_epoch;
                self.activation_sent = false;
                self.drawn = None;
                self.presented = None;
                self.admission_generation = self.admission_generation.saturating_add(1);
                self.status = "Eva connected — window kept open".into();
                if self.reclose_after_resume {
                    self.reclose_after_resume = false;
                    self.begin_withdrawal();
                }
            }
            Reply::WithdrawalAccepted {
                host_id,
                incarnation,
            } => {
                if !self.modern && host_id == self.host.id && incarnation == self.host.incarnation {
                    self.withdrawal_accepted = true;
                }
            }
            Reply::RetirementAccepted { token } => {
                if self.retired.as_ref().is_some_and(|(old, _)| old == &token)
                    && self.retirement_submitted
                {
                    self.retirement_accepted = Some(token);
                }
            }
            Reply::Comfort { quiet, night } => {
                self.quiet = quiet;
                self.night = night;
            }
            Reply::OperationError { operation, reason } => {
                self.operation_error = Some(format!("{operation}: {reason}"));
            }
            Reply::Appearance { skin, motion } => {
                self.skin = skin;
                self.motion = motion;
            }
            Reply::Accepted { boot_id, .. } => {
                if self.modern {
                    self.disconnect();
                    self.status = "Eva server does not support negotiated host admission".into();
                    return None;
                }
                self.withdrawal_accepted = false;
                self.connection_generation = self.connection_generation.saturating_add(1);
                if self.boot.as_ref() != Some(&boot_id) {
                    self.token = None;
                    self.revision = 0;
                }
                self.boot = Some(boot_id);
                self.status = "Eva connected — awaiting residence".into();
            }
            Reply::Rejected { reason } => {
                self.disconnect();
                self.status = format!("Eva unavailable: {reason}");
            }
            Reply::Prepare { epoch } => {
                if self.boot.is_some()
                    && (!self.modern || self.admitted)
                    && self.host.retirement != RetirementCapability::None
                    && (self.retired.is_none() || self.retirement_submitted)
                {
                    return Some(Request::Prepared { epoch });
                }
            }
            Reply::Grant { token, revision } => {
                if self.host.retirement != RetirementCapability::None
                    && (!self.modern
                        || (self.connection_nonce.is_some()
                            && (self.admitted || self.withdrawal.is_some())))
                    && self.matches_host(&token)
                    && self.boot.as_deref() == Some(token.boot_id.as_str())
                    && revision >= self.revision
                    && (self.retired.is_none() || self.retirement_submitted)
                    && self.retired.as_ref().is_none_or(|(old, _)| {
                        old.boot_id != token.boot_id || token.epoch > old.epoch
                    })
                    && self
                        .token
                        .as_ref()
                        .is_none_or(|current| token.epoch >= current.epoch)
                {
                    self.revision = revision;
                    self.withdrawal_accepted = false;
                    self.retired = None;
                    self.retirement_submitted = false;
                    self.token = Some(token);
                    self.status = "Eva · waiting".into();
                }
            }
            Reply::Revoke { token } => {
                if self.matches_host(&token)
                    && (self.token.as_ref() == Some(&token)
                        || self.retired.as_ref().is_some_and(|(old, _)| old == &token))
                {
                    if self.retired.as_ref().is_some_and(|(old, _)| old == &token)
                        && self.token.is_none()
                    {
                        // Reliable-stream retries can arrive after a genuine
                        // native frame was requested or submitted. Nothing
                        // could have redrawn this retired token meanwhile.
                        // Keep its exact obligation and original frame bound.
                        return self.next_control();
                    }
                    self.token = None;
                    self.drawn = None;
                    self.presented = None;
                    self.retired = Some((token, self.frame));
                    if let Some(withdrawal) = self.withdrawal.as_mut() {
                        withdrawal.previous_token =
                            self.retired.as_ref().map(|(token, _)| token.clone());
                    }
                    self.retirement_submitted = false;
                    self.status = "Eva moving — awaiting cleared frame".into();
                }
            }
            Reply::Snapshot {
                revision,
                token,
                pose,
                recovery_blocked,
                ..
            } if revision >= self.revision => {
                self.revision = revision;
                self.pose = pose;
                if token.as_ref() != self.token.as_ref() {
                    self.token = None;
                }
                if recovery_blocked {
                    self.status =
                        "Eva recovery blocked — old surface needs retirement proof".into();
                }
            }
            _ => {}
        }
        self.next_control()
    }
    /// Call ONLY after the native renderer successfully presented the cleared
    /// frame. A tick, message receipt, layout pass, GPU enqueue or heartbeat is
    /// not evidence. The serial must identify a frame begun after revocation.
    pub fn frame_presented(&mut self, serial: u64) -> Option<Request> {
        if self.retired.is_none() {
            let (token, at) = self.drawn.as_ref()?;
            if serial >= *at
                && self.accepts_callback(token)
                && self.presented.as_ref() != Some(token)
            {
                let token = token.clone();
                self.presented = Some(token.clone());
                return Some(Request::Presented { token });
            }
            return None;
        }
        let (token, after) = self.retired.as_ref()?;
        if serial <= *after
            || self.host.retirement == RetirementCapability::None
            || self.retirement_submitted
            || self.boot.is_none()
        {
            return None;
        }
        let token = token.clone();
        self.retirement_submitted = true;
        Some(Request::Retired {
            token,
            presentation_serial: serial,
        })
    }
}

fn requires_reconnect(reply: &Reply) -> bool {
    matches!(reply, Reply::Rejected { .. })
}

// The channel has a fixed 64-event bound; keeping replies inline avoids a heap
// allocation for every event. The rare disconnect marker shares that bounded storage.
#[allow(clippy::large_enum_variant)]
enum WorkerEvent {
    Reply(Reply),
    Disconnected,
}
/// GUI owner; do not create one per split, block, history entry or preview.
pub struct SharedHost {
    pub gate: HostGate,
    tx: mpsc::SyncSender<Request>,
    rx: mpsc::Receiver<WorkerEvent>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    failed: bool,
    metadata: Arc<std::sync::Mutex<[Option<Request>; 3]>>,
    dialogue: std::collections::VecDeque<Reply>,
    memory: std::collections::VecDeque<Reply>,
    context_access: std::collections::VecDeque<Reply>,
    /// Read-only history page for this window; see [`history::HistoryView`].
    pub history: history::HistoryView,
    /// Outcome of explicit voice-text requests; see [`voice_text::VoiceTextStatus`].
    pub voice_text: voice_text::VoiceTextStatus,
}
const FEATURES: [HostFeature; 3] = [
    HostFeature::WithdrawalResumeV1,
    HostFeature::DialogueHistoryV1,
    HostFeature::VoiceTextV1,
];
/// The first `level` features a modern host asks for. An older coordinator
/// rejects a longer list as a schema error before admission; the worker then
/// retries one level shorter and the missing features stay unavailable.
fn worker_features(level: usize) -> Vec<HostFeature> {
    FEATURES[..level.clamp(1, FEATURES.len())].to_vec()
}
fn predates_features(replies: &[Reply], accepted: bool) -> bool {
    !accepted
        && replies.iter().any(|reply| {
            matches!(reply, Reply::Rejected { reason }
                if reason == "invalid_schema" || reason == "invalid_host_features")
        })
}
impl SharedHost {
    pub fn from_env(
        app: &str,
        capability: RetirementCapability,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let host = Host {
            id: format!("{app}-{}", uuid::Uuid::new_v4()),
            incarnation: 1,
            surface_id: "eva-overlay".into(),
            retirement: capability,
        };
        let (tx, requests) = mpsc::sync_channel(64);
        let (events, rx) = mpsc::sync_channel(64);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let metadata = Arc::new(std::sync::Mutex::new([None, None, None]));
        let worker_metadata = metadata.clone();
        let worker_host = host.clone();
        let wake = Arc::new(wake);
        let spawned = std::thread::Builder::new()
            .name(format!("{app}-eva-ipc"))
            .spawn(move || {
                let session = std::env::var("XDG_SESSION_ID").unwrap_or_default();
                let path = std::env::var_os("EVA_COMPANION_SOCKET")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::path::PathBuf::from(
                            std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default(),
                        )
                        .join("jcompanion/eva.sock")
                    });
                let mut disconnected_notice = false;
                let mut level = FEATURES.len();
                while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                    let result = (|| -> std::io::Result<()> {
                        let mut client = companion_client::Client::connect_with_features(
                            &path,
                            &session,
                            worker_host.clone(),
                            worker_features(level),
                        )?;
                        let mut accepted = false;
                        while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                            if !client.boot_id().is_empty() {
                                let pending = if let Ok(mut latest) = worker_metadata.lock() {
                                    std::mem::take(&mut *latest)
                                } else {
                                    return Err(std::io::Error::other("metadata lock poisoned"));
                                };
                                // Geometry precedes focus, but task results and user intents remain ordered controls.
                                let [focus, geometry, input] = pending;
                                for request in [geometry, focus, input].into_iter().flatten() {
                                    client.send(request)?;
                                }
                                while let Ok(request) = requests.try_recv() {
                                    client.send(request)?;
                                }
                            }
                            let replies = client.poll()?;
                            if !replies.is_empty() {
                                if level > 1 && predates_features(&replies, accepted) {
                                    level -= 1;
                                }
                                accepted |= replies
                                    .iter()
                                    .any(|r| matches!(r, Reply::HostAccepted { .. }));
                                let reconnect = replies.iter().any(requires_reconnect);
                                for reply in replies {
                                    events.try_send(WorkerEvent::Reply(reply)).map_err(|_| {
                                        std::io::Error::other("GUI event queue saturated")
                                    })?;
                                }
                                disconnected_notice = false;
                                wake();
                                if reconnect {
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::ConnectionAborted,
                                        "daemon rejected session; reconnect required",
                                    ));
                                }
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Ok(())
                    })();
                    if result.is_ok() {
                        break;
                    }
                    if !disconnected_notice {
                        // If delivery itself fails, terminate and let the GUI observe channel closure.
                        if events.try_send(WorkerEvent::Disconnected).is_err() {
                            wake();
                            break;
                        }
                        wake();
                        disconnected_notice = true;
                    }
                    for _ in 0..20 {
                        if stopping.load(std::sync::atomic::Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            });
        Self {
            gate: HostGate::new_modern(host),
            tx,
            rx,
            stop,
            failed: spawned.is_err(),
            metadata,
            dialogue: Default::default(),
            memory: Default::default(),
            context_access: Default::default(),
            history: Default::default(),
            voice_text: Default::default(),
        }
    }
    /// Asks Eva, through the coordinator, for its latest retained spoken reply.
    /// A ready review arrives as an ordinary `MemoryReview` for `context`.
    pub fn request_voice_text_review(
        &mut self,
        context: &ContextRef,
        scope: MemoryScope,
    ) -> Result<(), &'static str> {
        if self.failed || !self.gate.admitted {
            return Err("Eva is not connected.");
        }
        if !self.gate.voice_text_supported {
            return Err("Update the Eva coordinator to review spoken replies.");
        }
        let request_id = self.voice_text.begin()?;
        self.send(Request::VoiceTextReview {
            request_id,
            context: context.clone(),
            scope,
        });
        Ok(())
    }
    /// Asks Eva to drop every retained spoken reply now.
    pub fn forget_voice_text(&mut self, context: &ContextRef) -> Result<(), &'static str> {
        if self.failed || !self.gate.admitted {
            return Err("Eva is not connected.");
        }
        if !self.gate.voice_text_supported {
            return Err("Update the Eva coordinator to manage spoken replies.");
        }
        let request_id = self.voice_text.begin()?;
        self.send(Request::VoiceTextForget {
            request_id,
            context: context.clone(),
        });
        Ok(())
    }
    /// Sends one explicit history page request for the window's current,
    /// already registered context.
    pub fn request_history(
        &mut self,
        context: &ContextRef,
        older: bool,
    ) -> Result<(), &'static str> {
        if self.failed || !self.gate.admitted {
            return Err("Eva is not connected. History is unavailable.");
        }
        if !self.gate.history_supported {
            return Err("Update the Eva coordinator to view history.");
        }
        let request = self.history.request(context, older)?;
        self.send(request);
        Ok(())
    }
    pub fn begin_frame(&mut self, serial: u64) {
        self.gate.begin_frame(serial)
    }
    /// Register actual body/compact drawing in this native frame (not a grant).
    pub fn body_submitted(&mut self, serial: u64) {
        self.gate.body_submitted(serial)
    }
    pub fn poll(&mut self) -> bool {
        if self.failed {
            return false;
        }
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(WorkerEvent::Reply(r)) => {
                    changed = true;
                    if self.history.observe(&r) || self.voice_text.observe(&r) {
                        continue;
                    }
                    if matches!(&r, Reply::ContextAccessReview { .. }) {
                        if self.context_access.len() >= 8 {
                            self.gate.disconnect();
                            self.failed = true;
                            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            return true;
                        }
                        self.context_access.push_back(r);
                        continue;
                    }
                    if matches!(&r, Reply::Dialogue { .. }) {
                        if self.dialogue.len() >= 32 {
                            self.gate.disconnect();
                            self.failed = true;
                            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            return true;
                        }
                        self.dialogue.push_back(r);
                        continue;
                    }
                    if matches!(
                        &r,
                        Reply::MemoryList { .. }
                            | Reply::MemoryReview { .. }
                            | Reply::MemoryResult { .. }
                            | Reply::MemoryScopeReview { .. }
                            | Reply::TaskScopes { .. }
                            | Reply::MemoryScopes { .. }
                    ) {
                        if self.memory.len() >= 32 {
                            self.gate.disconnect();
                            self.failed = true;
                            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                            return true;
                        }
                        self.memory.push_back(r);
                        continue;
                    }
                    if let Some(r) = self.gate.receive(r) {
                        self.send(r);
                        if self.failed {
                            return true;
                        }
                    }
                }
                Ok(WorkerEvent::Disconnected) => {
                    self.gate.disconnect();
                    self.history.disconnected();
                    changed = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.gate.disconnect();
                    self.history.disconnected();
                    self.failed = true;
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if !self.gate.history_supported || self.failed {
            // Every disconnect path, including bounded-queue failures.
            self.history.disconnected();
        }
        if !self.gate.voice_text_supported || self.failed {
            self.voice_text.disconnected();
        }
        self.flush_controls();
        changed
    }
    pub fn take_context_access(&mut self) -> Option<Reply> {
        self.context_access.pop_front()
    }
    pub fn take_dialogue(&mut self) -> Option<Reply> {
        self.dialogue.pop_front()
    }
    pub fn take_memory(&mut self) -> Option<Reply> {
        self.memory.pop_front()
    }
    pub fn send(&mut self, request: Request) {
        if self.failed {
            return;
        }
        if !self.gate.modern {
            self.send_control(request);
            return;
        }
        match request {
            Request::Withdraw => {
                self.gate.begin_withdrawal();
                self.flush_controls();
            }
            Request::Focus { focused } => self.send_metadata(HostMetadata::Focus { focused }),
            Request::Input { active, selecting } => {
                self.send_metadata(HostMetadata::Input { active, selecting })
            }
            Request::Geometry {
                revision,
                viewport,
                candidates,
                exclusions,
            } => self.send_metadata(HostMetadata::Geometry {
                revision,
                viewport,
                candidates,
                exclusions,
            }),
            Request::ContextObserved { context } => {
                self.send_metadata(HostMetadata::ContextObserved { context })
            }
            request => self.send_control(request),
        }
    }
    fn send_metadata(&mut self, metadata: HostMetadata) {
        if !self.gate.admitted || self.gate.withdrawal.is_some() {
            return;
        }
        let Some(connection_nonce) = self.gate.connection_nonce.clone() else {
            return;
        };
        self.send_control(Request::HostMetadata {
            connection_nonce,
            admission_epoch: self.gate.admission_epoch,
            metadata,
        });
    }
    fn send_control(&mut self, request: Request) {
        let slot = match &request {
            Request::Focus { .. }
            | Request::HostMetadata {
                metadata: HostMetadata::Focus { .. },
                ..
            } => Some(0),
            Request::Geometry { .. }
            | Request::HostMetadata {
                metadata: HostMetadata::Geometry { .. },
                ..
            } => Some(1),
            Request::Input { .. }
            | Request::HostMetadata {
                metadata: HostMetadata::Input { .. },
                ..
            } => Some(2),
            _ => None,
        };
        if let Some(slot) = slot {
            if let Ok(mut latest) = self.metadata.lock() {
                latest[slot] = Some(request);
                return;
            }
        }
        if self.tx.try_send(request).is_err() {
            self.gate.disconnect();
            self.failed = true;
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    pub fn frame_presented(&mut self, serial: u64) {
        if let Some(request) = self.gate.frame_presented(serial) {
            self.send(request)
        }
        self.flush_controls();
    }
    fn flush_controls(&mut self) {
        while !self.failed {
            let Some(request) = self.gate.next_control() else {
                break;
            };
            self.send(request);
        }
    }
    /// Cancel GUI exit now; the original clearance continues until exact
    /// retirement is accepted, followed by the explicit resume acknowledgement.
    pub fn cancel_withdrawal(&mut self) {
        if self.failed {
            return;
        }
        self.gate.cancel_withdrawal();
        self.flush_controls();
    }
}
impl Drop for SharedHost {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gate() -> HostGate {
        HostGate::new(Host {
            id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            retirement: RetirementCapability::NativePresented,
        })
    }
    fn token() -> Token {
        Token {
            companion_id: "eva".into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            boot_id: "b".into(),
            epoch: 1,
        }
    }
    fn active() -> HostGate {
        let mut g = gate();
        g.receive(Reply::Accepted {
            boot_id: "b".into(),
            companion_id: "eva".into(),
            session_id: "s".into(),
        });
        g.receive(Reply::Grant {
            token: token(),
            revision: 1,
        });
        g
    }
    #[test]
    fn revoked_pixels_need_later_real_frame() {
        let mut g = active();
        g.begin_frame(5);
        g.receive(Reply::Revoke { token: token() });
        assert!(g.token().is_none());
        assert!(g.frame_presented(5).is_none());
        assert!(matches!(
            g.frame_presented(6),
            Some(Request::Retired { .. })
        ));
        assert!(g.frame_presented(7).is_none());
    }
    #[test]
    fn disconnect_never_falls_back() {
        let mut g = active();
        g.disconnect();
        assert!(g.token().is_none());
        assert!(!g.accepts_callback(&token()));
        assert!(g.retired.is_some());
    }
    #[test]
    fn stale_callbacks_and_grants_rejected() {
        let mut g = active();
        let mut t = token();
        t.boot_id = "old".into();
        g.receive(Reply::Grant {
            token: t.clone(),
            revision: 9,
        });
        assert!(!g.accepts_callback(&t));
        assert_eq!(g.token(), Some(&token()));
    }
    #[test]
    fn no_presentation_capability_never_proves_retirement() {
        let mut g = active();
        g.host.retirement = RetirementCapability::None;
        g.receive(Reply::Revoke { token: token() });
        assert!(g.frame_presented(999).is_none());
    }
    #[test]
    fn reconnect_retires_old_boot_without_reviving_body() {
        let mut g = active();
        g.begin_frame(4);
        g.disconnect();
        assert!(g.frame_presented(5).is_none());
        g.receive(Reply::Accepted {
            boot_id: "new-boot".into(),
            companion_id: "eva".into(),
            session_id: "s".into(),
        });
        g.receive(Reply::Revoke { token: token() });
        assert!(!g.accepts_callback(&token()));
        assert!(
            matches!(g.frame_presented(6),Some(Request::Retired{token:Token{boot_id,..},..}) if boot_id=="b")
        );
    }
    #[test]
    fn completed_retirement_allows_later_prepare_not_old_callbacks() {
        let mut g = active();
        g.receive(Reply::Revoke { token: token() });
        g.frame_presented(1).unwrap();
        assert!(matches!(
            g.receive(Reply::Prepare { epoch: 2 }),
            Some(Request::Prepared { epoch: 2 })
        ));
        assert!(!g.accepts_callback(&token()));
    }
    #[test]
    fn wrong_incarnation_and_surface_do_not_grant() {
        for wrong in ["incarnation", "surface"] {
            let mut g = active();
            let mut t = token();
            if wrong == "incarnation" {
                t.incarnation += 1;
            } else {
                t.surface_id = "other".into();
            }
            g.receive(Reply::Grant {
                token: t.clone(),
                revision: 99,
            });
            assert!(!g.accepts_callback(&t));
        }
    }
    #[test]
    fn grant_receipt_is_not_actual_presentation() {
        let mut g = active();
        assert!(g.body_presentation_pending());
        assert!(g.frame_presented(1).is_none());
        g.body_submitted(2);
        assert!(g.body_presentation_pending());
        assert!(g.frame_presented(1).is_none());
        assert!(matches!(
            g.frame_presented(2),
            Some(Request::Presented { .. })
        ));
        assert!(g.frame_presented(3).is_none());
        assert!(!g.body_presentation_pending());
        g.begin_frame(4);
        g.receive(Reply::Revoke { token: token() });
        assert!(!g.body_presentation_pending());
        assert!(g.frame_presented(5).is_some());
        let mut next = token();
        next.epoch = 2;
        g.receive(Reply::Grant {
            token: next,
            revision: 2,
        });
        assert!(g.body_presentation_pending());
    }
    #[test]
    fn duplicate_old_grant_cannot_resurrect_retired_body() {
        let mut g = active();
        g.receive(Reply::Revoke { token: token() });
        g.frame_presented(1).unwrap();
        g.receive(Reply::Grant {
            token: token(),
            revision: 1,
        });
        assert!(g.token().is_none());
        let mut fresh = token();
        fresh.epoch = 2;
        g.receive(Reply::Grant {
            token: fresh.clone(),
            revision: 2,
        });
        assert_eq!(g.token(), Some(&fresh));
    }
    #[test]
    fn rejection_reconnects_and_preserves_retirement_obligation() {
        let mut g = active();
        g.begin_frame(7);
        let rejected = Reply::Rejected {
            reason: "session_binding".into(),
        };
        assert!(requires_reconnect(&rejected));
        g.receive(rejected);
        assert!(g.token().is_none());
        assert!(g.receive(Reply::Prepare { epoch: 2 }).is_none());
        assert!(g.frame_presented(8).is_none());
        g.receive(Reply::Accepted {
            boot_id: "b".into(),
            companion_id: "eva".into(),
            session_id: "s".into(),
        });
        g.receive(Reply::Revoke { token: token() });
        assert!(matches!(
            g.frame_presented(9),
            Some(Request::Retired { .. })
        ));
    }
    #[test]
    fn operation_error_does_not_disconnect_or_hide_body() {
        let mut g = active();
        let error = Reply::OperationError {
            operation: "recall".into(),
            reason: "handoff_pending".into(),
        };
        assert!(!requires_reconnect(&error));
        g.receive(error);
        assert!(g.accepts_callback(&token()));
        assert!(g.operation_error().unwrap().contains("handoff_pending"));
    }
    #[test]
    fn metadata_coalesces_without_dropping_ordered_controls() {
        let (tx, requests) = mpsc::sync_channel(64);
        let (_events, rx) = mpsc::sync_channel(64);
        let mut h = SharedHost {
            gate: active(),
            tx,
            rx,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed: false,
            metadata: Arc::new(std::sync::Mutex::new([None, None, None])),
            dialogue: Default::default(),
            memory: Default::default(),
            context_access: Default::default(),
            history: Default::default(),
            voice_text: Default::default(),
        };
        for n in 0..1000 {
            h.send(Request::Focus {
                focused: n % 2 == 0,
            });
            h.send(Request::Input {
                active: n % 2 == 0,
                selecting: false,
            });
        }
        h.send(Request::Recall);
        h.send(Request::Presence {
            mode: Presence::Hidden,
        });
        assert!(!h.failed);
        assert!(matches!(requests.try_recv(), Ok(Request::Recall)));
        assert!(matches!(
            requests.try_recv(),
            Ok(Request::Presence {
                mode: Presence::Hidden
            })
        ));
        assert!(matches!(
            h.metadata.lock().unwrap()[0],
            Some(Request::Focus { focused: false })
        ));
    }
    #[test]
    fn persisted_scope_discovery_is_bounded_and_preserves_review_origin() {
        let (tx, _requests) = mpsc::sync_channel(64);
        let (events, rx) = mpsc::sync_channel(64);
        let mut h = SharedHost {
            gate: active(),
            tx,
            rx,
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed: false,
            metadata: Arc::new(std::sync::Mutex::new([None, None, None])),
            dialogue: Default::default(),
            memory: Default::default(),
            context_access: Default::default(),
            history: Default::default(),
            voice_text: Default::default(),
        };
        let discovery = Reply::MemoryScopes {
            scopes: vec![MemoryScope::Project("project-without-task".into())],
            next_after: Some("opaque-cursor".into()),
        };
        let review = Reply::MemoryScopeReview {
            context: ContextRef {
                host_id: "h".into(),
                pane_id: "original-pane".into(),
                task_id: None,
                project_id: Some("project-without-task".into()),
                revision: 7,
            },
            scope: MemoryScope::Project("project-without-task".into()),
            review_id: "review".into(),
            digest: "exact-digest".into(),
        };
        events.send(WorkerEvent::Reply(discovery.clone())).unwrap();
        events.send(WorkerEvent::Reply(review.clone())).unwrap();
        assert!(h.poll());
        assert_eq!(h.take_memory(), Some(discovery.clone()));
        assert_eq!(h.take_memory(), Some(review));
        assert!(h.take_memory().is_none());
        assert!(h.gate.accepts_callback(&token()));
        for _ in 0..33 {
            events.send(WorkerEvent::Reply(discovery.clone())).unwrap();
        }
        h.poll();
        assert!(h.failed);
        assert_eq!(h.memory.len(), 32);
        assert!(h.gate.token().is_none());
    }
    #[test]
    fn healthy_close_waits_for_exact_durable_retirement_ack() {
        let mut g = active();
        assert_eq!(g.possible_body_token(), Some(&token()));
        g.receive(Reply::RetirementAccepted { token: token() });
        assert!(!g.retirement_accepted(&token()));
        g.receive(Reply::Revoke { token: token() });
        g.frame_presented(1).unwrap();
        assert!(!g.retirement_accepted(&token()));
        let mut stale = token();
        stale.epoch += 1;
        g.receive(Reply::RetirementAccepted { token: stale });
        assert!(!g.retirement_accepted(&token()));
        g.receive(Reply::RetirementAccepted { token: token() });
        assert!(g.retirement_accepted(&token()));
        assert!(g.possible_body_token().is_none());
    }
    #[test]
    fn restored_pane_counter_cannot_alias_an_earlier_window_task() {
        assert_ne!(
            host_scoped_task_id("window-a", "restored-pane", 1),
            host_scoped_task_id("window-b", "restored-pane", 1)
        );
        assert_ne!(
            host_scoped_task_id("window-a", "restored-pane", 1),
            host_scoped_task_id("window-a", "restored-pane", 2)
        );
        assert_eq!(
            host_scoped_task_id("window-a", "restored-pane", 1),
            host_scoped_task_id("window-a", "restored-pane", 1)
        );
    }
    #[test]
    fn withdrawal_ack_is_host_bound_and_new_grant_invalidates_it() {
        let mut g = active();
        g.receive(Reply::WithdrawalAccepted {
            host_id: "other".into(),
            incarnation: 1,
        });
        assert!(!g.withdrawal_accepted());
        g.receive(Reply::WithdrawalAccepted {
            host_id: "h".into(),
            incarnation: 1,
        });
        assert!(g.withdrawal_accepted());
        g.receive(Reply::Grant {
            token: token(),
            revision: 2,
        });
        assert!(!g.withdrawal_accepted());
    }
}

#[cfg(test)]
mod modern_admission_tests {
    use super::*;
    fn host() -> Host {
        Host {
            id: "h".into(),
            incarnation: 1,
            surface_id: "surface".into(),
            retirement: RetirementCapability::NativePresented,
        }
    }
    fn token(epoch: u64) -> Token {
        Token {
            companion_id: "eva".into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "surface".into(),
            boot_id: "boot".into(),
            epoch,
        }
    }
    fn accepted(nonce: &str, boot: &str) -> Reply {
        Reply::HostAccepted {
            boot_id: boot.into(),
            companion_id: "eva".into(),
            session_id: "session".into(),
            features: vec![HostFeature::WithdrawalResumeV1],
            connection_nonce: nonce.into(),
        }
    }
    #[test]
    fn history_is_available_only_on_a_connection_that_negotiated_it() {
        let mut gate = HostGate::new_modern(host());
        gate.receive(accepted("n1", "boot"));
        assert!(!gate.history_supported());
        let mut extended = accepted("n2", "boot");
        if let Reply::HostAccepted { features, .. } = &mut extended {
            features.push(HostFeature::DialogueHistoryV1);
        }
        gate.receive(extended);
        assert!(gate.history_supported());
        gate.disconnect();
        assert!(!gate.history_supported());
        let mut alone = accepted("n3", "boot");
        if let Reply::HostAccepted { features, .. } = &mut alone {
            *features = vec![HostFeature::DialogueHistoryV1];
        }
        gate.receive(alone);
        assert!(!gate.history_supported());
        assert_eq!(gate.status, "Eva host capabilities unavailable");
    }
    #[test]
    fn only_a_pre_admission_schema_rejection_drops_the_history_request() {
        let schema = [Reply::Rejected {
            reason: "invalid_schema".into(),
        }];
        assert!(predates_features(&schema, false));
        assert!(!predates_features(&schema, true), "after admission");
        assert!(!predates_features(
            &[Reply::Rejected {
                reason: "host_already_connected".into()
            }],
            false
        ));
        assert_eq!(worker_features(1), [HostFeature::WithdrawalResumeV1]);
        for level in 1..=FEATURES.len() {
            assert!(valid_host_features(&worker_features(level)));
        }
        assert_eq!(worker_features(9).len(), FEATURES.len());
    }
    fn ready(nonce: &str, intent: u64, retired_token: Option<Token>) -> Reply {
        Reply::WithdrawalReady {
            connection_nonce: nonce.into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "surface".into(),
            intent,
            retired_token,
        }
    }
    fn resumed(
        nonce: &str,
        intent: u64,
        retired_token: Option<Token>,
        admission_epoch: u64,
    ) -> Reply {
        Reply::WithdrawalResumed {
            connection_nonce: nonce.into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "surface".into(),
            intent,
            retired_token,
            admission_epoch,
        }
    }
    fn active() -> HostGate {
        let mut gate = HostGate::new_modern(host());
        assert!(matches!(
            gate.receive(accepted("connection", "boot")),
            Some(Request::ActivateHost { .. })
        ));
        gate.receive(Reply::HostActivated {
            connection_nonce: "connection".into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "surface".into(),
            admission_epoch: 1,
        });
        gate.receive(Reply::Grant {
            token: token(1),
            revision: 1,
        });
        assert_eq!(gate.token(), Some(&token(1)));
        gate
    }
    fn retire(gate: &mut HostGate) {
        gate.begin_frame(10);
        gate.receive(Reply::Revoke { token: token(1) });
        assert!(gate.frame_presented(10).is_none());
        assert!(matches!(
            gate.frame_presented(11),
            Some(Request::Retired {
                token: Token { epoch: 1, .. },
                presentation_serial: 11
            })
        ));
        gate.receive(Reply::RetirementAccepted { token: token(1) });
    }
    #[test]
    fn connection_loss_never_erases_a_possible_body() {
        let mut gate = HostGate::new_modern(host());
        assert!(!gate.has_connection());
        gate = active();
        assert!(gate.has_connection());
        gate.disconnect();
        assert!(!gate.has_connection());
        assert_eq!(gate.possible_body_token(), Some(&token(1)));
        assert!(!gate.withdrawal_accepted());
    }
    #[test]
    fn cancel_keeps_exact_proof_and_requires_bound_resume_ack_before_new_grant() {
        let mut gate = active();
        let connection = gate.connection_generation();
        let admission = gate.admission_generation();
        gate.begin_withdrawal();
        assert!(gate.token().is_none());
        assert!(!gate.accepts_callback(&token(1)));
        assert!(matches!(
            gate.next_control(),
            Some(Request::WithdrawIntent {
                intent: 1,
                previous_token: Some(_),
                ..
            })
        ));
        gate.cancel_withdrawal();
        assert!(gate.next_control().is_none());
        gate.receive(ready("connection", 1, Some(token(1))));
        assert!(!gate.withdrawal_accepted());
        retire(&mut gate);
        assert!(matches!(
            gate.receive(ready("connection", 1, Some(token(1)))),
            Some(Request::ResumeWithdrawal { intent: 1, .. })
        ));
        assert!(!gate.withdrawal_accepted());
        assert!(gate.token().is_none());
        gate.receive(resumed("old-connection", 1, Some(token(1)), 2));
        assert!(gate.withdrawal_pending());
        gate.receive(resumed("connection", 2, Some(token(1)), 2));
        assert!(gate.withdrawal_pending());
        gate.receive(resumed("connection", 1, Some(token(2)), 2));
        assert!(gate.withdrawal_pending());
        gate.receive(resumed("connection", 1, Some(token(1)), 2));
        assert!(!gate.withdrawal_pending());
        assert_eq!(gate.connection_generation(), connection);
        assert!(gate.admission_generation() > admission);
        assert!(gate.retirement_accepted(&token(1)));
        assert!(gate.token().is_none());
        gate.receive(Reply::Grant {
            token: token(2),
            revision: 2,
        });
        assert_eq!(gate.token(), Some(&token(2)));
        assert!(!gate.accepts_callback(&token(1)));
    }
    #[test]
    fn idle_withdrawal_ack_cannot_skip_an_existing_surface_obligation() {
        let mut gate = active();
        gate.begin_withdrawal();
        gate.next_control();
        gate.cancel_withdrawal();
        gate.receive(ready("connection", 1, None));
        assert!(!gate.withdrawal_accepted());
        assert!(gate.next_control().is_none());
        gate.receive(Reply::WithdrawalAccepted {
            host_id: "h".into(),
            incarnation: 1,
        });
        assert!(!gate.withdrawal_accepted());
    }
    #[test]
    fn fresh_close_while_resume_is_in_flight_gets_a_new_intent_and_no_auto_exit() {
        let mut gate = active();
        gate.begin_withdrawal();
        gate.next_control();
        retire(&mut gate);
        gate.receive(ready("connection", 1, Some(token(1))));
        assert!(gate.withdrawal_accepted());
        gate.cancel_withdrawal();
        assert!(matches!(
            gate.next_control(),
            Some(Request::ResumeWithdrawal { intent: 1, .. })
        ));
        gate.begin_withdrawal();
        assert!(!gate.withdrawal_accepted());
        assert!(matches!(
            gate.receive(resumed("connection", 1, Some(token(1)), 2)),
            Some(Request::WithdrawIntent {
                intent: 2,
                previous_token: None,
                ..
            })
        ));
        gate.receive(ready("connection", 1, Some(token(1))));
        assert!(!gate.withdrawal_accepted());
        gate.receive(ready("connection", 2, None));
        assert!(gate.withdrawal_accepted());
        assert!(gate.token().is_none());
    }
    #[test]
    fn latest_cancel_cancels_a_queued_reclose_without_discarding_retirement() {
        let mut gate = active();
        gate.begin_withdrawal();
        gate.next_control();
        retire(&mut gate);
        gate.receive(ready("connection", 1, Some(token(1))));
        gate.cancel_withdrawal();
        gate.next_control();
        gate.begin_withdrawal();
        gate.cancel_withdrawal();
        assert!(gate
            .receive(resumed("connection", 1, Some(token(1)), 2))
            .is_none());
        assert!(!gate.withdrawal_pending());
        assert!(gate.retirement_accepted(&token(1)));
    }
    #[test]
    fn reconnect_preserves_cancel_and_old_token_but_rebinds_all_control_nonces() {
        let mut gate = active();
        gate.begin_withdrawal();
        gate.next_control();
        gate.cancel_withdrawal();
        gate.begin_frame(4);
        gate.receive(Reply::Revoke { token: token(1) });
        gate.frame_presented(5);
        gate.disconnect();
        assert!(
            matches!(gate.receive(accepted("new-connection","new-boot")),Some(Request::WithdrawIntent {connection_nonce,intent:1,previous_token:Some(token),..}) if connection_nonce=="new-connection" && token.epoch==1)
        );
        gate.receive(ready("connection", 1, Some(token(1))));
        assert!(gate.withdrawal_pending());
        assert!(gate.next_control().is_none());
        assert!(matches!(
            gate.frame_presented(6),
            Some(Request::Retired {
                token: Token { epoch: 1, .. },
                ..
            })
        ));
        gate.receive(Reply::RetirementAccepted { token: token(1) });
        assert!(
            matches!(gate.receive(ready("new-connection",1,Some(token(1)))),Some(Request::ResumeWithdrawal {connection_nonce,..}) if connection_nonce=="new-connection")
        );
        gate.receive(resumed("connection", 1, Some(token(1)), 1));
        assert!(gate.withdrawal_pending());
        gate.receive(resumed("new-connection", 1, Some(token(1)), 1));
        assert!(!gate.withdrawal_pending());
        assert_eq!(gate.host_id(), "h");
    }
    #[test]
    fn duplicate_revoke_preserves_an_inflight_native_clearance_and_submitted_proof() {
        let mut gate = active();
        gate.begin_withdrawal();
        gate.next_control();
        gate.begin_frame(10);
        gate.receive(Reply::Revoke { token: token(1) });
        assert_eq!(gate.pending_retirement_boundary(), Some((&token(1), 10)));
        // Frame 11 is genuinely in flight. A duplicate received in frame 15
        // must not make that original exact-token native proof impossible.
        gate.begin_frame(15);
        gate.receive(Reply::Revoke { token: token(1) });
        assert_eq!(gate.pending_retirement_boundary(), Some((&token(1), 10)));
        assert!(
            matches!(gate.frame_presented(11), Some(Request::Retired { token, presentation_serial:11 }) if token.epoch==1)
        );
        gate.receive(Reply::Revoke { token: token(1) });
        assert!(gate.pending_retirement_boundary().is_none());
        assert!(gate.token().is_none());
    }
    #[test]
    fn new_connection_requires_fresh_native_boundary_for_the_same_old_token() {
        let mut gate = active();
        gate.begin_frame(10);
        gate.receive(Reply::Revoke { token: token(1) });
        gate.begin_frame(20);
        gate.disconnect();
        gate.receive(accepted("new-connection", "new-boot"));
        assert_eq!(gate.pending_retirement_boundary(), Some((&token(1), 20)));
        gate.receive(Reply::Revoke { token: token(1) });
        assert_eq!(gate.pending_retirement_boundary(), Some((&token(1), 20)));
        assert!(gate.frame_presented(11).is_none());
        assert!(gate.frame_presented(20).is_none());
        assert!(
            matches!(gate.frame_presented(21), Some(Request::Retired { token, .. }) if token.epoch==1)
        );
        assert!(gate.token().is_none());
    }
    #[test]
    fn initial_admission_requires_capability_and_activation_ack() {
        let mut gate = HostGate::new_modern(host());
        gate.receive(Reply::Accepted {
            boot_id: "boot".into(),
            companion_id: "eva".into(),
            session_id: "session".into(),
        });
        gate.receive(Reply::Grant {
            token: token(1),
            revision: 1,
        });
        assert!(gate.token().is_none());
        gate.receive(accepted("connection", "boot"));
        gate.receive(Reply::Grant {
            token: token(1),
            revision: 1,
        });
        assert!(gate.token().is_none());
        assert!(gate.token.is_none());
    }
}

#[cfg(test)]
mod cancel_key_tests {
    use super::*;
    #[test]
    fn repeat_and_release_stay_owned_after_the_dialog_is_gone() {
        let mut lease = CancelKeyLease::default();
        assert_eq!(lease.press(true, false), CancelKeyAction::Cancel);
        assert_eq!(lease.press(false, true), CancelKeyAction::Suppress);
        assert_eq!(lease.press(false, false), CancelKeyAction::Suppress);
        assert!(lease.release(true));
        assert_eq!(lease.press(false, false), CancelKeyAction::Pass);
        assert!(!lease.release(true));
    }
    #[test]
    fn both_blur_release_orders_require_a_focused_release_before_terminal_escape() {
        for release_first in [true, false] {
            let mut lease = CancelKeyLease::default();
            lease.press(true, false);
            if release_first {
                assert!(lease.release(true));
                lease.focus_lost();
            } else {
                lease.focus_lost();
                assert!(lease.release(false));
            }
            assert_eq!(lease.press(false, false), CancelKeyAction::Suppress);
            assert_eq!(lease.press(true, true), CancelKeyAction::Suppress);
            assert!(lease.release(true));
            assert_eq!(lease.press(false, false), CancelKeyAction::Pass);
        }
    }
    #[test]
    fn work_owned_escape_passes_and_an_old_hold_cannot_cancel_a_new_intent() {
        let mut lease = CancelKeyLease::default();
        assert_eq!(lease.press(false, false), CancelKeyAction::Pass);
        assert_eq!(lease.press(true, false), CancelKeyAction::Cancel);
        assert_eq!(lease.press(true, true), CancelKeyAction::Suppress);
        lease.release(true);
        assert_eq!(lease.press(true, false), CancelKeyAction::Cancel);
        lease.release(true);
        lease.other_input();
        lease.focus_lost();
        assert_eq!(lease.press(false, false), CancelKeyAction::Pass);
    }
}

#[cfg(test)]
mod queued_grant_tests {
    use super::*;
    #[test]
    fn an_already_queued_new_grant_changes_the_required_token_without_accepting_old_proof() {
        let h = Host {
            id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            retirement: RetirementCapability::NativePresented,
        };
        let token = |epoch| Token {
            companion_id: "eva".into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            boot_id: "b".into(),
            epoch,
        };
        let mut gate = HostGate::new_modern(h);
        gate.receive(Reply::HostAccepted {
            boot_id: "b".into(),
            companion_id: "eva".into(),
            session_id: "session".into(),
            features: vec![HostFeature::WithdrawalResumeV1],
            connection_nonce: "n".into(),
        });
        gate.receive(Reply::HostActivated {
            connection_nonce: "n".into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            admission_epoch: 1,
        });
        gate.receive(Reply::Grant {
            token: token(1),
            revision: 1,
        });
        gate.begin_frame(10);
        gate.receive(Reply::Revoke { token: token(1) });
        gate.frame_presented(11);
        gate.begin_withdrawal();
        gate.next_control();
        // Authority has retired epoch 1 and queued epoch 2 before it sees
        // withdrawal. Its old acknowledgement has not reached this GUI yet.
        gate.receive(Reply::Grant {
            token: token(2),
            revision: 2,
        });
        assert!(gate.token().is_none());
        gate.begin_frame(12);
        gate.receive(Reply::Revoke { token: token(2) });
        gate.receive(Reply::RetirementAccepted { token: token(1) });
        let ready = |retired_token| Reply::WithdrawalReady {
            connection_nonce: "n".into(),
            host_id: "h".into(),
            incarnation: 1,
            surface_id: "s".into(),
            intent: 1,
            retired_token: Some(retired_token),
        };
        gate.receive(ready(token(1)));
        assert!(!gate.withdrawal_accepted());
        assert!(matches!(
            gate.frame_presented(13),
            Some(Request::Retired {
                token: Token { epoch: 2, .. },
                ..
            })
        ));
        gate.receive(Reply::RetirementAccepted { token: token(2) });
        gate.receive(ready(token(2)));
        assert!(gate.withdrawal_accepted());
        assert!(!gate.retirement_accepted(&token(1)));
    }
}
