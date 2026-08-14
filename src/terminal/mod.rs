pub mod alt;
pub mod ansi;
pub mod block;
pub mod click_cursor;
mod cross_block_search;
pub mod grid;
pub mod kitty_graphics;
mod record_snapshot;
pub mod select;
pub mod url;
pub mod vte;

pub use block::BlockTerminal;
pub(crate) use url::open_uri;
pub(crate) use vte::default_tab_title;
pub use vte::{InitialCommands, PaneProbe, VteInit, VteInput, VteOutput, VteTerminal};

pub(crate) const CWD_TOKEN_ENV: &str = "ANVIL_CWD_TOKEN";
const CWD_AUTHORITY_PREFIX: &str = "anvil-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CwdAuthority {
    /// Emitted by this pane's shell integration with its unexported random
    /// token. This remains trustworthy when a Flatpak host shell hides its
    /// foreground process tree from `/proc`.
    AuthenticatedLocal,
    /// Claims the exact local hostname (or localhost), but carries no secret.
    ClaimedLocal,
    /// Names another host or presents an invalid/mismatched authentication
    /// token.
    External,
    /// The OSC 7 URI omitted its authority.
    Missing,
}

pub(crate) fn new_cwd_token() -> String {
    relm4::gtk::glib::uuid_string_random().to_string()
}

/// The per-pane extras every spawn path adds on top of the shared child
/// environment: only the cwd-authentication token now. `TERM_PROGRAM` used to be
/// spelled out here as well, and `jterm_core::child_env` sets it — with a
/// matching `TERM_PROGRAM_VERSION` — at all three spawn sites, so a second copy
/// would only be a place for the two names to drift apart.
pub(crate) fn cwd_token_environment(token: &str) -> [(&str, &str); 1] {
    [(CWD_TOKEN_ENV, token)]
}

fn authenticated_cwd_authority(token: &str) -> Option<String> {
    (!token.is_empty()).then(|| format!("{CWD_AUTHORITY_PREFIX}{token}"))
}

/// Classify the authority carried by an OSC 7 `file://` URI. A local hostname
/// is only a claim: remote output can spell it too. Only the per-pane token
/// authenticates a local namespace when process inspection is opaque.
pub(crate) fn classify_cwd_authority(host: Option<&str>, expected_token: &str) -> CwdAuthority {
    let Some(host) = host.map(str::trim).filter(|host| !host.is_empty()) else {
        return CwdAuthority::Missing;
    };
    if authenticated_cwd_authority(expected_token)
        .as_deref()
        .is_some_and(|expected| host.eq_ignore_ascii_case(expected))
    {
        return CwdAuthority::AuthenticatedLocal;
    }
    let local = relm4::gtk::glib::host_name();
    // Do not equate arbitrary FQDNs merely because their first label matches.
    // `node.remote` and local `node.local` are distinct authorities; treating
    // them as one would let a remote OSC 7 path drive local filesystem work.
    if host.eq_ignore_ascii_case(local.as_str()) || host.eq_ignore_ascii_case("localhost") {
        CwdAuthority::ClaimedLocal
    } else {
        CwdAuthority::External
    }
}

