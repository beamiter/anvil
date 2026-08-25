//! Cross-block pseudo-continuous text selection.
//!
//! VTE selection is per-Terminal: each finished block owns a separate VTE so a
//! pointer drag from the tail of one block into another paints two unrelated
//! selections in a vanilla setup. This module sits above `block_scroll` and
//! turns a drag that crosses widget boundaries into a contiguous selection
//! across the involved VTEs.
//!
//! V1 granularity: per-widget. Crossed blocks (and the active live VTE if the
//! drag reaches it) are fully `select_all()`'d; endpoints keep whatever
//! per-cell selection VTE has already painted from the same drag.
//! vte-rs 0.10 does not expose `select_text(col, row, col, row)`, so finer
//! granularity would have to go through subclassing — left for V2.
//!
//! Single-block drags are untouched: the controller installs in the Capture
//! phase but only claims the gesture once the pointer leaves the block where
//! the drag started, so VTE's native per-cell selection still owns the common
//! case.

use relm4::gtk;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use vte4::TerminalExt;

use super::selection_hold::{feed_hold_eligible, SelectionFeedHold};
use super::{
    clear_finished_block_selection, BlockState, FinishedBlock, MouseReportingMode, SelectedBlockIds,
};

const MAX_CROSS_SELECTION_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectionTextTooLarge {
    bytes: usize,
    limit: usize,
}

impl std::fmt::Display for SelectionTextTooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "selected text is {} bytes; the limit is {} bytes",
            self.bytes, self.limit
        )
    }
}

fn append_selected_text(
    output: &mut String,
    text: &str,
    max_bytes: usize,
) -> Result<(), SelectionTextTooLarge> {
    let separator = usize::from(!output.is_empty());
    let next_len = output
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(text.len()))
        .unwrap_or(usize::MAX);
    if next_len > max_bytes {
        return Err(SelectionTextTooLarge {
            bytes: next_len,
            limit: max_bytes,
        });
    }
    if separator != 0 {
        output.push('\n');
    }
    output.push_str(text);
    Ok(())
}

pub(crate) struct CrossSelection {
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    active_vte: vte4::Terminal,
    selected_block_ids: SelectedBlockIds,
    selected_block_id: Rc<Cell<Option<u64>>>,
    selection_anchor_id: Rc<Cell<Option<u64>>>,
    /// Index in widget-order of where the current drag began (None when idle
    /// or the drag started outside any tracked VTE).
    start_idx: Cell<Option<usize>>,
    /// Once we've claimed the gesture and started painting cross-block
    /// selection, stay claimed for the rest of the drag.
    claimed: Cell<bool>,
    /// Parks the PTY feed while a drag covers the live VTE, so streaming
    /// repaints cannot clear the selection out from under the pointer.
    feed_hold: Rc<SelectionFeedHold>,
    bstate: Rc<Cell<BlockState>>,
    mouse_reporting: Rc<Cell<MouseReportingMode>>,
}

