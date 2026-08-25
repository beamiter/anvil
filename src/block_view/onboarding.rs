//! One-shot empty-state guidance for the Block document.
//!
//! The card is an overlay, not a child of the scrolling block document or the
//! notice dock.  It therefore never becomes a `FinishedBlock`, competes for an
//! inline-notice parent, or contributes to the live terminal's allocation.

use gtk::prelude::*;
use relm4::gtk;
use std::cell::Cell;
use std::rc::Rc;

pub(crate) const BLOCK_ONBOARDING_ACCESSIBLE_LABEL: &str =
    "Finished commands become reusable cards here. \
Click a card header to select it. Right-click a card for more actions. \
Press Control+Shift+G to search.";

const TITLE: &str = "Finished commands become reusable cards here.";
const BODY: &str =
    "Click a card header to select · Right-click for more actions · Ctrl+Shift+G to search";

/// A pane-local, one-way lifecycle for the empty-state card.
///
/// A Block pane waits until its history result is known before revealing the
/// card, which prevents restored panes from flashing an empty state.  Once a
/// completed block is observed, `Dismissed` is absorbing: clearing, filtering,
/// or evicting every card must not make this pane replay onboarding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockOnboardingPhase {
    Disabled,
    AwaitingHistory,
    Visible,
    Dismissed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockOnboardingEvent {
    HistoryResolved { restored_finished_block: bool },
    FinishedBlockObserved,
}

fn transition(phase: BlockOnboardingPhase, event: BlockOnboardingEvent) -> BlockOnboardingPhase {
    use BlockOnboardingEvent::{FinishedBlockObserved, HistoryResolved};
    use BlockOnboardingPhase::{AwaitingHistory, Disabled, Dismissed, Visible};

    match (phase, event) {
        (Disabled, _) => Disabled,
        (Dismissed, _) => Dismissed,
        (
            _,
            FinishedBlockObserved
            | HistoryResolved {
                restored_finished_block: true,
            },
        ) => Dismissed,
        (
            AwaitingHistory,
            HistoryResolved {
                restored_finished_block: false,
            },
        ) => Visible,
        (
            Visible,
            HistoryResolved {
                restored_finished_block: false,
            },
        ) => Visible,
    }
}

struct BlockOnboardingInner {
    card: gtk::Box,
    phase: Cell<BlockOnboardingPhase>,
    surface_suspended: Cell<bool>,
}

/// Owns the pane-local overlay card and its one-way visibility state.
///
/// Clones share both the GTK widget and the state cell, so `TermView`, the
/// history loader, and `BlockBackend` can each retain a lightweight handle.
#[derive(Clone)]
pub(crate) struct BlockOnboarding {
    inner: Rc<BlockOnboardingInner>,
}

impl BlockOnboarding {
    /// Build the card and, for Block mode, attach it to `overlay` without
    /// allowing it to affect measurement or pointer/focus routing.
    ///
    /// `enabled` must be false for Unified.  VTE mode never constructs this
    /// type because it does not use the Block `TermView`.
    pub(crate) fn attach(overlay: &gtk::Overlay, enabled: bool) -> Self {
        let card = build_card();
        let phase = if enabled {
            overlay.add_overlay(&card);
            overlay.set_measure_overlay(&card, false);
            BlockOnboardingPhase::AwaitingHistory
        } else {
            BlockOnboardingPhase::Disabled
        };

        card.set_visible(false);
        Self {
            inner: Rc::new(BlockOnboardingInner {
                card,
                phase: Cell::new(phase),
                surface_suspended: Cell::new(false),
            }),
        }
    }

    /// Resolve the construction-time history gate.
    ///
    /// An empty (or failed/disabled) restore reveals the card only if no live
    /// block completed while the load was pending.  A restored card permanently
    /// dismisses it.
    pub(crate) fn history_resolved(&self, restored_finished_block: bool) {
        self.apply(BlockOnboardingEvent::HistoryResolved {
            restored_finished_block,
        });
    }

    /// Permanently dismiss the card after the first completed block is mounted.
    pub(crate) fn finished_block_observed(&self) {
        self.apply(BlockOnboardingEvent::FinishedBlockObserved);
    }

    /// Temporarily hide orientation while an alternate-screen program owns the
    /// viewport. This does not consume the one-shot lifecycle: if the program
    /// exits without producing a finished card, the empty pane may explain
    /// itself again.
    pub(crate) fn set_surface_suspended(&self, suspended: bool) {
        self.inner.surface_suspended.set(suspended);
        self.sync_visibility();
    }

    #[cfg(test)]
    fn widget(&self) -> &gtk::Widget {
        self.inner.card.upcast_ref()
    }

    #[cfg(test)]
    fn phase(&self) -> BlockOnboardingPhase {
        self.inner.phase.get()
    }

    fn apply(&self, event: BlockOnboardingEvent) {
        let next = transition(self.inner.phase.get(), event);
        self.inner.phase.set(next);
        self.sync_visibility();
    }

    fn sync_visibility(&self) {
        self.inner.card.set_visible(
            self.inner.phase.get() == BlockOnboardingPhase::Visible
                && !self.inner.surface_suspended.get(),
        );
    }
}

