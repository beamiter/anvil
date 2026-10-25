//! Checked session identities are independent from the currently inspected row.
use super::{gtk, Manager, SessionIdentity, SessionRow};
use gtk::prelude::*;
use jterm_core::agent_sessions::{
    validate_session_batch_tag, SessionBatchAction, SESSION_BATCH_LIMIT,
};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::{Rc, Weak};

#[derive(Clone, PartialEq, Eq)]
struct ControlsIdentity {
    keys: Vec<SessionIdentity>,
    tag: String,
    enabled: bool,
}

#[derive(Clone)]
struct BatchRequest {
    generation: u64,
    keys: Vec<SessionIdentity>,
    action: SessionBatchAction,
}

#[derive(Clone, PartialEq, Eq)]
struct SelectionIdentity {
    visible: Vec<SessionIdentity>,
    checked: Vec<SessionIdentity>,
}

#[derive(Clone)]
struct SelectionRequest {
    generation: u64,
    identity: SelectionIdentity,
}

/// Default help for the batch tag entry; replaced by a validation message.
const BATCH_TAG_TOOLTIP: &str =
    "One tag (no commas) to add to or remove from every checked session";

pub(super) struct SessionBulk {
    pub owner: RefCell<Weak<Manager>>,
    checked: RefCell<HashSet<SessionIdentity>>,
    synchronizing: Cell<bool>,
    generation: Cell<u64>,
    selection_generation: Cell<u64>,
    selection_identity: RefCell<Option<SelectionIdentity>>,
    rendered: RefCell<Option<ControlsIdentity>>,
    pending: Cell<Option<usize>>,
    pub root: gtk::Box,
    select_visible: gtk::Button,
    clear: gtk::Button,
    summary: gtk::Label,
    pub tag: gtk::Entry,
    actions: gtk::Box,
    notice: gtk::Label,
}

impl SessionBulk {
    /// A batch write from this window is waiting for its completion notice.
    pub fn is_settling(&self) -> bool {
        self.pending.get().is_some()
    }

    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let selection = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let select_visible = gtk::Button::with_label("Select visible");
        select_visible
            .set_tooltip_text(Some("Add the sessions on this page to the batch selection"));
        let clear = gtk::Button::with_label("Clear selection");
        let summary = gtk::Label::new(Some("0 selected"));
        summary.set_xalign(0.0);
        summary.set_hexpand(true);
        selection.append(&select_visible);
        selection.append(&clear);
        selection.append(&summary);
        root.append(&selection);
        let action_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let tag = gtk::Entry::new();
        tag.set_max_length(128);
        tag.set_width_chars(12);
        tag.set_placeholder_text(Some("One tag"));
        tag.set_tooltip_text(Some(BATCH_TAG_TOOLTIP));
        tag.update_property(&[gtk::accessible::Property::Label("Batch session tag")]);
        action_row.append(&actions);
        action_row.append(&tag);
        root.append(&action_row);
        let notice = gtk::Label::new(Some(
            "Check sessions to organize them together. Batch changes never start or stop an Agent.",
        ));
        notice.set_xalign(0.0);
        notice.set_wrap(true);
        notice.add_css_class("caption");
        notice.add_css_class("dim-label");
        root.append(&notice);
        Self {
            owner: RefCell::new(Weak::new()),
            checked: RefCell::new(HashSet::new()),
            synchronizing: Cell::new(false),
            generation: Cell::new(0),
            selection_generation: Cell::new(0),
            selection_identity: RefCell::new(None),
            rendered: RefCell::new(None),
            pending: Cell::new(None),
            root,
            select_visible,
            clear,
            summary,
            tag,
            actions,
            notice,
        }
    }

    fn keys(&self) -> Vec<SessionIdentity> {
        let mut keys = self.checked.borrow().iter().cloned().collect::<Vec<_>>();
        keys.sort_by(|left, right| {
            left.0
                .label()
                .cmp(right.0.label())
                .then_with(|| left.1.cmp(&right.1))
        });
        keys
    }

    pub fn connect(&self, manager: &Rc<Manager>) {
        *self.owner.borrow_mut() = Rc::downgrade(manager);
        for (button, select_visible) in [(&self.select_visible, true), (&self.clear, false)] {
            let pending = Rc::new(RefCell::new(None));
            let press = gtk::GestureClick::new();
            press.set_button(1);
            press.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = Rc::downgrade(manager);
            let captured = pending.clone();
            press.connect_pressed(move |_, _, _, _| {
                if let Some(manager) = weak.upgrade() {
                    *captured.borrow_mut() = Some(manager.batch_selection_request());
                }
            });
            button.add_controller(press);
            let weak = Rc::downgrade(manager);
            let captured = pending.clone();
            button.connect_local("activate", false, move |_| {
                if let Some(manager) = weak.upgrade() {
                    *captured.borrow_mut() = Some(manager.batch_selection_request());
                }
                None
            });
            let weak = Rc::downgrade(manager);
            button.connect_clicked(move |_| {
                if let Some(manager) = weak.upgrade() {
                    let request = pending
                        .borrow_mut()
                        .take()
                        .unwrap_or_else(|| manager.batch_selection_request());
                    manager.apply_batch_selection(request, select_visible);
                }
            });
        }
        let weak = Rc::downgrade(manager);
        self.tag.connect_changed(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.update_batch_controls();
            }
        });
        for entry in [&manager.alias, &manager.tags, &self.tag] {
            prevent_enter_activation(entry);
        }
    }
}

