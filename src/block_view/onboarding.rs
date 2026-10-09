//! One-shot empty-state guidance for the Block document.
//!
//! The card is an overlay, not a child of the scrolling block document or the
//! notice dock.  It therefore never becomes a `FinishedBlock`, competes for an
//! inline-notice parent, or contributes to the live terminal's allocation.

use gtk::glib;
use gtk::prelude::*;
use relm4::gtk;
use std::cell::{Cell, RefCell};
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
    HumanInputObserved,
}

fn transition(phase: BlockOnboardingPhase, event: BlockOnboardingEvent) -> BlockOnboardingPhase {
    use BlockOnboardingEvent::{FinishedBlockObserved, HistoryResolved, HumanInputObserved};
    use BlockOnboardingPhase::{AwaitingHistory, Disabled, Dismissed, Visible};

    match (phase, event) {
        (Disabled, _) => Disabled,
        (Dismissed, _) => Dismissed,
        (
            _,
            FinishedBlockObserved
            | HumanInputObserved
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

/// Only notices mounted into this pane may suppress its empty-state hint.
/// Widgets and their mount parents are weak so observation never keeps a
/// notice, document, or pane alive. Signal handlers are retired with the owner.
struct ObservedInlineNotice {
    widget: gtk::glib::WeakRef<gtk::Widget>,
    parent: gtk::glib::WeakRef<gtk::Widget>,
    visible_notify: Option<glib::SignalHandlerId>,
    parent_notify: Option<glib::SignalHandlerId>,
}

impl Drop for ObservedInlineNotice {
    fn drop(&mut self) {
        if let Some(widget) = self.widget.upgrade() {
            if let Some(handler) = self.visible_notify.take() {
                widget.disconnect(handler);
            }
            if let Some(handler) = self.parent_notify.take() {
                widget.disconnect(handler);
            }
        }
    }
}

fn should_show_onboarding(
    phase: BlockOnboardingPhase,
    surface_suspended: bool,
    visible_inline_notice: bool,
) -> bool {
    phase == BlockOnboardingPhase::Visible && !surface_suspended && !visible_inline_notice
}

struct BlockOnboardingInner {
    card: gtk::Box,
    phase: Cell<BlockOnboardingPhase>,
    surface_suspended: Cell<bool>,
    inline_notices: RefCell<Vec<ObservedInlineNotice>>,
}

impl BlockOnboardingInner {
    fn sync_visibility(&self) {
        let visible_notice = self.inline_notices.borrow().iter().any(|notice| {
            let (Some(widget), Some(parent)) = (notice.widget.upgrade(), notice.parent.upgrade())
            else {
                return false;
            };
            widget.is_visible() && widget.parent().as_ref() == Some(&parent)
        });
        self.card.set_visible(should_show_onboarding(
            self.phase.get(),
            self.surface_suspended.get(),
            visible_notice,
        ));
    }
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
                inline_notices: RefCell::new(Vec::new()),
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

    /// Permanently retire guidance when this pane accepts its first human
    /// input. Keeping it until CommandEnd would cover the command being typed
    /// and potentially a long first command's live output.
    pub(crate) fn human_input_observed(&self) {
        self.apply(BlockOnboardingEvent::HumanInputObserved);
    }

    /// Temporarily hide orientation while an alternate-screen program owns the
    /// viewport. This does not consume the one-shot lifecycle: if the program
    /// exits without producing a finished card, the empty pane may explain
    /// itself again.
    pub(crate) fn set_surface_suspended(&self, suspended: bool) {
        self.inner.surface_suspended.set(suspended);
        self.sync_visibility();
    }

    /// Observe a successfully mounted inline card. Visible cards take priority
    /// over empty-state guidance, without consuming its one-shot lifecycle.
    /// Hiding/removing the last card restores guidance only before real input
    /// or the first completed block. Re-pinning never duplicates observers.
    pub(crate) fn observe_inline_notice(&self, widget: &gtk::Widget) {
        let Some(parent) = widget.parent() else {
            return;
        };
        {
            let mut notices = self.inner.inline_notices.borrow_mut();
            notices.retain(|notice| notice.widget.upgrade().is_some());
            if let Some(notice) = notices
                .iter_mut()
                .find(|notice| notice.widget.upgrade().as_ref() == Some(widget))
            {
                notice.parent = parent.downgrade();
            } else {
                let weak = Rc::downgrade(&self.inner);
                let visible_notify = widget.connect_visible_notify(move |_| {
                    if let Some(inner) = weak.upgrade() {
                        inner.sync_visibility();
                    }
                });
                let weak = Rc::downgrade(&self.inner);
                let parent_notify = widget.connect_parent_notify(move |_| {
                    if let Some(inner) = weak.upgrade() {
                        inner.sync_visibility();
                    }
                });
                notices.push(ObservedInlineNotice {
                    widget: widget.downgrade(),
                    parent: parent.downgrade(),
                    visible_notify: Some(visible_notify),
                    parent_notify: Some(parent_notify),
                });
            }
        }
        self.sync_visibility();
    }

    #[cfg(test)]
    pub(crate) fn is_visible(&self) -> bool {
        self.inner.card.is_visible()
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
        self.inner.sync_visibility();
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
        use BlockOnboardingEvent::{FinishedBlockObserved, HistoryResolved, HumanInputObserved};
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
        assert_eq!(transition(Visible, HumanInputObserved), Dismissed);
        assert_eq!(transition(AwaitingHistory, HumanInputObserved), Dismissed);
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
    fn visible_notices_only_suspend_eligible_empty_guidance() {
        for phase in [
            BlockOnboardingPhase::Disabled,
            BlockOnboardingPhase::AwaitingHistory,
            BlockOnboardingPhase::Visible,
            BlockOnboardingPhase::Dismissed,
        ] {
            for suspended in [false, true] {
                assert!(!should_show_onboarding(phase, suspended, true));
                assert_eq!(
                    should_show_onboarding(phase, suspended, false),
                    phase == BlockOnboardingPhase::Visible && !suspended,
                );
            }
        }
    }

    #[test]
    #[ignore = "requires DISPLAY"]
    fn inline_notice_priority_handles_multiple_cards_reparenting_and_narrow_panes() {
        gtk::init().expect("gtk display");
        let main = gtk::glib::MainContext::default();
        for width in [700, 380, 280] {
            let overlay = gtk::Overlay::new();
            let document = gtk::Box::new(gtk::Orientation::Vertical, 0);
            overlay.set_child(Some(&document));
            let onboarding = BlockOnboarding::attach(&overlay, true);
            let window = gtk::Window::builder()
                .default_width(width)
                .default_height(240)
                .child(&overlay)
                .build();
            window.present();
            onboarding.history_resolved(false);
            assert!(onboarding.is_visible());

            let notice = gtk::Label::new(Some("Local companion status"));
            notice.set_wrap(true);
            document.append(&notice);
            for _ in 0..8 {
                onboarding.observe_inline_notice(notice.upcast_ref());
                assert_eq!(onboarding.inner.inline_notices.borrow().len(), 1);
                assert!(!onboarding.is_visible());
                notice.set_visible(false);
                assert!(
                    onboarding.is_visible(),
                    "off restores an untouched empty hint"
                );
                notice.set_visible(true);
                assert!(
                    !onboarding.is_visible(),
                    "on reserves the space for the notice"
                );
            }
            onboarding.history_resolved(false);
            assert!(
                !onboarding.is_visible(),
                "a late history result cannot cover a notice"
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !notice.is_mapped() && std::time::Instant::now() < deadline {
                while main.pending() {
                    main.iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(notice.is_mapped());
            assert!(
                !onboarding.widget().is_mapped(),
                "no overlap even in the narrowest pane"
            );
            let bounds = notice.compute_bounds(&overlay).expect("notice bounds");
            assert!(bounds.x() >= 0.0 && bounds.width() <= overlay.width() as f32);

            let second = gtk::Label::new(Some("Another inline notice"));
            document.append(&second);
            onboarding.observe_inline_notice(second.upcast_ref());
            notice.set_visible(false);
            assert!(
                !onboarding.is_visible(),
                "the second visible notice still owns priority"
            );
            second.set_visible(false);
            assert!(onboarding.is_visible());
            onboarding.set_surface_suspended(true);
            second.set_visible(true);
            second.set_visible(false);
            assert!(
                !onboarding.is_visible(),
                "notice changes cannot end alternate-screen suspension"
            );
            onboarding.set_surface_suspended(false);
            assert!(onboarding.is_visible());

            second.set_visible(true);
            document.remove(&second);
            assert!(
                onboarding.is_visible(),
                "removing the last visible notice restores the hint"
            );
            onboarding.observe_inline_notice(second.upcast_ref());
            assert_eq!(
                onboarding.inner.inline_notices.borrow().len(),
                2,
                "detached observations are ignored"
            );
            let other_document = gtk::Box::new(gtk::Orientation::Vertical, 0);
            other_document.append(&second);
            assert!(
                onboarding.is_visible(),
                "a card in another pane cannot suppress this hint"
            );
            second.set_visible(false);
            second.set_visible(true);
            assert!(onboarding.is_visible());
            other_document.remove(&second);
            document.append(&second);
            onboarding.observe_inline_notice(second.upcast_ref());
            assert!(!onboarding.is_visible());
            assert_eq!(
                onboarding.inner.inline_notices.borrow().len(),
                2,
                "remounting reuses observers"
            );

            onboarding.human_input_observed();
            notice.set_visible(false);
            second.set_visible(false);
            assert!(
                !onboarding.is_visible(),
                "first input dismisses even while guidance is suppressed"
            );
            onboarding.history_resolved(false);
            assert_eq!(onboarding.phase(), BlockOnboardingPhase::Dismissed);
            assert!(!onboarding.is_visible());

            let notice_weak = notice.downgrade();
            document.remove(&notice);
            drop(notice);
            assert!(
                notice_weak.upgrade().is_none(),
                "observation never retains a notice"
            );
            onboarding.observe_inline_notice(second.upcast_ref());
            assert_eq!(
                onboarding.inner.inline_notices.borrow().len(),
                1,
                "dead observations are pruned"
            );
            let inner_weak = Rc::downgrade(&onboarding.inner);
            drop(onboarding);
            assert!(
                inner_weak.upgrade().is_none(),
                "signal callbacks never retain the pane owner"
            );
            second.set_visible(true); // Safe after its observation owner is gone.
            window.close();
        }

        // Completion is a second, independent permanent-dismissal boundary.
        let overlay = gtk::Overlay::new();
        let document = gtk::Box::new(gtk::Orientation::Vertical, 0);
        overlay.set_child(Some(&document));
        let onboarding = BlockOnboarding::attach(&overlay, true);
        let notice = gtk::Label::new(Some("Companion"));
        document.append(&notice);
        onboarding.observe_inline_notice(notice.upcast_ref());
        onboarding.history_resolved(false);
        assert!(!onboarding.is_visible());
        onboarding.finished_block_observed();
        document.remove(&notice);
        onboarding.history_resolved(false);
        assert_eq!(onboarding.phase(), BlockOnboardingPhase::Dismissed);
        assert!(
            !onboarding.is_visible(),
            "a finished command never replays onboarding after notice removal"
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
        let input_handle = onboarding.clone();
        input_handle.human_input_observed();
        assert_eq!(onboarding.phase(), BlockOnboardingPhase::Dismissed);
        assert!(!card.is_visible());
        onboarding.history_resolved(false);
        assert!(
            !card.is_visible(),
            "accepted input permanently retires guidance before CommandEnd"
        );

        // A separate pane still covers the completion-driven path.
        let completion_overlay = gtk::Overlay::new();
        completion_overlay.set_child(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
        let onboarding = BlockOnboarding::attach(&completion_overlay, true);
        onboarding.history_resolved(false);
        let card = onboarding.widget();
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
