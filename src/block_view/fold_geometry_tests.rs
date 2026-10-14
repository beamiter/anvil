//! Display-backed coverage of the real pane fold actions and parked widgets.
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

// Read-only evidence for CI: distinguish a changed fold from a stale or
// virtualized allocation without altering the assertion or settling interval.
fn trace_first_card(view: &TermView, window: &gtk::Window, stage: &str, collapsed: bool) {
    let cards = view.finished_blocks.borrow();
    let card = &cards[0];
    let widget = card.widget();
    let content = widget.first_child();
    let output = card.output_vte.parent();
    let visibility = |widget: &gtk::Widget| {
        (
            widget.get_visible(),
            widget.is_visible(),
            widget.is_mapped(),
        )
    };
    let adjustment = view.block_scroll.vadjustment();
    let mut visible: Vec<_> = view.visible_indices.borrow().iter().copied().collect();
    visible.sort_unstable();
    let focus = gtk::prelude::GtkWindowExt::focus(window)
        .map(|widget| (widget.widget_name(), widget.type_().name().to_string()));
    let animations = gtk::Settings::default().map(|settings| settings.is_gtk_enable_animations());
    // A size query can refresh GTK's requisition cache. Do it only after the
    // original settling interval, never before the fold gets its layout turn.
    let measured = (stage == "settled selected toggle")
        .then(|| widget.measure(gtk::Orientation::Vertical, widget.width()));
    eprintln!(
        "fold geometry stage={stage} mode={:?} requested_fold={collapsed} actual_fold={} selected={:?} \
         own/effective/mapped={:?} content={:?} output={:?} height={} request={} measure={:?} \
         bounds={:?} metadata={} scroll=({},{},{}) visible={visible:?} locked={} \
         focus={focus:?} focus_visible={} widget_flags={:?} animations={animations:?}",
        view.config.borrow().terminal_mode, card.is_output_collapsed(), view.selected_block_id.get(),
        visibility(widget.upcast_ref()), content.as_ref().map(visibility), output.as_ref().map(visibility),
        widget.height(), widget.height_request(), measured,
        widget.compute_bounds(&view.block_scroll), view.block_data.borrow()[0].estimated_height,
        adjustment.value(), adjustment.upper(), adjustment.page_size(),
        view.scroll_debouncer.user_scrolled_up.get(), window.gets_focus_visible(), widget.state_flags(),
    );
}

#[test]
#[ignore = "requires an isolated GTK process and display"]
fn fold_actions_update_visible_and_parked_geometry() {
    crate::child_env::capture_inherited_environment().expect("test child environment");
    gtk::init().expect("GTK display");
    for mode in [
        crate::config::TerminalMode::Block,
        crate::config::TerminalMode::Chat,
    ] {
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
        // A pane-filtered card remains absent from the document even when its
        // cached fold state changes for a later reveal.
        view.block_data.borrow_mut()[30].estimated_height = 0;
        view.finished_blocks.borrow()[30]
            .widget()
            .set_visible(false);
        // Bulk and selected paths must agree before the next allocation, while
        // GTK still reports the previous visible-card height.
        for selected in [false, true] {
            for collapsed in [true, false] {
                let other_height = view.block_data.borrow()[0].estimated_height;
                view.selected_block_id.set(Some(32));
                if selected {
                    assert!(view.toggle_selected_block_collapsed());
                } else {
                    assert_eq!(view.set_all_blocks_collapsed(collapsed), 32);
                }
                if selected {
                    assert_eq!(
                        view.block_data.borrow()[0].estimated_height,
                        other_height,
                        "a selected fold must preserve unrelated measured geometry"
                    );
                }
                assert_eq!(view.block_data.borrow()[30].estimated_height, 0);
                let expected = {
                    let data = view.block_data.borrow();
                    collapsed_aware_block_height(&config, &data[31], collapsed, 80)
                };
                assert_eq!(view.block_data.borrow()[31].estimated_height, expected);
                assert_eq!(
                    view.finished_blocks.borrow()[31].widget().height_request(),
                    expected
                );
                assert!(
                    view.scroll_debouncer.user_scrolled_up.get(),
                    "folding is not an explicit return-to-live action"
                );
                settle();
                assert!(
                    view.scroll_debouncer.user_scrolled_up.get(),
                    "layout must not cancel history reading"
                );
                assert_eq!(
                    view.finished_blocks.borrow()[31].widget().height_request(),
                    expected
                );
            }
        }
        // The first card is visible and has a real old allocation. A selected
        // fold must not immediately write that allocation over the new estimate.
        view.selected_block_id.set(Some(1));
        for collapsed in [true, false] {
            assert!(
                view.finished_blocks.borrow()[0].widget().is_mapped(),
                "selected visible card must remain in the mapped transcript"
            );
            trace_first_card(&view, &window, "before selected toggle", collapsed);
            let old_height = view.finished_blocks.borrow()[0].widget().height();
            assert!(view.toggle_selected_block_collapsed());
            let expected = {
                let data = view.block_data.borrow();
                collapsed_aware_block_height(&config, &data[0], collapsed, 80)
            };
            assert_eq!(view.block_data.borrow()[0].estimated_height, expected);
            assert_eq!(
                view.finished_blocks.borrow()[0].widget().height_request(),
                -1
            );
            trace_first_card(&view, &window, "immediate selected toggle", collapsed);
            settle();
            trace_first_card(&view, &window, "settled selected toggle", collapsed);
            assert!(
                view.finished_blocks.borrow()[0].widget().is_mapped(),
                "folding must retain the visible card in the mapped transcript"
            );
            let new_height = view.finished_blocks.borrow()[0].widget().height();
            assert!(
                if collapsed {
                    new_height < old_height
                } else {
                    new_height > old_height
                },
                "visible card allocation must follow the fold state: {old_height} -> {new_height}"
            );
            assert!(view.scroll_debouncer.user_scrolled_up.get());
        }
        // New output must not start a bottom pin while history is locked.
        view.scroll_debouncer.user_scrolled_up.set(true);
        let before = view.block_scroll.vadjustment().value();
        view.scroll_debouncer.mark_dirty(&view.block_scroll);
        view.scroll_debouncer
            .pin_to_bottom_deferred(&view.block_scroll);
        settle();
        assert!((view.block_scroll.vadjustment().value() - before).abs() < 0.5);
        window.close();
        settle();
    }
}
