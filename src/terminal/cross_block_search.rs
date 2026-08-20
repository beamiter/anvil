//! Cross-block ripgrep-style search dialog for Block panes.
//!
//! The scan and jump implementation lives in `block_view::find`; this module
//! only exposes it through a GTK dialog while keeping the Relm4 app model
//! backend-neutral.

use relm4::adw;
use relm4::adw::prelude::*;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use crate::block_view::{CrossBlockHit, RecordNavigationResult, TermView};

use super::record_snapshot;

const CROSS_BLOCK_SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// What the palette does with one activated hit. Every arm of
/// [`RecordNavigationResult`] resolves to exactly one of these: a record the
/// view could reach, or could only show a snapshot of, always closes the
/// palette; only a record it can do neither for keeps it open with a status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JumpOutcome {
    Close,
    ShowSnapshot(u64),
    KeepOpen,
}

fn jump_outcome(result: RecordNavigationResult) -> JumpOutcome {
    match result {
        RecordNavigationResult::Navigated => JumpOutcome::Close,
        RecordNavigationResult::SnapshotView { record_id } => JumpOutcome::ShowSnapshot(record_id),
        RecordNavigationResult::LocationUnavailable | RecordNavigationResult::NoMatchingRecord => {
            JumpOutcome::KeepOpen
        }
    }
}

fn jump_unavailable_status() -> &'static str {
    "This result is searchable, but it has no terminal location and no retained output."
}

