//! GUI-independent bridge from the bounded live ChatStore to the shared host.
//! The provider is started only after a revision-bound submission receipt.
use jterm_core::agent_workspace::{
    ConversationId, ConversationKind, StoredChat, SubmissionReceipt, WorkspaceClient,
    WorkspaceCommand as Command, WorkspaceEvent, WorkspaceReply as Reply, WorkspaceRow,
};
use jterm_core::ai::{ChatSnapshot, ChatStore, ConversationSnapshot, RequestToken};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub(crate) enum Event {
    Opened(u64),
    Created {
        request: u64,
        chat_id: u64,
    },
    Authorized {
        request: u64,
        chat_id: u64,
        receipt: SubmissionReceipt,
    },
    Rejected {
        request: u64,
        message: String,
    },
    Deleted(u64),
    Earlier {
        chat_id: u64,
        text: String,
    },
    Error(String),
}

struct Known {
    chat: StoredChat,
    saved: String,
    writing: bool,
    blocked: bool,
}

enum Pending {
    Catalog,
    Ack,
    Import(Vec<u64>),
    Bind(u64),
    Fresh(u64),
    Open,
    Create,
    Save {
        local: u64,
        snapshot: String,
    },
    Draft {
        local: u64,
    },
    Begin {
        local: u64,
        request: u64,
    },
    Settle {
        local: u64,
    },
    Reload {
        local: u64,
        open: bool,
        keep_local: bool,
    },
    Delete {
        local: u64,
    },
    Mutation {
        id: ConversationId,
    },
    PrimeHistory {
        local: u64,
    },
    History {
        local: u64,
    },
}

struct Submission {
    request: u64,
    text: String,
    exact_draft: bool,
    sent: bool,
}

pub(crate) struct SharedChats {
    client: WorkspaceClient,
    namespace: String,
    pub rows: Vec<WorkspaceRow>,
    known: HashMap<u64, Known>,
    importing: HashSet<u64>,
    pending: HashMap<u64, Pending>,
    submissions: HashMap<u64, Submission>,
    receipts: HashMap<RequestToken, SubmissionReceipt>,
    settlements: HashMap<RequestToken, Result<String, ()>>,
    rejected_receipts: Vec<(u64, SubmissionReceipt)>,
    next_request: u64,
    refreshed: Option<Instant>,
    history_cursor: HashMap<u64, Option<u64>>,
    retry_after: Option<Instant>,
    pending_deletes: HashSet<u64>,
    legacy_sources: HashSet<u64>,
}

impl SharedChats {
    pub fn new(namespace: String) -> Self {
        Self {
            client: WorkspaceClient::new(),
            namespace,
            rows: Vec::new(),
            known: HashMap::new(),
            importing: HashSet::new(),
            pending: HashMap::new(),
            submissions: HashMap::new(),
            receipts: HashMap::new(),
            settlements: HashMap::new(),
            rejected_receipts: Vec::new(),
            next_request: 0,
            refreshed: None,
            history_cursor: HashMap::new(),
            retry_after: None,
            pending_deletes: HashSet::new(),
            legacy_sources: HashSet::new(),
        }
    }

    pub fn restore_legacy_sources(&mut self, store: &ChatStore) {
        self.legacy_sources
            .extend(store.summaries().into_iter().map(|row| row.id));
    }

    fn send(&mut self, command: Command, pending: Pending) -> Result<(), String> {
        let id = self.client.send(command)?;
        self.pending.insert(id, pending);
        Ok(())
    }

    pub fn prepare(&mut self, local: u64, text: String, exact_draft: bool) -> Result<u64, String> {
        if self.submissions.contains_key(&local)
            || self.receipts.keys().any(|token| token.chat_id == local)
        {
            return Err("This conversation already has a submission in progress.".into());
        }
        self.next_request = self.next_request.wrapping_add(1);
        let request = self.next_request;
        self.submissions.insert(
            local,
            Submission {
                request,
                text,
                exact_draft,
                sent: false,
            },
        );
        Ok(request)
    }

    pub fn attach(&mut self, token: RequestToken, receipt: SubmissionReceipt) {
        if let Some(known) = self.known.get_mut(&token.chat_id) {
            known.writing = false;
        }
        self.receipts.insert(token, receipt);
    }

    pub fn hosted_job(&self, local: u64) -> Option<jterm_core::agent_workspace::ApiSnapshot> {
        self.known
            .get(&local)?
            .chat
            .api_job
            .as_deref()
            .filter(|job| job.state.is_live())
            .cloned()
    }
    pub fn hosted_finished(&mut self, token: RequestToken) -> Result<(), String> {
        self.receipts.remove(&token);
        self.settlements.remove(&token);
        let Some(id) = self
            .known
            .get(&token.chat_id)
            .map(|known| known.chat.row.id.clone())
        else {
            return Ok(());
        };
        self.send(
            Command::LoadChat { id },
            Pending::Settle {
                local: token.chat_id,
            },
        )
    }

    pub fn complete(
        &mut self,
        token: RequestToken,
        store: &ChatStore,
        redact: bool,
    ) -> Result<(), String> {
        let text = store
            .snapshot_chat_for_persistence(token.chat_id, redact)
            .and_then(|chat| chat.turns().last().map(|turn| turn.text.clone()));
        self.finish(token, text.ok_or(()))
    }

