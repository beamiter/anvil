//! Persist structured block history as length-prefixed rkyv records.
//!
//! The in-memory deque is already bounded and seeded from this file, so saves
//! replace the file rather than append duplicate records.

use super::{BlockData, TermView};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_ENCODED_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_DECODED_RECORD_BYTES: usize = 16 * 1024 * 1024;
const MAX_HISTORY_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_HISTORY_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_HISTORY_FRAMES: usize = 100_000;
const MAX_HISTORY_DECODE_DURATION: Duration = Duration::from_secs(5);
const HISTORY_LOCK_TIMEOUT: Duration = Duration::from_millis(500);
const HISTORY_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryRevision {
    Missing,
    Present {
        device: u64,
        inode: u64,
        len: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
    },
}

impl HistoryRevision {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self::Present {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
}

static PANE_HISTORY_REVISIONS: OnceLock<Mutex<HashMap<(usize, PathBuf), HistoryRevision>>> =
    OnceLock::new();

fn pane_revisions() -> &'static Mutex<HashMap<(usize, PathBuf), HistoryRevision>> {
    PANE_HISTORY_REVISIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remembered_pane_revision(pane: usize, path: &Path) -> Option<HistoryRevision> {
    pane_revisions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(pane, path.to_path_buf()))
        .copied()
}

fn set_pane_revision(pane: usize, path: &Path, revision: Option<HistoryRevision>) {
    let mut revisions = pane_revisions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // A pane can switch configured targets at runtime. Retire its old target so
    // an allocator address reused by a later TermView cannot inherit it.
    revisions.retain(|(existing_pane, existing_path), _| {
        *existing_pane != pane || existing_path == path
    });
    let key = (pane, path.to_path_buf());
    if let Some(revision) = revision {
        revisions.insert(key, revision);
    } else {
        revisions.remove(&key);
    }
}

fn forget_pane_revisions(pane: usize) {
    pane_revisions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|(existing_pane, _), _| *existing_pane != pane);
}

/// Expand only the shell-style `~/` prefix used in configuration.
fn expand_home_prefix_with(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

fn history_path(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    expand_home_prefix_with(path, home.as_deref())
}

fn temp_file_name(target: &Path) -> io::Result<OsString> {
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("history path has no file name: {}", target.display()),
        )
    })?;
    let sequence = next_temp_id();
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    Ok(name)
}

fn next_temp_id() -> u64 {
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut random = [0_u8; std::mem::size_of::<u64>()];
    // SAFETY: random is a writable buffer of the exact supplied length;
    // nonblocking entropy failure falls through to a monotonic/time mix.
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
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    timestamp ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(std::process::id())
}

fn lock_file_name(target: &Path) -> io::Result<OsString> {
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("history path has no file name: {}", target.display()),
        )
    })?;
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(".lock");
    Ok(name)
}

fn parent_path(target: &Path) -> &Path {
    target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Create missing private directories, then retain a no-follow descriptor for
/// the final parent. Existing shared parents (for example `/tmp`) are never
/// chmodded as a side effect of enabling history.
fn open_parent_directory(target: &Path) -> io::Result<File> {
    let parent = parent_path(target);
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(parent)?;

    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("history parent is not a directory: {}", parent.display()),
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o022 != 0 && mode & nix::libc::S_ISVTX == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "history parent is group/world writable without the sticky bit: {}",
                parent.display()
            ),
        ));
    }
    Ok(directory)
}

fn validate_regular_user_file(file: &File, path: &Path, label: &str) -> io::Result<fs::Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} is not a regular file: {}", path.display()),
        ));
    }
    // SAFETY: `geteuid` has no preconditions and only reads process state.
    if metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{label} is not owned by the current user: {}",
                path.display()
            ),
        ));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{label} has multiple hard links: {}", path.display()),
        ));
    }
    Ok(metadata)
}

fn open_history_file(path: &Path) -> io::Result<Option<(File, fs::Metadata)>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = validate_regular_user_file(&file, path, "block history")?;
    if metadata.len() > MAX_HISTORY_FILE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!(
                "block history is {} bytes, exceeding the {} byte limit: {}",
                metadata.len(),
                MAX_HISTORY_FILE_BYTES,
                path.display()
            ),
        ));
    }
    Ok(Some((file, metadata)))
}

struct HistoryFileLock {
    directory: File,
    file: File,
}

