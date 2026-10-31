use relm4::gtk;
#[path = "gtk_close.rs"]
mod close_rules;
#[path = "eva_companion_scope.rs"]
mod scope_rules;
// One window-owned shared body. No PTY, output text, history or audio access.
use companion_host::{self as host, atlas, Presence, Rect, Request};
use gtk::prelude::*;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

struct State {
    client: host::SharedHost,
    generation: u64,
    last_task: Option<(String, u64, host::TaskStatus)>,
    active_turn: Option<(String, host::ContextRef)>,
    dialogue_save: host::DialogueSave,
    history_rendered: u64,
    voice_text_rendered: u64,
    pending_displayed: Option<String>,
    body: gtk::glib::WeakRef<gtk::Label>,
    close: close_rules::CloseIntent,
    close_work: Option<Box<dyn Fn() -> bool>>,
    on_close_ready: Option<Rc<dyn Fn(u64)>>,
    on_close_cancel: Option<Box<dyn FnOnce()>>,
    cancel_key: host::CancelKeyLease,
    close_focus: gtk::glib::WeakRef<gtk::Widget>,
    pressed_keys: std::collections::HashSet<u32>,
    admission_generation: u64,
    current_context: Option<host::ContextRef>,
    context_serial: u64,
    pending_recall: bool,
    selection_request: Option<(host::ContextRef, String)>,
    selection_review: Option<host::SelectionEvidence>,
    approved_selection: Option<host::SelectionEvidence>,
    memory_context: Option<host::ContextRef>,
    memory_review: Option<(host::ContextRef, host::MemoryReview)>,
    memory_entry: Option<host::MemoryEntry>,
    memory_next: Option<i64>,
    managed_scope: Option<host::MemoryScope>,
    scope_request: Option<(host::ContextRef, host::MemoryScope)>,
    scope_review: Option<(host::ContextRef, host::MemoryScope, String, String)>,
    task_scopes: Vec<host::TaskScopeInfo>,
    next_task_scope: Option<String>,
    memory_scopes: Vec<host::MemoryScope>,
    next_memory_scope: Option<String>,
    assigned_project: Option<(String, Option<String>, String)>,
    listed_scope: Option<host::MemoryScope>,
    skin: atlas::Skin,
    motion: atlas::Motion,
    started: Instant,
    last_poll: Instant,
    last_pose: String,
    next: Option<Instant>,
    input_until: Instant,
    selecting: bool,
    blocked: bool,
    geometry: (i32, i32, bool, i32, i32, i32, i32),
    last_exclusions: Vec<Rect>,
    compact: bool,
    high_contrast: bool,
    frame: Option<atlas::Frame>,
    full_markup: String,
    compact_markup: String,
    pango_serial: u32,
    pango_context_id: usize,
    revision: u64,
    pending_frames: crate::gtk_presentation::PendingFrames<FrameDomain>,
    frame_sequence: crate::gtk_presentation::FrameSequence<FrameDomain>,
    frame_clock_binding: Option<FrameClockBinding>,
    surface_lease: crate::gtk_presentation::SurfaceLease<gtk::gdk::Surface>,
}

#[derive(Clone, PartialEq)]
struct FrameDomain {
    clock: gtk::gdk::FrameClock,
    surface: gtk::gdk::Surface,
}

struct FrameClockBinding {
    domain: FrameDomain,
    before: Option<gtk::glib::SignalHandlerId>,
    after: Option<gtk::glib::SignalHandlerId>,
}

impl Drop for FrameClockBinding {
    fn drop(&mut self) {
        if let Some(id) = self.before.take() {
            self.domain.clock.disconnect(id);
        }
        if let Some(id) = self.after.take() {
            self.domain.clock.disconnect(id);
        }
    }
}

fn refresh_frame_clock(state: &Rc<RefCell<State>>, overlay: &gtk::Overlay, body: &gtk::Label) {
    let domain = overlay
        .is_mapped()
        .then(|| {
            Some(FrameDomain {
                clock: overlay.frame_clock()?,
                surface: overlay
                    .native()?
                    .surface()
                    .filter(|surface| !surface.is_destroyed())?,
            })
        })
        .flatten();
    let Ok(mut s) = state.try_borrow_mut() else {
        return;
    };
    if !s.frame_sequence.select(domain.clone()) {
        return;
    }
    s.pending_frames.clear();
    s.frame_clock_binding = None;
    s.geometry.0 = -1;
    s.geometry.1 = -1;
    body.set_visible(false);
    let Some(domain) = domain else {
        log::debug!(target: "eva_native_host", "GTK native clock detached; no retirement receipt inferred");
        return;
    };
    drop(s);
    let weak = Rc::downgrade(state);
    let surface = domain.surface.downgrade();
    let before = domain.clock.connect_before_paint(move |clock| {
        let (Some(state), Some(surface)) = (weak.upgrade(), surface.upgrade()) else { return; };
        if surface.is_destroyed() { return; }
        let Ok(mut s) = state.try_borrow_mut() else { return; };
        let domain = FrameDomain { clock: clock.clone(), surface };
        if let Some(stamp) = s.frame_sequence.begin(&domain, clock.frame_counter()) {
            s.client.begin_frame(stamp.serial());
            log::debug!(target: "eva_native_host", "GTK frame began frame={} serial={} native_generation={}", clock.frame_counter(), stamp.serial(), stamp.generation());
        }
    });
    let weak = Rc::downgrade(state);
    let surface = domain.surface.downgrade();
    let weak_body = body.downgrade();
    let after = domain.clock.connect_after_paint(move |clock| {
        let (Some(state), Some(surface), Some(body)) = (weak.upgrade(), surface.upgrade(), weak_body.upgrade()) else { return; };
        if surface.is_destroyed() { return; }
        let Ok(mut s) = state.try_borrow_mut() else { return; };
        let domain = FrameDomain { clock: clock.clone(), surface };
        let mapped = body.is_mapped() && body.native().and_then(|native| native.surface()).as_ref() == Some(&domain.surface);
        if mapped {
            let Some(token) = s.client.gate.token().cloned() else { return; };
            if !s.surface_lease.record_body(token, domain.surface.clone()) { return; }
        } else if !s.client.gate.pending_retirement_boundary().is_some_and(|(token, _)| s.surface_lease.can_clear(token, &domain.surface)) {
            return;
        }
        let Some(stamp) = s.frame_sequence.after_paint(&domain, clock.frame_counter()) else { return; };
        if let Some(frame) = crate::gtk_presentation::CapturedFrame::capture(stamp, mapped, &s.client.gate) {
            if s.pending_frames.record(domain, frame.clone()) {
                if frame.is_retirement() {
                    log::debug!(target: "eva_native_host", "GTK clearance submitted frame={}", frame.frame());
                }
                log::debug!(target: "eva_native_host", "GTK frame captured {frame:?}");
            }
        }
    });
    log::debug!(target: "eva_native_host", "GTK native clock bound surface={:p} clock={:p}", domain.surface.as_ptr(), domain.clock.as_ptr());
    state.borrow_mut().frame_clock_binding = Some(FrameClockBinding {
        domain,
        before: Some(before),
        after: Some(after),
    });
    overlay.queue_draw();
}

