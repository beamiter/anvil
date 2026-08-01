//! Review-first terminal insertion shared by history, workflows, AI, and files.

use super::*;

const MAX_LOCAL_REVIEW_INPUT_BYTES: usize = 256 * 1024;

fn local_review_issue(text: &str) -> Option<&'static str> {
    if text.len() > MAX_LOCAL_REVIEW_INPUT_BYTES {
        return Some("the command exceeds the 262144-byte review limit");
    }
    if crate::text_safety::contains_visual_spoof(text) {
        return Some("the command contains an invisible or bidirectional formatting character");
    }
    None
}

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
        if let Some(error) = local_review_issue(text) {
            log::warn!("refusing unsafe review-only shell input: {error}");
            self.show_toast(format!("Command was not inserted: {error}."));
            return false;
        }
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

#[cfg(test)]
mod tests {
    use super::{local_review_issue, MAX_LOCAL_REVIEW_INPUT_BYTES};

    #[test]
    fn local_review_gate_covers_new_core_visual_and_size_contract() {
        assert_eq!(local_review_issue("echo safe"), None);
        assert!(local_review_issue("echo safe\u{202e}txt").is_some());
        assert!(local_review_issue("echo safe\u{200b}hidden").is_some());
        assert!(local_review_issue("echo safe\u{00ad}hidden").is_some());
        assert!(local_review_issue("echo safe\u{e0020}hidden").is_some());
        assert!(local_review_issue(&"x".repeat(MAX_LOCAL_REVIEW_INPUT_BYTES + 1)).is_some());
    }
}
