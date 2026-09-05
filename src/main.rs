#![allow(dead_code)]

mod action_ops;
mod agent;
mod agent_ops;
mod agent_task;
mod agent_task_ui;
mod ai;
mod ai_palette_ops;
mod app_msg;
mod block_view;
mod bottom_bar_ui;
mod cli;
mod command_correction;
mod command_review;
mod config;
mod config_ops;
mod config_store;
mod diagnostics;
mod dialogs;
mod file_tree;
mod file_tree_ops;
mod git_meta_ui;
mod image_drop;
use jterm_core::{child_env, command_history, notify, parser, pty_input, review_input};

mod host {
    pub use jterm_core::host::*;

    pub const APP_ID: &str = "io.github.beamiter.anvil";
    /// Also the `TERM_PROGRAM` every spawn path reports, via
    /// `jterm_core::child_env`. The shell-integration snippets gate on this exact
    /// string (`[[ $TERM_PROGRAM == anvil ]] && source …`), so changing it
    /// silently disables OSC 133 block detection.
    pub const APP_NAME: &str = "anvil";
}

mod jsh_ops;
mod keybindings;
mod navigation_ui;
mod notebook;
mod organism_ui;
mod palette;
mod pane_header;
mod persistence;
mod process;
mod pty;
mod remote_fs;
mod review_input_ops;
mod review_text;
mod search;
mod session;
mod settings_ops;
mod sidebar;
mod sidebar_toggle;
mod startup_ui;
mod tab_strip;
mod task_ops;
mod terminal;
mod top_bar;
mod vte_pty;
mod workflow_ops;
mod workflows;
mod workspace;
mod workspace_ops;

use adw::prelude::*;
use gtk::gio::{self, Cancellable};
use gtk::glib;
use relm4::adw;
use relm4::factory::FactoryVecDeque;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use app_msg::AppMsg;
use config::{choose_shell_argv, config_file_path, load_config, Config, TerminalMode, Theme};
use keybindings::{chord_from_gdk, Action, Direction, KeybindingMap};
use terminal::{
    default_tab_title, BlockTerminal, InitialCommands, VteInit, VteInput, VteOutput, VteTerminal,
};
use workspace::{ConnStatus, Pane, RemoteConn, Tab, TermCtl, ZoomState};

const FONT_STEP: f64 = 0.025;
const OPACITY_STEP: f64 = 0.025;
/// Quiet period after the last font-scale step before the config is written.
const FONT_PERSIST_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);
const MIN_TAB_WIDTH: u32 = 80;
const MAX_TAB_WIDTH: u32 = 480;
const MIN_AI_PANEL_WIDTH: u32 = 240;
const MAX_AI_PANEL_WIDTH: u32 = 1_200;
const MIN_AI_WORKSPACE_WIDTH: i32 = 200;

fn restored_ai_panel_position(total_width: i32, requested_width: u32) -> Option<i32> {
    if total_width <= MIN_AI_PANEL_WIDTH as i32 + MIN_AI_WORKSPACE_WIDTH {
        return None;
    }
    let available = total_width - MIN_AI_WORKSPACE_WIDTH;
    let panel_width = (requested_width as i32).clamp(MIN_AI_PANEL_WIDTH as i32, available);
    Some(total_width - panel_width)
}

fn ai_panel_width_from_geometry(total_width: i32, position: i32) -> Option<u32> {
    if total_width <= 0 || position < 0 || position >= total_width {
        return None;
    }
    Some(
        (total_width - position).clamp(MIN_AI_PANEL_WIDTH as i32, MAX_AI_PANEL_WIDTH as i32) as u32,
    )
}

