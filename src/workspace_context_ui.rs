//! Native context editing over the revisioned workspace host. Reading material
//! never sends it; a reviewed handoff opens an ordinary, unsent new draft.
use super::workspace_context::{ContextEditor, ContextField};
use super::workspace_navigation::Action;
use gtk::prelude::*;
use jterm_core::agent_workspace::{
    ConversationId, WorkspaceContext, WorkspaceMaterial, WorkspaceTextDocument,
};
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone)]
struct MaterialWidgets {
    root: gtk::Box,
    title: gtk::Label,
    source: gtk::Label,
    buffer: gtk::TextBuffer,
}

#[derive(Clone)]
pub(crate) struct ContextSurface {
    pub root: gtk::ScrolledWindow,
    editor: Rc<RefCell<ContextEditor>>,
    rendering: Rc<Cell<bool>>,
    fields: Vec<gtk::TextBuffer>,
    field_views: Vec<gtk::ScrolledWindow>,
    materials: gtk::Box,
    material_widgets: Rc<RefCell<HashMap<String, MaterialWidgets>>>,
    status: gtk::Label,
    save: gtk::Button,
    reload: gtk::Button,
    add: gtk::Button,
    conflict: gtk::Box,
    preview: gtk::Button,
    provider: gtk::DropDown,
    handoff: gtk::TextBuffer,
    handoff_section: gtk::Box,
    create: gtk::Button,
    rendered: Rc<RefCell<Option<WorkspaceContext>>>,
    rendered_packet: Rc<RefCell<Option<String>>>,
    pending_evidence: Rc<RefCell<Option<WorkspaceTextDocument>>>,
    pending_material: Rc<RefCell<Option<(ConversationId, WorkspaceMaterial)>>>,
}

fn label(text: &str) -> gtk::Label {
    let widget = gtk::Label::new(Some(text));
    widget.set_xalign(0.0);
    widget.set_wrap(true);
    widget.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    widget
}

fn editor_view(editable: bool) -> (gtk::ScrolledWindow, gtk::TextBuffer) {
    let view = gtk::TextView::new();
    view.add_css_class("workspace-material-editor");
    view.set_editable(editable);
    view.set_cursor_visible(editable);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_top_margin(8);
    // Keep a full reading line beneath the cursor in a three-line minimum.
    view.set_bottom_margin(24);
    view.set_left_margin(8);
    view.set_right_margin(8);
    let buffer = view.buffer();
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_min_content_height(100);
    scroll.set_max_content_height(220);
    scroll.set_propagate_natural_height(true);
    scroll.set_child(Some(&view));
    if editable {
        crate::composer_geometry::install(&view, &scroll, 104, 240);
    }
    (scroll, buffer)
}

