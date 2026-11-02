//! SSH workspaces have a separate read-only surface, with no local execution
//! callbacks, context save, queue mutation or provider continuation controls.
use super::workspace_remote::RemoteWorkspace;
use gtk::prelude::*;
use relm4::gtk;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone)]
struct Surface {
    state: Rc<RefCell<RemoteWorkspace>>,
    status: gtk::Label,
    scope: gtk::Label,
    catalog: gtk::Box,
    hits: gtk::Box,
    catalog_more: gtk::Button,
    search_more: gtk::Button,
    refresh: gtk::Button,
    connect: gtk::Button,
    search: gtk::SearchEntry,
    document: gtk::TextBuffer,
    document_status: gtk::Label,
    catalog_signature: Rc<RefCell<String>>,
    search_signature: Rc<RefCell<String>>,
    document_signature: Rc<RefCell<String>>,
}
fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label
}

pub(crate) fn show(parent: &impl IsA<gtk::Window>) {
    let state = Rc::new(RefCell::new(RemoteWorkspace::default()));
    let window = gtk::Window::builder()
        .title("Remote workspace · read only")
        .transient_for(parent)
        .modal(true)
        .default_width(620)
        .default_height(760)
        .build();
    window.add_css_class("workspace-surface");
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.set_margin_start(16);
    body.set_margin_end(16);
    let heading = label("Remote workspace · read only");
    heading.add_css_class("title-2");
    body.append(&heading);
    body.append(&label("Connect through an existing SSH alias to an already-running workspace host. Strict known-host checks and BatchMode use your existing SSH setup. This viewer does not start a host, task, terminal or provider."));
    let alias = gtk::Entry::new();
    alias.set_placeholder_text(Some("SSH config alias"));
    alias.set_hexpand(true);
    body.append(&label("SSH alias"));
    body.append(&alias);
    let executable = gtk::Entry::new();
    executable.set_placeholder_text(Some(
        "Absolute remote path to the updated terminal executable",
    ));
    executable.set_hexpand(true);
    body.append(&label("Remote executable"));
    body.append(&executable);
    let host_id = gtk::Entry::new();
    host_id.set_placeholder_text(Some("Expected workspace host identity"));
    host_id.set_hexpand(true);
    body.append(&label("Expected host ID"));
    body.append(&host_id);
    body.append(&label("On the remote machine, obtain the identity with: <terminal executable> --jterm-workspace-host-id. Protocol and identity must match before any catalog is read."));
    let target = label("No remote workspace connected.");
    target.set_selectable(true);
    body.append(&target);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let connect = gtk::Button::with_label("Connect read only");
    connect.add_css_class("suggested-action");
    controls.append(&connect);
    let disconnect = gtk::Button::with_label("Disconnect");
    controls.append(&disconnect);
    body.append(&controls);
    let refresh = gtk::Button::with_label("Refresh remote catalog");
    refresh.set_halign(gtk::Align::Start);
    body.append(&refresh);
    let status = label("");
    status.add_css_class("dim-label");
    body.append(&status);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search the remote index"));
    search.set_sensitive(false);
    body.append(&search);
    let scope = label("All remote conversations");
    body.append(&scope);
    let all = gtk::Button::with_label("All remote conversations");
    all.set_halign(gtk::Align::Start);
    body.append(&all);
    let catalog = gtk::Box::new(gtk::Orientation::Vertical, 4);
    body.append(&catalog);
    let catalog_more = gtk::Button::with_label("More remote conversations");
    body.append(&catalog_more);
    let hits = gtk::Box::new(gtk::Orientation::Vertical, 4);
    body.append(&hits);
    let search_more = gtk::Button::with_label("More remote history matches");
    body.append(&search_more);
    let document_status = label("Select an exact history match to read its saved evidence.");
    body.append(&document_status);
    let view = gtk::TextView::new();
    view.add_css_class("workspace-material-editor");
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_top_margin(8);
    view.set_bottom_margin(24);
    view.set_left_margin(8);
    view.set_right_margin(8);
    let document = view.buffer();
    let doc_scroll = gtk::ScrolledWindow::new();
    doc_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    doc_scroll.set_min_content_height(200);
    doc_scroll.set_max_content_height(420);
    doc_scroll.set_propagate_natural_height(true);
    doc_scroll.set_child(Some(&view));
    body.append(&doc_scroll);
    let root = gtk::ScrolledWindow::new();
    root.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    root.set_child(Some(&body));
    super::workspace_navigation::wrap_button_labels(&body);
    window.set_child(Some(&root));
    let surface = Surface {
        state: state.clone(),
        status,
        scope,
        catalog,
        hits,
        catalog_more: catalog_more.clone(),
        search_more: search_more.clone(),
        refresh: refresh.clone(),
        connect: connect.clone(),
        search: search.clone(),
        document,
        document_status,
        catalog_signature: Rc::new(RefCell::new(String::new())),
        search_signature: Rc::new(RefCell::new(String::new())),
        document_signature: Rc::new(RefCell::new(String::new())),
    };
    let model = state.clone();
    let entry = search.clone();
    connect.connect_clicked(move |_| {
        let alias = alias.text().to_string();
        let executable = executable.text().to_string();
        let host = host_id.text().to_string();
        entry.set_text("");
        target.set_text(&format!(
            "Requested target: {}\nProgram: {}\nExpected host: {}",
            alias.trim(),
            executable.trim(),
            host.trim()
        ));
        model.borrow_mut().connect(&alias, &executable, &host);
    });
    let model = state.clone();
    disconnect.connect_clicked(move |_| model.borrow_mut().disconnect());
    let model = state.clone();
    refresh.connect_clicked(move |_| model.borrow_mut().refresh());
    let model = state.clone();
    catalog_more.connect_clicked(move |_| model.borrow_mut().load_more_catalog());
    let model = state.clone();
    search_more.connect_clicked(move |_| model.borrow_mut().load_more_search());
    let model = state.clone();
    search.connect_search_changed(move |entry| {
        let conversation = model.borrow().conversation().cloned();
        model.borrow_mut().set_search(&entry.text(), conversation);
    });
    let model = state.clone();
    let entry = search.clone();
    all.connect_clicked(move |_| {
        entry.set_text("");
        model.borrow_mut().set_search("", None);
    });
    let model = state.clone();
    window.connect_close_request(move |_| {
        model.borrow_mut().disconnect();
        gtk::glib::Propagation::Proceed
    });
    surface.render();
    let weak = window.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(window) = weak.upgrade().filter(|window| window.is_visible()) else {
            return gtk::glib::ControlFlow::Break;
        };
        surface.state.borrow_mut().poll();
        surface.render();
        drop(window);
        gtk::glib::ControlFlow::Continue
    });
    window.present();
}

