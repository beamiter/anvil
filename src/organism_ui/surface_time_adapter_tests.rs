//! The same observation trace goes through the production Block/Chat and VTE
//! adapter functions. No GTK initialization, fake command events, or PTY writes.
use super::*;

#[test]
fn adapters_agree_on_input_trace_including_repeat_expiry_and_clock_order() {
    let start = Instant::now();
    let mut last_input = None;
    // (observation ms, new input ms, remaining retreat ms)
    for (observed, input, remaining) in [
        (0, None, None),
        (0, Some(0), Some(900)),
        (899, None, Some(1)),
        (900, None, None),
        (901, None, None),
        (1000, Some(1000), Some(900)),
        (1700, Some(1700), Some(900)),
        (1900, None, Some(700)),
        (2599, None, Some(1)),
        (2600, None, None),
        (2601, None, None),
        (2500, Some(2700), Some(900)),
        (3600, None, None),
    ] {
        if let Some(input) = input {
            last_input = Some(start + Duration::from_millis(input));
        }
        let now = start + Duration::from_millis(observed);
        let age = last_input.map(|at| now.saturating_duration_since(at));
        let expected = remaining.map(Duration::from_millis);
        assert_eq!(vte_surface::retreat_remaining(last_input, now), expected);
        assert_eq!(
            surface_mode(Behavior::Idle, false, age) == SurfaceMode::Typing,
            expected.is_some(),
            "observation {observed}"
        );
        // Block preserves its 1ms boundary remeasure, not a new 900ms hold.
        if age.is_some() {
            assert_eq!(
                interaction_frame_delay(
                    DORMANT_FRAME_INTERVAL,
                    OrganismMotion::Full,
                    true,
                    age,
                    None
                ),
                expected.unwrap_or_default().max(Duration::from_millis(1))
            );
        }
    }
}

#[test]
fn adapters_agree_on_common_idle_frames_without_erasing_block_reaction_epochs() {
    for motion in [
        OrganismMotion::Full,
        OrganismMotion::Calm,
        OrganismMotion::Static,
    ] {
        for frame in [0, 1, 35, 55, 119, 120, u64::MAX] {
            let neutral = vte_surface::display_frame(motion, frame);
            assert_eq!(
                surface_animation_frames(motion, frame, 7, 11, SurfaceMode::Idle, None),
                (neutral, neutral, neutral)
            );
            let (_, baseline, live) =
                surface_animation_frames(motion, frame, 7, 11, SurfaceMode::Reacting, None);
            let expected = if motion == OrganismMotion::Full {
                frame.wrapping_sub(7)
            } else {
                0
            };
            assert_eq!((baseline, live), (expected, expected));
        }
    }
}

#[test]
fn shared_time_does_not_change_host_scheduling_policy() {
    for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
        for owner in [false, true] {
            assert_eq!(
                surface_frame_delay(motion, owner, false, false),
                DORMANT_FRAME_INTERVAL
            );
            assert_eq!(
                focus_transfer_rearm_delay(motion, owner, false, false),
                None
            );
        }
    }
    assert_eq!(
        surface_frame_delay(OrganismMotion::Full, false, false, false),
        DORMANT_FRAME_INTERVAL
    );
    // Block's existing semantic maintenance heartbeat is intentionally retained.
    // VTE's native fixture separately asserts no source in Static/background;
    // a pure frame helper must not be mistaken for permission to add a timer.
}

#[test]
fn block_keeps_command_and_cross_pane_epochs_while_reduced_motion_freezes_both() {
    for mode in [SurfaceMode::Watching, SurfaceMode::Reacting] {
        assert_eq!(
            surface_animation_frames(
                OrganismMotion::Full,
                13,
                7,
                11,
                mode,
                Some(PresenceCue::GlanceAside)
            ),
            (13, 6, 2)
        );
        for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
            assert_eq!(
                surface_animation_frames(motion, 13, 7, 11, mode, Some(PresenceCue::GlanceAside)),
                (0, 0, 0)
            );
        }
    }
    // Input retreat wins over verified commands, without teaching VTE any
    // command semantics. Once expired, the Block adapter resumes watching.
    assert_eq!(
        surface_mode(Behavior::InspectError, true, Some(Duration::ZERO)),
        SurfaceMode::Typing
    );
    assert_eq!(
        surface_mode(Behavior::InspectError, true, Some(HUMAN_INPUT_RETREAT)),
        SurfaceMode::Watching
    );
    assert_eq!(
        surface_mode(Behavior::InspectError, false, Some(HUMAN_INPUT_RETREAT)),
        SurfaceMode::Reacting
    );
}
