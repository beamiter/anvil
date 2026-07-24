//! Lightweight, privacy-conscious command history.
//!
//! Full block snapshots are optional because they contain terminal output.
//! This JSONL index stores only the command, cwd, exit status, and completion
//! time so History, the command palette, and opt-in AI context work out of the
//! box without persisting command output.

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const COMPACT_EVERY: u64 = 128;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Interactive history consumers only need a recent working set. Keep their
/// synchronous read bounded even when the on-disk history has reached its
/// 32 MiB compaction threshold.
const READ_RECENT_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);
const TEMP_FILE_ATTEMPTS: usize = 128;
const WRITER_QUEUE_CAPACITY: usize = 256;
static APPEND_COUNT: AtomicU64 = AtomicU64::new(0);
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
static HISTORY_WRITER: OnceLock<Result<mpsc::SyncSender<WriterMessage>, String>> = OnceLock::new();

fn sibling_path(path: &Path, suffix: &str) -> io::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command history path has no file name: {}", path.display()),
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut sibling_name = file_name.to_os_string();
    sibling_name.push(suffix);
    Ok(parent.join(sibling_name))
}

fn lock_path_for(path: &Path) -> io::Result<PathBuf> {
    sibling_path(path, ".lock")
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: `file` owns a live descriptor for this call. flock retains no
    // pointer and the descriptor remains owned by HistoryFileLock on success.
    let result =
        unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == nix::libc::EAGAIN || code == nix::libc::EWOULDBLOCK)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &File) -> io::Result<bool> {
    Ok(true)
}

#[cfg(unix)]
fn unlock(file: &File) {
    // SAFETY: the descriptor remains live until HistoryFileLock is dropped.
    if unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_UN) } != 0 {
        log::warn!(
            "failed to release command-history lock: {}",
            io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn unlock(_file: &File) {}

struct HistoryFileLock {
    file: File,
}

impl HistoryFileLock {
    fn acquire(history_path: &Path, timeout: Duration) -> io::Result<Self> {
        let path = lock_path_for(history_path)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }

        let started = Instant::now();
        loop {
            match try_lock_exclusive(&file)? {
                true => return Ok(Self { file }),
                false if started.elapsed() < timeout => thread::sleep(LOCK_POLL_INTERVAL),
                false => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "timed out waiting for command-history lock {}",
                            path.display()
                        ),
                    ));
                }
            }
        }
    }
}

impl Drop for HistoryFileLock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

fn create_unique_temp_file(target: &Path) -> io::Result<(File, PathBuf)> {
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "command history path has no file name: {}",
                target.display()
            ),
        )
    })?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    for _ in 0..TEMP_FILE_ATTEMPTS {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
        let temp_path = parent.join(temp_name);

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((file, temp_path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate a unique command-history temp file beside {}",
            target.display()
        ),
    ))
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CommandHistoryRecord {
    pub(crate) command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
    pub(crate) exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) end_time_ms: Option<u64>,
}

struct AppendRequest {
    path: PathBuf,
    max_entries: usize,
    command: String,
    cwd: Option<String>,
    exit_code: i32,
    end_time_ms: Option<u64>,
}

enum WriterMessage {
    Append(AppendRequest),
    Flush(mpsc::SyncSender<()>),
}

fn history_writer() -> io::Result<&'static mpsc::SyncSender<WriterMessage>> {
    let result = HISTORY_WRITER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        thread::Builder::new()
            .name("jterm1-command-history".to_string())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        WriterMessage::Append(request) => {
                            if let Err(error) = append(
                                &request.path,
                                request.max_entries,
                                &request.command,
                                request.cwd.as_deref(),
                                request.exit_code,
                                request.end_time_ms,
                            ) {
                                log::warn!("command history: {error}");
                            }
                        }
                        WriterMessage::Flush(acknowledge) => {
                            let _ = acknowledge.send(());
                        }
                    }
                }
            })
            .map(|_| sender)
            .map_err(|error| error.to_string())
    });
    result
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

/// Queue a history write without waiting on filesystem locks or compaction in
/// GTK's main thread. A bounded queue protects memory if storage stalls.
pub(crate) fn enqueue(
    path: &Path,
    max_entries: usize,
    command: &str,
    cwd: Option<&str>,
    exit_code: i32,
    end_time_ms: Option<u64>,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return Ok(());
    }
    crate::review_input::validate(command).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command is unsafe for review-only history: {error}"),
        )
    })?;
    if command.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command history record exceeds 1 MiB",
        ));
    }

    let request = AppendRequest {
        path: path.to_path_buf(),
        max_entries,
        command: command.to_string(),
        cwd: cwd.map(str::to_string),
        exit_code,
        end_time_ms,
    };
    history_writer()?
        .try_send(WriterMessage::Append(request))
        .map_err(|error| match error {
            mpsc::TrySendError::Full(_) => io::Error::new(
                io::ErrorKind::WouldBlock,
                "command-history writer queue is full",
            ),
            mpsc::TrySendError::Disconnected(_) => {
                io::Error::new(io::ErrorKind::BrokenPipe, "command-history writer stopped")
            }
        })
}

