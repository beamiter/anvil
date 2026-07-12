//! scroll — extracted from block_view (mechanical split, no logic changes)
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

/// Scrolls the block list to follow the live prompt — jterm1's `autoscroll`
/// model, ported faithfully.
///
/// The key (and subtle) property is that the scroll happens **synchronously**,
/// from inside the PTY-reader's event handling, *before* GTK lays out any block
/// that was just appended. At that instant `upper` still reflects the previous
/// layout, so `upper - page` lands the view at the *top* of the freshly-finished
/// block rather than at the bottom of the page-tall live holder. Because nothing
/// re-scrolls after layout settles, the last finished block stays visible with
/// the prompt directly below it. Deferring this to a timer (or re-running it from
/// the adjustment's `changed` signal) reads the settled, larger `upper` and parks
/// the view at the bottom of the blank holder, hiding all history.
pub(crate) struct ScrollDebouncer {
    pub(crate) user_scrolled_up: Rc<Cell<bool>>,
    pub(crate) programmatic_scroll: Rc<Cell<bool>>,
}

impl ScrollDebouncer {
    pub(crate) fn with_scroll_lock(
        user_scrolled_up: Rc<Cell<bool>>,
        programmatic_scroll: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            user_scrolled_up,
            programmatic_scroll,
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
        if self.user_scrolled_up.get() {
            return;
        }

        let scroll = scroll.clone();
        let user_scrolled = self.user_scrolled_up.clone();
        let programmatic = self.programmatic_scroll.clone();
        let tries = Rc::new(Cell::new(0u8));
        let last_target = Rc::new(Cell::new(None::<f64>));
        let stable_frames = Rc::new(Cell::new(0u8));

        // An idle source that returns `Continue` can run all retries before GTK
        // reaches another layout frame. Virtualized blocks then have not expanded
        // yet, so every retry observes the same stale adjustment. Space the
        // bounded retries over frames instead.
        glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            if user_scrolled.get() || tries.get() >= MAX_BOTTOM_PIN_TRIES {
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
                glib::ControlFlow::Break
            } else {
                // Virtualized blocks can become visible a frame or two after this
                // target appears stable. Require several stable frames before
                // stopping, while retaining the bounded retry fallback above.
                glib::ControlFlow::Continue
            }
        });
    }

    pub(crate) fn reset_scroll_lock(&self) {
        self.user_scrolled_up.set(false);
    }
}

// ─── Virtual Scrolling ────────────────────────────────────────────────────────

pub(crate) struct ViewportState {
    pub(crate) first_visible: usize,
    pub(crate) last_visible: usize,
    pub(crate) total_height: i32,
}

impl Clone for ViewportState {
    fn clone(&self) -> Self {
        Self {
            first_visible: self.first_visible,
            last_visible: self.last_visible,
            total_height: self.total_height,
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

    pub(crate) fn release(&mut self, widget: gtk::Box) {
        if self.available.len() < self.max_pool_size {
            // A recycled finished-block container has gesture/motion controllers
            // whose closures capture the old block ID and action handles. Keeping
            // them makes a newly rendered block react as its predecessor (and
            // stacks duplicate hover/right-click handlers on each reuse).
            // Controllers belong only to this short-lived outer Box, so clear
            // them before pooling; `FinishedBlock::new_with_pool` installs the
            // fresh handlers for the new block.
            let controllers = widget.observe_controllers();
            while let Some(controller) = controllers.item(0) {
                if let Ok(controller) = controller.downcast::<gtk::EventController>() {
                    widget.remove_controller(&controller);
                } else {
                    break;
                }
            }
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
}