fn lock_exclusive_bounded(file: &File, label: &str) -> io::Result<()> {
    let started = Instant::now();
    loop {
        // SAFETY: `file` owns a live descriptor for this call; flock retains no
        // pointer and the descriptor remains live for the guard's lifetime.
        let result =
            unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            if started.elapsed() >= HISTORY_LOCK_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for {label}"),
                ));
            }
            continue;
        }
        if error
            .raw_os_error()
            .is_some_and(|code| code == nix::libc::EAGAIN || code == nix::libc::EWOULDBLOCK)
        {
            let remaining = HISTORY_LOCK_TIMEOUT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for {label}"),
                ));
            }
            std::thread::sleep(HISTORY_LOCK_RETRY_INTERVAL.min(remaining));
            continue;
        }
        return Err(error);
    }
}

impl HistoryFileLock {
    fn acquire(target: &Path) -> io::Result<Self> {
        let parent_directory = open_parent_directory(target)?;
        // Lock the namespace before opening the named lock entry. A competing
        // process cannot bypass an existing guard merely by renaming that entry
        // and creating a new inode at the original pathname.
        lock_exclusive_bounded(&parent_directory, "block-history directory lock")?;
        let file = match (|| {
            let path = parent_path(target).join(lock_file_name(target)?);
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
                .open(&path)?;
            validate_regular_user_file(&file, &path, "block-history lock")?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            lock_exclusive_bounded(&file, "block-history file lock")?;
            Ok::<_, io::Error>(file)
        })() {
            Ok(file) => file,
            Err(error) => {
                // SAFETY: the descriptor remains live through this call.
                let _ =
                    unsafe { nix::libc::flock(parent_directory.as_raw_fd(), nix::libc::LOCK_UN) };
                return Err(error);
            }
        };
        Ok(Self {
            directory: parent_directory,
            file,
        })
    }
}

impl Drop for HistoryFileLock {
    fn drop(&mut self) {
        // Explicit unlock matters after `fork`: a child may inherit another
        // reference to this open-file description, but it must not extend this
        // logical critical section after the parent guard is dropped.
        // SAFETY: the descriptor is live until this method returns.
        if unsafe { nix::libc::flock(self.file.as_raw_fd(), nix::libc::LOCK_UN) } != 0 {
            log::warn!(
                "failed to unlock block-history file: {}",
                io::Error::last_os_error()
            );
        }
        // SAFETY: this descriptor is also live through the call.
        if unsafe { nix::libc::flock(self.directory.as_raw_fd(), nix::libc::LOCK_UN) } != 0 {
            log::warn!(
                "failed to unlock block-history directory: {}",
                io::Error::last_os_error()
            );
        }
    }
}

/// Write beside `target`, sync, then atomically rename over the previous file.
fn create_unique_temp(target: &Path) -> io::Result<(File, PathBuf)> {
    let parent = parent_path(target);
    for _ in 0..128 {
        let temp_path = parent.join(temp_file_name(target)?);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&temp_path)
        {
            Ok(file) => return Ok((file, temp_path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique block-history temporary file",
    ))
}

fn atomic_write(
    target: &Path,
    write_contents: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent_directory = open_parent_directory(target)?;

    let (mut temp, temp_path) = create_unique_temp(target)?;
    let result = (|| {
        temp.set_permissions(fs::Permissions::from_mode(0o600))?;
        write_contents(&mut temp)?;
        temp.flush()?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, target)?;
        parent_directory.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn push_bounded_back<T>(items: &mut VecDeque<T>, item: T, limit: usize) {
    if limit == 0 {
        return;
    }
    if items.len() == limit {
        items.pop_front();
    }
    items.push_back(item);
}

fn decode_record(data: &[u8], compressed: bool, max_decoded_bytes: usize) -> io::Result<Vec<u8>> {
    if !compressed {
        if data.len() > max_decoded_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("block history record exceeds {max_decoded_bytes} bytes"),
            ));
        }
        return Ok(data.to_vec());
    }

    let decoder = zstd::Decoder::new(data).map_err(|error| io::Error::other(error.to_string()))?;
    let mut decoded = Vec::new();
    decoder
        .take(max_decoded_bytes as u64 + 1)
        .read_to_end(&mut decoded)?;
    if decoded.len() > max_decoded_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("block history record expands beyond {max_decoded_bytes} bytes"),
        ));
    }
    Ok(decoded)
}

fn decode_block_record(data: &[u8], prefer_compressed: bool) -> io::Result<(BlockData, usize)> {
    let decode_as = |compressed| -> io::Result<(BlockData, usize)> {
        let decoded = decode_record(data, compressed, MAX_DECODED_RECORD_BYTES)?;
        let decoded_len = decoded.len();
        let block = rkyv::from_bytes::<BlockData, rkyv::rancor::Error>(&decoded)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok((block, decoded_len))
    };

    decode_as(prefer_compressed)
        .or_else(|_| decode_as(!prefer_compressed))
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "block-history frame is neither a valid raw nor bounded zstd BlockData record",
            )
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UndecodablePolicy {
    Skip,
    Reject,
}