/// Called exactly once per GUI, never from pane construction or paint.
pub(crate) fn install(window: &gtk::Window, overlay: &gtk::Overlay, app: &str) {
    if !shared_mode_requested() {
        return;
    }
    let body = gtk::Label::new(None);
    body.set_halign(gtk::Align::Start);
    body.set_valign(gtk::Align::Start);
    body.set_margin_top(0);
    body.set_margin_start(0);
    body.set_xalign(0.);
    body.set_yalign(0.);
    body.set_can_target(false);
    body.set_focusable(false);
    body.set_visible(false);
    let clip = crate::eva_companion_clip::BodyClip::new(&body);
    overlay.add_overlay(&clip);
    overlay.set_measure_overlay(&clip, false);
    overlay.set_clip_overlay(&clip, true);
    let state = Rc::new(RefCell::new(State {
        client: host::SharedHost::from_env(app, host::RetirementCapability::NativePresented, || {}),
        generation: 0,
        last_task: None,
        active_turn: None,
        dialogue_save: Default::default(),
        history_rendered: 0,
        voice_text_rendered: 0,
        pending_displayed: None,
        body: body.downgrade(),
        close: close_rules::CloseIntent::default(),
        close_work: None,
        on_close_ready: None,
        on_close_cancel: None,
        cancel_key: host::CancelKeyLease::default(),
        close_focus: gtk::glib::WeakRef::new(),
        pressed_keys: std::collections::HashSet::new(),
        admission_generation: 0,
        current_context: None,
        context_serial: 0,
        pending_recall: false,
        selection_request: None,
        selection_review: None,
        approved_selection: None,
        memory_context: None,
        memory_review: None,
        memory_entry: None,
        memory_next: None,
        managed_scope: None,
        scope_request: None,
        scope_review: None,
        task_scopes: Vec::new(),
        next_task_scope: None,
        memory_scopes: Vec::new(),
        next_memory_scope: None,
        assigned_project: None,
        listed_scope: None,
        skin: std::env::var("EVA_COMPANION_SKIN")
            .ok()
            .and_then(|s| atlas::Skin::from_key(&s))
            .unwrap_or(atlas::Skin::Cat),
        motion: if app == "anvil" {
            atlas::Motion::Static
        } else {
            atlas::Motion::Calm
        },
        started: Instant::now(),
        last_poll: Instant::now(),
        last_pose: String::new(),
        next: None,
        input_until: Instant::now(),
        selecting: false,
        blocked: false,
        geometry: (0, 0, false, 0, 0, 0, 0),
        last_exclusions: Vec::new(),
        compact: false,
        high_contrast: false,
        frame: None,
        full_markup: String::new(),
        compact_markup: String::new(),
        pango_serial: 0,
        pango_context_id: 0,
        revision: 0,
        pending_frames: crate::gtk_presentation::PendingFrames::default(),
        frame_sequence: crate::gtk_presentation::FrameSequence::default(),
        frame_clock_binding: None,
        surface_lease: crate::gtk_presentation::SurfaceLease::default(),
    }));
    {
        let weak = Rc::downgrade(&state);
        window.connect_focus_widget_notify(move |window| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let Some(focus) = gtk::prelude::GtkWindowExt::focus(window) else {
                return;
            };
            let pane = if has_remote_ancestor(&focus) {
                "remote-unavailable".to_owned()
            } else {
                let Some(pane) = native_pane_id(&focus) else {
                    return;
                };
                pane
            };
            let mut s = state.borrow_mut();
            if s.current_context
                .as_ref()
                .is_some_and(|c| c.pane_id == pane)
            {
                return;
            }
            let context = host::ContextRef {
                host_id: s.client.gate.host_id().into(),
                pane_id: pane,
                task_id: None,
                project_id: None,
                revision: 0,
            };
            s.register_context(context);
        });
    }
    HOSTS.with(|hosts| {
        let mut h = hosts.borrow_mut();
        h.retain(|(w, s)| w.upgrade().is_some() && s.upgrade().is_some());
        h.push((window.downgrade(), Rc::downgrade(&state)));
    });
    // A plain, keyboard-accessible menu is not a second living avatar.
    let menu = gtk::MenuButton::builder()
        .label("Eva")
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_end(24)
        .build();
    let pop = gtk::Popover::new();
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
    controls.add_css_class("eva-companion-card");
    let status = gtk::Label::new(Some("Eva connecting — shared body hidden"));
    status.set_wrap(true);
    status.set_max_width_chars(42);
    controls.append(&status);
    for (title, request) in [
        (
            "Follow me",
            Request::Presence {
                mode: Presence::FollowMe,
            },
        ),
        (
            "Stay here",
            Request::Presence {
                mode: Presence::StayHere,
            },
        ),
        (
            "Hide",
            Request::Presence {
                mode: Presence::Hidden,
            },
        ),
        ("Recall Eva", Request::Recall),
    ] {
        let button = gtk::Button::with_label(title);
        let state = state.clone();
        let pop = pop.clone();
        button.connect_clicked(move |_| {
            if matches!(request, Request::Recall) {
                {
                    let mut s = state.borrow_mut();
                    s.pending_recall = true;
                    s.input_until = Instant::now();
                    s.selecting = false;
                }
                pop.popdown();
            } else {
                state.borrow_mut().client.send(request.clone());
            }
        });
        controls.append(&button);
    }
    let applying = Rc::new(std::cell::Cell::new(false));
    let skin = gtk::DropDown::from_strings(&atlas::Skin::ALL.map(|s| s.name()));
    skin.set_selected(state.borrow().skin as u32);
    skin.set_tooltip_text(Some("Eva appearance — same identity"));
    {
        let state = state.clone();
        let applying = applying.clone();
        skin.connect_selected_notify(move |s| {
            if applying.get() {
                return;
            }
            let mut st = state.borrow_mut();
            if let Some(s) = atlas::Skin::ALL.get(s.selected() as usize) {
                let motion = motion_key(st.motion).to_owned();
                st.client.send(Request::Appearance {
                    skin: s.key().into(),
                    motion,
                });
                st.next = Some(Instant::now());
            }
        });
    }
    controls.append(&skin);
    let motion = gtk::DropDown::from_strings(&["Full motion", "Calm motion", "Static"]);
    motion.set_selected(if app == "anvil" { 2 } else { 1 });
    motion.set_tooltip_text(Some(
        "Motion; desktop reduced-motion preference always wins",
    ));
    {
        let state = state.clone();
        let applying = applying.clone();
        motion.connect_selected_notify(move |m| {
            if applying.get() {
                return;
            }
            let mut st = state.borrow_mut();
            st.motion = match m.selected() {
                0 => atlas::Motion::Full,
                1 => atlas::Motion::Calm,
                _ => atlas::Motion::Static,
            };
            let skin = st.skin.key().to_owned();
            let motion = motion_key(st.motion).to_owned();
            st.client.send(Request::Appearance { skin, motion });
            st.next = Some(Instant::now());
        });
    }
    controls.append(&motion);
    let contrast = gtk::CheckButton::with_label("High contrast ASCII");
    {
        let state = state.clone();
        contrast.connect_toggled(move |c| {
            let mut s = state.borrow_mut();
            s.high_contrast = c.is_active();
            s.next = Some(Instant::now());
        });
    }
    controls.append(&contrast);
    let dialogue = gtk::Label::new(Some("Ask Eva here. Terminal output is not attached."));
    dialogue.set_wrap(true);
    dialogue.set_max_width_chars(42);
    dialogue.set_selectable(true);
    controls.append(&dialogue);
    let earlier = gtk::Label::new(None);
    earlier.set_wrap(true);
    earlier.set_max_width_chars(42);
    controls.append(&earlier);
    let history = history_controls(&controls, &state);
    let input = gtk::Entry::builder()
        .placeholder_text("Message Eva — never sent to the shell")
        .max_length(8192)
        .build();
    controls.append(&input);
    {
        let state = state.clone();
        let dialogue = dialogue.clone();
        input.connect_activate(move |entry| {
            let text = entry.text().to_string();
            if text.trim().is_empty() {
                return;
            }
            if text.len() > 8192 {
                dialogue.set_text("Message exceeds 8192 bytes.");
                return;
            }
            let mut s = state.borrow_mut();
            let turn_id = uuid::Uuid::new_v4().to_string();
            let mut context = host::ContextRef {
                host_id: s.client.gate.host_id().to_owned(),
                pane_id: "companion-card".into(),
                task_id: None,
                project_id: None,
                revision: s.revision,
            };
            if let Some(current) = &s.current_context {
                context = current.clone();
            }
            let approved_selection = s.approved_selection.take();
            if let Some(e) = &approved_selection {
                context = e.context.clone();
            }
            if approved_selection
                .as_ref()
                .is_some_and(|e| s.current_context.as_ref() != Some(&e.context))
            {
                dialogue.set_text("Selection context changed; review it again before sending.");
                return;
            }
            let context = s.register_context(context);
            s.pending_displayed = None;
            s.active_turn = Some((turn_id.clone(), context.clone()));
            s.dialogue_save.begin(&turn_id, &context, &text);
            s.client.send(Request::DialogueStart {
                turn_id,
                context,
                text,
                approved_selection,
            });
            entry.set_text("");
            dialogue.set_text("Eva is answering · no terminal text attached");
        });
    }
    let selection_label = gtk::Label::new(Some("Selected text is never read automatically."));
    selection_label.set_wrap(true);
    selection_label.set_max_width_chars(42);
    selection_label.set_selectable(true);
    controls.append(&selection_label);
    let read_selection = gtk::Button::with_label("Review selected terminal text");
    {
        let state = state.clone();
        let overlay = overlay.downgrade();
        let label = selection_label.clone();
        read_selection.connect_clicked(move |_|{
        let Some(overlay)=overlay.upgrade()else{return;};let Some((pane_id,text))=capture_one_selection(overlay.upcast_ref())else{label.set_text("Select text in exactly one visible local VTE surface. Remote or ambiguous selections are unavailable.");return;};
        let mut s=state.borrow_mut();let context=host::ContextRef{host_id:s.client.gate.host_id().into(),pane_id,task_id:None,project_id:None,revision:s.revision};let source_event_id=uuid::Uuid::new_v4().to_string();s.current_context=None;let context=s.register_context(context);s.selection_request=Some((context.clone(),source_event_id.clone()));s.approved_selection=None;s.selection_review=None;
        let captured_at_ms=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        s.client.send(Request::ContextAccessRequested{context,text,source_event_id,captured_at_ms});label.set_text("Preparing exact selection review — not sent to a model.");
    });
    }
    controls.append(&read_selection);
    let approve_selection = gtk::Button::with_label("Approve this exact text once");
    approve_selection.set_sensitive(false);
    {
        let state = state.clone();
        let label = selection_label.clone();
        approve_selection.connect_clicked(move |b| {
            let mut s = state.borrow_mut();
            if let Some(e) = s.selection_review.take() {
                s.client.send(Request::ContextAccessGranted {
                    context: e.context.clone(),
                    consent_id: e.consent_id.clone(),
                });
                s.approved_selection = Some(e);
                label.set_text(
                    "Approved for your next Eva message only. No task or shell action authorized.",
                );
            }
            b.set_sensitive(false);
        });
    }
    controls.append(&approve_selection);
    let cancel = gtk::Button::with_label("Stop reply (not task)");
    {
        let state = state.clone();
        cancel.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            if let Some((turn_id, _)) = s.active_turn.clone() {
                s.client.send(Request::DialogueCancel { turn_id });
            }
        });
    }
    controls.append(&cancel);
    let memory = memory_controls(&controls, &state);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_max_content_height(560);
    scroll.set_min_content_width(440);
    scroll.set_max_content_width(560);
    scroll.set_propagate_natural_width(true);
    scroll.set_propagate_natural_height(true);
    scroll.set_child(Some(&controls));
    pop.set_child(Some(&scroll));
    menu.set_popover(Some(&pop));
    overlay.add_overlay(&menu);
    overlay.set_measure_overlay(&menu, false);
    let close_bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    close_bar.set_halign(gtk::Align::Center);
    close_bar.set_valign(gtk::Align::Start);
    close_bar.add_css_class("eva-companion-card");
    close_bar.add_css_class("card");
    close_bar.append(&gtk::Label::new(Some(
        "Closing window — waiting for Eva to leave",
    )));
    let keep = gtk::Button::with_label("Keep window open");
    {
        let state = state.clone();
        keep.connect_clicked(move |_| {
            let (focus, revision) = {
                let mut s = state.borrow_mut();
                if !s.close.waiting() {
                    return;
                }
                let saved = (s.close_focus.clone(), s.close.revision());
                s.cancel_close();
                saved
            };
            restore_close_focus(&state, focus, revision);
        });
    }
    close_bar.append(&keep);
    close_bar.set_visible(false);
    overlay.add_overlay(&close_bar);
    overlay.set_measure_overlay(&close_bar, false);
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let state = state.clone();
        let body = body.clone();
        keys.connect_key_pressed(move |_, key, code, _| {
            let mut s = state.borrow_mut();
            let repeat = !s.pressed_keys.insert(code);
            let busy = s.close_work.as_ref().is_some_and(|probe| probe());
            if s.close.should_cancel_for_work(busy) {
                s.cancel_close();
            }
            if key == gtk::gdk::Key::Escape {
                let available = s.close.waiting() && !busy;
                match s.cancel_key.press(available, repeat) {
                    host::CancelKeyAction::Cancel => {
                        let focus = s.close_focus.clone();
                        let revision = s.close.revision();
                        s.cancel_close();
                        drop(s);
                        restore_close_focus(&state, focus, revision);
                        return gtk::glib::Propagation::Stop;
                    }
                    host::CancelKeyAction::Suppress => return gtk::glib::Propagation::Stop,
                    host::CancelKeyAction::Pass => {}
                }
            } else {
                s.cancel_key.other_input();
            }
            // New native input revokes GUI-exit permission while continuing
            // through the original terminal/control. Clearance stays intact.
            if s.close.waiting() {
                s.cancel_close();
            }
            s.input_until = Instant::now() + Duration::from_secs(3);
            let selecting = s.selecting;
            s.client.send(Request::Input {
                active: true,
                selecting,
            });
            body.set_visible(false);
            gtk::glib::Propagation::Proceed
        });
    }
    {
        let state = state.clone();
        let window = window.downgrade();
        keys.connect_key_released(move |_, key, code, _| {
            let mut s = state.borrow_mut();
            s.pressed_keys.remove(&code);
            if key == gtk::gdk::Key::Escape {
                s.cancel_key
                    .release(window.upgrade().is_some_and(|w| w.is_active()));
            }
        });
    }
    window.add_controller(keys);
    let clicks = gtk::GestureClick::new();
    clicks.set_button(0);
    clicks.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let state = state.clone();
        let body = body.clone();
        clicks.connect_pressed(move |_, _, _, _| {
            state.borrow_mut().selecting = true;
            body.set_visible(false);
        });
    }
    {
        let state = state.clone();
        clicks.connect_released(move |_, _, _, _| {
            let mut s = state.borrow_mut();
            s.selecting = false;
            s.input_until = Instant::now() + Duration::from_secs(3);
        });
    }
    window.add_controller(clicks);
    {
        let state = state.clone();
        window.connect_is_active_notify(move |w| {
            let mut s = state.borrow_mut();
            if !w.is_active() {
                s.cancel_key.focus_lost();
                s.pressed_keys.clear();
            }
            s.client.send(Request::Focus {
                focused: w.is_active(),
            })
        });
    }
    // Each mapped native surface gets its own signal binding. Missing clocks
    // are retried by the normal poll; neither map nor a poll acknowledges paint.
    for mapped in [true, false] {
        let state = Rc::downgrade(&state);
        let body = body.downgrade();
        let changed = move |overlay: &gtk::Overlay| {
            if let (Some(state), Some(body)) = (state.upgrade(), body.upgrade()) {
                refresh_frame_clock(&state, overlay, &body);
            }
        };
        if mapped {
            overlay.connect_map(changed);
        } else {
            overlay.connect_unmap(changed);
        }
    }
    let mut full_layout = body.create_pango_layout(None);
    let mut compact_layout = body.create_pango_layout(None);
    let window = window.downgrade();
    let overlay = overlay.downgrade();
    let body = body.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
        let (Some(window), Some(overlay), Some(body)) =
            (window.upgrade(), overlay.upgrade(), body.upgrade())
        else {
            return gtk::glib::ControlFlow::Break;
        };
        refresh_frame_clock(&state, &overlay, &body);
        let mut s = state.borrow_mut();
        let now = Instant::now();
        if now.duration_since(s.last_poll) > Duration::from_secs(2) {
            s.started = now;
            s.next = Some(now);
        }
        s.last_poll = now;
        let frames = s.pending_frames.take();
        for (origin, frame) in frames {
            if !s.frame_sequence.accepts(&origin, frame.stamp())
                || !frame.is_current(&s.client.gate)
            {
                continue;
            }
            if let Some(t) = origin.clock.timings(frame.frame()) {
                if presentation_evidence(t.is_complete(), t.presentation_time()) {
                    if frame.prepare(&mut s.client.gate) {
                        log::debug!(target: "eva_native_host", "GTK presentation receipt frame={} presentation_time={} retirement={} serial={} native_generation={} surface={:p} clock={:p} token={:?}", frame.frame(), t.presentation_time(), frame.is_retirement(), frame.serial(), frame.stamp().generation(), origin.surface.as_ptr(), origin.clock.as_ptr(), s.client.gate.possible_body_token());
                        s.client.frame_presented(frame.serial());
                    }
                } else if !t.is_complete() {
                    s.pending_frames.record(origin, frame);
                } else {
                    log::debug!(target: "eva_native_host", "GTK completed timing has no presentation evidence frame={}", frame.frame());
                }
            } else {
                log::debug!(target: "eva_native_host", "GTK timing unavailable frame={}", frame.frame());
            }
        }
        let prior_body = if log::log_enabled!(target: "eva_native_host", log::Level::Debug) {
            s.client.gate.possible_body_token().cloned()
        } else {
            None
        };
        let changed = s.client.poll();
        {
            let State {
                surface_lease,
                client,
                ..
            } = &mut *s;
            surface_lease.refresh(&client.gate);
        }
        if changed {
            log::debug!(target: "eva_native_host", "GTK state observed frame={} token={:?} retirement={:?} connection={} admission={}", overlay.frame_clock().map(|c| c.frame_counter()).unwrap_or(-1), s.client.gate.token(), s.client.gate.pending_retirement_boundary(), s.client.gate.connection_generation(), s.client.gate.admission_generation());
        }
        if let Some(token) = prior_body.filter(|token| s.client.gate.retirement_accepted(token)) {
            log::debug!(target: "eva_native_host", "GTK retirement acknowledged token={token:?}");
        }
        if changed && s.client.gate.pending_retirement() {
            log::debug!(target: "eva_native_host", "GTK retirement awaiting native presentation boundary={:?}", s.client.gate.pending_retirement_boundary());
        }
        while let Some(host::Reply::Dialogue {
            turn_id,
            context,
            status: reply_status,
            reply,
        }) = s.client.take_dialogue()
        {
            s.dialogue_save
                .observe(&turn_id, &context, &reply_status, reply.as_deref());
            if s.active_turn.as_ref() == Some(&(turn_id.clone(), context.clone())) {
                dialogue.set_text(&format!(
                    "Eva · {} · source {} / {}\n{}",
                    reply_status,
                    context.host_id,
                    context.pane_id,
                    reply.as_deref().unwrap_or("")
                ));
                if reply.is_some() {
                    s.pending_displayed = Some(turn_id);
                }
            } else {
                earlier.set_text(&format!(
                    "Earlier turn {} · source {} / {} · {}\n{}",
                    turn_id,
                    context.host_id,
                    context.pane_id,
                    reply_status,
                    reply.as_deref().unwrap_or("")
                ));
            }
        }
        if dialogue.is_mapped() && window.is_mapped() {
            if let Some(turn_id) = s.pending_displayed.take() {
                s.client.send(Request::DialogueDisplayed { turn_id });
            }
        }
        {
            let current = s.current_context.clone();
            s.client.history.context_changed(current.as_ref());
            let revision = s.client.history.revision();
            if revision != s.history_rendered {
                s.history_rendered = revision;
                history.render(&s.client.history, current.as_ref());
            }
            let revision = s.client.voice_text.revision();
            if revision != s.voice_text_rendered {
                s.voice_text_rendered = revision;
                memory.voice_status.set_text(s.client.voice_text.status());
            }
        }

        while let Some(host::Reply::ContextAccessReview { evidence }) =
            s.client.take_context_access()
        {
            if s.selection_request.as_ref()
                == Some(&(evidence.context.clone(), evidence.source_event_id.clone()))
            {
                selection_label.set_text(&format!("Review exact selection · source {} / {} · {} bytes\n{}\nCheck for secrets before approving.",evidence.context.host_id,evidence.context.pane_id,evidence.text.len(),evidence.text));
                s.selection_review = Some(evidence);
                approve_selection.set_sensitive(true);
            }
        }
        while let Some(reply) = s.client.take_memory() {
            match reply {
                host::Reply::MemoryScopes { scopes, next_after } => {
                    let names: Vec<String> = scopes.iter().map(|s| format!("{:?}", s)).collect();
                    memory.persisted.set_model(Some(&gtk::StringList::new(
                        &names.iter().map(String::as_str).collect::<Vec<_>>(),
                    )));
                    s.memory_scopes = scopes;
                    s.next_memory_scope = next_after;
                }
                host::Reply::TaskScopes { scopes, next_after } => {
                    let names: Vec<String> = scopes
                        .iter()
                        .map(|i| {
                            format!(
                                "Task {} · project {} · owner {}",
                                i.task_id,
                                i.project_id.as_deref().unwrap_or("none"),
                                i.owner_host_id
                            )
                        })
                        .collect();
                    memory.discovered.set_model(Some(&gtk::StringList::new(
                        &names.iter().map(String::as_str).collect::<Vec<_>>(),
                    )));
                    s.task_scopes = scopes;
                    s.next_task_scope = next_after;
                }
                host::Reply::MemoryScopeReview {
                    context,
                    scope,
                    review_id,
                    digest,
                } => {
                    if scope_rules::accepts_review(
                        s.current_context.as_ref(),
                        s.scope_request.as_ref(),
                        &context,
                        &scope,
                    ) {
                        memory.scope_status.set_text(&format!("Review management-only access to {:?}\nOrigin {} / {} · revision {}\nDigest {}\nAllows inspect/edit/delete/clear. Never adopts this task or permits model retrieval.", scope, context.host_id, context.pane_id, context.revision, digest));
                        s.scope_review = Some((context, scope, review_id, digest));
                        memory.scope_approve.set_sensitive(true);
                    }
                }
                host::Reply::MemoryList {
                    context,
                    scope,
                    entries,
                    next_before_id,
                    paused,
                } => {
                    if s.memory_context.as_ref() == Some(&context)
                        && s.current_context.as_ref() == Some(&context)
                        && s.listed_scope.as_ref() == Some(&scope)
                    {
                        memory.approve.set_sensitive(false);
                        s.memory_next = next_before_id;
                        s.memory_entry = entries.into_iter().next();
                        if let Some(e) = &s.memory_entry {
                            memory.status.set_text(&format!("Memory {} · {:?} · revision {} · source {} · consent {} · writes paused {}\n{} = {}",e.id,e.scope,e.revision,e.source_event_id,e.consent_status,paused,e.key,e.value));
                            memory.key.set_text(&e.key);
                            memory.value.set_text(&e.value);
                        } else {
                            memory.status.set_text("No memory entries in this scope.");
                        }
                    }
                }
                host::Reply::MemoryReview { context, review } => {
                    if s.memory_context.as_ref() == Some(&context)
                        && s.current_context.as_ref() == Some(&context)
                    {
                        memory.status.set_text(&format!(
                            "Review exact change · source {} / {}\n{:#?}\nDigest {}",
                            context.host_id, context.pane_id, review.change, review.digest
                        ));
                        s.memory_review = Some((context, review));
                        memory.approve.set_sensitive(true);
                    }
                }
                host::Reply::MemoryResult {
                    review_id,
                    status: result,
                } => {
                    if (review_id.is_none() && s.memory_review.is_none())
                        || s.memory_review
                            .as_ref()
                            .is_some_and(|(_, r)| Some(r.id) == review_id)
                    {
                        memory
                            .status
                            .set_text(&format!("Memory result: {}", result));
                        memory.approve.set_sensitive(false);
                        s.memory_review = None;
                    }
                }
                _ => {}
            }
        }
        let generation = s.client.gate.connection_generation();
        let admission = s.client.gate.admission_generation();
        if generation != s.generation || admission != s.admission_generation {
            s.generation = generation;
            s.admission_generation = admission;
            s.geometry = (0, 0, false, 0, 0, 0, 0);
            s.last_exclusions.clear();
            s.last_task = None;
            s.current_context = None;
            s.dialogue_save.clear();
            if let Some(pane_id) =
                gtk::prelude::GtkWindowExt::focus(&window).and_then(|focus| native_pane_id(&focus))
            {
                let context = host::ContextRef {
                    host_id: s.client.gate.host_id().into(),
                    pane_id,
                    task_id: None,
                    project_id: None,
                    revision: 0,
                };
                s.register_context(context);
            }
            s.invalidate_memory_management();
            s.memory_review = None;
            s.selection_request = None;
            s.selection_review = None;
            s.approved_selection = None;
            approve_selection.set_sensitive(false);
            s.memory_context = None;
            s.memory_entry = None;
            memory.approve.set_sensitive(false);
            s.client.send(Request::Input {
                active: true,
                selecting: false,
            });
            s.blocked = true;
        }
        if changed {
            s.skin = atlas::Skin::from_key(s.client.gate.skin()).unwrap_or(atlas::Skin::Cat);
            s.motion = match s.client.gate.motion() {
                "full" => atlas::Motion::Full,
                "static" => atlas::Motion::Static,
                _ => atlas::Motion::Calm,
            };
        }
        applying.set(true);
        skin.set_selected(s.skin as u32);
        motion.set_selected(match s.motion {
            atlas::Motion::Full => 0,
            atlas::Motion::Calm => 1,
            atlas::Motion::Static => 2,
        });
        applying.set(false);
        status.set_text(
            s.client
                .gate
                .operation_error()
                .unwrap_or(s.client.gate.status()),
        );
        let busy = s.close_work.as_ref().is_some_and(|probe| probe());
        if s.close.should_cancel_for_work(busy) {
            log::debug!(target: "eva_native_host", "GTK close cancelled by new terminal work");
            s.cancel_close();
        }
        close_bar.set_visible(s.close.waiting());
        let accepted = s.ready_to_close();
        if let Some(revision) = s.close.queue_completion(accepted) {
            let state = Rc::downgrade(&state);
            gtk::glib::idle_add_local_once(move || {
                let Some(state) = state.upgrade() else {
                    return;
                };
                let ready = {
                    let mut s = state.borrow_mut();
                    if !s.close.completion_current(revision) {
                        return;
                    }
                    if !s.ready_to_close() {
                        s.close.retry_completion(revision);
                        return;
                    }
                    let busy = s.close_work.as_ref().is_some_and(|probe| probe());
                    if s.close.should_cancel_for_work(busy) {
                        s.cancel_close();
                        return;
                    }
                    s.on_close_ready.clone()
                };
                // Reentering the application's ordinary guards must not keep
                // the State RefCell borrowed or trust an obsolete callback.
                if let Some(ready) = ready {
                    ready(revision);
                }
            });
        }

        let pose = s.client.gate.pose().to_owned();
        let Some(known_pose) = atlas::Pose::from_key(&pose) else {
            body.set_visible(false);
            status.set_text("Eva hidden — unsupported pose");
            return gtk::glib::ControlFlow::Continue;
        };
        if pose != s.last_pose {
            s.last_pose = pose;
            s.started = now;
            s.next = Some(now);
        }
        let paint_changed = changed || s.frame.is_none() || s.next.is_some_and(|t| now >= t);
        if paint_changed {
            let mut request = atlas::Request::new(s.skin, known_pose);
            request.motion = s.motion;
            request.elapsed_ms = now.duration_since(s.started).as_millis() as u64;
            request.reduced_motion =
                gtk::Settings::default().is_some_and(|v| !v.is_gtk_enable_animations());
            let frame = atlas::sample(request).expect("visible native frame request");
            let full = markup(&frame, s.skin, s.high_contrast);
            let [r, g, b] = s.skin.palette(s.high_contrast).color(atlas::Ink::Outline);
            let compact=format!("<span font_family=\"monospace\" size=\"9000\" foreground=\"#{r:02x}{g:02x}{b:02x}\">{}</span>",gtk::glib::markup_escape_text(s.skin.original_sketch()));
            if full != s.full_markup {
                full_layout.set_markup(&full);
                s.full_markup = full;
            }
            if compact != s.compact_markup {
                compact_layout.set_markup(&compact);
                s.compact_markup = compact;
            }
            s.next = frame
                .next_frame_in_ms
                .map(|ms| now + Duration::from_millis(ms));
            s.frame = Some(frame);
        }
        let context = body.pango_context();
        let context_id = context.as_ptr() as usize;
        if context_id != s.pango_context_id {
            full_layout = body.create_pango_layout(None);
            compact_layout = body.create_pango_layout(None);
            full_layout.set_markup(&s.full_markup);
            compact_layout.set_markup(&s.compact_markup);
            s.pango_context_id = context_id;
        }
        let serial = context.serial();
        if serial != s.pango_serial {
            full_layout.context_changed();
            compact_layout.context_changed();
            s.pango_serial = serial;
        }
        let full_size = measured_extent(&full_layout);
        let compact_size = measured_extent(&compact_layout);
        let size = (overlay.width(), overlay.height());
        let editable = gtk::prelude::GtkWindowExt::focus(&window)
            .is_some_and(|w| w.is::<gtk::TextView>() || w.is::<gtk::Entry>());
        let (protected, exclusions) = protections(overlay.upcast_ref(), &overlay);
        let surface_conflict = s.client.gate.possible_body_token().is_some_and(|token| {
            s.frame_clock_binding
                .as_ref()
                .is_some_and(|binding| !s.surface_lease.can_paint(token, &binding.domain.surface))
        });
        let blocked = s.client.gate.withdrawal_pending()
            || surface_conflict
            || protected
            || s.selecting
            || now < s.input_until
            || editable
            || pop.is_visible();
        if blocked != s.blocked {
            s.blocked = blocked;
            let selecting = s.selecting;
            s.client.send(Request::Input {
                active: blocked,
                selecting,
            });
        }
        let placement = if !blocked && window.is_mapped() && overlay.is_mapped() {
            safe_edge(size, &exclusions, full_size, compact_size)
        } else {
            None
        };
        let safe = placement.is_some();
        let (candidate, compact) = placement.unwrap_or((
            Rect {
                x: 0.,
                y: 0.,
                width: 1.,
                height: 1.,
            },
            false,
        ));
        let layout = (
            size.0,
            size.1,
            safe,
            candidate.x as i32,
            candidate.y as i32,
            candidate.width as i32,
            candidate.height as i32,
        );
        let mode_changed = s.compact != compact;
        if mode_changed {
            s.compact = compact;
            s.next = Some(now);
        }
        clip.set_margin_start(candidate.x.max(0.) as i32);
        clip.set_margin_top(candidate.y.max(0.) as i32);
        clip.set_bounds(
            candidate.width.ceil() as i32,
            candidate.height.ceil() as i32,
        );
        if layout != s.geometry || exclusions != s.last_exclusions {
            s.last_exclusions = exclusions.clone();
            s.geometry = layout;
            s.revision += 1;
            let revision = s.revision;
            let viewport = Rect {
                x: 0.,
                y: 0.,
                width: size.0.max(1) as f64,
                height: size.1.max(1) as f64,
            };
            let candidates = if safe { vec![candidate] } else { vec![] };
            s.client.send(Request::Geometry {
                revision,
                viewport,
                candidates,
                exclusions,
            });
            s.client.send(Request::Focus {
                focused: window.is_active(),
            });
        }
        if s.pending_recall && safe && !pop.is_visible() {
            s.client.send(Request::Recall);
            s.pending_recall = false;
        }
        let visible = safe && s.client.gate.token().is_some();
        if !visible {
            body.set_visible(false);
            s.next = None;
            if s.client.gate.pending_retirement() {
                overlay.queue_draw();
            }
            return gtk::glib::ControlFlow::Continue;
        }
        let Some(surface) = s
            .frame_clock_binding
            .as_ref()
            .map(|binding| binding.domain.surface.clone())
        else {
            body.set_visible(false);
            return gtk::glib::ControlFlow::Continue;
        };
        let Some(token) = s.client.gate.token().cloned() else {
            return gtk::glib::ControlFlow::Continue;
        };
        if !s.surface_lease.record_body(token, surface) {
            body.set_visible(false);
            return gtk::glib::ControlFlow::Continue;
        }
        if paint_changed || !body.is_visible() || mode_changed {
            let text = if compact {
                &s.compact_markup
            } else {
                &s.full_markup
            };
            if body.label() != *text {
                body.set_markup(text);
            }
            if let Some(frame) = &s.frame {
                body.set_tooltip_text(Some(&frame.accessible_label));
                body.update_property(&[gtk::accessible::Property::Label(&frame.accessible_label)]);
            }
            if compact {
                s.next = None;
            }
            body.set_visible(true);
        }
        if s.client.gate.body_presentation_pending() {
            // Retry an actual native frame after unavailable/evicted timing,
            // including static motion. The timer itself supplies no proof.
            overlay.queue_draw();
        }
        gtk::glib::ControlFlow::Continue
    });
}

