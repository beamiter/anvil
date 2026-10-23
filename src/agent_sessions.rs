//! Native session switcher for provider-owned CLI conversations.
use gtk::prelude::*;
use jterm_core::agent_sessions::{
    AgentSession, SessionArchiveFilter, SessionCatalog, SessionLaunch, SessionLibraryFilter,
    SessionLibrarySnapshot, SessionLibraryStore, SessionPreflight, SessionPreflightState,
    SessionProvider, SessionQuery, SessionScanner, SessionSearchIndex, SessionSort, SessionStatus,
};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

thread_local! {
    #[cfg(test)]
    static TEST_MANAGER: RefCell<std::rc::Weak<Manager>> = const { RefCell::new(std::rc::Weak::new()) };
    static MANAGER_WINDOW: RefCell<gtk::glib::WeakRef<gtk::Window>> = RefCell::new(gtk::glib::WeakRef::new());
}

pub(crate) type SessionIdentity = (SessionProvider, String);

#[derive(Clone, Debug)]
pub(crate) struct ResumeRequest {
    pub provider: SessionProvider,
    pub id: String,
    pub title: String,
    /// None requests a verified same-window tab switch.
    pub launch: Option<SessionLaunch>,
    pub completion: Sender<Result<(), String>>,
}

struct Activation {
    key: SessionIdentity,
    existing: bool,
    result: Receiver<Result<(), String>>,
}

#[derive(Clone)]
struct OrganizationDraft {
    alias: String,
    tags: String,
    error: Option<String>,
}

#[derive(PartialEq, Eq)]
struct SessionActionIdentity {
    key: String,
    pinned: bool,
    archived: bool,
    open_here: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionQuickView {
    All,
    NeedsInput,
    Live,
    Pinned,
    Archived,
}

impl SessionQuickView {
    const ALL: [Self; 5] = [
        Self::All,
        Self::NeedsInput,
        Self::Live,
        Self::Pinned,
        Self::Archived,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::All => "Active",
            Self::NeedsInput => "Needs input",
            Self::Live => "Live CLI",
            Self::Pinned => "Pinned",
            Self::Archived => "Archived",
        }
    }
}

struct Manager {
    window: gtk::glib::WeakRef<gtk::Window>,
    library: Rc<RefCell<SessionLibraryStore>>,
    existing: RefCell<HashSet<SessionIdentity>>,
    preflight: RefCell<SessionPreflight>,
    confirmation: RefCell<Option<SessionIdentity>>,
    activation: RefCell<Option<Activation>>,
    enter_down: Rc<Cell<bool>>,
    completed_activation: Rc<Cell<bool>>,
    saved_view: gtk::DropDown,
    archive_view: gtk::DropDown,
    quick_views: Vec<(SessionQuickView, gtk::Button)>,
    project: gtk::DropDown,
    project_paths: RefCell<Vec<PathBuf>>,
    project_labels: RefCell<Vec<String>>,
    selected_project: RefCell<Option<PathBuf>>,
    alias: gtk::Entry,
    tags: gtk::Entry,
    save_details: gtk::Button,
    revert_details: gtk::Button,
    organization_notice: gtk::Label,
    organization_key: RefCell<Option<String>>,
    organization_dirty: Cell<bool>,
    organization_drafts: RefCell<HashMap<String, OrganizationDraft>>,
    organization_saving: RefCell<Option<String>>,
    organization_error: RefCell<Option<String>>,
    action_identity: RefCell<Option<SessionActionIdentity>>,
    archive: gtk::Button,
    pinned_first: gtk::CheckButton,
    pin: gtk::ToggleButton,
    library_status: gtk::Label,
    readiness: gtk::Label,
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
    launch: Box<dyn Fn(ResumeRequest)>,
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

    fn update(
        &self,
        session: &AgentSession,
        catalog: &SessionCatalog,
        library: &SessionLibrarySnapshot,
    ) {
        set_label(
            &self.title,
            &format!(
                "{}{}",
                if library.is_pinned(session.provider, &session.id) {
                    "★ "
                } else {
                    ""
                },
                library.display_title(session.provider, &session.id, &session.title)
            ),
        );
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
        let resumed = library
            .last_resumed(session.provider, &session.id)
            .map(|time| format!(" · Resumed {}", age(time)))
            .unwrap_or_default();
        let tags = library.tags(session.provider, &session.id);
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("  ·  #{}", tags.join("  #"))
        };
        let archive = if library.is_archived(session.provider, &session.id) {
            "  ·  Locally archived"
        } else {
            ""
        };
        let meta = format!(
            "{}  ·  {}  ·  {}{live}{resumed}{tags}{archive}",
            session.provider.label(),
            session.status.label(),
            age(session.updated_at_ms)
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
                if self.notice.text().as_str() == "Reading local Codex and Claude sessions…" {
                    self.notice.set_text(&notice);
                }
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
        self.saved_view.set_selected(0);
        self.archive_view.set_selected(0);
        self.project.set_selected(0);
        self.selected_project.borrow_mut().take();
        self.pinned_first.set_active(true);
        self.updating.set(false);
        self.page.set(0);
        self.selected.borrow_mut().take();
        self.render();
        self.search.grab_focus();
    }

    fn set_quick_view(&self, view: SessionQuickView) {
        self.updating.set(true);
        self.status
            .set_selected(if view == SessionQuickView::NeedsInput {
                2
            } else {
                0
            });
        self.live_only.set_active(view == SessionQuickView::Live);
        self.saved_view
            .set_selected(if view == SessionQuickView::Pinned {
                1
            } else {
                0
            });
        self.archive_view
            .set_selected(if view == SessionQuickView::Archived {
                1
            } else {
                0
            });
        self.updating.set(false);
        self.filters_changed();
    }

    fn update_projects(&self) {
        let library = self.library.borrow();
        let projects = self.search_index.borrow().projects(library.snapshot());
        let mut paths = projects
            .iter()
            .map(|project| project.root.clone())
            .collect::<Vec<_>>();
        if let Some(selected) = self.selected_project.borrow().as_ref() {
            if !paths.contains(selected) {
                paths.push(selected.clone());
            }
        }
        let mut labels = vec!["All projects".to_string()];
        let project_counts = projects
            .iter()
            .map(|project| (&project.root, (project.total, project.archived)))
            .collect::<HashMap<_, _>>();
        labels.extend(paths.iter().enumerate().map(|(index, path)| {
            let counts = project_counts.get(path).copied().unwrap_or_default();
            format!(
                "{} · {}  ·  {} active · {} archived",
                index + 1,
                jterm_core::review_input::safe_inline_display(&path.to_string_lossy(), 160),
                counts.0,
                counts.1,
            )
        }));
        let selected = self
            .selected_project
            .borrow()
            .as_ref()
            .and_then(|path| paths.iter().position(|candidate| candidate == path))
            .map_or(0, |index| index as u32 + 1);
        if *self.project_paths.borrow() != paths || *self.project_labels.borrow() != labels {
            let strings = labels.iter().map(String::as_str).collect::<Vec<_>>();
            self.project
                .set_model(Some(&gtk::StringList::new(&strings)));
            *self.project_paths.borrow_mut() = paths;
            *self.project_labels.borrow_mut() = labels;
        }
        self.project.set_selected(selected);
    }

