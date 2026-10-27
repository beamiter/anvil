//! Frame-clock motion for the retained Block/Chat transcript canvas.
use gtk::glib;
use gtk::prelude::*;
use gtk::ScrolledWindow;
use jterm_core::transcript_canvas::CanvasMotion;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const SCROLL_EPSILON_PX: f64 = 0.5;
const BOTTOM_STABLE_FRAMES: u8 = 4;
/// Wheel distance is independent of window height. Surface deltas are already
/// logical pixels and must never be converted into page-sized wheel notches.
const WHEEL_DISTANCE_PX: f64 = 54.0;
const COAST_FRICTION: f64 = 8.0;

fn scroll_value_changed(current: f64, target: f64) -> bool {
    (current - target).abs() > SCROLL_EPSILON_PX
}

/// Animation settling tolerates half a pixel; reader input must not. Reusing
/// that tolerance here lets a small precision gesture be reclaimed by output,
/// or lets an active wheel animation overwrite a fractional scrollbar move.
fn reader_position_changed(current: f64, target: f64) -> bool {
    (current - target).abs() > 1.0e-7
}

fn bottom_value(adjustment: &gtk::Adjustment) -> f64 {
    (adjustment.upper() - adjustment.page_size()).max(adjustment.lower())
}

pub(crate) fn scrolled_away_from_bottom(adjustment: &gtk::Adjustment) -> bool {
    reader_position_changed(adjustment.value(), bottom_value(adjustment))
}

pub(crate) fn next_scroll_lock(prev: bool, holder_visible: bool, value: f64, bottom: f64) -> bool {
    !holder_visible || (prev && reader_position_changed(value, bottom))
}

fn next_stable_frame_count(last_target: Option<f64>, target: f64, current: u8) -> u8 {
    match last_target {
        Some(last) if !scroll_value_changed(last, target) => current.saturating_add(1),
        _ => 0,
    }
}

