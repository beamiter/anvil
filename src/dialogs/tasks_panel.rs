//! Relm4 agent Tasks panel.
//!
//! The component is a pure view: the application model owns the task manager,
//! native runtime, and diff worker, pushes composed snapshots in through
//! [`TasksPanelMsg::Sync`], and executes the [`TaskPanelAction`] outputs this
//! panel stages. Provider-controlled text arrives already display-safe; every
//! widget here treats it as plain text.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

#[path = "task_filters.rs"]
mod task_filters;
use task_filters::{TaskFilters, STATES};

use crate::agent_task::{ApprovalId, CodexAppServerApproval, TaskId, TaskStatus};
use crate::agent_task_ui::{
    approval_summary, native_follow_up_can_send, plan_list_refresh, render_stream_history,
    render_stream_status, render_stream_text, row_status_line, stream_history_index, task_reason,
    FollowUpReceipts, TaskFollowUpRequest, TaskPanelAction, TaskRowSnapshot,
};

/// Counts describe current search matches before the selected quick view.
fn dashboard_filter_label(
    view: crate::agent_task::DashboardView,
    counts: &crate::agent_task::DashboardCounts,
) -> String {
    use crate::agent_task::DashboardView;
    let (label, count) = match view {
        DashboardView::All => ("All", counts.total),
        DashboardView::NeedsAttention => ("Needs you", counts.attention),
        DashboardView::Running => ("Running", counts.running),
        DashboardView::NotStarted => ("Not started", counts.created),
        DashboardView::Review => ("Review", counts.review),
        DashboardView::Finished => ("Finished", counts.done + counts.failed + counts.cancelled),
    };
    format!("{label} {count}")
}

const STREAM_PAGE: &str = "stream";
const DIFF_PAGE: &str = "diff";
const INSPECTOR_PAGE: &str = "details";
const CREATE_TASK_LABEL: &str = "New agent task from selected block";
const CLOSE_TASKS_LABEL: &str = "Close Tasks panel";

/// Derive the stack page and both mutually exclusive button states from one
/// request instead of treating the Diff button as the source of truth.
fn task_page_presentation(requested: &str) -> (&'static str, bool, bool) {
    if requested == INSPECTOR_PAGE {
        (INSPECTOR_PAGE, false, false)
    } else if requested == DIFF_PAGE {
        (DIFF_PAGE, false, true)
    } else {
        (STREAM_PAGE, true, false)
    }
}

fn task_page_toggle_request(
    active: bool,
    synchronizing: bool,
    page: &'static str,
) -> Option<&'static str> {
    (active && !synchronizing).then_some(page)
}

/// Full panel state pushed by the application after every domain change.
#[derive(Clone, Debug, Default)]
pub(crate) struct TasksPanelSync {
    pub(crate) rows: Vec<TaskRowSnapshot>,
    pub(crate) workspaces: HashMap<TaskId, std::path::PathBuf>,
    pub(crate) dashboard: crate::agent_task::DashboardSnapshot,
    pub(crate) selected: Option<TaskId>,
    pub(crate) detail: Option<Box<TaskDetailSync>>,
    pub(crate) create_enabled: bool,
    pub(crate) create_hint: String,
    pub(crate) pending_creation: bool,
}

/// Everything the detail pane renders for the selected task.
#[derive(Clone, Debug)]
pub(crate) struct TaskDetailSync {
    pub(crate) id: TaskId,
    pub(crate) provider: crate::agent_task::AgentProvider,
    /// Hidden tasks offer only Restore and read-only inspection.
    pub(crate) archived: bool,
    /// Hide is offered only when the task manager would accept it now.
    pub(crate) can_archive: bool,
    /// Why Hide is unavailable for a visible task.
    pub(crate) archive_note: Option<String>,
    pub(crate) can_restore: bool,
    pub(crate) title: String,
    pub(crate) status_line: String,
    pub(crate) branch: String,
    pub(crate) inspector: String,
    pub(crate) guidance: crate::agent_task::TaskGuidance,
    pub(crate) validation_status: crate::agent_task::TaskValidationStatus,
    pub(crate) native_label: String,
    pub(crate) manual_review: bool,
    pub(crate) can_accept_review: bool,
    /// Shared with the runtime until the provider view changes, so an idle
    /// task is neither copied each tick nor re-rendered as a changed stream.
    pub(crate) stream: Option<std::sync::Arc<crate::agent_task::CodexAppServerViewSnapshot>>,
    pub(crate) approvals: Vec<CodexAppServerApproval>,
    pub(crate) completed_turns: usize,
    pub(crate) can_start_native: bool,
    pub(crate) can_start_terminal: bool,
    pub(crate) can_focus_terminal: bool,
    pub(crate) can_focus_validation: bool,
    pub(crate) can_stop: bool,
    pub(crate) can_finish: bool,
    pub(crate) can_run_validation: bool,
    pub(crate) can_complete: bool,
    pub(crate) can_follow_up: bool,
    pub(crate) follow_up_hint: String,
    pub(crate) diff: Option<DiffSync>,
}

#[derive(Clone, Debug)]
pub(crate) struct DiffSync {
    pub(crate) header: String,
    pub(crate) scope: String,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    pub(crate) truncated: bool,
    pub(crate) text: String,
}

/// Fixed set of task action buttons; the selected task id is resolved when
/// the message is handled, never captured into a stale GTK closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionKind {
    StartNative,
    RecentHistory,
    EarlierHistory,
    StartTerminal,
    FocusTerminal,
    FocusValidation,
    StopCodex,
    FinishCodex,
    RunValidation,
    Complete,
    AcceptReviewed,
    ReviewDiff,
    Archive,
    Restore,
}

/// Choose a visible next operation only from the adapter's runtime gates.
/// Guidance text never grants permission or approves a pending request.
fn primary_task_action(detail: &TaskDetailSync) -> Option<(ActionKind, String)> {
    let (kind, label) = if detail.archived {
        return detail
            .can_restore
            .then(|| (ActionKind::Restore, "Restore task".into()));
    } else if !detail.approvals.is_empty() {
        return None;
    } else if detail.can_finish {
        (ActionKind::FinishCodex, "Finish session")
    } else if detail.can_focus_validation
        && (detail.validation_status.needs_attention()
            || detail.validation_status == crate::agent_task::TaskValidationStatus::Running)
    {
        (ActionKind::FocusValidation, "Open validation output")
    } else if detail.can_complete {
        (ActionKind::Complete, "Mark complete")
    } else if detail.can_accept_review {
        (ActionKind::ReviewDiff, "Review changes")
    } else if detail.can_run_validation {
        (ActionKind::RunValidation, "Run validation")
    } else if detail.can_start_native {
        return Some((ActionKind::StartNative, detail.native_label.clone()));
    } else if detail.can_start_terminal {
        (ActionKind::StartTerminal, "Start terminal")
    } else if detail.can_stop {
        return detail
            .can_focus_terminal
            .then(|| (ActionKind::FocusTerminal, "Open agent terminal".into()));
    } else if detail.can_focus_validation {
        (ActionKind::FocusValidation, "Open validation output")
    } else if detail.can_focus_terminal {
        (ActionKind::FocusTerminal, "Open agent terminal")
    } else {
        (ActionKind::ReviewDiff, "Review changes")
    };
    Some((kind, label.to_string()))
}

const ACTION_KINDS: [(ActionKind, &str); 14] = [
    (ActionKind::StartNative, "Start provider"),
    (ActionKind::RecentHistory, "Recent history"),
    (ActionKind::EarlierHistory, "Load earlier"),
    (ActionKind::StartTerminal, "Start terminal"),
    (ActionKind::FocusTerminal, "Agent terminal"),
    (ActionKind::FocusValidation, "Validation output"),
    (ActionKind::StopCodex, "Stop"),
    (ActionKind::FinishCodex, "Finish"),
    (ActionKind::RunValidation, "Validate"),
    (ActionKind::Complete, "Complete"),
    (ActionKind::AcceptReviewed, "Accept reviewed work"),
    (ActionKind::ReviewDiff, "Diff"),
    (ActionKind::Archive, "Hide"),
    (ActionKind::Restore, "Restore"),
];

#[derive(Debug)]
pub(crate) enum TasksPanelMsg {
    Sync(Box<TasksPanelSync>),
    DashboardQuery(String),
    OpenDashboardResult,
    ClearDashboardFilters,
    DashboardView(crate::agent_task::DashboardView),
    AttentionFilter(Option<crate::agent_task::DashboardAttentionKind>),
    DashboardSort(crate::agent_task::DashboardSort),
    ProviderFilter(Option<crate::agent_task::AgentProvider>),
    ProjectFilter(Option<std::path::PathBuf>),
    StatusFilter(Option<crate::agent_task::TaskStatus>),
    NextAttention,
    PreviousAttention,
    FocusSelectedProject,
    /// Switch between ordinary views and the hidden (archived) tasks.
    ShowHidden(bool),
    /// Return to the ordinary views, for example when the panel is opened.
    LeaveHidden,
    /// Stable identity captured when the row gesture occurs. A later filter
    /// change may hide it, but must never retarget a queued gesture.
    SelectRow(TaskId),
    FollowUpChanged(TaskId, String),
    SendFollowUp(TaskId),
    FollowUpResolved(TaskFollowUpRequest, Result<(), String>),
    Create,
    ComposeProject,
    Act(TaskId, ActionKind),
    Decide(TaskId, ApprovalId, bool),
    ShowPage(&'static str),
    HistoryStep(TaskId, i32),
    Close,
}

#[derive(Debug)]
pub(crate) enum TasksPanelOutput {
    Action(TaskPanelAction),
}

/// Cached by stable task identity, including currently filtered-out live rows.
#[derive(Clone)]
struct TaskRowWidgets {
    row: gtk::ListBoxRow,
    title: gtk::Label,
    status: gtk::Label,
    reason: gtk::Label,
    snapshot: TaskRowSnapshot,
    #[cfg(test)]
    updates: usize,
}

impl TaskRowWidgets {
    fn new(snapshot: &TaskRowSnapshot) -> Self {
        let title = gtk::Label::new(None);
        title.set_max_width_chars(28);
        let status = gtk::Label::new(None);
        status.add_css_class("dim-label");
        status.add_css_class("caption");
        let reason = gtk::Label::new(None);
        reason.set_single_line_mode(true);
        reason.set_max_width_chars(28);
        reason.add_css_class("caption");
        reason.add_css_class("dim-label");
        reason.add_css_class("task-reason");
        let lines = gtk::Box::new(gtk::Orientation::Vertical, 2);
        lines.set_margin_top(4);
        lines.set_margin_bottom(4);
        lines.set_margin_start(4);
        for label in [&title, &status, &reason] {
            label.set_halign(gtk::Align::Start);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            lines.append(label);
        }
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&lines));
        let mut widgets = Self {
            row,
            title,
            status,
            reason,
            snapshot: snapshot.clone(),
            #[cfg(test)]
            updates: 0,
        };
        widgets.update(snapshot);
        widgets
    }

    fn update(&mut self, snapshot: &TaskRowSnapshot) {
        #[cfg(test)]
        {
            self.updates += 1;
        }
        self.title.set_label(&snapshot.title);
        self.status.set_label(&row_status_line(snapshot));
        if snapshot.needs_attention {
            self.status.add_css_class("warning");
        } else {
            self.status.remove_css_class("warning");
        }
        let reason = task_reason(
            snapshot.status,
            snapshot.validation_status,
            snapshot.status_detail.as_deref(),
            snapshot.validation_detail.as_deref(),
        );
        self.reason.set_visible(reason.is_some());
        // Existing diagnostics identify present previews by this class.
        if reason.is_some() {
            self.reason.add_css_class("task-reason");
        } else {
            self.reason.remove_css_class("task-reason");
        }
        self.reason.set_label(reason.as_deref().unwrap_or_default());
        self.reason.set_tooltip_text(reason.as_deref());
        self.snapshot.clone_from(snapshot);
    }
}

/// GTK retains the selected bit on a detached row even after clearing the
/// ListBox's selected-row pointer. Clear it while attached so reinserting the
/// same widget can select it again. Caller holds the selection signal guard.
fn detach_task_row(list: &gtk::ListBox, row: &gtk::ListBoxRow) {
    if row.is_selected() {
        list.unselect_row(row);
    }
    list.remove(row);
}

/// Restores the previous guard even if GTK synchronously calls user handlers.
struct SelectionGuard {
    flag: Rc<Cell<bool>>,
    previous: bool,
}
impl SelectionGuard {
    fn enter(flag: &Rc<Cell<bool>>) -> Self {
        Self {
            previous: flag.replace(true),
            flag: flag.clone(),
        }
    }
}
impl Drop for SelectionGuard {
    fn drop(&mut self) {
        self.flag.set(self.previous);
    }
}

