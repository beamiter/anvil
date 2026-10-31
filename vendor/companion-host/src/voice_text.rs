//! Window-owned status of explicit voice-text requests. No voice text passes
//! through here: a ready review arrives as an ordinary `MemoryReview` and
//! still needs the existing exact-change approval.
use companion_protocol::Reply;

#[derive(Default)]
pub struct VoiceTextStatus {
    next: u64,
    pending: Option<u64>,
    status: String,
    revision: u64,
}

fn message(status: &str) -> String {
    match status {
        "voice_text_requested" => "Asking Eva for its retained spoken text…",
        "voice_text_review_ready" => {
            "Verified against the recorded reply and sent to Memory review. Only your approval there saves it."
        }
        "voice_text_forgotten" => "Eva dropped every retained spoken reply.",
        "voice_text_not_retained" => {
            "Eva no longer holds that reply. It expired or was never kept."
        }
        "voice_text_retention_paused" | "memory_paused" => {
            "Memory is paused, so Eva keeps no spoken text."
        }
        "voice_text_eva_unavailable" => {
            "Eva voice is not connected with spoken-text review support."
        }
        "voice_text_no_reply" => "Eva has no spoken reply in this coordinator session.",
        "voice_text_digest_mismatch" | "voice_text_wrong_turn" | "voice_text_invalid_offer" => {
            "Eva's text did not match the recorded reply. Nothing was proposed."
        }
        "voice_text_too_large" => "That reply is too long for one memory entry.",
        "desktop_locked_or_unknown" => "Unavailable while the session is locked or unknown.",
        "stale_context" => "The pane or task changed. Try again.",
        "memory_scope_denied" => "Choose the current task/project or explicitly choose Global.",
        "voice_text_busy" => "Another spoken-text request is in progress.",
        "voice_text_timeout" => "Eva did not answer in time. Nothing was proposed.",
        "voice_text_not_negotiated" => "Update the Eva coordinator to review spoken replies.",
        "memory_conflict" => "A memory entry for this reply already exists.",
        other => return format!("Spoken text unavailable: {other}"),
    }
    .into()
}

impl VoiceTextStatus {
    pub fn status(&self) -> &str {
        &self.status
    }
    /// Changes whenever the status text changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    fn set(&mut self, status: String) {
        self.status = status;
        self.revision = self.revision.wrapping_add(1);
    }
    pub(crate) fn begin(&mut self) -> Result<u64, &'static str> {
        if self.pending.is_some() {
            return Err("Another spoken-text request is in progress.");
        }
        self.next = self.next.checked_add(1).ok_or("Spoken text unavailable.")?;
        self.pending = Some(self.next);
        self.set(message("voice_text_requested"));
        Ok(self.next)
    }
    /// Returns true if the reply belonged to voice text.
    pub fn observe(&mut self, reply: &Reply) -> bool {
        let Reply::VoiceText { request_id, status } = reply else {
            return false;
        };
        if self.pending != Some(*request_id) {
            return true;
        }
        if status != "voice_text_requested" {
            self.pending = None;
        }
        self.set(message(status));
        true
    }
    pub fn disconnected(&mut self) {
        if self.pending.take().is_some() {
            self.set("Coordinator disconnected. Nothing was proposed.".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reply(request_id: u64, status: &str) -> Reply {
        Reply::VoiceText {
            request_id,
            status: status.into(),
        }
    }
    #[test]
    fn one_request_at_a_time_and_late_answers_are_ignored() {
        let mut view = VoiceTextStatus::default();
        let first = view.begin().unwrap();
        assert!(view.begin().is_err());
        assert!(view.observe(&reply(first, "voice_text_requested")));
        assert!(view.pending(), "still waiting for Eva");
        assert!(view.observe(&reply(first + 7, "voice_text_review_ready")));
        assert!(view.pending(), "foreign request id");
        view.observe(&reply(first, "voice_text_review_ready"));
        assert!(!view.pending());
        assert!(view.status().contains("Only your approval there saves it"));
        let before = view.revision();
        view.observe(&reply(first, "voice_text_forgotten"));
        assert_eq!(view.revision(), before, "late duplicate");
        assert!(!view.observe(&Reply::Ack));
    }
    #[test]
    fn failures_never_claim_a_save_and_disconnect_clears_pending() {
        let mut view = VoiceTextStatus::default();
        for status in [
            "voice_text_digest_mismatch",
            "voice_text_timeout",
            "memory_paused",
            "voice_text_not_retained",
        ] {
            let id = view.begin().unwrap();
            view.observe(&reply(id, status));
            assert!(!view.pending());
            assert!(!view.status().contains("saved") || view.status().contains("Nothing"));
        }
        view.begin().unwrap();
        view.disconnected();
        assert!(!view.pending() && view.status().contains("Nothing was proposed"));
        let id = view.begin().unwrap();
        view.observe(&reply(id, "surprise"));
        assert_eq!(view.status(), "Spoken text unavailable: surprise");
    }
}