/// Combine an OSC authority with the live foreground-process namespace.
///
/// An authenticated token or a positively identified local foreground process
/// is required to accept a local cwd. A known external process/authority always
/// wins. This deliberately makes old shell integrations conservative inside a
/// Flatpak host shell, where the host process tree is invisible.
pub(crate) fn resolve_cwd_external(
    authority: CwdAuthority,
    foreground_external: Option<bool>,
) -> bool {
    match (authority, foreground_external) {
        (CwdAuthority::External, _) | (_, Some(true)) => true,
        (CwdAuthority::AuthenticatedLocal, _) | (_, Some(false)) => false,
        (CwdAuthority::ClaimedLocal | CwdAuthority::Missing, None) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        authenticated_cwd_authority, classify_cwd_authority, cwd_token_environment, new_cwd_token,
        resolve_cwd_external, CwdAuthority, CWD_TOKEN_ENV,
    };
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn integration_path(file: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts/shell-integration")
            .join(file)
    }

    fn check_optional_syntax(program: &str, arguments: &[&str], path: &Path) {
        check_optional_syntax_with_path_env(program, arguments, path, None);
    }

    fn check_optional_syntax_with_path_env(
        program: &str,
        arguments: &[&str],
        path: &Path,
        path_environment: Option<&str>,
    ) {
        let mut command = Command::new(program);
        command.args(arguments);
        if let Some(environment) = path_environment {
            command.env(environment, path);
        } else {
            command.arg(path);
        }
        let result = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match result {
            Ok(output) => assert!(
                output.status.success(),
                "{program} rejected {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!(
                "could not syntax-check {} with {program}: {error}",
                path.display()
            ),
        }
    }

    #[test]
    fn cwd_uri_authority_requires_the_exact_pane_token_for_authentication() {
        let token = "01234567-89ab-4cde-8123-0123456789ab";
        let authenticated = authenticated_cwd_authority(token).unwrap();
        assert_eq!(
            classify_cwd_authority(Some(&authenticated), token),
            CwdAuthority::AuthenticatedLocal
        );
        let uri = format!("file://{authenticated}/tmp/project");
        let (_, parsed_host) =
            relm4::gtk::glib::filename_from_uri(&uri).expect("authenticated OSC 7 URI");
        assert_eq!(
            classify_cwd_authority(parsed_host.as_deref(), token),
            CwdAuthority::AuthenticatedLocal
        );
        assert_eq!(
            classify_cwd_authority(Some("anvil-ffffffff-ffff-4fff-8fff-ffffffffffff"), token),
            CwdAuthority::External
        );
        assert_eq!(classify_cwd_authority(None, token), CwdAuthority::Missing);
        assert_eq!(
            classify_cwd_authority(Some("localhost"), token),
            CwdAuthority::ClaimedLocal
        );
        let local = relm4::gtk::glib::host_name();
        assert_eq!(
            classify_cwd_authority(Some(local.as_str()), token),
            CwdAuthority::ClaimedLocal
        );
        assert_eq!(
            classify_cwd_authority(Some("remote-host.invalid"), token),
            CwdAuthority::External
        );
        let local_short = local.split('.').next().unwrap_or(local.as_str());
        assert_eq!(
            classify_cwd_authority(Some(&format!("{local_short}.different.invalid")), token),
            CwdAuthority::External
        );
    }

    #[test]
    fn opaque_foreground_accepts_only_an_authenticated_local_authority() {
        assert!(!resolve_cwd_external(
            CwdAuthority::AuthenticatedLocal,
            None
        ));
        assert!(resolve_cwd_external(CwdAuthority::ClaimedLocal, None));
        assert!(resolve_cwd_external(CwdAuthority::Missing, None));
        assert!(resolve_cwd_external(CwdAuthority::External, Some(false)));
        assert!(resolve_cwd_external(
            CwdAuthority::AuthenticatedLocal,
            Some(true)
        ));
        assert!(!resolve_cwd_external(
            CwdAuthority::ClaimedLocal,
            Some(false)
        ));
        assert!(!resolve_cwd_external(CwdAuthority::Missing, Some(false)));
    }

    #[test]
    fn pane_token_is_random_shaped_and_injected_with_term_program() {
        let first = new_cwd_token();
        let second = new_cwd_token();
        assert_ne!(first, second);
        assert_eq!(first.len(), 36);
        assert!(first.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        }));
        // TERM_PROGRAM/TERM_PROGRAM_VERSION now come from the shared child
        // environment policy, so this carries the token and nothing else.
        assert_eq!(
            cwd_token_environment(&first),
            [(CWD_TOKEN_ENV, first.as_str())]
        );
    }

    /// The shell-integration snippets are sourced from an rc file behind
    /// `[[ $TERM_PROGRAM == anvil ]]`, and `TERM_PROGRAM` now comes from the
    /// shared child-environment policy instead of being spelled out per spawn
    /// site. If the identity this app registers ever stops matching that gate,
    /// every shell silently loses OSC 133 and block mode stops finding commands.
    #[test]
    fn the_child_environment_reports_the_term_program_the_rc_snippets_gate_on() {
        jterm_core::identity::init(jterm_core::identity::AppIdentity {
            app_name: crate::host::APP_NAME,
            app_id: crate::host::APP_ID,
            app_version: env!("CARGO_PKG_VERSION"),
        });
        let overlay = jterm_core::child_env::pairs(
            &jterm_core::child_env::ChildEnv::from_identity(),
            &cwd_token_environment("token"),
        );
        let term_program = overlay
            .iter()
            .find(|(name, _)| name == "TERM_PROGRAM")
            .map(|(_, value)| value.to_string_lossy().to_string());
        assert_eq!(term_program.as_deref(), Some("anvil"));

        for script in ["anvil.bash", "anvil.zsh", "anvil.fish", "anvil.ps1"] {
            let source = std::fs::read_to_string(integration_path(script))
                .unwrap_or_else(|error| panic!("read {script}: {error}"));
            assert!(
                source.contains("anvil"),
                "{script} must still name the TERM_PROGRAM it is gated on"
            );
        }
    }

    #[test]
    fn shell_integrations_share_the_token_contract_and_parse_when_available() {
        let scripts = [
            ("anvil.bash", "unset ANVIL_CWD_TOKEN"),
            ("anvil.zsh", "unset ANVIL_CWD_TOKEN"),
            ("anvil.fish", "set --erase ANVIL_CWD_TOKEN"),
            (
                "anvil.ps1",
                "Remove-Item Env:ANVIL_CWD_TOKEN -ErrorAction SilentlyContinue",
            ),
        ];
        for (file, removal) in scripts {
            let source = std::fs::read_to_string(integration_path(file)).unwrap();
            assert!(
                source.contains(CWD_TOKEN_ENV),
                "{file} does not read the token"
            );
            assert!(
                source.contains("anvil-"),
                "{file} does not emit the authenticated authority"
            );
            assert!(
                source.contains(removal),
                "{file} does not remove the exported token"
            );
            assert!(
                source.contains(";id="),
                "{file} does not correlate OSC 133 C/D with a private id"
            );
            assert!(
                source.contains("__anvil_marker_id"),
                "{file} does not retain a private command marker"
            );
            assert!(
                source.contains("ANVIL_SHELL_INTEGRATION_FD")
                    && source.contains("ANVIL_SHELL_INTEGRATION_TOKEN"),
                "{file} does not scrub the reserved Agent integration environment"
            );
        }

        for file in ["anvil.bash", "anvil.zsh"] {
            let source = std::fs::read_to_string(integration_path(file)).unwrap();
            assert!(source.contains("7771;${__anvil_command_token}"));
            assert!(source.contains("__anvil_token_fd"));
            assert!(source.contains("__anvil_command_token}-${__anvil_marker_seq}"));
        }

        let bash = integration_path("anvil.bash");
        let output = Command::new("bash")
            .arg("-n")
            .arg(&bash)
            .output()
            .expect("bash is required to build anvil");
        assert!(
            output.status.success(),
            "bash rejected {}: {}",
            bash.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let bash_token = "01234567-89ab-4cde-8123-0123456789ab";
        let bash_runtime = Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#"
source "$1"
[[ ! ${ANVIL_CWD_TOKEN+x} ]]
[[ $__anvil_cwd_token == "$2" ]]
[[ $(export -p) != *ANVIL_CWD_TOKEN* ]]
cwd_sequence=$(__anvil_report_cwd)
[[ $cwd_sequence == *"7;file://anvil-$2/"* ]]
"#,
                "anvil-cwd-token-test",
            ])
            .arg(&bash)
            .arg(bash_token)
            .env(CWD_TOKEN_ENV, bash_token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("bash is required to build anvil");
        assert!(
            bash_runtime.status.success(),
            "bash integration did not consume and authenticate its token: {}",
            String::from_utf8_lossy(&bash_runtime.stderr)
        );
        check_optional_syntax("zsh", &["-n"], &integration_path("anvil.zsh"));
        check_optional_syntax("fish", &["-n"], &integration_path("anvil.fish"));

        // `-Command` does not consistently expose trailing native arguments in
        // `$args`, so pass the path through a process-local environment value.
        let powershell_parser = r#"
$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseFile(
    $env:ANVIL_PS1_SYNTAX_PATH, [ref]$tokens, [ref]$errors
) | Out-Null
if ($errors.Count -ne 0) {
    $errors | ForEach-Object { [Console]::Error.WriteLine($_) }
    exit 1
}
"#;
        let powershell_path = integration_path("anvil.ps1");
        let pwsh_available = Command::new("pwsh")
            .arg("-Version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if pwsh_available {
            check_optional_syntax_with_path_env(
                "pwsh",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    powershell_parser,
                ],
                &powershell_path,
                Some("ANVIL_PS1_SYNTAX_PATH"),
            );
        } else {
            check_optional_syntax_with_path_env(
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    powershell_parser,
                ],
                &powershell_path,
                Some("ANVIL_PS1_SYNTAX_PATH"),
            );
        }
    }
}
