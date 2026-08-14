//! Persist structured block history as length-prefixed rkyv records.
//!
//! The in-memory deque is already bounded and seeded from this file, so saves
//! replace the file rather than append duplicate records.

use super::zone_history;
use super::{mutate_block_data_and_redraw, BlockData, TermView};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
const CLEAR_TOMBSTONE_MAGIC: &[u8] = b"ANVIL-BLOCK-HISTORY-CLEAR-V1\0";
const CLEAR_TOMBSTONE_FRAME_BYTES: usize = 4 + CLEAR_TOMBSTONE_MAGIC.len() + 16;

fn encode_clear_tombstone(token: u128) -> Vec<u8> {
    let mut frame = Vec::with_capacity(CLEAR_TOMBSTONE_MAGIC.len() + 16);
    frame.extend_from_slice(CLEAR_TOMBSTONE_MAGIC);
    frame.extend_from_slice(&token.to_le_bytes());
    frame
}

fn decode_clear_tombstone(frame: &[u8]) -> Option<u128> {
    let token = frame.strip_prefix(CLEAR_TOMBSTONE_MAGIC)?;
    let token: [u8; 16] = token.try_into().ok()?;
    Some(u128::from_le_bytes(token))
}

fn new_clear_tombstone() -> u128 {
    (u128::from(next_temp_id()) << 64) | u128::from(next_temp_id())
}

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

/// Fully resolved persistence target. Explicit Clear authority is bound to
/// both fields so a later config reload cannot silently redirect deletion to a
/// different file or rewrite the original target with a different codec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryTarget {
    path: PathBuf,
    compress: bool,
}

/// Per-pane observation of one resolved history path. This value moves with
/// `TermView`; unlike the former pointer-keyed process-global maps, allocator
/// address reuse can neither inherit nor erase another pane's authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryBaseline {
    revision: Option<HistoryRevision>,
    clear_tombstone: Option<u128>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedHistorySave {
    target: HistoryTarget,
    explicit_replace: bool,
}

fn configured_history_target(path: Option<&str>, compress: bool) -> Option<HistoryTarget> {
    path.map(|path| HistoryTarget {
        path: history_path(path),
        compress,
    })
}

fn baseline_observation(
    baselines: &HashMap<PathBuf, HistoryBaseline>,
    path: &Path,
) -> (Option<HistoryRevision>, Option<u128>) {
    baselines.get(path).map_or((None, None), |baseline| {
        (baseline.revision, baseline.clear_tombstone)
    })
}

fn replace_history_baseline(
    baselines: &RefCell<HashMap<PathBuf, HistoryBaseline>>,
    path: PathBuf,
    revision: Option<HistoryRevision>,
    clear_tombstone: Option<u128>,
) {
    baselines.borrow_mut().insert(
        path,
        HistoryBaseline {
            revision,
            clear_tombstone,
        },
    );
}

/// Armed Clears are always first and retain user order. Once all succeed, a
/// changed configured target is saved normally in the same call so new
/// post-switch work is not stranded. Exact targets already cleared are not
/// written twice.
fn plan_history_saves(
    pending: &VecDeque<HistoryTarget>,
    configured: Option<HistoryTarget>,
) -> Vec<PlannedHistorySave> {
    let mut saves = Vec::with_capacity(pending.len().saturating_add(1));
    for pending in pending {
        saves.push(PlannedHistorySave {
            target: pending.clone(),
            explicit_replace: true,
        });
    }
    if let Some(target) = configured.filter(|target| !pending.contains(target)) {
        saves.push(PlannedHistorySave {
            target,
            explicit_replace: false,
        });
    }
    saves
}

fn enqueue_pending_clear(
    pending: &RefCell<VecDeque<HistoryTarget>>,
    target: Option<HistoryTarget>,
) {
    let Some(target) = target else {
        return;
    };
    let mut pending = pending.borrow_mut();
    if !pending.contains(&target) {
        pending.push_back(target);
    }
}

fn consume_succeeded_pending(
    pending: &RefCell<VecDeque<HistoryTarget>>,
    succeeded: &HistoryTarget,
) -> bool {
    let mut pending = pending.borrow_mut();
    if pending.front() == Some(succeeded) {
        pending.pop_front();
        true
    } else {
        false
    }
}

fn replace_reserved_history_ids(reserved: &RefCell<HashSet<u64>>, seen_ids: HashSet<u64>) {
    // The scanner admits at most one decoded id per bounded history frame.
    debug_assert!(seen_ids.len() <= MAX_HISTORY_FRAMES);
    *reserved.borrow_mut() = seen_ids;
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

#[cfg(test)]
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
    fully_retained: bool,
    /// Every successfully decoded id in the bounded scan, including records
    /// omitted from the resident tail. Returned to the UI loader without any
    /// process-global side effect; strict save rereads simply discard it.
    seen_ids: Option<HashSet<u64>>,
    clear_tombstone: Option<u128>,
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
    read_history_records_with_options(
        path,
        prefer_compressed,
        keep_limit,
        undecodable_policy,
        None,
        false,
    )
}

