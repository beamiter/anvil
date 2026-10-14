// Included only inside chat::tests. Real XTest events exercise VTE's private
// GtkIMContext. This does NOT verify optional Send/Stop during preedit: those
// synthetic byte actions bypass the native filter, whose state is not public.

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
#[ignore = "requires isolated X11 DISPLAY and GTK_IM_MODULE=simple; run through test-gtk-display.sh"]
fn native_ime_commit_cancel_and_independent_ptys() {
    assert_eq!(std::env::var("GTK_IM_MODULE").as_deref(), Ok("simple"));
    let (first, window, _) = fixture();
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
