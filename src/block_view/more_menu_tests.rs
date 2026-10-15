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
            view.select_all_blocks();
            let selected = view.selected_block_ids.borrow().clone();
            let card = view.finished_blocks.borrow().last().unwrap().clone();
            let more = card
                .action_box
                .last_child()
                .unwrap()
                .downcast::<gtk::Button>()
                .unwrap();
            assert_eq!(more.action_name().as_deref(), Some("block.show-menu"));
            assert!(more.grab_focus());
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
            more.grab_focus();
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