use crate::eva_companion_paint::markup;

#[cfg(test)]
#[path = "gtk_clock_fixture.rs"]
mod clock_fixture;

fn motion_key(m: atlas::Motion) -> &'static str {
    match m {
        atlas::Motion::Full => "full",
        atlas::Motion::Calm => "calm",
        atlas::Motion::Static => "static",
    }
}

thread_local! {
    static HOSTS: RefCell<Vec<(gtk::glib::WeakRef<gtk::Window>, std::rc::Weak<RefCell<State>>)>> = const { RefCell::new(Vec::new()) };
}
/// Only identifiers, revision and actual task receipt state cross IPC.
pub(crate) fn observe_task(window: &gtk::Window, task: Option<&jterm_core::agent_task::AgentTask>) {
    let Some(task) = task else {
        HOSTS.with(|hosts| {
            for (w, s) in hosts.borrow().iter() {
                if w.upgrade().as_ref() == Some(window) {
                    if let Some(s) = s.upgrade() {
                        let mut s = s.borrow_mut();
                        if s.last_task.take().is_some() {
                            let context = host::ContextRef {
                                host_id: s.client.gate.host_id().into(),
                                pane_id: "companion-card".into(),
                                task_id: None,
                                project_id: None,
                                revision: 0,
                            };
                            s.register_context(context);
                        }
                    }
                }
            }
        });
        return;
    };
    let status = task_status(task.status);
    HOSTS.with(|hosts| {
        for (w, s) in hosts.borrow().iter() {
            if w.upgrade().as_ref() == Some(window) {
                if let Some(s) = s.upgrade() {
                    let mut s = s.borrow_mut();
                    let key = (task.id.to_string(), task.updated_at_ms, status);
                    if s.last_task.as_ref() != Some(&key) {
                        let task_changed = s.last_task.as_ref().is_none_or(|old| old.0 != key.0);
                        s.last_task = Some(key.clone());
                        if task_changed {
                            let context = host::ContextRef {
                                host_id: s.client.gate.host_id().into(),
                                pane_id: "task-card".into(),
                                task_id: Some(key.0.clone()),
                                project_id: None,
                                revision: 0,
                            };
                            s.register_context(context);
                            s.memory_review = None;
                            s.memory_context = None;
                        }
                        s.client.send(Request::TaskObservation {
                            task_id: key.0,
                            context_revision: key.1,
                            status,
                        });
                    }
                }
            }
        }
    });
}

