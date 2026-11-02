//! Read-only changes/run inspection and explicit dependency queue editing.
//! Readiness is a projection; Start remains in the task's existing controls.
use super::workspace_context_ui::ContextSurface;
use super::workspace_navigation::{format_timestamp_ms, Action};
use super::workspace_review::WorkspaceReview;
use gtk::prelude::*;
use jterm_core::agent_workspace::{
    ConversationId, ConversationKind, MaterialKind, WorkspaceMaterial, WorkspaceOverview,
    WorkspaceQueueRow, WorkspaceResultReport, WorkspaceRow,
};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Clone)]
struct QueueWidgets {
    root: gtk::Box,
    title: gtk::Label,
    detail: gtk::Label,
}

#[derive(Clone)]
pub(crate) struct ReviewSurface {
    pub changes: gtk::ScrolledWindow,
    pub run: gtk::ScrolledWindow,
    pub overview: gtk::Label,
    state: Rc<RefCell<WorkspaceReview>>,
    local_rows: Rc<RefCell<Vec<WorkspaceRow>>>,
    usage: Rc<RefCell<HashMap<ConversationId, Option<String>>>>,
    status: gtk::Label,
    run_status: gtk::Label,
    changes_meta: gtk::Label,
    diff: gtk::TextBuffer,
    validation: gtk::TextBuffer,
    validation_meta: gtk::Label,
    usage_label: gtk::Label,
    refresh: gtk::Button,
    refresh_run: gtk::Button,
    add_diff: gtk::Button,
    add_log: gtk::Button,
    queue_status: gtk::Label,
    queue_add: gtk::Button,
    queue_remove: gtk::Button,
    dependency_search: gtk::SearchEntry,
    dependencies: gtk::Box,
    dependency_widgets: Rc<RefCell<HashMap<ConversationId, gtk::CheckButton>>>,
    dependency_note: gtk::Label,
    queue_rows: gtk::Box,
    queue_widgets: Rc<RefCell<HashMap<ConversationId, QueueWidgets>>>,
    rendering: Rc<Cell<bool>>,
    rendered_report: Rc<RefCell<String>>,
    callback: Rc<dyn Fn(Action)>,
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label
}
fn page() -> (gtk::ScrolledWindow, gtk::Box) {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.add_css_class("workspace-surface");
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    let root = gtk::ScrolledWindow::new();
    root.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    root.set_vexpand(true);
    root.set_child(Some(&body));
    (root, body)
}
fn text_view(body: &gtk::Box) -> gtk::TextBuffer {
    let view = gtk::TextView::new();
    view.add_css_class("workspace-material-editor");
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_top_margin(8);
    view.set_bottom_margin(24);
    view.set_left_margin(8);
    view.set_right_margin(8);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_min_content_height(180);
    scroll.set_max_content_height(480);
    scroll.set_propagate_natural_height(true);
    scroll.set_child(Some(&view));
    body.append(&scroll);
    view.buffer()
}

