//! Tab-strip placement, sidebar view, and navigation presentation operations.
//!
//! This remains part of the same Relm4 `AppModel`; it only moves GTK presentation
//! helpers out of the component lifecycle implementation.

use super::*;

impl AppModel {
    /// Move the tab strip into the holder matching the current placement and
    /// flip its orientation; sidebar = vertical list, top bar = horizontal.
    pub(crate) fn apply_tab_placement(&self) {
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
    pub(crate) fn apply_sidebar_view(&self, view: config::SidebarView, persist: bool) {
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
    pub(crate) fn apply_strip_row_placement(&self, row: &gtk::Widget) {
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

    /// Keep the top-bar tab strip visible even for a lone tab so its title and
    /// tab actions remain available in the configured placement.
    pub(crate) fn sync_tab_bar_visibility(&self) {
        match self.tab_placement.get() {
            config::TabPlacement::Sidebar => {
                self.tab_strip_scroll.set_visible(true);
                self.top_tab_scroll.set_visible(false);
            }
            config::TabPlacement::TopBar => {
                self.tab_strip_scroll.set_visible(true);
                self.top_tab_scroll.set_visible(!self.tabs.is_empty());
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
    pub(crate) fn update_tab_title_widget(&self, id: u64, title: &str) -> bool {
        let Some(index) = self.tab_rows.iter().position(|row| row.id == id) else {
            return false;
        };
        self.tab_rows
            .send(index, tab_strip::TabRowMsg::SetTitle(title.to_string()));
        true
    }

    /// Flip the tab strip between the sidebar and the top bar, then persist.
    pub(crate) fn toggle_tab_placement(&mut self) {
        use config::TabPlacement;
        let next = match self.tab_placement.get() {
            TabPlacement::Sidebar => TabPlacement::TopBar,
            TabPlacement::TopBar => TabPlacement::Sidebar,
        };
        self.tab_placement.set(next);
        self.config.borrow_mut().tab_placement = next;
        self.apply_tab_placement();
        self.sync_tab_strip();
        self.persist_config();
    }

    pub(crate) fn rebuild_tab_strip(&mut self, _sender: &ComponentSender<AppModel>) {
        self.refresh_tab_strip(true);
    }

    /// Refresh row state without writing a session snapshot. Bell/activity and
    /// connection-state changes are presentation-only and can arrive often.
    pub(crate) fn sync_tab_strip(&mut self) {
        self.refresh_tab_strip(false);
    }

    fn refresh_tab_strip(&mut self, persist: bool) {
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

        let same_rows = self.tab_rows.len() == rows.len()
            && self
                .tab_rows
                .iter()
                .zip(rows.iter())
                .all(|(current, next)| current.id == next.id);

        if same_rows {
            let updates: Vec<_> = self
                .tab_rows
                .iter()
                .zip(rows.iter())
                .enumerate()
                .filter(|(_, (current, next))| !current.matches_init(next))
                .map(|(index, (_, next))| (index, next.clone()))
                .collect();
            for (index, row) in updates {
                self.tab_rows.send(index, tab_strip::TabRowMsg::Sync(row));
            }
        } else {
            let mut factory = self.tab_rows.guard();
            factory.clear();
            for row in rows {
                factory.push_back(row);
            }
        }

        self.sync_tab_bar_visibility();
        if persist {
            self.persist_session();
        }
    }
}
