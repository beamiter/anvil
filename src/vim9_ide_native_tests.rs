//! Opt-in GTK/VTE coverage using an actual Vim executable.
//! Run the ignored test under Xvfb (or an existing display), with
//! JTERM_VIM9_BIN pointing to Vim 9+ with channel/job/timers.

use super::*;
use std::time::Instant;

fn descendants<T: IsA<gtk::Widget> + glib::object::ObjectType>(root: &gtk::Widget) -> Vec<T> {
    let mut widgets = Vec::new();
    if let Ok(widget) = root.clone().downcast::<T>() {
        widgets.push(widget);
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        widgets.extend(descendants::<T>(&widget));
        child = widget.next_sibling();
    }
    widgets
}

fn wait_for(window: &gtk::Widget, description: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    let context = glib::MainContext::default();
    while Instant::now() < deadline {
        while context.pending() {
            context.iteration(false);
        }
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let labels = descendants::<gtk::Label>(window)
        .iter()
        .rev()
        .take(20)
        .map(|label| label.text().to_string())
        .collect::<Vec<_>>();
    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        let _ = std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(PathBuf::from(path).with_extension("failure.png"))
            .status();
    }
    panic!("Timed out waiting for {description}. Visible labels: {labels:?}");
}

fn named_button(window: &gtk::Widget, tooltip: &str) -> gtk::Button {
    descendants::<gtk::Button>(window)
        .into_iter()
        .find(|button| button.tooltip_text().as_deref() == Some(tooltip))
        .unwrap_or_else(|| panic!("Missing native button {tooltip}"))
}

fn named_key_controller(widget: &impl IsA<gtk::Widget>, name: &str) -> gtk::EventControllerKey {
    let controllers = widget.observe_controllers();
    (0..controllers.n_items())
        .filter_map(|index| controllers.item(index))
        .filter_map(|item| item.downcast::<gtk::EventControllerKey>().ok())
        .find(|controller| controller.name().as_deref() == Some(name))
        .unwrap_or_else(|| panic!("Missing native key controller {name}"))
}