fn request_bottom_pin(user_scrolled: bool, active: &Cell<bool>, generation: &Cell<u64>) -> bool {
    if user_scrolled {
        return false;
    }
    generation.set(generation.get().wrapping_add(1));
    !active.replace(true)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CanvasDrive {
    #[default]
    Idle,
    Follow,
    Wheel,
    Coast,
}

/// One controller belongs to one pane's persistent scroller. PTY output only
/// retargets this controller; it never writes an abrupt adjustment value.
/// Native pointer/scrollbar moves interrupt it, precision input is direct, and
/// frame time (rather than a 16 ms timer) drives wheel/follow presentation.
#[derive(Clone)]
pub(crate) struct ScrollDebouncer {
    pub(crate) user_scrolled_up: Rc<Cell<bool>>,
    pub(crate) programmatic_scroll: Rc<Cell<bool>>,
    bottom_pin_active: Rc<Cell<bool>>,
    bottom_pin_generation: Rc<Cell<u64>>,
    motion: Rc<RefCell<CanvasMotion>>,
    drive: Rc<Cell<CanvasDrive>>,
    tick: Rc<RefCell<Option<gtk::TickCallbackId>>>,
    coast_velocity: Rc<Cell<f64>>,
    precision_forwarded: Rc<Cell<bool>>,
    immediate_follow: Rc<Cell<bool>>,
}

impl ScrollDebouncer {
    pub(crate) fn with_scroll_lock(
        user_scrolled_up: Rc<Cell<bool>>,
        programmatic_scroll: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            user_scrolled_up,
            programmatic_scroll,
            bottom_pin_active: Rc::new(Cell::new(false)),
            bottom_pin_generation: Rc::new(Cell::new(0)),
            motion: Rc::new(RefCell::new(CanvasMotion::default())),
            drive: Rc::new(Cell::new(CanvasDrive::Idle)),
            tick: Rc::new(RefCell::new(None)),
            coast_velocity: Rc::new(Cell::new(0.0)),
            precision_forwarded: Rc::new(Cell::new(false)),
            immediate_follow: Rc::new(Cell::new(false)),
        }
    }

    pub(crate) fn install(&self, scroll: &ScrolledWindow, content: &gtk::Box) {
        scroll.set_kinetic_scrolling(true);
        scroll.set_overlay_scrolling(true);
        // A hidden tab must not retain a clock callback, a strong widget, or a
        // fling that resumes unexpectedly when the tab becomes visible again.
        let this = self.clone();
        scroll.connect_unmap(move |_| this.stop_motion());
        let this = self.clone();
        scroll.connect_map(move |scroll| {
            if !this.user_scrolled_up.get() {
                // A tab returning after background output starts at its current
                // live edge; do not animate through unseen restored history.
                this.immediate_follow.set(true);
                this.pin_to_bottom_deferred(scroll);
            }
        });
        // Descendant VTE controllers claim their own scrollback first. At the
        // content's bubble phase the remaining header/gap events share the same
        // outer canvas as events forwarded at a terminal's boundary.
        let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        self.watch_gesture(&controller, scroll);
        let outer = scroll.downgrade();
        let this = self.clone();
        controller.connect_scroll(move |controller, _, dy| {
            let Some(outer) = outer.upgrade() else {
                return glib::Propagation::Proceed;
            };
            this.scroll_wheel(
                &outer,
                dy,
                controller.unit() == gtk::gdk::ScrollUnit::Surface,
            );
            glib::Propagation::Stop
        });
        content.add_controller(controller);
    }

    /// Register before the consumer's scroll callback. Only the last event
    /// actually handed to the outer canvas may coast; an inner VTE consuming
    /// the final event clears that ownership, avoiding a second scroll surface.
    pub(crate) fn watch_gesture(
        &self,
        controller: &gtk::EventControllerScroll,
        scroll: &ScrolledWindow,
    ) {
        controller.set_flags(controller.flags() | gtk::EventControllerScrollFlags::KINETIC);
        let this = self.clone();
        controller.connect_scroll_begin(move |_| this.stop_motion());
        let forwarded = self.precision_forwarded.clone();
        controller.connect_scroll(move |_, _, _| {
            forwarded.set(false);
            glib::Propagation::Proceed
        });
        let this = self.clone();
        let outer = scroll.downgrade();
        controller.connect_decelerate(move |_, _, velocity| {
            if !this.precision_forwarded.replace(false)
                || !velocity.is_finite()
                || velocity.abs() < 0.02
            {
                return;
            }
            let Some(outer) = outer.upgrade().filter(|outer| outer.is_mapped()) else {
                return;
            };
            if !animations_enabled(&outer) {
                return;
            }
            this.drive.set(CanvasDrive::Coast);
            // GTK's decelerate velocity is in pixels/ms. Integrate its bounded
            // exponential tail on the display clock; never add another spring
            // to precision deltas while the user's fingers are moving.
            this.coast_velocity
                .set((velocity * 1000.0).clamp(-12_000.0, 12_000.0));
            this.start_clock(&outer);
        });
    }

    pub(crate) fn mark_dirty(&self, scroll: &ScrolledWindow) {
        self.pin_to_bottom_deferred(scroll);
    }

    pub(crate) fn pin_to_bottom_deferred(&self, scroll: &ScrolledWindow) {
        if self.user_scrolled_up.get() || !scroll.is_mapped() {
            return;
        }
        let starting =
            request_bottom_pin(false, &self.bottom_pin_active, &self.bottom_pin_generation);
        let adjustment = scroll.vadjustment();
        {
            let mut motion = self.motion.borrow_mut();
            motion.set_bounds(adjustment.lower(), bottom_value(&adjustment));
            if starting || self.drive.get() != CanvasDrive::Follow {
                motion.jump_to(adjustment.value());
            }
            motion.set_reduced_motion(!animations_enabled(scroll));
            motion.set_target(bottom_value(&adjustment));
        }
        self.drive.set(CanvasDrive::Follow);
        if !animations_enabled(scroll) {
            let position = self.motion.borrow().position();
            self.programmatic_scroll.set(true);
            adjustment.set_value(position);
            self.programmatic_scroll.set(false);
        }
        self.start_clock(scroll);
    }

    pub(crate) fn scroll_wheel(&self, scroll: &ScrolledWindow, delta: f64, surface: bool) {
        if !delta.is_finite() || delta == 0.0 {
            return;
        }
        let adjustment = scroll.vadjustment();
        let (position, target) = {
            let mut motion = self.motion.borrow_mut();
            motion.set_bounds(adjustment.lower(), bottom_value(&adjustment));
            motion.set_reduced_motion(!animations_enabled(scroll));
            if self.drive.get() != CanvasDrive::Wheel
                || reader_position_changed(motion.position(), adjustment.value())
            {
                motion.jump_to(adjustment.value());
            }
            if surface {
                motion.jump_to(adjustment.value() + delta);
            } else {
                motion.scroll_by(delta * WHEEL_DISTANCE_PX);
            }
            (motion.position(), motion.target())
        };
        self.drive.set(CanvasDrive::Wheel);
        self.coast_velocity.set(0.0);
        self.precision_forwarded.set(surface);
        // Keep ownership until the presented position reaches the live edge,
        // not merely until a wheel destination points there. Otherwise an
        // output batch can replace an in-flight wheel target with a newer edge.
        let bottom = bottom_value(&adjustment);
        self.user_scrolled_up.set(
            reader_position_changed(position, bottom) || reader_position_changed(target, bottom),
        );
        if surface || !animations_enabled(scroll) {
            adjustment.set_value(position);
            self.record_wheel_intent(scroll);
        } else {
            self.start_clock(scroll);
        }
    }

    fn start_clock(&self, scroll: &ScrolledWindow) {
        if self.tick.borrow().is_some() || !scroll.is_mapped() {
            return;
        }
        self.bottom_pin_active.set(true);
        let this = self.clone();
        let last_time = Cell::new(None::<i64>);
        let last_target = Cell::new(None::<f64>);
        let stable_frames = Cell::new(0u8);
        let observed_generation = Cell::new(this.bottom_pin_generation.get());
        let settling_elapsed = Cell::new(0.0f64);
        let id = scroll.add_tick_callback(move |scroll, clock| {
            let finish = || {
                this.tick.borrow_mut().take();
                this.bottom_pin_active.set(false);
                this.drive.set(CanvasDrive::Idle);
                this.immediate_follow.set(false);
                this.motion.borrow_mut().cancel();
                glib::ControlFlow::Break
            };
            let drive = this.drive.get();
            if !scroll.is_mapped()
                || drive == CanvasDrive::Idle
                || (drive == CanvasDrive::Follow && this.user_scrolled_up.get())
            {
                return finish();
            }
            let adjustment = scroll.vadjustment();
            let now = clock.frame_time();
            let dt = last_time
                .replace(Some(now))
                .map(|last| ((now - last) as f64 / 1_000_000.0).clamp(0.0, 0.05))
                .unwrap_or(0.0);
            if observed_generation.replace(this.bottom_pin_generation.get())
                != this.bottom_pin_generation.get()
            {
                settling_elapsed.set(0.0);
                stable_frames.set(0);
            }
            settling_elapsed.set(settling_elapsed.get() + dt);
            let (position, animating, target) = {
                let mut motion = this.motion.borrow_mut();
                motion.set_bounds(adjustment.lower(), bottom_value(&adjustment));
                // Scrollbar drag, keyboard navigation, selection autoscroll and
                // native searches own their actual adjustment immediately.
                if reader_position_changed(motion.position(), adjustment.value()) {
                    drop(motion);
                    return finish();
                }
                motion.set_reduced_motion(!animations_enabled(scroll));
                if drive == CanvasDrive::Follow {
                    motion.set_target(bottom_value(&adjustment));
                    if adjustment.page_size() > 0.0 && this.immediate_follow.replace(false) {
                        motion.jump_to(bottom_value(&adjustment));
                    }
                }
                let position = if drive == CanvasDrive::Coast {
                    let velocity = this.coast_velocity.get();
                    let decay = (-COAST_FRICTION * dt).exp();
                    this.coast_velocity.set(velocity * decay);
                    let distance = velocity * (1.0 - decay) / COAST_FRICTION;
                    let old = motion.position();
                    motion.jump_to(old + distance);
                    motion.position()
                } else {
                    motion.advance(dt)
                };
                (position, motion.is_animating(), motion.target())
            };
            this.programmatic_scroll.set(drive == CanvasDrive::Follow);
            adjustment.set_value(position);
            this.programmatic_scroll.set(false);
            let stable = next_stable_frame_count(last_target.get(), target, stable_frames.get());
            last_target.set(Some(target));
            stable_frames.set(stable);
            let done = match drive {
                CanvasDrive::Follow => {
                    (!animating && stable >= BOTTOM_STABLE_FRAMES) || settling_elapsed.get() > 1.5
                }
                CanvasDrive::Coast => {
                    this.coast_velocity.get().abs() < 8.0
                        || (position <= adjustment.lower() && this.coast_velocity.get() < 0.0)
                        || (position >= bottom_value(&adjustment)
                            && this.coast_velocity.get() > 0.0)
                        || !animations_enabled(scroll)
                }
                CanvasDrive::Wheel => !animating,
                CanvasDrive::Idle => true,
            };
            if done {
                finish()
            } else {
                glib::ControlFlow::Continue
            }
        });
        *self.tick.borrow_mut() = Some(id);
    }

    fn stop_motion(&self) {
        // Take outside the RefCell borrow: removing a GTK callback can release
        // closures synchronously, including another clone of this controller.
        let tick = self.tick.borrow_mut().take();
        if let Some(tick) = tick {
            tick.remove();
        }
        self.bottom_pin_active.set(false);
        self.drive.set(CanvasDrive::Idle);
        self.immediate_follow.set(false);
        self.coast_velocity.set(0.0);
        self.precision_forwarded.set(false);
        self.motion.borrow_mut().cancel();
    }

    pub(crate) fn record_wheel_intent(&self, scroll: &ScrolledWindow) {
        let adjustment = scroll.vadjustment();
        self.user_scrolled_up.set(reader_position_changed(
            adjustment.value(),
            bottom_value(&adjustment),
        ));
    }

    pub(crate) fn reset_scroll_lock(&self) {
        self.user_scrolled_up.set(false);
    }
}

