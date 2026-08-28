//! Compatibility launcher for opaque Agent CLIs hosted in ordinary PTYs.
//!
//! Native provider protocols will eventually emit structured `AgentEvent`
//! values.  This launcher is the P0 bridge: it resolves a provider executable
//! before PTY creation, passes an exact argv (never a shell command string),
//! and relies on `SessionManager` to start it in the task worktree.

use super::AgentProvider;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Enough room for a conventional shebang while keeping launcher inspection
/// independent of the size of a provider-controlled installation artifact.
const NATIVE_LAUNCHER_PREFIX_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLaunchSpec {
    pub provider: AgentProvider,
    pub executable: PathBuf,
    /// Exact argv for the legacy PTY path. The kernel may interpret a shebang
    /// here because this path retains the user's ordinary login environment.
    pub argv: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentLaunchError {
    RepositoryMustBeAbsolute,
    RepositoryUnavailable(PathBuf),
    WorktreeMustBeAbsolute,
    WorktreeUnavailable(PathBuf),
    WorktreeNotUtf8,
    ExecutableInsideRepository(PathBuf),
    ExecutablePathNotUtf8(PathBuf),
    NativeInterpreterInsideRepository(PathBuf),
    NativeInterpreterPathNotUtf8(PathBuf),
    UntrustedNativeExecutable {
        path: PathBuf,
        detail: String,
    },
    UntrustedNativeInterpreter {
        path: PathBuf,
        detail: String,
    },
    UnsupportedCodexLauncher(PathBuf),
    ExecutableUnavailable {
        provider: AgentProvider,
        detail: String,
    },
}

impl fmt::Display for AgentLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepositoryMustBeAbsolute => {
                formatter.write_str("task repository must be absolute")
            }
            Self::RepositoryUnavailable(path) => write!(
                formatter,
                "task repository is unavailable or not canonical: {}",
                path.display()
            ),
            Self::WorktreeMustBeAbsolute => formatter.write_str("task worktree must be absolute"),
            Self::WorktreeUnavailable(path) => {
                write!(
                    formatter,
                    "task worktree is not a directory: {}",
                    path.display()
                )
            }
            Self::WorktreeNotUtf8 => formatter.write_str("task worktree path is not valid UTF-8"),
            Self::ExecutableInsideRepository(path) => write!(
                formatter,
                "refusing to run a repository-controlled Agent executable: {}",
                path.display()
            ),
            Self::ExecutablePathNotUtf8(path) => write!(
                formatter,
                "Agent executable path is not valid UTF-8: {}",
                path.display()
            ),
            Self::NativeInterpreterInsideRepository(path) => write!(
                formatter,
                "refusing to run a repository-controlled Agent interpreter: {}",
                path.display()
            ),
            Self::NativeInterpreterPathNotUtf8(path) => write!(
                formatter,
                "Agent interpreter path is not valid UTF-8: {}",
                path.display()
            ),
            Self::UntrustedNativeExecutable { path, detail } => write!(
                formatter,
                "refusing untrusted native Agent executable {}: {detail}",
                path.display()
            ),
            Self::UntrustedNativeInterpreter { path, detail } => write!(
                formatter,
                "refusing untrusted native Agent interpreter {}: {detail}",
                path.display()
            ),
            Self::UnsupportedCodexLauncher(path) => write!(
                formatter,
                "Codex launcher must be an ELF executable or use '#!/usr/bin/env node': {}",
                path.display()
            ),
            Self::ExecutableUnavailable { provider, detail } => write!(
                formatter,
                "{} is not available: {detail}",
                provider.display_name()
            ),
        }
    }
}

impl std::error::Error for AgentLaunchError {}

impl AgentLaunchSpec {
    /// Resolve the selected provider using the process PATH and pin the exact
    /// absolute executable path into argv before any PTY is created.
    pub fn resolve(
        provider: AgentProvider,
        repository: &Path,
        worktree: &Path,
    ) -> Result<Self, AgentLaunchError> {
        Self::resolve_with_path(
            provider,
            repository,
            worktree,
            std::env::var_os("PATH").as_deref(),
        )
    }

