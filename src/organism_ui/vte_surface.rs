//! Native, display-only companion outside the conventional VTE grid.
//!
//! VTE 0.76 exposes neither trustworthy command state nor alternate-screen
//! state. This surface never enters LiveAttention, interprets output, or writes
//! to the terminal. Its clock owns only native painting and visual retreat.

use super::{HUMAN_INPUT_RETREAT, SURFACE_FRAME_INTERVAL};
use crate::config::{Config, OrganismMotion};
use gtk::prelude::*;
use jterm_core::organism::{sticky_glyph_with_context, Behavior, BodyLanguage, RenderContext};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};
use vte4::TerminalExt;

pub(crate) const CAPABILITY_DETAIL: &str = "Traditional VTE shows a neutral companion outside the terminal grid. Command activity and alternate-screen state are unavailable, so hover greetings and Hello are unavailable. Sent input briefly hides the companion without moving the terminal.";
pub(crate) const HELLO_UNAVAILABLE: &str = "Hello is unavailable in traditional VTE: command activity and alternate-screen state cannot be verified.";

fn display_frame(motion: OrganismMotion, frame: u64) -> u64 {
    if motion == OrganismMotion::Full {
        frame
    } else {
        0
    }
}

fn retreat_remaining(last_input: Option<Instant>, now: Instant) -> Option<Duration> {
    last_input.and_then(|at| {
        HUMAN_INPUT_RETREAT
            .checked_sub(now.saturating_duration_since(at))
            .filter(|remaining| !remaining.is_zero())
    })
}

// Fail closed when a native transform cannot provide a finite, positive
// allocation. f32::min/max alone would otherwise silently discard NaN.
fn visible_edges(x: f32, y: f32, width: f32, height: f32) -> Option<[f32; 4]> {
    let right = x + width;
    let bottom = y + height;
    ([x, y, width, height, right, bottom]
        .iter()
        .all(|value| value.is_finite())
        && width > 0.0
        && height > 0.0)
        .then_some([x, y, right, bottom])
}

pub(crate) struct VteCompanion {
    root: gtk::glib::WeakRef<gtk::Box>,
    card: gtk::Box,
    glyph: gtk::Label,
    config: Rc<RefCell<Config>>,
    local: Cell<bool>,
    closed: Cell<bool>,
    motion: Cell<OrganismMotion>,
    frame: Cell<u64>,
    last_input: Cell<Option<Instant>>,
    revision: Cell<u64>,
    source: RefCell<Option<(gtk::glib::SourceId, Rc<()>)>>,
    host: RefCell<Option<(gtk::glib::WeakRef<gtk::Window>, gtk::glib::SignalHandlerId)>>,
    desktop: RefCell<Option<(gtk::Settings, gtk::glib::SignalHandlerId)>>,
    geometry_eligible: Cell<bool>,
    paint_observer: RefCell<
        Option<(
            gtk::glib::WeakRef<gtk::gdk::FrameClock>,
            gtk::glib::SignalHandlerId,
        )>,
    >,
    geometry_idle: RefCell<Option<(gtk::glib::SourceId, Rc<()>)>>,
}

