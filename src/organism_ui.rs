//! Native Block-pane body for the experimental ASCII organism.

use gtk::prelude::*;
use gtk::{Box as GBox, Label, Orientation};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::block_view::{AltScreenTransition, TermView};
use crate::config::OrganismMotion;
#[cfg(test)]
use crate::organism::sprite_frame;
use crate::organism::{
    classify_command, sprite_frame_with_context, sticky_glyph_with_context, AgentPulse,
    AmbientBehavior, AmbientMind, Behavior, BodyLanguage, CircadianPhase, CommandKind, LifeState,
    NativeOrganism, Reaction, RenderContext, RepoArrival, RepoVigil, RepoWorkState, Tone,
    VisualGrowthStage, VisualTransition, WatchRhythm,
};
use crate::organism_attention::{AttentionArbiter, AttentionCue};
use crate::organism_memory::{
    local_circadian_time_at_ms, unix_ms, CircadianProfile, GrowthProgress, GrowthStage,
    LocalCircadianTime, MemoryEvent, MemoryInsight, RepoContext,
};

/// An accepted correction only vouches for a command that starts promptly.
const CORRECTION_ASSIST_WINDOW: Duration = Duration::from_secs(30);
const HUMAN_INPUT_RETREAT: Duration = Duration::from_millis(900);
const SURFACE_FRAME_INTERVAL: Duration = Duration::from_millis(100);
/// Heartbeat while the mind rests or the body is static: life goes on, the
/// process barely wakes. Kept just under the tick's one-second dt clamp so
/// routine dispatch latency is still simulated instead of clipped away.
const DORMANT_FRAME_INTERVAL: Duration = Duration::from_millis(900);
/// A cross-pane failure is a brief orienting glance, not a queued reaction.
const GLANCE_ASIDE_HOLD: Duration = Duration::from_millis(1_400);
const SURFACE_MARGIN: i32 = 8;
/// After this long watching one command, the body settles into its vigil.
const SETTLED_WATCH_ONSET: Duration = Duration::from_secs(60);
/// Elapsed time only appears on the card once a command stops being quick.
const ACCOMPANY_LABEL_ONSET: Duration = Duration::from_secs(10);
/// A newly encountered checkout gets one short, silent look around after the
/// command that introduced it has settled. The cue is live-only and never
/// delays a command reaction or enters repository memory.
const TERRITORY_INTRO_HOLD: Duration = Duration::from_secs(4);
const MAX_TERRITORY_HASH_BYTES: usize = 2 * 1024;
/// Widest canonical inline pose (`CelebrateBig`); reserving the slot keeps the
/// title/status column still when reactions change silhouette.
const INLINE_SPRITE_SLOT_CHARS: i32 = 12;
const TONE_CLASSES: [&str; 5] = [
    "organism-quiet",
    "organism-active",
    "organism-success",
    "organism-error",
    "organism-warning",
];

fn surface_frame_delay(
    motion: OrganismMotion,
    owner: bool,
    alt_screen: bool,
    resting: bool,
) -> Duration {
    if motion == OrganismMotion::Full && owner && !alt_screen && !resting {
        SURFACE_FRAME_INTERVAL
    } else {
        DORMANT_FRAME_INTERVAL
    }
}

/// A focus transfer may replace an already-pending source, but must not start
/// a second source after the fired callback has taken its id from the slot.
/// That callback observes the new owner at its tail and schedules the right
/// cadence itself.
fn focus_transfer_rearm_delay(
    motion: OrganismMotion,
    owner: bool,
    alt_screen: bool,
    timer_pending: bool,
) -> Option<Duration> {
    timer_pending.then(|| surface_frame_delay(motion, owner, alt_screen, false))
}

fn reaction_hold(reaction: &Reaction) -> Duration {
    let millis = match reaction.behavior {
        // Ordinary/quiet passes acknowledge the event without occupying the
        // output centre for the old fixed eight seconds.
        Behavior::Idle => 1_500,
        Behavior::Celebrate if reaction.tone == Tone::Quiet => 1_800,
        Behavior::Celebrate => 2_500,
        // Errors need time to be noticed; a repeated streak deliberately sits
        // longer, but the next input still interrupts it immediately.
        Behavior::InspectError => 5_000,
        Behavior::SitNearError => 10_000,
        Behavior::CelebrateBig => 7_000,
        Behavior::RestAfterPush => 5_000,
        Behavior::UnknownOutcome => 4_500,
        // GlanceAside is live-only and uses its own timer. Keep this arm for
        // exhaustive safety if a future caller ever wraps it in a Reaction.
        Behavior::GlanceAside => 1_400,
        Behavior::WatchCommand
        | Behavior::Sleep
        | Behavior::Explore
        | Behavior::Approach
        | Behavior::WatchAgent
        | Behavior::WatchSettled
        | Behavior::GuardFailure
        | Behavior::GuardStuck
        | Behavior::GuardRecovery
        | Behavior::GuardCautious => 2_500,
    };
    Duration::from_millis(millis)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceMode {
    Idle,
    Typing,
    Watching,
    Reacting,
}

/// The only fact allowed to cross from one pane into another. Source command,
/// cwd, output, status code, and repository identity never enter this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresenceSignal {
    BackgroundCommandFailed,
}

/// Ephemeral state of the one live spatial body. This is deliberately
/// separate from pane-local reducer reactions and inline/sticky history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresenceCue {
    GlanceAside,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SurfaceBox {
    width: i32,
    height: i32,
    right_gutter: i32,
    cell_width: i32,
    cell_height: i32,
    body_width: i32,
    body_height: i32,
    /// On-screen grid row of the cursor: the output growth edge after which a
    /// watching/reaction pose may use only fully clear rows.
    cursor_row: i32,
}

type SurfaceSignature = (i32, i32, i32, i32, i32, i32, i32);

fn surface_signature(surface: SurfaceBox) -> SurfaceSignature {
    (
        surface.width,
        surface.height,
        surface.right_gutter,
        surface.cell_width,
        surface.cell_height,
        surface.body_width,
        surface.body_height,
    )
}

fn below_output_y(surface: SurfaceBox) -> Option<i32> {
    if surface.body_height <= 0 {
        return None;
    }
    let cell_height = surface.cell_height.max(1);
    let margin_y = SURFACE_MARGIN.max(cell_height);
    let min_y = align_up(margin_y, cell_height);
    let max_y = align_down(
        surface
            .height
            .saturating_sub(surface.body_height)
            .saturating_sub(margin_y),
        cell_height,
    );
    let cursor_bottom = surface
        .cursor_row
        .max(0)
        .saturating_add(1)
        .saturating_mul(cell_height);
    let clear_y = align_up(cursor_bottom.max(min_y), cell_height);
    (clear_y <= max_y).then_some(clear_y)
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SurfacePoint {
    x: f64,
    y: f64,
}

/// How eagerly the idle body wanders its 80-second cycle, derived from body
/// language: a drowsy mind stays put, a listless one paces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WanderTempo {
    Drowsy,
    Calm,
    Restless,
}

/// A spatial habit derived in the UI from a canonical repository identity.
/// The identity itself never crosses into the reducer and this value is never
/// persisted: it merely gives a familiar checkout a stable nest and route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerritoryHabit {
    familiarity: RepoArrival,
    nest_right: bool,
    route_offset: u16,
}

impl TerritoryHabit {
    fn for_repo(repo: &str, familiarity_days: u32) -> Self {
        let hash = stable_repo_hash(repo.as_bytes());
        Self {
            familiarity: RepoArrival::from_familiarity(familiarity_days),
            nest_right: hash & 1 != 0,
            route_offset: ((hash >> 1) % 800) as u16,
        }
    }

    const fn is_unfamiliar(self) -> bool {
        matches!(self.familiarity, RepoArrival::Unfamiliar)
    }

    const fn is_home(self) -> bool {
        matches!(self.familiarity, RepoArrival::Home)
    }

    fn route_frame(self, frame: u64) -> u64 {
        frame.wrapping_add(u64::from(self.route_offset))
    }

    const fn nest_x(self, min_x: i32, max_x: i32) -> i32 {
        if self.is_home() && self.nest_right {
            max_x
        } else {
            min_x
        }
    }
}

/// Stable FNV-1a rather than `DefaultHasher`, whose seed/algorithm is not a UI
/// contract. Only the resulting few bits survive; no path is displayed or
/// stored by the territory layer.
fn stable_repo_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes.iter().copied().take(MAX_TERRITORY_HASH_BYTES) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Clone, Copy, Default)]
struct OutputRhythmTracker {
    command_started: Option<Instant>,
    third_recent_output: Option<Instant>,
    second_recent_output: Option<Instant>,
    last_output: Option<Instant>,
    resumed_until: Option<Instant>,
    rhythm: WatchRhythm,
}

impl OutputRhythmTracker {
    const BUSY_WINDOW: Duration = Duration::from_millis(1_200);
    const WAITING_AFTER: Duration = Duration::from_secs(3);
    const RESUMED_HOLD: Duration = Duration::from_millis(900);

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn start(&mut self, now: Instant) {
        self.reset();
        self.command_started = Some(now);
    }

    fn note_output(&mut self, now: Instant) {
        let quiet_since = self.last_output.or(self.command_started);
        let was_waiting = quiet_since
            .is_some_and(|last| now.saturating_duration_since(last) >= Self::WAITING_AFTER);
        self.third_recent_output = self.second_recent_output;
        self.second_recent_output = self.last_output;
        self.last_output = Some(now);
        if was_waiting {
            self.rhythm = WatchRhythm::Resumed;
            self.resumed_until = Some(now + Self::RESUMED_HOLD);
        } else if self.resumed_until.is_some_and(|until| now < until) {
            self.rhythm = WatchRhythm::Resumed;
        } else if self
            .third_recent_output
            .is_some_and(|oldest| now.saturating_duration_since(oldest) <= Self::BUSY_WINDOW)
        {
            self.rhythm = WatchRhythm::Busy;
            self.resumed_until = None;
        } else {
            self.rhythm = WatchRhythm::Steady;
        }
    }

    fn sample(&mut self, now: Instant, command_running: bool) -> WatchRhythm {
        if !command_running {
            self.reset();
            return WatchRhythm::Steady;
        }
        if self.resumed_until.is_some_and(|until| now < until) {
            return WatchRhythm::Resumed;
        }
        self.resumed_until = None;
        if self
            .last_output
            .or(self.command_started)
            .is_some_and(|last| now.saturating_duration_since(last) >= Self::WAITING_AFTER)
        {
            self.rhythm = WatchRhythm::Waiting;
        } else if self
            .third_recent_output
            .is_some_and(|oldest| now.saturating_duration_since(oldest) <= Self::BUSY_WINDOW)
        {
            self.rhythm = WatchRhythm::Busy;
        } else {
            self.rhythm = WatchRhythm::Steady;
        }
        self.rhythm
    }
}

fn reset_output_rhythm_at_boundary(
    tracker: &mut OutputRhythmTracker,
    command_running: bool,
    now: Instant,
) {
    if command_running {
        tracker.start(now);
    } else {
        tracker.reset();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WatchRhythmPlan {
    visible: WatchRhythm,
    offered: WatchRhythm,
    attention_offer: Option<WatchRhythm>,
}

/// Consume each non-steady sensing edge exactly once. The neutral fallback is
/// installed before attention/presentation checks, so a rejected or invisible
/// edge cannot remain on screen or be replayed on a later heartbeat.
fn watch_rhythm_plan(
    desired: WatchRhythm,
    visible: WatchRhythm,
    offered: WatchRhythm,
) -> WatchRhythmPlan {
    if desired == WatchRhythm::Steady {
        WatchRhythmPlan {
            visible: WatchRhythm::Steady,
            offered: WatchRhythm::Steady,
            attention_offer: None,
        }
    } else if desired != offered {
        WatchRhythmPlan {
            visible: WatchRhythm::Steady,
            offered: desired,
            attention_offer: Some(desired),
        }
    } else {
        WatchRhythmPlan {
            visible,
            offered,
            attention_offer: None,
        }
    }
}

fn watch_rhythm_context_presentable(
    owner: bool,
    motion: OrganismMotion,
    mode: SurfaceMode,
    alt_screen: bool,
) -> bool {
    owner && motion != OrganismMotion::Static && mode == SurfaceMode::Watching && !alt_screen
}

fn watch_rhythm_surface_presentable(context_presentable: bool, visual_mapped: bool) -> bool {
    context_presentable && visual_mapped
}

fn should_begin_territory_intro(
    pending: bool,
    territory: Option<TerritoryHabit>,
    vigil: RepoVigil,
) -> bool {
    // `pending` is the arrival-time eligibility bit. The command that first
    // introduced this checkout creates today's memory record before settling,
    // so the current habit may already have advanced Unfamiliar -> Known.
    pending && territory.is_some() && !vigil.is_active()
}

/// Fold a newly synchronized durable repo intention into the volatile
/// first-look state. Any vigil cancels both a pending introduction and one
/// already on screen; a clean sync leaves the arrival-time eligibility intact.
fn territory_intro_after_repo_sync<T>(
    pending: bool,
    active_until: Option<T>,
    vigil: RepoVigil,
) -> (bool, Option<T>) {
    if vigil.is_active() {
        (false, None)
    } else {
        (pending, active_until)
    }
}

/// Typing, alternate-screen ownership, focus loss, and fail-closed geometry are
/// terminal boundaries for a one-shot first look. Returning to a presentable
/// surface must not replay either a pending or partially shown introduction.
fn territory_intro_after_interruption<T>(
    _pending: bool,
    _active_until: Option<T>,
) -> (bool, Option<T>) {
    (false, None)
}

fn wander_tempo(language: BodyLanguage) -> WanderTempo {
    if language.drowsy {
        WanderTempo::Drowsy
    } else if language.listless {
        WanderTempo::Restless
    } else {
        WanderTempo::Calm
    }
}

/// Position within the 800-frame wander cycle as `(step in 0..=40 cells,
/// currently_walking)`: sit at one edge, walk across, sit at the other, walk
/// back. The tempo resizes the walk legs; `Calm` reproduces the original
/// 40-frame legs exactly.
fn wander_phase(frame: u64, tempo: WanderTempo) -> (i32, bool) {
    let walk = match tempo {
        WanderTempo::Drowsy => return (0, false),
        WanderTempo::Calm => 40,
        WanderTempo::Restless => 120,
    };
    let phase = (frame % 800) as i32;
    let sit = (800 - 2 * walk) / 2;
    if phase < sit {
        (0, false)
    } else if phase < sit + walk {
        ((phase - sit) * 40 / walk, true)
    } else if phase < sit + walk + sit {
        (40, false)
    } else {
        (40 - (phase - sit - walk - sit) * 40 / walk, true)
    }
}

fn surface_mode(
    behavior: Behavior,
    command_running: bool,
    human_input_age: Option<Duration>,
) -> SurfaceMode {
    if human_input_age.is_some_and(|age| age < HUMAN_INPUT_RETREAT) {
        SurfaceMode::Typing
    } else if command_running {
        SurfaceMode::Watching
    } else if behavior == Behavior::Idle || behavior.is_repo_vigil() {
        SurfaceMode::Idle
    } else {
        SurfaceMode::Reacting
    }
}

fn suppress_live_body_for_focus(mode: SurfaceMode) -> bool {
    mode == SurfaceMode::Typing
}

fn visible_sleeping(
    owner: bool,
    motion: OrganismMotion,
    alt_screen: bool,
    body_visible: bool,
    cue_active: bool,
    mode: SurfaceMode,
    ambient: AmbientBehavior,
) -> bool {
    owner
        && motion != OrganismMotion::Static
        && !alt_screen
        && body_visible
        && !cue_active
        && mode == SurfaceMode::Idle
        && ambient == AmbientBehavior::Sleep
}

/// A durable vigil must be able to finish its forced-rest hysteresis even in
/// Static mode or when geometry hides the live sprite. Presence ownership and
/// the shared activity counter keep this one logical claim window-scoped;
/// ordinary non-vigil sleep retains the visible-body rule above.
fn repo_vigil_sleep_claim(
    owner: bool,
    mode: SurfaceMode,
    ambient: AmbientBehavior,
    vigil: RepoVigil,
) -> bool {
    owner && mode == SurfaceMode::Idle && ambient == AmbientBehavior::Sleep && vigil.is_active()
}

#[allow(clippy::too_many_arguments)]
fn sleeping_claim(
    owner: bool,
    motion: OrganismMotion,
    alt_screen: bool,
    body_visible: bool,
    cue_active: bool,
    mode: SurfaceMode,
    ambient: AmbientBehavior,
    vigil: RepoVigil,
) -> bool {
    visible_sleeping(
        owner,
        motion,
        alt_screen,
        body_visible,
        cue_active,
        mode,
        ambient,
    ) || repo_vigil_sleep_claim(owner, mode, ambient, vigil)
}

fn can_show_presence_cue(
    owner: bool,
    motion: OrganismMotion,
    alt_screen: bool,
    body_visible: bool,
    mode: SurfaceMode,
    repo_vigil_active: bool,
) -> bool {
    owner
        && motion != OrganismMotion::Static
        && !alt_screen
        && body_visible
        && mode == SurfaceMode::Idle
        && !repo_vigil_active
}

fn live_display_behavior(
    baseline: Behavior,
    mode: SurfaceMode,
    cue: Option<PresenceCue>,
) -> Behavior {
    if mode == SurfaceMode::Idle && cue == Some(PresenceCue::GlanceAside) {
        Behavior::GlanceAside
    } else {
        baseline
    }
}

/// Event reactions and cross-pane cues own a local animation epoch, so their
/// first immediate render is always the canonical signature frame instead of
/// inheriting whichever half of the window-global beat happened to be live.
/// Ambient wandering keeps the global frame and therefore stays continuous.
fn animation_frames(
    global: u64,
    behavior_origin: u64,
    cue_origin: u64,
    mode: SurfaceMode,
    cue: Option<PresenceCue>,
) -> (u64, u64) {
    let baseline = if matches!(mode, SurfaceMode::Watching | SurfaceMode::Reacting) {
        global.wrapping_sub(behavior_origin)
    } else {
        global
    };
    let live = if cue.is_some() {
        global.wrapping_sub(cue_origin)
    } else {
        baseline
    };
    (baseline, live)
}

fn presence_signal_for_exit(exit_code: Option<i32>) -> Option<PresenceSignal> {
    exit_code
        .is_some_and(|code| code != 0)
        .then_some(PresenceSignal::BackgroundCommandFailed)
}

fn completion_attention_cue(
    kind: CommandKind,
    exit_code: Option<i32>,
    recovered_failures: u32,
    agent_driven: bool,
) -> Option<AttentionCue> {
    match (kind, exit_code) {
        (_, Some(code)) if code != 0 => Some(AttentionCue::FailureVigil),
        (_, Some(0)) if agent_driven => None,
        (CommandKind::GitPush, Some(0)) => Some(AttentionCue::Push),
        (CommandKind::BuildOrTest, Some(0)) if recovered_failures > 0 => {
            Some(AttentionCue::Recovery)
        }
        (CommandKind::BuildOrTest | CommandKind::Other, Some(0)) => Some(AttentionCue::Closure),
        _ => None,
    }
}

fn remembered_insight_attention(has_optional_speech: bool) -> Option<AttentionCue> {
    has_optional_speech.then_some(AttentionCue::Insight)
}

fn visual_transition_for_motion(
    motion: OrganismMotion,
    from: Behavior,
    to: Behavior,
) -> Option<VisualTransition> {
    (motion == OrganismMotion::Full)
        .then(|| VisualTransition::between(from, to))
        .flatten()
}

/// Which watching pose fits: the Agent's commands get the crouch-apart pose,
/// a long human command earns the settled vigil, everything else the alert
/// watch.
fn watching_behavior(agent_watching: bool, elapsed: Option<Duration>) -> Behavior {
    if agent_watching {
        Behavior::WatchAgent
    } else if elapsed.is_some_and(|elapsed| elapsed >= SETTLED_WATCH_ONSET) {
        Behavior::WatchSettled
    } else {
        Behavior::WatchCommand
    }
}

/// Human-readable elapsed time for the accompaniment label: "40s",
/// "2m 30s", "1h 05m". Sub-hour times are quantized to ten-second steps so
/// the status label — an accessible live region — changes at most every ten
/// seconds instead of narrating every second of a long build.
fn elapsed_label(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    if total < 3_600 {
        let total = total / 10 * 10;
        if total < 60 {
            format!("{total}s")
        } else {
            format!("{}m {:02}s", total / 60, total % 60)
        }
    } else {
        format!("{}h {:02}m", total / 3_600, (total % 3_600) / 60)
    }
}

fn mark_likely_flaky(reaction: &mut Reaction, agent_driven: bool) {
    // A repeated one-run recovery points at an intermittent test, not at the
    // human. Preserve the reducer's ordinary success pose and only quiet the
    // tone and wording; Agent-owned commands remain speechless.
    if reaction.behavior == Behavior::CelebrateBig {
        reaction.behavior = Behavior::Celebrate;
    }
    reaction.tone = Tone::Quiet;
    reaction.speech = (!agent_driven).then_some("像是偶发的。");
    if let Some(rest) = reaction
        .description
        .strip_prefix("build/test passed after ")
    {
        if let Some((_, suffix)) = rest.split_once(" failure(s)") {
            reaction.description = format!("build/test passed after 1 failure(s){suffix}");
        }
    }
    reaction
        .description
        .push_str(" · repeated one-run recovery looks intermittent");
}

fn reaction_duration_label(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) if ms >= 1_000 => format!(" · {:.1}s", ms as f64 / 1_000.0),
        Some(ms) => format!(" · {ms}ms"),
        None => String::new(),
    }
}

fn append_reaction_detail(reaction: &mut Reaction, detail: &str) {
    if !reaction.description.contains(detail) {
        reaction.description.push_str(" · ");
        reaction.description.push_str(detail);
    }
}

/// Event-position replay owns the immediate pose and depth. A compacted late
/// event has no reconstructible position, so it deliberately falls back to a
/// restrained factual reaction instead of preserving arrival-order drama.
fn normalize_replayed_event(
    reaction: &mut Reaction,
    kind: CommandKind,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    insight: &MemoryInsight,
    agent_driven: bool,
) {
    let duration = reaction_duration_label(duration_ms);
    match (kind, exit_code) {
        (CommandKind::BuildOrTest, Some(code)) if code != 0 => {
            if insight.event_order_exact {
                let failures = insight.open_failures.max(1);
                reaction.behavior = if failures >= 2 {
                    Behavior::SitNearError
                } else {
                    Behavior::InspectError
                };
                reaction.tone = Tone::Error;
                reaction.speech = (!agent_driven && failures == 1).then_some("这里。");
                reaction.description = format!("exit {code}{duration} · build failure {failures}");
            } else {
                reaction.behavior = Behavior::InspectError;
                reaction.tone = Tone::Error;
                reaction.speech = None;
                reaction.description =
                    format!("exit {code}{duration} · older repo event order unavailable");
            }
            if agent_driven {
                append_reaction_detail(reaction, "agent-driven");
            }
        }
        (CommandKind::BuildOrTest, Some(0)) => {
            if insight.event_order_exact {
                let recovered = insight.recovered_failures;
                if recovered > 0 {
                    reaction.behavior = if recovered >= 3 && !agent_driven {
                        Behavior::CelebrateBig
                    } else {
                        Behavior::Celebrate
                    };
                    reaction.tone = Tone::Success;
                    reaction.speech = if agent_driven {
                        None
                    } else if recovered >= 3 {
                        Some("终于。")
                    } else {
                        Some("好了。")
                    };
                    reaction.description =
                        format!("build/test passed after {recovered} failure(s){duration}");
                } else {
                    // A stale final-state context can make a clean event look
                    // like a recovery or carry a different window's pass
                    // ordinal. The replay does not expose that ordinal, so use
                    // a factual clean-pass line instead of preserving either
                    // stale recovery or habituation wording.
                    if reaction.behavior == Behavior::CelebrateBig {
                        reaction.behavior = Behavior::Celebrate;
                    }
                    if matches!(reaction.speech, Some("好了。" | "终于。")) {
                        reaction.speech = None;
                    }
                    reaction.description = format!("build/test passed{duration}");
                }
            } else {
                reaction.behavior = Behavior::Celebrate;
                reaction.tone = Tone::Quiet;
                reaction.speech = None;
                reaction.description =
                    format!("build/test passed{duration} · older repo event order unavailable");
            }
            if agent_driven {
                append_reaction_detail(reaction, "agent-driven");
            }
        }
        _ => {}
    }
}

