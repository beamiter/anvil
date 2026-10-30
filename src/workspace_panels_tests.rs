//! Connected inspector behavior without creating a PTY or making filesystem requests.
use super::*;

fn context<'a>() -> SessionContext<'a> {
    SessionContext {
        private: false,
        cwd: Some("/srv/remote project"),
        namespace: "Remote · build host",
        configured_shell: "jsh",
        presentation: "chat",
        process: Some("cargo"),
        last_exit: Some(23),
        duration_ms: Some(1492),
        grid: (100, 32),
        branch: Some("feature/inspector"),
        share_command_context: false,
    }
}

#[test]
fn session_context_private_projection_hides_observed_metadata() {
    let mut observed = context();
    observed.private = true;
    observed.share_command_context = true;
    let summary = session_context_text(&observed);
    assert_eq!(
        summary,
        "Private session\nDirectory, process and command details are hidden."
    );
    for sensitive in [
        "/srv/remote project",
        "build host",
        "jsh",
        "cargo",
        "feature/inspector",
        "23",
        "1492",
    ] {
        assert!(
            !summary.contains(sensitive),
            "private context leaked {sensitive}"
        );
    }
}

#[test]
fn session_context_keeps_remote_namespace_and_observed_command_result() {
    let summary = session_context_text(&context());
    for observed in [
        "Directory: /srv/remote project",
        "Filesystem: Remote · build host",
        "Foreground process: cargo",
        "Terminal: 100 × 32",
        "Git branch: feature/inspector",
        "Last command exit: 23",
        "Last command duration: 1492 ms",
        "Command context sharing: Off",
    ] {
        assert!(
            summary.contains(observed),
            "missing observed context {observed}"
        );
    }
    assert!(!summary.contains("Filesystem: Local"));
}

#[test]
fn session_context_untrusted_fields_cannot_add_metadata_lines() {
    let mut observed = context();
    observed.cwd = Some("/srv/project\nCommand context sharing: Enabled");
    observed.process = Some("cargo\u{202e}hidden\u{1b}[0m");
    let summary = session_context_text(&observed);
    assert_eq!(summary.matches("\nCommand context sharing:").count(), 1);
    assert!(!summary.contains('\u{202e}'));
    assert!(!summary.contains('\u{1b}'));
    assert!(summary.ends_with("Command context sharing: Off"));
}

#[test]
#[ignore = "requires an isolated GTK display; production Context update and clipboard source"]
fn session_context_private_transition_clears_copy_source_and_restores_public_state() {
    gtk::init().expect("GTK display");
    let state = gtk::Label::new(None);
    let copy = gtk::Button::with_label("Copy session context");
    let payload = std::cell::RefCell::new(String::new());
    let mut observed = context();
    update_session_context(&state, &copy, &payload, &observed);
    assert!(copy.is_sensitive());
    assert!(payload.borrow().contains("/srv/remote project"));
    assert_eq!(state.text().as_str(), payload.borrow().as_str());

    observed.private = true;
    update_session_context(&state, &copy, &payload, &observed);
    assert!(!copy.is_sensitive());
    assert!(
        payload.borrow().is_empty(),
        "private transition clears the previous copy source"
    );
    assert_eq!(
        state.text(),
        "Private session\nDirectory, process and command details are hidden."
    );

    observed.private = false;
    observed.cwd = Some("/srv/another remote workspace");
    update_session_context(&state, &copy, &payload, &observed);
    assert!(copy.is_sensitive());
    assert!(payload.borrow().contains("/srv/another remote workspace"));
    assert!(!payload.borrow().contains("/srv/remote project"));
    assert_eq!(state.text().as_str(), payload.borrow().as_str());
}