impl ReviewSurface {
    pub fn new(callback: Rc<dyn Fn(Action)>, context: ContextSurface) -> Self {
        let state = Rc::new(RefCell::new(WorkspaceReview::default()));
        let (changes, changes_body) = page();
        let heading = label("Captured changes");
        heading.add_css_class("title-2");
        changes_body.append(&heading);
        let status = label("Select a task to capture its current worktree evidence.");
        changes_body.append(&status);
        let refresh = gtk::Button::with_label("Refresh captured changes");
        refresh.set_halign(gtk::Align::Start);
        let data = state.clone();
        refresh.connect_clicked(move |_| data.borrow_mut().refresh_result());
        changes_body.append(&refresh);
        let changes_meta = label("");
        changes_meta.set_selectable(true);
        changes_body.append(&changes_meta);
        let add_diff = gtk::Button::with_label("Add captured diff to context");
        add_diff.set_halign(gtk::Align::Start);
        let data = state.clone();
        let ctx = context.clone();
        let open = callback.clone();
        add_diff.connect_clicked(move |_| {
            let data = data.borrow();
            let Some(report) = data
                .report
                .as_ref()
                .filter(|report| Some(&report.conversation) == data.selected())
            else {
                return;
            };
            let Some(material) = result_material(report, false) else {
                return;
            };
            let id = report.conversation.clone();
            drop(data);
            ctx.add_material(id.clone(), material);
            open(Action::InspectContext(id));
        });
        changes_body.append(&add_diff);
        let diff = text_view(&changes_body);
        let (run, run_body) = page();
        let heading = label("Run and validation");
        heading.add_css_class("title-2");
        run_body.append(&heading);
        let run_status = label("");
        run_body.append(&run_status);
        run_body.append(&label("The session host owns accepted work. Hiding this workspace or closing a view does not resend a prompt. Open the task controls to stop, follow up, validate or review; readiness never starts execution automatically."));
        let refresh_run = gtk::Button::with_label("Refresh run evidence");
        refresh_run.set_halign(gtk::Align::Start);
        let data = state.clone();
        refresh_run.connect_clicked(move |_| data.borrow_mut().refresh_result());
        run_body.append(&refresh_run);
        let validation_meta = label("");
        validation_meta.set_selectable(true);
        run_body.append(&validation_meta);
        let usage_label = label("Provider-reported usage unavailable.");
        usage_label.add_css_class("dim-label");
        run_body.append(&usage_label);
        let add_log = gtk::Button::with_label("Add captured validation log to context");
        add_log.set_halign(gtk::Align::Start);
        let data = state.clone();
        let open = callback.clone();
        add_log.connect_clicked(move |_| {
            let data = data.borrow();
            let Some(report) = data
                .report
                .as_ref()
                .filter(|report| Some(&report.conversation) == data.selected())
            else {
                return;
            };
            let Some(material) = result_material(report, true) else {
                return;
            };
            let id = report.conversation.clone();
            drop(data);
            context.add_material(id.clone(), material);
            open(Action::InspectContext(id));
        });
        run_body.append(&add_log);
        let validation = text_view(&run_body);
        let heading = label("Dependency queue");
        heading.add_css_class("title-2");
        run_body.append(&heading);
        run_body.append(&label("Queue intent is shared across windows. Dependencies require accepted task results. Execution location: Local. Start always uses the existing task controls and their preflight checks."));
        let queue_status = label("");
        run_body.append(&queue_status);
        let queue_add = gtk::Button::with_label("Add this task to the queue");
        queue_add.set_halign(gtk::Align::Start);
        let data = state.clone();
        queue_add.connect_clicked(move |_| {
            let id = data.borrow().selected().cloned();
            if let Some(id) = id {
                data.borrow_mut().set_queue(id, Vec::new());
            }
        });
        run_body.append(&queue_add);
        let queue_remove = gtk::Button::with_label("Remove this task from the queue");
        queue_remove.set_halign(gtk::Align::Start);
        let data = state.clone();
        queue_remove.connect_clicked(move |_| {
            let id = data.borrow().selected().cloned();
            if let Some(id) = id {
                data.borrow_mut().remove_queue(id);
            }
        });
        run_body.append(&queue_remove);
        let dependency_search = gtk::SearchEntry::new();
        dependency_search.set_placeholder_text(Some("Find a task dependency"));
        run_body.append(&dependency_search);
        let dependency_note = label("");
        dependency_note.add_css_class("dim-label");
        run_body.append(&dependency_note);
        let dependencies = gtk::Box::new(gtk::Orientation::Vertical, 4);
        run_body.append(&dependencies);
        let heading = label("Shared task queue");
        heading.add_css_class("heading");
        run_body.append(&heading);
        let queue_rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
        run_body.append(&queue_rows);
        let overview = label("");
        overview.add_css_class("dim-label");
        overview.set_margin_start(8);
        overview.set_margin_end(8);
        overview.set_tooltip_text(Some(
            "Observed shared workspace resources; unknown usage is never counted as zero cost.",
        ));
        super::workspace_navigation::wrap_button_labels(&changes_body);
        super::workspace_navigation::wrap_button_labels(&run_body);
        Self {
            changes,
            run,
            overview,
            state,
            local_rows: Rc::new(RefCell::new(Vec::new())),
            usage: Rc::new(RefCell::new(HashMap::new())),
            status,
            run_status,
            changes_meta,
            diff,
            validation,
            validation_meta,
            usage_label,
            refresh,
            refresh_run,
            add_diff,
            add_log,
            queue_status,
            queue_add,
            queue_remove,
            dependency_search,
            dependencies,
            dependency_widgets: Rc::new(RefCell::new(HashMap::new())),
            dependency_note,
            queue_rows,
            queue_widgets: Rc::new(RefCell::new(HashMap::new())),
            rendering: Rc::new(Cell::new(false)),
            rendered_report: Rc::new(RefCell::new(String::new())),
            callback,
        }
    }

