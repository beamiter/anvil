//! Workspace, tab, pane, session-restore, and remote-connection operations.
//!
//! This remains an inherent `AppModel` implementation inside the same Relm4
//! component. The extraction only separates responsibilities from `main.rs`; it
//! does not introduce another model, message loop, or UI framework.

use super::*;

/// Detach one terminal leaf, remove its immediate split, and promote the sibling
/// into the validated grandparent slot. Validation happens before mutation so a
/// malformed widget tree cannot leave the model and GTK hierarchy out of sync.
fn detach_leaf_and_promote(holder: &gtk::Box, leaf: &gtk::Widget) -> Option<gtk::Widget> {
    enum Destination {
        PanedStart(gtk::Paned),
        PanedEnd(gtk::Paned),
        Holder,
    }

    let parent = leaf.parent()?.downcast::<gtk::Paned>().ok()?;
    let start = parent.start_child();
    let end = parent.end_child();
    let sibling = if start.as_ref() == Some(leaf) {
        end?
    } else if end.as_ref() == Some(leaf) {
        start?
    } else {
        return None;
    };

    let parent_widget = parent.clone().upcast::<gtk::Widget>();
    let grandparent = parent_widget.parent()?;
    let holder_widget = holder.clone().upcast::<gtk::Widget>();
    let destination = if grandparent == holder_widget {
        Destination::Holder
    } else if let Ok(grandparent) = grandparent.downcast::<gtk::Paned>() {
        if grandparent.start_child().as_ref() == Some(&parent_widget) {
            Destination::PanedStart(grandparent)
        } else if grandparent.end_child().as_ref() == Some(&parent_widget) {
            Destination::PanedEnd(grandparent)
        } else {
            return None;
        }
    } else {
        return None;
    };

    parent.set_start_child(None::<&gtk::Widget>);
    parent.set_end_child(None::<&gtk::Widget>);
    match destination {
        Destination::PanedStart(grandparent) => grandparent.set_start_child(Some(&sibling)),
        Destination::PanedEnd(grandparent) => grandparent.set_end_child(Some(&sibling)),
        Destination::Holder => {
            holder.remove(&parent_widget);
            holder.append(&sibling);
        }
    }
    Some(sibling)
}

/// Pane indices in visual order: depth-first through the `Paned` tree, start
/// child before end child.
///
/// The `panes` Vec keeps creation order, which stops describing the layout as
/// soon as two panes are swapped. Headers number themselves from the widget
/// tree so "pane 2" is always the second one the user sees.
fn visual_pane_order(tab: &Tab) -> Vec<usize> {
    fn walk(widget: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        match widget.clone().downcast::<gtk::Paned>() {
            Ok(paned) => {
                if let Some(start) = paned.start_child() {
                    walk(&start, out);
                }
                if let Some(end) = paned.end_child() {
                    walk(&end, out);
                }
            }
            Err(leaf) => out.push(leaf),
        }
    }

    let mut leaves = Vec::new();
    if let Some(root) = tab.holder.first_child() {
        walk(&root, &mut leaves);
    }
    let mut order: Vec<usize> = leaves
        .iter()
        .filter_map(|leaf| tab.panes.iter().position(|pane| pane.widget() == *leaf))
        .collect();
    // A zoomed tab has only the focused leaf in the tree, and a pane detached
    // mid-operation has none. Append whatever the walk missed so every pane
    // still gets a stable number rather than none at all.
    for index in 0..tab.panes.len() {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    order
}

/// Working directory with `$HOME` collapsed to `~`, for the pane header.
fn abbreviate_home(path: &str) -> String {
    match std::env::var_os("HOME") {
        Some(home) => abbreviate_prefix(path, &home.to_string_lossy()),
        None => path.to_string(),
    }
}

/// The substitution itself, with `home` supplied rather than read from the
/// environment so it is testable without mutating process-wide state.
fn abbreviate_prefix(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    // Only at a component boundary: `/home/user2` merely shares a prefix with
    // `/home/user` and is a different directory.
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Header title for one pane: its OSC title, else its directory's last
/// component, else a positional fallback.
fn pane_header_title(osc_title: Option<&str>, cwd: Option<&str>, position: usize) -> String {
    if let Some(title) = osc_title.map(str::trim) {
        if !title.is_empty() {
            return title.to_string();
        }
    }
    cwd.map(abbreviate_home)
        .filter(|cwd| !cwd.is_empty())
        .map(|cwd| {
            // `~` and `/` have no last component worth showing on their own.
            std::path::Path::new(&cwd)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or(cwd)
        })
        .unwrap_or_else(|| format!("Pane {}", position + 1))
}

fn active_index_after_remove(active: usize, removed: usize, remaining: usize) -> usize {
    debug_assert!(remaining > 0);
    if active > removed {
        active - 1
    } else {
        active.min(remaining - 1)
    }
}

fn restored_leaf_mode(configured: TerminalMode, remote_integrated: bool) -> TerminalMode {
    if remote_integrated {
        TerminalMode::Block
    } else {
        configured
    }
}

fn snapshot_restorable_command(
    managed_remote: bool,
    detected: Option<Vec<String>>,
) -> Option<Vec<String>> {
    (!managed_remote).then_some(detected).flatten()
}

/// This path is reached only after a managed profile lookup failed. A snapshot
/// from an older build may still contain its expanded SSH argv; do not execute
/// that stale command after the user removed or renamed the authoritative
/// profile.
fn replay_argv_for_unmanaged_leaf<'a>(
    remote_name: Option<&str>,
    commands: Option<&'a [String]>,
) -> Option<&'a [String]> {
    remote_name.is_none().then_some(commands).flatten()
}

