//! No-LLM state reducer for Anvil's native ASCII organism.
//!
//! This module is intentionally GTK-free. Block panes feed it authoritative
//! command lifecycle events; the UI renders the returned [`Reaction`]. It does
//! not inspect output contents, execute commands, or perform network I/O.

use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandKind {
    BuildOrTest,
    GitPush,
    Other,
}

impl CommandKind {
    const fn label(self) -> &'static str {
        match self {
            Self::BuildOrTest => "build/test",
            Self::GitPush => "git push",
            Self::Other => "command",
        }
    }
}

/// Content-free, repo/day-scoped work facts shared between memory, reducers,
/// and the window coordinator. Repository identity never enters this value;
/// the UI uses it only to route the snapshot to the matching pane bodies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RepoWorkState {
    pub(crate) open_failures: u32,
    pub(crate) recovered_pending_push: bool,
    pub(crate) failure_success_flips: u32,
}

impl RepoWorkState {
    pub(crate) const fn new(
        open_failures: u32,
        recovered_pending_push: bool,
        failure_success_flips: u32,
    ) -> Self {
        Self {
            open_failures,
            // Ordered replay never leaves recovered and failed work open at
            // once. Normalize hostile/runtime callers to the safer failure
            // interpretation instead of creating an ambiguous visible state.
            recovered_pending_push: recovered_pending_push && open_failures == 0,
            failure_success_flips,
        }
    }

    pub(crate) const fn vigil(self) -> RepoVigil {
        if self.open_failures >= STUCK_OPEN_FAILURES {
            RepoVigil::Stuck
        } else if self.open_failures > 0 {
            RepoVigil::Failure
        } else if self.recovered_pending_push
            && self.failure_success_flips >= CAUTIOUS_RECOVERY_FLIPS
        {
            RepoVigil::CautiousRecovery
        } else if self.recovered_pending_push {
            RepoVigil::Recovery
        } else {
            RepoVigil::None
        }
    }
}

/// Visible phase of the current repo/day work loop. It is always derived from
/// [`RepoWorkState`], never persisted as a second source of truth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RepoVigil {
    #[default]
    None,
    Failure,
    Stuck,
    Recovery,
    CautiousRecovery,
}

impl RepoVigil {
    pub(crate) const fn is_active(self) -> bool {
        !matches!(self, Self::None)
    }
}

const STUCK_OPEN_FAILURES: u32 = 3;
const CAUTIOUS_RECOVERY_FLIPS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Behavior {
    Idle,
    WatchCommand,
    InspectError,
    SitNearError,
    Celebrate,
    CelebrateBig,
    RestAfterPush,
    UnknownOutcome,
    /// A live-only orienting cue: another local pane reported a failure. It
    /// never comes from the reducer and carries no command or output content.
    GlanceAside,
    // Ambient dispositions chosen by the utility mind, never by event
    // reactions: they only ever reach the display through AmbientBehavior.
    Sleep,
    Explore,
    Approach,
    /// Crouched a little apart, watching the Shell Agent work — distinct from
    /// WatchCommand so the body shows whose command is running.
    WatchAgent,
    /// Settled in for a long command: still watching, but lying down with
    /// half-closed eyes — a vigil, not a nap.
    WatchSettled,
    /// A failed build is still open in this repo/day. It stays with the work
    /// after the immediate error reaction has settled.
    GuardFailure,
    /// Three or more failures remain open. The lower, quieter posture records
    /// that the loop is stubborn without escalating speech or stimulus.
    GuardStuck,
    /// A recovered build has not been pushed yet. Between real events the cat
    /// stays beside that finished work instead of forgetting it at settle.
    GuardRecovery,
    /// Repeated same-day failure/success flips make the recovery worth checking
    /// again before push; this is caution, not a claim that a test is flaky.
    GuardCautious,
}

// ── Live-body frame sets ────────────────────────────────────────────────
// Every frame within one visual set shares its bounding box (identical line
// count and maximum line width), so the overlay's measured size — and with it
// the fail-closed fit check in `surface_point` — never flaps between frames.
const IDLE_FRAMES: [&str; 2] = [" /\\_/\\\n( -.- )\n > ^ <", " /\\_/\\\n( -.- )\n >~^ <"];
const IDLE_TENSE: &str = " =\\_/=\n( -.- )\n > ^ <";
const YAWN_FRAME: &str = " /\\_/\\\n( >o< )\n > ^ <";
const DOZE_FRAMES: [&str; 2] = [" /\\_/\\\n( =.= )\n  zzZ ", " /\\_/\\\n( =.= )\n   zZ "];
const GAIT_FRAMES: [&str; 2] = [" /\\_/\\\n( o.o )\n >/ \\<", " /\\_/\\\n( o.o )\n >\\ /<"];
const GAIT_TENSE_FRAMES: [&str; 2] = [" =\\_/=\n( o.o )\n >/ \\<", " =\\_/=\n( o.o )\n >\\ /<"];
const WATCH_FRAMES: [&str; 2] = [" /\\_/\\\n( o.o )\n > ^ <", " /\\_/\\\n( o.o )\n >~^ <"];
const WATCH_TENSE_FRAMES: [&str; 2] = [" =\\_/=\n( o.o )\n > ^ <", " =\\_/=\n( o.o )\n >~^ <"];
const WATCH_BUSY_FRAMES: [&str; 2] = [" /\\_/\\\n( o.> )\n > ^ <", " /\\_/\\\n( <.o )\n >~^ <"];
const WATCH_BUSY_TENSE_FRAMES: [&str; 2] = [" =\\_/=\n( o.> )\n > ^ <", " =\\_/=\n( <.o )\n >~^ <"];
const WATCH_WAITING_FRAMES: [&str; 2] =
    [" /\\_/\\\n( -.- )\n (___) ", " /\\_/\\\n( -.o )\n (___) "];
const WATCH_WAITING_TENSE_FRAMES: [&str; 2] =
    [" =\\_/=\n( -.- )\n (___) ", " =\\_/=\n( -.o )\n (___) "];
const WATCH_RESUMED_FRAMES: [&str; 2] = [" /\\_/\\\n( O.O )\n > ^ <", " /\\_/\\\n( o.o )\n >~^ <"];
const WATCH_RESUMED_TENSE_FRAMES: [&str; 2] =
    [" =\\_/=\n( O.O )\n > ^ <", " =\\_/=\n( o.o )\n >~^ <"];
const INSPECT_FRAME: &str = " /\\_/\\  ->\n( o_o )\n /|_|\\";
const SIT_FRAMES: [&str; 2] = [
    " /\\_/\\\n( ._. )  !\n /|_|\\",
    " /\\_/\\\n( ._. )   \n /|_|\\",
];
// Celebration keeps the same cat silhouette as every other behavior; the
// old raised-arm human figure made a successful build look like a different
// character had replaced the organism.
const CELE_FRAMES: [&str; 2] = [
    " /\\_/\\\n<( ^.^ )>\n  > ^ <",
    " /\\_/\\\n<( ^o^ )>\n  > ^ <",
];
const BIG_FRAMES: [&str; 2] = [
    "* /\\_/\\ *\n<( ^o^ )>\n* > ^ < *",
    "  /\\_/\\  \n<( ^o^ )>\n  > ^ <  ",
];
const REST_FRAMES: [&str; 2] = [
    " /\\_/\\\n( ^.^ )  ok\n > ^ <",
    " /\\_/\\\n( ^.^ )  ok\n >~^ <",
];
const UNKNOWN_FRAME: &str = " /\\_/\\\n( ?.? )\n > ^ <";
const GLANCE_ASIDE_FRAMES: [&str; 2] = [" /\\_/\\\n( <.< )\n > ^ <", " /\\_/\\\n( <.< )\n >~^ <"];
const SLEEP_FRAMES: [&str; 2] = [
    " /\\_/\\\n( -_- )zZ\n (___) ",
    " /\\_/\\\n( -_- )Z \n (___) ",
];
const EXPLORE_FRAMES: [&str; 2] = [" /\\_/\\\n( o.o)?\n > ^ <", " /\\_/\\\n?(o.o )\n > ^ <"];
const APPROACH_FRAMES: [&str; 2] = [" /\\_/\\\n( ^.^ )\n > ^ <", " /\\_/\\\n( ^.^ )\n >~^ <"];
const WATCH_AGENT_FRAMES: [&str; 2] = [" /\\_/\\\n( -.o )\n (___) ", " /\\_/\\\n( o.- )\n (___) "];
const WATCH_AGENT_BUSY_FRAMES: [&str; 2] =
    [" /\\_/\\\n( -.> )\n (___) ", " /\\_/\\\n( <.- )\n (___) "];
const WATCH_AGENT_WAITING_FRAMES: [&str; 2] =
    [" /\\_/\\\n( -.- )\n (___) ", " /\\_/\\\n( -.o )\n (___) "];
const WATCH_AGENT_RESUMED_FRAMES: [&str; 2] =
    [" /\\_/\\\n( o.o )\n (___) ", " /\\_/\\\n( -.o )\n (___) "];
const WATCH_SETTLED_FRAMES: [&str; 2] =
    [" /\\_/\\\n( -.- )\n (___) ", " /\\_/\\\n( -.o )\n (___) "];
const WATCH_SETTLED_BUSY_FRAMES: [&str; 2] =
    [" /\\_/\\\n( -.> )\n (___) ", " /\\_/\\\n( <.- )\n (___) "];
const WATCH_SETTLED_WAITING_FRAMES: [&str; 2] =
    [" /\\_/\\\n( ._. )\n (___) ", " /\\_/\\\n( -.- )\n (___) "];
const WATCH_SETTLED_RESUMED_FRAMES: [&str; 2] =
    [" /\\_/\\\n( o.o )\n (___) ", " /\\_/\\\n( -.o )\n (___) "];
const GUARD_FAILURE_FRAMES: [&str; 2] = [
    " /\\_/\\\n( o_o ) [!]\n /|_|\\",
    " /\\_/\\\n( o.o ) [!]\n /|_|\\",
];
const GUARD_STUCK_FRAMES: [&str; 2] = [
    " =\\_/=\n( ._. ) [!!]\n /|_|\\",
    " =\\_/=\n( -.- ) [!!]\n /|_|\\",
];
const GUARD_RECOVERY_FRAMES: [&str; 2] = [
    " /\\_/\\\n( -.- ) [ok]\n /|_|\\",
    " /\\_/\\\n( -.o ) [ok]\n /|_|\\",
];
const GUARD_CAUTIOUS_FRAMES: [&str; 2] = [
    " /\\_/\\\n( ?.? ) [?]\n /|_|\\",
    " /\\_/\\\n( ?.o ) [?]\n /|_|\\",
];

// Semantic pose changes are short, one-shot arcs rather than new reducer
// states. Every frame in one arc has the same three-line bounding box. The UI
// can therefore render the intermediate frames in Full motion, while Calm and
// Static can keep using the canonical destination pose.
const INSPECT_TO_GUARD_FAILURE_FRAMES: [&str; 4] = [
    " /\\_/\\  -->\n( o_o )\n /|_|\\",
    " /\\_/\\  <- \n( o_o ) [!]\n /|_|\\",
    " /\\_/\\     \n( ._. ) [!]\n /|_|\\",
    " /\\_/\\     \n( o_o ) [!]\n /|_|\\",
];
const SIT_TO_GUARD_STUCK_FRAMES: [&str; 4] = [
    " =\\_/=      \n( ._. )  !!\n /|_|\\",
    " =\\_/=      \n( -.- ) [!!]\n /|_|\\",
    " =\\_/=      \n( ._. ) [!!]\n /|_|\\",
    " =\\_/=      \n( -.- ) [!!]\n /|_|\\",
];
const FAILURE_TO_RECOVERY_FRAMES: [&str; 4] = [
    " /\\_/\\      \n( o_o ) [! ]\n /|_|\\",
    " /\\_/\\      \n( ._. ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( -.o ) [ok]\n /|_|\\",
    " /\\_/\\      \n( -.- ) [ok]\n /|_|\\",
];
const FAILURE_TO_CAUTIOUS_FRAMES: [&str; 4] = [
    " /\\_/\\      \n( o_o ) [! ]\n /|_|\\",
    " /\\_/\\      \n( ._. ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( ?.o ) [? ]\n /|_|\\",
    " /\\_/\\      \n( ?.? ) [? ]\n /|_|\\",
];
const STUCK_TO_RECOVERY_FRAMES: [&str; 4] = [
    " =\\_/=      \n( ._. ) [!!]\n /|_|\\",
    " =\\_/=      \n( -.- ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( -.o ) [ok]\n /|_|\\",
    " /\\_/\\      \n( -.- ) [ok]\n /|_|\\",
];
const STUCK_TO_CAUTIOUS_FRAMES: [&str; 4] = [
    " =\\_/=      \n( ._. ) [!!]\n /|_|\\",
    " =\\_/=      \n( -.- ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( ?.o ) [? ]\n /|_|\\",
    " /\\_/\\      \n( ?.? ) [? ]\n /|_|\\",
];
const SETTLED_TO_CELEBRATE_FRAMES: [&str; 4] = [
    " /\\_/\\\n ( -.- ) \n  (___)  ",
    " /\\_/\\\n ( o.o ) \n  > ^ <  ",
    " /\\_/\\\n<( ^.^ )>\n  > ^ <  ",
    " /\\_/\\\n<( ^o^ )>\n  > ^ <  ",
];
const SETTLED_TO_CELEBRATE_BIG_FRAMES: [&str; 4] = [
    "  /\\_/\\  \n ( -.- ) \n  (___)  ",
    "  /\\_/\\  \n<( o.o )>\n  > ^ <  ",
    "* /\\_/\\ *\n<( ^.^ )>\n* > ^ < *",
    "* /\\_/\\ *\n<( ^o^ )>\n* > ^ < *",
];
const CELEBRATE_TO_RECOVERY_FRAMES: [&str; 4] = [
    " /\\_/\\      \n<( ^.^ )>   \n  > ^ <     ",
    " /\\_/\\      \n ( ^.^ )    \n  > ^ <     ",
    " /\\_/\\      \n( -.o ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( -.- ) [ok]\n /|_|\\",
];
const CELEBRATE_TO_CAUTIOUS_FRAMES: [&str; 4] = [
    " /\\_/\\      \n<( ^.^ )>   \n  > ^ <     ",
    " /\\_/\\      \n ( ^.^ )    \n  > ^ <     ",
    " /\\_/\\      \n( ?.o ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( ?.? ) [? ]\n /|_|\\",
];
const CELEBRATE_BIG_TO_RECOVERY_FRAMES: [&str; 4] = [
    "* /\\_/\\ *   \n<( ^o^ )>   \n* > ^ < *   ",
    "  /\\_/\\     \n<( ^.^ )>   \n  > ^ <     ",
    " /\\_/\\      \n( -.o ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( -.- ) [ok]\n /|_|\\",
];
const CELEBRATE_BIG_TO_CAUTIOUS_FRAMES: [&str; 4] = [
    "* /\\_/\\ *   \n<( ^o^ )>   \n* > ^ < *   ",
    "  /\\_/\\     \n<( ^.^ )>   \n  > ^ <     ",
    " /\\_/\\      \n( ?.o ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( ?.? ) [? ]\n /|_|\\",
];
const RECOVERY_TO_REST_FRAMES: [&str; 4] = [
    " /\\_/\\      \n( -.- ) [ok]\n /|_|\\",
    " /\\_/\\      \n( -.o ) [ok]\n /|_|\\",
    " /\\_/\\      \n( ^.^ ) [ok]\n > ^ <",
    " /\\_/\\      \n( ^.^ ) [ok]\n >~^ <",
];
const CAUTIOUS_TO_REST_FRAMES: [&str; 4] = [
    " /\\_/\\      \n( ?.? ) [? ]\n /|_|\\",
    " /\\_/\\      \n( ?.o ) [~ ]\n /|_|\\",
    " /\\_/\\      \n( ^.^ ) [ok]\n > ^ <",
    " /\\_/\\      \n( ^.^ ) [ok]\n >~^ <",
];

