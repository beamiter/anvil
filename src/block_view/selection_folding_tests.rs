// Native regression for the same selection batch used by palette actions.

#[test]
#[ignore = "requires DISPLAY; nonexecuting PTY and real GTK/VTE block widgets"]
fn selected_output_batch_preserves_live_input_focus_and_exact_selection() {
    use super::{BlockData, BlockState, FinishedBlock, FoldSelectionResult};
    use gtk::prelude::*;
    use relm4::gtk;
    use std::collections::HashSet;

    fn settle_gtk() {
        let context = gtk::glib::MainContext::default();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(60);
        while std::time::Instant::now() < until {
            while context.pending() && std::time::Instant::now() < until {
                context.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    gtk::init().expect("GTK display");
    let view = super::organism_settings_test_view();
    let config = view.config.borrow().clone();
    let insertion_point = view.block_list.last_child();
    for id in 1..=3 {
        let output = if id == 3 {
            ""
        } else {
            "first row\r\nsecond row\r\n"
        };
        let data = BlockData {
            id,
            prompt: "$ ".into(),
            cmd: format!("command-{id}"),
            cmd_markup: None,
            output: output.into(),
            exit_code: Some(0),
            lifecycle_schema: super::blocks::BLOCK_LIFECYCLE_SCHEMA,
            completion_provenance: super::CompletionProvenance::ShellReported.into(),
            start_mark_seen: true,
            estimated_height: 200,
            line_count: if output.is_empty() { 0 } else { 2 },
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: Some(5),
            cwd: None,
            cols: 80,
            command_exact: true,
            command_truncated: false,
            output_head_dropped: false,
        };
        let card = FinishedBlock::new(
            id,
            "$ ",
            &data.cmd,
            None,
            output,
            Some(0),
            &config,
            Some(5),
            None,
            None,
            80,
        );
        card.widget()
            .insert_before(&view.block_list, insertion_point.as_ref());
        view.finished_blocks.borrow_mut().push(card);
        view.block_data.borrow_mut().push_back(data);
    }
    for scope in [
        super::find::CrossBlockSearchScope::All,
        super::find::CrossBlockSearchScope::Command,
    ] {
        let browse = view
            .cross_block_search_in_scope(
                "",
                super::find::CrossBlockSearchOptions::default(),
                scope,
                500,
                &super::BlockFilters::default(),
            )
            .expect("empty query browses without enabling a filter");
        assert_eq!(
            browse
                .hits
                .iter()
                .map(|hit| hit.block_id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(browse.hits.iter().all(|hit| !hit.is_output));
    }
    assert!(
        view.cross_block_search_in_scope(
            &"x".repeat(super::prepared_search::MAX_QUERY_BYTES + 1),
            super::find::CrossBlockSearchOptions::default(),
            super::find::CrossBlockSearchScope::All,
            500,
            &super::BlockFilters::default(),
        )
        .is_err(),
        "direct callers also enforce the query byte limit"
    );
    let outside = gtk::Entry::new();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&outside);
    column.append(&view.widget());
    let window = gtk::Window::builder()
        .default_width(800)
        .default_height(600)
        .child(&column)
        .build();
    window.present();
    settle_gtk();
    assert!(
        outside.grab_focus(),
        "the test must acquire real entry focus"
    );
    let focus = gtk::prelude::GtkWindowExt::focus(&window).expect("focused entry descendant");
    assert!(focus == outside.clone().upcast::<gtk::Widget>() || focus.is_ancestor(&outside));
    *view.typed_cmd.borrow_mut() = "unfinished live input".into();
    view.bstate.set(BlockState::CollectingOutput);
    view.user_scrolled_up.set(true);
    view.unread_count.set(7);
    let selected = HashSet::from([1, 3]);
    *view.selected_block_ids.borrow_mut() = selected.clone();
    view.selected_block_id.set(Some(1));
    view.selection_anchor_id.set(Some(3));
    let unselected_height = view.block_data.borrow()[1].estimated_height;

    assert_eq!(
        view.set_selected_blocks_collapsed(true),
        FoldSelectionResult::Changed(1)
    );
    assert!(view.finished_blocks.borrow()[0].is_output_collapsed());
    assert!(!view.finished_blocks.borrow()[1].is_output_collapsed());
    assert!(
        !view.finished_blocks.borrow()[2].is_output_collapsed(),
        "no-output cards stay unchanged"
    );
    assert_eq!(
        view.block_data.borrow()[1].estimated_height,
        unselected_height
    );
    assert_eq!(
        view.set_selected_blocks_collapsed(true),
        FoldSelectionResult::NoChange
    );
    assert_eq!(*view.selected_block_ids.borrow(), selected);
    assert_eq!(view.selected_block_id.get(), Some(1));
    assert_eq!(view.selection_anchor_id.get(), Some(3));
    assert_eq!(view.typed_cmd.borrow().as_str(), "unfinished live input");
    assert_eq!(view.bstate.get(), BlockState::CollectingOutput);
    assert_eq!(view.unread_count.get(), 7);
    assert!(view.user_scrolled_up.get());
    settle_gtk();
    assert_eq!(
        gtk::prelude::GtkWindowExt::focus(&window),
        Some(focus),
        "queued fold allocation must not steal the external editor's focus"
    );

    view.selected_block_ids.borrow_mut().insert(99);
    assert_eq!(
        view.set_selected_blocks_collapsed(false),
        FoldSelectionResult::Stale
    );
    assert!(
        view.finished_blocks.borrow()[0].is_output_collapsed(),
        "stale batch cannot partly unfold"
    );
    view.selected_block_ids.borrow_mut().remove(&99);
    assert_eq!(
        view.set_selected_blocks_collapsed(false),
        FoldSelectionResult::Changed(1)
    );
    view.selected_block_ids.borrow_mut().clear();
    assert_eq!(
        view.set_selected_blocks_collapsed(true),
        FoldSelectionResult::Empty
    );
    assert!(!view.finished_blocks.borrow()[0].is_output_collapsed());
    view.bstate.set(BlockState::AltScreen);
    assert_eq!(
        view.set_selected_blocks_collapsed(true),
        FoldSelectionResult::Unavailable
    );
    view.bstate.set(BlockState::CollectingOutput);
    view.fullscreen.set(true);
    assert_eq!(
        view.set_selected_blocks_collapsed(true),
        FoldSelectionResult::Unavailable
    );
    view.fullscreen.set(false);
    window.close();
}