impl ContextSurface {
    pub fn new(callback: Rc<dyn Fn(Action)>) -> Self {
        let editor = Rc::new(RefCell::new(ContextEditor::default()));
        let rendering = Rc::new(Cell::new(false));
        let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
        body.add_css_class("workspace-surface");
        body.set_margin_top(16);
        body.set_margin_bottom(16);
        body.set_margin_start(16);
        body.set_margin_end(16);
        let heading = label("Context and handoff");
        heading.add_css_class("title-2");
        body.append(&heading);
        body.append(&label("Review the goal and selected excerpts. Saving stores these materials locally in the shared workspace. A handoff creates an editable new conversation; sending remains explicit."));
        let status = label("Loading saved materials…");
        status.add_css_class("dim-label");
        body.append(&status);
        let controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
        controls.set_halign(gtk::Align::Start);
        let save = gtk::Button::with_label("Save materials");
        save.add_css_class("suggested-action");
        let state = editor.clone();
        save.connect_clicked(move |_| state.borrow_mut().save());
        controls.append(&save);
        let reload = gtk::Button::with_label("Reload saved");
        let state = editor.clone();
        reload.connect_clicked(move |_| state.borrow_mut().reload());
        controls.append(&reload);
        body.append(&controls);
        let conflict = gtk::Box::new(gtk::Orientation::Vertical, 6);
        conflict.append(&label("Another window saved a newer revision. Your edits are retained. Choose a draft before saving."));
        for (title, keep_local) in [
            ("Keep my draft against the new revision", true),
            ("Use the shared saved materials", false),
        ] {
            let button = gtk::Button::with_label(title);
            let state = editor.clone();
            button.connect_clicked(move |_| {
                if keep_local {
                    state.borrow_mut().keep_local();
                } else {
                    state.borrow_mut().use_shared();
                }
            });
            conflict.append(&button);
        }
        conflict.set_visible(false);
        body.append(&conflict);
        let mut fields = Vec::new();
        let mut field_views = Vec::new();
        for (title, field) in [
            ("Goal", ContextField::Goal),
            ("Completed", ContextField::Completed),
            ("Remaining", ContextField::Remaining),
            ("Constraints", ContextField::Constraints),
        ] {
            let heading = label(title);
            heading.add_css_class("heading");
            body.append(&heading);
            let (view, buffer) = editor_view(true);
            view.update_property(&[gtk::accessible::Property::Label(title)]);
            let state = editor.clone();
            let guard = rendering.clone();
            buffer.connect_changed(move |buffer| {
                if !guard.get() {
                    state.borrow_mut().set_field(
                        field,
                        buffer
                            .text(&buffer.start_iter(), &buffer.end_iter(), false)
                            .into(),
                    );
                }
            });
            fields.push(buffer);
            body.append(&view);
            field_views.push(view);
        }
        let heading = label("Selected materials");
        heading.add_css_class("heading");
        body.append(&heading);
        body.append(&label("Add an editable note here, or use “Add excerpt to context” on an exact history match. Every material can be previewed, shortened or removed."));
        let add = gtk::Button::with_label("Add note or pasted excerpt");
        add.set_halign(gtk::Align::Start);
        let state = editor.clone();
        add.connect_clicked(move |_| state.borrow_mut().add_note());
        body.append(&add);
        let materials = gtk::Box::new(gtk::Orientation::Vertical, 14);
        body.append(&materials);
        let heading = label("New conversation handoff");
        heading.add_css_class("heading");
        body.append(&heading);
        let provider = gtk::DropDown::from_strings(&[
            "Codex task",
            "Claude task",
            "Kimi task",
            "API conversation",
        ]);
        provider.update_property(&[gtk::accessible::Property::Label("Handoff target")]);
        body.append(&provider);
        let preview = gtk::Button::with_label("Prepare handoff preview");
        preview.set_halign(gtk::Align::Start);
        let state = editor.clone();
        let target = provider.clone();
        preview.connect_clicked(move |_| {
            state
                .borrow_mut()
                .build_handoff(target_provider(target.selected()))
        });
        body.append(&preview);
        let handoff_section = gtk::Box::new(gtk::Orientation::Vertical, 8);
        handoff_section.append(&label("Review this exact handoff before opening the target draft. Existing provider threads, tool approvals and execution leases stay with the source."));
        let (handoff_view, handoff) = editor_view(false);
        handoff_view.set_min_content_height(200);
        handoff_view.set_max_content_height(360);
        handoff_section.append(&handoff_view);
        let create = gtk::Button::with_label("Open editable new draft");
        create.add_css_class("suggested-action");
        create.set_halign(gtk::Align::Start);
        let state = editor.clone();
        let target = provider.clone();
        create.connect_clicked(move |_| {
            let state = state.borrow();
            if !state.packet_current() {
                return;
            }
            let Some(packet) = state.packet.clone() else {
                return;
            };
            if packet.target_provider != target_provider(target.selected()) {
                return;
            }
            drop(state);
            callback(if packet.target_provider == "api" {
                Action::PrefillApi { body: packet.body }
            } else {
                Action::PrefillTask {
                    title: packet.title,
                    cwd: packet.cwd,
                    provider: packet.target_provider,
                    body: packet.body,
                }
            });
        });
        handoff_section.append(&create);
        handoff_section.set_visible(false);
        body.append(&handoff_section);
        let root = gtk::ScrolledWindow::new();
        root.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        root.set_vexpand(true);
        super::workspace_navigation::wrap_button_labels(&body);
        root.set_child(Some(&body));
        Self {
            root,
            editor,
            rendering,
            fields,
            field_views,
            materials,
            material_widgets: Rc::new(RefCell::new(HashMap::new())),
            status,
            save,
            reload,
            add,
            conflict,
            preview,
            provider,
            handoff,
            handoff_section,
            create,
            rendered: Rc::new(RefCell::new(None)),
            rendered_packet: Rc::new(RefCell::new(None)),
            pending_evidence: Rc::new(RefCell::new(None)),
            pending_material: Rc::new(RefCell::new(None)),
        }
    }

