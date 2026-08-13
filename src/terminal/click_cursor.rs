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
    /// Current OSC 133 `B` prompt anchor under the pane's render-surface
    /// policy. It bounds how far left a click may walk without bypassing the
    /// same rebase decision used by reviewed submission and prompt status.
    pub(crate) prompt_anchor: Rc<dyn Fn() -> (i64, i64)>,
    pub(crate) bstate: Rc<Cell<BlockState>>,
    pub(crate) mouse_mode: Rc<Cell<MouseReportingMode>>,
    pub(crate) fullscreen: Rc<Cell<bool>>,
    /// The palette colour inline suggestions are painted in — ANSI colour 8,
    /// which is what both jsh and zsh-autosuggestions use. Text right of the
    /// cursor in this colour is a preview, not buffer. Captured from the
    /// config at install time, like `enabled`.
    pub(crate) suggestion_rgb: [u8; 3],
}

/// ANSI colour 8 as VTE will spell it in an HTML export: each palette channel
/// rounded to eight bits.
pub(crate) fn suggestion_rgb(palette: &[gtk::gdk::RGBA; 16]) -> [u8; 3] {
    let channel = |v: f32| (f64::from(v.clamp(0.0, 1.0)) * 255.0).round() as u8;
    let color = &palette[8];
    [
        channel(color.red()),
        channel(color.green()),
        channel(color.blue()),
    ]
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

/// Strict reviewed-execution check for everything to the right of the cursor.
/// Visual colour and prompt geometry are deliberately irrelevant here: real
/// editable input can imitate suggestion styling. VTE emits CR/LF separators
/// for structurally empty rows, so those alone are allowed.
pub(crate) fn verified_suffix_is_empty(vte: &Terminal) -> Option<bool> {
    let (cursor_col, cursor_row) = vte.cursor_position();
    let bottom_row = cursor_row.saturating_add(vte.row_count().max(1));
    vte.text_range_format(
        vte4::Format::Text,
        cursor_row,
        cursor_col,
        bottom_row,
        vte.column_count(),
    )
    .0
    .map(|text| text.bytes().all(|byte| matches!(byte, b'\r' | b'\n')))
}

/// One character as VTE will paint it: `color` is `None` for the default
/// foreground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaintedChar {
    ch: char,
    color: Option<[u8; 3]>,
}

