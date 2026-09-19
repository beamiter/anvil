//! alt — VTE builder + small parser helpers for the live terminal.
//!
//! forge aligns with Warp's alt-screen model: when an alt-screen app
//! (top/vim/htop/...) sends `?1049h`, the live VTE switches to its alt buffer
//! and renders full-viewport; when it sends `?1049l`, the alt-screen content
//! is **discarded** — the active block keeps only the command name + exit code.
//! No frame-merge / pager-snapshot path runs, matching Warp.
use crate::config::Config;
use gtk::gdk::RGBA;
use gtk::glib;
use gtk::pango::FontDescription;
use gtk::prelude::*;
use relm4::gtk;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

/// Give dense block output a little breathing room.  In particular, long
/// compiler/type traces are otherwise difficult to follow because many patched
/// monospace fonts paint almost up to VTE's default cell boundary.
pub(crate) const BLOCK_CELL_HEIGHT_SCALE: f64 = 1.12;

/// Headroom for a finished VTE's pixel-height request.
///
/// VTE derives its grid rows from the allocated content height. GTK/CSS rounding
/// can leave an exact `rows * cell_height` request a few pixels short, dropping
/// one row into scrollback. The output scrollbar then appears, changes the
/// sibling allocation enough to restore the row, disappears again, and repeats.
/// Keep the slack below one cell so it cannot create a phantom terminal row.
const FINISHED_VTE_HEIGHT_SLACK_PX: i32 = 6;

/// Absolute cell bound for each read-only finished VTE. Full text remains in
/// BlockData/copy/export; the renderer retains only a bounded terminal grid.
pub(crate) const MAX_FINISHED_VTE_GRID_CELLS: usize = 1_048_576;
/// Defensive ceiling for hostile or legacy persisted column counts.
pub(crate) const MAX_FINISHED_VTE_COLUMNS: i64 = 4_096;

pub(crate) fn bounded_finished_vte_columns(cols: i64) -> i64 {
    cols.clamp(1, MAX_FINISHED_VTE_COLUMNS)
}

/// Total rows a finished VTE may hold at `cols` — screen plus scrollback.
pub(crate) fn bounded_finished_vte_max_rows(cols: i64) -> i64 {
    let cols = bounded_finished_vte_columns(cols);
    (MAX_FINISHED_VTE_GRID_CELLS / cols as usize).max(1) as i64
}

pub(crate) fn bounded_finished_vte_geometry(
    cols: i64,
    visible_rows: i64,
    requested_scrollback_rows: i64,
) -> (i64, i64, i64) {
    let cols = bounded_finished_vte_columns(cols);
    let max_rows = bounded_finished_vte_max_rows(cols);
    let visible_rows = visible_rows.clamp(1, max_rows);
    let scrollback_rows = requested_scrollback_rows
        .max(0)
        .min(max_rows.saturating_sub(visible_rows));
    (cols, visible_rows, scrollback_rows)
}

pub(crate) fn finished_vte_height_px(rows: i64, cell_height: i32) -> i32 {
    let cell = cell_height.max(1);
    (rows.clamp(1, i32::MAX as i64) as i32)
        .saturating_mul(cell)
        .saturating_add(FINISHED_VTE_HEIGHT_SLACK_PX.min(cell - 1))
}

// ─── Mouse Reporting Mode ─────────────────────────────────────────────────────

pub(crate) use jterm_core::parser::{MouseEncoding, MouseMode};

/// The mouse reporting a program asked for, as two independent settings, the
/// way xterm and libvte keep them: *which* events are reported (`?9`,
/// `?1000`, `?1002`, `?1003`) and *how* they are written (`?1005`, `?1006`,
/// `?1015`). One last-write-wins mode used to stand for both, so `?1006;1000h`
/// (htop, ncurses' `XM`) ended on "click" and got legacy `ESC [ M` wheel bytes
/// it reads as keys, while `?1006h` alone counted as reporting switched on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct MouseReporting {
    pub(crate) tracking: MouseMode,
    pub(crate) encoding: MouseEncoding,
}