    /// Resolve exact argv for the native Codex path. Script interpreters are
    /// pinned up front so the deliberately minimal provider environment never
    /// needs to search `PATH`. This stricter API is intentionally separate
    /// from [`Self::resolve`], preserving compatibility for opaque PTY
    /// launchers with other valid shebangs.
    pub(crate) fn resolve_native(
        provider: AgentProvider,
        repository: &Path,
        worktree: &Path,
    ) -> Result<Vec<String>, AgentLaunchError> {
        Self::resolve_native_with_path(
            provider,
            repository,
            worktree,
            std::env::var_os("PATH").as_deref(),
        )
    }

    fn resolve_native_with_path(
        provider: AgentProvider,
        repository: &Path,
        worktree: &Path,
        path: Option<&OsStr>,
    ) -> Result<Vec<String>, AgentLaunchError> {
        let launch = Self::resolve_with_path(provider, repository, worktree, path)?;
        resolve_native_argv(
            provider,
            repository,
            worktree,
            &launch.executable,
            &launch.argv[0],
            path,
        )
    }

    fn resolve_with_path(
        provider: AgentProvider,
        repository: &Path,
        worktree: &Path,
        path: Option<&OsStr>,
    ) -> Result<Self, AgentLaunchError> {
        if !repository.is_absolute() {
            return Err(AgentLaunchError::RepositoryMustBeAbsolute);
        }
        let repository = std::fs::canonicalize(repository)
            .ok()
            .filter(|resolved| resolved == repository && resolved.is_dir())
            .ok_or_else(|| AgentLaunchError::RepositoryUnavailable(repository.to_path_buf()))?;
        if !worktree.is_absolute() {
            return Err(AgentLaunchError::WorktreeMustBeAbsolute);
        }
        let worktree = std::fs::canonicalize(worktree)
            .ok()
            .filter(|resolved| resolved == worktree && resolved.is_dir())
            .ok_or_else(|| AgentLaunchError::WorktreeUnavailable(worktree.to_path_buf()))?;
        worktree.to_str().ok_or(AgentLaunchError::WorktreeNotUtf8)?;
        let program = provider.executable_name();
        // A task worktree is repository-controlled. Never apply execvp's
        // relative/empty PATH semantics against it: PATH=".:..." must not let
        // a checkout replace the Agent binary Anvil launches. The shared host
        // helper searches absolute PATH entries only and returns an absolute,
        // executable file.
        let executable = jterm_core::host::find_executable_in(program, path).ok_or_else(|| {
            AgentLaunchError::ExecutableUnavailable {
                provider,
                detail: "executable was not found in an absolute PATH directory".to_string(),
            }
        })?;
        let executable = std::fs::canonicalize(&executable).map_err(|error| {
            AgentLaunchError::ExecutableUnavailable {
                provider,
                detail: format!("cannot resolve executable: {error}"),
            }
        })?;
        if executable.starts_with(&repository) || executable.starts_with(&worktree) {
            return Err(AgentLaunchError::ExecutableInsideRepository(executable));
        }
        let executable_arg = executable
            .to_str()
            .ok_or_else(|| AgentLaunchError::ExecutablePathNotUtf8(executable.clone()))?
            .to_string();
        Ok(Self {
            provider,
            executable,
            argv: vec![executable_arg],
        })
    }
}

