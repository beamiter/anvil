//! Bottom status bar operations on the Relm4 `AppModel`.
//!
//! What the bar says is owned by `jterm_core::bottom_bar` — the family-wide
//! contract shared by all four jterms. This module only gathers the snapshot
//! from state the app already tracks and renders the composed segments as
//! GTK labels; tone colors are mapped to CSS classes in `apply_dynamic_css`.

use super::*;

use jterm_core::bottom_bar::{self, Segment, Tone};

/// CSS class for a tone. The class-to-color mapping lives in
/// `AppModel::apply_dynamic_css` so the bar follows theme changes.
fn tone_css_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Normal => "bb-normal",
        Tone::Muted => "bb-muted",
        Tone::Positive => "bb-ok",
        Tone::Negative => "bb-err",
    }
}

/// Replace a holder's children with one label per segment, in compose order.
fn fill_segments(holder: &gtk::Box, segments: &[Segment]) {
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
    for segment in segments {
        let label = gtk::Label::new(Some(&segment.text));
        label.add_css_class(tone_css_class(segment.tone));
        holder.append(&label);
    }
}

impl AppModel {
    /// Apply the config's `bottom_bar` toggle. Shaped like
    /// `set_sidebar_visible`, minus persistence: the key is file-only.
    pub(crate) fn set_bottom_bar_visible(&self, visible: bool) {
        self.bottom_bar.set_visible(visible);
        if visible {
            self.refresh_bottom_bar();
        }
    }

    /// Recompute the focused pane's snapshot and redraw the bar. Cheap enough
    /// for the 1 Hz tick: git metadata comes from the worker-backed cache in
    /// `jterm_core::git_meta`, everything else is state already in hand.
    pub(crate) fn refresh_bottom_bar(&self) {
        let tab = self.tabs.get(self.active);
        let pane = tab.and_then(|tab| tab.panes.get(tab.active_pane));
        let cwd = pane.and_then(Pane::local_cwd).map(std::path::PathBuf::from);
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let title = jterm_core::title_bar::compose(&jterm_core::title_bar::Snapshot {
            app: "Anvil",
            title: tab.map(|tab| tab.display_title()).unwrap_or_default(),
            cwd: cwd.as_deref(),
            home: home.as_deref(),
            mode: self.config.borrow().terminal_mode.as_str(),
            tab_index: self.active,
            tab_count: self.tabs.len(),
            pane_count: tab.map_or(0, |tab| tab.panes.len()),
            private: tab.is_some_and(|tab| tab.private_title),
        });
        self.window.set_title(Some(&title.window_title));
        let presentation = match pane.map(|pane| &pane.terminal) {
            Some(TermCtl::Block(controller)) => controller.model().mode(),
            _ => TerminalMode::Block,
        };
        let private = tab.is_some_and(|tab| tab.private_title);
        let git = (!private)
            .then(|| {
                cwd.as_deref()
                    .and_then(crate::git_meta_ui::read_cached_and_refresh)
            })
            .flatten();
        self.shell.sync(
            if tab.is_some_and(|tab| tab.private_title) {
                "Private session"
            } else {
                &title.window_title
            },
            pane.and_then(|pane| pane.cwd.as_deref())
                .unwrap_or("Directory not reported"),
            presentation,
            tab.is_some_and(|tab| tab.private_title),
        );
        let process = (!private)
            .then(|| pane.and_then(Pane::foreground_process))
            .flatten();
        let remote = tab
            .and_then(|tab| tab.remote.as_ref())
            .filter(|remote| pane.is_some_and(|pane| pane.id == remote.pane_id));
        let namespace = if let Some(remote) = remote {
            format!("Remote · {}", remote.host.name)
        } else if pane.is_some_and(|pane| pane.cwd_external) {
            "External session (reported directory)".into()
        } else {
            "Local".into()
        };
        let context = workspace_shell::SessionContext {
            private,
            cwd: pane.and_then(|pane| pane.cwd.as_deref()),
            namespace: &namespace,
            configured_shell: self
                .shell_argv
                .first()
                .map(String::as_str)
                .unwrap_or("Not reported"),
            presentation: pane
                .map(|pane| pane.terminal.mode().as_str())
                .unwrap_or("No session"),
            process: process.as_deref(),
            last_exit: pane.and_then(|pane| pane.last_exit),
            duration_ms: pane.and_then(|pane| pane.last_duration_ms),
            grid: pane.map(|pane| pane.terminal.grid_size()).unwrap_or((0, 0)),
            branch: git.as_ref().map(|meta| meta.branch.as_str()),
            share_command_context: crate::agent_task_ui::prompt_policy(&self.config.borrow())
                .share_command_context,
        };
        self.shell.sync_session_context(&context);
        self.shell.sync_workspace_entries(
            &self
                .tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| (tab.id, tab.display_title().to_owned(), index == self.active))
                .collect::<Vec<_>>(),
        );
        self.refresh_workspace_task_overview();
        // The own visible property, not effective visibility: the initial
        // refresh runs before the window is mapped.
        if !self.bottom_bar.get_visible() {
            return;
        }
        let (cols, rows) = pane.map(|p| p.terminal.grid_size()).unwrap_or((0, 0));
        let content = bottom_bar::compose(&bottom_bar::Snapshot {
            cwd: cwd.as_deref(),
            home: home.as_deref(),
            git: git.as_ref(),
            running: pane.is_some_and(|p| p.foreground_process().is_some()),
            last_exit: pane.and_then(|p| p.last_exit),
            last_duration_ms: pane.and_then(|p| p.last_duration_ms),
            cols,
            rows,
            tab_index: self.active,
            tab_count: self.tabs.len(),
        });
        if *self.bottom_bar_content.borrow() == content {
            return;
        }
        fill_segments(&self.bottom_bar_left, &content.left);
        fill_segments(&self.bottom_bar_right, &content.right);
        *self.bottom_bar_content.borrow_mut() = content;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tone_maps_to_a_distinct_css_class() {
        let classes: Vec<&str> = [Tone::Normal, Tone::Muted, Tone::Positive, Tone::Negative]
            .into_iter()
            .map(tone_css_class)
            .collect();
        assert_eq!(classes, vec!["bb-normal", "bb-muted", "bb-ok", "bb-err"]);
    }
}