impl Behavior {
    /// Canonical single pose: the first frame of each behavior's set. Used by
    /// the inline card, which records events rather than animating.
    pub(crate) const fn sprite(self) -> &'static str {
        match self {
            Self::Idle => IDLE_FRAMES[0],
            Self::WatchCommand => WATCH_FRAMES[0],
            Self::InspectError => INSPECT_FRAME,
            Self::SitNearError => SIT_FRAMES[0],
            Self::Celebrate => CELE_FRAMES[0],
            Self::CelebrateBig => BIG_FRAMES[0],
            Self::RestAfterPush => REST_FRAMES[0],
            Self::UnknownOutcome => UNKNOWN_FRAME,
            Self::GlanceAside => GLANCE_ASIDE_FRAMES[0],
            Self::Sleep => SLEEP_FRAMES[0],
            Self::Explore => EXPLORE_FRAMES[0],
            Self::Approach => APPROACH_FRAMES[0],
            Self::WatchAgent => WATCH_AGENT_FRAMES[0],
            Self::WatchSettled => WATCH_SETTLED_FRAMES[0],
            Self::GuardFailure => GUARD_FAILURE_FRAMES[0],
            Self::GuardStuck => GUARD_STUCK_FRAMES[0],
            Self::GuardRecovery => GUARD_RECOVERY_FRAMES[0],
            Self::GuardCautious => GUARD_CAUTIOUS_FRAMES[0],
        }
    }

    pub(crate) const fn is_repo_vigil(self) -> bool {
        matches!(
            self,
            Self::GuardFailure | Self::GuardStuck | Self::GuardRecovery | Self::GuardCautious
        )
    }

    /// One-line micro-poses for the sticky scrollback header. Every glyph is
    /// exactly five ASCII characters wide so the header never re-measures when
    /// the pose or animation frame changes.
    const fn sticky_frames(self) -> [&'static str; 2] {
        match self {
            Self::Idle => ["/\\_/\\", "/\\~/\\"],
            Self::WatchCommand => ["/\\_/\\", "/\\o/\\"],
            Self::InspectError => ["/\\!/\\", "/\\!/\\"],
            Self::SitNearError => ["=\\_/=", "=\\_/="],
            Self::Celebrate => ["*\\_/*", "*\\_/*"],
            Self::CelebrateBig => ["*\\o/*", "*\\_/*"],
            Self::RestAfterPush => ["/\\z/\\", "/\\_/\\"],
            Self::UnknownOutcome => ["/\\?/\\", "/\\?/\\"],
            Self::GlanceAside => ["/\\</\\", "/\\</\\"],
            Self::Sleep => ["=\\z/=", "=\\_/="],
            Self::Explore => ["~\\_/~", "/\\_/\\"],
            Self::Approach => ["/\\^/\\", "/\\_/\\"],
            Self::WatchAgent => ["/\\./\\", "/\\_/\\"],
            Self::WatchSettled => ["/\\-/\\", "/\\_/\\"],
            Self::GuardFailure => ["/\\!/\\", "/\\o/\\"],
            Self::GuardStuck => ["=\\!/=", "=\\_/="],
            Self::GuardRecovery => ["/\\+/\\", "/\\o/\\"],
            Self::GuardCautious => ["/\\?/\\", "/\\o/\\"],
        }
    }
}

/// Quantized, content-free body language derived from the continuous life
/// state. Only ambient poses (Idle/WatchCommand) let it show through —
/// reaction poses stay canonical so event records remain unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct BodyLanguage {
    /// Low energy: lie down and doze instead of sitting, stop wandering.
    pub(crate) drowsy: bool,
    /// High stress: ears pressed flat while idling or watching.
    pub(crate) tense: bool,
    /// Boredom at the ceiling: occasional yawns, restless wandering.
    pub(crate) listless: bool,
}

impl BodyLanguage {
    pub(crate) fn from_state(state: LifeState) -> Self {
        let drowsy = state.energy < 0.25;
        Self {
            drowsy,
            tense: state.stress > 0.60,
            listless: !drowsy && state.boredom > 0.85,
        }
    }
}

/// Coarse lifetime appearance supplied by the memory/UI boundary. This lives
/// in the renderer rather than importing memory's `GrowthStage`, which keeps
/// the no-LLM reducer independent and avoids a module cycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum VisualGrowthStage {
    Juvenile,
    #[default]
    Adult,
    Seasoned,
}

/// Content-free rhythm of a running command. It describes only the cadence of
/// output activity, never bytes, lines, commands, or terminal contents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WatchRhythm {
    #[default]
    Steady,
    Busy,
    Waiting,
    Resumed,
}

/// A short visual bridge between semantically adjacent canonical behaviors.
/// Reducer state remains authoritative: these variants carry no data and are
/// selected only after the UI has already observed both endpoint behaviors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualTransition {
    InspectErrorToGuardFailure,
    SitNearErrorToGuardStuck,
    GuardFailureToGuardRecovery,
    GuardFailureToGuardCautious,
    GuardStuckToGuardRecovery,
    GuardStuckToGuardCautious,
    WatchSettledToCelebrate,
    WatchSettledToCelebrateBig,
    CelebrateToGuardRecovery,
    CelebrateToGuardCautious,
    CelebrateBigToGuardRecovery,
    CelebrateBigToGuardCautious,
    GuardRecoveryToRestAfterPush,
    GuardCautiousToRestAfterPush,
}

impl VisualTransition {
    /// Recognize only intentional arcs. Unrelated behavior changes snap to the
    /// new canonical pose instead of inventing a misleading transition.
    pub(crate) const fn between(from: Behavior, to: Behavior) -> Option<Self> {
        match (from, to) {
            (Behavior::InspectError, Behavior::GuardFailure) => {
                Some(Self::InspectErrorToGuardFailure)
            }
            (Behavior::SitNearError, Behavior::GuardStuck) => Some(Self::SitNearErrorToGuardStuck),
            (Behavior::GuardFailure, Behavior::GuardRecovery) => {
                Some(Self::GuardFailureToGuardRecovery)
            }
            (Behavior::GuardFailure, Behavior::GuardCautious) => {
                Some(Self::GuardFailureToGuardCautious)
            }
            (Behavior::GuardStuck, Behavior::GuardRecovery) => {
                Some(Self::GuardStuckToGuardRecovery)
            }
            (Behavior::GuardStuck, Behavior::GuardCautious) => {
                Some(Self::GuardStuckToGuardCautious)
            }
            (Behavior::WatchSettled, Behavior::Celebrate) => Some(Self::WatchSettledToCelebrate),
            (Behavior::WatchSettled, Behavior::CelebrateBig) => {
                Some(Self::WatchSettledToCelebrateBig)
            }
            (Behavior::Celebrate, Behavior::GuardRecovery) => Some(Self::CelebrateToGuardRecovery),
            (Behavior::Celebrate, Behavior::GuardCautious) => Some(Self::CelebrateToGuardCautious),
            (Behavior::CelebrateBig, Behavior::GuardRecovery) => {
                Some(Self::CelebrateBigToGuardRecovery)
            }
            (Behavior::CelebrateBig, Behavior::GuardCautious) => {
                Some(Self::CelebrateBigToGuardCautious)
            }
            (Behavior::GuardRecovery, Behavior::RestAfterPush) => {
                Some(Self::GuardRecoveryToRestAfterPush)
            }
            (Behavior::GuardCautious, Behavior::RestAfterPush) => {
                Some(Self::GuardCautiousToRestAfterPush)
            }
            _ => None,
        }
    }

    pub(crate) const fn source(self) -> Behavior {
        match self {
            Self::InspectErrorToGuardFailure => Behavior::InspectError,
            Self::SitNearErrorToGuardStuck => Behavior::SitNearError,
            Self::GuardFailureToGuardRecovery | Self::GuardFailureToGuardCautious => {
                Behavior::GuardFailure
            }
            Self::GuardStuckToGuardRecovery | Self::GuardStuckToGuardCautious => {
                Behavior::GuardStuck
            }
            Self::WatchSettledToCelebrate | Self::WatchSettledToCelebrateBig => {
                Behavior::WatchSettled
            }
            Self::CelebrateToGuardRecovery | Self::CelebrateToGuardCautious => Behavior::Celebrate,
            Self::CelebrateBigToGuardRecovery | Self::CelebrateBigToGuardCautious => {
                Behavior::CelebrateBig
            }
            Self::GuardRecoveryToRestAfterPush => Behavior::GuardRecovery,
            Self::GuardCautiousToRestAfterPush => Behavior::GuardCautious,
        }
    }

    pub(crate) const fn target(self) -> Behavior {
        match self {
            Self::InspectErrorToGuardFailure => Behavior::GuardFailure,
            Self::SitNearErrorToGuardStuck => Behavior::GuardStuck,
            Self::GuardFailureToGuardRecovery | Self::GuardStuckToGuardRecovery => {
                Behavior::GuardRecovery
            }
            Self::GuardFailureToGuardCautious | Self::GuardStuckToGuardCautious => {
                Behavior::GuardCautious
            }
            Self::WatchSettledToCelebrate => Behavior::Celebrate,
            Self::WatchSettledToCelebrateBig => Behavior::CelebrateBig,
            Self::CelebrateToGuardRecovery | Self::CelebrateBigToGuardRecovery => {
                Behavior::GuardRecovery
            }
            Self::CelebrateToGuardCautious | Self::CelebrateBigToGuardCautious => {
                Behavior::GuardCautious
            }
            Self::GuardRecoveryToRestAfterPush | Self::GuardCautiousToRestAfterPush => {
                Behavior::RestAfterPush
            }
        }
    }

    pub(crate) const fn frame_count(self) -> u64 {
        4
    }

    /// `frame` is relative to the start of the arc. Holding beyond the fourth
    /// frame is safe and keeps the last bridge pose until the UI clears it.
    pub(crate) const fn sprite_frame(self, frame: u64) -> &'static str {
        let index = if frame >= self.frame_count() {
            self.frame_count() as usize - 1
        } else {
            frame as usize
        };
        match self {
            Self::InspectErrorToGuardFailure => INSPECT_TO_GUARD_FAILURE_FRAMES[index],
            Self::SitNearErrorToGuardStuck => SIT_TO_GUARD_STUCK_FRAMES[index],
            Self::GuardFailureToGuardRecovery => FAILURE_TO_RECOVERY_FRAMES[index],
            Self::GuardFailureToGuardCautious => FAILURE_TO_CAUTIOUS_FRAMES[index],
            Self::GuardStuckToGuardRecovery => STUCK_TO_RECOVERY_FRAMES[index],
            Self::GuardStuckToGuardCautious => STUCK_TO_CAUTIOUS_FRAMES[index],
            Self::WatchSettledToCelebrate => SETTLED_TO_CELEBRATE_FRAMES[index],
            Self::WatchSettledToCelebrateBig => SETTLED_TO_CELEBRATE_BIG_FRAMES[index],
            Self::CelebrateToGuardRecovery => CELEBRATE_TO_RECOVERY_FRAMES[index],
            Self::CelebrateToGuardCautious => CELEBRATE_TO_CAUTIOUS_FRAMES[index],
            Self::CelebrateBigToGuardRecovery => CELEBRATE_BIG_TO_RECOVERY_FRAMES[index],
            Self::CelebrateBigToGuardCautious => CELEBRATE_BIG_TO_CAUTIOUS_FRAMES[index],
            Self::GuardRecoveryToRestAfterPush => RECOVERY_TO_REST_FRAMES[index],
            Self::GuardCautiousToRestAfterPush => CAUTIOUS_TO_REST_FRAMES[index],
        }
    }
}

/// All orthogonal visual inputs for one render. Event semantics stay in
/// `behavior`; body language, lifetime appearance, output rhythm, and an
/// optional one-shot bridge merely choose how that state is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderContext {
    pub(crate) behavior: Behavior,
    pub(crate) body_language: BodyLanguage,
    pub(crate) walking: bool,
    pub(crate) growth_stage: VisualGrowthStage,
    pub(crate) watch_rhythm: WatchRhythm,
    pub(crate) transition: Option<VisualTransition>,
}

impl RenderContext {
    /// Backwards-compatible visual defaults: adult, steady cadence, and no
    /// semantic bridge.
    pub(crate) const fn new(
        behavior: Behavior,
        body_language: BodyLanguage,
        walking: bool,
    ) -> Self {
        Self {
            behavior,
            body_language,
            walking,
            growth_stage: VisualGrowthStage::Adult,
            watch_rhythm: WatchRhythm::Steady,
            transition: None,
        }
    }

    pub(crate) const fn with_growth_stage(mut self, growth_stage: VisualGrowthStage) -> Self {
        self.growth_stage = growth_stage;
        self
    }

    pub(crate) const fn with_watch_rhythm(mut self, watch_rhythm: WatchRhythm) -> Self {
        self.watch_rhythm = watch_rhythm;
        self
    }

    pub(crate) const fn with_transition(mut self, transition: Option<VisualTransition>) -> Self {
        self.transition = transition;
        self
    }
}

/// Pick the live-body sprite for this animation frame, on the same
/// half-second beat the sticky header uses; rare flourishes (tail flick,
/// yawn) sit on their own longer cadences. Output-activity pulses advance
/// `frame` faster, so a busy command visibly quickens the tail.
pub(crate) fn sprite_frame(
    behavior: Behavior,
    language: BodyLanguage,
    walking: bool,
    frame: u64,
) -> &'static str {
    base_sprite_frame(behavior, language, walking, WatchRhythm::Steady, frame / 5)
}

/// Render all composable appearance inputs. The returned `Cow` stays borrowed
/// for the adult stage and allocates only when the five-character maturity
/// grammar needs to be overlaid on a canonical frame.
pub(crate) fn sprite_frame_with_context(context: RenderContext, frame: u64) -> Cow<'static, str> {
    let sprite = if let Some(transition) = context.transition {
        transition.sprite_frame(frame)
    } else {
        let cadence = render_cadence(context);
        base_sprite_frame(
            context.behavior,
            context.body_language,
            context.walking,
            context.watch_rhythm,
            frame / cadence,
        )
    };
    apply_sprite_growth(sprite, context.growth_stage)
}

fn render_cadence(context: RenderContext) -> u64 {
    let base = match context.growth_stage {
        VisualGrowthStage::Juvenile => 4,
        VisualGrowthStage::Adult => 5,
        VisualGrowthStage::Seasoned => 8,
    };
    if !matches!(
        context.behavior,
        Behavior::WatchCommand | Behavior::WatchAgent | Behavior::WatchSettled
    ) {
        return base;
    }
    match context.watch_rhythm {
        WatchRhythm::Steady => base,
        WatchRhythm::Busy | WatchRhythm::Resumed => (base / 2).max(1),
        WatchRhythm::Waiting => base * 2,
    }
}

