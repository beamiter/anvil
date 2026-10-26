//! Hosted-only native identity, reentrancy, keyboard and timing fixtures.
use super::*;
use std::time::{Duration, Instant};

/// Shows only the fixture rows titled "Task 0". Free-text search also matches
/// each task's random UUID, so a bare `Task 0` would keep any "Task 1" row
/// whose id happens to contain a `0`; the title filter is deterministic.
const TASK_0_ONLY: &str = "title:\"Task 0\"";

fn drain() {
    let context = gtk::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(3);
    while context.pending() {
        assert!(Instant::now() < deadline, "component did not become idle");
        context.iteration(false);
    }
}

fn fixture(count: usize) -> TasksPanelSync {
    let seed = super::tests::dashboard_test_sync();
    let mut sync = TasksPanelSync::default();
    for i in 0..count {
        let id = TaskId::new();
        let mut row = seed.rows[0].clone();
        row.id = id;
        row.title = format!("Task {}", i % 2); // Intentionally duplicate titles.
        let mut projected = seed
            .dashboard
            .rows
            .iter()
            .find(|r| r.id == seed.rows[0].id)
            .unwrap()
            .clone();
        projected.id = id;
        projected.updated_at_ms = (count - i) as u64;
        projected.title.clone_from(&row.title);
        sync.rows.push(row);
        sync.dashboard.rows.push(projected);
    }
    sync.selected = Some(sync.rows[count / 2].id);
    sync
}

// Dashboard filtering re-sorts by rank/update time/TaskId; vector order alone
// cannot simulate a real reorder. Give the requested order explicit timestamps.
fn restamp_order(sync: &mut TasksPanelSync) {
    let count = sync.dashboard.rows.len();
    for (index, row) in sync.dashboard.rows.iter_mut().enumerate() {
        row.updated_at_ms = (count - index) as u64;
    }
}

fn rotate_order(sync: &mut TasksPanelSync, count: usize) {
    sync.dashboard.rows.rotate_right(count);
    restamp_order(sync);
}

fn set_sync(controller: &Controller<TasksPanelModel>, sync: TasksPanelSync) {
    controller.emit(TasksPanelMsg::Sync(Box::new(sync)));
    drain();
}

type ActionLog = Rc<RefCell<Vec<TaskPanelAction>>>;

fn panel() -> (Controller<TasksPanelModel>, ActionLog) {
    let actions = Rc::new(RefCell::new(Vec::new()));
    let sink = actions.clone();
    let controller = TasksPanelModel::builder()
        .launch(())
        .connect_receiver(move |_, output| {
            let TasksPanelOutput::Action(action) = output;
            sink.borrow_mut().push(action);
        });
    (controller, actions)
}

#[test]
#[ignore = "requires a native GTK display"]
fn keyed_rows_update_only_changed_widgets_and_prune_archives() {
    gtk::init().expect("GTK display");
    for count in [100, 1000] {
        let (controller, actions) = panel();
        let mut sync = fixture(count);
        set_sync(&controller, sync.clone());
        let before: HashMap<_, _> = controller
            .model()
            .row_widgets
            .iter()
            .map(|(id, w)| (*id, (w.row.clone(), w.updates)))
            .collect();
        let changed = sync.rows[3].id;
        sync.rows[3].validation_attempt += 1;
        sync.rows[3].validation_status = crate::agent_task::TaskValidationStatus::Failed;
        sync.rows[3].validation_detail = Some("new failure".into());
        set_sync(&controller, sync.clone());
        for (id, widgets) in &controller.model().row_widgets {
            assert_eq!(widgets.row, before[id].0, "row identity changed");
            assert_eq!(widgets.updates, before[id].1 + usize::from(*id == changed));
        }
        assert!(
            actions.borrow().is_empty(),
            "programmatic update emitted actions"
        );
        // Add/remove/move without changing the other row snapshots.
        let removed = sync.rows[5].id;
        sync.rows.retain(|r| r.id != removed);
        sync.dashboard.rows.retain(|r| r.id != removed);
        let added = fixture(1);
        let inserted = added.rows[0].id;
        sync.rows.extend(added.rows);
        sync.dashboard.rows.extend(added.dashboard.rows);
        rotate_order(&mut sync, 7);
        set_sync(&controller, sync.clone());
        assert!(!controller.model().row_widgets.contains_key(&removed));
        assert!(controller.model().row_widgets.contains_key(&inserted));
        for (id, widgets) in &controller.model().row_widgets {
            if let Some(old) = before.get(id) {
                assert_eq!(widgets.row, old.0);
            }
        }
        // Search hides rows but does not remove them from domain snapshots.
        controller.emit(TasksPanelMsg::DashboardQuery(TASK_0_ONLY.into()));
        drain();
        let hidden = sync.rows.iter().find(|r| r.title == "Task 1").unwrap().id;
        let hidden_widget = controller.model().row_widgets[&hidden].row.clone();
        sync.rows
            .iter_mut()
            .find(|r| r.id == hidden)
            .unwrap()
            .native_preparing = true;
        set_sync(&controller, sync.clone());
        assert!(
            controller
                .model()
                .unfiltered_sync
                .rows
                .iter()
                .find(|r| r.id == hidden)
                .unwrap()
                .native_preparing
        );
        assert!(hidden_widget.parent().is_none());
        controller.emit(TasksPanelMsg::ClearDashboardFilters);
        drain();
        assert_eq!(controller.model().row_widgets[&hidden].row, hidden_widget);
        assert!(
            controller.model().row_widgets[&hidden]
                .snapshot
                .native_preparing
        );
        // Archive remains in raw rows, but leaves the unfiltered dashboard.
        controller.emit(TasksPanelMsg::DashboardQuery(TASK_0_ONLY.into()));
        drain();
        sync.rows
            .iter_mut()
            .find(|r| r.id == hidden)
            .unwrap()
            .status = crate::agent_task::TaskStatus::Archived;
        sync.dashboard.rows.retain(|r| r.id != hidden);
        set_sync(&controller, sync);
        assert!(!controller.model().row_widgets.contains_key(&hidden));
        assert!(actions.borrow().is_empty());
    }
}

