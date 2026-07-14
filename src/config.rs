use gtk::gdk::RGBA;
use gtk::glib;
use relm4::gtk;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use crate::keybindings::KeybindingMap;

// ---------------------------------------------------------------------------
// Terminal Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum TerminalMode {
    Block,
    Vte,
}

/// Where the tab strip lives: down the left sidebar (vertical) or along the top
/// bar (horizontal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabPlacement {
    Sidebar,
    TopBar,
}

impl TabPlacement {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TabPlacement::Sidebar => "sidebar",
            TabPlacement::TopBar => "top",
        }
    }

    pub(crate) fn parse(s: &str) -> TabPlacement {
        match s.to_lowercase().as_str() {
            "top" | "topbar" | "top_bar" => TabPlacement::TopBar,
            _ => TabPlacement::Sidebar,
        }
    }
}

/// Which single view the sidebar shows (tab list vs file tree).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarView {
    Tabs,
    Files,
}

impl SidebarView {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SidebarView::Tabs => "tabs",
            SidebarView::Files => "files",
        }
    }

    pub(crate) fn parse(s: &str) -> SidebarView {
        match s.to_lowercase().as_str() {
            "files" | "file" | "filetree" | "file_tree" => SidebarView::Files,
            _ => SidebarView::Tabs,
        }
    }
}

// ---------------------------------------------------------------------------
// Remote host
// ---------------------------------------------------------------------------

/// A saved SSH target. A new tab can be opened that runs the remote shell over
/// `ssh -t`, reusing all local PTY/terminal infrastructure (OSC 133 markers
/// emitted by the remote shell flow through ssh, so block mode works remotely).
#[derive(Clone, Debug)]
pub struct RemoteHost {
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    /// Shell to launch on the remote side (default "rsh").
    pub remote_shell: String,
    /// Stable session id passed to the remote rsh for resume-on-reconnect.
    pub session: Option<String>,
    /// Extra flags inserted before the target (e.g. ["-p", "2222"]).
    pub ssh_args: Vec<String>,
    /// Run the remote command through a login shell (`bash -lc 'exec ...'`) so the
    /// user's profile (PATH, ~/.cargo/env, etc.) is loaded. ssh's plain command
    /// channel runs a non-login, non-interactive shell, which leaves tools like
    /// cargo off PATH. Defaults to true.
    pub login_shell: bool,
    /// Reuse one ssh connection for repeat tabs to this host (ControlMaster), so
    /// the 2nd+ tab skips the handshake/auth. Defaults to true.
    pub multiplex: bool,
}

/// Directory for ssh ControlMaster sockets. Prefers `$XDG_RUNTIME_DIR`, falls
/// back to `~/.cache/jterm1`. Created if missing.
fn control_socket_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/jterm1")))?;
    if let Err(err) = fs::create_dir_all(&base) {
        log::warn!(
            "Failed to create ssh control socket dir {}: {err}",
            base.display()
        );
        return None;
    }
    Some(base)
}

