use relm4::gtk;
// Content-free feedback for the clipboard encoder's lossy safety fallback.
use relm4::adw::prelude::*;
use std::rc::Rc;

pub(super) const PARTIAL: &str =
    "Only the first line was pasted. Multiline paste is unavailable in this terminal.";
pub(super) const EMPTY: &str = "Nothing was pasted. Multiline paste is unavailable and the first line contained no pasteable text.";

pub(super) const READ_FAILED: &str =
    "Could not read clipboard text. Nothing was pasted. Try copying the text again.";
pub(super) const NO_TEXT: &str = "The clipboard contains no text. Nothing was pasted.";

/// Report a read failure without displaying provider errors or clipboard data.
/// Keep nonempty text unchanged; in particular, whitespace is valid paste input.
pub(super) fn read_text<T: AsRef<str>, E>(
    result: Result<Option<T>, E>,
    notify: &dyn Fn(&str),
) -> Option<T> {
    match result {
        Ok(Some(text)) if !text.as_ref().is_empty() => Some(text),
        Ok(_) => {
            notify(NO_TEXT);
            None
        }
        Err(_) => {
            notify(READ_FAILED);
            None
        }
    }
}

/// Failed or empty reads must not release a selected streaming surface.
/// Replay a successful read's parked output before delivery, so the sink sees
/// the latest shell/paste mode. This does not bypass its admission checks.
pub(super) fn read_text_before_paste<T: AsRef<str>, E>(
    result: Result<Option<T>, E>,
    notify: &dyn Fn(&str),
    before_paste: impl FnOnce(),
) -> Option<T> {
    let text = read_text(result, notify)?;
    before_paste();
    Some(text)
}

pub(super) fn report(notify: &dyn Fn(&str), truncated: bool, delivered: bool) {
    if truncated {
        notify(if delivered { PARTIAL } else { EMPTY });
    }
}

/// Delivery admission failures belong to the original pane, never a late modal.
pub(super) fn report_refusal(notify: &dyn Fn(&str), reason: &str) {
    notify(&format!("Paste failed: {reason}. Nothing was pasted."));
}

