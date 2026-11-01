//! Additional native/API conversations in the persistent workspace sidebar.
//! Stable identities keep rows alive across stream updates; selection alone
//! never creates a process or sends a provider request.
use gtk::prelude::*;
use jterm_core::agent_task::TaskId;
use relm4::gtk;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Api(u64),
    Cli(jterm_core::agent_workspace::ConversationId),
    PinCli(jterm_core::agent_workspace::ConversationId, bool),
    ArchiveCli(jterm_core::agent_workspace::ConversationId, bool),
    SharedApi(jterm_core::agent_workspace::ConversationId),
    Task(TaskId, jterm_core::agent_workspace::ConversationId),
    ExternalTask(jterm_core::agent_workspace::ConversationId),
    NewApi,
    NewTask,
    NewTerminal,
    PinShared(jterm_core::agent_workspace::ConversationId, bool),
    ArchiveShared(jterm_core::agent_workspace::ConversationId, bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub action: Action,
    pub title: String,
    pub detail: String,
    pub searchable: String,
    pub active: bool,
    pub busy: bool,
    pub attention: bool,
    pub archived: bool,
    pub pinned: bool,
    pub project: String,
    pub provider: String,
    pub updated: u64,
}

#[derive(Default)]
pub(crate) struct Filter {
    pub query: String,
    pub provider: Option<String>,
    pub needs_attention: bool,
    pub running: bool,
    pub pinned: bool,
    pub archive: u32,
    pub project: Option<std::path::PathBuf>,
}

impl Filter {
    fn matches(&self, row: &Row) -> bool {
        if (self.archive == 0 && row.archived)
            || (self.archive == 1 && !row.archived)
            || (self.needs_attention && !row.attention)
            || (self.running && !row.busy)
            || (self.pinned && !row.pinned)
            || self
                .provider
                .as_ref()
                .is_some_and(|provider| !row.provider.eq_ignore_ascii_case(provider))
            || self
                .project
                .as_ref()
                .is_some_and(|project| !std::path::Path::new(&row.project).starts_with(project))
        {
            return false;
        }
        let text = format!(
            "{} {} {} {} {}",
            row.title, row.detail, row.searchable, row.provider, row.project
        )
        .to_lowercase();
        self.query.split_whitespace().all(|word| {
            let (negative, word) = word
                .strip_prefix('-')
                .map_or((false, word), |word| (true, word));
            let word = word.trim_matches('"').to_lowercase();
            let matches = match word.as_str() {
                "is:pinned" => row.pinned,
                "is:archived" => row.archived,
                "is:live" | "is:running" => row.busy,
                "is:waiting" | "is:attention" => row.attention,
                _ => {
                    if let Some(provider) = word.strip_prefix("provider:") {
                        row.provider.eq_ignore_ascii_case(provider)
                    } else {
                        text.contains(&word)
                    }
                }
            };
            matches != negative
        })
    }
}

#[derive(Clone)]
pub(crate) struct Navigation {
    pub root: gtk::Box,
    list: gtk::ListBox,
    rows: Rc<RefCell<Vec<Row>>>,
    widgets: Rc<RefCell<Vec<(gtk::ListBoxRow, gtk::Label, gtk::Label)>>>,
    callback: Rc<dyn Fn(Action)>,
}

