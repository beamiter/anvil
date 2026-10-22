//! Native navigation for the Vim workbench. Text input stays in GTK; actual
//! buffer changes, cursor movement and modified-buffer decisions belong to Vim.

use super::*;
use jterm_core::vim9::{
    parse_vim9_location as parse_position, vim9_palette_matches, vim9_palette_query,
    Vim9PaletteCommand, Vim9ProjectFile, Vim9ProjectIndex, Vim9QuickOpenQuery, Vim9Snapshot,
};
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
pub(super) fn close_dialog(dialog: &impl IsA<adw::Dialog>) {
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

const EDITOR_DIALOG_NAME: &str = "vim9-native-navigation-dialog";

pub(super) fn owns_dialog(dialog: &adw::Dialog) -> bool {
    dialog.widget_name() == EDITOR_DIALOG_NAME
}

pub(super) fn go_to_line(
    window: &adw::ApplicationWindow,
    session: &Vim9Session,
    editor: &vte4::Terminal,
    message: &gtk::Label,
) {
    let dialog = adw::AlertDialog::new(Some("Go to line"), Some("Enter line or line:column"));
    dialog.set_widget_name(EDITOR_DIALOG_NAME);
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum QuickOpenTarget {
    File {
        file: Option<Vim9ProjectFile>,
        query: Vim9QuickOpenQuery,
    },
    Command(Vim9PaletteCommand),
}

impl QuickOpenTarget {
    fn same_item(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Command(a), Self::Command(b)) => a == b,
            (Self::File { file: a, .. }, Self::File { file: b, .. }) => {
                a.as_ref().map(|file| &file.path) == b.as_ref().map(|file| &file.path)
            }
            _ => false,
        }
    }

    fn command(&self, snapshot: &Vim9Snapshot) -> Result<Vim9Command, &'static str> {
        match self {
            Self::File { file, query } => query.command(file.as_ref()),
            Self::Command(command) => command.command(snapshot),
        }
    }
}

