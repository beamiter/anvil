//! Window-close authority, separate from the original native clearance.
//! Cancelling never acknowledges, replaces or discards the retirement token.
#[derive(Default)]
pub(crate) struct CloseIntent {
    revision: u64,
    armed: bool,
    guarded: bool,
    completion_queued: bool,
    committed: bool,
}

impl CloseIntent {
    pub(crate) fn begin(&mut self, guarded: bool) -> bool {
        if self.armed || self.committed {
            return false;
        }
        let Some(revision) = self.revision.checked_add(1) else {
            return false;
        };
        self.revision = revision;
        self.armed = true;
        self.guarded = guarded;
        self.completion_queued = false;
        true
    }

    pub(crate) fn waiting(&self) -> bool {
        self.armed
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn cancelled_current(&self, revision: u64) -> bool {
        !self.armed && !self.committed && self.revision == revision
    }

    pub(crate) fn should_cancel_for_work(&self, busy: bool) -> bool {
        self.armed && self.guarded && busy
    }

    pub(crate) fn cancel(&mut self) -> bool {
        if !self.armed {
            return false;
        }
        self.armed = false;
        self.completion_queued = false;
        true
    }

    pub(crate) fn queue_completion(&mut self, ready: bool) -> Option<u64> {
        if !ready {
            self.completion_queued = false;
            return None;
        }
        if !self.armed || self.completion_queued {
            return None;
        }
        self.completion_queued = true;
        Some(self.revision)
    }

    pub(crate) fn completion_current(&self, revision: u64) -> bool {
        self.armed && self.completion_queued && self.revision == revision
    }

    pub(crate) fn retry_completion(&mut self, revision: u64) {
        if self.completion_current(revision) {
            self.completion_queued = false;
        }
    }

    pub(crate) fn commit(&mut self, ready: bool) -> bool {
        if self.committed {
            return true;
        }
        if !self.armed || !ready {
            return false;
        }
        self.armed = false;
        self.committed = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_and_previous_close_callbacks_cannot_authorize_exit() {
        let mut intent = CloseIntent::default();
        assert!(!intent.commit(true));
        assert!(intent.begin(true));
        assert!(!intent.commit(false));
        let old = intent.queue_completion(true).unwrap();
        assert_eq!(intent.queue_completion(true), None);
        assert!(intent.cancel());
        assert!(intent.cancelled_current(old));
        assert!(!intent.completion_current(old));
        assert!(!intent.commit(true));
        assert!(intent.begin(true));
        assert!(!intent.cancelled_current(old));
        assert!(!intent.completion_current(old));
        let fresh = intent.queue_completion(true).unwrap();
        assert!(intent.completion_current(fresh));
        assert!(intent.commit(true));
        assert!(!intent.cancel());
        assert!(!intent.begin(true));
    }

    #[test]
    fn new_work_requires_fresh_permission_but_explicit_termination_still_works() {
        let mut intent = CloseIntent::default();
        intent.begin(true);
        assert!(!intent.should_cancel_for_work(false));
        assert!(intent.should_cancel_for_work(true));
        intent.cancel();
        assert!(!intent.should_cancel_for_work(false));
        assert_eq!(intent.queue_completion(true), None);
        intent.begin(false);
        assert!(!intent.should_cancel_for_work(true));
        assert!(intent.commit(true));
    }

    #[test]
    fn lost_acceptance_before_idle_callback_retries_without_rearming() {
        let mut intent = CloseIntent::default();
        intent.begin(true);
        assert!(intent.waiting());
        let revision = intent.queue_completion(true).unwrap();
        intent.retry_completion(revision);
        assert!(!intent.completion_current(revision));
        assert!(!intent.commit(false));
        assert_eq!(intent.queue_completion(false), None);
        assert_eq!(intent.queue_completion(true), Some(revision));
        assert_eq!(intent.queue_completion(false), None);
        assert!(!intent.completion_current(revision));
        assert_eq!(intent.queue_completion(true), Some(revision));
        assert!(intent.commit(true));
    }
}
