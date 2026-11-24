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
//! phase and consumes motion after the pointer crosses a surface boundary.
//! The underlying widget owns per-cell selection within one surface.

use relm4::gtk;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use vte4::TerminalExt;

use super::selection_hold::{feed_hold_eligible, SelectionFeedHold};
#[cfg(anvil_shared_terminal)]
use super::shared_active::SharedActiveTerminal;
use super::text_surface::TerminalTextSurface;
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

impl SelectionWriter {
    fn new(limit: usize, deadline: std::time::Instant) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            deadline,
            failure: None,
        }
    }

    fn into_text(self, captured: bool) -> Result<String, SelectionCopyError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !captured {
            return Err(SelectionCopyError::CaptureFailed);
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(SelectionCopyError::TimedOut);
        }
        String::from_utf8(self.bytes).map_err(|_| SelectionCopyError::CaptureFailed)
    }
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
    let stream = gtk::gio::WriteOutputStream::new(SelectionWriter::new(max_bytes, deadline));
    let result = vte.write_contents_sync(
        &stream,
        vte4::WriteFlags::Default,
        gtk::gio::Cancellable::NONE,
    );
    let writer = stream
        .close_and_take()
        .downcast::<SelectionWriter>()
        .map_err(|_| SelectionCopyError::CaptureFailed)?;
    writer.into_text(result.is_ok())
}

struct SelectedSurface {
    widget: gtk::glib::WeakRef<gtk::Widget>,
    #[cfg(anvil_shared_terminal)]
    retained_text: Option<jterm_gtk_view::RetainedTextSource>,
    handlers: Vec<(
        gtk::glib::WeakRef<gtk::glib::Object>,
        gtk::glib::SignalHandlerId,
    )>,
}

