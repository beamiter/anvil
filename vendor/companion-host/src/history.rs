//! A bounded, window-owned history view. Nothing is fetched until an explicit
//! request; locking, disconnecting or changing context clears what is shown.
use companion_protocol::{
    history::{self, Entry, Scope, Text},
    ContextRef, Reply, Request,
};

pub const SCOPES: [Scope; 4] = [Scope::Context, Scope::Task, Scope::Project, Scope::Voice];

pub fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Context => "This pane",
        Scope::Task => "Current task",
        Scope::Project => "Current project",
        Scope::Voice => "Voice activity (no transcripts)",
    }
}

struct Pending {
    id: u64,
    context: ContextRef,
    scope: Scope,
    older: bool,
}

#[derive(Default)]
pub struct HistoryView {
    scope: Scope,
    entries: Vec<Entry>,
    next: Option<String>,
    shown_for: Option<ContextRef>,
    pending: Option<Pending>,
    request_id: u64,
    status: String,
    capped: bool,
    revision: u64,
}

fn same_origin(a: &ContextRef, b: &ContextRef) -> bool {
    a.host_id == b.host_id
        && a.pane_id == b.pane_id
        && a.task_id == b.task_id
        && a.project_id == b.project_id
}

/// The coordinator already filters; the host re-checks so a faulty or foreign
/// page can never show another project's turns.
fn belongs(entry: &Entry, context: &ContextRef, scope: Scope) -> bool {
    match (scope, &entry.source) {
        (Scope::Voice, None) => true,
        (Scope::Context, Some(source)) => same_origin(source, context),
        (Scope::Task, Some(source)) => {
            context.task_id.is_some()
                && source.task_id == context.task_id
                && source.project_id == context.project_id
        }
        (Scope::Project, Some(source)) => {
            context.project_id.is_some() && source.project_id == context.project_id
        }
        _ => false,
    }
}

