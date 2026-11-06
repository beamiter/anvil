//! Pure form adaptation. Execution, evidence and persistence stay in the host.
use jterm_core::agent_workspace::{
    WorkspaceCommand, WorkspaceMission, WorkspaceMissionAttention, WorkspaceMissionRepository,
    WorkspaceResultReport,
};
use std::path::PathBuf;

/// An explicit editable list, not repository discovery or a filesystem mutation.
pub(crate) fn parse_roots(text: &str) -> Result<Vec<(String, PathBuf)>, String> {
    let mut roots = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (name, path) = line
            .split_once('=')
            .ok_or("Use owner/repository = /absolute/worktree/path on each line")?;
        let repository = WorkspaceMissionRepository::new(name.trim(), path.trim())?;
        repository.validate()?;
        if roots
            .iter()
            .any(|(name, _)| name == &repository.identity.full_name)
        {
            return Err("Each repository must occur only once".into());
        }
        roots.push((repository.identity.full_name, repository.path));
    }
    if roots.is_empty() {
        return Err("Enter at least one repository and its exact worktree path".into());
    }
    Ok(roots)
}

pub(crate) fn update_draft(
    mut mission: WorkspaceMission,
    title: &str,
    goal: &str,
    environment: &str,
    roots: &str,
) -> Result<WorkspaceMission, String> {
    let roots = parse_roots(roots)?;
    let mut repositories = Vec::new();
    for (name, path) in roots {
        // A changed worktree loses its old candidate, task and pin bindings.
        // The host independently enforces the acceptance/evidence boundary.
        let repository = mission
            .repositories
            .iter()
            .find(|repository| repository.identity.full_name == name);
        repositories.push(match repository {
            Some(repository) if repository.path == path => repository.clone(),
            Some(repository) => {
                let mut moved = WorkspaceMissionRepository::new(&name, path)?;
                // A different checkout invalidates evidence, not the user's
                // delivery target or required validation contract.
                moved.target_ref = repository.target_ref.clone();
                moved.required_checks = repository.required_checks.clone();
                moved
            }
            None => WorkspaceMissionRepository::new(&name, path)?,
        });
    }
    mission.title = title.into();
    mission.goal = goal.into();
    mission.required_environment = environment.into();
    mission.repositories = repositories;
    mission.validate()?;
    Ok(mission)
}

/// Only an acknowledgement of this form's exact contents can advance its CAS
/// base. A refresh of another view's edit must leave the old draft revision.
pub(crate) fn confirmed_submission(current: &WorkspaceMission, attempt: &WorkspaceMission) -> bool {
    current.id == attempt.id
        && current.revision > attempt.revision
        && current.title == attempt.title
        && current.goal == attempt.goal
        && current.required_environment == attempt.required_environment
        && current.preset == attempt.preset
        && current.repositories == attempt.repositories
}

pub(crate) fn draft_matches_selected(
    mission: &WorkspaceMission,
    title: &str,
    goal: &str,
    environment: &str,
    roots: &str,
) -> bool {
    update_draft(mission.clone(), title, goal, environment, roots)
        .is_ok_and(|draft| draft == *mission)
}

/// Advance after a host-owned binding/candidate update without reusing stale
/// repository metadata. Concurrent changes to editable form fields still retain
/// the old base and produce a conflict. Typed widget contents are never replaced.
pub(crate) fn can_advance_draft_base(
    base: &WorkspaceMission,
    current: &WorkspaceMission,
    submitted: Option<&WorkspaceMission>,
) -> bool {
    if current.id != base.id || current.revision <= base.revision {
        return false;
    }
    submitted.is_some_and(|attempt| confirmed_submission(current, attempt))
        || (current.title == base.title
            && current.goal == base.goal
            && current.required_environment == base.required_environment
            && current.preset == base.preset
            && current.repositories.len() == base.repositories.len()
            && current
                .repositories
                .iter()
                .zip(&base.repositories)
                .all(|(a, b)| {
                    a.identity == b.identity
                        && a.path == b.path
                        && a.target_ref == b.target_ref
                        && a.required_checks == b.required_checks
                }))
}

pub(crate) fn validate_new_environment(environment: &str) -> Result<(), String> {
    if environment == "local" {
        Ok(())
    } else {
        Err("New local plans require environment 'local'. Set it to local to create, then edit and save the selected plan to use a different acceptance environment.".into())
    }
}