/// Post-replay work may contain observations newer than this event. It owns
/// whether any wording may claim the whole debugging loop is now closed; the
/// immediate success/error fact and the eventual vigil remain distinct.
fn normalize_replayed_closure(
    reaction: &mut Reaction,
    kind: CommandKind,
    exit_code: Option<i32>,
    insight: &MemoryInsight,
) {
    match (kind, exit_code) {
        (CommandKind::BuildOrTest, Some(0)) if insight.current_work.open_failures > 0 => {
            reaction.speech = None;
            append_reaction_detail(reaction, "later repo failure remains open");
        }
        (CommandKind::BuildOrTest, Some(code))
            if code != 0 && insight.current_work.open_failures == 0 =>
        {
            append_reaction_detail(
                reaction,
                if insight.current_work.recovered_pending_push {
                    "ordered replay already contains a later recovery"
                } else {
                    "ordered repo history already contains a later closure"
                },
            );
        }
        (CommandKind::GitPush, Some(0)) => {
            let closed_current_loop = insight.event_order_exact
                && insight.push_after_recovery
                && insight.current_work.open_failures == 0
                && !insight.current_work.recovered_pending_push;
            if !closed_current_loop {
                reaction.speech = None;
            }
            if insight.current_work.open_failures > 0 {
                append_reaction_detail(reaction, "later repo failure remains open");
            } else if insight.current_work.recovered_pending_push {
                append_reaction_detail(reaction, "newer recovered work still awaits push");
            }
        }
        _ => {}
    }
}

fn mark_circadian_greeting(reaction: &mut Reaction, bucket: u8) {
    // The greeting is a once-per-window/session acknowledgement, not a
    // stronger stimulus. A more specific repo-home line wins, and evening
    // or night-shift sessions avoid calling 21:00 "morning".
    if reaction.speech.is_none() {
        reaction.speech = Some(if bucket < 4 { "早。" } else { "来了。" });
    }
    reaction.description.push_str(" · habitual working hours");
}

fn apply_growth_voice(
    reaction: &mut Reaction,
    stage: GrowthStage,
    recovered_failures: u32,
    agent_driven: bool,
) -> bool {
    // Age changes expression, never stimulus strength. An organism seasoned
    // by months and many debugging episodes keeps the same full recovery pose
    // and state transition, but no longer needs the exuberant sentence.
    if stage == GrowthStage::Seasoned
        && recovered_failures >= 3
        && !agent_driven
        && reaction.behavior == Behavior::CelebrateBig
    {
        reaction.speech = Some("嗯。");
        true
    } else {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryBadgeState {
    Persistent,
    Volatile,
    SaveFailed,
}

fn growth_badge(stage: GrowthStage, memory: MemoryBadgeState) -> &'static str {
    match (stage, memory) {
        (GrowthStage::Juvenile, MemoryBadgeState::Persistent) => "juvenile · repo memory · no LLM",
        (GrowthStage::Adult, MemoryBadgeState::Persistent) => "adult · repo memory · no LLM",
        (GrowthStage::Seasoned, MemoryBadgeState::Persistent) => "seasoned · repo memory · no LLM",
        (_, MemoryBadgeState::Volatile) => "volatile · no LLM",
        (GrowthStage::Juvenile, MemoryBadgeState::SaveFailed) => "juvenile · save failed · no LLM",
        (GrowthStage::Adult, MemoryBadgeState::SaveFailed) => "adult · save failed · no LLM",
        (GrowthStage::Seasoned, MemoryBadgeState::SaveFailed) => "seasoned · save failed · no LLM",
    }
}

const fn visual_growth_stage(stage: GrowthStage) -> VisualGrowthStage {
    match stage {
        GrowthStage::Juvenile => VisualGrowthStage::Juvenile,
        GrowthStage::Adult => VisualGrowthStage::Adult,
        GrowthStage::Seasoned => VisualGrowthStage::Seasoned,
    }
}

fn unusual_build_pace(typical_ms: Option<u64>, duration_ms: Option<u64>) -> Option<&'static str> {
    let (Some(typical), Some(duration)) = (typical_ms, duration_ms) else {
        return None;
    };
    if duration >= typical.saturating_mul(2) && duration >= typical.saturating_add(10_000) {
        Some("slower than usual here")
    } else if duration.saturating_mul(2) <= typical && typical >= duration.saturating_add(10_000) {
        Some("quicker than usual here")
    } else {
        None
    }
}

/// Watching/reaction poses and unresolved-work vigils may only occupy a full
/// blank body-height band below the latest terminal cursor edge. Recovery
/// vigils belong beside the prompt and use the ordinary idle safe band.
fn needs_output_clearance(mode: SurfaceMode, ambient: AmbientBehavior, vigil: RepoVigil) -> bool {
    matches!(mode, SurfaceMode::Watching | SurfaceMode::Reacting)
        || (mode == SurfaceMode::Idle
            && (matches!(vigil, RepoVigil::Failure | RepoVigil::Stuck)
                || matches!(
                    ambient,
                    AmbientBehavior::GuardFailure | AmbientBehavior::GuardStuck
                )))
}

/// Pick a point inside the live VTE without changing its allocation. The
/// right gutter is owned by the live scrollbar, so every pose stays left of
/// it. A surface that cannot fit the complete sprite fails closed.
#[cfg(test)]
fn surface_point(
    surface: SurfaceBox,
    mode: SurfaceMode,
    tempo: WanderTempo,
    ambient: AmbientBehavior,
    frame: u64,
) -> Option<SurfacePoint> {
    surface_point_for_vigil(surface, mode, tempo, ambient, RepoVigil::None, frame)
}

fn surface_point_for_vigil(
    surface: SurfaceBox,
    mode: SurfaceMode,
    tempo: WanderTempo,
    ambient: AmbientBehavior,
    vigil: RepoVigil,
    frame: u64,
) -> Option<SurfacePoint> {
    surface_point_for_territory(surface, mode, tempo, ambient, vigil, frame, None)
}

#[allow(clippy::too_many_arguments)]
fn surface_point_for_territory(
    surface: SurfaceBox,
    mode: SurfaceMode,
    tempo: WanderTempo,
    ambient: AmbientBehavior,
    vigil: RepoVigil,
    frame: u64,
    territory: Option<TerritoryHabit>,
) -> Option<SurfacePoint> {
    let cell_width = surface.cell_width.max(1);
    let cell_height = surface.cell_height.max(1);
    let margin_x = SURFACE_MARGIN.max(cell_width);
    let margin_y = SURFACE_MARGIN.max(cell_height);
    let min_width = surface
        .body_width
        .saturating_add(surface.right_gutter)
        .saturating_add(margin_x.saturating_mul(2));
    let min_height = surface
        .body_height
        .saturating_add(margin_y.saturating_mul(2));
    if surface.width < min_width
        || surface.height < min_height
        || surface.body_width <= 0
        || surface.body_height <= 0
    {
        return None;
    }

    let min_x = align_up(margin_x, cell_width);
    let max_x = align_down(
        surface
            .width
            .saturating_sub(surface.right_gutter)
            .saturating_sub(surface.body_width)
            .saturating_sub(margin_x),
        cell_width,
    );
    let min_y = align_up(margin_y, cell_height);
    let max_y = align_down(
        surface
            .height
            .saturating_sub(surface.body_height)
            .saturating_sub(margin_y),
        cell_height,
    );
    if max_x < min_x || max_y < min_y {
        return None;
    }
    let output_clear_y = if needs_output_clearance(mode, ambient, vigil) {
        // With no complete sprite row below the latest output, the inline card
        // carries the reaction instead. Overlaying terminal text is never the
        // fallback.
        below_output_y(surface)?
    } else {
        min_y
    };

    let (x, y) = match mode {
        SurfaceMode::Idle => {
            let span = max_x.saturating_sub(min_x);
            let wander_x = |step: i32| {
                align_down(
                    min_x.saturating_add(span.saturating_mul(step) / 40),
                    cell_width,
                )
            };
            let unresolved_vigil = matches!(vigil, RepoVigil::Failure | RepoVigil::Stuck);
            if unresolved_vigil {
                // Exhaustion may curl the body up, but cannot erase the
                // unresolved work's output boundary or reintroduce overlap.
                if ambient == AmbientBehavior::Sleep {
                    (min_x, output_clear_y)
                } else {
                    (max_x, output_clear_y)
                }
            } else {
                match ambient {
                    // Familiar repositories acquire one stable nest side. An
                    // unknown/merely-known checkout retains the quiet original
                    // bottom-left curl so a path cannot create visual noise.
                    AmbientBehavior::Sleep => (
                        territory.map_or(min_x, |habit| habit.nest_x(min_x, max_x)),
                        max_y,
                    ),
                    // Sit by the prompt edge, where the human works.
                    AmbientBehavior::Approach => (max_x, max_y),
                    // Unresolved work stays at the completed output edge. If no
                    // whole blank band exists below it, `below_output_y` above has
                    // already failed closed and the inline card carries the vigil.
                    AmbientBehavior::GuardFailure | AmbientBehavior::GuardStuck => {
                        (max_x, output_clear_y)
                    }
                    // A recovered build waiting for push is a quiet intention:
                    // stay beside the prompt instead of resuming random wandering.
                    AmbientBehavior::GuardRecovery | AmbientBehavior::GuardCautious => {
                        (max_x, max_y)
                    }
                    // Pace the restless cycle regardless of the idle tempo.
                    AmbientBehavior::Explore => (
                        wander_x(
                            wander_phase(
                                territory.map_or(frame, |habit| habit.route_frame(frame)),
                                WanderTempo::Restless,
                            )
                            .0,
                        ),
                        max_y,
                    ),
                    // Mostly sit at an edge, occasionally walk between them — the
                    // walk share follows the wander tempo, so a listless mind
                    // paces while a drowsy one lies still. It feels alive without
                    // turning ordinary terminal work into a perpetual desktop-pet
                    // animation.
                    AmbientBehavior::Idle => (
                        wander_x(
                            wander_phase(
                                territory.map_or(frame, |habit| habit.route_frame(frame)),
                                tempo,
                            )
                            .0,
                        ),
                        max_y,
                    ),
                }
            }
        }
        // The runtime hides accepted input before geometry; retain a defensive
        // safe coordinate if a future non-live caller asks for this mode.
        SurfaceMode::Typing => (max_x, min_y),
        // Watching holds the output edge steadily. Real output pulses still
        // advance the tail animation faster; a content-free global timer no
        // longer makes the whole body jump one terminal row every 300 ms.
        SurfaceMode::Watching => (max_x, output_clear_y),
        SurfaceMode::Reacting => (
            align_down(
                min_x.saturating_add(max_x.saturating_sub(min_x) / 2),
                cell_width,
            ),
            align_down(
                output_clear_y.saturating_add(max_y.saturating_sub(output_clear_y) / 2),
                cell_height,
            )
            .clamp(output_clear_y, max_y),
        ),
    };

    Some(SurfacePoint {
        x: f64::from(x.clamp(min_x, max_x)),
        y: f64::from(y.clamp(min_y, max_y)),
    })
}

/// Advance one axis toward its target in whole-cell steps: a quarter of the
/// remaining distance per frame (minimum one cell), so long trips start brisk
/// and ease out on arrival — roughly a second across a full pane at the 100ms
/// frame cadence. Never overshoots; within half a cell it snaps home.
fn approach(current: f64, target: f64, cell: f64) -> f64 {
    let cell = cell.max(1.0);
    let distance = target - current;
    if distance.abs() < cell * 0.5 {
        return target;
    }
    let remaining_cells = (distance.abs() / cell).ceil();
    let step_cells = (remaining_cells / 4.0).ceil().max(1.0);
    current + distance.signum() * step_cells * cell
}

fn align_down(value: i32, cell: i32) -> i32 {
    value.div_euclid(cell.max(1)).saturating_mul(cell.max(1))
}

fn align_up(value: i32, cell: i32) -> i32 {
    let cell = cell.max(1);
    value
        .saturating_add(cell.saturating_sub(1))
        .div_euclid(cell)
        .saturating_mul(cell)
}

/// Opaque identity of the Block pane hosting a correction card. Pointer
/// identity only — it carries no path, command, or content data.
pub(crate) fn pane_token(view: &Rc<TermView>) -> usize {
    Rc::as_ptr(view) as usize
}

/// Window-shared, content-free pulses from the command-correction card. The
/// organism learns only the accept/dismiss fact — never the failed command,
/// the proposed correction, or any output text.
pub(crate) struct OrganismCorrectionSignal {
    life: Rc<Cell<LifeState>>,
    accepted: Cell<Option<(usize, Instant)>>,
    dismiss_streak: Cell<u32>,
}

impl OrganismCorrectionSignal {
    pub(crate) fn new(life: Rc<Cell<LifeState>>) -> Rc<Self> {
        Rc::new(Self {
            life,
            accepted: Cell::new(None),
            dismiss_streak: Cell::new(0),
        })
    }

    /// `pane` scopes the acceptance to the Block pane hosting the card, so a
    /// command starting in any other pane can never claim the assist.
    pub(crate) fn note_accepted(&self, pane: usize) {
        self.dismiss_streak.set(0);
        self.accepted.set(Some((pane, Instant::now())));
        self.life
            .set(crate::organism::correction_accepted(self.life.get()));
    }

    /// Drop a pending acceptance whose command demonstrably did not run.
    pub(crate) fn revoke_accept(&self, pane: usize) {
        if self.accepted.get().is_some_and(|(id, _)| id == pane) {
            self.accepted.set(None);
        }
    }

    pub(crate) fn note_dismissed(&self) {
        let streak = self.dismiss_streak.get().saturating_add(1);
        self.dismiss_streak.set(streak);
        self.life.set(crate::organism::correction_dismissed(
            self.life.get(),
            streak,
        ));
    }

    /// Consume a fresh acceptance for the command that is about to start in
    /// the accepting pane, so one card vouches for at most one command there.
    fn take_recent_accept(&self, pane: usize, now: Instant) -> bool {
        match self.accepted.get() {
            Some((id, at)) if id == pane => {
                self.accepted.set(None);
                now.saturating_duration_since(at) < CORRECTION_ASSIST_WINDOW
            }
            _ => false,
        }
    }
}

/// How long the window must stay completely quiet (no accepted input, no
/// running command, no output) before the shared mind starts resting and
/// energy recovers.
const REST_ONSET: Duration = Duration::from_secs(60);

/// Window-shared, content-free activity aggregate driving the continuous
/// life tick: whether a human recently typed into any organism pane, how many
/// commands are running, when the window was last active at all, and a shared
/// tick clock so several pane bodies never multiply the homeostasis rates of
/// their one shared mind.
pub(crate) struct OrganismActivity {
    session_started: Instant,
    attention: RefCell<AttentionArbiter>,
    input_at: Cell<Option<Instant>>,
    commands_running: Cell<u32>,
    active_at: Cell<Option<Instant>>,
    ticked_at: Cell<Option<Instant>>,
    sleeping_bodies: Cell<u32>,
    circadian_profile: Cell<Option<CircadianProfile>>,
    circadian_profile_day: Cell<Option<i64>>,
    morning_greeted_session: Cell<Option<i64>>,
    growth: Cell<GrowthProgress>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CircadianRefresh {
    NotAttempted,
    Succeeded(i64),
    Failed,
}

impl OrganismActivity {
    pub(crate) fn new(
        circadian_profile: Option<CircadianProfile>,
        growth: GrowthProgress,
    ) -> Rc<Self> {
        // A fresh window counts as activity: rest only begins after a real
        // quiet stretch, never at attach time.
        Rc::new(Self {
            session_started: Instant::now(),
            attention: RefCell::new(AttentionArbiter::default()),
            input_at: Cell::new(None),
            commands_running: Cell::new(0),
            active_at: Cell::new(Some(Instant::now())),
            ticked_at: Cell::new(None),
            sleeping_bodies: Cell::new(0),
            circadian_profile: Cell::new(circadian_profile),
            // Force one fresh cross-window read at the first command, then at
            // most once per civil day for non-semantic commands.
            circadian_profile_day: Cell::new(None),
            morning_greeted_session: Cell::new(None),
            growth: Cell::new(growth),
        })
    }

    fn set_growth(&self, growth: GrowthProgress) {
        self.growth.set(growth);
    }

    /// Spend this window/session's attention immediately. Suppressed cues are
    /// deliberately forgotten; there is no deferred speech or animation queue.
    fn offer_attention(&self, cue: AttentionCue, now: Instant) -> bool {
        self.attention
            .borrow_mut()
            .offer(cue, now.saturating_duration_since(self.session_started))
    }

    fn growth(&self) -> GrowthProgress {
        self.growth.get()
    }

    fn set_circadian_profile(&self, profile: Option<CircadianProfile>, refresh: CircadianRefresh) {
        self.circadian_profile.set(profile);
        match refresh {
            CircadianRefresh::NotAttempted => {}
            CircadianRefresh::Succeeded(day) => self.circadian_profile_day.set(Some(day)),
            CircadianRefresh::Failed => self.circadian_profile_day.set(None),
        }
    }

    fn circadian_profile_needs_refresh(&self, day: i64) -> bool {
        self.circadian_profile_day.get() != Some(day)
    }

    fn circadian_phase(&self, bucket: u8) -> CircadianPhase {
        match self.circadian_profile.get() {
            None => CircadianPhase::Unlearned,
            Some(profile) if profile.contains(bucket) => CircadianPhase::InHours,
            Some(_) => CircadianPhase::OffHours,
        }
    }

    fn take_morning_greeting(&self, local: LocalCircadianTime, human_owned: bool) -> bool {
        let Some(profile) = self.circadian_profile.get() else {
            return false;
        };
        if !human_owned || !profile.contains(local.bucket) {
            return false;
        }
        let session = profile.session_day(local);
        if self.morning_greeted_session.get() == Some(session) {
            return false;
        }
        self.morning_greeted_session.set(Some(session));
        true
    }

    fn note_input(&self, now: Instant) {
        self.input_at.set(Some(now));
        self.active_at.set(Some(now));
    }

    fn note_output(&self, now: Instant) {
        self.active_at.set(Some(now));
    }

    fn command_started(&self, now: Instant) {
        self.commands_running
            .set(self.commands_running.get().saturating_add(1));
        self.active_at.set(Some(now));
    }

    fn command_finished(&self, now: Instant) {
        self.commands_running
            .set(self.commands_running.get().saturating_sub(1));
        self.active_at.set(Some(now));
    }

    fn user_active(&self, now: Instant) -> bool {
        self.input_at
            .get()
            .is_some_and(|at| now.saturating_duration_since(at) < HUMAN_INPUT_RETREAT)
    }

    fn resting(&self, now: Instant) -> bool {
        self.commands_running.get() == 0
            && self
                .active_at
                .get()
                .is_some_and(|at| now.saturating_duration_since(at) >= REST_ONSET)
    }

    /// No command is running in any organism pane of this window. Gates the
    /// sleep-regeneration path the way the prototype's build guard did.
    fn no_commands_running(&self) -> bool {
        self.commands_running.get() == 0
    }

    fn body_started_sleeping(&self) {
        self.sleeping_bodies
            .set(self.sleeping_bodies.get().saturating_add(1));
    }

    fn body_stopped_sleeping(&self) {
        self.sleeping_bodies
            .set(self.sleeping_bodies.get().saturating_sub(1));
    }

    /// Any pane-local mind is visibly curled up, and no command in the
    /// window is running. This aggregate keeps shared-life regeneration
    /// independent of which pane happens to claim the next timer slice.
    fn sleeping_rest(&self) -> bool {
        self.sleeping_bodies.get() > 0 && self.no_commands_running()
    }

    /// Seconds since the window was last active at all, feeding the ambient
    /// mind's sleep utility.
    fn idle_for_secs(&self, now: Instant) -> f32 {
        self.active_at
            .get()
            .map(|at| now.saturating_duration_since(at).as_secs_f32())
            .unwrap_or(0.0)
    }

    /// Claim the wall-clock slice since the previous claim, in seconds. The
    /// clock is shared: with several organism panes ticking, every slice is
    /// consumed exactly once, so simulated time tracks wall time no matter
    /// how many bodies the mind wears — while at least one body's frame clock
    /// runs. A long gap (hidden window, suspend) is claimed whole, and the
    /// reducer then simulates at most one second of it.
    fn tick_slice(&self, now: Instant) -> f32 {
        let dt = match self.ticked_at.get() {
            Some(previous) => now.saturating_duration_since(previous).as_secs_f32(),
            None => 0.0,
        };
        self.ticked_at.set(Some(now));
        dt
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct OrganismPaneToken(u64);

#[derive(Default)]
struct PresenceLedger {
    next: u64,
    registered: Vec<OrganismPaneToken>,
    owner: Option<OrganismPaneToken>,
}

impl PresenceLedger {
    fn reserve(&mut self) -> OrganismPaneToken {
        self.next = self
            .next
            .checked_add(1)
            .expect("ASCII organism pane tokens are never exhausted");
        OrganismPaneToken(self.next)
    }

    fn bind(&mut self, token: OrganismPaneToken) {
        if !self.registered.contains(&token) {
            self.registered.push(token);
        }
    }

    fn claim(&mut self, token: Option<OrganismPaneToken>) {
        self.owner = token.filter(|token| self.registered.contains(token));
    }

    fn unregister(&mut self, token: OrganismPaneToken) {
        self.registered.retain(|registered| *registered != token);
        if self.owner == Some(token) {
            self.owner = None;
        }
    }

    fn is_owner(&self, token: OrganismPaneToken) -> bool {
        self.owner == Some(token)
    }

    fn signal_target(&self, source: OrganismPaneToken) -> Option<OrganismPaneToken> {
        if !self.registered.contains(&source) {
            return None;
        }
        self.owner.filter(|owner| *owner != source)
    }
}

struct PresenceEntry {
    token: OrganismPaneToken,
    view: std::rc::Weak<TermView>,
    runtime: std::rc::Weak<OrganismRuntime>,
}

#[derive(Default)]
struct PresenceState {
    ledger: PresenceLedger,
    entries: Vec<PresenceEntry>,
}

/// Window-shared arbiter for the one spatial body. Every pane keeps its own
/// reducer and inline/sticky representations, but only the genuinely focused
/// local Block pane may opt its live overlay into visibility.
pub(crate) struct OrganismPresence {
    state: RefCell<PresenceState>,
}

impl OrganismPresence {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(PresenceState::default()),
        })
    }