struct LoadedRecords {
    blocks: VecDeque<BlockData>,
    total_loaded: usize,
    revision: HistoryRevision,
    fully_decoded: bool,
}

/// Read one four-byte frame header while preserving the distinction between a
/// clean frame-boundary EOF, a corrupt partial header, and a real I/O failure.
fn read_frame_header(file: &mut File, frame_index: usize) -> io::Result<Option<[u8; 4]>> {
    let mut header = [0u8; 4];
    let mut read = 0usize;
    while read < header.len() {
        match file.read(&mut header[read..]) {
            Ok(0) if read == 0 => return Ok(None),
            Ok(0) => {
                log::warn!(
                    "load_history: partial header for frame #{frame_index} ({read}/4 bytes)"
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("partial block-history frame header #{frame_index}: {read}/4 bytes"),
                ));
            }
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                log::warn!("load_history: I/O error reading frame #{frame_index} header: {error}");
                return Err(error);
            }
        }
    }
    Ok(Some(header))
}

fn read_frame_payload(file: &mut File, payload: &mut [u8], frame_index: usize) -> io::Result<()> {
    let mut read = 0usize;
    while read < payload.len() {
        match file.read(&mut payload[read..]) {
            Ok(0) => {
                log::warn!(
                    "load_history: partial payload for frame #{frame_index} ({read}/{} bytes)",
                    payload.len()
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "partial block-history frame payload #{frame_index}: {read}/{} bytes",
                        payload.len()
                    ),
                ));
            }
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                log::warn!("load_history: I/O error reading frame #{frame_index} payload: {error}");
                return Err(error);
            }
        }
    }
    Ok(())
}

fn read_history_records(
    path: &Path,
    prefer_compressed: bool,
    keep_limit: usize,
    undecodable_policy: UndecodablePolicy,
) -> io::Result<LoadedRecords> {
    let Some((mut file, metadata)) = open_history_file(path)? else {
        return Ok(LoadedRecords {
            blocks: VecDeque::new(),
            total_loaded: 0,
            revision: HistoryRevision::Missing,
            fully_decoded: true,
        });
    };
    let revision = HistoryRevision::from_metadata(&metadata);

    let mut blocks = VecDeque::new();
    let mut total_loaded = 0usize;
    let mut total_file_bytes = 0usize;
    let mut total_decoded_bytes = 0usize;
    let mut undecodable = 0usize;
    let mut frame_index = 0usize;
    let decode_started = Instant::now();
    loop {
        validate_history_progress(frame_index, decode_started.elapsed(), false)?;
        let Some(header) = read_frame_header(&mut file, frame_index)? else {
            break;
        };
        validate_history_progress(frame_index, decode_started.elapsed(), true)?;
        total_file_bytes = total_file_bytes.checked_add(header.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::FileTooLarge, "block-history size overflow")
        })?;
        let len = u32::from_le_bytes(header) as usize;
        if len > MAX_ENCODED_RECORD_BYTES {
            log::warn!(
                "load_history: frame #{frame_index} length {len} exceeds the {MAX_ENCODED_RECORD_BYTES} byte limit"
            );
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("oversized block-history frame #{frame_index}: {len} bytes"),
            ));
        }
        total_file_bytes = total_file_bytes.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::FileTooLarge, "block-history size overflow")
        })?;
        if total_file_bytes > MAX_HISTORY_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("block history exceeds {MAX_HISTORY_FILE_BYTES} bytes while reading"),
            ));
        }

        let mut payload = vec![0u8; len];
        read_frame_payload(&mut file, &mut payload, frame_index)?;
        match decode_block_record(&payload, prefer_compressed) {
            Ok((block, decoded_len)) => {
                total_decoded_bytes =
                    total_decoded_bytes
                        .checked_add(decoded_len)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::FileTooLarge,
                                "decoded block-history size overflow",
                            )
                        })?;
                if total_decoded_bytes > MAX_HISTORY_DECODED_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        format!("decoded block history exceeds {MAX_HISTORY_DECODED_BYTES} bytes"),
                    ));
                }
                total_loaded = total_loaded.saturating_add(1);
                push_bounded_back(&mut blocks, block, keep_limit);
            }
            Err(error) if undecodable_policy == UndecodablePolicy::Skip => {
                undecodable = undecodable.saturating_add(1);
                log::warn!(
                    "load_history: skipping undecodable frame #{frame_index} ({len} bytes): {error}"
                );
            }
            Err(error) => {
                log::warn!(
                    "save_history: refusing to replace history with undecodable frame #{frame_index}: {error}"
                );
                return Err(error);
            }
        }
        frame_index = frame_index.saturating_add(1);
    }

    if undecodable > 0 {
        log::warn!(
            "Skipped {undecodable} block-history record(s) this build cannot decode; they remain unchanged on disk"
        );
    }
    Ok(LoadedRecords {
        blocks,
        total_loaded,
        revision,
        fully_decoded: undecodable == 0,
    })
}

