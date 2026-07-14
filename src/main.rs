#![allow(dead_code)]

mod action_ops;
mod agent;
mod agent_ops;
mod ai;
mod ai_palette_ops;
mod app_msg;
mod block_view;
mod cli;
mod command_history;
mod config;
mod config_ops;
mod dialogs;
mod file_tree;
mod file_tree_ops;
mod git_meta;
mod keybindings;
mod navigation_ui;
mod notebook;
mod notify;
mod palette;
mod parser;
mod process;
mod pty;
mod search;
mod session;
mod settings_ops;
mod sidebar;
mod sidebar_toggle;
mod startup_ui;
mod tab_strip;
mod terminal;
mod top_bar;
mod vte_pty;
mod workflow_ops;
mod workflows;
mod workspace;
mod workspace_ops;

use adw::prelude::*;
use gtk::gdk::ModifierType;
use gtk::gio::{self, Cancellable};
use gtk::glib;
use relm4::adw;
use relm4::factory::FactoryVecDeque;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use app_msg::AppMsg;
use config::{choose_shell_argv, config_file_path, load_config, Config, TerminalMode, Theme};
use keybindings::{normalize_key, Action, Direction, KeyCombo, KeybindingMap};
use terminal::{default_tab_title, BlockTerminal, VteInit, VteInput, VteOutput, VteTerminal};
use workspace::{ConnStatus, Pane, RemoteConn, Tab, TermCtl, ZoomState};

const FONT_STEP: f64 = 0.025;
const OPACITY_STEP: f64 = 0.025;
const MIN_TAB_WIDTH: u32 = 80;
const MAX_TAB_WIDTH: u32 = 480;

// `file_tree_store: gtk::TreeStore` uses the GTK4 TreeStore family deprecated in
// 4.10; it stays functional and a ColumnView rewrite is out of scope.
#[allow(deprecated)]
struct AppModel {
    config: Rc<RefCell<Config>>,
    themes: Rc<Vec<Theme>>,
    kbmap: Rc<RefCell<KeybindingMap>>,
    shell_argv: Rc<Vec<String>>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    next_pane_id: u64,
    sidebar_visible: bool,
    font_scale: f64,
    window_opacity: f64,
    stack: gtk::Stack,
    tab_strip: gtk::Box,
    tab_rows: FactoryVecDeque<tab_strip::TabRow>,
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    quit_allowed: Rc<std::cell::Cell<bool>>,
    session_persistence: bool,
    dyn_css: gtk::CssProvider,
    search: Controller<search::SearchModel>,
    tab_filter_control: Controller<sidebar::TabFilterModel>,
    tab_filter: String,
    file_tree_store: gtk::TreeStore,
    file_header: Controller<sidebar::FileHeaderModel>,
    file_tree_root: Rc<RefCell<std::path::PathBuf>>,
    tab_strip_scroll: gtk::ScrolledWindow,
    top_tab_scroll: gtk::ScrolledWindow,
    top_bar: Controller<top_bar::TopBarModel>,
    sidebar_box: gtk::Box,
    sidebar_stack: gtk::Stack,
    sidebar_toggle: Controller<sidebar_toggle::SidebarToggleModel>,
    tab_placement: std::cell::Cell<config::TabPlacement>,
    sidebar_view: std::cell::Cell<config::SidebarView>,
    command_palette: Controller<dialogs::command_palette::PaletteModel>,
    settings: Controller<dialogs::settings::SettingsModel>,
    settings_font_names: Rc<Vec<String>>,
    remote_picker: Controller<dialogs::remote_picker::RemotePickerModel>,
    debug_dashboard: Controller<dialogs::debug_dashboard::DebugDashboardModel>,
    history: Controller<dialogs::history::HistoryModel>,
    workflow_dialog: Controller<dialogs::workflow::WorkflowModel>,
    ai_panel: Controller<dialogs::ai_panel::AiPanelModel>,
    notebook: Controller<notebook::NotebookModel>,
    /// Workflows loaded from disk. Refreshed on demand each time the palette
    /// is opened (cheap — handful of small YAML files) so users see edits
    /// without a restart.
    workflows: Rc<RefCell<Vec<workflows::Workflow>>>,
    /// At most one agent session is active per app. Opening the panel
    /// while another session is alive cancels the previous one.
    active_agent: Rc<RefCell<Option<agent::AgentSession>>>,
    agent_panel: Controller<agent::AgentPanelModel>,
    agent_edit: Controller<agent::AgentEditModel>,
}