/// Conservative metadata-only native exclusion scan. Never reads label/text contents.
fn protections(root: &gtk::Widget, overlay: &gtk::Overlay) -> (bool, Vec<Rect>) {
    use vte4::prelude::TerminalExt;
    let mut pending = vec![root.clone()];
    let mut exclusions = Vec::new();
    let mut blocked = false;
    let mut visited = 0;
    while let Some(w) = pending.pop() {
        visited += 1;
        if visited > 2048 {
            return (true, Vec::new());
        }
        if !w.is_mapped() {
            continue;
        }
        blocked |= w.has_css_class("eva-remote-surface")
            || w.has_css_class("block-selection-active")
            || w.has_css_class("block-alt-screen")
            || w.has_css_class("command-review-standalone")
            || w.has_css_class("command-review-body");
        if let Some(vte) = w.downcast_ref::<vte4::Terminal>() {
            blocked |= vte.has_selection();
        }
        if w.is::<gtk::Entry>()
            || w.is::<gtk::TextView>()
            || w.is::<gtk::Button>()
            || w.is::<gtk::Scrollbar>()
        {
            if let Some(r) = w.compute_bounds(overlay) {
                if r.width() > 0. && r.height() > 0. {
                    exclusions.push(Rect {
                        x: r.x() as f64 - 8.,
                        y: r.y() as f64 - 8.,
                        width: r.width() as f64 + 16.,
                        height: r.height() as f64 + 16.,
                    });
                }
            }
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            pending.push(c);
        }
        if exclusions.len() > host::MAX_REGIONS {
            return (true, Vec::new());
        }
    }
    (blocked, exclusions)
}