pub(super) fn toggle(
    view: Rc<TermView>,
    dialog_slot: Rc<RefCell<Option<adw::Dialog>>>,
    snapshot_slot: Rc<RefCell<Option<adw::Dialog>>>,
) {
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
    let pending_rebuild: Rc<RefCell<Option<gtk::glib::SourceId>>> = Rc::new(RefCell::new(None));
    let search_generation = Rc::new(Cell::new(0_u64));
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
                    let jumpable = view.jumpable_search_hits(&results);
                    for hit in &results {
                        let surface = if hit.is_output { "out" } else { "cmd" };
                        // A hit whose record has no location and no retained
                        // output says so before the user activates it.
                        let unreachable = if jumpable.contains(&(hit.block_id, hit.is_output)) {
                            ""
                        } else {
                            " — location unavailable"
                        };
                        let subtitle = format!(
                            "{surface} L{}: {}{unreachable}",
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

    let schedule_rebuild = {
        let pending_rebuild = pending_rebuild.clone();
        let search_generation = search_generation.clone();
        let rebuild = rebuild.clone();
        let hits = hits.clone();
        let list_box = list_box.clone();
        let status_label = status_label.clone();
        let filter_entry = filter_entry.clone();
        Rc::new(move || {
            let generation = search_generation.get().wrapping_add(1);
            search_generation.set(generation);
            if let Some(source) = pending_rebuild.borrow_mut().take() {
                source.remove();
            }

            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            hits.borrow_mut().clear();
            if filter_entry.text().is_empty() {
                status_label.set_text("Type to search across blocks.");
                return;
            }
            status_label.set_text("Searching blocks…");

            let pending_slot = pending_rebuild.clone();
            let pending_clear = pending_rebuild.clone();
            let search_generation = search_generation.clone();
            let rebuild = rebuild.clone();
            let source = gtk::glib::timeout_add_local(CROSS_BLOCK_SEARCH_DEBOUNCE, move || {
                if search_generation.get() == generation {
                    rebuild();
                    // A stale callback must never clear a newer timeout.
                    pending_clear.borrow_mut().take();
                }
                gtk::glib::ControlFlow::Break
            });
            *pending_slot.borrow_mut() = Some(source);
        })
    };

    {
        let schedule_rebuild = schedule_rebuild.clone();
        filter_entry.connect_search_changed(move |_| schedule_rebuild());
    }
    {
        let schedule_rebuild = schedule_rebuild.clone();
        regex_toggle.connect_toggled(move |_| schedule_rebuild());
    }

    let jump = {
        let view = view.clone();
        let hits = hits.clone();
        let filter_entry = filter_entry.clone();
        let regex_toggle = regex_toggle.clone();
        let status_label = status_label.clone();
        Rc::new(move |index: usize| -> JumpOutcome {
            let Some(hit) = hits.borrow().get(index).cloned() else {
                return JumpOutcome::KeepOpen;
            };
            let pattern = filter_entry.text().to_string();
            let is_regex = regex_toggle.is_active();
            let outcome = jump_outcome(view.navigate_to_record_id(hit.block_id, hit.is_output));
            match outcome {
                JumpOutcome::Close => {
                    // The surface has already scrolled and taken focus. A
                    // highlight that cannot be set is not a reason to strand
                    // this modal over it.
                    view.focus_match_in_block(hit.block_id, &pattern, is_regex, hit.is_output);
                }
                JumpOutcome::KeepOpen => status_label.set_text(jump_unavailable_status()),
                JumpOutcome::ShowSnapshot(_) => {}
            }
            outcome
        })
    };

    let apply_jump_outcome = {
        let view = view.clone();
        let dialog = dialog.clone();
        let status_label = status_label.clone();
        Rc::new(move |outcome: JumpOutcome| match outcome {
            JumpOutcome::Close => dialog.force_close(),
            // The budget can evict a snapshot between the search that found it
            // and this activation, so the palette closes only once the view is
            // actually up; otherwise it stays open to say what happened.
            JumpOutcome::ShowSnapshot(record_id) => {
                match record_snapshot::present(&view, &snapshot_slot, record_id) {
                    Some(notice) => status_label.set_text(notice),
                    None => dialog.force_close(),
                }
            }
            JumpOutcome::KeepOpen => {}
        })
    };

    {
        let jump = jump.clone();
        let apply_jump_outcome = apply_jump_outcome.clone();
        list_box.connect_row_activated(move |_, row| {
            apply_jump_outcome(jump(row.index() as usize));
        });
    }

    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let dialog = dialog.clone();
        let list_box = list_box.clone();
        let jump = jump.clone();
        let apply_jump_outcome = apply_jump_outcome.clone();
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
                    apply_jump_outcome(jump(row.index() as usize));
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
        let pending_rebuild = pending_rebuild.clone();
        dialog.connect_closed(move |_| {
            if let Some(source) = pending_rebuild.borrow_mut().take() {
                source.remove();
            }
            *dialog_slot.borrow_mut() = None;
        });
    }

    *dialog_slot.borrow_mut() = Some(dialog.clone());
    let parent = view.widget();
    dialog.present(Some(&parent));
    filter_entry.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::{jump_outcome, JumpOutcome, RecordNavigationResult, CROSS_BLOCK_SEARCH_DEBOUNCE};

    /// The palette dispatches on the whole navigation ladder, not on "did it
    /// scroll": a record whose retained snapshot produced the hit is
    /// reachable, and only a record with neither location nor snapshot keeps
    /// the palette open with the unavailable status.
    #[test]
    fn jump_dispatches_every_navigation_outcome() {
        assert_eq!(
            jump_outcome(RecordNavigationResult::Navigated),
            JumpOutcome::Close
        );
        assert_eq!(
            jump_outcome(RecordNavigationResult::SnapshotView { record_id: 42 }),
            JumpOutcome::ShowSnapshot(42)
        );
        assert_eq!(
            jump_outcome(RecordNavigationResult::LocationUnavailable),
            JumpOutcome::KeepOpen
        );
        assert_eq!(
            jump_outcome(RecordNavigationResult::NoMatchingRecord),
            JumpOutcome::KeepOpen
        );
    }

    #[test]
    fn cross_block_search_waits_for_a_quiet_input_window() {
        assert_eq!(
            CROSS_BLOCK_SEARCH_DEBOUNCE,
            std::time::Duration::from_millis(150)
        );
    }
}