fn widget_is_within(mut current: gtk::Widget, ancestor: &gtk::Widget) -> bool {
    loop {
        if current == *ancestor {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        current = parent;
    }
}

fn file_tree_f5_should_refresh(
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
    focus_within_tree: bool,
    pointer_within_tree: bool,
    tree_mapped: bool,
) -> bool {
    let command_modifiers = gtk::gdk::ModifierType::CONTROL_MASK
        | gtk::gdk::ModifierType::SHIFT_MASK
        | gtk::gdk::ModifierType::ALT_MASK
        | gtk::gdk::ModifierType::SUPER_MASK
        | gtk::gdk::ModifierType::HYPER_MASK
        | gtk::gdk::ModifierType::META_MASK;
    key == gtk::gdk::Key::F5
        && !state.intersects(command_modifiers)
        && tree_mapped
        && (focus_within_tree || pointer_within_tree)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileTreeNavigationShortcut {
    Back,
    Forward,
    Up,
    Home,
    OpenPath,
}

fn file_tree_navigation_shortcut(
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
    focus_within_tree: bool,
    _pointer_within_tree: bool,
    tree_mapped: bool,
) -> Option<FileTreeNavigationShortcut> {
    if !tree_mapped || !focus_within_tree {
        return None;
    }
    let command_state = state - gtk::gdk::ModifierType::LOCK_MASK;
    if matches!(key, gtk::gdk::Key::l | gtk::gdk::Key::L)
        && command_state == gtk::gdk::ModifierType::CONTROL_MASK
    {
        return Some(FileTreeNavigationShortcut::OpenPath);
    }
    let conflicting = gtk::gdk::ModifierType::CONTROL_MASK
        | gtk::gdk::ModifierType::SHIFT_MASK
        | gtk::gdk::ModifierType::SUPER_MASK
        | gtk::gdk::ModifierType::HYPER_MASK
        | gtk::gdk::ModifierType::META_MASK;
    if !state.contains(gtk::gdk::ModifierType::ALT_MASK) || state.intersects(conflicting) {
        return None;
    }
    match key {
        gtk::gdk::Key::Left => Some(FileTreeNavigationShortcut::Back),
        gtk::gdk::Key::Right => Some(FileTreeNavigationShortcut::Forward),
        gtk::gdk::Key::Up => Some(FileTreeNavigationShortcut::Up),
        gtk::gdk::Key::Home => Some(FileTreeNavigationShortcut::Home),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CrossBlockSearchKeyPress {
    Proceed,
    DispatchToggle,
    SuppressHeldRepeat,
}

/// Window-capture state for the physical key that opened Block Search.
///
/// `AdwDialog` is a widget presented inside this same window, so repeats from
/// the opening press still cross the window controller before reaching the
/// dialog. Remember the hardware keycode here, before the asynchronous
/// `AppMsg::Action` opens the dialog: subsequent press edges are repeats even
/// if the user releases a modifier and changes their keysym/chord mid-hold.
#[derive(Debug, Default)]
struct CrossBlockSearchKeyLatch {
    held_keycodes: HashSet<u32>,
}

impl CrossBlockSearchKeyLatch {
    fn press(&mut self, keycode: u32, is_toggle: bool) -> CrossBlockSearchKeyPress {
        if self.held_keycodes.contains(&keycode) {
            return CrossBlockSearchKeyPress::SuppressHeldRepeat;
        }
        if is_toggle {
            self.held_keycodes.insert(keycode);
            CrossBlockSearchKeyPress::DispatchToggle
        } else {
            CrossBlockSearchKeyPress::Proceed
        }
    }

    fn release(&mut self, keycode: u32) {
        self.held_keycodes.remove(&keycode);
    }

    fn reset(&mut self) {
        self.held_keycodes.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrganismFocusDecision {
    ClaimCurrentPane,
    Revoke,
}

/// Relm's last observed activation and GTK's current toplevel state must both
/// agree before a pane may own the spatial organism. Checking both closes the
/// gap where a pane-focus message was queued just before the window lost focus.
fn organism_focus_decision(
    observed_window_active: bool,
    current_window_active: bool,
) -> OrganismFocusDecision {
    if observed_window_active && current_window_active {
        OrganismFocusDecision::ClaimCurrentPane
    } else {
        OrganismFocusDecision::Revoke
    }
}

/// A workspace mutation needs an explicit two-phase organism handoff whenever
/// it changes pane identity or temporarily hides/reparents the current pane.
/// The latter matters for moves where the same stable pane id remains selected
/// but its old tab/container disappears underneath it.
fn organism_focus_transfer_required(
    previous_pane: Option<u64>,
    next_pane: Option<u64>,
    hides_previous: bool,
) -> bool {
    hides_previous || previous_pane != next_pane
}

// `file_tree_store: gtk::TreeStore` uses the GTK4 TreeStore family deprecated in
// 4.10; it stays functional and a ColumnView rewrite is out of scope.
#[allow(deprecated)]
struct AppModel {
    config: Rc<RefCell<Config>>,
    /// Shared native-organism state; Relm4 remains responsible for resolving
    /// which Block component owns focus and for attaching new panes.
    organism_hub: Rc<organism_ui::OrganismHub>,
    /// Last activation state delivered through the Relm4 update loop. GTK's
    /// live state is checked alongside it before any presence claim.
    window_active: bool,
    config_revision: RefCell<Option<config_store::ConfigRevision>>,
    themes: Rc<Vec<Theme>>,
    kbmap: Rc<RefCell<KeybindingMap>>,
    shell_argv: Rc<Vec<String>>,
    tabs: Vec<Tab>,
    active: usize,
    /// Stable source/original-active identities for a tab drag. A delayed
    /// hover preview is reverted if the source still exists when drag ends.
    tab_drag_origin: Option<(u64, u64, u64)>,
    tab_drag_coordinator: Rc<tab_strip::TabDragCoordinator>,
    next_id: u64,
    next_pane_id: u64,
    /// Conventional VTE split spawns complete asynchronously. Until their
    /// success acknowledgement arrives, the new stable pane id owns rollback
    /// authority back to `(source tab id, source pane id)`.
    pending_split_spawns: std::collections::HashMap<u64, (u64, u64)>,
    sidebar_visible: bool,
    font_scale: f64,
    /// Generation token for the debounced font-scale config write. Ctrl+wheel
    /// emits a step per notch, so only the last step in a burst reaches disk.
    font_persist_generation: Rc<std::cell::Cell<u64>>,
    window_opacity: f64,
    stack: gtk::Stack,
    tab_strip: gtk::Box,
    tab_rows: FactoryVecDeque<tab_strip::TabRow>,
    /// Second, always-vertical tab list living in the sidebar's "tabs" page.
    /// `tab_strip` is a single widget that gets reparented into whichever
    /// holder the placement names, so it cannot be in the top bar and the
    /// sidebar at once; this mirror is what keeps the sidebar tab list
    /// reachable while the strip is docked to the top bar.
    sidebar_tab_strip: gtk::Box,
    sidebar_tab_rows: FactoryVecDeque<tab_strip::TabRow>,
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    /// The live opacity-hotkey toast, if one is currently shown. Held so rapid
    /// Ctrl+Alt+=/- presses update one toast in place instead of queueing a
    /// separate toast per step.
    opacity_toast: Rc<RefCell<Option<adw::Toast>>>,
    quit_allowed: Rc<std::cell::Cell<bool>>,
    session_persistence: bool,
    /// Last user-visible background-save warning per operation. Worker errors
    /// remain logged immediately; this map prevents a failing mount from
    /// queueing one toast every second while autosave continues.
    persistence_failure_notices: std::collections::HashMap<String, std::time::Instant>,
    safe_mode: bool,
    dyn_css: gtk::CssProvider,
    search: Controller<search::SearchModel>,
    tab_filter_control: Controller<sidebar::TabFilterModel>,
    tab_filter: String,
    file_tree_store: gtk::TreeStore,
    file_header: Controller<sidebar::FileHeaderModel>,
    file_tree_root: Rc<RefCell<std::path::PathBuf>>,
    file_tree_scan_generation: Rc<std::cell::Cell<u64>>,
    /// Loaded-model revision captured by visible file-tree actions. A
    /// successful reconciliation that changes rows revokes stale menus/dialogs
    /// without cancelling already-dispatched filesystem settlement.
    file_tree_content_revision: Rc<std::cell::Cell<u64>>,
    /// Per-path completion time and explicit invalidation for loaded directory
    /// snapshots in the current tree authority.
    file_tree_snapshots: Rc<RefCell<file_tree::DirectorySnapshots>>,
    /// Latest staged root/location navigation. Navigation lists before it
    /// commits, so failures leave the live authority, rows, and selection.
    file_tree_navigation_revision: Rc<std::cell::Cell<u64>>,
    file_tree_navigation_cancellation: Rc<RefCell<Option<file_tree::ScanCancellation>>>,
    file_tree_navigation_history: Rc<RefCell<file_tree::FileTreeNavigationHistory>>,
    file_tree_root_cache: Rc<RefCell<file_tree::RootListingCache>>,
    file_tree_failure_gate: Rc<RefCell<file_tree::DirectoryFailureGate>>,
    /// Per-directory publication revisions for in-place refreshes. A later
    /// request for one path supersedes every earlier result for that path,
    /// even while the overall tree generation stays unchanged.
    file_tree_refresh_revisions: Rc<RefCell<file_tree::DirectoryRefreshRevisions>>,
    /// The tree view, its filter model, and the live filter state behind the
    /// header's filter entry.
    file_tree_view: gtk::TreeView,
    file_tree_filter_model: gtk::TreeModelFilter,
    file_tree_filter: Rc<RefCell<file_tree::TreeFilter>>,
    file_tree_status: Rc<file_tree::FileTreeStatusUi>,
    file_tree_pointer_inside: Rc<std::cell::Cell<bool>>,
    /// Which filesystem the tree browses; drives both scans and file ops.
    file_tree_location: Rc<RefCell<remote_fs::FsLocation>>,
    /// The active pane's last process-observed SSH intent. A worker may
    /// publish only while its token, pane, process target, and captured tree
    /// authority all remain current.
    file_tree_ssh_observation: Option<file_tree::SshFileTreeObservation>,
    file_tree_ssh_detection_revision: std::cell::Cell<u64>,
    /// Monotonic user interaction token. A remote-follow probe captured
    /// before a file action or explicit sidebar choice must never replace
    /// that newer intent.
    file_tree_user_operation_revision: std::cell::Cell<u64>,
    /// Copy/Cut row awaiting a Paste; usable only in its source location.
    file_tree_clipboard: Rc<RefCell<Option<remote_fs::FsClipboard>>>,
    /// Monotonic identity of user Copy/Cut intent. Async completions may retire
    /// only the exact intent they captured, even when a newer one has the same
    /// paths and mode.
    file_tree_clipboard_revision: std::cell::Cell<u64>,
    /// The live busy toast of an in-flight cross-location transfer, held so
    /// long transfers are not left without any indication.
    file_tree_transfer_toast: Rc<RefCell<Option<adw::Toast>>>,
    /// Monotonic identity of the most recently started file transfer. A late
    /// progress/completion event cannot publish over a newer transfer's UI.
    file_tree_transfer_revision: Rc<std::cell::Cell<u64>>,
    tab_strip_scroll: gtk::ScrolledWindow,
    sidebar_tab_scroll: gtk::ScrolledWindow,
    top_tab_scroll: gtk::ScrolledWindow,
    top_bar: Controller<top_bar::TopBarModel>,
    sidebar_box: gtk::Box,
    content_paned: gtk::Paned,
    ai_paned: gtk::Paned,
    /// Family-wide bottom status bar (`jterm_core::bottom_bar`): the container
    /// plus its left/right segment holders.
    bottom_bar: gtk::Box,
    bottom_bar_left: gtk::Box,
    bottom_bar_right: gtk::Box,
    /// Last composed content. The one-second status tick is intentionally a
    /// no-op when none of the visible segments changed.
    bottom_bar_content: Rc<RefCell<jterm_core::bottom_bar::Content>>,
    sidebar_stack: gtk::Stack,
    sidebar_toggle: Controller<sidebar_toggle::SidebarToggleModel>,
    tab_placement: std::cell::Cell<config::TabPlacement>,
    sidebar_view: std::cell::Cell<config::SidebarView>,
    command_palette: Controller<dialogs::command_palette::PaletteModel>,
    settings: Controller<dialogs::settings::SettingsModel>,
    settings_font_names: Rc<Vec<String>>,
    remote_picker: Controller<dialogs::remote_picker::RemotePickerModel>,
    debug_dashboard: Controller<dialogs::debug_dashboard::DebugDashboardModel>,
    workflow_dialog: Controller<dialogs::workflow::WorkflowModel>,
    ai_panel: Controller<dialogs::ai_panel::AiPanelModel>,
    ai_panel_visible: std::cell::Cell<bool>,
    ai_panel_width_generation: Rc<std::cell::Cell<u64>>,
    /// Right-side stack holding the AI Chats panel and the agent Tasks panel;
    /// it is the `ai_paned` end child, so both share the persisted width.
    side_stack: gtk::Stack,
    tasks_panel: Controller<dialogs::tasks_panel::TasksPanelModel>,
    tasks_panel_visible: std::cell::Cell<bool>,
    /// Native Codex agent task domain: the reducer, the app-server runtime,
    /// and the single-flight diff worker.
    task_manager: crate::agent_task::TaskManager,
    agent_runtime: crate::agent_task::AgentRuntimeManager,
    agent_diff: crate::agent_task::AgentDiffPanel,
    selected_task: Option<crate::agent_task::TaskId>,
    pending_task_creation: Option<crate::agent_task_ui::PendingTaskCreation>,
    /// Validation cwd pins retained between tab spawn and PaneLaunched so the
    /// child enters the worktree through the validated descriptor.
    pending_validation_pins:
        std::collections::HashMap<u64, crate::agent_task::PreparedTaskValidation>,
    agent_tasks_timer_armed: std::cell::Cell<bool>,
    ai_conversation: Option<String>,
    /// One pane-bound natural-language command draft. Keeping the request
    /// handle here makes Stop/Dismiss real transport cancellation.
    command_suggestion: Rc<RefCell<Option<ai_palette_ops::CommandSuggestionSession>>>,
    command_suggestion_generation: Rc<std::cell::Cell<u64>>,
    /// Per-pane review-first correction requests/cards. Stable generations
    /// prevent a late local/AI result from replacing a newer failure.
    command_corrections:
        Rc<RefCell<std::collections::HashMap<u64, command_correction::CorrectionSession>>>,
    command_correction_generation: Rc<std::cell::Cell<u64>>,
    notebook: Controller<notebook::NotebookModel>,
    /// Workflows loaded from disk. Refreshed on demand each time the palette
    /// is opened, with one background prewarm at startup. The palette always
    /// presents immediately from the last completed snapshot.
    workflows: Rc<RefCell<Vec<workflows::Workflow>>>,
    workflow_refresh: workflow_ops::WorkflowRefreshState,
    /// Workflow files the last completed scan refused, so a refusal is
    /// announced when it appears rather than on every palette open. anvil
    /// gained `O_NOFOLLOW` with the shared loader, and a symlinked file it
    /// used to read must not just quietly stop being a workflow.
    workflow_refusals: Vec<std::path::PathBuf>,
    /// At most one agent session is active per app. Opening the panel
    /// while another session is alive cancels the previous one.
    active_agent: Rc<RefCell<Option<agent::AgentSession>>>,
    /// UI-lifetime identity for the inline card. Unlike the protocol epoch it
    /// deliberately survives New Task, but changes when the card is replaced.
    agent_panel_generation: Rc<std::cell::Cell<u64>>,
    agent_panel: Controller<agent::AgentPanelModel>,
}

#[allow(clippy::too_many_arguments)]
fn create_pane(
    config: &Rc<RefCell<Config>>,
    organism_hub: &Rc<organism_ui::OrganismHub>,
    shell_argv: &Rc<Vec<String>>,
    tab_id: u64,
    pane_id: u64,
    mode: TerminalMode,
    initial_commands: InitialCommands,
    working_directory: Option<String>,
    session_id: Option<String>,
    cwd_external: bool,
    env_extra: Vec<(String, String)>,
    sender: &ComponentSender<AppModel>,
) -> Pane {
    // A restored or remote pane arrives with the identity it must keep, so jsh
    // reopens the same session. Every other pane mints one here rather than
    // launching without: `--session` is what makes jsh announce `session_id=`
    // on OSC 133 `C`, and without that slot no pane can ever satisfy
    // `ExecutionLifecycle::from_command_meta` and open a journal Output slot.
    // Doing it in the constructor rather than at each of the seven call sites
    // is deliberate — a new pane path added later inherits the identity instead
    // of silently rejoining the set that journals nothing.
    let session_id = Some(config::pane_session_id(session_id, pane_id));
    let probe = terminal::PaneProbe::default();
    // -1 means "no PTY yet"; foreground probing skips it (0 would alias stdin).
    probe.pty_fd.set(-1);
    let cwd_token = terminal::new_cwd_token();
    let init = VteInit {
        config: config.clone(),
        mode,
        shell_argv: shell_argv.clone(),
        working_directory: working_directory.clone(),
        working_directory_external: cwd_external,
        session_id: session_id.clone(),
        cwd_token,
        initial_commands,
        probe: probe.clone(),
        env_extra,
    };
    let forward = move |out| match out {
        VteOutput::Launched => AppMsg::PaneLaunched(pane_id),
        VteOutput::LaunchFailed(message) => AppMsg::PaneLaunchFailed(pane_id, message),
        VteOutput::Exited(code) => AppMsg::PaneExited(tab_id, pane_id, code),
        VteOutput::CwdChanged { path, external } => {
            AppMsg::PaneCwdChanged(tab_id, pane_id, path, external)
        }
        // Pane identity is stable across detach/move; tab identity is not.
        VteOutput::TitleChanged(t) => AppMsg::TitleChanged(pane_id, t),
        VteOutput::Bell => AppMsg::Bell(pane_id),
        VteOutput::Activity => AppMsg::Activity(pane_id),
        VteOutput::Focused => AppMsg::PaneFocused(tab_id, pane_id),
        // A command completed in this pane. Inactive tabs show failures with
        // the bell style and successes with the lighter activity style.
        VteOutput::CommandFinished(true) => AppMsg::Activity(pane_id),
        VteOutput::CommandFinished(false) => AppMsg::Bell(pane_id),
        VteOutput::RemoteSessionId(id) => AppMsg::PaneRemoteSessionId(pane_id, id),
        VteOutput::Notice(message) => AppMsg::Toast(message),
        VteOutput::NoticeWithUndo {
            message,
            button,
            undo,
        } => AppMsg::ToastWithUndo {
            pane_id,
            message,
            button,
            undo,
        },
        VteOutput::SearchStatus(status) => AppMsg::SearchStatus(pane_id, status),
        VteOutput::BlockFinished {
            command,
            exit_code,
            completion_provenance,
            output_sample,
            agent_execution,
            duration_ms,
        } => AppMsg::AgentBlockFinished {
            tab_id,
            pane_id,
            command,
            exit_code,
            completion_provenance,
            output_sample,
            agent_execution,
            duration_ms,
        },
        VteOutput::AgentExecutionStartFailed { execution } => {
            AppMsg::AgentExecutionStartFailed { execution }
        }
        VteOutput::AskAiAboutBlock(context, intent) => AppMsg::AskAiAboutBlock(context, intent),
        VteOutput::FixBlockWithAgent => AppMsg::FixBlockWithAgent(pane_id),
    };
    let terminal = match mode {
        TerminalMode::Block | TerminalMode::Unified => {
            let controller = BlockTerminal::builder()
                .launch(init)
                .forward(sender.input_sender(), forward);
            if let Some(view) = controller.model().term_view() {
                organism_hub.attach_ascii_organism_to_view(&view, cwd_external);
            }
            TermCtl::Block(controller)
        }
        TerminalMode::Vte => TermCtl::Vte(
            VteTerminal::builder()
                .launch(init)
                .forward(sender.input_sender(), forward),
        ),
    };
    let frame = pane_header::PaneFrame::new(&terminal.widget());
    // The header is this pane's drag handle and its frame is a typed workspace
    // drop zone. Ids, not indices, cross the drag boundary: pane and tab
    // positions may both shift while the gesture is in flight.
    {
        let sender = sender.clone();
        frame.install_drag_and_drop(pane_id, move |dragged, edge| match dragged {
            pane_header::WorkspaceDragItem::Pane(dragged) => {
                if dragged == pane_id {
                    return false;
                }
                sender.input(AppMsg::SwapPanes {
                    dragged,
                    target: pane_id,
                });
                true
            }
            pane_header::WorkspaceDragItem::Tab(tab_id) => {
                let Some(edge) = edge else {
                    return false;
                };
                sender.input(AppMsg::MoveTabToPane {
                    tab_id,
                    target_pane_id: pane_id,
                    edge,
                });
                true
            }
        });
    }
    {
        let target = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY,
        );
        let sender = sender.clone();
        target.connect_drop(move |_, value, _x, _y| {
            let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                return false;
            };
            let paths = files
                .files()
                .into_iter()
                .filter_map(|file| file.path())
                .collect::<Vec<_>>();
            if paths.is_empty() {
                return false;
            }
            sender.input(AppMsg::ImageFilesDropped { pane_id, paths });
            true
        });
        frame.widget().add_controller(target);
    }
    Pane {
        terminal,
        frame,
        id: pane_id,
        title: None,
        cwd: working_directory,
        cwd_external,
        session_id,
        mode,
        probe,
        last_exit: None,
        last_duration_ms: None,
        task_role: None,
        task_session_id: None,
    }
}

impl AppModel {
    fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn active_terminal(&self) -> Option<&TermCtl> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.panes.get(t.active_pane))
            .map(|p| &p.terminal)
    }

    fn active_pane_id(&self) -> Option<u64> {
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.panes.get(tab.active_pane))
            .map(|pane| pane.id)
    }

    /// Phase one of a workspace focus handoff. Revocation happens before GTK
    /// hides/reparents a tab or before the model changes `active_pane`, so a
    /// dormant timeout can never keep drawing into the old hidden surface.
    fn begin_organism_focus_transfer(&self, next_pane: Option<u64>, hides_previous: bool) -> bool {
        let transfer =
            organism_focus_transfer_required(self.active_pane_id(), next_pane, hides_previous);
        if transfer {
            // Any queued process-observed SSH result belonged to the previous
            // focus epoch. Advancing its token here also closes the
            // pane-A -> pane-B -> pane-A ABA window before model selection
            // changes or GTK queues a delayed focus notification.
            self.invalidate_file_tree_ssh_detection_context();
            self.organism_hub.revoke_organism_presence();
        }
        transfer
    }

    /// Phase two resolves from the final model state and the live window gate.
    /// It never relies on an asynchronous `GrabFocus`/`Focused` round trip;
    /// launch-error pages naturally resolve to no `TermView` and stay revoked.
    fn finish_organism_focus_transfer(&self, transfer: bool) {
        if transfer {
            self.sync_organism_focus();
        }
    }

    fn sync_organism_focus(&self) {
        match organism_focus_decision(self.window_active, self.window.is_active()) {
            OrganismFocusDecision::ClaimCurrentPane => {
                let focused = self.active_terminal().and_then(TermCtl::term_view);
                self.organism_hub.focus_view(focused.as_ref());
            }
            OrganismFocusDecision::Revoke => self.organism_hub.revoke_organism_presence(),
        }
    }

    /// Local working directory of the active pane, if it reports one. External
    /// ssh/mosh/container paths are deliberately excluded even when the same
    /// pathname happens to exist on this host.
    fn active_cwd(&self) -> Option<std::path::PathBuf> {
        let tab = self.tabs.get(self.active)?;
        tab.panes
            .get(tab.active_pane)
            .and_then(Pane::local_cwd)
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
    }
}

#[relm4::component]
impl SimpleComponent for AppModel {
    type Init = cli::LaunchOptions;
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("anvil"),
            set_default_width: 800,
            set_default_height: 600,
            set_modal: false,
            set_resizable: true,

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                #[wrap(Some)]
                set_child = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,

                    #[local_ref]
                    top_bar_handle -> gtk::WindowHandle {},

                    #[local_ref]
                    search_bar -> gtk::SearchBar {},

                    #[local_ref]
                    content_paned -> gtk::Paned {
                        set_vexpand: true,
                    },

                    #[local_ref]
                    bottom_bar -> gtk::Box {},
                },
            }
        }
    }

    #[allow(deprecated)]
    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let config_warning = if init.safe_mode {
            None
        } else {
            config::config_file_error()
        };
        let config_validation = (!init.safe_mode).then(config_store::validate_current_config);
        let config_revision = if init.safe_mode {
            None
        } else {
            match config_store::current_revision() {
                Ok(revision) => Some(revision),
                Err(error) => {
                    log::warn!("configuration revision unavailable at startup: {error}");
                    None
                }
            }
        };
        let (mut config, themes, kbmap) = if init.safe_mode {
            config::load_safe_config()
        } else {
            load_config()
        };
        if !init.safe_mode {
            if let Some(mode) = init.mode {
                config.terminal_mode = match mode {
                    cli::Mode::Block => TerminalMode::Block,
                    cli::Mode::Vte => TerminalMode::Vte,
                    cli::Mode::Unified => TerminalMode::Unified,
                };
            }
        }
        let shell_argv = if init.safe_mode {
            Rc::new(vec!["sh".to_string()])
        } else {
            Rc::new(choose_shell_argv(config.shell.as_deref()))
        };
        let startup = config.startup_commands.clone();
        let requested_cwd = init.working_directory.as_ref().map(|path| {
            path.to_str()
                .expect("launch validation rejects non-UTF-8 working directories")
                .to_owned()
        });
        let execute_argv = init.execute.clone().map(Rc::new);
        let restore_session = !init.safe_mode
            && !init.no_restore
            && init.working_directory.is_none()
            && init.execute.is_none();
        let session_persistence = init.execute.is_none() && !init.safe_mode;
        let window_opacity = config.window_opacity;
        let font_scale = config.default_font_scale;
        let config = Rc::new(RefCell::new(config));
        let organism_hub = organism_ui::OrganismHub::new(config.clone());
        let kbmap = Rc::new(RefCell::new(kbmap));
        let workflows = Rc::new(RefCell::new(Vec::new()));

        root.set_opacity(window_opacity);

        startup_ui::install_static_css();
        let dyn_css = startup_ui::install_dynamic_css_provider();

        let stack = gtk::Stack::new();
        let tab_strip = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let sidebar_tab_strip = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_tab_strip.set_valign(gtk::Align::Start);
        sidebar_tab_strip.set_vexpand(true);

        let search =
            search::SearchModel::builder()
                .launch(())
                .forward(sender.input_sender(), |output| match output {
                    search::SearchOutput::Changed(query) => AppMsg::SearchChanged(query),
                    search::SearchOutput::Next => AppMsg::SearchNext,
                    search::SearchOutput::Previous => AppMsg::SearchPrev,
                    search::SearchOutput::Closed => AppMsg::SearchClose,
                });

        let tab_filter_control = sidebar::TabFilterModel::builder().launch(()).forward(
            sender.input_sender(),
            |output| match output {
                sidebar::TabFilterOutput::Changed(filter) => AppMsg::SetTabFilter(filter),
            },
        );

        let file_tree_location = Rc::new(RefCell::new(remote_fs::FsLocation::Local));
        let file_tree_clipboard = Rc::new(RefCell::new(None));
        let startup_ui::FileTreeUi {
            store: file_tree_store,
            filter_model: file_tree_filter_model,
            view: file_tree_view,
            filter: file_tree_filter,
            scroll: file_tree_scroll,
            header: file_header,
            scan_generation: file_tree_scan_generation,
            content_revision: file_tree_content_revision,
            snapshots: file_tree_snapshots,
            failure_gate: file_tree_failure_gate,
            status: file_tree_status,
            pointer_inside: file_tree_pointer_inside,
        } = startup_ui::build_file_tree(
            &sender,
            &config,
            &file_tree_location,
            &file_tree_clipboard,
        );

        let sidebar_width = config.borrow().sidebar_width as i32;
        let tab_placement = config.borrow().tab_placement;
        let sidebar_view = config.borrow().sidebar_view;
        let sidebar_visible = config.borrow().sidebar_visible;
        let sidebar_toggle = sidebar_toggle::SidebarToggleModel::builder()
            .launch((sidebar_view, true))
            .forward(sender.input_sender(), |output| match output {
                sidebar_toggle::SidebarToggleOutput::View(view) => AppMsg::SetSidebarView(view),
            });

        let (tab_strip_scroll, top_tab_scroll) = startup_ui::build_tab_scrolls();
        let top_bar = top_bar::TopBarModel::builder()
            .launch(top_tab_scroll.clone())
            .forward(sender.input_sender(), |output| match output {
                top_bar::TopBarOutput::OpenPalette => AppMsg::Action(Action::ToggleCommandPalette),
                top_bar::TopBarOutput::ToggleSidebar => AppMsg::ToggleSidebar,
                top_bar::TopBarOutput::ToggleTabPlacement => {
                    AppMsg::Action(Action::ToggleTabPlacement)
                }
                top_bar::TopBarOutput::ToggleAgent => AppMsg::OpenAgent,
                top_bar::TopBarOutput::NewTab => AppMsg::NewTab,
                top_bar::TopBarOutput::MinimizeWindow => AppMsg::MinimizeWindow,
                top_bar::TopBarOutput::ToggleMaximizedWindow => AppMsg::ToggleMaximizedWindow,
                top_bar::TopBarOutput::Quit => AppMsg::Quit,
            });

        let toggle_row = sidebar_toggle.widget();

        // "tabs" page: filter entry, the tab strip's sidebar holder, and the
        // mirror list. Exactly one of the two holders is visible at a time —
        // the real strip when tabs are docked here, the mirror when they live
        // in the top bar. See `sync_tab_bar_visibility`.
        let sidebar_tab_scroll = gtk::ScrolledWindow::new();
        sidebar_tab_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        sidebar_tab_scroll.set_vexpand(true);
        sidebar_tab_scroll.set_child(Some(&sidebar_tab_strip));
        sidebar_tab_scroll.set_visible(false);

        let tabs_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        tabs_page.append(tab_filter_control.widget());
        tabs_page.append(&tab_strip_scroll);
        tabs_page.append(&sidebar_tab_scroll);

        // "files" page: root header (up / goto-cwd / path) + file tree.
        let files_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        files_page.append(file_header.widget());
        files_page.append(file_tree_status.widget());
        files_page.append(&file_tree_scroll);

        let sidebar_stack = gtk::Stack::new();
        sidebar_stack.add_named(&tabs_page, Some("tabs"));
        sidebar_stack.add_named(&files_page, Some("files"));
        sidebar_stack.set_vexpand(true);

        let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_box.set_width_request(sidebar_width);
        sidebar_box.set_hexpand(false);
        sidebar_box.add_css_class("tab-strip");
        sidebar_box.append(toggle_row);
        sidebar_box.append(&sidebar_stack);

        // The outer divider owns the file/tab sidebar. The inner divider owns
        // the persistent right-side AI Chats panel while the terminal stack
        // remains its expanding start child.
        let ai_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        ai_paned.set_vexpand(true);
        ai_paned.set_wide_handle(true);
        ai_paned.set_start_child(Some(&stack));
        ai_paned.set_resize_start_child(true);
        ai_paned.set_resize_end_child(false);
        ai_paned.set_shrink_start_child(true);
        ai_paned.set_shrink_end_child(false);

        let content_paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        content_paned.set_vexpand(true);
        content_paned.set_wide_handle(true);
        content_paned.set_start_child(Some(&sidebar_box));
        content_paned.set_end_child(Some(&ai_paned));
        content_paned.set_resize_start_child(false);
        content_paned.set_resize_end_child(true);
        content_paned.set_shrink_start_child(false);
        content_paned.set_shrink_end_child(true);
        content_paned.set_position(sidebar_width);

        // Bottom status bar: left segments pack from the left edge, right
        // segments against the right edge, an expanding spacer between them.
        let bottom_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bottom_bar.add_css_class("bottom-bar");
        let bottom_bar_left = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let bottom_bar_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        bottom_bar_spacer.set_hexpand(true);
        let bottom_bar_right = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        bottom_bar.append(&bottom_bar_left);
        bottom_bar.append(&bottom_bar_spacer);
        bottom_bar.append(&bottom_bar_right);
        bottom_bar.set_visible(config.borrow().bottom_bar);

        let settings_font_names: Vec<String> = root
            .pango_context()
            .list_families()
            .iter()
            .filter(|family| family.is_monospace())
            .map(|family| family.name().to_string())
            .collect();
        let current_font_desc =
            gtk::pango::FontDescription::from_string(&config.borrow().font_desc);
        let current_family = current_font_desc
            .family()
            .map(|family| family.to_string())
            .unwrap_or_default();
        let (settings_font_names, current_font) =
            dialogs::settings::font_choices(settings_font_names, &current_family);
        let current_theme = themes
            .iter()
            .position(|theme| theme.name == config.borrow().theme_name)
            .unwrap_or(0) as u32;
        let settings = dialogs::settings::SettingsModel::builder()
            .launch(dialogs::settings::SettingsInit {
                theme_names: themes.iter().map(|theme| theme.name.clone()).collect(),
                font_names: settings_font_names.clone(),
                values: dialogs::settings::SettingsValues {
                    theme: current_theme,
                    font: current_font,
                    font_size: (current_font_desc.size() as f64 / gtk::pango::SCALE as f64)
                        .max(6.0),
                    font_scale,
                    opacity: window_opacity,
                    scrollback: config.borrow().terminal_scrollback_lines as f64,
                    terminal_mode: match config.borrow().terminal_mode {
                        TerminalMode::Block => 0,
                        TerminalMode::Vte => 1,
                        TerminalMode::Unified => 2,
                    },
                    block_compact: config.borrow().block_compact,
                    command_history: config.borrow().command_history_enabled,
                    ascii_organism_enabled: config.borrow().ascii_organism_enabled,
                    ascii_organism_motion: match config.borrow().ascii_organism_motion {
                        None => 0,
                        Some(config::OrganismMotion::Full) => 1,
                        Some(config::OrganismMotion::Calm) => 2,
                        Some(config::OrganismMotion::Static) => 3,
                    },
                    ai_enabled: config.borrow().ai_enabled,
                    ai_panel_visible: config.borrow().ai_panel_visible,
                    ai_panel_width: config.borrow().ai_panel_width as f64,
                    agent_enabled: config.borrow().agent_enabled,
                    command_correction_enabled: config.borrow().command_correction_enabled,
                    ai_provider: match config.borrow().ai_provider.as_str() {
                        "openai-compatible" => 1,
                        "ollama" => 2,
                        _ => 0,
                    },
                    ai_model: config.borrow().ai_model.clone(),
                    ai_base_url: config.borrow().ai_base_url.clone(),
                    ai_api_key_file: config.borrow().ai_api_key_file.clone(),
                    ai_max_tokens: config.borrow().ai_max_tokens as f64,
                    ai_redact_secrets: config.borrow().ai_redact_secrets,
                    ai_stream: config.borrow().ai_stream,
                    agent_max_turns: config.borrow().agent_max_turns as f64,
                    safe_mode: init.safe_mode,
                    notifications: config.borrow().notify_long_blocks,
                    remote_clipboard: config.borrow().allow_remote_clipboard_write,
                    remote_hosts: config.borrow().remote_hosts.clone(),
                },
            })
            .forward(sender.input_sender(), |output| match output {
                dialogs::settings::SettingsOutput::Theme(index) => AppMsg::SettingsTheme(index),
                dialogs::settings::SettingsOutput::FontDesc(desc) => AppMsg::SettingsFontDesc(desc),
                dialogs::settings::SettingsOutput::FontScale(scale) => {
                    AppMsg::SettingsFontScale(scale)
                }
                dialogs::settings::SettingsOutput::Opacity(opacity) => {
                    AppMsg::SettingsOpacity(opacity)
                }
                dialogs::settings::SettingsOutput::Scrollback(lines) => {
                    AppMsg::SettingsScrollback(lines)
                }
                dialogs::settings::SettingsOutput::TerminalMode(mode) => {
                    AppMsg::SettingsTerminalMode(mode)
                }
                dialogs::settings::SettingsOutput::BlockCompact(enabled) => {
                    AppMsg::SettingsBlockCompact(enabled)
                }
                dialogs::settings::SettingsOutput::CommandHistory(enabled) => {
                    AppMsg::SettingsCommandHistory(enabled)
                }
                dialogs::settings::SettingsOutput::AsciiOrganism(enabled) => {
                    AppMsg::SettingsAsciiOrganism(enabled)
                }
                dialogs::settings::SettingsOutput::AsciiOrganismMotion(motion) => {
                    AppMsg::SettingsAsciiOrganismMotion(motion)
                }
                dialogs::settings::SettingsOutput::AiEnabled(enabled) => {
                    AppMsg::SettingsAiEnabled(enabled)
                }
                dialogs::settings::SettingsOutput::AiPanelVisible(visible) => {
                    AppMsg::SettingsAiPanelVisible(visible)
                }
                dialogs::settings::SettingsOutput::AiPanelWidth(width) => {
                    AppMsg::SettingsAiPanelWidth(width)
                }
                dialogs::settings::SettingsOutput::AgentEnabled(enabled) => {
                    AppMsg::SettingsAgentEnabled(enabled)
                }
                dialogs::settings::SettingsOutput::CommandCorrection(enabled) => {
                    AppMsg::SettingsCommandCorrection(enabled)
                }
                dialogs::settings::SettingsOutput::AiProvider(provider) => {
                    AppMsg::SettingsAiProvider(provider)
                }
                dialogs::settings::SettingsOutput::AiModel(model) => AppMsg::SettingsAiModel(model),
                dialogs::settings::SettingsOutput::AiApiKeyFile(path) => {
                    AppMsg::SettingsAiKeyFile(path)
                }
                dialogs::settings::SettingsOutput::AiBaseUrl(base_url) => {
                    AppMsg::SettingsAiBaseUrl(base_url)
                }
                dialogs::settings::SettingsOutput::AiMaxTokens(max_tokens) => {
                    AppMsg::SettingsAiMaxTokens(max_tokens)
                }
                dialogs::settings::SettingsOutput::AiRedactSecrets(enabled) => {
                    AppMsg::SettingsAiRedactSecrets(enabled)
                }
                dialogs::settings::SettingsOutput::AiStream(enabled) => {
                    AppMsg::SettingsAiStream(enabled)
                }
                dialogs::settings::SettingsOutput::AgentMaxTurns(turns) => {
                    AppMsg::SettingsAgentMaxTurns(turns)
                }
                dialogs::settings::SettingsOutput::Notifications(enabled) => {
                    AppMsg::SettingsNotifications(enabled)
                }
                dialogs::settings::SettingsOutput::RemoteClipboard(enabled) => {
                    AppMsg::SettingsRemoteClipboard(enabled)
                }
                dialogs::settings::SettingsOutput::RemoteHosts(hosts) => {
                    AppMsg::SettingsRemoteHosts(hosts)
                }
            });
        let remote_picker = dialogs::remote_picker::RemotePickerModel::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), |output| match output {
                dialogs::remote_picker::RemotePickerOutput::Connect(index) => {
                    AppMsg::Action(Action::ConnectRemote(index as u8))
                }
            });
        let command_palette = dialogs::command_palette::PaletteModel::builder()
            .launch(dialogs::command_palette::PaletteInit {
                parent: root.clone(),
                keybindings: kbmap.clone(),
                workflows: workflows.clone(),
            })
            .forward(sender.input_sender(), |output| match output {
                dialogs::command_palette::PaletteOutput::Action(action) => AppMsg::Action(action),
                dialogs::command_palette::PaletteOutput::TypeCommand(command) => {
                    AppMsg::PaletteTypeCommand(command)
                }
                dialogs::command_palette::PaletteOutput::AskAi(query) => {
                    AppMsg::PaletteAskAi(query)
                }
                dialogs::command_palette::PaletteOutput::RunWorkflow(path) => {
                    AppMsg::PaletteRunWorkflow(path)
                }
            });
        let debug_dashboard = dialogs::debug_dashboard::DebugDashboardModel::builder()
            .launch(root.clone())
            .detach();
        let workflow_dialog = dialogs::workflow::WorkflowModel::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), |output| match output {
                dialogs::workflow::WorkflowOutput::Command(command) => {
                    AppMsg::PaletteTypeCommand(command)
                }
            });
        let initial_ai_panel_visible =
            config.borrow().ai_enabled && config.borrow().ai_panel_visible && !init.safe_mode;
        let initial_ai_panel_width = config.borrow().ai_panel_width;
        let ai_panel = dialogs::ai_panel::AiPanelModel::builder()
            .launch(dialogs::ai_panel::AiPanelInit {
                redact_secrets: config.borrow().ai_redact_secrets,
            })
            .forward(sender.input_sender(), |output| match output {
                dialogs::ai_panel::AiPanelOutput::SnapshotChanged(snapshot) => {
                    AppMsg::AiConversationSnapshot(snapshot)
                }
                dialogs::ai_panel::AiPanelOutput::CloseRequested => AppMsg::AiPanelCloseRequested,
            });
        let tasks_panel = dialogs::tasks_panel::TasksPanelModel::builder()
            .launch(())
            .forward(sender.input_sender(), |output| match output {
                dialogs::tasks_panel::TasksPanelOutput::Action(action) => {
                    AppMsg::TaskPanelAction(action)
                }
            });
        // Both right-side panels share the `ai_paned` end slot through one
        // stack: the persisted width/visibility behavior stays identical for
        // AI Chats, and Tasks rides the same chrome without its own config.
        let side_stack = gtk::Stack::new();
        side_stack.add_named(ai_panel.widget(), Some("chats"));
        side_stack.add_named(tasks_panel.widget(), Some("tasks"));
        side_stack.set_visible_child_name("chats");
        side_stack.set_visible(initial_ai_panel_visible);
        ai_paned.set_end_child(Some(&side_stack));
        // The initial window is 800 px wide and the left sidebar has already
        // claimed its configured share. A map-time idle below corrects this
        // estimate against the compositor's actual allocation.
        let initial_inner_width = 800_i32.saturating_sub(sidebar_width);
        if let Some(position) =
            restored_ai_panel_position(initial_inner_width, initial_ai_panel_width)
        {
            ai_paned.set_position(position);
        }
        let notebook = notebook::NotebookModel::builder()
            .launch(notebook::NotebookInit {
                parent: root.clone(),
                safe_mode: init.safe_mode,
                configured_shell: shell_argv.as_ref().clone(),
            })
            .detach();
        let agent_panel = agent::AgentPanelModel::builder()
            .launch(root.clone())
            .forward(sender.input_sender(), |output| match output {
                agent::AgentPanelOutput::Send(text) => AppMsg::AgentSend(text),
                agent::AgentPanelOutput::Approve(reference, command) => {
                    AppMsg::AgentEditAndApprove(reference, command)
                }
                agent::AgentPanelOutput::Insert(reference, command) => {
                    AppMsg::AgentInsert(reference, command)
                }
                agent::AgentPanelOutput::Reject(reference) => AppMsg::AgentReject(reference),
                agent::AgentPanelOutput::StopRequest => AppMsg::AgentStopRequest,
                agent::AgentPanelOutput::RetryRequest => AppMsg::AgentRetryRequest,
                agent::AgentPanelOutput::Continue => AppMsg::AgentContinue,
                agent::AgentPanelOutput::NewTask => AppMsg::AgentNewTask,
                agent::AgentPanelOutput::AttachContext => AppMsg::AgentAttachContext,
                agent::AgentPanelOutput::ClearContext => AppMsg::AgentClearContext,
                agent::AgentPanelOutput::OpenSettings => AppMsg::OpenAgentSettings,
                agent::AgentPanelOutput::Closed => AppMsg::AgentClose,
            });
        // Both tab lists speak the same output vocabulary, so they route
        // through one translation.
        let tab_rows = FactoryVecDeque::builder()
            .launch(tab_strip.clone())
            .forward(sender.input_sender(), startup_ui::tab_row_output_to_msg);
        let sidebar_tab_rows = FactoryVecDeque::builder()
            .launch(sidebar_tab_strip.clone())
            .forward(sender.input_sender(), startup_ui::tab_row_output_to_msg);
        // Row targets choose an insertion anchor. The parent targets make the
        // blank remainder of either tab bar useful as a pane-to-tab drop zone.
        for tab_bar in [&tab_strip, &sidebar_tab_strip] {
            let sender = sender.clone();
            tab_strip::install_pane_drop_target(tab_bar, move |pane_id| {
                sender.input(AppMsg::PromotePaneToTab {
                    pane_id,
                    anchor_tab_id: None,
                    after: true,
                });
                true
            });
        }

        let toast_overlay = adw::ToastOverlay::new();
        let quit_allowed = Rc::new(std::cell::Cell::new(false));
        let tab_drag_coordinator = Rc::new(tab_strip::TabDragCoordinator::default());
        let mut model = AppModel {
            config,
            organism_hub,
            window_active: root.is_active(),
            config_revision: RefCell::new(config_revision),
            themes: Rc::new(themes),
            kbmap,
            shell_argv,
            tabs: Vec::new(),
            active: 0,
            tab_drag_origin: None,
            tab_drag_coordinator,
            next_id: 0,
            next_pane_id: 0,
            pending_split_spawns: std::collections::HashMap::new(),
            // With tabs in the top bar, keep the optional file sidebar closed
            // until the user explicitly opens it.
            sidebar_visible,
            font_scale,
            font_persist_generation: Rc::new(std::cell::Cell::new(0)),
            window_opacity,
            stack: stack.clone(),
            tab_strip: tab_strip.clone(),
            tab_rows,
            sidebar_tab_strip: sidebar_tab_strip.clone(),
            sidebar_tab_rows,
            window: root.clone(),
            toast_overlay: toast_overlay.clone(),
            opacity_toast: Rc::new(RefCell::new(None)),
            quit_allowed: quit_allowed.clone(),
            session_persistence,
            persistence_failure_notices: std::collections::HashMap::new(),
            safe_mode: init.safe_mode,
            dyn_css,
            search,
            tab_filter_control,
            tab_filter: String::new(),
            file_tree_store: file_tree_store.clone(),
            file_header,
            file_tree_root: Rc::new(RefCell::new(std::path::PathBuf::new())),
            file_tree_scan_generation,
            file_tree_content_revision,
            file_tree_snapshots,
            file_tree_navigation_revision: Rc::new(std::cell::Cell::new(0)),
            file_tree_navigation_cancellation: Rc::new(RefCell::new(None)),
            file_tree_navigation_history: Rc::new(RefCell::new(
                file_tree::FileTreeNavigationHistory::default(),
            )),
            file_tree_root_cache: Rc::new(RefCell::new(file_tree::RootListingCache::default())),
            file_tree_failure_gate,
            file_tree_refresh_revisions: Rc::new(RefCell::new(
                file_tree::DirectoryRefreshRevisions::default(),
            )),
            file_tree_view,
            file_tree_filter_model,
            file_tree_filter,
            file_tree_status,
            file_tree_pointer_inside,
            file_tree_location,
            file_tree_ssh_observation: None,
            file_tree_ssh_detection_revision: std::cell::Cell::new(0),
            file_tree_user_operation_revision: std::cell::Cell::new(0),
            file_tree_clipboard,
            file_tree_clipboard_revision: std::cell::Cell::new(0),
            file_tree_transfer_toast: Rc::new(RefCell::new(None)),
            file_tree_transfer_revision: Rc::new(std::cell::Cell::new(0)),
            tab_strip_scroll: tab_strip_scroll.clone(),
            sidebar_tab_scroll: sidebar_tab_scroll.clone(),
            top_tab_scroll: top_tab_scroll.clone(),
            top_bar,
            sidebar_box: sidebar_box.clone(),
            content_paned: content_paned.clone(),
            ai_paned: ai_paned.clone(),
            bottom_bar: bottom_bar.clone(),
            bottom_bar_left: bottom_bar_left.clone(),
            bottom_bar_right: bottom_bar_right.clone(),
            bottom_bar_content: Rc::new(RefCell::new(Default::default())),
            sidebar_stack: sidebar_stack.clone(),
            sidebar_toggle,
            tab_placement: std::cell::Cell::new(tab_placement),
            sidebar_view: std::cell::Cell::new(sidebar_view),
            command_palette,
            settings,
            settings_font_names: Rc::new(settings_font_names),
            remote_picker,
            debug_dashboard,
            workflow_dialog,
            ai_panel,
            ai_panel_visible: std::cell::Cell::new(initial_ai_panel_visible),
            ai_panel_width_generation: Rc::new(std::cell::Cell::new(0)),
            side_stack: side_stack.clone(),
            tasks_panel,
            tasks_panel_visible: std::cell::Cell::new(false),
            task_manager: crate::agent_task::TaskManager::new(),
            agent_runtime: crate::agent_task::AgentRuntimeManager::new(),
            agent_diff: crate::agent_task::AgentDiffPanel::new(),
            selected_task: None,
            pending_task_creation: None,
            pending_validation_pins: std::collections::HashMap::new(),
            agent_tasks_timer_armed: std::cell::Cell::new(false),
            ai_conversation: None,
            command_suggestion: Rc::new(RefCell::new(None)),
            command_suggestion_generation: Rc::new(std::cell::Cell::new(0)),
            command_corrections: Rc::new(RefCell::new(std::collections::HashMap::new())),
            command_correction_generation: Rc::new(std::cell::Cell::new(0)),
            notebook,
            workflows,
            workflow_refresh: workflow_ops::WorkflowRefreshState::default(),
            workflow_refusals: Vec::new(),
            active_agent: Rc::new(RefCell::new(None)),
            agent_panel_generation: Rc::new(std::cell::Cell::new(0)),
            agent_panel,
        };
        model.sync_agent_toggle();

        // Populate the palette cache opportunistically without adding startup
        // latency. Safe mode avoids reading user workflow directories until an
        // explicit palette action asks for them.
        if !model.safe_mode {
            model.refresh_workflows_async(&sender);
        }

        {
            let width_sender = sender.clone();
            let panel = model.side_stack.clone();
            model.ai_paned.connect_position_notify(move |paned| {
                if !panel.is_visible() {
                    return;
                }
                if let Some(measured) =
                    ai_panel_width_from_geometry(paned.width(), paned.position())
                {
                    width_sender.input(AppMsg::AiPanelWidthChanged(measured));
                }
            });
        }
        if initial_ai_panel_visible {
            let paned = model.ai_paned.clone();
            gtk::glib::idle_add_local_once(move || {
                if let Some(position) =
                    restored_ai_panel_position(paned.width(), initial_ai_panel_width)
                {
                    paned.set_position(position);
                }
            });
        }

        let search_bar = model.search.widget();
        // WindowHandle gives the custom Relm4 toolbar native titlebar move,
        // double-click, and context-menu behavior without stealing gestures
        // from its buttons or the draggable tab strip.
        let top_bar = model.top_bar.widget();
        let top_bar_handle = gtk::WindowHandle::new();
        top_bar_handle.set_child(Some(top_bar));
        let toast_overlay = &model.toast_overlay;
        let widgets = view_output!();
        let cross_block_search_key_latch =
            Rc::new(RefCell::new(CrossBlockSearchKeyLatch::default()));

        // Route both the title-bar button and the window manager's close action
        // through the same running-process confirmation. ForceQuit flips the
        // shared flag before calling close(), allowing that second signal
        // through without presenting the dialog again.
        {
            let close_sender = sender.clone();
            let quit_allowed = model.quit_allowed.clone();
            root.connect_close_request(move |_| {
                if quit_allowed.get() {
                    glib::Propagation::Proceed
                } else {
                    close_sender.input(AppMsg::Quit);
                    glib::Propagation::Stop
                }
            });
        }
        {
            let window_sender = sender.clone();
            root.connect_maximized_notify(move |window| {
                window_sender.input(AppMsg::WindowMaximized(window.is_maximized()));
            });
        }
        {
            let window_sender = sender.clone();
            let organism_hub = model.organism_hub.clone();
            let cross_block_search_key_latch = cross_block_search_key_latch.clone();
            root.connect_is_active_notify(move |window| {
                let active = window.is_active();
                // Revocation is a visibility safety boundary, so perform it
                // synchronously in GTK's notification. The Relm message still
                // records activation order and reclaims the current pane on
                // the active edge; delayed focus events remain gated there.
                if !active {
                    organism_hub.revoke_organism_presence();
                    // A compositor may deactivate us without delivering the
                    // opening key's release. The next real press must not stay
                    // trapped behind a stale repeat guard.
                    cross_block_search_key_latch.borrow_mut().reset();
                }
                window_sender.input(AppMsg::WindowActive(active));
            });
        }

        if let Some(error) = config_warning {
            model.show_toast(format!(
                "Config could not be loaded; defaults are active. Your file was left untouched. {error}"
            ));
        } else if let Some(validation) = config_validation {
            if validation.errors() > 0 {
                model.show_toast(format!(
                    "Configuration has {} validation error(s). Run `anvil --check-config`; invalid values kept safe defaults.",
                    validation.errors()
                ));
            } else if validation.warnings() > 0 {
                model.show_toast(format!(
                    "Configuration loaded with {} warning(s). Run `anvil --check-config` for details.",
                    validation.warnings()
                ));
            }
        }
        if init.safe_mode {
            model.show_toast(
                "Safe mode: VTE + sh, with startup commands, restore, persistence, remote hosts, AI, and jsh updates disabled.",
            );
        }

        // Place the tab strip (sidebar vs top bar) and select the sidebar view.
        model.apply_tab_placement();
        model.sidebar_box.set_visible(model.sidebar_visible);

        // Window-level key controller: intercept shortcuts before VTE.
        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let kb = model.kbmap.clone();
            let ksender = sender.clone();
            let window = root.clone();
            let ai_panel_root = model.ai_panel.widget().clone().upcast::<gtk::Widget>();
            let file_tree_header_root = model.file_header.widget().clone().upcast::<gtk::Widget>();
            let file_tree_root = model.file_tree_view.clone().upcast::<gtk::Widget>();
            let file_tree_status_root = model
                .file_tree_status
                .widget()
                .clone()
                .upcast::<gtk::Widget>();
            let file_tree_pointer_inside = model.file_tree_pointer_inside.clone();
            let file_tree_generation = model.file_tree_scan_generation.clone();
            let file_tree_content_revision = model.file_tree_content_revision.clone();
            let file_tree_location = model.file_tree_location.clone();
            let file_tree_config = model.config.clone();
            let cross_block_search_key_latch = cross_block_search_key_latch.clone();
            key_controller.connect_key_pressed(move |_c, keyval, keycode, state| {
                let focused = gtk::prelude::RootExt::focus(&window);
                let ai_panel_focused = focused
                    .clone()
                    .is_some_and(|focus| widget_is_within(focus, &ai_panel_root));
                let file_tree_focused = focused.is_some_and(|focus| {
                    widget_is_within(focus.clone(), &file_tree_header_root)
                        || widget_is_within(focus.clone(), &file_tree_root)
                        || widget_is_within(focus, &file_tree_status_root)
                });
                if file_tree_f5_should_refresh(
                    keyval,
                    state,
                    file_tree_focused,
                    file_tree_pointer_inside.get(),
                    file_tree_root.is_mapped(),
                ) {
                    let location = file_tree_location.borrow();
                    let config = file_tree_config.borrow();
                    ksender.input(AppMsg::FileTreeRefresh {
                        intent: Box::new(file_tree::capture_file_tree_user_intent(
                            file_tree_generation.get(),
                            file_tree_content_revision.get(),
                            &location,
                            &config.remote_hosts,
                        )),
                    });
                    return glib::Propagation::Stop;
                }
                if let Some(shortcut) = file_tree_navigation_shortcut(
                    keyval,
                    state,
                    file_tree_focused,
                    file_tree_pointer_inside.get(),
                    file_tree_root.is_mapped(),
                ) {
                    ksender.input(match shortcut {
                        FileTreeNavigationShortcut::Back => AppMsg::FileTreeGoBack,
                        FileTreeNavigationShortcut::Forward => AppMsg::FileTreeGoForward,
                        FileTreeNavigationShortcut::Up => AppMsg::FileTreeGoUp,
                        FileTreeNavigationShortcut::Home => AppMsg::FileTreeGoHome,
                        FileTreeNavigationShortcut::OpenPath => AppMsg::FileTreeOpenPathEntry,
                    });
                    return glib::Propagation::Stop;
                }
                // Composer/search/list Enter semantics and IME candidate
                // confirmation belong to the focused AI child before any
                // optional global binding for Enter.
                if matches!(keyval, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                    && ai_panel_focused
                {
                    return glib::Propagation::Proceed;
                }
                // The GTK edge: keysym + modifier state -> toolkit-neutral
                // chord. `None` means no chord string could name this key.
                let chord = chord_from_gdk(keyval, state);
                let action = chord.as_ref().and_then(|chord| kb.borrow().lookup(chord));
                match cross_block_search_key_latch
                    .borrow_mut()
                    .press(keycode, action == Some(Action::CrossBlockSearch))
                {
                    CrossBlockSearchKeyPress::DispatchToggle => {
                        ksender.input(AppMsg::Action(Action::CrossBlockSearch));
                        return glib::Propagation::Stop;
                    }
                    CrossBlockSearchKeyPress::SuppressHeldRepeat => {
                        return glib::Propagation::Stop;
                    }
                    CrossBlockSearchKeyPress::Proceed => {}
                }
                let Some(chord) = chord else {
                    return glib::Propagation::Proceed;
                };
                if let Some(action) = action {
                    ksender.input(AppMsg::Action(action));
                    return glib::Propagation::Stop;
                }
                // Alt+<Copy-binding> in block mode → copy block output only.
                // Re-lookup with ALT stripped so users only need to bind Copy once.
                if chord.mods.alt {
                    let mut stripped = chord;
                    stripped.mods.alt = false;
                    if kb.borrow().lookup(&stripped) == Some(Action::Copy) {
                        ksender.input(if ai_panel_focused {
                            AppMsg::Action(Action::Copy)
                        } else {
                            AppMsg::CopyOutputOnly
                        });
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
        }
        {
            let cross_block_search_key_latch = cross_block_search_key_latch.clone();
            key_controller.connect_key_released(move |_, _, keycode, _| {
                cross_block_search_key_latch.borrow_mut().release(keycode);
            });
        }
        root.add_controller(key_controller);

        // Ctrl+wheel zooms the font. Capture phase so it wins over VTE's own
        // scroll handling and over the block view's mouse reporting; both see
        // the event only when Ctrl is not held.
        let scroll_controller =
            gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let zsender = sender.clone();
            scroll_controller.connect_scroll(move |controller, _dx, dy| {
                if !controller
                    .current_event_state()
                    .contains(gtk::gdk::ModifierType::CONTROL_MASK)
                {
                    return glib::Propagation::Proceed;
                }
                // Touchpads emit fractional deltas; a zero step would still
                // claim the event, so let those through untouched.
                if dy == 0.0 {
                    return glib::Propagation::Proceed;
                }
                let action = if dy < 0.0 {
                    Action::FontIncrease
                } else {
                    Action::FontDecrease
                };
                zsender.input(AppMsg::Action(action));
                glib::Propagation::Stop
            });
        }
        root.add_controller(scroll_controller);

        // Config file hot reload is intentionally disabled in safe mode: a
        // change on disk must not re-enable startup, persistence, remote, or AI
        // behavior in the isolated recovery session.
        if !init.safe_mode {
            let config_path = config_file_path();
            if let Err(error) = config_store::ensure_config_parent(&config_path) {
                log::warn!(
                    "Config hot reload is unavailable for {}: {error}",
                    config_path.display()
                );
            } else if let Ok(monitor) = gio::File::for_path(&config_path)
                .monitor_file(gio::FileMonitorFlags::NONE, None::<&Cancellable>)
            {
                let rsender = sender.clone();
                let reload_pending = Rc::new(std::cell::Cell::new(false));
                monitor.connect_changed(move |_, _, _, event| {
                    if matches!(
                        event,
                        gio::FileMonitorEvent::Changed | gio::FileMonitorEvent::Created
                    ) && !reload_pending.get()
                    {
                        reload_pending.set(true);
                        let rsender = rsender.clone();
                        let pending = reload_pending.clone();
                        glib::timeout_add_local_once(
                            std::time::Duration::from_millis(200),
                            move || {
                                pending.set(false);
                                rsender.input(AppMsg::ReloadConfig);
                            },
                        );
                    }
                });
                unsafe { root.set_data("config-monitor", monitor) };
            }
        }

        model.apply_dynamic_css();

        // Restore a previously-saved session if present (consume-on-start);
        // otherwise open a single fresh tab running startup_commands.
        match restore_session.then(session::load_session).flatten() {
            Some(saved) => {
                model.ai_conversation = saved.ai_conversation.clone();
                if let Some(snapshot) = saved.ai_conversation.clone() {
                    model
                        .ai_panel
                        .emit(dialogs::ai_panel::AiPanelMsg::Restore(snapshot));
                }
                for tab in &saved.tabs {
                    model.restore_tab(tab, &sender);
                }
                let active_id = model
                    .tabs
                    .get(saved.active.min(model.tabs.len().saturating_sub(1)))
                    .map(|t| t.id);
                if let Some(id) = active_id {
                    model.select_tab(id, &sender);
                }
                // Loading durably claims the exited process's snapshot without
                // consuming it. This checkpoint publishes the restored
                // workspace under the current owner; only then can the old
                // claim be committed and removed.
                model.persist_session();
            }
            None => {
                let initial_argv = execute_argv.unwrap_or_else(|| model.shell_argv.clone());
                let initial_commands = if init.execute.is_some() {
                    InitialCommands::default()
                } else {
                    InitialCommands::from_config(startup.as_deref())
                };
                model.add_tab_with(initial_commands, requested_cwd, initial_argv, &sender);
            }
        }

        // A panel restored as visible must be immediately usable, not merely
        // painted with its saved transcript while lacking a provider client.
        if initial_ai_panel_visible {
            if let Ok(client) = ai::client_from_config(&model.config.borrow()) {
                model.ai_panel.emit(dialogs::ai_panel::AiPanelMsg::Open {
                    history_path: model.config.borrow().command_history_path.clone(),
                    client,
                    stream: model.config.borrow().ai_stream,
                    redact_secrets: model.config.borrow().ai_redact_secrets,
                    // Same consent projection the native Codex prompt uses:
                    // no terminal evidence leaves the machine until both the
                    // AI switch and the sharing opt-in are on.
                    share_command_context: crate::agent_task_ui::prompt_policy(
                        &model.config.borrow(),
                    )
                    .share_command_context,
                    initial_context: None,
                });
            }
        }

        model.init_file_tree();
        {
            let ttl_sender = sender.clone();
            gtk::glib::timeout_add_local(std::time::Duration::from_secs(5), move || {
                ttl_sender.input(AppMsg::FileTreeTtlTick);
                gtk::glib::ControlFlow::Continue
            });
        }
        model.refresh_bottom_bar();

        // anvil prefers jsh as its shell, so it is worth noticing when the
        // machine has none or an old one. The check runs on a worker thread and
        // stays silent unless it has something actionable to offer.
        model.start_jsh_update_check(&sender);

        // Directories and foreground commands are polled, not pushed, so the
        // split panes' headers need a slow tick to stay honest. It touches
        // only the visible tab, and only while that tab is actually split.
        {
            let sender = sender.clone();
            glib::timeout_add_seconds_local(1, move || {
                sender.input(AppMsg::RefreshPaneHeaders);
                glib::ControlFlow::Continue
            });
        }

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::NewTab => {
                let startup = self.config.borrow().startup_commands.clone();
                self.add_tab(startup, &sender);
            }
            AppMsg::CloseTab(id) => self.request_close_tab(id, &sender),
            AppMsg::ForceCloseTab(id) => self.close_tab(id, &sender),
            AppMsg::ForceClosePane(pane_id) => self.close_pane(pane_id, &sender),
            AppMsg::ForceCloseMarked(ids) => self.close_tabs(ids, &sender),
            AppMsg::SelectTab(id) => self.select_tab(id, &sender),
            AppMsg::NextTab => self.switch_tab(1, &sender),
            AppMsg::PrevTab => self.switch_tab(-1, &sender),
            AppMsg::ToggleSidebar => {
                self.set_sidebar_visible(!self.sidebar_visible, true);
            }
            AppMsg::MinimizeWindow => self.window.minimize(),
            AppMsg::ToggleMaximizedWindow => {
                if self.window.is_maximized() {
                    self.window.unmaximize();
                } else {
                    self.window.maximize();
                }
            }
            AppMsg::WindowMaximized(maximized) => {
                self.top_bar
                    .emit(top_bar::TopBarMsg::SetMaximized(maximized));
            }
            AppMsg::WindowActive(active) => {
                self.window_active = active;
                self.sync_organism_focus();
                if active {
                    self.file_tree_revalidate_due();
                }
            }
            AppMsg::Quit => {
                self.request_quit(&sender);
            }
            AppMsg::ForceQuit => self.force_quit(),
            AppMsg::Toast(message) => self.show_toast(message),
            AppMsg::ToastWithUndo {
                pane_id,
                message,
                button,
                undo,
            } => self.show_undo_toast(pane_id, &message, &button, undo, &sender),
            AppMsg::ApplyNoticeUndo { pane_id, undo } => {
                let input = match undo {
                    terminal::NoticeUndo::ClearBlocks => VteInput::UndoClearBlocks,
                };
                match self.find_pane(pane_id) {
                    Some((tab_index, pane_index)) => {
                        self.tabs[tab_index].panes[pane_index].terminal.emit(input)
                    }
                    None => self.show_toast("That pane has closed — nothing to undo."),
                }
            }
            AppMsg::ImageFilesDropped { pane_id, paths } => {
                match image_drop::prompt_payload(&paths) {
                    Ok(payload) => {
                        if let Some((tab_index, pane_index)) = self.find_pane(pane_id) {
                            let terminal = &self.tabs[tab_index].panes[pane_index].terminal;
                            terminal.emit(VteInput::GrabFocus);
                            terminal.emit(VteInput::WriteInput(payload.into_bytes()));
                        } else {
                            self.show_toast("Image drop rejected: the target terminal closed.");
                        }
                    }
                    Err(error) => {
                        self.show_toast(format!("Image drop rejected: {error}"));
                    }
                }
            }
            AppMsg::JshUpdateChecked(status) => self.offer_jsh_update(&status, &sender),
            AppMsg::WorkflowRefreshFinished(result) => self.finish_workflow_refresh(result),
            AppMsg::CopyOutputOnly => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::CopyOutputOnly);
                }
            }
            AppMsg::Action(action) => self.execute_action(action, &sender),
            AppMsg::TaskPanelAction(action) => self.execute_task_panel_action(action, &sender),
            AppMsg::AgentTasksTick => self.agent_tasks_tick(&sender),
            AppMsg::ReloadConfig => self.reload_config(&sender),
            AppMsg::PaneLaunched(pane_id) => {
                if let Some((source_tab_id, _)) = self.pending_split_spawns.remove(&pane_id) {
                    if let Some((tab_index, _)) = self.find_pane(pane_id) {
                        if self.tabs[tab_index].id == source_tab_id {
                            if let Some(root) = self.tabs[tab_index].holder.first_child() {
                                workspace_ops::schedule_pane_rebalance(root);
                            }
                        }
                    }
                }
                // A successful backend is enough to resolve organism
                // ownership; an error page or backend that never emits a
                // focus-enter must not be required for the handoff.
                if self.active_pane_id() == Some(pane_id) {
                    self.sync_organism_focus();
                }
                // The spawn consumed the pinned validation cwd: the child has
                // entered the directory, so the descriptor no longer needs
                // retention on the app side.
                self.note_pane_launched_task_pin(pane_id);
            }
            AppMsg::PaneLaunchFailed(pane_id, message) => {
                if let Some((source_tab_id, source_pane_id)) =
                    self.pending_split_spawns.remove(&pane_id)
                {
                    let restored =
                        self.rollback_failed_split(pane_id, source_tab_id, source_pane_id);
                    if restored {
                        log::error!(
                            "{message} Rolled back failed split pane {pane_id} to source pane {source_pane_id}."
                        );
                        self.show_toast(format!(
                            "{message} The failed split was rolled back and the existing layout was restored."
                        ));
                    } else {
                        log::error!(
                            "{message} Could not safely roll back failed split pane {pane_id}."
                        );
                        self.show_toast(format!(
                            "{message} The failed split could not be rolled back safely."
                        ));
                    }
                } else if let Some((idx, pane_index)) = self.find_pane(pane_id) {
                    let active_failure =
                        idx == self.active && self.tabs[idx].active_pane == pane_index;
                    let focus_transfer = if active_failure {
                        self.begin_organism_focus_transfer(None, false)
                    } else {
                        false
                    };
                    if let Some(connection) = self.tabs[idx]
                        .remote
                        .as_mut()
                        .filter(|connection| connection.pane_id == pane_id)
                    {
                        connection.status = ConnStatus::Disconnected;
                    }
                    self.sync_tab_strip();
                    self.show_toast(message);
                    self.finish_organism_focus_transfer(focus_transfer);
                } else {
                    // A synchronously rejected Block split drops its prepared
                    // component before commit and reports its own toast. Ignore
                    // the component's deferred stale failure output.
                    log::debug!("Ignoring launch failure for retired pane {pane_id}: {message}");
                }
            }
            AppMsg::SetTabWidth(width) => {
                self.config.borrow_mut().tab_width = width.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH);
                self.sync_tab_strip();
                self.persist_config();
            }
            AppMsg::PaneExited(_, pane_id, code) => {
                // A task terminal's exit is authoritative task evidence; the
                // task model consumes it before the pane closes. Validation
                // and agent terminals are never remote-reconnect candidates.
                if self.note_task_terminal_exited(pane_id, code) {
                    self.close_pane(pane_id, &sender);
                    return;
                }
                // A remote single-pane tab that died abnormally is reconnected in
                // place instead of closed; everything else closes normally.
                if self.schedule_remote_reconnect(pane_id, code, &sender) {
                    return;
                }
                self.close_pane(pane_id, &sender);
            }
            AppMsg::RemoteReconnectTick(pane_id, secs) => {
                if self.remote_reconnect_target_is_valid(pane_id) {
                    if let Some((idx, _)) = self.find_pane(pane_id) {
                        if let Some(conn) = self.tabs[idx].remote.as_ref() {
                            self.tabs[idx].title =
                                format!("{} — reconnect {secs}s", conn.host.name);
                            self.sync_tab_strip();
                        }
                    }
                } else {
                    self.cancel_remote_reconnect(pane_id, &sender);
                }
            }
            AppMsg::RemoteReconnectNow(pane_id, attempt) => {
                self.do_remote_reconnect(pane_id, attempt, &sender)
            }
            AppMsg::PaneCwdChanged(_, pane_id, path, external) => {
                if !external {
                    self.clear_file_tree_ssh_observation_for_pane(pane_id);
                }
                if let Some((ti, pi)) = self.find_pane(pane_id) {
                    let managed_remote = self.tabs[ti]
                        .remote
                        .as_ref()
                        .is_some_and(|remote| remote.pane_id == pane_id);
                    // Each backend has already combined its authenticated OSC
                    // authority with the live foreground process. Preserve
                    // that result; a second app-level pass would discard the
                    // per-pane token and make opaque Flatpak host shells look
                    // external again.
                    self.tabs[ti].panes[pi].cwd_external = managed_remote || external;
                    self.tabs[ti].panes[pi].cwd = Some(path.clone());
                    if self.tabs[ti].panes.len() > 1 {
                        self.refresh_pane_headers(ti);
                    }
                    let connection_changed = self.mark_remote_connected(ti, pane_id);
                    if self.tabs[ti].active_pane == pi && !self.tabs[ti].custom_title {
                        let number = ti as u32 + 1;
                        self.tabs[ti].title = default_tab_title(number, Some(&path));
                        self.rebuild_tab_strip(&sender);
                    } else if connection_changed {
                        self.sync_tab_strip();
                    }
                    self.refresh_bottom_bar();
                }
            }
            AppMsg::PaneRemoteSessionId(pane_id, id) => {
                if !config::valid_session_id(&id) {
                    log::warn!("Ignoring invalid runtime remote session id");
                    return;
                }
                if let Some((idx, pane_index)) = self.find_pane(pane_id) {
                    self.tabs[idx].panes[pane_index].session_id = Some(id.clone());
                    if let Some(conn) = self.tabs[idx]
                        .remote
                        .as_mut()
                        .filter(|conn| conn.pane_id == pane_id)
                    {
                        // Learn jsh's session id so a reconnect passes the same
                        // `--session <id>` and jsh restores cwd/env/aliases.
                        // Overrides any static value the TOML config set.
                        let learned = conn.learn_session(id);
                        debug_assert!(learned, "session id was validated above");
                    }
                }
            }
            AppMsg::PaneFocused(_, pane_id) => {
                if let Some((ti, pi)) = self.find_pane(pane_id) {
                    let previous_pane_id = self
                        .tabs
                        .get(self.active)
                        .and_then(|tab| tab.panes.get(tab.active_pane))
                        .map(|pane| pane.id);
                    let active_pane_changed = ti == self.active
                        && search::active_pane_changed(previous_pane_id, Some(pane_id));
                    if active_pane_changed {
                        if let Some(terminal) = self.active_terminal() {
                            terminal.emit(VteInput::SearchClear);
                        }
                    }
                    let next_owner = if ti == self.active {
                        Some(pane_id)
                    } else {
                        self.active_pane_id()
                    };
                    let focus_transfer = self.begin_organism_focus_transfer(next_owner, false);
                    self.tabs[ti].active_pane = pi;
                    // Resolve from the selected tab/pane instead of trusting
                    // this possibly delayed event's pane. The activation gate
                    // also prevents focus queued before deactivation from
                    // reclaiming a live body while the window is inactive.
                    self.finish_organism_focus_transfer(focus_transfer);
                    if !focus_transfer {
                        // Even a same-pane notification must re-evaluate the
                        // activation gate: it may have been queued before the
                        // window changed state.
                        self.sync_organism_focus();
                    }
                    self.refresh_pane_headers(ti);
                    // The tab shows its selected pane, so the label follows
                    // focus across a split.
                    self.retitle_tab_from_active_pane(ti, &sender);
                    if self.tabs[ti].bell || self.tabs[ti].activity {
                        self.tabs[ti].bell = false;
                        self.tabs[ti].activity = false;
                        self.sync_tab_strip();
                    }
                    self.refresh_bottom_bar();
                    if active_pane_changed {
                        self.search.emit(search::SearchMsg::ActivePaneChanged);
                    }
                }
            }
            AppMsg::SwapPanes { dragged, target } => self.swap_panes(dragged, target),
            AppMsg::MoveTabToPane {
                tab_id,
                target_pane_id,
                edge,
            } => self.move_tab_to_pane(tab_id, target_pane_id, edge, &sender),
            AppMsg::TabDragStarted {
                source_tab_id,
                drag_id,
            } => {
                if self
                    .tab_drag_coordinator
                    .drag_is_current(source_tab_id, drag_id)
                    && self.index_of(source_tab_id).is_some()
                {
                    self.tab_drag_origin = self
                        .tabs
                        .get(self.active)
                        .map(|tab| (source_tab_id, tab.id, drag_id));
                }
            }
            AppMsg::TabDragEnded {
                source_tab_id,
                drag_id,
            } => {
                if self
                    .tab_drag_origin
                    .is_some_and(|(tracked_source_id, _, tracked_drag_id)| {
                        tracked_source_id == source_tab_id && tracked_drag_id == drag_id
                    })
                {
                    let (_, original_active_id, _) = self
                        .tab_drag_origin
                        .take()
                        .expect("matching drag identity was present");
                    self.tab_drag_coordinator.invalidate_hover();
                    if self.index_of(source_tab_id).is_some()
                        && self.index_of(original_active_id).is_some()
                    {
                        self.select_tab(original_active_id, &sender);
                    }
                }
            }
            AppMsg::PreviewTabDrop {
                source_tab_id,
                target_tab_id,
                drag_id,
                hover_generation,
            } => {
                if self
                    .tab_drag_origin
                    .is_some_and(|(tracked_source_id, _, tracked_drag_id)| {
                        tracked_source_id == source_tab_id && tracked_drag_id == drag_id
                    })
                    && self.tab_drag_coordinator.hover_is_current(
                        source_tab_id,
                        drag_id,
                        hover_generation,
                    )
                    && self.can_preview_tab_drop(source_tab_id, target_tab_id)
                {
                    self.select_tab(target_tab_id, &sender);
                }
            }
            AppMsg::PromotePaneToTab {
                pane_id,
                anchor_tab_id,
                after,
            } => self.promote_pane_to_tab(pane_id, anchor_tab_id, after, &sender),
            AppMsg::RefreshPaneHeaders => {
                self.poll_active_ssh_file_tree(&sender);
                self.refresh_active_pane_headers();
                // The bar's running-command and grid segments are polled too.
                self.refresh_bottom_bar();
                // Persistence workers cannot touch GTK. Surface their bounded
                // failure queue here so a session-save error is visible before
                // the user closes the window.
                self.report_persistence_failures();
            }
            AppMsg::TitleChanged(pane_id, title) => {
                if let Some((idx, pane_index)) = self.find_pane(pane_id) {
                    self.tabs[idx].panes[pane_index].title =
                        (!title.is_empty()).then(|| title.clone());
                    if self.tabs[idx].panes.len() > 1 {
                        self.refresh_pane_headers(idx);
                    }
                    let id = self.tabs[idx].id;
                    // A tab shows its selected pane. A background pane in a
                    // split reports OSC titles too, and letting those through
                    // made the label name a pane the user is not looking at.
                    let is_selected_pane = pane_index == self.tabs[idx].active_pane;
                    if is_selected_pane && !self.tabs[idx].custom_title && !title.is_empty() {
                        let filter = self.tab_filter.to_lowercase();
                        let was_visible = filter.is_empty()
                            || self.tabs[idx]
                                .display_title()
                                .to_lowercase()
                                .contains(&filter);
                        self.tabs[idx].title = title;
                        let is_visible = filter.is_empty()
                            || self.tabs[idx]
                                .display_title()
                                .to_lowercase()
                                .contains(&filter);
                        // A filter membership change really does alter the row
                        // set. Otherwise update only the label: OSC-title
                        // spinners can arrive many times per second.
                        if was_visible != is_visible
                            || (is_visible && !self.update_tab_title_widget(id))
                        {
                            self.rebuild_tab_strip(&sender);
                        }
                    }
                }
            }
            AppMsg::Bell(pane_id) => {
                if let Some((idx, pane_index)) = self.find_pane(pane_id) {
                    if idx != self.active || pane_index != self.tabs[idx].active_pane {
                        self.tabs[idx].bell = true;
                        self.sync_tab_strip();
                    }
                }
            }
            AppMsg::Activity(pane_id) => {
                if let Some((idx, pane_index)) = self.find_pane(pane_id) {
                    let mut changed = self.mark_remote_connected(idx, pane_id);
                    if (idx != self.active || pane_index != self.tabs[idx].active_pane)
                        && !self.tabs[idx].activity
                    {
                        self.tabs[idx].activity = true;
                        changed = true;
                    }
                    if changed {
                        self.sync_tab_strip();
                    }
                }
            }
            AppMsg::SettingsTheme(idx) => self.apply_settings_theme(idx),
            AppMsg::SettingsFontDesc(desc) => self.apply_settings_font_desc(desc),
            AppMsg::SettingsFontScale(scale) => self.apply_settings_font_scale(scale),
            AppMsg::PersistFontScale => self.persist_config(),
            AppMsg::SettingsOpacity(opacity) => self.apply_settings_opacity(opacity),
            AppMsg::SettingsScrollback(lines) => self.apply_settings_scrollback(lines),
            AppMsg::SettingsTerminalMode(mode) => self.apply_settings_terminal_mode(mode),
            AppMsg::SettingsBlockCompact(enabled) => self.apply_settings_block_compact(enabled),
            AppMsg::SettingsCommandHistory(enabled) => self.apply_settings_command_history(enabled),
            AppMsg::SettingsAsciiOrganism(enabled) => self.apply_settings_ascii_organism(enabled),
            AppMsg::SettingsAsciiOrganismMotion(motion) => {
                self.apply_settings_ascii_organism_motion(motion)
            }
            AppMsg::SettingsAiEnabled(enabled) => self.apply_settings_ai_enabled(enabled),
            AppMsg::SettingsAiPanelVisible(visible) => {
                self.apply_settings_ai_panel_visible(visible)
            }
            AppMsg::SettingsAiPanelWidth(width) => self.apply_settings_ai_panel_width(width),
            AppMsg::SettingsAgentEnabled(enabled) => self.apply_settings_agent_enabled(enabled),
            AppMsg::SettingsCommandCorrection(enabled) => {
                self.apply_settings_command_correction(enabled)
            }
            AppMsg::SettingsAiProvider(provider) => self.apply_settings_ai_provider(provider),
            AppMsg::SettingsAiModel(model) => self.apply_settings_ai_model(model),
            AppMsg::SettingsAiKeyFile(path) => self.apply_settings_ai_key_file(path),
            AppMsg::SettingsAiBaseUrl(base_url) => self.apply_settings_ai_base_url(base_url),
            AppMsg::SettingsAiMaxTokens(max_tokens) => {
                self.apply_settings_ai_max_tokens(max_tokens)
            }
            AppMsg::SettingsAiRedactSecrets(enabled) => {
                self.apply_settings_ai_redact_secrets(enabled)
            }
            AppMsg::SettingsAiStream(enabled) => self.apply_settings_ai_stream(enabled),
            AppMsg::SettingsAgentMaxTurns(turns) => self.apply_settings_agent_max_turns(turns),
            AppMsg::SettingsNotifications(enabled) => self.apply_settings_notifications(enabled),
            AppMsg::SettingsRemoteClipboard(enabled) => {
                self.apply_settings_remote_clipboard(enabled)
            }
            AppMsg::SettingsRemoteHosts(hosts) => self.apply_settings_remote_hosts(hosts, &sender),
            AppMsg::SearchChanged(text) => {
                if let Some(t) = self.active_terminal() {
                    if text.is_empty() {
                        t.emit(VteInput::SearchClear);
                    } else {
                        let (query, use_regex) = Self::search_query(&text);
                        t.emit(VteInput::SearchSet(query, use_regex));
                    }
                }
            }
            AppMsg::SearchNext => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::SearchNext);
                }
            }
            AppMsg::SearchPrev => {
                if let Some(t) = self.active_terminal() {
                    t.emit(VteInput::SearchPrev);
                }
            }
            AppMsg::SearchClose => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.emit(VteInput::SearchClear);
                    terminal.emit(VteInput::GrabFocus);
                }
            }
            AppMsg::SearchStatus(pane_id, status) => {
                let is_active_pane = self
                    .tabs
                    .get(self.active)
                    .and_then(|tab| tab.panes.get(tab.active_pane))
                    .is_some_and(|pane| pane.id == pane_id);
                if is_active_pane {
                    self.search.emit(search::SearchMsg::Status(status));
                }
            }
            AppMsg::RenameTab(id, title) => {
                if let Some(idx) = self.index_of(id) {
                    let trimmed = title.trim();
                    if trimmed.is_empty() {
                        self.tabs[idx].custom_title = false;
                        let number = idx as u32 + 1;
                        let cwd = self.tabs[idx]
                            .panes
                            .get(self.tabs[idx].active_pane)
                            .and_then(|p| p.cwd.clone());
                        self.tabs[idx].title = default_tab_title(number, cwd.as_deref());
                    } else {
                        self.tabs[idx].title = trimmed.to_string();
                        self.tabs[idx].custom_title = true;
                    }
                    self.rebuild_tab_strip(&sender);
                }
            }
            AppMsg::ReorderTab(src_id, to_idx) => self.reorder_tab(src_id, to_idx, &sender),
            AppMsg::TabRowAction(id, action) => {
                // Privacy is a chrome-only action. Do not activate an inactive
                // tab just to hide its title: that would reveal the very
                // terminal content the user is trying to keep in the background.
                if matches!(action, tab_strip::TabAction::TogglePrivateTitle) {
                    if let Some(idx) = self.index_of(id) {
                        self.tabs[idx].private_title = !self.tabs[idx].private_title;
                        self.show_toast(if self.tabs[idx].private_title {
                            "Tab title details hidden"
                        } else {
                            "Tab title details visible"
                        });
                        self.rebuild_tab_strip(&sender);
                    }
                    return;
                }
                self.select_tab(id, &sender);
                let action = match action {
                    tab_strip::TabAction::Duplicate => Action::DuplicateTab,
                    tab_strip::TabAction::ToggleMarked => Action::ToggleTabMarked,
                    tab_strip::TabAction::TogglePinned => Action::ToggleTabPinned,
                    tab_strip::TabAction::TogglePrivateTitle => unreachable!(),
                };
                self.execute_action(action, &sender);
            }
            AppMsg::SetTabFilter(text) => {
                self.tab_filter = text;
                self.sync_tab_strip();
            }
            AppMsg::FileTreeActivateFile(path) => {
                if let Some(path) = path.to_str() {
                    let snippet = format!("{} ", process::shell_quote_path(path));
                    self.insert_review_text(&snippet);
                } else {
                    self.show_toast(
                        "File path contains non-UTF-8 bytes and cannot be inserted safely.",
                    );
                }
            }
            AppMsg::OpenNotebook(path) => {
                if self.safe_mode {
                    self.show_toast("Notebooks are unavailable in safe mode.");
                } else {
                    self.notebook.emit(notebook::NotebookMsg::Open(path));
                }
            }
            AppMsg::OpenAgent => self.open_agent_panel(&sender),
            AppMsg::OpenAgentSettings => self.open_agent_settings(&sender),
            AppMsg::AgentSend(text) => self.agent_send(text, &sender),
            AppMsg::AgentStopRequest => self.agent_stop_request(),
            AppMsg::AgentRetryRequest => self.agent_retry_request(&sender),
            AppMsg::AgentContinue => self.agent_continue(),
            AppMsg::AgentNewTask => self.agent_new_task(),
            AppMsg::AgentAttachContext => self.agent_attach_context(),
            AppMsg::AgentClearContext => self.agent_clear_context(),
            AppMsg::AgentEditAndApprove(reference, new_cmd) => {
                self.agent_approve(reference, Some(new_cmd), &sender);
            }
            AppMsg::AgentInsert(reference, command) => {
                self.agent_insert_for_manual_review(reference, command);
            }
            AppMsg::AgentReject(reference) => self.agent_reject(reference, &sender),
            AppMsg::AgentRefreshPrompt(epoch) => self.agent_refresh_prompt(epoch),
            AppMsg::AgentLlmReply { epoch, reply } => {
                self.agent_handle_reply(epoch, reply, &sender);
            }
            AppMsg::AgentBlockFinished {
                tab_id: _,
                pane_id,
                command,
                exit_code,
                completion_provenance,
                output_sample,
                agent_execution,
                duration_ms,
            } => {
                if let Some((tab_index, pane_index)) = self.find_pane(pane_id) {
                    let tab_id = self.tabs[tab_index].id;
                    let reported_exit = crate::block_view::exit_code_from_shared_surface(exit_code);
                    {
                        let pane = &mut self.tabs[tab_index].panes[pane_index];
                        pane.last_exit = reported_exit;
                        pane.last_duration_ms = duration_ms;
                    }
                    self.refresh_bottom_bar();
                    self.pin_command_suggestion(pane_id);
                    self.maybe_start_command_correction(
                        pane_id,
                        crate::command_correction::FinishedBlock {
                            command: command.clone(),
                            exit_code: reported_exit,
                            output: output_sample.clone(),
                            agent_issued: agent_execution.is_some(),
                            completion_provenance,
                        },
                        &sender,
                    );
                    self.agent_handle_block_finished(
                        agent_ops::AgentBlockCompletion {
                            tab_id,
                            pane_id,
                            command,
                            exit_code,
                            output: output_sample,
                            agent_execution,
                        },
                        &sender,
                    );
                }
            }
            AppMsg::AgentExecutionStartFailed { execution } => {
                self.agent_execution_start_failed(execution);
            }
            AppMsg::AgentClose => self.agent_close(),
            AppMsg::PaletteTypeCommand(cmd) => {
                self.insert_review_text(&cmd);
            }
            AppMsg::PaletteAskAi(query) => {
                self.handle_palette_ask_ai(query, &sender);
            }
            AppMsg::PaletteSuggestionReply {
                generation,
                request_id,
                reply,
            } => {
                self.command_suggestion_reply(generation, request_id, reply, &sender);
            }
            AppMsg::PaletteSuggestionStop(generation) => {
                self.stop_command_suggestion(generation);
            }
            AppMsg::PaletteSuggestionRetry(generation) => {
                self.start_command_suggestion(generation, &sender);
            }
            AppMsg::PaletteSuggestionInsert(generation) => {
                self.insert_command_suggestion(generation);
            }
            AppMsg::PaletteSuggestionDismiss(generation) => {
                self.close_command_suggestion_generation(generation);
            }
            AppMsg::CommandCorrectionLocalReply {
                pane_id,
                generation,
                candidate,
            } => {
                self.command_correction_local_reply(pane_id, generation, candidate, &sender);
            }
            AppMsg::CommandCorrectionAiReply {
                pane_id,
                generation,
                reply,
            } => {
                self.command_correction_ai_reply(pane_id, generation, reply, &sender);
            }
            AppMsg::CommandCorrectionAccept {
                pane_id,
                generation,
            } => self.accept_command_correction(pane_id, generation),
            AppMsg::CommandCorrectionTimeout {
                pane_id,
                generation,
            } => self.command_correction_timeout(pane_id, generation),
            AppMsg::CommandCorrectionDismiss {
                pane_id,
                generation,
            } => self.dismiss_command_correction(pane_id, generation),
            AppMsg::OpenAiPanel => {
                self.show_ai_session_panel();
            }
            AppMsg::AskAiAboutBlock(context, intent) => {
                self.show_ai_session_panel_with_context(Some(context), intent);
            }
            AppMsg::FixBlockWithAgent(pane_id) => {
                self.fix_block_with_agent(pane_id, &sender);
            }
            AppMsg::AiConversationSnapshot(snapshot) => {
                self.ai_conversation = Some(snapshot);
                self.persist_session();
            }
            AppMsg::AiPanelCloseRequested => {
                self.set_ai_panel_visible(false, true);
            }
            AppMsg::AiPanelWidthChanged(width) => {
                let width = width.clamp(MIN_AI_PANEL_WIDTH, MAX_AI_PANEL_WIDTH);
                if self.config.borrow().ai_panel_width == width {
                    return;
                }
                self.config.borrow_mut().ai_panel_width = width;
                let generation = self.ai_panel_width_generation.get().wrapping_add(1);
                self.ai_panel_width_generation.set(generation);
                let sender = sender.clone();
                let token = self.ai_panel_width_generation.clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(300),
                    move || {
                        if token.get() == generation {
                            sender.input(AppMsg::PersistAiPanelWidth(generation));
                        }
                    },
                );
            }
            AppMsg::PersistAiPanelWidth(generation) => {
                if self.ai_panel_width_generation.get() == generation {
                    self.persist_config();
                }
            }
            AppMsg::PaletteRunWorkflow(path) => {
                self.run_workflow_from_path(path, &sender);
            }
            AppMsg::FileTreeGotoCwd => self.file_tree_goto_current_cwd(&sender),
            AppMsg::FileTreeGoUp => self.file_tree_go_up(&sender),
            AppMsg::FileTreeGoHome => self.file_tree_go_home(&sender),
            AppMsg::FileTreeGoBack => self.file_tree_go_back(&sender),
            AppMsg::FileTreeGoForward => self.file_tree_go_forward(&sender),
            AppMsg::FileTreeNavigatePath(path) => self.file_tree_navigate_path(path, &sender),
            AppMsg::FileTreePathEntered(path) => self.file_tree_path_entered(path, &sender),
            AppMsg::FileTreeOpenPathEntry => self.file_tree_open_path_entry(),
            AppMsg::FileTreeTtlTick => self.file_tree_revalidate_due(),
            AppMsg::FileTreeEnterDirectory { path, intent } => {
                self.file_tree_enter_directory(path, *intent, &sender)
            }
            AppMsg::FileTreeSelectLocation(index) => self.file_tree_select_location(index, &sender),
            AppMsg::FileTreeLocationResolved {
                token,
                location,
                hosts,
                result,
            } => self.file_tree_location_resolved(token, location, hosts, result),
            AppMsg::FileTreeNavigationResolved {
                navigation,
                listing,
            } => self.file_tree_navigation_resolved(*navigation, listing),
            AppMsg::FileTreeHomeResolved {
                token,
                intent,
                start,
            } => self.file_tree_home_resolved(token, *intent, start, &sender),
            AppMsg::FileTreeSshProbeResolved {
                pane_id,
                token,
                start,
            } => self.file_tree_ssh_probe_resolved(pane_id, token, start, &sender),
            AppMsg::FileTreeSshRetry { pane_id, token } => {
                self.retry_file_tree_ssh_follow(pane_id, token, &sender)
            }
            AppMsg::FileTreeNewFile { dir, intent } => {
                self.file_tree_prompt_new(dir, false, *intent, &sender)
            }
            AppMsg::FileTreeNewFolder { dir, intent } => {
                self.file_tree_prompt_new(dir, true, *intent, &sender)
            }
            AppMsg::FileTreeRename { path, intent } => {
                self.file_tree_prompt_rename(path, *intent, &sender)
            }
            AppMsg::FileTreeDelete { paths, intent } => {
                self.file_tree_confirm_delete(paths, *intent, &sender)
            }
            AppMsg::FileTreeCopy { items, intent } => {
                self.file_tree_clipboard_set(items, false, *intent)
            }
            AppMsg::FileTreeCut { items, intent } => {
                self.file_tree_clipboard_set(items, true, *intent)
            }
            AppMsg::FileTreePaste {
                dir,
                intent,
                clipboard_token,
            } => self.file_tree_paste(dir, *intent, clipboard_token, &sender),
            AppMsg::FileTreeImportPaths { paths, dir, intent } => {
                self.file_tree_import_paths(paths, dir, *intent, &sender)
            }
            AppMsg::FileTreeRefresh { intent } => self.file_tree_refresh(*intent),
            AppMsg::FileTreeRefreshDirs { dirs, intent } => {
                self.file_tree_refresh_dirs(dirs, *intent)
            }
            AppMsg::FileTreeRetry(target) => self.file_tree_retry(target, &sender),
            AppMsg::FileTreeOpenTerminal { intent } => {
                self.file_tree_open_terminal(*intent, &sender)
            }
            AppMsg::FileTreeFilterChanged(query) => self.file_tree_apply_filter(&query),
            AppMsg::FileTreeShowHiddenChanged(show) => self.file_tree_set_show_hidden(show),
            AppMsg::FileTreeOpSucceeded {
                dirs,
                intent,
                transfer_id,
            } => self.file_tree_op_succeeded(dirs, *intent, transfer_id),
            AppMsg::FileTreeCreateNamed {
                dir,
                name,
                is_dir,
                intent,
            } => self.file_tree_create_named(dir, name, is_dir, *intent, &sender),
            AppMsg::FileTreeRenameNamed { src, name, intent } => {
                self.file_tree_rename_named(src, name, *intent, &sender)
            }
            AppMsg::FileTreeDeleteConfirmed { paths, intent } => {
                self.file_tree_delete_confirmed(paths, *intent, &sender)
            }
            AppMsg::SetSidebarView(view) => self.apply_sidebar_view(view, true),
            AppMsg::Ignore => {}
        }
    }
}

