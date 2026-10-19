//! Navigation for the persistent Unified surface. Geometry comes exclusively
//! from VTE and the existing authenticated zone proof, never from text guesses.

use super::{BackendRecords, BlockState, RecordNavigationResult, TermView};
use crate::keybindings::Action;
use gtk::prelude::*;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Position {
    pub(super) rows_above_live: u64,
    pub(super) reading: bool,
}

pub(super) fn position(lower: f64, upper: f64, page: f64, value: f64) -> Option<Position> {
    if ![lower, upper, page, value].into_iter().all(f64::is_finite) || upper < lower || page <= 0.0
    {
        return None;
    }
    let distance = ((upper - page).max(lower) - value.clamp(lower, upper)).max(0.0);
    Some(Position {
        rows_above_live: distance.ceil() as u64,
        reading: distance > 1.0,
    })
}

pub(super) fn terminal_position(terminal: &vte4::Terminal) -> Option<Position> {
    let adjustment = terminal.vadjustment()?;
    position(
        adjustment.lower(),
        adjustment.upper(),
        adjustment.page_size(),
        adjustment.value(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Record(u64),
    Live,
    Empty,
    Oldest,
    Stale,
    Unproven,
}

/// `records` are completed commands in retained order. Proof values may share
/// a bottom-clamped position. Explicit identity advances through those ties;
/// an unanchored Next chooses the first command at the nearest later position.
pub(super) fn step(
    records: &[(u64, Option<f64>)],
    selected: Option<u64>,
    reading: bool,
    viewport: f64,
    previous: bool,
) -> Step {
    if let Some(selected) = selected {
        let Some(index) = records.iter().position(|(id, _)| *id == selected) else {
            return Step::Stale;
        };
        return if previous {
            index
                .checked_sub(1)
                .map_or(Step::Oldest, |index| Step::Record(records[index].0))
        } else {
            records
                .get(index + 1)
                .map_or(Step::Live, |(id, _)| Step::Record(*id))
        };
    }
    if records.is_empty() {
        return Step::Empty;
    }
    if !reading {
        return if previous {
            Step::Record(records.last().expect("nonempty records").0)
        } else {
            Step::Live
        };
    }
    if !viewport.is_finite() {
        return Step::Unproven;
    }
    let mut nearest: Option<(u64, f64)> = None;
    for &(id, value) in records {
        let Some(value) = value.filter(|value| value.is_finite()) else {
            continue;
        };
        let in_direction = if previous {
            value < viewport - 0.01
        } else {
            value > viewport + 0.01
        };
        if !in_direction {
            continue;
        }
        let nearer =
            nearest.is_none_or(|(_, old)| if previous { value >= old } else { value < old });
        if nearer {
            nearest = Some((id, value));
        }
    }
    if let Some((id, _)) = nearest {
        return Step::Record(id);
    }
    if !previous
        && records
            .last()
            .and_then(|(_, value)| *value)
            .is_some_and(|value| value.is_finite() && value <= viewport + 0.01)
    {
        return Step::Live;
    }
    Step::Unproven
}

fn command_preview(command: &str) -> String {
    let mut preview = String::new();
    for character in command.chars().take(160) {
        if character.is_control()
            || jterm_core::review_input::is_visual_spoofing_character(character)
        {
            preview.push(' ');
        } else {
            preview.push(character);
        }
    }
    preview.trim().to_string()
}

const MAX_COPY_COMMAND_BYTES: usize = 256 * 1024;

fn command_is_copyable(command: &str, truncated: bool) -> bool {
    !truncated
        && !command.trim().is_empty()
        && command.len() <= MAX_COPY_COMMAND_BYTES
        && jterm_core::review_input::safe_multiline_display(command, MAX_COPY_COMMAND_BYTES)
            == command
}

type Refresh = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
type Stamp = (usize, Option<u64>, Option<u64>, Option<u64>);

#[derive(Default)]
struct Summary {
    stamp: Option<Stamp>,
    count: usize,
    selected: Option<String>,
    copyable: bool,
}

#[derive(Clone)]
pub(super) struct NavigationBar {
    pub(super) root: gtk::Overlay,
    pub(super) status: gtk::Label,
    pub(super) command: gtk::Label,
    pub(super) previous: gtk::Button,
    pub(super) next: gtk::Button,
    pub(super) find: gtk::Button,
    pub(super) browse: gtk::Button,
    pub(super) copy: gtk::Button,
    pub(super) live: gtk::Button,
    text_labels: Vec<gtk::Label>,
    width: Rc<Cell<i32>>,
    suspended: Rc<Cell<bool>>,
    programmatic: Rc<Cell<u32>>,
    refresh: Refresh,
    summary: Rc<RefCell<Summary>>,
    bound: Rc<Cell<bool>>,
}

pub(super) struct NavigationGuard(Rc<Cell<u32>>);

impl Drop for NavigationGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

impl NavigationBar {
    pub(super) fn new() -> Self {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        row.add_css_class("unified-navigation");
        row.set_accessible_role(gtk::AccessibleRole::Toolbar);
        row.update_property(&[gtk::accessible::Property::Label(
            "Unified terminal navigation",
        )]);
        let status = gtk::Label::new(Some("Live"));
        status.add_css_class("unified-navigation-status");
        status.set_accessible_role(gtk::AccessibleRole::Status);
        row.append(&status);
        let command = gtk::Label::new(None);
        command.add_css_class("unified-navigation-command");
        command.set_ellipsize(gtk::pango::EllipsizeMode::End);
        command.set_width_chars(0);
        command.set_hexpand(true);
        command.set_halign(gtk::Align::Fill);
        command.set_xalign(0.0);
        row.append(&command);
        let mut text_labels = Vec::new();
        let mut button = |icon, text: &'static str, tooltip| {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            button.set_focusable(true);
            button.set_focus_on_click(false);
            button.set_tooltip_text(Some(tooltip));
            button.update_property(&[gtk::accessible::Property::Label(tooltip)]);
            let contents = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            contents.append(&gtk::Image::from_icon_name(icon));
            if !text.is_empty() {
                let label = gtk::Label::new(Some(text));
                // Captions contribute natural width, never a wide minimum.
                // Otherwise GTK can clamp the first narrow allocation before
                // our allocation observer gets a chance to hide the labels.
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_width_chars(0);
                label.set_visible(false);
                contents.append(&label);
                text_labels.push(label);
            }
            button.set_child(Some(&contents));
            row.append(&button);
            button
        };
        let previous = button("go-up-symbolic", "", "Previous command");
        let next = button("go-down-symbolic", "", "Next command");
        let find = button("edit-find-symbolic", "Find", "Find text in this terminal");
        let browse = button(
            "view-list-symbolic",
            "Browse",
            "Browse completed commands in this terminal",
        );
        let copy = button("edit-copy-symbolic", "", "Copy the current command");
        let live = button("go-bottom-symbolic", "Live", "Return to live input");
        let root = gtk::Overlay::new();
        root.set_child(Some(&row));
        root.set_vexpand(false);
        let observer = gtk::DrawingArea::new();
        observer.set_can_target(false);
        observer.set_focusable(false);
        root.add_overlay(&observer);
        root.set_measure_overlay(&observer, false);
        let width = Rc::new(Cell::new(0));
        let refresh: Refresh = Rc::new(RefCell::new(None));
        observer.connect_resize({
            let width = width.clone();
            let refresh = refresh.clone();
            move |_, allocated, _| {
                if width.replace(allocated) != allocated {
                    let refresh = refresh.borrow().clone();
                    if let Some(refresh) = refresh {
                        refresh();
                    }
                }
            }
        });
        Self {
            root,
            status,
            command,
            previous,
            next,
            find,
            browse,
            copy,
            live,
            text_labels,
            width,
            refresh,
            suspended: Rc::new(Cell::new(false)),
            programmatic: Rc::new(Cell::new(0)),
            summary: Rc::new(RefCell::new(Summary::default())),
            bound: Rc::new(Cell::new(false)),
        }
    }

    pub(super) fn guard(&self) -> NavigationGuard {
        self.programmatic
            .set(self.programmatic.get().saturating_add(1));
        NavigationGuard(self.programmatic.clone())
    }

    pub(super) fn request_refresh(&self) {
        let refresh = self.refresh.borrow().clone();
        if let Some(refresh) = refresh {
            refresh();
        }
    }

    pub(super) fn set_suspended(&self, suspended: bool) {
        self.suspended.set(suspended);
        self.root.set_visible(!suspended);
        self.request_refresh();
    }

    fn update(&self, view: &TermView) {
        let hidden = self.suspended.get()
            || view.fullscreen.get()
            || view.bstate.get() == BlockState::AltScreen;
        if self.root.is_visible() == hidden {
            self.root.set_visible(!hidden);
            (view.resize_kick)();
        }
        if hidden {
            return;
        }
        let position = terminal_position(&view.active_vte);
        let reading = position.is_some_and(|position| position.reading);
        let compact = self.width.get() < 640;
        for label in &self.text_labels {
            label.set_visible(!compact);
        }
        let unread = view.unread_count.get();
        let status = if reading {
            if unread > 0 {
                let count = if unread > 99 {
                    "99+".to_string()
                } else {
                    unread.to_string()
                };
                if compact {
                    format!("History · +{count}")
                } else {
                    format!("History · {count} new")
                }
            } else if compact {
                "History".into()
            } else {
                "Reading history".into()
            }
        } else if position.is_some() {
            "Live".into()
        } else {
            "Preparing".into()
        };
        self.status.set_text(&status);
        if reading {
            self.status.add_css_class("reading");
        } else {
            self.status.remove_css_class("reading");
        }
        self.status.set_tooltip_text(Some(&position.map_or_else(
            || "Waiting for terminal geometry".to_string(),
            |position| {
                if position.reading {
                    format!(
                        "{} rows above live input · {unread} completed commands while reading",
                        position.rows_above_live
                    )
                } else {
                    "Following live terminal output".to_string()
                }
            },
        )));
        let BackendRecords::Metadata(zones) = view.render_backend.records() else {
            return;
        };
        let stamp = (
            zones.records.len(),
            zones.records.front().map(|r| r.id),
            zones.records.back().map(|r| r.id),
            view.navigated_record_id.get(),
        );
        let mut summary = self.summary.borrow_mut();
        if summary.stamp != Some(stamp) {
            summary.stamp = Some(stamp);
            summary.count = 0;
            summary.selected = None;
            summary.copyable = false;
            for record in &zones.records {
                if record.is_background || record.cmd.trim().is_empty() {
                    continue;
                }
                summary.count += 1;
                if Some(record.id) == stamp.3 {
                    summary.selected =
                        Some(format!("#{} · {}", record.id, command_preview(&record.cmd)));
                    summary.copyable = command_is_copyable(
                        &record.cmd,
                        record.command_source == super::CommandTextSource::ScreenAfterTruncation,
                    );
                }
            }
            if stamp.3.is_some() && summary.selected.is_none() {
                view.navigated_record_id.set(None);
            }
        }
        self.previous.set_sensitive(summary.count > 0);
        self.next
            .set_sensitive(summary.count > 0 && (reading || summary.selected.is_some()));
        self.live
            .set_sensitive(reading || summary.selected.is_some());
        self.copy.set_sensitive(summary.copyable);
        let text = summary.selected.clone().unwrap_or_else(|| {
            if summary.count == 0 {
                "Commands appear with shell integration".into()
            } else if compact {
                format!("{} commands", summary.count)
            } else {
                format!("{} completed commands", summary.count)
            }
        });
        self.command.set_text(&text);
        self.command.set_tooltip_text(Some(&text));
        self.copy.set_tooltip_text(Some(if summary.copyable {
            "Copy this recorded command without changing the terminal selection"
        } else { "Choose a completed command with Previous, Next or Browse; incomplete command text cannot be copied here" }));
    }
}

impl TermView {
    /// Native Find can change the selected occurrence without scrolling the
    /// viewport. Retire the command cursor at the action boundary as well as
    /// on manual adjustment movement, so Copy never names an unrelated row.
    pub(crate) fn clear_unified_navigation_cursor(&self) {
        if let Some(bar) = &self.unified_navigation {
            if self.navigated_record_id.take().is_some() {
                bar.request_refresh();
            }
        }
    }

    /// Bind once after the pane has its Rc owner. All widget/signal closures
    /// lease a Weak view; a pending refresh cannot keep a closed pane alive.
    pub(crate) fn install_unified_navigation(view: &Rc<Self>, dispatch: impl Fn(Action) + 'static) {
        let Some(bar) = view.unified_navigation.as_ref() else {
            return;
        };
        if bar.bound.replace(true) {
            return;
        }
        let pending = Rc::new(Cell::new(false));
        let weak = Rc::downgrade(view);
        let refresh: Rc<dyn Fn()> = Rc::new(move || {
            if pending.replace(true) {
                return;
            }
            let weak = weak.clone();
            let pending = pending.clone();
            gtk::glib::idle_add_local_once(move || {
                pending.set(false);
                if let Some(view) = weak.upgrade() {
                    if let Some(bar) = &view.unified_navigation {
                        bar.update(&view);
                    }
                }
            });
        });
        *bar.refresh.borrow_mut() = Some(refresh.clone());
        if let Some(adjustment) = view.active_vte.vadjustment() {
            let weak = Rc::downgrade(view);
            let last_value = Cell::new(adjustment.value());
            adjustment.connect_value_changed(move |adjustment| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                let moved = last_value.replace(adjustment.value()) != adjustment.value();
                if moved
                    && view
                        .unified_navigation
                        .as_ref()
                        .is_some_and(|bar| bar.programmatic.get() == 0)
                {
                    view.navigated_record_id.set(None);
                }
                view.sync_unified_scroll_state();
            });
            let weak = Rc::downgrade(view);
            adjustment.connect_changed(move |_| {
                if let Some(view) = weak.upgrade() {
                    view.sync_unified_scroll_state();
                }
            });
        }
        let dispatch: Rc<dyn Fn(Action)> = Rc::new(dispatch);
        for (button, action) in [
            (&bar.previous, Action::PreviousCommand),
            (&bar.next, Action::NextCommand),
            (&bar.find, Action::ToggleSearch),
            (&bar.browse, Action::CrossBlockSearch),
            (&bar.copy, Action::CopyCurrentCommand),
            (&bar.live, Action::ReturnToLive),
        ] {
            let dispatch = dispatch.clone();
            button.connect_clicked(move |_| dispatch(action));
        }
        let weak = Rc::downgrade(view);
        view.jump_fab.connect_clicked(move |_| {
            if let Some(view) = weak.upgrade() {
                let _ = view.return_to_live();
            }
        });
        let weak = Rc::downgrade(view);
        view.connect_alt_screen_transition(move |_| {
            if let Some(view) = weak.upgrade() {
                if let Some(bar) = &view.unified_navigation {
                    bar.request_refresh();
                }
                (view.resize_kick)();
            }
        });
        view.sync_unified_scroll_state();
        refresh();
    }

    fn sync_unified_scroll_state(&self) {
        let reading = self.bstate.get() != BlockState::AltScreen
            && !self.fullscreen.get()
            && terminal_position(&self.active_vte).is_some_and(|position| position.reading);
        self.user_scrolled_up.set(reading);
        if !reading {
            self.unread_count.set(0);
        }
        // The permanent toolbar already owns Return to live. Do not rebuild
        // the Block FAB's child widgets for every native scroll/range signal.
        self.jump_fab.set_visible(false);
        if let Some(bar) = &self.unified_navigation {
            bar.request_refresh();
        }
    }

    pub(crate) fn return_to_live(&self) -> Result<(), &'static str> {
        let Some(bar) = &self.unified_navigation else {
            return Err("Return to live is available in Unified mode.");
        };
        if self.fullscreen.get() || self.bstate.get() == BlockState::AltScreen {
            return Err("The full-screen application owns this terminal.");
        }
        let _guard = bar.guard();
        self.selection_feed_hold.flush_now();
        if let Some(adjustment) = self.active_vte.vadjustment() {
            adjustment
                .set_value((adjustment.upper() - adjustment.page_size()).max(adjustment.lower()));
        }
        self.navigated_record_id.set(None);
        self.user_scrolled_up.set(false);
        self.unread_count.set(0);
        self.jump_fab.set_visible(false);
        self.active_vte.grab_focus();
        bar.request_refresh();
        Ok(())
    }

    pub(crate) fn navigate_unified_command(
        &self,
        previous: bool,
    ) -> Result<RecordNavigationResult, &'static str> {
        let Some(bar) = &self.unified_navigation else {
            return Err("Command navigation is available in Unified mode.");
        };
        if self.fullscreen.get() || self.bstate.get() == BlockState::AltScreen {
            return Err("The full-screen application owns this terminal.");
        }
        let selected = self.navigated_record_id.get();
        let position = terminal_position(&self.active_vte);
        let viewport = self
            .active_vte
            .vadjustment()
            .map_or(f64::NAN, |adjustment| adjustment.value());
        let ids = {
            let BackendRecords::Metadata(zones) = self.render_backend.records() else {
                unreachable!();
            };
            zones
                .records
                .iter()
                .filter(|record| !record.is_background && !record.cmd.trim().is_empty())
                .map(|record| record.id)
                .collect::<Vec<_>>()
        };
        let records = ids
            .into_iter()
            .map(|id| {
                let proof = (selected.is_none()
                    && position.is_some_and(|position| position.reading))
                .then(|| self.render_backend.record_scroll_value(id))
                .flatten();
                (id, proof)
            })
            .collect::<Vec<_>>();
        match step(&records, selected, position.is_none_or(|position| position.reading), viewport, previous) {
            Step::Record(id) => Ok(self.navigate_to_record_id(id, false)),
            Step::Live => { self.return_to_live()?; Ok(RecordNavigationResult::Navigated) }
            Step::Empty => Err("No completed commands yet. Browse becomes useful after shell integration records a command."),
            Step::Oldest => Err("Already at the oldest retained command."),
            Step::Stale => {
                self.navigated_record_id.set(None);
                bar.request_refresh();
                Err("That command is no longer retained. Choose another command in Browse.")
            }
            Step::Unproven => Err("No exact command location is available in this direction. Use Browse to inspect retained commands."),
        }
    }

    pub(crate) fn copy_current_unified_command(&self) -> Result<(), &'static str> {
        if self.unified_navigation.is_none() {
            return Err("Current-command copy is available in Unified mode.");
        }
        if self.fullscreen.get() || self.bstate.get() == BlockState::AltScreen {
            return Err("The full-screen application owns this terminal.");
        }
        let id = self
            .navigated_record_id
            .get()
            .ok_or("Choose a command with Previous, Next or Browse first.")?;
        let command = {
            let BackendRecords::Metadata(zones) = self.render_backend.records() else {
                unreachable!();
            };
            let record =
                zones.records.iter().find(|record| record.id == id).ok_or(
                    "That command is no longer retained. Choose another command in Browse.",
                )?;
            if record.is_background
                || !command_is_copyable(
                    &record.cmd,
                    record.command_source == super::CommandTextSource::ScreenAfterTruncation,
                )
            {
                return Err("Complete, copyable command text is unavailable. Use Browse to inspect the retained record.");
            }
            record.cmd.clone()
        };
        self.active_vte.clipboard().set_text(&command);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_position_tracks_range_only_growth_and_rejects_invalid_geometry() {
        assert!(!position(0.0, 100.0, 20.0, 80.0).unwrap().reading);
        assert_eq!(
            position(0.0, 110.0, 20.0, 80.0),
            Some(Position {
                reading: true,
                rows_above_live: 10
            })
        );
        assert!(!position(20.0, 25.0, 40.0, 20.0).unwrap().reading);
        for input in [
            (0.0, 2.0, 0.0, 0.0),
            (5.0, 2.0, 1.0, 0.0),
            (0.0, f64::INFINITY, 1.0, 0.0),
            (0.0, 3.0, 1.0, f64::NAN),
        ] {
            assert!(position(input.0, input.1, input.2, input.3).is_none());
        }
    }

    #[test]
    fn explicit_commands_do_not_wrap_and_retired_identity_cannot_select_a_replacement() {
        let records = [(10, None), (20, None), (30, None)];
        assert_eq!(step(&records, None, false, 80.0, true), Step::Record(30));
        assert_eq!(step(&records, Some(30), true, 80.0, true), Step::Record(20));
        assert_eq!(step(&records, Some(10), true, 0.0, true), Step::Oldest);
        assert_eq!(step(&records, Some(30), true, 80.0, false), Step::Live);
        assert_eq!(step(&records, Some(15), true, 40.0, false), Step::Stale);
        assert_eq!(step(&[], None, false, 0.0, true), Step::Empty);
    }

    #[test]
    fn viewport_navigation_uses_only_proven_heads_with_stable_bottom_ties() {
        let records = [(1, Some(10.0)), (2, None), (3, Some(80.0)), (4, Some(80.0))];
        assert_eq!(step(&records, None, true, 40.0, false), Step::Record(3));
        assert_eq!(step(&records, None, true, 40.0, true), Step::Record(1));
        assert_eq!(step(&records, Some(3), true, 80.0, false), Step::Record(4));
        assert_eq!(step(&records, None, true, 90.0, true), Step::Record(4));
        assert_eq!(step(&records, None, true, 90.0, false), Step::Live);
        assert_eq!(step(&[(1, None)], None, true, 40.0, true), Step::Unproven);
        assert_eq!(
            step(&[(1, Some(f64::NAN))], None, true, 40.0, false),
            Step::Unproven
        );
    }

    #[test]
    fn command_labels_are_bounded_single_line_and_do_not_render_direction_overrides() {
        assert_eq!(
            command_preview("echo one\n\u{202e}<two>"),
            "echo one  <two>"
        );
        assert_eq!(command_preview(&"界".repeat(200)).chars().count(), 160);
    }

    #[test]
    fn command_copy_preserves_structure_but_never_copies_a_partial_or_hidden_control_command() {
        for command in [
            "printf '你好 🦀'",
            "for x in one two; do\n\tprintf '%s\\n' \"$x\"\ndone",
        ] {
            assert!(command_is_copyable(command, false));
            assert!(!command_is_copyable(command, true));
        }
        for command in [
            "",
            " \n\t",
            "echo\u{1b}[31mred",
            "echo\rhidden",
            "echo\u{202e}hidden",
            "echo\u{200b}hidden",
        ] {
            assert!(!command_is_copyable(command, false));
        }
        assert!(command_is_copyable(
            &"a".repeat(MAX_COPY_COMMAND_BYTES),
            false
        ));
        assert!(!command_is_copyable(
            &"a".repeat(MAX_COPY_COMMAND_BYTES + 1),
            false
        ));
    }
}