    fn reserve(&self) -> OrganismPaneToken {
        self.state.borrow_mut().ledger.reserve()
    }

    fn bind(&self, token: OrganismPaneToken, view: &Rc<TermView>, runtime: &Rc<OrganismRuntime>) {
        let mut state = self.state.borrow_mut();
        state.ledger.bind(token);
        state.entries.push(PresenceEntry {
            token,
            view: Rc::downgrade(view),
            runtime: Rc::downgrade(runtime),
        });
    }

    fn focus_view(&self, focused: Option<&Rc<TermView>>) {
        let (changed, new_owner, live_entries) = {
            let mut state = self.state.borrow_mut();
            state
                .entries
                .retain(|entry| entry.view.strong_count() > 0 && entry.runtime.strong_count() > 0);
            let live_tokens: Vec<_> = state.entries.iter().map(|entry| entry.token).collect();
            state
                .ledger
                .registered
                .retain(|token| live_tokens.contains(token));
            if state
                .ledger
                .owner
                .is_some_and(|owner| !live_tokens.contains(&owner))
            {
                state.ledger.owner = None;
            }

            let new_owner = focused.and_then(|focused| {
                state.entries.iter().find_map(|entry| {
                    let view = entry.view.upgrade()?;
                    Rc::ptr_eq(&view, focused).then_some(entry.token)
                })
            });
            let changed = state.ledger.owner != new_owner;
            if changed {
                // Phase one is visible immediately after the borrow is
                // released: no old and new body may overlap during transfer.
                state.ledger.claim(None);
            }
            let live_entries = state
                .entries
                .iter()
                .filter_map(|entry| {
                    Some((entry.token, entry.view.upgrade()?, entry.runtime.upgrade()?))
                })
                .collect::<Vec<_>>();
            (changed, new_owner, live_entries)
        };
        if !changed {
            return;
        }

        for (_, view, runtime) in &live_entries {
            runtime.hide_live_body(view);
        }

        {
            let mut state = self.state.borrow_mut();
            state.ledger.claim(new_owner);
        }
        for (token, view, runtime) in &live_entries {
            if Some(*token) == new_owner {
                let now = Instant::now();
                runtime.refresh_surface(view, now);
                runtime.reconcile_sleeping_claim(view, now);
            }
            OrganismRuntime::rearm_surface_tick_for_focus(runtime, view);
        }
    }

    fn unregister(&self, token: OrganismPaneToken) {
        let view = {
            let mut state = self.state.borrow_mut();
            let view = state
                .entries
                .iter()
                .find(|entry| entry.token == token)
                .and_then(|entry| entry.view.upgrade());
            state.entries.retain(|entry| entry.token != token);
            state.ledger.unregister(token);
            view
        };
        if let Some(view) = view {
            view.set_live_organism_visible(false);
        }
    }

    fn is_owner(&self, token: OrganismPaneToken) -> bool {
        self.state.borrow().ledger.is_owner(token)
    }

    fn signal_from(&self, source: OrganismPaneToken, signal: PresenceSignal) {
        let target = {
            let state = self.state.borrow();
            let Some(target) = state.ledger.signal_target(source) else {
                return;
            };
            state.entries.iter().find_map(|entry| {
                if entry.token != target {
                    return None;
                }
                Some((entry.view.upgrade()?, entry.runtime.upgrade()?))
            })
        };
        // Never hold the coordinator's RefCell borrow across GTK/runtime work:
        // a cue may synchronously hide itself if geometry has gone stale.
        if let Some((view, runtime)) = target {
            OrganismRuntime::receive_presence_signal(&runtime, &view, signal, Instant::now());
        }
    }

    /// Reconcile every pane in this window that is already known to represent
    /// the same repo/day. The repository path is used only for coordinator-side
    /// routing; each reducer receives one content-free work snapshot.
    fn sync_repo_work(&self, repo: &str, day: i64, work: RepoWorkState) {
        let entries = {
            let state = self.state.borrow();
            state
                .entries
                .iter()
                .filter_map(|entry| Some((entry.view.upgrade()?, entry.runtime.upgrade()?)))
                .collect::<Vec<_>>()
        };
        for (view, runtime) in entries {
            let cwd = view.cwd();
            if repo_work_scope_matches(
                repo,
                day,
                runtime.last_repo_root.borrow().as_deref(),
                runtime.last_local_day.get(),
                runtime.last_repo_cwd.borrow().as_deref(),
                Some(&cwd),
            ) {
                OrganismRuntime::receive_repo_work_sync(&runtime, &view, work, Instant::now());
            }
        }
    }
}

/// Window-shared, content-free pulses from the Shell Agent lifecycle. Only
/// coarse [`AgentPulse`] phases reach the life state — never proposals,
/// commands, model output, or error text. Repeated renders of the same phase
/// are deduplicated so status refreshes cannot pump the state.
pub(crate) struct OrganismAgentSignal {
    life: Rc<Cell<LifeState>>,
    last: Cell<Option<AgentPulse>>,
}

impl OrganismAgentSignal {
    pub(crate) fn new(life: Rc<Cell<LifeState>>) -> Rc<Self> {
        Rc::new(Self {
            life,
            last: Cell::new(None),
        })
    }

    pub(crate) fn note_phase(&self, pulse: AgentPulse) {
        if self.last.replace(Some(pulse)) == Some(pulse) {
            return;
        }
        self.life
            .set(crate::organism::agent_pulse(self.life.get(), pulse));
    }
}

/// True when `cwd` is `root` itself or a path strictly inside it.
fn cwd_within(cwd: &str, root: &str) -> bool {
    if root == "/" {
        return cwd.starts_with('/');
    }
    cwd.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Decide whether a volatile (non-build) command is still working inside the
/// last known checkout: either its cwd sits under the canonical repo root, or
/// it equals the exact raw cwd that last resolved to that root. A different
/// subdirectory of a symlinked checkout matches neither arm and conservatively
/// falls back to a context reset.
fn same_checkout(root: Option<&str>, repo_cwd: Option<&str>, cwd: Option<&str>) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    root.is_some_and(|root| cwd_within(cwd, root)) || repo_cwd.is_some_and(|known| known == cwd)
}

/// A window-shared work-state update targets only panes already resolved to
/// the exact same repo on the exact same local day. Repository identity stays
/// in this UI coordinator; the reducer receives only the content-free typed
/// snapshot.
fn repo_work_scope_matches(
    repo: &str,
    day: i64,
    pane_repo: Option<&str>,
    pane_day: Option<i64>,
    pane_repo_cwd: Option<&str>,
    pane_cwd: Option<&str>,
) -> bool {
    pane_repo == Some(repo)
        && pane_day == Some(day)
        && same_checkout(pane_repo, pane_repo_cwd, pane_cwd)
}

struct OrganismRuntime {
    organism: RefCell<NativeOrganism>,
    motion: OrganismMotion,
    memory_badge: Cell<MemoryBadgeState>,
    shared_life: Rc<Cell<LifeState>>,
    activity: Rc<OrganismActivity>,
    presence: Rc<OrganismPresence>,
    presence_token: OrganismPaneToken,
    /// Live-only cross-pane orienting cue. It never enters the reducer, card,
    /// sticky header, or inline history.
    presence_cue: Cell<Option<PresenceCue>>,
    presence_cue_frame_origin: Cell<u64>,
    presence_cue_timer: RefCell<Option<gtk::glib::SourceId>>,
    /// Pane-local utility disposition; stepped only while this body is
    /// genuinely idle, interrupted the moment anything else claims it.
    ambient: RefCell<AmbientMind>,
    ambient_display: Cell<AmbientBehavior>,
    sleeping: Cell<bool>,
    active_memory_kind: Cell<Option<CommandKind>>,
    active_context_key: RefCell<Option<String>>,
    active_repo_context: RefCell<Option<RepoContext>>,
    /// Canonical root of the last resolved repository, plus the raw cwd that
    /// resolved to it. Greeting/shyness keys on this identity, never on the
    /// mixed root/cwd context key.
    last_repo_root: RefCell<Option<String>>,
    last_repo_cwd: RefCell<Option<String>>,
    /// UI-only spatial habit derived from the canonical repo identity. The
    /// path never enters the reducer or persistence through this field.
    territory: Cell<Option<TerritoryHabit>>,
    territory_intro_pending: Cell<bool>,
    territory_intro_until: Cell<Option<Instant>>,
    last_local_day: Cell<Option<i64>>,
    generation: Cell<u64>,
    settle_timer: RefCell<Option<gtk::glib::SourceId>>,
    surface_timer: RefCell<Option<gtk::glib::SourceId>>,
    surface_last_frame: Cell<Option<Instant>>,
    surface_frame: Cell<u64>,
    surface_behavior_frame_origin: Cell<u64>,
    visual_transition: Cell<Option<VisualTransition>>,
    /// One-shot bridges advance exactly once per Full-motion heartbeat. They
    /// deliberately do not share `surface_frame`, which output pulses may
    /// accelerate for ordinary watch animation.
    visual_transition_frame: Cell<u64>,
    last_live_behavior: Cell<Behavior>,
    transition_source_override: Cell<Option<Behavior>>,
    command_origin_behavior: Cell<Option<Behavior>>,
    /// Where the body currently stands on the live surface; `None` while
    /// hidden, so every reappearance snaps into place and the cat only ever
    /// walks where it can be seen.
    body_position: Cell<Option<(f64, f64)>>,
    /// The body moved last frame — drives gait frames during transit.
    body_in_transit: Cell<bool>,
    /// Surface/body geometry the standing spot was computed against. A resize,
    /// font/scrollbar change, or differently sized pose snaps to its fresh
    /// clamped point instead of interpolating from an out-of-band position.
    surface_signature: Cell<SurfaceSignature>,
    surface_behavior: Cell<Behavior>,
    last_surface_mode: Cell<Option<SurfaceMode>>,
    command_running: Cell<bool>,
    /// When the running command started, for the accompaniment label and the
    /// settled vigil pose.
    command_started_at: Cell<Option<Instant>>,
    /// The base status text rendered at the last reaction, so the elapsed
    /// suffix can be appended without accumulating.
    status_base: RefCell<String>,
    /// The running command is the Agent's; the watching pose crouches apart.
    agent_watching: Cell<bool>,
    last_human_input: Cell<Option<Instant>>,
    output_activity: Cell<bool>,
    output_rhythm: RefCell<OutputRhythmTracker>,
    visible_watch_rhythm: Cell<WatchRhythm>,
    /// The last rhythm boundary offered to attention. A rejected boundary is
    /// remembered until the sensed rhythm changes, so it is dropped rather
    /// than retried as a delayed animation on every heartbeat.
    offered_watch_rhythm: Cell<WatchRhythm>,
    card: gtk::Widget,
    sprite: Label,
    live_body: Label,
    sticky_avatar: Label,
    badge: Label,
    status: Label,
    state: Label,
}

