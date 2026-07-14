#!/usr/bin/env python3
"""One-shot, deterministic source migration for the resilience evolution branch."""

from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, content: str, executable: bool = False) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")
    if executable:
        target.chmod(0o755)


def replace_once(path: str, old: str, new: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}: {old[:80]!r}")
    write(path, content.replace(old, new, 1))


def insert_before_last_brace(path: str, insertion: str) -> None:
    content = read(path)
    index = content.rfind("\n}")
    if index < 0:
        raise RuntimeError(f"{path}: final brace not found")
    write(path, content[:index] + insertion + content[index:])


# CLI: safe-mode launch contract and machine-readable doctor output.
replace_once(
    "src/cli.rs",
    """#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Block,
    Vte,
}
""",
    """#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Block,
    Vte,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DoctorFormat {
    Human,
    Json,
}
""",
)
replace_once(
    "src/cli.rs",
    """    pub(crate) no_restore: bool,
    pub(crate) mode: Option<Mode>,
""",
    """    pub(crate) no_restore: bool,
    pub(crate) safe_mode: bool,
    pub(crate) mode: Option<Mode>,
""",
)
replace_once(
    "src/cli.rs",
    """    Doctor,
""",
    """    Doctor(DoctorFormat),
""",
)
replace_once(
    "src/cli.rs",
    """    if args.first().is_some_and(|arg| arg == "--doctor") {
        require_exact_args(&args, 1, "--doctor")?;
        return Ok(Command::Doctor);
    }
""",
    """    if args.first().is_some_and(|arg| arg == "--doctor") {
        let format = match args.as_slice() {
            [_] => DoctorFormat::Human,
            [_, flag] if flag == "--json" => DoctorFormat::Json,
            _ => return Err("usage: jterm1 --doctor [--json]".to_string()),
        };
        return Ok(Command::Doctor(format));
    }
""",
)
replace_once(
    "src/cli.rs",
    """            Some("--no-restore") => launch.no_restore = true,
""",
    """            Some("--no-restore") => launch.no_restore = true,
            Some("--safe-mode") => {
                launch.safe_mode = true;
                launch.no_restore = true;
            }
""",
)
replace_once(
    "src/cli.rs",
    """      --no-restore             Start a fresh workspace
""",
    """      --no-restore             Start a fresh workspace
      --safe-mode             Use isolated VTE defaults without restore or persistence
""",
)
replace_once(
    "src/cli.rs",
    """      --doctor                 Check configuration and runtime dependencies
""",
    """      --doctor [--json]        Check configuration and runtime dependencies
""",
)
replace_once(
    "src/cli.rs",
    """  jterm1 --mode block --no-restore
""",
    """  jterm1 --mode block --no-restore
  jterm1 --safe-mode
  jterm1 --doctor --json
""",
)
replace_once(
    "src/cli.rs",
    """                no_restore: false,
                mode: Some(Mode::Block),
""",
    """                no_restore: false,
                safe_mode: false,
                mode: Some(Mode::Block),
""",
)
insert_before_last_brace(
    "src/cli.rs",
    """

    #[test]
    fn doctor_supports_human_and_json_formats() {
        assert_eq!(
            parse_strs(&["--doctor"]).unwrap(),
            Command::Doctor(DoctorFormat::Human)
        );
        assert_eq!(
            parse_strs(&["--doctor", "--json"]).unwrap(),
            Command::Doctor(DoctorFormat::Json)
        );
        assert!(parse_strs(&["--doctor", "--verbose"]).is_err());
    }

    #[test]
    fn safe_mode_implies_a_fresh_workspace() {
        let Command::Run(options) = parse_strs(&["--safe-mode"]).unwrap() else {
            panic!("expected run")
        };
        assert!(options.safe_mode);
        assert!(options.no_restore);
        assert!(options.execute.is_none());
    }
""",
)

# Config safe-mode policy.
replace_once(
    "src/config.rs",
    """}

// ---------------------------------------------------------------------------
// Theme
""",
    """}

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
""",
)
insert_before_last_brace(
    "src/config.rs",
    """

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
""",
)

