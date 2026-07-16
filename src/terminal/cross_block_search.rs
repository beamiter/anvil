//! Cross-block ripgrep-style search dialog for Block panes.
//!
//! The scan and jump implementation lives in `block_view::find`; this module
//! only exposes it through a GTK dialog while keeping the Relm4 app model
//! backend-neutral.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use std::cell::RefCell;
use std::rc::Rc;

use crate::block_view::{CrossBlockHit, TermView};

pub(super) fn toggle(view: Rc<TermView>, dialog_slot: Rc<RefCell<Option<adw::Dialog>>>) {
    let open_dialog = { dialog_slot.borrow_mut().take() };
    if let Some(dialog) = open_dialog {
        dialog.force_close();
        return;
    }

    let dialog = adw::Dialog::builder()
        .title("Search Blocks (ripgrep)")
        .content_width(720)
        .content_height(520)
        .build();

    let header_bar = adw::HeaderBar::new();
    let regex_toggle = gtk::ToggleButton::builder()
        .label(".*")
        .tooltip_text("Treat the query as a regular expression")
        .build();
    header_bar.pack_end(&regex_toggle);

    let filter_entry = gtk::SearchEntry::new();
    filter_entry.set_placeholder_text(Some("Search across blocks…"));
    filter_entry.set_hexpand(true);
    filter_entry.set_margin_start(12);
    filter_entry.set_margin_end(12);
    filter_entry.set_margin_top(8);
    filter_entry.set_margin_bottom(8);

    let list_box = gtk::ListBox::new();
    list_box.set_selection_mode(gtk::SelectionMode::Single);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    list_box.set_margin_bottom(12);

    let status_label = gtk::Label::new(Some("Type to search across blocks."));
    status_label.add_css_class("dim-label");
    status_label.set_xalign(0.0);
    status_label.set_margin_start(12);
    status_label.set_margin_end(12);
    status_label.set_margin_bottom(6);

    let scrolled = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&list_box)
        .build();
    let search_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    search_box.append(&filter_entry);
    search_box.append(&status_label);
    search_box.append(&scrolled);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header_bar);
    toolbar_view.set_content(Some(&search_box));
    dialog.set_child(Some(&toolbar_view));

    let hits: Rc<RefCell<Vec<CrossBlockHit>>> = Rc::new(RefCell::new(Vec::new()));
    let rebuild = {
        let view = view.clone();
        let list_box = list_box.clone();
        let hits = hits.clone();
        let status_label = status_label.clone();
        let filter_entry = filter_entry.clone();
        let regex_toggle = regex_toggle.clone();
        Rc::new(move || {
            let query = filter_entry.text().to_string();
            let is_regex = regex_toggle.is_active();
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            if query.is_empty() {
                hits.borrow_mut().clear();
                status_label.set_text("Type to search across blocks.");
                return;
            }

            match view.cross_block_search(&query, is_regex, 500) {
                Ok(results) => {
                    let total = results.len();
                    status_label.set_text(match total {
                        0 => "No matches.",
                        500 => "500 matches (capped) — refine your query.",
                        _ => "",
                    });
                    if total > 0 && total < 500 {
                        status_label.set_text(&format!("{total} matches"));
                    }
                    for hit in &results {
                        let surface = if hit.is_output { "out" } else { "cmd" };
                        let subtitle = format!(
                            "{surface} L{}: {}",
                            hit.line_no,
                            gtk::glib::markup_escape_text(&hit.line_text)
                        );
                        let title = gtk::glib::markup_escape_text(&hit.cmd_preview);
                        let row = adw::ActionRow::builder()
                            .title(title.as_str())
                            .subtitle(&subtitle)
                            .activatable(true)
                            .build();
                        list_box.append(&row);
                    }
                    *hits.borrow_mut() = results;
                    list_box.select_row(list_box.row_at_index(0).as_ref());
                }
                Err(error) => {
                    hits.borrow_mut().clear();
                    status_label.set_text(&format!("Bad regex: {error}"));
                }
            }
        })
    };

    {
        let rebuild = rebuild.clone();
        filter_entry.connect_search_changed(move |_| rebuild());
    }
    {
        let rebuild = rebuild.clone();
        regex_toggle.connect_toggled(move |_| rebuild());
    }

    let jump = {
        let view = view.clone();
        let hits = hits.clone();
        let filter_entry = filter_entry.clone();
        let regex_toggle = regex_toggle.clone();
        Rc::new(move |index: usize| {
            let Some(hit) = hits.borrow().get(index).cloned() else {
                return;
            };
            let pattern = filter_entry.text().to_string();
            let is_regex = regex_toggle.is_active();
            if view.scroll_to_block_id(hit.block_id) {
                view.focus_match_in_block(hit.block_id, &pattern, is_regex, hit.is_output);
            }
        })
    };

    {
        let jump = jump.clone();
        let dialog = dialog.clone();
        list_box.connect_row_activated(move |_, row| {
            jump(row.index() as usize);
            dialog.force_close();
        });
    }

    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let dialog = dialog.clone();
        let list_box = list_box.clone();
        let jump = jump.clone();
        key_controller.connect_key_pressed(move |_, key, _, state| {
            use gtk::gdk::{Key, ModifierType};
            if key == Key::Escape
                || (matches!(key, Key::g | Key::G)
                    && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
            {
                dialog.force_close();
                return gtk::glib::Propagation::Stop;
            }
            if matches!(key, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box.selected_row() {
                    jump(row.index() as usize);
                    dialog.force_close();
                }
                return gtk::glib::Propagation::Stop;
            }
            let delta = match key {
                Key::Down => 1,
                Key::Up => -1,
                _ => return gtk::glib::Propagation::Proceed,
            };
            let current = list_box.selected_row().map(|row| row.index()).unwrap_or(0);
            let next = (current + delta).max(0);
            if let Some(row) = list_box.row_at_index(next) {
                list_box.select_row(Some(&row));
            }
            gtk::glib::Propagation::Stop
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
    let parent = view.widget();
    dialog.present(Some(&parent));
    filter_entry.grab_focus();
}
