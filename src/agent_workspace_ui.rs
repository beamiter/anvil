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
            WorkspaceAction::Cli(id) => self.session_sidebar.activate(&id),
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
            WorkspaceAction::Task(id, _) => {
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
            WorkspaceAction::ExternalTask(id) => {
                if let Some(row) = self.session_sidebar.shared_row(&id) {
                    if let Some(old) = self.workspace_stack.child_by_name("task-context") {
                        self.workspace_stack.remove(&old);
                    }
                    let page = workspace_navigation::task_context(&row);
                    self.workspace_stack.add_named(&page, Some("task-context"));
                    self.workspace_stack.set_visible_child_name("task-context");
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
        }
        self.sync_workspace_navigation();
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
        let mut task_index = Vec::new();
        let mut owned = std::collections::HashSet::new();
        for task in self.task_manager.tasks() {
            let Some(metadata) = self
                .session_sidebar
                .task_id(&namespace, &task.id.to_string())
            else {
                continue;
            };
            owned.insert(metadata.clone());
            let shared = self.session_sidebar.shared_row(&metadata);
            task_index.push(jterm_core::agent_workspace::WorkspaceRow {
                id: metadata.clone(),
                kind: jterm_core::agent_workspace::ConversationKind::Task,
                title: task.title.clone(),
                provider: task.provider.display_name().into(),
                cwd: task.repo_root.to_string_lossy().into_owned(),
                preview: format!("{} · {}", task.status.label(), task.branch),
                updated_at_ms: task.updated_at_ms,
                archived: task.status == crate::agent_task::TaskStatus::Archived,
                pinned: false,
                tags: Vec::new(),
                runtime: jterm_core::agent_workspace::RuntimeState::Idle,
            });
            rows.push(Row {
                action: WorkspaceAction::Task(task.id, metadata),
                title: task.title.clone(),
                detail: format!("{} · {}", task.provider.display_name(), task.status.label()),
                searchable: format!("{} {}", task.repo_root.display(), task.branch),
                active: self.selected_task == Some(task.id) && surface.as_deref() == Some("tasks"),
                busy: task.status.is_running() || self.agent_runtime.has_preparing(task.id),
                attention: task.needs_attention(),
                archived: task.status == agent_task::TaskStatus::Archived,
                pinned: shared.is_some_and(|row| row.pinned),
                project: task.repo_root.to_string_lossy().into_owned(),
                provider: task.provider.display_name().into(),
                updated: task.updated_at_ms,
            });
        }
        self.session_sidebar.index_tasks(task_index);
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