/// Anvil's durability lane for `jterm_core::organism_memory`.
///
/// Core decides *what* an organism-memory update is and *when* to ask; the app
/// decides *how* it reaches the disk. Routing it through `persistence` is the
/// whole point of registering: the shared worker coalesces two pending writes
/// to the same memory file into one, charges them against the same admission
/// budget as history and session snapshots, and is drained by the same shutdown
/// accounting. Core's unregistered fallback is a correct, bounded writer thread
/// of its own, so nothing breaks without this — the organism still remembers —
/// which is exactly why the registration below is asserted by a test instead of
/// left to a compile error.
struct OrganismLane;

impl jterm_core::organism_memory::MemoryScheduler for OrganismLane {
    fn schedule(&self, write: jterm_core::organism_memory::MemoryWrite) -> std::io::Result<()> {
        // Copy the coalescing key out of `write` before the closure takes it,
        // so the borrow ends before the move.
        let key = persistence::PersistenceKey::for_path(write.kind(), write.path());
        let operation = write.operation();
        persistence::enqueue(key, operation, move || write.run())
    }
}

fn main() {
    // Freeze the inherited environment before anything can mutate it: CLI
    // parsing writes ANVIL_CONFIG below, input-method setup writes
    // GTK_PATH/GTK_IM_MODULE/XMODIFIERS, and GTK itself starts threads. Every
    // spawn path builds the child environment from this snapshot, so a capture
    // after any of those would leak frontend-private variables to the shell.
    // A failure here is an initialization-ordering bug, not a runtime error.
    if let Err(error) = child_env::capture_inherited_environment() {
        eprintln!("anvil: {error}");
        std::process::exit(1);
    }
    jterm_core::identity::init(jterm_core::identity::AppIdentity {
        app_name: host::APP_NAME,
        app_id: host::APP_ID,
        // This crate's version, not jterm_core's: shared code reports it to
        // child shells as TERM_PROGRAM_VERSION, and a tool that feature-gates
        // on the TERM_PROGRAM/version pair would otherwise read the core
        // library's version paired with our name.
        app_version: env!("CARGO_PKG_VERSION"),
    });
    // Beside `identity::init` and before the first `OrganismMemory::load`: a
    // lane cannot be swapped underneath writes that are already in flight, and
    // an unregistered process silently writes through core's own thread instead
    // of ours. Every `main` exit path below shares this line, including the
    // subcommands that never open a window, so no route can lose the lane.
    jterm_core::organism_memory::init_scheduler(Box::new(OrganismLane));
    let parsed = match cli::parse(std::env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("anvil: {error}\nTry 'anvil --help' for usage.");
            std::process::exit(2);
        }
    };

    if let Some(path) = parsed.config_path {
        // SAFETY: no GTK runtime, worker thread, or configuration read exists
        // yet. The inherited-environment freeze already ran above, so this
        // process-only variable stays out of every spawned child.
        unsafe { std::env::set_var("ANVIL_CONFIG", path) };
    }

    match parsed.command {
        cli::Command::Help => print!("{}", cli::HELP),
        cli::Command::Version => println!("anvil {}", env!("CARGO_PKG_VERSION")),
        cli::Command::Doctor(format) => {
            init_logging();
            if !run_doctor(format) {
                std::process::exit(1);
            }
        }
        cli::Command::CheckConfig(path, format) => {
            init_logging();
            let path = path.unwrap_or_else(config_file_path);
            if !config_store::run_check_path(&path, format) {
                std::process::exit(1);
            }
        }
        cli::Command::RestoreConfigBackup => {
            init_logging();
            match config_store::restore_backup() {
                Ok((source, _revision)) => println!(
                    "Restored {} from {}",
                    config_file_path().display(),
                    source.display()
                ),
                Err(error) => {
                    eprintln!("anvil: {error}");
                    std::process::exit(1);
                }
            }
        }
        cli::Command::ConfigPath => println!("{}", config_file_path().display()),
        cli::Command::InitConfig => {
            if let Err(error) = init_config_file() {
                eprintln!("anvil: {error}");
                std::process::exit(1);
            }
        }
        cli::Command::PrintDefaultConfig => print!("{}", include_str!("../config.toml.example")),
        cli::Command::PrintShellIntegration(shell) => print_shell_integration(shell),
        cli::Command::PrintCompletion(shell) => print_completion(shell),
        cli::Command::Run(mut options) => {
            if let Err(error) = validate_launch_options(&mut options) {
                eprintln!("anvil: {error}");
                std::process::exit(2);
            }
            init_logging();
            init_input_method_env();
            // NON_UNIQUE: each launch is its own process with its own window.
            // Session persistence uses per-process snapshots so instances do
            // not overwrite one another.
            let app = RelmApp::from_app(
                adw::Application::builder()
                    .application_id(host::APP_ID)
                    .flags(gio::ApplicationFlags::NON_UNIQUE)
                    .build(),
            )
            // anvil has already parsed its command line. Passing only argv[0]
            // prevents GApplication from rejecting our launch options as
            // unknown GTK options during its second-stage initialization.
            .with_args(vec!["anvil".to_string()]);
            app.run::<AppModel>(options);
        }
    }
}

