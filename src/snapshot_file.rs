//! Reading, retiring and replacing the family's on-disk session snapshots.
//!
//! Seeded from the three persistence layers that already disagreed about this:
//! the bounded read is the missing half of anvil `src/session.rs` (a plain
//! `fs::read_to_string` on a file chosen by directory scan) and frost
//! `src/session_persistence.rs::load` (the same); [`quarantine_corrupt`] is
//! lifted from ember `src/session_persistence.rs::quarantine_corrupt_snapshot`,
//! the only repo that had the idea; [`write_atomic_private`] is the union of
//! anvil `src/session.rs::atomic_write` + `ensure_private_directory`, anvil
//! `src/state.rs::atomic_write_private_file`, and frost's `save` (which was
//! `fs::write` + `rename`, so a crash between them could publish a truncated
//! snapshot). The durable-replacement mechanics themselves are not re-done here
//! — `crate::atomic_file::write_atomic` already had them right.
//!
//! The failure mode that motivated the module: a session snapshot is data the
//! terminal reads *at startup*, from a path it found by scanning a directory,
//! and then hands to `serde_json`. Every one of those reads was unbounded, so a
//! snapshot that had grown (a runaway writer, a filesystem full of another
//! program's output at a colliding name, a deliberately fattened file) is read
//! into memory in full before anything can reject it. Two of the three loaders
//! also wrote fresh state over a snapshot they had just failed to parse, which
//! destroys the evidence of the corruption and the user's tabs with it.
//!
//! Family decisions frozen here (do not relitigate per-app):
//!
//! - **The size bound is the caller's, and it is a hard rejection, not a
//!   truncation.** A truncated JSON document is a parse error whose message
//!   points at the wrong thing; [`read_bounded`] would rather say the file is
//!   too large. Snapshots are kilobytes, so any limit in the low megabytes is
//!   generous — the number stays per-app because only the app knows how many
//!   panes and how much scrollback metadata it persists.
//! - **Quarantine happens *before* the fresh write, never after the failed
//!   parse.** Ordering is the whole point: anvil and frost both retain a
//!   corrupt file and then save over it on the next autosave tick.
//! - **A snapshot directory is `0700` and a snapshot file is `0600`.** Restored
//!   argv can contain hostnames, remote paths and `docker exec` targets, and
//!   the cwd of every pane is a map of the user's filesystem. This is not
//!   configurable.
//! - **Only a directory this module *created* is chmodded by
//!   [`write_atomic_private`]; tightening one that already existed is the
//!   caller's explicit request, through `ensure_private_directory`.** Two of
//!   the four apps take the snapshot path from config (ember
//!   `session_history_file`, frost `session_history_path` — which also expands
//!   `~`), so the parent directory of a destination is not necessarily the
//!   app's own: it can be `$HOME` or `/tmp`. A `write_atomic_private` that
//!   tightened whatever parent it was handed would chmod `$HOME` to `0700` as a
//!   side effect of an autosave, and fail the whole save with `EPERM` on
//!   `/tmp`. The apps that do own their directory (anvil
//!   `~/.config/anvil`, anvil `~/.config/anvil/windows`) say so once.
//! - **Nothing here parses.** No `serde_json`, no snapshot types: the four
//!   apps' snapshot schemas differ (anvil/anvil `SavedSession`, frost
//!   `SessionsSnapshot`, ember its own), and folding a schema in here would
//!   make this module a versioning problem instead of an I/O one.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Open `path` for reading and prove it is a regular file.
///
/// The check is done with `fstat` on the open descriptor rather than `stat` on
/// the path, so nothing can swap the path for something else between the check
/// and the read.
fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A snapshot path replaced by a fifo would otherwise block `open` until
        // some writer appears, hanging whichever thread the restore runs on —
        // for anvil and anvil that is the GTK main thread, i.e. a window that
        // never draws. O_NONBLOCK lets the open return so the fstat below can
        // reject it, and is a no-op for the regular files this is really for.
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session snapshot path is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session snapshot must have exactly one hard link",
            ));
        }
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session snapshot is not owned by the current user",
            ));
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session snapshot is writable by another user or group",
            ));
        }
    }
    Ok(file)
}

/// Read a UTF-8 snapshot, refusing anything larger than `max_bytes`.
///
/// Directories, fifos, devices and sockets are rejected rather than read. The
/// `fstat` size is only a fast path for the common oversize case: the bound
/// that actually holds is [`Read::take`], because a file being appended to by
/// another process can pass the size check and then deliver more bytes than it
/// declared.
pub fn read_bounded(path: &Path, max_bytes: u64) -> io::Result<String> {
    let file = open_regular(path)?;
    read_bounded_file(path, file, max_bytes)
}