#[allow(clippy::too_many_arguments)]
fn create_pane(
    config: &Rc<RefCell<Config>>,
    shell_argv: &Rc<Vec<String>>,
    tab_id: u64,
    pane_id: u64,
    mode: TerminalMode,
    initial_commands: Option<String>,
    working_directory: Option<String>,
    sender: &ComponentSender<AppModel>,
) -> Pane {
    let probe = terminal::PaneProbe::default();
    // -1 means "no PTY yet"; foreground probing skips it (0 would alias stdin).
    probe.pty_fd.set(-1);
    let init = VteInit {
        config: config.clone(),
        shell_argv: shell_argv.clone(),
        working_directory: working_directory.clone(),
        session_id: None,
        initial_commands,
        probe: probe.clone(),
    };
    let forward = move |out| match out {
        VteOutput::Exited(code) => AppMsg::PaneExited(tab_id, pane_id, code),
        VteOutput::CwdChanged(p) => AppMsg::PaneCwdChanged(tab_id, pane_id, p),
        VteOutput::TitleChanged(t) => AppMsg::TitleChanged(tab_id, t),
        VteOutput::Bell => AppMsg::Bell(tab_id),
        VteOutput::Activity => AppMsg::Activity(tab_id),
        VteOutput::Focused => AppMsg::PaneFocused(tab_id, pane_id),
        // Slow command finished while unattended: failure draws the bell
        // (attention) style, success the lighter activity style.
        VteOutput::CommandFinished(true) => AppMsg::Activity(tab_id),
        VteOutput::CommandFinished(false) => AppMsg::Bell(tab_id),
        VteOutput::RemoteSessionId(id) => AppMsg::PaneRemoteSessionId(tab_id, id),
        VteOutput::BlockFinished {
            command,
            exit_code,
            output_sample,
        } => AppMsg::AgentBlockFinished {
            tab_id,
            pane_id,
            command,
            exit_code,
            output_sample,
        },
    };
    let terminal = match mode {
        TerminalMode::Block => TermCtl::Block(
            BlockTerminal::builder()
                .launch(init)
                .forward(sender.input_sender(), forward),
        ),
        TerminalMode::Vte => TermCtl::Vte(
            VteTerminal::builder()
                .launch(init)
                .forward(sender.input_sender(), forward),
        ),
    };
    Pane {
        terminal,
        id: pane_id,
        cwd: working_directory,
        mode,
        probe,
    }
}

impl AppModel {
    fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn active_terminal(&self) -> Option<&TermCtl> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .map(|p| &p.terminal)
    }

    /// Working directory of the active pane, if it reports one. Remote (ssh)
    /// tabs return None: their cwd is a path on the remote filesystem and the
    /// file tree must not follow it (even if a same-named dir exists locally,
    /// it's a different machine).
    fn active_cwd(&self) -> Option<std::path::PathBuf> {
        let tab = self.tabs.get(self.active)?;
        if tab.remote.is_some() {
            return None;
        }
        tab.panes
            .get(tab.active_pane)
            .and_then(|p| p.cwd.clone())
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
    }
}

#[relm4::component]
impl SimpleComponent for AppModel {
    type Init = cli::LaunchOptions;
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("jterm1"),
            set_default_width: 800,
            set_default_height: 600,

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                #[wrap(Some)]
                set_child = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    #[local_ref]
                    top_bar -> gtk::Overlay {},

                    #[local_ref]
                    search_bar -> gtk::SearchBar {},

