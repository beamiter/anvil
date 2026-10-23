//! Explicit, in-memory project task authoring. Creating a task never starts a provider.
use gtk::prelude::*;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use crate::agent_task::{AgentProvider, TaskBrief, TaskValidationRecipe};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskProjectDraft {
    pub title: String,
    pub directory: PathBuf,
    pub provider: AgentProvider,
    pub brief: TaskBrief,
}

impl TaskProjectDraft {
    pub fn validate(&self) -> Result<(), String> {
        let title = self.title.trim();
        if title.is_empty() || title.len() > 256 || title.chars().any(char::is_control) {
            return Err(
                "Enter a task title of at most 256 bytes without control characters".into(),
            );
        }
        if !self.directory.is_absolute() {
            return Err("Choose an absolute local repository directory".into());
        }
        self.brief.validate()
    }
}

/// One exact submission, acknowledged only after its worktree and task registration succeed.
/// The goal stays in memory; recovery journals never receive this request.
#[derive(Clone, Debug)]
pub(crate) struct TaskProjectRequest {
    pub draft: TaskProjectDraft,
    identity: uuid::Uuid,
    completion: Sender<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
    worktree_cancel: Arc<AtomicBool>,
}

impl PartialEq for TaskProjectRequest {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for TaskProjectRequest {}

impl TaskProjectRequest {
    pub fn finish(&self, result: Result<(), String>) {
        let _ = self.completion.send(result);
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn worktree_cancel_flag(&self) -> Arc<AtomicBool> {
        self.worktree_cancel.clone()
    }
}

struct PendingSubmission {
    completion: Receiver<Result<(), String>>,
    cancelled: Arc<AtomicBool>,
    worktree_cancel: Arc<AtomicBool>,
}

#[cfg(test)]
thread_local! {
    static TEST_COMPOSER: std::cell::RefCell<std::rc::Weak<Composer>> = const { std::cell::RefCell::new(std::rc::Weak::new()) };
}

struct Composer {
    window: gtk::Window,
    title: gtk::Entry,
    directory: gtk::Entry,
    provider: gtk::DropDown,
    goal: gtk::TextBuffer,
    goal_view: gtk::TextView,
    validation: gtk::Entry,
    shell: Option<String>,
    error: gtk::Label,
    create_button: gtk::Button,
    revision: Cell<u64>,
    activation_revision: Cell<Option<u64>>,
    pending: RefCell<Option<PendingSubmission>>,
    submit: Box<dyn Fn(TaskProjectRequest)>,
}

impl Composer {
    fn changed(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        // GTK keeps the first activation timer when another activate signal
        // arrives. Cancel it before a changed draft can acquire a new meaning.
        if let Some(parent) = self.create_button.parent().and_downcast::<gtk::Box>() {
            let previous = self.create_button.prev_sibling();
            parent.remove(&self.create_button);
            parent.insert_child_after(&self.create_button, previous.as_ref());
        }
    }

    fn draft(&self) -> Result<TaskProjectDraft, String> {
        let command = self.validation.text().to_string();
        let validation = if command.trim().is_empty() {
            None
        } else {
            let shell = self.shell.as_deref().ok_or_else(|| {
                "Configure an absolute validation shell before adding a validation command"
                    .to_string()
            })?;
            Some(TaskValidationRecipe::new(command, shell)?)
        };
        let brief = TaskBrief::new(
            self.goal
                .text(&self.goal.start_iter(), &self.goal.end_iter(), true)
                .to_string(),
            validation,
        )?;
        let draft = TaskProjectDraft {
            title: self.title.text().trim().to_string(),
            directory: PathBuf::from(self.directory.text().as_str()),
            provider: AgentProvider::ALL[self.provider.selected() as usize],
            brief,
        };
        draft.validate()?;
        Ok(draft)
    }