impl MouseReporting {
    /// Nothing reported, default encoding: the state after RIS, and the one a
    /// prompt boundary restores.
    pub(crate) const OFF: Self = Self {
        tracking: MouseMode::None,
        encoding: MouseEncoding::Default,
    };

    /// Whether the program receives mouse events at all. The encoding alone
    /// reports nothing.
    pub(crate) fn is_tracking(self) -> bool {
        self.tracking != MouseMode::None
    }

    /// Apply one DEC private mode from `CSI ? … h/l`, with the same rules as
    /// `jterm_core::parser::Parser`: a tracking mode's `l` turns tracking off
    /// whichever mode is on, and so does an encoding's `l` for the encoding.
    /// Other modes are ignored.
    pub(crate) fn apply_decset(&mut self, mode: u32, set: bool) {
        let tracking = match mode {
            9 => Some(MouseMode::X10),
            1000 => Some(MouseMode::Normal),
            1002 => Some(MouseMode::ButtonEvent),
            1003 => Some(MouseMode::AnyEvent),
            _ => None,
        };
        if let Some(tracking) = tracking {
            self.tracking = if set { tracking } else { MouseMode::None };
            return;
        }
        let encoding = match mode {
            1005 => Some(MouseEncoding::Utf8),
            1006 => Some(MouseEncoding::Sgr),
            1015 => Some(MouseEncoding::Urxvt),
            _ => None,
        };
        if let Some(encoding) = encoding {
            self.encoding = if set {
                encoding
            } else {
                MouseEncoding::Default
            };
        }
    }
}

/// `CSI ? … l` for every mouse tracking and encoding mode, plus focus
/// reporting. Fed to a live VTE that is *not* reset at a prompt boundary
/// (`preserve_live_scrollback`), so a program that died without switching
/// them off cannot leave libvte turning the next shell's clicks and focus
/// changes into reports.
pub(crate) const MOUSE_AND_FOCUS_REPORTING_OFF: &[u8] =
    b"\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1004l";

/// Most wheel reports one scroll event may produce. A flung touchpad or a
/// huge surface delta would otherwise queue hundreds of reports the program
/// has to work through after the user has stopped.
pub(crate) const MAX_WHEEL_REPORTS_PER_EVENT: i32 = 10;

/// Wheel reports for one scroll event that completed `notches` whole notches
/// (negative up, as in GTK), clamped to [`MAX_WHEEL_REPORTS_PER_EVENT`].
pub(crate) fn wheel_reports(
    reporting: MouseReporting,
    notches: i32,
    col: i64,
    row: i64,
) -> Vec<u8> {
    let count = notches
        .unsigned_abs()
        .min(MAX_WHEEL_REPORTS_PER_EVENT as u32);
    let Some(report) = encode_mouse_wheel(reporting, f64::from(notches.signum()), col, row) else {
        return Vec::new();
    };
    report.repeat(count as usize)
}

