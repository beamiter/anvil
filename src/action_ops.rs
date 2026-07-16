//! Keyboard action dispatch and live view controls.
//!
//! This is an inherent implementation of the existing Relm4 `AppModel`. The
//! extraction changes file ownership only; `Component::update` remains the single
//! application message loop.

use super::*;

impl AppModel {
    pub(crate) fn set_font_scale_all(&mut self, scale: f64) {
        self.font_scale = scale;
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SetFontScale(scale));
            }
        }
    }

    pub(crate) fn set_window_opacity(&mut self, opacity: f64) {
        self.window_opacity = opacity;
        self.window.set_opacity(opacity);
    }

    pub(crate) fn toggle_search(&mut self) {
        self.search.emit(search::SearchMsg::Toggle);
    }

    /// Parse the find-bar text: `/pattern/` means regex, anything else literal.
    pub(crate) fn search_query(text: &str) -> (String, bool) {
        if text.starts_with('/') && text.ends_with('/') && text.len() > 2 {
            (text[1..text.len() - 1].to_string(), true)
        } else {
            (text.to_string(), false)
        }
    }

    pub(crate) fn execute_action(&mut self, action: Action, sender: &ComponentSender<AppModel>) {
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
            Action::FontReset => self.set_font_scale_all(1.0),
            Action::OpacityIncrease => {
                let o = (self.window_opacity + OPACITY_STEP).clamp(0.01, 1.0);
                self.set_window_opacity(o);
            }
            Action::OpacityDecrease => {
                let o = (self.window_opacity - OPACITY_STEP).clamp(0.01, 1.0);
                self.set_window_opacity(o);
            }
            Action::ToggleSidebar => {
                self.set_sidebar_visible(!self.sidebar_visible, true);
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
                        ai_provider: match config.ai_provider.as_str() {
                            "openai-compatible" => 1,
                            "ollama" => 2,
                            _ => 0,
                        },
                        ai_model: config.ai_model.clone(),
                        ai_base_url: config.ai_base_url.clone(),
                        ai_max_tokens: config.ai_max_tokens as f64,
                        ai_redact_secrets: config.ai_redact_secrets,
                        agent_max_turns: config.agent_max_turns as f64,
                        safe_mode: self.safe_mode,
                        notifications: config.notify_long_blocks,
                        remote_clipboard: config.allow_remote_clipboard_write,
                    },
                    self.window.clone(),
                ));
            }
            Action::OpenWelcome => {
                if self.safe_mode {
                    self.show_toast("Notebooks are unavailable in safe mode.");
                } else {
                    match workflows::welcome_notebook_path() {
                        Some(path) => self.notebook.emit(notebook::NotebookMsg::Open(path)),
                        None => self.show_toast(
                            "Welcome notebook was not found. Reinstall jterm1's shared assets.",
                        ),
                    }
                }
            }
            Action::ToggleSearch => self.toggle_search(),
            Action::ReloadConfig => self.reload_config(sender),
            Action::MoveTabLeft => self.move_tab(-1, sender),
            Action::MoveTabRight => self.move_tab(1, sender),
            Action::DuplicateTab => self.duplicate_active_tab(sender),
            Action::ToggleTabMarked => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.marked = !tab.marked;
                }
                self.sync_tab_strip();
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
                self.set_sidebar_visible(true, true);
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
            Action::SelectAllBlocks => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::SelectAllBlocks);
                }
            }
            Action::ClearBlocks => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ClearBlocks);
                }
            }
            Action::ReinputSelectedCommands => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::ReinputSelectedCommands);
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
            Action::AskAiAboutSelectedBlock => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.emit(VteInput::AskAiAboutSelectedBlock);
                }
            }
            Action::OpenAgent => {
                self.open_agent_panel(sender);
            }
            Action::CrossBlockSearch => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.emit(VteInput::CrossBlockSearch);
                }
            }
        }
    }
}
