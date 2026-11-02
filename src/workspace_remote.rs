//! An explicitly configured SSH workspace viewer. It owns a separate read-only
//! client and never exposes local activation, queue writes or context saves.
use jterm_core::agent_workspace::{
    ConversationId, WorkspaceClient, WorkspaceCommand, WorkspaceEvent, WorkspaceReply,
    WorkspaceRow, WorkspaceSearchHit, WorkspaceSearchPage, WorkspaceSearchRequest,
    WorkspaceSshHost, WorkspaceTextDocument,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

enum Pending {
    Catalog {
        epoch: u64,
        append: bool,
    },
    Search {
        epoch: u64,
        query: u64,
        append: bool,
    },
    Document {
        epoch: u64,
        generation: u64,
        conversation: ConversationId,
        event: String,
        revision: String,
    },
}

pub(crate) struct RemoteWorkspace {
    client: Option<WorkspaceClient>,
    pending: HashMap<u64, Pending>,
    epoch: u64,
    search_epoch: u64,
    document_epoch: u64,
    query: String,
    conversation: Option<ConversationId>,
    search_due: Option<Instant>,
    pub(crate) connected: bool,
    pub(crate) loading: bool,
    pub(crate) search_loading: bool,
    pub(crate) document_loading: bool,
    pub(crate) rows: Vec<WorkspaceRow>,
    pub(crate) next_before: Option<u64>,
    pub(crate) total: usize,
    pub(crate) page: WorkspaceSearchPage,
    pub(crate) selected_hit: Option<WorkspaceSearchHit>,
    pub(crate) document: Option<WorkspaceTextDocument>,
    pub(crate) notice: Option<String>,
}

impl Default for RemoteWorkspace {
    fn default() -> Self {
        Self {
            client: None,
            pending: HashMap::new(),
            epoch: 0,
            search_epoch: 0,
            document_epoch: 0,
            query: String::new(),
            conversation: None,
            search_due: None,
            connected: false,
            loading: false,
            search_loading: false,
            document_loading: false,
            rows: Vec::new(),
            next_before: None,
            total: 0,
            page: WorkspaceSearchPage::default(),
            selected_hit: None,
            document: None,
            notice: None,
        }
    }
}

impl RemoteWorkspace {
    pub(crate) fn generation(&self) -> u64 {
        self.epoch
    }
    pub(crate) fn connect(&mut self, alias: &str, executable: &str, expected_host_id: &str) {
        self.disconnect();
        let config = WorkspaceSshHost::new(
            alias.trim(),
            std::path::Path::new(executable.trim()),
            expected_host_id.trim(),
        );
        match config.and_then(WorkspaceClient::with_ssh_host) {
            Ok(client) => {
                self.client = Some(client);
                self.notice = Some(
                    "Checking the remote host identity and loading its read-only catalog…".into(),
                );
                self.refresh();
            }
            Err(error) => self.notice = Some(error),
        }
    }
    pub(crate) fn disconnect(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.client = None;
        self.pending.clear();
        self.connected = false;
        self.loading = false;
        self.search_loading = false;
        self.document_loading = false;
        self.rows.clear();
        self.next_before = None;
        self.total = 0;
        self.page = WorkspaceSearchPage::default();
        self.query.clear();
        self.conversation = None;
        self.search_due = None;
        self.selected_hit = None;
        self.document = None;
        self.notice = None;
    }
    pub(crate) fn query(&self) -> &str {
        &self.query
    }
    pub(crate) fn conversation(&self) -> Option<&ConversationId> {
        self.conversation.as_ref()
    }
    pub(crate) fn searching(&self) -> bool {
        !self.query.is_empty() || self.conversation.is_some()
    }
    fn send(&mut self, command: WorkspaceCommand, pending: Pending) -> bool {
        let Some(client) = &self.client else {
            return false;
        };
        match client.send(command) {
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
    pub(crate) fn refresh(&mut self) {
        if self.loading {
            return;
        }
        self.loading = self.send(
            WorkspaceCommand::CatalogPage {
                before: None,
                limit: 100,
            },
            Pending::Catalog {
                epoch: self.epoch,
                append: false,
            },
        );
    }
    pub(crate) fn load_more_catalog(&mut self) {
        if self.loading {
            return;
        }
        let Some(before) = self.next_before else {
            return;
        };
        self.loading = self.send(
            WorkspaceCommand::CatalogPage {
                before: Some(before),
                limit: 100,
            },
            Pending::Catalog {
                epoch: self.epoch,
                append: true,
            },
        );
    }
    pub(crate) fn set_search(&mut self, query: &str, conversation: Option<ConversationId>) {
        let query = query.trim();
        if self.query == query && self.conversation == conversation {
            return;
        }
        self.query = query.to_owned();
        self.conversation = conversation;
        self.search_epoch = self.search_epoch.wrapping_add(1);
        self.page = WorkspaceSearchPage::default();
        self.search_loading = false;
        self.notice = None;
        self.clear_document();
        self.search_due = self.searching().then(Instant::now);
    }
    fn search(&mut self, append: bool) {
        if !self.searching()
            || self.search_loading
            || self
                .pending
                .values()
                .any(|pending| matches!(pending, Pending::Search { .. }))
        {
            return;
        }
        let before = if append { self.page.next_before } else { None };
        if append && before.is_none() {
            return;
        }
        self.search_due = None;
        let request = WorkspaceSearchRequest {
            query: self.query.clone(),
            conversation: self.conversation.clone(),
            before,
            limit: 30,
            include_archived: true,
        };
        self.search_loading = self.send(
            WorkspaceCommand::Search { request },
            Pending::Search {
                epoch: self.epoch,
                query: self.search_epoch,
                append,
            },
        );
    }
    pub(crate) fn load_more_search(&mut self) {
        self.search(true);
    }
    pub(crate) fn clear_document(&mut self) {
        self.document_epoch = self.document_epoch.wrapping_add(1);
        self.selected_hit = None;
        self.document = None;
        self.document_loading = false;
    }
    pub(crate) fn open_hit(&mut self, hit: WorkspaceSearchHit) {
        if !self
            .page
            .hits
            .iter()
            .any(|current| current.row.id == hit.row.id && current.anchor == hit.anchor)
        {
            self.notice =
                Some("That remote search result changed. Search again before reading it.".into());
            return;
        }
        self.clear_document();
        self.notice = None;
        self.selected_hit = Some(hit.clone());
        let command = WorkspaceCommand::ReadSearchDocument {
            conversation: hit.row.id.clone(),
            event_id: hit.anchor.event_id.clone(),
            revision: hit.anchor.revision.clone(),
        };
        self.document_loading = self.send(
            command,
            Pending::Document {
                epoch: self.epoch,
                generation: self.document_epoch,
                conversation: hit.row.id,
                event: hit.anchor.event_id,
                revision: hit.anchor.revision,
            },
        );
    }
    fn receive(&mut self, request_id: u64, result: Result<WorkspaceReply, String>) -> bool {
        let Some(pending) = self.pending.remove(&request_id) else {
            return false;
        };
        match pending {
            Pending::Catalog { epoch, append } => {
                if epoch != self.epoch {
                    return false;
                }
                self.loading = false;
                match result {
                    Ok(WorkspaceReply::CatalogPage {
                        rows,
                        next_before,
                        total,
                    }) => {
                        if !append {
                            self.rows.clear();
                        }
                        for row in rows {
                            if let Some(old) = self.rows.iter_mut().find(|old| old.id == row.id) {
                                *old = row;
                            } else {
                                self.rows.push(row);
                            }
                        }
                        self.next_before = next_before;
                        self.total = total;
                        self.connected = true;
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                    _ => {
                        self.notice =
                            Some("Remote host returned an unexpected catalog response.".into())
                    }
                }
            }
            Pending::Search {
                epoch,
                query,
                append,
            } => {
                if epoch != self.epoch || query != self.search_epoch {
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
                    Err(error) => self.notice = Some(error),
                    _ => {
                        self.notice =
                            Some("Remote host returned an unexpected search response.".into())
                    }
                }
            }
            Pending::Document {
                epoch,
                generation,
                conversation,
                event,
                revision,
            } => {
                if epoch != self.epoch || generation != self.document_epoch {
                    return false;
                }
                self.document_loading = false;
                match result {
                    Ok(WorkspaceReply::SearchDocument { document }) if document.conversation == conversation && document.event_id == event && document.revision == revision => { self.document = Some(document); self.notice = None; }
                    Err(error) => self.notice = Some(error),
                    _ => self.notice = Some("Remote evidence did not match the selected identity and revision; nothing was opened.".into()),
                }
            }
        }
        true
    }
    pub(crate) fn poll(&mut self) -> bool {
        let events = self
            .client
            .as_ref()
            .map(|client| client.poll())
            .unwrap_or_default();
        let mut changed = false;
        for WorkspaceEvent::Reply { request_id, result } in events {
            changed |= self.receive(request_id, result);
        }
        if self
            .search_due
            .is_some_and(|due| due.elapsed() >= Duration::from_millis(180))
        {
            self.search(false);
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_and_query_epochs_reject_old_remote_results() {
        let mut state = RemoteWorkspace::default();
        state.epoch = 2;
        state.search_epoch = 3;
        state.pending.insert(
            1,
            Pending::Search {
                epoch: 1,
                query: 3,
                append: false,
            },
        );
        assert!(!state.receive(
            1,
            Ok(WorkspaceReply::Search {
                page: WorkspaceSearchPage::default()
            })
        ));
        state.pending.insert(
            2,
            Pending::Search {
                epoch: 2,
                query: 2,
                append: false,
            },
        );
        assert!(!state.receive(2, Err("old query error".into())));
        assert!(state.notice.is_none());
        state.pending.insert(
            3,
            Pending::Catalog {
                epoch: 2,
                append: false,
            },
        );
        state.disconnect();
        assert!(!state.receive(
            3,
            Ok(WorkspaceReply::CatalogPage {
                rows: Vec::new(),
                next_before: None,
                total: 0
            })
        ));
        assert!(!state.connected);
    }

    #[test]
    fn a_slow_remote_search_keeps_only_the_latest_debounced_request_due() {
        let mut state = RemoteWorkspace::default();
        state.epoch = 1;
        state.pending.insert(
            1,
            Pending::Search {
                epoch: 1,
                query: 0,
                append: false,
            },
        );
        state.set_search("latest query", None);
        state.search(false);
        assert!(
            state.search_due.is_some(),
            "a pending search must not consume the latest query"
        );
        assert_eq!(
            state.pending.len(),
            1,
            "typing queued an additional SSH search"
        );
        assert!(!state.receive(
            1,
            Ok(WorkspaceReply::Search {
                page: WorkspaceSearchPage::default()
            })
        ));
        assert!(state.search_due.is_some());
        assert!(state.page.hits.is_empty());
    }
}