                    #[local_ref]
                    content_paned -> gtk::Paned {
                        set_vexpand: true,
                    },
                },
            }
        }
    }

    #[allow(deprecated)]
    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let config_warning = config::config_file_error();
        let (mut config, themes, kbmap) = load_config();
        if let Some(mode) = init.mode {
            config.terminal_mode = match mode {
                cli::Mode::Block => TerminalMode::Block,
                cli::Mode::Vte => TerminalMode::Vte,
            };
        }
        let shell_argv = Rc::new(choose_shell_argv(config.shell.as_deref()));
        let startup = config.startup_commands.clone();
        let requested_cwd = init
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let execute_argv = init.execute.clone().map(Rc::new);
        let restore_session =
            !init.no_restore && init.working_directory.is_none() && init.execute.is_none();
        let session_persistence = init.execute.is_none();
        let window_opacity = config.window_opacity;
        let font_scale = config.default_font_scale;
        let config = Rc::new(RefCell::new(config));
        let kbmap = Rc::new(RefCell::new(kbmap));
        let workflows = Rc::new(RefCell::new(Vec::new()));

        root.set_opacity(window_opacity);

        startup_ui::install_static_css();
        let dyn_css = startup_ui::install_dynamic_css_provider();

        let stack = gtk::Stack::new();
        let tab_strip = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let search =
            search::SearchModel::builder()
                .launch(())
                .forward(sender.input_sender(), |output| match output {
                    search::SearchOutput::Changed(query) => AppMsg::SearchChanged(query),
                    search::SearchOutput::Next => AppMsg::SearchNext,
                    search::SearchOutput::Previous => AppMsg::SearchPrev,
                    search::SearchOutput::Closed => AppMsg::SearchClose,
                });

        let tab_filter_control = sidebar::TabFilterModel::builder().launch(()).forward(
            sender.input_sender(),
            |output| match output {
                sidebar::TabFilterOutput::Changed(filter) => AppMsg::SetTabFilter(filter),
            },
        );

        let startup_ui::FileTreeUi {
            store: file_tree_store,
            scroll: file_tree_scroll,
            header: file_header,
        } = startup_ui::build_file_tree(&sender);

        let sidebar_width = config.borrow().sidebar_width as i32;
        let tab_placement = config.borrow().tab_placement;
        let sidebar_view = config.borrow().sidebar_view;
        let sidebar_toggle = sidebar_toggle::SidebarToggleModel::builder()
            .launch((sidebar_view, tab_placement == config::TabPlacement::Sidebar))
            .forward(sender.input_sender(), |output| match output {
                sidebar_toggle::SidebarToggleOutput::View(view) => AppMsg::SetSidebarView(view),
            });

        let (tab_strip_scroll, top_tab_scroll) = startup_ui::build_tab_scrolls();
        let top_bar = top_bar::TopBarModel::builder()
            .launch(top_tab_scroll.clone())
            .forward(sender.input_sender(), |output| match output {
                top_bar::TopBarOutput::OpenPalette => AppMsg::Action(Action::ToggleCommandPalette),
                top_bar::TopBarOutput::ToggleSidebar => AppMsg::ToggleSidebar,
                top_bar::TopBarOutput::ToggleTabPlacement => {
                    AppMsg::Action(Action::ToggleTabPlacement)
                }
                top_bar::TopBarOutput::NewTab => AppMsg::NewTab,
                top_bar::TopBarOutput::Quit => AppMsg::Quit,
            });

        let toggle_row = sidebar_toggle.widget();

        // "tabs" page: filter entry + the tab strip's sidebar holder.
        let tabs_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        tabs_page.append(tab_filter_control.widget());
        tabs_page.append(&tab_strip_scroll);

        // "files" page: root header (up / goto-cwd / path) + file tree.
        let files_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        files_page.append(file_header.widget());
        files_page.append(&file_tree_scroll);

        let sidebar_stack = gtk::Stack::new();
        sidebar_stack.add_named(&tabs_page, Some("tabs"));
        sidebar_stack.add_named(&files_page, Some("files"));
        sidebar_stack.set_vexpand(true);

        let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_box.set_width_request(sidebar_width);
        sidebar_box.set_hexpand(false);
        sidebar_box.add_css_class("tab-strip");
        sidebar_box.append(toggle_row);
        sidebar_box.append(&sidebar_stack);

        // A Paned gives the sidebar the visible, draggable divider used by
        // jterm4. Keep the sidebar at its persisted width on startup.
        let content_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        content_paned.set_vexpand(true);
        content_paned.set_wide_handle(true);
        content_paned.set_start_child(Some(&sidebar_box));
        content_paned.set_end_child(Some(&stack));
        content_paned.set_resize_start_child(false);
        content_paned.set_resize_end_child(true);
        content_paned.set_shrink_start_child(false);
        content_paned.set_shrink_end_child(true);
        content_paned.set_position(sidebar_width);

        let mut settings_font_names: Vec<String> = root
            .pango_context()
            .list_families()
            .iter()
            .filter(|family| family.is_monospace())
            .map(|family| family.name().to_string())
            .collect();
        settings_font_names.sort_by_key(|name| name.to_lowercase());

        let current_font_desc =
            gtk::pango::FontDescription::from_string(&config.borrow().font_desc);
        let current_family = current_font_desc
            .family()
            .map(|family| family.to_string())
            .unwrap_or_default();
        let current_font = settings_font_names
            .iter()
            .position(|family| family == &current_family)
            .unwrap_or(0) as u32;
        let current_theme = themes
            .iter()
            .position(|theme| theme.name == config.borrow().theme_name)
            .unwrap_or(0) as u32;
        let settings = dialogs::settings::SettingsModel::builder()
            .launch(dialogs::settings::SettingsInit {
                theme_names: themes.iter().map(|theme| theme.name.clone()).collect(),
                font_names: settings_font_names.clone(),
                values: dialogs::settings::SettingsValues {
                    theme: current_theme,
                    font: current_font,
                    font_size: (current_font_desc.size() as f64 / gtk::pango::SCALE as f64)
                        .max(6.0),
                    font_scale,
                    opacity: window_opacity,
                    scrollback: config.borrow().terminal_scrollback_lines as f64,
                    terminal_mode: match config.borrow().terminal_mode {
                        TerminalMode::Block => 0,
                        TerminalMode::Vte => 1,
                    },
                    block_compact: config.borrow().block_compact,
                    command_history: config.borrow().command_history_enabled,
                    ai_enabled: config.borrow().ai_enabled,
                    agent_enabled: config.borrow().agent_enabled,
                    notifications: config.borrow().notify_long_blocks,
                    remote_clipboard: config.borrow().allow_remote_clipboard_write,
                },
            })
            .forward(sender.input_sender(), |output| match output {
                dialogs::settings::SettingsOutput::Theme(index) => AppMsg::SettingsTheme(index),
                dialogs::settings::SettingsOutput::FontDesc(desc) => AppMsg::SettingsFontDesc(desc),
                dialogs::settings::SettingsOutput::FontScale(scale) => {
                    AppMsg::SettingsFontScale(scale)
                }
                dialogs::settings::SettingsOutput::Opacity(opacity) => {
                    AppMsg::SettingsOpacity(opacity)
                }
                dialogs::settings::SettingsOutput::Scrollback(lines) => {
                    AppMsg::SettingsScrollback(lines)
                }
                dialogs::settings::SettingsOutput::TerminalMode(mode) => {
                    AppMsg::SettingsTerminalMode(mode)
                }
                dialogs::settings::SettingsOutput::BlockCompact(enabled) => {
                    AppMsg::SettingsBlockCompact(enabled)
                }
                dialogs::settings::SettingsOutput::CommandHistory(enabled) => {
                    AppMsg::SettingsCommandHistory(enabled)
                }
                dialogs::settings::SettingsOutput::AiEnabled(enabled) => {
                    AppMsg::SettingsAiEnabled(enabled)
                }
                dialogs::settings::SettingsOutput::AgentEnabled(enabled) => {
                    AppMsg::SettingsAgentEnabled(enabled)
                }
                dialogs::settings::SettingsOutput::Notifications(enabled) => {
                    AppMsg::SettingsNotifications(enabled)
                }
                dialogs::settings::SettingsOutput::RemoteClipboard(enabled) => {
                    AppMsg::SettingsRemoteClipboard(enabled)
                }
            });
        let remote_picker = dialogs::remote_picker::RemotePickerModel::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), |output| match output {
                dialogs::remote_picker::RemotePickerOutput::Connect(index) => {
                    AppMsg::Action(Action::ConnectRemote(index as u8))
                }
            });
        let command_palette = dialogs::command_palette::PaletteModel::builder()
            .launch(dialogs::command_palette::PaletteInit {
                parent: root.clone(),
                keybindings: kbmap.clone(),
                workflows: workflows.clone(),
            })
            .forward(sender.input_sender(), |output| match output {
                dialogs::command_palette::PaletteOutput::Action(action) => AppMsg::Action(action),
                dialogs::command_palette::PaletteOutput::TypeCommand(command) => {
                    AppMsg::PaletteTypeCommand(command)
                }
                dialogs::command_palette::PaletteOutput::AskAi(query) => {
                    AppMsg::PaletteAskAi(query)
                }
                dialogs::command_palette::PaletteOutput::RunWorkflow(path) => {
                    AppMsg::PaletteRunWorkflow(path)
                }
            });
        let history = dialogs::history::HistoryModel::builder()
            .launch(dialogs::history::HistoryInit {
                keybindings: kbmap.clone(),
                workflows: workflows.clone(),
            })
            .forward(sender.input_sender(), |output| match output {
                dialogs::history::HistoryOutput::Action(action) => AppMsg::Action(action),
                dialogs::history::HistoryOutput::TypeCommand(command) => {
                    AppMsg::PaletteTypeCommand(command)
                }
                dialogs::history::HistoryOutput::AskAi(query) => AppMsg::PaletteAskAi(query),
                dialogs::history::HistoryOutput::RunWorkflow(path) => {
                    AppMsg::PaletteRunWorkflow(path)
                }
            });
        let debug_dashboard = dialogs::debug_dashboard::DebugDashboardModel::builder()
            .launch(root.clone())
            .detach();
        let workflow_dialog = dialogs::workflow::WorkflowModel::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), |output| match output {
                dialogs::workflow::WorkflowOutput::Command(command) => {
                    AppMsg::PaletteTypeCommand(command)
                }
            });
        let ai_panel = dialogs::ai_panel::AiPanelModel::builder()
            .launch(root.clone())
            .detach();
        let notebook = notebook::NotebookModel::builder()
            .launch(root.clone())
            .detach();
        let agent_panel = agent::AgentPanelModel::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), |output| match output {
                agent::AgentPanelOutput::Send(text) => AppMsg::AgentSend(text),
                agent::AgentPanelOutput::Approve(index) => AppMsg::AgentApprove(index),
                agent::AgentPanelOutput::Edit(index, command) => {
                    AppMsg::AgentEditRequested(index, command)
                }
                agent::AgentPanelOutput::Reject(index) => AppMsg::AgentReject(index),
                agent::AgentPanelOutput::Closed => AppMsg::AgentClose,
            });
        let agent_edit = agent::AgentEditModel::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), |output| match output {
                agent::AgentEditOutput::Approved(index, command) => {
                    AppMsg::AgentEditAndApprove(index, command)
                }
            });
        let tab_rows = FactoryVecDeque::builder()
            .launch(tab_strip.clone())
            .forward(sender.input_sender(), |output| match output {
                tab_strip::TabRowOutput::Select(id) => AppMsg::SelectTab(id),
                tab_strip::TabRowOutput::Close(id) => AppMsg::CloseTab(id),
                tab_strip::TabRowOutput::Rename(id, title) => AppMsg::RenameTab(id, title),
                tab_strip::TabRowOutput::NewTab => AppMsg::NewTab,
                tab_strip::TabRowOutput::Action(id, action) => AppMsg::TabRowAction(id, action),
                tab_strip::TabRowOutput::ConnectRemote(index) => {
                    AppMsg::Action(Action::ConnectRemote(index))
                }
                tab_strip::TabRowOutput::Resize(width) => AppMsg::SetTabWidth(width),
                tab_strip::TabRowOutput::Reorder { source_id, target } => {
                    AppMsg::ReorderTab(source_id, target)
                }
            });

        let toast_overlay = adw::ToastOverlay::new();
        let quit_allowed = Rc::new(std::cell::Cell::new(false));
        let mut model = AppModel {
            config,
            themes: Rc::new(themes),
            kbmap,
            shell_argv,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            next_pane_id: 0,
            // With tabs in the top bar, keep the optional file sidebar closed
            // until the user explicitly opens it.
            sidebar_visible: tab_placement == config::TabPlacement::Sidebar,
            font_scale,
            window_opacity,
            stack: stack.clone(),
            tab_strip: tab_strip.clone(),
            tab_rows,
            window: root.clone(),
            toast_overlay: toast_overlay.clone(),
            quit_allowed: quit_allowed.clone(),
            session_persistence,
            dyn_css,
            search,
            tab_filter_control,
            tab_filter: String::new(),
            file_tree_store: file_tree_store.clone(),
            file_header,
            file_tree_root: Rc::new(RefCell::new(std::path::PathBuf::new())),
            tab_strip_scroll: tab_strip_scroll.clone(),
            top_tab_scroll: top_tab_scroll.clone(),
            top_bar,
            sidebar_box: sidebar_box.clone(),
            sidebar_stack: sidebar_stack.clone(),
            sidebar_toggle,
            tab_placement: std::cell::Cell::new(tab_placement),
            sidebar_view: std::cell::Cell::new(sidebar_view),
            command_palette,
            settings,
            settings_font_names: Rc::new(settings_font_names),
            remote_picker,
            debug_dashboard,
            history,
            workflow_dialog,
            ai_panel,
            notebook,
            workflows,
            active_agent: Rc::new(RefCell::new(None)),
            agent_panel,
            agent_edit,
        };

        let search_bar = model.search.widget();
        let top_bar = model.top_bar.widget();
        let toast_overlay = &model.toast_overlay;
        let widgets = view_output!();

        // Route both the title-bar button and the window manager's close action
        // through the same running-process confirmation. ForceQuit flips the
        // shared flag before calling close(), allowing that second signal
        // through without presenting the dialog again.
        {
            let close_sender = sender.clone();
            let quit_allowed = model.quit_allowed.clone();
            root.connect_close_request(move |_| {
                if quit_allowed.get() {
                    glib::Propagation::Proceed
                } else {
                    close_sender.input(AppMsg::Quit);
                    glib::Propagation::Stop
                }
            });
        }

        if let Some(error) = config_warning {
            model.show_toast(format!(
                "Config could not be loaded; defaults are active. Your file was left untouched. {error}"
            ));
        }

        // Place the tab strip (sidebar vs top bar) and select the sidebar view.
        model.apply_tab_placement();
        model.sidebar_box.set_visible(model.sidebar_visible);

        // Window-level key controller: intercept shortcuts before VTE.
        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let kb = model.kbmap.clone();
            let ksender = sender.clone();
            key_controller.connect_key_pressed(move |_c, keyval, _kc, state| {
                let mods = state
                    & (ModifierType::CONTROL_MASK
                        | ModifierType::SHIFT_MASK
                        | ModifierType::ALT_MASK);
                let combo = KeyCombo {
                    modifiers: mods,
                    key: normalize_key(keyval),
                };
                let lookup = kb.borrow().lookup(&combo);
                // eprintln!("[jterm1] key combo={:?} -> {:?}", combo, lookup);
                if let Some(action) = lookup {
                    ksender.input(AppMsg::Action(action));
                    return glib::Propagation::Stop;
                }
                // Alt+<Copy-binding> in block mode → copy block output only.
                // Re-lookup with ALT stripped so users only need to bind Copy once.
                if mods.contains(ModifierType::ALT_MASK) {
                    let alt_combo = KeyCombo {
                        modifiers: mods - ModifierType::ALT_MASK,
                        key: combo.key,
                    };
                    if kb.borrow().lookup(&alt_combo) == Some(Action::Copy) {
                        ksender.input(AppMsg::CopyOutputOnly);
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
        }
        root.add_controller(key_controller);

        // Config file hot reload: watch config.toml for external changes.
        let config_path = config_file_path();
        if let Some(parent) = config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let config_file = gio::File::for_path(&config_path);
        if let Ok(monitor) =
            config_file.monitor_file(gio::FileMonitorFlags::NONE, None::<&Cancellable>)
        {
            let rsender = sender.clone();
            let reload_pending = Rc::new(std::cell::Cell::new(false));
            monitor.connect_changed(move |_, _, _, event| {
                if matches!(
                    event,
                    gio::FileMonitorEvent::Changed | gio::FileMonitorEvent::Created
                ) && !reload_pending.get()
                {
                    reload_pending.set(true);
                    let rsender = rsender.clone();
                    let pending = reload_pending.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(200),
                        move || {
                            pending.set(false);
                            rsender.input(AppMsg::ReloadConfig);
                        },
                    );
                }
            });
            unsafe { root.set_data("config-monitor", monitor) };
        }

        model.apply_dynamic_css();

        // Restore a previously-saved session if present (consume-on-start);
        // otherwise open a single fresh tab running startup_commands.
        match restore_session.then(session::load_session).flatten() {
            Some(saved) => {
                for tab in &saved.tabs {
                    model.restore_tab(tab, &sender);
                }
                let active_id = model
                    .tabs
                    .get(saved.active.min(model.tabs.len().saturating_sub(1)))
                    .map(|t| t.id);
                if let Some(id) = active_id {
                    model.select_tab(id, &sender);
                }
            }
            None => {
                let initial_argv = execute_argv.unwrap_or_else(|| model.shell_argv.clone());
                let initial_commands = if init.execute.is_some() {
                    None
                } else {
                    startup
                };
                model.add_tab_with(initial_commands, requested_cwd, initial_argv, &sender);
            }
        }

        model.init_file_tree();

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::NewTab => {
                let startup = self.config.borrow().startup_commands.clone();
                self.add_tab(startup, &sender);
            }
            AppMsg::CloseTab(id) => self.request_close_tab(id, &sender),
            AppMsg::ForceCloseTab(id) => self.close_tab(id, &sender),
            AppMsg::ForceClosePane(pane_id) => self.close_pane(pane_id, &sender),
            AppMsg::SelectTab(id) => self.select_tab(id, &sender),
            AppMsg::NextTab => self.switch_tab(1, &sender),
            AppMsg::PrevTab => self.switch_tab(-1, &sender),
            AppMsg::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                self.sidebar_box.set_visible(self.sidebar_visible);
            }
            AppMsg::Quit => {
                self.request_quit(&sender);
            }
            AppMsg::ForceQuit => self.force_quit(),
            AppMsg::Toast(message) => self.show_toast(message),
            AppMsg::CopyOutputOnly => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::CopyOutputOnly);
                }
            }
            AppMsg::Action(action) => self.execute_action(action, &sender),
            AppMsg::ReloadConfig => self.reload_config(&sender),
            AppMsg::SetTabWidth(width) => {
                self.config.borrow_mut().tab_width = width.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH);
                self.rebuild_tab_strip(&sender);
                self.persist_config();
            }
            AppMsg::PaneExited(tab_id, pane_id, code) => {
                // A remote single-pane tab that died abnormally is reconnected in
                // place instead of closed; everything else closes normally.
                if self.schedule_remote_reconnect(tab_id, code, &sender) {
                    return;
                }
                self.close_pane(pane_id, &sender);
            }
            AppMsg::RemoteReconnectTick(id, secs) => {
                if let Some(idx) = self.index_of(id) {
                    if let Some(conn) = self.tabs[idx].remote.as_ref() {
                        self.tabs[idx].title = format!("{} — reconnect {secs}s", conn.host.name);
                        self.rebuild_tab_strip(&sender);
                    }
                }
            }
            AppMsg::RemoteReconnectNow(id, attempt) => {
                self.do_remote_reconnect(id, attempt, &sender)
            }
            AppMsg::PaneCwdChanged(_, pane_id, path) => {
                if let Some((ti, pi)) = self.find_pane(pane_id) {
                    self.tabs[ti].panes[pi].cwd = Some(path.clone());
                    self.mark_remote_connected(ti, &sender);
                    if self.tabs[ti].active_pane == pi && !self.tabs[ti].custom_title {
                        let number = ti as u32 + 1;
                        self.tabs[ti].title = default_tab_title(number, Some(&path));
                        self.rebuild_tab_strip(&sender);
                    }
                }
            }
            AppMsg::PaneRemoteSessionId(tab_id, id) => {
                if let Some(idx) = self.index_of(tab_id) {
                    if let Some(conn) = self.tabs[idx].remote.as_mut() {
                        // Learn rsh's session id so a reconnect passes the same
                        // `--session <id>` and rsh restores cwd/env/aliases.
                        // Overrides any static value the TOML config set.
                        conn.host.session = Some(id);
                    }
                }
            }
            AppMsg::PaneFocused(_, pane_id) => {
                if let Some((ti, pi)) = self.find_pane(pane_id) {
                    self.tabs[ti].active_pane = pi;
                }
            }
            AppMsg::TitleChanged(id, title) => {
                if let Some(idx) = self.index_of(id) {
                    if !self.tabs[idx].custom_title && !title.is_empty() {
                        let filter = self.tab_filter.to_lowercase();
                        let was_visible = filter.is_empty()
                            || self.tabs[idx].title.to_lowercase().contains(&filter);
                        let is_visible =
                            filter.is_empty() || title.to_lowercase().contains(&filter);
                        self.tabs[idx].title = title;
                        // A filter membership change really does alter the row
                        // set. Otherwise update only the label: OSC-title
                        // spinners can arrive many times per second.
                        if was_visible != is_visible
                            || (is_visible
                                && !self.update_tab_title_widget(id, &self.tabs[idx].title))
                        {
                            self.rebuild_tab_strip(&sender);
                        }
                    }
                }
            }
            AppMsg::Bell(id) => {
                if let Some(idx) = self.index_of(id) {
                    if idx != self.active {
                        self.tabs[idx].bell = true;
                        self.rebuild_tab_strip(&sender);
                    }
                }
            }
            AppMsg::Activity(id) => {
                if let Some(idx) = self.index_of(id) {
                    self.mark_remote_connected(idx, &sender);
                    if idx != self.active && !self.tabs[idx].activity {
                        self.tabs[idx].activity = true;
                        self.rebuild_tab_strip(&sender);
                    }
                }
            }
            AppMsg::SettingsTheme(idx) => self.apply_settings_theme(idx),
            AppMsg::SettingsFontDesc(desc) => self.apply_settings_font_desc(desc),
            AppMsg::SettingsFontScale(scale) => self.apply_settings_font_scale(scale),
            AppMsg::SettingsOpacity(opacity) => self.apply_settings_opacity(opacity),
            AppMsg::SettingsScrollback(lines) => self.apply_settings_scrollback(lines),
            AppMsg::SettingsTerminalMode(mode) => self.apply_settings_terminal_mode(mode),
            AppMsg::SettingsBlockCompact(enabled) => self.apply_settings_block_compact(enabled),
            AppMsg::SettingsCommandHistory(enabled) => self.apply_settings_command_history(enabled),
            AppMsg::SettingsAiEnabled(enabled) => self.apply_settings_ai_enabled(enabled),
            AppMsg::SettingsAgentEnabled(enabled) => self.apply_settings_agent_enabled(enabled),
            AppMsg::SettingsNotifications(enabled) => self.apply_settings_notifications(enabled),
            AppMsg::SettingsRemoteClipboard(enabled) => {
                self.apply_settings_remote_clipboard(enabled)
            }
            AppMsg::SearchChanged(text) => {
                if let Some(t) = self.active_terminal() {
                    if text.is_empty() {
                        t.emit(VteInput::SearchClear);
                    } else {
                        let (query, use_regex) = Self::search_query(&text);
                        t.emit(VteInput::SearchSet(query, use_regex));
                    }
                }
            }
            AppMsg::SearchNext => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::SearchNext);
                }
            }
            AppMsg::SearchPrev => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::SearchPrev);
                }
            }
            AppMsg::SearchClose => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.emit(VteInput::SearchClear);
                    terminal.emit(VteInput::GrabFocus);
                }
            }
            AppMsg::RenameTab(id, title) => {
                if let Some(idx) = self.index_of(id) {
                    let trimmed = title.trim();
                    if trimmed.is_empty() {
                        self.tabs[idx].custom_title = false;
                        let number = idx as u32 + 1;
                        let cwd = self.tabs[idx]
                            .panes
                            .get(self.tabs[idx].active_pane)
                            .and_then(|p| p.cwd.clone());
                        self.tabs[idx].title = default_tab_title(number, cwd.as_deref());
                    } else {
                        self.tabs[idx].title = trimmed.to_string();
                        self.tabs[idx].custom_title = true;
                    }
                    self.rebuild_tab_strip(&sender);
                }
            }
            AppMsg::ReorderTab(src_id, to_idx) => self.reorder_tab(src_id, to_idx, &sender),
            AppMsg::TabRowAction(id, action) => {
                self.select_tab(id, &sender);
                let action = match action {
                    tab_strip::TabAction::Duplicate => Action::DuplicateTab,
                    tab_strip::TabAction::ToggleMarked => Action::ToggleTabMarked,
                    tab_strip::TabAction::TogglePinned => Action::ToggleTabPinned,
                };
                self.execute_action(action, &sender);
            }
            AppMsg::SetTabFilter(text) => {
                self.tab_filter = text;
                self.rebuild_tab_strip(&sender);
            }
            AppMsg::FileTreeActivateFile(path) => {
                if let Some(term) = self.active_terminal() {
                    let snippet = format!("{} ", file_tree::shell_quote(&path));
                    term.emit(VteInput::WriteInput(snippet.into_bytes()));
                    term.emit(VteInput::GrabFocus);
                }
            }
            AppMsg::OpenNotebook(path) => {
                self.notebook.emit(notebook::NotebookMsg::Open(path));
            }
            AppMsg::OpenAgent => self.open_agent_panel(&sender),
            AppMsg::AgentSend(text) => self.agent_send(text, &sender),
            AppMsg::AgentApprove(idx) => self.agent_approve(idx, None, &sender),
            AppMsg::AgentEditAndApprove(idx, new_cmd) => {
                self.agent_approve(idx, Some(new_cmd), &sender);
            }
            AppMsg::AgentEditRequested(idx, command) => {
                self.agent_edit
                    .emit(agent::AgentEditMsg::Open(idx, command));
            }
            AppMsg::AgentReject(idx) => self.agent_reject(idx, &sender),
            AppMsg::AgentLlmReply(reply) => self.agent_handle_reply(reply, &sender),
            AppMsg::AgentBlockFinished {
                tab_id,
                pane_id,
                command,
                exit_code,
                output_sample,
            } => {
                self.agent_handle_block_finished(
                    tab_id,
                    pane_id,
                    command,
                    exit_code,
                    output_sample,
                    &sender,
                );
            }
            AppMsg::AgentClose => self.agent_close(),
            AppMsg::PaletteTypeCommand(cmd) => {
                if let Some(term) = self.active_terminal() {
                    term.emit(VteInput::WriteInput(cmd.into_bytes()));
                    term.emit(VteInput::GrabFocus);
                }
            }
            AppMsg::PaletteAskAi(query) => {
                self.handle_palette_ask_ai(query, &sender);
            }
            AppMsg::OpenAiPanel => {
                self.show_ai_session_panel();
            }
            AppMsg::PaletteRunWorkflow(path) => {
                self.run_workflow_from_path(path, &sender);
            }
            AppMsg::FileTreeGotoCwd => self.file_tree_goto_current_cwd(),
            AppMsg::FileTreeGoUp => self.file_tree_go_up(),
            AppMsg::SetSidebarView(view) => self.apply_sidebar_view(view, true),
            AppMsg::Ignore => {}
        }
    }
}

