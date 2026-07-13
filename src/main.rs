#![allow(dead_code)]

mod agent;
mod ai;
mod app_msg;
mod block_view;
mod cli;
mod command_history;
mod config;
mod dialogs;
mod file_tree;
mod git_meta;
mod keybindings;
mod notebook;
mod notify;
mod palette;
mod parser;
mod process;
mod pty;
mod search;
mod session;
mod sidebar;
mod sidebar_toggle;
mod tab_strip;
mod terminal;
mod top_bar;
mod vte_pty;
mod workflows;
mod workspace;

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

/// Strip one layer of markdown code fence (```bash … ``` or ``` … ```) if it
/// wraps the entire response. LLMs often format single-command outputs that
/// way even when asked for raw text.
fn strip_code_fences(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let after_lang = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        if let Some(inner) = after_lang.trim_end().strip_suffix("```") {
            return inner.trim();
        }
    }
    s
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

    /// Re-scan the user's workflow directory. Called before each palette
    /// open so users see new/edited YAMLs without a restart. Cheap: a few
    /// short files, parsed once.
    fn reload_workflows(&self) {
        let dirs = workflows::workflow_dirs();
        let loaded = workflows::load_all(&dirs);
        *self.workflows.borrow_mut() = loaded;
    }

    /// Look up a workflow by source path (the palette gives us a path, not
    /// an index, because the workflow list can be rebuilt between
    /// gather() and accept). If the workflow has no args, render and type
    /// immediately; otherwise open the param-fill dialog.
    fn run_workflow_from_path(&self, path: std::path::PathBuf, sender: &ComponentSender<AppModel>) {
        let workflow = self
            .workflows
            .borrow()
            .iter()
            .find(|w| w.source_path.as_deref() == Some(path.as_path()))
            .cloned();
        let Some(workflow) = workflow else {
            log::warn!("workflow not found: {}", path.display());
            self.show_toast(format!("Workflow not found: {}", path.display()));
            return;
        };
        if workflow.args.is_empty() {
            match workflows::render(&workflow, &std::collections::HashMap::new()) {
                Ok(rendered) => sender.input(AppMsg::PaletteTypeCommand(rendered)),
                Err(e) => {
                    log::warn!("workflow render failed: {e}");
                    self.show_toast(format!("Workflow could not be rendered: {e}"));
                }
            }
            return;
        }
        self.workflow_dialog
            .emit(dialogs::workflow::WorkflowMsg::Open(workflow));
    }

    /// `?` palette accept handler: run the natural-language query through the
    /// configured AI provider and, on success, type the returned command into
    /// the active pane (no autosubmit). Errors raise a transient toast/log
    /// only — the user can always retry.
    fn handle_palette_ask_ai(&self, query: String, sender: &ComponentSender<AppModel>) {
        if !self.config.borrow().ai_enabled {
            return;
        }
        let Some(client) = ai::AiClient::from_env() else {
            log::warn!("AI palette: no provider configured");
            self.show_toast("No AI provider is configured.");
            return;
        };
        let cwd = self
            .active_cwd()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unknown>".to_string());
        let (system, user) = ai::build_nl_to_cmd_prompt(&query, &cwd);
        let sender_clone = sender.clone();
        // Fire and forget — keeping the handle would just let us cancel, but
        // the palette has already closed by the time we get here, so there's
        // nothing user-visible to cancel against.
        let _h = ai::ask(client, system, user, move |result| match result {
            Ok(cmd) => {
                let cleaned = strip_code_fences(cmd.trim()).to_string();
                if !cleaned.is_empty() {
                    sender_clone.input(AppMsg::PaletteTypeCommand(cleaned));
                }
            }
            Err(e) => {
                log::warn!("AI palette request failed: {e}");
                sender_clone.input(AppMsg::Toast(format!("AI request failed: {e}")));
            }
        });
        std::mem::forget(_h);
    }

    // ── Agent mode ───────────────────────────────────────────────────────

    fn open_agent_panel(&self, _sender: &ComponentSender<AppModel>) {
        let cfg = self.config.borrow();
        if !cfg.ai_enabled || !cfg.agent_enabled {
            log::info!(
                "agent: disabled (ai_enabled={}, agent_enabled={})",
                cfg.ai_enabled,
                cfg.agent_enabled
            );
            self.show_toast("AI Agent is disabled in configuration.");
            return;
        }
        drop(cfg);
        let active_is_block = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane))
            .is_some_and(|pane| matches!(pane.mode, TerminalMode::Block));
        if !active_is_block {
            self.show_toast(
                "AI Agent requires a Block-mode pane so command results can be observed.",
            );
            return;
        }
        let Some(client) = ai::AiClient::from_env() else {
            log::warn!("agent: no AI provider configured");
            self.show_toast("No AI provider is configured.");
            return;
        };

        // Cancel any pre-existing session before replacing.
        if let Some(prev) = self.active_agent.borrow_mut().take() {
            prev.cancel();
        }

        let tab_id = self.tabs.get(self.active).map(|t| t.id).unwrap_or(0);
        let pane_id = self
            .tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .map(|p| p.id)
            .unwrap_or(0);
        *self.active_agent.borrow_mut() = Some(agent::AgentSession::new(tab_id, pane_id));

        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Open {
                provider_name: client.display_name(),
                view,
            });
        }
    }

    fn agent_panel_view(&self) -> Option<agent::AgentPanelView> {
        let session = self.active_agent.borrow();
        let session = session.as_ref()?;
        Some(agent::AgentPanelView {
            transcript: session.transcript.clone(),
            turns_used: session.turns_used,
            max_turns: self.config.borrow().agent_max_turns,
            awaiting_command: session.awaiting_command.is_some(),
            sealed: session.sealed,
            loading: session.in_flight.is_some(),
        })
    }

    fn refresh_agent_panel(&self) {
        if let Some(view) = self.agent_panel_view() {
            self.agent_panel.emit(agent::AgentPanelMsg::Render(view));
        }
    }

    /// Push a user turn and kick off the next LLM turn.
    fn agent_send(&self, text: String, sender: &ComponentSender<AppModel>) {
        if self.active_agent.borrow().is_none() {
            return;
        }
        {
            let mut guard = self.active_agent.borrow_mut();
            let sess = guard.as_mut().unwrap();
            if sess.sealed {
                return;
            }
            sess.transcript.push(agent::Turn::User(text));
        }
        self.refresh_agent_panel();
        self.agent_kick_llm(sender);
    }

    fn agent_approve(
        &self,
        idx: usize,
        edited: Option<String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        let (cmd, tab_id, pane_id) = {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            if sess.sealed {
                return;
            }
            let final_cmd = match sess.transcript.get_mut(idx) {
                Some(agent::Turn::AssistantProposed { cmd, approved }) => {
                    if let Some(new_cmd) = edited {
                        *cmd = new_cmd;
                    }
                    *approved = Some(true);
                    cmd.clone()
                }
                _ => return,
            };
            sess.awaiting_command = Some(final_cmd.clone());
            (final_cmd, sess.bound_tab, sess.bound_pane)
        };
        // Type the command into the bound pane, autosubmit with \r since
        // the user has explicitly approved.
        if let Some(term) = self.terminal_for(tab_id, pane_id) {
            let mut bytes = cmd.into_bytes();
            bytes.push(b'\r');
            term.emit(VteInput::WriteInput(bytes));
            term.emit(VteInput::GrabFocus);
        }
        self.refresh_agent_panel();
    }

    fn agent_reject(&self, idx: usize, sender: &ComponentSender<AppModel>) {
        {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            if let Some(agent::Turn::AssistantProposed { approved, .. }) =
                sess.transcript.get_mut(idx)
            {
                *approved = Some(false);
            }
        }
        self.refresh_agent_panel();
        // Kick the LLM again so it can suggest something else.
        self.agent_kick_llm(sender);
    }

    fn agent_handle_block_finished(
        &self,
        tab_id: u64,
        pane_id: u64,
        command: String,
        exit_code: i32,
        output_sample: String,
        sender: &ComponentSender<AppModel>,
    ) {
        let should_feed = {
            let guard = self.active_agent.borrow();
            let Some(sess) = guard.as_ref() else { return };
            if sess.bound_tab != tab_id || sess.bound_pane != pane_id {
                return;
            }
            match sess.awaiting_command.as_ref() {
                Some(expected) if expected.trim() == command.trim() => true,
                // The user typed something themselves while the agent was
                // waiting — drop this block and keep waiting.
                _ => false,
            }
        };
        if !should_feed {
            return;
        }
        {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            sess.awaiting_command = None;
            sess.transcript.push(agent::Turn::Observation {
                exit: exit_code,
                output_sample: agent::sample_observation(&output_sample),
            });
        }
        self.refresh_agent_panel();
        self.agent_kick_llm(sender);
    }

    fn agent_handle_reply(
        &self,
        reply: Result<String, String>,
        _sender: &ComponentSender<AppModel>,
    ) {
        {
            let mut guard = self.active_agent.borrow_mut();
            let Some(sess) = guard.as_mut() else { return };
            if sess.is_cancelled() {
                return;
            }
            sess.in_flight = None;
            sess.turns_used = sess.turns_used.saturating_add(1);

            match reply {
                Err(e) => {
                    sess.transcript.push(agent::Turn::AssistantSay(format!(
                        "[error contacting model: {e}]"
                    )));
                }
                Ok(raw) => {
                    let parsed = agent::parse_action(&raw);
                    match parsed {
                        agent::ParsedAction::Run { thought, command } => {
                            if let Some(t) = thought {
                                sess.transcript.push(agent::Turn::AssistantThought(t));
                            }
                            sess.transcript.push(agent::Turn::AssistantProposed {
                                cmd: command,
                                approved: None,
                            });
                        }
                        agent::ParsedAction::Say { thought, message } => {
                            if let Some(t) = thought {
                                sess.transcript.push(agent::Turn::AssistantThought(t));
                            }
                            sess.transcript.push(agent::Turn::AssistantSay(message));
                        }
                        agent::ParsedAction::Done { thought, message } => {
                            if let Some(t) = thought {
                                sess.transcript.push(agent::Turn::AssistantThought(t));
                            }
                            sess.transcript.push(agent::Turn::AssistantSay(message));
                            sess.sealed = true;
                        }
                    }
                }
            }
            // Turn-cap seal.
            let cap = self.config.borrow().agent_max_turns;
            if sess.turns_used >= cap {
                sess.sealed = true;
            }
        }
        self.refresh_agent_panel();
    }

    fn agent_kick_llm(&self, sender: &ComponentSender<AppModel>) {
        let Some(client) = ai::AiClient::from_env() else {
            return;
        };
        // Build the prompt outside the borrow.
        let (system, user) = {
            let guard = self.active_agent.borrow();
            let Some(sess) = guard.as_ref() else { return };
            if sess.sealed {
                return;
            }
            // Don't double-fire while still waiting for a command's output.
            if sess.awaiting_command.is_some() {
                return;
            }
            let cwd = self
                .active_cwd()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string());
            let shell = self
                .shell_argv
                .first()
                .cloned()
                .unwrap_or_else(|| "/bin/sh".to_string());
            let os = std::env::consts::OS.to_string();
            (
                ai::build_agent_system_prompt(&cwd, &shell, &os),
                sess.build_user_prompt(),
            )
        };

        let sender_for_reply = sender.clone();
        let cancelled = {
            let guard = self.active_agent.borrow();
            guard.as_ref().map(|s| s.cancelled.clone())
        };
        let handle = ai::ask(client, system, user, move |result| {
            // Cancelled-check is already done by ask() against its own flag,
            // but the agent session may have moved on between fire and
            // delivery — re-check here.
            if let Some(c) = &cancelled {
                if c.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
            }
            sender_for_reply.input(AppMsg::AgentLlmReply(result));
        });
        // Stash the handle on the business session; the panel derives its
        // spinner state from `in_flight` through a fresh view snapshot.
        {
            let mut guard = self.active_agent.borrow_mut();
            if let Some(sess) = guard.as_mut() {
                sess.in_flight = Some(handle);
            }
        }
        self.refresh_agent_panel();
    }

    fn agent_close(&self) {
        self.agent_edit.emit(agent::AgentEditMsg::Close);
        if let Some(prev) = self.active_agent.borrow_mut().take() {
            prev.cancel();
        }
    }

    fn terminal_for(&self, tab_id: u64, pane_id: u64) -> Option<&TermCtl> {
        self.tabs
            .iter()
            .find(|t| t.id == tab_id)
            .and_then(|t| t.panes.iter().find(|p| p.id == pane_id))
            .map(|p| &p.terminal)
    }

    /// Open the session-level AI panel with the configured history source.
    fn show_ai_session_panel(&self) {
        if !self.config.borrow().ai_enabled {
            return;
        }
        self.ai_panel.emit(dialogs::ai_panel::AiPanelMsg::Open(
            self.config.borrow().command_history_path.clone(),
        ));
    }

    /// Rebuild the file tree with `root` at the top.
    #[allow(deprecated)]
    fn set_file_tree_root(&self, root: std::path::PathBuf) {
        self.file_tree_store.clear();
        self.file_header.emit(sidebar::FileHeaderMsg::SetRoot {
            display: file_tree::display_path(&root),
            tooltip: root.to_string_lossy().into_owned(),
        });
        file_tree::populate_dir(&self.file_tree_store, None, &root);
        *self.file_tree_root.borrow_mut() = root;
    }

    /// Initialize the file tree to the active cwd, else `$HOME`, else `/`.
    fn init_file_tree(&self) {
        let start = self
            .active_cwd()
            .or_else(file_tree::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        self.set_file_tree_root(start);
    }

    /// Jump the file tree to the active tab's working directory.
    fn file_tree_goto_current_cwd(&self) {
        match self.active_cwd() {
            Some(dir) => {
                if *self.file_tree_root.borrow() != dir {
                    self.set_file_tree_root(dir);
                }
            }
            None => {
                if self.file_tree_root.borrow().as_os_str().is_empty() {
                    if let Some(home) = file_tree::home_dir() {
                        self.set_file_tree_root(home);
                    }
                }
            }
        }
    }

    /// Move the file tree root up to its parent directory.
    fn file_tree_go_up(&self) {
        let parent = self
            .file_tree_root
            .borrow()
            .parent()
            .map(std::path::Path::to_path_buf);
        if let Some(parent) = parent {
            self.set_file_tree_root(parent);
        }
    }

    fn add_tab(&mut self, initial_commands: Option<String>, sender: &ComponentSender<AppModel>) {
        // New tabs inherit the active pane's working directory (matches
        // DuplicateTab), so Ctrl+Shift+T opens where the user already is.
        let cwd = self
            .tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .and_then(|p| p.cwd.clone());
        self.add_tab_with(initial_commands, cwd, self.shell_argv.clone(), sender);
    }

    fn add_tab_with(
        &mut self,
        initial_commands: Option<String>,
        working_directory: Option<String>,
        shell_argv: Rc<Vec<String>>,
        sender: &ComponentSender<AppModel>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let number = self.tabs.len() as u32 + 1;
        let mode = self.config.borrow().terminal_mode;
        let title_cwd = working_directory.clone();
        let pane = create_pane(
            &self.config,
            &shell_argv,
            id,
            pane_id,
            mode,
            initial_commands,
            working_directory,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.terminal.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title: default_tab_title(number, title_cwd.as_deref()),
            custom_title: false,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            id,
            zoom: None,
            remote: None,
        };
        self.insert_tab_after_active(tab);
        self.select_tab(id, sender);
    }

    /// Insert a newly-created tab immediately after the active tab. Session
    /// restoration intentionally bypasses this so its saved tab order remains
    /// unchanged.
    fn insert_tab_after_active(&mut self, tab: Tab) {
        let insert_at = self.active.saturating_add(1).min(self.tabs.len());
        self.tabs.insert(insert_at, tab);
    }

    /// Recreate a tab from a persisted snapshot, rebuilding the full nested
    /// `Paned` split tree and replaying any restorable command per pane.
    fn restore_tab(&mut self, saved: &session::SavedTab, sender: &ComponentSender<AppModel>) {
        let id = self.next_id;
        self.next_id += 1;
        let mut panes = Vec::new();
        let root_widget = self.build_pane_layout(&saved.layout, id, &mut panes, sender);
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&root_widget);
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes,
            active_pane: 0,
            title: saved.title.clone(),
            custom_title: saved.custom_title,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            id,
            zoom: None,
            remote: None,
        };
        self.tabs.push(tab);
    }

    /// Recursively build the GTK widget tree for a persisted `PaneLayout`,
    /// pushing each created leaf into `panes` in tree order.
    ///
    /// Pane mode used to be persisted with the session.  That made a mode
    /// change in config appear to have no effect: restoring an old VTE pane
    /// recreated it as VTE even when `terminal_mode = "block"`.  The current
    /// configuration is the authority for every newly-created backend,
    /// including restored panes; the snapshot only restores layout and shell
    /// state.
    fn build_pane_layout(
        &mut self,
        node: &session::PaneLayout,
        tab_id: u64,
        panes: &mut Vec<Pane>,
        sender: &ComponentSender<AppModel>,
    ) -> gtk::Widget {
        match node {
            session::PaneLayout::Leaf { cwd, cmds, .. } => {
                let pane_id = self.next_pane_id;
                self.next_pane_id += 1;
                let pane = create_pane(
                    &self.config,
                    &self.shell_argv,
                    tab_id,
                    pane_id,
                    self.config.borrow().terminal_mode,
                    cmds.clone(),
                    cwd.clone(),
                    sender,
                );
                let widget = pane.terminal.widget();
                panes.push(pane);
                widget
            }
            session::PaneLayout::Split {
                orientation,
                position,
                start,
                end,
            } => {
                let o = if *orientation == 'v' {
                    gtk::Orientation::Vertical
                } else {
                    gtk::Orientation::Horizontal
                };
                let paned = gtk::Paned::new(o);
                paned.set_hexpand(true);
                paned.set_vexpand(true);
                let start_w = self.build_pane_layout(start, tab_id, panes, sender);
                let end_w = self.build_pane_layout(end, tab_id, panes, sender);
                paned.set_start_child(Some(&start_w));
                paned.set_end_child(Some(&end_w));
                paned.set_position(*position);
                paned.upcast()
            }
        }
    }

    /// Serialize a tab's live `Paned` widget tree into a persistable `PaneLayout`.
    /// When the tab is pane-zoomed the real tree is detached into `ZoomState`, so
    /// we serialize from there and refill the removed pane's slot.
    fn serialize_layout(&self, tab: &Tab) -> session::PaneLayout {
        let root = tab
            .zoom
            .as_ref()
            .map(|z| z.tree_root.clone())
            .or_else(|| tab.holder.first_child());
        match root {
            Some(w) => self.serialize_widget(tab, &w),
            None => session::PaneLayout::Leaf {
                mode: "block".to_string(),
                cwd: None,
                cmds: None,
            },
        }
    }

    fn serialize_widget(&self, tab: &Tab, widget: &gtk::Widget) -> session::PaneLayout {
        if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
            let orientation = match paned.orientation() {
                gtk::Orientation::Vertical => 'v',
                _ => 'h',
            };
            let start = self.resolve_child(tab, paned, paned.start_child(), true);
            let end = self.resolve_child(tab, paned, paned.end_child(), false);
            session::PaneLayout::Split {
                orientation,
                position: paned.position(),
                start: Box::new(start),
                end: Box::new(end),
            }
        } else {
            let pane = tab.panes.iter().find(|p| p.terminal.widget() == *widget);
            let (mode, cwd, cmds) = match pane {
                Some(p) => (
                    match p.mode {
                        TerminalMode::Vte => "vte",
                        TerminalMode::Block => "block",
                    }
                    .to_string(),
                    p.cwd.clone(),
                    p.restorable_command(),
                ),
                None => ("block".to_string(), None, None),
            };
            session::PaneLayout::Leaf { mode, cwd, cmds }
        }
    }

    /// A `Paned` child, substituting the zoomed-out pane when its slot is empty.
    fn resolve_child(
        &self,
        tab: &Tab,
        paned: &gtk::Paned,
        child: Option<gtk::Widget>,
        want_start: bool,
    ) -> session::PaneLayout {
        if let Some(c) = child {
            return self.serialize_widget(tab, &c);
        }
        if let Some(z) = &tab.zoom {
            if &z.parent == paned && z.was_start == want_start {
                return self.serialize_widget(tab, &z.pane_widget);
            }
        }
        session::PaneLayout::Leaf {
            mode: "block".to_string(),
            cwd: None,
            cmds: None,
        }
    }

    /// Capture the current tab list as a persistable snapshot, including each
    /// tab's full split layout.
    fn snapshot_session(&self) -> session::SavedSession {
        let tabs = self
            .tabs
            .iter()
            .map(|t| session::SavedTab {
                title: t.title.clone(),
                custom_title: t.custom_title,
                layout: self.serialize_layout(t),
            })
            .collect();
        session::SavedSession {
            active: self.active,
            tabs,
        }
    }

    fn persist_session(&self) {
        if self.session_persistence {
            session::save_session(&self.snapshot_session());
        }
    }

    fn show_toast(&self, message: impl AsRef<str>) {
        self.toast_overlay
            .add_toast(adw::Toast::new(message.as_ref()));
    }

    fn persist_config(&self) {
        if let Err(err) = config::save_config(&self.config.borrow()) {
            log::error!("{err}");
            self.show_toast(format!("Settings were not saved: {err}"));
        }
    }

    fn running_process_summary(&self) -> Option<String> {
        let mut running = Vec::new();
        for tab in &self.tabs {
            for pane in &tab.panes {
                if let Some(process) = pane.foreground_process() {
                    let label = format!("{} — {process}", tab.title);
                    if !running.contains(&label) {
                        running.push(label);
                    }
                }
            }
        }
        if running.is_empty() {
            return None;
        }
        const MAX_SHOWN: usize = 8;
        let hidden = running.len().saturating_sub(MAX_SHOWN);
        running.truncate(MAX_SHOWN);
        let mut summary = running.join("\n");
        if hidden > 0 {
            summary.push_str(&format!("\n…and {hidden} more"));
        }
        Some(summary)
    }

    fn request_quit(&self, sender: &ComponentSender<AppModel>) {
        if let Some(running) = self.running_process_summary() {
            dialogs::confirm_close(&self.window, &running, AppMsg::ForceQuit, sender);
        } else {
            sender.input(AppMsg::ForceQuit);
        }
    }

    fn force_quit(&self) {
        self.persist_session();
        self.quit_allowed.set(true);
        self.window.close();
    }

    /// App-level diagnostics for the debug dashboard. (jterm4 surfaces per-block
    /// stats from the block backend; jterm1 exposes window/session state — block
    /// internals would need a backend round-trip, noted as a parity gap.)
    fn debug_info_snapshot(&self) -> Vec<(String, Vec<(String, String)>)> {
        let cfg = self.config.borrow();
        let total_panes: usize = self.tabs.iter().map(|t| t.panes.len()).sum();
        let active_tab = self.tabs.get(self.active);
        let session = vec![
            ("Tabs".to_string(), self.tabs.len().to_string()),
            ("Total panes".to_string(), total_panes.to_string()),
            (
                "Active tab".to_string(),
                active_tab.map(|t| t.title.clone()).unwrap_or_default(),
            ),
            (
                "Panes in active tab".to_string(),
                active_tab.map(|t| t.panes.len()).unwrap_or(0).to_string(),
            ),
            (
                "Zoomed".to_string(),
                active_tab
                    .map(|t| t.zoom.is_some().to_string())
                    .unwrap_or_else(|| "false".to_string()),
            ),
        ];
        let appearance = vec![
            ("Theme".to_string(), cfg.theme_name.clone()),
            ("Font".to_string(), cfg.font_desc.clone()),
            ("Font scale".to_string(), format!("{:.3}", self.font_scale)),
            ("Opacity".to_string(), format!("{:.2}", self.window_opacity)),
            (
                "Terminal mode".to_string(),
                match cfg.terminal_mode {
                    TerminalMode::Vte => "vte",
                    TerminalMode::Block => "block",
                }
                .to_string(),
            ),
            (
                "Scrollback".to_string(),
                cfg.terminal_scrollback_lines.to_string(),
            ),
        ];
        let config = vec![
            (
                "Keybindings".to_string(),
                self.kbmap.borrow().bindings.len().to_string(),
            ),
            (
                "Remote hosts".to_string(),
                cfg.remote_hosts.len().to_string(),
            ),
            (
                "Startup commands".to_string(),
                cfg.startup_commands.clone().unwrap_or_default(),
            ),
        ];
        vec![
            ("Session".to_string(), session),
            ("Appearance".to_string(), appearance),
            ("Config".to_string(), config),
        ]
    }

    /// Open a new tab that connects to a remote host via ssh. Uses block mode
    /// so OSC 133 / 7 / 7770 from the remote rsh drive the block UI; for a remote
    /// shell without OSC 133, block.rs falls back to a streaming raw view, which
    /// is no worse than the bare-VTE path this used to take.
    fn add_remote_tab(&mut self, host: &config::RemoteHost, sender: &ComponentSender<AppModel>) {
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let argv = Rc::new(config::build_remote_argv(host));
        // Remote sessions need OSC 133/7/7770 parsing for blocks, cwd updates,
        // resumable session ids, and Agent observations. Keep them on the Block
        // backend even when the local compatibility backend is configured.
        let mode = TerminalMode::Block;
        let pane = create_pane(&self.config, &argv, id, pane_id, mode, None, None, sender);
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.terminal.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title: host.name.clone(),
            custom_title: true,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            id,
            zoom: None,
            remote: Some(RemoteConn {
                host: host.clone(),
                status: ConnStatus::Connecting,
                attempt: 0,
                spawn_at: std::time::Instant::now(),
            }),
        };
        self.insert_tab_after_active(tab);
        self.select_tab(id, sender);
    }

    /// Flip a Connecting remote tab to Connected (first output/cwd seen).
    fn mark_remote_connected(&mut self, idx: usize, sender: &ComponentSender<AppModel>) {
        if let Some(conn) = self.tabs[idx].remote.as_mut() {
            if conn.status != ConnStatus::Connected {
                conn.status = ConnStatus::Connected;
                self.rebuild_tab_strip(sender);
            }
        }
    }

    /// If `tab_id` is a single-pane remote tab that died abnormally, start a
    /// backoff countdown and reconnect in place; returns true when handled (the
    /// caller should NOT close the tab). A clean exit (code 0) returns false so
    /// the tab closes normally.
    fn schedule_remote_reconnect(
        &mut self,
        tab_id: u64,
        code: i32,
        sender: &ComponentSender<AppModel>,
    ) -> bool {
        const MAX_ATTEMPT: u32 = 6;
        let Some(idx) = self.index_of(tab_id) else {
            return false;
        };
        if self.tabs[idx].panes.len() != 1 {
            return false;
        }
        let Some(conn) = self.tabs[idx].remote.clone() else {
            return false;
        };
        if code == 0 {
            // User logged out cleanly — drop the connection record, close normally.
            self.tabs[idx].remote = None;
            return false;
        }
        // A link that stayed up a while is treated as a healthy drop (reset
        // backoff); a short-lived one (failed handshake/auth) grows it.
        let stable = conn.spawn_at.elapsed() >= std::time::Duration::from_secs(10);
        let next_attempt = if stable { 0 } else { conn.attempt + 1 };
        if next_attempt > MAX_ATTEMPT {
            log::warn!(
                "[remote] giving up reconnect for '{}' after {} attempts",
                conn.host.name,
                conn.attempt
            );
            if let Some(c) = self.tabs[idx].remote.as_mut() {
                c.status = ConnStatus::Disconnected;
            }
            self.tabs[idx].title = format!("{} — disconnected", conn.host.name);
            self.rebuild_tab_strip(sender);
            return true;
        }
        let delay = if next_attempt == 0 {
            1u64
        } else {
            (1u64 << next_attempt.min(5)).min(30)
        };
        if let Some(c) = self.tabs[idx].remote.as_mut() {
            c.status = ConnStatus::Disconnected;
            c.attempt = next_attempt;
        }
        self.tabs[idx].title = format!("{} — reconnect {delay}s", conn.host.name);
        self.rebuild_tab_strip(sender);
        log::info!(
            "[remote] '{}' disconnected (exit {code}); reconnecting in {delay}s (attempt {next_attempt})",
            conn.host.name
        );

        let remaining = Rc::new(std::cell::Cell::new(delay));
        let s = sender.clone();
        glib::timeout_add_seconds_local(1, move || {
            let left = remaining.get();
            if left > 1 {
                remaining.set(left - 1);
                s.input(AppMsg::RemoteReconnectTick(tab_id, left - 1));
                glib::ControlFlow::Continue
            } else {
                s.input(AppMsg::RemoteReconnectNow(tab_id, next_attempt));
                glib::ControlFlow::Break
            }
        });
        true
    }

    /// Respawn a dead remote tab's connection in place (same tab id / position).
    fn do_remote_reconnect(
        &mut self,
        tab_id: u64,
        attempt: u32,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(idx) = self.index_of(tab_id) else {
            return;
        };
        let Some(conn) = self.tabs[idx].remote.clone() else {
            return;
        };
        // Swap the dead pane widget for a fresh remote pane.
        let old_widget = self.tabs[idx].panes[0].terminal.widget();
        self.tabs[idx].holder.remove(&old_widget);
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        // Pull the *current* host snapshot — `conn.host.session` may have been
        // learned dynamically via OSC 7770 during the prior connection, so we
        // can't reuse the cloned-at-spawn `conn`.
        let host_now = self.tabs[idx]
            .remote
            .as_ref()
            .map(|c| c.host.clone())
            .unwrap_or(conn.host.clone());
        let argv = Rc::new(config::build_remote_argv(&host_now));
        let mode = TerminalMode::Block;
        let pane = create_pane(
            &self.config,
            &argv,
            tab_id,
            pane_id,
            mode,
            None,
            None,
            sender,
        );
        self.tabs[idx].holder.append(&pane.terminal.widget());
        self.tabs[idx].panes = vec![pane];
        self.tabs[idx].active_pane = 0;
        self.tabs[idx].title = host_now.name.clone();
        if let Some(c) = self.tabs[idx].remote.as_mut() {
            c.status = ConnStatus::Connecting;
            c.attempt = attempt;
            c.spawn_at = std::time::Instant::now();
        }
        if self.active == idx {
            self.tabs[idx].panes[0].terminal.emit(VteInput::GrabFocus);
        }
        self.rebuild_tab_strip(sender);
    }

    /// Stable-partition the tab list so pinned tabs sort to the front, keeping
    /// `self.active` pointing at the same tab.
    fn reorder_pinned_first(&mut self) {
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        self.tabs.sort_by_key(|t| !t.pinned);
        if let Some(id) = active_id {
            if let Some(idx) = self.index_of(id) {
                self.active = idx;
            }
        }
    }

    fn select_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        let Some(idx) = self.index_of(id) else { return };
        self.active = idx;
        self.stack.set_visible_child_name(&id.to_string());
        {
            let tab = &mut self.tabs[idx];
            tab.bell = false;
            tab.activity = false;
        }
        let tab = &self.tabs[idx];
        if let Some(pane) = tab.panes.get(tab.active_pane) {
            pane.terminal.emit(VteInput::GrabFocus);
        }
        self.file_tree_goto_current_cwd();
        self.rebuild_tab_strip(sender);
    }

    fn close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        let Some(idx) = self.index_of(id) else { return };
        let tab = self.tabs.remove(idx);
        self.stack.remove(&tab.holder);
        drop(tab);

        if self.tabs.is_empty() {
            relm4::main_application().quit();
            return;
        }
        let new_idx = if idx >= self.tabs.len() {
            self.tabs.len() - 1
        } else {
            idx
        };
        let new_id = self.tabs[new_idx].id;
        self.select_tab(new_id, sender);
    }

    /// First restorable command running in any of a tab's panes, if any.
    fn tab_running_command(&self, idx: usize) -> Option<String> {
        self.tabs
            .get(idx)?
            .panes
            .iter()
            .find_map(|p| p.restorable_command())
    }

    /// Close a tab, first confirming if a process is still running in it.
    fn request_close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        if let Some(idx) = self.index_of(id) {
            if let Some(cmd) = self.tab_running_command(idx) {
                dialogs::confirm_close(&self.window, &cmd, AppMsg::ForceCloseTab(id), sender);
                return;
            }
        }
        self.close_tab(id, sender);
    }

    /// Close a pane, first confirming if a process is still running in it.
    fn request_close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        if let Some((ti, pi)) = self.find_pane(pane_id) {
            if let Some(cmd) = self.tabs[ti].panes[pi].restorable_command() {
                dialogs::confirm_close(&self.window, &cmd, AppMsg::ForceClosePane(pane_id), sender);
                return;
            }
        }
        self.close_pane(pane_id, sender);
    }

    /// Move the tab with `src_id` to `to_idx`, preserving which tab is active.
    fn reorder_tab(&mut self, src_id: u64, to_idx: usize, sender: &ComponentSender<AppModel>) {
        let Some(from) = self.index_of(src_id) else {
            return;
        };
        let to = to_idx.min(self.tabs.len().saturating_sub(1));
        if from == to {
            return;
        }
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if let Some(aid) = active_id {
            self.active = self.index_of(aid).unwrap_or(0);
        }
        self.rebuild_tab_strip(sender);
    }

    fn switch_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as i32;
        let idx = ((self.active as i32 + delta) % len + len) % len;
        let id = self.tabs[idx as usize].id;
        self.select_tab(id, sender);
    }

    /// Reorder the active tab one slot left (-1) or right (+1) and keep it active.
    fn move_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
        if self.tabs.len() < 2 {
            return;
        }
        let from = self.active as i32;
        let to = from + delta;
        if to < 0 || to >= self.tabs.len() as i32 {
            return;
        }
        self.tabs.swap(from as usize, to as usize);
        self.active = to as usize;
        self.rebuild_tab_strip(sender);
    }

    /// Open a new tab inheriting the active tab's mode, cwd and (custom) title.
    fn duplicate_active_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let Some(src) = self.tabs.get(self.active) else {
            return;
        };
        let cwd = src.panes.get(src.active_pane).and_then(|p| p.cwd.clone());
        let title = src.title.clone();
        let custom_title = src.custom_title;

        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let mode = self.config.borrow().terminal_mode;
        let pane = create_pane(
            &self.config,
            &self.shell_argv,
            id,
            pane_id,
            mode,
            None,
            cwd,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.terminal.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title,
            custom_title,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            id,
            zoom: None,
            remote: None,
        };
        self.insert_tab_after_active(tab);
        self.select_tab(id, sender);
    }

    /// Close every marked tab (marking is the multi-select model in jterm1).
    fn close_marked_tabs(&mut self, sender: &ComponentSender<AppModel>) {
        let ids: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.marked)
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.close_tab(id, sender);
        }
    }

    fn find_pane(&self, pane_id: u64) -> Option<(usize, usize)> {
        for (ti, tab) in self.tabs.iter().enumerate() {
            if let Some(pi) = tab.panes.iter().position(|p| p.id == pane_id) {
                return Some((ti, pi));
            }
        }
        None
    }

    /// Split the active pane, placing a fresh bare-VTE pane beside it.
    fn split_active(&mut self, orientation: gtk::Orientation, sender: &ComponentSender<AppModel>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.zoom.is_some() {
            return;
        }
        let ti = self.active;
        let tab_id = tab.id;
        let api = tab.active_pane;
        let cur_widget = tab.panes[api].terminal.widget();
        let wd = tab.panes[api].cwd.clone();

        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let new_pane = create_pane(
            &self.config,
            &self.shell_argv,
            tab_id,
            pane_id,
            TerminalMode::Vte,
            None,
            wd,
            sender,
        );
        let new_widget = new_pane.terminal.widget();

        let paned = gtk::Paned::new(orientation);
        paned.set_hexpand(true);
        paned.set_vexpand(true);

        if let Some(parent) = cur_widget.parent() {
            if let Ok(pp) = parent.clone().downcast::<gtk::Paned>() {
                let is_start = pp.start_child().as_ref() == Some(&cur_widget);
                if is_start {
                    pp.set_start_child(None::<&gtk::Widget>);
                } else {
                    pp.set_end_child(None::<&gtk::Widget>);
                }
                paned.set_start_child(Some(&cur_widget));
                paned.set_end_child(Some(&new_widget));
                if is_start {
                    pp.set_start_child(Some(&paned));
                } else {
                    pp.set_end_child(Some(&paned));
                }
            } else {
                let holder = &self.tabs[ti].holder;
                holder.remove(&cur_widget);
                paned.set_start_child(Some(&cur_widget));
                paned.set_end_child(Some(&new_widget));
                holder.append(&paned);
            }
        }

        let tab = &mut self.tabs[ti];
        tab.panes.push(new_pane);
        tab.active_pane = tab.panes.len() - 1;
        tab.panes[tab.active_pane]
            .terminal
            .emit(VteInput::GrabFocus);
    }

    /// Remove a pane from its tab, collapsing the Paned tree and promoting the
    /// sibling. Closes the whole tab if it was the last pane.
    fn close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        let Some((ti, pi)) = self.find_pane(pane_id) else {
            return;
        };
        if self.tabs[ti].zoom.is_some() {
            self.toggle_pane_zoom_for(ti);
        }
        if self.tabs[ti].panes.len() == 1 {
            let tab_id = self.tabs[ti].id;
            self.close_tab(tab_id, sender);
            return;
        }
        let eff = self.tabs[ti].panes[pi].terminal.widget();
        if let Some(parent) = eff.parent() {
            if let Ok(paned) = parent.downcast::<gtk::Paned>() {
                let start = paned.start_child();
                let end = paned.end_child();
                let sibling = if start.as_ref() == Some(&eff) {
                    end
                } else {
                    start
                };
                paned.set_start_child(None::<&gtk::Widget>);
                paned.set_end_child(None::<&gtk::Widget>);
                if let Some(sibling) = sibling {
                    let paned_w: gtk::Widget = paned.clone().upcast();
                    if let Some(gp) = paned_w.parent() {
                        if let Ok(gpp) = gp.clone().downcast::<gtk::Paned>() {
                            if gpp.start_child().as_ref() == Some(&paned_w) {
                                gpp.set_start_child(Some(&sibling));
                            } else {
                                gpp.set_end_child(Some(&sibling));
                            }
                        } else {
                            let holder = &self.tabs[ti].holder;
                            holder.remove(&paned_w);
                            holder.append(&sibling);
                        }
                    }
                }
            }
        }

        let tab = &mut self.tabs[ti];
        let removed = tab.panes.remove(pi);
        if tab.active_pane >= tab.panes.len() {
            tab.active_pane = tab.panes.len() - 1;
        }
        let ap = tab.active_pane;
        tab.panes[ap].terminal.emit(VteInput::GrabFocus);
        drop(removed);
    }

    fn cycle_pane_focus(&mut self, delta: i32) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let n = tab.panes.len() as i32;
        if n <= 1 {
            return;
        }
        let cur = tab.active_pane as i32;
        let next = ((cur + delta) % n + n) % n;
        tab.active_pane = next as usize;
        tab.panes[tab.active_pane]
            .terminal
            .emit(VteInput::GrabFocus);
    }

    fn focus_pane_directional(&mut self, direction: Direction) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.len() <= 1 {
            return;
        }
        let holder: gtk::Widget = tab.holder.clone().upcast();
        let api = tab.active_pane;
        let focused_widget = tab.panes[api].terminal.widget();
        let Some(fb) = focused_widget.compute_bounds(&holder) else {
            return;
        };
        let fcx = fb.x() + fb.width() / 2.0;
        let fcy = fb.y() + fb.height() / 2.0;

        let mut best: Option<(f32, usize)> = None;
        for (i, pane) in tab.panes.iter().enumerate() {
            if i == api {
                continue;
            }
            let w = pane.terminal.widget();
            let Some(b) = w.compute_bounds(&holder) else {
                continue;
            };
            let cx = b.x() + b.width() / 2.0;
            let cy = b.y() + b.height() / 2.0;
            let dx = cx - fcx;
            let dy = cy - fcy;
            let in_dir = match direction {
                Direction::Left => dx < -1.0,
                Direction::Right => dx > 1.0,
                Direction::Up => dy < -1.0,
                Direction::Down => dy > 1.0,
            };
            if !in_dir {
                continue;
            }
            let dist = match direction {
                Direction::Left | Direction::Right => dx.abs() + dy.abs() * 0.1,
                Direction::Up | Direction::Down => dy.abs() + dx.abs() * 0.1,
            };
            if best.is_none() || dist < best.unwrap().0 {
                best = Some((dist, i));
            }
        }

        if let Some((_, i)) = best {
            let tab = &mut self.tabs[self.active];
            tab.active_pane = i;
            tab.panes[i].terminal.emit(VteInput::GrabFocus);
        }
    }

    fn resize_pane(&mut self, target: gtk::Orientation, delta: i32) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let api = tab.active_pane;
        let mut widget = tab.panes[api].terminal.widget().parent();
        while let Some(cur) = widget {
            if let Ok(paned) = cur.clone().downcast::<gtk::Paned>() {
                if paned.orientation() == target {
                    let new_pos = (paned.position() + delta).max(0);
                    paned.set_position(new_pos);
                    return;
                }
            }
            widget = cur.parent();
        }
    }

    fn toggle_pane_zoom(&mut self) {
        self.toggle_pane_zoom_for(self.active);
    }

    fn toggle_pane_zoom_for(&mut self, ti: usize) {
        let Some(tab) = self.tabs.get_mut(ti) else {
            return;
        };
        if let Some(z) = tab.zoom.take() {
            tab.holder.remove(&z.pane_widget);
            if z.was_start {
                z.parent.set_start_child(Some(&z.pane_widget));
            } else {
                z.parent.set_end_child(Some(&z.pane_widget));
            }
            tab.holder.append(&z.tree_root);
            let ap = tab.active_pane;
            tab.panes[ap].terminal.emit(VteInput::GrabFocus);
        } else {
            if tab.panes.len() <= 1 {
                return;
            }
            let api = tab.active_pane;
            let pane_widget = tab.panes[api].terminal.widget();
            let Some(parent) = pane_widget.parent() else {
                return;
            };
            let Ok(parent_paned) = parent.downcast::<gtk::Paned>() else {
                return;
            };
            let was_start = parent_paned.start_child().as_ref() == Some(&pane_widget);
            let Some(tree_root) = tab.holder.first_child() else {
                return;
            };
            if was_start {
                parent_paned.set_start_child(None::<&gtk::Widget>);
            } else {
                parent_paned.set_end_child(None::<&gtk::Widget>);
            }
            tab.holder.remove(&tree_root);
            tab.holder.append(&pane_widget);
            tab.zoom = Some(ZoomState {
                tree_root,
                pane_widget: pane_widget.clone(),
                parent: parent_paned,
                was_start,
            });
            tab.panes[api].terminal.emit(VteInput::GrabFocus);
        }
    }

    /// Detach the active pane from a split tab and host it in a brand-new tab.
    fn move_pane_to_new_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.len() <= 1 || tab.zoom.is_some() {
            return;
        }
        let ti = self.active;
        let pi = tab.active_pane;
        let eff = tab.panes[pi].terminal.widget();

        // Collapse the source tree, promoting the sibling (same as close_pane).
        if let Some(parent) = eff.parent() {
            if let Ok(paned) = parent.downcast::<gtk::Paned>() {
                let start = paned.start_child();
                let end = paned.end_child();
                let sibling = if start.as_ref() == Some(&eff) {
                    end
                } else {
                    start
                };
                paned.set_start_child(None::<&gtk::Widget>);
                paned.set_end_child(None::<&gtk::Widget>);
                if let Some(sibling) = sibling {
                    let paned_w: gtk::Widget = paned.clone().upcast();
                    if let Some(gp) = paned_w.parent() {
                        if let Ok(gpp) = gp.clone().downcast::<gtk::Paned>() {
                            if gpp.start_child().as_ref() == Some(&paned_w) {
                                gpp.set_start_child(Some(&sibling));
                            } else {
                                gpp.set_end_child(Some(&sibling));
                            }
                        } else {
                            let holder = &self.tabs[ti].holder;
                            holder.remove(&paned_w);
                            holder.append(&sibling);
                        }
                    }
                }
            }
        }

        let moved = self.tabs[ti].panes.remove(pi);
        {
            let tab = &mut self.tabs[ti];
            if tab.active_pane >= tab.panes.len() {
                tab.active_pane = tab.panes.len() - 1;
            }
        }

        let new_id = self.next_id;
        self.next_id += 1;
        let mw = moved.terminal.widget();
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&mw);
        self.stack.add_named(&holder, Some(&new_id.to_string()));
        let number = self.tabs.len() as u32 + 1;
        let title = default_tab_title(number, moved.cwd.as_deref());
        let new_tab = Tab {
            holder,
            panes: vec![moved],
            active_pane: 0,
            title,
            custom_title: false,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            id: new_id,
            zoom: None,
            remote: None,
        };
        self.insert_tab_after_active(new_tab);
        self.select_tab(new_id, sender);
    }

    fn set_font_scale_all(&mut self, scale: f64) {
        self.font_scale = scale;
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SetFontScale(scale));
            }
        }
    }

    fn set_window_opacity(&mut self, opacity: f64) {
        self.window_opacity = opacity;
        self.window.set_opacity(opacity);
    }

    fn toggle_search(&mut self) {
        self.search.emit(search::SearchMsg::Toggle);
    }

    /// Parse the find-bar text: `/pattern/` means regex, anything else literal.
    fn search_query(text: &str) -> (String, bool) {
        if text.starts_with('/') && text.ends_with('/') && text.len() > 2 {
            (text[1..text.len() - 1].to_string(), true)
        } else {
            (text.to_string(), false)
        }
    }

    fn execute_action(&mut self, action: Action, sender: &ComponentSender<AppModel>) {
        match action {
            Action::NewTab => {
                let startup = self.config.borrow().startup_commands.clone();
                self.add_tab(startup, sender);
            }
            Action::CloseTab => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let id = tab.id;
                    self.request_close_tab(id, sender);
                }
            }
            Action::ClosePaneOrTab => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let tab_id = tab.id;
                    if tab.panes.len() > 1 {
                        let pane_id = tab.panes[tab.active_pane].id;
                        self.request_close_pane(pane_id, sender);
                    } else {
                        self.request_close_tab(tab_id, sender);
                    }
                }
            }
            Action::SplitHorizontal => self.split_active(gtk::Orientation::Horizontal, sender),
            Action::SplitVertical => self.split_active(gtk::Orientation::Vertical, sender),
            Action::CyclePaneFocusForward => self.cycle_pane_focus(1),
            Action::CyclePaneFocusBackward => self.cycle_pane_focus(-1),
            Action::FocusPaneLeft => self.focus_pane_directional(Direction::Left),
            Action::FocusPaneRight => self.focus_pane_directional(Direction::Right),
            Action::FocusPaneUp => self.focus_pane_directional(Direction::Up),
            Action::FocusPaneDown => self.focus_pane_directional(Direction::Down),
            Action::ResizePaneLeft => self.resize_pane(gtk::Orientation::Horizontal, -40),
            Action::ResizePaneRight => self.resize_pane(gtk::Orientation::Horizontal, 40),
            Action::ResizePaneUp => self.resize_pane(gtk::Orientation::Vertical, -40),
            Action::ResizePaneDown => self.resize_pane(gtk::Orientation::Vertical, 40),
            Action::TogglePaneZoom => self.toggle_pane_zoom(),
            Action::MovePaneToNewTab => self.move_pane_to_new_tab(sender),
            Action::Copy => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::Copy);
                }
            }
            Action::Paste => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::Paste);
                }
            }
            Action::FontIncrease => {
                let s = (self.font_scale + FONT_STEP).min(10.0);
                self.set_font_scale_all(s);
            }
            Action::FontDecrease => {
                let s = (self.font_scale - FONT_STEP).max(0.1);
                self.set_font_scale_all(s);
            }
            Action::OpacityIncrease => {
                let o = (self.window_opacity + OPACITY_STEP).clamp(0.01, 1.0);
                self.set_window_opacity(o);
            }
            Action::OpacityDecrease => {
                let o = (self.window_opacity - OPACITY_STEP).clamp(0.01, 1.0);
                self.set_window_opacity(o);
            }
            Action::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                self.sidebar_box.set_visible(self.sidebar_visible);
            }
            Action::ToggleCommandPalette => {
                self.reload_workflows();
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::Toggle {
                        mode: palette::PaletteMode::Commands,
                        history_path: None,
                    });
            }
            Action::OpenPalette => {
                self.reload_workflows();
                let history = self.config.borrow().command_history_path.clone();
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::Toggle {
                        mode: palette::PaletteMode::All,
                        history_path: history.map(std::path::PathBuf::from),
                    });
            }
            Action::OpenHistoryPalette => {
                self.reload_workflows();
                let history = self.config.borrow().command_history_path.clone();
                if let Some(term) = self.active_terminal() {
                    self.history.emit(dialogs::history::HistoryMsg::Toggle {
                        anchor: term.widget(),
                        history_path: history.map(std::path::PathBuf::from),
                    });
                }
            }
            Action::OpenWorkflows => {
                self.reload_workflows();
                let history = self.config.borrow().command_history_path.clone();
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::Toggle {
                        mode: palette::PaletteMode::Workflows,
                        history_path: history.map(std::path::PathBuf::from),
                    });
            }
            Action::ToggleSettings => {
                let config = self.config.borrow();
                let font_desc = gtk::pango::FontDescription::from_string(&config.font_desc);
                let family = font_desc
                    .family()
                    .map(|family| family.to_string())
                    .unwrap_or_default();
                let font = self
                    .settings_font_names
                    .iter()
                    .position(|candidate| candidate == &family)
                    .unwrap_or(0) as u32;
                let theme = self
                    .themes
                    .iter()
                    .position(|candidate| candidate.name == config.theme_name)
                    .unwrap_or(0) as u32;
                self.settings.emit(dialogs::settings::SettingsMsg::Toggle(
                    dialogs::settings::SettingsValues {
                        theme,
                        font,
                        font_size: (font_desc.size() as f64 / gtk::pango::SCALE as f64).max(6.0),
                        font_scale: self.font_scale,
                        opacity: self.window_opacity,
                        scrollback: config.terminal_scrollback_lines as f64,
                        terminal_mode: match config.terminal_mode {
                            TerminalMode::Block => 0,
                            TerminalMode::Vte => 1,
                        },
                        block_compact: config.block_compact,
                        command_history: config.command_history_enabled,
                        ai_enabled: config.ai_enabled,
                        agent_enabled: config.agent_enabled,
                        notifications: config.notify_long_blocks,
                        remote_clipboard: config.allow_remote_clipboard_write,
                    },
                    self.window.clone(),
                ));
            }
            Action::OpenWelcome => match workflows::welcome_notebook_path() {
                Some(path) => self.notebook.emit(notebook::NotebookMsg::Open(path)),
                None => self.show_toast(
                    "Welcome notebook was not found. Reinstall jterm1's shared assets.",
                ),
            },
            Action::ToggleSearch => self.toggle_search(),
            Action::MoveTabLeft => self.move_tab(-1, sender),
            Action::MoveTabRight => self.move_tab(1, sender),
            Action::DuplicateTab => self.duplicate_active_tab(sender),
            Action::ToggleTabMarked => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.marked = !tab.marked;
                }
                self.rebuild_tab_strip(sender);
            }
            Action::ToggleTabPinned => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.pinned = !tab.pinned;
                }
                self.reorder_pinned_first();
                self.rebuild_tab_strip(sender);
            }
            Action::ToggleTabPlacement => self.toggle_tab_placement(),
            Action::CloseSelectedTabs => self.close_marked_tabs(sender),
            Action::FilterTabs => {
                self.sidebar_visible = true;
                self.sidebar_box.set_visible(true);
                if self.tab_placement.get() == config::TabPlacement::Sidebar {
                    self.apply_sidebar_view(config::SidebarView::Tabs, true);
                }
                self.tab_filter_control.emit(sidebar::TabFilterMsg::Focus);
            }
            Action::PrevTab => self.switch_tab(-1, sender),
            Action::NextTab => self.switch_tab(1, sender),
            Action::ScrollUp => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ScrollLines(-3));
                }
            }
            Action::ScrollDown => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ScrollLines(3));
                }
            }
            Action::FilterFailedBlocks => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::FilterFailedBlocks);
                }
            }
            Action::FilterSlowBlocks => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::FilterSlowBlocks);
                }
            }
            Action::FilterPinnedBlocks => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::FilterPinnedBlocks);
                }
            }
            Action::JumpToPrevPinned => {
                eprintln!("[jterm1] Action::JumpToPrevPinned");
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::JumpToPrevPinned);
                }
            }
            Action::JumpToNextPinned => {
                eprintln!("[jterm1] Action::JumpToNextPinned");
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::JumpToNextPinned);
                }
            }
            Action::ClearBlockFilter => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ClearBlockFilter);
                }
            }
            Action::QuickSwitchTab(n) => {
                if !self.tabs.is_empty() {
                    let last = self.tabs.len() - 1;
                    let target = if n == 9 { last } else { (n as usize).min(last) };
                    let id = self.tabs[target].id;
                    self.select_tab(id, sender);
                }
            }
            Action::ShowRemotePicker => {
                self.remote_picker
                    .emit(dialogs::remote_picker::RemotePickerMsg::Toggle(
                        self.config.borrow().remote_hosts.clone(),
                    ));
            }
            Action::ToggleDebugDashboard => {
                let info = self.debug_info_snapshot();
                self.debug_dashboard
                    .emit(dialogs::debug_dashboard::DebugDashboardMsg::Toggle(info));
            }
            Action::ConnectRemote(n) => {
                let host = self.config.borrow().remote_hosts.get(n as usize).cloned();
                if let Some(host) = host {
                    self.add_remote_tab(&host, sender);
                }
            }
            Action::OpenAiPanel => {
                self.show_ai_session_panel();
            }
            Action::OpenAgent => {
                self.open_agent_panel(sender);
            }
        }
    }

    fn reload_config(&mut self, sender: &ComponentSender<AppModel>) {
        if let Some(error) = config::config_file_error() {
            log::warn!("configuration reload rejected: {error}");
            self.show_toast(format!(
                "Config reload rejected; the current settings remain active. {error}"
            ));
            return;
        }
        let (new_config, themes, new_kb) = load_config();
        let new_shell_argv = Rc::new(choose_shell_argv(new_config.shell.as_deref()));
        let backend_changed = std::mem::discriminant(&self.config.borrow().terminal_mode)
            != std::mem::discriminant(&new_config.terminal_mode);
        *self.config.borrow_mut() = new_config.clone();
        self.shell_argv = new_shell_argv;

        self.set_window_opacity(new_config.window_opacity);
        let font_desc = self.config.borrow().font_desc.clone();
        let scrollback = new_config.terminal_scrollback_lines as i64;
        self.font_scale = new_config.default_font_scale;
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal
                    .emit(VteInput::SetFontScale(new_config.default_font_scale));
                pane.terminal.emit(VteInput::SetFont(font_desc.clone()));
                pane.terminal.emit(VteInput::SetScrollback(scrollback));
                pane.terminal.emit(VteInput::ApplyTheme);
            }
        }

        *self.kbmap.borrow_mut() = new_kb;
        self.themes = Rc::new(themes);
        self.apply_dynamic_css();
        self.rebuild_tab_strip(sender);
        log::info!("Configuration reloaded from disk");
        if backend_changed {
            self.show_toast("Terminal mode changed; it will apply to new panes and tabs.");
        } else {
            self.show_toast("Configuration reloaded.");
        }
    }

    #[allow(deprecated)]
    fn apply_dynamic_css(&self) {
        let config = self.config.borrow();
        let bg = &config.background;
        let fg = &config.foreground;
        let br = (bg.red() * 255.0) as u8;
        let bgg = (bg.green() * 255.0) as u8;
        let bb = (bg.blue() * 255.0) as u8;
        let fr = (fg.red() * 255.0) as u8;
        let fgg = (fg.green() * 255.0) as u8;
        let fb = (fg.blue() * 255.0) as u8;
        let css = format!(
            ".terminal-box scrollbar {{ background-color: rgb({br},{bgg},{bb}); }}
             .terminal-box scrollbar trough {{ background-color: rgb({br},{bgg},{bb}); }}
             .terminal-box scrollbar slider {{ background-color: rgba({fr},{fgg},{fb},0.4); }}
             .terminal-box scrollbar slider:hover {{ background-color: rgba({fr},{fgg},{fb},0.7); }}
             .top-bar {{ background-color: rgb({br},{bgg},{bb}); color: rgb({fr},{fgg},{fb}); }}
             .top-bar-actions {{ background-color: rgb({br},{bgg},{bb}); }}
             .top-bar button {{ color: rgb({fr},{fgg},{fb}); }}
             .tab-strip {{ background-color: rgb({br},{bgg},{bb}); }}
             .tab-strip-btn {{ color: rgba({fr},{fgg},{fb},0.6); }}
             .tab-strip-btn:checked {{ color: rgb({fr},{fgg},{fb}); }}"
        );
        self.dyn_css.load_from_data(&css);
    }

    /// Move the tab strip into the holder matching the current placement and
    /// flip its orientation; sidebar = vertical list, top bar = horizontal.
    fn apply_tab_placement(&self) {
        use config::{SidebarView, TabPlacement};
        let placement = self.tab_placement.get();

        self.tab_strip_scroll.set_child(None::<&gtk::Widget>);
        self.top_tab_scroll.set_child(None::<&gtk::Widget>);

        match placement {
            TabPlacement::Sidebar => {
                self.tab_strip.set_orientation(gtk::Orientation::Vertical);
                self.tab_strip.set_valign(gtk::Align::Start);
                self.tab_strip.set_hexpand(false);
                self.tab_strip.set_vexpand(true);
                self.tab_strip.set_width_request(-1);
                self.tab_strip.remove_css_class("top-tabs");
                self.tab_strip_scroll.set_child(Some(&self.tab_strip));
            }
            TabPlacement::TopBar => {
                self.tab_strip.set_orientation(gtk::Orientation::Horizontal);
                self.tab_strip.set_valign(gtk::Align::Center);
                self.tab_strip.set_hexpand(true);
                self.tab_strip.set_vexpand(false);
                // Do not let the sum of every tab's minimum width become the
                // application's minimum width. The viewport owns overflow.
                self.tab_strip.set_width_request(1);
                self.tab_strip.add_css_class("top-tabs");
                self.top_tab_scroll.set_child(Some(&self.tab_strip));
            }
        }

        // Resize each existing strip row for the new orientation.
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            self.apply_strip_row_placement(&c);
            child = c.next_sibling();
        }

        // The Tabs sidebar view only makes sense when tabs live in the sidebar.
        match placement {
            TabPlacement::Sidebar => {
                self.sidebar_toggle
                    .emit(sidebar_toggle::SidebarToggleMsg::SetTabsEnabled(true));
                self.apply_sidebar_view(self.sidebar_view.get(), false);
            }
            TabPlacement::TopBar => {
                self.sidebar_toggle
                    .emit(sidebar_toggle::SidebarToggleMsg::SetTabsEnabled(false));
                self.apply_sidebar_view(SidebarView::Files, false);
            }
        }

        self.sync_tab_bar_visibility();
    }

    /// Show one sidebar view (tab list vs file tree) and reflect it in the
    /// segmented buttons. When `persist`, remember the choice in config.
    fn apply_sidebar_view(&self, view: config::SidebarView, persist: bool) {
        use config::SidebarView;
        match view {
            SidebarView::Tabs => self.sidebar_stack.set_visible_child_name("tabs"),
            SidebarView::Files => self.sidebar_stack.set_visible_child_name("files"),
        }
        self.sidebar_toggle
            .emit(sidebar_toggle::SidebarToggleMsg::SetView(view));

        if persist {
            self.sidebar_view.set(view);
            self.config.borrow_mut().sidebar_view = view;
            self.persist_config();
        }
    }

    /// Size tab rows for the active placement. Like jterm4, top-bar tabs use
    /// their natural label width rather than a shared fixed width.
    fn apply_strip_row_placement(&self, row: &gtk::Widget) {
        match self.tab_placement.get() {
            config::TabPlacement::Sidebar => {
                row.set_hexpand(true);
                let mut child = row.first_child();
                while let Some(widget) = child {
                    if let Ok(button) = widget.clone().downcast::<gtk::ToggleButton>() {
                        if button.has_css_class("tab-strip-btn") {
                            button.set_hexpand(true);
                            button.set_width_request(-1);
                        }
                    }
                    if widget.has_css_class("tab-resize-handle") {
                        widget.set_visible(false);
                    }
                    child = widget.next_sibling();
                }
            }
            config::TabPlacement::TopBar => {
                row.set_hexpand(false);
                let mut child = row.first_child();
                while let Some(widget) = child {
                    if let Ok(button) = widget.clone().downcast::<gtk::ToggleButton>() {
                        if button.has_css_class("tab-strip-btn") {
                            button.set_hexpand(false);
                            button.set_width_request(-1);
                        }
                    }
                    if widget.has_css_class("tab-resize-handle") {
                        widget.set_visible(false);
                    }
                    child = widget.next_sibling();
                }
            }
        }
    }

    /// Match jterm4: a lone tab needs no top-bar tab control.
    fn sync_tab_bar_visibility(&self) {
        match self.tab_placement.get() {
            config::TabPlacement::Sidebar => {
                self.tab_strip_scroll.set_visible(true);
                self.top_tab_scroll.set_visible(false);
            }
            config::TabPlacement::TopBar => {
                self.tab_strip_scroll.set_visible(true);
                self.top_tab_scroll.set_visible(self.tabs.len() > 1);
            }
        }
    }

    /// Update a tab's displayed title without replacing its button.
    ///
    /// Some terminal applications (notably Codex CLI) animate an OSC title by
    /// cycling a leading spinner glyph. Rebuilding the whole strip for every
    /// frame destroys the button between pointer press and release, so GTK
    /// never emits `clicked`. Keeping the existing widget alive also avoids a
    /// surprising amount of layout and session-persistence work.
    fn update_tab_title_widget(&self, id: u64, title: &str) -> bool {
        let Some(index) = self.tab_rows.iter().position(|row| row.id == id) else {
            return false;
        };
        self.tab_rows
            .send(index, tab_strip::TabRowMsg::SetTitle(title.to_string()));
        true
    }

    /// Flip the tab strip between the sidebar and the top bar, then persist.
    fn toggle_tab_placement(&self) {
        use config::TabPlacement;
        let next = match self.tab_placement.get() {
            TabPlacement::Sidebar => TabPlacement::TopBar,
            TabPlacement::TopBar => TabPlacement::Sidebar,
        };
        self.tab_placement.set(next);
        self.config.borrow_mut().tab_placement = next;
        self.apply_tab_placement();
        self.persist_config();
    }

    fn rebuild_tab_strip(&mut self, _sender: &ComponentSender<AppModel>) {
        let config = self.config.borrow();
        let remote_hosts: Vec<(u8, String)> = config
            .remote_hosts
            .iter()
            .take(u8::MAX as usize)
            .enumerate()
            .map(|(index, host)| (index as u8, host.name.clone()))
            .collect();
        let tab_width = config.tab_width;
        drop(config);

        let filter = self.tab_filter.to_lowercase();
        let sidebar = self.tab_placement.get() == config::TabPlacement::Sidebar;
        let rows: Vec<_> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| filter.is_empty() || tab.title.to_lowercase().contains(&filter))
            .map(|(index, tab)| tab_strip::TabRowInit {
                id: tab.id,
                target_index: index,
                title: tab.title.clone(),
                active: index == self.active,
                bell: tab.bell,
                activity: tab.activity,
                marked: tab.marked,
                pinned: tab.pinned,
                connection: tab.remote.as_ref().map(|remote| match remote.status {
                    ConnStatus::Connecting => tab_strip::ConnectionState::Connecting,
                    ConnStatus::Connected => tab_strip::ConnectionState::Connected,
                    ConnStatus::Disconnected => tab_strip::ConnectionState::Disconnected,
                }),
                remote_hosts: remote_hosts.clone(),
                tab_width,
                sidebar,
            })
            .collect();

        let mut factory = self.tab_rows.guard();
        factory.clear();
        for row in rows {
            factory.push_back(row);
        }
        drop(factory);
        self.sync_tab_bar_visibility();
        self.persist_session();
    }
}