/// Read a bounded snapshot whose contents include private local metadata.
///
/// [`read_bounded`] rejects files writable by other users, which is enough for
/// integrity. Organism memory also contains absolute repository paths, so its
/// loader requires the stronger confidentiality invariant: no group/other
/// permission bits at all. The check is made on the same descriptor that is
/// read, avoiding a path-based check/open race.
pub(crate) fn read_bounded_private(path: &Path, max_bytes: u64) -> io::Result<String> {
    let file = open_regular(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = file.metadata()?;
        // open_regular already checked ownership and link count; retain the
        // explicit owner assertion here so this stronger contract stays true
        // if the shared helper's policy ever changes.
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private snapshot must be mode 0600 and owned by the current user",
            ));
        }
    }
    read_bounded_file(path, file, max_bytes)
}

fn read_bounded_file(path: &Path, file: File, max_bytes: u64) -> io::Result<String> {
    let declared_len = file.metadata()?.len();
    if declared_len > max_bytes {
        return Err(oversize_error(path, declared_len, max_bytes));
    }

    // Read one byte past the limit so a file that grew between the fstat and
    // here is *detected* instead of silently truncated into a parse error.
    let limit = max_bytes.saturating_add(1);
    let mut bytes = Vec::with_capacity(usize::try_from(declared_len.min(max_bytes)).unwrap_or(0));
    file.take(limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(oversize_error(path, bytes.len() as u64, max_bytes));
    }

    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("session snapshot {} is not valid UTF-8", path.display()),
        )
    })
}

fn oversize_error(path: &Path, actual: u64, max_bytes: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!(
            "session snapshot {} is {actual} bytes, over the {max_bytes}-byte limit",
            path.display()
        ),
    )
}

/// Highest number of same-millisecond quarantine attempts before giving up.
/// Matches ember's bound; a caller retrying a hundred times inside one
/// millisecond is looping, not making progress.
const MAX_QUARANTINE_ATTEMPTS: u32 = 100;

/// Move a malformed snapshot aside, returning the path it now lives at.
///
/// Call this *before* writing any fresh state, so a snapshot that failed to
/// parse survives for recovery instead of being overwritten by the next
/// autosave. On Unix a no-clobber hard-link/unlink move closes the existence
/// check race between concurrent quarantine attempts. `hard_link` acts on the
/// symlink entry itself rather than its target, so a symlink at `path` is still
/// retired without touching what it points to.
pub fn quarantine_corrupt(path: &Path) -> io::Result<PathBuf> {
    let file_type = fs::symlink_metadata(path)?.file_type();
    if !file_type.is_file() && !file_type.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to quarantine a non-file session snapshot path",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "session snapshot path has no file name",
        )
    })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    for attempt in 0..MAX_QUARANTINE_ATTEMPTS {
        let mut backup_name = file_name.to_os_string();
        backup_name.push(quarantine_suffix(timestamp, attempt));
        let backup = parent.join(backup_name);
        #[cfg(unix)]
        match fs::hard_link(path, &backup) {
            Ok(()) => match fs::remove_file(path) {
                Ok(()) => return Ok(backup),
                Err(error) => {
                    let _ = fs::remove_file(&backup);
                    return Err(error);
                }
            },
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
        #[cfg(not(unix))]
        {
            // symlink_metadata, not `exists()`: a dangling symlink at this name
            // reports "does not exist" and must not be overwritten.
            if fs::symlink_metadata(&backup).is_ok() {
                continue;
            }
            fs::rename(path, &backup)?;
            return Ok(backup);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique corrupt-snapshot backup name",
    ))
}

/// Suffix appended to a quarantined snapshot's name. Kept as one function
/// because both the naming and the test that proves the two consumers cannot
/// misread it have to agree on it.
fn quarantine_suffix(timestamp_millis: u128, attempt: u32) -> String {
    format!(
        ".corrupt-{timestamp_millis}-{}-{attempt}",
        std::process::id()
    )
}