    pub fn select(&self, row: &WorkspaceRow) {
        let mut state = self.state.borrow_mut();
        if state.selected() != Some(&row.id) {
            self.queue_add.set_sensitive(false);
            self.queue_remove.set_sensitive(false);
            for (_, button) in self.dependency_widgets.borrow_mut().drain() {
                self.dependencies.remove(&button);
            }
        }
        if row.kind == ConversationKind::Task {
            state.select_task(row.id.clone());
        } else {
            state.clear_task();
        }
        drop(state);
        self.render();
    }
    pub fn set_usage(&self, id: ConversationId, usage: Option<String>) {
        self.usage.borrow_mut().insert(id, usage);
    }
    pub fn row(&self, id: &ConversationId) -> Option<WorkspaceRow> {
        self.state
            .borrow()
            .combined_rows(&self.local_rows.borrow())
            .into_iter()
            .find(|row| &row.id == id)
    }
    pub fn catalog_rows(&self) -> Vec<WorkspaceRow> {
        self.state.borrow().combined_rows(&self.local_rows.borrow())
    }
    pub fn task_indexed(&self, id: &ConversationId) -> bool {
        self.state.borrow().task_indexed(id)
    }
    pub fn registration_notice(&self) -> Option<String> {
        self.state.borrow().notice.clone()
    }
    pub fn clear_selection(&self) {
        self.state.borrow_mut().clear_task();
        self.render();
    }
    pub fn sync(&self, rows: Vec<WorkspaceRow>) {
        let mut state = self.state.borrow_mut();
        // The same client admits new task identities before catalog/queue reads.
        // A receipt, not optimistic local metadata, unlocks writes.
        state.index_tasks(&rows);
        *self.local_rows.borrow_mut() = rows;
        state.start();
        state.poll();
        drop(state);
        self.render();
    }