fn main() {
    let command = match cli::parse(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("jterm1: {error}\nTry 'jterm1 --help' for usage.");
            std::process::exit(2);
        }
    };

    match command {
        cli::Command::Help => print!("{}", cli::HELP),
        cli::Command::Version => println!("jterm1 {}", env!("CARGO_PKG_VERSION")),
        cli::Command::Doctor => {
            init_logging();
            if !run_doctor() {
                std::process::exit(1);
            }
        }
        cli::Command::InitConfig => {
            if let Err(error) = init_config_file() {
                eprintln!("jterm1: {error}");
                std::process::exit(1);
            }
        }
        cli::Command::PrintShellIntegration(shell) => print_shell_integration(shell),
        cli::Command::Run(mut options) => {
            if let Err(error) = validate_launch_options(&mut options) {
                eprintln!("jterm1: {error}");
                std::process::exit(2);
            }
            init_logging();
            init_input_method_env();
            // NON_UNIQUE: each launch is its own process with its own window.
            // Session persistence uses per-process snapshots so instances do
            // not overwrite one another.
            let app = RelmApp::from_app(
                adw::Application::builder()
                    .application_id("app.jterm1")
                    .flags(gio::ApplicationFlags::NON_UNIQUE)
                    .build(),
            )
            // jterm1 has already parsed its command line. Passing only argv[0]
            // prevents GApplication from rejecting our launch options as
            // unknown GTK options during its second-stage initialization.
            .with_args(vec!["jterm1".to_string()]);
            app.run::<AppModel>(options);
        }
    }
}

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .try_init();
}

