//! Block-view scrolling, follow-bottom settling, and widget virtualization.
use gtk::glib;
use gtk::prelude::*;
use gtk::ScrolledWindow;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Ignore sub-pixel adjustment churn. GTK can report tiny floating-point
/// differences after layout even though the viewport is visually stationary;
/// writing those values back wakes every value-changed observer for no benefit.
const SCROLL_EPSILON_PX: f64 = 0.5;
/// Once the computed bottom target is unchanged for this many layout frames,
/// virtualized blocks have had enough time to realize and the pin can release.
const BOTTOM_STABLE_FRAMES: u8 = 4;
/// Safety bound for unusually slow virtual-layout settling.
const MAX_BOTTOM_PIN_TRIES: u8 = 12;

fn scroll_value_changed(current: f64, target: f64) -> bool {
    (current - target).abs() > SCROLL_EPSILON_PX
}

fn next_stable_frame_count(last_target: Option<f64>, target: f64, current: u8) -> u8 {
    match last_target {
        Some(last) if !scroll_value_changed(last, target) => current.saturating_add(1),
        _ => 0,
    }
}

/// Claim or refresh the single follow-bottom settling source.
///
/// A coalesced request still increments `generation`. The active timer observes
/// that change on its next frame and resets its retry/stability counters, giving
/// newly-added virtualized content a full settling window without starting a
/// second timer.
fn request_bottom_pin(user_scrolled: bool, active: &Cell<bool>, generation: &Cell<u64>) -> bool {
    if user_scrolled {
        return false;
    }
    generation.set(generation.get().wrapping_add(1));
    !active.replace(true)
}

