pub mod alt;
pub mod ansi;
pub mod block;
mod cross_block_search;
pub mod grid;
pub mod kitty_graphics;
pub mod select;
pub mod url;
pub mod vte;

pub use block::BlockTerminal;
pub(crate) use url::open_uri;
pub(crate) use vte::default_tab_title;
pub use vte::{InitialCommands, PaneProbe, VteInit, VteInput, VteOutput, VteTerminal};

pub(crate) const CWD_TOKEN_ENV: &str = "JTERM1_CWD_TOKEN";
const CWD_AUTHORITY_PREFIX: &str = "jterm1-";

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

pub(crate) fn cwd_token_environment(token: &str) -> [(&str, &str); 2] {
    [("TERM_PROGRAM", "jterm1"), (CWD_TOKEN_ENV, token)]
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
        let result = Command::new(program)
            .args(arguments)
            .arg(path)
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
            classify_cwd_authority(Some("jterm1-ffffffff-ffff-4fff-8fff-ffffffffffff"), token),
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
        assert_eq!(
            cwd_token_environment(&first),
            [("TERM_PROGRAM", "jterm1"), (CWD_TOKEN_ENV, first.as_str())]
        );
    }

    #[test]
    fn shell_integrations_share_the_token_contract_and_parse_when_available() {
        let scripts = [
            ("jterm1.bash", "unset JTERM1_CWD_TOKEN"),
            ("jterm1.zsh", "unset JTERM1_CWD_TOKEN"),
            ("jterm1.fish", "set --erase JTERM1_CWD_TOKEN"),
            (
                "jterm1.ps1",
                "Remove-Item Env:JTERM1_CWD_TOKEN -ErrorAction SilentlyContinue",
            ),
        ];
        for (file, removal) in scripts {
            let source = std::fs::read_to_string(integration_path(file)).unwrap();
            assert!(
                source.contains(CWD_TOKEN_ENV),
                "{file} does not read the token"
            );
            assert!(
                source.contains("jterm1-"),
                "{file} does not emit the authenticated authority"
            );
            assert!(
                source.contains(removal),
                "{file} does not remove the exported token"
            );
        }

        let bash = integration_path("jterm1.bash");
        let output = Command::new("bash")
            .arg("-n")
            .arg(&bash)
            .output()
            .expect("bash is required to build jterm1");
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
[[ ! ${JTERM1_CWD_TOKEN+x} ]]
[[ $__jterm1_cwd_token == "$2" ]]
[[ $(export -p) != *JTERM1_CWD_TOKEN* ]]
cwd_sequence=$(__jterm1_report_cwd)
[[ $cwd_sequence == *"7;file://jterm1-$2/"* ]]
"#,
                "jterm1-cwd-token-test",
            ])
            .arg(&bash)
            .arg(bash_token)
            .env(CWD_TOKEN_ENV, bash_token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("bash is required to build jterm1");
        assert!(
            bash_runtime.status.success(),
            "bash integration did not consume and authenticate its token: {}",
            String::from_utf8_lossy(&bash_runtime.stderr)
        );
        check_optional_syntax("zsh", &["-n"], &integration_path("jterm1.zsh"));
        check_optional_syntax("fish", &["-n"], &integration_path("jterm1.fish"));

        let powershell_parser = r#"
$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseFile(
    $args[0], [ref]$tokens, [ref]$errors
) | Out-Null
if ($errors.Count -ne 0) {
    $errors | ForEach-Object { [Console]::Error.WriteLine($_) }
    exit 1
}
"#;
        let powershell_path = integration_path("jterm1.ps1");
        let pwsh_available = Command::new("pwsh")
            .arg("-Version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok();
        if pwsh_available {
            check_optional_syntax(
                "pwsh",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    powershell_parser,
                ],
                &powershell_path,
            );
        } else {
            check_optional_syntax(
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    powershell_parser,
                ],
                &powershell_path,
            );
        }
    }
}