impl VteCompanion {
    pub(crate) fn new(
        root: &gtk::Box,
        terminal: &vte4::Terminal,
        config: Rc<RefCell<Config>>,
        local: bool,
    ) -> Rc<Self> {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        card.set_can_target(false);
        card.set_focusable(false);
        card.set_margin_start(8);
        card.set_margin_end(8);
        card.set_margin_top(2);
        card.set_margin_bottom(2);
        card.update_property(&[gtk::accessible::Property::Description(CAPABILITY_DETAIL)]);
        let glyph = gtk::Label::builder()
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        glyph.add_css_class("monospace");
        glyph.set_width_chars(5);
        glyph.set_max_width_chars(5);
        glyph.set_can_target(false);
        glyph.set_focusable(false);
        let label = gtk::Label::new(Some("VTE · display only"));
        label.add_css_class("dim-label");
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_width_chars(1);
        label.set_max_width_chars(18);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_can_target(false);
        label.set_focusable(false);
        label.update_property(&[gtk::accessible::Property::Description(CAPABILITY_DETAIL)]);
        card.append(&glyph);
        card.append(&label);
        root.append(&card);
        let surface = Rc::new(Self {
            root: root.downgrade(),
            card,
            glyph,
            config,
            local: Cell::new(local),
            closed: Cell::new(false),
            motion: Cell::new(OrganismMotion::Static),
            frame: Cell::new(0),
            last_input: Cell::new(None),
            revision: Cell::new(0),
            source: RefCell::new(None),
            host: RefCell::new(None),
            desktop: RefCell::new(None),
            geometry_eligible: Cell::new(false),
            paint_observer: RefCell::new(None),
            geometry_idle: RefCell::new(None),
        });
        root.connect_map({
            let weak = Rc::downgrade(&surface);
            move |_| {
                if let Some(surface) = weak.upgrade() {
                    surface.refresh();
                }
            }
        });
        root.connect_unmap({
            let weak = Rc::downgrade(&surface);
            move |_| {
                if let Some(surface) = weak.upgrade() {
                    surface.refresh();
                }
            }
        });
        root.connect_root_notify({
            let weak = Rc::downgrade(&surface);
            move |_| {
                if let Some(surface) = weak.upgrade() {
                    surface.refresh();
                }
            }
        });
        let focus = gtk::EventControllerFocus::new();
        focus.connect_enter({
            let weak = Rc::downgrade(&surface);
            move |_| {
                if let Some(surface) = weak.upgrade() {
                    surface.refresh();
                }
            }
        });
        focus.connect_leave({
            let weak = Rc::downgrade(&surface);
            move |_| {
                if let Some(surface) = weak.upgrade() {
                    surface.refresh();
                }
            }
        });
        root.add_controller(focus);
        terminal.connect_commit({
            let weak = Rc::downgrade(&surface);
            move |_, _, _| {
                if let Some(surface) = weak.upgrade() {
                    // Includes programmatic feed and terminal responses. This
                    // is deliberately not a human-input or command event.
                    surface.last_input.set(Some(Instant::now()));
                    surface.refresh();
                }
            }
        });
        if let Some(desktop) = gtk::Settings::default() {
            let weak = Rc::downgrade(&surface);
            let handler = desktop.connect_gtk_enable_animations_notify(move |_| {
                if let Some(surface) = weak.upgrade() {
                    surface.refresh();
                }
            });
            *surface.desktop.borrow_mut() = Some((desktop, handler));
        }
        surface.refresh();
        surface
    }

    pub(crate) fn set_local(self: &Rc<Self>, local: bool) {
        if self.local.replace(local) != local {
            self.refresh();
        }
    }

    pub(crate) fn close(self: &Rc<Self>) {
        self.closed.set(true);
        self.refresh();
    }

    fn current_host(&self) -> Option<gtk::Window> {
        self.root.upgrade()?.root().and_downcast::<gtk::Window>()
    }

    fn bind_host(self: &Rc<Self>) {
        let next = self.current_host();
        let previous = self
            .host
            .borrow()
            .as_ref()
            .and_then(|(window, _)| window.upgrade());
        if previous == next {
            return;
        }
        let old = self.host.borrow_mut().take();
        if let Some((window, handler)) = old {
            if let Some(window) = window.upgrade() {
                window.disconnect(handler);
            }
        }
        if let Some(window) = next {
            let weak = Rc::downgrade(self);
            let handler = window.connect_is_active_notify(move |_| {
                if let Some(surface) = weak.upgrade() {
                    surface.refresh();
                }
            });
            *self.host.borrow_mut() = Some((window.downgrade(), handler));
        }
    }

    fn focused_and_mapped(&self) -> bool {
        let Some(root) = self.root.upgrade() else {
            return false;
        };
        let Some(window) = self.current_host() else {
            return false;
        };
        root.is_mapped()
            && self.card.is_mapped()
            && window.is_active()
            && gtk::prelude::GtkWindowExt::focus(&window)
                .is_some_and(|focus| focus == root || focus.is_ancestor(&root))
    }

