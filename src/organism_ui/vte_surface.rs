//! Native, display-only companion outside the conventional VTE grid.
//!
//! VTE 0.76 exposes neither trustworthy command state nor alternate-screen
//! state. This surface never enters LiveAttention, interprets output, or writes
//! to the terminal. Its clock owns only native painting and visual retreat.

use super::SURFACE_FRAME_INTERVAL;
use crate::config::{Config, OrganismMotion};
use gtk::prelude::*;
use jterm_core::organism::{
    sprite_frame_with_context, sticky_glyph_with_context, Behavior, BodyLanguage, RenderContext,
};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};
use vte4::TerminalExt;

pub(crate) const CAPABILITY_DETAIL: &str = "Traditional VTE shows a neutral companion outside the terminal grid. Command activity and alternate-screen state are unavailable, so hover greetings and Hello are unavailable. Sent input briefly hides the companion without moving the terminal. Use the command palette to expand or collapse this neutral companion.";
pub(crate) const HELLO_UNAVAILABLE: &str = "Hello is unavailable in traditional VTE: command activity and alternate-screen state cannot be verified.";

pub(super) fn display_frame(motion: OrganismMotion, frame: u64) -> u64 {
    super::surface_time::display_frame(motion == OrganismMotion::Full, frame)
}

pub(super) fn retreat_remaining(last_input: Option<Instant>, now: Instant) -> Option<Duration> {
    super::surface_time::input_retreat_remaining(
        last_input.map(|at| now.saturating_duration_since(at)),
    )
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presentation {
    Compact,
    Expanded,
    Fallback,
    Hidden,
}

fn presentation(expanded: bool, full_fits: bool, fallback_fits: bool) -> Presentation {
    if !expanded {
        if fallback_fits {
            Presentation::Compact
        } else {
            Presentation::Hidden
        }
    } else if full_fits {
        Presentation::Expanded
    } else if fallback_fits {
        Presentation::Fallback
    } else {
        Presentation::Hidden
    }
}

// Adult Idle's weighted full-body clip lasts 160 frames; its compact glyph
// repeats every 60. Reserve their complete joint cycle once at construction.
const NEUTRAL_FRAME_PERIOD: u64 = 480;

fn neutral_candidates() -> (Vec<String>, usize) {
    let context = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
    let mut candidates = Vec::new();
    for frame in 0..NEUTRAL_FRAME_PERIOD {
        let text = sprite_frame_with_context(context, frame);
        if !candidates.iter().any(|known| known == text.as_ref()) {
            candidates.push(text.into_owned());
        }
    }
    let full_count = candidates.len();
    for frame in 0..NEUTRAL_FRAME_PERIOD {
        let text = sticky_glyph_with_context(context, frame);
        if !candidates[full_count..]
            .iter()
            .any(|known| known == text.as_ref())
        {
            candidates.push(text.into_owned());
        }
    }
    (candidates, full_count)
}

fn contains_edges(outer: [f32; 4], inner: [f32; 4]) -> bool {
    outer.iter().chain(inner.iter()).all(|v| v.is_finite())
        && outer[2] > outer[0]
        && outer[3] > outer[1]
        && inner[2] > inner[0]
        && inner[3] > inner[1]
        && inner[0] >= outer[0]
        && inner[1] >= outer[1]
        && inner[2] <= outer[2]
        && inner[3] <= outer[3]
}

pub(crate) struct VteCompanion {
    root: gtk::glib::WeakRef<gtk::Box>,
    card: gtk::Box,
    glyph: gtk::Label,
    capability: gtk::Label,
    expanded_slot: gtk::Overlay,
    candidates: Vec<gtk::Label>,
    full_count: usize,
    expanded: Cell<bool>,
    mounting: Cell<bool>,
    presentation: Cell<Presentation>,
    config: Rc<RefCell<Config>>,
    local: Cell<bool>,
    closed: Cell<bool>,
    motion: Cell<OrganismMotion>,
    frame: Cell<u64>,
    last_input: Cell<Option<Instant>>,
    revision: Cell<u64>,
    source: RefCell<Option<(gtk::glib::SourceId, Rc<()>)>>,
    host: RefCell<
        Option<(
            gtk::glib::WeakRef<gtk::Window>,
            [gtk::glib::SignalHandlerId; 2],
        )>,
    >,
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
        // Every neutral candidate is both measured and painted by the same
        // native label. Measured overlays reserve the maximum requisition;
        // only opacity changes between frames, retreat and width fallback.
        // GTK font/CSS/scale invalidation remeasures the real labels directly.
        let expanded_slot = gtk::Overlay::new();
        expanded_slot.set_can_target(false);
        expanded_slot.set_focusable(false);
        expanded_slot.set_overflow(gtk::Overflow::Hidden);
        let (texts, full_count) = neutral_candidates();
        let candidates: Vec<_> = texts
            .into_iter()
            .map(|text| {
                let candidate = gtk::Label::builder()
                    .accessible_role(gtk::AccessibleRole::Presentation)
                    .label(text)
                    .build();
                candidate.add_css_class("monospace");
                candidate.set_can_target(false);
                candidate.set_focusable(false);
                candidate.set_opacity(0.0);
                // GTK owns Hidden during map/unmap. Presentation leaf labels are
                // omitted by the accessibility backend without overriding it.
                candidate
            })
            .collect();
        expanded_slot.set_child(Some(&candidates[0]));
        for candidate in &candidates[1..] {
            expanded_slot.add_overlay(candidate);
            expanded_slot.set_measure_overlay(candidate, true);
            expanded_slot.set_clip_overlay(candidate, true);
        }
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
            capability: label,
            expanded_slot,
            candidates,
            full_count,
            expanded: Cell::new(false),
            mounting: Cell::new(false),
            presentation: Cell::new(Presentation::Compact),
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
                    // GTK emits the crossing before Window::focus points to
                    // the new widget. Reuse the one cancellable weak idle to
                    // observe the committed focus after this event stack.
                    surface.queue_geometry_refresh();
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

    pub(crate) fn set_expanded(self: &Rc<Self>, expanded: bool) {
        if self.closed.get()
            || !self.local.get()
            || !self.config.borrow().ascii_organism_enabled
            || !self.focused_and_mapped()
            || self.expanded.get() == expanded
        {
            return;
        }
        self.expanded.set(expanded);
        let detail = format!(
            "{} Current presentation: {}.",
            CAPABILITY_DETAIL,
            if expanded { "expanded" } else { "compact" }
        );
        self.capability
            .update_property(&[gtk::accessible::Property::Description(&detail)]);
        self.refresh();
    }

    fn sync_presentation_layout(&self, revision: u64) {
        struct Mounting<'a>(&'a Cell<bool>);
        impl Drop for Mounting<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        self.mounting.set(true);
        let _mounting = Mounting(&self.mounting);
        // Native unparent/map/property signals may synchronously request the
        // opposite state. The reentrant refresh retires the revision and queues
        // the existing idle, rather than mutating this hierarchy recursively.
        // Reconcile actual parents, never assume a preceding remove completed.
        if self.expanded.get() {
            if self.glyph.parent().as_ref() == Some(self.card.upcast_ref::<gtk::Widget>()) {
                self.card.remove(&self.glyph);
                if self.revision.get() != revision {
                    return;
                }
            }
            if self.expanded_slot.parent().is_none() {
                self.card.prepend(&self.expanded_slot);
                // The caller checks the revision before doing any more work.
            }
        } else {
            if self.expanded_slot.parent().as_ref() == Some(self.card.upcast_ref::<gtk::Widget>()) {
                self.card.remove(&self.expanded_slot);
                if self.revision.get() != revision {
                    return;
                }
            }
            if self.glyph.parent().is_none() {
                self.card.prepend(&self.glyph);
            }
        }
    }

    fn geometry_widget(&self) -> &gtk::Widget {
        if self.expanded.get() {
            self.expanded_slot.upcast_ref()
        } else {
            self.glyph.upcast_ref()
        }
    }

    // Use the actual candidate Pango ink AND logical bounds, transformed into
    // the host coordinate space, and require complete ancestor containment.
    fn candidate_fits(&self, candidate: &gtk::Label) -> bool {
        let Some(window) = self.current_host() else {
            return false;
        };
        if !candidate.is_mapped() {
            return false;
        }
        let (ink, logical) = candidate.layout().pixel_extents();
        let (ox, oy) = candidate.layout_offsets();
        let left = i64::from(ink.x().min(logical.x())) + i64::from(ox);
        let top = i64::from(ink.y().min(logical.y())) + i64::from(oy);
        let right = (i64::from(ink.x()) + i64::from(ink.width()))
            .max(i64::from(logical.x()) + i64::from(logical.width()))
            + i64::from(ox);
        let bottom = (i64::from(ink.y()) + i64::from(ink.height()))
            .max(i64::from(logical.y()) + i64::from(logical.height()))
            + i64::from(oy);
        let mut edges = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for (x, y) in [(left, top), (right, top), (left, bottom), (right, bottom)] {
            let Some(point) =
                candidate.compute_point(&window, &gtk::graphene::Point::new(x as f32, y as f32))
            else {
                return false;
            };
            if !point.x().is_finite() || !point.y().is_finite() {
                return false;
            }
            edges[0] = edges[0].min(point.x());
            edges[1] = edges[1].min(point.y());
            edges[2] = edges[2].max(point.x());
            edges[3] = edges[3].max(point.y());
        }
        let mut ancestor = Some(candidate.clone().upcast::<gtk::Widget>());
        while let Some(widget) = ancestor {
            let Some(bounds) = widget.compute_bounds(&window) else {
                return false;
            };
            let Some(outer) =
                visible_edges(bounds.x(), bounds.y(), bounds.width(), bounds.height())
            else {
                return false;
            };
            if !contains_edges(outer, edges) {
                return false;
            }
            ancestor = widget.parent();
        }
        true
    }

    fn current_presentation(&self) -> Presentation {
        if !self.expanded.get() {
            return presentation(false, false, self.candidate_fits(&self.glyph));
        }
        let full_fits = self.candidates[..self.full_count]
            .iter()
            .all(|c| self.candidate_fits(c));
        let fallback_fits = !full_fits
            && self.candidates[self.full_count..]
                .iter()
                .all(|c| self.candidate_fits(c));
        presentation(true, full_fits, fallback_fits)
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
                for handler in handler {
                    window.disconnect(handler);
                }
            }
        }
        if let Some(window) = next {
            let weak = Rc::downgrade(self);
            let active = window.connect_is_active_notify(move |window| {
                if let Some(surface) = weak.upgrade() {
                    if surface.current_host().as_ref() == Some(window) {
                        surface.refresh();
                    }
                }
            });
            let weak = Rc::downgrade(self);
            // Crossing controller state can disagree with actual root focus.
            // Window notifies after committing its actual focus widget.
            // Observe that final fact without another timer or cached focus.
            let focus = window.connect_focus_widget_notify(move |window| {
                if let Some(surface) = weak.upgrade() {
                    if surface.current_host().as_ref() == Some(window) {
                        surface.refresh();
                    }
                }
            });
            *self.host.borrow_mut() = Some((window.downgrade(), [active, focus]));
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
        let Some(bounds) = self.geometry_widget().compute_bounds(&window) else {
            return false;
        };
        let Some([mut left, mut top, mut right, mut bottom]) =
            visible_edges(bounds.x(), bounds.y(), bounds.width(), bounds.height())
        else {
            return false;
        };
        let mut ancestor = self.geometry_widget().parent();
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
        if self.closed.get() || self.geometry_idle.borrow().is_some() {
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
                let choice = surface.current_presentation();
                let changed = surface.presentation.replace(choice) != choice;
                if surface.geometry_eligible.replace(eligible) != eligible || changed {
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
        // Closing wins even during a synchronous hierarchy mutation. It must
        // retire all wakes before returning, without a topology cleanup idle.
        if self.closed.get() {
            self.sync_paint_observer(false);
            self.card.set_visible(false);
            return;
        }
        if self.mounting.get() {
            self.queue_geometry_refresh();
            return;
        }
        self.bind_host();
        self.sync_presentation_layout(revision);
        if self.revision.get() != revision {
            self.queue_geometry_refresh();
            return;
        }
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
        // Qualify the exact compact frame that will be painted, including
        // native ink overhang. Text changes precede containment, never follow it.
        let context = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
        let frame = display_frame(motion, self.frame.get());
        if !self.expanded.get() {
            let text = sticky_glyph_with_context(context, frame);
            if self.glyph.text().as_str() != text.as_ref() {
                self.glyph.set_text(text.as_ref());
                if self.revision.get() != revision {
                    return;
                }
            }
        }
        let choice = self.current_presentation();
        self.presentation.set(choice);
        let active = focused && geometry && choice != Presentation::Hidden;
        let retreat = retreat_remaining(self.last_input.get(), Instant::now());
        // Preserve the expanded measurement envelope throughout all paint-only
        // changes. Invisible candidates remain visible widgets and keep their
        // native min/natural requisitions.
        let painted = active && retreat.is_none();
        if self.expanded.get() {
            let text = if choice == Presentation::Expanded {
                sprite_frame_with_context(context, frame)
            } else {
                sticky_glyph_with_context(context, frame)
            };
            for candidate in &self.candidates {
                candidate.set_opacity(if painted && candidate.text().as_str() == text.as_ref() {
                    1.0
                } else {
                    0.0
                });
                if self.revision.get() != revision {
                    return;
                }
            }
        } else {
            self.glyph.set_opacity(if painted { 1.0 } else { 0.0 });
            if self.revision.get() != revision {
                return;
            }
        }
        // GTK property notifications can synchronously re-enter settings or
        // lifecycle code. Only the most recent refresh may own the next wake.
        if self.revision.get() != revision
            || !self.focused_and_mapped()
            || !self.glyph_has_visible_area()
            || self.current_presentation() == Presentation::Hidden
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
                for handler in handler {
                    window.disconnect(handler);
                }
            }
        }
        if let Some((desktop, handler)) = self.desktop.get_mut().take() {
            desktop.disconnect(handler);
        }
    }
}