fn base_sprite_frame(
    behavior: Behavior,
    language: BodyLanguage,
    walking: bool,
    watch_rhythm: WatchRhythm,
    beat: u64,
) -> &'static str {
    let alt = usize::from(beat % 2 == 1);
    match behavior {
        Behavior::Idle if language.drowsy => DOZE_FRAMES[alt],
        Behavior::Idle if walking && language.tense => GAIT_TENSE_FRAMES[alt],
        Behavior::Idle if walking => GAIT_FRAMES[alt],
        Behavior::Idle if language.listless && beat % 12 == 11 => YAWN_FRAME,
        Behavior::Idle if language.tense => IDLE_TENSE,
        Behavior::Idle => IDLE_FRAMES[usize::from(beat % 8 == 7)],
        Behavior::WatchCommand => match (watch_rhythm, language.tense) {
            (WatchRhythm::Busy, true) => WATCH_BUSY_TENSE_FRAMES[alt],
            (WatchRhythm::Busy, false) => WATCH_BUSY_FRAMES[alt],
            (WatchRhythm::Waiting, true) => WATCH_WAITING_TENSE_FRAMES[alt],
            (WatchRhythm::Waiting, false) => WATCH_WAITING_FRAMES[alt],
            (WatchRhythm::Resumed, true) => WATCH_RESUMED_TENSE_FRAMES[alt],
            (WatchRhythm::Resumed, false) => WATCH_RESUMED_FRAMES[alt],
            (WatchRhythm::Steady, true) => WATCH_TENSE_FRAMES[alt],
            (WatchRhythm::Steady, false) => WATCH_FRAMES[alt],
        },
        Behavior::SitNearError => SIT_FRAMES[alt],
        Behavior::Celebrate => CELE_FRAMES[alt],
        Behavior::CelebrateBig => BIG_FRAMES[alt],
        Behavior::RestAfterPush => REST_FRAMES[alt],
        Behavior::GlanceAside => GLANCE_ASIDE_FRAMES[alt],
        Behavior::Sleep => SLEEP_FRAMES[alt],
        // Step only while actually moving; scanning happens while seated.
        Behavior::Explore if walking => GAIT_FRAMES[alt],
        Behavior::Explore => EXPLORE_FRAMES[alt],
        Behavior::Approach => APPROACH_FRAMES[alt],
        Behavior::WatchAgent => match watch_rhythm {
            WatchRhythm::Steady => WATCH_AGENT_FRAMES[alt],
            WatchRhythm::Busy => WATCH_AGENT_BUSY_FRAMES[alt],
            WatchRhythm::Waiting => WATCH_AGENT_WAITING_FRAMES[alt],
            WatchRhythm::Resumed => WATCH_AGENT_RESUMED_FRAMES[alt],
        },
        Behavior::WatchSettled => match watch_rhythm {
            WatchRhythm::Steady => WATCH_SETTLED_FRAMES[alt],
            WatchRhythm::Busy => WATCH_SETTLED_BUSY_FRAMES[alt],
            WatchRhythm::Waiting => WATCH_SETTLED_WAITING_FRAMES[alt],
            WatchRhythm::Resumed => WATCH_SETTLED_RESUMED_FRAMES[alt],
        },
        Behavior::GuardFailure => GUARD_FAILURE_FRAMES[alt],
        Behavior::GuardStuck => GUARD_STUCK_FRAMES[alt],
        Behavior::GuardRecovery => GUARD_RECOVERY_FRAMES[alt],
        Behavior::GuardCautious => GUARD_CAUTIOUS_FRAMES[alt],
        Behavior::InspectError | Behavior::UnknownOutcome => behavior.sprite(),
    }
}

fn apply_sprite_growth(sprite: &'static str, growth_stage: VisualGrowthStage) -> Cow<'static, str> {
    match growth_stage {
        VisualGrowthStage::Adult => Cow::Borrowed(sprite),
        VisualGrowthStage::Juvenile => {
            // Rounded ears, larger open eyes, and a short tail. Replacements
            // are ASCII and exactly width-preserving.
            let mut evolved = sprite
                .replace("/\\_/\\", "(\\_/)")
                .replace("=\\_/=", "(\\_/)");
            for (from, to) in [
                (" o.o ", " O.O "),
                (" o_o ", " O_O "),
                (" -.o ", " -.O "),
                (" o.- ", " O.- "),
                (" ?.o ", " ?.O "),
                (" ^o^ ", " ^O^ "),
                (" >o< ", " >O< "),
                (">~^ <", "> ^ <"),
            ] {
                evolved = evolved.replace(from, to);
            }
            Cow::Owned(evolved)
        }
        VisualGrowthStage::Seasoned => {
            // The right ear has a small notch and neutral open eyes settle to
            // a half-lidded gaze. Its animation cadence is slowed separately.
            let evolved = sprite
                .replace("/\\_/\\", "/\\_/|")
                .replace("=\\_/=", "=\\_/|")
                .replace(" o.o ", " -.o ")
                .replace(" O.O ", " -.O ");
            Cow::Owned(evolved)
        }
    }
}

/// Ambient disposition of a genuinely idle body — no command, no reaction
/// hold, no recent typing. Usually utility-selected by [`AmbientMind`]; repo
/// vigils are durable work intentions supplied by the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AmbientBehavior {
    Idle,
    Sleep,
    Explore,
    Approach,
    /// One or two unresolved build failures: keep the work in sight without
    /// turning a completed error reaction into permanent alarm.
    GuardFailure,
    /// Three or more unresolved failures: a lower, quieter debugging vigil.
    GuardStuck,
    /// A durable, repo-scoped intention supplied by the reducer rather than a
    /// random utility candidate: keep the recovered work company until push.
    GuardRecovery,
    /// Repeated failure/success flips make this recovery worth one more check.
    GuardCautious,
}

impl AmbientBehavior {
    pub(crate) const fn display(self) -> Behavior {
        match self {
            Self::Idle => Behavior::Idle,
            Self::Sleep => Behavior::Sleep,
            Self::Explore => Behavior::Explore,
            Self::Approach => Behavior::Approach,
            Self::GuardFailure => Behavior::GuardFailure,
            Self::GuardStuck => Behavior::GuardStuck,
            Self::GuardRecovery => Behavior::GuardRecovery,
            Self::GuardCautious => Behavior::GuardCautious,
        }
    }

    /// Once chosen, a disposition is held before rescoring so behavior does
    /// not reroll every frame — the prototype's behavior_hold_for timers.
    const fn hold_secs(self) -> f32 {
        match self {
            Self::Sleep => 2.5,
            Self::Explore => 1.4,
            Self::Approach => 1.8,
            Self::GuardFailure | Self::GuardStuck | Self::GuardRecovery | Self::GuardCautious => {
                2.0
            }
            Self::Idle => 1.0,
        }
    }

    const fn is_repo_vigil(self) -> bool {
        matches!(
            self,
            Self::GuardFailure | Self::GuardStuck | Self::GuardRecovery | Self::GuardCautious
        )
    }
}

/// Utility-scored ambient behavior selection, ported from the prototype's
/// `choose_utility_behavior`: candidates are scored from the continuous
/// state, the incumbent gets a small inertia bonus, deterministic xorshift
/// jitter keeps ties from freezing, and the winner is held for its own
/// timer. Exhaustion below [`FORCED_REST_ENERGY`] overrides the scores, so
/// the sleep-regenerate loop closes: a drained mind curls up, energy climbs,
/// and another disposition eventually outscores sleep.
#[derive(Debug)]
pub(crate) struct AmbientMind {
    current: AmbientBehavior,
    hold_for: f32,
    seed: u64,
}

impl Default for AmbientMind {
    fn default() -> Self {
        Self {
            current: AmbientBehavior::Idle,
            hold_for: 0.0,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

impl AmbientMind {
    /// A per-body seed so split-window bodies do not nap and pace in perfect
    /// lockstep. Any seed works; zero is displaced so xorshift never sticks.
    pub(crate) fn seeded(seed: u64) -> Self {
        Self {
            seed: (seed | 1).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            ..Self::default()
        }
    }

    pub(crate) fn current(&self) -> AmbientBehavior {
        self.current
    }

    /// Reset to plain idle when the body leaves ambient display (typing,
    /// watching, reacting), so a stale disposition never resumes later.
    pub(crate) fn interrupt(&mut self) {
        self.current = AmbientBehavior::Idle;
        self.hold_for = 0.0;
    }

    /// Advance the hold timer by `dt` seconds and rescore once it expires.
    /// `idle_for` is how long the terminal has been completely quiet.
    pub(crate) fn step(
        &mut self,
        state: LifeState,
        idle_for: f32,
        dt: f32,
        vigil: RepoVigil,
    ) -> AmbientBehavior {
        let dt = if dt.is_finite() {
            dt.clamp(0.0, 1.0)
        } else {
            0.0
        };

        // An unfinished repo loop is an intention, not another noisy utility
        // roll. Its phase picks a stable guard immediately, while genuine
        // exhaustion still wins so the homeostatic sleep loop can close.
        if vigil.is_active() {
            let guard = match vigil {
                RepoVigil::Failure => AmbientBehavior::GuardFailure,
                RepoVigil::Stuck => AmbientBehavior::GuardStuck,
                RepoVigil::Recovery => AmbientBehavior::GuardRecovery,
                RepoVigil::CautiousRecovery => AmbientBehavior::GuardCautious,
                RepoVigil::None => unreachable!("an active vigil has a guard pose"),
            };
            let sleeping_through_vigil =
                self.current == AmbientBehavior::Sleep && state.energy < REPO_VIGIL_WAKE_ENERGY;
            let next = if state.energy < FORCED_REST_ENERGY || sleeping_through_vigil {
                AmbientBehavior::Sleep
            } else {
                guard
            };
            if self.current != next {
                self.current = next;
                self.hold_for = self.current.hold_secs();
            } else {
                self.hold_for = (self.hold_for - dt).max(0.0);
            }
            return self.current;
        }
        if self.current.is_repo_vigil() {
            self.current = AmbientBehavior::Idle;
            self.hold_for = 0.0;
        }
        self.hold_for -= dt;
        if self.hold_for > 0.0 {
            return self.current;
        }
        if state.energy < FORCED_REST_ENERGY {
            self.current = AmbientBehavior::Sleep;
        } else {
            let idle_for = if idle_for.is_finite() {
                idle_for.max(0.0)
            } else {
                0.0
            };
            let candidates = [
                (AmbientBehavior::Idle, 0.30 + state.mood * 0.10),
                (
                    AmbientBehavior::Sleep,
                    (1.0 - state.energy) * 1.15 + idle_for.min(60.0) / 180.0,
                ),
                (
                    AmbientBehavior::Explore,
                    state.boredom * 0.72 + state.curiosity * 0.30,
                ),
                (
                    AmbientBehavior::Approach,
                    state.social_need * 0.72 + state.attachment * 0.12,
                ),
            ];
            let mut best = (AmbientBehavior::Idle, f32::MIN);
            for (candidate, base) in candidates {
                let inertia = if candidate == self.current { 0.08 } else { 0.0 };
                let score = base + inertia + self.jitter();
                if score > best.1 {
                    best = (candidate, score);
                }
            }
            self.current = best.0;
        }
        self.hold_for = self.current.hold_secs();
        self.current
    }

    /// Deterministic xorshift64* noise in [0, 0.08).
    fn jitter(&mut self) -> f32 {
        let mut x = self.seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.seed = x;
        let unit = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32;
        unit * 0.08
    }
}

/// Select the sticky-header micro-pose for the current animation frame. The
/// cadence is deliberately slow: reacting poses alternate every half second
/// (five 100ms frames), while the idle pose only flicks its tail on one beat
/// in twelve so a quiet header stays quiet. A drowsy mind dozes in the header
/// too.
pub(crate) fn sticky_glyph(behavior: Behavior, language: BodyLanguage, frame: u64) -> &'static str {
    base_sticky_glyph(behavior, language, WatchRhythm::Steady, frame / 5)
}

/// Context-aware sticky form. Semantic transitions intentionally remain a
/// property of the one spatial body; the scrollback header keeps the target
/// behavior legible while sharing growth and output rhythm.
pub(crate) fn sticky_glyph_with_context(context: RenderContext, frame: u64) -> Cow<'static, str> {
    let beat = frame / render_cadence(context);
    let glyph = base_sticky_glyph(
        context.behavior,
        context.body_language,
        context.watch_rhythm,
        beat,
    );
    apply_glyph_growth(glyph, context.growth_stage)
}

fn base_sticky_glyph(
    behavior: Behavior,
    language: BodyLanguage,
    watch_rhythm: WatchRhythm,
    beat: u64,
) -> &'static str {
    // Flat-eared doze, distinct from RestAfterPush's perked-ear rest glyph.
    if behavior == Behavior::Idle && language.drowsy {
        return if beat % 2 == 1 { "=\\_/=" } else { "=\\z/=" };
    }
    let frames = match (behavior, watch_rhythm) {
        (
            Behavior::WatchCommand | Behavior::WatchAgent | Behavior::WatchSettled,
            WatchRhythm::Busy,
        ) => ["/\\>/\\", "/\\</\\"],
        (Behavior::WatchCommand | Behavior::WatchAgent, WatchRhythm::Waiting) => {
            ["/\\-/\\", "/\\_/\\"]
        }
        (Behavior::WatchSettled, WatchRhythm::Waiting) => ["=\\-/=", "=\\_/="],
        (
            Behavior::WatchCommand | Behavior::WatchAgent | Behavior::WatchSettled,
            WatchRhythm::Resumed,
        ) => ["/\\O/\\", "/\\o/\\"],
        _ => behavior.sticky_frames(),
    };
    let alternate = match behavior {
        Behavior::Idle => beat % 12 == 11,
        _ => beat % 2 == 1,
    };
    frames[usize::from(alternate)]
}

fn apply_glyph_growth(glyph: &'static str, growth_stage: VisualGrowthStage) -> Cow<'static, str> {
    if growth_stage == VisualGrowthStage::Adult {
        return Cow::Borrowed(glyph);
    }
    debug_assert!(glyph.is_ascii());
    debug_assert_eq!(glyph.len(), 5);
    let mut evolved = glyph.as_bytes().to_vec();
    match growth_stage {
        VisualGrowthStage::Juvenile => {
            // Rounded outer ears preserve the center behavior mark.
            evolved[0] = b'(';
            evolved[4] = b')';
            if evolved[2] == b'o' {
                evolved[2] = b'O';
            }
        }
        VisualGrowthStage::Seasoned => {
            // A clipped right ear leaves the behavior mark untouched.
            evolved[4] = b'|';
        }
        VisualGrowthStage::Adult => unreachable!("adult glyph returned borrowed above"),
    }
    Cow::Owned(String::from_utf8(evolved).expect("sticky glyphs are fixed ASCII"))
}

/// Coarse, content-free phases of the Shell Agent lifecycle. Only the phase
/// kind ever crosses the organism boundary — never proposals, commands,
/// model output, or error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentPulse {
    /// The session is thinking or running an approved command.
    Working,
    /// A proposal is waiting for the human's review.
    AskingReview,
    /// The task completed (or hit its turn limit).
    Finished,
    /// The session was cancelled or went away.
    Gone,
}

