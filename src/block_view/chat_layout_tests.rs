//! Native geometry checks for the conversation layout.
use super::blocks::FinishedBlock;
use gtk::{prelude::*, Orientation};
use relm4::gtk;
use vte4::TerminalExt;

fn settle() {
    for _ in 0..40 {
        while gtk::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
#[ignore = "requires a GTK display"]
fn chat_bubbles_keep_alignment_through_resize_and_font_changes() {
    gtk::init().expect("GTK display");
    let mut config = crate::config::Config::safe_defaults();
    config.terminal_mode = crate::config::TerminalMode::Chat;
    super::install_block_css(&config);
    let root = gtk::Box::new(Orientation::Vertical, 0);
    root.add_css_class("chat-mode");
    let list = gtk::Box::new(Orientation::Vertical, 12);
    list.add_css_class("block-list");
    let long_command = format!("printf '%s\\n' {}", "some-long-value ".repeat(12));
    let mut cards = Vec::new();
    for (id, command) in [(1, "pwd"), (2, long_command.as_str())] {
        let card = FinishedBlock::new(
            id,
            "$ ",
            command,
            None,
            "/home/user/projects\r\n",
            Some(0),
            &config,
            Some(3),
            Some(1_700_000_000_000),
            Some("/home/user/projects"),
            100,
        );

        list.append(card.widget());
        cards.push(card);
    }
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(&list));
    scroll.set_vexpand(true);
    root.append(&scroll);
    let window = gtk::Window::builder()
        .default_width(900)
        .default_height(600)
        .child(&root)
        .build();
    window.present();
    for width in [900, 420, 760] {
        window.set_default_size(width, 600);
        settle();
        assert!(
            window.width() <= width + 1,
            "chat content must allow the requested window width"
        );
        for card in &cards {
            card.refit_output_to_viewport();
        }
        settle();
        for (i, card) in cards.iter().enumerate() {
            let command = card
                .command_vte
                .ancestor(super::command_row::CommandRow::static_type())
                .expect("command capsule");
            let content = command.parent().expect("card content");
            let bounds = command.compute_bounds(&content).unwrap();
            let content_width = content.width() as f32;
            assert!(
                (bounds.x() + bounds.width() - content_width).abs() <= 1.0,
                "command must meet the right edge at {width}: {bounds:?}, content={content_width}"
            );
            if i == 0 {
                assert!(
                    bounds.width() < content_width * 0.5,
                    "short commands must shrink to content: {bounds:?}"
                );
            }
            let result = content.last_child().unwrap();
            let output = result.compute_bounds(&content).unwrap();
            assert!(
                output.x().abs() <= 1.0 && (output.width() - content_width).abs() <= 1.0,
                "result stays at the left edge and uses the reading width"
            );
            let collapse = card.header_row.last_child().unwrap();
            let control = collapse.compute_bounds(&card.header_row).unwrap();
            assert!(
                (control.x() + control.width() - card.header_row.width() as f32).abs() <= 1.0,
                "collapse control must stay at the far right: {control:?}"
            );
            let rendered = card.command_vte.text_format(vte4::Format::Text).unwrap();
            let normalize = |text: &str| {
                text.chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>()
            };
            let expected = normalize(if i == 0 { "pwd" } else { &long_command });
            assert!(
                normalize(rendered.as_str()).contains(&expected),
                "the complete command must survive wrapping at {width}px: {rendered:?}"
            );
            let rows = super::blocks::output_visual_row_count(
                if i == 0 { "pwd" } else { &long_command },
                card.command_vte.column_count(),
            );
            assert!(
                card.command_vte.height() >= rows as i32 * card.command_vte.char_height() as i32,
                "wrapped command cannot clip: rows={rows}, height={}, cell={}",
                card.command_vte.height(),
                card.command_vte.char_height()
            );
        }
        if let Ok(dir) = std::env::var("CHAT_LAYOUT_SCREENSHOTS") {
            std::fs::create_dir_all(&dir).unwrap();
            let paintable = gtk::WidgetPaintable::new(Some(&root));
            let snapshot = gtk::Snapshot::new();
            paintable.snapshot(&snapshot, root.width() as f64, root.height() as f64);
            let node = snapshot.to_node().unwrap();
            window
                .renderer()
                .unwrap()
                .render_texture(&node, None)
                .save_to_png(format!("{dir}/anvil-chat-{width}.png"))
                .unwrap();
        }
    }
    let before = cards[0]
        .command_vte
        .ancestor(super::command_row::CommandRow::static_type())
        .unwrap()
        .width();
    config.default_font_scale *= 1.25;
    for card in &cards {
        super::alt_screen::apply_snapshot_theme_to_vte(&card.command_vte, &config);
        super::alt_screen::apply_snapshot_theme_to_vte(&card.output_vte, &config);
    }
    settle();
    assert!(
        cards[0]
            .command_vte
            .ancestor(super::command_row::CommandRow::static_type())
            .unwrap()
            .width()
            > before,
        "content-sized capsules must track live font changes"
    );
    window.close();
}