fn cancel_selection_activation(button: &gtk::Button) {
    // Unparenting cancels GTK's pending pointer and delayed keyboard activation.
    // Keep the same position and use a separate epoch for already queued callbacks.
    button.set_sensitive(false);
    if let Some(parent) = button.parent().and_downcast::<gtk::Box>() {
        let previous = button.prev_sibling();
        parent.remove(button);
        parent.insert_child_after(button, previous.as_ref());
    }
}

fn prevent_enter_activation(widget: &impl IsA<gtk::Widget>) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(|_, key, _, _| {
        if matches!(
            key,
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter
        ) {
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });
    widget.add_controller(keys);
}

impl Manager {
    fn batch_selection_identity(&self) -> SelectionIdentity {
        SelectionIdentity {
            visible: self
                .visible
                .borrow()
                .iter()
                .map(|session| (session.provider, session.id.clone()))
                .collect(),
            checked: self.bulk.keys(),
        }
    }

    fn batch_selection_request(&self) -> SelectionRequest {
        SelectionRequest {
            generation: self.bulk.selection_generation.get(),
            identity: self.batch_selection_identity(),
        }
    }

    fn apply_batch_selection(&self, request: SelectionRequest, select_visible: bool) {
        if request.generation != self.bulk.selection_generation.get()
            || request.identity != self.batch_selection_identity()
        {
            return;
        }
        if select_visible {
            self.select_batch_keys(request.identity.visible);
        } else {
            self.bulk.checked.borrow_mut().clear();
            self.sync_batch_checks();
            self.update_batch_controls();
        }
    }

    pub(super) fn wire_batch_check(&self, row: &SessionRow, key: SessionIdentity) {
        let weak = self.bulk.owner.borrow().clone();
        row.checked.connect_toggled(move |checkbox| {
            let Some(manager) = weak.upgrade() else {
                return;
            };
            if manager.bulk.synchronizing.get() {
                return;
            }
            let mut checked = manager.bulk.checked.borrow_mut();
            if checkbox.is_active() {
                if checked.len() == SESSION_BATCH_LIMIT && !checked.contains(&key) {
                    drop(checked);
                    manager.bulk.notice.set_text(&format!(
                        "Select at most {SESSION_BATCH_LIMIT} sessions per batch"
                    ));
                    manager.sync_batch_checks();
                    return;
                }
                checked.insert(key.clone());
            } else {
                checked.remove(&key);
            }
            drop(checked);
            manager.update_batch_controls();
        });
        prevent_enter_activation(&row.checked);
    }

    pub(super) fn sync_batch_checks(&self) {
        self.bulk.synchronizing.set(true);
        for (row, session) in self.rows.borrow().iter().zip(self.visible.borrow().iter()) {
            row.checked.set_active(
                self.bulk
                    .checked
                    .borrow()
                    .contains(&(session.provider, session.id.clone())),
            );
        }
        self.bulk.synchronizing.set(false);
    }

