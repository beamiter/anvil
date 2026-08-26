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
const CROSS_BLOCK_SEARCH_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES: usize = 8 * 1024;
const CROSS_BLOCK_SEARCH_LIMIT: usize = 500;
const CROSS_BLOCK_SEARCH_PAGE_STEP: usize = 10;

fn query_error(query: &str) -> Option<&'static str> {
    (query.len() > CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES)
        .then_some("Query is too long (maximum 8 KiB).")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Memory {
    query: String,
    options: crate::block_view::CrossBlockSearchOptions,
    scope: crate::block_view::CrossBlockSearchScope,
    failed_only: bool,
    slow_only: bool,
}

fn memory(
    query: &str,
    options: crate::block_view::CrossBlockSearchOptions,
    scope: crate::block_view::CrossBlockSearchScope,
    failed_only: bool,
    slow_only: bool,
) -> Memory {
    Memory {
        // Keep the pane-lifetime state bounded even if the user closes while
        // the explicit oversized-query diagnostic is visible.
        query: if query.len() <= CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES {
            query.to_string()
        } else {
            String::new()
        },
        options,
        scope,
        failed_only,
        slow_only,
    }
}

fn idle_status() -> &'static str {
    "Type to search. Shift+Enter jumps and advances; Ctrl+Shift+U resets."
}

fn refresh_status() -> &'static str {
    "Refreshing blocks…"
}

fn search_status(total: usize, selected: Option<usize>) -> String {
    if total == 0 {
        return "No matches.".to_string();
    }
    let noun = if total == 1 { "match" } else { "matches" };
    let position = selected
        .filter(|index| *index < total)
        .map(|index| format!("{} of ", index + 1))
        .unwrap_or_default();
    if total == CROSS_BLOCK_SEARCH_LIMIT {
        format!("{position}{total} {noun} (capped) — refine your query.")
    } else {
        format!("{position}{total} {noun}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionMove {
    First,
    Previous,
    Next,
    PagePrevious,
    PageNext,
    Last,
}

fn selection_index(current: Option<usize>, total: usize, movement: SelectionMove) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let Some(current) = current.filter(|index| *index < total) else {
        return Some(match movement {
            SelectionMove::Previous | SelectionMove::Last => total - 1,
            _ => 0,
        });
    };
    Some(match movement {
        SelectionMove::First => 0,
        SelectionMove::Previous => (current + total - 1) % total,
        SelectionMove::Next => (current + 1) % total,
        SelectionMove::PagePrevious => current.saturating_sub(CROSS_BLOCK_SEARCH_PAGE_STEP),
        SelectionMove::PageNext => current
            .saturating_add(CROSS_BLOCK_SEARCH_PAGE_STEP)
            .min(total - 1),
        SelectionMove::Last => total - 1,
    })
}

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

/// Only an exact live-terminal jump can remain in the palette and advance.
/// Snapshot-only results still open their dedicated view; unavailable hits
/// remain selected with their diagnostic.
fn should_step(outcome: JumpOutcome, requested: bool) -> bool {
    requested && outcome == JumpOutcome::Close
}

/// `exit:1 · 2.4s · …/anvil` for one hit, or `None` when the record carried
/// none of the three.
fn hit_outcome_label(hit: &CrossBlockHit) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    match hit.exit_code {
        Some(0) => {}
        Some(code) => parts.push(format!("exit:{code}")),
        None => {}
    }
    if let Some(duration) = hit.duration_ms {
        parts.push(crate::block_view::format_block_duration(duration));
    }
    if let Some(cwd) = hit.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
        parts.push(crate::block_view::shorten_path(cwd));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Bring `row` fully inside `scrolled` without moving keyboard focus.
///
/// Focus is what GTK scrolls to follow, and focus belongs to the search entry
/// here — the whole point is that Down walks the list while typing still works.
/// So the adjustment is driven directly: shift by the shortfall at whichever
/// edge the row is past, and not at all when it is already visible.
fn scroll_row_into_view(scrolled: &gtk::ScrolledWindow, row: &impl IsA<gtk::Widget>) {
    let Some(bounds) = row.compute_bounds(scrolled) else {
        return;
    };
    let adjustment = scrolled.vadjustment();
    let page = adjustment.page_size();
    let top = bounds.y() as f64;
    let bottom = top + bounds.height() as f64;
    let shift = if top < 0.0 {
        top
    } else if bottom > page {
        bottom - page
    } else {
        return;
    };
    let target = (adjustment.value() + shift).clamp(
        adjustment.lower(),
        (adjustment.upper() - page).max(adjustment.lower()),
    );
    adjustment.set_value(target);
}

fn jump_unavailable_status() -> &'static str {
    "This result is searchable, but it has no terminal location and no retained output."
}