/// Fold one Agent lifecycle phase into the shared life state. The Agent
/// occupying the human's attention slowly feeds the organism's social need,
/// which is what finally gives the Approach disposition a genuine niche:
/// when the Agent leaves, the cat comes looking for its human.
pub(crate) fn agent_pulse(mut state: LifeState, pulse: AgentPulse) -> LifeState {
    match pulse {
        AgentPulse::Working => {
            state.curiosity += 0.03;
            state.social_need += 0.015;
        }
        AgentPulse::AskingReview => {
            state.curiosity += 0.04;
            state.social_need += 0.02;
        }
        AgentPulse::Finished => {
            state.mood += 0.04;
            state.attachment += 0.02;
            state.social_need += 0.03;
        }
        AgentPulse::Gone => {
            state.social_need += 0.05;
        }
    }
    state.clamp();
    state
}

/// Content-free pulse from the command-correction card: the user accepted the
/// proposed fix. Carries only the fact of acceptance — never the command or
/// correction text.
pub(crate) fn correction_accepted(mut state: LifeState) -> LifeState {
    state.confidence += 0.02;
    state.attachment += 0.02;
    state.social_need -= 0.02;
    state.clamp();
    state
}

/// Content-free pulse: the user closed or dismissed a correction card.
/// Repeated dismissals teach the organism to stay quieter — boredom rises and
/// curiosity falls a little more each consecutive time, bounded by clamping.
pub(crate) fn correction_dismissed(mut state: LifeState, consecutive: u32) -> LifeState {
    let weight = consecutive.clamp(1, 4) as f32;
    state.boredom += 0.03 * weight;
    state.curiosity -= 0.02 * weight;
    state.social_need -= 0.01;
    state.clamp();
    state
}

/// How well the persistent memory knows the repository a command runs in,
/// derived from the number of remembered per-day records. Content-free: it
/// carries no path or command data, only a coarse familiarity bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoArrival {
    Unfamiliar,
    Known,
    Home,
}

