//! Additional native/API conversations in the persistent workspace sidebar.
//! Stable identities keep rows alive across stream updates; selection alone
//! never creates a process or sends a provider request.
use gtk::prelude::*;
use jterm_core::agent_task::TaskId;
use relm4::gtk;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Action {
    Api(u64),
    Cli(jterm_core::agent_workspace::ConversationId),
    PinCli(jterm_core::agent_workspace::ConversationId, bool),
    ArchiveCli(jterm_core::agent_workspace::ConversationId, bool),
    SharedApi(jterm_core::agent_workspace::ConversationId),
    Task(TaskId, jterm_core::agent_workspace::ConversationId),
    ExternalTask(jterm_core::agent_workspace::ConversationId),
    NewApi,
    NewTask,
    NewTerminal,
    RemoteWorkspace,
    Inspect(jterm_core::agent_workspace::ConversationId),
    InspectContext(jterm_core::agent_workspace::ConversationId),
    OpenTask(jterm_core::agent_workspace::ConversationId),
    ShowEvidence(u64),
    AddEvidenceContext(u64),
    PrefillTask {
        title: String,
        cwd: String,
        provider: String,
        body: String,
    },
    PrefillApi {
        body: String,
    },
    PinShared(jterm_core::agent_workspace::ConversationId, bool),
    ArchiveShared(jterm_core::agent_workspace::ConversationId, bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub action: Action,
    pub title: String,
    pub detail: String,
    pub searchable: String,
    pub active: bool,
    pub busy: bool,
    pub attention: bool,
    pub archived: bool,
    pub pinned: bool,
    pub project: String,
    pub provider: String,
    pub updated: u64,
    /// The host's exact facts. UI flags remain useful for an unsaved local
    /// draft, but must not replace provider capabilities or durable identity.
    pub source: Option<jterm_core::agent_workspace::WorkspaceRow>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct Filter {
    pub query: String,
    pub provider: Option<String>,
    pub needs_attention: bool,
    pub running: bool,
    pub pinned: bool,
    pub archive: u32,
    pub project: Option<std::path::PathBuf>,
}

impl Filter {
    fn history_query(&self) -> String {
        if self.query.trim().is_empty() {
            return String::new();
        }
        let quote =
            |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
        let mut parts = vec![self.query.clone()];
        if let Some(provider) = &self.provider {
            parts.push(format!("provider:{}", quote(provider)));
        }
        if let Some(project) = &self.project {
            parts.push(format!("project:{}", quote(&project.to_string_lossy())));
        }
        if self.needs_attention {
            parts.push("is:attention".into());
        }
        if self.running {
            parts.push("is:running".into());
        }
        if self.pinned {
            parts.push("is:pinned".into());
        }
        if self.archive == 1 {
            parts.push("is:archived".into());
        }
        parts.join(" ")
    }
    #[cfg(test)]
    fn matches(&self, row: &Row) -> bool {
        jterm_core::agent_workspace::WorkspaceQuery::parse(&self.query)
            .is_ok_and(|query| self.matches_scope(row, &query) && row.matches_query(&query))
    }

    fn matches_scope(
        &self,
        row: &Row,
        query: &jterm_core::agent_workspace::WorkspaceQuery,
    ) -> bool {
        if (self.archive == 0 && row.archived && !query.includes_archived())
            || (self.archive == 1 && !row.archived)
            || (self.needs_attention && !row.attention)
            || (self.running && !row.busy)
            || (self.pinned && !row.pinned)
            || self
                .provider
                .as_ref()
                .is_some_and(|provider| !row.provider.eq_ignore_ascii_case(provider))
            || self
                .project
                .as_ref()
                .is_some_and(|project| !std::path::Path::new(&row.project).starts_with(project))
        {
            return false;
        }
        true
    }
}

impl Row {
    fn identity(&self) -> RowIdentity {
        self.source
            .as_ref()
            .map(|row| &row.id)
            .or_else(|| self.shared_id())
            .map_or_else(
                || RowIdentity::Local(self.action.clone()),
                |id| RowIdentity::Shared(id.clone()),
            )
    }
    fn matches_query(&self, query: &jterm_core::agent_workspace::WorkspaceQuery) -> bool {
        self.source.as_ref().map_or_else(
            || query.matches(&self.search_row()),
            |row| query.matches(row),
        )
    }
    fn shared_id(&self) -> Option<&jterm_core::agent_workspace::ConversationId> {
        match &self.action {
            Action::Cli(id)
            | Action::SharedApi(id)
            | Action::Task(_, id)
            | Action::ExternalTask(id) => Some(id),
            _ => None,
        }
    }

    fn search_row(&self) -> jterm_core::agent_workspace::WorkspaceRow {
        use jterm_core::agent_workspace::{
            ConversationId, ConversationKind, RuntimeState, WorkspaceRow,
        };
        let mut row = self.source.clone().unwrap_or_else(|| WorkspaceRow {
            id: self
                .shared_id()
                .cloned()
                .unwrap_or_else(|| ConversationId(format!("local:{:?}", self.action))),
            kind: match self.action {
                Action::Cli(_) => ConversationKind::Cli,
                Action::Task(..) | Action::ExternalTask(_) => ConversationKind::Task,
                _ => ConversationKind::Api,
            },
            title: self.title.clone(),
            provider: self.provider.clone(),
            cwd: self.project.clone(),
            preview: self.searchable.clone(),
            updated_at_ms: self.updated,
            archived: self.archived,
            pinned: self.pinned,
            tags: Vec::new(),
            runtime: if self.busy {
                RuntimeState::Running
            } else {
                RuntimeState::Idle
            },
            facts: jterm_core::agent_workspace::WorkspaceFacts {
                attention: self
                    .attention
                    .then_some(jterm_core::agent_workspace::WorkspaceAttention::Failed),
                ..Default::default()
            },
        });
        // Local names and the actual selected project are presentation facts,
        // not permission to infer a live provider state from transcript text.
        row.title.clone_from(&self.title);
        row.archived = self.archived;
        row.pinned = self.pinned;
        row
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum RowIdentity {
    Shared(jterm_core::agent_workspace::ConversationId),
    Local(Action),
}

#[derive(Default)]
struct Interaction {
    pointer_inside: bool,
    pointer_down: bool,
    keyboard_until: Option<Instant>,
}

impl Interaction {
    fn freezes_order(&self) -> bool {
        self.pointer_inside
            || self.pointer_down
            || self
                .keyboard_until
                .is_some_and(|until| Instant::now() < until)
    }
}

struct RowWidgets {
    key: RowIdentity,
    widget: gtk::ListBoxRow,
    title: gtk::Label,
    detail: gtk::Label,
    pin: Option<(gtk::Button, Rc<RefCell<Action>>)>,
    archive: Option<(gtk::Button, Rc<RefCell<Action>>)>,
    popover: Option<gtk::Popover>,
}

#[derive(Clone)]
pub(crate) struct Navigation {
    pub root: gtk::Box,
    list: gtk::ListBox,
    rows: Rc<RefCell<Vec<Row>>>,
    all_rows: Rc<RefCell<Vec<Row>>>,
    widgets: Rc<RefCell<Vec<RowWidgets>>>,
    scroll: gtk::ScrolledWindow,
    summary: gtk::Label,
    notice: gtk::Label,
    next_attention: gtk::Button,
    reorder: gtk::Button,
    more: gtk::Button,
    visible_limit: Rc<Cell<usize>>,
    force_reorder: Rc<Cell<bool>>,
    interaction: Rc<RefCell<Interaction>>,
    last_filter: Rc<RefCell<Option<Filter>>>,
    requested_selection: Rc<RefCell<Option<RowIdentity>>>,
    search: super::workspace_search_ui::SearchSurface,
    scoped_search: Rc<RefCell<Option<(u64, super::workspace_search_ui::SearchSurface)>>>,
    next_surface: Rc<Cell<u64>>,
    context: super::workspace_context_ui::ContextSurface,
    review: super::workspace_review_ui::ReviewSurface,
    callback: Rc<dyn Fn(Action)>,
}

impl Navigation {
    pub fn new(callback: impl Fn(Action) + 'static) -> Self {
        let callback: Rc<dyn Fn(Action)> = Rc::new(callback);
        let context = super::workspace_context_ui::ContextSurface::new(callback.clone());
        let review =
            super::workspace_review_ui::ReviewSurface::new(callback.clone(), context.clone());
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let new = gtk::MenuButton::new();
        new.set_label("＋ New");
        new.set_tooltip_text(Some("Create an API conversation, agent task or terminal"));
        new.set_hexpand(true);
        let popover = gtk::Popover::new();
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 4);
        use jterm_core::agent_workspace::WorkspaceAction as SharedAction;
        for (label, action) in [
            (SharedAction::NewApiChat.label(), Action::NewApi),
            (SharedAction::NewTask.label(), Action::NewTask),
            (SharedAction::NewTerminal.label(), Action::NewTerminal),
            ("Remote workspace · read only", Action::RemoteWorkspace),
        ] {
            let button = gtk::Button::with_label(label);
            button.add_css_class("flat");
            let callback = callback.clone();
            let popover = popover.clone();
            button.connect_clicked(move |_| {
                popover.popdown();
                callback(action.clone());
            });
            choices.append(&button);
        }
        popover.set_child(Some(&choices));
        new.set_popover(Some(&popover));
        root.append(&new);
        let summary_line = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let summary = gtk::Label::new(None);
        summary.set_xalign(0.0);
        summary.set_hexpand(true);
        summary.set_wrap(true);
        summary.add_css_class("dim-label");
        let next_attention = gtk::Button::with_label(SharedAction::NextAttention.label());
        next_attention.add_css_class("flat");
        next_attention.set_tooltip_text(Some("Open the next visible conversation that needs your attention. No action is approved or sent."));
        summary_line.append(&summary);
        summary_line.append(&next_attention);
        root.append(&summary_line);
        let resources = gtk::Expander::new(Some("Workspace resources"));
        resources.set_expanded(false);
        resources.set_child(Some(&review.overview));
        root.append(&resources);
        let notice = gtk::Label::new(None);
        notice.set_xalign(0.0);
        notice.set_wrap(true);
        notice.add_css_class("error");
        notice.set_visible(false);
        root.append(&notice);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        list.add_css_class("navigation-sidebar");
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        scroll.set_propagate_natural_height(false);
        let search = super::workspace_search_ui::SearchSurface::new(0, callback.clone());
        search.root.set_visible(false);
        let scroll_body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        scroll_body.append(&list);
        scroll_body.append(&search.root);
        scroll.set_child(Some(&scroll_body));
        root.set_vexpand(true);
        root.append(&scroll);
        let rows = Rc::new(RefCell::new(Vec::<Row>::new()));
        let all_rows = Rc::new(RefCell::new(Vec::<Row>::new()));
        let interaction = Rc::new(RefCell::new(Interaction::default()));
        let visible_limit = Rc::new(Cell::new(120usize));
        let force_reorder = Rc::new(Cell::new(false));
        let requested_selection = Rc::new(RefCell::new(None));
        let footer = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let reorder = gtk::Button::with_label("Update conversation order");
        reorder.add_css_class("flat");
        reorder.set_tooltip_text(Some(
            "New activity is available. Keep your place until you choose to reorder.",
        ));
        reorder.set_visible(false);
        let force = force_reorder.clone();
        reorder.connect_clicked(move |_| force.set(true));
        let more = gtk::Button::with_label("Show more conversations");
        more.add_css_class("flat");
        more.set_visible(false);
        let limit = visible_limit.clone();
        more.connect_clicked(move |_| limit.set(limit.get().saturating_add(120)));
        footer.append(&reorder);
        footer.append(&more);
        root.append(&footer);
        {
            let all_rows = all_rows.clone();
            let visible_rows = rows.clone();
            let list = list.clone();
            let callback = callback.clone();
            let limit = visible_limit.clone();
            let requested = requested_selection.clone();
            next_attention.connect_clicked(move |_| {
                let current = list.selected_row().and_then(|row| {
                    visible_rows
                        .borrow()
                        .get(row.index() as usize)
                        .map(|row| row.action.clone())
                });
                let rows = all_rows.borrow();
                let current =
                    current.and_then(|action| rows.iter().position(|row| row.action == action));
                let start = current.map_or(0, |index| index + 1);
                let next = (start..rows.len())
                    .chain(0..start.min(rows.len()))
                    .find(|&index| rows[index].attention);
                if let Some(index) = next {
                    let action = rows[index].action.clone();
                    *requested.borrow_mut() = Some(rows[index].identity());
                    limit.set(limit.get().max(index + 1));
                    if let Some(visible) = visible_rows
                        .borrow()
                        .iter()
                        .position(|row| row.action == action)
                    {
                        list.select_row(list.row_at_index(visible as i32).as_ref());
                    }
                    drop(rows);
                    callback(action);
                }
            });
        }
        {
            let motion = gtk::EventControllerMotion::new();
            let entered = interaction.clone();
            motion.connect_enter(move |_, _, _| entered.borrow_mut().pointer_inside = true);
            let left = interaction.clone();
            motion.connect_leave(move |_| left.borrow_mut().pointer_inside = false);
            list.add_controller(motion);
            let click = gtk::GestureClick::new();
            click.set_button(0);
            click.set_propagation_phase(gtk::PropagationPhase::Capture);
            let pressed = interaction.clone();
            click.connect_pressed(move |_, _, _, _| pressed.borrow_mut().pointer_down = true);
            let released = interaction.clone();
            click.connect_released(move |_, _, _, _| {
                let mut state = released.borrow_mut();
                state.pointer_down = false;
                state.keyboard_until = Some(Instant::now() + Duration::from_millis(450));
            });
            let stopped = interaction.clone();
            click.connect_stopped(move |_| stopped.borrow_mut().pointer_down = false);
            list.add_controller(click);
        }
        let sections = rows.clone();
        list.set_header_func(move |row, previous| {
            let rows = sections.borrow();
            let Some(current) = rows.get(row.index() as usize) else {
                row.set_header(None::<&gtk::Widget>);
                return;
            };
            let same_section = previous
                .and_then(|previous| rows.get(previous.index() as usize))
                .is_some_and(|previous| previous.pinned == current.pinned);
            if same_section {
                row.set_header(None::<&gtk::Widget>);
            } else {
                let section =
                    gtk::Label::new(Some(if current.pinned { "PINNED" } else { "RECENT" }));
                section.set_xalign(0.0);
                section.set_margin_start(10);
                section.set_margin_top(10);
                section.set_margin_bottom(4);
                section.add_css_class("dim-label");
                row.set_header(Some(&section));
            }
        });
        let selected = rows.clone();
        let activate = callback.clone();
        list.connect_row_activated(move |_, row| {
            let action = selected
                .borrow()
                .get(row.index() as usize)
                .map(|row| row.action.clone());
            if let Some(action) = action {
                activate(action);
            }
        });
        // GtkListBox normally activates on Space. Navigation keys, including
        // Space, only browse; only a click or a distinct Enter opens a row.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let key_interaction = interaction.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            key_interaction.borrow_mut().keyboard_until =
                Some(Instant::now() + Duration::from_secs(3));
            if matches!(key, gtk::gdk::Key::space | gtk::gdk::Key::KP_Space) {
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });
        list.add_controller(keys);
        Self {
            root,
            list,
            rows,
            all_rows,
            widgets: Rc::new(RefCell::new(Vec::new())),
            scroll,
            summary,
            notice,
            next_attention,
            reorder,
            more,
            visible_limit,
            force_reorder,
            interaction,
            last_filter: Rc::new(RefCell::new(None)),
            requested_selection,
            search,
            scoped_search: Rc::new(RefCell::new(None)),
            next_surface: Rc::new(Cell::new(1)),
            context,
            review,
            callback,
        }
    }

    pub fn navigate(&self, key: gtk::gdk::Key) -> bool {
        self.interaction.borrow_mut().keyboard_until =
            Some(Instant::now() + Duration::from_secs(3));
        let count = self.rows.borrow().len();
        if count == 0 {
            return false;
        }
        let current = self.list.selected_row().map(|row| row.index() as usize);
        let next = match key {
            gtk::gdk::Key::Down => current.map_or(0, |i| (i + 1).min(count - 1)),
            gtk::gdk::Key::Up => current.map_or(0, |i| i.saturating_sub(1)),
            gtk::gdk::Key::Home => 0,
            gtk::gdk::Key::End => count - 1,
            gtk::gdk::Key::Page_Down => current.unwrap_or(0).saturating_add(10).min(count - 1),
            gtk::gdk::Key::Page_Up => current.unwrap_or(0).saturating_sub(10),
            _ => return false,
        };
        self.list
            .select_row(self.list.row_at_index(next as i32).as_ref());
        true
    }

    pub fn activate_selected(&self) {
        let action = self.list.selected_row().and_then(|row| {
            self.rows
                .borrow()
                .get(row.index() as usize)
                .map(|row| row.action.clone())
        });
        if let Some(action) = action {
            (self.callback)(action);
        }
    }

    pub fn sync(&self, mut rows: Vec<Row>, filter: &Filter) {
        self.context.poll();
        self.review
            .sync(rows.iter().filter_map(|row| row.source.clone()).collect());
        let catalog = self.review.catalog_rows();
        let catalog_by_id: HashMap<_, _> = catalog.iter().map(|row| (&row.id, row)).collect();
        for row in &mut rows {
            if let Some(source) = row
                .shared_id()
                .and_then(|id| catalog_by_id.get(id))
                .copied()
            {
                let presentation = source.presentation();
                row.title.clone_from(&source.title);
                row.detail = format!("{} · {}", source.provider, presentation.status_label);
                row.pinned = source.pinned;
                row.archived = source.archived;
                row.busy |= source.runtime.is_live();
                row.attention |= presentation.attention.is_some();
                row.source = Some(source.clone());
                row.updated = row.updated.max(source.updated_at_ms);
            }
            if row.archived {
                row.attention = false;
            }
        }
        let known: std::collections::HashSet<_> = rows
            .iter()
            .filter_map(|row| row.shared_id().cloned())
            .collect();
        for source in catalog.into_iter().filter(|row| !known.contains(&row.id)) {
            let presentation = source.presentation();
            let action = match source.kind {
                jterm_core::agent_workspace::ConversationKind::Api => {
                    Action::SharedApi(source.id.clone())
                }
                jterm_core::agent_workspace::ConversationKind::Cli => {
                    Action::Cli(source.id.clone())
                }
                jterm_core::agent_workspace::ConversationKind::Task => {
                    Action::ExternalTask(source.id.clone())
                }
            };
            rows.push(Row {
                action,
                title: source.title.clone(),
                detail: format!("{} · {}", source.provider, presentation.status_label),
                searchable: source.preview.clone(),
                active: false,
                busy: source.runtime.is_live(),
                attention: presentation.attention.is_some(),
                archived: source.archived,
                pinned: source.pinned,
                project: source.cwd.clone(),
                provider: source.provider.clone(),
                updated: source.updated_at_ms,
                source: Some(source),
            });
        }
        self.search
            .set_query(&filter.history_query(), None, filter.archive != 0);
        self.search.poll();
        if let Some((_, surface)) = self.scoped_search.borrow().as_ref() {
            surface.poll();
        }
        let filter_changed = self.last_filter.borrow().as_ref() != Some(filter);
        if filter_changed {
            self.visible_limit.set(120);
            *self.last_filter.borrow_mut() = Some(filter.clone());
        }
        let query = jterm_core::agent_workspace::WorkspaceQuery::parse(&filter.query);
        let error = query.as_ref().err().cloned();
        self.notice.set_text(error.as_deref().unwrap_or_default());
        self.notice.set_visible(error.is_some());
        let mut visible: Vec<Row> = if error.is_some() {
            Vec::new()
        } else {
            let query = query.as_ref().expect("a validated query has no error");
            rows.into_iter()
                .filter(|row| filter.matches_scope(row, query) && row.matches_query(query))
                .collect()
        };
        visible.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.updated.cmp(&a.updated))
                .then_with(|| format!("{:?}", a.action).cmp(&format!("{:?}", b.action)))
        });
        let total = visible.len();
        let all_attention = visible.iter().filter(|row| row.attention).count();
        let old_order: Vec<RowIdentity> = self.rows.borrow().iter().map(Row::identity).collect();
        let menu_open = self.widgets.borrow().iter().any(|row| {
            row.popover
                .as_ref()
                .is_some_and(|popover| popover.is_visible())
        });
        let force = self.force_reorder.replace(false);
        let freeze = !filter_changed
            && !force
            && (self.interaction.borrow().freezes_order()
                || menu_open
                || self.scroll.vadjustment().value() > 1.0);
        let order_changed = stable_reading_order(&mut visible, &old_order, freeze);
        self.reorder.set_visible(order_changed);
        *self.all_rows.borrow_mut() = visible.clone();
        if let Some(requested) = self.requested_selection.borrow().as_ref() {
            if let Some(index) = visible.iter().position(|row| &row.identity() == requested) {
                self.visible_limit
                    .set(self.visible_limit.get().max(index + 1));
            }
        }
        visible.truncate(self.visible_limit.get());
        self.summary.set_text(&if total == 0 {
            if filter.query.trim().is_empty() {
                "No conversations yet".into()
            } else {
                "No matching conversations".into()
            }
        } else if total > visible.len() {
            format!("{} of {total} conversations", visible.len())
        } else {
            format!("{total} conversations")
        });
        self.next_attention
            .set_label(&format!("Needs you · {all_attention}"));
        self.next_attention.set_visible(all_attention > 0);
        self.next_attention.set_sensitive(all_attention > 0);
        self.more.set_visible(total > visible.len());
        self.more.set_label(&format!(
            "Show more · {} remaining",
            total.saturating_sub(visible.len())
        ));

        let selected = self.requested_selection.borrow_mut().take().or_else(|| {
            self.list.selected_row().and_then(|selected| {
                self.widgets
                    .borrow()
                    .iter()
                    .find(|row| row.widget == selected)
                    .map(|row| row.key.clone())
            })
        });
        let old: Vec<RowWidgets> = self.widgets.borrow_mut().drain(..).collect();
        let mut by_key: HashMap<RowIdentity, RowWidgets> =
            old.into_iter().map(|row| (row.key.clone(), row)).collect();
        // Headers and row activation resolve the same exact order throughout
        // reconciliation; an index never becomes execution authority.
        let headers_changed = self
            .rows
            .borrow()
            .iter()
            .map(|row| (&row.action, row.pinned))
            .ne(visible.iter().map(|row| (&row.action, row.pinned)));
        *self.rows.borrow_mut() = visible.clone();
        let mut next = Vec::with_capacity(visible.len());
        for row in &visible {
            let widgets = by_key
                .remove(&row.identity())
                .unwrap_or_else(|| RowWidgets::new(row, self.callback.clone()));
            widgets.update(row);
            next.push(widgets);
        }
        for stale in by_key.into_values() {
            self.list.remove(&stale.widget);
        }
        for (index, row) in next.iter().enumerate() {
            if self.list.row_at_index(index as i32).as_ref() != Some(&row.widget) {
                if row.widget.parent().is_some() {
                    self.list.remove(&row.widget);
                }
                self.list.insert(&row.widget, index as i32);
            }
        }
        if let Some(selected) = selected {
            if let Some(row) = next.iter().find(|row| row.key == selected) {
                self.list.select_row(Some(&row.widget));
            } else {
                // A removed source must never silently select a replacement.
                self.list.unselect_all();
            }
        }
        *self.widgets.borrow_mut() = next;
        if headers_changed {
            self.list.invalidate_headers();
        }
    }

    pub fn evidence(&self, id: u64) -> Option<gtk::Box> {
        if id == 0 {
            return Some(self.search.evidence.clone());
        }
        self.scoped_search
            .borrow()
            .as_ref()
            .filter(|(surface, _)| *surface == id)
            .map(|(_, surface)| surface.evidence.clone())
    }

    pub fn inspect(&self, row: &jterm_core::agent_workspace::WorkspaceRow) -> gtk::Box {
        self.inspect_tab(row, "details")
    }

    pub fn inspect_context(&self, row: &jterm_core::agent_workspace::WorkspaceRow) -> gtk::Box {
        self.inspect_tab(row, "context")
    }

    pub fn set_task_usage(
        &self,
        id: jterm_core::agent_workspace::ConversationId,
        usage: Option<String>,
    ) {
        self.review.set_usage(id, usage);
    }

    pub fn lookup(
        &self,
        id: &jterm_core::agent_workspace::ConversationId,
    ) -> Option<jterm_core::agent_workspace::WorkspaceRow> {
        self.review.row(id).or_else(|| {
            // Exact search evidence remains navigable even when its source is
            // outside the bounded, currently loaded navigation catalog.
            self.search
                .selected_document()
                .filter(|(row, _)| &row.id == id)
                .map(|(row, _)| row)
                .or_else(|| {
                    self.scoped_search
                        .borrow()
                        .as_ref()
                        .and_then(|(_, search)| search.selected_document())
                        .filter(|(row, _)| &row.id == id)
                        .map(|(row, _)| row)
                })
        })
    }

    pub fn evidence_context(&self, id: u64) -> Option<gtk::Box> {
        let selected = if id == 0 {
            self.search.selected_document()
        } else {
            self.scoped_search
                .borrow()
                .as_ref()
                .filter(|(surface, _)| *surface == id)
                .and_then(|(_, surface)| surface.selected_document())
        }?;
        self.context.add_evidence(selected.1);
        Some(self.inspect_tab(&selected.0, "context"))
    }

    fn inspect_tab(&self, row: &jterm_core::agent_workspace::WorkspaceRow, tab: &str) -> gtk::Box {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_focusable(true);
        root.add_css_class("workspace-surface");
        if row.kind == jterm_core::agent_workspace::ConversationKind::Task
            && row.facts.source_sequence.is_none()
            && row.facts.task_id.is_some()
            && row.facts.task_status.is_some()
            && !self.review.task_indexed(&row.id)
        {
            self.context.deselect();
            self.review.clear_selection();
            let status = gtk::Label::new(Some(
                "Registering this task with the shared workspace… Context and queue controls become available after the host confirms its identity.",
            ));
            status.set_wrap(true);
            status.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            status.set_xalign(0.0);
            status.set_margin_start(16);
            status.set_margin_end(16);
            status.set_margin_top(16);
            root.append(&status);
            let pending = root.downgrade();
            let review = self.review.clone();
            let callback = self.callback.clone();
            let id = row.id.clone();
            let context_requested = tab == "context";
            gtk::glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
                let Some(root) = pending.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                if let Some(notice) = review.registration_notice() {
                    status.set_text(&format!("Task registration is pending. {notice}"));
                }
                if root.is_mapped() && review.task_indexed(&id) {
                    callback(if context_requested {
                        Action::InspectContext(id.clone())
                    } else {
                        Action::Inspect(id.clone())
                    });
                    return gtk::glib::ControlFlow::Break;
                }
                gtk::glib::ControlFlow::Continue
            });
            return root;
        }
        let stack = gtk::Stack::new();
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);
        stack.set_vexpand(true);
        let details = task_context(row);
        let refresh = gtk::Button::with_label("Refresh conversation details");
        refresh.set_halign(gtk::Align::Start);
        refresh.set_margin_start(16);
        let callback = self.callback.clone();
        let conversation = row.id.clone();
        refresh.connect_clicked(move |_| callback(Action::Inspect(conversation.clone())));
        details.prepend(&refresh);
        stack.add_titled(&details, Some("details"), "Details");
        self.context.select(row.id.clone());
        if let Some(parent) = self.context.root.parent().and_downcast::<gtk::Stack>() {
            parent.remove(&self.context.root);
        }
        stack.add_titled(&self.context.root, Some("context"), "Context");
        self.review.select(row);
        if row.kind == jterm_core::agent_workspace::ConversationKind::Task {
            for page in [&self.review.changes, &self.review.run] {
                if let Some(parent) = page.parent().and_downcast::<gtk::Stack>() {
                    parent.remove(page);
                }
            }
            stack.add_titled(&self.review.changes, Some("changes"), "Changes");
            stack.add_titled(&self.review.run, Some("run"), "Run");
        }
        let id = self.next_surface.get();
        self.next_surface.set(id.wrapping_add(1));
        let search = super::workspace_search_ui::SearchSurface::new(id, self.callback.clone());
        let history = gtk::Box::new(gtk::Orientation::Vertical, 10);
        history.set_margin_start(16);
        history.set_margin_end(16);
        history.set_margin_top(12);
        history.append(&search.search_entry(row.id.clone()));
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_vexpand(true);
        scroll.set_child(Some(&search.root));
        search.root.set_visible(false);
        history.append(&scroll);
        stack.add_titled(&history, Some("history"), "Search history");
        stack.set_visible_child_name(tab);
        // A combo remains reachable in a 9:16 window with all five pages.
        let pages: Vec<(&str, &str)> =
            if row.kind == jterm_core::agent_workspace::ConversationKind::Task {
                vec![
                    ("Details", "details"),
                    ("Context", "context"),
                    ("Changes", "changes"),
                    ("Run", "run"),
                    ("Search history", "history"),
                ]
            } else {
                vec![
                    ("Details", "details"),
                    ("Context", "context"),
                    ("Search history", "history"),
                ]
            };
        let selector =
            gtk::DropDown::from_strings(&pages.iter().map(|(label, _)| *label).collect::<Vec<_>>());
        selector.set_selected(pages.iter().position(|(_, name)| *name == tab).unwrap_or(0) as u32);
        selector.update_property(&[gtk::accessible::Property::Label(
            "Conversation inspector page",
        )]);
        let selected_stack = stack.clone();
        selector.connect_selected_notify(move |selector| {
            if let Some((_, name)) = pages.get(selector.selected() as usize) {
                selected_stack.set_visible_child_name(name);
            }
        });
        root.append(&selector);
        root.append(&stack);
        wrap_button_labels(&root);
        *self.scoped_search.borrow_mut() = Some((id, search));
        root
    }
}

