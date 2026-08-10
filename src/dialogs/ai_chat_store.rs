//! Bounded, toolkit-independent state for the persistent AI chat sidebar.
//!
//! GTK renders only the selected chat. This store owns all durable state and
//! keys every asynchronous mutation by `(chat_id, epoch)`, so selecting a
//! different row while a request runs cannot redirect its reply.

use crate::ai::{
    BlockContext, ChatSnapshot, ConversationSnapshot, Role, Turn, MAX_PERSISTED_CHATS,
};

pub(crate) const DEFAULT_CHAT_TITLE: &str = "New chat";
pub(crate) const MAX_LIVE_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_LIVE_ASSISTANT_BYTES: usize = 256 * 1024;
const MAX_LIVE_TURNS: usize = 100;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ChatStatus {
    #[default]
    Idle,
    Thinking(String),
    Info(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct RequestToken {
    pub(crate) chat_id: u64,
    pub(crate) epoch: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RequestStart {
    pub(crate) token: RequestToken,
    pub(crate) history: Vec<Turn>,
    pub(crate) effective_context: Option<BlockContext>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatSummary {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) preview: String,
    pub(crate) archived: bool,
    pub(crate) active: bool,
    pub(crate) busy: bool,
    pub(crate) unread: bool,
    pub(crate) error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatStoreError {
    LimitReached,
    Archived,
    Busy,
    EmptyMessage,
    MessageTooLarge,
    SnapshotInvalid,
}

#[derive(Clone, Debug)]
struct ChatRuntime {
    id: u64,
    title: String,
    archived: bool,
    history: Vec<Turn>,
    block_context: Option<BlockContext>,
    draft: String,
    history_truncated: bool,
    epoch: u64,
    active_epoch: Option<u64>,
    pending_user: Option<String>,
    restore_pending_as_draft: bool,
    previous_context: Option<Option<BlockContext>>,
    partial: String,
    status: ChatStatus,
    unread: bool,
}

impl ChatRuntime {
    fn empty(id: u64) -> Self {
        Self {
            id,
            title: DEFAULT_CHAT_TITLE.into(),
            archived: false,
            history: Vec::new(),
            block_context: None,
            draft: String::new(),
            history_truncated: false,
            epoch: 0,
            active_epoch: None,
            pending_user: None,
            restore_pending_as_draft: false,
            previous_context: None,
            partial: String::new(),
            status: ChatStatus::Idle,
            unread: false,
        }
    }

    fn from_snapshot(snapshot: ChatSnapshot) -> Self {
        let (id, title, archived, history, block_context, draft, history_truncated) =
            snapshot.into_parts();
        Self {
            id,
            title,
            archived,
            history,
            block_context,
            draft: bounded_message(draft),
            history_truncated,
            ..Self::empty(id)
        }
    }

    fn busy(&self) -> bool {
        self.active_epoch.is_some()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChatStore {
    chats: Vec<ChatRuntime>,
    active_chat_id: u64,
    next_id: u64,
}

impl Default for ChatStore {
    fn default() -> Self {
        Self {
            chats: vec![ChatRuntime::empty(1)],
            active_chat_id: 1,
            next_id: 2,
        }
    }
}

impl ChatStore {
    pub(crate) fn restore(snapshot: ConversationSnapshot) -> Self {
        let (active_chat_id, snapshots) = snapshot.into_parts();
        let chats: Vec<_> = snapshots
            .into_iter()
            .map(ChatRuntime::from_snapshot)
            .collect();
        let next_id = chats
            .iter()
            .map(|chat| chat.id)
            .max()
            .unwrap_or(0)
            .wrapping_add(1)
            .max(1);
        let mut store = Self {
            chats,
            active_chat_id,
            next_id,
        };
        store.active_mut().unread = false;
        store
    }

    pub(crate) fn snapshot(&self, redact: bool) -> Result<ConversationSnapshot, ChatStoreError> {
        let chats = self
            .chats
            .iter()
            .map(|chat| {
                let mut title = chat.title.clone();
                let mut history = chat.history.clone();
                let mut context = durable_context(chat);
                let mut draft = durable_draft(chat);
                if redact {
                    title = jterm_core::redact::redact_secrets(&title);
                    draft = jterm_core::redact::redact_secrets(&draft);
                    for turn in &mut history {
                        turn.text = jterm_core::redact::redact_secrets(&turn.text);
                    }
                    if let Some(context) = context.as_mut() {
                        context.cmd = jterm_core::redact::redact_secrets(&context.cmd);
                        context.output = jterm_core::redact::redact_secrets(&context.output);
                        context.cwd = context
                            .cwd
                            .take()
                            .map(|cwd| jterm_core::redact::redact_secrets(&cwd));
                    }
                }
                ChatSnapshot::from_completed_history(
                    chat.id,
                    &title,
                    chat.archived,
                    &history,
                    context.as_ref(),
                    &draft,
                )
                .with_history_truncated(chat.history_truncated)
            })
            .collect();
        ConversationSnapshot::from_chats(self.active_chat_id, chats)
            .ok_or(ChatStoreError::SnapshotInvalid)
    }

    pub(crate) fn active_id(&self) -> u64 {
        self.active_chat_id
    }

    pub(crate) fn active_title(&self) -> &str {
        &self.active().title
    }

    pub(crate) fn active_archived(&self) -> bool {
        self.active().archived
    }

    pub(crate) fn active_history(&self) -> &[Turn] {
        &self.active().history
    }

    pub(crate) fn active_context(&self) -> Option<&BlockContext> {
        self.active().block_context.as_ref()
    }

    pub(crate) fn active_draft(&self) -> &str {
        &self.active().draft
    }

    pub(crate) fn active_partial(&self) -> &str {
        &self.active().partial
    }

    pub(crate) fn active_status(&self) -> &ChatStatus {
        &self.active().status
    }

    pub(crate) fn active_request_token(&self) -> Option<RequestToken> {
        let chat = self.active();
        chat.active_epoch.map(|epoch| RequestToken {
            chat_id: chat.id,
            epoch,
        })
    }

    pub(crate) fn set_active_draft(&mut self, draft: String) -> bool {
        let draft = bounded_message(draft);
        if self.active().draft == draft {
            return false;
        }
        self.active_mut().draft = draft;
        true
    }

    pub(crate) fn clear_active_context(&mut self) -> Result<bool, ChatStoreError> {
        if self.active().busy() {
            return Err(ChatStoreError::Busy);
        }
        Ok(self.active_mut().block_context.take().is_some())
    }

    pub(crate) fn new_chat(&mut self) -> Result<u64, ChatStoreError> {
        if self.chats.len() >= MAX_PERSISTED_CHATS {
            return Err(ChatStoreError::LimitReached);
        }
        let mut id = self.next_id.max(1);
        while self.chats.iter().any(|chat| chat.id == id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);
        self.chats.push(ChatRuntime::empty(id));
        self.active_chat_id = id;
        Ok(id)
    }

    pub(crate) fn select_chat(&mut self, id: u64) -> bool {
        if id == self.active_chat_id || !self.chats.iter().any(|chat| chat.id == id) {
            return false;
        }
        self.active_chat_id = id;
        self.active_mut().unread = false;
        true
    }

    pub(crate) fn rename_active(&mut self, title: &str) -> bool {
        let title = normalise_title(title);
        if title == self.active().title {
            return false;
        }
        self.active_mut().title = title;
        true
    }

    pub(crate) fn toggle_archive_active(&mut self) -> Result<bool, ChatStoreError> {
        if self.active().busy() {
            return Err(ChatStoreError::Busy);
        }
        if self.active().archived {
            self.active_mut().archived = false;
            return Ok(false);
        }
        let archived = self.active_chat_id;
        self.active_mut().archived = true;
        if let Some(id) = self
            .chats
            .iter()
            .rev()
            .find(|chat| chat.id != archived && !chat.archived)
            .map(|chat| chat.id)
        {
            self.active_chat_id = id;
        } else {
            self.new_chat()?;
        }
        Ok(true)
    }

    pub(crate) fn delete_active(&mut self) -> Result<u64, ChatStoreError> {
        if self.active().busy() {
            return Err(ChatStoreError::Busy);
        }
        let deleted = self.active_chat_id;
        self.chats.retain(|chat| chat.id != deleted);
        if let Some(id) = self
            .chats
            .iter()
            .rev()
            .find(|chat| !chat.archived)
            .map(|chat| chat.id)
        {
            self.active_chat_id = id;
        } else {
            let _ = self.new_chat();
        }
        Ok(deleted)
    }

    pub(crate) fn begin_turn(
        &mut self,
        text: String,
        context: Option<BlockContext>,
        thinking: String,
        restore_pending_as_draft: bool,
    ) -> Result<RequestStart, ChatStoreError> {
        if text.trim().is_empty() {
            return Err(ChatStoreError::EmptyMessage);
        }
        if text.len() > MAX_LIVE_MESSAGE_BYTES {
            return Err(ChatStoreError::MessageTooLarge);
        }
        if self.active().archived {
            return Err(ChatStoreError::Archived);
        }
        if self.active().busy() {
            return Err(ChatStoreError::Busy);
        }
        let chat = self.active_mut();
        chat.previous_context = context.as_ref().map(|_| chat.block_context.clone());
        if let Some(context) = context {
            chat.block_context = Some(context);
        }
        if chat.title == DEFAULT_CHAT_TITLE && chat.history.is_empty() {
            chat.title = title_from_text(&text);
        }
        chat.epoch = chat.epoch.wrapping_add(1);
        let token = RequestToken {
            chat_id: chat.id,
            epoch: chat.epoch,
        };
        chat.history.push(Turn {
            role: Role::User,
            text: text.clone(),
        });
        chat.active_epoch = Some(token.epoch);
        chat.pending_user = Some(text);
        chat.restore_pending_as_draft = restore_pending_as_draft;
        chat.partial.clear();
        chat.status = ChatStatus::Thinking(thinking);
        chat.unread = false;
        Ok(RequestStart {
            token,
            history: chat.history.clone(),
            effective_context: chat.block_context.clone(),
        })
    }

    pub(crate) fn push_delta(&mut self, token: RequestToken, text: &str) -> Option<bool> {
        let active = self.active_chat_id;
        let chat = self.chat_mut(token.chat_id)?;
        if chat.active_epoch != Some(token.epoch) {
            return None;
        }
        let room = MAX_LIVE_ASSISTANT_BYTES.saturating_sub(chat.partial.len());
        let mut end = text.len().min(room);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chat.partial.push_str(&text[..end]);
        Some(chat.id == active)
    }

    pub(crate) fn complete_success(&mut self, token: RequestToken, text: String) -> Option<bool> {
        let active = self.active_chat_id;
        let chat = self.chat_mut(token.chat_id)?;
        if chat.active_epoch != Some(token.epoch) {
            return None;
        }
        let truncated = text.len() > MAX_LIVE_ASSISTANT_BYTES;
        chat.history.push(Turn {
            role: Role::Assistant,
            text: bounded_assistant(text),
        });
        chat.active_epoch = None;
        chat.pending_user = None;
        chat.restore_pending_as_draft = false;
        chat.previous_context = None;
        chat.partial.clear();
        chat.status = ChatStatus::Idle;
        chat.history_truncated |= truncated;
        chat.unread = chat.id != active;
        while chat.history.len() > MAX_LIVE_TURNS {
            if chat.history.len() < 2 {
                break;
            }
            chat.history.drain(..2);
            chat.history_truncated = true;
        }
        Some(chat.id == active)
    }

    pub(crate) fn complete_error(&mut self, token: RequestToken, message: String) -> Option<bool> {
        self.rollback(token, ChatStatus::Error(message))
    }

    pub(crate) fn cancel_request(&mut self, token: RequestToken, message: String) -> Option<bool> {
        self.rollback(token, ChatStatus::Info(message))
    }

    fn rollback(&mut self, token: RequestToken, status: ChatStatus) -> Option<bool> {
        let active = self.active_chat_id;
        let chat = self.chat_mut(token.chat_id)?;
        if chat.active_epoch != Some(token.epoch) {
            return None;
        }
        let popped = if chat
            .history
            .last()
            .is_some_and(|turn| turn.role == Role::User)
        {
            chat.history.pop().map(|turn| turn.text)
        } else {
            None
        };
        if chat.restore_pending_as_draft {
            let pending = chat.pending_user.take().or(popped).unwrap_or_default();
            chat.draft = merge_drafts(&pending, &chat.draft);
        } else {
            chat.pending_user = None;
        }
        if let Some(previous) = chat.previous_context.take() {
            chat.block_context = previous;
        }
        chat.active_epoch = None;
        chat.restore_pending_as_draft = false;
        chat.partial.clear();
        chat.status = status;
        chat.unread = chat.id != active;
        Some(chat.id == active)
    }

    pub(crate) fn summaries(&self, query: &str) -> Vec<ChatSummary> {
        let query = query.trim().to_lowercase();
        self.chats
            .iter()
            .rev()
            .filter(|chat| {
                query.is_empty()
                    || chat.title.to_lowercase().contains(&query)
                    || preview(chat).to_lowercase().contains(&query)
            })
            .map(|chat| ChatSummary {
                id: chat.id,
                title: chat.title.clone(),
                preview: preview(chat),
                archived: chat.archived,
                active: chat.id == self.active_chat_id,
                busy: chat.busy(),
                unread: chat.unread,
                error: matches!(chat.status, ChatStatus::Error(_)),
            })
            .collect()
    }

    /// Materialize a memory-only retry into a cloned store before persistence.
    /// This is intentionally used on a clone: the live composer can preserve
    /// an unrelated draft while a selected-Block request is running.
    pub(crate) fn recover_retry_payload(
        &mut self,
        chat_id: u64,
        user_text: &str,
        context: Option<BlockContext>,
    ) -> bool {
        let Some(chat) = self.chat_mut(chat_id) else {
            return false;
        };
        if chat.busy()
            && chat
                .history
                .last()
                .is_some_and(|turn| turn.role == Role::User)
        {
            chat.history.pop();
        }
        chat.active_epoch = None;
        chat.pending_user = None;
        chat.previous_context = None;
        chat.partial.clear();
        chat.restore_pending_as_draft = false;
        chat.draft = merge_drafts(user_text, &chat.draft);
        if let Some(context) = context {
            chat.block_context = Some(context);
        }
        true
    }

    fn active(&self) -> &ChatRuntime {
        self.chats
            .iter()
            .find(|chat| chat.id == self.active_chat_id)
            .expect("active chat invariant")
    }

    fn active_mut(&mut self) -> &mut ChatRuntime {
        let id = self.active_chat_id;
        self.chat_mut(id).expect("active chat invariant")
    }

    fn chat_mut(&mut self, id: u64) -> Option<&mut ChatRuntime> {
        self.chats.iter_mut().find(|chat| chat.id == id)
    }
}

fn durable_context(chat: &ChatRuntime) -> Option<BlockContext> {
    chat.previous_context
        .as_ref()
        .cloned()
        .unwrap_or_else(|| chat.block_context.clone())
}

fn durable_draft(chat: &ChatRuntime) -> String {
    if chat.restore_pending_as_draft {
        if let Some(pending) = chat.pending_user.as_deref() {
            return merge_drafts(pending, &chat.draft);
        }
    }
    chat.draft.clone()
}

fn bounded_message(mut text: String) -> String {
    if text.len() > MAX_LIVE_MESSAGE_BYTES {
        let mut end = MAX_LIVE_MESSAGE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn bounded_assistant(mut text: String) -> String {
    const NOTICE: &str = "\n\n[Response truncated to the 256 KiB live limit.]";
    if text.len() <= MAX_LIVE_ASSISTANT_BYTES {
        return text;
    }
    let mut end = MAX_LIVE_ASSISTANT_BYTES.saturating_sub(NOTICE.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(NOTICE);
    text
}

fn merge_drafts(first: &str, second: &str) -> String {
    if first.is_empty()
        || first == second
        || second
            .strip_prefix(first)
            .is_some_and(|suffix| suffix.starts_with("\n\n"))
    {
        return bounded_message(second.to_string());
    }
    if second.is_empty() {
        return bounded_message(first.to_string());
    }
    bounded_message(format!("{first}\n\n{second}"))
}

fn normalise_title(title: &str) -> String {
    let collapsed = title
        .chars()
        .map(|ch| {
            if ch.is_control() || ch.is_whitespace() {
                ' '
            } else if crate::review_input::is_visual_spoofing_character(ch) {
                '\u{fffd}'
            } else {
                ch
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut bounded = String::new();
    for ch in collapsed.chars().take(80) {
        if bounded.len().saturating_add(ch.len_utf8()) > 256 {
            break;
        }
        bounded.push(ch);
    }
    if bounded.is_empty() {
        DEFAULT_CHAT_TITLE.into()
    } else {
        bounded
    }
}

fn title_from_text(text: &str) -> String {
    let title = normalise_title(text);
    let mut chars = title.chars();
    let short: String = chars.by_ref().take(52).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

fn preview(chat: &ChatRuntime) -> String {
    let source = chat
        .history
        .last()
        .map(|turn| turn.text.as_str())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| (!chat.draft.trim().is_empty()).then_some(chat.draft.as_str()))
        .unwrap_or("Empty conversation");
    let collapsed = source.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let short: String = chars.by_ref().take(72).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(store: &mut ChatStore, text: &str) -> RequestToken {
        store
            .begin_turn(text.into(), None, "Thinking…".into(), true)
            .unwrap()
            .token
    }

    #[test]
    fn chats_keep_independent_history_drafts_and_titles() {
        let mut store = ChatStore::default();
        let first = store.active_id();
        store.set_active_draft("draft one".into());
        let token = start(&mut store, "first question");
        store.complete_success(token, "first answer".into());
        let second = store.new_chat().unwrap();
        store.set_active_draft("draft two".into());
        assert_ne!(first, second);
        assert!(store.select_chat(first));
        assert_eq!(store.active_draft(), "draft one");
        assert_eq!(store.active_history().len(), 2);
        assert_eq!(store.active_title(), "first question");
    }

    #[test]
    fn late_results_are_owned_by_chat_and_epoch() {
        let mut store = ChatStore::default();
        let first = store.active_id();
        let first_request = start(&mut store, "one");
        let second = store.new_chat().unwrap();
        let second_request = start(&mut store, "two");
        assert_eq!(
            store.complete_success(first_request, "answer one".into()),
            Some(false)
        );
        assert_eq!(store.active_id(), second);
        assert_eq!(
            store.complete_error(second_request, "failed".into()),
            Some(true)
        );
        assert!(store.select_chat(first));
        assert_eq!(store.active_history()[1].text, "answer one");
        assert_eq!(store.complete_success(first_request, "stale".into()), None);
    }

    #[test]
    fn inflight_and_failed_requests_restore_as_drafts() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "retry me");
        let snapshot = store.snapshot(false).unwrap();
        assert_eq!(snapshot.active_chat().unwrap().draft(), "retry me");
        store.complete_error(token, "offline".into());
        assert_eq!(store.active_draft(), "retry me");
        assert!(store.active_history().is_empty());
    }

    #[test]
    fn selected_block_retry_can_be_materialized_without_touching_live_draft() {
        let mut live = ChatStore::default();
        live.set_active_draft("unrelated notes".into());
        let context = BlockContext {
            cmd: "false".into(),
            output: "failed".into(),
            cwd: Some("/tmp".into()),
            exit_code: 1,
            truncated: false,
        };
        live.begin_turn(
            "diagnose".into(),
            Some(context.clone()),
            "Thinking…".into(),
            false,
        )
        .unwrap();
        assert_eq!(live.active_draft(), "unrelated notes");

        let mut durable = live.clone();
        assert!(durable.recover_retry_payload(1, "diagnose", Some(context.clone())));
        assert_eq!(durable.active_context(), Some(&context));
        let snapshot = durable.snapshot(false).unwrap();
        let chat = snapshot.active_chat().unwrap();
        assert_eq!(chat.draft(), "diagnose\n\nunrelated notes");
        assert!(chat.turns().is_empty());
        // The shared snapshot schema deliberately associates Block context
        // only with a completed user/assistant pair. The retry still survives
        // as a draft, while the cloned runtime retains its context until the
        // app either retries or finishes shutting down.
        assert!(chat.block_context().is_none());
        assert_eq!(live.active_draft(), "unrelated notes");
        assert_eq!(live.active_context(), Some(&context));
        assert!(live.active_request_token().is_some());
    }

    #[test]
    fn snapshot_round_trip_keeps_multiple_chat_metadata() {
        let mut store = ChatStore::default();
        let token = start(&mut store, "hello");
        store.complete_success(token, "world".into());
        store.rename_active("Renamed");
        store.new_chat().unwrap();
        store.set_active_draft("unfinished".into());
        let encoded = store.snapshot(false).unwrap().to_json().unwrap();
        let restored = ChatStore::restore(ConversationSnapshot::from_json(&encoded).unwrap());
        assert_eq!(restored.summaries("").len(), 2);
        assert_eq!(restored.active_draft(), "unfinished");
        assert!(restored
            .summaries("renamed")
            .iter()
            .any(|chat| chat.id == 1));
    }

    #[test]
    fn archive_and_delete_always_leave_a_writable_chat() {
        let mut store = ChatStore::default();
        let archived_id = store.active_id();
        assert!(store.toggle_archive_active().unwrap());
        assert!(!store.active_archived());
        store.delete_active().unwrap();
        assert!(!store.active_archived());
        let summaries = store.summaries("");
        // Archive preserves the original chat. Deleting its temporary
        // writable replacement therefore creates another writable chat; it
        // must not silently delete or unarchive the archived conversation.
        assert_eq!(summaries.len(), 2);
        assert!(summaries
            .iter()
            .any(|chat| chat.id == archived_id && chat.archived));
        assert_eq!(summaries.iter().filter(|chat| !chat.archived).count(), 1);
        assert!(summaries
            .iter()
            .any(|chat| chat.id == store.active_id() && !chat.archived));
    }
}
