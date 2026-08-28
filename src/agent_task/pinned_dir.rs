//! Directory capability pinning for task worktrees and validation cwds.
//!
//! Ported from ember's `pty.rs` descriptor-pinning helpers so the agent task
//! runtime never re-resolves a pathname an attacker could replace.

use std::ffi::CString;
use std::os::unix::io::RawFd;

/// Errors carry the exact messages of ember's anyhow-based original; every
/// consumer stringifies them into its own domain error, so a plain `String`
/// keeps this module dependency-free without changing observable behavior.
type Result<T> = std::result::Result<T, String>;

#[cfg(unix)]
pub(crate) struct PinnedDirectory(std::fs::File);

#[cfg(unix)]
impl std::fmt::Debug for PinnedDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::os::fd::AsRawFd;
        formatter
            .debug_tuple("PinnedDirectory")
            .field(&self.0.as_raw_fd())
            .finish()
    }
}

#[cfg(unix)]
impl PinnedDirectory {
    pub(crate) fn open(path: &std::path::Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;

        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| {
                format!(
                    "cannot pin validation directory {}: {error}",
                    path.display()
                )
            })?;
        if !directory
            .metadata()
            .map_err(|error| {
                format!(
                    "cannot inspect pinned validation directory {}: {error}",
                    path.display()
                )
            })?
            .is_dir()
        {
            return Err(format!(
                "validation working directory is not a directory: {}",
                path.display()
            ));
        }
        Ok(Self(directory))
    }

    pub(crate) fn proc_path(&self) -> std::path::PathBuf {
        use std::os::fd::AsRawFd;
        std::path::PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }

    /// Open a descendant directory without ever resolving a pathname from the
    /// process root. Each component is resolved relative to an already-open
    /// parent descriptor and rejects symlinks, `..`, and absolute paths.
    ///
    /// This closes the canonicalize-then-open race for nested task working
    /// directories: replacing an ancestor with a symlink can no longer move
    /// the returned capability outside `self`.
    pub(crate) fn open_beneath(&self, relative: &std::path::Path) -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::path::Component;

        if relative.is_absolute() {
            return Err(format!(
                "cannot pin absolute descendant directory {}",
                relative.display()
            ));
        }

        let mut directory = self
            .0
            .try_clone()
            .map_err(|error| format!("cannot clone pinned directory: {error}"))?;
        for component in relative.components() {
            let name = match component {
                Component::CurDir => continue,
                Component::Normal(name) => name,
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "descendant directory contains an unsafe component: {}",
                        relative.display()
                    ));
                }
            };
            let name = CString::new(name.as_bytes()).map_err(|_| {
                format!(
                    "descendant directory contains a NUL byte: {}",
                    relative.display()
                )
            })?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "cannot pin descendant directory {}: {}",
                    relative.display(),
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: `openat` returned a new owned descriptor on success.
            directory = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        Ok(Self(directory))
    }

    pub(crate) fn as_raw_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }
}