impl HistoryView {
    pub fn scope(&self) -> Scope {
        self.scope
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn status(&self) -> &str {
        &self.status
    }
    /// Changes whenever anything a viewer renders changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn can_load_older(&self) -> bool {
        self.next.is_some() && self.pending.is_none() && !self.capped
    }
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    fn clear_with(&mut self, status: &str) {
        let changed = !self.entries.is_empty()
            || self.pending.is_some()
            || self.next.is_some()
            || self.status != status;
        self.entries.clear();
        self.next = None;
        self.shown_for = None;
        self.pending = None;
        self.capped = false;
        self.status = status.into();
        if changed {
            self.touch();
        }
    }
    pub fn clear(&mut self) {
        self.clear_with("");
    }
    pub fn set_scope(&mut self, scope: Scope) {
        if scope != self.scope {
            self.scope = scope;
            self.clear_with("Scope changed. Load history to view it.");
        }
    }
    /// Call with the window's current context on every poll.
    pub fn context_changed(&mut self, current: Option<&ContextRef>) {
        let shown = self
            .pending
            .as_ref()
            .map(|p| &p.context)
            .or(self.shown_for.as_ref());
        if shown.is_some_and(|shown| current.is_none_or(|c| !same_origin(shown, c))) {
            self.clear_with("The pane or task changed. Load history again.");
        }
    }
    pub fn disconnected(&mut self) {
        if !self.entries.is_empty() || self.pending.is_some() {
            self.clear_with("Coordinator disconnected. History cleared.");
        }
    }
    /// Builds one explicit page request for `context`, which must be the
    /// window's registered current context.
    pub fn request(&mut self, context: &ContextRef, older: bool) -> Result<Request, &'static str> {
        if self.pending.is_some() {
            return Err("History is already loading.");
        }
        if !self.scope.available(context) {
            return Err(match self.scope {
                Scope::Task => "This pane has no current task. Choose another scope.",
                _ => "This pane has no current project. Choose another scope.",
            });
        }
        let before = if older {
            if !self.can_load_older()
                || self
                    .shown_for
                    .as_ref()
                    .is_none_or(|shown| !same_origin(shown, context))
            {
                return Err("No older history to load.");
            }
            self.next.clone()
        } else {
            None
        };
        self.request_id = self
            .request_id
            .checked_add(1)
            .ok_or("History unavailable.")?;
        self.pending = Some(Pending {
            id: self.request_id,
            context: context.clone(),
            scope: self.scope,
            older,
        });
        self.status = "Loading history…".into();
        self.touch();
        Ok(Request::DialogueHistory {
            request_id: self.request_id,
            context: context.clone(),
            scope: self.scope,
            before,
        })
    }
    /// Returns true if the reply belonged to history.
    pub fn observe(&mut self, reply: &Reply) -> bool {
        match reply {
            Reply::DialogueHistoryWithdrawn { .. } => {
                self.clear_with("History hidden: the session is locked or its state is unknown.");
                true
            }
            Reply::DialogueHistory {
                request_id,
                context,
                scope,
                status,
                entries,
                next_before,
            } => {
                let Some(pending) = self
                    .pending
                    .as_ref()
                    .filter(|p| p.id == *request_id && &p.context == context && p.scope == *scope)
                else {
                    // A late page for an older request, context or scope.
                    return true;
                };
                let older = pending.older;
                let context = pending.context.clone();
                let scope = pending.scope;
                self.pending = None;
                if status != "ok" {
                    let message = match status.as_str() {
                        "scope_unavailable" => "This scope is not available for the current pane.",
                        "stale_context" => "The pane or task changed. Load history again.",
                        "history_cursor_stale" => {
                            "Older history changed. Load history again from the newest turn."
                        }
                        "desktop_locked_or_unknown" => {
                            "History is hidden while the session is locked or unknown."
                        }
                        "history_unavailable" => "Local history is unavailable.",
                        _ => "This coordinator does not provide history.",
                    };
                    self.clear_with(message);
                    return true;
                }
                let mut cursors = std::collections::HashSet::new();
                if entries.len() > history::MAX_PAGE
                    || next_before
                        .as_ref()
                        .is_some_and(|c| !history::cursor_valid(c))
                    || entries.iter().any(|e| {
                        !e.valid() || !belongs(e, &context, scope) || !cursors.insert(&e.cursor)
                    })
                {
                    self.clear_with("History reply rejected.");
                    return true;
                }
                if !older {
                    self.entries.clear();
                    self.capped = false;
                }
                for entry in entries {
                    if self.entries.len() >= history::MAX_VIEW {
                        self.capped = true;
                        break;
                    }
                    if self.entries.iter().all(|old| old.cursor != entry.cursor) {
                        self.entries.push(entry.clone());
                    }
                }
                self.capped |= self.entries.len() >= history::MAX_VIEW && next_before.is_some();
                self.next = next_before.clone();
                self.shown_for = Some(context);
                let n = self.entries.len();
                let turns = if n == 1 {
                    "1 turn".to_owned()
                } else {
                    format!("{n} turns")
                };
                self.status = match (n, self.capped, self.next.is_some()) {
                    (0, _, _) => "No history in this scope yet.".into(),
                    (_, true, _) => format!(
                        "Showing the newest {turns}. Older history stays with the coordinator."
                    ),
                    (_, false, true) => {
                        format!("Showing {turns}, newest first. Older turns exist.")
                    }
                    (1, false, false) => "Showing the only turn in this scope.".into(),
                    (_, false, false) => format!("Showing all {turns}, newest first."),
                };
                self.touch();
                true
            }
            _ => false,
        }
    }
    /// Plain text for label-based viewers. Static text only: no avatar.
    pub fn text(&self, viewer: Option<&ContextRef>) -> String {
        let mut out = Vec::new();
        for entry in &self.entries {
            out.extend(describe(entry, viewer));
            out.push(String::new());
        }
        out.join("\n").trim_end().to_owned()
    }
}

fn host_label(id: &str) -> String {
    match id.split_once('-') {
        Some((app @ ("forge" | "anvil" | "ember" | "frost"), rest)) => {
            let mut name: Vec<char> = app.chars().collect();
            name[0] = name[0].to_ascii_uppercase();
            format!(
                "{} {}",
                name.into_iter().collect::<String>(),
                rest.chars().take(8).collect::<String>()
            )
        }
        _ => id.chars().take(12).collect(),
    }
}
fn pane_label(id: &str) -> String {
    match id {
        "companion-card" => "Eva card".into(),
        "task-card" => "task card".into(),
        _ => format!("pane {}", id.chars().take(8).collect::<String>()),
    }
}

