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
    panic!("Timed out waiting for {description}. Visible labels: {labels:?}");
}

fn named_button(window: &gtk::Widget, tooltip: &str) -> gtk::Button {
    descendants::<gtk::Button>(window)
        .into_iter()
        .find(|button| button.tooltip_text().as_deref() == Some(tooltip))
        .unwrap_or_else(|| panic!("Missing native button {tooltip}"))
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
    open(app.upcast_ref(), None, file.clone());
    let window = app.windows().into_iter().next().expect("native IDE window");
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
    save.emit_clicked();
    wait_for(widget, "native save reaches the file", || {
        std::fs::read_to_string(&file).is_ok_and(|text| text == "NATIVE before\n")
    });

    let build_entry = descendants::<gtk::Entry>(widget)
        .into_iter()
        .find(|entry| entry.placeholder_text().as_deref() == Some("Build command"))
        .expect("native build command");
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
    save.emit_clicked();
    wait_for(widget, "save after refused quit", || {
        std::fs::read_to_string(&file).is_ok_and(|text| text == "UNSAVED NATIVE before\n")
    });
    window.close();
    wait_for(widget, "graceful Vim exit closes the workbench", || {
        app.windows().is_empty()
    });
    std::fs::remove_dir_all(root).unwrap();
}