pub(crate) fn format_timestamp_ms(timestamp: u64) -> String {
    if timestamp == 0 {
        return "time unavailable".into();
    }
    gtk::glib::DateTime::from_unix_local((timestamp / 1000).min(i64::MAX as u64) as i64)
        .and_then(|time| time.format("%Y-%m-%d %H:%M:%S %Z"))
        .map(|time| time.to_string())
        .unwrap_or_else(|_| "time unavailable".into())
}

/// Action captions may wrap in a portrait inspector instead of setting the
/// application window's minimum width to the longest label.
pub(crate) fn wrap_button_labels(root: &impl IsA<gtk::Widget>) {
    fn visit(widget: &gtk::Widget) {
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            if let Some(label) = button.child().and_downcast::<gtk::Label>() {
                label.set_wrap(true);
                label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            }
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            visit(&widget);
        }
    }
    visit(root.as_ref());
}

/// Preserve existing identities while the user is reading or interacting.
/// Newly discovered rows remain reachable, after the retained rows, until an
/// explicit refresh or an idle-at-top update applies the chronological order.
fn stable_reading_order(rows: &mut [Row], previous: &[RowIdentity], freeze: bool) -> bool {
    if !freeze {
        return false;
    }
    let sorted: Vec<_> = rows.iter().map(Row::identity).collect();
    let rank: HashMap<_, _> = previous
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, key)| (key, i))
        .collect();
    rows.sort_by_key(|row| rank.get(&row.identity()).copied().unwrap_or(usize::MAX));
    sorted.into_iter().ne(rows.iter().map(Row::identity))
}

