//! VTE terminal backend as a relm4 Component.
//!
//! Wraps a `vte4::Terminal` + `gtk::Scrollbar` in a horizontal box. The shell
//! is spawned on init. VTE signals (cwd/exit/bell/title/activity) are forwarded
//! as component Output messages instead of forge's callback-Vec observer model.

use gtk::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk::gdk::ModifierType;
use gtk::gdk::RGBA;
use gtk::gio::{self, Cancellable};
use gtk::glib::translate::IntoGlib;
use gtk::glib::SpawnFlags;
use gtk::pango::FontDescription;
use gtk::prelude::*;
use gtk::GestureClick;
use gtk::Orientation;
use relm4::gtk;
use relm4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, PtyFlags, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::child_env;
use crate::config::Config;
use crate::search::{invalid_regex_message, SearchStatus};

// ─── Terminal widget construction (ported from forge terminal.rs) ──────────

pub(crate) fn create_terminal(config: &Config) -> Terminal {
    let font_scale = config.default_font_scale;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .name("term_name")
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::System)
        .cursor_shape(CursorShape::Block)
        .font_scale(font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();

    terminal.set_mouse_autohide(true);
    // Backspace must send DEL (0x7f), not BS (0x08); readline/most shells only
    // erase on DEL, so the default binding leaves Backspace unable to delete.
    terminal.set_backspace_binding(vte4::EraseBinding::AsciiDelete);
    terminal.set_delete_binding(vte4::EraseBinding::DeleteSequence);

    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));

    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));

    crate::block_view::add_url_match_regex(&terminal);

    terminal
}

/// Wrap a terminal in an hbox with a scrollbar on the right side.
pub(crate) fn wrap_with_scrollbar(terminal: &Terminal) -> gtk::Box {
    let hbox = gtk::Box::new(Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.add_css_class("terminal-box");
    let scrollbar = gtk::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
    hbox.append(terminal);
    hbox.append(&scrollbar);
    hbox
}

pub(crate) fn terminal_working_directory(terminal: &Terminal) -> Option<String> {
    if let Some(uri) = terminal.current_directory_uri() {
        let file = gio::File::for_uri(uri.as_str());
        if let Some(path) = file
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(path);
        }
    }
    let pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };
    jterm_core::process::process_cwd(pid)
}

pub(crate) fn default_tab_title(tab_index_1based: u32, working_directory: Option<&str>) -> String {
    let mut resolved_dir = working_directory
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    if resolved_dir.is_none() {
        resolved_dir = std::env::var("HOME").ok();
    }

    let Some(dir) = resolved_dir.as_deref() else {
        return format!("Terminal {tab_index_1based}");
    };

    let mut normalized = dir.trim_end_matches('/');
    if normalized.is_empty() {
        normalized = "/";
    }

    let home = std::env::var("HOME").ok();
    let display_dir = if let Some(home) = home.as_deref() {
        if normalized == home {
            "~".to_string()
        } else if let Some(rest) = normalized.strip_prefix(home) {
            if rest.starts_with('/') {
                format!("~{rest}")
            } else {
                normalized.to_string()
            }
        } else {
            normalized.to_string()
        }
    } else {
        normalized.to_string()
    };

    if display_dir == "/" || display_dir == "~" {
        return display_dir;
    }

    fn shorten_component(component: &str) -> String {
        if component.is_empty() {
            return String::new();
        }
        if component == "." || component == ".." {
            return component.to_string();
        }
        let mut chars = component.chars();
        let first = chars.next().unwrap();
        if first == '.' {
            if let Some(second) = chars.next() {
                let mut out = String::new();
                out.push(first);
                out.push(second);
                out
            } else {
                ".".to_string()
            }
        } else {
            first.to_string()
        }
    }

    let (prefix, rest) = if let Some(r) = display_dir.strip_prefix("~/") {
        ("~/", r)
    } else if let Some(r) = display_dir.strip_prefix('/') {
        ("/", r)
    } else {
        ("", display_dir.as_str())
    };

    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 1 {
        return format!("{prefix}{rest}");
    }

    let mut out_parts: Vec<String> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            out_parts.push((*part).to_string());
        } else {
            out_parts.push(shorten_component(part));
        }
    }

    format!("{prefix}{}", out_parts.join("/"))
}

