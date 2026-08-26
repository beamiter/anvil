use std::cell::{Cell, RefCell};
use std::collections::HashSet;

/// Pane-local bookmark truth for completed records.
///
/// Widgets and Unified chrome are projections of this store. Keeping the ids
/// private makes every effective membership change advance `revision`, which is
/// what an open Block Search dialog observes without cloning record text.
#[derive(Default)]
pub(super) struct BookmarkState {
    ids: RefCell<HashSet<u64>>,
    revision: Cell<u64>,
}

impl BookmarkState {
    pub(super) fn contains(&self, id: u64) -> bool {
        self.ids.borrow().contains(&id)
    }

    pub(super) fn snapshot(&self) -> HashSet<u64> {
        self.ids.borrow().clone()
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision.get()
    }

    fn changed(&self) {
        self.revision.set(
            self.revision
                .get()
                .checked_add(1)
                .expect("bookmark revision exhausted"),
        );
    }

    /// Set one live record's bookmark and return its authoritative state.
    ///
    /// A stale UI action is rejected. Defensive removal of a pre-existing ghost
    /// still counts as a real set mutation and therefore advances the revision.
    pub(super) fn set_existing(
        &self,
        id: u64,
        bookmarked: bool,
        record_exists: bool,
    ) -> Option<bool> {
        if !record_exists {
            self.remove(id);
            return None;
        }
        let changed = {
            let mut ids = self.ids.borrow_mut();
            if bookmarked {
                ids.insert(id)
            } else {
                ids.remove(&id)
            }
        };
        if changed {
            self.changed();
        }
        Some(bookmarked)
    }

    pub(super) fn toggle_existing(&self, id: u64, record_exists: bool) -> Option<bool> {
        let bookmarked = !self.contains(id);
        self.set_existing(id, bookmarked, record_exists)
    }

    pub(super) fn remove(&self, id: u64) -> bool {
        let changed = self.ids.borrow_mut().remove(&id);
        if changed {
            self.changed();
        }
        changed
    }

    /// Prune records retired by a backend, advancing the revision once for the
    /// whole backend transaction rather than once per id.
    pub(super) fn remove_ids(&self, ids: impl IntoIterator<Item = u64>) -> usize {
        let mut removed = 0usize;
        {
            let mut bookmarked = self.ids.borrow_mut();
            for id in ids {
                removed = removed.saturating_add(usize::from(bookmarked.remove(&id)));
            }
        }
        if removed != 0 {
            self.changed();
        }
        removed
    }

    pub(super) fn clear(&self) -> usize {
        let removed = self.ids.borrow().len();
        if removed == 0 {
            return 0;
        }
        self.ids.borrow_mut().clear();
        self.changed();
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::BookmarkState;

    #[test]
    fn every_effective_membership_change_advances_revision() {
        let state = BookmarkState::default();
        assert_eq!(state.set_existing(7, true, true), Some(true));
        assert_eq!(state.revision(), 1);
        assert_eq!(state.set_existing(7, true, true), Some(true));
        assert_eq!(state.revision(), 1, "idempotent set is not a change");
        assert_eq!(state.toggle_existing(7, true), Some(false));
        assert_eq!(state.revision(), 2);
        assert_eq!(state.toggle_existing(7, true), Some(true));
        assert_eq!(state.revision(), 3);
        assert_eq!(state.remove_ids([7, 99]), 1);
        assert_eq!(state.revision(), 4);
        assert_eq!(state.clear(), 0);
        assert_eq!(state.revision(), 4);
    }

    #[test]
    fn stale_record_actions_cannot_create_ghost_bookmarks() {
        let state = BookmarkState::default();
        assert_eq!(state.toggle_existing(41, false), None);
        assert!(!state.contains(41));
        assert_eq!(state.revision(), 0);

        assert_eq!(state.set_existing(41, true, true), Some(true));
        assert_eq!(state.set_existing(41, false, false), None);
        assert!(!state.contains(41));
        assert_eq!(state.revision(), 2, "defensive ghost cleanup is observable");
    }

    #[test]
    fn retirement_prunes_only_named_ids_in_one_revision() {
        let state = BookmarkState::default();
        for id in [1, 2, 3] {
            state.set_existing(id, true, true);
        }
        let before = state.revision();
        assert_eq!(state.remove_ids([1, 3, 9]), 2);
        assert_eq!(state.revision(), before + 1);
        assert_eq!(state.snapshot(), std::collections::HashSet::from([2]));
    }
}