fn format_running_process_summary(mut running: Vec<String>) -> Option<String> {
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

fn running_process_summary_for_tabs<'a>(tabs: impl IntoIterator<Item = &'a Tab>) -> Option<String> {
    let mut running = Vec::new();
    for tab in tabs {
        for (pane_index, pane) in tab.panes.iter().enumerate() {
            if let Some(process) = pane.foreground_process() {
                let location = if tab.panes.len() > 1 {
                    format!("{} (pane {})", tab.title, pane_index + 1)
                } else {
                    tab.title.clone()
                };
                running.push(format!("{location} — {process}"));
            }
        }
    }
    format_running_process_summary(running)
}

impl AppModel {
    pub(crate) fn add_tab(
        &mut self,
        initial_commands: Option<String>,
        sender: &ComponentSender<AppModel>,
    ) {
        // New tabs inherit the active pane's working directory (matches
        // DuplicateTab), so Ctrl+Shift+T opens where the user already is.
        let cwd = self
            .tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .and_then(Pane::local_cwd)
            .map(str::to_string);
        self.add_tab_with(
            InitialCommands::from_config(initial_commands.as_deref()),
            cwd,
            self.shell_argv.clone(),
            sender,
        );
    }

    pub(crate) fn add_tab_with(
        &mut self,
        initial_commands: InitialCommands,
        working_directory: Option<String>,
        shell_argv: Rc<Vec<String>>,
        sender: &ComponentSender<AppModel>,
    ) {
        self.add_tab_full(
            initial_commands,
            working_directory,
            shell_argv,
            None,
            sender,
        );
    }

    /// Launch an explicit argv in its own named tab, in conventional VTE mode.
    ///
    /// Used for one-shot helpers such as the jsh installer: they emit no
    /// shell-integration sequences, so Block mode would have nothing to build
    /// blocks from, and their prompts expect a plain terminal to type into.
    pub(crate) fn add_command_tab(
        &mut self,
        title: &str,
        argv: Vec<String>,
        sender: &ComponentSender<AppModel>,
    ) {
        self.add_tab_full(
            InitialCommands::default(),
            None,
            Rc::new(argv),
            Some((TerminalMode::Vte, title.to_string())),
            sender,
        );
    }

