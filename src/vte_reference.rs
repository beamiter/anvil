use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use unicode_width::UnicodeWidthStr;
use vte4::{Format, Terminal, TerminalExt};

use jterm_core::link::{detect_delimited_references, DelimitedReference};

use relm4::gtk as gtk4;

const CONTEXT_ROWS: i64 = 7;

fn rows(terminal: &Terminal, first: i64, last: i64) -> Vec<String> {
    (first..=last)
        .map(|row| {
            terminal
                .text_range_format(Format::Text, row, 0, row, terminal.column_count())
                .0
                .map_or_else(String::new, |text| text.to_string())
        })
        .collect()
}

pub(crate) fn hit(terminal: &Terminal, x: f64, y: f64) -> Option<DelimitedReference> {
    let border = terminal.border();
    let (left, top) = border.map_or((0.0, 0.0), |border| {
        (f64::from(border.left()), f64::from(border.top()))
    });
    let dx = (x - left) / terminal.char_width().max(1) as f64;
    let dy = (y - top) / terminal.char_height().max(1) as f64;
    if dx < 0.0
        || dx >= terminal.column_count() as f64
        || dy < 0.0
        || dy >= terminal.row_count() as f64
    {
        return None;
    }
    let adjustment = terminal.vadjustment()?;
    let row = (adjustment.value() + dy).floor() as i64;
    let first = (row - CONTEXT_ROWS).max(adjustment.lower() as i64);
    let last = (row + CONTEXT_ROWS).min(adjustment.upper().ceil() as i64 - 1);
    let lines = rows(terminal, first, last);
    let selected_row = (row - first) as usize;
    let col = dx.floor() as usize;
    let rows: Vec<_> = lines.iter().map(String::as_str).collect();
    detect_delimited_references(&rows).into_iter().find(|reference| {
        reference.spans.iter().any(|(row, bytes)| {
            *row == selected_row
                && (lines[*row][..bytes.start].width()..lines[*row][..bytes.end].width())
                    .contains(&col)
        })
    })
}

pub(crate) fn web_match(terminal: &Terminal, x: f64, y: f64) -> Option<String> {
    if let Some(reference) = hit(terminal, x, y) {
        return (reference.web && !reference.blocked).then_some(reference.target);
    }
    let text = terminal.check_match_at(x, y).0?;
    let mut url: &str = text.as_str();
    if url.ends_with(')') {
        let opens = url.bytes().filter(|&byte| byte == b'(').count();
        let closes = url.bytes().filter(|&byte| byte == b')').count();
        let trailing = url.bytes().rev().take_while(|&byte| byte == b')').count();
        url = &url[..url.len() - closes.saturating_sub(opens).min(trailing)];
    }
    Some(
        url.trim_end_matches(['.', ',', ';', '!', '"', '\''])
            .to_owned(),
    )
}

pub(crate) fn selected_text(terminal: &Terminal) -> Option<String> {
    // VTE owns selection boundaries and genuine soft-wrap handling. In
    // particular, do not trim or join hard-line fragments into another name.
    terminal
        .text_selected(Format::Text)
        .map(|text| text.to_string())
}

pub(crate) fn copy_selection(terminal: &Terminal) {
    if let Some(text) = selected_text(terminal) {
        terminal.clipboard().set_text(&text);
    }
}

/// Keep native drag selection available. Activation requires a release over
/// the same target, on the same widget, without crossing the drag threshold.
pub(crate) fn click<T: PartialEq + 'static>(
    widget: &impl IsA<gtk4::Widget>,
    hit: impl Fn(f64, f64) -> Option<T> + 'static,
    activate: impl Fn(T) + 'static,
) {
    let gesture = gtk4::GestureClick::new();
    gesture.set_button(1);
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let pending = Rc::new(RefCell::new(None));
    let hit = Rc::new(hit);
    let press = pending.clone();
    let press_hit = hit.clone();
    gesture.connect_pressed(move |gesture, count, x, y| {
        use gtk4::gdk::ModifierType as M;
        let state = gesture.current_event_state();
        let ctrl = state.contains(M::CONTROL_MASK)
            && !state.intersects(
                M::SHIFT_MASK | M::ALT_MASK | M::SUPER_MASK | M::HYPER_MASK | M::META_MASK,
            );
        let target = (count == 1 && ctrl).then(|| press_hit(x, y)).flatten();
        gesture.set_state(if target.is_some() {
            gtk4::EventSequenceState::Claimed
        } else {
            gtk4::EventSequenceState::Denied
        });
        *press.borrow_mut() = target.map(|target| (x, y, target));
    });
    let stopped = pending.clone();
    gesture.connect_stopped(move |_| {
        stopped.borrow_mut().take();
    });
    let cancelled = pending.clone();
    gesture.connect_cancel(move |_, _| {
        cancelled.borrow_mut().take();
    });
    gesture.connect_released(move |gesture, count, x, y| {
        let Some((px, py, target)) = pending.borrow_mut().take() else {
            return;
        };
        if count == 1
            && (x - px).hypot(y - py) <= 4.0
            && hit(x, y).is_some_and(|current| current == target)
        {
            activate(target);
            gesture.set_state(gtk4::EventSequenceState::Claimed);
        }
    });
    widget.add_controller(gesture);
}

