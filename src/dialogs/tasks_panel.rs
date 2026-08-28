//! Relm4 agent Tasks panel.
//!
//! The component is a pure view: the application model owns the task manager,
//! native runtime, and diff worker, pushes composed snapshots in through
//! [`TasksPanelMsg::Sync`], and executes the [`TaskPanelAction`] outputs this
//! panel stages. Provider-controlled text arrives already display-safe; every
//! widget here treats it as plain text.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::prelude::*;

use crate::agent_task::{ApprovalId, CodexAppServerApproval, TaskId};
use crate::agent_task_ui::{
    approval_summary, native_follow_up_can_send, plan_list_refresh, render_stream_text,
    row_status_line, TaskPanelAction, TaskRowSnapshot,
};

const STREAM_PAGE: &str = "stream";
const DIFF_PAGE: &str = "diff";
const CREATE_TASK_LABEL: &str = "New agent task from selected block";
const CLOSE_TASKS_LABEL: &str = "Close Tasks panel";

/// Full panel state pushed by the application after every domain change.
#[derive(Clone, Debug, Default)]
pub(crate) struct TasksPanelSync {
    pub(crate) rows: Vec<TaskRowSnapshot>,
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
    StopCodex,
    FinishCodex,
    RunValidation,
    Complete,
    ReviewDiff,
    Archive,
}

