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
    open_with_config(
        app.upcast_ref(),
        None,
        Vim9EntryRequest::user_action(file.clone()),
        Vim9UiConfig {
            toolbar: true,
            buffer_tabs: true,
            status_bar: true,
            project_panel: true,
            tool_panel: true,
            ..Default::default()
        },
    );
    let window = app.windows().into_iter().next().expect("native IDE window");
    let native_window = window.clone().downcast::<adw::ApplicationWindow>().unwrap();
    let widget: &gtk::Widget = window.upcast_ref();
    let editor = descendants::<vte4::Terminal>(widget)
        .into_iter()
        .next()
        .expect("real VTE editor");
    let save = named_button(widget, "Save (Ctrl+S)");
    wait_for(widget, "authenticated Vim bridge", || save.is_sensitive());

    let native_buffer = descendants::<native_buffer::BufferSurface>(widget)
        .into_iter()
        .next()
        .expect("GTK/Pango buffer paint surface");
    wait_for(widget, "a displayed native buffer frame", || {
        native_buffer.painted_frame().is_some()
    });
    // Deliberately disagree with ANSI colors and disable termguicolors. A
    // terminal repaint would be green; the GUI must receive the exact pink
    // guifg and cyan guisp through the independent semantic render channel.
    editor.feed_child(b":set notermguicolors | hi Normal guifg=#abcdef guibg=#101820 ctermfg=2 ctermbg=0 | hi JTermGuiProbe guifg=#fa5ea9 guibg=#101820 guisp=#2ce5d7 gui=bold,italic,undercurl ctermfg=2 ctermbg=0 cterm=NONE | syn match JTermGuiProbe /before/\r");
    wait_for(
        widget,
        "GUI RGB survives a different terminal palette",
        || {
            native_buffer.painted_frame().is_some_and(|frame| {
                frame.default_style.background == Some(0x101820)
                    && frame.runs.iter().any(|run| {
                        run.text.contains("before")
                            && run.style.foreground == Some(0xfa5ea9)
                            && run.style.special == Some(0x2ce5d7)
                            && run.style.bold
                            && run.style.italic
                            && run.style.undercurl
                    })
            })
        },
    );
    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        let rgb_path = PathBuf::from(path).with_extension("gui-rgb.png");
        assert!(std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(rgb_path)
            .status()
            .expect("capture the real GUI RGB buffer")
            .success());
    }

    // Plugin APIs share the native RGB path while termguicolors remains off.
    editor.feed_child(b":hi JTermPluginProbe guifg=#f1a234 guibg=#304050 guisp=#ee3344 gui=undercurl ctermfg=2 ctermbg=0 | let g:gui_plugin = matchadd('JTermPluginProbe', 'before')\r");
    wait_for(
        widget,
        "plugin regex highlight reaches native GUI paint",
        || {
            native_buffer.painted_frame().is_some_and(|frame| {
                frame.runs.iter().any(|run| {
                    run.text == "before"
                        && run.style.foreground == Some(0xf1a234)
                        && run.style.background == Some(0x304050)
                        && run.style.special == Some(0xee3344)
                        && run.style.undercurl
                })
            })
        },
    );
    editor.feed_child(b":call matchdelete(g:gui_plugin) | let g:gui_plugin = matchaddpos('JTermPluginProbe', [[1,2,3]])\r");
    wait_for(widget, "plugin positional highlight stays native", || {
        native_buffer.painted_frame().is_some_and(|frame| {
            frame
                .runs
                .iter()
                .any(|run| run.text == "efo" && run.style.foreground == Some(0xf1a234))
        })
    });
    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        assert!(std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(PathBuf::from(path).with_extension("gui-plugin.png"))
            .status()
            .expect("capture native plugin highlight")
            .success());
    }
    editor.feed_child(b":call matchdelete(g:gui_plugin)\r");
    wait_for(
        widget,
        "plugin removal restores syntax without editing text",
        || {
            native_buffer.painted_frame().is_some_and(|frame| {
                frame
                    .runs
                    .iter()
                    .any(|run| run.text == "before" && run.style.foreground == Some(0xfa5ea9))
            })
        },
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before\n");

    editor.feed_child(b"i");
    wait_for(widget, "native insert-mode buffer", || {
        native_buffer
            .painted_frame()
            .is_some_and(|frame| frame.mode.starts_with('i'))
    });
    let native_im = named_key_controller(&editor, "vim9-native-input-method")
        .im_context()
        .expect("native composition context is active");
    native_im.emit_by_name::<()>("commit", &[&"中文"]);
    wait_for(
        widget,
        "IME commits UTF-8 once to the real Vim buffer",
        || {
            native_buffer.painted_frame().is_some_and(|frame| {
                frame
                    .runs
                    .iter()
                    .filter(|run| run.line == 1)
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    == "中文before"
            })
        },
    );
    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        assert!(std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(PathBuf::from(path).with_extension("gui-ime.png"))
            .status()
            .expect("capture native Unicode buffer")
            .success());
    }
    editor.feed_child(b"\x1bu");
    wait_for(widget, "Vim undo reverses the native IM commit", || {
        native_buffer.painted_frame().is_some_and(|frame| {
            frame.mode == "n"
                && frame.runs.iter().any(|run| run.text.contains("before"))
                && !frame
                    .runs
                    .iter()
                    .any(|run| run.text.contains('中') || run.text.contains('文'))
        })
    });

    let native_toggle = descendants::<gtk::CheckButton>(widget)
        .into_iter()
        .find(|button| button.label().as_deref() == Some("Native GUI buffer"))
        .expect("native render preference");
    native_toggle.set_active(false);
    wait_for(
        widget,
        "compatibility preference retires the native surface",
        || native_buffer.painted_frame().is_none(),
    );
    native_toggle.set_active(true);
    wait_for(widget, "native buffer subscription resumes", || {
        native_buffer.painted_frame().is_some()
    });

    // Optional X11 integration exercises actual key-event delivery through
    // the native IM filter, not only feed_child or a synthetic commit signal.
    if std::env::var_os("JTERM_VIM9_TEST_REAL_KEYS").is_some() {
        let pid = std::process::id().to_string();
        let found = std::process::Command::new("xdotool")
            .args([
                "search",
                "--all",
                "--onlyvisible",
                "--pid",
                &pid,
                "--name",
                "sample.txt.*Vim9 IDE",
            ])
            .output()
            .expect("find native test window");
        assert!(found.status.success());
        let id = String::from_utf8(found.stdout).unwrap();
        let id = id.lines().next().expect("test X11 window id");
        assert!(std::process::Command::new("xdotool")
            .args(["windowfocus", "--sync", id])
            .status()
            .unwrap()
            .success());
        editor.grab_focus();
        assert!(std::process::Command::new("xdotool")
            .args(["type", "--clearmodifiers", "--delay", "4", "AKEY"])
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("xdotool")
            .args(["key", "--clearmodifiers", "Escape"])
            .status()
            .unwrap()
            .success());
        wait_for(
            widget,
            "X11 keys cross the native IM filter exactly once",
            || {
                native_buffer.painted_frame().is_some_and(|frame| {
                    frame.mode == "n"
                        && frame
                            .runs
                            .iter()
                            .filter(|run| run.line == 1)
                            .map(|run| run.text.as_str())
                            .collect::<String>()
                            == "beforeKEY"
                })
            },
        );
        assert!(std::process::Command::new("xdotool")
            .args(["key", "--clearmodifiers", "u"])
            .status()
            .unwrap()
            .success());
        wait_for(widget, "X11 undo restores the real Vim buffer", || {
            native_buffer.painted_frame().is_some_and(|frame| {
                frame.mode == "n"
                    && frame
                        .runs
                        .iter()
                        .filter(|run| run.line == 1)
                        .map(|run| run.text.as_str())
                        .collect::<String>()
                        == "before"
            })
        });
    }

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
    wait_for(widget, "both Vim split windows remain native", || {
        native_buffer.painted_frame().is_some_and(|frame| {
            frame.windows.len() == 2
                && frame.windows.iter().all(|window| {
                    frame.runs.iter().any(|run| {
                        run.line == 1
                            && run.text.contains("first")
                            && window.view.contains(run.row, run.column)
                    })
                })
        })
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

/// Runs against Vim and the actual GTK snapshot path. The same test may be
/// invoked with GDK_SCALE=2 to validate a device-scaled native editing window.
#[test]
#[ignore = "needs a display and Vim 9 with +channel +job +timers"]
fn native_vim9_split_unicode_visual_and_font_scaling() {
    if !jterm_core::child_env::inherited_environment_is_captured() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
    }
    adw::init().unwrap();
    let app = adw::Application::builder()
        .application_id("io.github.beamiter.Vim9SplitTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let root = std::env::temp_dir().join(format!("vim9-splits-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    // Anchor project discovery even when another test left a marker in /tmp.
    std::fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
    let left = root.join("left.txt");
    let right = root.join("right.txt");
    std::fs::write(&left, "LEFT 中文 cafe\u{301}\nsecond left\n").unwrap();
    std::fs::write(&right, "RIGHT 日本語 e\u{301}\nsecond right\n").unwrap();
    open_with_config(
        app.upcast_ref(),
        None,
        Vim9EntryRequest::user_action(left.clone()),
        Vim9UiConfig::default(),
    );
    let window = app.windows().into_iter().next().expect("native IDE window");
    window.set_default_size(1440, 980);
    let widget: &gtk::Widget = window.upcast_ref();
    let editor = descendants::<vte4::Terminal>(widget)
        .into_iter()
        .next()
        .expect("real Vim PTY");
    let buffer = descendants::<native_buffer::BufferSurface>(widget)
        .into_iter()
        .next()
        .expect("native Pango surface");
    wait_for(widget, "initial native Unicode buffer", || {
        buffer.painted_frame().is_some()
    });

    // Minimal defaults retain the native editor while every legacy workbench
    // action is hidden. Hidden tools must not consume user Vim mappings.
    assert!(!named_button(widget, "Save (Ctrl+S)").is_visible());
    assert!(!named_button(widget, "Quick open project file (Ctrl+Shift+P)").is_visible());
    assert!(descendants::<gtk::Paned>(widget).iter().all(|split| {
        split
            .start_child()
            .is_none_or(|child| !child.is_visible() || child.is::<gtk::Overlay>())
    }));
    let shortcuts = named_key_controller(&window, "vim9-workbench-shortcuts");
    for (key, modifiers) in [
        (gdk::Key::F5, gdk::ModifierType::empty()),
        (gdk::Key::F8, gdk::ModifierType::empty()),
        (
            gdk::Key::F,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
        ),
        (
            gdk::Key::B,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
        ),
    ] {
        assert!(!shortcuts.emit_by_name::<bool>("key-pressed", &[&key, &0u32, &modifiers]));
    }

    // Fixture options are intentionally ordinary supported Vim views. GUI and
    // terminal colors disagree to expose any accidental terminal-grid repaint.
    editor.feed_child(b":set nowrap nonumber norelativenumber nocursorline notermguicolors | hi Normal guifg=#dce8f5 guibg=#17212b ctermfg=2 ctermbg=0 | hi GuiLeft guifg=#fa5ea9 gui=bold,italic ctermfg=2 | hi Visual guibg=#574b90 gui=NONE ctermbg=2 | syn match GuiLeft /LEFT/ | vsplit right.txt\r");
    editor.feed_child(b":set nowrap nonumber norelativenumber nocursorline | hi GuiRight guifg=#4cdec1 gui=underline ctermfg=2 | syn match GuiRight /RIGHT/\r");
    wait_for(
        widget,
        "independent split text and GUI syntax colors",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.windows.len() == 2
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "LEFT" && run.style.foreground == Some(0xfa5ea9))
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "RIGHT" && run.style.foreground == Some(0x4cdec1))
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "中" && run.cells == 2)
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "e\u{301}" && run.cells == 1)
            })
        },
    );
    let first_frame = buffer.painted_frame().unwrap();
    let mut columns: Vec<_> = first_frame.paint_views().collect();
    columns.sort_by_key(|view| view.column);
    let divider = columns[0].column + columns[0].columns;
    assert!(columns[1].column > divider);
    assert!(
        !first_frame.covers(columns[0].row, divider),
        "Vim's vertical divider must remain a live compatibility cell"
    );
    assert!(first_frame.covers(first_frame.cursor.row, first_frame.cursor.column));

    let scene_builds = buffer.scene_build_count();
    editor.feed_child(b"l");
    wait_for(
        widget,
        "cursor-only frame reuses the native text scene",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.revision > first_frame.revision
                    && frame.cursor.column != first_frame.cursor.column
            })
        },
    );
    assert_eq!(
        buffer.scene_build_count(),
        scene_builds,
        "cursor-only updates must not rebuild GTK text and background nodes"
    );

    editor.feed_child(b":set hlsearch | hi Search guibg=#70421f gui=NONE\r/RIGHT\r");
    wait_for(
        widget,
        "Vim search highlight stays in its matching split",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.mode == "n"
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "RIGHT" && run.style.background == Some(0x70421f))
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "LEFT" && run.style.background != Some(0x70421f))
            })
        },
    );
    editor.feed_child(b"gg0v4l");
    wait_for(
        widget,
        "native Visual selection in the active split",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.mode == "v"
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "RIGHT" && run.style.background == Some(0x574b90))
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text == "LEFT" && run.style.background != Some(0x574b90))
            })
        },
    );
    editor.feed_child(b"\x1bgg0iEDIT \x1b");
    wait_for(widget, "actual Vim edit is painted in its split", || {
        buffer.painted_frame().is_some_and(|frame| {
            frame.mode == "n" && frame.runs.iter().any(|run| run.text.contains("EDIT "))
        })
    });
    named_button(widget, "Save (Ctrl+S)").emit_clicked();
    wait_for(
        widget,
        "native Save writes the active real Vim buffer",
        || {
            std::fs::read_to_string(&right)
                .is_ok_and(|text| text == "EDIT RIGHT 日本語 e\u{301}\nsecond right\n")
        },
    );
    assert_eq!(
        std::fs::read_to_string(&left).unwrap(),
        "LEFT 中文 cafe\u{301}\nsecond left\n"
    );

    // A horizontal split nested inside a vertical one must retain both status
    // rows and the outside divider, while every editing pane stays native.
    named_button(widget, "Split editor horizontally").emit_clicked();
    wait_for(widget, "three native Vim windows in a mixed layout", || {
        buffer
            .painted_frame()
            .is_some_and(|frame| frame.windows.len() == 3)
    });
    let capture = |extension: &str| {
        if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
            let clock = window
                .frame_clock()
                .expect("mapped split window frame clock");
            let previous = clock.frame_counter();
            window.queue_draw();
            wait_for(widget, "font and split snapshot reaches the window", || {
                clock.frame_counter() > previous && buffer.painted_frame().is_some()
            });
            gtk::prelude::WidgetExt::display(&window).flush();
            assert!(std::process::Command::new("import")
                .args(["-window", "root"])
                .arg(PathBuf::from(path).with_extension(extension))
                .status()
                .unwrap()
                .success());
        }
    };
    capture("gui-splits-before.png");
    let font_menu = descendants::<gtk::MenuButton>(widget)
        .into_iter()
        .find(|menu| {
            menu.tooltip_text().as_deref() == Some("Editor rendering, font and line spacing")
        })
        .unwrap();
    font_menu.popup();
    let popover = font_menu.popover().unwrap();
    let font = descendants::<gtk::FontDialogButton>(popover.upcast_ref())
        .into_iter()
        .next()
        .unwrap();
    let previous_metrics = (editor.char_width(), editor.char_height());
    let previous_frame = buffer.painted_frame().unwrap();
    font.set_font_desc(&gtk::pango::FontDescription::from_string("Monospace 18"));
    font_menu.popdown();
    wait_for(
        widget,
        "larger font resizes the actual Vim grid and all panes",
        || {
            editor.char_height() > previous_metrics.1
                && buffer.painted_frame().is_some_and(|frame| {
                    frame.windows.len() == 3
                        && frame.revision > previous_frame.revision
                        && frame.columns < previous_frame.columns
                        && frame.rows == editor.row_count() as usize
                        && frame.columns == editor.column_count() as usize
                        && frame.covers(frame.cursor.row, frame.cursor.column)
                })
        },
    );
    let larger_frame = buffer.painted_frame().unwrap();
    eprintln!(
        "Native font scaling at GTK scale {}: cell {:?} -> {:?}, Vim grid {}x{} -> {}x{}, revision {} -> {}",
        window.scale_factor(),
        previous_metrics,
        (editor.char_width(), editor.char_height()),
        previous_frame.columns,
        previous_frame.rows,
        larger_frame.columns,
        larger_frame.rows,
        previous_frame.revision,
        larger_frame.revision,
    );
    capture("gui-splits.png");

    let before_style = buffer.painted_frame().unwrap();
    let before_metrics = (editor.char_width(), editor.char_height());
    let mut styled_font = font.font_desc().unwrap();
    styled_font.set_weight(gtk::pango::Weight::Bold);
    font.set_font_desc(&styled_font);
    wait_for(
        widget,
        "font style changes receive a fresh native keyframe",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.revision > before_style.revision
                    && frame.rows == editor.row_count() as usize
                    && frame.columns == editor.column_count() as usize
            })
        },
    );
    if before_metrics == (editor.char_width(), editor.char_height()) {
        let styled = buffer.painted_frame().unwrap();
        assert_eq!(styled.rows, before_style.rows);
        assert_eq!(styled.columns, before_style.columns);
    }

    let geometry_matches = |frame: &jterm_core::vim9::Vim9RenderFrame| {
        frame.rows == editor.row_count() as usize
            && frame.columns == editor.column_count() as usize
            && buffer.painted_geometry().is_some_and(|(metrics, caret)| {
                metrics.cell_width == editor.char_width() as f32
                    && metrics.line_height == editor.char_height() as f32
                    && Some(caret) == metrics.cursor_rect(&frame.cursor)
            })
    };
    let zoom = named_key_controller(&editor, "vim9-buffer-zoom");
    let before_zoom = buffer.painted_frame().unwrap().revision;
    assert!(zoom.emit_by_name::<bool>(
        "key-pressed",
        &[&gdk::Key::equal, &0u32, &gdk::ModifierType::CONTROL_MASK,]
    ));
    assert_eq!(font.font_desc().unwrap().size(), 19 * gtk::pango::SCALE);
    wait_for(
        widget,
        "keyboard zoom refreshes native buffer geometry",
        || {
            buffer
                .painted_frame()
                .is_some_and(|frame| frame.revision > before_zoom && geometry_matches(&frame))
        },
    );
    let before_reset = buffer.painted_frame().unwrap().revision;
    assert!(zoom.emit_by_name::<bool>(
        "key-pressed",
        &[&gdk::Key::_0, &0u32, &gdk::ModifierType::CONTROL_MASK,]
    ));
    assert_eq!(font.font_desc().unwrap().size(), 12 * gtk::pango::SCALE);
    wait_for(
        widget,
        "keyboard zoom reset refreshes native buffer geometry",
        || {
            buffer
                .painted_frame()
                .is_some_and(|frame| frame.revision > before_reset && geometry_matches(&frame))
        },
    );

    // An unsupported feature in any pane must retire every native overlay,
    // never leave a stale valid pane over Vim's updated screen.
    editor.feed_child(b":setlocal conceallevel=2\r");
    wait_for(
        widget,
        "unsupported split state retires the native overlay",
        || {
            buffer.painted_frame().is_none()
                && descendants::<gtk::Label>(widget)
                    .iter()
                    .any(|label| label.text().starts_with("Vim ·"))
        },
    );
    editor.feed_child(b":setlocal conceallevel=0\r");
    wait_for(widget, "native mixed split layout resumes", || {
        buffer
            .painted_frame()
            .is_some_and(|frame| frame.windows.len() == 3)
    });
    window.close();
    wait_for(widget, "clean split session closes", || {
        app.windows().is_empty()
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "needs a display and Vim 9 with +channel +job +timers"]
fn native_vim9_fractional_scroll_and_split_isolation() {
    if !jterm_core::child_env::inherited_environment_is_captured() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
    }
    adw::init().unwrap();
    let app = adw::Application::builder()
        .application_id("io.github.beamiter.Vim9ScrollTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let root = std::env::temp_dir().join(format!("vim9-scroll-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
    let file = root.join("long.txt");
    std::fs::write(
        &file,
        (1..=400)
            .map(|line| format!("row {line:04} 中文 cafe\u{301}\n"))
            .collect::<String>(),
    )
    .unwrap();
    open_with_config(
        app.upcast_ref(),
        None,
        Vim9EntryRequest::user_action(file),
        Vim9UiConfig::default(),
    );
    let window = app.windows().into_iter().next().unwrap();
    let widget: &gtk::Widget = window.upcast_ref();
    let editor = descendants::<vte4::Terminal>(widget)
        .into_iter()
        .next()
        .unwrap();
    let buffer = descendants::<native_buffer::BufferSurface>(widget)
        .into_iter()
        .next()
        .unwrap();
    wait_for(widget, "native scroll fixture", || {
        buffer.painted_frame().is_some()
    });
    // Inspect the real GTK snapshot for mode-aware GUI cursor RGB and shape.
    // Cursor changes reuse shaped text; none of this examines VTE's cells.
    let cursor_pixels = |target: u32| -> Option<(usize, usize)> {
        let paintable = gtk::WidgetPaintable::new(Some(&buffer));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(buffer.width()),
            f64::from(buffer.height()),
        );
        let node = snapshot.to_node()?;
        let texture = window.renderer()?.render_texture(&node, None);
        let stride = texture.width() as usize * 4;
        let mut pixels = vec![0; stride * texture.height() as usize];
        texture.download(&mut pixels, stride);
        let positions: Vec<_> = pixels
            .chunks_exact(4)
            .enumerate()
            .filter(|(_, p)| {
                (u32::from(p[2]) << 16 | u32::from(p[1]) << 8 | u32::from(p[0])) == target
            })
            .map(|(i, _)| (i % texture.width() as usize, i / texture.width() as usize))
            .collect();
        Some((
            positions.iter().map(|p| p.0).max()? - positions.iter().map(|p| p.0).min()? + 1,
            positions.iter().map(|p| p.1).max()? - positions.iter().map(|p| p.1).min()? + 1,
        ))
    };
    editor.feed_child(b":let g:native_saved_cursor = &guicursor | highlight GuiCursorProbe guifg=#102030 guibg=#e1a273 gui=NONE | set guicursor=n-v:block-GuiCursorProbe,i:ver25-GuiCursorProbe,r:hor30-GuiCursorProbe,a:blinkon0\r");
    wait_for(widget, "native solid cursor highlight", || {
        buffer.painted_frame().is_some_and(|f| {
            f.mode == "n"
                && f.cursor
                    .appearance
                    .as_ref()
                    .is_some_and(|a| a.style.background == Some(0xe1a273))
        }) && cursor_pixels(0xe1a273).is_some()
    });
    let block_cursor = cursor_pixels(0xe1a273).unwrap();
    assert!(
        cursor_pixels(0x102030).is_some(),
        "solid cursor must repaint readable foreground ink"
    );
    editor.feed_child(b"i");
    wait_for(widget, "native insertion cursor shape", || {
        buffer.painted_frame().is_some_and(|f| f.mode == "i")
            && cursor_pixels(0xe1a273)
                .is_some_and(|r| r.0 * 2 < block_cursor.0 && r.1 == block_cursor.1)
    });
    editor.feed_child(b"\x1bR");
    wait_for(widget, "native replacement cursor shape", || {
        buffer
            .painted_frame()
            .is_some_and(|f| f.mode.starts_with('R'))
            && cursor_pixels(0xe1a273).is_some_and(|r| r.1 * 2 < block_cursor.1)
    });
    editor.feed_child("\x1b:normal! gg0f中\r".as_bytes());
    wait_for(widget, "wide cursor retains both source cells", || {
        buffer
            .painted_frame()
            .is_some_and(|f| f.mode == "n" && f.cursor.width == 2)
            && cursor_pixels(0xe1a273).is_some_and(|r| r.0 >= block_cursor.0 * 2)
    });
    editor
        .feed_child(b":set guicursor=a:block-GuiCursorProbe-blinkwait250-blinkoff150-blinkon150\r");
    wait_for(widget, "native cursor blink off", || {
        buffer.painted_frame().is_some_and(|f| {
            f.mode == "n"
                && f.cursor
                    .appearance
                    .as_ref()
                    .is_some_and(|a| a.blinkwait == 250)
        }) && cursor_pixels(0xe1a273).is_none()
    });
    let scene_builds = buffer.scene_build_count();
    wait_for(widget, "native cursor blink on", || {
        cursor_pixels(0xe1a273).is_some()
    });
    assert_eq!(
        buffer.scene_build_count(),
        scene_builds,
        "blinking must retain shaped text"
    );
    editor.feed_child(b":let &guicursor = g:native_saved_cursor | normal! gg0\r");
    editor.feed_child(b":set nowrap mouse=a nonumber norelativenumber scrolloff=0 | vsplit\r");
    wait_for(widget, "two independent scroll viewports", || {
        buffer
            .painted_frame()
            .is_some_and(|frame| frame.windows.len() == 2 && frame.mode == "n")
    });
    let frame = buffer.painted_frame().unwrap();
    let active = frame.window;
    let inactive = frame
        .windows
        .iter()
        .find(|view| view.window != active)
        .unwrap();
    let inactive_id = inactive.window;
    let view = &inactive.view;
    let cell_width = editor.char_width() as f64;
    let line_height = editor.char_height() as f64;
    let x = (view.column + 3) as f64 * cell_width;
    let y = (view.row + 3) as f64 * line_height;
    assert!(buffer.enqueue_scroll(x, y, line_height * 0.4, gdk::ScrollUnit::Surface));
    assert!(buffer.enqueue_scroll(x, y, line_height * 0.4, gdk::ScrollUnit::Surface));
    // Fine deltas stay below one source row until their fractional sum crosses
    // the threshold; the host must never round every trackpad event up.
    let context = glib::MainContext::default();
    let until = Instant::now() + Duration::from_millis(80);
    while Instant::now() < until {
        while context.pending() {
            context.iteration(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(buffer
        .painted_frame()
        .unwrap()
        .windows
        .iter()
        .all(|view| view.topline == 1));
    assert!(buffer.enqueue_scroll(x, y, line_height * 0.4, gdk::ScrollUnit::Surface));
    wait_for(
        widget,
        "fractional input scrolls only the hovered split",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame
                    .windows
                    .iter()
                    .any(|view| view.window == inactive_id && view.topline == 2)
            })
        },
    );
    let frame = buffer.painted_frame().unwrap();
    assert_eq!(
        frame.window, active,
        "scrolling an inactive pane must not steal editing focus"
    );
    assert_eq!(
        frame
            .windows
            .iter()
            .find(|view| view.window == active)
            .unwrap()
            .topline,
        1
    );
    for _ in 0..3 {
        assert!(buffer.enqueue_scroll(x, y, 1.0, gdk::ScrollUnit::Wheel));
    }
    let wheel_positions = RefCell::new(Vec::new());
    wait_for(
        widget,
        "wheel burst is coalesced without losing distance",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                let view = frame
                    .windows
                    .iter()
                    .find(|view| view.window == inactive_id)
                    .unwrap();
                wheel_positions
                    .borrow_mut()
                    .push(view.topline as f64 - buffer.scroll_offset(inactive_id) / line_height);
                view.topline == 11
            })
        },
    );
    if buffer.settings().is_gtk_enable_animations() {
        assert!(
            wheel_positions
                .borrow()
                .iter()
                .any(|position| *position > 2.25 && *position < 10.95),
            "the GUI frame clock must paint intermediate source positions: {:?}",
            wheel_positions.borrow()
        );
    }
    if buffer.settings().is_gtk_enable_animations() {
        assert!(
            buffer.scroll_offset(inactive_id).abs() > 0.0,
            "native viewport retains its exact fractional pixel offset"
        );
    }
    // A sustained gesture must keep its easing tail instead of periodically
    // snapping when an outgoing-frame cache fills up.
    for step in 1..=32 {
        assert!(buffer.enqueue_scroll(x, y, 1.0, gdk::ScrollUnit::Wheel));
        let expected = 11 + step * 3;
        wait_for(widget, "sustained smooth scroll", || {
            buffer.painted_frame().is_some_and(|frame| {
                frame
                    .windows
                    .iter()
                    .any(|view| view.window == inactive_id && view.topline == expected)
            })
        });
        if buffer.settings().is_gtk_enable_animations() {
            assert!(buffer.scroll_offset(inactive_id).abs() > 0.0);
        }
    }
    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(window.width()),
            f64::from(window.height()),
        );
        let node = snapshot.to_node().expect("scrolling native scene");
        window
            .renderer()
            .unwrap()
            .render_texture(&node, None)
            .save_to_png(PathBuf::from(path).with_extension("scroll.png"))
            .unwrap();
    }
    let stop = named_key_controller(&editor, "vim9-buffer-scroll-stop");
    assert!(!stop.emit_by_name::<bool>(
        "key-pressed",
        &[&gdk::Key::i, &0u32, &gdk::ModifierType::empty()]
    ));
    assert_eq!(
        buffer.scroll_offset(inactive_id),
        0.0,
        "editing cancels visual interpolation before hit testing"
    );
    editor.feed_child(b":qa!\r");
    wait_for(widget, "scroll fixture exits", || app.windows().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "needs a display and Vim 9 with +channel +job +timers"]