    fn create(&self) {
        let activation_revision = self.activation_revision.take();
        if self.pending.borrow().is_some() {
            return;
        }
        if activation_revision.is_some_and(|revision| revision != self.revision.get()) {
            self.error
                .set_text("The draft changed. Review it and choose Create task again.");
            return;
        }
        match self.draft() {
            Ok(draft) => {
                let (completion, receiver) = mpsc::channel();
                let cancelled = Arc::new(AtomicBool::new(false));
                let worktree_cancel = Arc::new(AtomicBool::new(false));
                let request = TaskProjectRequest {
                    draft,
                    identity: uuid::Uuid::new_v4(),
                    completion,
                    cancelled: cancelled.clone(),
                    worktree_cancel: worktree_cancel.clone(),
                };
                *self.pending.borrow_mut() = Some(PendingSubmission {
                    completion: receiver,
                    cancelled,
                    worktree_cancel,
                });
                self.set_pending(true);
                self.error.set_text("Preparing the isolated worktree…");
                (self.submit)(request);
            }
            Err(error) => self.error.set_text(&error),
        }
    }

    fn set_pending(&self, pending: bool) {
        self.title.set_editable(!pending);
        self.directory.set_editable(!pending);
        self.provider.set_sensitive(!pending);
        self.goal_view.set_editable(!pending);
        self.validation.set_editable(!pending);
        self.create_button.set_sensitive(!pending);
        self.create_button.set_label(if pending {
            "Creating…"
        } else {
            "Create task"
        });
    }

    fn poll(&self) {
        let result = self
            .pending
            .borrow()
            .as_ref()
            .map(|pending| pending.completion.try_recv());
        let result = match result {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Disconnected)) => {
                Err("Task creation stopped without a result. Your draft is preserved.".into())
            }
            _ => return,
        };
        self.pending.borrow_mut().take();
        self.set_pending(false);
        match result {
            Ok(()) => {
                crate::enter_ownership::claim_held();
                self.window.close();
            }
            Err(error) => self
                .error
                .set_text(&crate::review_text::visible_bounded(&error, 2048)),
        }
    }

    fn cancel(&self) {
        if let Some(pending) = self.pending.borrow().as_ref() {
            pending.cancelled.store(true, Ordering::Release);
            pending.worktree_cancel.store(true, Ordering::Release);
        }
    }
}