#[cfg(test)]
fn fixture_state(surface: &VteCompanion, terminal: &vte4::Terminal) -> String {
    let root = surface.root.upgrade();
    let host = surface.current_host();
    let focus = host.as_ref().and_then(gtk::prelude::GtkWindowExt::focus);
    let focus_in_root = root.as_ref().is_some_and(|root| {
        focus
            .as_ref()
            .is_some_and(|focus| focus == root || focus.is_ancestor(root))
    });
    format!(
        "root_mapped={} card_mapped={} glyph_mapped={} window_active={} terminal_has_focus={} terminal_is_focus={} focus_in_root={} geometry={} source={} paint={} idle={} root_size={:?} glyph_size={:?} frame={} revision={} focus_type={:?} candidate_opacity={:?} controller_contains_focus={:?}",
        root.as_ref().is_some_and(|root| root.is_mapped()),
        surface.card.is_mapped(),
        surface.glyph.is_mapped(),
        host.as_ref().is_some_and(|host| host.is_active()),
        terminal.has_focus(),
        terminal.is_focus(),
        focus_in_root,
        surface.glyph_has_visible_area(),
        surface.source.borrow().is_some(),
        surface.paint_observer.borrow().is_some(),
        surface.geometry_idle.borrow().is_some(),
        root.as_ref().map(|root| (root.width(), root.height())),
        (surface.glyph.width(), surface.glyph.height()),
        surface.frame.get(),
        surface.revision.get(),
        focus.as_ref().map(|widget| widget.type_().name()),
        surface.candidates.iter().map(|candidate| candidate.opacity()).collect::<Vec<_>>(),
        root.as_ref().map(|root| {
            let controllers = root.observe_controllers();
            (0..controllers.n_items()).filter_map(|index| {
                controllers.item(index).and_then(|controller| controller.downcast::<gtk::EventControllerFocus>().ok())
                    .map(|controller| controller.contains_focus())
            }).collect::<Vec<_>>()
        })
    )
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
    assert!(
        surface.source.borrow().is_some(),
        "{}",
        fixture_state(&surface, terminal)
    );
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
    // Extended presentation uses the same native fixture and terminal.
    exercise_expanded_presentation(&surface, terminal, view, entry, &split, &config, settle);
    exercise_shared_adapter_trace(&surface, terminal, view, entry, &config, settle);
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
    assert!(
        surface.source.borrow().is_some(),
        "{}",
        fixture_state(&surface, terminal)
    );
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
        (later + super::HUMAN_INPUT_RETREAT).saturating_duration_since(Instant::now())
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
    assert!(terminal.has_focus());
    assert!(surface.focused_and_mapped());
    assert!(surface.geometry_idle.borrow().is_none());
    let context = gtk::glib::MainContext::default();
    let old_focus_source = context
        .find_source_by_id(&surface.source.borrow().as_ref().unwrap().0)
        .unwrap();
    // The final Window focus notification commits the Full source immediately
    // and retires the earlier crossing idle. A subsequent blur must destroy
    // that source before returning, with no queued focus revival.
    assert!(entry.grab_focus());
    assert!(surface.geometry_idle.borrow().is_none());
    assert!(surface.source.borrow().is_none());
    assert!(old_focus_source.is_destroyed());
    assert!(surface.paint_observer.borrow().is_none());
    settle(Duration::from_millis(40));
    assert!(surface.source.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    assert_eq!(surface.glyph.opacity(), 0.0);
    assert!(terminal.grab_focus());
    assert!(terminal.has_focus());
    assert!(surface.focused_and_mapped());
    assert!(surface.geometry_idle.borrow().is_none());
    assert!(surface.source.borrow().is_some());
    settle(Duration::from_millis(40));
    assert!(
        surface.source.borrow().is_some(),
        "focus restore: {}",
        fixture_state(&surface, terminal)
    );
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
    assert!(
        surface.source.borrow().is_some(),
        "{}",
        fixture_state(&surface, terminal)
    );
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
    assert!(
        surface.source.borrow().is_some(),
        "{}",
        fixture_state(&surface, terminal)
    );
    let old_clock = root.frame_clock().unwrap();
    split.set_start_child(None::<&gtk::Widget>);
    settle(Duration::from_millis(40));
    assert!(surface.host.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    assert!(surface.source.borrow().is_none());
    let detached_revision = surface.revision.get();
    window.notify("focus-widget");
    window.notify("is-active");
    assert_eq!(surface.revision.get(), detached_revision);
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
    assert!(
        surface.source.borrow().is_some(),
        "{}",
        fixture_state(&surface, terminal)
    );
    assert!(surface.host.borrow().is_some());
    config.borrow_mut().ascii_organism_enabled = false;
    surface.refresh();
    surface.set_expanded(true);
    assert!(!surface.expanded.get());
    assert!(!surface.card.is_visible());
    assert!(surface.source.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    config.borrow_mut().ascii_organism_enabled = true;
    surface.refresh();
    settle(Duration::from_millis(60));
    surface.set_local(false);
    surface.set_expanded(true);
    assert!(!surface.expanded.get());
    assert!(!surface.card.is_visible());
    assert!(surface.source.borrow().is_none());
    surface.set_local(true);
    settle(Duration::from_millis(60));
    assert!(
        surface.source.borrow().is_some(),
        "{}",
        fixture_state(&surface, terminal)
    );
    let closed_during_mount = Rc::new(Cell::new(false));
    let close_handler = surface.glyph.connect_parent_notify({
        let weak = Rc::downgrade(&surface);
        let closed = closed_during_mount.clone();
        move |glyph| {
            if glyph.parent().is_none() && !closed.replace(true) {
                if let Some(surface) = weak.upgrade() {
                    surface.close();
                    assert!(surface.source.borrow().is_none());
                    assert!(surface.geometry_idle.borrow().is_none());
                    assert!(surface.paint_observer.borrow().is_none());
                }
            }
        }
    });
    surface.set_expanded(true);
    surface.glyph.disconnect(close_handler);
    assert!(closed_during_mount.get());
    assert!(surface.closed.get());
    assert!(!surface.mounting.get());
    assert!(surface.expanded_slot.parent().is_none());
    assert!(!surface.card.is_visible());
    assert!(surface.source.borrow().is_none());
    assert!(surface.geometry_idle.borrow().is_none());
    assert!(surface.paint_observer.borrow().is_none());
    surface.refresh();
    assert!(
        !surface.card.is_visible(),
        "settings cannot revive an exited or failed child"
    );
    assert!(surface.expanded_slot.parent().is_none());
    assert!(surface.geometry_idle.borrow().is_none());
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
    exercise_production_grid_fixture(window, overlay, terminal, settle);
    assert!(
        crate::block_view::organism_settings_test_pty_bytes(view).is_empty(),
        "display and lifecycle actions never write terminal bytes"
    );
    println!("VTE_COMPANION_NATIVE full_animates=true calm_static_hold=true allocation_stable=true commit_passes_once=true clipped_timer_retires=true paint_edge_restores=true hidden_closed_drop_retired=true");
}

#[cfg(test)]
fn exercise_production_grid_fixture(
    window: &gtk::Window,
    overlay: &gtk::Overlay,
    original_terminal: &vte4::Terminal,
    settle: &impl Fn(Duration),
) {
    let original_child = overlay.child().expect("existing fixture content");
    gtk::prelude::RootExt::set_focus(window, None::<&gtk::Widget>);
    overlay.set_child(None::<&gtk::Widget>);
    let mut settings = Config::safe_defaults();
    settings.ascii_organism_enabled = true;
    settings.ascii_organism_motion = Some(OrganismMotion::Static);
    let config = Rc::new(RefCell::new(settings));
    // Only a terminal widget: no Relm component init, spawn, PTY or process.
    let terminal = crate::terminal::vte::create_terminal(&config.borrow());
    assert!(terminal.pty().is_none());
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let grid = crate::terminal::vte::mount_terminal_grid(&root, &terminal);
    let surface = VteCompanion::new(&root, &terminal, config.clone(), true);
    overlay.set_child(Some(&root));
    assert!(terminal.grab_focus());
    settle(Duration::from_millis(100));
    assert!(surface.focused_and_mapped());
    let requisition = || {
        let h = surface.card.measure(gtk::Orientation::Horizontal, -1);
        (h, surface.card.measure(gtk::Orientation::Vertical, h.1))
    };
    let compact_req = requisition();
    let compact = settled_fixture_layout(&surface, grid.upcast_ref(), &terminal, settle);
    surface.set_expanded(true);
    assert!(surface.expanded.get());
    let expanded = settled_fixture_layout(&surface, grid.upcast_ref(), &terminal, settle);
    let expanded_req = requisition();
    assert_eq!(surface.current_presentation(), Presentation::Expanded);
    assert_fixture_budget(compact, expanded);
    assert_eq!(
        compact[7] - expanded[7],
        compact[5] - expanded[5],
        "production VTE/grid budget: compact={compact:?}, expanded={expanded:?}"
    );
    assert!(expanded_req.1 .0 > compact_req.1 .0);
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Full);
    for frame in 0..NEUTRAL_FRAME_PERIOD {
        surface.frame.set(frame);
        surface.refresh();
        assert_eq!(requisition(), expanded_req);
    }
    assert_eq!(
        settled_fixture_layout(&surface, grid.upcast_ref(), &terminal, settle),
        expanded
    );
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Static);
    surface.refresh();
    terminal.emit_by_name::<()>("commit", &[&"x", &1u32]);
    assert!(surface
        .candidates
        .iter()
        .all(|label| label.opacity() == 0.0));
    assert_eq!(
        settled_fixture_layout(&surface, grid.upcast_ref(), &terminal, settle),
        expanded,
        "input retreat must not reallocate the production VTE/grid"
    );
    surface.set_expanded(false);
    assert!(!surface.expanded.get());
    assert_eq!(
        settled_fixture_layout(&surface, grid.upcast_ref(), &terminal, settle),
        compact
    );
    assert_eq!(requisition(), compact_req);
    assert!(terminal.pty().is_none());
    surface.close();
    assert!(surface.source.borrow().is_none());
    assert!(surface.geometry_idle.borrow().is_none());
    let weak = Rc::downgrade(&surface);
    drop(surface);
    assert!(weak.upgrade().is_none());
    gtk::prelude::RootExt::set_focus(window, None::<&gtk::Widget>);
    overlay.set_child(None::<&gtk::Widget>);
    overlay.set_child(Some(&original_child));
    assert!(original_terminal.grab_focus());
    settle(Duration::from_millis(100));
    assert!(gtk::prelude::RootExt::focus(window).is_some_and(|focus| {
        focus == *original_terminal.upcast_ref::<gtk::Widget>()
            || focus.is_ancestor(original_terminal)
    }));
    assert!(terminal.pty().is_none());
}

#[cfg(test)]
fn exercise_shared_adapter_trace(
    surface: &Rc<VteCompanion>,
    terminal: &vte4::Terminal,
    view: &crate::block_view::TermView,
    entry: &gtk::Entry,
    config: &Rc<RefCell<Config>>,
    settle: &impl Fn(Duration),
) {
    use super::surface_adapter_trace::{run, Host, Observed};
    assert!(crate::block_view::organism_settings_test_pty_bytes(view).is_empty());
    run(Host::NeutralVte, |step| {
        let old_source = surface
            .source
            .borrow()
            .as_ref()
            .and_then(|(id, _)| gtk::glib::MainContext::default().find_source_by_id(id));
        {
            let mut config = config.borrow_mut();
            config.ascii_organism_enabled = step.enabled;
            config.ascii_organism_motion = Some(step.motion);
        }
        if step.focused {
            assert!(terminal.grab_focus());
        } else {
            assert!(entry.grab_focus());
        }
        surface.last_input.set(Some(if step.retreat {
            Instant::now()
        } else {
            Instant::now() - super::HUMAN_INPUT_RETREAT
        }));
        surface.refresh();
        assert!(
            crate::block_view::organism_settings_test_pty_bytes(view).is_empty(),
            "before settle: {}",
            step.name
        );
        settle(Duration::from_millis(120));
        surface.refresh();
        assert!(
            crate::block_view::organism_settings_test_pty_bytes(view).is_empty(),
            "after settle: {}",
            step.name
        );
        if !step.enabled {
            // The preceding Full step owns a live source. Disable must retire
            // that exact epoch, with no revival after a former frame interval.
            assert!(old_source.is_some_and(|source| source.is_destroyed()));
            assert!(surface.geometry_idle.borrow().is_none());
        }
        let context = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
        assert_eq!(
            surface.glyph.text().as_str(),
            sticky_glyph_with_context(context, display_frame(step.motion, surface.frame.get()))
                .as_ref(),
            "VTE stays neutral through {}",
            step.name
        );
        Observed {
            primary_painted: surface.glyph.is_mapped() && surface.glyph.opacity() == 1.0,
            card_visible: surface.card.is_visible(),
            wake_pending: surface.source.borrow().is_some(),
        }
    });
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Full);
    surface.last_input.set(None);
    surface.refresh();
    assert!(crate::block_view::organism_settings_test_pty_bytes(view).is_empty());
}

