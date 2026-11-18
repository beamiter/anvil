use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use unicode_width::UnicodeWidthStr;
use vte4::{Format, Terminal, TerminalExt};

use crate::delimited_reference::{self, Reference};

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

fn detect(terminal: &Terminal, lines: &[String]) -> Vec<Reference> {
    let cols = terminal.column_count().max(1) as usize;
    let rows: Vec<_> = lines
        .iter()
        .map(|text| {
            (
                text.as_str(),
                text.trim_end().width() >= cols.saturating_sub(1),
            )
        })
        .collect();
    delimited_reference::detect(&rows)
}

pub(crate) fn hit(terminal: &Terminal, x: f64, y: f64) -> Option<Reference> {
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
    detect(terminal, &lines).into_iter().find(|reference| {
        reference.spans.iter().any(|(row, bytes)| {
            *row == selected_row
                && (lines[*row][..bytes.start].width()..lines[*row][..bytes.end].width())
                    .contains(&col)
        })
    })
}

pub(crate) fn web_match(terminal: &Terminal, x: f64, y: f64) -> Option<String> {
    if let Some(reference) = hit(terminal, x, y) {
        return reference.web.then_some(reference.target);
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
    let selected = terminal.text_selected(Format::Text)?.to_string();
    if selected.len() > 4096 || !selected.trim_end_matches('\n').contains('\n') {
        return Some(selected);
    }
    let Some(adjustment) = terminal.vadjustment() else {
        return Some(selected);
    };
    let first = adjustment.value().floor() as i64;
    let last = (first + terminal.row_count()).min(adjustment.upper().ceil() as i64 - 1);
    let lines = rows(terminal, first, last);
    let fragments: Vec<_> = selected
        .trim_end_matches('\n')
        .split('\n')
        .map(str::trim)
        .collect();
    for reference in detect(terminal, &lines) {
        if (!reference.web || jterm_core::link::is_openable_url(&reference.target))
            && reference
                .spans
                .iter()
                .map(|(row, span)| &lines[*row][span.clone()])
                .eq(fragments.iter().copied())
        {
            return Some(reference.target);
        }
    }
    Some(selected)
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
                !reference.web || jterm_core::link::is_openable_url(&reference.target)
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
    fn mapped_vte_resolves_both_fragments_of_a_codex_reference() {
        let exercise_pointer = std::env::var_os("JTERM_TEST_VTE_POINTER").is_some();
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
            .map(|reference| reference.target)
        };
        let expected = Some("/tmp/中 文/PMG-SOTA-roadmap.canvas.tsx".into());
        assert_eq!(hit_col(0.0, 12.0), expected);
        assert_eq!(hit_col(1.0, 8.0), expected);
        assert_eq!(hit_col(1.0, 2.0), None);
        if exercise_pointer {
            let target = Rc::new(RefCell::new(None));
            let activated = target.clone();
            let weak = terminal.downgrade();
            click(
                &terminal,
                move |x, y| hit(&weak.upgrade()?, x, y).map(|reference| reference.target),
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
            assert_eq!(
                selected_text(&terminal),
                expected,
                "raw VTE selection: {:?}",
                terminal.text_selected(Format::Text),
            );
        }
        window.close();
    }
}