fn task_status(value: jterm_core::agent_task::TaskStatus) -> host::TaskStatus {
    use jterm_core::agent_task::TaskStatus as T;
    match value {
        T::Starting | T::Working => host::TaskStatus::Running,
        T::WaitingForApproval | T::WaitingForHuman | T::ReadyForReview => {
            host::TaskStatus::NeedsReview
        }
        T::Completed => host::TaskStatus::Succeeded,
        T::Failed => host::TaskStatus::Failed,
        _ => host::TaskStatus::Unknown,
    }
}
fn presentation_evidence(complete: bool, presentation_time: i64) -> bool {
    complete && presentation_time > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_skin_pose_is_colored_and_escaped_without_emoji() {
        for skin in atlas::Skin::ALL {
            for pose in atlas::Pose::ALL {
                let f = atlas::sample(atlas::Request::new(skin, pose)).unwrap();
                let m = markup(&f, skin, false);
                assert!(m.starts_with("<span font_family=\"monospace\""));
                assert!(m.contains("foreground=\"#"));
                assert!(!m.contains("<3"));
                assert_eq!(f.cells.len(), f.width * f.height);
                assert!(f.cells.iter().all(|c| c.glyph.is_ascii()));
            }
        }
    }
    #[test]
    fn safe_edges_hide_when_fully_excluded_and_compact_when_narrow() {
        assert!(safe_edge(
            (800, 600),
            &[Rect {
                x: 0.,
                y: 0.,
                width: 800.,
                height: 600.
            }],
            (280, 264),
            (100, 72)
        )
        .is_none());
        assert!(safe_edge((220, 240), &[], (280, 264), (100, 72)).unwrap().1);
        assert!(!safe_edge((800, 600), &[], (280, 264), (100, 72)).unwrap().1);
    }
    #[test]
    fn only_actual_receipts_map_to_success() {
        use jterm_core::agent_task::TaskStatus as T;
        assert_eq!(
            task_status(T::ReadyForReview),
            host::TaskStatus::NeedsReview
        );
        assert_eq!(task_status(T::Completed), host::TaskStatus::Succeeded);
        assert_eq!(task_status(T::Cancelled), host::TaskStatus::Unknown);
    }
    #[test]
    fn native_proof_rejects_submission_without_presentation() {
        assert!(!presentation_evidence(false, 42));
        assert!(!presentation_evidence(true, 0));
        assert!(presentation_evidence(true, 42));
    }
    /// Requires a real GTK display; intentionally not silently mocked by CI.
    #[test]
    #[ignore = "requires native GTK display; run explicitly in native matrix"]
    fn shared_body_is_non_layout_and_pointer_transparent() {
        gtk::init().unwrap();
        let overlay = gtk::Overlay::new();
        let body = gtk::Label::new(Some("Eva"));
        body.set_can_target(false);
        overlay.add_overlay(&body);
        overlay.set_measure_overlay(&body, false);
        assert!(!overlay.is_measure_overlay(&body));
        assert!(!body.can_target());
    }
}

static SHARED_MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
pub(crate) fn configure(enabled: bool) {
    let _ = SHARED_MODE.set(enabled || host::shared_mode_requested());
}
pub(crate) fn shared_mode_requested() -> bool {
    *SHARED_MODE.get_or_init(host::shared_mode_requested)
}

fn safe_edge(
    size: (i32, i32),
    exclusions: &[Rect],
    full: (i32, i32),
    compact_size: (i32, i32),
) -> Option<(Rect, bool)> {
    let viewport = Rect {
        x: 0.,
        y: 0.,
        width: size.0 as f64,
        height: size.1 as f64,
    };
    if !viewport.valid() {
        return None;
    }
    for (width, height, compact) in [
        (full.0 as f64, full.1 as f64, false),
        (compact_size.0 as f64, compact_size.1 as f64, true),
    ] {
        for (x, y) in [
            (viewport.width - width - 24., viewport.height - height - 86.),
            (viewport.width - width - 24., 80.),
            (24., viewport.height - height - 86.),
        ] {
            let r = Rect {
                x,
                y,
                width,
                height,
            };
            if viewport.contains(r) && !exclusions.iter().any(|e| e.overlaps(r)) {
                return Some((r, compact));
            }
        }
    }
    None
}

