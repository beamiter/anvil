//! Native Vim9 workbench. Vim owns every editing operation and modified buffer;
//! GTK/Pango paints the buffer from Vim's GUI RGB frame. VTE owns the real
//! interactive PTY/input path and displays views outside the native contract.

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use jterm_core::vim9::{
    list_project_directory, ProjectEntry, Vim9Command, Vim9EntryRequest, Vim9Session, Vim9UiConfig,
};
use relm4::{adw, gtk};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use vte4::prelude::*;

#[path = "vim9_ide_buffer.rs"]
mod native_buffer;
#[path = "vim9_ide_navigation.rs"]
mod navigation;
#[path = "vim9_ide_search.rs"]
mod project_search;

fn visible(text: &str) -> String {
    jterm_core::review_input::safe_inline_display(text, 1024)
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    label
}

fn button(icon: &str, title: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(title));
    button.set_focus_on_click(false);
    button.update_property(&[gtk::accessible::Property::Label(title)]);
    button.add_css_class("flat");
    button
}

fn scroll(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_hexpand(true);
    scroll.set_vexpand(true);
    scroll.set_child(Some(child));
    scroll
}

fn dispatch(
    session: &Vim9Session,
    command: Vim9Command,
    message: &gtk::Label,
    editor: &vte4::Terminal,
) {
    match session.send(command) {
        Ok(_) => {
            message.set_label("");
            editor.grab_focus();
        }
        Err(error) => message.set_label(&visible(&error.to_string())),
    }
}

fn command_button(
    session: &Vim9Session,
    editor: &vte4::Terminal,
    message: &gtk::Label,
    icon: &str,
    title: &str,
    command: impl Fn() -> Vim9Command + 'static,
) -> gtk::Button {
    let button = button(icon, title);
    let session = session.clone();
    let editor = editor.clone();
    let message = message.clone();
    button.connect_clicked(move |_| dispatch(&session, command(), &message, &editor));
    button
}

/// One bounded directory read at a time, off the GTK thread. File filtering
/// never recursively scans a repository or follows a symlink outside its root.
struct Browser {
    root: PathBuf,
    directory: RefCell<PathBuf>,
    entries: RefCell<Vec<ProjectEntry>>,
    shown: RefCell<Vec<(PathBuf, bool)>>,
    busy: Cell<bool>,
    list: gtk::ListBox,
    query: gtk::SearchEntry,
    location: gtk::Label,
    up: gtk::Button,
    message: gtk::Label,
    session: Vim9Session,
    editor: vte4::Terminal,
}