impl Surface {
    fn render(&self) {
        let state = self.state.borrow();
        let status = if let Some(notice) = &state.notice {
            notice.clone()
        } else if state.loading {
            "Loading the remote catalog…".into()
        } else if state.search_loading {
            "Searching the remote index…".into()
        } else if state.connected {
            format!(
                "Connected read only · {} of {} catalog entries loaded · {}",
                state.rows.len(),
                state.total,
                state.page.coverage.label()
            )
        } else {
            "Not connected. Enter all three connection fields to continue.".into()
        };
        self.status.set_text(&status);
        self.connect.set_sensitive(!state.loading);
        self.refresh
            .set_sensitive(state.connected && !state.loading);
        self.search.set_sensitive(state.connected);
        self.catalog.set_visible(!state.searching());
        self.hits.set_visible(state.searching());
        self.catalog_more
            .set_visible(!state.searching() && state.next_before.is_some());
        self.catalog_more.set_sensitive(!state.loading);
        self.search_more
            .set_visible(state.searching() && state.page.next_before.is_some());
        self.search_more.set_sensitive(!state.search_loading);
        let scope = state
            .conversation()
            .and_then(|id| state.rows.iter().find(|row| &row.id == id))
            .map(|row| format!("Remote history: {}", row.title))
            .unwrap_or_else(|| "All remote conversations".into());
        self.scope.set_text(&scope);
        let signature = format!(
            "{}:{}",
            state.generation(),
            state
                .rows
                .iter()
                .map(|row| format!("{}:{}:{}", row.id, row.updated_at_ms, row.title))
                .collect::<Vec<_>>()
                .join("\n")
        );
        if *self.catalog_signature.borrow() != signature {
            *self.catalog_signature.borrow_mut() = signature;
            while let Some(child) = self.catalog.first_child() {
                self.catalog.remove(&child);
            }
            for row in &state.rows {
                let button = gtk::Button::new();
                button.add_css_class("flat");
                let text = label(&format!(
                    "{}\n{} · {}\n{}",
                    row.title,
                    row.provider,
                    row.presentation().status_label,
                    row.cwd
                ));
                button.set_child(Some(&text));
                button.set_tooltip_text(Some(
                    "Search this remote conversation's saved history. No runtime is activated.",
                ));
                let model = self.state.clone();
                let generation = state.generation();
                let id = row.id.clone();
                let entry = self.search.clone();
                button.connect_clicked(move |_| {
                    if model.borrow().generation() == generation
                        && model.borrow().rows.iter().any(|row| row.id == id)
                    {
                        model
                            .borrow_mut()
                            .set_search(&entry.text(), Some(id.clone()));
                    }
                });
                self.catalog.append(&button);
            }
        }
        let signature = format!(
            "{}:{}",
            state.generation(),
            state
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
                .join("\n")
        );
        if *self.search_signature.borrow() != signature {
            *self.search_signature.borrow_mut() = signature;
            while let Some(child) = self.hits.first_child() {
                self.hits.remove(&child);
            }
            for hit in &state.page.hits {
                let button = gtk::Button::new();
                button.add_css_class("flat");
                let text = label(&format!(
                    "{} · {}\n{}",
                    hit.row.title,
                    hit.source.label(),
                    hit.snippet
                ));
                button.set_child(Some(&text));
                let model = self.state.clone();
                let generation = state.generation();
                let hit = hit.clone();
                button.connect_clicked(move |_| {
                    if model.borrow().generation() == generation {
                        model.borrow_mut().open_hit(hit.clone());
                    }
                });
                self.hits.append(&button);
            }
        }
        let signature = format!(
            "{}:{:?}:{}:{:?}",
            state.generation(),
            state
                .document
                .as_ref()
                .map(|doc| (&doc.conversation, &doc.event_id, &doc.revision)),
            state.document_loading,
            state.notice
        );
        if *self.document_signature.borrow() != signature {
            *self.document_signature.borrow_mut() = signature;
            if let Some(document) = &state.document {
                self.document_status.set_text(&format!(
                    "Remote evidence · {} · revision {}{}",
                    document.source.label(),
                    document.revision,
                    if document.truncated {
                        " · partial indexed excerpt"
                    } else {
                        ""
                    }
                ));
                self.document.set_text(&document.text);
            } else {
                self.document_status.set_text(if state.document_loading {
                    "Reading the exact remote evidence revision…"
                } else {
                    "Select an exact history match to read its saved evidence."
                });
                self.document.set_text("");
            }
        }
    }
}
