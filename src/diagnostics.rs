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
use crate::config::{choose_shell_argv, config_file_path, load_config, TerminalMode};
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
        if crate::host::is_flatpak() {
            crate::host::command("test")
                .args(["-x", executable])
                .status()
                .is_ok_and(|status| status.success())
        } else {
            let Ok(metadata) = fs::metadata(path) else {
                return false;
            };
            if !metadata.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    } else {
        crate::host::command_available(executable)
    }
}

/// Support-bundle mode retains readiness checks while suppressing local paths
/// and user-authored values. It is intentionally an internal environment flag,
/// not a general command-line option.
fn diagnostics_redacted() -> bool {
    std::env::var_os("ANVIL_DIAGNOSTICS_REDACT")
        .is_some_and(|value| !value.is_empty() && value != "0")
}

fn diagnostic_path(path: &Path) -> String {
    if diagnostics_redacted() {
        "<config-file>".to_string()
    } else {
        path.display().to_string()
    }
}

fn config_backup_health() -> (usize, usize, usize) {
    let mut present = 0;
    let mut valid = 0;
    let mut invalid_or_unreadable = 0;
    for path in config_store::backup_paths() {
        if !path.exists() {
            continue;
        }
        present += 1;
        let validation = config_store::validate_path(&path);
        if validation.exists() && validation.errors() == 0 {
            valid += 1;
        } else {
            invalid_or_unreadable += 1;
        }
    }
    (present, valid, invalid_or_unreadable)
}

fn workflow_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "toml" | "yaml" | "yml"
            )
        })
}

fn workflow_discovery() -> (usize, usize, usize, usize) {
    let dirs = crate::workflows::workflow_dirs();
    let mut readable_dirs = 0;
    let mut rejected = 0;
    for dir in &dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        readable_dirs += 1;
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            if path.is_file() && workflow_file(&path) && crate::workflows::load_one(&path).is_err()
            {
                rejected += 1;
            }
        }
    }
    (
        crate::workflows::load_all(&dirs).len(),
        readable_dirs,
        dirs.len(),
        rejected,
    )
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
                diagnostic_path(&config_path)
            ),
        );
    } else if validation.errors() > 0 {
        report.push(
            "config",
            CheckStatus::Error,
            format!(
                "{} has {} validation error(s); run `anvil --check-config`",
                diagnostic_path(&config_path),
                validation.errors()
            ),
        );
    } else if validation.warnings() > 0 {
        report.push(
            "config",
            CheckStatus::Warning,
            format!(
                "{} is readable with {} warning(s); run `anvil --check-config`",
                diagnostic_path(&config_path),
                validation.warnings()
            ),
        );
    } else {
        report.push("config", CheckStatus::Ok, diagnostic_path(&config_path));
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

    let (backup_count, valid_backups, bad_backups) = config_backup_health();
    report.push(
        "config backups",
        if bad_backups > 0 || valid_backups == 0 {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        if backup_count == 0 {
            "none yet; backups are created after in-app saves".to_string()
        } else {
            format!("{valid_backups} valid, {bad_backups} invalid or unreadable")
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

    let flatpak = crate::host::is_flatpak();
    let bridge_available = crate::host::bridge_available();
    report.push(
        "runtime",
        if !flatpak || bridge_available {
            CheckStatus::Ok
        } else {
            CheckStatus::Error
        },
        if flatpak {
            format!(
                "flatpak; host bridge {}",
                if bridge_available {
                    "available"
                } else {
                    "missing"
                }
            )
        } else {
            "native".to_string()
        },
    );

    let (config, _, _) = load_config();
    let shell_argv = choose_shell_argv(config.shell.as_deref());
    let shell = shell_argv.first().cloned().unwrap_or_default();
    if executable_exists(&shell) {
        let detail = if diagnostics_redacted() {
            Path::new(&shell)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("available")
                .to_string()
        } else {
            shell_argv.join(" ")
        };
        report.push("shell", CheckStatus::Ok, detail);
    } else {
        report.push(
            "shell",
            CheckStatus::Error,
            if diagnostics_redacted() {
                "configured shell is not executable".to_string()
            } else {
                format!("not executable: {shell}")
            },
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
        Some(path) => report.push(
            "command history",
            CheckStatus::Ok,
            if diagnostics_redacted() {
                "enabled; metadata only".to_string()
            } else {
                path.clone()
            },
        ),
        None => report.push("command history", CheckStatus::Warning, "disabled"),
    }

    let (workflow_count, readable_workflow_dirs, workflow_dirs, rejected_workflows) =
        workflow_discovery();
    report.push(
        "workflows",
        if workflow_count == 0 || rejected_workflows > 0 {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        format!(
            "{workflow_count} available; {readable_workflow_dirs}/{workflow_dirs} search locations readable; {rejected_workflows} invalid or unreadable file(s)"
        ),
    );

    let welcome_notebook = crate::workflows::welcome_notebook_path();
    report.push(
        "welcome notebook",
        if welcome_notebook.is_some() {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        if welcome_notebook.is_some() {
            "available in configured/user/system/source assets"
        } else {
            "not found in configured/user/system/source assets"
        },
    );

    if !config.ai_enabled {
        report.push("AI", CheckStatus::Warning, "disabled by configuration");
    } else {
        match crate::ai::client_from_config(&config) {
            Ok(client) => report.push(
                "AI",
                CheckStatus::Ok,
                if diagnostics_redacted() {
                    format!(
                        "{} configured; API key {}",
                        client.provider.display_name(),
                        if client.api_key.is_some() {
                            "present"
                        } else {
                            "not set (optional for local/compatible endpoints)"
                        }
                    )
                } else {
                    client.display_name()
                },
            ),
            Err(error) => report.push(
                "AI",
                CheckStatus::Warning,
                if diagnostics_redacted() {
                    "provider configuration or credentials are incomplete".to_string()
                } else {
                    error
                },
            ),
        }
    }

    for (name, purpose) in [
        ("git", "repository status"),
        ("ssh", "remote sessions"),
        ("notify-send", "long-command notifications"),
    ] {
        let available = crate::host::command_available(name);
        report.push(
            name,
            if available {
                CheckStatus::Ok
            } else {
                CheckStatus::Warning
            },
            if available {
                format!("available ({purpose})")
            } else {
                format!("not found ({purpose} unavailable)")
            },
        );
    }

    if config.remote_hosts.is_empty() {
        report.push("remote hosts", CheckStatus::Ok, "none configured");
    } else if crate::host::command_available("ssh") {
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

    let (ready_snapshots, active_snapshots) = crate::session::session_snapshot_counts();
    report.push(
        "session snapshots",
        CheckStatus::Ok,
        format!("{ready_snapshots} ready, {active_snapshots} active"),
    );

    report
}

fn print_human(report: &DiagnosticReport) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "anvil {} diagnostics\n", report.version)?;
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
        eprintln!("anvil: failed to write diagnostics: {error}");
        return false;
    }
    report.healthy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn configured_shell_path_must_have_an_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "anvil-doctor-shell-permissions-{}",
            std::process::id()
        ));
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!executable_exists(path.to_str().unwrap()));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(executable_exists(path.to_str().unwrap()));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn workflow_discovery_recognizes_all_supported_extensions() {
        assert!(workflow_file(Path::new("one.toml")));
        assert!(workflow_file(Path::new("two.YAML")));
        assert!(workflow_file(Path::new("three.yml")));
        assert!(!workflow_file(Path::new("README.md")));
    }

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
