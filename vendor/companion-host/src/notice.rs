//! Window-owned proactive notices (negotiated `CognitionNoticeV1`).
//!
//! A notice is display data from a background cognitive turn that already
//! passed the coordinator's commit check. It executes nothing; an offer in it
//! is a label, and acting on it still takes the user's own explicit request.
//! Notices are kept only for the context they were generated for: a project
//! or pane change, lock, or disconnect clears them.
use companion_protocol::{valid_notice, ContextRef, Notice, Reply, Request};
use std::collections::VecDeque;

pub const MAX_NOTICES: usize = 4;

#[derive(Default)]
pub struct NoticeBoard {
    items: VecDeque<Notice>,
    acknowledged: VecDeque<String>,
    context: Option<ContextRef>,
    revision: u64,
}

/// Same host, and the same explicit project; without a project, the same
/// pane and task. Revision changes inside one project keep the notice.
fn fits(n: &Notice, c: Option<&ContextRef>) -> bool {
    let Some(c) = c else { return false };
    n.context.host_id == c.host_id
        && match (&n.context.project_id, &c.project_id) {
            (Some(a), Some(b)) => a == b,
            (None, None) => n.context.pane_id == c.pane_id && n.context.task_id == c.task_id,
            _ => false,
        }
}

impl NoticeBoard {
    /// Newest last.
    pub fn notices(&self) -> impl Iterator<Item = &Notice> {
        self.items.iter()
    }
    pub fn latest(&self) -> Option<&Notice> {
        self.items.back()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// Changes whenever the visible set changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Consumes notice replies. Invalid or out-of-context notices are dropped.
    pub fn observe(&mut self, reply: &Reply) -> bool {
        match reply {
            Reply::CognitionNotice { notice } => {
                if valid_notice(notice)
                    && fits(notice, self.context.as_ref())
                    && !self.items.iter().any(|n| n.notice_id == notice.notice_id)
                {
                    if self.items.len() >= MAX_NOTICES {
                        self.items.pop_front();
                    }
                    self.items.push_back(notice.clone());
                    self.changed();
                }
                true
            }
            Reply::CognitionNoticeWithdrawn { .. } => {
                self.clear();
                true
            }
            _ => false,
        }
    }

    pub fn context_changed(&mut self, context: &ContextRef) {
        self.context = Some(context.clone());
        let before = self.items.len();
        self.items.retain(|n| fits(n, Some(context)));
        if self.items.len() != before {
            self.changed();
        }
    }

    fn clear(&mut self) {
        if !self.items.is_empty() {
            self.items.clear();
            self.changed();
        }
    }

    pub fn disconnected(&mut self) {
        self.clear();
        self.acknowledged.clear();
    }

    /// Call after the notice was actually drawn; acknowledges each id once.
    pub fn displayed(&mut self, notice_id: &str) -> Option<Request> {
        if !self.items.iter().any(|n| n.notice_id == notice_id)
            || self.acknowledged.iter().any(|id| id == notice_id)
        {
            return None;
        }
        if self.acknowledged.len() >= 32 {
            self.acknowledged.pop_front();
        }
        self.acknowledged.push_back(notice_id.into());
        Some(Request::CognitionNoticeDisplayed {
            notice_id: notice_id.into(),
        })
    }

    /// The user closed it. The coordinator keeps that cause quiet for longer.
    pub fn dismiss(&mut self, notice_id: &str) -> Option<Request> {
        let at = self.items.iter().position(|n| n.notice_id == notice_id)?;
        self.items.remove(at);
        self.changed();
        Some(Request::CognitionNoticeDismissed {
            notice_id: notice_id.into(),
        })
    }
}

/// First line of the notice text, for an indicator or tooltip.
pub fn headline(n: &Notice) -> &str {
    n.text.lines().next().unwrap_or("").trim()
}

/// Toolkit-neutral card text: what, the unexecuted offer, and why.
pub fn card_text(n: &Notice) -> String {
    let mut out = format!("Eva notice · {}\n{}", n.task_ref, n.text.trim());
    if let Some(offer) = &n.offer {
        out.push_str("\nOffer: ");
        out.push_str(offer.trim());
    }
    out.push_str("\nWhy: ");
    out.push_str(n.explanation.trim());
    if !n.evidence_refs.is_empty() {
        out.push_str("\nEvidence: ");
        out.push_str(&n.evidence_refs.join(", "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(project: Option<&str>, pane: &str, revision: u64) -> ContextRef {
        ContextRef {
            host_id: "anvil-1".into(),
            pane_id: pane.into(),
            task_id: Some("build".into()),
            project_id: project.map(str::to_owned),
            revision,
        }
    }
    fn notice(id: &str, c: ContextRef) -> Reply {
        Reply::CognitionNotice {
            notice: Notice {
                notice_id: id.into(),
                context: c,
                task_ref: "task:build".into(),
                mode: "report_status".into(),
                text: "build 第 3 次运行失败了。".into(),
                explanation: "收到失败回执。".into(),
                evidence_refs: vec!["evt:task:build:3:failed".into()],
                offer: None,
            },
        }
    }

    #[test]
    fn notices_stay_with_their_project_and_clear_on_change_or_withdrawal() {
        let mut b = NoticeBoard::default();
        // Unknown context: nothing is shown.
        assert!(b.observe(&notice("n1", context(Some("alpha"), "p", 1))));
        assert!(b.is_empty());
        b.context_changed(&context(Some("alpha"), "p", 1));
        b.observe(&notice("n1", context(Some("alpha"), "p", 1)));
        b.observe(&notice("n1", context(Some("alpha"), "p", 1)));
        assert_eq!(b.notices().count(), 1);
        // Another pane of the same project keeps it; another project drops it.
        b.context_changed(&context(Some("alpha"), "p2", 2));
        assert_eq!(b.notices().count(), 1);
        b.observe(&notice("other", context(Some("beta"), "p2", 2)));
        assert_eq!(b.notices().count(), 1);
        let r = b.revision();
        b.context_changed(&context(Some("beta"), "p2", 3));
        assert!(b.is_empty());
        assert_ne!(b.revision(), r);
        b.context_changed(&context(Some("alpha"), "p", 4));
        b.observe(&notice("n2", context(Some("alpha"), "p", 4)));
        assert!(b.observe(&Reply::CognitionNoticeWithdrawn {
            reason: "desktop_locked_or_unknown".into()
        }));
        assert!(b.is_empty());
    }

    #[test]
    fn invalid_notices_are_dropped_and_the_board_is_bounded() {
        let mut b = NoticeBoard::default();
        b.context_changed(&context(Some("alpha"), "p", 1));
        let Reply::CognitionNotice { notice: mut bad } =
            notice("bad", context(Some("alpha"), "p", 1))
        else {
            unreachable!()
        };
        bad.text = "\u{1b}]52;c;AAAA\u{7}".into();
        b.observe(&Reply::CognitionNotice { notice: bad });
        assert!(b.is_empty());
        for i in 0..6 {
            b.observe(&notice(&format!("n{i}"), context(Some("alpha"), "p", 1)));
        }
        let ids: Vec<_> = b.notices().map(|n| n.notice_id.as_str()).collect();
        assert_eq!(ids, ["n2", "n3", "n4", "n5"]);
        assert!(!b.observe(&Reply::Ack));
    }

    #[test]
    fn card_text_says_what_why_and_that_offers_are_not_run() {
        let Reply::CognitionNotice { notice: mut n } = notice("n", context(None, "p", 1)) else {
            unreachable!()
        };
        n.text = "第一行\n第二行".into();
        n.offer = Some("交给 Agent 检查 · task:build · 检查失败原因（尚未执行）".into());
        assert_eq!(headline(&n), "第一行");
        let text = card_text(&n);
        assert!(text.starts_with("Eva notice · task:build\n第一行\n第二行"));
        assert!(text.contains("\nOffer: 交给 Agent 检查"));
        assert!(text.contains("\nWhy: 收到失败回执。"));
        assert!(text.ends_with("Evidence: evt:task:build:3:failed"));
    }

    #[test]
    fn display_is_acknowledged_once_and_dismiss_removes() {
        let mut b = NoticeBoard::default();
        b.context_changed(&context(Some("alpha"), "p", 1));
        b.observe(&notice("n1", context(Some("alpha"), "p", 1)));
        assert_eq!(
            b.displayed("n1"),
            Some(Request::CognitionNoticeDisplayed {
                notice_id: "n1".into()
            })
        );
        assert_eq!(b.displayed("n1"), None);
        assert_eq!(b.displayed("missing"), None);
        assert_eq!(
            b.dismiss("n1"),
            Some(Request::CognitionNoticeDismissed {
                notice_id: "n1".into()
            })
        );
        assert!(b.is_empty());
        assert_eq!(b.dismiss("n1"), None);
    }
}