#[cfg(test)]
fn assert_accessible_hidden(label: &gtk::Label, hidden: bool) {
    use gtk::glib::translate::{from_glib_full, ToGlibPtr};
    // The existing fixture alone starts with GTK_A11Y=test. GTK's varargs
    // collector consumes Hidden as gboolean (C int); mismatch text is owned.
    let expected: gtk::glib::ffi::gboolean = i32::from(hidden);
    let mismatch: Option<gtk::glib::GString> = unsafe {
        from_glib_full(gtk::ffi::gtk_test_accessible_check_state(
            label.upcast_ref::<gtk::Accessible>().to_glib_none().0,
            gtk::ffi::GTK_ACCESSIBLE_STATE_HIDDEN,
            expected,
        ))
    };
    assert!(
        mismatch.is_none(),
        "unexpected accessible Hidden: {mismatch:?}"
    );
}

#[cfg(test)]
fn settled_fixture_layout(
    surface: &Rc<VteCompanion>,
    slot: &gtk::Widget,
    terminal: &vte4::Terminal,
    settle: &impl Fn(Duration),
) -> [i32; 14] {
    let root = surface.root.upgrade().expect("live fixture root");
    assert_eq!(
        slot.parent().as_ref(),
        Some(root.upcast_ref::<gtk::Widget>())
    );
    assert_eq!(
        surface.card.parent().as_ref(),
        Some(root.upcast_ref::<gtk::Widget>())
    );
    let sample = Rc::new({
        let root = root.clone();
        let card = surface.card.clone();
        let slot = slot.clone();
        let terminal = terminal.clone();
        move || {
            let paned = root.parent().and_downcast::<gtk::Paned>();
            let axis = paned
                .as_ref()
                .map(|p| match p.orientation() {
                    gtk::Orientation::Horizontal => 0,
                    gtk::Orientation::Vertical => 1,
                    _ => -1,
                })
                .unwrap_or(-1);
            [
                root.width(),
                root.height(),
                card.width(),
                card.height(),
                slot.width(),
                slot.height(),
                terminal.width(),
                terminal.height(),
                axis,
                paned.as_ref().map_or(-1, |p| p.width()),
                paned.as_ref().map_or(-1, |p| p.height()),
                paned.as_ref().map_or(-1, |p| p.position()),
                paned.as_ref().map_or(-1, |p| p.min_position()),
                paned.as_ref().map_or(-1, |p| p.max_position()),
            ]
        }
    });
    struct PaintObserver {
        clock: gtk::gdk::FrameClock,
        handler: Option<gtk::glib::SignalHandlerId>,
    }
    impl Drop for PaintObserver {
        fn drop(&mut self) {
            if let Some(handler) = self.handler.take() {
                self.clock.disconnect(handler);
            }
        }
    }
    let clock = root.frame_clock().expect("mapped fixture clock");
    let previous = Rc::new(Cell::new(None));
    let stable = Rc::new(Cell::new(0u32));
    let handler = clock.connect_after_paint({
        let sample = sample.clone();
        let previous = previous.clone();
        let stable = stable.clone();
        move |_| {
            let current = sample();
            if previous.replace(Some(current)) == Some(current) {
                stable.set(stable.get().saturating_add(1));
            } else {
                stable.set(0);
            }
        }
    });
    let observer = PaintObserver {
        clock,
        handler: Some(handler),
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while stable.get() < 2 && Instant::now() < deadline {
        root.queue_draw();
        settle(Duration::from_millis(10));
    }
    drop(observer);
    let result = sample();
    assert_eq!(
        Some(result),
        previous.get(),
        "layout changed after its last actual paint"
    );
    assert!(
        stable.get() >= 2,
        "root/card/direct-slot/terminal layout did not settle: {result:?}"
    );
    result
}

#[cfg(test)]
fn assert_fixture_budget(compact: [i32; 14], expanded: [i32; 14]) {
    // All dimensions are actual allocations. Card margins are unchanged.
    // Account for parent resizing instead of assuming Paned stayed fixed.
    assert_eq!(
        compact[4] - expanded[4],
        0,
        "direct-slot widths: compact={compact:?}, expanded={expanded:?}"
    );
    assert_eq!(
        compact[5] - expanded[5],
        expanded[3] - compact[3] + compact[1] - expanded[1],
        "root/card/direct-slot/terminal budget: compact={compact:?}, expanded={expanded:?}"
    );
}

#[cfg(test)]
fn exercise_expanded_presentation(
    surface: &Rc<VteCompanion>,
    terminal: &vte4::Terminal,
    view: &crate::block_view::TermView,
    entry: &gtk::Entry,
    split: &gtk::Paned,
    config: &Rc<RefCell<Config>>,
    settle: &impl Fn(Duration),
) {
    let requisition = || {
        let horizontal = surface.card.measure(gtk::Orientation::Horizontal, -1);
        (
            horizontal,
            surface
                .card
                .measure(gtk::Orientation::Vertical, horizontal.1),
        )
    };
    assert_accessible_hidden(&surface.capability, false);
    let reversed = Rc::new(Cell::new(false));
    let handler = surface.glyph.connect_parent_notify({
        let weak = Rc::downgrade(surface);
        let reversed = reversed.clone();
        move |glyph| {
            if glyph.parent().is_none() && !reversed.replace(true) {
                if let Some(surface) = weak.upgrade() {
                    surface.set_expanded(false);
                }
            }
        }
    });
    surface.set_expanded(true);
    settle(Duration::from_millis(100));
    surface.glyph.disconnect(handler);
    assert!(reversed.get());
    assert!(!surface.expanded.get());
    assert_eq!(
        surface.glyph.parent().as_ref(),
        Some(surface.card.upcast_ref::<gtk::Widget>())
    );
    assert!(surface.expanded_slot.parent().is_none());
    assert!(!surface.mounting.get());
    let compact_req = requisition();
    let compact_layout = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    let compact_terminal = (compact_layout[6], compact_layout[7]);
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Static);
    surface.refresh();
    surface.set_expanded(true);
    settle(Duration::from_millis(100));
    assert!(surface.expanded.get());
    assert_eq!(surface.current_presentation(), Presentation::Expanded);
    assert_eq!(surface.glyph.parent(), None);
    let expanded_req = requisition();
    let expanded_layout = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    let expanded_terminal = (expanded_layout[6], expanded_layout[7]);
    let expanded_height = expanded_layout[3];
    let slot_h = surface
        .expanded_slot
        .measure(gtk::Orientation::Horizontal, -1);
    let slot_v = surface
        .expanded_slot
        .measure(gtk::Orientation::Vertical, slot_h.1);
    for candidate in &surface.candidates {
        let h = candidate.measure(gtk::Orientation::Horizontal, -1);
        let v = candidate.measure(gtk::Orientation::Vertical, slot_h.1);
        assert!(slot_h.0 >= h.0 && slot_h.1 >= h.1);
        assert!(slot_v.0 >= v.0 && slot_v.1 >= v.1);
    }
    for candidate in &surface.candidates[1..] {
        assert!(surface.expanded_slot.is_measure_overlay(candidate));
        assert!(surface.expanded_slot.is_clip_overlay(candidate));
    }
    assert!(expanded_req.1 .0 > compact_req.1 .0);
    assert_eq!(expanded_terminal.0, compact_terminal.0);
    // This preserved fixture carries a Block TermView: its internal VTE is
    // quantized to cell rows. The exact companion budget belongs to the
    // direct TermView slot; internal VTE stability is checked below.
    assert_fixture_budget(compact_layout, expanded_layout);
    surface.set_expanded(true);
    assert_eq!(requisition(), expanded_req);
    for candidate in &surface.candidates {
        assert!(candidate.is_visible());
        assert!(!candidate.can_target());
        assert!(!candidate.is_focusable());
        assert_eq!(
            candidate.accessible_role(),
            gtk::AccessibleRole::Presentation
        );
        assert!(gtk::test_accessible_has_role(
            candidate,
            gtk::AccessibleRole::Presentation
        ));
        assert!(candidate.first_accessible_child().is_none());
        assert_eq!(requisition(), expanded_req);
    }
    assert!(surface.capability.is_mapped());
    assert!(!matches!(
        surface.capability.accessible_role(),
        gtk::AccessibleRole::Presentation | gtk::AccessibleRole::None
    ));
    assert!(gtk::test_accessible_has_property(
        &surface.capability,
        gtk::AccessibleProperty::Description
    ));
    assert_accessible_hidden(&surface.capability, false);
    surface.set_expanded(false);
    settle(Duration::from_millis(100));
    assert_eq!(requisition(), compact_req);
    for candidate in &surface.candidates {
        assert!(candidate.is_visible());
        assert_eq!(
            candidate.accessible_role(),
            gtk::AccessibleRole::Presentation
        );
        assert!(gtk::test_accessible_has_role(
            candidate,
            gtk::AccessibleRole::Presentation
        ));
        assert!(candidate.first_accessible_child().is_none());
    }
    surface.set_expanded(true);
    settle(Duration::from_millis(100));
    assert_eq!(requisition(), expanded_req);
    for candidate in &surface.candidates {
        assert!(candidate.is_visible());
        assert_eq!(
            candidate.accessible_role(),
            gtk::AccessibleRole::Presentation
        );
        assert!(gtk::test_accessible_has_role(
            candidate,
            gtk::AccessibleRole::Presentation
        ));
        assert!(candidate.first_accessible_child().is_none());
    }
    assert!(surface.capability.is_mapped());
    assert_accessible_hidden(&surface.capability, false);
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Full);
    for frame in 0..NEUTRAL_FRAME_PERIOD {
        surface.frame.set(frame);
        surface.refresh();
        assert_eq!(
            surface
                .candidates
                .iter()
                .filter(|c| c.opacity() == 1.0)
                .count(),
            1
        );
        let text = sprite_frame_with_context(
            RenderContext::new(Behavior::Idle, BodyLanguage::default(), false),
            frame,
        );
        assert!(surface.candidates[..surface.full_count]
            .iter()
            .any(|c| c.opacity() == 1.0 && c.text().as_str() == text.as_ref()));
        assert_eq!(requisition(), expanded_req);
        assert_eq!((terminal.width(), terminal.height()), expanded_terminal);
    }
    terminal.emit_by_name::<()>("commit", &[&"e", &1u32]);
    assert_eq!(
        crate::block_view::organism_settings_test_pty_bytes(view),
        b"e"
    );
    assert!(surface.candidates.iter().all(|c| c.opacity() == 0.0));
    assert_eq!(requisition(), expanded_req);
    assert_eq!((terminal.width(), terminal.height()), expanded_terminal);
    surface.last_input.set(None);
    for motion in [OrganismMotion::Calm, OrganismMotion::Static] {
        config.borrow_mut().ascii_organism_motion = Some(motion);
        surface.refresh();
        assert_eq!(surface.candidates[0].opacity(), 1.0);
        assert!(surface.source.borrow().is_none());
        assert_eq!(requisition(), expanded_req);
    }
    let entry_has_native_focus = || {
        surface
            .current_host()
            .and_then(|window| gtk::prelude::RootExt::focus(&window))
            .is_some_and(|focus| {
                focus == *entry.upcast_ref::<gtk::Widget>() || focus.is_ancestor(entry)
            })
            && !terminal.has_focus()
    };
    println!(
        "VTE_EXPANDED_BLUR before {}",
        fixture_state(surface, terminal)
    );
    assert!(entry.grab_focus(), "{}", fixture_state(surface, terminal));
    println!(
        "VTE_EXPANDED_BLUR immediate entry_focus={} {}",
        entry_has_native_focus(),
        fixture_state(surface, terminal)
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while !entry_has_native_focus() && Instant::now() < deadline {
        settle(Duration::from_millis(10));
    }
    assert!(
        entry_has_native_focus(),
        "Entry did not acquire native focus: {}",
        fixture_state(surface, terminal)
    );
    let _ = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    println!(
        "VTE_EXPANDED_BLUR settled entry_focus={} {}",
        entry_has_native_focus(),
        fixture_state(surface, terminal)
    );
    assert!(
        entry_has_native_focus(),
        "Entry lost focus while layout settled: {}",
        fixture_state(surface, terminal)
    );
    assert!(
        surface.candidates.iter().all(|c| c.opacity() == 0.0),
        "{}",
        fixture_state(surface, terminal)
    );
    assert!(
        surface.source.borrow().is_none(),
        "Static blur must have no timer: {}",
        fixture_state(surface, terminal)
    );
    surface.set_expanded(false); // A late action cannot operate on the unfocused pane.
    assert!(surface.expanded.get());
    assert_eq!(requisition(), expanded_req);
    assert!(terminal.grab_focus());
    settle(Duration::from_millis(40));
    assert_eq!(surface.current_presentation(), Presentation::Expanded);
    let font = gtk::pango::FontDescription::from_string("monospace 24");
    let attrs = gtk::pango::AttrList::new();
    attrs.insert(gtk::pango::AttrFontDesc::new(&font));
    for candidate in &surface.candidates {
        candidate.set_attributes(Some(&attrs));
    }
    settle(Duration::from_millis(100));
    assert!(
        requisition().1 .0 > expanded_req.1 .0,
        "native font invalidation must remeasure the real paint labels"
    );
    for candidate in &surface.candidates {
        candidate.set_attributes(None);
    }
    settle(Duration::from_millis(100));
    assert_eq!(requisition(), expanded_req);
    // Use real ancestor-only Paned clipping, not a guessed terminal font size.
    // The centered one-line candidate must become drawable before all three
    // lines fit, while every step retains exactly the same requisition.
    let mut found_fallback = false;
    for position in 1..=expanded_height + 4 {
        split.set_position(position);
        settle(Duration::from_millis(20));
        assert_eq!(requisition(), expanded_req);
        if surface.current_presentation() == Presentation::Fallback {
            assert!(surface.candidates[..surface.full_count]
                .iter()
                .all(|c| c.opacity() == 0.0));
            assert_eq!(
                surface.candidates[surface.full_count..]
                    .iter()
                    .filter(|c| c.opacity() == 1.0)
                    .count(),
                1
            );
            // A settled fit must not oscillate or schedule more work merely
            // because the window already paints again. No frame is requested.
            let clock = surface.current_host().unwrap().frame_clock().unwrap();
            for _ in 0..3 {
                clock.emit_by_name::<()>("after-paint", &[]);
                assert_eq!(surface.current_presentation(), Presentation::Fallback);
                assert!(surface.geometry_idle.borrow().is_none());
                assert!(surface.source.borrow().is_none());
                assert_eq!(requisition(), expanded_req);
            }
            found_fallback = true;
            break;
        }
    }
    assert!(found_fallback, "{}", fixture_state(surface, terminal));
    split.set_position(1);
    settle(Duration::from_millis(100));
    assert_eq!(surface.current_presentation(), Presentation::Hidden);
    assert!(surface.source.borrow().is_none());
    assert_eq!(
        requisition(),
        expanded_req,
        "ancestor clipping must not change the envelope"
    );
    split.set_position(300);
    settle(Duration::from_millis(100));
    assert_eq!(surface.current_presentation(), Presentation::Expanded);
    assert_eq!((terminal.width(), terminal.height()), expanded_terminal);
    // Reuse this same Paned to exercise real horizontal narrowing as well as
    // the vertical clipping above. The complete seven-column body must yield
    // to a complete five-column glyph, then disappear, without losing height.
    let vertical_layout = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    let vertical_position = split.position();
    split.set_orientation(gtk::Orientation::Horizontal);
    // GtkPaned retains last_allocation across axis changes. Let the new axis
    // allocate before requesting a position, otherwise its resize policy can
    // scale that request using the old axis length.
    let horizontal_axis = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    let horizontal_requested = split.width() / 2;
    split.set_position(horizontal_requested);
    let horizontal_layout = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    println!("VTE_PANED_AXIS horizontal requested={horizontal_requested} before={vertical_layout:?} axis={horizontal_axis:?} settled={horizontal_layout:?}");
    let wide_position = split.position();
    assert!(wide_position > 1);
    assert_eq!(surface.current_presentation(), Presentation::Expanded);
    assert_eq!(requisition(), expanded_req);
    split.set_position(1);
    settle(Duration::from_millis(100));
    assert_eq!(surface.current_presentation(), Presentation::Hidden);
    assert!(surface.candidates.iter().all(|c| c.opacity() == 0.0));
    assert!(surface.source.borrow().is_none());
    assert_eq!(requisition(), expanded_req);
    let mut found_narrow_fallback = false;
    for position in 2..wide_position {
        split.set_position(position);
        settle(Duration::from_millis(20));
        assert_eq!(requisition(), expanded_req);
        if surface.current_presentation() == Presentation::Fallback {
            assert!(surface.candidates[..surface.full_count]
                .iter()
                .all(|c| c.opacity() == 0.0));
            assert_eq!(
                surface.candidates[surface.full_count..]
                    .iter()
                    .filter(|c| c.opacity() == 1.0)
                    .count(),
                1
            );
            assert!(surface.candidates[surface.full_count..]
                .iter()
                .all(|c| surface.candidate_fits(c)));
            let clock = surface.current_host().unwrap().frame_clock().unwrap();
            for _ in 0..3 {
                clock.emit_by_name::<()>("after-paint", &[]);
                assert_eq!(surface.current_presentation(), Presentation::Fallback);
                assert!(surface.geometry_idle.borrow().is_none());
                assert!(surface.source.borrow().is_none());
                assert_eq!(requisition(), expanded_req);
            }
            found_narrow_fallback = true;
            break;
        }
    }
    assert!(
        found_narrow_fallback,
        "{}",
        fixture_state(surface, terminal)
    );
    split.set_position(wide_position);
    settle(Duration::from_millis(100));
    assert_eq!(surface.current_presentation(), Presentation::Expanded);
    assert_eq!(requisition(), expanded_req);
    // Reuse this mapped Paned for compact partial clipping. A positive
    // intersection alone must not paint half a glyph or keep Full ticking.
    surface.set_expanded(false);
    settled_fixture_layout(surface, &view.widget(), terminal, settle);
    let mut saw_partial_compact = false;
    for position in 1..wide_position {
        split.set_position(position);
        settled_fixture_layout(surface, &view.widget(), terminal, settle);
        assert_eq!(requisition(), compact_req);
        if surface.glyph_has_visible_area() && !surface.candidate_fits(&surface.glyph) {
            saw_partial_compact = true;
            for motion in [
                OrganismMotion::Full,
                OrganismMotion::Calm,
                OrganismMotion::Static,
            ] {
                config.borrow_mut().ascii_organism_motion = Some(motion);
                surface.refresh();
                settle(Duration::from_millis(20));
                assert_eq!(surface.current_presentation(), Presentation::Hidden);
                assert_eq!(surface.glyph.opacity(), 0.0);
                assert!(surface.source.borrow().is_none());
                assert!(surface.card.is_mapped());
                assert!(!surface.expanded.get());
                assert_eq!(requisition(), compact_req);
            }
            let clock = surface.current_host().unwrap().frame_clock().unwrap();
            for _ in 0..3 {
                clock.emit_by_name::<()>("after-paint", &[]);
                assert!(surface.geometry_idle.borrow().is_none());
                assert_eq!(surface.glyph.opacity(), 0.0);
                assert_eq!(requisition(), compact_req);
            }
            break;
        }
    }
    assert!(saw_partial_compact, "{}", fixture_state(surface, terminal));
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Full);
    surface.refresh();
    assert!(surface.source.borrow().is_none());
    split.set_position(wide_position);
    settled_fixture_layout(surface, &view.widget(), terminal, settle);
    assert_eq!(surface.current_presentation(), Presentation::Compact);
    assert_eq!(surface.glyph.opacity(), 1.0);
    assert!(surface.source.borrow().is_some());
    assert_eq!(requisition(), compact_req);
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Static);
    surface.refresh();
    surface.set_expanded(true);
    settled_fixture_layout(surface, &view.widget(), terminal, settle);
    assert_eq!(requisition(), expanded_req);
    split.set_orientation(gtk::Orientation::Vertical);
    let vertical_axis = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    split.set_position(vertical_position);
    let vertical_restored = settled_fixture_layout(surface, &view.widget(), terminal, settle);
    println!("VTE_PANED_AXIS vertical requested={vertical_position} axis={vertical_axis:?} settled={vertical_restored:?} original={vertical_layout:?}");
    assert_eq!(
        split.position(),
        vertical_position,
        "restored Paned position: {vertical_restored:?}"
    );
    assert_eq!(surface.current_presentation(), Presentation::Expanded);
    assert_eq!((terminal.width(), terminal.height()), expanded_terminal,
        "restored vertical allocation: original={vertical_layout:?} axis={vertical_axis:?} restored={vertical_restored:?}");
    surface.set_expanded(false);
    settle(Duration::from_millis(100));
    assert!(!surface.expanded.get());
    assert_eq!(surface.glyph.width_chars(), 5);
    assert_eq!(surface.glyph.max_width_chars(), 5);
    assert_eq!(requisition(), compact_req);
    assert_eq!((terminal.width(), terminal.height()), compact_terminal);
    surface.set_expanded(false);
    assert_eq!(requisition(), compact_req);
    let current = Rc::new(Cell::new(Some(42)));
    crate::dialogs::command_palette::exercise_companion_presentation_palette(
        &surface.current_host().expect("existing host"),
        terminal.upcast_ref(),
        current.clone(),
        Rc::new({
            let weak = Rc::downgrade(surface);
            move |target, desired| {
                if crate::action_ops::hello_target_is_current(target, current.get()) {
                    if let Some(surface) = weak.upgrade() {
                        surface.set_expanded(desired);
                    }
                }
            }
        }),
        Rc::new({
            let weak = Rc::downgrade(surface);
            move || weak.upgrade().is_some_and(|s| s.expanded.get())
        }),
        settle,
    );
    assert!(
        crate::block_view::organism_settings_test_pty_bytes(view).is_empty(),
        "palette Enter/Escape never reaches the terminal writer"
    );
    assert_eq!(requisition(), compact_req);
    assert_eq!((terminal.width(), terminal.height()), compact_terminal);
    config.borrow_mut().ascii_organism_motion = Some(OrganismMotion::Full);
    surface.refresh();
}

