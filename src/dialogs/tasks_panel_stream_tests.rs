//! Hosted-only checks for bounded stream paging and GTK selection semantics.
use super::*;
use std::time::{Duration, Instant};

fn drain() {
    let context = gtk::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(3);
    while context.pending() {
        assert!(Instant::now() < deadline, "stream panel did not settle");
        context.iteration(false);
    }
}

// Wait through two mapped frame ticks so pending text/label allocations have
// settled before comparing geometry. A fixed number of main-loop drains alone
// can finish before the frame clock performs layout.
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
        assert!(
            Instant::now() < deadline,
            "mapped stream layout did not settle"
        );
        drain();
        std::thread::sleep(Duration::from_millis(5));
    }
    drain();
}

fn text(view: &gtk::TextView) -> String {
    let buffer = view.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

fn fixture() -> TasksPanelSync {
    let mut sync = super::tests::dashboard_test_sync();
    let snapshot = crate::agent_task::CodexAppServerViewSnapshot {
        displayed_turn_ordinal: Some(4),
        agent_text: "CURRENT PROGRESS".into(),
        last_error: Some(format!("LATEST ERROR {}", "x".repeat(100_000))),
        turn_history: (1..=3)
            .map(|ordinal| crate::agent_task::CodexAppServerTurnHistory {
                ordinal,
                local_turn_id: crate::agent_task::AgentTurnId::new(),
                follow_up_feedback: None,
                agent_text: format!("HISTORY-{ordinal} {}", "界\u{1b}\u{202e}".repeat(20_000)),
                agent_text_truncated: true,
                commands: Vec::new(),
                file_changes: Vec::new(),
                dropped_updates: 0,
            })
            .collect::<Vec<_>>()
            .into(),
        ..Default::default()
    };
    sync.detail.as_mut().unwrap().stream = Some(std::sync::Arc::new(snapshot));
    sync
}

#[test]
#[ignore = "requires a native GTK display"]
fn stream_paging_preserves_current_and_copies_only_visible_history() {
    gtk::init().expect("GTK display");
    let controller = TasksPanelModel::builder().launch(()).detach();
    let window = gtk::Window::builder()
        .default_width(900)
        .default_height(800)
        .build();
    window.set_child(Some(controller.widget()));
    window.present();
    let mut sync = fixture();
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    let current = controller.widgets().stream_view.clone();
    let history = controller.widgets().history_view.clone();
    let status = controller.widgets().stream_status.clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !status.is_mapped() || status.height() == 0 {
        assert!(Instant::now() < deadline, "stream status did not map");
        drain();
        std::thread::sleep(Duration::from_millis(5));
    }
    let initial = text(&current);
    assert!(status.label().contains("turn 4"));
    assert!(status.label().contains("LATEST ERROR"));
    assert!(initial.contains("CURRENT PROGRESS") && initial.contains("LATEST ERROR"));
    assert!(text(&history).contains("HISTORY-3"));
    assert!(!controller.widgets().history_newer.is_sensitive());
    let current_buffer = current.buffer();
    current_buffer.select_range(&current_buffer.start_iter(), &current_buffer.end_iter());
    controller.widgets().history_older.emit_clicked();
    drain();
    assert!(
        current_buffer.selection_bounds().is_some(),
        "history paging must preserve current selection"
    );
    let history_buffer = history.buffer();
    history_buffer.select_range(&history_buffer.start_iter(), &history_buffer.end_iter());
    let (start, end) = history_buffer
        .selection_bounds()
        .expect("select visible page");
    let selected = history_buffer.text(&start, &end, false);
    assert_eq!(selected.as_str(), text(&history));
    assert!(!selected.contains("HISTORY-1") && !selected.contains("CURRENT PROGRESS"));
    controller.widgets().history_older.emit_clicked();
    drain();
    assert!(text(&history).contains("HISTORY-1"));
    assert!(
        history_buffer.selection_bounds().is_none(),
        "paging must clear old selection"
    );
    assert_eq!(text(&current), initial);
    assert!(
        status.label().len() + initial.len() + text(&history).len()
            <= crate::agent_task_ui::MAX_NATIVE_STREAM_DISPLAY_BYTES
    );
    for unsafe_char in ['\u{1b}', '\u{202e}'] {
        assert!(!text(&history).contains(unsafe_char));
    }
    // Identical Sync preserves history position and selection.
    history_buffer.select_range(&history_buffer.start_iter(), &history_buffer.end_iter());
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert!(text(&history).contains("HISTORY-1"));
    assert!(history_buffer.selection_bounds().is_some());
    // Live Sync changes current output while the selected old history remains stable.
    let previous_history = text(&history);
    let live = std::sync::Arc::make_mut(sync.detail.as_mut().unwrap().stream.as_mut().unwrap());
    live.agent_text = "NEW CURRENT PROGRESS".into();
    live.last_error = Some("NEW LATEST ERROR".into());
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert_eq!(text(&history), previous_history);
    assert!(history_buffer.selection_bounds().is_some());
    assert!(text(&current).contains("NEW CURRENT PROGRESS"));
    assert!(status.label().contains("NEW LATEST ERROR"));
    // A changed current buffer clears its selection, without paging history.
    current_buffer.select_range(&current_buffer.start_iter(), &current_buffer.end_iter());
    assert!(current_buffer.selection_bounds().is_some());
    std::sync::Arc::make_mut(sync.detail.as_mut().unwrap().stream.as_mut().unwrap())
        .agent_text
        .push_str(" AGAIN");
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert!(current_buffer.selection_bounds().is_none());
    assert_eq!(controller.model().history_ordinal, Some(1));
    // Appending retained history preserves the old page and its selection.
    history_buffer.select_range(&history_buffer.start_iter(), &history_buffer.end_iter());
    let live = std::sync::Arc::make_mut(sync.detail.as_mut().unwrap().stream.as_mut().unwrap());
    let mut turns = live.turn_history.to_vec();
    let mut added = turns[2].clone();
    added.ordinal = 4;
    added.agent_text = added.agent_text.replace("HISTORY-3", "HISTORY-4");
    turns.push(added);
    live.turn_history = turns.into();
    live.displayed_turn_ordinal = Some(5);
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert_eq!(text(&history), previous_history);
    assert!(history_buffer.selection_bounds().is_some());
    assert_eq!(controller.model().history_ordinal, Some(1));
    // Eviction moves the cursor to the first surviving turn, with an explicit loss notice.
    let live = std::sync::Arc::make_mut(sync.detail.as_mut().unwrap().stream.as_mut().unwrap());
    live.turn_history = live.turn_history[1..].to_vec().into();
    live.dropped_turns = 1;
    controller.emit(TasksPanelMsg::Sync(Box::new(sync.clone())));
    drain();
    assert!(text(&history).contains("HISTORY-2"));
    assert!(text(&history).contains("1 oldest turns elided"));
    assert!(!controller.widgets().history_older.is_sensitive());
    assert!(history_buffer.selection_bounds().is_none());
    // Scrolling retained text does not move the pinned phase/error summary.
    settle_layout(&window);
    let bounds = status
        .compute_bounds(&window)
        .expect("status in mapped window");
    let adjustment = controller.widgets().history_scroll.vadjustment();
    let range = adjustment.upper() - adjustment.page_size();
    assert!(
        range > 1.0,
        "history fixture must actually overflow its viewport"
    );
    let before_scroll = adjustment.value();
    let target = if before_scroll <= range / 2.0 {
        range
    } else {
        0.0
    };
    adjustment.set_value(target);
    settle_layout(&window);
    assert!(
        (adjustment.value() - before_scroll).abs() > 1.0,
        "history adjustment must really move before checking pinned geometry"
    );
    let after = status
        .compute_bounds(&window)
        .expect("status remains mapped");
    assert_eq!(bounds, after);
    assert!(after.y() >= 0.0 && after.y() + after.height() <= window.height() as f32);
    assert!(status.label().contains("turn 5") && status.label().contains("NEW LATEST ERROR"));
    controller.widgets().history_latest.emit_clicked();
    drain();
    assert!(text(&history).contains("HISTORY-4"));
    // A queued click on the old task must not change the new task's page.
    controller.emit(TasksPanelMsg::Sync(Box::new(fixture())));
    controller.widgets().history_older.emit_clicked();
    drain();
    assert!(text(&history).contains("HISTORY-3"));
    assert_eq!(controller.model().history_ordinal, None);
    window.close();
}
