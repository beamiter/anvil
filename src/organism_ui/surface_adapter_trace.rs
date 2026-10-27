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
            // Static uses the same single surface at frame zero. Semantic
            // maintenance remains separate from presentation ownership.
            Host::Block | Host::Chat => (common_visible && idle_space, step.enabled),
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
            observed.card_visible,
            matches!(host, Host::NeutralVte) && step.enabled && step.focused,
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
        let (_, body_width, _, _) = runtime
            .live_presentation
            .measure(Orientation::Horizontal, -1);
        let (_, body_height, _, _) = runtime
            .live_presentation
            .measure(Orientation::Vertical, body_width);
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
        let eligible = step.enabled && step.focused && !step.retreat;
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
fn constrained_chat_preserves_single_surface_and_wake_oracles() {
    run_with_geometry(Host::Chat, |step| {
        (
            Observed {
                primary_painted: false,
                card_visible: false,
                wake_pending: step.enabled,
            },
            false,
        )
    });
    assert!(
        std::panic::catch_unwind(|| run_with_geometry(Host::Chat, |step| (
            Observed {
                primary_painted: false,
                card_visible: step.enabled,
                wake_pending: step.enabled,
            },
            false
        )))
        .is_err()
    );
    assert!(
        std::panic::catch_unwind(|| run_with_geometry(Host::Chat, |_| (
            Observed {
                primary_painted: false,
                card_visible: false,
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
                assert!(
                    runtime.card.parent().is_none(),
                    "{host:?}: no duplicate inline card"
                );
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

#[test]
#[ignore = "requires DISPLAY; measures native Chat canvas, output and fixed64 body"]
fn chat_canvas_body_preserves_geometry_and_refuses_a_real_59px_viewport() {
    gtk::init().expect("native display");
    relm4::adw::init().expect("native theme");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../workspace_shell.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    let view = crate::block_view::organism_settings_test_view_for_mode(
        crate::config::TerminalMode::Chat,
        Some("DejaVu Sans Mono 32"),
    );
    let root = view.widget();
    let host = gtk::Box::new(Orientation::Vertical, 0);
    host.add_css_class("workspace-center");
    host.append(&root);
    let window = gtk::Window::builder()
        .title("Anvil companion visual acceptance fixture")
        .default_width(700)
        .default_height(600)
        .build();
    window.set_child(Some(&host));
    window.present();
    let settle = |duration: Duration| {
        let deadline = Instant::now() + duration;
        let context = gtk::glib::MainContext::default();
        while Instant::now() < deadline {
            for _ in 0..100 {
                if !context.iteration(false) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    settle(Duration::from_millis(200));
    crate::block_view::organism_settings_test_clean_prompt(&view);
    let hub = fixture_hub();
    hub.attach_ascii_organism_to_view(&view, false);
    hub.organism_presence.focus_view(Some(&view));
    let runtime = hub
        .organism_presence
        .runtime_for(&view)
        .expect("original owner");
    let recorder = crate::block_view::organism_settings_test_recorder(&view);
    runtime.refresh_surface(&view, Instant::now());
    settle(Duration::from_millis(120));
    runtime.refresh_surface(&view, Instant::now());
    assert!(
        runtime.body_position.get().is_some(),
        "real blank Chat canvas admits fixed64: metrics={:?} position={:?} no_safe={}",
        view.live_organism_surface_metrics(),
        runtime.body_position.get(),
        runtime.no_safe_body_placement.get()
    );
    assert_eq!(
        runtime
            .live_presentation
            .measure(Orientation::Horizontal, -1)
            .1,
        64
    );
    assert_eq!(
        runtime
            .live_presentation
            .measure(Orientation::Vertical, 64)
            .1,
        64
    );
    fn find_scroll(widget: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
        if widget.has_css_class("block-scroll") {
            return widget.clone().downcast().ok();
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(found) = find_scroll(&widget) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }
    let scroll = find_scroll(&root).expect("native retained viewport");
    let adjustment = scroll.vadjustment();
    let snapshot = || {
        (
            adjustment.value(),
            adjustment.upper(),
            adjustment.page_size(),
        )
    };
    let anchor = || {
        let list = scroll
            .child()
            .and_downcast::<gtk::Viewport>()
            .and_then(|v| v.child())
            .expect("document");
        let first = list.first_child().expect("stable document child");
        (
            first.as_ptr() as usize,
            first.compute_bounds(&scroll).expect("native anchor").y(),
        )
    };
    let before = snapshot();
    let before_anchor = anchor();
    for motion in [
        OrganismMotion::Full,
        OrganismMotion::Calm,
        OrganismMotion::Static,
    ] {
        OrganismRuntime::apply_settings(&runtime, &view, false, motion);
        OrganismRuntime::apply_settings(&runtime, &view, true, motion);
        runtime.refresh_surface(&view, Instant::now());
        settle(Duration::from_millis(100));
        assert_eq!(
            snapshot(),
            before,
            "body enable/move never scrolls the canvas"
        );
        assert_eq!(
            anchor(),
            before_anchor,
            "body enable/move preserves native reading anchor"
        );
        assert!(runtime.body_position.get().is_some());
    }
    // Explicit visual-fixture speech exercises the real renderer/callbacks; it
    // is never inserted into a production organism reaction or agent state.
    runtime.render(&Reaction {
        behavior: Behavior::Idle,
        tone: Tone::Quiet,
        description: "Visual acceptance fixture".into(),
        speech: Some("Visual acceptance fixture"),
    });
    runtime.refresh_surface(&view, Instant::now());
    settle(Duration::from_millis(120));
    assert!(runtime.live_speech_box.is_visible());
    assert_eq!(
        snapshot(),
        before,
        "expanding the speech component is non-measuring"
    );
    assert_eq!(anchor(), before_anchor);
    if let Ok(directory) = std::env::var("ANVIL_COMPANION_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("capture directory");
        let probe =
            super::surface_probe::SurfaceProbe::new(&window).expect("real X11 native capture");
        probe
            .capture(probe.full_rect().expect("native bounds"))
            .expect("pixels")
            .write_ppm(&directory.join("expanded-fixture.ppm"))
            .expect("save native pixels");
    }
    runtime
        .live_speech_box
        .last_child()
        .and_downcast::<gtk::Button>()
        .expect("native dismiss")
        .emit_clicked();
    runtime.refresh_surface(&view, Instant::now());
    settle(Duration::from_millis(100));
    assert!(!runtime.live_speech_box.is_visible());
    assert_eq!(snapshot(), before);
    assert_eq!(anchor(), before_anchor);
    // A real59px history viewport cannot admit a fixed64 contour. Preserve
    // the same owner, wake lifecycle and input authority while refusing paint.
    let original_height = window.height();
    for _ in 0..4 {
        let actual = view.live_organism_surface_metrics().height;
        if actual == 59 {
            break;
        }
        window.set_default_size(window.width(), (window.height() + 59 - actual).max(1));
        settle(Duration::from_millis(160));
    }
    assert_eq!(
        view.live_organism_surface_metrics().height,
        59,
        "actual allocated canvas"
    );
    runtime.refresh_surface(&view, Instant::now());
    settle(Duration::from_millis(100));
    assert!(runtime.body_position.get().is_none());
    assert!(!runtime.live_presentation.is_mapped());
    assert!(runtime.no_safe_body_placement.get());
    assert!(runtime.surface_timer.borrow().is_some());
    assert_eq!(hub.organism_presence.eligible_views().len(), 1);
    assert!(recorder
        .drain_test_slave(Duration::from_millis(30))
        .is_empty());
    window.set_default_size(window.width(), original_height);
    settle(Duration::from_millis(160));
    runtime.refresh_surface(&view, Instant::now());
    assert!(
        runtime.body_position.get().is_some(),
        "same owner returns in a real spacious canvas"
    );
    // A real native expander changes document geometry, never companion
    // allocation. Code expansion/collapse retains the top reading anchor.
    let expander = gtk::Expander::new(Some("Visual acceptance fixture · code details"));
    let code = gtk::Label::new(Some("Fixture code content"));
    code.set_height_request(120);
    expander.set_child(Some(&code));
    assert!(view.insert_inline_notice(expander.upcast_ref()));
    settle(Duration::from_millis(200));
    let first = anchor();
    runtime.refresh_surface(&view, Instant::now());
    settle(Duration::from_millis(80));
    let body = runtime
        .live_presentation
        .compute_bounds(&scroll)
        .expect("current body");
    let heading = expander
        .compute_bounds(&scroll)
        .expect("current code header");
    code.set_height_request(
        ((body.y() - heading.y() + 32.0) as i32)
            .min(scroll.height() - 48)
            .max(120),
    );
    let overlap = Rc::new(Cell::new(false));
    let observed = overlap.clone();
    let painted_body = runtime.live_presentation.clone();
    let painted_code = expander.clone();
    let painted_scroll = scroll.clone();
    let clock = window.frame_clock().expect("native frame clock");
    let paint_guard = clock.connect_after_paint(move |_| {
        if painted_body.is_mapped() {
            if let (Some(body), Some(code)) = (
                painted_body.compute_bounds(&painted_scroll),
                painted_code.compute_bounds(&painted_scroll),
            ) {
                if body.y() < code.y() + code.height() {
                    observed.set(true);
                }
            }
        }
    });
    for expanded in [true, false, true, false] {
        let value = adjustment.value();
        expander.set_expanded(expanded);
        settle(Duration::from_millis(160));
        assert!(
            !overlap.get(),
            "no painted frame may cover newly expanded code"
        );
        runtime.refresh_surface(&view, Instant::now());
        assert_eq!(adjustment.value(), value);
        assert_eq!(anchor(), first);
        if let Some(bounds) = runtime.live_presentation.compute_bounds(&scroll) {
            if runtime.live_presentation.is_mapped() {
                let code_bounds = expander.compute_bounds(&scroll).expect("real code bounds");
                assert!(bounds.y() >= code_bounds.y() + code_bounds.height());
            }
        }
    }
    clock.disconnect(paint_guard);
    assert_eq!(
        runtime.live_graphic.width(),
        64,
        "actual fixed contour allocation"
    );
    assert_eq!(
        runtime.live_graphic.height(),
        64,
        "actual fixed contour allocation"
    );
    let long = gtk::Box::new(Orientation::Vertical, 0);
    long.set_height_request(900);
    long.append(&gtk::Label::new(Some(
        "Visual acceptance fixture · long history",
    )));
    assert!(view.insert_inline_notice(long.upcast_ref()));
    settle(Duration::from_millis(200));
    // CommandStart is a new execution boundary; establish user reading after
    // it, then stream further output through the real retained-wheel owner.
    recorder.emit_test_output(b"\x1b]133;C;cmdline_url=printf%20fixture\x07");
    settle(Duration::from_millis(300));
    crate::block_view::organism_settings_test_read_at(&view, 120.0);
    settle(Duration::from_millis(160));
    assert_eq!(
        adjustment.value(),
        120.0,
        "actual precision-wheel reading position"
    );
    runtime.refresh_surface(&view, Instant::now());
    assert!(
        runtime.body_position.get().is_none(),
        "no body over real document/placeholder content"
    );
    let reading_value = adjustment.value();
    let reading_anchor = anchor();
    OrganismRuntime::apply_settings(&runtime, &view, false, OrganismMotion::Calm);
    OrganismRuntime::apply_settings(&runtime, &view, true, OrganismMotion::Calm);
    runtime.refresh_surface(&view, Instant::now());
    settle(Duration::from_millis(120));
    assert_eq!(adjustment.value(), reading_value);
    assert_eq!(anchor(), reading_anchor);
    recorder.emit_test_output(b"fixture output\r\n");
    settle(Duration::from_millis(180));
    assert!(
        !runtime.live_presentation.is_mapped(),
        "output hides before changed geometry paints"
    );
    assert_eq!(
        adjustment.value(),
        reading_value,
        "output never steals history reading"
    );
    assert_eq!(anchor(), reading_anchor);
    assert!(recorder
        .drain_test_slave(Duration::from_millis(30))
        .is_empty());
    OrganismRuntime::apply_settings(&runtime, &view, false, OrganismMotion::Static);
    hub.revoke_organism_presence();
    view.kill();
    window.close();
}
