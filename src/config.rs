use gtk::gdk::RGBA;
use gtk::glib;
use relm4::gtk;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::keybindings::KeybindingMap;
use jterm_core::host::find_executable_in_path;
use jterm_core::process::shell_single_quote;

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

fn resolve_sidebar_visibility(explicit: Option<bool>, placement: TabPlacement) -> bool {
    explicit.unwrap_or(placement == TabPlacement::Sidebar)
}

/// When to check whether a newer jsh has been published. Shared with the other
/// terminals so one config vocabulary covers the family.
pub use jterm_core::jsh_install::UpdateCheck as JshUpdateCheck;

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
    /// Shell to launch on the remote side (default "jsh").
    pub remote_shell: String,
    /// Stable session id passed to the remote jsh for resume-on-reconnect.
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

fn wrap_exec_in_login_bash(command: &str) -> String {
    format!(
        "bash -lc {}",
        shell_single_quote(&format!("exec {command}"))
    )
}

fn wrap_jsh_argv_in_interactive_bash(jsh_path: &str) -> Option<Vec<String>> {
    let bash_path = find_executable_in_path("bash")?;
    Some(vec![
        bash_path.to_string_lossy().to_string(),
        "-ic".to_string(),
        // Keep the executable and any later session arguments as structured
        // argv. `bash -c` assigns the first post-command argument to $0 and
        // the rest to $@; no shell reconstruction of either value is needed.
        "exec \"$0\" \"$@\"".to_string(),
        jsh_path.to_string(),
    ])
}

pub(crate) fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.len() <= 1024 && !session_id.chars().any(char::is_control)
}

/// Apply a saved jsh session id to either a direct jsh argv or the exact
/// interactive-bash wrapper generated above. Returns whether the id was
/// applied, allowing callers to expose the matching environment marker.
pub(crate) fn shell_argv_with_session(
    shell_argv: &[String],
    session_id: Option<&str>,
) -> (Vec<String>, bool) {
    let direct_jsh = shell_argv
        .first()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        == Some("jsh");
    let wrapped_jsh = shell_argv.len() >= 4
        && shell_argv
            .first()
            .and_then(|path| Path::new(path).file_name())
            .and_then(|name| name.to_str())
            == Some("bash")
        && shell_argv[1] == "-ic"
        && shell_argv[2] == "exec \"$0\" \"$@\""
        && Path::new(&shell_argv[3])
            .file_name()
            .and_then(|name| name.to_str())
            == Some("jsh");

    let mut argv = shell_argv.to_vec();
    let applied = match session_id {
        Some(session_id) if (direct_jsh || wrapped_jsh) && valid_session_id(session_id) => {
            argv.push("--session".to_string());
            argv.push(session_id.to_string());
            true
        }
        Some(_) if direct_jsh || wrapped_jsh => {
            log::warn!("Ignoring invalid saved jsh session id");
            false
        }
        _ => false,
    };
    (argv, applied)
}