/// Resolve only the original, still-visible pane. Toasts contain no clipboard
/// content, have no action button, and never acquire keyboard focus.
pub(super) fn notifier(target: &impl IsA<gtk::Widget>) -> Rc<dyn Fn(&str)> {
    let target = target.as_ref().downgrade();
    Rc::new(move |message| {
        let Some(target) = target.upgrade().filter(|widget| widget.is_mapped()) else {
            return;
        };
        let mut ancestor = target.parent();
        while let Some(widget) = ancestor {
            if let Ok(overlay) = widget.clone().downcast::<relm4::adw::ToastOverlay>() {
                overlay.add_toast(relm4::adw::Toast::new(message));
                return;
            }
            ancestor = widget.parent();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_read_failure_is_content_free() {
        let messages = std::cell::RefCell::new(Vec::new());
        let result = read_text::<String, _>(Err("provider private details"), &|message| {
            messages.borrow_mut().push(message.to_owned());
        });
        assert!(result.is_none());
        assert_eq!(*messages.borrow(), [READ_FAILED]);
        assert!(!messages.borrow()[0].contains("provider private details"));
    }

    #[test]
    fn clipboard_missing_and_empty_text_have_distinct_feedback_from_read_failure() {
        for result in [Ok(None), Ok(Some(String::new()))] {
            let messages = std::cell::RefCell::new(Vec::new());
            assert!(read_text::<String, ()>(result, &|message| {
                messages.borrow_mut().push(message.to_owned());
            })
            .is_none());
            assert_eq!(*messages.borrow(), [NO_TEXT]);
            assert_ne!(NO_TEXT, READ_FAILED);
        }
    }

    #[test]
    fn clipboard_read_preserves_whitespace_unicode_and_line_endings() {
        for text in [" ", "\t", "\r\n", "中文\r\nsecond\n", "\u{202e}text"] {
            let result = read_text::<_, ()>(Ok(Some(text.to_owned())), &|_| {
                panic!("successful nonempty read must not report a failure");
            });
            assert_eq!(result.as_deref(), Some(text));
        }
    }

    #[test]
    fn failed_or_empty_clipboard_read_keeps_streaming_selection_held() {
        for result in [Err(()), Ok(None), Ok(Some(String::new()))] {
            let replay_count = std::cell::Cell::new(0);
            let result = read_text_before_paste(result, &|_| {}, || {
                replay_count.set(replay_count.get() + 1);
            });
            assert!(result.is_none());
            assert_eq!(replay_count.get(), 0);
        }
    }

    #[test]
    fn successful_clipboard_read_replays_before_delivery_even_for_whitespace() {
        for text in [" ", "\r\n", "中文"] {
            let order = std::cell::RefCell::new(Vec::new());
            let received = read_text_before_paste::<_, ()>(Ok(Some(text)), &|_| {}, || {
                order.borrow_mut().push("replay");
            });
            assert_eq!(received, Some(text));
            order.borrow_mut().push("deliver");
            assert_eq!(*order.borrow(), ["replay", "deliver"]);
        }
    }

    #[test]
    fn refusal_keeps_admission_reason_and_reports_no_delivery() {
        let messages = std::cell::RefCell::new(Vec::new());
        report_refusal(
            &|message| messages.borrow_mut().push(message.to_owned()),
            "Input queue is full",
        );
        assert_eq!(
            *messages.borrow(),
            ["Paste failed: Input queue is full. Nothing was pasted."]
        );
    }

    #[test]
    #[ignore = "requires DISPLAY; verifies passive paste feedback"]
    fn notice_preserves_focus() {
        relm4::adw::init().expect("gtk display");
        for message in [
            PARTIAL,
            READ_FAILED,
            NO_TEXT,
            "Paste failed: Input queue is full. Nothing was pasted.",
        ] {
            let target = gtk::Entry::new();
            let newer = gtk::Entry::new();
            let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
            column.append(&target);
            column.append(&newer);
            let overlay = relm4::adw::ToastOverlay::new();
            overlay.set_child(Some(&column));
            let window = gtk::Window::builder().child(&overlay).build();
            window.present();
            while gtk::glib::MainContext::default().iteration(false) {}
            assert!(target.is_mapped());
            assert!(newer.grab_focus());
            let before = gtk::prelude::GtkWindowExt::focus(&window);
            let notify = notifier(&target);
            assert!(before.is_some());
            target.set_visible(false);
            assert!(!target.is_mapped());
            report_refusal(notify.as_ref(), "Late refusal after switching tabs");
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(200) {
                while gtk::glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(!has_notice(
                overlay.upcast_ref(),
                "Paste failed: Late refusal after switching tabs. Nothing was pasted."
            ));
            assert_eq!(gtk::prelude::GtkWindowExt::focus(&window), before);
            target.set_visible(true);
            while gtk::glib::MainContext::default().iteration(false) {}
            assert!(target.is_mapped());
            if message.starts_with("Paste failed:") {
                report_refusal(notify.as_ref(), "Input queue is full");
            } else {
                notify(message);
            }
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(200) {
                while gtk::glib::MainContext::default().iteration(false) {}
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            fn has_notice(widget: &gtk::Widget, expected: &str) -> bool {
                if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                    if label.text() == expected {
                        return true;
                    }
                }
                let mut child = widget.first_child();
                while let Some(widget) = child {
                    if has_notice(&widget, expected) {
                        return true;
                    }
                    child = widget.next_sibling();
                }
                false
            }
            assert!(
                has_notice(overlay.upcast_ref(), message),
                "the toast is actually rendered"
            );
            assert_eq!(gtk::prelude::GtkWindowExt::focus(&window), before);
            window.close();
            notify(EMPTY); // detached/hidden owners never create a late dialog
        }
    }
}
