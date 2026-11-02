//! Workspace routing keeps native/API drafts separate from the terminal PTY.
use super::*;
use workspace_navigation::{Action as WorkspaceAction, Row};

impl AppModel {
    pub(crate) fn show_terminal_workspace(&self) {
        self.tasks_panel_visible.set(false);
        self.ai_panel_visible.set(false);
        self.workspace_stack.set_visible_child_name("terminal");
    }

    pub(crate) fn workspace_action(
        &mut self,
        action: WorkspaceAction,
        sender: &ComponentSender<Self>,
    ) {
        match action {
            WorkspaceAction::Cli(id) => {
                if self.session_sidebar.contains(&id) {
                    self.session_sidebar.activate(&id);
                } else {
                    self.workspace_action(WorkspaceAction::Inspect(id), sender);
                    self.show_toast("The saved CLI source is not available in the current local catalog. Its indexed evidence remains read only; refresh Sessions to rediscover the source.");
                }
            }
            WorkspaceAction::PinCli(id, pinned) => {
                self.session_sidebar.organize(&id, Some(pinned), None)
            }
            WorkspaceAction::ArchiveCli(id, archived) => {
                self.session_sidebar.organize(&id, None, Some(archived))
            }
            WorkspaceAction::Api(id) => {
                self.show_ai_session_panel();
                self.ai_panel
                    .emit(dialogs::ai_panel::AiPanelMsg::SelectChat(id));
            }
            WorkspaceAction::SharedApi(id) => {
                self.show_ai_session_panel();
                self.workspace_stack.set_visible_child_name("chats");
                self.ai_panel
                    .emit(dialogs::ai_panel::AiPanelMsg::OpenShared(id));
            }
            WorkspaceAction::NewApi => {
                if !self.safe_mode && self.config.borrow().ai_enabled {
                    self.show_ai_session_panel();
                    self.ai_panel.emit(dialogs::ai_panel::AiPanelMsg::NewChat);
                } else {
                    self.show_toast("Enable AI in Settings to create an API conversation.");
                }
            }
            WorkspaceAction::OpenTask(conversation) => {
                if let Some(row) = self.workspace_navigation.lookup(&conversation) {
                    if let Some(id) = row
                        .facts
                        .task_id
                        .filter(|id| self.task_manager.get(*id).is_some())
                    {
                        self.workspace_action(WorkspaceAction::Task(id, conversation), sender);
                    } else {
                        self.workspace_action(WorkspaceAction::Inspect(conversation), sender);
                        self.show_toast("Task controls will appear after the host's task state has synchronized.");
                    }
                }
            }
            WorkspaceAction::Task(id, conversation) => {
                let terminal = self.agent_runtime.hosted_terminal(id).cloned();
                if let Some(session) = terminal {
                    self.open_hosted_native_terminal(session, sender);
                    return;
                }

                if let Some(task) = self.task_manager.get(id) {
                    self.tasks_panel
                        .emit(dialogs::tasks_panel::TasksPanelMsg::ClearDashboardFilters);
                    self.tasks_panel
                        .emit(dialogs::tasks_panel::TasksPanelMsg::ShowHidden(
                            task.status == agent_task::TaskStatus::Archived,
                        ));
                    self.tasks_panel_visible.set(true);
                    self.sync_side_panel();
                    self.execute_task_panel_action(
                        agent_task_ui::TaskPanelAction::Select(id),
                        sender,
                    );
                } else {
                    self.workspace_action(WorkspaceAction::Inspect(conversation), sender);
                }
            }
            WorkspaceAction::PinShared(id, pinned) => {
                self.ai_panel
                    .emit(dialogs::ai_panel::AiPanelMsg::SharedMutation(
                        jterm_core::agent_workspace::WorkspaceCommand::Pin { id, pinned },
                    ))
            }
            WorkspaceAction::ArchiveShared(id, archived) => {
                self.ai_panel
                    .emit(dialogs::ai_panel::AiPanelMsg::SharedMutation(
                        jterm_core::agent_workspace::WorkspaceCommand::Archive { id, archived },
                    ))
            }
            WorkspaceAction::ExternalTask(id) | WorkspaceAction::Inspect(id) => {
                if let Some(row) = self
                    .workspace_navigation
                    .lookup(&id)
                    .or_else(|| self.session_sidebar.shared_row(&id))
                {
                    if let Some(old) = self.workspace_stack.child_by_name("task-context") {
                        self.workspace_stack.remove(&old);
                    }
                    let page = self.workspace_navigation.inspect(&row);
                    self.workspace_stack.add_named(&page, Some("task-context"));
                    self.workspace_stack.set_visible_child_name("task-context");
                    page.grab_focus();
                }
            }
            WorkspaceAction::InspectContext(id) => {
                if let Some(row) = self
                    .workspace_navigation
                    .lookup(&id)
                    .or_else(|| self.session_sidebar.shared_row(&id))
                {
                    if let Some(old) = self.workspace_stack.child_by_name("task-context") {
                        self.workspace_stack.remove(&old);
                    }
                    let page = self.workspace_navigation.inspect_context(&row);
                    self.workspace_stack.add_named(&page, Some("task-context"));
                    self.workspace_stack.set_visible_child_name("task-context");
                    page.grab_focus();
                }
            }
            WorkspaceAction::ShowEvidence(id) => {
                if let Some(page) = self.workspace_navigation.evidence(id) {
                    if let Some(old) = self.workspace_stack.child_by_name("workspace-evidence") {
                        self.workspace_stack.remove(&old);
                    }
                    self.workspace_stack
                        .add_named(&page, Some("workspace-evidence"));
                    self.workspace_stack
                        .set_visible_child_name("workspace-evidence");
                    page.grab_focus();
                }
            }
            WorkspaceAction::AddEvidenceContext(id) => {
                if let Some(page) = self.workspace_navigation.evidence_context(id) {
                    if let Some(old) = self.workspace_stack.child_by_name("task-context") {
                        self.workspace_stack.remove(&old);
                    }
                    self.workspace_stack.add_named(&page, Some("task-context"));
                    self.workspace_stack.set_visible_child_name("task-context");
                    page.grab_focus();
                }
            }
            WorkspaceAction::PrefillTask {
                title,
                cwd,
                provider,
                body,
            } => {
                self.compose_handoff_task(title, cwd, provider, body, sender);
            }
            WorkspaceAction::PrefillApi { body } => {
                if !self.safe_mode && self.config.borrow().ai_enabled {
                    self.show_ai_session_panel();
                    self.ai_panel
                        .emit(dialogs::ai_panel::AiPanelMsg::NewDraft(body));
                } else {
                    self.show_toast("Enable AI in Settings before creating an API draft.");
                }
            }
            WorkspaceAction::NewTask => {
                self.execute_task_panel_action(
                    agent_task_ui::TaskPanelAction::ComposeProject,
                    sender,
                );
            }
            WorkspaceAction::NewTerminal => {
                self.show_terminal_workspace();
                let startup = self.config.borrow().startup_commands.clone();
                self.add_tab(startup, sender);
            }
            WorkspaceAction::RemoteWorkspace => crate::workspace_remote_ui::show(&self.window),
        }
        self.sync_workspace_navigation();
    }