fn init_logging() {
    let filter = std::env::var("ANVIL_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "warn".to_string());
    let mut builder = env_logger::Builder::new();
    builder.parse_filters(&filter).format_timestamp_millis();
    let _ = builder.try_init();
}

fn validate_launch_options(options: &mut cli::LaunchOptions) -> Result<(), String> {
    if let Some(directory) = options.working_directory.as_mut() {
        let canonical = std::fs::canonicalize(&*directory)
            .map_err(|err| format!("cannot open directory {}: {err}", directory.display()))?;
        if !canonical.is_dir() {
            return Err(format!("{} is not a directory", canonical.display()));
        }
        if canonical.to_str().is_none() {
            return Err(format!(
                "working directory {} contains non-UTF-8 bytes and cannot be passed to the terminal safely",
                canonical.display()
            ));
        }
        *directory = canonical;
    }
    if let Some(argv) = &options.execute {
        let executable = argv.first().expect("CLI parser rejects empty commands");
        let path = std::path::Path::new(executable);
        let found = if path.components().count() > 1 {
            path.is_file()
        } else {
            host::find_executable_in_path(executable).is_some()
        };
        if !found {
            return Err(format!("command not found: {executable}"));
        }
    }
    Ok(())
}

fn init_config_file() -> Result<(), String> {
    use std::io::Write;

    let path = config_file_path();
    config_store::ensure_config_parent(&path).map_err(|err| err.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let mut file = options.open(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} already exists; it was not overwritten", path.display())
        } else {
            format!("cannot create {}: {err}", path.display())
        }
    })?;
    file.write_all(include_str!("../config.toml.example").as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    drop(file);
    config_store::sync_config_parent(&path).map_err(|err| err.to_string())?;
    println!("Created {}", path.display());
    Ok(())
}

