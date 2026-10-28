//! Display-backed activation of the actual per-card More action.
use super::*;
use std::time::{Duration, Instant};

fn settle() {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(250) {
        while glib::MainContext::default().iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn popovers(card: &gtk::Box) -> Vec<gtk::Popover> {
    let mut found = Vec::new();
    let mut child = card.first_child();
    while let Some(widget) = child {
        if let Ok(popover) = widget.clone().downcast::<gtk::Popover>() {
            found.push(popover);
        }
        child = widget.next_sibling();
    }
    found
}

fn button_named(widget: &gtk::Widget, name: &str) -> Option<gtk::Button> {
    if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
        if button.label().as_deref() == Some(name) {
            return Some(button);
        }
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        if let Some(button) = button_named(&widget, name) {
            return Some(button);
        }
        child = widget.next_sibling();
    }
    None
}

#[test]
#[ignore = "requires DISPLAY; run explicitly under Xvfb"]
fn more_opens_complete_menu_preserves_selection_and_retires_stale_cards() {
    gtk::init().expect("GTK display");
    for mode in [
        crate::config::TerminalMode::Block,
        crate::config::TerminalMode::Chat,
    ] {
        let mut config = Config::safe_defaults();
        config.finished_block_viewport_rows = 8;
        let view = TermView::new_with_spawner(
            &config,
            &mode,
            &["sh".into()],
            None,
            false,
            None,
            "more-menu-test",
            &[],
            &[],
            |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
        )
        .unwrap();
        let outside = gtk::Entry::new();
        let column = gtk::Box::new(Orientation::Vertical, 0);
        column.append(&view.widget());
        column.append(&outside);
        let window = gtk::Window::builder()
            .default_width(700)
            .default_height(500)
            .child(&column)
            .build();
        window.present();
        view.pty.emit_test_output(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        settle();
        for _ in 0..2 {
            view.pty.emit_test_output(b"printf hello\r\n\x1b]133;C\x07");
            settle();
            view.pty.emit_test_output("hello\r\n".repeat(60).as_bytes());
            settle();
            view.pty
                .emit_test_output(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
            settle();
        }
        assert_eq!(view.finished_blocks.borrow().len(), 2);
        view.pty.drain_test_slave(Duration::from_millis(20));
        for restored in [false, true] {
            if restored {
                assert_eq!(view.undo_clear_blocks(), 2);
                settle();
                view.pty.drain_test_slave(Duration::from_millis(20));
            }
            // Selecting a range keeps the live prompt focused and does not
            // scroll history. Reveal the target through the production jump
            // path first: a parked card deliberately hides its controls.
            view.scroll_to_block(1);
            settle();
            view.select_all_blocks();
            settle();
            let selected = view.selected_block_ids.borrow().clone();
            let card = view.finished_blocks.borrow().last().unwrap().clone();
            let more = card.more_btn.clone();
            assert_eq!(more.action_name().as_deref(), Some("block.show-menu"));
            assert!(more.is_mapped(), "the target header must be on screen");
            assert!(
                more.is_sensitive(),
                "the active edge owns enabled actions: mode={mode:?} restored={restored} \
                 card={} selected={:?} active={:?} active_css={} \
                 actions_own_sensitive={} actions_effective_sensitive={} \
                 more_own_sensitive={} copy_effective_sensitive={} action={:?} \
                 target={:?} parent_is_actions={} card_mapped={}",
                card.id,
                selected,
                view.selected_block_id.get(),
                card.widget().has_css_class("block-selection-active"),
                card.action_box.get_sensitive(),
                card.action_box.is_sensitive(),
                more.get_sensitive(),
                card.copy_cmd_btn.is_sensitive(),
                more.action_name(),
                more.action_target_value(),
                more.parent().as_ref() == Some(card.action_box.upcast_ref()),
                card.widget().is_mapped(),
            );
            assert!(more.grab_focus(), "visible More must accept keyboard focus");
            assert_eq!(
                gtk::prelude::GtkWindowExt::focus(&window),
                Some(more.clone().upcast()),
                "More is the actual keyboard focus owner"
            );
            more.emit_clicked();
            settle();
            assert_eq!(*view.selected_block_ids.borrow(), selected);
            let menus = popovers(card.widget());
            assert_eq!(menus.len(), 1);
            let menu = &menus[0];
            for label in [
                "Review block details",
                "Toggle Output Filter",
                "Copy Commands",
                "Copy Outputs",
                "Copy Blocks",
                "Bookmark Block",
                "Export as JSON",
            ] {
                assert!(
                    button_named(menu.upcast_ref(), label).is_some(),
                    "missing {label}"
                );
            }
            if card.long_output {
                assert!(button_named(menu.upcast_ref(), "Jump to Bottom of Block").is_some());
            }
            if card.expand_btn.get_visible() {
                let title = card.expand_btn.tooltip_text().unwrap();
                assert!(
                    button_named(menu.upcast_ref(), &title).is_some(),
                    "viewport expansion is distinct from folding"
                );
            }
            more.emit_clicked();
            settle();
            assert_eq!(popovers(card.widget()), menus, "reuse one live popover");
            outside.grab_focus();
            menu.popdown();
            settle();
            assert!(
                gtk::prelude::GtkWindowExt::focus(&window)
                    .is_some_and(|focus| widget_is_within(&focus, outside.upcast_ref())),
                "close cannot take a newer focus owner"
            );
            assert!(more.is_mapped());
            assert!(
                more.grab_focus(),
                "More remains keyboard reachable after close"
            );
            more.emit_clicked();
            settle();
            let menu = popovers(card.widget()).pop().unwrap();
            let filter = button_named(menu.upcast_ref(), "Toggle Output Filter").unwrap();
            filter.emit_clicked();
            settle();
            assert!(
                card.filter_btn.has_css_class("block-action-active"),
                "old filter control stays alive behind complete menu"
            );
            assert!(
                view.pty
                    .drain_test_slave(Duration::from_millis(20))
                    .is_empty(),
                "menu activation does not write terminal input"
            );
            assert_eq!(view.clear_blocks(), 2);
            settle();
            more.emit_clicked();
            card.widget()
                .activate_action("block.show-menu", None)
                .unwrap();
            settle();
            assert!(
                popovers(card.widget()).is_empty(),
                "retired card cannot reopen or target a replacement"
            );
        }
        window.close();
        settle();
    }
}

#[test]
#[ignore = "requires isolated DISPLAY; real More popover and frame allocation"]
fn more_fold_round_trip_reconciles_the_actual_document_height() {
    gtk::init().expect("GTK display");
    crate::workspace_shell::install_css();
    for mode in [
        crate::config::TerminalMode::Block,
        crate::config::TerminalMode::Chat,
    ] {
        let view = organism_settings_test_view_for_mode(mode, None);
        let center = gtk::Box::new(Orientation::Vertical, 0);
        center.add_css_class("workspace-center");
        center.append(&view.root);
        let window = gtk::Window::builder()
            .default_width(758)
            .default_height(662)
            .child(&center)
            .build();
        window.present();
        let paint_barrier = || {
            let clock = window.frame_clock().expect("native frame clock");
            let frames = Rc::new(Cell::new(0));
            let observed = frames.clone();
            let signal = clock.connect_after_paint(move |_| observed.set(observed.get() + 1));
            let started = Instant::now();
            while frames.get() < 8 && started.elapsed() < Duration::from_secs(3) {
                window.queue_draw();
                while glib::MainContext::default().iteration(false) {}
                std::thread::sleep(Duration::from_millis(2));
            }
            clock.disconnect(signal);
            assert!(frames.get() >= 8, "native allocation and paint barrier");
        };
        paint_barrier();
        view.pty.emit_test_output(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        paint_barrier();
        // This bounded protocol fixture drives the actual reader/backend. No
        // command is executed by the recorder and no metadata is fabricated
        // as an agent task or a claimed test result in the application.
        view.pty.emit_test_output(b"fixture_output\r\n\x1b]133;C;cmdline_url=fixture_output\x07first\r\nsecond\r\nthird\r\nfourth\r\nfifth\r\nsixth\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        paint_barrier();
        assert_eq!(view.finished_blocks.borrow().len(), 1);
        let card = view.finished_blocks.borrow()[0].clone();
        let native_output = card.output_vte.text_format(vte4::Format::Text).unwrap();
        let source_output = view.block_data.borrow()[0].output.clone();
        view.pty.drain_test_slave(Duration::from_millis(20));
        view.scroll_debouncer.user_scrolled_up.set(true);

        let action = |name: &str| {
            let menus = popovers(card.widget());
            assert_eq!(menus.len(), 1, "one actual More popover");
            let button = button_named(menus[0].upcast_ref(), name)
                .unwrap_or_else(|| panic!("actual More action missing: {name}"));
            assert!(button.is_mapped(), "the menu action must really be visible");
            button
        };
        card.more_btn.emit_clicked();
        paint_barrier();
        let expanded = card.widget().height();
        action("Fold Output").emit_clicked();
        paint_barrier();
        assert!(card.is_output_collapsed());
        assert!(!card.output_vte.is_mapped());
        assert!(card.widget().height() < expanded);
        assert_eq!(
            view.block_data.borrow()[0].estimated_height,
            card.widget().height()
        );

        card.more_btn.emit_clicked();
        paint_barrier();
        action("Unfold Output").emit_clicked();
        paint_barrier();
        assert!(!card.is_output_collapsed());
        assert!(card.output_vte.is_mapped());
        assert_eq!(card.widget().height(), expanded);
        assert_eq!(view.block_data.borrow()[0].estimated_height, expanded);
        let indexed_height: i32 = view
            .block_data
            .borrow()
            .iter()
            .map(|record| record.estimated_height)
            .sum();
        assert_eq!(indexed_height, expanded);
        assert_eq!(
            card.output_vte.text_format(vte4::Format::Text).unwrap(),
            native_output
        );
        assert_eq!(view.block_data.borrow()[0].output, source_output);
        assert!(view.scroll_debouncer.user_scrolled_up.get());
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(20))
            .is_empty());
        window.close();
        settle();
    }
}