fn validate_history_progress(
    frame_index: usize,
    elapsed: Duration,
    has_frame: bool,
) -> io::Result<()> {
    if elapsed > MAX_HISTORY_DECODE_DURATION {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "block-history decode exceeded {:?}",
                MAX_HISTORY_DECODE_DURATION
            ),
        ));
    }
    if has_frame && frame_index >= MAX_HISTORY_FRAMES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("block history exceeds {MAX_HISTORY_FRAMES} frames"),
        ));
    }
    Ok(())
}

fn block_identity_hash(block: &BlockData) -> u64 {
    let mut hasher = DefaultHasher::new();
    block.id.hash(&mut hasher);
    block.prompt.hash(&mut hasher);
    block.cmd.hash(&mut hasher);
    block.cmd_markup.hash(&mut hasher);
    block.output.hash(&mut hasher);
    block.exit_code.hash(&mut hasher);
    block.line_count.hash(&mut hasher);
    block.start_time_ms.hash(&mut hasher);
    block.end_time_ms.hash(&mut hasher);
    block.duration_ms.hash(&mut hasher);
    block.cwd.hash(&mut hasher);
    block.cols.hash(&mut hasher);
    hasher.finish()
}

/// `estimated_height` is deliberately excluded: it is recomputed for the live
/// viewport after restore and is not part of a command block's identity.
fn same_block_identity(left: &BlockData, right: &BlockData) -> bool {
    left.id == right.id
        && left.prompt == right.prompt
        && left.cmd == right.cmd
        && left.cmd_markup == right.cmd_markup
        && left.output == right.output
        && left.exit_code == right.exit_code
        && left.line_count == right.line_count
        && left.start_time_ms == right.start_time_ms
        && left.end_time_ms == right.end_time_ms
        && left.duration_ms == right.duration_ms
        && left.cwd == right.cwd
        && left.cols == right.cols
}

fn deduplicate_newest(blocks: impl IntoIterator<Item = BlockData>) -> Vec<BlockData> {
    let mut newest_first = Vec::new();
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    let collected = blocks.into_iter().collect::<Vec<_>>();
    for block in collected.into_iter().rev() {
        let hash = block_identity_hash(&block);
        let duplicate = buckets.get(&hash).is_some_and(|indices| {
            indices
                .iter()
                .any(|&index| same_block_identity(&newest_first[index], &block))
        });
        if duplicate {
            continue;
        }
        let index = newest_first.len();
        newest_first.push(block);
        buckets.entry(hash).or_default().push(index);
    }
    newest_first.reverse();
    newest_first
}

/// Merge a potentially stale pane snapshot into the latest locked disk state.
/// Existing order is authoritative; matching incoming records update in place,
/// while genuinely new records append in incoming order. Thus a stale pane can
/// add its own commands without moving old records past commands saved by a
/// different pane in the meantime.
fn merge_stale_snapshot(
    existing: impl IntoIterator<Item = BlockData>,
    incoming: impl IntoIterator<Item = BlockData>,
) -> Vec<BlockData> {
    let mut merged = deduplicate_newest(existing);
    let incoming = deduplicate_newest(incoming);
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (index, block) in merged.iter().enumerate() {
        buckets
            .entry(block_identity_hash(block))
            .or_default()
            .push(index);
    }

    for block in incoming {
        let hash = block_identity_hash(&block);
        let existing_index = buckets.get(&hash).and_then(|indices| {
            indices
                .iter()
                .copied()
                .find(|&index| same_block_identity(&merged[index], &block))
        });
        if let Some(index) = existing_index {
            // Last writer supplies non-identity/derived fields without making a
            // stale record appear newer than concurrently persisted commands.
            merged[index] = block;
        } else {
            let index = merged.len();
            merged.push(block);
            buckets.entry(hash).or_default().push(index);
        }
    }
    merged
}