pub(super) struct QuickOpen {
    index: Rc<RefCell<Vim9ProjectIndex>>,
    dialog: adw::Dialog,
    title: adw::WindowTitle,
    refresh: gtk::Button,
    help: gtk::Label,
    query: gtk::SearchEntry,
    list: gtk::ListBox,
    feedback: gtk::Label,
    targets: RefCell<Vec<QuickOpenTarget>>,
    parsed_query: RefCell<Result<Vim9QuickOpenQuery, &'static str>>,
    last_files: RefCell<Option<Arc<Vec<Vim9ProjectFile>>>>,
    last_commands: RefCell<Vec<(Vim9PaletteCommand, Option<&'static str>)>>,
    last_query: RefCell<String>,
    command_mode: Cell<bool>,
    activation_error: RefCell<Option<(String, String)>>,
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
        dialog.set_widget_name(EDITOR_DIALOG_NAME);
        dialog.set_title("Quick open project file");
        dialog.set_content_width(620);
        dialog.set_content_height(480);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let header = adw::HeaderBar::new();
        let title = adw::WindowTitle::new("Quick open", "Project files");
        header.set_title_widget(Some(&title));
        let refresh = button("view-refresh-symbolic", "Refresh project file index");
        header.pack_start(&refresh);
        content.append(&header);
        let query = gtk::SearchEntry::new();
        query.set_placeholder_text(Some("File[:line[:column]], :line[:column], or >command…"));
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
        let help = label("↑ ↓ select · Enter open · Escape return to Vim · > commands");
        help.add_css_class("caption");
        help.set_margin_start(12);
        help.set_margin_bottom(12);
        content.append(&help);
        dialog.set_child(Some(&content));
        list_navigation(&query, &list);
        let this = Rc::new(Self {
            index,
            dialog,
            title,
            refresh,
            help,
            query,
            list,
            feedback,
            targets: RefCell::new(Vec::new()),
            parsed_query: RefCell::new(Err("Enter a file or :line[:column].")),
            last_files: RefCell::new(None),
            last_commands: RefCell::new(Vec::new()),
            last_query: RefCell::new(String::new()),
            command_mode: Cell::new(false),
            activation_error: RefCell::new(None),
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
                // Enter can precede SearchEntry's debounced change signal.
                this.sync();
                let target = this.list.selected_row().and_then(|row| {
                    usize::try_from(row.index())
                        .ok()
                        .and_then(|index| this.targets.borrow().get(index).cloned())
                });
                if let Some(target) = target {
                    this.activate(&target);
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.query.connect_stop_search(move |_| {
            if let Some(this) = weak.upgrade() {
                this.dismiss();
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_selected(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.activation_error.borrow_mut().take();
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                // Capture the displayed identity before sync can remove the
                // row. A detached GTK row has index -1 (meaning "last row"
                // to row_at_index), so it must never select a new action.
                let Some(index) = usize::try_from(row.index()).ok() else {
                    return;
                };
                if this.list.row_at_index(index as i32).as_ref() != Some(row) {
                    return;
                }
                let target = this.targets.borrow().get(index).cloned();
                let shown_query = this.last_query.borrow().clone();
                this.sync();
                if shown_query == this.query.text().as_str() {
                    if let Some(target) = target {
                        this.activate(&target);
                    }
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.refresh.connect_clicked(move |_| {
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
        if self.dismissing.get() || self.closed.get() {
            return;
        }
        let query = self.query.text().to_string();
        let editor = self.session.snapshot();
        if let Some(command_query) = vim9_palette_query(&query) {
            self.sync_commands(&query, command_query, &editor);
        } else {
            self.sync_files(&query, &editor);
        }
        // Plugin context refreshes independently of the query and file index.
        // Keep it inside the transient picker when persistent chrome is off.
        let kind = if self.command_mode.get() {
            "Commands"
        } else {
            "Project files"
        };
        let context = editor.workbench.status_text();
        self.title.set_subtitle(&if context.is_empty() {
            kind.to_owned()
        } else {
            format!("{kind} · {}", visible(&context))
        });
    }

    fn sync_commands(&self, query: &str, command_query: &str, editor: &Vim9Snapshot) {
        let commands = vim9_palette_matches(command_query, 100)
            .into_iter()
            .map(|command| (command, command.unavailable_reason(editor)))
            .collect::<Vec<_>>();
        // Availability can change while the query and file-index Arc stay
        // fixed: the bridge, jobs, jump history and language server are live.
        let changed = !self.command_mode.replace(true)
            || *self.last_query.borrow() != query
            || *self.last_commands.borrow() != commands;
        if changed {
            self.replace_rows(
                commands
                    .iter()
                    .map(|(command, _)| QuickOpenTarget::Command(*command))
                    .collect(),
                editor,
            );
            *self.last_commands.borrow_mut() = commands;
            *self.last_query.borrow_mut() = query.to_owned();
            self.dialog.set_title("Quick open command");
            self.title.set_subtitle("Commands");
            self.refresh.set_visible(false);
            self.help
                .set_label("↑ ↓ select · Enter run · Escape return to Vim");
        }
        let count = self.targets.borrow().len();
        self.set_feedback(
            if count == 0 {
                "No matching commands · Edit the text after >".into()
            } else {
                format!("{count} commands · Enter to run")
            },
            false,
        );
    }

    fn sync_files(&self, query: &str, editor: &Vim9Snapshot) {
        let snapshot = self.index.borrow().snapshot();
        let changed = self.command_mode.replace(false)
            || self
                .last_files
                .borrow()
                .as_ref()
                .is_none_or(|old| !Arc::ptr_eq(old, &snapshot.files))
            || *self.last_query.borrow() != query;
        if changed {
            let parsed = snapshot.quick_open_query(query);
            let targets = parsed.as_ref().map_or_else(
                |_| Vec::new(),
                |query| {
                    if query.current_buffer {
                        vec![QuickOpenTarget::File {
                            file: None,
                            query: query.clone(),
                        }]
                    } else {
                        snapshot
                            .matches(&query.file_query, 100)
                            .into_iter()
                            .map(|file| QuickOpenTarget::File {
                                file: Some(file),
                                query: query.clone(),
                            })
                            .collect()
                    }
                },
            );
            self.replace_rows(targets, editor);
            *self.last_files.borrow_mut() = Some(snapshot.files.clone());
            *self.last_query.borrow_mut() = query.to_owned();
            *self.parsed_query.borrow_mut() = parsed;
            self.dialog.set_title("Quick open project file");
            self.title.set_subtitle("Project files");
            self.refresh.set_visible(true);
            self.help
                .set_label("↑ ↓ select · Enter open · Escape return to Vim · > commands");
        }
        let parsed = self.parsed_query.borrow();
        let feedback = if let Err(error) = parsed.as_ref() {
            (*error).into()
        } else if let Some(query) = parsed.as_ref().ok().filter(|query| query.current_buffer) {
            format!("Current buffer · {} · Enter to go", destination(query))
        } else if let Some(error) = &snapshot.error {
            format!("Index error: {} · Refresh to retry.", visible(error))
        } else if snapshot.scanning {
            "Indexing project files…".into()
        } else if self.targets.borrow().is_empty() {
            format!("No matching files · {} indexed", snapshot.files.len())
        } else {
            format!(
                "{} shown · {} indexed{}{}",
                self.targets.borrow().len(),
                snapshot.files.len(),
                if snapshot.truncated {
                    " · index limit reached"
                } else {
                    ""
                },
                parsed
                    .as_ref()
                    .ok()
                    .filter(|query| query.location.is_some())
                    .map_or_else(String::new, |query| format!(" · {}", destination(query))),
            )
        };
        self.set_feedback(feedback, parsed.is_err());
    }

    fn replace_rows(&self, targets: Vec<QuickOpenTarget>, editor: &Vim9Snapshot) {
        let selected = self.list.selected_row().and_then(|row| {
            usize::try_from(row.index())
                .ok()
                .and_then(|index| self.targets.borrow().get(index).cloned())
        });
        self.activation_error.borrow_mut().take();
        self.list.remove_all();
        let mut selected_index = 0;
        for (index, target) in targets.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 3);
            row.set_margin_start(12);
            row.set_margin_end(12);
            row.set_margin_top(7);
            row.set_margin_bottom(7);
            match target {
                QuickOpenTarget::File {
                    file: Some(file), ..
                } => {
                    row.append(&label(&visible(&file.relative)));
                    row.set_tooltip_text(Some(&visible(&file.path.display().to_string())));
                }
                QuickOpenTarget::File { query, .. } => {
                    row.append(&label(&format!(
                        "Go to {} · Current buffer",
                        destination(query)
                    )));
                }
                QuickOpenTarget::Command(command) => {
                    let title = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                    let name = label(command.title());
                    name.set_hexpand(true);
                    title.append(&name);
                    if !command.shortcut().is_empty() {
                        let shortcut = label(command.shortcut());
                        shortcut.add_css_class("dim-label");
                        title.append(&shortcut);
                    }
                    row.append(&title);
                    if let Some(reason) = command.unavailable_reason(editor) {
                        // Keep this row selectable so Enter can explain why
                        // it cannot run, without closing the picker.
                        name.add_css_class("dim-label");
                        let reason = label(reason);
                        reason.set_wrap(true);
                        reason.add_css_class("caption");
                        reason.add_css_class("dim-label");
                        row.append(&reason);
                    }
                }
            }
            self.list.append(&row);
            if selected.as_ref().is_some_and(|old| old.same_item(target)) {
                selected_index = index as i32;
            }
        }
        *self.targets.borrow_mut() = targets;
        self.list
            .select_row(self.list.row_at_index(selected_index).as_ref());
    }

    fn set_feedback(&self, text: String, is_error: bool) {
        let activation_error = self.activation_error.borrow();
        let error = activation_error
            .as_ref()
            .filter(|(query, _)| query == self.query.text().as_str())
            .map(|(_, error)| error);
        if is_error || error.is_some() {
            self.feedback.add_css_class("error");
        } else {
            self.feedback.remove_css_class("error");
        }
        self.feedback
            .set_label(error.map_or(text.as_str(), String::as_str));
    }

    fn report_error(&self, error: &str) {
        *self.activation_error.borrow_mut() =
            Some((self.query.text().to_string(), error.to_owned()));
        self.feedback.add_css_class("error");
        self.feedback.set_label(error);
    }

    fn dismiss(&self) {
        if !self.dismissing.replace(true) {
            close_dialog(&self.dialog);
        }
    }

    fn activate(&self, target: &QuickOpenTarget) {
        if self.dismissing.get() || !self.targets.borrow().contains(target) {
            return;
        }
        // Query and stable row identity were reconciled by the caller. Check
        // live eligibility again immediately before sending the typed action.
        let command = match target.command(&self.session.snapshot()) {
            Ok(command) => command,
            Err(error) => {
                self.report_error(error);
                return;
            }
        };
        match self.session.send(command) {
            Ok(_) => {
                self.message.set_label("");
                self.dismiss();
            }
            Err(error) => self.report_error(&visible(&error.to_string())),
        }
    }
}

fn destination(query: &Vim9QuickOpenQuery) -> String {
    match query.location {
        Some((line, Some(column))) => format!("line {line}, byte column {column}"),
        Some((line, None)) => format!("line {line}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_rows_preserve_selection_but_do_not_replay_an_old_destination() {
        let file = Vim9ProjectFile {
            path: PathBuf::from("/project/sample.txt"),
            relative: "sample.txt".into(),
        };
        let query = Vim9QuickOpenQuery {
            file_query: "sample".into(),
            location: Some((2, None)),
            current_buffer: false,
        };
        let before = QuickOpenTarget::File {
            file: Some(file.clone()),
            query: query.clone(),
        };
        let after = QuickOpenTarget::File {
            file: Some(file),
            query: Vim9QuickOpenQuery {
                location: Some((3, None)),
                ..query
            },
        };
        assert!(before.same_item(&after));
        assert_ne!(
            before, after,
            "a delayed row must retain its original location identity"
        );
        let command = QuickOpenTarget::Command(Vim9PaletteCommand::Save);
        assert!(!before.same_item(&command));
        assert!(command.same_item(&QuickOpenTarget::Command(Vim9PaletteCommand::Save)));
        assert!(!command.same_item(&QuickOpenTarget::Command(Vim9PaletteCommand::SaveAll)));
    }

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