    /// Shared body: `command` forces a terminal mode and a fixed tab title,
    /// which is what one-shot helper tabs need and ordinary tabs must not have.
    fn add_tab_full(
        &mut self,
        initial_commands: InitialCommands,
        working_directory: Option<String>,
        shell_argv: Rc<Vec<String>>,
        command: Option<(TerminalMode, String)>,
        sender: &ComponentSender<AppModel>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let number = self.tabs.len() as u32 + 1;
        let mode = command
            .as_ref()
            .map(|(mode, _)| *mode)
            .unwrap_or_else(|| self.config.borrow().terminal_mode);
        let title_cwd = working_directory.clone();
        let pane = create_pane(
            &self.config,
            &shell_argv,
            id,
            pane_id,
            mode,
            initial_commands,
            working_directory,
            None,
            false,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.widget());
        self.stack.add_named(&holder, Some(&id.to_string()));
        let tab = Tab {
            holder,
            panes: vec![pane],
            active_pane: 0,
            title: command
                .as_ref()
                .map(|(_, title)| title.clone())
                .unwrap_or_else(|| default_tab_title(number, title_cwd.as_deref())),
            custom_title: command.is_some(),
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
    pub(crate) fn insert_tab_after_active(&mut self, tab: Tab) {
        let insert_at = self.active.saturating_add(1).min(self.tabs.len());
        self.tabs.insert(insert_at, tab);
    }

    /// Recreate a tab from a persisted snapshot, rebuilding the full nested
    /// `Paned` split tree and replaying any restorable command per pane.
    pub(crate) fn restore_tab(
        &mut self,
        saved: &session::SavedTab,
        sender: &ComponentSender<AppModel>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let mut panes = Vec::new();
        let mut restored_remote = None;
        let root_widget =
            self.build_pane_layout(&saved.layout, id, &mut panes, &mut restored_remote, sender);
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
            pinned: saved.pinned,
            id,
            zoom: None,
            remote: restored_remote,
        };
        self.tabs.push(tab);
    }

    /// Recursively build the GTK widget tree for a persisted `PaneLayout`,
    /// pushing each created leaf into `panes` in tree order.
    ///
    /// Pane mode used to be persisted with the session.  That made a mode
    /// change in config appear to have no effect: restoring an old VTE pane
    /// recreated it as VTE even when `terminal_mode = "block"`.  The current
    /// configuration is the authority for every newly-created local backend,
    /// including restored panes; remote-integrated restores keep Block mode so
    /// OSC session metadata remains available. The snapshot otherwise restores
    /// only layout and shell state.
    pub(crate) fn build_pane_layout(
        &mut self,
        node: &session::PaneLayout,
        tab_id: u64,
        panes: &mut Vec<Pane>,
        restored_remote: &mut Option<RemoteConn>,
        sender: &ComponentSender<AppModel>,
    ) -> gtk::Widget {
        self.build_pane_layout_node(node, tab_id, panes, restored_remote, sender)
    }

    fn build_pane_layout_node(
        &mut self,
        node: &session::PaneLayout,
        tab_id: u64,
        panes: &mut Vec<Pane>,
        restored_remote: &mut Option<RemoteConn>,
        sender: &ComponentSender<AppModel>,
    ) -> gtk::Widget {
        match node {
            session::PaneLayout::Leaf {
                cwd,
                cwd_external,
                remote_name,
                sid,
                cmds,
                ..
            } => {
                let pane_id = self.next_pane_id;
                self.next_pane_id += 1;
                let managed_host = remote_name.as_deref().and_then(|name| {
                    self.config
                        .borrow()
                        .remote_hosts
                        .iter()
                        .find(|host| host.name == name)
                        .cloned()
                });
                if let Some(mut host) = managed_host {
                    let restored_sid = sid
                        .as_deref()
                        .filter(|value| config::valid_session_id(value))
                        .map(str::to_string);
                    if let Some(restored_sid) = restored_sid.as_ref() {
                        host.session = Some(restored_sid.clone());
                    } else if sid.is_some() {
                        log::warn!("Ignoring invalid session id in managed remote snapshot");
                    }
                    let shell_argv = Rc::new(config::build_remote_argv(&host));
                    let pane = create_pane(
                        &self.config,
                        &shell_argv,
                        tab_id,
                        pane_id,
                        TerminalMode::Block,
                        InitialCommands::default(),
                        None,
                        restored_sid,
                        true,
                        sender,
                    );
                    if restored_remote.is_none() {
                        *restored_remote = Some(RemoteConn {
                            host,
                            pane_id,
                            status: ConnStatus::Connecting,
                            attempt: 0,
                            spawn_at: std::time::Instant::now(),
                        });
                    }
                    let widget = pane.widget();
                    panes.push(pane);
                    return widget;
                } else if let Some(name) = remote_name {
                    log::warn!(
                        "Managed remote '{name}' is no longer configured; restoring a local shell without replaying stale connection data"
                    );
                    self.show_toast(format!(
                        "Remote profile “{name}” was removed or renamed; its saved connection was not restored."
                    ));
                }
                let missing_managed_remote = remote_name.is_some();
                let replay_argv =
                    replay_argv_for_unmanaged_leaf(remote_name.as_deref(), cmds.as_deref());
                // The current configuration remains authoritative for restored
                // local backends. Remote-integrated panes stay on Block because
                // their OSC cwd/session/reconnect signals are part of the
                // restore contract even in VTE compatibility mode.
                let external_cwd = missing_managed_remote
                    || *cwd_external
                    || replay_argv.is_some_and(process::command_uses_external_cwd);
                let remote_integrated = !missing_managed_remote
                    && (sid.is_some()
                        || replay_argv.is_some_and(process::command_requires_block_integration));
                let mode =
                    restored_leaf_mode(self.config.borrow().terminal_mode, remote_integrated);
                // OSC 7 from ssh/mosh/container shells reports a path in that
                // remote namespace. It must neither be passed as a local spawn
                // cwd nor suppress safe argv replay when absent on this host.
                let cwd_available = external_cwd
                    || cwd
                        .as_deref()
                        .is_none_or(crate::host::working_directory_available);
                if !cwd_available {
                    log::warn!(
                        "Restored working directory is unavailable; skipping its command replay"
                    );
                }
                let pane = create_pane(
                    &self.config,
                    &self.shell_argv,
                    tab_id,
                    pane_id,
                    mode,
                    if cwd_available {
                        InitialCommands::from_restored_argv(replay_argv, self.shell_argv.as_ref())
                    } else {
                        InitialCommands::default()
                    },
                    if external_cwd || !cwd_available {
                        None
                    } else {
                        cwd.clone()
                    },
                    if missing_managed_remote {
                        None
                    } else {
                        sid.clone()
                    },
                    external_cwd,
                    sender,
                );
                let widget = pane.widget();
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
                let start_w =
                    self.build_pane_layout_node(start, tab_id, panes, restored_remote, sender);
                let end_w =
                    self.build_pane_layout_node(end, tab_id, panes, restored_remote, sender);
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
    pub(crate) fn serialize_layout(&self, tab: &Tab) -> session::PaneLayout {
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
                cwd_external: false,
                remote_name: None,
                sid: None,
                cmds: None,
            },
        }
    }

    pub(crate) fn serialize_widget(&self, tab: &Tab, widget: &gtk::Widget) -> session::PaneLayout {
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
            let pane = tab.panes.iter().find(|p| p.widget() == *widget);
            let (mode, cwd, cwd_external, remote_name, sid, cmds) = match pane {
                Some(p) => {
                    let managed_remote =
                        tab.remote.as_ref().filter(|remote| remote.pane_id == p.id);
                    let cmds = snapshot_restorable_command(
                        managed_remote.is_some(),
                        if managed_remote.is_some() {
                            None
                        } else {
                            p.restorable_command()
                        },
                    );
                    let cwd_external = p.cwd_external
                        || cmds
                            .as_deref()
                            .is_some_and(process::command_uses_external_cwd);
                    (
                        match p.mode {
                            TerminalMode::Vte => "vte",
                            TerminalMode::Block => "block",
                        }
                        .to_string(),
                        p.cwd.clone(),
                        cwd_external,
                        managed_remote.map(|remote| remote.host.name.clone()),
                        p.session_id.clone(),
                        cmds,
                    )
                }
                None => ("block".to_string(), None, false, None, None, None),
            };
            session::PaneLayout::Leaf {
                mode,
                cwd,
                cwd_external,
                remote_name,
                sid,
                cmds,
            }
        }
    }

    /// A `Paned` child, substituting the zoomed-out pane when its slot is empty.
    pub(crate) fn resolve_child(
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
            cwd_external: false,
            remote_name: None,
            sid: None,
            cmds: None,
        }
    }