/// Wait until every history write accepted before this call has completed.
pub(crate) fn flush_pending(timeout: Duration) -> io::Result<()> {
    let Some(result) = HISTORY_WRITER.get() else {
        return Ok(());
    };
    let sender = result
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))?;
    let (acknowledge, received) = mpsc::sync_channel(0);
    let deadline = Instant::now() + timeout;
    let mut message = WriterMessage::Flush(acknowledge);

    loop {
        match sender.try_send(message) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(returned)) if Instant::now() < deadline => {
                message = returned;
                thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(mpsc::TrySendError::Full(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out queueing command-history flush",
                ));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "command-history writer stopped",
                ));
            }
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    received
        .recv_timeout(remaining)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out flushing command history",
            ),
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::new(io::ErrorKind::BrokenPipe, "command-history writer stopped")
            }
        })
}

pub(crate) fn append(
    path: &Path,
    max_entries: usize,
    command: &str,
    cwd: Option<&str>,
    exit_code: i32,
    end_time_ms: Option<u64>,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return Ok(());
    }
    crate::review_input::validate(command).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command is unsafe for review-only history: {error}"),
        )
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let record = CommandHistoryRecord {
        command: command.to_string(),
        cwd: cwd.map(str::to_string),
        exit_code,
        end_time_ms,
    };
    let mut encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
    if encoded.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command history record exceeds 1 MiB",
        ));
    }
    // Keep one physical JSONL record in one write_all call. The flock below is
    // the cross-process consistency boundary; combining the newline also keeps
    // readers safe if a future caller writes without sharing this process.
    encoded.push(b'\n');

    let _lock = HistoryFileLock::acquire(path, LOCK_TIMEOUT)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&encoded)?;
    file.flush()?;

    let append_number = APPEND_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let oversized = file.metadata()?.len() > MAX_FILE_BYTES;
    drop(file);
    if oversized || append_number.is_multiple_of(COMPACT_EVERY) {
        compact_locked(path, max_entries.max(1))?;
    }
    Ok(())
}

fn read_recent_from<R: Read + Seek>(
    input: &mut R,
    file_len: u64,
    max_entries: usize,
) -> io::Result<Vec<CommandHistoryRecord>> {
    if max_entries == 0 || file_len == 0 {
        return Ok(Vec::new());
    }

    let start = file_len.saturating_sub(READ_RECENT_TAIL_BYTES);
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        input.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0_u8; 1];
        input.read(&mut previous)? == 1 && previous[0] == b'\n'
    };

    input.seek(SeekFrom::Start(start))?;
    let tail_len = file_len - start;
    let mut tail = Vec::with_capacity(tail_len as usize);
    input.take(tail_len).read_to_end(&mut tail)?;
    if tail.is_empty() {
        return Ok(Vec::new());
    }

    // A bounded tail normally starts in the middle of a physical JSONL line.
    // Drop that fragment unless the byte immediately before the window was a
    // newline. Likewise, ignore an unterminated final line: another process
    // may still be appending it.
    let first_complete = if starts_at_line_boundary {
        0
    } else {
        let Some(newline) = tail.iter().position(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        newline + 1
    };
    let Some(last_newline) = tail.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(Vec::new());
    };
    if first_complete >= last_newline {
        return Ok(Vec::new());
    }

    let mut seen = HashSet::new();
    let mut records = Vec::with_capacity(max_entries.min(256));
    for line in tail[first_complete..last_newline]
        .rsplit(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        // Match append/read's physical-record limit, including the newline
        // excluded by `rsplit`.
        if line.len().saturating_add(1) > MAX_RECORD_BYTES {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<CommandHistoryRecord>(line) else {
            continue;
        };
        if crate::review_input::validate(&record.command).is_err()
            || !seen.insert(record.command.clone())
        {
            continue;
        }
        records.push(record);
        if records.len() == max_entries {
            break;
        }
    }
    Ok(records)
}

/// Read newest-first from a bounded tail window, deduplicating commands while
/// retaining newest metadata. Corrupt, oversized, incomplete, and unsafe
/// review-only records are ignored.
pub(crate) fn read_recent(
    path: &Path,
    max_entries: usize,
) -> io::Result<Vec<CommandHistoryRecord>> {
    let mut input = File::open(path)?;
    let file_len = input.metadata()?.len();
    read_recent_from(&mut input, file_len, max_entries)
}

