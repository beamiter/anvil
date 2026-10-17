//! One native event sequence for both production adapters. This fixture does
//! not define a universal product presentation: capabilities stay host-specific.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum Host {
    BlockChat,
    NeutralVte,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Step {
    pub name: &'static str,
    pub motion: OrganismMotion,
    pub enabled: bool,
    pub focused: bool,
    pub retreat: bool,
}

pub(super) const TRACE: [Step; 10] = [
    Step {
        name: "full",
        motion: OrganismMotion::Full,
        enabled: true,
        focused: true,
        retreat: false,
    },
    Step {
        name: "calm",
        motion: OrganismMotion::Calm,
        enabled: true,
        focused: true,
        retreat: false,
    },
    Step {
        name: "input-retreat",
        motion: OrganismMotion::Calm,
        enabled: true,
        focused: true,
        retreat: true,
    },
    Step {
        name: "expired-input",
        motion: OrganismMotion::Calm,
        enabled: true,
        focused: true,
        retreat: false,
    },
    Step {
        name: "background",
        motion: OrganismMotion::Calm,
        enabled: true,
        focused: false,
        retreat: false,
    },
    Step {
        name: "focus-return",
        motion: OrganismMotion::Calm,
        enabled: true,
        focused: true,
        retreat: false,
    },
    Step {
        name: "static",
        motion: OrganismMotion::Static,
        enabled: true,
        focused: true,
        retreat: false,
    },
    Step {
        name: "full-before-disable",
        motion: OrganismMotion::Full,
        enabled: true,
        focused: true,
        retreat: false,
    },
    Step {
        name: "disabled",
        motion: OrganismMotion::Static,
        enabled: false,
        focused: true,
        retreat: false,
    },
    Step {
        name: "reenabled",
        motion: OrganismMotion::Calm,
        enabled: true,
        focused: true,
        retreat: false,
    },
];

#[derive(Debug)]
pub(super) struct Observed {
    pub primary_painted: bool,
    pub card_visible: bool,
    pub wake_pending: bool,
}

pub(super) fn run(host: Host, mut apply: impl FnMut(Step) -> Observed) {
    for step in TRACE {
        let observed = apply(step);
        let common_visible = step.enabled && step.focused && !step.retreat;
        let (primary_painted, wake_pending) = match host {
            // Static keeps only Block's inline card. Its existing maintenance
            // heartbeat remains legitimate even without a spatial body.
            Host::BlockChat => (
                common_visible && step.motion != OrganismMotion::Static,
                step.enabled,
            ),
            // VTE has no semantic maintenance heartbeat or command reaction.
            Host::NeutralVte => (
                common_visible,
                step.enabled
                    && step.focused
                    && (step.retreat || step.motion == OrganismMotion::Full),
            ),
        };
        assert_eq!(
            observed.primary_painted, primary_painted,
            "{host:?} {step:?}: {observed:?}"
        );
        assert_eq!(
            observed.card_visible, step.enabled,
            "{host:?} {step:?}: {observed:?}"
        );
        assert_eq!(
            observed.wake_pending, wake_pending,
            "{host:?} {step:?}: {observed:?}"
        );
        println!("ORGANISM_ADAPTER_TRACE {host:?} {} {observed:?}", step.name);
    }
}

#[test]
fn trace_reaches_every_named_boundary_and_disables_a_live_source() {
    fn check_steps(steps: [Step; 10]) {
        assert_eq!(
            steps.map(|step| step.name),
            [
                "full",
                "calm",
                "input-retreat",
                "expired-input",
                "background",
                "focus-return",
                "static",
                "full-before-disable",
                "disabled",
                "reenabled"
            ]
        );
        assert!(steps[2].retreat && !steps[3].retreat);
        assert!(!steps[4].focused && steps[5].focused);
        assert_eq!(steps[6].motion, OrganismMotion::Static);
        assert!(steps[7].enabled && steps[7].focused);
        assert_eq!(steps[7].motion, OrganismMotion::Full);
        assert!(!steps[8].enabled && steps[9].enabled);
    }
    check_steps(TRACE);
}

#[test]
fn trace_driver_rejects_a_missing_primary_paint() {
    for host in [Host::BlockChat, Host::NeutralVte] {
        assert!(std::panic::catch_unwind(|| run(host, |_| Observed {
            primary_painted: false,
            card_visible: true,
            wake_pending: true,
        }))
        .is_err());
    }
}

pub(super) fn exercise_block(
    runtime: &Rc<OrganismRuntime>,
    view: &Rc<TermView>,
    settle: &impl Fn(Duration),
) {
    assert!(crate::block_view::organism_settings_test_pty_bytes(view).is_empty());
    crate::block_view::organism_settings_test_clean_prompt(view);
    runtime.surface_behavior.set(Behavior::Idle);
    runtime.ambient_display.set(AmbientBehavior::Idle);
    runtime.command_running.set(false);
    runtime.cancel_pointer_greeting();
    run(Host::BlockChat, |step| {
        let old_source = runtime
            .surface_timer
            .borrow()
            .as_ref()
            .and_then(|id| gtk::glib::MainContext::default().find_source_by_id(id));
        runtime.presence.focus_view(step.focused.then_some(view));
        // Feed accepted-input evidence, never a synthetic terminal commit.
        runtime.last_human_input.set(Some(if step.retreat {
            Instant::now()
        } else {
            Instant::now() - HUMAN_INPUT_RETREAT
        }));
        OrganismRuntime::apply_settings(runtime, view, step.enabled, step.motion);
        runtime.refresh_surface(view, Instant::now());
        assert!(
            crate::block_view::organism_settings_test_pty_bytes(view).is_empty(),
            "before settle: {}",
            step.name
        );
        settle(Duration::from_millis(120));
        runtime.refresh_surface(view, Instant::now());
        assert!(
            crate::block_view::organism_settings_test_pty_bytes(view).is_empty(),
            "after settle: {}",
            step.name
        );
        if !step.enabled {
            assert!(old_source.is_some_and(|source| source.is_destroyed()));
            assert!(
                runtime.surface_timer_deadline.get().is_none(),
                "retired epoch cannot rearm"
            );
        }
        Observed {
            primary_painted: runtime.body_position.get().is_some() && runtime.live_body.is_mapped(),
            card_visible: runtime.card.is_visible(),
            wake_pending: runtime.surface_timer.borrow().is_some(),
        }
    });
    OrganismRuntime::apply_settings(runtime, view, false, OrganismMotion::Static);
    runtime.presence.focus_view(None);
    assert!(crate::block_view::organism_settings_test_pty_bytes(view).is_empty());
}
