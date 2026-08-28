//! Hardened Git worktree lifecycle for Agent tasks.
//!
//! This service deliberately invokes a verified system Git directly, never a
//! shell. Managed paths are canonical, symlink-free children of one private
//! root. Retirement archives a worktree by default; irreversible removal is an
//! explicit clean-only operation that first creates a recovery ref.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_GIT_STDOUT_BYTES: usize = 1024 * 1024;
pub const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;

const MAX_PATH_BYTES: usize = 4096;
const MAX_TASK_NAME_BYTES: usize = 80;
const MAX_BRANCH_BYTES: usize = 240;
const MAX_REVISION_BYTES: usize = 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ARCHIVE_DIRECTORY: &str = ".archive";
const TRUSTED_GIT_CANDIDATES: &[&str] = &["/usr/bin/git", "/bin/git"];
static UNIQUE_SUFFIX: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
const NULL_DEVICE: &str = "/dev/null";
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(any(unix, windows)))]
const NULL_DEVICE: &str = "/dev/null";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateWorktreeRequest {
    pub repository: PathBuf,
    pub task_name: String,
    pub branch: String,
    pub start_point: String,
}

impl CreateWorktreeRequest {
    pub fn new(
        repository: impl Into<PathBuf>,
        task_name: impl Into<String>,
        branch: impl Into<String>,
        start_point: impl Into<String>,
    ) -> Self {
        Self {
            repository: repository.into(),
            task_name: task_name.into(),
            branch: branch.into(),
            start_point: start_point.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedWorktree {
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub head: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RetirePolicy {
    /// Preserve the complete working directory and Git registration by moving
    /// it into the service's private archive directory.
    #[default]
    Archive,
    /// Remove only when tracked, untracked, ignored and submodule state are all
    /// clean. A recovery ref is created before Git removes any files.
    RemoveIfClean,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetireOutcome {
    Archived {
        previous_path: PathBuf,
        archived_path: PathBuf,
    },
    RemovedClean {
        removed_path: PathBuf,
        recovery_ref: String,
        head: String,
    },
}

#[derive(Debug)]
pub enum WorktreeError {
    InvalidInput(String),
    UnsafePath(String),
    UnsafeRepository(String),
    AlreadyExists(PathBuf),
    NotClean {
        path: PathBuf,
        summary: String,
        truncated: bool,
    },
    GitUnavailable(String),
    GitCommand {
        operation: String,
        detail: String,
    },
    Io {
        operation: String,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(formatter, "invalid worktree input: {message}"),
            Self::UnsafePath(message) => write!(formatter, "unsafe worktree path: {message}"),
            Self::UnsafeRepository(message) => write!(formatter, "unsafe repository: {message}"),
            Self::AlreadyExists(path) => {
                write!(
                    formatter,
                    "managed worktree path already exists: {}",
                    path.display()
                )
            }
            Self::NotClean { path, summary, .. } => write!(
                formatter,
                "worktree {} is not clean and was not removed: {summary}",
                path.display()
            ),
            Self::GitUnavailable(message) => formatter.write_str(message),
            Self::GitCommand { operation, detail } => {
                write!(formatter, "{operation} failed: {detail}")
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for WorktreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorktreeService {
    git: PathBuf,
    managed_root: PathBuf,
    archive_root: PathBuf,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
}

impl WorktreeService {
    /// Initialize a private managed root and resolve a trusted system Git.
    pub fn new(managed_root: impl Into<PathBuf>) -> Result<Self, WorktreeError> {
        let managed_root = initialize_managed_root(managed_root.into())?;
        let git = trusted_git_path().map_err(WorktreeError::GitUnavailable)?;
        Ok(Self {
            git,
            archive_root: managed_root.join(ARCHIVE_DIRECTORY),
            managed_root,
            timeout: GIT_COMMAND_TIMEOUT,
            cancel: None,
        })
    }

    /// Attach an owner-controlled cancellation flag. Setting it stops the
    /// current Git subprocess and prevents later lifecycle commands from
    /// starting, allowing UI/task owners to join their worker promptly.
    pub fn with_cancel_flag(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    pub fn archive_root(&self) -> &Path {
        &self.archive_root
    }

    /// Revalidate an existing task worktree before executing trusted task
    /// automation in it. Path containment alone is insufficient: an attacker
    /// could replace a removed worktree with an ordinary directory at the same
    /// canonical path. This proves Git still recognizes the exact top-level,
    /// that it shares the source repository's common directory, and that the
    /// task's branch identity has not changed.
    pub fn verify_active_task_worktree(
        &self,
        repository: impl AsRef<Path>,
        worktree: impl AsRef<Path>,
        expected_branch: &str,
    ) -> Result<(), WorktreeError> {
        self.verify_active_task_worktree_through(
            repository,
            worktree.as_ref(),
            worktree.as_ref(),
            expected_branch,
        )
    }

    /// Descriptor-anchored form used by validation. `access_path` may be a
    /// Linux `/proc/self/fd/N` directory kept open by the caller; Git then
    /// inspects the same directory inode that the validation child will enter,
    /// not a pathname that can be swapped between preflight and spawn.
    pub fn verify_active_task_worktree_through(
        &self,
        repository: impl AsRef<Path>,
        worktree: impl AsRef<Path>,
        access_path: impl AsRef<Path>,
        expected_branch: &str,
    ) -> Result<(), WorktreeError> {
        validate_branch_text(expected_branch)?;
        let repository = self.validate_repository(repository.as_ref())?;
        let worktree =
            self.validate_managed_existing(worktree.as_ref(), ManagedLocation::Active)?;
        self.verify_registered_worktree_through(&repository, &worktree, access_path.as_ref())?;
        let branch = self.symbolic_branch(access_path.as_ref())?;
        if branch != expected_branch {
            return Err(WorktreeError::UnsafePath(format!(
                "managed worktree branch {branch:?} does not match task branch {expected_branch:?}"
            )));
        }
        Ok(())
    }

    /// Resolve a source-command cwd (including a repository subdirectory) to
    /// its canonical Git top-level without consulting PATH or inherited Git
    /// configuration. Mutating lifecycle methods still require this exact root
    /// so a caller cannot accidentally create from an ambiguous nested path.
    pub fn resolve_repository_root(
        &self,
        candidate: impl AsRef<Path>,
    ) -> Result<PathBuf, WorktreeError> {
        let candidate = candidate.as_ref();
        validate_absolute_clean_path(candidate, "repository candidate")?;
        let candidate = canonical_exact(candidate, "repository candidate")?;
        let metadata = fs::metadata(&candidate).map_err(|source| WorktreeError::Io {
            operation: "cannot inspect repository candidate".to_string(),
            path: candidate.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(WorktreeError::UnsafeRepository(format!(
                "{} is not a directory",
                candidate.display()
            )));
        }

        let top = self.git_text(
            git_command(&self.git, &candidate, ["rev-parse", "--show-toplevel"]),
            "git rev-parse --show-toplevel",
        )?;
        let repository = canonical_exact(Path::new(&top), "reported repository root")?;
        if !fs::metadata(&repository).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(WorktreeError::UnsafeRepository(format!(
                "Git top-level {} is not a directory",
                repository.display()
            )));
        }
        if repository.starts_with(&self.managed_root) || self.managed_root.starts_with(&repository)
        {
            return Err(WorktreeError::UnsafeRepository(
                "repository and managed root must not contain one another".to_string(),
            ));
        }
        let bare = self.git_text(
            git_command(
                &self.git,
                &repository,
                ["rev-parse", "--is-bare-repository"],
            ),
            "git rev-parse --is-bare-repository",
        )?;
        if bare != "false" {
            return Err(WorktreeError::UnsafeRepository(
                "bare repositories cannot own task worktrees".to_string(),
            ));
        }
        Ok(repository)
    }

    /// Create a new branch-backed worktree as a direct child of the managed root.
    pub fn create(
        &self,
        request: &CreateWorktreeRequest,
    ) -> Result<ManagedWorktree, WorktreeError> {
        validate_task_name(&request.task_name)?;
        validate_branch_text(&request.branch)?;
        validate_revision_text(&request.start_point)?;
        let repository = self.validate_repository(&request.repository)?;
        self.reject_external_checkout_filters(&repository)?;
        self.check_branch_name(&repository, &request.branch)?;
        let start_commit = self.resolve_commit(&repository, &request.start_point)?;
        let target = self.managed_root.join(&request.task_name);
        ensure_missing_target(&target)?;

        let output = self.run(
            worktree_add_command(
                &self.git,
                &repository,
                &target,
                &request.branch,
                &start_commit,
            ),
            "git worktree add",
            MAX_GIT_STDOUT_BYTES,
        )?;
        self.require_success("git worktree add", output)?;

        let path = self.validate_managed_existing(&target, ManagedLocation::Active)?;
        self.verify_registered_worktree(&repository, &path)?;
        let head = self.resolve_head(&path)?;
        Ok(ManagedWorktree {
            repository,
            path,
            branch: request.branch.clone(),
            head,
        })
    }

    /// Safe default: move the worktree into the private archive without losing
    /// dirty files or unregistering its branch.
    pub fn retire(
        &self,
        repository: impl AsRef<Path>,
        worktree: impl AsRef<Path>,
    ) -> Result<RetireOutcome, WorktreeError> {
        self.retire_with_policy(repository, worktree, RetirePolicy::Archive)
    }

    pub fn retire_with_policy(
        &self,
        repository: impl AsRef<Path>,
        worktree: impl AsRef<Path>,
        policy: RetirePolicy,
    ) -> Result<RetireOutcome, WorktreeError> {
        let repository = self.validate_repository(repository.as_ref())?;
        let worktree =
            self.validate_managed_existing(worktree.as_ref(), ManagedLocation::Either)?;
        self.verify_registered_worktree(&repository, &worktree)?;
        match policy {
            RetirePolicy::Archive => self.archive_registered(&repository, &worktree),
            RetirePolicy::RemoveIfClean => self.remove_clean_registered(&repository, &worktree),
        }
    }

    /// Restore an archived worktree to a new direct child of the managed root.
    pub fn restore(
        &self,
        repository: impl AsRef<Path>,
        archived_worktree: impl AsRef<Path>,
        task_name: &str,
    ) -> Result<ManagedWorktree, WorktreeError> {
        validate_task_name(task_name)?;
        let repository = self.validate_repository(repository.as_ref())?;
        let archived =
            self.validate_managed_existing(archived_worktree.as_ref(), ManagedLocation::Archived)?;
        self.verify_registered_worktree(&repository, &archived)?;
        let target = self.managed_root.join(task_name);
        ensure_missing_target(&target)?;

        let output = self.run(
            worktree_move_command(&self.git, &repository, &archived, &target),
            "git worktree move (restore)",
            MAX_GIT_STDOUT_BYTES,
        )?;
        self.require_success("git worktree move (restore)", output)?;
        let path = self.validate_managed_existing(&target, ManagedLocation::Active)?;
        self.verify_registered_worktree(&repository, &path)?;
        let head = self.resolve_head(&path)?;
        let branch = self.symbolic_branch(&path).unwrap_or_default();
        Ok(ManagedWorktree {
            repository,
            path,
            branch,
            head,
        })
    }

    fn validate_repository(&self, repository: &Path) -> Result<PathBuf, WorktreeError> {
        validate_absolute_clean_path(repository, "repository")?;
        let canonical = canonical_exact(repository, "repository")?;
        let metadata = fs::metadata(&canonical).map_err(|source| WorktreeError::Io {
            operation: "cannot inspect repository".to_string(),
            path: canonical.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(WorktreeError::UnsafeRepository(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        let reported = self.resolve_repository_root(&canonical)?;
        if reported != canonical {
            return Err(WorktreeError::UnsafeRepository(format!(
                "{} is inside repository {}; pass the exact top-level path",
                canonical.display(),
                reported.display()
            )));
        }
        Ok(reported)
    }

    fn check_branch_name(&self, repository: &Path, branch: &str) -> Result<(), WorktreeError> {
        let output = self.run(
            git_command_os(
                &self.git,
                repository,
                [
                    OsString::from("check-ref-format"),
                    OsString::from("--branch"),
                    OsString::from(branch),
                ],
            ),
            "git check-ref-format --branch",
            8 * 1024,
        )?;
        self.require_success("git check-ref-format --branch", output)
            .map(|_| ())
    }

    fn resolve_commit(&self, repository: &Path, revision: &str) -> Result<String, WorktreeError> {
        let expression = format!("{revision}^{{commit}}");
        self.git_text(
            git_command_os(
                &self.git,
                repository,
                [
                    OsString::from("rev-parse"),
                    OsString::from("--verify"),
                    OsString::from("--end-of-options"),
                    OsString::from(expression),
                ],
            ),
            "git rev-parse --verify start point",
        )
        .and_then(validate_object_id)
    }

    fn resolve_head(&self, worktree: &Path) -> Result<String, WorktreeError> {
        self.git_text(
            git_command(&self.git, worktree, ["rev-parse", "--verify", "HEAD"]),
            "git rev-parse HEAD",
        )
        .and_then(validate_object_id)
    }

    fn symbolic_branch(&self, worktree: &Path) -> Result<String, WorktreeError> {
        self.git_text(
            git_command(
                &self.git,
                worktree,
                ["symbolic-ref", "--quiet", "--short", "HEAD"],
            ),
            "git symbolic-ref HEAD",
        )
    }

    fn reject_external_checkout_filters(&self, repository: &Path) -> Result<(), WorktreeError> {
        // Repository-local include/includeIf directives can pull executable
        // filter drivers from a file outside the repository. Reject includes
        // outright for this mutating checkout path; auditing only the literal
        // local file or following a mutable external include is not a stable
        // authority boundary.
        let includes = self.run(
            git_command(
                &self.git,
                repository,
                [
                    "config",
                    "--no-includes",
                    "--name-only",
                    "--get-regexp",
                    r"^(include|includeIf)\..*path$",
                ],
            ),
            "git config include audit",
            64 * 1024,
        )?;
        match includes.status.code() {
            Some(1) if includes.stdout.bytes.is_empty() => {}
            Some(0) => {
                return Err(WorktreeError::UnsafeRepository(format!(
                    "repository config includes external files ({})",
                    bounded_diagnostic(includes.stdout.bytes, 64 * 1024).trim()
                )));
            }
            _ => {
                self.require_success("git config include audit", includes)?;
            }
        }

        let output = self.run(
            git_command(
                &self.git,
                repository,
                [
                    "config",
                    "--includes",
                    "--name-only",
                    "--get-regexp",
                    r"^filter\..*\.(clean|smudge|process)$",
                ],
            ),
            "git config checkout-filter audit",
            64 * 1024,
        )?;
        match output.status.code() {
            Some(1) if output.stdout.bytes.is_empty() => Ok(()),
            Some(0) => Err(WorktreeError::UnsafeRepository(format!(
                "external checkout filters are configured ({})",
                bounded_diagnostic(output.stdout.bytes, 64 * 1024).trim()
            ))),
            _ => self
                .require_success("git config checkout-filter audit", output)
                .map(|_| ()),
        }
    }

    fn verify_registered_worktree(
        &self,
        repository: &Path,
        worktree: &Path,
    ) -> Result<(), WorktreeError> {
        self.verify_registered_worktree_through(repository, worktree, worktree)
    }

    fn verify_registered_worktree_through(
        &self,
        repository: &Path,
        expected_worktree: &Path,
        access_path: &Path,
    ) -> Result<(), WorktreeError> {
        let top = self.git_text(
            git_command(&self.git, access_path, ["rev-parse", "--show-toplevel"]),
            "git rev-parse managed worktree",
        )?;
        let top = canonical_exact(Path::new(&top), "managed worktree top-level")?;
        if top != expected_worktree {
            return Err(WorktreeError::UnsafePath(format!(
                "{} is not an exact worktree top-level",
                expected_worktree.display()
            )));
        }
        let repository_common = self.git_common_directory(repository)?;
        let worktree_common = self.git_common_directory(access_path)?;
        if repository_common != worktree_common {
            return Err(WorktreeError::UnsafeRepository(format!(
                "managed path {} belongs to a different Git repository",
                expected_worktree.display()
            )));
        }
        Ok(())
    }

    fn git_common_directory(&self, cwd: &Path) -> Result<PathBuf, WorktreeError> {
        let value = self.git_text(
            git_command(
                &self.git,
                cwd,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
            "git rev-parse --git-common-dir",
        )?;
        canonical_exact(Path::new(&value), "Git common directory")
    }

    fn archive_registered(
        &self,
        repository: &Path,
        worktree: &Path,
    ) -> Result<RetireOutcome, WorktreeError> {
        if worktree.parent() == Some(self.archive_root.as_path()) {
            return Ok(RetireOutcome::Archived {
                previous_path: worktree.to_path_buf(),
                archived_path: worktree.to_path_buf(),
            });
        }
        ensure_private_directory(&self.archive_root)?;
        let stem = worktree
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| {
                WorktreeError::UnsafePath("managed path has no UTF-8 leaf".to_string())
            })?;
        let archived = unique_child(&self.archive_root, stem)?;
        let output = self.run(
            worktree_move_command(&self.git, repository, worktree, &archived),
            "git worktree move (archive)",
            MAX_GIT_STDOUT_BYTES,
        )?;
        self.require_success("git worktree move (archive)", output)?;
        let archived = self.validate_managed_existing(&archived, ManagedLocation::Archived)?;
        self.verify_registered_worktree(repository, &archived)?;
        Ok(RetireOutcome::Archived {
            previous_path: worktree.to_path_buf(),
            archived_path: archived,
        })
    }

    fn remove_clean_registered(
        &self,
        repository: &Path,
        worktree: &Path,
    ) -> Result<RetireOutcome, WorktreeError> {
        self.reject_external_checkout_filters(repository)?;
        let status = self.run(
            git_command(
                &self.git,
                worktree,
                [
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--ignored=matching",
                    "--ignore-submodules=none",
                ],
            ),
            "git status clean check",
            MAX_GIT_STDOUT_BYTES,
        )?;
        let status = self.require_success("git status clean check", status)?;
        if !status.stdout.bytes.is_empty() {
            let truncated = status.stdout.truncated;
            let summary = status_summary(status.stdout.bytes);
            return Err(WorktreeError::NotClean {
                path: worktree.to_path_buf(),
                summary,
                truncated,
            });
        }

        let head = self.resolve_head(worktree)?;
        let recovery_ref = unique_recovery_ref();
        let update = self.run(
            git_command_os(
                &self.git,
                repository,
                [
                    OsString::from("update-ref"),
                    OsString::from("--create-reflog"),
                    OsString::from("-m"),
                    OsString::from("Anvil clean worktree retirement"),
                    OsString::from(&recovery_ref),
                    OsString::from(&head),
                ],
            ),
            "git update-ref recovery",
            16 * 1024,
        )?;
        self.require_success("git update-ref recovery", update)?;

        // Deliberately no --force. Git provides a second race-safe cleanliness
        // check if files change after the porcelain snapshot above.
        let remove = self.run(
            worktree_remove_command(&self.git, repository, worktree),
            "git worktree remove",
            MAX_GIT_STDOUT_BYTES,
        )?;
        self.require_success("git worktree remove", remove)?;
        if fs::symlink_metadata(worktree).is_ok() {
            return Err(WorktreeError::GitCommand {
                operation: "git worktree remove".to_string(),
                detail: "Git reported success but the managed path still exists".to_string(),
            });
        }
        Ok(RetireOutcome::RemovedClean {
            removed_path: worktree.to_path_buf(),
            recovery_ref,
            head,
        })
    }

    fn validate_managed_existing(
        &self,
        path: &Path,
        location: ManagedLocation,
    ) -> Result<PathBuf, WorktreeError> {
        validate_absolute_clean_path(path, "managed worktree")?;
        let canonical = canonical_exact(path, "managed worktree")?;
        let active = canonical.parent() == Some(self.managed_root.as_path())
            && canonical.file_name() != Some(OsStr::new(ARCHIVE_DIRECTORY));
        let archived = canonical.parent() == Some(self.archive_root.as_path());
        let allowed = match location {
            ManagedLocation::Active => active,
            ManagedLocation::Archived => archived,
            ManagedLocation::Either => active || archived,
        };
        if !allowed {
            return Err(WorktreeError::UnsafePath(format!(
                "{} is not an allowed direct child of {}",
                canonical.display(),
                self.managed_root.display()
            )));
        }
        if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(WorktreeError::UnsafePath(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    fn git_text(&self, command: Command, operation: &str) -> Result<String, WorktreeError> {
        let output = self.run(command, operation, 16 * 1024)?;
        let output = self.require_success(operation, output)?;
        let text =
            String::from_utf8(output.stdout.bytes).map_err(|_| WorktreeError::GitCommand {
                operation: operation.to_string(),
                detail: "Git returned non-UTF-8 metadata".to_string(),
            })?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() || text.contains(['\r', '\n', '\0']) {
            return Err(WorktreeError::GitCommand {
                operation: operation.to_string(),
                detail: "Git returned malformed metadata".to_string(),
            });
        }
        Ok(text.to_string())
    }

    fn run(
        &self,
        command: Command,
        operation: &str,
        stdout_limit: usize,
    ) -> Result<ProcessOutput, WorktreeError> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
        {
            return Err(WorktreeError::GitCommand {
                operation: operation.to_string(),
                detail: "operation was cancelled".to_string(),
            });
        }
        run_command_with_timeout(
            command,
            operation,
            stdout_limit,
            MAX_GIT_STDERR_BYTES,
            self.timeout,
            self.cancel.as_deref(),
        )
        .map_err(|detail| WorktreeError::GitCommand {
            operation: operation.to_string(),
            detail,
        })
    }

    fn require_success(
        &self,
        operation: &str,
        output: ProcessOutput,
    ) -> Result<ProcessOutput, WorktreeError> {
        if output.status.success() {
            return Ok(output);
        }
        let status = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |code| format!("exit {code}"));
        let stderr = bounded_diagnostic(output.stderr.bytes, MAX_GIT_STDERR_BYTES);
        let suffix = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", stderr.trim_end())
        };
        Err(WorktreeError::GitCommand {
            operation: operation.to_string(),
            detail: format!("{status}{suffix}"),
        })
    }

    #[cfg(test)]
    fn with_timeout_for_test(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_git_for_test(
        managed_root: impl Into<PathBuf>,
        git: impl AsRef<Path>,
    ) -> Result<Self, WorktreeError> {
        let managed_root = initialize_managed_root(managed_root.into())?;
        let git = fs::canonicalize(git.as_ref()).map_err(|source| WorktreeError::Io {
            operation: "cannot resolve test Git".to_string(),
            path: git.as_ref().to_path_buf(),
            source,
        })?;
        if !fs::metadata(&git).is_ok_and(|metadata| metadata.is_file()) {
            return Err(WorktreeError::GitUnavailable(
                "test Git is not a regular file".to_string(),
            ));
        }
        Ok(Self {
            git,
            archive_root: managed_root.join(ARCHIVE_DIRECTORY),
            managed_root,
            timeout: GIT_COMMAND_TIMEOUT,
            cancel: None,
        })
    }
}

#[derive(Clone, Copy)]
enum ManagedLocation {
    Active,
    Archived,
    Either,
}

#[derive(Debug)]
struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug)]
struct ProcessOutput {
    status: ExitStatus,
    stdout: CapturedBytes,
    stderr: CapturedBytes,
}

fn validate_task_name(value: &str) -> Result<(), WorktreeError> {
    if value.is_empty() || value.len() > MAX_TASK_NAME_BYTES {
        return Err(WorktreeError::InvalidInput(
            "task_name has an invalid length".to_string(),
        ));
    }
    if matches!(value, "." | ".." | ARCHIVE_DIRECTORY)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || value.ends_with('.')
        || value.contains("..")
    {
        return Err(WorktreeError::InvalidInput(
            "task_name must be one safe ASCII path component".to_string(),
        ));
    }
    Ok(())
}

fn validate_branch_text(value: &str) -> Result<(), WorktreeError> {
    if value.is_empty() || value.len() > MAX_BRANCH_BYTES || value.starts_with('-') {
        return Err(WorktreeError::InvalidInput(
            "branch has an invalid length or option-like prefix".to_string(),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || value.ends_with('/')
        || value.ends_with('.')
        || value
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(WorktreeError::InvalidInput(
            "branch is outside Anvil's strict safe ref grammar".to_string(),
        ));
    }
    Ok(())
}

fn validate_revision_text(value: &str) -> Result<(), WorktreeError> {
    if value.is_empty()
        || value.len() > MAX_REVISION_BYTES
        || value.starts_with('-')
        || value.chars().any(char::is_control)
    {
        return Err(WorktreeError::InvalidInput(
            "start_point is empty, oversized, option-like, or contains controls".to_string(),
        ));
    }
    Ok(())
}

fn validate_object_id(value: String) -> Result<String, WorktreeError> {
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value)
    } else {
        Err(WorktreeError::GitCommand {
            operation: "validate Git object id".to_string(),
            detail: "Git returned an invalid object id".to_string(),
        })
    }
}

fn validate_absolute_clean_path(path: &Path, label: &str) -> Result<(), WorktreeError> {
    if !path.is_absolute() {
        return Err(WorktreeError::UnsafePath(format!(
            "{label} must be absolute"
        )));
    }
    let text = path.to_str().ok_or_else(|| {
        WorktreeError::UnsafePath(format!(
            "{label} must be valid UTF-8 for safe Git diagnostics"
        ))
    })?;
    if text.len() > MAX_PATH_BYTES || text.chars().any(char::is_control) {
        return Err(WorktreeError::UnsafePath(format!(
            "{label} is oversized or contains control characters"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(WorktreeError::UnsafePath(format!(
            "{label} contains dot traversal components"
        )));
    }
    Ok(())
}

fn canonical_exact(path: &Path, label: &str) -> Result<PathBuf, WorktreeError> {
    let canonical = fs::canonicalize(path).map_err(|source| WorktreeError::Io {
        operation: format!("cannot resolve {label}"),
        path: path.to_path_buf(),
        source,
    })?;
    if canonical != path {
        return Err(WorktreeError::UnsafePath(format!(
            "{label} {} is not canonical or traverses a symlink (resolved {})",
            path.display(),
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn initialize_managed_root(path: PathBuf) -> Result<PathBuf, WorktreeError> {
    validate_absolute_clean_path(&path, "managed root")?;
    if path.parent().is_none() || path == Path::new("/") {
        return Err(WorktreeError::UnsafePath(
            "managed root cannot be the filesystem root".to_string(),
        ));
    }
    // Discover and validate the nearest existing ancestor before the first
    // write. `create_dir_all` would otherwise follow an existing symlink and
    // create directories outside the requested managed root before the final
    // canonical-path check had a chance to reject it.
    let mut missing = Vec::<OsString>::new();
    let mut existing = path.as_path();
    loop {
        match fs::symlink_metadata(existing) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(WorktreeError::UnsafePath(format!(
                        "managed-root ancestor {} is not a real directory",
                        existing.display()
                    )));
                }
                let _ = canonical_exact(existing, "managed-root ancestor")?;
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let leaf = existing.file_name().ok_or_else(|| {
                    WorktreeError::UnsafePath(
                        "managed root has no creatable path component".to_string(),
                    )
                })?;
                missing.push(leaf.to_owned());
                existing = existing.parent().ok_or_else(|| {
                    WorktreeError::UnsafePath("managed root has no existing ancestor".to_string())
                })?;
            }
            Err(source) => {
                return Err(WorktreeError::Io {
                    operation: "cannot inspect managed-root ancestor".to_string(),
                    path: existing.to_path_buf(),
                    source,
                });
            }
        }
    }

    let mut current = existing.to_path_buf();
    for leaf in missing.into_iter().rev() {
        current.push(leaf);
        create_private_directory_component(&current)?;
        let _ = canonical_exact(&current, "new managed-root component")?;
    }
    let canonical = canonical_exact(&path, "managed root")?;
    ensure_private_directory(&canonical)?;
    Ok(canonical)
}

fn ensure_private_directory(path: &Path) -> Result<(), WorktreeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(WorktreeError::UnsafePath(format!(
                    "{} is not a real directory",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory_component(path)?;
            return Ok(());
        }
        Err(source) => {
            return Err(WorktreeError::Io {
                operation: "cannot inspect private directory".to_string(),
                path: path.to_path_buf(),
                source,
            });
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::metadata(path).map_err(|source| WorktreeError::Io {
            operation: "cannot inspect private directory".to_string(),
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: geteuid has no preconditions and does not dereference memory.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(WorktreeError::UnsafePath(format!(
                "{} is not owned by the current user",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(WorktreeError::UnsafePath(format!(
                "{} is accessible by group or other users",
                path.display()
            )));
        }
    }
    Ok(())
}

fn create_private_directory_component(path: &Path) -> Result<(), WorktreeError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        // Apply 0700 in the mkdir syscall, avoiding a world-readable window
        // between creation and chmod even under a permissive umask.
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(WorktreeError::Io {
                operation: "cannot create private directory".to_string(),
                path: path.to_path_buf(),
                source,
            });
        }
    }
    // AlreadyExists can be a racing symlink or foreign directory. Inspect it
    // before any child path is constructed beneath it.
    ensure_private_directory(path)
}

#[cfg(test)]
fn set_private_permissions(path: &Path) -> Result<(), WorktreeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            WorktreeError::Io {
                operation: "cannot secure managed directory".to_string(),
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn ensure_missing_target(path: &Path) -> Result<(), WorktreeError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(WorktreeError::AlreadyExists(path.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorktreeError::Io {
            operation: "cannot inspect worktree target".to_string(),
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn unique_numbers() -> (u128, u64) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = UNIQUE_SUFFIX.fetch_add(1, Ordering::Relaxed);
    (nanos, sequence)
}

fn unique_child(parent: &Path, stem: &str) -> Result<PathBuf, WorktreeError> {
    for _ in 0..32 {
        let (nanos, sequence) = unique_numbers();
        let candidate = parent.join(format!("{stem}-{nanos:x}-{sequence:x}"));
        if fs::symlink_metadata(&candidate)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            return Ok(candidate);
        }
    }
    Err(WorktreeError::AlreadyExists(parent.to_path_buf()))
}

fn unique_recovery_ref() -> String {
    let (nanos, sequence) = unique_numbers();
    format!("refs/anvil/archive/{nanos:x}-{sequence:x}")
}

fn status_summary(mut bytes: Vec<u8>) -> String {
    for byte in &mut bytes {
        if *byte == 0 {
            *byte = b'\n';
        }
    }
    let summary = bounded_diagnostic(bytes, MAX_GIT_STDERR_BYTES);
    let summary = summary.trim();
    if summary.is_empty() {
        "Git reported worktree changes".to_string()
    } else {
        summary.to_string()
    }
}

fn trusted_git_path() -> Result<PathBuf, String> {
    let mut failures = Vec::new();
    for candidate in TRUSTED_GIT_CANDIDATES {
        match validate_trusted_git_candidate(Path::new(candidate)) {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{candidate}: {error}")),
        }
    }
    Err(format!(
        "no trusted system Git executable is available ({})",
        failures.join("; ")
    ))
}

fn validate_trusted_git_candidate(candidate: &Path) -> Result<PathBuf, String> {
    if !candidate.is_absolute() {
        return Err("candidate is not absolute".to_string());
    }
    let resolved = fs::canonicalize(candidate)
        .map_err(|error| format!("cannot resolve executable: {error}"))?;
    let metadata =
        fs::metadata(&resolved).map_err(|error| format!("cannot inspect executable: {error}"))?;
    if !metadata.is_file() {
        return Err("resolved executable is not a regular file".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if !is_system_owner(metadata.uid()) {
            return Err("resolved executable is not system-owned".to_string());
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("resolved executable is not executable".to_string());
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err("resolved executable is group- or world-writable".to_string());
        }
        let candidate_parent = fs::canonicalize(
            candidate
                .parent()
                .ok_or_else(|| "candidate has no parent".to_string())?,
        )
        .map_err(|error| format!("cannot resolve candidate directory: {error}"))?;
        validate_system_directory_chain(&candidate_parent)?;
        validate_system_directory_chain(
            resolved
                .parent()
                .ok_or_else(|| "resolved executable has no parent".to_string())?,
        )?;
    }
    Ok(resolved)
}

#[cfg(unix)]
fn is_system_owner(uid: u32) -> bool {
    // Rootless/managed OCI images commonly expose image-root ownership as the
    // overflow uid (`nobody`, conventionally 65534). Accept that representation
    // only when Anvil itself is not running as the same uid; the executable and
    // every checked ancestor must still be non-writable by group/other.
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    uid == 0 || (uid == 65_534 && uid != effective_uid)
}

#[cfg(unix)]
fn validate_system_directory_chain(directory: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for ancestor in directory.ancestors() {
        let metadata = fs::metadata(ancestor)
            .map_err(|error| format!("cannot inspect {}: {error}", ancestor.display()))?;
        if !metadata.is_dir() || !is_system_owner(metadata.uid()) {
            return Err(format!(
                "{} is not a system-owned directory",
                ancestor.display()
            ));
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "{} is group- or world-writable",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn configure_git_environment(
    command: &mut Command,
    inherited_keys: impl IntoIterator<Item = OsString>,
) {
    // Git's subprocess surface is larger than its own GIT_* namespace (remote
    // helpers, askpass, dynamic loaders, shell/editor hooks). Start from an
    // allowlist, then keep explicit removals below so tests pin sensitive
    // names and future changes cannot accidentally reintroduce them.
    command.env_clear();
    for key in inherited_keys {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    for key in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "SSH_ASKPASS",
        "EDITOR",
        "VISUAL",
    ] {
        command.env_remove(key);
    }
    command
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", NULL_DEVICE)
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
}

fn base_git_command(git: &Path, cwd: &Path) -> Command {
    debug_assert!(git.is_absolute());
    let mut command = Command::new(git);
    command
        .arg("--no-pager")
        .arg("-c")
        .arg(format!("core.hooksPath={NULL_DEVICE}"))
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("credential.helper=");
    configure_git_environment(&mut command, std::env::vars_os().map(|(key, _)| key));
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn git_command<const N: usize>(git: &Path, cwd: &Path, args: [&str; N]) -> Command {
    let mut command = base_git_command(git, cwd);
    command.args(args);
    command
}

fn git_command_os<const N: usize>(git: &Path, cwd: &Path, args: [OsString; N]) -> Command {
    let mut command = base_git_command(git, cwd);
    command.args(args);
    command
}

fn worktree_add_command(
    git: &Path,
    repository: &Path,
    target: &Path,
    branch: &str,
    commit: &str,
) -> Command {
    git_command_os(
        git,
        repository,
        [
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(branch),
            OsString::from("--"),
            target.as_os_str().to_owned(),
            OsString::from(commit),
        ],
    )
}

fn worktree_move_command(git: &Path, repository: &Path, from: &Path, to: &Path) -> Command {
    git_command_os(
        git,
        repository,
        [
            OsString::from("worktree"),
            OsString::from("move"),
            OsString::from("--"),
            from.as_os_str().to_owned(),
            to.as_os_str().to_owned(),
        ],
    )
}

fn worktree_remove_command(git: &Path, repository: &Path, path: &Path) -> Command {
    git_command_os(
        git,
        repository,
        [
            OsString::from("worktree"),
            OsString::from("remove"),
            OsString::from("--"),
            path.as_os_str().to_owned(),
        ],
    )
}

fn run_command_with_timeout(
    mut command: Command,
    label: &str,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<ProcessOutput, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        #[cfg(target_os = "linux")]
        // SAFETY: the closure calls only async-signal-safe libc operations
        // before exec. PDEATHSIG prevents a mutating Git child from surviving
        // an abrupt Anvil process exit after its worker thread is detached.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start {label}: {error}"))?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = kill_process_group_and_wait(&mut child);
            return Err("Git stdout pipe was unavailable".to_string());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let _ = kill_process_group_and_wait(&mut child);
            return Err("Git stderr pipe was unavailable".to_string());
        }
    };

    // Keep the direct child unreaped until both output readers have observed
    // EOF. Its zombie PID anchors the private process-group identity, so a
    // drain timeout can never signal a newly-reused PID/PGID. The Unix readers
    // are nonblocking and share the same hard deadline, which also keeps a
    // pipe-holding descendant from hanging the scoped joins forever.
    let (stdout, stderr) = std::thread::scope(|scope| {
        let stdout_reader =
            scope.spawn(move || read_bounded_until(stdout, stdout_limit, deadline, cancel));
        let stderr_reader =
            scope.spawn(move || read_bounded_until(stderr, stderr_limit, deadline, cancel));
        let stdout = stdout_reader
            .join()
            .map_err(|_| "Git stdout reader panicked".to_string())
            .and_then(|result| result.map_err(|error| format!("cannot read Git stdout: {error}")));
        let stderr = stderr_reader
            .join()
            .map_err(|_| "Git stderr reader panicked".to_string())
            .and_then(|result| result.map_err(|error| format!("cannot read Git stderr: {error}")));
        (stdout, stderr)
    });

    let (stdout, stdout_closed) = match stdout {
        Ok(stdout) => stdout,
        Err(error) => {
            let _ = kill_process_group_and_wait(&mut child);
            return Err(error);
        }
    };
    let (stderr, stderr_closed) = match stderr {
        Ok(stderr) => stderr,
        Err(error) => {
            let _ = kill_process_group_and_wait(&mut child);
            return Err(error);
        }
    };
    if !stdout_closed || !stderr_closed {
        let cleanup = kill_process_group_and_wait(&mut child).err();
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
            return Err(match cleanup {
                Some(error) => format!("{label} was cancelled; cleanup failed: {error}"),
                None => format!("{label} was cancelled"),
            });
        }
        return Err(match cleanup {
            Some(error) => format!(
                "{label} timed out after {} ms while draining output; cleanup failed: {error}",
                timeout.as_millis()
            ),
            None => format!(
                "{label} timed out after {} ms while draining output",
                timeout.as_millis()
            ),
        });
    }
    let status = wait_for_child(&mut child, label, timeout, deadline, cancel)?;
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn wait_for_child(
    child: &mut Child,
    label: &str,
    timeout: Duration,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // An error such as ECHILD can mean another subsystem already
                // reaped the leader. Its PID may be reusable, so signalling
                // the cached numeric PGID here would be unsafe.
                return Err(format!("could not wait for {label}: {error}"));
            }
        }
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
            let cleanup = kill_process_group_and_wait(child).err();
            return Err(match cleanup {
                Some(error) => format!("{label} was cancelled; cleanup failed: {error}"),
                None => format!("{label} was cancelled"),
            });
        }
        let now = Instant::now();
        if now >= deadline {
            let cleanup = kill_process_group_and_wait(child).err();
            return Err(match cleanup {
                Some(error) => format!(
                    "{label} timed out after {} ms; cleanup failed: {error}",
                    timeout.as_millis()
                ),
                None => format!("{label} timed out after {} ms", timeout.as_millis()),
            });
        }
        std::thread::sleep(CHILD_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn kill_process_group_and_wait(child: &mut Child) -> Result<(), String> {
    let mut kill_error = kill_process_group_id(child.id()).err();
    #[cfg(unix)]
    if kill_error.is_some() {
        if let Err(error) = child.kill() {
            if error.kind() != io::ErrorKind::InvalidInput {
                kill_error = Some(format!(
                    "{}; direct kill failed: {error}",
                    kill_error.take().unwrap_or_default()
                ));
            }
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = child.kill() {
        if error.kind() != io::ErrorKind::InvalidInput {
            kill_error = Some(format!("direct kill failed: {error}"));
        }
    }
    let wait_error = child
        .wait()
        .err()
        .map(|error| format!("could not reap process: {error}"));
    match (kill_error, wait_error) {
        (None, None) => Ok(()),
        (Some(error), None) | (None, Some(error)) => Err(error),
        (Some(kill), Some(wait)) => Err(format!("{kill}; {wait}")),
    }
}

#[cfg(unix)]
fn kill_process_group_id(child_id: u32) -> Result<(), String> {
    let group = i32::try_from(child_id)
        .map_err(|_| "Git process id does not fit a process group id".to_string())?;
    // SAFETY: the child was spawned with process_group(0); negating its PID
    // targets only that private group.
    if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("could not kill Git process group: {error}"))
    }
}

#[cfg(not(unix))]
fn kill_process_group_id(_child_id: u32) -> Result<(), String> {
    Ok(())
}

#[cfg(any(test, not(unix)))]
fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<CapturedBytes> {
    let mut bytes = Vec::with_capacity(limit.min(READ_CHUNK_BYTES));
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let keep = read.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
    Ok(CapturedBytes { bytes, truncated })
}

#[cfg(unix)]
fn read_bounded_until(
    mut reader: impl Read + std::os::fd::AsRawFd,
    limit: usize,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> io::Result<(CapturedBytes, bool)> {
    let descriptor = reader.as_raw_fd();
    // SAFETY: fcntl reads/updates flags on the owned pipe descriptor; the
    // descriptor remains alive for the duration of this function.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same live descriptor, preserving every existing status flag.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut bytes = Vec::with_capacity(limit.min(READ_CHUNK_BYTES));
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut truncated = false;
    loop {
        if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
            return Ok((CapturedBytes { bytes, truncated }, false));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok((CapturedBytes { bytes, truncated }, false));
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Ok((CapturedBytes { bytes, truncated }, true)),
            Ok(read) => {
                let keep = read.min(limit.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&chunk[..keep]);
                truncated |= keep < read;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(
                    CHILD_POLL_INTERVAL.min(deadline.saturating_duration_since(now)),
                );
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(unix))]
fn read_bounded_until(
    reader: impl Read,
    limit: usize,
    _deadline: Instant,
    _cancel: Option<&AtomicBool>,
) -> io::Result<(CapturedBytes, bool)> {
    read_bounded(reader, limit).map(|captured| (captured, true))
}

fn bounded_diagnostic(bytes: Vec<u8>, limit: usize) -> String {
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > limit {
        let mut end = limit;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text.chars()
        .map(|character| match character {
            unsafe_character
                if unsafe_character.is_control() || is_visual_spoof(unsafe_character) =>
            {
                '?'
            }
            visible => visible,
        })
        .collect()
}

fn is_visual_spoof(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let (nanos, sequence) = unique_numbers();
            let path = std::env::temp_dir().join(format!(
                "anvil-worktree-{label}-{}-{nanos:x}-{sequence:x}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            set_private_permissions(&path).expect("secure test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn checked(command: Command, label: &str) -> ProcessOutput {
        let output = run_command_with_timeout(
            command,
            label,
            MAX_GIT_STDOUT_BYTES,
            MAX_GIT_STDERR_BYTES,
            Duration::from_secs(10),
            None,
        )
        .expect("Git test command runs");
        assert!(
            output.status.success(),
            "{label}: {}",
            bounded_diagnostic(output.stderr.bytes, MAX_GIT_STDERR_BYTES)
        );
        output
    }

    fn test_git_path() -> PathBuf {
        TRUSTED_GIT_CANDIDATES
            .iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .and_then(|path| fs::canonicalize(path).ok())
            .expect("Git executable for integration tests")
    }

    fn repository_fixture(label: &str) -> (TestDirectory, PathBuf, WorktreeService) {
        let root = TestDirectory::new(label);
        let repository = root.0.join("repository");
        let managed = root.0.join("managed");
        fs::create_dir(&repository).expect("repository directory");
        let git = test_git_path();
        checked(
            git_command(&git, &repository, ["init", "--quiet"]),
            "git init",
        );
        fs::write(repository.join("tracked.txt"), b"baseline\n").expect("seed file");
        fs::write(repository.join(".gitignore"), b"ignored.log\n").expect("gitignore");
        checked(
            git_command(
                &git,
                &repository,
                ["add", "--", "tracked.txt", ".gitignore"],
            ),
            "git add",
        );
        checked(
            git_command(
                &git,
                &repository,
                [
                    "-c",
                    "user.name=Anvil Tests",
                    "-c",
                    "user.email=anvil@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "baseline",
                ],
            ),
            "git commit",
        );
        // Exercise the same trusted-Git resolver used by production. Dedicated
        // tests still inject a helper only when they need timeout behavior.
        let service = WorktreeService::new(managed).expect("worktree service");
        (root, repository, service)
    }

    fn create_fixture(service: &WorktreeService, repository: &Path, task: &str) -> ManagedWorktree {
        service
            .create(&CreateWorktreeRequest::new(
                repository,
                task,
                format!("anvil/{task}"),
                "HEAD",
            ))
            .expect("create worktree")
    }

    #[test]
    fn task_and_branch_validation_rejects_traversal_and_options() {
        for invalid in ["", ".", "..", ".archive", "../escape", "a/b", "a..b"] {
            assert!(validate_task_name(invalid).is_err(), "{invalid}");
        }
        for invalid in ["-branch", "a..b", "a//b", ".hidden/x", "a.lock"] {
            assert!(validate_branch_text(invalid).is_err(), "{invalid}");
        }
        assert!(validate_task_name("task-42_ok").is_ok());
        assert!(validate_branch_text("anvil/task-42").is_ok());
    }

    #[test]
    fn bounded_reader_drains_but_never_retains_beyond_limit() {
        let captured = read_bounded(Cursor::new(vec![b'x'; READ_CHUNK_BYTES * 2]), 17).unwrap();
        assert_eq!(captured.bytes.len(), 17);
        assert!(captured.truncated);
    }

    #[test]
    fn diagnostics_are_bounded_single_line_and_neutralize_bidi() {
        let text = bounded_diagnostic(b"safe\nforged\tstatus\xe2\x80\xaeend".to_vec(), 24);
        assert!(text.len() <= 24);
        assert!(!text.contains('\n'));
        assert!(!text.contains('\t'));
        assert!(!text.contains('\u{202e}'));
        assert_eq!(text, "safe?forged?status?end");
    }

    #[test]
    fn configured_command_uses_fixed_git_and_scrubs_injection_environment() {
        let mut command = Command::new("/usr/bin/git");
        command.env("UNRELATED_INHERITED_VALUE", "must-not-survive");
        configure_git_environment(
            &mut command,
            [
                OsString::from("GIT_DIR"),
                OsString::from("GIT_CONFIG_KEY_0"),
                OsString::from("GIT_EXEC_PATH"),
                OsString::from("LD_PRELOAD"),
            ],
        );
        let entries = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<HashMap<_, _>>();
        for removed in [
            "UNRELATED_INHERITED_VALUE",
            "GIT_DIR",
            "GIT_CONFIG_KEY_0",
            "GIT_EXEC_PATH",
            "LD_PRELOAD",
        ] {
            assert!(!entries.contains_key(removed), "{removed} leaked");
        }
        assert_eq!(
            entries.get("GIT_CONFIG_GLOBAL").and_then(Option::as_deref),
            Some(NULL_DEVICE)
        );
        assert_eq!(
            entries
                .get("GIT_TERMINAL_PROMPT")
                .and_then(Option::as_deref),
            Some("0")
        );
        assert_eq!(
            entries.get("GIT_NO_LAZY_FETCH").and_then(Option::as_deref),
            Some("1")
        );
    }

    #[cfg(unix)]
    #[test]
    fn trusted_git_ownership_accepts_only_root_or_distinct_overflow_owner() {
        assert!(is_system_owner(0));
        // SAFETY: geteuid has no preconditions and does not dereference memory.
        let current = unsafe { libc::geteuid() };
        assert_eq!(is_system_owner(65_534), current != 65_534);
        if current != 0 && current != 65_534 {
            assert!(!is_system_owner(current));
        }
    }

    #[test]
    fn production_git_resolver_accepts_the_validated_system_binary() {
        let git = trusted_git_path().expect("trusted system Git");
        assert!(git.is_absolute());
        assert!(git.is_file());
    }

    #[test]
    fn worktree_commands_have_fixed_arguments_and_no_force() {
        let add = worktree_add_command(
            Path::new("/usr/bin/git"),
            Path::new("/repo"),
            Path::new("/managed/task"),
            "anvil/task",
            "0123456789012345678901234567890123456789",
        );
        assert_eq!(add.get_program(), "/usr/bin/git");
        let args = add
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["worktree", "add"]));
        assert!(args.contains(&"--".to_string()));

        let remove = worktree_remove_command(
            Path::new("/usr/bin/git"),
            Path::new("/repo"),
            Path::new("/managed/task"),
        );
        let args = remove
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(!args.iter().any(|arg| arg == "--force" || arg == "-f"));
    }

    #[test]
    fn create_archive_and_restore_preserves_dirty_files() {
        let (_root, repository, service) = repository_fixture("archive");
        let created = create_fixture(&service, &repository, "task-one");
        fs::write(created.path.join("dirty.txt"), b"keep me").unwrap();

        let RetireOutcome::Archived { archived_path, .. } =
            service.retire(&repository, &created.path).expect("archive")
        else {
            panic!("expected archive");
        };
        assert!(!created.path.exists());
        assert_eq!(
            fs::read(archived_path.join("dirty.txt")).unwrap(),
            b"keep me"
        );

        let restored = service
            .restore(&repository, &archived_path, "task-restored")
            .expect("restore");
        assert_eq!(
            fs::read(restored.path.join("dirty.txt")).unwrap(),
            b"keep me"
        );
    }

    #[test]
    fn explicit_remove_rejects_untracked_and_ignored_files() {
        let (_root, repository, service) = repository_fixture("dirty-remove");
        let created = create_fixture(&service, &repository, "task-dirty");
        fs::write(created.path.join("ignored.log"), b"valuable build output").unwrap();
        let error = service
            .retire_with_policy(&repository, &created.path, RetirePolicy::RemoveIfClean)
            .expect_err("ignored data must prevent removal");
        assert!(matches!(error, WorktreeError::NotClean { .. }));
        assert!(created.path.join("ignored.log").exists());
    }

    #[test]
    fn explicit_clean_remove_creates_recovery_ref_first() {
        let (_root, repository, service) = repository_fixture("clean-remove");
        let created = create_fixture(&service, &repository, "task-clean");
        let RetireOutcome::RemovedClean {
            recovery_ref, head, ..
        } = service
            .retire_with_policy(&repository, &created.path, RetirePolicy::RemoveIfClean)
            .expect("clean remove")
        else {
            panic!("expected clean removal");
        };
        assert!(!created.path.exists());
        let git = test_git_path();
        let output = checked(
            git_command_os(
                &git,
                &repository,
                [
                    OsString::from("show-ref"),
                    OsString::from("--verify"),
                    OsString::from(&recovery_ref),
                ],
            ),
            "verify recovery ref",
        );
        assert!(String::from_utf8_lossy(&output.stdout.bytes).starts_with(&head));
    }

    #[test]
    fn repository_checkout_filter_is_rejected_without_executing_it() {
        let (root, repository, service) = repository_fixture("filter");
        let marker = root.0.join("filter-ran");
        let git = test_git_path();
        checked(
            git_command_os(
                &git,
                &repository,
                [
                    OsString::from("config"),
                    OsString::from("--local"),
                    OsString::from("filter.hostile.smudge"),
                    OsString::from(format!("touch {}", marker.display())),
                ],
            ),
            "configure hostile filter",
        );
        let error = service
            .create(&CreateWorktreeRequest::new(
                &repository,
                "filter-task",
                "anvil/filter-task",
                "HEAD",
            ))
            .expect_err("external filter must be rejected");
        assert!(matches!(error, WorktreeError::UnsafeRepository(_)));
        assert!(!marker.exists());
    }

    #[test]
    fn repository_config_include_is_rejected_without_executing_its_filter() {
        let (root, repository, service) = repository_fixture("included-filter");
        let marker = root.0.join("included-filter-ran");
        let included = root.0.join("outside-repository.conf");
        fs::write(
            &included,
            format!(
                "[filter \"hostile\"]\n\tsmudge = touch {}\n",
                marker.display()
            ),
        )
        .expect("write included config");
        let git = test_git_path();
        checked(
            git_command_os(
                &git,
                &repository,
                [
                    OsString::from("config"),
                    OsString::from("--local"),
                    OsString::from("include.path"),
                    included.into_os_string(),
                ],
            ),
            "configure repository include",
        );

        let error = service
            .create(&CreateWorktreeRequest::new(
                &repository,
                "included-filter-task",
                "anvil/included-filter-task",
                "HEAD",
            ))
            .expect_err("repository include must be rejected");
        assert!(matches!(error, WorktreeError::UnsafeRepository(_)));
        assert!(!marker.exists());
    }

    #[test]
    fn worktree_scope_filter_is_rejected_without_executing_it() {
        let (root, repository, service) = repository_fixture("worktree-filter");
        let marker = root.0.join("worktree-filter-ran");
        let git = test_git_path();
        checked(
            git_command(
                &git,
                &repository,
                ["config", "--local", "extensions.worktreeConfig", "true"],
            ),
            "enable worktree config",
        );
        checked(
            git_command_os(
                &git,
                &repository,
                [
                    OsString::from("config"),
                    OsString::from("--worktree"),
                    OsString::from("filter.hostile.smudge"),
                    OsString::from(format!("touch {}", marker.display())),
                ],
            ),
            "configure worktree-scoped filter",
        );

        let error = service
            .create(&CreateWorktreeRequest::new(
                &repository,
                "worktree-filter-task",
                "anvil/worktree-filter-task",
                "HEAD",
            ))
            .expect_err("worktree-scoped filter must be rejected");
        assert!(matches!(error, WorktreeError::UnsafeRepository(_)));
        assert!(!marker.exists());
    }

    #[test]
    fn worktree_scope_include_is_rejected_without_following_it() {
        let (root, repository, service) = repository_fixture("worktree-include");
        let marker = root.0.join("worktree-include-ran");
        let included = root.0.join("outside-worktree.conf");
        fs::write(
            &included,
            format!(
                "[filter \"hostile\"]\n\tsmudge = touch {}\n",
                marker.display()
            ),
        )
        .expect("write included config");
        let git = test_git_path();
        checked(
            git_command(
                &git,
                &repository,
                ["config", "--local", "extensions.worktreeConfig", "true"],
            ),
            "enable worktree config",
        );
        checked(
            git_command_os(
                &git,
                &repository,
                [
                    OsString::from("config"),
                    OsString::from("--worktree"),
                    OsString::from("include.path"),
                    included.into_os_string(),
                ],
            ),
            "configure worktree-scoped include",
        );

        let error = service
            .create(&CreateWorktreeRequest::new(
                &repository,
                "worktree-include-task",
                "anvil/worktree-include-task",
                "HEAD",
            ))
            .expect_err("worktree-scoped include must be rejected");
        assert!(matches!(error, WorktreeError::UnsafeRepository(_)));
        assert!(!marker.exists());
    }

    #[test]
    fn repository_subdirectory_resolves_root_but_create_requires_exact_root() {
        let (_root, repository, service) = repository_fixture("resolve-subdir");
        let subdirectory = repository.join("src/nested");
        fs::create_dir_all(&subdirectory).unwrap();

        assert_eq!(
            service
                .resolve_repository_root(&subdirectory)
                .expect("resolve repository root"),
            repository
        );
        let error = service
            .create(&CreateWorktreeRequest::new(
                &subdirectory,
                "subdir-task",
                "anvil/subdir-task",
                "HEAD",
            ))
            .expect_err("mutating create still requires the exact root");
        assert!(matches!(error, WorktreeError::UnsafeRepository(_)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_managed_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = TestDirectory::new("root-link");
        let real = root.0.join("real");
        let link = root.0.join("link");
        fs::create_dir(&real).unwrap();
        set_private_permissions(&real).unwrap();
        symlink(&real, &link).unwrap();
        assert!(matches!(
            WorktreeService::new(link),
            Err(WorktreeError::UnsafePath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_is_rejected_before_creating_through_it() {
        use std::os::unix::fs::symlink;
        let root = TestDirectory::new("ancestor-link");
        let real = root.0.join("real");
        let link = root.0.join("link");
        fs::create_dir(&real).unwrap();
        set_private_permissions(&real).unwrap();
        symlink(&real, &link).unwrap();

        let requested = link.join("must-not-exist/managed");
        assert!(matches!(
            WorktreeService::new(requested),
            Err(WorktreeError::UnsafePath(_))
        ));
        assert!(
            !real.join("must-not-exist").exists(),
            "validation wrote through a symlinked ancestor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_and_reaps_the_private_process_group() {
        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .expect("system sleep");
        let mut command = Command::new(sleep);
        command
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let error = run_command_with_timeout(
            command,
            "worktree timeout test",
            32,
            32,
            Duration::from_millis(25),
            None,
        )
        .expect_err("sleep must time out");
        assert!(error.contains("timed out after 25 ms"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_and_reaps_without_waiting_for_the_full_timeout() {
        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .expect("system sleep");
        let mut command = Command::new(sleep);
        command
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancel);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            trigger.store(true, Ordering::Release);
        });

        let started = Instant::now();
        let error = run_command_with_timeout(
            command,
            "worktree cancellation test",
            32,
            32,
            Duration::from_secs(5),
            Some(&cancel),
        )
        .expect_err("cancelled child must stop");
        canceller.join().expect("canceller joins");
        assert!(error.contains("was cancelled"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn repository_and_managed_root_must_not_contain_one_another() {
        let (root, repository, _service) = repository_fixture("overlap");
        let nested_service = WorktreeService::with_git_for_test(
            repository.join("managed-inside-repo"),
            test_git_path(),
        )
        .expect("nested root initializes before repository validation");
        let error = nested_service
            .create(&CreateWorktreeRequest::new(
                &repository,
                "overlap-task",
                "anvil/overlap-task",
                "HEAD",
            ))
            .expect_err("overlap must be rejected");
        assert!(matches!(error, WorktreeError::UnsafeRepository(_)));
        drop(root);
    }

    #[test]
    fn test_timeout_override_is_scoped_to_service_instance() {
        let root = TestDirectory::new("timeout-api");
        let service = WorktreeService::with_git_for_test(root.0.join("managed"), test_git_path())
            .unwrap()
            .with_timeout_for_test(Duration::from_millis(50));
        assert_eq!(service.timeout, Duration::from_millis(50));
    }
}