fn validate_launch_options(options: &mut cli::LaunchOptions) -> Result<(), String> {
    if let Some(directory) = options.working_directory.as_mut() {
        let canonical = std::fs::canonicalize(&*directory)
            .map_err(|err| format!("cannot open directory {}: {err}", directory.display()))?;
        if !canonical.is_dir() {
            return Err(format!("{} is not a directory", canonical.display()));
        }
        *directory = canonical;
    }
    if let Some(argv) = &options.execute {
        let executable = argv.first().expect("CLI parser rejects empty commands");
        let path = std::path::Path::new(executable);
        let found = if path.components().count() > 1 {
            path.is_file()
        } else {
            config::find_executable_in_path(executable).is_some()
        };
        if !found {
            return Err(format!("command not found: {executable}"));
        }
    }
    Ok(())
}

fn init_config_file() -> Result<(), String> {
    use std::io::Write;

    let path = config_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} already exists; it was not overwritten", path.display())
        } else {
            format!("cannot create {}: {err}", path.display())
        }
    })?;
    file.write_all(include_str!("../config.toml.example").as_bytes())
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    println!("Created {}", path.display());
    Ok(())
}

fn print_shell_integration(shell: cli::ShellIntegration) {
    let script = match shell {
        cli::ShellIntegration::Bash => include_str!("../scripts/shell-integration/jterm1.bash"),
        cli::ShellIntegration::Zsh => include_str!("../scripts/shell-integration/jterm1.zsh"),
        cli::ShellIntegration::Fish => include_str!("../scripts/shell-integration/jterm1.fish"),
        cli::ShellIntegration::PowerShell => {
            include_str!("../scripts/shell-integration/jterm1.ps1")
        }
    };
    print!("{script}");
}