fn print_shell_integration(shell: cli::ShellIntegration) {
    let script = match shell {
        cli::ShellIntegration::Bash => include_str!("../scripts/shell-integration/anvil.bash"),
        cli::ShellIntegration::Zsh => include_str!("../scripts/shell-integration/anvil.zsh"),
        cli::ShellIntegration::Fish => include_str!("../scripts/shell-integration/anvil.fish"),
        cli::ShellIntegration::PowerShell => {
            include_str!("../scripts/shell-integration/anvil.ps1")
        }
    };
    print!("{script}");
}

fn print_completion(shell: cli::ShellIntegration) {
    let script = match shell {
        cli::ShellIntegration::Bash => include_str!("../scripts/completions/anvil.bash"),
        cli::ShellIntegration::Zsh => include_str!("../scripts/completions/_anvil"),
        cli::ShellIntegration::Fish => include_str!("../scripts/completions/anvil.fish"),
        cli::ShellIntegration::PowerShell => include_str!("../scripts/completions/anvil.ps1"),
    };
    print!("{script}");
}

fn run_doctor(format: cli::ReportFormat) -> bool {
    diagnostics::run(format)
}

/// Make the fcitx5 GTK4 input-method module discoverable before GTK initializes,
/// so CJK (Chinese) preedit/commit works even when the binary is launched
/// outside the nix dev shell.
///
/// The flake bakes the module directory into `FCITX5_GTK_PATH` at build time. The
/// nix-built GTK4 only searches its own store path plus `GTK_PATH`, so without
/// re-exporting that directory it never finds `libim-fcitx5.so` and IME silently
/// falls back to raw keysyms. Each var is only filled in when the environment
/// hasn't already set it, so an existing (e.g. ibus) setup is left untouched.
fn init_input_method_env() {
    let is_unset = |k: &str| std::env::var_os(k).is_none_or(|v| v.is_empty());

    if let Some(fcitx_gtk_path) = option_env!("FCITX5_GTK_PATH") {
        if !fcitx_gtk_path.is_empty() {
            let combined = match std::env::var_os("GTK_PATH") {
                Some(existing) if !existing.is_empty() => {
                    format!("{fcitx_gtk_path}:{}", existing.to_string_lossy())
                }
                _ => fcitx_gtk_path.to_string(),
            };
            unsafe { std::env::set_var("GTK_PATH", combined) };
        }
    }
    if is_unset("GTK_IM_MODULE") {
        unsafe { std::env::set_var("GTK_IM_MODULE", "fcitx") };
    }
    if is_unset("XMODIFIERS") {
        unsafe { std::env::set_var("XMODIFIERS", "@im=fcitx") };
    }
}

