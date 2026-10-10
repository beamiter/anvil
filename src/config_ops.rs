//! Configuration reload and dynamic appearance operations.
//!
//! These are inherent methods on the existing Relm4 `AppModel`. The module
//! separates configuration responsibilities without introducing another model,
//! controller framework, or event loop.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReloadIntent {
    Monitor,
    Explicit,
}

impl ReloadIntent {
    fn skips_matching_revision(self, revision_matches: bool) -> bool {
        self == Self::Monitor && revision_matches
    }
}

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

    pub(crate) fn reload_config(&mut self, sender: &ComponentSender<AppModel>) {
        self.reload_config_with_intent(sender, ReloadIntent::Monitor);
    }

    pub(crate) fn reload_config_explicit(&mut self, sender: &ComponentSender<AppModel>) {
        self.reload_config_with_intent(sender, ReloadIntent::Explicit);
    }

    fn reload_config_with_intent(
        &mut self,
        sender: &ComponentSender<AppModel>,
        intent: ReloadIntent,
    ) {
        if self.safe_mode {
            self.show_toast("Configuration reload is disabled in safe mode.");
            return;
        }
        let snapshot = match config_store::read_validated_snapshot(&config::config_file_path()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::warn!("configuration reload rejected: {error}");
                self.show_toast(format!(
                    "Config reload rejected; the current settings remain active. {error}"
                ));
                return;
            }
        };
        let revision = snapshot.revision;
        let validation = snapshot.validation;
        // The file monitor also sees our own saves. Reloading those is a no-op
        // that still costs a full pane refresh and a toast, which is loud once
        // Ctrl+wheel writes the font scale on every zoom burst.
        // An explicit reload also discards live edits whose save failed (or
        // whose font-scale save is still debounced), even if disk is unchanged.
        // Validate the disk snapshot first so rejection keeps live state intact.
        if intent.skips_matching_revision(self.config_revision.borrow().as_ref() == Some(&revision))
        {
            log::debug!("configuration reload skipped: file matches the last save");
            return;
        }
        let (new_config, themes, new_kb) = config::load_config_from_table(snapshot.table.as_ref());
        let new_shell_argv = Rc::new(choose_shell_argv(new_config.shell.as_deref()));
        let backend_changed = std::mem::discriminant(&self.config.borrow().terminal_mode)
            != std::mem::discriminant(&new_config.terminal_mode);
        let tab_placement = new_config.tab_placement;
        let sidebar_view = new_config.sidebar_view;
        let sidebar_visible = new_config.sidebar_visible;
        let sidebar_width = new_config.sidebar_width.clamp(120, 800) as i32;
        let old_remote_hosts = self.config.borrow().remote_hosts.clone();
        *self.config.borrow_mut() = new_config.clone();
        *self.config_revision.borrow_mut() = Some(revision);
        self.shell_argv = new_shell_argv;
        self.reconcile_file_tree_remote_hosts(&old_remote_hosts, sender);
        self.sync_terminal_configs();
        self.organism_hub.sync_ascii_organism_settings();
        self.sync_ascii_organism_settings_dialog();
        self.sync_organism_focus();
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
        if new_config.ai_enabled && new_config.ai_panel_visible {
            self.show_ai_session_panel();
        } else {
            self.set_ai_panel_visible(false, false);
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
        let css = dynamic_css(&self.config.borrow());
        self.dyn_css.load_from_data(&css);
    }
}

/// The theme-derived rules loaded into the provider above the static one.
pub(crate) fn dynamic_css(config: &Config) -> String {
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
    // The bell badge is colour-only, so it has to outrank the tab colour
    // this provider sets (a higher-priority provider wins over the static
    // one's `.tab-bell` whatever the specificity), checked tab included:
    // with the window inactive the current tab is badged too.
    let warn = &config.palette[3];
    let wr = (warn.red() * 255.0) as u8;
    let wg = (warn.green() * 255.0) as u8;
    let wb = (warn.blue() * 255.0) as u8;
    format!(
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
         .bottom-bar .bb-err {{ color: rgb({er},{eg},{eb}); }}
         .tab-strip-btn.tab-bell, .tab-strip-btn.tab-bell:checked {{ color: rgb({wr},{wg},{wb}); }}"
    )
}

#[cfg(test)]
mod tests {
    use super::{ReloadIntent, dynamic_css};
    use crate::config::Config;