/// Ctrl+Click on a hyperlink opens it; other clicks pass through to VTE selection.
pub(crate) fn setup_terminal_click_handler(terminal: &Terminal) {
    let click_controller = GestureClick::new();
    click_controller.set_button(GDK_BUTTON_PRIMARY as u32);
    click_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let terminal_clone = terminal.clone();
    click_controller.connect_pressed(move |controller, n_press, x, y| {
        if n_press == 1 {
            let state = controller.current_event_state();
            if state.contains(ModifierType::CONTROL_MASK) {
                if let Some(uri) = terminal_clone.check_match_at(x, y).0 {
                    super::url::open_uri(&uri);
                    controller.set_state(gtk::EventSequenceState::Claimed);
                    return;
                }
            }
        }
        controller.set_state(gtk::EventSequenceState::Denied);
    });
    terminal.add_controller(click_controller);
}

fn launch_failure_message(error: &impl std::fmt::Display) -> String {
    format!("Terminal failed to start: {error}. Check the shell, remote command, or host bridge.")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_shell(
    terminal: &Terminal,
    argv_owned: &[String],
    working_directory: Option<&str>,
    session_id: Option<&str>,
    cwd_token: &str,
    initial_commands: &[String],
    probe: PaneProbe,
    sender: ComponentSender<VteTerminal>,
) {
    let (argv_vec, _) = crate::config::shell_argv_with_session(argv_owned, session_id);
    let home = std::env::var("HOME").ok();
    let requested_working_directory = working_directory.or(home.as_deref());
    let effective_working_directory = requested_working_directory.filter(|directory| {
        let available = crate::host::working_directory_available(directory);
        if !available {
            log::warn!("VTE working directory is unavailable; using the application directory");
        }
        available
    });
    // The integration marker and cwd-authentication token must be encoded in
    // the host wrapper as well as VTE's environment. The shell integration
    // immediately removes the token from its exported environment.
    let host_environment = crate::terminal::cwd_token_environment(cwd_token);
    let argv_vec =
        crate::host::wrap_argv(&argv_vec, effective_working_directory, &host_environment);
    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // `vte_envv_from_captured` is the child's complete environment, built from
    // the launch-time snapshot `main` froze; the cwd-authentication token rides
    // along as the per-call extra. VTE_SPAWN_NO_PARENT_ENVV below stops libvte
    // from merging the live, GTK-mutated process environment back in, which
    // would reintroduce the frontend-private writes the freeze exists to
    // exclude (ANVIL_CONFIG and anvil's own input-method overrides; values the
    // user launched with stay frozen and intact). libvte's own TERM/VTE_VERSION
    // injection loses to explicit envv entries.
    let envv_owned: Vec<String> = match child_env::vte_envv_from_captured(
        &child_env::ChildEnv::from_identity(),
        &host_environment,
    ) {
        Ok(envv) => envv,
        Err(error) => {
            let message = launch_failure_message(&error);
            log::error!("{message}");
            terminal.feed(format!("\r\nanvil: {message}\r\n").as_bytes());
            let _ = sender.output(VteOutput::LaunchFailed(message));
            return;
        }
    };
    let envv: Vec<&str> = envv_owned.iter().map(String::as_str).collect();
    // gtk-rs predates VTE 0.60's VTE_SPAWN_NO_PARENT_ENVV, so OR the numeric
    // flag jterm_core pins into the ordinary GLib spawn flags. SEARCH_PATH
    // resolves argv[0] against the parent's live PATH while the child receives
    // the frozen one; the two cannot diverge today because nothing mutates
    // PATH after the capture.
    let spawn_flags = SpawnFlags::from_bits_retain(
        SpawnFlags::SEARCH_PATH.bits() | child_env::VTE_SPAWN_NO_PARENT_ENVV_BITS,
    );
    let cancellable: Option<&Cancellable> = None;
    let spawn_working_directory = if crate::host::is_flatpak() {
        None
    } else {
        effective_working_directory
    };
    let terminal_for_pid = terminal.clone();

    let init_cmds = initial_commands.to_vec();
    let terminal_for_init = terminal.clone();

    terminal.spawn_async(
        PtyFlags::DEFAULT,
        spawn_working_directory,
        &argv,
        &envv,
        spawn_flags,
        || {},
        -1,
        cancellable,
        move |res| {
            log::debug!("spawn_async: {res:?}");
            match res {
                Ok(pid) => {
                    let pid_i32: i32 = pid.into_glib();
                    unsafe {
                        terminal_for_pid.set_data::<i32>("child-pid", pid_i32);
                    }
                    probe.shell_pid.set(pid_i32);
                    if let Some(pty) = terminal_for_pid.pty() {
                        use std::os::fd::AsRawFd;
                        probe.pty_fd.set(pty.fd().as_raw_fd());
                    }
                    let _ = sender.output(VteOutput::Launched);
                }
                Err(error) => {
                    let message = launch_failure_message(&error);
                    log::error!("{message}");
                    let terminal_message = format!("\r\nanvil: {message}\r\n");
                    terminal_for_pid.feed(terminal_message.as_bytes());
                    let _ = sender.output(VteOutput::LaunchFailed(message));
                    return;
                }
            }
            if !init_cmds.is_empty() {
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(500),
                    move || {
                        for command in init_cmds {
                            let text = format!("{command}\r");
                            terminal_for_init.feed_child(text.as_bytes());
                        }
                    },
                );
            }
        },
    );
}