impl CrossSelection {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn install(
        block_scroll: &gtk::ScrolledWindow,
        finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
        active_vte: vte4::Terminal,
        selected_block_ids: SelectedBlockIds,
        selected_block_id: Rc<Cell<Option<u64>>>,
        selection_anchor_id: Rc<Cell<Option<u64>>>,
        feed_hold: Rc<SelectionFeedHold>,
        bstate: Rc<Cell<BlockState>>,
        mouse_reporting: Rc<Cell<MouseReportingMode>>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            finished_blocks,
            active_vte,
            selected_block_ids,
            selected_block_id,
            selection_anchor_id,
            start_idx: Cell::new(None),
            claimed: Cell::new(false),
            feed_hold,
            bstate,
            mouse_reporting,
        });

        // A native selection starts a new selection model. Clear stale text
        // selections on other surfaces and any whole-card selection first.
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_PRIMARY);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let scroll_for_click = block_scroll.downgrade();
        let this_for_click = Rc::downgrade(&this);
        click.connect_pressed(move |gesture, _n_press, x, y| {
            let (Some(this), Some(scroll)) = (this_for_click.upgrade(), scroll_for_click.upgrade())
            else {
                return;
            };
            let target = this.vte_at(&scroll, x, y);
            if target.is_some() {
                this.clear_block_selection();
            }
            this.clear_other_selections(target.as_ref());
            // Preserve VTE ownership of cell/word/line selection.
            gesture.set_state(gtk::EventSequenceState::Denied);
        });
        block_scroll.add_controller(click);

        let drag = gtk::GestureDrag::new();
        drag.set_button(gtk::gdk::BUTTON_PRIMARY);
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);

        let scroll_for_begin = block_scroll.downgrade();
        let this_for_begin = Rc::downgrade(&this);
        drag.connect_drag_begin(move |gesture, x, y| {
            let (Some(this), Some(scroll)) = (this_for_begin.upgrade(), scroll_for_begin.upgrade())
            else {
                return;
            };
            let start = this.vte_index_at(&scroll, x, y);
            this.start_idx.set(start);
            this.claimed.set(false);
            if let Some(start) = start {
                this.clear_block_selection();
                let vtes = this.ordered_vtes();
                this.maybe_hold_active_feed(vtes.get(start), shift_held(gesture));
                this.clear_other_selections(vtes.get(start));
            }
        });

        let scroll_for_update = block_scroll.downgrade();
        let this_for_update = Rc::downgrade(&this);
        drag.connect_drag_update(move |gesture, dx, dy| {
            let (Some(this), Some(scroll)) =
                (this_for_update.upgrade(), scroll_for_update.upgrade())
            else {
                return;
            };
            let Some(start) = this.start_idx.get() else {
                return;
            };
            let Some((sx, sy)) = gesture.start_point() else {
                return;
            };
            let cur = this.vte_index_at(&scroll, sx + dx, sy + dy);
            let Some(cur_idx) = cur else { return };
            if cur_idx == start && !this.claimed.get() {
                // Still within the original widget — let VTE's native gesture
                // own the per-cell selection.
                return;
            }
            // Crossed a boundary: claim and paint per-widget select_all on the
            // covered range.
            gesture.set_state(gtk::EventSequenceState::Claimed);
            this.claimed.set(true);
            this.paint_range(start, cur_idx);
            let vtes = this.ordered_vtes();
            this.maybe_hold_active_feed(vtes.get(start.max(cur_idx)), shift_held(gesture));
        });

        let this_for_end = Rc::downgrade(&this);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(this) = this_for_end.upgrade() {
                this.start_idx.set(None);
                this.end_active_feed_hold();
            }
            // Leave `claimed` and the painted selections in place so the user
            // can copy with Ctrl+Shift+C after releasing.
        });

        let this_for_cancel = Rc::downgrade(&this);
        drag.connect_cancel(move |_, _| {
            if let Some(this) = this_for_cancel.upgrade() {
                this.start_idx.set(None);
                this.end_active_feed_hold();
            }
        });

        block_scroll.add_controller(drag);
        this
    }

    fn maybe_hold_active_feed(&self, vte: Option<&vte4::Terminal>, shift_held: bool) {
        if vte == Some(&self.active_vte)
            && feed_hold_eligible(self.bstate.get(), self.mouse_reporting.get(), shift_held)
        {
            self.feed_hold.begin_drag();
        }
    }

    fn end_active_feed_hold(&self) {
        // Our capture-phase gesture ends before VTE's child gesture has
        // finalized its native selection. Reading `has_selection()` here can
        // therefore report false and replay Codex's next repaint immediately,
        // erasing the range the user just drew. Let the event finish, then
        // decide from VTE's settled state on the next main-loop turn.
        let hold = self.feed_hold.clone();
        let terminal = self.active_vte.downgrade();
        gtk::glib::idle_add_local_once(move || {
            hold.end_drag(
                terminal
                    .upgrade()
                    .is_some_and(|terminal| terminal.has_selection()),
            );
        });
    }

    fn clear_block_selection(&self) {
        if self.selected_block_id.get().is_none() {
            return;
        }
        let finished = self.finished_blocks.borrow();
        clear_finished_block_selection(
            &finished,
            &self.selected_block_ids,
            &self.selected_block_id,
            &self.selection_anchor_id,
        );
    }

    /// Every terminal surface in document order, including hidden ones used
    /// when clearing stale selections.
    fn all_vtes(&self) -> Vec<vte4::Terminal> {
        let finished = self.finished_blocks.borrow();
        let mut vtes = Vec::with_capacity(finished.len().saturating_mul(2) + 1);
        for block in finished.iter() {
            vtes.push(block.command_vte.clone());
            vtes.push(block.output_vte.clone());
        }
        vtes.push(self.active_vte.clone());
        vtes
    }

    /// Hidden, collapsed, or virtualized surfaces cannot contribute invisible
    /// text to cross-selection or copy.
    fn ordered_vtes(&self) -> Vec<vte4::Terminal> {
        self.all_vtes()
            .into_iter()
            .filter(|vte| vte.is_mapped() && vte.is_visible())
            .collect()
    }

    fn clear_other_selections(&self, keep: Option<&vte4::Terminal>) {
        for vte in self.all_vtes() {
            if keep.map(|target| target != &vte).unwrap_or(true) {
                vte.unselect_all();
            }
        }
    }

    fn vte_at(&self, block_scroll: &gtk::ScrolledWindow, x: f64, y: f64) -> Option<vte4::Terminal> {
        let picked = block_scroll.pick(x, y, gtk::PickFlags::DEFAULT)?;
        self.ordered_vtes()
            .into_iter()
            .find(|vte| widget_contains(vte, &picked))
    }

    /// Find which VTE in `ordered_vtes()` the pointer `(x, y)` (in
    /// `block_scroll` coords) lies over. Returns None when the pointer is over
    /// chrome/empty space.
    fn vte_index_at(&self, block_scroll: &gtk::ScrolledWindow, x: f64, y: f64) -> Option<usize> {
        let picked = block_scroll.pick(x, y, gtk::PickFlags::DEFAULT)?;
        let vtes = self.ordered_vtes();
        for (i, vte) in vtes.iter().enumerate() {
            if widget_contains(vte, &picked) {
                return Some(i);
            }
        }
        None
    }

    /// Set selection on every VTE in [min(a,b)..=max(a,b)] and clear all
    /// others. Idempotent — safe to call on every drag-update frame.
    fn paint_range(&self, a: usize, b: usize) {
        let vtes = self.ordered_vtes();
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        self.clear_all();
        for (i, vte) in vtes.iter().enumerate() {
            if i >= lo && i <= hi {
                vte.select_all();
            }
        }
    }

    pub(crate) fn clear_all(&self) {
        for vte in self.all_vtes() {
            vte.unselect_all();
        }
    }

    /// Collect every visible native or cross-widget VTE selection in document
    /// order. A single output-line drag is just as authoritative as a
    /// cross-block drag: callers use this before the whole-card selection so
    /// Ctrl+Shift+C copies the smaller highlight the user can still see.
    pub(crate) fn copy_text(&self) -> Result<Option<String>, SelectionTextTooLarge> {
        let mut output = String::new();
        for vte in self.ordered_vtes() {
            if !vte.has_selection() {
                continue;
            }
            if let Some(text) = vte.text_selected(vte4::Format::Text) {
                let s = text.to_string();
                if !s.is_empty() {
                    append_selected_text(&mut output, &s, MAX_CROSS_SELECTION_BYTES)?;
                }
            }
        }
        if output.is_empty() {
            Ok(None)
        } else {
            Ok(Some(output))
        }
    }

    pub(crate) fn has_text_selection(&self) -> bool {
        self.ordered_vtes()
            .into_iter()
            .any(|vte| vte.has_selection())
    }
}

