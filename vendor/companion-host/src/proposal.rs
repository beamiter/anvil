//! Window-owned proposals and goals (negotiated `CognitionProposalV1`).
//!
//! A proposal is something Eva suggests: watch a task, or remember (or
//! correct) one memory entry. It changes nothing until the user approves the
//! exact text shown here; approval carries the proposal's digest and its exact
//! origin context, so a pane or revision change makes it stale. Goals listed
//! here only let Eva notice and tell; stopping one is always available.
use companion_protocol::{
    valid_goal, valid_proposal, CognitionProposal, ContextRef, GoalView, ProposalChange, Reply,
    Request, TaskStatus,
};
use std::collections::VecDeque;

pub const MAX_PROPOSALS: usize = 4;

#[derive(Default)]
pub struct ProposalBoard {
    items: VecDeque<CognitionProposal>,
    goals: Vec<GoalView>,
    context: Option<ContextRef>,
    status: String,
    revision: u64,
}

fn status_text(status: &str) -> String {
    match status {
        "goal_active" => "Eva is watching that task and will tell you once when it finishes.",
        "handoff_sent" => {
            "Opening the task composer. Nothing starts until you create the task there."
        }
        "delegation_not_negotiated" => "This terminal cannot open an agent task composer.",
        "remembered" => "Saved to memory after your approval.",
        "declined" => "Declined. Nothing was saved or started.",
        "goal_cancelled" => "Stopped watching that task.",
        "goal_already_active" => "Eva is already watching that task.",
        "proposal_stale" => "The pane, task or memory changed. Nothing was saved.",
        "memory_paused" => "Memory is paused. Nothing was saved.",
        "memory_conflict" => "That memory changed meanwhile. Nothing was saved.",
        "no_current_task" => "This pane has no task to watch.",
        "proposal_not_negotiated" => "Update the Eva coordinator to use proposals.",
        other => return format!("Not done: {other}"),
    }
    .into()
}

impl ProposalBoard {
    /// Newest last.
    pub fn proposals(&self) -> impl Iterator<Item = &CognitionProposal> {
        self.items.iter()
    }
    pub fn latest(&self) -> Option<&CognitionProposal> {
        self.items.back()
    }
    pub fn goals(&self) -> &[GoalView] {
        &self.goals
    }
    /// Last outcome, in words.
    pub fn status(&self) -> &str {
        &self.status
    }
    /// Changes whenever proposals, goals or the status change.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn observe(&mut self, reply: &Reply) -> bool {
        match reply {
            Reply::CognitionProposal { proposal } => {
                if valid_proposal(proposal)
                    && self.context.as_ref() == Some(&proposal.context)
                    && !self
                        .items
                        .iter()
                        .any(|p| p.proposal_id == proposal.proposal_id)
                {
                    if self.items.len() >= MAX_PROPOSALS {
                        self.items.pop_front();
                    }
                    self.items.push_back(proposal.clone());
                    self.changed();
                }
                true
            }
            Reply::CognitionProposalResult {
                proposal_id,
                status,
            } => {
                if let Some(id) = proposal_id {
                    self.items.retain(|p| &p.proposal_id != id);
                }
                self.status = status_text(status);
                self.changed();
                true
            }
            Reply::Goals { context, goals } => {
                if self.context.as_ref() == Some(context) {
                    self.goals = goals
                        .iter()
                        .filter(|g| valid_goal(g))
                        .take(companion_protocol::MAX_GOALS_LISTED)
                        .cloned()
                        .collect();
                    self.changed();
                }
                true
            }
            _ => false,
        }
    }

    /// Approval needs the exact origin context: any context change drops
    /// proposals and the goal list until the coordinator sends a fresh one.
    pub fn context_changed(&mut self, context: &ContextRef) {
        if self.context.as_ref() == Some(context) {
            return;
        }
        self.context = Some(context.clone());
        self.items.clear();
        self.goals.clear();
        self.changed();
    }

    pub fn disconnected(&mut self) {
        if !self.items.is_empty() || !self.goals.is_empty() {
            self.items.clear();
            self.goals.clear();
            self.changed();
        }
    }

    /// The approval request for the exact proposal shown; it leaves the list.
    pub fn approve(&mut self, proposal_id: &str) -> Option<Request> {
        let at = self
            .items
            .iter()
            .position(|p| p.proposal_id == proposal_id)?;
        let p = self.items.remove(at)?;
        self.changed();
        Some(Request::CognitionProposalApprove {
            proposal_id: p.proposal_id,
            context: p.context,
            digest: p.digest,
        })
    }

    pub fn decline(&mut self, proposal_id: &str) -> Option<Request> {
        let at = self
            .items
            .iter()
            .position(|p| p.proposal_id == proposal_id)?;
        self.items.remove(at);
        self.changed();
        Some(Request::CognitionProposalDecline {
            proposal_id: proposal_id.into(),
        })
    }

    pub fn local_status(&mut self, status: &str) {
        self.status = status_text(status);
        self.changed();
    }
}