    fn render(&self) {
        self.render_view(false);
    }

    fn render_view(&self, preserve_view: bool) {
        self.finish_organization_save();
        if self.updating.replace(true) {
            return;
        }
        let selected = self.selected.borrow().clone();
        let offset = self.list_scroll.vadjustment().value();
        self.update_projects();
        let query_text = self.search.text();
        let selected_project = self.selected_project.borrow();
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
                selected_project.as_deref()
            },
            sort: match self.sort.selected() {
                1 => SessionSort::Recent,
                2 => SessionSort::LiveFirst,
                3 => SessionSort::RecentlyResumed,
                _ => SessionSort::Relevance,
            },
        };
        let matches = self.search_index.borrow().query_with_library(
            &query,
            self.library.borrow().snapshot(),
            SessionLibraryFilter {
                pinned_only: self.saved_view.selected() == 1,
                recent_only: self.saved_view.selected() == 2,
                pinned_first: self.pinned_first.is_active(),
                archive: match self.archive_view.selected() {
                    1 => SessionArchiveFilter::Archived,
                    2 => SessionArchiveFilter::All,
                    _ => SessionArchiveFilter::Active,
                },
            },
        );
        let summary_matches = self.search_index.borrow().query_with_library(
            &SessionQuery {
                status: None,
                live_only: false,
                ..query
            },
            self.library.borrow().snapshot(),
            SessionLibraryFilter {
                archive: SessionArchiveFilter::All,
                ..Default::default()
            },
        );
        let catalog = self.catalog.borrow();
        {
            let library = self.library.borrow();
            let live = catalog
                .live_processes
                .iter()
                .map(|process| (process.provider, process.id.as_str()))
                .collect::<HashSet<_>>();
            for (view, button) in &self.quick_views {
                let count = summary_matches
                    .iter()
                    .filter(|index| {
                        let session = &catalog.sessions[**index];
                        let archived = library
                            .snapshot()
                            .is_archived(session.provider, &session.id);
                        match view {
                            SessionQuickView::All => !archived,
                            SessionQuickView::NeedsInput => {
                                !archived && session.status == SessionStatus::Waiting
                            }
                            SessionQuickView::Live => {
                                !archived && live.contains(&(session.provider, session.id.as_str()))
                            }
                            SessionQuickView::Pinned => {
                                !archived
                                    && library.snapshot().is_pinned(session.provider, &session.id)
                            }
                            SessionQuickView::Archived => archived,
                        }
                    })
                    .count();
                button.set_label(&format!("{}  {count}", view.label()));
                let active = match view {
                    SessionQuickView::All => {
                        self.status.selected() == 0
                            && !self.live_only.is_active()
                            && self.saved_view.selected() == 0
                            && self.archive_view.selected() == 0
                    }
                    SessionQuickView::NeedsInput => {
                        self.status.selected() == 2
                            && !self.live_only.is_active()
                            && self.saved_view.selected() == 0
                            && self.archive_view.selected() == 0
                    }
                    SessionQuickView::Live => {
                        self.status.selected() == 0
                            && self.live_only.is_active()
                            && self.saved_view.selected() == 0
                            && self.archive_view.selected() == 0
                    }
                    SessionQuickView::Pinned => {
                        self.status.selected() == 0
                            && !self.live_only.is_active()
                            && self.saved_view.selected() == 1
                            && self.archive_view.selected() == 0
                    }
                    SessionQuickView::Archived => {
                        self.status.selected() == 0
                            && !self.live_only.is_active()
                            && self.saved_view.selected() == 0
                            && self.archive_view.selected() == 1
                    }
                };
                if active {
                    button.add_css_class("suggested-action");
                } else {
                    button.remove_css_class("suggested-action");
                }
            }
        }
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
                || self.sort.selected() != 0
                || self.saved_view.selected() != 0
                || self.archive_view.selected() != 0
                || selected_project.is_some()
                || !self.pinned_first.is_active(),
        );
        self.notice.set_text(if catalog.sessions.is_empty() {
            "No saved sessions found. Start Codex or Claude, then refresh. Custom CODEX_HOME and CLAUDE_CONFIG_DIR are supported."
        } else if matches.is_empty() {
            "No matching sessions. Clear filters or try a project, title, session ID, branch or conversation text."
        } else if catalog.warnings.is_empty() {
            "↑ / ↓ navigate · PgUp / PgDn change page · Ctrl+Home / End jump · Ctrl+L search · Enter resume · Esc close. States describe the latest saved event."
        } else { &catalog.warnings[0] });
        if let Some(error) = SessionSearchIndex::query_error(query_text.as_str()) {
            self.notice.set_text(&error);
        }
        drop(catalog);
        drop(selected_project);
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
            row.update(session, &catalog, self.library.borrow().snapshot());
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

    fn sync_action_identity(&self, session: Option<&AgentSession>) {
        let next = session.map(|session| {
            let library = self.library.borrow();
            SessionActionIdentity {
                key: identity(session),
                pinned: library.snapshot().is_pinned(session.provider, &session.id),
                archived: library
                    .snapshot()
                    .is_archived(session.provider, &session.id),
                open_here: self
                    .existing
                    .borrow()
                    .contains(&(session.provider, session.id.clone())),
            }
        });
        if *self.action_identity.borrow() == next {
            return;
        }
        *self.action_identity.borrow_mut() = next;
        // GtkButton retains both a pointer press and a delayed keyboard
        // activation across label/sensitivity changes. Unparenting unrealizes
        // the button and cancels both, so a gesture begun for A cannot operate
        // B after a refresh. Keep order and signal connections when reattaching.
        for button in [
            self.save_details.upcast_ref::<gtk::Widget>(),
            self.revert_details.upcast_ref(),
            self.archive.upcast_ref(),
            self.pin.upcast_ref(),
            self.resume.upcast_ref(),
        ] {
            button.set_sensitive(false);
            if let Some(parent) = button.parent().and_downcast::<gtk::Box>() {
                let previous = button.prev_sibling();
                parent.remove(button);
                parent.insert_child_after(button, previous.as_ref());
            }
        }
    }

    fn show_selection(&self, index: Option<usize>) {
        if self.updating.get() {
            return;
        }
        let visible = self.visible.borrow();
        let session = index.and_then(|index| visible.get(index));
        self.sync_action_identity(session);
        let Some(session) = session else {
            self.selected.borrow_mut().take();
            self.preflight.borrow_mut().select(None);
            self.confirmation.borrow_mut().take();
            set_label(&self.readiness, "Select a session to check readiness");
            self.pin.set_sensitive(false);
            self.pin.set_label("☆ Pin session");
            set_label(&self.detail, "Select a session to inspect its project and recent conversation.\n\nSearch by title, project, branch, session ID or conversation text.");
            self.resume.set_sensitive(false);
            self.organization_key.borrow_mut().take();
            self.organization_dirty.set(false);
            self.organization_error.borrow_mut().take();
            self.alias.set_sensitive(false);
            self.tags.set_sensitive(false);
            self.save_details.set_sensitive(false);
            self.revert_details.set_sensitive(false);
            self.archive.set_sensitive(false);
            self.updating.set(true);
            self.alias.set_text("");
            self.tags.set_text("");
            self.updating.set(false);
            self.organization_notice
                .set_text("Select a session to organize it");
            return;
        };
        *self.selected.borrow_mut() = Some(identity(session));
        self.sync_organization(session);
        let existing = self
            .existing
            .borrow()
            .contains(&(session.provider, session.id.clone()));
        let changed =
            self.preflight
                .borrow_mut()
                .select(if existing { None } else { Some(session) });
        if changed {
            self.confirmation.borrow_mut().take();
        }
        let preflight = self.preflight.borrow();
        let ready = preflight.ready_launch(session).is_some();
        let readiness = if self.completed_activation.get() {
            "Session opened — release Enter to switch".into()
        } else if self.activation.borrow().is_some() {
            "Opening session…".into()
        } else if existing {
            "Already open in this window — switch to its terminal".into()
        } else {
            match preflight.state() {
                SessionPreflightState::Idle => "Select a session to check readiness".into(),
                SessionPreflightState::Checking => "Checking project and CLI readiness…".into(),
                SessionPreflightState::Ready(launch) => format!(
                    "Ready · {}",
                    launch
                        .args
                        .get(launch.args.len().saturating_sub(3))
                        .map(String::as_str)
                        .unwrap_or(&launch.program)
                ),
                SessionPreflightState::Unavailable(error) => error.clone(),
            }
        };
        set_label(&self.readiness, &readiness);
        self.resume.set_label(if existing {
            "Switch to Session"
        } else {
            "Resume in New Tab"
        });
        let command = format!(
            "{} {} {}",
            session.provider.label().to_lowercase(),
            if session.provider == SessionProvider::Codex {
                "resume"
            } else {
                "--resume"
            },
            session.id
        );
        let library = self.library.borrow();
        let pinned = library.snapshot().is_pinned(session.provider, &session.id);
        self.updating.set(true);
        self.pin.set_active(pinned);
        self.updating.set(false);
        self.pin.set_label(if pinned {
            "★ Pinned"
        } else {
            "☆ Pin session"
        });
        let saved = library
            .snapshot()
            .last_resumed(session.provider, &session.id)
            .map(|time| format!("Last resumed {}", age(time)))
            .unwrap_or_else(|| "Not resumed from a terminal manager yet".into());
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
                "{}\n\n{} · {} · {}\nSession {}\nProject {}\nBranch {}\n{}\n\n{}\n\n{saved}\n\n{command}",
                if let Some(alias) = library.snapshot().alias(session.provider, &session.id) {
                    format!("{alias}\nProvider title: {}", session.title)
                } else {
                    session.title.clone()
                },
                session.provider.label(),
                session.status.label(),
                age(session.updated_at_ms),
                session.id,
                session.cwd.display(),
                session.branch.as_deref().unwrap_or("—"),
                process,
                session.preview
            ),
        );
        self.resume.set_sensitive(
            (existing || ready)
                && self.activation.borrow().is_none()
                && self.confirmation.borrow().is_none()
                && !self.completed_activation.get(),
        );
        drop(library);
        drop(preflight);
        self.update_library_status();
    }

    fn sync_organization(&self, session: &AgentSession) {
        let key = identity(session);
        let changed = self.organization_key.borrow().as_deref() != Some(key.as_str());
        if changed || (!self.organization_dirty.get() && !self.organization_is_saving()) {
            let library = self.library.borrow();
            let draft = self.organization_drafts.borrow().get(&key).cloned();
            self.updating.set(true);
            if let Some(draft) = &draft {
                self.alias.set_text(&draft.alias);
                self.tags.set_text(&draft.tags);
                *self.organization_error.borrow_mut() = draft.error.clone();
            } else {
                self.alias.set_text(
                    library
                        .snapshot()
                        .alias(session.provider, &session.id)
                        .unwrap_or(""),
                );
                self.tags.set_text(
                    &library
                        .snapshot()
                        .tags(session.provider, &session.id)
                        .join(", "),
                );
                self.organization_error.borrow_mut().take();
            }
            self.updating.set(false);
            *self.organization_key.borrow_mut() = Some(key);
            self.organization_dirty.set(draft.is_some());
        }
        self.update_organization_controls();
    }

    fn organization_is_saving(&self) -> bool {
        self.organization_saving
            .borrow()
            .as_ref()
            .is_some_and(|key| self.organization_key.borrow().as_ref() == Some(key))
    }

    fn remember_organization_draft(&self) {
        if let Some(key) = self.organization_key.borrow().as_ref() {
            self.organization_drafts.borrow_mut().insert(
                key.clone(),
                OrganizationDraft {
                    alias: self.alias.text().to_string(),
                    tags: self.tags.text().to_string(),
                    error: self.organization_error.borrow().clone(),
                },
            );
        }
    }

    fn organization_changed(&self) {
        if self.updating.get() || self.organization_key.borrow().is_none() {
            return;
        }
        self.organization_dirty.set(true);
        self.organization_error.borrow_mut().take();
        self.remember_organization_draft();
        self.update_organization_controls();
    }

    fn update_organization_controls(&self) {
        let library = self.library.borrow();
        let selected = self.selected_session();
        let loaded = library.is_loaded() && library.error().is_none();
        let saving = self.organization_is_saving();
        let editable = loaded && selected.is_some() && !saving;
        self.alias.set_sensitive(editable);
        self.tags.set_sensitive(editable);
        self.save_details
            .set_sensitive(editable && self.organization_dirty.get() && !library.is_busy());
        self.revert_details
            .set_sensitive(selected.is_some() && self.organization_dirty.get() && !saving);
        self.archive
            .set_sensitive(editable && !library.is_busy() && !self.organization_dirty.get());
        let archived = selected.as_ref().is_some_and(|session| {
            library
                .snapshot()
                .is_archived(session.provider, &session.id)
        });
        self.archive.set_label(if archived {
            "Restore to active"
        } else {
            "Archive locally"
        });
        let message = if saving {
            "Saving organization…"
        } else if self.organization_dirty.get() {
            "Unsaved changes · Save or Revert before archiving"
        } else {
            "Names, tags and archive are shared across all four terminals"
        };
        if let Some(error) = library.error() {
            self.organization_notice.set_text(error);
        } else if let Some(error) = self.organization_error.borrow().as_ref() {
            self.organization_notice.set_text(error);
        } else {
            self.organization_notice.set_text(message);
        }
    }

    fn save_organization(&self) {
        self.finish_organization_save();
        let Some(session) = self.selected_session() else {
            return;
        };
        if !self.save_details.is_sensitive()
            || self.organization_key.borrow().as_deref() != Some(identity(&session).as_str())
        {
            return;
        }
        let result = self.library.borrow_mut().set_details(
            session.provider,
            &session.id,
            self.alias.text().as_str(),
            self.tags.text().as_str(),
        );
        match result {
            Ok(()) => {
                *self.organization_saving.borrow_mut() = Some(identity(&session));
                self.update_organization_controls();
            }
            Err(error) => {
                *self.organization_error.borrow_mut() = Some(error);
                self.remember_organization_draft();
                self.update_organization_controls();
            }
        }
    }

    fn finish_organization_save(&self) {
        if self.library.borrow().is_busy() {
            return;
        }
        let Some(key) = self.organization_saving.borrow_mut().take() else {
            return;
        };
        let error = self.library.borrow().error().map(str::to_owned);
        if error.is_none() {
            self.organization_drafts.borrow_mut().remove(&key);
            if self.organization_key.borrow().as_ref() == Some(&key) {
                self.organization_dirty.set(false);
                self.organization_error.borrow_mut().take();
            }
        } else {
            if let Some(draft) = self.organization_drafts.borrow_mut().get_mut(&key) {
                draft.error.clone_from(&error);
            }
            if self.organization_key.borrow().as_ref() == Some(&key) {
                *self.organization_error.borrow_mut() = error;
            }
        }
    }

    fn revert_organization(&self) {
        if self.organization_is_saving() {
            return;
        }
        if let Some(key) = self.organization_key.borrow().as_ref() {
            self.organization_drafts.borrow_mut().remove(key);
        }
        self.organization_dirty.set(false);
        self.organization_error.borrow_mut().take();
        if let Some(session) = self.selected_session() {
            self.sync_organization(&session);
        }
    }

    fn archive_selected(&self) {
        if !self.archive.is_sensitive() {
            return;
        }
        let Some(session) = self.selected_session() else {
            return;
        };
        let archived = self
            .library
            .borrow()
            .snapshot()
            .is_archived(session.provider, &session.id);
        let result =
            self.library
                .borrow_mut()
                .set_archived(session.provider, &session.id, !archived);
        if let Err(error) = result {
            *self.organization_error.borrow_mut() = Some(error);
        }
        self.update_library_status();
    }
    fn update_library_status(&self) {
        // An acknowledged save can leave the snapshot unchanged (for example,
        // editing a name and then restoring its original text). Such a poll
        // does not request a repaint, but still completes this editor's save.
        self.finish_organization_save();
        let library = self.library.borrow();
        let loaded = library.is_loaded();
        let busy = library.is_busy();
        let message = if let Some(error) = library.error() {
            format!("Saved sessions unavailable: {error}")
        } else if !loaded {
            "Loading saved sessions…".into()
        } else if busy {
            "Saving session library…".into()
        } else {
            format!(
                "{} pinned · Shared across terminals",
                library.snapshot().pinned_count()
            )
        };
        if self.library_status.text().as_str() != message {
            self.library_status.set_text(&message);
            self.library_status.set_tooltip_text(Some(&message));
        }
        self.saved_view.set_sensitive(loaded);
        self.pinned_first.set_sensitive(loaded);
        self.pin.set_sensitive(
            loaded && !busy && library.error().is_none() && self.selected.borrow().is_some(),
        );
        drop(library);
        self.update_organization_controls();
    }

    fn toggle_pin(&self) {
        if self.updating.get() {
            return;
        }
        let session = self.selected_session();
        let Some(session) = session else {
            return;
        };
        let pinned = self.pin.is_active();
        let result = self
            .library
            .borrow_mut()
            .set_pinned(session.provider, &session.id, pinned);
        if let Err(error) = result {
            set_label(&self.library_status, &error);
        } else {
            self.update_library_status();
        }
    }

    fn selected_session(&self) -> Option<AgentSession> {
        let selected = self.selected.borrow().clone();
        self.visible
            .borrow()
            .iter()
            .find(|session| selected.as_deref() == Some(identity(session).as_str()))
            .cloned()
    }

    fn dispatch_activation(&self, session: AgentSession, launch: Option<SessionLaunch>) {
        let key = (session.provider, session.id.clone());
        let title = jterm_core::review_input::safe_inline_display(
            &format!(
                "{} · {}",
                session.provider.label(),
                self.library.borrow().snapshot().display_title(
                    session.provider,
                    &session.id,
                    &session.title
                )
            ),
            192,
        );
        let (completion, result) = mpsc::channel();
        *self.activation.borrow_mut() = Some(Activation {
            key: key.clone(),
            existing: launch.is_none(),
            result,
        });
        self.confirmation.borrow_mut().take();
        self.resume.set_sensitive(false);
        set_label(&self.readiness, "Opening session…");
        (self.launch)(ResumeRequest {
            provider: session.provider,
            id: session.id,
            title,
            launch,
            completion,
        });
    }

    fn poll_activation(&self) {
        let outcome = {
            let pending = self.activation.borrow();
            pending
                .as_ref()
                .and_then(|pending| match pending.result.try_recv() {
                    Ok(result) => Some((pending.key.clone(), pending.existing, result)),
                    Err(mpsc::TryRecvError::Disconnected) => Some((
                        pending.key.clone(),
                        pending.existing,
                        Err("The terminal window could not finish opening this session.".into()),
                    )),
                    Err(mpsc::TryRecvError::Empty) => None,
                })
        };
        if let Some((key, existing, result)) = outcome {
            self.activation.borrow_mut().take();
            match result {
                Ok(()) => {
                    self.completed_activation.set(true);
                    set_label(&self.readiness, "Session opened — release Enter to switch");
                    self.close_when_released();
                }
                Err(error) => {
                    if existing {
                        self.existing.borrow_mut().remove(&key);
                    }
                    self.notice.set_text(&error);
                    self.preflight.borrow_mut().refresh();
                    self.show_selection(self.list.selected_row().map(|row| row.index() as usize));
                }
            }
        }
    }

    fn close_when_released(&self) {
        let window = self.window.clone();
        let down = self.enter_down.clone();
        let completed = self.completed_activation.clone();
        // The shared ownership controller queues physical-release settlement
        // before this idle, so focus transfer cannot invalidate that release.
        gtk::glib::idle_add_local_once(move || {
            if completed.get() && !down.get() {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
            }
        });
    }

    fn poll_confirmation(&self) {
        if self.activation.borrow().is_some() {
            return;
        }
        let Some(key) = self.confirmation.borrow().clone() else {
            return;
        };
        let Some(session) = self.selected_session() else {
            self.confirmation.borrow_mut().take();
            return;
        };
        if key != (session.provider, session.id.clone()) {
            self.confirmation.borrow_mut().take();
            return;
        }
        let ready = self.preflight.borrow().ready_launch(&session).cloned();
        if let Some(launch) = ready {
            self.dispatch_activation(session, Some(launch));
        } else if let SessionPreflightState::Unavailable(error) = self.preflight.borrow().state() {
            self.notice.set_text(error);
            self.confirmation.borrow_mut().take();
        }
    }

    fn resume_selected(&self) {
        if self.activation.borrow().is_some()
            || self.confirmation.borrow().is_some()
            || self.completed_activation.get()
        {
            return;
        }
        let Some(session) = self.selected_session() else {
            return;
        };
        crate::enter_ownership::claim_held();
        let key = (session.provider, session.id.clone());
        if self.existing.borrow().contains(&key) {
            self.dispatch_activation(session, None);
        } else {
            *self.confirmation.borrow_mut() = Some(key);
            self.preflight.borrow_mut().refresh();
            self.show_selection(self.list.selected_row().map(|row| row.index() as usize));
        }
    }
}