pub(crate) fn install_hover(terminal: &Terminal) {
    terminal.set_word_char_exceptions("-_./~:+?&=#%@");
    let motion = gtk4::EventControllerMotion::new();
    let custom = Rc::new(std::cell::Cell::new(false));
    let previous = custom.clone();
    let weak = terminal.downgrade();
    motion.connect_motion(move |_, x, y| {
        let Some(terminal) = weak.upgrade() else {
            return;
        };
        let matched = terminal.check_hyperlink_at(x, y).is_none()
            && hit(&terminal, x, y).is_some_and(|reference| {
                !reference.blocked
                    && (!reference.web || jterm_core::link::is_openable_url(&reference.target))
            });
        if matched != previous.replace(matched) {
            terminal.set_cursor_from_name(matched.then_some("pointer"));
        }
    });
    let weak = terminal.downgrade();
    motion.connect_leave(move |_| {
        if custom.replace(false) {
            if let Some(terminal) = weak.upgrade() {
                terminal.set_cursor_from_name(None);
            }
        }
    });
    terminal.add_controller(motion);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle() {
        let context = gtk4::glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(180);
        while std::time::Instant::now() < deadline {
            while context.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    #[ignore = "requires an isolated X11 display"]
    fn mapped_url_rows_cannot_retarget_the_origin_or_bypass_osc8() {
        gtk4::init().expect("isolated display is required");
        let terminal = Terminal::new();
        // Match production live/block VTE construction: OSC8 is opt-in,
        // independent of installing the plain-text URL matcher below.
        terminal.set_allow_hyperlink(true);
        terminal.set_size(80, 4);
        crate::block_view::add_url_match_regex(&terminal);
        let window = gtk4::Window::new();
        window.set_child(Some(&terminal));
        window.present();
        terminal.feed(b"(https://safe. \r\n example/path)\r\n");
        settle();
        let border = terminal.border();
        let (left, top) = border.map_or((0.0, 0.0), |b| (f64::from(b.left()), f64::from(b.top())));
        let at = |row: f64, col: f64| {
            (
                left + (col + 0.5) * terminal.char_width() as f64,
                top + (row + 0.5) * terminal.char_height() as f64,
            )
        };
        for (row, col) in [(0.0, 5.0), (1.0, 4.0)] {
            let (x, y) = at(row, col);
            let reference = hit(&terminal, x, y).expect("must reserve both URL fragments");
            assert!(reference.web && reference.blocked);
            assert!(web_match(&terminal, x, y).is_none());
            assert!(crate::terminal::url::openable_link_at(&terminal, x, y).is_none());
        }
        terminal.select_all();
        let native = terminal.text_selected(Format::Text).unwrap().to_string();
        assert!(native.contains('\n'));
        assert_eq!(selected_text(&terminal), Some(native));
        terminal.unselect_all();
        terminal.feed(b"\x1b[2J\x1b[H(https://safe.example/path)\r\n");
        settle();
        let (x, y) = at(0.0, 5.0);
        assert_eq!(
            crate::terminal::url::openable_link_at(&terminal, x, y).as_deref(),
            Some("https://safe.example/path")
        );
        // Explicit targets retain priority, even over an ambiguous visible label.
        terminal.feed(b"\x1b[2J\x1b[H\x1b]8;;https://explicit.example/target\x1b\\(https://safe. \r\n example/path)\x1b]8;;\x1b\\\r\n");
        settle();
        assert_eq!(
            crate::terminal::url::openable_link_at(&terminal, x, y).as_deref(),
            Some("https://explicit.example/target")
        );
        window.close();
    }

    #[test]
    #[ignore = "requires an isolated X11 display and xdotool"]
    fn mapped_vte_blocks_ambiguous_paths_and_preserves_native_selection() {
        let exercise_pointer = true;
        if gtk4::init().is_err() {
            assert!(!exercise_pointer, "pointer test requires an X11 display");
            return;
        }
        let terminal = Terminal::new();
        terminal.set_size(80, 4);
        let window = gtk4::Window::new();
        window.set_decorated(false);
        window.set_title(Some("jterm-wrapped-reference-test"));
        window.set_child(Some(&terminal));
        window.present();
        terminal.feed("路线图 (/tmp/中 文/PMG-SOTA-\r\n    roadmap.canvas.tsx) \r\n".as_bytes());
        settle();
        let border = terminal.border();
        let (left, top) = border.map_or((0.0, 0.0), |border| {
            (f64::from(border.left()), f64::from(border.top()))
        });
        let hit_col = |row: f64, col: f64| {
            hit(
                &terminal,
                left + (col + 0.5) * terminal.char_width() as f64,
                top + (row + 0.5) * terminal.char_height() as f64,
            )
            .filter(|reference| !reference.blocked)
            .map(|reference| reference.target)
        };
        let expected = Some("/tmp/中 文/PMG-SOTA-roadmap.canvas.tsx".into());
        assert_eq!(hit_col(0.0, 12.0), None);
        assert_eq!(hit_col(1.0, 8.0), None);
        assert_eq!(hit_col(1.0, 2.0), None);
        // Positive control: the identical exact target on one physical row is
        // still available. Keep the same click coordinates for the OS fixture.
        terminal
            .feed("\x1b[2J\x1b[H\r\n    (/tmp/中 文/PMG-SOTA-roadmap.canvas.tsx) \r\n".as_bytes());
        settle();
        assert_eq!(hit_col(1.0, 8.0), expected);
        if exercise_pointer {
            let target = Rc::new(RefCell::new(None));
            let activated = target.clone();
            let weak = terminal.downgrade();
            click(
                &terminal,
                move |x, y| {
                    hit(&weak.upgrade()?, x, y)
                        .filter(|reference| !reference.blocked)
                        .map(|reference| reference.target)
                },
                move |path| *activated.borrow_mut() = Some(path),
            );
            let xdotool = |args: &[&str]| {
                let output = std::process::Command::new("xdotool")
                    .args(args)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{args:?}: {output:?}");
                String::from_utf8(output.stdout).unwrap()
            };
            let id = xdotool(&["search", "--name", "^jterm-wrapped-reference-test$"]);
            let id = id.lines().next().unwrap();
            xdotool(&["windowfocus", id]);
            settle();
            let x = (left + 8.5 * terminal.char_width() as f64)
                .floor()
                .to_string();
            let y = (top + 1.5 * terminal.char_height() as f64)
                .floor()
                .to_string();
            xdotool(&[
                "mousemove",
                "--window",
                id,
                &x,
                &y,
                "keydown",
                "Control_L",
                "mousedown",
                "1",
            ]);
            settle();
            assert!(target.borrow().is_none());
            xdotool(&["mouseup", "1", "keyup", "Control_L"]);
            settle();
            assert_eq!(target.borrow_mut().take(), expected);
            std::thread::sleep(std::time::Duration::from_millis(300));
            xdotool(&[
                "keydown",
                "Control_L",
                "mousedown",
                "1",
                "mousemove_relative",
                "50",
                "0",
                "mouseup",
                "1",
                "keyup",
                "Control_L",
            ]);
            settle();
            assert!(target.borrow().is_none());

            // Hard-line whitespace remains native selection data, never a
            // synthesized no-space path. No blocked region may activate.
            terminal.feed(
                "\x1b[2J\x1b[H路线图 (/tmp/中 文/PMG-SOTA- \r\n    roadmap.canvas.tsx) \r\n"
                    .as_bytes(),
            );
            settle();
            xdotool(&[
                "mousemove",
                "--window",
                id,
                &x,
                &y,
                "keydown",
                "Control_L",
                "mousedown",
                "1",
                "mouseup",
                "1",
                "keyup",
                "Control_L",
            ]);
            settle();
            assert!(target.borrow().is_none());
            let start_x = (left + 8.1 * terminal.char_width() as f64)
                .ceil()
                .to_string();
            let start_y = (top + 0.5 * terminal.char_height() as f64)
                .floor()
                .to_string();
            let end_x = (left + 21.9 * terminal.char_width() as f64)
                .floor()
                .to_string();
            xdotool(&[
                "mousemove",
                "--window",
                id,
                &start_x,
                &start_y,
                "mousedown",
                "1",
            ]);
            settle();
            xdotool(&["mousemove", "--window", id, &end_x, &y]);
            settle();
            xdotool(&["mouseup", "1"]);
            settle();
            let native = terminal
                .text_selected(Format::Text)
                .expect("drag must select native text")
                .to_string();
            assert!(
                native.contains('\n'),
                "fixture must select both hard lines: {native:?}"
            );
            assert_eq!(
                selected_text(&terminal),
                Some(native),
                "selection must remain exactly VTE-owned"
            );
        }
        window.close();
    }
}
