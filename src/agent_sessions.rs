//! Native session switcher for provider-owned CLI conversations.
use gtk::prelude::*;
use jterm_core::agent_sessions::{
    AgentSession, SessionCatalog, SessionLaunch, SessionProvider, SessionQuery, SessionScanner,
    SessionSearchIndex, SessionSort, SessionStatus,
};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

thread_local! {
    #[cfg(test)]
    static TEST_MANAGER: RefCell<std::rc::Weak<Manager>> = RefCell::new(std::rc::Weak::new());
    static MANAGER_WINDOW: RefCell<gtk::glib::WeakRef<gtk::Window>> = RefCell::new(gtk::glib::WeakRef::new());
}

struct Manager {
    window: gtk::glib::WeakRef<gtk::Window>,
    scanner: RefCell<SessionScanner>,
    catalog: RefCell<SessionCatalog>,
    visible: RefCell<Vec<AgentSession>>,
    search_index: RefCell<SessionSearchIndex>,
    rows: RefCell<Vec<SessionRow>>,
    matched: RefCell<Vec<usize>>,
    page: Cell<usize>,
    updating: Cell<bool>,
    current_directory: Option<PathBuf>,
    list_scroll: gtk::ScrolledWindow,
    project_only: gtk::CheckButton,
    sort: gtk::DropDown,
    clear: gtk::Button,
    previous_page: gtk::Button,
    next_page: gtk::Button,
    page_label: gtk::Label,
    refresh_notice: RefCell<Option<String>>,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    provider: gtk::DropDown,
    status: gtk::DropDown,
    live_only: gtk::CheckButton,
    overview: gtk::Label,
    notice: gtk::Label,
    detail: gtk::Label,
    resume: gtk::Button,
    refresh: gtk::Button,
    selected: RefCell<Option<String>>,
    last_refresh: RefCell<Instant>,
    launch: Box<dyn Fn(SessionLaunch, String)>,
}

fn identity(session: &AgentSession) -> String {
    format!("{}:{}", session.provider.label(), session.id)
}