impl Navigation {
    pub fn new(callback: impl Fn(Action) + 'static) -> Self {
        let callback: Rc<dyn Fn(Action)> = Rc::new(callback);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let new = gtk::MenuButton::new();
        new.set_label("＋ New conversation");
        new.set_hexpand(true);
        let popover = gtk::Popover::new();
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for (label, action) in [
            ("AI conversation", Action::NewApi),
            ("Agent task", Action::NewTask),
            ("Terminal", Action::NewTerminal),
        ] {
            let button = gtk::Button::with_label(label);
            button.add_css_class("flat");
            let callback = callback.clone();
            let popover = popover.clone();
            button.connect_clicked(move |_| {
                popover.popdown();
                callback(action.clone());
            });
            choices.append(&button);
        }
        popover.set_child(Some(&choices));
        new.set_popover(Some(&popover));
        root.append(&new);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        list.add_css_class("navigation-sidebar");
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        scroll.set_propagate_natural_height(false);
        scroll.set_child(Some(&list));
        root.set_vexpand(true);
        root.append(&scroll);
        let rows = Rc::new(RefCell::new(Vec::<Row>::new()));
        let sections = rows.clone();
        list.set_header_func(move |row, previous| {
            let rows = sections.borrow();
            let Some(current) = rows.get(row.index() as usize) else {
                row.set_header(None::<&gtk::Widget>);
                return;
            };
            let same_section = previous
                .and_then(|previous| rows.get(previous.index() as usize))
                .is_some_and(|previous| previous.pinned == current.pinned);
            if same_section {
                row.set_header(None::<&gtk::Widget>);
            } else {
                let section =
                    gtk::Label::new(Some(if current.pinned { "PINNED" } else { "RECENT" }));
                section.set_xalign(0.0);
                section.set_margin_start(10);
                section.set_margin_top(10);
                section.set_margin_bottom(4);
                section.add_css_class("dim-label");
                row.set_header(Some(&section));
            }
        });
        let selected = rows.clone();
        let activate = callback.clone();
        list.connect_row_activated(move |_, row| {
            let action = selected
                .borrow()
                .get(row.index() as usize)
                .map(|row| row.action.clone());
            if let Some(action) = action {
                activate(action);
            }
        });
        // GtkListBox normally activates on Space. Navigation keys, including
        // Space, only browse; only a click or a distinct Enter opens a row.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(|_, key, _, _| {
            if matches!(key, gtk::gdk::Key::space | gtk::gdk::Key::KP_Space) {
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        list.add_controller(keys);
        Self {
            root,
            list,
            rows,
            widgets: Rc::new(RefCell::new(Vec::new())),
            callback,
        }
    }

    pub fn navigate(&self, key: gtk::gdk::Key) -> bool {
        let count = self.rows.borrow().len();
        if count == 0 {
            return false;
        }
        let current = self.list.selected_row().map(|row| row.index() as usize);
        let next = match key {
            gtk::gdk::Key::Down => current.map_or(0, |i| (i + 1).min(count - 1)),
            gtk::gdk::Key::Up => current.map_or(0, |i| i.saturating_sub(1)),
            gtk::gdk::Key::Home => 0,
            gtk::gdk::Key::End => count - 1,
            gtk::gdk::Key::Page_Down => current.unwrap_or(0).saturating_add(10).min(count - 1),
            gtk::gdk::Key::Page_Up => current.unwrap_or(0).saturating_sub(10),
            _ => return false,
        };
        self.list
            .select_row(self.list.row_at_index(next as i32).as_ref());
        true
    }

    pub fn activate_selected(&self) {
        let action = self.list.selected_row().and_then(|row| {
            self.rows
                .borrow()
                .get(row.index() as usize)
                .map(|row| row.action.clone())
        });
        if let Some(action) = action {
            (self.callback)(action);
        }
    }

    pub fn sync(&self, rows: Vec<Row>, filter: &Filter) {
        let mut visible: Vec<Row> = rows.into_iter().filter(|row| filter.matches(row)).collect();
        visible.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.updated.cmp(&a.updated)));
        let same_ids = self
            .rows
            .borrow()
            .iter()
            .map(|row| (&row.action, row.pinned, row.archived))
            .eq(visible
                .iter()
                .map(|row| (&row.action, row.pinned, row.archived)));
        if !same_ids {
            let selected = self.list.selected_row().and_then(|row| {
                self.rows
                    .borrow()
                    .get(row.index() as usize)
                    .map(|row| row.action.clone())
            });
            for (row, _, _) in self.widgets.borrow_mut().drain(..) {
                self.list.remove(&row);
            }
            for row in &visible {
                let widget = gtk::ListBoxRow::new();
                let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
                content.set_margin_top(7);
                content.set_margin_bottom(7);
                content.set_margin_start(10);
                content.set_margin_end(8);
                let title = gtk::Label::new(None);
                title.set_xalign(0.0);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                let detail = gtk::Label::new(None);
                detail.set_xalign(0.0);
                detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
                detail.add_css_class("dim-label");
                content.append(&title);
                content.append(&detail);
                if matches!(
                    &row.action,
                    Action::SharedApi(_)
                        | Action::Cli(_)
                        | Action::Task(_, _)
                        | Action::ExternalTask(_)
                ) {
                    let (pin, archive) = match &row.action {
                        Action::SharedApi(id) => (
                            Action::PinShared(id.clone(), !row.pinned),
                            Action::ArchiveShared(id.clone(), !row.archived),
                        ),
                        Action::Cli(id) => (
                            Action::PinCli(id.clone(), !row.pinned),
                            Action::ArchiveCli(id.clone(), !row.archived),
                        ),
                        Action::Task(_, id) | Action::ExternalTask(id) => (
                            Action::PinCli(id.clone(), !row.pinned),
                            Action::ArchiveCli(id.clone(), !row.archived),
                        ),
                        _ => unreachable!(),
                    };
                    let menu = gtk::MenuButton::new();
                    menu.set_icon_name("view-more-symbolic");
                    menu.add_css_class("flat");
                    menu.set_tooltip_text(Some("Conversation actions"));
                    let actions = gtk::Box::new(gtk::Orientation::Vertical, 4);
                    let popover = gtk::Popover::new();
                    for (label, action) in [
                        (if row.pinned { "Unpin" } else { "Pin" }, pin),
                        (if row.archived { "Restore" } else { "Archive" }, archive),
                    ] {
                        if (label == "Archive" || label == "Restore")
                            && matches!(&row.action, Action::Task(_, _) | Action::ExternalTask(_))
                        {
                            continue;
                        }
                        let button = gtk::Button::with_label(label);
                        let callback = self.callback.clone();
                        let popover = popover.clone();
                        button.connect_clicked(move |_| {
                            popover.popdown();
                            callback(action.clone());
                        });
                        actions.append(&button);
                    }
                    popover.set_child(Some(&actions));
                    menu.set_popover(Some(&popover));
                    let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    content.set_hexpand(true);
                    line.append(&content);
                    line.append(&menu);
                    widget.set_child(Some(&line));
                } else {
                    widget.set_child(Some(&content));
                }
                self.list.append(&widget);
                if selected.as_ref() == Some(&row.action) {
                    self.list.select_row(Some(&widget));
                }
                self.widgets.borrow_mut().push((widget, title, detail));
            }
        }
        for ((widget, title, detail), row) in self.widgets.borrow().iter().zip(&visible) {
            if title.text().as_str() != row.title {
                title.set_text(&row.title);
            }
            let text = format!(
                "{}{}",
                if row.attention {
                    "● "
                } else if row.busy {
                    "◌ "
                } else {
                    ""
                },
                row.detail
            );
            if detail.text().as_str() != text {
                detail.set_text(&text);
            }
            widget.set_tooltip_text(Some(&format!("{}\n{}", row.title, row.searchable)));
            if row.active {
                widget.add_css_class("active-conversation");
            } else {
                widget.remove_css_class("active-conversation");
            }
        }
        *self.rows.borrow_mut() = visible;
        if !same_ids {
            self.list.invalidate_headers();
        }
    }
}