    /// Capture the current tab list as a persistable snapshot, including each
    /// tab's full split layout.
    pub(crate) fn snapshot_session(&self) -> session::SavedSession {
        let tabs = self
            .tabs
            .iter()
            .map(|t| session::SavedTab {
                title: t.title.clone(),
                custom_title: t.custom_title,
                pinned: t.pinned,
                layout: self.serialize_layout(t),
            })
            .collect();
        session::SavedSession {
            active: self.active,
            tabs,
        }
    }

    pub(crate) fn persist_session(&self) {
        if self.session_persistence {
            session::save_session(&self.snapshot_session());
        }
    }

    pub(crate) fn show_toast(&self, message: impl AsRef<str>) {
        self.toast_overlay
            .add_toast(adw::Toast::new(message.as_ref()));
    }

    pub(crate) fn persist_config(&self) {
        if self.safe_mode {
            self.show_toast("Settings are temporary and are not saved in safe mode.");
            return;
        }
        let expected = self.config_revision.borrow().clone();
        let result = {
            let config = self.config.borrow();
            config_store::save_config(&config, expected.as_ref())
        };
        match result {
            Ok(revision) => {
                *self.config_revision.borrow_mut() = Some(revision);
            }
            Err(error) if error.is_conflict() => {
                log::warn!("settings save conflict: {error}");
                self.show_toast(
                    "Settings were not saved because the config changed elsewhere. The newer file will reload automatically; reapply your change.",
                );
            }
            Err(error) => {
                log::error!("{error}");
                self.show_toast(format!("Settings were not saved: {error}"));
            }
        }
    }

    pub(crate) fn running_process_summary(&self) -> Option<String> {
        running_process_summary_for_tabs(&self.tabs)
    }

    pub(crate) fn request_quit(&self, sender: &ComponentSender<AppModel>) {
        if let Some(running) = self.running_process_summary() {
            dialogs::confirm_close(&self.window, &running, AppMsg::ForceQuit, sender);
        } else {
            sender.input(AppMsg::ForceQuit);
        }
    }

    pub(crate) fn force_quit(&self) {
        if !self.safe_mode {
            let width = self.content_paned.position().clamp(120, 800) as u32;
            let changed = {
                let config = self.config.borrow();
                config.sidebar_width != width || config.sidebar_visible != self.sidebar_visible
            };
            if changed {
                let mut config = self.config.borrow_mut();
                config.sidebar_width = width;
                config.sidebar_visible = self.sidebar_visible;
                drop(config);
                self.persist_config();
            }
        }
        self.persist_session();
        self.persist_agent_session();
        if let Err(error) = command_history::flush_pending(std::time::Duration::from_secs(3)) {
            log::warn!("flush command history on exit: {error}");
        }
        self.quit_allowed.set(true);
        self.window.close();
    }

    /// App-level diagnostics plus the active Block backend's PTY/viewport state.
    pub(crate) fn debug_info_snapshot(&self) -> Vec<(String, Vec<(String, String)>)> {
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
        let mut info = vec![
            ("Session".to_string(), session),
            ("Appearance".to_string(), appearance),
            ("Config".to_string(), config),
        ];
        if let Some(block_info) = self.active_terminal().and_then(TermCtl::block_debug_info) {
            info.extend(
                block_info
                    .into_iter()
                    .map(|(section, rows)| (format!("Block · {section}"), rows)),
            );
        }
        info
    }

