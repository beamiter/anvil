//! Window-owned agent handoffs (negotiated `CognitionDelegationV1`).
//!
//! An approved handoff only asks this window to open its own task composer
//! prefilled with a title and goal. The user still picks repository, provider
//! and validation and creates the task there; starting the agent and every
//! approval inside it stay with the task runtime. This board then reports what
//! actually happened: composer opened/closed, task created, and the task's own
//! lifecycle and validation. Reports are facts from the task runtime, never
//! commands to it.
use companion_protocol::{
    valid_handoff, AgentHandoff, AgentTaskState, AgentValidation, HandoffOutcome, Reply, Request,
};
use std::collections::{BTreeMap, VecDeque};

pub const MAX_PENDING: usize = 4;
pub const MAX_LINKS: usize = 32;

fn ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Link {
    agent_task_id: Option<String>,
    last: Option<(AgentTaskState, AgentValidation)>,
}

#[derive(Default)]
pub struct HandoffBoard {
    pending: VecDeque<AgentHandoff>,
    links: BTreeMap<String, Link>,
}

impl HandoffBoard {
    pub fn observe(&mut self, reply: &Reply) -> bool {
        let Reply::AgentHandoff { handoff } = reply else {
            return false;
        };
        if valid_handoff(handoff)
            && !self.links.contains_key(&handoff.operation_id)
            && self.links.len() < MAX_LINKS
        {
            if self.pending.len() >= MAX_PENDING {
                if let Some(old) = self.pending.pop_front() {
                    self.links.remove(&old.operation_id);
                }
            }
            self.links
                .insert(handoff.operation_id.clone(), Link::default());
            self.pending.push_back(handoff.clone());
        }
        true
    }

    /// The next handoff for the window to open in its composer.
    pub fn take(&mut self) -> Option<AgentHandoff> {
        self.pending.pop_front()
    }

    fn receipt(&self, operation_id: &str, outcome: HandoffOutcome) -> Option<Request> {
        self.links
            .contains_key(operation_id)
            .then(|| Request::AgentHandoffReceipt {
                operation_id: operation_id.into(),
                outcome,
            })
    }

    pub fn opened(&self, operation_id: &str) -> Option<Request> {
        self.receipt(operation_id, HandoffOutcome::ComposerOpened)
    }

    /// The user closed the composer without creating a task.
    pub fn closed(&mut self, operation_id: &str) -> Option<Request> {
        if self.links.get(operation_id)?.agent_task_id.is_some() {
            return None;
        }
        let r = self.receipt(operation_id, HandoffOutcome::ComposerClosed);
        self.links.remove(operation_id);
        r
    }

    pub fn unavailable(&mut self, operation_id: &str, reason: &str) -> Option<Request> {
        if !ident(reason) {
            return None;
        }
        let r = self.receipt(
            operation_id,
            HandoffOutcome::Unavailable {
                reason: reason.into(),
            },
        );
        self.links.remove(operation_id);
        r
    }

    /// The task runtime registered a task from this handoff (once).
    pub fn created(&mut self, operation_id: &str, agent_task_id: &str) -> Option<Request> {
        if !ident(agent_task_id) {
            return None;
        }
        let link = self.links.get_mut(operation_id)?;
        if link.agent_task_id.is_some() {
            return None;
        }
        link.agent_task_id = Some(agent_task_id.into());
        self.receipt(
            operation_id,
            HandoffOutcome::TaskCreated {
                agent_task_id: agent_task_id.into(),
            },
        )
    }

    /// Report the task's current lifecycle; unchanged states are not resent.
    pub fn status(
        &mut self,
        agent_task_id: &str,
        state: AgentTaskState,
        validation: AgentValidation,
    ) -> Option<Request> {
        let (operation_id, link) = self
            .links
            .iter_mut()
            .find(|(_, l)| l.agent_task_id.as_deref() == Some(agent_task_id))?;
        if link.last == Some((state, validation)) {
            return None;
        }
        link.last = Some((state, validation));
        Some(Request::AgentTaskStatus {
            operation_id: operation_id.clone(),
            agent_task_id: agent_task_id.into(),
            state,
            validation,
        })
    }

    /// Task ids this window reports on.
    pub fn linked_tasks(&self) -> impl Iterator<Item = &str> {
        self.links
            .values()
            .filter_map(|l| l.agent_task_id.as_deref())
    }