    fn render(&self) {
        let state = self.state.borrow();
        let rows = state.combined_rows(&self.local_rows.borrow());
        let overview = WorkspaceOverview::from_rows(&rows, state.catalog_complete);
        self.overview.set_text(&format!(
            "{}\n{} tasks · {} accepted · {} need you · {} unconfirmed",
            overview.resource_label(),
            overview.tasks,
            overview.accepted,
            overview.attention_total(),
            overview.unconfirmed
        ));
        let selected = state.selected();
        let row = selected.and_then(|id| rows.iter().find(|row| &row.id == id));
        self.refresh
            .set_sensitive(selected.is_some() && !state.report_loading);
        self.refresh_run
            .set_sensitive(selected.is_some() && !state.report_loading);
        let report = state
            .report
            .as_ref()
            .filter(|report| Some(&report.conversation) == selected);
        let stale = report.is_some_and(|report| {
            row.and_then(|row| row.facts.source_sequence)
                .is_none_or(|sequence| sequence != report.source_sequence)
        });
        let status = if state.report_loading {
            "Capturing read-only worktree evidence…".into()
        } else if let Some(report) = report {
            format!(
                "Run {} · captured sequence {}{}",
                report.run_id,
                report.source_sequence,
                if stale {
                    " · Source state changed or is unavailable; refresh before assessing the current result"
                } else {
                    " · Current observed source sequence"
                }
            )
        } else if let Some(notice) = &state.notice {
            notice.clone()
        } else {
            "No captured result. Open a native task and refresh to inspect its worktree.".into()
        };
        self.status.set_text(&status);
        self.run_status.set_text(&status);
        self.add_diff.set_sensitive(
            report.is_some_and(|report| {
                report
                    .current
                    .as_ref()
                    .is_some_and(|current| !current.diff.is_empty())
            }) && !state.report_loading,
        );
        self.add_log.set_sensitive(
            report.is_some_and(|report| !report.validation_output.is_empty())
                && !state.report_loading,
        );
        let usage = selected
            .and_then(|id| self.usage.borrow().get(id).cloned())
            .flatten();
        self.usage_label.set_text(
            usage.as_deref().unwrap_or(
                "Provider-reported usage unavailable. No tokens or cost were estimated.",
            ),
        );
        let signature = report
            .map(|report| {
                format!(
                    "{}:{}:{}:{:?}:{:?}:{}:{}",
                    report.conversation.0,
                    report.run_id,
                    report.source_sequence,
                    report
                        .current
                        .as_ref()
                        .map(|current| (&current.fingerprint, current.captured_at_ms)),
                    report.error,
                    report.validation_status,
                    report.validation_current
                )
            })
            .unwrap_or_default();
        if *self.rendered_report.borrow() != signature {
            *self.rendered_report.borrow_mut() = signature;
            if let Some(report) = report {
                let (meta, diff) = match &report.current {
                    Some(current) => (format!("{}\nCaptured: {}\nBranch: {}\nBase: {}\nHEAD: {}\nFingerprint: {}\nUntracked files: {}\n{}{}",
                        current.attribution, format_timestamp_ms(current.captured_at_ms), current.branch, current.base_commit, current.head, current.fingerprint, current.untracked_files, current.status,
                        report.baseline.as_ref().map(|base| format!("\nStart baseline fingerprint: {}", base.fingerprint)).unwrap_or_else(|| "\nStart baseline capture unavailable.".into())), current.diff.as_str()),
                    None => (report.error.clone().unwrap_or_else(|| "Current worktree evidence unavailable.".into()), ""),
                };
                self.changes_meta.set_text(&meta);
                self.diff.set_text(diff);
                let mut validation = format!(
                    "Validation: {}\n{}",
                    report.validation_status,
                    if report.validation_current {
                        "Evidence matches the captured worktree; the host rechecks before acceptance."
                    } else {
                        "No current validation evidence is bound to this captured worktree."
                    }
                );
                if let Some(evidence) = &report.validation {
                    validation.push_str(&format!("\nCommand: {}\nDirectory: {}\nShell: {}\nStarted: {}\nCompleted: {}\nFingerprint: {}\nHEAD: {}\nBase: {}\nUnchanged after run: {}", evidence.command, evidence.cwd, evidence.shell,
                        format_timestamp_ms(evidence.started_at_ms), evidence.completed_at_ms.map(format_timestamp_ms).unwrap_or_else(|| "not completed".into()), evidence.fingerprint, evidence.head, evidence.base_commit, evidence.unchanged_after_run));
                    if let Some(detail) = &evidence.detail {
                        validation.push_str(&format!("\n{detail}"));
                    }
                }
                if report.validation_output_truncated {
                    validation.push_str("\nSaved log is partial; earlier output was not retained.");
                }
                self.validation_meta.set_text(&validation);
                self.validation.set_text(&report.validation_output);
            } else {
                self.changes_meta.set_text("");
                self.diff.set_text("");
                self.validation_meta.set_text("");
                self.validation.set_text("");
            }
        }
        let queued = selected.and_then(|id| {
            state
                .queue
                .entries
                .iter()
                .find(|entry| &entry.intent.id == id)
        });
        let indexed = selected.is_some_and(|id| state.task_indexed(id));
        let queue_busy = state.queue_loading || state.queue_saving || !indexed;
        self.queue_add
            .set_visible(selected.is_some() && queued.is_none());
        self.queue_add.set_sensitive(!queue_busy);
        self.queue_remove.set_visible(queued.is_some());
        self.queue_remove.set_sensitive(!queue_busy);
        self.dependency_search.set_visible(queued.is_some());
        self.dependencies.set_visible(queued.is_some());
        self.dependency_note.set_visible(queued.is_some());
        let queue_status = if let Some(notice) = &state.notice {
            notice.clone()
        } else if state.queue_saving {
            "Saving queue intent…".into()
        } else if state.queue_loading {
            "Refreshing shared queue…".into()
        } else {
            format!(
                "Queue revision {} · {} entries",
                state.queue.revision,
                state.queue.entries.len()
            )
        };
        self.queue_status.set_text(&queue_status);
        self.rendering.set(true);
        if let (Some(id), Some(entry)) = (selected, queued) {
            if !state.queue_saving {
                let query = self.dependency_search.text().to_lowercase();
                let mut candidates: Vec<_> = rows
                    .iter()
                    .filter(|row| {
                        row.kind == ConversationKind::Task
                            && state.task_indexed(&row.id)
                            && &row.id != id
                            && (entry.intent.dependencies.contains(&row.id)
                                || query.is_empty()
                                || format!("{} {} {}", row.title, row.provider, row.cwd)
                                    .to_lowercase()
                                    .contains(&query))
                    })
                    .collect();
                candidates.sort_by(|a, b| {
                    entry
                        .intent
                        .dependencies
                        .contains(&b.id)
                        .cmp(&entry.intent.dependencies.contains(&a.id))
                        .then(a.id.cmp(&b.id))
                });
                let count = candidates.len();
                candidates.truncate(48);
                self.dependency_note.set_text(&format!("{} of {count} matching tasks. Selected dependencies remain visible; type a title or project to find others.", candidates.len()));
                let keep: HashSet<_> = candidates.iter().map(|row| row.id.clone()).collect();
                let mut widgets = self.dependency_widgets.borrow_mut();
                let stale: Vec<_> = widgets
                    .keys()
                    .filter(|key| !keep.contains(*key))
                    .cloned()
                    .collect();
                for id in stale {
                    if let Some(button) = widgets.remove(&id) {
                        self.dependencies.remove(&button);
                    }
                }
                for row in candidates {
                    let button = widgets.entry(row.id.clone()).or_insert_with(|| {
                        let button = gtk::CheckButton::new();
                        let data = self.state.clone();
                        let guard = self.rendering.clone();
                        let source = id.clone();
                        let dependency = row.id.clone();
                        button.connect_toggled(move |button| {
                            if !guard.get() && data.borrow().selected() == Some(&source) {
                                data.borrow_mut().set_dependency(
                                    source.clone(),
                                    dependency.clone(),
                                    button.is_active(),
                                );
                            }
                        });
                        self.dependencies.append(&button);
                        button
                    });
                    button.set_label(Some(&format!(
                        "{} · {}",
                        row.title,
                        row.presentation().status_label
                    )));
                    if let Some(label) = button.child().and_downcast::<gtk::Label>() {
                        label.set_wrap(true);
                        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                        label.set_xalign(0.0);
                    }
                    button.set_tooltip_text(Some(&format!("{}\n{}", row.cwd, row.id.0)));
                    button.set_active(entry.intent.dependencies.contains(&row.id));
                    button.set_sensitive(!queue_busy);
                }
            }
        } else {
            for (_, button) in self.dependency_widgets.borrow_mut().drain() {
                self.dependencies.remove(&button);
            }
        }
        if queue_busy {
            for button in self.dependency_widgets.borrow().values() {
                button.set_sensitive(false);
            }
        }
        self.rendering.set(false);
        let queue = state.queue.project(&rows);
        self.render_queue(&queue);
    }