#[test]
#[ignore = "requires a native GTK display"]
fn keyed_selected_row_survives_move_hide_and_reveal() {
    gtk::init().expect("GTK display");
    let (controller, actions) = panel();
    let mut sync = fixture(2);
    let selected = sync.selected.unwrap();
    set_sync(&controller, sync.clone());
    let list = controller.widgets().task_list.clone();
    let row = controller.model().row_widgets[&selected].row.clone();
    assert_eq!(list.selected_row(), Some(row.clone()));
    assert!(row.is_selected());
    // The selected row itself moves, unlike shifting its index by moving a
    // different row. No window/focus side effect can repair selection here.
    rotate_order(&mut sync, 1);
    set_sync(&controller, sync.clone());
    assert_eq!(row.index(), 0);
    assert_eq!(list.selected_row(), Some(row.clone()));
    assert!(row.is_selected());
    controller.emit(TasksPanelMsg::DashboardQuery(TASK_0_ONLY.into()));
    drain();
    assert!(row.parent().is_none());
    assert!(
        !row.is_selected(),
        "cached hidden row must not retain GTK selection"
    );
    assert!(list.selected_row().is_none());
    assert_eq!(controller.model().sync.selected, Some(selected));
    controller.emit(TasksPanelMsg::ClearDashboardFilters);
    drain();
    assert_eq!(controller.model().row_widgets[&selected].row, row);
    assert_eq!(list.selected_row(), Some(row.clone()));
    assert!(row.is_selected());
    assert!(actions.borrow().is_empty());
}

#[test]
#[ignore = "requires a native GTK display"]
fn keyed_selection_guard_survives_synchronous_callbacks() {
    gtk::init().expect("GTK display");
    let (controller, actions) = panel();
    let mut sync = fixture(100);
    set_sync(&controller, sync.clone());
    let list = controller.widgets().task_list.clone();
    let guard = controller.model().selection_guard.clone();
    let ids = controller.model().selection_ids.clone();
    let callbacks = Rc::new(Cell::new(0));
    let seen = callbacks.clone();
    let sender = controller.sender().clone();
    let queued = sync.clone();
    let handler = list.connect_row_selected(move |_, _| {
        assert!(guard.get(), "synchronous callback must remain guarded");
        // This would panic if reconciliation kept the ID table borrowed.
        let _ids = ids.borrow_mut();
        seen.set(seen.get() + 1);
        if seen.get() == 1 {
            sender
                .send(TasksPanelMsg::Sync(Box::new(queued.clone())))
                .unwrap();
        }
    });
    // Moving the selected row emits synchronous signals while the model is
    // borrowed. Relm4 queues the nested sync; it must not become a gesture.
    let selected = sync.selected.unwrap();
    let index = sync
        .dashboard
        .rows
        .iter()
        .position(|r| r.id == selected)
        .unwrap();
    sync.dashboard.rows.swap(0, index);
    restamp_order(&mut sync);
    set_sync(&controller, sync);
    assert!(callbacks.get() > 0);
    assert!(!controller.model().selection_guard.get());
    assert_eq!(controller.model().sync.selected, Some(selected));
    assert_eq!(
        controller.model().selection_ids.borrow()[list.selected_row().unwrap().index() as usize],
        selected
    );
    assert!(actions.borrow().is_empty());
    list.disconnect(handler);
    // Nested guards restore their previous value, not unconditionally false.
    let flag = Rc::new(Cell::new(false));
    let outer = SelectionGuard::enter(&flag);
    {
        let _inner = SelectionGuard::enter(&flag);
    }
    assert!(flag.get());
    drop(outer);
    assert!(!flag.get());
}