fn encode_history_frames_bounded(
    blocks: &[BlockData],
    compress: bool,
    max_encoded_record_bytes: usize,
    max_decoded_record_bytes: usize,
    max_history_bytes: usize,
    max_total_decoded_bytes: usize,
) -> io::Result<Vec<Vec<u8>>> {
    let mut newest_first = Vec::new();
    let mut encoded_bytes = 0usize;
    let mut decoded_bytes = 0usize;

    for block in blocks.iter().rev() {
        let serialized = rkyv::to_bytes::<rkyv::rancor::Error>(block)
            .map_err(|error| io::Error::other(error.to_string()))?;
        if serialized.len() > max_decoded_record_bytes {
            log::warn!(
                "save_history: skipping a {} byte block (decoded record limit {})",
                serialized.len(),
                max_decoded_record_bytes
            );
            continue;
        }
        let record = if compress {
            zstd::encode_all(serialized.as_slice(), 3)
                .map_err(|error| io::Error::other(error.to_string()))?
        } else {
            serialized.as_slice().to_vec()
        };
        if record.len() > max_encoded_record_bytes || record.len() > u32::MAX as usize {
            log::warn!(
                "save_history: skipping a {} byte block (encoded record limit {})",
                record.len(),
                max_encoded_record_bytes
            );
            continue;
        }

        let frame_bytes = 4usize.checked_add(record.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::FileTooLarge,
                "block-history frame size overflow",
            )
        })?;
        let Some(next_encoded) = encoded_bytes.checked_add(frame_bytes) else {
            break;
        };
        let Some(next_decoded) = decoded_bytes.checked_add(serialized.len()) else {
            break;
        };
        if next_encoded > max_history_bytes || next_decoded > max_total_decoded_bytes {
            log::warn!("save_history: retaining newest blocks within history byte budgets");
            break;
        }
        encoded_bytes = next_encoded;
        decoded_bytes = next_decoded;
        newest_first.push(record);
    }
    newest_first.reverse();
    Ok(newest_first)
}

struct SaveHistoryOutcome {
    revision: HistoryRevision,
    authoritative: bool,
}

fn save_history_snapshot(
    path: &Path,
    incoming: &[BlockData],
    compress: bool,
    expected_revision: Option<HistoryRevision>,
) -> io::Result<SaveHistoryOutcome> {
    let _lock = HistoryFileLock::acquire(path)?;
    // Strict decoding under the lock is essential: replacing a file containing
    // an unknown/corrupt frame would turn a recoverable read problem into data
    // loss. Normal UI loading may still skip such a frame and show the rest.
    let existing = read_history_records(path, compress, usize::MAX, UndecodablePolicy::Reject)?;
    let authoritative = expected_revision.is_some_and(|expected| expected == existing.revision);
    let merged = if authoritative {
        // The pane still owns the exact revision it loaded or last replaced, so
        // removals and Clear Blocks are intentional and remain effective.
        deduplicate_newest(incoming.iter().cloned())
    } else {
        // Another pane/process committed since this pane's baseline. Never let
        // stale absence delete those records; merge only additions/updates.
        merge_stale_snapshot(existing.blocks, incoming.iter().cloned())
    };
    let frames = encode_history_frames_bounded(
        &merged,
        compress,
        MAX_ENCODED_RECORD_BYTES,
        MAX_DECODED_RECORD_BYTES,
        MAX_HISTORY_FILE_BYTES,
        MAX_HISTORY_DECODED_BYTES,
    )?;
    atomic_write(path, |file| {
        for record in &frames {
            file.write_all(&(record.len() as u32).to_le_bytes())?;
            file.write_all(record)?;
        }
        Ok(())
    })?;
    let (_, metadata) = open_history_file(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("history disappeared after atomic write: {}", path.display()),
        )
    })?;
    Ok(SaveHistoryOutcome {
        revision: HistoryRevision::from_metadata(&metadata),
        authoritative,
    })
}

#[allow(dead_code)]
impl TermView {
    /// Retire pointer-keyed revision state before this TermView's allocation
    /// can be reused by a later pane.
    pub(super) fn forget_history_revision(&self) {
        forget_pane_revisions(self as *const Self as usize);
    }

    /// Save block history without risking truncation of the last good snapshot.
    pub fn save_history(&self) -> io::Result<()> {
        let (path_opt, compress) = {
            let config = self.config.borrow();
            (
                config.block_history_path.as_ref().cloned(),
                config.block_history_compress,
            )
        };
        let Some(path) = path_opt else {
            return Ok(());
        };

        let path = history_path(&path);
        let blocks = self.block_data.borrow().iter().cloned().collect::<Vec<_>>();
        let pane = self as *const Self as usize;
        let expected_revision = remembered_pane_revision(pane, &path);
        let outcome = save_history_snapshot(&path, &blocks, compress, expected_revision)?;
        // A stale merge deliberately does not grant this pane authority over
        // records it never loaded. It stays merge-only until a future reload.
        set_pane_revision(
            pane,
            &path,
            outcome.authoritative.then_some(outcome.revision),
        );
        Ok(())
    }

