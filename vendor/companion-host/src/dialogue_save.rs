//! Volatile capture of one explicit text exchange for the existing memory
//! review UI. Receiving a reply never sends a memory request or grants consent.
use companion_protocol::{identifier, valid_context, ContextRef, MemoryChange, MemoryScope};

#[derive(Default)]
pub struct DialogueSave {
    turn: Option<Turn>,
}
struct Turn {
    id: String,
    context: ContextRef,
    input: String,
    reply: Option<String>,
}
impl DialogueSave {
    pub fn clear(&mut self) {
        self.turn = None;
    }
    pub fn begin(&mut self, id: &str, context: &ContextRef, input: &str) {
        self.clear();
        if identifier(id)
            && valid_context(context)
            && !input.trim().is_empty()
            && input.len() <= 8192
        {
            self.turn = Some(Turn {
                id: id.into(),
                context: context.clone(),
                input: input.into(),
                reply: None,
            });
        }
    }
    pub fn observe(&mut self, id: &str, context: &ContextRef, status: &str, reply: Option<&str>) {
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|t| t.id == id && &t.context == context)
        else {
            return;
        };
        if let Some(reply) = reply.filter(|r| !r.is_empty() && r.len() <= 8192) {
            if matches!(
                status,
                "generated" | "generated_for_previous_context" | "offline"
            ) {
                turn.reply = Some(reply.into());
            }
        }
    }
    pub fn turn_id(&self) -> Option<&str> {
        self.turn
            .as_ref()
            .filter(|t| t.reply.is_some())
            .map(|t| t.id.as_str())
    }
    /// Call only from an explicit save control. This creates a proposal for the
    /// normal exact-change review; it cannot write memory or approve itself.
    pub fn proposal(
        &self,
        expected_turn: &str,
        current: &ContextRef,
        scope: MemoryScope,
    ) -> Result<MemoryChange, &'static str> {
        let turn = self
            .turn
            .as_ref()
            .filter(|t| t.id == expected_turn)
            .ok_or("This exchange is no longer available.")?;
        if &turn.context != current {
            return Err("The pane or task changed. Save an exchange from the current context.");
        }
        let allowed = match &scope {
            MemoryScope::Global => true,
            MemoryScope::Task(id) => current.task_id.as_ref() == Some(id),
            MemoryScope::Project(id) => current.project_id.as_ref() == Some(id),
        };
        if !allowed {
            return Err("Choose the current task/project or explicitly choose Global.");
        }
        let reply = turn
            .reply
            .as_deref()
            .ok_or("Wait for the complete reply before saving.")?;
        let value = format!("User:\n{}\n\nEva:\n{}", turn.input, reply);
        if value.len() > 8192 {
            return Err(
                "This exchange exceeds 8192 bytes. Select a shorter fact in the memory editor.",
            );
        }
        Ok(MemoryChange::Put {
            scope,
            key: turn.id.clone(),
            value,
            source_event_id: turn.id.clone(),
            expected_revision: None,
            expires_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> ContextRef {
        ContextRef {
            host_id: "host".into(),
            pane_id: "pane".into(),
            task_id: Some("task".into()),
            project_id: Some("project".into()),
            revision: 1,
        }
    }
    #[test]
    fn incomplete_and_unrelated_replies_cannot_be_saved() {
        let c = context();
        let mut save = DialogueSave::default();
        save.begin("turn", &c, "request");
        save.observe("another", &c, "generated", Some("wrong reply"));
        save.observe("turn", &c, "generating", Some("partial reply"));
        assert!(save.turn_id().is_none());
        assert!(save.proposal("turn", &c, MemoryScope::Global).is_err());
        save.observe("turn", &c, "generated", Some("complete reply"));
        assert_eq!(save.turn_id(), Some("turn"));
        assert!(save.proposal("another", &c, MemoryScope::Global).is_err());
    }
    #[test]
    fn exact_exchange_uses_existing_review_without_overwrite_or_selection_capture() {
        let c = context();
        let mut save = DialogueSave::default();
        save.begin("turn", &c, "explicit user input");
        save.observe("turn", &c, "offline", Some("actual reply"));
        let MemoryChange::Put {
            scope,
            key,
            value,
            source_event_id,
            expected_revision,
            expires_at,
        } = save
            .proposal("turn", &c, MemoryScope::Task("task".into()))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(scope, MemoryScope::Task("task".into()));
        assert_eq!((key.as_str(), source_event_id.as_str()), ("turn", "turn"));
        assert_eq!(value, "User:\nexplicit user input\n\nEva:\nactual reply");
        assert_eq!(expected_revision, None);
        assert_eq!(expires_at, None);
    }
    #[test]
    fn changed_context_and_archived_scope_are_not_silent_destinations() {
        let c = context();
        let mut save = DialogueSave::default();
        save.begin("turn", &c, "request");
        save.observe("turn", &c, "generated", Some("reply"));
        let mut changed = c.clone();
        changed.revision += 1;
        assert!(save
            .proposal("turn", &changed, MemoryScope::Global)
            .is_err());
        assert!(save
            .proposal("turn", &c, MemoryScope::Task("other".into()))
            .is_err());
        assert!(save
            .proposal("turn", &c, MemoryScope::Project("other".into()))
            .is_err());
        assert!(save
            .proposal("turn", &c, MemoryScope::Project("project".into()))
            .is_ok());
        assert!(save.proposal("turn", &c, MemoryScope::Global).is_ok());
    }
    #[test]
    fn new_turn_and_disconnect_discard_the_previous_capture() {
        let c = context();
        let mut save = DialogueSave::default();
        save.begin("old", &c, "first");
        save.observe("old", &c, "generated", Some("first reply"));
        save.begin("new", &c, "second");
        save.observe("old", &c, "generated", Some("late old reply"));
        assert!(save.turn_id().is_none());
        save.observe("new", &c, "generated", Some("second reply"));
        save.clear();
        assert!(save.proposal("new", &c, MemoryScope::Global).is_err());
    }
    #[test]
    fn oversized_unicode_exchange_is_rejected_without_truncation() {
        let c = context();
        let mut save = DialogueSave::default();
        save.begin("turn", &c, &"字".repeat(2000));
        save.observe("turn", &c, "generated", Some(&"语".repeat(2000)));
        assert!(save
            .proposal("turn", &c, MemoryScope::Global)
            .unwrap_err()
            .contains("8192"));
    }
}
