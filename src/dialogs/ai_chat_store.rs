//! anvil's binding to the shared multi-chat store in `jterm_core::ai`.
//!
//! Every jterm terminal grew its own copy of this state machine; the copies
//! contained no toolkit code at all, so they drifted. The union now lives in
//! `jterm_core::ai::chat_store` (forge's hardening — an aggregate live-history
//! budget with real compaction, persistence that compacts before serialising,
//! typed archive/delete outcomes — plus anvil's in-store streaming, library
//! filtering and idempotent draft merging). What is left here is the one
//! decision the shared store deliberately refuses to make for its callers.
//!
//! The panel keeps talking to `ChatStore` directly; only construction goes
//! through this module, so anvil's busy policy cannot be forgotten at one of
//! the two entry points.

pub(crate) use jterm_core::ai::{
    ChatStatus, ChatStore, ChatStoreError, RequestToken, MAX_LIVE_MESSAGE_BYTES,
};

use jterm_core::ai::{BusyChatPolicy, ConversationSnapshot};

/// Archive and Delete are plain buttons in anvil's panel: there is no
/// cancel-then-mutate step, so the store must refuse them while a reply is
/// streaming and let the panel say "Stop this response before …". forge picked
/// `Allow` precisely because its panel cancels first.
const BUSY_POLICY: BusyChatPolicy = BusyChatPolicy::Refuse;

/// A fresh library holding one empty chat.
pub(crate) fn new_chat_store() -> ChatStore {
    ChatStore::with_busy_policy(BUSY_POLICY)
}

/// A library restored from a persisted session snapshot.
pub(crate) fn restore_chat_store(snapshot: ConversationSnapshot) -> ChatStore {
    ChatStore::restore_with_busy_policy(snapshot, BUSY_POLICY)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store's own semantics are covered by the core's unit tests. What
    /// anvil still owns is the policy choice, and that it survives a restore:
    /// a library rebuilt from disk must refuse the same mutations as a fresh
    /// one, or a chat restored mid-request would become deletable.
    #[test]
    fn both_constructors_refuse_mutating_a_busy_chat() {
        let mut store = new_chat_store();
        assert_eq!(store.busy_policy(), BusyChatPolicy::Refuse);
        store
            .begin_turn("ask".into(), None, "Thinking…".into(), true)
            .expect("a fresh chat accepts a turn");
        assert_eq!(
            store.toggle_archive_active().unwrap_err(),
            ChatStoreError::Busy
        );
        assert_eq!(store.delete_active().unwrap_err(), ChatStoreError::Busy);

        let (snapshot, _) = store
            .snapshot_for_persistence(false)
            .expect("a one-chat library is always persistable");
        assert_eq!(
            restore_chat_store(snapshot).busy_policy(),
            BusyChatPolicy::Refuse
        );
    }

    /// anvil's own copy of the store used to merge a rolled-back question in
    /// front of the composer draft, silently dropping whatever crossed 64 KiB
    /// — always the follow-up the user had just typed by hand, and then
    /// persisted in that truncated form. The shared store reports the loss.
    /// The panel renders `active_status()` verbatim and the library row keys
    /// on `history_truncated`, so both halves of the report have a consumer
    /// here; this pins them against a future core that stops reporting.
    #[test]
    fn a_failed_request_says_when_the_recovered_draft_lost_bytes() {
        let mut store = new_chat_store();
        let question = "q".repeat(60 * 1024);
        let start = store
            .begin_turn(question, None, "Thinking…".into(), true)
            .expect("60 KiB is inside the live message budget");
        // Typed into the composer while the request was in flight.
        assert!(store.set_active_draft("f".repeat(6 * 1024)));

        assert_eq!(
            store.complete_error(start.token, "AI error: upstream refused.".into()),
            Some(true)
        );

        let ChatStatus::Error(message) = store.active_status() else {
            panic!("a failed request leaves an error status");
        };
        assert!(
            message.contains("omitted at the 64 KiB limit"),
            "the dropped follow-up must be reported: {message}"
        );
        assert!(store.active_history_truncated());
        assert_eq!(store.active_draft().len(), MAX_LIVE_MESSAGE_BYTES);
    }
}
