// Native coverage for the continuous surface and the production toolbar.

#[test]
#[ignore = "requires DISPLAY; nonexecuting PTY and real Unified GTK/VTE widgets"]
fn unified_navigation_preserves_native_ownership_and_follows_live() {
    use super::{
        BackendRecords, CapturedFinalizeOutput, CommandTextSource, CompletedCommandRecord,
        CompletionProvenance, LazyBlockRenderPayload, RecordNavigationResult, TermView,
    };
    use crate::keybindings::Action;
    use relm4::gtk;
    use vte4::prelude::*;

    fn settle() {
        let context = gtk::glib::MainContext::default();
        let until = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while std::time::Instant::now() < until {
            while context.pending() && std::time::Instant::now() < until {
                context.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    fn record(id: u64, command: &str, background: bool) -> CompletedCommandRecord {
        CompletedCommandRecord {
            id,
            cmd: command.into(),
            exit_code: (!background).then_some(0),
            start_time_ms: Some(id * 1000),
            end_time_ms: Some(id * 1000 + 80),
            duration_ms: (!background).then_some(80),
            cwd: Some("/workspace/project".into()),
            is_background: background,
            completion_provenance: CompletionProvenance::ShellReported,
            command_source: CommandTextSource::ShellReported,
            start_mark_seen: !background,
        }
    }
    fn finalize(view: &TermView, record: &CompletedCommandRecord, output: &str) {
        let ring = Rc::new(RefCell::new(output.bytes().collect()));
        let payload = LazyBlockRenderPayload::new(
            "$ ".into(),
            CapturedFinalizeOutput::Foreground(ring),
            false,
            (80, 24),
        );
        view.render_backend.finalize_block(record, &payload);
        assert_eq!(
            payload.materialization_count(),
            0,
            "Unified stays metadata-only"
        );
    }
    fn owns_focus(window: &gtk::Window, widget: &impl IsA<gtk::Widget>) -> bool {
        gtk::prelude::GtkWindowExt::focus(window)
            .is_some_and(|focus| focus == *widget.as_ref() || focus.is_ancestor(widget))
    }
    fn grid_matches_pty(view: &TermView) -> bool {
        view.pty.test_slave_winsize() == (view.grid_size().0 as u16, view.grid_size().1 as u16)
    }
    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready() && std::time::Instant::now() < deadline {
            settle();
        }
        settle();
    }

    gtk::init().expect("GTK display");
    let view = super::organism_settings_test_view_for_mode(
        crate::config::TerminalMode::Unified,
        Some("Monospace 11"),
    );
    let actions = Rc::new(RefCell::new(Vec::new()));
    TermView::install_unified_navigation(&view, {
        let weak = Rc::downgrade(&view);
        let actions = actions.clone();
        move |action| {
            actions.borrow_mut().push(action);
            let Some(view) = weak.upgrade() else {
                return;
            };
            match action {
                Action::PreviousCommand | Action::NextCommand => {
                    view.navigate_unified_command(action == Action::PreviousCommand)
                        .unwrap();
                }
                Action::ReturnToLive => view.return_to_live().unwrap(),
                Action::CopyCurrentCommand => view.copy_current_unified_command().unwrap(),
                // Find/Browse are routed by the owning terminal component;
                // this fixture records those explicit pane actions below.
                _ => {}
            }
        }
    });
    let outside = gtk::Entry::new();
    outside.set_placeholder_text(Some("Separate editor — focus must stay here during Copy"));
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&outside);
    column.append(&view.widget());
    let window = gtk::Window::builder()
        .title("Anvil · Unified navigation")
        .default_width(900)
        .default_height(600)
        .child(&column)
        .build();
    window.present();
    settle();
    let bar = view.unified_navigation.as_ref().unwrap();
    assert!(bar.root.is_visible());
    assert!(bar.previous.is_focusable());
    assert!(!bar.previous.gets_focus_on_click());
    assert!(super::focused_widget_keeps_key(
        bar.previous.upcast_ref(),
        gtk::gdk::Key::Return,
        gtk::gdk::ModifierType::empty(),
        false,
    ));
    let commands = [
        "git status --short",
        "cargo test --lib",
        "printf 'ready\\n'",
    ];
    for (offset, command) in commands.iter().enumerate() {
        let id = offset as u64 + 1;
        view.render_backend.begin_prompt_zone(id);
        view.render_backend
            .feed_live(format!("\x1b[32m~/project\x1b[0m $ {command}\r\n").as_bytes());
        settle(); // Let VTE prove the visible authenticated head before output scrolls it away.
        let output = (1..=24)
            .map(|row| {
                format!(
                    "  needle {id}.{row:02}  completed output retained in native scrollback\r\n"
                )
            })
            .collect::<String>();
        view.render_backend.feed_live(output.as_bytes());
        settle();
        view.render_backend.close_prompt_zone(Some(id));
        finalize(&view, &record(id, command, false), &output);
        view.return_to_live().unwrap();
        settle();
    }
    view.render_backend
        .feed_live(b"\x1b[32m~/project\x1b[0m $ unfinished draft");
    settle();
    *view.typed_cmd.borrow_mut() = "unfinished draft".into();
    view.return_to_live().unwrap();
    settle();
    assert_eq!(bar.status.text().as_str(), "Live");
    assert!(!view.user_scrolled_up.get());
    assert!(!view.jump_fab.is_visible());
    bar.previous.emit_clicked();
    settle();
    assert_eq!(view.navigated_record_id.get(), Some(3));
    assert!(bar.next.is_sensitive());
    let native_adjustment = view.active_vte.vadjustment().unwrap();
    native_adjustment.emit_by_name::<()>("value-changed", &[]);
    native_adjustment.emit_by_name::<()>("changed", &[]);
    assert_eq!(
        view.navigated_record_id.get(),
        Some(3),
        "unchanged value and range notifications do not retire explicit identity"
    );
    bar.next.emit_clicked();
    assert_eq!(view.navigated_record_id.get(), None);
    assert!(owns_focus(&window, &view.active_vte));

    // Copy is identity-bound and does not touch the ordinary native selection,
    // the input shadow, or a different editor's keyboard focus.
    assert!(matches!(
        view.navigate_to_record_id(2, false),
        RecordNavigationResult::Navigated | RecordNavigationResult::SnapshotView { record_id: 2 }
    ));
    settle();
    assert!(bar.copy.is_sensitive());
    view.active_vte.select_all();
    assert!(view.active_vte.has_selection());
    assert!(outside.grab_focus());
    bar.copy.emit_clicked();
    let copied = gtk::glib::MainContext::default()
        .block_on(view.active_vte.clipboard().read_text_future())
        .unwrap();
    assert_eq!(copied.as_deref(), Some(commands[1]));
    assert!(view.active_vte.has_selection());
    assert!(owns_focus(&window, &outside));
    assert_eq!(view.typed_cmd.borrow().as_str(), "unfinished draft");
    view.active_vte.unselect_all();

    let adjustment = view.active_vte.vadjustment().expect("native adjustment");
    let bottom = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
    assert!(
        bottom > adjustment.lower() + 20.0,
        "fixture must have real scrollback"
    );
    adjustment.set_value(bottom - 15.0);
    settle();
    assert!(view.user_scrolled_up.get());
    assert_eq!(
        view.navigated_record_id.get(),
        None,
        "native scroll retires old command cursor"
    );
    assert!(bar.status.text().starts_with("Reading history"));
    // The outer container has no authority over the native history position.
    view.block_scroll
        .vadjustment()
        .emit_by_name::<()>("value-changed", &[]);
    settle();
    assert!(view.user_scrolled_up.get());

    // Find explicitly clears the command cursor even when the chosen native
    // occurrence stays on the same screen. Completion must invalidate the
    // active query so the component rebuilds it instead of keeping dead hits.
    view.navigated_record_id.set(Some(2));
    let found = view.find_in_blocks("needle", false, false);
    assert!(
        matches!(found, super::find::FindSearchResult::Matches(_)),
        "{found:?}"
    );
    assert_eq!(view.navigated_record_id.get(), None);
    adjustment.set_value(adjustment.lower());
    settle();
    let before = view.unread_count.get();
    finalize(
        &view,
        &record(4, "cargo check --tests", false),
        "Finished dev profile\r\n",
    );
    assert_eq!(view.unread_count.get(), before + 1);
    finalize(&view, &record(5, "", true), "background notice\r\n");
    assert_eq!(
        view.unread_count.get(),
        before + 1,
        "background output is not a completed command"
    );
    assert_eq!(
        view.find_next(),
        super::find::FindNavigationResult::Invalidated
    );
    assert!(matches!(
        view.find_in_blocks("needle", false, false),
        super::find::FindSearchResult::Matches(_)
    ));
    view.navigated_record_id.set(Some(2));
    let _ = view.find_next();
    assert_eq!(
        view.navigated_record_id.get(),
        None,
        "Find step also retires a same-viewport cursor"
    );
    view.clear_find();
    // Use the still-proven continuous surface before resize/TUI invalidates
    // its row projection. An exact zero-hit query must remain observable.
    view.return_to_live().unwrap();
    assert_eq!(
        view.find_in_blocks("new-completion-needle", false, false),
        super::find::FindSearchResult::NoMatches
    );
    adjustment.set_value(adjustment.lower());
    settle();
    view.render_backend
        .feed_live(b"\r\nnew-completion-needle\r\n");
    settle();
    finalize(
        &view,
        &record(6, "printf fresh-result", false),
        "new-completion-needle\r\n",
    );
    assert_eq!(
        view.find_next(),
        super::find::FindNavigationResult::Invalidated,
        "an open zero-hit query must rebuild after completion"
    );
    assert!(matches!(
        view.find_in_blocks("new-completion-needle", false, false),
        super::find::FindSearchResult::Matches(_)
    ));
    view.clear_find();
    adjustment.set_value(adjustment.lower());
    settle();
    bar.find.emit_clicked();
    bar.browse.emit_clicked();
    assert!(actions.borrow().contains(&Action::ToggleSearch));
    assert!(actions.borrow().contains(&Action::CrossBlockSearch));
    outside.set_visible(false);

    // The toolbar has its own allocation at every width, above the VTE. Use
    // the existing optional native screenshot pattern for visual review.
    for (name, width) in [("wide", 900), ("narrow", 360)] {
        window.set_default_size(width, 600);
        wait_until(|| window.width() <= width + 8 && grid_matches_pty(&view));
        let nav = bar.root.compute_bounds(&window).expect("mapped navigation");
        let terminal = view.active_vte.compute_bounds(&window).expect("mapped VTE");
        assert!(
            nav.height() > 0.0 && nav.height() < 60.0,
            "compact independent row: {nav:?}"
        );
        assert!(
            nav.y() + nav.height() <= terminal.y() + 1.0,
            "toolbar must not overlay terminal text"
        );
        assert!(
            window.width() <= width + 8,
            "toolbar must fit requested width {width}; actual {} (toolbar {:?}, pane {:?}, scroller {:?})",
            window.width(),
            bar.root.measure(gtk::Orientation::Horizontal, -1),
            view.root.measure(gtk::Orientation::Horizontal, -1), view.block_scroll.measure(gtk::Orientation::Horizontal, -1)
        );
        for button in [
            &bar.previous,
            &bar.next,
            &bar.find,
            &bar.browse,
            &bar.copy,
            &bar.live,
        ] {
            let bounds = button.compute_bounds(&window).unwrap();
            assert!(bounds.x() >= 0.0 && bounds.x() + bounds.width() <= window.width() as f32);
        }
        assert_eq!(
            view.pty.test_slave_winsize(),
            (view.grid_size().0 as u16, view.grid_size().1 as u16)
        );
        if let Ok(directory) = std::env::var("ANVIL_UNIFIED_SCREENSHOT_DIR") {
            std::fs::create_dir_all(&directory).unwrap();
            let paintable = gtk::WidgetPaintable::new(Some(&window));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(
                &snapshot,
                f64::from(window.width()),
                f64::from(window.height()),
            );
            let node = snapshot.to_node().expect("painted Unified pane");
            let texture = window
                .renderer()
                .expect("native renderer")
                .render_texture(&node, None);
            texture
                .save_to_png(format!("{directory}/anvil-unified-{name}.png"))
                .expect("screenshot");
        }
    }

    // Return to live restores actual VTE focus and scroll position, including
    // after the viewport changed size. It never submits the unfinished draft.
    outside.set_visible(true);
    assert!(outside.grab_focus());
    bar.live.emit_clicked();
    settle();
    assert!(owns_focus(&window, &view.active_vte));
    assert_eq!(view.navigated_record_id.get(), None);
    assert_eq!(view.unread_count.get(), 0);
    assert!(!view.user_scrolled_up.get());
    assert!(
        (adjustment.value()
            - (adjustment.upper() - adjustment.page_size()).max(adjustment.lower()))
        .abs()
            <= 1.0
    );
    assert_eq!(view.typed_cmd.borrow().as_str(), "unfinished draft");

    let rows_with_bar = view.grid_size().1;
    view.bstate.set(BlockState::AltScreen);
    view.render_backend.enter_alt_screen_chrome();
    view.render_backend.enter_fullscreen();
    view.render_backend
        .feed_live(b"\x1b[?1049h\x1b[Hnative full-screen application");
    wait_until(|| {
        !bar.root.is_visible() && view.grid_size().1 > rows_with_bar && grid_matches_pty(&view)
    });
    assert!(!bar.root.is_visible());
    assert!(
        view.grid_size().1 > rows_with_bar,
        "hidden toolbar returns rows to the TUI"
    );
    assert_eq!(
        view.pty.test_slave_winsize(),
        (view.grid_size().0 as u16, view.grid_size().1 as u16)
    );
    assert!(view.return_to_live().is_err());
    assert!(view.copy_current_unified_command().is_err());
    let alt_position = adjustment.value();
    let alt_focus = gtk::prelude::GtkWindowExt::focus(&window);
    assert_eq!(
        view.navigate_to_record_id(2, false),
        RecordNavigationResult::SnapshotView { record_id: 2 }
    );
    assert_eq!(
        adjustment.value(),
        alt_position,
        "read-only Browse fallback cannot move the TUI viewport"
    );
    assert_eq!(gtk::prelude::GtkWindowExt::focus(&window), alt_focus);
    view.render_backend.feed_live(b"\x1b[?1049l");
    view.bstate.set(BlockState::AwaitingCommand);
    view.render_backend.exit_fullscreen();
    view.render_backend.exit_alt_screen_chrome();
    wait_until(|| bar.root.is_visible() && grid_matches_pty(&view));
    assert!(bar.root.is_visible());
    assert_eq!(
        view.pty.test_slave_winsize(),
        (view.grid_size().0 as u16, view.grid_size().1 as u16)
    );

    // Lost row authority must use the explicit retained snapshot fallback;
    // it cannot guess a command's current screen position after a reset.
    let _ = view.find_in_blocks("needle", false, false);
    view.render_backend.hard_reset();
    assert_eq!(
        view.find_next(),
        super::find::FindNavigationResult::Invalidated
    );
    assert_eq!(
        view.navigate_to_record_id(2, false),
        RecordNavigationResult::SnapshotView { record_id: 2 }
    );
    if let BackendRecords::Metadata(zones) = view.render_backend.records() {
        assert_eq!(zones.records.len(), 6);
    }
    assert!(
        view.pty
            .drain_test_slave(std::time::Duration::from_millis(40))
            .is_empty(),
        "navigation, Find and Copy must not write terminal input"
    );
    window.close();
    window.set_child(gtk::Widget::NONE);
    let weak = Rc::downgrade(&view);
    drop(view);
    settle();
    assert!(
        weak.upgrade().is_none(),
        "toolbar and queued refresh must not retain a closed pane"
    );
}