# Application wiring.
replace_once("src/main.rs", "mod config;\n", "mod config;\nmod diagnostics;\n")
replace_once(
    "src/main.rs",
    """    session_persistence: bool,
    dyn_css: gtk::CssProvider,
""",
    """    session_persistence: bool,
    safe_mode: bool,
    dyn_css: gtk::CssProvider,
""",
)
replace_once(
    "src/main.rs",
    """        let config_warning = config::config_file_error();
        let (mut config, themes, kbmap) = load_config();
        if let Some(mode) = init.mode {
""",
    """        let config_warning = if init.safe_mode {
            None
        } else {
            config::config_file_error()
        };
        let (mut config, themes, kbmap) = load_config();
        if init.safe_mode {
            config.apply_safe_mode();
        }
        if let Some(mode) = init.mode {
""",
)
replace_once(
    "src/main.rs",
    """        let shell_argv = Rc::new(choose_shell_argv(config.shell.as_deref()));
""",
    """        let shell_argv = if init.safe_mode {
            Rc::new(vec!["sh".to_string()])
        } else {
            Rc::new(choose_shell_argv(config.shell.as_deref()))
        };
""",
)
replace_once(
    "src/main.rs",
    """        let restore_session =
            !init.no_restore && init.working_directory.is_none() && init.execute.is_none();
        let session_persistence = init.execute.is_none();
""",
    """        let restore_session = !init.safe_mode
            && !init.no_restore
            && init.working_directory.is_none()
            && init.execute.is_none();
        let session_persistence = init.execute.is_none() && !init.safe_mode;
""",
)
replace_once(
    "src/main.rs",
    """            session_persistence,
            dyn_css,
""",
    """            session_persistence,
            safe_mode: init.safe_mode,
            dyn_css,
""",
)
replace_once(
    "src/main.rs",
    """        if let Some(error) = config_warning {
            model.show_toast(format!(
                "Config could not be loaded; defaults are active. Your file was left untouched. {error}"
            ));
        }

        // Place the tab strip (sidebar vs top bar) and select the sidebar view.
""",
    """        if let Some(error) = config_warning {
            model.show_toast(format!(
                "Config could not be loaded; defaults are active. Your file was left untouched. {error}"
            ));
        }
        if init.safe_mode {
            model.show_toast(
                "Safe mode: VTE + sh, with startup commands, restore, persistence, remote hosts, and AI disabled.",
            );
        }

        // Place the tab strip (sidebar vs top bar) and select the sidebar view.
""",
)
monitor_old = """        // Config file hot reload: watch config.toml for external changes.
        let config_path = config_file_path();
        if let Some(parent) = config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let config_file = gio::File::for_path(&config_path);
        if let Ok(monitor) =
            config_file.monitor_file(gio::FileMonitorFlags::NONE, None::<&Cancellable>)
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
"""
monitor_new = """        // Config file hot reload is intentionally disabled in safe mode: a
        // change on disk must not re-enable startup, persistence, remote, or AI
        // behavior in the isolated recovery session.
        if !init.safe_mode {
            let config_path = config_file_path();
            if let Some(parent) = config_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let config_file = gio::File::for_path(&config_path);
            if let Ok(monitor) =
                config_file.monitor_file(gio::FileMonitorFlags::NONE, None::<&Cancellable>)
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
"""
replace_once("src/main.rs", monitor_old, monitor_new)
replace_once(
    "src/main.rs",
    """        cli::Command::Doctor => {
            init_logging();
            if !run_doctor() {
""",
    """        cli::Command::Doctor(format) => {
            init_logging();
            if !run_doctor(format) {
""",
)
main = read("src/main.rs")
start = main.find("fn run_doctor() -> bool {")
end_marker = "\n/// Make the fcitx5 GTK4 input-method module discoverable"
end = main.find(end_marker, start)
if start < 0 or end < 0:
    raise RuntimeError("src/main.rs: doctor function markers not found")
main = (
    main[:start]
    + "fn run_doctor(format: cli::DoctorFormat) -> bool {\n    diagnostics::run(format)\n}\n"
    + main[end:]
)
write("src/main.rs", main)

