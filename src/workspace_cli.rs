//! Shared, source-qualified CLI organization. Legacy facts are import hints only.
use jterm_core::agent_sessions::{
    AgentSession, SessionCatalog, SessionLibrarySnapshot, SessionProvider,
};
use jterm_core::agent_workspace::{
    ConversationId, ConversationKind, WorkspaceClient, WorkspaceCommand, WorkspaceEvent,
    WorkspaceOrganization, WorkspaceReply, WorkspaceRow,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

type SourceKey = (SessionProvider, String, PathBuf);
type IndexedSources = (HashMap<SourceKey, ConversationId>, Vec<WorkspaceRow>);

fn source_key(session: &AgentSession) -> SourceKey {
    (session.provider, session.id.clone(), session.source.clone())
}

/// Canonical provider homes and the machine identity require filesystem reads.
/// Resolve them once per changed scan in this single bounded worker.
fn index_sources(catalog: SessionCatalog, legacy: SessionLibrarySnapshot) -> IndexedSources {
    let mut counts = HashMap::new();
    for session in &catalog.sessions {
        *counts
            .entry((session.provider, session.id.as_str()))
            .or_insert(0usize) += 1;
    }
    let mut identities = HashMap::new();
    let mut rows = Vec::new();
    for session in &catalog.sessions {
        let Ok(mut row) = WorkspaceRow::from_agent(session) else {
            continue;
        };
        identities.insert(source_key(session), row.id.clone());
        // Provider/UUID legacy facts are ambiguous across multiple homes.
        if counts.get(&(session.provider, session.id.as_str())) == Some(&1) {
            let facts = legacy.facts(session.provider, &session.id);
            if let Some(alias) = facts.alias {
                row.title = alias.to_string();
            }
            row.pinned = facts.pinned;
            row.archived |= facts.archived;
            row.tags = facts.tags.to_vec();
        }
        rows.push(row);
    }
    (identities, rows)
}

enum Pending {
    Index,
    Catalog,
    Organize(Vec<WorkspaceOrganization>),
}

pub(crate) struct SharedCli {
    client: WorkspaceClient,
    pub rows: HashMap<ConversationId, WorkspaceRow>,
    indexed: Vec<WorkspaceRow>,
    projected: Vec<WorkspaceRow>,
    identities: HashMap<SourceKey, ConversationId>,
    index_job: Option<Receiver<IndexedSources>>,
    last_catalog: Option<SessionCatalog>,
    last_legacy: Option<SessionLibrarySnapshot>,
    task_identities: HashMap<(String, String), Option<ConversationId>>,
    indexed_tasks: Vec<WorkspaceRow>,
    pending: HashMap<u64, Pending>,
    settled: HashMap<ConversationId, Result<(), String>>,
    refreshed: Option<Instant>,
    retry_after: Option<Instant>,
    pub error: Option<String>,
}
impl SharedCli {
    pub fn new() -> Self {
        Self {
            client: WorkspaceClient::new(),
            rows: HashMap::new(),
            indexed: Vec::new(),
            projected: Vec::new(),
            identities: HashMap::new(),
            index_job: None,
            last_catalog: None,
            last_legacy: None,
            task_identities: HashMap::new(),
            indexed_tasks: Vec::new(),
            pending: HashMap::new(),
            settled: HashMap::new(),
            refreshed: None,
            retry_after: None,
            error: None,
        }
    }
    #[cfg(test)]
    pub fn isolated_state(&mut self, path: &std::path::Path) {
        self.client = WorkspaceClient::for_state_dir(path);
    }

    pub fn id(&self, session: &AgentSession) -> Option<&ConversationId> {
        self.identities.get(&source_key(session))
    }
    pub fn task_id(&mut self, namespace: &str, native_id: &str) -> Option<ConversationId> {
        let key = (namespace.to_owned(), native_id.to_owned());
        if !self.task_identities.contains_key(&key) {
            // The TaskManager already bounds its owned records; this also
            // caps obsolete namespace entries without an unbounded memo table.
            if self.task_identities.len() >= 4096 {
                return None;
            }
            self.task_identities.insert(
                key.clone(),
                jterm_core::agent_workspace::task_id(namespace, native_id).ok(),
            );
        }
        self.task_identities.get(&key).cloned().flatten()
    }

    fn send(&mut self, command: WorkspaceCommand, pending: Pending) -> Result<(), String> {
        let id = self.client.send(command)?;
        self.pending.insert(id, pending);
        Ok(())
    }
    pub fn organize(&mut self, changes: Vec<WorkspaceOrganization>) -> Result<(), String> {
        if changes.iter().any(|change| self.is_writing(&change.id)) {
            return Err("This conversation is still saving. Try again when it finishes.".into());
        }
        self.send(
            WorkspaceCommand::Organize {
                changes: changes.clone(),
            },
            Pending::Organize(changes),
        )
    }
    pub fn index_tasks(&mut self, rows: Vec<WorkspaceRow>) {
        if rows == self.indexed_tasks || self.retry_after.is_some_and(|at| Instant::now() < at) {
            return;
        }
        for chunk in rows.chunks(2048) {
            if let Err(error) = self.send(
                WorkspaceCommand::IndexSessions {
                    sessions: chunk.to_vec(),
                },
                Pending::Index,
            ) {
                self.error = Some(error);
                return;
            }
        }
        self.indexed_tasks = rows;
    }
    pub fn is_writing(&self, id: &ConversationId) -> bool {
        self.pending
            .values()
            .any(|p| matches!(p, Pending::Organize(changes) if changes.iter().any(|c| &c.id == id)))
    }
    pub fn take_result(&mut self, id: &ConversationId) -> Option<Result<(), String>> {
        self.settled.remove(id)
    }
    pub fn poll(&mut self, catalog: &SessionCatalog, legacy: &SessionLibrarySnapshot) {
        for event in self.client.poll() {
            let WorkspaceEvent::Reply { request_id, result } = event;
            let Some(pending) = self.pending.remove(&request_id) else {
                continue;
            };
            match (pending, result) {
                (Pending::Catalog, Ok(WorkspaceReply::Catalog { rows })) => {
                    self.rows = rows
                        .into_iter()
                        .filter(|r| {
                            r.kind == ConversationKind::Cli || r.kind == ConversationKind::Task
                        })
                        .map(|r| (r.id.clone(), r))
                        .collect();
                    self.error = None;
                }
                (Pending::Index, Ok(WorkspaceReply::Ack)) => {
                    self.refreshed = None;
                }
                (Pending::Organize(changes), Ok(WorkspaceReply::Ack)) => {
                    for change in changes {
                        if let Some(row) = self.rows.get_mut(&change.id) {
                            if let Some(title) = change.title {
                                row.title = title;
                            }
                            if let Some(tags) = change.tags {
                                row.tags = tags;
                            }
                            if let Some(pinned) = change.pinned {
                                row.pinned = pinned;
                            }
                            if let Some(archived) = change.archived {
                                row.archived = archived;
                            }
                        }
                        self.settled.insert(change.id, Ok(()));
                    }
                    self.refreshed = None;
                    self.error = None;
                }
                (pending, Err(error)) => {
                    if let Pending::Organize(changes) = pending {
                        for change in changes {
                            self.settled.insert(change.id, Err(error.clone()));
                        }
                    } else if matches!(pending, Pending::Index) {
                        self.indexed.clear();
                        self.indexed_tasks.clear();
                    }
                    self.error = Some(error);
                    self.retry_after = Some(Instant::now() + Duration::from_secs(5));
                }
                _ => {
                    self.error = Some("Unexpected shared session acknowledgement".into());
                }
            }
        }
        if let Some(result) = self.index_job.as_ref().map(|job| job.try_recv()) {
            match result {
                Ok((identities, rows)) => {
                    self.index_job = None;
                    if self.last_catalog.as_ref() == Some(catalog)
                        && self.last_legacy.as_ref() == Some(legacy)
                    {
                        self.identities = identities;
                        self.projected = rows;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.index_job = None;
                    self.last_catalog = None;
                    self.error =
                        Some("Session indexing stopped; refreshing the source catalog.".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.index_job.is_none()
            && (self.last_catalog.as_ref() != Some(catalog)
                || self.last_legacy.as_ref() != Some(legacy))
        {
            let snapshot = catalog.clone();
            let metadata = legacy.clone();
            self.last_catalog = Some(snapshot.clone());
            self.last_legacy = Some(metadata.clone());
            let (send, receive) = mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("workspace-session-index".into())
                .spawn(move || {
                    let _ = send.send(index_sources(snapshot, metadata));
                }) {
                Ok(_) => self.index_job = Some(receive),
                Err(error) => {
                    self.error = Some(error.to_string());
                    self.last_catalog = None;
                }
            }
        }
        if self.retry_after.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if self.projected != self.indexed
            && !self.pending.values().any(|p| matches!(p, Pending::Index))
        {
            let rows = self.projected.clone();
            let mut failed = false;
            for chunk in rows.chunks(2048) {
                if let Err(error) = self.send(
                    WorkspaceCommand::IndexSessions {
                        sessions: chunk.to_vec(),
                    },
                    Pending::Index,
                ) {
                    self.error = Some(error);
                    failed = true;
                    break;
                }
            }
            if !failed {
                self.indexed = rows;
            }
        }
        if self
            .refreshed
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
            && !self
                .pending
                .values()
                .any(|p| matches!(p, Pending::Catalog | Pending::Index))
            && self
                .send(WorkspaceCommand::Refresh, Pending::Catalog)
                .is_ok()
        {
            self.refreshed = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_core::agent_sessions::SessionStatus;

    #[test]
    fn cached_source_identity_changes_only_after_a_catalog_revision() {
        let root = std::env::temp_dir().join(format!(
            "gtk-source-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first_home = root.join("first");
        let second_home = root.join("second");
        std::fs::create_dir_all(first_home.join("sessions")).unwrap();
        std::fs::create_dir_all(second_home.join("sessions")).unwrap();
        let profile = root.join("profile");
        std::os::unix::fs::symlink(&first_home, &profile).unwrap();
        let session = AgentSession {
            provider: SessionProvider::Codex,
            id: "01912345-1234-7123-8123-123456789abc".into(),
            title: "Cache regression".into(),
            cwd: root.clone(),
            updated_at_ms: 1,
            status: SessionStatus::Idle,
            preview: String::new(),
            branch: None,
            source: profile.join("sessions/rollout.jsonl"),
            archived: false,
        };
        let mut catalog = SessionCatalog::default();
        catalog.sessions.push(session.clone());
        let legacy = SessionLibrarySnapshot::default();
        let mut cache = SharedCli::new();
        cache.isolated_state(&root.join("unavailable-host"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.id(&session).is_none() {
            cache.poll(&catalog, &legacy);
            assert!(
                Instant::now() < deadline,
                "offline indexing stalled behind host retry"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let first = cache.id(&session).unwrap().clone();
        std::fs::remove_file(&profile).unwrap();
        std::os::unix::fs::symlink(&second_home, &profile).unwrap();
        let changed = jterm_core::agent_workspace::session_id(&session).unwrap();
        assert_ne!(first, changed);
        for _ in 0..5 {
            cache.poll(&catalog, &legacy);
            assert_eq!(
                cache.id(&session),
                Some(&first),
                "a UI poll recomputed filesystem identity"
            );
        }
        catalog.sessions[0].updated_at_ms += 1;
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.id(&session) != Some(&changed) {
            cache.poll(&catalog, &legacy);
            assert!(Instant::now() < deadline, "changed scan was not indexed");
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(cache);
        std::fs::remove_dir_all(root).unwrap();
    }
}
