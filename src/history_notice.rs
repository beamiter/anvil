//! Persistent surface for Block-history fail-closed states.
//!
//! A Block-history save can refuse for reasons that stay true until somebody
//! acts: the file's revision moved under this window, the load it must not
//! overwrite failed, the volume is full. Those used to arrive as the same
//! eight-second toast every other persistence failure gets, so the one class
//! of failure that *needs* a decision was the class most likely to be missed.
//! This bar stays until it is answered, and it carries the answer — matching
//! forge's `ui/history_notice.rs`.
//!
//! Production saves enqueue under `"Save Block history"` so worker I/O
//! refusals reach `persistence::drain_failures` and raise this bar. Failed-load
//! overwrite refusals and enqueue admission errors that never leave the GTK
//! thread are parked here too; `AppModel` drains both onto the sticky chrome
//! via [`partition_persistence_failures`] so Block-history never falls through
//! to the toast cooldown. Explicit Clear answers a Failed load (forge
//! ExplicitReplace bypass) and must not park the ordinary overwrite refusal.

use gtk::prelude::*;
use relm4::gtk;
use std::sync::Mutex;

use crate::block_view::BLOCK_HISTORY_PERSIST_OPERATION;
use crate::persistence::PersistenceFailure;

/// Where a persistence failure should be shown.
///
/// Most operations keep the ordinary toast. Block-history saves are fail-closed
/// and stay wrong until somebody acts, so they get a sticky bar rather than an
/// eight-second toast.
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

/// Split drained persistence failures into sticky-bar vs toast buckets.
///
/// Block-history must never fall through to the eight-second toast cooldown
/// even when mixed with routine failures in the same drain — the sticky bar is
/// the only surface that waits for an answer.
pub(crate) fn partition_persistence_failures(
    failures: impl IntoIterator<Item = PersistenceFailure>,
) -> (Vec<PersistenceFailure>, Vec<PersistenceFailure>) {
    let mut bar = Vec::new();
    let mut toast = Vec::new();
    for failure in failures {
        match persistence_failure_surface(&failure.operation) {
            PersistenceFailureSurface::BlockHistoryBar => bar.push(failure),
            PersistenceFailureSurface::Toast => toast.push(failure),
        }
    }
    (bar, toast)
}

static SYNC_BLOCK_HISTORY_FAILURES: Mutex<Vec<PersistenceFailure>> = Mutex::new(Vec::new());

/// Park a Block-history refusal that never reached the persistence worker.
///
/// Failed-load overwrite refusals and enqueue admission errors still return on
/// the GTK thread, so they never appear in `persistence::drain_failures`. The
/// newest reason replaces older ones for the same operation string: one window,
/// one file family.
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

/// Sticky Retry bar chrome under the top bar (forge `build_block_history_notice`).
///
/// The caller places `bar` in the window layout and wires `retry` / `dismiss`.
/// Label text and visibility are updated through [`reveal_block_history_failure`].
pub(crate) struct BlockHistoryNoticeChrome {
    pub bar: gtk::Box,
    pub label: gtk::Label,
    pub retry: gtk::Button,
    pub dismiss: gtk::Button,
}

/// Build the (initially hidden) Block-history failure bar.
pub(crate) fn build_block_history_notice() -> BlockHistoryNoticeChrome {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.add_css_class("toolbar");
    bar.add_css_class("error");
    bar.set_margin_start(6);
    bar.set_margin_end(6);
    bar.set_margin_top(2);
    bar.set_margin_bottom(2);
    bar.set_visible(false);

    let label = gtk::Label::new(None);
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    // One line, shortened in the middle: a notice bar must not grow the
    // header when the window is narrow. The whole reason is in the log.
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    label.set_xalign(0.0);
    bar.append(&label);

    let retry = gtk::Button::with_label("Retry");
    retry.add_css_class("suggested-action");
    retry.set_tooltip_text(Some("Reload and save this window's Block history again"));
    bar.append(&retry);

    let dismiss = gtk::Button::from_icon_name("window-close-symbolic");
    dismiss.add_css_class("flat");
    dismiss.set_tooltip_text(Some("Hide until the next failure"));
    dismiss.update_property(&[gtk::accessible::Property::Label(
        "Hide Block history failure notice",
    )]);
    bar.append(&dismiss);

    BlockHistoryNoticeChrome {
        bar,
        label,
        retry,
        dismiss,
    }
}

