//! Native navigation for the Vim workbench. Text input stays in GTK; actual
//! buffer changes, cursor movement and modified-buffer decisions belong to Vim.

use super::*;
use jterm_core::vim9::{Vim9ProjectFile, Vim9ProjectIndex};
use std::sync::Arc;

fn plain_key(modifiers: gdk::ModifierType) -> bool {
    !modifiers.intersects(
        gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::HYPER_MASK
            | gdk::ModifierType::META_MASK,
    )
}

pub(super) fn native_text_has_focus(window: &adw::ApplicationWindow) -> bool {
    let Some(focus) = gtk::prelude::GtkWindowExt::focus(window) else {
        return false;
    };
    // Entry/SearchEntry usually delegate keyboard focus to an internal GtkText.
    // Checking the Editable interface includes that delegate, without treating
    // the read-only output panel or file lists as text-entry fields.
    focus.is::<gtk::Editable>()
        || focus.ancestor(gtk::Entry::static_type()).is_some()
        || focus.ancestor(gtk::SearchEntry::static_type()).is_some()
}

/// libadwaita 1.5 can lose close() while its first two map frames open a sheet.
/// Keep the explicit dismissal pending until `closed`, using frame callbacks
/// rather than delaying input or requiring a newer system libadwaita.
fn close_dialog(dialog: &impl IsA<adw::Dialog>) {
    let dialog: &adw::Dialog = dialog.as_ref();
    let closed = Rc::new(Cell::new(false));
    let observed = closed.clone();
    dialog.connect_closed(move |_| observed.set(true));
    dialog.close();
    if closed.get() {
        return;
    }
    dialog.add_tick_callback(move |dialog, _| {
        if closed.get() || !dialog.is_mapped() {
            return glib::ControlFlow::Break;
        }
        dialog.close();
        if closed.get() {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// An explicit line[:column] input never becomes an Ex command or shell text.
fn parse_position(value: &str) -> Result<(u64, Option<u64>), &'static str> {
    let mut parts = value.trim().split(':');
    let line = parts
        .next()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| (1..=i32::MAX as u64).contains(value))
        .ok_or("Enter a line from 1 to 2147483647, optionally followed by :column.")?;
    let column = parts
        .next()
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=i32::MAX as u64).contains(value))
                .ok_or("The column must be from 1 to 2147483647.")
        })
        .transpose()?;
    if parts.next().is_some() {
        return Err("Use line or line:column, for example 42:8.");
    }
    Ok((line, column))
}

pub(super) fn go_to_line(
    window: &adw::ApplicationWindow,
    session: &Vim9Session,
    editor: &vte4::Terminal,
    message: &gtk::Label,
) {
    let dialog = adw::AlertDialog::new(Some("Go to line"), Some("Enter line or line:column"));
    dialog.add_responses(&[("cancel", "Cancel"), ("go", "Go")]);
    dialog.set_default_response(Some("go"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("go", adw::ResponseAppearance::Suggested);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("Line or line:column"));
    entry.set_text(&session.snapshot().line.max(1).to_string());
    entry.select_region(0, -1);
    entry.set_activates_default(true);
    let error = label("");
    error.set_wrap(true);
    error.add_css_class("error");
    content.append(&entry);
    content.append(&error);
    dialog.set_extra_child(Some(&content));
    dialog.set_focus(Some(&entry));
    let weak = dialog.downgrade();
    entry.connect_changed(move |entry| {
        let valid = parse_position(&entry.text());
        error.set_label(valid.as_ref().err().copied().unwrap_or(""));
        if let Some(dialog) = weak.upgrade() {
            dialog.set_response_enabled("go", valid.is_ok());
        }
    });
    let session = session.clone();
    let editor = editor.clone();
    let message = message.clone();
    let closing_dialog = dialog.clone();
    dialog.choose(Some(window), None::<&gio::Cancellable>, move |response| {
        close_dialog(&closing_dialog);
        if response == "go" {
            if let Ok((line, column)) = parse_position(&entry.text()) {
                dispatch(
                    &session,
                    Vim9Command::Goto { line, column },
                    &message,
                    &editor,
                );
            }
        }
        editor.grab_focus();
    });
}

/// Escape in a native field returns to Vim without consuming Vim's own Escape.
pub(super) fn editor_escape(widget: &impl IsA<gtk::Widget>, editor: &vte4::Terminal) {
    let keys = gtk::EventControllerKey::new();
    keys.set_name(Some("vim9-native-field-escape"));
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let editor = editor.clone();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if key == gdk::Key::Escape && plain_key(modifiers) {
            editor.grab_focus();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    widget.add_controller(keys);
}

/// Keep typing in the search entry while navigating the selected native row.
pub(super) fn list_navigation(entry: &gtk::SearchEntry, list: &gtk::ListBox) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let list = list.clone();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if !plain_key(modifiers) || (key != gdk::Key::Down && key != gdk::Key::Up) {
            return glib::Propagation::Proceed;
        }
        let current = list.selected_row().map(|row| row.index());
        let next = match (key == gdk::Key::Down, current) {
            (true, Some(current)) => current + 1,
            (false, Some(current)) => (current - 1).max(0),
            (_, None) => 0,
        };
        if let Some(row) = list.row_at_index(next) {
            list.select_row(Some(&row));
            if let Some(scroll) = list
                .ancestor(gtk::ScrolledWindow::static_type())
                .and_then(|widget| widget.downcast::<gtk::ScrolledWindow>().ok())
            {
                if let Some(bounds) = row.compute_bounds(&list) {
                    let adjustment = scroll.vadjustment();
                    let top = f64::from(bounds.y());
                    let bottom = top + f64::from(bounds.height());
                    if top < adjustment.value() {
                        adjustment.set_value(top);
                    } else if bottom > adjustment.value() + adjustment.page_size() {
                        adjustment.set_value(bottom - adjustment.page_size());
                    }
                }
            }
        }
        glib::Propagation::Stop
    });
    entry.add_controller(keys);
}

