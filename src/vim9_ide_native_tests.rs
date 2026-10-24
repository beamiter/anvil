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
    wait_for(widget, "Problems explains a completed empty search", || {
        descendants::<gtk::Label>(widget).iter().any(|label| {
            label.is_visible()
                && label
                    .text()
                    .starts_with("No project matches for “absent_fixture_token”")
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
        descendants::<gtk::Box>(widget).iter().any(|row| {
            row.tooltip_text().is_some_and(|text| {
                text.contains("sample.txt:1:1") && text.contains("native fixture")
            }) && descendants::<gtk::Label>(row.upcast_ref())
                .iter()
                .any(|label| label.text() == "sample.txt:1:1")
        }),
        "diagnostics show a relative location and retain full tooltip text"
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

#[test]
#[ignore = "needs a display and Vim 9 with +channel +job +timers"]
fn native_vim9_quick_open_locations() {
    if !jterm_core::child_env::inherited_environment_is_captured() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
    }
    adw::init().unwrap();
    let app = adw::Application::builder()
        .application_id("io.github.beamiter.Vim9QuickOpenTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let root = std::env::temp_dir().join(format!("vim9-quick-open-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
    let sample = root.join("sample.txt");
    std::fs::write(&sample, "before\n").unwrap();
    std::fs::write(root.join("src/chosen.rs"), "first\nsecond\nthird\nfourth\n").unwrap();
    std::fs::write(root.join("src/notes:12"), "literal colon filename\n").unwrap();
    open_with_config(
        app.upcast_ref(),
        None,
        Vim9EntryRequest::user_action(sample.clone()),
        Vim9UiConfig {
            toolbar: true,
            status_bar: true,
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
    wait_for(widget, "authenticated Vim bridge", || {
        named_button(widget, "Save (Ctrl+S)").is_sensitive()
    });
    let open_picker = || {
        named_button(widget, "Quick open project file (Ctrl+Shift+P)").emit_clicked();
        wait_for(widget, "quick-open dialog", || {
            native_window.visible_dialog().is_some()
        });
        let dialog = native_window.visible_dialog().unwrap();
        let query = descendants::<gtk::SearchEntry>(dialog.upcast_ref())
            .into_iter()
            .next()
            .unwrap();
        (dialog, query)
    };
    let at_position = |position: &str| {
        descendants::<gtk::Label>(widget)
            .iter()
            .any(|label| label.text().contains(position))
    };

    let (picker, query) = open_picker();
    query.set_text("sample");
    wait_for(widget, "initial file match", || {
        descendants::<gtk::Label>(picker.upcast_ref())
            .iter()
            .any(|label| label.text() == "sample.txt")
    });
    // The location and path must both come from the newly typed query even
    // when Enter arrives before SearchEntry's debounce signal.
    query.set_text("chsrs:3:2");
    query.emit_activate();
    wait_for(widget, "quick open file and byte location", || {
        native_window.visible_dialog().is_none()
            && window
                .title()
                .is_some_and(|title| title.contains("chosen.rs"))
            && at_position("Ln 3/4, Col 2")
    });

    let (picker, query) = open_picker();
    query.set_text("sample");
    wait_for(widget, "file row before a changed query", || {
        descendants::<gtk::Label>(picker.upcast_ref())
            .iter()
            .any(|label| label.text() == "sample.txt")
    });
    let list = descendants::<gtk::ListBox>(picker.upcast_ref())
        .into_iter()
        .next()
        .unwrap();
    let stale_row = list.row_at_index(0).unwrap();
    query.set_text(":2:3");
    list.emit_by_name::<()>("row-activated", &[&stale_row]);
    assert!(native_window.visible_dialog().is_some());
    assert!(at_position("Ln 3/4, Col 2"));
    assert_eq!(list.row_at_index(1), None);
    assert!(descendants::<gtk::Label>(picker.upcast_ref())
        .iter()
        .any(|label| label.text() == "Go to line 2, byte column 3 · Current buffer"));
    query.emit_activate();
    wait_for(widget, "current buffer location", || {
        native_window.visible_dialog().is_none()
            && window
                .title()
                .is_some_and(|title| title.contains("chosen.rs"))
            && at_position("Ln 2/4, Col 3")
    });

    let (picker, query) = open_picker();
    query.set_text("chsrs:0");
    query.emit_activate();
    assert!(native_window.visible_dialog().is_some());
    assert!(descendants::<gtk::Label>(picker.upcast_ref())
        .iter()
        .any(|label| label.has_css_class("error")));
    assert!(descendants::<gtk::ListBoxRow>(picker.upcast_ref()).is_empty());
    query.set_text("src/notes:12");
    query.emit_activate();
    wait_for(widget, "literal indexed colon filename", || {
        native_window.visible_dialog().is_none()
            && window
                .title()
                .is_some_and(|title| title.contains("notes:12"))
            && at_position("Ln 1/1, Col 1")
    });

    // Current-buffer navigation is a Goto action, so it must not reopen a
    // named file, reject an unnamed buffer, or overwrite its unsaved text.
    editor.feed_child(b":enew\rione\ntwo\nthree\x1b");
    wait_for(widget, "modified unnamed buffer", || {
        window
            .title()
            .is_some_and(|title| title.contains("[No Name] ●"))
            && at_position("Ln 3/3")
            && descendants::<gtk::Label>(widget)
                .iter()
                .any(|label| label.text().starts_with("N · [No Name]"))
    });
    let (_, query) = open_picker();
    query.set_text(":2:2");
    query.emit_activate();
    wait_for(
        widget,
        "navigation preserves modified unnamed buffer",
        || {
            native_window.visible_dialog().is_none()
                && window
                    .title()
                    .is_some_and(|title| title.contains("[No Name] ●"))
                && at_position("Ln 2/3, Col 2")
        },
    );
    assert_eq!(std::fs::read_to_string(sample).unwrap(), "before\n");
    editor.feed_child(b":qa!\r");
    wait_for(widget, "Vim fixture exits", || app.windows().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "needs a display and Vim 9 with +channel +job +timers"]
fn native_vim9_command_palette_modes_and_live_availability() {
    if !jterm_core::child_env::inherited_environment_is_captured() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
    }
    adw::init().unwrap();
    let app = adw::Application::builder()
        .application_id("io.github.beamiter.Vim9PaletteTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let root = std::env::temp_dir().join(format!("vim9-palette-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
    let file = root.join("sample.txt");
    std::fs::write(&file, "before\n").unwrap();
    std::fs::write(root.join(">notes"), "literal prefix filename\n").unwrap();
    open_with_config(
        app.upcast_ref(),
        None,
        Vim9EntryRequest::user_action(file.clone()),
        Vim9UiConfig::default(),
    );
    let window = app.windows().into_iter().next().unwrap();
    let native_window = window.clone().downcast::<adw::ApplicationWindow>().unwrap();
    let widget: &gtk::Widget = window.upcast_ref();
    let editor = descendants::<vte4::Terminal>(widget)
        .into_iter()
        .next()
        .unwrap();
    let buffer = descendants::<native_buffer::BufferSurface>(widget)
        .into_iter()
        .next()
        .unwrap();
    wait_for(widget, "palette bridge", || {
        named_button(widget, "Save (Ctrl+S)").is_sensitive()
    });
    assert!(!named_button(widget, "Quick open project file (Ctrl+Shift+P)").is_visible());
    let shortcuts = named_key_controller(&window, "vim9-workbench-shortcuts");
    let open_picker = || {
        assert!(
            !shortcuts.emit_by_name::<bool>(
                "key-pressed",
                &[&gdk::Key::p, &0u32, &gdk::ModifierType::CONTROL_MASK]
            ),
            "plain Ctrl+P remains Vim input"
        );
        assert!(
            shortcuts.emit_by_name::<bool>(
                "key-pressed",
                &[
                    &gdk::Key::p,
                    &0u32,
                    &(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK)
                ]
            ),
            "Ctrl+Shift+P opens a dialog without toolbar chrome"
        );
        wait_for(widget, "palette dialog", || {
            native_window.visible_dialog().is_some()
        });
        let dialog = native_window.visible_dialog().unwrap();
        let query = descendants::<gtk::SearchEntry>(dialog.upcast_ref())
            .into_iter()
            .next()
            .unwrap();
        let list = descendants::<gtk::ListBox>(dialog.upcast_ref())
            .into_iter()
            .next()
            .unwrap();
        (dialog, query, list)
    };
    let contains = |dialog: &adw::Dialog, text: &str| {
        descendants::<gtk::Label>(dialog.upcast_ref())
            .iter()
            .any(|label| label.text().contains(text))
    };
    let (picker, query, _) = open_picker();
    assert_eq!(picker.title(), "Quick open project file");
    query.set_text(">save current buffer");
    query.emit_activate();
    // Immediate Enter uses command mode without waiting for the file index
    // or SearchEntry debounce. Name the harmless action explicitly because
    // command recency can legitimately change the unfiltered first result.
    wait_for(widget, "palette save returns focus", || {
        native_window.visible_dialog().is_none() && editor.has_focus()
    });
    let (picker, query, list) = open_picker();
    query.set_text(">");
    wait_for(widget, "command catalog", || {
        contains(&picker, "Save all buffers") && contains(&picker, "Ctrl+Shift+S")
    });
    assert!(!named_button(picker.upcast_ref(), "Refresh project file index").is_visible());
    assert_eq!(picker.title(), "Quick open command");
    assert!(navigation::native_text_has_focus(&native_window));
    assert!(!shortcuts.emit_by_name::<bool>(
        "key-pressed",
        &[&gdk::Key::s, &0u32, &gdk::ModifierType::CONTROL_MASK]
    ));
    query.set_text(">cancel build");
    query.emit_activate();
    assert!(native_window.visible_dialog().is_some());
    assert!(contains(&picker, "No build is running"));
    let settled = Rc::new(Cell::new(false));
    let observed = settled.clone();
    glib::timeout_add_local_once(Duration::from_millis(160), move || observed.set(true));
    wait_for(
        widget,
        "unavailable command survives picker polling",
        || settled.get(),
    );
    assert!(descendants::<gtk::Label>(picker.upcast_ref())
        .iter()
        .any(|label| label.has_css_class("error") && label.text() == "No build is running"));
    query.set_text(">no_command_fixture_token");
    query.emit_activate();
    assert!(native_window.visible_dialog().is_some());
    assert!(list.row_at_index(0).is_none());
    assert!(contains(&picker, "No matching commands"));
    query.set_text(">next diagnostic");
    query.emit_activate();
    assert!(contains(&picker, "The quickfix list has no problems"));
    editor.feed_child(b":call setqflist([{'filename':'sample.txt','lnum':1,'col':1,'text':'palette problem','type':'W'}])\r");
    wait_for(
        widget,
        "live command availability with unchanged query",
        || {
            descendants::<gtk::Label>(list.upcast_ref())
                .iter()
                .any(|label| {
                    label.text() == "Go to next quickfix problem"
                        && !label.has_css_class("dim-label")
                })
        },
    );
    assert!(descendants::<gtk::Label>(picker.upcast_ref())
        .iter()
        .any(|label| {
            label.has_css_class("error") && label.text() == "The quickfix list has no problems"
        }));
    assert_eq!(query.text(), ">next diagnostic");
    query.emit_by_name::<()>("stop-search", &[]);
    wait_for(widget, "Escape returns palette focus", || {
        native_window.visible_dialog().is_none() && editor.has_focus()
    });
    // Prepare a modified buffer without an ambiguous standalone Escape,
    // which can leave Vim waiting for a terminal key sequence after the
    // modified-buffer title has already arrived.
    editor.feed_child(b":call setqflist([]) | call setline(1, 'PALETTE before')\r");
    wait_for(widget, "unsaved palette fixture", || {
        window.title().is_some_and(|title| title.contains('●'))
    });
    let (picker, query, list) = open_picker();
    query.set_text(">save current buffer");
    wait_for(widget, "save row before a file query", || {
        contains(&picker, "Save current buffer")
    });
    let stale_command = list.row_at_index(0).unwrap();
    query.set_text("sample");
    list.emit_by_name::<()>("row-activated", &[&stale_command]);
    assert!(native_window.visible_dialog().is_some());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before\n");
    wait_for(widget, "file results after switching modes", || {
        contains(&picker, "sample.txt")
    });
    let stale_file = list.row_at_index(0).unwrap();
    query.set_text(">vertical split");
    list.emit_by_name::<()>("row-activated", &[&stale_file]);
    assert!(native_window.visible_dialog().is_some());
    assert!(buffer
        .painted_frame()
        .is_some_and(|frame| frame.windows.len() == 1));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "before\n");
    // A detached stale row has index -1 and may not resolve as GTK's last row.
    list.emit_by_name::<()>("row-activated", &[&stale_file]);
    assert!(native_window.visible_dialog().is_some());
    query.emit_activate();
    wait_for(widget, "typed palette split", || {
        native_window.visible_dialog().is_none()
            && buffer
                .painted_frame()
                .is_some_and(|frame| frame.windows.len() == 2)
    });
    editor.feed_child(b"u");
    wait_for(widget, "fixture undo retains disk contents", || {
        window.title().is_some_and(|title| !title.contains('●'))
    });
    // Exercise the optional provider through a real Vim process. The cached
    // context must refresh even when neither the query nor file index changes.
    let provider = root.join("simple-provider.vim");
    let action_output = root.join("simple-action.txt");
    std::fs::write(
        &provider,
        format!(
            "let g:vim9_simple_branch = 'palette-main'\n\
             function! VimrcWorkbenchCapabilities()\n\
               return ['simple_git_status', 'simple_workbench']\n\
             endfunction\n\
             function! VimrcWorkbenchState()\n\
               return {{'provider':'simple', 'git':{{'head':g:vim9_simple_branch,'ahead':1}}, 'diagnostics':{{'warning':2}}}}\n\
             endfunction\n\
             function! VimrcWorkbenchAction(action)\n\
               call writefile([a:action], '{}')\n\
             endfunction\n",
            action_output.display()
        ),
    )
    .unwrap();
    editor.feed_child(format!(":source {}\r", provider.display()).as_bytes());
    let (picker, query, _) = open_picker();
    query.set_text(">simple git status");
    wait_for(widget, "Simple provider in native palette", || {
        contains(&picker, "Simple: Show Git status")
            && contains(&picker, "palette-main ↑1")
            && contains(&picker, "W2")
    });
    let section_button = |name: &str| {
        descendants::<gtk::Button>(picker.upcast_ref())
            .into_iter()
            .find(|button| button.widget_name() == name)
            .unwrap()
    };
    section_button("vim9-section-plugins").emit_clicked();
    wait_for(
        widget,
        "Plugins category filters the current picker",
        || query.text() == ">simpleplug" && contains(&picker, "Plugins"),
    );
    assert!(
        !action_output.exists(),
        "choosing a category must not run a command"
    );
    assert!(navigation::native_text_has_focus(&native_window));
    section_button("vim9-section-git").emit_clicked();
    wait_for(
        widget,
        "Git category shows cached context and typed actions",
        || query.text() == ">simplegit" && contains(&picker, "Simple: Show Git status"),
    );
    assert!(!action_output.exists());
    if let Some(path) = std::env::var_os("JTERM_VIM9_PALETTE_SCREENSHOT") {
        let clock = window.frame_clock().expect("mapped palette frame clock");
        let frame = clock.frame_counter();
        window.queue_draw();
        wait_for(widget, "Simple palette paint", || {
            clock.frame_counter() > frame
        });
        gtk::prelude::WidgetExt::display(&window).flush();
        let captured = std::process::Command::new("import")
            .arg("-window")
            .arg("root")
            .arg(path)
            .status()
            .unwrap();
        assert!(captured.success(), "native Simple palette screenshot");
    }
    query.set_text(">simplegit status");
    editor.feed_child(b":let g:vim9_simple_branch = 'palette-next'\r");
    wait_for(
        widget,
        "Simple context refresh without query change",
        || contains(&picker, "palette-next ↑1") && !contains(&picker, "palette-main"),
    );
    assert_eq!(query.text(), ">simplegit status");
    query.emit_activate();
    wait_for(widget, "Simple typed action returns to Vim", || {
        native_window.visible_dialog().is_none()
            && std::fs::read_to_string(&action_output)
                .is_ok_and(|text| text == "simple_git_status\n")
    });
    assert_eq!(
        std::fs::read_to_string(action_output).unwrap(),
        "simple_git_status\n"
    );
    let (picker, query, list) = open_picker();
    query.set_text(">format");
    wait_for(
        widget,
        "format unavailable before capability refresh",
        || {
            contains(&picker, "Format current buffer")
                && descendants::<gtk::Label>(list.upcast_ref())
                    .iter()
                    .any(|label| {
                        label.text() == "Format current buffer" && label.has_css_class("dim-label")
                    })
        },
    );
    query.emit_activate();
    let format_error = "The current buffer's language service does not provide this action";
    assert!(contains(&picker, format_error));
    editor.feed_child(b":command! LspFormat echo 'format fixture'\r");
    wait_for(
        widget,
        "format capability refresh preserves activation feedback",
        || {
            descendants::<gtk::Label>(list.upcast_ref())
                .iter()
                .any(|label| {
                    label.text() == "Format current buffer" && !label.has_css_class("dim-label")
                })
        },
    );
    assert!(contains(&picker, format_error));
    query.set_text(">simple");
    wait_for(
        widget,
        "query change clears prior activation feedback",
        || contains(&picker, "Simple: Show Git status") && !contains(&picker, format_error),
    );
    query.emit_by_name::<()>("stop-search", &[]);
    wait_for(widget, "format feedback fixture dismissal", || {
        native_window.visible_dialog().is_none()
    });
    let (picker, query, _) = open_picker();
    query.set_text(">simple");
    wait_for(widget, "Simple palette screenshot catalog", || {
        contains(&picker, "Simple: Show Git status") && contains(&picker, "palette-next ↑1")
    });
    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        let clock = window.frame_clock().unwrap();
        let previous = clock.frame_counter();
        window.queue_draw();
        wait_for(widget, "palette reaches a painted window frame", || {
            clock.frame_counter() > previous
        });
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(window.width()),
            f64::from(window.height()),
        );
        window
            .renderer()
            .unwrap()
            .render_texture(snapshot.to_node().unwrap(), None)
            .save_to_png(PathBuf::from(path).with_extension("palette.png"))
            .unwrap();
    }
    query.emit_by_name::<()>("stop-search", &[]);
    wait_for(widget, "catalog dismissal", || {
        native_window.visible_dialog().is_none()
    });
    let (_, query, _) = open_picker();
    query.set_text("./>notes");
    query.emit_activate();
    wait_for(
        widget,
        "literal greater-than filename remains reachable",
        || {
            native_window.visible_dialog().is_none()
                && window.title().is_some_and(|title| title.contains(">notes"))
        },
    );
    editor.feed_child(b":qa!\r");
    wait_for(widget, "palette fixture exit", || app.windows().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "needs an X11 display, xdotool and Vim 9 with +channel +job +timers"]
fn native_embedded_vim9_palette_precedes_parent_and_preserves_sibling_keys() {
    exercise_embedded_vim9_capture(false);
}

#[test]
#[ignore = "needs a display, xdotool and Vim 9 with +channel +job +timers"]
fn native_embedded_vim9_font_capture_routes_keyboard_and_wheel() {
    exercise_embedded_vim9_capture(true);
}

fn exercise_embedded_vim9_capture(font_only: bool) {
    use std::io::Write;
    if !jterm_core::child_env::inherited_environment_is_captured() {
        jterm_core::child_env::capture_inherited_environment().unwrap();
    }
    adw::init().unwrap();
    let app = adw::Application::builder()
        .application_id("io.github.beamiter.Vim9EmbeddedPaletteTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    let root = std::env::temp_dir().join(format!("vim9-embedded-palette-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Makefile"), "all:\n\t@true\n").unwrap();
    std::fs::write(root.join("sample.txt"), "embedded before\n").unwrap();
    let window = adw::ApplicationWindow::builder()
        .application(&app)
        .title("Embedded Vim9 palette test")
        .default_width(1000)
        .default_height(700)
        .build();
    let layout = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
    pane.set_hexpand(true);
    let source = gtk::Entry::new();
    pane.append(&source);
    let sibling = gtk::Entry::new();
    sibling.set_placeholder_text(Some("Sibling terminal input"));
    layout.append(&pane);
    layout.append(&sibling);
    window.set_content(Some(&layout));
    let terminal_palette_count = Rc::new(Cell::new(0));
    let terminal_font_count = Rc::new(Cell::new(0));
    let parent_keys = gtk::EventControllerKey::new();
    parent_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let parent_window = window.downgrade();
    let parent_count = terminal_palette_count.clone();
    let parent_font_count = terminal_font_count.clone();
    parent_keys.connect_key_pressed(move |_, key, _, modifiers| {
        let window = parent_window.upgrade().unwrap();
        // Same boundary as the real terminal window's Capture callback. Real
        // X11 events below must pass it before reaching embedded content.
        if native_dialog_owns_keyboard(&window)
            || embedded_owns_quick_open_key(&window, key, modifiers)
            || embedded_owns_font_key(&window, key, modifiers)
        {
            return glib::Propagation::Proceed;
        }
        if native_buffer::font_zoom_key(key, modifiers).is_some() {
            parent_font_count.set(parent_font_count.get() + 1);
            return glib::Propagation::Stop;
        }
        if key.to_unicode().map(|key| key.to_ascii_lowercase()) == Some('p')
            && modifiers.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK)
        {
            parent_count.set(parent_count.get() + 1);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(parent_keys);
    let parent_wheel_count = Rc::new(Cell::new(0));
    let wheel_count = parent_wheel_count.clone();
    let parent_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    parent_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    parent_scroll.connect_scroll(move |controller, _, _| {
        let control = controller
            .current_event_state()
            .contains(gdk::ModifierType::CONTROL_MASK);
        if crate::vim9_zoom::owns_wheel(
            control,
            controller
                .widget()
                .is_some_and(|root| embedded_owns_font_scroll(&root)),
        ) {
            return glib::Propagation::Proceed;
        }
        if control {
            wheel_count.set(wheel_count.get() + 1);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    layout.add_controller(parent_scroll);
    window.present();
    // Obtain an ordinary authenticated shell completion so the real
    // EmbeddedHost attach/finish path is exercised without a second GUI.
    let integration =
        jterm_core::vim9::Vim9ShellIntegration::prepare(&std::env::current_exe().unwrap()).unwrap();
    let environment = integration
        .env()
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
    let mut connection =
        std::os::unix::net::UnixStream::connect(&environment["JTERM_VIM9_HOST_SOCKET"]).unwrap();
    // The authenticated shell boundary requires a real absolute executable,
    // whereas the ordinary session launcher may resolve a bare PATH name.
    let requested = std::env::var_os("JTERM_VIM9_BIN").unwrap_or_else(|| "vim".into());
    let requested = std::path::PathBuf::from(requested);
    let program = if requested.components().count() > 1 || requested.is_absolute() {
        requested
    } else {
        use std::os::unix::fs::PermissionsExt;
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(&requested))
            .find(|path| {
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .expect("Vim executable on PATH")
    };
    let program = program.canonicalize().expect("absolute Vim executable");
    let argv = [
        "--vim-shell".into(),
        program.into_os_string(),
        "--".into(),
        "sample.txt".into(),
    ];
    serde_json::to_writer(
        &mut connection,
        &serde_json::json!({
            "token": environment["JTERM_VIM9_HOST_TOKEN"],
            "cwd": root,
            "argv": argv,
            "tty": "/dev/pts/1"
        }),
    )
    .unwrap();
    connection.write_all(b"\n").unwrap();
    let invocation = RefCell::new(None);
    wait_for(
        window.upcast_ref(),
        "authenticated embedded invocation",
        || {
            if invocation.borrow().is_none() {
                *invocation.borrow_mut() = jterm_core::vim9::take_shell_vim_requests()
                    .into_iter()
                    .next();
            }
            invocation.borrow().is_some()
        },
    );
    let invocation = invocation.into_inner().unwrap();
    open_with_config_and_exit(
        app.upcast_ref(),
        Some(&window),
        invocation.request,
        Vim9UiConfig::default(),
        None,
        Some(EmbeddedLaunch {
            container: pane.clone(),
            source: source.clone().upcast(),
            completion: invocation.completion,
        }),
    );
    let widget: &gtk::Widget = window.upcast_ref();
    let editor = descendants::<vte4::Terminal>(widget)
        .into_iter()
        .next()
        .unwrap();
    wait_for(widget, "embedded Vim bridge and focus", || {
        named_button(widget, "Save (Ctrl+S)").is_sensitive() && editor.has_focus()
    });
    let ctrl_shift = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
    assert!(embedded_owns_quick_open_key(
        &window,
        gdk::Key::P,
        ctrl_shift
    ));
    assert!(!embedded_owns_quick_open_key(
        &window,
        gdk::Key::p,
        gdk::ModifierType::CONTROL_MASK
    ));
    assert!(!embedded_owns_quick_open_key(
        &window,
        gdk::Key::P,
        ctrl_shift | gdk::ModifierType::ALT_MASK
    ));
    let found = std::process::Command::new("xdotool")
        .args([
            "search",
            "--all",
            "--onlyvisible",
            "--pid",
            &std::process::id().to_string(),
            "--name",
            "^Embedded Vim9 palette test$",
        ])
        .output()
        .unwrap();
    assert!(found.status.success());
    let ids = String::from_utf8(found.stdout).unwrap();
    let id = ids.lines().next().unwrap();
    assert!(std::process::Command::new("xdotool")
        .args(["windowfocus", "--sync", id])
        .status()
        .unwrap()
        .success());
    let send = |key: &str| {
        assert!(std::process::Command::new("xdotool")
            .args(["key", "--clearmodifiers", key])
            .status()
            .unwrap()
            .success());
    };
    if font_only {
        let buffer = descendants::<native_buffer::BufferSurface>(widget)
            .into_iter()
            .next()
            .unwrap();
        wait_for(widget, "embedded native buffer geometry", || {
            buffer.painted_geometry().is_some()
        });
        let old_line_height = buffer.painted_geometry().unwrap().0.line_height;
        let old_size = editor.font().unwrap().size();
        assert!(embedded_owns_font_key(
            &window,
            gdk::Key::equal,
            gdk::ModifierType::CONTROL_MASK
        ));
        send("ctrl+equal");
        wait_for(
            widget,
            "parent capture defers to embedded buffer font",
            || {
                editor.font().unwrap().size() == old_size + gtk::pango::SCALE
                    && buffer.native_font_size() == old_size + gtk::pango::SCALE
                    && buffer
                        .painted_geometry()
                        .is_some_and(|(metrics, _)| metrics.line_height > old_line_height)
            },
        );
        assert_eq!(terminal_font_count.get(), 0);
        send("ctrl+0");
        wait_for(widget, "embedded buffer font reset", || {
            editor.font().unwrap().size() == 12 * gtk::pango::SCALE
                && buffer.native_font_size() == 12 * gtk::pango::SCALE
        });
        wait_for(widget, "font reset restores buffer grid", || {
            buffer
                .painted_geometry()
                .is_some_and(|(metrics, _)| metrics.line_height == old_line_height)
        });
        let move_pointer = |target: &gtk::Widget| {
            let bounds = target.compute_bounds(widget).unwrap();
            let x = (bounds.x() + bounds.width() * 0.5).round().to_string();
            let y = (bounds.y() + bounds.height() * 0.5).round().to_string();
            assert!(std::process::Command::new("xdotool")
                .args(["mousemove", "--window", id, &x, &y])
                .status()
                .unwrap()
                .success());
        };
        let wheel_up = || {
            for args in [
                vec!["keydown", "Control_L"],
                vec!["click", "4"],
                vec!["keyup", "Control_L"],
            ] {
                assert!(std::process::Command::new("xdotool")
                    .args(args)
                    .status()
                    .unwrap()
                    .success());
            }
        };
        move_pointer(editor.upcast_ref());
        wait_for(widget, "pointer enters embedded Vim buffer", || {
            embedded_owns_font_scroll(layout.upcast_ref())
        });
        wheel_up();
        wait_for(
            widget,
            "parent capture defers real Ctrl-wheel to buffer font",
            || {
                editor.font().unwrap().size() == 13 * gtk::pango::SCALE
                    && buffer.native_font_size() == 13 * gtk::pango::SCALE
                    && buffer
                        .painted_geometry()
                        .is_some_and(|(metrics, _)| metrics.line_height > old_line_height)
            },
        );
        assert_eq!(parent_wheel_count.get(), 0);
        send("ctrl+0");
        wait_for(widget, "wheel font reset restores buffer grid", || {
            editor.font().unwrap().size() == 12 * gtk::pango::SCALE
                && buffer
                    .painted_geometry()
                    .is_some_and(|(metrics, _)| metrics.line_height == old_line_height)
        });
        move_pointer(sibling.upcast_ref());
        wait_for(widget, "pointer leaves embedded Vim buffer", || {
            !embedded_owns_font_scroll(layout.upcast_ref())
        });
        wheel_up();
        wait_for(widget, "sibling wheel remains owned by parent", || {
            parent_wheel_count.get() == 1
        });
        assert_eq!(buffer.native_font_size(), 12 * gtk::pango::SCALE);
        sibling.grab_focus();
        assert!(!embedded_owns_font_key(
            &window,
            gdk::Key::equal,
            gdk::ModifierType::CONTROL_MASK
        ));
        send("ctrl+equal");
        wait_for(widget, "sibling keeps its normal font shortcut", || {
            terminal_font_count.get() == 1
        });
        assert_eq!(buffer.native_font_size(), 12 * gtk::pango::SCALE);
        eprintln!(
            "font capture verified: keyboard and real Ctrl-wheel changed VTE/Pango/grid, sibling font routes preserved"
        );
        editor.grab_focus();
        editor.feed_child(b":qa!\r");
        wait_for(widget, "font-test embedded exit restores shell", || {
            gtk::prelude::WidgetExt::is_visible(&source) && !has_embedded_editor(&pane)
        });
        window.close();
        std::fs::remove_dir_all(root).unwrap();
        return;
    }
    send("ctrl+shift+p");
    wait_for(widget, "parent capture defers to embedded picker", || {
        window.visible_dialog().is_some()
    });
    assert_eq!(terminal_palette_count.get(), 0);
    let picker = window.visible_dialog().unwrap();
    let query = descendants::<gtk::SearchEntry>(picker.upcast_ref())
        .into_iter()
        .next()
        .unwrap();
    query.set_text(">hover");
    send("ctrl+shift+p");
    let settled = Rc::new(Cell::new(false));
    let observed = settled.clone();
    glib::timeout_add_local_once(Duration::from_millis(120), move || observed.set(true));
    wait_for(widget, "native dialog keeps parent palette key", || {
        settled.get()
    });
    assert_eq!(terminal_palette_count.get(), 0);
    assert!(window.visible_dialog().as_ref() == Some(&picker));
    assert_eq!(query.text(), ">hover");
    query.emit_stop_search();
    wait_for(widget, "embedded dialog returns focus", || {
        window.visible_dialog().is_none() && editor.has_focus()
    });
    sibling.grab_focus();
    assert!(!embedded_owns_quick_open_key(
        &window,
        gdk::Key::P,
        ctrl_shift
    ));
    send("ctrl+shift+p");
    wait_for(
        widget,
        "sibling terminal retains its palette shortcut",
        || terminal_palette_count.get() == 1,
    );
    assert!(window.visible_dialog().is_none());
    let sibling_dialog = adw::Dialog::new();
    let sibling_query = gtk::Entry::new();
    let native_modal_count = Rc::new(Cell::new(0));
    let native_modal_font_count = Rc::new(Cell::new(0));
    let observed_modal_font_count = native_modal_font_count.clone();
    let observed_modal_count = native_modal_count.clone();
    let native_modal_keys = gtk::EventControllerKey::new();
    native_modal_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    native_modal_keys.connect_key_pressed(move |_, key, _, modifiers| {
        if native_buffer::font_zoom_key(key, modifiers).is_some() {
            observed_modal_font_count.set(observed_modal_font_count.get() + 1);
            return glib::Propagation::Stop;
        }
        if key.to_unicode().map(|key| key.to_ascii_lowercase()) == Some('p')
            && modifiers.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK)
        {
            observed_modal_count.set(observed_modal_count.get() + 1);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    sibling_query.add_controller(native_modal_keys);
    sibling_dialog.set_child(Some(&sibling_query));
    sibling_dialog.present(Some(&window));
    wait_for(widget, "sibling native modal is mapped", || {
        window.visible_dialog().as_ref() == Some(&sibling_dialog) && sibling_query.is_mapped()
    });
    assert!(sibling_query.grab_focus());
    wait_for(widget, "sibling modal field actually owns focus", || {
        gtk::prelude::RootExt::focus(&window).is_some_and(|focus| {
            &focus == sibling_query.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&sibling_query)
        })
    });
    assert!(native_dialog_owns_keyboard(&window));
    assert!(!embedded_owns_quick_open_key(
        &window,
        gdk::Key::P,
        ctrl_shift
    ));
    send("ctrl+shift+p");
    wait_for(
        widget,
        "sibling native modal retains its own key handler",
        || native_modal_count.get() == 1,
    );
    assert_eq!(terminal_palette_count.get(), 1);
    send("ctrl+equal");
    wait_for(widget, "sibling native modal retains its font key", || {
        native_modal_font_count.get() == 1
    });
    assert_eq!(terminal_font_count.get(), 0);
    eprintln!(
        "native modal ownership: parent_palette={}, modal_palette={}, parent_font={}, modal_font={}, field_focus={}",
        terminal_palette_count.get(),
        native_modal_count.get(),
        terminal_font_count.get(),
        native_modal_font_count.get(),
        gtk::prelude::RootExt::focus(&window)
            .is_some_and(|focus| &focus == sibling_query.upcast_ref::<gtk::Widget>()
                || focus.is_ancestor(&sibling_query))
    );
    navigation::close_dialog(&sibling_dialog);
    wait_for(widget, "sibling modal dismissal", || {
        window.visible_dialog().is_none()
    });
    editor.grab_focus();
    send("ctrl+shift+p");
    wait_for(
        widget,
        "embedded picker works again after sibling focus",
        || window.visible_dialog().is_some(),
    );
    let picker = window.visible_dialog().unwrap();
    descendants::<gtk::SearchEntry>(picker.upcast_ref())
        .into_iter()
        .next()
        .unwrap()
        .emit_stop_search();
    wait_for(widget, "embedded picker final dismissal", || {
        window.visible_dialog().is_none() && editor.has_focus()
    });
    editor.feed_child(b":qa!\r");
    wait_for(widget, "embedded exit restores its waiting shell", || {
        gtk::prelude::WidgetExt::is_visible(&source)
            && gtk::prelude::RootExt::focus(&window).is_some_and(|focus| {
                &focus == source.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&source)
            })
            && !has_embedded_editor(&pane)
    });
    assert!(!embedded_owns_quick_open_key(
        &window,
        gdk::Key::P,
        ctrl_shift
    ));
    window.close();
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
            .as_chunks::<4>()
            .0
            .iter()
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
    // Real insert-mode completion remains native, preserves Vim's input
    // ownership, and paints GUI menu colors above the retained buffer scene.
    let original_buffer = buffer.painted_frame().unwrap().buffer;
    editor.feed_child(b":enew | set completeopt=menuone,noinsert,noselect pumheight=8 | call setline(1, ['', 'PUM_underlay sentinel']) | normal! gg0\r");
    editor.feed_child(b":hi Pmenu guifg=#9fe1f3 guibg=#25374b ctermfg=1 ctermbg=2 gui=NONE | hi PmenuSel guifg=#102030 guibg=#c479a1 ctermfg=2 ctermbg=1 gui=NONE | hi! link PmenuMatch Pmenu | hi! link PmenuMatchSel PmenuSel\r");
    editor.grab_focus();
    editor.feed_child("i\x12=[complete(1, ['现代alpha', '现代beta']), ''][1]\r".as_bytes());
    wait_for(widget, "native Unicode completion menu", || {
        buffer.painted_frame().is_some_and(|frame| {
            frame.completion.as_ref().is_some_and(|menu| {
                menu.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .contains("现代alpha")
                    && menu
                        .runs
                        .iter()
                        .any(|run| run.cells == 2 && run.text == "现")
            })
        }) && cursor_pixels(0x25374b).is_some()
            && cursor_pixels(0x9fe1f3).is_some()
    });
    assert!(
        editor.has_focus(),
        "completion must retain the editor's input focus"
    );
    let menu_frame = buffer.painted_frame().unwrap();
    let menu = menu_frame.completion.as_ref().unwrap();
    let menu_x = (menu.view.column + 1) as f64 * editor.char_width() as f64;
    let menu_y = (menu.view.row + 1) as f64 * editor.char_height() as f64;
    assert!(
        !buffer.enqueue_scroll(menu_x, menu_y, 1.0, gdk::ScrollUnit::Wheel),
        "completion wheel input must pass through to Vim, never become a semantic buffer scroll"
    );
    editor.feed_child(b"\x0e");
    let selected_row = |frame: &jterm_core::vim9::Vim9RenderFrame| {
        frame
            .completion
            .as_ref()?
            .runs
            .iter()
            .filter(|run| run.style.background == Some(0xc479a1))
            .map(|run| run.row)
            .min()
    };
    wait_for(widget, "Vim selects first native completion", || {
        buffer
            .painted_frame()
            .as_ref()
            .and_then(|frame| selected_row(frame))
            .is_some()
            && cursor_pixels(0xc479a1).is_some()
    });
    let first_selected = selected_row(&buffer.painted_frame().unwrap()).unwrap();
    if let Some(path) = std::env::var_os("JTERM_VIM9_TEST_SCREENSHOT") {
        gtk::prelude::WidgetExt::display(&window).flush();
        assert!(std::process::Command::new("import")
            .args(["-window", "root"])
            .arg(PathBuf::from(path).with_extension("completion.png"))
            .status()
            .unwrap()
            .success());
    }
    editor.feed_child(b"\x0e");
    wait_for(widget, "Vim selects next native completion", || {
        buffer
            .painted_frame()
            .as_ref()
            .and_then(|frame| selected_row(frame))
            .is_some_and(|row| row != first_selected)
    });
    editor.feed_child(b"\x10");
    wait_for(widget, "Vim selects previous native completion", || {
        buffer
            .painted_frame()
            .as_ref()
            .and_then(|frame| selected_row(frame))
            == Some(first_selected)
    });
    editor.feed_child(b"\x19\x1b");
    wait_for(
        widget,
        "accepted completion restores the native body",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.mode == "n"
                    && frame.completion.is_none()
                    && frame
                        .runs
                        .iter()
                        .map(|run| run.text.as_str())
                        .collect::<String>()
                        .contains("现代alpha")
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text.contains("PUM_underlay sentinel"))
            }) && cursor_pixels(0x25374b).is_none()
                && cursor_pixels(0xc479a1).is_none()
        },
    );
    assert!(
        editor.has_focus(),
        "accepting completion must retain editor focus"
    );
    editor.feed_child(b":call setline(1, '') | normal! gg0\r");
    editor.feed_child("i\x12=[complete(1, ['现代alpha', '现代beta']), ''][1]\r".as_bytes());
    wait_for(widget, "completion reopens", || {
        buffer
            .painted_frame()
            .is_some_and(|frame| frame.completion.is_some())
    });
    editor.feed_child(b"\x0e\x05\x1b");
    wait_for(
        widget,
        "cancelled completion restores the original line",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.mode == "n"
                    && frame.completion.is_none()
                    && !frame
                        .runs
                        .iter()
                        .any(|run| run.text.contains("alpha") || run.text.contains("beta"))
                    && frame
                        .runs
                        .iter()
                        .any(|run| run.text.contains("PUM_underlay sentinel"))
            })
        },
    );
    editor.feed_child(b":bwipeout!\r");
    wait_for(
        widget,
        "completion test returns to the original scroll buffer",
        || {
            buffer
                .painted_frame()
                .is_some_and(|frame| frame.buffer == original_buffer && frame.mode == "n")
        },
    );
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
    let settings = buffer.settings();
    let saved_animations = settings.is_gtk_enable_animations();
    let smooth = descendants::<gtk::CheckButton>(widget)
        .into_iter()
        .find(|button| button.label().as_deref() == Some("Smooth scrolling"))
        .unwrap();
    let saved_smooth = smooth.is_active();
    // Each setting combination must retain exact direct manipulation. Wait
    // for the frontend frame callback so this also exercises its admission
    // gates rather than merely a controller left over from the previous tick.
    for (animate, interpolate_wheel) in [(true, true), (false, true), (true, false)] {
        settings.set_gtk_enable_animations(animate);
        smooth.set_active(interpolate_wheel);
        let ticked = Rc::new(Cell::new(false));
        let observed = ticked.clone();
        buffer.add_tick_callback(move |_, _| {
            observed.set(true);
            glib::ControlFlow::Break
        });
        wait_for(widget, "precision scroll animation policy", || ticked.get());
        for (delta, expected) in [(1.0, -1.0), (0.25, -1.25), (-1.25, 0.0)] {
            assert!(buffer.enqueue_scroll(x, y, delta, gdk::ScrollUnit::Surface));
            assert!(
                (buffer.scroll_offset(inactive_id) - expected).abs() < 0.001,
                "precision input must retain the exact sub-line pixel distance"
            );
            assert_eq!(buffer.scroll_offset(active), 0.0);
            assert!(buffer
                .painted_frame()
                .unwrap()
                .windows
                .iter()
                .all(|view| view.topline == 1));
        }
    }
    settings.set_gtk_enable_animations(saved_animations);
    smooth.set_active(saved_smooth);
    // A mouse mapping/virtualedit governs pointer gestures, not scrolling.
    // Insert-mode scrolling must commit its source viewport without leaving
    // Insert mode or moving focus into the hovered inactive split.
    editor.feed_child(b":set virtualedit=all\r");
    editor.feed_child(b"i");
    wait_for(
        widget,
        "Insert scroll without native mouse ownership",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.mode == "i"
                    && frame
                        .windows
                        .iter()
                        .all(|view| view.native_scroll && !view.native_pointer)
            })
        },
    );
    assert!(buffer.enqueue_scroll(x, y, line_height * 1.25, gdk::ScrollUnit::Surface));
    wait_for(
        widget,
        "Insert-mode precision viewport acknowledgement",
        || {
            buffer.painted_frame().is_some_and(|frame| {
                frame.mode == "i"
                    && frame.window == active
                    && frame
                        .windows
                        .iter()
                        .any(|view| view.window == inactive_id && view.topline == 2)
            })
        },
    );
    assert!((buffer.scroll_offset(inactive_id) + line_height * 0.25).abs() < 0.001);
    assert_eq!(buffer.scroll_offset(active), 0.0);
    assert!(buffer.enqueue_scroll(x, y, -line_height * 1.25, gdk::ScrollUnit::Surface));
    wait_for(widget, "Insert-mode precision reversal", || {
        buffer.painted_frame().is_some_and(|frame| {
            frame.mode == "i" && frame.windows.iter().all(|view| view.topline == 1)
        })
    });
    assert!(buffer.scroll_offset(inactive_id).abs() < 0.001);
    editor.feed_child(b"\x1b:set virtualedit=\r");
    wait_for(widget, "Normal-mode scroll fixture restored", || {
        buffer
            .painted_frame()
            .is_some_and(|frame| frame.mode == "n")
    });
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

#[test]
fn terminal_modes_keep_diagnostic_function_keys_with_vim() {
    for visible in [false, true] {
        for mode in ["t", "nt", "ntT"] {
            assert!(!diagnostic_shortcuts_enabled(visible, mode));
        }
    }
    for mode in ["n", "i", "v", "no"] {
        assert!(diagnostic_shortcuts_enabled(true, mode));
        assert!(!diagnostic_shortcuts_enabled(false, mode));
    }
}

#[test]
#[ignore = "requires GTK display; run under Xvfb"]
fn workspace_search_options_restore_without_running_search() {
    gtk::init().unwrap();
    let controls = project_search::SearchOptions::new();
    let options = jterm_core::vim9::Vim9SearchOptions {
        case: jterm_core::vim9::Vim9SearchCase::Sensitive,
        regex: true,
        whole_word: true,
        glob: "src/**/*.rs".into(),
    };
    controls.restore(&options);
    assert_eq!(controls.options(), options);
    controls.restore(&Default::default());
    assert_eq!(controls.options(), Default::default());
}

#[test]
#[ignore = "requires GTK display; run under Xvfb"]
fn workspace_output_focus_restores_the_visible_target_or_editor() {
    gtk::init().unwrap();
    let stack = gtk::Stack::new();
    let output = gtk::TextView::new();
    output.set_editable(false);
    output.set_cursor_visible(false);
    let problems = gtk::ListBox::new();
    problems.append(&gtk::Label::new(Some("A problem")));
    stack.add_named(&output, Some("output"));
    stack.add_named(&problems, Some("problems"));
    let editor = gtk::Entry::new();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    layout.append(&stack);
    layout.append(&editor);
    let window = gtk::Window::builder().child(&layout).build();
    window.present();
    let context = glib::MainContext::default();
    for _ in 0..30 {
        while context.pending() {
            context.iteration(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let owns_focus = |target: &gtk::Widget| {
        gtk::prelude::GtkWindowExt::focus(&window)
            .is_some_and(|focused| focused == *target || focused.is_ancestor(target))
    };
    stack.set_visible_child_name("output");
    restore_workspace_output_focus(&stack, &output, &problems, &editor);
    assert!(owns_focus(output.upcast_ref()));
    stack.set_visible_child_name("problems");
    restore_workspace_output_focus(&stack, &output, &problems, &editor);
    assert!(owns_focus(problems.upcast_ref()));
    stack.set_visible_child_name("output");
    output.set_sensitive(false);
    restore_workspace_output_focus(&stack, &output, &problems, &editor);
    assert!(owns_focus(editor.upcast_ref()));
    window.close();
}