    fn glyph_has_visible_area(&self) -> bool {
        let Some(window) = self.current_host() else {
            return false;
        };
        let Some(bounds) = self.glyph.compute_bounds(&window) else {
            return false;
        };
        let Some([mut left, mut top, mut right, mut bottom]) =
            visible_edges(bounds.x(), bounds.y(), bounds.width(), bounds.height())
        else {
            return false;
        };
        let mut ancestor = self.glyph.parent();
        // Stay within each owning pane/container's allocation, including the
        // Paned viewport when GTK gives a shrunken child its minimum size and
        // translates it outside that viewport. Opacity is intentionally not
        // part of this query: a retreated glyph must be able to recover.
        while let Some(widget) = ancestor {
            let Some(bounds) = widget.compute_bounds(&window) else {
                return false;
            };
            let Some([x, y, right_edge, bottom_edge]) =
                visible_edges(bounds.x(), bounds.y(), bounds.width(), bounds.height())
            else {
                return false;
            };
            left = left.max(x);
            top = top.max(y);
            right = right.min(right_edge);
            bottom = bottom.min(bottom_edge);
            if right <= left || bottom <= top {
                return false;
            }
            ancestor = widget.parent();
        }
        right > left && bottom > top
    }

    fn retire_geometry_idle(&self) {
        let old = self.geometry_idle.borrow_mut().take();
        if let Some((source, _)) = old {
            source.remove();
        }
    }

    fn queue_geometry_refresh(self: &Rc<Self>) {
        if self.geometry_idle.borrow().is_some() {
            return;
        }
        let ticket = Rc::new(());
        let callback_ticket = ticket.clone();
        let weak = Rc::downgrade(self);
        let source = gtk::glib::idle_add_local_once(move || {
            let Some(surface) = weak.upgrade() else {
                return;
            };
            if !surface
                .geometry_idle
                .borrow()
                .as_ref()
                .is_some_and(|(_, current)| Rc::ptr_eq(current, &callback_ticket))
            {
                return;
            }
            surface.geometry_idle.borrow_mut().take();
            surface.refresh();
        });
        *self.geometry_idle.borrow_mut() = Some((source, ticket));
    }

    fn sync_paint_observer(self: &Rc<Self>, active: bool) {
        let next = active
            .then(|| self.root.upgrade().and_then(|root| root.frame_clock()))
            .flatten();
        let previous = self
            .paint_observer
            .borrow()
            .as_ref()
            .and_then(|(clock, _)| clock.upgrade());
        if previous == next {
            if !active {
                self.retire_geometry_idle();
            }
            return;
        }
        self.retire_geometry_idle();
        let old = self.paint_observer.borrow_mut().take();
        if let Some((clock, handler)) = old {
            if let Some(clock) = clock.upgrade() {
                clock.disconnect(handler);
            }
        }
        if let Some(clock) = next {
            let weak = Rc::downgrade(self);
            // GTK generally discourages application after-paint handlers. This
            // narrow observer never requests a frame or paints inside that
            // phase. It only notices a geometry-qualification edge during an
            // already occurring native paint, then coalesces one weak idle.
            // This covers ancestor-only clipping/translation, which does not
            // necessarily invoke a child's size-allocation virtual method.
            let handler = clock.connect_after_paint(move |clock| {
                let Some(surface) = weak.upgrade() else {
                    return;
                };
                let current = surface
                    .paint_observer
                    .borrow()
                    .as_ref()
                    .and_then(|(observed, _)| observed.upgrade());
                if current.as_ref() != Some(clock) {
                    return;
                }
                let eligible = surface.glyph_has_visible_area();
                if surface.geometry_eligible.replace(eligible) != eligible {
                    surface.queue_geometry_refresh();
                }
            });
            *self.paint_observer.borrow_mut() = Some((clock.downgrade(), handler));
        }
    }

    fn retire_source(&self) {
        let old = self.source.borrow_mut().take();
        if let Some((source, _)) = old {
            source.remove();
        }
    }

