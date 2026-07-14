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
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SetFont(desc.clone()));
            }
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_font_scale(&mut self, scale: f64) {
        self.set_font_scale_all(scale);
        self.config.borrow_mut().default_font_scale = scale;
        self.persist_config();
    }

    pub(crate) fn apply_settings_opacity(&mut self, opacity: f64) {
        self.set_window_opacity(opacity);
        self.config.borrow_mut().window_opacity = opacity;
        self.persist_config();
    }

    pub(crate) fn apply_settings_scrollback(&mut self, lines: u32) {
        self.config.borrow_mut().terminal_scrollback_lines = lines;
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
        self.config.borrow_mut().terminal_mode = if mode == 0 {
            TerminalMode::Block
        } else {
            TerminalMode::Vte
        };
        self.persist_config();
        self.show_toast("Terminal backend will apply to new local panes.");
    }

    pub(crate) fn apply_settings_block_compact(&mut self, enabled: bool) {
        self.config.borrow_mut().block_compact = enabled;
        self.persist_config();
        self.show_toast("Block density will apply to new Block panes.");
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
        self.persist_config();
        self.show_toast("Command history preference will apply to new Block panes.");
    }

    pub(crate) fn apply_settings_ai_enabled(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("AI is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().ai_enabled = enabled;
        if !enabled {
            self.agent_close();
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_agent_enabled(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("AI Agent is disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().agent_enabled = enabled;
        if !enabled {
            self.agent_close();
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_notifications(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("Notifications are disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().notify_long_blocks = enabled;
        self.persist_config();
        self.show_toast("Notification preference will apply to new Block panes.");
    }

    pub(crate) fn apply_settings_remote_clipboard(&mut self, enabled: bool) {
        if self.safe_mode {
            self.show_toast("Remote clipboard writes are disabled in safe mode.");
            return;
        }
        self.config.borrow_mut().allow_remote_clipboard_write = enabled;
        self.persist_config();
        self.show_toast("Clipboard policy will apply to new panes.");
    }
}
