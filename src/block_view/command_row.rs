//! Keep command width independent of VTE's last allocated grid.
use gtk::{glib, prelude::*, subclass::prelude::*};
use relm4::gtk;
use std::cell::Cell;
use vte4::{Terminal, TerminalExt};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct CommandRow {
        pub content: gtk::Box,
        pub terminal: glib::WeakRef<Terminal>,
        pub columns: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CommandRow {
        const NAME: &'static str = "AnvilChatCommandRow";
        type Type = super::CommandRow;
        type ParentType = gtk::Widget;

        fn class_init(class: &mut Self::Class) {
            class.set_css_name("box");
        }
    }

    impl ObjectImpl for CommandRow {
        fn constructed(&self) {
            self.parent_constructed();
            self.content.set_parent(&*self.obj());
        }

        fn dispose(&self) {
            if self.content.parent().is_some() {
                self.content.unparent();
            }
        }
    }

    // This outer widget deliberately has no layout manager: GtkBoxLayout
    // would bypass measure() and request VTE's previous full-pane grid again.
    // The inner box still owns native child allocation, focus and hit testing.
    impl WidgetImpl for CommandRow {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.content.request_mode()
        }

        // GTK still exposes native terminal padding/borders through this API.
        #[allow(deprecated)]
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let measured = self.content.measure(orientation, for_size);
            let Some(terminal) = self.terminal.upgrade() else {
                return measured;
            };
            if orientation != gtk::Orientation::Horizontal || self.columns.get() == 0 {
                return measured;
            }
            // Preserve the prompt chevron and CSS insets, replacing only the
            // terminal's grid-dependent width with the command's text width.
            let (minimum, natural, ..) = terminal.measure(orientation, for_size);
            let style = terminal.style_context();
            let padding = style.padding();
            let border = style.border();
            let insets =
                i32::from(padding.left() + padding.right() + border.left() + border.right())
                    + terminal.margin_start()
                    + terminal.margin_end();
            let cell = terminal.char_width().max(1) as i32;
            let target = (self.columns.get().min(i32::MAX as i64) as i32)
                .saturating_mul(cell)
                .saturating_add(insets);
            let min = measured.0.saturating_sub(minimum) + cell.saturating_mul(2) + insets;
            let nat = measured.1.saturating_sub(natural).saturating_add(target);
            (min, nat.max(min), -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.content.allocate(width, height, baseline, None);
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().snapshot_child(&self.content, snapshot);
        }

        fn focus(&self, direction: gtk::DirectionType) -> bool {
            self.content.child_focus(direction)
        }
    }
}

glib::wrapper! {
    pub struct CommandRow(ObjectSubclass<imp::CommandRow>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl CommandRow {
    pub(super) fn new() -> Self {
        glib::Object::new()
    }

    pub(super) fn append(&self, child: &impl IsA<gtk::Widget>) {
        self.imp().content.append(child);
    }
}

pub(super) fn fit(row: &CommandRow, terminal: &Terminal, columns: i64) {
    row.imp().terminal.set(Some(terminal));
    row.imp().columns.set(columns.max(2));
    row.set_halign(gtk::Align::End);
    row.set_hexpand(false);
    row.set_overflow(gtk::Overflow::Hidden);
    terminal.set_hexpand(false);
    let weak = row.downgrade();
    terminal.connect_char_size_changed(move |_, _, _| {
        if let Some(row) = weak.upgrade() {
            row.queue_resize();
        }
    });
    row.queue_resize();
}