impl RowWidgets {
    fn new(row: &Row, callback: Rc<dyn Fn(Action)>) -> Self {
        let widget = gtk::ListBoxRow::new();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        content.set_margin_top(7);
        content.set_margin_bottom(7);
        content.set_margin_start(10);
        content.set_margin_end(8);
        content.set_hexpand(true);
        let title = gtk::Label::new(None);
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let detail = gtk::Label::new(None);
        detail.set_xalign(0.0);
        detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        detail.add_css_class("dim-label");
        content.append(&title);
        content.append(&detail);
        let mut result = Self {
            key: row.identity(),
            widget,
            title,
            detail,
            pin: None,
            archive: None,
            popover: None,
        };
        if let Some(id) = row.shared_id() {
            let menu = gtk::MenuButton::new();
            menu.set_icon_name("view-more-symbolic");
            menu.add_css_class("flat");
            menu.set_tooltip_text(Some("Conversation actions"));
            let actions = gtk::Box::new(gtk::Orientation::Vertical, 4);
            let popover = gtk::Popover::new();
            for archive in [false, true] {
                if archive && matches!(row.action, Action::Task(..) | Action::ExternalTask(_)) {
                    continue;
                }
                let action = Rc::new(RefCell::new(organization_action(row, archive)));
                let command = action.clone();
                let button = gtk::Button::new();
                let callback = callback.clone();
                let popup = popover.clone();
                button.connect_clicked(move |_| {
                    let command = command.borrow().clone();
                    popup.popdown();
                    callback(command);
                });
                actions.append(&button);
                if archive {
                    result.archive = Some((button, action));
                } else {
                    result.pin = Some((button, action));
                }
            }
            let inspect = gtk::Button::with_label("Conversation details");
            let id = id.clone();
            let popup = popover.clone();
            inspect.connect_clicked(move |_| {
                popup.popdown();
                callback(Action::Inspect(id.clone()));
            });
            actions.append(&inspect);
            popover.set_child(Some(&actions));
            menu.set_popover(Some(&popover));
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            line.append(&content);
            line.append(&menu);
            result.widget.set_child(Some(&line));
            result.popover = Some(popover);
        } else {
            result.widget.set_child(Some(&content));
        }
        result
    }