# Safe mode remains isolated even when UI actions are invoked.
replace_once(
    "src/config_ops.rs",
    """    pub(crate) fn reload_config(&mut self, _sender: &ComponentSender<AppModel>) {
        if let Some(error) = config::config_file_error() {
""",
    """    pub(crate) fn reload_config(&mut self, _sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("Configuration reload is disabled in safe mode.");
            return;
        }
        if let Some(error) = config::config_file_error() {
""",
)
replace_once(
    "src/workspace_ops.rs",
    """    pub(crate) fn persist_config(&self) {
        if let Err(err) = config::save_config(&self.config.borrow()) {
""",
    """    pub(crate) fn persist_config(&self) {
        if self.safe_mode {
            self.show_toast("Settings are temporary and are not saved in safe mode.");
            return;
        }
        if let Err(err) = config::save_config(&self.config.borrow()) {
""",
)
replace_once(
    "src/ai_palette_ops.rs",
    """    pub(crate) fn handle_palette_ask_ai(&self, query: String, sender: &ComponentSender<AppModel>) {
        if !self.config.borrow().ai_enabled {
""",
    """    pub(crate) fn handle_palette_ask_ai(&self, query: String, sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("AI is unavailable in safe mode.");
            return;
        }
        if !self.config.borrow().ai_enabled {
""",
)
replace_once(
    "src/ai_palette_ops.rs",
    """    pub(crate) fn show_ai_session_panel(&self) {
        if !self.config.borrow().ai_enabled {
""",
    """    pub(crate) fn show_ai_session_panel(&self) {
        if self.safe_mode {
            self.show_toast("AI is unavailable in safe mode.");
            return;
        }
        if !self.config.borrow().ai_enabled {
""",
)
replace_once(
    "src/agent_ops.rs",
    """    pub(crate) fn open_agent_panel(&self, _sender: &ComponentSender<AppModel>) {
        let cfg = self.config.borrow();
""",
    """    pub(crate) fn open_agent_panel(&self, _sender: &ComponentSender<AppModel>) {
        if self.safe_mode {
            self.show_toast("AI Agent is unavailable in safe mode.");
            return;
        }
        let cfg = self.config.borrow();
""",
)

for signature, message in [
    ("apply_settings_terminal_mode(&mut self, mode: usize)", "Terminal mode is fixed to VTE in safe mode."),
    ("apply_settings_command_history(&mut self, enabled: bool)", "Command history is disabled in safe mode."),
    ("apply_settings_ai_enabled(&mut self, enabled: bool)", "AI is disabled in safe mode."),
    ("apply_settings_agent_enabled(&mut self, enabled: bool)", "AI Agent is disabled in safe mode."),
    ("apply_settings_notifications(&mut self, enabled: bool)", "Notifications are disabled in safe mode."),
    ("apply_settings_remote_clipboard(&mut self, enabled: bool)", "Remote clipboard writes are disabled in safe mode."),
]:
    old = f"    pub(crate) fn {signature} {{\n"
    new = old + f"        if self.safe_mode {{\n            self.show_toast(\"{message}\");\n            return;\n        }}\n"
    replace_once("src/settings_ops.rs", old, new)