/// Raise the bar for a Block-history persistence failure.
///
/// The newest reason replaces an older one rather than queueing behind it:
/// every pane in this window shares one file family, and a stale reason would
/// send the user after a problem that has already been superseded.
pub(crate) fn reveal_block_history_failure(
    bar: &gtk::Box,
    label: &gtk::Label,
    reason: &str,
) {
    let reason = crate::review_input::safe_inline_display(reason, 2 * 1024);
    log::error!("Block history is not being saved: {reason}");
    label.set_text(&format!("Block history was not saved: {reason}"));
    bar.set_visible(true);
}

#[cfg(test)]
mod tests {
    use super::{
        drain_sync_block_history_failures, park_sync_block_history_failure,
        partition_persistence_failures, persistence_failure_surface, PersistenceFailureSurface,
    };
    use crate::block_view::BLOCK_HISTORY_PERSIST_OPERATION;
    use crate::persistence::PersistenceFailure;
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

    /// Near-miss labels must not raise the sticky bar — the surface is keyed
    /// on the exact `"Save Block history"` operation string workers enqueue.
    #[test]
    fn near_miss_operation_labels_stay_on_the_toast_surface() {
        for near_miss in [
            "Save Block history ",
            " Save Block history",
            "save Block history",
            "Save Block History",
            "Save block history",
            "Save Block histor",
            "Save Block history\n",
            "Save Block history\t",
            "Save Block history\r",
            "Save\tBlock history",
            "Save Block\u{00a0}history",
            // Invisible / format-control near-misses beside tab/CR/NBSP.
            "Save Block\u{200b}history",
            "\u{feff}Save Block history",
            "Save Block history\u{000b}",
            "Block history",
        ] {
            assert_eq!(
                persistence_failure_surface(near_miss),
                PersistenceFailureSurface::Toast,
                "{near_miss:?}"
            );
        }
        let mixed = vec![
            PersistenceFailure {
                operation: "Save Block history ".to_string(),
                error: "padded".to_string(),
            },
            PersistenceFailure {
                operation: BLOCK_HISTORY_PERSIST_OPERATION.to_string(),
                error: "real".to_string(),
            },
            PersistenceFailure {
                operation: "save Block history".to_string(),
                error: "cased".to_string(),
            },
        ];
        let (bar, toast) = partition_persistence_failures(mixed);
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].error, "real");
        assert_eq!(toast.len(), 2);
    }

    #[test]
    fn partition_keeps_block_history_off_the_toast_cooldown() {
        let mixed = vec![
            PersistenceFailure {
                operation: "Save window session".to_string(),
                error: "disk full".to_string(),
            },
            PersistenceFailure {
                operation: BLOCK_HISTORY_PERSIST_OPERATION.to_string(),
                error: "revision moved".to_string(),
            },
            PersistenceFailure {
                operation: "Save AI conversation".to_string(),
                error: "permission denied".to_string(),
            },
            PersistenceFailure {
                operation: BLOCK_HISTORY_PERSIST_OPERATION.to_string(),
                error: "volume full".to_string(),
            },
        ];
        let (bar, toast) = partition_persistence_failures(mixed);
        assert_eq!(bar.len(), 2);
        assert!(bar.iter().all(|f| f.operation == BLOCK_HISTORY_PERSIST_OPERATION));
        assert_eq!(toast.len(), 2);
        assert!(toast
            .iter()
            .all(|f| persistence_failure_surface(&f.operation)
                == PersistenceFailureSurface::Toast));
        // Newest Block-history reason is preserved in order for the bar.
        assert_eq!(bar[1].error, "volume full");
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