fn age(updated: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(updated) / 1000;
    if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

const PAGE_SIZE: usize = 80;

struct SessionRow {
    key: String,
    row: gtk::ListBoxRow,
    title: gtk::Label,
    meta: gtk::Label,
    cwd: gtk::Label,
}

fn set_label(label: &gtk::Label, value: &str) {
    if label.text().as_str() != value {
        label.set_text(value);
    }
}

impl SessionRow {
    fn new(session: &AgentSession) -> Self {
        let row = gtk::ListBoxRow::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(12);
        content.set_margin_end(12);
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.add_css_class("heading");
        let meta = gtk::Label::new(None);
        meta.set_xalign(0.0);
        meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
        meta.add_css_class("dim-label");
        let cwd = gtk::Label::new(None);
        cwd.set_xalign(0.0);
        cwd.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        cwd.add_css_class("dim-label");
        content.append(&title);
        content.append(&meta);
        content.append(&cwd);
        row.set_child(Some(&content));
        Self {
            key: identity(session),
            row,
            title,
            meta,
            cwd,
        }
    }

    fn update(&self, session: &AgentSession, catalog: &SessionCatalog) {
        set_label(&self.title, &session.title);
        let pids = catalog.live_pids(session);
        let live = if pids.is_empty() {
            String::new()
        } else {
            format!(
                "  ·  CLI process observed (PID {})",
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let meta = format!(
            "{}  ·  {}  ·  {}{}",
            session.provider.label(),
            session.status.label(),
            age(session.updated_at_ms),
            live
        );
        if self.meta.text().as_str() != meta {
            self.meta.set_text(&meta);
            self.meta.set_tooltip_text(Some(&meta));
        }
        set_label(&self.cwd, &session.cwd.to_string_lossy());
        self.cwd
            .set_tooltip_text(Some(&session.cwd.to_string_lossy()));
    }
}

impl Manager {
    fn refresh(&self) {
        if self.scanner.borrow_mut().refresh() {
            self.refresh.set_sensitive(false);
            *self.refresh_notice.borrow_mut() = Some(self.notice.text().to_string());
            self.notice
                .set_text("Reading local Codex and Claude sessions…");
            *self.last_refresh.borrow_mut() = Instant::now();
        }
    }

    fn accept_catalog(&self, catalog: SessionCatalog) {
        self.refresh.set_sensitive(true);
        let changed = *self.catalog.borrow() != catalog;
        if changed {
            *self.search_index.borrow_mut() = SessionSearchIndex::new(&catalog);
            *self.catalog.borrow_mut() = catalog;
            self.render_view(true);
        } else {
            // Neither the model nor GTK row identities are replaced on an
            // unchanged scan. Only relative ages may need a label update.
            if let Some(notice) = self.refresh_notice.borrow_mut().take() {
                self.notice.set_text(&notice);
            }
            self.update_rows();
            self.show_selection(self.list.selected_row().map(|row| row.index() as usize));
        }
        self.refresh_notice.borrow_mut().take();
    }

    fn filters_changed(&self) {
        if self.updating.get() {
            return;
        }
        self.page.set(0);
        self.selected.borrow_mut().take();
        self.render();
    }

    fn clear_filters(&self) {
        self.updating.set(true);
        self.search.set_text("");
        self.provider.set_selected(0);
        self.status.set_selected(0);
        self.live_only.set_active(false);
        self.project_only.set_active(false);
        self.sort.set_selected(0);
        self.updating.set(false);
        self.page.set(0);
        self.selected.borrow_mut().take();
        self.render();
        self.search.grab_focus();
    }

    fn render(&self) {
        self.render_view(false);
    }

    fn render_view(&self, preserve_view: bool) {
        if self.updating.replace(true) {
            return;
        }
        let selected = self.selected.borrow().clone();
        let offset = self.list_scroll.vadjustment().value();
        let query_text = self.search.text();
        let query = SessionQuery {
            text: query_text.as_str(),
            provider: match self.provider.selected() {
                1 => Some(SessionProvider::Codex),
                2 => Some(SessionProvider::Claude),
                _ => None,
            },
            status: match self.status.selected() {
                1 => Some(SessionStatus::Working),
                2 => Some(SessionStatus::Waiting),
                3 => Some(SessionStatus::Idle),
                4 => Some(SessionStatus::Unknown),
                5 => Some(SessionStatus::Archived),
                _ => None,
            },
            live_only: self.live_only.is_active(),
            project: if self.project_only.is_active() {
                self.current_directory.as_deref()
            } else {
                None
            },
            sort: match self.sort.selected() {
                1 => SessionSort::Recent,
                2 => SessionSort::LiveFirst,
                _ => SessionSort::Relevance,
            },
        };
        let matches = self.search_index.borrow().query(&query);
        let catalog = self.catalog.borrow();
        if preserve_view {
            if let Some(position) = matches.iter().position(|index| {
                selected.as_deref() == Some(identity(&catalog.sessions[*index]).as_str())
            }) {
                self.page.set(position / PAGE_SIZE);
            }
        }
        let pages = matches.len().div_ceil(PAGE_SIZE).max(1);
        self.page.set(self.page.get().min(pages - 1));
        let start = (self.page.get() * PAGE_SIZE).min(matches.len());
        let end = (start + PAGE_SIZE).min(matches.len());
        let sessions: Vec<_> = matches[start..end]
            .iter()
            .map(|index| catalog.sessions[*index].clone())
            .collect();
        let attention = catalog
            .sessions
            .iter()
            .filter(|session| session.status == SessionStatus::Waiting)
            .count();
        set_label(
            &self.overview,
            &format!(
                "{} sessions  ·  {} match  ·  {} last needed input  ·  {} observed CLI processes",
                catalog.sessions.len(),
                matches.len(),
                attention,
                catalog.live_processes.len()
            ),
        );
        set_label(
            &self.page_label,
            &format!(
                "{}–{} of {}  ·  Page {} / {}",
                if matches.is_empty() { 0 } else { start + 1 },
                end,
                matches.len(),
                self.page.get() + 1,
                pages
            ),
        );
        self.previous_page.set_sensitive(self.page.get() > 0);
        self.next_page.set_sensitive(self.page.get() + 1 < pages);
        self.clear.set_sensitive(
            !query_text.is_empty()
                || self.provider.selected() != 0
                || self.status.selected() != 0
                || self.live_only.is_active()
                || self.project_only.is_active()
                || self.sort.selected() != 0,
        );
        self.notice.set_text(if catalog.sessions.is_empty() {
            "No saved sessions found. Start Codex or Claude, then refresh. Custom CODEX_HOME and CLAUDE_CONFIG_DIR are supported."
        } else if matches.is_empty() {
            "No matching sessions. Clear filters or try a project, title, session ID, branch or conversation text."
        } else if catalog.warnings.is_empty() {
            "↑ / ↓ navigate · PgUp / PgDn change page · Ctrl+Home / End jump · Ctrl+L search · Enter resume · Esc close. States describe the latest saved event."
        } else { &catalog.warnings[0] });
        drop(catalog);
        *self.matched.borrow_mut() = matches;
        let same_rows = {
            let rows = self.rows.borrow();
            rows.len() == sessions.len()
                && rows
                    .iter()
                    .zip(&sessions)
                    .all(|(row, session)| row.key == identity(session))
        };
        *self.visible.borrow_mut() = sessions;
        if !same_rows {
            while let Some(child) = self.list.first_child() {
                self.list.remove(&child);
            }
            let rows: Vec<_> = self.visible.borrow().iter().map(SessionRow::new).collect();
            for row in &rows {
                self.list.append(&row.row);
            }
            *self.rows.borrow_mut() = rows;
        }
        self.update_rows();
        let selected_index = self
            .visible
            .borrow()
            .iter()
            .position(|session| selected.as_deref() == Some(identity(session).as_str()))
            .unwrap_or(0);
        if let Some(row) = self.list.row_at_index(selected_index as i32) {
            self.list.select_row(Some(&row));
        }
        self.updating.set(false);
        self.show_selection(self.list.selected_row().map(|row| row.index() as usize));
        if !same_rows {
            let adjustment = self.list_scroll.vadjustment();
            let value = if preserve_view { offset } else { 0.0 };
            gtk::glib::idle_add_local_once(move || {
                adjustment
                    .set_value(value.min(
                        (adjustment.upper() - adjustment.page_size()).max(adjustment.lower()),
                    ));
            });
        }
    }

    fn update_rows(&self) {
        let catalog = self.catalog.borrow();
        for (row, session) in self.rows.borrow().iter().zip(self.visible.borrow().iter()) {
            row.update(session, &catalog);
        }
    }

    fn change_page(&self, delta: isize) {
        let pages = self.matched.borrow().len().div_ceil(PAGE_SIZE).max(1);
        self.page
            .set(self.page.get().saturating_add_signed(delta).min(pages - 1));
        self.render();
    }

    fn navigate(&self, key: gtk::gdk::Key) -> bool {
        let count = self.matched.borrow().len();
        let current = self.page.get() * PAGE_SIZE
            + self
                .list
                .selected_row()
                .map(|row| row.index() as usize)
                .unwrap_or(0);
        let next = match key {
            gtk::gdk::Key::Up => current.saturating_sub(1),
            gtk::gdk::Key::Down => current.saturating_add(1),
            gtk::gdk::Key::Page_Up => current.saturating_sub(PAGE_SIZE),
            gtk::gdk::Key::Page_Down => current.saturating_add(PAGE_SIZE),
            gtk::gdk::Key::Home => 0,
            gtk::gdk::Key::End => count.saturating_sub(1),
            _ => return false,
        }
        .min(count.saturating_sub(1));
        if count == 0 {
            return true;
        }
        if next / PAGE_SIZE != self.page.get() {
            self.page.set(next / PAGE_SIZE);
            self.render();
        }
        if let Some(row) = self.list.row_at_index((next % PAGE_SIZE) as i32) {
            self.list.select_row(Some(&row));
            // Scroll the selected row into view while returning the focus to
            // the query, so keyboard browsing never discards the typed search.
            let search = self.search.clone();
            gtk::glib::idle_add_local_once(move || {
                row.grab_focus();
                search.grab_focus();
            });
        }
        true
    }

    fn show_selection(&self, index: Option<usize>) {
        if self.updating.get() {
            return;
        }
        let visible = self.visible.borrow();
        let Some(session) = index.and_then(|index| visible.get(index)) else {
            self.selected.borrow_mut().take();
            set_label(&self.detail, "Select a session to inspect its project and recent conversation.\n\nSearch by title, project, branch, session ID or conversation text.");
            self.resume.set_sensitive(false);
            return;
        };
        *self.selected.borrow_mut() = Some(identity(session));
        let launch = session.resume_launch();
        let command = launch
            .as_ref()
            .map(|launch| launch.display_command())
            .unwrap_or_else(|error| error.clone());
        let pids = self.catalog.borrow().live_pids(session);
        let process = if pids.is_empty() {
            "No matching resumed CLI process observed".into()
        } else {
            format!(
                "CLI process observed: PID {} (activity is shown separately)",
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        set_label(
            &self.detail,
            &format!(
                "{}\n\n{} · {} · {}\nSession {}\nProject {}\nBranch {}\n{}\n\n{}\n\n{}",
                session.title,
                session.provider.label(),
                session.status.label(),
                age(session.updated_at_ms),
                session.id,
                session.cwd.display(),
                session.branch.as_deref().unwrap_or("—"),
                process,
                session.preview,
                command
            ),
        );
        self.resume.set_sensitive(launch.is_ok());
    }
    fn resume_selected(&self) {
        let selected = self.selected.borrow().clone();
        let session = self
            .visible
            .borrow()
            .iter()
            .find(|session| selected.as_deref() == Some(identity(session).as_str()))
            .cloned();
        if let Some(session) = session {
            match session.resume_launch() {
                Ok(launch) => {
                    crate::enter_ownership::claim_held();
                    (self.launch)(
                        launch,
                        format!("{} · {}", session.provider.label(), session.title),
                    );
                    if let Some(window) = self.window.upgrade() {
                        window.close();
                    }
                }
                Err(error) => self.notice.set_text(&error),
            }
        }
    }
}

pub(crate) fn show(
    parent: &impl IsA<gtk::Window>,
    current_directory: Option<PathBuf>,
    launch: impl Fn(SessionLaunch, String) + 'static,
) {
    show_inner(parent, current_directory, launch, true);
}

fn show_inner(
    parent: &impl IsA<gtk::Window>,
    current_directory: Option<PathBuf>,
    launch: impl Fn(SessionLaunch, String) + 'static,
    auto_scan: bool,
) {
    if let Some(window) = MANAGER_WINDOW.with(|slot| slot.borrow().upgrade()) {
        if window.transient_for().as_ref() == Some(parent.as_ref()) {
            window.present();
            return;
        }
    }
    let window = gtk::Window::builder()
        .title("Agent Sessions")
        .default_width(1080)
        .default_height(700)
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .build();
    MANAGER_WINDOW.with(|slot| slot.borrow_mut().set(Some(&window)));
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(18);
    root.set_margin_end(18);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let heading = gtk::Label::new(Some("Agent Sessions"));
    heading.add_css_class("title-1");
    heading.set_hexpand(true);
    heading.set_xalign(0.0);
    let refresh = gtk::Button::with_label("Refresh");
    refresh.set_tooltip_text(Some("Rescan local session history"));
    let close = gtk::Button::with_label("Close");
    header.append(&heading);
    header.append(&refresh);
    header.append(&close);
    root.append(&header);
    let overview = gtk::Label::new(Some("Discovering conversations…"));
    overview.set_xalign(0.0);
    root.append(&overview);
    let filters = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search sessions, projects, IDs and conversation…"));
    search.set_hexpand(true);
    let provider = gtk::DropDown::from_strings(&["All providers", "Codex", "Claude"]);
    provider.set_tooltip_text(Some("Filter by agent provider"));
    let status = gtk::DropDown::from_strings(&[
        "All states",
        "Last recorded: working",
        "Last recorded: needs input",
        "Ready to resume",
        "State unknown",
        "Archived",
    ]);
    status.set_tooltip_text(Some("Filter by the latest recorded session state"));
    let live_only = gtk::CheckButton::with_label("Live CLI");
    live_only.set_tooltip_text(Some("Only sessions with a matching currently observed CLI process; recorded activity remains separate"));
    root.append(&search);
    let project_only = gtk::CheckButton::with_label("Current directory + subfolders");
    project_only.set_sensitive(current_directory.is_some());
    project_only.set_tooltip_text(Some(
        &current_directory
            .as_ref()
            .map(|path| format!("Only sessions in {} and its subdirectories", path.display()))
            .unwrap_or_else(|| {
                "No local working directory is available for the active pane".into()
            }),
    ));
    let sort = gtk::DropDown::from_strings(&["Best match", "Most recent", "Live first"]);
    sort.set_tooltip_text(Some("Order matching sessions"));
    let clear = gtk::Button::with_label("Clear filters");
    clear.add_css_class("flat");
    filters.append(&provider);
    filters.append(&status);
    filters.append(&live_only);
    filters.append(&project_only);
    filters.append(&sort);
    filters.append(&clear);
    root.append(&filters);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_width(330)
        .vexpand(true)
        .child(&list)
        .build();
    let detail = gtk::Label::new(None);
    detail.set_xalign(0.0);
    detail.set_yalign(0.0);
    detail.set_wrap(true);
    detail.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    detail.set_selectable(true);
    detail.set_margin_start(16);
    detail.set_margin_end(12);
    detail.set_margin_top(12);
    detail.set_margin_bottom(12);
    let details_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&detail)
        .build();
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    let list_column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    list_column.append(&list_scroll);
    let pager = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let previous_page = gtk::Button::with_label("Previous");
    let next_page = gtk::Button::with_label("Next");
    previous_page.set_tooltip_text(Some("Previous 80 matching sessions (Page Up)"));
    next_page.set_tooltip_text(Some("Next 80 matching sessions (Page Down)"));
    let page_label = gtk::Label::new(None);
    page_label.set_hexpand(true);
    page_label.add_css_class("dim-label");
    pager.append(&previous_page);
    pager.append(&page_label);
    pager.append(&next_page);
    list_column.append(&pager);
    paned.set_start_child(Some(&list_column));
    paned.set_end_child(Some(&details_scroll));
    paned.set_position(420);
    paned.set_vexpand(true);
    root.append(&paned);
    let notice = gtk::Label::new(None);
    notice.set_xalign(0.0);
    notice.set_wrap(true);
    notice.add_css_class("dim-label");
    root.append(&notice);
    let resume = gtk::Button::with_label("Resume in New Tab");
    resume.add_css_class("suggested-action");
    resume.set_halign(gtk::Align::End);
    root.append(&resume);
    window.set_child(Some(&root));
    let manager = Rc::new(Manager {
        window: window.downgrade(),
        scanner: RefCell::new(SessionScanner::new()),
        catalog: RefCell::new(SessionCatalog {
            sessions: Vec::new(),
            warnings: Vec::new(),
            ..Default::default()
        }),
        visible: RefCell::new(Vec::new()),
        search_index: RefCell::new(SessionSearchIndex::default()),
        rows: RefCell::new(Vec::new()),
        matched: RefCell::new(Vec::new()),
        page: Cell::new(0),
        updating: Cell::new(false),
        current_directory,
        list_scroll,
        project_only,
        sort,
        clear,
        previous_page,
        next_page,
        page_label,
        refresh_notice: RefCell::new(None),
        list,
        search,
        provider,
        status,
        live_only,
        overview,
        notice,
        detail,
        resume,
        refresh,
        selected: RefCell::new(None),
        last_refresh: RefCell::new(Instant::now()),
        launch: Box::new(launch),
    });
    let weak = Rc::downgrade(&manager);
    manager.search.connect_changed(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.provider.connect_selected_notify(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.status.connect_selected_notify(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.live_only.connect_toggled(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.project_only.connect_toggled(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.sort.connect_selected_notify(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.clear.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.clear_filters();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.previous_page.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.change_page(-1);
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.next_page.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.change_page(1);
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.list.connect_row_selected(move |_, row| {
        if let Some(manager) = weak.upgrade() {
            manager.show_selection(row.map(|row| row.index() as usize));
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.list.connect_row_activated(move |_, _| {
        if let Some(manager) = weak.upgrade() {
            manager.resume_selected();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.search.connect_activate(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.resume_selected();
        }
    });
    let search_keys = gtk::EventControllerKey::new();
    search_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&manager);
    search_keys.connect_key_pressed(move |_, key, _, modifiers| {
        let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        if modifiers
            .intersects(gtk::gdk::ModifierType::ALT_MASK | gtk::gdk::ModifierType::SUPER_MASK)
            || (control && !matches!(key, gtk::gdk::Key::Home | gtk::gdk::Key::End))
            || (!control && matches!(key, gtk::gdk::Key::Home | gtk::gdk::Key::End))
        {
            return gtk::glib::Propagation::Proceed;
        }
        if weak.upgrade().is_some_and(|manager| manager.navigate(key)) {
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    manager.search.add_controller(search_keys);
    let list_keys = gtk::EventControllerKey::new();
    list_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&manager);
    list_keys.connect_key_pressed(move |_, key, _, modifiers| {
        if !modifiers.is_empty() {
            return gtk::glib::Propagation::Proceed;
        }
        if weak.upgrade().is_some_and(|manager| manager.navigate(key)) {
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    manager.list.add_controller(list_keys);

    let weak = Rc::downgrade(&manager);
    manager.resume.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.resume_selected();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.refresh.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.refresh();
        }
    });
    let window_weak = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = window_weak.upgrade() {
            window.close();
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    crate::enter_ownership::install_controller(&keys);
    crate::enter_ownership::watch_window(&window);
    let window_weak = window.downgrade();
    let weak = Rc::downgrade(&manager);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
            && matches!(key, gtk::gdk::Key::l | gtk::gdk::Key::L)
        {
            if let Some(manager) = weak.upgrade() {
                manager.search.grab_focus();
                manager.search.select_region(0, -1);
            }
            return gtk::glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::F5 {
            if let Some(manager) = weak.upgrade() {
                manager.refresh();
            }
            return gtk::glib::Propagation::Stop;
        }
        if key == gtk::gdk::Key::Escape {
            if let Some(window) = window_weak.upgrade() {
                window.close();
            }
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
    #[cfg(test)]
    TEST_MANAGER.with(|slot| *slot.borrow_mut() = Rc::downgrade(&manager));
    manager.render();
    if auto_scan {
        manager.refresh();
    }
    window.present();
    manager.search.grab_focus();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(window) = manager.window.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        if !window.is_visible() {
            return gtk::glib::ControlFlow::Break;
        }
        let catalog = manager.scanner.borrow_mut().poll();
        if let Some(catalog) = catalog {
            manager.accept_catalog(catalog);
        }
        if auto_scan && manager.last_refresh.borrow().elapsed() >= Duration::from_secs(5) {
            manager.refresh();
        }
        gtk::glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use jterm_core::agent_sessions::{LiveSessionProcess, SessionProvider};

    /// Run in an isolated Xvfb session; verifies real GTK signals and exact
    /// launch argv without starting a provider or reading personal history.
    #[test]
    #[ignore = "requires a native GTK display; run under Xvfb"]
    fn session_manager_filters_preserves_selection_and_resumes_exact_argv() {
        gtk::init().expect("GTK display");
        let directory = std::env::temp_dir().join(format!(
            "agent-manager-gtk-{}",
            gtk::glib::uuid_string_random()
        ));
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join("session.jsonl");
        std::fs::write(&source, "{}\n").unwrap();
        let received = Rc::new(RefCell::new(None));
        let captured = received.clone();
        let parent = gtk::Window::new();
        show_inner(
            &parent,
            Some(directory.clone()),
            move |launch, _| *captured.borrow_mut() = Some(launch),
            false,
        );
        let manager = TEST_MANAGER.with(|slot| slot.borrow().upgrade()).unwrap();
        let session = |provider, id: &str, title: &str, status| AgentSession {
            provider,
            id: id.into(),
            title: title.into(),
            cwd: directory.clone(),
            updated_at_ms: 0,
            status,
            preview: "Repair renderer and preserve input".into(),
            branch: Some("feature/renderer".into()),
            source: source.clone(),
            archived: false,
        };
        manager.catalog.borrow_mut().sessions = vec![
            session(
                SessionProvider::Codex,
                "01912345-1234-7123-8123-123456789abc",
                "Renderer regression",
                SessionStatus::Waiting,
            ),
            session(
                SessionProvider::Claude,
                "01912345-1234-7123-8123-123456789abd",
                "Editor diagnostics",
                SessionStatus::Idle,
            ),
        ];
        manager.catalog.borrow_mut().live_processes = vec![LiveSessionProcess {
            provider: SessionProvider::Codex,
            id: "01912345-1234-7123-8123-123456789abc".into(),
            pid: 42,
        }];
        *manager.search_index.borrow_mut() = SessionSearchIndex::new(&manager.catalog.borrow());
        manager.render();
        assert_eq!(manager.visible.borrow().len(), 2);
        if let Some(path) = std::env::var_os("JTERM_AGENT_SESSIONS_SCREENSHOT") {
            let context = gtk::glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(std::process::Command::new("import")
                .args(["-window", "root"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }

        assert!(manager.overview.text().contains("1 last needed input"));
        manager
            .list
            .select_row(manager.list.row_at_index(1).as_ref());
        let selected = manager.selected.borrow().clone();
        manager.render();
        assert_eq!(
            *manager.selected.borrow(),
            selected,
            "refresh retains stable provider/session identity"
        );
        // An unchanged background scan must preserve actual GTK objects,
        // selection and the scroller, rather than only reselecting by index.
        let original = manager.catalog.borrow().clone();
        let row_before = manager.list.row_at_index(1).unwrap();
        let old_scroll = manager.list_scroll.vadjustment().value();
        manager.accept_catalog(original.clone());
        assert_eq!(manager.list.row_at_index(1).unwrap(), row_before);
        assert_eq!(manager.list_scroll.vadjustment().value(), old_scroll);
        assert_eq!(*manager.selected.borrow(), selected);

        let sibling = directory.with_extension("outside");
        let nested = directory.join("nested-project");
        std::fs::create_dir(&sibling).unwrap();
        std::fs::create_dir(&nested).unwrap();
        let mut large = original.clone();
        large.sessions = (0..2048)
            .map(|i| {
                let mut item = session(
                    SessionProvider::Codex,
                    &format!("01912345-1234-7123-8123-{i:012x}"),
                    &format!("Renderer task {i:04}"),
                    SessionStatus::Idle,
                );
                item.cwd = if i % 2 == 0 {
                    nested.clone()
                } else {
                    sibling.clone()
                };
                item.updated_at_ms = i;
                item
            })
            .collect();
        large.live_processes.clear();
        manager.accept_catalog(large.clone());
        assert_eq!(manager.matched.borrow().len(), 2048);
        assert_eq!(manager.visible.borrow().len(), PAGE_SIZE);
        assert_eq!(manager.rows.borrow().len(), PAGE_SIZE);
        assert!(manager.page_label.text().contains("of 2048"));
        assert!(manager.next_page.is_sensitive());
        assert!(manager.navigate(gtk::gdk::Key::End));
        assert_eq!(manager.page.get(), 25);
        assert_eq!(manager.visible.borrow().len(), 48);
        let last = manager.selected.borrow().clone();
        let page_last_row = manager.list.row_at_index(0).unwrap();
        manager.accept_catalog(large.clone());
        assert_eq!(manager.page.get(), 25);
        assert_eq!(*manager.selected.borrow(), last);
        assert_eq!(manager.list.row_at_index(0).unwrap(), page_last_row);
        manager.navigate(gtk::gdk::Key::Home);
        assert_eq!(manager.page.get(), 0);
        manager.project_only.set_active(true);
        assert_eq!(manager.matched.borrow().len(), 1024);
        assert!(manager
            .visible
            .borrow()
            .iter()
            .all(|item| item.cwd.starts_with(&directory)));
        manager.clear_filters();
        assert_eq!(manager.matched.borrow().len(), 2048);
        assert!(!manager.project_only.is_active());
        // A fresh ranking must choose the first result, even when the old
        // selection also matches the new query through an incidental preview.
        large.sessions[0].preview = "critical cache repair".into();
        large.sessions[1].title = "critical cache repair".into();
        large.live_processes = vec![LiveSessionProcess {
            provider: SessionProvider::Codex,
            id: large.sessions[0].id.clone(),
            pid: 77,
        }];
        manager.accept_catalog(large.clone());
        manager.search.set_text(&large.sessions[0].id);
        assert_eq!(
            *manager.selected.borrow(),
            Some(identity(&large.sessions[0]))
        );
        manager.search.set_text("critical cache repair");
        assert_eq!(
            *manager.selected.borrow(),
            Some(identity(&large.sessions[1]))
        );
        manager.clear_filters();
        manager.sort.set_selected(2);
        assert_eq!(
            *manager.selected.borrow(),
            Some(identity(&large.sessions[0]))
        );
        manager.clear_filters();
        manager.accept_catalog(original);
        std::fs::remove_dir(&nested).unwrap();
        std::fs::remove_dir(&sibling).unwrap();

        manager.provider.set_selected(1);
        assert_eq!(manager.visible.borrow().len(), 1);
        manager.status.set_selected(2);
        assert_eq!(manager.visible.borrow().len(), 1);
        manager.search.set_text("not-present");
        assert!(manager.visible.borrow().is_empty());
        assert!(!manager.resume.is_sensitive());
        manager.search.set_text("renderer");
        assert!(manager.resume.is_sensitive());
        std::fs::remove_file(&source).unwrap();
        manager.resume.emit_clicked();
        assert!(
            received.borrow().is_none(),
            "stale history cannot launch from a displayed snapshot"
        );
        assert!(manager.notice.text().contains("no longer exists"));
        std::fs::write(&source, "{}\n").unwrap();
        manager.search.emit_activate();
        let launch = received
            .borrow_mut()
            .take()
            .expect("explicit confirmation emits launch");
        assert_eq!(launch.program, "codex");
        assert_eq!(
            launch.args,
            ["resume", "01912345-1234-7123-8123-123456789abc"]
        );
        assert_eq!(launch.cwd, directory);
        parent.close();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