fn resolve_native_argv(
    provider: AgentProvider,
    repository: &Path,
    worktree: &Path,
    executable: &Path,
    executable_arg: &str,
    path: Option<&OsStr>,
) -> Result<Vec<String>, AgentLaunchError> {
    validate_native_launch_artifact(executable).map_err(|detail| {
        AgentLaunchError::UntrustedNativeExecutable {
            path: executable.to_path_buf(),
            detail,
        }
    })?;
    if provider != AgentProvider::Codex {
        return Ok(vec![executable_arg.to_string()]);
    }

    let prefix = read_bounded_prefix(executable).map_err(|error| {
        AgentLaunchError::ExecutableUnavailable {
            provider,
            detail: format!("cannot inspect native launcher: {error}"),
        }
    })?;
    if prefix.starts_with(b"\x7fELF") {
        return Ok(vec![executable_arg.to_string()]);
    }
    if !has_env_node_shebang(&prefix) {
        return Err(AgentLaunchError::UnsupportedCodexLauncher(
            executable.to_path_buf(),
        ));
    }

    let interpreter = jterm_core::host::find_executable_in("node", path).ok_or_else(|| {
        AgentLaunchError::ExecutableUnavailable {
            provider,
            detail: "the native Codex launcher requires node, but it was not found in an absolute PATH directory"
                .to_string(),
        }
    })?;
    let interpreter = std::fs::canonicalize(&interpreter).map_err(|error| {
        AgentLaunchError::ExecutableUnavailable {
            provider,
            detail: format!("cannot resolve native Codex interpreter: {error}"),
        }
    })?;
    if interpreter.starts_with(repository) || interpreter.starts_with(worktree) {
        return Err(AgentLaunchError::NativeInterpreterInsideRepository(
            interpreter,
        ));
    }
    validate_native_launch_artifact(&interpreter).map_err(|detail| {
        AgentLaunchError::UntrustedNativeInterpreter {
            path: interpreter.clone(),
            detail,
        }
    })?;
    let interpreter_arg = interpreter
        .to_str()
        .ok_or_else(|| AgentLaunchError::NativeInterpreterPathNotUtf8(interpreter.clone()))?
        .to_string();
    Ok(vec![interpreter_arg, executable_arg.to_string()])
}

/// Validate the canonical file and every directory that can replace it before
/// native startup receives the user's ChatGPT access grant. The ordinary PTY
/// compatibility path deliberately keeps its broader launcher behavior; this
/// stricter gate applies only to native providers.
#[cfg(unix)]
fn validate_native_launch_artifact(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot inspect canonical target: {error}"))?;
    if !metadata.is_file() {
        return Err("canonical target is not a regular file".into());
    }
    let mode = metadata.mode();
    if mode & 0o111 == 0 {
        return Err("canonical target has no executable bit".into());
    }
    if metadata.nlink() != 1 {
        return Err("canonical target must have exactly one hard link".into());
    }
    if mode & (libc::S_ISUID | libc::S_ISGID) != 0 {
        return Err("canonical target must not be setuid or setgid".into());
    }
    validate_native_owner_and_mode(path, &metadata, false)?;

    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
        let metadata = std::fs::metadata(directory).map_err(|error| {
            format!(
                "cannot inspect canonical parent {}: {error}",
                directory.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "canonical parent {} is not a directory",
                directory.display()
            ));
        }
        validate_native_owner_and_mode(directory, &metadata, true)?;
        ancestor = directory.parent();
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_native_launch_artifact(_path: &Path) -> Result<(), String> {
    Err("native Agent launcher trust checks require Unix metadata".into())
}

#[cfg(unix)]
fn validate_native_owner_and_mode(
    path: &Path,
    metadata: &std::fs::Metadata,
    allow_sticky_tmp: bool,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: geteuid/getegid have no preconditions and only read process
    // credentials.
    let current_user = unsafe { libc::geteuid() };
    let current_group = unsafe { libc::getegid() };
    let uid = metadata.uid();
    let gid = metadata.gid();
    let mode = metadata.mode();
    if uid != 0 && uid != current_user {
        return Err(format!(
            "{} is owned by uid {uid}, not root or the current user",
            path.display()
        ));
    }

    let root_sticky_tmp =
        allow_sticky_tmp && path == Path::new("/tmp") && uid == 0 && mode & libc::S_ISVTX != 0;
    if mode & 0o002 != 0 && !root_sticky_tmp {
        return Err(format!("{} is world-writable", path.display()));
    }
    if mode & 0o020 != 0 && !root_sticky_tmp {
        // User-local npm/nvm installations commonly use 0775 throughout.
        // Permit that only for the current user's own primary-group tree;
        // root-owned or unrelated group-writable paths remain untrusted.
        let private_primary_group = uid == current_user && gid == current_group;
        if !private_primary_group {
            return Err(format!(
                "{} is writable by an untrusted group",
                path.display()
            ));
        }
    }
    Ok(())
}

fn read_bounded_prefix(path: &Path) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut prefix = Vec::with_capacity(NATIVE_LAUNCHER_PREFIX_MAX_BYTES + 1);
    file.take((NATIVE_LAUNCHER_PREFIX_MAX_BYTES + 1) as u64)
        .read_to_end(&mut prefix)?;
    Ok(prefix)
}

