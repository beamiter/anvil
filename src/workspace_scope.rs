use jterm_core::agent_workspace::ConversationKind;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ConversationScope {
    #[default]
    All,
    Cli,
    Api,
    Task,
}

impl ConversationScope {
    pub const ALL: [Self; 4] = [Self::All, Self::Cli, Self::Api, Self::Task];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All types",
            Self::Cli => "CLI sessions",
            Self::Api => "API chats",
            Self::Task => "Tasks",
        }
    }

    pub fn includes(self, kind: ConversationKind) -> bool {
        matches!(
            (self, kind),
            (Self::All, _)
                | (Self::Cli, ConversationKind::Cli)
                | (Self::Api, ConversationKind::Api)
                | (Self::Task, ConversationKind::Task)
        )
    }

    pub fn qualify(self, query: &mut String) {
        match self {
            Self::All => {}
            Self::Cli => query.push_str(" kind:cli"),
            Self::Api => query.push_str(" kind:api"),
            Self::Task => query.push_str(" kind:task"),
        }
    }
}

impl std::fmt::Display for ConversationScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

pub(crate) fn project_choices(
    paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<_> = paths
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
}

pub(crate) fn conversation_summary(count: usize, filtered: bool, discovering: bool) -> String {
    match count {
        0 if discovering => "Discovering local conversations…".into(),
        0 if filtered => "No conversations match these filters".into(),
        0 => "No conversations yet".into(),
        1 => "1 conversation".into(),
        count => format!("{count} conversations"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jterm_core::agent_workspace::{ConversationId, RuntimeState, WorkspaceQuery, WorkspaceRow};

    #[test]
    fn project_choices_keep_same_named_projects_at_distinct_paths() {
        use std::path::PathBuf;
        let paths = ["", "/work/client/app", "/tmp/app", "/work/client/app"].map(PathBuf::from);
        assert_eq!(
            project_choices(paths),
            vec![PathBuf::from("/tmp/app"), PathBuf::from("/work/client/app")]
        );
    }

    #[test]
    fn kind_scope_qualifies_catalog_and_history_the_same_way() {
        for scope in ConversationScope::ALL {
            let mut query = "title:\"release notes\" -tag:draft".to_owned();
            scope.qualify(&mut query);
            let query = WorkspaceQuery::parse(&query).unwrap();
            for kind in [
                ConversationKind::Cli,
                ConversationKind::Api,
                ConversationKind::Task,
            ] {
                let row = WorkspaceRow {
                    id: ConversationId("test:one".into()),
                    kind,
                    title: "Release notes".into(),
                    provider: "Codex".into(),
                    cwd: "/work/project".into(),
                    preview: String::new(),
                    updated_at_ms: 1,
                    archived: false,
                    pinned: false,
                    tags: Vec::new(),
                    runtime: RuntimeState::Idle,
                    facts: Default::default(),
                };
                assert_eq!(query.matches(&row), scope.includes(kind));
                assert_eq!(
                    query.matches_document(&row, "historical text"),
                    scope.includes(kind)
                );
            }
        }
    }
}
