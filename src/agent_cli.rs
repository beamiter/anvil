//! Child-local CLI defaults: keep interactive conversations in terminal history.
//! Kept identical in the four frontends until the next shared-core release.

use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const CODEX: jterm_core::vendored_script::VendoredScript =
    jterm_core::vendored_script::VendoredScript {
        name: "codex",
        source: r#"#!/bin/sh
# Remove this shim alone, preserving all other PATH entries and their order.
shim_dir=${0%/*}
rest=${PATH-}
clean=
separator=
while :; do
    entry=${rest%%:*}
    if [ "$entry" != "$shim_dir" ]; then
        clean=$clean$separator$entry
        separator=:
    fi
    case $rest in *:*) rest=${rest#*:} ;; *) break ;; esac
done
PATH=$clean
export PATH
program=$(command -v codex) || { printf '%s\n' 'codex: executable not found' >&2; exit 127; }
if [ "${JTERM_AGENT_SCROLLBACK-1}" = 0 ]; then
    exec "$program" "$@"
fi
for arg do
    [ "$arg" != --no-alt-screen ] || exec "$program" "$@"
done
exec "$program" --no-alt-screen "$@"
"#,
    };

fn value<'a>(block: &'a [CString], name: &str) -> Option<&'a [u8]> {
    block.iter().find_map(|entry| {
        let (key, value) = entry.as_bytes().split_at_checked(name.len())?;
        (key == name.as_bytes())
            .then(|| value.strip_prefix(b"="))
            .flatten()
    })
}

fn set(block: &mut Vec<CString>, name: &str, value: &[u8]) {
    let prefix = format!("{name}=");
    block.retain(|entry| !entry.as_bytes().starts_with(prefix.as_bytes()));
    let mut entry = prefix.into_bytes();
    entry.extend_from_slice(value);
    if let Ok(entry) = CString::new(entry) {
        block.push(entry);
    }
}

fn prepare(mut block: Vec<CString>, directory: Option<&Path>) -> Vec<CString> {
    if value(&block, "JTERM_AGENT_SCROLLBACK") == Some(b"0") {
        return block;
    }
    // Presence wins, including an explicit 0 from the user.
    if value(&block, "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN").is_none() {
        set(&mut block, "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", b"1");
    }
    let Some(directory) = directory else {
        return block;
    };
    let Some(path) = value(&block, "PATH") else {
        return block;
    };
    // Do not make an absent CLI appear installed. Also avoid nested shims.
    let paths: Vec<_> = std::env::split_paths(OsStr::from_bytes(path))
        .filter(|entry| entry != directory)
        .collect();
    let Ok(clean_path) = std::env::join_paths(&paths) else {
        return block;
    };
    if jterm_core::host::find_executable_in("codex", Some(&clean_path)).is_none() {
        return block;
    }
    if let Err(error) = CODEX.materialize(directory) {
        log::warn!("could not prepare CLI scrollback integration: {error}");
        return block;
    }
    let Ok(path) = std::env::join_paths(std::iter::once(directory.to_path_buf()).chain(paths))
    else {
        return block;
    };
    set(&mut block, "PATH", path.as_bytes());
    block
}

/// Absolute launchers (task tabs and session resume) bypass PATH shims.
pub(crate) fn inline_argv(argv: &[String]) -> Vec<String> {
    let mut argv = argv.to_vec();
    if std::env::var_os("JTERM_AGENT_SCROLLBACK").as_deref() != Some(OsStr::new("0"))
        && argv
            .first()
            .and_then(|program| Path::new(program).file_name())
            .is_some_and(|name| name == "codex" || name == "codex.js")
        && !argv.iter().any(|arg| arg == "--no-alt-screen")
    {
        argv.insert(1, "--no-alt-screen".into());
    }
    argv
}

/// Run in the parent before fork. Never mutate the terminal process environment.
pub(crate) fn environment(block: Vec<CString>) -> Vec<CString> {
    let directory = dirs::cache_dir().map(|root| root.join("jterm/agent-cli/bin"));
    prepare(block, directory.as_deref())
}

pub(crate) fn vte_environment(block: Vec<String>) -> Vec<String> {
    environment(
        block
            .into_iter()
            .filter_map(|entry| CString::new(entry).ok())
            .collect(),
    )
    .into_iter()
    .filter_map(|entry| entry.into_string().ok())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn defaults_preserve_explicit_choices_and_other_environment() {
        let block = prepare(vec![CString::new("X=unchanged").unwrap()], None);
        assert_eq!(
            value(&block, "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN"),
            Some(b"1".as_slice())
        );
        assert_eq!(value(&block, "X"), Some(b"unchanged".as_slice()));
        let original = vec![CString::new("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=0").unwrap()];
        assert_eq!(prepare(original.clone(), None), original);
        let original = vec![CString::new("JTERM_AGENT_SCROLLBACK=0").unwrap()];
        assert_eq!(prepare(original.clone(), None), original);
    }

    #[test]
    fn codex_new_resume_and_exec_keep_exact_arguments_and_exit_status() {
        let root = std::env::temp_dir().join(format!("agent-cli-{}", uuid::Uuid::new_v4()));
        let bin = root.join("real bin");
        let shim = root.join("shim bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join("codex");
        std::fs::write(&program, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nexit 23\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let block = prepare(
            vec![CString::new(format!("PATH={}", bin.display())).unwrap()],
            Some(&shim),
        );
        assert_eq!(prepare(block.clone(), Some(&shim)), block);
        for args in [
            vec!["中文 prompt; $(false)"],
            vec!["resume", "session-id"],
            vec!["exec", "a b"],
        ] {
            let output = std::process::Command::new(shim.join("codex"))
                .args(&args)
                .env("PATH", OsStr::from_bytes(value(&block, "PATH").unwrap()))
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(23));
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("--no-alt-screen\n{}\n", args.join("\n"))
            );
        }
        let output = std::process::Command::new(shim.join("codex"))
            .arg("--version")
            .env("PATH", OsStr::from_bytes(value(&block, "PATH").unwrap()))
            .env("JTERM_AGENT_SCROLLBACK", "0")
            .output()
            .unwrap();
        assert_eq!(output.stdout, b"--version\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