    pub(crate) fn open_hosted_native_terminal(
        &mut self,
        session: jterm_core::agent_sessions::AgentSession,
        sender: &relm4::ComponentSender<crate::AppModel>,
    ) {
        match jterm_core::agent_workspace::managed_launch(&session)
            .and_then(|launch| Ok((jterm_core::agent_workspace::session_id(&session)?, launch)))
        {
            Ok((runtime_key, launch)) => {
                let (completion, _) = std::sync::mpsc::channel();
                self.add_agent_session_tab(
                    crate::agent_sessions::ResumeRequest {
                        runtime_key,
                        provider: session.provider,
                        id: session.id,
                        title: session.title,
                        launch: Some(launch),
                        completion,
                    },
                    sender,
                );
            }
            Err(error) => self.show_toast(error),
        }
    }

    pub(crate) fn sync_workspace_navigation(&self) {
        self.session_sidebar
            .set_existing(self.agent_session_tabs.keys().cloned().collect());
        let surface = self.workspace_stack.visible_child_name();
        self.session_sidebar
            .set_terminal_active(surface.as_deref() == Some("terminal"));
        let mut rows = self
            .ai_panel
            .model()
            .workspace_rows(surface.as_deref() == Some("chats"));
        rows.extend(self.session_sidebar.cli_rows().into_iter().map(|mut row| {
            row.active &= surface.as_deref() == Some("terminal");
            row
        }));
        let namespace = format!("anvil:{}", crate::config::config_file_path().display());
        let mut owned = std::collections::HashSet::new();
        for task in self.task_manager.tasks() {
            let Some(metadata) = self
                .agent_runtime
                .conversation_id(task.id)
                .cloned()
                .or_else(|| {
                    self.session_sidebar
                        .task_id(&namespace, &task.id.to_string())
                })
            else {
                continue;
            };
            owned.insert(metadata.clone());
            self.workspace_navigation.set_task_usage(
                metadata.clone(),
                self.agent_runtime
                    .shared_snapshot(task.id)
                    .as_deref()
                    .and_then(|snapshot| snapshot.usage.as_ref())
                    .map(|usage| usage.summary()),
            );
            let shared = self.session_sidebar.shared_row(&metadata);
            let activity = jterm_core::agent_task::DashboardActivity {
                has_agent_terminal: self.agent_runtime.hosted_terminal(task.id).is_some(),
                has_validation_terminal: self
                    .agent_runtime
                    .hosted_validation_terminal(task.id)
                    .is_some(),
                has_active_agent_stream: self.task_manager.has_active_agent_event_stream(task.id),
                native_preparing: self.agent_runtime.has_preparing(task.id),
            };
            let mut projection = self
                .agent_runtime
                .workspace_row(task.id)
                .unwrap_or_else(|| jterm_core::agent_workspace::WorkspaceRow {
                    id: metadata.clone(),
                    kind: jterm_core::agent_workspace::ConversationKind::Task,
                    title: task.title.clone(),
                    provider: task.provider.display_name().into(),
                    cwd: task.repo_root.to_string_lossy().into_owned(),
                    preview: format!("{} · {}", task.status.label(), task.branch),
                    updated_at_ms: task.updated_at_ms,
                    archived: task.status == agent_task::TaskStatus::Archived,
                    pinned: false,
                    tags: Vec::new(),
                    runtime: if activity.native_preparing {
                        jterm_core::agent_workspace::RuntimeState::Starting
                    } else if activity.has_agent_terminal
                        || activity.has_validation_terminal
                        || activity.has_active_agent_stream
                    {
                        jterm_core::agent_workspace::RuntimeState::Running
                    } else {
                        jterm_core::agent_workspace::RuntimeState::Idle
                    },
                    facts: jterm_core::agent_workspace::WorkspaceFacts::from_task(task, activity),
                });
            if let Some(shared) = &shared {
                projection.pinned = shared.pinned;
                projection.tags.clone_from(&shared.tags);
            }
            let presentation = projection.presentation();
            rows.push(Row {
                action: WorkspaceAction::Task(task.id, metadata),
                title: projection.title.clone(),
                detail: format!(
                    "{} · {}",
                    task.provider.display_name(),
                    presentation.status_label
                ),
                searchable: format!(
                    "{} {} {}",
                    task.repo_root.display(),
                    task.branch,
                    presentation.detail.as_deref().unwrap_or_default()
                ),
                active: self.selected_task == Some(task.id) && surface.as_deref() == Some("tasks"),
                busy: projection.runtime.is_live(),
                attention: presentation.attention.is_some(),
                archived: projection.archived,
                pinned: projection.pinned,
                project: projection.cwd.clone(),
                provider: projection.provider.clone(),
                updated: projection.updated_at_ms,
                source: Some(projection),
            });
        }
        rows.extend(self.session_sidebar.external_tasks(&owned));
        self.session_sidebar.set_projects(
            rows.iter()
                .filter(|row| !row.project.is_empty())
                .map(|row| std::path::PathBuf::from(&row.project))
                .collect(),
        );
        self.workspace_navigation
            .sync(rows, &self.session_sidebar.filter());
    }
}