    pub(crate) fn refresh(self: &Rc<Self>) {
        self.retire_source();
        let revision = self.revision.get().wrapping_add(1);
        self.revision.set(revision);
        self.bind_host();
        let (enabled, requested_motion) = {
            let config = self.config.borrow();
            (config.ascii_organism_enabled, config.ascii_organism_motion)
        };
        let motion = requested_motion.unwrap_or_else(|| {
            if gtk::Settings::default()
                .map(|settings| settings.is_gtk_enable_animations())
                .unwrap_or(true)
            {
                OrganismMotion::Full
            } else {
                OrganismMotion::Calm
            }
        });
        self.motion.set(motion);
        self.card
            .set_visible(enabled && self.local.get() && !self.closed.get());
        if self.revision.get() != revision {
            return;
        }
        let focused = self.focused_and_mapped();
        self.sync_paint_observer(focused);
        let geometry = self.glyph_has_visible_area();
        self.geometry_eligible.set(geometry);
        let active = focused && geometry;
        let retreat = retreat_remaining(self.last_input.get(), Instant::now());
        // Keep the grid and this five-character slot allocated across input,
        // focus and animation changes. Only an explicit Off/locality/close
        // boundary removes the card from layout.
        self.glyph.set_opacity(if active && retreat.is_none() {
            1.0
        } else {
            0.0
        });
        if self.revision.get() != revision {
            return;
        }
        let context = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
        let glyph = sticky_glyph_with_context(context, display_frame(motion, self.frame.get()));
        if self.glyph.text().as_str() != glyph.as_ref() {
            self.glyph.set_text(glyph.as_ref());
            if self.revision.get() != revision {
                return;
            }
        }
        // GTK property notifications can synchronously re-enter settings or
        // lifecycle code. Only the most recent refresh may own the next wake.
        if self.revision.get() != revision
            || !self.focused_and_mapped()
            || !self.glyph_has_visible_area()
        {
            return;
        }
        let retreat = retreat_remaining(self.last_input.get(), Instant::now());
        let (delay, advance_frame) = match retreat {
            Some(remaining) => (remaining.max(Duration::from_millis(1)), false),
            None if motion == OrganismMotion::Full => (SURFACE_FRAME_INTERVAL, true),
            None => return,
        };
        let ticket = Rc::new(());
        let callback_ticket = ticket.clone();
        let weak = Rc::downgrade(self);
        let source = gtk::glib::timeout_add_local_once(delay, move || {
            let Some(surface) = weak.upgrade() else {
                return;
            };
            if !surface
                .source
                .borrow()
                .as_ref()
                .is_some_and(|(_, current)| Rc::ptr_eq(current, &callback_ticket))
            {
                return;
            }
            surface.source.borrow_mut().take();
            if advance_frame && surface.focused_and_mapped() {
                surface.frame.set(surface.frame.get().wrapping_add(1));
            }
            surface.refresh();
        });
        *self.source.borrow_mut() = Some((source, ticket));
    }
}

impl Drop for VteCompanion {
    fn drop(&mut self) {
        self.retire_source();
        self.retire_geometry_idle();
        if let Some((clock, handler)) = self.paint_observer.get_mut().take() {
            if let Some(clock) = clock.upgrade() {
                clock.disconnect(handler);
            }
        }
        self.card.set_visible(false);
        if let Some(root) = self.root.upgrade() {
            root.remove(&self.card);
        }
        if let Some((window, handler)) = self.host.get_mut().take() {
            if let Some(window) = window.upgrade() {
                window.disconnect(handler);
            }
        }
        if let Some((desktop, handler)) = self.desktop.get_mut().take() {
            desktop.disconnect(handler);
        }
    }
}