    pub fn select(&self, id: ConversationId) {
        if self.editor.borrow().selected() != Some(&id) {
            // Commit/cancel the previous TextView's IME while its original
            // conversation is still selected; composition cannot leak across.
            for scroll in &self.field_views {
                if let Some(view) = scroll.child().and_downcast::<gtk::TextView>() {
                    view.reset_im_context();
                }
            }
            for widget in self.material_widgets.borrow().values() {
                if let Some(scroll) = widget
                    .source
                    .next_sibling()
                    .and_downcast::<gtk::ScrolledWindow>()
                {
                    if let Some(view) = scroll.child().and_downcast::<gtk::TextView>() {
                        view.reset_im_context();
                    }
                }
            }
            self.pending_evidence.borrow_mut().take();
            self.pending_material.borrow_mut().take();
            self.save.set_sensitive(false);
            self.preview.set_sensitive(false);
            self.create.set_sensitive(false);
        }
        self.editor.borrow_mut().select(id);
        self.render();
    }

    pub fn deselect(&self) {
        for scroll in &self.field_views {
            if let Some(view) = scroll.child().and_downcast::<gtk::TextView>() {
                view.reset_im_context();
            }
        }
        self.editor.borrow_mut().deselect();
        self.pending_evidence.borrow_mut().take();
        self.pending_material.borrow_mut().take();
        self.render();
    }

    pub fn add_evidence(&self, document: WorkspaceTextDocument) {
        self.select(document.conversation.clone());
        *self.pending_evidence.borrow_mut() = Some(document);
        self.poll();
    }

    pub fn add_material(&self, id: ConversationId, material: WorkspaceMaterial) {
        self.select(id.clone());
        *self.pending_material.borrow_mut() = Some((id, material));
        self.poll();
    }

    pub fn poll(&self) {
        self.editor.borrow_mut().poll();
        let ready = {
            let editor = self.editor.borrow();
            editor.draft().is_some() && !editor.loading()
        };
        if ready {
            if let Some(document) = self.pending_evidence.borrow_mut().take() {
                let mut editor = self.editor.borrow_mut();
                if editor.selected() == Some(&document.conversation) {
                    editor.add_evidence(&document);
                }
            }
            if let Some((id, material)) = self.pending_material.borrow_mut().take() {
                let mut editor = self.editor.borrow_mut();
                if editor.selected() == Some(&id) {
                    editor.add_material(material);
                }
            }
        }
        self.render();
    }

