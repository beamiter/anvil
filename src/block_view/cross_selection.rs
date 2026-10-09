//! Cross-block pseudo-continuous text selection.
//!
//! VTE selection is per-Terminal: each finished block owns a separate VTE so a
//! pointer drag from the tail of one block into another paints two unrelated
//! selections in a vanilla setup. This module sits above `block_scroll` and
//! turns a drag that crosses widget boundaries into a contiguous selection
//! across the involved VTEs.
//!
//! Cross-widget selection covers whole terminal surfaces. One application
//! owner tracks their identities and paints a non-measuring outline. VTE's
//! public ring exporter supplies the complete retained text without changing
//! PRIMARY; repeated native `select_all()` calls would clear earlier owners.
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
    clear_finished_block_selection, BlockState, FinishedBlock, MouseReporting, SelectedBlockIds,
};

const MAX_CROSS_SELECTION_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionCopyError {
    TooLarge { bytes: usize, limit: usize },
    TimedOut,
    CaptureFailed,
}

impl std::fmt::Display for SelectionCopyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes, limit } => write!(
                formatter,
                "selected text is at least {bytes} bytes; the limit is {limit} bytes"
            ),
            Self::TimedOut => formatter.write_str("selected text took too long to read"),
            Self::CaptureFailed => {
                formatter.write_str("selected text could not be read completely")
            }
        }
    }
}

/// VTE's public ring exporter writes incrementally and never claims PRIMARY.
/// Refuse overflow or slow extraction before publishing any clipboard text.
struct SelectionWriter {
    bytes: Vec<u8>,
    limit: usize,
    deadline: std::time::Instant,
    failure: Option<SelectionCopyError>,
}