    fn render_queue(&self, queue: &[WorkspaceQueueRow]) {
        let keep: HashSet<_> = queue.iter().map(|row| &row.id).collect();
        let mut widgets = self.queue_widgets.borrow_mut();
        let stale: Vec<_> = widgets
            .keys()
            .filter(|id| !keep.contains(id))
            .cloned()
            .collect();
        for id in stale {
            if let Some(widget) = widgets.remove(&id) {
                self.queue_rows.remove(&widget.root);
            }
        }
        for row in queue {
            let widget = widgets.entry(row.id.clone()).or_insert_with(|| {
                let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
                let title = label("");
                title.add_css_class("heading");
                root.append(&title);
                let detail = label("");
                root.append(&detail);
                let open = gtk::Button::with_label("Open task controls");
                open.set_halign(gtk::Align::Start);
                let callback = self.callback.clone();
                let id = row.id.clone();
                open.connect_clicked(move |_| callback(Action::OpenTask(id.clone())));
                root.append(&open);
                self.queue_rows.append(&root);
                QueueWidgets {
                    root,
                    title,
                    detail,
                }
            });
            widget.title.set_text(&row.title);
            widget.detail.set_text(&format!(
                "{} · {}{}{}",
                row.state.label(),
                row.location.label(),
                row.reason
                    .as_ref()
                    .map(|reason| format!("\n{reason}"))
                    .unwrap_or_default(),
                if row.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!("\n{} dependencies await acceptance.", row.blocked_by.len())
                }
            ));
        }
    }
}

fn result_material(report: &WorkspaceResultReport, log: bool) -> Option<WorkspaceMaterial> {
    let current = report.current.as_ref();
    let text = if log {
        report.validation_output.as_str()
    } else {
        current?.diff.as_str()
    };
    if text.is_empty() {
        return None;
    }
    let captured = current
        .map(|current| format_timestamp_ms(current.captured_at_ms))
        .unwrap_or_else(|| "capture time unavailable".into());
    let fingerprint = if log {
        report
            .validation
            .as_ref()
            .map(|value| value.fingerprint.as_str())
    } else {
        current.map(|value| value.fingerprint.as_str())
    }
    .unwrap_or("unavailable");
    let attribution = current
        .map(|current| current.attribution.as_str())
        .unwrap_or("Captured validation output; change attribution unavailable.");
    Some(WorkspaceMaterial { id: uuid::Uuid::new_v4().to_string(), label: if log { "Captured validation log" } else { "Captured worktree diff" }.into(),
        kind: if log { MaterialKind::Log } else { MaterialKind::Diff },
        text: format!("Source conversation: {}\nRun: {}\nSequence: {}\nCaptured: {captured}\nFingerprint: {fingerprint}\n{attribution}\n\n{text}", report.conversation.0, report.run_id, report.source_sequence),
        source: None, truncated: log && report.validation_output_truncated })
}