    fn update(&self, row: &Row) {
        if self.title.text().as_str() != row.title {
            self.title.set_text(&row.title);
        }
        let status = if row.attention {
            "Needs you · "
        } else if row.busy {
            "Running · "
        } else {
            ""
        };
        let detail = format!("{status}{}", row.detail);
        if self.detail.text().as_str() != detail {
            self.detail.set_text(&detail);
        }
        let lifecycle = match row.source.as_ref().map(|row| (row.kind, row.presentation().capabilities.background_execution)) {
            Some((jterm_core::agent_workspace::ConversationKind::Cli, true)) => "Managed CLI: hiding this view leaves the process running; Ctrl+] detaches input.",
            Some((jterm_core::agent_workspace::ConversationKind::Cli, false)) => "CLI history: open the exact conversation to inspect its current runtime or resume availability.",
            Some((jterm_core::agent_workspace::ConversationKind::Api, _)) => "API: the session host owns an accepted request; closing a view does not send it again.",
            Some((jterm_core::agent_workspace::ConversationKind::Task, _)) => "Agent task: execution and validation belong to the session host; this is a view.",
            None => "Local conversation; waiting for shared-session registration.",
        };
        self.widget.set_tooltip_text(Some(&format!(
            "{}\n{}\n{}\n{lifecycle}",
            row.title, row.detail, row.searchable
        )));
        if row.active {
            self.widget.add_css_class("active-conversation");
        } else {
            self.widget.remove_css_class("active-conversation");
        }
        // A visible menu is a concrete action snapshot. A remote change must
        // not reverse Pin/Unpin between the user's press and release.
        if !self
            .popover
            .as_ref()
            .is_some_and(|popup| popup.is_visible())
        {
            if let Some((button, action)) = &self.pin {
                button.set_label(if row.pinned { "Unpin" } else { "Pin" });
                *action.borrow_mut() = organization_action(row, false);
            }
            if let Some((button, action)) = &self.archive {
                button.set_label(if row.archived { "Restore" } else { "Archive" });
                *action.borrow_mut() = organization_action(row, true);
            }
        }
    }
}

