use gtk::gdk::RGBA;
use gtk::glib;
use relm4::gtk;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::keybindings::KeybindingMap;
use jterm_core::host::{find_executable_in_path, is_executable_file};
use jterm_core::process::shell_single_quote;

const DEFAULT_FONT_DESC: &str = "Monospace 14";

// ---------------------------------------------------------------------------
// Terminal Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalMode {
    Block,
    Vte,
    /// One long-lived full-size VTE driven by the same OSC 133 lifecycle as
    /// Block, but without per-command block widgets. Experimental.
    Unified,
}

impl TerminalMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Vte => "vte",
            Self::Unified => "unified",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "block" => Some(Self::Block),
            "vte" => Some(Self::Vte),
            "unified" => Some(Self::Unified),
            _ => None,
        }
    }

    /// Block and Unified share the OSC 133 `TermView`; only their render
    /// backends differ. Conventional VTE panes use the other component.
    pub(crate) fn uses_term_view(self) -> bool {
        matches!(self, Self::Block | Self::Unified)
    }

    pub(crate) fn is_unified(self) -> bool {
        matches!(self, Self::Unified)
    }
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

/// Motion level for the optional ASCII organism. `None` means automatic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganismMotion {
    Full,
    Calm,
    Static,
}

impl OrganismMotion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Calm => "calm",
            Self::Static => "static",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "calm" => Some(Self::Calm),
            "static" => Some(Self::Static),
            _ => None,
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

pub(crate) const MAX_REMOTE_HOSTS: usize = 128;
pub(crate) const MAX_SESSION_ID_BYTES: usize =
    jterm_core::execution_journal::MAX_JSH_SESSION_ID_BYTES;
const MAX_REMOTE_ARGV_BYTES: usize = 512 * 1024;
const MAX_CONFIG_PATH_BYTES: usize = 16 * 1024;
const MAX_AI_BASE_URL_BYTES: usize = 4 * 1024;
const MAX_FONT_DESC_BYTES: usize = 1024;
const MAX_AI_IDENTIFIER_BYTES: usize = 1024;
const MAX_STARTUP_COMMANDS_BYTES: usize = jterm_core::review_input::MAX_REVIEW_INPUT_BYTES;

/// A saved SSH target. A new tab can be opened that runs the remote shell over
/// `ssh -t`, reusing all local PTY/terminal infrastructure (OSC 133 markers
/// emitted by the remote shell flow through ssh, so block mode works remotely).
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteHost {
    pub name: String,
    /// The ssh destination, or — when `docker` is set — the name of a running
    /// container.
    pub host: String,
    /// The ssh login, or the `docker exec -u` user inside the container.
    pub user: Option<String>,
    /// Reach `host` with `docker exec` instead of ssh. The container has to be
    /// running already: this attaches to one, it does not start one.
    ///
    /// `ssh_args`, `multiplex` and `login_shell` have no meaning here and are
    /// ignored, which is also what the shared launcher does with them.
    pub docker: bool,
    /// A jsh built on this machine for `deploy` to push, instead of the
    /// published release it would otherwise fetch. Without it, deployment on a
    /// machine whose jsh has no release — or with no network — spends a few
    /// seconds failing to reach the release host and then falls back to shell
    /// integration, which keeps blocks but none of jsh's own behaviour.
    ///
    /// Must be an absolute path, and must be a jsh the destination can run:
    /// the launcher checks the binary's own version banner after it lands, but
    /// nothing here can tell whether it was built for that libc.
    pub deploy_artifact: Option<String>,
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
    /// Put a jsh on the destination for the life of the session instead of
    /// hoping one is installed there. `off` (the default) keeps the historical
    /// behaviour: run `remote_shell` over plain ssh and take what is there.
    ///
    /// This is what makes a remote tab a *jterm* tab on a machine nobody has
    /// prepared — blocks, cwd tracking and exit codes all come from jsh, so
    /// without it a bare `sh` on the far side silently drops them.
    pub deploy: jterm_core::jsh_remote::Deploy,
}

/// Directory for ssh ControlMaster sockets. Prefers `$XDG_RUNTIME_DIR`, falls
/// back to `~/.cache/anvil`. Created if missing.
fn control_socket_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/anvil")))?;
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

/// The interactive-bash wrapper runs the *user's* rc, so it needs the system's
/// interactive bash — not whichever bash the PATH we inherited happens to name
/// first. A `nix develop`/`nix-shell` puts stdenv's bash ahead of the system
/// one, and that build has no programmable completion: no `complete` builtin,
/// and `progcomp`/`hostcomplete` are not shopt names. A stock `~/.bashrc`
/// sources `/usr/share/bash-completion/bash_completion`, so every one of its
/// directives fails and ~65 error lines land on the pane before the shell's
/// first prompt — where a continuous surface never clears them away.
fn interactive_bash_path() -> Option<std::path::PathBuf> {
    [
        "/usr/bin/bash",
        "/bin/bash",
        "/run/current-system/sw/bin/bash",
    ]
    .into_iter()
    .map(std::path::PathBuf::from)
    .find(|candidate| is_executable_file(candidate))
    .or_else(|| find_executable_in_path("bash"))
}

fn wrap_jsh_argv_in_interactive_bash(jsh_path: &str) -> Option<Vec<String>> {
    let bash_path = interactive_bash_path()?;
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
    jterm_core::execution_journal::is_valid_jsh_session_id(session_id)
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
pub(crate) fn checked_remote_argv(host: &RemoteHost) -> Result<Vec<String>, &'static str> {
    validate_remote_host(host)?;
    Ok(build_remote_argv(host))
}

/// Build a plain interactive SSH login for an unsaved process-observed Files
/// target. This deliberately has no trailing jsh command, deployment, saved
/// session, or new ControlMaster configuration. A validated process-observed
/// ControlPath may be retained so the plain login can reuse the live session;
/// the user asked for a terminal at the temporary destination, not for that
/// destination to become a saved Anvil profile.
pub(crate) fn checked_interactive_ssh_argv(host: &RemoteHost) -> Result<Vec<String>, &'static str> {
    validate_remote_host(host)?;
    if host.docker {
        return Err("a temporary SSH target cannot be a container");
    }
    let target = match &host.user {
        Some(user) => format!("{user}@{}", host.host),
        None => host.host.clone(),
    };
    let mut argv = vec!["ssh".to_string(), "-t".to_string()];
    argv.extend(host.ssh_args.iter().cloned());
    argv.push("--".to_string());
    argv.push(target);
    Ok(argv)
}

/// Low-level argv construction. Production consumers use
/// [`checked_remote_argv`] so the gate is immediately adjacent to execution.
pub(crate) fn build_remote_argv(host: &RemoteHost) -> Vec<String> {
    if host.deploy.is_enabled() {
        match jterm_core::jsh_remote::publish_launcher() {
            Ok(script) => return build_deployed_argv(host, &script),
            // Publishing the launcher is the only thing that can fail here, and
            // it fails for reasons that have nothing to do with the host. Plain
            // ssh still reaches the machine, so degrade to it rather than
            // refusing to open the tab at all.
            Err(err) => log::warn!(
                "Cannot publish jsh-remote.sh for {}: {err}; connecting without deployment",
                host.name
            ),
        }
    }
    if host.docker {
        return build_docker_argv(host);
    }
    build_plain_ssh_argv(host)
}