pub(crate) struct TasksPanelModel {
    sync: TasksPanelSync,
    unfiltered_sync: TasksPanelSync,
    dashboard_query: String,
    dashboard_view: crate::agent_task::DashboardView,
    attention_kind: Option<crate::agent_task::DashboardAttentionKind>,
    attention_counts: Vec<usize>,
    dashboard_sort: crate::agent_task::DashboardSort,
    dashboard_sort_guard: Rc<Cell<bool>>,
    filters: TaskFilters,
    workspace_choices: Rc<RefCell<Vec<std::path::PathBuf>>>,
    dashboard_summary: String,
    dashboard_counts: crate::agent_task::DashboardCounts,
    dashboard_buttons: Vec<(crate::agent_task::DashboardView, gtk::Button)>,
    /// Per-task follow-up drafts; the app never sees keystrokes, only the
    /// final text carried by `TaskPanelAction::FollowUp`.
    follow_up_drafts: HashMap<TaskId, String>,
    follow_up_receipts: FollowUpReceipts,
    follow_up_guard: Rc<Cell<bool>>,
    action_buttons: Vec<(ActionKind, gtk::Button)>,
    /// Task shown by the action buttons, captured before queuing a click.
    action_target: Rc<Cell<Option<TaskId>>>,
    primary_target: Rc<Cell<Option<(TaskId, ActionKind)>>>,
    /// Render cache: what the list and approval widgets currently show.
    /// Refresh diffs each pushed Sync against it and touches GTK only on
    /// real change, so an unchanged Sync is a pure no-op — no widget churn,
    /// and no signals feeding back into the component's input queue.
    rendered_rows: Vec<TaskRowSnapshot>,
    row_widgets: HashMap<TaskId, TaskRowWidgets>,
    rendered_selected: Option<TaskId>,
    rendered_approvals: Vec<CodexAppServerApproval>,
    /// Set while refresh applies a programmatic selection, so the resulting
    /// `row-selected` emission is not mistaken for a user gesture.
    selection_guard: Rc<Cell<bool>>,
    selection_ids: Rc<RefCell<Vec<TaskId>>>,
    page: &'static str,
    history_ordinal: Option<usize>,
    history_task: Option<TaskId>,
    /// Programmatic toggle synchronization must not enqueue stale ShowPage
    /// messages after newer user selections already waiting in the queue.
    page_guard: Rc<Cell<bool>>,
}

impl TasksPanelModel {
    fn apply_dashboard_filters(&mut self) {
        let mut sync = self.unfiltered_sync.clone();
        let mut dashboard =
            self.filters
                .scope(&sync.dashboard, &sync.workspaces, &self.dashboard_query);
        let attention_counts = dashboard.attention_counts();
        self.attention_counts = crate::agent_task::DashboardAttentionKind::ALL
            .iter()
            .map(|kind| attention_counts.get(*kind))
            .collect();
        dashboard.rows.retain(|row| {
            self.filters
                .status
                .is_none_or(|status| row.status == status)
        });
        dashboard.apply_view(
            self.dashboard_view,
            self.attention_kind,
            self.dashboard_sort,
        );
        let mut by_id: HashMap<_, _> = sync.rows.into_iter().map(|row| (row.id, row)).collect();
        sync.rows = dashboard
            .rows
            .iter()
            .filter_map(|projected| {
                let mut row = by_id.remove(&projected.id)?;
                row.title.clone_from(&projected.title);
                row.branch.clone_from(&projected.branch);
                row.status_detail.clone_from(&projected.status_detail);
                row.needs_attention = projected.needs_attention;
                Some(row)
            })
            .collect();
        if dashboard.selected_index(sync.selected).is_none() {
            sync.detail = None;
        }
        self.dashboard_summary = if self.filters.hidden {
            format!(
                "{} hidden shown · {} search matches · restore a task to return it to its previous state",
                dashboard.rows.len(),
                dashboard.counts.total,
            )
        } else {
            format!(
                "{} shown · {} search matches · {} need you · {} awaiting approval",
                dashboard.rows.len(),
                dashboard.counts.total,
                dashboard.counts.attention,
                dashboard.counts.pending_approval,
            )
        };
        self.dashboard_counts = dashboard.counts.clone();
        sync.dashboard = dashboard;
        self.sync = sync;
    }

    fn emit_action(&self, action: TaskPanelAction, sender: &ComponentSender<Self>) {
        if sender.output(TasksPanelOutput::Action(action)).is_err() {
            log::warn!("tasks panel output channel closed");
        }
    }

    /// Hidden (archived) tasks the domain still holds and can restore.
    /// The selected task and its status in the latest unfiltered snapshot.
    fn selected_status(&self) -> (Option<TaskId>, Option<TaskStatus>) {
        let selected = self.unfiltered_sync.selected;
        let status = selected.and_then(|id| {
            self.unfiltered_sync
                .dashboard
                .rows
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.status)
        });
        (selected, status)
    }

    fn archived_count(&self) -> usize {
        self.unfiltered_sync
            .dashboard
            .rows
            .iter()
            .filter(|row| row.status == crate::agent_task::TaskStatus::Archived)
            .count()
    }

    /// Rows eligible for the current view: hidden tasks only while the
    /// hidden view is open, every other task otherwise.
    fn in_archive_scope(&self, row: &crate::agent_task::DashboardRow) -> bool {
        (row.status == crate::agent_task::TaskStatus::Archived) == self.filters.hidden
    }

    /// Reconcile by stable TaskId. The domain and hidden tasks are untouched.
    fn sync_task_list(&mut self, widgets: &TasksPanelModelWidgets) {
        let plan = plan_list_refresh(
            &self.rendered_rows,
            self.rendered_selected,
            &self.sync.rows,
            self.sync.selected,
        );
        // Raw task rows may contain archived records. The unfiltered dashboard
        // is the authoritative set of live visible-capable task identities.
        let live: std::collections::HashSet<_> = self
            .unfiltered_sync
            .dashboard
            .rows
            .iter()
            .filter(|row| self.in_archive_scope(row))
            .map(|row| row.id)
            .collect();
        let _guard = SelectionGuard::enter(&self.selection_guard);
        let focused = self
            .row_widgets
            .iter()
            .find_map(|(id, widgets)| widgets.row.has_focus().then_some(*id));
        if plan.update_rows {
            for row in &self.sync.rows {
                match self.row_widgets.get_mut(&row.id) {
                    Some(widgets) if widgets.snapshot != *row => widgets.update(row),
                    Some(_) => {}
                    None => {
                        self.row_widgets.insert(row.id, TaskRowWidgets::new(row));
                    }
                }
            }
        }
        if plan.reconcile_order {
            let ids: Vec<_> = self.sync.rows.iter().map(|row| row.id).collect();
            let visible: std::collections::HashSet<_> = ids.iter().copied().collect();
            // Release the RefCell before GTK calls: row-selected is synchronous.
            *self.selection_ids.borrow_mut() = ids.clone();
            for old in &self.rendered_rows {
                if !visible.contains(&old.id) {
                    if let Some(row) = self.row_widgets.get(&old.id) {
                        detach_task_row(&widgets.task_list, &row.row);
                    }
                }
            }
            for (index, id) in ids.iter().enumerate() {
                let row = &self.row_widgets[id].row;
                if widgets.task_list.row_at_index(index as i32).as_ref() != Some(row) {
                    if row.parent().is_some() {
                        detach_task_row(&widgets.task_list, row);
                    }
                    widgets.task_list.insert(row, index as i32);
                }
            }
        }
        // Prune even when only a hidden/archived task changed.
        self.row_widgets.retain(|id, _| live.contains(id));
        if plan.apply_selection {
            // Resolve the final widget by stable identity after all moves.
            let row = self
                .sync
                .selected
                .filter(|_| plan.select_index.is_some())
                .and_then(|id| self.row_widgets.get(&id))
                .map(|widgets| widgets.row.clone());
            if widgets.task_list.selected_row() != row {
                widgets.task_list.select_row(row.as_ref());
            }
        }
        if plan.reconcile_order {
            // Preserve focus only if it was on a surviving visible task; never
            // steal it from search, detail controls, or a now-hidden task.
            if let Some(row) = focused.and_then(|id| self.row_widgets.get(&id)) {
                if row.row.parent().is_some() && !row.row.has_focus() {
                    row.row.grab_focus();
                }
            }
        }
        if plan.update_rows {
            self.rendered_rows.clone_from(&self.sync.rows);
        }
        self.rendered_selected = self.sync.selected;
    }

    /// Rebuild the approval cards only when the pushed approval set changed;
    /// an unchanged Sync leaves the existing cards (and their wired Decide
    /// closures) untouched.
    fn sync_approvals(&mut self, widgets: &TasksPanelModelWidgets, sender: &ComponentSender<Self>) {
        let incoming: &[CodexAppServerApproval] = self
            .sync
            .detail
            .as_deref()
            .map(|detail| detail.approvals.as_slice())
            .unwrap_or(&[]);
        if self.rendered_approvals.as_slice() == incoming {
            widgets.approvals_box.set_visible(!incoming.is_empty());
            return;
        }
        while let Some(child) = widgets.approvals_box.first_child() {
            widgets.approvals_box.remove(&child);
        }
        widgets.approvals_box.set_visible(!incoming.is_empty());
        if let Some(detail) = self.sync.detail.as_deref() {
            for approval in incoming {
                let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
                card.add_css_class("card");
                let summary = gtk::Label::new(Some(&approval_summary(approval)));
                summary.set_halign(gtk::Align::Start);
                summary.set_wrap(true);
                summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                summary.set_margin_all(6);
                card.append(&summary);
                let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                buttons.set_halign(gtk::Align::End);
                buttons.set_margin_bottom(4);
                let approve = gtk::Button::with_label("Approve");
                approve.add_css_class("suggested-action");
                let deny = gtk::Button::with_label("Deny");
                deny.add_css_class("destructive-action");
                let task_id = detail.id;
                let approval_id: ApprovalId = approval.id;
                approve.connect_clicked({
                    let sender = sender.clone();
                    move |_| sender.input(TasksPanelMsg::Decide(task_id, approval_id, true))
                });
                deny.connect_clicked({
                    let sender = sender.clone();
                    move |_| sender.input(TasksPanelMsg::Decide(task_id, approval_id, false))
                });
                buttons.append(&approve);
                buttons.append(&deny);
                card.append(&buttons);
                widgets.approvals_box.append(&card);
            }
        }
        self.rendered_approvals = incoming.to_vec();
    }
}

#[relm4::component(pub(crate))]
impl Component for TasksPanelModel {
    type Init = ();
    type Input = TasksPanelMsg;
    type Output = TasksPanelOutput;
    type CommandOutput = ();

    view! {
        root = gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            // Same floor as the AI Chats page (280): both pages share the
            // `ai_paned` end slot through one stack, and a higher floor made
            // the stack's minimum exceed the slot's default width, which
            // GTK reported as a measure negotiation fight.
            set_width_request: 280,
            set_hexpand: false,
            set_vexpand: true,
            add_css_class: "ai-panel",

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 4,
                set_margin_all: 6,

                gtk::Label {
                    set_label: "Agent tasks",
                    set_halign: gtk::Align::Start,
                    set_hexpand: true,
                    add_css_class: "heading",
                },

                #[name(compose_button)]
                gtk::Button {
                    set_label: "New task",
                    add_css_class: "suggested-action",
                    set_tooltip_text: Some("Create a project task from your own goal"),
                    connect_clicked => TasksPanelMsg::ComposeProject,
                },

                #[name(create_button)]
                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    add_css_class: "flat",
                    set_tooltip_text: Some("New agent task from the selected block"),
                    update_property: &[
                        gtk::accessible::Property::Label(CREATE_TASK_LABEL),
                    ],
                    connect_clicked => TasksPanelMsg::Create,
                },

