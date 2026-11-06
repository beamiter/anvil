//! Native delivery-plan view over the host-owned Mission controller.
//! Opening a plan never starts a task, validates code, pushes or merges.
use super::workspace_mission_model::{
    attention_is_current, can_advance_draft_base, draft_matches_selected, parse_roots,
    receipt_command, receipt_is_current, update_draft, validate_new_environment,
};
use super::workspace_navigation::Action;
use gtk::prelude::*;
use jterm_core::agent_workspace::{
    ConversationId, MissionWorkbench, WorkspaceClient, WorkspaceEvent, WorkspaceMission,
    WorkspaceMissionAttention, WorkspaceReply,
};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct MissionSurface {
    state: Rc<RefCell<MissionWorkbench>>,
    window: Rc<RefCell<gtk::glib::WeakRef<gtk::Window>>>,
    callback: Rc<dyn Fn(Action)>,
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label
}

fn buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), true)
        .into()
}

/// Each receipt window owns one read request and one immutable queue entry.
/// It never changes the selected task, runs validation or substitutes newer logs.
fn show_receipt(
    parent: &gtk::Window,
    state: Rc<RefCell<MissionWorkbench>>,
    entry: WorkspaceMissionAttention,
) {
    let command = {
        let model = state.borrow();
        receipt_command(&entry, &model.attention_items(), model.busy)
    };
    let Some(command) = command else {
        state.borrow_mut().notice =
            Some("This receipt changed. Refresh the saved Mission first.".into());
        return;
    };
    let client = WorkspaceClient::new();
    let pending = client.send(command);
    let window = gtk::Window::builder()
        .title("Exact Mission validation receipt")
        .transient_for(parent)
        .destroy_with_parent(true)
        .default_width(720)
        .default_height(500)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.append(&label(&format!(
        "{} · revision {}\n{}\nReceipt: {}\nThis is recorded validation output, not proof of the current worktree or delivery.",
        entry.mission_title, entry.revision, entry.kind.label(),
        entry.receipt_id.as_deref().unwrap_or("unavailable")
    )));
    let status = label("Requesting the exact linked run from the current host…");
    body.append(&status);
    let output = gtk::TextView::new();
    output.set_editable(false);
    output.set_cursor_visible(false);
    output.set_monospace(true);
    output.set_wrap_mode(gtk::WrapMode::WordChar);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&output));
    body.append(&scroll);
    window.set_child(Some(&body));
    window.present();
    let request_id = match pending {
        Ok(id) => id,
        Err(error) => {
            status.set_text(&format!("Exact receipt unavailable: {error}"));
            return;
        }
    };
    let weak = window.downgrade();
    let mut completed = false;
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        if weak.upgrade().is_none() {
            return gtk::glib::ControlFlow::Break;
        }
        let model = state.borrow();
        let queue = model.attention_items();
        if !attention_is_current(&entry, &queue, model.busy) {
            output.buffer().set_text("");
            status.set_text("This saved Mission or receipt changed. Close this view and inspect the Mission again; newer output was not substituted.");
            return gtk::glib::ControlFlow::Break;
        }
        for WorkspaceEvent::Reply {
            request_id: received,
            result,
        } in client.poll()
        {
            if received != request_id || completed {
                continue;
            }
            completed = true;
            match result {
                Ok(WorkspaceReply::Result { report })
                    if receipt_is_current(&entry, &queue, model.busy, &report) =>
                {
                    status.set_text(&format!(
                        "Run {} · sequence {} · {}\n{}{}",
                        report.run_id, report.source_sequence, report.validation_status,
                        report.error.as_deref().unwrap_or(""),
                        if report.validation_output_truncated { "\nOutput was truncated by the host." } else { "" }
                    ));
                    output.buffer().set_text(&report.validation_output);
                }
                Err(error) => status.set_text(&format!("Exact receipt unavailable: {error}")),
                _ => status.set_text("The host no longer exposes this exact receipt. No newer run or log was substituted."),
            }
        }
        gtk::glib::ControlFlow::Continue
    });
}

impl MissionSurface {
    pub fn new(callback: Rc<dyn Fn(Action)>) -> Self {
        Self {
            state: Rc::new(RefCell::new(MissionWorkbench::new())),
            window: Rc::new(RefCell::new(gtk::glib::WeakRef::new())),
            callback,
        }
    }

    pub fn poll(&self) {
        self.state.borrow_mut().poll();
    }

    pub fn needs_attention(&self) -> usize {
        self.state.borrow().needs_attention()
    }