# Structured diagnostics module.
write(
    "src/diagnostics.rs",
    r'''//! Human- and machine-readable runtime diagnostics.
//!
//! The report deliberately excludes configuration contents, terminal history,
//! command output, environment values, and credentials. It is safe to attach to
//! a support request after reviewing the generated text or JSON.

use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::cli::DoctorFormat;
use crate::config::{self, choose_shell_argv, config_file_path, load_config, TerminalMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Serialize)]
struct DiagnosticCheck {
    name: &'static str,
    status: CheckStatus,
    detail: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DiagnosticReport {
    version: &'static str,
    checks: Vec<DiagnosticCheck>,
    errors: usize,
    warnings: usize,
}

impl DiagnosticReport {
    fn new() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            checks: Vec::new(),
            errors: 0,
            warnings: 0,
        }
    }

    fn push(&mut self, name: &'static str, status: CheckStatus, detail: impl Into<String>) {
        match status {
            CheckStatus::Ok => {}
            CheckStatus::Warning => self.warnings += 1,
            CheckStatus::Error => self.errors += 1,
        }
        self.checks.push(DiagnosticCheck {
            name,
            status,
            detail: detail.into(),
        });
    }

    fn healthy(&self) -> bool {
        self.errors == 0
    }
}

fn executable_exists(executable: &str) -> bool {
    let path = Path::new(executable);
    if path.components().count() > 1 {
        path.is_file()
    } else {
        config::find_executable_in_path(executable).is_some()
    }
}

fn collect() -> DiagnosticReport {
    let mut report = DiagnosticReport::new();
    let config_path = config_file_path();

    match config::config_file_error() {
        Some(message) => report.push("config", CheckStatus::Error, message),
        None if config_path.is_file() => report.push(
            "config",
            CheckStatus::Ok,
            config_path.display().to_string(),
        ),
        None => report.push(
            "config",
            CheckStatus::Warning,
            format!(
                "{} does not exist (built-in defaults)",
                config_path.display()
            ),
        ),
    }

    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(&config_path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            report.push(
                "config permissions",
                CheckStatus::Ok,
                format!("{mode:04o}"),
            );
        } else {
            report.push(
                "config permissions",
                CheckStatus::Warning,
                format!("{mode:04o} (recommended: 0600)"),
            );
        }
    }

    let (config, _, _) = load_config();
    let shell_argv = choose_shell_argv(config.shell.as_deref());
    let shell = shell_argv.first().cloned().unwrap_or_default();
    if executable_exists(&shell) {
        report.push("shell", CheckStatus::Ok, shell_argv.join(" "));
    } else {
        report.push(
            "shell",
            CheckStatus::Error,
            format!("not executable: {shell}"),
        );
    }

    let display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|_| "Wayland display is available".to_string())
        .or_else(|| {
            std::env::var("DISPLAY")
                .ok()
                .filter(|value| !value.is_empty())
                .map(|_| "X11 display is available".to_string())
        });
    match display {
        Some(display) => report.push("display", CheckStatus::Ok, display),
        None => report.push(
            "display",
            CheckStatus::Warning,
            "DISPLAY and WAYLAND_DISPLAY are unset",
        ),
    }

    match &config.command_history_path {
        Some(path) => report.push("command history", CheckStatus::Ok, path.clone()),
        None => report.push("command history", CheckStatus::Warning, "disabled"),
    }

    let workflow_count = crate::workflows::load_all(&crate::workflows::workflow_dirs()).len();
    report.push(
        "workflows",
        CheckStatus::Ok,
        format!("{workflow_count} available"),
    );

    if !config.ai_enabled {
        report.push("AI", CheckStatus::Warning, "disabled by configuration");
    } else if let Some(client) = crate::ai::AiClient::from_env() {
        report.push("AI", CheckStatus::Ok, client.display_name());
    } else {
        report.push(
            "AI",
            CheckStatus::Warning,
            "provider configuration is incomplete",
        );
    }

    if config::find_executable_in_path("notify-send").is_some() {
        report.push(
            "notifications",
            CheckStatus::Ok,
            "notify-send available",
        );
    } else {
        report.push(
            "notifications",
            CheckStatus::Warning,
            "notify-send missing (long-command alerts disabled)",
        );
    }

    if config.remote_hosts.is_empty() {
        report.push("remote hosts", CheckStatus::Ok, "none configured");
    } else if config::find_executable_in_path("ssh").is_some() {
        report.push(
            "remote hosts",
            CheckStatus::Ok,
            format!("{} configured; ssh available", config.remote_hosts.len()),
        );
    } else {
        report.push(
            "remote hosts",
            CheckStatus::Error,
            format!("{} configured; ssh is missing", config.remote_hosts.len()),
        );
    }

    report.push(
        "terminal mode",
        CheckStatus::Ok,
        match config.terminal_mode {
            TerminalMode::Block => "block",
            TerminalMode::Vte => "vte",
        },
    );

    let session_dir = crate::session::state_file_path()
        .parent()
        .map(Path::to_path_buf);
    if let Some(session_dir) = session_dir {
        let snapshots = fs::read_dir(&session_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("tabs.") && name.contains(".state"))
            })
            .count();
        report.push(
            "session state",
            CheckStatus::Ok,
            format!("{} ({snapshots} snapshot(s))", session_dir.display()),
        );
    }

    report
}

fn print_human(report: &DiagnosticReport) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "jterm1 {} diagnostics\n", report.version)?;
    for check in &report.checks {
        let status = match check.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warning => "warn",
            CheckStatus::Error => "error",
        };
        writeln!(stdout, "[{status:<5}] {}: {}", check.name, check.detail)?;
    }
    writeln!(
        stdout,
        "\nSummary: {} error(s), {} warning(s)",
        report.errors, report.warnings
    )
}

fn print_json(report: &DiagnosticReport) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, report).map_err(io::Error::other)?;
    writeln!(stdout)
}

pub(crate) fn run(format: DoctorFormat) -> bool {
    let report = collect();
    let printed = match format {
        DoctorFormat::Human => print_human(&report),
        DoctorFormat::Json => print_json(&report),
    };
    if let Err(error) = printed {
        eprintln!("jterm1: failed to write diagnostics: {error}");
        return false;
    }
    report.healthy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_counts_warnings_and_errors() {
        let mut report = DiagnosticReport::new();
        report.push("one", CheckStatus::Ok, "ok");
        report.push("two", CheckStatus::Warning, "warn");
        report.push("three", CheckStatus::Error, "error");
        assert_eq!(report.warnings, 1);
        assert_eq!(report.errors, 1);
        assert!(!report.healthy());
    }

    #[test]
    fn report_serializes_without_sensitive_fields() {
        let mut report = DiagnosticReport::new();
        report.push("shell", CheckStatus::Ok, "/bin/sh");
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"errors\":0"));
        assert!(!json.contains("API_KEY"));
    }
}
''',
)

