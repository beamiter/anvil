//! Revisioned, explicit context editing. A material is never sent merely
//! because it was read, and a handoff packet is a draft for a new task.
use jterm_core::agent_workspace::{
    ConversationId, MaterialKind, WorkspaceClient, WorkspaceCommand, WorkspaceContext,
    WorkspaceEvent, WorkspaceHandoff, WorkspaceMaterial, WorkspaceMaterialSource, WorkspaceReply,
    WorkspaceTextDocument,
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ContextField {
    Goal,
    Completed,
    Remaining,
    Constraints,
}

struct Entry {
    value: WorkspaceContext,
    generation: u64,
    dirty: bool,
    remote: Option<WorkspaceContext>,
}

enum Pending {
    Read(ConversationId),
    Save {
        id: ConversationId,
        generation: u64,
    },
    Handoff {
        id: ConversationId,
        revision: u64,
        provider: String,
    },
}

pub(crate) struct ContextEditor {
    client: WorkspaceClient,
    pending: HashMap<u64, Pending>,
    entries: HashMap<ConversationId, Entry>,
    selected: Option<ConversationId>,
    pub(crate) packet: Option<WorkspaceHandoff>,
    pub(crate) notice: Option<String>,
}

impl Default for ContextEditor {
    fn default() -> Self {
        Self {
            client: WorkspaceClient::new(),
            pending: HashMap::new(),
            entries: HashMap::new(),
            selected: None,
            packet: None,
            notice: None,
        }
    }
}

impl ContextEditor {
    pub(crate) fn selected(&self) -> Option<&ConversationId> {
        self.selected.as_ref()
    }
    pub(crate) fn draft(&self) -> Option<&WorkspaceContext> {
        Some(&self.entries.get(self.selected.as_ref()?)?.value)
    }
    pub(crate) fn dirty(&self) -> bool {
        self.selected
            .as_ref()
            .and_then(|id| self.entries.get(id))
            .is_some_and(|entry| entry.dirty)
    }
    pub(crate) fn conflicted(&self) -> bool {
        self.selected
            .as_ref()
            .and_then(|id| self.entries.get(id))
            .is_some_and(|entry| entry.remote.is_some())
    }
    pub(crate) fn loading(&self) -> bool {
        self.pending.values().any(
            |pending| matches!(pending, Pending::Read(id) if Some(id) == self.selected.as_ref()),
        )
    }
    pub(crate) fn saving(&self) -> bool {
        self.pending.values().any(|pending| matches!(pending, Pending::Save { id, .. } if Some(id) == self.selected.as_ref()))
    }
    pub(crate) fn building(&self) -> bool {
        self.pending.values().any(|pending| matches!(pending, Pending::Handoff { id, .. } if Some(id) == self.selected.as_ref()))
    }
    pub(crate) fn select(&mut self, id: ConversationId) {
        if self.selected.as_ref() == Some(&id) {
            return;
        }
        self.selected = Some(id);
        self.packet = None;
        self.notice = None;
        self.reload();
    }
    pub(crate) fn deselect(&mut self) {
        self.selected = None;
        self.packet = None;
        self.notice = None;
    }
    pub(crate) fn reload(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if self.loading() {
            return;
        }
        self.send(
            WorkspaceCommand::ReadContext { id: id.clone() },
            Pending::Read(id),
        );
    }
    fn send(&mut self, command: WorkspaceCommand, pending: Pending) {
        match self.client.send(command) {
            Ok(id) => {
                self.pending.insert(id, pending);
            }
            Err(error) => self.notice = Some(error),
        }
    }
    fn edit(&mut self, change: impl FnOnce(&mut WorkspaceContext)) {
        let Some(id) = &self.selected else {
            return;
        };
        let Some(entry) = self.entries.get_mut(id) else {
            return;
        };
        change(&mut entry.value);
        entry.generation = entry.generation.wrapping_add(1);
        entry.dirty = true;
        self.packet = None;
        self.notice = entry.value.validate().err();
    }
    pub(crate) fn set_field(&mut self, field: ContextField, text: String) {
        // Preserve ordinary over-budget edits for review; refuse only an
        // implausibly large paste instead of truncating its meaning.
        if text.len() > 256 * 1024 {
            self.notice =
                Some("That paste is too large; the previous context draft was kept.".into());
            return;
        }
        self.edit(|draft| {
            *match field {
                ContextField::Goal => &mut draft.goal,
                ContextField::Completed => &mut draft.completed,
                ContextField::Remaining => &mut draft.remaining,
                ContextField::Constraints => &mut draft.constraints,
            } = text
        });
    }
    pub(crate) fn remove_material(&mut self, id: &str) {
        self.edit(|draft| draft.materials.retain(|material| material.id != id));
    }
    pub(crate) fn set_material(&mut self, id: &str, text: String) {
        if text.len() > 256 * 1024 {
            self.notice = Some("That material is too large; the previous excerpt was kept.".into());
            return;
        }
        self.edit(|draft| {
            if let Some(material) = draft
                .materials
                .iter_mut()
                .find(|material| material.id == id)
            {
                material.text = text;
                material.truncated = true;
            }
        });
    }
    pub(crate) fn add_note(&mut self) {
        self.edit(|draft| {
            draft.materials.push(WorkspaceMaterial {
                id: uuid::Uuid::new_v4().to_string(),
                label: "Selected note".into(),
                kind: MaterialKind::Note,
                text: String::new(),
                source: None,
                truncated: false,
            })
        });
    }
    pub(crate) fn add_material(&mut self, material: WorkspaceMaterial) {
        self.edit(|draft| draft.materials.push(material));
    }
    pub(crate) fn add_evidence(&mut self, document: &WorkspaceTextDocument) {
        let source = WorkspaceMaterialSource {
            conversation: document.conversation.clone(),
            event_id: document.event_id.clone(),
            revision: document.revision.clone(),
        };
        let material = WorkspaceMaterial {
            id: uuid::Uuid::new_v4().to_string(),
            label: jterm_core::review_input::safe_inline_display(
                &format!("{} · {}", document.source.label(), document.event_id),
                240,
            ),
            kind: MaterialKind::ConversationExcerpt,
            text: document.text.clone(),
            source: Some(source),
            truncated: document.truncated,
        };
        self.edit(|draft| draft.materials.push(material));
    }
    pub(crate) fn keep_local(&mut self) {
        let Some(entry) = self
            .selected
            .as_ref()
            .and_then(|id| self.entries.get_mut(id))
        else {
            return;
        };
        if let Some(remote) = entry.remote.take() {
            entry.value.revision = remote.revision;
            entry.dirty = true;
            entry.generation = entry.generation.wrapping_add(1);
            self.notice = Some("Local materials kept against the latest saved revision. Review and Save to replace it.".into());
            self.packet = None;
        }
    }
    pub(crate) fn use_shared(&mut self) {
        let Some(entry) = self
            .selected
            .as_ref()
            .and_then(|id| self.entries.get_mut(id))
        else {
            return;
        };
        if let Some(remote) = entry.remote.take() {
            entry.value = remote;
            entry.dirty = false;
            entry.generation = entry.generation.wrapping_add(1);
            self.notice = None;
            self.packet = None;
        }
    }
    pub(crate) fn save(&mut self) {
        if self.saving() || self.loading() {
            return;
        }
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(entry) = self.entries.get(&id) else {
            return;
        };
        if entry.remote.is_some() {
            self.notice = Some(
                "Materials changed in another window. Choose which draft to keep before saving."
                    .into(),
            );
            return;
        }
        if let Err(error) = entry.value.validate() {
            self.notice = Some(error);
            return;
        }
        self.send(
            WorkspaceCommand::SaveContext {
                id: id.clone(),
                expected_revision: entry.value.revision,
                context: entry.value.clone(),
            },
            Pending::Save {
                id,
                generation: entry.generation,
            },
        );
    }
    pub(crate) fn build_handoff(&mut self, provider: &str) {
        if self.saving() || self.loading() || self.building() {
            return;
        }
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(entry) = self.entries.get(&id) else {
            return;
        };
        if entry.dirty || entry.remote.is_some() {
            self.notice = Some("Save the reviewed materials before preparing a handoff.".into());
            return;
        }
        let revision = entry.value.revision;
        self.packet = None;
        self.send(
            WorkspaceCommand::BuildHandoff {
                id: id.clone(),
                expected_revision: revision,
                target_provider: provider.into(),
            },
            Pending::Handoff {
                id,
                revision,
                provider: provider.into(),
            },
        );
    }
    pub(crate) fn packet_current(&self) -> bool {
        self.packet.as_ref().is_some_and(|packet| {
            self.selected.as_ref() == Some(&packet.source)
                && self.entries.get(&packet.source).is_some_and(|entry| {
                    !entry.dirty
                        && entry.remote.is_none()
                        && entry.value.revision == packet.source_revision
                })
        })
    }
    fn receive(&mut self, request_id: u64, result: Result<WorkspaceReply, String>) -> bool {
        let Some(pending) = self.pending.remove(&request_id) else {
            return false;
        };
        match pending {
            Pending::Read(id) => match result {
                Ok(WorkspaceReply::Context { context }) if context.conversation == id => {
                    if let Some(entry) = self.entries.get_mut(&id) {
                        if context.revision < entry.value.revision {
                            return false;
                        }
                        if entry.dirty {
                            if context.revision != entry.value.revision {
                                entry.remote = Some(context);
                            }
                        } else {
                            entry.value = context;
                            entry.remote = None;
                        }
                    } else {
                        self.entries.insert(
                            id.clone(),
                            Entry {
                                value: context,
                                generation: 0,
                                dirty: false,
                                remote: None,
                            },
                        );
                    }
                    if self.selected.as_ref() == Some(&id) && self.conflicted() {
                        self.notice = Some(
                            "Saved materials changed elsewhere. Your local draft is retained."
                                .into(),
                        );
                    }
                }
                Err(error) if self.selected.as_ref() == Some(&id) => self.notice = Some(error),
                _ => {}
            },
            Pending::Save { id, generation } => match result {
                Ok(WorkspaceReply::Context { context }) if context.conversation == id => {
                    if let Some(entry) = self.entries.get_mut(&id) {
                        if context.revision < entry.value.revision {
                            return false;
                        }
                        if entry.generation == generation {
                            entry.value = context;
                            entry.dirty = false;
                        } else {
                            entry.value.revision = context.revision;
                        }
                        entry.remote = None;
                    }
                    if self.selected.as_ref() == Some(&id) {
                        self.notice = Some("Materials saved. They are not sent to a model until you create and submit a new handoff draft.".into());
                    }
                }
                Err(error) if self.selected.as_ref() == Some(&id) => {
                    self.notice = Some(format!(
                        "Materials were not saved: {error}. Your local draft is retained."
                    ));
                    self.reload();
                }
                _ => {}
            },
            Pending::Handoff {
                id,
                revision,
                provider,
            } => {
                if self.selected.as_ref() != Some(&id) {
                    return false;
                }
                match result {
                    Ok(WorkspaceReply::Handoff { packet })
                        if packet.source == id
                            && packet.source_revision == revision
                            && packet.target_provider == provider
                            && self.entries.get(&id).is_some_and(|entry| {
                                !entry.dirty
                                    && entry.remote.is_none()
                                    && entry.value.revision == revision
                            }) =>
                    {
                        self.packet = Some(packet);
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                    _ => self.notice = Some(
                        "The handoff materials changed; prepare a new preview before continuing."
                            .into(),
                    ),
                }
            }
        }
        true
    }
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        for WorkspaceEvent::Reply { request_id, result } in self.client.poll() {
            changed |= self.receive(request_id, result);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn editor() -> (ContextEditor, ConversationId) {
        let id = ConversationId("fixture-context".into());
        let mut editor = ContextEditor {
            selected: Some(id.clone()),
            ..Default::default()
        };
        editor.entries.insert(
            id.clone(),
            Entry {
                value: WorkspaceContext::empty(id.clone()),
                generation: 0,
                dirty: false,
                remote: None,
            },
        );
        (editor, id)
    }
    #[test]
    fn save_ack_preserves_edits_typed_while_saving() {
        let (mut editor, id) = editor();
        editor.set_field(ContextField::Goal, "submitted".into());
        let generation = editor.entries[&id].generation;
        let mut saved = editor.draft().unwrap().clone();
        saved.revision = 1;
        editor.pending.insert(1, Pending::Save { id, generation });
        editor.set_field(ContextField::Goal, "newer unsaved goal".into());
        editor.receive(1, Ok(WorkspaceReply::Context { context: saved }));
        assert_eq!(editor.draft().unwrap().goal, "newer unsaved goal");
        assert_eq!(editor.draft().unwrap().revision, 1);
        assert!(editor.dirty());
    }
    #[test]
    fn remote_revision_requires_an_explicit_draft_choice() {
        let (mut editor, id) = editor();
        editor.set_field(ContextField::Goal, "local".into());
        let mut remote = WorkspaceContext::empty(id.clone());
        remote.revision = 3;
        remote.goal = "shared".into();
        editor.pending.insert(1, Pending::Read(id));
        editor.receive(1, Ok(WorkspaceReply::Context { context: remote }));
        assert!(editor.conflicted());
        assert_eq!(editor.draft().unwrap().goal, "local");
        editor.keep_local();
        assert_eq!(editor.draft().unwrap().goal, "local");
        assert_eq!(editor.draft().unwrap().revision, 3);
        assert!(editor.dirty());
    }
    #[test]
    fn old_context_read_does_not_retarget_the_visible_editor() {
        let (mut editor, old) = editor();
        let current = ConversationId("other-context".into());
        let mut current_value = WorkspaceContext::empty(current.clone());
        current_value.goal = "Current draft".into();
        editor.entries.insert(
            current.clone(),
            Entry {
                value: current_value,
                generation: 0,
                dirty: true,
                remote: None,
            },
        );
        editor.selected = Some(current);
        editor.pending.insert(9, Pending::Read(old.clone()));
        let mut value = WorkspaceContext::empty(old);
        value.goal = "Late old result".into();
        editor.receive(9, Ok(WorkspaceReply::Context { context: value }));
        assert_eq!(editor.draft().unwrap().goal, "Current draft");
    }

    #[test]
    fn edited_context_retires_a_pending_handoff_preview() {
        let (mut editor, id) = editor();
        editor.pending.insert(
            8,
            Pending::Handoff {
                id: id.clone(),
                revision: 0,
                provider: "codex".into(),
            },
        );
        editor.set_field(ContextField::Constraints, "New constraint".into());
        let packet = WorkspaceHandoff {
            source: id,
            source_revision: 0,
            target_provider: "codex".into(),
            title: "Old packet".into(),
            cwd: "/fixture".into(),
            body: "Old context".into(),
            materials_count: 0,
        };
        editor.receive(8, Ok(WorkspaceReply::Handoff { packet }));
        assert!(editor.packet.is_none());
        assert!(editor.dirty());
    }
}
