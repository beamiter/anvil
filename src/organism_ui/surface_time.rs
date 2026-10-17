//! Shared display-only time contract. No scheduler, capability, terminal I/O,
//! geometry, or semantic activity lives here. Hosts retain those decisions.
use std::time::Duration;

pub(super) const HUMAN_INPUT_RETREAT: Duration = Duration::from_millis(900);

/// Unknown input is not input. The exact deadline is already expired.
/// Hosts normalize monotonic timestamps with saturating_duration_since first;
/// a timestamp ahead of the observation conservatively has age zero.
pub(super) fn input_retreat_remaining(age: Option<Duration>) -> Option<Duration> {
    age.and_then(|age| HUMAN_INPUT_RETREAT.checked_sub(age))
        .filter(|remaining| !remaining.is_zero())
}

/// Freeze only the display frame. This does not stop a host's semantic clock,
/// change its capability, or authorize a timer while hidden/backgrounded.
pub(super) fn display_frame(full_motion: bool, frame: u64) -> u64 {
    if full_motion {
        frame
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_input_and_exact_expiry_are_not_retreats() {
        assert_eq!(input_retreat_remaining(None), None);
        assert_eq!(input_retreat_remaining(Some(HUMAN_INPUT_RETREAT)), None);
        assert_eq!(input_retreat_remaining(Some(Duration::MAX)), None);
    }

    #[test]
    fn nanosecond_boundary_is_half_open() {
        let ns = Duration::from_nanos(1);
        assert_eq!(
            input_retreat_remaining(Some(Duration::ZERO)),
            Some(HUMAN_INPUT_RETREAT)
        );
        assert_eq!(
            input_retreat_remaining(Some(HUMAN_INPUT_RETREAT - ns)),
            Some(ns)
        );
        assert_eq!(
            input_retreat_remaining(Some(HUMAN_INPUT_RETREAT + ns)),
            None
        );
    }

    #[test]
    fn repeated_input_moves_the_original_deadline() {
        let last_input = Duration::from_millis(700);
        for (now, expected) in [
            (900, Some(700)),
            (1599, Some(1)),
            (1600, None),
            (1601, None),
        ] {
            assert_eq!(
                input_retreat_remaining(Some(Duration::from_millis(now) - last_input)),
                expected.map(Duration::from_millis)
            );
        }
    }

    #[test]
    fn future_observation_saturates_conservatively() {
        let future_input = Duration::from_secs(2);
        let now = Duration::from_secs(1);
        assert_eq!(
            input_retreat_remaining(Some(now.saturating_sub(future_input))),
            Some(HUMAN_INPUT_RETREAT)
        );
    }

    #[test]
    fn reduced_motion_never_inherits_an_animation_phase() {
        for frame in [0, 1, 35, 55, 119, 120, u64::MAX] {
            assert_eq!(display_frame(true, frame), frame);
            assert_eq!(display_frame(false, frame), 0);
        }
    }
}