/// Encode one wheel notch as a mouse report in the program's encoding.
/// Returns `None` while it tracks no mouse events.
///
/// `delta_y` follows the GTK convention (negative = up, positive = down); only
/// its sign matters, so accumulate fractional deltas into notches first (see
/// [`jterm_core::wheel::WheelAccumulator`]). `col` / `row` are 1-based cell
/// coordinates under the pointer; if you don't have them, pass 1/1 — pagers
/// (less/vim) look at the button code, not the coordinate.
///
/// libvte would encode an SGR wheel report itself, but it counts every event
/// as a whole notch and ignores the scroll unit, and it cannot send the
/// legacy encodings at all without a PTY of its own, so anvil writes them.
pub(crate) fn encode_mouse_wheel(
    reporting: MouseReporting,
    delta_y: f64,
    col: i64,
    row: i64,
) -> Option<Vec<u8>> {
    if delta_y == 0.0 || !reporting.is_tracking() {
        return None;
    }
    // Buttons per xterm: 64 = wheel up, 65 = wheel down.
    let button: u32 = if delta_y < 0.0 { 64 } else { 65 };
    let c = col.clamp(1, u32::MAX as i64) as u32;
    let r = row.clamp(1, u32::MAX as i64) as u32;
    Some(match reporting.encoding {
        MouseEncoding::Sgr => format!("\x1b[<{button};{c};{r}M").into_bytes(),
        MouseEncoding::Urxvt => format!("\x1b[{};{c};{r}M", button + 32).into_bytes(),
        // `?1005`: every field is `value + 32` as a UTF-8 character, which
        // lifts the legacy 223-cell limit to 2015.
        MouseEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for value in [button, c.min(2015), r.min(2015)] {
                let ch = char::from_u32(value + 32).unwrap_or(' ');
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
        // Legacy X10 encoding: each field is one byte, `value + 32`, clamped
        // to the 223-cell limit (255 - 32).
        MouseEncoding::Default => {
            let cb = (button + 32).min(255) as u8;
            let cc = (c + 32).min(255) as u8;
            let cr = (r + 32).min(255) as u8;
            vec![0x1b, b'[', b'M', cb, cc, cr]
        }
    })
}

/// A scroll delta in wheel notches. Surface-unit deltas (Wayland touchpads in
/// GTK 4.14 are pixels) become notches the way `EventControllerScroll` does
/// when asked for discrete steps; a plain ±1.0 wheel notch is unchanged.
pub(crate) fn wheel_notches(dy: f64, unit: gtk::gdk::ScrollUnit) -> f64 {
    if unit == gtk::gdk::ScrollUnit::Surface {
        dy / jterm_core::wheel::SURFACE_UNITS_PER_STEP
    } else {
        dy
    }
}

/// Rows a finished snapshot's own content occupies, from where VTE left the
/// cursor (`cursor_row`, an absolute scrollback row) and the first row its ring
/// still holds (`first_row`, the vertical adjustment's lower bound).
///
/// The adjustment *span* (`upper - lower`) cannot be used for this. It counts
/// every row the ring retains, and each time the block layout squeezes a
/// snapshot VTE the rows it was showing move into scrollback rather than being
/// dropped — so the span grows on every squeeze. Sizing the widget from it grew
/// the block, the next squeeze added more rows, and a one-line `ls` result
/// ratcheted taller while every re-render flipped it back to the estimate. The
/// cursor sits on the last row the feed wrote, so this measure holds still under
/// layout churn and shrinks again for a shorter snapshot.
fn finished_content_rows(first_row: f64, cursor_row: i64, fallback_rows: i64) -> i64 {
    let fallback_rows = fallback_rows.max(1);
    if !first_row.is_finite() || first_row < i64::MIN as f64 {
        return fallback_rows;
    }
    let first = first_row.floor();
    if first > cursor_row as f64 {
        return fallback_rows;
    }
    (cursor_row - first as i64).saturating_add(1).max(1)
}

/// Fit a read-only finished VTE to the rows its snapshot actually occupies:
/// never below `floor_rows` (the caller's own row estimate, which still counts
/// rows a cursor-up sequence left below the cursor) and never above `max_rows`
/// (the viewport cap the layout gave this block — past it the block's own
/// scrollbar takes over).
///
/// VTE resolves wrapping, wide glyphs, tabs and cursor motion itself, so it is
/// the authority on how tall a snapshot really is. It also parses `feed()`
/// asynchronously, so an early settling pass can still be measuring the
/// *previous* snapshot; the target is therefore recomputed from the content on
/// every pass — never from the grid a previous pass set — so a later pass
/// corrects a stale measurement downwards as well as upwards.
pub(crate) fn fit_finished_terminal_to_content(
    terminal: &Terminal,
    floor_rows: i64,
    max_rows: i64,
) {
    let floor_rows = floor_rows.max(1);
    let max_rows = max_rows.max(floor_rows);
    let first_row = terminal
        .vadjustment()
        .map(|adj| adj.lower())
        .unwrap_or(f64::NAN);
    let (_, cursor_row) = terminal.cursor_position();
    let rows = finished_content_rows(first_row, cursor_row, terminal.row_count())
        .clamp(floor_rows, max_rows);
    let (cols, rows, scrollback_rows) =
        bounded_finished_vte_geometry(terminal.column_count(), rows, i64::MAX);

    if terminal.row_count() != rows || terminal.column_count() != cols {
        terminal.set_size(cols, rows);
    }
    // Preserve the unused portion of the one-grid budget as scrollback. An
    // older asynchronous settle cannot then discard a newer feed's tail.
    terminal.set_scrollback_lines(scrollback_rows);

    let cell_height = (terminal.char_height() as i32).max(1);
    terminal.set_height_request(finished_vte_height_px(rows, cell_height));

    if let Some(adj) = terminal.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// Settle a finished snapshot after bytes have been fed. VTE parses `feed()`
/// asynchronously, so use two idle passes: the first folds any overflow /
/// soft-wrapped rows into the widget, and the second re-measures once VTE has
/// caught up, correcting the first in either direction. `floor_rows` / `max_rows`
/// bound both passes, so a measurement taken mid-parse can never resize the
/// block past the height its own render asked for.
pub(crate) fn settle_finished_terminal_after_feed(
    terminal: &Terminal,
    floor_rows: i64,
    max_rows: i64,
) {
    let terminal = terminal.clone();
    glib::idle_add_local_once(move || {
        fit_finished_terminal_to_content(&terminal, floor_rows, max_rows);
        let terminal = terminal.clone();
        glib::idle_add_local_once(move || {
            fit_finished_terminal_to_content(&terminal, floor_rows, max_rows);
            let terminal = terminal.clone();
            glib::idle_add_local_once(move || {
                if let Some(adj) = terminal.vadjustment() {
                    adj.set_value(adj.lower());
                }
            });
        });
    });
}

/// The command snapshot is the only finished VTE placed directly inside the
/// horizontal prompt row. Output snapshots are direct children of the block's
/// vertical card and already use `FinishedBlock::connect_scroll_forwarding`.
fn is_finished_command_surface(terminal: &Terminal) -> bool {
    terminal
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Box>().ok())
        .is_some_and(|parent| parent.orientation() == gtk::Orientation::Horizontal)
}

fn outer_block_scroller(terminal: &Terminal) -> Option<gtk::ScrolledWindow> {
    let mut parent = terminal.parent();
    while let Some(widget) = parent {
        if let Ok(scroll) = widget.clone().downcast::<gtk::ScrolledWindow>() {
            return Some(scroll);
        }
        parent = widget.parent();
    }
    None
}

fn forward_command_surface_scroll(terminal: &Terminal, dy: f64) -> bool {
    if dy == 0.0 || !is_finished_command_surface(terminal) {
        return false;
    }
    let Some(outer) = outer_block_scroller(terminal) else {
        return false;
    };
    let adjustment = outer.vadjustment();
    let step = adjustment
        .step_increment()
        .max(adjustment.page_size() * 0.1);
    let max_value = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    let target = (adjustment.value() + dy * step).clamp(adjustment.lower(), max_value);
    adjustment.set_value(target);
    true
}

#[cfg(test)]
// Protocol tests stay beside the encoder helpers they specify; moving this
// block below GTK builders would separate the small pure-logic unit.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn reporting(tracking: MouseMode, encoding: MouseEncoding) -> MouseReporting {
        MouseReporting { tracking, encoding }
    }

    #[test]
    fn sgr_wheel_up_encodes_button_64() {
        // delta_y < 0 → wheel up → button 64 (xterm convention).
        let seq = encode_mouse_wheel(
            reporting(MouseMode::AnyEvent, MouseEncoding::Sgr),
            -1.0,
            10,
            5,
        )
        .unwrap();
        assert_eq!(seq, b"\x1b[<64;10;5M");
    }

    #[test]
    fn sgr_wheel_down_encodes_button_65() {
        let seq = encode_mouse_wheel(reporting(MouseMode::Normal, MouseEncoding::Sgr), 1.0, 1, 1)
            .unwrap();
        assert_eq!(seq, b"\x1b[<65;1;1M");
    }

    #[test]
    fn x10_wheel_up_uses_value_plus_32() {
        // Legacy encoding: each field encoded as byte = value + 32.
        let seq = encode_mouse_wheel(
            reporting(MouseMode::ButtonEvent, MouseEncoding::Default),
            -1.0,
            1,
            1,
        )
        .unwrap();
        assert_eq!(seq, vec![0x1b, b'[', b'M', 64 + 32, 1 + 32, 1 + 32]);
    }

    #[test]
    fn urxvt_and_utf8_wheel_reports_use_their_own_encodings() {
        let urxvt = encode_mouse_wheel(
            reporting(MouseMode::Normal, MouseEncoding::Urxvt),
            1.0,
            3,
            4,
        )
        .unwrap();
        assert_eq!(urxvt, b"\x1b[97;3;4M");
        // Column 300 is past the legacy limit; UTF-8 carries it.
        let utf8 = encode_mouse_wheel(
            reporting(MouseMode::Normal, MouseEncoding::Utf8),
            -1.0,
            300,
            2,
        )
        .unwrap();
        let mut expected = vec![0x1b, b'[', b'M', 96];
        expected.extend_from_slice("\u{14c}".as_bytes()); // 300 + 32
        expected.push(34);
        assert_eq!(utf8, expected);
    }

    #[test]
    fn none_mode_returns_no_bytes() {
        assert!(encode_mouse_wheel(MouseReporting::OFF, -1.0, 1, 1).is_none());
        // An encoding without tracking reports nothing either.
        let encoding_only = reporting(MouseMode::None, MouseEncoding::Sgr);
        assert!(!encoding_only.is_tracking());
        assert!(encode_mouse_wheel(encoding_only, -1.0, 1, 1).is_none());
    }

    #[test]
    fn zero_delta_returns_no_bytes() {
        // Spurious 0 delta from GTK shouldn't paginate the app.
        assert!(encode_mouse_wheel(
            reporting(MouseMode::AnyEvent, MouseEncoding::Sgr),
            0.0,
            1,
            1
        )
        .is_none());
    }

    /// Feed `bytes` through the real parser and apply its DECSET events.
    fn reporting_after(bytes: &[u8]) -> MouseReporting {
        let mut parser =
            jterm_core::parser::Parser::with_config(jterm_core::parser::ParserConfig {
                mouse_reporting: true,
                focus_reporting: true,
            });
        let mut events = Vec::new();
        parser.feed(bytes, &mut events);
        let mut state = MouseReporting::OFF;
        for event in events {
            if let jterm_core::parser::ParserEvent::DecsetMode { mode, set } = event {
                state.apply_decset(mode, set);
            }
        }
        // The shadow must agree with the parser's own view of the stream.
        assert_eq!(state.tracking, parser.mouse_mode());
        assert_eq!(state.encoding, parser.mouse_encoding());
        state
    }

    #[test]
    fn tracking_and_encoding_are_separate_settings() {
        // htop / ncurses `XM`: SGR first, then click tracking. The wheel must
        // still be written in SGR, not as legacy `ESC [ M` key-like bytes.
        let htop = reporting_after(b"\x1b[?1006;1000h");
        assert_eq!(htop, reporting(MouseMode::Normal, MouseEncoding::Sgr));
        assert_eq!(
            encode_mouse_wheel(htop, 1.0, 7, 9).unwrap(),
            b"\x1b[<65;7;9M"
        );
        // vim 9: the same, then button tracking on top.
        let vim = reporting_after(b"\x1b[?1006;1000h\x1b[?1002h");
        assert_eq!(vim, reporting(MouseMode::ButtonEvent, MouseEncoding::Sgr));
        // claude's fullscreen UI and opencode end on SGR any-event tracking.
        let claude = reporting_after(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
        assert_eq!(claude, reporting(MouseMode::AnyEvent, MouseEncoding::Sgr));
        // Their exit turns tracking off; a stray encoding alone is not
        // reporting.
        let exited = reporting_after(b"\x1b[?1000h\x1b[?1003h\x1b[?1006h\x1b[?1003l\x1b[?1000l");
        assert!(!exited.is_tracking());
        assert_eq!(exited.encoding, MouseEncoding::Sgr);
        assert_eq!(reporting_after(b"\x1b[?1006l"), MouseReporting::OFF);
    }

    #[test]
    fn one_event_writes_one_report_per_notch_up_to_the_clamp() {
        let sgr = reporting(MouseMode::AnyEvent, MouseEncoding::Sgr);
        assert_eq!(wheel_reports(sgr, 0, 1, 1), b"");
        assert_eq!(wheel_reports(sgr, -2, 1, 1), b"\x1b[<64;1;1M\x1b[<64;1;1M");
        let flung = wheel_reports(sgr, 500, 1, 1);
        assert_eq!(
            flung.len(),
            b"\x1b[<65;1;1M".len() * MAX_WHEEL_REPORTS_PER_EVENT as usize
        );
        assert!(wheel_reports(MouseReporting::OFF, 3, 1, 1).is_empty());
    }

    #[test]
    fn surface_deltas_are_normalised_and_wheel_notches_pass_through() {
        use gtk::gdk::ScrollUnit;
        assert_eq!(wheel_notches(1.0, ScrollUnit::Wheel), 1.0);
        assert_eq!(wheel_notches(-0.25, ScrollUnit::Wheel), -0.25);
        assert_eq!(wheel_notches(25.0, ScrollUnit::Surface), 2.5);
        // And accumulated, 25 px is two notches with half a notch carried.
        let mut acc = jterm_core::wheel::WheelAccumulator::new();
        assert_eq!(acc.push(25.0, true), 2);
        assert_eq!(acc.push(5.0, true), 1);
    }

    #[test]
    fn content_rows_span_the_ring_from_its_first_row_to_the_cursor() {
        // Wrapped rows sit between the ring's first row and the cursor: three
        // rows of content with the cursor on the last one.
        assert_eq!(finished_content_rows(-7.0, -5, 1), 3);
        assert_eq!(finished_content_rows(0.0, 0, 4), 1);
    }

    #[test]
    fn content_rows_ignore_rows_a_squeeze_pushed_into_scrollback() {
        // One 3-row snapshot, measured while the widget is allocated a single row
        // (the other two are in scrollback) and again while allocated eight. A
        // span-based measure reports 3 then 8 and ratchets the block taller; the
        // cursor reports the content either way.
        assert_eq!(finished_content_rows(10.0, 12, 1), 3);
        assert_eq!(finished_content_rows(10.0, 12, 8), 3);
    }

    #[test]
    fn content_rows_fall_back_when_vte_cannot_describe_a_height() {
        assert_eq!(finished_content_rows(f64::NAN, 4, 3), 3);
        // A cursor above the ring's first row cannot describe a height.
        assert_eq!(finished_content_rows(9.0, 4, 5), 5);
        assert_eq!(finished_content_rows(f64::NAN, 4, 0), 1);
    }

    #[test]
    fn finished_vte_height_keeps_rounding_slack_below_one_row() {
        let height = finished_vte_height_px(2, 28);
        assert!(height > 2 * 28);
        assert!(height < 3 * 28);
        assert_eq!(finished_vte_height_px(2, 1), 2);
    }

    #[test]
    fn finished_vte_geometry_caps_columns_and_total_cells() {
        let (cols, visible, scrollback) =
            bounded_finished_vte_geometry(i64::MAX, i64::MAX, i64::MAX);
        assert_eq!(cols, MAX_FINISHED_VTE_COLUMNS);
        assert_eq!(
            visible,
            (MAX_FINISHED_VTE_GRID_CELLS / cols as usize) as i64
        );
        assert_eq!(scrollback, 0);
        assert!((visible + scrollback) as usize * cols as usize <= MAX_FINISHED_VTE_GRID_CELLS);
    }

    #[test]
    fn finished_vte_columns_clamp_before_metadata_height_math() {
        assert_eq!(bounded_finished_vte_columns(0), 1);
        assert_eq!(bounded_finished_vte_columns(80), 80);
        assert_eq!(
            bounded_finished_vte_columns(i64::MAX),
            MAX_FINISHED_VTE_COLUMNS
        );
    }

    #[test]
    fn finished_vte_geometry_shares_one_budget_with_scrollback() {
        let (cols, visible, scrollback) = bounded_finished_vte_geometry(80, 20, i64::MAX);
        assert_eq!(cols, 80);
        assert_eq!(visible, 20);
        assert_eq!(
            visible + scrollback,
            (MAX_FINISHED_VTE_GRID_CELLS / 80) as i64
        );
    }
}

