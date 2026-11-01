//! Real GTK regression for task triage, changing primary controls and rejected turns.
use super::*;
use std::time::{Duration, Instant};

fn drain() {
    let context = gtk::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(3);
    while context.pending() {
        assert!(Instant::now() < deadline, "panel failed to become idle");
        context.iteration(false);
    }
}

fn settle_layout(window: &gtk::Window) {
    let ticks = Rc::new(Cell::new(0));
    let observed = ticks.clone();
    window.add_tick_callback(move |_, _| {
        observed.set(observed.get() + 1);
        if observed.get() >= 2 {
            gtk::glib::ControlFlow::Break
        } else {
            gtk::glib::ControlFlow::Continue
        }
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while ticks.get() < 2 {
        assert!(Instant::now() < deadline, "control layout did not settle");
        drain();
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[ignore = "requires a native GTK display"]
fn task_control_center_preserves_search_action_identity_and_rejected_drafts() {
    gtk::init().expect("GTK display");
    let actions = Rc::new(RefCell::new(Vec::new()));
    let sink = actions.clone();
    let controller = TasksPanelModel::builder()
        .launch(())
        .connect_receiver(move |_, output| {
            let TasksPanelOutput::Action(action) = output;
            sink.borrow_mut().push(action);
        });
    let window = gtk::Window::builder()
        .default_width(520)
        .default_height(980)
        .build();
    window.set_child(Some(controller.widget()));
    window.present();
    let mut sync = super::tests::dashboard_test_sync();
    let task = sync.selected.unwrap();
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();

    controller
        .widgets()
        .dashboard_query_entry
        .set_text("provider:codex");
    drain();
    controller.widgets().status_filter.set_selected(1); // Created, incompatible with Failed.
    drain();
    let failed_index = crate::agent_task::DashboardAttentionKind::ALL
        .iter()
        .position(|kind| *kind == crate::agent_task::DashboardAttentionKind::Failed)
        .unwrap() as u32
        + 1;
    controller
        .widgets()
        .attention_filter
        .set_selected(failed_index);
    drain();
    assert_eq!(
        controller.widgets().dashboard_query_entry.text().as_str(),
        "provider:codex"
    );
    assert_eq!(controller.model().sync.rows.len(), 1);
    assert_eq!(controller.widgets().status_filter.selected(), 0);
    assert_eq!(
        controller.model().sync.rows[0].status,
        crate::agent_task::TaskStatus::Failed
    );
    assert_eq!(controller.model().sync.selected, Some(task));
    assert!(controller.model().sync.detail.is_none());
    assert!(!controller.widgets().next_step_card.is_visible());
    assert!(
        actions.borrow().is_empty(),
        "triage must not execute or select a task"
    );

    controller
        .widgets()
        .dashboard_query_entry
        .set_text("provider:unknown");
    drain();
    assert!(controller.model().sync.rows.is_empty());
    assert!(controller.widgets().dashboard_query_error.is_visible());
    controller.widgets().dashboard_query_entry.emit_activate();
    drain();
    assert!(
        actions.borrow().is_empty(),
        "an invalid search must not open another task"
    );
    let oversized = format!("provider:codex {} -status:failed", "x".repeat(300));
    controller
        .widgets()
        .dashboard_query_entry
        .set_text(&oversized);
    drain();
    assert_eq!(
        controller.widgets().dashboard_query_entry.text().as_str(),
        oversized
    );
    assert!(controller.widgets().dashboard_query_error.is_visible());
    controller.widgets().dashboard_clear.emit_clicked();
    drain();
    assert_eq!(controller.model().sync.rows.len(), 2);
    assert!(!controller.widgets().dashboard_query_error.is_visible());

    sync.detail.as_mut().unwrap().can_finish = true;
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert_eq!(
        controller.widgets().primary_action.label().as_deref(),
        Some("Finish session")
    );
    controller
        .widgets()
        .primary_action
        .emit_by_name::<()>("activate", &[]);
    sync.detail.as_mut().unwrap().can_finish = false;
    sync.detail.as_mut().unwrap().can_run_validation = true;
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    let deadline = Instant::now() + Duration::from_millis(350);
    while Instant::now() < deadline {
        drain();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        actions.borrow().is_empty(),
        "a held primary button must not become a different action"
    );
    controller.widgets().primary_action.emit_clicked();
    drain();
    assert_eq!(
        actions.borrow().as_slice(),
        &[TaskPanelAction::RunValidation(task)]
    );
    actions.borrow_mut().clear();

    sync.detail.as_mut().unwrap().validation_status =
        crate::agent_task::TaskValidationStatus::Failed;
    sync.detail.as_mut().unwrap().can_focus_validation = true;
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert_eq!(
        controller.widgets().primary_action.label().as_deref(),
        Some("Open validation output")
    );
    controller.widgets().primary_action.emit_clicked();
    drain();
    assert_eq!(
        actions.borrow().as_slice(),
        &[TaskPanelAction::FocusValidation(task)]
    );
    actions.borrow_mut().clear();
    sync.detail.as_mut().unwrap().validation_status =
        crate::agent_task::TaskValidationStatus::NotRun;
    sync.detail.as_mut().unwrap().can_focus_validation = false;
    sync.detail.as_mut().unwrap().can_run_validation = false;
    sync.detail.as_mut().unwrap().can_follow_up = true;
    sync.detail.as_mut().unwrap().can_finish = true;
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    controller
        .widgets()
        .follow_up_entry
        .buffer()
        .set_text("Keep this instruction if the session closes");
    drain();
    controller.widgets().follow_up_send.emit_clicked();
    drain();
    let request = match actions.borrow_mut().pop().unwrap() {
        TaskPanelAction::FollowUp(request) => request,
        unexpected => panic!("unexpected action: {unexpected:?}"),
    };
    assert_eq!(request.task_id, task);
    assert_eq!(controller.model().follow_up_drafts[&task], request.text);
    assert!(!controller.widgets().follow_up_send.is_sensitive());
    controller.widgets().follow_up_entry.buffer().set_text("");
    drain();
    assert!(
        !controller.widgets().primary_action.is_visible(),
        "a pending follow-up must not recommend finishing the session"
    );
    controller
        .widgets()
        .follow_up_entry
        .buffer()
        .set_text(&request.text);
    drain();
    controller.widgets().follow_up_send.emit_clicked();
    drain();
    assert!(
        actions.borrow().is_empty(),
        "pending follow-up cannot be double-submitted"
    );
    controller.emit(TasksPanelMsg::FollowUpResolved(
        request.clone(),
        Err("Session closed; draft retained".into()),
    ));
    drain();
    assert_eq!(
        controller
            .widgets()
            .follow_up_entry
            .buffer()
            .text(
                &controller.widgets().follow_up_entry.buffer().start_iter(),
                &controller.widgets().follow_up_entry.buffer().end_iter(),
                true
            )
            .as_str(),
        request.text
    );
    assert!(controller.widgets().follow_up_send.is_sensitive());
    assert!(controller
        .widgets()
        .follow_up_hint
        .label()
        .contains("Session closed"));

    controller.widgets().follow_up_send.emit_clicked();
    drain();
    let retry = match actions.borrow_mut().pop().unwrap() {
        TaskPanelAction::FollowUp(request) => request,
        unexpected => panic!("unexpected action: {unexpected:?}"),
    };
    controller
        .widgets()
        .follow_up_entry
        .buffer()
        .set_text("New correction typed while submitting");
    drain();
    controller.emit(TasksPanelMsg::FollowUpResolved(request, Ok(())));
    drain();
    assert!(
        !controller.widgets().follow_up_send.is_sensitive(),
        "late receipt cannot consume a newer attempt"
    );
    controller.emit(TasksPanelMsg::FollowUpResolved(retry, Ok(())));
    drain();
    assert_eq!(
        controller
            .widgets()
            .follow_up_entry
            .buffer()
            .text(
                &controller.widgets().follow_up_entry.buffer().start_iter(),
                &controller.widgets().follow_up_entry.buffer().end_iter(),
                true
            )
            .as_str(),
        "New correction typed while submitting"
    );
    assert!(controller.widgets().follow_up_send.is_sensitive());
    assert!(actions.borrow().is_empty());

    sync.detail.as_mut().unwrap().guidance = crate::agent_task::TaskGuidance {
        title: "Review or continue the live session",
        detail: "Inspect the changes and send follow-up feedback if needed. Finish the Agent session before running validation or accepting the task.",
        reason: Some("The existing worktree and follow-up draft remain available for review.".into()),
    };
    sync.detail.as_mut().unwrap().can_finish = true;
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert!(
        !controller.widgets().primary_action.is_visible(),
        "an unsent draft must not recommend finishing the session"
    );
    controller.widgets().follow_up_entry.buffer().set_text("");
    drain();
    assert!(controller.widgets().primary_action.is_visible());
    assert!(!controller.widgets().actions_expander.is_expanded());
    settle_layout(&window);
    let adjustment = controller.widgets().detail_viewport.vadjustment().unwrap();
    adjustment.set_value(adjustment.upper() - adjustment.page_size());
    settle_layout(&window);
    let composer = controller
        .widgets()
        .follow_up_row
        .compute_bounds(&window)
        .unwrap();
    assert!(
        composer.y() >= 0.0 && composer.y() + composer.height() <= window.height() as f32,
        "follow-up controls must be reachable inside the detail viewport"
    );
    adjustment.set_value(0.0);
    settle_layout(&window);
    if let Some(path) = std::env::var_os("JTERM_TASK_CONTROL_SCREENSHOT") {
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            drain();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(path)
            .status()
            .unwrap()
            .success());
    }
    controller
        .widgets()
        .follow_up_entry
        .buffer()
        .set_text("New correction typed while submitting");
    drain();
    assert!(!controller.widgets().primary_action.is_visible());
    let original = sync.clone();
    let other = sync.rows.iter().find(|row| row.id != task).unwrap().id;
    sync.selected = Some(other);
    sync.detail.as_mut().unwrap().id = other;
    controller.emit(TasksPanelMsg::Sync(Box::new(sync)));
    controller.emit(TasksPanelMsg::FollowUpChanged(
        task,
        "Late edit belongs to the original task".into(),
    ));
    drain();
    assert_eq!(
        controller.model().follow_up_drafts[&task],
        "Late edit belongs to the original task"
    );
    assert_ne!(
        controller
            .widgets()
            .follow_up_entry
            .buffer()
            .text(
                &controller.widgets().follow_up_entry.buffer().start_iter(),
                &controller.widgets().follow_up_entry.buffer().end_iter(),
                true
            )
            .as_str(),
        "Late edit belongs to the original task"
    );
    controller.emit(TasksPanelMsg::Sync(Box::new(original)));
    drain();
    assert_eq!(
        controller
            .widgets()
            .follow_up_entry
            .buffer()
            .text(
                &controller.widgets().follow_up_entry.buffer().start_iter(),
                &controller.widgets().follow_up_entry.buffer().end_iter(),
                true
            )
            .as_str(),
        "Late edit belongs to the original task"
    );
    assert!(actions.borrow().is_empty());
    window.close();
}