fn run_doctor() -> bool {
    let mut errors = 0usize;
    let mut warnings = 0usize;
    let ok = |label: &str, value: String| println!("[ok]    {label}: {value}");
    let mut warn = |label: &str, value: String| {
        warnings += 1;
        println!("[warn]  {label}: {value}");
    };
    let mut error = |label: &str, value: String| {
        errors += 1;
        println!("[error] {label}: {value}");
    };

    println!("jterm1 {} diagnostics\n", env!("CARGO_PKG_VERSION"));
    let config_path = config_file_path();
    match config::config_file_error() {
        Some(message) => error("config", message),
        None if config_path.is_file() => ok("config", config_path.display().to_string()),
        None => warn(
            "config",
            format!(
                "{} does not exist (built-in defaults)",
                config_path.display()
            ),
        ),
    }

    let (config, _, _) = load_config();
    let shell_argv = choose_shell_argv(config.shell.as_deref());
    let shell = shell_argv.first().cloned().unwrap_or_default();
    let shell_found = if std::path::Path::new(&shell).components().count() > 1 {
        std::path::Path::new(&shell).is_file()
    } else {
        config::find_executable_in_path(&shell).is_some()
    };
    if shell_found {
        ok("shell", shell_argv.join(" "));
    } else {
        error("shell", format!("not executable: {shell}"));
    }

    let display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .map(|value| format!("Wayland {value}"))
        .or_else(|| {
            std::env::var("DISPLAY")
                .ok()
                .map(|value| format!("X11 {value}"))
        });
    match display {
        Some(display) => ok("display", display),
        None => warn(
            "display",
            "DISPLAY and WAYLAND_DISPLAY are unset".to_string(),
        ),
    }

    match &config.command_history_path {
        Some(path) => ok("command history", path.clone()),
        None => warn("command history", "disabled".to_string()),
    }
    let workflow_count = workflows::load_all(&workflows::workflow_dirs()).len();
    ok("workflows", format!("{workflow_count} available"));
    if let Some(client) = ai::AiClient::from_env() {
        ok("AI", client.display_name());
    } else {
        warn("AI", "provider configuration is incomplete".to_string());
    }
    if config::find_executable_in_path("notify-send").is_some() {
        ok("notifications", "notify-send available".to_string());
    } else {
        warn(
            "notifications",
            "notify-send missing (long-command alerts disabled)".to_string(),
        );
    }
    ok(
        "terminal mode",
        match config.terminal_mode {
            TerminalMode::Block => "block".to_string(),
            TerminalMode::Vte => "vte".to_string(),
        },
    );

    println!("\nSummary: {errors} error(s), {warnings} warning(s)");
    errors == 0
}

