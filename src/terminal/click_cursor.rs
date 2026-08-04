//! Click-to-place-cursor for the live VTE prompt.
//!
//! A click cannot move the shell's caret directly; it has to become the arrow
//! keys the user would otherwise hold down. The decisions — whether a click may
//! move the cursor at all, how far it may travel, what bytes that becomes —
//! belong to `jterm_core::click_cursor`, which all four terminals share. What
//! lives here is the VTE-specific half: turning pointer pixels into ring
//! coordinates, and measuring distance in *characters* by reading the text
//! between two points rather than by subtracting cell indices (a CJK character
//! covers two cells but is one arrow press).
//!
//! Distinguishing a click from the start of a selection drag needs the release,
//! and a capture-phase `GestureClick` never sees one: VTE claims the sequence
//! for its own selection gesture, which cancels ours. `EventControllerLegacy`
//! is not a gesture, so it is not cancelled — it is used here purely to observe
//! that the button came back up.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk::gdk::{EventType, ModifierType};
use gtk::prelude::*;
use jterm_core::click_cursor as core_click;
use relm4::gtk;
use vte4::{Terminal, TerminalExt};

use crate::block_view::{BlockState, MouseReportingMode};
use crate::pty::OwnedPty;

/// Everything the handler needs from the block view's shared state.
pub(crate) struct ClickCursorCtx {
    pub(crate) enabled: bool,
    pub(crate) pty: Rc<OwnedPty>,
    /// VTE cursor position captured at OSC 133 `B`, i.e. where the user's
    /// command starts. It bounds how far left a click may walk.
    pub(crate) prompt_end_pos: Rc<Cell<(i64, i64)>>,
    pub(crate) bstate: Rc<Cell<BlockState>>,
    pub(crate) mouse_mode: Rc<Cell<MouseReportingMode>>,
    pub(crate) fullscreen: Rc<Cell<bool>>,
}

/// How much of the shell's work the block state machine can vouch for.
fn phase_of(state: BlockState) -> core_click::ShellPhase {
    match state {
        BlockState::AwaitingCommand => core_click::ShellPhase::Editing,
        BlockState::CollectingOutput | BlockState::AltScreen => core_click::ShellPhase::Running,
        // `RawFallback` is a shell with no OSC 133 integration at all. Staying
        // `Unknown` keeps the feature working there.
        BlockState::Idle
        | BlockState::CollectingPrompt
        | BlockState::PostCommand
        | BlockState::RawFallback => core_click::ShellPhase::Unknown,
    }
}

impl ClickCursorCtx {
    fn guards(&self) -> core_click::Guards {
        core_click::Guards {
            enabled: self.enabled,
            mouse_reporting: self.mouse_mode.get() != MouseReportingMode::None,
            alt_screen: self.fullscreen.get(),
            // Click and cursor are both read in absolute ring coordinates, so
            // a scrolled-back view needs no separate veto: the distance stays
            // truthful and the input-span clamp below bounds it.
            scrolled_back: false,
            phase: phase_of(self.bstate.get()),
        }
    }
}

/// Pointer pixels to an absolute ring cell (the space `cursor_position` and
/// `text_range_format` both use).
fn cell_at(vte: &Terminal, x: f64, y: f64) -> core_click::Cell {
    let char_width = (vte.char_width() as f64).max(1.0);
    let char_height = (vte.char_height() as f64).max(1.0);
    let top_row = vte
        .vadjustment()
        .map(|adjustment| adjustment.value())
        .unwrap_or(0.0);
    let col = (x.max(0.0) / char_width).floor() as i64;
    let row = top_row as i64 + (y.max(0.0) / char_height).floor() as i64;
    core_click::Cell::new(row, col.max(0))
}

/// The text VTE holds between two ring positions, or empty when the range is
/// empty or unreadable.
fn text_between(vte: &Terminal, from: core_click::Cell, to: core_click::Cell) -> String {
    if (from.row, from.col) >= (to.row, to.col) {
        return String::new();
    }
    vte.text_range_format(vte4::Format::Text, from.row, from.col, to.row, to.col)
        .0
        .map(|text| text.to_string())
        .unwrap_or_default()
}

/// Characters between two ring positions. Soft-wrapped rows are joined by VTE
/// without a newline, so an interior newline really is one editable position.
fn chars_between(vte: &Terminal, from: core_click::Cell, to: core_click::Cell) -> i64 {
    text_between(vte, from, to).chars().count() as i64
}

