//! Exact selection planning for a deterministic batch fold/unfold action.

use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldSelectionResult {
    Changed(usize),
    NoChange,
    Empty,
    Stale,
    Unavailable,
}

impl FoldSelectionResult {
    pub(crate) fn notice(self, collapsed: bool) -> String {
        match self {
            Self::Changed(count) => format!(
                "{} output in {count} selected block{}.",
                if collapsed { "Collapsed" } else { "Expanded" },
                if count == 1 { "" } else { "s" },
            ),
            Self::NoChange => "No selected output needed changing.".into(),
            Self::Empty => "Select one or more finished blocks first.".into(),
            Self::Stale => "The block selection changed; select the retained blocks again.".into(),
            Self::Unavailable => "Selected output folding is available in a Block or Chat pane outside full-screen applications.".into(),
        }
    }
}

/// Capture the entire selected batch in terminal order before any widget is
/// changed. A vanished or duplicate selected identity rejects the whole batch;
/// it must never fold the newest block or a replacement at the same index.
pub(super) fn selected_indices(
    ordered_ids: impl IntoIterator<Item = u64>,
    selected: &HashSet<u64>,
) -> Result<Vec<usize>, FoldSelectionResult> {
    if selected.is_empty() {
        return Err(FoldSelectionResult::Empty);
    }
    let mut found = HashSet::with_capacity(selected.len());
    let mut indices = Vec::with_capacity(selected.len());
    for (index, id) in ordered_ids.into_iter().enumerate() {
        if selected.contains(&id) {
            if !found.insert(id) {
                return Err(FoldSelectionResult::Stale);
            }
            indices.push(index);
        }
    }
    if found.len() != selected.len() {
        return Err(FoldSelectionResult::Stale);
    }
    Ok(indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_disjoint_selection_keeps_terminal_order() {
        let selected = HashSet::from([90, 30, 70]);
        assert_eq!(
            selected_indices([10, 30, 50, 70, 90], &selected),
            Ok(vec![1, 3, 4])
        );
    }

    #[test]
    fn empty_selection_has_no_newest_block_fallback() {
        assert_eq!(
            selected_indices([10, 20], &HashSet::new()),
            Err(FoldSelectionResult::Empty)
        );
    }

    #[test]
    fn retention_or_duplicate_identity_rejects_the_whole_batch() {
        let selected = HashSet::from([10, 30]);
        assert_eq!(
            selected_indices([20, 30, 40], &selected),
            Err(FoldSelectionResult::Stale)
        );
        assert_eq!(
            selected_indices([10, 20, 30, 30], &selected),
            Err(FoldSelectionResult::Stale)
        );
        assert_eq!(
            selected_indices([], &selected),
            Err(FoldSelectionResult::Stale)
        );
    }
}