impl Drop for SelectedSurface {
    fn drop(&mut self) {
        for (object, handler) in self.handlers.drain(..) {
            if let Some(object) = object.upgrade() {
                object.disconnect(handler);
            }
        }
        if let Some(widget) = self.widget.upgrade() {
            widget.remove_css_class("cross-block-text-selected");
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
    active_surface: TerminalTextSurface,
    #[cfg(anvil_shared_terminal)]
    shared_active: Option<Rc<SharedActiveTerminal>>,
    selected_block_ids: SelectedBlockIds,
    selected_block_id: Rc<Cell<Option<u64>>>,
    selection_anchor_id: Rc<Cell<Option<u64>>>,
    /// Stable identity of the drag origin. Virtualization and history eviction
    /// can change mapped-widget indices while a pointer drag is in progress.
    /// A weak reference does not retain an evicted terminal and its scrollback.
    start_vte: RefCell<Option<gtk::glib::WeakRef<gtk::Widget>>>,
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
    pub(super) fn install(
        block_scroll: &impl IsA<gtk::Widget>,
        finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
        active_surface: TerminalTextSurface,
        selected_block_ids: SelectedBlockIds,
        selected_block_id: Rc<Cell<Option<u64>>>,
        selection_anchor_id: Rc<Cell<Option<u64>>>,
        feed_hold: Rc<SelectionFeedHold>,
        bstate: Rc<Cell<BlockState>>,
        mouse_reporting: Rc<Cell<MouseReporting>>,
        #[cfg(anvil_shared_terminal)] shared_active: Option<Rc<SharedActiveTerminal>>,
    ) -> Rc<Self> {
        // Chat's live terminal is outside its history scroller. Mount the same
        // native pick-based owner on their common canvas; Block still passes
        // its original scroller. No synthetic chrome acquires a text origin.
        let block_scroll = block_scroll.upcast_ref::<gtk::Widget>();
        let this = Rc::new(Self {
            finished_blocks,
            active_surface,
            #[cfg(anvil_shared_terminal)]
            shared_active,
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
                if this.surface_selected(this.active_surface.widget()) {
                    this.clear_cross_selection();
                }
            }
        });

        // Native terminals and GtkTextView claim their own drag gestures.
        // Observe the pointer in capture so a claimed child gesture can
        // still cross into another surface.
        let drag = gtk::EventControllerLegacy::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let root = block_scroll.downgrade();
        let owner = Rc::downgrade(&this);
        drag.connect_event(move |_, event| {
            let (Some(owner), Some(root)) = (owner.upgrade(), root.upgrade()) else {
                return gtk::glib::Propagation::Proceed;
            };
            owner.track_drag(&root, event).into()
        });
        block_scroll.add_controller(drag);
        this
    }

    fn track_drag(self: &Rc<Self>, root: &gtk::Widget, event: &gtk::gdk::Event) -> bool {
        use gtk::gdk::{ButtonEvent, EventType, ModifierType, TouchEvent, BUTTON_PRIMARY};

        let state = event.modifier_state();
        let shift_held = state.contains(ModifierType::SHIFT_MASK);
        let primary = || {
            event
                .downcast_ref::<ButtonEvent>()
                .is_some_and(|event| event.button() == BUTTON_PRIMARY)
        };
        let pointer_touch = || {
            event
                .downcast_ref::<TouchEvent>()
                .is_some_and(TouchEvent::is_emulating_pointer)
        };
        let point = || event_point(root, event);
        match event.event_type() {
            EventType::ButtonPress if primary() => {
                if let Some((x, y)) = point() {
                    self.begin_drag(root, x, y, shift_held);
                }
            }
            EventType::TouchBegin if pointer_touch() => {
                if let Some((x, y)) = point() {
                    self.begin_drag(root, x, y, shift_held);
                }
            }
            EventType::MotionNotify if state.contains(ModifierType::BUTTON1_MASK) => {
                return point().is_some_and(|(x, y)| self.drag_to(root, x, y, shift_held));
            }
            EventType::TouchUpdate if pointer_touch() => {
                return point().is_some_and(|(x, y)| self.drag_to(root, x, y, shift_held));
            }
            EventType::ButtonRelease if primary() => self.finish_drag(),
            EventType::TouchEnd if pointer_touch() => self.finish_drag(),
            EventType::GrabBroken => self.finish_drag(),
            EventType::TouchCancel if pointer_touch() => self.finish_drag(),
            _ => {}
        }
        false
    }

    fn begin_drag(&self, root: &gtk::Widget, x: f64, y: f64, shift_held: bool) {
        self.drag_generation
            .set(self.drag_generation.get().wrapping_add(1));
        self.start_vte.borrow_mut().take();
        self.claimed.set(false);
        // A Chat command control captures the pressed command before
        // releasing any held output.
        if chat_command_control_at(root, x, y) {
            return;
        }
        let target = self.vte_at(root, x, y);
        if target.is_some() {
            self.clear_block_selection();
        }
        self.clear_other_selections(target.as_ref());
        let Some(start) = target else { return };
        if start == self.active_surface && self.mouse_reporting.get().is_tracking() && !shift_held {
            return;
        }
        self.start_vte.replace(Some(start.widget().downgrade()));
        self.maybe_hold_active_feed(Some(&start), shift_held);
    }

    fn drag_to(self: &Rc<Self>, root: &gtk::Widget, x: f64, y: f64, shift_held: bool) -> bool {
        let Some(origin) = self.start_vte.borrow().clone() else {
            return false;
        };
        let Some(start_vte) = origin.upgrade().and_then(TerminalTextSurface::from_widget) else {
            self.cancel_drag_selection();
            return false;
        };
        let Some(current_vte) = self.vte_at(root, x, y) else {
            return self.claimed.get();
        };
        let vtes = self.ordered_vtes();
        let Some((start, current)) = drag_surface_indices(&vtes, &start_vte, &current_vte) else {
            self.cancel_drag_selection();
            return false;
        };
        if current == start && !self.claimed.get() {
            return false;
        }
        self.claimed.set(true);
        self.paint_range(&vtes, start, current);
        self.maybe_hold_active_feed(vtes.get(start.max(current)), shift_held);
        true
    }

    fn finish_drag(self: &Rc<Self>) {
        if self.start_vte.borrow_mut().take().is_some() {
            // Let GTK finalize any native selection on release before
            // deciding whether to resume the held output.
            self.end_active_feed_hold();
        }
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
                this.active_surface.widget().is_mapped()
                    && this.active_surface.widget().is_visible()
                    && (this.active_surface.has_selection()
                        || this.surface_selected(this.active_surface.widget()))
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

    fn maybe_hold_active_feed(&self, surface: Option<&TerminalTextSurface>, shift_held: bool) {
        if surface == Some(&self.active_surface)
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
                this.active_surface.has_selection()
                    || this.surface_selected(this.active_surface.widget())
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
    fn all_vtes(&self) -> Vec<TerminalTextSurface> {
        let finished = self.finished_blocks.borrow();
        let mut vtes = Vec::with_capacity(finished.len().saturating_mul(2) + 1);
        for block in finished.iter() {
            vtes.extend(block.command_surface());
            vtes.push(block.output_surface());
        }
        vtes.push(self.active_surface.clone());
        vtes
    }

    /// Hidden, collapsed, or virtualized surfaces cannot contribute invisible
    /// text to cross-selection or copy.
    fn ordered_vtes(&self) -> Vec<TerminalTextSurface> {
        self.all_vtes()
            .into_iter()
            .filter(|surface| surface.widget().is_mapped() && surface.widget().is_visible())
            .collect()
    }

    fn clear_other_selections(&self, keep: Option<&TerminalTextSurface>) {
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
                vte.clear_selection();
            }
        }
    }

    fn vte_at(&self, block_scroll: &gtk::Widget, x: f64, y: f64) -> Option<TerminalTextSurface> {
        let picked = block_scroll.pick(x, y, gtk::PickFlags::DEFAULT)?;
        self.ordered_vtes()
            .into_iter()
            .find(|vte| widget_contains(vte.widget(), &picked))
    }

    fn surface_selected(&self, widget: &impl IsA<gtk::Widget>) -> bool {
        self.selected_surfaces
            .borrow()
            .iter()
            .any(|surface| surface.widget.upgrade().as_ref() == Some(widget.upcast_ref()))
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
    fn paint_range(self: &Rc<Self>, vtes: &[TerminalTextSurface], a: usize, b: usize) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let range = &vtes[lo.min(vtes.len())..hi.saturating_add(1).min(vtes.len())];
        if self.selected_surfaces.borrow().len() == range.len()
            && self
                .selected_surfaces
                .borrow()
                .iter()
                .zip(range)
                .all(|(selected, terminal)| {
                    selected.widget.upgrade().as_ref() == Some(terminal.widget())
                })
        {
            return;
        }
        self.clear_cross_selection();
        for vte in self.all_vtes() {
            vte.clear_selection();
        }
        let mut selected = Vec::new();
        for surface in range {
            let widget = surface.widget();
            widget.add_css_class("cross-block-text-selected");
            let weak = Rc::downgrade(self);
            let unmap = widget.connect_unmap(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.cancel_drag_selection();
                }
            });
            let mut handlers = vec![(widget.upcast_ref::<gtk::glib::Object>().downgrade(), unmap)];
            let weak = Rc::downgrade(self);
            let native_selection_started = move || {
                if let Some(this) = weak.upgrade() {
                    this.clear_cross_selection();
                    this.start_vte.borrow_mut().take();
                    this.claimed.set(false);
                }
            };
            match surface {
                TerminalTextSurface::Native(terminal) => {
                    let weak = Rc::downgrade(self);
                    let contents = terminal.connect_contents_changed(move |_| {
                        if let Some(this) = weak.upgrade() {
                            this.cancel_drag_selection();
                        }
                    });
                    let selection = terminal.connect_selection_changed(move |terminal| {
                        if terminal.has_selection() {
                            native_selection_started();
                        }
                    });
                    let object = terminal.upcast_ref::<gtk::glib::Object>().downgrade();
                    handlers.push((object.clone(), contents));
                    handlers.push((object, selection));
                }
                #[cfg(anvil_shared_terminal)]
                TerminalTextSurface::Document(text) => {
                    let buffer = text.buffer();
                    let weak = Rc::downgrade(self);
                    let contents = buffer.connect_changed(move |_| {
                        if let Some(this) = weak.upgrade() {
                            this.cancel_drag_selection();
                        }
                    });
                    handlers.push((
                        buffer.upcast_ref::<gtk::glib::Object>().downgrade(),
                        contents,
                    ));
                    let selection = buffer.connect_has_selection_notify(move |buffer| {
                        if buffer.has_selection() {
                            native_selection_started();
                        }
                    });
                    handlers.push((
                        buffer.upcast_ref::<gtk::glib::Object>().downgrade(),
                        selection,
                    ));
                }
            }
            selected.push(SelectedSurface {
                widget: widget.downgrade(),
                #[cfg(anvil_shared_terminal)]
                retained_text: self
                    .shared_active_for(surface)
                    .and_then(|terminal| terminal.embedded().retained_text_source()),
                handlers,
            });
        }
        *self.selected_surfaces.borrow_mut() = selected;
    }

    pub(crate) fn clear_all(&self) {
        // Navigation and whole-card selection replace the active drag too.
        // Retire its origin before unselecting or replaying held PTY bytes,
        // so a later motion cannot recreate the selection just cleared.
        self.start_vte.borrow_mut().take();
        self.claimed.set(false);
        self.clear_cross_selection();
        for vte in self.all_vtes() {
            vte.clear_selection();
        }
        self.feed_hold.flush_now();
    }

    fn selected_surface_snapshot(&self) -> Result<Vec<TerminalTextSurface>, SelectionCopyError> {
        let selected: Vec<_> = self
            .selected_surfaces
            .borrow()
            .iter()
            .map(|surface| {
                surface
                    .widget
                    .upgrade()
                    .and_then(TerminalTextSurface::from_widget)
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
                    self.capture_whole_surface(
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
                self.selected_text(&vte)
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

    fn selected_text(&self, surface: &TerminalTextSurface) -> Option<String> {
        #[cfg(anvil_shared_terminal)]
        if let Some(terminal) = self.shared_active_for(surface) {
            return terminal.embedded().pane().selected_text();
        }
        surface.selected_text()
    }

    #[cfg(anvil_shared_terminal)]
    fn shared_active_for(&self, surface: &TerminalTextSurface) -> Option<&SharedActiveTerminal> {
        self.shared_active
            .as_deref()
            .filter(|_| surface == &self.active_surface)
    }

    fn capture_whole_surface(
        &self,
        surface: &TerminalTextSurface,
        max_bytes: usize,
        deadline: std::time::Instant,
    ) -> Result<String, SelectionCopyError> {
        #[cfg(anvil_shared_terminal)]
        if let Some(terminal) = self.shared_active_for(surface) {
            let selections = self.selected_surfaces.borrow();
            let source = selections
                .iter()
                .find(|selected| selected.widget.upgrade().as_ref() == Some(surface.widget()))
                .and_then(|selected| selected.retained_text.as_ref())
                .ok_or(SelectionCopyError::CaptureFailed)?;
            let mut writer = SelectionWriter::new(max_bytes, deadline);
            let result = terminal.embedded().write_retained_text(source, &mut writer);
            return writer.into_text(result.is_ok());
        }
        match surface {
            TerminalTextSurface::Native(terminal) => {
                capture_surface_text(terminal, max_bytes, deadline)
            }
            #[cfg(anvil_shared_terminal)]
            TerminalTextSurface::Document(text) => {
                if std::time::Instant::now() >= deadline {
                    return Err(SelectionCopyError::TimedOut);
                }
                let buffer = text.buffer();
                let value = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
                if value.len() > max_bytes {
                    return Err(SelectionCopyError::TooLarge {
                        bytes: value.len(),
                        limit: max_bytes,
                    });
                }
                Ok(value.to_string())
            }
        }
    }

    pub(crate) fn has_text_selection(&self) -> bool {
        self.ordered_vtes()
            .into_iter()
            .any(|vte| vte.has_selection() || self.surface_selected(vte.widget()))
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

fn event_point(widget: &gtk::Widget, event: &gtk::gdk::Event) -> Option<(f64, f64)> {
    let (x, y) = event.position()?;
    let native = widget.native()?;
    if event.surface() != native.surface() {
        return None;
    }
    let (dx, dy) = native.surface_transform();
    native
        .dynamic_cast::<gtk::Widget>()
        .ok()?
        .compute_point(
            widget,
            &gtk::graphene::Point::new((x - dx) as f32, (y - dy) as f32),
        )
        .map(|point| (f64::from(point.x()), f64::from(point.y())))
}

// The command control captures its identity before its own feed release.
fn chat_command_control_at(root: &gtk::Widget, x: f64, y: f64) -> bool {
    if !root.has_css_class("chat-mode") {
        return false;
    }
    let mut picked = root.pick(x, y, gtk::PickFlags::DEFAULT);
    while let Some(widget) = picked {
        if widget.has_css_class("chat-command-control") {
            return true;
        }
        if widget == *root {
            break;
        }
        picked = widget.parent();
    }
    false
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
    use super::gtk;
    use super::{append_selected_text, drag_surface_indices, CrossSelection, TerminalTextSurface};

    #[cfg(anvil_shared_terminal)]
    #[test]
    #[ignore = "requires an isolated native GTK display and candidate environment"]
    fn shared_finished_command_preserves_search_and_cross_card_copy() {
        check_shared_finished_document(crate::config::TerminalMode::Block, false);
    }

    #[cfg(anvil_shared_terminal)]
    #[test]
    #[ignore = "requires an isolated native GTK display and candidate environment"]
    fn shared_finished_output_preserves_search_filter_and_cross_card_copy() {
        check_shared_finished_document(crate::config::TerminalMode::Block, true);
    }

    #[cfg(anvil_shared_terminal)]
    #[test]
    #[ignore = "requires an isolated native GTK display and candidate environment"]
    fn shared_finished_chat_preserves_capsule_search_filter_and_copy() {
        check_shared_finished_document(crate::config::TerminalMode::Chat, true);
    }

    #[cfg(anvil_shared_terminal)]
    #[test]
    #[ignore = "requires an isolated native GTK display and candidate environment"]
    fn shared_live_output_keeps_the_owned_shell_through_alternate_screen() {
        use super::super::find::{FindNavigationResult, FindSearchResult};
        use crate::config::{Config, TerminalMode};
        use gtk::prelude::*;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        gtk::init().expect("GTK display");
        for name in ["JTERM_SHARED_ACTIVE_TERMINAL", "JTERM_SHARED_TRANSCRIPT"] {
            assert!(std::env::var_os(name).is_none(), "test the default route");
        }
        crate::child_env::capture_inherited_environment().expect("test child environment");
        let shell = r#"
            stty -echo
            printf '\033]133;A\007$ \033]133;B\007'
            IFS= read -r start
            printf '\r\n\033]133;C;cmdline_url=live-witness\007shared live row\nsecond captured row\nwide 界 é\n'
            IFS= read -r fullscreen
            printf '\033[?1049hfull screen witness\n'
            IFS= read -r finish
            printf '\033[?1049l\033]133;D;0\007\033]133;A\007$ \033]133;B\007'
            IFS= read -r next
            printf '\r\n\033]133;C;cmdline_url=next\007same shell %s\n' "$next"
            IFS= read -r complete_next
            printf '\033]133;D;0\007\033]133;A\007$ \033]133;B\007'
            IFS= read -r history
            printf '\r\n\033]133;C;cmdline_url=history\007'
            seq 1 90
            printf '\r\narchived zones ready\r\n\033]133;D;0\007\033]133;A\007$ \033]133;B\007'
            IFS= read -r shutdown
        "#;
        let wait_for = |mode: TerminalMode, what: &str, condition: &dyn Fn() -> bool| {
            let context = gtk::glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_secs(6);
            while Instant::now() < deadline {
                while context.iteration(false) {}
                if condition() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            panic!("{mode:?}: timed out while waiting for {what}");
        };
        let argv = ["/bin/sh".into(), "-c".into(), shell.into()];
        for mode in [
            TerminalMode::Block,
            TerminalMode::Chat,
            TerminalMode::Unified,
        ] {
            let mut config = Config::safe_defaults();
            config.terminal_mode = mode;
            config.preserve_live_scrollback = false;
            let view = super::super::TermView::new(
                &config,
                &mode,
                &argv,
                None,
                false,
                None,
                Default::default(),
                "live-output-test",
                &[],
                &[],
            )
            .expect("spawn the production shared PTY");
            let terminal = view
                .active
                .borrow()
                .shared_active
                .clone()
                .expect("shared active renderer");
            let screen = terminal.screen().clone();
            let chrome = view.active.borrow().unified_chrome_surface.clone();
            let window = gtk::Window::builder()
                .default_width(920)
                .default_height(680)
                .child(&view.widget())
                .build();
            let buffer = screen.buffer();
            let text = || buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            let pid = view.pid_i32();
            let fd = view.pty_fd_i32();
            assert!(pid > 0 && fd >= 0);
            let same_surface = || {
                screen.is_mapped()
                    && screen.buffer() == buffer
                    && view.legacy_terminal().is_none()
                    && view
                        .active
                        .borrow()
                        .shared_active
                        .as_ref()
                        .is_some_and(|current| Rc::ptr_eq(current, &terminal))
                    && view.pid_i32() == pid
                    && view.pty_fd_i32() == fd
            };
            window.present();
            wait_for(mode, "shared prompt over the real PTY", &|| {
                same_surface()
                    && text().contains("$ ")
                    && view.bstate.get() == super::super::BlockState::AwaitingCommand
            });
            view.pty
                .try_write_bytes(b"start\r")
                .expect("send held command");
            wait_for(mode, "all live output rows", &|| {
                let output = text();
                same_surface()
                    && output.contains("shared live row")
                    && output.contains("second captured row")
                    && output.contains("wide 界 e\u{301}")
            });
            let (cols, rows) = terminal.grid_size();
            assert_eq!(view.grid_size(), (i64::from(cols), i64::from(rows)));
            assert!(matches!(
                view.find_in_blocks("shared (?:live) row|second captured row", true, true),
                FindSearchResult::Matches(p) if p.current == 1 && p.total == 2
            ));
            assert_eq!(
                current_shared_find_text(&screen).as_deref(),
                Some("shared live row")
            );
            assert!(
                matches!(view.find_next(), FindNavigationResult::Progress(p) if p.current == 2)
            );
            assert_eq!(
                current_shared_find_text(&screen).as_deref(),
                Some("second captured row")
            );
            assert!(!buffer.has_selection(), "Find must leave the PTY running");
            view.pty
                .try_write_bytes(b"fullscreen\r")
                .expect("enter alternate screen");
            wait_for(mode, "same shared surface in alternate screen", &|| {
                same_surface()
                    && view.bstate.get() == super::super::BlockState::AltScreen
                    && text().contains("full screen witness")
                    && !chrome.is_visible()
            });
            assert_eq!(view.find_next(), FindNavigationResult::Invalidated);
            assert!(matches!(
                view.find_in_blocks("full screen witness", false, true),
                FindSearchResult::Matches(p) if p.total == 1
            ));
            assert_eq!(
                current_shared_find_text(&screen).as_deref(),
                Some("full screen witness")
            );
            assert_eq!(
                view.find_in_blocks("shared live row", false, true),
                FindSearchResult::NoMatches
            );
            view.pty
                .try_write_bytes(b"finish\r")
                .expect("return to primary screen");
            wait_for(mode, "same PTY after the first command", &|| {
                same_surface()
                    && view.render_backend.records().len() == 1
                    && view.bstate.get() == super::super::BlockState::AwaitingCommand
            });
            let id = view.render_backend.records().iter().next().unwrap().id();
            if let Some(transcript) = terminal.transcript() {
                wait_for(mode, "completed live zone and native chrome", &|| {
                    chrome.is_mapped()
                        && transcript
                            .painted_zone_rows()
                            .iter()
                            .any(|row| row.id == id && row.heading && row.completed)
                });
                wait_for(mode, "native exit badge pixels", &|| {
                    transcript
                        .painted_zone_rows()
                        .iter()
                        .find(|row| row.id == id && row.heading)
                        .is_some_and(|row| {
                            chrome_exit_badge_has_pixels(
                                &window,
                                &screen,
                                &chrome,
                                terminal.cell_metrics(),
                                row.row,
                            )
                        })
                });
            }
            view.pty
                .try_write_bytes(b"continued\r")
                .expect("send the next command to the same shell");
            wait_for(
                mode,
                "next command output while the same shell waits",
                &|| same_surface() && text().contains("same shell continued"),
            );
            assert!(matches!(
                view.find_in_blocks("shared live row|same shell continued", true, true),
                FindSearchResult::Matches(p) if p.total == 2
            ));
            assert!(
                matches!(view.find_next(), FindNavigationResult::Progress(p) if p.current == 2)
            );
            assert_eq!(
                current_shared_find_text(&screen).as_deref(),
                Some("same shell continued")
            );
            view.pty.try_write_bytes(b"complete\r").unwrap();
            wait_for(mode, "same shell's next command", &|| {
                same_surface()
                    && view.render_backend.records().len() == 2
                    && view.bstate.get() == super::super::BlockState::AwaitingCommand
            });
            assert!(view
                .render_backend
                .records()
                .iter()
                .next_back()
                .and_then(|record| record.output())
                .is_some_and(|output| output.contains("same shell continued")));
            if !mode.is_unified() {
                view.select_all_blocks();
                assert!(view.copy_to_clipboard_with_modifier(true));
                let clipboard = window.clipboard();
                let copied = gtk::glib::MainContext::default()
                    .block_on(clipboard.read_text_future())
                    .expect("read copied outputs")
                    .expect("completed outputs on the clipboard");
                let first = copied
                    .find("shared live row")
                    .unwrap_or_else(|| panic!("{mode:?}: first output missing from {copied:?}"));
                let second = copied
                    .find("same shell continued")
                    .unwrap_or_else(|| panic!("{mode:?}: next output missing from {copied:?}"));
                assert!(first < second, "copied outputs preserve command order");
            }
            if let Some(transcript) = terminal.transcript() {
                view.pty.try_write_bytes(b"archive\r").unwrap();
                wait_for(mode, "the first zone rolls into retained history", &|| {
                    view.render_backend.records().len() == 3
                        && text().contains("archived zones ready")
                });
                assert!(matches!(
                    view.find_in_blocks("shared live row", false, true),
                    FindSearchResult::Matches(p) if p.total == 1
                ));
                assert_eq!(
                    current_shared_find_text(&screen).as_deref(),
                    Some("shared live row")
                );
                view.clear_find();
                assert!(transcript.reveal_zone(id));
                wait_for(
                    mode,
                    "archive chrome remains tied to the same zone",
                    &|| {
                        transcript
                            .painted_zone_rows()
                            .iter()
                            .any(|row| row.id == id && row.heading && row.completed)
                    },
                );
                let before = terminal.grid_size().0;
                window.set_default_size(610, 450);
                wait_for(mode, "a distinct terminal column layout", &|| {
                    terminal.grid_size().0 < before
                });
                assert!(transcript.reveal_zone(id));
                wait_for(
                    mode,
                    "the original zone and exit badge survive reflow",
                    &|| {
                        transcript
                            .painted_zone_rows()
                            .iter()
                            .find(|row| row.id == id && row.heading)
                            .is_some_and(|row| {
                                chrome_exit_badge_has_pixels(
                                    &window,
                                    &screen,
                                    &chrome,
                                    terminal.cell_metrics(),
                                    row.row,
                                )
                            })
                    },
                );
            }
            view.pty.kill();
            window.close();
        }
    }

    #[cfg(anvil_shared_terminal)]
    fn current_shared_find_text(screen: &gtk::TextView) -> Option<String> {
        use gtk::prelude::*;
        let buffer = screen.buffer();
        let tag = buffer.tag_table().lookup("jterm-screen-find-current")?;
        let mut start = buffer.start_iter();
        if !start.has_tag(&tag) && !start.forward_to_tag_toggle(Some(&tag)) {
            return None;
        }
        let mut end = start;
        end.forward_to_tag_toggle(Some(&tag));
        Some(buffer.text(&start, &end, true).to_string())
    }

    #[cfg(anvil_shared_terminal)]
    fn chrome_exit_badge_has_pixels(
        window: &gtk::Window,
        screen: &gtk::TextView,
        chrome: &gtk::DrawingArea,
        (_, top, _, cell_height): (i32, i32, i32, i32),
        row: usize,
    ) -> bool {
        use gtk::prelude::*;
        let (width, height) = (chrome.width(), chrome.height());
        if width < 60 || height < 24 || cell_height < 5 {
            return false;
        }
        let Some(origin) = screen.compute_point(chrome, &gtk::graphene::Point::new(0.0, 0.0))
        else {
            return false;
        };
        let y = origin.y().round() as i32 + top + row as i32 * cell_height;
        if y < 0 || y + cell_height > height {
            return false;
        }
        let snapshot = gtk::Snapshot::new();
        gtk::WidgetPaintable::new(Some(chrome)).snapshot(
            &snapshot,
            f64::from(width),
            f64::from(height),
        );
        let Some(node) = snapshot.to_node() else {
            return false;
        };
        let Some(renderer) = gtk::prelude::NativeExt::renderer(window) else {
            return false;
        };
        let viewport = gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
        let texture = renderer.render_texture(&node, Some(&viewport));
        let stride = width as usize * 4;
        let mut pixels = vec![0; stride * height as usize];
        texture.download(&mut pixels, stride);
        // The gutter and the single-pixel separator cannot light this band.
        ((y + 2)..(y + cell_height - 1)).any(|line| {
            let start = line as usize * stride + (width as usize - 58) * 4;
            pixels[start..start + 50 * 4].iter().any(|&byte| byte != 0)
        })
    }

    #[cfg(anvil_shared_terminal)]
    fn check_shared_finished_document(mode: crate::config::TerminalMode, document_output: bool) {
        use super::super::find::{CrossBlockSearchOptions, FindNavigationResult, FindSearchResult};
        use gtk::prelude::*;
        use std::time::{Duration, Instant};

        gtk::init().expect("GTK display");
        assert!(std::env::var_os("JTERM_SHARED_FINISHED_COMMANDS").is_none_or(|value| value == "1"));
        assert_eq!(
            std::env::var_os("JTERM_SHARED_FINISHED_OUTPUT").is_none_or(|value| value == "1"),
            document_output
        );
        let mut config = crate::config::Config::safe_defaults();
        config.terminal_mode = mode;
        crate::child_env::capture_inherited_environment().expect("test child environment");
        let view = super::super::TermView::new_with_spawner(
            &config,
            &mode,
            &["sh".into()],
            None,
            false,
            None,
            Default::default(),
            "finished-command-test",
            &[],
            &[],
            |_, _, _, _| crate::pty::OwnedPty::from_live_openpty(Some(true)),
        )
        .unwrap();
        view.pty.set_test_foreground_group(Some(101));
        let window = gtk::Window::builder()
            .default_width(920)
            .default_height(680)
            .child(&view.widget())
            .build();
        window.present();
        let settle = || {
            let main = gtk::glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_millis(350);
            while Instant::now() < deadline {
                while main.iteration(false) {}
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        settle();
        view.pty.emit_test_output(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        settle();
        view.pty.emit_test_output("echo needle needle\r\n\x1b]133;C;cmdline_url=echo%20needle%20needle\x07progress-before-rewrite\r\x1b[2K\x1b]8;;https://example.test/completed\x07\x1b[1;31mdocument-body\x1b[0m\x1b]8;;\x07\r\nrow two e\u{301} 👩‍💻\r\nrow three\r\nrow four\r\nrow five\r\n\x1b[38;2;33;77;121mfindbody marker\x1b[0m\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07".as_bytes());
        settle();
        view.pty.emit_test_output(b"echo link");
        settle();
        view.pty.emit_test_output(b"\r\n\x1b]133;C\x07https://example.test/mixed-output\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        settle();
        view.pty.emit_test_output(b"echo color\r\n\x1b]133;C;cmdline_url=echo%20color\x07\x1b[5mblinking text\x1b[0m\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        settle();
        view.pty.emit_test_output(b"echo https://example.test\r\n./src/file.rs\r\n\x1b]133;C;cmdline_url=echo%20https%3A%2F%2Fexample.test%0A.%2Fsrc%2Ffile.rs\x07ordinary text\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        settle();
        let (card_id, command, output, second) = {
            let finished = view.finished_blocks.borrow();
            assert_eq!(finished.len(), 4);
            assert!(finished[1].command_vte.is_none());
            let captured = finished[1].cmd_text.as_str();
            assert_eq!(captured.strip_prefix("$ ").unwrap_or(captured), "echo link");
            assert_eq!(
                finished[1]
                    .command_surface()
                    .unwrap()
                    .document_text()
                    .as_deref(),
                Some(captured)
            );
            let records = view.block_data.borrow();
            let record = &records[1];
            assert_eq!(
                super::super::failed_block_retry_disabled_reason(
                    false,
                    &record.cmd,
                    record.command_exact,
                    record.command_truncated,
                    None,
                    None,
                    None,
                ),
                Some("The shell did not provide exact command metadata")
            );
            assert!(finished[3].command_vte.is_none());
            let multiline = finished[3]
                .command_document
                .as_ref()
                .expect("shared command with a URL and a path")
                .widget()
                .buffer();
            assert_eq!(
                multiline.text(&multiline.start_iter(), &multiline.end_iter(), false),
                "echo https://example.test\n./src/file.rs"
            );
            assert!(
                finished[2].output_vte.is_some(),
                "unsupported blinking output stays on VTE"
            );
            assert_eq!(finished[1].output_vte.is_none(), document_output);
            let first = &finished[0];
            if document_output {
                let capture = first.full_output.borrow();
                let archived = jterm_core::screen_replay::project_finished_output(&capture, 80)
                    .expect("completed output retains its styled document projection");
                assert_eq!(archived.links.len(), 1);
                let link = &archived.links[0];
                assert_eq!(link.target, "https://example.test/completed");
                assert_eq!(&archived.text[link.bytes.clone()], "document-body");
                assert!(!archived.text.contains("progress-before-rewrite"));
            }
            assert!(first.command_vte.is_none());
            assert_eq!(first.output_vte.is_none(), document_output);
            let command = first
                .command_document
                .as_ref()
                .expect("shared command view")
                .widget()
                .clone();
            let second = finished[1].output_surface();
            (first.id, command, first.output_surface(), second)
        };
        assert!(command.is_mapped());
        assert!(output.widget().is_mapped());
        if mode == crate::config::TerminalMode::Chat {
            assert!(command.width() > 0);
            assert!(output.widget().width() > command.width());
        }
        assert_eq!(output.native().is_none(), document_output);
        assert!(second.widget().is_mapped());
        if document_output {
            assert_eq!(
                second.document_text().as_deref(),
                Some("https://example.test/mixed-output")
            );
        }
        assert!(!command.is_editable());
        let buffer = command.buffer();
        assert_eq!(
            buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
            "echo needle needle"
        );
        let selection = || {
            buffer
                .selection_bounds()
                .map(|(start, end)| (start.offset(), end.offset()))
        };
        assert!(
            matches!(view.find_in_blocks(r"nee(?:dle)", true, true), FindSearchResult::Matches(p) if p.total == 2)
        );
        assert_eq!(selection(), Some((5, 11)));
        assert!(matches!(view.find_next(), FindNavigationResult::Progress(p) if p.current == 2));
        assert_eq!(selection(), Some((12, 18)));
        assert!(matches!(view.find_prev(), FindNavigationResult::Progress(p) if p.current == 1));
        assert_eq!(selection(), Some((5, 11)));
        assert!(view.focus_match_in_block(
            card_id,
            "needle",
            CrossBlockSearchOptions::default(),
            false,
            1
        ));
        assert_eq!(selection(), Some((12, 18)));
        view.copy_to_clipboard();
        let copied = gtk::glib::MainContext::default()
            .block_on(command.clipboard().read_text_future())
            .unwrap()
            .unwrap();
        assert_eq!(copied, "needle");
        view.clear_find();
        assert!(selection().is_none());

        if let TerminalTextSurface::Document(output) = &output {
            assert_eq!(
                TerminalTextSurface::Document(output.clone())
                    .document_text()
                    .as_deref(),
                Some("document-body\nrow two e\u{301} 👩‍💻\nrow three\nrow four\nrow five\nfindbody marker")
            );
            assert!(!output.is_editable());
            assert!(view.set_block_collapsed(card_id, true));
            settle();
            assert!(!output.is_mapped());
            assert!(
                matches!(view.find_in_blocks("findbody", false, true), FindSearchResult::Matches(p) if p.total == 1)
            );
            settle();
            assert!(output.is_mapped());
            let capture = output.buffer();
            assert!(capture
                .start_iter()
                .tags()
                .iter()
                .any(|tag| tag.weight() == 700));
            let (start, end) = capture.selection_bounds().expect("exact output hit");
            let bold_red = view.config.borrow().palette[9];
            assert!(capture
                .start_iter()
                .tags()
                .iter()
                .any(|tag| tag.foreground_rgba() == Some(bold_red)));
            let rgb = gtk::gdk::RGBA::new(33.0 / 255.0, 77.0 / 255.0, 121.0 / 255.0, 1.0);
            let has_color = |iter: &gtk::TextIter| {
                iter.tags()
                    .iter()
                    .any(|tag| tag.foreground_rgba() == Some(rgb))
            };
            assert!(
                has_color(&start),
                "retained truecolor must reach the GTK tag"
            );
            assert_eq!(capture.text(&start, &end, true), "findbody");
            view.copy_to_clipboard();
            let copied = gtk::glib::MainContext::default()
                .block_on(output.clipboard().read_text_future())
                .unwrap()
                .unwrap();
            assert_eq!(copied, "findbody");
            view.clear_find();

            let toggle = view.finished_blocks.borrow()[0].toggle_filter.clone();
            toggle();
            settle();
            fn find_filter(widget: &gtk::Widget) -> Option<gtk::SearchEntry> {
                if let Ok(entry) = widget.clone().downcast::<gtk::SearchEntry>() {
                    return Some(entry);
                }
                let mut child = widget.first_child();
                while let Some(node) = child {
                    if let Some(found) = find_filter(&node) {
                        return Some(found);
                    }
                    child = node.next_sibling();
                }
                None
            }
            let card = view.finished_blocks.borrow()[0].widget().clone();
            let filter = find_filter(card.upcast_ref()).expect("output filter");
            filter.set_text("marker");
            settle();
            assert_eq!(
                output
                    .buffer()
                    .text(&capture.start_iter(), &capture.end_iter(), true),
                "findbody marker"
            );
            assert!(has_color(&capture.start_iter()), "filter retains color");
            assert!(view.finished_blocks.borrow()[0]
                .full_output
                .borrow()
                .contains("document-body"));
            assert!(!view.finished_blocks.borrow()[0].is_output_collapsed());
            toggle();
            settle();
            assert_eq!(
                output
                    .buffer()
                    .text(&capture.start_iter(), &capture.end_iter(), true),
                "document-body\nrow two e\u{301} 👩‍💻\nrow three\nrow four\nrow five\nfindbody marker"
            );
        }

        if document_output {
            assert!(matches!(
                view.find_in_blocks("mixed-output", false, true),
                FindSearchResult::Matches(progress) if progress.total == 1
            ));
            settle();
            view.copy_to_clipboard();
            let copied = gtk::glib::MainContext::default()
                .block_on(second.widget().clipboard().read_text_future())
                .unwrap()
                .unwrap();
            assert_eq!(copied, "mixed-output");
            view.clear_find();
        }
        assert!(matches!(
            view.find_in_blocks("echo link", false, true),
            FindSearchResult::Matches(progress) if progress.total == 1
        ));
        settle();
        view.copy_to_clipboard();
        let copied = gtk::glib::MainContext::default()
            .block_on(window.clipboard().read_text_future())
            .unwrap()
            .unwrap();
        assert_eq!(copied, "echo link");
        view.clear_find();

        let cross = &view.cross_selection;
        let surfaces = cross.ordered_vtes();
        let first = TerminalTextSurface::Document(command);
        let (start, end) = drag_surface_indices(&surfaces, &first, &second).unwrap();
        cross.paint_range(&surfaces, start, end);
        let copied = cross.copy_text().unwrap().unwrap();
        assert!(copied.starts_with("echo needle needle\n"), "{copied:?}");
        let body = copied
            .find("document-body")
            .expect("selected first card output");
        let inferred_command = copied
            .find("echo link")
            .unwrap_or_else(|| panic!("captured command missing from {copied:?}"));
        let following = copied
            .find("https://example.test/mixed-output")
            .expect("selected next card output");
        assert!(
            body < inferred_command && inferred_command < following,
            "{copied:?}"
        );
        if document_output {
            assert_eq!(view.clear_blocks(), 4);
            assert_eq!(view.undo_clear_blocks(), 4);
            settle();
            let finished = view.finished_blocks.borrow();
            assert!(finished[0].command_document.is_some());
            assert!(finished[1].command_document.is_some());
            assert!(!view.block_data.borrow()[1].command_exact);
            assert_eq!(
                finished[1].output_surface().document_text().as_deref(),
                Some("https://example.test/mixed-output")
            );
            assert!(finished[3].command_document.is_some());
            drop(finished);
            assert!(matches!(
                view.find_in_blocks("findbody marker", false, true),
                FindSearchResult::Matches(progress) if progress.total == 1
            ));
        }
        let tail = "long-tail-witness";
        let long_command = format!("echo {} {tail}", "0123456789".repeat(18));
        let encoded = long_command.replace(' ', "%20");
        view.pty.emit_test_output(format!(
            "{long_command}\r\n\x1b]133;C;cmdline_url={encoded}\x07short output\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07"
        ).as_bytes());
        settle();
        let document = {
            let blocks = view.finished_blocks.borrow();
            assert_eq!(blocks.len(), 5);
            let block = &blocks[4];
            assert!(block.command_vte.is_none());
            block.command_document.as_ref().unwrap().widget().clone()
        };
        assert!(document.is_mapped());
        let buffer = document.buffer();
        let (start, end) = buffer.bounds();
        assert_eq!(buffer.text(&start, &end, true), long_command);
        assert!(document.iter_location(&end).y() > document.iter_location(&start).y());
        assert!(matches!(
            view.find_in_blocks(tail, false, true),
            FindSearchResult::Matches(p) if p.total == 1
        ));
        let (start, end) = buffer.selection_bounds().expect("selected command tail");
        assert_eq!(buffer.text(&start, &end, true), tail);
        if document_output {
            let body = "archived output line before the last row\r\n".repeat(96);
            view.pty
                .emit_test_output(b"echo archive\r\n\x1b]133;C;cmdline_url=echo%20archive\x07");
            view.pty.emit_test_output(body.as_bytes());
            view.pty.emit_test_output(
                b"\x1b[32marchive-tail-witness\x1b[0m\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07",
            );
            settle();
            let (output, expand, jump, copy) = {
                let blocks = view.finished_blocks.borrow();
                assert_eq!(blocks.len(), 6);
                let block = &blocks[5];
                assert!(block.output_vte.is_none());
                let output = block.output_document.clone().expect("shared tall output");
                (
                    output,
                    block.expand_btn.clone(),
                    block.jump_bottom_btn.clone(),
                    block.copy_output_btn.clone(),
                )
            };
            let adjustment = output.adjustment();
            let height = adjustment.page_size();
            assert!(height > 0.0 && adjustment.upper() > height * 2.0);
            let buffer = output.text().widget().buffer();
            assert_eq!(buffer.line_count(), 97);
            copy.emit_clicked();
            let copied = gtk::glib::MainContext::default()
                .block_on(window.clipboard().read_text_future())
                .expect("read archived output")
                .expect("copied output text");
            let expected = format!("{}archive-tail-witness", body.replace("\r\n", "\n"));
            assert_eq!(
                copied.lines().collect::<Vec<_>>(),
                expected.lines().collect::<Vec<_>>()
            );
            assert!(matches!(
                view.find_in_blocks("archive-tail-witness", false, true),
                FindSearchResult::Matches(p) if p.total == 1
            ));
            settle();
            let (start, end) = buffer.selection_bounds().expect("selected output tail");
            assert_eq!(buffer.text(&start, &end, true), "archive-tail-witness");
            assert!(adjustment.value() > 0.0);
            adjustment.set_value(0.0);
            jump.emit_clicked();
            assert_eq!(adjustment.value(), adjustment.upper() - height);
            expand.emit_clicked();
            settle();
            assert!(adjustment.page_size() > height);
        }
        let mut retained = view.block_data.borrow()[1].clone();
        retained.command_truncated = true;
        retained.cmd_markup = Some(format!("\x1b[38;2;33;77;121m{}\x1b[0m", retained.cmd));
        let config = view.config.borrow().clone();
        let restored = super::super::FinishedBlock::from_retained(&retained, &config, 80, None);
        assert!(restored.command_vte.is_none());
        let document = restored
            .command_document
            .as_ref()
            .expect("matching archived command");
        let buffer = document.widget().buffer();
        assert_eq!(
            buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
            retained.cmd
        );
        assert!(!document.widget().is_editable());
        let color = gtk::gdk::RGBA::new(33.0 / 255.0, 77.0 / 255.0, 121.0 / 255.0, 1.0);
        assert!(buffer
            .start_iter()
            .tags()
            .iter()
            .any(|tag| tag.foreground_rgba() == Some(color)));

        retained.cmd_markup = Some("\x1b[32ma different title\x1b[0m".into());
        let fallback = super::super::FinishedBlock::from_retained(&retained, &config, 80, None);
        assert!(fallback.command_vte.is_some());
        assert_eq!(fallback.output_vte.is_none(), document_output);
        if document_output {
            let (mut retained, mut record_ids) = {
                let records = view.block_data.borrow();
                (
                    records.front().unwrap().clone(),
                    records.iter().map(|block| block.id).collect::<Vec<_>>(),
                )
            };
            retained.id = record_ids.iter().max().unwrap() + 1;
            let restored_id = retained.id;
            retained.cmd = "printf\tretained".into();
            retained.cmd_markup = Some("\x1b[7mprintf\x1b[27m\tretained".into());
            retained.output = "discarded sample\r\x1b[Kretained\tvalue".into();
            record_ids.push(restored_id);
            retained.output_head_dropped = true;
            *view.deleted_order.borrow_mut() = record_ids;
            view.deleted_stash
                .borrow_mut()
                .push((view.block_data.borrow().len(), retained));
            assert_eq!(view.undo_deleted_blocks(), 1);
            settle();
            let finished = view.finished_blocks.borrow();
            let restored = finished
                .iter()
                .find(|block| block.id == restored_id)
                .unwrap();
            assert!(restored.command_vte.is_none());
            assert!(restored.output_vte.is_none());
            assert_eq!(
                restored
                    .command_surface()
                    .unwrap()
                    .document_text()
                    .as_deref(),
                Some("printf\tretained")
            );
            assert_eq!(
                restored.output_surface().document_text().as_deref(),
                Some("retained\tvalue")
            );
            assert!(restored.output_head_dropped());
        }
        window.close();
    }

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

    fn assert_explicit_clear_retires_drag(
        cross: &std::rc::Rc<CrossSelection>,
        active: &vte4::Terminal,
    ) {
        use super::gtk::prelude::*;

        *cross.start_vte.borrow_mut() = Some(active.upcast_ref::<gtk::Widget>().downgrade());
        cross.claimed.set(true);
        let weak = std::rc::Rc::downgrade(cross);
        let replayed = std::rc::Rc::new(std::cell::Cell::new(0));
        let replayed_cb = replayed.clone();
        cross.feed_hold.set_flush(move |bytes| {
            let cross = weak.upgrade().expect("selection owner is still alive");
            assert!(cross.start_vte.borrow().is_none());
            assert!(!cross.claimed.get());
            assert_eq!(bytes, b"explicit clear tail");
            replayed_cb.set(replayed_cb.get() + 1);
        });
        cross.feed_hold.begin_drag();
        assert!(cross.feed_hold.try_buffer(b"explicit clear tail"));
        cross.clear_all();
        assert_eq!(replayed.get(), 1);
        assert!(cross.start_vte.borrow().is_none());
        assert!(!cross.claimed.get());
        // A duplicate clear is harmless and must not replay anything twice.
        cross.clear_all();
        assert_eq!(replayed.get(), 1);
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
            TerminalTextSurface::Native(active.clone()),
            selected,
            Rc::new(Cell::new(Some(41))),
            Rc::new(Cell::new(Some(41))),
            SelectionFeedHold::new(),
            Rc::new(Cell::new(BlockState::CollectingOutput)),
            Rc::new(Cell::new(MouseReporting::OFF)),
            #[cfg(anvil_shared_terminal)]
            None,
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
            let start = card.output_vte_for_test().compute_bounds(&window).unwrap();
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
                    cross.surface_selected(card.output_vte_for_test()),
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

        card.output_vte_for_test().select_all();
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
        *cross.start_vte.borrow_mut() = Some(
            card.output_vte_for_test()
                .upcast_ref::<gtk::Widget>()
                .downgrade(),
        );
        let before = cross.ordered_vtes();
        let before_indices = drag_surface_indices(
            &before,
            &TerminalTextSurface::Native(card.output_vte_for_test().clone()),
            &TerminalTextSurface::Native(active.clone()),
        )
        .expect("both drag endpoints are initially mapped");
        card.command_vte_for_test().set_visible(false);
        // Hiding command chrome queues a geometry refit and a VTE re-feed.
        // Let that complete before exercising selection on the new layout.
        settle();
        let after = cross.ordered_vtes();
        let anchor = cross
            .start_vte
            .borrow()
            .as_ref()
            .and_then(|origin| origin.upgrade())
            .and_then(TerminalTextSurface::from_widget)
            .expect("origin still exists");
        let after_indices = drag_surface_indices(
            &after,
            &anchor,
            &TerminalTextSurface::Native(active.clone()),
        )
        .expect("hiding an earlier surface must preserve the endpoints");
        assert_eq!(after_indices.0 + 1, before_indices.0);
        let primary = active.primary_clipboard();
        let clipboard = active.clipboard();
        primary.set_text("primary sentinel");
        clipboard.set_text("clipboard sentinel");
        cross.paint_range(&after, after_indices.0, after_indices.1);
        settle();
        assert!(cross.surface_selected(card.output_vte_for_test()));
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
        assert!(!card.command_vte_for_test().has_selection());

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
            card.output_vte_for_test().set_visible(true);
            settle();
            cross.paint_range(&after, after_indices.0, after_indices.1);
            flushed.borrow_mut().clear();
            reentry.set(false);
            cross.feed_hold.begin_drag();
            assert!(cross.feed_hold.try_buffer(b"borrowed history tail"));
            cross.feed_hold.end_drag(true);
            if mutable_borrow {
                let _borrowed_history = cross.finished_blocks.borrow_mut();
                card.output_vte_for_test().set_visible(false);
                assert!(
                    flushed.borrow().is_empty(),
                    "unmap under mutable history borrow must defer replay"
                );
            } else {
                let _borrowed_history = cross.finished_blocks.borrow();
                card.output_vte_for_test().set_visible(false);
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
            drag_surface_indices(
                &cross.ordered_vtes(),
                &anchor,
                &TerminalTextSurface::Native(active.clone())
            ),
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
                widget: card
                    .output_vte_for_test()
                    .upcast_ref::<gtk::Widget>()
                    .downgrade(),
                #[cfg(anvil_shared_terminal)]
                retained_text: None,
                handlers: Vec::new(),
            },
            super::SelectedSurface {
                widget: active.upcast_ref::<gtk::Widget>().downgrade(),
                #[cfg(anvil_shared_terminal)]
                retained_text: None,
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
        // Hiding output above queues allocation and VTE geometry work. A
        // nonblocking pump may finish before the frame clock dispatches it;
        // that late resize can correctly invalidate a newly created selection.
        // Settle the fixture before creating one genuine native selection,
        // then require it to survive another layout interval unchanged.
        let geometry = || {
            (
                active.width(),
                active.height(),
                active.row_count(),
                active.column_count(),
            )
        };
        let before_layout = geometry();
        settle();
        let settled_layout = geometry();
        assert!(active.is_mapped());
        active.select_all();
        settle();
        let selected_layout = geometry();
        eprintln!("native selection geometry before={before_layout:?} settled={settled_layout:?} selected={selected_layout:?} mapped={} selected={}", active.is_mapped(), active.has_selection());
        assert_eq!(
            selected_layout, settled_layout,
            "fixture layout must stay settled after the single selection"
        );
        assert!(
            active.has_selection(),
            "one native select_all must survive settled layout"
        );
        let native = cross.copy_text().unwrap().unwrap();
        assert!(native.contains("live-tail"));
        assert!(!native.contains("visible needle"));
        active.unselect_all();

        // Exercise the installed controller's cancellation path, not only
        // the endpoint helper: an evicted VTE must release a parked live feed.
        let evicted = vte4::Terminal::new();
        *cross.start_vte.borrow_mut() = Some(evicted.upcast_ref::<gtk::Widget>().downgrade());
        cross.claimed.set(true);
        let flushed = Rc::new(RefCell::new(Vec::new()));
        let flushed_cb = flushed.clone();
        cross
            .feed_hold
            .set_flush(move |bytes| flushed_cb.borrow_mut().extend(bytes));
        cross.feed_hold.begin_drag();
        cross.clear_other_selections(Some(&TerminalTextSurface::Native(active.clone())));
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

        // An explicit selection replacement can run while the mouse is
        // still down (for example Ctrl+Up navigation). It must retire the
        // old gesture before synchronous feed replay and later drag updates.
        assert_explicit_clear_retires_drag(&cross, &active);

        window.close();
        pump();
    }
}