/// argv for a tab that deploys jsh, given a launcher already on disk. Split out
/// from [`build_remote_argv`] so it can be asserted without publishing anything.
fn build_deployed_argv(host: &RemoteHost, script: &std::path::Path) -> Vec<String> {
    // A container takes its user through `--docker-user`, not through an
    // `user@host` destination that `docker exec` would read as a container
    // name nobody has.
    let target = match (&host.user, host.docker) {
        (Some(u), false) => format!("{u}@{}", host.host),
        _ => host.host.clone(),
    };
    jterm_core::jsh_remote::launch_argv_with_script(
        script,
        &jterm_core::jsh_remote::RemoteTarget {
            destination: &target,
            docker: host.docker,
            docker_user: host.docker.then_some(host.user.as_deref()).flatten(),
            artifact: host.deploy_artifact.as_deref().map(std::path::Path::new),
            session: host.session.as_deref(),
            ssh_args: &host.ssh_args,
            deploy: host.deploy,
        },
    )
}

/// argv for a container tab that deploys nothing, for an image that already
/// carries the shell. The ssh path's counterpart is [`build_plain_ssh_argv`];
/// there is no connection to multiplex and no login shell to wrap, because
/// `docker exec` starts a process rather than a session.
fn build_docker_argv(host: &RemoteHost) -> Vec<String> {
    let mut argv = vec!["docker".to_string(), "exec".to_string(), "-it".to_string()];
    if let Some(user) = &host.user {
        argv.push("-u".to_string());
        argv.push(user.clone());
    }
    argv.push(host.host.clone());
    argv.push(host.remote_shell.clone());
    if let Some(sid) = &host.session {
        argv.push("--session".to_string());
        argv.push(sid.clone());
    }
    argv
}

fn build_plain_ssh_argv(host: &RemoteHost) -> Vec<String> {
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
    pub(crate) max_visible_blocks: u32,
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
    pub(crate) ascii_organism_enabled: bool,
    pub(crate) ascii_organism_motion: Option<OrganismMotion>,
    /// Allow OSC 52 SET (`\e]52;c;<base64>\e\\`) from remote/local apps to
    /// overwrite the system clipboard. Off by default — a malicious or buggy
    /// remote process can otherwise silently replace the user's clipboard
    /// (OWASP-style concern). Most users enable this only on trusted hosts.
    pub(crate) allow_remote_clipboard_write: bool,
    pub(crate) mouse_reporting_enabled: bool,
    pub(crate) focus_reporting_enabled: bool,
    pub(crate) scroll_reporting_enabled: bool,
    pub(crate) preserve_live_scrollback: bool,
    /// Show anvil-side AI surfaces (per-block error explain button, the
    /// session AI panel, and the `?` palette prefix). Default on; flip to
    /// `false` to hide all AI UI even when API keys are present. The actual
    /// network call still only fires on an explicit user click — this just
    /// removes the entry points.
    pub(crate) ai_enabled: bool,
    /// Whether the persistent right-side AI Chats panel is open.
    pub(crate) ai_panel_visible: bool,
    /// Requested panel width in pixels.
    pub(crate) ai_panel_width: u32,
    /// Show the agent-mode entry point (`Ctrl+Alt+G` / palette). Default
    /// on, but suppressed when `ai_enabled` is false. Independent toggle so
    /// users who like the per-block AI helpers but find agent mode too
    /// risky can disable the multi-turn loop without losing the rest.
    pub(crate) agent_enabled: bool,
    /// Hard cap on assistant turns per agent session. Once reached the
    /// session is sealed and the user must start a new one — this is a
    /// runaway-loop safety net, not a usability lever.
    pub(crate) agent_max_turns: u32,
    /// Offer review-first corrections for narrowly classified failed commands.
    /// Verified local candidates may run only after one explicit exact-command
    /// action; edits and unverified candidates remain insert-only.
    pub(crate) command_correction_enabled: bool,
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
    /// Show the family-wide bottom status bar (`jterm_core::bottom_bar`).
    /// File-only toggle, not persisted by the settings dialog.
    pub(crate) bottom_bar: bool,
    /// A plain click in the live prompt places the shell's edit cursor there
    /// (`jterm_core::click_cursor`). Same key and default in every jterm.
    pub(crate) click_moves_cursor: bool,
    /// Saved SSH targets selectable from the context menu.
    pub(crate) remote_hosts: Vec<RemoteHost>,
}

impl Config {
    /// Replace the complete configuration with an isolated, built-in VTE
    /// profile. This deliberately ignores both the user's file and ANVIL_*
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
            font_desc: DEFAULT_FONT_DESC.to_string(),
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
            // Recovery mode must not initiate network or shared-cache work.
            jsh_update_check: JshUpdateCheck::Never,
            sidebar_visible: true,
            sidebar_width: 220,
            tab_width: 180,
            max_visible_blocks: 200,
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
            ascii_organism_enabled: false,
            ascii_organism_motion: None,
            allow_remote_clipboard_write: false,
            mouse_reporting_enabled: true,
            focus_reporting_enabled: true,
            scroll_reporting_enabled: true,
            preserve_live_scrollback: false,
            ai_enabled: false,
            ai_panel_visible: false,
            ai_panel_width: 360,
            agent_enabled: false,
            agent_max_turns: 20,
            command_correction_enabled: false,
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
            bottom_bar: true,
            click_moves_cursor: jterm_core::click_cursor::ENABLED_BY_DEFAULT,
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
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok())
}

fn env_f32(name: &str) -> Option<f32> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite())
}

fn finite_clamp_f64(value: Option<f64>, fallback: f64, min: f64, max: f64) -> f64 {
    value
        .filter(|value| value.is_finite())
        .unwrap_or(fallback)
        .clamp(min, max)
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
    let override_path = std::env::var_os("ANVIL_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    resolve_config_file_path(override_path, glib::user_config_dir())
}

fn resolve_config_file_path(override_path: Option<PathBuf>, user_config_dir: PathBuf) -> PathBuf {
    override_path.unwrap_or_else(|| user_config_dir.join("anvil").join("config.toml"))
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
            PathBuf::from("/xdg/anvil/config.toml")
        );
    }
}

pub(crate) fn default_command_history_path() -> String {
    glib::user_state_dir()
        .join("anvil")
        .join("history.jsonl")
        .to_string_lossy()
        .into_owned()
}

/// Private native-schema memory for the optional ASCII organism.
pub(crate) fn default_ascii_organism_memory_path() -> PathBuf {
    glib::user_state_dir()
        .join("anvil")
        .join("ascii-organism-native.json")
}

pub(crate) fn setting_text_is_safe(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
        && !crate::review_input::contains_visual_spoofing(value)
}

/// Apply one textual environment override without letting malformed or
/// visually deceptive text erase a valid file setting. The same normalized
/// bytes then feed parsers, process resolution, and UI labels.
fn resolve_setting_text(
    override_value: Option<String>,
    configured_value: Option<String>,
    max_bytes: usize,
) -> Option<String> {
    let normalize = |value: String| {
        let value = value.trim();
        setting_text_is_safe(value, max_bytes).then(|| value.to_string())
    };
    override_value
        .and_then(&normalize)
        .or_else(|| configured_value.and_then(normalize))
}

pub(crate) fn configured_path_is_safe(value: &str, require_absolute_or_home: bool) -> bool {
    setting_text_is_safe(value, MAX_CONFIG_PATH_BYTES)
        && (!require_absolute_or_home
            || Path::new(value).is_absolute()
            || (value.starts_with("~/") && !value.starts_with("~//")))
}

fn expand_configured_path_with(value: &str, home: Option<&Path>) -> Option<String> {
    if !configured_path_is_safe(value, true) {
        return None;
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let home = home.filter(|home| home.is_absolute())?;
        return home.join(rest).into_os_string().into_string().ok();
    }
    Some(value.to_string())
}

fn safe_history_path(value: Option<String>, setting: &str) -> Option<String> {
    match value {
        Some(path) => {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from);
            let expanded = expand_configured_path_with(&path, home.as_deref());
            if expanded.is_none() {
                log::warn!("{setting} is not a safe absolute or ~/ path; using its safe default");
            }
            expanded
        }
        None => None,
    }
}

