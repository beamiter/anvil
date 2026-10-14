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
        settle();
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
            settle();
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