// Capture the immutable e188 full-rebuild implementation for an apples-to-
// apples list-only comparison. No model filtering or detail rendering here.
fn baseline_full_rebuild(list: &gtk::ListBox, rows: &[TaskRowSnapshot]) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for row in rows {
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
        if let Some(reason) = task_reason(
            row.status,
            row.validation_status,
            row.status_detail.as_deref(),
            row.validation_detail.as_deref(),
        ) {
            let preview = gtk::Label::new(Some(&reason));
            preview.set_halign(gtk::Align::Start);
            preview.set_single_line_mode(true);
            preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
            preview.set_max_width_chars(28);
            preview.set_tooltip_text(Some(&reason));
            preview.add_css_class("caption");
            preview.add_css_class("dim-label");
            preview.add_css_class("task-reason");
            lines.append(&preview);
        }
        let list_row = gtk::ListBoxRow::new();
        list_row.set_child(Some(&lines));
        list.append(&list_row);
    }
}

fn p95(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[(sorted.len() * 95).div_ceil(100) - 1]
}

#[test]
#[ignore = "requires a native GTK display"]
fn keyed_task_list_mixed_update_benchmark() {
    gtk::init().expect("GTK display");
    for count in [100, 1000] {
        let (controller, actions) = panel();
        let sync = fixture(count);
        set_sync(&controller, sync);
        let window = gtk::Window::new();
        window.set_default_size(800, 900);
        window.set_child(Some(controller.widget()));
        window.present();
        let baseline = gtk::ListBox::new();
        let baseline_window = gtk::Window::new();
        baseline_window.set_default_size(320, 900);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&baseline));
        baseline_window.set_child(Some(&scroll));
        baseline_window.present();
        // Both timed paths start populated. The baseline must include removal
        // of existing rows even in sample zero, rather than empty construction.
        baseline_full_rebuild(&baseline, &controller.model().sync.rows);
        drain();
        let mut updates = Vec::new();
        let mut content_updates = Vec::new();
        let mut rebuilds = Vec::new();
        let mut filters = Vec::new();
        let pulses = Rc::new(Cell::new(0));
        for sample in 0..30 {
            {
                let mut parts = controller.state().get_mut();
                let ComponentParts { model, widgets } = &mut *parts;
                let row = &mut model.sync.rows[sample % count];
                row.title = format!("updated {sample}");
                row.native_preparing = sample % 2 == 0;
                // Mix structural reorder with changed content every fifth update.
                if sample % 5 == 0 {
                    model.sync.rows.rotate_right(1);
                }
                let start = Instant::now();
                model.sync_task_list(widgets);
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                updates.push(elapsed);
                if sample % 5 != 0 {
                    content_updates.push(elapsed);
                }
                let start = Instant::now();
                baseline_full_rebuild(&baseline, &model.sync.rows);
                rebuilds.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            drain();
            // This pulse proves eventual event-loop progress between updates;
            // it does not prove responsiveness during a synchronous update.
            let pulse = pulses.clone();
            gtk::glib::idle_add_local_once(move || pulse.set(pulse.get() + 1));
            let start = Instant::now();
            controller.emit(TasksPanelMsg::DashboardQuery(TASK_0_ONLY.into()));
            drain();
            controller.emit(TasksPanelMsg::ClearDashboardFilters);
            drain();
            filters.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        eprintln!("A07 native {count} tasks raw list-update ms={updates:?}; e188 full-rebuild ms={rebuilds:?}; filter round-trip ms={filters:?}");
        eprintln!("A07 native {count} tasks list update p95={:.3}ms; baseline p95={:.3}ms; filter round-trip p95={:.3}ms; pulses={}", p95(&updates), p95(&rebuilds), p95(&filters), pulses.get());
        eprintln!(
            "A07 native {count} tasks content-only raw ms={content_updates:?}; p95={:.3}ms",
            p95(&content_updates)
        );
        window.close();
        baseline_window.close();
        assert_eq!(pulses.get(), 30);
        assert!(actions.borrow().is_empty());
        assert!(
            p95(&updates) < 16.0,
            "same-runner list-update target exceeded"
        );
    }
}