    fn is_pending_write(&self, local: u64) -> bool {
        self.rejected_receipts.iter().any(|(id, _)| *id == local) || self.pending.values().any(|p| matches!(p, Pending::Save { local: id, .. } | Pending::Draft { local: id } | Pending::Settle { local: id } | Pending::Begin { local: id, .. } | Pending::Delete { local: id } | Pending::Reload { local: id, .. } if *id == local))
    }

    pub fn finish(
        &mut self,
        token: RequestToken,
        result: Result<String, ()>,
    ) -> Result<(), String> {
        if !self.receipts.contains_key(&token) {
            return Ok(());
        }
        self.settlements.insert(token, result);
        self.flush_settlement(token)
    }

    fn flush_settlement(&mut self, token: RequestToken) -> Result<(), String> {
        let Some(receipt) = self.receipts.get(&token).cloned() else {
            self.settlements.remove(&token);
            return Ok(());
        };
        let Some(result) = self.settlements.get(&token).cloned() else {
            return Ok(());
        };
        let command = match result {
            Ok(text) => Command::CompleteSubmission { receipt, text },
            Err(()) => Command::CancelSubmission {
                receipt,
                restore_draft: true,
            },
        };
        self.send(
            command,
            Pending::Settle {
                local: token.chat_id,
            },
        )?;
        self.receipts.remove(&token);
        self.settlements.remove(&token);
        if let Some(known) = self.known.get_mut(&token.chat_id) {
            known.writing = true;
        }
        Ok(())
    }

    pub fn reject_receipt(&mut self, local: u64, receipt: SubmissionReceipt) {
        self.rejected_receipts.push((local, receipt));
        if let Some(known) = self.known.get_mut(&local) {
            known.writing = true;
        }
        self.flush_rejected_receipts();
    }

    fn flush_rejected_receipts(&mut self) {
        while let Some((local, receipt)) = self.rejected_receipts.last().cloned() {
            if self
                .send(
                    Command::CancelSubmission {
                        receipt,
                        restore_draft: false,
                    },
                    Pending::Settle { local },
                )
                .is_err()
            {
                break;
            }
            self.rejected_receipts.pop();
        }
    }

    pub fn local_id(&self, id: &ConversationId) -> Option<u64> {
        self.known
            .iter()
            .find_map(|(local, known)| (known.chat.row.id == *id).then_some(*local))
    }

    pub fn is_shared(&self, local: u64) -> bool {
        self.known.contains_key(&local)
    }

    pub fn read_position(&self, local: u64) -> Option<f64> {
        self.known
            .get(&local)?
            .chat
            .read_position
            .as_ref()
            .filter(|p| p.view == "conversation")
            .map(|p| p.position)
    }

    pub fn save_position(&mut self, local: u64, position: f64) {
        if let Some(known) = self.known.get(&local) {
            let command = Command::SetReadPosition {
                id: known.chat.row.id.clone(),
                view: "conversation".into(),
                position: position.clamp(0.0, 1.0),
            };
            let _ = self.send(command, Pending::Ack);
        }
    }

    pub fn open(&mut self, id: ConversationId) -> Result<Option<u64>, String> {
        if let Some(local) = self.local_id(&id) {
            if self.known.get(&local).is_some_and(|known| known.writing)
                || self.receipts.keys().any(|token| token.chat_id == local)
                || self.submissions.contains_key(&local)
            {
                return Ok(Some(local));
            }
            self.send(
                Command::LoadChat { id },
                Pending::Reload {
                    local,
                    open: true,
                    keep_local: false,
                },
            )?;
            if let Some(known) = self.known.get_mut(&local) {
                known.writing = true;
            }
            return Ok(None);
        }
        self.send(Command::LoadChat { id }, Pending::Open)?;
        Ok(None)
    }

    pub fn create(&mut self) -> Result<u64, String> {
        let request = self.client.send(Command::CreateChat {
            title: "New chat".into(),
        })?;
        self.pending.insert(request, Pending::Create);
        Ok(request)
    }

    pub fn mutation(&mut self, command: Command) -> Result<(), String> {
        let id = match &command {
            Command::Pin { id, .. } | Command::Archive { id, .. } => id.clone(),
            _ => return Err("Unsupported conversation action".into()),
        };
        if let Some(local) = self.local_id(&id) {
            if self.known.get(&local).is_some_and(|known| known.writing)
                || self.receipts.keys().any(|token| token.chat_id == local)
                || self.submissions.contains_key(&local)
            {
                return Err("Wait for this conversation to finish saving or stop its response before changing it.".into());
            }
        }
        let local = self.local_id(&id);
        self.send(command, Pending::Mutation { id })?;
        if let Some(known) = local.and_then(|local| self.known.get_mut(&local)) {
            known.writing = true;
        }
        Ok(())
    }

    pub fn reload(&mut self, local: u64) -> Result<(), String> {
        let known = self
            .known
            .get(&local)
            .ok_or("This conversation is still syncing")?;
        if known.writing
            || self.receipts.keys().any(|token| token.chat_id == local)
            || self.submissions.contains_key(&local)
        {
            return Err("Finish or stop the current response before reloading.".into());
        }
        self.send(
            Command::LoadChat {
                id: known.chat.row.id.clone(),
            },
            Pending::Reload {
                local,
                open: true,
                keep_local: true,
            },
        )?;
        if let Some(known) = self.known.get_mut(&local) {
            known.writing = true;
        }
        Ok(())
    }

