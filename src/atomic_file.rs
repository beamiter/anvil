//! Crash-safe replacement of small persistence files.
//!
//! The temporary file is created next to the destination so the final rename
//! stays on the same filesystem and remains atomic.

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
#[cfg(any(not(unix), test))]
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn next_temp_id() -> u64 {
    let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    {
        let mut random = [0_u8; std::mem::size_of::<u64>()];
        // SAFETY: random is a writable byte buffer of the exact supplied
        // length. GRND_NONBLOCK avoids making persistence wait for entropy.
        let read = unsafe {
            nix::libc::getrandom(
                random.as_mut_ptr().cast(),
                random.len(),
                nix::libc::GRND_NONBLOCK,
            )
        };
        if read == random.len() as isize {
            return u64::from_ne_bytes(random) ^ sequence;
        }
    }

    // Entropy failure must not break persistence. This fallback still mixes a
    // high-resolution timestamp with a process-local monotonic value, while
    // O_EXCL remains the actual no-clobber guarantee.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    timestamp ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(std::process::id())
}

#[cfg(not(unix))]
struct TempFileGuard {
    path: PathBuf,
    committed: bool,
}

#[cfg(not(unix))]
impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

#[cfg(not(unix))]
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
struct TempFileGuard<'a> {
    parent: &'a File,
    name: std::ffi::CString,
    committed: bool,
}

#[cfg(unix)]
impl<'a> TempFileGuard<'a> {
    fn new(parent: &'a File, name: std::ffi::CString) -> Self {
        Self {
            parent,
            name,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

#[cfg(unix)]
impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        if !self.committed {
            // SAFETY: both the live directory descriptor and CString pointer
            // remain valid for the duration of this call.
            let _ = unsafe { nix::libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

fn destination_parent(path: &Path) -> io::Result<&Path> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic-write destination has no file name",
        ));
    }

    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent),
        _ => Ok(Path::new(".")),
    }
}

/// Name of the sibling temporary file used while replacing `destination_name`.
///
/// Exposed to the crate because [`crate::snapshot_file`] has to assert that the
/// name cannot be read back as a session snapshot by the frontends' directory
/// scans, and a test asserting against a *copy* of this formula would keep
/// passing after the formula changed.
pub(crate) fn temp_file_name(destination_name: &OsStr, id: u64) -> OsString {
    let mut temp_name = OsString::from(".");
    temp_name.push(destination_name);
    temp_name.push(format!(".tmp.{}.{id}", std::process::id()));
    temp_name
}

#[cfg(not(unix))]
fn create_unique_temp(path: &Path, parent: &Path) -> io::Result<(File, PathBuf)> {
    let destination_name = path
        .file_name()
        .expect("destination_parent validates the file name");

    // A stale file can still collide with an entropy-backed identifier.
    // create_new plus retry handles that without clobbering it.
    for _ in 0..128 {
        let id = next_temp_id();
        let temp_path = parent.join(temp_file_name(destination_name, id));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
        }

        match options.open(&temp_path) {
            Ok(file) => return Ok((file, temp_path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique atomic-write temporary file",
    ))
}

#[cfg(unix)]
fn open_parent_directory(parent: &Path) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;

    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = directory.metadata()?;
    let mode = metadata.mode();
    // A foreign directory owner can unlink and replace entries even when the
    // sticky bit protects them from other users. Only the current user and the
    // system administrator are trusted to own the namespace in which an
    // atomic publication takes place (the latter keeps `/tmp` usable).
    // SAFETY: geteuid has no preconditions and only reads process state.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if metadata.uid() != effective_uid && metadata.uid() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "atomic-write parent is not owned by the current user or root",
        ));
    }
    if mode & 0o022 != 0 && mode & nix::libc::S_ISVTX == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "atomic-write parent is group/world-writable without the sticky bit",
        ));
    }
    Ok(directory)
}

#[cfg(unix)]
fn os_string_to_cstring(value: &OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic-write file name contains a NUL byte",
        )
    })
}

