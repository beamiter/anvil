//! Keep command width independent of VTE's last allocated grid.
use gtk::{glib, prelude::*, subclass::prelude::*};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use vte4::{Terminal, TerminalExt};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct CommandRow {
        pub content: gtk::Box,
        pub terminal: glib::WeakRef<Terminal>,
        pub columns: Cell<i64>,
        pub command: RefCell<String>,
        pub allocated_width: Cell<i32>,
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
            // VTE is a fixed-grid child. Advertising HFW while it still has
            // the preceding grid makes GTK reverse-solve a window minimum
            // from that stale height. Use the last real allocation instead.
            self.content.request_mode()
        }

        // GTK still exposes native terminal padding/borders through this API.
        #[allow(deprecated)]
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let measured = self.content.measure(orientation, for_size);
            let Some(terminal) = self.terminal.upgrade() else {
                return measured;
            };
            if self.columns.get() == 0 {
                return measured;
            }
            if orientation == gtk::Orientation::Vertical {
                let cell = terminal.char_width().max(1) as i32;
                let allocated = self.allocated_width.get();
                let width = if allocated > 0 { allocated } else { for_size };
                let other = self
                    .content
                    .measure(gtk::Orientation::Horizontal, -1)
                    .0
                    .saturating_sub(terminal.measure(gtk::Orientation::Horizontal, -1).0);
                let columns = if width > 0 {
                    self.columns
                        .get()
                        .min(i64::from(((width - other).max(0) / cell).max(2)))
                } else {
                    self.columns.get()
                };
                let rows =
                    super::super::blocks::output_visual_row_count(&self.command.borrow(), columns);
                let height = super::super::alt_screen::finished_vte_height_px(
                    rows,
                    terminal.char_height().max(1) as i32,
                ) + super::super::blocks::terminal_vertical_insets(&terminal);
                return (height, height, -1, -1);
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
            let width_changed = self.allocated_width.replace(width) != width;
            if width_changed {
                if let Some(terminal) = self.terminal.upgrade() {
                    let cell = terminal.char_width().max(1) as i32;
                    let other = self
                        .content
                        .measure(gtk::Orientation::Horizontal, -1)
                        .0
                        .saturating_sub(terminal.measure(gtk::Orientation::Horizontal, -1).0);
                    let columns = self
                        .columns
                        .get()
                        .min(i64::from(((width - other).max(0) / cell).max(2)));
                    let rows = super::super::blocks::output_visual_row_count(
                        &self.command.borrow(),
                        columns,
                    );
                    // This is a read-only snapshot. Reflow its native grid
                    // before the child box queries the preceding narrow grid.
                    terminal.set_size(columns, rows.max(1));
                }
            }
            if width_changed {
                // set_size invalidates GTK's cached measurement. Refresh it
                // before allocation instead of allocating an unmeasured box.
                self.content.measure(gtk::Orientation::Horizontal, height);
                self.content.measure(gtk::Orientation::Vertical, width);
            }
            self.content.allocate(width, height, baseline, None);
            if width_changed {
                self.obj().queue_resize();
            }
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

pub(super) fn fit(row: &CommandRow, terminal: &Terminal, columns: i64, command: &str) {
    row.imp().terminal.set(Some(terminal));
    row.imp().columns.set(columns.max(2));
    *row.imp().command.borrow_mut() = command.to_owned();
    // Keep short commands content-sized on the same reading edge as output.
    // Long commands still consume the available width and reflow natively.
    row.set_halign(gtk::Align::Start);
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
