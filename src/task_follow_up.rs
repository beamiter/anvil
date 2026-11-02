//! Acknowledged native follow-up submissions. Text stays local until the runtime accepts it.
use crate::agent_task::TaskId;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskFollowUpRequest {
    pub task_id: TaskId,
    pub text: String,
    pub request_id: uuid::Uuid,
}

impl TaskFollowUpRequest {
    pub fn new(task_id: TaskId, text: String) -> Self {
        Self {
            task_id,
            text,
            request_id: uuid::Uuid::new_v4(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FollowUpReceipts {
    pending: HashMap<TaskId, (uuid::Uuid, u64)>,
    revisions: HashMap<TaskId, u64>,
    errors: HashMap<TaskId, String>,
}

impl FollowUpReceipts {
    pub fn edited(&mut self, task: TaskId) {
        let revision = self.revisions.entry(task).or_default();
        *revision = revision.wrapping_add(1);
    }
    pub fn is_pending(&self, task: TaskId) -> bool {
        self.pending.contains_key(&task)
    }

    pub fn error(&self, task: TaskId) -> Option<&str> {
        self.errors.get(&task).map(String::as_str)
    }

    pub fn begin(&mut self, task: TaskId, text: String) -> Option<TaskFollowUpRequest> {
        if self.is_pending(task) {
            return None;
        }
        let request = TaskFollowUpRequest::new(task, text);
        self.pending.insert(
            task,
            (request.request_id, *self.revisions.get(&task).unwrap_or(&0)),
        );
        self.errors.remove(&task);
        Some(request)
    }

    /// Late receipts never clear another attempt or any text typed after submission.
    pub fn resolve(
        &mut self,
        drafts: &mut HashMap<TaskId, String>,
        request: &TaskFollowUpRequest,
        result: Result<(), String>,
    ) -> bool {
        let Some((id, revision)) = self.pending.get(&request.task_id).copied() else {
            return false;
        };
        if id != request.request_id {
            return false;
        }
        self.pending.remove(&request.task_id);
        match result {
            Ok(()) => {
                self.errors.remove(&request.task_id);
                if *self.revisions.get(&request.task_id).unwrap_or(&0) == revision
                    && drafts.get(&request.task_id) == Some(&request.text)
                {
                    drafts.remove(&request.task_id);
                }
            }
            Err(error) => {
                self.errors.insert(
                    request.task_id,
                    jterm_core::review_input::safe_inline_display(&error, 1024),
                );
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_turn_retains_input_and_old_receipt_cannot_clear_retry() {
        let task = TaskId::new();
        let mut drafts: HashMap<TaskId, String> = HashMap::from([(task, "repair the race".into())]);
        let mut receipts = FollowUpReceipts::default();
        let first = receipts.begin(task, drafts[&task].clone()).unwrap();
        assert!(receipts.begin(task, drafts[&task].clone()).is_none());
        assert!(receipts.resolve(&mut drafts, &first, Err("Session closed".into())));
        assert_eq!(drafts[&task], "repair the race");
        assert_eq!(receipts.error(task), Some("Session closed"));
        let retry = receipts.begin(task, drafts[&task].clone()).unwrap();
        assert!(!receipts.resolve(&mut drafts, &first, Ok(())));
        assert!(receipts.is_pending(task));
        assert!(receipts.resolve(&mut drafts, &retry, Ok(())));
        assert!(!drafts.contains_key(&task));
    }

    #[test]
    fn accepted_turn_preserves_newer_text_and_other_task_drafts() {
        let task = TaskId::new();
        let other = TaskId::new();
        let mut drafts: HashMap<TaskId, String> =
            HashMap::from([(task, "first turn".into()), (other, "other project".into())]);
        let mut receipts = FollowUpReceipts::default();
        let request = receipts.begin(task, drafts[&task].clone()).unwrap();
        drafts.insert(task, "new correction".into());
        assert!(receipts.resolve(&mut drafts, &request, Ok(())));
        assert_eq!(drafts[&task], "new correction");
        assert_eq!(drafts[&other], "other project");
        assert!(!receipts.is_pending(task));
    }
    #[test]
    fn acknowledgement_cannot_clear_text_edited_and_changed_back() {
        let task = TaskId::new();
        let mut drafts = HashMap::from([(task, "same text".into())]);
        let mut receipts = FollowUpReceipts::default();
        let request = receipts.begin(task, "same text".into()).unwrap();
        receipts.edited(task);
        drafts.insert(task, "different".into());
        receipts.edited(task);
        drafts.insert(task, "same text".into());
        assert!(receipts.resolve(&mut drafts, &request, Ok(())));
        assert_eq!(drafts[&task], "same text");
    }
}
