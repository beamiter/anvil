//! Small dependency-free command-line contract for launching and diagnosing
//! jterm1. Parsing stays independent of GTK so `--help`, `--version`, and
//! `--doctor` remain fast and usable on headless machines.

use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Block,
    Vte,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportFormat {
    Human,
    Json,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LaunchOptions {
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) execute: Option<Vec<String>>,
    pub(crate) no_restore: bool,
    pub(crate) safe_mode: bool,
    pub(crate) mode: Option<Mode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellIntegration {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Run(LaunchOptions),
    Help,
    Version,
    Doctor(ReportFormat),
    CheckConfig(ReportFormat),
    RestoreConfigBackup,
    ConfigPath,
    InitConfig,
    PrintShellIntegration(ShellIntegration),
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let args: Vec<OsString> = args.into_iter().collect();
    if args.first().is_some_and(|arg| arg == "--doctor") {
        return Ok(Command::Doctor(parse_report_format(
            &args,
            "--doctor [--json]",
        )?));
    }
    if args.first().is_some_and(|arg| arg == "--check-config") {
        return Ok(Command::CheckConfig(parse_report_format(
            &args,
            "--check-config [--json]",
        )?));
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--restore-config-backup")
    {
        require_exact_args(&args, 1, "--restore-config-backup")?;
        return Ok(Command::RestoreConfigBackup);
    }
    if args.first().is_some_and(|arg| arg == "--config-path") {
        require_exact_args(&args, 1, "--config-path")?;
        return Ok(Command::ConfigPath);
    }
    if args.first().is_some_and(|arg| arg == "--init-config") {
        require_exact_args(&args, 1, "--init-config")?;
        return Ok(Command::InitConfig);
    }
    if args.first().is_some_and(|arg| arg == "--shell-integration") {
        require_exact_args(&args, 2, "--shell-integration <shell>")?;
        let shell = args[1]
            .to_str()
            .ok_or_else(|| "shell name must be valid UTF-8".to_string())?;
        let shell = match shell.to_ascii_lowercase().as_str() {
            "bash" => ShellIntegration::Bash,
            "zsh" => ShellIntegration::Zsh,
            "fish" => ShellIntegration::Fish,
            "powershell" | "pwsh" | "ps1" => ShellIntegration::PowerShell,
            _ => {
                return Err(format!(
                    "unsupported shell '{shell}' (use bash, zsh, fish, or pwsh)"
                ))
            }
        };
        return Ok(Command::PrintShellIntegration(shell));
    }

    let mut launch = LaunchOptions::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help),
            Some("-V" | "--version") => return Ok(Command::Version),
            Some("--no-restore") => launch.no_restore = true,
            Some("--safe-mode") => {
                launch.safe_mode = true;
                launch.no_restore = true;
            }
            Some("-d" | "--working-directory") => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| "--working-directory requires a path".to_string())?;
                launch.working_directory = Some(PathBuf::from(path));
            }
            Some("--mode") => {
                index += 1;
                let mode = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "--mode requires 'block' or 'vte'".to_string())?;
                launch.mode = Some(match mode.to_ascii_lowercase().as_str() {
                    "block" => Mode::Block,
                    "vte" => Mode::Vte,
                    _ => return Err(format!("invalid terminal mode '{mode}' (use block or vte)")),
                });
            }
            Some("-e" | "--execute" | "--") => {
                let command = args[index + 1..]
                    .iter()
                    .map(|arg| {
                        arg.clone().into_string().map_err(|_| {
                            "command arguments must be valid UTF-8 in this release".to_string()
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if command.is_empty() {
                    return Err(format!("{} requires a command", arg.to_string_lossy()));
                }
                launch.execute = Some(command);
                break;
            }
            Some(value) if value.starts_with('-') => {
                return Err(format!("unknown option '{value}'"));
            }
            _ => {
                if launch.working_directory.is_some() {
                    return Err("only one working directory may be specified".to_string());
                }
                launch.working_directory = Some(PathBuf::from(arg));
            }
        }
        index += 1;
    }

    if launch.safe_mode {
        if launch.mode.is_some() {
            return Err("--safe-mode cannot be combined with --mode".to_string());
        }
        if launch.execute.is_some() {
            return Err("--safe-mode cannot be combined with --execute".to_string());
        }
    }

    Ok(Command::Run(launch))
}

fn parse_report_format(args: &[OsString], usage: &str) -> Result<ReportFormat, String> {
    match args {
        [_] => Ok(ReportFormat::Human),
        [_, flag] if flag == "--json" => Ok(ReportFormat::Json),
        _ => Err(format!("usage: jterm1 {usage}")),
    }
}

fn require_exact_args(args: &[OsString], expected: usize, usage: &str) -> Result<(), String> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(format!("usage: jterm1 {usage}"))
    }
}

pub(crate) const HELP: &str = r#"jterm1 — a Block-first terminal workspace