#[cfg(unix)]
fn create_unique_temp_at(
    parent: &File,
    destination_name: &OsStr,
) -> io::Result<(File, std::ffi::CString)> {
    use std::os::fd::{AsRawFd, FromRawFd};

    for _ in 0..128 {
        let id = next_temp_id();
        let temp_name = temp_file_name(destination_name, id);
        let temp_name = os_string_to_cstring(&temp_name)?;
        // SAFETY: the directory descriptor and CString are live; on success
        // ownership of the returned descriptor is transferred to File.
        let descriptor = unsafe {
            nix::libc::openat(
                parent.as_raw_fd(),
                temp_name.as_ptr(),
                nix::libc::O_WRONLY
                    | nix::libc::O_CREAT
                    | nix::libc::O_EXCL
                    | nix::libc::O_NOFOLLOW
                    | nix::libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor >= 0 {
            // SAFETY: openat returned a fresh, owned descriptor.
            return Ok((unsafe { File::from_raw_fd(descriptor) }, temp_name));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique atomic-write temporary file",
    ))
}

#[cfg(unix)]
fn rename_at(parent: &File, source: &std::ffi::CStr, destination: &OsStr) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let destination = os_string_to_cstring(destination)?;
    // SAFETY: both names are valid C strings and parent is a live directory
    // descriptor used for both sides of the same-filesystem replacement.
    let result = unsafe {
        nix::libc::renameat(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn create_parent_directories(parent: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
}

#[cfg(not(unix))]
fn create_parent_directories(parent: &Path) -> io::Result<()> {
    std::fs::create_dir_all(parent)
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    // Opening directories is not portable on every non-Unix target. The file
    // data is still synced before replacement there.
    Ok(())
}

/// Write `contents` durably and atomically replace `path`.
///
/// Temporary files are mode `0600` on Unix and are removed after every error
/// that occurs before the rename.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = destination_parent(path)?;
    create_parent_directories(parent)?;
    // Retain the validated directory descriptor through the replacement. This
    // rejects a static final-component symlink and lets the durability sync use
    // the same directory we inspected instead of resolving the path again.
    #[cfg(unix)]
    let parent_directory = open_parent_directory(parent)?;

    #[cfg(unix)]
    {
        let destination_name = path
            .file_name()
            .expect("destination_parent validates the file name");
        let (mut file, temp_name) = create_unique_temp_at(&parent_directory, destination_name)?;
        let mut cleanup = TempFileGuard::new(&parent_directory, temp_name);

        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        rename_at(&parent_directory, &cleanup.name, destination_name)?;
        cleanup.commit();

        // Persist the replacement directory entry, not just the new file data.
        parent_directory.sync_all()
    }
    #[cfg(not(unix))]
    {
        let (mut file, temp_path) = create_unique_temp(path, parent)?;
        let mut cleanup = TempFileGuard::new(temp_path.clone());

        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        std::fs::rename(&temp_path, path)?;
        cleanup.commit();
        sync_parent(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm-core-atomic-file-{label}-{}-{id}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomically_replaces_existing_contents() {
        let root = TestDir::new("replace");
        let destination = root.0.join("state.json");
        std::fs::write(&destination, b"old").unwrap();

        write_atomic(&destination, b"new state").unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"new state");
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_files_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("mode");
        let destination = root.0.join("private.json");
        write_atomic(&destination, b"secret").unwrap();

        let mode = std::fs::metadata(destination).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_final_parent_symlink_before_creating_a_temporary() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("parent-symlink");
        let victim = root.0.join("victim");
        let linked_parent = root.0.join("linked");
        std::fs::create_dir(&victim).unwrap();
        symlink(&victim, &linked_parent).unwrap();

        assert!(write_atomic(&linked_parent.join("state.json"), b"secret").is_err());
        assert_eq!(std::fs::read_dir(&victim).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn temporary_descriptors_are_close_on_exec() {
        use std::os::fd::AsRawFd;

        let root = TestDir::new("temp-cloexec");
        let destination = root.0.join("state.json");
        let parent = open_parent_directory(&root.0).unwrap();
        let (file, temp_name) =
            create_unique_temp_at(&parent, destination.file_name().unwrap()).unwrap();
        // SAFETY: `file` owns a live descriptor and F_GETFD only reads its flags.
        let flags = unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & nix::libc::FD_CLOEXEC, 0);
        drop(file);
        // SAFETY: parent and temp_name are live and identify the entry above.
        assert_eq!(
            unsafe { nix::libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0) },
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_nonsticky_shared_parent_without_leaving_a_temporary() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("unsafe-parent");
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        let destination = root.0.join("state.json");

        assert!(write_atomic(&destination, b"secret").is_err());
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn accepts_sticky_shared_parent_and_creates_missing_parents_privately() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("sticky-parent");
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let nested = root.0.join("private").join("nested");
        let destination = nested.join("state.json");

        write_atomic(&destination, b"secret").unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"secret");
        for directory in [root.0.join("private"), nested] {
            let mode = std::fs::metadata(directory).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_commit_cannot_be_redirected_by_parent_path_replacement() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("parent-replacement");
        let original = root.0.join("parent");
        let moved = root.0.join("moved-parent");
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = open_parent_directory(&original).unwrap();

        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();

        let destination = OsStr::new("state.json");
        let (mut file, temp_name) = create_unique_temp_at(&parent, destination).unwrap();
        let mut cleanup = TempFileGuard::new(&parent, temp_name);
        file.write_all(b"trusted").unwrap();
        file.sync_all().unwrap();
        drop(file);
        rename_at(&parent, &cleanup.name, destination).unwrap();
        cleanup.commit();
        parent.sync_all().unwrap();

        assert_eq!(std::fs::read(moved.join(destination)).unwrap(), b"trusted");
        assert_eq!(std::fs::read_dir(original).unwrap().count(), 0);
    }

    #[test]
    fn removes_temporary_file_when_rename_fails() {
        let root = TestDir::new("cleanup");
        let destination = root.0.join("occupied");
        std::fs::create_dir(&destination).unwrap();

        assert!(write_atomic(&destination, b"cannot replace a directory").is_err());

        let entries: Vec<_> = std::fs::read_dir(&root.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![OsString::from("occupied")]);
    }
}
