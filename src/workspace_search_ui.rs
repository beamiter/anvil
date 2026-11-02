//! Native read-only search surfaces over the shared evidence index.
use super::workspace_navigation::Action;
use super::workspace_tools::WorkspaceTools;
use gtk::prelude::*;
use jterm_core::agent_workspace::{ConversationId, ConversationKind, WorkspaceSearchHit};
use relm4::gtk;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct SearchSurface {
    pub root: gtk::Box,
    pub evidence: gtk::Box,
    tools: Rc<RefCell<WorkspaceTools>>,
    matches: gtk::Box,
    status: gtk::Label,
    more: gtk::Button,
    document_title: gtk::Label,
    document_status: gtk::Label,
    document_text: gtk::TextView,
    open_source: gtk::Button,
    add_context: gtk::Button,
    rendered: Rc<RefCell<String>>,
    rendered_document: Rc<RefCell<String>>,
    surface_id: u64,
    callback: Rc<dyn Fn(Action)>,
}

impl SearchSurface {
    pub fn new(surface_id: u64, callback: Rc<dyn Fn(Action)>) -> Self {
        let tools = Rc::new(RefCell::new(WorkspaceTools::default()));
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let heading = gtk::Label::new(Some("History matches"));
        heading.set_xalign(0.0);
        heading.add_css_class("heading");
        root.append(&heading);
        let status = gtk::Label::new(Some("Search saved messages and task evidence"));
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        status.add_css_class("dim-label");
        root.append(&status);
        let matches = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.append(&matches);
        let more = gtk::Button::with_label("More history matches");
        more.add_css_class("flat");
        more.set_visible(false);
        let next = tools.clone();
        more.connect_clicked(move |_| next.borrow_mut().load_more());
        root.append(&more);

        let evidence = gtk::Box::new(gtk::Orientation::Vertical, 10);
        evidence.add_css_class("workspace-surface");
        evidence.add_css_class("workspace-evidence");
        evidence.set_focusable(true);
        evidence.set_vexpand(true);
        let document_title = gtk::Label::new(Some("Search evidence"));
        document_title.set_xalign(0.0);
        document_title.set_wrap(true);
        document_title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        document_title.add_css_class("title-2");
        evidence.append(&document_title);
        let document_status = gtk::Label::new(Some("Loading the exact saved revision…"));
        document_status.set_xalign(0.0);
        document_status.set_wrap(true);
        document_status.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        evidence.append(&document_status);
        let open_source = gtk::Button::with_label("Open conversation");
        open_source.set_halign(gtk::Align::Start);
        open_source.set_sensitive(false);
        let source_tools = tools.clone();
        let open = callback.clone();
        open_source.connect_clicked(move |_| {
            let state = source_tools.borrow();
            let Some(hit) = state.selected_hit.as_ref() else {
                return;
            };
            let Some(document) = state.document.as_ref() else {
                return;
            };
            if document.conversation != hit.row.id
                || document.event_id != hit.anchor.event_id
                || document.revision != hit.anchor.revision
            {
                return;
            }
            let action = source_action(hit);
            drop(state);
            open(action);
        });
        evidence.append(&open_source);
        let add_context = gtk::Button::with_label("Add excerpt to source context");
        add_context.set_halign(gtk::Align::Start);
        add_context.set_sensitive(false);
        let add = callback.clone();
        add_context.connect_clicked(move |_| add(Action::AddEvidenceContext(surface_id)));
        evidence.append(&add_context);
        let clear = gtk::Button::with_label("Clear evidence selection");
        clear.add_css_class("flat");
        clear.set_halign(gtk::Align::Start);
        let clear_tools = tools.clone();
        clear.connect_clicked(move |_| clear_tools.borrow_mut().close_document());
        evidence.append(&clear);
        let document_text = gtk::TextView::new();
        document_text.add_css_class("workspace-material-editor");
        document_text.set_editable(false);
        document_text.set_cursor_visible(false);
        document_text.set_wrap_mode(gtk::WrapMode::WordChar);
        document_text.set_top_margin(8);
        document_text.set_bottom_margin(8);
        document_text.set_left_margin(8);
        document_text.set_right_margin(8);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_child(Some(&document_text));
        scroll.set_vexpand(true);
        evidence.append(&scroll);
        super::workspace_navigation::wrap_button_labels(&evidence);
        Self {
            root,
            evidence,
            tools,
            matches,
            status,
            more,
            document_title,
            document_status,
            document_text,
            open_source,
            add_context,
            rendered: Rc::new(RefCell::new(String::new())),
            rendered_document: Rc::new(RefCell::new(String::new())),
            surface_id,
            callback,
        }
    }

    pub fn set_query(
        &self,
        query: &str,
        conversation: Option<ConversationId>,
        include_archived: bool,
    ) {
        self.tools
            .borrow_mut()
            .set_search(query, conversation, include_archived);
        self.root.set_visible(!query.trim().is_empty());
        self.render();
    }

