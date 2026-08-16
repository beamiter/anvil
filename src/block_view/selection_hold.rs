//! Feed hold that keeps a live-VTE text selection alive while output streams.
//!
//! VTE clears a selection when the cells beneath it are repainted. Interactive
//! commands, progress displays, and other live output can repaint many times a
//! second, so selection over the running surface needs to defer the PTY feed.
//! The hold lasts as long as the selection itself; copy, input, clearing, or
//! the bounded safety cap resumes it. Parked bytes are replayed through the
//! same parser/state-machine pipeline in arrival order; display is delayed,
//! never discarded or reordered.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use vte4::TerminalExt;

use super::{BlockState, MouseReportingMode};

type FlushFn = Box<dyn Fn(Vec<u8>)>;
type StateFn = Box<dyn Fn(bool)>;

/// A firehose must not let selection parking grow memory without bound or
/// stall the Block state machine indefinitely.
const MAX_PARKED_BYTES: usize = 2 * 1024 * 1024;

/// Only streaming states can destroy a live selection. When mouse reporting
/// is active the child owns the drag, unless Shift forces VTE local selection.
pub(crate) fn feed_hold_eligible(
    state: BlockState,
    mouse: MouseReportingMode,
    shift_held: bool,
) -> bool {
    let streaming = matches!(
        state,
        BlockState::CollectingOutput
            | BlockState::PostCommand
            | BlockState::AltScreen
            | BlockState::RawFallback
    );
    streaming && (mouse == MouseReportingMode::None || shift_held)
}

pub(crate) struct SelectionFeedHold {
    holding: Cell<bool>,
    /// VTE clears an old selection on press before growing the new one. Do not
    /// treat that mid-drag selection-changed signal as an instruction to flush.
    dragging: Cell<bool>,
    parked: RefCell<Vec<u8>>,
    flush_cb: RefCell<Option<FlushFn>>,
    /// The indicator changes only once bytes are really parked, so a click
    /// without output never flashes a false paused state.
    state_cb: RefCell<Option<StateFn>>,
}