/// Create `dir` (and any missing parents) and make `dir` itself private.
///
/// `create_dir_all`'s mode argument only applies to directories it actually
/// creates, so a snapshot directory left behind at `0755` by an older release —
/// or created under a looser umask — stays world-readable forever unless it is
/// tightened here. Only `dir` is tightened: existing *ancestors* such as
/// `~/.config` are shared with every other application and are not a terminal
/// emulator's to chmod.
pub fn ensure_private_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // Open the directory itself without following a final symlink, then
        // validate and chmod the descriptor. Path-based metadata/chmod here
        // would let a substituted parent symlink change an unrelated target's
        // permissions before the atomic writer even creates its temp file.
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(dir)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session snapshot parent is not a directory",
            ));
        }
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session snapshot parent is not owned by the current user",
            ));
        }
        // Only chmod when it would change something. Ownership was checked
        // above, so this cannot tighten an unrelated user's directory.
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

fn validate_existing_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(dir)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session snapshot parent is not a directory",
            ));
        }
        let mode = metadata.permissions().mode();
        // A foreign owner can replace entries even in a sticky directory.
        // Root ownership keeps explicitly configured `/tmp/...` snapshots
        // working while rejecting attacker-owned shared namespaces.
        // SAFETY: geteuid has no preconditions and only reads process state.
        let effective_uid = unsafe { nix::libc::geteuid() };
        if metadata.uid() != effective_uid && metadata.uid() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session snapshot parent is not owned by the current user or root",
            ));
        }
        if mode & 0o022 != 0 && mode & nix::libc::S_ISVTX == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "session snapshot parent is group/world writable without the sticky bit",
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        if fs::metadata(dir)?.is_dir() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session snapshot parent is not a directory",
            ))
        }
    }
}