// ─── VteTerminal relm4 Component ────────────────────────────────────────────

/// Commands to submit after a terminal reaches its first prompt.
///
/// Configuration retains its historical comma-separated syntax, but it is
/// parsed once at the application boundary. Session restore instead constructs
/// exactly one safely quoted command from a persisted argv. Downstream terminal
/// backends therefore never reinterpret a restored command's commas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitialCommands(Vec<String>);

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

pub struct VteInit {
    pub config: Rc<RefCell<Config>>,
    /// Backend requested by the pane creator. Conventional VTE ignores this;
    /// the shared Block component passes it through to `TermView` so Unified
    /// is never accidentally re-derived from mutable global configuration.
    pub mode: crate::config::TerminalMode,
    pub shell_argv: Rc<Vec<String>>,
    pub working_directory: Option<String>,
    pub working_directory_external: bool,
    pub session_id: Option<String>,
    /// Per-pane secret consumed by the shell integration to authenticate OSC 7
    /// cwd updates even when a Flatpak host shell hides foreground processes.
    pub cwd_token: String,
    pub initial_commands: InitialCommands,
    pub probe: PaneProbe,
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
pub enum VteInput {
    WriteInput(Vec<u8>),
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
    /// Block-view only: switch existing cards and the live input cell between
    /// the normal and compact densities. Card margins are set imperatively, so
    /// a CSS reinstall alone cannot move them.
    ApplyBlockDensity(bool),
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
    /// Block-view only: put all selected commands back into the input editor.
    ReinputSelectedCommands,
    /// Block-view only: jump to the previous / next pinned block.
    JumpToPrevPinned,
    JumpToNextPinned,
    /// Block-view only: jump to the previous / next failed block.
    JumpToPrevFailed,
    JumpToNextFailed,
    /// Block-view only: write the whole session's blocks to a Markdown / JSON
    /// file under the anvil data directory.
    ExportSessionMarkdown,
    ExportSessionJson,
    /// Search: set the query and jump to the first match. `use_regex` treats the
    /// query as a regex; otherwise it is matched literally (case-insensitive).
    SearchSet(String, bool),
    SearchNext,
    SearchPrev,
    SearchClear,
    /// Block-view only: open the flat ripgrep-style search over completed blocks.
    CrossBlockSearch,
    /// Block-view only: snapshot the selected finished block and forward it to
    /// the application AI panel. Plain VTE panes ignore this action.
    AskAiAboutSelectedBlock,
}

/// What a [`VteOutput::NoticeWithUndo`] button takes back, in the pane that
/// raised it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeUndo {
    ClearBlocks,
}

#[derive(Debug)]
pub enum VteOutput {
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
    /// A [`VteOutput::Notice`] whose action can be taken back. The application
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
    /// A finished block, with the full reconstructed command + exit + a
    /// captured-output sample. Drives agent-mode's run-observe loop. Emitted
    /// by BlockTerminal only (the plain VTE wrapper has no block concept).
    BlockFinished {
        command: String,
        exit_code: i32,
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
    AskAiAboutBlock(crate::ai::BlockContext),
}

pub struct VteTerminal {
    terminal: Terminal,
    config: Rc<RefCell<Config>>,
    search_status: SearchStatus,
}

impl VteTerminal {
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }
}

