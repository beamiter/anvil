//! Read-only result capture and explicit queue intent, shared by native UIs.
//! Queue readiness never starts an Agent, validation or remote job.
use jterm_core::agent_workspace::{
    ConversationId, ConversationKind, RuntimeState, WorkspaceClient, WorkspaceCommand,
    WorkspaceEvent, WorkspaceExecutionLocation, WorkspaceQueueChange, WorkspaceQueueIntent,
    WorkspaceQueueSnapshot, WorkspaceQueueUpdate, WorkspaceReply, WorkspaceResultReport,
    WorkspaceRow,
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

enum Pending {
    Result { id: ConversationId, generation: u64 },
    Queue { write: bool },
    Catalog { generation: u64 },
    IndexTasks { rows: Vec<WorkspaceRow> },
}

pub(crate) struct WorkspaceReview {
    client: WorkspaceClient,
    pending: HashMap<u64, Pending>,
    selected: Option<ConversationId>,
    generation: u64,
    catalog_generation: u64,
    catalog_staging: Vec<WorkspaceRow>,
    started: bool,
    refreshed: Instant,
    local_tasks: Vec<WorkspaceRow>,
    indexed_tasks: HashMap<ConversationId, WorkspaceRow>,
    index_retry_at: Instant,
    pub(crate) report: Option<WorkspaceResultReport>,
    pub(crate) report_loading: bool,
    pub(crate) queue: WorkspaceQueueSnapshot,
    pub(crate) queue_loading: bool,
    pub(crate) queue_saving: bool,
    pub(crate) rows: Vec<WorkspaceRow>,
    pub(crate) catalog_complete: bool,
    pub(crate) catalog_loading: bool,
    pub(crate) notice: Option<String>,
}

impl Default for WorkspaceReview {
    fn default() -> Self {
        Self {
            client: WorkspaceClient::new(),
            pending: HashMap::new(),
            selected: None,
            generation: 0,
            catalog_generation: 0,
            catalog_staging: Vec::new(),
            started: false,
            refreshed: Instant::now(),
            local_tasks: Vec::new(),
            indexed_tasks: HashMap::new(),
            index_retry_at: Instant::now(),
            report: None,
            report_loading: false,
            queue: WorkspaceQueueSnapshot::default(),
            queue_loading: false,
            queue_saving: false,
            rows: Vec::new(),
            catalog_complete: false,
            catalog_loading: false,
            notice: None,
        }
    }
}

impl WorkspaceReview {
    /// Publish source-qualified identities for unstarted local tasks. The host
    /// stores discovery metadata only; this does not claim an execution lease.
    pub(crate) fn index_tasks(&mut self, rows: &[WorkspaceRow]) {
        self.local_tasks = rows
            .iter()
            .filter(|row| row.kind == ConversationKind::Task && row.facts.source_sequence.is_none())
            .cloned()
            .map(|mut row| {
                row.runtime = RuntimeState::Idle;
                row.facts = row.facts.recorded_only();
                row
            })
            .collect();
        self.local_tasks.sort_by(|a, b| a.id.cmp(&b.id));
        self.send_task_index();
    }
    fn send_task_index(&mut self) {
        if Instant::now() < self.index_retry_at
            || self
                .pending
                .values()
                .any(|pending| matches!(pending, Pending::IndexTasks { .. }))
        {
            return;
        }
        let rows: Vec<_> = self
            .local_tasks
            .iter()
            .filter(|row| self.indexed_tasks.get(&row.id) != Some(row))
            .take(2048)
            .cloned()
            .collect();
        if rows.is_empty() {
            return;
        }
        if !self.send(
            WorkspaceCommand::IndexSessions {
                sessions: rows.clone(),
            },
            Pending::IndexTasks { rows },
        ) {
            self.index_retry_at = Instant::now() + Duration::from_secs(5);
        }
    }
    pub(crate) fn task_indexed(&self, id: &ConversationId) -> bool {
        self.indexed_tasks.contains_key(id)
            || self
                .rows
                .iter()
                .any(|row| row.kind == ConversationKind::Task && &row.id == id)
    }
    /// Snapshot discovery authority once for a whole dependency render. Repeated
    /// `task_indexed` calls would scan the catalog for every candidate (O(n²)).
    /// Borrow identities so this does not clone potentially long source IDs or
    /// retain stale authority across a catalog refresh.
    pub(crate) fn indexed_task_ids(&self) -> HashSet<&ConversationId> {
        self.indexed_tasks
            .keys()
            .chain(
                self.rows
                    .iter()
                    .filter(|row| row.kind == ConversationKind::Task)
                    .map(|row| &row.id),
            )
            .collect()
    }
    fn send(&mut self, command: WorkspaceCommand, pending: Pending) -> bool {
        match self.client.send(command) {
            Ok(id) => {
                self.pending.insert(id, pending);
                true
            }
            Err(error) => {
                self.notice = Some(error);
                false
            }
        }
    }
    pub(crate) fn start(&mut self) {
        if !self.started {
            self.started = true;
            self.refresh();
        }
    }
    pub(crate) fn selected(&self) -> Option<&ConversationId> {
        self.selected.as_ref()
    }
    pub(crate) fn select_task(&mut self, id: ConversationId) {
        if self.selected.as_ref() == Some(&id) {
            return;
        }
        self.selected = Some(id);
        self.generation = self.generation.wrapping_add(1);
        self.report = None;
        self.report_loading = false;
        self.refresh_result();
    }
    pub(crate) fn clear_task(&mut self) {
        self.selected = None;
        self.generation = self.generation.wrapping_add(1);
        self.report = None;
        self.report_loading = false;
    }
    pub(crate) fn refresh_result(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if self.report_loading {
            return;
        }
        self.report_loading = self.send(
            WorkspaceCommand::InspectResult { id: id.clone() },
            Pending::Result {
                id,
                generation: self.generation,
            },
        );
    }
    pub(crate) fn refresh(&mut self) {
        if !self.queue_loading && !self.queue_saving {
            self.queue_loading = self.send(
                WorkspaceCommand::QueueSnapshot,
                Pending::Queue { write: false },
            );
        }
        if !self.catalog_loading {
            self.catalog_generation = self.catalog_generation.wrapping_add(1);
            self.catalog_staging.clear();
            self.catalog_complete = false;
            self.catalog_loading = self.send(
                WorkspaceCommand::CatalogPage {
                    before: None,
                    limit: 200,
                },
                Pending::Catalog {
                    generation: self.catalog_generation,
                },
            );
        }
        self.refreshed = Instant::now();
    }
    pub(crate) fn set_queue(&mut self, id: ConversationId, dependencies: Vec<ConversationId>) {
        if self.queue_loading || self.queue_saving {
            return;
        }
        let intent = WorkspaceQueueIntent {
            id,
            dependencies,
            location: WorkspaceExecutionLocation::Local,
        };
        if let Err(error) = intent.validate() {
            self.notice = Some(error);
            return;
        }
        self.write_queue(WorkspaceQueueChange::Upsert { intent });
    }
    pub(crate) fn remove_queue(&mut self, id: ConversationId) {
        if self.queue_loading || self.queue_saving {
            return;
        }
        self.write_queue(WorkspaceQueueChange::Remove { id });
    }
    fn write_queue(&mut self, change: WorkspaceQueueChange) {
        let update = WorkspaceQueueUpdate {
            expected_revision: self.queue.revision,
            change,
        };
        self.queue_saving = self.send(
            WorkspaceCommand::QueueUpdate { update },
            Pending::Queue { write: true },
        );
    }
    pub(crate) fn set_dependency(
        &mut self,
        id: ConversationId,
        dependency: ConversationId,
        enabled: bool,
    ) {
        let mut dependencies = self
            .queue
            .entries
            .iter()
            .find(|entry| entry.intent.id == id)
            .map(|entry| entry.intent.dependencies.clone())
            .unwrap_or_default();
        dependencies.retain(|current| current != &dependency);
        if enabled {
            dependencies.push(dependency);
        }
        self.set_queue(id, dependencies);
    }
    /// Prefer the newest source sequence; an unsequenced local fallback
    /// cannot replace a hosted observation.
    pub(crate) fn combined_rows(&self, local: &[WorkspaceRow]) -> Vec<WorkspaceRow> {
        let mut rows: HashMap<_, _> = local
            .iter()
            .map(|row| (row.id.clone(), row.clone()))
            .collect();
        for row in &self.rows {
            // A discovered task has identity and organization, but deliberately
            // no current execution facts. Only its owning TaskManager can
            // enrich it; a native host observation must always remain intact.
            if let Some(local) = rows.get(&row.id).filter(|local| {
                row.kind == ConversationKind::Task
                    && row.facts.source_sequence.is_none()
                    && row.facts.task_status.is_none()
                    && local.facts.source_sequence.is_none()
                    && local.facts.task_id.is_some()
                    && local.facts.task_status.is_some()
            }) {
                let mut enriched = row.clone();
                enriched.facts = local.facts.clone();
                if row.runtime != RuntimeState::Conflict {
                    enriched.runtime = local.runtime;
                }
                rows.insert(row.id.clone(), enriched);
                continue;
            }
            let keep_local = rows.get(&row.id).is_some_and(|local| {
                match (local.facts.source_sequence, row.facts.source_sequence) {
                    (Some(local), Some(host)) => local >= host,
                    (Some(_), None) => true,
                    (None, None) => local.updated_at_ms > row.updated_at_ms,
                    _ => false,
                }
            });
            if !keep_local {
                rows.insert(row.id.clone(), row.clone());
            }
        }
        let mut rows: Vec<_> = rows.into_values().collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows
    }
    fn receive(&mut self, request_id: u64, result: Result<WorkspaceReply, String>) -> bool {
        let Some(pending) = self.pending.remove(&request_id) else {
            return false;
        };
        match pending {
            Pending::IndexTasks { rows } => match result {
                Ok(WorkspaceReply::Ack) => {
                    for row in rows {
                        self.indexed_tasks.insert(row.id.clone(), row);
                    }
                }
                Err(error) => {
                    self.notice = Some(format!("Task directory: {error}"));
                    self.index_retry_at = Instant::now() + Duration::from_secs(5);
                }
                _ => {
                    self.notice = Some("The host did not acknowledge the task identities.".into());
                    self.index_retry_at = Instant::now() + Duration::from_secs(5);
                }
            },
            Pending::Result { id, generation } => {
                if generation != self.generation || self.selected.as_ref() != Some(&id) {
                    return false;
                }
                self.report_loading = false;
                match result {
                    Ok(WorkspaceReply::Result { report }) if report.conversation == id => {
                        self.report = Some(*report);
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(format!("Result evidence: {error}")),
                    _ => {
                        self.notice = Some(
                            "A different result was returned; no evidence was replaced.".into(),
                        )
                    }
                }
            }
            Pending::Queue { write } => {
                self.queue_loading = false;
                if write {
                    self.queue_saving = false;
                }
                match result {
                    Ok(WorkspaceReply::Queue { snapshot }) => {
                        if snapshot.revision >= self.queue.revision {
                            self.queue = snapshot;
                        }
                        if write {
                            self.notice = Some(
                                "Queue saved. Ready tasks still require an explicit Start.".into(),
                            );
                        }
                    }
                    Err(error) => {
                        self.notice = Some(format!("Queue not changed: {error}"));
                        if write {
                            self.queue_loading = self.send(
                                WorkspaceCommand::QueueSnapshot,
                                Pending::Queue { write: false },
                            );
                        }
                    }
                    _ => self.notice = Some("The host did not acknowledge the queue state.".into()),
                }
            }
            Pending::Catalog { generation } => {
                if generation != self.catalog_generation {
                    return false;
                }
                self.catalog_loading = false;
                match result {
                    Ok(WorkspaceReply::CatalogPage {
                        rows,
                        next_before,
                        total,
                    }) => {
                        for row in rows {
                            if let Some(old) =
                                self.catalog_staging.iter_mut().find(|old| old.id == row.id)
                            {
                                *old = row;
                            } else {
                                self.catalog_staging.push(row);
                            }
                        }
                        if let Some(before) =
                            next_before.filter(|_| self.catalog_staging.len() < 8192)
                        {
                            self.catalog_loading = self.send(
                                WorkspaceCommand::CatalogPage {
                                    before: Some(before),
                                    limit: 200,
                                },
                                Pending::Catalog { generation },
                            );
                        } else {
                            self.rows = std::mem::take(&mut self.catalog_staging);
                            self.catalog_complete =
                                next_before.is_none() && self.rows.len() >= total;
                        }
                    }
                    Err(error) => self.notice = Some(format!("Catalog: {error}")),
                    _ => self.notice = Some("The host did not return a catalog page.".into()),
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
        self.send_task_index();
        if self.started && self.refreshed.elapsed() >= Duration::from_secs(5) {
            self.refresh();
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(sequence: Option<u64>, title: &str) -> WorkspaceRow {
        let mut row = WorkspaceRow {
            id: ConversationId("fixture-task".into()),
            kind: ConversationKind::Task,
            title: title.into(),
            provider: "Codex".into(),
            cwd: String::new(),
            preview: String::new(),
            updated_at_ms: 1,
            archived: false,
            pinned: false,
            tags: Vec::new(),
            runtime: RuntimeState::Idle,
            facts: Default::default(),
        };
        row.facts.source_sequence = sequence;
        row
    }
    #[test]
    fn indexed_task_snapshot_matches_single_lookup_and_refresh() {
        let mut state = WorkspaceReview::default();
        let mut acknowledged = row(None, "Acknowledged");
        acknowledged.id = ConversationId("acknowledged".into());
        state
            .indexed_tasks
            .insert(acknowledged.id.clone(), acknowledged.clone());
        let mut hosted = row(Some(1), "Hosted");
        hosted.id = ConversationId("hosted".into());
        let mut chat = row(None, "Not a task");
        chat.id = ConversationId("chat".into());
        chat.kind = ConversationKind::Api;
        state.rows = vec![hosted.clone(), acknowledged.clone(), chat.clone()];
        let missing = ConversationId("pending-or-local-only".into());
        {
            let ids = state.indexed_task_ids();
            assert_eq!(ids.len(), 2, "duplicate authority must be deduplicated");
            for id in [&acknowledged.id, &hosted.id, &chat.id, &missing] {
                assert_eq!(ids.contains(id), state.task_indexed(id));
            }
        }
        state.rows.clear();
        let ids = state.indexed_task_ids();
        assert!(ids.contains(&acknowledged.id));
        assert!(
            !ids.contains(&hosted.id),
            "refresh must not leave stale authority"
        );
        assert!(!ids.contains(&chat.id));
        assert!(!ids.contains(&missing));
    }

    #[test]
    fn immediate_runtime_fact_wins_over_older_catalog_but_fallback_does_not() {
        let state = WorkspaceReview {
            rows: vec![row(Some(10), "Catalog")],
            ..Default::default()
        };
        assert_eq!(
            state.combined_rows(&[row(Some(11), "Live current")])[0].title,
            "Live current"
        );
        assert_eq!(
            state.combined_rows(&[row(None, "Local fallback")])[0].title,
            "Catalog"
        );
        assert_eq!(
            state.combined_rows(&[row(Some(9), "Older observation")])[0].title,
            "Catalog"
        );
    }
    #[test]
    fn newer_unsequenced_metadata_survives_an_older_catalog_refresh() {
        let state = WorkspaceReview {
            rows: vec![row(None, "Old API title")],
            ..Default::default()
        };
        let mut current = row(None, "Renamed API");
        current.updated_at_ms = 2;
        assert_eq!(state.combined_rows(&[current])[0].title, "Renamed API");
    }

    #[test]
    fn discovered_created_task_keeps_shared_metadata_and_local_current_facts() {
        let mut state = WorkspaceReview::default();
        let mut discovered = row(None, "Shared title");
        discovered.pinned = true;
        discovered.updated_at_ms = 9;
        let mut local = row(None, "Local title");
        local.facts.task_id = Some(jterm_core::agent_task::TaskId::new());
        local.facts.task_status = Some(jterm_core::agent_task::TaskStatus::Created);
        state.rows = vec![discovered.clone()];
        let merged = state.combined_rows(&[local.clone()]).remove(0);
        assert_eq!(merged.title, "Shared title");
        assert!(merged.pinned);
        assert_eq!(merged.facts, local.facts);
        discovered.runtime = RuntimeState::Conflict;
        state.rows = vec![discovered];
        assert_eq!(
            state.combined_rows(&[local])[0].runtime,
            RuntimeState::Conflict
        );
    }

    #[test]
    fn only_acknowledged_task_identity_unlocks_context_and_queue() {
        let mut state = WorkspaceReview::default();
        let task = row(None, "New task");
        state.pending.insert(
            1,
            Pending::IndexTasks {
                rows: vec![task.clone()],
            },
        );
        assert!(!state.task_indexed(&task.id));
        assert!(state.receive(1, Ok(WorkspaceReply::Ack)));
        assert!(state.task_indexed(&task.id));
    }

    #[test]
    fn hosted_stream_updates_do_not_republish_discovery_metadata() {
        let mut state = WorkspaceReview::default();
        state.index_tasks(&[row(Some(10), "Hosted task")]);
        state.index_tasks(&[row(Some(11), "Hosted task")]);
        assert!(state.local_tasks.is_empty());
        assert!(state.pending.is_empty());
    }

    #[test]
    fn late_result_cannot_replace_a_new_selection() {
        let mut state = WorkspaceReview {
            selected: Some(ConversationId("current".into())),
            generation: 2,
            ..Default::default()
        };
        state.pending.insert(
            1,
            Pending::Result {
                id: ConversationId("previous".into()),
                generation: 1,
            },
        );
        assert!(!state.receive(1, Err("Previous task is gone".into())));
        assert!(state.report.is_none());
        assert!(state.notice.is_none());
    }

    #[test]
    fn boxed_result_preserves_current_selection_identity() {
        for matches_selection in [true, false] {
            let id = ConversationId("selected".into());
            let mut state = WorkspaceReview {
                selected: Some(id.clone()),
                generation: 7,
                report_loading: true,
                ..Default::default()
            };
            state.pending.insert(
                1,
                Pending::Result {
                    id: id.clone(),
                    generation: 7,
                },
            );
            let report = WorkspaceResultReport {
                conversation: if matches_selection {
                    id.clone()
                } else {
                    ConversationId("other".into())
                },
                task_id: jterm_core::agent_task::TaskId::new(),
                run_id: "fixture-run".into(),
                source_sequence: 9,
                baseline: None,
                current: None,
                validation: None,
                validation_current: false,
                validation_status: "Not run".into(),
                validation_output: String::new(),
                validation_output_truncated: false,
                error: None,
            };
            assert!(state.receive(
                1,
                Ok(WorkspaceReply::Result {
                    report: Box::new(report)
                })
            ));
            assert!(!state.report_loading);
            assert_eq!(state.report.is_some(), matches_selection);
            if let Some(report) = &state.report {
                assert_eq!(report.conversation, id);
                assert_eq!(report.run_id, "fixture-run");
                assert_eq!(report.source_sequence, 9);
            } else {
                assert!(state
                    .notice
                    .as_deref()
                    .unwrap()
                    .contains("different result"));
            }
        }
    }
}
