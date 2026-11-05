//! Pure form adaptation. Execution, evidence and persistence stay in the host.
use jterm_core::agent_workspace::{WorkspaceMission, WorkspaceMissionRepository};
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
}