    pub fn show(&self, parent: &impl IsA<gtk::Window>, current: Option<ConversationId>) {
        if let Some(window) = self.window.borrow().upgrade() {
            window.present();
            return;
        }
        let window = self.build_window(parent, current);
        self.state.borrow_mut().refresh();
        window.present();
    }

    fn build_window(
        &self,
        parent: &impl IsA<gtk::Window>,
        current: Option<ConversationId>,
    ) -> gtk::Window {
        let window = gtk::Window::builder()
            .title("Missions · delivery plans")
            .transient_for(parent)
            .destroy_with_parent(true)
            .default_width(760)
            .default_height(780)
            .build();
        window.add_css_class("workspace-surface");
        self.window.borrow_mut().set(Some(&window));
        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.set_margin_top(16);
        body.set_margin_bottom(16);
        body.set_margin_start(16);
        body.set_margin_end(16);
        body.append(&label("One goal, exact repositories and linked task runs. Inspection reads Git and saved evidence; it does not start work or publish code."));
        let refresh = gtk::Button::with_label("Refresh saved Missions");
        body.append(&refresh);
        let catalog = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.append(&catalog);
        body.append(&label("Needs-you reasons for saved plans · read-only"));
        let attention = gtk::Box::new(gtk::Orientation::Vertical, 6);
        body.append(&attention);
        let title = gtk::Entry::new();
        title.set_placeholder_text(Some("Delivery title"));
        let goal = gtk::Entry::new();
        goal.set_placeholder_text(Some("Goal and acceptance conditions"));
        let environment = gtk::Entry::new();
        environment.set_text("local");
        body.append(&label("Title, acceptance goal and required environment"));
        body.append(&title);
        body.append(&goal);
        body.append(&environment);
        let roots = gtk::TextView::new();
        roots.set_wrap_mode(gtk::WrapMode::WordChar);
        roots.set_height_request(112);
        roots.set_top_margin(8);
        roots.set_bottom_margin(8);
        roots.buffer().set_text("beamiter/jterm_core = /path/to/jterm_core\nbeamiter/anvil = /path/to/anvil\nbeamiter/forge = /path/to/forge\nbeamiter/ember = /path/to/ember\nbeamiter/frost = /path/to/frost");
        body.append(&label(
            "Repository = exact absolute worktree path, one per line",
        ));
        body.append(&roots);
        let actions = gtk::FlowBox::new();
        actions.set_selection_mode(gtk::SelectionMode::None);
        let create = gtk::Button::with_label("Create new local plan");
        let save = gtk::Button::with_label("Save selected plan");
        let reload = gtk::Button::with_label("Reload selected plan (discard edits)");
        let inspect = gtk::Button::with_label("Inspect saved candidates");
        let freeze = gtk::Button::with_label("Freeze inspected commits");
        let link = gtk::Button::with_label(&current.as_ref().map_or_else(
            || "Select a task before opening Missions".into(),
            |id| format!("Link task {}", id.0),
        ));
        link.set_tooltip_text(Some("Links the task selected in the workspace when this window opened, only if its exact worktree belongs to this Mission."));
        for button in [&create, &save, &reload, &inspect, &freeze, &link] {
            actions.insert(button, -1);
        }
        body.append(&actions);
        let status = label("");
        status.add_css_class("dim-label");
        body.append(&status);
        let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.append(&details);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_child(Some(&body));
        super::workspace_navigation::wrap_button_labels(&body);
        window.set_child(Some(&scroll));

        let draft_base = Rc::new(RefCell::new(None::<WorkspaceMission>));
        let submitted = Rc::new(RefCell::new(None::<WorkspaceMission>));
        let reload_requested = Rc::new(Cell::new(false));
        let state = self.state.clone();
        let reload_flag = reload_requested.clone();
        reload.connect_clicked(move |_| {
            let mut state = state.borrow_mut();
            if state.busy {
                return;
            }
            if let Some(id) = state.selected.as_ref().map(|mission| mission.id.clone()) {
                reload_flag.set(true);
                state.select(&id);
            }
        });
        let state = self.state.clone();
        refresh.connect_clicked(move |_| state.borrow_mut().refresh());
        let state = self.state.clone();
        let fields = (
            title.clone(),
            goal.clone(),
            environment.clone(),
            roots.buffer(),
        );
        create.connect_clicked(move |_| {
            let (title, goal, environment, roots) = &fields;
            let mut state = state.borrow_mut();
            if let Err(error) = validate_new_environment(&environment.text()) {
                state.notice = Some(error);
                return;
            }
            match parse_roots(&buffer_text(roots)) {
                Ok(roots) => {
                    state.create(&title.text(), &goal.text(), &roots);
                }
                Err(error) => state.notice = Some(error),
            }
        });
        let state = self.state.clone();
        let fields = (
            title.clone(),
            goal.clone(),
            environment.clone(),
            roots.buffer(),
        );
        let base = draft_base.clone();
        let submission = submitted.clone();
        save.connect_clicked(move |_| {
            let (title, goal, environment, roots) = &fields;
            let mut state = state.borrow_mut();
            if state.busy {
                return;
            }
            let Some(mission) = base.borrow().clone() else {
                return;
            };
            if state
                .selected
                .as_ref()
                .is_none_or(|selected| selected.id != mission.id)
            {
                return;
            }
            match update_draft(
                mission,
                &title.text(),
                &goal.text(),
                &environment.text(),
                &buffer_text(roots),
            ) {
                Ok(mission) => {
                    *submission.borrow_mut() = Some(mission.clone());
                    state.save(mission);
                }
                Err(error) => state.notice = Some(error),
            }
        });
        let state = self.state.clone();
        inspect.connect_clicked(move |_| state.borrow_mut().inspect());
        let state = self.state.clone();
        let fields = (
            title.clone(),
            goal.clone(),
            environment.clone(),
            roots.buffer(),
        );
        freeze.connect_clicked(move |_| {
            let (title, goal, environment, roots) = &fields;
            let mut state = state.borrow_mut();
            // Recheck at activation, not only on the 100 ms sensitivity tick.
            // A just-edited form or an expired inspection never freezes a plan.
            let draft_current = state.selected.as_ref().is_some_and(|mission| {
                draft_matches_selected(
                    mission,
                    &title.text(),
                    &goal.text(),
                    &environment.text(),
                    &buffer_text(roots),
                )
            });
            if !draft_current {
                state.notice =
                    Some("Save or reload the edited plan before freezing candidates.".into());
                return;
            }
            if state.can_adopt_candidates() {
                state.adopt_candidates();
            }
        });
        let state = self.state.clone();
        let has_task = current.is_some();
        link.connect_clicked(move |_| {
            if let Some(id) = &current {
                state.borrow_mut().link_conversation(id);
            }
        });
        let state = self.state.clone();
        let callback = self.callback.clone();
        let mut catalog_signature = String::new();
        let mut report_signature = String::new();
        let mut attention_signature = String::new();
        let mut selected_id = String::new();
        let weak = window.downgrade();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            if weak.upgrade().is_none() {
                return gtk::glib::ControlFlow::Break;
            }
            state.borrow_mut().poll();
            let model = state.borrow();
            let queue = model.attention_items();
            let signature = format!("{:?}|{}", queue, model.busy);
            if signature != attention_signature {
                attention_signature = signature;
                while let Some(child) = attention.first_child() {
                    attention.remove(&child);
                }
                for entry in &queue {
                    attention.append(&label(&format!(
                        "{} · revision {} · {}\n{} · {} · environment {}\n{}\nCandidate: {}",
                        entry.mission_title,
                        entry.revision,
                        entry.kind.label(),
                        entry
                            .repository
                            .as_ref()
                            .map(|r| r.full_name.as_str())
                            .unwrap_or("Mission"),
                        entry.check_name.as_deref().unwrap_or("plan"),
                        entry.environment.as_deref().unwrap_or("not specified"),
                        entry.detail,
                        entry.candidate.as_deref().unwrap_or("not selected")
                    )));
                    let open = gtk::Button::with_label("Open saved plan");
                    open.set_sensitive(!model.busy);
                    let captured = entry.clone();
                    let data = state.clone();
                    open.connect_clicked(move |_| {
                        let mut model = data.borrow_mut();
                        if attention_is_current(&captured, &model.attention_items(), model.busy) {
                            model.select(&captured.mission_id);
                        }
                    });
                    attention.append(&open);
                    if entry.receipt_id.is_some() {
                        let open = gtk::Button::with_label("Show exact validation receipt");
                        open.set_sensitive(receipt_command(entry, &queue, model.busy).is_some());
                        open.set_tooltip_text(Some("Only the exact linked run and receipt sequence may be shown. Missing or ambiguous task bindings remain unavailable."));
                        let captured = entry.clone();
                        let data = state.clone();
                        let parent = weak.clone();
                        open.connect_clicked(move |_| {
                            if let Some(parent) = parent.upgrade() {
                                show_receipt(&parent, data.clone(), captured.clone());
                            }
                        });
                        attention.append(&open);
                    }
                }
                super::workspace_navigation::wrap_button_labels(&attention);
            }
            let states: Vec<_> = model
                .missions
                .iter()
                .map(|mission| {
                    model
                        .report_for(&mission.id)
                        .map(|report| report.state.label())
                        .unwrap_or("Not inspected")
                })
                .collect();
            let signature = format!("{:?}|{:?}", model.missions, states);
            if signature != catalog_signature {
                catalog_signature = signature;
                while let Some(child) = catalog.first_child() {
                    catalog.remove(&child);
                }
                for mission in &model.missions {
                    let button = gtk::Button::with_label(&format!(
                        "{} · revision {} · {}",
                        mission.title,
                        mission.revision,
                        model
                            .report_for(&mission.id)
                            .map(|report| report.state.label())
                            .unwrap_or("Not inspected")
                    ));
                    let id = mission.id.clone();
                    let model = state.clone();
                    button.connect_clicked(move |_| model.borrow_mut().select(&id));
                    super::workspace_navigation::wrap_button_labels(&button);
                    catalog.append(&button);
                }
            }
            let id = model.selected.as_ref().map(|m| m.id.as_str()).unwrap_or("");
            // Background replies and other clients must not overwrite typed drafts.
            // Only a different selection or explicit discard/reload resets the form.
            if id != selected_id || (reload_requested.get() && !model.busy) {
                selected_id = id.into();
                reload_requested.set(false);
                *draft_base.borrow_mut() = model.selected.clone();
                submitted.borrow_mut().take();
                if let Some(mission) = &model.selected {
                    title.set_text(&mission.title);
                    goal.set_text(&mission.goal);
                    environment.set_text(&mission.required_environment);
                    roots.buffer().set_text(
                        &mission
                            .repositories
                            .iter()
                            .map(|r| format!("{} = {}", r.identity.full_name, r.path.display()))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }
            if let Some(current) = model.selected.as_ref().filter(|current| {
                draft_base.borrow().as_ref().is_some_and(|base| {
                    can_advance_draft_base(base, current, submitted.borrow().as_ref())
                })
            }) {
                *draft_base.borrow_mut() = Some(current.clone());
            }
            let selected = model.selected.is_some();
            let draft_current = model.selected.as_ref().is_some_and(|mission| {
                draft_matches_selected(
                    mission,
                    &title.text(),
                    &goal.text(),
                    &environment.text(),
                    &buffer_text(&roots.buffer()),
                )
            });
            create.set_sensitive(!model.busy);
            save.set_sensitive(selected && !model.busy);
            reload.set_sensitive(selected && !model.busy);
            inspect.set_sensitive(selected && !model.busy);
            freeze.set_sensitive(model.can_adopt_candidates() && draft_current);
            link.set_sensitive(selected && has_task && !model.busy);
            refresh.set_sensitive(!model.busy);
            status.set_text(&format!(
                "{} Mission(s) need review. {}",
                model.needs_attention(),
                model.notice.as_deref().unwrap_or(if model.busy {
                    "Waiting for the host…"
                } else {
                    "Only inspected, version-bound receipts count as passed."
                })
            ));
            let signature = format!("{:?}|{:?}|{}", model.selected, model.report, draft_current);
            if signature != report_signature {
                report_signature = signature;
                while let Some(child) = details.first_child() {
                    details.remove(&child);
                }
                if let Some(mission) = &model.selected {
                    details.append(&label(&format!(
                        "{} · revision {} · environment {}",
                        mission.id, mission.revision, mission.required_environment
                    )));
                    for repository in &mission.repositories {
                        details.append(&label(&format!(
                            "{}\n{}\nBaseline: {}\nCandidate: {}\nRequired core: {}\nTarget: {}\nRequired checks: {}",
                            repository.identity.full_name,
                            repository.path.display(),
                            repository.baseline,
                            repository.candidate.as_deref().unwrap_or("not frozen"),
                            repository.required_core_pin.as_deref().unwrap_or("not set"),
                            repository.target_ref,
                            repository.required_checks.join(", ")
                        )));
                        for task in &repository.tasks {
                            let button = gtk::Button::with_label(&format!(
                                "Open task {} · run {}",
                                task.task_id, task.run_id
                            ));
                            let id = task.conversation.clone();
                            let callback = callback.clone();
                            button.connect_clicked(move |_| callback(Action::OpenTask(id.clone())));
                            super::workspace_navigation::wrap_button_labels(&button);
                            details.append(&button);
                        }
                    }
                }
                if selected && !draft_current {
                    details.append(&label("Unsaved or conflicting form edits. The saved plan's evidence is not a pass for this draft. Save and inspect again, or reload the selected plan."));
                } else if let Some(report) = &model.report {
                    details.append(&label(report.state.label()));
                    for repository in &report.repositories {
                        details.append(&label(&format!("{}\nModified: {} · committed: {} · pushed: {} · integrated: {}\nValidated: {} · pins current: {}", repository.identity.full_name, repository.modified, repository.committed, repository.pushed, repository.integrated, repository.validated, repository.pins_current)));
                        if let Some(observed) = &repository.observed {
                            details.append(&label(&format!("Observed local HEAD: {}\nInput fingerprint: {}\nDirty: {} · observed at {}\nManifest core: {}\nLock core: {}", observed.head, observed.fingerprint, observed.dirty, super::workspace_navigation::format_timestamp_ms(observed.observed_at_ms), observed.manifest_core_pin.as_deref().unwrap_or("unknown"), observed.lock_core_pin.as_deref().unwrap_or("unknown"))));
                        }
                        if let Some(remote) = &repository.pushed_observation {
                            details.append(&label(&format!("Dated remote observation: {} {}\nHEAD: {}\nObserved at {}. This receipt remains historical when current gates expire.", remote.repository.full_name, remote.ref_name, remote.head, super::workspace_navigation::format_timestamp_ms(remote.observed_at_ms))));
                        }
                        if let Some(integration) = &repository.integration_observation {
                            details.append(&label(&format!("Integration observation: {} {}\nTarget HEAD: {}\nCandidate: {} · contained at observation: {}\nObserved at {}. Re-inspect to establish current integration.", integration.remote.repository.full_name, integration.remote.ref_name, integration.remote.head, integration.candidate, integration.contains_candidate, super::workspace_navigation::format_timestamp_ms(integration.remote.observed_at_ms))));
                        }
                        for check in &repository.checks {
                            details.append(&label(&format!(
                                "{} · {} · {}\n{}",
                                check.check_name,
                                check.environment,
                                check.status.label(),
                                check.detail.as_deref().unwrap_or("")
                            )));
                        }
                        for error in &repository.errors {
                            details.append(&label(error));
                        }
                    }
                    for error in &report.errors {
                        details.append(&label(error));
                    }
                } else if selected {
                    details.append(&label(
                        "Not inspected. Saved plans are not proof of publication or validation.",
                    ));
                }
            }
            gtk::glib::ControlFlow::Continue
        });
        window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated GTK display"]
    fn mission_window_is_narrow_and_construction_does_not_start_work() {
        gtk::init().unwrap();
        let parent = gtk::Window::new();
        let surface = MissionSurface::new(Rc::new(|_| panic!("Construction dispatched a task")));
        let window = surface.build_window(&parent, None);
        window.set_default_size(360, 480);
        window.present();
        let context = gtk::glib::MainContext::default();
        for _ in 0..80 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            window.is_mapped() && window.width() >= 300,
            "The GTK fixture did not map the Mission window"
        );
        assert!(
            window.width() <= 400,
            "Mission content prevented a narrow window: {}",
            window.width()
        );
        assert!(
            !surface.state.borrow().busy,
            "Constructing widgets sent a host request"
        );
        assert!(surface.state.borrow().missions.is_empty());
        assert_eq!(surface.window.borrow().upgrade().as_ref(), Some(&window));
        let mut mission = WorkspaceMission::new("Native attention fixture", "Read-only review");
        mission.revision = 1;
        mission.repositories.push(
            jterm_core::agent_workspace::WorkspaceMissionRepository::new(
                "beamiter/anvil",
                "/tmp/anvil-attention-fixture",
            )
            .unwrap(),
        );
        surface.state.borrow_mut().missions.push(mission);
        for _ in 0..40 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        fn has_reason(widget: &gtk::Widget) -> bool {
            if widget
                .downcast_ref::<gtk::Label>()
                .is_some_and(|label| label.text().contains("Inspect current delivery"))
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                if has_reason(&current) {
                    return true;
                }
                child = current.next_sibling();
            }
            false
        }
        assert!(
            has_reason(window.upcast_ref()),
            "The shared attention reason was not rendered"
        );
        assert!(
            !surface.state.borrow().busy,
            "Rendering attention dispatched work"
        );
        assert!(
            window.width() <= 400,
            "Attention content broke the narrow layout"
        );
        window.close();
        parent.close();
    }
}
