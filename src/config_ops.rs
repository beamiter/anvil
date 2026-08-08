//! Configuration reload and dynamic appearance operations.
//!
//! These are inherent methods on the existing Relm4 `AppModel`. The module
//! separates configuration responsibilities without introducing another model,
//! controller framework, or event loop.

use super::*;

impl AppModel {
    /// Block panes keep a callback-safe configuration snapshot inside
    /// `TermView`; tell each backend to refresh it after the app-level value
    /// changes. Plain VTE panes already share the app's `Rc` and no-op.
    pub(crate) fn sync_terminal_configs(&self) {
        for tab in &self.tabs {
            for pane in &tab.panes {
                pane.terminal.emit(VteInput::SyncConfig);
            }
        }
    }

    pub(crate) fn reload_config(&mut self, _sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("Configuration reload is disabled in safe mode.");
            return;
        }
        if let Some(error) = config::config_file_error() {
            log::warn!("configuration reload rejected: {error}");
            self.show_toast(format!(
                "Config reload rejected; the current settings remain active. {error}"
            ));
            return;
        }
        let validation = config_store::validate_current_config();
        if validation.errors() > 0 {
            log::warn!(
                "configuration reload rejected: {} validation error(s)",
                validation.errors()
            );
            self.show_toast(format!(
                "Config reload rejected: {} validation error(s). Run `anvil --check-config`.",
                validation.errors()
            ));
            return;
        }
        let revision = match config_store::current_revision() {
            Ok(revision) => revision,
            Err(error) => {
                log::warn!("configuration reload rejected: {error}");
                self.show_toast(format!(
                    "Config reload rejected because its revision could not be read: {error}"
                ));
                return;
            }
        };
        // The file monitor also sees our own saves. Reloading those is a no-op
        // that still costs a full pane refresh and a toast, which is loud once
        // Ctrl+wheel writes the font scale on every zoom burst.
        if self.config_revision.borrow().as_ref() == Some(&revision) {
            log::debug!("configuration reload skipped: file matches the last save");
            return;
        }
        let (new_config, themes, new_kb) = load_config();
        let new_shell_argv = Rc::new(choose_shell_argv(new_config.shell.as_deref()));
        let backend_changed = std::mem::discriminant(&self.config.borrow().terminal_mode)
            != std::mem::discriminant(&new_config.terminal_mode);
        let tab_placement = new_config.tab_placement;
        let sidebar_view = new_config.sidebar_view;
        let sidebar_visible = new_config.sidebar_visible;
        let sidebar_width = new_config.sidebar_width.clamp(120, 800) as i32;
        *self.config.borrow_mut() = new_config.clone();
        *self.config_revision.borrow_mut() = Some(revision);
        self.shell_argv = new_shell_argv;
        self.sync_terminal_configs();
        if !new_config.ai_enabled {
            self.close_command_suggestion();
            self.close_all_command_corrections();
        } else if !new_config.command_correction_enabled {
            self.close_all_command_corrections();
        }
        if !new_config.ai_enabled || !new_config.agent_enabled {
            self.agent_close();
        } else {
            self.sync_agent_toggle();
            self.refresh_agent_panel();
        }

        self.set_window_opacity(new_config.window_opacity);
        self.tab_placement.set(tab_placement);
        self.sidebar_view.set(sidebar_view);
        self.sidebar_box.set_width_request(sidebar_width);
        self.content_paned.set_position(sidebar_width);
        self.apply_tab_placement();
        self.set_sidebar_visible(sidebar_visible, false);
        self.set_bottom_bar_visible(new_config.bottom_bar);
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
        self.sync_tab_strip();
        log::info!("Configuration reloaded from disk");
        if backend_changed {
            self.show_toast(
                "Terminal mode changed; it will apply to new and restored local panes.",
            );
        } else if validation.warnings() > 0 {
            self.show_toast(format!(
                "Configuration reloaded with {} warning(s).",
                validation.warnings()
            ));
        } else {
            self.show_toast("Configuration reloaded.");
        }
    }

    #[allow(deprecated)]
    pub(crate) fn apply_dynamic_css(&self) {
        let config = self.config.borrow();
        let bg = &config.background;
        let fg = &config.foreground;
        let br = (bg.red() * 255.0) as u8;
        let bgg = (bg.green() * 255.0) as u8;
        let bb = (bg.blue() * 255.0) as u8;
        let fr = (fg.red() * 255.0) as u8;
        let fgg = (fg.green() * 255.0) as u8;
        let fb = (fg.blue() * 255.0) as u8;
        // The bottom bar's positive/negative tones reuse the terminal
        // palette's ANSI green/red (jterm_core::bottom_bar's Tone contract).
        let ok = &config.palette[2];
        let err = &config.palette[1];
        let okr = (ok.red() * 255.0) as u8;
        let okg = (ok.green() * 255.0) as u8;
        let okb = (ok.blue() * 255.0) as u8;
        let er = (err.red() * 255.0) as u8;
        let eg = (err.green() * 255.0) as u8;
        let eb = (err.blue() * 255.0) as u8;
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
             .tab-strip-btn:checked {{ color: rgb({fr},{fgg},{fb}); }}
             .bottom-bar {{ background-color: rgb({br},{bgg},{bb}); color: rgb({fr},{fgg},{fb}); border-top-color: rgba({fr},{fgg},{fb},0.2); }}
             .bottom-bar .bb-muted {{ color: rgba({fr},{fgg},{fb},0.55); }}
             .bottom-bar .bb-ok {{ color: rgb({okr},{okg},{okb}); }}
             .bottom-bar .bb-err {{ color: rgb({er},{eg},{eb}); }}"
        );
        self.dyn_css.load_from_data(&css);
    }
}