fn native_vim9_shell_argv_and_exit_status() {
    if !jterm_core::child_env::inherited_environment_is_captured() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
    }
    adw::init().unwrap();
    let root = std::env::temp_dir().join(format!("vim9-shell-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let file = root.join("name with spaces.txt");
    let report = root.join("result.txt");
    std::fs::write(
        &file,
        (1..=50)
            .map(|line| format!("line {line}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let program = std::env::var_os("JTERM_VIM9_BIN")
        .or_else(|| std::env::var_os("VIM9_TEST_VIM"))
        .expect("explicit Vim executable for shell fixture");
    let command = format!(
        "call writefile([expand('%:p'), string(line('.'))], '{}')",
        report.to_string_lossy().replace('\'', "''")
    );
    let args = vec![
        "--vim-shell".into(),
        program,
        "--".into(),
        "-Nu".into(),
        "NONE".into(),
        "-n".into(),
        "+37".into(),
        "-c".into(),
        command.into(),
        "-c".into(),
        "cq 7".into(),
        file.clone().into_os_string(),
    ];
    let request = jterm_core::vim9::shell_vim_request(&args).unwrap().unwrap();
    assert_eq!(run_shell(request, "io.github.beamiter.Vim9ShellTest"), 7);
    assert_eq!(
        std::fs::read_to_string(report).unwrap(),
        format!("{}\n37\n", file.display())
    );
    std::fs::remove_dir_all(root).unwrap();
}