fn organization_action(row: &Row, archive: bool) -> Action {
    let id = row
        .shared_id()
        .expect("organization is only built for shared rows")
        .clone();
    match (&row.action, archive) {
        (Action::SharedApi(_), true) => Action::ArchiveShared(id, !row.archived),
        (Action::SharedApi(_), false) => Action::PinShared(id, !row.pinned),
        (_, true) => Action::ArchiveCli(id, !row.archived),
        (_, false) => Action::PinCli(id, !row.pinned),
    }
}

/// Inspect the host's real metadata and adapter capabilities without claiming
/// an execution lease or inventing an approval from old transcript text.
pub(crate) fn task_context(row: &jterm_core::agent_workspace::WorkspaceRow) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_hexpand(true);
    let presentation = row.presentation();
    let kind = match row.kind {
        jterm_core::agent_workspace::ConversationKind::Api => "API conversation",
        jterm_core::agent_workspace::ConversationKind::Cli => "CLI conversation",
        jterm_core::agent_workspace::ConversationKind::Task => "Agent task",
    };
    let lifecycle = if presentation.capabilities.background_execution {
        "Accepted work belongs to the session host. Hiding or closing this view does not resend a prompt. Use the conversation's Stop action to request cancellation."
    } else {
        "This catalog entry does not establish a background runtime. Open the conversation to inspect its current execution state."
    };
    let mut lines = vec![
        (row.title.clone(), true),
        (
            format!(
                "Catalog snapshot · updated {}{}",
                format_timestamp_ms(row.updated_at_ms),
                row.facts
                    .source_sequence
                    .map(|sequence| format!(" · source sequence {sequence}"))
                    .unwrap_or_default()
            ),
            false,
        ),
        (
            format!("{} · {kind}\n{}", row.provider, presentation.status_label),
            false,
        ),
        (lifecycle.into(), false),
    ];
    if let Some(reason) = &presentation.detail {
        lines.push((reason.clone(), false));
    }
    if !row.cwd.is_empty() {
        lines.push((format!("Project\n{}", row.cwd), false));
    }
    if let Some(branch) = &row.facts.branch {
        lines.push((format!("Branch\n{branch}"), false));
    }
    if let Some(validation) = presentation.validation_label {
        lines.push((format!("Validation\n{validation}"), false));
    }
    if !row.tags.is_empty() {
        lines.push((format!("Tags\n{}", row.tags.join(" · ")), false));
    }
    if !row.preview.is_empty() {
        lines.push((format!("Saved context\n{}", row.preview), false));
    }
    let capabilities = presentation.capabilities;
    if capabilities.known {
        let available: Vec<&str> = [
            (capabilities.terminal, "Terminal input"),
            (capabilities.follow_up, "Follow-up turns"),
            (capabilities.native_resume, "Native continuation"),
            (capabilities.terminal_handoff, "Continue in terminal"),
            (
                capabilities.structured_approval,
                "Structured permission requests",
            ),
            (capabilities.history_paging, "Earlier history"),
        ]
        .into_iter()
        .filter_map(|(available, label)| available.then_some(label))
        .collect();
        lines.push((
            format!(
                "Adapter capabilities\n{}",
                if available.is_empty() {
                    "No interactive continuation capability reported".into()
                } else {
                    available.join(" · ")
                }
            ),
            false,
        ));
    }
    for (text, heading) in lines {
        let label = gtk::Label::new(Some(&text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_selectable(true);
        if heading {
            label.add_css_class("title-1");
        }
        content.append(&label);
    }
    scroll.set_child(Some(&content));
    root.append(&scroll);
    root
}

/// Keep the existing task controls but give the selected agent the main canvas.
/// The full dashboard stays available in one bounded overview drawer.
pub(crate) fn compact_task_surface(root: &gtk::Box) {
    let mut children = Vec::new();
    let mut child = root.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        children.push(widget);
    }
    let Some((index, paned)) = children.iter().enumerate().find_map(|(index, widget)| {
        widget
            .clone()
            .downcast::<gtk::Paned>()
            .ok()
            .map(|paned| (index, paned))
    }) else {
        return;
    };
    let Some(detail) = paned.end_child() else {
        return;
    };
    let overview = gtk::Box::new(gtk::Orientation::Vertical, 4);
    for widget in children.iter().take(index).skip(2) {
        root.remove(widget);
        overview.append(widget);
    }
    if let Some(list) = paned.start_child() {
        paned.set_start_child(None::<&gtk::Widget>);
        list.set_vexpand(false);
        if let Some(scroll) = list.downcast_ref::<gtk::ScrolledWindow>() {
            scroll.set_min_content_height(110);
            scroll.set_max_content_height(160);
            scroll.set_propagate_natural_height(true);
        }
        overview.append(&list);
    }
    paned.set_end_child(None::<&gtk::Widget>);
    root.remove(&paned);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_max_content_height(260);
    scroll.set_propagate_natural_height(true);
    scroll.set_child(Some(&overview));
    let drawer = gtk::Expander::new(Some("Task overview and filters"));
    drawer.set_margin_start(8);
    drawer.set_margin_end(8);
    drawer.set_margin_top(6);
    drawer.set_margin_bottom(6);
    drawer.set_child(Some(&scroll));
    root.append(&drawer);
    detail.set_vexpand(true);
    root.append(&detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        Row {
            action: Action::Api(1),
            title: "Investigate renderer".into(),
            detail: "API conversation".into(),
            searchable: "GPU redraw latency".into(),
            active: false,
            busy: true,
            attention: true,
            archived: false,
            pinned: true,
            project: "/work/project/src".into(),
            provider: "API".into(),
            updated: 1,
            source: None,
        }
    }

    #[test]
    fn project_attention_archive_and_provider_filters_apply_to_every_source() {
        let row = row();
        assert!(Filter {
            query: "renderer is:pinned -provider:claude".into(),
            project: Some("/work/project".into()),
            needs_attention: true,
            running: true,
            ..Default::default()
        }
        .matches(&row));
        assert!(!Filter {
            project: Some("/work/project-other".into()),
            ..Default::default()
        }
        .matches(&row));
        assert!(!Filter {
            provider: Some("Codex".into()),
            ..Default::default()
        }
        .matches(&row));
        assert!(!Filter {
            archive: 1,
            ..Default::default()
        }
        .matches(&row));
        let mut archived = row;
        archived.archived = true;
        assert!(!Filter::default().matches(&archived));
        assert!(Filter {
            archive: 1,
            ..Default::default()
        }
        .matches(&archived));
    }

    #[test]
    fn workspace_queries_fail_closed_and_preserve_host_task_identity() {
        assert!(!Filter {
            query: "unrecognized:state".into(),
            ..Default::default()
        }
        .matches(&row()));
        assert!(!Filter {
            query: "title:\"unfinished".into(),
            ..Default::default()
        }
        .matches(&row()));
        let id = jterm_core::agent_workspace::ConversationId("fixture-task".into());
        let mut external = row();
        external.action = Action::ExternalTask(id.clone());
        let mut loaded = external.clone();
        loaded.action = Action::Task(TaskId::new(), id);
        assert_eq!(external.identity(), loaded.identity());
        assert_eq!(
            Filter {
                query: "title:\"Investigate renderer\"".into(),
                ..Default::default()
            }
            .history_query(),
            "title:\"Investigate renderer\""
        );
    }

    #[test]
    fn updates_keep_reading_order_until_explicit_reorder() {
        let rows: Vec<_> = (0..4)
            .map(|index| {
                let mut row = row();
                row.action = Action::Api(index);
                row.updated = index;
                row
            })
            .collect();
        let previous: Vec<_> = rows.iter().map(Row::identity).collect();
        let mut updated = vec![
            rows[3].clone(),
            rows[1].clone(),
            rows[0].clone(),
            rows[2].clone(),
        ];
        let mut new = row();
        new.action = Action::Api(9);
        updated.insert(0, new.clone());
        assert!(stable_reading_order(&mut updated, &previous, true));
        assert_eq!(
            updated.iter().map(Row::identity).collect::<Vec<_>>(),
            previous
                .into_iter()
                .chain([new.identity()])
                .collect::<Vec<_>>()
        );
        let mut chronological = vec![rows[3].clone(), rows[0].clone()];
        assert!(!stable_reading_order(&mut chronological, &[], false));
        assert_eq!(chronological[0].action, rows[3].action);
    }

    #[test]
    #[ignore = "requires an isolated JTERM_WORKSPACE_STATE_DIR and native GTK display"]
    fn workspace_v2_native_identity_attention_and_narrow_inspector() {
        assert!(
            std::env::var_os("JTERM_WORKSPACE_STATE_DIR").is_some(),
            "run this fixture with isolated workspace state"
        );
        gtk::init().expect("GTK display");
        let actions = Rc::new(RefCell::new(Vec::new()));
        let sink = actions.clone();
        let navigation = Navigation::new(move |action| sink.borrow_mut().push(action));
        let window = gtk::Window::new();
        window.set_default_size(360, 640);
        window.set_child(Some(&navigation.root));
        window.present();
        let mut rows: Vec<_> = (0..180)
            .map(|index| {
                let mut row = row();
                row.action = Action::Api(index);
                row.title = format!("Duplicate task {}", index % 2);
                row.pinned = false;
                row.attention = index == 160;
                row.updated = 180 - index;
                row
            })
            .collect();
        navigation.sync(rows.clone(), &Filter::default());
        assert_eq!(navigation.rows.borrow().len(), 120);
        let first = navigation.widgets.borrow()[0].widget.clone();
        let selected = navigation.widgets.borrow()[4].widget.clone();
        navigation.list.select_row(Some(&selected));
        navigation.interaction.borrow_mut().pointer_down = true;
        rows[90].updated = 999;
        rows[0].detail = "Streaming output changed".into();
        navigation.sync(rows.clone(), &Filter::default());
        assert_eq!(
            navigation.widgets.borrow()[0].widget,
            first,
            "stream updates replaced a native row"
        );
        assert_eq!(navigation.list.selected_row(), Some(selected.clone()));
        assert!(navigation.reorder.is_visible());
        assert!(
            actions.borrow().is_empty(),
            "refresh or focus triggered a conversation action"
        );
        navigation.interaction.borrow_mut().pointer_down = false;
        navigation.next_attention.emit_clicked();
        assert_eq!(actions.borrow().as_slice(), &[Action::Api(160)]);
        navigation.sync(rows.clone(), &Filter::default());
        assert!(navigation.visible_limit.get() >= 161);
        let selected_index = navigation.list.selected_row().unwrap().index() as usize;
        assert_eq!(
            navigation.rows.borrow()[selected_index].action,
            Action::Api(160),
            "Needs you did not locate an off-page conversation"
        );
        rows.retain(|row| row.action != Action::Api(160));
        navigation.sync(rows, &Filter::default());
        assert!(
            navigation.list.selected_row().is_none(),
            "deleted identity silently selected another conversation"
        );
        assert_eq!(actions.borrow().len(), 1);
        let fixture = row().search_row();
        let inspector = navigation.inspect(&fixture);
        window.set_child(Some(&inspector));
        let context = gtk::glib::MainContext::default();
        for _ in 0..30 {
            if context.pending() {
                context.iteration(false);
            }
        }
        let (minimum, _, _, _) = inspector.measure(gtk::Orientation::Horizontal, -1);
        assert!(
            minimum <= 360,
            "inspector required {minimum}px in a 9:16 window"
        );
        if let Some(path) = std::env::var_os("JTERM_WORKSPACE_NATIVE_SCREENSHOT") {
            assert!(std::process::Command::new("import")
                .args(["-window", "root"])
                .arg(path)
                .status()
                .unwrap()
                .success());
        }
        window.close();
    }
}
