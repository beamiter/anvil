//! Real record navigation must expose output hits without unfolding command-only targets.
use super::*;
use std::time::{Duration, Instant};

fn settle() {
    let context = gtk::glib::MainContext::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(250) {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn collapsed_output_navigation(mode: crate::config::TerminalMode) {
    crate::child_env::capture_inherited_environment().expect("test child environment");
    gtk::init().expect("GTK display");
    let mut config = Config::safe_defaults();
    config.terminal_mode = mode;
    let view = TermView::new_with_spawner(
        &config,
        &mode,
        &["sh".into()],
        None,
        false,
        None,
        "fold-geometry-test",
        &[],
        &[],
        |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
    )
    .unwrap();
    // A long document keeps the last card parked even after bulk folding.
    for id in 1..=32 {
        let output = "long command output\r\n".repeat(200);
        let card = FinishedBlock::new(
            id,
            "$ ",
            "seq 200",
            None,
            &output,
            Some(0),
            &config,
            None,
            None,
            None,
            80,
        );
        let height = card.set_virtualized(true);
        card.widget()
            .insert_before(&view.block_list, Some(view.active.borrow().widget()));
        view.block_data.borrow_mut().push_back(BlockData {
            id,
            prompt: "$ ".into(),
            cmd: "seq 200".into(),
            cmd_markup: None,
            output,
            exit_code: Some(0),
            lifecycle_schema: blocks::BLOCK_LIFECYCLE_SCHEMA,
            completion_provenance: CompletionProvenance::ShellReported.into(),
            start_mark_seen: true,
            estimated_height: height,
            line_count: 200,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            cols: 80,
            command_exact: true,
            command_truncated: false,
            output_head_dropped: false,
        });
        view.finished_blocks.borrow_mut().push(card);
    }
    let window = gtk::Window::builder()
        .default_width(640)
        .default_height(480)
        .build();
    window.set_child(Some(&view.root));
    window.present();
    // A fresh recorder has no shell prompt. Chat deliberately treats Idle
    // as native passthrough and hides history, so establish the same real
    // OSC 133 prompt lifecycle as the other display-backed Chat fixtures.
    assert_eq!(view.finished_blocks.borrow().len(), 32);
    view.pty.set_test_foreground_group(Some(101));
    view.pty.emit_test_output(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    let deadline = Instant::now() + Duration::from_secs(3);
    while view.bstate.get() != BlockState::AwaitingCommand || !view.block_scroll.is_mapped() {
        assert!(
            Instant::now() < deadline,
            "prompt must expose the history transcript"
        );
        settle();
    }
    settle();
    assert_eq!(
        view.finished_blocks.borrow().len(),
        32,
        "establishing a prompt must not create an extra completed card"
    );
    assert!(view.block_scroll.is_mapped());
    view.scroll_debouncer.user_scrolled_up.set(true);
    view.block_scroll.vadjustment().set_value(0.0);
    view.update_viewport();
    view.update_block_visibility();
    settle();
    assert_eq!(
        view.finished_blocks.borrow()[0].widget().height_request(),
        -1
    );
    assert!(view.finished_blocks.borrow()[0].widget().height() > 1);
    assert!(view.finished_blocks.borrow()[0].widget().is_mapped());
    assert!(view.finished_blocks.borrow()[31].widget().height_request() > 0);

    let searchable_before = view.finished_blocks.borrow()[0].with_searchable_output(str::to_string);
    assert_eq!(view.set_all_blocks_collapsed(true), 32);
    settle();
    assert!(view.finished_blocks.borrow()[0].widget().is_mapped());
    assert!(!view.finished_blocks.borrow()[0].output_vte.is_mapped());
    assert!(view.scroll_debouncer.user_scrolled_up.get());
    // This is the production path used by a command-only palette hit.
    assert_eq!(
        view.navigate_to_record_id(1, false),
        find::RecordNavigationResult::Navigated
    );
    assert!(view
        .finished_blocks
        .borrow()
        .iter()
        .all(|card| card.is_output_collapsed()));
    // The palette passes is_output=true for an output hit, before its
    // exact-occurrence highlighter runs. Only that target may unfold.
    assert_eq!(
        view.navigate_to_record_id(1, true),
        find::RecordNavigationResult::Navigated
    );
    assert!(
        !view.finished_blocks.borrow()[0].is_output_collapsed(),
        "output navigation reported success but left its output hidden"
    );
    assert!(
        view.finished_blocks.borrow()[1..]
            .iter()
            .all(|card| card.is_output_collapsed()),
        "unrelated folds must remain unchanged"
    );
    assert!(
        view.focus_match_in_block(
            1,
            "long command output",
            find::CrossBlockSearchOptions::default(),
            true,
            0,
        ),
        "the palette's immediate highlighter must reach the requested output hit"
    );
    assert_eq!(
        view.finished_blocks.borrow()[0].with_searchable_output(str::to_string),
        searchable_before,
        "navigation must preserve the current searchable output/filter view"
    );
    assert_eq!(
        view.navigate_to_record_id(1, true),
        find::RecordNavigationResult::Navigated
    );
    settle();
    assert!(
        view.finished_blocks.borrow()[0].output_vte.is_mapped(),
        "the selected output must be visible after layout"
    );
    assert!(
        view.scroll_debouncer.user_scrolled_up.get(),
        "search navigation must not resume follow-bottom"
    );
    let parked = view.block_scroll.vadjustment().value();
    view.scroll_debouncer.mark_dirty(&view.block_scroll);
    view.scroll_debouncer
        .pin_to_bottom_deferred(&view.block_scroll);
    settle();
    assert!(
        (view.block_scroll.vadjustment().value() - parked).abs() < 1.0,
        "streaming follow-bottom must respect the search position"
    );
    window.close();
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_output_search_unfolds_only_target() {
    collapsed_output_navigation(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_output_search_unfolds_only_target() {
    collapsed_output_navigation(crate::config::TerminalMode::Chat);
}