pub(crate) fn show(
    parent: &impl IsA<gtk::Window>,
    current_directory: Option<PathBuf>,
    library: Rc<RefCell<SessionLibraryStore>>,
    existing: HashSet<SessionIdentity>,
    launch: impl Fn(ResumeRequest) + 'static,
) {
    show_inner(parent, current_directory, library, existing, launch, true);
}

fn show_inner(
    parent: &impl IsA<gtk::Window>,
    current_directory: Option<PathBuf>,
    library: Rc<RefCell<SessionLibraryStore>>,
    existing: HashSet<SessionIdentity>,
    launch: impl Fn(ResumeRequest) + 'static,
    auto_scan: bool,
) {
    if let Some(window) = MANAGER_WINDOW.with(|slot| slot.borrow().upgrade()) {
        if window.transient_for().as_ref() == Some(parent.as_ref()) {
            window.present();
            return;
        }
    }
    let window = gtk::Window::builder()
        .title("Agent Manager")
        .default_width(1180)
        .default_height(780)
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
    let heading = gtk::Label::new(Some("Agent Manager"));
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
    let quick_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let quick_views = SessionQuickView::ALL
        .into_iter()
        .map(|view| {
            let button = gtk::Button::with_label(view.label());
            button.set_hexpand(true);
            button.set_tooltip_text(Some(match view {
                SessionQuickView::All => "Active sessions in the current search and project scope",
                SessionQuickView::NeedsInput => "Sessions whose latest saved event needed input",
                SessionQuickView::Live => "Sessions with a currently observed CLI process",
                SessionQuickView::Pinned => "Pinned active sessions in the current scope",
                SessionQuickView::Archived => "Locally archived sessions; restore them at any time",
            }));
            quick_row.append(&button);
            (view, button)
        })
        .collect::<Vec<_>>();
    root.append(&quick_row);
    let filters = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search names or conversation · tag:review · provider:codex · -is:archived · \"exact phrase\""));
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
    let sort = gtk::DropDown::from_strings(&[
        "Best match",
        "Most recent",
        "Live first",
        "Recently resumed",
    ]);
    sort.set_tooltip_text(Some("Order matching sessions"));
    let clear = gtk::Button::with_label("Clear filters");
    clear.add_css_class("flat");
    filters.append(&provider);
    filters.append(&status);
    filters.append(&live_only);
    filters.append(&project_only);
    root.append(&filters);
    let project_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let project_label = gtk::Label::new(Some("Project"));
    let project = gtk::DropDown::from_strings(&["All projects"]);
    project.set_hexpand(true);
    project.set_enable_search(true);
    project.update_property(&[gtk::accessible::Property::Label("Session project filter")]);
    project.set_tooltip_text(Some("Browse a project directory and its subdirectories"));
    let project_factory = gtk::SignalListItemFactory::new();
    project_factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_max_width_chars(52);
        item.set_child(Some(&label));
    });
    project_factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let value = item
            .item()
            .unwrap()
            .downcast::<gtk::StringObject>()
            .unwrap();
        let label = item.child().unwrap().downcast::<gtk::Label>().unwrap();
        label.set_text(&value.string());
        label.set_tooltip_text(Some(&value.string()));
    });
    project.set_factory(Some(&project_factory));
    project.set_list_factory(Some(&project_factory));
    let archive_view =
        gtk::DropDown::from_strings(&["Active sessions", "Locally archived", "Active + archived"]);
    archive_view.set_tooltip_text(Some("Local organization shared across terminals"));
    archive_view.update_property(&[gtk::accessible::Property::Label("Session archive view")]);
    project_row.append(&project_label);
    project_row.append(&project);
    project_row.append(&archive_view);
    root.append(&project_row);
    let saved_filters = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let saved_view =
        gtk::DropDown::from_strings(&["All sessions", "★ Pinned only", "Recently resumed only"]);
    saved_view.set_tooltip_text(Some("Saved metadata shared across all four terminals"));
    let pinned_first = gtk::CheckButton::with_label("Pinned first");
    pinned_first.set_active(true);
    pinned_first.set_tooltip_text(Some("Prioritize pins only for an empty best-match search"));
    let library_status = gtk::Label::new(Some("Loading saved sessions…"));
    library_status.set_hexpand(true);
    library_status.set_xalign(0.0);
    library_status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    library_status.add_css_class("dim-label");
    saved_filters.append(&saved_view);
    saved_filters.append(&pinned_first);
    saved_filters.append(&sort);
    saved_filters.append(&clear);
    saved_filters.append(&library_status);
    root.append(&saved_filters);
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
    let inspector = gtk::Box::new(gtk::Orientation::Vertical, 12);
    inspector.set_margin_start(16);
    inspector.set_margin_end(12);
    inspector.set_margin_top(12);
    let organization_heading = gtk::Label::new(Some("Organize session"));
    organization_heading.set_xalign(0.0);
    organization_heading.add_css_class("heading");
    inspector.append(&organization_heading);
    let organization_fields = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(6)
        .column_homogeneous(true)
        .build();
    let alias_label = gtk::Label::new(Some("Display name"));
    alias_label.set_xalign(0.0);
    alias_label.add_css_class("dim-label");
    organization_fields.attach(&alias_label, 0, 0, 1, 1);
    let alias = gtk::Entry::new();
    alias.set_max_length(512);
    alias.set_hexpand(true);
    alias.update_property(&[gtk::accessible::Property::Label("Session display name")]);
    alias.set_placeholder_text(Some("Use the original title when empty"));
    alias.set_tooltip_text(Some(
        "A local display name shared across your four terminals; up to 160 UTF-8 bytes",
    ));
    organization_fields.attach(&alias, 0, 1, 1, 1);
    let tags_label = gtk::Label::new(Some("Tags"));
    tags_label.set_xalign(0.0);
    tags_label.add_css_class("dim-label");
    organization_fields.attach(&tags_label, 1, 0, 1, 1);
    let tags = gtk::Entry::new();
    tags.set_max_length(1024);
    tags.set_hexpand(true);
    tags.update_property(&[gtk::accessible::Property::Label("Session tags")]);
    tags.set_placeholder_text(Some("frontend, review, urgent"));
    tags.set_tooltip_text(Some(
        "Separate up to 12 tags with commas; search with tag:review",
    ));
    organization_fields.attach(&tags, 1, 1, 1, 1);
    inspector.append(&organization_fields);
    let organization_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let save_details = gtk::Button::with_label("Save");
    save_details.add_css_class("suggested-action");
    let revert_details = gtk::Button::with_label("Revert");
    let archive = gtk::Button::with_label("Archive locally");
    archive.set_tooltip_text(Some(
        "Hide this session from the active view. Its conversation remains available to restore.",
    ));
    organization_actions.append(&save_details);
    organization_actions.append(&revert_details);
    organization_actions.append(&archive);
    inspector.append(&organization_actions);
    let organization_notice = gtk::Label::new(None);
    organization_notice.set_xalign(0.0);
    organization_notice.set_wrap(true);
    organization_notice.add_css_class("dim-label");
    inspector.append(&organization_notice);
    inspector.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let detail_heading = gtk::Label::new(Some("Session & recent conversation"));
    detail_heading.set_xalign(0.0);
    detail_heading.add_css_class("heading");
    inspector.append(&detail_heading);
    detail.set_margin_start(0);
    detail.set_margin_end(0);
    detail.set_margin_top(0);
    inspector.append(&detail);
    let details_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&inspector)
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
    paned.set_position(520);
    paned.set_vexpand(true);
    root.append(&paned);
    let notice = gtk::Label::new(None);
    notice.set_xalign(0.0);
    notice.set_wrap(true);
    notice.add_css_class("dim-label");
    root.append(&notice);
    let resume = gtk::Button::with_label("Resume in New Tab");
    resume.add_css_class("suggested-action");
    let pin = gtk::ToggleButton::with_label("☆ Pin session");
    let readiness = gtk::Label::new(Some("Select a session to check readiness"));
    readiness.set_hexpand(true);
    readiness.set_xalign(0.0);
    readiness.set_wrap(true);
    readiness.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    readiness.add_css_class("dim-label");
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    actions.append(&pin);
    actions.append(&readiness);
    actions.append(&resume);
    root.append(&actions);
    window.set_child(Some(&root));
    let manager = Rc::new(Manager {
        window: window.downgrade(),
        library,
        existing: RefCell::new(existing),
        preflight: RefCell::new(SessionPreflight::new()),
        confirmation: RefCell::new(None),
        activation: RefCell::new(None),
        enter_down: Rc::new(Cell::new(false)),
        completed_activation: Rc::new(Cell::new(false)),
        saved_view,
        archive_view,
        quick_views,
        project,
        project_paths: RefCell::new(Vec::new()),
        project_labels: RefCell::new(Vec::new()),
        selected_project: RefCell::new(None),
        alias,
        tags,
        save_details,
        revert_details,
        organization_notice,
        organization_key: RefCell::new(None),
        organization_dirty: Cell::new(false),
        organization_drafts: RefCell::new(HashMap::new()),
        organization_saving: RefCell::new(None),
        organization_error: RefCell::new(None),
        action_identity: RefCell::new(None),
        archive,
        pinned_first,
        pin,
        library_status,
        readiness,
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
    for (view, button) in &manager.quick_views {
        let weak = Rc::downgrade(&manager);
        let view = *view;
        button.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.set_quick_view(view);
            }
        });
    }
    let weak = Rc::downgrade(&manager);
    manager.project.connect_selected_notify(move |project| {
        if let Some(manager) = weak.upgrade() {
            if manager.updating.get() {
                return;
            }
            *manager.selected_project.borrow_mut() = project
                .selected()
                .checked_sub(1)
                .and_then(|index| manager.project_paths.borrow().get(index as usize).cloned());
            manager.updating.set(true);
            manager.project_only.set_active(false);
            manager.updating.set(false);
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.archive_view.connect_selected_notify(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    for entry in [&manager.alias, &manager.tags] {
        let weak = Rc::downgrade(&manager);
        entry.connect_changed(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.organization_changed();
            }
        });
    }
    let weak = Rc::downgrade(&manager);
    manager.save_details.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.save_organization();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.revert_details.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.revert_organization();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.archive.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.archive_selected();
        }
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
            if manager.updating.get() {
                return;
            }
            if manager.project_only.is_active() {
                manager.updating.set(true);
                manager.selected_project.borrow_mut().take();
                manager.project.set_selected(0);
                manager.updating.set(false);
            }
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
    manager.saved_view.connect_selected_notify(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.pinned_first.connect_toggled(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.filters_changed();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.pin.connect_clicked(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.toggle_pin();
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
        if !(modifiers - gtk::gdk::ModifierType::LOCK_MASK).is_empty() {
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
            manager.preflight.borrow_mut().refresh();
            manager.library.borrow_mut().refresh();
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
    let weak = Rc::downgrade(&manager);
    keys.connect_key_pressed(move |_, key, _, _| {
        if matches!(
            key,
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter
        ) {
            if let Some(manager) = weak.upgrade() {
                let confirms_session = manager
                    .window
                    .upgrade()
                    .and_then(|window| gtk::prelude::GtkWindowExt::focus(&window))
                    .is_some_and(|focus| {
                        [
                            manager.search.upcast_ref::<gtk::Widget>(),
                            manager.list.upcast_ref(),
                            manager.resume.upcast_ref(),
                        ]
                        .iter()
                        .any(|target| focus == **target || focus.is_ancestor(*target))
                    });
                if confirms_session {
                    manager.enter_down.set(true);
                }
            }
        }
        gtk::glib::Propagation::Proceed
    });
    let hint_owner = Rc::downgrade(&manager);
    crate::enter_ownership::install_controller_with_hint(&keys, move || {
        if let Some(manager) = hint_owner.upgrade() {
            manager
                .notice
                .set_text(crate::enter_ownership::RELEASE_HINT);
        }
    });
    let weak = Rc::downgrade(&manager);
    keys.connect_key_released(move |controller, key, _, _| {
        if controller
            .current_event()
            .is_none_or(|event| event.event_type() != gtk::gdk::EventType::KeyRelease)
        {
            return;
        }
        if matches!(
            key,
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter
        ) {
            if let Some(manager) = weak.upgrade() {
                manager.enter_down.set(false);
                if manager.completed_activation.get() {
                    manager.close_when_released();
                }
            }
        }
    });

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
        let library_changed = manager.library.borrow_mut().poll();
        if library_changed {
            manager.render_view(true);
        }
        manager.update_library_status();
        let ready_changed = manager.preflight.borrow_mut().poll();
        if ready_changed {
            manager.show_selection(manager.list.selected_row().map(|row| row.index() as usize));
        }
        manager.poll_confirmation();
        manager.poll_activation();
        let catalog = manager.scanner.borrow_mut().poll();
        if let Some(catalog) = catalog {
            manager.accept_catalog(catalog);
        }
        if auto_scan && manager.last_refresh.borrow().elapsed() >= Duration::from_secs(5) {
            manager.library.borrow_mut().refresh();
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
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let library = Rc::new(RefCell::new(SessionLibraryStore::with_path(
            directory.join("library.json"),
        )));
        let received = Rc::new(RefCell::new(None));
        let captured = received.clone();
        let opened_title = Rc::new(RefCell::new(None));
        let captured_title = opened_title.clone();
        let parent = gtk::Window::new();
        show_inner(
            &parent,
            Some(directory.clone()),
            library.clone(),
            HashSet::new(),
            move |request| {
                *captured.borrow_mut() = request.launch.clone();
                *captured_title.borrow_mut() = Some(request.title.clone());
                let _ = request.completion.send(Ok(()));
            },
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
        let wait_library = |library: &Rc<RefCell<SessionLibraryStore>>| {
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end {
                library.borrow_mut().poll();
                if library.borrow().is_loaded() && !library.borrow().is_busy() {
                    library.borrow_mut().poll();
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(
                library.borrow().error().is_none(),
                "{:?}",
                library.borrow().error()
            );
        };
        wait_library(&library);
        manager.show_selection(Some(0));
        let settle_events = || {
            let context = gtk::glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_millis(350);
            while Instant::now() < deadline {
                while context.pending() {
                    context.iteration(false);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let first = manager.visible.borrow()[0].clone();
        let second = manager.visible.borrow()[1].clone();
        manager.existing.borrow_mut().extend([
            (first.provider, first.id.clone()),
            (second.provider, second.id.clone()),
        ]);
        manager.show_selection(Some(0));
        // GtkButton's keyboard action waits 250 ms before clicked. Begin all
        // eligible actions for A, then move to B before that timeout expires.
        for button in [
            &manager.archive,
            manager.pin.upcast_ref::<gtk::Button>(),
            &manager.resume,
        ] {
            assert!(button.is_sensitive() && button.is_realized());
            button.emit_by_name::<()>("activate", &[]);
        }
        manager
            .list
            .select_row(manager.list.row_at_index(1).as_ref());
        settle_events();
        assert!(
            opened_title.borrow().is_none(),
            "a delayed resume cannot switch B"
        );
        for session in [&first, &second] {
            assert!(!library
                .borrow()
                .snapshot()
                .is_archived(session.provider, &session.id));
            assert!(!library
                .borrow()
                .snapshot()
                .is_pinned(session.provider, &session.id));
        }
        manager.existing.borrow_mut().clear();
        manager
            .list
            .select_row(manager.list.row_at_index(0).as_ref());
        settle_events();
        let controllers = manager.archive.observe_controllers();
        let click = (0..controllers.n_items())
            .find_map(|index| controllers.item(index).and_downcast::<gtk::GestureClick>())
            .expect("GtkButton click gesture");
        assert!(manager.archive.contains(8.0, 8.0));
        click.emit_by_name::<()>("pressed", &[&1_i32, &8.0_f64, &8.0_f64]);
        manager
            .list
            .select_row(manager.list.row_at_index(1).as_ref());
        settle_events();
        assert!(manager.archive.contains(8.0, 8.0));
        click.emit_by_name::<()>("released", &[&1_i32, &8.0_f64, &8.0_f64]);
        settle_events();
        assert!(
            !library
                .borrow()
                .snapshot()
                .is_archived(second.provider, &second.id),
            "a pointer press for A cannot archive B on release"
        );
        manager
            .list
            .select_row(manager.list.row_at_index(0).as_ref());
        let organized = manager.selected_session().unwrap();
        manager.alias.set_text("Renderer command center");
        manager.tags.set_text("review, frontend");
        assert!(manager.organization_dirty.get());
        assert!(!manager.archive.is_sensitive());
        manager
            .list
            .select_row(manager.list.row_at_index(1).as_ref());
        assert!(!manager.organization_dirty.get());
        manager
            .list
            .select_row(manager.list.row_at_index(0).as_ref());
        assert_eq!(manager.alias.text().as_str(), "Renderer command center");
        assert_eq!(manager.tags.text().as_str(), "review, frontend");
        assert!(
            manager.organization_dirty.get(),
            "row navigation retains identity-keyed drafts"
        );
        manager.search.set_text("no such session");
        assert!(manager.visible.borrow().is_empty());
        manager.clear_filters();
        assert_eq!(manager.alias.text().as_str(), "Renderer command center");
        assert!(
            manager.organization_dirty.get(),
            "filters retain a hidden session's draft"
        );
        let mut refreshed = manager.catalog.borrow().clone();
        refreshed.sessions[0]
            .preview
            .push_str(" · new saved activity");
        manager.accept_catalog(refreshed);
        assert_eq!(manager.alias.text().as_str(), "Renderer command center");
        assert_eq!(manager.tags.text().as_str(), "review, frontend");
        assert!(
            manager.organization_dirty.get(),
            "background scans preserve drafts"
        );
        assert!(library
            .borrow()
            .snapshot()
            .alias(organized.provider, &organized.id)
            .is_none());
        manager
            .list
            .select_row(manager.list.row_at_index(1).as_ref());
        manager.alias.set_text("Keep this unsaved draft");
        manager
            .list
            .select_row(manager.list.row_at_index(0).as_ref());
        for button in [&manager.save_details, &manager.revert_details] {
            assert!(button.is_sensitive() && button.is_realized());
            button.emit_by_name::<()>("activate", &[]);
        }
        manager
            .list
            .select_row(manager.list.row_at_index(1).as_ref());
        settle_events();
        assert!(
            manager.organization_dirty.get(),
            "a delayed revert must not discard B"
        );
        assert_eq!(manager.alias.text().as_str(), "Keep this unsaved draft");
        assert!(
            library
                .borrow()
                .snapshot()
                .alias(second.provider, &second.id)
                .is_none(),
            "a delayed save must not write B"
        );
        manager.revert_organization();
        manager
            .list
            .select_row(manager.list.row_at_index(0).as_ref());
        manager.save_organization();
        assert_eq!(
            manager.organization_saving.borrow().as_deref(),
            Some(identity(&organized).as_str())
        );
        manager
            .list
            .select_row(manager.list.row_at_index(1).as_ref());
        let other = manager.selected_session().unwrap();
        manager.alias.set_text("Other session draft");
        manager.tags.set_text("backend, held");
        wait_library(&library);
        manager.render_view(true);
        assert_eq!(manager.selected_session().unwrap().id, other.id);
        assert!(
            manager.organization_dirty.get(),
            "completion for A must not clear draft B"
        );
        assert_eq!(manager.alias.text().as_str(), "Other session draft");
        assert_eq!(manager.tags.text().as_str(), "backend, held");
        assert!(library
            .borrow()
            .snapshot()
            .alias(other.provider, &other.id)
            .is_none());
        manager.revert_organization();
        manager
            .list
            .select_row(manager.list.row_at_index(0).as_ref());
        assert!(!manager.organization_dirty.get());
        assert_eq!(
            library
                .borrow()
                .snapshot()
                .alias(organized.provider, &organized.id),
            Some("Renderer command center")
        );
        assert_eq!(
            library
                .borrow()
                .snapshot()
                .tags(organized.provider, &organized.id),
            &["frontend", "review"]
        );
        manager.alias.set_text("Temporary name");
        manager.alias.set_text("Renderer command center");
        manager.save_organization();
        assert!(manager.organization_saving.borrow().is_some());
        wait_library(&library);
        manager.update_library_status();
        assert!(
            manager.organization_saving.borrow().is_none(),
            "an unchanged snapshot still acknowledges the save"
        );
        assert!(!manager.organization_dirty.get());
        assert!(manager.alias.is_sensitive());
        manager.search.set_text("tag:review provider:codex");
        assert_eq!(manager.visible.borrow().len(), 1);
        assert_eq!(manager.selected_session().unwrap().id, organized.id);
        manager.clear_filters();
        manager.alias.set_text(&"x".repeat(161));
        manager.save_organization();
        assert!(manager.organization_saving.borrow().is_none());
        assert!(manager.organization_error.borrow().is_some());
        manager.update_library_status();
        assert!(manager
            .organization_error
            .borrow()
            .as_ref()
            .is_some_and(|error| manager.organization_notice.text().as_str() == error));
        manager.revert_organization();
        assert_eq!(manager.alias.text().as_str(), "Renderer command center");
        manager.archive_selected();
        wait_library(&library);
        manager.render_view(true);
        assert!(library
            .borrow()
            .snapshot()
            .is_archived(organized.provider, &organized.id));
        assert_eq!(manager.visible.borrow().len(), 1);
        assert!(manager
            .visible
            .borrow()
            .iter()
            .all(|session| session.id != organized.id));
        manager.set_quick_view(SessionQuickView::Archived);
        assert_eq!(manager.visible.borrow().len(), 1);
        assert_eq!(manager.selected_session().unwrap().id, organized.id);
        assert_eq!(
            manager.archive.label().as_deref(),
            Some("Restore to active")
        );
        manager.archive_selected();
        wait_library(&library);
        manager.render_view(true);
        assert!(manager.visible.borrow().is_empty());
        manager.set_quick_view(SessionQuickView::All);
        assert_eq!(manager.visible.borrow().len(), 2);
        assert!(
            received.borrow().is_none(),
            "organization and view changes never launch a provider"
        );
        assert!(
            opened_title.borrow().is_none(),
            "organization does not even request a tab switch"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "{}\n");
        assert!(library
            .borrow()
            .snapshot()
            .last_resumed(organized.provider, &organized.id)
            .is_none());
        manager.set_quick_view(SessionQuickView::NeedsInput);
        assert_eq!(manager.selected_session().unwrap().id, organized.id);
        assert_eq!(manager.visible.borrow().len(), 1);
        manager.set_quick_view(SessionQuickView::Live);
        assert_eq!(manager.visible.borrow().len(), 1);
        manager.clear_filters();
        manager.show_selection(Some(0));
        let pinned = manager.selected_session().unwrap();
        let pinned_row = manager.list.row_at_index(0).unwrap();
        manager.pin.set_active(true);
        manager.toggle_pin();
        wait_library(&library);
        manager.render_view(true);
        assert!(library
            .borrow()
            .snapshot()
            .is_pinned(pinned.provider, &pinned.id));
        assert_eq!(
            manager.list.row_at_index(0).unwrap(),
            pinned_row,
            "pin marker updates reuse the GTK row"
        );
        assert!(manager.rows.borrow()[0].title.text().starts_with('★'));
        assert!(
            received.borrow().is_none(),
            "pinning never launches a session"
        );
        let restarted = Rc::new(RefCell::new(SessionLibraryStore::with_path(
            directory.join("library.json"),
        )));
        wait_library(&restarted);
        assert!(
            restarted
                .borrow()
                .snapshot()
                .is_pinned(pinned.provider, &pinned.id),
            "pins survive a fresh store instance"
        );
        manager.saved_view.set_selected(1);
        assert_eq!(manager.matched.borrow().len(), 1);
        manager.pin.set_active(false);
        manager.toggle_pin();
        wait_library(&library);
        manager.render_view(true);
        assert!(
            manager.visible.borrow().is_empty(),
            "unpinning respects the pinned-only filter"
        );
        assert!(received.borrow().is_none());
        manager.clear_filters();
        let resumed = manager.catalog.borrow().sessions[1].clone();
        // The host alone records a completed activation; opening/filtering the
        // manager and both pin mutations have left all resume timestamps empty.
        assert!(library
            .borrow()
            .snapshot()
            .last_resumed(pinned.provider, &pinned.id)
            .is_none());
        library
            .borrow_mut()
            .record_resumed(resumed.provider, &resumed.id)
            .unwrap();
        wait_library(&library);
        manager.render_view(true);
        manager.saved_view.set_selected(2);
        assert_eq!(manager.visible.borrow().len(), 1);
        assert_eq!(manager.selected_session().unwrap().id, resumed.id);
        manager.sort.set_selected(3);
        assert!(manager.rows.borrow()[0].meta.text().contains("Resumed"));
        manager.clear_filters();

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
        let project_index = manager
            .project_paths
            .borrow()
            .iter()
            .position(|path| path == &nested)
            .unwrap();
        manager.project.set_selected(project_index as u32 + 1);
        assert_eq!(manager.matched.borrow().len(), 1024);
        assert_eq!(manager.selected_project.borrow().as_ref(), Some(&nested));
        assert!(manager
            .visible
            .borrow()
            .iter()
            .all(|session| session.cwd == nested));
        manager.accept_catalog(large.clone());
        assert_eq!(manager.selected_project.borrow().as_ref(), Some(&nested));
        manager.clear_filters();
        assert!(manager.selected_project.borrow().is_none());
        assert_eq!(manager.matched.borrow().len(), 2048);
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
        let wait_ready = |manager: &Manager| {
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end {
                manager.preflight.borrow_mut().poll();
                if !manager.preflight.borrow().is_busy() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            manager.show_selection(manager.list.selected_row().map(|row| row.index() as usize));
        };
        wait_ready(&manager);
        std::fs::remove_file(&source).unwrap();
        manager.resume_selected();
        wait_ready(&manager);
        manager.poll_confirmation();
        assert!(
            received.borrow().is_none(),
            "stale history cannot launch from a displayed snapshot"
        );
        assert!(manager.notice.text().contains("no longer exists"));
        std::fs::write(&source, "{}\n").unwrap();
        // An already open tab remains switchable independently of launch
        // readiness; the host acknowledges it without a new process plan.
        manager.confirmation.borrow_mut().take();
        let target = manager.selected_session().unwrap();
        manager
            .existing
            .borrow_mut()
            .insert((target.provider, target.id.clone()));
        manager.search.emit_activate();
        manager.poll_activation();
        assert!(
            received.borrow().is_none(),
            "existing focus never requests a process launch"
        );
        // Prepared launch correctness is covered by native host smoke; this
        // component test also validates generic provider/session identity.
        assert_eq!(target.provider, SessionProvider::Codex);
        assert_eq!(target.id, "01912345-1234-7123-8123-123456789abc");
        assert_eq!(
            opened_title.borrow().as_deref(),
            Some("Codex · Renderer command center")
        );
        parent.close();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
