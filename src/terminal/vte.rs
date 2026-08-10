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

    if let Ok(regex_pattern) = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    ) {
        terminal.match_add_regex(&regex_pattern, 0);
    }

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

    // libvte injects its own TERM/VTE_VERSION and adds `envv` on top, so this is
    // identity only: a LESS or a LANG asserted here would override whatever the
    // user configured, which `child_env::vte_envv` documents and refuses to do.
    // The cwd-authentication token rides along as the per-call extra.
    let envv_owned: Vec<String> =
        child_env::vte_envv(&child_env::ChildEnv::from_identity(), &host_environment);
    let envv: Vec<&str> = envv_owned.iter().map(String::as_str).collect();
    let spawn_flags = SpawnFlags::SEARCH_PATH;
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
                let pattern = if use_regex {
                    query
                } else {
                    gtk::glib::Regex::escape_string(&query).to_string()
                };
                if let Ok(regex) = vte4::Regex::for_search(&pattern, pcre2_sys::PCRE2_CASELESS) {
                    self.terminal.search_set_regex(Some(&regex), 0);
                    self.terminal.search_set_wrap_around(true);
                    self.terminal.search_find_next();
                }
            }
            VteInput::SearchNext => {
                self.terminal.search_find_next();
            }
            VteInput::SearchPrev => {
                self.terminal.search_find_previous();
            }
            VteInput::SearchClear => {
                self.terminal.search_set_regex(None::<&vte4::Regex>, 0);
            }
            VteInput::CrossBlockSearch => {}
            VteInput::AskAiAboutSelectedBlock => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{launch_failure_message, InitialCommands};

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