pub(crate) fn ai_base_url_is_structurally_safe(value: &str) -> bool {
    let value = value.trim();
    if !setting_text_is_safe(value, MAX_AI_BASE_URL_BYTES)
        || !(value.starts_with("http://") || value.starts_with("https://"))
        || value.chars().any(char::is_whitespace)
        || value.contains(['?', '#', '\\'])
    {
        return false;
    }
    value.split_once("://").is_some_and(|(_, remainder)| {
        let authority = remainder.split('/').next().unwrap_or_default();
        !authority.is_empty() && !authority.contains('@')
    })
}

fn is_port(port: &str) -> bool {
    !port.is_empty() && port.chars().all(|digit| digit.is_ascii_digit())
}

fn is_loopback_authority(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        let Some((literal, port)) = rest.split_once(']') else {
            return false;
        };
        if !port.is_empty() && !port.strip_prefix(':').is_some_and(is_port) {
            return false;
        }
        literal
    } else {
        match authority.split_once(':') {
            Some((host, port)) if is_port(port) => host,
            Some(_) => return false,
            None => authority,
        }
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<std::net::Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback())
}

pub(crate) fn ai_base_url_is_safe(provider: &str, value: &str) -> bool {
    if !ai_base_url_is_structurally_safe(value) {
        return false;
    }
    let value = value.trim();
    let Some((scheme, remainder)) = value.split_once("://") else {
        return false;
    };
    let authority = remainder.split('/').next().unwrap_or_default();
    match scheme {
        "https" => normalize_ai_provider(provider).is_some(),
        "http" => {
            normalize_ai_provider(provider) == Some("ollama") && is_loopback_authority(authority)
        }
        _ => false,
    }
}

fn resolve_ai_base_url(requested: Option<String>, default: &str) -> String {
    match requested {
        None => default.to_string(),
        Some(value) if ai_base_url_is_structurally_safe(&value) => {
            value.trim().trim_end_matches('/').to_string()
        }
        Some(_) => String::new(),
    }
}

/// Return a user-facing diagnostic when the config exists but cannot be read
/// or parsed. Callers use this before hot reload and before any write so a
/// malformed hand-edited file is never silently replaced with defaults.
pub(crate) fn config_file_error() -> Option<String> {
    let path = config_file_path();
    match crate::config_store::read_config_text(&path) {
        Ok(Some(contents)) => contents
            .parse::<toml::Table>()
            .err()
            .map(|err| format!("{}: {err}", path.display())),
        Ok(None) => None,
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
    max_visible_blocks: Option<u32>,
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
    ascii_organism_enabled: Option<bool>,
    ascii_organism_motion: Option<String>,
    allow_remote_clipboard_write: Option<bool>,
    ai_enabled: Option<bool>,
    ai_panel_visible: Option<bool>,
    ai_panel_width: Option<u32>,
    agent_enabled: Option<bool>,
    agent_max_turns: Option<u32>,
    agent_auto_approve_readonly: Option<bool>,
    command_correction_enabled: Option<bool>,
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
    bottom_bar: Option<bool>,
    click_moves_cursor: Option<bool>,
    remote_hosts: Vec<RemoteHost>,
}

fn load_file_config() -> FileConfig {
    let path = config_file_path();
    let Ok(Some(contents)) = crate::config_store::read_config_text(&path) else {
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
        max_visible_blocks: table
            .get("max_visible_blocks")
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
        ascii_organism_enabled: table
            .get("ascii_organism_enabled")
            .and_then(|v| v.as_bool()),
        ascii_organism_motion: table
            .get("ascii_organism_motion")
            .and_then(|v| v.as_str())
            .map(str::to_string),
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
        ai_panel_visible: table.get("ai_panel_visible").and_then(|v| v.as_bool()),
        ai_panel_width: table
            .get("ai_panel_width")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        agent_enabled: table.get("agent_enabled").and_then(|v| v.as_bool()),
        agent_max_turns: table
            .get("agent_max_turns")
            .and_then(|v| v.as_integer())
            .and_then(|v| u32::try_from(v).ok()),
        agent_auto_approve_readonly: table
            .get("agent_auto_approve_readonly")
            .and_then(|v| v.as_bool()),
        command_correction_enabled: table
            .get("command_correction_enabled")
            .and_then(|v| v.as_bool()),
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
        bottom_bar: table
            .get(jterm_core::bottom_bar::CONFIG_KEY)
            .and_then(|v| v.as_bool()),
        click_moves_cursor: table.get("click_moves_cursor").and_then(|v| v.as_bool()),
        remote_hosts,
    }
}

pub(crate) fn remote_text_is_safe(value: &str, allow_whitespace: bool, max_chars: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= max_chars
        && !value.chars().any(char::is_control)
        && !crate::review_input::contains_visual_spoofing(value)
        && (allow_whitespace || !value.chars().any(char::is_whitespace))
}

fn ssh_argument_is_safe(value: &str) -> bool {
    value.chars().count() <= 16 * 1_024
        && value.len() <= 64 * 1_024
        && !value.chars().any(char::is_control)
        && !crate::review_input::contains_visual_spoofing(value)
}

fn remote_ssh_args_are_structured(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--" || !option.starts_with('-') || option.starts_with("--") {
            return false;
        }
        let bytes = option.as_bytes();
        if bytes.len() < 2 || !bytes[1].is_ascii_alphabetic() {
            return false;
        }
        let flag = bytes[1] as char;
        let takes_operand = "BbcDEeFIiJLlmOoPpQRSWw".contains(flag);
        if takes_operand {
            if bytes.len() == 2 {
                index += 1;
                if index >= args.len() {
                    return false;
                }
            }
        } else if !option[1..]
            .chars()
            .all(|value| "46AaCfgKkMNnqTtVvXxYy".contains(value))
        {
            return false;
        }
        index += 1;
    }
    true
}

/// Single application-level gate used at every connection and remote-fs argv
/// boundary. Runtime objects and restored snapshots do not get to rely on the
/// earlier, advisory config report.
pub(crate) fn validate_remote_host(host: &RemoteHost) -> Result<(), &'static str> {
    // An omitted display name inherits the target, whose documented limit is
    // 1024 characters; UI consumers still render only a bounded safe prefix.
    if !remote_text_is_safe(&host.name, true, 1_024) || host.name.len() > 4 * 1_024 {
        return Err("Remote profile name is invalid; edit it in Settings or config.toml.");
    }
    if !remote_text_is_safe(&host.host, false, 1_024)
        || host.host.len() > 4 * 1_024
        || host.host.starts_with('-')
    {
        return Err("Remote target is invalid; edit it in Settings or config.toml.");
    }
    if host.user.as_deref().is_some_and(|user| {
        !remote_text_is_safe(user, false, 256) || user.len() > 1024 || user.contains('@')
    }) {
        return Err("Remote user is invalid; edit it in Settings or config.toml.");
    }
    if !remote_text_is_safe(&host.remote_shell, true, 16 * 1_024)
        || host.remote_shell.len() > 64 * 1_024
    {
        return Err("Remote shell is invalid; edit it in config.toml.");
    }
    if host
        .session
        .as_deref()
        .is_some_and(|session| !valid_session_id(session))
    {
        return Err("Remote session id is invalid; edit it in config.toml.");
    }
    if host.ssh_args.len() > 128
        || host.ssh_args.iter().any(|arg| !ssh_argument_is_safe(arg))
        || !remote_ssh_args_are_structured(&host.ssh_args)
    {
        return Err("Remote SSH options are invalid; edit them in config.toml.");
    }
    if host.deploy_artifact.as_deref().is_some_and(|artifact| {
        !remote_text_is_safe(artifact, false, 4_096)
            || artifact.len() > 16 * 1_024
            || !Path::new(artifact).is_absolute()
    }) {
        return Err("Remote deployment artifact is invalid; edit it in config.toml.");
    }
    let argv_bytes = host
        .ssh_args
        .iter()
        .try_fold(
            host.host
                .len()
                .saturating_add(host.name.len())
                .saturating_add(host.remote_shell.len())
                .saturating_add(host.user.as_deref().map_or(0, str::len))
                .saturating_add(host.session.as_deref().map_or(0, str::len))
                .saturating_add(host.deploy_artifact.as_deref().map_or(0, str::len)),
            |total, argument| total.checked_add(argument.len()),
        )
        .unwrap_or(usize::MAX);
    if argv_bytes > MAX_REMOTE_ARGV_BYTES {
        return Err("Remote profile exceeds the execution byte budget.");
    }
    Ok(())
}