/// A task catalog record does not grant ownership of another window's runtime.
/// Show its real project/context in the center without inventing a resume action.
pub(crate) fn task_context(row: &jterm_core::agent_workspace::WorkspaceRow) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 18);
    root.set_margin_top(36);
    root.set_margin_bottom(36);
    root.set_margin_start(36);
    root.set_margin_end(36);
    root.set_hexpand(true);
    root.set_vexpand(true);
    for (text, heading) in [
        (row.title.clone(), true),
        (format!("{} · Task context", row.provider), false),
        ("This task belongs to another window. Its project and saved metadata are available here; continue its native agent in the window that owns it.".into(), false),
        (format!("Project\n{}\n\n{}", row.cwd, row.preview), false),
    ] {
        let label = gtk::Label::new(Some(&text)); label.set_xalign(0.0); label.set_wrap(true); label.set_selectable(true);
        if heading { label.add_css_class("title-1"); }
        root.append(&label);
    }
    root
}

/// Keep the existing task controls but give the selected agent the main canvas.
/// The full dashboard stays available in one bounded overview drawer.
pub(crate) fn compact_task_surface(root: &gtk::Box) {
    let mut children = Vec::new();
    let mut child = root.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        children.push(widget);
    }
    let Some((index, paned)) = children.iter().enumerate().find_map(|(index, widget)| {
        widget
            .clone()
            .downcast::<gtk::Paned>()
            .ok()
            .map(|paned| (index, paned))
    }) else {
        return;
    };
    let Some(detail) = paned.end_child() else {
        return;
    };
    let overview = gtk::Box::new(gtk::Orientation::Vertical, 4);
    for widget in children.iter().take(index).skip(2) {
        root.remove(widget);
        overview.append(widget);
    }
    if let Some(list) = paned.start_child() {
        paned.set_start_child(None::<&gtk::Widget>);
        list.set_vexpand(false);
        if let Some(scroll) = list.downcast_ref::<gtk::ScrolledWindow>() {
            scroll.set_min_content_height(110);
            scroll.set_max_content_height(160);
            scroll.set_propagate_natural_height(true);
        }
        overview.append(&list);
    }
    paned.set_end_child(None::<&gtk::Widget>);
    root.remove(&paned);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_max_content_height(260);
    scroll.set_propagate_natural_height(true);
    scroll.set_child(Some(&overview));
    let drawer = gtk::Expander::new(Some("Task overview and filters"));
    drawer.set_margin_start(8);
    drawer.set_margin_end(8);
    drawer.set_margin_top(6);
    drawer.set_margin_bottom(6);
    drawer.set_child(Some(&scroll));
    root.append(&drawer);
    detail.set_vexpand(true);
    root.append(&detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        Row {
            action: Action::Api(1),
            title: "Investigate renderer".into(),
            detail: "API conversation".into(),
            searchable: "GPU redraw latency".into(),
            active: false,
            busy: true,
            attention: true,
            archived: false,
            pinned: true,
            project: "/work/project/src".into(),
            provider: "API".into(),
            updated: 1,
        }
    }

    #[test]
    fn project_attention_archive_and_provider_filters_apply_to_every_source() {
        let row = row();
        assert!(Filter {
            query: "renderer is:pinned -provider:claude".into(),
            project: Some("/work/project".into()),
            needs_attention: true,
            running: true,
            ..Default::default()
        }
        .matches(&row));
        assert!(!Filter {
            project: Some("/work/project-other".into()),
            ..Default::default()
        }
        .matches(&row));
        assert!(!Filter {
            provider: Some("Codex".into()),
            ..Default::default()
        }
        .matches(&row));
        assert!(!Filter {
            archive: 1,
            ..Default::default()
        }
        .matches(&row));
        let mut archived = row;
        archived.archived = true;
        assert!(!Filter::default().matches(&archived));
        assert!(Filter {
            archive: 1,
            ..Default::default()
        }
        .matches(&archived));
    }
}