#[cfg(test)]
pub(super) fn exercise_existing_fixture(
    window: &gtk::Window,
    overlay: &gtk::Overlay,
    root: &gtk::Box,
    entry: &gtk::Entry,
    view: &crate::block_view::TermView,
    settle: &impl Fn(Duration),
) {
    let mut config = Config::safe_defaults();
    config.ascii_organism_enabled = true;
    config.ascii_organism_motion = Some(OrganismMotion::Full);
    let config = Rc::new(RefCell::new(config));
    let terminal = view.vte();
    let surface = VteCompanion::new(root, terminal, config.clone(), true);
    // Same existing window, terminal and nonexecuting fixture PTY. Move the
    // existing entry outside the pane so focus leave is a genuine boundary.
    root.remove(entry);
    overlay.set_child(None::<&gtk::Widget>);
    let host = gtk::Box::new(gtk::Orientation::Vertical, 0);
    host.append(entry);
    let split = gtk::Paned::new(gtk::Orientation::Vertical);
    split.set_vexpand(true);
    split.set_shrink_start_child(true);
    split.set_shrink_end_child(true);
    let sibling = gtk::Box::new(gtk::Orientation::Vertical, 0);
    split.set_start_child(Some(root));
    split.set_end_child(Some(&sibling));
    host.append(&split);
    overlay.set_child(Some(&host));
    split.set_position(300);
    assert!(terminal.grab_focus());
    settle(Duration::from_millis(100));
    surface.refresh();
    assert!(surface.focused_and_mapped());
    assert_eq!(surface.current_host().as_ref(), Some(window));
    assert!(surface.glyph_has_visible_area());
    assert!(surface.source.borrow().is_some());
    assert!(surface.paint_observer.borrow().is_some());
    let clock = root.frame_clock().unwrap();
    let source_ticket = surface.source.borrow().as_ref().unwrap().1.clone();
    for _ in 0..3 {
        clock.emit_by_name::<()>("after-paint", &[]);
        assert!(surface.geometry_idle.borrow().is_none());
        assert!(Rc::ptr_eq(
            &surface.source.borrow().as_ref().unwrap().1,
            &source_ticket
        ));
    }
    assert_eq!(surface.glyph.opacity(), 1.0);
    assert!(!surface.card.can_target());
    assert!(!surface.glyph.can_target());
    assert!(!surface.card.is_focusable());
    assert_eq!(
        surface.glyph.accessible_role(),
        gtk::AccessibleRole::Presentation
    );
    let (glyph_min, _, _, _) = surface.glyph.measure(gtk::Orientation::Horizontal, -1);
    let (card_min, _, _, _) = surface.card.measure(gtk::Orientation::Horizontal, -1);
    assert!(
        card_min <= glyph_min * 2 + 24,
        "capability text must ellipsize rather than widen a split"
    );
    let allocation = || {
        (
            root.width(),
            root.height(),
            terminal.width(),
            terminal.height(),
            surface.card.height(),
        )
    };
    let settled_allocation = allocation();
    surface.frame.set(54);
    surface.refresh();
    settle(Duration::from_millis(140));
    assert!(surface.frame.get() >= 55);
    assert_eq!(
        surface.glyph.text().as_str(),
        sticky_glyph_with_context(
            RenderContext::new(Behavior::Idle, BodyLanguage::default(), false),
            surface.frame.get()
        )
        .as_ref(),
        "Full advances on the native source and renders the current shared frame"
    );
    assert_eq!(allocation(), settled_allocation);
    for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
        config.borrow_mut().ascii_organism_motion = Some(motion);
        surface.refresh();
        assert!(surface.source.borrow().is_none());
        assert_eq!(
            surface.glyph.text().as_str(),
            sticky_glyph_with_context(
                RenderContext::new(Behavior::Idle, BodyLanguage::default(), false),
                0
            )
            .as_ref()
        );
        assert_eq!(allocation(), settled_allocation);
    }
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Full);
    surface.refresh();
    terminal.emit_by_name::<()>("commit", &[&"x", &1u32]);
    assert_eq!(
        crate::block_view::organism_settings_test_pty_bytes(view),
        b"x",
        "the existing terminal writer receives the input exactly once"
    );
    assert_eq!(surface.glyph.opacity(), 0.0);
    assert!(surface.source.borrow().is_some());
    settle(Duration::from_millis(100));
    assert_eq!(
        allocation(),
        settled_allocation,
        "retreat never resizes or reflows the terminal"
    );
    let later = Instant::now();
    terminal.emit_by_name::<()>("commit", &[&"y", &1u32]);
    assert_eq!(
        crate::block_view::organism_settings_test_pty_bytes(view),
        b"y"
    );
    settle(Duration::from_millis(100));
    assert!(surface.last_input.get().is_some_and(|at| at >= later));
    assert_eq!(
        surface.glyph.opacity(),
        0.0,
        "later input preserves its own full retreat"
    );
    settle(
        (later + HUMAN_INPUT_RETREAT).saturating_duration_since(Instant::now())
            + Duration::from_millis(80),
    );
    assert_eq!(surface.glyph.opacity(), 1.0);
    assert_eq!(allocation(), settled_allocation);
    assert!(entry.grab_focus());
    settle(Duration::from_millis(40));
    assert_eq!(surface.glyph.opacity(), 0.0);
    assert!(surface.source.borrow().is_none());
    assert!(surface.geometry_idle.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    assert_eq!(allocation(), settled_allocation);
    assert!(terminal.grab_focus());
    settle(Duration::from_millis(40));
    assert!(surface.source.borrow().is_some());
    // A partially collapsed Paned keeps its child mapped at a translated
    // minimum allocation. The bottom margin is visible but the glyph is not.
    split.set_position(1);
    settle(Duration::from_millis(100));
    assert!(root.is_mapped());
    assert!(!surface.glyph_has_visible_area());
    assert!(surface.source.borrow().is_none());
    assert!(surface.geometry_idle.borrow().is_none());
    assert!(
        surface.paint_observer.borrow().is_some(),
        "passive native paints can observe recovery without requesting frames"
    );
    split.set_position(300);
    settle(Duration::from_millis(100));
    assert!(surface.glyph_has_visible_area());
    assert!(surface.source.borrow().is_some());
    assert_eq!(surface.glyph.opacity(), 1.0);
    split.set_position(0);
    settle(Duration::from_millis(60));
    assert!(!root.is_mapped());
    assert!(surface.source.borrow().is_none());
    assert!(surface.geometry_idle.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    split.set_position(300);
    assert!(terminal.grab_focus());
    settle(Duration::from_millis(60));
    assert!(surface.source.borrow().is_some());
    let old_clock = root.frame_clock().unwrap();
    split.set_start_child(None::<&gtk::Widget>);
    settle(Duration::from_millis(40));
    assert!(surface.host.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    assert!(surface.source.borrow().is_none());
    old_clock.emit_by_name::<()>("after-paint", &[]);
    assert!(
        surface.geometry_idle.borrow().is_none(),
        "retired clock cannot queue a detached pane"
    );
    split.set_start_child(Some(root));
    split.set_position(300);
    assert!(terminal.grab_focus());
    settle(Duration::from_millis(80));
    assert!(surface.source.borrow().is_some());
    assert!(surface.host.borrow().is_some());
    config.borrow_mut().ascii_organism_enabled = false;
    surface.refresh();
    assert!(!surface.card.is_visible());
    assert!(surface.source.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    config.borrow_mut().ascii_organism_enabled = true;
    surface.refresh();
    settle(Duration::from_millis(60));
    surface.set_local(false);
    assert!(!surface.card.is_visible());
    assert!(surface.source.borrow().is_none());
    surface.set_local(true);
    settle(Duration::from_millis(60));
    assert!(surface.source.borrow().is_some());
    surface.close();
    assert!(!surface.card.is_visible());
    assert!(surface.source.borrow().is_none());
    assert!(surface.geometry_idle.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    surface.refresh();
    assert!(
        !surface.card.is_visible(),
        "settings cannot revive an exited or failed child"
    );
    let weak = Rc::downgrade(&surface);
    let card_weak = surface.card.downgrade();
    let glyph_weak = surface.glyph.downgrade();
    drop(surface);
    assert!(weak.upgrade().is_none());
    assert!(card_weak.upgrade().is_none());
    assert!(glyph_weak.upgrade().is_none());
    // Closing above verifies the permanent child boundary. A second surface
    // in the same fixture separately proves Drop retires still-active native
    // resources, including the paint-edge idle before it can run.
    let active = VteCompanion::new(root, terminal, config.clone(), true);
    settle(Duration::from_millis(60));
    assert!(active.focused_and_mapped());
    assert!(active.source.borrow().is_some());
    let context = gtk::glib::MainContext::default();
    let live_source = context
        .find_source_by_id(&active.source.borrow().as_ref().unwrap().0)
        .unwrap();
    active.geometry_eligible.set(false);
    clock.emit_by_name::<()>("after-paint", &[]);
    let idle_ticket = active.geometry_idle.borrow().as_ref().unwrap().1.clone();
    for _ in 0..3 {
        clock.emit_by_name::<()>("after-paint", &[]);
        assert!(Rc::ptr_eq(
            &active.geometry_idle.borrow().as_ref().unwrap().1,
            &idle_ticket
        ));
    }
    let idle_source = context
        .find_source_by_id(&active.geometry_idle.borrow().as_ref().unwrap().0)
        .unwrap();
    let weak = Rc::downgrade(&active);
    let card_weak = active.card.downgrade();
    let glyph_weak = active.glyph.downgrade();
    drop(active);
    assert!(weak.upgrade().is_none());
    assert!(card_weak.upgrade().is_none());
    assert!(glyph_weak.upgrade().is_none());
    assert!(live_source.is_destroyed());
    assert!(idle_source.is_destroyed());
    clock.emit_by_name::<()>("after-paint", &[]);
    assert!(
        crate::block_view::organism_settings_test_pty_bytes(view).is_empty(),
        "display and lifecycle actions never write terminal bytes"
    );
    println!("VTE_COMPANION_NATIVE full_animates=true calm_static_hold=true allocation_stable=true commit_passes_once=true clipped_timer_retires=true paint_edge_restores=true hidden_closed_drop_retired=true");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_display_uses_shared_frames_without_changing_slot_width() {
        let context = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
        let first = sticky_glyph_with_context(context, 0);
        let mut changed = false;
        for frame in 0..120 {
            let glyph =
                sticky_glyph_with_context(context, display_frame(OrganismMotion::Full, frame));
            assert!(glyph.is_ascii());
            assert_eq!(glyph.len(), 5);
            changed |= glyph != first;
            for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
                assert_eq!(
                    sticky_glyph_with_context(context, display_frame(motion, frame)),
                    first
                );
            }
        }
        assert!(changed, "Full retains the shared renderer's animation");
    }

    #[test]
    fn geometry_requires_finite_positive_native_bounds() {
        assert_eq!(
            visible_edges(1.0, 2.0, 3.0, 4.0),
            Some([1.0, 2.0, 4.0, 6.0])
        );
        for (x, y, width, height) in [
            (f32::NAN, 0.0, 1.0, 1.0),
            (0.0, f32::INFINITY, 1.0, 1.0),
            (0.0, 0.0, 0.0, 1.0),
            (0.0, 0.0, 1.0, -1.0),
            (0.0, 0.0, f32::NAN, 1.0),
            (0.0, 0.0, 1.0, f32::INFINITY),
            (f32::MAX, 0.0, f32::MAX, 1.0),
        ] {
            assert_eq!(visible_edges(x, y, width, height), None);
        }
    }

    #[test]
    fn conservative_input_retreat_uses_the_latest_absolute_boundary() {
        let start = Instant::now();
        assert_eq!(retreat_remaining(None, start), None);
        assert_eq!(
            retreat_remaining(Some(start), start),
            Some(HUMAN_INPUT_RETREAT)
        );
        let later = start + Duration::from_millis(500);
        assert_eq!(
            retreat_remaining(Some(later), start + HUMAN_INPUT_RETREAT),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            retreat_remaining(Some(later), later + HUMAN_INPUT_RETREAT),
            None
        );
        assert_eq!(
            retreat_remaining(
                Some(later),
                later + HUMAN_INPUT_RETREAT + Duration::from_nanos(1)
            ),
            None
        );
    }

    #[test]
    fn vte_display_never_invents_work_or_installs_an_interaction_writer() {
        let source = include_str!("vte_surface.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "feed_child(",
            "write_bytes(",
            "write_input(",
            "LiveAttention::",
            "GentleInteraction::",
            "connect_motion(",
            "connect_contents_changed(",
            "WatchCommand",
            "WatchRhythm::",
        ] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
        assert!(production.contains("terminal.connect_commit("));
        assert!(production.contains("Rc::ptr_eq(current, &callback_ticket)"));
        assert!(production.contains("self.glyph"));
        assert!(production.contains(".set_opacity("));
        let settings = include_str!("../dialogs/settings.rs");
        let production = settings.split("#[cfg(test)]").next().unwrap();
        for detail in [
            "Traditional VTE shows a neutral companion",
            "Command activity and alternate-screen state are unavailable",
            "greetings and Hello are unavailable",
        ] {
            assert!(production.contains(detail), "{detail}");
        }
        let backend = include_str!("../terminal/vte.rs");
        let spawn = backend
            .split("pub(crate) fn spawn_shell(")
            .nth(1)
            .unwrap()
            .split("pub struct VteInit")
            .next()
            .unwrap();
        assert_eq!(spawn.matches("on_launch_failed();").count(), 2);
        let action = include_str!("../action_ops.rs");
        let greeting = action
            .split("pub(crate) fn greet_ascii_organism(")
            .nth(1)
            .unwrap()
            .split("fn emit_block_action(")
            .next()
            .unwrap();
        assert!(greeting.contains("vte_surface::HELLO_UNAVAILABLE"));
        assert!(
            greeting.find("hello_target_is_current").unwrap()
                < greeting.find("vte_surface::HELLO_UNAVAILABLE").unwrap()
        );
    }
}