/// Build the local argv that connects to a remote host via ssh.
/// Produces e.g. `["ssh", "-t", "-p", "2222", "mm@100.x.x.x", "jsh --session home-main"]`.
pub(crate) fn build_remote_argv(host: &RemoteHost) -> Vec<String> {
    let target = match &host.user {
        Some(u) => format!("{u}@{}", host.host),
        None => host.host.clone(),
    };
    let mut remote_cmd = host.remote_shell.clone();
    if let Some(sid) = &host.session {
        remote_cmd.push_str(" --session ");
        // `ssh` passes its trailing command through the remote login shell.
        // Session ids are data, not shell syntax, so preserve them as exactly
        // one argument even when they contain whitespace or metacharacters.
        remote_cmd.push_str(&shell_single_quote(sid));
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
    // End option parsing before the user-owned destination. In particular, a
    // host alias beginning with '-' must never be reinterpreted as an ssh flag.
    argv.push("--".to_string());
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
    /// When to look for a newer jsh. Installing always stays an explicit
    /// choice: this only governs whether the offer appears.
    pub(crate) jsh_update_check: JshUpdateCheck,
    /// Whether the left sidebar is visible. Older configs derive this from
    /// tab placement so sidebar tabs remain discoverable after an upgrade.
    pub(crate) sidebar_visible: bool,
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
    /// Show the agent-mode entry point (`Ctrl+Alt+G` / palette). Default
    /// on, but suppressed when `ai_enabled` is false. Independent toggle so
    /// users who like the per-block AI helpers but find agent mode too
    /// risky can disable the multi-turn loop without losing the rest.
    pub(crate) agent_enabled: bool,
    /// Hard cap on assistant turns per agent session. Once reached the
    /// session is sealed and the user must start a new one — this is a
    /// runaway-loop safety net, not a usability lever.
    pub(crate) agent_max_turns: u32,
    /// Provider wire protocol: anthropic, openai-compatible, or ollama.
    pub(crate) ai_provider: String,
    /// Provider API root. Endpoint suffixes are added by the AI client.
    pub(crate) ai_base_url: String,
    /// Provider-specific model id.
    pub(crate) ai_model: String,
    /// Per-request maximum output tokens.
    pub(crate) ai_max_tokens: u32,
    /// Optional sampling temperature (0.0..=2.0); None keeps the provider default.
    pub(crate) ai_temperature: Option<f32>,
    /// Redact secret-looking values before sending context to an AI provider.
    pub(crate) ai_redact_secrets: bool,
    /// Stream session AI panel replies as they generate instead of waiting
    /// for the complete response. Affects only the conversational panel;
    /// one-shot helpers and the agent loop always wait for the full reply.
    pub(crate) ai_stream: bool,
    /// Optional path to a 0600 file holding the provider API key, so the key
    /// never has to live in the process environment or this config file.
    pub(crate) ai_api_key_file: Option<String>,
    pub(crate) notify_long_blocks: bool,
    pub(crate) notify_long_block_threshold_ms: u64,
    pub(crate) show_repo_strip: bool,
    /// Saved SSH targets selectable from the context menu.
    pub(crate) remote_hosts: Vec<RemoteHost>,
}

impl Config {
    /// Replace the complete configuration with an isolated, built-in VTE
    /// profile. This deliberately ignores both the user's file and JTERM1_*
    /// appearance/behavior overrides, making safe mode useful for diagnosis.
    #[cfg(test)]
    pub(crate) fn apply_safe_mode(&mut self) {
        *self = Self::safe_defaults();
    }

    pub(crate) fn safe_defaults() -> Self {
        let themes = builtin_themes();
        let theme = &themes[0];
        Self {
            window_opacity: 0.95,
            terminal_scrollback_lines: 5_000,
            font_desc: "SauceCodePro Nerd Font Mono 14".to_string(),
            default_font_scale: 1.0,
            theme_name: theme.name.clone(),
            foreground: theme.foreground,
            background: theme.background,
            cursor: theme.cursor,
            cursor_foreground: theme.cursor_foreground,
            palette: theme.palette,
            shell: None,
            startup_commands: None,
            terminal_mode: TerminalMode::Vte,
            tab_placement: TabPlacement::Sidebar,
            sidebar_view: SidebarView::Tabs,
            jsh_update_check: JshUpdateCheck::default(),
            sidebar_visible: true,
            sidebar_width: 220,
            tab_width: 180,
            ansi_cache_capacity: 256,
            max_visible_blocks: 200,
            output_batch_min_ms: 10,
            output_batch_max_ms: 100,
            lazy_load_threshold: 1_000,
            truncation_threshold_lines: 50_000,
            finished_block_viewport_rows: 24,
            finished_block_max_expanded_rows: 5_000,
            max_collapsed_output_lines: 25,
            virtual_scroll_margin: 1,
            command_history_enabled: false,
            command_history_path: None,
            command_history_max_entries: 10_000,
            block_history_path: None,
            block_history_compress: true,
            block_compact: false,
            editor_input: true,
            allow_remote_clipboard_write: false,
            mouse_reporting_enabled: true,
            focus_reporting_enabled: true,
            scroll_reporting_enabled: true,
            preserve_live_scrollback: false,
            ai_enabled: false,
            agent_enabled: false,
            agent_max_turns: 20,
            ai_provider: "anthropic".to_string(),
            ai_base_url: "https://api.anthropic.com".to_string(),
            ai_model: "claude-sonnet-4-6".to_string(),
            ai_max_tokens: 1_024,
            ai_temperature: None,
            ai_redact_secrets: true,
            ai_stream: true,
            ai_api_key_file: None,
            notify_long_blocks: false,
            notify_long_block_threshold_ms: 10_000,
            show_repo_strip: false,
            remote_hosts: Vec::new(),
        }
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

fn env_f32(name: &str) -> Option<f32> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
}

fn env_bool(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

fn env_rgba(name: &str) -> Option<RGBA> {
    env_string(name).and_then(|v| RGBA::parse(&v).ok())
}

fn normalize_ai_provider(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "openai-compatible" | "openai_compatible" => Some("openai-compatible"),
        "ollama" => Some("ollama"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// File config
// ---------------------------------------------------------------------------

pub(crate) fn config_file_path() -> PathBuf {
    let override_path = std::env::var_os("JTERM1_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    resolve_config_file_path(override_path, glib::user_config_dir())
}

fn resolve_config_file_path(override_path: Option<PathBuf>, user_config_dir: PathBuf) -> PathBuf {
    override_path.unwrap_or_else(|| user_config_dir.join("jterm1").join("config.toml"))
}

#[cfg(test)]
mod config_path_tests {
    use super::*;

    #[test]
    fn explicit_config_path_wins_over_xdg_default() {
        assert_eq!(
            resolve_config_file_path(
                Some(PathBuf::from("/tmp/自定义.toml")),
                PathBuf::from("/xdg")
            ),
            PathBuf::from("/tmp/自定义.toml")
        );
        assert_eq!(
            resolve_config_file_path(None, PathBuf::from("/xdg")),
            PathBuf::from("/xdg/jterm1/config.toml")
        );
    }
}

pub(crate) fn default_command_history_path() -> String {
    glib::user_state_dir()
        .join("jterm1")
        .join("history.jsonl")
        .to_string_lossy()
        .into_owned()
}

fn safe_absolute_history_path(value: Option<String>, setting: &str) -> Option<String> {
    match value {
        Some(path)
            if !path.trim().is_empty()
                && path.chars().count() <= 16 * 1_024
                && !path.chars().any(char::is_control)
                && Path::new(&path).is_absolute() =>
        {
            Some(path)
        }
        Some(_) => {
            log::warn!("{setting} is not a safe absolute path; using its safe default");
            None
        }
        None => None,
    }
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
    jsh_update_check: Option<String>,
    sidebar_visible: Option<bool>,
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
    ai_provider: Option<String>,
    ai_base_url: Option<String>,
    ai_model: Option<String>,
    ai_max_tokens: Option<u32>,
    ai_temperature: Option<f32>,
    ai_redact_secrets: Option<bool>,
    ai_stream: Option<bool>,
    ai_api_key_file: Option<String>,
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
        jsh_update_check: table
            .get("jsh_update_check")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        sidebar_visible: table.get("sidebar_visible").and_then(|v| v.as_bool()),
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
        ai_provider: table
            .get("ai_provider")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ai_base_url: table
            .get("ai_base_url")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ai_model: table
            .get("ai_model")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ai_max_tokens: table
            .get("ai_max_tokens")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        ai_temperature: table
            .get("ai_temperature")
            .and_then(|v| v.as_float())
            .map(|v| v as f32),
        ai_redact_secrets: table.get("ai_redact_secrets").and_then(|v| v.as_bool()),
        ai_stream: table.get("ai_stream").and_then(|v| v.as_bool()),
        ai_api_key_file: table
            .get("ai_api_key_file")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        notify_long_blocks: table.get("notify_long_blocks").and_then(|v| v.as_bool()),
        notify_long_block_threshold_ms: table
            .get("notify_long_block_threshold_ms")
            .and_then(|v| v.as_integer())
            .and_then(|v| u64::try_from(v).ok()),
        show_repo_strip: table.get("show_repo_strip").and_then(|v| v.as_bool()),
        remote_hosts,
    }
}

fn remote_text_is_safe(value: &str, allow_whitespace: bool, max_chars: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= max_chars
        && !value.chars().any(char::is_control)
        && (allow_whitespace || !value.chars().any(char::is_whitespace))
}

fn ssh_argument_is_safe(value: &str) -> bool {
    value.chars().count() <= 16 * 1_024 && !value.chars().any(char::is_control)
}

/// Parse `[[remote_hosts]]` array-of-tables. Entries missing a host or carrying
/// unsafe semantic values are skipped, matching the validator's safe-default
/// contract even when startup continues after reporting configuration errors.
fn parse_remote_hosts(table: &toml::Table) -> Vec<RemoteHost> {
    let Some(arr) = table.get("remote_hosts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut names = HashSet::new();
    arr.iter()
        .filter_map(|v| v.as_table())
        .filter_map(|t| {
            let host = t.get("host").and_then(|v| v.as_str())?.to_string();
            if !remote_text_is_safe(&host, false, 1_024) {
                return None;
            }
            let name = match t.get("name").and_then(|v| v.as_str()) {
                Some(value) if remote_text_is_safe(value, true, 256) => value.to_string(),
                Some(_) => return None,
                None => host.clone(),
            };
            // Session snapshots use the display name as the stable profile
            // identifier. Keep the first safe definition and reject later
            // duplicates so restore can never silently select another host.
            if !names.insert(name.clone()) {
                return None;
            }
            let user = t
                .get("user")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if user
                .as_deref()
                .is_some_and(|value| !remote_text_is_safe(value, false, 256))
            {
                return None;
            }
            let remote_shell = t
                .get("remote_shell")
                .and_then(|v| v.as_str())
                .unwrap_or("jsh")
                .to_string();
            if !remote_text_is_safe(&remote_shell, true, 16 * 1_024) {
                return None;
            }
            let session = t
                .get("session")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if session
                .as_deref()
                .is_some_and(|value| !remote_text_is_safe(value, true, 1_024))
            {
                return None;
            }
            let ssh_args: Vec<String> = match t.get("ssh_args").and_then(|v| v.as_array()) {
                Some(arguments) if arguments.iter().all(toml::Value::is_str) => arguments
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect(),
                Some(_) => return None,
                None => Vec::new(),
            };
            if ssh_args.len() > 128 || ssh_args.iter().any(|value| !ssh_argument_is_safe(value)) {
                return None;
            }
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
        safe_absolute_history_path(
            std::env::var("JTERM1_COMMAND_HISTORY_PATH")
                .ok()
                .or(fc.command_history_path),
            "command_history_path",
        )
        .unwrap_or_else(default_command_history_path)
    });
    let command_history_max_entries = fc
        .command_history_max_entries
        .unwrap_or(10_000)
        .clamp(100, 100_000);
    let block_history_path = safe_absolute_history_path(
        std::env::var("JTERM1_HISTORY_PATH")
            .ok()
            .or(fc.block_history_path),
        "block_history_path",
    );
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
    let jsh_update_check =
        JshUpdateCheck::parse(&fc.jsh_update_check.unwrap_or_else(|| "daily".to_string()));
    let sidebar_visible = resolve_sidebar_visibility(fc.sidebar_visible, tab_placement);
    let sidebar_width = fc.sidebar_width.unwrap_or(220).clamp(120, 800);
    let tab_width = fc.tab_width.unwrap_or(180).clamp(80, 480);

    let requested_ai_provider = env_string("JTERM1_AI_PROVIDER")
        .or(fc.ai_provider)
        .unwrap_or_else(|| "anthropic".to_string());
    let ai_provider = normalize_ai_provider(&requested_ai_provider)
        .unwrap_or_else(|| {
            log::warn!(
                "Unknown ai_provider '{}', using anthropic",
                requested_ai_provider.trim()
            );
            "anthropic"
        })
        .to_string();
    let (default_ai_model, default_ai_base_url) = match ai_provider.as_str() {
        "openai-compatible" => ("gpt-4o-mini", "https://api.openai.com/v1"),
        "ollama" => ("codellama:7b", "http://localhost:11434"),
        _ => ("claude-sonnet-4-6", "https://api.anthropic.com"),
    };
    let ai_model = env_string("JTERM1_AI_MODEL")
        .or(fc.ai_model)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| default_ai_model.to_string());
    let ai_base_url = env_string("JTERM1_AI_BASE_URL")
        .or(fc.ai_base_url)
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| default_ai_base_url.to_string())
        .trim_end_matches('/')
        .to_string();

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
        jsh_update_check,
        sidebar_visible,
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
        ai_enabled: env_bool("JTERM1_AI_ENABLED")
            .or(fc.ai_enabled)
            .unwrap_or(true),
        agent_enabled: env_bool("JTERM1_AGENT_ENABLED")
            .or(fc.agent_enabled)
            .unwrap_or(true),
        agent_max_turns: env_u32("JTERM1_AGENT_MAX_TURNS")
            .or(fc.agent_max_turns)
            .unwrap_or(20)
            .clamp(1, 100),
        ai_provider,
        ai_base_url,
        ai_model,
        ai_max_tokens: env_u32("JTERM1_AI_MAX_TOKENS")
            .or(fc.ai_max_tokens)
            .unwrap_or(1_024)
            .clamp(1, 32_768),
        ai_temperature: env_f32("JTERM1_AI_TEMPERATURE")
            .or(fc.ai_temperature)
            .filter(|t| t.is_finite() && (0.0..=2.0).contains(t)),
        ai_redact_secrets: env_bool("JTERM1_AI_REDACT_SECRETS")
            .or(fc.ai_redact_secrets)
            .unwrap_or(true),
        ai_stream: env_bool("JTERM1_AI_STREAM")
            .or(fc.ai_stream)
            .unwrap_or(true),
        // Unlike the other JTERM1_* overrides, the key-path override is applied
        // at client construction (`jterm_core::ai::resolve_api_key_file`), so
        // the environment-managed path can never be persisted back to TOML.
        ai_api_key_file: fc.ai_api_key_file.filter(|value| !value.trim().is_empty()),
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

/// Load no external configuration at all. Unlike applying a partial override
/// after `load_config`, this cannot block on or inherit a user-selected config
/// path, and it also resets custom keybindings.
pub(crate) fn load_safe_config() -> (Config, Vec<Theme>, KeybindingMap) {
    (
        Config::safe_defaults(),
        builtin_themes(),
        KeybindingMap::from_defaults(),
    )
}

// ---------------------------------------------------------------------------
// Config serialization helpers
// ---------------------------------------------------------------------------

pub(crate) fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8
    )
}

// ---------------------------------------------------------------------------
// Shell selection
// ---------------------------------------------------------------------------

/// Resolve a configured `shell = ` token to an executable file.
///
/// The exec-bit check and the "a bare name is a `PATH` lookup, never `./name`"
/// rule both live in [`jterm_core::host`] now; the local copy predated
/// `find_executable_in_path` being exec-bit-checked. A bare name resolving
/// through `PATH` is new here — it used to warn and auto-detect — and it is what
/// makes `shell = "bash"` work under a launcher that strips nothing but leaves
/// the shell unqualified.
fn resolve_configured_shell(token: &str) -> Option<PathBuf> {
    crate::host::resolve_configured_program(token, std::env::var_os("PATH").as_deref())
}

pub(crate) fn choose_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
    // Explicit config / env var wins (needed when PATH is stripped by launchers like wofi).
    if let Some(token) = configured_shell {
        match resolve_configured_shell(token) {
            Some(resolved) => {
                let path = resolved.to_string_lossy().to_string();
                let shell_name = resolved
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if shell_name == "jsh" {
                    if let Some(argv) = wrap_jsh_argv_in_interactive_bash(&path) {
                        return argv;
                    }
                }
                return vec![path];
            }
            None => log::warn!(
                "Configured shell '{token}' is not an executable file, falling back to auto-detection"
            ),
        }
    }

    // Prefer jsh when it's on PATH.
    if let Some(jsh_path) = find_executable_in_path("jsh") {
        if let Some(argv) = wrap_jsh_argv_in_interactive_bash(&jsh_path.to_string_lossy()) {
            return argv;
        }
        return vec![jsh_path.to_string_lossy().to_string()];
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
            remote_shell: "/home/tester/.local/bin/jsh".into(),
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
                "--",
                "tester@203.0.113.10",
                r#"bash -lc 'exec /home/tester/.local/bin/jsh --session '"'"'staging-test'"'"''"#,
            ]
        );
    }

    /// Replaces `configured_shell_must_be_an_absolute_executable`. An absolute
    /// path still has to be an executable file, but a bare name is now a `PATH`
    /// lookup instead of a rejection — the family rule from
    /// `jterm_core::host::resolve_configured_program`.
    #[test]
    fn configured_shell_resolves_absolute_paths_and_path_lookups() {
        assert_eq!(
            resolve_configured_shell("/bin/sh"),
            Some(PathBuf::from("/bin/sh"))
        );
        // The lookup, not the caller's cwd: whatever `sh` is on PATH.
        assert_eq!(
            resolve_configured_shell("sh"),
            find_executable_in_path("sh")
        );
        // Not a file at all, and not something to keep looking for.
        assert_eq!(resolve_configured_shell("/bin"), None);
        assert_eq!(resolve_configured_shell(""), None);
        assert_eq!(
            resolve_configured_shell("/definitely/not/here/jsh"),
            None,
            "a missing absolute path must not fall through to a PATH lookup"
        );
    }

    #[test]
    fn no_login_shell_passes_command_bare() {
        let mut h = host();
        h.login_shell = false;
        let argv = build_remote_argv(&h);
        assert_eq!(
            argv.last().unwrap(),
            "/home/tester/.local/bin/jsh --session 'staging-test'"
        );
    }

    #[test]
    fn session_payload_is_one_shell_argument() {
        let mut h = host();
        h.login_shell = false;
        h.session = Some("it's; printf injected".into());
        let argv = build_remote_argv(&h);
        assert_eq!(
            argv.last().unwrap(),
            r#"/home/tester/.local/bin/jsh --session 'it'"'"'s; printf injected'"#
        );
    }

    #[test]
    fn jsh_wrapper_uses_interactive_bash() {
        let argv = wrap_jsh_argv_in_interactive_bash("/home/tester/.local/bin/jsh")
            .expect("bash should be available in the test environment");
        assert_eq!(argv[1], "-ic");
        assert_eq!(
            &argv[2..],
            ["exec \"$0\" \"$@\"", "/home/tester/.local/bin/jsh"]
        );
    }

    #[test]
    fn saved_session_is_applied_to_direct_and_wrapped_jsh_argv() {
        let direct = vec!["/usr/bin/jsh".to_string()];
        let (direct, applied) = shell_argv_with_session(&direct, Some("session one"));
        assert!(applied);
        assert_eq!(&direct[1..], ["--session", "session one"]);

        let wrapped = vec![
            "/usr/bin/bash".to_string(),
            "-ic".to_string(),
            "exec \"$0\" \"$@\"".to_string(),
            "/usr/bin/jsh".to_string(),
        ];
        let (wrapped, applied) = shell_argv_with_session(&wrapped, Some("session two"));
        assert!(applied);
        assert_eq!(&wrapped[4..], ["--session", "session two"]);

        let (_, applied) = shell_argv_with_session(&wrapped, Some("bad\nsession"));
        assert!(!applied);
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
        assert_eq!(argv[target_idx - 1], "--");
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
    fn history_paths_must_be_safe_and_absolute() {
        assert_eq!(
            safe_absolute_history_path(Some("/tmp/history.jsonl".to_string()), "history"),
            Some("/tmp/history.jsonl".to_string())
        );
        for path in ["Cargo.toml", "", "/tmp/bad\nname"] {
            assert!(safe_absolute_history_path(Some(path.to_string()), "history").is_none());
        }
    }

    #[test]
    fn unsafe_remote_hosts_fall_back_to_being_unavailable() {
        let table = concat!(
            "[[remote_hosts]]\n",
            "name = 'bad-host'\n",
            "host = 'bad host'\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'bad-session'\n",
            "host = 'session.example'\n",
            "session = \"line\\tbreak\"\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'bad-arguments'\n",
            "host = 'arguments.example'\n",
            "ssh_args = ['-p', 2222]\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'safe'\n",
            "host = 'safe.example'\n",
            "ssh_args = ['-p', '2222']\n",
        )
        .parse::<toml::Table>()
        .unwrap();
        let hosts = parse_remote_hosts(&table);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "safe");
        assert_eq!(hosts[0].host, "safe.example");
    }

    #[test]
    fn duplicate_remote_names_keep_only_the_first_destination() {
        let table = concat!(
            "[[remote_hosts]]\n",
            "name = 'shared-name'\n",
            "host = 'first.example'\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'shared-name'\n",
            "host = 'second.example'\n",
        )
        .parse::<toml::Table>()
        .unwrap();

        let hosts = parse_remote_hosts(&table);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, "first.example");
    }

    #[test]
    fn implicit_remote_name_may_use_the_full_valid_host_length() {
        let host = "h".repeat(300);
        let mut entry = toml::Table::new();
        entry.insert("host".to_string(), toml::Value::String(host.clone()));
        let mut table = toml::Table::new();
        table.insert(
            "remote_hosts".to_string(),
            toml::Value::Array(vec![toml::Value::Table(entry)]),
        );

        let hosts = parse_remote_hosts(&table);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, host);
    }

    #[test]
    fn ai_provider_aliases_normalize_to_wire_protocols() {
        for alias in ["anthropic", "Anthropic", "claude"] {
            assert_eq!(normalize_ai_provider(alias), Some("anthropic"));
        }
        for alias in ["openai", "OpenAI-Compatible", "openai_compatible"] {
            assert_eq!(normalize_ai_provider(alias), Some("openai-compatible"));
        }
        assert_eq!(normalize_ai_provider("ollama"), Some("ollama"));
        assert_eq!(normalize_ai_provider("unknown"), None);
    }

    #[test]
    fn sidebar_visibility_defaults_follow_tab_placement() {
        assert!(resolve_sidebar_visibility(None, TabPlacement::Sidebar));
        assert!(!resolve_sidebar_visibility(None, TabPlacement::TopBar));
        assert!(resolve_sidebar_visibility(Some(true), TabPlacement::TopBar));
        assert!(!resolve_sidebar_visibility(
            Some(false),
            TabPlacement::Sidebar
        ));
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
        assert_eq!(config.ai_provider, "anthropic");
        assert_eq!(config.ai_model, "claude-sonnet-4-6");
        assert_eq!(config.ai_base_url, "https://api.anthropic.com");
        assert_eq!(config.ai_max_tokens, 1_024);
        assert!(config.ai_redact_secrets);
        assert!(!config.notify_long_blocks);
        assert!(!config.allow_remote_clipboard_write);
        assert!(config.remote_hosts.is_empty());
    }
}