const ACTION_KINDS: [(ActionKind, &str); 8] = [
    (ActionKind::StartCodex, "Start Codex"),
    (ActionKind::StartTerminal, "Terminal"),
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
    /// Row index into the model's current row table; the model maps it to the
    /// stable `TaskId`, so a list rebuild mid-gesture cannot retarget it.
    SelectRow(i32),
    FollowUpChanged(String),
    SendFollowUp,
    Create,
    Act(ActionKind),
    Decide(TaskId, ApprovalId, bool),
    ShowPage(&'static str),
    Close,
}

#[derive(Debug)]
pub(crate) enum TasksPanelOutput {
    Action(TaskPanelAction),
}

pub(crate) struct TasksPanelModel {
    sync: TasksPanelSync,
    /// Per-task follow-up drafts; the app never sees keystrokes, only the
    /// final text carried by `TaskPanelAction::FollowUp`.
    follow_up_drafts: HashMap<TaskId, String>,
    action_buttons: Vec<(ActionKind, gtk::Button)>,
    /// Render cache: what the list and approval widgets currently show.
    /// Refresh diffs each pushed Sync against it and touches GTK only on
    /// real change, so an unchanged Sync is a pure no-op — no widget churn,
    /// and no signals feeding back into the component's input queue.
    rendered_rows: Vec<TaskRowSnapshot>,
    rendered_selected: Option<TaskId>,
    rendered_approvals: Vec<CodexAppServerApproval>,
    /// Set while refresh applies a programmatic selection, so the resulting
    /// `row-selected` emission is not mistaken for a user gesture.
    selection_guard: Rc<Cell<bool>>,
}

impl TasksPanelModel {
    fn emit_action(&self, action: TaskPanelAction, sender: &ComponentSender<Self>) {
        if sender.output(TasksPanelOutput::Action(action)).is_err() {
            log::warn!("tasks panel output channel closed");
        }
    }

    /// Reconcile the task list widget with the pushed row table. Rows are
    /// rebuilt only when their snapshots change, and selection is applied
    /// only when it (or the table) changed, behind `selection_guard` so the
    /// emitted `row-selected` cannot echo back as a `SelectRow` input.
    /// Together these make refreshing unchanged state idempotent; an
    /// unguarded re-select on every refresh previously re-queued `SelectRow`
    /// per refresh and livelocked the component's message loop.
    fn sync_task_list(&mut self, widgets: &TasksPanelModelWidgets) {
        let plan = plan_list_refresh(
            &self.rendered_rows,
            self.rendered_selected,
            &self.sync.rows,
            self.sync.selected,
        );
        if plan.rebuild_rows {
            while let Some(child) = widgets.task_list.first_child() {
                widgets.task_list.remove(&child);
            }
            for row in &self.sync.rows {
                let title = gtk::Label::new(Some(&row.title));
                title.set_halign(gtk::Align::Start);
                title.set_ellipsize(gtk::pango::EllipsizeMode::End);
                title.set_max_width_chars(28);
                let status = gtk::Label::new(Some(&row_status_line(row)));
                status.set_halign(gtk::Align::Start);
                status.set_ellipsize(gtk::pango::EllipsizeMode::End);
                status.add_css_class("dim-label");
                status.add_css_class("caption");
                if row.needs_attention {
                    status.add_css_class("warning");
                }
                let lines = gtk::Box::new(gtk::Orientation::Vertical, 2);
                lines.set_margin_top(4);
                lines.set_margin_bottom(4);
                lines.set_margin_start(4);
                lines.append(&title);
                lines.append(&status);
                let list_row = gtk::ListBoxRow::new();
                list_row.set_child(Some(&lines));
                widgets.task_list.append(&list_row);
            }
            self.rendered_rows.clone_from(&self.sync.rows);
        }
        if plan.apply_selection {
            self.selection_guard.set(true);
            match plan.select_index {
                Some(index) => {
                    let row = widgets.task_list.row_at_index(index as i32);
                    widgets.task_list.select_row(row.as_ref());
                }
                None => widgets.task_list.unselect_all(),
            }
            self.selection_guard.set(false);
        }
        if plan.rebuild_rows || plan.apply_selection {
            self.rendered_selected = self.sync.selected;
        }
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
        let selection_guard = Rc::new(Cell::new(false));
        let model = Self {
            sync: TasksPanelSync::default(),
            follow_up_drafts: HashMap::new(),
            action_buttons: Vec::new(),
            rendered_rows: Vec::new(),
            rendered_selected: None,
            rendered_approvals: Vec::new(),
            selection_guard: selection_guard.clone(),
        };
        let widgets: TasksPanelModelWidgets = view_output!();

        for (kind, label) in ACTION_KINDS {
            let button = gtk::Button::with_label(label);
            button.add_css_class("caption");
            button.connect_clicked({
                let sender = sender.clone();
                move |_| sender.input(TasksPanelMsg::Act(kind))
            });
            widgets.actions_box.insert(&button, -1);
            action_buttons.push((kind, button));
        }

        widgets.task_list.connect_row_selected({
            let sender = sender.clone();
            let selection_guard = selection_guard.clone();
            move |_, row| {
                // Only genuine gestures may queue SelectRow; programmatic
                // selection applied by refresh runs behind the guard.
                if selection_guard.get() {
                    return;
                }
                if let Some(row) = row {
                    sender.input(TasksPanelMsg::SelectRow(row.index()));
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
            move |button| {
                if button.is_active() {
                    sender.input(TasksPanelMsg::ShowPage(STREAM_PAGE));
                }
            }
        });
        widgets.diff_page_button.connect_toggled({
            let sender = sender.clone();
            move |button| {
                if button.is_active() {
                    sender.input(TasksPanelMsg::ShowPage(DIFF_PAGE));
                }
            }
        });

        let model = Self {
            action_buttons,
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
        let show_diff = matches!(&message, TasksPanelMsg::Act(ActionKind::ReviewDiff));
        match message {
            TasksPanelMsg::Sync(sync) => {
                self.sync = *sync;
            }
            TasksPanelMsg::SelectRow(index) => {
                if let Some(row) = self
                    .sync
                    .rows
                    .get(usize::try_from(index).unwrap_or(usize::MAX))
                {
                    if self.sync.selected != Some(row.id) {
                        self.emit_action(TaskPanelAction::Select(row.id), &sender);
                    }
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
            TasksPanelMsg::Act(kind) => {
                if let Some(id) = self.sync.selected {
                    let action = match kind {
                        ActionKind::StartCodex => TaskPanelAction::StartCodex(id),
                        ActionKind::StartTerminal => TaskPanelAction::StartTerminal(id),
                        ActionKind::StopCodex => TaskPanelAction::StopCodex(id),
                        ActionKind::FinishCodex => TaskPanelAction::FinishCodex(id),
                        ActionKind::RunValidation => TaskPanelAction::RunValidation(id),
                        ActionKind::Complete => TaskPanelAction::Complete(id),
                        ActionKind::ReviewDiff => TaskPanelAction::ReviewDiff(id),
                        ActionKind::Archive => TaskPanelAction::Archive(id),
                    };
                    self.emit_action(action, &sender);
                }
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
                let _ = page; // applied in update_view through the toggle buttons
            }
            TasksPanelMsg::Close => {
                self.emit_action(TaskPanelAction::Close, &sender);
            }
        }
        if show_diff {
            widgets.diff_page_button.set_active(true);
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

        self.sync_task_list(widgets);

        let detail = self.sync.detail.as_deref();
        for (kind, button) in &self.action_buttons {
            let sensitive = detail.is_some_and(|detail| {
                match kind {
                    ActionKind::StartCodex => detail.can_start_codex,
                    ActionKind::StartTerminal => detail.can_start_terminal,
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

        // The page toggle pair mirrors the visible stack child.
        let on_diff = widgets.diff_page_button.is_active();
        widgets
            .page_stack
            .set_visible_child_name(if on_diff { DIFF_PAGE } else { STREAM_PAGE });
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