pub(crate) fn show(
    parent: &impl IsA<gtk::Window>,
    initial_directory: Option<PathBuf>,
    shell: Option<String>,
    submit: impl Fn(TaskProjectRequest) + 'static,
) {
    let window = gtk::Window::builder()
        .title("New project task")
        .default_width(650)
        .default_height(640)
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    for (edge, margin) in [(0, 20), (1, 20), (2, 20), (3, 20)] {
        match edge {
            0 => root.set_margin_top(margin),
            1 => root.set_margin_bottom(margin),
            2 => root.set_margin_start(margin),
            _ => root.set_margin_end(margin),
        }
    }
    let heading = gtk::Label::new(Some("New project task"));
    heading.set_xalign(0.0);
    heading.add_css_class("title-1");
    root.append(&heading);
    let introduction = gtk::Label::new(Some(
        "Describe work for an isolated Git worktree. Create the task, review its details, then choose how to start the Agent.",
    ));
    introduction.set_xalign(0.0);
    introduction.set_wrap(true);
    root.append(&introduction);

    let field = |caption: &str, child: &gtk::Widget| {
        let label = gtk::Label::new(Some(caption));
        label.set_xalign(0.0);
        label.add_css_class("heading");
        root.append(&label);
        root.append(child);
    };
    let title = gtk::Entry::new();
    title.set_max_length(256);
    title.set_placeholder_text(Some("A short description of the outcome"));
    title.update_property(&[gtk::accessible::Property::Label("Task title")]);
    field("Title", title.upcast_ref());
    let directory = gtk::Entry::new();
    directory.set_max_length(4096);
    directory.set_placeholder_text(Some("/absolute/path/to/repository"));
    directory.update_property(&[gtk::accessible::Property::Label("Repository directory")]);
    if let Some(path) = initial_directory.as_deref().and_then(|path| path.to_str()) {
        directory.set_text(path);
    }
    field("Repository", directory.upcast_ref());
    let provider =
        gtk::DropDown::from_strings(&AgentProvider::ALL.map(|provider| provider.display_name()));
    provider.update_property(&[gtk::accessible::Property::Label("Task provider")]);
    field("Provider", provider.upcast_ref());
    let goal_view = gtk::TextView::new();
    goal_view.set_wrap_mode(gtk::WrapMode::WordChar);
    goal_view.set_top_margin(8);
    goal_view.set_bottom_margin(8);
    goal_view.set_left_margin(8);
    goal_view.set_right_margin(8);
    goal_view.update_property(&[gtk::accessible::Property::Label("Task goal")]);
    let goal = goal_view.buffer();
    let goal_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(150)
        .vexpand(true)
        .child(&goal_view)
        .build();
    goal_scroll.add_css_class("card");
    field("Goal", goal_scroll.upcast_ref());
    let validation = gtk::Entry::new();
    validation.set_max_length(4096);
    validation.set_placeholder_text(Some("Optional, for example: cargo test --locked"));
    validation.update_property(&[gtk::accessible::Property::Label("Validation command")]);
    field("Validation command", validation.upcast_ref());
    let shell_text = shell
        .as_deref()
        .map(|shell| crate::review_text::visible_bounded(shell, 512));
    let validation_hint = gtk::Label::new(Some(&format!(
        "Runs only when you choose Validate, at the worktree root, using {}. Leave blank to accept completed work through explicit manual review.",
        shell_text.as_deref().unwrap_or("the configured shell (currently unavailable)")
    )));
    validation_hint.set_xalign(0.0);
    validation_hint.set_wrap(true);
    validation_hint.add_css_class("caption");
    validation_hint.add_css_class("dim-label");
    root.append(&validation_hint);
    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.set_wrap(true);
    error.add_css_class("error");
    root.append(&error);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let create = gtk::Button::with_label("Create task");
    create.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&create);
    root.append(&actions);
    window.set_child(Some(&root));

    let composer = Rc::new(Composer {
        window: window.clone(),
        title,
        directory,
        provider,
        goal: goal.clone(),
        goal_view,
        validation,
        shell,
        error: error.clone(),
        create_button: create.clone(),
        revision: Cell::new(0),
        activation_revision: Cell::new(None),
        pending: RefCell::new(None),
        submit: Box::new(submit),
    });
    #[cfg(test)]
    TEST_COMPOSER.with(|slot| *slot.borrow_mut() = Rc::downgrade(&composer));
    goal.connect_insert_text(move |buffer, _, text| {
        let current = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
        if current.len().saturating_add(text.len()) > 16 * 1024 {
            buffer.stop_signal_emission_by_name("insert-text");
            error.set_text("The goal is limited to 16 KiB; shorten the text before inserting more");
        }
    });
    for entry in [&composer.title, &composer.directory, &composer.validation] {
        let weak = Rc::downgrade(&composer);
        entry.connect_changed(move |_| {
            if let Some(composer) = weak.upgrade() {
                composer.changed();
            }
        });
    }
    let weak = Rc::downgrade(&composer);
    composer.goal.connect_changed(move |_| {
        if let Some(composer) = weak.upgrade() {
            composer.changed();
        }
    });
    let weak = Rc::downgrade(&composer);
    composer.provider.connect_selected_notify(move |_| {
        if let Some(composer) = weak.upgrade() {
            composer.changed();
        }
    });
    let press = gtk::GestureClick::new();
    press.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&composer);
    press.connect_pressed(move |_, _, _, _| {
        if let Some(composer) = weak.upgrade() {
            composer
                .activation_revision
                .set(Some(composer.revision.get()));
        }
    });
    create.add_controller(press);
    // GtkButton activation emits clicked after a short keyboard animation.
    // Capture its meaning now, before edits can change the eventual submission.
    let weak = Rc::downgrade(&composer);
    create.connect_local("activate", false, move |_| {
        if let Some(composer) = weak.upgrade() {
            composer
                .activation_revision
                .set(Some(composer.revision.get()));
        }
        None
    });
    let weak = Rc::downgrade(&composer);
    create.connect_clicked(move |_| {
        if let Some(composer) = weak.upgrade() {
            composer.create();
        }
    });
    let weak = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            crate::enter_ownership::claim_held();
            window.close();
        }
    });
    let keys = gtk::EventControllerKey::new();
    let weak = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            if let Some(window) = weak.upgrade() {
                window.close();
            }
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
    let weak = Rc::downgrade(&composer);
    window.connect_close_request(move |_| {
        if let Some(composer) = weak.upgrade() {
            composer.cancel();
        }
        gtk::glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&composer);
    gtk::glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
        let Some(composer) = weak.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        composer.poll();
        if composer.window.is_visible() {
            gtk::glib::ControlFlow::Continue
        } else {
            gtk::glib::ControlFlow::Break
        }
    });
    crate::enter_ownership::watch_window(&window);
    // Own the form only for this window lifetime; it is never serialized into recovery journals.
    let form_owner = std::cell::RefCell::new(Some(composer.clone()));
    window.connect_destroy(move |_| {
        if let Some(composer) = form_owner.borrow_mut().take() {
            composer.cancel();
        }
    });
    window.present();
    composer.title.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_draft_keeps_real_goal_separate_from_terminal_evidence() {
        let draft = TaskProjectDraft {
            title: "Add task inspector".into(),
            directory: PathBuf::from("/tmp/project"),
            provider: AgentProvider::Codex,
            brief: TaskBrief::new("Show the real worktree and validation result", None).unwrap(),
        };
        assert!(draft.validate().is_ok());
        let mut invalid = draft.clone();
        invalid.directory = PathBuf::from("relative");
        assert!(invalid.validate().is_err());
        invalid = draft;
        invalid.title = "bad\ntitle".into();
        assert!(invalid.validate().is_err());
    }
    #[test]
    #[ignore = "requires a native GTK display; run under Xvfb"]
    fn agent_management_v3_composer_creates_only_an_explicit_draft() {
        gtk::init().expect("GTK display");
        let parent = gtk::Window::new();
        let requests = Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = requests.clone();
        show(
            &parent,
            Some(PathBuf::from("/tmp/project")),
            Some("/bin/bash".into()),
            move |draft| {
                sink.borrow_mut().push(draft);
            },
        );
        let form = TEST_COMPOSER.with(|slot| slot.borrow().upgrade()).unwrap();
        form.title.set_text("Build a task inspector");
        form.create();
        assert!(!form.error.text().is_empty());
        assert!(requests.borrow().is_empty());
        form.goal
            .set_text("Show the real project and worktree.\nPreserve explicit approval gates.");
        form.provider.set_selected(3);
        form.validation.set_text("cargo test --locked");
        form.title.emit_activate();
        form.validation.emit_activate();
        assert!(
            requests.borrow().is_empty(),
            "editing must not create or launch a task"
        );
        let valid_goal = form
            .goal
            .text(&form.goal.start_iter(), &form.goal.end_iter(), true);
        form.goal.insert_at_cursor(&"x".repeat(16 * 1024));
        assert_eq!(
            form.goal
                .text(&form.goal.start_iter(), &form.goal.end_iter(), true),
            valid_goal
        );
        if let Some(path) = std::env::var_os("JTERM_TASK_COMPOSER_SCREENSHOT") {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
            assert!(std::process::Command::new("import")
                .args(["-window", "root"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }
        let controllers = form.create_button.observe_controllers();
        let gesture = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index).and_downcast::<gtk::GestureClick>())
            .find(|gesture| gesture.propagation_phase() == gtk::PropagationPhase::Capture)
            .unwrap();
        gesture.emit_by_name::<()>("pressed", &[&1_i32, &8.0_f64, &8.0_f64]);
        form.title.set_text("Build a reviewed task inspector");
        form.create_button.emit_clicked();
        assert!(
            requests.borrow().is_empty(),
            "a held pointer cannot submit a changed draft"
        );
        let drain_for = |milliseconds| {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(milliseconds);
            while std::time::Instant::now() < deadline {
                while gtk::glib::MainContext::default().pending() {
                    gtk::glib::MainContext::default().iteration(false);
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        form.create_button.emit_by_name::<()>("activate", &[]);
        drain_for(180);
        form.validation.set_text("cargo test --locked --lib");
        form.create_button.emit_by_name::<()>("activate", &[]);
        drain_for(100);
        assert!(
            requests.borrow().is_empty(),
            "a repeated activation cannot reuse the first draft's keyboard timer"
        );
        form.title
            .set_text("Build the final reviewed task inspector");
        drain_for(280);
        assert!(
            requests.borrow().is_empty(),
            "a delayed keyboard activation cannot submit a changed draft"
        );
        form.create_button.emit_clicked();
        assert!(
            requests.borrow().is_empty(),
            "an already queued old click is rejected after a draft change"
        );
        let exact_draft = form.draft().unwrap();
        form.create();
        form.create();
        assert_eq!(requests.borrow().len(), 1);
        let first = requests.borrow()[0].clone();
        assert_eq!(first.draft.provider, AgentProvider::Kimi);
        assert_eq!(first.draft.directory, PathBuf::from("/tmp/project"));
        assert!(first.draft.brief.goal.contains('\n'));
        let recipe = first.draft.brief.validation.as_ref().unwrap();
        assert_eq!(recipe.command, "cargo test --locked --lib");
        assert_eq!(recipe.shell, "/bin/bash");
        assert!(
            form.window.is_visible(),
            "async preparation keeps the form alive"
        );
        assert!(!form.title.is_editable());
        assert!(!form.create_button.is_sensitive());
        first.finish(Err(
            "The repository could not be prepared; retry after correcting its path".into(),
        ));
        form.poll();
        assert!(form.window.is_visible());
        assert!(form.title.is_editable());
        assert_eq!(
            form.draft().unwrap(),
            exact_draft,
            "a background failure preserves every authored field"
        );
        assert!(form.error.text().contains("could not be prepared"));
        form.create();
        assert_eq!(requests.borrow().len(), 2);
        let retry = requests.borrow()[1].clone();
        assert_ne!(first, retry, "a retry has a fresh completion identity");
        assert_eq!(retry.draft, exact_draft);
        first.finish(Ok(()));
        form.poll();
        assert!(
            form.window.is_visible(),
            "an old completion cannot close the retry form"
        );
        retry.finish(Ok(()));
        form.poll();
        assert!(!form.window.is_visible());
        assert!(
            !retry.cancelled(),
            "successful close must not cancel its registered task"
        );

        let sink = requests.clone();
        show(
            &parent,
            Some(PathBuf::from("/tmp/project")),
            Some("/bin/bash".into()),
            move |request| {
                sink.borrow_mut().push(request);
            },
        );
        let cancelled_form = TEST_COMPOSER.with(|slot| slot.borrow().upgrade()).unwrap();
        cancelled_form.title.set_text("Cancelled work");
        cancelled_form.goal.set_text("Do not register after Cancel");
        cancelled_form.create();
        let cancelled_request = requests.borrow().last().unwrap().clone();
        cancelled_form.window.close();
        assert!(cancelled_request.cancelled());
        assert!(cancelled_request
            .worktree_cancel_flag()
            .load(Ordering::Acquire));
        parent.close();
    }
}