    /// Open a new tab that connects to a remote host via ssh. Uses block mode
    /// so OSC 133 / 7 / 7770 from the remote jsh drive the block UI; for a remote
    /// shell without OSC 133, block.rs falls back to a streaming raw view, which
    /// is no worse than the bare-VTE path this used to take.
    pub(crate) fn add_remote_tab(
        &mut self,
        host: &config::RemoteHost,
        sender: &ComponentSender<AppModel>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let argv = Rc::new(config::build_remote_argv(host));
        // Remote sessions need OSC 133/7/7770 parsing for blocks, cwd updates,
        // resumable session ids, and Agent observations. Keep them on the Block
        // backend even when the local compatibility backend is configured.
        let mode = TerminalMode::Block;
        let pane = create_pane(
            &self.config,
            &argv,
            id,
            pane_id,
            mode,
            InitialCommands::default(),
            None,
            None,
            true,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.widget());
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
                pane_id,
                status: ConnStatus::Connecting,
                attempt: 0,
                spawn_at: std::time::Instant::now(),
            }),
        };
        self.insert_tab_after_active(tab);
        self.select_tab(id, sender);
    }

    /// Flip a Connecting remote tab to Connected (first output/cwd seen).
    pub(crate) fn mark_remote_connected(&mut self, idx: usize, pane_id: u64) -> bool {
        if let Some(conn) = self.tabs[idx]
            .remote
            .as_mut()
            .filter(|conn| conn.pane_id == pane_id)
        {
            if conn.status != ConnStatus::Connected {
                conn.status = ConnStatus::Connected;
                return true;
            }
        }
        false
    }

    /// If `tab_id` is a single-pane remote tab that died abnormally, start a
    /// backoff countdown and reconnect in place; returns true when handled (the
    /// caller should NOT close the tab). A clean exit (code 0) returns false so
    /// the tab closes normally.
    pub(crate) fn schedule_remote_reconnect(
        &mut self,
        pane_id: u64,
        code: i32,
        sender: &ComponentSender<AppModel>,
    ) -> bool {
        const MAX_ATTEMPT: u32 = 6;
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return false;
        };
        if !reconnect_target_is_valid(
            self.tabs[idx].panes.len(),
            self.tabs[idx].zoom.is_some(),
            self.tabs[idx].remote.as_ref().map(|conn| conn.pane_id),
            pane_id,
        ) {
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
                s.input(AppMsg::RemoteReconnectTick(pane_id, left - 1));
                glib::ControlFlow::Continue
            } else {
                s.input(AppMsg::RemoteReconnectNow(pane_id, next_attempt));
                glib::ControlFlow::Break
            }
        });
        true
    }

    /// Respawn a dead remote tab's connection in place (same tab id / position).
    pub(crate) fn do_remote_reconnect(
        &mut self,
        pane_id: u64,
        attempt: u32,
        sender: &ComponentSender<AppModel>,
    ) {
        if !self.remote_reconnect_target_is_valid(pane_id) {
            self.cancel_remote_reconnect(pane_id, sender);
            return;
        }
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        let Some(conn) = self.tabs[idx].remote.clone() else {
            return;
        };
        // Swap the dead pane widget for a fresh remote pane.
        let old_widget = self.tabs[idx].panes[0].widget();
        self.tabs[idx].holder.remove(&old_widget);
        let new_pane_id = self.next_pane_id;
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
            self.tabs[idx].id,
            new_pane_id,
            mode,
            InitialCommands::default(),
            None,
            None,
            true,
            sender,
        );
        self.tabs[idx].holder.append(&pane.widget());
        self.tabs[idx].panes = vec![pane];
        self.tabs[idx].active_pane = 0;
        self.tabs[idx].title = host_now.name.clone();
        if let Some(c) = self.tabs[idx].remote.as_mut() {
            c.pane_id = new_pane_id;
            c.status = ConnStatus::Connecting;
            c.attempt = attempt;
            c.spawn_at = std::time::Instant::now();
        }
        if self.active == idx {
            self.tabs[idx].panes[0].terminal.emit(VteInput::GrabFocus);
        }
        self.rebuild_tab_strip(sender);
    }

    /// Revalidate the reconnect ownership at every timer tick and immediately
    /// before respawn. A split or moved/replaced leaf must never be overwritten
    /// by a stale reconnect timer.
    pub(crate) fn remote_reconnect_target_is_valid(&self, pane_id: u64) -> bool {
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return false;
        };
        reconnect_target_is_valid(
            self.tabs[idx].panes.len(),
            self.tabs[idx].zoom.is_some(),
            self.tabs[idx].remote.as_ref().map(|conn| conn.pane_id),
            pane_id,
        )
    }

    /// Cancel a stale reconnect and remove only its dead remote leaf. Live
    /// siblings created while the countdown was running remain untouched.
    pub(crate) fn cancel_remote_reconnect(
        &mut self,
        pane_id: u64,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some((idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        if self.tabs[idx]
            .remote
            .as_ref()
            .is_some_and(|conn| conn.pane_id == pane_id)
        {
            self.tabs[idx].remote = None;
        }
        self.close_pane(pane_id, sender);
    }

    /// Stable-partition the tab list so pinned tabs sort to the front, keeping
    /// `self.active` pointing at the same tab.
    pub(crate) fn reorder_pinned_first(&mut self) {
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        self.tabs.sort_by_key(|t| !t.pinned);
        if let Some(id) = active_id {
            if let Some(idx) = self.index_of(id) {
                self.active = idx;
            }
        }
    }

    pub(crate) fn select_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
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
        self.refresh_pane_headers(idx);
        self.rebuild_tab_strip(sender);
        self.refresh_bottom_bar();
    }

    pub(crate) fn close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        let Some(idx) = self.index_of(id) else { return };
        let tab = self.tabs.remove(idx);
        self.stack.remove(&tab.holder);
        drop(tab);

        if self.tabs.is_empty() {
            self.force_quit();
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

    /// Re-label a tab from the pane it currently has selected.
    ///
    /// A tab shows its selected pane, so moving focus between the panes of a
    /// split has to move the label with it. Without this the strip kept naming
    /// whichever pane last reported an OSC title. A tab the user renamed keeps
    /// that name.
    pub(crate) fn retitle_tab_from_active_pane(
        &mut self,
        ti: usize,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(tab) = self.tabs.get(ti) else {
            return;
        };
        if tab.custom_title {
            return;
        }
        let Some(pane) = tab.panes.get(tab.active_pane) else {
            return;
        };
        let title = pane
            .title
            .clone()
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| default_tab_title(ti as u32 + 1, pane.cwd.as_deref()));
        if tab.title == title {
            return;
        }
        let id = tab.id;
        self.tabs[ti].title = title;
        // A filter is matched against the label, so a changed label can move
        // the row in or out of the filtered set; that needs a full rebuild.
        let filter = self.tab_filter.to_lowercase();
        let visible = filter.is_empty() || self.tabs[ti].title.to_lowercase().contains(&filter);
        if !visible || !self.update_tab_title_widget(id, &self.tabs[ti].title) {
            self.rebuild_tab_strip(sender);
        }
    }

    /// Bring one tab's pane headers up to date: visibility, numbering, focus
    /// highlight, and the title / directory / running-command line.
    ///
    /// A tab with a single pane hides its header entirely — the tab strip and
    /// window title already name it, and the strip would only cost a row.
    pub(crate) fn refresh_pane_headers(&self, ti: usize) {
        let Some(tab) = self.tabs.get(ti) else {
            return;
        };
        let split = tab.panes.len() > 1;
        for (position, &pane_index) in visual_pane_order(tab).iter().enumerate() {
            let Some(pane) = tab.panes.get(pane_index) else {
                continue;
            };
            pane.frame.set_header_visible(split);
            pane.frame.set_focused(pane_index == tab.active_pane);
            if !split {
                continue;
            }
            let title = pane_header_title(pane.title.as_deref(), pane.cwd.as_deref(), position);
            let cwd = pane.cwd.as_deref().map(abbreviate_home);
            pane.frame.set_status(
                position,
                &title,
                cwd.as_deref(),
                pane.foreground_process().as_deref(),
            );
        }
    }

    /// Refresh the headers of the tab the user is looking at. Background tabs
    /// are not rendered, so polling their PTYs would be pure waste.
    pub(crate) fn refresh_active_pane_headers(&self) {
        self.refresh_pane_headers(self.active);
    }

    /// Exchange two panes' positions in the split tree after a header drag.
    ///
    /// Only the panes move: the tree shape and every divider position the user
    /// arranged stay exactly as they were, and focus follows the dragged pane
    /// into its new slot.
    pub(crate) fn swap_panes(&mut self, dragged: u64, target: u64) {
        let (Some((ti, di)), Some((tj, tj_index))) =
            (self.find_pane(dragged), self.find_pane(target))
        else {
            return;
        };
        // A cross-tab drop would have to move a pane between two widget trees
        // and two tab identities; refuse rather than half-apply it.
        if ti != tj || di == tj_index || self.tabs[ti].zoom.is_some() {
            return;
        }
        let dragged_widget = self.tabs[ti].panes[di].widget();
        let target_widget = self.tabs[ti].panes[tj_index].widget();
        if !crate::pane_header::swap_pane_widgets(&dragged_widget, &target_widget) {
            return;
        }
        self.tabs[ti].active_pane = di;
        self.tabs[ti].panes[di].terminal.emit(VteInput::GrabFocus);
        self.refresh_pane_headers(ti);
    }

    /// Foreground processes running in a tab's panes, formatted for one
    /// confirmation dialog. Ordinary shells are omitted by the PTY probe.
    pub(crate) fn tab_running_process_summary(&self, idx: usize) -> Option<String> {
        running_process_summary_for_tabs(std::iter::once(self.tabs.get(idx)?))
    }

    /// Close a tab, first confirming if a process is still running in it.
    pub(crate) fn request_close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        if let Some(idx) = self.index_of(id) {
            if let Some(running) = self.tab_running_process_summary(idx) {
                dialogs::confirm_close(&self.window, &running, AppMsg::ForceCloseTab(id), sender);
                return;
            }
        }
        self.close_tab(id, sender);
    }

    /// Close a pane, first confirming if a process is still running in it.
    pub(crate) fn request_close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        if let Some((ti, pi)) = self.find_pane(pane_id) {
            if let Some(process) = self.tabs[ti].panes[pi].foreground_process() {
                dialogs::confirm_close(
                    &self.window,
                    &process,
                    AppMsg::ForceClosePane(pane_id),
                    sender,
                );
                return;
            }
        }
        self.close_pane(pane_id, sender);
    }

    /// Move the tab with `src_id` to `to_idx`, preserving which tab is active.
    pub(crate) fn reorder_tab(
        &mut self,
        src_id: u64,
        to_idx: usize,
        sender: &ComponentSender<AppModel>,
    ) {
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

    pub(crate) fn switch_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
        if self.tabs.is_empty() {
            return;
        }
        let len = self.tabs.len() as i32;
        let idx = ((self.active as i32 + delta) % len + len) % len;
        let id = self.tabs[idx as usize].id;
        self.select_tab(id, sender);
    }

    /// Reorder the active tab one slot left (-1) or right (+1) and keep it active.
    pub(crate) fn move_tab(&mut self, delta: i32, sender: &ComponentSender<AppModel>) {
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
    pub(crate) fn duplicate_active_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let Some(src) = self.tabs.get(self.active) else {
            return;
        };
        let cwd = src
            .panes
            .get(src.active_pane)
            .and_then(Pane::local_cwd)
            .map(str::to_string);
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
            InitialCommands::default(),
            cwd,
            None,
            false,
            sender,
        );
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&pane.widget());
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

    /// Close every marked tab (marking is the multi-select model in anvil).
    pub(crate) fn close_marked_tabs(&mut self, sender: &ComponentSender<AppModel>) {
        let ids: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.marked)
            .map(|t| t.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let selected: std::collections::HashSet<u64> = ids.iter().copied().collect();
        if let Some(running) = running_process_summary_for_tabs(
            self.tabs.iter().filter(|tab| selected.contains(&tab.id)),
        ) {
            // Capture the current selection in the confirmation message. A
            // cancellation closes nothing and leaves every mark intact; one
            // confirmation closes the whole captured set without modal spam.
            dialogs::confirm_close(
                &self.window,
                &running,
                AppMsg::ForceCloseMarked(ids),
                sender,
            );
        } else {
            self.close_tabs(ids, sender);
        }
    }

    /// Remove a captured set of tab ids as one workspace mutation. This keeps
    /// the current tab selected when possible and rebuilds/persists once.
    pub(crate) fn close_tabs(&mut self, ids: Vec<u64>, sender: &ComponentSender<AppModel>) {
        if ids.is_empty() {
            return;
        }
        let selected: std::collections::HashSet<u64> = ids.into_iter().collect();
        let Some(first_removed) = self.tabs.iter().position(|tab| selected.contains(&tab.id))
        else {
            return;
        };
        let active_id = self.tabs.get(self.active).map(|tab| tab.id);
        let mut index = 0;
        while index < self.tabs.len() {
            if selected.contains(&self.tabs[index].id) {
                let tab = self.tabs.remove(index);
                self.stack.remove(&tab.holder);
                drop(tab);
            } else {
                index += 1;
            }
        }
        if self.tabs.is_empty() {
            self.force_quit();
            return;
        }
        let new_id = active_id
            .filter(|id| self.index_of(*id).is_some())
            .unwrap_or_else(|| self.tabs[first_removed.min(self.tabs.len() - 1)].id);
        self.select_tab(new_id, sender);
    }

    pub(crate) fn find_pane(&self, pane_id: u64) -> Option<(usize, usize)> {
        for (ti, tab) in self.tabs.iter().enumerate() {
            if let Some(pi) = tab.panes.iter().position(|p| p.id == pane_id) {
                return Some((ti, pi));
            }
        }
        None
    }

    /// Split the active pane using the configured local terminal backend.
    pub(crate) fn split_active(
        &mut self,
        orientation: gtk::Orientation,
        sender: &ComponentSender<AppModel>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.zoom.is_some() {
            return;
        }
        let ti = self.active;
        let tab_id = tab.id;
        let api = tab.active_pane;
        let cur_widget = tab.panes[api].widget();
        let wd = tab.panes[api].local_cwd().map(str::to_string);
        let mode = self.config.borrow().terminal_mode;

        let pane_id = self.next_pane_id;
        self.next_pane_id += 1;
        let new_pane = create_pane(
            &self.config,
            &self.shell_argv,
            tab_id,
            pane_id,
            mode,
            InitialCommands::default(),
            wd,
            None,
            false,
            sender,
        );
        let new_widget = new_pane.widget();

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
        // The tab just became split, so every pane's header appears now.
        self.refresh_pane_headers(ti);
    }

    /// Remove a pane from its tab, collapsing the Paned tree and promoting the
    /// sibling. Closes the whole tab if it was the last pane.
    pub(crate) fn close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
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
        let was_remote = self.tabs[ti]
            .remote
            .as_ref()
            .is_some_and(|conn| conn.pane_id == pane_id);
        let Some(removed) = self.detach_pane_from_tab(ti, pi) else {
            log::error!("Failed to detach pane {pane_id} from tab widget tree");
            return;
        };
        let tab = &mut self.tabs[ti];
        if was_remote {
            tab.remote = None;
        }
        let ap = tab.active_pane;
        tab.panes[ap].terminal.emit(VteInput::GrabFocus);
        drop(removed);
        // Numbering shifted, and dropping back to one pane hides the headers.
        self.refresh_pane_headers(ti);
    }

    pub(crate) fn cycle_pane_focus(&mut self, delta: i32) {
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
        self.refresh_active_pane_headers();
    }

    pub(crate) fn focus_pane_directional(&mut self, direction: Direction) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.panes.len() <= 1 {
            return;
        }
        let holder: gtk::Widget = tab.holder.clone().upcast();
        let api = tab.active_pane;
        let focused_widget = tab.panes[api].widget();
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
            let w = pane.widget();
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

    pub(crate) fn resize_pane(&mut self, target: gtk::Orientation, delta: i32) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let api = tab.active_pane;
        let mut widget = tab.panes[api].widget().parent();
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

    pub(crate) fn toggle_pane_zoom(&mut self) {
        self.toggle_pane_zoom_for(self.active);
    }

    pub(crate) fn toggle_pane_zoom_for(&mut self, ti: usize) {
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
            let pane_widget = tab.panes[api].widget();
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
        // Zooming leaves only one leaf in the tree, so pane numbering has to
        // be recomputed in both directions.
        self.refresh_pane_headers(ti);
    }

    /// Detach the active pane from a split tab and host it in a brand-new tab.
    pub(crate) fn move_pane_to_new_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let (ti, pi, pane_id, moves_remote) = {
            let Some(tab) = self.tabs.get(self.active) else {
                return;
            };
            if tab.panes.len() <= 1 || tab.zoom.is_some() {
                return;
            }
            let pane_id = tab.panes[tab.active_pane].id;
            (
                self.active,
                tab.active_pane,
                pane_id,
                tab.remote
                    .as_ref()
                    .is_some_and(|conn| conn.pane_id == pane_id),
            )
        };
        let Some(moved) = self.detach_pane_from_tab(ti, pi) else {
            log::error!("Failed to detach pane {pane_id} into a new tab");
            return;
        };

        let remote = moves_remote.then(|| self.tabs[ti].remote.take()).flatten();
        let new_id = self.next_id;
        self.next_id += 1;
        let mw = moved.widget();
        let holder = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        holder.set_hexpand(true);
        holder.set_vexpand(true);
        holder.append(&mw);
        self.stack.add_named(&holder, Some(&new_id.to_string()));
        let number = self.tabs.len() as u32 + 1;
        let (title, custom_title) = remote.as_ref().map_or_else(
            || (default_tab_title(number, moved.cwd.as_deref()), false),
            |conn| (conn.host.name.clone(), true),
        );
        let new_tab = Tab {
            holder,
            panes: vec![moved],
            active_pane: 0,
            title,
            custom_title,
            bell: false,
            activity: false,
            marked: false,
            pinned: false,
            id: new_id,
            zoom: None,
            remote,
        };
        if let Some(session) = self.active_agent.borrow_mut().as_mut() {
            if session.bound_pane == pane_id {
                session.bound_tab = new_id;
            }
        }
        self.insert_tab_after_active(new_tab);
        // The source tab lost a pane and may be back to one; `select_tab`
        // below only refreshes the destination.
        self.refresh_pane_headers(ti);
        self.select_tab(new_id, sender);
    }

    /// Remove a non-final pane from both the GTK split tree and the tab model.
    /// Keeping these mutations together prevents either representation from
    /// advancing when structural validation fails.
    fn detach_pane_from_tab(&mut self, tab_index: usize, pane_index: usize) -> Option<Pane> {
        let tab = self.tabs.get(tab_index)?;
        if tab.panes.len() <= 1 || pane_index >= tab.panes.len() {
            return None;
        }

        let leaf = tab.panes[pane_index].widget();
        detach_leaf_and_promote(&tab.holder, &leaf)?;

        let tab = self.tabs.get_mut(tab_index)?;
        let removed = tab.panes.remove(pane_index);
        tab.active_pane = active_index_after_remove(tab.active_pane, pane_index, tab.panes.len());
        Some(removed)
    }
}