#[cfg(test)]
mod cross_block_search_key_latch_tests {
    use super::*;

    #[test]
    fn opening_toggle_dispatches_once_per_physical_key_press() {
        let mut latch = CrossBlockSearchKeyLatch::default();
        assert_eq!(
            latch.press(42, true),
            CrossBlockSearchKeyPress::DispatchToggle
        );
        assert_eq!(
            latch.press(42, true),
            CrossBlockSearchKeyPress::SuppressHeldRepeat,
            "auto-repeat must not close the dialog opened by the first edge"
        );
        assert_eq!(
            latch.press(42, false),
            CrossBlockSearchKeyPress::SuppressHeldRepeat,
            "dropping Ctrl/Shift mid-hold must not leak the opener into the query"
        );
        assert_eq!(
            latch.press(7, false),
            CrossBlockSearchKeyPress::Proceed,
            "an unrelated physical key remains available"
        );

        latch.release(7);
        assert_eq!(
            latch.press(42, true),
            CrossBlockSearchKeyPress::SuppressHeldRepeat,
            "only the opening physical key's release clears its guard"
        );
        latch.release(42);
        assert_eq!(
            latch.press(42, true),
            CrossBlockSearchKeyPress::DispatchToggle,
            "a new physical press may intentionally close the dialog"
        );
    }