    /// After (re)admission: the coordinator may have restarted and marked these
    /// operations unknown, so resend every created task's last known state.
    pub fn reconnected(&self) -> Vec<Request> {
        let mut out = Vec::new();
        for (op, link) in &self.links {
            if let Some(task) = &link.agent_task_id {
                out.push(Request::AgentHandoffReceipt {
                    operation_id: op.clone(),
                    outcome: HandoffOutcome::TaskCreated {
                        agent_task_id: task.clone(),
                    },
                });
                if let Some((state, validation)) = link.last {
                    out.push(Request::AgentTaskStatus {
                        operation_id: op.clone(),
                        agent_task_id: task.clone(),
                        state,
                        validation,
                    });
                }
            }
        }
        out
    }

    /// Unopened handoffs are dropped (the coordinator then sees no receipt);
    /// created tasks stay linked so they are reported after reconnecting.
    pub fn disconnected(&mut self) {
        for h in self.pending.drain(..) {
            self.links.remove(&h.operation_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use companion_protocol::ContextRef;

    fn handoff(op: &str) -> Reply {
        Reply::AgentHandoff {
            handoff: AgentHandoff {
                operation_id: op.into(),
                context: ContextRef {
                    host_id: "anvil-1".into(),
                    pane_id: "p".into(),
                    task_id: Some("build".into()),
                    project_id: Some("alpha".into()),
                    revision: 1,
                },
                title: "检查构建失败".into(),
                goal: "找出失败原因。".into(),
            },
        }
    }

    #[test]
    fn a_handoff_is_opened_created_and_reported_once_per_change() {
        let mut b = HandoffBoard::default();
        assert!(b.observe(&handoff("op-1")));
        b.observe(&handoff("op-1"));
        let h = b.take().unwrap();
        assert!(b.take().is_none(), "duplicates are not reopened");
        assert!(matches!(
            b.opened(&h.operation_id),
            Some(Request::AgentHandoffReceipt { .. })
        ));
        assert!(
            b.status("t-1", AgentTaskState::Working, AgentValidation::NotRun)
                .is_none(),
            "not linked yet"
        );
        assert!(b.created("op-1", "t-1").is_some());
        assert!(b.created("op-1", "t-2").is_none(), "created once");
        assert!(
            b.closed("op-1").is_none(),
            "a created task cannot be 'closed'"
        );
        assert!(b
            .status("t-1", AgentTaskState::Working, AgentValidation::NotRun)
            .is_some());
        assert!(b
            .status("t-1", AgentTaskState::Working, AgentValidation::NotRun)
            .is_none());
        let ready = b.status(
            "t-1",
            AgentTaskState::ReadyForReview,
            AgentValidation::Passed,
        );
        assert_eq!(
            ready,
            Some(Request::AgentTaskStatus {
                operation_id: "op-1".into(),
                agent_task_id: "t-1".into(),
                state: AgentTaskState::ReadyForReview,
                validation: AgentValidation::Passed
            })
        );
        assert_eq!(b.linked_tasks().collect::<Vec<_>>(), ["t-1"]);
        // After reconnecting, the creation and the last state are resent.
        assert_eq!(b.reconnected().len(), 2);
        b.disconnected();
        assert_eq!(b.linked_tasks().count(), 1);
    }

    #[test]
    fn closed_unavailable_invalid_and_bounds() {
        let mut b = HandoffBoard::default();
        b.observe(&handoff("op-1"));
        b.take();
        assert!(b.closed("op-1").is_some());
        assert!(
            b.created("op-1", "t-1").is_none(),
            "closed handoffs cannot create tasks"
        );
        b.observe(&handoff("op-2"));
        let h2 = b.take().unwrap();
        assert!(
            b.unavailable(&h2.operation_id, "agent tasks off").is_none(),
            "reason is an identifier"
        );
        assert!(b
            .unavailable(&h2.operation_id, "agent_tasks_disabled")
            .is_some());
        assert!(
            b.opened("op-2").is_none(),
            "an unavailable handoff is finished"
        );
        let Reply::AgentHandoff { handoff: mut bad } = handoff("op-3") else {
            unreachable!()
        };
        bad.title = "two\nlines".into();
        b.observe(&Reply::AgentHandoff { handoff: bad });
        assert!(b.take().is_none(), "an invalid handoff is never queued");
        for i in 0..6 {
            b.observe(&handoff(&format!("op-x{i}")));
        }
        assert!(b.pending.len() <= MAX_PENDING);
        b.disconnected();
        assert!(b.take().is_none());
        assert!(!b.observe(&Reply::Ack));
    }
}