# Privacy-preserving support bundle.
write(
    "scripts/support-bundle.sh",
    r'''#!/usr/bin/env bash
# Create a privacy-preserving jterm1 support archive.

set -euo pipefail
umask 077

usage() {
    echo "Usage: $0 [OUTPUT_DIRECTORY]" >&2
}

if (( $# > 1 )); then
    usage
    exit 2
fi

OUTPUT_DIR="${1:-.}"
JTERM1_BIN="${JTERM1_BIN:-jterm1}"
if ! command -v "${JTERM1_BIN}" >/dev/null 2>&1; then
    echo "Error: jterm1 executable not found: ${JTERM1_BIN}" >&2
    exit 1
fi

mkdir -p "${OUTPUT_DIR}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BUNDLE_NAME="jterm1-support-${STAMP}"
WORK_DIR="$(mktemp -d)"
BUNDLE_DIR="${WORK_DIR}/${BUNDLE_NAME}"
trap 'rm -rf -- "${WORK_DIR}"' EXIT
mkdir -p "${BUNDLE_DIR}"

human_status=0
json_status=0
"${JTERM1_BIN}" --doctor >"${BUNDLE_DIR}/doctor.txt" 2>&1 || human_status=$?
"${JTERM1_BIN}" --doctor --json >"${BUNDLE_DIR}/doctor.json" 2>"${BUNDLE_DIR}/doctor-json.stderr" || json_status=$?

binary_path="$(command -v "${JTERM1_BIN}")"
version="$(${JTERM1_BIN} --version 2>&1 || true)"
config_home="${XDG_CONFIG_HOME:-${HOME}/.config}"
data_home="${XDG_DATA_HOME:-${HOME}/.local/share}"
state_home="${XDG_STATE_HOME:-${HOME}/.local/state}"

{
    printf 'generated_at_utc=%s\n' "${STAMP}"
    printf 'version=%s\n' "${version}"
    printf 'binary=%s\n' "${binary_path}"
    printf 'doctor_exit=%s\n' "${human_status}"
    printf 'doctor_json_exit=%s\n' "${json_status}"
    printf 'uname=%s\n' "$(uname -a 2>/dev/null || true)"
    printf 'architecture=%s\n' "$(uname -m 2>/dev/null || true)"
    printf 'session_type=%s\n' "${XDG_SESSION_TYPE:-unset}"
    printf 'wayland_display_present=%s\n' "$([[ -n "${WAYLAND_DISPLAY:-}" ]] && echo yes || echo no)"
    printf 'x11_display_present=%s\n' "$([[ -n "${DISPLAY:-}" ]] && echo yes || echo no)"
} >"${BUNDLE_DIR}/system.txt"

{
    printf 'config=%s/jterm1/config.toml\n' "${config_home}"
    printf 'data=%s/jterm1\n' "${data_home}"
    printf 'state=%s/jterm1\n' "${state_home}"
    for path in \
        "${config_home}/jterm1/config.toml" \
        "${config_home}/jterm1/config.toml.bak" \
        "${state_home}/jterm1/history.jsonl"; do
        if [[ -e "${path}" ]]; then
            stat --printf='%A %a %s bytes %n\n' "${path}" 2>/dev/null || ls -ld -- "${path}"
        else
            printf 'missing %s\n' "${path}"
        fi
    done
} >"${BUNDLE_DIR}/paths-and-metadata.txt"

{
    for name in ANTHROPIC_API_KEY OPENAI_API_KEY JTERM1_AI_PROVIDER JTERM1_AI_MODEL JTERM1_AI_BASE_URL; do
        if [[ -n "${!name:-}" ]]; then
            printf '%s=present\n' "${name}"
        else
            printf '%s=absent\n' "${name}"
        fi
    done
} >"${BUNDLE_DIR}/environment-presence.txt"

if command -v ldd >/dev/null 2>&1; then
    ldd "${binary_path}" >"${BUNDLE_DIR}/linked-libraries.txt" 2>&1 || true
fi
if command -v locale >/dev/null 2>&1; then
    locale >"${BUNDLE_DIR}/locale.txt" 2>&1 || true
fi

cat >"${BUNDLE_DIR}/README.txt" <<'EOF_README'
This support bundle intentionally excludes configuration contents, terminal
history, command output, clipboard data, environment values, API keys, SSH host
details, and session snapshots. It contains diagnostics, file metadata, system
identity, and the presence/absence of selected integration variables only.
Review every file before sharing the archive.
EOF_README

ARCHIVE_PATH="${OUTPUT_DIR}/${BUNDLE_NAME}.tar.gz"
tar --sort=name --owner=0 --group=0 --numeric-owner -C "${WORK_DIR}" -cf - "${BUNDLE_NAME}" \
    | gzip -n -9 >"${ARCHIVE_PATH}"
printf 'Created %s\n' "${ARCHIVE_PATH}"
''',
    executable=True,
)