Usage:
  jterm1 [OPTIONS] [DIRECTORY]
  jterm1 [OPTIONS] --execute COMMAND [ARG...]

Launch options:
  -d, --working-directory DIR  Start in DIR
  -e, --execute COMMAND ...    Run a command instead of the configured shell
      --mode block|vte         Override the terminal backend for this window
      --no-restore             Start a fresh workspace
      --safe-mode              Use isolated VTE defaults without restore or persistence

Utilities:
      --doctor [--json]        Check configuration and runtime dependencies
      --check-config [--json]  Validate keys, types, ranges, colors, and shortcuts
      --restore-config-backup  Restore the newest valid rotating config backup
      --config-path            Print the active configuration file path
      --init-config            Create a documented config without overwriting one
      --shell-integration SH   Print integration for bash, zsh, fish, or pwsh
  -h, --help                   Show this help
  -V, --version                Show the version

Examples:
  jterm1 ~/project
  jterm1 --mode block --no-restore
  jterm1 --safe-mode
  jterm1 --doctor --json
  jterm1 --check-config
  jterm1 --restore-config-backup
  jterm1 -d /tmp -e bash -lc 'printf "hello\\n"'
  source <(jterm1 --shell-integration bash)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_launch_options_and_execute_remainder() {
        let command = parse_strs(&[
            "--mode", "block", "-d", "/tmp", "-e", "bash", "-lc", "echo hi",
        ])
        .unwrap();
        assert_eq!(
            command,
            Command::Run(LaunchOptions {
                working_directory: Some(PathBuf::from("/tmp")),
                execute: Some(vec!["bash".into(), "-lc".into(), "echo hi".into()]),
                no_restore: false,
                safe_mode: false,
                mode: Some(Mode::Block),
            })
        );
    }

    #[test]
    fn positional_argument_is_working_directory() {
        let Command::Run(options) = parse_strs(&["~/project"]).unwrap() else {
            panic!("expected run")
        };
        assert_eq!(options.working_directory, Some(PathBuf::from("~/project")));
    }

    #[test]
    fn parses_shell_integration_alias() {
        assert_eq!(
            parse_strs(&["--shell-integration", "pwsh"]).unwrap(),
            Command::PrintShellIntegration(ShellIntegration::PowerShell)
        );
    }

    #[test]
    fn rejects_unknown_option() {
        assert!(parse_strs(&["--wat"])
            .unwrap_err()
            .contains("unknown option"));
    }

    #[test]
    fn execute_requires_command() {
        assert!(parse_strs(&["-e"])
            .unwrap_err()
            .contains("requires a command"));
    }

    #[test]
    fn execute_remainder_may_contain_help_or_version_flags() {
        let Command::Run(help) = parse_strs(&["-e", "cargo", "--help"]).unwrap() else {
            panic!("expected run")
        };
        assert_eq!(help.execute, Some(vec!["cargo".into(), "--help".into()]));

        let Command::Run(version) = parse_strs(&["--execute", "bash", "--version"]).unwrap() else {
            panic!("expected run")
        };
        assert_eq!(
            version.execute,
            Some(vec!["bash".into(), "--version".into()])
        );
    }

    #[test]
    fn doctor_supports_human_and_json_formats() {
        assert_eq!(
            parse_strs(&["--doctor"]).unwrap(),
            Command::Doctor(ReportFormat::Human)
        );
        assert_eq!(
            parse_strs(&["--doctor", "--json"]).unwrap(),
            Command::Doctor(ReportFormat::Json)
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
        assert!(options.mode.is_none());
    }

    #[test]
    fn safe_mode_rejects_backend_and_execute_overrides() {
        assert!(parse_strs(&["--safe-mode", "--mode", "block"])
            .unwrap_err()
            .contains("--mode"));
        assert!(parse_strs(&["--safe-mode", "--execute", "bash"])
            .unwrap_err()
            .contains("--execute"));
    }

    #[test]
    fn config_check_supports_human_and_json_formats() {
        assert_eq!(
            parse_strs(&["--check-config"]).unwrap(),
            Command::CheckConfig(ReportFormat::Human)
        );
        assert_eq!(
            parse_strs(&["--check-config", "--json"]).unwrap(),
            Command::CheckConfig(ReportFormat::Json)
        );
        assert!(parse_strs(&["--check-config", "--verbose"]).is_err());
    }

    #[test]
    fn config_recovery_utilities_require_exact_arguments() {
        assert_eq!(
            parse_strs(&["--restore-config-backup"]).unwrap(),
            Command::RestoreConfigBackup
        );
        assert_eq!(parse_strs(&["--config-path"]).unwrap(), Command::ConfigPath);
        assert!(parse_strs(&["--restore-config-backup", "extra"]).is_err());
        assert!(parse_strs(&["--config-path", "extra"]).is_err());
    }
}