// ─── VTE builder ─────────────────────────────────────────────────────────────

/// Apply colors + font + font scale from `config` onto an existing Terminal.
/// Single source of truth for VTE theming so the live VTE and read-only
/// finished-block VTEs stay visually identical.
/// One Pango parse per distinct font string, shared by every VTE.
///
/// Building the description re-parses the same configured string for the live
/// surface and for both read-only VTEs of every finished card. The value
/// changes only when the config does, so a one-entry cache is enough — keyed
/// on the icon fallback too, since that is the other half of what gets built.
fn with_font_description<R>(config: &Config, f: impl FnOnce(&FontDescription) -> R) -> R {
    thread_local! {
        static CACHED: std::cell::RefCell<Option<(String, Option<String>, FontDescription)>> =
            const { std::cell::RefCell::new(None) };
    }
    let icon_family = crate::font::icon_family(config);
    CACHED.with(|cached| {
        let mut cached = cached.borrow_mut();
        let current = cached.as_ref().is_some_and(|(desc, icon, _)| {
            desc == &config.font_desc && icon.as_deref() == icon_family
        });
        if !current {
            *cached = Some((
                config.font_desc.clone(),
                icon_family.map(str::to_owned),
                crate::font::font_description(&config.font_desc, icon_family),
            ));
        }
        let (_, _, parsed) = cached.as_ref().expect("populated above");
        f(parsed)
    })
}

