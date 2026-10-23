//! Native session switcher for provider-owned CLI conversations.
use gtk::prelude::*;
use jterm_core::agent_sessions::{
    AgentSession, SessionCatalog, SessionLaunch, SessionScanner, SessionStatus,
};
use relm4::gtk;
use std::cell::RefCell;
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

impl Manager {
    fn refresh(&self) {
        if self.scanner.borrow_mut().refresh() {
            self.refresh.set_sensitive(false);
            self.notice
                .set_text("Reading local Codex and Claude sessions…");
            *self.last_refresh.borrow_mut() = Instant::now();
        }
    }

    fn render(&self) {
        let selected = self.selected.borrow().clone();
        // Drop the session borrow before changing selection, which emits a signal.
        let sessions = {
            let catalog = self.catalog.borrow();
            let query = self.search.text();
            let provider = self.provider.selected();
            let status = self.status.selected();
            let filtered: Vec<_> = catalog
                .sessions
                .iter()
                .filter(|session| {
                    session.matches_query(query.as_str())
                        && (!self.live_only.is_active() || !catalog.live_pids(session).is_empty())
                        && match provider {
                            1 => session.provider.label() == "Codex",
                            2 => session.provider.label() == "Claude",
                            _ => true,
                        }
                        && match status {
                            1 => session.status == SessionStatus::Working,
                            2 => session.status == SessionStatus::Waiting,
                            3 => session.status == SessionStatus::Idle,
                            4 => session.status == SessionStatus::Unknown,
                            5 => session.status == SessionStatus::Archived,
                            _ => true,
                        }
                })
                .cloned()
                .collect();
            let attention = catalog
                .sessions
                .iter()
                .filter(|session| session.status == SessionStatus::Waiting)
                .count();
            self.overview.set_text(&format!(
                "{} sessions  ·  {} need attention  ·  {} observed CLI processes  ·  {} shown",
                catalog.sessions.len(),
                attention,
                catalog.live_processes.len(),
                filtered.len()
            ));
            self.notice.set_text(if catalog.sessions.is_empty() {
                "No saved sessions found. Start Codex or Claude, then refresh. Custom CODEX_HOME and CLAUDE_CONFIG_DIR are supported."
            } else if filtered.is_empty() {
                "No matching sessions. Try a project name, provider, session ID, or words from your conversation."
            } else if catalog.warnings.is_empty() {
                "Local history · Status describes the latest saved event; a quiet transcript does not prove the agent is running."
            } else { &catalog.warnings[0] });
            filtered
        };
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        *self.visible.borrow_mut() = sessions;
        let mut selected_row = None;
        for (index, session) in self.visible.borrow().iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
            content.set_margin_top(10);
            content.set_margin_bottom(10);
            content.set_margin_start(12);
            content.set_margin_end(12);
            let title = gtk::Label::new(Some(&session.title));
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.add_css_class("heading");
            let pids = self.catalog.borrow().live_pids(session);
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
            let meta = gtk::Label::new(Some(&format!(
                "{}  ·  {}  ·  {}{}",
                session.provider.label(),
                session.status.label(),
                age(session.updated_at_ms),
                live
            )));
            meta.set_xalign(0.0);
            meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
            meta.set_tooltip_text(Some(meta.text().as_str()));
            meta.add_css_class("dim-label");
            let cwd = gtk::Label::new(Some(&session.cwd.to_string_lossy()));
            cwd.set_xalign(0.0);
            cwd.set_ellipsize(gtk::pango::EllipsizeMode::Start);
            cwd.add_css_class("dim-label");
            content.append(&title);
            content.append(&meta);
            content.append(&cwd);
            row.set_child(Some(&content));
            self.list.append(&row);
            if selected.as_deref() == Some(identity(session).as_str()) {
                selected_row = Some(index as i32);
            }
        }
        if let Some(row) = self.list.row_at_index(selected_row.unwrap_or(0)) {
            self.list.select_row(Some(&row));
        } else {
            self.show_selection(None);
        }
    }

    fn show_selection(&self, index: Option<usize>) {
        let visible = self.visible.borrow();
        let Some(session) = index.and_then(|index| visible.get(index)) else {
            self.selected.borrow_mut().take();
            self.detail.set_text("Select a session to inspect its project and recent conversation.\n\n↑ / ↓ navigate · Enter opens the selected session in a new tab · Esc closes");
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
            "No matching resumed CLI process observed".to_string()
        } else {
            format!(
                "CLI process observed: PID {} (activity is shown separately)",
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        self.detail.set_text(&format!(
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
        ));
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
    launch: impl Fn(SessionLaunch, String) + 'static,
) {
    show_inner(parent, launch, true);
}

fn show_inner(
    parent: &impl IsA<gtk::Window>,
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
    filters.append(&search);
    filters.append(&provider);
    filters.append(&status);
    filters.append(&live_only);
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
    paned.set_start_child(Some(&list_scroll));
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
            manager.render();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.provider.connect_selected_notify(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.render();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.status.connect_selected_notify(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.render();
        }
    });
    let weak = Rc::downgrade(&manager);
    manager.live_only.connect_toggled(move |_| {
        if let Some(manager) = weak.upgrade() {
            manager.render();
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
        if modifiers.intersects(
            gtk::gdk::ModifierType::CONTROL_MASK
                | gtk::gdk::ModifierType::ALT_MASK
                | gtk::gdk::ModifierType::SUPER_MASK,
        ) {
            return gtk::glib::Propagation::Proceed;
        }
        let delta = match key {
            gtk::gdk::Key::Up => -1,
            gtk::gdk::Key::Down => 1,
            _ => return gtk::glib::Propagation::Proceed,
        };
        if let Some(manager) = weak.upgrade() {
            let count = manager.visible.borrow().len() as i32;
            if count > 0 {
                let current = manager
                    .list
                    .selected_row()
                    .map(|row| row.index())
                    .unwrap_or(0);
                let next = (current + delta).clamp(0, count - 1);
                if let Some(row) = manager.list.row_at_index(next) {
                    manager.list.select_row(Some(&row));
                }
            }
        }
        gtk::glib::Propagation::Stop
    });
    manager.search.add_controller(search_keys);
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
    keys.connect_key_pressed(move |_, key, _, _| {
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
    manager.show_selection(None);
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
            *manager.catalog.borrow_mut() = catalog;
            manager.refresh.set_sensitive(true);
            manager.render();
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

        assert!(manager.overview.text().contains("1 need attention"));
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
