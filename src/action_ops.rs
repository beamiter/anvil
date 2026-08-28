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

    /// Hotkey feedback: show the current opacity as a percentage. Repeat
    /// presses update the toast in place rather than queueing one per step.
    fn show_opacity_toast(&self) {
        let message = format!("Opacity: {:.0}%", self.window_opacity * 100.0);
        if let Some(toast) = self.opacity_toast.borrow().as_ref() {
            toast.set_title(&message);
            return;
        }
        let toast = adw::Toast::new(&message);
        toast.set_timeout(2);
        let slot = Rc::clone(&self.opacity_toast);
        toast.connect_dismissed(move |_| {
            slot.borrow_mut().take();
        });
        *self.opacity_toast.borrow_mut() = Some(toast.clone());
        self.toast_overlay.add_toast(toast);
    }

    pub(crate) fn toggle_search(&mut self) {
        self.search.emit(search::SearchMsg::Toggle);
    }

    fn ai_panel_contains_focus(&self) -> bool {
        self.ai_panel.widget().is_visible()
            && gtk::prelude::RootExt::focus(&self.window).is_some_and(|focus| {
                widget_is_within(focus, self.ai_panel.widget().upcast_ref::<gtk::Widget>())
            })
    }

    fn emit_block_action(&self, input: VteInput, feature: &str) {
        let pane = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane));
        match pane {
            Some(pane) if pane.mode.uses_term_view() => {
                pane.terminal.emit(input);
            }
            Some(_) => {
                self.show_toast(format!("{feature} is available only in a Block-mode pane."))
            }
            None => self.show_toast("No active terminal pane."),
        }
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
                if self.ai_panel_contains_focus() {
                    self.ai_panel
                        .emit(dialogs::ai_panel::AiPanelMsg::CopyFocused);
                } else if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::Copy);
                }
            }
            Action::Paste => {
                if self.ai_panel_contains_focus() {
                    self.ai_panel
                        .emit(dialogs::ai_panel::AiPanelMsg::PasteFocused);
                } else if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::Paste);
                }
            }
            Action::FontIncrease => {
                let s = (self.font_scale + FONT_STEP).min(10.0);
                // Same apply-and-persist path as the settings dialog, so the
                // hotkey survives restarts like it does in ember/frost.
                self.apply_font_scale_step(s, sender);
            }
            Action::FontDecrease => {
                let s = (self.font_scale - FONT_STEP).max(0.1);
                self.apply_font_scale_step(s, sender);
            }
            Action::FontReset => self.apply_font_scale_step(1.0, sender),
            Action::OpacityIncrease => {
                let o = (self.window_opacity + OPACITY_STEP).clamp(0.01, 1.0);
                // Same apply-and-persist path as the settings dialog, so the
                // hotkey survives restarts like it does in ember/frost.
                self.apply_settings_opacity(o);
                self.show_opacity_toast();
            }
            Action::OpacityDecrease => {
                let o = (self.window_opacity - OPACITY_STEP).clamp(0.01, 1.0);
                self.apply_settings_opacity(o);
                self.show_opacity_toast();
            }
            Action::ToggleSidebar => {
                self.set_sidebar_visible(!self.sidebar_visible, true);
            }
            Action::ToggleCommandPalette => {
                self.refresh_workflows_async(sender);
                let history = self.config.borrow().command_history_path.clone();
                let live_history = self
                    .active_terminal()
                    .map(TermCtl::command_history)
                    .unwrap_or_default();
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::Toggle {
                        // Match Forge's command center: the default entry point
                        // searches actions, history, workflows and AI together.
                        // Prefixes still narrow the same surface immediately.
                        mode: palette::PaletteMode::All,
                        history_path: history.map(std::path::PathBuf::from),
                        live_history,
                    });
            }
            Action::OpenPalette => {
                self.refresh_workflows_async(sender);
                let history = self.config.borrow().command_history_path.clone();
                let live_history = self
                    .active_terminal()
                    .map(TermCtl::command_history)
                    .unwrap_or_default();
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::Toggle {
                        mode: palette::PaletteMode::All,
                        history_path: history.map(std::path::PathBuf::from),
                        live_history,
                    });
            }
            Action::OpenHistoryPalette => {
                self.refresh_workflows_async(sender);
                let history = self.config.borrow().command_history_path.clone();
                let live_history = self
                    .active_terminal()
                    .map(TermCtl::command_history)
                    .unwrap_or_default();
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::Toggle {
                        mode: palette::PaletteMode::History,
                        history_path: history.map(std::path::PathBuf::from),
                        live_history,
                    });
            }
            Action::OpenWorkflows => {
                self.refresh_workflows_async(sender);
                let history = self.config.borrow().command_history_path.clone();
                let live_history = self
                    .active_terminal()
                    .map(TermCtl::command_history)
                    .unwrap_or_default();
                self.command_palette
                    .emit(dialogs::command_palette::PaletteMsg::Toggle {
                        mode: palette::PaletteMode::Workflows,
                        history_path: history.map(std::path::PathBuf::from),
                        live_history,
                    });
            }
            Action::ToggleSettings => {
                let config = self.config.borrow();
                let font_desc = gtk::pango::FontDescription::from_string(&config.font_desc);
                let family = font_desc
                    .family()
                    .map(|family| family.to_string())
                    .unwrap_or_default();
                let (font_names, font) = dialogs::settings::font_choices(
                    self.settings_font_names.as_ref().clone(),
                    &family,
                );
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
                            TerminalMode::Unified => 2,
                        },
                        block_compact: config.block_compact,
                        command_history: config.command_history_enabled,
                        ascii_organism_enabled: config.ascii_organism_enabled,
                        ascii_organism_motion: match config.ascii_organism_motion {
                            None => 0,
                            Some(config::OrganismMotion::Full) => 1,
                            Some(config::OrganismMotion::Calm) => 2,
                            Some(config::OrganismMotion::Static) => 3,
                        },
                        ai_enabled: config.ai_enabled,
                        ai_panel_visible: config.ai_panel_visible,
                        ai_panel_width: config.ai_panel_width as f64,
                        agent_enabled: config.agent_enabled,
                        command_correction_enabled: config.command_correction_enabled,
                        ai_provider: match config.ai_provider.as_str() {
                            "openai-compatible" => 1,
                            "ollama" => 2,
                            _ => 0,
                        },
                        ai_model: config.ai_model.clone(),
                        ai_base_url: config.ai_base_url.clone(),
                        ai_api_key_file: config.ai_api_key_file.clone(),
                        ai_max_tokens: config.ai_max_tokens as f64,
                        ai_redact_secrets: config.ai_redact_secrets,
                        ai_stream: config.ai_stream,
                        agent_max_turns: config.agent_max_turns as f64,
                        safe_mode: self.safe_mode,
                        notifications: config.notify_long_blocks,
                        remote_clipboard: config.allow_remote_clipboard_write,
                        remote_hosts: config.remote_hosts.clone(),
                    },
                    font_names,
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
                            "Welcome notebook was not found. Reinstall anvil's shared assets.",
                        ),
                    }
                }
            }
            Action::InstallJsh => self.install_or_update_jsh(sender),
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
                // The tab list is reachable from the sidebar in both
                // placements, so filtering can always take the user there.
                self.set_sidebar_visible(true, true);
                self.apply_sidebar_view(config::SidebarView::Tabs, true);
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
                self.emit_block_action(VteInput::FilterFailedBlocks, "Failed-block navigation");
            }
            Action::FilterSlowBlocks => {
                self.emit_block_action(VteInput::FilterSlowBlocks, "Slow-block navigation");
            }
            Action::FilterPinnedBlocks => {
                self.emit_block_action(VteInput::FilterPinnedBlocks, "Bookmarked-block navigation");
            }
            Action::JumpToPrevPinned => {
                self.emit_block_action(VteInput::JumpToPrevPinned, "Bookmarked-block navigation");
            }
            Action::JumpToNextPinned => {
                self.emit_block_action(VteInput::JumpToNextPinned, "Bookmarked-block navigation");
            }
            Action::JumpToPrevFailed => {
                self.emit_block_action(VteInput::JumpToPrevFailed, "Failed-block navigation");
            }
            Action::JumpToNextFailed => {
                self.emit_block_action(VteInput::JumpToNextFailed, "Failed-block navigation");
            }
            Action::ExportSessionMarkdown => {
                self.emit_block_action(VteInput::ExportSessionMarkdown, "Session export");
            }
            Action::ExportSessionJson => {
                self.emit_block_action(VteInput::ExportSessionJson, "Session export");
            }
            Action::ClearBlockFilter => {
                self.emit_block_action(VteInput::ClearBlockFilter, "Block navigation");
            }
            Action::SelectAllBlocks => {
                self.emit_block_action(VteInput::SelectAllBlocks, "Block selection");
            }
            Action::ClearBlocks => {
                self.emit_block_action(VteInput::ClearBlocks, "Clearing finished blocks");
            }
            Action::UndoClearBlocks => {
                self.emit_block_action(VteInput::UndoClearBlocks, "Restoring cleared blocks");
            }
            Action::CollapseAllBlocks => {
                self.emit_block_action(VteInput::CollapseAllBlocks, "Block folding");
            }
            Action::ExpandAllBlocks => {
                self.emit_block_action(VteInput::ExpandAllBlocks, "Block folding");
            }
            Action::ToggleBlockCollapsed => {
                self.emit_block_action(VteInput::ToggleBlockCollapsed, "Block folding");
            }
            Action::ReinputSelectedCommands => {
                self.emit_block_action(
                    VteInput::ReinputSelectedCommands,
                    "Selected-command recall",
                );
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
                if self.safe_mode {
                    self.show_toast("Remote connections are disabled in safe mode.");
                    return;
                }
                // Gate borrowed runtime objects before cloning or deriving
                // picker text. Keep the original config index so skipping an
                // invalid draft cannot redirect activation to another host.
                let hosts: Vec<_> = self
                    .config
                    .borrow()
                    .remote_hosts
                    .iter()
                    .take(config::MAX_REMOTE_HOSTS)
                    .enumerate()
                    .filter_map(|(index, host)| match config::validate_remote_host(host) {
                        Ok(()) => Some((index, host.clone())),
                        Err(_) => None,
                    })
                    .collect();
                if hosts.is_empty() {
                    self.show_toast(format!(
                        "No remote hosts are configured. Add [[remote_hosts]] in {}.",
                        config_file_path().display()
                    ));
                } else {
                    self.remote_picker
                        .emit(dialogs::remote_picker::RemotePickerMsg::Toggle(hosts));
                }
            }
            Action::ToggleDebugDashboard => {
                let info = self.debug_info_snapshot();
                self.debug_dashboard
                    .emit(dialogs::debug_dashboard::DebugDashboardMsg::Toggle(info));
            }
            Action::ConnectRemote(n) => {
                if self.safe_mode {
                    self.show_toast("Remote connections are disabled in safe mode.");
                    return;
                }
                let host = {
                    let config = self.config.borrow();
                    config::checked_remote_host(&config.remote_hosts, n as usize).cloned()
                };
                match host {
                    Ok(host) => self.add_remote_tab(&host, sender),
                    Err(message) => self.show_toast(message),
                }
            }
            Action::ToggleAiPanel | Action::OpenAiPanel => {
                match action.ai_panel_target_visibility(self.ai_panel_visible.get()) {
                    Some(true) => self.show_ai_session_panel(),
                    Some(false) => self.set_ai_panel_visible(false, true),
                    None => unreachable!("matched only AI-panel actions"),
                }
            }
            Action::AskAiAboutSelectedBlock => {
                self.emit_block_action(
                    VteInput::AskAiAboutSelectedBlock,
                    "AI context for a finished block",
                );
            }
            Action::OpenAgent => {
                self.open_agent_panel(sender);
            }
            Action::ToggleTasksPanel => {
                self.toggle_tasks_panel(sender);
            }
            Action::CrossBlockSearch => {
                self.emit_block_action(VteInput::CrossBlockSearch, "Cross-block search");
            }
        }
    }
}
