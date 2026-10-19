// Exercise the production metadata walker with a realistic large snapshot.
#[test]
fn empty_query_browse_does_not_spend_output_budget_for_command_rows() {
    use super::{metadata_filter_hits_with_budget, CrossBlockSearchScope, FindScanBudget};
    use crate::block_view::{
        BackendRecordRef, BlockFilters, CompletedCommandRecord, ZoneOutputSnapshot,
    };
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    let records: Vec<_> = (1..=2)
        .map(|id| CompletedCommandRecord {
            id,
            cmd: format!("echo {id}"),
            exit_code: Some(0),
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            is_background: false,
            completion_provenance: crate::block_view::CompletionProvenance::ShellReported,
            command_source: crate::block_view::CommandTextSource::Screen,
            start_mark_seen: true,
        })
        .collect();
    let snapshot = ZoneOutputSnapshot {
        plain: "large output row\n".repeat(4096),
        truncated: false,
    };
    for scope in [
        CrossBlockSearchScope::All,
        CrossBlockSearchScope::Command,
        CrossBlockSearchScope::Output,
    ] {
        let mut budget = FindScanBudget {
            remaining_bytes: 64,
            started: Instant::now(),
            time_limit: Duration::from_secs(5),
        };
        let (hits, resume) = metadata_filter_hits_with_budget(
            records.iter().map(|record| BackendRecordRef::Metadata {
                record,
                snapshot: Some(&snapshot),
            }),
            scope,
            500,
            &BlockFilters::default(),
            &HashSet::new(),
            &mut budget,
            0,
        );
        if scope == CrossBlockSearchScope::Output {
            assert_eq!(hits.len(), 1);
            assert!(hits[0].is_output);
            assert!(
                resume.is_some(),
                "output previews still spend their byte budget"
            );
        } else {
            assert_eq!(
                hits.iter().map(|hit| hit.block_id).collect::<Vec<_>>(),
                [1, 2]
            );
            assert!(hits.iter().all(|hit| !hit.is_output));
            assert!(
                resume.is_none(),
                "large unneeded outputs cannot starve command browsing"
            );
            assert_eq!(budget.remaining_bytes(), 52);
        }
    }
}
