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
//! / admission refusals that never leave the GTK thread are parked here too;
//! `AppModel` drains both onto the sticky chrome.

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
