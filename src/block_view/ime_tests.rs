// Included only inside chat::tests. Real XTest events exercise VTE's private
// GtkIMContext. The established keyboard regression covers commit/cancel.
// The Send regression uses the actual pointer path and checks native IM safety.

fn native_ime_keys(action: &str) {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/qa-native-ime-keyboard.py"
        ))
        .arg(action)
        .output()
        .expect("start fixed-action native IME helper");
    assert!(
        output.status.success(),
        "native IME helper: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    pump_for(Duration::from_millis(180));
}

// Accumulate through short writer-thread scheduling gaps, then reject extras.
fn native_ime_expect(pty: &crate::pty::OwnedPty, expected: &[u8]) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut actual = Vec::new();
    while actual.len() < expected.len() && std::time::Instant::now() < deadline {
        actual.extend(pty.drain_test_slave(std::time::Duration::from_millis(20)));
    }
    assert_eq!(actual, expected, "native IME PTY payload");
    assert!(
        pty.drain_test_slave(std::time::Duration::from_millis(50))
            .is_empty(),
        "duplicate native IME bytes"
    );
}

#[test]
#[ignore = "requires isolated X11 DISPLAY and GTK_IM_MODULE=simple"]
fn native_chat_passthrough_initial_focus_accepts_composition() {
    assert_eq!(std::env::var("GTK_IM_MODULE").as_deref(), Ok("simple"));
    crate::child_env::capture_inherited_environment().expect("test child environment");
    gtk::init().expect("GTK display");
    let view = TermView::new_with_spawner(
        &crate::config::Config::safe_defaults(),
        &crate::config::TerminalMode::Chat,
        &["sh".into()],
        None,
        false,
        None,
        "chat-passthrough-ime-test",
        &[],
        &[],
        |_, _, _, _| crate::pty::OwnedPty::from_openpty(Some(true)),
    )
    .expect("nonexecuting PTY recorder");
    view.block_onboarding.history_resolved(false);
    let sidebar = gtk::Entry::new();
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.append(&sidebar);
    row.append(&view.root);
    let window = gtk::Window::builder()
        .title("anvil-native-ime-qa")
        .default_width(800)
        .default_height(600)
        .child(&row)
        .build();
    assert!(!view.active_vte.is_mapped());
    view.grab_focus();
    window.present();
    wait_for(
        "passthrough terminal maps and receives requested focus",
        || view.active_vte.is_mapped() && view.active_vte.has_focus(),
    );
    pump_for(Duration::from_millis(180));
    assert_ne!(
        view.bstate.get(),
        BlockState::AwaitingCommand,
        "startup must exercise passthrough without a shell prompt"
    );
    view.pty.drain_test_slave(Duration::from_millis(30));
    native_ime_keys("compose");
    assert!(view
        .pty
        .drain_test_slave(Duration::from_millis(30))
        .is_empty());
    native_ime_keys("commit");
    native_ime_expect(&view.pty, "中".as_bytes());
    // Exercise both directions of the real Chat reparent: the initial native
    // overlay docks at a shell prompt, then a foreground fullscreen surface
    // takes over again. Each rebound VTE context must still accept composition.
    for (output, state) in [
        (PROMPT, BlockState::AwaitingCommand),
        (START, BlockState::CollectingOutput),
        (b"\x1b[?1049h".as_slice(), BlockState::AltScreen),
    ] {
        drive(&view, output, state);
        assert!(
            view.active_vte.has_focus(),
            "Chat reparent must preserve its own logical focus"
        );
        view.pty.drain_test_slave(Duration::from_millis(30));
        native_ime_keys("compose");
        assert!(view
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        native_ime_keys("commit");
        native_ime_expect(&view.pty, "中".as_bytes());
    }
    sidebar.grab_focus();
    pump_for(Duration::from_millis(100));
    assert!(
        gtk::prelude::GtkWindowExt::focus(&window)
            .is_some_and(|focus| focus == *sidebar.upcast_ref::<gtk::Widget>()
                || focus.is_ancestor(&sidebar)),
        "retired startup request must not steal sidebar focus"
    );
    window.close();
    view.pty.kill();
}

#[test]
#[ignore = "requires isolated X11 DISPLAY and GTK_IM_MODULE=simple; run through test-gtk-display.sh"]
fn native_ime_commit_cancel_and_independent_ptys() {
    assert_eq!(std::env::var("GTK_IM_MODULE").as_deref(), Ok("simple"));
    // Exercise the new Block dock and existing Chat dock with independent
    // native IM contexts and PTYs in the same window.
    let (first, window, _) = fixture_for_mode(crate::config::TerminalMode::Block);
    let (second, second_window, _) = fixture();
    let old_parent = second
        .root
        .parent()
        .unwrap()
        .downcast::<gtk::Box>()
        .unwrap();
    old_parent.remove(&second.root);
    second_window.close();
    let parent = first.root.parent().unwrap().downcast::<gtk::Box>().unwrap();
    parent.append(&second.root);
    window.set_title(Some("anvil-native-ime-qa"));
    window.set_default_size(800, 700);
    window.present();
    drive(&first, PROMPT, BlockState::AwaitingCommand);
    drive(&second, PROMPT, BlockState::AwaitingCommand);
    assert_ne!(first.pty.master_fd_raw(), second.pty.master_fd_raw());
    for view in [&first, &second] {
        view.pty.drain_test_slave(Duration::from_millis(30));
    }
    // Commit and cancel in each pane, while its sibling is a byte recorder.
    for (owner, other, ascii, expected_ascii) in [
        (&first, &second, "ascii-a", b"a".as_slice()),
        (&second, &first, "ascii-b", b"b".as_slice()),
    ] {
        assert!(owner.active_vte.grab_focus());
        pump_for(Duration::from_millis(180));
        assert!(owner.active_vte.has_focus());
        native_ime_keys("compose");
        assert!(
            owner
                .pty
                .drain_test_slave(Duration::from_millis(30))
                .is_empty(),
            "preedit must not reach PTY"
        );
        assert!(other
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        native_ime_keys("commit");
        // Return commits Unicode without submitting.
        native_ime_expect(&owner.pty, "中".as_bytes());
        native_ime_keys("commit");
        // The next Return reaches native input exactly once.
        native_ime_expect(&owner.pty, b"\r");
        native_ime_keys("compose");
        assert!(owner
            .pty
            .drain_test_slave(Duration::from_millis(30))
            .is_empty());
        native_ime_keys("cancel");
        assert!(
            owner
                .pty
                .drain_test_slave(Duration::from_millis(30))
                .is_empty(),
            "Escape cancels preedit without sending ESC"
        );
        native_ime_keys(ascii);
        // ASCII editing resumes after cancellation.
        native_ime_expect(&owner.pty, expected_ascii);
        assert!(
            other
                .pty
                .drain_test_slave(Duration::from_millis(50))
                .is_empty(),
            "all native input stays on the owner's PTY"
        );
    }
    window.close();
    first.pty.kill();
    second.pty.kill();
}

// A real pointer click must never bypass native preedit with a raw shell CR.
#[test]
#[ignore = "requires isolated X11 DISPLAY, GTK_IM_MODULE=simple and xdotool"]
fn native_ime_send_button_preserves_composition() {
    assert_eq!(std::env::var("GTK_IM_MODULE").as_deref(), Ok("simple"));
    // Exercise the new Block dock and existing Chat dock with independent
    // native IM contexts and PTYs in the same window.
    let (first, window, _) = fixture_for_mode(crate::config::TerminalMode::Block);
    let (second, second_window, _) = fixture();
    let old_parent = second
        .root
        .parent()
        .unwrap()
        .downcast::<gtk::Box>()
        .unwrap();
    old_parent.remove(&second.root);
    second_window.close();
    let parent = first.root.parent().unwrap().downcast::<gtk::Box>().unwrap();
    parent.append(&second.root);
    window.set_title(Some("anvil-native-ime-qa"));
    window.set_default_size(800, 700);
    window.present();
    drive(&first, PROMPT, BlockState::AwaitingCommand);
    drive(&second, PROMPT, BlockState::AwaitingCommand);
    assert_ne!(first.pty.master_fd_raw(), second.pty.master_fd_raw());
    for view in [&first, &second] {
        view.pty.drain_test_slave(Duration::from_millis(30));
    }

    for (owner, other) in [(&first, &second), (&second, &first)] {
        let send = button(owner, "chat-send");
        assert!(send.is_mapped() && send.is_sensitive());
        let click = || {
            let bounds = send.compute_bounds(&window).expect("mapped Send bounds");
            assert!(bounds.width() > 0.0 && bounds.height() > 0.0);
            let x = (bounds.x() + bounds.width() / 2.0).round() as i32;
            let y = (bounds.y() + bounds.height() / 2.0).round() as i32;
            let found = std::process::Command::new("xdotool")
                .args(["search", "--onlyvisible", "--name", "^anvil-native-ime-qa$"])
                .output()
                .expect("locate isolated QA window");
            assert!(found.status.success());
            let ids = String::from_utf8(found.stdout).unwrap();
            let ids: Vec<_> = ids.lines().collect();
            assert_eq!(
                ids.len(),
                1,
                "only the dedicated QA window may receive input"
            );
            let status = std::process::Command::new("xdotool")
                .args([
                    "mousemove",
                    "--window",
                    ids[0],
                    &x.to_string(),
                    &y.to_string(),
                    "click",
                    "1",
                ])
                .status()
                .expect("native Send pointer click");
            assert!(status.success());
            pump_for(Duration::from_millis(250));
        };
        assert!(owner.active_vte.grab_focus());
        pump_for(Duration::from_millis(250));
        // Positive control: use the production gesture, capture and admission
        // callbacks rather than directly emitting clicked or commit.
        click();
        native_ime_expect(&owner.pty, b"\r");
        assert!(
            owner.active_vte.has_focus(),
            "idle Send retains the input owner"
        );
        native_ime_keys("compose");
        assert!(owner
            .pty
            .drain_test_slave(Duration::from_millis(50))
            .is_empty());
        click();
        assert!(
            owner
                .pty
                .drain_test_slave(Duration::from_millis(100))
                .is_empty(),
            "Send cannot submit or alter an active composition"
        );
        assert!(
            owner.active_vte.has_focus(),
            "blocked Send keeps native candidate ownership"
        );
        native_ime_keys("commit");
        native_ime_expect(&owner.pty, "中".as_bytes());
        // Only the next deliberate Send after native commit may submit.
        click();
        native_ime_expect(&owner.pty, b"\r");
        native_ime_keys("compose");
        assert!(owner
            .pty
            .drain_test_slave(Duration::from_millis(100))
            .is_empty());
        native_ime_keys("cancel");
        assert!(owner
            .pty
            .drain_test_slave(Duration::from_millis(100))
            .is_empty());
        native_ime_keys("ascii-a");
        native_ime_expect(&owner.pty, b"a");
        click();
        native_ime_expect(&owner.pty, b"\r");
        assert!(other
            .pty
            .drain_test_slave(Duration::from_millis(100))
            .is_empty());
    }
    // Hidden-but-live contexts must not be forgotten prematurely. Remapping
    // and native cancellation must recover without any application IM reset.
    assert!(first.active_vte.grab_focus());
    native_ime_keys("compose");
    assert!(super::super::ime_gate::blocked());
    first.root.set_visible(false);
    pump_for(Duration::from_millis(180));
    first.root.set_visible(true);
    pump_for(Duration::from_millis(180));
    assert!(first.active_vte.grab_focus());
    native_ime_keys("cancel");
    assert!(!first
        .pty
        .drain_test_slave(Duration::from_millis(100))
        .contains(&b'\r'));
    assert!(
        !super::super::ime_gate::blocked(),
        "native cancel after remap clears the gate"
    );
    native_ime_keys("compose");
    assert!(super::super::ime_gate::blocked());
    window.close();
    pump_for(Duration::from_millis(180));
    assert!(
        !super::super::ime_gate::blocked(),
        "unrealized contexts cannot strand Send"
    );
    first.pty.kill();
    second.pty.kill();
}
