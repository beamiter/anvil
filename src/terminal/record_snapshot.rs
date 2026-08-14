//! Read-only view of one completed record's bounded output snapshot.
//!
//! Retention and the snapshot payload live in `block_view::find`; this module
//! only presents them through a GTK dialog while keeping the Relm4 app model
//! backend-neutral, the same split `cross_block_search` uses. Deliberately not
//! a terminal surface — nothing here replays bytes or accepts input.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use std::cell::RefCell;
use std::rc::Rc;

use crate::block_view::TermView;

const DIALOG_TITLE: &str = "Output Snapshot";

/// Reported when the record no longer retains a snapshot: the global budget
/// can evict one between navigation and presentation, and an empty pane would
/// claim the command produced no output.
pub(super) const UNAVAILABLE_NOTICE: &str = "This record's output snapshot is no longer retained.";

/// Present the record's retained snapshot, returning the notice to report when
/// there is nothing to present.
pub(super) fn present(
    view: &Rc<TermView>,
    dialog_slot: &Rc<RefCell<Option<adw::Dialog>>>,
    record_id: u64,
) -> Option<&'static str> {
    let Some(snapshot) = view.record_snapshot_view(record_id) else {
        return Some(UNAVAILABLE_NOTICE);
    };
    // `force_close` emits `closed` synchronously, and that handler writes the
    // slot. The borrow must end before the call, or it re-enters this cell
    // inside a C signal trampoline, where the panic cannot unwind.
    let open_dialog = { dialog_slot.borrow_mut().take() };
    if let Some(open) = open_dialog {
        open.force_close();
    }

    let dialog = adw::Dialog::builder()
        .title(DIALOG_TITLE)
        .content_width(640)
        .content_height(480)
        .build();
    let header_bar = adw::HeaderBar::new();

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_top(12);
    body.set_margin_bottom(12);

    if !snapshot.cmd.is_empty() {
        let command = gtk::Label::new(Some(&snapshot.cmd));
        command.set_xalign(0.0);
        command.set_wrap(true);
        command.set_selectable(true);
        command.add_css_class("monospace");
        body.append(&command);
    }

    let status = gtk::Label::new(Some(&snapshot.status_line()));
    status.set_xalign(0.0);
    status.add_css_class("dim-label");
    body.append(&status);

    if let Some(note) = snapshot.truncation_note() {
        let note_label = gtk::Label::new(Some(&note));
        note_label.set_xalign(0.0);
        note_label.add_css_class("dim-label");
        body.append(&note_label);
    }

    let text = gtk::TextView::new();
    text.buffer().set_text(&snapshot.output);
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_monospace(true);
    text.set_left_margin(8);
    text.set_right_margin(8);
    text.set_top_margin(8);
    text.set_bottom_margin(8);
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&text)
        .build();
    body.append(&scrolled);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&body));
    dialog.set_child(Some(&toolbar_view));

    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let dialog = dialog.clone();
        key_controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dialog.force_close();
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }
    dialog.add_controller(key_controller);

    {
        let dialog_slot = dialog_slot.clone();
        dialog.connect_closed(move |_| {
            *dialog_slot.borrow_mut() = None;
        });
    }

    *dialog_slot.borrow_mut() = Some(dialog.clone());
    dialog.present(Some(&view.widget()));
    None
}