struct MemoryUi {
    status: gtk::Label,
    key: gtk::Entry,
    value: gtk::Entry,
    approve: gtk::Button,
    discovered: gtk::DropDown,
    persisted: gtk::DropDown,
    scope_status: gtk::Label,
    scope_approve: gtk::Button,
    voice_status: gtk::Label,
}
fn memory_controls(controls: &gtk::Box, state: &Rc<RefCell<State>>) -> MemoryUi {
    let group = gtk::Expander::new(Some("Memory · inspect / propose / review"));
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 5);
    group.set_child(Some(&panel));
    controls.append(&group);
    let status = gtk::Label::new(Some(
        "Nothing is saved until you review and approve the exact change.",
    ));
    status.set_wrap(true);
    status.set_max_width_chars(42);
    status.set_selectable(true);
    panel.append(&status);
    let scope = gtk::DropDown::from_strings(&[
        "Current selected task",
        "Current pane project (explicit assignment)",
        "Global scope (explicit)",
    ]);
    panel.append(&scope);
    let project = gtk::Entry::builder()
        .placeholder_text("Named project scope ID (no directory access)")
        .max_length(128)
        .build();
    panel.append(&project);
    let assign = gtk::Button::with_label("Assign named project to current pane context");
    panel.append(&assign);
    let project_note = gtk::Label::new(Some("No automatic directory access. Assignment is local to this pane context; no automatic model memory retrieval."));
    project_note.set_wrap(true);
    panel.append(&project_note);
    {
        let state = state.clone();
        let project = project.clone();
        let note = project_note.clone();
        assign.connect_clicked(move |_| {
            let id=project.text().trim().to_owned();
            if !host::valid_scope(&host::MemoryScope::Project(id.clone())) { note.set_text("Enter a valid nonempty project identifier (maximum 128 bytes)."); return; }
            let mut s=state.borrow_mut();
            let Some(mut context)=s.current_context.clone().filter(|c|uuid::Uuid::parse_str(&c.pane_id).is_ok())else{note.set_text("Focus the intended local terminal pane first, then assign its named project here.");return;};
            if context.task_id.is_some() && context.project_id.as_ref().is_some_and(|old|old!=&id){note.set_text("This pane already has a different project. Switch pane context or use archived management instead of silent reassignment.");return;}
            s.assigned_project=Some((context.pane_id.clone(),context.task_id.clone(),id.clone()));
            context.project_id=Some(id.clone());
            let context=s.register_context(context);
            note.set_text(&format!("Assigned project {} to pane {} · task {:?}. No directory access; no automatic model retrieval.",id,context.pane_id,context.task_id));
        });
    }
    let registered = gtk::Button::with_label("Use discovered registered project for current task");
    panel.append(&registered);
    {
        let state = state.clone();
        let note = project_note.clone();
        registered.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            let mut context = active_memory_context(&mut s);
            let Some(project) = s
                .task_scopes
                .iter()
                .find(|item| Some(&item.task_id) == context.task_id.as_ref())
                .and_then(|item| item.project_id.clone())
            else {
                note.set_text("Discover scopes first. Current context has no registered project.");
                return;
            };
            context.project_id = Some(project.clone());
            s.register_context(context);
            note.set_text(&format!(
                "Using registered project {}. No directory access or automatic model retrieval.",
                project
            ));
        });
    }
    let archive = gtk::Expander::new(Some("Manage archived task / project scopes"));
    let archive_panel = gtk::Box::new(gtk::Orientation::Vertical, 4);
    archive.set_child(Some(&archive_panel));
    panel.append(&archive);
    let scope_status = gtk::Label::new(Some(
        "Management only: never takes over a task or adds model context.",
    ));
    scope_status.set_wrap(true);
    scope_status.set_selectable(true);
    archive_panel.append(&scope_status);
    for (label, more) in [
        ("Discover registered task/project scopes", false),
        ("Next scopes", true),
    ] {
        let button = gtk::Button::with_label(label);
        let state = state.clone();
        button.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            let after = if more {
                s.next_task_scope.clone()
            } else {
                None
            };
            if more && after.is_none() {
                return;
            }
            s.client.send(Request::TaskScopes { after });
        });
        archive_panel.append(&button);
    }
    for (label, more) in [
        ("Discover persisted memory scopes", false),
        ("Next persisted scopes", true),
    ] {
        let button = gtk::Button::with_label(label);
        let state = state.clone();
        button.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            let after = if more {
                s.next_memory_scope.clone()
            } else {
                None
            };
            if more && after.is_none() {
                return;
            }
            s.client.send(Request::MemoryScopes { after });
        });
        archive_panel.append(&button);
    }
    let persisted = gtk::DropDown::from_strings(&[]);
    archive_panel.append(&persisted);
    let review_persisted = gtk::Button::with_label("Review selected persisted scope access");
    archive_panel.append(&review_persisted);
    {
        let state = state.clone();
        let persisted = persisted.clone();
        let status = scope_status.clone();
        review_persisted.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            let Some(scope) = s.memory_scopes.get(persisted.selected() as usize).cloned() else {
                return;
            };
            let context = active_memory_context(&mut s);
            s.scope_review = None;
            s.scope_request = Some((context.clone(), scope.clone()));
            s.client
                .send(Request::MemoryScopeReviewRequested { context, scope });
            status.set_text("Waiting for exact persisted-scope review; no access approved yet.");
        });
    }
    let discovered = gtk::DropDown::from_strings(&[]);
    archive_panel.append(&discovered);
    let archive_id = gtk::Entry::builder()
        .placeholder_text("Exact archived scope ID")
        .max_length(128)
        .build();
    archive_panel.append(&archive_id);
    let archive_project = gtk::CheckButton::with_label("Project scope (otherwise task)");
    archive_panel.append(&archive_project);
    for (label, from_list) in [
        ("Review discovered scope access", true),
        ("Review typed scope access", false),
    ] {
        let button = gtk::Button::with_label(label);
        let state = state.clone();
        let discovered = discovered.clone();
        let archive_id = archive_id.clone();
        let archive_project = archive_project.clone();
        let status = scope_status.clone();
        button.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            let project = archive_project.is_active();
            let id = if from_list {
                let Some(item) = s.task_scopes.get(discovered.selected() as usize) else {
                    return;
                };
                if project {
                    let Some(id) = item.project_id.clone() else {
                        status.set_text("This registered task has no project scope.");
                        return;
                    };
                    id
                } else {
                    item.task_id.clone()
                }
            } else {
                archive_id.text().trim().to_owned()
            };
            let scope = if project {
                host::MemoryScope::Project(id)
            } else {
                host::MemoryScope::Task(id)
            };
            if !host::valid_scope(&scope) {
                status.set_text("Invalid scope identifier.");
                return;
            }
            let context = active_memory_context(&mut s);
            s.scope_review = None;
            s.scope_request = Some((context.clone(), scope.clone()));
            s.client
                .send(Request::MemoryScopeReviewRequested { context, scope });
            status.set_text("Waiting for exact scope review; no access approved yet.");
        });
        archive_panel.append(&button);
    }
    let scope_approve = gtk::Button::with_label("Approve exactly this management scope");
    scope_approve.set_sensitive(false);
    archive_panel.append(&scope_approve);
    {
        let state = state.clone();
        let status = scope_status.clone();
        scope_approve.connect_clicked(move |button| {
            let mut s=state.borrow_mut(); let Some((context,scope,review_id,digest))=s.scope_review.take()else{return;};
            button.set_sensitive(false);
            if !scope_rules::accepts_review(s.current_context.as_ref(),s.scope_request.as_ref(),&context,&scope) {status.set_text("Context changed. Review access again.");return;}
            s.client.send(Request::MemoryScopeApprove{context:context.clone(),review_id,digest});
            s.managed_scope=Some(scope.clone()); s.memory_review=None; s.memory_entry=None; s.memory_next=None; s.memory_context=Some(context.clone()); s.listed_scope=Some(scope.clone());
            s.client.send(Request::MemoryList{context,scope:scope.clone(),query:String::new(),before_id:None});
            status.set_text(&format!("Managing {:?} only. View/save/edit/delete/clear below target this scope, never the active task.",scope));
        });
    }
    let return_current = gtk::Button::with_label("Dismiss review / return to current scope");
    archive_panel.append(&return_current);
    {
        let state = state.clone();
        let status = scope_status.clone();
        let approve = scope_approve.clone();
        return_current.connect_clicked(move |_| {
            state.borrow_mut().invalidate_memory_management();
            approve.set_sensitive(false);
            status.set_text("Current scope restored. No archived scope selected.");
        });
    }
    {
        let state = state.clone();
        scope.connect_selected_notify(move |_| {
            state.borrow_mut().invalidate_memory_management();
        });
    }

    let key = gtk::Entry::builder()
        .placeholder_text("Memory key, e.g. prefer_quiet")
        .max_length(128)
        .build();
    panel.append(&key);
    let value = gtk::Entry::builder()
        .placeholder_text("Memory value / edited value")
        .max_length(8192)
        .build();
    panel.append(&value);
    let selected_scope: Rc<dyn Fn() -> Option<host::MemoryScope>> = {
        let scope = scope.clone();
        let state = state.clone();
        Rc::new(move || {
            if let Some(managed) = state.borrow().managed_scope.clone() {
                return Some(managed);
            }
            match scope.selected() {
                1 => state
                    .borrow()
                    .current_context
                    .as_ref()
                    .and_then(|c| c.project_id.clone())
                    .map(host::MemoryScope::Project),
                2 => Some(host::MemoryScope::Global),
                0 => state
                    .borrow()
                    .last_task
                    .as_ref()
                    .map(|(id, _, _)| host::MemoryScope::Task(id.clone())),
                _ => None,
            }
        })
    };

    for (label, next) in [("View memory", false), ("Next memory entry", true)] {
        let button = gtk::Button::with_label(label);
        let state = state.clone();
        let get = selected_scope.clone();
        let status = status.clone();
        button.connect_clicked(move |_|{let Some(scope)=get()else{status.set_text("Select a real task, assign a named project to this pane, or explicitly choose Global.");return;};let mut s=state.borrow_mut();let context=memory_context(&mut s,&scope);s.memory_review=None;s.memory_context=Some(context.clone());s.listed_scope=Some(scope.clone());let before_id=if next{s.memory_next}else{None};s.client.send(Request::MemoryList{context,scope,query:String::new(),before_id});});
        panel.append(&button);
    }
    let propose = gtk::Button::with_label("Review save / edit");
    {
        let state = state.clone();
        let get = selected_scope.clone();
        let key = key.clone();
        let value = value.clone();
        let status = status.clone();
        propose.connect_clicked(move |_| {
            let Some(scope) = get() else {
                status.set_text("Choose an explicit valid scope first.");
                return;
            };
            let key = key.text().to_string();
            let value = value.text().to_string();
            if !host::identifier(&key) || value.is_empty() || value.len() > 8192 {
                status.set_text("A valid key and 1–8192 byte value are required.");
                return;
            }
            let mut s = state.borrow_mut();
            let context = memory_context(&mut s, &scope);
            s.memory_review = None;
            s.memory_context = Some(context.clone());
            let expected_revision = s
                .memory_entry
                .as_ref()
                .filter(|e| e.key == key && e.scope == scope)
                .map(|e| e.revision);
            let change = host::MemoryChange::Put {
                scope,
                key,
                value,
                source_event_id: uuid::Uuid::new_v4().to_string(),
                expected_revision,
                expires_at: None,
            };
            s.client.send(Request::MemoryPropose { context, change });
        });
    }
    panel.append(&propose);
    let save_exchange = gtk::Button::with_label("Review saving this exchange");
    {
        let state = state.clone();
        let get = selected_scope.clone();
        let status = status.clone();
        save_exchange.connect_clicked(move |_| {
            let Some(scope) = get() else {
                status.set_text("Choose the current task/project or explicitly choose Global.");
                return;
            };
            let mut s = state.borrow_mut();
            let Some(context) = s.current_context.clone() else {
                return;
            };
            let Some(turn) = s.dialogue_save.turn_id().map(str::to_owned) else {
                status.set_text("Wait for a complete reply before saving this exchange.");
                return;
            };
            match s.dialogue_save.proposal(&turn, &context, scope) {
                Ok(change) => {
                    s.memory_review = None;
                    s.memory_context = Some(context.clone());
                    s.client.send(Request::MemoryPropose { context, change });
                    status.set_text(
                        "Review the full exchange and scope below. Nothing has been saved yet.",
                    );
                }
                Err(reason) => status.set_text(reason),
            }
        });
    }
    panel.append(&save_exchange);
    // Eva's spoken replies stay only briefly in Eva's memory. The coordinator
    // verifies the text against the recorded reply; the normal review below
    // still decides whether anything is saved.
    let voice_status = gtk::Label::new(Some(
        "Eva keeps recent spoken replies only briefly in memory. Nothing is saved unless you review it here.",
    ));
    voice_status.set_wrap(true);
    voice_status.set_max_width_chars(42);
    voice_status.set_xalign(0.0);
    let save_voice = gtk::Button::with_label("Review saving Eva's last spoken reply");
    {
        let state = state.clone();
        let get = selected_scope.clone();
        let voice_status = voice_status.clone();
        save_voice.connect_clicked(move |_| {
            let Some(scope) = get() else {
                voice_status
                    .set_text("Choose the current task/project or explicitly choose Global.");
                return;
            };
            let mut s = state.borrow_mut();
            let Some(context) = s.current_context.clone() else {
                voice_status.set_text("Focus a terminal pane first.");
                return;
            };
            s.memory_review = None;
            s.memory_context = Some(context.clone());
            if let Err(reason) = s.client.request_voice_text_review(&context, scope) {
                voice_status.set_text(reason);
            }
        });
    }
    let forget_voice = gtk::Button::with_label("Forget Eva's retained spoken text");
    {
        let state = state.clone();
        let voice_status = voice_status.clone();
        forget_voice.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            let Some(context) = s.current_context.clone() else {
                voice_status.set_text("Focus a terminal pane first.");
                return;
            };
            if let Err(reason) = s.client.forget_voice_text(&context) {
                voice_status.set_text(reason);
            }
        });
    }
    panel.append(&save_voice);
    panel.append(&forget_voice);
    panel.append(&voice_status);
    for (label, clear) in [
        ("Review delete shown entry", false),
        ("Review clear this scope", true),
    ] {
        let button = gtk::Button::with_label(label);
        let state = state.clone();
        let get = selected_scope.clone();
        let status = status.clone();
        button.connect_clicked(move |_| {
            let Some(scope) = get() else {
                return;
            };
            let mut s = state.borrow_mut();
            let context = memory_context(&mut s, &scope);
            let change = if clear {
                host::MemoryChange::Clear { scope }
            } else {
                let Some(e) = s.memory_entry.as_ref().filter(|e| e.scope == scope) else {
                    status.set_text("View the entry in this scope before deleting.");
                    return;
                };
                host::MemoryChange::Delete {
                    id: e.id,
                    expected_revision: e.revision,
                }
            };
            s.memory_review = None;
            s.memory_context = Some(context.clone());
            s.client.send(Request::MemoryPropose { context, change });
        });
        panel.append(&button);
    }
    let approve = gtk::Button::with_label("Approve exactly the displayed change");
    approve.set_sensitive(false);
    {
        let state = state.clone();
        approve.connect_clicked(move |button| {
            let mut s = state.borrow_mut();
            if let Some((context, review)) = s.memory_review.clone() {
                if s.current_context.as_ref() != Some(&context) {
                    s.memory_review = None;
                    button.set_sensitive(false);
                    return;
                }
                s.client.send(Request::MemoryApprove {
                    context,
                    review_id: review.id,
                    digest: review.digest,
                });
                button.set_sensitive(false);
            }
        });
    }
    panel.append(&approve);
    let cancel = gtk::Button::with_label("Cancel memory change");
    {
        let state = state.clone();
        let approve = approve.clone();
        cancel.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            if let Some((_, review)) = s.memory_review.take() {
                s.client.send(Request::MemoryCancel {
                    review_id: review.id,
                });
            }
            approve.set_sensitive(false);
        });
    }
    panel.append(&cancel);
    let pause = gtk::CheckButton::with_label("Pause memory writes");
    {
        let state = state.clone();
        pause.connect_toggled(move |p| {
            state.borrow_mut().client.send(Request::MemoryPause {
                paused: p.is_active(),
            })
        });
    }
    panel.append(&pause);
    MemoryUi {
        status,
        key,
        value,
        approve,
        discovered,
        persisted,
        scope_status,
        scope_approve,
        voice_status,
    }
}
struct HistoryUi {
    status: gtk::Label,
    content: gtk::Label,
    area: gtk::ScrolledWindow,
    older: gtk::Button,
}
impl HistoryUi {
    fn render(&self, view: &host::history::HistoryView, viewer: Option<&host::ContextRef>) {
        if !view.status().is_empty() {
            self.status.set_text(view.status());
        }
        self.content.set_text(&view.text(viewer));
        self.area.set_visible(!view.entries().is_empty());
        self.older.set_sensitive(view.can_load_older());
    }
}
/// Static, read-only history. Loading never moves focus or adds an avatar.
fn history_controls(controls: &gtk::Box, state: &Rc<RefCell<State>>) -> HistoryUi {
    let group = gtk::Expander::new(Some("Conversation history · read-only"));
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let scope = gtk::DropDown::from_strings(&host::history::SCOPES.map(host::history::scope_label));
    scope.set_tooltip_text(Some(
        "Eva dialogue only. Terminal output and voice transcripts are never included.",
    ));
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let load = gtk::Button::with_label("Load history");
    let older = gtk::Button::with_label("Older");
    older.set_sensitive(false);
    let status = gtk::Label::new(Some("Nothing is loaded until you choose Load history."));
    status.set_wrap(true);
    status.set_max_width_chars(42);
    status.set_xalign(0.0);
    let content = gtk::Label::new(None);
    content.set_wrap(true);
    content.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    content.set_selectable(true);
    content.set_max_width_chars(42);
    content.set_xalign(0.0);
    // A fixed, readable area: wrapped labels under nested scrolling do not
    // reliably report their natural height.
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(220)
        .max_content_height(320)
        .child(&content)
        .visible(false)
        .build();
    {
        let state = state.clone();
        scope.connect_selected_notify(move |scope| {
            if let Some(scope) = host::history::SCOPES.get(scope.selected() as usize) {
                state.borrow_mut().client.history.set_scope(*scope);
            }
        });
    }
    for (button, older_page) in [(&load, false), (&older, true)] {
        let state = state.clone();
        let status = status.clone();
        button.connect_clicked(move |_| {
            let mut s = state.borrow_mut();
            let context = s
                .current_context
                .clone()
                .unwrap_or_else(|| host::ContextRef {
                    host_id: s.client.gate.host_id().into(),
                    pane_id: "companion-card".into(),
                    task_id: None,
                    project_id: None,
                    revision: s.revision,
                });
            let context = s.register_context(context);
            if let Err(message) = s.client.request_history(&context, older_page) {
                status.set_text(message);
            }
        });
        buttons.append(button);
    }
    panel.append(&scope);
    panel.append(&buttons);
    panel.append(&status);
    panel.append(&scroll);
    group.set_child(Some(&panel));
    controls.append(&group);
    HistoryUi {
        status,
        content,
        area: scroll,
        older,
    }
}
fn active_memory_context(s: &mut State) -> host::ContextRef {
    let context = s
        .current_context
        .clone()
        .unwrap_or_else(|| host::ContextRef {
            host_id: s.client.gate.host_id().to_owned(),
            pane_id: "companion-card".into(),
            task_id: s.last_task.as_ref().map(|t| t.0.clone()),
            project_id: None,
            revision: 0,
        });
    s.register_context(context)
}
fn memory_context(s: &mut State, scope: &host::MemoryScope) -> host::ContextRef {
    // Archived management NEVER adopts its target as active task/project.
    if s.managed_scope.is_none() {
        if let host::MemoryScope::Task(id) = scope {
            if s.last_task.as_ref().is_some_and(|t| &t.0 == id)
                && s.current_context
                    .as_ref()
                    .is_none_or(|c| c.task_id.as_ref() != Some(id))
            {
                let context = host::ContextRef {
                    host_id: s.client.gate.host_id().into(),
                    pane_id: "task-card".into(),
                    task_id: Some(id.clone()),
                    project_id: None,
                    revision: 0,
                };
                return s.register_context(context);
            }
        }
    }
    active_memory_context(s)
}