/// Decode VTE's `Format::Html` export into painted characters.
///
/// This is the only surviving channel for per-character colour: the
/// `VteCharAttributes` array of `vte_terminal_get_text_range` is deprecated
/// and already comes back empty on current VTE. The export is narrow and
/// regular — a `<pre>` wrapper, `<font color="#RRGGBB">` runs for anything
/// not in the default foreground, `<b>`/`<i>`-style tags for attributes, and
/// `g_markup` entities — so a small hand parser beats dragging in an HTML
/// crate. Unknown tags are ignored; unknown entities are kept verbatim so a
/// parsing surprise degrades to miscounting, not to a panic.
fn painted_chars(html: &str) -> Vec<PaintedChar> {
    let mut painted = Vec::new();
    let mut fonts: Vec<Option<[u8; 3]>> = Vec::new();
    let mut rest = html;
    while let Some(ch) = rest.chars().next() {
        match ch {
            '<' => {
                let Some(close) = rest.find('>') else { break };
                let tag = &rest[1..close];
                rest = &rest[close + 1..];
                if let Some(attrs) = tag.strip_prefix("font") {
                    let color = attrs.split_once("color=\"#").and_then(|(_, tail)| {
                        let hex = tail.get(..6)?;
                        let n = u32::from_str_radix(hex, 16).ok()?;
                        Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
                    });
                    fonts.push(color);
                } else if tag.starts_with("/font") {
                    fonts.pop();
                }
            }
            '&' => {
                let entity = rest[1..].find(';').map(|semi| &rest[1..1 + semi]);
                let decoded = match entity {
                    Some("lt") => Some('<'),
                    Some("gt") => Some('>'),
                    Some("amp") => Some('&'),
                    Some("quot") => Some('"'),
                    Some("apos") => Some('\''),
                    Some(numeric) => numeric
                        .strip_prefix('#')
                        .and_then(|d| {
                            d.strip_prefix('x')
                                .map_or_else(|| d.parse().ok(), |h| u32::from_str_radix(h, 16).ok())
                        })
                        .and_then(char::from_u32),
                    None => None,
                };
                let color = fonts.last().copied().flatten();
                if let (Some(ch), Some(semi)) = (decoded, rest[1..].find(';')) {
                    painted.push(PaintedChar { ch, color });
                    rest = &rest[semi + 2..];
                } else {
                    painted.push(PaintedChar { ch: '&', color });
                    rest = &rest[1..];
                }
            }
            _ => {
                painted.push(PaintedChar {
                    ch,
                    color: fonts.last().copied().flatten(),
                });
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    painted
}

/// The HTML VTE holds between two ring positions, or `None` when the range is
/// empty or unreadable (the caller then falls back to colour-blind counting).
fn html_between(vte: &Terminal, from: core_click::Cell, to: core_click::Cell) -> Option<String> {
    if (from.row, from.col) >= (to.row, to.col) {
        return None;
    }
    vte.text_range_format(vte4::Format::Html, from.row, from.col, to.row, to.col)
        .0
        .map(|text| text.to_string())
}

/// Characters the buffer can absorb to the right of the cursor, from the
/// painted text VTE reports between the cursor and the bottom of the screen.
///
/// Everything here errs toward fewer arrows, because a `Right` the buffer
/// cannot absorb is jsh accepting its inline suggestion — even one that is
/// not currently drawn (jsh keeps the suggestion while the cursor sits
/// mid-buffer and merely stops painting it). In order:
/// - stop at the first hard newline: rows below the input hold the shell's
///   own furniture (signature hint, completion menu), while soft-wrapped
///   input arrives joined without a newline;
/// - stop at the first suggestion-coloured character;
/// - trim trailing whitespace, then drop a right-aligned decoration — a
///   trailing run touching the right edge, parted from the input by a wide
///   blank gap, the shape jsh and fish park the previous command's duration
///   in — and re-trim.
fn absorbable_chars(
    painted: &[PaintedChar],
    suggestion: [u8; 3],
    columns: i64,
    cursor_col: i64,
) -> i64 {
    let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b.iter()).all(|(x, y)| x.abs_diff(*y) <= 1);

    // `col` is the cell the character starts in. Soft-wrapped rows are full by
    // definition, so wrapping the running total keeps it truthful across them.
    let mut kept: Vec<(char, i64)> = Vec::new();
    let mut col = cursor_col;
    for piece in painted {
        if piece.ch == '\n' || piece.color.is_some_and(|c| near(c, suggestion)) {
            break;
        }
        let width = unicode_width::UnicodeWidthChar::width(piece.ch).unwrap_or(0) as i64;
        kept.push((piece.ch, col % columns.max(1)));
        col += width;
    }

    let trim = |kept: &mut Vec<(char, i64)>| {
        while kept.last().is_some_and(|(ch, _)| ch.is_whitespace()) {
            kept.pop();
        }
    };
    trim(&mut kept);

    if let Some(&(last, last_col)) = kept.last() {
        let last_width = unicode_width::UnicodeWidthChar::width(last).unwrap_or(0) as i64;
        if last_col + last_width >= columns - 1 {
            let mut run = kept.len();
            while run > 0 && !kept[run - 1].0.is_whitespace() {
                run -= 1;
            }
            let mut gap = run;
            while gap > 0 && kept[gap - 1].0.is_whitespace() {
                gap -= 1;
            }
            if run - gap >= 3 {
                kept.truncate(gap);
                trim(&mut kept);
            }
        }
    }

    kept.len() as i64
}

/// How far the adjustment's row space lags behind the ring's.
///
/// `cell_at` reads rows off the vadjustment, but VTE renumbers that space
/// when rows fall off a bounded scrollback, while `cursor_position` and the
/// text APIs keep absolute ring rows — observed live in forge as a cursor on
/// ring row 44 under an adjustment whose upper bound was 10, which made
/// every click on the prompt read as far above the cursor and walk it to
/// the start of the buffer. The two spaces are anchored at the bottom: at a
/// prompt the cursor sits on the ring's last row, so `cursor.row + 1` and
/// the adjustment's upper bound name the same boundary. With no renumbering
/// the difference is zero (never negative — a shell that parks its cursor
/// mid-screen must not shift clicks upward, where extra arrows do damage;
/// biased low they at worst do less than asked).
fn adjustment_rebase_offset(vte: &Terminal, cursor: core_click::Cell) -> i64 {
    let upper = vte
        .vadjustment()
        .map(|adjustment| adjustment.upper() as i64)
        .unwrap_or(cursor.row + 1);
    rebase_offset(cursor.row, upper)
}

/// The arithmetic of [`adjustment_rebase_offset`], kept free of the widget so
/// the anchoring rule stays pinned by tests.
fn rebase_offset(cursor_row: i64, adjustment_upper: i64) -> i64 {
    (cursor_row + 1 - adjustment_upper).max(0)
}

/// Bytes that walk the line editor from the cursor to `click`, or nothing when
/// the click must not move it.
fn move_for_click(vte: &Terminal, ctx: &ClickCursorCtx, click: core_click::Cell) -> Vec<u8> {
    if !core_click::click_may_move_cursor(&ctx.guards()) {
        return Vec::new();
    }

    let (cursor_col, cursor_row) = vte.cursor_position();
    let cursor = core_click::Cell::new(cursor_row, cursor_col);
    let click = core_click::Cell::new(click.row + adjustment_rebase_offset(vte, cursor), click.col);
    if click == cursor {
        return Vec::new();
    }

    let steps = if (click.row, click.col) < (cursor.row, cursor.col) {
        -chars_between(vte, click, cursor)
    } else {
        chars_between(vte, cursor, click)
    };

    // How far left the input reaches: back to where the prompt handed over.
    let (start_col, start_row) = (ctx.prompt_anchor)();
    let max_left = chars_between(vte, core_click::Cell::new(start_row, start_col), cursor);

    // How far right it reaches: to the end of what the buffer really holds —
    // not the inline suggestion, not a right-aligned duration, not the rows
    // below the input. Every arrow past that end is one jsh spends accepting
    // the suggestion. Colour-blind counting is the fallback when the HTML
    // export is unavailable; it at least trims trailing blanks.
    let bottom = core_click::Cell::new(cursor.row + vte.row_count().max(1), vte.column_count());
    let max_right = match html_between(vte, cursor, bottom) {
        Some(html) => absorbable_chars(
            &painted_chars(&html),
            ctx.suggestion_rgb,
            vte.column_count(),
            cursor.col,
        ),
        None => text_between(vte, cursor, bottom).trim_end().chars().count() as i64,
    };

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

    const GREY: [u8; 3] = [0x59, 0x5C, 0x5E];

    fn plain(text: &str) -> Vec<PaintedChar> {
        text.chars()
            .map(|ch| PaintedChar { ch, color: None })
            .collect()
    }

    #[test]
    fn vte_html_decodes_to_painted_characters() {
        // The exact shape VTE emits, captured from a live terminal.
        let painted =
            painted_chars("<pre>ab<font color=\"#595C5E\">GH</font>\n你好<b>&lt;&amp;</b></pre>");
        let text: String = painted.iter().map(|piece| piece.ch).collect();
        assert_eq!(text, "abGH\n你好<&");
        assert_eq!(painted[0].color, None);
        assert_eq!(
            painted[2].color,
            Some(GREY),
            "the font run carries its colour"
        );
        assert_eq!(painted[4].color, None, "the closed font no longer applies");
        assert_eq!(
            painted[7].color, None,
            "unknown tags are ignored, entities decode"
        );
    }

    #[test]
    fn a_suggestion_stops_the_absorbable_count() {
        // "echo he" typed, "llo world" as the grey preview: the buffer can
        // absorb nothing to the right — the first arrow would accept it.
        let mut painted = plain("");
        painted.extend("llo world".chars().map(|ch| PaintedChar {
            ch,
            color: Some(GREY),
        }));
        assert_eq!(absorbable_chars(&painted, GREY, 80, 9), 0);

        // Mid-buffer, the typed tail before the preview stays reachable.
        let mut painted = plain("he");
        painted.extend("llo".chars().map(|ch| PaintedChar {
            ch,
            color: Some(GREY),
        }));
        assert_eq!(absorbable_chars(&painted, GREY, 80, 7), 2);

        // Ordinary coloured text is not a suggestion.
        let mut painted = plain("he");
        painted.extend("llo".chars().map(|ch| PaintedChar {
            ch,
            color: Some([0x20, 0xD0, 0x20]),
        }));
        assert_eq!(absorbable_chars(&painted, GREY, 80, 7), 5);
    }

    #[test]
    fn rows_below_the_input_are_not_absorbable() {
        // A signature hint or completion menu renders below a hard newline;
        // soft-wrapped input arrives joined without one.
        assert_eq!(
            absorbable_chars(&plain("llo\nls file:path"), GREY, 80, 9),
            3
        );
    }

    #[test]
    fn a_right_aligned_duration_is_not_absorbable() {
        // 32 columns: "llo" ends the command at column 12, "2.3s" sits flush
        // with the right edge the way jsh and fish park a duration.
        let text = format!("llo{}2.3s", " ".repeat(32 - 12 - 4));
        assert_eq!(absorbable_chars(&plain(&text), GREY, 32, 9), 3);
        // With nothing typed to its left the decoration is all there is.
        let text = format!("{}2.3s", " ".repeat(32 - 12 - 4));
        assert_eq!(absorbable_chars(&plain(&text), GREY, 32, 12), 0);
    }

    #[test]
    fn an_interior_gap_away_from_the_edge_stays_absorbable() {
        // A wide run of spaces inside a command whose tail stops short of the
        // right edge is buffer, and trailing blanks always trim.
        assert_eq!(absorbable_chars(&plain("a          b'  "), GREY, 40, 6), 13);
    }

    #[test]
    fn wide_characters_keep_the_column_count_honest() {
        // Four CJK characters starting at column 24 of 32 span to the edge;
        // a two-cell run with a wide gap before it must still be recognised
        // as reaching the edge... but here it is genuine input, preceded by
        // only two blanks, so nothing is cut.
        let text = "ab  你好世界";
        assert_eq!(absorbable_chars(&plain(text), GREY, 32, 20), 8);
    }

    #[test]
    fn a_renumbered_adjustment_shifts_clicks_back_down() {
        // The live capture that found this: cursor on ring row 44 under an
        // adjustment whose upper bound was 10 — every click read 35 rows too
        // high and walked the cursor to the start of the buffer.
        assert_eq!(rebase_offset(44, 10), 35);
        // An un-renumbered adjustment names the cursor row's boundary itself.
        assert_eq!(rebase_offset(5, 6), 0);
        // A cursor parked mid-screen must never shift clicks upward.
        assert_eq!(rebase_offset(0, 6), 0);
    }

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