/// Scrolls the block list to follow the live prompt — anvil's `autoscroll`
/// model, ported faithfully.
///
/// The key (and subtle) property is that the scroll happens **synchronously**,
/// from inside the PTY-reader's event handling, *before* GTK lays out any block
/// that was just appended. At that instant `upper` still reflects the previous
/// layout, so `upper - page` lands the view at the *top* of the freshly-finished
/// block instead of below it. Because nothing re-scrolls after layout settles,
/// the last finished block stays visible with the prompt directly below it.
/// Deferring this to a timer (or re-running it from the adjustment's `changed`
/// signal) reads the settled, larger `upper` and parks the view past the block.
///
/// This used to matter far more: the live holder was a full page tall during a
/// command, so a settled read hid every finished block behind blank rows. The
/// live card now grows with its output, which makes the stale-`upper` read a
/// smaller correction — but still the one that keeps the finished block's first
/// row on screen, and `pin_to_bottom_deferred` completes it.
#[derive(Clone)]
pub(crate) struct ScrollDebouncer {
    pub(crate) user_scrolled_up: Rc<Cell<bool>>,
    pub(crate) programmatic_scroll: Rc<Cell<bool>>,
    /// At most one frame-spaced follow-bottom source may run at a time. Repeated
    /// output/layout notifications refresh its generation instead of creating
    /// overlapping timers.
    bottom_pin_active: Rc<Cell<bool>>,
    bottom_pin_generation: Rc<Cell<u64>>,
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
        }
    }

    pub(crate) fn mark_dirty(&self, scroll: &ScrolledWindow) {
        if self.user_scrolled_up.get() {
            return;
        }
        let adj = scroll.vadjustment();
        let target = (adj.upper() - adj.page_size()).max(adj.lower());
        if !scroll_value_changed(adj.value(), target) {
            return;
        }
        // Guard the scroll with the programmatic flag so the scroll-lock detector
        // doesn't mistake it for the user dragging the scrollbar.
        self.programmatic_scroll.set(true);
        adj.set_value(target);
        self.programmatic_scroll.set(false);
    }

    /// Follow the live prompt across a few settled layout passes.
    ///
    /// With virtual scrolling, hidden blocks have 0 GTK height until the scroll
    /// position brings them near the viewport. A single `upper - page` jump can
    /// therefore land short: the jump reveals more blocks, those blocks expand,
    /// and `upper` grows on the next frame. Re-applying the bottom pin briefly
    /// keeps the latest finished block and prompt visible instead of leaving
    /// their lower rows clipped behind the active cell.
    pub(crate) fn pin_to_bottom_deferred(&self, scroll: &ScrolledWindow) {
        if !request_bottom_pin(
            self.user_scrolled_up.get(),
            &self.bottom_pin_active,
            &self.bottom_pin_generation,
        ) {
            return;
        }

        let scroll = scroll.clone();
        let user_scrolled = self.user_scrolled_up.clone();
        let programmatic = self.programmatic_scroll.clone();
        let bottom_pin_active = self.bottom_pin_active.clone();
        let bottom_pin_generation = self.bottom_pin_generation.clone();
        let observed_generation = Rc::new(Cell::new(bottom_pin_generation.get()));
        let tries = Rc::new(Cell::new(0u8));
        let last_target = Rc::new(Cell::new(None::<f64>));
        let stable_frames = Rc::new(Cell::new(0u8));

        // An idle source that returns `Continue` can run all retries before GTK
        // reaches another layout frame. Virtualized blocks then have not expanded
        // yet, so every retry observes the same stale adjustment. Space the
        // bounded retries over frames instead.
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            if user_scrolled.get() {
                bottom_pin_active.set(false);
                return glib::ControlFlow::Break;
            }

            // Another output/layout notification arrived while this timer was
            // active. Refresh the settling budget before checking the retry cap;
            // otherwise a request arriving on the last frame would be discarded.
            let generation = bottom_pin_generation.get();
            if observed_generation.get() != generation {
                observed_generation.set(generation);
                tries.set(0);
                last_target.set(None);
                stable_frames.set(0);
            }

            if tries.get() >= MAX_BOTTOM_PIN_TRIES {
                bottom_pin_active.set(false);
                return glib::ControlFlow::Break;
            }
            tries.set(tries.get() + 1);

            let adj = scroll.vadjustment();
            let target = (adj.upper() - adj.page_size()).max(adj.lower());
            let next_stable =
                next_stable_frame_count(last_target.get(), target, stable_frames.get());
            last_target.set(Some(target));
            stable_frames.set(next_stable);

            // Avoid re-emitting value-changed when layout has not moved the bottom.
            // Besides reducing work, this releases the viewport sooner so a user
            // wheel/drag immediately after command completion does not feel fought.
            if scroll_value_changed(adj.value(), target) {
                programmatic.set(true);
                adj.set_value(target);
                programmatic.set(false);
            }

            if next_stable >= BOTTOM_STABLE_FRAMES {
                bottom_pin_active.set(false);
                glib::ControlFlow::Break
            } else {
                // Virtualized blocks can become visible a frame or two after this
                // target appears stable. Require several stable frames before
                // stopping, while retaining the bounded retry fallback above.
                glib::ControlFlow::Continue
            }
        });
    }

    /// Record whether a wheel notch left the history away from its bottom.
    ///
    /// Call this straight after writing the outer adjustment. The other writer
    /// of `user_scrolled_up` is a deferred probe that compares the live card's
    /// top edge against the viewport, so it only flips once the view has moved
    /// by a whole card height — and while a command streams, the card is a full
    /// viewport. One notch moves a tenth of a page and the follow-bottom pin
    /// puts it back on the next frame, so the displacement could never
    /// accumulate to what that probe needs: wheeling up into the finished
    /// blocks kicked once per notch and went nowhere until the command ended.
    /// Reading the adjustment we just wrote settles it in one step, and a notch
    /// that lands back at the bottom clears the flag again.
    pub(crate) fn record_wheel_intent(&self, scroll: &ScrolledWindow) {
        let adjustment = scroll.vadjustment();
        let bottom = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        self.user_scrolled_up
            .set(scroll_value_changed(adjustment.value(), bottom));
    }

    pub(crate) fn reset_scroll_lock(&self) {
        self.user_scrolled_up.set(false);
    }
}

// ─── Virtual Scrolling ────────────────────────────────────────────────────────

pub(crate) struct ViewportState {
    pub(crate) first_visible: usize,
    pub(crate) last_visible: usize,
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
    fn ignores_subpixel_scroll_churn() {
        assert!(!scroll_value_changed(100.0, 100.4));
        assert!(scroll_value_changed(100.0, 100.6));
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