/// Preserve the document coordinate of surviving content when bounded history
/// retires an old prefix. Measure actual GTK origins before detaching anything;
/// widget estimates may differ after wrapping, font changes or virtualization.
pub(crate) struct PrefixRebase {
    adjustment: gtk::Adjustment,
    removed_height: f64,
    motion: CanvasMotion,
    previous_programmatic: bool,
    controller: ScrollDebouncer,
}

impl PrefixRebase {
    pub(crate) fn capture(
        first: &gtk::Widget,
        last: &gtk::Widget,
        controller: &ScrollDebouncer,
    ) -> Option<Self> {
        let scroll = first
            .ancestor(ScrolledWindow::static_type())?
            .downcast::<ScrolledWindow>()
            .ok()?;
        if !scroll.is_mapped() {
            return None;
        }
        let mut next = last.next_sibling();
        while next.as_ref().is_some_and(|widget| !widget.is_visible()) {
            next = next?.next_sibling();
        }
        let after = next?;
        let removed_height =
            f64::from(after.compute_bounds(&scroll)?.y() - first.compute_bounds(&scroll)?.y());
        if !removed_height.is_finite() || removed_height <= 0.0 {
            return None;
        }
        // Capture before GTK can clamp the adjustment while the prefix is
        // detached. Rebasing an already-clamped coordinate subtracts the
        // removed height twice near the old live edge.
        let adjustment = scroll.vadjustment();
        let mut motion = controller.motion.borrow().clone();
        motion.set_bounds(adjustment.lower(), bottom_value(&adjustment));
        if reader_position_changed(motion.position(), adjustment.value()) {
            motion.jump_to(adjustment.value());
        }
        Some(Self {
            adjustment,
            removed_height,
            motion,
            // Layout-triggered adjustment clamps are not reader input. Keep
            // their synchronous/idle observers inside the same transaction.
            previous_programmatic: controller.programmatic_scroll.replace(true),
            controller: controller.clone(),
        })
    }

