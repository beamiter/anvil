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

fn active_index_after_remove(active: usize, removed: usize, remaining: usize) -> usize {
    debug_assert!(remaining > 0);
    if active > removed {
        active - 1
    } else {
        active.min(remaining - 1)
    }
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
            .and_then(|p| p.cwd.clone());
        self.add_tab_with(initial_commands, cwd, self.shell_argv.clone(), sender);
    }

    pub(crate) fn add_tab_with(
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
    pub(crate) fn build_pane_layout(
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
        if let Err(err) = config::save_config(&self.config.borrow()) {
            log::error!("{err}");
            self.show_toast(format!("Settings were not saved: {err}"));
        }
    }

    pub(crate) fn running_process_summary(&self) -> Option<String> {
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

    pub(crate) fn request_quit(&self, sender: &ComponentSender<AppModel>) {
        if let Some(running) = self.running_process_summary() {
            dialogs::confirm_close(&self.window, &running, AppMsg::ForceQuit, sender);
        } else {
            sender.input(AppMsg::ForceQuit);
        }
    }

    pub(crate) fn force_quit(&self) {
        self.persist_session();
        self.quit_allowed.set(true);
        self.window.close();
    }

    /// App-level diagnostics for the debug dashboard. (jterm4 surfaces per-block
    /// stats from the block backend; jterm1 exposes window/session state — block
    /// internals would need a backend round-trip, noted as a parity gap.)
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
    pub(crate) fn mark_remote_connected(&mut self, idx: usize) -> bool {
        if let Some(conn) = self.tabs[idx].remote.as_mut() {
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
    pub(crate) fn do_remote_reconnect(
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
        self.rebuild_tab_strip(sender);
    }

    pub(crate) fn close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
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
    pub(crate) fn tab_running_command(&self, idx: usize) -> Option<String> {
        self.tabs
            .get(idx)?
            .panes
            .iter()
            .find_map(|p| p.restorable_command())
    }

    /// Close a tab, first confirming if a process is still running in it.
    pub(crate) fn request_close_tab(&mut self, id: u64, sender: &ComponentSender<AppModel>) {
        if let Some(idx) = self.index_of(id) {
            if let Some(cmd) = self.tab_running_command(idx) {
                dialogs::confirm_close(&self.window, &cmd, AppMsg::ForceCloseTab(id), sender);
                return;
            }
        }
        self.close_tab(id, sender);
    }

    /// Close a pane, first confirming if a process is still running in it.
    pub(crate) fn request_close_pane(&mut self, pane_id: u64, sender: &ComponentSender<AppModel>) {
        if let Some((ti, pi)) = self.find_pane(pane_id) {
            if let Some(cmd) = self.tabs[ti].panes[pi].restorable_command() {
                dialogs::confirm_close(&self.window, &cmd, AppMsg::ForceClosePane(pane_id), sender);
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
    pub(crate) fn close_marked_tabs(&mut self, sender: &ComponentSender<AppModel>) {
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

    pub(crate) fn find_pane(&self, pane_id: u64) -> Option<(usize, usize)> {
        for (ti, tab) in self.tabs.iter().enumerate() {
            if let Some(pi) = tab.panes.iter().position(|p| p.id == pane_id) {
                return Some((ti, pi));
            }
        }
        None
    }

    /// Split the active pane, placing a fresh bare-VTE pane beside it.
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
        let Some(removed) = self.detach_pane_from_tab(ti, pi) else {
            log::error!("Failed to detach pane {pane_id} from tab widget tree");
            return;
        };
        let tab = &mut self.tabs[ti];
        let ap = tab.active_pane;
        tab.panes[ap].terminal.emit(VteInput::GrabFocus);
        drop(removed);
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

    pub(crate) fn resize_pane(&mut self, target: gtk::Orientation, delta: i32) {
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
    pub(crate) fn move_pane_to_new_tab(&mut self, sender: &ComponentSender<AppModel>) {
        let (ti, pi, pane_id) = {
            let Some(tab) = self.tabs.get(self.active) else {
                return;
            };
            if tab.panes.len() <= 1 || tab.zoom.is_some() {
                return;
            }
            (self.active, tab.active_pane, tab.panes[tab.active_pane].id)
        };
        let Some(moved) = self.detach_pane_from_tab(ti, pi) else {
            log::error!("Failed to detach pane {pane_id} into a new tab");
            return;
        };

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

    /// Remove a non-final pane from both the GTK split tree and the tab model.
    /// Keeping these mutations together prevents either representation from
    /// advancing when structural validation fails.
    fn detach_pane_from_tab(&mut self, tab_index: usize, pane_index: usize) -> Option<Pane> {
        let tab = self.tabs.get(tab_index)?;
        if tab.panes.len() <= 1 || pane_index >= tab.panes.len() {
            return None;
        }

        let leaf = tab.panes[pane_index].terminal.widget();
        detach_leaf_and_promote(&tab.holder, &leaf)?;

        let tab = self.tabs.get_mut(tab_index)?;
        let removed = tab.panes.remove(pane_index);
        tab.active_pane = active_index_after_remove(tab.active_pane, pane_index, tab.panes.len());
        Some(removed)
    }
}

#[cfg(test)]
mod pane_tree_tests {
    use super::active_index_after_remove;

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
}