    #[test]
    fn deactivation_recovers_a_lost_toggle_release() {
        let mut latch = CrossBlockSearchKeyLatch::default();
        assert_eq!(
            latch.press(99, true),
            CrossBlockSearchKeyPress::DispatchToggle
        );
        latch.reset();
        assert_eq!(
            latch.press(99, true),
            CrossBlockSearchKeyPress::DispatchToggle
        );
    }
}

#[cfg(test)]
mod file_tree_f5_tests {
    use super::*;

    #[test]
    fn plain_f5_refreshes_only_inside_the_visible_file_tree_scope() {
        let plain = gtk::gdk::ModifierType::empty();
        assert!(file_tree_f5_should_refresh(
            gtk::gdk::Key::F5,
            plain,
            true,
            false,
            true
        ));
        assert!(file_tree_f5_should_refresh(
            gtk::gdk::Key::F5,
            gtk::gdk::ModifierType::LOCK_MASK,
            false,
            true,
            true
        ));
        assert!(!file_tree_f5_should_refresh(
            gtk::gdk::Key::F5,
            plain,
            false,
            false,
            true
        ));
        assert!(!file_tree_f5_should_refresh(
            gtk::gdk::Key::F5,
            plain,
            true,
            true,
            false
        ));
    }

    #[test]
    fn modified_or_non_f5_keys_remain_available_to_the_terminal() {
        for modifiers in [
            gtk::gdk::ModifierType::CONTROL_MASK,
            gtk::gdk::ModifierType::SHIFT_MASK,
            gtk::gdk::ModifierType::ALT_MASK,
            gtk::gdk::ModifierType::SUPER_MASK,
            gtk::gdk::ModifierType::HYPER_MASK,
            gtk::gdk::ModifierType::META_MASK,
        ] {
            assert!(!file_tree_f5_should_refresh(
                gtk::gdk::Key::F5,
                modifiers,
                true,
                true,
                true
            ));
        }
        assert!(!file_tree_f5_should_refresh(
            gtk::gdk::Key::F6,
            gtk::gdk::ModifierType::empty(),
            true,
            true,
            true
        ));
    }