    pub(crate) fn apply(mut self) {
        self.motion.rebase(
            self.adjustment.lower(),
            bottom_value(&self.adjustment),
            -self.removed_height,
        );
        let position = self.motion.position();
        *self.controller.motion.borrow_mut() = self.motion.clone();
        self.adjustment.set_value(position);
    }
}

impl Drop for PrefixRebase {
    fn drop(&mut self) {
        self.controller
            .programmatic_scroll
            .set(self.previous_programmatic);
    }
}

fn animations_enabled(scroll: &ScrolledWindow) -> bool {
    scroll.settings().is_gtk_enable_animations()
}

#[cfg(test)]
mod canvas_native_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn spin_for(milliseconds: u64) {
        let deadline = Instant::now() + Duration::from_millis(milliseconds);
        let context = glib::MainContext::default();
        while Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[ignore = "requires a GTK display and frame clock; run with test-gtk-display.sh"]
    fn capped_output_hands_remaining_pixels_to_retained_canvas() {
        gtk::init().expect("GTK display");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let retained = gtk::Label::new(Some("Retained history"));
        retained.set_height_request(2000);
        content.append(&retained);
        let scroll = gtk::ScrolledWindow::builder().child(&content).build();
        let window = gtk::Window::builder()
            .default_width(640)
            .default_height(400)
            .child(&scroll)
            .build();
        let controller =
            ScrollDebouncer::with_scroll_lock(Rc::new(Cell::new(true)), Rc::new(Cell::new(false)));
        controller.install(&scroll, &content);
        window.present();
        spin_for(150);
        let outer = scroll.vadjustment();
        assert!(
            bottom_value(&outer) > 1000.0,
            "fixture must have a real scroll range"
        );
        outer.set_value(200.0);
        let inner = gtk::Adjustment::new(99.5, 0.0, 120.0, 1.0, 10.0, 20.0);
        let remaining = super::super::blocks::scroll_adjustment_by_input(&inner, 50.0, true, 20.0);
        assert_eq!(inner.value(), 100.0);
        assert_eq!(remaining, 40.0);
        controller.scroll_wheel(&scroll, remaining, true);
        assert_eq!(
            outer.value(),
            240.0,
            "the same event must cross the output boundary"
        );
        assert!(controller.user_scrolled_up.get());
        assert!(
            retained.is_mapped(),
            "handoff must retain the existing history widget"
        );
        assert!(
            controller.tick.borrow().is_none(),
            "precision deltas must not start a second easing loop"
        );

        inner.set_value(0.25);
        let remaining = super::super::blocks::scroll_adjustment_by_input(&inner, -12.5, true, 20.0);
        assert_eq!(inner.value(), 0.0);
        assert_eq!(remaining, -7.5);
        controller.scroll_wheel(&scroll, remaining, true);
        assert_eq!(
            outer.value(),
            232.5,
            "reverse handoff must retain subpixel position"
        );
        inner.set_value(50.0);
        assert_eq!(
            super::super::blocks::scroll_adjustment_by_input(&inner, 4.0, true, 20.0),
            0.0
        );
        assert_eq!(
            outer.value(),
            232.5,
            "inner consumption must not move the outer canvas too"
        );
        window.close();
        spin_for(30);
        assert!(controller.tick.borrow().is_none());
    }

