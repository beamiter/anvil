//! Toolkit-independent, read-only workspace search and evidence navigation.
//! Results carry source identity and revision; viewing them never launches,
//! resumes or writes to a provider, and never changes a model's context.
use jterm_core::agent_workspace::{
    ConversationId, WorkspaceClient, WorkspaceCommand, WorkspaceEvent, WorkspaceQuery,
    WorkspaceReply, WorkspaceSearchHit, WorkspaceSearchPage, WorkspaceSearchRequest,
    WorkspaceTextDocument,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);
const PAGE_SIZE: usize = 30;

enum Pending {
    Search {
        epoch: u64,
        append: bool,
    },
    Document {
        epoch: u64,
        conversation: ConversationId,
        event_id: String,
        revision: String,
    },
}

pub(crate) struct WorkspaceTools {
    client: WorkspaceClient,
    pending: HashMap<u64, Pending>,
    query: String,
    conversation: Option<ConversationId>,
    include_archived: bool,
    search_epoch: u64,
    document_epoch: u64,
    changed: Instant,
    search_due: bool,
    pub(crate) page: WorkspaceSearchPage,
    pub(crate) search_loading: bool,
    pub(crate) document_loading: bool,
    pub(crate) document: Option<WorkspaceTextDocument>,
    pub(crate) selected_hit: Option<WorkspaceSearchHit>,
    pub(crate) notice: Option<String>,
}

impl Default for WorkspaceTools {
    fn default() -> Self {
        Self {
            client: WorkspaceClient::new(),
            pending: HashMap::new(),
            query: String::new(),
            conversation: None,
            include_archived: false,
            search_epoch: 0,
            document_epoch: 0,
            changed: Instant::now(),
            search_due: false,
            page: WorkspaceSearchPage::default(),
            search_loading: false,
            document_loading: false,
            document: None,
            selected_hit: None,
            notice: None,
        }
    }
}

impl WorkspaceTools {
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Debounce only the evidence index. Metadata filtering remains immediate.
    /// A changed scope retires every older response without losing the open
    /// evidence document or mutating the active conversation.
    pub(crate) fn set_search(
        &mut self,
        query: &str,
        conversation: Option<ConversationId>,
        include_archived: bool,
    ) {
        let query = query.trim();
        if self.query == query
            && self.conversation == conversation
            && self.include_archived == include_archived
        {
            return;
        }
        self.query = query.to_owned();
        self.conversation = conversation;
        self.include_archived = include_archived;
        self.search_epoch = self.search_epoch.wrapping_add(1);
        self.changed = Instant::now();
        self.page = WorkspaceSearchPage::default();
        self.search_loading = false;
        self.notice = WorkspaceQuery::parse(query).err();
        self.search_due = !query.is_empty() && self.notice.is_none();
    }

    pub(crate) fn searching(&self) -> bool {
        self.search_due || self.search_loading
    }