    #[cfg(test)]
    fn select_visible_for_batch(&self) {
        self.select_batch_keys(self.batch_selection_identity().visible);
    }

    fn select_batch_keys(&self, keys: Vec<SessionIdentity>) {
        let additions = keys.into_iter().collect::<HashSet<_>>();
        if self.bulk.checked.borrow().union(&additions).count() > SESSION_BATCH_LIMIT {
            self.bulk.notice.set_text(&format!("This page would exceed {SESSION_BATCH_LIMIT} selected sessions. Clear some selections first."));
            return;
        }
        self.bulk.checked.borrow_mut().extend(additions);
        self.sync_batch_checks();
        self.update_batch_controls();
    }

    pub(super) fn batch_focus_is_editing(&self) -> bool {
        self.window
            .upgrade()
            .and_then(|window| gtk::prelude::GtkWindowExt::focus(&window))
            .is_some_and(|focus| {
                [&self.alias, &self.tags, &self.bulk.tag]
                    .iter()
                    .any(|entry| {
                        let widget = entry.upcast_ref::<gtk::Widget>();
                        focus == *widget || focus.is_ancestor(widget)
                    })
                    || self.rows.borrow().iter().any(|row| {
                        let widget = row.checked.upcast_ref::<gtk::Widget>();
                        focus == *widget || focus.is_ancestor(widget)
                    })
            })
    }

    pub(super) fn update_batch_controls(&self) {
        let keys = self.bulk.keys();
        let selection = self.batch_selection_identity();
        if self.bulk.selection_identity.borrow().as_ref() != Some(&selection) {
            self.bulk
                .selection_generation
                .set(self.bulk.selection_generation.get().wrapping_add(1));
            cancel_selection_activation(&self.bulk.select_visible);
            cancel_selection_activation(&self.bulk.clear);
            *self.bulk.selection_identity.borrow_mut() = Some(selection);
        }
        let visible = self
            .visible
            .borrow()
            .iter()
            .filter(|session| {
                self.bulk
                    .checked
                    .borrow()
                    .contains(&(session.provider, session.id.clone()))
            })
            .count();
        self.bulk.summary.set_text(&format!(
            "{} selected · {visible} visible · {} hidden · limit {SESSION_BATCH_LIMIT}",
            keys.len(),
            keys.len().saturating_sub(visible)
        ));
        self.bulk.clear.set_sensitive(!keys.is_empty());
        self.bulk
            .select_visible
            .set_sensitive(!self.visible.borrow().is_empty());
        let library = self.library.borrow();
        // Only the batch write itself matters; a periodic reload is not a save.
        if !library.is_saving() {
            if let Some(count) = self.bulk.pending.take() {
                self.bulk.notice.set_text(&match library.error() {
                    Some(error) => format!("Batch was not saved: {error}. Selection and drafts are retained."),
                    None => format!("Updated {count} selected sessions. Selection is retained for your next action."),
                });
            }
        }
        let enabled = !keys.is_empty()
            && library.is_loaded()
            && !library.is_saving()
            && library.error().is_none();
        drop(library);
        let state = ControlsIdentity {
            keys,
            tag: self.bulk.tag.text().to_string(),
            enabled,
        };
        if self.bulk.rendered.borrow().as_ref() == Some(&state) {
            return;
        }
        self.bulk
            .generation
            .set(self.bulk.generation.get().wrapping_add(1));
        let generation = self.bulk.generation.get();
        // Removing the old widgets also cancels GTK's deferred keyboard activation.
        // The epoch check still rejects any callback already queued by an old widget.
        while let Some(child) = self.bulk.actions.first_child() {
            self.bulk.actions.remove(&child);
        }
        // Explain an unusable tag while typing, exactly as the batch store
        // would refuse it, instead of only after Add/Remove is pressed.
        let tag_error = (!state.tag.trim().is_empty())
            .then(|| validate_session_batch_tag(&state.tag).err())
            .flatten();
        self.bulk
            .tag
            .set_tooltip_text(Some(tag_error.as_deref().unwrap_or(BATCH_TAG_TOOLTIP)));
        if tag_error.is_some() {
            self.bulk.tag.add_css_class("error");
        } else {
            self.bulk.tag.remove_css_class("error");
        }
        let actions = [
            ("Pin", SessionBatchAction::Pin(true)),
            ("Unpin", SessionBatchAction::Pin(false)),
            ("Archive", SessionBatchAction::Archive(true)),
            ("Restore", SessionBatchAction::Archive(false)),
            ("Add tag", SessionBatchAction::AddTag(state.tag.clone())),
            (
                "Remove tag",
                SessionBatchAction::RemoveTag(state.tag.clone()),
            ),
        ];
        for (label, action) in actions {
            let button = gtk::Button::with_label(label);
            let tag_action = matches!(
                action,
                SessionBatchAction::AddTag(_) | SessionBatchAction::RemoveTag(_)
            );
            button.set_sensitive(
                enabled && (!tag_action || (!state.tag.trim().is_empty() && tag_error.is_none())),
            );
            let rendered = BatchRequest {
                generation,
                keys: state.keys.clone(),
                action,
            };
            let pressed = Rc::new(RefCell::new(None));
            let gesture = gtk::GestureClick::new();
            gesture.set_button(1);
            gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
            gesture.connect_pressed({
                let pressed = pressed.clone();
                let rendered = rendered.clone();
                move |_, _, _, _| {
                    *pressed.borrow_mut() = Some(rendered.clone());
                }
            });
            button.add_controller(gesture);
            let weak = self.bulk.owner.borrow().clone();
            button.connect_clicked(move |_| {
                let request = pressed
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| rendered.clone());
                if let Some(manager) = weak.upgrade() {
                    manager.apply_session_batch(request);
                }
            });
            self.bulk.actions.append(&button);
        }
        *self.bulk.rendered.borrow_mut() = Some(state);
    }

    fn apply_session_batch(&self, request: BatchRequest) {
        if request.generation != self.bulk.generation.get() || request.keys != self.bulk.keys() {
            return;
        }
        if !matches!(request.action, SessionBatchAction::Pin(_))
            && request.keys.iter().any(|(provider, id)| {
                self.organization_drafts
                    .borrow()
                    .contains_key(&format!("{}:{id}", provider.label()))
            })
        {
            self.bulk.notice.set_text("Save or Revert the selected sessions' name/tag drafts before archiving or changing their tags.");
            return;
        }
        let result = self
            .library
            .borrow_mut()
            .apply_batch(&request.keys, request.action);
        match result {
            Ok(count) => {
                self.bulk.pending.set(Some(count));
                self.bulk
                    .notice
                    .set_text(&format!("Saving changes for {count} sessions…"));
                self.update_library_status();
            }
            Err(error) => self.bulk.notice.set_text(&format!(
                "Batch was not saved: {error}. Selection and drafts are retained."
            )),
        }
        self.update_batch_controls();
    }
}