fn reconnect_target_is_valid(
    panes_len: usize,
    zoomed: bool,
    remote_pane_id: Option<u64>,
    event_pane_id: u64,
) -> bool {
    panes_len == 1 && !zoomed && remote_pane_id == Some(event_pane_id)
}

#[cfg(test)]
mod pane_tree_tests {
    use super::{
        abbreviate_prefix, active_index_after_remove, format_running_process_summary,
        pane_header_title, reconnect_target_is_valid, replay_argv_for_unmanaged_leaf,
        restored_leaf_mode, snapshot_restorable_command,
    };
    use crate::config::TerminalMode;

    #[test]
    fn home_is_abbreviated_only_at_a_component_boundary() {
        assert_eq!(abbreviate_prefix("/home/user", "/home/user"), "~");
        assert_eq!(abbreviate_prefix("/home/user/src", "/home/user"), "~/src");
        // A sibling directory that merely shares the prefix must stay intact.
        assert_eq!(
            abbreviate_prefix("/home/user2/src", "/home/user"),
            "/home/user2/src"
        );
        assert_eq!(abbreviate_prefix("/etc", "/home/user"), "/etc");
        assert_eq!(abbreviate_prefix("/etc", ""), "/etc");
    }

    #[test]
    fn pane_header_title_prefers_osc_then_directory_then_position() {
        assert_eq!(
            pane_header_title(Some("vim README"), Some("/tmp"), 0),
            "vim README"
        );
        // Whitespace-only OSC titles must not blank the header.
        assert_eq!(pane_header_title(Some("   "), Some("/tmp/work"), 0), "work");
        assert_eq!(pane_header_title(None, Some("/tmp/work"), 0), "work");
        // A path with no last component keeps whatever it does have.
        assert_eq!(pane_header_title(None, Some("/"), 0), "/");
        assert_eq!(pane_header_title(None, None, 2), "Pane 3");
        assert_eq!(pane_header_title(Some(""), None, 0), "Pane 1");
    }