write(
    "scripts/security-check.sh",
    r'''#!/usr/bin/env bash
# Reproducible dependency and shell-script security checks.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

cargo metadata --locked --format-version 1 --no-deps >/dev/null

if ! command -v cargo-audit >/dev/null 2>&1; then
    echo "Error: cargo-audit is required (install with 'cargo install cargo-audit --locked')." >&2
    exit 1
fi
cargo audit

# Duplicate crates are not automatically vulnerabilities, but surfacing them
# here makes dependency drift visible during reviews and scheduled audits.
cargo tree --locked --duplicates

if ! command -v shellcheck >/dev/null 2>&1; then
    echo "Error: shellcheck is required." >&2
    exit 1
fi
mapfile -t shell_files < <(find scripts packaging -type f -name '*.sh' -print | sort)
shellcheck "${shell_files[@]}"
''',
    executable=True,
)

write(
    "rust-toolchain.toml",
    '''[toolchain]
channel = "stable"
profile = "minimal"
components = ["rustfmt", "clippy"]
''',
)

write(
    ".github/workflows/security.yml",
    r'''name: Security

on:
  pull_request:
  push:
    branches:
      - master
  schedule:
    - cron: "23 4 * * 1"
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: security-${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

jobs:
  dependency-and-shell-audit:
    name: Dependency and shell audit
    runs-on: ubuntu-24.04
    timeout-minutes: 30

    steps:
      - name: Check out repository
        uses: actions/checkout@v5

      - name: Install Rust toolchain
        run: |
          rustup show
          rustc --version
          cargo --version

      - name: Cache Cargo data
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/bin
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-security-${{ hashFiles('Cargo.lock', 'rust-toolchain.toml') }}
          restore-keys: |
            ${{ runner.os }}-security-

      - name: Install audit tooling
        run: |
          sudo apt-get update
          sudo apt-get install --no-install-recommends --yes shellcheck
          if ! command -v cargo-audit >/dev/null 2>&1; then
            cargo install cargo-audit --locked
          fi

      - name: Audit dependencies and shell scripts
        run: bash scripts/security-check.sh
''',
)

# Developer entry points.
replace_once(
    "Makefile",
    ".PHONY: help build run test check fmt clippy verify package clean install dev watch benchmark debug",
    ".PHONY: help build run test check fmt clippy security verify package support-bundle clean install dev watch benchmark debug",
)
replace_once(
    "Makefile",
    "\t@echo \"  make clippy     - Run the repository lint policy\"\n\t@echo \"  make verify     - Run the complete local quality gate\"",
    "\t@echo \"  make clippy     - Run the repository lint policy\"\n\t@echo \"  make security   - Audit dependencies and shell scripts\"\n\t@echo \"  make verify     - Run the complete local quality gate\"",
)
replace_once(
    "Makefile",
    "\t@echo \"  make debug      - Show debug information\"",
    "\t@echo \"  make debug      - Show debug information\"\n\t@echo \"  make support-bundle - Create a privacy-preserving support archive\"",
)
replace_once(
    "Makefile",
    """clippy:
	@./scripts/dev.sh clippy

verify:
""",
    """clippy:
	@./scripts/dev.sh clippy

security:
	@./scripts/dev.sh security

verify:
""",
)
replace_once(
    "Makefile",
    """package:
	@./scripts/dev.sh package

clean:
""",
    """package:
	@./scripts/dev.sh package

support-bundle:
	@./scripts/support-bundle.sh

clean:
""",
)