#[cfg(test)]
mod tests {
    use super::super::HUMAN_INPUT_RETREAT;
    use super::*;

    #[test]
    #[ignore = "requires DISPLAY"]
    fn neutral_candidates_preserve_native_paint_and_fallback_geometry() {
        gtk::init().expect("gtk display");
        relm4::adw::init().expect("native companion fixture window");
        // Reuse the existing nonexecuting terminal and all VTE assertions
        // independently of the Block companion's unrelated state scenarios.
        let view = crate::block_view::organism_settings_test_view();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let entry = gtk::Entry::new();
        content.append(&entry);
        content.append(&view.widget());
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&content));
        let window = relm4::adw::Window::builder()
            .default_width(700)
            .default_height(500)
            .content(&overlay)
            .build()
            .upcast::<gtk::Window>();
        let main = gtk::glib::MainContext::default();
        let settle = |delay: Duration| {
            let started = Instant::now();
            while started.elapsed() < delay {
                while main.iteration(false) {}
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        window.present();
        settle(Duration::from_millis(120));
        assert!(view.widget().is_mapped());
        crate::block_view::organism_settings_test_clean_prompt(&view);
        crate::block_view::organism_settings_test_first_command(&view);
        exercise_existing_fixture(&window, &overlay, &content, &entry, &view, &settle);
        assert!(crate::block_view::organism_settings_test_pty_bytes(&view).is_empty());
        window.close();
        settle(Duration::from_millis(40));
    }

    #[test]
    fn neutral_presentation_candidates_cover_the_joint_480_frame_period() {
        let context = RenderContext::new(Behavior::Idle, BodyLanguage::default(), false);
        let (candidates, full_count) = neutral_candidates();
        assert_eq!(full_count, 7);
        assert_eq!(candidates.len() - full_count, 2);
        assert_eq!(candidates[0], Behavior::Idle.sprite());
        assert!(candidates[..full_count]
            .iter()
            .all(|text| text.lines().count() == 3));
        assert!(candidates[full_count..]
            .iter()
            .all(|text| text.lines().count() == 1));
        let expected: std::collections::BTreeSet<_> = candidates.iter().cloned().collect();
        assert_eq!(
            expected.len(),
            candidates.len(),
            "each frame paints exactly one native label"
        );
        let mut actual = std::collections::BTreeSet::new();
        for frame in 0..NEUTRAL_FRAME_PERIOD {
            let sprite = sprite_frame_with_context(context, frame);
            assert_eq!(sprite.lines().count(), 3);
            assert!(sprite.is_ascii() && sprite.lines().all(|line| line.len() <= 7));
            assert_eq!(
                sprite,
                sprite_frame_with_context(context, frame + NEUTRAL_FRAME_PERIOD)
            );
            actual.insert(sprite.into_owned());
            let compact = sticky_glyph_with_context(context, frame);
            assert_eq!(compact.len(), 5);
            assert_eq!(
                compact,
                sticky_glyph_with_context(context, frame + NEUTRAL_FRAME_PERIOD)
            );
            actual.insert(compact.into_owned());
        }
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 9);
        for frame in u64::MAX - NEUTRAL_FRAME_PERIOD + 1..=u64::MAX {
            assert!(expected.contains(sprite_frame_with_context(context, frame).as_ref()));
            assert!(expected.contains(sticky_glyph_with_context(context, frame).as_ref()));
        }
    }

    #[test]
    fn compact_requires_complete_fit_without_changing_requested_presentation() {
        let body = [0.0, 0.0, 50.0, 20.0];
        for clip in [
            [0.0, 0.0, 49.5, 20.0],
            [0.5, 0.0, 50.0, 20.0],
            [0.0, 0.0, 50.0, 19.5],
            [0.0, 0.5, 50.0, 20.0],
        ] {
            assert_eq!(
                presentation(false, false, contains_edges(clip, body)),
                Presentation::Hidden
            );
        }
        assert_eq!(
            presentation(false, false, contains_edges(body, body)),
            Presentation::Compact
        );
        assert_eq!(presentation(false, false, false), Presentation::Hidden);
    }

    #[test]
    fn expanded_fit_is_complete_and_never_changes_the_requested_preference() {
        assert_eq!(presentation(false, false, true), Presentation::Compact);
        assert_eq!(presentation(true, true, true), Presentation::Expanded);
        assert_eq!(presentation(true, false, true), Presentation::Fallback);
        assert_eq!(presentation(true, false, false), Presentation::Hidden);
        let full = [0.0, 0.0, 7.0, 3.0];
        assert!(contains_edges(full, full));
        assert!(!contains_edges([0.0, 0.0, 6.999, 3.0], full));
        assert!(!contains_edges([0.0, 0.0, 7.0, 2.999], full));
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            assert!(!contains_edges([0.0, 0.0, bad, 3.0], full));
        }
    }

    #[test]
    fn compact_native_path_checks_exact_pose_before_paint_and_keeps_recovery() {
        let source = include_str!("vte_surface.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production
            .contains("return presentation(false, false, self.candidate_fits(&self.glyph))"));
        let refresh = production.split("pub(crate) fn refresh").nth(1).unwrap();
        assert!(
            refresh.find("self.glyph.set_text").unwrap()
                < refresh
                    .find("let choice = self.current_presentation()")
                    .unwrap()
        );
        assert!(
            refresh.find("self.sync_paint_observer(focused)").unwrap()
                < refresh.find("let geometry =").unwrap()
        );
        assert!(refresh.contains("|| self.current_presentation() == Presentation::Hidden"));
        assert!(!refresh.contains("self.card.set_visible(painted)"));
        assert!(source.contains("assert!(saw_partial_compact,"));
    }

    #[test]
    fn native_envelope_measures_the_same_candidate_widgets_that_it_paints() {
        let source = include_str!("vte_surface.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("gtk::Grid"));
        assert!(production.contains("expanded_slot.set_child(Some(&candidates[0]))"));
        assert!(production.contains("expanded_slot.set_measure_overlay(candidate, true)"));
        assert!(production.contains("expanded_slot.set_clip_overlay(candidate, true)"));
        assert!(production.contains(".accessible_role(gtk::AccessibleRole::Presentation)"));
        assert!(!production.contains("accessible::State::Hidden"));
        assert!(!production.contains("set_height_request"));
        assert_eq!(
            production.matches("neutral_candidates()").count(),
            2,
            "candidate enumeration runs only in its definition and constructor"
        );
        let refresh = production.split("pub(crate) fn refresh").nth(1).unwrap();
        assert!(!refresh.contains("neutral_candidates("));
        assert!(
            refresh.find("if self.closed.get()").unwrap()
                < refresh.find("if self.mounting.get()").unwrap()
        );
        assert!(
            production.contains("if self.closed.get() || self.geometry_idle.borrow().is_some()")
        );
        let script = include_str!("../../scripts/test-gtk-display.sh");
        assert!(script.contains("export GTK_A11Y=none"));
        assert!(!script.contains("export GTK_A11Y=test"));
        assert!(script.contains("if [[ \"${test_name}\" == organism_ui::tests::live_settings_reuse_body_preserve_focus_and_stop_disabled_timers ]]; then\n        fixture_a11y=\"test\""));
        assert!(script.contains("GTK_A11Y=\"${fixture_a11y}\" run_case"));
        let paint = production.split("let painted =").nth(1).unwrap();
        assert!(!paint.contains("candidate.set_text"));
        assert!(!paint.contains("candidate.set_visible"));
    }

    #[test]
    fn palette_presentation_preserves_original_target_and_has_no_binding() {
        let palette = include_str!("../dialogs/command_palette.rs");
        let accept = palette
            .split("    fn accept(")
            .nth(1)
            .unwrap()
            .split("fn spawn_history_snapshot_load")
            .next()
            .unwrap();
        assert!(
            accept
                .find("let workflow_target = self.workflow_target")
                .unwrap()
                < accept.find("root.force_close()").unwrap()
        );
        assert!(accept.contains("PaletteOutput::SetVteCompanionExpanded(workflow_target, true)"));
        assert!(accept.contains("PaletteOutput::SetVteCompanionExpanded(workflow_target, false)"));
        let keys = include_str!("../keybindings.rs");
        assert!(keys.contains("Action::ExpandVteCompanion | Action::CollapseVteCompanion => None"));
        let actions = include_str!("../action_ops.rs");
        let target = actions
            .split("pub(crate) fn set_vte_companion_expanded")
            .nth(1)
            .unwrap()
            .split("pub(crate) fn greet_ascii_organism")
            .next()
            .unwrap();
        for gate in [
            "self.safe_mode",
            "ascii_organism_enabled",
            "hello_target_is_current",
            "self.window.is_active()",
            "self.find_pane(id)",
            "pane.cwd_external",
            "tab.remote",
            "TerminalMode::Vte",
        ] {
            assert!(
                target
                    .split_whitespace()
                    .collect::<String>()
                    .contains(&gate.split_whitespace().collect::<String>()),
                "{gate}"
            );
        }
        assert!(!target.contains("grab_focus"));
        assert!(!target.contains("write_input"));
        assert!(!target.contains("say_hello"));
    }

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
        let enter = production
            .split("focus.connect_enter({")
            .nth(1)
            .unwrap()
            .split("focus.connect_leave({")
            .next()
            .unwrap();
        assert!(enter.contains("surface.queue_geometry_refresh();"));
        assert!(!enter.contains("surface.refresh();"));
        let leave = production
            .split("focus.connect_leave({")
            .nth(1)
            .unwrap()
            .split("root.add_controller(focus)")
            .next()
            .unwrap();
        assert!(leave.contains("surface.refresh();"));
        let host = production
            .split("fn bind_host(")
            .nth(1)
            .unwrap()
            .split("fn focused_and_mapped(")
            .next()
            .unwrap();
        assert!(host.contains("connect_focus_widget_notify"));
        assert_eq!(
            host.matches("surface.current_host().as_ref() == Some(window)")
                .count(),
            2
        );
        assert!(host.contains("[active, focus]"));
        assert!(host.contains("for handler in handler"));
        let drop = production
            .split("impl Drop for VteCompanion")
            .nth(1)
            .unwrap();
        assert!(drop.contains("self.host.get_mut().take()"));
        assert!(drop.contains("for handler in handler"));
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
