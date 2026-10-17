//! One native event sequence for both production adapters. This fixture does
//! not define a universal product presentation: capabilities stay host-specific.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum Host {
    Block,
    Chat,
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
    run_with_geometry(host, |step| (apply(step), true));
}

fn run_with_geometry(host: Host, mut apply: impl FnMut(Step) -> (Observed, bool)) {
    for step in TRACE {
        let (observed, idle_space) = apply(step);
        let common_visible = step.enabled && step.focused && !step.retreat;
        let (primary_painted, wake_pending) = match host {
            // Static keeps only Block's inline card. Its existing maintenance
            // heartbeat remains legitimate even without a spatial body.
            Host::Block | Host::Chat => (
                common_visible && step.motion != OrganismMotion::Static && idle_space,
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
    for host in [Host::Block, Host::Chat, Host::NeutralVte] {
        assert!(std::panic::catch_unwind(|| run(host, |_| Observed {
            primary_painted: false,
            card_visible: true,
            wake_pending: true,
        }))
        .is_err());
    }
}

pub(super) fn exercise_block(
    host: Host,
    allow_constrained: bool,
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
    assert!(matches!(host, Host::Block | Host::Chat));
    run_with_geometry(host, |step| {
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
        let metrics = view.live_organism_surface_metrics();
        let (_, body_width, _, _) = runtime.live_body.measure(Orientation::Horizontal, -1);
        let (_, body_height, _, _) = runtime.live_body.measure(Orientation::Vertical, body_width);
        let measured = SurfaceBox {
            width: metrics.width,
            height: metrics.height,
            right_gutter: metrics.right_gutter,
            cell_width: metrics.cell_width,
            cell_height: metrics.cell_height,
            body_width,
            body_height,
            cursor_row: metrics.cursor_row,
        };
        let idle_space = idle_space_available(measured);
        let eligible =
            step.enabled && step.focused && !step.retreat && step.motion != OrganismMotion::Static;
        if eligible {
            assert!(measured_surface_allocation(
                measured,
                view.widget().is_mapped()
            ));
            assert!(!metrics.alt_screen);
            if !allow_constrained {
                assert!(
                    idle_space,
                    "{host:?}: required visible control has space: {measured:?}"
                );
            }
            if !idle_space {
                assert!(runtime.no_safe_body_placement.get());
            }
        }
        println!(
            "ORGANISM_ADAPTER_GEOMETRY {host:?} {} {measured:?} fit={idle_space}",
            step.name
        );
        (
            Observed {
                primary_painted: runtime.body_position.get().is_some()
                    && runtime.live_body.is_mapped(),
                card_visible: runtime.card.is_visible(),
                wake_pending: runtime.surface_timer.borrow().is_some(),
            },
            idle_space,
        )
    });
    OrganismRuntime::apply_settings(runtime, view, false, OrganismMotion::Static);
    runtime.presence.focus_view(None);
    assert!(crate::block_view::organism_settings_test_pty_bytes(view).is_empty());
}

/// Independent fit oracle for the idle fixture, using native dimensions only.
/// Do not call the production placement/render function to predict its result.
fn idle_space_available(surface: SurfaceBox) -> bool {
    let cell_width = surface.cell_width.max(1);
    let cell_height = surface.cell_height.max(1);
    let margin_x = SURFACE_MARGIN.max(cell_width);
    let margin_y = SURFACE_MARGIN.max(cell_height);
    surface.body_width > 0
        && surface.body_height > 0
        && align_down(
            surface.width - surface.right_gutter - surface.body_width - margin_x,
            cell_width,
        ) >= align_up(margin_x, cell_width)
        && align_down(surface.height - surface.body_height - margin_y, cell_height)
            >= align_up(margin_y, cell_height)
}

#[test]
fn idle_fit_oracle_checks_native_size_and_grid_margins() {
    let exact = SurfaceBox {
        width: 132,
        height: 96,
        right_gutter: 16,
        cell_width: 8,
        cell_height: 16,
        body_width: 100,
        body_height: 64,
        cursor_row: 0,
    };
    assert!(idle_space_available(exact));
    assert!(!idle_space_available(SurfaceBox {
        width: 131,
        ..exact
    }));
    assert!(!idle_space_available(SurfaceBox {
        height: 95,
        ..exact
    }));
    assert!(!idle_space_available(SurfaceBox {
        body_height: 0,
        ..exact
    }));
    assert!(!idle_space_available(SurfaceBox {
        body_width: 0,
        ..exact
    }));
}

#[test]
fn constrained_chat_retains_card_and_wake_oracles() {
    run_with_geometry(Host::Chat, |step| {
        (
            Observed {
                primary_painted: false,
                card_visible: step.enabled,
                wake_pending: step.enabled,
            },
            false,
        )
    });
    assert!(
        std::panic::catch_unwind(|| run_with_geometry(Host::Chat, |step| (
            Observed {
                primary_painted: false,
                card_visible: false,
                wake_pending: step.enabled,
            },
            false
        )))
        .is_err()
    );
    assert!(
        std::panic::catch_unwind(|| run_with_geometry(Host::Chat, |step| (
            Observed {
                primary_painted: false,
                card_visible: step.enabled,
                wake_pending: false,
            },
            false
        )))
        .is_err()
    );
}

/// No persistence or network setup: only the window-owned state is seeded.
/// Attachment, capability rejection, settings and interaction remain the real
/// OrganismHub methods, including the early `remote` gate before eligibility.
fn fixture_hub() -> Rc<OrganismHub> {
    let config = Rc::new(RefCell::new(crate::config::Config {
        ascii_organism_enabled: true,
        ascii_organism_motion: Some(OrganismMotion::Calm),
        ..crate::config::Config::safe_defaults()
    }));
    let life = Rc::new(Cell::new(LifeState::default()));
    Rc::new(OrganismHub {
        config: config.clone(),
        desktop_motion_observer: RefCell::new(None),
        organism_memory: Rc::new(RefCell::new(None)),
        organism_life: life.clone(),
        organism_correction: OrganismCorrectionSignal::new(life.clone()),
        organism_activity: OrganismActivity::new(None, GrowthProgress::default()),
        organism_presence: OrganismPresence::new(),
        organism_agent: OrganismAgentSignal::new(life, config),
    })
}

/// Use the existing native runner/window, but independently construct and mount
/// each actual layout. Remote is a capability input, never an SSH connection.
pub(super) fn exercise_mounted_modes(
    window: &gtk::Window,
    overlay: &gtk::Overlay,
    settle: &impl Fn(Duration),
) {
    use crate::config::TerminalMode;
    let original = overlay.child().expect("original fixture content");
    let mut default_chat_cell_height = None;
    for (mode, host, font_desc, allow_constrained) in [
        (TerminalMode::Block, Host::Block, None, false),
        (TerminalMode::Chat, Host::Chat, None, true),
        (TerminalMode::Chat, Host::Chat, Some("monospace 32"), false),
    ] {
        for remote in [false, true] {
            println!("ORGANISM_MOUNTED_CASE {host:?} remote={remote} font={font_desc:?} constrained={allow_constrained}");
            let view = crate::block_view::organism_settings_test_view_for_mode(mode, font_desc);
            let recorder = crate::block_view::organism_settings_test_recorder(&view);
            let root = view.widget();
            let mounted = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let entry = gtk::Entry::new();
            mounted.append(&entry);
            mounted.append(&root);
            overlay.set_child(Some(&mounted));
            assert!(entry.grab_focus());
            settle(Duration::from_millis(120));
            assert!(
                root.is_mapped(),
                "{host:?} remote={remote}: mounted constructor"
            );
            assert!(root.width() > 0 && root.height() > 0);
            if matches!(host, Host::Chat) && !remote {
                let cell_height = view.live_organism_surface_metrics().cell_height;
                if font_desc.is_none() {
                    default_chat_cell_height = Some(cell_height);
                } else {
                    assert!(
                        cell_height > default_chat_cell_height.expect("default Chat measured"),
                        "spacious Chat must use the actual configured terminal font"
                    );
                }
            }
            crate::block_view::organism_settings_test_clean_prompt(&view);
            let hub = fixture_hub();
            hub.attach_ascii_organism_to_view(&view, remote);
            assert!(recorder
                .drain_test_slave(Duration::from_millis(40))
                .is_empty());
            if remote {
                // Exercise the actual gate and follow-up global settings path.
                // A remote view must never enter eligibility and later acquire
                // a local runtime through a settings refresh.
                for step in TRACE {
                    assert!(recorder
                        .drain_test_slave(Duration::from_millis(40))
                        .is_empty());
                    {
                        let mut config = hub.config.borrow_mut();
                        config.ascii_organism_enabled = step.enabled;
                        config.ascii_organism_motion = Some(step.motion);
                    }
                    hub.attach_ascii_organism_to_view(&view, true);
                    hub.sync_ascii_organism_settings();
                    hub.organism_presence
                        .focus_view(step.focused.then_some(&view));
                    assert!(
                        hub.say_hello(&view).is_err(),
                        "remote interaction: {}",
                        step.name
                    );
                    assert!(hub.organism_presence.eligible_views().is_empty());
                    assert!(hub.organism_presence.runtime_for(&view).is_none());
                    settle(Duration::from_millis(120));
                    assert!(hub.organism_presence.eligible_views().is_empty());
                    assert!(hub.organism_presence.runtime_for(&view).is_none());
                    assert!(recorder
                        .drain_test_slave(Duration::from_millis(40))
                        .is_empty());
                    println!("ORGANISM_REMOTE_TRACE {host:?} {} rejected", step.name);
                }
            } else {
                let runtime = hub
                    .organism_presence
                    .runtime_for(&view)
                    .expect("local attachment");
                assert_eq!(hub.organism_presence.eligible_views().len(), 1);
                settle(Duration::from_millis(120));
                assert!(runtime.card.is_mapped(), "{host:?}: actual inline card");
                exercise_block(host, allow_constrained, &runtime, &view, settle);
                // Drop a live Full source, not the already disabled trace tail.
                // Hidden-but-owned panes intentionally retain maintenance; this
                // verifies pane destruction, not a new unmap scheduler policy.
                OrganismRuntime::apply_settings(&runtime, &view, true, OrganismMotion::Full);
                hub.organism_presence.focus_view(Some(&view));
                runtime.refresh_surface(&view, Instant::now());
                assert!(entry.grab_focus());
                hub.revoke_organism_presence();
                let source = runtime
                    .surface_timer
                    .borrow()
                    .as_ref()
                    .and_then(|id| gtk::glib::MainContext::default().find_source_by_id(id))
                    .expect("live source before pane destruction");
                let weak_runtime = Rc::downgrade(&runtime);
                let weak_view = Rc::downgrade(&view);
                assert!(recorder
                    .drain_test_slave(Duration::from_millis(40))
                    .is_empty());
                mounted.remove(&root);
                assert!(
                    !root.is_mapped(),
                    "unmounted, before destroying the GTK owner"
                );
                drop(runtime);
                // The real Block component's shutdown calls view.kill().
                // Rc<TermView> drop alone leaves the PTY reader's callback
                // lists alive, including their runtime owners. Retire that
                // transport through the same public close path first.
                view.kill();
                drop(view);
                drop(root);
                settle(Duration::from_millis(120));
                assert!(weak_view.upgrade().is_none(), "{host:?}: pane released");
                assert!(
                    weak_runtime.upgrade().is_none(),
                    "{host:?}: runtime released"
                );
                assert!(
                    source.is_destroyed(),
                    "{host:?}: old epoch cannot resurrect"
                );
                assert!(hub.organism_presence.eligible_views().is_empty());
                assert!(recorder
                    .drain_test_slave(Duration::from_millis(40))
                    .is_empty());
                println!("ORGANISM_UNMOUNT_TRACE {host:?} retired after 120ms; zero PTY bytes");
                continue;
            }
            assert!(entry.grab_focus());
            let weak_view = Rc::downgrade(&view);
            mounted.remove(&root);
            assert!(!root.is_mapped());
            view.kill();
            drop(view);
            drop(root);
            settle(Duration::from_millis(120));
            assert!(weak_view.upgrade().is_none());
            assert!(hub.organism_presence.eligible_views().is_empty());
            assert!(recorder
                .drain_test_slave(Duration::from_millis(40))
                .is_empty());
            println!("ORGANISM_UNMOUNT_TRACE {host:?} remote retired after 120ms; zero PTY bytes");
        }
    }
    gtk::prelude::RootExt::set_focus(window, None::<&gtk::Widget>);
    overlay.set_child(Some(&original));
    settle(Duration::from_millis(120));
}