impl OrganismRuntime {
    fn new(
        shared_life: Rc<Cell<LifeState>>,
        activity: Rc<OrganismActivity>,
        presence: Rc<OrganismPresence>,
        presence_token: OrganismPaneToken,
        motion: OrganismMotion,
        persistent: bool,
    ) -> Rc<Self> {
        let outer = GBox::new(Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.add_css_class("block-organism");
        outer.add_css_class("organism-quiet");
        outer.set_hexpand(true);
        outer.set_vexpand(false);
        outer.set_margin_top(3);
        outer.set_margin_bottom(3);
        outer.set_margin_start(8);
        outer.set_margin_end(8);
        outer.set_can_target(false);
        outer.set_focusable(false);

        let content = GBox::new(Orientation::Horizontal, 12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_top(8);
        content.set_margin_bottom(8);

        let sprite = Label::new(None);
        sprite.add_css_class("organism-sprite");
        sprite.set_width_chars(INLINE_SPRITE_SLOT_CHARS);
        sprite.set_xalign(0.0);
        sprite.set_yalign(0.5);
        sprite.set_selectable(false);
        content.append(&sprite);

        let detail = GBox::new(Orientation::Vertical, 3);
        detail.set_hexpand(true);
        let header = GBox::new(Orientation::Horizontal, 8);
        let title = Label::new(Some("ASCII organism"));
        title.add_css_class("organism-title");
        title.set_xalign(0.0);
        header.append(&title);
        let badge = Label::new(Some(if persistent {
            "repo memory · no LLM"
        } else {
            "volatile · no LLM"
        }));
        badge.add_css_class("organism-badge");
        badge.set_hexpand(true);
        badge.set_halign(gtk::Align::End);
        badge.set_max_width_chars(32);
        badge.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header.append(&badge);
        detail.append(&header);

        let status = Label::new(None);
        status.add_css_class("organism-status");
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_lines(2);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status.set_accessible_role(gtk::AccessibleRole::Status);
        detail.append(&status);

        let state = Label::new(None);
        state.add_css_class("organism-state");
        state.set_xalign(0.0);
        state.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.append(&state);
        content.append(&detail);
        outer.append(&content);

        let live_body = Label::new(None);
        live_body.set_widget_name("ascii-organism-live-body");
        live_body.add_css_class("organism-live-body");
        live_body.add_css_class("organism-quiet");
        live_body.set_xalign(0.0);
        live_body.set_yalign(0.0);
        live_body.set_selectable(false);
        live_body.set_can_target(false);
        live_body.set_focusable(false);
        live_body.set_accessible_role(gtk::AccessibleRole::Presentation);

        let sticky_avatar = Label::new(Some("/\\_/\\"));
        // Micro-poses are all five ASCII characters; pinning the width keeps
        // the sticky header steady even under a proportional fallback font.
        sticky_avatar.set_width_chars(5);
        sticky_avatar.set_widget_name("ascii-organism-sticky-avatar");
        sticky_avatar.add_css_class("organism-sticky-avatar");
        sticky_avatar.add_css_class("organism-active");
        sticky_avatar.set_selectable(false);
        sticky_avatar.set_can_target(false);
        sticky_avatar.set_focusable(false);
        sticky_avatar.set_accessible_role(gtk::AccessibleRole::Presentation);

        let runtime = Rc::new(Self {
            organism: RefCell::new(NativeOrganism::from_persisted_state(shared_life.get())),
            motion,
            memory_badge: Cell::new(if persistent {
                MemoryBadgeState::Persistent
            } else {
                MemoryBadgeState::Volatile
            }),
            shared_life,
            activity,
            presence,
            presence_token,
            presence_cue: Cell::new(None),
            presence_cue_frame_origin: Cell::new(0),
            presence_cue_timer: RefCell::new(None),
            ambient: RefCell::new(AmbientMind::default()),
            ambient_display: Cell::new(AmbientBehavior::Idle),
            sleeping: Cell::new(false),
            active_memory_kind: Cell::new(None),
            active_context_key: RefCell::new(None),
            active_repo_context: RefCell::new(None),
            last_repo_root: RefCell::new(None),
            last_repo_cwd: RefCell::new(None),
            territory: Cell::new(None),
            territory_intro_pending: Cell::new(false),
            territory_intro_until: Cell::new(None),
            last_local_day: Cell::new(None),
            generation: Cell::new(0),
            settle_timer: RefCell::new(None),
            surface_timer: RefCell::new(None),
            surface_last_frame: Cell::new(None),
            surface_frame: Cell::new(0),
            surface_behavior_frame_origin: Cell::new(0),
            visual_transition: Cell::new(None),
            visual_transition_frame: Cell::new(0),
            last_live_behavior: Cell::new(Behavior::Idle),
            transition_source_override: Cell::new(None),
            command_origin_behavior: Cell::new(None),
            body_position: Cell::new(None),
            body_in_transit: Cell::new(false),
            surface_signature: Cell::new((0, 0, 0, 0, 0, 0, 0)),
            surface_behavior: Cell::new(Behavior::Idle),
            last_surface_mode: Cell::new(None),
            command_running: Cell::new(false),
            command_started_at: Cell::new(None),
            status_base: RefCell::new(String::new()),
            agent_watching: Cell::new(false),
            last_human_input: Cell::new(None),
            output_activity: Cell::new(false),
            output_rhythm: RefCell::new(OutputRhythmTracker::default()),
            visible_watch_rhythm: Cell::new(WatchRhythm::Steady),
            offered_watch_rhythm: Cell::new(WatchRhythm::Steady),
            card: outer.upcast(),
            sprite,
            live_body,
            sticky_avatar,
            badge,
            status,
            state,
        });
        let idle = runtime.organism.borrow().idle_reaction();
        runtime.render(&idle);
        runtime
    }

    fn set_sleeping(&self, sleeping: bool) {
        if self.sleeping.replace(sleeping) == sleeping {
            return;
        }
        if sleeping {
            self.activity.body_started_sleeping();
        } else {
            self.activity.body_stopped_sleeping();
        }
    }

    /// Publish both visible sleep and the geometry-independent vigil-rest
    /// claim from one snapshot. Non-frame callbacks use this too, so clearing
    /// or acquiring a durable vigil cannot leave the window-wide rest counter
    /// stale until the next (possibly one-second) heartbeat.
    fn reconcile_sleeping_claim(&self, view: &TermView, now: Instant) {
        let mode = surface_mode(
            self.surface_behavior.get(),
            self.command_running.get(),
            self.human_input_age(now),
        );
        let alt_screen = view.live_organism_surface_metrics().alt_screen;
        let owner = self.presence.is_owner(self.presence_token);
        let ambient = self.ambient_display.get();
        let vigil = self.organism.borrow().repo_vigil();
        self.set_sleeping(sleeping_claim(
            owner,
            self.motion,
            alt_screen,
            self.body_position.get().is_some(),
            self.presence_cue.get().is_some(),
            mode,
            ambient,
            vigil,
        ));
    }

    fn clear_presence_cue(&self) {
        self.presence_cue.set(None);
        if let Some(source) = self.presence_cue_timer.borrow_mut().take() {
            source.remove();
        }
    }

    fn receive_presence_signal(
        runtime: &Rc<Self>,
        view: &Rc<TermView>,
        signal: PresenceSignal,
        now: Instant,
    ) {
        let mode = surface_mode(
            runtime.surface_behavior.get(),
            runtime.command_running.get(),
            runtime.human_input_age(now),
        );
        let alt_screen = view.live_organism_surface_metrics().alt_screen;
        if !can_show_presence_cue(
            runtime.presence.is_owner(runtime.presence_token),
            runtime.motion,
            alt_screen,
            runtime.body_position.get().is_some(),
            mode,
            runtime.organism.borrow().repo_vigil().is_active(),
        ) {
            return;
        }

        runtime.clear_presence_cue();
        let cue = match signal {
            PresenceSignal::BackgroundCommandFailed => PresenceCue::GlanceAside,
        };
        runtime.presence_cue.set(Some(cue));
        runtime
            .presence_cue_frame_origin
            .set(runtime.surface_frame.get());
        runtime.set_sleeping(false);
        runtime.refresh_surface(view, now);
        // A concurrent geometry/alternate-screen check may have failed closed
        // during refresh. Never leave a timer for a cue that was not shown.
        if runtime.presence_cue.get() != Some(cue) {
            return;
        }

        let runtime_weak = Rc::downgrade(runtime);
        let view_weak = Rc::downgrade(view);
        let source = gtk::glib::timeout_add_local_once(GLANCE_ASIDE_HOLD, move || {
            let Some(runtime) = runtime_weak.upgrade() else {
                return;
            };
            // Clear before refresh: if refresh fails closed, hide_live_body
            // must not try to remove the source that is currently firing.
            runtime.presence_cue_timer.borrow_mut().take();
            runtime.presence_cue.set(None);
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            let now = Instant::now();
            runtime.refresh_surface(&view, now);
            runtime.reconcile_sleeping_claim(&view, now);
        });
        *runtime.presence_cue_timer.borrow_mut() = Some(source);
    }

    /// Fold the memory layer's post-replay repo/day truth back into a pane.
    /// Active reactions are never interrupted; an already-idle card changes
    /// immediately so another pane's failure/recovery/push cannot leave a
    /// stale vigil or the wrong escalation tier.
    fn receive_repo_work_sync(
        runtime: &Rc<Self>,
        view: &Rc<TermView>,
        work: RepoWorkState,
        now: Instant,
    ) {
        let changed = runtime.organism.borrow_mut().sync_repo_work_state(work);
        if !changed {
            return;
        }
        // A durable work intention always outranks a live-only glance that
        // may have begun a few milliseconds before this sync arrived.
        runtime.clear_presence_cue();
        let vigil = runtime.organism.borrow().repo_vigil();
        let (territory_intro_pending, territory_intro_until) = territory_intro_after_repo_sync(
            runtime.territory_intro_pending.get(),
            runtime.territory_intro_until.get(),
            vigil,
        );
        runtime.territory_intro_pending.set(territory_intro_pending);
        runtime.territory_intro_until.set(territory_intro_until);
        // Revocation must be immediate even when an active command/reaction
        // makes the visual update wait. Such modes never own a sleep claim, and
        // the vigil above has already cancelled any queued/live first-look.
        runtime.set_sleeping(false);
        if runtime.command_running.get() {
            return;
        }
        let surface_behavior = runtime.surface_behavior.get();
        if surface_behavior != Behavior::Idle && !surface_behavior.is_repo_vigil() {
            return;
        }

        // Derive from the reducer after its defensive normalization instead
        // of trusting a future internal caller to construct the snapshot.
        let ambient = if vigil.is_active() {
            runtime.ambient.borrow_mut().step(
                runtime.shared_life.get(),
                runtime.activity.idle_for_secs(now),
                0.0,
                vigil,
            )
        } else {
            runtime.ambient.borrow_mut().interrupt();
            AmbientBehavior::Idle
        };
        runtime.ambient_display.set(ambient);
        let idle = runtime.organism.borrow().idle_reaction();
        runtime.render(&idle);
        runtime.refresh_surface(view, now);
        runtime.reconcile_sleeping_claim(view, now);
        view.insert_inline_notice(&runtime.card);
    }

    fn cancel_territory_intro(&self) {
        let (pending, active_until) = territory_intro_after_interruption(
            self.territory_intro_pending.get(),
            self.territory_intro_until.get(),
        );
        self.territory_intro_pending.set(pending);
        self.territory_intro_until.set(active_until);
    }

    fn reset_watch_rhythm_at_boundary(&self, now: Instant) {
        reset_output_rhythm_at_boundary(
            &mut self.output_rhythm.borrow_mut(),
            self.command_running.get(),
            now,
        );
        self.output_activity.set(false);
        self.visible_watch_rhythm.set(WatchRhythm::Steady);
        self.offered_watch_rhythm.set(WatchRhythm::Steady);
    }

    fn hide_live_body(&self, view: &TermView) {
        self.reset_watch_rhythm_at_boundary(Instant::now());
        self.clear_presence_cue();
        self.visual_transition.set(None);
        self.cancel_territory_intro();
        self.set_sleeping(false);
        self.body_position.set(None);
        self.body_in_transit.set(false);
        view.set_live_organism_visible(false);
    }

    fn bump_generation(&self) -> u64 {
        self.advance_generation(false)
    }

    /// A normal command finish is the sole generation boundary across which an
    /// arrival-time first-look may travel to its settle callback. A new command,
    /// lost execution, or any other superseding generation uses
    /// [`Self::bump_generation`] and drops it immediately instead of queueing it.
    fn bump_generation_for_command_finish(&self) -> u64 {
        self.advance_generation(true)
    }

    fn advance_generation(&self, preserve_territory_intro: bool) -> u64 {
        let territory_intro_pending =
            preserve_territory_intro && self.territory_intro_pending.get();
        self.clear_presence_cue();
        self.visual_transition.set(None);
        self.transition_source_override.set(None);
        self.territory_intro_pending.set(territory_intro_pending);
        self.territory_intro_until.set(None);
        if let Some(source) = self.settle_timer.borrow_mut().take() {
            source.remove();
        }
        let next = self.generation.get().wrapping_add(1);
        self.generation.set(next);
        next
    }

    fn render(&self, reaction: &Reaction) {
        let transition_from = self
            .transition_source_override
            .take()
            .unwrap_or_else(|| self.last_live_behavior.get());
        let transition =
            visual_transition_for_motion(self.motion, transition_from, reaction.behavior);
        self.visual_transition.set(transition);
        self.visual_transition_frame.set(0);
        self.surface_behavior_frame_origin
            .set(self.surface_frame.get());
        self.surface_behavior.set(reaction.behavior);
        self.refresh_inline_sprite();
        self.refresh_growth_badge();
        let status = match reaction.speech {
            Some(speech) => format!("{speech}  {}", reaction.description),
            None => reaction.description.clone(),
        };
        self.status.set_text(&status);
        self.status.set_tooltip_text(Some(&status));
        *self.status_base.borrow_mut() = status;
        self.refresh_state(self.organism.borrow().state());

        for class in TONE_CLASSES {
            self.card.remove_css_class(class);
            self.live_body.remove_css_class(class);
            self.sticky_avatar.remove_css_class(class);
        }
        let tone_class = match reaction.tone {
            Tone::Quiet => "organism-quiet",
            Tone::Active => "organism-active",
            Tone::Success => "organism-success",
            Tone::Error => "organism-error",
            Tone::Warning => "organism-warning",
        };
        self.card.add_css_class(tone_class);
        self.live_body.add_css_class(tone_class);
        self.sticky_avatar.add_css_class(tone_class);
    }

    fn refresh_growth_badge(&self) {
        let growth = self.activity.growth();
        let memory = self.memory_badge.get();
        let badge = growth_badge(growth.stage(), memory);
        if self.badge.text().as_str() != badge {
            self.badge.set_text(badge);
        }
        let tooltip = match memory {
            MemoryBadgeState::Persistent => format!(
                "{} remembered work day(s) · {} recovery episode(s)",
                growth.days_seen, growth.lifetime_recoveries
            ),
            MemoryBadgeState::Volatile => {
                "Growth is unavailable because repository memory could not be loaded".to_string()
            }
            MemoryBadgeState::SaveFailed => {
                "Repository memory could not be queued for durable storage".to_string()
            }
        };
        self.badge.set_tooltip_text(Some(&tooltip));
    }

    fn refresh_inline_sprite(&self) {
        let sprite = sprite_frame_with_context(
            RenderContext::new(self.surface_behavior.get(), BodyLanguage::default(), false)
                .with_growth_stage(visual_growth_stage(self.activity.growth().stage())),
            0,
        );
        if self.sprite.text().as_str() != sprite.as_ref() {
            self.sprite.set_text(sprite.as_ref());
        }
    }

    fn refresh_state(&self, state: LifeState) {
        let words = state_words(state);
        if self.state.text().as_str() != words {
            self.state.set_text(&words);
        }
        let detail = state_summary(state);
        if self.state.tooltip_text().as_deref() != Some(detail.as_str()) {
            self.state.set_tooltip_text(Some(&detail));
        }
    }

    fn human_input_age(&self, now: Instant) -> Option<Duration> {
        self.last_human_input
            .get()
            .map(|at| now.saturating_duration_since(at))
    }

    /// Day-scoped repo work and habituation state must expire even if a pane
    /// remains open and no new command arrives around midnight. Returns true
    /// only on a real boundary, never on first observation of the local day.
    fn roll_over_local_day(&self, day: i64) -> bool {
        let previous = self.last_local_day.replace(Some(day));
        if previous.is_none_or(|previous| previous == day) {
            return false;
        }
        let mut organism = self.organism.borrow_mut();
        organism.sync_state(self.shared_life.get());
        organism.roll_over_day();
        self.ambient.borrow_mut().interrupt();
        self.ambient_display.set(AmbientBehavior::Idle);
        self.set_sleeping(false);
        true
    }

    /// Retire a pane's repo binding once authoritative cwd tracking shows that
    /// it left that checkout. This runs even before a vigil exists so a
    /// later same-window broadcast cannot target a stale binding. The return
    /// value says whether a currently visible guard was actually released.
    fn clear_repo_work_after_leave(&self, cwd: &str) -> bool {
        let had_repo_context =
            self.last_repo_root.borrow().is_some() || self.last_repo_cwd.borrow().is_some();
        if !had_repo_context
            || same_checkout(
                self.last_repo_root.borrow().as_deref(),
                self.last_repo_cwd.borrow().as_deref(),
                Some(cwd),
            )
        {
            return false;
        }

        let released_vigil = self.organism.borrow().repo_vigil().is_active();
        {
            let mut organism = self.organism.borrow_mut();
            organism.sync_state(self.shared_life.get());
            organism.restore_repo_context(0, false, 0, 0);
            organism.clear_repo_arrival();
        }
        *self.last_repo_root.borrow_mut() = None;
        *self.last_repo_cwd.borrow_mut() = None;
        *self.active_context_key.borrow_mut() = None;
        self.territory.set(None);
        self.territory_intro_pending.set(false);
        self.territory_intro_until.set(None);
        if released_vigil {
            self.ambient.borrow_mut().interrupt();
            self.ambient_display.set(AmbientBehavior::Idle);
            self.set_sleeping(false);
        }
        released_vigil
    }

    /// If an asynchronous boundary clears the currently displayed guard,
    /// replace only that idle-like card. Active command reactions keep their
    /// semantic hold and will settle to ordinary idle in due course.
    fn render_released_vigil(&self) {
        if self.command_running.get() || !self.surface_behavior.get().is_repo_vigil() {
            return;
        }
        let idle = self.organism.borrow().idle_reaction();
        self.render(&idle);
    }

    fn refresh_surface(&self, view: &TermView, now: Instant) {
        // Growth is window-shared. Even static or unfocused bodies refresh
        // their badge on the low-frequency heartbeat after another pane ages
        // the organism into its next stage.
        self.refresh_growth_badge();
        self.refresh_inline_sprite();
        if self.motion == OrganismMotion::Static {
            // The inline card is the whole visual surface; the live body and
            // sticky avatar were never attached. Consume a first-look rather
            // than carrying it behind an unavailable surface.
            self.cancel_territory_intro();
            return;
        }
        let behavior = self.surface_behavior.get();
        let mode = surface_mode(
            behavior,
            self.command_running.get(),
            self.human_input_age(now),
        );
        if self.last_surface_mode.get() != Some(mode) {
            log::trace!("ASCII organism live-surface mode: {mode:?}");
            self.last_surface_mode.set(Some(mode));
        }
        let mut ambient = self.ambient_display.get();
        if self
            .territory_intro_until
            .get()
            .is_some_and(|until| now >= until)
        {
            self.territory_intro_until.set(None);
        }
        if mode == SurfaceMode::Idle && self.territory_intro_until.get().is_some() {
            // One silent, post-settle look around in a never-seen checkout. A
            // command/input/geometry boundary still preempts it normally.
            ambient = AmbientBehavior::Explore;
        }
        // A body still under way to its next pose walks there openly instead
        // of sliding in a seated (or curled) form.
        let in_transit = self.body_in_transit.get();
        let baseline_behavior = match mode {
            SurfaceMode::Idle if in_transit => Behavior::Idle,
            SurfaceMode::Idle => ambient.display(),
            SurfaceMode::Watching => watching_behavior(
                self.agent_watching.get(),
                self.command_started_at
                    .get()
                    .map(|started| now.saturating_duration_since(started)),
            ),
            SurfaceMode::Typing => Behavior::WatchCommand,
            SurfaceMode::Reacting => behavior,
        };
        let display_behavior =
            live_display_behavior(baseline_behavior, mode, self.presence_cue.get());
        // The continuous state shows through the ambient poses as body
        // language; reaction poses stay canonical. Gait runs while the body
        // is genuinely under way OR a wander leg is in progress — the leg
        // advances its target slower than once per frame, so transit alone
        // would stutter the walk animation. Calm motion freezes the frame at
        // zero: first frames only, no wandering, no bob, no flourishes.
        let (geometry_frame, baseline_frame, live_frame) = if self.motion == OrganismMotion::Full {
            let global = self.surface_frame.get();
            let (baseline, live) = animation_frames(
                global,
                self.surface_behavior_frame_origin.get(),
                self.presence_cue_frame_origin.get(),
                mode,
                self.presence_cue.get(),
            );
            (global, baseline, live)
        } else {
            (0, 0, 0)
        };
        let language = BodyLanguage::from_state(self.shared_life.get());
        let tempo = wander_tempo(language);
        let wander_walking = match ambient {
            AmbientBehavior::Idle => wander_phase(geometry_frame, tempo).1,
            AmbientBehavior::Explore => wander_phase(geometry_frame, WanderTempo::Restless).1,
            AmbientBehavior::Sleep
            | AmbientBehavior::Approach
            | AmbientBehavior::GuardFailure
            | AmbientBehavior::GuardStuck
            | AmbientBehavior::GuardRecovery
            | AmbientBehavior::GuardCautious => false,
        };
        let walking = mode == SurfaceMode::Idle && (in_transit || wander_walking);
        let growth = visual_growth_stage(self.activity.growth().stage());
        let rhythm = if mode == SurfaceMode::Watching {
            self.visible_watch_rhythm.get()
        } else {
            WatchRhythm::Steady
        };
        let mut transition = self.visual_transition.get();
        let transition_frame = self.visual_transition_frame.get();
        if transition.is_some_and(|arc| transition_frame >= arc.frame_count()) {
            self.visual_transition.set(None);
            transition = None;
        }
        let sprite_frame = if transition.is_some() {
            transition_frame
        } else {
            live_frame
        };
        let sprite = sprite_frame_with_context(
            RenderContext::new(display_behavior, language, walking)
                .with_growth_stage(growth)
                .with_watch_rhythm(rhythm)
                .with_transition(transition),
            sprite_frame,
        );
        if self.live_body.text().as_str() != sprite.as_ref() {
            self.live_body.set_text(sprite.as_ref());
        }
        self.last_live_behavior.set(display_behavior);
        // The sticky one-line form mirrors the same displayed behavior with a
        // fixed-width micro-pose, so scrollback readers see the mood too.
        // Cross-pane cues belong only to the one spatial body. The focused
        // pane's sticky and inline forms retain their own local semantics.
        let glyph = sticky_glyph_with_context(
            RenderContext::new(baseline_behavior, language, false)
                .with_growth_stage(growth)
                .with_watch_rhythm(rhythm),
            baseline_frame,
        );
        if self.sticky_avatar.text().as_str() != glyph.as_ref() {
            self.sticky_avatar.set_text(glyph.as_ref());
        }

        if !self.presence.is_owner(self.presence_token) {
            self.hide_live_body(view);
            // Sticky and inline forms remain pane-local evidence; only the
            // spatial overlay participates in one-body focus arbitration.
            self.sticky_avatar.set_visible(mode != SurfaceMode::Typing);
            return;
        }

        if suppress_live_body_for_focus(mode) {
            // Accepted human input owns the prompt completely. Hiding also
            // forgets the old position, so the body returns by snapping after
            // the retreat window instead of visibly running back into view.
            self.hide_live_body(view);
            self.sticky_avatar.set_visible(false);
            return;
        }

        let metrics = view.live_organism_surface_metrics();
        if metrics.alt_screen {
            // ActiveBlock owns the override and cleared desired visibility on
            // smcup. Keep it cleared through rmcup; a later heartbeat must
            // remeasure the primary screen before showing anything. Forget the
            // standing spot so that safe return snaps rather than walking
            // across a surface the body was never seen leaving.
            self.hide_live_body(view);
            self.sticky_avatar.set_visible(false);
            return;
        }
        // Keep the child measurable while the non-measuring overlay is hidden.
        // GTK reports zero requisition for an explicitly invisible Label,
        // which would otherwise make a tiny initial allocation self-locking.
        self.live_body.set_visible(true);
        let (_, body_width, _, _) = self.live_body.measure(Orientation::Horizontal, -1);
        let (_, body_height, _, _) = self.live_body.measure(Orientation::Vertical, body_width);
        // A size change can tighten the legal x/y band even when the terminal
        // itself did not resize. Snap before interpolating so a wider reaction
        // pose never spends intermediate frames inside the scrollbar gutter.
        let surface = SurfaceBox {
            width: metrics.width,
            height: metrics.height,
            right_gutter: metrics.right_gutter,
            cell_width: metrics.cell_width,
            cell_height: metrics.cell_height,
            body_width,
            body_height,
            cursor_row: metrics.cursor_row,
        };
        let signature = surface_signature(surface);
        if self.surface_signature.replace(signature) != signature {
            self.body_position.set(None);
            self.body_in_transit.set(false);
        }
        let vigil = self.organism.borrow().repo_vigil();
        let point = surface_point_for_territory(
            surface,
            mode,
            tempo,
            ambient,
            vigil,
            geometry_frame,
            self.territory.get(),
        );

        if let Some(target) = point {
            let previous = self.body_position.get();
            let (x, y, moved) = match previous {
                // Hidden or first placement: appear directly at the target.
                None => (target.x, target.y, false),
                // Calm motion never animates a walk; poses snap, and a snap
                // is not a walk — no transit gait afterwards.
                Some(_) if self.motion != OrganismMotion::Full => (target.x, target.y, false),
                Some((px, py)) => {
                    let x = approach(px, target.x, f64::from(metrics.cell_width.max(1)));
                    let mut y = approach(py, target.y, f64::from(metrics.cell_height.max(1)));
                    if needs_output_clearance(mode, ambient, vigil) {
                        // Never animate through the output band on the way to
                        // a safe below-output target. Horizontal travel may
                        // stay smooth while the safety-critical axis snaps.
                        if let Some(clear_y) = below_output_y(surface) {
                            y = y.max(f64::from(clear_y));
                        }
                    }
                    (x, y, (px, py) != (x, y))
                }
            };
            if view.move_live_organism_body(self.live_body.upcast_ref(), x, y) {
                self.body_in_transit.set(moved);
                self.body_position.set(Some((x, y)));
                view.set_live_organism_visible(true);
            } else {
                // A detached/reparenting surface is not a place the body can
                // visibly sleep. Fail closed until the next measured frame.
                self.hide_live_body(view);
            }
        } else {
            self.hide_live_body(view);
        }
        // The Block layer decides whether the sticky running header is active;
        // this child only follows the same accepted-input retreat window.
        self.sticky_avatar.set_visible(mode != SurfaceMode::Typing);
    }

    fn start_surface_tick(runtime: &Rc<Self>, view: &Rc<TermView>) {
        let owner = runtime.presence.is_owner(runtime.presence_token);
        let alt_screen = view.live_organism_surface_metrics().alt_screen;
        Self::schedule_surface_frame(
            runtime,
            view,
            surface_frame_delay(runtime.motion, owner, alt_screen, false),
        );
    }

    fn rearm_surface_tick_for_focus(runtime: &Rc<Self>, view: &Rc<TermView>) {
        let owner = runtime.presence.is_owner(runtime.presence_token);
        let alt_screen = view.live_organism_surface_metrics().alt_screen;
        let (source, delay) = {
            let mut slot = runtime.surface_timer.borrow_mut();
            let Some(delay) =
                focus_transfer_rearm_delay(runtime.motion, owner, alt_screen, slot.is_some())
            else {
                // A fired callback takes the id before doing any work. It will
                // see the new owner and choose its next delay at the tail; a
                // second source here would otherwise escape the single slot.
                return;
            };
            (
                slot.take()
                    .expect("a pending surface timer keeps its source id in the slot"),
                delay,
            )
        };
        source.remove();
        Self::schedule_surface_frame(runtime, view, delay);
    }

    /// Self-rescheduling frame driver. A glib timeout wakes at most ten times
    /// a second — unlike a frame-clock tick callback it never forces the
    /// window's frame clock to run at full rate — and it drops to a
    /// one-second heartbeat while the mind rests, the body is static, or this
    /// pane does not own the one live presence.
    fn schedule_surface_frame(runtime: &Rc<Self>, view: &Rc<TermView>, delay: Duration) {
        debug_assert!(
            runtime.surface_timer.borrow().is_none(),
            "a surface runtime must never own two pending frame sources"
        );
        let runtime_weak = Rc::downgrade(runtime);
        let view_weak = Rc::downgrade(view);
        let source = gtk::glib::timeout_add_local_once(delay, move || {
            let Some(runtime) = runtime_weak.upgrade() else {
                return;
            };
            // Cleared before ANY other guard: a runtime kept alive past its
            // view (agent-lost idle closures, parser-owned callback vectors)
            // must never leave this fired source's id behind for Drop to
            // remove — glib panics on removing a dead source.
            runtime.surface_timer.borrow_mut().take();
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            let now = Instant::now();
            let previous_frame = runtime.surface_last_frame.replace(Some(now));
            let mode = surface_mode(
                runtime.surface_behavior.get(),
                runtime.command_running.get(),
                runtime.human_input_age(now),
            );
            let alt_screen = view.live_organism_surface_metrics().alt_screen;
            // Day rollover is pane-local even though physiology is shared.
            // Every runtime must retire its own repo/day vigil at midnight;
            // tying this to the one pane that wins the shared tick slice would
            // leave the other panes visibly guarding yesterday's work.
            let local = local_circadian_time_at_ms(unix_ms());
            if runtime.roll_over_local_day(local.day) {
                runtime.render_released_vigil();
            }

            // Publish this pane's currently visible sleep state before the
            // shared slice is claimed. Regeneration then depends on the
            // window aggregate, never on which body's callback runs first.
            runtime.reconcile_sleeping_claim(&view, now);

            // Continuous homeostasis: claim this pane's slice of the shared
            // clock and evolve the one shared mind. Persistence is untouched —
            // the evolved state only reaches disk with the next lifecycle
            // event, exactly as before.
            let dt = runtime.activity.tick_slice(now);
            if dt > 0.0 {
                let mut life = runtime.shared_life.get();
                life.tick(
                    dt,
                    runtime.activity.user_active(now),
                    runtime.activity.resting(now) || runtime.activity.sleeping_rest(),
                    runtime.activity.circadian_phase(local.bucket),
                );
                runtime.shared_life.set(life);
                runtime.refresh_state(life);
            }
            // The ambient mind runs on this pane's own frame cadence, so its
            // hold timers follow wall time however many panes share the tick
            // clock above.
            if mode == SurfaceMode::Idle {
                let pane_dt = previous_frame
                    .map(|last| now.saturating_duration_since(last).as_secs_f32())
                    .unwrap_or(0.0);
                let ambient = runtime.ambient.borrow_mut().step(
                    runtime.shared_life.get(),
                    runtime.activity.idle_for_secs(now),
                    pane_dt,
                    runtime.organism.borrow().repo_vigil(),
                );
                runtime.ambient_display.set(ambient);
            } else {
                runtime.ambient.borrow_mut().interrupt();
                runtime.ambient_display.set(AmbientBehavior::Idle);
            }
            let desired_rhythm = runtime
                .output_rhythm
                .borrow_mut()
                .sample(now, runtime.command_running.get());
            let rhythm_plan = watch_rhythm_plan(
                desired_rhythm,
                runtime.visible_watch_rhythm.get(),
                runtime.offered_watch_rhythm.get(),
            );
            let rhythm_context_presentable = watch_rhythm_context_presentable(
                runtime.presence.is_owner(runtime.presence_token),
                runtime.motion,
                mode,
                alt_screen,
            );
            // Record the sensing edge before any presentation/arbitration test.
            // An invisible or rejected edge is consumed, never held for later.
            runtime.offered_watch_rhythm.set(rhythm_plan.offered);
            runtime
                .visible_watch_rhythm
                .set(if rhythm_context_presentable {
                    rhythm_plan.visible
                } else {
                    WatchRhythm::Steady
                });
            if runtime.visual_transition.get().is_some() {
                runtime
                    .visual_transition_frame
                    .set(runtime.visual_transition_frame.get().saturating_add(1));
            }
            let pulse = u64::from(runtime.output_activity.replace(false));
            runtime
                .surface_frame
                .set(runtime.surface_frame.get().wrapping_add(1 + pulse));
            runtime.refresh_surface(&view, now);

            // Geometry and GTK mapping are known only after the neutral frame
            // has performed its fail-closed placement. Spend shared attention
            // only if either the safe live body or its sticky form can actually
            // be presented now. A successful edge gets one immediate repaint;
            // all rhythm families retain the same bounding box.
            let rhythm_surface_presentable = || {
                watch_rhythm_surface_presentable(
                    rhythm_context_presentable,
                    runtime.live_body.is_mapped() || runtime.sticky_avatar.is_mapped(),
                )
            };
            if !rhythm_surface_presentable() {
                runtime.visible_watch_rhythm.set(WatchRhythm::Steady);
            } else if let Some(offered) = rhythm_plan.attention_offer {
                if runtime
                    .activity
                    .offer_attention(AttentionCue::LongCommandChange, now)
                {
                    runtime.visible_watch_rhythm.set(offered);
                    runtime.refresh_surface(&view, now);
                    if !rhythm_surface_presentable() {
                        // A concurrent fail-closed placement cannot make the
                        // admitted edge pending; consume it at neutral instead.
                        runtime.visible_watch_rhythm.set(WatchRhythm::Steady);
                    }
                }
            }
            runtime.reconcile_sleeping_claim(&view, now);
            // Accompaniment: a long-running command's card counts the time
            // spent watching together. Text only — no pose escalation, no
            // interruption, and short commands never show it.
            if runtime.command_running.get() {
                if let Some(started) = runtime.command_started_at.get() {
                    let elapsed = now.saturating_duration_since(started);
                    if elapsed >= ACCOMPANY_LABEL_ONSET {
                        let status = format!(
                            "{} · {} in",
                            runtime.status_base.borrow(),
                            elapsed_label(elapsed)
                        );
                        if runtime.status.text().as_str() != status {
                            runtime.status.set_text(&status);
                            // Keep hover text in step: a narrow card may
                            // ellipsize the suffix out of the label itself.
                            runtime.status.set_tooltip_text(Some(&status));
                        }
                    }
                }
            }
            let next = surface_frame_delay(
                runtime.motion,
                runtime.presence.is_owner(runtime.presence_token),
                alt_screen,
                runtime.activity.resting(now),
            );
            Self::schedule_surface_frame(&runtime, &view, next);
        });
        *runtime.surface_timer.borrow_mut() = Some(source);
    }

    fn mark_volatile(&self) {
        self.memory_badge.set(MemoryBadgeState::SaveFailed);
        self.refresh_growth_badge();
    }

    fn settle_later(
        runtime: &Rc<Self>,
        view: std::rc::Weak<TermView>,
        generation: u64,
        hold: Duration,
    ) {
        let runtime_weak = Rc::downgrade(runtime);
        let source = gtk::glib::timeout_add_local_once(hold, move || {
            let Some(runtime) = runtime_weak.upgrade() else {
                return;
            };
            runtime.settle_timer.borrow_mut().take();
            if runtime.generation.get() != generation {
                return;
            }
            let view = view.upgrade();
            if let Some(view) = view.as_ref() {
                runtime.clear_repo_work_after_leave(&view.cwd());
            }
            let (idle, vigil) = {
                let mut organism = runtime.organism.borrow_mut();
                // Settle must not jump the state line back to the finish-time
                // snapshot; pull the tick-evolved shared state first.
                organism.sync_state(runtime.shared_life.get());
                (organism.idle_reaction(), organism.repo_vigil())
            };
            // Do not leave a one-heartbeat flash of generic idle between the
            // reaction and its durable repo intention, especially in Calm or
            // Static mode where that heartbeat is deliberately slow.
            let ambient = runtime.ambient.borrow_mut().step(
                runtime.shared_life.get(),
                runtime.activity.idle_for_secs(Instant::now()),
                0.0,
                vigil,
            );
            runtime.ambient_display.set(ambient);
            if should_begin_territory_intro(
                runtime.territory_intro_pending.replace(false),
                runtime.territory.get(),
                vigil,
            ) {
                runtime
                    .territory_intro_until
                    .set(Some(Instant::now() + TERRITORY_INTRO_HOLD));
            }
            runtime.render(&idle);
            if let Some(view) = view {
                let now = Instant::now();
                runtime.refresh_surface(&view, now);
                runtime.reconcile_sleeping_claim(&view, now);
                view.insert_inline_notice(&runtime.card);
            }
        });
        *runtime.settle_timer.borrow_mut() = Some(source);
    }
}

impl Drop for OrganismRuntime {
    fn drop(&mut self) {
        self.clear_presence_cue();
        self.presence.unregister(self.presence_token);
        self.set_sleeping(false);
        // A pane closed mid-command must return its slot in the shared
        // running-command count, or the mind could never rest again.
        if self.command_running.get() {
            self.activity.command_finished(Instant::now());
        }
        if let Some(source) = self.settle_timer.get_mut().take() {
            source.remove();
        }
        if let Some(source) = self.surface_timer.get_mut().take() {
            source.remove();
        }
    }
}

fn state_summary(state: LifeState) -> String {
    format!(
        "E{:02} M{:02} C{:02} B{:02} S{:02} N{:02} A{:02} F{:02}",
        percent(state.energy),
        percent(state.mood),
        percent(state.curiosity),
        percent(state.boredom),
        percent(state.stress),
        percent(state.social_need),
        percent(state.attachment),
        percent(state.confidence),
    )
}

fn state_words(state: LifeState) -> String {
    let mut words = Vec::with_capacity(3);
    if state.energy < 0.30 {
        words.push("sleepy");
    }
    if state.stress > 0.60 {
        words.push("tense");
    }
    if state.mood > 0.72 {
        words.push("bright");
    } else if state.mood < 0.30 {
        words.push("subdued");
    }
    if state.curiosity > 0.70 {
        words.push("curious");
    }
    if state.boredom > 0.75 {
        words.push("restless");
    }
    if state.social_need > 0.70 {
        words.push("lonely");
    }
    if state.attachment > 0.75 {
        words.push("close");
    }
    if state.confidence < 0.35 {
        words.push("unsure");
    } else if state.confidence > 0.75 {
        words.push("assured");
    }
    words.truncate(3);
    if words.is_empty() {
        "steady".to_string()
    } else {
        words.join(" · ")
    }
}

fn percent(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// Window-scoped state shared by every Relm4 Block component.
///
/// The organism reducer and persistence layer are deliberately toolkit-shell
/// agnostic.  Forge owns these values from its GTK4 `UiState`; Anvil keeps the
/// same state behind this small coordinator and lets Relm4 remain the only
/// application state machine.
pub(crate) struct OrganismHub {
    config: Rc<RefCell<crate::config::Config>>,
    organism_memory: Rc<RefCell<Option<crate::organism_memory::OrganismMemory>>>,
    organism_life: Rc<Cell<LifeState>>,
    organism_correction: Rc<OrganismCorrectionSignal>,
    organism_activity: Rc<OrganismActivity>,
    organism_presence: Rc<OrganismPresence>,
    organism_agent: Rc<OrganismAgentSignal>,
}

impl OrganismHub {
    pub(crate) fn new(config: Rc<RefCell<crate::config::Config>>) -> Rc<Self> {
        // Load even while the feature is disabled. A live config reload can
        // then opt a newly-created Block pane in without silently discarding
        // its existing local memory. Corrupt/future/unsafe state fails closed.
        let organism_memory = match crate::organism_memory::OrganismMemory::load_default() {
            Ok(memory) => Some(memory),
            Err(error) => {
                log::error!("ASCII organism memory disabled: {error}");
                None
            }
        };
        let organism_life = Rc::new(Cell::new(
            organism_memory
                .as_ref()
                .map(crate::organism_memory::OrganismMemory::life_state)
                .unwrap_or_default(),
        ));
        let circadian = organism_memory
            .as_ref()
            .and_then(|memory| memory.circadian_profile_at(crate::organism_memory::unix_ms()));
        let growth = organism_memory
            .as_ref()
            .map(crate::organism_memory::OrganismMemory::growth_progress)
            .unwrap_or_default();

        Rc::new(Self {
            config,
            organism_memory: Rc::new(RefCell::new(organism_memory)),
            organism_correction: OrganismCorrectionSignal::new(organism_life.clone()),
            organism_activity: OrganismActivity::new(circadian, growth),
            organism_presence: OrganismPresence::new(),
            organism_agent: OrganismAgentSignal::new(organism_life.clone()),
            organism_life,
        })
    }

    pub(crate) fn correction_signal(&self) -> Rc<OrganismCorrectionSignal> {
        self.organism_correction.clone()
    }

    pub(crate) fn agent_signal(&self) -> Rc<OrganismAgentSignal> {
        self.organism_agent.clone()
    }

    /// Revoke synchronously before a tab/pane ownership transition.
    pub(crate) fn revoke_organism_presence(&self) {
        self.organism_presence.focus_view(None);
    }

    /// Relm4 already owns the selected tab/pane identity, so it hands the
    /// resolved Block view to the shared visibility arbiter directly.
    pub(crate) fn focus_view(&self, focused: Option<&Rc<TermView>>) {
        self.organism_presence.focus_view(focused);
    }

    pub(crate) fn attach_ascii_organism_to_view(&self, view: &Rc<TermView>, remote: bool) {
        if remote || !self.config.borrow().ascii_organism_enabled {
            return;
        }

        let persistent = self.organism_memory.borrow().is_some();
        // An explicit config level wins; otherwise follow the desktop's
        // animation preference — a reduced-motion desktop gets a calm body.
        let motion = self
            .config
            .borrow()
            .ascii_organism_motion
            .unwrap_or_else(|| {
                let animations = gtk::Settings::default()
                    .map(|settings| settings.is_gtk_enable_animations())
                    .unwrap_or(true);
                if animations {
                    OrganismMotion::Full
                } else {
                    OrganismMotion::Calm
                }
            });
        let presence_token = self.organism_presence.reserve();
        let runtime = OrganismRuntime::new(
            self.organism_life.clone(),
            self.organism_activity.clone(),
            self.organism_presence.clone(),
            presence_token,
            motion,
            persistent,
        );
        self.organism_presence.bind(presence_token, view, &runtime);
        view.insert_inline_notice(&runtime.card);
        if motion != OrganismMotion::Static {
            if !view.put_live_organism_body(runtime.live_body.upcast_ref(), 0.0, 0.0) {
                log::warn!("could not attach ASCII organism to the live terminal surface");
            }
            if !view.put_sticky_organism_avatar(runtime.sticky_avatar.upcast_ref()) {
                log::warn!("could not attach ASCII organism to the sticky running header");
            }
        }
        // Per-body seeding keeps split-window bodies from napping and pacing
        // in perfect lockstep.
        *runtime.ambient.borrow_mut() = AmbientMind::seeded(pane_token(view) as u64);
        runtime.refresh_surface(view, Instant::now());
        OrganismRuntime::start_surface_tick(&runtime, view);

        {
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            view.connect_human_input(move |_kind| {
                let now = Instant::now();
                let entering_retreat = runtime
                    .human_input_age(now)
                    .is_none_or(|age| age >= HUMAN_INPUT_RETREAT);
                runtime.last_human_input.set(Some(now));
                runtime.activity.note_input(now);
                runtime.reset_watch_rhythm_at_boundary(now);
                runtime.clear_presence_cue();
                runtime.visual_transition.set(None);
                runtime.cancel_territory_intro();
                runtime.set_sleeping(false);
                if entering_retreat {
                    // Keep the accepted-input hot path O(1): hide once, then
                    // keep the single frame callback suppressed for the whole
                    // retreat window. Repeated keys only extend time. Hiding
                    // the standing spot makes the later reappearance a snap —
                    // no typing-triggered run competes with the prompt.
                    runtime.body_position.set(None);
                    runtime.body_in_transit.set(false);
                    if let Some(view) = view_weak.upgrade() {
                        // Clear desired visibility even behind the alternate-
                        // screen override, so rmcup cannot briefly restore a
                        // stale pre-input position before the next frame.
                        view.set_live_organism_visible(false);
                    }
                }
            });
        }

        {
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            view.connect_alt_screen_transition(move |transition| {
                let now = Instant::now();
                runtime.reset_watch_rhythm_at_boundary(now);
                runtime.clear_presence_cue();
                runtime.visual_transition.set(None);
                if transition == AltScreenTransition::Entered {
                    runtime.cancel_territory_intro();
                }
                runtime.set_sleeping(false);
                runtime.body_position.set(None);
                runtime.body_in_transit.set(false);
                if let Some(view) = view_weak.upgrade() {
                    view.set_live_organism_visible(false);
                }
            });
        }

        {
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            view.connect_activity(move || {
                let now = Instant::now();
                let view = view_weak.upgrade();
                let rhythm_presentable_at_origin = view.as_ref().is_some_and(|view| {
                    watch_rhythm_context_presentable(
                        runtime.presence.is_owner(runtime.presence_token),
                        runtime.motion,
                        surface_mode(
                            runtime.surface_behavior.get(),
                            runtime.command_running.get(),
                            runtime.human_input_age(now),
                        ),
                        view.live_organism_surface_metrics().alt_screen,
                    )
                });
                if rhythm_presentable_at_origin {
                    runtime.output_activity.set(true);
                    runtime.output_rhythm.borrow_mut().note_output(now);
                } else {
                    runtime.reset_watch_rhythm_at_boundary(now);
                }
                runtime.activity.note_output(now);
                runtime.clear_presence_cue();
                // Preserve arrival eligibility while its introducing command
                // emits ordinary output, but output that interrupts an already
                // visible first-look consumes that one-shot cue immediately.
                if runtime.territory_intro_until.get().is_some() {
                    runtime.cancel_territory_intro();
                }
                runtime.set_sleeping(false);
                // Output is reported just before the bytes update terminal
                // geometry. Hide in O(1) now; the coalesced frame remeasures
                // and restores the body below the new cursor edge.
                runtime.body_position.set(None);
                runtime.body_in_transit.set(false);
                if let Some(view) = view {
                    view.set_live_organism_visible(false);
                }
            });
        }

        {
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            view.connect_cwd_changed(move |_display_cwd, _external| {
                // A running command still owns its start-time repo context for
                // the authoritative finish. Its settle path rechecks the cwd;
                // idle cwd changes can release the guard immediately. The
                // callback argument is display-sanitized; identity must use the
                // raw cwd that TermView stored before notifying observers.
                if runtime.command_running.get() {
                    return;
                }
                let Some(view) = view_weak.upgrade() else {
                    return;
                };
                if runtime.clear_repo_work_after_leave(&view.cwd()) {
                    runtime.render_released_vigil();
                    runtime.refresh_surface(&view, Instant::now());
                }
            });
        }

        {
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            let memory = self.organism_memory.clone();
            let shared_life = self.organism_life.clone();
            let correction = self.organism_correction.clone();
            let pane = pane_token(view);
            view.connect_command_started(move |event| {
                let now = Instant::now();
                let command_origin = runtime.surface_behavior.get();
                runtime.bump_generation();
                runtime.set_sleeping(false);
                runtime
                    .command_origin_behavior
                    .set(command_origin.is_repo_vigil().then_some(command_origin));
                runtime.output_rhythm.borrow_mut().start(now);
                runtime.visible_watch_rhythm.set(WatchRhythm::Steady);
                runtime.offered_watch_rhythm.set(WatchRhythm::Steady);
                runtime.output_activity.set(false);
                // Transition-guarded so a repeated start can never over-count
                // the shared running total.
                if !runtime.command_running.replace(true) {
                    runtime.activity.command_started(now);
                }
                runtime.command_started_at.set(Some(now));
                // Identity-verified at CommandStart: content-free fact only.
                let agent_driven = view_weak
                    .upgrade()
                    .is_some_and(|view| view.agent_command_active());
                runtime.agent_watching.set(agent_driven);
                // Enter's retreat protected the editable prompt; once OSC C
                // establishes a running command, the dedicated watching pose
                // is already anchored away from that line.
                runtime.last_human_input.set(None);
                let kind = classify_command(&event.command);
                let semantic = matches!(kind, CommandKind::BuildOrTest | CommandKind::GitPush);
                runtime.active_memory_kind.set(Some(kind));
                let now_ms = unix_ms();
                let local = local_circadian_time_at_ms(now_ms);
                let (repo_context, circadian_profile, circadian_refresh, growth) = {
                    let mut memory_slot = memory.borrow_mut();
                    if let Some(memory) = memory_slot.as_mut() {
                        let refresh_requested =
                            semantic || runtime.activity.circadian_profile_needs_refresh(local.day);
                        let circadian_refresh = if refresh_requested {
                            match memory.refresh() {
                                Ok(()) => CircadianRefresh::Succeeded(local.day),
                                Err(error) => {
                                    log::error!("could not refresh ASCII organism memory: {error}");
                                    // Keep the last usable profile, but leave
                                    // this civil day unacknowledged so the next
                                    // command retries a transient read failure.
                                    CircadianRefresh::Failed
                                }
                            }
                        } else {
                            CircadianRefresh::NotAttempted
                        };
                        let repo_context = if semantic {
                            memory.context_for_day(event.cwd.as_deref(), local.day)
                        } else {
                            None
                        };
                        (
                            repo_context,
                            memory.circadian_profile_at(now_ms),
                            circadian_refresh,
                            Some(memory.growth_progress()),
                        )
                    } else {
                        (None, None, CircadianRefresh::NotAttempted, None)
                    }
                };
                runtime
                    .activity
                    .set_circadian_profile(circadian_profile, circadian_refresh);
                if let Some(growth) = growth {
                    runtime.activity.set_growth(growth);
                }
                *runtime.active_repo_context.borrow_mut() = repo_context.clone();
                // The context key is the canonical repo root for build/push
                // commands. Volatile commands still inside the same checkout
                // reuse that root instead of their raw cwd, so interleaved
                // `ls`/`git status` from a subdirectory can never flap the key
                // and fake a context switch.
                let context_key = repo_context
                    .as_ref()
                    .map(|context| context.repo.clone())
                    .or_else(|| {
                        if same_checkout(
                            runtime.last_repo_root.borrow().as_deref(),
                            runtime.last_repo_cwd.borrow().as_deref(),
                            event.cwd.as_deref(),
                        ) {
                            runtime.last_repo_root.borrow().clone()
                        } else {
                            event.cwd.clone()
                        }
                    });
                let context_changed = *runtime.active_context_key.borrow() != context_key;
                *runtime.active_context_key.borrow_mut() = context_key;
                let has_repo_context = repo_context.is_some();
                let morning = runtime.activity.take_morning_greeting(local, !agent_driven);
                runtime.roll_over_local_day(local.day);
                let mut reaction = {
                    let mut organism = runtime.organism.borrow_mut();
                    organism.sync_state(shared_life.get());
                    if let Some(context) = repo_context {
                        debug_assert_eq!(context.day, local.day);
                        // Greeting/shyness keys on the resolved repository
                        // identity; the mixed root/cwd key cannot re-fire it.
                        let entered_new_repo = runtime.last_repo_root.borrow().as_deref()
                            != Some(context.repo.as_str());
                        organism.restore_repo_work_context(
                            context.work,
                            context.successes_today,
                            context.failures_today,
                        );
                        // Recompute the UI-only habit on every authoritative
                        // context refresh. A checkout that crosses the
                        // familiarity threshold while this pane stays in it
                        // should gain its home nest without requiring a leave
                        // and re-entry.
                        let territory =
                            TerritoryHabit::for_repo(&context.repo, context.familiarity_days);
                        runtime.territory.set(Some(territory));
                        if entered_new_repo {
                            let arrival = RepoArrival::from_familiarity(context.familiarity_days);
                            organism.note_repo_arrival(arrival);
                            runtime
                                .territory_intro_pending
                                .set(territory.is_unfamiliar());
                        }
                        *runtime.last_repo_root.borrow_mut() = Some(context.repo.clone());
                        *runtime.last_repo_cwd.borrow_mut() = event.cwd.clone();
                    } else if context_changed {
                        // A volatile/non-Git command genuinely left the last
                        // known checkout; never inherit a streak across a real
                        // context switch or spend a queued repo greeting in
                        // the unrelated directory.
                        organism.restore_repo_context(0, false, 0, 0);
                        organism.clear_repo_arrival();
                        *runtime.last_repo_root.borrow_mut() = None;
                        *runtime.last_repo_cwd.borrow_mut() = None;
                        runtime.territory.set(None);
                        runtime.territory_intro_pending.set(false);
                        runtime.territory_intro_until.set(None);
                    }
                    if !has_repo_context && semantic {
                        // A non-Git or temporarily memory-less build still
                        // gets a pane-local vigil, scoped conservatively to
                        // the exact raw cwd so `cd` can release it immediately.
                        *runtime.last_repo_root.borrow_mut() = None;
                        *runtime.last_repo_cwd.borrow_mut() = event.cwd.clone();
                        runtime.territory.set(None);
                    }
                    if correction.take_recent_accept(pane, Instant::now()) {
                        organism.note_assisted_command();
                    }
                    organism.set_agent_command(agent_driven);
                    let reaction = organism.command_started(kind);
                    shared_life.set(organism.state());
                    reaction
                };
                let greeting_candidate = morning || reaction.speech.is_some();
                if greeting_candidate
                    && runtime
                        .activity
                        .offer_attention(AttentionCue::Greeting, Instant::now())
                {
                    if morning {
                        mark_circadian_greeting(&mut reaction, local.bucket);
                    }
                } else if greeting_candidate {
                    reaction.speech = None;
                }
                runtime.render(&reaction);
                if let Some(view) = view_weak.upgrade() {
                    runtime.refresh_surface(&view, Instant::now());
                    view.insert_inline_notice(&runtime.card);
                }
            });
        }

        {
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            let memory = self.organism_memory.clone();
            let shared_life = self.organism_life.clone();
            view.connect_command_finished(move |event| {
                let generation = runtime.bump_generation_for_command_finish();
                if runtime.command_running.replace(false) {
                    runtime.activity.command_finished(Instant::now());
                }
                runtime.command_started_at.set(None);
                runtime.output_rhythm.borrow_mut().reset();
                runtime.visible_watch_rhythm.set(WatchRhythm::Steady);
                runtime.offered_watch_rhythm.set(WatchRhythm::Steady);
                runtime.output_activity.set(false);
                let agent_driven = runtime.agent_watching.replace(false);
                // Show the authoritative result for the complete hold window.
                // Any genuinely new prompt input will immediately replace this
                // with a fresh sliding retreat.
                runtime.last_human_input.set(None);
                let classified = classify_command(&event.command);
                let kind = if classified == CommandKind::Other {
                    runtime.active_memory_kind.take().unwrap_or(classified)
                } else {
                    runtime.active_memory_kind.take();
                    classified
                };
                let mut completion_attention =
                    completion_attention_cue(kind, event.exit_code, 0, agent_driven);
                let repo = runtime
                    .active_repo_context
                    .borrow_mut()
                    .take()
                    .map(|context| context.repo);
                let work_repo = repo.clone();
                // Refresh first, then timestamp this observation. Every event
                // incorporated by a potentially blocking refresh is therefore
                // an ordered predecessor of the context used by the reducer,
                // never a future event folded into an earlier timestamp.
                let refreshed = {
                    let mut memory_slot = memory.borrow_mut();
                    if let Some(memory) = memory_slot.as_mut() {
                        match memory.refresh() {
                            Ok(()) => true,
                            Err(error) => {
                                log::error!("could not refresh ASCII organism memory: {error}");
                                false
                            }
                        }
                    } else {
                        false
                    }
                };
                // Freeze one wall-clock sample for every remaining completion
                // decision. A command may span midnight while a dormant/static
                // pane misses the boundary heartbeat; yesterday's open failures
                // must not turn today's clean build into a false recovery, and
                // the persisted event must use this exact same day and bucket.
                let finished_at_ms = unix_ms();
                let finished_local = local_circadian_time_at_ms(finished_at_ms);
                runtime.roll_over_local_day(finished_local.day);
                // Rebuild pane-local counters from the canonical key captured
                // at start so habituation, sensitization, and physiology see
                // work completed by other windows during this command.
                let latest_repo_context = memory.borrow().as_ref().and_then(|memory| {
                    repo.as_deref()
                        .map(|repo| memory.context_for_repo_day(repo, finished_local.day))
                });
                let mut reaction = {
                    let mut organism = runtime.organism.borrow_mut();
                    organism.sync_state(shared_life.get());
                    if let Some(context) = latest_repo_context {
                        debug_assert_eq!(context.day, finished_local.day);
                        organism.restore_repo_work_context(
                            context.work,
                            context.successes_today,
                            context.failures_today,
                        );
                    }
                    let reaction =
                        organism.command_finished(classified, event.exit_code, event.duration_ms);
                    shared_life.set(organism.state());
                    reaction
                };
                let state = shared_life.get();
                let memory_event = MemoryEvent::at_ms_for_repo(
                    finished_at_ms,
                    kind,
                    event.exit_code,
                    repo,
                    state,
                    event.duration_ms,
                );
                let work_day = memory_event.day();
                debug_assert_eq!(work_day, finished_local.day);
                let mut work_sync = None;
                let mut refreshed_territory = None;
                if let Some(memory) = memory.borrow_mut().as_mut() {
                    let (insight, persist_result, retained) =
                        memory.apply_and_enqueue(memory_event);
                    if let Some(repo) = work_repo.as_deref() {
                        let context = memory.context_for_repo_day(repo, work_day);
                        refreshed_territory =
                            Some(TerritoryHabit::for_repo(repo, context.familiarity_days));
                        work_sync =
                            Some((repo.to_owned(), work_day, insight.current_work, retained));
                    }
                    runtime.activity.set_growth(memory.growth_progress());
                    runtime.activity.set_circadian_profile(
                        memory.circadian_profile_at(finished_at_ms),
                        if refreshed {
                            CircadianRefresh::Succeeded(finished_local.day)
                        } else {
                            CircadianRefresh::Failed
                        },
                    );
                    normalize_replayed_event(
                        &mut reaction,
                        kind,
                        event.exit_code,
                        event.duration_ms,
                        &insight,
                        agent_driven,
                    );
                    completion_attention = completion_attention_cue(
                        kind,
                        event.exit_code,
                        insight.recovered_failures,
                        agent_driven,
                    );
                    if kind == CommandKind::BuildOrTest
                        && event.exit_code == Some(0)
                        && insight.event_order_exact
                        && insight.likely_flaky
                    {
                        mark_likely_flaky(&mut reaction, agent_driven);
                        if let Some(cue) = remembered_insight_attention(reaction.speech.is_some()) {
                            completion_attention = Some(cue);
                        }
                    }
                    // A successful build measured against this repo's own
                    // baseline: one quiet sentence, no pose change.
                    if kind == CommandKind::BuildOrTest && event.exit_code == Some(0) {
                        if let Some(pace) =
                            unusual_build_pace(insight.typical_build_ms, event.duration_ms)
                        {
                            reaction.description.push_str(" · ");
                            reaction.description.push_str(pace);
                        }
                    }
                    if insight.faster_than_yesterday && !agent_driven {
                        reaction.speech = Some("这次比昨天快。");
                        reaction.description.push_str(" · remembered this repo");
                        completion_attention = remembered_insight_attention(true);
                    } else if insight.push_after_recovery
                        && reaction.speech.is_none()
                        && !agent_driven
                    {
                        // The build may have recovered before this window was
                        // restarted; repo memory still closes the loop.
                        reaction.speech = Some("收好了。");
                        completion_attention = Some(AttentionCue::Push);
                    }
                    if apply_growth_voice(
                        &mut reaction,
                        runtime.activity.growth().stage(),
                        insight.recovered_failures,
                        agent_driven,
                    ) {
                        completion_attention = Some(AttentionCue::Recovery);
                    }
                    // Run last: no later voice decorator may resurrect a
                    // closure claim that the final ordered work disproves.
                    normalize_replayed_closure(&mut reaction, kind, event.exit_code, &insight);
                    if let Err(error) = persist_result {
                        log::error!("could not queue ASCII organism memory: {error}");
                        runtime.mark_volatile();
                    }
                }
                if let Some(territory) = refreshed_territory {
                    runtime.territory.set(Some(territory));
                }
                if let Some((repo, day, work, retained)) = work_sync {
                    // The pane-local reducer handled completion in arrival
                    // order, while memory just replayed this repo/day in its
                    // stable total order. Fold that authoritative truth back
                    // before rendering, then reconcile other already-resolved
                    // panes without holding the memory RefCell borrow.
                    runtime.organism.borrow_mut().sync_repo_work_state(work);
                    // A rejected preview is useful to the source reducer but is
                    // not authoritative for sibling panes: it cannot survive a
                    // refresh because it reached neither disk nor retry queue.
                    if retained {
                        runtime.presence.sync_repo_work(&repo, day, work);
                    }
                }
                if matches!(kind, CommandKind::BuildOrTest | CommandKind::GitPush)
                    && runtime.last_repo_root.borrow().is_none()
                    && runtime.last_repo_cwd.borrow().is_none()
                {
                    // With neither a canonical repo nor a raw cwd there is no
                    // honest boundary for a durable loop. Keep the immediate
                    // reaction, but do not let it leak into another context.
                    runtime
                        .organism
                        .borrow_mut()
                        .sync_repo_work_state(RepoWorkState::default());
                }
                // The arbiter spends attention only for an expression it can
                // actually suppress. Durable behavior/status facts render
                // regardless, and a speechless completion consumes no focus.
                if reaction.speech.is_some()
                    && completion_attention
                        .is_some_and(|cue| !runtime.activity.offer_attention(cue, Instant::now()))
                {
                    reaction.speech = None;
                }
                let command_origin = runtime.command_origin_behavior.take();
                if kind == CommandKind::GitPush && event.exit_code == Some(0) {
                    runtime.transition_source_override.set(command_origin);
                }
                runtime.render(&reaction);
                if let Some(view) = view_weak.upgrade() {
                    runtime.refresh_surface(&view, Instant::now());
                    view.insert_inline_notice(&runtime.card);
                }
                OrganismRuntime::settle_later(
                    &runtime,
                    view_weak.clone(),
                    generation,
                    reaction_hold(&reaction),
                );
                // Dispatch last, after this pane has committed its own
                // reducer/memory/render work. Only the typed failure fact
                // crosses panes; the coordinator suppresses owner-local ends.
                if let Some(signal) = presence_signal_for_exit(event.exit_code) {
                    runtime.presence.signal_from(runtime.presence_token, signal);
                }
            });
        }

        {
            // Correlation loss arrives in several flavors: before any command
            // started, at CommandEnd right before an authoritative
            // CommandFinished in the same parser turn, after a verified
            // finish at the next prompt, or on the recovery path where no
            // finish will ever come. Only the last one is the organism's to
            // handle, so defer one main-loop turn and let an arriving finish
            // win — it carries the verified outcome and, because nothing is
            // cleared here, the correct agent attribution.
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            let shared_life = self.organism_life.clone();
            view.connect_agent_execution_lost(move |_generation, _reason| {
                if !runtime.command_running.get() {
                    // Nothing is running: either nothing ever started, or the
                    // finish was already consumed with correct attribution.
                    return;
                }
                if !runtime.agent_watching.get() {
                    // The running command is not agent-attributed (e.g. the
                    // human pressed Enter on an inserted reviewed command and
                    // the verification poll aborted afterwards): the agent's
                    // command never ran, and the human's live command must
                    // keep its slot and its outcome.
                    return;
                }
                let runtime = runtime.clone();
                let view_weak = view_weak.clone();
                let shared_life = shared_life.clone();
                let observed_generation = runtime.generation.get();
                gtk::glib::idle_add_local_once(move || {
                    // A finish (or a fresh start) in the meantime owns the
                    // outcome; only a still-unresolved running command falls
                    // to this restrained warning.
                    if runtime.generation.get() != observed_generation
                        || !runtime.command_running.get()
                    {
                        return;
                    }
                    if runtime.command_running.replace(false) {
                        runtime.activity.command_finished(Instant::now());
                    }
                    runtime.command_started_at.set(None);
                    runtime.agent_watching.set(false);
                    runtime.output_rhythm.borrow_mut().reset();
                    runtime.visible_watch_rhythm.set(WatchRhythm::Steady);
                    runtime.offered_watch_rhythm.set(WatchRhythm::Steady);
                    runtime.output_activity.set(false);
                    runtime.command_origin_behavior.set(None);
                    let generation = runtime.bump_generation();
                    let reaction = {
                        let mut organism = runtime.organism.borrow_mut();
                        organism.sync_state(shared_life.get());
                        let reaction = organism.agent_execution_lost();
                        shared_life.set(organism.state());
                        reaction
                    };
                    runtime.render(&reaction);
                    if let Some(view) = view_weak.upgrade() {
                        runtime.refresh_surface(&view, Instant::now());
                        view.insert_inline_notice(&runtime.card);
                    }
                    OrganismRuntime::settle_later(
                        &runtime,
                        view_weak,
                        generation,
                        reaction_hold(&reaction),
                    );
                });
            });
        }

        {
            // CommandEnd arrives before the finished GTK block is committed at
            // the next PromptStart. Re-pin only; the reducer already consumed
            // the authoritative lifecycle event above.
            let runtime = runtime.clone();
            let view_weak = Rc::downgrade(view);
            view.connect_block_finished(
                move |_command, _exit_code, _agent_generation, _duration_ms| {
                    if let Some(view) = view_weak.upgrade() {
                        view.insert_inline_notice(&runtime.card);
                    }
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_rhythm_uses_only_pulse_count_and_monotonic_quiet_time() {
        let start = Instant::now();
        let mut rhythm = OutputRhythmTracker::default();
        rhythm.start(start);
        rhythm.note_output(start);
        rhythm.note_output(start + Duration::from_millis(300));
        rhythm.note_output(start + Duration::from_millis(600));
        assert_eq!(
            rhythm.sample(start + Duration::from_millis(600), true),
            WatchRhythm::Busy
        );
        assert_eq!(
            rhythm.sample(start + Duration::from_secs(2), true),
            WatchRhythm::Steady
        );
        assert_eq!(
            rhythm.sample(start + Duration::from_millis(3_600), true),
            WatchRhythm::Waiting
        );

        rhythm.note_output(start + Duration::from_millis(3_700));
        assert_eq!(
            rhythm.sample(start + Duration::from_millis(4_000), true),
            WatchRhythm::Resumed
        );
        assert_eq!(
            rhythm.sample(start + Duration::from_millis(4_700), true),
            WatchRhythm::Steady
        );
        assert_eq!(
            rhythm.sample(start + Duration::from_secs(5), false),
            WatchRhythm::Steady
        );
        assert_eq!(rhythm.last_output, None);
        assert_eq!(rhythm.command_started, None);
    }

    #[test]
    fn output_rhythm_handles_silent_commands_and_a_true_sliding_window() {
        let start = Instant::now();
        let mut silent = OutputRhythmTracker::default();
        silent.start(start);
        assert_eq!(
            silent.sample(start + Duration::from_millis(2_999), true),
            WatchRhythm::Steady
        );
        assert_eq!(
            silent.sample(start + Duration::from_secs(3), true),
            WatchRhythm::Waiting
        );
        silent.note_output(start + Duration::from_millis(3_100));
        assert_eq!(
            silent.sample(start + Duration::from_millis(3_100), true),
            WatchRhythm::Resumed
        );

        let mut sliding = OutputRhythmTracker::default();
        sliding.start(start);
        sliding.note_output(start);
        sliding.note_output(start + Duration::from_millis(1_100));
        sliding.note_output(start + Duration::from_millis(1_210));
        assert_eq!(
            sliding.sample(start + Duration::from_millis(1_210), true),
            WatchRhythm::Steady
        );
        sliding.note_output(start + Duration::from_millis(1_220));
        assert_eq!(
            sliding.sample(start + Duration::from_millis(1_220), true),
            WatchRhythm::Busy,
            "the most recent three pulses, not a tumbling first-pulse window, define busy"
        );
    }

    #[test]
    fn presentation_boundary_consumes_rhythm_and_restarts_visible_quiet_time() {
        let start = Instant::now();
        let boundary = start + Duration::from_secs(4);
        let mut rhythm = OutputRhythmTracker::default();
        rhythm.start(start);
        rhythm.note_output(start);
        rhythm.note_output(start + Duration::from_millis(100));
        rhythm.note_output(start + Duration::from_millis(200));
        assert_eq!(
            rhythm.sample(start + Duration::from_millis(200), true),
            WatchRhythm::Busy
        );

        reset_output_rhythm_at_boundary(&mut rhythm, true, boundary);
        assert_eq!(rhythm.sample(boundary, true), WatchRhythm::Steady);
        assert_eq!(
            rhythm.sample(boundary + Duration::from_millis(2_999), true),
            WatchRhythm::Steady,
            "hidden busy/waiting edges must not replay after presentation resumes"
        );
        assert_eq!(
            rhythm.sample(boundary + Duration::from_secs(3), true),
            WatchRhythm::Waiting,
            "a new visible quiet interval may create a fresh edge"
        );
    }

    #[test]
    fn rejected_or_invisible_watch_rhythm_edges_are_not_reoffered() {
        let first = watch_rhythm_plan(WatchRhythm::Waiting, WatchRhythm::Busy, WatchRhythm::Steady);
        assert_eq!(
            first,
            WatchRhythmPlan {
                visible: WatchRhythm::Steady,
                offered: WatchRhythm::Waiting,
                attention_offer: Some(WatchRhythm::Waiting),
            },
            "a new edge first clears any older expression"
        );
        let dropped = watch_rhythm_plan(WatchRhythm::Waiting, WatchRhythm::Steady, first.offered);
        assert_eq!(dropped.attention_offer, None);
        assert_eq!(dropped.visible, WatchRhythm::Steady);

        let neutral = watch_rhythm_plan(WatchRhythm::Steady, dropped.visible, dropped.offered);
        assert_eq!(neutral.offered, WatchRhythm::Steady);
        assert_eq!(
            watch_rhythm_plan(WatchRhythm::Waiting, neutral.visible, neutral.offered)
                .attention_offer,
            Some(WatchRhythm::Waiting),
            "a genuinely new boundary may be offered later"
        );
    }

    #[test]
    fn watch_rhythm_attention_requires_a_mapped_owner_watching_surface() {
        assert!(watch_rhythm_context_presentable(
            true,
            OrganismMotion::Full,
            SurfaceMode::Watching,
            false,
        ));
        assert!(watch_rhythm_context_presentable(
            true,
            OrganismMotion::Calm,
            SurfaceMode::Watching,
            false,
        ));
        for (owner, motion, mode, alt_screen) in [
            (false, OrganismMotion::Full, SurfaceMode::Watching, false),
            (true, OrganismMotion::Full, SurfaceMode::Typing, false),
            (true, OrganismMotion::Full, SurfaceMode::Watching, true),
            (true, OrganismMotion::Static, SurfaceMode::Watching, false),
        ] {
            assert!(!watch_rhythm_context_presentable(
                owner, motion, mode, alt_screen,
            ));
        }
        assert!(watch_rhythm_surface_presentable(true, true));
        assert!(!watch_rhythm_surface_presentable(true, false));
        assert!(!watch_rhythm_surface_presentable(false, true));
    }

    #[test]
    fn territory_hash_is_bounded_and_home_nests_are_stable() {
        let prefix = "r".repeat(MAX_TERRITORY_HASH_BYTES);
        assert_eq!(
            stable_repo_hash(format!("{prefix}/one").as_bytes()),
            stable_repo_hash(format!("{prefix}/two").as_bytes()),
            "identity hashing must remain bounded"
        );

        let home = TerritoryHabit::for_repo("/work/anvil", 7);
        assert!(home.is_home());
        assert_eq!(home, TerritoryHabit::for_repo("/work/anvil", 99));
        assert_eq!(home.nest_x(8, 240), if home.nest_right { 240 } else { 8 });
        assert!(home.route_offset < 800);
        assert_eq!(home.route_frame(0), u64::from(home.route_offset));
        let known = TerritoryHabit::for_repo("/work/known", 2);
        assert_eq!(known.nest_x(8, 240), 8);
        let unfamiliar_at_arrival = TerritoryHabit::for_repo("/work/new", 0);
        assert!(unfamiliar_at_arrival.is_unfamiliar());
        assert!(should_begin_territory_intro(
            true,
            Some(unfamiliar_at_arrival),
            RepoVigil::None,
        ));
        let known_after_first_finish = TerritoryHabit::for_repo("/work/new", 1);
        assert!(!known_after_first_finish.is_unfamiliar());
        assert!(
            should_begin_territory_intro(true, Some(known_after_first_finish), RepoVigil::None),
            "arrival-time eligibility must survive the first event's Unfamiliar -> Known refresh"
        );
        assert!(!should_begin_territory_intro(
            true,
            Some(known_after_first_finish),
            RepoVigil::Failure,
        ));
        assert!(!should_begin_territory_intro(
            false,
            Some(known),
            RepoVigil::None,
        ));
        assert!(!should_begin_territory_intro(true, None, RepoVigil::None));
        let mut pending = true;
        assert!(!should_begin_territory_intro(
            std::mem::replace(&mut pending, false),
            Some(known_after_first_finish),
            RepoVigil::Failure,
        ));
        assert!(
            !pending,
            "a conflicting first-look is consumed, never queued"
        );
        assert!(!should_begin_territory_intro(
            std::mem::replace(&mut pending, false),
            Some(known_after_first_finish),
            RepoVigil::None,
        ));

        for vigil in [
            RepoVigil::Failure,
            RepoVigil::Stuck,
            RepoVigil::Recovery,
            RepoVigil::CautiousRecovery,
        ] {
            assert_eq!(
                territory_intro_after_repo_sync(true, Some(7_u8), vigil),
                (false, None),
                "{vigil:?} must cancel pending and already-active exploration"
            );
        }
        assert_eq!(
            territory_intro_after_repo_sync(true, Some(7_u8), RepoVigil::None),
            (true, Some(7)),
            "a clean cross-pane sync must not consume arrival eligibility"
        );
        assert_eq!(
            territory_intro_after_interruption(true, None::<u8>),
            (false, None),
            "typing/alt/focus loss consumes a pending first-look"
        );
        assert_eq!(
            territory_intro_after_interruption(false, Some(7_u8)),
            (false, None),
            "an already-visible first-look cannot resume after interruption"
        );
    }

    #[test]
    fn attention_classes_keep_failures_and_human_closures_above_chatter() {
        assert_eq!(
            completion_attention_cue(CommandKind::BuildOrTest, Some(1), 0, false),
            Some(AttentionCue::FailureVigil)
        );
        assert_eq!(
            completion_attention_cue(CommandKind::BuildOrTest, Some(0), 2, false),
            Some(AttentionCue::Recovery)
        );
        assert_eq!(
            completion_attention_cue(CommandKind::GitPush, Some(0), 0, false),
            Some(AttentionCue::Push)
        );
        assert_eq!(
            completion_attention_cue(CommandKind::BuildOrTest, Some(0), 0, true),
            None,
            "agent success must not spend the human greeting budget"
        );
        assert_eq!(
            completion_attention_cue(CommandKind::Other, Some(0), 0, false),
            Some(AttentionCue::Closure)
        );
        assert_eq!(
            completion_attention_cue(CommandKind::Other, None, 0, false),
            None
        );
        assert_eq!(
            remembered_insight_attention(true),
            Some(AttentionCue::Insight)
        );
        assert_eq!(remembered_insight_attention(false), None);
    }

    #[test]
    fn semantic_bridges_run_only_in_full_motion() {
        assert_eq!(
            visual_transition_for_motion(
                OrganismMotion::Full,
                Behavior::Celebrate,
                Behavior::GuardRecovery,
            ),
            VisualTransition::between(Behavior::Celebrate, Behavior::GuardRecovery)
        );
        for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
            assert_eq!(
                visual_transition_for_motion(motion, Behavior::Celebrate, Behavior::GuardRecovery,),
                None
            );
        }
    }

    #[test]
    fn memory_growth_maps_to_the_visual_phenotype_without_new_state() {
        assert_eq!(
            visual_growth_stage(GrowthStage::Juvenile),
            VisualGrowthStage::Juvenile
        );
        assert_eq!(
            visual_growth_stage(GrowthStage::Adult),
            VisualGrowthStage::Adult
        );
        assert_eq!(
            visual_growth_stage(GrowthStage::Seasoned),
            VisualGrowthStage::Seasoned
        );
    }

    #[test]
    fn compact_state_summary_reports_all_eight_bounded_dimensions() {
        let summary = state_summary(LifeState {
            energy: 0.0,
            mood: 0.1,
            curiosity: 0.2,
            boredom: 0.3,
            stress: 0.4,
            social_need: 0.5,
            attachment: 0.75,
            confidence: 1.0,
        });
        assert_eq!(summary, "E00 M10 C20 B30 S40 N50 A75 F100");
    }

    #[test]
    fn visible_state_uses_a_few_character_words_and_keeps_raw_detail_separate() {
        let state = LifeState {
            energy: 0.10,
            mood: 0.20,
            curiosity: 0.90,
            boredom: 0.95,
            stress: 0.80,
            social_need: 0.90,
            attachment: 0.90,
            confidence: 0.10,
        };
        assert_eq!(state_words(state), "sleepy · tense · subdued");
        assert!(state_summary(state).starts_with("E10 M20 C90"));
        assert_eq!(state_words(LifeState::default()), "steady");
    }

    #[test]
    fn every_inline_pose_fits_the_fixed_sprite_slot() {
        for behavior in [
            Behavior::Idle,
            Behavior::WatchCommand,
            Behavior::InspectError,
            Behavior::SitNearError,
            Behavior::Celebrate,
            Behavior::CelebrateBig,
            Behavior::RestAfterPush,
            Behavior::UnknownOutcome,
            Behavior::Sleep,
            Behavior::Explore,
            Behavior::Approach,
            Behavior::WatchAgent,
            Behavior::WatchSettled,
            Behavior::GuardFailure,
            Behavior::GuardStuck,
            Behavior::GuardRecovery,
            Behavior::GuardCautious,
        ] {
            let width = behavior
                .sprite()
                .lines()
                .map(str::chars)
                .map(Iterator::count)
                .max()
                .unwrap_or(0);
            assert!(
                width <= INLINE_SPRITE_SLOT_CHARS as usize,
                "{behavior:?} needs {width} columns"
            );
        }
    }

    #[test]
    fn flaky_hint_quiets_words_without_escalating_the_success_pose() {
        let mut human = Reaction {
            // A stale pane-local streak must not leak a big pose past the
            // freshly replayed repo-level flaky classification.
            behavior: Behavior::CelebrateBig,
            tone: Tone::Success,
            description: "build/test passed after 3 failure(s) · 20s".to_string(),
            speech: Some("好了。"),
        };
        mark_likely_flaky(&mut human, false);
        assert_eq!(human.behavior, Behavior::Celebrate);
        assert_eq!(human.tone, Tone::Quiet);
        assert_eq!(human.speech, Some("像是偶发的。"));
        assert!(human.description.contains("after 1 failure(s) · 20s"));
        assert!(!human.description.contains("after 3 failure(s)"));
        assert!(human.description.contains("looks intermittent"));

        let mut agent = human.clone();
        mark_likely_flaky(&mut agent, true);
        assert_eq!(agent.behavior, Behavior::Celebrate);
        assert_eq!(agent.tone, Tone::Quiet);
        assert_eq!(agent.speech, None);
    }

    #[test]
    fn ordered_replay_can_downgrade_stale_failure_and_recovery_reactions() {
        let mut failure = Reaction {
            behavior: Behavior::SitNearError,
            tone: Tone::Error,
            description: "exit 1 · first crack after 5 clean run(s)".to_string(),
            speech: None,
        };
        let first_failure = MemoryInsight {
            event_order_exact: true,
            open_failures: 1,
            current_work: RepoWorkState::new(1, false, 0),
            ..MemoryInsight::default()
        };
        normalize_replayed_event(
            &mut failure,
            CommandKind::BuildOrTest,
            Some(1),
            Some(1_250),
            &first_failure,
            false,
        );
        assert_eq!(failure.behavior, Behavior::InspectError);
        assert_eq!(failure.speech, Some("这里。"));
        assert!(failure.description.contains("build failure 1"));
        assert!(failure.description.contains("1.2s"));
        assert!(!failure.description.contains("first crack"));

        let mut late_success = Reaction {
            behavior: Behavior::CelebrateBig,
            tone: Tone::Success,
            description: "build/test passed after 3 failure(s) · 20s".to_string(),
            speech: Some("终于。"),
        };
        let still_open = MemoryInsight {
            event_order_exact: true,
            current_work: RepoWorkState::new(1, false, 0),
            ..MemoryInsight::default()
        };
        normalize_replayed_event(
            &mut late_success,
            CommandKind::BuildOrTest,
            Some(0),
            Some(20_000),
            &still_open,
            false,
        );
        normalize_replayed_closure(
            &mut late_success,
            CommandKind::BuildOrTest,
            Some(0),
            &still_open,
        );
        assert_eq!(late_success.behavior, Behavior::Celebrate);
        assert_eq!(late_success.speech, None);
        assert_eq!(
            late_success.description,
            "build/test passed · 20.0s · later repo failure remains open"
        );
        assert!(late_success
            .description
            .contains("later repo failure remains open"));
    }

    #[test]
    fn compacted_order_is_neutral_and_final_work_vetoes_closure_words() {
        let mut compacted = Reaction {
            behavior: Behavior::CelebrateBig,
            tone: Tone::Success,
            description: "build/test passed after 8 failure(s)".to_string(),
            speech: Some("终于。"),
        };
        let unknown_order = MemoryInsight {
            current_work: RepoWorkState::new(1, false, 4),
            ..MemoryInsight::default()
        };
        normalize_replayed_event(
            &mut compacted,
            CommandKind::BuildOrTest,
            Some(0),
            None,
            &unknown_order,
            false,
        );
        normalize_replayed_closure(
            &mut compacted,
            CommandKind::BuildOrTest,
            Some(0),
            &unknown_order,
        );
        assert_eq!(compacted.behavior, Behavior::Celebrate);
        assert_eq!(compacted.tone, Tone::Quiet);
        assert_eq!(compacted.speech, None);
        assert!(compacted
            .description
            .contains("older repo event order unavailable"));
        assert!(!compacted.description.contains("after 8"));

        let mut push = Reaction {
            behavior: Behavior::RestAfterPush,
            tone: Tone::Success,
            description: "git push completed".to_string(),
            speech: Some("收好了。"),
        };
        let newer_recovery = MemoryInsight {
            event_order_exact: true,
            push_after_recovery: true,
            current_work: RepoWorkState::new(0, true, 3),
            ..MemoryInsight::default()
        };
        normalize_replayed_closure(&mut push, CommandKind::GitPush, Some(0), &newer_recovery);
        normalize_replayed_closure(&mut push, CommandKind::GitPush, Some(0), &newer_recovery);
        assert_eq!(push.speech, None);
        assert_eq!(
            push.description
                .matches("newer recovered work still awaits push")
                .count(),
            1
        );

        let mut exact_push = Reaction {
            description: "git push completed".to_string(),
            speech: Some("收好了。"),
            ..push
        };
        let closed = MemoryInsight {
            event_order_exact: true,
            push_after_recovery: true,
            current_work: RepoWorkState::default(),
            ..MemoryInsight::default()
        };
        normalize_replayed_closure(&mut exact_push, CommandKind::GitPush, Some(0), &closed);
        assert_eq!(exact_push.speech, Some("收好了。"));
    }

    #[test]
    fn final_ordered_work_owns_every_failure_and_push_closure_branch() {
        let base_failure = Reaction {
            behavior: Behavior::InspectError,
            tone: Tone::Error,
            description: "exit 1 · build failure 1".to_string(),
            speech: Some("这里。"),
        };
        let mut later_recovery = base_failure.clone();
        normalize_replayed_closure(
            &mut later_recovery,
            CommandKind::BuildOrTest,
            Some(1),
            &MemoryInsight {
                current_work: RepoWorkState::new(0, true, 1),
                ..MemoryInsight::default()
            },
        );
        assert!(later_recovery
            .description
            .contains("ordered replay already contains a later recovery"));

        let mut later_push = base_failure;
        normalize_replayed_closure(
            &mut later_push,
            CommandKind::BuildOrTest,
            Some(1),
            &MemoryInsight::default(),
        );
        assert!(later_push
            .description
            .contains("ordered repo history already contains a later closure"));

        let base_push = Reaction {
            behavior: Behavior::RestAfterPush,
            tone: Tone::Success,
            description: "git push completed".to_string(),
            speech: Some("收好了。"),
        };
        let mut later_failure = base_push.clone();
        normalize_replayed_closure(
            &mut later_failure,
            CommandKind::GitPush,
            Some(0),
            &MemoryInsight {
                event_order_exact: true,
                push_after_recovery: true,
                current_work: RepoWorkState::new(2, false, 1),
                ..MemoryInsight::default()
            },
        );
        assert_eq!(later_failure.speech, None);
        assert!(later_failure
            .description
            .contains("later repo failure remains open"));

        for insight in [
            MemoryInsight {
                event_order_exact: true,
                ..MemoryInsight::default()
            },
            MemoryInsight::default(),
        ] {
            let mut ordinary_or_inexact = base_push.clone();
            normalize_replayed_closure(
                &mut ordinary_or_inexact,
                CommandKind::GitPush,
                Some(0),
                &insight,
            );
            assert_eq!(ordinary_or_inexact.speech, None);
        }

        // Voice decorators intentionally run before this final veto in the
        // lifecycle callback. Even a freshly produced flaky/growth line may
        // not claim closure when ordered replay ends with another failure.
        let still_open = MemoryInsight {
            event_order_exact: true,
            recovered_failures: 1,
            likely_flaky: true,
            current_work: RepoWorkState::new(1, false, 3),
            ..MemoryInsight::default()
        };
        let mut decorated = Reaction {
            behavior: Behavior::CelebrateBig,
            tone: Tone::Success,
            description: String::new(),
            speech: None,
        };
        normalize_replayed_event(
            &mut decorated,
            CommandKind::BuildOrTest,
            Some(0),
            None,
            &still_open,
            false,
        );
        mark_likely_flaky(&mut decorated, false);
        apply_growth_voice(&mut decorated, GrowthStage::Seasoned, 1, false);
        assert!(decorated.speech.is_some());
        normalize_replayed_closure(
            &mut decorated,
            CommandKind::BuildOrTest,
            Some(0),
            &still_open,
        );
        assert_eq!(decorated.speech, None);
    }

    #[test]
    fn seasoned_recovery_gets_terser_without_changing_its_strength() {
        let original = Reaction {
            behavior: Behavior::CelebrateBig,
            tone: Tone::Success,
            description: "build/test passed · repo recovery after 3 failure(s)".to_string(),
            speech: Some("终于。"),
        };
        let mut seasoned = original.clone();
        apply_growth_voice(&mut seasoned, GrowthStage::Seasoned, 3, false);
        assert_eq!(seasoned.behavior, original.behavior);
        assert_eq!(seasoned.tone, original.tone);
        assert_eq!(seasoned.description, original.description);
        assert_eq!(seasoned.speech, Some("嗯。"));

        let mut adult = original.clone();
        apply_growth_voice(&mut adult, GrowthStage::Adult, 3, false);
        assert_eq!(adult.speech, original.speech);

        let mut agent = original;
        apply_growth_voice(&mut agent, GrowthStage::Seasoned, 3, true);
        assert_eq!(agent.speech, Some("终于。"));
    }

    #[test]
    fn growth_badges_name_every_stage_without_exceeding_the_slot() {
        for (stage, name) in [
            (GrowthStage::Juvenile, "juvenile"),
            (GrowthStage::Adult, "adult"),
            (GrowthStage::Seasoned, "seasoned"),
        ] {
            let badge = growth_badge(stage, MemoryBadgeState::Persistent);
            assert!(badge.starts_with(name));
            assert!(badge.chars().count() <= 32);
        }
        assert_eq!(
            growth_badge(GrowthStage::Seasoned, MemoryBadgeState::Volatile),
            "volatile · no LLM"
        );
        assert_eq!(
            growth_badge(GrowthStage::Seasoned, MemoryBadgeState::SaveFailed),
            "seasoned · save failed · no LLM"
        );
    }

    #[test]
    fn unusual_build_pace_uses_the_prior_baseline_and_absolute_guard() {
        assert_eq!(
            unusual_build_pace(Some(60_000), Some(120_000)),
            Some("slower than usual here")
        );
        assert_eq!(
            unusual_build_pace(Some(120_000), Some(60_000)),
            Some("quicker than usual here")
        );
        // A twofold change measured in milliseconds is noise, not insight.
        assert_eq!(unusual_build_pace(Some(5_000), Some(10_000)), None);
        assert_eq!(unusual_build_pace(None, Some(120_000)), None);
        assert_eq!(unusual_build_pace(Some(60_000), None), None);
    }

    #[test]
    fn live_surface_fails_closed_when_the_complete_body_does_not_fit() {
        let tiny_width = SurfaceBox {
            width: 99,
            height: 80,
            right_gutter: 20,
            cell_width: 8,
            cell_height: 16,
            body_width: 64,
            body_height: 48,
            cursor_row: 8,
        };
        let tiny_height = SurfaceBox {
            width: 300,
            height: 63,
            ..tiny_width
        };
        assert_eq!(
            surface_point(
                tiny_width,
                SurfaceMode::Idle,
                WanderTempo::Calm,
                AmbientBehavior::Idle,
                0
            ),
            None
        );
        assert_eq!(
            surface_point(
                tiny_height,
                SurfaceMode::Watching,
                WanderTempo::Calm,
                AmbientBehavior::Idle,
                0
            ),
            None
        );

        let unaligned_near_boundary = SurfaceBox {
            width: 64 + 20 + 16,
            height: 48 + 16 + 16,
            right_gutter: 20,
            cell_width: 7,
            cell_height: 16,
            body_width: 64,
            body_height: 48,
            cursor_row: 8,
        };
        assert_eq!(
            surface_point(
                unaligned_near_boundary,
                SurfaceMode::Typing,
                WanderTempo::Calm,
                AmbientBehavior::Idle,
                0
            ),
            None
        );
    }

    #[test]
    fn geometry_signature_invalidates_stale_positions_when_pose_bounds_change() {
        let surface = SurfaceBox {
            width: 320,
            height: 180,
            right_gutter: 20,
            cell_width: 8,
            cell_height: 16,
            body_width: 56,
            body_height: 48,
            cursor_row: 4,
        };
        let original = surface_signature(surface);
        let old = surface_point(
            surface,
            SurfaceMode::Typing,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            0,
        )
        .unwrap();
        let wider = SurfaceBox {
            body_width: 96,
            ..surface
        };
        let safe_right = wider.width - wider.right_gutter - SURFACE_MARGIN.max(wider.cell_width);
        assert!(old.x + f64::from(wider.body_width) > f64::from(safe_right));
        assert_ne!(original, surface_signature(wider));
        let fresh = surface_point(
            wider,
            SurfaceMode::Typing,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            0,
        )
        .unwrap();
        assert!(fresh.x + f64::from(wider.body_width) <= f64::from(safe_right));
        assert_ne!(
            original,
            surface_signature(SurfaceBox {
                right_gutter: 28,
                ..surface
            })
        );
        // Cursor motion is an intended walk, not a stale-geometry snap.
        assert_eq!(
            original,
            surface_signature(SurfaceBox {
                cursor_row: 12,
                ..surface
            })
        );
    }

    #[test]
    fn every_surface_pose_is_clamped_left_of_the_scrollbar_gutter() {
        let surface = SurfaceBox {
            width: 320,
            height: 180,
            right_gutter: 20,
            cell_width: 8,
            cell_height: 16,
            body_width: 88,
            body_height: 48,
            cursor_row: 1,
        };
        for mode in [
            SurfaceMode::Idle,
            SurfaceMode::Typing,
            SurfaceMode::Watching,
            SurfaceMode::Reacting,
        ] {
            for frame in [0, 1, 359, 360, 399, 400, 759, 799, u64::MAX] {
                let point = surface_point(
                    surface,
                    mode,
                    WanderTempo::Calm,
                    AmbientBehavior::Idle,
                    frame,
                )
                .expect("body fits");
                assert!(point.x >= f64::from(SURFACE_MARGIN));
                assert!(point.y >= f64::from(SURFACE_MARGIN));
                assert_eq!(point.x as i32 % surface.cell_width, 0);
                assert_eq!(point.y as i32 % surface.cell_height, 0);
                assert!(
                    point.x + f64::from(surface.body_width)
                        <= f64::from(surface.width - surface.right_gutter - SURFACE_MARGIN)
                );
                assert!(
                    point.y + f64::from(surface.body_height)
                        <= f64::from(surface.height - SURFACE_MARGIN)
                );
            }
        }
    }

    #[test]
    fn accepted_typing_owns_a_brief_live_body_hide_window() {
        assert_eq!(
            surface_mode(
                Behavior::Idle,
                false,
                Some(HUMAN_INPUT_RETREAT - Duration::from_millis(1))
            ),
            SurfaceMode::Typing
        );
        assert!(suppress_live_body_for_focus(SurfaceMode::Typing));
        assert!(!suppress_live_body_for_focus(SurfaceMode::Idle));
        assert_eq!(
            surface_mode(Behavior::Idle, false, Some(HUMAN_INPUT_RETREAT)),
            SurfaceMode::Idle
        );
        assert_eq!(
            surface_mode(Behavior::WatchCommand, true, None),
            SurfaceMode::Watching
        );
        assert_eq!(
            surface_mode(Behavior::Celebrate, false, None),
            SurfaceMode::Reacting
        );
        assert_eq!(
            surface_mode(Behavior::GuardRecovery, false, None),
            SurfaceMode::Idle,
            "the durable guard is an ambient intention, not an endless reaction"
        );
        for vigil in [
            Behavior::GuardFailure,
            Behavior::GuardStuck,
            Behavior::GuardCautious,
        ] {
            assert_eq!(surface_mode(vigil, false, None), SurfaceMode::Idle);
        }
    }

    #[test]
    fn correction_accept_pulse_is_pane_scoped_single_use_and_expires() {
        let life = Rc::new(Cell::new(LifeState::default()));
        let signal = OrganismCorrectionSignal::new(life);
        let now = Instant::now();

        signal.note_accepted(7);
        assert!(!signal.take_recent_accept(9, now));
        assert!(signal.take_recent_accept(7, now));
        assert!(!signal.take_recent_accept(7, now));

        signal.note_accepted(7);
        assert!(!signal.take_recent_accept(7, now + CORRECTION_ASSIST_WINDOW * 2));

        signal.note_accepted(3);
        signal.revoke_accept(4);
        assert!(signal.take_recent_accept(3, Instant::now()));
        signal.note_accepted(3);
        signal.revoke_accept(3);
        assert!(!signal.take_recent_accept(3, Instant::now()));
    }

    #[test]
    fn agent_signal_deduplicates_repeated_phase_renders() {
        let life = Rc::new(Cell::new(LifeState::default()));
        let signal = OrganismAgentSignal::new(life.clone());
        let before = life.get().social_need;
        signal.note_phase(AgentPulse::Working);
        let after_first = life.get().social_need;
        assert!(after_first > before);
        signal.note_phase(AgentPulse::Working);
        assert_eq!(life.get().social_need, after_first);
        signal.note_phase(AgentPulse::Gone);
        assert!(life.get().social_need > after_first);
    }

    #[test]
    fn dismissal_streak_resets_on_acceptance() {
        let life = Rc::new(Cell::new(LifeState::default()));
        let signal = OrganismCorrectionSignal::new(life);
        signal.note_dismissed();
        signal.note_dismissed();
        assert_eq!(signal.dismiss_streak.get(), 2);
        signal.note_accepted(1);
        assert_eq!(signal.dismiss_streak.get(), 0);
    }

    #[test]
    fn shared_activity_clock_hands_out_each_slice_exactly_once() {
        let activity = OrganismActivity::new(None, GrowthProgress::default());
        let start = Instant::now();
        assert_eq!(activity.tick_slice(start), 0.0);
        let later = start + Duration::from_millis(250);
        let slice = activity.tick_slice(later);
        assert!((slice - 0.25).abs() < 0.005);
        // A second body asking at the same instant gets nothing: the mind
        // never lives the same moment twice.
        assert_eq!(activity.tick_slice(later), 0.0);
    }

    #[test]
    fn presence_tokens_are_monotonic_and_have_at_most_one_owner() {
        let mut ledger = PresenceLedger::default();
        let first = ledger.reserve();
        let second = ledger.reserve();
        assert_ne!(first, second);
        ledger.bind(first);
        ledger.bind(second);

        ledger.claim(Some(first));
        assert!(ledger.is_owner(first));
        assert!(!ledger.is_owner(second));
        ledger.claim(Some(second));
        assert!(!ledger.is_owner(first));
        assert!(ledger.is_owner(second));

        ledger.unregister(second);
        assert!(!ledger.is_owner(second));
        let third = ledger.reserve();
        assert_ne!(third, second);
        ledger.claim(Some(third));
        assert!(
            !ledger.is_owner(third),
            "an unbound token cannot own presence"
        );
    }

    #[test]
    fn presence_signals_route_only_from_a_registered_background_pane() {
        let mut ledger = PresenceLedger::default();
        let owner = ledger.reserve();
        let background = ledger.reserve();
        let unbound = ledger.reserve();
        ledger.bind(owner);
        ledger.bind(background);

        assert_eq!(ledger.signal_target(background), None);
        ledger.claim(Some(owner));
        assert_eq!(ledger.signal_target(background), Some(owner));
        assert_eq!(
            ledger.signal_target(owner),
            None,
            "owner-local failure stays local"
        );
        assert_eq!(ledger.signal_target(unbound), None);

        // Notebook page switching revokes synchronously before its idle can
        // resolve the new page. Nothing routes through the old owner in that
        // interval; claiming the new token restores routing afterwards.
        ledger.claim(None);
        assert_eq!(ledger.signal_target(background), None);
        assert!(!ledger.is_owner(owner));
        ledger.claim(Some(background));
        assert_eq!(ledger.signal_target(owner), Some(background));

        ledger.unregister(background);
        assert_eq!(ledger.signal_target(owner), None);
    }

    #[test]
    fn only_nonzero_authoritative_status_becomes_a_content_free_signal() {
        assert_eq!(presence_signal_for_exit(Some(0)), None);
        assert_eq!(presence_signal_for_exit(None), None);
        assert_eq!(
            presence_signal_for_exit(Some(1)),
            Some(PresenceSignal::BackgroundCommandFailed)
        );
        assert_eq!(
            presence_signal_for_exit(Some(137)),
            Some(PresenceSignal::BackgroundCommandFailed)
        );
    }

    #[test]
    fn glance_aside_is_live_only_and_never_overrides_owner_work() {
        let cue = Some(PresenceCue::GlanceAside);
        assert!(can_show_presence_cue(
            true,
            OrganismMotion::Full,
            false,
            true,
            SurfaceMode::Idle,
            false,
        ));
        for (owner, motion, alt_screen, visible, mode) in [
            (false, OrganismMotion::Full, false, true, SurfaceMode::Idle),
            (true, OrganismMotion::Static, false, true, SurfaceMode::Idle),
            (true, OrganismMotion::Full, true, true, SurfaceMode::Idle),
            (true, OrganismMotion::Full, false, false, SurfaceMode::Idle),
            (true, OrganismMotion::Full, false, true, SurfaceMode::Typing),
            (
                true,
                OrganismMotion::Full,
                false,
                true,
                SurfaceMode::Watching,
            ),
            (
                true,
                OrganismMotion::Full,
                false,
                true,
                SurfaceMode::Reacting,
            ),
        ] {
            assert!(!can_show_presence_cue(
                owner, motion, alt_screen, visible, mode, false,
            ));
        }
        assert!(!can_show_presence_cue(
            true,
            OrganismMotion::Full,
            false,
            true,
            SurfaceMode::Idle,
            true,
        ));

        assert_eq!(
            live_display_behavior(Behavior::Sleep, SurfaceMode::Idle, cue),
            Behavior::GlanceAside
        );
        assert_eq!(
            live_display_behavior(Behavior::WatchCommand, SurfaceMode::Watching, cue),
            Behavior::WatchCommand
        );
        assert_eq!(
            live_display_behavior(Behavior::InspectError, SurfaceMode::Reacting, cue),
            Behavior::InspectError
        );
        assert_eq!(
            live_display_behavior(Behavior::Idle, SurfaceMode::Idle, None),
            Behavior::Idle
        );
    }

    #[test]
    fn reactions_and_presence_cues_start_on_their_canonical_frame() {
        let global = 57;
        let (baseline, live) = animation_frames(global, global, 0, SurfaceMode::Reacting, None);
        assert_eq!((baseline, live), (0, 0));
        assert_eq!(
            sprite_frame(Behavior::Celebrate, BodyLanguage::default(), false, live),
            Behavior::Celebrate.sprite()
        );
        assert_ne!(
            sprite_frame(Behavior::Celebrate, BodyLanguage::default(), false, global,),
            Behavior::Celebrate.sprite(),
            "the regression setup must begin on the alternate global frame"
        );

        let (baseline, live) = animation_frames(
            global,
            0,
            global,
            SurfaceMode::Idle,
            Some(PresenceCue::GlanceAside),
        );
        assert_eq!((baseline, live), (global, 0));
        // Event epochs remain correct across the global wrapping counter.
        assert_eq!(
            animation_frames(1, u64::MAX, 0, SurfaceMode::Watching, None),
            (2, 2)
        );
    }

    #[test]
    fn shared_sleep_rest_is_independent_of_the_timer_claiming_pane() {
        let activity = OrganismActivity::new(None, GrowthProgress::default());
        activity.body_started_sleeping();
        activity.body_started_sleeping();
        assert!(activity.sleeping_rest());

        // One awake body cannot erase another body's sleep, while any
        // running command gates regeneration for the whole window.
        activity.body_stopped_sleeping();
        assert!(activity.sleeping_rest());
        activity.command_started(Instant::now());
        assert!(!activity.sleeping_rest());
        activity.command_finished(Instant::now());
        assert!(activity.sleeping_rest());

        activity.body_stopped_sleeping();
        assert!(!activity.sleeping_rest());
    }

    #[test]
    fn ordinary_sleep_regeneration_requires_the_visible_presence() {
        let sleeping = |owner, motion, alt_screen| {
            visible_sleeping(
                owner,
                motion,
                alt_screen,
                true,
                false,
                SurfaceMode::Idle,
                AmbientBehavior::Sleep,
            )
        };
        assert!(sleeping(true, OrganismMotion::Full, false));
        assert!(!sleeping(false, OrganismMotion::Full, false));
        assert!(!sleeping(true, OrganismMotion::Static, false));
        assert!(!sleeping(true, OrganismMotion::Full, true));
        assert!(!visible_sleeping(
            true,
            OrganismMotion::Full,
            false,
            true,
            false,
            SurfaceMode::Typing,
            AmbientBehavior::Sleep,
        ));
        assert!(!visible_sleeping(
            true,
            OrganismMotion::Full,
            false,
            false,
            false,
            SurfaceMode::Idle,
            AmbientBehavior::Sleep,
        ));
        assert!(!visible_sleeping(
            true,
            OrganismMotion::Full,
            false,
            true,
            true,
            SurfaceMode::Idle,
            AmbientBehavior::Sleep,
        ));
    }

    #[test]
    fn durable_vigil_rest_is_owner_scoped_and_geometry_independent() {
        for vigil in [
            RepoVigil::Failure,
            RepoVigil::Stuck,
            RepoVigil::Recovery,
            RepoVigil::CautiousRecovery,
        ] {
            assert!(repo_vigil_sleep_claim(
                true,
                SurfaceMode::Idle,
                AmbientBehavior::Sleep,
                vigil,
            ));
        }
        assert!(!repo_vigil_sleep_claim(
            false,
            SurfaceMode::Idle,
            AmbientBehavior::Sleep,
            RepoVigil::Failure,
        ));
        assert!(!repo_vigil_sleep_claim(
            true,
            SurfaceMode::Typing,
            AmbientBehavior::Sleep,
            RepoVigil::Failure,
        ));
        assert!(!repo_vigil_sleep_claim(
            true,
            SurfaceMode::Idle,
            AmbientBehavior::GuardFailure,
            RepoVigil::Failure,
        ));
        assert!(!repo_vigil_sleep_claim(
            true,
            SurfaceMode::Idle,
            AmbientBehavior::Sleep,
            RepoVigil::None,
        ));

        // The non-frame reconciler uses this same pure snapshot: acquiring a
        // hidden vigil establishes rest immediately, and clearing it revokes
        // the claim without waiting for geometry or a heartbeat.
        assert!(sleeping_claim(
            true,
            OrganismMotion::Static,
            true,
            false,
            false,
            SurfaceMode::Idle,
            AmbientBehavior::Sleep,
            RepoVigil::Failure,
        ));
        assert!(!sleeping_claim(
            true,
            OrganismMotion::Static,
            true,
            false,
            false,
            SurfaceMode::Idle,
            AmbientBehavior::Idle,
            RepoVigil::None,
        ));

        // Static/fail-closed geometry makes ordinary visible sleep false, but
        // the logical owner claim still closes the 0.15→0.25 wake hysteresis.
        assert!(!visible_sleeping(
            true,
            OrganismMotion::Static,
            true,
            false,
            false,
            SurfaceMode::Idle,
            AmbientBehavior::Sleep,
        ));
        let mut state = LifeState {
            energy: 0.10,
            ..LifeState::default()
        };
        let mut mind = AmbientMind::default();
        assert_eq!(
            mind.step(state, 120.0, 0.0, RepoVigil::Failure),
            AmbientBehavior::Sleep
        );
        for _ in 0..120 {
            state.tick(1.0, false, true, CircadianPhase::Unlearned);
        }
        assert!(state.energy >= 0.25);
        assert_eq!(
            mind.step(state, 120.0, 1.0, RepoVigil::Failure),
            AmbientBehavior::GuardFailure
        );
    }

    #[test]
    fn morning_greeting_is_human_owned_once_per_work_session() {
        let daytime = CircadianProfile::from_mask(0b0001_1100); // buckets 2, 3, 4
        let activity = OrganismActivity::new(Some(daytime), GrowthProgress::default());
        assert!(activity.circadian_profile_needs_refresh(10));
        activity.set_circadian_profile(Some(daytime), CircadianRefresh::Succeeded(10));
        assert!(!activity.circadian_profile_needs_refresh(10));
        assert!(activity.circadian_profile_needs_refresh(11));
        assert_eq!(activity.circadian_phase(3), CircadianPhase::InHours);
        assert_eq!(activity.circadian_phase(6), CircadianPhase::OffHours);

        let outside = LocalCircadianTime { day: 10, bucket: 6 };
        assert!(!activity.take_morning_greeting(outside, true));
        let inside = LocalCircadianTime { day: 10, bucket: 2 };
        assert!(!activity.take_morning_greeting(inside, false));
        assert!(activity.take_morning_greeting(inside, true));
        assert!(!activity.take_morning_greeting(inside, true));
        assert!(activity.take_morning_greeting(LocalCircadianTime { day: 11, bucket: 3 }, true));

        activity.set_circadian_profile(None, CircadianRefresh::Succeeded(11));
        assert_eq!(activity.circadian_phase(3), CircadianPhase::Unlearned);
        assert!(!activity.take_morning_greeting(LocalCircadianTime { day: 12, bucket: 3 }, true));
    }

    #[test]
    fn failed_circadian_refresh_is_retried() {
        let daytime = CircadianProfile::from_mask(0b0001_1100);
        let activity = OrganismActivity::new(Some(daytime), GrowthProgress::default());

        // Updating from the still-usable cache must not acknowledge the day
        // when the disk refresh that preceded it failed.
        activity.set_circadian_profile(Some(daytime), CircadianRefresh::Failed);
        assert!(activity.circadian_profile_needs_refresh(10));

        activity.set_circadian_profile(Some(daytime), CircadianRefresh::Succeeded(10));
        assert!(!activity.circadian_profile_needs_refresh(10));
        activity.set_circadian_profile(Some(daytime), CircadianRefresh::Failed);
        assert!(activity.circadian_profile_needs_refresh(10));

        // A same-day command that deliberately skipped I/O keeps the last
        // successful refresh marker intact.
        activity.set_circadian_profile(Some(daytime), CircadianRefresh::Succeeded(10));
        activity.set_circadian_profile(Some(daytime), CircadianRefresh::NotAttempted);
        assert!(!activity.circadian_profile_needs_refresh(10));
    }

    #[test]
    fn wrapped_night_shift_does_not_greet_twice_across_midnight() {
        let night = CircadianProfile::from_mask(0b1000_0011); // buckets 7, 0, 1
        let activity = OrganismActivity::new(Some(night), GrowthProgress::default());
        assert!(activity.take_morning_greeting(LocalCircadianTime { day: 20, bucket: 7 }, true));
        assert!(!activity.take_morning_greeting(LocalCircadianTime { day: 21, bucket: 0 }, true));
        assert!(!activity.take_morning_greeting(LocalCircadianTime { day: 21, bucket: 1 }, true));
        assert!(activity.take_morning_greeting(LocalCircadianTime { day: 21, bucket: 7 }, true));
    }

    #[test]
    fn circadian_words_preserve_a_more_specific_repo_greeting() {
        let mut reaction = Reaction {
            behavior: Behavior::WatchCommand,
            tone: Tone::Active,
            description: "watching real build/test event · well-known repo".to_string(),
            speech: Some("回来了。"),
        };
        mark_circadian_greeting(&mut reaction, 2);
        assert_eq!(reaction.behavior, Behavior::WatchCommand);
        assert_eq!(reaction.tone, Tone::Active);
        assert_eq!(reaction.speech, Some("回来了。"));
        assert!(reaction.description.contains("habitual working hours"));

        reaction.speech = None;
        mark_circadian_greeting(&mut reaction, 7);
        assert_eq!(reaction.speech, Some("来了。"));
    }

    #[test]
    fn rest_needs_a_long_quiet_stretch_with_no_running_command() {
        let activity = OrganismActivity::new(None, GrowthProgress::default());
        let now = Instant::now();
        assert!(!activity.resting(now));
        let quiet = now + REST_ONSET + Duration::from_secs(1);
        assert!(activity.resting(quiet));

        activity.command_started(quiet);
        assert!(!activity.resting(quiet + REST_ONSET * 2));
        activity.command_finished(quiet + REST_ONSET * 2);
        assert!(!activity.resting(quiet + REST_ONSET * 2 + Duration::from_secs(1)));
        assert!(activity.resting(quiet + REST_ONSET * 3 + Duration::from_secs(1)));

        // A pane dying mid-command can never wedge the counter below zero.
        activity.command_finished(quiet);
        activity.command_finished(quiet);
        assert_eq!(activity.commands_running.get(), 0);
    }

    #[test]
    fn typing_window_marks_the_user_active_briefly() {
        let activity = OrganismActivity::new(None, GrowthProgress::default());
        let now = Instant::now();
        assert!(!activity.user_active(now));
        activity.note_input(now);
        assert!(activity.user_active(now + Duration::from_millis(500)));
        assert!(!activity.user_active(now + Duration::from_millis(1000)));
        activity.note_output(now + Duration::from_secs(5));
        assert!(!activity.resting(now + REST_ONSET));
    }

    #[test]
    fn volatile_commands_inside_the_known_checkout_keep_their_context() {
        assert!(cwd_within("/", "/"));
        assert!(cwd_within("/repo/sub", "/"));
        assert!(cwd_within("/repo", "/repo"));
        assert!(cwd_within("/repo/sub/dir", "/repo"));
        assert!(!cwd_within("/repository", "/repo"));
        assert!(!cwd_within("/tmp", "/repo"));

        assert!(same_checkout(Some("/repo"), None, Some("/repo/sub")));
        assert!(same_checkout(
            None,
            Some("/home/u/link"),
            Some("/home/u/link")
        ));
        assert!(!same_checkout(
            None,
            Some("/work/non-git-a"),
            Some("/work/non-git-b")
        ));
        assert!(!same_checkout(
            Some("/repo"),
            Some("/home/u/link"),
            Some("/tmp")
        ));
        assert!(!same_checkout(None, None, Some("/tmp")));
        assert!(!same_checkout(Some("/repo"), None, None));
    }

    #[test]
    fn work_state_broadcasts_are_scoped_to_exact_repo_and_local_day() {
        assert!(repo_work_scope_matches(
            "/repo/a",
            20,
            Some("/repo/a"),
            Some(20),
            Some("/repo/a"),
            Some("/repo/a/sub")
        ));
        assert!(!repo_work_scope_matches(
            "/repo/a",
            20,
            Some("/repo/b"),
            Some(20),
            Some("/repo/b"),
            Some("/repo/b")
        ));
        assert!(!repo_work_scope_matches(
            "/repo/a",
            20,
            Some("/repo/a"),
            Some(19),
            Some("/repo/a"),
            Some("/repo/a")
        ));
        assert!(!repo_work_scope_matches(
            "/repo/a",
            20,
            Some("/repo/a"),
            Some(20),
            Some("/repo/a"),
            Some("/tmp")
        ));
        assert!(!repo_work_scope_matches(
            "/repo/a",
            20,
            None,
            Some(20),
            None,
            Some("/repo/a")
        ));
    }

    #[test]
    fn checkout_identity_requires_the_raw_cwd_not_its_bounded_display_copy() {
        let root = format!("/repo/{}", "r".repeat(5_000));
        let raw_cwd = format!("{root}/nested");
        let display_cwd = crate::text_safety::bounded_display_text(&raw_cwd, 4 * 1_024, false);

        assert_ne!(display_cwd, raw_cwd);
        assert!(!same_checkout(Some(&root), Some(&root), Some(&display_cwd)));
        assert!(same_checkout(Some(&root), Some(&root), Some(&raw_cwd)));
    }

    #[test]
    fn ambient_dispositions_take_their_own_poses() {
        let surface = SurfaceBox {
            width: 360,
            height: 200,
            right_gutter: 20,
            cell_width: 8,
            cell_height: 16,
            body_width: 88,
            body_height: 48,
            cursor_row: 8,
        };
        let pose = |ambient, frame| {
            surface_point(
                surface,
                SurfaceMode::Idle,
                WanderTempo::Calm,
                ambient,
                frame,
            )
            .unwrap()
        };
        let idle_home = pose(AmbientBehavior::Idle, 0);
        let idle_far = pose(AmbientBehavior::Idle, 400);
        // Sleep curls at the bottom-left corner regardless of the frame.
        assert_eq!(pose(AmbientBehavior::Sleep, 0), idle_home);
        assert_eq!(pose(AmbientBehavior::Sleep, 400), idle_home);
        assert!(idle_far.x > idle_home.x);
        // Approach sits at the prompt-side edge, on the bottom row.
        let approach = pose(AmbientBehavior::Approach, 0);
        assert!(approach.x > idle_home.x);
        assert_eq!(approach.y, idle_home.y);
        assert_eq!(
            pose(AmbientBehavior::GuardRecovery, 0),
            approach,
            "recovered work is guarded beside the prompt"
        );
        assert_eq!(pose(AmbientBehavior::GuardRecovery, 400), approach);
        assert_eq!(pose(AmbientBehavior::GuardCautious, 0), approach);
        assert_eq!(pose(AmbientBehavior::GuardCautious, 400), approach);
        // Explore walks at frames where a calm idle would still be sitting.
        let explore = pose(AmbientBehavior::Explore, 300);
        let calm_sit = pose(AmbientBehavior::Idle, 300);
        assert!(explore.x > calm_sit.x);
    }

    #[test]
    fn a_long_watch_settles_and_the_elapsed_label_reads_naturally() {
        assert_eq!(watching_behavior(false, None), Behavior::WatchCommand);
        assert_eq!(
            watching_behavior(false, Some(Duration::from_secs(59))),
            Behavior::WatchCommand
        );
        assert_eq!(
            watching_behavior(false, Some(Duration::from_secs(60))),
            Behavior::WatchSettled
        );
        // The Agent's crouch outranks settling in.
        assert_eq!(
            watching_behavior(true, Some(Duration::from_secs(600))),
            Behavior::WatchAgent
        );

        assert_eq!(elapsed_label(Duration::from_secs(42)), "40s");
        assert_eq!(elapsed_label(Duration::from_secs(150)), "2m 30s");
        assert_eq!(elapsed_label(Duration::from_secs(155)), "2m 30s");
        assert_eq!(elapsed_label(Duration::from_secs(3_905)), "1h 05m");
    }

    #[test]
    fn reaction_holds_follow_semantic_weight_instead_of_one_fixed_timeout() {
        let reaction = |behavior, tone| Reaction {
            behavior,
            tone,
            description: String::new(),
            speech: None,
        };
        let quiet_pass = reaction(Behavior::Celebrate, Tone::Quiet);
        let ordinary_pass = reaction(Behavior::Celebrate, Tone::Success);
        let first_error = reaction(Behavior::InspectError, Tone::Error);
        let repeated_error = reaction(Behavior::SitNearError, Tone::Error);
        let big_recovery = reaction(Behavior::CelebrateBig, Tone::Success);

        assert!(reaction_hold(&quiet_pass) < reaction_hold(&ordinary_pass));
        assert!(reaction_hold(&ordinary_pass) < reaction_hold(&first_error));
        assert!(reaction_hold(&first_error) < reaction_hold(&big_recovery));
        assert!(reaction_hold(&big_recovery) < reaction_hold(&repeated_error));
        assert_eq!(reaction_hold(&repeated_error), Duration::from_secs(10));
    }

    #[test]
    fn only_an_active_full_motion_owner_uses_the_fast_frame_cadence() {
        assert_eq!(
            surface_frame_delay(OrganismMotion::Full, true, false, false),
            SURFACE_FRAME_INTERVAL
        );
        assert_eq!(
            surface_frame_delay(OrganismMotion::Full, true, false, true),
            DORMANT_FRAME_INTERVAL
        );
        assert_eq!(
            surface_frame_delay(OrganismMotion::Full, true, true, false),
            DORMANT_FRAME_INTERVAL,
            "an alternate-screen owner has no visible live surface to animate"
        );
        assert_eq!(
            surface_frame_delay(OrganismMotion::Full, false, false, false),
            DORMANT_FRAME_INTERVAL
        );
        assert_eq!(
            surface_frame_delay(OrganismMotion::Calm, true, false, false),
            DORMANT_FRAME_INTERVAL
        );
        assert_eq!(
            surface_frame_delay(OrganismMotion::Static, true, false, false),
            DORMANT_FRAME_INTERVAL
        );
    }

    #[test]
    fn focus_transfer_rearms_only_a_still_pending_timer() {
        assert_eq!(
            focus_transfer_rearm_delay(OrganismMotion::Full, true, false, true),
            Some(SURFACE_FRAME_INTERVAL)
        );
        assert_eq!(
            focus_transfer_rearm_delay(OrganismMotion::Full, true, true, true),
            Some(DORMANT_FRAME_INTERVAL),
            "focus cannot make an alternate-screen surface visible"
        );
        assert_eq!(
            focus_transfer_rearm_delay(OrganismMotion::Full, false, false, true),
            Some(DORMANT_FRAME_INTERVAL)
        );
        assert_eq!(
            focus_transfer_rearm_delay(OrganismMotion::Calm, true, false, true),
            Some(DORMANT_FRAME_INTERVAL)
        );
        assert_eq!(
            focus_transfer_rearm_delay(OrganismMotion::Full, true, false, false),
            None,
            "a fired callback owns the vacant slot and will reschedule itself"
        );
    }

    #[test]
    fn approach_eases_out_arrives_exactly_and_never_overshoots() {
        let cell = 8.0;
        // Long trip: brisk start, easing steps, exact arrival.
        let mut position = 0.0;
        let target = 40.0 * cell;
        let mut steps = 0;
        while position != target {
            let next = approach(position, target, cell);
            assert!(next > position && next <= target);
            assert_eq!((next - position) as i32 % cell as i32, 0);
            position = next;
            steps += 1;
            assert!(steps < 40, "must converge quickly");
        }
        assert!(steps <= 16, "a full pane crosses in ~a second");

        // One-cell trip snaps home; sub-half-cell noise snaps home.
        assert_eq!(approach(0.0, cell, cell), cell);
        assert_eq!(approach(cell - 2.0, cell, cell), cell);
        // Works in both directions.
        assert!(approach(target, 0.0, cell) < target);
        // Hostile cell size clamps to one pixel and still makes progress.
        let hostile = approach(0.0, 5.0, 0.0);
        assert!(hostile > 0.0 && hostile <= 5.0);
    }

    #[test]
    fn watching_and_reacting_stay_below_output_or_hide() {
        let surface = SurfaceBox {
            width: 360,
            height: 400,
            right_gutter: 20,
            cell_width: 8,
            cell_height: 16,
            body_width: 88,
            body_height: 48,
            cursor_row: 6,
        };
        let clear_floor = f64::from((surface.cursor_row + 1) * surface.cell_height);
        for mode in [SurfaceMode::Watching, SurfaceMode::Reacting] {
            for frame in [0, 3] {
                let point = surface_point(
                    surface,
                    mode,
                    WanderTempo::Calm,
                    AmbientBehavior::Idle,
                    frame,
                )
                .unwrap();
                assert!(point.y >= clear_floor);
                assert!(point.y + f64::from(surface.body_height) <= 384.0);
            }
        }
        assert_eq!(
            surface_point(
                surface,
                SurfaceMode::Watching,
                WanderTempo::Calm,
                AmbientBehavior::Idle,
                0,
            ),
            surface_point(
                surface,
                SurfaceMode::Watching,
                WanderTempo::Calm,
                AmbientBehavior::Idle,
                3,
            ),
            "watching stays spatially still between real output pulses"
        );

        // Failure/stuck are idle intentions, but spatially remain at the
        // completed output edge. Recovery moves to the prompt-side bottom.
        let failure = surface_point(
            surface,
            SurfaceMode::Idle,
            WanderTempo::Calm,
            AmbientBehavior::GuardFailure,
            0,
        )
        .unwrap();
        let stuck = surface_point(
            surface,
            SurfaceMode::Idle,
            WanderTempo::Calm,
            AmbientBehavior::GuardStuck,
            400,
        )
        .unwrap();
        assert_eq!(failure, stuck);
        assert_eq!(failure.y, f64::from(below_output_y(surface).unwrap()));
        let recovery = surface_point(
            surface,
            SurfaceMode::Idle,
            WanderTempo::Calm,
            AmbientBehavior::GuardRecovery,
            0,
        )
        .unwrap();
        assert!(recovery.y > failure.y);
        let sleeping_failure = surface_point_for_vigil(
            surface,
            SurfaceMode::Idle,
            WanderTempo::Drowsy,
            AmbientBehavior::Sleep,
            RepoVigil::Failure,
            0,
        )
        .unwrap();
        assert_eq!(sleeping_failure.y, failure.y);

        // Full-motion interpolation from the old upper typing corner would
        // cross the output band. The runtime projects it directly to the safe
        // target floor while horizontal motion remains free to interpolate.
        let typing = surface_point(
            surface,
            SurfaceMode::Typing,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            0,
        )
        .unwrap();
        let watching = surface_point(
            surface,
            SurfaceMode::Watching,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            0,
        )
        .unwrap();
        let raw = approach(typing.y, watching.y, f64::from(surface.cell_height));
        assert!(raw < clear_floor);
        assert!(raw.max(f64::from(below_output_y(surface).unwrap())) >= clear_floor);

        // Once already inside the clear band, a reaction keeps the ordinary
        // eased walk instead of teleporting to its centered target.
        let reacting = surface_point(
            surface,
            SurfaceMode::Reacting,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            0,
        )
        .unwrap();
        let eased = approach(watching.y, reacting.y, f64::from(surface.cell_height));
        let projected = eased.max(f64::from(below_output_y(surface).unwrap()));
        assert!(projected > watching.y);
        assert!(projected < reacting.y);

        let full_output = SurfaceBox {
            height: 200,
            cursor_row: 10,
            ..surface
        };
        assert_eq!(
            surface_point(
                full_output,
                SurfaceMode::Watching,
                WanderTempo::Calm,
                AmbientBehavior::Idle,
                0,
            ),
            None
        );
        assert_eq!(
            surface_point(
                full_output,
                SurfaceMode::Reacting,
                WanderTempo::Calm,
                AmbientBehavior::Idle,
                0,
            ),
            None
        );
        for vigil in [AmbientBehavior::GuardFailure, AmbientBehavior::GuardStuck] {
            assert_eq!(
                surface_point(full_output, SurfaceMode::Idle, WanderTempo::Calm, vigil, 0,),
                None,
                "an unresolved-work vigil must never fall back over output"
            );
        }
        assert_eq!(
            surface_point_for_vigil(
                full_output,
                SurfaceMode::Idle,
                WanderTempo::Drowsy,
                AmbientBehavior::Sleep,
                RepoVigil::Stuck,
                0,
            ),
            None,
            "forced rest must retain the unresolved-work output boundary"
        );
        assert!(surface_point(
            full_output,
            SurfaceMode::Idle,
            WanderTempo::Calm,
            AmbientBehavior::GuardRecovery,
            0,
        )
        .is_some());
    }

    #[test]
    fn wander_tempo_reshapes_the_cycle_and_calm_matches_the_original() {
        for (frame, expected) in [(0, 0), (359, 0), (380, 20), (400, 40), (759, 40), (799, 1)] {
            let (step, _) = wander_phase(frame, WanderTempo::Calm);
            assert_eq!(step, expected, "frame {frame}");
        }
        for frame in 0..1_600 {
            assert_eq!(wander_phase(frame, WanderTempo::Drowsy), (0, false));
            for tempo in [WanderTempo::Calm, WanderTempo::Restless] {
                let (step, _) = wander_phase(frame, tempo);
                assert!((0..=40).contains(&step));
            }
        }
        let walking_frames = |tempo| {
            (0..800)
                .filter(|frame| wander_phase(*frame, tempo).1)
                .count()
        };
        assert_eq!(walking_frames(WanderTempo::Calm), 80);
        assert_eq!(walking_frames(WanderTempo::Restless), 240);
    }

    #[test]
    fn idle_body_wanders_and_the_hidden_typing_fallback_is_geometrically_safe() {
        let surface = SurfaceBox {
            width: 360,
            height: 200,
            right_gutter: 20,
            cell_width: 8,
            cell_height: 16,
            body_width: 88,
            body_height: 48,
            cursor_row: 8,
        };
        let idle_left = surface_point(
            surface,
            SurfaceMode::Idle,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            0,
        )
        .unwrap();
        let idle_still = surface_point(
            surface,
            SurfaceMode::Idle,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            359,
        )
        .unwrap();
        let idle_right = surface_point(
            surface,
            SurfaceMode::Idle,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            400,
        )
        .unwrap();
        let typing = surface_point(
            surface,
            SurfaceMode::Typing,
            WanderTempo::Calm,
            AmbientBehavior::Idle,
            0,
        )
        .unwrap();
        assert_eq!(idle_still, idle_left);
        assert!(idle_right.x > idle_left.x);
        assert_eq!(typing.x, idle_right.x);
        assert!(typing.y < idle_left.y);
    }
}