    #[test]
    fn explicit_reload_restores_unchanged_disk_after_an_unsaved_live_edit() {
        let disk = "saved A";
        let live = "unsaved B";
        let apply = |intent: ReloadIntent, revision_matches| {
            if intent.skips_matching_revision(revision_matches) {
                live
            } else {
                disk
            }
        };
        assert_eq!(apply(ReloadIntent::Monitor, true), live);
        assert_eq!(apply(ReloadIntent::Explicit, true), disk);
        assert_eq!(apply(ReloadIntent::Monitor, false), disk);
        assert_eq!(apply(ReloadIntent::Explicit, false), disk);
    }

    #[test]
    fn explicit_and_monitor_routes_validate_before_revision_or_live_state_changes() {
        let source = include_str!("config_ops.rs")
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("production source");
        assert!(source.contains("self.reload_config_with_intent(sender, ReloadIntent::Monitor)"));
        assert!(source.contains("self.reload_config_with_intent(sender, ReloadIntent::Explicit)"));
        let reload = source
            .split("    fn reload_config_with_intent(")
            .nth(1)
            .unwrap();
        let read = reload
            .find("config_store::read_validated_snapshot")
            .unwrap();
        let rejected = reload.find("Err(error) => {").unwrap();
        let rejection_return = reload[rejected..].find("return;").unwrap() + rejected;
        let dedup = reload.find("intent.skips_matching_revision").unwrap();
        let apply = reload
            .find("*self.config.borrow_mut() = new_config.clone()")
            .unwrap();
        let revision = reload
            .find("*self.config_revision.borrow_mut() = Some(revision)")
            .unwrap();
        assert!(read < rejected && rejection_return < dedup && dedup < apply && apply < revision);
        assert!(
            include_str!("action_ops.rs")
                .contains("Action::ReloadConfig => self.reload_config_explicit(sender)")
        );
        assert!(
            include_str!("main.rs").contains("AppMsg::ReloadConfig => self.reload_config(&sender)")
        );
    }

    #[test]
    fn queued_font_persistence_reads_current_config_without_a_stale_scale_payload() {
        let settings = include_str!("settings_ops.rs");
        let debounce = settings
            .split("let token = Rc::clone(&self.font_persist_generation)")
            .nth(1)
            .expect("font debounce generation");
        let callback = debounce.split("});").next().unwrap();
        assert!(callback.contains("token.get() == generation"));
        assert!(callback.contains("sender.input(AppMsg::PersistFontScale)"));
        assert!(!callback.contains("scale"));
        let main = include_str!("main.rs");
        assert!(main.contains("AppMsg::PersistFontScale => self.persist_config()"));
        let workspace = include_str!("workspace_ops.rs");
        let persist = workspace.split("fn persist_config(&self)").nth(1).unwrap();
        assert!(persist.contains("let config = self.config.borrow()"));
        assert!(persist.contains("config_store::save_config(&config, expected.as_ref())"));
    }

    #[test]
    fn the_bell_badge_outranks_the_tab_colour_checked_or_not() {
        let css = dynamic_css(&Config::safe_defaults());
        let tab_colour = css.find(".tab-strip-btn:checked").expect("tab colour");
        let bell = css
            .find(".tab-strip-btn.tab-bell, .tab-strip-btn.tab-bell:checked")
            .expect("bell rule in the dynamic provider");
        assert!(bell > tab_colour);
    }

    /// What the user sees: the static and dynamic providers together, read
    /// back from a checked tab button that rang. Needs a display (`make
    /// test-display`).
    #[test]
    #[ignore = "requires a GTK display"]
    #[allow(deprecated)]
    fn a_checked_tab_that_rang_is_drawn_in_the_bell_colour() {
        use relm4::gtk::{self, prelude::*};

        gtk::init().expect("GTK display");
        crate::startup_ui::install_static_css();
        let config = Config::safe_defaults();
        crate::startup_ui::install_dynamic_css_provider().load_from_data(&dynamic_css(&config));
        let window = gtk::Window::new();
        let bell = gtk::ToggleButton::with_label("codex");
        bell.add_css_class("tab-strip-btn");
        bell.add_css_class("tab-bell");
        bell.set_active(true);
        let plain = gtk::ToggleButton::with_label("zsh");
        plain.add_css_class("tab-strip-btn");
        plain.set_active(true);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.append(&bell);
        row.append(&plain);
        window.set_child(Some(&row));
        let warn = config.palette[3];
        let close = |a: f32, b: f32| (a - b).abs() < 0.01;
        let colour = bell.color();
        assert!(
            close(colour.red(), warn.red())
                && close(colour.green(), warn.green())
                && close(colour.blue(), warn.blue()),
            "{colour:?} vs {warn:?}"
        );
        assert_ne!(plain.color(), colour);
        window.destroy();
    }
}