impl SelectionFeedHold {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            holding: Cell::new(false),
            dragging: Cell::new(false),
            parked: RefCell::new(Vec::new()),
            flush_cb: RefCell::new(None),
            state_cb: RefCell::new(None),
        })
    }

    pub(crate) fn set_flush(&self, flush: impl Fn(Vec<u8>) + 'static) {
        *self.flush_cb.borrow_mut() = Some(Box::new(flush));
    }

    pub(crate) fn set_state_listener(&self, listener: impl Fn(bool) + 'static) {
        *self.state_cb.borrow_mut() = Some(Box::new(listener));
    }

    fn notify_state(&self, parked: bool) {
        if let Some(callback) = self.state_cb.borrow().as_ref() {
            callback(parked);
        }
    }

    /// Selection clearing and keyboard input are early-release triggers on the
    /// live VTE itself. Explicit input paths also flush before writing.
    pub(crate) fn install_vte_hooks(self: &Rc<Self>, vte: &vte4::Terminal) {
        let weak = Rc::downgrade(self);
        vte.connect_selection_changed(move |vte| {
            if let Some(hold) = weak.upgrade() {
                if !vte.has_selection() {
                    hold.selection_cleared();
                }
            }
        });

        let weak = Rc::downgrade(self);
        vte.connect_commit(move |_, _, _| {
            if let Some(hold) = weak.upgrade() {
                hold.flush_now();
            }
        });
    }

    pub(crate) fn begin_drag(&self) {
        self.dragging.set(true);
        self.holding.set(true);
    }

    pub(crate) fn end_drag(self: &Rc<Self>, selection_alive: bool) {
        if !self.dragging.replace(false) || !self.holding.get() {
            return;
        }
        if !selection_alive {
            self.flush_now();
        }
    }

    /// Returns true when the caller must skip normal processing. Overflow has
    /// already replayed the buffered bytes, including this chunk, in order.
    pub(crate) fn try_buffer(&self, data: &[u8]) -> bool {
        if !self.holding.get() {
            return false;
        }
        let (first_park, overflow) = {
            let mut parked = self.parked.borrow_mut();
            let was_empty = parked.is_empty();
            parked.extend_from_slice(data);
            (
                was_empty && !parked.is_empty(),
                parked.len() > MAX_PARKED_BYTES,
            )
        };
        if first_park {
            self.notify_state(true);
        }
        if overflow {
            self.flush_now();
        }
        true
    }

    fn selection_cleared(&self) {
        if self.holding.get() && !self.dragging.get() {
            self.flush_now();
        }
    }

    pub(crate) fn flush_now(&self) {
        if !self.holding.replace(false) {
            return;
        }
        let parked = std::mem::take(&mut *self.parked.borrow_mut());
        if parked.is_empty() {
            return;
        }
        self.notify_state(false);
        if let Some(flush) = self.flush_cb.borrow().as_ref() {
            flush(parked);
        }
    }

    /// Run an irreversible follow-up only after parked bytes have re-entered
    /// the normal pipeline. PTY exit and direct control writes use this helper
    /// so teardown/input cannot overtake the stream tail.
    pub(crate) fn flush_then(&self, action: impl FnOnce()) {
        self.flush_now();
        action();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::{feed_hold_eligible, SelectionFeedHold, MAX_PARKED_BYTES};
    use crate::block_view::{BlockState, MouseReportingMode};

    type FlushLog = Rc<RefCell<Vec<Vec<u8>>>>;

    fn hold_with_log() -> (Rc<SelectionFeedHold>, FlushLog) {
        let hold = SelectionFeedHold::new();
        let log: FlushLog = Rc::new(RefCell::new(Vec::new()));
        let log_for_flush = log.clone();
        hold.set_flush(move |bytes| log_for_flush.borrow_mut().push(bytes));
        (hold, log)
    }

    #[test]
    fn eligibility_requires_streaming_or_shift_for_mouse_reporting() {
        assert!(feed_hold_eligible(
            BlockState::CollectingOutput,
            MouseReportingMode::None,
            false
        ));
        assert!(feed_hold_eligible(
            BlockState::AltScreen,
            MouseReportingMode::None,
            false
        ));
        assert!(feed_hold_eligible(
            BlockState::RawFallback,
            MouseReportingMode::None,
            false
        ));
        assert!(!feed_hold_eligible(
            BlockState::Idle,
            MouseReportingMode::None,
            false
        ));
        assert!(!feed_hold_eligible(
            BlockState::AwaitingCommand,
            MouseReportingMode::None,
            false
        ));
        assert!(!feed_hold_eligible(
            BlockState::CollectingOutput,
            MouseReportingMode::Sgr,
            false
        ));
        assert!(feed_hold_eligible(
            BlockState::CollectingOutput,
            MouseReportingMode::Sgr,
            true
        ));
        assert!(!feed_hold_eligible(
            BlockState::Idle,
            MouseReportingMode::Sgr,
            true
        ));
    }

    #[test]
    fn parks_bytes_only_while_holding() {
        let (hold, log) = hold_with_log();
        assert!(!hold.try_buffer(b"live"));
        hold.begin_drag();
        assert!(hold.try_buffer(b"parked"));
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn drag_end_without_selection_flushes_in_order() {
        let (hold, log) = hold_with_log();
        hold.begin_drag();
        assert!(hold.try_buffer(b"one "));
        assert!(hold.try_buffer(b"two"));
        hold.end_drag(false);
        assert_eq!(log.borrow().as_slice(), [b"one two".to_vec()]);
        assert!(!hold.try_buffer(b"after"));
    }

    #[test]
    fn surviving_selection_keeps_feed_parked_until_copy_or_input() {
        let (hold, log) = hold_with_log();
        hold.begin_drag();
        assert!(hold.try_buffer(b"codex redraw"));
        hold.end_drag(true);
        assert!(hold.try_buffer(b" stays parked"));
        assert!(log.borrow().is_empty());
        hold.flush_now();
        assert_eq!(
            log.borrow().as_slice(),
            [b"codex redraw stays parked".to_vec()]
        );
    }

    #[test]
    fn selection_cleared_flushes_only_after_drag() {
        let (hold, log) = hold_with_log();
        hold.begin_drag();
        assert!(hold.try_buffer(b"kept"));
        hold.selection_cleared();
        assert!(log.borrow().is_empty());
        hold.dragging.set(false);
        hold.selection_cleared();
        assert_eq!(log.borrow().as_slice(), [b"kept".to_vec()]);
    }

    #[test]
    fn overflow_releases_hold_immediately() {
        let (hold, log) = hold_with_log();
        hold.begin_drag();
        let big = vec![b'x'; MAX_PARKED_BYTES];
        assert!(hold.try_buffer(&big));
        assert!(log.borrow().is_empty());
        assert!(hold.try_buffer(b"y"));
        assert_eq!(log.borrow().len(), 1);
        assert_eq!(log.borrow()[0].len(), MAX_PARKED_BYTES + 1);
        assert!(!hold.try_buffer(b"live"));
    }

    #[test]
    fn ended_drag_without_hold_is_noop() {
        let (hold, log) = hold_with_log();
        hold.end_drag(true);
        hold.flush_now();
        assert!(log.borrow().is_empty());
        assert!(!hold.try_buffer(b"live"));
    }

    #[test]
    fn followup_action_runs_after_parked_bytes_replay() {
        let hold = SelectionFeedHold::new();
        let order = Rc::new(RefCell::new(Vec::new()));
        let replay_order = order.clone();
        hold.set_flush(move |_| replay_order.borrow_mut().push("replay"));
        hold.begin_drag();
        assert!(hold.try_buffer(b"tail"));

        let action_order = order.clone();
        hold.flush_then(move || action_order.borrow_mut().push("exit-or-control"));
        assert_eq!(order.borrow().as_slice(), ["replay", "exit-or-control"]);
    }
}