    #[test]
    fn alt_up_and_alt_home_navigate_only_in_the_visible_file_tree_scope() {
        let alt = gtk::gdk::ModifierType::ALT_MASK;
        assert_eq!(
            file_tree_navigation_shortcut(gtk::gdk::Key::Up, alt, true, false, true),
            Some(FileTreeNavigationShortcut::Up)
        );
        assert_eq!(
            file_tree_navigation_shortcut(
                gtk::gdk::Key::Home,
                alt | gtk::gdk::ModifierType::LOCK_MASK,
                true,
                false,
                true,
            ),
            Some(FileTreeNavigationShortcut::Home)
        );
        assert_eq!(
            file_tree_navigation_shortcut(gtk::gdk::Key::Up, alt, false, false, true),
            None
        );
        assert_eq!(
            file_tree_navigation_shortcut(gtk::gdk::Key::Home, alt, true, true, false),
            None
        );
    }

    #[test]
    fn history_and_path_shortcuts_are_scoped_to_the_visible_file_tree() {
        let alt = gtk::gdk::ModifierType::ALT_MASK;
        assert_eq!(
            file_tree_navigation_shortcut(gtk::gdk::Key::Left, alt, true, false, true),
            Some(FileTreeNavigationShortcut::Back)
        );
        assert_eq!(
            file_tree_navigation_shortcut(gtk::gdk::Key::Right, alt, true, false, true),
            Some(FileTreeNavigationShortcut::Forward)
        );
        assert_eq!(
            file_tree_navigation_shortcut(
                gtk::gdk::Key::l,
                gtk::gdk::ModifierType::CONTROL_MASK,
                true,
                false,
                true,
            ),
            Some(FileTreeNavigationShortcut::OpenPath)
        );
        assert_eq!(
            file_tree_navigation_shortcut(
                gtk::gdk::Key::l,
                gtk::gdk::ModifierType::CONTROL_MASK,
                false,
                false,
                true,
            ),
            None,
            "Ctrl+L must remain available to the terminal outside Files"
        );
    }

    #[test]
    fn pointer_hover_never_captures_terminal_navigation_chords() {
        let alt = gtk::gdk::ModifierType::ALT_MASK;
        for key in [
            gtk::gdk::Key::Left,
            gtk::gdk::Key::Right,
            gtk::gdk::Key::Up,
            gtk::gdk::Key::Home,
        ] {
            assert_eq!(
                file_tree_navigation_shortcut(key, alt, false, true, true),
                None
            );
        }
        assert_eq!(
            file_tree_navigation_shortcut(
                gtk::gdk::Key::l,
                gtk::gdk::ModifierType::CONTROL_MASK,
                false,
                true,
                true,
            ),
            None
        );
        assert!(
            file_tree_f5_should_refresh(
                gtk::gdk::Key::F5,
                gtk::gdk::ModifierType::empty(),
                false,
                true,
                true,
            ),
            "F5 deliberately retains the independent hover refresh policy"
        );
    }

    #[test]
    fn file_tree_navigation_does_not_capture_plain_or_conflicting_terminal_keys() {
        for state in [
            gtk::gdk::ModifierType::empty(),
            gtk::gdk::ModifierType::ALT_MASK | gtk::gdk::ModifierType::CONTROL_MASK,
            gtk::gdk::ModifierType::ALT_MASK | gtk::gdk::ModifierType::SHIFT_MASK,
            gtk::gdk::ModifierType::ALT_MASK | gtk::gdk::ModifierType::SUPER_MASK,
        ] {
            assert_eq!(
                file_tree_navigation_shortcut(gtk::gdk::Key::Up, state, true, true, true),
                None
            );
        }
        assert_eq!(
            file_tree_navigation_shortcut(
                gtk::gdk::Key::Left,
                gtk::gdk::ModifierType::empty(),
                true,
                true,
                true,
            ),
            None
        );
    }
}

#[cfg(test)]
mod organism_focus_tests {
    use super::*;

    #[test]
    fn organism_focus_requires_observed_and_current_window_activation() {
        assert_eq!(
            organism_focus_decision(true, true),
            OrganismFocusDecision::ClaimCurrentPane
        );
        assert_eq!(
            organism_focus_decision(true, false),
            OrganismFocusDecision::Revoke,
            "a pane-focus event queued before deactivation cannot reclaim presence"
        );
        assert_eq!(
            organism_focus_decision(false, true),
            OrganismFocusDecision::Revoke,
            "reactivation waits for its Relm4 activation message"
        );
        assert_eq!(
            organism_focus_decision(false, false),
            OrganismFocusDecision::Revoke
        );
    }

    #[test]
    fn workspace_focus_handoffs_cover_hidden_and_missing_destinations() {
        assert!(!organism_focus_transfer_required(Some(7), Some(7), false));
        assert!(organism_focus_transfer_required(Some(7), Some(8), false));
        assert!(organism_focus_transfer_required(Some(7), None, false));
        assert!(
            organism_focus_transfer_required(Some(7), Some(7), true),
            "reparenting the same pane still hides its old surface"
        );
        assert!(!organism_focus_transfer_required(None, None, false));
    }
}

#[cfg(test)]
mod startup_wiring_tests {
    /// The one adoption step no compiler error catches.
    ///
    /// `jterm_core::organism_memory` records commands whether or not this
    /// process registers a lane: with none, core writes through a bounded
    /// writer thread of its own. Deleting the registration therefore compiles,
    /// passes every other test, and looks correct from the outside — while
    /// every organism-memory write silently leaves anvil's persistence worker,
    /// and with it the coalescing that collapses two pending writes to one
    /// memory file, the shared admission budget, and the shutdown accounting
    /// that `flush_pending` and `persistence::shutdown` perform in that order.
    ///
    /// There is nothing to observe at runtime instead. `main` is not callable
    /// from a test binary, and `scheduler_is_registered` answers for whichever
    /// process asks — in this one, `main` never ran. So the guard is
    /// structural, as it is for frost's single flushing exit.
    #[test]
    fn organism_memory_writes_are_registered_onto_anvils_persistence_lane() {
        let source = include_str!("main.rs");
        // Built rather than written out, so this test's own source does not
        // count as one of the call sites it is looking for.
        let needle = format!("organism_memory::init_{}", "scheduler");
        let call_sites: Vec<&str> = source
            .lines()
            .map(str::trim)
            .filter(|line| line.contains(&needle) && !line.starts_with("//"))
            .collect();
        assert_eq!(
            call_sites,
            [format!("jterm_core::{needle}(Box::new(OrganismLane));")],
            "startup must register anvil's organism-memory lane exactly once"
        );

        // Inside `main`, after `identity::init`: a lane cannot be swapped
        // underneath writes already in flight, so the first registration is the
        // only one that counts and it has to precede every route that can load
        // organism memory.
        let main_body = source
            .split_once("\nfn main() {")
            .expect("the process entry point exists")
            .1;
        // Bounded to `main`'s own body: every block inside it is indented, so
        // the first closing brace in column zero is its own. Without this the
        // ordering below would also be satisfied by a registration parked in
        // some helper further down the file, which `main` need never call.
        let main_body = &main_body[..main_body.find("\n}\n").expect("main closes")];
        let identity_at = main_body
            .find("identity::init(")
            .expect("main registers the app identity");
        let lane_at = main_body
            .find(&needle)
            .expect("main registers the organism-memory lane");
        assert!(
            identity_at < lane_at,
            "the lane is registered beside identity::init, not before it"
        );

        // And the lane is anvil's persistence worker rather than a stub that
        // accepts writes and drops them, or one that runs the two cross-process
        // `flock`s inline on the GTK main thread.
        let lane = source
            .split_once("impl jterm_core::organism_memory::MemoryScheduler for OrganismLane {")
            .expect("the lane exists")
            .1;
        let lane = &lane[..lane.find("\nfn main() {").expect("the impl closes")];
        assert!(
            lane.contains("persistence::enqueue(key, operation, move || write.run())"),
            "the lane must hand the write to anvil's persistence worker"
        );
        assert!(
            lane.contains("PersistenceKey::for_path(write.kind(), write.path())"),
            "coalescing must key on core's own kind and path, so two pending \
             writes to one memory file collapse and unrelated ones never do"
        );
    }
}

#[cfg(all(test, unix))]
mod launch_validation_tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(std::path::PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn launch_validation_rejects_a_non_utf8_working_directory() {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "anvil-launch-validation-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("create launch-validation root");
        let _cleanup = TestDirectory(root.clone());

        let mut name = OsString::from("raw-");
        name.push(OsString::from_vec(vec![0xff]));
        let raw_directory = root.join(name);
        std::fs::create_dir(&raw_directory).expect("create non-UTF-8 directory");

        // A lossy conversion would redirect this raw path to a different UTF-8
        // directory when one happens to exist. Validation must fail before the
        // path reaches the terminal's String-only launch boundary.
        std::fs::create_dir(root.join("raw-�")).expect("create lossy replacement directory");
        let mut options = cli::LaunchOptions {
            working_directory: Some(raw_directory),
            ..cli::LaunchOptions::default()
        };

        let error = validate_launch_options(&mut options)
            .expect_err("non-UTF-8 launch directory must be rejected");
        assert!(error.contains("contains non-UTF-8 bytes"), "{error}");
    }
}