replace_once(
    "scripts/dev.sh",
    "Usage: $0 {run|build|test|check|fmt|clippy|verify|package|clean|watch}",
    "Usage: $0 {run|build|test|check|fmt|clippy|security|verify|package|clean|watch}",
)
replace_once(
    "scripts/dev.sh",
    '    echo "  clippy   - Run the repository lint policy"\n    echo "  verify   - Run formatting, checks, tests, lints, and docs"',
    '    echo "  clippy   - Run the repository lint policy"\n    echo "  security - Audit dependencies and shell scripts"\n    echo "  verify   - Run formatting, checks, tests, lints, and docs"',
)
replace_once(
    "scripts/dev.sh",
    "run|build|test|check|fmt|clippy|verify|package|clean|watch",
    "run|build|test|check|fmt|clippy|security|verify|package|clean|watch",
)
replace_once(
    "scripts/dev.sh",
    """    clippy)
        echo "Running Clippy..."
        run_in_nix bash scripts/clippy.sh
        ;;

    verify)
""",
    """    clippy)
        echo "Running Clippy..."
        run_in_nix bash scripts/clippy.sh
        ;;

    security)
        echo "Running dependency and shell-script security checks..."
        run_in_nix bash scripts/security-check.sh
        ;;

    verify)
""",
)

# Nix dev environment and package include the new tools.
replace_once(
    "flake.nix",
    """              clippy
              cargo-watch
""",
    """              clippy
              cargo-audit
              cargo-watch
              shellcheck
""",
)
replace_once(
    "flake.nix",
    """              install -Dm644 README.md \\
                "$out/share/doc/jterm1/README.md"
""",
    """              install -Dm644 README.md \\
                "$out/share/doc/jterm1/README.md"
              install -Dm644 Cargo.lock \\
                "$out/share/doc/jterm1/Cargo.lock"
              install -Dm755 scripts/support-bundle.sh \\
                "$out/bin/jterm1-support-bundle"
""",
)

# Source and portable installers.
replace_once(
    "scripts/install.sh",
    """echo "Installing ${INSTALL_DIR}/jterm1..."
install -Dm755 "${BINARY}" "${INSTALL_DIR}/jterm1"
""",
    """echo "Installing ${INSTALL_DIR}/jterm1..."
install -Dm755 "${BINARY}" "${INSTALL_DIR}/jterm1"
install -Dm755 "${PROJECT_ROOT}/scripts/support-bundle.sh" \\
    "${INSTALL_DIR}/jterm1-support-bundle"
""",
)
replace_once(
    "scripts/install.sh",
    """echo "  Binary:            ${INSTALL_DIR}/jterm1"
""",
    """echo "  Binary:            ${INSTALL_DIR}/jterm1"
echo "  Support bundle:    ${INSTALL_DIR}/jterm1-support-bundle"
""",
)
replace_once(
    "scripts/package-release.sh",
    """install -Dm755 "${BINARY}" "${PACKAGE_ROOT}/bin/jterm1"
install -Dm755 packaging/install-release.sh "${PACKAGE_ROOT}/install.sh"
""",
    """install -Dm755 "${BINARY}" "${PACKAGE_ROOT}/bin/jterm1"
install -Dm755 scripts/support-bundle.sh "${PACKAGE_ROOT}/bin/jterm1-support-bundle"
install -Dm755 packaging/install-release.sh "${PACKAGE_ROOT}/install.sh"
""",
)
replace_once(
    "scripts/package-release.sh",
    """install -Dm644 config.toml.example \\
    "${PACKAGE_ROOT}/share/doc/jterm1/config.toml.example"
""",
    """install -Dm644 config.toml.example \\
    "${PACKAGE_ROOT}/share/doc/jterm1/config.toml.example"
install -Dm644 Cargo.lock "${PACKAGE_ROOT}/share/doc/jterm1/Cargo.lock"
cat >"${PACKAGE_ROOT}/share/doc/jterm1/BUILDINFO" <<EOF_BUILDINFO
version=${VERSION}
target=${TARGET}
source_date_epoch=${SOURCE_DATE_EPOCH}
git_commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)
rustc=$(rustc --version)
EOF_BUILDINFO
""",
)
replace_once(
    "packaging/install-release.sh",
    """install -Dm755 "${SCRIPT_DIR}/bin/jterm1" "${INSTALL_DIR}/jterm1"
""",
    """install -Dm755 "${SCRIPT_DIR}/bin/jterm1" "${INSTALL_DIR}/jterm1"
install -Dm755 "${SCRIPT_DIR}/bin/jterm1-support-bundle" \\
    "${INSTALL_DIR}/jterm1-support-bundle"
""",
)
replace_once(
    "packaging/install-release.sh",
    """install -Dm644 "${SCRIPT_DIR}/share/doc/jterm1/README.md" \\
    "${DOC_DIR}/README.md"
""",
    """install -Dm644 "${SCRIPT_DIR}/share/doc/jterm1/README.md" \\
    "${DOC_DIR}/README.md"
install -Dm644 "${SCRIPT_DIR}/share/doc/jterm1/Cargo.lock" \\
    "${DOC_DIR}/Cargo.lock"
install -Dm644 "${SCRIPT_DIR}/share/doc/jterm1/BUILDINFO" \\
    "${DOC_DIR}/BUILDINFO"
""",
)
replace_once(
    "packaging/install-release.sh",
    """  Binary:            ${INSTALL_DIR}/jterm1
""",
    """  Binary:            ${INSTALL_DIR}/jterm1
  Support bundle:    ${INSTALL_DIR}/jterm1-support-bundle
""",
)