    fn send_search(&mut self, append: bool) {
        if self.query.is_empty()
            || self.search_loading
            || self
                .pending
                .values()
                .any(|pending| matches!(pending, Pending::Search { .. }))
        {
            // A slow old epoch still occupies the worker. Keep search_due so
            // its retirement sends only the latest query, not a typing queue.
            return;
        }
        let before = if append { self.page.next_before } else { None };
        if append && before.is_none() {
            return;
        }
        let request = WorkspaceSearchRequest {
            query: self.query.clone(),
            conversation: self.conversation.clone(),
            before,
            limit: PAGE_SIZE,
            include_archived: self.include_archived,
        };
        self.search_due = false;
        match self.client.send(WorkspaceCommand::Search { request }) {
            Ok(id) => {
                self.pending.insert(
                    id,
                    Pending::Search {
                        epoch: self.search_epoch,
                        append,
                    },
                );
                self.search_loading = true;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    pub(crate) fn load_more(&mut self) {
        self.send_search(true);
    }

    /// Accept only a hit from the current page. Retained callbacks from an
    /// older search cannot open a replacement or silently resume a session.
    pub(crate) fn open_hit(&mut self, hit: WorkspaceSearchHit) {
        if !self
            .page
            .hits
            .iter()
            .any(|current| current.row.id == hit.row.id && current.anchor == hit.anchor)
        {
            self.notice =
                Some("This search result changed. Search again before opening it.".into());
            return;
        }
        self.document_epoch = self.document_epoch.wrapping_add(1);
        self.document = None;
        self.notice = None;
        self.selected_hit = Some(hit.clone());
        self.document_loading = false;
        let command = WorkspaceCommand::ReadSearchDocument {
            conversation: hit.row.id.clone(),
            event_id: hit.anchor.event_id.clone(),
            revision: hit.anchor.revision.clone(),
        };
        match self.client.send(command) {
            Ok(id) => {
                self.pending.insert(
                    id,
                    Pending::Document {
                        epoch: self.document_epoch,
                        conversation: hit.row.id,
                        event_id: hit.anchor.event_id,
                        revision: hit.anchor.revision,
                    },
                );
                self.document_loading = true;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    pub(crate) fn close_document(&mut self) {
        self.document_epoch = self.document_epoch.wrapping_add(1);
        self.document_loading = false;
        self.selected_hit = None;
        self.document = None;
    }

    fn receive(&mut self, id: u64, result: Result<WorkspaceReply, String>) -> bool {
        let Some(pending) = self.pending.remove(&id) else {
            return false;
        };
        match pending {
            Pending::Search { epoch, append } => {
                if epoch != self.search_epoch {
                    return false;
                }
                self.search_loading = false;
                match result {
                    Ok(WorkspaceReply::Search { page }) => {
                        if append {
                            for hit in page.hits {
                                if !self
                                    .page
                                    .hits
                                    .iter()
                                    .any(|old| old.row.id == hit.row.id && old.anchor == hit.anchor)
                                {
                                    self.page.hits.push(hit);
                                }
                            }
                            self.page.next_before = page.next_before;
                            self.page.coverage = page.coverage;
                        } else {
                            self.page = page;
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(format!("Search: {error}")),
                    Ok(_) => {
                        self.notice =
                            Some("The session host did not return search evidence.".into())
                    }
                }
            }
            Pending::Document {
                epoch,
                conversation,
                event_id,
                revision,
            } => {
                if epoch != self.document_epoch {
                    return false;
                }
                self.document_loading = false;
                match result {
                    Ok(WorkspaceReply::SearchDocument { document })
                        if document.conversation == conversation
                            && document.event_id == event_id
                            && document.revision == revision =>
                    {
                        self.document = Some(document);
                        self.notice = None;
                    }
                    Err(error) => {
                        self.notice = Some(format!(
                            "Evidence: {error}. Search again to read the current revision."
                        ))
                    }
                    Ok(_) => {
                        self.notice =
                            Some("The evidence identity changed; no replacement was opened.".into())
                    }
                }
            }
        }
        true
    }

    /// Call from the frontend's regular update/poll, including when a drawer
    /// is hidden. This method performs no blocking I/O on the GUI thread.
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        for WorkspaceEvent::Reply { request_id, result } in self.client.poll() {
            changed |= self.receive(request_id, result);
        }
        if self.search_due && self.changed.elapsed() >= SEARCH_DEBOUNCE {
            self.send_search(false);
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_core::agent_workspace::SearchSource;

    #[test]
    fn a_late_search_cannot_replace_the_new_query() {
        let mut tools = WorkspaceTools::default();
        tools.set_search("older", None, false);
        let old = tools.search_epoch;
        tools.pending.insert(
            1,
            Pending::Search {
                epoch: old,
                append: false,
            },
        );
        tools.set_search("current", None, false);
        assert!(!tools.receive(
            1,
            Ok(WorkspaceReply::Search {
                page: WorkspaceSearchPage::default()
            })
        ));
        assert_eq!(tools.query(), "current");
        assert!(tools.searching());
    }

    #[test]
    fn closing_an_evidence_view_retires_its_read() {
        let mut tools = WorkspaceTools::default();
        let id = ConversationId("api:test".into());
        tools.pending.insert(
            1,
            Pending::Document {
                epoch: 0,
                conversation: id.clone(),
                event_id: "message:7".into(),
                revision: "v1".into(),
            },
        );
        tools.close_document();
        let document = WorkspaceTextDocument {
            conversation: id,
            event_id: "message:7".into(),
            revision: "v1".into(),
            source: SearchSource::ApiMessage,
            message_sequence: Some(7),
            text: "retained evidence".into(),
            truncated: false,
        };
        assert!(!tools.receive(1, Ok(WorkspaceReply::SearchDocument { document })));
        assert!(tools.document.is_none());
    }

    #[test]
    fn a_mismatched_document_never_becomes_the_selected_evidence() {
        let mut tools = WorkspaceTools::default();
        let id = ConversationId("api:test".into());
        tools.pending.insert(
            1,
            Pending::Document {
                epoch: 0,
                conversation: id.clone(),
                event_id: "message:7".into(),
                revision: "v1".into(),
            },
        );
        let document = WorkspaceTextDocument {
            conversation: id,
            event_id: "message:8".into(),
            revision: "v1".into(),
            source: SearchSource::ApiMessage,
            message_sequence: Some(8),
            text: "another message".into(),
            truncated: false,
        };
        assert!(tools.receive(1, Ok(WorkspaceReply::SearchDocument { document })));
        assert!(tools.document.is_none());
        assert!(tools
            .notice
            .as_ref()
            .is_some_and(|notice| notice.contains("identity changed")));
    }
    #[test]
    fn typing_during_a_slow_search_keeps_one_flight_and_the_latest_query() {
        let mut tools = WorkspaceTools::default();
        tools.set_search("old query", None, false);
        tools.pending.insert(
            77,
            Pending::Search {
                epoch: tools.search_epoch,
                append: false,
            },
        );
        tools.set_search("latest query", None, false);
        tools.send_search(false);
        assert_eq!(tools.pending.len(), 1);
        assert!(tools.search_due);
        assert_eq!(tools.query(), "latest query");
        assert!(!tools.receive(
            77,
            Ok(WorkspaceReply::Search {
                page: WorkspaceSearchPage::default()
            })
        ));
        assert!(tools.pending.is_empty());
        assert!(tools.search_due);
    }
}