/// Stable per-widget UUID, not paths or terminal contents. Recreated panes never reuse authority.
pub(crate) fn tag_pane(widget: &impl IsA<gtk::Widget>) {
    if !widget
        .css_classes()
        .iter()
        .any(|c| c.starts_with("eva-pane-id-"))
    {
        widget.add_css_class(&format!("eva-pane-id-{}", uuid::Uuid::new_v4()));
    }
}
fn has_remote_ancestor(widget: &gtk::Widget) -> bool {
    let mut next = Some(widget.clone());
    while let Some(w) = next {
        if w.has_css_class("eva-remote-surface") {
            return true;
        }
        next = w.parent();
    }
    false
}
fn native_pane_id(widget: &gtk::Widget) -> Option<String> {
    let mut next = Some(widget.clone());
    let mut result = None;
    while let Some(w) = next {
        if w.has_css_class("eva-remote-surface") || w.has_css_class("eva-companion-card") {
            return None;
        }
        if let Some(id) = w
            .css_classes()
            .iter()
            .find_map(|c| c.strip_prefix("eva-pane-id-").map(str::to_owned))
        {
            result = Some(id);
        }
        next = w.parent();
    }
    result
}

/// Explicit button-only selection read. No output fetch occurs in the frame loop.
fn capture_one_selection(root: &gtk::Widget) -> Option<(String, String)> {
    use vte4::prelude::TerminalExt;
    let mut pending = vec![root.clone()];
    let mut found = None;
    let mut visited = 0;
    while let Some(w) = pending.pop() {
        visited += 1;
        if visited > 2048 {
            return None;
        }
        if !w.is_mapped() {
            continue;
        }
        if w.has_css_class("eva-remote-surface") {
            continue;
        }
        if let Some(vte) = w.downcast_ref::<vte4::Terminal>() {
            if vte.has_selection() {
                if found.is_some() {
                    return None;
                }
                let mut text = vte.text_selected(vte4::Format::Text)?.to_string();
                let mut end = text.len().min(4096);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                if text.is_empty() {
                    return None;
                }
                found = Some((native_pane_id(vte.upcast_ref())?, text));
            }
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            pending.push(c);
        }
    }
    found
}
impl State {
    fn ready_to_close(&self) -> bool {
        self.client.gate.withdrawal_accepted()
            || (!self.client.gate.has_connection()
                && self.client.gate.possible_body_token().is_none())
    }
    fn commit_close_if_ready(&mut self) -> bool {
        let ready = self.ready_to_close();
        if !self.close.commit(ready) {
            return false;
        }
        self.close_work = None;
        self.on_close_cancel = None;
        self.on_close_ready = None;
        true
    }
    fn cancel_close(&mut self) {
        if !self.close.cancel() {
            return;
        }
        self.on_close_ready = None;
        self.close_work = None;
        self.client.cancel_withdrawal();
        log::debug!(target: "eva_native_host", "GTK close cancelled; original retirement continues");
        if let Some(cancel) = self.on_close_cancel.take() {
            cancel();
        }
    }
    fn invalidate_memory_management(&mut self) {
        self.managed_scope = None;
        self.scope_request = None;
        self.scope_review = None;
        self.memory_review = None;
        self.memory_context = None;
        self.memory_entry = None;
        self.memory_next = None;
        self.listed_scope = None;
    }
    fn register_context(&mut self, mut context: host::ContextRef) -> host::ContextRef {
        if let Some((pane, task, project)) = &self.assigned_project {
            if pane == &context.pane_id && task == &context.task_id {
                context.project_id = Some(project.clone());
            } else {
                self.assigned_project = None;
            }
        }
        let changed = self
            .current_context
            .as_ref()
            .is_none_or(|old| !scope_rules::same_origin(old, &context));
        if changed {
            self.invalidate_memory_management();
            self.selection_request = None;
            self.selection_review = None;
            self.approved_selection = None;
        }
        if let Some(old) = &self.current_context {
            if old.host_id == context.host_id
                && old.pane_id == context.pane_id
                && old.task_id == context.task_id
                && old.project_id == context.project_id
            {
                context.revision = old.revision;
            } else {
                self.context_serial += 1;
                context.revision = self.context_serial;
            }
        } else {
            self.context_serial += 1;
            context.revision = self.context_serial;
        }
        self.current_context = Some(context.clone());
        self.client.send(Request::ContextObserved {
            context: context.clone(),
        });
        context
    }
}