    #[test]
    #[ignore = "requires a GTK display and frame clock; run with test-gtk-display.sh"]
    fn retained_canvas_scrolls_through_frames_and_yields_to_reading() {
        gtk::init().expect("GTK display");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let retained = gtk::Label::new(Some("Retained command and output"));
        retained.set_height_request(1200);
        let stream = gtk::Label::new(Some("stream grows in place"));
        stream.set_height_request(160);
        let retired_prefix = gtk::Label::new(Some("Old bounded history"));
        retired_prefix.set_height_request(500);
        content.append(&retired_prefix);
        content.append(&retained);
        content.append(&stream);
        let scroll = gtk::ScrolledWindow::builder().child(&content).build();
        let window = gtk::Window::builder()
            .default_width(640)
            .default_height(400)
            .child(&scroll)
            .build();
        let controller =
            ScrollDebouncer::with_scroll_lock(Rc::new(Cell::new(false)), Rc::new(Cell::new(false)));
        controller.install(&scroll, &content);
        let settings = scroll.settings();
        let animations = settings.is_gtk_enable_animations();
        settings.set_gtk_enable_animations(true);
        let samples = Rc::new(RefCell::new(Vec::<f64>::new()));
        let values = samples.clone();
        let user = controller.user_scrolled_up.clone();
        let programmatic = controller.programmatic_scroll.clone();
        scroll
            .vadjustment()
            .connect_value_changed(move |adjustment| {
                values.borrow_mut().push(adjustment.value());
                if !programmatic.get() {
                    user.set(reader_position_changed(
                        adjustment.value(),
                        bottom_value(adjustment),
                    ));
                }
            });
        window.present();
        spin_for(650);
        let adjustment = scroll.vadjustment();
        assert!((adjustment.value() - bottom_value(&adjustment)).abs() < 0.5);
        assert!(
            controller.tick.borrow().is_none(),
            "settled canvas must stop waking frames"
        );
        let before = adjustment.value();
        samples.borrow_mut().clear();
        stream.set_height_request(460);
        controller.mark_dirty(&scroll);
        assert_eq!(
            adjustment.value(),
            before,
            "output must not synchronously jump"
        );
        spin_for(650);
        let target = bottom_value(&adjustment);
        assert!(target > before + 250.0);
        let values = samples.borrow();
        assert!(
            values
                .iter()
                .filter(|&&value| value > before + 0.5 && value < target - 0.5)
                .count()
                >= 4,
            "growth must be presented over real intermediate frames: {values:?}"
        );
        assert!(
            values.windows(2).all(|pair| pair[1] + 0.5 >= pair[0]),
            "follow must be monotonic"
        );
        drop(values);
        assert!((adjustment.value() - target).abs() < 0.5);
        assert_eq!(
            retired_prefix.next_sibling().as_ref(),
            Some(retained.upcast_ref()),
            "updates retain the same history widget"
        );

        // A tiny touchpad movement is exact, independent of page height, and
        // immediately revokes follow even while output continues below it.
        controller.scroll_wheel(&scroll, -0.375, true);
        assert!((adjustment.value() - (target - 0.375)).abs() < 0.0001);
        assert!(
            controller.user_scrolled_up.get(),
            "fractional input owns the viewport"
        );
        assert!(
            next_scroll_lock(true, true, adjustment.value(), target),
            "the deferred geometry probe must not discard fractional reading intent"
        );
        let reading = adjustment.value();
        stream.set_height_request(700);
        controller.mark_dirty(&scroll);
        spin_for(250);
        assert!(
            (adjustment.value() - reading).abs() < 0.0001,
            "stream must not pull a reader down"
        );
        assert!(controller.user_scrolled_up.get());

        // Discrete input moves over multiple frames and an external scrollbar
        // move interrupts that motion instead of being overwritten next frame.
        controller.scroll_wheel(&scroll, -2.0, false);
        assert_eq!(adjustment.value(), reading);
        spin_for(35);
        assert!(adjustment.value() < reading);
        let manual = adjustment.value() - 0.375;
        adjustment.set_value(manual);
        spin_for(150);
        assert!((adjustment.value() - manual).abs() < 0.0001);
        assert!(controller.tick.borrow().is_none());

        // A wheel destination at the current edge is not arrival. New output
        // while the wheel is still travelling must not seize its destination.
        let old_edge = bottom_value(&adjustment);
        adjustment.set_value(old_edge - 20.0);
        controller.scroll_wheel(&scroll, 2.0, false);
        assert_eq!(controller.motion.borrow().target(), old_edge);
        assert!(controller.user_scrolled_up.get());
        stream.set_height_request(900);
        controller.mark_dirty(&scroll);
        assert_eq!(controller.drive.get(), CanvasDrive::Wheel);
        spin_for(650);
        assert!((adjustment.value() - old_edge).abs() < 0.5);
        assert!(bottom_value(&adjustment) > old_edge + 100.0);
        assert!(controller.user_scrolled_up.get());

        // Read near the old end: a synchronous layout clamp on prefix
        // retirement must not subtract that height a second time.
        adjustment.set_value((bottom_value(&adjustment) - 20.0).max(0.0));
        spin_for(20);
        let retained_y = retained
            .compute_bounds(&scroll)
            .expect("retained card bounds")
            .y();
        let rebase = PrefixRebase::capture(
            retired_prefix.upcast_ref(),
            retired_prefix.upcast_ref(),
            &controller,
        )
        .expect("measured prefix");
        let shortened_upper = adjustment.upper() - rebase.removed_height;
        content.remove(&retired_prefix);
        // Force the GTK layout ordering that exposed the bug: the adjustment
        // publishes its shorter range before our preservation transaction.
        assert!(controller.user_scrolled_up.get());
        assert!(controller.programmatic_scroll.get());
        adjustment.set_upper(shortened_upper);
        assert!(
            controller.user_scrolled_up.get(),
            "layout clamp is not user input"
        );
        rebase.apply();
        assert!(!controller.programmatic_scroll.get());
        spin_for(80);
        assert!(
            (retained.compute_bounds(&scroll).unwrap().y() - retained_y).abs() < 0.5,
            "retiring old history keeps surviving content under the same screen pixel"
        );
        assert!(controller.user_scrolled_up.get());

        controller.scroll_wheel(&scroll, -2.0, false);
        assert!(controller.tick.borrow().is_some());
        window.set_visible(false);
        assert!(
            controller.tick.borrow().is_none(),
            "unmapping retires the display callback synchronously"
        );
        let hidden = adjustment.value();
        spin_for(80);
        window.present();
        spin_for(100);
        assert!(
            (adjustment.value() - hidden).abs() < 0.0001,
            "remap preserves the reader's position"
        );

        settings.set_gtk_enable_animations(false);
        controller.reset_scroll_lock();
        controller.pin_to_bottom_deferred(&scroll);
        spin_for(100);
        assert!(
            (adjustment.value() - bottom_value(&adjustment)).abs() < 0.5,
            "reduced motion still reaches live output"
        );
        assert!(controller.tick.borrow().is_none());
        settings.set_gtk_enable_animations(animations);
        window.close();
        spin_for(20);
    }
}