fn statuses(s: &[TaskStatus]) -> String {
    s.iter()
        .map(|s| match s {
            TaskStatus::Failed => "fails",
            TaskStatus::NeedsReview => "needs review",
            TaskStatus::Succeeded => "succeeds",
            TaskStatus::Running => "runs",
            TaskStatus::Unknown => "is unknown",
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Toolkit-neutral text for one proposal: exactly what approval would do.
pub fn card_text(p: &CognitionProposal) -> String {
    let what = match &p.change {
        ProposalChange::WatchTask {
            task_ref,
            notify_on,
            ..
        } => format!(
            "Eva suggests: watch {task_ref} and tell you once when it {}.\nIt only notices and tells; nothing is run.",
            statuses(notify_on)
        ),
        ProposalChange::AgentTask { title, goal } => format!(
            "Eva suggests handing a task to an agent: {title}\n{}\nApproving opens the task composer prefilled; you choose the repository, agent and validation and create the task there. Eva then tells you when it is ready for review or fails.",
            goal.trim()
        ),
        ProposalChange::Remember {
            scope,
            value,
            memory_kind,
            supersedes,
            source_refs,
            ..
        } => {
            let mut t = format!(
                "Eva suggests remembering ({memory_kind}, {scope:?}):\n{}",
                value.trim()
            );
            if let Some(id) = supersedes {
                t.push_str(&format!("\nThis replaces memory entry #{id}."));
            }
            if !source_refs.is_empty() {
                t.push_str(&format!("\nSources: {}", source_refs.join(", ")));
            }
            t.push_str("\nNothing is saved unless you approve.");
            t
        }
    };
    format!("{what}\nWhy: {}", p.explanation.trim())
}

pub fn goal_line(g: &GoalView) -> String {
    format!(
        "Watching {} · tells you when it {}",
        g.task_ref,
        statuses(&g.notify_on)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(revision: u64) -> ContextRef {
        ContextRef {
            host_id: "anvil-1".into(),
            pane_id: "p".into(),
            task_id: Some("build".into()),
            project_id: Some("alpha".into()),
            revision,
        }
    }
    fn proposal(id: &str, c: ContextRef) -> Reply {
        Reply::CognitionProposal {
            proposal: CognitionProposal {
                proposal_id: id.into(),
                digest: "c".repeat(64),
                context: c,
                change: ProposalChange::WatchTask {
                    task_ref: "task:build".into(),
                    notify_on: vec![TaskStatus::Failed, TaskStatus::Succeeded],
                    expires_at_ms: 1,
                },
                explanation: "你说完成后告诉你。".into(),
            },
        }
    }
    fn goal(id: &str) -> GoalView {
        GoalView {
            goal_id: id.into(),
            task_ref: "task:build".into(),
            notify_on: vec![TaskStatus::Succeeded],
            expires_at_ms: 1,
            origin: "explicit".into(),
        }
    }

    #[test]
    fn proposals_need_the_exact_current_context_and_approval_echoes_digest() {
        let mut b = ProposalBoard::default();
        assert!(b.observe(&proposal("p1", ctx(1))));
        assert!(b.latest().is_none(), "no context yet");
        b.context_changed(&ctx(1));
        b.observe(&proposal("p1", ctx(1)));
        b.observe(&proposal("p1", ctx(1)));
        b.observe(&proposal("old", ctx(0)));
        assert_eq!(b.proposals().count(), 1);
        assert_eq!(
            b.approve("p1"),
            Some(Request::CognitionProposalApprove {
                proposal_id: "p1".into(),
                context: ctx(1),
                digest: "c".repeat(64)
            })
        );
        assert!(b.approve("p1").is_none());
        b.observe(&proposal("p2", ctx(1)));
        // A revision change makes it stale: approval would need the old context.
        b.context_changed(&ctx(2));
        assert!(b.latest().is_none());
        assert!(b.decline("p2").is_none());
    }

    #[test]
    fn results_goals_and_text() {
        let mut b = ProposalBoard::default();
        b.context_changed(&ctx(1));
        b.observe(&proposal("p1", ctx(1)));
        let text = card_text(b.latest().unwrap());
        assert!(text.contains("watch task:build and tell you once when it fails / succeeds"));
        assert!(text.contains("nothing is run"));
        b.observe(&Reply::CognitionProposalResult {
            proposal_id: Some("p1".into()),
            status: "goal_active".into(),
        });
        assert!(b.latest().is_none());
        assert!(b.status().starts_with("Eva is watching"));
        b.observe(&Reply::Goals {
            context: ctx(9),
            goals: vec![goal("goal-1")],
        });
        assert!(
            b.goals().is_empty(),
            "goals for another context are ignored"
        );
        b.observe(&Reply::Goals {
            context: ctx(1),
            goals: vec![goal("goal-1"), goal("bad id")],
        });
        assert_eq!(b.goals().len(), 1);
        assert_eq!(
            goal_line(&b.goals()[0]),
            "Watching task:build · tells you when it succeeds"
        );
        b.disconnected();
        assert!(b.goals().is_empty());
        assert!(!b.observe(&Reply::Ack));
    }

    #[test]
    fn invalid_proposals_are_dropped_and_the_board_is_bounded() {
        let mut b = ProposalBoard::default();
        b.context_changed(&ctx(1));
        let Reply::CognitionProposal { proposal: mut bad } = proposal("bad", ctx(1)) else {
            unreachable!()
        };
        bad.explanation = "\u{1b}]0;x\u{7}".into();
        b.observe(&Reply::CognitionProposal { proposal: bad });
        assert!(b.latest().is_none());
        for i in 0..6 {
            b.observe(&proposal(&format!("p{i}"), ctx(1)));
        }
        let ids: Vec<_> = b.proposals().map(|p| p.proposal_id.as_str()).collect();
        assert_eq!(ids, ["p2", "p3", "p4", "p5"]);
    }
}
