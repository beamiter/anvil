//! Fail-closed preflight for re-running a task's source command as validation.
//!
//! This module turns already-owned task evidence into a command, source-shell
//! identity, and descriptor-pinned working directory. It runs only read-only
//! Git identity checks and never starts the validation command or mutates a
//! repository, so every authority check finishes before a terminal is created.

use super::task::AgentTask;
use crate::review_text::{sanitize_history_replay, ReviewTextError, MAX_HISTORY_COMMAND_BYTES};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

const MAX_SOURCE_SHELL_BYTES: usize = 4096;

/// Exact command and canonical worktree directory approved by preflight.
#[derive(Debug)]
pub struct PreparedTaskValidation {
    pub command: String,
    pub cwd: PathBuf,
    pub source_shell: String,
    #[allow(dead_code)]
    pub(crate) pinned_cwd: crate::agent_task::pinned_dir::PinnedDirectory,
}

/// Filesystem location involved in a validation preflight failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskValidationPath {
    RepositoryRoot,
    SourceCwd,
    WorktreeRoot,
    WorktreeCwd,
}

impl fmt::Display for TaskValidationPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RepositoryRoot => "source repository root",
            Self::SourceCwd => "source command cwd",
            Self::WorktreeRoot => "task worktree root",
            Self::WorktreeCwd => "mapped worktree cwd",
        })
    }
}

/// Why a task's source command cannot safely become a validation process.
#[derive(Debug)]
pub enum TaskValidationError {
    MissingSourceContext,
    MissingCommand,
    MissingSourceShell,
    InvalidSourceShell,
    CommandNotExact,
    CommandTruncated,
    EmptyCommand,
    CommandTooLarge {
        limit: usize,
    },
    CommandContainsControlCharacter,
    CommandContainsVisualSpoof,
    MultilineCommand,
    MissingSourceCwd,
    CannotCanonicalize {
        location: TaskValidationPath,
        path: PathBuf,
        source: io::Error,
    },
    NotDirectory {
        location: TaskValidationPath,
        path: PathBuf,
    },
    SourceCwdOutsideRepository {
        cwd: PathBuf,
        repository: PathBuf,
    },
    WorktreeCwdEscapesWorktree {
        cwd: PathBuf,
        worktree: PathBuf,
    },
    RepositoryRootRedirected {
        configured: PathBuf,
        resolved: PathBuf,
    },
    WorktreeRootRedirected {
        configured: PathBuf,
        resolved: PathBuf,
    },
    WorktreeIdentity(String),
}

impl fmt::Display for TaskValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSourceContext => {
                formatter.write_str("task has no source command context to validate")
            }
            Self::MissingCommand => formatter.write_str("task source context has no command"),
            Self::MissingSourceShell => {
                formatter.write_str("task source context has no stable shell identity")
            }
            Self::InvalidSourceShell => formatter.write_str(
                "task source shell identity is not an absolute, bounded, single-line path",
            ),
            Self::CommandNotExact => {
                formatter.write_str("task source command is not exact shell metadata")
            }
            Self::CommandTruncated => {
                formatter.write_str("task source command was truncated or omitted")
            }
            Self::EmptyCommand => formatter.write_str("task source command is empty"),
            Self::CommandTooLarge { limit } => write!(
                formatter,
                "task source command exceeds the {limit}-byte history command limit"
            ),
            Self::CommandContainsControlCharacter => {
                formatter.write_str("task source command contains a terminal control character")
            }
            Self::CommandContainsVisualSpoof => formatter.write_str(
                "task source command contains invisible or bidirectional formatting characters",
            ),
            Self::MultilineCommand => {
                formatter.write_str("task validation requires a single-line source command")
            }
            Self::MissingSourceCwd => {
                formatter.write_str("task source context has no command working directory")
            }
            Self::CannotCanonicalize {
                location,
                path,
                source,
            } => write!(
                formatter,
                "cannot resolve {location} {}: {source}",
                path.display()
            ),
            Self::NotDirectory { location, path } => {
                write!(
                    formatter,
                    "{location} is not a directory: {}",
                    path.display()
                )
            }
            Self::SourceCwdOutsideRepository { cwd, repository } => write!(
                formatter,
                "source command cwd {} is outside repository {}",
                cwd.display(),
                repository.display()
            ),
            Self::WorktreeCwdEscapesWorktree { cwd, worktree } => write!(
                formatter,
                "mapped validation cwd {} escapes worktree {}",
                cwd.display(),
                worktree.display()
            ),
            Self::RepositoryRootRedirected {
                configured,
                resolved,
            } => write!(
                formatter,
                "source repository root {} now resolves to a different path {}",
                configured.display(),
                resolved.display()
            ),
            Self::WorktreeRootRedirected {
                configured,
                resolved,
            } => write!(
                formatter,
                "task worktree root {} now resolves to a different path {}",
                configured.display(),
                resolved.display()
            ),
            Self::WorktreeIdentity(detail) => {
                write!(formatter, "task worktree identity check failed: {detail}")
            }
        }
    }
}