pub(super) fn toggle(
    view: Rc<TermView>,
    dialog_slot: Rc<RefCell<Option<adw::Dialog>>>,
    snapshot_slot: Rc<RefCell<Option<adw::Dialog>>>,
    memory_slot: Rc<RefCell<Memory>>,
) {
    let open_dialog = { dialog_slot.borrow_mut().take() };
    if let Some(dialog) = open_dialog {
        dialog.force_close();
        return;
    }
    let remembered = memory_slot.borrow().clone();

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
    let case_toggle = gtk::ToggleButton::builder()
        .label("Aa")
        .tooltip_text("Match case")
        .build();
    let whole_word_toggle = gtk::ToggleButton::builder()
        .label("W")
        .tooltip_text("Match whole words")
        .build();
    let scope_dropdown = gtk::DropDown::from_strings(&["All", "Cmd", "Out"]);
    scope_dropdown.set_tooltip_text(Some("Search all text, commands only, or output only"));
    let reset_button = gtk::Button::with_label("Reset");
    reset_button.set_tooltip_text(Some("Reset query, matching options, scope, and filters"));
    header_bar.pack_start(&reset_button);
    header_bar.pack_end(&scope_dropdown);
    header_bar.pack_end(&whole_word_toggle);
    header_bar.pack_end(&regex_toggle);
    header_bar.pack_end(&case_toggle);
    // The outcome and duration predicates existed already with no surface that
    // could reach them, so "which failing build took over a second" was
    // unanswerable with the data sitting right there.
    let failed_toggle = gtk::ToggleButton::builder()
        .label("Failed")
        .tooltip_text("Only blocks whose command reported a non-zero exit")
        .build();
    let slow_toggle = gtk::ToggleButton::builder()
        .label("Slow")
        .tooltip_text("Only blocks that ran longer than the slow-block threshold")
        .build();
    header_bar.pack_end(&slow_toggle);
    header_bar.pack_end(&failed_toggle);

    let filter_entry = gtk::SearchEntry::new();
    filter_entry.set_placeholder_text(Some("Search across blocks…"));
    filter_entry.set_hexpand(true);
    filter_entry.set_margin_start(12);
    filter_entry.set_margin_end(12);
    filter_entry.set_margin_top(8);
    filter_entry.set_margin_bottom(8);
    case_toggle.set_active(remembered.options.case_sensitive);
    regex_toggle.set_active(remembered.options.regex);
    whole_word_toggle.set_active(remembered.options.whole_word);
    scope_dropdown.set_selected(remembered.scope.index());
    failed_toggle.set_active(remembered.failed_only);
    slow_toggle.set_active(remembered.slow_only);
    filter_entry.set_text(&remembered.query);

    let list_box = gtk::ListBox::new();
    list_box.set_selection_mode(gtk::SelectionMode::Single);
    list_box.add_css_class("boxed-list");
    list_box.set_margin_start(12);
    list_box.set_margin_end(12);
    list_box.set_margin_bottom(12);

    let status_label = gtk::Label::new(Some(idle_status()));
    status_label.add_css_class("dim-label");
    status_label.set_accessible_role(gtk::AccessibleRole::Status);
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
    let retained_hit: Rc<RefCell<Option<CrossBlockHit>>> = Rc::new(RefCell::new(None));
    let pending_rebuild: Rc<RefCell<Option<gtk::glib::SourceId>>> = Rc::new(RefCell::new(None));
    let search_generation = Rc::new(Cell::new(0_u64));
    {
        let hits = hits.clone();
        let status_label = status_label.clone();
        list_box.connect_row_selected(move |_, row| {
            status_label.set_text(&search_status(
                hits.borrow().len(),
                row.map(|row| row.index() as usize),
            ));
        });
    }
    let rebuild = {
        let view = view.clone();
        let failed_toggle = failed_toggle.clone();
        let slow_toggle = slow_toggle.clone();
        let list_box = list_box.clone();
        let hits = hits.clone();
        let status_label = status_label.clone();
        let filter_entry = filter_entry.clone();
        let regex_toggle = regex_toggle.clone();
        let case_toggle = case_toggle.clone();
        let whole_word_toggle = whole_word_toggle.clone();
        let scope_dropdown = scope_dropdown.clone();
        let retained_hit = retained_hit.clone();
        Rc::new(move || {
            let query = filter_entry.text().to_string();
            let retained_hit = retained_hit.borrow_mut().take();
            let options = crate::block_view::CrossBlockSearchOptions {
                case_sensitive: case_toggle.is_active(),
                regex: regex_toggle.is_active(),
                whole_word: whole_word_toggle.is_active(),
            };
            while let Some(child) = list_box.first_child() {
                list_box.remove(&child);
            }
            if query.is_empty() {
                hits.borrow_mut().clear();
                status_label.set_text(idle_status());
                return;
            }
            if let Some(message) = query_error(&query) {
                hits.borrow_mut().clear();
                status_label.set_text(message);
                return;
            }

            let filters = crate::block_view::BlockFilters {
                failed_only: failed_toggle.is_active(),
                slow_only: slow_toggle.is_active(),
                slow_threshold_ms: crate::block_view::SLOW_BLOCK_THRESHOLD_MS,
                ..Default::default()
            };
            let scope =
                crate::block_view::CrossBlockSearchScope::from_index(scope_dropdown.selected());
            match view.cross_block_search_in_scope(
                &query,
                options,
                scope,
                CROSS_BLOCK_SEARCH_LIMIT,
                &filters,
            ) {
                Ok(results) => {
                    let total = results.len();
                    status_label.set_text(&search_status(total, None));
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
                        // Outcome at a glance: telling the failing `cargo build`
                        // from the passing ones should not require visiting each.
                        if let Some(outcome) = hit_outcome_label(hit) {
                            let label = gtk::Label::new(Some(&outcome));
                            label.add_css_class(if hit.exit_code.is_some_and(|code| code != 0) {
                                "block-status-bad"
                            } else {
                                "block-status-ok"
                            });
                            label.set_valign(gtk::Align::Center);
                            row.add_suffix(&label);
                        }
                        list_box.append(&row);
                    }
                    let selected = retained_hit
                        .and_then(|retained| results.iter().position(|hit| hit == &retained))
                        .unwrap_or(0);
                    *hits.borrow_mut() = results;
                    list_box.select_row(list_box.row_at_index(selected as i32).as_ref());
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
        let retained_hit = retained_hit.clone();
        Rc::new(move |preserve_selection: bool| {
            let generation = search_generation.get().wrapping_add(1);
            search_generation.set(generation);
            if let Some(source) = pending_rebuild.borrow_mut().take() {
                source.remove();
            }

            *retained_hit.borrow_mut() = if preserve_selection {
                list_box
                    .selected_row()
                    .and_then(|row| hits.borrow().get(row.index() as usize).cloned())
            } else {
                None
            };
            if filter_entry.text().is_empty() {
                while let Some(child) = list_box.first_child() {
                    list_box.remove(&child);
                }
                hits.borrow_mut().clear();
                status_label.set_text(idle_status());
                return;
            }
            if let Some(message) = query_error(filter_entry.text().as_str()) {
                while let Some(child) = list_box.first_child() {
                    list_box.remove(&child);
                }
                hits.borrow_mut().clear();
                status_label.set_text(message);
                return;
            }
            if preserve_selection {
                status_label.set_text(refresh_status());
            } else {
                while let Some(child) = list_box.first_child() {
                    list_box.remove(&child);
                }
                hits.borrow_mut().clear();
                status_label.set_text("Searching blocks…");
            }

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
        filter_entry.connect_search_changed(move |_| schedule_rebuild(false));
    }
    {
        let schedule_rebuild = schedule_rebuild.clone();
        let filter_entry = filter_entry.clone();
        regex_toggle.connect_toggled(move |_| {
            schedule_rebuild(false);
            filter_entry.grab_focus();
        });
    }
    {
        let schedule_rebuild = schedule_rebuild.clone();
        let filter_entry = filter_entry.clone();
        case_toggle.connect_toggled(move |_| {
            schedule_rebuild(false);
            filter_entry.grab_focus();
        });
    }
    {
        let schedule_rebuild = schedule_rebuild.clone();
        let filter_entry = filter_entry.clone();
        whole_word_toggle.connect_toggled(move |_| {
            schedule_rebuild(false);
            filter_entry.grab_focus();
        });
    }
    {
        let schedule_rebuild = schedule_rebuild.clone();
        let filter_entry = filter_entry.clone();
        scope_dropdown.connect_selected_notify(move |_| {
            schedule_rebuild(false);
            filter_entry.grab_focus();
        });
    }
    {
        let schedule_rebuild = schedule_rebuild.clone();
        let filter_entry = filter_entry.clone();
        failed_toggle.connect_toggled(move |_| {
            schedule_rebuild(false);
            filter_entry.grab_focus();
        });
    }
    {
        let schedule_rebuild = schedule_rebuild.clone();
        let filter_entry = filter_entry.clone();
        slow_toggle.connect_toggled(move |_| {
            schedule_rebuild(false);
            filter_entry.grab_focus();
        });
    }
    {
        let filter_entry = filter_entry.clone();
        let case_toggle = case_toggle.clone();
        let regex_toggle = regex_toggle.clone();
        let whole_word_toggle = whole_word_toggle.clone();
        let scope_dropdown = scope_dropdown.clone();
        let failed_toggle = failed_toggle.clone();
        let slow_toggle = slow_toggle.clone();
        reset_button.connect_clicked(move |_| {
            filter_entry.set_text("");
            case_toggle.set_active(false);
            regex_toggle.set_active(false);
            whole_word_toggle.set_active(false);
            scope_dropdown.set_selected(crate::block_view::CrossBlockSearchScope::All.index());
            failed_toggle.set_active(false);
            slow_toggle.set_active(false);
            filter_entry.grab_focus();
        });
    }

    // Probe finalized-record identity without cloning terminal content. A
    // completion or same-length retention rotation refreshes the open picker,
    // preserving the exact selected hit when it still exists.
    let refresh_source = {
        let view = view.clone();
        let observed_version = Rc::new(Cell::new(view.cross_block_search_version()));
        let schedule_rebuild = schedule_rebuild.clone();
        gtk::glib::timeout_add_local(CROSS_BLOCK_SEARCH_REFRESH_INTERVAL, move || {
            let current = view.cross_block_search_version();
            if current != observed_version.get() {
                observed_version.set(current);
                schedule_rebuild(true);
            }
            gtk::glib::ControlFlow::Continue
        })
    };
    let refresh_source = Rc::new(RefCell::new(Some(refresh_source)));

    let jump = {
        let view = view.clone();
        let hits = hits.clone();
        let filter_entry = filter_entry.clone();
        let regex_toggle = regex_toggle.clone();
        let case_toggle = case_toggle.clone();
        let whole_word_toggle = whole_word_toggle.clone();
        let status_label = status_label.clone();
        Rc::new(move |index: usize| -> JumpOutcome {
            let Some(hit) = hits.borrow().get(index).cloned() else {
                return JumpOutcome::KeepOpen;
            };
            let pattern = filter_entry.text().to_string();
            let options = crate::block_view::CrossBlockSearchOptions {
                case_sensitive: case_toggle.is_active(),
                regex: regex_toggle.is_active(),
                whole_word: whole_word_toggle.is_active(),
            };
            let outcome = jump_outcome(view.navigate_to_record_id(hit.block_id, hit.is_output));
            match outcome {
                JumpOutcome::Close => {
                    // The surface has already scrolled and taken focus. A
                    // highlight that cannot be set is not a reason to strand
                    // this modal over it.
                    view.focus_match_in_block(
                        hit.block_id,
                        &pattern,
                        options,
                        hit.is_output,
                        hit.occurrence,
                    );
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
        let scrolled = scrolled.clone();
        let hits = hits.clone();
        let filter_entry = filter_entry.clone();
        let jump = jump.clone();
        let apply_jump_outcome = apply_jump_outcome.clone();
        let case_toggle = case_toggle.clone();
        let regex_toggle = regex_toggle.clone();
        let whole_word_toggle = whole_word_toggle.clone();
        let scope_dropdown = scope_dropdown.clone();
        let reset_button = reset_button.clone();
        key_controller.connect_key_pressed(move |_, key, _, state| {
            use gtk::gdk::{Key, ModifierType};
            if key == Key::Escape
                || (matches!(key, Key::g | Key::G)
                    && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
            {
                dialog.force_close();
                return gtk::glib::Propagation::Stop;
            }
            if state.contains(ModifierType::CONTROL_MASK) {
                if matches!(key, Key::u | Key::U) {
                    if state.contains(ModifierType::SHIFT_MASK) {
                        reset_button.emit_clicked();
                    } else {
                        filter_entry.set_text("");
                        filter_entry.grab_focus();
                    }
                    return gtk::glib::Propagation::Stop;
                }
                let toggle = match key {
                    Key::i | Key::I => Some(&case_toggle),
                    Key::r | Key::R => Some(&regex_toggle),
                    Key::w | Key::W => Some(&whole_word_toggle),
                    _ => None,
                };
                if let Some(toggle) = toggle {
                    toggle.set_active(!toggle.is_active());
                    return gtk::glib::Propagation::Stop;
                }
                if matches!(key, Key::o | Key::O) {
                    let scope = crate::block_view::CrossBlockSearchScope::from_index(
                        scope_dropdown.selected(),
                    );
                    scope_dropdown.set_selected(scope.cycled().index());
                    return gtk::glib::Propagation::Stop;
                }
            }
            if matches!(key, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box.selected_row() {
                    let index = row.index() as usize;
                    let outcome = jump(index);
                    if should_step(outcome, state.contains(ModifierType::SHIFT_MASK)) {
                        if let Some(next) =
                            selection_index(Some(index), hits.borrow().len(), SelectionMove::Next)
                        {
                            if let Some(next_row) = list_box.row_at_index(next as i32) {
                                list_box.select_row(Some(&next_row));
                                scroll_row_into_view(&scrolled, &next_row);
                            }
                        }
                        filter_entry.grab_focus();
                    } else {
                        apply_jump_outcome(outcome);
                    }
                }
                return gtk::glib::Propagation::Stop;
            }
            let movement = match key {
                Key::Home | Key::KP_Home => SelectionMove::First,
                Key::Up => SelectionMove::Previous,
                Key::Down => SelectionMove::Next,
                Key::Page_Up => SelectionMove::PagePrevious,
                Key::Page_Down => SelectionMove::PageNext,
                Key::End | Key::KP_End => SelectionMove::Last,
                _ => return gtk::glib::Propagation::Proceed,
            };
            let current = list_box.selected_row().map(|row| row.index() as usize);
            if let Some(next) = selection_index(current, hits.borrow().len(), movement) {
                if let Some(row) = list_box.row_at_index(next as i32) {
                    list_box.select_row(Some(&row));
                    // Selection does not move focus away from the query, so
                    // drive the viewport directly for page/edge navigation.
                    scroll_row_into_view(&scrolled, &row);
                }
            }
            gtk::glib::Propagation::Stop
        });
    }
    dialog.add_controller(key_controller);

    {
        let dialog_slot = dialog_slot.clone();
        let pending_rebuild = pending_rebuild.clone();
        let refresh_source = refresh_source.clone();
        let filter_entry = filter_entry.clone();
        let case_toggle = case_toggle.clone();
        let regex_toggle = regex_toggle.clone();
        let whole_word_toggle = whole_word_toggle.clone();
        let scope_dropdown = scope_dropdown.clone();
        let failed_toggle = failed_toggle.clone();
        let slow_toggle = slow_toggle.clone();
        dialog.connect_closed(move |_| {
            if let Some(source) = refresh_source.borrow_mut().take() {
                source.remove();
            }
            if let Some(source) = pending_rebuild.borrow_mut().take() {
                source.remove();
            }
            *memory_slot.borrow_mut() = memory(
                filter_entry.text().as_str(),
                crate::block_view::CrossBlockSearchOptions {
                    case_sensitive: case_toggle.is_active(),
                    regex: regex_toggle.is_active(),
                    whole_word: whole_word_toggle.is_active(),
                },
                crate::block_view::CrossBlockSearchScope::from_index(scope_dropdown.selected()),
                failed_toggle.is_active(),
                slow_toggle.is_active(),
            );
            *dialog_slot.borrow_mut() = None;
        });
    }

    *dialog_slot.borrow_mut() = Some(dialog.clone());
    let parent = view.widget();
    dialog.present(Some(&parent));
    if !remembered.query.is_empty() {
        schedule_rebuild(false);
    }
    filter_entry.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::{
        hit_outcome_label, idle_status, jump_outcome, memory, query_error, refresh_status,
        search_status, selection_index, should_step, CrossBlockHit, JumpOutcome,
        RecordNavigationResult, SelectionMove, CROSS_BLOCK_SEARCH_DEBOUNCE,
        CROSS_BLOCK_SEARCH_LIMIT, CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES,
    };
    use crate::block_view::{CrossBlockSearchOptions, CrossBlockSearchScope};

    fn hit(exit_code: Option<i32>, duration_ms: Option<u64>, cwd: Option<&str>) -> CrossBlockHit {
        CrossBlockHit {
            block_id: 1,
            is_output: true,
            line_no: 1,
            line_text: "error[E0308]".to_string(),
            cmd_preview: "cargo build".to_string(),
            exit_code,
            duration_ms,
            cwd: cwd.map(str::to_string),
            occurrence: 0,
        }
    }

    /// The row suffix exists to answer "which of these `cargo build`s failed"
    /// without visiting each one, so a success must not wear an `exit:` badge
    /// and a record that carried nothing must not wear an empty one.
    #[test]
    fn outcome_suffix_reports_only_what_the_record_carried() {
        // An absolute path outside `$HOME`, so the expectation does not depend
        // on whose machine runs the test.
        assert_eq!(
            hit_outcome_label(&hit(Some(1), Some(2_400), Some("/srv/ci/work/anvil"))).as_deref(),
            Some("exit:1 · 2.4s · …/work/anvil")
        );
        assert_eq!(
            hit_outcome_label(&hit(Some(0), Some(2_400), None)).as_deref(),
            Some("2.4s"),
            "a success is the absence of an exit badge, not `exit:0`"
        );
        assert_eq!(
            hit_outcome_label(&hit(None, None, None)),
            None,
            "a record with no outcome gets no suffix rather than an empty one"
        );
        assert_eq!(
            hit_outcome_label(&hit(None, None, Some(""))),
            None,
            "an empty cwd is not a directory"
        );
    }

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

    #[test]
    fn cross_block_search_rejects_oversized_queries_before_regex_compilation() {
        assert_eq!(
            query_error(&"x".repeat(CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES)),
            None
        );
        assert_eq!(
            query_error(&"x".repeat(CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES + 1)),
            Some("Query is too long (maximum 8 KiB).")
        );
        assert!(query_error(&"界".repeat(CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES / 3 + 1)).is_some());
    }

    #[test]
    fn cross_block_search_memory_is_bounded_and_keeps_all_filters() {
        let options = CrossBlockSearchOptions {
            case_sensitive: true,
            regex: true,
            whole_word: true,
        };
        let remembered = memory("needle", options, CrossBlockSearchScope::Output, true, true);
        assert_eq!(remembered.query, "needle");
        assert_eq!(remembered.options, options);
        assert_eq!(remembered.scope, CrossBlockSearchScope::Output);
        assert!(remembered.failed_only);
        assert!(remembered.slow_only);

        let oversized = memory(
            &"x".repeat(CROSS_BLOCK_SEARCH_QUERY_LIMIT_BYTES + 1),
            options,
            CrossBlockSearchScope::Command,
            true,
            false,
        );
        assert!(oversized.query.is_empty());
        assert_eq!(oversized.options, options);
        assert_eq!(oversized.scope, CrossBlockSearchScope::Command);
        assert!(oversized.failed_only);
        assert!(!oversized.slow_only);
    }

    #[test]
    fn search_status_and_navigation_report_position_and_stay_bounded() {
        use SelectionMove as Move;

        assert_eq!(search_status(0, None), "No matches.");
        assert_eq!(search_status(1, Some(0)), "1 of 1 match");
        assert_eq!(
            search_status(CROSS_BLOCK_SEARCH_LIMIT, Some(36)),
            "37 of 500 matches (capped) — refine your query."
        );
        assert_eq!(selection_index(None, 0, Move::Next), None);
        assert_eq!(selection_index(None, 37, Move::Previous), Some(36));
        assert_eq!(selection_index(Some(36), 37, Move::Next), Some(0));
        assert_eq!(selection_index(Some(0), 37, Move::Previous), Some(36));
        assert_eq!(selection_index(Some(20), 37, Move::First), Some(0));
        assert_eq!(selection_index(Some(2), 37, Move::Last), Some(36));
        assert_eq!(selection_index(Some(23), 37, Move::PagePrevious), Some(13));
        assert_eq!(selection_index(Some(31), 37, Move::PageNext), Some(36));
    }

    #[test]
    fn continuous_review_advances_only_after_a_live_jump() {
        assert_eq!(
            idle_status(),
            "Type to search. Shift+Enter jumps and advances; Ctrl+Shift+U resets."
        );
        assert_eq!(refresh_status(), "Refreshing blocks…");
        assert!(should_step(JumpOutcome::Close, true));
        assert!(!should_step(JumpOutcome::Close, false));
        assert!(!should_step(JumpOutcome::ShowSnapshot(7), true));
        assert!(!should_step(JumpOutcome::KeepOpen, true));
    }
}