/// A queue entry is an ephemeral projection, not authority. Stable identity
/// alone is insufficient after a refresh, plan edit or receipt replacement.
pub(crate) fn attention_is_current(
    entry: &WorkspaceMissionAttention,
    queue: &[WorkspaceMissionAttention],
    busy: bool,
) -> bool {
    !busy && queue.iter().any(|current| current == entry)
}

/// Request only read-only evidence for an unambiguous linked task. The current
/// host may expose a different run; that reply must be rejected independently.
pub(crate) fn receipt_command(
    entry: &WorkspaceMissionAttention,
    queue: &[WorkspaceMissionAttention],
    busy: bool,
) -> Option<WorkspaceCommand> {
    if !attention_is_current(entry, queue, busy) || entry.receipt_id.is_none() {
        return None;
    }
    let task = entry.task.as_ref()?;
    task.validate().ok()?;
    Some(WorkspaceCommand::InspectResult {
        id: task.conversation.clone(),
    })
}

pub(crate) fn receipt_is_current(
    entry: &WorkspaceMissionAttention,
    queue: &[WorkspaceMissionAttention],
    busy: bool,
    report: &WorkspaceResultReport,
) -> bool {
    receipt_command(entry, queue, busy).is_some() && entry.matches_result(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mission_roots_require_exact_unique_absolute_worktrees() {
        assert!(parse_roots("beamiter/anvil = relative").is_err());
        assert!(parse_roots("beamiter/anvil=/tmp/a\nbeamiter/anvil=/tmp/b").is_err());
        assert!(parse_roots("").is_err());
        assert_eq!(
            parse_roots("beamiter/anvil = /tmp/work tree").unwrap()[0].1,
            PathBuf::from("/tmp/work tree")
        );
    }

    #[test]
    fn editing_a_worktree_drops_previous_candidate_bindings() {
        let mut mission = WorkspaceMission::new("Delivery", "Acceptance");
        let mut repository = WorkspaceMissionRepository::new("beamiter/anvil", "/tmp/old").unwrap();
        repository.baseline = "c".repeat(40);
        repository.candidate = Some("a".repeat(40));
        repository.required_core_pin = Some("b".repeat(40));
        repository.target_ref = "refs/heads/release".into();
        repository.required_checks = vec!["unit".into(), "security".into(), "gtk-display".into()];
        let task_id = jterm_core::agent_task::TaskId::new();
        repository
            .tasks
            .push(jterm_core::agent_workspace::WorkspaceMissionTaskRef {
                conversation: jterm_core::agent_workspace::ConversationId(format!(
                    "jterm:task:v1:[\"host\",\"namespace\",\"{task_id}\"]"
                )),
                task_id,
                run_id: "previous-worktree-run".into(),
                source_sequence: 4,
            });
        repository.validate().unwrap();
        mission.repositories.push(repository);
        let changed = update_draft(
            mission,
            "Delivery",
            "Acceptance",
            "local",
            "beamiter/anvil = /tmp/new",
        )
        .unwrap();
        assert!(changed.repositories[0].candidate.is_none());
        assert!(changed.repositories[0].required_core_pin.is_none());
        assert!(changed.repositories[0].baseline.is_empty());
        assert!(changed.repositories[0].tasks.is_empty());
        assert_eq!(changed.repositories[0].target_ref, "refs/heads/release");
        assert_eq!(
            changed.repositories[0].required_checks,
            ["unit", "security", "gtk-display"]
        );
    }
    #[test]
    fn edits_preserve_the_form_revision_for_conflict_detection() {
        let mut mission = WorkspaceMission::new("Delivery", "Acceptance");
        mission.revision = 7;
        mission
            .repositories
            .push(WorkspaceMissionRepository::new("beamiter/anvil", "/tmp/repo").unwrap());
        let edited = update_draft(
            mission,
            "Changed",
            "New acceptance",
            "local",
            "beamiter/anvil = /tmp/repo",
        )
        .unwrap();
        assert_eq!(edited.revision, 7);
    }

    #[test]
    fn only_the_matching_save_acknowledgement_advances_the_draft() {
        let mut attempt = WorkspaceMission::new("Delivery", "Acceptance");
        attempt.revision = 7;
        let mut saved = attempt.clone();
        assert!(!confirmed_submission(&saved, &attempt));
        saved.revision = 8;
        assert!(confirmed_submission(&saved, &attempt));
        saved.goal = "Another editor's goal".into();
        assert!(!confirmed_submission(&saved, &attempt));
    }
    #[test]
    fn delayed_create_and_binding_updates_advance_without_losing_new_edits() {
        let mut base = WorkspaceMission::new("Delivery", "Acceptance");
        base.repositories
            .push(WorkspaceMissionRepository::new("beamiter/anvil", "/tmp/repo").unwrap());
        let mut saved = base.clone();
        saved.revision = 1;
        assert!(can_advance_draft_base(&base, &saved, None));
        base = saved.clone();
        saved.revision = 2;
        saved.repositories[0].candidate = Some("a".repeat(40));
        assert!(can_advance_draft_base(&base, &saved, None));
        let edited = update_draft(
            saved,
            "User typed while freezing",
            "Acceptance",
            "local",
            "beamiter/anvil=/tmp/repo",
        )
        .unwrap();
        assert_eq!(edited.revision, 2);
        assert_eq!(edited.title, "User typed while freezing");
        assert_eq!(
            edited.repositories[0].candidate.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn another_editors_form_change_cannot_advance_the_old_draft() {
        let mut base = WorkspaceMission::new("Delivery", "Acceptance");
        base.revision = 1;
        let mut saved = base.clone();
        saved.revision = 2;
        saved.goal = "Another editor's acceptance".into();
        assert!(!can_advance_draft_base(&base, &saved, None));
        assert!(can_advance_draft_base(
            &base,
            &saved,
            Some(&WorkspaceMission {
                revision: 1,
                ..saved.clone()
            })
        ));
    }

    #[test]
    fn creating_a_local_plan_never_silently_changes_the_environment() {
        assert!(validate_new_environment("local").is_ok());
        assert!(validate_new_environment("linux-wayland").is_err());
    }

    #[test]
    fn candidate_adoption_requires_the_live_form_to_match_the_saved_plan() {
        let mut mission = WorkspaceMission::new("Delivery", "Acceptance");
        mission
            .repositories
            .push(WorkspaceMissionRepository::new("beamiter/anvil", "/tmp/repo").unwrap());
        assert!(draft_matches_selected(
            &mission,
            "Delivery",
            "Acceptance",
            "local",
            "beamiter/anvil=/tmp/repo"
        ));
        for (title, goal, environment, roots) in [
            (
                "New draft",
                "Acceptance",
                "local",
                "beamiter/anvil=/tmp/repo",
            ),
            (
                "Delivery",
                "New acceptance",
                "local",
                "beamiter/anvil=/tmp/repo",
            ),
            (
                "Delivery",
                "Acceptance",
                "other",
                "beamiter/anvil=/tmp/repo",
            ),
            (
                "Delivery",
                "Acceptance",
                "local",
                "beamiter/anvil=/tmp/other",
            ),
            ("Delivery", "Acceptance", "local", "unfinished path"),
        ] {
            assert!(!draft_matches_selected(
                &mission,
                title,
                goal,
                environment,
                roots
            ));
        }
    }

    #[test]
    fn a_changed_preset_is_not_an_exact_save_acknowledgement() {
        let attempted = WorkspaceMission::new("Delivery", "Acceptance");
        let mut current = attempted.clone();
        current.revision += 1;
        current.preset = jterm_core::agent_workspace::WorkspaceMissionPreset::FourTerminals;
        assert!(!confirmed_submission(&current, &attempted));
    }
    fn receipt_fixture() -> (WorkspaceMissionAttention, WorkspaceResultReport) {
        use jterm_core::agent_workspace::{
            ConversationId, WorkspaceMissionAttentionKind, WorkspaceMissionTaskRef,
        };
        let task_id = jterm_core::agent_task::TaskId::new();
        let conversation = ConversationId(format!(
            "jterm:task:v1:[\"host\",\"namespace\",\"{task_id}\"]"
        ));
        let entry = WorkspaceMissionAttention {
            id: "stable-attention-id".into(),
            mission_id: "mission".into(),
            revision: 4,
            mission_title: "Delivery".into(),
            repository: Some(
                WorkspaceMissionRepository::new("beamiter/anvil", "/tmp/anvil")
                    .unwrap()
                    .identity,
            ),
            candidate: Some("a".repeat(40)),
            kind: WorkspaceMissionAttentionKind::ValidationFailed,
            check_name: Some("gtk-display".into()),
            environment: Some("linux-x11".into()),
            receipt_id: Some("workspace:run-a:12".into()),
            task: Some(WorkspaceMissionTaskRef {
                conversation: conversation.clone(),
                task_id,
                run_id: "run-a".into(),
                source_sequence: 10,
            }),
            detail: "A native validation failed".into(),
        };
        let report = WorkspaceResultReport {
            conversation,
            task_id,
            run_id: "run-a".into(),
            source_sequence: 12,
            baseline: None,
            current: None,
            validation: None,
            validation_current: false,
            validation_status: "Failed".into(),
            validation_output: "exact captured output".into(),
            validation_output_truncated: false,
            error: None,
        };
        (entry, report)
    }

    #[test]
    fn receipt_navigation_only_requests_read_only_exact_task_evidence() {
        let (entry, report) = receipt_fixture();
        let queue = [entry.clone()];
        assert!(matches!(receipt_command(&entry, &queue, false),
            Some(WorkspaceCommand::InspectResult { id }) if id == report.conversation));
        assert!(receipt_is_current(&entry, &queue, false, &report));
        // Historical validation may be useful even though current worktree
        // evidence is unavailable; this never promotes a Mission to passed.
        assert!(!report.validation_current);
    }

    #[test]
    fn stale_callback_is_not_authorized_by_a_stable_attention_id() {
        let (entry, report) = receipt_fixture();
        let mut variants = Vec::new();
        let mut changed = entry.clone();
        changed.revision += 1;
        variants.push(changed);
        let mut changed = entry.clone();
        changed.candidate = Some("b".repeat(40));
        variants.push(changed);
        let mut changed = entry.clone();
        changed.environment = Some("linux-wayland".into());
        variants.push(changed);
        let mut changed = entry.clone();
        changed.receipt_id = Some("workspace:run-a:13".into());
        variants.push(changed);
        let mut changed = entry.clone();
        changed.task.as_mut().unwrap().run_id = "run-b".into();
        variants.push(changed);
        for changed in variants {
            assert_eq!(changed.id, entry.id);
            assert!(receipt_command(&entry, std::slice::from_ref(&changed), false).is_none());
            assert!(!receipt_is_current(&entry, &[changed], false, &report));
        }
    }

    #[test]
    fn refresh_removal_or_pending_writes_invalidate_receipt_callbacks() {
        let (entry, report) = receipt_fixture();
        assert!(receipt_command(&entry, &[], false).is_none());
        assert!(!receipt_is_current(&entry, &[], false, &report));
        assert!(receipt_command(&entry, std::slice::from_ref(&entry), true).is_none());
        assert!(!receipt_is_current(
            &entry,
            std::slice::from_ref(&entry),
            true,
            &report
        ));
    }

    #[test]
    fn reused_task_or_newer_receipt_never_substitutes_output() {
        let (entry, report) = receipt_fixture();
        let mut variants = Vec::new();
        let mut changed = report.clone();
        changed.conversation.0.push_str("-other-host");
        variants.push(changed);
        let mut changed = report.clone();
        changed.task_id = jterm_core::agent_task::TaskId::new();
        variants.push(changed);
        let mut changed = report.clone();
        changed.run_id = "run-b".into();
        variants.push(changed);
        let mut changed = report.clone();
        changed.source_sequence = 13;
        variants.push(changed);
        let mut changed = report.clone();
        changed.source_sequence = 11;
        variants.push(changed);
        for changed in variants {
            assert!(!receipt_is_current(
                &entry,
                std::slice::from_ref(&entry),
                false,
                &changed
            ));
        }
    }

    #[test]
    fn absent_ambiguous_and_invalid_task_bindings_remain_unavailable() {
        let (entry, report) = receipt_fixture();
        let mut variants = Vec::new();
        let mut changed = entry.clone();
        changed.task = None;
        variants.push(changed);
        let mut changed = entry.clone();
        changed.receipt_id = None;
        variants.push(changed);
        let mut changed = entry.clone();
        changed.task.as_mut().unwrap().conversation.0 = "invalid".into();
        variants.push(changed);
        for changed in variants {
            assert!(receipt_command(&changed, std::slice::from_ref(&changed), false).is_none());
            assert!(!receipt_is_current(
                &changed,
                std::slice::from_ref(&changed),
                false,
                &report
            ));
        }
    }
}