    /// Load only the configured number of most-recent history records.
    pub fn load_history(&self) -> io::Result<()> {
        let (path_opt, compress, lazy_load_threshold) = {
            let config = self.config.borrow();
            (
                config.block_history_path.as_ref().cloned(),
                config.block_history_compress,
                config.lazy_load_threshold as usize,
            )
        };
        let Some(path) = path_opt else {
            return Ok(());
        };

        let path = history_path(&path);
        let loaded = read_history_records(
            &path,
            compress,
            lazy_load_threshold,
            UndecodablePolicy::Skip,
        )?;
        let recent_blocks = loaded.blocks;
        let total_loaded = loaded.total_loaded;
        let pane = self as *const Self as usize;
        set_pane_revision(pane, &path, loaded.fully_decoded.then_some(loaded.revision));

        if total_loaded > recent_blocks.len() {
            log::info!(
                "Lazy loading history: keeping {} recent blocks out of {} total",
                recent_blocks.len(),
                total_loaded
            );
        }

        let start_index = total_loaded.saturating_sub(recent_blocks.len());
        let mut blocks = self.block_data.borrow_mut();
        for (offset, block) in recent_blocks.into_iter().enumerate() {
            log::debug!(
                "Loaded historical block #{}: prompt={:?}, cmd={:?}, output_len={}, exit_code={:?}",
                start_index + offset,
                block.prompt,
                block.cmd,
                block.output.len(),
                block.exit_code
            );
            blocks.push_back(block);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        atomic_write, decode_block_record, decode_record, encode_history_frames_bounded,
        expand_home_prefix_with, forget_pane_revisions, lock_file_name, push_bounded_back,
        read_history_records, remembered_pane_revision, save_history_snapshot, set_pane_revision,
        validate_history_progress, HistoryFileLock, HistoryRevision, UndecodablePolicy,
        MAX_HISTORY_DECODE_DURATION, MAX_HISTORY_FILE_BYTES, MAX_HISTORY_FRAMES,
    };
    use crate::block_view::BlockData;
    use std::collections::VecDeque;
    use std::ffi::CString;
    use std::fs;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "jterm1-history-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_block(id: u64, cmd: &str) -> BlockData {
        BlockData {
            id,
            prompt: "$ ".into(),
            cmd: cmd.into(),
            cmd_markup: None,
            output: format!("output for {cmd}\n"),
            exit_code: Some(0),
            estimated_height: 2,
            line_count: 1,
            start_time_ms: Some(1_000 + id),
            end_time_ms: Some(2_000 + id),
            duration_ms: Some(1_000),
            cwd: Some("/tmp".into()),
            cols: 80,
        }
    }

    fn read_commands(path: &Path) -> Vec<String> {
        read_history_records(path, false, usize::MAX, UndecodablePolicy::Reject)
            .unwrap()
            .blocks
            .into_iter()
            .map(|block| block.cmd)
            .collect()
    }