/// Durably replace a snapshot file, creating its directory `0700` if needed.
///
/// The atomic replacement itself is `crate::atomic_file::write_atomic`, whose
/// temporary file is already `0600`, so the renamed-into-place snapshot is too.
/// An existing parent is validated without following a final symlink but is not
/// chmodded: configured snapshot paths may legitimately live directly under
/// `$HOME`, `/tmp`, or another directory the application does not own. Call
/// [`ensure_private_directory`] first when the application owns that directory
/// and explicitly wants to tighten a legacy installation.
pub fn write_atomic_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session snapshot path has no file name",
        ));
    }
    // A bare file name has no directory to make private. Treating it as "."
    // here would chmod the process's current working directory — someone's
    // project checkout — which is not this function's business.
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        match fs::symlink_metadata(parent) {
            Ok(_) => validate_existing_directory(parent)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ensure_private_directory(parent)?;
            }
            Err(error) => return Err(error),
        }
    }
    crate::atomic_file::write_atomic(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jterm-core-snapshot-file-{label}-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_private_fixture(path: &Path, contents: impl AsRef<[u8]>) {
        fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn reads_a_snapshot_that_fits() {
        let root = TestDir::new("read-ok");
        let path = root.join("tabs.state");
        write_private_fixture(&path, b"{\"tabs\":[]}");

        assert_eq!(read_bounded(&path, 4096).unwrap(), "{\"tabs\":[]}");
        // The limit is inclusive: a snapshot exactly at the bound is valid.
        assert_eq!(read_bounded(&path, 11).unwrap(), "{\"tabs\":[]}");
    }

    #[test]
    fn rejects_a_snapshot_over_the_limit() {
        let root = TestDir::new("read-big");
        let path = root.join("tabs.state");
        write_private_fixture(&path, vec![b'x'; 64]);

        let error = read_bounded(&path, 63).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert!(error.to_string().contains("64 bytes"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_never_follows_a_snapshot_symlink() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("read-symlink");
        let target = root.join("outside.json");
        let link = root.join("tabs.state");
        fs::write(&target, b"{\"tabs\":[]}").unwrap();
        symlink(&target, &link).unwrap();

        assert!(read_bounded(&link, 4096).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"{\"tabs\":[]}");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_rejects_a_multiply_linked_snapshot() {
        let root = TestDir::new("read-hard-link");
        let target = root.join("outside.json");
        let link = root.join("tabs.state");
        fs::write(&target, b"{\"tabs\":[]}").unwrap();
        fs::hard_link(&target, &link).unwrap();

        assert_eq!(
            read_bounded(&link, 4096).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(&target).unwrap(), b"{\"tabs\":[]}");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_rejects_a_snapshot_writable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("read-writable");
        let path = root.join("tabs.state");
        fs::write(&path, b"{\"tabs\":[]}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o622)).unwrap();

        assert_eq!(
            read_bounded(&path, 4096).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn take_bounds_the_read_even_when_the_size_check_passes() {
        // Simulates the file growing between the fstat and the read: a zero
        // limit means the fstat says "fits" for an empty file, so only `take`
        // and the post-read length check can reject content.
        let root = TestDir::new("read-grow");
        let path = root.join("tabs.state");
        write_private_fixture(&path, b"");
        assert_eq!(read_bounded(&path, 0).unwrap(), "");

        write_private_fixture(&path, b"x");
        assert_eq!(
            read_bounded(&path, 0).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
    }

    #[test]
    fn rejects_non_utf8_contents_as_invalid_data() {
        let root = TestDir::new("read-utf8");
        let path = root.join("tabs.state");
        write_private_fixture(&path, [0xffu8, 0xfe]);

        assert_eq!(
            read_bounded(&path, 4096).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_a_directory() {
        let root = TestDir::new("read-dir");
        let path = root.join("tabs.state");
        fs::create_dir(&path).unwrap();

        // A directory can fail either at open (EISDIR) or at the fstat check,
        // depending on the platform. What matters is that it never reads.
        assert!(read_bounded(&path, 4096).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_fifo_without_blocking() {
        let root = TestDir::new("read-fifo");
        let path = root.join("tabs.state");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `name` is a NUL-terminated path that lives across the call.
        let made = unsafe { nix::libc::mkfifo(name.as_ptr(), 0o600) };
        if made != 0 {
            // Some sandboxes forbid mkfifo; the directory case still covers the
            // non-regular rejection.
            return;
        }

        let error = read_bounded(&path, 4096).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn quarantine_keeps_the_original_bytes_recoverable() {
        let root = TestDir::new("quarantine");
        let path = root.join("tabs.state");
        fs::write(&path, b"{ truncated").unwrap();

        let moved = quarantine_corrupt(&path).unwrap();

        assert!(!path.exists(), "the corrupt snapshot must be moved aside");
        assert_eq!(fs::read(&moved).unwrap(), b"{ truncated");
        assert_eq!(moved.parent(), path.parent());
    }

    #[test]
    fn quarantine_does_not_clobber_an_existing_backup() {
        let root = TestDir::new("quarantine-twice");
        let path = root.join("tabs.state");

        fs::write(&path, b"first corruption").unwrap();
        let first = quarantine_corrupt(&path).unwrap();
        fs::write(&path, b"second corruption").unwrap();
        let second = quarantine_corrupt(&path).unwrap();

        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"first corruption");
        assert_eq!(fs::read(&second).unwrap(), b"second corruption");
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_quarantine_has_one_winner_without_clobbering_evidence() {
        use std::sync::{Arc, Barrier};

        let root = TestDir::new("quarantine-concurrent");
        let path = root.join("tabs.state");
        fs::write(&path, b"recover me").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    quarantine_corrupt(&path)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let winners = workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap().ok())
            .collect::<Vec<_>>();

        assert_eq!(winners.len(), 1);
        assert!(!path.exists());
        assert_eq!(fs::read(&winners[0]).unwrap(), b"recover me");
    }

    #[test]
    fn quarantine_refuses_a_directory() {
        let root = TestDir::new("quarantine-dir");
        let path = root.join("tabs.state");
        fs::create_dir(&path).unwrap();

        assert_eq!(
            quarantine_corrupt(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn quarantine_moves_a_symlink_instead_of_following_it() {
        let root = TestDir::new("quarantine-link");
        let target = root.join("real.state");
        let link = root.join("tabs.state");
        fs::write(&target, b"still here").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let moved = quarantine_corrupt(&link).unwrap();

        assert!(fs::symlink_metadata(&moved)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"still here");
        assert!(target.exists(), "the link's target must not be moved");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_creates_a_0600_file_under_a_0700_parent() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("write-modes");
        // Two directories deep, so the missing one has to be created too.
        let path = root.join("windows").join("nested").join("window-1.state");

        write_atomic_private(&path, b"{}").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"{}");
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        let parent_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent_mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn an_owned_pre_existing_directory_is_tightened_only_when_explicitly_requested() {
        use std::os::unix::fs::PermissionsExt;

        // The case `create_dir_all`'s mode argument cannot fix: the snapshot
        // directory already exists, group- and world-readable, from an older
        // release that used a plain create_dir_all.
        let root = TestDir::new("write-tighten");
        let directory = root.join("windows");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();

        write_atomic_private(&directory.join("window-1.state"), b"{}").unwrap();

        let untouched = fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
        assert_eq!(untouched, 0o755);

        ensure_private_directory(&directory).unwrap();

        let mode = fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_never_chmods_or_writes_through_a_parent_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = TestDir::new("write-parent-symlink");
        let victim = root.join("victim");
        let linked_parent = root.join("windows");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&victim, &linked_parent).unwrap();

        assert!(write_atomic_private(&linked_parent.join("window.state"), b"{}").is_err());
        assert!(!victim.join("window.state").exists());
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_does_not_chmod_an_existing_shared_parent() {
        use std::os::unix::fs::PermissionsExt;

        let parent = std::env::temp_dir();
        let path = parent.join(format!(
            "jterm-core-shared-parent-{}-{}.state",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&path);
        let before = fs::metadata(&parent).unwrap().permissions().mode();

        write_atomic_private(&path, b"{}").unwrap();

        let after = fs::metadata(&parent).unwrap().permissions().mode();
        assert_eq!(after, before);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_rejects_a_nonsticky_writable_parent() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("write-unsafe-parent");
        let parent = root.join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let path = parent.join("window.state");

        assert!(write_atomic_private(&path, b"{}").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn write_atomic_private_replaces_without_leaving_temporaries() {
        let root = TestDir::new("write-replace");
        let path = root.join("tabs.state");
        write_atomic_private(&path, b"old").unwrap();
        write_atomic_private(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    }

    #[test]
    fn write_atomic_private_rejects_a_path_without_a_file_name() {
        assert_eq!(
            write_atomic_private(Path::new("/"), b"{}")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    // -----------------------------------------------------------------------
    // The names this module puts in a snapshot directory must not be read back
    // as snapshots. Both consumers scan a directory and hand whatever they
    // accept to serde, so a temporary or quarantined file that parses as a
    // session is a restore of half-written state — or of state the user has
    // already been told was corrupt.
    // -----------------------------------------------------------------------

    /// Shape of anvil `src/session.rs::parse_state_file_name`, which accepts
    /// `tabs.state`, `tabs.<identity>.state` and either of those followed by
    /// `.claim.<identity>`. Reproduced structurally, not verbatim: only the
    /// prefix/suffix anchoring matters for this assertion.
    fn anvil_accepts_state_file_name(name: &str) -> bool {
        fn unclaimed(name: &str) -> bool {
            name == "tabs.state"
                || name
                    .strip_prefix("tabs.")
                    .and_then(|rest| rest.strip_suffix(".state"))
                    .is_some_and(|identity| !identity.is_empty())
        }
        match name.rsplit_once(".claim.") {
            Some((base, claimer)) => !claimer.is_empty() && unclaimed(base),
            None => unclaimed(name),
        }
    }

    /// Shape of anvil `src/state.rs::snapshots_with_extension`, which keeps a
    /// regular file whose *final* extension is exactly `state` or `active`.
    fn anvil_accepts_snapshot_name(name: &str) -> bool {
        Path::new(name)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension == "state" || extension == "active")
    }

    #[test]
    fn consumers_accept_the_real_snapshot_names() {
        // Guard the guard: a predicate that rejected everything would make the
        // assertions below vacuous.
        assert!(anvil_accepts_state_file_name("tabs.state"));
        assert!(anvil_accepts_state_file_name("tabs.7f3a.state"));
        assert!(anvil_accepts_state_file_name("tabs.7f3a.state.claim.9b1c"));
        assert!(anvil_accepts_snapshot_name("window-12.state"));
        assert!(anvil_accepts_snapshot_name("window-12.active"));
    }

    #[test]
    fn atomic_temporary_names_are_not_snapshots() {
        for destination in ["tabs.state", "tabs.7f3a.state", "window-12.active"] {
            let temp =
                crate::atomic_file::temp_file_name(OsString::from(destination).as_os_str(), 0)
                    .into_string()
                    .expect("temp names are ASCII for ASCII destinations");
            assert!(
                !anvil_accepts_state_file_name(&temp),
                "anvil would restore the temporary {temp}"
            );
            assert!(
                !anvil_accepts_snapshot_name(&temp),
                "anvil would restore the temporary {temp}"
            );
        }
    }

    #[test]
    fn quarantined_names_are_not_snapshots() {
        for destination in ["tabs.state", "tabs.7f3a.state", "window-12.active"] {
            let quarantined = format!("{destination}{}", quarantine_suffix(1_700_000_000_000, 0));
            assert!(
                !anvil_accepts_state_file_name(&quarantined),
                "anvil would restore the quarantined {quarantined}"
            );
            assert!(
                !anvil_accepts_snapshot_name(&quarantined),
                "anvil would restore the quarantined {quarantined}"
            );
        }
    }
}