/// Make the fcitx5 GTK4 input-method module discoverable before GTK initializes,
/// so CJK (Chinese) preedit/commit works even when the binary is launched
/// outside the nix dev shell.
///
/// The flake bakes the module directory into `FCITX5_GTK_PATH` at build time. The
/// nix-built GTK4 only searches its own store path plus `GTK_PATH`, so without
/// re-exporting that directory it never finds `libim-fcitx5.so` and IME silently
/// falls back to raw keysyms. Each var is only filled in when the environment
/// hasn't already set it, so an existing (e.g. ibus) setup is left untouched.
fn init_input_method_env() {
    let is_unset = |k: &str| std::env::var_os(k).is_none_or(|v| v.is_empty());

    if let Some(fcitx_gtk_path) = option_env!("FCITX5_GTK_PATH") {
        if !fcitx_gtk_path.is_empty() {
            let combined = match std::env::var_os("GTK_PATH") {
                Some(existing) if !existing.is_empty() => {
                    format!("{fcitx_gtk_path}:{}", existing.to_string_lossy())
                }
                _ => fcitx_gtk_path.to_string(),
            };
            unsafe { std::env::set_var("GTK_PATH", combined) };
        }
    }
    if is_unset("GTK_IM_MODULE") {
        unsafe { std::env::set_var("GTK_IM_MODULE", "fcitx") };
    }
    if is_unset("XMODIFIERS") {
        unsafe { std::env::set_var("XMODIFIERS", "@im=fcitx") };
    }
}