impl Component for VteTerminal {
    type Init = VteInit;
    type Input = VteInput;
    type Output = VteOutput;
    type CommandOutput = ();
    type Root = gtk::Box;
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk::Box::new(Orientation::Horizontal, 0)
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let terminal = create_terminal(&init.config.borrow());

        // Build the scrollbar wrapper directly into the provided root box.
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.add_css_class("terminal-box");
        let scrollbar = gtk::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
        root.append(&terminal);
        root.append(&scrollbar);

        setup_terminal_click_handler(&terminal);

        // Forward VTE signals as Output messages.
        {
            let sender = sender.clone();
            let term_for_cwd = terminal.clone();
            let probe_for_cwd = init.probe.clone();
            let cwd_token = init.cwd_token.clone();
            terminal.connect_current_directory_uri_notify(move |_| {
                if let Some(uri) = term_for_cwd.current_directory_uri() {
                    if let Ok((path, host)) = gtk::glib::filename_from_uri(uri.as_str()) {
                        let path = path.to_string_lossy().to_string();
                        if path.is_empty() {
                            return;
                        }
                        let authority =
                            crate::terminal::classify_cwd_authority(host.as_deref(), &cwd_token);
                        let foreground = crate::process::foreground_uses_external_cwd(
                            probe_for_cwd.pty_fd.get(),
                            probe_for_cwd.shell_pid.get(),
                        );
                        let external = crate::terminal::resolve_cwd_external(authority, foreground);
                        let _ = sender.output(VteOutput::CwdChanged { path, external });
                    }
                }
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_child_exited(move |_term, status| {
                let _ = sender.output(VteOutput::Exited(status));
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_bell(move |_term| {
                let _ = sender.output(VteOutput::Bell);
            });
        }
        {
            let sender = sender.clone();
            let term_for_title = terminal.clone();
            terminal.connect_window_title_changed(move |_term| {
                if let Some(title) = term_for_title.window_title() {
                    let title_str = title.to_string();
                    if !title_str.is_empty() {
                        let _ = sender.output(VteOutput::TitleChanged(title_str));
                    }
                }
            });
        }
        {
            let sender = sender.clone();
            terminal.connect_contents_changed(move |_term| {
                let _ = sender.output(VteOutput::Activity);
            });
        }

        spawn_shell(
            &terminal,
            &init.shell_argv,
            init.working_directory.as_deref(),
            init.session_id.as_deref(),
            &init.cwd_token,
            init.initial_commands.as_slice(),
            init.probe.clone(),
            sender.clone(),
        );

        // Grab focus once the widget is realized.
        {
            let term_for_focus = terminal.clone();
            terminal.connect_realize(move |_| {
                term_for_focus.grab_focus();
            });
        }

        // Report focus-enter so the app can track the active pane.
        {
            let sender = sender.clone();
            let focus_ctl = gtk::EventControllerFocus::new();
            focus_ctl.connect_enter(move |_| {
                let _ = sender.output(VteOutput::Focused);
            });
            terminal.add_controller(focus_ctl);
        }

        let model = VteTerminal {
            terminal,
            config: init.config,
            search_status: SearchStatus::Idle,
        };
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            VteInput::WriteInput(data) => self.terminal.feed_child(&data),
            VteInput::RunAgentCommand { execution, .. } => {
                let _ = sender.output(VteOutput::AgentExecutionStartFailed { execution });
            }
            VteInput::Resize(cols, rows) => {
                if let Some(pty) = self.terminal.pty() {
                    let _ = pty.set_size(rows as i32, cols as i32);
                }
            }
            VteInput::GrabFocus => {
                self.terminal.grab_focus();
                let terminal = self.terminal.clone();
                gtk::glib::idle_add_local_once(move || {
                    terminal.grab_focus();
                });
            }
            VteInput::Copy | VteInput::CopyOutputOnly => {
                self.terminal.copy_clipboard_format(vte4::Format::Text)
            }
            VteInput::Paste => self.terminal.paste_clipboard(),
            VteInput::SetFontScale(scale) => self.terminal.set_font_scale(scale),
            VteInput::SetFont(desc) => {
                let fd = FontDescription::from_string(&desc);
                self.terminal.set_font(Some(&fd));
            }
            VteInput::SetScrollback(lines) => self.terminal.set_scrollback_lines(lines),
            VteInput::ScrollLines(lines) => {
                if let Some(adj) = self.terminal.vadjustment() {
                    let delta = adj.step_increment() * lines as f64;
                    let max_val = adj.upper() - adj.page_size();
                    let new_val =
                        (adj.value() + delta).clamp(adj.lower(), max_val.max(adj.lower()));
                    adj.set_value(new_val);
                }
            }
            VteInput::ApplyTheme => {
                let config = self.config.borrow();
                let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
                self.terminal.set_colors(
                    Some(&config.foreground),
                    Some(&config.background),
                    &palette_refs,
                );
                self.terminal.set_color_bold(None);
                self.terminal.set_color_cursor(Some(&config.cursor));
                self.terminal
                    .set_color_cursor_foreground(Some(&config.cursor_foreground));
            }
            VteInput::SyncConfig => {}
            VteInput::Kill => {
                if let Some(pid) = unsafe { self.terminal.data::<i32>("child-pid") } {
                    let pid_val = unsafe { *pid.as_ref() };
                    unsafe {
                        nix::libc::kill(pid_val, nix::libc::SIGHUP);
                    }
                }
            }
            // Block-view only; no-op for the bare VTE backend.
            VteInput::FilterFailedBlocks
            | VteInput::FilterSlowBlocks
            | VteInput::FilterPinnedBlocks
            | VteInput::ClearBlockFilter
            | VteInput::SelectAllBlocks
            | VteInput::ClearBlocks
            | VteInput::UndoClearBlocks
            | VteInput::ReinputSelectedCommands
            | VteInput::JumpToPrevPinned
            | VteInput::JumpToNextPinned
            | VteInput::JumpToPrevFailed
            | VteInput::JumpToNextFailed
            | VteInput::ExportSessionMarkdown
            | VteInput::ExportSessionJson => {}
            VteInput::SearchSet(query, use_regex) => {
                let pattern = search_pattern(&query, use_regex);
                self.search_status = match vte4::Regex::for_search(
                    &pattern,
                    pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
                ) {
                    Ok(regex) => {
                        self.terminal.search_set_regex(Some(&regex), 0);
                        self.terminal.search_set_wrap_around(true);
                        let found = self.terminal.search_find_next();
                        search_status_for_vte(
                            compile_count_regex(&pattern).ok().as_ref(),
                            terminal_search_snapshot(&self.terminal),
                            found,
                            !use_regex,
                        )
                    }
                    Err(error) => {
                        self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
                        SearchStatus::Error(invalid_regex_message(error))
                    }
                };
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchNext => {
                let found = self.terminal.search_find_next();
                self.search_status = self.search_status.stepped(1, found);
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchPrev => {
                let found = self.terminal.search_find_previous();
                self.search_status = self.search_status.stepped(-1, found);
                let _ = sender.output(VteOutput::SearchStatus(self.search_status.clone()));
            }
            VteInput::SearchClear => {
                self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
                self.search_status = SearchStatus::Idle;
                let _ = sender.output(VteOutput::SearchStatus(SearchStatus::Idle));
            }
            VteInput::CrossBlockSearch => {}
            VteInput::AskAiAboutSelectedBlock => {}
            // A conventional VTE pane has no cards to give a density to.
            VteInput::ApplyBlockDensity(_) => {}
        }
    }
}

pub(super) fn search_pattern(query: &str, use_regex: bool) -> String {
    if use_regex {
        query.to_string()
    } else {
        regex::escape(query)
    }
}

/// Compile the Rust-regex mirror used by Block search and by VTE result
/// counting. VTE installs its native PCRE2 regex first: a PCRE2 feature such as
/// look-around may be uncountable here, but it remains searchable and is shown
/// with an explicitly inexact `+` total instead of being rejected.
pub(super) fn compile_count_regex(pattern: &str) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        // Native terminal search uses PCRE2_MULTILINE. Keep anchors aligned in
        // the mirror counter; regex totals remain marked approximate because
        // the two engines still differ in other grammar and semantics.
        .multi_line(true)
        .build()
        .map_err(invalid_regex_message)
}

const VTE_SEARCH_SCAN_MAX_ROWS: i64 = 10_000;
const VTE_SEARCH_SCAN_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_UTF8_BYTES_PER_CELL: usize = 4;

#[derive(Debug)]
struct TerminalSearchSnapshot {
    text: String,
    truncated: bool,
}

/// VTE exposes next/previous success but no total match count. Count only a
/// bounded suffix: at most 10k rows, with a conservative cell-derived 2 MiB
/// budget and a hard post-extraction byte cap. The returned `truncated` bit is
/// part of the user-visible result (`N+`), never hidden as an exact total.
fn terminal_search_snapshot(terminal: &Terminal) -> TerminalSearchSnapshot {
    let columns = terminal.column_count();
    if columns <= 0 {
        return TerminalSearchSnapshot {
            text: String::new(),
            truncated: false,
        };
    }
    let (_, cursor_row) = terminal.cursor_position();
    let (start_row, end_row) = terminal
        .vadjustment()
        .map_or((0, cursor_row), |adjustment| {
            let start = adjustment.lower().floor() as i64;
            let adjustment_end = (adjustment.upper().ceil() as i64).saturating_sub(1);
            (start.min(cursor_row), adjustment_end.max(cursor_row))
        });
    if end_row < start_row {
        return TerminalSearchSnapshot {
            text: String::new(),
            truncated: false,
        };
    }

    let columns_usize = usize::try_from(columns).unwrap_or(usize::MAX).max(1);
    let rows_from_cell_budget = VTE_SEARCH_SCAN_MAX_BYTES
        .saturating_sub(VTE_SEARCH_SCAN_MAX_ROWS as usize)
        .checked_div(columns_usize.saturating_mul(MAX_UTF8_BYTES_PER_CELL).max(1))
        .unwrap_or(0)
        .max(1);
    let row_budget = VTE_SEARCH_SCAN_MAX_ROWS.min(rows_from_cell_budget as i64);
    let scan_start = end_row
        .saturating_sub(row_budget.saturating_sub(1))
        .max(start_row);
    let rows_truncated = scan_start > start_row;
    let mut text = terminal
        .text_range_format(
            vte4::Format::Text,
            scan_start,
            0,
            end_row,
            columns.saturating_sub(1),
        )
        .0
        .map(|text| text.to_string())
        .unwrap_or_default();
    let bytes_truncated = text.len() > VTE_SEARCH_SCAN_MAX_BYTES;
    if bytes_truncated {
        let mut keep_from = text.len() - VTE_SEARCH_SCAN_MAX_BYTES;
        while !text.is_char_boundary(keep_from) {
            keep_from += 1;
        }
        text = text.split_off(keep_from);
    }
    TerminalSearchSnapshot {
        text,
        truncated: rows_truncated || bytes_truncated,
    }
}

fn search_status_for_vte(
    counter: Option<&regex::Regex>,
    snapshot: TerminalSearchSnapshot,
    native_found: bool,
    counter_matches_native_semantics: bool,
) -> SearchStatus {
    // Native VTE searched the complete scrollback, so failure is an exact zero
    // even when our count snapshot was bounded.
    if !native_found {
        return SearchStatus::results(0, 0);
    }
    let Some(counter) = counter else {
        // Legal PCRE2 outside Rust regex's grammar remains fully searchable.
        return SearchStatus::partial_results(1, 1);
    };
    let counted = counter.find_iter(&snapshot.text).count();
    if !counter_matches_native_semantics || snapshot.truncated || counted == 0 {
        SearchStatus::partial_results(1, counted.max(1))
    } else {
        SearchStatus::results(1, counted)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compile_count_regex, launch_failure_message, search_pattern, search_status_for_vte,
        InitialCommands, TerminalSearchSnapshot,
    };
    use crate::search::SearchStatus;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn configured_commands_are_split_only_at_the_boundary() {
        let commands = InitialCommands::from_config(Some("cd /tmp, printf ready"));
        assert_eq!(
            commands.as_slice(),
            strings(&["cd /tmp", "printf ready"]).as_slice()
        );
    }

    #[test]
    fn search_query_compilation_is_literal_by_default_and_reports_bad_block_regex() {
        let literal = compile_count_regex(&search_pattern("a+b", false)).unwrap();
        assert_eq!(literal.find_iter("A+B aab").count(), 1);

        let regex = compile_count_regex(&search_pattern("a+b", true)).unwrap();
        assert_eq!(regex.find_iter("A+B aab").count(), 1);

        let error = compile_count_regex(&search_pattern("(", true)).unwrap_err();
        assert!(error.starts_with("Invalid regex:"));
        assert!(!error.contains('\n'));
    }

    #[test]
    fn bounded_or_pcre_only_counts_are_explicitly_inexact() {
        let counter = compile_count_regex("hit").unwrap();
        assert_eq!(
            search_status_for_vte(
                Some(&counter),
                TerminalSearchSnapshot {
                    text: "hit hit".into(),
                    truncated: false,
                },
                true,
                true,
            ),
            SearchStatus::results(1, 2)
        );
        let partial = search_status_for_vte(
            Some(&counter),
            TerminalSearchSnapshot {
                text: "hit hit".into(),
                truncated: true,
            },
            true,
            true,
        );
        assert_eq!(partial, SearchStatus::partial_results(1, 2));

        // Look-ahead is valid PCRE2 but deliberately outside Rust regex. VTE
        // accepts it natively; lack of a mirror counter must not become an error.
        assert!(compile_count_regex("(?=hit)").is_err());
        assert!(vte4::Regex::for_search("(?=hit)", pcre2_sys::PCRE2_CASELESS).is_ok());
        assert_eq!(
            search_status_for_vte(
                None,
                TerminalSearchSnapshot {
                    text: "hit".into(),
                    truncated: false,
                },
                true,
                false,
            ),
            SearchStatus::partial_results(1, 1)
        );
        assert_eq!(
            search_status_for_vte(
                None,
                TerminalSearchSnapshot {
                    text: String::new(),
                    truncated: true,
                },
                false,
                false,
            ),
            SearchStatus::results(0, 0)
        );
    }

    #[test]
    fn regex_counts_are_partial_even_when_both_engines_compile() {
        let pattern = "[a&&a]";
        let counter = compile_count_regex(pattern).unwrap();
        assert!(vte4::Regex::for_search(
            pattern,
            pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
        )
        .is_ok());
        assert_eq!(
            search_status_for_vte(
                Some(&counter),
                TerminalSearchSnapshot {
                    text: "a".into(),
                    truncated: false,
                },
                true,
                false,
            ),
            SearchStatus::partial_results(1, 1)
        );
    }

    #[test]
    fn mirror_counter_uses_the_native_multiline_anchor_mode() {
        let counter = compile_count_regex("^hit$").unwrap();
        assert_eq!(counter.find_iter("miss\nhit\ntail").count(), 1);
    }

    #[test]
    fn restored_argv_is_always_one_command_even_when_arguments_contain_commas() {
        let argv = strings(&["ssh", "host", "printf '%s, %s' one two"]);
        let commands = InitialCommands::from_restored_argv(Some(&argv), &strings(&["bash"]));
        assert_eq!(commands.as_slice().len(), 1);
        assert_eq!(
            commands.as_slice()[0],
            "'ssh' 'host' 'printf '\"'\"'%s, %s'\"'\"' one two'"
        );
    }

    #[test]
    fn unsafe_restored_argv_is_not_replayed() {
        let argv = strings(&["ssh", "host", "echo first\necho second"]);
        assert!(
            InitialCommands::from_restored_argv(Some(&argv), &strings(&["bash"]))
                .as_slice()
                .is_empty()
        );
    }

    #[test]
    fn restored_argv_uses_powershell_call_syntax() {
        let argv = strings(&["ssh", "host", "printf 'safe'; one argument"]);
        let commands =
            InitialCommands::from_restored_argv(Some(&argv), &strings(&["/usr/bin/pwsh"]));
        assert_eq!(
            commands.as_slice(),
            strings(&["& 'ssh' 'host' 'printf ''safe''; one argument'"]).as_slice()
        );
    }

    #[test]
    fn launch_failure_message_includes_the_underlying_io_error_and_recovery_hint() {
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "flatpak-spawn missing");
        let message = launch_failure_message(&error);

        assert_eq!(
            message,
            "Terminal failed to start: flatpak-spawn missing. \
             Check the shell, remote command, or host bridge."
        );
    }
}