fn shell_single_quote(s: &str) -> String {
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            quoted.push_str("'\"'\"'");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn wrap_exec_in_login_bash(command: &str) -> String {
    format!("bash -lc 'exec {}'", command.replace('\'', "'\\''"))
}

fn wrap_rsh_argv_in_interactive_bash(rsh_path: &str) -> Option<Vec<String>> {
    let bash_path = find_executable_in_path("bash")?;
    Some(vec![
        bash_path.to_string_lossy().to_string(),
        "-ic".to_string(),
        format!("exec {}", shell_single_quote(rsh_path)),
    ])
}

/// Build the local argv that connects to a remote host via ssh.
/// Produces e.g. `["ssh", "-t", "-p", "2222", "mm@100.x.x.x", "rsh --session home-main"]`.
pub(crate) fn build_remote_argv(host: &RemoteHost) -> Vec<String> {
    let target = match &host.user {
        Some(u) => format!("{u}@{}", host.host),
        None => host.host.clone(),
    };
    let mut remote_cmd = host.remote_shell.clone();
    if let Some(sid) = &host.session {
        remote_cmd.push_str(" --session ");
        remote_cmd.push_str(sid);
    }
    if host.login_shell {
        remote_cmd = wrap_exec_in_login_bash(&remote_cmd);
    }
    let mut argv = vec!["ssh".to_string(), "-t".to_string()];
    if host.multiplex {
        if let Some(dir) = control_socket_dir() {
            // %C is ssh's hash of (local user, host, port, user) — a safe filename.
            let ctl_path = dir.join("cm-%C");
            argv.push("-o".to_string());
            argv.push("ControlMaster=auto".to_string());
            argv.push("-o".to_string());
            argv.push("ControlPersist=120".to_string());
            argv.push("-o".to_string());
            argv.push(format!("ControlPath={}", ctl_path.display()));
        }
    }
    argv.extend(host.ssh_args.iter().cloned());
    argv.push(target);
    argv.push(remote_cmd);
    argv
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Config {
    pub(crate) window_opacity: f64,
    pub(crate) terminal_scrollback_lines: u32,
    pub(crate) font_desc: String,
    pub(crate) default_font_scale: f64,
    pub(crate) theme_name: String,
    pub(crate) foreground: RGBA,
    pub(crate) background: RGBA,
    pub(crate) cursor: RGBA,
    pub(crate) cursor_foreground: RGBA,
    pub(crate) palette: [RGBA; 16],
    /// Explicit shell path (overrides auto-detection). Useful when PATH is stripped by launchers.
    pub(crate) shell: Option<String>,
    /// Commands to feed to new shells on startup (comma-separated).
    pub(crate) startup_commands: Option<String>,
    pub(crate) terminal_mode: TerminalMode,
    /// Where the tab strip is shown (left sidebar vs top bar).
    pub(crate) tab_placement: TabPlacement,
    /// Which single view the sidebar shows (tab list vs file tree).
    pub(crate) sidebar_view: SidebarView,
    /// Sidebar width in pixels.
    pub(crate) sidebar_width: u32,
    /// Width of each tab in the top tab bar, in pixels.
    pub(crate) tab_width: u32,
    // Block view optimizations
    pub(crate) ansi_cache_capacity: u32,
    pub(crate) max_visible_blocks: u32,
    pub(crate) output_batch_min_ms: u32,
    pub(crate) output_batch_max_ms: u32,
    pub(crate) lazy_load_threshold: u32,
    pub(crate) truncation_threshold_lines: u32,
    pub(crate) finished_block_viewport_rows: u32,
    pub(crate) finished_block_max_expanded_rows: u32,
    pub(crate) max_collapsed_output_lines: u32,
    pub(crate) virtual_scroll_margin: u32,
    /// Lightweight JSONL command index used by History, the command palette,
    /// and optional AI context. Unlike `block_history_path`, this never stores
    /// command output.
    pub(crate) command_history_enabled: bool,
    pub(crate) command_history_path: Option<String>,
    pub(crate) command_history_max_entries: u32,
    pub(crate) block_history_path: Option<String>,
    pub(crate) block_history_compress: bool,
    /// Compact (denser) block-mode spacing, matching Warp's compact density.
    pub(crate) block_compact: bool,
    pub(crate) editor_input: bool,
    /// Allow OSC 52 SET (`\e]52;c;<base64>\e\\`) from remote/local apps to
    /// overwrite the system clipboard. Off by default — a malicious or buggy
    /// remote process can otherwise silently replace the user's clipboard
    /// (OWASP-style concern). Most users enable this only on trusted hosts.
    pub(crate) allow_remote_clipboard_write: bool,
    pub(crate) mouse_reporting_enabled: bool,
    pub(crate) focus_reporting_enabled: bool,
    pub(crate) scroll_reporting_enabled: bool,
    pub(crate) preserve_live_scrollback: bool,
    /// Show jterm1-side AI surfaces (per-block error explain button, the
    /// session AI panel, and the `?` palette prefix). Default on; flip to
    /// `false` to hide all AI UI even when API keys are present. The actual
    /// network call still only fires on an explicit user click — this just
    /// removes the entry points.
    pub(crate) ai_enabled: bool,
    /// Show the agent-mode entry point (`Ctrl+Shift+G` / palette). Default
    /// on, but suppressed when `ai_enabled` is false. Independent toggle so
    /// users who like the per-block AI helpers but find agent mode too
    /// risky can disable the multi-turn loop without losing the rest.
    pub(crate) agent_enabled: bool,
    /// Hard cap on assistant turns per agent session. Once reached the
    /// session is sealed and the user must start a new one — this is a
    /// runaway-loop safety net, not a usability lever.
    pub(crate) agent_max_turns: u32,
    pub(crate) notify_long_blocks: bool,
    pub(crate) notify_long_block_threshold_ms: u64,
    pub(crate) show_repo_strip: bool,
    /// Saved SSH targets selectable from the context menu.
    pub(crate) remote_hosts: Vec<RemoteHost>,
}

impl Config {
    /// Reduce startup to a local, non-persistent compatibility session. Safe
    /// mode intentionally ignores user commands, remote destinations, history,
    /// notifications, repository probes, clipboard writes, and all AI surfaces.
    pub(crate) fn apply_safe_mode(&mut self) {
        self.shell = None;
        self.startup_commands = None;
        self.terminal_mode = TerminalMode::Vte;
        self.command_history_enabled = false;
        self.command_history_path = None;
        self.block_history_path = None;
        self.preserve_live_scrollback = false;
        self.allow_remote_clipboard_write = false;
        self.ai_enabled = false;
        self.agent_enabled = false;
        self.notify_long_blocks = false;
        self.show_repo_strip = false;
        self.remote_hosts.clear();
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Theme {
    pub(crate) name: String,
    pub(crate) foreground: RGBA,
    pub(crate) background: RGBA,
    pub(crate) cursor: RGBA,
    pub(crate) cursor_foreground: RGBA,
    pub(crate) palette: [RGBA; 16],
}

fn parse_palette(hex: [&str; 16]) -> [RGBA; 16] {
    hex.map(|s| RGBA::parse(s).unwrap())
}

pub(crate) fn builtin_themes() -> Vec<Theme> {
    thread_local! {
        static CACHED: RefCell<Option<Vec<Theme>>> = const { RefCell::new(None) };
    }
    if let Some(themes) = CACHED.with(|c| c.borrow().clone()) {
        return themes;
    }
    let themes = vec![
        Theme {
            name: "default".into(),
            foreground: RGBA::parse("#f8f7e9").unwrap(),
            background: RGBA::parse("#121616").unwrap(),
            cursor: RGBA::parse("#7fb80e").unwrap(),
            cursor_foreground: RGBA::parse("#1b315e").unwrap(),
            palette: parse_palette([
                "#130c0e", "#ed1941", "#45b97c", "#fdb933", "#2585a6", "#ae5039", "#009ad6",
                "#fffef9", "#7c8577", "#f05b72", "#84bf96", "#ffc20e", "#7bbfea", "#f58f98",
                "#33a3dc", "#f6f5ec",
            ]),
        },
        Theme {
            name: "light".into(),
            foreground: RGBA::parse("#2e3440").unwrap(),
            background: RGBA::parse("#eceff4").unwrap(),
            cursor: RGBA::parse("#4c566a").unwrap(),
            cursor_foreground: RGBA::parse("#eceff4").unwrap(),
            palette: parse_palette([
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0",
                "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead",
                "#8fbcbb", "#eceff4",
            ]),
        },
        Theme {
            name: "solarized-dark".into(),
            foreground: RGBA::parse("#839496").unwrap(),
            background: RGBA::parse("#002b36").unwrap(),
            cursor: RGBA::parse("#93a1a1").unwrap(),
            cursor_foreground: RGBA::parse("#002b36").unwrap(),
            palette: parse_palette([
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198",
                "#eee8d5", "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4",
                "#93a1a1", "#fdf6e3",
            ]),
        },
        Theme {
            name: "solarized-light".into(),
            foreground: RGBA::parse("#657b83").unwrap(),
            background: RGBA::parse("#fdf6e3").unwrap(),
            cursor: RGBA::parse("#586e75").unwrap(),
            cursor_foreground: RGBA::parse("#fdf6e3").unwrap(),
            palette: parse_palette([
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198",
                "#eee8d5", "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4",
                "#93a1a1", "#fdf6e3",
            ]),
        },
        Theme {
            name: "gruvbox-dark".into(),
            foreground: RGBA::parse("#ebdbb2").unwrap(),
            background: RGBA::parse("#282828").unwrap(),
            cursor: RGBA::parse("#ebdbb2").unwrap(),
            cursor_foreground: RGBA::parse("#282828").unwrap(),
            palette: parse_palette([
                "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
                "#a89984", "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b",
                "#8ec07c", "#ebdbb2",
            ]),
        },
        Theme {
            name: "gruvbox-light".into(),
            foreground: RGBA::parse("#3c3836").unwrap(),
            background: RGBA::parse("#fbf1c7").unwrap(),
            cursor: RGBA::parse("#3c3836").unwrap(),
            cursor_foreground: RGBA::parse("#fbf1c7").unwrap(),
            palette: parse_palette([
                "#fbf1c7", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
                "#7c6f64", "#928374", "#9d0006", "#79740e", "#b57614", "#076678", "#8f3f71",
                "#427b58", "#3c3836",
            ]),
        },
        Theme {
            name: "dracula".into(),
            foreground: RGBA::parse("#f8f8f2").unwrap(),
            background: RGBA::parse("#282a36").unwrap(),
            cursor: RGBA::parse("#f8f8f2").unwrap(),
            cursor_foreground: RGBA::parse("#282a36").unwrap(),
            palette: parse_palette([
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd",
                "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df",
                "#a4ffff", "#ffffff",
            ]),
        },
        Theme {
            name: "nord".into(),
            foreground: RGBA::parse("#d8dee9").unwrap(),
            background: RGBA::parse("#2e3440").unwrap(),
            cursor: RGBA::parse("#d8dee9").unwrap(),
            cursor_foreground: RGBA::parse("#2e3440").unwrap(),
            palette: parse_palette([
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0",
                "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead",
                "#8fbcbb", "#eceff4",
            ]),
        },
    ];
    CACHED.with(|c| *c.borrow_mut() = Some(themes.clone()));
    themes
}

// ---------------------------------------------------------------------------
// Env helpers
// ---------------------------------------------------------------------------

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok())
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok())
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

fn env_rgba(name: &str) -> Option<RGBA> {
    env_string(name).and_then(|v| RGBA::parse(&v).ok())
}

// ---------------------------------------------------------------------------
// File config
// ---------------------------------------------------------------------------

pub(crate) fn config_file_path() -> PathBuf {
    glib::user_config_dir().join("jterm1").join("config.toml")
}

pub(crate) fn default_command_history_path() -> String {
    glib::user_state_dir()
        .join("jterm1")
        .join("history.jsonl")
        .to_string_lossy()
        .into_owned()
}

/// Return a user-facing diagnostic when the config exists but cannot be read
/// or parsed. Callers use this before hot reload and before any write so a
/// malformed hand-edited file is never silently replaced with defaults.
pub(crate) fn config_file_error() -> Option<String> {
    let path = config_file_path();
    match fs::read_to_string(&path) {
        Ok(contents) => contents
            .parse::<toml::Table>()
            .err()
            .map(|err| format!("{}: {err}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(format!("{}: {err}", path.display())),
    }
}

/// Parsed TOML config file structure.
#[derive(Default)]
struct FileConfig {
    opacity: Option<f64>,
    scrollback: Option<u32>,
    font: Option<String>,
    font_scale: Option<f64>,
    theme: Option<String>,
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    cursor_foreground: Option<String>,
    keybindings: Option<toml::Table>,
    shell: Option<String>,
    /// Commands to run when a new tab opens (comma-separated, e.g. "cd ~/project, nix develop").
    startup_commands: Option<String>,
    terminal_mode: Option<String>,
    tab_placement: Option<String>,
    sidebar_view: Option<String>,
    sidebar_width: Option<u32>,
    tab_width: Option<u32>,
    // Block view optimizations
    ansi_cache_capacity: Option<u32>,
    max_visible_blocks: Option<u32>,
    output_batch_min_ms: Option<u32>,
    output_batch_max_ms: Option<u32>,
    lazy_load_threshold: Option<u32>,
    truncation_threshold_lines: Option<u32>,
    finished_block_viewport_rows: Option<u32>,
    finished_block_max_expanded_rows: Option<u32>,
    max_collapsed_output_lines: Option<u32>,
    virtual_scroll_margin: Option<u32>,
    command_history_enabled: Option<bool>,
    command_history_path: Option<String>,
    command_history_max_entries: Option<u32>,
    block_history_path: Option<String>,
    block_history_compress: Option<bool>,
    block_compact: Option<bool>,
    editor_input: Option<bool>,
    allow_remote_clipboard_write: Option<bool>,
    ai_enabled: Option<bool>,
    agent_enabled: Option<bool>,
    agent_max_turns: Option<u32>,
    mouse_reporting_enabled: Option<bool>,
    focus_reporting_enabled: Option<bool>,
    scroll_reporting_enabled: Option<bool>,
    preserve_live_scrollback: Option<bool>,
    notify_long_blocks: Option<bool>,
    notify_long_block_threshold_ms: Option<u64>,
    show_repo_strip: Option<bool>,
    remote_hosts: Vec<RemoteHost>,
}

fn load_file_config() -> FileConfig {
    let path = config_file_path();
    let Ok(contents) = fs::read_to_string(&path) else {
        return FileConfig {
            remote_hosts: default_remote_hosts(),
            ..Default::default()
        };
    };
    let Ok(table) = contents.parse::<toml::Table>() else {
        log::warn!("Failed to parse config file {}", path.display());
        return FileConfig {
            remote_hosts: default_remote_hosts(),
            ..Default::default()
        };
    };

    let colors = table.get("colors").and_then(|v| v.as_table());
    // Fall back to built-in defaults when the section is entirely absent (e.g. a
    // config file first created to persist some other setting). An explicit,
    // possibly empty, [[remote_hosts]] array is respected as-is.
    let remote_hosts = if table.contains_key("remote_hosts") {
        parse_remote_hosts(&table)
    } else {
        default_remote_hosts()
    };

    FileConfig {
        opacity: table.get("opacity").and_then(|v| v.as_float()),
        scrollback: table
            .get("scrollback")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        font: table
            .get("font")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        font_scale: table.get("font_scale").and_then(|v| v.as_float()),
        theme: table
            .get("theme")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        foreground: colors
            .and_then(|c| c.get("foreground"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        background: colors
            .and_then(|c| c.get("background"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        cursor: colors
            .and_then(|c| c.get("cursor"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        cursor_foreground: colors
            .and_then(|c| c.get("cursor_foreground"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        keybindings: table.get("keybindings").and_then(|v| v.as_table()).cloned(),
        shell: table
            .get("shell")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        startup_commands: table
            .get("startup_commands")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        terminal_mode: table
            .get("terminal_mode")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        tab_placement: table
            .get("tab_placement")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        sidebar_view: table
            .get("sidebar_view")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        sidebar_width: table
            .get("sidebar_width")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        tab_width: table
            .get("tab_width")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        ansi_cache_capacity: table
            .get("ansi_cache_capacity")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        max_visible_blocks: table
            .get("max_visible_blocks")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        output_batch_min_ms: table
            .get("output_batch_min_ms")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        output_batch_max_ms: table
            .get("output_batch_max_ms")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        lazy_load_threshold: table
            .get("lazy_load_threshold")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        truncation_threshold_lines: table
            .get("truncation_threshold_lines")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        finished_block_viewport_rows: table
            .get("finished_block_viewport_rows")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        finished_block_max_expanded_rows: table
            .get("finished_block_max_expanded_rows")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        max_collapsed_output_lines: table
            .get("max_collapsed_output_lines")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        virtual_scroll_margin: table
            .get("virtual_scroll_margin")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        command_history_enabled: table
            .get("command_history_enabled")
            .and_then(|v| v.as_bool()),
        command_history_path: table
            .get("command_history_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        command_history_max_entries: table
            .get("command_history_max_entries")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        block_history_path: table
            .get("block_history_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        block_history_compress: table
            .get("block_history_compress")
            .and_then(|v| v.as_bool()),
        block_compact: table.get("block_compact").and_then(|v| v.as_bool()),
        editor_input: table.get("editor_input").and_then(|v| v.as_bool()),
        allow_remote_clipboard_write: table
            .get("allow_remote_clipboard_write")
            .and_then(|v| v.as_bool()),
        mouse_reporting_enabled: table
            .get("mouse_reporting_enabled")
            .and_then(|v| v.as_bool()),
        focus_reporting_enabled: table
            .get("focus_reporting_enabled")
            .and_then(|v| v.as_bool()),
        scroll_reporting_enabled: table
            .get("scroll_reporting_enabled")
            .and_then(|v| v.as_bool()),
        preserve_live_scrollback: table
            .get("preserve_live_scrollback")
            .and_then(|v| v.as_bool()),
        ai_enabled: table.get("ai_enabled").and_then(|v| v.as_bool()),
        agent_enabled: table.get("agent_enabled").and_then(|v| v.as_bool()),
        agent_max_turns: table
            .get("agent_max_turns")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        notify_long_blocks: table.get("notify_long_blocks").and_then(|v| v.as_bool()),
        notify_long_block_threshold_ms: table
            .get("notify_long_block_threshold_ms")
            .and_then(|v| v.as_integer())
            .and_then(|v| u64::try_from(v).ok()),
        show_repo_strip: table.get("show_repo_strip").and_then(|v| v.as_bool()),
        remote_hosts,
    }
}

/// Parse `[[remote_hosts]]` array-of-tables. Entries missing a `host` are skipped.
fn parse_remote_hosts(table: &toml::Table) -> Vec<RemoteHost> {
    let Some(arr) = table.get("remote_hosts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_table())
        .filter_map(|t| {
            let host = t.get("host").and_then(|v| v.as_str())?.to_string();
            let name = t
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| host.clone());
            let user = t
                .get("user")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let remote_shell = t
                .get("remote_shell")
                .and_then(|v| v.as_str())
                .unwrap_or("rsh")
                .to_string();
            let session = t
                .get("session")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let ssh_args = t
                .get("ssh_args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let login_shell = t
                .get("login_shell")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let multiplex = t.get("multiplex").and_then(|v| v.as_bool()).unwrap_or(true);
            Some(RemoteHost {
                name,
                host,
                user,
                remote_shell,
                session,
                ssh_args,
                login_shell,
                multiplex,
            })
        })
        .collect()
}

/// Serialize a `RemoteHost` back into a TOML table that `parse_remote_hosts`
/// round-trips. Optional fields are only emitted when present.
fn remote_host_to_toml(h: &RemoteHost) -> toml::Value {
    let mut t = toml::Table::new();
    t.insert("name".into(), toml::Value::String(h.name.clone()));
    t.insert("host".into(), toml::Value::String(h.host.clone()));
    if let Some(user) = &h.user {
        t.insert("user".into(), toml::Value::String(user.clone()));
    }
    t.insert(
        "remote_shell".into(),
        toml::Value::String(h.remote_shell.clone()),
    );
    if let Some(session) = &h.session {
        t.insert("session".into(), toml::Value::String(session.clone()));
    }
    if !h.ssh_args.is_empty() {
        let args: Vec<toml::Value> = h
            .ssh_args
            .iter()
            .map(|a| toml::Value::String(a.clone()))
            .collect();
        t.insert("ssh_args".into(), toml::Value::Array(args));
    }
    t.insert("login_shell".into(), toml::Value::Boolean(h.login_shell));
    t.insert("multiplex".into(), toml::Value::Boolean(h.multiplex));
    toml::Value::Table(t)
}

/// A fresh install has no implicit network destinations. Remote targets are
/// user-owned configuration and must never ship with developer addresses,
/// usernames, session ids, or absolute paths baked in.
fn default_remote_hosts() -> Vec<RemoteHost> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// load_config
// ---------------------------------------------------------------------------

pub(crate) fn load_config() -> (Config, Vec<Theme>, KeybindingMap) {
    let fc = load_file_config();
    let themes = builtin_themes();

    // Resolve active theme
    let theme_name = env_string("JTERM1_THEME")
        .or(fc.theme)
        .unwrap_or_else(|| "default".to_string());
    let theme = themes
        .iter()
        .find(|t| t.name == theme_name)
        .unwrap_or(&themes[0]);

    // Priority: env var > config file > theme default
    let window_opacity = env_f64("JTERM1_OPACITY")
        .or(fc.opacity)
        .unwrap_or(0.95)
        .clamp(0.01, 1.0);
    let terminal_scrollback_lines = env_u32("JTERM1_SCROLLBACK")
        .or(fc.scrollback)
        .unwrap_or(5000)
        .min(1_000_000);
    let default_font_scale = env_f64("JTERM1_FONT_SCALE")
        .or(fc.font_scale)
        .unwrap_or(1.0)
        .clamp(0.1, 10.0);
    let font_desc = env_string("JTERM1_FONT")
        .or(fc.font)
        // Use the "Mono" (NFM) Nerd Font variant: the plain "Nerd Font" (NF)
        // variant renders proportionally in VTE (glyphs draw at non-cell widths)
        // even though fontconfig reports it spacing=100, so output never aligns
        // like a real terminal. NFM forces single-cell glyphs.
        .unwrap_or_else(|| "SauceCodePro Nerd Font Mono 14".to_string());

    let foreground = env_rgba("JTERM1_FG")
        .or_else(|| fc.foreground.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.foreground);
    let background = env_rgba("JTERM1_BG")
        .or_else(|| fc.background.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.background);
    let cursor = env_rgba("JTERM1_CURSOR")
        .or_else(|| fc.cursor.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.cursor);
    let cursor_foreground = env_rgba("JTERM1_CURSOR_FG")
        .or_else(|| {
            fc.cursor_foreground
                .as_deref()
                .and_then(|v| RGBA::parse(v).ok())
        })
        .unwrap_or(theme.cursor_foreground);

    // Block view optimization settings
    let ansi_cache_capacity = env_u32("JTERM1_ANSI_CACHE_CAP")
        .or(fc.ansi_cache_capacity)
        .unwrap_or(256)
        .clamp(1, 65_536);
    let max_visible_blocks = env_u32("JTERM1_MAX_BLOCKS")
        .or(fc.max_visible_blocks)
        .unwrap_or(200)
        .clamp(1, 10_000);
    let output_batch_min_ms = env_u32("JTERM1_BATCH_MIN")
        .or(fc.output_batch_min_ms)
        .unwrap_or(10)
        .clamp(1, 1_000);
    let output_batch_max_ms = env_u32("JTERM1_BATCH_MAX")
        .or(fc.output_batch_max_ms)
        .unwrap_or(100)
        .clamp(output_batch_min_ms, 5_000);
    let lazy_load_threshold = env_u32("JTERM1_LAZY_LINES")
        .or(fc.lazy_load_threshold)
        .unwrap_or(1000)
        .clamp(1, 100_000);
    let truncation_threshold_lines = env_u32("JTERM1_TRUNCATION_LINES")
        .or(fc.truncation_threshold_lines)
        .unwrap_or(50000)
        .clamp(100, 1_000_000);
    let finished_block_viewport_rows = env_u32("JTERM1_FINISHED_VIEWPORT_ROWS")
        .or(fc.finished_block_viewport_rows)
        .unwrap_or(24)
        .clamp(3, 5_000);
    let finished_block_max_expanded_rows = env_u32("JTERM1_FINISHED_MAX_EXPANDED_ROWS")
        .or(fc.finished_block_max_expanded_rows)
        .unwrap_or(5000)
        .clamp(finished_block_viewport_rows, 5000);
    let max_collapsed_output_lines = env_u32("JTERM1_MAX_COLLAPSED_LINES")
        .or(fc.max_collapsed_output_lines)
        .unwrap_or(25)
        .min(10_000);
    let virtual_scroll_margin = env_u32("JTERM1_VSCROLL_MARGIN")
        .or(fc.virtual_scroll_margin)
        .unwrap_or(1)
        .min(100);
    let command_history_enabled = fc.command_history_enabled.unwrap_or(true);
    let command_history_path = command_history_enabled.then(|| {
        std::env::var("JTERM1_COMMAND_HISTORY_PATH")
            .ok()
            .or(fc.command_history_path)
            .filter(|path| !path.trim().is_empty())
            .unwrap_or_else(default_command_history_path)
    });
    let command_history_max_entries = fc
        .command_history_max_entries
        .unwrap_or(10_000)
        .clamp(100, 100_000);
    let block_history_path = std::env::var("JTERM1_HISTORY_PATH")
        .ok()
        .or(fc.block_history_path);
    let block_history_compress = fc.block_history_compress.unwrap_or(true);
    let block_compact = match std::env::var("JTERM1_BLOCK_COMPACT").ok().as_deref() {
        Some("1") | Some("true") => Some(true),
        Some("0") | Some("false") => Some(false),
        _ => None,
    }
    .or(fc.block_compact)
    .unwrap_or(false);
    let shell = std::env::var("JTERM1_SHELL").ok().or(fc.shell);

    // Block mode is jterm1's defining experience and is required for the
    // command-completion events consumed by Agent and command history. Users
    // can still opt into the compatibility VTE backend explicitly.
    let terminal_mode_str = env_string("JTERM1_MODE")
        .or(fc.terminal_mode)
        .unwrap_or_else(|| "block".to_string());
    let terminal_mode = match terminal_mode_str.to_lowercase().as_str() {
        "vte" => TerminalMode::Vte,
        _ => TerminalMode::Block,
    };

    let tab_placement = TabPlacement::parse(
        &env_string("JTERM1_TAB_PLACEMENT")
            .or(fc.tab_placement)
            .unwrap_or_else(|| "sidebar".to_string()),
    );
    let sidebar_view = SidebarView::parse(&fc.sidebar_view.unwrap_or_else(|| "tabs".to_string()));
    let sidebar_width = fc.sidebar_width.unwrap_or(220).clamp(120, 800);
    let tab_width = fc.tab_width.unwrap_or(180).clamp(80, 480);

    let config = Config {
        window_opacity,
        terminal_scrollback_lines,
        font_desc,
        default_font_scale,
        theme_name: theme.name.clone(),
        foreground,
        background,
        cursor,
        cursor_foreground,
        palette: theme.palette,
        shell,
        startup_commands: fc.startup_commands,
        terminal_mode,
        tab_placement,
        sidebar_view,
        sidebar_width,
        tab_width,
        ansi_cache_capacity,
        max_visible_blocks,
        output_batch_min_ms,
        output_batch_max_ms,
        lazy_load_threshold,
        truncation_threshold_lines,
        finished_block_viewport_rows,
        finished_block_max_expanded_rows,
        max_collapsed_output_lines,
        virtual_scroll_margin,
        command_history_enabled,
        command_history_path,
        command_history_max_entries,
        block_history_path,
        block_history_compress,
        block_compact,
        editor_input: fc.editor_input.unwrap_or(true),
        allow_remote_clipboard_write: fc.allow_remote_clipboard_write.unwrap_or(false),
        mouse_reporting_enabled: fc.mouse_reporting_enabled.unwrap_or(true),
        focus_reporting_enabled: fc.focus_reporting_enabled.unwrap_or(true),
        scroll_reporting_enabled: fc.scroll_reporting_enabled.unwrap_or(true),
        preserve_live_scrollback: fc.preserve_live_scrollback.unwrap_or(false),
        ai_enabled: fc.ai_enabled.unwrap_or(true),
        agent_enabled: fc.agent_enabled.unwrap_or(true),
        agent_max_turns: fc.agent_max_turns.unwrap_or(20).max(1),
        notify_long_blocks: fc.notify_long_blocks.unwrap_or(true),
        notify_long_block_threshold_ms: fc.notify_long_block_threshold_ms.unwrap_or(10_000),
        show_repo_strip: fc.show_repo_strip.unwrap_or(true),
        remote_hosts: fc.remote_hosts,
    };

    let mut keybinding_map = KeybindingMap::from_defaults();
    if let Some(ref kb_table) = fc.keybindings {
        keybinding_map.apply_user_overrides(kb_table);
    }

    (config, themes, keybinding_map)
}

// ---------------------------------------------------------------------------
// save_config
// ---------------------------------------------------------------------------

pub(crate) fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8
    )
}

fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(contents)?;
    file.sync_all()
}

pub(crate) fn save_config(config: &Config) -> Result<(), String> {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("create config dir {}: {err}", parent.display()))?;
    }

    // Read existing config to preserve user-authored sections (e.g.
    // [keybindings]). A parse failure is a hard stop: falling back to an empty
    // table here would destroy the user's file on the next settings change.
    let existing = match fs::read_to_string(&path) {
        Ok(contents) => Some(contents),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(format!("read config {}: {err}", path.display())),
    };
    let mut table = match existing.as_deref() {
        Some(contents) => contents.parse::<toml::Table>().map_err(|err| {
            format!(
                "refusing to overwrite invalid config {}: {err}",
                path.display()
            )
        })?,
        None => toml::Table::new(),
    };

    table.insert("opacity".into(), toml::Value::Float(config.window_opacity));
    table.insert(
        "scrollback".into(),
        toml::Value::Integer(config.terminal_scrollback_lines as i64),
    );
    table.insert("font".into(), toml::Value::String(config.font_desc.clone()));
    table.insert(
        "font_scale".into(),
        toml::Value::Float(config.default_font_scale),
    );
    table.insert(
        "theme".into(),
        toml::Value::String(config.theme_name.clone()),
    );
    table.insert(
        "terminal_mode".into(),
        toml::Value::String(
            match config.terminal_mode {
                TerminalMode::Block => "block",
                TerminalMode::Vte => "vte",
            }
            .to_string(),
        ),
    );
    table.insert(
        "tab_placement".into(),
        toml::Value::String(config.tab_placement.as_str().to_string()),
    );
    table.insert(
        "sidebar_view".into(),
        toml::Value::String(config.sidebar_view.as_str().to_string()),
    );
    table.insert(
        "sidebar_width".into(),
        toml::Value::Integer(config.sidebar_width as i64),
    );
    table.insert(
        "tab_width".into(),
        toml::Value::Integer(config.tab_width as i64),
    );
    table.insert(
        "block_compact".into(),
        toml::Value::Boolean(config.block_compact),
    );
    table.insert(
        "command_history_enabled".into(),
        toml::Value::Boolean(config.command_history_enabled),
    );
    table.insert("ai_enabled".into(), toml::Value::Boolean(config.ai_enabled));
    table.insert(
        "agent_enabled".into(),
        toml::Value::Boolean(config.agent_enabled),
    );
    table.insert(
        "notify_long_blocks".into(),
        toml::Value::Boolean(config.notify_long_blocks),
    );
    table.insert(
        "allow_remote_clipboard_write".into(),
        toml::Value::Boolean(config.allow_remote_clipboard_write),
    );

    let mut colors = toml::Table::new();
    colors.insert(
        "foreground".into(),
        toml::Value::String(rgba_to_hex(&config.foreground)),
    );
    colors.insert(
        "background".into(),
        toml::Value::String(rgba_to_hex(&config.background)),
    );
    colors.insert(
        "cursor".into(),
        toml::Value::String(rgba_to_hex(&config.cursor)),
    );
    colors.insert(
        "cursor_foreground".into(),
        toml::Value::String(rgba_to_hex(&config.cursor_foreground)),
    );
    table.insert("colors".into(), toml::Value::Table(colors));

    let content = table.to_string();
    let tmp_path = path.with_extension("toml.tmp");
    write_private_file(&tmp_path, content.as_bytes())
        .map_err(|err| format!("write config {}: {err}", tmp_path.display()))?;

    // Keep one known-good snapshot. If creating the backup fails, leave the
    // live file untouched instead of weakening the safety guarantee.
    if let Some(existing) = existing {
        let backup = path.with_extension("toml.bak");
        if let Err(err) = write_private_file(&backup, existing.as_bytes()) {
            let _ = fs::remove_file(&tmp_path);
            return Err(format!("write config backup {}: {err}", backup.display()));
        }
    }

    if let Err(err) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("replace config {}: {err}", path.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shell selection
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub(crate) fn find_executable_in_path(exe_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(exe_name))
        .find(|candidate| is_executable(candidate))
}

pub(crate) fn choose_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
    // Explicit config / env var wins (needed when PATH is stripped by launchers like wofi).
    if let Some(path) = configured_shell {
        if is_executable(Path::new(path)) {
            let shell_name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if shell_name == "rsh" {
                if let Some(argv) = wrap_rsh_argv_in_interactive_bash(path) {
                    return argv;
                }
            }
            return vec![path.to_string()];
        }
        log::warn!(
            "Configured shell '{}' is not executable, falling back to auto-detection",
            path
        );
    }

    // Prefer rsh when it's on PATH.
    if let Some(rsh_path) = find_executable_in_path("rsh") {
        if let Some(argv) = wrap_rsh_argv_in_interactive_bash(&rsh_path.to_string_lossy()) {
            return argv;
        }
        return vec![rsh_path.to_string_lossy().to_string()];
    }

    // Fallback: bash
    if let Some(bash_path) = find_executable_in_path("bash") {
        return vec![bash_path.to_string_lossy().to_string(), "-l".to_string()];
    }

    // Last resort: POSIX sh
    vec!["sh".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> RemoteHost {
        RemoteHost {
            name: "h".into(),
            host: "203.0.113.10".into(),
            user: Some("tester".into()),
            remote_shell: "/home/tester/.local/bin/rsh".into(),
            session: Some("staging-test".into()),
            ssh_args: Vec::new(),
            login_shell: true,
            // Off by default in tests so exact-argv assertions stay deterministic
            // (multiplex injects an env-dependent ControlPath).
            multiplex: false,
        }
    }

    #[test]
    fn login_shell_wraps_in_bash_lc() {
        let argv = build_remote_argv(&host());
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-t",
                "tester@203.0.113.10",
                "bash -lc 'exec /home/tester/.local/bin/rsh --session staging-test'",
            ]
        );
    }

    #[test]
    fn no_login_shell_passes_command_bare() {
        let mut h = host();
        h.login_shell = false;
        let argv = build_remote_argv(&h);
        assert_eq!(
            argv.last().unwrap(),
            "/home/tester/.local/bin/rsh --session staging-test"
        );
    }

    #[test]
    fn single_quotes_in_payload_are_escaped() {
        let mut h = host();
        h.session = Some("it's".into());
        let argv = build_remote_argv(&h);
        assert_eq!(
            argv.last().unwrap(),
            r#"bash -lc 'exec /home/tester/.local/bin/rsh --session it'\''s'"#
        );
    }

    #[test]
    fn rsh_wrapper_uses_interactive_bash() {
        let argv = wrap_rsh_argv_in_interactive_bash("/home/tester/.local/bin/rsh")
            .expect("bash should be available in the test environment");
        assert_eq!(argv[1], "-ic");
        assert_eq!(argv[2], "exec '/home/tester/.local/bin/rsh'");
    }

    #[test]
    fn multiplex_injects_controlmaster_flags() {
        let mut h = host();
        h.multiplex = true;
        std::env::set_var("XDG_RUNTIME_DIR", std::env::temp_dir());
        let argv = build_remote_argv(&h);
        assert!(
            argv.iter().any(|a| a == "ControlMaster=auto"),
            "argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "ControlPersist=120"),
            "argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a.starts_with("ControlPath=")),
            "argv: {argv:?}"
        );
        // ControlMaster flags must precede the target.
        let target_idx = argv
            .iter()
            .position(|a| a == "tester@203.0.113.10")
            .unwrap();
        let cm_idx = argv.iter().position(|a| a == "ControlMaster=auto").unwrap();
        assert!(cm_idx < target_idx);
    }

    #[test]
    fn no_multiplex_omits_controlmaster_flags() {
        let argv = build_remote_argv(&host());
        assert!(
            !argv.iter().any(|a| a.contains("ControlMaster")),
            "argv: {argv:?}"
        );
    }

    #[test]
    fn missing_remote_hosts_section_has_no_implicit_destinations() {
        let table = "font = 'monospace 12'".parse::<toml::Table>().unwrap();
        let remote_hosts = if table.contains_key("remote_hosts") {
            parse_remote_hosts(&table)
        } else {
            default_remote_hosts()
        };
        assert!(remote_hosts.is_empty());
    }

    #[test]
    fn explicit_empty_remote_hosts_stays_empty() {
        let table = "remote_hosts = []".parse::<toml::Table>().unwrap();
        let remote_hosts = if table.contains_key("remote_hosts") {
            parse_remote_hosts(&table)
        } else {
            default_remote_hosts()
        };
        assert!(remote_hosts.is_empty());
    }

    #[test]
    fn remote_host_toml_round_trips() {
        let original = host();
        let mut table = toml::Table::new();
        table.insert(
            "remote_hosts".into(),
            toml::Value::Array(vec![remote_host_to_toml(&original)]),
        );
        let parsed = parse_remote_hosts(&table);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, original.name);
        assert_eq!(parsed[0].host, original.host);
        assert_eq!(parsed[0].user, original.user);
        assert_eq!(parsed[0].remote_shell, original.remote_shell);
        assert_eq!(parsed[0].session, original.session);
        assert_eq!(parsed[0].login_shell, original.login_shell);
        assert_eq!(parsed[0].multiplex, original.multiplex);
    }

    #[test]
    fn safe_mode_removes_external_and_persistent_state() {
        let (mut config, _, _) = load_config();
        config.shell = Some("/custom/shell".into());
        config.startup_commands = Some("touch /tmp/should-not-run".into());
        config.command_history_enabled = true;
        config.command_history_path = Some("/tmp/history".into());
        config.block_history_path = Some("/tmp/blocks".into());
        config.ai_enabled = true;
        config.agent_enabled = true;
        config.notify_long_blocks = true;
        config.allow_remote_clipboard_write = true;
        config.remote_hosts.push(host());

        config.apply_safe_mode();

        assert!(matches!(config.terminal_mode, TerminalMode::Vte));
        assert!(config.shell.is_none());
        assert!(config.startup_commands.is_none());
        assert!(!config.command_history_enabled);
        assert!(config.command_history_path.is_none());
        assert!(config.block_history_path.is_none());
        assert!(!config.ai_enabled);
        assert!(!config.agent_enabled);
        assert!(!config.notify_long_blocks);
        assert!(!config.allow_remote_clipboard_write);
        assert!(config.remote_hosts.is_empty());
    }
}