#[cfg(test)]
mod native_tests {
    use super::super::{
        show_inner, AgentSession, SessionLibraryStore, SessionProvider, SessionSearchIndex,
        SessionStatus, TEST_MANAGER,
    };
    use super::*;
    use std::time::{Duration, Instant};

    fn settle(manager: &Manager) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let changed = manager.library.borrow_mut().poll();
            if changed {
                manager.render_view(true);
            }
            manager.update_library_status();
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
            if manager.library.borrow().is_loaded() && !manager.library.borrow().is_busy() {
                break;
            }
            assert!(Instant::now() < deadline, "library did not settle");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(manager.library.borrow().error().is_none());
    }

    fn button(manager: &Manager, label: &str) -> gtk::Button {
        let mut child = manager.bulk.actions.first_child();
        while let Some(widget) = child {
            if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                if button.label().as_deref() == Some(label) {
                    return button.clone();
                }
            }
            child = widget.next_sibling();
        }
        panic!("missing batch action {label}");
    }

    #[test]
    #[ignore = "requires a native GTK display; run under Xvfb"]
    fn agent_management_v3_batch_controls_keep_exact_targets_and_drafts() {
        gtk::init().expect("GTK display");
        let directory = std::env::temp_dir().join(format!(
            "session-batch-gtk-{}",
            gtk::glib::uuid_string_random()
        ));
        std::fs::create_dir(&directory).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let library = Rc::new(RefCell::new(SessionLibraryStore::with_path(
            directory.join("library.json"),
        )));
        let launches = Rc::new(Cell::new(0));
        let captured = launches.clone();
        let parent = gtk::Window::new();
        show_inner(
            &parent,
            Some(directory.clone()),
            library.clone(),
            HashSet::new(),
            move |request| {
                captured.set(captured.get() + 1);
                let _ = request.completion.send(Ok(()));
            },
            false,
        );
        let manager = TEST_MANAGER.with(|slot| slot.borrow().upgrade()).unwrap();
        let a = (
            SessionProvider::Codex,
            "01912345-1234-7123-8123-123456789abc".to_string(),
        );
        let b = (
            SessionProvider::Claude,
            "01912345-1234-7123-8123-123456789abd".to_string(),
        );
        manager.catalog.borrow_mut().sessions = [(&a, "Alpha task"), (&b, "Beta task")]
            .into_iter()
            .map(|(key, title)| AgentSession {
                provider: key.0,
                id: key.1.clone(),
                title: title.into(),
                cwd: directory.clone(),
                updated_at_ms: 0,
                status: SessionStatus::Idle,
                preview: String::new(),
                branch: None,
                source: directory.join("fixture.jsonl"),
                archived: false,
            })
            .collect();
        manager.existing.borrow_mut().extend([a.clone(), b.clone()]);
        *manager.search_index.borrow_mut() = SessionSearchIndex::new(&manager.catalog.borrow());
        manager.render();
        settle(&manager);
        library
            .borrow_mut()
            .set_details(a.0, &a.1, "Alpha named", "existing")
            .unwrap();
        settle(&manager);
        let inspected = manager.selected.borrow().clone();
        manager.search.set_text("Alpha");
        let selection_controllers = manager.bulk.select_visible.observe_controllers();
        let selection_press = (0..selection_controllers.n_items())
            .filter_map(|index| {
                selection_controllers
                    .item(index)
                    .and_downcast::<gtk::GestureClick>()
            })
            .find(|gesture| gesture.propagation_phase() == gtk::PropagationPhase::Capture)
            .unwrap();
        selection_press.emit_by_name::<()>("pressed", &[&1_i32, &8.0_f64, &8.0_f64]);
        manager.search.set_text("Beta");
        manager.bulk.select_visible.emit_clicked();
        assert!(
            manager.bulk.keys().is_empty(),
            "Select visible cannot acquire a replacement page after pointer press"
        );
        manager
            .bulk
            .select_visible
            .emit_by_name::<()>("activate", &[]);
        manager.search.set_text("Alpha");
        let deadline = Instant::now() + Duration::from_millis(320);
        while Instant::now() < deadline {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            manager.bulk.keys().is_empty(),
            "Select visible cannot acquire a replacement page during keyboard activation"
        );
        manager.search.set_text("");
        manager.select_visible_for_batch();
        manager.bulk.clear.emit_by_name::<()>("activate", &[]);
        manager.bulk.checked.borrow_mut().remove(&b);
        manager.sync_batch_checks();
        manager.update_batch_controls();
        let deadline = Instant::now() + Duration::from_millis(320);
        while Instant::now() < deadline {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            manager.bulk.keys(),
            vec![a.clone()],
            "Clear selection cannot clear a replacement selection"
        );
        manager.bulk.clear.emit_clicked();
        assert_eq!(
            manager.bulk.keys(),
            vec![a.clone()],
            "an already queued stale clear callback is also rejected"
        );
        manager.select_visible_for_batch();
        assert_eq!(manager.bulk.keys().len(), 2);
        assert_eq!(*manager.selected.borrow(), inspected);
        manager.search.set_text("Alpha");
        assert_eq!(manager.visible.borrow().len(), 1);
        assert!(manager.bulk.summary.text().contains("1 hidden"));
        button(&manager, "Pin").emit_clicked();
        assert_eq!(
            manager.bulk.keys().len(),
            2,
            "pending admission retains selection"
        );
        settle(&manager);
        assert!(library.borrow().snapshot().is_pinned(a.0, &a.1));
        assert!(library.borrow().snapshot().is_pinned(b.0, &b.1));

        manager.bulk.tag.set_text("old-tag");
        let stale = button(&manager, "Add tag");
        let controllers = stale.observe_controllers();
        let gesture = (0..controllers.n_items())
            .filter_map(|index| controllers.item(index).and_downcast::<gtk::GestureClick>())
            .find(|gesture| gesture.propagation_phase() == gtk::PropagationPhase::Capture)
            .unwrap();
        gesture.emit_by_name::<()>("pressed", &[&1_i32, &8.0_f64, &8.0_f64]);
        manager.bulk.tag.set_text("shared");
        // Even an already queued old callback is rejected after the tag changes.
        stale.emit_clicked();
        settle(&manager);
        assert!(!library
            .borrow()
            .snapshot()
            .tags(a.0, &a.1)
            .iter()
            .any(|tag| tag == "old-tag"));
        button(&manager, "Add tag").emit_clicked();
        settle(&manager);
        assert_eq!(
            library.borrow().snapshot().alias(a.0, &a.1),
            Some("Alpha named")
        );
        assert!(library
            .borrow()
            .snapshot()
            .tags(a.0, &a.1)
            .contains(&"existing".into()));
        assert!(library
            .borrow()
            .snapshot()
            .tags(a.0, &a.1)
            .contains(&"shared".into()));
        assert!(library
            .borrow()
            .snapshot()
            .tags(b.0, &b.1)
            .contains(&"shared".into()));

        // Delayed GtkButton keyboard activation cannot acquire a new selection.
        let stale = button(&manager, "Unpin");
        stale.emit_by_name::<()>("activate", &[]);
        manager.bulk.clear.emit_clicked();
        manager.select_visible_for_batch();
        let deadline = Instant::now() + Duration::from_millis(320);
        while Instant::now() < deadline {
            while gtk::glib::MainContext::default().pending() {
                gtk::glib::MainContext::default().iteration(false);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        settle(&manager);
        assert!(library.borrow().snapshot().is_pinned(a.0, &a.1));
        assert!(library.borrow().snapshot().is_pinned(b.0, &b.1));
        assert_eq!(manager.bulk.keys(), vec![a.clone()]);
        manager.alias.set_text("Unsaved name retained");
        button(&manager, "Archive").emit_clicked();
        assert!(manager.bulk.notice.text().contains("Save or Revert"));
        assert_eq!(manager.alias.text().as_str(), "Unsaved name retained");
        assert_eq!(manager.bulk.keys(), vec![a.clone()]);
        assert!(!library.borrow().snapshot().is_archived(a.0, &a.1));
        manager.revert_organization();
        manager.bulk.tag.set_text("invalid,tag");
        button(&manager, "Add tag").emit_clicked();
        assert!(manager.bulk.notice.text().contains("not saved"));
        assert_eq!(manager.bulk.keys(), vec![a.clone()]);
        manager.bulk.tag.set_text("shared");
        button(&manager, "Remove tag").emit_clicked();
        settle(&manager);
        assert_eq!(library.borrow().snapshot().tags(a.0, &a.1), &["existing"]);
        assert_eq!(library.borrow().snapshot().tags(b.0, &b.1), &["shared"]);
        manager.bulk.tag.grab_focus();
        while gtk::glib::MainContext::default().pending() {
            gtk::glib::MainContext::default().iteration(false);
        }
        assert!(manager.batch_focus_is_editing());
        manager
            .list
            .emit_by_name::<()>("row-activated", &[&manager.list.row_at_index(0).unwrap()]);
        assert_eq!(
            launches.get(),
            0,
            "an editor Enter must never resume a session"
        );

        manager.clear_filters();
        manager.select_visible_for_batch();
        button(&manager, "Archive").emit_clicked();
        settle(&manager);
        assert!(manager.visible.borrow().is_empty());
        assert_eq!(manager.bulk.keys().len(), 2);
        assert!(manager.bulk.summary.text().contains("2 hidden"));
        button(&manager, "Restore").emit_clicked();
        settle(&manager);
        assert_eq!(manager.visible.borrow().len(), 2);
        assert_eq!(launches.get(), 0);
        if let Some(path) = std::env::var_os("JTERM_AGENT_BATCH_SCREENSHOT") {
            assert!(std::process::Command::new("import")
                .args(["-window", "root"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }
        manager.window.upgrade().unwrap().close();
        parent.close();
        drop(manager);
        drop(library);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