fn measured_extent(layout: &gtk::pango::Layout) -> (i32, i32) {
    let (ink, logical) = layout.pixel_extents();
    (
        (ink.x() + ink.width())
            .max(logical.x() + logical.width())
            .max(1)
            + 2,
        (ink.y() + ink.height())
            .max(logical.y() + logical.height())
            .max(1)
            + 2,
    )
}

pub(crate) fn owns_keyboard(window: &gtk::Window) -> bool {
    let mut focus = gtk::prelude::GtkWindowExt::focus(window);
    while let Some(w) = focus {
        if w.has_css_class("eva-companion-card") {
            return true;
        }
        focus = w.parent();
    }
    false
}

/// Called only after the app's existing unsaved/process guards accept close.
/// A possible body requires native retirement and negotiated WithdrawalReady.
/// Without a possible body, a disconnected control channel cannot hold the
/// window open. Withdraw still suppresses later admission during child cleanup.
/// Requests during resumption stay serialized on the same host.
pub(crate) fn defer_close(
    window: &gtk::Window,
    guarded: bool,
    work: impl Fn() -> bool + 'static,
    on_ready: impl Fn(u64) + 'static,
    on_cancel: impl FnOnce() + 'static,
) -> bool {
    HOSTS.with(|hosts| {
        for (w, s) in hosts.borrow().iter() {
            if w.upgrade().as_ref() != Some(window) { continue; }
            let Some(s) = s.upgrade() else { continue; };
            let mut s = s.borrow_mut();
            let busy = work();
            let original_work = s.close_work.as_ref().is_some_and(|probe| probe());
            if s.close.should_cancel_for_work(busy || original_work) {
                s.cancel_close();
                return true;
            }
            if s.commit_close_if_ready() { return false; }
            // An explicitly confirmed VTE close may have an unverified prompt
            // but no actual work. Protect work that appears after that point.
            let guarded = guarded || !busy;
            if s.close.begin(guarded) {
                s.close_focus.set(gtk::prelude::GtkWindowExt::focus(window).as_ref());
                s.close_work = Some(Box::new(work));
                s.on_close_ready = Some(Rc::new(on_ready));
                s.on_close_cancel = Some(Box::new(on_cancel));
                if let Some(body) = s.body.upgrade() { body.set_visible(false); }
                s.client.send(Request::Withdraw);
                let current_work = s.close_work.as_ref().is_some_and(|probe| probe());
                if s.close.should_cancel_for_work(current_work) {
                    s.cancel_close();
                    return true;
                }
                if s.commit_close_if_ready() { return false; }
                log::debug!(target: "eva_native_host", "GTK negotiated close started guarded={guarded}");
            }
            return true;
        }
        false
    })
}

/// The model additionally observes its current full pane set, including panes
/// created after the close began. A completed job never rearms GUI exit.
pub(crate) fn observe_close_work(window: &gtk::Window, busy: bool) {
    HOSTS.with(|hosts| {
        for (w, s) in hosts.borrow().iter() {
            if w.upgrade().as_ref() == Some(window) {
                if let Some(s) = s.upgrade() {
                    let mut s = s.borrow_mut();
                    if s.close.should_cancel_for_work(busy) {
                        s.cancel_close();
                    }
                }
            }
        }
    });
}

pub(crate) fn close_waiting(window: &gtk::Window) -> bool {
    HOSTS.with(|hosts| {
        hosts.borrow().iter().any(|(w, s)| {
            w.upgrade().as_ref() == Some(window)
                && s.upgrade().is_some_and(|s| s.borrow().close.waiting())
        })
    })
}

pub(crate) fn close_callback_current(window: &gtk::Window, revision: u64) -> bool {
    HOSTS.with(|hosts| {
        hosts.borrow().iter().any(|(w, s)| {
            w.upgrade().as_ref() == Some(window)
                && s.upgrade().is_some_and(|s| {
                    let s = s.borrow();
                    s.close.completion_current(revision) && s.ready_to_close()
                })
        })
    })
}

fn restore_close_focus(
    state: &Rc<RefCell<State>>,
    focus: gtk::glib::WeakRef<gtk::Widget>,
    revision: u64,
) {
    let state = Rc::downgrade(state);
    gtk::glib::idle_add_local_once(move || {
        let Some(state) = state.upgrade() else {
            return;
        };
        if !state.borrow().close.cancelled_current(revision) {
            return;
        }
        // Focus notification reenters State; release its borrow first.
        if let Some(focus) = focus
            .upgrade()
            .filter(|w| w.is_mapped() && w.is_sensitive())
        {
            focus.grab_focus();
        }
    });
}
