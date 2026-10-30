//! Pure management authority guards, shared by native callbacks and offscreen tests.
use super::host::{ContextRef, MemoryScope};
pub fn same_origin(a: &ContextRef, b: &ContextRef) -> bool {
    a.host_id == b.host_id
        && a.pane_id == b.pane_id
        && a.task_id == b.task_id
        && a.project_id == b.project_id
}
pub fn accepts_review(
    current: Option<&ContextRef>,
    requested: Option<&(ContextRef, MemoryScope)>,
    origin: &ContextRef,
    scope: &MemoryScope,
) -> bool {
    current == Some(origin) && requested.is_some_and(|(c, s)| c == origin && s == scope)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> ContextRef {
        ContextRef {
            host_id: "host".into(),
            pane_id: "pane".into(),
            task_id: Some("active".into()),
            project_id: None,
            revision: 3,
        }
    }
    #[test]
    fn archived_management_never_requires_adopting_target() {
        let c = context();
        let target = MemoryScope::Task("archived-other-host".into());
        let request = (c.clone(), target.clone());
        assert!(accepts_review(Some(&c), Some(&request), &c, &target));
        assert_eq!(c.task_id.as_deref(), Some("active"));
        assert!(!accepts_review(
            Some(&c),
            Some(&request),
            &c,
            &MemoryScope::Global
        ));
    }
    #[test]
    fn changed_pane_project_revision_and_host_invalidate_review() {
        let c = context();
        let scope = MemoryScope::Project("archived".into());
        let request = (c.clone(), scope.clone());
        for changed in 0..5 {
            let mut other = c.clone();
            match changed {
                0 => other.pane_id = "other".into(),
                1 => other.project_id = Some("new".into()),
                2 => other.revision += 1,
                3 => other.host_id = "other".into(),
                _ => other.task_id = None,
            };
            assert!(!accepts_review(Some(&other), Some(&request), &c, &scope));
            if changed != 2 {
                assert!(!same_origin(&other, &c));
            }
        }
        assert!(!accepts_review(None, Some(&request), &c, &scope));
        assert!(!accepts_review(Some(&c), None, &c, &scope));
    }
}