    #[test]
    fn restored_splits_use_the_configured_backend_for_every_leaf() {
        for _ in 0..3 {
            assert!(matches!(
                restored_leaf_mode(TerminalMode::Block, false),
                TerminalMode::Block
            ));
            assert!(matches!(
                restored_leaf_mode(TerminalMode::Vte, false),
                TerminalMode::Vte
            ));
        }
    }

    #[test]
    fn remote_restore_keeps_block_mode_and_ignores_remote_cwd_namespace() {
        let ssh = vec!["/usr/bin/ssh".to_string(), "example.test".to_string()];
        let nix = vec!["nix".to_string(), "develop".to_string()];
        assert!(crate::process::command_uses_external_cwd(&ssh));
        assert!(!crate::process::command_uses_external_cwd(&nix));
        assert!(matches!(
            restored_leaf_mode(TerminalMode::Vte, true),
            TerminalMode::Block
        ));
    }

    #[test]
    fn managed_remote_snapshots_store_only_the_profile_identifier() {
        let stale = vec!["ssh".to_string(), "old.example".to_string()];
        assert_eq!(snapshot_restorable_command(true, Some(stale.clone())), None);
        assert_eq!(
            snapshot_restorable_command(false, Some(stale.clone())),
            Some(stale)
        );
    }

