//! Human- and machine-readable runtime diagnostics.
//!
//! The report deliberately excludes configuration contents, terminal history,
//! command output, environment values, and credentials. It is safe to attach to
//! a support request after reviewing the generated text or JSON.

use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::cli::ReportFormat;
use crate::config::{choose_shell_argv, config_file_path, load_config};
use crate::config_store::{self, ConfigLockStatus};

const OPTIONAL_RUNTIME_TOOLS: [(&str, &str); 4] = [
    ("git", "repository status"),
    ("ssh", "remote sessions"),
    ("curl", "AI panel"),
    ("notify-send", "long-command notifications"),
];

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

const DIAGNOSTIC_PATH_BYTES: usize = 2 * 1024;

fn diagnostic_path_for(path: &Path, redacted: bool) -> String {
    if redacted {
        "<config-file>".to_string()
    } else {
        crate::review_input::safe_inline_display(&path.to_string_lossy(), DIAGNOSTIC_PATH_BYTES)
    }
}

fn diagnostic_path(path: &Path) -> String {
    diagnostic_path_for(path, diagnostics_redacted())
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

#[derive(Debug, Default)]
struct WorkflowDiscovery {
    available: usize,
    readable_locations: usize,
    locations: usize,
    refused: Vec<(PathBuf, String)>,
}

fn workflow_discovery() -> WorkflowDiscovery {
    // Every question here is asked through `crate::workflows`, which asks
    // `jterm_core::workflows`. This report used to carry its own
    // `toml|yaml|yml` predicate and an uncapped `read_dir` of every workflow
    // directory — a second implementation of the same on-disk contract inside
    // one app, and the only place that ignored the per-directory caps the
    // loader exists to enforce.
    let dirs = crate::workflows::workflow_dirs();
    let readable_dirs = dirs.iter().filter(|dir| fs::read_dir(dir).is_ok()).count();
    let scan = crate::workflows::scan(&dirs);
    WorkflowDiscovery {
        available: scan.workflows.len(),
        readable_locations: readable_dirs,
        locations: dirs.len(),
        refused: scan.refused,
    }
}

const WORKFLOW_DIAGNOSTIC_FIELD_BYTES: usize = 256;

fn workflow_diagnostic_detail(discovery: &WorkflowDiscovery, redacted: bool) -> String {
    let mut detail = format!(
        "{} available; {}/{} search locations readable; {} invalid or unreadable file(s)",
        discovery.available,
        discovery.readable_locations,
        discovery.locations,
        discovery.refused.len()
    );
    let Some((path, reason)) = discovery.refused.first() else {
        return detail;
    };
    if redacted {
        detail.push_str("; rejected file details redacted");
        return detail;
    }

    // A scanned-directory writer chooses the path, and serde parse errors can
    // quote source lines verbatim. Neither may regain formatting effects in a
    // terminal or a JSON consumer just because this is a headless surface.
    let path = crate::review_input::safe_inline_display(
        &path.to_string_lossy(),
        WORKFLOW_DIAGNOSTIC_FIELD_BYTES,
    );
    let reason = crate::review_input::safe_inline_display(reason, WORKFLOW_DIAGNOSTIC_FIELD_BYTES);
    detail.push_str(&format!("; first rejected file: {path}: {reason}"));
    detail
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

    let workflow_discovery = workflow_discovery();
    report.push(
        "workflows",
        if workflow_discovery.available == 0 || !workflow_discovery.refused.is_empty() {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        workflow_diagnostic_detail(&workflow_discovery, diagnostics_redacted()),
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

    let curl_available = crate::host::command_available("curl");
    if !config.ai_enabled {
        report.push("AI", CheckStatus::Warning, "disabled by configuration");
    } else {
        match crate::ai::client_from_config(&config) {
            Ok(client) => report.push(
                "AI",
                if curl_available {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warning
                },
                if diagnostics_redacted() {
                    format!(
                        "{} configured; API key {}; curl {}",
                        client.provider.display_name(),
                        if client.api_key.is_some() {
                            "present"
                        } else {
                            "not set (optional for local/compatible endpoints)"
                        },
                        if curl_available {
                            "available"
                        } else {
                            "missing"
                        }
                    )
                } else {
                    format!(
                        "{}; curl {}",
                        client.display_name(),
                        if curl_available {
                            "available"
                        } else {
                            "missing"
                        }
                    )
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

    for (name, purpose) in OPTIONAL_RUNTIME_TOOLS {
        let available = if name == "curl" {
            curl_available
        } else {
            crate::host::command_available(name)
        };
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

    let containers = config
        .remote_hosts
        .iter()
        .filter(|host| host.docker)
        .count();
    let over_ssh = config.remote_hosts.len().saturating_sub(containers);
    let ssh_available = over_ssh == 0 || crate::host::command_available("ssh");
    let docker_available = containers == 0 || crate::host::command_available("docker");
    let remote_status = if ssh_available && docker_available {
        CheckStatus::Ok
    } else {
        CheckStatus::Error
    };
    let remote_detail = if config.remote_hosts.is_empty() {
        "none configured".to_string()
    } else {
        let mut detail = format!("{} configured", config.remote_hosts.len());
        if over_ssh > 0 {
            detail.push_str(&format!(
                "; {over_ssh} over ssh, which is {}",
                if ssh_available {
                    "available"
                } else {
                    "missing"
                }
            ));
        }
        if containers > 0 {
            detail.push_str(&format!(
                "; {containers} in containers, and docker is {}",
                if docker_available {
                    "available"
                } else {
                    "missing"
                }
            ));
        }
        detail
    };
    report.push("remote hosts", remote_status, remote_detail);

    report.push(
        "terminal mode",
        CheckStatus::Ok,
        config.terminal_mode.as_str(),
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
    fn doctor_checks_curl_for_the_ai_panel() {
        assert!(OPTIONAL_RUNTIME_TOOLS
            .iter()
            .any(|(name, purpose)| *name == "curl" && *purpose == "AI panel"));
    }

    #[test]
    fn config_path_diagnostic_is_bounded_and_cannot_format_the_terminal() {
        let path = PathBuf::from(format!(
            "/config/{}\n\u{1b}]0;PWNED\u{7}\u{202e}.toml",
            "x".repeat(DIAGNOSTIC_PATH_BYTES + 32)
        ));
        let detail = diagnostic_path_for(&path, false);
        assert!(detail.len() <= DIAGNOSTIC_PATH_BYTES);
        assert!(!detail.contains('\n'), "{detail}");
        assert!(!detail.contains('\u{1b}'), "{detail}");
        assert!(!detail.contains('\u{7}'), "{detail}");
        assert!(!detail.contains('\u{202e}'), "{detail}");
        assert!(detail.contains('…'), "{detail}");
    }

    #[test]
    fn redacted_config_path_diagnostic_never_retains_the_path() {
        assert_eq!(
            diagnostic_path_for(Path::new("/private/config.toml"), true),
            "<config-file>"
        );
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

    #[test]
    fn workflow_diagnostic_names_a_safe_bounded_rejection_sample() {
        let discovery = WorkflowDiscovery {
            available: 6,
            readable_locations: 2,
            locations: 4,
            refused: vec![(
                PathBuf::from(format!("/w/{}\u{1b}]0;PWNED\u{7}.toml", "x".repeat(300))),
                "parse TOML: line 2\ncommand = \"echo \u{202e}".to_string(),
            )],
        };

        let detail = workflow_diagnostic_detail(&discovery, false);
        assert!(detail.starts_with(
            "6 available; 2/4 search locations readable; 1 invalid or unreadable file(s); \
             first rejected file: "
        ));
        assert!(!detail.contains('\u{1b}'), "{detail}");
        assert!(!detail.contains('\u{7}'), "{detail}");
        assert!(!detail.contains('\u{202e}'), "{detail}");
        assert!(!detail.contains('\n'), "{detail}");
        assert!(
            detail.contains('…'),
            "the long path should be bounded: {detail}"
        );
    }

    #[test]
    fn redacted_workflow_diagnostic_retains_counts_but_not_file_content() {
        let discovery = WorkflowDiscovery {
            available: 2,
            readable_locations: 1,
            locations: 3,
            refused: vec![(
                PathBuf::from("/private/workflows/secret.toml"),
                "parse TOML: command = \"private value\"".to_string(),
            )],
        };

        let detail = workflow_diagnostic_detail(&discovery, true);
        assert_eq!(
            detail,
            "2 available; 1/3 search locations readable; 1 invalid or unreadable file(s); \
             rejected file details redacted"
        );
        assert!(!detail.contains("secret"));
        assert!(!detail.contains("private value"));
    }

    #[test]
    fn healthy_workflow_diagnostic_has_no_rejection_suffix() {
        assert_eq!(
            workflow_diagnostic_detail(
                &WorkflowDiscovery {
                    available: 6,
                    readable_locations: 2,
                    locations: 4,
                    refused: Vec::new(),
                },
                false,
            ),
            "6 available; 2/4 search locations readable; 0 invalid or unreadable file(s)"
        );
    }
}