fn compact(path: &Path, max_entries: usize) -> io::Result<()> {
    let _lock = HistoryFileLock::acquire(path, LOCK_TIMEOUT)?;
    compact_locked(path, max_entries.max(1))
}

/// Consume the remainder of one physical line without allocating storage
/// proportional to its length. `BufRead::read_until` appends every skipped byte
/// to its output buffer, so using it for a corrupt multi-gigabyte line could
/// exhaust the process even though command-history records are size-bounded.
fn discard_through_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(());
        }
    }
}

/// Compact while the caller owns the history sidecar lock. Keeping append,
/// scan, temp write, and rename in one critical section prevents a second
/// jterm1 process from appending to an inode that is about to be replaced.
fn compact_locked(path: &Path, max_entries: usize) -> io::Result<()> {
    let input = File::open(path)?;
    let mut reader = BufReader::new(input);
    let mut recent = VecDeque::with_capacity(max_entries.min(16_384));
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .by_ref()
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes > MAX_RECORD_BYTES || !line.ends_with('\n') {
            // Finish consuming a corrupt/oversized physical line before
            // looking for the next valid record.
            if !line.ends_with('\n') {
                discard_through_newline(&mut reader)?;
            }
            continue;
        }
        if serde_json::from_str::<serde_json::Value>(line.trim_end()).is_err() {
            continue;
        }
        if recent.len() == max_entries {
            recent.pop_front();
        }
        recent.push_back(line.clone());
    }

    let (output, temp_path) = create_unique_temp_file(path)?;
    let result = (|| {
        {
            let mut writer = BufWriter::new(output);
            for record in recent {
                writer.write_all(record.as_bytes())?;
            }
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::process::{Command, Stdio};

    const CHILD_PATH_ENV: &str = "JTERM1_HISTORY_TEST_CHILD_PATH";
    const CHILD_PREFIX_ENV: &str = "JTERM1_HISTORY_TEST_CHILD_PREFIX";
    const CHILD_COUNT_ENV: &str = "JTERM1_HISTORY_TEST_CHILD_COUNT";

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "jterm1-command-history-{name}-{}-{}.jsonl",
            std::process::id(),
            APPEND_COUNT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        if let Ok(lock_path) = lock_path_for(path) {
            let _ = fs::remove_file(lock_path);
        }
    }

    struct CountingCursor {
        inner: Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl std::io::Read for CountingCursor {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    impl std::io::Seek for CountingCursor {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn append_writes_palette_compatible_jsonl() {
        let path = temp_path("append");
        append(&path, 100, "cargo test", Some("/tmp/project"), 0, Some(42)).unwrap();
        let records = read_recent(&path, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "cargo test");
        assert_eq!(records[0].cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(records[0].exit_code, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        cleanup(&path);
    }

    #[test]
    fn read_recent_deduplicates_and_skips_corruption() {
        let path = temp_path("read");
        fs::write(
            &path,
            "{\"command\":\"one\",\"exit_code\":1}\nnot-json\n{\"command\":\"two\",\"exit_code\":0}\n{\"command\":\"one\",\"exit_code\":0}\n",
        )
        .unwrap();
        let records = read_recent(&path, 2).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(records[0].exit_code, 0);
        cleanup(&path);
    }

    #[test]
    fn read_recent_reads_only_the_bounded_tail() {
        let mut contents = vec![b'x'; READ_RECENT_TAIL_BYTES as usize + 64 * 1024];
        contents.extend_from_slice(
            b"\n{\"command\":\"older\",\"exit_code\":0}\n{\"command\":\"newer\",\"exit_code\":0}\n",
        );
        let file_len = contents.len() as u64;
        let mut input = CountingCursor {
            inner: Cursor::new(contents),
            bytes_read: 0,
        };

        let records = read_recent_from(&mut input, file_len, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
        assert!(
            input.bytes_read <= READ_RECENT_TAIL_BYTES as usize + 1,
            "read {} bytes for a {}-byte tail budget",
            input.bytes_read,
            READ_RECENT_TAIL_BYTES
        );
    }

    #[test]
    fn read_recent_skips_oversized_invalid_and_incomplete_tail_records() {
        let path = temp_path("tail-corruption");
        let mut contents = b"{\"command\":\"older\",\"exit_code\":0}\n".to_vec();
        let oversized = CommandHistoryRecord {
            command: "x".repeat(MAX_RECORD_BYTES),
            cwd: None,
            exit_code: 0,
            end_time_ms: None,
        };
        contents.extend_from_slice(&serde_json::to_vec(&oversized).unwrap());
        contents.push(b'\n');
        contents.extend_from_slice(b"\xff\n");
        contents.extend_from_slice(b"{\"command\":\"newer\",\"exit_code\":0}\n");
        contents.extend_from_slice(b"{\"command\":\"still-being-written\"");
        fs::write(&path, contents).unwrap();

        let records = read_recent(&path, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["newer", "older"]
        );
        cleanup(&path);
    }

    #[test]
    fn unsafe_control_characters_never_reach_the_palette() {
        let path = temp_path("control");
        let error = append(&path, 100, "echo one\necho two", Some("/tmp"), 0, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        fs::write(
            &path,
            "{\"command\":\"safe\",\"exit_code\":0}\n{\"command\":\"echo one\\necho two\",\"exit_code\":0}\n{\"command\":\"nul\\u0000byte\",\"exit_code\":0}\n",
        )
        .unwrap();
        let records = read_recent(&path, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["safe"]
        );
        cleanup(&path);
    }

    #[test]
    fn compact_keeps_only_recent_valid_records() {
        let path = temp_path("compact");
        fs::write(
            &path,
            "{\"command\":\"one\"}\nnot-json\n{\"command\":\"two\"}\n{\"command\":\"three\"}\n",
        )
        .unwrap();
        compact(&path, 2).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("one"));
        assert!(!text.contains("not-json"));
        assert!(text.contains("two"));
        assert!(text.contains("three"));
        cleanup(&path);
    }

    #[test]
    fn compact_streams_past_oversized_unterminated_record() {
        let path = temp_path("compact-oversized-unterminated");
        let oversized = vec![b'x'; MAX_RECORD_BYTES * 3 + 17];
        fs::write(&path, oversized).unwrap();

        compact(&path, 10).unwrap();

        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_is_private_and_contention_times_out() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("lock");
        let lock_path = lock_path_for(&path).unwrap();
        fs::write(&lock_path, b"").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o666)).unwrap();

        let held = HistoryFileLock::acquire(&path, LOCK_TIMEOUT).unwrap();
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let error = HistoryFileLock::acquire(&path, Duration::from_millis(30))
            .err()
            .expect("contended lock must time out");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("command-history lock"));

        drop(held);
        append(&path, 100, "after unlock", None, 0, None).unwrap();
        assert_eq!(read_recent(&path, 10).unwrap()[0].command, "after unlock");
        cleanup(&path);
    }

    #[test]
    fn queued_appends_are_persisted_by_flush() {
        let path = temp_path("queued");
        enqueue(&path, 100, "first", Some("/tmp"), 0, Some(1)).unwrap();
        enqueue(&path, 100, "second", Some("/tmp"), 1, Some(2)).unwrap();
        flush_pending(Duration::from_secs(2)).unwrap();

        let records = read_recent(&path, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
        cleanup(&path);
    }

    #[test]
    fn concurrent_append_child_process() {
        let Some(path) = std::env::var_os(CHILD_PATH_ENV) else {
            return;
        };
        let prefix = std::env::var(CHILD_PREFIX_ENV).unwrap();
        let count = std::env::var(CHILD_COUNT_ENV)
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let path = PathBuf::from(path);
        for index in 0..count {
            append(
                &path,
                10_000,
                &format!("{prefix}-{index}"),
                Some("/tmp"),
                0,
                Some(index as u64),
            )
            .unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_processes_keep_every_record_valid_through_compaction() {
        const WORKERS: usize = 2;
        // Each child crosses COMPACT_EVERY independently, forcing append and
        // rename transactions from separate processes to contend on the lock.
        const RECORDS_PER_WORKER: usize = COMPACT_EVERY as usize + 12;

        let path = temp_path("multiprocess");
        let executable = std::env::current_exe().unwrap();
        let test_name = "command_history::tests::concurrent_append_child_process";
        let mut children = Vec::new();
        for worker in 0..WORKERS {
            children.push(
                Command::new(&executable)
                    .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
                    .env(CHILD_PATH_ENV, &path)
                    .env(CHILD_PREFIX_ENV, format!("worker-{worker}"))
                    .env(CHILD_COUNT_ENV, RECORDS_PER_WORKER.to_string())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "history writer child failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let contents = fs::read(&path).unwrap();
        assert!(contents.ends_with(b"\n"));
        let mut commands = HashSet::new();
        for line in contents.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let record: CommandHistoryRecord = serde_json::from_slice(line).unwrap();
            assert!(commands.insert(record.command));
        }
        assert_eq!(commands.len(), WORKERS * RECORDS_PER_WORKER);
        cleanup(&path);
    }
}
