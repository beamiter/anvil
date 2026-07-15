//! Human- and machine-readable runtime diagnostics.
//!
//! The report deliberately excludes configuration contents, terminal history,
//! command output, environment values, and credentials. It is safe to attach to
//! a support request after reviewing the generated text or JSON.

use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::cli::ReportFormat;
use crate::config::{self, choose_shell_argv, config_file_path, load_config, TerminalMode};
use crate::config_store::{self, ConfigLockStatus};

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

    let validation = config_store::validate_current_config();
    if !validation.exists() {
        report.push(
            "config",
            CheckStatus::Warning,
            format!(
                "{} does not exist (built-in defaults)",
                config_path.display()
            ),
        );
    } else if validation.errors() > 0 {
        report.push(
            "config",
            CheckStatus::Error,
            format!(
                "{} has {} validation error(s); run `jterm1 --check-config`",
                config_path.display(),
                validation.errors()
            ),
        );
    } else if validation.warnings() > 0 {
        report.push(
            "config",
            CheckStatus::Warning,
            format!(
                "{} is readable with {} warning(s); run `jterm1 --check-config`",
                config_path.display(),
                validation.warnings()
            ),
        );
    } else {
        report.push("config", CheckStatus::Ok, config_path.display().to_string());
    }

    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(&config_path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            report.push("config permissions", CheckStatus::Ok, format!("{mode:04o}"));
        } else {
            report.push(
                "config permissions",
                CheckStatus::Warning,
                format!("{mode:04o} (recommended: 0600)"),
            );
        }
    }

    let backup_count = config_store::backup_paths()
        .into_iter()
        .filter(|path| path.is_file())
        .count();
    report.push(
        "config backups",
        if backup_count > 0 {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        if backup_count > 0 {
            format!("{backup_count} rotating backup(s) available")
        } else {
            "none yet; backups are created after in-app saves".to_string()
        },
    );

    match config_store::lock_status() {
        ConfigLockStatus::Clear => {
            report.push("config write lock", CheckStatus::Ok, "clear");
        }
        ConfigLockStatus::Active => report.push(
            "config write lock",
            CheckStatus::Warning,
            "another process may currently be saving settings",
        ),
        ConfigLockStatus::Unavailable => report.push(
            "config write lock",
            CheckStatus::Warning,
            "status could not be inspected",
        ),
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
        report.push("notifications", CheckStatus::Ok, "notify-send available");
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

pub(crate) fn run(format: ReportFormat) -> bool {
    let report = collect();
    let printed = match format {
        ReportFormat::Human => print_human(&report),
        ReportFormat::Json => print_json(&report),
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
