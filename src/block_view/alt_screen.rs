//! alt — VTE builder + small parser helpers for the live terminal.
//!
//! jterm4 aligns with Warp's alt-screen model: when an alt-screen app
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

// ─── Mouse Reporting Mode ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum MouseReportingMode {
    /// No mouse reporting (CSI ?1000l, etc.)
    #[default]
    None,
    /// Basic click reporting (CSI ?1000h)
    Click,
    /// Button press/release/drag (CSI ?1002h)
    Button,
    /// All mouse motion (CSI ?1003h)
    Motion,
    /// SGR-style reporting (CSI ?1006h) - modern format
    Sgr,
}

/// Encode a wheel-scroll event as a mouse-reporting byte sequence appropriate
/// for `mode`. Returns `None` if the mode has no wheel reporting (e.g. `None`,
/// or a mode where wheel deltas don't translate).
///
/// `delta_y` follows the GTK convention (negative = up, positive = down).
/// `col` / `row` are 1-based cell coordinates under the pointer; if you don't
/// have them, pass 1/1 — pagers (less/vim) look at the button code, not the
/// coordinate.
///
/// VTE 4 normally encodes wheel events itself, but only when it owns the PTY;
/// jterm4's live VTE is fed by our own reader so we synthesize the bytes here.
pub(crate) fn encode_mouse_wheel(
    mode: MouseReportingMode,
    delta_y: f64,
    col: i64,
    row: i64,
) -> Option<Vec<u8>> {
    if delta_y == 0.0 {
        return None;
    }
    // Buttons per xterm: 64 = wheel up, 65 = wheel down.
    let button: u32 = if delta_y < 0.0 { 64 } else { 65 };
    let c = col.max(1);
    let r = row.max(1);
    match mode {
        MouseReportingMode::None => None,
        MouseReportingMode::Sgr => Some(format!("\x1b[<{};{};{}M", button, c, r).into_bytes()),
        // X10-style modes encode each field as `value + 32` in a single byte.
        // Wheel reporting requires at least Button-event tracking (1002), but
        // xterm's de-facto behavior also forwards wheel under plain Click
        // (1000), so we emit for any non-None, non-SGR mode.
        MouseReportingMode::Click | MouseReportingMode::Button | MouseReportingMode::Motion => {
            // Clamp to the legacy 223-column limit (255 - 32).
            let cb = (button + 32).min(255) as u8;
            let cc = (c as u32 + 32).min(255) as u8;
            let cr = (r as u32 + 32).min(255) as u8;
            Some(vec![0x1b, b'[', b'M', cb, cc, cr])
        }
    }
}

/// Convert VTE's adjustment extent into the number of rows currently retained
/// by the snapshot. `lower` is negative when wrapped/overflow rows live in
/// scrollback, while `upper - lower` covers both scrollback and the visible grid.
fn finished_buffer_rows_from_adjustment(lower: f64, upper: f64, visible_rows: i64) -> i64 {
    let visible_rows = visible_rows.max(1);
    if !lower.is_finite() || !upper.is_finite() || upper <= lower {
        return visible_rows;
    }
    let span = (upper - lower).ceil();
    if span >= i64::MAX as f64 {
        i64::MAX
    } else {
        (span as i64).max(visible_rows)
    }
}

