//! Native multiline input geometry. The reading space is a GTK margin and is
//! never inserted into the authored text or provider request.
use gtk::prelude::*;
use relm4::gtk;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

pub(crate) fn install(
    view: &gtk::TextView,
    scroll: &gtk::ScrolledWindow,
    minimum: i32,
    maximum: i32,
) {
    apply(view, scroll, minimum, maximum);
    let weak_view = view.downgrade();
    let weak_scroll = scroll.downgrade();
    // CSS fonts can change with theme/font zoom without a scale-factor notify.
    // Read actual metrics only while mapped; no layout or buffer replacement.
    gtk::glib::timeout_add_local(Duration::from_millis(500), move || {
        let (Some(view), Some(scroll)) = (weak_view.upgrade(), weak_scroll.upgrade()) else {
            return gtk::glib::ControlFlow::Break;
        };
        if view.is_mapped() {
            apply(&view, &scroll, minimum, maximum);
        }
        gtk::glib::ControlFlow::Continue
    });
    let queued = Rc::new(Cell::new(false));
    let weak_view = view.downgrade();
    view.buffer().connect_mark_set(move |_, _, mark| {
        if mark.name().as_deref() != Some("insert") || queued.replace(true) {
            return;
        }
        let weak_view = weak_view.clone();
        let queued = queued.clone();
        gtk::glib::idle_add_local_once(move || {
            queued.set(false);
            let Some(view) = weak_view.upgrade().filter(|view| view.has_focus()) else {
                return;
            };
            let height = view.height().max(1);
            let margin = (f64::from(line_height(&view)) / f64::from(height)).clamp(0.05, 0.4);
            view.scroll_to_mark(&view.buffer().get_insert(), margin, false, 0.0, 0.0);
        });
    });
}

fn line_height(view: &gtk::TextView) -> i32 {
    view.create_pango_layout(Some("Mg中文"))
        .pixel_size()
        .1
        .max(20)
}

fn apply(view: &gtk::TextView, scroll: &gtk::ScrolledWindow, minimum: i32, maximum: i32) {
    let line = line_height(view);
    let height = minimum.max(4 * line + 8);
    if view.top_margin() != 8 {
        view.set_top_margin(8);
    }
    if view.bottom_margin() != line {
        view.set_bottom_margin(line);
    }
    if view.height_request() != height {
        view.set_height_request(height);
    }
    // Raise max before min so GTK never observes a contradictory pair.
    let maximum = maximum.max(height);
    if scroll.max_content_height() != maximum {
        scroll.set_max_content_height(maximum);
    }
    if scroll.min_content_height() != height {
        scroll.set_min_content_height(height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a native GTK display"]
    fn workspace_v2_composer_has_three_lines_and_visual_caret_space() {
        gtk::init().expect("GTK display");
        let view = gtk::TextView::new();
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&view));
        view.buffer().set_text("第一行\nsecond line\nthird line");
        install(&view, &scroll, 104, 220);
        let window = gtk::Window::new();
        window.set_decorated(false);
        window.set_default_size(360, 1);
        window.set_child(Some(&scroll));
        window.present();
        let context = gtk::glib::MainContext::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while view.height() == 0 && std::time::Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
        let line = line_height(&view);
        assert!(view.height_request() - view.top_margin() - view.bottom_margin() >= 3 * line);
        assert!(view.bottom_margin() >= line);
        let third = view.iter_location(&view.buffer().iter_at_line(2).unwrap());
        let viewport = view.visible_rect();
        assert!(
            third.y() + third.height() + line <= viewport.y() + viewport.height(),
            "the rendered third line needs one visible reading line beneath it"
        );
        assert_eq!(
            view.buffer().text(
                &view.buffer().start_iter(),
                &view.buffer().end_iter(),
                false
            ),
            "第一行\nsecond line\nthird line"
        );
        window.close();
    }
}