    #[test]
    fn push_bounded_back_keeps_only_recent_items() {
        let mut items = VecDeque::new();
        for item in 0..5 {
            push_bounded_back(&mut items, item, 3);
        }
        assert_eq!(items.into_iter().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn push_bounded_back_honors_zero_limit() {
        let mut items = VecDeque::new();
        push_bounded_back(&mut items, 1, 0);
        assert!(items.is_empty());
    }

    #[test]
    fn history_decode_has_frame_and_wall_clock_budgets() {
        assert!(validate_history_progress(
            MAX_HISTORY_FRAMES - 1,
            MAX_HISTORY_DECODE_DURATION,
            true
        )
        .is_ok());
        assert_eq!(
            validate_history_progress(MAX_HISTORY_FRAMES, Duration::ZERO, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            validate_history_progress(
                0,
                MAX_HISTORY_DECODE_DURATION + Duration::from_nanos(1),
                false
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn expands_only_home_slash_prefix() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_home_prefix_with("~/.local/share/jterm1/history", Some(home)),
            home.join(".local/share/jterm1/history")
        );
        assert_eq!(expand_home_prefix_with("~", Some(home)), PathBuf::from("~"));
        assert_eq!(
            expand_home_prefix_with("~other/history", Some(home)),
            PathBuf::from("~other/history")
        );
        assert_eq!(
            expand_home_prefix_with("cache/~/history", Some(home)),
            PathBuf::from("cache/~/history")
        );
    }

    #[test]
    fn atomic_write_creates_parent_directories_and_replaces_file() {
        let dir = TestDir::new("replace");
        let target = dir.path().join("nested/deeper/history.bin");
        atomic_write(&target, |file| {
            use std::io::Write as _;
            file.write_all(b"first")
        })
        .unwrap();
        atomic_write(&target, |file| {
            use std::io::Write as _;
            file.write_all(b"second")
        })
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
    }

    #[test]
    fn atomic_write_rejects_final_parent_symlink() {
        let dir = TestDir::new("parent-symlink");
        let real_parent = dir.path().join("real");
        let linked_parent = dir.path().join("linked");
        fs::create_dir(&real_parent).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();

        let error = atomic_write(&linked_parent.join("history.bin"), |file| {
            use std::io::Write as _;
            file.write_all(b"must-not-land")
        })
        .unwrap_err();
        assert!(matches!(
            error.raw_os_error(),
            Some(code) if code == nix::libc::ELOOP || code == nix::libc::ENOTDIR
        ));
        assert!(!real_parent.join("history.bin").exists());
    }

    #[test]
    fn atomic_temp_descriptor_is_close_on_exec() {
        let dir = TestDir::new("temp-cloexec");
        let target = dir.path().join("history.bin");
        atomic_write(&target, |file| {
            // SAFETY: F_GETFD only inspects the live descriptor.
            let flags = unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & nix::libc::FD_CLOEXEC, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn failed_atomic_write_preserves_previous_file_and_cleans_temp() {
        let dir = TestDir::new("failure");
        let target = dir.path().join("history.bin");
        fs::write(&target, b"last-good").unwrap();

        let error = atomic_write(&target, |file| {
            use std::io::Write as _;
            file.write_all(b"partial")?;
            Err(io::Error::other("simulated encoder failure"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&target).unwrap(), b"last-good");
        let entries = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![target.file_name().unwrap()]);
    }

    #[test]
    fn compressed_record_decode_enforces_output_limit() {
        let compressed = zstd::encode_all(&b"0123456789abcdef"[..], 1).unwrap();
        let error = decode_record(&compressed, true, 8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn history_reader_rejects_symlink_without_touching_target() {
        let dir = TestDir::new("history-symlink");
        let victim = dir.path().join("victim");
        let history = dir.path().join("history.bin");
        fs::write(&victim, b"not history").unwrap();
        symlink(&victim, &history).unwrap();

        let error = read_history_records(&history, false, 10, UndecodablePolicy::Reject)
            .err()
            .expect("history symlink must be rejected");
        assert!(matches!(
            error.raw_os_error(),
            Some(code) if code == nix::libc::ELOOP
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"not history");
    }

    #[test]
    fn history_reader_rejects_fifo_without_blocking() {
        let dir = TestDir::new("history-fifo");
        let history = dir.path().join("history.fifo");
        let history_c = CString::new(history.as_os_str().as_bytes()).unwrap();
        // SAFETY: the CString is NUL-terminated and points to a valid path for
        // the duration of this call.
        assert_eq!(unsafe { nix::libc::mkfifo(history_c.as_ptr(), 0o600) }, 0);

        let error = read_history_records(&history, false, 10, UndecodablePolicy::Reject)
            .err()
            .expect("history FIFO must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn history_reader_rejects_multiply_linked_file() {
        let dir = TestDir::new("history-hardlink");
        let history = dir.path().join("history.bin");
        let alias = dir.path().join("alias.bin");
        fs::write(&history, []).unwrap();
        fs::hard_link(&history, &alias).unwrap();

        let error = read_history_records(&history, false, 10, UndecodablePolicy::Reject)
            .err()
            .expect("multiply linked history must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn history_reader_rejects_file_over_total_limit_before_scanning() {
        let dir = TestDir::new("history-oversize");
        let history = dir.path().join("history.bin");
        let file = fs::File::create(&history).unwrap();
        file.set_len(MAX_HISTORY_FILE_BYTES as u64 + 1).unwrap();

        let error = read_history_records(&history, false, 10, UndecodablePolicy::Reject)
            .err()
            .expect("oversized history must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn clean_eof_is_valid_but_partial_header_is_corruption() {
        let dir = TestDir::new("history-eof");
        let history = dir.path().join("history.bin");
        fs::write(&history, []).unwrap();
        let empty = read_history_records(&history, false, 10, UndecodablePolicy::Reject).unwrap();
        assert_eq!(empty.total_loaded, 0);

        fs::write(&history, [1, 0]).unwrap();
        let error = read_history_records(&history, false, 10, UndecodablePolicy::Reject)
            .err()
            .expect("partial header must be reported");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("partial"));
    }

    #[test]
    fn save_refuses_to_replace_partial_existing_history() {
        let dir = TestDir::new("history-preserve-corrupt");
        let history = dir.path().join("history.bin");
        let corrupt = [8, 0, 0, 0, 1, 2, 3];
        fs::write(&history, corrupt).unwrap();

        let error = save_history_snapshot(
            &history,
            &[sample_block(1, "new")],
            false,
            Some(HistoryRevision::Missing),
        )
        .err()
        .expect("partial existing history must block replacement");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&history).unwrap(), corrupt);
    }

    #[test]
    fn writer_budget_keeps_newest_complete_frames_in_original_order() {
        let blocks = vec![
            sample_block(1, "oldest"),
            sample_block(2, "middle"),
            sample_block(3, "newest"),
        ];
        let all = encode_history_frames_bounded(
            &blocks,
            false,
            usize::MAX,
            usize::MAX,
            usize::MAX,
            usize::MAX,
        )
        .unwrap();
        let newest_two_budget = 8 + all[1].len() + all[2].len();
        let kept = encode_history_frames_bounded(
            &blocks,
            false,
            usize::MAX,
            usize::MAX,
            newest_two_budget,
            usize::MAX,
        )
        .unwrap();
        let commands = kept
            .iter()
            .map(|frame| decode_block_record(frame, false).unwrap().0.cmd)
            .collect::<Vec<_>>();
        assert_eq!(commands, ["middle", "newest"]);
        assert!(kept.iter().all(|frame| frame.len() <= u32::MAX as usize));
    }

    #[test]
    fn concurrent_stale_writers_merge_without_losing_new_records() {
        let dir = TestDir::new("history-stale-writers");
        let history = dir.path().join("history.bin");
        let original = sample_block(1, "original");
        let initial = save_history_snapshot(
            &history,
            std::slice::from_ref(&original),
            false,
            Some(HistoryRevision::Missing),
        )
        .unwrap();
        assert!(initial.authoritative);

        let barrier = Arc::new(Barrier::new(3));
        let spawn_writer = |new_block: BlockData| {
            let barrier = Arc::clone(&barrier);
            let history = history.clone();
            let original = original.clone();
            thread::spawn(move || {
                let stale_snapshot = [original, new_block];
                barrier.wait();
                save_history_snapshot(&history, &stale_snapshot, false, Some(initial.revision))
                    .unwrap()
            })
        };
        let first = spawn_writer(sample_block(2, "from-first-pane"));
        let second = spawn_writer(sample_block(3, "from-second-pane"));
        barrier.wait();
        let first_outcome = first.join().unwrap();
        let second_outcome = second.join().unwrap();
        assert_ne!(first_outcome.authoritative, second_outcome.authoritative);

        let commands = read_commands(&history);
        assert_eq!(commands.first().map(String::as_str), Some("original"));
        assert_eq!(commands.len(), 3);
        assert!(commands.iter().any(|command| command == "from-first-pane"));
        assert!(commands.iter().any(|command| command == "from-second-pane"));
    }

    #[test]
    fn history_directory_lock_is_bounded_and_survives_lock_entry_replacement() {
        let dir = TestDir::new("history-lock-replacement");
        let history = dir.path().join("history.bin");
        let guard = HistoryFileLock::acquire(&history).unwrap();
        let lock = dir.path().join(lock_file_name(&history).unwrap());
        fs::rename(&lock, dir.path().join("retired.lock")).unwrap();

        let contender_path = history.clone();
        let started = Instant::now();
        let error = thread::spawn(move || {
            HistoryFileLock::acquire(&contender_path)
                .err()
                .expect("contender must time out")
        })
        .join()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(400));
        assert!(started.elapsed() < Duration::from_secs(2));

        drop(guard);
        assert!(HistoryFileLock::acquire(&history).is_ok());
    }

    #[test]
    fn nonsticky_writable_history_parent_is_rejected_before_creating_files() {
        let dir = TestDir::new("history-shared-parent");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let history = dir.path().join("history.bin");

        assert!(HistoryFileLock::acquire(&history).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn retired_pane_cannot_leak_revision_authority_to_a_reused_address() {
        let pane = usize::MAX - 7;
        let path = Path::new("/tmp/jterm1-history-revision-test");
        set_pane_revision(pane, path, Some(HistoryRevision::Missing));
        assert_eq!(
            remembered_pane_revision(pane, path),
            Some(HistoryRevision::Missing)
        );
        forget_pane_revisions(pane);
        assert_eq!(remembered_pane_revision(pane, path), None);
    }
}
