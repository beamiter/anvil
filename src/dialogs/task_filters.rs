//! View-only constraints. Original repository paths never become display keys.
use crate::agent_task::{AgentProvider, DashboardSnapshot, DashboardView, TaskId, TaskStatus};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct TaskFilters {
    pub provider: Option<AgentProvider>,
    pub workspace: Option<PathBuf>,
    pub status: Option<TaskStatus>,
    /// Show only hidden (archived) tasks, which can be restored. The normal
    /// views never include them.
    pub hidden: bool,
}

pub(super) const STATES: [TaskStatus; 9] = [
    TaskStatus::Created,
    TaskStatus::Starting,
    TaskStatus::Working,
    TaskStatus::WaitingForApproval,
    TaskStatus::WaitingForHuman,
    TaskStatus::ReadyForReview,
    TaskStatus::Completed,
    TaskStatus::Failed,
    TaskStatus::Cancelled,
];

impl TaskFilters {
    pub fn project(
        &self,
        source: &DashboardSnapshot,
        workspaces: &HashMap<TaskId, PathBuf>,
        query: &str,
        view: DashboardView,
    ) -> DashboardSnapshot {
        let mut result = self.scope(source, workspaces, query);
        result
            .rows
            .retain(|row| self.status.is_none_or(|status| row.status == status));
        result.apply_view(view, None, Default::default());
        result
    }

    /// Provider/project/query scope for both badges and the final list. State
    /// and quick-view narrowing happen afterward without a second search.
    pub fn scope(
        &self,
        source: &DashboardSnapshot,
        workspaces: &HashMap<TaskId, PathBuf>,
        query: &str,
    ) -> DashboardSnapshot {
        DashboardSnapshot::filter_iter_unsorted(
            source.rows.iter().filter(|row| {
                (row.status == TaskStatus::Archived) == self.hidden
                    && self
                        .provider
                        .is_none_or(|provider| row.provider == provider)
                    && self
                        .workspace
                        .as_ref()
                        .is_none_or(|path| workspaces.get(&row.id) == Some(path))
            }),
            query,
            DashboardView::All,
        )
    }

    /// Original, lossless project identity of a currently visible selection.
    /// Hidden selections and missing workspace facts never broaden the scope.
    pub fn selected_project(
        visible: &DashboardSnapshot,
        selected: Option<TaskId>,
        workspaces: &HashMap<TaskId, PathBuf>,
    ) -> Option<PathBuf> {
        let id = selected?;
        visible.selected_index(Some(id))?;
        workspaces.get(&id).cloned()
    }