fn x11_input(args: &[&str]) -> String {
    struct InputChild(Option<std::process::Child>);
    impl Drop for InputChild {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut child = InputChild(Some(
        std::process::Command::new("xdotool")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("hosted fixture requires official xdotool"),
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let context = gtk::glib::MainContext::default();
    loop {
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "xdotool command timed out: {args:?}"
        );
        context.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "xdotool {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn wait_for_gtk(mut predicate: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let context = gtk::glib::MainContext::default();
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "GTK fixture condition timed out"
        );
        context.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
#[ignore = "requires isolated X11 display and official xdotool"]
fn keyed_tasks_keep_focus_and_route_real_keyboard() {
    gtk::init().expect("GTK display");
    let (controller, actions) = panel();
    let mut sync = fixture(100);
    sync.selected = Some(sync.dashboard.rows[0].id);
    set_sync(&controller, sync.clone());
    let window = gtk::Window::new();
    let title = format!(
        "anvil-a07-keyboard-{}-{}",
        std::process::id(),
        TaskId::new()
    );
    window.set_title(Some(&title));
    window.set_default_size(800, 900);
    window.set_child(Some(controller.widget()));
    window.present();
    wait_for_gtk(|| window.is_mapped());
    let pattern = format!("^{title}$");
    let ids = x11_input(&["search", "--sync", "--onlyvisible", "--name", &pattern]);
    let ids = ids.lines().collect::<Vec<_>>();
    assert_eq!(ids.len(), 1);
    let window_id = ids[0];
    x11_input(&["windowfocus", "--sync", window_id]);
    assert_eq!(x11_input(&["getwindowfocus"]).trim(), window_id);
    let selected = sync.selected.unwrap();
    let focused = controller.model().row_widgets[&selected].row.clone();
    focused.grab_focus();
    wait_for_gtk(|| focused.has_focus());
    sync.rows[3].native_preparing = true;
    set_sync(&controller, sync.clone());
    assert!(focused.has_focus(), "content refresh lost focus");
    rotate_order(&mut sync, 1);
    set_sync(&controller, sync.clone());
    assert!(focused.has_focus(), "targeted move lost focus");
    assert_eq!(controller.widgets().task_list.selected_row(), Some(focused));
    assert!(actions.borrow().is_empty());
    for key in ["End", "Home", "Down", "Up"] {
        let target = match key {
            "End" => 99,
            "Down" => 1,
            _ => 0,
        };
        let expected = controller.model().sync.rows[target].id;
        actions.borrow_mut().clear();
        x11_input(&["key", "--clearmodifiers", key]);
        wait_for_gtk(|| !actions.borrow().is_empty());
        assert!(
            matches!(actions.borrow().as_slice(), [TaskPanelAction::Select(id)] if *id == expected)
        );
        sync.selected = Some(expected);
        set_sync(&controller, sync.clone());
        assert!(controller.model().row_widgets[&expected].row.has_focus());
    }
    // Native navigation resolves the filtered table, not the original index.
    controller.emit(TasksPanelMsg::DashboardQuery(TASK_0_ONLY.into()));
    drain();
    sync.selected = Some(controller.model().sync.rows[0].id);
    set_sync(&controller, sync.clone());
    controller
        .widgets()
        .task_list
        .row_at_index(0)
        .unwrap()
        .grab_focus();
    let expected = controller.model().sync.rows.last().unwrap().id;
    actions.borrow_mut().clear();
    x11_input(&["key", "--clearmodifiers", "End"]);
    wait_for_gtk(|| !actions.borrow().is_empty());
    assert!(
        matches!(actions.borrow().as_slice(), [TaskPanelAction::Select(id)] if *id == expected)
    );
    sync.selected = Some(expected);
    set_sync(&controller, sync.clone());
    controller.emit(TasksPanelMsg::ClearDashboardFilters);
    drain();
    // Queue a new order before a genuine row gesture is handled. The queued
    // action must retain the TaskId captured at gesture time, not its old index.
    actions.borrow_mut().clear();
    let clicked = controller.model().sync.rows[3].id;
    let clicked_row = controller.model().row_widgets[&clicked].row.clone();
    rotate_order(&mut sync, 2);
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    controller
        .widgets()
        .task_list
        .select_row(Some(&clicked_row));
    drain();
    assert!(matches!(actions.borrow().as_slice(), [TaskPanelAction::Select(id)] if *id == clicked));
    // A search control keeps input ownership across changed and reordered sync.
    let search = controller.widgets().dashboard_query_entry.clone();
    search.grab_focus();
    rotate_order(&mut sync, 1);
    set_sync(&controller, sync.clone());
    x11_input(&["key", "--clearmodifiers", "s"]);
    wait_for_gtk(|| search.text() == "s");
    assert_eq!(controller.model().sync.selected, sync.selected);
    window.close();
}