#[test]
#[ignore = "needs a display and Vim 9 with +channel +job +timers"]
fn native_vim9_edit_save_build_and_modified_close() {
    if !jterm_core::child_env::inherited_environment_is_captured() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
    }
    adw::init().unwrap();
    let app = adw::Application::builder()
        .application_id("io.github.beamiter.Vim9WorkbenchTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let root = std::env::temp_dir().join(format!("vim9-native-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
    let file = root.join("sample.txt");
    std::fs::write(&file, "before\n").unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    let nested = root.join("src/chosen.rs");
    std::fs::write(&nested, "first\nsecond\nthird\nfourth\n").unwrap();
    std::fs::write(
        root.join("src/search.rs"),
        "needle\nNeedle\nNEEDLE\nneedles\nneedle42\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.txt"), "needle\n").unwrap();
    open(app.upcast_ref(), None, file.clone());
    let window = app.windows().into_iter().next().expect("native IDE window");
    let native_window = window.clone().downcast::<adw::ApplicationWindow>().unwrap();
    let widget: &gtk::Widget = window.upcast_ref();
    let editor = descendants::<vte4::Terminal>(widget)
        .into_iter()
        .next()
        .expect("real VTE editor");
    let save = named_button(widget, "Save (Ctrl+S)");
    wait_for(widget, "authenticated Vim bridge", || save.is_sensitive());

    // PTY keystrokes mutate real Vim. The native Save button then sends a typed
    // bridge request; the test does not simulate either text editing or saving.
    editor.feed_child(b"gg0iNATIVE \x1b");
    wait_for(widget, "modified editor state", || {
        window.title().is_some_and(|title| title.contains('●'))
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text().starts_with("N ·"))
    });
    let text_search = descendants::<gtk::Entry>(widget)
        .into_iter()
        .find(|entry| entry.placeholder_text().as_deref() == Some("Search project text (Enter)"))
        .expect("native project search field");
    text_search.grab_focus();
    wait_for(widget, "native text field owns keyboard input", || {
        navigation::native_text_has_focus(&native_window)
    });
    let shortcuts = named_key_controller(&window, "vim9-workbench-shortcuts");
    assert!(
        !shortcuts.emit_by_name::<bool>(
            "key-pressed",
            &[&gdk::Key::s, &0u32, &gdk::ModifierType::CONTROL_MASK]
        ),
        "Ctrl+S in a native field must not save a Vim buffer"
    );
    assert!(
        !shortcuts.emit_by_name::<bool>(
            "key-pressed",
            &[&gdk::Key::F5, &0u32, &gdk::ModifierType::empty()]
        ),
        "F5 in a native field must not start a build"
    );
    assert!(
        !shortcuts.emit_by_name::<bool>(
            "key-pressed",
            &[&gdk::Key::Left, &0u32, &gdk::ModifierType::ALT_MASK]
        ),
        "Alt+Left in a native search field must not navigate Vim history"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before\n");
    let field_escape = named_key_controller(&text_search, "vim9-native-field-escape");
    assert!(field_escape.emit_by_name::<bool>(
        "key-pressed",
        &[&gdk::Key::Escape, &0u32, &gdk::ModifierType::empty()]
    ));
    wait_for(widget, "Escape returns native field focus to Vim", || {
        editor.has_focus()
    });
    save.emit_clicked();
    wait_for(widget, "native save reaches the file", || {
        std::fs::read_to_string(&file).is_ok_and(|text| text == "NATIVE before\n")
    });

    // This uses the actual shared asynchronous index and the native fuzzy
    // picker, rather than opening a known path directly through the protocol.
    named_button(widget, "Quick open project file (Ctrl+Shift+P)").emit_clicked();
    wait_for(widget, "native quick-open dialog", || {
        native_window.visible_dialog().is_some()
    });
    let picker = native_window.visible_dialog().unwrap();
    let picker_widget: &gtk::Widget = picker.upcast_ref();
    let picker_query = descendants::<gtk::SearchEntry>(picker_widget)
        .into_iter()
        .next()
        .expect("native quick-open query");
    picker_query.set_text("sample");
    wait_for(widget, "initial indexed file match", || {
        descendants::<gtk::Label>(picker_widget)
            .iter()
            .any(|label| label.text() == "sample.txt")
    });
    // Changing the query and pressing Enter in one event turn must never open
    // the previous query's first result while its debounce timer is pending.
    picker_query.set_text("chsrs");
    picker_query.emit_activate();
    wait_for(widget, "quick-open reaches the nested Vim buffer", || {
        native_window.visible_dialog().is_none()
            && window
                .title()
                .is_some_and(|title| title.contains("chosen.rs"))
    });
    named_button(widget, "Quick open project file (Ctrl+Shift+P)").emit_clicked();
    wait_for(widget, "reopened native picker", || {
        native_window.visible_dialog().is_some()
    });
    let dismissed_picker = native_window.visible_dialog().unwrap();
    descendants::<gtk::SearchEntry>(dismissed_picker.upcast_ref())
        .into_iter()
        .next()
        .expect("picker Escape action")
        .emit_stop_search();
    wait_for(widget, "Escape dismisses the native file picker", || {
        native_window.visible_dialog().is_none()
    });
    named_button(widget, "Go to line (Ctrl+Shift+G)").emit_clicked();
    wait_for(widget, "native go-to dialog", || {
        native_window.visible_dialog().is_some()
    });
    let go_dialog = native_window.visible_dialog().unwrap();
    let go_widget: &gtk::Widget = go_dialog.upcast_ref();
    let position = descendants::<gtk::Entry>(go_widget)
        .into_iter()
        .find(|entry| entry.placeholder_text().as_deref() == Some("Line or line:column"))
        .expect("native position input");
    position.set_text("3:2");
    descendants::<gtk::Button>(go_widget)
        .into_iter()
        .find(|button| button.label().as_deref() == Some("Go"))
        .expect("native Go response")
        .emit_clicked();
    wait_for(widget, "typed cursor navigation reaches Vim", || {
        native_window.visible_dialog().is_none()
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text().contains("Ln 3/4, Col 2"))
    });
    let navigate_back = named_button(widget, "Navigate back (Alt+Left)");
    wait_for(widget, "native back reflects Vim jump history", || {
        navigate_back.is_sensitive()
    });
    navigate_back.emit_clicked();
    wait_for(widget, "back restores the preceding Vim location", || {
        window
            .title()
            .is_some_and(|title| title.contains("chosen.rs"))
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text().contains("Ln 1/4, Col 1"))
    });
    wait_for(widget, "native forward reflects Vim jump history", || {
        named_button(widget, "Navigate forward (Alt+Right)").is_sensitive() && editor.has_focus()
    });
    assert!(shortcuts.emit_by_name::<bool>(
        "key-pressed",
        &[&gdk::Key::Right, &0u32, &gdk::ModifierType::ALT_MASK]
    ));
    wait_for(widget, "Alt+Right restores line and column", || {
        window
            .title()
            .is_some_and(|title| title.contains("chosen.rs"))
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text().contains("Ln 3/4, Col 2"))
    });

    // Set actual native controls and observe real rg results through Vim.
    // No search starts until Enter or the explicit Search button is used.
    let options_menu = descendants::<gtk::MenuButton>(widget)
        .into_iter()
        .find(|menu| menu.tooltip_text().as_deref() == Some("Project search options"))
        .expect("native search options menu");
    options_menu.popup();
    let options_popover = options_menu.popover().expect("search options popover");
    let options_widget: &gtk::Widget = options_popover.upcast_ref();
    let case = descendants::<gtk::DropDown>(options_widget)
        .into_iter()
        .next()
        .expect("native case selector");
    case.set_selected(2); // Ignore case, even for uppercase query text.
    let regex = descendants::<gtk::CheckButton>(options_widget)
        .into_iter()
        .find(|check| check.label().as_deref() == Some("Regular expression"))
        .unwrap();
    regex.set_active(true);
    descendants::<gtk::CheckButton>(options_widget)
        .into_iter()
        .find(|check| check.label().as_deref() == Some("Match whole words"))
        .unwrap()
        .set_active(true);
    let glob = descendants::<gtk::Entry>(options_widget)
        .into_iter()
        .find(|entry| {
            entry.placeholder_text().as_deref() == Some("File pattern, e.g. *.rs or !*.lock")
        })
        .expect("native file glob");
    glob.set_text("*.rs");
    options_menu.popdown();
    text_search.set_text("^NEEDLE$");
    named_button(widget, "Search project using the selected options").emit_clicked();
    wait_for(
        widget,
        "native regex, case, word and file options reach rg",
        || {
            descendants::<gtk::Label>(widget).iter().any(|label| {
                let text = label.text();
                text.starts_with("Search: 3 matches for ^NEEDLE$")
                    && text.contains("regex · ignore case · whole words · files: *.rs")
            })
        },
    );
    assert!(descendants::<gtk::Label>(widget)
        .iter()
        .any(|label| label.text().contains("Quickfix: Search")));
    assert!(
        !descendants::<gtk::Label>(widget)
            .iter()
            .any(|label| { label.text().contains("notes.txt:1:") }),
        "the file glob must exclude the text-file match"
    );
    case.set_selected(1); // Case-sensitive regex now matches only one line.
    text_search.emit_activate();
    wait_for(widget, "native Match case produces one result", || {
        descendants::<gtk::Label>(widget).iter().any(|label| {
            label.text().starts_with("Search: 1 match for ^NEEDLE$")
                && label.text().contains("regex · match case")
        })
    });
    text_search.set_text("[");
    text_search.emit_activate();
    wait_for(widget, "invalid native regex is reported", || {
        descendants::<gtk::Label>(widget)
            .iter()
            .any(|label| label.text().starts_with("Search failed (2):"))
    });
    regex.set_active(false);
    text_search.set_text("absent_fixture_token");
    text_search.emit_activate();
    wait_for(widget, "no matches is a completed search", || {
        descendants::<gtk::Label>(widget).iter().any(|label| {
            label
                .text()
                .starts_with("Search: 0 matches for absent_fixture_token")
        })
    });
    named_button(widget, "Split editor vertically").emit_clicked();
    wait_for(widget, "real Vim vertical split", || {
        descendants::<gtk::Label>(widget)
            .iter()
            .any(|label| label.text().contains("2 windows"))
    });
    editor.feed_child(b"gg0iSAVED \x1b");
    wait_for(widget, "modified nested buffer", || {
        window.title().is_some_and(|title| title.contains('●'))
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text().starts_with("N ·"))
    });
    named_button(widget, "Close current buffer (Ctrl+Shift+W)").emit_clicked();
    wait_for(widget, "native buffer close respects unsaved edits", || {
        descendants::<gtk::Label>(widget).iter().any(|label| {
            let text = label.text();
            text.contains("Unsaved changes")
                || text.contains("No write since last change")
                || text.contains("E89")
                || text.contains("E37")
        })
    });
    assert_eq!(
        std::fs::read_to_string(&nested).unwrap(),
        "first\nsecond\nthird\nfourth\n"
    );

    let build_entry = descendants::<gtk::Entry>(widget)
        .into_iter()
        .find(|entry| entry.placeholder_text().as_deref() == Some("Build command"))
        .expect("native build command");
    build_entry.set_text(
        "i=1; while [ \"$i\" -le 100 ]; do printf 'saved build fixture %s\\n' \"$i\"; i=$((i + 1)); done",
    );
    descendants::<gtk::Button>(widget)
        .into_iter()
        .find(|button| button.label().as_deref() == Some("Save all + Build"))
        .expect("native save-and-build action")
        .emit_clicked();
    wait_for(widget, "save-all completes before the native build", || {
        std::fs::read_to_string(&nested).is_ok_and(|text| text.starts_with("SAVED first\n"))
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text() == "Exited: 0")
    });
    assert!(
        descendants::<gtk::Label>(widget)
            .iter()
            .any(|label| label.text().contains("20 output lines omitted")),
        "the native output panel must disclose its retained tail"
    );
    assert!(
        descendants::<gtk::TextView>(widget).iter().any(|view| {
            let buffer = view.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
            text.contains("saved build fixture 100") && !text.contains("saved build fixture 1\n")
        }),
        "the build output must retain the latest lines"
    );
    // Invoke the close shortcut from the actual focused Vim editor. A native
    // close must resolve Vim's current buffer and preserve modified buffers;
    // queued-navigation ordering is covered by the shared bridge regression.
    editor.grab_focus();
    wait_for(widget, "editor owns the buffer-close shortcut", || {
        editor.has_focus()
    });
    let ctrl_shift = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
    assert!(shortcuts.emit_by_name::<bool>("key-pressed", &[&gdk::Key::W, &0u32, &ctrl_shift]));
    wait_for(
        widget,
        "native buffer close returns to the original file",
        || {
            window
                .title()
                .is_some_and(|title| title.contains("sample.txt") && !title.contains('●'))
                && descendants::<gtk::Label>(widget)
                    .iter()
                    .any(|label| label.text() == "OPEN BUFFERS · 1")
        },
    );

    build_entry.set_text("printf 'sample.txt:1:1: error: native fixture\\n'; exit 2");
    let build = descendants::<gtk::Button>(widget)
        .into_iter()
        .find(|button| button.label().as_deref() == Some("Build · F5"))
        .expect("native build button");
    build.emit_clicked();
    wait_for(widget, "native build exit status", || {
        descendants::<gtk::Label>(widget)
            .iter()
            .any(|label| label.text() == "Exited: 2")
    });
    assert!(
        descendants::<gtk::TextView>(widget).iter().any(|view| {
            let buffer = view.buffer();
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .contains("native fixture")
        }),
        "build output must appear in a native text view"
    );
    assert!(
        descendants::<gtk::Label>(widget).iter().any(|label| {
            label.text().contains("sample.txt:1:1") && label.text().contains("native fixture")
        }),
        "compiler output must populate native diagnostics"
    );

    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        let clock = window.frame_clock().expect("mapped workbench frame clock");
        let frame = clock.frame_counter();
        window.queue_draw();
        wait_for(widget, "native build output paint", || {
            clock.frame_counter() > frame
        });
        gtk::prelude::WidgetExt::display(&window).flush();
        let status = std::process::Command::new("import")
            .arg("-window")
            .arg("root")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "native workbench screenshot");
    }

    editor.feed_child(b"gg0iUNSAVED \x1b");
    wait_for(widget, "second modified buffer", || {
        window.title().is_some_and(|title| title.contains('●'))
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text().starts_with("N ·"))
    });
    window.close();
    wait_for(widget, "Vim rejects closing a modified buffer", || {
        descendants::<gtk::Label>(widget).iter().any(|label| {
            let text = label.text();
            text.contains("No write since last change")
                || text.contains("E37")
                || text.contains("E162")
        })
    });
    assert!(window.is_visible(), "window must survive a refused quit");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "NATIVE before\n");
    assert!(
        shortcuts.emit_by_name::<bool>(
            "key-pressed",
            &[&gdk::Key::s, &0u32, &gdk::ModifierType::CONTROL_MASK]
        ),
        "Ctrl+S in Vim must reach its native save action"
    );
    wait_for(widget, "save after refused quit", || {
        std::fs::read_to_string(&file).is_ok_and(|text| text == "UNSAVED NATIVE before\n")
    });
    window.close();
    wait_for(widget, "graceful Vim exit closes the workbench", || {
        app.windows().is_empty()
    });
    std::fs::remove_dir_all(root).unwrap();
}