    fn render(&self) {
        let state = self.editor.borrow();
        let available = state.draft().is_some();
        for view in &self.field_views {
            view.set_sensitive(available);
        }
        let valid = state.draft().is_some_and(|draft| draft.validate().is_ok());
        self.save.set_sensitive(
            available
                && valid
                && state.dirty()
                && !state.loading()
                && !state.saving()
                && !state.conflicted(),
        );
        self.reload
            .set_sensitive(!state.loading() && !state.saving());
        self.add.set_sensitive(
            available
                && state
                    .draft()
                    .is_some_and(|draft| draft.materials.len() < 16),
        );
        self.preview.set_sensitive(
            available
                && valid
                && !state.dirty()
                && !state.conflicted()
                && !state.loading()
                && !state.saving()
                && !state.building(),
        );
        self.conflict.set_visible(state.conflicted());
        let status = if let Some(notice) = &state.notice {
            notice.clone()
        } else if state.loading() {
            "Loading saved materials…".into()
        } else if state.saving() {
            "Saving the reviewed revision…".into()
        } else if state.building() {
            "Preparing the saved handoff preview…".into()
        } else if let Some(draft) = state.draft() {
            let bytes = draft.goal.len()
                + draft.completed.len()
                + draft.remaining.len()
                + draft.constraints.len()
                + draft
                    .materials
                    .iter()
                    .map(|material| material.text.len() + material.label.len())
                    .sum::<usize>();
            format!(
                "Revision {} · {bytes} / 65,536 bytes · {}",
                draft.revision,
                if state.dirty() {
                    "Unsaved local edits"
                } else {
                    "Saved materials"
                }
            )
        } else {
            "No saved materials are available.".into()
        };
        if self.status.text().as_str() != status {
            self.status.set_text(&status);
        }
        self.rendering.set(true);
        if self.rendered.borrow().as_ref() != state.draft() {
            let previous = self.rendered.borrow().clone();
            let draft = state.draft();
            let values = draft.map(|d| [&d.goal, &d.completed, &d.remaining, &d.constraints]);
            for (index, buffer) in self.fields.iter().enumerate() {
                let value = values
                    .as_ref()
                    .map(|fields| fields[index].as_str())
                    .unwrap_or_default();
                set_buffer(buffer, value);
            }
            let mut widgets = self.material_widgets.borrow_mut();
            let ids: std::collections::HashSet<&str> = draft
                .map(|draft| {
                    draft
                        .materials
                        .iter()
                        .map(|material| material.id.as_str())
                        .collect()
                })
                .unwrap_or_default();
            let stale: Vec<String> = widgets
                .keys()
                .filter(|id| !ids.contains(id.as_str()))
                .cloned()
                .collect();
            for id in stale {
                if let Some(widget) = widgets.remove(&id) {
                    self.materials.remove(&widget.root);
                }
            }
            if let Some(draft) = draft {
                for material in &draft.materials {
                    let widget = widgets
                        .entry(material.id.clone())
                        .or_insert_with(|| self.material(&material.id));
                    widget.title.set_text(&material.label);
                    let source = material
                        .source
                        .as_ref()
                        .map(|source| {
                            format!(
                                "{:?} · {} / {} @ {}{}",
                                material.kind,
                                source.conversation.0,
                                source.event_id,
                                source.revision,
                                if material.truncated {
                                    " · edited or partial excerpt"
                                } else {
                                    ""
                                }
                            )
                        })
                        .unwrap_or_else(|| {
                            format!(
                                "{:?} · user-selected text{}",
                                material.kind,
                                if material.truncated {
                                    " · edited excerpt"
                                } else {
                                    ""
                                }
                            )
                        });
                    widget.source.set_text(&source);
                    let unchanged = previous
                        .as_ref()
                        .and_then(|draft| draft.materials.iter().find(|old| old.id == material.id))
                        .is_some_and(|old| old.text == material.text);
                    if !unchanged {
                        set_buffer(&widget.buffer, &material.text);
                    }
                }
            }
            *self.rendered.borrow_mut() = draft.cloned();
        }
        let packet = state.packet.as_ref().filter(|_| state.packet_current());
        self.handoff_section.set_visible(packet.is_some());
        self.create.set_sensitive(packet.is_some_and(|packet| {
            packet.target_provider == target_provider(self.provider.selected())
        }));
        let packet_text = packet.map(|packet| packet.body.as_str());
        if self.rendered_packet.borrow().as_deref() != packet_text {
            self.handoff.set_text(packet_text.unwrap_or_default());
            *self.rendered_packet.borrow_mut() = packet_text.map(str::to_owned);
        }
        self.rendering.set(false);
    }

    fn material(&self, id: &str) -> MaterialWidgets {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let title = label("");
        title.add_css_class("heading");
        root.append(&title);
        let source = label("");
        source.add_css_class("dim-label");
        root.append(&source);
        let (view, buffer) = editor_view(true);
        let state = self.editor.clone();
        let guard = self.rendering.clone();
        let key = id.to_owned();
        buffer.connect_changed(move |buffer| {
            if !guard.get() {
                state.borrow_mut().set_material(
                    &key,
                    buffer
                        .text(&buffer.start_iter(), &buffer.end_iter(), false)
                        .into(),
                );
            }
        });
        root.append(&view);
        let remove = gtk::Button::with_label("Remove material");
        remove.set_halign(gtk::Align::Start);
        remove.add_css_class("flat");
        let state = self.editor.clone();
        let key = id.to_owned();
        remove.connect_clicked(move |_| state.borrow_mut().remove_material(&key));
        root.append(&remove);
        self.materials.append(&root);
        MaterialWidgets {
            root,
            title,
            source,
            buffer,
        }
    }
}

fn set_buffer(buffer: &gtk::TextBuffer, text: &str) {
    if buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .as_str()
        != text
    {
        buffer.set_text(text);
    }
}

fn target_provider(selected: u32) -> &'static str {
    match selected {
        1 => "claude",
        2 => "kimi",
        3 => "api",
        _ => "codex",
    }
}