impl RepoArrival {
    pub(crate) fn from_familiarity(days_remembered: u32) -> Self {
        match days_remembered {
            0 => Self::Unfamiliar,
            1..=6 => Self::Known,
            _ => Self::Home,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Quiet,
    Active,
    Success,
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reaction {
    pub(crate) behavior: Behavior,
    pub(crate) tone: Tone,
    pub(crate) description: String,
    pub(crate) speech: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LifeState {
    pub(crate) energy: f32,
    pub(crate) mood: f32,
    pub(crate) curiosity: f32,
    pub(crate) boredom: f32,
    pub(crate) stress: f32,
    pub(crate) social_need: f32,
    pub(crate) attachment: f32,
    pub(crate) confidence: f32,
}

/// Coarse, content-free relation between local wall time and the organism's
/// learned working-hours profile. `Unlearned` preserves the original energy
/// drift exactly until enough prior days exist to infer a stable rhythm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CircadianPhase {
    #[default]
    Unlearned,
    InHours,
    OffHours,
}

impl Default for LifeState {
    fn default() -> Self {
        Self {
            energy: 0.72,
            mood: 0.62,
            curiosity: 0.68,
            boredom: 0.22,
            stress: 0.14,
            social_need: 0.35,
            attachment: 0.30,
            confidence: 0.58,
        }
    }
}

impl LifeState {
    /// Continuous homeostasis between semantic events, ported from the
    /// prototype life engine (`prototypes/ascii-organism/src/life.rs`). `dt`
    /// is seconds; slices are clamped to [0, 1] and non-finite time is
    /// ignored. `resting` is the native stand-in for the prototype's sleep: a
    /// terminal left quiet long enough lets energy recover. Before a rhythm is
    /// learned, waking work keeps the original slow drain; afterwards energy
    /// eases toward a higher in-hours target and a lower off-hours target.
    /// Drives nothing by itself — event reactions remain authoritative.
    pub(crate) fn tick(
        &mut self,
        dt: f32,
        user_active: bool,
        resting: bool,
        circadian: CircadianPhase,
    ) {
        let dt = if dt.is_finite() {
            dt.clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Exhaustion forces micro-rest even in a busy terminal, mirroring the
        // prototype's forced sleep below this energy level: the mind
        // self-regulates near the floor instead of pinning at zero while it
        // watches a long-lived command.
        let resting = resting || self.energy < FORCED_REST_ENERGY;

        if resting {
            self.energy += 0.030 * dt;
        } else {
            match circadian {
                CircadianPhase::Unlearned => self.energy -= 0.002 * dt,
                CircadianPhase::InHours => {
                    self.energy = move_toward(self.energy, 0.65, 0.002 * dt);
                }
                CircadianPhase::OffHours => {
                    self.energy = move_toward(self.energy, 0.35, 0.002 * dt);
                }
            }
        }

        if user_active {
            self.boredom -= 0.010 * dt;
            self.curiosity += 0.003 * dt;
            self.social_need -= 0.006 * dt;
        } else {
            self.boredom += 0.004 * dt;
            self.social_need += 0.001 * dt;
        }

        self.stress -= 0.003 * dt;
        let target_mood = (self.energy + self.confidence) * 0.5 - self.stress * 0.35;
        self.mood += (target_mood - self.mood) * 0.08 * dt;
        self.clamp();
    }

    fn clamp(&mut self) {
        self.energy = bounded(self.energy);
        self.mood = bounded(self.mood);
        self.curiosity = bounded(self.curiosity);
        self.boredom = bounded(self.boredom);
        self.stress = bounded(self.stress);
        self.social_need = bounded(self.social_need);
        self.attachment = bounded(self.attachment);
        self.confidence = bounded(self.confidence);
    }

    #[cfg(test)]
    fn values(self) -> [f32; 8] {
        [
            self.energy,
            self.mood,
            self.curiosity,
            self.boredom,
            self.stress,
            self.social_need,
            self.attachment,
            self.confidence,
        ]
    }
}

fn move_toward(current: f32, target: f32, max_step: f32) -> f32 {
    if !current.is_finite() || !target.is_finite() || !max_step.is_finite() {
        return 0.5;
    }
    let step = max_step.max(0.0);
    if current < target {
        (current + step).min(target)
    } else {
        (current - step).max(target)
    }
}

fn bounded(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

/// Below this energy the tick rests regardless of terminal activity — the
/// prototype's forced-sleep threshold, keeping exhaustion self-limiting.
const FORCED_REST_ENERGY: f32 = 0.15;
/// Once a repo vigil has actually fallen asleep, let it refill beyond the
/// forced-rest edge before resuming. This hysteresis prevents a one-frame
/// sleep/guard oscillation around [`FORCED_REST_ENERGY`].
const REPO_VIGIL_WAKE_ENERGY: f32 = 0.25;
/// First failure after this many clean passes today reads as a broken streak
/// and reacts with amplified stress instead of the routine inspection.
const SENSITIZATION_CLEAN_RUNS: u32 = 5;
/// Any-command consecutive non-zero exits before the organism visibly wearies.
const ROUGH_STREAK_THRESHOLD: u32 = 3;

#[derive(Debug, Default)]
pub(crate) struct NativeOrganism {
    state: LifeState,
    build_failures: u32,
    active_kind: Option<CommandKind>,
    recovered_build: bool,
    /// Same-day failure→success episodes remembered for this repo. It does not
    /// claim a particular test is flaky; the third flip only makes a pending
    /// recovery visibly cautious until push.
    failure_success_flips: u32,
    /// Today's build/test successes in the active repo context. Habituation:
    /// each additional clean pass lands with 1/(1+prior/4) of the excitement,
    /// where `prior` is this count before the pass is recorded.
    successes_today: u32,
    /// Today's build/test failures in the active repo context, for the
    /// sensitized first-crack-after-a-clean-run reaction.
    failures_today: u32,
    /// Session-local consecutive non-zero exits across every command kind.
    /// Never persisted; the durable memory keeps observing only build/push.
    rough_streak: u32,
    pending_arrival: Option<RepoArrival>,
    /// The active command directly followed an accepted correction card.
    assisted: bool,
    /// The active command was submitted by the Shell Agent, not typed by the
    /// human. Content-free: only the fact, never the proposal or command.
    agent_driven: bool,
}

impl NativeOrganism {
    pub(crate) fn from_persisted_state(mut state: LifeState) -> Self {
        state.clamp();
        Self {
            state,
            ..Self::default()
        }
    }

    pub(crate) fn state(&self) -> LifeState {
        self.state
    }

    /// Whether this repo/day has a recovered build that memory has not yet
    /// seen followed by a successful push. This is a content-free intention:
    /// it carries neither the command nor repository identity.
    pub(crate) fn guarding_recovery(&self) -> bool {
        self.recovered_build
    }

    pub(crate) fn repo_work_state(&self) -> RepoWorkState {
        RepoWorkState::new(
            self.build_failures,
            self.recovered_build,
            self.failure_success_flips,
        )
    }

    pub(crate) fn repo_vigil(&self) -> RepoVigil {
        self.repo_work_state().vigil()
    }

    /// Reconcile the pane-local intention with the memory layer's ordered
    /// repo/day replay. Returns whether any visible work fact changed.
    pub(crate) fn sync_repo_work_state(&mut self, work: RepoWorkState) -> bool {
        let work = RepoWorkState::new(
            work.open_failures,
            work.recovered_pending_push,
            work.failure_success_flips,
        );
        if self.repo_work_state() == work {
            return false;
        }
        self.build_failures = work.open_failures;
        self.recovered_build = work.recovered_pending_push;
        self.failure_success_flips = work.failure_success_flips;
        true
    }

    /// Restore the unfinished build streak for the exact repo/day selected by
    /// the memory layer. Switching repositories calls this again, so failures
    /// can never leak into another checkout's celebration level.
    pub(crate) fn restore_build_failures(&mut self, failures: u32) {
        self.build_failures = failures;
        self.recovered_build = false;
        self.failure_success_flips = 0;
    }

    pub(crate) fn restore_repo_context(
        &mut self,
        failures: u32,
        recovered_build: bool,
        successes_today: u32,
        failures_today: u32,
    ) {
        self.restore_repo_work_context(
            RepoWorkState::new(failures, recovered_build, 0),
            successes_today,
            failures_today,
        );
    }

    pub(crate) fn restore_repo_work_context(
        &mut self,
        work: RepoWorkState,
        successes_today: u32,
        failures_today: u32,
    ) {
        self.sync_repo_work_state(work);
        self.successes_today = successes_today;
        self.failures_today = failures_today;
    }

    /// Note that the next command runs in a repository the memory layer just
    /// switched to. Consumed by the next `command_started` reduction.
    pub(crate) fn note_repo_arrival(&mut self, arrival: RepoArrival) {
        self.pending_arrival = Some(arrival);
    }

    /// The pane genuinely left the repository before a queued human greeting
    /// could be consumed (for example, an Agent entered it first). Never let
    /// that repo-specific attachment leak into an unrelated cwd.
    pub(crate) fn clear_repo_arrival(&mut self) {
        self.pending_arrival = None;
    }

    /// Note that the active command directly followed an accepted correction
    /// card. Consumed by the next `command_finished` reduction.
    pub(crate) fn note_assisted_command(&mut self) {
        self.assisted = true;
    }

    /// Record whether the active command was submitted by the Shell Agent.
    /// Set unconditionally at every command start (so a stale flag can never
    /// outlive a lost command), consumed by the next `command_finished`
    /// reduction: the organism watches from a little apart and keeps its big
    /// celebrations — and its debugging empathy — for commands the human
    /// typed themself.
    pub(crate) fn set_agent_command(&mut self, agent_driven: bool) {
        self.agent_driven = agent_driven;
    }

    /// The Agent's approved command ended without its authoritative end
    /// marker. React with restrained caution, never with celebration.
    pub(crate) fn agent_execution_lost(&mut self) -> Reaction {
        self.agent_driven = false;
        self.active_kind = None;
        self.state.curiosity += 0.04;
        self.state.stress += 0.05;
        self.state.clamp();
        Reaction {
            behavior: Behavior::UnknownOutcome,
            tone: Tone::Warning,
            description: "the Agent's command ended without its end marker".to_string(),
            speech: None,
        }
    }

    /// A local calendar boundary passed while this pane stayed alive. Today's
    /// habituation/sensitization counters and unfinished repo intentions
    /// restart; repo-backed contexts are re-seeded from the day-scoped memory
    /// on the next build anyway.
    pub(crate) fn roll_over_day(&mut self) {
        self.build_failures = 0;
        self.recovered_build = false;
        self.failure_success_flips = 0;
        self.successes_today = 0;
        self.failures_today = 0;
    }

    /// Pull the latest window-shared continuous state into this pane-local
    /// behavior context before reducing an event.
    pub(crate) fn sync_state(&mut self, mut state: LifeState) {
        state.clamp();
        self.state = state;
    }

    pub(crate) fn idle_reaction(&self) -> Reaction {
        match self.repo_vigil() {
            RepoVigil::Failure => Reaction {
                behavior: Behavior::GuardFailure,
                tone: Tone::Quiet,
                description: format!(
                    "keeping build failure {} in sight · waiting for another build/test",
                    self.build_failures
                ),
                speech: None,
            },
            RepoVigil::Stuck => Reaction {
                behavior: Behavior::GuardStuck,
                // More failures lower the posture; they do not make the
                // durable card louder than the first unresolved failure.
                tone: Tone::Quiet,
                description: format!(
                    "quiet beside {} unresolved build failures · waiting without nagging",
                    self.build_failures
                ),
                speech: None,
            },
            RepoVigil::Recovery => Reaction {
                behavior: Behavior::GuardRecovery,
                tone: Tone::Quiet,
                description: "keeping watch over recovered work · waiting for git push".to_string(),
                speech: None,
            },
            RepoVigil::CautiousRecovery => Reaction {
                behavior: Behavior::GuardCautious,
                tone: Tone::Warning,
                description:
                    "recovered work has flipped repeatedly today · checking once more before git push"
                        .to_string(),
                speech: None,
            },
            RepoVigil::None => Reaction {
                behavior: Behavior::Idle,
                tone: Tone::Quiet,
                description: "quiet · waiting for a real Block event".to_string(),
                speech: None,
            },
        }
    }

    pub(crate) fn command_started(&mut self, kind: CommandKind) -> Reaction {
        self.active_kind = Some(kind);
        self.state.energy -= 0.01;
        self.state.curiosity += if kind == CommandKind::BuildOrTest {
            0.10
        } else {
            0.04
        };
        self.state.boredom -= 0.08;

        if self.agent_driven {
            // Crouch a little apart: the Agent is working, not the human. A
            // pending repo greeting stays queued for the human's own first
            // command instead of being spent on the Agent's.
            self.state.clamp();
            return Reaction {
                behavior: Behavior::WatchAgent,
                tone: Tone::Quiet,
                description: format!("watching the Agent run a {} command", kind.label()),
                speech: None,
            };
        }

        let arrival = self.pending_arrival.take();
        match arrival {
            Some(RepoArrival::Unfamiliar) => {
                // Shy in a checkout it has never remembered: less sure of
                // itself, more curious, and deliberately quiet.
                self.state.confidence -= 0.06;
                self.state.attachment -= 0.02;
                self.state.stress += 0.03;
                self.state.curiosity += 0.08;
            }
            Some(RepoArrival::Known) => self.state.attachment += 0.02,
            Some(RepoArrival::Home) => {
                self.state.attachment += 0.05;
                self.state.mood += 0.03;
                self.state.social_need -= 0.02;
            }
            None => {}
        }
        self.state.clamp();
        match arrival {
            Some(RepoArrival::Unfamiliar) => Reaction {
                behavior: Behavior::WatchCommand,
                tone: Tone::Quiet,
                description: format!(
                    "watching real {} event · first day in this repo",
                    kind.label()
                ),
                speech: None,
            },
            Some(RepoArrival::Home) => Reaction {
                behavior: Behavior::WatchCommand,
                tone: Tone::Active,
                description: format!("watching real {} event · well-known repo", kind.label()),
                speech: Some("回来了。"),
            },
            _ => Reaction {
                behavior: Behavior::WatchCommand,
                tone: Tone::Active,
                description: format!("watching real {} event", kind.label()),
                speech: None,
            },
        }
    }

    pub(crate) fn command_finished(
        &mut self,
        classified: CommandKind,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
    ) -> Reaction {
        let kind = if classified == CommandKind::Other {
            self.active_kind.unwrap_or(classified)
        } else {
            classified
        };
        self.active_kind = None;
        let assisted = std::mem::take(&mut self.assisted);
        let agent_driven = std::mem::take(&mut self.agent_driven);

        let duration = duration_label(duration_ms);
        let Some(exit_code) = exit_code else {
            self.state.curiosity += 0.03;
            self.state.clamp();
            return Reaction {
                behavior: Behavior::UnknownOutcome,
                tone: Tone::Warning,
                description: format!("{} finished · status unknown{duration}", kind.label()),
                speech: None,
            };
        };

        if exit_code != 0 {
            self.rough_streak = self.rough_streak.saturating_add(1);
            // The first crack after a clean run of passes stings more than one
            // more failure in an already rough day. An Agent's failure is the
            // Agent's problem: softer stress, and the human's confidence and
            // clean-run pride are untouched.
            let sensitized = !agent_driven
                && kind == CommandKind::BuildOrTest
                && self.failures_today == 0
                && self.successes_today >= SENSITIZATION_CLEAN_RUNS;
            self.state.mood -= if agent_driven { 0.04 } else { 0.08 };
            self.state.stress += if agent_driven {
                0.06
            } else if sensitized {
                0.20
            } else {
                0.12
            };
            if !agent_driven {
                self.state.confidence -= if sensitized { 0.06 } else { 0.04 };
            }
            self.state.curiosity += 0.05;
            self.state.clamp();

            let failures = if kind == CommandKind::BuildOrTest {
                self.failures_today = self.failures_today.saturating_add(1);
                self.build_failures = self.build_failures.saturating_add(1);
                self.recovered_build = false;
                self.build_failures
            } else {
                0
            };
            let rough = failures == 0 && self.rough_streak >= ROUGH_STREAK_THRESHOLD;
            let mut description = if sensitized {
                format!(
                    "exit {exit_code}{duration} · first crack after {} clean run(s)",
                    self.successes_today
                )
            } else if rough {
                format!(
                    "exit {exit_code}{duration} · {} rough commands in a row",
                    self.rough_streak
                )
            } else if failures == 0 {
                format!("exit {exit_code}{duration} · inspecting the finished Block")
            } else {
                format!("exit {exit_code}{duration} · build failure {failures}")
            };
            if agent_driven {
                description.push_str(" · agent-driven");
            }
            return Reaction {
                behavior: if failures >= 2 || rough {
                    Behavior::SitNearError
                } else {
                    Behavior::InspectError
                },
                tone: Tone::Error,
                description,
                speech: if agent_driven {
                    // The human is not debugging yet; pointing would nag.
                    None
                } else if failures > 0 {
                    (failures <= 1).then_some("这里。")
                } else {
                    // Speak up on the first stumble; a continuing rough streak
                    // sits nearby in silence instead of nagging.
                    (self.rough_streak <= 1).then_some("这里。")
                },
            };
        }

        self.rough_streak = 0;
        if assisted {
            self.state.confidence += 0.04;
            self.state.attachment += 0.03;
        }
        match kind {
            CommandKind::BuildOrTest => {
                let failures = std::mem::take(&mut self.build_failures);
                if failures > 0 {
                    self.failure_success_flips = self.failure_success_flips.saturating_add(1);
                }
                // A later clean build must not forget an earlier recovery that
                // is still waiting for push. Memory preserves this flag until
                // a new build failure or a successful push; mirror it here.
                self.recovered_build |= failures > 0;
                // Habituation: excitement scales by 1/(1+prior/4), where
                // `prior` counts the clean passes already seen today, so the
                // day's first pass lands at full strength. Recoveries always
                // keep full strength — big celebrations stay reserved for
                // genuinely rare moments.
                let habituation = if failures == 0 {
                    self.successes_today
                } else {
                    0
                };
                let damp = 1.0 / (1.0 + habituation as f32 / 4.0);
                self.successes_today = self.successes_today.saturating_add(1);
                // An Agent-driven pass earns half the glow and none of the
                // human's confidence — they didn't type it.
                let ownership = if agent_driven { 0.5 } else { 1.0 };
                self.state.mood += (0.10 + failures.min(5) as f32 * 0.025) * damp * ownership;
                self.state.stress -= 0.12;
                self.state.confidence += 0.08 * damp * ownership;
                self.state.attachment += 0.025;
                self.state.clamp();
                let (behavior, tone, speech) = if agent_driven {
                    // A quiet nod: big celebrations and words stay reserved
                    // for commands the human typed themself.
                    (Behavior::Celebrate, Tone::Success, None)
                } else if failures == 0 {
                    match habituation {
                        0..=2 => (Behavior::Celebrate, Tone::Success, Some("过了。")),
                        3..=5 => (Behavior::Celebrate, Tone::Success, None),
                        _ => (Behavior::Celebrate, Tone::Quiet, None),
                    }
                } else if failures <= 2 {
                    (Behavior::Celebrate, Tone::Success, Some("好了。"))
                } else {
                    (Behavior::CelebrateBig, Tone::Success, Some("终于。"))
                };
                let mut description = if failures == 0 && habituation >= 3 {
                    format!(
                        "build/test passed{duration} · pass {} today",
                        habituation.saturating_add(1)
                    )
                } else {
                    format!("build/test passed after {failures} failure(s){duration}")
                };
                if agent_driven {
                    description.push_str(" · agent-driven");
                }
                Reaction {
                    behavior,
                    tone,
                    description,
                    speech,
                }
            }
            CommandKind::GitPush => {
                self.state.energy -= 0.02;
                self.state.mood += 0.06;
                self.state.stress -= 0.08;
                self.state.attachment += if agent_driven { 0.02 } else { 0.04 };
                self.state.clamp();
                let recovered = std::mem::take(&mut self.recovered_build);
                Reaction {
                    behavior: Behavior::RestAfterPush,
                    tone: Tone::Success,
                    description: if agent_driven {
                        format!("git push completed{duration} · agent-driven")
                    } else {
                        format!("git push completed{duration}")
                    },
                    speech: (recovered && !agent_driven).then_some("收好了。"),
                }
            }
            CommandKind::Other => {
                self.state.mood += 0.01;
                self.state.stress -= 0.02;
                self.state.boredom -= 0.02;
                self.state.clamp();
                if assisted {
                    // Acknowledge accepted help with a small nod, never a big
                    // celebration and never a word.
                    Reaction {
                        behavior: Behavior::Celebrate,
                        tone: Tone::Success,
                        description: format!("corrected command worked{duration}"),
                        speech: None,
                    }
                } else {
                    Reaction {
                        behavior: Behavior::Idle,
                        tone: Tone::Quiet,
                        description: format!("command finished cleanly{duration}"),
                        speech: None,
                    }
                }
            }
        }
    }
}

fn duration_label(duration_ms: Option<u64>) -> String {
    match duration_ms {
        Some(ms) if ms >= 1_000 => format!(" · {:.1}s", ms as f64 / 1_000.0),
        Some(ms) => format!(" · {ms}ms"),
        None => String::new(),
    }
}

pub(crate) fn classify_command(command: &str) -> CommandKind {
    const MAX_CLASSIFIER_TOKENS: usize = 16;
    const MAX_WRAPPER_DEPTH: usize = 8;

    let mut raw_tokens = command.split_whitespace();
    let bounded_tokens = raw_tokens
        .by_ref()
        .take(MAX_CLASSIFIER_TOKENS)
        .map(normalize_token)
        .collect::<Vec<_>>();
    // Classification stays bounded, but a truncated push can hide a trailing
    // `--dry-run`. Such a command must fail closed instead of publishing the
    // recovery intention on incomplete evidence.
    let truncated = raw_tokens.next().is_some();
    let mut tokens = bounded_tokens
        .into_iter()
        .filter(|token| !token.is_empty())
        .peekable();

    while tokens
        .peek()
        .is_some_and(|token| is_environment_assignment(token))
    {
        tokens.next();
    }

    let mut program = tokens.next().unwrap_or_default();
    for _ in 0..MAX_WRAPPER_DEPTH {
        let wrapper = program.rsplit('/').next().unwrap_or(program.as_str());
        match wrapper {
            "command" | "builtin" => skip_wrapper_options(&mut tokens, &[]),
            "exec" => skip_wrapper_options(&mut tokens, &["-a", "--argv0"]),
            "env" => {
                skip_wrapper_options(
                    &mut tokens,
                    &["-u", "--unset", "-c", "--chdir", "-s", "--split-string"],
                );
                while tokens
                    .peek()
                    .is_some_and(|token| is_environment_assignment(token))
                {
                    tokens.next();
                }
            }
            "sudo" => skip_wrapper_options(
                &mut tokens,
                &[
                    "-u",
                    "--user",
                    "-g",
                    "--group",
                    "-h",
                    "--host",
                    "-p",
                    "--prompt",
                    "-c",
                    "--close-from",
                    "-r",
                    "--chroot",
                    "-t",
                    "--command-timeout",
                    "--role",
                    "--type",
                ],
            ),
            "time" => skip_wrapper_options(&mut tokens, &["-f", "--format", "-o", "--output"]),
            "nohup" => skip_wrapper_options(&mut tokens, &[]),
            _ => break,
        }
        program = tokens.next().unwrap_or_default();
    }

    let program = program.rsplit('/').next().unwrap_or(program.as_str());
    let args: Vec<String> = tokens.collect();
    match program {
        "git"
            if git_push_args(&args)
                .is_some_and(|push_args| !truncated && !git_push_is_dry_run(push_args)) =>
        {
            CommandKind::GitPush
        }
        "cargo"
            if args.first().is_some_and(|arg| {
                matches!(arg.as_str(), "build" | "check" | "clippy" | "test")
            }) || matches!(args.as_slice(), [nextest, run, ..] if nextest == "nextest" && run == "run") =>
        {
            CommandKind::BuildOrTest
        }
        "make" | "ninja" | "pytest" | "ctest" => CommandKind::BuildOrTest,
        "go" if args.first().is_some_and(|arg| arg == "test") => CommandKind::BuildOrTest,
        "cmake" if args.first().is_some_and(|arg| arg == "--build") => CommandKind::BuildOrTest,
        "npm" | "pnpm" | "yarn"
            if args.first().is_some_and(|arg| arg == "test")
                || args
                    .windows(2)
                    .any(|pair| pair[0] == "run" && pair[1] == "test") =>
        {
            CommandKind::BuildOrTest
        }
        _ => CommandKind::Other,
    }
}

/// Find a real `push` after a bounded, identity-preserving Git global-option
/// prefix. Options such as `-C`, `--git-dir`, and `--work-tree` deliberately
/// fail closed: the UI resolved repo identity from the terminal cwd, so a Git
/// invocation that redirects discovery must not close that repo's work loop.
fn git_push_args(args: &[String]) -> Option<&[String]> {
    let mut index = 0;
    while let Some(argument) = args.get(index) {
        match argument.as_str() {
            "push" => return Some(&args[index..]),
            "--no-pager"
            | "--paginate"
            | "-p"
            | "--no-replace-objects"
            | "--no-optional-locks"
            | "--no-lazy-fetch"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs" => index += 1,
            "-c" | "--config-env" => {
                args.get(index + 1)?;
                index += 2;
            }
            argument
                if (argument.starts_with("-c") && argument.len() > 2)
                    || argument.starts_with("--config-env=") =>
            {
                index += 1;
            }
            _ => return None,
        }
    }
    None
}

/// Consume one wrapper's bounded option prefix. Exact options in
/// `takes_value` consume the following token; attached/`--name=value` forms
/// are already self-contained. `--` ends wrapper option parsing.
fn skip_wrapper_options<I>(tokens: &mut std::iter::Peekable<I>, takes_value: &[&str])
where
    I: Iterator<Item = String>,
{
    while let Some(option) = tokens.peek() {
        if option == "--" {
            tokens.next();
            break;
        }
        if option == "-" || !option.starts_with('-') {
            break;
        }
        let consumes_value = takes_value.contains(&option.as_str());
        tokens.next();
        if consumes_value {
            tokens.next();
        }
    }
}

/// A dry-run proves reachability but does not close a recovered-work loop.
/// Parse only the bounded normalized argument prefix and stop recognizing
/// options after `--`, matching Git's usual option boundary.
fn git_push_is_dry_run(args: &[String]) -> bool {
    let mut options = true;
    for argument in args.iter().skip(1) {
        if argument == "--" {
            options = false;
            continue;
        }
        if !options {
            continue;
        }
        if argument == "--dry-run"
            || argument.starts_with("--dry-run=")
            || (argument.starts_with('-')
                && !argument.starts_with("--")
                && argument[1..].contains('n'))
        {
            return true;
        }
    }
    false
}

fn normalize_token(token: &str) -> String {
    token
        .trim_matches(|character| matches!(character, '\'' | '"'))
        .chars()
        .take(96)
        .collect()
}

fn is_environment_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_real_commands_without_treating_every_success_as_a_build() {
        assert_eq!(
            classify_command("cargo test --all"),
            CommandKind::BuildOrTest
        );
        assert_eq!(
            classify_command("MODE=release env RUST_LOG=info cargo check"),
            CommandKind::BuildOrTest
        );
        assert_eq!(
            classify_command("sudo -n /usr/bin/git push"),
            CommandKind::GitPush
        );
        for command in [
            "git --no-pager push",
            "git -c color.ui=false push",
            "git -ccolor.ui=false push",
        ] {
            assert_eq!(
                classify_command(command),
                CommandKind::GitPush,
                "{command} keeps cwd-derived repo identity"
            );
        }
        for command in [
            "git --no-pager push --dry-run",
            "git -C /tmp push",
            "git -C/tmp push",
            "git --git-dir=/tmp/repo.git push",
            "git --work-tree /tmp push",
        ] {
            assert_eq!(
                classify_command(command),
                CommandKind::Other,
                "{command} must not close cwd-derived recovered work"
            );
        }
        assert_eq!(
            classify_command("time -p cargo nextest run"),
            CommandKind::BuildOrTest
        );
        assert_eq!(
            classify_command("nohup cargo test"),
            CommandKind::BuildOrTest
        );
        assert_eq!(
            classify_command("time sudo -n cargo test"),
            CommandKind::BuildOrTest
        );
        assert_eq!(
            classify_command("nohup env FORGE_TEST=1 cargo nextest run"),
            CommandKind::BuildOrTest
        );
        assert_eq!(
            classify_command("env -u OLD sudo -u root time -p cargo check"),
            CommandKind::BuildOrTest
        );
        for command in [
            "git push --dry-run",
            "git push origin main -n",
            "git push -vn origin main",
        ] {
            assert_eq!(
                classify_command(command),
                CommandKind::Other,
                "{command} must not claim to have published recovered work"
            );
        }
        assert_eq!(
            classify_command("git push -- --dry-run"),
            CommandKind::GitPush,
            "an option-shaped refspec after -- is not a dry-run flag"
        );
        let hidden_dry_run = format!(
            "git push {} --dry-run",
            (0..14)
                .map(|index| format!("--push-option=guard-{index}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert_eq!(
            classify_command(&hidden_dry_run),
            CommandKind::Other,
            "a bounded classifier must not publish when later options were truncated"
        );
        assert_eq!(classify_command("printf done"), CommandKind::Other);
    }

    #[test]
    fn repeated_real_failures_escalate_then_success_celebrates() {
        let mut organism = NativeOrganism::default();
        organism.command_started(CommandKind::BuildOrTest);
        let first = organism.command_finished(CommandKind::BuildOrTest, Some(101), Some(900));
        organism.command_started(CommandKind::BuildOrTest);
        let second = organism.command_finished(CommandKind::BuildOrTest, Some(101), Some(800));
        organism.command_started(CommandKind::BuildOrTest);
        let third = organism.command_finished(CommandKind::BuildOrTest, Some(101), Some(700));
        organism.command_started(CommandKind::BuildOrTest);
        let success = organism.command_finished(CommandKind::BuildOrTest, Some(0), Some(600));

        assert_eq!(first.behavior, Behavior::InspectError);
        assert_eq!(second.behavior, Behavior::SitNearError);
        assert_eq!(third.behavior, Behavior::SitNearError);
        assert_eq!(success.behavior, Behavior::CelebrateBig);
        assert_eq!(success.speech, Some("终于。"));
    }

    #[test]
    fn unknown_exit_status_is_never_presented_as_success() {
        let mut organism = NativeOrganism::default();
        organism.command_started(CommandKind::BuildOrTest);
        let reaction = organism.command_finished(CommandKind::BuildOrTest, None, None);
        assert_eq!(reaction.behavior, Behavior::UnknownOutcome);
        assert_eq!(reaction.tone, Tone::Warning);
        assert_eq!(reaction.speech, None);
    }

    #[test]
    fn unrelated_success_does_not_erase_a_build_debugging_streak() {
        let mut organism = NativeOrganism::default();
        organism.command_finished(CommandKind::BuildOrTest, Some(1), None);
        organism.command_finished(CommandKind::Other, Some(0), None);
        let success = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(success.speech, Some("好了。"));
    }

    #[test]
    fn every_state_dimension_stays_finite_and_bounded() {
        let mut organism = NativeOrganism::default();
        for index in 0..10_000 {
            organism.command_started(CommandKind::BuildOrTest);
            let status = if index % 3 == 0 { 0 } else { 101 };
            organism.command_finished(CommandKind::BuildOrTest, Some(status), Some(index));
        }
        assert!(organism
            .state()
            .values()
            .into_iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(&value)));
    }

    #[test]
    fn tick_drains_waking_energy_and_restores_it_at_rest() {
        let mut waking = LifeState::default();
        let mut resting = LifeState::default();
        for _ in 0..120 {
            waking.tick(1.0, false, false, CircadianPhase::Unlearned);
            resting.tick(1.0, false, true, CircadianPhase::Unlearned);
        }
        assert!(waking.energy < LifeState::default().energy);
        assert!(resting.energy > waking.energy);
        assert_eq!(resting.energy, 1.0);
    }

    #[test]
    fn learned_hours_shape_energy_without_jumping_at_boundaries() {
        let mut in_hours = LifeState {
            energy: 0.50,
            ..LifeState::default()
        };
        let mut off_hours = in_hours;
        in_hours.tick(1.0, false, false, CircadianPhase::InHours);
        off_hours.tick(1.0, false, false, CircadianPhase::OffHours);
        assert_eq!(in_hours.energy.to_bits(), 0.502_f32.to_bits());
        assert_eq!(off_hours.energy.to_bits(), 0.498_f32.to_bits());

        for _ in 0..500 {
            in_hours.tick(1.0, false, false, CircadianPhase::InHours);
            off_hours.tick(1.0, false, false, CircadianPhase::OffHours);
        }
        assert_eq!(in_hours.energy, 0.65);
        assert_eq!(off_hours.energy, 0.35);

        let mut resting_in = LifeState {
            energy: 0.50,
            ..LifeState::default()
        };
        let mut resting_off = resting_in;
        resting_in.tick(1.0, false, true, CircadianPhase::InHours);
        resting_off.tick(1.0, false, true, CircadianPhase::OffHours);
        assert_eq!(resting_in.energy, resting_off.energy);
        assert!(resting_in.energy > 0.50);
    }

    #[test]
    fn unlearned_circadian_phase_preserves_the_original_energy_drift() {
        let mut state = LifeState::default();
        let before = state.energy;
        state.tick(1.0, false, false, CircadianPhase::Unlearned);
        assert_eq!(state.energy.to_bits(), (before - 0.002).to_bits());
    }

    #[test]
    fn exhaustion_forces_micro_rest_so_energy_never_pins_at_zero() {
        let mut state = LifeState::default();
        for _ in 0..3_600 {
            state.tick(1.0, false, false, CircadianPhase::Unlearned);
        }
        assert!(state.energy > 0.10);
        assert!(state.energy < 0.30);
    }

    #[test]
    fn a_single_time_slice_simulates_at_most_one_second() {
        let mut long_slice = LifeState::default();
        let mut capped = LifeState::default();
        long_slice.tick(3600.0, true, false, CircadianPhase::Unlearned);
        capped.tick(1.0, true, false, CircadianPhase::Unlearned);
        assert_eq!(long_slice.values(), capped.values());
    }

    #[test]
    fn tick_moves_boredom_and_social_need_with_user_activity() {
        let mut engaged = LifeState::default();
        let mut ignored = LifeState::default();
        for _ in 0..60 {
            engaged.tick(1.0, true, false, CircadianPhase::Unlearned);
            ignored.tick(1.0, false, false, CircadianPhase::Unlearned);
        }
        assert!(engaged.boredom < ignored.boredom);
        assert!(engaged.social_need < ignored.social_need);
        assert!(engaged.curiosity > ignored.curiosity);
    }

    #[test]
    fn tick_eases_mood_toward_its_homeostatic_target() {
        let mut stressed = LifeState {
            stress: 1.0,
            mood: 0.9,
            ..LifeState::default()
        };
        let before = stressed.mood;
        for _ in 0..30 {
            stressed.tick(1.0, false, false, CircadianPhase::Unlearned);
        }
        assert!(stressed.mood < before);
    }

    #[test]
    fn tick_survives_hostile_time_slices_and_stays_bounded() {
        let mut state = LifeState::default();
        for (index, dt) in [f32::NAN, f32::INFINITY, -5.0, 3600.0, 0.1]
            .into_iter()
            .cycle()
            .take(10_000)
            .enumerate()
        {
            state.tick(
                dt,
                index % 2 == 0,
                index % 3 == 0,
                CircadianPhase::Unlearned,
            );
        }
        assert!(state
            .values()
            .into_iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(&value)));
    }

    #[test]
    fn clean_passes_habituate_but_recoveries_celebrate_at_full_strength() {
        let mut organism = NativeOrganism::default();
        let first = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(first.speech, Some("过了。"));
        assert_eq!(first.tone, Tone::Success);

        organism.restore_repo_context(0, false, 4, 0);
        let fifth = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(fifth.speech, None);
        assert_eq!(fifth.tone, Tone::Success);

        organism.restore_repo_context(0, false, 9, 0);
        let tenth = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(tenth.behavior, Behavior::Celebrate);
        assert_eq!(tenth.tone, Tone::Quiet);
        assert_eq!(tenth.speech, None);
        assert!(tenth.description.contains("pass 10 today"));

        // A recovery after real failures is never dampened by today's count.
        organism.restore_repo_context(3, false, 9, 3);
        let recovery = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(recovery.behavior, Behavior::CelebrateBig);
        assert_eq!(recovery.speech, Some("终于。"));
    }

    #[test]
    fn state_increments_shrink_as_the_day_of_passes_grows() {
        let mut fresh = NativeOrganism::default();
        let fresh_before = fresh.state().mood;
        fresh.command_finished(CommandKind::BuildOrTest, Some(0), None);
        let fresh_delta = fresh.state().mood - fresh_before;

        let mut jaded = NativeOrganism::default();
        jaded.restore_repo_context(0, false, 8, 0);
        let jaded_before = jaded.state().mood;
        jaded.command_finished(CommandKind::BuildOrTest, Some(0), None);
        let jaded_delta = jaded.state().mood - jaded_before;
        assert!(jaded_delta < fresh_delta);
    }

    #[test]
    fn first_failure_after_a_clean_run_is_sensitized() {
        let mut organism = NativeOrganism::default();
        organism.restore_repo_context(0, false, 5, 0);
        let stress_before = organism.state().stress;
        let reaction = organism.command_finished(CommandKind::BuildOrTest, Some(101), None);
        assert!(reaction
            .description
            .contains("first crack after 5 clean run(s)"));
        assert_eq!(reaction.speech, Some("这里。"));
        assert!(organism.state().stress - stress_before > 0.15);

        let ordinary = organism.command_finished(CommandKind::BuildOrTest, Some(101), None);
        assert!(!ordinary.description.contains("first crack"));
    }

    #[test]
    fn any_command_streak_wearies_and_any_success_clears_it() {
        let mut organism = NativeOrganism::default();
        let first = organism.command_finished(CommandKind::Other, Some(255), None);
        assert_eq!(first.behavior, Behavior::InspectError);
        assert_eq!(first.speech, Some("这里。"));
        let second = organism.command_finished(CommandKind::Other, Some(255), None);
        assert_eq!(second.behavior, Behavior::InspectError);
        assert_eq!(second.speech, None);
        let third = organism.command_finished(CommandKind::Other, Some(255), None);
        assert_eq!(third.behavior, Behavior::SitNearError);
        assert!(third.description.contains("3 rough commands in a row"));
        assert_eq!(third.speech, None);

        organism.command_finished(CommandKind::Other, Some(0), None);
        let after_reset = organism.command_finished(CommandKind::Other, Some(255), None);
        assert_eq!(after_reset.behavior, Behavior::InspectError);
        assert_eq!(after_reset.speech, Some("这里。"));
    }

    #[test]
    fn unknown_exit_neither_extends_nor_clears_the_rough_streak() {
        let mut organism = NativeOrganism::default();
        organism.command_finished(CommandKind::Other, Some(255), None);
        organism.command_finished(CommandKind::Other, Some(255), None);
        organism.command_finished(CommandKind::Other, None, None);
        let third = organism.command_finished(CommandKind::Other, Some(255), None);
        assert_eq!(third.behavior, Behavior::SitNearError);
        assert!(third.description.contains("3 rough commands in a row"));
    }

    #[test]
    fn context_reset_and_day_rollover_restart_todays_rhythm() {
        let mut organism = NativeOrganism::default();
        organism.restore_repo_context(0, false, 9, 2);
        organism.restore_repo_context(0, false, 0, 0);
        let pass = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(pass.speech, Some("过了。"));

        let mut overnight = NativeOrganism::default();
        overnight.restore_repo_context(0, false, 9, 2);
        overnight.roll_over_day();
        let fresh = overnight.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(fresh.speech, Some("过了。"));
        assert_eq!(fresh.tone, Tone::Success);
    }

    #[test]
    fn recovered_work_is_guarded_until_successful_push_or_context_boundary() {
        let mut organism = NativeOrganism::default();
        organism.restore_repo_context(2, false, 0, 2);
        organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert!(organism.guarding_recovery());
        assert_eq!(organism.idle_reaction().behavior, Behavior::GuardRecovery);

        // Rechecking green work and unrelated commands do not erase the open
        // loop. A failed push does not close it either.
        organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        organism.command_finished(CommandKind::Other, Some(0), None);
        organism.command_finished(CommandKind::GitPush, Some(1), None);
        assert!(organism.guarding_recovery());

        let pushed = organism.command_finished(CommandKind::GitPush, Some(0), None);
        assert_eq!(pushed.speech, Some("收好了。"));
        assert!(!organism.guarding_recovery());
        assert_eq!(organism.idle_reaction().behavior, Behavior::Idle);

        organism.restore_repo_context(0, true, 1, 1);
        organism.roll_over_day();
        assert!(!organism.guarding_recovery());
        organism.restore_repo_context(0, true, 1, 1);
        organism.restore_repo_context(0, false, 0, 0);
        assert!(!organism.guarding_recovery());
    }

    #[test]
    fn unresolved_builds_progress_from_failure_to_stuck_then_recovery() {
        let mut organism = NativeOrganism::default();

        for expected in [Behavior::GuardFailure, Behavior::GuardFailure] {
            organism.command_finished(CommandKind::BuildOrTest, Some(1), None);
            assert_eq!(organism.idle_reaction().behavior, expected);
        }
        organism.command_finished(CommandKind::BuildOrTest, Some(1), None);
        let stuck = organism.idle_reaction();
        assert_eq!(stuck.behavior, Behavior::GuardStuck);
        assert_eq!(stuck.tone, Tone::Quiet);

        // Unknown outcomes, unrelated commands, and a failed push cannot
        // erase a repo/day debugging loop they did not resolve.
        organism.command_finished(CommandKind::BuildOrTest, None, None);
        organism.command_finished(CommandKind::Other, Some(0), None);
        organism.command_finished(CommandKind::GitPush, Some(1), None);
        assert_eq!(organism.idle_reaction().behavior, Behavior::GuardStuck);

        organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(organism.idle_reaction().behavior, Behavior::GuardRecovery);

        // A relapse starts a fresh open streak; the day's flip count does not
        // falsely make one unresolved failure look like the fourth failure.
        organism.command_finished(CommandKind::BuildOrTest, Some(1), None);
        assert_eq!(organism.idle_reaction().behavior, Behavior::GuardFailure);
    }

    #[test]
    fn authoritative_work_state_normalizes_and_can_downgrade_a_stale_pane() {
        assert_eq!(
            RepoWorkState::new(1, true, u32::MAX),
            RepoWorkState {
                open_failures: 1,
                recovered_pending_push: false,
                failure_success_flips: u32::MAX,
            }
        );

        let mut organism = NativeOrganism::default();
        organism.restore_repo_context(3, false, 0, 3);
        assert_eq!(organism.repo_vigil(), RepoVigil::Stuck);
        assert!(organism.sync_repo_work_state(RepoWorkState::new(1, false, 0)));
        assert_eq!(organism.repo_vigil(), RepoVigil::Failure);
        assert!(!organism.sync_repo_work_state(RepoWorkState::new(1, false, 0)));
    }

    #[test]
    fn the_third_same_day_flip_makes_pending_recovery_cautious_until_push() {
        let mut organism = NativeOrganism::default();
        organism.restore_repo_work_context(RepoWorkState::new(1, false, 2), 2, 3);

        organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(organism.repo_vigil(), RepoVigil::CautiousRecovery);
        let guard = organism.idle_reaction();
        assert_eq!(guard.behavior, Behavior::GuardCautious);
        assert!(!guard.description.contains("flaky"));

        organism.command_finished(CommandKind::GitPush, Some(0), None);
        assert_eq!(organism.repo_vigil(), RepoVigil::None);
    }

    #[test]
    fn a_successful_push_dry_run_never_closes_recovered_work() {
        let mut organism = NativeOrganism::default();
        organism.restore_repo_context(0, true, 1, 1);
        let kind = classify_command("git push --dry-run origin main");
        organism.command_started(kind);
        let reaction = organism.command_finished(kind, Some(0), None);
        assert_eq!(reaction.behavior, Behavior::Idle);
        assert_eq!(reaction.speech, None);
        assert!(organism.guarding_recovery());
        assert_eq!(organism.idle_reaction().behavior, Behavior::GuardRecovery);
    }

    #[test]
    fn repo_arrival_shapes_the_next_command_start_only() {
        assert_eq!(RepoArrival::from_familiarity(0), RepoArrival::Unfamiliar);
        assert_eq!(RepoArrival::from_familiarity(3), RepoArrival::Known);
        assert_eq!(RepoArrival::from_familiarity(7), RepoArrival::Home);

        let mut organism = NativeOrganism::default();
        organism.note_repo_arrival(RepoArrival::Unfamiliar);
        let confidence_before = organism.state().confidence;
        let shy = organism.command_started(CommandKind::BuildOrTest);
        assert_eq!(shy.tone, Tone::Quiet);
        assert!(shy.description.contains("first day in this repo"));
        assert_eq!(shy.speech, None);
        assert!(organism.state().confidence < confidence_before);

        let plain = organism.command_started(CommandKind::BuildOrTest);
        assert_eq!(plain.tone, Tone::Active);
        assert!(!plain.description.contains("first day"));

        organism.note_repo_arrival(RepoArrival::Home);
        let home = organism.command_started(CommandKind::BuildOrTest);
        assert_eq!(home.speech, Some("回来了。"));
    }

    #[test]
    fn accepted_correction_gets_a_small_nod_on_the_next_success_only() {
        let mut organism = NativeOrganism::default();
        organism.note_assisted_command();
        let success = organism.command_finished(CommandKind::Other, Some(0), None);
        assert_eq!(success.behavior, Behavior::Celebrate);
        assert_eq!(success.tone, Tone::Success);
        assert!(success.description.contains("corrected command worked"));
        assert_eq!(success.speech, None);

        let ordinary = organism.command_finished(CommandKind::Other, Some(0), None);
        assert_eq!(ordinary.behavior, Behavior::Idle);

        // A failed assisted command earns no celebration, and the assist does
        // not carry over to the next command.
        organism.note_assisted_command();
        let failed = organism.command_finished(CommandKind::Other, Some(1), None);
        assert_eq!(failed.behavior, Behavior::InspectError);
        let following = organism.command_finished(CommandKind::Other, Some(0), None);
        assert_eq!(following.behavior, Behavior::Idle);
    }

    #[test]
    fn sticky_glyphs_stay_five_ascii_characters_and_animate_slowly() {
        for behavior in [
            Behavior::Idle,
            Behavior::WatchCommand,
            Behavior::InspectError,
            Behavior::SitNearError,
            Behavior::Celebrate,
            Behavior::CelebrateBig,
            Behavior::RestAfterPush,
            Behavior::UnknownOutcome,
            Behavior::GlanceAside,
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
            for frame in [0, 4, 5, 54, 55, 59, u64::MAX] {
                for drowsy in [false, true] {
                    let language = BodyLanguage {
                        drowsy,
                        ..Default::default()
                    };
                    let glyph = sticky_glyph(behavior, language, frame);
                    assert_eq!(glyph.chars().count(), 5);
                    assert!(glyph.is_ascii());
                }
            }
        }
        // Watching alternates every five frames; idle almost never moves.
        let calm = BodyLanguage::default();
        assert_ne!(
            sticky_glyph(Behavior::WatchCommand, calm, 0),
            sticky_glyph(Behavior::WatchCommand, calm, 5)
        );
        assert_eq!(
            sticky_glyph(Behavior::Idle, calm, 0),
            sticky_glyph(Behavior::Idle, calm, 5)
        );
        assert_ne!(
            sticky_glyph(Behavior::Idle, calm, 55),
            sticky_glyph(Behavior::Idle, calm, 0)
        );
    }

    fn bounding_box_of(sprite: &str) -> (usize, usize) {
        (
            sprite.lines().count(),
            sprite
                .lines()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0),
        )
    }

    #[test]
    fn every_sprite_frame_keeps_its_behaviors_bounding_box_stable() {
        let languages = [
            BodyLanguage::default(),
            BodyLanguage {
                drowsy: true,
                ..Default::default()
            },
            BodyLanguage {
                tense: true,
                ..Default::default()
            },
            BodyLanguage {
                listless: true,
                ..Default::default()
            },
            BodyLanguage {
                drowsy: true,
                tense: true,
                listless: true,
            },
        ];
        for behavior in [
            Behavior::Idle,
            Behavior::WatchCommand,
            Behavior::InspectError,
            Behavior::SitNearError,
            Behavior::Celebrate,
            Behavior::CelebrateBig,
            Behavior::RestAfterPush,
            Behavior::UnknownOutcome,
            Behavior::GlanceAside,
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
            let reference = bounding_box_of(behavior.sprite());
            for language in languages {
                for walking in [false, true] {
                    for frame in 0..130 {
                        let frame_box =
                            bounding_box_of(sprite_frame(behavior, language, walking, frame));
                        assert_eq!(
                            frame_box, reference,
                            "{behavior:?} {language:?} walking={walking} frame={frame}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn adult_steady_render_context_is_byte_for_byte_backwards_compatible() {
        let languages = [
            BodyLanguage::default(),
            BodyLanguage {
                drowsy: true,
                tense: true,
                listless: true,
            },
        ];
        for behavior in [
            Behavior::Idle,
            Behavior::WatchCommand,
            Behavior::InspectError,
            Behavior::SitNearError,
            Behavior::Celebrate,
            Behavior::CelebrateBig,
            Behavior::RestAfterPush,
            Behavior::UnknownOutcome,
            Behavior::GlanceAside,
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
            for language in languages {
                for walking in [false, true] {
                    let context = RenderContext::new(behavior, language, walking);
                    for frame in 0..130 {
                        assert_eq!(
                            sprite_frame_with_context(context, frame).as_ref(),
                            sprite_frame(behavior, language, walking, frame)
                        );
                        assert_eq!(
                            sticky_glyph_with_context(context, frame).as_ref(),
                            sticky_glyph(behavior, language, frame)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn growth_is_visible_width_preserving_and_keeps_behavior_marks() {
        let adult = RenderContext::new(Behavior::WatchCommand, BodyLanguage::default(), false);
        let juvenile = adult.with_growth_stage(VisualGrowthStage::Juvenile);
        let seasoned = adult.with_growth_stage(VisualGrowthStage::Seasoned);
        let adult_sprite = sprite_frame_with_context(adult, 0);
        let juvenile_sprite = sprite_frame_with_context(juvenile, 0);
        let seasoned_sprite = sprite_frame_with_context(seasoned, 0);

        assert_ne!(juvenile_sprite, adult_sprite);
        assert_ne!(seasoned_sprite, adult_sprite);
        assert!(juvenile_sprite.contains("(\\_/)") && juvenile_sprite.contains("O.O"));
        assert!(seasoned_sprite.contains("/\\_/|") && seasoned_sprite.contains("-.o"));
        assert_eq!(
            bounding_box_of(&juvenile_sprite),
            bounding_box_of(&adult_sprite)
        );
        assert_eq!(
            bounding_box_of(&seasoned_sprite),
            bounding_box_of(&adult_sprite)
        );

        let juvenile_glyph = sticky_glyph_with_context(juvenile, 0);
        let adult_glyph = sticky_glyph_with_context(adult, 0);
        let seasoned_glyph = sticky_glyph_with_context(seasoned, 0);
        assert_eq!(juvenile_glyph.chars().count(), 5);
        assert_eq!(seasoned_glyph.chars().count(), 5);
        assert_ne!(juvenile_glyph, adult_glyph);
        assert_ne!(seasoned_glyph, adult_glyph);

        // Maturity never erases an event marker in the compact form.
        for stage in [
            VisualGrowthStage::Juvenile,
            VisualGrowthStage::Adult,
            VisualGrowthStage::Seasoned,
        ] {
            let glyph = sticky_glyph_with_context(
                RenderContext::new(Behavior::GuardFailure, BodyLanguage::default(), false)
                    .with_growth_stage(stage),
                0,
            );
            assert_eq!(glyph.as_bytes()[2], b'!');
        }

        // Seasoned motion holds the first tail pose longer; juvenile motion
        // reaches the alternate pose sooner. Adult remains the old cadence.
        let juvenile_agent =
            RenderContext::new(Behavior::WatchAgent, BodyLanguage::default(), false)
                .with_growth_stage(VisualGrowthStage::Juvenile);
        assert_ne!(
            sprite_frame_with_context(juvenile_agent, 0),
            sprite_frame_with_context(juvenile_agent, 4)
        );
        assert_ne!(
            sprite_frame_with_context(adult, 0),
            sprite_frame_with_context(adult, 5)
        );
        assert_eq!(
            sprite_frame_with_context(seasoned, 0),
            sprite_frame_with_context(seasoned, 5)
        );
    }

    #[test]
    fn every_growth_layer_preserves_every_pose_family_envelope() {
        let behaviors = [
            Behavior::Idle,
            Behavior::WatchCommand,
            Behavior::InspectError,
            Behavior::SitNearError,
            Behavior::Celebrate,
            Behavior::CelebrateBig,
            Behavior::RestAfterPush,
            Behavior::UnknownOutcome,
            Behavior::GlanceAside,
            Behavior::Sleep,
            Behavior::Explore,
            Behavior::Approach,
            Behavior::WatchAgent,
            Behavior::WatchSettled,
            Behavior::GuardFailure,
            Behavior::GuardStuck,
            Behavior::GuardRecovery,
            Behavior::GuardCautious,
        ];
        let languages = [
            BodyLanguage::default(),
            BodyLanguage {
                drowsy: true,
                tense: true,
                listless: true,
            },
        ];
        for behavior in behaviors {
            for language in languages {
                for walking in [false, true] {
                    for rhythm in [
                        WatchRhythm::Steady,
                        WatchRhythm::Busy,
                        WatchRhythm::Waiting,
                        WatchRhythm::Resumed,
                    ] {
                        for frame in 0..130 {
                            let adult = RenderContext::new(behavior, language, walking)
                                .with_watch_rhythm(rhythm);
                            let reference =
                                bounding_box_of(&sprite_frame_with_context(adult, frame));
                            for stage in [
                                VisualGrowthStage::Juvenile,
                                VisualGrowthStage::Adult,
                                VisualGrowthStage::Seasoned,
                            ] {
                                let context = adult.with_growth_stage(stage);
                                assert_eq!(
                                    bounding_box_of(&sprite_frame_with_context(context, frame)),
                                    reference,
                                    "{behavior:?} {language:?} walking={walking} {rhythm:?} {stage:?} frame={frame}"
                                );
                                assert_eq!(
                                    sticky_glyph_with_context(context, frame).chars().count(),
                                    5
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn watch_rhythms_are_content_free_distinct_and_box_stable() {
        for behavior in [
            Behavior::WatchCommand,
            Behavior::WatchAgent,
            Behavior::WatchSettled,
        ] {
            let reference = bounding_box_of(behavior.sprite());
            let mut sprites = Vec::new();
            let mut glyphs = Vec::new();
            for rhythm in [
                WatchRhythm::Steady,
                WatchRhythm::Busy,
                WatchRhythm::Waiting,
                WatchRhythm::Resumed,
            ] {
                let context = RenderContext::new(behavior, BodyLanguage::default(), false)
                    .with_watch_rhythm(rhythm);
                let sprite = sprite_frame_with_context(context, 0);
                assert_eq!(
                    bounding_box_of(&sprite),
                    reference,
                    "{behavior:?} {rhythm:?}"
                );
                let glyph = sticky_glyph_with_context(context, 0);
                assert_eq!(glyph.chars().count(), 5);
                assert!(glyph.is_ascii());
                sprites.push(sprite.into_owned());
                glyphs.push(glyph.into_owned());
            }
            sprites.sort();
            sprites.dedup();
            glyphs.sort();
            glyphs.dedup();
            assert_eq!(sprites.len(), 4, "{behavior:?} live rhythms collapsed");
            assert_eq!(glyphs.len(), 4, "{behavior:?} sticky rhythms collapsed");
        }

        // A rhythm is ignored outside a watching behavior.
        let idle = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
        assert_eq!(
            sprite_frame_with_context(idle, 20),
            sprite_frame_with_context(idle.with_watch_rhythm(WatchRhythm::Busy), 20)
        );
    }

    #[test]
    fn semantic_transitions_are_bounded_one_shot_arcs() {
        let transitions = [
            VisualTransition::InspectErrorToGuardFailure,
            VisualTransition::SitNearErrorToGuardStuck,
            VisualTransition::GuardFailureToGuardRecovery,
            VisualTransition::GuardFailureToGuardCautious,
            VisualTransition::GuardStuckToGuardRecovery,
            VisualTransition::GuardStuckToGuardCautious,
            VisualTransition::WatchSettledToCelebrate,
            VisualTransition::WatchSettledToCelebrateBig,
            VisualTransition::CelebrateToGuardRecovery,
            VisualTransition::CelebrateToGuardCautious,
            VisualTransition::CelebrateBigToGuardRecovery,
            VisualTransition::CelebrateBigToGuardCautious,
            VisualTransition::GuardRecoveryToRestAfterPush,
            VisualTransition::GuardCautiousToRestAfterPush,
        ];
        for transition in transitions {
            assert_eq!(
                VisualTransition::between(transition.source(), transition.target()),
                Some(transition)
            );
            assert_eq!(transition.frame_count(), 4);
            let reference = bounding_box_of(transition.sprite_frame(0));
            for frame in 0..transition.frame_count() {
                assert_eq!(
                    bounding_box_of(transition.sprite_frame(frame)),
                    reference,
                    "{transition:?} frame {frame}"
                );
            }
            assert_eq!(
                transition.sprite_frame(u64::MAX),
                transition.sprite_frame(transition.frame_count() - 1)
            );

            for stage in [
                VisualGrowthStage::Juvenile,
                VisualGrowthStage::Adult,
                VisualGrowthStage::Seasoned,
            ] {
                let context =
                    RenderContext::new(transition.target(), BodyLanguage::default(), false)
                        .with_growth_stage(stage)
                        .with_transition(Some(transition));
                for frame in 0..transition.frame_count() {
                    assert_eq!(
                        bounding_box_of(&sprite_frame_with_context(context, frame)),
                        reference,
                        "{transition:?} {stage:?} frame {frame}"
                    );
                }
            }
        }
        assert_eq!(
            VisualTransition::between(Behavior::Idle, Behavior::Celebrate),
            None
        );
    }

    #[test]
    fn celebrations_keep_the_organisms_cat_silhouette() {
        for behavior in [Behavior::Celebrate, Behavior::CelebrateBig] {
            for frame in 0..10 {
                assert!(
                    sprite_frame(behavior, BodyLanguage::default(), false, frame)
                        .contains("/\\_/\\"),
                    "{behavior:?} frame {frame} lost its ears"
                );
            }
        }
    }

    #[test]
    fn body_language_quantizes_the_continuous_state() {
        assert_eq!(
            BodyLanguage::from_state(LifeState::default()),
            BodyLanguage::default()
        );

        let exhausted = BodyLanguage::from_state(LifeState {
            energy: 0.10,
            boredom: 0.95,
            ..LifeState::default()
        });
        assert!(exhausted.drowsy);
        assert!(!exhausted.listless);

        let wired = BodyLanguage::from_state(LifeState {
            stress: 0.70,
            boredom: 0.90,
            ..LifeState::default()
        });
        assert!(wired.tense);
        assert!(wired.listless);
        assert!(!wired.drowsy);
    }

    #[test]
    fn drowsiness_overrides_walking_and_shows_in_the_sticky_header() {
        let drowsy = BodyLanguage {
            drowsy: true,
            ..Default::default()
        };
        for frame in 0..130 {
            let sprite = sprite_frame(Behavior::Idle, drowsy, true, frame);
            assert!(sprite.contains("zZ"), "dozing cat must not walk");
        }
        assert_eq!(sticky_glyph(Behavior::Idle, drowsy, 0), "=\\z/=");
        assert_ne!(
            sticky_glyph(Behavior::Idle, drowsy, 0),
            sticky_glyph(Behavior::RestAfterPush, BodyLanguage::default(), 0)
        );
        assert_eq!(
            sticky_glyph(Behavior::Idle, BodyLanguage::default(), 0),
            "/\\_/\\"
        );
    }

    #[test]
    fn a_listless_cat_yawns_rarely_and_a_tense_cat_flattens_its_ears() {
        let listless = BodyLanguage {
            listless: true,
            ..Default::default()
        };
        let yawns = (0..600)
            .filter(|frame| sprite_frame(Behavior::Idle, listless, false, *frame) == YAWN_FRAME)
            .count();
        assert!(yawns > 0);
        assert!(yawns * 8 < 600);

        let tense = BodyLanguage {
            tense: true,
            ..Default::default()
        };
        assert!(sprite_frame(Behavior::WatchCommand, tense, false, 0).starts_with(" =\\_/="));
        assert!(sprite_frame(Behavior::Idle, tense, false, 0).starts_with(" =\\_/="));
    }

    #[test]
    fn utility_scores_pick_the_disposition_the_state_calls_for() {
        // Clear margins (> inertia + jitter) so outcomes are deterministic.
        let rested = LifeState {
            energy: 0.9,
            mood: 0.8,
            boredom: 0.1,
            curiosity: 0.2,
            social_need: 0.2,
            attachment: 0.3,
            ..LifeState::default()
        };
        assert_eq!(
            AmbientMind::default().step(rested, 0.0, 0.0, RepoVigil::None),
            AmbientBehavior::Idle
        );

        let bored = LifeState {
            energy: 0.8,
            boredom: 1.0,
            curiosity: 1.0,
            social_need: 0.1,
            ..LifeState::default()
        };
        assert_eq!(
            AmbientMind::default().step(bored, 0.0, 0.0, RepoVigil::None),
            AmbientBehavior::Explore
        );

        let lonely = LifeState {
            energy: 0.9,
            boredom: 0.0,
            curiosity: 0.0,
            social_need: 1.0,
            attachment: 1.0,
            ..LifeState::default()
        };
        assert_eq!(
            AmbientMind::default().step(lonely, 0.0, 0.0, RepoVigil::None),
            AmbientBehavior::Approach
        );

        // A long quiet stretch tilts a merely tired mind toward sleep.
        let tired = LifeState {
            energy: 0.5,
            boredom: 0.3,
            curiosity: 0.2,
            social_need: 0.2,
            attachment: 0.2,
            ..LifeState::default()
        };
        assert_eq!(
            AmbientMind::default().step(tired, 60.0, 0.0, RepoVigil::None),
            AmbientBehavior::Sleep
        );
    }

    #[test]
    fn exhaustion_overrides_scoring_and_dispositions_hold_before_rescoring() {
        let mut mind = AmbientMind::default();
        let exhausted = LifeState {
            energy: 0.1,
            boredom: 1.0,
            curiosity: 1.0,
            ..LifeState::default()
        };
        assert_eq!(
            mind.step(exhausted, 0.0, 0.0, RepoVigil::None),
            AmbientBehavior::Sleep
        );

        // Held for 2.5s even when the state now argues for something else.
        let recovered = LifeState {
            energy: 0.9,
            boredom: 1.0,
            curiosity: 1.0,
            social_need: 0.1,
            ..LifeState::default()
        };
        assert_eq!(
            mind.step(recovered, 0.0, 1.0, RepoVigil::None),
            AmbientBehavior::Sleep
        );
        assert_eq!(
            mind.step(recovered, 0.0, 1.0, RepoVigil::None),
            AmbientBehavior::Sleep
        );
        assert_eq!(
            mind.step(recovered, 0.0, 1.0, RepoVigil::None),
            AmbientBehavior::Explore
        );

        mind.interrupt();
        assert_eq!(mind.current(), AmbientBehavior::Idle);

        // Hostile inputs never panic and always yield a valid disposition.
        let mut hostile = AmbientMind::default();
        for dt in [f32::NAN, f32::INFINITY, -3.0, 1e30] {
            hostile.step(LifeState::default(), f32::NAN, dt, RepoVigil::None);
        }
    }

    #[test]
    fn repo_vigils_keep_distinct_watch_but_never_override_exhaustion() {
        let mut mind = AmbientMind::default();
        let restless = LifeState {
            energy: 0.8,
            boredom: 1.0,
            curiosity: 1.0,
            ..LifeState::default()
        };
        for (vigil, expected) in [
            (RepoVigil::Failure, AmbientBehavior::GuardFailure),
            (RepoVigil::Stuck, AmbientBehavior::GuardStuck),
            (RepoVigil::Recovery, AmbientBehavior::GuardRecovery),
            (RepoVigil::CautiousRecovery, AmbientBehavior::GuardCautious),
        ] {
            let mut mapped = AmbientMind::default();
            assert_eq!(mapped.step(restless, 60.0, 0.0, vigil), expected);
        }
        assert_eq!(
            mind.step(restless, 60.0, 0.0, RepoVigil::Recovery),
            AmbientBehavior::GuardRecovery
        );
        assert_eq!(mind.current(), AmbientBehavior::GuardRecovery);

        let exhausted = LifeState {
            energy: 0.1,
            ..restless
        };
        assert_eq!(
            mind.step(exhausted, 60.0, 0.0, RepoVigil::Recovery),
            AmbientBehavior::Sleep
        );

        let barely_recovered = LifeState {
            energy: FORCED_REST_ENERGY + 0.01,
            ..restless
        };
        assert_eq!(
            mind.step(barely_recovered, 60.0, 1.0, RepoVigil::Recovery),
            AmbientBehavior::Sleep,
            "crossing the force-rest edge must not flutter awake"
        );
        let awake = LifeState {
            energy: REPO_VIGIL_WAKE_ENERGY,
            ..restless
        };
        assert_eq!(
            mind.step(awake, 60.0, 1.0, RepoVigil::Recovery),
            AmbientBehavior::GuardRecovery
        );

        assert_ne!(
            mind.step(restless, 0.0, 0.0, RepoVigil::None),
            AmbientBehavior::GuardRecovery,
            "clearing the durable intention must release its forced pose"
        );
    }

    #[test]
    fn an_exploring_cat_only_steps_while_actually_moving() {
        let calm = BodyLanguage::default();
        assert!(sprite_frame(Behavior::Explore, calm, false, 0).contains("> ^ <"));
        assert!(sprite_frame(Behavior::Explore, calm, true, 0).contains(">/ \\<"));
    }

    #[test]
    fn agent_commands_get_quiet_nods_and_the_big_celebrations_stay_human() {
        let mut organism = NativeOrganism::default();
        organism.set_agent_command(true);
        let started = organism.command_started(CommandKind::BuildOrTest);
        assert_eq!(started.behavior, Behavior::WatchAgent);
        assert_eq!(started.tone, Tone::Quiet);

        // Even a big recovery earns only a small, wordless celebration.
        organism.restore_repo_context(3, false, 0, 3);
        organism.set_agent_command(true);
        let recovery = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(recovery.behavior, Behavior::Celebrate);
        assert_eq!(recovery.speech, None);
        assert!(recovery.description.contains("agent-driven"));

        // The same recovery typed by the human celebrates at full strength.
        let mut human = NativeOrganism::default();
        human.restore_repo_context(3, false, 0, 3);
        let big = human.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert_eq!(big.behavior, Behavior::CelebrateBig);
        assert_eq!(big.speech, Some("终于。"));
    }

    #[test]
    fn a_repo_greeting_waits_for_the_humans_own_command() {
        let mut organism = NativeOrganism::default();
        organism.note_repo_arrival(RepoArrival::Home);
        organism.set_agent_command(true);
        let agent_start = organism.command_started(CommandKind::BuildOrTest);
        assert_eq!(agent_start.behavior, Behavior::WatchAgent);
        assert_eq!(agent_start.speech, None);
        organism.command_finished(CommandKind::BuildOrTest, Some(0), None);

        organism.set_agent_command(false);
        let human_start = organism.command_started(CommandKind::BuildOrTest);
        assert_eq!(human_start.speech, Some("回来了。"));
    }

    #[test]
    fn leaving_a_repo_cancels_its_queued_human_greeting() {
        let mut organism = NativeOrganism::default();
        organism.note_repo_arrival(RepoArrival::Home);
        organism.set_agent_command(true);
        organism.command_started(CommandKind::BuildOrTest);
        organism.command_finished(CommandKind::BuildOrTest, Some(0), None);

        organism.clear_repo_arrival();
        organism.set_agent_command(false);
        let elsewhere = organism.command_started(CommandKind::Other);
        assert_eq!(elsewhere.speech, None);
        assert!(!elsewhere.description.contains("well-known repo"));
    }

    #[test]
    fn agent_failures_spare_the_humans_confidence_and_stay_silent() {
        let mut organism = NativeOrganism::default();
        let confidence_before = organism.state().confidence;
        organism.set_agent_command(true);
        let failure = organism.command_finished(CommandKind::BuildOrTest, Some(101), None);
        assert_eq!(failure.speech, None);
        assert!(failure.description.contains("agent-driven"));
        assert_eq!(organism.state().confidence, confidence_before);

        // A sensitized first crack never triggers for the Agent's failure.
        let mut proud = NativeOrganism::default();
        proud.restore_repo_context(0, false, 9, 0);
        proud.set_agent_command(true);
        let crack = proud.command_finished(CommandKind::BuildOrTest, Some(101), None);
        assert!(!crack.description.contains("first crack"));

        // An agent push never claims the human's follow-through phrase.
        let mut push = NativeOrganism::default();
        push.restore_repo_context(1, false, 0, 1);
        push.command_finished(CommandKind::BuildOrTest, Some(0), None);
        push.set_agent_command(true);
        let pushed = push.command_finished(CommandKind::GitPush, Some(0), None);
        assert_eq!(pushed.speech, None);
    }

    #[test]
    fn agent_execution_lost_reacts_with_restrained_caution() {
        let mut organism = NativeOrganism::default();
        organism.set_agent_command(true);
        organism.command_started(CommandKind::BuildOrTest);
        let lost = organism.agent_execution_lost();
        assert_eq!(lost.behavior, Behavior::UnknownOutcome);
        assert_eq!(lost.tone, Tone::Warning);
        assert_eq!(lost.speech, None);

        // The stale flag is gone: the next human command is fully owned.
        let success = organism.command_finished(CommandKind::BuildOrTest, Some(0), None);
        assert!(!success.description.contains("agent-driven"));
        assert_eq!(success.speech, Some("过了。"));
    }

    #[test]
    fn agent_pulses_feed_social_need_and_stay_bounded() {
        let mut state = LifeState::default();
        let social_before = state.social_need;
        state = agent_pulse(state, AgentPulse::Working);
        state = agent_pulse(state, AgentPulse::AskingReview);
        state = agent_pulse(state, AgentPulse::Finished);
        state = agent_pulse(state, AgentPulse::Gone);
        assert!(state.social_need > social_before);

        for _ in 0..10_000 {
            state = agent_pulse(state, AgentPulse::Gone);
        }
        assert!(state
            .values()
            .into_iter()
            .all(|value| (0.0..=1.0).contains(&value)));
    }

    #[test]
    fn ambient_dispositions_map_to_their_display_behaviors() {
        assert_eq!(AmbientBehavior::Idle.display(), Behavior::Idle);
        assert_eq!(AmbientBehavior::Sleep.display(), Behavior::Sleep);
        assert_eq!(AmbientBehavior::Explore.display(), Behavior::Explore);
        assert_eq!(AmbientBehavior::Approach.display(), Behavior::Approach);
        assert_eq!(
            AmbientBehavior::GuardFailure.display(),
            Behavior::GuardFailure
        );
        assert_eq!(AmbientBehavior::GuardStuck.display(), Behavior::GuardStuck);
        assert_eq!(
            AmbientBehavior::GuardRecovery.display(),
            Behavior::GuardRecovery
        );
        assert_eq!(
            AmbientBehavior::GuardCautious.display(),
            Behavior::GuardCautious
        );
    }

    #[test]
    fn correction_pulses_stay_bounded_and_scale_with_dismissal_streaks() {
        let mut state = LifeState::default();
        for _ in 0..1_000 {
            state = correction_dismissed(state, 4);
        }
        assert!(state
            .values()
            .into_iter()
            .all(|value| (0.0..=1.0).contains(&value)));
        assert_eq!(state.boredom, 1.0);

        let single = correction_dismissed(LifeState::default(), 1);
        let heavy = correction_dismissed(LifeState::default(), 4);
        assert!(heavy.boredom > single.boredom);

        let accepted = correction_accepted(LifeState::default());
        assert!(accepted.confidence > LifeState::default().confidence);
    }

    #[test]
    fn persisted_state_and_repo_failure_streak_resume_safely() {
        let mut organism = NativeOrganism::from_persisted_state(LifeState {
            energy: f32::NAN,
            mood: 2.0,
            ..LifeState::default()
        });
        assert_eq!(organism.state().energy, 0.5);
        assert_eq!(organism.state().mood, 1.0);

        organism.restore_build_failures(3);
        organism.command_started(CommandKind::BuildOrTest);
        let reaction = organism.command_finished(CommandKind::BuildOrTest, Some(0), Some(100));
        assert_eq!(reaction.behavior, Behavior::CelebrateBig);
        assert_eq!(reaction.speech, Some("终于。"));
    }
}
