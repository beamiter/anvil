//! Top-level Relm4 application messages.
//!
//! Keeping the event vocabulary separate from the component implementation
//! makes the update loop easier to navigate without changing Relm4 ownership.

use crate::keybindings::Action;

#[derive(Debug, Clone)]
pub(crate) enum AppMsg {
    NewTab,
    CloseTab(u64),
    ForceCloseTab(u64),
    ForceClosePane(u64),
    ForceCloseMarked(Vec<u64>),
    SelectTab(u64),
    NextTab,
    PrevTab,
    ToggleSidebar,
    MinimizeWindow,
    ToggleMaximizedWindow,
    WindowMaximized(bool),
    /// GTK toplevel activation changed. The app loop owns organism presence so
    /// delayed pane-focus messages cannot reclaim a body after deactivation.
    WindowActive(bool),
    Quit,
    ForceQuit,
    Toast(String),
    CopyOutputOnly,
    Action(Action),
    /// Result of the background "is a newer jsh published?" check. Boxed so one
    /// rare message does not widen every other variant.
    JshUpdateChecked(Box<jterm_core::jsh_install::Status>),
    /// Completion of the single-flight workflow cache refresh. Discovery and
    /// parsing happen off the GTK thread; the app loop owns the cache swap and
    /// any user-facing error.
    WorkflowRefreshFinished(Result<Vec<crate::workflows::Workflow>, String>),
    ReloadConfig,
    PaneLaunched(u64),
    PaneLaunchFailed(u64, String),
    PaneExited(u64, u64, i32),
    PaneCwdChanged(u64, u64, String, bool),
    PaneRemoteSessionId(u64, String),
    RemoteReconnectTick(u64, u64),
    RemoteReconnectNow(u64, u32),
    PaneFocused(u64, u64),
    /// Local files dropped on one concrete pane. The model validates that all
    /// are supported images before inserting their quoted paths without Enter.
    ImageFilesDropped {
        pane_id: u64,
        paths: Vec<std::path::PathBuf>,
    },
    /// A pane header was dropped onto another pane: exchange their positions
    /// in the split tree.
    SwapPanes {
        dragged: u64,
        target: u64,
    },
    /// Move the sole pane owned by an ordinary tab beside a target pane.
    /// Both identities remain stable while tab and pane indices may shift.
    MoveTabToPane {
        tab_id: u64,
        target_pane_id: u64,
        edge: crate::pane_header::PaneDropEdge,
    },
    /// Track a tab drag so a hover-previewed page can be restored when the
    /// source remains a tab (cancel, invalid drop, or ordinary reorder).
    TabDragStarted {
        source_tab_id: u64,
        drag_id: u64,
    },
    TabDragEnded {
        source_tab_id: u64,
        drag_id: u64,
    },
    /// Select a stable target after the pointer rests over its row long enough
    /// to expose that page's pane drop zones.
    PreviewTabDrop {
        source_tab_id: u64,
        target_tab_id: u64,
        drag_id: u64,
        hover_generation: u64,
    },
    /// Detach one pane from a split and promote it to an ordinary tab. A row
    /// drop supplies a stable anchor; blank tab-bar space leaves it absent.
    PromotePaneToTab {
        pane_id: u64,
        anchor_tab_id: Option<u64>,
        after: bool,
    },
    /// Periodic refresh of the split panes' status headers (cwd and the
    /// running command are polled, not pushed).
    RefreshPaneHeaders,
    TitleChanged(u64, String),
    Bell(u64),
    Activity(u64),
    SettingsTheme(usize),
    SettingsFontDesc(String),
    SettingsFontScale(f64),
    /// Debounced write of a font scale already applied by the hotkey or
    /// Ctrl+wheel path.
    PersistFontScale,
    SettingsOpacity(f64),
    SettingsScrollback(u32),
    SettingsTerminalMode(usize),
    SettingsBlockCompact(bool),
    SettingsCommandHistory(bool),
    SettingsAsciiOrganism(bool),
    SettingsAsciiOrganismMotion(u32),
    SettingsAiEnabled(bool),
    SettingsAiPanelVisible(bool),
    SettingsAiPanelWidth(u32),
    SettingsAgentEnabled(bool),
    SettingsCommandCorrection(bool),
    SettingsAiProvider(usize),
    SettingsAiModel(String),
    SettingsAiBaseUrl(String),
    SettingsAiKeyFile(String),
    SettingsAiMaxTokens(u32),
    SettingsAiRedactSecrets(bool),
    SettingsAiStream(bool),
    SettingsAgentMaxTurns(u32),
    SettingsNotifications(bool),
    SettingsRemoteClipboard(bool),
    SettingsRemoteHosts(Vec<crate::config::RemoteHost>),
    SearchChanged(String),
    SearchNext,
    SearchPrev,
    SearchClose,
    SearchStatus(u64, crate::search::SearchStatus),
    SetTabWidth(u32),
    RenameTab(u64, String),
    ReorderTab(u64, usize),
    TabRowAction(u64, crate::tab_strip::TabAction),
    SetTabFilter(String),
    FileTreeActivateFile(std::path::PathBuf),
    /// Header location selector moved: 0 is Local, i > 0 is
    /// `config.remote_hosts[i - 1]`.
    FileTreeSelectLocation(usize),
    /// The background `start_dir` probe for a location switch answered.
    FileTreeLocationResolved {
        loc: crate::remote_fs::FsLocation,
        start: Result<std::path::PathBuf, String>,
    },
    /// Context-menu requests; `dir: None` targets the current tree root.
    FileTreeNewFile {
        dir: Option<std::path::PathBuf>,
    },
    FileTreeNewFolder {
        dir: Option<std::path::PathBuf>,
    },
    FileTreeRename {
        path: std::path::PathBuf,
    },
    FileTreeDelete {
        path: std::path::PathBuf,
    },
    FileTreeCopy {
        path: std::path::PathBuf,
        is_dir: bool,
    },
    FileTreeCut {
        path: std::path::PathBuf,
        is_dir: bool,
    },
    FileTreePaste {
        dir: Option<std::path::PathBuf>,
    },
    /// OS file-manager drop onto the tree; `dir: None` targets the root.
    FileTreeImportPaths {
        paths: Vec<std::path::PathBuf>,
        dir: Option<std::path::PathBuf>,
    },
    FileTreeRefresh,
    /// A background op or transfer finished; refresh these directories in
    /// place, preserving all other expansion.
    FileTreeOpSucceeded(Vec<std::path::PathBuf>),
    /// Dialog results, names already validated against `remote_fs` rules.
    FileTreeCreateNamed {
        dir: std::path::PathBuf,
        name: String,
        is_dir: bool,
    },
    FileTreeRenameNamed {
        src: std::path::PathBuf,
        name: String,
    },
    FileTreeDeleteConfirmed(std::path::PathBuf),
    OpenNotebook(std::path::PathBuf),
    OpenAgent,
    OpenAgentSettings,
    AgentSend(String),
    AgentContinue,
    AgentNewTask,
    AgentStopRequest,
    AgentRetryRequest,
    AgentAttachContext,
    AgentClearContext,
    AgentReject(crate::agent::AgentProposalRef),
    AgentEditAndApprove(crate::agent::AgentProposalRef, String),
    AgentInsert(crate::agent::AgentProposalRef, String),
    AgentRefreshPrompt(crate::agent::AgentSessionEpoch),
    AgentLlmReply {
        epoch: crate::agent::AgentSessionEpoch,
        reply: Result<String, String>,
    },
    AgentBlockFinished {
        tab_id: u64,
        pane_id: u64,
        command: String,
        exit_code: i32,
        output_sample: String,
        agent_execution: Option<crate::agent::AgentExecutionRef>,
        duration_ms: Option<u64>,
    },
    AgentExecutionStartFailed {
        execution: crate::agent::AgentExecutionRef,
    },
    AgentClose,
    FileTreeGotoCwd,
    FileTreeGoUp,
    SetSidebarView(crate::config::SidebarView),
    PaletteTypeCommand(String),
    PaletteAskAi(String),
    PaletteSuggestionReply {
        generation: u64,
        request_id: u64,
        reply: Result<String, String>,
    },
    PaletteSuggestionStop(u64),
    PaletteSuggestionRetry(u64),
    PaletteSuggestionInsert(u64),
    PaletteSuggestionDismiss(u64),
    CommandCorrectionLocalReply {
        pane_id: u64,
        generation: u64,
        candidate: Option<crate::command_correction::CorrectionCandidate>,
    },
    CommandCorrectionAiReply {
        pane_id: u64,
        generation: u64,
        reply: Result<String, String>,
    },
    CommandCorrectionAccept {
        pane_id: u64,
        generation: u64,
    },
    CommandCorrectionTimeout {
        pane_id: u64,
        generation: u64,
    },
    CommandCorrectionDismiss {
        pane_id: u64,
        generation: u64,
    },
    OpenAiPanel,
    AskAiAboutBlock(crate::ai::BlockContext),
    AiConversationSnapshot(String),
    AiPanelCloseRequested,
    AiPanelWidthChanged(u32),
    PersistAiPanelWidth(u64),
    PaletteRunWorkflow(std::path::PathBuf),
    Ignore,
}
