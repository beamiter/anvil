//! Settings mutations and live propagation to existing panes.
//!
//! Settings continue to arrive as `AppMsg` variants through the same Relm4
//! component update loop. These methods only isolate the mutation and broadcast
//! details from `main.rs`.

use super::*;

impl AppModel {
    pub(crate) fn apply_settings_theme(&mut self, idx: usize) {
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
            self.sync_terminal_configs();
            for tab in &self.tabs {
                for pane in &tab.panes {
                    pane.terminal.emit(VteInput::ApplyTheme);
                }
            }
            self.apply_dynamic_css();
            self.persist_config();
        }
    }

    pub(crate) fn apply_settings_font_desc(&mut self, desc: String) {
        self.config.borrow_mut().font_desc = desc.clone();
        self.sync_terminal_configs();
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SetFont(desc.clone()));
            }
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_font_scale(&mut self, scale: f64) {
        self.stage_font_scale(scale);
        self.persist_config();
    }

    /// Apply a font scale to the live panes and the in-memory config without
    /// touching disk. Both the settings dialog and the hotkey/Ctrl+wheel path
    /// go through here; only the write policy differs.
    pub(crate) fn stage_font_scale(&mut self, scale: f64) {
        self.config.borrow_mut().default_font_scale = scale;
        self.sync_terminal_configs();
        self.set_font_scale_all(scale);
    }

    /// Hotkey / Ctrl+wheel path: apply immediately, write the config once the
    /// steps stop arriving. A wheel notch train would otherwise rewrite the
    /// config file on every notch.
    pub(crate) fn apply_font_scale_step(&mut self, scale: f64, sender: &ComponentSender<AppModel>) {
        self.stage_font_scale(scale);
        let generation = self.font_persist_generation.get().wrapping_add(1);
        self.font_persist_generation.set(generation);
        let token = Rc::clone(&self.font_persist_generation);
        let sender = sender.clone();
        glib::timeout_add_local_once(FONT_PERSIST_DEBOUNCE, move || {
            // A newer step superseded this one; it owns the write instead.
            if token.get() == generation {
                sender.input(AppMsg::PersistFontScale);
            }
        });
    }

    pub(crate) fn apply_settings_opacity(&mut self, opacity: f64) {
        self.set_window_opacity(opacity);
        self.config.borrow_mut().window_opacity = opacity;
        self.persist_config();
    }

    pub(crate) fn apply_settings_scrollback(&mut self, lines: u32) {
        self.config.borrow_mut().terminal_scrollback_lines = lines;
        self.sync_terminal_configs();
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SetScrollback(lines as i64));
            }
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_terminal_mode(&mut self, mode: usize) {
        if self.safe_mode {
            self.show_toast("Terminal mode is fixed to VTE in safe mode.");
            return;
        }
        self.config.borrow_mut().terminal_mode = match mode {
            0 => TerminalMode::Block,
            2 => TerminalMode::Unified,
            _ => TerminalMode::Vte,
        };
        self.persist_config();
        self.show_toast("Terminal backend will apply to new and restored local panes.");
    }

    pub(crate) fn apply_settings_block_compact(&mut self, enabled: bool) {
        self.config.borrow_mut().block_compact = enabled;
        self.sync_terminal_configs();
        for tab in &self.tabs {
            for pane in &tab.panes {
                // Not `ApplyTheme`: no color changed, and the density the cards
                // already on screen are drawn at is imperative margins that a
                // CSS reinstall cannot reach.
                pane.terminal.emit(VteInput::ApplyBlockDensity(enabled));
            }
        }
        self.persist_config();
        self.show_toast("Block density updated.");
    }

    pub(crate) fn apply_settings_command_history(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("Command history is disabled in safe mode.");
            return;
        }
        let mut config = self.config.borrow_mut();
        config.command_history_enabled = enabled;
        if enabled && config.command_history_path.is_none() {
            config.command_history_path = Some(config::default_command_history_path());
        }
        drop(config);
        self.sync_terminal_configs();
        self.persist_config();
        self.show_toast("Command history preference updated.");
    }

    pub(crate) fn apply_settings_ascii_organism(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("ASCII organism is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ascii_organism_enabled = enabled;
        self.persist_config();
        self.show_toast(if enabled {
            "ASCII organism will appear in new local Block panes."
        } else {
            "ASCII organism disabled for new panes."
        });
    }

    pub(crate) fn apply_settings_ascii_organism_motion(&mut self, motion: u32) {
        if self.safe_mode {
            self.show_toast("ASCII organism is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ascii_organism_motion = match motion {
            1 => Some(config::OrganismMotion::Full),
            2 => Some(config::OrganismMotion::Calm),
            3 => Some(config::OrganismMotion::Static),
            _ => None,
        };
        self.persist_config();
    }

    pub(crate) fn apply_settings_ai_enabled(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ai_enabled = enabled;
        self.sync_terminal_configs();
        if !enabled {
            self.set_ai_panel_visible(false, false);
            self.agent_close();
            self.close_command_suggestion();
            self.close_all_command_corrections();
        } else {
            self.sync_agent_toggle();
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_ai_panel_visible(&mut self, visible: bool) {
        if self.safe_mode || !self.config.borrow().ai_enabled {
            return;
        }
        if visible {
            self.show_ai_session_panel();
        } else {
            self.set_ai_panel_visible(false, true);
        }
    }

    pub(crate) fn apply_settings_ai_panel_width(&mut self, width: u32) {
        if self.safe_mode {
            return;
        }
        let width = width.clamp(MIN_AI_PANEL_WIDTH, MAX_AI_PANEL_WIDTH);
        self.config.borrow_mut().ai_panel_width = width;
        if self.ai_panel_visible.get() {
            self.restore_ai_panel_width();
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_agent_enabled(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("Shell Agent is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().agent_enabled = enabled;
        self.sync_terminal_configs();
        if !enabled {
            self.agent_close();
        } else {
            self.sync_agent_toggle();
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_command_correction(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("AI command correction is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().command_correction_enabled = enabled;
        if !enabled {
            self.close_all_command_corrections();
        }
        self.persist_config();
        self.show_toast(if enabled {
            "Review-first command correction enabled."
        } else {
            "Command correction disabled."
        });
    }

    pub(crate) fn apply_settings_ai_provider(&mut self, provider: usize) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ai_provider = match provider {
            1 => "openai-compatible",
            2 => "ollama",
            _ => "anthropic",
        }
        .to_string();
        self.persist_config();
    }

    pub(crate) fn apply_settings_ai_model(&mut self, model: String) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        self.config.borrow_mut().ai_model = model.to_string();
        self.persist_config();
    }

    /// The settings dialog already wrote the key file (0600, atomic); the app
    /// only records where it lives so future sessions read the same file.
    pub(crate) fn apply_settings_ai_key_file(&mut self, path: String) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        if path.trim().is_empty() {
            return;
        }
        self.config.borrow_mut().ai_api_key_file = Some(path);
        self.persist_config();
        self.show_toast("API key stored.");
    }

    pub(crate) fn apply_settings_ai_base_url(&mut self, base_url: String) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        let base_url = base_url.trim().trim_end_matches('/');
        let provider = self.config.borrow().ai_provider.clone();
        if !config::ai_base_url_is_safe(&provider, base_url) {
            self.show_toast(
                "AI endpoint must use HTTPS; HTTP is allowed only for loopback Ollama.",
            );
            return;
        }
        self.config.borrow_mut().ai_base_url = base_url.to_string();
        self.persist_config();
    }

    pub(crate) fn apply_settings_ai_max_tokens(&mut self, max_tokens: u32) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ai_max_tokens = max_tokens.clamp(64, 32_768);
        self.persist_config();
    }

    pub(crate) fn apply_settings_ai_redact_secrets(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ai_redact_secrets = enabled;
        self.persist_config();
    }

    /// Applies to the next request; an in-flight panel reply keeps the
    /// transport it started with.
    pub(crate) fn apply_settings_ai_stream(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ai_stream = enabled;
        self.persist_config();
    }

    pub(crate) fn apply_settings_agent_max_turns(&mut self, turns: u32) {
        if self.safe_mode {
            self.show_toast("Shell Agent is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().agent_max_turns = turns.clamp(1, 100);
        self.persist_config();
    }

    pub(crate) fn apply_settings_notifications(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("Notifications are disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().notify_long_blocks = enabled;
        self.sync_terminal_configs();
        self.persist_config();
        self.show_toast("Notification preference updated.");
    }

    /// The dialog already validated each entry against the parser's rules; the
    /// app replaces the whole list so removals persist too.
    pub(crate) fn apply_settings_remote_hosts(&mut self, hosts: Vec<config::RemoteHost>) {
        if self.safe_mode {
            self.show_toast("Remote host changes are not saved in safe mode.");
            return;
        }
        self.config.borrow_mut().remote_hosts = hosts;
        // A removed host must not keep driving the tree: fall back to Local.
        let stale = match &*self.file_tree_location.borrow() {
            remote_fs::FsLocation::Local => false,
            remote_fs::FsLocation::Remote(index) => {
                *index >= self.config.borrow().remote_hosts.len()
            }
        };
        if stale {
            *self.file_tree_location.borrow_mut() = remote_fs::FsLocation::Local;
            let root = file_tree::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
            self.set_file_tree_root(root);
        }
        self.sync_file_header_locations();
        self.persist_config();
    }

    pub(crate) fn apply_settings_remote_clipboard(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("Remote clipboard writes are disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().allow_remote_clipboard_write = enabled;
        self.sync_terminal_configs();
        self.persist_config();
        self.show_toast("Clipboard policy updated.");
    }
}