// ─── Virtual Scrolling ────────────────────────────────────────────────────────

pub(crate) struct ViewportState {
    pub(crate) first_visible: usize,
    pub(crate) last_visible: usize,
}

impl ViewportState {
    /// A valid viewport with no finished-card intersection (for example while
    /// reading a tall live output). This is distinct from unusable tab geometry,
    /// where callers retain the previous range instead of applying a new one.
    pub(crate) fn empty() -> Self {
        Self {
            first_visible: 1,
            last_visible: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.first_visible > self.last_visible
    }
}

impl Clone for ViewportState {
    fn clone(&self) -> Self {
        Self {
            first_visible: self.first_visible,
            last_visible: self.last_visible,
        }
    }
}

pub(crate) struct WidgetPool {
    pub(crate) available: Vec<gtk::Box>,
    pub(crate) max_pool_size: usize,
}

impl WidgetPool {
    pub(crate) fn new() -> Self {
        Self {
            available: Vec::new(),
            max_pool_size: 20,
        }
    }

    pub(crate) fn acquire(&mut self) -> Option<gtk::Box> {
        self.available.pop()
    }

    pub(crate) fn teardown(widget: &gtk::Box) {
        // A degraded block explains itself in a tooltip on this shell. Nothing
        // clears one on the way back out, and the card that reuses the shell
        // only ever sets a tooltip when it is degraded itself.
        widget.set_has_tooltip(false);
        // Pool only the lightweight outer shell. A finished card's child tree
        // owns both VTEs and their scrollback; retaining it here would put up to
        // twenty evicted cards outside the completed-block byte ledger.
        while let Some(child) = widget.first_child() {
            widget.remove(&child);
        }
        // Outer controllers can themselves retain the old FinishedBlock and
        // its VTEs through action closures. Tear them down even when the pool is
        // already full and this shell will be dropped instead of recycled.
        let controllers = widget.observe_controllers();
        while let Some(controller) = controllers.item(0) {
            if let Ok(controller) = controller.downcast::<gtk::EventController>() {
                widget.remove_controller(&controller);
            } else {
                break;
            }
        }
    }