fn has_env_node_shebang(prefix: &[u8]) -> bool {
    let first_line_end = prefix.iter().position(|byte| *byte == b'\n');
    if first_line_end.is_none() && prefix.len() > NATIVE_LAUNCHER_PREFIX_MAX_BYTES {
        return false;
    }
    let mut line = &prefix[..first_line_end.unwrap_or(prefix.len())];
    if let Some(without_carriage_return) = line.strip_suffix(b"\r") {
        line = without_carriage_return;
    }
    let Some(arguments) = line.strip_prefix(b"#!") else {
        return false;
    };
    let tokens: Vec<_> = arguments
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|token| !token.is_empty())
        .collect();
    tokens.as_slice() == [b"/usr/bin/env".as_slice(), b"node".as_slice()]
}

impl AgentProvider {
    pub fn executable_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "anvil-agent-launcher-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn provider_names_are_explicit_and_stable() {
        assert_eq!(AgentProvider::Codex.executable_name(), "codex");
        assert_eq!(AgentProvider::Claude.executable_name(), "claude");
        assert_eq!(AgentProvider::OpenCode.executable_name(), "opencode");
    }

    #[test]
    fn resolves_exact_executable_without_building_a_shell_command() {
        let root = TempDir::new("resolve");
        let bin = root.0.join("bin");
        let repository = root.0.join("repository");
        let worktree = root.0.join("worktree");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&worktree).unwrap();
        let codex = bin.join("codex");
        fs::write(&codex, b"\x7fELF native fixture").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();

        let spec = AgentLaunchSpec::resolve_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(bin.as_os_str()),
        )
        .unwrap();

        assert!(spec.executable.is_absolute());
        assert_eq!(spec.executable, codex);
        assert_eq!(spec.argv, vec![codex.to_string_lossy().into_owned()]);
        assert_eq!(
            AgentLaunchSpec::resolve_native_with_path(
                AgentProvider::Codex,
                &repository,
                &worktree,
                Some(bin.as_os_str()),
            )
            .unwrap(),
            vec![codex.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn native_codex_js_pins_node_without_relying_on_child_path() {
        let root = TempDir::new("native-node");
        let bin = root.0.join("bin");
        let repository = root.0.join("repository");
        let worktree = root.0.join("worktree");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&worktree).unwrap();
        let codex = bin.join("codex");
        let node = bin.join("node");
        fs::write(&codex, b"#!/usr/bin/env node\nconsole.log('codex');\n").unwrap();
        fs::write(&node, b"\x7fELF node fixture").unwrap();
        for executable in [&codex, &node] {
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let legacy = AgentLaunchSpec::resolve_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(bin.as_os_str()),
        )
        .unwrap();
        assert_eq!(legacy.executable, codex);
        assert_eq!(
            AgentLaunchSpec::resolve_native_with_path(
                AgentProvider::Codex,
                &repository,
                &worktree,
                Some(bin.as_os_str()),
            )
            .unwrap(),
            vec![
                node.to_string_lossy().into_owned(),
                codex.to_string_lossy().into_owned()
            ]
        );
    }

    #[test]
    fn native_codex_rejects_untrusted_executable_metadata_but_allows_private_group_write() {
        let root = TempDir::new("native-metadata");
        let bin = root.0.join("bin");
        let repository = root.0.join("repository");
        let worktree = root.0.join("worktree");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&worktree).unwrap();
        let codex = bin.join("codex");
        fs::write(&codex, b"\x7fELF native fixture").unwrap();

        fs::set_permissions(&codex, fs::Permissions::from_mode(0o707)).unwrap();
        assert!(matches!(
            AgentLaunchSpec::resolve_native_with_path(
                AgentProvider::Codex,
                &repository,
                &worktree,
                Some(bin.as_os_str()),
            ),
            Err(AgentLaunchError::UntrustedNativeExecutable { path, .. }) if path == codex
        ));

        // npm/nvm installations commonly use a private-user-group 0775
        // layout. Keep that real installation shape compatible.
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(AgentLaunchSpec::resolve_native_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(bin.as_os_str()),
        )
        .is_ok());

        let alias = bin.join("codex-alias");
        fs::hard_link(&codex, &alias).unwrap();
        assert!(matches!(
            AgentLaunchSpec::resolve_native_with_path(
                AgentProvider::Codex,
                &repository,
                &worktree,
                Some(bin.as_os_str()),
            ),
            Err(AgentLaunchError::UntrustedNativeExecutable { path, .. }) if path == codex
        ));
    }

    #[test]
    fn native_codex_rejects_untrusted_node_and_replaceable_parent() {
        let root = TempDir::new("native-node-metadata");
        let bin = root.0.join("bin");
        let repository = root.0.join("repository");
        let worktree = root.0.join("worktree");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&worktree).unwrap();
        let codex = bin.join("codex");
        let node = bin.join("node");
        fs::write(&codex, b"#!/usr/bin/env node\n").unwrap();
        fs::write(&node, b"\x7fELF node fixture").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&node, fs::Permissions::from_mode(0o707)).unwrap();

        assert!(matches!(
            AgentLaunchSpec::resolve_native_with_path(
                AgentProvider::Codex,
                &repository,
                &worktree,
                Some(bin.as_os_str()),
            ),
            Err(AgentLaunchError::UntrustedNativeInterpreter { path, .. }) if path == node
        ));

        fs::set_permissions(&node, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o707)).unwrap();
        assert!(matches!(
            AgentLaunchSpec::resolve_native_with_path(
                AgentProvider::Codex,
                &repository,
                &worktree,
                Some(bin.as_os_str()),
            ),
            Err(AgentLaunchError::UntrustedNativeExecutable { path, .. }) if path == codex
        ));
    }

    #[test]
    fn missing_provider_and_invalid_worktree_fail_before_pty_spawn() {
        let root = TempDir::new("missing");
        let repository = root.0.join("repository");
        fs::create_dir(&repository).unwrap();
        let missing_worktree = root.0.join("missing-worktree");
        assert!(matches!(
            AgentLaunchSpec::resolve_with_path(
                AgentProvider::Claude,
                &repository,
                &missing_worktree,
                Some(root.0.as_os_str())
            ),
            Err(AgentLaunchError::WorktreeUnavailable(_))
        ));

        assert!(matches!(
            AgentLaunchSpec::resolve_with_path(
                AgentProvider::Claude,
                &repository,
                &root.0,
                Some(root.0.as_os_str())
            ),
            Err(AgentLaunchError::ExecutableUnavailable { .. })
        ));
    }

    #[test]
    fn repository_cannot_hijack_agent_through_relative_path_entries() {
        let root = TempDir::new("path-hijack");
        let trusted_bin = root.0.join("bin");
        let repository = root.0.join("repository");
        let worktree = root.0.join("worktree");
        fs::create_dir(&trusted_bin).unwrap();
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&worktree).unwrap();

        let trusted_codex = trusted_bin.join("codex");
        let repository_codex = worktree.join("codex");
        for executable in [&trusted_codex, &repository_codex] {
            fs::write(executable, b"\x7fELF native fixture").unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = std::ffi::OsString::from(format!(":.:{}", trusted_bin.display()));

        let spec = AgentLaunchSpec::resolve_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(path.as_os_str()),
        )
        .unwrap();

        assert_eq!(spec.executable, trusted_codex);
        assert_ne!(spec.executable, repository_codex);
    }

    #[test]
    fn absolute_or_symlinked_repository_executable_is_rejected() {
        let root = TempDir::new("absolute-repository-path");
        let repository = root.0.join("repository");
        let repository_bin = repository.join("bin");
        let link_bin = root.0.join("link-bin");
        let worktree = root.0.join("worktree");
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&repository_bin).unwrap();
        fs::create_dir(&link_bin).unwrap();
        fs::create_dir(&worktree).unwrap();
        let repository_codex = repository_bin.join("codex");
        fs::write(&repository_codex, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&repository_codex, fs::Permissions::from_mode(0o700)).unwrap();

        for candidate_dir in [&repository_bin, &link_bin] {
            let linked = candidate_dir == &link_bin;
            if linked {
                symlink(&repository_codex, link_bin.join("codex")).unwrap();
            }
            let error = AgentLaunchSpec::resolve_with_path(
                AgentProvider::Codex,
                &repository,
                &worktree,
                Some(candidate_dir.as_os_str()),
            )
            .unwrap_err();
            assert_eq!(
                error,
                AgentLaunchError::ExecutableInsideRepository(repository_codex.clone())
            );
        }
    }

    #[test]
    fn native_codex_rejects_repository_controlled_node() {
        let root = TempDir::new("repository-node");
        let trusted_bin = root.0.join("trusted-bin");
        let repository = root.0.join("repository");
        let worktree = root.0.join("worktree");
        fs::create_dir(&trusted_bin).unwrap();
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&worktree).unwrap();
        let codex = trusted_bin.join("codex");
        let node = worktree.join("node");
        fs::write(&codex, b"#!/usr/bin/env node\n").unwrap();
        fs::write(&node, b"\x7fELF node fixture").unwrap();
        for executable in [&codex, &node] {
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path =
            std::ffi::OsString::from(format!("{}:{}", trusted_bin.display(), worktree.display()));

        let error = AgentLaunchSpec::resolve_native_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(path.as_os_str()),
        )
        .unwrap_err();

        assert_eq!(
            error,
            AgentLaunchError::NativeInterpreterInsideRepository(node)
        );
    }

    #[test]
    fn native_codex_rejects_unsupported_or_unbounded_shebangs() {
        let root = TempDir::new("unsupported-shebang");
        let bin = root.0.join("bin");
        let repository = root.0.join("repository");
        let worktree = root.0.join("worktree");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&repository).unwrap();
        fs::create_dir(&worktree).unwrap();
        let codex = bin.join("codex");
        fs::write(&codex, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();

        let legacy = AgentLaunchSpec::resolve_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(bin.as_os_str()),
        )
        .unwrap();
        assert_eq!(legacy.argv, vec![codex.to_string_lossy().into_owned()]);

        let unsupported = AgentLaunchSpec::resolve_native_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(bin.as_os_str()),
        )
        .unwrap_err();
        assert_eq!(
            unsupported,
            AgentLaunchError::UnsupportedCodexLauncher(codex.clone())
        );

        let mut long_shebang = b"#!/usr/bin/env ".to_vec();
        long_shebang.extend(std::iter::repeat_n(b'x', NATIVE_LAUNCHER_PREFIX_MAX_BYTES));
        long_shebang.extend_from_slice(b" node\n");
        fs::write(&codex, long_shebang).unwrap();
        let unbounded = AgentLaunchSpec::resolve_native_with_path(
            AgentProvider::Codex,
            &repository,
            &worktree,
            Some(bin.as_os_str()),
        )
        .unwrap_err();
        assert_eq!(unbounded, AgentLaunchError::UnsupportedCodexLauncher(codex));
    }

    #[test]
    fn env_node_shebang_parser_is_exact_and_bounded() {
        assert!(has_env_node_shebang(b"#!/usr/bin/env node\nrest"));
        assert!(has_env_node_shebang(b"#! /usr/bin/env\tnode\r\n"));
        assert!(!has_env_node_shebang(b"#!/usr/bin/env node --flag\n"));
        assert!(!has_env_node_shebang(b"#!/usr/bin/env -S node\n"));
        assert!(!has_env_node_shebang(b"#!/bin/node\n"));
        assert!(!has_env_node_shebang(&vec![
            b'x';
            NATIVE_LAUNCHER_PREFIX_MAX_BYTES
                + 1
        ]));
    }
}
