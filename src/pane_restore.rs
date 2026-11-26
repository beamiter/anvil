use gtk::{glib, prelude::*};
use std::cell::{Cell, RefCell};
use relm4::gtk;

/// Retain the saved dividers before replacing the prepared first-leaf slot.
/// GTK may recompute a Paned position while its children or first allocation
/// change; applying it only to the off-notebook tree loses the saved geometry.
pub(crate) struct RestoredPaneDividers(Vec<(glib::WeakRef<gtk::Paned>, i32)>);

impl RestoredPaneDividers {
    pub(crate) fn capture(root: &gtk::Widget) -> Self {
        fn collect(widget: &gtk::Widget, saved: &mut Vec<(glib::WeakRef<gtk::Paned>, i32)>) {
            let Some(paned) = widget.downcast_ref::<gtk::Paned>() else {
                return;
            };
            if paned.position() >= 0 {
                saved.push((paned.downgrade(), paned.position()));
            }
            if let Some(start) = paned.start_child() {
                collect(&start, saved);
            }
            if let Some(end) = paned.end_child() {
                collect(&end, saved);
            }
        }
        let mut saved = Vec::new();
        collect(root, &mut saved);
        Self(saved)
    }

    pub(crate) fn schedule(self, root: &gtk::Widget) {
        if self.0.is_empty() {
            return;
        }
        let limit = self.0.len().saturating_mul(2).saturating_add(6);
        let rounds = Cell::new(0);
        let stable = Cell::new(0);
        let previous = RefCell::new(Vec::new());
        root.add_tick_callback(move |_, _| {
            rounds.set(rounds.get() + 1);
            let panes: Vec<_> = self
                .0
                .iter()
                .filter_map(|(weak, position)| weak.upgrade().map(|paned| (paned, *position)))
                .collect();
            if panes.len() != self.0.len() {
                return glib::ControlFlow::Break;
            }
            let geometry: Vec<_> = panes
                .iter()
                .map(|(pane, _)| (pane.width(), pane.height()))
                .collect();
            if geometry.iter().all(|(width, height)| *width > 1 && *height > 1) {
                // Parent-first on consecutive allocated frames also settles
                // nested splits after an ancestor's divider moves.
                for (paned, position) in &panes {
                    paned.set_position(*position);
                }
                stable.set(if geometry == *previous.borrow() { stable.get() + 1 } else { 0 });
                *previous.borrow_mut() = geometry;
                if stable.get() >= 2 {
                    return glib::ControlFlow::Break;
                }
            }
            if rounds.get() >= limit {
                log::warn!("Restored pane dividers did not settle within the bounded allocation window; keeping GTK's last valid geometry");
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}