    #[test]
    fn removed_managed_remote_never_replays_legacy_snapshot_argv() {
        let stale = vec!["ssh".to_string(), "removed.example".to_string()];
        assert_eq!(
            replay_argv_for_unmanaged_leaf(Some("removed"), Some(&stale)),
            None
        );
        assert_eq!(
            replay_argv_for_unmanaged_leaf(None, Some(&stale)),
            Some(stale.as_slice())
        );
    }

    #[test]
    fn remote_reconnect_requires_the_same_only_unzoomed_pane() {
        assert!(reconnect_target_is_valid(1, false, Some(7), 7));
        assert!(!reconnect_target_is_valid(2, false, Some(7), 7));
        assert!(!reconnect_target_is_valid(1, true, Some(7), 7));
        assert!(!reconnect_target_is_valid(1, false, Some(8), 7));
        assert!(!reconnect_target_is_valid(1, false, None, 7));
    }

    #[test]
    fn active_index_tracks_the_same_pane_when_an_earlier_pane_is_removed() {
        assert_eq!(active_index_after_remove(2, 0, 2), 1);
        assert_eq!(active_index_after_remove(1, 0, 2), 0);
    }

    #[test]
    fn removing_the_active_pane_prefers_the_next_then_previous_pane() {
        assert_eq!(active_index_after_remove(1, 1, 2), 1);
        assert_eq!(active_index_after_remove(2, 2, 2), 1);
    }

    #[test]
    fn removing_a_later_pane_keeps_the_active_index() {
        assert_eq!(active_index_after_remove(0, 2, 2), 0);
    }

    #[test]
    fn running_process_summary_is_empty_or_bounded_without_losing_count() {
        assert_eq!(format_running_process_summary(Vec::new()), None);
        let summary = format_running_process_summary(
            (1..=10).map(|index| format!("tab {index} — vim")).collect(),
        )
        .unwrap();
        assert!(summary.contains("tab 1 — vim"));
        assert!(!summary.contains("tab 9 — vim"));
        assert!(summary.ends_with("…and 2 more"));
    }
}