fn local_time(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return format!("{ms} ms");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// Lines for one entry. Raw counters stay raw; nothing is inferred.
pub fn describe(entry: &Entry, viewer: Option<&ContextRef>) -> Vec<String> {
    use history::Route;
    let route = match entry.route {
        Route::Text => "Text",
        Route::Offline => "Offline",
        Route::Voice => "Voice",
    };
    let source = match &entry.source {
        Some(source) if viewer.is_some_and(|v| same_origin(v, source)) => "this pane".into(),
        Some(source) => {
            let mut label = format!(
                "{} · {}",
                host_label(&source.host_id),
                pane_label(&source.pane_id)
            );
            if let Some(project) = &source.project_id {
                label += &format!(" · project {project}");
            }
            if let Some(task) = &source.task_id {
                label += &format!(" · task {task}");
            }
            label
        }
        None => "Eva voice".into(),
    };
    let mut lines = vec![format!(
        "{} · {route} · {source}",
        local_time(entry.created_at_ms)
    )];
    match entry.text {
        Text::Shown => {
            if let Some(input) = &entry.input {
                lines.push(format!("You: {input}"));
            }
            match &entry.reply {
                Some(reply) => lines.push(format!("Eva: {reply}")),
                None => lines.push("Eva: (no reply text)".into()),
            }
        }
        Text::EarlierSession => {
            lines.push("Text was kept only in memory during an earlier coordinator session.".into())
        }
        Text::NotRecorded => lines.push(if entry.route == Route::Voice {
            "Voice transcripts are not kept.".into()
        } else {
            "No text was recorded for this turn.".into()
        }),
        Text::WithheldOutsideOrigin => lines.push(
            "Text is shown only in its original pane because it used reviewed terminal text."
                .into(),
        ),
        Text::TooLarge => lines.push("Text is too large for the history view.".into()),
    }
    let state = if !entry.finished {
        "in progress".into()
    } else {
        match entry.end {
            Some(history::End::ResponseComplete) => "finished".to_owned(),
            Some(history::End::Interrupted) => "interrupted".into(),
            Some(history::End::RestartedUnknown) => {
                "ended by coordinator restart; outcome unknown".into()
            }
            Some(history::End::ObservationLost) => "observation lost".into(),
            Some(history::End::Error) => "ended with an error".into(),
            Some(_) => "ended".into(),
            None => "finished".into(),
        }
    };
    let mut facts = vec![if entry.generated {
        "reply generated"
    } else {
        "no reply generated"
    }
    .to_owned()];
    if entry.route != Route::Voice {
        facts.push(
            if entry.displayed {
                "a frontend reported showing it"
            } else {
                "not reported as shown"
            }
            .into(),
        );
    }
    facts.push(state);
    lines.push(facts.join(" · "));
    if let (Some(generated), Some(played)) =
        (entry.generated_samples, entry.confirmed_played_samples)
    {
        lines.push(format!(
            "Audio samples: {generated} generated, at least {played} confirmed played."
        ));
    }
    if let Some(r) = entry.receipts {
        lines.push(format!(
            "Tool calls: {}. Operation receipts: {} succeeded, {} partial, {} failed/cancelled, {} unknown. Unsubmitted steps are not implied.",
            r.tool_calls, r.succeeded, r.partial, r.failed_or_cancelled, r.unknown
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use companion_protocol::history::{End, Receipts, Route};
    fn context(pane: &str, project: Option<&str>) -> ContextRef {
        ContextRef {
            host_id: "forge-1234567890".into(),
            pane_id: pane.into(),
            task_id: None,
            project_id: project.map(str::to_owned),
            revision: 3,
        }
    }
    fn entry(n: u8, source: Option<ContextRef>) -> Entry {
        Entry {
            cursor: format!("{n:02x}").repeat(32),
            turn_id: format!("turn-{n}"),
            created_at_ms: 1_700_000_000_000 + n as u64,
            route: if source.is_some() {
                Route::Offline
            } else {
                Route::Voice
            },
            text: if source.is_some() {
                Text::Shown
            } else {
                Text::NotRecorded
            },
            input: source.as_ref().map(|_| format!("question {n}")),
            reply: source.as_ref().map(|_| format!("answer {n}")),
            source,
            generated: true,
            displayed: true,
            interrupted: false,
            finished: true,
            end: Some(End::ResponseComplete),
            recovered_after_restart: false,
            generated_samples: None,
            confirmed_played_samples: None,
            receipts: None,
        }
    }
    fn page(request: &Request, entries: Vec<Entry>, next: Option<String>) -> Reply {
        let Request::DialogueHistory {
            request_id,
            context,
            scope,
            ..
        } = request
        else {
            panic!("history request")
        };
        Reply::DialogueHistory {
            request_id: *request_id,
            context: context.clone(),
            scope: *scope,
            status: "ok".into(),
            entries,
            next_before: next,
        }
    }

    #[test]
    fn nothing_is_requested_or_shown_until_an_explicit_load() {
        let view = HistoryView::default();
        assert!(view.entries().is_empty() && !view.pending());
        assert_eq!(view.text(None), "");
    }

    #[test]
    fn late_pages_for_old_requests_contexts_or_scopes_are_ignored() {
        let a = context("pane-a", Some("p"));
        let mut view = HistoryView::default();
        let first = view.request(&a, false).unwrap();
        assert!(view.request(&a, false).is_err(), "one request at a time");
        view.set_scope(Scope::Project);
        let late = page(&first, vec![entry(1, Some(a.clone()))], None);
        assert!(view.observe(&late));
        assert!(view.entries().is_empty() && !view.pending());
        let second = view.request(&a, false).unwrap();
        assert!(view.observe(&late), "old request id");
        assert!(view.pending());
        let mut wrong_context = page(&second, vec![], None);
        if let Reply::DialogueHistory { context, .. } = &mut wrong_context {
            context.pane_id = "pane-b".into();
        }
        view.observe(&wrong_context);
        assert!(view.pending());
        view.observe(&page(&second, vec![entry(2, Some(a.clone()))], None));
        assert_eq!(view.entries().len(), 1);
        assert!(!view.pending());
        view.observe(&page(&second, vec![entry(3, Some(a.clone()))], None));
        assert_eq!(
            view.entries()[0].turn_id,
            "turn-2",
            "duplicate reply ignored"
        );
    }

    #[test]
    fn context_change_lock_and_disconnect_clear_the_view() {
        let a = context("pane-a", None);
        let mut view = HistoryView::default();
        let request = view.request(&a, false).unwrap();
        view.observe(&page(&request, vec![entry(1, Some(a.clone()))], None));
        let mut revised = a.clone();
        revised.revision = 9;
        view.context_changed(Some(&revised));
        assert_eq!(
            view.entries().len(),
            1,
            "revision alone is the same context"
        );
        view.context_changed(Some(&context("pane-b", None)));
        assert!(view.entries().is_empty());
        let request = view.request(&a, false).unwrap();
        view.context_changed(None);
        assert!(
            !view.pending(),
            "pending request for an old context is dropped"
        );
        view.observe(&page(&request, vec![entry(1, Some(a.clone()))], None));
        assert!(view.entries().is_empty());
        let request = view.request(&a, false).unwrap();
        view.observe(&page(&request, vec![entry(1, Some(a.clone()))], None));
        let before = view.revision();
        assert!(view.observe(&Reply::DialogueHistoryWithdrawn {
            reason: "desktop_locked_or_unknown".into()
        }));
        assert!(view.entries().is_empty() && view.revision() != before);
        assert!(view.status().contains("locked"));
        let request = view.request(&a, false).unwrap();
        view.observe(&page(&request, vec![entry(1, Some(a.clone()))], None));
        view.disconnected();
        assert!(view.entries().is_empty());
    }

    #[test]
    fn foreign_scope_invalid_or_oversized_pages_are_rejected_whole() {
        let a = context("pane-a", Some("project-a"));
        let other = context("pane-b", Some("project-b"));
        let mut view = HistoryView::default();
        view.set_scope(Scope::Project);
        let request = view.request(&a, false).unwrap();
        view.observe(&page(
            &request,
            vec![entry(1, Some(a.clone())), entry(2, Some(other))],
            None,
        ));
        assert!(view.entries().is_empty());
        assert_eq!(view.status(), "History reply rejected.");
        let request = view.request(&a, false).unwrap();
        view.observe(&page(&request, vec![entry(1, None)], None));
        assert!(view.entries().is_empty(), "voice entry outside voice scope");
        let request = view.request(&a, false).unwrap();
        let many = (0..9).map(|n| entry(n, Some(a.clone()))).collect();
        view.observe(&page(&request, many, None));
        assert!(view.entries().is_empty());
        let request = view.request(&a, false).unwrap();
        view.observe(&page(
            &request,
            vec![entry(1, Some(a.clone()))],
            Some("BAD".into()),
        ));
        assert!(view.entries().is_empty());
        let request = view.request(&a, false).unwrap();
        let mut withheld = entry(1, Some(a.clone()));
        withheld.text = Text::WithheldOutsideOrigin;
        view.observe(&page(&request, vec![withheld], None));
        assert!(
            view.entries().is_empty(),
            "withheld text cannot carry a body"
        );
    }

    #[test]
    fn older_pages_append_once_and_stop_at_the_view_bound() {
        let a = context("pane-a", None);
        let mut view = HistoryView::default();
        assert!(view.request(&a, true).is_err(), "no cursor yet");
        let mut n = 0u8;
        let mut request = view.request(&a, false).unwrap();
        loop {
            let entries: Vec<_> = (0..8).map(|i| entry(n + i, Some(a.clone()))).collect();
            let next = entries.last().map(|e| e.cursor.clone());
            view.observe(&page(&request, entries, next));
            n += 8;
            match view.request(&a, true) {
                Ok(next) => request = next,
                Err(_) => break,
            }
        }
        assert_eq!(view.entries().len(), history::MAX_VIEW);
        assert!(!view.can_load_older());
        assert!(view.status().contains("newest 32"));
        let ids: std::collections::HashSet<_> = view.entries().iter().map(|e| &e.cursor).collect();
        assert_eq!(ids.len(), history::MAX_VIEW);
        let reload = view.request(&a, false).unwrap();
        view.observe(&page(&reload, vec![entry(200, Some(a.clone()))], None));
        assert_eq!(view.entries().len(), 1, "a fresh load replaces the view");
    }

    #[test]
    fn task_and_project_scopes_need_identity_and_status_is_explained() {
        let a = context("pane-a", None);
        let mut view = HistoryView::default();
        view.set_scope(Scope::Project);
        assert!(view.request(&a, false).is_err());
        view.set_scope(Scope::Task);
        assert!(view.request(&a, false).is_err());
        view.set_scope(Scope::Voice);
        let request = view.request(&a, false).unwrap();
        let mut locked = page(&request, vec![], None);
        if let Reply::DialogueHistory { status, .. } = &mut locked {
            *status = "desktop_locked_or_unknown".into();
        }
        view.observe(&locked);
        assert!(view.status().contains("locked"));
        assert!(!view.pending());
    }

    #[test]
    fn descriptions_use_raw_counters_and_never_claim_unsubmitted_steps() {
        let mut voice = entry(1, None);
        voice.generated_samples = Some(4800);
        voice.confirmed_played_samples = Some(1200);
        voice.receipts = Some(Receipts {
            tool_calls: 2,
            succeeded: 1,
            partial: 0,
            failed_or_cancelled: 0,
            unknown: 1,
        });
        let text = describe(&voice, None).join("\n");
        assert!(text.contains("4800 generated, at least 1200 confirmed played"));
        assert!(!text.contains(" ms"), "no duration is inferred");
        assert!(text.contains("1 unknown") && text.contains("not implied"));
        assert!(text.contains("Voice transcripts are not kept."));
        let a = context("pane-a", Some("p"));
        let shown = describe(&entry(2, Some(a.clone())), Some(&a)).join("\n");
        assert!(shown.contains("this pane") && shown.contains("You: question 2"));
        let elsewhere = describe(&entry(2, Some(a)), Some(&context("pane-z", None))).join("\n");
        assert!(elsewhere.contains("Forge 12345678 · pane pane-a · project p"));
    }
}