    pub(crate) fn release(&mut self, widget: gtk::Box) {
        Self::teardown(&widget);
        if self.available.len() < self.max_pool_size {
            self.available.push(widget);
        }
    }
}

// ─── TermView ─────────────────────────────────────────────────────────────────

/// Shared lists of observer callbacks, keyed by the payload they receive.
pub(crate) type StrCallbacks = Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>;
pub(crate) type IntCallbacks = Rc<RefCell<Vec<Box<dyn Fn(i32)>>>>;
pub(crate) type VoidCallbacks = Rc<RefCell<Vec<Box<dyn Fn()>>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_reader_input_is_not_animation_settling_noise() {
        for bottom in [0.0, 100.0, 5000.0, 1_000_000.0] {
            for delta in [0.001, 0.125, 0.375, 0.5] {
                assert!(reader_position_changed(bottom - delta, bottom));
                assert!(next_scroll_lock(true, true, bottom - delta, bottom));
                assert!(!next_scroll_lock(false, true, bottom - delta, bottom));
            }
            assert!(!reader_position_changed(bottom, bottom));
            assert!(!next_scroll_lock(true, true, bottom, bottom));
        }
        // Keep the existing animation/layout coalescing tolerance independent.
        assert!(!scroll_value_changed(100.0, 100.375));
    }