fn shift_held(gesture: &gtk::GestureDrag) -> bool {
    gesture
        .current_event_state()
        .contains(gtk::gdk::ModifierType::SHIFT_MASK)
}

/// True if `needle` is `haystack` or one of its descendants. GTK's `pick()`
/// returns the deepest widget at a coordinate (often a text view inside the
/// VTE), so direct identity comparison won't match the VTE itself.
fn widget_contains(haystack: &impl IsA<gtk::Widget>, needle: &gtk::Widget) -> bool {
    let haystack = haystack.upcast_ref::<gtk::Widget>();
    let mut cur: Option<gtk::Widget> = Some(needle.clone());
    while let Some(w) = cur {
        if &w == haystack {
            return true;
        }
        cur = w.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{append_selected_text, CrossSelection};

    #[test]
    fn selected_text_aggregation_is_bounded_and_atomic() {
        let mut output = "first".to_owned();
        assert!(append_selected_text(&mut output, "two", 9).is_ok());
        assert_eq!(output, "first\ntwo");
        assert!(append_selected_text(&mut output, "x", 9).is_err());
        assert_eq!(output, "first\ntwo");
    }

    #[test]
    #[ignore = "requires DISPLAY; run explicitly under Xvfb"]
    fn a_single_native_text_selection_survives_whole_card_selection_precedence() {
        use std::cell::{Cell, RefCell};
        use std::collections::HashSet;
        use std::rc::Rc;

        use gtk::prelude::*;
        use relm4::gtk;
        use vte4::TerminalExt;

        use crate::block_view::{BlockState, FinishedBlock, MouseReportingMode, SelectionFeedHold};
        use crate::config::Config;

        gtk::init().expect("gtk init");
        let card = FinishedBlock::new(
            41,
            "$ ",
            "whole-card command",
            None,
            "visible needle\r\nother output\r\n",
            Some(0),
            &Config::safe_defaults(),
            Some(5),
            None,
            None,
            80,
        );
        let active = vte4::Terminal::new();
        let selected = Rc::new(RefCell::new(HashSet::from([41])));
        let cross = CrossSelection {
            finished_blocks: Rc::new(RefCell::new(vec![card.clone()])),
            active_vte: active.clone(),
            selected_block_ids: selected,
            selected_block_id: Rc::new(Cell::new(Some(41))),
            selection_anchor_id: Rc::new(Cell::new(Some(41))),
            start_idx: Cell::new(None),
            claimed: Cell::new(false),
            feed_hold: SelectionFeedHold::new(),
            bstate: Rc::new(Cell::new(BlockState::AwaitingCommand)),
            mouse_reporting: Rc::new(Cell::new(MouseReportingMode::None)),
        };

        let pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pane.append(card.widget());
        pane.append(&active);
        let window = gtk::Window::builder().child(&pane).build();
        window.present();
        while gtk::glib::MainContext::default().iteration(false) {}

        card.output_vte.select_all();
        while gtk::glib::MainContext::default().iteration(false) {}
        assert!(cross.has_text_selection());
        let copied = cross
            .copy_text()
            .expect("selection is below the clipboard cap")
            .expect("native selection is visible");
        assert!(copied.contains("visible needle"));
        assert!(
            !copied.contains("whole-card command"),
            "the native output highlight must win over the active whole-card selection"
        );

        window.close();
        while gtk::glib::MainContext::default().iteration(false) {}
    }
}