/// Grow a read-only finished VTE from its provisional grid to every row VTE
/// actually rendered. This is intentionally based on the terminal buffer after
/// `feed()`, not a string-width estimate: ANSI controls, wide/combining glyphs,
/// tabs, carriage-return redraws and automatic wrapping are all already resolved
/// by VTE at this point.
pub(crate) fn expand_finished_terminal_to_buffer(terminal: &Terminal) {
    let visible_rows = terminal.row_count().max(1);
    let rows = terminal
        .vadjustment()
        .map(|adj| finished_buffer_rows_from_adjustment(adj.lower(), adj.upper(), visible_rows))
        .unwrap_or(visible_rows);
    let cols = terminal.column_count().max(1);

    if rows > visible_rows {
        terminal.set_size(cols, rows);
    }
    // Keep the capture capacity armed after expansion. The configured value is
    // only a limit, so unused rows do not create an inner scroll range. More
    // importantly, an older idle-settling callback can no longer clear the
    // scrollback needed by a newer filter render before VTE has processed it.

    let cell_height = (terminal.char_height() as i32).max(1);
    let rows_i32 = rows.clamp(1, i32::MAX as i64) as i32;
    terminal.set_height_request(rows_i32.saturating_mul(cell_height));

    if let Some(adj) = terminal.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// Settle a finished snapshot after bytes have been fed. VTE updates its grid
/// and adjustment asynchronously, so use two idle passes: the first folds any
/// overflow/soft-wrapped rows into the widget, and the second observes any
/// adjustment changes caused by that resize. Capture capacity remains armed so
/// overlapping filter renders cannot invalidate one another.
pub(crate) fn settle_finished_terminal_after_feed(terminal: &Terminal) {
    let terminal = terminal.clone();
    glib::idle_add_local_once(move || {
        expand_finished_terminal_to_buffer(&terminal);
        let terminal = terminal.clone();
        glib::idle_add_local_once(move || {
            expand_finished_terminal_to_buffer(&terminal);
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

    #[test]
    fn sgr_wheel_up_encodes_button_64() {
        // delta_y < 0 → wheel up → button 64 (xterm convention).
        let seq = encode_mouse_wheel(MouseReportingMode::Sgr, -1.0, 10, 5).unwrap();
        assert_eq!(seq, b"\x1b[<64;10;5M");
    }

    #[test]
    fn sgr_wheel_down_encodes_button_65() {
        let seq = encode_mouse_wheel(MouseReportingMode::Sgr, 1.0, 1, 1).unwrap();
        assert_eq!(seq, b"\x1b[<65;1;1M");
    }

    #[test]
    fn x10_wheel_up_uses_value_plus_32() {
        // Legacy mode: each field encoded as byte = value + 32.
        let seq = encode_mouse_wheel(MouseReportingMode::Button, -1.0, 1, 1).unwrap();
        assert_eq!(seq, vec![0x1b, b'[', b'M', 64 + 32, 1 + 32, 1 + 32]);
    }

    #[test]
    fn none_mode_returns_no_bytes() {
        assert!(encode_mouse_wheel(MouseReportingMode::None, -1.0, 1, 1).is_none());
    }

    #[test]
    fn zero_delta_returns_no_bytes() {
        // Spurious 0 delta from GTK shouldn't paginate the app.
        assert!(encode_mouse_wheel(MouseReportingMode::Sgr, 0.0, 1, 1).is_none());
    }

    #[test]
    fn finished_buffer_rows_include_wrapped_scrollback() {
        assert_eq!(finished_buffer_rows_from_adjustment(-7.0, 5.0, 5), 12);
    }

    #[test]
    fn finished_buffer_rows_never_shrink_the_provisional_grid() {
        assert_eq!(finished_buffer_rows_from_adjustment(0.0, 1.0, 5), 5);
        assert_eq!(finished_buffer_rows_from_adjustment(f64::NAN, 1.0, 3), 3);
    }
}

// ─── VTE builder ─────────────────────────────────────────────────────────────

/// Apply colors + font + font scale from `config` onto an existing Terminal.
/// Single source of truth for VTE theming so the live VTE and read-only
/// finished-block VTEs stay visually identical.
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
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));
    terminal.set_font_scale(config.default_font_scale);
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
    if let Ok(regex) = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    ) {
        terminal.match_add_regex(&regex, 0);
    }
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
    let visible_rows = output_rows.min(viewport_cap).max(1);
    // The caller's estimate can be too small (most notably a long single-line
    // command that wraps). Keep enough temporary scrollback to retain those rows
    // until the post-feed expansion below makes them part of the widget itself.
    // VTE treats this as a limit and allocates used rows, not a 50k-row grid.
    let capture_rows = output_rows
        .max(viewport_cap)
        .max(config.truncation_threshold_lines as i64)
        .max(4096)
        .clamp(1, u32::MAX as i64) as u32;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(false)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(false)
        .scrollback_lines(capture_rows)
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .cell_height_scale(BLOCK_CELL_HEIGHT_SCALE)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .scroll_on_output(false)
        .scroll_on_keystroke(false)
        .build();
    terminal.set_mouse_autohide(true);
    apply_snapshot_theme_to_vte(&terminal, config);
    terminal.set_size(cols.max(1), visible_rows);

    // `blocks.rs` performs the actual feed from its map/filter paths. Keep
    // one constructor-level hook for command snapshots and other one-shot users;
    // every explicit re-feed also calls the same settling helper.
    if expand_to_buffer {
        let expanded = std::cell::Cell::new(false);
        terminal.connect_map(move |terminal| {
            if expanded.replace(true) {
                return;
            }
            settle_finished_terminal_after_feed(terminal);
        });
    }

    // VTE consumes wheel events even when its private scrollback is empty. The
    // output surface already has explicit outer-scroll forwarding in blocks.rs;
    // add the same behavior to the command surface, identified by its horizontal
    // prompt-row parent, so scrolling never sticks while the pointer is over it.
    {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let terminal_for_scroll = terminal.clone();
        scroll.connect_scroll(move |_, _dx, dy| {
            if forward_command_surface_scroll(&terminal_for_scroll, dy) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        terminal.add_controller(scroll);
    }

    // URL detection — mirror the live-VTE pattern at src/terminal.rs:52-56.
    if let Ok(regex) = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    ) {
        terminal.match_add_regex(&regex, 0);
    }
    terminal
}
