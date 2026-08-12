//! Session-local attention budget for the ASCII organism.
//!
//! This module arbitrates only content-free event classes. It has no clock of
//! its own, does not persist anything, and deliberately has no pending queue:
//! callers offer a discrete event once and use the returned boolean
//! immediately to keep or remove optional speech/insight.

use std::time::Duration;

/// A content-free reason the organism might spend its limited attention.
///
/// The declaration order is an implementation detail; [`AttentionCue::rank`]
/// is the single source of truth for precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionCue {
    /// A failure remains open, including entry into a durable failure/stuck
    /// vigil. The lasting body posture is independent from optional speech.
    FailureVigil,
    /// A generic work-loop closure not more precisely described below.
    Closure,
    /// A failed build/test loop recovered.
    Recovery,
    /// Recovered work was pushed or another push closed the current loop.
    Push,
    /// A content-free elapsed-time transition while accompanying a command.
    LongCommandChange,
    /// A repo/circadian/session greeting.
    Greeting,
    /// An optional remembered-pattern or pace observation.
    Insight,
}

impl AttentionCue {
    const COUNT: usize = 7;

    const fn index(self) -> usize {
        self as usize
    }

    /// Larger ranks may interrupt smaller ones. Equal ranks share one budget,
    /// so recovery and push cannot talk over each other, nor can a greeting
    /// and an insight.
    const fn rank(self) -> u8 {
        match self {
            Self::FailureVigil => 3,
            Self::Closure | Self::Recovery | Self::Push => 2,
            Self::LongCommandChange => 1,
            Self::Greeting | Self::Insight => 0,
        }
    }
}

/// Timing policy for the ephemeral arbiter.
///
/// A focus window controls how long an admitted cue suppresses equal/lower
/// ranks. A cooldown controls how soon that exact cue may be admitted again.
/// Both are measured against caller-supplied session time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttentionPolicy {
    focus_windows: [Duration; AttentionCue::COUNT],
    cooldowns: [Duration; AttentionCue::COUNT],
}

impl AttentionPolicy {
    /// A deliberately quiet default: durable work gets the longest focus,
    /// while greetings and learned observations habituate most strongly.
    pub(crate) const DEFAULT: Self = Self {
        focus_windows: [
            Duration::from_secs(10),
            Duration::from_secs(6),
            Duration::from_secs(6),
            Duration::from_secs(6),
            Duration::from_millis(400),
            Duration::from_secs(3),
            Duration::from_secs(3),
        ],
        cooldowns: [
            Duration::from_secs(30),
            Duration::from_secs(20),
            Duration::from_secs(20),
            Duration::from_secs(20),
            Duration::from_millis(400),
            Duration::from_secs(30 * 60),
            Duration::from_secs(5 * 60),
        ],
    };

    /// Construct a simple policy, mainly useful for deterministic tests or a
    /// future motion/accessibility mode with one uniform cadence.
    pub(crate) const fn uniform(focus_window: Duration, cooldown: Duration) -> Self {
        Self {
            focus_windows: [focus_window; AttentionCue::COUNT],
            cooldowns: [cooldown; AttentionCue::COUNT],
        }
    }

    pub(crate) const fn with_focus_window(
        mut self,
        cue: AttentionCue,
        focus_window: Duration,
    ) -> Self {
        self.focus_windows[cue.index()] = focus_window;
        self
    }

    pub(crate) const fn with_cooldown(mut self, cue: AttentionCue, cooldown: Duration) -> Self {
        self.cooldowns[cue.index()] = cooldown;
        self
    }

    const fn focus_window(self, cue: AttentionCue) -> Duration {
        self.focus_windows[cue.index()]
    }

    const fn cooldown(self, cue: AttentionCue) -> Duration {
        self.cooldowns[cue.index()]
    }
}

impl Default for AttentionPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveFocus {
    cue: AttentionCue,
    admitted_at: Duration,
}

/// Pure, volatile attention state for one organism session.
///
/// This type intentionally exposes no polling/draining API. [`Self::offer`]
/// makes an immediate decision and retains only cooldown/focus timestamps, so
/// a suppressed event is discarded rather than replayed after a busy period.
#[derive(Debug, Clone)]
pub(crate) struct AttentionArbiter {
    policy: AttentionPolicy,
    active: Option<ActiveFocus>,
    admitted_at: [Option<Duration>; AttentionCue::COUNT],
    observed_at: Option<Duration>,
}

impl AttentionArbiter {
    pub(crate) const fn new(policy: AttentionPolicy) -> Self {
        Self {
            policy,
            active: None,
            admitted_at: [None; AttentionCue::COUNT],
            observed_at: None,
        }
    }

    /// Offer one discrete event at a monotonic, session-relative timestamp.
    ///
    /// `true` means the caller may show its optional speech/insight now;
    /// `false` means it must drop that optional expression. The underlying
    /// reaction/body state is outside this arbiter and should still update.
    #[must_use]
    pub(crate) fn offer(&mut self, cue: AttentionCue, now: Duration) -> bool {
        // Runtime event sources should already be monotonic. Clamping makes a
        // stale callback conservative instead of letting it escape cooldowns.
        let now = self.observed_at.map_or(now, |previous| previous.max(now));
        self.observed_at = Some(now);

        if self.admitted_at[cue.index()]
            .is_some_and(|previous| now.saturating_sub(previous) < self.policy.cooldown(cue))
        {
            return false;
        }

        if self.active.is_some_and(|active| {
            now.saturating_sub(active.admitted_at) < self.policy.focus_window(active.cue)
                && active.cue.rank() >= cue.rank()
        }) {
            return false;
        }

        self.active = Some(ActiveFocus {
            cue,
            admitted_at: now,
        });
        self.admitted_at[cue.index()] = Some(now);
        true
    }
}

