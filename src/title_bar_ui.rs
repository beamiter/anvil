//! GTK renderer for the family title row. Tabs and actions occupy the row below.
use gtk::prelude::*;
use jterm_core::title_bar::Content;
use relm4::gtk;
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) struct TitleBar {
    pub root: gtk::Box,
    heading: gtk::Label,
    details: gtk::Label,
    content: Rc<RefCell<Content>>,
}

impl TitleBar {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        root.add_css_class("top-bar");
        root.add_css_class("workspace-title-bar");
        root.set_height_request(32);
        let heading = gtk::Label::new(None);
        heading.set_hexpand(true);
        heading.set_xalign(0.0);
        heading.set_margin_start(10);
        heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
        heading.set_width_chars(1);
        heading.add_css_class("heading");
        let details = gtk::Label::new(None);
        details.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        details.set_max_width_chars(72);
        details.add_css_class("dim-label");
        root.append(&heading);
        root.append(&details);
        let content = Rc::new(RefCell::new(Content::default()));
        let last_width = std::cell::Cell::new(-1);
        let state = content.clone();
        let label = details.clone();
        root.add_tick_callback(move |root, _| {
            let width = root.width();
            if last_width.replace(width) != width {
                label.set_label(state.borrow().details_for_width(width as f32));
            }
            gtk::glib::ControlFlow::Continue
        });
        Self {
            root,
            heading,
            details,
            content,
        }
    }

    pub fn update(&self, content: Content) {
        if *self.content.borrow() == content {
            return;
        }
        self.heading.set_label(&content.heading);
        self.details
            .set_label(content.details_for_width(self.root.width() as f32));
        self.heading.set_tooltip_text(Some(&content.tooltip));
        self.details.set_tooltip_text(Some(&content.tooltip));
        *self.content.borrow_mut() = content;
    }
}
