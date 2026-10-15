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

fn retained_scrollback_find(mode: crate::config::TerminalMode) {
    for new_lines in [0, 3, 80] {
        retained_scrollback_case(mode, new_lines);
    }
}

fn retained_scrollback_case(mode: crate::config::TerminalMode, new_lines: usize) {
    super::alt_screen::set_find_render_trace(std::env::var_os("ANVIL_TEST_TRACE_FIND").is_some());
    gtk::init().expect("GTK display");
    let mut config = Config::safe_defaults();
    config.terminal_mode = mode;
    config.preserve_live_scrollback = true;
    let view = TermView::new_with_spawner(
        &config,
        &mode,
        &["sh".into()],
        None,
        false,
        None,
        "live-find-range-test",
        &[],
        &[],
        |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
    )
    .unwrap();
    let window = gtk::Window::builder()
        .default_width(640)
        .default_height(320)
        .build();
    window.set_child(Some(&view.root));
    window.present();
    // Establish the running grid before recording absolute source rows. A
    // synthetic idle-to-running resize after capture is not a shell boundary.
    view.ftcs_seen.set(true);
    view.bstate.set(BlockState::CollectingOutput);
    settle();
    {
        view.active_vte
            .feed("needle old\r\n".repeat(100).as_bytes());
        settle();
        let origin = view.active_vte.cursor_position().1;
        view.active.borrow().live_cursor_origin().set(Some(origin));
        view.active_vte.feed(b"$ run\r\n");
        view.active_vte
            .feed("needle new\r\n".repeat(new_lines).as_bytes());
        settle();
        assert_eq!(
            view.active.borrow().live_cursor_origin().get(),
            Some(origin),
            "layout must preserve the captured origin"
        );
        let result = view.find_in_blocks("needle.*", true, false);
        if new_lines == 0 {
            assert_eq!(result, find::FindSearchResult::NoMatches);
            assert!(!view.active_vte.has_selection());
            window.close();
            settle();
            return;
        }
        let find::FindSearchResult::Matches(progress) = result else {
            panic!("running output must match");
        };
        assert_eq!(
            progress.total, new_lines,
            "old scrollback must not be counted"
        );
        assert_eq!(progress.current, 1);
        assert_eq!(
            view.active_vte.text_selected(vte4::Format::Text).as_deref(),
            Some("needle new")
        );
        // Previous at the first match wraps only within the running range.
        let find::FindNavigationResult::Progress(progress) = view.find_prev() else {
            panic!("wrap backwards");
        };
        assert_eq!(progress.current, new_lines);
        assert_eq!(
            view.active_vte.text_selected(vte4::Format::Text).as_deref(),
            Some("needle new")
        );
        let find::FindNavigationResult::Progress(progress) = view.find_next() else {
            panic!("wrap forwards");
        };
        assert_eq!(progress.current, 1);
        assert_eq!(
            view.active_vte.text_selected(vte4::Format::Text).as_deref(),
            Some("needle new")
        );
        for expected in 2..=new_lines {
            let next = view.find_next();
            let find::FindNavigationResult::Progress(progress) = next else {
                panic!("{new_lines} lines, step {expected}: {next:?}");
            };
            assert_eq!(progress.current, expected);
            assert_eq!(
                view.active_vte.text_selected(vte4::Format::Text).as_deref(),
                Some("needle new")
            );
        }
        let find::FindNavigationResult::Progress(progress) = view.find_prev() else {
            panic!("reverse navigation must keep the same range");
        };
        assert_eq!(progress.current, new_lines - 1);
        // An absent origin conservatively retains the old whole-ring scope.
        view.active.borrow().live_cursor_origin().set(None);
        let find::FindSearchResult::Matches(progress) =
            view.find_in_blocks("needle.*", true, false)
        else {
            panic!("unknown origin must remain searchable");
        };
        assert_eq!(progress.total, 100 + new_lines);
        view.active.borrow().live_cursor_origin().set(Some(origin));
    }
    // A finished surface followed by the live surface exercises returning in
    // both directions, including a logical wrap that must skip old ring rows.
    let card = FinishedBlock::new(
        1,
        "$ ",
        "done",
        None,
        "needle finished\r\n",
        Some(0),
        &config,
        None,
        None,
        None,
        80,
    );
    // Anvil's constructor derives Chat layout directly from config.
    assert_eq!(
        card.widget().has_css_class("chat-turn"),
        mode == crate::config::TerminalMode::Chat,
    );
    card.widget()
        .insert_before(&view.block_list, Some(view.active.borrow().widget()));
    view.finished_blocks.borrow_mut().push(card);
    settle();
    let find::FindSearchResult::Matches(progress) = view.find_in_blocks("needle.*", true, false)
    else {
        panic!("finished and live matches");
    };
    assert_eq!(progress.total, new_lines + 1);
    assert_eq!(progress.current, 1);
    let find::FindNavigationResult::Progress(progress) = view.find_prev() else {
        panic!("return to live backwards");
    };
    assert_eq!(progress.current, new_lines + 1);
    assert_eq!(
        view.active_vte.text_selected(vte4::Format::Text).as_deref(),
        Some("needle new")
    );
    let find::FindNavigationResult::Progress(progress) = view.find_next() else {
        panic!("return to finished");
    };
    assert_eq!(progress.current, 1);
    let find::FindNavigationResult::Progress(progress) = view.find_next() else {
        panic!("return to live forwards");
    };
    assert_eq!(progress.current, 2);
    assert_eq!(
        view.active_vte.text_selected(vte4::Format::Text).as_deref(),
        Some("needle new")
    );
    window.close();
    settle();
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_live_find_excludes_retained_scrollback() {
    retained_scrollback_find(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_live_find_excludes_retained_scrollback() {
    retained_scrollback_find(crate::config::TerminalMode::Chat);
}

fn live_find_observes_native_changes(mode: crate::config::TerminalMode) {
    crate::child_env::capture_inherited_environment().expect("test child environment");
    for change in ["append", "rewrite", "resize"] {
        live_find_change_case(mode, change);
    }
}

fn live_find_change_case(mode: crate::config::TerminalMode, change: &str) {
    gtk::init().expect("GTK display");
    let mut config = Config::safe_defaults();
    config.terminal_mode = mode;
    config.preserve_live_scrollback = true;
    let view = TermView::new_with_spawner(
        &config,
        &mode,
        &["sh".into()],
        None,
        false,
        None,
        "live-find-mutation-test",
        &[],
        &[],
        |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
    )
    .unwrap();
    let window = gtk::Window::builder()
        .default_width(640)
        .default_height(320)
        .build();
    window.set_child(Some(&view.root));
    window.present();
    view.ftcs_seen.set(true);
    view.bstate.set(BlockState::CollectingOutput);
    settle();
    view.active_vte
        .feed("older output\r\n".repeat(40).as_bytes());
    settle();
    let origin = view.active_vte.cursor_position().1;
    view.active.borrow().live_cursor_origin().set(Some(origin));
    view.active_vte.feed(b"$ run\r\nneedle\r\n");
    settle();
    let find::FindSearchResult::Matches(progress) = view.find_in_blocks("needle", false, false)
    else {
        panic!("initial live match");
    };
    assert_eq!(progress.total, 1);
    assert_eq!(
        view.active_vte.text_selected(vte4::Format::Text).as_deref(),
        Some("needle")
    );

    // Ordinary scrolling/selection do not change terminal contents and must
    // not invalidate a query or create a find/rebuild feedback loop.
    let generation = view.contents_generation.get();
    let adjustment = view.active_vte.vadjustment().unwrap();
    adjustment.set_value(adjustment.lower());
    view.active_vte.select_all();
    view.active_vte.unselect_all();
    settle();
    assert_eq!(view.contents_generation.get(), generation);
    assert!(matches!(
        view.find_next(),
        find::FindNavigationResult::Progress(_)
    ));
    assert!(matches!(
        view.find_in_blocks("needle", false, false),
        find::FindSearchResult::Matches(_)
    ));

    match change {
        "append" => view.active_vte.feed(b"needle\r\n"),
        "rewrite" => view.active_vte.feed(b"\x1b[1A\r\x1b[2Kgone\r\n"),
        "resize" => {
            let old_grid = (view.active_vte.row_count(), view.active_vte.column_count());
            window.set_default_size(860, 440);
            settle();
            assert_ne!(
                (view.active_vte.row_count(), view.active_vte.column_count()),
                old_grid,
                "fixture must change the native search geometry"
            );
        }
        _ => unreachable!(),
    }
    settle();
    assert_eq!(
        view.find_next(),
        find::FindNavigationResult::Invalidated,
        "{change}: stale one-hit cursor must not report old progress"
    );
    // This is the existing find bar's recovery path: keep its query and
    // rebuild on Invalidated, without replaying any input to the PTY.
    let result = view.find_in_blocks("needle", false, false);
    if change == "rewrite" {
        assert_eq!(result, find::FindSearchResult::NoMatches);
        assert!(!view.active_vte.has_selection());
    } else {
        let find::FindSearchResult::Matches(progress) = result else {
            panic!("refreshed live match");
        };
        assert_eq!(progress.total, if change == "append" { 2 } else { 1 });
        assert_eq!(
            view.active_vte.text_selected(vte4::Format::Text).as_deref(),
            Some("needle")
        );
        assert!(matches!(
            view.find_next(),
            find::FindNavigationResult::Progress(_)
        ));
    }
    window.close();
    settle();
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn block_live_find_rebuilds_after_native_changes() {
    live_find_observes_native_changes(crate::config::TerminalMode::Block);
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn chat_live_find_rebuilds_after_native_changes() {
    live_find_observes_native_changes(crate::config::TerminalMode::Chat);
}