/// Bytes that walk the line editor from the cursor to `click`, or nothing when
/// the click must not move it.
fn move_for_click(vte: &Terminal, ctx: &ClickCursorCtx, click: core_click::Cell) -> Vec<u8> {
    if !core_click::click_may_move_cursor(&ctx.guards()) {
        return Vec::new();
    }

    let (cursor_col, cursor_row) = vte.cursor_position();
    let cursor = core_click::Cell::new(cursor_row, cursor_col);
    if click == cursor {
        return Vec::new();
    }

    let steps = if (click.row, click.col) < (cursor.row, cursor.col) {
        -chars_between(vte, click, cursor)
    } else {
        chars_between(vte, cursor, click)
    };

    // How far left the input reaches: back to where the prompt handed over.
    let (start_col, start_row) = ctx.prompt_end_pos.get();
    let max_left = chars_between(vte, core_click::Cell::new(start_row, start_col), cursor);

    // How far right it reaches: to the last character still on screen. The
    // trailing trim is what stops a click on empty space from spending arrows
    // the buffer cannot absorb — in jsh a `Right` at end-of-buffer accepts the
    // inline suggestion instead of moving.
    let bottom = core_click::Cell::new(cursor.row + vte.row_count().max(1), vte.column_count());
    let max_right = text_between(vte, cursor, bottom).trim_end().chars().count() as i64;

    let steps = core_click::clamp_steps(steps, max_left, max_right);
    // VTE owns the terminal state, so DECCKM is not observable from here. The
    // normal-mode encoding is the safe choice: shells bind both forms, and the
    // applications that turn DECCKM on are full-screen ones this never runs
    // for (the alt-screen guard already refused them).
    core_click::arrow_bytes(steps, false)
}

/// Attach the press/motion/release trio to a live VTE.
pub(crate) fn install(vte: &Terminal, ctx: ClickCursorCtx) {
    let tracker = Rc::new(RefCell::new(core_click::ClickTracker::default()));
    let ctx = Rc::new(ctx);

    {
        let tracker = tracker.clone();
        let vte_for_press = vte.clone();
        let press = gtk::GestureClick::new();
        press.set_button(GDK_BUTTON_PRIMARY as u32);
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        press.connect_pressed(move |controller, n_press, x, y| {
            let modifiers = controller.current_event_state();
            let plain = n_press == 1
                && !modifiers.intersects(
                    ModifierType::CONTROL_MASK
                        | ModifierType::SHIFT_MASK
                        | ModifierType::ALT_MASK
                        | ModifierType::SUPER_MASK,
                );
            tracker
                .borrow_mut()
                .press(cell_at(&vte_for_press, x, y), plain);
        });
        vte.add_controller(press);
    }

    {
        let vte_for_motion = vte.clone();
        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        motion.connect_motion({
            let tracker = tracker.clone();
            move |_, x, y| {
                tracker
                    .borrow_mut()
                    .pointer_at(cell_at(&vte_for_motion, x, y));
            }
        });
        motion.connect_leave({
            let tracker = tracker.clone();
            move |_| tracker.borrow_mut().cancel()
        });
        vte.add_controller(motion);
    }

    {
        let vte_for_release = vte.clone();
        let legacy = gtk::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
        legacy.connect_event(move |_, event| {
            if event.event_type() != EventType::ButtonRelease {
                return gtk::glib::Propagation::Proceed;
            }
            let Some(click) = tracker.borrow_mut().release() else {
                return gtk::glib::Propagation::Proceed;
            };
            let bytes = move_for_click(&vte_for_release, &ctx, click);
            if !bytes.is_empty() {
                ctx.pty.write_bytes(&bytes);
            }
            // Never consume the release: VTE still has to finish its own
            // selection gesture with it.
            gtk::glib::Propagation::Proceed
        });
        vte.add_controller(legacy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_foreground_program_blocks_the_click() {
        // A shell with no OSC 133 integration never leaves `RawFallback`, and
        // refusing there would remove the feature from plain bash.
        for state in [
            BlockState::Idle,
            BlockState::CollectingPrompt,
            BlockState::PostCommand,
            BlockState::RawFallback,
        ] {
            assert_eq!(phase_of(state), core_click::ShellPhase::Unknown);
        }
        assert_eq!(
            phase_of(BlockState::AwaitingCommand),
            core_click::ShellPhase::Editing
        );
        for state in [BlockState::CollectingOutput, BlockState::AltScreen] {
            assert_eq!(phase_of(state), core_click::ShellPhase::Running);
        }
    }
}