                gtk::Button {
                    set_icon_name: "window-close-symbolic",
                    add_css_class: "flat",
                    set_tooltip_text: Some("Close Tasks panel"),
                    update_property: &[
                        gtk::accessible::Property::Label(CLOSE_TASKS_LABEL),
                    ],
                    connect_clicked => TasksPanelMsg::Close,
                },
            },

            #[name(create_hint)]
            gtk::Label {
                set_halign: gtk::Align::Start,
                set_margin_start: 8,
                set_margin_end: 8,
                set_wrap: true,
                add_css_class: "dim-label",
                add_css_class: "caption",
                set_visible: false,
            },

            #[name(dashboard_query_entry)]
            gtk::Entry {
                set_placeholder_text: Some("Search tasks · provider:codex status:review"),
                set_max_length: 4096,
                update_property: &[gtk::accessible::Property::Label("Search agent tasks")],
                connect_changed[sender] => move |entry| {
                    sender.input(TasksPanelMsg::DashboardQuery(entry.text().to_string()));
                },
                connect_activate => TasksPanelMsg::OpenDashboardResult,
            },
            #[name(dashboard_query_error)]
            gtk::Label {
                set_xalign: 0.0,
                set_wrap: true,
                set_visible: false,
                add_css_class: "error",
                add_css_class: "caption",
            },
            gtk::Expander {
                set_label: Some("Search syntax"),
                gtk::Label {
                    set_label: "Words combine with AND. Quote phrases and use - to exclude.\nprovider:codex status:working,review\nattention:approval validation:failed\nproject:repo branch:feature -runtime:terminal\nOther fields: id:, is:. Filters and views stay active.",
                    set_xalign: 0.0,
                    set_wrap: true,
                    set_selectable: true,
                    set_max_width_chars: 36,
                    add_css_class: "caption",
                    set_margin_all: 6,
                },
            },
            #[name(scope_filters)]
            gtk::Expander {
                set_label: Some("Provider, project and state"),
                gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 4,
                    #[name(provider_filter)]
                    gtk::DropDown {
                        set_model: Some(&gtk::StringList::new(&std::iter::once("All providers").chain(crate::agent_task::AgentProvider::ALL.iter().map(|provider| provider.display_name())).collect::<Vec<_>>())),
                        update_property: &[gtk::accessible::Property::Label("Task provider filter")],
                        connect_selected_notify[sender, guard = model.dashboard_sort_guard.clone()] => move |dropdown| {
                            if !guard.get() {
                                let provider = dropdown.selected().checked_sub(1).and_then(|index| crate::agent_task::AgentProvider::ALL.get(index as usize).copied());
                                sender.input(TasksPanelMsg::ProviderFilter(provider));
                            }
                        },
                    },
                    #[name(workspace_filter)]
                    gtk::DropDown {
                        set_model: Some(&gtk::StringList::new(&["All projects"])),
                        set_enable_search: true,
                        update_property: &[gtk::accessible::Property::Label("Task project filter")],
                        connect_selected_notify[sender, guard = model.dashboard_sort_guard.clone(), paths = model.workspace_choices.clone()] => move |dropdown| {
                            if !guard.get() {
                                let project = dropdown.selected().checked_sub(1).and_then(|index| paths.borrow().get(index as usize).cloned());
                                sender.input(TasksPanelMsg::ProjectFilter(project));
                            }
                        },
                    },
                    #[name(status_filter)]
                    gtk::DropDown {
                        set_model: Some(&gtk::StringList::new(&std::iter::once("All states").chain(STATES.iter().map(|status| status.label())).collect::<Vec<_>>())),
                        update_property: &[gtk::accessible::Property::Label("Task state filter")],
                        connect_selected_notify[sender, guard = model.dashboard_sort_guard.clone()] => move |dropdown| {
                            if !guard.get() {
                                let status = dropdown.selected().checked_sub(1).and_then(|index| STATES.get(index as usize).copied());
                                sender.input(TasksPanelMsg::StatusFilter(status));
                            }
                        },
                    },
                },
            },
            #[name(attention_filter)]
            gtk::DropDown {
                set_model: Some(&gtk::StringList::new(&["All attention reasons"])),
                update_property: &[gtk::accessible::Property::Label("Task attention reason")],
                set_tooltip_text: Some("Focus approvals, questions, failed validations, review or failed tasks without replacing your search"),
                connect_selected_notify[sender, guard = model.dashboard_sort_guard.clone()] => move |dropdown| {
                    if !guard.get() {
                        let kind = dropdown.selected().checked_sub(1).and_then(|index| crate::agent_task::DashboardAttentionKind::ALL.get(index as usize).copied());
                        sender.input(TasksPanelMsg::AttentionFilter(kind));
                    }
                },
            },
            #[name(dashboard_filters)]
            gtk::FlowBox {
                set_selection_mode: gtk::SelectionMode::None,
                set_max_children_per_line: 3,
            },
            #[name(dashboard_summary)]
            gtk::Label {
                set_wrap: true,
                add_css_class: "caption",
            },

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 6,
                #[name(dashboard_sort)]
                gtk::DropDown {
                    set_model: Some(&gtk::StringList::new(&crate::agent_task::DashboardSort::ALL.map(|sort| sort.label()))),
                    set_hexpand: true,
                    set_tooltip_text: Some("Order visible tasks"),
                    update_property: &[gtk::accessible::Property::Label("Task sort order")],
                    connect_selected_notify[sender, guard = model.dashboard_sort_guard.clone()] => move |dropdown| {
                        if guard.get() {
                            return;
                        }
                        if let Some(sort) = crate::agent_task::DashboardSort::ALL.get(dropdown.selected() as usize) {
                            sender.input(TasksPanelMsg::DashboardSort(*sort));
                        }
                    },
                },
                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 6,
                    #[name(previous_attention)]
                    gtk::Button {
                        set_label: "Previous needs you",
                        set_tooltip_text: Some("Select the previous visible task needing attention"),
                        connect_clicked => TasksPanelMsg::PreviousAttention,
                    },
                    #[name(next_attention)]
                    gtk::Button {
                        set_label: "Next needs you",
                        set_tooltip_text: Some("Select the next visible task needing attention"),
                        connect_clicked => TasksPanelMsg::NextAttention,
                    },
                },
            },

            #[name(dashboard_focus_project)]
            gtk::Button {
                set_label: "Selected project",
                set_tooltip_text: Some("Narrow to the selected task's exact project; keep the current search and provider"),
                connect_clicked => TasksPanelMsg::FocusSelectedProject,
            },

            #[name(dashboard_clear)]
            gtk::Button {
                set_label: "Clear filters",
                set_visible: false,
                connect_clicked => TasksPanelMsg::ClearDashboardFilters,
            },
            #[name(dashboard_reveal)]
            gtk::Button {
                set_visible: false,
                connect_clicked => TasksPanelMsg::ClearDashboardFilters,
                gtk::Label {
                    set_label: "Selected task hidden · Show selected task",
                    set_wrap: true,
                },
            },
            #[name(hidden_toggle)]
            gtk::ToggleButton {
                set_visible: false,
                set_tooltip_text: Some("Hidden tasks keep their worktrees; restore one to bring it back"),
                connect_toggled[sender, guard = model.dashboard_sort_guard.clone()] => move |button| {
                    if !guard.get() {
                        sender.input(TasksPanelMsg::ShowHidden(button.is_active()));
                    }
                },
            },

            gtk::Paned {
                set_orientation: gtk::Orientation::Vertical,
                set_vexpand: true,
                set_wide_handle: true,
                set_position: 170,

                #[wrap(Some)]
                set_start_child = &gtk::ScrolledWindow {
                    set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                    set_min_content_height: 110,

                    #[name(task_list)]
                    gtk::ListBox {
                        set_selection_mode: gtk::SelectionMode::Single,
                        add_css_class: "navigation-sidebar",
                    },
                },

                #[wrap(Some)]
                set_end_child = &gtk::ScrolledWindow {
                    set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                    #[name(detail_viewport)]
                    gtk::Viewport {
                        #[wrap(Some)]
                        set_child = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 4,
                            set_margin_all: 6,

                            #[name(detail_title)]
                            gtk::Label {
                                set_halign: gtk::Align::Start,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                add_css_class: "heading",
                                set_visible: false,
                            },

                            #[name(detail_status)]
                            gtk::Label {
                                set_halign: gtk::Align::Start,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                add_css_class: "dim-label",
                                add_css_class: "caption",
                                set_visible: false,
                            },

                            #[name(next_step_card)]
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 4,
                                set_margin_top: 4,
                                set_margin_bottom: 4,
                                add_css_class: "card",
                                set_visible: false,
                                #[name(next_step_title)]
                                gtk::Label {
                                    set_halign: gtk::Align::Start,
                                    set_margin_start: 8,
                                    set_margin_end: 8,
                                    set_margin_top: 6,
                                    set_wrap: true,
                                    add_css_class: "heading",
                                },
                                #[name(next_step_detail)]
                                gtk::Label {
                                    set_xalign: 0.0,
                                    set_margin_start: 8,
                                    set_margin_end: 8,
                                    set_wrap: true,
                                    set_max_width_chars: 36,
                                    set_lines: 3,
                                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                                    add_css_class: "caption",
                                },
                                #[name(next_step_reason)]
                                gtk::Label {
                                    set_xalign: 0.0,
                                    set_margin_start: 8,
                                    set_margin_end: 8,
                                    set_wrap: true,
                                    set_max_width_chars: 36,
                                    set_lines: 3,
                                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                                    add_css_class: "caption",
                                    add_css_class: "warning",
                                },
                                #[name(primary_action)]
                                gtk::Button {
                                    set_halign: gtk::Align::Start,
                                    set_margin_all: 6,
                                    add_css_class: "suggested-action",
                                    update_property: &[gtk::accessible::Property::Label("Recommended task action")],
                                },
                            },

                            #[name(actions_expander)]
                            gtk::Expander {
                                set_label: Some("All task actions"),
                                set_visible: false,
                                #[name(actions_box)]
                                gtk::FlowBox {
                                    set_selection_mode: gtk::SelectionMode::None,
                                    set_column_spacing: 4,
                                    set_row_spacing: 4,
                                    set_max_children_per_line: 4,
                                },
                            },

                            #[name(approvals_box)]
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 4,
                                set_visible: false,
                            },

                            gtk::Box {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_spacing: 0,
                                set_halign: gtk::Align::Center,

                                #[name(stream_page_button)]
                                gtk::ToggleButton {
                                    set_label: "Stream",
                                    set_active: true,
                                    add_css_class: "flat",
                                    add_css_class: "caption",
                                },

                                #[name(inspector_page_button)]
                                gtk::Button {
                                    set_label: "Details",
                                    add_css_class: "flat",
                                    add_css_class: "caption",
                                    connect_clicked => TasksPanelMsg::ShowPage(INSPECTOR_PAGE),
                                },

                                #[name(diff_page_button)]
                                gtk::ToggleButton {
                                    set_label: "Diff",
                                    add_css_class: "flat",
                                    add_css_class: "caption",
                                },
                            },

                            #[name(page_stack)]
                            gtk::Stack {
                                set_hexpand: true,
                                set_hhomogeneous: false,
                                set_vexpand: true,

                                add_named[Some(STREAM_PAGE)] = &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_spacing: 4,
                                    #[name(stream_status)]
                                    gtk::Label {
                                        set_halign: gtk::Align::Start,
                                        set_wrap: true,
                                        set_lines: 3,
                                        set_ellipsize: gtk::pango::EllipsizeMode::End,
                                    },
                                    gtk::ScrolledWindow {
                                        set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                                        set_vexpand: true,
                                        set_min_content_height: 100,
                                        #[name(stream_view)]
                                        gtk::TextView {
                                            set_editable: false,
                                            set_cursor_visible: false,
                                            set_wrap_mode: gtk::WrapMode::WordChar,
                                            set_left_margin: 4,
                                            set_right_margin: 4,
                                            add_css_class: "ai-explain-body",
                                        },
                                    },
                                    gtk::Box {
                                        set_spacing: 4,
                                        #[name(history_older)]
                                        gtk::Button {
                                            set_label: "Older",
                                        },
                                        #[name(history_newer)]
                                        gtk::Button {
                                            set_label: "Newer",
                                        },
                                        #[name(history_latest)]
                                        gtk::Button {
                                            set_label: "Latest history",
                                        },
                                    },
                                    gtk::Label {
                                        set_label: "Select and copy visible text in either pane. Select All copies only that pane; updates or paging replace that pane’s selection. Omitted text is not copied.",
                                        set_wrap: true,
                                        add_css_class: "caption",
                                        add_css_class: "dim-label",
                                    },
                                    #[name(history_scroll)]
                                    gtk::ScrolledWindow {
                                        set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                                        set_vexpand: true,
                                        set_min_content_height: 100,
                                        #[name(history_view)]
                                        gtk::TextView {
                                            set_editable: false,
                                            set_cursor_visible: false,
                                            set_wrap_mode: gtk::WrapMode::WordChar,
                                            set_left_margin: 4,
                                            set_right_margin: 4,
                                            add_css_class: "ai-explain-body",
                                        },
                                    },
                                },

                                add_named[Some(INSPECTOR_PAGE)] = &gtk::ScrolledWindow {
                                    set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),
                                    #[name(inspector_view)]
                                    gtk::TextView {
                                        set_editable: false,
                                        set_cursor_visible: false,
                                        set_wrap_mode: gtk::WrapMode::WordChar,
                                        set_left_margin: 6,
                                        set_right_margin: 6,
                                        set_top_margin: 8,
                                        set_bottom_margin: 8,
                                    },
                                },

                                add_named[Some(DIFF_PAGE)] = &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_spacing: 2,

                                    #[name(diff_header)]
                                    gtk::Label {
                                        set_halign: gtk::Align::Start,
                                        set_ellipsize: gtk::pango::EllipsizeMode::Middle,
                                        add_css_class: "dim-label",
                                        add_css_class: "caption",
                                    },

                                    gtk::Label {
                                        set_halign: gtk::Align::Start,
                                        set_wrap: true,
                                        add_css_class: "warning",
                                        add_css_class: "caption",
                                        set_label: "Repository-controlled paths and content are untrusted; control and bidirectional formatting characters are made visible or replaced.",
                                    },

                                    gtk::ScrolledWindow {
                                        set_hexpand: true,
                                        set_vexpand: true,
                                        set_policy: (gtk::PolicyType::Automatic, gtk::PolicyType::Automatic),

                                        #[name(diff_view)]
                                        gtk::TextView {
                                            set_editable: false,
                                            set_cursor_visible: false,
                                            set_monospace: true,
                                            set_left_margin: 4,
                                            set_right_margin: 4,
                                        },
                                    },
                                },
                            },

                            #[name(follow_up_hint)]
                            gtk::Label {
                                set_halign: gtk::Align::Start,
                                add_css_class: "dim-label",
                                add_css_class: "caption",
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                set_visible: false,
                            },

                            #[name(follow_up_row)]
                            gtk::Box {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_spacing: 4,
                                set_visible: false,

                                gtk::ScrolledWindow {
                                    set_hexpand: true,
                                    set_min_content_height: 76,
                                    set_max_content_height: 150,
                                    set_hscrollbar_policy: gtk::PolicyType::Never,
                                    set_vscrollbar_policy: gtk::PolicyType::Automatic,
                                    #[name(follow_up_entry)]
                                    gtk::TextView {
                                        set_hexpand: true,
                                        set_wrap_mode: gtk::WrapMode::WordChar,
                                        set_top_margin: 8,
                                        set_bottom_margin: 8,
                                        set_left_margin: 8,
                                        set_right_margin: 8,
                                        add_css_class: "ai-panel-entry",
                                        set_tooltip_text: Some("Follow-up turn · Enter for a new line · Ctrl+Enter to send"),
                                    },
                                },

                                #[name(follow_up_send)]
                                gtk::Button {
                                    set_icon_name: "go-next-symbolic",
                                    add_css_class: "flat",
                                    set_tooltip_text: Some("Send follow-up turn (Ctrl+Enter)"),
                                },
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut action_buttons = Vec::new();
        let action_target = Rc::new(Cell::new(None));
        let primary_target = Rc::new(Cell::new(None));
        let follow_up_guard = Rc::new(Cell::new(false));
        let selection_guard = Rc::new(Cell::new(false));
        let selection_ids = Rc::new(RefCell::new(Vec::<TaskId>::new()));
        let page_guard = Rc::new(Cell::new(false));
        let model = Self {
            sync: TasksPanelSync::default(),
            unfiltered_sync: TasksPanelSync::default(),
            dashboard_query: String::new(),
            dashboard_view: crate::agent_task::DashboardView::All,
            attention_kind: None,
            attention_counts: Vec::new(),
            dashboard_sort: crate::agent_task::DashboardSort::default(),
            dashboard_sort_guard: Rc::new(Cell::new(false)),
            filters: TaskFilters::default(),
            workspace_choices: Rc::new(RefCell::new(Vec::new())),
            dashboard_summary: String::new(),
            dashboard_counts: Default::default(),
            dashboard_buttons: Vec::new(),
            follow_up_drafts: HashMap::new(),
            follow_up_receipts: FollowUpReceipts::default(),
            follow_up_guard: follow_up_guard.clone(),
            action_buttons: Vec::new(),
            action_target: action_target.clone(),
            primary_target: primary_target.clone(),
            rendered_rows: Vec::new(),
            row_widgets: HashMap::new(),
            rendered_selected: None,
            rendered_approvals: Vec::new(),
            selection_guard: selection_guard.clone(),
            selection_ids: selection_ids.clone(),
            page: STREAM_PAGE,
            history_ordinal: None,
            history_task: None,
            page_guard: page_guard.clone(),
        };
        let widgets: TasksPanelModelWidgets = view_output!();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::new(None);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_max_width_chars(24);
            label.set_halign(gtk::Align::Start);
            item.set_child(Some(&label));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let value = item
                .item()
                .unwrap()
                .downcast::<gtk::StringObject>()
                .unwrap();
            let label = item.child().unwrap().downcast::<gtk::Label>().unwrap();
            label.set_label(&value.string());
            label.set_tooltip_text(Some(&value.string()));
        });
        widgets.workspace_filter.set_factory(Some(&factory));
        widgets.workspace_filter.set_list_factory(Some(&factory));

        widgets
            .diff_page_button
            .set_group(Some(&widgets.stream_page_button));

        for (button, direction) in [
            (&widgets.history_older, -1),
            (&widgets.history_newer, 1),
            (&widgets.history_latest, 0),
        ] {
            let target = action_target.clone();
            let sender = sender.clone();
            button.connect_clicked(move |_| {
                if let Some(id) = target.get() {
                    sender.input(TasksPanelMsg::HistoryStep(id, direction));
                }
            });
        }
        widgets.primary_action.connect_clicked({
            let sender = sender.clone();
            let target = primary_target.clone();
            move |_| {
                if let Some((id, kind)) = target.get() {
                    sender.input(TasksPanelMsg::Act(id, kind));
                }
            }
        });
        for (kind, label) in ACTION_KINDS {
            let button = gtk::Button::with_label(label);
            button.add_css_class("caption");
            button.connect_clicked({
                let sender = sender.clone();
                let action_target = action_target.clone();
                move |_| {
                    if let Some(id) = action_target.get() {
                        sender.input(TasksPanelMsg::Act(id, kind));
                    }
                }
            });
            widgets.actions_box.insert(&button, -1);
            action_buttons.push((kind, button));
        }

        let keys = gtk::EventControllerKey::new();
        keys.set_name(Some("task-list-navigation"));
        keys.connect_key_pressed({
            let list = widgets.task_list.downgrade();
            let selection_ids = selection_ids.clone();
            let selection_guard = selection_guard.clone();
            move |_, key, _, modifiers| {
                use gtk::gdk::Key;
                if selection_guard.get()
                    || modifiers.intersects(
                        gtk::gdk::ModifierType::CONTROL_MASK
                            | gtk::gdk::ModifierType::ALT_MASK
                            | gtk::gdk::ModifierType::SUPER_MASK,
                    )
                    || !matches!(key, Key::Up | Key::Down | Key::Home | Key::End)
                {
                    return gtk::glib::Propagation::Proceed;
                }
                let Some(list) = list.upgrade() else {
                    return gtk::glib::Propagation::Proceed;
                };
                let len = selection_ids.borrow().len();
                if len != 0 {
                    let current = list.selected_row().map(|row| row.index() as usize);
                    let index = match key {
                        Key::Home => 0,
                        Key::End => len - 1,
                        Key::Up => current.map_or(len - 1, |i| i.saturating_sub(1)),
                        _ => current.map_or(0, |i| (i + 1).min(len - 1)),
                    };
                    if let Some(row) = list.row_at_index(index as i32) {
                        list.select_row(Some(&row));
                        row.grab_focus();
                    }
                }
                gtk::glib::Propagation::Stop
            }
        });
        widgets.task_list.add_controller(keys);

        widgets.task_list.connect_row_selected({
            let sender = sender.clone();
            let selection_guard = selection_guard.clone();
            let selection_ids = selection_ids.clone();
            move |_, row| {
                // Only genuine gestures may queue SelectRow; programmatic
                // selection applied by refresh runs behind the guard.
                if selection_guard.get() {
                    return;
                }
                if let Some(row) = row {
                    let id = usize::try_from(row.index())
                        .ok()
                        .and_then(|index| selection_ids.borrow().get(index).copied());
                    if let Some(id) = id {
                        sender.input(TasksPanelMsg::SelectRow(id));
                    }
                }
            }
        });
        widgets.follow_up_entry.buffer().connect_changed({
            let sender = sender.clone();
            let target = action_target.clone();
            let guard = follow_up_guard.clone();
            move |entry| {
                if guard.get() {
                    return;
                }
                if let Some(id) = target.get() {
                    sender.input(TasksPanelMsg::FollowUpChanged(
                        id,
                        entry
                            .text(&entry.start_iter(), &entry.end_iter(), true)
                            .to_string(),
                    ));
                }
            }
        });
        let follow_up_keys = gtk::EventControllerKey::new();
        follow_up_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        follow_up_keys.connect_key_pressed({
            let sender = sender.clone();
            let target = action_target.clone();
            let editor = widgets.follow_up_entry.clone();
            move |controller, key, _, modifiers| {
                if matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                    && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                    && !modifiers.intersects(
                        gtk::gdk::ModifierType::SHIFT_MASK
                            | gtk::gdk::ModifierType::ALT_MASK
                            | gtk::gdk::ModifierType::SUPER_MASK
                            | gtk::gdk::ModifierType::META_MASK,
                    )
                {
                    if controller
                        .current_event()
                        .as_ref()
                        .is_some_and(|event| editor.im_context_filter_keypress(event))
                    {
                        return gtk::glib::Propagation::Stop;
                    }
                    if let Some(id) = target.get() {
                        sender.input(TasksPanelMsg::SendFollowUp(id));
                    }
                    gtk::glib::Propagation::Stop
                } else {
                    gtk::glib::Propagation::Proceed
                }
            }
        });
        widgets.follow_up_entry.add_controller(follow_up_keys);
        widgets.follow_up_send.connect_clicked({
            let sender = sender.clone();
            let target = action_target.clone();
            move |_| {
                if let Some(id) = target.get() {
                    sender.input(TasksPanelMsg::SendFollowUp(id));
                }
            }
        });
        widgets.stream_page_button.connect_toggled({
            let sender = sender.clone();
            let page_guard = page_guard.clone();
            move |button| {
                if let Some(page) =
                    task_page_toggle_request(button.is_active(), page_guard.get(), STREAM_PAGE)
                {
                    sender.input(TasksPanelMsg::ShowPage(page));
                }
            }
        });
        widgets.diff_page_button.connect_toggled({
            let sender = sender.clone();
            let page_guard = page_guard.clone();
            move |button| {
                if let Some(page) =
                    task_page_toggle_request(button.is_active(), page_guard.get(), DIFF_PAGE)
                {
                    sender.input(TasksPanelMsg::ShowPage(page));
                }
            }
        });

        let mut dashboard_buttons = Vec::new();
        use crate::agent_task::DashboardView;
        for view in [
            DashboardView::All,
            DashboardView::NeedsAttention,
            DashboardView::Running,
            DashboardView::NotStarted,
            DashboardView::Review,
            DashboardView::Finished,
        ] {
            let button =
                gtk::Button::with_label(&dashboard_filter_label(view, &Default::default()));
            button.add_css_class("caption");
            let sender = sender.clone();
            button.connect_clicked(move |_| sender.input(TasksPanelMsg::DashboardView(view)));
            widgets.dashboard_filters.insert(&button, -1);
            dashboard_buttons.push((view, button));
        }
        let model = Self {
            action_buttons,
            dashboard_buttons,
            ..model
        };
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: TasksPanelMsg,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        // Requesting a diff also opens the diff page so the action has a
        // visible effect even while the worker is still loading.
        let show_diff = matches!(&message, TasksPanelMsg::Act(_, ActionKind::ReviewDiff));
        match message {
            TasksPanelMsg::HistoryStep(id, direction) => {
                if self.sync.detail.as_deref().map(|detail| detail.id) != Some(id) {
                    return;
                }
                if let Some(snapshot) = self
                    .sync
                    .detail
                    .as_deref()
                    .and_then(|detail| detail.stream.as_deref())
                {
                    if direction == 0 {
                        self.history_ordinal = None;
                    } else if let Some(index) = stream_history_index(snapshot, self.history_ordinal)
                    {
                        let next = if direction < 0 {
                            index.saturating_sub(1)
                        } else {
                            (index + 1).min(snapshot.turn_history.len() - 1)
                        };
                        self.history_ordinal = Some(snapshot.turn_history[next].ordinal);
                    }
                }
            }
            TasksPanelMsg::Sync(sync) => {
                let before = self.selected_status();
                self.unfiltered_sync = *sync;
                // A task that newly becomes selected outside the hidden list,
                // such as one just created or just restored, must never be
                // invisible: the hidden view gives way to it. The view also
                // closes once nothing hidden remains.
                let surfaced = match self.selected_status() {
                    (Some(id), Some(status)) if status != TaskStatus::Archived => {
                        before.0 != Some(id) || before.1 == Some(TaskStatus::Archived)
                    }
                    _ => false,
                };
                if self.filters.hidden && (surfaced || self.archived_count() == 0) {
                    self.filters.hidden = false;
                }
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::LeaveHidden => {
                if self.filters.hidden {
                    self.filters.hidden = false;
                    self.apply_dashboard_filters();
                }
            }
            TasksPanelMsg::ShowHidden(hidden) => {
                self.filters.hidden = hidden;
                self.attention_kind = None;
                self.dashboard_view = crate::agent_task::DashboardView::All;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::ClearDashboardFilters => {
                self.attention_kind = None;
                self.filters = TaskFilters::default();
                self.dashboard_query.clear();
                self.dashboard_view = crate::agent_task::DashboardView::All;
                self.dashboard_sort = crate::agent_task::DashboardSort::default();
                widgets.dashboard_query_entry.set_text("");
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::OpenDashboardResult => {
                self.apply_dashboard_filters();
                if let Some(row) = self.sync.rows.first() {
                    if self.sync.selected != Some(row.id) {
                        self.emit_action(TaskPanelAction::Select(row.id), &sender);
                    }
                }
            }
            TasksPanelMsg::DashboardQuery(query) => {
                self.dashboard_query = query;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::AttentionFilter(kind) => {
                self.attention_kind = kind;
                self.filters.status = None;
                self.dashboard_view = crate::agent_task::DashboardView::All;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::DashboardView(view) => {
                self.attention_kind = None;
                self.dashboard_view = view;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::DashboardSort(sort) => {
                self.dashboard_sort = sort;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::ProviderFilter(provider) => {
                self.filters.provider = provider;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::ProjectFilter(project) => {
                self.filters.workspace = project;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::StatusFilter(status) => {
                self.attention_kind = None;
                self.filters.status = status;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::FocusSelectedProject => {
                self.apply_dashboard_filters();
                if let Some(project) = TaskFilters::selected_project(
                    &self.sync.dashboard,
                    self.sync.selected,
                    &self.sync.workspaces,
                ) {
                    self.filters.workspace = Some(project);
                    self.apply_dashboard_filters();
                }
            }
            TasksPanelMsg::PreviousAttention => {
                self.apply_dashboard_filters();
                if let Some(id) = self.sync.dashboard.previous_attention(self.sync.selected) {
                    if Some(id) != self.sync.selected {
                        self.emit_action(TaskPanelAction::Select(id), &sender);
                    }
                }
            }
            TasksPanelMsg::NextAttention => {
                self.apply_dashboard_filters();
                if let Some(id) = self.sync.dashboard.next_attention(self.sync.selected) {
                    if Some(id) != self.sync.selected {
                        self.emit_action(TaskPanelAction::Select(id), &sender);
                    }
                }
            }
            TasksPanelMsg::SelectRow(id) => {
                if self.sync.selected != Some(id) && self.sync.rows.iter().any(|row| row.id == id) {
                    self.emit_action(TaskPanelAction::Select(id), &sender);
                }
            }
            TasksPanelMsg::FollowUpChanged(id, text) => {
                self.follow_up_drafts.insert(id, text);
            }
            TasksPanelMsg::FollowUpResolved(request, result) => {
                self.follow_up_receipts
                    .resolve(&mut self.follow_up_drafts, &request, result);
            }
            TasksPanelMsg::SendFollowUp(id) => {
                if let Some(detail) = self.sync.detail.as_deref().filter(|detail| detail.id == id) {
                    let text = self
                        .follow_up_drafts
                        .get(&detail.id)
                        .cloned()
                        .unwrap_or_default();
                    if native_follow_up_can_send(&text, detail.completed_turns)
                        && detail.can_follow_up
                    {
                        if let Some(request) = self.follow_up_receipts.begin(id, text) {
                            self.emit_action(TaskPanelAction::FollowUp(request), &sender);
                        }
                    }
                }
            }
            TasksPanelMsg::ComposeProject => {
                self.emit_action(TaskPanelAction::ComposeProject, &sender);
            }
            TasksPanelMsg::Create => {
                self.emit_action(TaskPanelAction::CreateFromBlock, &sender);
            }
            TasksPanelMsg::Act(id, kind) => {
                // Keep the clicked task even if a queued Sync changes selection.
                // The application/runtime rechecks this task and its live gates.
                let action = match kind {
                    ActionKind::StartNative => TaskPanelAction::StartNative(id),
                    ActionKind::RecentHistory => TaskPanelAction::LoadNativeHistory(id, true),
                    ActionKind::EarlierHistory => TaskPanelAction::LoadNativeHistory(id, false),
                    ActionKind::StartTerminal => TaskPanelAction::StartTerminal(id),
                    ActionKind::FocusTerminal => TaskPanelAction::FocusTerminal(id),
                    ActionKind::FocusValidation => TaskPanelAction::FocusValidation(id),
                    ActionKind::StopCodex => TaskPanelAction::StopCodex(id),
                    ActionKind::FinishCodex => TaskPanelAction::FinishCodex(id),
                    ActionKind::RunValidation => TaskPanelAction::RunValidation(id),
                    ActionKind::Complete => TaskPanelAction::Complete(id),
                    ActionKind::AcceptReviewed => TaskPanelAction::AcceptReviewed(id),
                    ActionKind::ReviewDiff => TaskPanelAction::ReviewDiff(id),
                    ActionKind::Archive => TaskPanelAction::Archive(id),
                    ActionKind::Restore => TaskPanelAction::Restore(id),
                };
                self.emit_action(action, &sender);
            }
            TasksPanelMsg::Decide(task_id, approval_id, approved) => {
                let action = if approved {
                    TaskPanelAction::Approve(task_id, approval_id)
                } else {
                    TaskPanelAction::Deny(task_id, approval_id)
                };
                self.emit_action(action, &sender);
            }
            TasksPanelMsg::ShowPage(page) => {
                self.page = task_page_presentation(page).0;
            }
            TasksPanelMsg::Close => {
                self.emit_action(TaskPanelAction::Close, &sender);
            }
        }
        if show_diff {
            self.page = DIFF_PAGE;
        }
        self.refresh_view(widgets, &sender);
    }
}

impl TasksPanelModel {
    fn refresh_view(
        &mut self,
        widgets: &mut TasksPanelModelWidgets,
        sender: &ComponentSender<Self>,
    ) {
        widgets
            .compose_button
            .set_sensitive(self.sync.create_enabled && !self.sync.pending_creation);
        widgets
            .create_button
            .set_sensitive(self.sync.create_enabled && !self.sync.pending_creation);
        let show_hint = !self.sync.create_hint.is_empty();
        widgets.create_hint.set_visible(show_hint);
        if show_hint {
            widgets.create_hint.set_label(&self.sync.create_hint);
        }

        let guard = self.dashboard_sort_guard.replace(true);
        let choices = self
            .filters
            .workspace_choices(&self.unfiltered_sync.workspaces);
        if *self.workspace_choices.borrow() != choices {
            let labels = std::iter::once("All projects".to_string())
                .chain(choices.iter().map(|path| {
                    jterm_core::review_input::safe_inline_display(&path.to_string_lossy(), 160)
                }))
                .collect::<Vec<_>>();
            widgets
                .workspace_filter
                .set_model(Some(&gtk::StringList::new(
                    &labels.iter().map(String::as_str).collect::<Vec<_>>(),
                )));
            *self.workspace_choices.borrow_mut() = choices;
        }
        widgets.provider_filter.set_selected(
            self.filters
                .provider
                .and_then(|provider| {
                    crate::agent_task::AgentProvider::ALL
                        .iter()
                        .position(|candidate| *candidate == provider)
                })
                .map_or(0, |index| index as u32 + 1),
        );
        widgets.workspace_filter.set_selected(
            self.filters
                .workspace
                .as_ref()
                .and_then(|path| {
                    self.workspace_choices
                        .borrow()
                        .iter()
                        .position(|candidate| candidate == path)
                })
                .map_or(0, |index| index as u32 + 1),
        );
        widgets.status_filter.set_selected(
            self.filters
                .status
                .and_then(|status| STATES.iter().position(|candidate| *candidate == status))
                .map_or(0, |index| index as u32 + 1),
        );
        let attention_labels = std::iter::once("All attention reasons".to_string())
            .chain(
                crate::agent_task::DashboardAttentionKind::ALL
                    .iter()
                    .enumerate()
                    .map(|(index, kind)| {
                        format!(
                            "{} · {}",
                            kind.label(),
                            self.attention_counts.get(index).copied().unwrap_or(0)
                        )
                    }),
            )
            .collect::<Vec<_>>();
        let labels_changed = widgets
            .attention_filter
            .model()
            .and_downcast::<gtk::StringList>()
            .is_none_or(|list| {
                list.n_items() as usize != attention_labels.len()
                    || attention_labels.iter().enumerate().any(|(index, label)| {
                        list.string(index as u32).as_deref() != Some(label.as_str())
                    })
            });
        if labels_changed {
            widgets
                .attention_filter
                .set_model(Some(&gtk::StringList::new(
                    &attention_labels
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                )));
        }
        widgets.attention_filter.set_selected(
            self.attention_kind
                .and_then(|kind| {
                    crate::agent_task::DashboardAttentionKind::ALL
                        .iter()
                        .position(|candidate| *candidate == kind)
                })
                .map_or(0, |index| index as u32 + 1),
        );
        self.dashboard_sort_guard.set(guard);
        let query_error = crate::agent_task::dashboard_query_error(&self.dashboard_query);
        widgets
            .dashboard_query_error
            .set_visible(query_error.is_some());
        widgets
            .dashboard_query_error
            .set_label(query_error.as_deref().unwrap_or_default());
        if query_error.is_some() {
            widgets.dashboard_query_entry.add_css_class("error");
        } else {
            widgets.dashboard_query_entry.remove_css_class("error");
        }
        let active_scope = usize::from(self.filters.provider.is_some())
            + usize::from(self.filters.workspace.is_some())
            + usize::from(self.filters.status.is_some());
        let scope_label = if active_scope == 0 {
            "Provider, project and state".to_string()
        } else {
            format!("Filters · {active_scope} active")
        };
        widgets.scope_filters.set_label(Some(&scope_label));
        widgets.dashboard_summary.set_label(&self.dashboard_summary);
        widgets.dashboard_clear.set_visible(
            self.filters != TaskFilters::default()
                || self.attention_kind.is_some()
                || !self.dashboard_query.is_empty()
                || self.dashboard_view != crate::agent_task::DashboardView::All
                || self.dashboard_sort != crate::agent_task::DashboardSort::default(),
        );
        let sort_index = crate::agent_task::DashboardSort::ALL
            .iter()
            .position(|sort| *sort == self.dashboard_sort)
            .unwrap_or(0) as u32;
        if widgets.dashboard_sort.selected() != sort_index {
            let previous = self.dashboard_sort_guard.replace(true);
            widgets.dashboard_sort.set_selected(sort_index);
            self.dashboard_sort_guard.set(previous);
        }
        widgets.dashboard_focus_project.set_sensitive(
            TaskFilters::selected_project(
                &self.sync.dashboard,
                self.sync.selected,
                &self.sync.workspaces,
            )
            .is_some(),
        );
        widgets.previous_attention.set_sensitive(
            self.sync
                .dashboard
                .previous_attention(self.sync.selected)
                .is_some(),
        );
        widgets.next_attention.set_sensitive(
            self.sync
                .dashboard
                .next_attention(self.sync.selected)
                .is_some(),
        );
        let hidden = self.sync.selected.is_some_and(|id| {
            self.unfiltered_sync
                .dashboard
                .rows
                .iter()
                .any(|row| row.id == id && self.in_archive_scope(row))
                && !self.sync.rows.iter().any(|row| row.id == id)
        });
        widgets.dashboard_reveal.set_visible(hidden);
        let archived = self.archived_count();
        let previous = self.dashboard_sort_guard.replace(true);
        widgets
            .hidden_toggle
            .set_visible(archived > 0 || self.filters.hidden);
        widgets
            .hidden_toggle
            .set_label(&format!("Hidden tasks ({archived})"));
        if widgets.hidden_toggle.is_active() != self.filters.hidden {
            widgets.hidden_toggle.set_active(self.filters.hidden);
        }
        self.dashboard_sort_guard.set(previous);
        for (view, button) in &self.dashboard_buttons {
            let label = dashboard_filter_label(*view, &self.dashboard_counts);
            if button.label().as_deref() != Some(label.as_str()) {
                button.set_label(&label);
            }
            if *view == self.dashboard_view && self.attention_kind.is_none() {
                button.add_css_class("suggested-action");
            } else {
                button.remove_css_class("suggested-action");
            }
        }
        self.sync_task_list(widgets);

        let detail = self.sync.detail.as_deref();
        if self.history_task != detail.map(|detail| detail.id) {
            self.history_task = detail.map(|detail| detail.id);
            self.history_ordinal = None;
        }
        if self.action_target.get() != detail.map(|detail| detail.id) {
            for (_, button) in &self.action_buttons {
                cancel_task_action_activation(button);
            }
            cancel_task_action_activation(&widgets.follow_up_send);
            if let Some(adjustment) = widgets.detail_viewport.vadjustment() {
                adjustment.set_value(0.0);
            }
        }
        self.action_target.set(detail.map(|detail| detail.id));
        let primary = detail.and_then(|detail| {
            primary_task_action(detail).filter(|(kind, _)| {
                if *kind != ActionKind::FinishCodex {
                    return true;
                }
                let has_draft = self
                    .follow_up_drafts
                    .get(&detail.id)
                    .is_some_and(|draft| !draft.trim().is_empty());
                !has_draft && !self.follow_up_receipts.is_pending(detail.id)
            })
        });
        let primary_target = detail
            .zip(primary.as_ref())
            .map(|(detail, (kind, _))| (detail.id, *kind));
        if self.primary_target.get() != primary_target {
            cancel_task_action_activation(&widgets.primary_action);
            self.primary_target.set(primary_target);
        }
        widgets.primary_action.set_visible(primary.is_some());
        widgets.primary_action.set_sensitive(primary.is_some());
        if let Some((_, label)) = primary {
            widgets.primary_action.set_label(&label);
            widgets
                .primary_action
                .update_property(&[gtk::accessible::Property::Label(&label)]);
        }
        widgets.next_step_card.set_visible(detail.is_some());
        if let Some(detail) = detail {
            widgets.next_step_title.set_label(detail.guidance.title);
            widgets.next_step_detail.set_label(detail.guidance.detail);
            widgets
                .next_step_detail
                .set_tooltip_text(Some(detail.guidance.detail));
            widgets
                .next_step_reason
                .set_visible(detail.guidance.reason.is_some());
            widgets
                .next_step_reason
                .set_label(detail.guidance.reason.as_deref().unwrap_or_default());
            widgets
                .next_step_reason
                .set_tooltip_text(detail.guidance.reason.as_deref());
        }
        for (kind, button) in &self.action_buttons {
            let sensitive = detail.is_some_and(|detail| {
                match kind {
                    ActionKind::StartNative => detail.can_start_native,
                    ActionKind::RecentHistory => {
                        detail.provider == crate::agent_task::AgentProvider::Codex
                            && detail.can_finish
                            && detail
                                .stream
                                .as_ref()
                                .is_some_and(|view| !view.history_loading)
                    }
                    ActionKind::EarlierHistory => {
                        detail.provider == crate::agent_task::AgentProvider::Codex
                            && detail.can_finish
                            && detail.stream.as_ref().is_some_and(|view| {
                                !view.history_loading && view.history_cursor.is_some()
                            })
                    }
                    ActionKind::StartTerminal => detail.can_start_terminal,
                    ActionKind::FocusTerminal => detail.can_focus_terminal,
                    ActionKind::FocusValidation => detail.can_focus_validation,
                    ActionKind::StopCodex => detail.can_stop,
                    ActionKind::FinishCodex => detail.can_finish,
                    ActionKind::RunValidation => detail.can_run_validation,
                    ActionKind::Complete => detail.can_complete,
                    ActionKind::AcceptReviewed => detail.can_accept_review,
                    ActionKind::ReviewDiff => true,
                    // Offered only when the manager would accept it now.
                    ActionKind::Archive => detail.can_archive,
                    ActionKind::Restore => detail.can_restore,
                }
            });
            button.set_sensitive(sensitive);
            if *kind == ActionKind::Archive {
                button.set_tooltip_text(Some(
                    detail
                        .and_then(|detail| detail.archive_note.as_deref())
                        .unwrap_or("Hide this task; its worktree is unchanged and it can be restored from Hidden tasks"),
                ));
            }
            let visible = detail.is_some_and(|detail| match kind {
                ActionKind::RecentHistory | ActionKind::EarlierHistory => {
                    detail.provider == crate::agent_task::AgentProvider::Codex
                        && detail.stream.is_some()
                        && !detail.archived
                }
                ActionKind::Complete => !detail.manual_review && !detail.archived,
                ActionKind::AcceptReviewed => detail.manual_review && !detail.archived,
                ActionKind::Archive => !detail.archived,
                ActionKind::Restore => detail.archived,
                // A hidden task keeps only read-only inspection.
                ActionKind::ReviewDiff
                | ActionKind::FocusTerminal
                | ActionKind::FocusValidation => true,
                _ => !detail.archived,
            });
            button.set_visible(visible);
            if let Some(holder) = button.parent().and_downcast::<gtk::FlowBoxChild>() {
                holder.set_visible(visible);
            }
            if *kind == ActionKind::StartNative {
                if let Some(detail) = detail {
                    button.set_label(&detail.native_label);
                }
            }
        }
        widgets.actions_expander.set_visible(detail.is_some());
        widgets.detail_title.set_visible(detail.is_some());
        widgets.detail_status.set_visible(detail.is_some());
        widgets.page_stack.set_visible(detail.is_some());
        widgets.inspector_page_button.set_visible(detail.is_some());
        widgets.stream_page_button.set_visible(detail.is_some());
        widgets.diff_page_button.set_visible(detail.is_some());
        widgets
            .follow_up_row
            .set_visible(detail.is_some_and(|d| d.can_follow_up));
        if detail.is_none() {
            widgets.follow_up_hint.set_visible(false);
            widgets.follow_up_send.set_sensitive(false);
        }

        if let Some(detail) = detail {
            set_view_text(&widgets.inspector_view, &detail.inspector);
            widgets.detail_title.set_label(&detail.title);
            widgets
                .detail_status
                .set_label(&format!("{} · {}", detail.status_line, detail.branch));

            let stream_text = detail
                .stream
                .as_deref()
                .map(render_stream_text)
                .unwrap_or_else(|| "No native session snapshot for this task yet.".to_string());
            set_view_text(&widgets.stream_view, &stream_text);
            let snapshot = detail.stream.as_deref();
            widgets
                .stream_status
                .set_label(&snapshot.map(render_stream_status).unwrap_or_default());
            let index =
                snapshot.and_then(|snapshot| stream_history_index(snapshot, self.history_ordinal));
            widgets
                .history_older
                .set_sensitive(index.is_some_and(|index| index > 0));
            widgets.history_newer.set_sensitive(
                snapshot
                    .zip(index)
                    .is_some_and(|(snapshot, index)| index + 1 < snapshot.turn_history.len()),
            );
            widgets
                .history_latest
                .set_sensitive(self.history_ordinal.is_some());
            let history = snapshot
                .map(|snapshot| render_stream_history(snapshot, self.history_ordinal))
                .unwrap_or_default();
            widgets.history_scroll.set_visible(!history.is_empty());
            set_view_text(&widgets.history_view, &history);

            if let Some(diff) = &detail.diff {
                widgets.diff_header.set_label(&diff.header);
                widgets.diff_header.set_tooltip_text(Some(&diff.scope));
                let text = if diff.loading {
                    "Loading tracked changes…".to_string()
                } else if let Some(error) = &diff.error {
                    error.clone()
                } else if diff.text.is_empty() {
                    "No tracked changes.".to_string()
                } else {
                    let mut text = diff.text.clone();
                    if diff.truncated {
                        text.push_str("\n(diff exceeded the retained limits; output truncated)");
                    }
                    text
                };
                set_view_text(&widgets.diff_view, &text);
            } else {
                widgets
                    .diff_header
                    .set_label("Use Diff to review this task's worktree changes.");
                set_view_text(&widgets.diff_view, "");
            }

            widgets.follow_up_row.set_visible(detail.can_follow_up);
            widgets.follow_up_entry.set_tooltip_text(Some(&format!(
                "Follow-up turn for {} · Enter for a new line · Ctrl+Enter to send",
                detail.provider.display_name()
            )));
            let error = self.follow_up_receipts.error(detail.id);
            let hint = error.unwrap_or(&detail.follow_up_hint);
            widgets.follow_up_hint.set_visible(!hint.is_empty());
            widgets.follow_up_hint.set_label(hint);
            widgets.follow_up_hint.set_tooltip_text(error);
            if error.is_some() {
                widgets.follow_up_hint.add_css_class("error");
            } else {
                widgets.follow_up_hint.remove_css_class("error");
            }
            let draft = self
                .follow_up_drafts
                .get(&detail.id)
                .cloned()
                .unwrap_or_default();
            if widgets
                .follow_up_entry
                .buffer()
                .text(
                    &widgets.follow_up_entry.buffer().start_iter(),
                    &widgets.follow_up_entry.buffer().end_iter(),
                    true,
                )
                .as_str()
                != draft
            {
                // Replacing nonempty Entry text emits delete/insert notifications.
                // View synchronization must never enqueue another authored edit.
                let guard = self.follow_up_guard.replace(true);
                widgets.follow_up_entry.buffer().set_text(&draft);
                self.follow_up_guard.set(guard);
            }
            widgets.follow_up_send.set_sensitive(
                detail.can_follow_up
                    && !self.follow_up_receipts.is_pending(detail.id)
                    && native_follow_up_can_send(&draft, detail.completed_turns),
            );
        }

        let (page, stream_active, diff_active) = task_page_presentation(self.page);
        self.page_guard.set(true);
        widgets.stream_page_button.set_active(stream_active);
        widgets.diff_page_button.set_active(diff_active);
        self.page_guard.set(false);
        widgets.page_stack.set_visible_child_name(page);
        self.sync_approvals(widgets, sender);
    }
}

/// Set a read-only text view's buffer only when the content actually
/// changed; re-setting identical text still forces a fresh layout pass.
// GTK's delayed keyboard activation survives a sensitivity toggle. Reparenting
// cancels that pending activation before a button can acquire another task.
fn cancel_task_action_activation(button: &gtk::Button) {
    button.set_sensitive(false);
    if let Some(holder) = button.parent().and_downcast::<gtk::FlowBoxChild>() {
        holder.set_child(None::<&gtk::Widget>);
        holder.set_child(Some(button));
    } else if let Some(parent) = button.parent().and_downcast::<gtk::Box>() {
        let previous = button.prev_sibling();
        parent.remove(button);
        parent.insert_child_after(button, previous.as_ref());
    }
}

fn set_view_text(view: &gtk::TextView, text: &str) {
    let buffer = view.buffer();
    if buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .as_str()
        != text
    {
        buffer.set_text(text);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dashboard_filter_labels_map_activity_and_finished_counts() {
        use crate::agent_task::{DashboardCounts, DashboardView};
        let counts = DashboardCounts {
            total: 20,
            attention: 7,
            running: 9,
            created: 3,
            review: 4,
            done: 2,
            failed: 5,
            cancelled: 1,
            archived: 99,
            ..Default::default()
        };
        for (view, expected) in [
            (DashboardView::All, "All 20"),
            (DashboardView::NeedsAttention, "Needs you 7"),
            (DashboardView::Running, "Running 9"),
            (DashboardView::NotStarted, "Not started 3"),
            (DashboardView::Review, "Review 4"),
            (DashboardView::Finished, "Finished 8"),
        ] {
            assert_eq!(super::dashboard_filter_label(view, &counts), expected);
        }
        assert_eq!(
            super::dashboard_filter_label(DashboardView::Running, &DashboardCounts::default()),
            "Running 0"
        );
    }

    use super::*;

    use super::{task_page_presentation, task_page_toggle_request, DIFF_PAGE, STREAM_PAGE};

    #[test]
    fn task_pages_follow_each_request_with_exclusive_button_state() {
        for requested in [STREAM_PAGE, DIFF_PAGE, STREAM_PAGE, DIFF_PAGE, STREAM_PAGE] {
            let (page, stream, diff) = task_page_presentation(requested);
            assert_eq!(page, requested);
            assert_ne!(stream, diff);
            assert_eq!(stream, page == STREAM_PAGE);
            assert_eq!(diff, page == DIFF_PAGE);
        }
    }

    #[test]
    fn repeated_and_unknown_page_requests_have_a_stable_safe_result() {
        assert_eq!(task_page_presentation(DIFF_PAGE), (DIFF_PAGE, false, true));
        assert_eq!(task_page_presentation(DIFF_PAGE), (DIFF_PAGE, false, true));
        assert_eq!(
            task_page_presentation("unknown"),
            (STREAM_PAGE, true, false)
        );
    }

    #[test]
    fn queued_page_requests_finish_without_programmatic_feedback() {
        let mut queue = std::collections::VecDeque::from([DIFF_PAGE, STREAM_PAGE]);
        let mut final_page = STREAM_PAGE;
        let mut processed = 0;
        while let Some(requested) = queue.pop_front() {
            processed += 1;
            assert!(
                processed <= 2,
                "programmatic changes must not requeue old intent"
            );
            let (page, stream, diff) = task_page_presentation(requested);
            final_page = page;
            for (active, page) in [(stream, STREAM_PAGE), (diff, DIFF_PAGE)] {
                if let Some(request) = task_page_toggle_request(active, true, page) {
                    queue.push_back(request);
                }
            }
        }
        assert_eq!(final_page, STREAM_PAGE);
        assert_eq!(processed, 2);
        assert_eq!(task_page_toggle_request(false, false, DIFF_PAGE), None);
        assert_eq!(
            task_page_toggle_request(true, false, DIFF_PAGE),
            Some(DIFF_PAGE)
        );
    }
    pub(super) fn dashboard_test_sync() -> TasksPanelSync {
        use crate::agent_task::{AgentProvider, NewTask, TaskManager, TaskStatus};
        let mut manager = TaskManager::new();
        for title in ["First task", "Needs review"] {
            manager
                .create(NewTask {
                    title: title.into(),
                    provider: AgentProvider::Codex,
                    repo_root: "/fixture".into(),
                    worktree_path: "/fixture/worktree".into(),
                    branch: "test/dashboard".into(),
                    base_commit: "a".repeat(40),
                    source_context: None,
                })
                .unwrap();
        }
        let mut tasks = manager.tasks().to_vec();
        tasks[1].status = TaskStatus::Failed;
        let selected = tasks[0].id;
        let dashboard = crate::agent_task::DashboardSnapshot::project(
            &tasks,
            |_| Default::default(),
            Default::default(),
        );
        let rows = tasks
            .iter()
            .map(|task| TaskRowSnapshot {
                id: task.id,
                title: task.title.clone(),
                provider: task.provider,
                status: task.status,
                runtime_kind: task.runtime_kind,
                branch: task.branch.clone(),
                has_agent_terminal: false,
                has_validation_terminal: false,
                has_active_agent_stream: false,
                native_preparing: false,
                validation_status: task.validation.status,
                validation_attempt: 0,
                needs_attention: task.needs_attention(),
                status_detail: None,
                validation_detail: None,
            })
            .collect();
        let detail = TaskDetailSync {
            id: selected,
            provider: crate::agent_task::AgentProvider::Codex,
            archived: false,
            can_archive: true,
            archive_note: None,
            can_restore: false,
            title: "First task".into(),
            status_line: "Created".into(),
            branch: "test/dashboard".into(),
            inspector: "Repository\n/tmp/project".into(),
            guidance: crate::agent_task::TaskGuidance::from_task(&tasks[0], Default::default()),
            validation_status: tasks[0].validation.status,
            native_label: "Start Codex".into(),
            manual_review: false,
            can_accept_review: false,
            stream: None,
            approvals: Vec::new(),
            completed_turns: 0,
            can_start_native: false,
            can_start_terminal: false,
            can_focus_terminal: false,
            can_focus_validation: false,
            can_stop: false,
            can_finish: false,
            can_run_validation: false,
            can_complete: false,
            can_follow_up: false,
            follow_up_hint: String::new(),
            diff: None,
        };
        TasksPanelSync {
            workspaces: tasks
                .iter()
                .map(|task| (task.id, task.repo_root.clone()))
                .collect(),
            rows,
            dashboard,
            selected: Some(selected),
            detail: Some(Box::new(detail)),
            ..Default::default()
        }
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn hidden_tasks_offer_only_restore_and_hide_follows_manager_gates() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::builder()
            .default_width(520)
            .default_height(920)
            .build();
        window.set_child(Some(controller.widget()));
        window.present();
        let drain = || {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
        };
        let action_button = |kind| {
            controller
                .model()
                .action_buttons
                .iter()
                .find(|(candidate, _)| *candidate == kind)
                .unwrap()
                .1
                .clone()
        };
        let mut sync = dashboard_test_sync();
        let visible = sync.selected.unwrap();
        let hidden_id = sync.rows[1].id;
        // Hide the second task the way the domain projection reports it.
        sync.rows[1].status = crate::agent_task::TaskStatus::Archived;
        sync.dashboard
            .rows
            .iter_mut()
            .find(|row| row.id == hidden_id)
            .unwrap()
            .status = crate::agent_task::TaskStatus::Archived;
        // A running task cannot be hidden: the button explains why.
        {
            let detail = sync.detail.as_mut().unwrap();
            detail.can_archive = false;
            detail.archive_note = Some("Stop the Agent first".into());
        }
        controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
        controller.widgets().actions_expander.set_expanded(true);
        drain();
        assert_eq!(
            controller
                .model()
                .sync
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [visible],
            "ordinary views exclude hidden tasks"
        );
        let hidden_toggle = controller.widgets().hidden_toggle.clone();
        assert!(hidden_toggle.is_visible());
        assert_eq!(hidden_toggle.label().as_deref(), Some("Hidden tasks (1)"));
        let hide = action_button(ActionKind::Archive);
        assert!(hide.is_visible() && !hide.is_sensitive());
        assert_eq!(hide.tooltip_text().as_deref(), Some("Stop the Agent first"));
        assert!(!action_button(ActionKind::Restore).is_visible());

        // Open the hidden view and inspect the hidden task.
        hidden_toggle.set_active(true);
        drain();
        assert!(controller.model().filters.hidden);
        assert_eq!(
            controller
                .model()
                .sync
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [hidden_id]
        );
        let mut hidden_sync = sync.clone();
        hidden_sync.selected = Some(hidden_id);
        {
            let detail = hidden_sync.detail.as_mut().unwrap();
            detail.id = hidden_id;
            detail.archived = true;
            detail.can_archive = false;
            detail.archive_note = None;
            detail.can_restore = true;
            detail.can_start_terminal = true;
            detail.can_run_validation = true;
        }
        controller.emit(TasksPanelMsg::Sync(Box::new(hidden_sync.clone())));
        drain();
        let restore = action_button(ActionKind::Restore);
        assert!(restore.is_visible() && restore.is_sensitive());
        assert!(!action_button(ActionKind::Archive).is_visible());
        for kind in [ActionKind::StartTerminal, ActionKind::RunValidation] {
            assert!(
                !action_button(kind).is_visible(),
                "a hidden task offers no lifecycle action: {kind:?}"
            );
        }
        assert!(action_button(ActionKind::ReviewDiff).is_visible());
        assert_eq!(
            primary_task_action(hidden_sync.detail.as_ref().unwrap()),
            Some((ActionKind::Restore, "Restore task".into()))
        );
        restore.emit_clicked();
        drain();
        assert!(matches!(
            actions.borrow().last(),
            Some(TaskPanelAction::Restore(id)) if *id == hidden_id
        ));

        // A task newly selected outside the hidden list (one just created,
        // say) is never left invisible: the hidden view gives way to it.
        let mut created = sync.clone();
        created.selected = Some(visible);
        controller.emit(TasksPanelMsg::Sync(Box::new(created)));
        drain();
        assert!(!controller.model().filters.hidden);
        assert!(!hidden_toggle.is_active());
        assert!(controller
            .model()
            .sync
            .rows
            .iter()
            .any(|row| row.id == visible));
        // Opening the panel returns to the ordinary views as well.
        hidden_toggle.set_active(true);
        drain();
        assert!(controller.model().filters.hidden);
        controller.emit(TasksPanelMsg::LeaveHidden);
        drain();
        assert!(!controller.model().filters.hidden);
        assert!(!hidden_toggle.is_active());
        hidden_toggle.set_active(true);
        controller.emit(TasksPanelMsg::Sync(Box::new(hidden_sync.clone())));
        drain();
        assert!(controller.model().filters.hidden, "browsing hidden tasks");

        // Restoring the selected task (here the last hidden one) shows it
        // in the ordinary views.
        let mut restored = sync.clone();
        restored.rows[1].status = crate::agent_task::TaskStatus::Failed;
        restored
            .dashboard
            .rows
            .iter_mut()
            .find(|row| row.id == hidden_id)
            .unwrap()
            .status = crate::agent_task::TaskStatus::Failed;
        restored.selected = Some(hidden_id);
        controller.emit(TasksPanelMsg::Sync(Box::new(restored)));
        drain();
        assert!(!controller.model().filters.hidden);
        assert!(!controller.widgets().hidden_toggle.is_visible());
        assert!(controller
            .model()
            .sync
            .rows
            .iter()
            .any(|row| row.id == hidden_id));
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn agent_management_v3_inspector_and_manual_review_keep_task_identity() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::builder()
            .default_width(520)
            .default_height(920)
            .build();
        window.set_child(Some(controller.widget()));
        window.present();
        let drain = || {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
        };
        let mut sync = dashboard_test_sync();
        let selected = sync.selected.unwrap();
        let detail = sync.detail.as_mut().unwrap();
        detail.manual_review = true;
        detail.can_accept_review = true;
        detail.native_label = "Start Claude".into();
        detail.inspector = "Provider\nClaude\n\nRepository\n/tmp/project\n\nGoal\nReview the work before accepting it".into();
        controller.emit(TasksPanelMsg::Sync(Box::new(sync)));
        controller.widgets().actions_expander.set_expanded(true);
        drain();
        let action_button = |kind| {
            controller
                .model()
                .action_buttons
                .iter()
                .find(|(candidate, _)| *candidate == kind)
                .unwrap()
                .1
                .clone()
        };
        let accept = action_button(ActionKind::AcceptReviewed);
        assert!(accept.is_visible() && accept.is_sensitive());
        assert!(!action_button(ActionKind::Complete).is_visible());
        assert_eq!(
            action_button(ActionKind::StartNative).label().as_deref(),
            Some("Start Claude")
        );
        controller.widgets().inspector_page_button.emit_clicked();
        drain();
        assert_eq!(
            controller
                .widgets()
                .page_stack
                .visible_child_name()
                .as_deref(),
            Some(INSPECTOR_PAGE)
        );
        assert!(!controller.widgets().inspector_view.is_editable());
        assert!(
            actions.borrow().is_empty(),
            "reading details does not operate a task"
        );
        if let Some(path) = std::env::var_os("JTERM_TASK_INSPECTOR_SCREENSHOT") {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(150);
            while std::time::Instant::now() < deadline {
                drain();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(std::process::Command::new("import")
                .args(["-window", "root"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }
        accept.emit_clicked();
        drain();
        assert_eq!(
            actions.borrow().as_slice(),
            &[TaskPanelAction::AcceptReviewed(selected)]
        );
        actions.borrow_mut().clear();
        accept.emit_by_name::<()>("activate", &[]);
        let mut replacement = dashboard_test_sync();
        replacement.detail.as_mut().unwrap().manual_review = true;
        replacement.detail.as_mut().unwrap().can_accept_review = true;
        controller.emit(TasksPanelMsg::Sync(Box::new(replacement)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(320);
        while std::time::Instant::now() < deadline {
            drain();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            actions.borrow().is_empty(),
            "a delayed review action cannot accept a newly selected task"
        );
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn queued_task_controls_keep_the_rendered_task_across_sync() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let drain = || {
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while main.pending() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "bounded component drain"
                );
                main.iteration(false);
            }
        };
        for kind in [ActionKind::StopCodex, ActionKind::FinishCodex] {
            let mut first = dashboard_test_sync();
            let clicked = first.selected.unwrap();
            first.detail.as_mut().unwrap().can_stop = true;
            first.detail.as_mut().unwrap().can_finish = true;
            controller.emit(TasksPanelMsg::Sync(Box::new(first)));
            drain();
            actions.borrow_mut().clear();
            let button = controller
                .model()
                .action_buttons
                .iter()
                .find(|(candidate, _)| *candidate == kind)
                .unwrap()
                .1
                .clone();
            // Queue a replacement snapshot without rendering it yet. The
            // visible button still belongs to the first task when clicked.
            controller.emit(TasksPanelMsg::Sync(Box::new(dashboard_test_sync())));
            button.emit_clicked();
            drain();
            assert_ne!(controller.model().sync.selected, Some(clicked));
            assert_eq!(actions.borrow().len(), 1);
            match (&actions.borrow()[0], kind) {
                (TaskPanelAction::StopCodex(id), ActionKind::StopCodex)
                | (TaskPanelAction::FinishCodex(id), ActionKind::FinishCodex) => {
                    assert_eq!(*id, clicked);
                }
                _ => panic!("wrong task control"),
            }
        }
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn task_reason_updates_and_disappears_without_retargeting() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::new();
        window.set_child(Some(controller.widget()));
        let drain = || {
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while main.pending() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "bounded component drain"
                );
                main.iteration(false);
            }
        };
        let find_reason = || {
            let mut pending = vec![controller
                .widgets()
                .task_list
                .clone()
                .upcast::<gtk::Widget>()];
            while let Some(widget) = pending.pop() {
                if widget.has_css_class("task-reason") {
                    return Some(widget.downcast::<gtk::Label>().unwrap());
                }
                let mut child = widget.first_child();
                while let Some(widget) = child {
                    child = widget.next_sibling();
                    pending.push(widget);
                }
            }
            None
        };
        let mut sync = dashboard_test_sync();
        let selected = sync.selected.unwrap();
        let row = sync.rows.iter_mut().find(|row| row.id == selected).unwrap();
        row.status = crate::agent_task::TaskStatus::ReadyForReview;
        row.status_detail = Some("task reason".into());
        row.validation_status = crate::agent_task::TaskValidationStatus::Failed;
        row.validation_detail = Some("validation old".into());
        let row = sync
            .dashboard
            .rows
            .iter_mut()
            .find(|row| row.id == selected)
            .unwrap();
        row.status = crate::agent_task::TaskStatus::ReadyForReview;
        row.status_detail = Some("task reason".into());
        row.validation_status = crate::agent_task::TaskValidationStatus::Failed;
        controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
        drain();
        let reason = find_reason().unwrap();
        assert_eq!(reason.label(), "validation old");
        assert_eq!(reason.tooltip_text().as_deref(), Some("validation old"));
        assert_eq!(reason.ellipsize(), gtk::pango::EllipsizeMode::End);
        assert!(reason.is_single_line_mode());
        sync.rows
            .iter_mut()
            .find(|row| row.id == selected)
            .unwrap()
            .validation_detail = Some("updated".into());
        controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
        drain();
        assert_eq!(find_reason().unwrap().label(), "updated");
        sync.rows
            .iter_mut()
            .find(|row| row.id == selected)
            .unwrap()
            .validation_detail = None;
        sync.rows
            .iter_mut()
            .find(|row| row.id == selected)
            .unwrap()
            .status_detail = None;
        sync.dashboard
            .rows
            .iter_mut()
            .find(|row| row.id == selected)
            .unwrap()
            .status_detail = None;
        controller.emit(TasksPanelMsg::Sync(Box::new(sync)));
        drain();
        assert!(
            find_reason().is_none(),
            "old preview and tooltip must disappear"
        );
        assert_eq!(controller.model().sync.selected, Some(selected));
        assert!(actions.borrow().is_empty());
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn task_terminal_buttons_only_emit_focus_for_existing_bindings() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::new();
        window.set_child(Some(controller.widget()));
        let drain = || {
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while main.pending() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "bounded component drain"
                );
                main.iteration(false);
            }
        };
        let mut sync = dashboard_test_sync();
        let id = sync.selected.unwrap();
        sync.detail.as_mut().unwrap().can_focus_terminal = true;
        sync.detail.as_mut().unwrap().can_focus_validation = true;
        controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
        drain();
        for kind in [ActionKind::FocusTerminal, ActionKind::FocusValidation] {
            let button = controller
                .model()
                .action_buttons
                .iter()
                .find(|(candidate, _)| *candidate == kind)
                .unwrap()
                .1
                .clone();
            assert!(button.is_sensitive());
            button.emit_clicked();
            drain();
        }
        assert_eq!(
            *actions.borrow(),
            [
                TaskPanelAction::FocusTerminal(id),
                TaskPanelAction::FocusValidation(id)
            ]
        );
        assert_eq!(controller.model().sync.selected, Some(id));
        assert_eq!(controller.model().unfiltered_sync.rows, sync.rows);
        sync.detail.as_mut().unwrap().can_focus_terminal = false;
        sync.detail.as_mut().unwrap().can_focus_validation = false;
        controller.emit(TasksPanelMsg::Sync(Box::new(sync)));
        drain();
        assert!(controller
            .model()
            .action_buttons
            .iter()
            .filter(|(kind, _)| matches!(
                kind,
                ActionKind::FocusTerminal | ActionKind::FocusValidation
            ))
            .all(|(_, button)| !button.is_sensitive()));
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn selected_project_focus_keeps_query_identity_and_rejects_hidden_selection() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::new();
        window.set_child(Some(controller.widget()));
        let drain = || {
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while main.pending() {
                assert!(std::time::Instant::now() < deadline);
                main.iteration(false);
            }
        };
        let mut sync = dashboard_test_sync();
        let selected = sync.selected.unwrap();
        sync.workspaces = sync
            .rows
            .iter()
            .map(|row| {
                (
                    row.id,
                    std::path::PathBuf::from(if row.id == selected {
                        "/repo/selected"
                    } else {
                        "/repo/other"
                    }),
                )
            })
            .collect();
        controller.emit(TasksPanelMsg::Sync(Box::new(sync)));
        controller.emit(TasksPanelMsg::DashboardQuery("provider:codex".into()));
        controller.emit(TasksPanelMsg::ProviderFilter(Some(
            crate::agent_task::AgentProvider::Codex,
        )));
        drain();
        assert!(controller.widgets().dashboard_focus_project.is_sensitive());
        controller.emit(TasksPanelMsg::FocusSelectedProject);
        drain();
        assert_eq!(
            controller.model().filters.workspace.as_deref(),
            Some(std::path::Path::new("/repo/selected"))
        );
        assert_eq!(controller.model().dashboard_query, "provider:codex");
        assert_eq!(
            controller.model().filters.provider,
            Some(crate::agent_task::AgentProvider::Codex)
        );
        assert_eq!(controller.model().sync.selected, Some(selected));
        assert_eq!(controller.model().sync.rows.len(), 1);
        assert!(
            actions.borrow().is_empty(),
            "view focus cannot operate a task"
        );
        controller.emit(TasksPanelMsg::ClearDashboardFilters);
        controller.emit(TasksPanelMsg::DashboardView(
            crate::agent_task::DashboardView::Finished,
        ));
        drain();
        assert!(!controller.widgets().dashboard_focus_project.is_sensitive());
        controller.emit(TasksPanelMsg::FocusSelectedProject);
        drain();
        assert!(controller.model().filters.workspace.is_none());
        assert_eq!(controller.model().sync.selected, Some(selected));
        assert!(actions.borrow().is_empty());
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn dashboard_filter_counts_follow_search_not_selected_view() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::new();
        window.set_child(Some(controller.widget()));
        let drain = || {
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while main.pending() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "bounded component drain"
                );
                main.iteration(false);
            }
        };
        let sync = dashboard_test_sync();
        let selected = sync.selected;
        controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
        drain();
        let labels = || {
            controller
                .model()
                .dashboard_buttons
                .iter()
                .map(|(_, button)| button.label().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(),
            [
                "All 2",
                "Needs you 1",
                "Running 0",
                "Not started 1",
                "Review 0",
                "Finished 1"
            ]
        );
        controller.emit(TasksPanelMsg::DashboardView(
            crate::agent_task::DashboardView::Running,
        ));
        drain();
        assert!(controller.model().sync.rows.is_empty());
        assert_eq!(
            labels()[0],
            "All 2",
            "choosing a view must not shrink badges"
        );
        controller.widgets().dashboard_query_entry.set_text("First");
        drain();
        assert_eq!(
            labels(),
            [
                "All 1",
                "Needs you 0",
                "Running 0",
                "Not started 1",
                "Review 0",
                "Finished 0"
            ]
        );
        controller
            .widgets()
            .dashboard_query_entry
            .set_text("missing");
        drain();
        assert!(labels().iter().all(|label| label.ends_with(" 0")));
        assert!(controller
            .model()
            .dashboard_buttons
            .iter()
            .all(|(_, button)| button.is_sensitive()));
        assert_eq!(controller.model().sync.selected, selected);
        assert_eq!(controller.model().unfiltered_sync.rows, sync.rows);
        assert!(actions.borrow().is_empty());
        controller.emit(TasksPanelMsg::ClearDashboardFilters);
        controller.emit(TasksPanelMsg::DashboardSort(
            crate::agent_task::DashboardSort::Title,
        ));
        drain();
        assert_eq!(controller.model().sync.rows[0].title, "First task");
        assert!(actions.borrow().is_empty(), "sorting never operates a task");
        let attention = sync.rows.iter().find(|row| row.needs_attention).unwrap().id;
        controller.emit(TasksPanelMsg::NextAttention);
        drain();
        assert_eq!(
            actions.borrow().as_slice(),
            &[TaskPanelAction::Select(attention)]
        );
        actions.borrow_mut().clear();
        controller.emit(TasksPanelMsg::PreviousAttention);
        drain();
        assert_eq!(
            actions.borrow().as_slice(),
            &[TaskPanelAction::Select(attention)]
        );
        actions.borrow_mut().clear();
        controller.widgets().dashboard_query_entry.set_text("First");
        drain();
        assert!(!controller.widgets().next_attention.is_sensitive());
        assert!(!controller.widgets().previous_attention.is_sensitive());
        controller.emit(TasksPanelMsg::PreviousAttention);
        controller.emit(TasksPanelMsg::NextAttention);
        drain();
        assert!(
            actions.borrow().is_empty(),
            "next-attention cannot select a hidden task"
        );
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn dashboard_search_activate_selects_current_visible_result_only() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::new();
        window.set_child(Some(controller.widget()));
        let drain = || {
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while main.pending() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "bounded component drain"
                );
                main.iteration(false);
            }
        };
        let sync = dashboard_test_sync();
        let expected = sync
            .dashboard
            .rows
            .iter()
            .find(|row| row.title == "Needs review")
            .unwrap()
            .id;
        controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
        drain();
        controller
            .widgets()
            .dashboard_query_entry
            .set_text("review");
        drain();
        assert!(actions.borrow().is_empty(), "typing must not select");
        controller.emit(TasksPanelMsg::DashboardView(
            crate::agent_task::DashboardView::Running,
        ));
        controller.widgets().dashboard_query_entry.emit_activate();
        drain();
        assert!(
            actions.borrow().is_empty(),
            "the current view can hide all matches"
        );
        controller.emit(TasksPanelMsg::DashboardView(
            crate::agent_task::DashboardView::All,
        ));
        controller.widgets().dashboard_query_entry.emit_activate();
        drain();
        assert!(
            matches!(actions.borrow().as_slice(), [TaskPanelAction::Select(id)] if *id == expected)
        );
        assert_eq!(controller.model().unfiltered_sync.rows, sync.rows);
        controller
            .widgets()
            .dashboard_query_entry
            .set_text("no matches");
        controller.widgets().dashboard_query_entry.emit_activate();
        drain();
        assert_eq!(actions.borrow().len(), 1, "empty results are inert");
        window.close();
    }

    #[test]
    #[ignore = "requires a native GTK display"]
    fn dashboard_filters_preserve_selection_without_retargeting() {
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let controller =
            TasksPanelModel::builder()
                .launch(())
                .connect_receiver(move |_, output| {
                    let TasksPanelOutput::Action(action) = output;
                    sink.borrow_mut().push(action);
                });
        let window = gtk::Window::new();
        window.set_child(Some(controller.widget()));
        let drain = || {
            let main = gtk::glib::MainContext::default();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while main.pending() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "bounded component drain"
                );
                main.iteration(false);
            }
        };
        let sync = dashboard_test_sync();
        let selected = sync.selected;
        controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
        drain();
        assert_eq!(controller.model().sync.rows.len(), 2);
        controller.emit(TasksPanelMsg::DashboardView(
            crate::agent_task::DashboardView::NeedsAttention,
        ));
        drain();
        assert_eq!(controller.model().sync.selected, selected);
        assert_eq!(controller.model().sync.rows.len(), 1);
        assert!(controller.model().sync.detail.is_none());
        // The old row gesture carries its original task ID, not the new row 0.
        controller.emit(TasksPanelMsg::SelectRow(selected.unwrap()));
        controller.emit(TasksPanelMsg::DashboardQuery("missing".into()));
        drain();
        assert!(controller.model().sync.rows.is_empty());
        controller.widgets().dashboard_reveal.emit_clicked();
        drain();
        assert_eq!(controller.model().sync.rows.len(), 2);
        assert_eq!(controller.model().sync.selected, selected);
        assert_eq!(
            controller.model().sync.detail.as_ref().map(|d| d.id),
            selected
        );
        let mut removed = sync;
        removed.rows.retain(|row| Some(row.id) != selected);
        removed
            .dashboard
            .rows
            .retain(|row| Some(row.id) != selected);
        removed.detail = None;
        controller.emit(TasksPanelMsg::Sync(Box::new(removed)));
        drain();
        assert!(!controller.widgets().dashboard_reveal.is_visible());
        assert!(
            actions.borrow().is_empty(),
            "filtering or a hidden stale gesture must not stage an action"
        );
        window.close();
    }
}

#[cfg(test)]
#[path = "tasks_panel_keyed_tests.rs"]
mod keyed_tests;

#[cfg(test)]
#[path = "tasks_panel_stream_tests.rs"]
mod stream_tests;

#[cfg(test)]
#[path = "tasks_panel_control_tests.rs"]
mod control_tests;
