//! Routing for Block-history fail-closed persistence failures.
//!
//! Forge keeps a sticky Retry bar under the top bar. Anvil does not host that
//! chrome yet, but the window still needs (1) a stable way to recognize the
//! `"Save Block history"` operation and (2) a drainable parking lot for the
//! sync GTK-thread save failures that never reach `persistence::drain_failures`
//! today. Sticky Retry binds to both later; until then callers keep log/toast
//! behavior and also park here so the bar has something to show on day one.

use crate::block_view::BLOCK_HISTORY_PERSIST_OPERATION;
use crate::persistence::PersistenceFailure;
use std::sync::Mutex;

/// Where a persistence failure should be shown once the UI surface exists.
///
/// Most operations keep the ordinary toast. Block-history saves are fail-closed
/// and stay wrong until somebody acts, so they get a sticky bar (forge's
/// `history_notice`) rather than an eight-second toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PersistenceFailureSurface {
    BlockHistoryBar,
    Toast,
}

pub(crate) fn persistence_failure_surface(operation: &str) -> PersistenceFailureSurface {
    if operation == BLOCK_HISTORY_PERSIST_OPERATION {
        PersistenceFailureSurface::BlockHistoryBar
    } else {
        PersistenceFailureSurface::Toast
    }
}

static SYNC_BLOCK_HISTORY_FAILURES: Mutex<Vec<PersistenceFailure>> = Mutex::new(Vec::new());

/// Park a synchronous Block-history save/retry refusal for the sticky bar.
///
/// Anvil's `TermView::save_history` still runs on the GTK thread, so these
/// never appear in `persistence::drain_failures`. The newest reason replaces
/// older ones for the same operation string: one window, one file family.
pub(crate) fn park_sync_block_history_failure(error: &std::io::Error) {
    let failure = PersistenceFailure {
        operation: BLOCK_HISTORY_PERSIST_OPERATION.to_string(),
        error: error.to_string(),
    };
    if let Ok(mut parked) = SYNC_BLOCK_HISTORY_FAILURES.lock() {
        parked.retain(|existing| existing.operation != failure.operation);
        parked.push(failure);
    }
}

/// Drain parked sync Block-history failures (for the sticky bar / tests).
pub(crate) fn drain_sync_block_history_failures() -> Vec<PersistenceFailure> {
    SYNC_BLOCK_HISTORY_FAILURES
        .lock()
        .map(|mut parked| std::mem::take(&mut *parked))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        drain_sync_block_history_failures, park_sync_block_history_failure,
        persistence_failure_surface, PersistenceFailureSurface,
    };
    use crate::block_view::BLOCK_HISTORY_PERSIST_OPERATION;
    use std::io;

    #[test]
    fn only_block_history_gets_the_sticky_surface() {
        assert_eq!(
            persistence_failure_surface(BLOCK_HISTORY_PERSIST_OPERATION),
            PersistenceFailureSurface::BlockHistoryBar
        );
        for routine in [
            "Load Block history",
            "Save window session",
            "Save AI conversation",
            "",
        ] {
            assert_eq!(
                persistence_failure_surface(routine),
                PersistenceFailureSurface::Toast,
                "{routine}"
            );
        }
    }

    #[test]
    fn sync_block_history_failures_park_and_drain_newest_wins() {
        let _ = drain_sync_block_history_failures();
        park_sync_block_history_failure(&io::Error::new(io::ErrorKind::PermissionDenied, "first"));
        park_sync_block_history_failure(&io::Error::new(io::ErrorKind::InvalidData, "second"));
        let drained = drain_sync_block_history_failures();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].operation, BLOCK_HISTORY_PERSIST_OPERATION);
        assert_eq!(drained[0].error, "second");
        assert!(drain_sync_block_history_failures().is_empty());
    }
}
