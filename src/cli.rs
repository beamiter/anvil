//! Small dependency-free command-line contract for launching and diagnosing
//! anvil. Parsing stays independent of GTK so `--help`, `--version`, and
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
    CheckConfig(Option<PathBuf>, ReportFormat),
    RestoreConfigBackup,
    ConfigPath,
    InitConfig,
    PrintDefaultConfig,
    PrintShellIntegration(ShellIntegration),
    PrintCompletion(ShellIntegration),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedArgs {
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) command: Command,
}

fn set_utility(utility: &mut Option<Command>, command: Command) -> Result<(), String> {
    if utility.is_some() {
        return Err("only one utility command may be used at a time".to_string());
    }
    *utility = Some(command);
    Ok(())
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value.to_ascii_lowercase().as_str() {
        "block" => Ok(Mode::Block),
        "vte" => Ok(Mode::Vte),
        _ => Err(format!(
            "invalid terminal mode '{value}' (use block or vte)"
        )),
    }
}

fn parse_shell(value: &str) -> Result<ShellIntegration, String> {
    match value.to_ascii_lowercase().as_str() {
        "bash" => Ok(ShellIntegration::Bash),
        "zsh" => Ok(ShellIntegration::Zsh),
        "fish" => Ok(ShellIntegration::Fish),
        "powershell" | "pwsh" | "ps1" => Ok(ShellIntegration::PowerShell),
        _ => Err(format!(
            "unsupported shell '{value}' (use bash, zsh, fish, or pwsh)"
        )),
    }
}

pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<ParsedArgs, String> {
    let args: Vec<OsString> = args.into_iter().collect();
    let mut config_path = None;
    let mut utility = None;
    let mut launch = LaunchOptions::default();
    let mut report_json = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let option = arg
            .to_str()
            .ok_or_else(|| "options must be valid UTF-8".to_string())?;
        match option {
            "-h" | "--help" => set_utility(&mut utility, Command::Help)?,
            "-V" | "--version" => set_utility(&mut utility, Command::Version)?,
            "--doctor" => set_utility(&mut utility, Command::Doctor(ReportFormat::Human))?,
            "--check-config" => {
                let path = args.get(index + 1).and_then(|next| {
                    (!next.to_string_lossy().starts_with('-')).then(|| PathBuf::from(next))
                });
                if path.is_some() {
                    index += 1;
                }
                set_utility(
                    &mut utility,
                    Command::CheckConfig(path, ReportFormat::Human),
                )?;
            }
            "--restore-config-backup" => set_utility(&mut utility, Command::RestoreConfigBackup)?,
            "--config-path" | "--print-config-path" => {
                set_utility(&mut utility, Command::ConfigPath)?
            }
            "--init-config" => set_utility(&mut utility, Command::InitConfig)?,
            "--print-default-config" => set_utility(&mut utility, Command::PrintDefaultConfig)?,
            "--shell-integration" => {
                index += 1;
                let shell = args
                    .get(index)
                    .ok_or_else(|| "--shell-integration requires a shell".to_string())?
                    .to_str()
                    .ok_or_else(|| "shell name must be valid UTF-8".to_string())?;
                set_utility(
                    &mut utility,
                    Command::PrintShellIntegration(parse_shell(shell)?),
                )?;
            }
            "--generate-completion" | "--completion" => {
                index += 1;
                let shell = args
                    .get(index)
                    .ok_or_else(|| format!("{option} requires a shell"))?
                    .to_str()
                    .ok_or_else(|| "shell name must be valid UTF-8".to_string())?;
                set_utility(&mut utility, Command::PrintCompletion(parse_shell(shell)?))?;
            }
            "--json" => report_json = true,
            "-c" | "--config" => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| format!("{option} requires a path"))?;
                if path.to_string_lossy().starts_with('-') {
                    return Err(format!("{option} requires a path"));
                }
                config_path = Some(PathBuf::from(path));
            }
            "--no-restore" => launch.no_restore = true,
            "--safe-mode" => {
                launch.safe_mode = true;
                launch.no_restore = true;
            }
            "-d" | "--working-directory" => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| "--working-directory requires a path".to_string())?;
                launch.working_directory = Some(PathBuf::from(path));
            }
            "--mode" => {
                index += 1;
                let mode = args
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| "--mode requires 'block' or 'vte'".to_string())?;
                launch.mode = Some(parse_mode(mode)?);
            }
            "-e" | "--execute" | "--" => {
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
            _ if option.starts_with("--config=") => {
                let value = option.trim_start_matches("--config=");
                if value.is_empty() {
                    return Err("--config requires a path".to_string());
                }
                config_path = Some(PathBuf::from(value));
            }
            _ if option.starts_with("--check-config=") => {
                let value = option.trim_start_matches("--check-config=");
                if value.is_empty() {
                    return Err("--check-config requires a non-empty path".to_string());
                }
                set_utility(
                    &mut utility,
                    Command::CheckConfig(Some(PathBuf::from(value)), ReportFormat::Human),
                )?;
            }
            _ if option.starts_with("--shell-integration=") => {
                set_utility(
                    &mut utility,
                    Command::PrintShellIntegration(parse_shell(
                        option.trim_start_matches("--shell-integration="),
                    )?),
                )?;
            }
            _ if option.starts_with("--generate-completion=") => {
                set_utility(
                    &mut utility,
                    Command::PrintCompletion(parse_shell(
                        option.trim_start_matches("--generate-completion="),
                    )?),
                )?;
            }
            _ if option.starts_with("--completion=") => {
                set_utility(
                    &mut utility,
                    Command::PrintCompletion(parse_shell(
                        option.trim_start_matches("--completion="),
                    )?),
                )?;
            }
            _ if option.starts_with("--mode=") => {
                launch.mode = Some(parse_mode(option.trim_start_matches("--mode="))?);
            }
            _ if option.starts_with("--working-directory=") => {
                let value = option.trim_start_matches("--working-directory=");
                if value.is_empty() {
                    return Err("--working-directory requires a path".to_string());
                }
                launch.working_directory = Some(PathBuf::from(value));
            }
            _ if option.starts_with('-') => {
                return Err(format!("unknown option '{option}'"));
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

    // Keep the check utility tolerant of `--json` before its optional path (or
    // even a path before the utility) without treating that path as a GUI cwd.
    if let Some(Command::CheckConfig(path @ None, _)) = utility.as_mut() {
        if launch.execute.is_none()
            && !launch.no_restore
            && !launch.safe_mode
            && launch.mode.is_none()
        {
            *path = launch.working_directory.take();
        }
    }

    if report_json {
        utility = match utility.take() {
            Some(Command::Doctor(_)) => Some(Command::Doctor(ReportFormat::Json)),
            Some(Command::CheckConfig(path, _)) => {
                Some(Command::CheckConfig(path, ReportFormat::Json))
            }
            Some(_) => {
                return Err("--json is only valid with --doctor or --check-config".to_string());
            }
            None => return Err("--json requires --doctor or --check-config".to_string()),
        };
    }

    if utility.is_some() && launch != LaunchOptions::default() {
        return Err("launch options cannot be combined with a utility command".to_string());
    }

    if launch.safe_mode {
        if launch.mode.is_some() {
            return Err("--safe-mode cannot be combined with --mode".to_string());
        }
        if launch.execute.is_some() {
            return Err("--safe-mode cannot be combined with --execute".to_string());
        }
    }

    Ok(ParsedArgs {
        config_path,
        command: utility.unwrap_or(Command::Run(launch)),
    })
}

pub(crate) const HELP: &str = r#"anvil — a Block-first terminal workspace

Usage:
  anvil [OPTIONS] [DIRECTORY]
  anvil [OPTIONS] --execute COMMAND [ARG...]

Global options:
  -c, --config PATH           Use an alternate config file for this process

Launch options:
  -d, --working-directory DIR  Start in DIR
  -e, --execute COMMAND ...    Run a command instead of the configured shell
      --mode block|vte         Override the terminal backend for this window
      --no-restore             Start a fresh workspace
      --safe-mode              Use isolated VTE defaults without restore or persistence

Utilities:
      --doctor [--json]        Check configuration and runtime dependencies
      --check-config [PATH] [--json]
                               Validate keys, types, ranges, colors, and shortcuts
      --restore-config-backup  Restore the newest valid rotating config backup
      --config-path            Print the active configuration file path
      --init-config            Create a documented config without overwriting one
      --print-default-config   Print the bundled example configuration
      --shell-integration SH   Print integration for bash, zsh, fish, or pwsh
      --generate-completion SH Print CLI completion for bash, zsh, fish, or pwsh
  -h, --help                   Show this help
  -V, --version                Show the version

Examples:
  anvil ~/project
  anvil --mode block --no-restore
  anvil --safe-mode
  anvil --doctor --json
  anvil --check-config
  anvil --check-config ~/custom-anvil.toml --json
  anvil --config ~/custom-anvil.toml --config-path
  anvil --restore-config-backup
  anvil -d /tmp -e bash -lc 'printf "hello\\n"'
  source <(anvil --shell-integration bash)
  source <(anvil --generate-completion bash)

ANVIL_CONFIG provides the same process-local config-path override as --config.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<ParsedArgs, String> {
        parse(args.iter().map(OsString::from))
    }

    fn parse_command(args: &[&str]) -> Result<Command, String> {
        parse_strs(args).map(|parsed| parsed.command)
    }

    #[test]
    fn parses_launch_options_and_execute_remainder() {
        let command = parse_command(&[
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
        let Command::Run(options) = parse_command(&["~/project"]).unwrap() else {
            panic!("expected run")
        };
        assert_eq!(options.working_directory, Some(PathBuf::from("~/project")));
    }

    #[test]
    fn parses_shell_integration_alias() {
        assert_eq!(
            parse_command(&["--shell-integration", "pwsh"]).unwrap(),
            Command::PrintShellIntegration(ShellIntegration::PowerShell)
        );
    }

    #[test]
    fn parses_completion_utility_and_alias() {
        assert_eq!(
            parse_command(&["--generate-completion=fish"]).unwrap(),
            Command::PrintCompletion(ShellIntegration::Fish)
        );
        assert_eq!(
            parse_command(&["--completion", "powershell"]).unwrap(),
            Command::PrintCompletion(ShellIntegration::PowerShell)
        );
    }

    #[test]
    fn bundled_completions_cover_every_shell_and_core_option() {
        for script in [
            include_str!("../scripts/completions/anvil.bash"),
            include_str!("../scripts/completions/_anvil"),
            include_str!("../scripts/completions/anvil.fish"),
            include_str!("../scripts/completions/anvil.ps1"),
        ] {
            assert!(script.contains("anvil"));
            assert!(script.contains("generate-completion"));
            assert!(script.contains("shell-integration"));
            assert!(script.contains("working-directory"));
            assert!(script.contains("safe-mode"));
        }
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
        let Command::Run(help) = parse_command(&["-e", "cargo", "--help"]).unwrap() else {
            panic!("expected run")
        };
        assert_eq!(help.execute, Some(vec!["cargo".into(), "--help".into()]));

        let Command::Run(version) = parse_command(&["--execute", "bash", "--version"]).unwrap()
        else {
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
            parse_command(&["--doctor"]).unwrap(),
            Command::Doctor(ReportFormat::Human)
        );
        assert_eq!(
            parse_command(&["--doctor", "--json"]).unwrap(),
            Command::Doctor(ReportFormat::Json)
        );
        assert!(parse_strs(&["--doctor", "--verbose"]).is_err());
    }

    #[test]
    fn safe_mode_implies_a_fresh_workspace() {
        let Command::Run(options) = parse_command(&["--safe-mode"]).unwrap() else {
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
            parse_command(&["--check-config"]).unwrap(),
            Command::CheckConfig(None, ReportFormat::Human)
        );
        assert_eq!(
            parse_command(&["--check-config", "--json"]).unwrap(),
            Command::CheckConfig(None, ReportFormat::Json)
        );
        assert!(parse_strs(&["--check-config", "--verbose"]).is_err());
    }

    #[test]
    fn check_config_accepts_explicit_path_without_setting_global_override() {
        for args in [
            ["--check-config", "/tmp/检查.toml", "--json"],
            ["--json", "--check-config", "/tmp/检查.toml"],
            ["--check-config", "--json", "/tmp/检查.toml"],
        ] {
            let parsed = parse_strs(&args).unwrap();
            assert_eq!(parsed.config_path, None);
            assert_eq!(
                parsed.command,
                Command::CheckConfig(Some(PathBuf::from("/tmp/检查.toml")), ReportFormat::Json)
            );
        }
        assert_eq!(
            parse_command(&["--check-config=/tmp/explicit.toml"]).unwrap(),
            Command::CheckConfig(
                Some(PathBuf::from("/tmp/explicit.toml")),
                ReportFormat::Human
            )
        );
    }

    #[test]
    fn global_config_combines_with_launch_and_order_independent_utilities() {
        let launch = parse_strs(&["-c", "/tmp/custom.toml", "--no-restore"]).unwrap();
        assert_eq!(launch.config_path, Some(PathBuf::from("/tmp/custom.toml")));
        assert!(matches!(launch.command, Command::Run(_)));

        for args in [
            vec!["--config", "/tmp/custom.toml", "--doctor", "--json"],
            vec!["--doctor", "--json", "-c", "/tmp/custom.toml"],
            vec!["--check-config", "--json", "-c", "/tmp/custom.toml"],
            vec!["--config-path", "--config=/tmp/custom.toml"],
            vec!["--init-config", "-c", "/tmp/custom.toml"],
            vec!["-c", "/tmp/custom.toml", "--restore-config-backup"],
            vec!["--print-default-config", "--config", "/tmp/custom.toml"],
        ] {
            let parsed = parse_strs(&args).unwrap();
            assert_eq!(
                parsed.config_path,
                Some(PathBuf::from("/tmp/custom.toml")),
                "args: {args:?}"
            );
            assert!(!matches!(parsed.command, Command::Run(_)));
        }
    }

    #[test]
    fn config_recovery_utilities_require_exact_arguments() {
        assert_eq!(
            parse_command(&["--restore-config-backup"]).unwrap(),
            Command::RestoreConfigBackup
        );
        assert_eq!(
            parse_command(&["--config-path"]).unwrap(),
            Command::ConfigPath
        );
        assert_eq!(
            parse_command(&["--print-config-path"]).unwrap(),
            Command::ConfigPath
        );
        assert_eq!(
            parse_command(&["--print-default-config"]).unwrap(),
            Command::PrintDefaultConfig
        );
        assert!(parse_strs(&["--restore-config-backup", "extra"]).is_err());
        assert!(parse_strs(&["--config-path", "extra"]).is_err());
    }
}