    pub fn search_entry(&self, conversation: ConversationId) -> gtk::SearchEntry {
        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some("Search this conversation's history"));
        entry.set_hexpand(true);
        let surface = self.clone();
        entry.connect_search_changed(move |entry| {
            surface.set_query(&entry.text(), Some(conversation.clone()), true);
        });
        entry
    }

    pub fn poll(&self) {
        let changed = self.tools.borrow_mut().poll();
        if changed {
            self.render();
        }
        // A click queues its read outside the poll reducer; reflect that state
        // without rebuilding result rows or resetting a reader's selection.
        self.render_document();
    }

    fn render(&self) {
        let state = self.tools.borrow();
        let summary = if let Some(error) = &state.notice {
            error.clone()
        } else if state.searching() {
            "Searching saved history…".into()
        } else if state.page.hits.is_empty() {
            format!("No indexed matches. {}", state.page.coverage.label())
        } else {
            format!(
                "{} matches on loaded pages. {}",
                state.page.hits.len(),
                state.page.coverage.label()
            )
        };
        self.status.set_text(&summary);
        self.more.set_visible(state.page.next_before.is_some());
        self.more.set_sensitive(!state.searching());
        let signature = state
            .page
            .hits
            .iter()
            .map(|hit| {
                format!(
                    "{}:{}:{}",
                    hit.row.id, hit.anchor.event_id, hit.anchor.revision
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if *self.rendered.borrow() == signature {
            return;
        }
        *self.rendered.borrow_mut() = signature;
        while let Some(child) = self.matches.first_child() {
            self.matches.remove(&child);
        }
        for hit in &state.page.hits {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
            let title = gtk::Label::new(Some(&hit.row.title));
            title.set_xalign(0.0);
            title.set_wrap(true);
            title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            title.add_css_class("heading");
            body.append(&title);
            let source = gtk::Label::new(Some(&format!(
                "{} · {}",
                hit.row.provider,
                hit.source.label()
            )));
            source.set_xalign(0.0);
            source.set_wrap(true);
            source.add_css_class("dim-label");
            body.append(&source);
            let excerpt = gtk::Label::new(Some(&hit.snippet));
            excerpt.set_xalign(0.0);
            excerpt.set_wrap(true);
            excerpt.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            excerpt.set_lines(3);
            excerpt.set_ellipsize(gtk::pango::EllipsizeMode::End);
            body.append(&excerpt);
            button.set_child(Some(&body));
            button.set_tooltip_text(Some(
                "Read the exact saved evidence. This does not launch or resume its conversation.",
            ));
            let tools = self.tools.clone();
            let hit = hit.clone();
            let callback = self.callback.clone();
            let id = self.surface_id;
            button.connect_clicked(move |_| {
                let mut state = tools.borrow_mut();
                if !state
                    .page
                    .hits
                    .iter()
                    .any(|current| current.row.id == hit.row.id && current.anchor == hit.anchor)
                {
                    return;
                }
                state.open_hit(hit.clone());
                drop(state);
                callback(Action::ShowEvidence(id));
            });
            self.matches.append(&button);
        }
    }

    fn render_document(&self) {
        let state = self.tools.borrow();
        let signature = format!(
            "{:?}/{:?}/{}/{:?}",
            state.selected_hit.as_ref().map(|hit| (
                &hit.row.id,
                &hit.anchor.event_id,
                &hit.anchor.revision
            )),
            state
                .document
                .as_ref()
                .map(|doc| (&doc.conversation, &doc.event_id, &doc.revision)),
            state.document_loading,
            state.notice
        );
        if *self.rendered_document.borrow() == signature {
            return;
        }
        *self.rendered_document.borrow_mut() = signature;
        let Some(hit) = state.selected_hit.as_ref() else {
            self.document_title.set_text("Search evidence");
            self.document_status
                .set_text("Choose a history match to read its saved source.");
            self.open_source.set_sensitive(false);
            self.add_context.set_sensitive(false);
            self.document_text.buffer().set_text("");
            return;
        };
        self.document_title.set_text(&hit.row.title);
        self.open_source
            .set_sensitive(state.document.is_some() && !state.document_loading);
        self.add_context
            .set_sensitive(state.document.is_some() && !state.document_loading);
        let (status, text) = if let Some(document) = &state.document {
            (
                format!(
                    "{} · Read only{}",
                    document.source.label(),
                    if document.truncated {
                        " · Indexed excerpt; source is longer"
                    } else {
                        ""
                    }
                ),
                document.text.as_str(),
            )
        } else if let Some(error) = &state.notice {
            (error.clone(), "")
        } else {
            ("Loading the exact saved revision…".into(), "")
        };
        if self.document_status.text().as_str() != status {
            self.document_status.set_text(&status);
        }
        let buffer = self.document_text.buffer();
        if buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .as_str()
            != text
        {
            buffer.set_text(text);
        }
    }

    pub fn selected_document(
        &self,
    ) -> Option<(
        jterm_core::agent_workspace::WorkspaceRow,
        jterm_core::agent_workspace::WorkspaceTextDocument,
    )> {
        let state = self.tools.borrow();
        let hit = state.selected_hit.as_ref()?;
        let document = state.document.as_ref()?;
        if state.document_loading
            || document.conversation != hit.row.id
            || document.event_id != hit.anchor.event_id
            || document.revision != hit.anchor.revision
        {
            return None;
        }
        Some((hit.row.clone(), document.clone()))
    }
}

fn source_action(hit: &WorkspaceSearchHit) -> Action {
    match hit.row.kind {
        ConversationKind::Api => Action::SharedApi(hit.row.id.clone()),
        ConversationKind::Cli => Action::Cli(hit.row.id.clone()),
        ConversationKind::Task => Action::OpenTask(hit.row.id.clone()),
    }
}