pub(crate) fn apply_theme_to_vte(terminal: &Terminal, config: &Config) {
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    with_font_description(config, |font_desc| {
        terminal.set_font(Some(font_desc));
    });
    terminal.set_font_scale(config.default_font_scale);
}

/// One PCRE2 compile of the URL pattern for the whole thread.
///
/// `vte4::Regex::for_match` compiles and JIT-studies the pattern; every live
/// VTE and every finished card's TWO read-only VTEs used to do that
/// independently, so a session with 200 blocks paid ~400 identical compiles.
/// `match_add_regex` takes its own reference, so one handle can be shared by
/// every terminal.
fn with_url_match_regex(f: impl FnOnce(&vte4::Regex)) {
    thread_local! {
        static URL_REGEX: Option<vte4::Regex> = vte4::Regex::for_match(
            r"[a-z]+://[[:graph:]]+",
            pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
        )
        .ok();
    }
    URL_REGEX.with(|regex| {
        if let Some(regex) = regex.as_ref() {
            f(regex);
        }
    });
}

/// Register the shared URL pattern on `terminal` so hyperlink hovering works.
pub(crate) fn add_url_match_regex(terminal: &Terminal) {
    with_url_match_regex(|regex| {
        terminal.match_add_regex(regex, 0);
    });
}