    pub fn workspace_choices(&self, workspaces: &HashMap<TaskId, PathBuf>) -> Vec<PathBuf> {
        let mut choices = workspaces.values().cloned().collect::<Vec<_>>();
        if let Some(selected) = &self.workspace {
            choices.push(selected.clone());
        }
        choices.sort();
        choices.dedup();
        choices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::{
        DashboardActivity, DashboardRow, TaskRuntimeKind, TaskValidationStatus,
    };

    fn row(provider: AgentProvider, status: TaskStatus) -> DashboardRow {
        DashboardRow {
            id: TaskId::new(),
            title: "Same task".into(),
            provider,
            status,
            runtime_kind: TaskRuntimeKind::Terminal,
            branch: "same".into(),
            repo_label: "same displayed workspace".into(),
            activity: DashboardActivity::default(),
            validation_status: TaskValidationStatus::NotRun,
            validation_attempt: 0,
            needs_attention: matches!(status, TaskStatus::Failed | TaskStatus::WaitingForApproval),
            status_detail: None,
            updated_at_ms: 42,
        }
    }

    #[test]
    fn hidden_scope_is_exactly_the_archived_tasks_and_keeps_other_filters() {
        let rows = vec![
            row(AgentProvider::Codex, TaskStatus::Archived),
            row(AgentProvider::Claude, TaskStatus::Archived),
            row(AgentProvider::Codex, TaskStatus::Failed),
        ];
        let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let source = DashboardSnapshot {
            rows,
            ..Default::default()
        };
        let workspaces = HashMap::new();
        let visible = TaskFilters::default().scope(&source, &workspaces, "");
        assert_eq!(
            visible.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [ids[2]]
        );
        let hidden = TaskFilters {
            hidden: true,
            ..Default::default()
        };
        let mut scope = hidden.scope(&source, &workspaces, "");
        scope.sort_rows(Default::default());
        assert_eq!(scope.rows.len(), 2);
        assert_eq!(scope.counts.archived, 2);
        assert!(scope
            .rows
            .iter()
            .all(|row| row.status == TaskStatus::Archived));
        let codex = TaskFilters {
            provider: Some(AgentProvider::Codex),
            ..hidden.clone()
        };
        assert_eq!(
            codex
                .scope(&source, &workspaces, "")
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [ids[0]]
        );
        assert!(
            hidden
                .scope(&source, &workspaces, "status:archived")
                .rows
                .len()
                == 2
        );
    }

    #[test]
    fn exact_workspace_provider_state_intersection_keeps_counts_before_state_view() {
        let rows = vec![
            row(AgentProvider::Codex, TaskStatus::Working),
            row(AgentProvider::Codex, TaskStatus::Failed),
            row(AgentProvider::Claude, TaskStatus::Failed),
            row(AgentProvider::Codex, TaskStatus::WaitingForApproval),
        ];
        let paths = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                (
                    row.id,
                    PathBuf::from(if index == 3 {
                        "/other/same"
                    } else {
                        "/one/same"
                    }),
                )
            })
            .collect();
        let source = DashboardSnapshot {
            rows,
            ..Default::default()
        };
        let filters = TaskFilters {
            provider: Some(AgentProvider::Codex),
            workspace: Some("/one/same".into()),
            status: Some(TaskStatus::Failed),
            hidden: false,
        };
        let result = filters.project(&source, &paths, "same", DashboardView::All);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].id, source.rows[1].id);
        assert_eq!(result.counts.total, 2);
        assert_eq!(result.counts.attention, 1);
        assert_eq!(result.selected_index(Some(source.rows[0].id)), None);
        assert!(filters
            .project(&source, &paths, "same", DashboardView::Running)
            .rows
            .is_empty());
        assert_eq!(
            filters
                .project(&source, &paths, "missing", DashboardView::All)
                .counts
                .total,
            0
        );
        assert_eq!(
            source.rows.len(),
            4,
            "projection never mutates domain-derived input"
        );
    }

    #[test]
    fn workspace_identity_is_lossless_and_missing_selection_never_widens_scope() {
        use std::os::unix::ffi::OsStringExt;
        let a = PathBuf::from(std::ffi::OsString::from_vec(b"/repo/\xff".to_vec()));
        let b = PathBuf::from(std::ffi::OsString::from_vec(b"/repo/\xfe".to_vec()));
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        let rows = vec![
            row(AgentProvider::Codex, TaskStatus::Working),
            row(AgentProvider::Codex, TaskStatus::Working),
        ];
        let paths = HashMap::from([(rows[0].id, a.clone()), (rows[1].id, b.clone())]);
        let source = DashboardSnapshot {
            rows,
            ..Default::default()
        };
        let filters = TaskFilters {
            workspace: Some(a.clone()),
            ..Default::default()
        };
        assert_eq!(
            filters
                .project(&source, &paths, "", DashboardView::All)
                .rows[0]
                .id,
            source.rows[0].id
        );
        assert_eq!(filters.workspace_choices(&paths).len(), 2);
        assert_eq!(filters.workspace_choices(&HashMap::new()), vec![a]);
        assert!(filters
            .project(&source, &HashMap::new(), "", DashboardView::All)
            .rows
            .is_empty());
    }

    #[test]
    fn selected_project_uses_original_identity_and_refuses_hidden_or_missing_sources() {
        use std::os::unix::ffi::OsStringExt;
        let a = PathBuf::from(std::ffi::OsString::from_vec(b"/repo/\xff".to_vec()));
        let b = PathBuf::from(std::ffi::OsString::from_vec(b"/repo/\xfe".to_vec()));
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        let rows = vec![
            row(AgentProvider::Codex, TaskStatus::Working),
            row(AgentProvider::Codex, TaskStatus::Failed),
        ];
        let selected = rows[0].id;
        let paths = HashMap::from([(selected, a.clone()), (rows[1].id, b)]);
        let mut visible = DashboardSnapshot {
            rows,
            ..Default::default()
        };
        assert_eq!(
            TaskFilters::selected_project(&visible, Some(selected), &paths),
            Some(a)
        );
        assert_eq!(TaskFilters::selected_project(&visible, None, &paths), None);
        assert_eq!(
            TaskFilters::selected_project(&visible, Some(selected), &HashMap::new()),
            None
        );
        visible.rows.remove(0);
        assert_eq!(
            TaskFilters::selected_project(&visible, Some(selected), &paths),
            None
        );
    }

    #[test]
    fn thousand_tasks_have_stable_identity_order_and_attention_counts() {
        let mut source = DashboardSnapshot {
            rows: (0..1000)
                .map(|index| {
                    row(
                        AgentProvider::ALL[index % 4],
                        if index % 5 == 0 {
                            TaskStatus::Failed
                        } else {
                            TaskStatus::Working
                        },
                    )
                })
                .collect(),
            ..Default::default()
        };
        let paths = source
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.id, PathBuf::from(format!("/repo/{}", index % 3))))
            .collect();
        let filters = TaskFilters::default();
        let expected = filters.project(&source, &paths, "", DashboardView::All);
        assert_eq!(expected.counts.total, 1000);
        assert_eq!(expected.counts.attention, 200);
        source.rows.reverse();
        assert_eq!(
            filters.project(&source, &paths, "", DashboardView::All),
            expected
        );
        source.rows[0].status = TaskStatus::Archived;
        assert_eq!(
            filters
                .project(&source, &paths, "", DashboardView::All)
                .counts
                .total,
            999
        );
    }
}
