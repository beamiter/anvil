//! Real widget coverage; run in an isolated display-backed test process.
use super::*;
use std::time::{Duration, Instant};

fn settle() {
    let context = gtk::glib::MainContext::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(350) {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn search_entry(widget: &gtk::Widget) -> Option<gtk::SearchEntry> {
    if let Ok(entry) = widget.clone().downcast::<gtk::SearchEntry>() {
        return Some(entry);
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(entry) = search_entry(&widget) {
            return Some(entry);
        }
        child = widget.next_sibling();
    }
    None
}

fn preview_label(widget: &gtk::Widget) -> Option<gtk::Label> {
    if widget.has_css_class("block-output-preview") {
        return widget.clone().downcast::<gtk::Label>().ok();
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(label) = preview_label(&widget) {
            return Some(label);
        }
        child = widget.next_sibling();
    }
    None
}

#[test]
#[ignore = "requires DISPLAY; run explicitly under Xvfb"]
fn folded_preview_tracks_filters_and_preserves_output() {
    gtk::init().expect("GTK display");
    let config = Config::safe_defaults();
    let output = "keep visible match\n\x1b[31mprivate final\x1b[0m\u{202e}\n";
    let block = FinishedBlock::new(
        101,
        "$ ",
        "cat example.log",
        None,
        output,
        Some(0),
        &config,
        None,
        None,
        None,
        80,
    );
    let window = gtk::Window::builder()
        .default_width(360)
        .default_height(320)
        .child(block.widget())
        .build();
    window.present();
    settle();
    block.set_collapsed(true);
    settle();
    let preview = preview_label(block.widget().upcast_ref()).expect("preview label");
    assert_eq!(preview.text(), "Preview: private final�");
    assert!(preview.get_visible());
    assert_eq!(preview.ellipsize(), gtk::pango::EllipsizeMode::End);
    assert_eq!(preview.width_chars(), 1);
    assert!(preview.width() <= window.width());
    assert!(preview.is_single_line_mode());
    (block.toggle_filter)();
    let entry = search_entry(block.widget().upcast_ref()).expect("block filter");
    entry.set_text("keep");
    settle();
    assert_eq!(preview.text(), "Preview: keep visible match");
    entry.set_text("no-such-result");
    settle();
    assert!(
        !preview.get_visible(),
        "no match must not expose original output"
    );
    entry.set_text("");
    settle();
    assert_eq!(preview.text(), "Preview: private final�");
    assert!(preview.get_visible());
    block.set_collapsed(false);
    block.set_collapsed(true);
    assert_eq!(preview.text(), "Preview: private final�");
    assert_eq!(
        block.full_output.borrow().as_str(),
        output,
        "preview must not mutate copy source"
    );
    let empty = FinishedBlock::new(
        102,
        "$ ",
        "true",
        None,
        "",
        Some(0),
        &config,
        None,
        None,
        None,
        80,
    );
    let empty_preview = preview_label(empty.widget().upcast_ref()).expect("empty preview label");
    assert!(!empty_preview.get_visible());
    window.close();
    settle();
}