/// The single persistent live VTE for block mode. It keeps `input_enabled(true)`
/// so the VTE translates keypresses into terminal byte sequences and emits them
/// via its `commit` signal (which we forward to our PTY). It also owns IME
/// natively, so there is no separate IMMulticontext to fight for fcitx/ibus focus.
pub(crate) fn create_active_terminal(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .name("term_name")
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        // Reading the running command's scrollback must survive streaming:
        // VTE's default yanks the view to the bottom on every output chunk, so
        // a reader who scrolled up lost their place tens of times a second. A
        // view that is already at the bottom still follows output natively.
        // Matches forge's live surface (forge alt_screen.rs).
        .scroll_on_output(false)
        .cursor_blink_mode(CursorBlinkMode::System)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .cell_height_scale(BLOCK_CELL_HEIGHT_SCALE)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();
    terminal.set_mouse_autohide(true);
    // Backspace must emit ASCII DEL (0x7f), not BS (0x08). Our PTY isn't VTE-owned,
    // so VTE's Auto binding can't read the tty erase char and falls back to 0x08,
    // which readline-style line editors (incl. jsh) ignore — making Backspace dead.
    terminal.set_backspace_binding(vte4::EraseBinding::AsciiDelete);
    apply_theme_to_vte(&terminal, config);
    add_url_match_regex(&terminal);
    terminal
}