# CI validates structured diagnostics and the support archive.
replace_once(
    ".github/workflows/ci.yml",
    """          target/release/jterm1 --doctor
          desktop-file-validate packaging/app.jterm1.desktop
""",
    """          target/release/jterm1 --doctor
          target/release/jterm1 --doctor --json | python3 -m json.tool >/dev/null
          desktop-file-validate packaging/app.jterm1.desktop
""",
)
replace_once(
    ".github/workflows/ci.yml",
    """          bash scripts/package-release.sh target/release/jterm1
          (cd target/dist && sha256sum --check *.sha256)
""",
    """          bash scripts/package-release.sh target/release/jterm1
          (cd target/dist && sha256sum --check *.sha256)
          JTERM1_BIN=target/release/jterm1 bash scripts/support-bundle.sh target/support
          tar -tzf target/support/jterm1-support-*.tar.gz | grep -F '/doctor.json'
""",
)

# Documentation.
replace_once(
    "README.md",
    """jterm1 --doctor
""",
    """jterm1 --doctor
jterm1 --doctor --json            # machine-readable support diagnostics
jterm1 --safe-mode                # isolated VTE + sh recovery session
""",
)
replace_once(
    "README.md",
    """make clippy    # repository lint policy
""",
    """make clippy    # repository lint policy
make security  # dependency audit + ShellCheck
""",
)
replace_once(
    "README.md",
    """## Terminal modes
""",
    """## Diagnostics and recovery

`jterm1 --doctor` reports configuration, shell, display, integrations, remote
readiness, permissions, and session-state metadata. Add `--json` for automation
or support tooling; neither format includes configuration contents, terminal
history, command output, environment values, or credentials.

When configuration, startup commands, session restore, or an integration causes
a bad launch, use:

```bash
jterm1 --safe-mode
```

Safe mode starts a local VTE pane with `sh`, skips session restore and persistence,
ignores configured startup commands and remote hosts, disables AI, notifications,
repository probes, history, and remote clipboard writes, and refuses to save or
hot-reload settings for that process.

Create a privacy-preserving support archive with:

```bash
jterm1-support-bundle ~/Desktop
```

Review the archive before sharing it. The bundle contains structured diagnostics,
system identity, linked-library information, and file metadata only.

## Terminal modes
""",
)
replace_once(
    "packaging/RELEASE_README.md",
    """After installation:

```bash
jterm1 --doctor
jterm1
```
""",
    """After installation:

```bash
jterm1 --doctor
jterm1 --doctor --json
jterm1 --safe-mode
jterm1
```

For support, `jterm1-support-bundle [OUTPUT_DIRECTORY]` creates a privacy-preserving
archive that excludes configuration contents, terminal history/output, and secret
values.
""",
)
replace_once(
    "CHANGELOG.md",
    """### Added

""",
    """### Added

- Isolated `--safe-mode` recovery sessions with VTE + `sh`, no restore or
  persistence, and network/state-producing integrations disabled.
- Machine-readable `--doctor --json` diagnostics and a privacy-preserving
  `jterm1-support-bundle` archive generator.
- A scheduled dependency vulnerability audit, ShellCheck gate, shared
  `make security` command, and repository Rust toolchain contract.
- Build provenance metadata and the exact Cargo lockfile in portable bundles.

""",
)
replace_once(
    "CONTRIBUTING.md",
    """make verify
```
""",
    """make verify
make security
```
""",
)
replace_once(
    "SECURITY.md",
    """Do not open a public issue for an unpatched vulnerability or include live
credentials, private keys, access tokens, or sensitive terminal output in a
report. Replace secrets with clearly marked test values.
""",
    """Do not open a public issue for an unpatched vulnerability or include live
credentials, private keys, access tokens, or sensitive terminal output in a
report. Replace secrets with clearly marked test values. The installed
`jterm1-support-bundle` command intentionally excludes configuration contents,
terminal history/output, environment values, and credentials; review its files
before attaching the archive.
""",
)

print("round 2 migration applied")
