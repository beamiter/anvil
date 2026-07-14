from pathlib import Path
import re

main_path = Path("src/main.rs")
text = main_path.read_text()

if "mod action_ops;\n" not in text:
    text = text.replace("mod agent;\n", "mod action_ops;\nmod agent;\n", 1)
if "mod settings_ops;\n" not in text:
    text = text.replace("mod session;\n", "mod session;\nmod settings_ops;\n", 1)

# Extract the contiguous action-dispatch implementation. These remain inherent
# methods on the same Relm4 AppModel; only their source file changes.
start = text.find("    fn set_font_scale_all(")
end = text.find("    fn reload_config(", start)
if start < 0 or end < 0:
    raise SystemExit("action operation markers not found")
action_block = text[start:end]
action_block = re.sub(r"^    fn ", "    pub(crate) fn ", action_block, flags=re.MULTILINE)
action_module = '''//! Keyboard action dispatch and live view controls.\n//!\n//! This is an inherent implementation of the existing Relm4 `AppModel`. The\n//! extraction changes file ownership only; `Component::update` remains the single\n//! application message loop.\n\nuse super::*;\n\nimpl AppModel {\n''' + action_block + "}\n"
Path("src/action_ops.rs").write_text(action_module)
text = text[:start] + text[end:]

# Replace the large settings arms with focused method calls. Keeping the message
# variants in AppMsg preserves the current Relm4 input contract.
settings_start = text.find("            AppMsg::SettingsTheme(idx) => {")
settings_end = text.find("            AppMsg::SearchChanged(text) => {", settings_start)
if settings_start < 0 or settings_end < 0:
    raise SystemExit("settings dispatch markers not found")
settings_dispatch = '''            AppMsg::SettingsTheme(idx) => self.apply_settings_theme(idx),
            AppMsg::SettingsFontDesc(desc) => self.apply_settings_font_desc(desc),
            AppMsg::SettingsFontScale(scale) => self.apply_settings_font_scale(scale),
            AppMsg::SettingsOpacity(opacity) => self.apply_settings_opacity(opacity),
            AppMsg::SettingsScrollback(lines) => self.apply_settings_scrollback(lines),
            AppMsg::SettingsTerminalMode(mode) => self.apply_settings_terminal_mode(mode),
            AppMsg::SettingsBlockCompact(enabled) => self.apply_settings_block_compact(enabled),
            AppMsg::SettingsCommandHistory(enabled) => {
                self.apply_settings_command_history(enabled)
            }
            AppMsg::SettingsAiEnabled(enabled) => self.apply_settings_ai_enabled(enabled),
            AppMsg::SettingsAgentEnabled(enabled) => self.apply_settings_agent_enabled(enabled),
            AppMsg::SettingsNotifications(enabled) => {
                self.apply_settings_notifications(enabled)
            }
            AppMsg::SettingsRemoteClipboard(enabled) => {
                self.apply_settings_remote_clipboard(enabled)
            }
'''
text = text[:settings_start] + settings_dispatch + text[settings_end:]
main_path.write_text(text)

settings_module = r'''//! Settings mutations and live propagation to existing panes.
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
        self.config.borrow_mut().ai_enabled = enabled;
        if !enabled {
            self.agent_close();
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_agent_enabled(&mut self, enabled: bool) {
        self.config.borrow_mut().agent_enabled = enabled;
        if !enabled {
            self.agent_close();
        }
        self.persist_config();
    }

    pub(crate) fn apply_settings_notifications(&mut self, enabled: bool) {
        self.config.borrow_mut().notify_long_blocks = enabled;
        self.persist_config();
        self.show_toast("Notification preference will apply to new Block panes.");
    }

    pub(crate) fn apply_settings_remote_clipboard(&mut self, enabled: bool) {
        self.config.borrow_mut().allow_remote_clipboard_write = enabled;
        self.persist_config();
        self.show_toast("Clipboard policy will apply to new panes.");
    }
}
'''
Path("src/settings_ops.rs").write_text(settings_module)