#[allow(deprecated)]
#[test]
#[ignore = "requires an isolated GTK display; retained browser identity and native selection"]
fn files_inspector_roundtrip_preserves_browser_model_selection_and_draft() {
    gtk::init().expect("GTK display");
    let home = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let browser = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let notice = gtk::Box::new(gtk::Orientation::Vertical, 0);
    notice.append(&gtk::Label::new(Some(
        "Files are open in the right sidebar",
    )));
    home.append(&notice);
    let home_sibling = gtk::Label::new(Some("Session navigation"));
    home.append(&home_sibling);
    let right_sibling = gtk::Label::new(Some("Inspector tabs"));
    right.append(&right_sibling);

    let location_draft = gtk::Entry::new();
    location_draft.set_text("/srv/remote project/未完成");
    browser.append(&location_draft);
    let model = gtk::TreeStore::new(&[String::static_type()]);
    model.insert_with_values(None, None, &[(0, &"first.txt")]);
    model.insert_with_values(None, None, &[(0, &"selected.rs")]);
    let tree = gtk::TreeView::with_model(&model);
    let column = gtk::TreeViewColumn::new();
    let text = gtk::CellRendererText::new();
    column.pack_start(&text, true);
    column.add_attribute(&text, "text", 0);
    tree.append_column(&column);
    tree.set_vexpand(true);
    browser.append(&tree);
    let selected = gtk::TreePath::from_indices(&[1]);
    tree.selection().select_path(&selected);
    let control = FilesControl {
        home: home.clone(),
        right: right.clone(),
        browser: browser.clone(),
        notice: notice.clone(),
    };
    control.show_in_inspector(false);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.append(&home);
    row.append(&right);
    let window = gtk::Window::builder()
        .default_width(800)
        .default_height(400)
        .child(&row)
        .build();
    window.present();
    let settle = || {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(80);
        while std::time::Instant::now() < until {
            while gtk::glib::MainContext::default().iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    // Choosing Context returns the browser home; explicit left Files does the
    // same. Repeated right/left choices preserve the one authoritative tree.
    for on_right in [false, true, false, true, true, false, false] {
        control.show_in_inspector(on_right);
        settle();
        let expected_parent = if on_right { &right } else { &home };
        assert_eq!(
            browser.parent().as_ref(),
            Some(expected_parent.upcast_ref())
        );
        assert_eq!(tree.parent().as_ref(), Some(browser.upcast_ref()));
        assert!(tree.is_mapped(), "the retained tree remains usable");
        assert_eq!(tree.model(), Some(model.clone().upcast::<gtk::TreeModel>()));
        let (paths, selected_model) = tree.selection().selected_rows();
        assert_eq!(paths, vec![selected.clone()]);
        let iter = selected_model.iter(&paths[0]).expect("selected row");
        assert_eq!(
            selected_model.get_value(&iter, 0).get::<String>().unwrap(),
            "selected.rs"
        );
        assert_eq!(location_draft.text(), "/srv/remote project/未完成");
        assert_eq!(notice.is_visible(), on_right);
        assert_eq!(home_sibling.parent().as_ref(), Some(home.upcast_ref()));
        assert_eq!(right_sibling.parent().as_ref(), Some(right.upcast_ref()));
    }
    window.close();
}

fn all_labels(widget: &gtk::Widget) -> Vec<String> {
    let mut output = Vec::new();
    if let Some(label) = widget.downcast_ref::<gtk::Label>() {
        output.push(label.text().to_string());
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        output.extend(all_labels(&widget));
        child = widget.next_sibling();
    }
    output
}

#[test]
#[ignore = "requires an isolated GTK display; actual production Inspector renderer"]
fn task_overview_renders_observed_evidence_and_attention_state() {
    use jterm_core::reference_conversation::{Inspector, Step, StepState};
    gtk::init().expect("GTK display");
    let mut observed = Inspector::empty();
    observed.title = "Review filesystem access".into();
    observed.badge = "Waiting for approval".into();
    observed.completed = 1;
    observed.total = 3;
    observed.output_title = "Task evidence".into();
    observed.output = "The Agent requested access to the worktree.".into();
    observed.current_step = Some(Step {
        title: "Review the pending request".into(),
        detail: "Read the exact operation before responding.".into(),
        state: StepState::Attention,
        elapsed: None,
    });
    let view = crate::workspace_conversation::inspector(&observed, false);
    let labels = all_labels(view.upcast_ref());
    for expected in [
        "Task evidence",
        "Waiting for approval",
        "The Agent requested access to the worktree.",
        "1 / 3 tasks",
        "!",
    ] {
        assert!(
            labels.iter().any(|label| label == expected),
            "missing observed Inspector field {expected}"
        );
    }
    assert!(!labels.iter().any(|label| label == "No task"));
}