impl Default for AttentionArbiter {
    fn default() -> Self {
        Self::new(AttentionPolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn precedence_matches_work_before_chatter() {
        assert!(AttentionCue::FailureVigil.rank() > AttentionCue::Recovery.rank());
        assert_eq!(AttentionCue::Closure.rank(), AttentionCue::Recovery.rank());
        assert_eq!(AttentionCue::Recovery.rank(), AttentionCue::Push.rank());
        assert!(AttentionCue::Push.rank() > AttentionCue::LongCommandChange.rank());
        assert!(AttentionCue::LongCommandChange.rank() > AttentionCue::Greeting.rank());
        assert_eq!(AttentionCue::Greeting.rank(), AttentionCue::Insight.rank());
    }

    #[test]
    fn higher_rank_preempts_lower_focus() {
        let mut arbiter =
            AttentionArbiter::new(AttentionPolicy::uniform(seconds(10), Duration::ZERO));

        assert!(arbiter.offer(AttentionCue::Greeting, Duration::ZERO));
        assert!(arbiter.offer(AttentionCue::LongCommandChange, SECOND));
        assert!(arbiter.offer(AttentionCue::Recovery, seconds(2)));
        assert!(arbiter.offer(AttentionCue::FailureVigil, seconds(3)));
    }

    #[test]
    fn active_focus_drops_equal_and_lower_rank_events() {
        let mut arbiter =
            AttentionArbiter::new(AttentionPolicy::uniform(seconds(10), Duration::ZERO));

        assert!(arbiter.offer(AttentionCue::Recovery, Duration::ZERO));
        assert!(!arbiter.offer(AttentionCue::Push, SECOND));
        assert!(!arbiter.offer(AttentionCue::LongCommandChange, seconds(2)));
        assert!(!arbiter.offer(AttentionCue::Insight, seconds(3)));
    }

    #[test]
    fn lower_rank_can_speak_after_focus_expires() {
        let mut arbiter =
            AttentionArbiter::new(AttentionPolicy::uniform(seconds(5), Duration::ZERO));

        assert!(arbiter.offer(AttentionCue::FailureVigil, Duration::ZERO));
        assert!(!arbiter.offer(AttentionCue::Greeting, seconds(4)));
        // The boundary is open: suppression lasts strictly less than the
        // configured window and never grows a hidden pending tail.
        assert!(arbiter.offer(AttentionCue::Insight, seconds(5)));
    }

    #[test]
    fn cooldown_is_per_cue_and_opens_at_its_boundary() {
        let policy = AttentionPolicy::uniform(Duration::ZERO, Duration::ZERO)
            .with_cooldown(AttentionCue::Insight, seconds(10));
        let mut arbiter = AttentionArbiter::new(policy);

        assert!(arbiter.offer(AttentionCue::Insight, Duration::ZERO));
        assert!(!arbiter.offer(AttentionCue::Insight, seconds(9)));
        assert!(arbiter.offer(AttentionCue::Greeting, seconds(9)));
        assert!(arbiter.offer(AttentionCue::Insight, seconds(10)));
    }

    #[test]
    fn cue_specific_focus_windows_are_honored() {
        let policy = AttentionPolicy::uniform(SECOND, Duration::ZERO)
            .with_focus_window(AttentionCue::FailureVigil, seconds(12));
        let mut arbiter = AttentionArbiter::new(policy);

        assert!(arbiter.offer(AttentionCue::FailureVigil, Duration::ZERO));
        assert!(!arbiter.offer(AttentionCue::Closure, seconds(11)));
        assert!(arbiter.offer(AttentionCue::Closure, seconds(12)));
    }

    #[test]
    fn suppressed_event_is_not_queued_behind_newer_work() {
        let mut arbiter =
            AttentionArbiter::new(AttentionPolicy::uniform(seconds(5), Duration::ZERO));

        assert!(arbiter.offer(AttentionCue::FailureVigil, Duration::ZERO));
        assert!(!arbiter.offer(AttentionCue::Greeting, SECOND));

        // Nothing is drained or replayed at t=5. Only this newly offered
        // command transition is considered, and it wins on its own merits.
        assert!(arbiter.offer(AttentionCue::LongCommandChange, seconds(5)));
        assert!(!arbiter.offer(AttentionCue::Greeting, seconds(6)));
    }

    #[test]
    fn stale_timestamps_are_clamped_conservatively() {
        let mut arbiter = AttentionArbiter::new(AttentionPolicy::uniform(seconds(5), seconds(10)));

        assert!(arbiter.offer(AttentionCue::Insight, seconds(20)));
        assert!(!arbiter.offer(AttentionCue::Insight, seconds(2)));
        assert!(!arbiter.offer(AttentionCue::Greeting, seconds(2)));
        assert!(arbiter.offer(AttentionCue::FailureVigil, seconds(2)));
    }
}
