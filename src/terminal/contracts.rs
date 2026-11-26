//! Pane launch, command and event contracts shared by terminal backends.
use crate::config::Config;
use crate::search::SearchStatus;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Commands to submit after a terminal reaches its first prompt.
///
/// Configuration retains its historical comma-separated syntax, but it is
/// parsed once at the application boundary. Session restore instead constructs
/// exactly one safely quoted command from a persisted argv. Downstream terminal
/// backends therefore never reinterpret a restored command's commas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitialCommands(pub(super) Vec<String>);

impl InitialCommands {
    pub(crate) fn from_config(configured: Option<&str>) -> Self {
        let commands = configured
            .into_iter()
            .flat_map(|value| value.split(", "))
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_string)
            .collect();
        Self(commands)
    }

    pub(crate) fn from_restored_argv(argv: Option<&[String]>, shell_argv: &[String]) -> Self {
        let command = argv.and_then(|argv| crate::process::shell_quote_argv_for(argv, shell_argv));
        if argv.is_some() && command.is_none() {
            log::warn!(
                "Skipping session command replay because its argv is unsafe or the configured shell grammar is unsupported"
            );
        }
        Self(command.into_iter().collect())
    }

    pub(crate) fn as_slice(&self) -> &[String] {
        &self.0
    }
}

pub struct TerminalInit {
    pub config: Rc<RefCell<Config>>,
    /// Backend requested by the pane creator. Conventional VTE ignores this;
    /// the shared Block component passes it through to `TermView` so Unified
    /// is never accidentally re-derived from mutable global configuration.
    pub mode: crate::config::TerminalMode,
    pub shell_argv: Rc<Vec<String>>,
    pub working_directory: Option<String>,
    pub working_directory_external: bool,
    pub session_id: Option<String>,
    pub(crate) history: crate::block_view::PaneHistoryInit,
    /// Per-pane secret consumed by the shell integration to authenticate OSC 7
    /// cwd updates even when a Flatpak host shell hides foreground processes.
    pub cwd_token: String,
    pub initial_commands: InitialCommands,
    pub probe: PaneProbe,
    /// Extra `KEY=value` pairs overlaid on the frozen child environment.
    /// Empty for ordinary shells; task terminals use it to pin validation
    /// children to no-rc startup files.
    pub env_extra: Vec<(String, String)>,
}

/// Shared, cheaply-clonable handle exposing a pane's shell pid and PTY master fd
/// to the app, so it can probe the foreground process (for restorable-command
/// detection and close-confirmation) without a synchronous round-trip into the
/// backend component. Both fields default to -1/0 until the shell is spawned.
#[derive(Clone, Default)]
pub struct PaneProbe {
    pub shell_pid: Rc<Cell<i32>>,
    pub pty_fd: Rc<Cell<i32>>,
}

#[derive(Debug)]
pub enum TerminalInput {
    WriteInput(Vec<u8>),
    /// Validated insert-only UI text; claim any held confirming key at write time.
    InsertReviewText(Vec<u8>),
    /// Text inserted the way a clipboard paste is: bracketed when the
    /// foreground program enabled bracketed paste (Block mode). A dropped
    /// file's quoted path arrives this way; claim held Enter at the write boundary.
    PasteText(String),
    /// Block-mode only: atomically re-check a clean prompt, arm the local
    /// Agent execution identity, and submit the reviewed command.
    RunAgentCommand {
        execution: crate::agent::AgentExecutionRef,
        command: String,
    },
    Resize(u16, u16),
    GrabFocus,
    Copy,
    /// Block-view only: when a finished block is selected, copy its output
    /// only (Warp's Alt+Ctrl+Shift+C). Falls back to a regular Copy elsewhere.
    CopyOutputOnly,
    Paste,
    SetFontScale(f64),
    SetFont(String),
    SetScrollback(i64),
    ScrollLines(i32),
    ApplyTheme,
    /// Refresh backend-owned behavioral configuration from the shared app value.
    /// VTE panes already hold that shared `Rc`; Block panes copy it internally.
    SyncConfig,
    Kill,
    /// Block-view only: show only failed / only slow / only pinned / all blocks.
    FilterFailedBlocks,
    FilterSlowBlocks,
    FilterPinnedBlocks,
    ClearBlockFilter,
    /// Block-view only: select all completed blocks.
    SelectAllBlocks,
    /// Block-view only: remove all completed blocks from the pane.
    ClearBlocks,
    /// Block-view only: restore the blocks removed by the last ClearBlocks.
    UndoClearBlocks,
    /// Block-view only: delete the currently selected finished cards.
    DeleteSelectedBlocks,
    /// Block-view only: fold / unfold every finished block's output at once.
    CollapseAllBlocks,
    ExpandAllBlocks,
    CollapseSelectedBlocks,
    ExpandSelectedBlocks,
    /// Block-view only: fold or unfold the selected block (or the newest one).
    ToggleBlockCollapsed,
    /// Block-view only: put all selected commands back into the input editor.
    ReinputSelectedCommands,
    /// Block-view only: jump to the previous / next pinned block.
    JumpToPrevPinned,
    JumpToNextPinned,
    /// Block-view only: jump to the previous / next failed block.
    JumpToPrevFailed,
    JumpToNextFailed,
    PreviousCommand,
    NextCommand,
    ReturnToLive,
    CopyCurrentCommand,
    CompareCommandOutput,
    /// Block-view only: write the whole session's blocks to a Markdown / JSON
    /// file under the anvil data directory.
    ExportSessionMarkdown,
    ExportSessionJson,
    /// Search: set the query and jump to the first match. `use_regex` treats the
    /// query as a regex; otherwise it is matched literally. `case_sensitive`
    /// is off by default (`Aa` in the find bar).
    SearchSet(String, bool, bool),
    SearchNext,
    SearchPrev,
    SearchClear,
    /// Block-view only: open the flat ripgrep-style search over completed blocks.
    CrossBlockSearch,
    /// Block-view only: snapshot the selected finished block and forward it to
    /// the application AI panel. Plain VTE panes ignore this action.
    AskAiAboutSelectedBlock,
}