impl Browser {
    fn render(&self) {
        let selected = self.list.selected_row().and_then(|row| {
            self.shown
                .borrow()
                .get(row.index() as usize)
                .map(|entry| entry.0.clone())
        });
        self.list.remove_all();
        let needle = self.query.text().to_lowercase();
        let entries = self.entries.borrow();
        let mut shown = self.shown.borrow_mut();
        shown.clear();
        for entry in entries
            .iter()
            .filter(|entry| entry.name.to_lowercase().contains(&needle))
        {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 7);
            row.set_margin_start(8);
            row.set_margin_end(8);
            row.set_margin_top(5);
            row.set_margin_bottom(5);
            row.append(&gtk::Image::from_icon_name(if entry.is_dir {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            }));
            row.append(&label(&visible(&entry.name)));
            row.set_tooltip_text(Some(&visible(&entry.path.display().to_string())));
            self.list.append(&row);
            shown.push((entry.path.clone(), entry.is_dir));
        }
        let selected_index = shown
            .iter()
            .position(|entry| Some(&entry.0) == selected.as_ref())
            .unwrap_or(0);
        self.list
            .select_row(self.list.row_at_index(selected_index as i32).as_ref());
    }

    fn navigate(this: &Rc<Self>, directory: PathBuf) {
        if this.busy.replace(true) {
            return;
        }
        this.list.set_sensitive(false);
        this.up.set_sensitive(false);
        this.location.set_label("Loading directory…");
        let root = this.root.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = directory.canonicalize().and_then(|directory| {
                if !directory.starts_with(&root) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "This folder leaves the project root; use Open file to choose an external file.",
                    ));
                }
                let entries = list_project_directory(&directory)?;
                Ok((directory, entries))
            });
            let _ = tx.send(result);
        });
        let weak = Rc::downgrade(this);
        glib::timeout_add_local(Duration::from_millis(40), move || {
            let Some(browser) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match rx.try_recv() {
                Ok(result) => {
                    browser.busy.set(false);
                    browser.list.set_sensitive(true);
                    match result {
                        Ok((directory, entries)) => {
                            *browser.directory.borrow_mut() = directory;
                            *browser.entries.borrow_mut() = entries;
                            browser.query.set_text("");
                            browser.render();
                        }
                        Err(error) => browser.message.set_label(&visible(&error.to_string())),
                    }
                    let directory = browser.directory.borrow();
                    let capped = browser.entries.borrow().len() >= 1024;
                    browser.location.set_label(&format!(
                        "{}{}",
                        visible(&directory.display().to_string()),
                        if capped { " · 1,024+ entries" } else { "" }
                    ));
                    browser
                        .location
                        .set_tooltip_text(Some(&visible(&directory.display().to_string())));
                    browser.up.set_sensitive(*directory != browser.root);
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    browser.busy.set(false);
                    browser.list.set_sensitive(true);
                    browser
                        .up
                        .set_sensitive(*browser.directory.borrow() != browser.root);
                    browser
                        .message
                        .set_label("Project directory reader stopped. Use Refresh to retry.");
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn activate(this: &Rc<Self>, index: i32) {
        let Some((path, directory)) = this.shown.borrow().get(index as usize).cloned() else {
            return;
        };
        if directory {
            Self::navigate(this, path);
        } else if let Some(path) = path.to_str() {
            dispatch(
                &this.session,
                Vim9Command::Open {
                    path: path.into(),
                    line: None,
                    column: None,
                },
                &this.message,
                &this.editor,
            );
        } else {
            this.message
                .set_label("Vim9 IDE currently requires UTF-8 file paths.");
        }
    }
}

fn default_build(root: &Path) -> &'static str {
    if root.join("Cargo.toml").is_file() {
        "cargo check --message-format=short"
    } else if root.join("Makefile").is_file() {
        "make"
    } else if root.join("CMakeLists.txt").is_file() {
        "cmake --build build"
    } else {
        "make"
    }
}

fn open_error(
    application: &gtk::Application,
    parent: Option<&adw::ApplicationWindow>,
    error: &str,
) {
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Vim9 IDE — launch error")
        .default_width(560)
        .default_height(240)
        .build();
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }
    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    let heading = label("Vim9 IDE could not start");
    heading.add_css_class("title-2");
    let detail = label(&visible(error));
    detail.set_wrap(true);
    detail.set_ellipsize(gtk::pango::EllipsizeMode::None);
    detail.set_selectable(true);
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    let weak = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    content.append(&heading);
    content.append(&detail);
    content.append(&close);
    window.set_content(Some(&content));
    window.present();
}

/// Existing terminal windows explicitly quit their application on close. Give
/// each editor its own process so that terminal teardown cannot kill unsaved
/// buffers. The same binary's --ide path owns the native workbench below.
pub(crate) fn launch(request: Vim9EntryRequest) -> std::io::Result<()> {
    use std::process::Command;
    let env = jterm_core::child_env::vte_envv_from_captured(
        &jterm_core::child_env::ChildEnv::from_identity(),
        &[],
    )?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--ide")
        // The editor is another window of this application, so carry the
        // selected config explicitly. The frozen PTY environment intentionally
        // omits process-local --config overrides for ordinary shell children.
        .arg("--config")
        .arg(crate::config::config_file_path())
        .arg("--working-directory")
        .arg(request.target());
    command.env_clear();
    for entry in &env {
        if let Some((name, value)) = entry.split_once('=') {
            command.env(name, value);
        }
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Prepare automatic Vim entry only for local interactive shell panes. The
/// shim preserves shell cwd/arguments and is independent of buffer rendering.
pub(crate) fn shell_environment(argv: &[String], enabled: bool) -> Vec<(String, String)> {
    if !enabled {
        // A nested terminal may inherit another terminal's shim or Bash
        // function. Explicit opt-out must also disable that inherited route.
        return vec![("JTERM_VIM9_AUTO_GUI".into(), "0".into())];
    }
    if crate::host::is_flatpak() || !jterm_core::vim9::shell_argv_eligible(argv) {
        return Vec::new();
    }
    let prepared = std::env::current_exe()
        .and_then(|program| jterm_core::vim9::Vim9ShellIntegration::prepare(&program));
    match prepared {
        Ok(integration) => {
            let mut env = integration.env();
            env.push((
                "JTERM_VIM9_CONFIG".into(),
                crate::config::config_file_path()
                    .to_string_lossy()
                    .into_owned(),
            ));
            env
        }
        Err(error) => {
            log::warn!("Cannot prepare automatic GUI Vim entry: {error}");
            Vec::new()
        }
    }
}

/// The originating shell waits for this window and receives Vim's actual exit
/// code, including :cquit. Failed children cannot hold the prompt open forever.
pub(crate) fn run_shell(request: Vim9EntryRequest, application_id: &str) -> i32 {
    let application = adw::Application::builder()
        .application_id(application_id)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let outcome = Rc::new(Cell::new(None));
    let request = RefCell::new(Some(request));
    let result = outcome.clone();
    application.connect_activate(move |application| {
        if let Some(request) = request.borrow_mut().take() {
            let config = crate::config::load_config().0.vim9;
            open_with_config_and_exit(
                application.upcast_ref(),
                None,
                request,
                config,
                Some(result.clone()),
            );
        } else if let Some(window) = application.active_window() {
            window.present();
        }
    });
    let _ = application.run_with_args(&["anvil"]);
    outcome.get().unwrap_or(1)
}

pub(crate) fn open(
    application: &gtk::Application,
    parent: Option<&adw::ApplicationWindow>,
    request: Vim9EntryRequest,
) {
    let config = crate::config::load_config().0.vim9;
    open_with_config(application, parent, request, config);
}

fn open_with_config(
    application: &gtk::Application,
    parent: Option<&adw::ApplicationWindow>,
    request: Vim9EntryRequest,
    config: Vim9UiConfig,
) {
    open_with_config_and_exit(application, parent, request, config, None);
}

fn open_with_config_and_exit(
    application: &gtk::Application,
    parent: Option<&adw::ApplicationWindow>,
    request: Vim9EntryRequest,
    config: Vim9UiConfig,
    shell_exit: Option<Rc<Cell<Option<i32>>>>,
) {
    let session = match Vim9Session::start(request) {
        Ok(session) => session,
        Err(error) => {
            if let Some(result) = &shell_exit {
                result.set(Some(1));
                eprintln!("anvil: cannot start GUI Vim: {error}");
                application.quit();
            } else {
                open_error(application, parent, &error.to_string());
            }
            return;
        }
    };
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Vim9 IDE")
        .default_width(1240)
        .default_height(820)
        .build();
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }
    // Toolbar entry points launch a separate --ide process. This window's
    // lifetime therefore cannot be cut short by terminal-workspace shutdown.
    window.set_destroy_with_parent(false);

    let editor = vte4::Terminal::new();
    editor.set_hexpand(true);
    editor.set_vexpand(true);
    editor.set_mouse_autohide(true);
    editor.set_scroll_on_keystroke(true);
    editor.set_scrollback_lines(5000);
    editor.set_font(Some(&gtk::pango::FontDescription::from_string(
        "Monospace 12",
    )));
    editor.update_property(&[gtk::accessible::Property::Label("Vim9 editor")]);
    let foreground = gdk::RGBA::new(0.85, 0.88, 0.93, 1.0);
    let background = gdk::RGBA::new(0.07, 0.09, 0.12, 1.0);
    editor.set_colors(Some(&foreground), Some(&background), &[]);
    let native = native_buffer::NativeBuffer::new(
        &session,
        &editor,
        config.native_buffer,
        config.smooth_scroll,
    );

    // A wheel burst is coalesced until the next display frame. Stop that
    // pending burst before a toolbar/tab action or editing key can change the
    // buffer it belongs to; the widget must not overtake queued scroll input.
    let interaction = gtk::EventControllerLegacy::new();
    interaction.set_name(Some("vim9-window-scroll-stop"));
    interaction.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let surface = native.surface.downgrade();
        interaction.connect_event(move |_, event| {
            if matches!(
                event.event_type(),
                gdk::EventType::KeyPress | gdk::EventType::ButtonPress | gdk::EventType::TouchBegin
            ) {
                if let Some(surface) = surface.upgrade() {
                    surface.cancel_scroll();
                }
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(interaction);

    let message = label(if config.status_bar {
        "Starting Vim 9 and connecting the workbench…"
    } else {
        ""
    });
    // Connection errors and unsaved-close feedback must remain visible even
    // in the minimal editor. Empty informational chrome takes no space.
    message.set_visible(!message.text().is_empty());
    message.connect_label_notify(|message| message.set_visible(!message.text().is_empty()));
    message.set_margin_start(10);
    message.set_margin_end(10);
    message.set_margin_top(4);
    message.set_margin_bottom(4);
    message.set_wrap(true);
    message.set_ellipsize(gtk::pango::EllipsizeMode::None);
    let status = label("VIM9 · CONNECTING");
    status.set_margin_start(10);
    status.set_margin_end(10);
    status.set_margin_top(6);
    status.set_margin_bottom(6);
    status.add_css_class("caption");
    status.set_visible(config.status_bar);

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Vim9 IDE", &visible(&session.root().display().to_string()));
    header.set_title_widget(Some(&title));
    header.pack_end(&native.menu);
    header.pack_end(&native.badge);
    native.badge.set_visible(config.status_bar);
    let navigate_back = command_button(
        &session,
        &editor,
        &message,
        "go-previous-symbolic",
        "Navigate back (Alt+Left)",
        || Vim9Command::NavigateBack,
    );
    let navigate_forward = command_button(
        &session,
        &editor,
        &message,
        "go-next-symbolic",
        "Navigate forward (Alt+Right)",
        || Vim9Command::NavigateForward,
    );
    navigate_back.set_sensitive(false);
    navigate_forward.set_sensitive(false);
    header.pack_start(&navigate_back);
    header.pack_start(&navigate_forward);
    let new_file = button("document-new-symbolic", "New file");
    header.pack_start(&new_file);
    let open_file = button("document-open-symbolic", "Open file (Ctrl+Shift+O)");
    header.pack_start(&open_file);
    let quick_open = button(
        "system-search-symbolic",
        "Quick open project file (Ctrl+Shift+P)",
    );
    header.pack_start(&quick_open);
    {
        let weak = window.downgrade();
        let session = session.clone();
        let editor = editor.clone();
        let message = message.clone();
        let index = Rc::new(RefCell::new(None));
        quick_open.connect_clicked(move |_| {
            let Some(window) = weak.upgrade() else { return };
            if window.visible_dialog().is_some() {
                return;
            }
            let index = index
                .borrow_mut()
                .get_or_insert_with(|| {
                    Rc::new(RefCell::new(jterm_core::vim9::Vim9ProjectIndex::start(
                        session.root(),
                    )))
                })
                .clone();
            navigation::QuickOpen::show(&window, index, &session, &editor, &message);
        });
    }
    let save = command_button(
        &session,
        &editor,
        &message,
        "document-save-symbolic",
        "Save (Ctrl+S)",
        || Vim9Command::Save,
    );
    header.pack_start(&save);
    let save_all = command_button(
        &session,
        &editor,
        &message,
        "document-save-symbolic",
        "Save all (Ctrl+Shift+S)",
        || Vim9Command::SaveAll,
    );
    save_all.set_label("Save all");
    header.pack_start(&save_all);
    let previous = command_button(
        &session,
        &editor,
        &message,
        "go-up-symbolic",
        "Previous diagnostic (Shift+F8)",
        || Vim9Command::PreviousDiagnostic,
    );
    let next = command_button(
        &session,
        &editor,
        &message,
        "go-down-symbolic",
        "Next diagnostic (F8)",
        || Vim9Command::NextDiagnostic,
    );
    header.pack_end(&next);
    header.pack_end(&previous);

    let tools = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    tools.set_margin_start(8);
    tools.set_margin_end(8);
    tools.set_margin_top(4);
    tools.set_margin_bottom(4);
    let search = gtk::Entry::new();
    search.set_placeholder_text(Some("Search project text (Enter)"));
    search.set_hexpand(true);
    search.set_tooltip_text(Some("Search project text and populate Vim quickfix"));
    tools.append(&search);
    navigation::editor_escape(&search, &editor);
    let search_options = project_search::SearchOptions::new();
    tools.append(&search_options.menu);
    let run_search = button(
        "system-search-symbolic",
        "Search project using the selected options",
    );
    tools.append(&run_search);
    {
        let search = search.clone();
        run_search.connect_clicked(move |_| search.emit_activate());
    }
    let cancel_search = command_button(
        &session,
        &editor,
        &message,
        "process-stop-symbolic",
        "Cancel project search",
        || Vim9Command::CancelSearch,
    );
    cancel_search.set_sensitive(false);
    tools.append(&cancel_search);
    let go_to_line = button("go-jump-symbolic", "Go to line (Ctrl+Shift+G)");
    tools.append(&go_to_line);
    {
        let weak = window.downgrade();
        let session = session.clone();
        let editor = editor.clone();
        let message = message.clone();
        go_to_line.connect_clicked(move |_| {
            if let Some(window) = weak.upgrade() {
                if window.visible_dialog().is_none() {
                    navigation::go_to_line(&window, &session, &editor, &message);
                }
            }
        });
    }
    let mut language_actions = [
        (
            "definition",
            "go-jump-symbolic",
            "Go to definition",
            Vim9Command::Definition,
        ),
        (
            "hover",
            "dialog-information-symbolic",
            "Hover documentation",
            Vim9Command::Hover,
        ),
        (
            "references",
            "edit-find-symbolic",
            "Find references",
            Vim9Command::References,
        ),
        (
            "format",
            "format-justify-fill-symbolic",
            "Format buffer",
            Vim9Command::Format,
        ),
    ]
    .into_iter()
    .map(|(capability, icon, tooltip, command)| {
        let button = command_button(&session, &editor, &message, icon, tooltip, move || {
            command.clone()
        });
        button.set_sensitive(false);
        tools.append(&button);
        (capability, button, tooltip)
    })
    .collect::<Vec<_>>();
    let rename = button("document-edit-symbolic", "Rename symbol");
    rename.set_sensitive(false);
    tools.append(&rename);
    language_actions.push(("rename", rename.clone(), "Rename symbol"));
    {
        let weak = window.downgrade();
        let session = session.clone();
        let message = message.clone();
        let editor = editor.clone();
        rename.connect_clicked(move |_| {
            let Some(window) = weak.upgrade() else { return };
            let dialog = adw::AlertDialog::new(Some("Rename symbol"), None);
            dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
            dialog.set_default_response(Some("rename"));
            dialog.set_close_response("cancel");
            dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
            let name = gtk::Entry::new();
            name.set_placeholder_text(Some("New symbol name"));
            name.set_activates_default(true);
            name.set_margin_start(16);
            name.set_margin_end(16);
            name.set_margin_top(12);
            name.set_margin_bottom(12);
            dialog.set_extra_child(Some(&name));
            let session = session.clone();
            let message = message.clone();
            let editor = editor.clone();
            dialog.choose(Some(&window), None::<&gio::Cancellable>, move |response| {
                let value = name.text().to_string();
                if response == "rename" {
                    dispatch(
                        &session,
                        Vim9Command::Rename { name: value },
                        &message,
                        &editor,
                    );
                }
            });
        });
    }

    let project = gtk::Box::new(gtk::Orientation::Vertical, 4);
    project.set_width_request(230);
    let project_heading = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let project_label = label("PROJECT");
    project_label.add_css_class("heading");
    project_label.set_hexpand(true);
    project_heading.set_margin_start(8);
    project_heading.set_margin_end(4);
    project_heading.append(&project_label);
    let up = button("go-up-symbolic", "Parent folder");
    let refresh = button("view-refresh-symbolic", "Refresh project directory");
    project_heading.append(&up);
    project_heading.append(&refresh);
    let location = label(&visible(&session.root().display().to_string()));
    location.set_margin_start(8);
    location.set_margin_end(8);
    location.add_css_class("caption");
    let query = gtk::SearchEntry::new();
    query.set_placeholder_text(Some("Filter this folder"));
    query.set_margin_start(6);
    query.set_margin_end(6);
    let files = gtk::ListBox::new();
    files.set_selection_mode(gtk::SelectionMode::Single);
    files.add_css_class("navigation-sidebar");
    navigation::list_navigation(&query, &files);
    navigation::editor_escape(&query, &editor);
    let files_section = gtk::Box::new(gtk::Orientation::Vertical, 4);
    files_section.set_vexpand(true);
    files_section.set_visible(config.project_panel);
    files_section.append(&project_heading);
    files_section.append(&location);
    files_section.append(&query);
    files_section.append(&scroll(&files));
    project.append(&files_section);
    let browser = Rc::new(Browser {
        root: session.root().into(),
        directory: RefCell::new(session.root().into()),
        entries: RefCell::new(Vec::new()),
        shown: RefCell::new(Vec::new()),
        busy: Cell::new(false),
        list: files.clone(),
        query: query.clone(),
        location,
        up: up.clone(),
        message: message.clone(),
        session: session.clone(),
        editor: editor.clone(),
    });
    let weak = Rc::downgrade(&browser);
    files.connect_row_activated(move |_, row| {
        if let Some(browser) = weak.upgrade() {
            Browser::activate(&browser, row.index());
        }
    });
    let weak = Rc::downgrade(&browser);
    query.connect_search_changed(move |_| {
        if let Some(browser) = weak.upgrade() {
            browser.render();
        }
    });
    let weak = Rc::downgrade(&browser);
    query.connect_activate(move |_| {
        if let Some(browser) = weak.upgrade() {
            browser.render();
            let index = browser.list.selected_row().map_or(0, |row| row.index());
            Browser::activate(&browser, index);
        }
    });
    let weak = Rc::downgrade(&browser);
    up.connect_clicked(move |_| {
        if let Some(browser) = weak.upgrade() {
            let parent = browser.directory.borrow().parent().map(Path::to_path_buf);
            if let Some(parent) = parent {
                Browser::navigate(&browser, parent);
            }
        }
    });
    let weak = Rc::downgrade(&browser);
    refresh.connect_clicked(move |_| {
        if let Some(browser) = weak.upgrade() {
            let directory = browser.directory.borrow().clone();
            Browser::navigate(&browser, directory);
        }
    });

    let buffers = gtk::ListBox::new();
    buffers.set_selection_mode(gtk::SelectionMode::Single);
    buffers.add_css_class("navigation-sidebar");
    let buffer_ids: Rc<RefCell<Vec<i64>>> = Rc::new(RefCell::new(Vec::new()));
    let buffer_scroll = scroll(&buffers);
    buffer_scroll.set_vexpand(false);
    buffer_scroll.set_height_request(160);
    let buffers_heading = label("OPEN BUFFERS");
    buffers_heading.add_css_class("heading");
    buffers_heading.set_margin_start(8);
    buffers_heading.set_margin_top(8);
    let buffers_section = gtk::Box::new(gtk::Orientation::Vertical, 4);
    buffers_section.set_visible(config.buffer_tabs);
    buffers_section.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    buffers_section.append(&buffers_heading);
    project.append(&buffers_section);
    let buffer_controls = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    buffer_controls.set_margin_start(4);
    let previous_buffer = command_button(
        &session,
        &editor,
        &message,
        "go-previous-symbolic",
        "Previous buffer (Ctrl+Shift+PageUp)",
        || Vim9Command::PreviousBuffer,
    );
    let next_buffer = command_button(
        &session,
        &editor,
        &message,
        "go-next-symbolic",
        "Next buffer (Ctrl+Shift+PageDown)",
        || Vim9Command::NextBuffer,
    );
    let close_buffer = command_button(
        &session,
        &editor,
        &message,
        "window-close-symbolic",
        "Close current buffer (Ctrl+Shift+W)",
        || Vim9Command::CloseCurrentBuffer,
    );
    let split_vertical = command_button(
        &session,
        &editor,
        &message,
        "view-dual-symbolic",
        "Split editor vertically",
        || Vim9Command::Split { vertical: true },
    );
    let split_horizontal = command_button(
        &session,
        &editor,
        &message,
        "view-more-symbolic",
        "Split editor horizontally",
        || Vim9Command::Split { vertical: false },
    );
    for button in [
        &previous_buffer,
        &next_buffer,
        &close_buffer,
        &split_vertical,
        &split_horizontal,
    ] {
        buffer_controls.append(button);
    }
    buffers_section.append(&buffer_controls);
    buffers_section.append(&buffer_scroll);
    project.set_visible(config.project_panel || config.buffer_tabs);
    {
        let session = session.clone();
        let ids = buffer_ids.clone();
        let message = message.clone();
        let editor = editor.clone();
        buffers.connect_row_activated(move |_, row| {
            if let Some(buffer) = ids.borrow().get(row.index() as usize) {
                dispatch(
                    &session,
                    Vim9Command::Buffer { buffer: *buffer },
                    &message,
                    &editor,
                );
            }
        });
    }

    let problems = gtk::ListBox::new();
    problems.set_selection_mode(gtk::SelectionMode::Single);
    let diagnostic_targets = Rc::new(RefCell::new(Vec::<(String, u64, u64)>::new()));
    let problems_title = label("PROBLEMS · 0");
    problems_title.add_css_class("heading");
    problems_title.set_margin_start(8);
    problems_title.set_margin_top(8);
    problems_title.set_margin_bottom(4);
    let problem_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    problem_box.append(&problems_title);
    problem_box.append(&scroll(&problems));
    let build_output = gtk::TextView::new();
    build_output.set_editable(false);
    build_output.set_cursor_visible(false);
    build_output.set_monospace(true);
    build_output.set_wrap_mode(gtk::WrapMode::WordChar);
    build_output.set_left_margin(10);
    build_output.set_right_margin(10);
    build_output.set_top_margin(8);
    build_output.update_property(&[gtk::accessible::Property::Label("Build output")]);
    let output_note = label("");
    output_note.add_css_class("caption");
    output_note.set_margin_start(10);
    output_note.set_margin_end(10);
    output_note.set_margin_top(6);
    output_note.set_visible(false);
    let build_output_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    build_output_box.append(&output_note);
    build_output_box.append(&scroll(&build_output));
    let output_stack = gtk::Stack::new();
    output_stack.add_titled(&problem_box, Some("problems"), "Problems");
    output_stack.add_titled(&build_output_box, Some("output"), "Build output");
    output_stack.set_vexpand(true);
    let output_tabs = gtk::StackSwitcher::new();
    output_tabs.set_stack(Some(&output_stack));
    output_tabs.set_halign(gtk::Align::Start);
    output_tabs.set_margin_start(8);
    output_tabs.set_margin_top(4);
    let output_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    output_box.append(&output_tabs);
    output_box.append(&output_stack);
    output_box.set_visible(config.tool_panel);
    let editor_split = gtk::Paned::new(gtk::Orientation::Vertical);
    editor_split.set_start_child(Some(&native.widget));
    editor_split.set_end_child(Some(&output_box));
    editor_split.set_resize_start_child(true);
    editor_split.set_shrink_start_child(false);
    editor_split.set_resize_end_child(false);
    editor_split.set_shrink_end_child(true);
    editor_split.set_position(470);
    let main_split = gtk::Paned::new(gtk::Orientation::Horizontal);
    main_split.set_start_child(Some(&project));
    main_split.set_end_child(Some(&editor_split));
    main_split.set_resize_start_child(false);
    main_split.set_shrink_start_child(true);
    main_split.set_resize_end_child(true);
    main_split.set_shrink_end_child(false);
    main_split.set_position(260);
    main_split.set_vexpand(true);
    let toggle_project = button("view-sidebar-symbolic", "Show or hide project panel");
    {
        let project = project.clone();
        let files_section = files_section.clone();
        let buffers_section = buffers_section.clone();
        let browser = browser.clone();
        let editor = editor.clone();
        toggle_project.connect_clicked(move |_| {
            let show = !files_section.is_visible();
            files_section.set_visible(show);
            project.set_visible(show || buffers_section.is_visible());
            if show && browser.entries.borrow().is_empty() {
                let directory = browser.directory.borrow().clone();
                Browser::navigate(&browser, directory);
            }
            editor.grab_focus();
        });
    }
    header.pack_end(&toggle_project);
    let toggle_output = button("view-list-symbolic", "Show or hide output panel");
    {
        let output_box = output_box.clone();
        let editor = editor.clone();
        toggle_output.connect_clicked(move |_| {
            output_box.set_visible(!output_box.is_visible());
            editor.grab_focus();
        });
    }
    header.pack_end(&toggle_output);
    {
        let session = session.clone();
        let targets = diagnostic_targets.clone();
        let message = message.clone();
        let editor = editor.clone();
        problems.connect_row_activated(move |_, row| {
            if let Some((path, line, column)) = targets.borrow().get(row.index() as usize).cloned()
            {
                dispatch(
                    &session,
                    Vim9Command::Open {
                        path,
                        line: Some(line.max(1)),
                        column: Some(column.max(1)),
                    },
                    &message,
                    &editor,
                );
            }
        });
    }

    let build_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    build_bar.set_margin_start(8);
    build_bar.set_margin_end(8);
    build_bar.set_margin_top(5);
    build_bar.set_margin_bottom(5);
    let build_command = gtk::Entry::new();
    build_command.set_hexpand(true);
    build_command.set_text(default_build(session.root()));
    build_command.set_placeholder_text(Some("Build command"));
    build_command.set_tooltip_text(Some(
        "Shell command run in this project only when you press Build or F5",
    ));
    let build = gtk::Button::with_label("Build · F5");
    build.set_tooltip_text(Some(
        "Run the visible command using files already saved on disk",
    ));
    let save_and_build = gtk::Button::with_label("Save all + Build");
    save_and_build.set_tooltip_text(Some(
        "Save every modified buffer, then build only if all saves succeed",
    ));
    let cancel_build = command_button(
        &session,
        &editor,
        &message,
        "process-stop-symbolic",
        "Cancel build",
        || Vim9Command::CancelBuild,
    );
    cancel_build.set_sensitive(false);
    let build_state = label("Ready");
    build_state.set_width_chars(18);
    build_bar.append(&label("BUILD"));
    build_bar.append(&build_command);
    build_bar.append(&build);
    build_bar.append(&save_and_build);
    build_bar.append(&cancel_build);
    build_bar.append(&build_state);
    {
        let session = session.clone();
        let command = build_command.clone();
        let message = message.clone();
        let editor = editor.clone();
        let output_stack = output_stack.clone();
        let output_box = output_box.clone();
        build.connect_clicked(move |_| {
            output_box.set_visible(true);
            output_stack.set_visible_child_name("output");
            dispatch(
                &session,
                Vim9Command::Build {
                    command: command.text().into(),
                },
                &message,
                &editor,
            );
        });
    }
    {
        let session = session.clone();
        let command = build_command.clone();
        let message = message.clone();
        let editor = editor.clone();
        let output_stack = output_stack.clone();
        let output_box = output_box.clone();
        save_and_build.connect_clicked(move |_| {
            output_box.set_visible(true);
            output_stack.set_visible_child_name("output");
            dispatch(
                &session,
                Vim9Command::SaveAndBuild {
                    command: command.text().into(),
                },
                &message,
                &editor,
            );
        });
    }
    navigation::editor_escape(&build_command, &editor);
    {
        let build = build.clone();
        build_command.connect_activate(move |_| {
            build.emit_clicked();
        });
    }
    {
        let session = session.clone();
        let message = message.clone();
        let editor = editor.clone();
        let output_stack = output_stack.clone();
        let output_box = output_box.clone();
        let search_options = search_options.clone();
        search.connect_activate(move |entry| {
            output_box.set_visible(true);
            output_stack.set_visible_child_name("problems");
            dispatch(
                &session,
                Vim9Command::SearchWithOptions {
                    query: entry.text().into(),
                    options: search_options.options(),
                },
                &message,
                &editor,
            )
        });
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    tools.set_visible(config.tool_panel);
    build_bar.set_visible(config.tool_panel);
    for button in [
        &navigate_back,
        &navigate_forward,
        &new_file,
        &open_file,
        &quick_open,
        &save,
        &save_all,
        &previous,
        &next,
        &toggle_project,
        &toggle_output,
    ] {
        button.set_visible(config.toolbar);
    }
    let tools_separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    tools_separator.set_visible(config.tool_panel);
    let build_separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    build_separator.set_visible(config.tool_panel);
    content.append(&tools);
    content.append(&tools_separator);
    content.append(&main_split);
    content.append(&build_separator);
    content.append(&build_bar);
    let search_feedback = label("");
    search_feedback.set_margin_start(10);
    search_feedback.set_margin_end(10);
    search_feedback.set_wrap(true);
    search_feedback.add_css_class("caption");
    search_feedback.set_visible(false);
    content.append(&search_feedback);
    content.append(&message);
    content.append(&status);
    window.set_content(Some(&content));

    {
        let weak = window.downgrade();
        let session = session.clone();
        let message = message.clone();
        let editor = editor.clone();
        new_file.connect_clicked(move |_| {
            let Some(window) = weak.upgrade() else { return };
            let chooser = gtk::FileDialog::builder()
                .title("Choose a name for the new Vim file")
                .initial_folder(&gio::File::for_path(session.root()))
                .initial_name("untitled.txt")
                .modal(true)
                .build();
            let session = session.clone();
            let message = message.clone();
            let editor = editor.clone();
            chooser.save(
                Some(&window),
                None::<&gio::Cancellable>,
                move |result| match result {
                    Ok(file) => {
                        if let Some(path) = file
                            .path()
                            .and_then(|path| path.to_str().map(str::to_owned))
                        {
                            dispatch(
                                &session,
                                Vim9Command::Open {
                                    path,
                                    line: None,
                                    column: None,
                                },
                                &message,
                                &editor,
                            );
                        } else {
                            message
                                .set_label("Choose a local UTF-8 filename for this Vim9 project.");
                        }
                    }
                    Err(error)
                        if error.matches(gtk::DialogError::Dismissed)
                            || error.matches(gtk::DialogError::Cancelled) => {}
                    Err(error) => message.set_label(&visible(&error.to_string())),
                },
            );
        });
    }

    {
        let weak = window.downgrade();
        let session = session.clone();
        let message = message.clone();
        let editor = editor.clone();
        open_file.connect_clicked(move |_| {
            let Some(window) = weak.upgrade() else { return };
            let chooser = gtk::FileDialog::builder()
                .title("Open file in Vim9")
                .initial_folder(&gio::File::for_path(session.root()))
                .modal(true)
                .build();
            let session = session.clone();
            let message = message.clone();
            let editor = editor.clone();
            chooser.open(
                Some(&window),
                None::<&gio::Cancellable>,
                move |result| match result {
                    Ok(file) => {
                        if let Some(path) = file.path() {
                            if let Some(path) = path.to_str() {
                                dispatch(
                                    &session,
                                    Vim9Command::Open {
                                        path: path.into(),
                                        line: None,
                                        column: None,
                                    },
                                    &message,
                                    &editor,
                                );
                            } else {
                                message.set_label("Vim9 IDE currently requires UTF-8 file paths.");
                            }
                        } else {
                            message.set_label("Choose a local file for this Vim9 project.");
                        }
                    }
                    Err(error)
                        if error.matches(gtk::DialogError::Dismissed)
                            || error.matches(gtk::DialogError::Cancelled) => {}
                    Err(error) => message.set_label(&visible(&error.to_string())),
                },
            );
        });
    }

    // Editor-specific shortcuts live only on this window. All other keys,
    // including Escape/Ctrl+[ and insert-mode input, pass directly to Vim.
    let keys = gtk::EventControllerKey::new();
    keys.set_name(Some("vim9-workbench-shortcuts"));
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let session = session.clone();
        let message = message.clone();
        let editor = editor.clone();
        let quick_open = quick_open.clone();
        let go_to_line = go_to_line.clone();
        let search = search.clone();
        let build_command = build_command.clone();
        let open_file = open_file.clone();
        let build = build.clone();
        let close_buffer = close_buffer.clone();
        let native_surface = native.surface.clone();
        let weak = window.downgrade();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            // Composition keys belong to the native input method until its
            // preedit is committed or cancelled, including Escape and Enter.
            if native_surface.is_preediting() {
                return glib::Propagation::Proceed;
            }
            // Let the active native dialog own its text and shortcut handling.
            if weak.upgrade().is_some_and(|window| {
                window.visible_dialog().is_some() || navigation::native_text_has_focus(&window)
            }) {
                return glib::Propagation::Proceed;
            }
            let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            let alt =
                modifiers.intersects(gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK);
            // Navigation history belongs to the Vim grid. In particular,
            // Alt+Left in a native field or an options popover remains native.
            if (key == gdk::Key::Left || key == gdk::Key::Right)
                && modifiers.contains(gdk::ModifierType::ALT_MASK)
                && !modifiers.intersects(
                    gdk::ModifierType::CONTROL_MASK
                        | gdk::ModifierType::SHIFT_MASK
                        | gdk::ModifierType::SUPER_MASK
                        | gdk::ModifierType::META_MASK
                        | gdk::ModifierType::HYPER_MASK,
                )
                && editor.has_focus()
                && config.toolbar
            {
                dispatch(
                    &session,
                    if key == gdk::Key::Left {
                        Vim9Command::NavigateBack
                    } else {
                        Vim9Command::NavigateForward
                    },
                    &message,
                    &editor,
                );
                return glib::Propagation::Stop;
            }
            // Hidden workbench controls never intercept Vim mappings or
            // focus an invisible field. Saving remains an editor action.
            let command = match key.to_unicode().map(|value| value.to_ascii_lowercase()) {
                Some('s') if ctrl && !alt => Some(if shift {
                    Vim9Command::SaveAll
                } else {
                    Vim9Command::Save
                }),
                Some('p') if ctrl && shift && !alt && config.toolbar => {
                    if quick_open.is_sensitive() {
                        quick_open.emit_clicked();
                    }
                    return glib::Propagation::Stop;
                }
                Some('g') if ctrl && shift && !alt && (config.toolbar || config.tool_panel) => {
                    if go_to_line.is_sensitive() {
                        go_to_line.emit_clicked();
                    }
                    return glib::Propagation::Stop;
                }
                Some('f') if ctrl && shift && !alt && config.tool_panel => {
                    search.grab_focus();
                    search.select_region(0, -1);
                    return glib::Propagation::Stop;
                }
                Some('b') if ctrl && shift && !alt && config.tool_panel => {
                    build_command.grab_focus();
                    build_command.select_region(0, -1);
                    return glib::Propagation::Stop;
                }
                Some('w') if ctrl && shift && !alt && config.buffer_tabs => {
                    if close_buffer.is_sensitive() {
                        close_buffer.emit_clicked();
                    }
                    return glib::Propagation::Stop;
                }
                Some('o') if ctrl && shift && !alt && config.toolbar => {
                    open_file.emit_clicked();
                    return glib::Propagation::Stop;
                }
                _ if key == gdk::Key::F5 && !ctrl && !alt && !shift && config.tool_panel => {
                    if build.is_sensitive() {
                        build.emit_clicked();
                    }
                    return glib::Propagation::Stop;
                }
                _ if key == gdk::Key::F8 && !ctrl && !alt && config.tool_panel => Some(if shift {
                    Vim9Command::PreviousDiagnostic
                } else {
                    Vim9Command::NextDiagnostic
                }),
                _ if key == gdk::Key::Page_Up && ctrl && shift && !alt && config.buffer_tabs => {
                    Some(Vim9Command::PreviousBuffer)
                }
                _ if key == gdk::Key::Page_Down && ctrl && shift && !alt && config.buffer_tabs => {
                    Some(Vim9Command::NextBuffer)
                }
                _ => None,
            };
            if let Some(command) = command {
                dispatch(&session, command, &message, &editor);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(keys);

    let destroy_session = session.clone();
    window.connect_destroy(move |_| destroy_session.revoke());
    let child_finished = Rc::new(Cell::new(false));
    let closing = Rc::new(Cell::new(false));
    {
        let finished = child_finished.clone();
        let closing = closing.clone();
        let session = session.clone();
        let editor = editor.clone();
        let message = message.clone();
        window.connect_close_request(move |_| {
            if finished.get() { return glib::Propagation::Proceed; }
            closing.set(true);
            dispatch(&session, Vim9Command::Quit, &message, &editor);
            message.set_label("Close requested. Vim keeps modified buffers open: save them, or use :qa! in Vim to discard them explicitly.");
            glib::Propagation::Stop
        });
    }
    {
        let finished = child_finished.clone();
        let closing = closing.clone();
        let weak = window.downgrade();
        let message = message.clone();
        let exited_session = session.clone();
        let shell_exit = shell_exit.clone();
        editor.connect_child_exited(move |_, status| {
            let Some(code) = jterm_core::process::exit_code_from_wait_status(status) else {
                return;
            };
            exited_session.revoke();
            finished.set(true);
            if let Some(result) = &shell_exit {
                result.set(Some(code));
            }
            if shell_exit.is_some() || closing.get() || status == 0 {
                if let Some(window) = weak.upgrade() {
                    window.close();
                }
            } else {
                message.set_label(&format!(
                    "Vim exited (status {status}). You can close this window and reopen Vim9 IDE."
                ));
            }
        });
    }

    let launch = session.launch();
    let mut argv = vec![launch.program.clone()];
    argv.extend(launch.args.clone());
    let extra = launch
        .env
        .iter()
        .filter_map(|item| item.split_once('='))
        .collect::<Vec<_>>();
    let env = match jterm_core::child_env::vte_envv_from_captured(
        &jterm_core::child_env::ChildEnv::from_identity(),
        &extra,
    ) {
        Ok(env) => env,
        Err(error) => {
            session.revoke();
            child_finished.set(true);
            if let Some(result) = &shell_exit {
                result.set(Some(1));
                eprintln!("anvil: cannot prepare Vim environment: {error}");
                window.close();
                return;
            }
            message.set_label(&format!(
                "Cannot prepare Vim environment: {}",
                visible(&error.to_string())
            ));
            window.present();
            return;
        }
    };
    let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let env = env.iter().map(String::as_str).collect::<Vec<_>>();
    let flags = glib::SpawnFlags::SEARCH_PATH
        | glib::SpawnFlags::from_bits_retain(jterm_core::child_env::VTE_SPAWN_NO_PARENT_ENVV_BITS);
    {
        let finished = child_finished.clone();
        let message = message.clone();
        let failed_session = session.clone();
        let shell_exit = shell_exit.clone();
        let weak_window = window.downgrade();
        editor.spawn_async(
            vte4::PtyFlags::DEFAULT,
            launch.cwd.to_str(),
            &argv,
            &env,
            flags,
            || {},
            15_000,
            None::<&gio::Cancellable>,
            move |result| {
                if let Err(error) = result {
                    failed_session.revoke();
                    finished.set(true);
                    if let Some(result) = &shell_exit {
                        result.set(Some(1));
                        eprintln!("anvil: cannot start Vim: {error}");
                        if let Some(window) = weak_window.upgrade() {
                            window.close();
                        }
                        return;
                    }
                    message.set_label(&format!(
                        "Cannot start Vim: {}",
                        visible(&error.to_string())
                    ));
                }
            },
        );
    }

    if config.project_panel {
        Browser::navigate(&browser, session.root().into());
    }
    let weak_window = window.downgrade();
    let mut old_buffers = Vec::new();
    let mut old_diagnostics = Vec::new();
    let mut old_output = Vec::new();
    let mut last_error = None;
    let mut was_connected = false;
    let connected_controls = [
        new_file,
        open_file,
        quick_open,
        go_to_line,
        save,
        save_all,
        previous,
        next,
        previous_buffer,
        next_buffer,
        close_buffer,
        split_vertical,
        split_horizontal,
        run_search,
    ];
    for button in &connected_controls {
        button.set_sensitive(false);
    }
    let poll_editor = editor.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(window) = weak_window.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // Keep the directory browser alive only as long as this workbench.
        let _ = &browser;
        let snapshot = session.snapshot();
        let connected = snapshot.connected && !child_finished.get();
        for button in &connected_controls {
            button.set_sensitive(connected);
        }
        search.set_sensitive(connected);
        search_options.menu.set_sensitive(connected);
        cancel_search.set_sensitive(connected && snapshot.search.running);
        navigate_back.set_sensitive(connected && snapshot.navigation.can_go_back);
        navigate_forward.set_sensitive(connected && snapshot.navigation.can_go_forward);
        build.set_sensitive(connected && !snapshot.build.running);
        save_and_build.set_sensitive(connected && !snapshot.build.running);
        cancel_build.set_sensitive(connected && snapshot.build.running);
        for (capability, button, tooltip) in &language_actions {
            let supported = snapshot
                .capabilities
                .iter()
                .any(|available| available == capability);
            button.set_sensitive(connected && supported);
            if supported {
                button.set_tooltip_text(Some(tooltip));
            } else {
                button.set_tooltip_text(Some(
                    "Requires a configured Vim language server plugin providing this action",
                ));
            }
        }
        if connected && !was_connected {
            if closing.get() {
                dispatch(&session, Vim9Command::Quit, &message, &poll_editor);
            } else {
                message.set_label(if config.status_bar {
                    "Vim9 connected · your Vim configuration and native keybindings are active."
                } else {
                    ""
                });
            }
        }
        was_connected = connected;
        let file = if snapshot.file.is_empty() {
            "[No Name]"
        } else {
            &snapshot.file
        };
        let modified = if snapshot.modified { " ●" } else { "" };
        status.set_label(&format!(
            "{} · {}{} · Ln {}/{}, Col {} · {} · {} · {} windows · {}{} buffers · {}",
            visible(&snapshot.mode).to_uppercase(),
            visible(file),
            modified,
            snapshot.line,
            snapshot.line_count,
            snapshot.column,
            if snapshot.filetype.is_empty() {
                "plain text".into()
            } else {
                visible(&snapshot.filetype)
            },
            if snapshot.encoding.is_empty() {
                "UTF-8".into()
            } else {
                visible(&snapshot.encoding)
            },
            snapshot.window_count,
            snapshot.buffers.len(),
            if snapshot.buffers_truncated { "+" } else { "" },
            if connected && snapshot.waiting_for_input {
                "SAVE WAITING FOR VIM INPUT"
            } else if connected {
                "CONNECTED"
            } else {
                "DISCONNECTED"
            }
        ));
        title.set_title(&format!(
            "{}{} — Vim9 IDE",
            visible(
                Path::new(file)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(file)
            ),
            modified
        ));
        window.set_title(Some(&format!("{}{} — Vim9 IDE", visible(file), modified)));
        if snapshot.error != last_error {
            if let Some(error) = &snapshot.error {
                message.set_label(&visible(error));
            }
            last_error = snapshot.error.clone();
        }
        search_feedback.set_label(&project_search::feedback(&snapshot.search));
        search_feedback.set_visible(config.tool_panel && !snapshot.search.query.is_empty());
        build_state.set_label(&if snapshot.build.running {
            "Running…".into()
        } else if snapshot.build.cancelled {
            "Cancelled".into()
        } else if let Some(code) = snapshot.build.exit_code {
            format!("Exited: {code}")
        } else {
            "Ready".into()
        });
        output_note.set_label(&format!(
            "Showing the latest output · {} output lines omitted",
            snapshot.build.truncated
        ));
        output_note.set_visible(snapshot.build.truncated > 0);
        if snapshot.build.output != old_output {
            build_output
                .buffer()
                .set_text(&jterm_core::review_input::safe_multiline_display(
                    &snapshot.build.output.join("\n"),
                    128 * 1024,
                ));
            old_output = snapshot.build.output.clone();
        }
        buffers_heading.set_label(&format!(
            "OPEN BUFFERS · {}{}",
            snapshot.buffers.len(),
            if snapshot.buffers_truncated { "+" } else { "" }
        ));
        let buffer_rows = snapshot
            .buffers
            .iter()
            .map(|b| (b.id, b.name.clone(), b.modified, b.current))
            .collect::<Vec<_>>();
        if buffer_rows != old_buffers {
            buffers.remove_all();
            buffer_ids.borrow_mut().clear();
            for buffer in &snapshot.buffers {
                let name = if buffer.name.is_empty() {
                    "[No Name]"
                } else {
                    &buffer.name
                };
                let row = label(&format!(
                    "{}{} {}",
                    if buffer.current { "▸" } else { " " },
                    if buffer.modified { "●" } else { " " },
                    visible(name)
                ));
                row.set_margin_start(8);
                row.set_margin_end(8);
                row.set_margin_top(5);
                row.set_margin_bottom(5);
                row.set_tooltip_text(Some(&visible(name)));
                buffers.append(&row);
                buffer_ids.borrow_mut().push(buffer.id);
            }
            old_buffers = buffer_rows;
        }
        let diagnostic_rows = snapshot
            .diagnostics
            .iter()
            .map(|d| {
                (
                    d.filename.clone(),
                    d.line,
                    d.column,
                    d.text.clone(),
                    d.kind.clone(),
                )
            })
            .collect::<Vec<_>>();
        problems_title.set_label(&format!(
            "PROBLEMS · {}{} · Quickfix: {}",
            snapshot.diagnostics.len(),
            if snapshot.diagnostics_truncated {
                "+"
            } else {
                ""
            },
            match snapshot.quickfix.source.as_str() {
                "search" => "Search",
                "build" => "Build",
                _ => "Vim",
            }
        ));
        problems_title.set_tooltip_text(Some(&format!(
            "{} · quickfix item {} of {}. Vim may also supply location-list or language-server diagnostics.",
            visible(&snapshot.quickfix.title),
            snapshot.quickfix.index,
            snapshot.quickfix.count
        )));
        if diagnostic_rows != old_diagnostics {
            problems.remove_all();
            diagnostic_targets.borrow_mut().clear();
            for diagnostic in &snapshot.diagnostics {
                let row = label(&format!(
                    "{}  {}:{}:{}  {}",
                    visible(&diagnostic.kind),
                    visible(&diagnostic.filename),
                    diagnostic.line,
                    diagnostic.column,
                    visible(&diagnostic.text)
                ));
                row.set_margin_start(8);
                row.set_margin_end(8);
                row.set_margin_top(4);
                row.set_margin_bottom(4);
                row.set_tooltip_text(Some(&visible(&diagnostic.text)));
                problems.append(&row);
                diagnostic_targets.borrow_mut().push((
                    diagnostic.filename.clone(),
                    diagnostic.line,
                    diagnostic.column,
                ));
            }
            old_diagnostics = diagnostic_rows;
        }
        glib::ControlFlow::Continue
    });
    // The poll owns the browser until this workbench is destroyed.
    window.present();
    editor.grab_focus();
}

#[cfg(test)]
#[path = "vim9_ide_native_tests.rs"]
mod native_tests;