    #[test]
    fn ignores_subpixel_scroll_churn() {
        assert!(!scroll_value_changed(100.0, 100.4));
        assert!(scroll_value_changed(100.0, 100.6));
    }

    #[test]
    fn a_user_scroll_shorter_than_the_live_card_keeps_the_lock() {
        let bottom = 5000.0;
        // A drag or find jump 200 px up, with the live card still on screen:
        // the lock the move set survives the probe.
        assert!(next_scroll_lock(true, true, bottom - 200.0, bottom));
        // The probe never creates a lock from the value alone.
        assert!(!next_scroll_lock(false, true, bottom - 200.0, bottom));
        // Back at the bottom the lock clears, whoever set it.
        assert!(!next_scroll_lock(true, true, bottom, bottom));
        assert!(next_scroll_lock(true, true, bottom - 0.3, bottom));
        // The live card has left the viewport: scrolled up, as before.
        assert!(next_scroll_lock(false, false, bottom - 2000.0, bottom));
        assert!(next_scroll_lock(true, false, bottom - 2000.0, bottom));
    }

    #[test]
    fn a_kept_scroll_lock_refuses_the_follow_bottom_pin() {
        let active = Cell::new(false);
        let generation = Cell::new(0u64);
        let locked = next_scroll_lock(true, true, 4800.0, 5000.0);
        assert!(!request_bottom_pin(locked, &active, &generation));
        assert!(!active.get());
    }

    #[test]
    fn stable_frame_count_resets_when_layout_moves() {
        assert_eq!(next_stable_frame_count(None, 100.0, 3), 0);
        assert_eq!(next_stable_frame_count(Some(100.0), 100.2, 2), 3);
        assert_eq!(next_stable_frame_count(Some(100.0), 101.0, 3), 0);
    }

    #[test]
    fn bottom_pin_coalesces_and_refreshes_overlapping_requests() {
        let active = Cell::new(false);
        let generation = Cell::new(0u64);

        assert!(request_bottom_pin(false, &active, &generation));
        assert!(active.get());
        assert_eq!(generation.get(), 1);

        assert!(!request_bottom_pin(false, &active, &generation));
        assert!(active.get());
        assert_eq!(generation.get(), 2);

        active.set(false);
        assert!(!request_bottom_pin(true, &active, &generation));
        assert!(!active.get());
        assert_eq!(generation.get(), 2);
    }

    #[test]
    #[ignore = "requires an isolated GTK process and display"]
    fn widget_pool_releases_heavy_children_and_stale_controllers() {
        gtk::init().expect("isolated widget-pool test requires a GTK display");
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        outer.append(&gtk::Label::new(Some("heavy child stand-in")));
        outer.add_controller(gtk::GestureClick::new());
        assert!(outer.first_child().is_some());
        assert!(outer.observe_controllers().n_items() > 0);

        let mut pool = WidgetPool::new();
        pool.release(outer);
        let recycled = pool.acquire().expect("outer shell should be pooled");
        assert!(recycled.first_child().is_none());
        assert_eq!(recycled.observe_controllers().n_items(), 0);

        let dropped = gtk::Box::new(gtk::Orientation::Vertical, 0);
        dropped.append(&gtk::Label::new(Some("full-pool child stand-in")));
        dropped.add_controller(gtk::GestureClick::new());
        let dropped_probe = dropped.clone();
        pool.max_pool_size = 0;
        pool.release(dropped);
        assert!(dropped_probe.first_child().is_none());
        assert_eq!(dropped_probe.observe_controllers().n_items(), 0);
        assert!(pool.acquire().is_none());

        let config = crate::config::Config::safe_defaults();
        let block = crate::block_view::FinishedBlock::new(
            77,
            "$ ",
            "printf test",
            None,
            "test\n",
            Some(0),
            &config,
            Some(1),
            None,
            None,
            80,
        );
        let output_vte = block.output_vte.downgrade();
        let outer = block.widget().clone();
        block.connect_scroll_forwarding(
            &gtk::ScrolledWindow::new(),
            &ScrollDebouncer::with_scroll_lock(
                Rc::new(Cell::new(false)),
                Rc::new(Cell::new(false)),
            ),
        );
        drop(block);
        pool.release(outer);
        assert!(output_vte.upgrade().is_none());
    }
}