/// What a [`TerminalOutput::NoticeWithUndo`] button takes back, in the pane that
/// raised it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeUndo {
    ClearBlocks,
}

#[derive(Debug)]
pub enum TerminalOutput {
    /// The backend's child process was created successfully. Conventional VTE
    /// spawning is asynchronous, so split transactions use this acknowledgement
    /// to retire their rollback authority.
    Launched,
    CwdChanged {
        path: String,
        external: bool,
    },
    LaunchFailed(String),
    Exited(i32),
    Bell,
    TitleChanged(String),
    Activity,
    Focused,
    /// A command finished. `true` = success, `false` = failure. Emitted by
    /// BlockTerminal; the application decides whether an inactive tab needs an
    /// activity or attention marker.
    CommandFinished(bool),
    /// Remote shell announced its session id via OSC 7770. Carries the id so
    /// the parent app can store it on the tab's RemoteConn for resume-on-reconnect.
    RemoteSessionId(String),
    /// User-facing feedback for a backend action (e.g. undo-clear, export)
    /// that the application surfaces as a toast.
    Notice(String),
    /// A [`TerminalOutput::Notice`] whose action can be taken back. The application
    /// puts `button` on the toast and sends `undo` to the pane that emitted
    /// this — never to whichever pane happens to be focused when the button is
    /// clicked, which a toast outliving a tab switch otherwise would be.
    NoticeWithUndo {
        message: String,
        button: String,
        undo: NoticeUndo,
    },
    /// Current find-in-terminal result. Both backends emit the same state so
    /// the window search bar never has to infer a count or regex failure.
    SearchStatus(SearchStatus),
    /// Open Find for the emitting pane, without relying on focus event timing.
    FindRequested,
    /// Native workspace menu requests the existing application command palette.
    CommandPaletteRequested,
    /// A finished block, with the full reconstructed command + exit + a
    /// captured-output sample. Drives agent-mode's run-observe loop. Emitted
    /// by BlockTerminal only (the plain VTE wrapper has no block concept).
    BlockFinished {
        command: String,
        exit_code: i32,
        /// How this block came to be closed. `exit_code` above is an i32 with
        /// a sentinel and cannot say whether the shell reported anything, so
        /// consumers that read the output as evidence about *this* command —
        /// command correction — gate on this instead.
        completion_provenance: jterm_core::block_contract::CompletionProvenance,
        /// Bytes sampled head+tail by the caller to a small bound — the
        /// agent already truncates to its own cap, but block.rs trims first
        /// so we don't ship 256 KB across a relm4 channel.
        output_sample: String,
        /// One-shot identity armed locally before the reviewed PTY write.
        agent_execution: Option<crate::agent::AgentExecutionRef>,
        /// Wall-clock duration of the block, when one was recorded. Feeds the
        /// bottom bar's last-command segment.
        duration_ms: Option<u64>,
    },
    /// Approval advanced the protocol, but the pane could no longer arm/write
    /// that exact execution. The Agent integration must fail closed.
    AgentExecutionStartFailed {
        execution: crate::agent::AgentExecutionRef,
    },
    AskAiAboutBlock(crate::ai::BlockContext, crate::ai::BlockAiIntent),
    /// The failed-block menu's Fix action. Carries no payload: the app
    /// re-derives authoritative evidence from the emitting pane's live
    /// selection and owns every task gate.
    FixBlockWithAgent,
}

/// Keyboard Copy with nothing selected. The context menu greys out Copy, but
/// Ctrl+Shift+C still fires and used to look like a broken clipboard.
pub(crate) fn copy_empty_notice(has_copyable: bool) -> Option<&'static str> {
    (!has_copyable).then_some("Nothing selected to copy.")
}

/// Ctrl+Shift+Q / Ask AI about selected block with no finished card selected
/// (live prompt, empty pane). Used to look like a dead shortcut.
pub(crate) fn ask_ai_empty_notice() -> &'static str {
    "Select a finished block to ask AI about."
}