pub(crate) fn apply_snapshot_theme_to_vte(terminal: &Terminal, config: &Config) {
    apply_theme_to_vte(terminal, config);
    let mut transparent = config.background;
    transparent.set_alpha(0.0);
    terminal.set_color_cursor(Some(&transparent));
}

/// A read-only PTY-less VTE used as the renderer for one finished command or
/// output surface. The caller supplies a provisional row count; after the first
/// map/feed pass, the widget expands to VTE's complete rendered buffer so no
/// wrapping, multiline input or terminal control sequence is clipped into a
/// private inner scroll area.
pub(crate) fn create_finished_terminal(
    config: &Config,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
    expand_to_buffer: bool,
) -> Terminal {
    let requested_visible_rows = output_rows.min(viewport_cap).max(1);
    // The caller's estimate can be too small (most notably a long single-line
    // command that wraps). Keep enough temporary scrollback to retain those rows
    // until the post-feed expansion below makes them part of the widget itself.
    // VTE treats this as a limit and allocates used rows, not a 50k-row grid.
    let capture_rows = output_rows
        .max(viewport_cap)
        .max(config.truncation_threshold_lines as i64)
        .max(4096)
        .clamp(1, u32::MAX as i64);
    let (cols, visible_rows, capture_rows) =
        bounded_finished_vte_geometry(cols, requested_visible_rows, capture_rows);
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(false)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(false)
        .scrollback_lines(capture_rows as u32)
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .cell_height_scale(BLOCK_CELL_HEIGHT_SCALE)
        .opacity(1.0)
        .pointer_autohide(true)
        // Images decoded from the live surface are mounted separately under a
        // completed card. Replaying arbitrary DCS here would bypass both the
        // finished-grid and kitty-image ledgers.
        .enable_sixel(false)
        .scroll_on_output(false)
        .scroll_on_keystroke(false)
        .build();
    terminal.set_mouse_autohide(true);
    apply_snapshot_theme_to_vte(&terminal, config);
    terminal.set_size(cols, visible_rows);

    // `blocks.rs` performs the actual feed from its map/filter paths. Keep
    // one constructor-level hook for command snapshots and other one-shot users;
    // every explicit re-feed also calls the same settling helper.
    if expand_to_buffer {
        let settled = std::cell::Cell::new(false);
        let cap = viewport_cap.max(visible_rows);
        terminal.connect_map(move |terminal| {
            if settled.replace(true) {
                return;
            }
            settle_finished_terminal_after_feed(terminal, visible_rows, cap);
        });
    }

    // VTE consumes wheel events even when its private scrollback is empty. The
    // output surface already has explicit outer-scroll forwarding in blocks.rs;
    // add the same behavior to the command surface, identified by its horizontal
    // prompt-row parent, so scrolling never sticks while the pointer is over it.
    {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        // The controller is owned by `terminal`; a strong clone here would
        // form terminal -> controller -> closure -> terminal and keep every
        // evicted finished VTE (including its scrollback grid) alive forever.
        let terminal_for_scroll = terminal.downgrade();
        scroll.connect_scroll(move |controller, _dx, dy| {
            let Some(terminal_for_scroll) = terminal_for_scroll.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let dy = wheel_notches(dy, controller.unit());
            if forward_command_surface_scroll(&terminal_for_scroll, dy) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        terminal.add_controller(scroll);
    }

    // URL detection — the same shared pattern the live VTE registers.
    add_url_match_regex(&terminal);
    terminal
}