pub(super) struct QuickOpen {
    index: Rc<RefCell<Vim9ProjectIndex>>,
    dialog: adw::Dialog,
    query: gtk::SearchEntry,
    list: gtk::ListBox,
    feedback: gtk::Label,
    matches: RefCell<Vec<Vim9ProjectFile>>,
    last_files: RefCell<Option<Arc<Vec<Vim9ProjectFile>>>>,
    last_query: RefCell<String>,
    dismissing: Cell<bool>,
    closed: Cell<bool>,
    session: Vim9Session,
    editor: vte4::Terminal,
    message: gtk::Label,
}

impl QuickOpen {
    pub(super) fn show(
        window: &adw::ApplicationWindow,
        index: Rc<RefCell<Vim9ProjectIndex>>,
        session: &Vim9Session,
        editor: &vte4::Terminal,
        message: &gtk::Label,
    ) {
        let dialog = adw::Dialog::new();
        dialog.set_title("Quick open project file");
        dialog.set_content_width(620);
        dialog.set_content_height(480);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new("Quick open", "Project files")));
        let refresh = button("view-refresh-symbolic", "Refresh project file index");
        header.pack_start(&refresh);
        content.append(&header);
        let query = gtk::SearchEntry::new();
        query.set_placeholder_text(Some("Type a filename or path…"));
        query.set_search_delay(60);
        query.set_margin_start(12);
        query.set_margin_end(12);
        content.append(&query);
        let feedback = label("Indexing project files…");
        feedback.set_margin_start(12);
        feedback.set_margin_end(12);
        feedback.add_css_class("caption");
        feedback.set_wrap(true);
        content.append(&feedback);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("navigation-sidebar");
        content.append(&scroll(&list));
        let help = label("↑ ↓ select · Enter open · Escape return to Vim");
        help.add_css_class("caption");
        help.set_margin_start(12);
        help.set_margin_bottom(12);
        content.append(&help);
        dialog.set_child(Some(&content));
        list_navigation(&query, &list);
        let this = Rc::new(Self {
            index,
            dialog,
            query,
            list,
            feedback,
            matches: RefCell::new(Vec::new()),
            last_files: RefCell::new(None),
            last_query: RefCell::new(String::new()),
            dismissing: Cell::new(false),
            closed: Cell::new(false),
            session: session.clone(),
            editor: editor.clone(),
            message: message.clone(),
        });
        let weak = Rc::downgrade(&this);
        this.query.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.sync();
            }
        });
        let weak = Rc::downgrade(&this);
        this.query.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                // Enter can arrive before SearchEntry's debounced change signal.
                // Reconcile the current query before resolving its selected row.
                this.sync();
                let selected = this.list.selected_row().map_or(0, |row| row.index());
                this.activate(selected);
            }
        });
        let weak = Rc::downgrade(&this);
        this.query.connect_stop_search(move |_| {
            if let Some(this) = weak.upgrade() {
                this.dismiss();
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                this.activate(row.index());
            }
        });
        let weak = Rc::downgrade(&this);
        refresh.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                *this.index.borrow_mut() = Vim9ProjectIndex::start(this.session.root());
                this.sync();
            }
        });
        let weak = Rc::downgrade(&this);
        this.dialog.connect_closed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.closed.set(true);
                this.editor.grab_focus();
            }
        });
        this.dialog.present(Some(window));
        this.query.grab_focus();
        this.sync();
        let weak_window = window.downgrade();
        // Only one small timer owns the picker; closing it drops all callbacks.
        glib::timeout_add_local(Duration::from_millis(100), move || {
            if this.closed.get()
                || weak_window
                    .upgrade()
                    .is_none_or(|window| !window.is_visible())
            {
                return glib::ControlFlow::Break;
            }
            this.sync();
            glib::ControlFlow::Continue
        });
    }

    fn sync(&self) {
        let snapshot = self.index.borrow().snapshot();
        let query = self.query.text().to_string();
        let changed = self
            .last_files
            .borrow()
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, &snapshot.files))
            || *self.last_query.borrow() != query;
        if changed {
            let selected = self.list.selected_row().and_then(|row| {
                self.matches
                    .borrow()
                    .get(row.index() as usize)
                    .map(|file| file.path.clone())
            });
            let matches = snapshot.matches(&query, 100);
            self.list.remove_all();
            let mut selected_index = 0;
            for (i, file) in matches.iter().enumerate() {
                let row = label(&visible(&file.relative));
                row.set_margin_start(12);
                row.set_margin_end(12);
                row.set_margin_top(7);
                row.set_margin_bottom(7);
                row.set_tooltip_text(Some(&visible(&file.path.display().to_string())));
                self.list.append(&row);
                if selected.as_ref() == Some(&file.path) {
                    selected_index = i as i32;
                }
            }
            self.list
                .select_row(self.list.row_at_index(selected_index).as_ref());
            *self.matches.borrow_mut() = matches;
            *self.last_files.borrow_mut() = Some(snapshot.files.clone());
            *self.last_query.borrow_mut() = query;
        }
        self.feedback
            .set_label(&if let Some(error) = &snapshot.error {
                format!("Index error: {} · Refresh to retry.", visible(error))
            } else if snapshot.scanning {
                "Indexing project files…".into()
            } else if self.matches.borrow().is_empty() {
                format!("No matching files · {} indexed", snapshot.files.len())
            } else {
                format!(
                    "{} shown · {} indexed{}",
                    self.matches.borrow().len(),
                    snapshot.files.len(),
                    if snapshot.truncated {
                        " · index limit reached"
                    } else {
                        ""
                    },
                )
            });
    }

    fn dismiss(&self) {
        if !self.dismissing.replace(true) {
            close_dialog(&self.dialog);
        }
    }

    fn activate(&self, index: i32) {
        if self.dismissing.get() {
            return;
        }
        let Some(file) = self.matches.borrow().get(index as usize).cloned() else {
            return;
        };
        let Some(path) = file.path.to_str() else {
            self.feedback
                .set_label("Vim9 IDE currently requires UTF-8 file paths.");
            return;
        };
        match self.session.send(Vim9Command::Open {
            path: path.into(),
            line: None,
            column: None,
        }) {
            Ok(_) => {
                self.message.set_label("");
                self.dismiss();
            }
            Err(error) => self.feedback.set_label(&visible(&error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_position;

    #[test]
    fn positions_accept_only_positive_numeric_coordinates() {
        assert_eq!(parse_position(" 42 "), Ok((42, None)));
        assert_eq!(parse_position("7: 3"), Ok((7, Some(3))));
        for input in [
            "",
            "0",
            "-1",
            "1:0",
            "1:",
            "1:2:3",
            "2147483648",
            "1:2147483648",
            "2|quit!",
            "1\nqa!",
            "18446744073709551616",
        ] {
            assert!(
                parse_position(input).is_err(),
                "accepted invalid position {input:?}"
            );
        }
    }
}