#[allow(deprecated)]
fn install_static_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".tab-strip-btn { padding: 4px 8px; border-radius: 4px; margin-bottom: 2px; color: #ffffff; }
         .tab-strip-btn:checked { font-weight: bold; border: 1px solid currentColor; border-radius: 4px; }
         .tab-strip-close { min-width: 16px; min-height: 16px; color: #ffffff; }
         .tab-resize-handle { min-width: 6px; margin: 0 1px; border-left: 1px solid rgba(255,255,255,0.38); }
         .tab-resize-handle:hover { border-left-color: rgba(255,255,255,0.9); }
         .tab-strip { min-width: 140px; padding: 2px 4px; }
         .file-tree { padding: 2px; }
         .sidebar-toggle { color: #ffffff; }
         .top-bar { padding: 2px 4px; }
         .terminal-box scrollbar slider { min-width: 6px; border-radius: 3px; }
         .terminal-box scrollbar { padding: 0; }
         .tab-activity { font-style: italic; }
         .tab-bell { color: #f1fa8c; }
         .tab-marked { background-color: rgba(80,160,255,0.22); font-weight: bold; }
         .tab-pinned { background-color: rgba(255,200,80,0.18); }
         .conn-dot { margin: 0 4px; font-size: 9px; }
         .conn-connecting { color: #f1fa8c; }
         .conn-connected { color: #50fa7b; }
         .conn-disconnected { color: #ff5555; }
         .top-tabs { } .top-tabs .tab-row { margin-right: 2px; }
         .top-tab-scroll, .top-tab-scroll > viewport { min-width: 0; }
         .sidebar-toggle-row { margin-bottom: 2px; }
         .sidebar-toggle { padding: 2px 6px; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
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

        install_static_css();
        let dyn_css = gtk::CssProvider::new();
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &dyn_css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }

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

        // File tree browser (lower half of the sidebar).
        let file_tree_store = file_tree::new_store();
        let file_tree_view = file_tree::new_view(&file_tree_store);
        file_tree_view.add_css_class("file-tree");
        {
            // Lazy directory expansion: fill children on first expand.
            let store = file_tree_store.clone();
            file_tree_view.connect_row_expanded(move |_tv, iter, _path| {
                file_tree::on_expand(&store, iter);
            });
        }
        {
            // Activate: toggle directories, insert file paths into the terminal.
            let store = file_tree_store.clone();
            let sender = sender.clone();
            file_tree_view.connect_row_activated(move |tv, path, _col| {
                let Some(iter) = store.iter(path) else { return };
                let is_dir: bool = store
                    .get_value(&iter, file_tree::COL_IS_DIR as i32)
                    .get()
                    .unwrap_or(false);
                if is_dir {
                    if tv.row_expanded(path) {
                        tv.collapse_row(path);
                    } else {
                        tv.expand_row(path, false);
                    }
                    return;
                }
                let file_path: String = store
                    .get_value(&iter, file_tree::COL_PATH as i32)
                    .get()
                    .unwrap_or_default();
                if !file_path.is_empty() {
                    if file_path.ends_with(".jtnb.md") {
                        sender.input(AppMsg::OpenNotebook(std::path::PathBuf::from(file_path)));
                    } else {
                        sender.input(AppMsg::FileTreeActivateFile(file_path));
                    }
                }
            });
        }
        let file_tree_scroll = gtk::ScrolledWindow::new();
        file_tree_scroll.set_vexpand(true);
        file_tree_scroll.set_child(Some(&file_tree_view));
        let file_header = sidebar::FileHeaderModel::builder().launch(()).forward(
            sender.input_sender(),
            |output| match output {
                sidebar::FileHeaderOutput::Up => AppMsg::FileTreeGoUp,
                sidebar::FileHeaderOutput::CurrentDirectory => AppMsg::FileTreeGotoCwd,
            },
        );

        let sidebar_width = config.borrow().sidebar_width as i32;
        let tab_placement = config.borrow().tab_placement;
        let sidebar_view = config.borrow().sidebar_view;
        let sidebar_toggle = sidebar_toggle::SidebarToggleModel::builder()
            .launch((sidebar_view, tab_placement == config::TabPlacement::Sidebar))
            .forward(sender.input_sender(), |output| match output {
                sidebar_toggle::SidebarToggleOutput::View(view) => AppMsg::SetSidebarView(view),
            });

        // Scroll holders the tab strip can be reparented between (sidebar vs
        // top bar). apply_tab_placement() owns which one holds tab_strip.
        let tab_strip_scroll = gtk::ScrolledWindow::new();
        tab_strip_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        tab_strip_scroll.set_vexpand(true);
        let top_tab_scroll = gtk::ScrolledWindow::new();
        // The strip remains horizontally scrollable by touchpad/Shift+wheel,
        // but its scrollbar must not consume a second row in the title bar.
        top_tab_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        top_tab_scroll.set_hexpand(true);
        top_tab_scroll.set_vexpand(false);
        top_tab_scroll.set_overflow(gtk::Overflow::Hidden);
        // It is the only expanding item in the toolbar and must yield space
        // to the trailing New-tab / Close-window buttons as tabs accumulate.
        top_tab_scroll.set_width_request(0);
        top_tab_scroll.set_min_content_width(0);
        top_tab_scroll.set_max_content_width(1);
        top_tab_scroll.set_propagate_natural_width(false);
        top_tab_scroll.add_css_class("top-tab-scroll");
        top_tab_scroll.set_visible(false);

        top_tab_scroll.set_margin_start(128);
        // Reserve room for the leading controls and trailing window actions.
        top_tab_scroll.set_margin_end(104);
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
            AppMsg::SettingsTheme(idx) => {
                if let Some(theme) = self.themes.get(idx).cloned() {
                    {
                        let mut config = self.config.borrow_mut();
                        config.theme_name = theme.name.clone();
                        config.foreground = theme.foreground;
                        config.background = theme.background;
                        config.cursor = theme.cursor;
                        config.cursor_foreground = theme.cursor_foreground;
                        config.palette = theme.palette;
                    }
                    for tab in &self.tabs {
                        for pane in &tab.panes {
                            pane.terminal.emit(VteInput::ApplyTheme);
                        }
                    }
                    self.apply_dynamic_css();
                    self.persist_config();
                }
            }
            AppMsg::SettingsFontDesc(desc) => {
                self.config.borrow_mut().font_desc = desc.clone();
                for tab in &self.tabs {
                    for pane in &tab.panes {
                        pane.terminal.emit(VteInput::SetFont(desc.clone()));
                    }
                }
                self.persist_config();
            }
            AppMsg::SettingsFontScale(scale) => {
                self.set_font_scale_all(scale);
                self.config.borrow_mut().default_font_scale = scale;
                self.persist_config();
            }
            AppMsg::SettingsOpacity(opacity) => {
                self.set_window_opacity(opacity);
                self.config.borrow_mut().window_opacity = opacity;
                self.persist_config();
            }
            AppMsg::SettingsScrollback(lines) => {
                self.config.borrow_mut().terminal_scrollback_lines = lines;
                for tab in &self.tabs {
                    for pane in &tab.panes {
                        pane.terminal.emit(VteInput::SetScrollback(lines as i64));
                    }
                }
                self.persist_config();
            }
            AppMsg::SettingsTerminalMode(mode) => {
                self.config.borrow_mut().terminal_mode = if mode == 0 {
                    TerminalMode::Block
                } else {
                    TerminalMode::Vte
                };
                self.persist_config();
                self.show_toast("Terminal backend will apply to new local panes.");
            }
            AppMsg::SettingsBlockCompact(enabled) => {
                self.config.borrow_mut().block_compact = enabled;
                self.persist_config();
                self.show_toast("Block density will apply to new Block panes.");
            }
            AppMsg::SettingsCommandHistory(enabled) => {
                let mut config = self.config.borrow_mut();
                config.command_history_enabled = enabled;
                if enabled && config.command_history_path.is_none() {
                    config.command_history_path = Some(config::default_command_history_path());
                }
                drop(config);
                self.persist_config();
                self.show_toast("Command history preference will apply to new Block panes.");
            }
            AppMsg::SettingsAiEnabled(enabled) => {
                self.config.borrow_mut().ai_enabled = enabled;
                if !enabled {
                    self.agent_close();
                }
                self.persist_config();
            }
            AppMsg::SettingsAgentEnabled(enabled) => {
                self.config.borrow_mut().agent_enabled = enabled;
                if !enabled {
                    self.agent_close();
                }
                self.persist_config();
            }
            AppMsg::SettingsNotifications(enabled) => {
                self.config.borrow_mut().notify_long_blocks = enabled;
                self.persist_config();
                self.show_toast("Notification preference will apply to new Block panes.");
            }
            AppMsg::SettingsRemoteClipboard(enabled) => {
                self.config.borrow_mut().allow_remote_clipboard_write = enabled;
                self.persist_config();
                self.show_toast("Clipboard policy will apply to new panes.");
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
