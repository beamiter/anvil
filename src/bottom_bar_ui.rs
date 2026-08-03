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
        // The own visible property, not effective visibility: the initial
        // refresh runs before the window is mapped.
        if !self.bottom_bar.get_visible() {
            return;
        }
        let pane = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane));
        let cwd = pane.and_then(Pane::local_cwd).map(std::path::PathBuf::from);
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let git = cwd.as_deref().and_then(git_meta::read);
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
        fill_segments(&self.bottom_bar_left, &content.left);
        fill_segments(&self.bottom_bar_right, &content.right);
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