impl std::io::Write for SelectionWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.len().saturating_add(bytes.len());
        self.failure = if next > self.limit {
            Some(SelectionCopyError::TooLarge {
                bytes: next,
                limit: self.limit,
            })
        } else if std::time::Instant::now() >= self.deadline {
            Some(SelectionCopyError::TimedOut)
        } else {
            None
        };
        if self.failure.is_some() {
            return Err(std::io::Error::other("selection capture limit reached"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_surface_text(
    vte: &vte4::Terminal,
    max_bytes: usize,
    deadline: std::time::Instant,
) -> Result<String, SelectionCopyError> {
    let stream = gtk::gio::WriteOutputStream::new(SelectionWriter {
        bytes: Vec::new(),
        limit: max_bytes,
        deadline,
        failure: None,
    });
    let result = vte.write_contents_sync(
        &stream,
        vte4::WriteFlags::Default,
        gtk::gio::Cancellable::NONE,
    );
    let writer = stream
        .close_and_take()
        .downcast::<SelectionWriter>()
        .map_err(|_| SelectionCopyError::CaptureFailed)?;
    if let Some(error) = writer.failure {
        return Err(error);
    }
    result.map_err(|_| SelectionCopyError::CaptureFailed)?;
    String::from_utf8(writer.bytes).map_err(|_| SelectionCopyError::CaptureFailed)
}

struct SelectedSurface {
    terminal: gtk::glib::WeakRef<vte4::Terminal>,
    handlers: Vec<gtk::glib::SignalHandlerId>,
}

impl Drop for SelectedSurface {
    fn drop(&mut self) {
        if let Some(terminal) = self.terminal.upgrade() {
            for handler in self.handlers.drain(..) {
                terminal.disconnect(handler);
            }
            terminal.remove_css_class("cross-block-text-selected");
        }
    }
}

fn append_selected_text(
    output: &mut String,
    text: &str,
    max_bytes: usize,
) -> Result<(), SelectionCopyError> {
    let separator = usize::from(!output.is_empty());
    let next_len = output
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(text.len()))
        .unwrap_or(usize::MAX);
    if next_len > max_bytes {
        return Err(SelectionCopyError::TooLarge {
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
    /// Stable identity of the drag origin. Virtualization and history eviction
    /// can change mapped-widget indices while a pointer drag is in progress.
    /// A weak reference does not retain an evicted terminal and its scrollback.
    start_vte: RefCell<Option<gtk::glib::WeakRef<vte4::Terminal>>>,
    /// Once we've claimed the gesture and started painting cross-block
    /// selection, stay claimed for the rest of the drag.
    claimed: Cell<bool>,
    drag_generation: Cell<u64>,
    /// The only owner of a whole-surface cross-block selection. Native VTE
    /// selections share PRIMARY and cannot coexist on multiple terminals.
    selected_surfaces: RefCell<Vec<SelectedSurface>>,
    /// Parks the PTY feed while a drag covers the live VTE, so streaming
    /// repaints cannot clear the selection out from under the pointer.
    feed_hold: Rc<SelectionFeedHold>,
    bstate: Rc<Cell<BlockState>>,
    mouse_reporting: Rc<Cell<MouseReporting>>,
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
        mouse_reporting: Rc<Cell<MouseReporting>>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            finished_blocks,
            active_vte,
            selected_block_ids,
            selected_block_id,
            selection_anchor_id,
            start_vte: RefCell::new(None),
            claimed: Cell::new(false),
            drag_generation: Cell::new(0),
            selected_surfaces: RefCell::new(Vec::new()),
            feed_hold,
            bstate,
            mouse_reporting,
        });

        let weak = Rc::downgrade(&this);
        this.feed_hold.set_release_listener(move || {
            if let Some(this) = weak.upgrade() {
                if this.surface_selected(&this.active_vte) {
                    this.clear_cross_selection();
                }
            }
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
            this.drag_generation
                .set(this.drag_generation.get().wrapping_add(1));
            let start = this.vte_at(&scroll, x, y);
            if start.as_ref() == Some(&this.active_vte)
                && this.mouse_reporting.get().is_tracking()
                && !shift_held(gesture)
            {
                this.start_vte.borrow_mut().take();
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            *this.start_vte.borrow_mut() = start.as_ref().map(|vte| vte.downgrade());
            this.claimed.set(false);
            if let Some(start) = start {
                this.clear_block_selection();
                this.clear_other_selections(Some(&start));
                this.maybe_hold_active_feed(Some(&start), shift_held(gesture));
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
            let Some(origin) = this.start_vte.borrow().clone() else {
                return;
            };
            let Some(start_vte) = origin.upgrade() else {
                this.cancel_drag_selection();
                return;
            };
            let Some((sx, sy)) = gesture.start_point() else {
                return;
            };
            let Some(current_vte) = this.vte_at(&scroll, sx + dx, sy + dy) else {
                return;
            };
            let vtes = this.ordered_vtes();
            let Some((start, cur_idx)) = drag_surface_indices(&vtes, &start_vte, &current_vte)
            else {
                // The origin was folded, virtualized or evicted. Never apply
                // its old index to an unrelated surface.
                this.cancel_drag_selection();
                return;
            };
            if cur_idx == start && !this.claimed.get() {
                // Still within the original widget — let VTE's native gesture
                // own the per-cell selection.
                return;
            }
            // Crossed a boundary: one owner marks the covered surfaces without
            // competing for VTE's shared PRIMARY selection.
            gesture.set_state(gtk::EventSequenceState::Claimed);
            this.claimed.set(true);
            this.paint_range(&vtes, start, cur_idx);
            this.maybe_hold_active_feed(vtes.get(start.max(cur_idx)), shift_held(gesture));
        });

        let this_for_end = Rc::downgrade(&this);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(this) = this_for_end.upgrade() {
                this.start_vte.borrow_mut().take();
                this.end_active_feed_hold();
            }
            // Leave `claimed` and the painted selections in place so the user
            // can copy with Ctrl+Shift+C after releasing.
        });

        let this_for_cancel = Rc::downgrade(&this);
        drag.connect_cancel(move |_, _| {
            if let Some(this) = this_for_cancel.upgrade() {
                this.start_vte.borrow_mut().take();
                this.end_active_feed_hold();
            }
        });

        block_scroll.add_controller(drag);
        this
    }

    fn cancel_drag_selection(self: &Rc<Self>) {
        self.start_vte.borrow_mut().take();
        self.claimed.set(false);
        // GTK can unmap a selected VTE while the caller is borrowing the
        // history or parser state. Clear only weak-owned visual state here;
        // traversing all_vtes or replaying PTY bytes would re-enter that borrow.
        self.clear_cross_selection();
        let hold = self.feed_hold.clone();
        let weak = Rc::downgrade(self);
        let generation = self.drag_generation.get();
        gtk::glib::idle_add_local_once(move || {
            if weak
                .upgrade()
                .is_some_and(|this| this.drag_generation.get() != generation)
            {
                return;
            }
            let selection_alive = weak.upgrade().is_some_and(|this| {
                this.active_vte.is_mapped()
                    && this.active_vte.is_visible()
                    && (this.active_vte.has_selection() || this.surface_selected(&this.active_vte))
            });
            hold.end_drag(selection_alive);
            // Cancellation also happens after drag-end, when end_drag(false)
            // alone would do nothing. A newer native selection may already
            // own the live hold, so only force release without a visible owner.
            if !selection_alive {
                hold.flush_now();
            }
        });
    }

    fn maybe_hold_active_feed(&self, vte: Option<&vte4::Terminal>, shift_held: bool) {
        if vte == Some(&self.active_vte)
            && feed_hold_eligible(self.bstate.get(), self.mouse_reporting.get(), shift_held)
        {
            self.feed_hold.begin_drag();
        }
    }

    fn end_active_feed_hold(self: &Rc<Self>) {
        // Our capture-phase gesture ends before VTE's child gesture has
        // finalized its native selection. Reading `has_selection()` here can
        // therefore report false and replay Codex's next repaint immediately,
        // erasing the range the user just drew. Let the event finish, then
        // decide from VTE's settled state on the next main-loop turn.
        let hold = self.feed_hold.clone();
        let weak = Rc::downgrade(self);
        let generation = self.drag_generation.get();
        gtk::glib::idle_add_local_once(move || {
            if weak
                .upgrade()
                .is_some_and(|this| this.drag_generation.get() != generation)
            {
                return;
            }
            hold.end_drag(weak.upgrade().is_some_and(|this| {
                this.active_vte.has_selection() || this.surface_selected(&this.active_vte)
            }));
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
        let had_cross_selection = !self.selected_surfaces.borrow().is_empty();
        self.clear_cross_selection();
        // Click and drag capture controllers can observe the same press in
        // either order. Only retire an old custom owner here; an unconditional
        // flush could undo the new drag's just-established live feed hold.
        if had_cross_selection {
            self.feed_hold.flush_now();
        }
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

    fn surface_selected(&self, terminal: &vte4::Terminal) -> bool {
        self.selected_surfaces
            .borrow()
            .iter()
            .any(|surface| surface.terminal.upgrade().as_ref() == Some(terminal))
    }

    fn clear_cross_selection(&self) {
        // Detach before GTK callbacks so unmap/selection signals cannot
        // re-enter a mutable borrow of the selection owner.
        let old = std::mem::take(&mut *self.selected_surfaces.borrow_mut());
        drop(old);
    }

    /// Whole-surface cross selection has one application owner. Calling
    /// select_all on several VTEs loses every selection except PRIMARY's last
    /// owner. A non-measuring outline marks the exact surfaces copied instead.
    fn paint_range(self: &Rc<Self>, vtes: &[vte4::Terminal], a: usize, b: usize) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let range = &vtes[lo.min(vtes.len())..hi.saturating_add(1).min(vtes.len())];
        if self.selected_surfaces.borrow().len() == range.len()
            && self
                .selected_surfaces
                .borrow()
                .iter()
                .zip(range)
                .all(|(selected, terminal)| selected.terminal.upgrade().as_ref() == Some(terminal))
        {
            return;
        }
        self.clear_cross_selection();
        for vte in self.all_vtes() {
            vte.unselect_all();
        }
        let mut selected = Vec::new();
        for terminal in range {
            terminal.add_css_class("cross-block-text-selected");
            let weak = Rc::downgrade(self);
            let unmap = terminal.connect_unmap(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.cancel_drag_selection();
                }
            });
            let weak = Rc::downgrade(self);
            let contents = terminal.connect_contents_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.cancel_drag_selection();
                }
            });
            let weak = Rc::downgrade(self);
            let native_selection = terminal.connect_selection_changed(move |terminal| {
                if terminal.has_selection() {
                    if let Some(this) = weak.upgrade() {
                        this.clear_cross_selection();
                        this.start_vte.borrow_mut().take();
                        this.claimed.set(false);
                    }
                }
            });
            selected.push(SelectedSurface {
                terminal: terminal.downgrade(),
                handlers: vec![unmap, contents, native_selection],
            });
        }
        *self.selected_surfaces.borrow_mut() = selected;
    }

    pub(crate) fn clear_all(&self) {
        self.clear_cross_selection();
        for vte in self.all_vtes() {
            vte.unselect_all();
        }
        self.feed_hold.flush_now();
    }

    fn selected_surface_snapshot(&self) -> Result<Vec<vte4::Terminal>, SelectionCopyError> {
        let selected: Vec<_> = self
            .selected_surfaces
            .borrow()
            .iter()
            .map(|surface| {
                surface
                    .terminal
                    .upgrade()
                    .ok_or(SelectionCopyError::CaptureFailed)
            })
            .collect::<Result<_, _>>()?;
        let in_document: Vec<_> = self
            .ordered_vtes()
            .into_iter()
            .filter(|terminal| selected.contains(terminal))
            .collect();
        if in_document != selected {
            return Err(SelectionCopyError::CaptureFailed);
        }
        Ok(selected)
    }

    /// Collect every visible native or cross-widget VTE selection in document
    /// order. A single output-line drag is just as authoritative as a
    /// cross-block drag: callers use this before the whole-card selection so
    /// Ctrl+Shift+C copies the smaller highlight the user can still see.
    pub(crate) fn copy_text(&self) -> Result<Option<String>, SelectionCopyError> {
        let mut output = String::new();
        let selected = if self.selected_surfaces.borrow().is_empty() {
            None
        } else {
            Some(self.selected_surface_snapshot()?)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        for vte in selected.clone().unwrap_or_else(|| self.ordered_vtes()) {
            let text = if selected.is_some() {
                Some(
                    capture_surface_text(
                        &vte,
                        MAX_CROSS_SELECTION_BYTES.saturating_sub(output.len()),
                        deadline,
                    )
                    .map_err(|error| match error {
                        SelectionCopyError::TooLarge { bytes, .. } => {
                            SelectionCopyError::TooLarge {
                                bytes: output.len().saturating_add(bytes),
                                limit: MAX_CROSS_SELECTION_BYTES,
                            }
                        }
                        error => error,
                    })?,
                )
            } else {
                if !vte.has_selection() {
                    continue;
                }
                vte.text_selected(vte4::Format::Text)
                    .map(|text| text.to_string())
            };
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                append_selected_text(&mut output, &text, MAX_CROSS_SELECTION_BYTES)?;
            }
        }
        if let Some(selected) = selected {
            if self.selected_surface_snapshot()? != selected {
                return Err(SelectionCopyError::CaptureFailed);
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
            .any(|vte| vte.has_selection() || self.surface_selected(&vte))
    }
}

/// Resolve both endpoints against the same current mapped-surface snapshot.
fn drag_surface_indices<T: PartialEq>(
    surfaces: &[T],
    start: &T,
    current: &T,
) -> Option<(usize, usize)> {
    Some((
        surfaces.iter().position(|surface| surface == start)?,
        surfaces.iter().position(|surface| surface == current)?,
    ))
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
    use super::{append_selected_text, drag_surface_indices, CrossSelection};

    #[test]
    fn streaming_selection_writer_refuses_overflow_and_expiry_without_partial_append() {
        use std::io::Write;
        let mut writer = super::SelectionWriter {
            bytes: Vec::new(),
            limit: 4,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
            failure: None,
        };
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"de").is_err());
        assert_eq!(writer.bytes, b"abc");
        assert!(matches!(
            writer.failure,
            Some(super::SelectionCopyError::TooLarge { .. })
        ));
        writer.deadline = std::time::Instant::now();
        assert!(writer.write_all(b"d").is_err());
        assert_eq!(writer.bytes, b"abc");
        assert_eq!(writer.failure, Some(super::SelectionCopyError::TimedOut));
    }

    #[test]
    fn drag_origin_follows_identity_when_mapped_history_changes() {
        let initial = [10, 11, 20, 21, 30];
        assert_eq!(drag_surface_indices(&initial, &21, &30), Some((3, 4)));
        // Virtualizing a preceding card removes its two surfaces.
        assert_eq!(drag_surface_indices(&initial[2..], &21, &30), Some((1, 2)));
        // Newly realized history before the anchor changes indices again.
        assert_eq!(
            drag_surface_indices(&[1, 2, 20, 21, 30], &21, &20),
            Some((3, 2))
        );
        // Hiding/evicting either endpoint must not select its replacement.
        assert_eq!(drag_surface_indices(&[20, 30], &21, &30), None);
        assert_eq!(drag_surface_indices(&[20, 21], &21, &30), None);
    }

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

        use crate::block_view::{BlockState, FinishedBlock, MouseReporting, SelectionFeedHold};
        use crate::config::Config;

        fn pump() {
            for _ in 0..64 {
                if !gtk::glib::MainContext::default().iteration(false) {
                    break;
                }
            }
        }
        fn settle() {
            for _ in 0..20 {
                pump();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        gtk::init().expect("gtk init");
        super::super::install_block_css(&Config::safe_defaults());
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
        active.feed(b"live-tail\r\n");
        let selected = Rc::new(RefCell::new(HashSet::from([41])));
        let scroll = gtk::ScrolledWindow::new();
        let cross = CrossSelection::install(
            &scroll,
            Rc::new(RefCell::new(vec![card.clone()])),
            active.clone(),
            selected,
            Rc::new(Cell::new(Some(41))),
            Rc::new(Cell::new(Some(41))),
            SelectionFeedHold::new(),
            Rc::new(Cell::new(BlockState::CollectingOutput)),
            Rc::new(Cell::new(MouseReporting::OFF)),
        );

        cross
            .feed_hold
            .install_vte_hooks(&active, Rc::new(Cell::new(0)));

        let pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pane.append(card.widget());
        pane.append(&active);
        scroll.set_child(Some(&pane));
        let window = gtk::Window::builder()
            .child(&scroll)
            .default_width(640)
            .default_height(480)
            .build();
        window.present();
        settle();

        if let Ok(driver) = std::env::var("ANVIL_XTEST_DRIVER") {
            window.set_title(Some("anvil-cross-selection-qa"));
            let start = card.output_vte.compute_bounds(&window).unwrap();
            let end = active.compute_bounds(&window).unwrap();
            let pointer_replayed = Rc::new(RefCell::new(Vec::new()));
            let pointer_replayed_cb = pointer_replayed.clone();
            cross
                .feed_hold
                .set_flush(move |bytes| pointer_replayed_cb.borrow_mut().extend(bytes));
            for (from, to) in [(&start, &end), (&end, &start)] {
                let actual_primary = active.primary_clipboard();
                let actual_clipboard = active.clipboard();
                actual_primary.set_text("actual pointer primary sentinel");
                actual_clipboard.set_text("actual pointer clipboard sentinel");
                settle();
                let mut child = std::process::Command::new("python3")
                    .arg(&driver)
                    .args([
                        (from.x() + 10.0).to_string(),
                        (from.y() + 8.0).to_string(),
                        (to.x() + 10.0).to_string(),
                        (to.y() + 8.0).to_string(),
                    ])
                    .spawn()
                    .expect("XTest pointer probe starts");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    pump();
                    if let Some(status) = child.try_wait().unwrap() {
                        assert!(status.success(), "pointer probe failed");
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("pointer probe timed out");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                settle();
                assert!(
                    cross.surface_selected(&card.output_vte),
                    "real drag owns the origin output"
                );
                assert!(
                    cross.surface_selected(&active),
                    "real drag owns the reached live surface"
                );
                assert_eq!(
                    gtk::glib::MainContext::default()
                        .block_on(actual_primary.read_text_future())
                        .unwrap()
                        .as_deref(),
                    Some("actual pointer primary sentinel"),
                    "real pointer drag preserves PRIMARY"
                );
                assert_eq!(
                    gtk::glib::MainContext::default()
                        .block_on(actual_clipboard.read_text_future())
                        .unwrap()
                        .as_deref(),
                    Some("actual pointer clipboard sentinel"),
                    "real pointer drag preserves CLIPBOARD"
                );
                let text = cross.copy_text().unwrap().unwrap();
                assert!(text.contains("visible needle") && text.contains("live-tail"));
                assert!(
                    cross.feed_hold.try_buffer(b"pointer held tail"),
                    "actual drag keeps live output parked until selection is released"
                );
                cross.clear_all();
                assert_eq!(&*pointer_replayed.borrow(), b"pointer held tail");
                pointer_replayed.borrow_mut().clear();
            }
        }

        card.output_vte.select_all();
        pump();
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

        // A drag's anchor must survive a preceding surface disappearing from
        // the mapped list (the same transition virtualization/folding causes).
        *cross.start_vte.borrow_mut() = Some(card.output_vte.downgrade());
        let before = cross.ordered_vtes();
        let before_indices = drag_surface_indices(&before, &card.output_vte, &active)
            .expect("both drag endpoints are initially mapped");
        card.command_vte.set_visible(false);
        // Hiding command chrome queues a geometry refit and a VTE re-feed.
        // Let that complete before exercising selection on the new layout.
        settle();
        let after = cross.ordered_vtes();
        let anchor = cross
            .start_vte
            .borrow()
            .as_ref()
            .and_then(|origin| origin.upgrade())
            .expect("origin still exists");
        let after_indices = drag_surface_indices(&after, &anchor, &active)
            .expect("hiding an earlier surface must preserve the endpoints");
        assert_eq!(after_indices.0 + 1, before_indices.0);
        let primary = active.primary_clipboard();
        let clipboard = active.clipboard();
        primary.set_text("primary sentinel");
        clipboard.set_text("clipboard sentinel");
        cross.paint_range(&after, after_indices.0, after_indices.1);
        settle();
        assert!(cross.surface_selected(&card.output_vte));
        assert!(cross.surface_selected(&active));
        let style_changes = Rc::new(Cell::new(0));
        let observed_styles = style_changes.clone();
        let style_handler = active.connect_notify_local(Some("css-classes"), move |_, _| {
            observed_styles.set(observed_styles.get() + 1)
        });
        cross.paint_range(&after, after_indices.0, after_indices.1);
        assert_eq!(
            style_changes.get(),
            0,
            "identical drag frames must not repaint ownership"
        );
        active.disconnect(style_handler);
        let copied_range = cross.copy_text().unwrap().unwrap();
        assert!(copied_range.contains("visible needle"));
        assert!(copied_range.contains("live-tail"));
        assert!(copied_range.find("visible needle") < copied_range.find("live-tail"));
        if let Ok(directory) = std::env::var("ANVIL_REVIEW_SCREENSHOT_DIR") {
            let paintable = gtk::WidgetPaintable::new(Some(&window));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(
                &snapshot,
                f64::from(window.width()),
                f64::from(window.height()),
            );
            {
                let node = snapshot
                    .to_node()
                    .expect("mapped cross-selection window paints");
                std::fs::create_dir_all(&directory).unwrap();
                window
                    .renderer()
                    .unwrap()
                    .render_texture(&node, None)
                    .save_to_png(format!("{directory}/anvil-cross-selection.png"))
                    .unwrap();
            }
        }
        assert_eq!(
            gtk::glib::MainContext::default()
                .block_on(primary.read_text_future())
                .unwrap()
                .as_deref(),
            Some("primary sentinel")
        );
        assert_eq!(
            gtk::glib::MainContext::default()
                .block_on(clipboard.read_text_future())
                .unwrap()
                .as_deref(),
            Some("clipboard sentinel")
        );
        assert!(!card.command_vte.has_selection());

        let replayed = Rc::new(RefCell::new(Vec::new()));
        let replayed_cb = replayed.clone();
        cross
            .feed_hold
            .set_flush(move |bytes| replayed_cb.borrow_mut().extend(bytes));
        cross.feed_hold.begin_drag();
        assert!(cross.feed_hold.try_buffer(b"live parked"));
        cross.end_active_feed_hold();
        // A new press can arrive before the old release's idle callback.
        // Its temporary empty native selection must not replay parked output.
        cross
            .drag_generation
            .set(cross.drag_generation.get().wrapping_add(1));
        cross.feed_hold.begin_drag();
        pump();
        active.emit_by_name::<()>("selection-changed", &[]);
        assert!(
            replayed.borrow().is_empty(),
            "an old release cannot end a newer drag"
        );
        cross.end_active_feed_hold();
        pump();
        assert!(
            replayed.borrow().is_empty(),
            "visible cross selection keeps the live feed parked"
        );
        active.select_all();
        pump();
        assert!(active.has_selection());
        assert!(
            !cross.surface_selected(&active),
            "native selection takes ownership"
        );
        assert!(
            replayed.borrow().is_empty(),
            "native selection keeps the same feed hold"
        );
        active.unselect_all();
        pump();
        assert_eq!(&*replayed.borrow(), b"live parked");
        assert!(!cross.surface_selected(&active));
        cross.paint_range(&after, after_indices.0, after_indices.1);
        let reentry = Rc::new(Cell::new(false));
        let reentry_cb = reentry.clone();
        let flushed = Rc::new(RefCell::new(Vec::new()));
        let flushed_cb = flushed.clone();
        let history = cross.finished_blocks.clone();
        cross.feed_hold.set_flush(move |bytes| {
            reentry_cb.set(history.try_borrow_mut().is_err());
            flushed_cb.borrow_mut().extend(bytes);
        });
        for mutable_borrow in [false, true] {
            card.output_vte.set_visible(true);
            settle();
            cross.paint_range(&after, after_indices.0, after_indices.1);
            flushed.borrow_mut().clear();
            reentry.set(false);
            cross.feed_hold.begin_drag();
            assert!(cross.feed_hold.try_buffer(b"borrowed history tail"));
            cross.feed_hold.end_drag(true);
            if mutable_borrow {
                let _borrowed_history = cross.finished_blocks.borrow_mut();
                card.output_vte.set_visible(false);
                assert!(
                    flushed.borrow().is_empty(),
                    "unmap under mutable history borrow must defer replay"
                );
            } else {
                let _borrowed_history = cross.finished_blocks.borrow();
                card.output_vte.set_visible(false);
                assert!(
                    flushed.borrow().is_empty(),
                    "unmap under immutable history borrow must defer replay"
                );
            }
            pump();
            assert!(!reentry.get());
            assert_eq!(&*flushed.borrow(), b"borrowed history tail");
        }
        assert_eq!(
            drag_surface_indices(&cross.ordered_vtes(), &anchor, &active),
            None
        );

        // A new native selection before the cancellation idle owns the same
        // parked feed; retiring the old cross range must not erase it.
        flushed.borrow_mut().clear();
        cross.feed_hold.begin_drag();
        assert!(cross.feed_hold.try_buffer(b"native successor tail"));
        cross.feed_hold.end_drag(true);
        cross.cancel_drag_selection();
        active.select_all();
        pump();
        assert!(active.has_selection());
        assert!(flushed.borrow().is_empty());
        active.unselect_all();
        pump();
        assert_eq!(&*flushed.borrow(), b"native successor tail");

        // Even before a disappearance notification is handled, copy must not
        // silently return the remaining subset of the old selection.
        cross.selected_surfaces.borrow_mut().extend([
            super::SelectedSurface {
                terminal: card.output_vte.downgrade(),
                handlers: Vec::new(),
            },
            super::SelectedSurface {
                terminal: active.downgrade(),
                handlers: Vec::new(),
            },
        ]);
        assert_eq!(
            cross.copy_text(),
            Err(super::SelectionCopyError::CaptureFailed)
        );
        cross.clear_all();
        assert!(matches!(
            super::capture_surface_text(
                &active,
                3,
                std::time::Instant::now() + std::time::Duration::from_secs(1)
            ),
            Err(super::SelectionCopyError::TooLarge { .. })
        ));
        assert_eq!(
            super::capture_surface_text(&active, 100, std::time::Instant::now()),
            Err(super::SelectionCopyError::TimedOut)
        );
        // A subsequent genuine native selection becomes the sole copy owner.
        active.select_all();
        pump();
        let native = cross.copy_text().unwrap().unwrap();
        assert!(native.contains("live-tail"));
        assert!(!native.contains("visible needle"));
        active.unselect_all();

        // Exercise the installed controller's cancellation path, not only
        // the endpoint helper: an evicted VTE must release a parked live feed.
        let evicted = vte4::Terminal::new();
        *cross.start_vte.borrow_mut() = Some(evicted.downgrade());
        cross.claimed.set(true);
        let flushed = Rc::new(RefCell::new(Vec::new()));
        let flushed_cb = flushed.clone();
        cross
            .feed_hold
            .set_flush(move |bytes| flushed_cb.borrow_mut().extend(bytes));
        cross.feed_hold.begin_drag();
        cross.clear_other_selections(Some(&active));
        assert!(
            cross.feed_hold.try_buffer(b"parked tail"),
            "the click observer cannot undo a new drag hold"
        );
        drop(evicted);
        let controllers = scroll.observe_controllers();
        let drag = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .find_map(|controller| controller.downcast::<gtk::GestureDrag>().ok())
            .expect("cross-selection drag controller is installed");
        drag.emit_by_name::<()>("drag-update", &[&0.0_f64, &0.0_f64]);
        pump();
        assert!(cross.start_vte.borrow().is_none());
        assert!(!cross.claimed.get());
        assert_eq!(&*flushed.borrow(), b"parked tail");

        window.close();
        pump();
    }
}
