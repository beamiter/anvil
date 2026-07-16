//! Review-first terminal insertion shared by history, workflows, AI, and files.

use super::*;

impl AppModel {
    /// Insert printable, single-line text without submitting it. Centralizing
    /// this boundary prevents a history entry, workflow parameter, file name,
    /// or model response from smuggling Enter or a terminal escape into a PTY.
    pub(crate) fn insert_review_text(&self, text: &str) -> bool {
        let Some(pane_id) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane))
            .map(|pane| pane.id)
        else {
            return false;
        };
        self.insert_review_text_into_pane(pane_id, text)
    }

    /// Targeted counterpart used by asynchronous review flows. The pane that
    /// initiated a request remains the destination even if focus changes.
    pub(crate) fn insert_review_text_into_pane(&self, pane_id: u64, text: &str) -> bool {
        let text = match review_input::validate(text) {
            Ok(text) => text,
            Err(error) => {
                log::warn!("refusing unsafe review-only shell input: {error}");
                self.show_toast(format!("Command was not inserted: {error}."));
                return false;
            }
        };
        let Some((tab_index, pane_index)) = self.find_pane(pane_id) else {
            return false;
        };
        let term = &self.tabs[tab_index].panes[pane_index].terminal;
        term.emit(VteInput::WriteInput(text.as_bytes().to_vec()));
        term.emit(VteInput::GrabFocus);
        true
    }
}
