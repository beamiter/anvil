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

use crate::agent_task::{ApprovalId, CodexAppServerApproval, TaskId};
use crate::agent_task_ui::{
    approval_summary, native_follow_up_can_send, plan_list_refresh, render_stream_text,
    row_status_line, task_reason, TaskPanelAction, TaskRowSnapshot,
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
const CREATE_TASK_LABEL: &str = "New agent task from selected block";
const CLOSE_TASKS_LABEL: &str = "Close Tasks panel";

/// Derive the stack page and both mutually exclusive button states from one
/// request instead of treating the Diff button as the source of truth.
fn task_page_presentation(requested: &str) -> (&'static str, bool, bool) {
    if requested == DIFF_PAGE {
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
    pub(crate) title: String,
    pub(crate) status_line: String,
    pub(crate) branch: String,
    pub(crate) stream: Option<Box<crate::agent_task::CodexAppServerViewSnapshot>>,
    pub(crate) approvals: Vec<CodexAppServerApproval>,
    pub(crate) completed_turns: usize,
    pub(crate) can_start_codex: bool,
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
    StartCodex,
    StartTerminal,
    FocusTerminal,
    FocusValidation,
    StopCodex,
    FinishCodex,
    RunValidation,
    Complete,
    ReviewDiff,
    Archive,
}

const ACTION_KINDS: [(ActionKind, &str); 10] = [
    (ActionKind::StartCodex, "Start Codex"),
    (ActionKind::StartTerminal, "Start terminal"),
    (ActionKind::FocusTerminal, "Agent terminal"),
    (ActionKind::FocusValidation, "Validation output"),
    (ActionKind::StopCodex, "Stop"),
    (ActionKind::FinishCodex, "Finish"),
    (ActionKind::RunValidation, "Validate"),
    (ActionKind::Complete, "Complete"),
    (ActionKind::ReviewDiff, "Diff"),
    (ActionKind::Archive, "Archive"),
];

#[derive(Debug)]
pub(crate) enum TasksPanelMsg {
    Sync(Box<TasksPanelSync>),
    DashboardQuery(String),
    OpenDashboardResult,
    ClearDashboardFilters,
    DashboardView(crate::agent_task::DashboardView),
    /// Stable identity captured when the row gesture occurs. A later filter
    /// change may hide it, but must never retarget a queued gesture.
    SelectRow(TaskId),
    FollowUpChanged(String),
    SendFollowUp,
    Create,
    Act(TaskId, ActionKind),
    Decide(TaskId, ApprovalId, bool),
    ShowPage(&'static str),
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
    dashboard_summary: String,
    dashboard_counts: crate::agent_task::DashboardCounts,
    dashboard_buttons: Vec<(crate::agent_task::DashboardView, gtk::Button)>,
    /// Per-task follow-up drafts; the app never sees keystrokes, only the
    /// final text carried by `TaskPanelAction::FollowUp`.
    follow_up_drafts: HashMap<TaskId, String>,
    action_buttons: Vec<(ActionKind, gtk::Button)>,
    /// Task shown by the action buttons, captured before queuing a click.
    action_target: Rc<Cell<Option<TaskId>>>,
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
    /// Programmatic toggle synchronization must not enqueue stale ShowPage
    /// messages after newer user selections already waiting in the queue.
    page_guard: Rc<Cell<bool>>,
}

impl TasksPanelModel {
    fn apply_dashboard_filters(&mut self) {
        let mut sync = self.unfiltered_sync.clone();
        let dashboard = crate::agent_task::DashboardSnapshot::filter_rows(
            &sync.dashboard.rows,
            &self.dashboard_query,
            self.dashboard_view,
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
        self.dashboard_summary = format!(
            "{} shown · {} search matches · {} need you · {} awaiting approval",
            dashboard.rows.len(),
            dashboard.counts.total,
            dashboard.counts.attention,
            dashboard.counts.pending_approval,
        );
        self.dashboard_counts = dashboard.counts;
        self.sync = sync;
    }

    fn emit_action(&self, action: TaskPanelAction, sender: &ComponentSender<Self>) {
        if sender.output(TasksPanelOutput::Action(action)).is_err() {
            log::warn!("tasks panel output channel closed");
        }
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
                        widgets.task_list.remove(&row.row);
                    }
                }
            }
            for (index, id) in ids.iter().enumerate() {
                let row = &self.row_widgets[id].row;
                if widgets.task_list.row_at_index(index as i32).as_ref() != Some(row) {
                    if row.parent().is_some() {
                        widgets.task_list.remove(row);
                    }
                    widgets.task_list.insert(row, index as i32);
                }
            }
        }
        // Prune even when only a hidden/archived task changed.
        self.row_widgets.retain(|id, _| live.contains(id));
        if plan.apply_selection {
            let row = plan
                .select_index
                .and_then(|i| widgets.task_list.row_at_index(i as i32));
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
                set_placeholder_text: Some("Search tasks, providers or repositories · Enter to open"),
                set_max_length: 256,
                update_property: &[gtk::accessible::Property::Label("Search agent tasks")],
                connect_changed[sender] => move |entry| {
                    sender.input(TasksPanelMsg::DashboardQuery(entry.text().to_string()));
                },
                connect_activate => TasksPanelMsg::OpenDashboardResult,
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
                set_end_child = &gtk::Box {
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

                    #[name(actions_box)]
                    gtk::FlowBox {
                        set_selection_mode: gtk::SelectionMode::None,
                        set_column_spacing: 4,
                        set_row_spacing: 4,
                        set_max_children_per_line: 4,
                        set_visible: false,
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
                        set_vexpand: true,

                        add_named[Some(STREAM_PAGE)] = &gtk::ScrolledWindow {
                            set_policy: (gtk::PolicyType::Never, gtk::PolicyType::Automatic),

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

                        #[name(follow_up_entry)]
                        gtk::Entry {
                            set_hexpand: true,
                            set_placeholder_text: Some("Follow-up turn for Codex…"),
                        },

                        #[name(follow_up_send)]
                        gtk::Button {
                            set_icon_name: "go-next-symbolic",
                            add_css_class: "flat",
                            set_tooltip_text: Some("Send follow-up turn"),
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
        let selection_guard = Rc::new(Cell::new(false));
        let selection_ids = Rc::new(RefCell::new(Vec::<TaskId>::new()));
        let page_guard = Rc::new(Cell::new(false));
        let model = Self {
            sync: TasksPanelSync::default(),
            unfiltered_sync: TasksPanelSync::default(),
            dashboard_query: String::new(),
            dashboard_view: crate::agent_task::DashboardView::All,
            dashboard_summary: String::new(),
            dashboard_counts: Default::default(),
            dashboard_buttons: Vec::new(),
            follow_up_drafts: HashMap::new(),
            action_buttons: Vec::new(),
            action_target: action_target.clone(),
            rendered_rows: Vec::new(),
            row_widgets: HashMap::new(),
            rendered_selected: None,
            rendered_approvals: Vec::new(),
            selection_guard: selection_guard.clone(),
            selection_ids: selection_ids.clone(),
            page: STREAM_PAGE,
            page_guard: page_guard.clone(),
        };
        let widgets: TasksPanelModelWidgets = view_output!();
        widgets
            .diff_page_button
            .set_group(Some(&widgets.stream_page_button));

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
        widgets.follow_up_entry.connect_changed({
            let sender = sender.clone();
            move |entry| {
                sender.input(TasksPanelMsg::FollowUpChanged(entry.text().to_string()));
            }
        });
        widgets.follow_up_entry.connect_activate({
            let sender = sender.clone();
            move |_| sender.input(TasksPanelMsg::SendFollowUp)
        });
        widgets.follow_up_send.connect_clicked({
            let sender = sender.clone();
            move |_| sender.input(TasksPanelMsg::SendFollowUp)
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
            TasksPanelMsg::Sync(sync) => {
                self.unfiltered_sync = *sync;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::ClearDashboardFilters => {
                self.dashboard_query.clear();
                self.dashboard_view = crate::agent_task::DashboardView::All;
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
                self.dashboard_query = query.chars().take(256).collect();
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::DashboardView(view) => {
                self.dashboard_view = view;
                self.apply_dashboard_filters();
            }
            TasksPanelMsg::SelectRow(id) => {
                if self.sync.selected != Some(id) && self.sync.rows.iter().any(|row| row.id == id) {
                    self.emit_action(TaskPanelAction::Select(id), &sender);
                }
            }
            TasksPanelMsg::FollowUpChanged(text) => {
                if let Some(id) = self.sync.selected {
                    self.follow_up_drafts.insert(id, text);
                }
            }
            TasksPanelMsg::SendFollowUp => {
                if let Some(detail) = self.sync.detail.as_deref() {
                    let text = self
                        .follow_up_drafts
                        .get(&detail.id)
                        .cloned()
                        .unwrap_or_default();
                    if native_follow_up_can_send(&text, detail.completed_turns)
                        && detail.can_follow_up
                    {
                        self.follow_up_drafts.remove(&detail.id);
                        self.emit_action(TaskPanelAction::FollowUp(detail.id, text), &sender);
                    }
                }
            }
            TasksPanelMsg::Create => {
                self.emit_action(TaskPanelAction::CreateFromBlock, &sender);
            }
            TasksPanelMsg::Act(id, kind) => {
                // Keep the clicked task even if a queued Sync changes selection.
                // The application/runtime rechecks this task and its live gates.
                let action = match kind {
                    ActionKind::StartCodex => TaskPanelAction::StartCodex(id),
                    ActionKind::StartTerminal => TaskPanelAction::StartTerminal(id),
                    ActionKind::FocusTerminal => TaskPanelAction::FocusTerminal(id),
                    ActionKind::FocusValidation => TaskPanelAction::FocusValidation(id),
                    ActionKind::StopCodex => TaskPanelAction::StopCodex(id),
                    ActionKind::FinishCodex => TaskPanelAction::FinishCodex(id),
                    ActionKind::RunValidation => TaskPanelAction::RunValidation(id),
                    ActionKind::Complete => TaskPanelAction::Complete(id),
                    ActionKind::ReviewDiff => TaskPanelAction::ReviewDiff(id),
                    ActionKind::Archive => TaskPanelAction::Archive(id),
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
            .create_button
            .set_sensitive(self.sync.create_enabled && !self.sync.pending_creation);
        let show_hint = !self.sync.create_hint.is_empty();
        widgets.create_hint.set_visible(show_hint);
        if show_hint {
            widgets.create_hint.set_label(&self.sync.create_hint);
        }

        widgets.dashboard_summary.set_label(&self.dashboard_summary);
        widgets.dashboard_clear.set_visible(
            !self.dashboard_query.is_empty()
                || self.dashboard_view != crate::agent_task::DashboardView::All,
        );
        let hidden = self.sync.selected.is_some_and(|id| {
            self.unfiltered_sync
                .dashboard
                .rows
                .iter()
                .any(|row| row.id == id)
                && !self.sync.rows.iter().any(|row| row.id == id)
        });
        widgets.dashboard_reveal.set_visible(hidden);
        for (view, button) in &self.dashboard_buttons {
            let label = dashboard_filter_label(*view, &self.dashboard_counts);
            if button.label().as_deref() != Some(label.as_str()) {
                button.set_label(&label);
            }
            if *view == self.dashboard_view {
                button.add_css_class("suggested-action");
            } else {
                button.remove_css_class("suggested-action");
            }
        }
        self.sync_task_list(widgets);

        let detail = self.sync.detail.as_deref();
        self.action_target.set(detail.map(|detail| detail.id));
        for (kind, button) in &self.action_buttons {
            let sensitive = detail.is_some_and(|detail| {
                match kind {
                    ActionKind::StartCodex => detail.can_start_codex,
                    ActionKind::StartTerminal => detail.can_start_terminal,
                    ActionKind::FocusTerminal => detail.can_focus_terminal,
                    ActionKind::FocusValidation => detail.can_focus_validation,
                    ActionKind::StopCodex => detail.can_stop,
                    ActionKind::FinishCodex => detail.can_finish,
                    ActionKind::RunValidation => detail.can_run_validation,
                    ActionKind::Complete => detail.can_complete,
                    ActionKind::ReviewDiff => true,
                    // Archiving is always safe to offer for a selected task.
                    ActionKind::Archive => true,
                }
            });
            button.set_sensitive(sensitive);
        }
        widgets.actions_box.set_visible(detail.is_some());
        widgets.detail_title.set_visible(detail.is_some());
        widgets.detail_status.set_visible(detail.is_some());
        widgets.page_stack.set_visible(detail.is_some());
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
            widgets.detail_title.set_label(&detail.title);
            widgets
                .detail_status
                .set_label(&format!("{} · {}", detail.status_line, detail.branch));

            let stream_text = detail
                .stream
                .as_deref()
                .map(render_stream_text)
                .unwrap_or_else(|| {
                    "No native Codex session snapshot for this task yet.".to_string()
                });
            set_view_text(&widgets.stream_view, &stream_text);

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
            let show_hint = !detail.follow_up_hint.is_empty();
            widgets.follow_up_hint.set_visible(show_hint);
            if show_hint {
                widgets.follow_up_hint.set_label(&detail.follow_up_hint);
            }
            let draft = self
                .follow_up_drafts
                .get(&detail.id)
                .cloned()
                .unwrap_or_default();
            if widgets.follow_up_entry.text().as_str() != draft {
                widgets.follow_up_entry.set_text(&draft);
            }
            widgets.follow_up_send.set_sensitive(
                detail.can_follow_up && native_follow_up_can_send(&draft, detail.completed_turns),
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
            title: "First task".into(),
            status_line: "Created".into(),
            branch: "test/dashboard".into(),
            stream: None,
            approvals: Vec::new(),
            completed_turns: 0,
            can_start_codex: false,
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
            rows,
            dashboard,
            selected: Some(selected),
            detail: Some(Box::new(detail)),
            ..Default::default()
        }
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
        row.status = crate::agent_task::TaskStatus::Failed;
        row.status_detail = Some("task reason".into());
        row.validation_status = crate::agent_task::TaskValidationStatus::Failed;
        row.validation_detail = Some("validation old".into());
        let row = sync
            .dashboard
            .rows
            .iter_mut()
            .find(|row| row.id == selected)
            .unwrap();
        row.status = crate::agent_task::TaskStatus::Failed;
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