pub(crate) fn checked_remote_host(
    hosts: &[RemoteHost],
    index: usize,
) -> Result<&RemoteHost, &'static str> {
    if index >= MAX_REMOTE_HOSTS {
        return Err("Remote host index exceeds the supported 128-profile limit.");
    }
    let host = hosts
        .get(index)
        .ok_or("That remote host is no longer configured.")?;
    validate_remote_host(host)?;
    Ok(host)
}

/// Resolve one immutable configured profile through the live host list.
/// Numeric positions are presentation state, so a reorder may move the match;
/// edited, removed, invalid, out-of-range, or duplicated profiles fail closed.
pub(crate) fn unique_checked_remote_profile_index(
    hosts: &[RemoteHost],
    expected: &RemoteHost,
) -> Option<usize> {
    let mut matches = (0..hosts.len().min(MAX_REMOTE_HOSTS))
        .filter(|index| checked_remote_host(hosts, *index).is_ok_and(|host| host == expected));
    let index = matches.next()?;
    matches.next().is_none().then_some(index)
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
        .take(MAX_REMOTE_HOSTS)
        .filter_map(|v| v.as_table())
        .filter_map(|t| {
            let host = t.get("host").and_then(|v| v.as_str())?.to_string();
            if !remote_text_is_safe(&host, false, 1_024) || host.starts_with('-') {
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
                .is_some_and(|value| !remote_text_is_safe(value, false, 256) || value.contains('@'))
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
                .is_some_and(|value| !remote_text_is_safe(value, true, MAX_SESSION_ID_BYTES))
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
            let docker = t.get("docker").and_then(|v| v.as_bool()).unwrap_or(false);
            // Rejected rather than dropped, for the same reason a `deploy`
            // spelling this build does not understand rejects the host: a path
            // that is quietly ignored looks exactly like deployment working,
            // right up until the tab is a bash prompt with none of jsh in it.
            let deploy_artifact = match t.get("deploy_artifact") {
                None => None,
                Some(toml::Value::String(value)) => {
                    if !remote_text_is_safe(value, false, 4_096)
                        || !std::path::Path::new(value).is_absolute()
                    {
                        return None;
                    }
                    Some(value.to_string())
                }
                Some(_) => return None,
            };
            // A spelling this build does not understand rejects the host rather
            // than falling back to `off`. Silently downgrading `incognito` would
            // write jsh's dot-files into an account the user asked to leave
            // untouched, which is the one outcome the mode exists to prevent.
            let deploy = match t.get("deploy") {
                None => jterm_core::jsh_remote::Deploy::Off,
                Some(toml::Value::String(value)) => {
                    match jterm_core::jsh_remote::Deploy::parse(value) {
                        Some(deploy) => deploy,
                        None => return None,
                    }
                }
                Some(_) => return None,
            };
            let host = RemoteHost {
                name,
                host,
                user,
                remote_shell,
                session,
                ssh_args,
                login_shell,
                multiplex,
                deploy,
                docker,
                deploy_artifact,
            };
            validate_remote_host(&host).ok()?;
            Some(host)
        })
        .collect()
}

/// Serialize a `RemoteHost` back into a TOML table that `parse_remote_hosts`
/// round-trips. Optional fields are only emitted when present.
pub(crate) fn remote_host_to_toml(h: &RemoteHost) -> toml::Value {
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
    if h.docker {
        // Same rule as `deploy`: written only when on, so an ssh host does not
        // grow the key on a round trip.
        t.insert("docker".into(), toml::Value::Boolean(true));
    }
    if let Some(artifact) = &h.deploy_artifact {
        t.insert(
            "deploy_artifact".into(),
            toml::Value::String(artifact.clone()),
        );
    }
    if h.deploy.is_enabled() {
        // Only written when it is on, so a config file that never asked for
        // deployment does not grow a key after a round trip.
        t.insert(
            "deploy".into(),
            toml::Value::String(h.deploy.as_str().to_string()),
        );
    }
    toml::Value::Table(t)
}

/// Two worked entries a new destination can be copied from: one ssh target and
/// one running container. They exist because the two mistakes the grammar
/// cannot forgive are invisible in an empty list — the port belongs in
/// `ssh_args`, never as `host:port`, and the login belongs in `user`, never as
/// a `user@host` string that ssh would take literally as a hostname.
///
/// Only consulted when the file has no `remote_hosts` key at all. An explicit
/// list — including `remote_hosts = []` — always wins, so deleting these in the
/// settings dialog (which writes the key back) makes them stay gone.
fn default_remote_hosts() -> Vec<RemoteHost> {
    vec![
        RemoteHost {
            name: "dev-60".to_string(),
            host: "10.68.18.60".to_string(),
            user: Some("root".to_string()),
            docker: false,
            deploy_artifact: None,
            remote_shell: "jsh".to_string(),
            session: None,
            // 22 is ssh's default and could be omitted; it is spelled out so a
            // copied entry has the flag to change rather than one to remember.
            ssh_args: vec!["-p".to_string(), "22".to_string()],
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Persist,
        },
        RemoteHost {
            name: "myubuntu".to_string(),
            host: "myubuntu".to_string(),
            // The container user is `docker exec -u`; unset means the image's.
            user: None,
            docker: true,
            deploy_artifact: None,
            remote_shell: "jsh".to_string(),
            session: None,
            // Meaningless for docker, and the launcher ignores them.
            ssh_args: Vec::new(),
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Persist,
        },
    ]
}

// ---------------------------------------------------------------------------
// load_config
// ---------------------------------------------------------------------------

pub(crate) fn load_config() -> (Config, Vec<Theme>, KeybindingMap) {
    let fc = load_file_config();
    let themes = builtin_themes();

    // Resolve active theme
    let theme_name = resolve_setting_text(env_string("ANVIL_THEME"), fc.theme, 256)
        .unwrap_or_else(|| "default".to_string());
    let theme = themes
        .iter()
        .find(|t| t.name == theme_name)
        .unwrap_or(&themes[0]);

    // Priority: env var > config file > theme default
    let window_opacity = finite_clamp_f64(env_f64("ANVIL_OPACITY").or(fc.opacity), 0.95, 0.01, 1.0);
    let terminal_scrollback_lines = env_u32("ANVIL_SCROLLBACK")
        .or(fc.scrollback)
        .unwrap_or(5000)
        .min(1_000_000);
    let default_font_scale = finite_clamp_f64(
        env_f64("ANVIL_FONT_SCALE").or(fc.font_scale),
        1.0,
        0.1,
        10.0,
    );
    let font_desc = resolve_setting_text(env_string("ANVIL_FONT"), fc.font, MAX_FONT_DESC_BYTES)
        .unwrap_or_else(|| DEFAULT_FONT_DESC.to_string());

    let foreground = env_rgba("ANVIL_FG")
        .or_else(|| fc.foreground.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.foreground);
    let background = env_rgba("ANVIL_BG")
        .or_else(|| fc.background.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.background);
    let cursor = env_rgba("ANVIL_CURSOR")
        .or_else(|| fc.cursor.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.cursor);
    let cursor_foreground = env_rgba("ANVIL_CURSOR_FG")
        .or_else(|| {
            fc.cursor_foreground
                .as_deref()
                .and_then(|v| RGBA::parse(v).ok())
        })
        .unwrap_or(theme.cursor_foreground);

    // Block view optimization settings
    let max_visible_blocks = env_u32("ANVIL_MAX_BLOCKS")
        .or(fc.max_visible_blocks)
        .unwrap_or(200)
        .clamp(1, 100_000);
    let lazy_load_threshold = env_u32("ANVIL_LAZY_LINES")
        .or(fc.lazy_load_threshold)
        .unwrap_or(1000)
        .clamp(1, 10_000_000);
    let truncation_threshold_lines = env_u32("ANVIL_TRUNCATION_LINES")
        .or(fc.truncation_threshold_lines)
        .unwrap_or(50000)
        .clamp(1, 10_000_000);
    let finished_block_viewport_rows = env_u32("ANVIL_FINISHED_VIEWPORT_ROWS")
        .or(fc.finished_block_viewport_rows)
        .unwrap_or(24)
        .clamp(3, 5_000);
    let finished_block_max_expanded_rows = env_u32("ANVIL_FINISHED_MAX_EXPANDED_ROWS")
        .or(fc.finished_block_max_expanded_rows)
        .unwrap_or(5000)
        .clamp(finished_block_viewport_rows, 5000);
    let max_collapsed_output_lines = env_u32("ANVIL_MAX_COLLAPSED_LINES")
        .or(fc.max_collapsed_output_lines)
        .unwrap_or(25)
        .clamp(1, 1_000_000);
    let virtual_scroll_margin = env_u32("ANVIL_VSCROLL_MARGIN")
        .or(fc.virtual_scroll_margin)
        .unwrap_or(1)
        .min(10_000);
    let command_history_enabled = fc.command_history_enabled.unwrap_or(true);
    let command_history_path = if command_history_enabled {
        let requested = std::env::var("ANVIL_COMMAND_HISTORY_PATH")
            .ok()
            .or(fc.command_history_path)
            .unwrap_or_else(default_command_history_path);
        safe_history_path(Some(requested), "command_history_path")
    } else {
        None
    };
    let command_history_max_entries = fc
        .command_history_max_entries
        .unwrap_or(10_000)
        .clamp(100, 1_000_000);
    let block_history_path = safe_history_path(
        std::env::var("ANVIL_HISTORY_PATH")
            .ok()
            .or(fc.block_history_path),
        "block_history_path",
    );
    let block_history_compress = fc.block_history_compress.unwrap_or(true);
    let block_compact = match std::env::var("ANVIL_BLOCK_COMPACT").ok().as_deref() {
        Some("1") | Some("true") => Some(true),
        Some("0") | Some("false") => Some(false),
        _ => None,
    }
    .or(fc.block_compact)
    .unwrap_or(false);
    let shell = resolve_setting_text(env_string("ANVIL_SHELL"), fc.shell, MAX_CONFIG_PATH_BYTES);
    let startup_commands =
        resolve_setting_text(None, fc.startup_commands, MAX_STARTUP_COMMANDS_BYTES);

    // Block mode is anvil's defining experience and is required for the
    // command-completion events consumed by Agent and command history. Users
    // can still opt into the compatibility VTE backend explicitly.
    let terminal_mode_str = resolve_setting_text(env_string("ANVIL_MODE"), fc.terminal_mode, 64)
        .unwrap_or_else(|| "block".to_string());
    let terminal_mode = TerminalMode::parse(&terminal_mode_str).unwrap_or_else(|| {
        log::warn!("Unknown terminal_mode '{terminal_mode_str}', using block");
        TerminalMode::Block
    });

    let tab_placement = TabPlacement::parse(
        &resolve_setting_text(env_string("ANVIL_TAB_PLACEMENT"), fc.tab_placement, 64)
            .unwrap_or_else(|| "sidebar".to_string()),
    );
    let sidebar_view = SidebarView::parse(
        &resolve_setting_text(None, fc.sidebar_view, 64).unwrap_or_else(|| "tabs".to_string()),
    );
    let jsh_update_check = JshUpdateCheck::parse(
        &resolve_setting_text(None, fc.jsh_update_check, 64).unwrap_or_else(|| "daily".to_string()),
    );
    let sidebar_visible = resolve_sidebar_visibility(fc.sidebar_visible, tab_placement);
    let sidebar_width = fc.sidebar_width.unwrap_or(220).clamp(120, 800);
    let tab_width = fc.tab_width.unwrap_or(180).clamp(80, 480);
    let ascii_organism_enabled = env_bool("ANVIL_ASCII_ORGANISM_ENABLED")
        .or(fc.ascii_organism_enabled)
        .unwrap_or(false);
    let ascii_organism_motion = resolve_setting_text(
        env_string("ANVIL_ASCII_ORGANISM_MOTION"),
        fc.ascii_organism_motion,
        64,
    )
    .as_deref()
    .and_then(OrganismMotion::parse);

    let requested_ai_provider =
        resolve_setting_text(env_string("ANVIL_AI_PROVIDER"), fc.ai_provider, 64)
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
    let ai_model = resolve_setting_text(
        env_string("ANVIL_AI_MODEL"),
        fc.ai_model,
        MAX_AI_IDENTIFIER_BYTES,
    )
    .unwrap_or_else(|| default_ai_model.to_string());
    let ai_base_url = resolve_ai_base_url(
        env_string("ANVIL_AI_BASE_URL").or(fc.ai_base_url),
        default_ai_base_url,
    );
    let requested_agent_auto_approve = env_bool("ANVIL_AGENT_AUTO_APPROVE_READONLY")
        .or(fc.agent_auto_approve_readonly)
        .unwrap_or(false);
    if requested_agent_auto_approve {
        log::warn!(
            "agent_auto_approve_readonly is retired; every Agent proposal requires explicit approval"
        );
    }

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
        startup_commands,
        terminal_mode,
        tab_placement,
        sidebar_view,
        jsh_update_check,
        sidebar_visible,
        sidebar_width,
        tab_width,
        max_visible_blocks,
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
        ascii_organism_enabled,
        ascii_organism_motion,
        allow_remote_clipboard_write: fc.allow_remote_clipboard_write.unwrap_or(false),
        mouse_reporting_enabled: fc.mouse_reporting_enabled.unwrap_or(true),
        focus_reporting_enabled: fc.focus_reporting_enabled.unwrap_or(true),
        scroll_reporting_enabled: fc.scroll_reporting_enabled.unwrap_or(true),
        preserve_live_scrollback: fc.preserve_live_scrollback.unwrap_or(false),
        ai_enabled: env_bool("ANVIL_AI_ENABLED")
            .or(fc.ai_enabled)
            .unwrap_or(true),
        ai_panel_visible: fc.ai_panel_visible.unwrap_or(false),
        ai_panel_width: fc.ai_panel_width.unwrap_or(360).clamp(240, 1_200),
        agent_enabled: env_bool("ANVIL_AGENT_ENABLED")
            .or(fc.agent_enabled)
            .unwrap_or(true),
        agent_max_turns: env_u32("ANVIL_AGENT_MAX_TURNS")
            .or(fc.agent_max_turns)
            .unwrap_or(20)
            .clamp(1, 100),
        command_correction_enabled: env_bool("ANVIL_COMMAND_CORRECTION_ENABLED")
            .or(fc.command_correction_enabled)
            .unwrap_or(true),
        ai_provider,
        ai_base_url,
        ai_model,
        ai_max_tokens: env_u32("ANVIL_AI_MAX_TOKENS")
            .or(fc.ai_max_tokens)
            .unwrap_or(1_024)
            .clamp(64, 32_768),
        ai_temperature: env_f32("ANVIL_AI_TEMPERATURE")
            .or(fc.ai_temperature)
            .filter(|t| t.is_finite() && (0.0..=2.0).contains(t)),
        ai_redact_secrets: env_bool("ANVIL_AI_REDACT_SECRETS")
            .or(fc.ai_redact_secrets)
            .unwrap_or(true),
        ai_stream: env_bool("ANVIL_AI_STREAM").or(fc.ai_stream).unwrap_or(true),
        // Unlike the other ANVIL_* overrides, the key-path override is applied
        // at client construction (`jterm_core::ai::resolve_api_key_file`), so
        // the environment-managed path can never be persisted back to TOML.
        ai_api_key_file: fc
            .ai_api_key_file
            .filter(|value| configured_path_is_safe(value, true)),
        notify_long_blocks: fc.notify_long_blocks.unwrap_or(true),
        notify_long_block_threshold_ms: fc.notify_long_block_threshold_ms.unwrap_or(10_000),
        bottom_bar: fc
            .bottom_bar
            .unwrap_or(jterm_core::bottom_bar::ENABLED_BY_DEFAULT),
        click_moves_cursor: fc
            .click_moves_cursor
            .unwrap_or(jterm_core::click_cursor::ENABLED_BY_DEFAULT),
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

    // Fallback: bash. This one *is* the pane's shell rather than a wrapper
    // that execs itself away, so resolving it off the inherited PATH would
    // leave the user in a bash their rc was not written for.
    if let Some(bash_path) = interactive_bash_path() {
        return vec![bash_path.to_string_lossy().to_string(), "-l".to_string()];
    }

    // Last resort: POSIX sh
    vec!["sh".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_mode_parses_and_round_trips_every_backend() {
        for (text, mode) in [
            ("block", TerminalMode::Block),
            ("vte", TerminalMode::Vte),
            ("unified", TerminalMode::Unified),
        ] {
            assert_eq!(TerminalMode::parse(text), Some(mode));
            assert_eq!(TerminalMode::parse(&text.to_uppercase()), Some(mode));
            assert_eq!(mode.as_str(), text);
            assert_eq!(TerminalMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(TerminalMode::parse("warp"), None);
        assert!(TerminalMode::Unified.uses_term_view());
        assert!(TerminalMode::Block.uses_term_view());
        assert!(!TerminalMode::Vte.uses_term_view());
    }

    #[test]
    fn portable_font_default_matches_the_example_config() {
        assert_eq!(Config::safe_defaults().font_desc, DEFAULT_FONT_DESC);

        let example = include_str!("../config.toml.example")
            .parse::<toml::Table>()
            .expect("example config must remain valid TOML");
        assert_eq!(
            example.get("font").and_then(toml::Value::as_str),
            Some(DEFAULT_FONT_DESC)
        );
    }

    #[test]
    fn non_finite_presentation_numbers_fall_back_before_clamping() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(finite_clamp_f64(Some(value), 0.95, 0.01, 1.0), 0.95);
        }
        assert_eq!(finite_clamp_f64(Some(-1.0), 0.95, 0.01, 1.0), 0.01);
        assert_eq!(finite_clamp_f64(Some(4.0), 0.95, 0.01, 1.0), 1.0);
        assert_eq!(finite_clamp_f64(Some(0.7), 0.95, 0.01, 1.0), 0.7);
    }

    #[test]
    fn invalid_text_override_does_not_erase_a_safe_file_setting() {
        let configured = Some("  Monospace 15  ".to_string());
        assert_eq!(
            resolve_setting_text(
                Some("spoof\u{202e}font".to_string()),
                configured.clone(),
                MAX_FONT_DESC_BYTES,
            )
            .as_deref(),
            Some("Monospace 15")
        );
        assert_eq!(
            resolve_setting_text(
                Some("  JetBrains Mono  ".to_string()),
                configured,
                MAX_FONT_DESC_BYTES,
            )
            .as_deref(),
            Some("JetBrains Mono")
        );
        assert!(resolve_setting_text(
            Some("x".repeat(MAX_FONT_DESC_BYTES + 1)),
            Some("bad\nfont".to_string()),
            MAX_FONT_DESC_BYTES,
        )
        .is_none());
    }

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
            // Likewise: deployment publishes a script and its path depends on
            // the cache directory. The deploy tests below opt in explicitly.
            deploy: jterm_core::jsh_remote::Deploy::Off,
            docker: false,
            deploy_artifact: None,
        }
    }

    #[test]
    fn a_host_can_name_the_jsh_it_deploys() {
        let mut h = host();
        h.deploy = jterm_core::jsh_remote::Deploy::Incognito;
        h.deploy_artifact = Some("/home/tester/jsh/target/release/jsh".into());

        let argv = build_deployed_argv(&h, std::path::Path::new("/c/jsh-remote.sh"));

        let artifact = argv
            .iter()
            .position(|a| a == "--artifact")
            .expect("--artifact");
        assert_eq!(argv[artifact + 1], "/home/tester/jsh/target/release/jsh");
    }

    #[test]
    fn an_artifact_that_could_be_read_as_an_option_or_a_relative_path_rejects_the_host() {
        // Silently ignoring it would look exactly like deployment working,
        // right up to the moment the tab is a bash prompt.
        for artifact in ["target/release/jsh", "-artifact", ""] {
            let mut table = toml::Table::new();
            let mut entry = toml::Table::new();
            entry.insert("host".into(), toml::Value::String("h".into()));
            entry.insert("deploy".into(), toml::Value::String("persist".into()));
            entry.insert(
                "deploy_artifact".into(),
                toml::Value::String(artifact.to_string()),
            );
            table.insert(
                "remote_hosts".into(),
                toml::Value::Array(vec![toml::Value::Table(entry)]),
            );
            assert!(
                parse_remote_hosts(&table).is_empty(),
                "accepted deploy_artifact {artifact:?}"
            );
        }
    }

    #[test]
    fn a_container_tab_runs_docker_exec_rather_than_ssh() {
        let mut h = host();
        h.host = "devbox".into();
        h.user = Some("devuser".into());
        h.docker = true;
        // Inert for a container, and set here to prove they stay inert.
        h.ssh_args = vec!["-p".into(), "2222".into()];
        h.login_shell = true;

        let argv = build_remote_argv(&h);
        assert_eq!(
            argv,
            [
                "docker",
                "exec",
                "-it",
                "-u",
                "devuser",
                "devbox",
                "/home/tester/.local/bin/jsh",
                "--session",
                "staging-test",
            ]
        );
        assert!(!argv.iter().any(|a| a == "ssh"), "{argv:?}");
        // `user@host` would be read as a container name nobody has.
        assert!(!argv.iter().any(|a| a.contains('@')), "{argv:?}");
    }

    #[test]
    fn a_deployed_container_tab_names_the_container_and_its_user_separately() {
        let mut h = host();
        h.host = "devbox".into();
        h.user = Some("devuser".into());
        h.docker = true;
        h.deploy = jterm_core::jsh_remote::Deploy::Persist;

        let argv = build_deployed_argv(&h, std::path::Path::new("/c/jsh-remote.sh"));

        let container = argv.iter().position(|a| a == "--docker").expect("--docker");
        assert_eq!(argv[container + 1], "devbox");
        let user = argv
            .iter()
            .position(|a| a == "--docker-user")
            .expect("--docker-user");
        assert_eq!(argv[user + 1], "devuser");
        assert!(!argv.iter().any(|a| a.contains('@')), "{argv:?}");
        assert!(argv.contains(&"--persist".to_string()), "{argv:?}");
    }

    #[test]
    fn deploy_routes_through_the_remote_launcher_and_keeps_ssh_arguments() {
        let mut h = host();
        h.deploy = jterm_core::jsh_remote::Deploy::Incognito;
        h.ssh_args = vec!["-p".into(), "2222".into()];
        // A fixed path, not the published one: publishing writes into the real
        // cache directory, and on a machine where that fails this test would
        // silently assert the plain-ssh fallback instead.
        let argv = build_deployed_argv(&h, std::path::Path::new("/c/jsh-remote.sh"));

        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "/c/jsh-remote.sh");
        assert!(argv.contains(&"--incognito".to_string()), "{argv:?}");
        let expected_target = format!("{}@{}", h.user.as_deref().unwrap_or_default(), h.host);
        assert!(argv.contains(&expected_target), "{argv:?}");
        // The remote shell is chosen by the launcher, not by remote_shell: the
        // whole point is that the destination has no jsh to name.
        assert!(
            !argv.iter().any(|a| a.contains("/.local/bin/jsh")),
            "{argv:?}"
        );
        let separator = argv.iter().position(|a| a == "--").expect("ssh separator");
        assert_eq!(&argv[separator + 1..], ["-p", "2222"]);
    }

    #[test]
    fn deploy_off_is_byte_for_byte_the_old_ssh_command() {
        let h = host();
        assert!(!h.deploy.is_enabled());
        assert_eq!(build_remote_argv(&h), build_plain_ssh_argv(&h));
    }

    #[test]
    fn a_deploy_mode_this_build_cannot_parse_rejects_the_host() {
        // Not "falls back to off": a typo in `incognito` must never resolve to a
        // mode that writes into a shared account's home directory.
        let table: toml::Table = toml::from_str(
            "[[remote_hosts]]\nname = \"h\"\nhost = \"example.test\"\ndeploy = \"incognito!\"\n",
        )
        .expect("toml");
        assert!(parse_remote_hosts(&table).is_empty());

        let ok: toml::Table = toml::from_str(
            "[[remote_hosts]]\nname = \"h\"\nhost = \"example.test\"\ndeploy = \"incognito\"\n",
        )
        .expect("toml");
        let hosts = parse_remote_hosts(&ok);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].deploy, jterm_core::jsh_remote::Deploy::Incognito);
    }

    #[test]
    fn deploy_survives_a_config_round_trip_and_is_absent_when_off() {
        let mut h = host();
        h.deploy = jterm_core::jsh_remote::Deploy::Persist;
        let value = remote_host_to_toml(&h);
        assert_eq!(
            value.get("deploy").and_then(toml::Value::as_str),
            Some("persist")
        );

        h.deploy = jterm_core::jsh_remote::Deploy::Off;
        assert!(remote_host_to_toml(&h).get("deploy").is_none());
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
        // The wrapper sources the user's rc, so it must be the system's
        // interactive bash whenever one exists — never whichever bash the
        // inherited PATH names first.
        if is_executable_file(std::path::Path::new("/usr/bin/bash")) {
            assert_eq!(argv[0], "/usr/bin/bash");
        }
        assert_eq!(argv[1], "-ic");
        assert_eq!(
            &argv[2..],
            ["exec \"$0\" \"$@\"", "/home/tester/.local/bin/jsh"]
        );
    }

    #[test]
    fn saved_session_is_applied_to_direct_and_wrapped_jsh_argv() {
        let direct = vec!["/usr/bin/jsh".to_string()];
        let (direct, applied) = shell_argv_with_session(&direct, Some("session-one"));
        assert!(applied);
        assert_eq!(&direct[1..], ["--session", "session-one"]);

        let wrapped = vec![
            "/usr/bin/bash".to_string(),
            "-ic".to_string(),
            "exec \"$0\" \"$@\"".to_string(),
            "/usr/bin/jsh".to_string(),
        ];
        let (wrapped, applied) = shell_argv_with_session(&wrapped, Some("session_two"));
        assert!(applied);
        assert_eq!(&wrapped[4..], ["--session", "session_two"]);

        for invalid in ["bad session", "bad.session", "bad\nsession", "雪"] {
            let (_, applied) = shell_argv_with_session(&wrapped, Some(invalid));
            assert!(!applied, "{invalid:?}");
        }
    }

    #[test]
    fn session_id_grammar_is_shared_with_jsh_and_persisted_panes() {
        assert!(valid_session_id(&"s".repeat(MAX_SESSION_ID_BYTES)));
        assert!(!valid_session_id(&"s".repeat(MAX_SESSION_ID_BYTES + 1)));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("session\nspoof"));
        assert!(!valid_session_id("session with spaces"));
        assert!(!valid_session_id("session.with.dots"));
        assert!(!valid_session_id("雪"));
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
    fn missing_remote_hosts_section_falls_back_to_the_worked_examples() {
        let table = "font = 'monospace 12'".parse::<toml::Table>().unwrap();
        let remote_hosts = if table.contains_key("remote_hosts") {
            parse_remote_hosts(&table)
        } else {
            default_remote_hosts()
        };
        let names: Vec<&str> = remote_hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["dev-60", "myubuntu"]);
    }

    /// The defaults are what a user copies, so they have to be spelled the way
    /// the parser accepts: the port as an `ssh_args` flag and the login in
    /// `user`, never folded into `host` as `root@10.68.18.60:22`.
    #[test]
    fn default_remote_hosts_survive_their_own_round_trip() {
        let mut array = toml::value::Array::new();
        for host in default_remote_hosts() {
            array.push(remote_host_to_toml(&host));
        }
        let mut table = toml::Table::new();
        table.insert("remote_hosts".into(), toml::Value::Array(array));

        let reparsed = parse_remote_hosts(&table);
        assert_eq!(
            reparsed.len(),
            2,
            "an example the parser drops teaches the wrong shape"
        );
        assert_eq!(reparsed, default_remote_hosts());

        let ssh = &reparsed[0];
        assert_eq!(ssh.host, "10.68.18.60");
        assert_eq!(ssh.user.as_deref(), Some("root"));
        assert_eq!(ssh.ssh_args, ["-p", "22"]);
        assert!(!ssh.docker);
        assert!(reparsed[1].docker);
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
    fn history_paths_accept_absolute_or_home_and_expand_consistently() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_configured_path_with("/tmp/history.jsonl", Some(home)),
            Some("/tmp/history.jsonl".to_string())
        );
        assert_eq!(
            expand_configured_path_with("~/.local/state/anvil/history.jsonl", Some(home)),
            Some("/home/tester/.local/state/anvil/history.jsonl".to_string())
        );
        assert!(expand_configured_path_with("~/history", None).is_none());
        for path in [
            "Cargo.toml",
            "",
            "/tmp/bad\nname",
            "/tmp/visual\u{202e}spoof",
            "~//etc/passwd",
        ] {
            assert!(
                expand_configured_path_with(path, Some(home)).is_none(),
                "accepted {path:?}"
            );
        }
        let oversized = format!("/tmp/{}", "x".repeat(MAX_CONFIG_PATH_BYTES));
        assert!(expand_configured_path_with(&oversized, Some(home)).is_none());
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
            "name = 'option-shaped-host'\n",
            "host = '-oProxyCommand=bad'\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'embedded-user-target'\n",
            "host = 'user.example'\n",
            "user = 'root@other.example'\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'visually-deceptive-host'\n",
            "host = 'safe.example\u{202e}hidden'\n",
            "\n",
            "[[remote_hosts]]\n",
            "name = 'visually-deceptive-argument'\n",
            "host = 'argument-spoof.example'\n",
            "ssh_args = ['-o', 'ProxyJump=safe\u{200b}hidden']\n",
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
    fn remote_host_collection_is_bounded() {
        let entries = (0..MAX_REMOTE_HOSTS + 5)
            .map(|index| {
                let mut entry = toml::Table::new();
                entry.insert(
                    "host".to_string(),
                    toml::Value::String(format!("host-{index}.example")),
                );
                toml::Value::Table(entry)
            })
            .collect();
        let mut table = toml::Table::new();
        table.insert("remote_hosts".to_string(), toml::Value::Array(entries));

        assert_eq!(parse_remote_hosts(&table).len(), MAX_REMOTE_HOSTS);
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
    fn ai_base_url_matches_the_provider_transport_contract() {
        assert!(ai_base_url_is_safe(
            "openai-compatible",
            "https://api.example.com/v1"
        ));
        assert!(!ai_base_url_is_safe(
            "openai-compatible",
            "http://127.0.0.1:8000/v1"
        ));
        for url in [
            "http://localhost:11434",
            "http://127.0.0.1:11434",
            "http://127.12.3.4:11434",
            "http://[::1]:11434",
        ] {
            assert!(ai_base_url_is_safe("ollama", url), "rejected {url:?}");
        }
        assert!(!ai_base_url_is_safe(
            "ollama",
            "http://models.example.com:11434"
        ));
        for value in [
            "https://user:secret@example.com/v1",
            "https://example.com/v1?key=secret",
            "https://example.com/v1#fragment",
            "https://example.com\\@attacker.invalid/v1",
            "https:///missing-authority",
            "https://example.com/\u{fe0f}",
        ] {
            assert!(
                !ai_base_url_is_safe("openai-compatible", value),
                "accepted {value:?}"
            );
        }
        let oversized = format!("https://example.com/{}", "x".repeat(MAX_AI_BASE_URL_BYTES));
        assert!(!ai_base_url_is_safe("openai-compatible", &oversized));
    }

    #[test]
    fn explicit_invalid_ai_endpoint_never_drifts_to_a_public_default() {
        let insecure_loopback = "http://127.0.0.1:8000/v1";
        assert_eq!(
            resolve_ai_base_url(
                Some(insecure_loopback.to_string()),
                "https://api.openai.com/v1"
            ),
            insecure_loopback
        );
        assert_eq!(
            resolve_ai_base_url(
                Some("https://user:secret@example.com/v1".to_string()),
                "https://api.openai.com/v1"
            ),
            ""
        );
        assert_eq!(
            resolve_ai_base_url(None, "https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
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
        // An ssh host must not grow the key it never asked for.
        assert!(!parsed[0].docker);
        assert!(remote_host_to_toml(&original)
            .as_table()
            .is_some_and(|t| !t.contains_key("docker")));

        let mut container = host();
        container.docker = true;
        container.deploy_artifact = Some("/opt/jsh".into());
        let mut table = toml::Table::new();
        table.insert(
            "remote_hosts".into(),
            toml::Value::Array(vec![remote_host_to_toml(&container)]),
        );
        let restored = parse_remote_hosts(&table);
        assert!(restored[0].docker);
        assert_eq!(restored[0].deploy_artifact.as_deref(), Some("/opt/jsh"));
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
        assert!(!config.command_correction_enabled);
        assert_eq!(config.jsh_update_check, JshUpdateCheck::Never);
        assert_eq!(config.ai_provider, "anthropic");
        assert_eq!(config.ai_model, "claude-sonnet-4-6");
        assert_eq!(config.ai_base_url, "https://api.anthropic.com");
        assert_eq!(config.ai_max_tokens, 1_024);
        assert!(config.ai_redact_secrets);
        assert!(!config.notify_long_blocks);
        assert!(!config.allow_remote_clipboard_write);
        assert!(config.bottom_bar);
        assert!(config.remote_hosts.is_empty());
    }

    #[test]
    fn forge_compatible_panel_and_organism_defaults_are_safe() {
        let config = Config::safe_defaults();
        assert!(!config.ai_panel_visible);
        assert_eq!(config.ai_panel_width, 360);
        assert!(!config.ascii_organism_enabled);
        assert_eq!(config.ascii_organism_motion, None);
        assert_eq!(OrganismMotion::parse("FULL"), Some(OrganismMotion::Full));
        assert_eq!(OrganismMotion::parse("calm"), Some(OrganismMotion::Calm));
        assert_eq!(
            OrganismMotion::parse("static"),
            Some(OrganismMotion::Static)
        );
        assert_eq!(OrganismMotion::parse("sleepy"), None);
        assert!(default_ascii_organism_memory_path().ends_with("anvil/ascii-organism-native.json"));
    }

    #[test]
    fn execution_gate_rejects_spoofing_unstructured_ssh_args_and_high_indexes() {
        let valid = host();
        assert!(checked_remote_argv(&valid).is_ok());

        let mut spoofed = valid.clone();
        spoofed.name = "trusted\u{202e}host".into();
        assert!(checked_remote_argv(&spoofed).is_err());

        let mut second_destination = valid.clone();
        second_destination.ssh_args = vec!["attacker.example".into()];
        assert!(checked_remote_argv(&second_destination).is_err());

        let mut premature_separator = valid.clone();
        premature_separator.ssh_args = vec!["--".into()];
        assert!(checked_remote_argv(&premature_separator).is_err());

        let hosts = vec![valid; MAX_REMOTE_HOSTS + 1];
        assert!(checked_remote_host(&hosts, MAX_REMOTE_HOSTS - 1).is_ok());
        assert!(checked_remote_host(&hosts, MAX_REMOTE_HOSTS).is_err());
    }

    #[test]
    fn temporary_ssh_terminal_is_plain_interactive_without_a_remote_command() {
        let mut temporary = host();
        temporary.host = "dsw-notebook.example.com".into();
        temporary.user = Some("root".into());
        temporary.ssh_args = vec![
            "-p".into(),
            "22".into(),
            "-S".into(),
            "/run/user/1000/live-cm-%C".into(),
        ];
        temporary.remote_shell = "jsh --should-not-run".into();
        temporary.deploy = jterm_core::jsh_remote::Deploy::Off;
        temporary.deploy_artifact = None;
        temporary.session = None;
        temporary.multiplex = false;

        assert_eq!(
            checked_interactive_ssh_argv(&temporary).expect("validated temporary login"),
            [
                "ssh",
                "-t",
                "-p",
                "22",
                "-S",
                "/run/user/1000/live-cm-%C",
                "--",
                "root@dsw-notebook.example.com"
            ]
        );
    }

    #[test]
    fn immutable_remote_profile_resolves_only_one_complete_valid_match() {
        let expected = host();
        let mut other = expected.clone();
        other.name = "other".into();
        other.host = "198.51.100.9".into();

        assert_eq!(
            unique_checked_remote_profile_index(&[other.clone(), expected.clone()], &expected),
            Some(1),
            "a pure reorder follows the complete configured profile"
        );

        let mut same_name_replacement = expected.clone();
        same_name_replacement.host = "198.51.100.10".into();
        assert_eq!(
            unique_checked_remote_profile_index(&[same_name_replacement], &expected),
            None,
            "a display-name match is not profile identity"
        );
        let mut session_edited = expected.clone();
        session_edited.session = Some("runtime-session".into());
        assert_eq!(
            unique_checked_remote_profile_index(&[session_edited], &expected),
            None,
            "a runtime session that happens to match edited config cannot rewrite identity"
        );
        assert_eq!(
            unique_checked_remote_profile_index(&[expected.clone(), expected.clone()], &expected),
            None,
            "an ambiguous exact identity fails closed"
        );

        let mut invalid = expected.clone();
        invalid.host = "-option-like-target".into();
        assert_eq!(
            unique_checked_remote_profile_index(&[invalid.clone()], &invalid),
            None,
            "an invalid profile cannot authorize itself"
        );

        let mut beyond_limit = vec![other; MAX_REMOTE_HOSTS];
        beyond_limit.push(expected.clone());
        assert_eq!(
            unique_checked_remote_profile_index(&beyond_limit, &expected),
            None,
            "profile 129 is outside the execution authority"
        );
    }
}