fn build_card() -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
    card.add_css_class("block-onboarding");
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Start);
    card.set_margin_top(18);
    card.set_margin_start(18);
    card.set_margin_end(18);
    card.set_can_target(false);
    card.set_focusable(false);
    card.set_accessible_role(gtk::AccessibleRole::Status);
    card.update_property(&[gtk::accessible::Property::Label(
        BLOCK_ONBOARDING_ACCESSIBLE_LABEL,
    )]);

    let title = gtk::Label::new(Some(TITLE));
    title.add_css_class("block-onboarding-title");
    title.set_wrap(true);
    title.set_max_width_chars(72);
    title.set_justify(gtk::Justification::Center);
    title.set_can_target(false);
    title.set_focusable(false);
    title.set_accessible_role(gtk::AccessibleRole::Presentation);

    let body = gtk::Label::new(Some(BODY));
    body.add_css_class("block-onboarding-body");
    body.set_wrap(true);
    body.set_max_width_chars(72);
    body.set_justify(gtk::Justification::Center);
    body.set_can_target(false);
    body.set_focusable(false);
    body.set_accessible_role(gtk::AccessibleRole::Presentation);

    card.append(&title);
    card.append(&body);
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_onboarding_state_is_block_only_and_one_way() {
        use BlockOnboardingEvent::{FinishedBlockObserved, HistoryResolved};
        use BlockOnboardingPhase::{AwaitingHistory, Disabled, Dismissed, Visible};

        assert_eq!(transition(Disabled, FinishedBlockObserved), Disabled);
        assert_eq!(
            transition(
                AwaitingHistory,
                HistoryResolved {
                    restored_finished_block: false,
                }
            ),
            Visible
        );
        assert_eq!(transition(Visible, FinishedBlockObserved), Dismissed);
        assert_eq!(
            transition(
                Dismissed,
                HistoryResolved {
                    restored_finished_block: false,
                }
            ),
            Dismissed,
            "an empty later state must not replay onboarding in this pane"
        );
        assert_eq!(
            transition(
                AwaitingHistory,
                HistoryResolved {
                    restored_finished_block: true,
                }
            ),
            Dismissed
        );
    }

    #[test]
    fn live_finish_wins_over_late_empty_history() {
        use BlockOnboardingEvent::{FinishedBlockObserved, HistoryResolved};
        use BlockOnboardingPhase::{AwaitingHistory, Dismissed};

        let after_live_finish = transition(AwaitingHistory, FinishedBlockObserved);
        assert_eq!(after_live_finish, Dismissed);
        assert_eq!(
            transition(
                after_live_finish,
                HistoryResolved {
                    restored_finished_block: false,
                }
            ),
            Dismissed
        );
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn block_onboarding_overlay_is_non_measuring_and_non_targetable() {
        gtk::init().expect("gtk init");

        let overlay = gtk::Overlay::new();
        let surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        surface.set_size_request(320, 180);
        overlay.set_child(Some(&surface));
        let width_before = overlay.measure(gtk::Orientation::Horizontal, -1);
        let height_before = overlay.measure(gtk::Orientation::Vertical, -1);

        let onboarding = BlockOnboarding::attach(&overlay, true);
        let card = onboarding.widget();
        assert_eq!(card.parent().as_ref(), Some(overlay.upcast_ref()));
        assert!(!overlay.is_measure_overlay(card));
        assert!(!card.can_target());
        assert!(!card.is_focusable());
        assert_eq!(card.accessible_role(), gtk::AccessibleRole::Status);
        assert_eq!(
            overlay.measure(gtk::Orientation::Horizontal, -1),
            width_before
        );
        assert_eq!(
            overlay.measure(gtk::Orientation::Vertical, -1),
            height_before
        );

        assert_eq!(onboarding.phase(), BlockOnboardingPhase::AwaitingHistory);
        assert!(!card.is_visible());
        onboarding.history_resolved(false);
        assert_eq!(onboarding.phase(), BlockOnboardingPhase::Visible);
        assert!(card.is_visible());
        onboarding.set_surface_suspended(true);
        assert_eq!(onboarding.phase(), BlockOnboardingPhase::Visible);
        assert!(
            !card.is_visible(),
            "onboarding must not cover a full-screen app"
        );
        onboarding.history_resolved(false);
        assert!(
            !card.is_visible(),
            "history refresh must not defeat suspension"
        );
        onboarding.set_surface_suspended(false);
        assert!(card.is_visible());
        assert_eq!(
            overlay.measure(gtk::Orientation::Horizontal, -1),
            width_before,
            "revealing an overlay must not change the live surface width"
        );
        assert_eq!(
            overlay.measure(gtk::Orientation::Vertical, -1),
            height_before,
            "revealing an overlay must not consume live terminal rows"
        );
        let backend_handle = onboarding.clone();
        backend_handle.finished_block_observed();
        assert_eq!(onboarding.phase(), BlockOnboardingPhase::Dismissed);
        assert!(!card.is_visible());
        onboarding.history_resolved(false);
        assert!(!card.is_visible());
        onboarding.set_surface_suspended(false);
        assert!(
            !card.is_visible(),
            "dismissal remains absorbing after suspension"
        );

        let disabled = BlockOnboarding::attach(&overlay, false);
        assert_eq!(disabled.phase(), BlockOnboardingPhase::Disabled);
        assert!(disabled.widget().parent().is_none());
        disabled.history_resolved(false);
        assert_eq!(disabled.phase(), BlockOnboardingPhase::Disabled);
        assert!(!disabled.widget().is_visible());
    }
}