impl std::error::Error for TaskValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CannotCanonicalize { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Validate immutable task evidence and map its source cwd into the worktree.
///
/// The returned command is the sanitized, byte-bounded history command. The
/// returned cwd is canonical and is accompanied by the exact open directory
/// descriptor used for both Git verification and the child's later `fchdir`.
pub fn prepare_task_validation(
    task: &AgentTask,
) -> Result<PreparedTaskValidation, TaskValidationError> {
    let context = task
        .source_context
        .as_ref()
        .ok_or(TaskValidationError::MissingSourceContext)?;

    // Producer-declared truncation is stronger evidence than an absent value:
    // never let an omitted oversized command look like an ordinary gap.
    if context.command_truncated {
        return Err(TaskValidationError::CommandTruncated);
    }
    let raw_command = context
        .command
        .as_deref()
        .ok_or(TaskValidationError::MissingCommand)?;
    if !context.command_exact {
        return Err(TaskValidationError::CommandNotExact);
    }
    let command = sanitize_history_replay(raw_command, MAX_HISTORY_COMMAND_BYTES)
        .map_err(map_command_error)?;
    if raw_command.contains(['\n', '\r']) || command.contains(['\n', '\r']) {
        return Err(TaskValidationError::MultilineCommand);
    }
    // History replay strips some terminal controls for interactive Fill. A
    // validation command is execution-authorizing, so it must never run a
    // cleaned-up spelling that differs from the producer's exact metadata.
    if command != raw_command {
        return Err(TaskValidationError::CommandContainsControlCharacter);
    }
    let source_shell = context
        .source_shell
        .as_deref()
        .filter(|shell| !shell.trim().is_empty())
        .ok_or(TaskValidationError::MissingSourceShell)?
        .to_string();
    if source_shell.len() > MAX_SOURCE_SHELL_BYTES
        || source_shell.chars().any(char::is_control)
        || !Path::new(&source_shell).is_absolute()
    {
        return Err(TaskValidationError::InvalidSourceShell);
    }

    let source_cwd = context
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or(TaskValidationError::MissingSourceCwd)?;
    let repository = canonical_directory(&task.repo_root, TaskValidationPath::RepositoryRoot)?;
    if repository != task.repo_root {
        return Err(TaskValidationError::RepositoryRootRedirected {
            configured: task.repo_root.clone(),
            resolved: repository,
        });
    }
    let source_cwd = canonical_directory(Path::new(source_cwd), TaskValidationPath::SourceCwd)?;
    let relative_cwd = source_cwd.strip_prefix(&repository).map_err(|_| {
        TaskValidationError::SourceCwdOutsideRepository {
            cwd: source_cwd.clone(),
            repository: repository.clone(),
        }
    })?;

    let worktree = canonical_directory(&task.worktree_path, TaskValidationPath::WorktreeRoot)?;
    if worktree != task.worktree_path {
        return Err(TaskValidationError::WorktreeRootRedirected {
            configured: task.worktree_path.clone(),
            resolved: worktree,
        });
    }
    let managed_root = worktree.parent().ok_or_else(|| {
        TaskValidationError::WorktreeIdentity(
            "task worktree has no managed-root parent".to_string(),
        )
    })?;
    let pinned_root = crate::agent_task::pinned_dir::PinnedDirectory::open(&worktree)
        .map_err(|error| TaskValidationError::WorktreeIdentity(error.to_string()))?;
    let pinned_cwd = pinned_root
        .open_beneath(relative_cwd)
        .map_err(|error| TaskValidationError::WorktreeIdentity(error.to_string()))?;
    let pinned_path = pinned_cwd.proc_path();
    let cwd = canonical_path(&pinned_path, TaskValidationPath::WorktreeCwd)?;
    if cwd != worktree && !cwd.starts_with(&worktree) {
        return Err(TaskValidationError::WorktreeCwdEscapesWorktree { cwd, worktree });
    }
    if !cwd.is_dir() {
        return Err(TaskValidationError::NotDirectory {
            location: TaskValidationPath::WorktreeCwd,
            path: cwd,
        });
    }
    super::worktree::WorktreeService::new(managed_root)
        .and_then(|service| {
            service.verify_active_task_worktree_through(
                &repository,
                &worktree,
                &pinned_path,
                &task.branch,
            )
        })
        .map_err(|error| TaskValidationError::WorktreeIdentity(error.to_string()))?;

    Ok(PreparedTaskValidation {
        command,
        cwd,
        source_shell,
        pinned_cwd,
    })
}

fn map_command_error(error: ReviewTextError) -> TaskValidationError {
    match error {
        ReviewTextError::Empty => TaskValidationError::EmptyCommand,
        ReviewTextError::TooLarge { limit } => TaskValidationError::CommandTooLarge { limit },
        ReviewTextError::ControlCharacter => TaskValidationError::CommandContainsControlCharacter,
        ReviewTextError::VisualSpoof => TaskValidationError::CommandContainsVisualSpoof,
    }
}

fn canonical_directory(
    path: &Path,
    location: TaskValidationPath,
) -> Result<PathBuf, TaskValidationError> {
    let canonical = canonical_path(path, location)?;
    if !canonical.is_dir() {
        return Err(TaskValidationError::NotDirectory {
            location,
            path: canonical,
        });
    }
    Ok(canonical)
}

fn canonical_path(
    path: &Path,
    location: TaskValidationPath,
) -> Result<PathBuf, TaskValidationError> {
    std::fs::canonicalize(path).map_err(|source| TaskValidationError::CannotCanonicalize {
        location,
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::context::SemanticCommandContext;
    use crate::agent_task::task::{
        AgentProvider, TaskId, TaskRuntimeKind, TaskStatus, TaskValidationState,
    };
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "anvil-validation-{label}-{}-{nanos:x}-{sequence:x}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create validation test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn validation_task(repository: PathBuf, worktree: PathBuf, cwd: PathBuf) -> AgentTask {
        AgentTask {
            id: TaskId::new(),
            title: "validation fixture".to_string(),
            provider: AgentProvider::Codex,
            status: TaskStatus::ReadyForReview,
            repo_root: repository,
            worktree_path: worktree,
            branch: "anvil/validation-fixture".to_string(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            source: None,
            source_context: Some(SemanticCommandContext {
                source_session_id: "source-session".to_string(),
                source_execution_id: "execution-1".to_string(),
                source_sequence: 1,
                source_shell: Some("/bin/bash".to_string()),
                command: Some("cargo test".to_string()),
                command_exact: true,
                command_truncated: false,
                cwd: Some(cwd.to_string_lossy().into_owned()),
                cwd_after: None,
                exit_code: Some(1),
                duration_ms: None,
                output_text: String::new(),
                output_available: true,
                output_truncated: false,
                output_total_bytes: 0,
                started_at: None,
                finished_at: None,
            }),
            runtime_kind: TaskRuntimeKind::Unassigned,
            terminal_session_id: None,
            validation: TaskValidationState::default(),
            exit_code: None,
            status_detail: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn repository_and_worktree(label: &str) -> (TestDirectory, PathBuf, PathBuf) {
        let root = TestDirectory::new(label);
        let repository = root.0.join("repository");
        fs::create_dir(&repository).unwrap();
        let git = |arguments: &[&str]| {
            let output = Command::new("/usr/bin/git")
                .current_dir(&repository)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "user.name", "Anvil Validation Test"]);
        git(&["config", "user.email", "validation@example.invalid"]);
        fs::write(repository.join("tracked"), b"baseline\n").unwrap();
        git(&["add", "--", "tracked"]);
        git(&["commit", "--quiet", "-m", "baseline"]);

        let service = crate::agent_task::WorktreeService::new(root.0.join("managed")).unwrap();
        let repository = service.resolve_repository_root(&repository).unwrap();
        let managed = service
            .create(&crate::agent_task::CreateWorktreeRequest::new(
                &repository,
                "validation-fixture",
                "anvil/validation-fixture",
                "HEAD",
            ))
            .unwrap();
        (root, repository, managed.path)
    }

    #[test]
    fn prepares_exact_command_at_canonical_mapped_subdirectory() {
        let (_root, repository, worktree) = repository_and_worktree("mapped-subdirectory");
        let source_cwd = repository.join("crates/core");
        let worktree_cwd = worktree.join("crates/core");
        fs::create_dir_all(&source_cwd).unwrap();
        fs::create_dir_all(&worktree_cwd).unwrap();
        let task = validation_task(repository, worktree, source_cwd);

        let prepared = prepare_task_validation(&task).unwrap();

        assert_eq!(prepared.command, "cargo test");
        assert_eq!(prepared.cwd, fs::canonicalize(worktree_cwd).unwrap());
        assert_eq!(prepared.source_shell, "/bin/bash");
    }

    #[test]
    fn rejects_missing_context_and_command_provenance_failures() {
        let (_root, repository, worktree) = repository_and_worktree("provenance");
        let mut missing_context_task =
            validation_task(repository.clone(), worktree.clone(), repository);
        missing_context_task.source_context = None;
        assert!(matches!(
            prepare_task_validation(&missing_context_task),
            Err(TaskValidationError::MissingSourceContext)
        ));

        let mut task = validation_task(worktree.clone(), worktree.clone(), worktree);
        let context = task.source_context.as_mut().unwrap();
        context.source_shell = None;
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::MissingSourceShell)
        ));
        let context = task.source_context.as_mut().unwrap();
        context.source_shell = Some("bash".to_string());
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::InvalidSourceShell)
        ));
        let context = task.source_context.as_mut().unwrap();
        context.source_shell = Some("/bin/bash".to_string());
        context.command = None;
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::MissingCommand)
        ));
        let context = task.source_context.as_mut().unwrap();
        context.command = Some("cargo test".to_string());
        context.command_exact = false;
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::CommandNotExact)
        ));
        let context = task.source_context.as_mut().unwrap();
        context.command_exact = true;
        context.command_truncated = true;
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::CommandTruncated)
        ));
        let context = task.source_context.as_mut().unwrap();
        context.command_truncated = false;
        context.cwd = None;
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::MissingSourceCwd)
        ));
    }

    #[test]
    fn rejects_empty_multiline_oversized_and_spoofed_commands() {
        let (_root, repository, worktree) = repository_and_worktree("unsafe-command");
        let mut task = validation_task(repository.clone(), worktree, repository);

        task.source_context.as_mut().unwrap().command = Some(" \t ".to_string());
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::EmptyCommand)
        ));

        task.source_context.as_mut().unwrap().command = Some("cargo test\r\necho done".to_string());
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::MultilineCommand)
        ));

        task.source_context.as_mut().unwrap().command = Some("cargo\u{0} test".to_string());
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::CommandContainsControlCharacter)
        ));

        task.source_context.as_mut().unwrap().command =
            Some("x".repeat(MAX_HISTORY_COMMAND_BYTES + 1));
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::CommandTooLarge {
                limit: MAX_HISTORY_COMMAND_BYTES
            })
        ));

        task.source_context.as_mut().unwrap().command = Some("echo safe\u{202e}hidden".to_string());
        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::CommandContainsVisualSpoof)
        ));
    }

    #[test]
    fn rejects_source_cwd_outside_repository() {
        let (root, repository, worktree) = repository_and_worktree("outside-source");
        let outside = root.0.join("outside");
        fs::create_dir(&outside).unwrap();
        let task = validation_task(repository, worktree, outside);

        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::SourceCwdOutsideRepository { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_worktree_symlink_escape() {
        use std::os::unix::fs::symlink;

        let (root, repository, worktree) = repository_and_worktree("symlink-escape");
        let source_cwd = repository.join("nested");
        let outside = root.0.join("outside");
        fs::create_dir(&source_cwd).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, worktree.join("nested")).unwrap();
        let task = validation_task(repository, worktree, source_cwd);

        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::WorktreeIdentity(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_replaced_worktree_root_symlink() {
        use std::os::unix::fs::symlink;

        let (root, repository, worktree) = repository_and_worktree("root-symlink-escape");
        let outside = root.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::remove_dir_all(&worktree).unwrap();
        symlink(&outside, &worktree).unwrap();
        let task = validation_task(repository.clone(), worktree.clone(), repository);

        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::WorktreeRootRedirected {
                configured,
                resolved,
            }) if configured == worktree && resolved == fs::canonicalize(outside).unwrap()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_replaced_repository_root_symlink() {
        use std::os::unix::fs::symlink;

        let (root, repository, worktree) = repository_and_worktree("repository-symlink-escape");
        let outside = root.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::remove_dir_all(&repository).unwrap();
        symlink(&outside, &repository).unwrap();
        let task = validation_task(repository.clone(), worktree, repository.clone());

        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::RepositoryRootRedirected {
                configured,
                resolved,
            }) if configured == repository && resolved == fs::canonicalize(outside).unwrap()
        ));
    }

    #[test]
    fn rejects_non_directory_repository_source_and_worktree_paths() {
        let (root, repository, worktree) = repository_and_worktree("not-directory");
        let file = root.0.join("file");
        fs::write(&file, b"not a directory").unwrap();

        let repository_file_task =
            validation_task(file.clone(), worktree.clone(), repository.clone());
        assert!(matches!(
            prepare_task_validation(&repository_file_task),
            Err(TaskValidationError::NotDirectory {
                location: TaskValidationPath::RepositoryRoot,
                ..
            })
        ));

        let source_file_task = validation_task(repository.clone(), worktree.clone(), file.clone());
        assert!(matches!(
            prepare_task_validation(&source_file_task),
            Err(TaskValidationError::NotDirectory {
                location: TaskValidationPath::SourceCwd,
                ..
            })
        ));

        let worktree_file_task = validation_task(repository.clone(), file, repository);
        assert!(matches!(
            prepare_task_validation(&worktree_file_task),
            Err(TaskValidationError::NotDirectory {
                location: TaskValidationPath::WorktreeRoot,
                ..
            })
        ));
    }

    #[test]
    fn rejects_an_ordinary_directory_replacing_the_registered_worktree() {
        let (_root, repository, worktree) = repository_and_worktree("replaced-directory");
        fs::remove_dir_all(&worktree).unwrap();
        fs::create_dir(&worktree).unwrap();
        let task = validation_task(repository.clone(), worktree, repository);

        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::WorktreeIdentity(_))
        ));
    }

    #[test]
    fn rejects_a_registered_worktree_whose_branch_identity_changed() {
        let (_root, repository, worktree) = repository_and_worktree("changed-branch");
        let output = Command::new("/usr/bin/git")
            .current_dir(&worktree)
            .args(["switch", "--quiet", "-c", "anvil/replacement"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let task = validation_task(repository.clone(), worktree, repository);

        assert!(matches!(
            prepare_task_validation(&task),
            Err(TaskValidationError::WorktreeIdentity(_))
        ));
    }
}