    pub fn delete(&mut self, local: u64) -> Result<bool, String> {
        let Some(known) = self.known.get(&local) else {
            return Ok(false);
        };
        if self.receipts.keys().any(|token| token.chat_id == local)
            || self.submissions.contains_key(&local)
        {
            return Err(
                "Finish or stop the current response before deleting this conversation.".into(),
            );
        }
        if known.writing {
            self.pending_deletes.insert(local);
            return Ok(true);
        }
        self.send(
            Command::Delete {
                id: known.chat.row.id.clone(),
            },
            Pending::Delete { local },
        )?;
        if let Some(known) = self.known.get_mut(&local) {
            known.writing = true;
        }
        Ok(true)
    }

    pub fn history(&mut self, local: u64) -> Result<(), String> {
        let known = self
            .known
            .get(&local)
            .ok_or("This conversation is still syncing")?;
        if self.pending.values().any(|p| matches!(p, Pending::History { local: id } | Pending::PrimeHistory { local: id } if *id == local)) { return Ok(()); }
        if let Some(before) = self.history_cursor.get(&local) {
            let Some(before) = before else {
                return Err("You have reached the beginning of this conversation.".into());
            };
            self.send(
                Command::History {
                    id: known.chat.row.id.clone(),
                    before: Some(*before),
                    limit: 100,
                },
                Pending::History { local },
            )
        } else {
            self.send(
                Command::History {
                    id: known.chat.row.id.clone(),
                    before: None,
                    limit: 100,
                },
                Pending::PrimeHistory { local },
            )
        }
    }

    fn snapshot(store: &ChatStore, id: u64, redact: bool) -> Option<String> {
        let chat = store.snapshot_chat_for_persistence(id, redact)?;
        ConversationSnapshot::from_chats(id, vec![chat])?
            .to_json()
            .ok()
    }

    fn retain_cache_room(&mut self, store: &mut ChatStore, incoming: u64, redact: bool) {
        if !store.at_capacity() || store.summaries().iter().any(|chat| chat.id == incoming) {
            return;
        }
        let candidate = store.summaries().iter().find_map(|row| {
            self.known
                .get(&row.id)
                .filter(|known| {
                    !known.writing
                        && !known.blocked
                        && !row.busy
                        && !row.active
                        && !self.submissions.contains_key(&row.id)
                        && !self.receipts.keys().any(|token| token.chat_id == row.id)
                        && Self::snapshot(store, row.id, redact).as_deref()
                            == Some(known.saved.as_str())
                })
                .map(|_| row.id)
        });
        if let Some(id) = candidate {
            if store.evict_cached_chat(id).unwrap_or(false) {
                self.known.remove(&id);
            }
        }
    }

    fn accept_chat(
        &mut self,
        store: &mut ChatStore,
        chat: StoredChat,
        old: Option<u64>,
        open: bool,
        redact: bool,
    ) -> Result<Option<u64>, String> {
        let remote = ConversationSnapshot::from_json(&chat.snapshot_json)
            .map_err(|e| format!("Invalid shared conversation: {e}"))?;
        let incoming = remote
            .active_chat()
            .ok_or("Shared conversation has no active chat")?;
        let local = incoming.id();
        let active = store.active_id();
        // During migration the user may keep typing. Move that exact local
        // draft/history to the new global identity before any remote save.
        let restored_draft = old
            .filter(|old| *old == local)
            .and_then(|id| store.chat_draft(id).map(str::to_string));
        let preserved = old
            .filter(|old| *old != local)
            .and_then(|old| store.snapshot_chat_for_persistence(old, redact));
        let snapshot = preserved
            .as_ref()
            .map(|legacy| {
                ChatSnapshot::from_completed_history(
                    local,
                    legacy.title(),
                    legacy.archived(),
                    legacy.turns(),
                    legacy.block_context(),
                    legacy.draft(),
                )
                .with_history_truncated(legacy.history_truncated())
            })
            .unwrap_or_else(|| incoming.clone());
        if let Some(old) = old.filter(|old| *old != local) {
            store
                .replace_imported_legacy_chat(old, snapshot)
                .map_err(|e| format!("Cannot migrate this conversation while it is busy: {e:?}"))?;
            if let Some(submission) = self.submissions.remove(&old) {
                self.submissions.insert(local, submission);
            }
        } else {
            self.retain_cache_room(store, local, redact);
            store.upsert_persisted_chat(snapshot).map_err(|e| format!("Cannot load this conversation while it is busy or the live cache is full: {e:?}"))?;
        }
        if open || old == Some(active) {
            store.select_chat(local);
        } else {
            store.select_chat(active);
        }
        let saved = ConversationSnapshot::from_chats(local, vec![incoming.clone()])
            .and_then(|s| s.to_json().ok())
            .ok_or("Invalid shared chat")?;
        let restored_conflict = restored_draft
            .as_ref()
            .is_some_and(|draft| !draft.is_empty() && draft != incoming.draft());
        if restored_conflict {
            if let Some(draft) = restored_draft {
                store.set_chat_draft(local, draft);
            }
        }
        self.known.insert(
            local,
            Known {
                chat,
                saved,
                writing: false,
                blocked: restored_conflict,
            },
        );
        Ok((open || old == Some(active)).then_some(local))
    }