fn read_history_records_with_retained_budget(
    path: &Path,
    prefer_compressed: bool,
    keep_limit: usize,
    undecodable_policy: UndecodablePolicy,
    retained_byte_limit: Option<usize>,
) -> io::Result<LoadedRecords> {
    read_history_records_with_options(
        path,
        prefer_compressed,
        keep_limit,
        undecodable_policy,
        retained_byte_limit,
        true,
    )
}

fn read_history_records_with_options(
    path: &Path,
    prefer_compressed: bool,
    keep_limit: usize,
    undecodable_policy: UndecodablePolicy,
    retained_byte_limit: Option<usize>,
    collect_seen_ids: bool,
) -> io::Result<LoadedRecords> {
    let Some((mut file, metadata)) = open_history_file(path)? else {
        return Ok(LoadedRecords {
            blocks: VecDeque::new(),
            total_loaded: 0,
            revision: HistoryRevision::Missing,
            fully_decoded: true,
            fully_retained: true,
            seen_ids: collect_seen_ids.then(HashSet::new),
            clear_tombstone: None,
        });
    };
    let revision = HistoryRevision::from_metadata(&metadata);

    let mut blocks: VecDeque<BlockData> = VecDeque::new();
    let mut retained_costs: VecDeque<usize> = VecDeque::new();
    let mut retained_estimated_bytes = 0usize;
    let mut retained_records_dropped = false;
    let mut total_loaded = 0usize;
    let mut total_file_bytes = 0usize;
    let mut total_decoded_bytes = 0usize;
    let mut undecodable = 0usize;
    let mut frame_index = 0usize;
    let mut seen_ids = collect_seen_ids.then(HashSet::new);
    let mut duplicate_ids = 0usize;
    let mut clear_tombstone = None;
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
        if let Some(token) = decode_clear_tombstone(&payload) {
            // A tombstone is an atomic persisted clear barrier. Any frames
            // before it are logically obsolete (writers normally place it
            // first, but this ordering rule also makes recovery deterministic).
            clear_tombstone = Some(token);
            blocks.clear();
            retained_costs.clear();
            retained_estimated_bytes = 0;
            total_loaded = 0;
            retained_records_dropped = false;
            if let Some(seen_ids) = seen_ids.as_mut() {
                seen_ids.clear();
            }
            duplicate_ids = 0;
            frame_index = frame_index.saturating_add(1);
            continue;
        }
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
                let duplicate_id = seen_ids
                    .as_mut()
                    .is_some_and(|seen_ids| !seen_ids.insert(block.id));
                if duplicate_id {
                    duplicate_ids = duplicate_ids.saturating_add(1);
                    retained_records_dropped = true;
                    // Runtime lists are keyed by id for selection, deletion,
                    // bookmarks, and eviction. Keep only the newest occurrence
                    // so two processes that emitted the same legacy id cannot
                    // break those parallel structures. This is not a decode
                    // error: Explicit Clear must still be able to recover.
                    if let Some(index) = blocks.iter().position(|existing| existing.id == block.id)
                    {
                        blocks.remove(index);
                        if let Some(cost) = retained_costs.remove(index) {
                            retained_estimated_bytes =
                                retained_estimated_bytes.saturating_sub(cost);
                        }
                    }
                }
                let retained_cost = block.estimated_restored_retained_bytes();
                if keep_limit == 0 {
                    retained_records_dropped = true;
                } else {
                    while blocks.len() >= keep_limit {
                        blocks.pop_front();
                        if let Some(cost) = retained_costs.pop_front() {
                            retained_estimated_bytes =
                                retained_estimated_bytes.saturating_sub(cost);
                        }
                        retained_records_dropped = true;
                    }
                    if let Some(byte_limit) = retained_byte_limit {
                        while !blocks.is_empty()
                            && retained_estimated_bytes
                                .checked_add(retained_cost)
                                .is_none_or(|next| next > byte_limit)
                        {
                            blocks.pop_front();
                            if let Some(cost) = retained_costs.pop_front() {
                                retained_estimated_bytes =
                                    retained_estimated_bytes.saturating_sub(cost);
                            }
                            retained_records_dropped = true;
                        }
                    }
                    retained_estimated_bytes =
                        retained_estimated_bytes.saturating_add(retained_cost);
                    retained_costs.push_back(retained_cost);
                    blocks.push_back(block);
                }
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
    if duplicate_ids > 0 {
        log::warn!(
            "Collapsed {duplicate_ids} duplicate block-history id record(s), keeping each newest occurrence; ordinary deletion authority is revoked"
        );
    }
    Ok(LoadedRecords {
        blocks,
        total_loaded,
        revision,
        fully_decoded: undecodable == 0,
        fully_retained: !retained_records_dropped,
        seen_ids,
        clear_tombstone,
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
    max_frames: usize,
    max_history_bytes: usize,
    max_total_decoded_bytes: usize,
) -> io::Result<Vec<Vec<u8>>> {
    let mut newest_first = Vec::new();
    let mut encoded_bytes = 0usize;
    let mut decoded_bytes = 0usize;

    for block in blocks.iter().rev() {
        if newest_first.len() == max_frames {
            break;
        }
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

#[derive(Debug)]
struct SaveHistoryOutcome {
    revision: HistoryRevision,
    authoritative: bool,
    clear_tombstone: Option<u128>,
}

#[derive(Clone, Copy)]
enum SaveIntent {
    Revision {
        expected_revision: Option<HistoryRevision>,
        observed_clear_tombstone: Option<u128>,
    },
    /// User-invoked Clear Blocks deliberately replaces the complete on-disk
    /// set, even when count/byte-bounded startup restore withheld ordinary
    /// deletion authority from this pane.
    ExplicitReplace,
}

fn save_history_snapshot(
    path: &Path,
    incoming: &[BlockData],
    compress: bool,
    expected_revision: Option<HistoryRevision>,
) -> io::Result<SaveHistoryOutcome> {
    save_history_snapshot_with_intent(
        path,
        incoming,
        compress,
        SaveIntent::Revision {
            expected_revision,
            observed_clear_tombstone: None,
        },
    )
}

fn save_history_snapshot_with_intent(
    path: &Path,
    incoming: &[BlockData],
    compress: bool,
    intent: SaveIntent,
) -> io::Result<SaveHistoryOutcome> {
    let _lock = HistoryFileLock::acquire(path)?;
    // Strict decoding under the lock is essential: replacing a file containing
    // an unknown/corrupt frame would turn a recoverable read problem into data
    // loss. Normal UI loading may still skip such a frame and show the rest.
    let existing = read_history_records(path, compress, usize::MAX, UndecodablePolicy::Reject)?;
    let (authoritative, clear_tombstone) = match intent {
        SaveIntent::Revision {
            expected_revision,
            observed_clear_tombstone,
        } => {
            if observed_clear_tombstone != existing.clear_tombstone {
                // The stale snapshot can contain both inherited pre-Clear
                // records and genuinely new work. This schema has no per-record
                // baseline provenance, so it cannot separate them safely: fail
                // the whole full-output snapshot rather than resurrect cleared
                // data. Command-only JSONL persistence is independent.
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "block history was explicitly cleared after this pane's snapshot; refusing to resurrect stale records",
                ));
            }
            (
                expected_revision.is_some_and(|expected| expected == existing.revision),
                existing.clear_tombstone,
            )
        }
        SaveIntent::ExplicitReplace => (true, Some(new_clear_tombstone())),
    };
    let merged = if authoritative {
        // The pane still owns the exact revision it loaded or last replaced, so
        // removals and Clear Blocks are intentional and remain effective.
        deduplicate_newest(incoming.iter().cloned())
    } else {
        // Another pane/process committed since this pane's baseline. Never let
        // stale absence delete those records; merge only additions/updates.
        merge_stale_snapshot(existing.blocks, incoming.iter().cloned())
    };
    let history_bytes = if clear_tombstone.is_some() {
        MAX_HISTORY_FILE_BYTES.saturating_sub(CLEAR_TOMBSTONE_FRAME_BYTES)
    } else {
        MAX_HISTORY_FILE_BYTES
    };
    let record_frames = MAX_HISTORY_FRAMES.saturating_sub(usize::from(clear_tombstone.is_some()));
    let frames = encode_history_frames_bounded(
        &merged,
        compress,
        MAX_ENCODED_RECORD_BYTES,
        MAX_DECODED_RECORD_BYTES,
        record_frames,
        history_bytes,
        MAX_HISTORY_DECODED_BYTES,
    )?;
    atomic_write(path, |file| {
        if let Some(token) = clear_tombstone {
            let tombstone = encode_clear_tombstone(token);
            file.write_all(&(tombstone.len() as u32).to_le_bytes())?;
            file.write_all(&tombstone)?;
        }
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
        clear_tombstone,
    })
}

fn execute_history_saves(
    baselines: &RefCell<HashMap<PathBuf, HistoryBaseline>>,
    pending: &RefCell<VecDeque<HistoryTarget>>,
    blocks: &[BlockData],
    saves: Vec<PlannedHistorySave>,
) -> io::Result<()> {
    for save in saves {
        let intent = if save.explicit_replace {
            SaveIntent::ExplicitReplace
        } else {
            let (expected_revision, observed_clear_tombstone) = {
                let baselines = baselines.borrow();
                baseline_observation(&baselines, &save.target.path)
            };
            SaveIntent::Revision {
                expected_revision,
                observed_clear_tombstone,
            }
        };
        let outcome = save_history_snapshot_with_intent(
            &save.target.path,
            blocks,
            save.target.compress,
            intent,
        )?;
        // A stale merge deliberately does not grant this pane deletion
        // authority over records it never loaded. The path still retains its
        // observed tombstone so a later Clear cannot be bypassed.
        replace_history_baseline(
            baselines,
            save.target.path.clone(),
            outcome.authoritative.then_some(outcome.revision),
            outcome.clear_tombstone,
        );
        if save.explicit_replace {
            // Consume only after this exact bound target was atomically
            // replaced. Failure above leaves it armed for Undo or Drop.
            if !consume_succeeded_pending(pending, &save.target) {
                return Err(io::Error::other(
                    "completed history Clear target no longer matches the pending queue",
                ));
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
impl TermView {
    /// Bind deletion authority to the target selected when the user invoked
    /// Clear. Older failed targets stay first; exact repeats are coalesced.
    pub(super) fn arm_history_explicit_replace(&self) {
        let target = {
            let config = self.config.borrow();
            configured_history_target(
                config.block_history_path.as_deref(),
                config.block_history_compress,
            )
        };
        enqueue_pending_clear(&self.history_explicit_replace_pending, target);
    }

    /// Where this pane's bounded zone document lives: a sibling of the Block
    /// history file, distinct by stem so a pane that changes mode between runs
    /// can never decode one as the other.
    fn zone_history_path(&self) -> Option<PathBuf> {
        let configured = self.config.borrow().block_history_path.as_ref().cloned()?;
        let base = history_path(&configured);
        let stem = base
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "blocks".to_string());
        let name = match base.extension() {
            Some(ext) => format!("{stem}-zones.{}", ext.to_string_lossy()),
            None => format!("{stem}-zones"),
        };
        Some(base.with_file_name(name))
    }

    /// Persist the backend's bounded zone document. Encoding is small and
    /// bounded by construction, so it stays on this thread rather than joining
    /// the Block history's revision/baseline protocol, which guards a format
    /// this document deliberately does not share.
    fn save_zone_history(&self) -> io::Result<()> {
        let Some(zones) = self.render_backend.zone_replay_snapshot(
            zone_history::MAX_RESTORED_ZONES,
            zone_history::MAX_RESTORED_SNAPSHOT_BYTES,
        ) else {
            return Ok(());
        };
        let Some(path) = self.zone_history_path() else {
            return Ok(());
        };
        if zones.is_empty() {
            // An empty session must not leave a stale document behind for the
            // next run to replay.
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            return Ok(());
        }
        let encoded = zone_history::encode_session(zones)?;
        atomic_write(&path, |file| file.write_all(&encoded))
    }

    /// Replay this pane's persisted zones onto the surface before any PTY byte
    /// reaches it. A failed or unreadable document is logged and skipped: a
    /// restart with no history is a working pane, and refusing to start is not.
    fn restore_zone_history(&self) {
        let Some(path) = self.zone_history_path() else {
            return;
        };
        let zones = match zone_history::read_session(&path) {
            Ok(zones) => zones,
            Err(error) => {
                log::warn!("zone history not restored from {}: {error}", path.display());
                return;
            }
        };
        if zones.is_empty() {
            return;
        }
        let restored = self.render_backend.replay_zone_snapshot(zones);
        log::debug!("restored {restored} zones from {}", path.display());
    }

    /// Save block history without risking truncation of the last good snapshot.
    pub fn save_history(&self) -> io::Result<()> {
        // A backend that does not own the Block card document persists its own
        // bounded zone document instead, on a sibling path, so neither
        // representation can overwrite the other.
        if !self.render_backend.persists_block_history() {
            return self.save_zone_history();
        }
        let configured = {
            let config = self.config.borrow();
            configured_history_target(
                config.block_history_path.as_deref(),
                config.block_history_compress,
            )
        };
        let saves = {
            let pending = self.history_explicit_replace_pending.borrow();
            plan_history_saves(&pending, configured)
        };
        if saves.is_empty() {
            return Ok(());
        }

        let records = self.render_backend.records();
        let Some(block_data) = records.block_data() else {
            return Ok(());
        };
        let blocks = block_data.iter().cloned().collect::<Vec<_>>();
        execute_history_saves(
            &self.history_baselines,
            &self.history_explicit_replace_pending,
            &blocks,
            saves,
        )
    }

    /// Load only the configured number of most-recent history records.
    pub fn load_history(&self) -> io::Result<()> {
        // A backend without the Block card document restores its own bounded
        // zone document instead. That replay is synchronous on purpose: it
        // must reach the surface before the shell's first prompt, or restored
        // rows would land under output they precede.
        if !self.render_backend.persists_block_history() {
            self.restore_zone_history();
            return Ok(());
        }
        let (target, load_limit) = {
            let config = self.config.borrow();
            (
                configured_history_target(
                    config.block_history_path.as_deref(),
                    config.block_history_compress,
                ),
                (config.lazy_load_threshold as usize).min(config.max_visible_blocks as usize),
            )
        };
        let Some(target) = target else {
            self.reserved_history_block_ids.borrow_mut().clear();
            return Ok(());
        };

        let loaded = read_history_records_with_retained_budget(
            &target.path,
            target.compress,
            load_limit,
            UndecodablePolicy::Skip,
            Some(super::MAX_COMPLETED_BLOCK_RETAINED_BYTES),
        )?;
        let LoadedRecords {
            blocks: recent_blocks,
            total_loaded,
            revision,
            fully_decoded,
            fully_retained,
            seen_ids,
            clear_tombstone,
        } = loaded;
        let seen_ids = seen_ids.unwrap_or_default();
        let max_seen_id = seen_ids.iter().copied().max();
        replace_reserved_history_ids(&self.reserved_history_block_ids, seen_ids);
        replace_history_baseline(
            &self.history_baselines,
            target.path,
            (fully_decoded && fully_retained).then_some(revision),
            clear_tombstone,
        );
        if let Some(max_id) = max_seen_id {
            // Every decoded id (including omitted records) was reserved during
            // the scan; report the maximum without forcing a hostile high id
            // to jump the monotonic live allocator near overflow.
            log::debug!("Loaded Block history ids through {max_id}");
        }

        if total_loaded > recent_blocks.len() {
            log::info!(
                "Lazy loading history: keeping {} recent blocks out of {} total",
                recent_blocks.len(),
                total_loaded
            );
        }

        let start_index = total_loaded.saturating_sub(recent_blocks.len());
        mutate_block_data_and_redraw(
            &self.block_data,
            self.failure_marker_redraw.as_ref(),
            |blocks| {
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
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        atomic_write, baseline_observation, consume_succeeded_pending, decode_block_record,
        decode_clear_tombstone, decode_record, encode_clear_tombstone,
        encode_history_frames_bounded, enqueue_pending_clear, execute_history_saves,
        expand_home_prefix_with, lock_file_name, plan_history_saves, push_bounded_back,
        read_history_records, read_history_records_with_retained_budget, replace_history_baseline,
        replace_reserved_history_ids, save_history_snapshot, save_history_snapshot_with_intent,
        validate_history_progress, HistoryFileLock, HistoryRevision, HistoryTarget, SaveIntent,
        UndecodablePolicy, CLEAR_TOMBSTONE_FRAME_BYTES, MAX_HISTORY_DECODE_DURATION,
        MAX_HISTORY_FILE_BYTES, MAX_HISTORY_FRAMES,
    };
    use crate::block_view::BlockData;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::ffi::CString;
    use std::fs;
    use std::io::{self, Write as _};
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
                "anvil-history-{name}-{}-{unique}",
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
            start_time_ms: Some(1_000_u64.saturating_add(id)),
            end_time_ms: Some(2_000_u64.saturating_add(id)),
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
    fn retained_history_budget_keeps_newest_complete_records() {
        let dir = TestDir::new("retained-budget");
        let history = dir.path().join("history.bin");
        let mut blocks = vec![
            sample_block(1, "oldest"),
            sample_block(2, "middle"),
            sample_block(3, "newest"),
        ];
        blocks[0].output = "a".repeat(2048);
        blocks[1].output = "b".repeat(1024);
        blocks[2].output = "c".repeat(512);
        save_history_snapshot(&history, &blocks, false, Some(HistoryRevision::Missing)).unwrap();

        let newest_two_budget = blocks[1]
            .estimated_restored_retained_bytes()
            .saturating_add(blocks[2].estimated_restored_retained_bytes());
        let exact = read_history_records_with_retained_budget(
            &history,
            false,
            100,
            UndecodablePolicy::Reject,
            Some(newest_two_budget),
        )
        .unwrap();
        assert_eq!(
            exact
                .blocks
                .iter()
                .map(|block| block.id)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(!exact.fully_retained);
        assert_eq!(exact.seen_ids, Some(HashSet::from([1, 2, 3])));

        let over = read_history_records_with_retained_budget(
            &history,
            false,
            100,
            UndecodablePolicy::Reject,
            Some(newest_two_budget - 1),
        )
        .unwrap();
        assert_eq!(
            over.blocks.iter().map(|block| block.id).collect::<Vec<_>>(),
            [3]
        );

        let tiny = read_history_records_with_retained_budget(
            &history,
            false,
            100,
            UndecodablePolicy::Reject,
            Some(0),
        )
        .unwrap();
        assert_eq!(
            tiny.blocks.iter().map(|block| block.id).collect::<Vec<_>>(),
            [3]
        );
    }

    #[test]
    fn count_limited_load_revokes_ordinary_deletion_authority_and_tracks_all_ids() {
        let dir = TestDir::new("count-partial-authority");
        let history = dir.path().join("history.bin");
        let blocks = vec![
            sample_block(u64::MAX, "old-high-id"),
            sample_block(2, "new-low-id"),
        ];
        save_history_snapshot(&history, &blocks, false, Some(HistoryRevision::Missing)).unwrap();

        let loaded = read_history_records_with_retained_budget(
            &history,
            false,
            1,
            UndecodablePolicy::Reject,
            Some(usize::MAX),
        )
        .unwrap();
        assert_eq!(loaded.blocks.front().map(|block| block.id), Some(2));
        assert!(loaded.fully_decoded);
        assert!(!loaded.fully_retained);
        assert_eq!(loaded.seen_ids, Some(HashSet::from([2, u64::MAX])));
    }

    #[test]
    fn ui_scan_keeps_newest_duplicate_id_and_revokes_deletion_authority() {
        let dir = TestDir::new("duplicate-id-runtime");
        let history = dir.path().join("history.bin");
        let blocks = vec![
            sample_block(5, "old-process-value"),
            sample_block(5, "new-process-value"),
            sample_block(6, "unique"),
        ];
        save_history_snapshot(&history, &blocks, false, Some(HistoryRevision::Missing)).unwrap();

        let loaded = read_history_records_with_retained_budget(
            &history,
            false,
            usize::MAX,
            UndecodablePolicy::Reject,
            None,
        )
        .unwrap();
        assert_eq!(
            loaded
                .blocks
                .iter()
                .map(|block| block.cmd.as_str())
                .collect::<Vec<_>>(),
            ["new-process-value", "unique"]
        );
        assert!(loaded.fully_decoded);
        assert!(!loaded.fully_retained);
        assert_eq!(loaded.seen_ids, Some(HashSet::from([5, 6])));

        // The strict writer reread has no UI reservation collection cost or
        // global registration side effect; save-time merge remains recoverable.
        let strict =
            read_history_records(&history, false, usize::MAX, UndecodablePolicy::Reject).unwrap();
        assert_eq!(strict.seen_ids, None);
        assert_eq!(strict.blocks.len(), 3);
    }

    #[test]
    fn explicit_clear_replaces_history_after_a_partial_load() {
        let dir = TestDir::new("partial-explicit-clear");
        let history = dir.path().join("history.bin");
        let blocks = vec![sample_block(1, "old"), sample_block(2, "new")];
        let initial =
            save_history_snapshot(&history, &blocks, false, Some(HistoryRevision::Missing))
                .unwrap();

        let partial = read_history_records_with_retained_budget(
            &history,
            false,
            1,
            UndecodablePolicy::Reject,
            Some(usize::MAX),
        )
        .unwrap();
        assert!(!partial.fully_retained);

        // Without revision authority an empty ordinary snapshot is merge-only
        // and cannot erase records the pane did not retain.
        let ordinary = save_history_snapshot(&history, &[], false, None).unwrap();
        assert!(!ordinary.authoritative);
        assert_eq!(read_commands(&history), ["old", "new"]);

        let explicit =
            save_history_snapshot_with_intent(&history, &[], false, SaveIntent::ExplicitReplace)
                .unwrap();
        assert!(explicit.authoritative);
        assert!(read_commands(&history).is_empty());

        let mut stale_snapshot = blocks.clone();
        stale_snapshot.push(sample_block(7, "new-in-stale-pane"));
        let stale = save_history_snapshot_with_intent(
            &history,
            &stale_snapshot,
            false,
            SaveIntent::Revision {
                expected_revision: Some(initial.revision),
                observed_clear_tombstone: None,
            },
        )
        .unwrap_err();
        assert_eq!(stale.kind(), io::ErrorKind::PermissionDenied);
        assert!(read_commands(&history).is_empty());
        assert_eq!(
            save_history_snapshot_with_intent(
                &history,
                &stale_snapshot,
                false,
                SaveIntent::Revision {
                    expected_revision: Some(initial.revision),
                    observed_clear_tombstone: None,
                },
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );

        // A pane which loaded after Clear observes the marker and may append
        // genuinely new work without weakening the deletion barrier.
        let fresh = sample_block(3, "after-clear");
        let fresh_outcome = save_history_snapshot_with_intent(
            &history,
            std::slice::from_ref(&fresh),
            false,
            SaveIntent::Revision {
                expected_revision: Some(explicit.revision),
                observed_clear_tombstone: explicit.clear_tombstone,
            },
        )
        .unwrap();
        assert_eq!(read_commands(&history), ["after-clear"]);
        assert_eq!(fresh_outcome.clear_tombstone, explicit.clear_tombstone);

        // A fresh save does not erase the barrier or make an ancient pane
        // current by accident.
        assert_eq!(
            save_history_snapshot_with_intent(
                &history,
                &stale_snapshot,
                false,
                SaveIntent::Revision {
                    expected_revision: Some(initial.revision),
                    observed_clear_tombstone: None,
                },
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );

        let second_clear =
            save_history_snapshot_with_intent(&history, &[], false, SaveIntent::ExplicitReplace)
                .unwrap();
        assert_ne!(second_clear.clear_tombstone, explicit.clear_tombstone);
        assert_eq!(
            save_history_snapshot_with_intent(
                &history,
                std::slice::from_ref(&fresh),
                false,
                SaveIntent::Revision {
                    expected_revision: Some(fresh_outcome.revision),
                    observed_clear_tombstone: explicit.clear_tombstone,
                },
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(read_commands(&history).is_empty());
    }

    #[test]
    fn clear_tombstone_survives_compressed_history_round_trip() {
        let dir = TestDir::new("compressed-clear-tombstone");
        let history = dir.path().join("history.bin");
        let cleared =
            save_history_snapshot_with_intent(&history, &[], true, SaveIntent::ExplicitReplace)
                .unwrap();
        let block = sample_block(8, "after compressed clear");
        save_history_snapshot_with_intent(
            &history,
            std::slice::from_ref(&block),
            true,
            SaveIntent::Revision {
                expected_revision: Some(cleared.revision),
                observed_clear_tombstone: cleared.clear_tombstone,
            },
        )
        .unwrap();
        let loaded =
            read_history_records(&history, true, usize::MAX, UndecodablePolicy::Reject).unwrap();
        assert_eq!(loaded.clear_tombstone, cleared.clear_tombstone);
        assert_eq!(loaded.blocks.front().map(|block| block.id), Some(8));
    }

    #[test]
    fn clear_tombstone_has_an_exact_bounded_frame_cost() {
        let token = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
        let encoded = encode_clear_tombstone(token);
        assert_eq!(decode_clear_tombstone(&encoded), Some(token));
        assert_eq!(encoded.len() + 4, CLEAR_TOMBSTONE_FRAME_BYTES);
        assert_eq!(decode_clear_tombstone(&encoded[..encoded.len() - 1]), None);
    }

    #[test]
    fn record_frame_limit_reserves_the_tombstone_slot_and_keeps_newest() {
        let blocks = vec![
            sample_block(1, "oldest"),
            sample_block(2, "middle"),
            sample_block(3, "newest"),
        ];
        let frames = encode_history_frames_bounded(
            &blocks,
            false,
            usize::MAX,
            usize::MAX,
            2,
            usize::MAX,
            usize::MAX,
        )
        .unwrap();
        let ids = frames
            .iter()
            .map(|frame| decode_block_record(frame, false).unwrap().0.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [2, 3]);
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
            expand_home_prefix_with("~/.local/share/anvil/history", Some(home)),
            home.join(".local/share/anvil/history")
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
        .expect_err("partial existing history must block replacement");
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
            usize::MAX,
        )
        .unwrap();
        let newest_two_budget = 8 + all[1].len() + all[2].len();
        let kept = encode_history_frames_bounded(
            &blocks,
            false,
            usize::MAX,
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
    fn history_baselines_move_with_the_owner_and_remain_path_keyed() {
        let baselines = RefCell::new(HashMap::new());
        let first = PathBuf::from("/tmp/anvil-history-first");
        let second = PathBuf::from("/tmp/anvil-history-second");
        replace_history_baseline(
            &baselines,
            first.clone(),
            Some(HistoryRevision::Missing),
            Some(11),
        );
        // A partial second load retains its tombstone observation without
        // receiving deletion authority, and does not retire the first path.
        replace_history_baseline(&baselines, second.clone(), None, Some(22));

        // Moving the owning value cannot change lookup identity; no address is
        // present in either the state or its key.
        let moved = RefCell::new(baselines.into_inner());
        assert_eq!(
            baseline_observation(&moved.borrow(), &first),
            (Some(HistoryRevision::Missing), Some(11))
        );
        assert_eq!(
            baseline_observation(&moved.borrow(), &second),
            (None, Some(22))
        );
        assert_eq!(
            baseline_observation(&moved.borrow(), Path::new("/tmp/unobserved")),
            (None, None)
        );
    }

    #[test]
    fn pending_clear_target_is_first_and_only_matching_success_consumes_it() {
        let original = HistoryTarget {
            path: PathBuf::from("/tmp/original-history"),
            compress: false,
        };
        let configured = HistoryTarget {
            path: PathBuf::from("/tmp/new-history"),
            compress: true,
        };
        let pending = RefCell::new(VecDeque::from([original.clone()]));
        let saves = plan_history_saves(&pending.borrow(), Some(configured.clone()));
        assert_eq!(
            saves,
            [
                super::PlannedHistorySave {
                    target: original.clone(),
                    explicit_replace: true,
                },
                super::PlannedHistorySave {
                    target: configured.clone(),
                    explicit_replace: false,
                },
            ]
        );
        assert!(!consume_succeeded_pending(&pending, &configured));
        assert_eq!(pending.borrow().front(), Some(&original));
        assert!(consume_succeeded_pending(&pending, &original));
        assert!(pending.borrow().is_empty());
    }

    #[test]
    fn failed_clears_queue_targets_and_consume_only_each_successful_prefix() {
        let dir = TestDir::new("bound-clear-queue");
        let original = HistoryTarget {
            path: dir.path().join("original/history.bin"),
            compress: false,
        };
        let configured = HistoryTarget {
            path: dir.path().join("configured/history.bin"),
            compress: true,
        };
        save_history_snapshot(
            &configured.path,
            &[sample_block(9, "must-survive")],
            true,
            Some(HistoryRevision::Missing),
        )
        .unwrap();

        let pending = RefCell::new(VecDeque::new());
        enqueue_pending_clear(&pending, Some(original.clone()));
        enqueue_pending_clear(&pending, Some(original.clone()));
        enqueue_pending_clear(&pending, Some(configured.clone()));
        assert_eq!(
            pending.borrow().iter().cloned().collect::<Vec<_>>(),
            [original.clone(), configured.clone()]
        );
        let baselines = RefCell::new(HashMap::new());
        let guard = HistoryFileLock::acquire(&original.path).unwrap();
        let first_attempt = plan_history_saves(&pending.borrow(), Some(configured.clone()));
        assert!(first_attempt.iter().all(|save| save.explicit_replace));
        let error = execute_history_saves(&baselines, &pending, &[], first_attempt).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            pending.borrow().iter().cloned().collect::<Vec<_>>(),
            [original.clone(), configured.clone()]
        );
        assert!(baselines.borrow().is_empty());
        drop(guard);

        // A Drop/Undo retry executes A successfully, then stops at B. Only A
        // is consumed; the failed target and every later target stay queued.
        let configured_guard = HistoryFileLock::acquire(&configured.path).unwrap();
        let retry = plan_history_saves(&pending.borrow(), Some(configured.clone()));
        let error = execute_history_saves(&baselines, &pending, &[], retry).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(pending.borrow().front(), Some(&configured));
        assert_eq!(pending.borrow().len(), 1);
        let cleared =
            read_history_records(&original.path, false, usize::MAX, UndecodablePolicy::Reject)
                .unwrap();
        assert!(cleared.clear_tombstone.is_some());
        assert!(cleared.blocks.is_empty());
        let not_yet_cleared = read_history_records(
            &configured.path,
            true,
            usize::MAX,
            UndecodablePolicy::Reject,
        )
        .unwrap();
        assert_eq!(not_yet_cleared.clear_tombstone, None);
        assert_eq!(
            not_yet_cleared.blocks.front().map(|block| block.id),
            Some(9)
        );
        drop(configured_guard);

        let final_retry = plan_history_saves(&pending.borrow(), Some(configured.clone()));
        execute_history_saves(&baselines, &pending, &[], final_retry).unwrap();
        assert!(pending.borrow().is_empty());
        let also_cleared = read_history_records(
            &configured.path,
            true,
            usize::MAX,
            UndecodablePolicy::Reject,
        )
        .unwrap();
        assert!(also_cleared.clear_tombstone.is_some());
        assert!(also_cleared.blocks.is_empty());
    }

    #[test]
    fn pending_clear_keeps_original_codec_before_same_path_codec_flip() {
        let dir = TestDir::new("bound-clear-codec");
        let original = HistoryTarget {
            path: dir.path().join("history.bin"),
            compress: false,
        };
        let configured = HistoryTarget {
            path: original.path.clone(),
            compress: true,
        };
        let pending = RefCell::new(VecDeque::from([original]));
        let baselines = RefCell::new(HashMap::new());
        let saves = plan_history_saves(&pending.borrow(), Some(configured.clone()));
        assert_eq!(saves.len(), 2);
        execute_history_saves(
            &baselines,
            &pending,
            &[sample_block(4, "after-clear")],
            saves,
        )
        .unwrap();
        assert!(pending.borrow().is_empty());
        let loaded = read_history_records(
            &configured.path,
            true,
            usize::MAX,
            UndecodablePolicy::Reject,
        )
        .unwrap();
        assert!(loaded.clear_tombstone.is_some());
        assert_eq!(loaded.blocks.front().map(|block| block.id), Some(4));
    }

    #[test]
    fn reservation_reload_replaces_the_bounded_per_pane_set() {
        let reserved = RefCell::new(HashSet::from([7, 8, 9]));
        replace_reserved_history_ids(&reserved, HashSet::from([1, 2, u64::MAX]));
        assert_eq!(*reserved.borrow(), HashSet::from([1, 2, u64::MAX]));
        replace_reserved_history_ids(&reserved, HashSet::from([3]));
        assert_eq!(*reserved.borrow(), HashSet::from([3]));
        assert!(reserved.borrow().len() <= MAX_HISTORY_FRAMES);
    }

    #[test]
    fn clear_tombstone_discards_prefix_ids_from_the_logical_scan() {
        let dir = TestDir::new("tombstone-id-prefix");
        let history = dir.path().join("history.bin");
        let encode_one = |block: BlockData| {
            encode_history_frames_bounded(
                &[block],
                false,
                usize::MAX,
                usize::MAX,
                usize::MAX,
                usize::MAX,
                usize::MAX,
            )
            .unwrap()
            .pop()
            .unwrap()
        };
        let frames = [
            encode_one(sample_block(70, "obsolete")),
            encode_clear_tombstone(123),
            encode_one(sample_block(2, "current")),
        ];
        atomic_write(&history, |file| {
            for frame in &frames {
                file.write_all(&(frame.len() as u32).to_le_bytes())?;
                file.write_all(frame)?;
            }
            Ok(())
        })
        .unwrap();

        let loaded = read_history_records_with_retained_budget(
            &history,
            false,
            usize::MAX,
            UndecodablePolicy::Reject,
            None,
        )
        .unwrap();
        assert_eq!(loaded.seen_ids, Some(HashSet::from([2])));
        assert_eq!(loaded.blocks.front().map(|block| block.id), Some(2));
    }
}