    pub fn poll(&mut self, store: &mut ChatStore, redact: bool) -> Vec<Event> {
        let mut events = Vec::new();
        for event in self.client.poll() {
            let WorkspaceEvent::Reply { request_id, result } = event;
            let Some(pending) = self.pending.remove(&request_id) else {
                continue;
            };
            let reply = match result {
                Ok(reply) => reply,
                Err(message) => {
                    self.retry_after = Some(Instant::now() + Duration::from_secs(5));
                    match pending {
                        Pending::Import(ids) => {
                            for id in ids {
                                self.importing.remove(&id);
                            }
                        }
                        Pending::Bind(id) | Pending::Fresh(id) => {
                            self.importing.remove(&id);
                        }
                        Pending::Save { local, .. }
                        | Pending::Draft { local }
                        | Pending::Settle { local }
                        | Pending::Reload { local, .. }
                        | Pending::Delete { local } => {
                            self.pending_deletes.remove(&local);
                            if let Some(known) = self.known.get_mut(&local) {
                                known.writing = false;
                                known.blocked = true;
                            }
                            if let Some(submission) = self.submissions.remove(&local) {
                                events.push(Event::Rejected {
                                    request: submission.request,
                                    message: message.clone(),
                                });
                            }
                        }
                        Pending::Mutation { id } => {
                            if let Some(local) = self.local_id(&id) {
                                if let Some(known) = self.known.get_mut(&local) {
                                    known.writing = false;
                                }
                            }
                        }
                        Pending::Begin { local, request } => {
                            if let Some(known) = self.known.get_mut(&local) {
                                known.writing = false;
                                known.blocked = true;
                            }
                            self.submissions.remove(&local);
                            events.push(Event::Rejected {
                                request,
                                message: message.clone(),
                            });
                        }
                        _ => {}
                    }
                    events.push(Event::Error(message));
                    continue;
                }
            };
            match (pending, reply) {
                (Pending::Ack, Reply::Ack) => {}
                (Pending::Catalog, Reply::Catalog { rows }) => {
                    self.rows = rows
                        .into_iter()
                        .filter(|row| row.kind == ConversationKind::Api)
                        .collect();
                }
                (Pending::Import(_), Reply::Imported { mappings }) => {
                    for mapping in mappings {
                        if let Err(error) = self.send(
                            Command::LoadChat { id: mapping.id },
                            Pending::Bind(mapping.legacy_id),
                        ) {
                            events.push(Event::Error(error));
                        }
                    }
                }
                (Pending::Bind(old) | Pending::Fresh(old), Reply::Chat { chat }) => {
                    self.importing.remove(&old);
                    match self.accept_chat(store, chat, Some(old), false, redact) {
                        Ok(Some(local)) => events.push(Event::Opened(local)),
                        Ok(None) => {}
                        Err(error) => events.push(Event::Error(error)),
                    }
                }
                (Pending::Create, Reply::Chat { chat }) => {
                    self.refreshed = None;
                    match self.accept_chat(store, chat, None, true, redact) {
                        Ok(Some(chat_id)) => events.push(Event::Created {
                            request: request_id,
                            chat_id,
                        }),
                        Ok(None) => {}
                        Err(error) => events.push(Event::Error(error)),
                    }
                }
                (Pending::Open, Reply::Chat { chat }) => {
                    match self.accept_chat(store, chat, None, true, redact) {
                        Ok(Some(local)) => events.push(Event::Opened(local)),
                        Ok(None) => {}
                        Err(error) => events.push(Event::Error(error)),
                    }
                }
                (Pending::Save { local, snapshot }, Reply::Chat { chat }) => {
                    let writing = self.is_pending_write(local);
                    if let Some(known) = self.known.get_mut(&local) {
                        known.chat = chat;
                        known.saved = snapshot;
                        known.writing = writing;
                    }
                }
                (Pending::Draft { local }, Reply::Chat { chat }) => {
                    let writing = self.is_pending_write(local);
                    if let Some(known) = self.known.get_mut(&local) {
                        known.chat = chat;
                        known.writing = writing;
                    }
                }
                (Pending::Settle { local }, Reply::Chat { chat }) => {
                    let local_draft = store.chat_draft(local).unwrap_or_default().to_string();
                    let remote_draft = snapshot_draft(&chat.snapshot_json);
                    let baseline = self
                        .known
                        .get(&local)
                        .map(|known| snapshot_draft(&known.chat.snapshot_json))
                        .unwrap_or_default();
                    let writing = self.is_pending_write(local);
                    let previous_revision = self
                        .known
                        .get(&local)
                        .map(|known| known.chat.draft_revision);
                    let own_clear = chat.api_job.as_ref().is_some_and(|job| {
                        job.state == jterm_core::agent_workspace::ApiState::Completed
                            && remote_draft.is_empty()
                            && chat.draft_revision == job.submitted_draft_revision.saturating_add(1)
                            && previous_revision
                                .is_some_and(|rev| rev <= job.submitted_draft_revision)
                    });
                    let mut resolution = reconcile_draft(&local_draft, &remote_draft, &baseline);
                    if local_draft != remote_draft
                        && resolution == DraftResolution::Local
                        && previous_revision != Some(chat.draft_revision)
                        && !own_clear
                    {
                        resolution = DraftResolution::Conflict;
                    }
                    if own_clear && !local_draft.is_empty() {
                        resolution = DraftResolution::Local;
                    }
                    if resolution == DraftResolution::Remote {
                        store.set_chat_draft(local, remote_draft);
                    }
                    if let Some(known) = self.known.get_mut(&local) {
                        known.chat = chat;
                        known.writing = writing;
                        known.blocked = resolution == DraftResolution::Conflict;
                    }
                    if resolution == DraftResolution::Conflict {
                        events.push(Event::Error("Another window edited this draft during the response. Your local draft is kept; reload shared history explicitly to reconcile before sending.".into()));
                    }
                    self.refreshed = None;
                }
                (
                    Pending::Reload {
                        local,
                        open,
                        keep_local,
                    },
                    Reply::Chat { chat },
                ) => {
                    let draft = store.chat_draft(local).unwrap_or_default().to_string();
                    let remote_draft = snapshot_draft(&chat.snapshot_json);
                    let baseline = self
                        .known
                        .get(&local)
                        .map(|known| snapshot_draft(&known.chat.snapshot_json))
                        .unwrap_or_default();
                    let resolution = if keep_local {
                        DraftResolution::Local
                    } else {
                        reconcile_draft(&draft, &remote_draft, &baseline)
                    };
                    let active = store.active_id();
                    match self.accept_chat(store, chat, None, open, redact) {
                        Ok(_) => {
                            if resolution != DraftResolution::Remote {
                                store.set_chat_draft(local, draft);
                            }
                            if resolution == DraftResolution::Conflict {
                                if let Some(known) = self.known.get_mut(&local) {
                                    known.blocked = true;
                                }
                                events.push(Event::Error("Another window edited this draft. Your local draft is kept; use Reload shared history to explicitly keep it with the newer history.".into()));
                            }
                            if !open {
                                store.select_chat(active);
                            }
                            events.push(Event::Opened(store.active_id()));
                        }
                        Err(error) => events.push(Event::Error(error)),
                    }
                }

                (Pending::Delete { local }, Reply::Ack) => {
                    if let Some(known) = self.known.remove(&local) {
                        self.rows.retain(|row| row.id != known.chat.row.id);
                    }
                    self.history_cursor.remove(&local);
                    self.importing.insert(local);
                    events.push(Event::Deleted(local));
                }
                (Pending::Mutation { id }, Reply::Ack) => {
                    self.refreshed = None;
                    if let Some(local) = self.local_id(&id) {
                        if let Err(error) = self.send(
                            Command::LoadChat { id },
                            Pending::Reload {
                                local,
                                open: false,
                                keep_local: false,
                            },
                        ) {
                            if let Some(known) = self.known.get_mut(&local) {
                                known.writing = false;
                                known.blocked = true;
                            }
                            events.push(Event::Error(error));
                        }
                    }
                }
                (Pending::PrimeHistory { local }, Reply::Messages { next_before, .. }) => {
                    self.history_cursor.insert(local, next_before);
                    if let Err(error) = self.history(local) {
                        events.push(Event::Error(error));
                    }
                }
                (
                    Pending::History { local },
                    Reply::Messages {
                        messages,
                        next_before,
                    },
                ) => {
                    self.history_cursor.insert(local, next_before);
                    let text = messages
                        .into_iter()
                        .map(|m| {
                            format!(
                                "{}\n{}\n\n",
                                if m.role == "user" { "You" } else { "Assistant" },
                                m.text
                            )
                        })
                        .collect::<String>();
                    if text.is_empty() {
                        events.push(Event::Error(
                            "You have reached the beginning of this conversation.".into(),
                        ));
                    } else {
                        events.push(Event::Earlier {
                            chat_id: local,
                            text,
                        });
                    }
                }
                (Pending::Begin { local, request }, Reply::Submission { receipt }) => {
                    self.submissions.remove(&local);
                    if let Some(known) = self.known.get_mut(&local) {
                        known.chat.revision = known.chat.revision.saturating_add(1);
                        known.chat.epoch = receipt.epoch;
                        known.writing = true;
                    }
                    events.push(Event::Authorized {
                        request,
                        chat_id: local,
                        receipt,
                    });
                }
                _ => events.push(Event::Error(
                    "Unexpected workspace acknowledgement; the draft was kept.".into(),
                )),
            }
        }
        self.flush_rejected_receipts();
        let settling: Vec<_> = self.settlements.keys().copied().collect();
        for token in settling {
            let _ = self.flush_settlement(token);
        }
        if self.retry_after.is_some_and(|at| Instant::now() < at) {
            return events;
        }
        if self
            .refreshed
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
        {
            self.refreshed = Some(Instant::now());
            if let Err(error) = self.send(Command::Refresh, Pending::Catalog) {
                events.push(Event::Error(error));
            }
        }
        // Import only identities not already bound. Changed existing chats
        // use compare-and-swap commands, never repeat whole-file replacement.
        let unknown: Vec<_> = store
            .summaries()
            .into_iter()
            .filter(|row| {
                !self.known.contains_key(&row.id) && !self.importing.contains(&row.id) && !row.busy
            })
            .filter_map(|row| store.snapshot_chat_for_persistence(row.id, redact))
            .filter(|chat| {
                !chat.turns().is_empty()
                    || !chat.draft().is_empty()
                    || chat.title() != "New chat"
                    || self.submissions.contains_key(&chat.id())
            })
            .collect();
        let (fresh, unknown): (Vec<_>, Vec<_>) = unknown.into_iter().partition(|chat| {
            chat.id() < jterm_core::agent_workspace::SHARED_CHAT_ID_BASE
                && !self.legacy_sources.contains(&chat.id())
        });
        for chat in fresh {
            let local = chat.id();
            match self.send(
                Command::CreateChat {
                    title: chat.title().to_string(),
                },
                Pending::Fresh(local),
            ) {
                Ok(()) => {
                    self.importing.insert(local);
                }
                Err(error) => events.push(Event::Error(error)),
            }
        }
        if !unknown.is_empty() {
            let ids = unknown.iter().map(ChatSnapshot::id).collect::<Vec<_>>();
            if let Some(snapshot) = ConversationSnapshot::from_chats(unknown[0].id(), unknown) {
                if let Ok(snapshot_json) = snapshot.to_json() {
                    match self.send(
                        Command::ImportChats {
                            namespace: self.namespace.clone(),
                            snapshot_json,
                        },
                        Pending::Import(ids.clone()),
                    ) {
                        Ok(()) => self.importing.extend(ids),
                        Err(error) => events.push(Event::Error(error)),
                    }
                }
            }
        }
        let deletes: Vec<_> = self
            .pending_deletes
            .iter()
            .copied()
            .filter(|id| {
                self.known
                    .get(id)
                    .is_some_and(|known| !known.writing && !known.blocked)
            })
            .collect();
        for local in deletes {
            if let Some(known) = self.known.get(&local) {
                let command = Command::Delete {
                    id: known.chat.row.id.clone(),
                };
                if self.send(command, Pending::Delete { local }).is_ok() {
                    self.pending_deletes.remove(&local);
                    if let Some(known) = self.known.get_mut(&local) {
                        known.writing = true;
                    }
                }
            }
        }
        let summaries = store.summaries();
        for row in summaries {
            let Some(snapshot) = Self::snapshot(store, row.id, redact) else {
                continue;
            };
            let Some(known) = self.known.get(&row.id) else {
                continue;
            };
            if known.blocked {
                if let Some(submission) = self.submissions.remove(&row.id) {
                    events.push(Event::Rejected { request: submission.request, message: "Shared history or draft changed in another window. Your draft is preserved; reload shared history explicitly before sending.".into() });
                }
                continue;
            }
            if known.writing {
                continue;
            }
            if row.busy || self.receipts.keys().any(|token| token.chat_id == row.id) {
                let draft = store.chat_draft(row.id).unwrap_or_default();
                let draft = if redact {
                    jterm_core::redact::redact_secrets(draft)
                } else {
                    draft.to_string()
                };
                let previous = ConversationSnapshot::from_json(&known.chat.snapshot_json)
                    .ok()
                    .map(|s| {
                        s.active_chat()
                            .map(|c| c.draft().to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                if draft != previous {
                    let command = Command::SetDraft {
                        id: known.chat.row.id.clone(),
                        expected_revision: known.chat.draft_revision,
                        text: draft,
                    };
                    if self.send(command, Pending::Draft { local: row.id }).is_ok() {
                        if let Some(known) = self.known.get_mut(&row.id) {
                            known.writing = true;
                        }
                    }
                }
                continue;
            }
            if snapshot != known.saved {
                let command = Command::SaveChat {
                    id: known.chat.row.id.clone(),
                    expected_revision: known.chat.revision,
                    snapshot_json: snapshot.clone(),
                };
                match self.send(
                    command,
                    Pending::Save {
                        local: row.id,
                        snapshot,
                    },
                ) {
                    Ok(()) => {
                        if let Some(known) = self.known.get_mut(&row.id) {
                            known.writing = true;
                        }
                    }
                    Err(error) => events.push(Event::Error(error)),
                }
                continue;
            }
            if let Some(submission) = self.submissions.get(&row.id).filter(|s| !s.sent) {
                let draft = store
                    .snapshot_chat_for_persistence(row.id, redact)
                    .map(|s| s.draft().to_string())
                    .unwrap_or_default();
                if submission.exact_draft && draft.trim() != submission.text.trim() {
                    let request = submission.request;
                    self.submissions.remove(&row.id);
                    events.push(Event::Rejected { request, message: "The draft changed before it was accepted. Your current draft is preserved; send it when ready.".into() });
                    continue;
                }
                if !known.chat.background_api_supported {
                    let request = submission.request;
                    self.submissions.remove(&row.id);
                    events.push(Event::Rejected { request, message: "This session host needs an update before API chats can run in the background. Finish its running tasks and restart it, then reopen the conversation. Your draft is kept.".into() });
                    continue;
                }
                let request = submission.request;
                let command = Command::BeginSubmission {
                    id: known.chat.row.id.clone(),
                    expected_revision: known.chat.revision,
                    draft_revision: known.chat.draft_revision,
                    text: submission.text.clone(),
                };
                match self.send(
                    command,
                    Pending::Begin {
                        local: row.id,
                        request,
                    },
                ) {
                    Ok(()) => {
                        if let Some(submission) = self.submissions.get_mut(&row.id) {
                            submission.sent = true;
                        }
                        if let Some(known) = self.known.get_mut(&row.id) {
                            known.writing = true;
                        }
                    }
                    Err(message) => {
                        self.submissions.remove(&row.id);
                        events.push(Event::Rejected { request, message });
                    }
                }
            }
        }
        events
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DraftResolution {
    Local,
    Remote,
    Conflict,
}
fn snapshot_draft(json: &str) -> String {
    ConversationSnapshot::from_json(json)
        .ok()
        .and_then(|s| s.active_chat().map(|c| c.draft().to_string()))
        .unwrap_or_default()
}
fn reconcile_draft(local: &str, remote: &str, baseline: &str) -> DraftResolution {
    if local == remote || remote == baseline {
        DraftResolution::Local
    } else if local == baseline || local.is_empty() {
        DraftResolution::Remote
    } else {
        DraftResolution::Conflict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn settlement_draft_reconciliation_does_not_bless_a_peer_overwrite() {
        assert_eq!(
            reconcile_draft("", "peer edit", "submitted"),
            DraftResolution::Remote
        );
        assert_eq!(
            reconcile_draft("local edit", "peer edit", "submitted"),
            DraftResolution::Conflict
        );
        assert_eq!(
            reconcile_draft("local edit", "", ""),
            DraftResolution::Local
        );
        assert_eq!(
            reconcile_draft("unchanged", "peer edit", "unchanged"),
            DraftResolution::Remote
        );
        assert_eq!(
            reconcile_draft("same", "same", "older"),
            DraftResolution::Local
        );
    }

    struct Host {
        child: std::process::Child,
        root: std::path::PathBuf,
    }
    impl Drop for Host {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn host() -> Host {
        let executable = std::env::var_os("JTERM_TEST_HOST")
            .expect("Set JTERM_TEST_HOST to the built frontend executable");
        let root = std::env::temp_dir().join(format!(
            "gtk-chat-host-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = std::process::Command::new(executable)
            .arg("--jterm-session-host")
            .env("JTERM_WORKSPACE_STATE_DIR", &root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("run/host.sock").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            root.join("run/host.sock").exists(),
            "embedded host did not start"
        );
        Host { child, root }
    }
    fn bridge(host: &Host, namespace: &str) -> SharedChats {
        let mut bridge = SharedChats::new(namespace.into());
        bridge.client = WorkspaceClient::for_state_dir(&host.root);
        bridge
    }
    fn until<T>(
        bridge: &mut SharedChats,
        store: &mut ChatStore,
        mut done: impl FnMut(&mut SharedChats, &mut ChatStore, Vec<Event>) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let events = bridge.poll(store, false);
            if let Some(value) = done(bridge, store, events) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "workspace acknowledgement timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "requires JTERM_TEST_HOST and local Unix sockets; no network/provider access"]
    fn shared_receipts_reject_second_writer_preserve_new_draft_and_delete_after_cancel() {
        let host = host();
        let mut first = bridge(&host, "first-window");
        let mut store = ChatStore::default();
        let create = first.create().unwrap();
        let local = until(&mut first, &mut store, |_, _, events| {
            events.into_iter().find_map(|event| match event {
                Event::Created { request, chat_id } if request == create => Some(chat_id),
                Event::Error(e) => panic!("{e}"),
                _ => None,
            })
        });
        assert!(local >= jterm_core::agent_workspace::SHARED_CHAT_ID_BASE);
        let id = first.known[&local].chat.row.id.clone();
        store.set_active_draft("first request".into());
        let request = first.prepare(local, "first request".into(), true).unwrap();
        let receipt = until(&mut first, &mut store, |_, _, events| {
            events.into_iter().find_map(|event| match event {
                Event::Authorized {
                    request: accepted,
                    receipt,
                    ..
                } if accepted == request => Some(receipt),
                Event::Error(e) => panic!("{e}"),
                _ => None,
            })
        });
        let token = store
            .begin_turn("first request".into(), None, "Thinking".into(), true)
            .unwrap()
            .token;
        store.set_active_draft(String::new());
        first.attach(token, receipt);

        let mut second = bridge(&host, "second-window");
        let mut other = ChatStore::default();
        second.open(id.clone()).unwrap();
        until(&mut second, &mut other, |_, _, events| {
            events.into_iter().find_map(|event| match event {
                Event::Opened(chat) if chat == local => Some(()),
                Event::Error(e) => panic!("{e}"),
                _ => None,
            })
        });
        let competing = second
            .prepare(local, "first request".into(), false)
            .unwrap();
        until(&mut second, &mut other, |_, _, events| {
            events.into_iter().find_map(|event| match event {
                Event::Rejected { request, .. } if request == competing => Some(()),
                Event::Authorized { .. } => panic!("second app acquired an active receipt"),
                _ => None,
            })
        });
        assert!(
            !other.active_draft().is_empty(),
            "rejected send discarded the recoverable draft"
        );

        // Deliberately overlap a draft CAS with final completion. Both replies
        // must settle before another whole-chat save is allowed.
        store.set_active_draft("edited while first reply was running".into());
        first.poll(&mut store, false);
        assert_eq!(
            store.complete_success(token, "completed answer".into()),
            Some(true)
        );
        first.complete(token, &store, false).unwrap();
        until(&mut first, &mut store, |bridge, store, events| {
            for event in events {
                if let Event::Error(error) = event {
                    panic!("{error}");
                }
            }
            (bridge.pending.is_empty()
                && !bridge.known[&local].writing
                && SharedChats::snapshot(store, local, false).as_deref()
                    == Some(bridge.known[&local].saved.as_str()))
            .then_some(())
        });
        first.save_position(local, 0.35);
        until(&mut first, &mut store, |bridge, _, _| {
            bridge.pending.is_empty().then_some(())
        });
        let mut third = bridge(&host, "third-window");
        let mut restored = ChatStore::default();
        third.open(id).unwrap();
        until(&mut third, &mut restored, |_, _, events| {
            events
                .into_iter()
                .find_map(|event| matches!(event, Event::Opened(_)).then_some(()))
        });
        assert_eq!(
            restored.active_draft(),
            "edited while first reply was running"
        );
        let snapshot = restored
            .snapshot_chat_for_persistence(local, false)
            .unwrap();
        assert_eq!(snapshot.turns().len(), 2);
        assert_eq!(snapshot.turns()[1].text, "completed answer");
        assert_eq!(third.read_position(local), Some(0.35));

        store.set_active_draft("cancel then delete".into());
        let request = first
            .prepare(local, "cancel then delete".into(), true)
            .unwrap();
        let receipt = until(&mut first, &mut store, |_, _, events| {
            events.into_iter().find_map(|event| match event {
                Event::Authorized {
                    request: accepted,
                    receipt,
                    ..
                } if accepted == request => Some(receipt),
                Event::Error(e) => panic!("{e}"),
                _ => None,
            })
        });
        let token = store
            .begin_turn("cancel then delete".into(), None, "Thinking".into(), true)
            .unwrap()
            .token;
        first.attach(token, receipt);
        first.finish(token, Err(())).unwrap();
        store.cancel_request(token, "Stopped".into());
        assert!(first.delete(local).unwrap());
        until(&mut first, &mut store, |_, _, events| {
            events.into_iter().find_map(|event| match event {
                Event::Deleted(id) if id == local => Some(()),
                Event::Error(e) => panic!("{e}"),
                _ => None,
            })
        });
        assert!(!first.known.contains_key(&local));
    }
    #[test]
    #[ignore = "requires JTERM_TEST_HOST and local Unix sockets; no network/provider access"]
    fn settlement_keeps_peer_draft_and_blocks_divergent_local_edits() {
        let host = host();
        for local_draft in ["", "different local edit"] {
            let mut first = bridge(&host, "settlement-window");
            let mut store = ChatStore::default();
            let create = first.create().unwrap();
            let local = until(&mut first, &mut store, |_, _, events| {
                events.into_iter().find_map(|event| match event {
                    Event::Created { request, chat_id } if request == create => Some(chat_id),
                    Event::Error(e) => panic!("{e}"),
                    _ => None,
                })
            });
            let id = first.known[&local].chat.row.id.clone();
            store.set_active_draft("submitted".into());
            let request = first.prepare(local, "submitted".into(), true).unwrap();
            let receipt = until(&mut first, &mut store, |_, _, events| {
                events.into_iter().find_map(|event| match event {
                    Event::Authorized {
                        request: accepted,
                        receipt,
                        ..
                    } if accepted == request => Some(receipt),
                    Event::Error(e) => panic!("{e}"),
                    _ => None,
                })
            });
            let draft_revision = receipt.draft_revision;
            let token = store
                .begin_turn("submitted".into(), None, "Thinking".into(), true)
                .unwrap()
                .token;
            first.attach(token, receipt);
            store.set_active_draft(local_draft.into());

            // A different app changes only the durable draft while this app's
            // exact submission receipt owns the provider turn.
            let peer = WorkspaceClient::for_state_dir(&host.root);
            let write = peer
                .send(Command::SetDraft {
                    id: id.clone(),
                    expected_revision: draft_revision,
                    text: "peer draft".into(),
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if peer.poll().into_iter().any(|event| match event {
                    WorkspaceEvent::Reply { request_id, result } if request_id == write => {
                        assert!(
                            matches!(result, Ok(Reply::Chat { .. })),
                            "peer draft failed: {result:?}"
                        );
                        true
                    }
                    _ => false,
                }) {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(store.complete_success(token, "answer".into()), Some(true));
            first.complete(token, &store, false).unwrap();
            until(&mut first, &mut store, |bridge, _, _| {
                (bridge.pending.is_empty() && !bridge.known[&local].writing).then_some(())
            });
            assert_eq!(
                store.active_draft(),
                if local_draft.is_empty() {
                    "peer draft"
                } else {
                    local_draft
                }
            );
            assert_eq!(first.known[&local].blocked, !local_draft.is_empty());

            let mut observer = bridge(&host, "settlement-observer");
            let mut restored = ChatStore::default();
            observer.open(id).unwrap();
            until(&mut observer, &mut restored, |_, _, events| {
                events
                    .into_iter()
                    .find_map(|event| matches!(event, Event::Opened(_)).then_some(()))
            });
            assert_eq!(
                restored.active_draft(),
                "peer draft",
                "completion overwrote a peer's newer draft"
            );
            let saved = restored
                .snapshot_chat_for_persistence(local, false)
                .unwrap();
            assert_eq!(saved.turns().len(), 2);
            assert_eq!(saved.turns()[1].text, "answer");
        }
    }
}
